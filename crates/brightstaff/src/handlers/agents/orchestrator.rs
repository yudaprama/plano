use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use hermesllm::apis::openai_responses::InputItem;
use hermesllm::apis::OpenAIMessage;
use hermesllm::clients::SupportedAPIsFromClient;
use hermesllm::providers::request::ProviderRequest;
use hermesllm::transforms::ExtractText;
use hermesllm::ProviderRequestType;
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use hyper::{Request, Response};
use opentelemetry::trace::get_active_span;
use tracing::{debug, info, info_span, warn, Instrument};

use super::errors::build_error_chain_response;
use super::pipeline::{PipelineError, PipelineProcessor};
use super::responses_translator::AgentResponsesTranslatorProcessor;
use super::selector::{AgentSelectionError, AgentSelector};
use crate::app_state::AppState;
use crate::handlers::extract_request_id;
use crate::handlers::response::ResponseHandler;
use crate::state::response_state_processor::ResponsesStateProcessor;
use crate::state::{
    extract_input_items, retrieve_and_combine_input, StateStorage, StateStorageError,
};
use crate::streaming::{self, ObservableStreamProcessor, StreamProcessor};
use crate::tracing::{collect_custom_trace_attributes, operation_component, set_service_name};

/// Main errors for agent chat completions
#[derive(Debug, thiserror::Error)]
pub enum AgentFilterChainError {
    #[error("Agent selection error: {0}")]
    Selection(#[from] AgentSelectionError),
    #[error("Pipeline processing error: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("Response handling error: {0}")]
    Response(#[from] common::errors::BrightStaffError),
    #[error("Request parsing error: {0}")]
    RequestParsing(String),
    #[error("HTTP error: {0}")]
    Http(#[from] hyper::Error),
    #[error("Unsupported endpoint: {0}")]
    UnsupportedEndpoint(String),
    #[error("No agents configured")]
    NoAgentsConfigured,
    #[error("Agent '{0}' not found in configuration")]
    AgentNotFound(String),
    #[error("No messages in conversation history")]
    EmptyHistory,
    #[error("Agent chain completed without producing a response")]
    IncompleteChain,
}

pub async fn agent_chat(
    request: Request<hyper::body::Incoming>,
    state: Arc<AppState>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let request_id = extract_request_id(&request);
    let custom_attrs =
        collect_custom_trace_attributes(request.headers(), state.span_attributes.as_ref());

    // Create a span with request_id that will be included in all log lines
    let request_span = info_span!(
        "(orchestrator)",
        component = "orchestrator",
        request_id = %request_id,
        http.method = %request.method(),
        http.path = %request.uri().path()
    );

    // Execute the handler inside the span
    async {
        // Set service name for orchestrator operations
        set_service_name(operation_component::ORCHESTRATOR);

        match handle_agent_chat_inner(request, state, request_id, custom_attrs).await {
            Ok(response) => Ok(response),
            Err(err) => {
                // Check if this is a client error from the pipeline that should be cascaded
                if let AgentFilterChainError::Pipeline(PipelineError::ClientError {
                    agent,
                    status,
                    body,
                }) = &err
                {
                    warn!(
                        agent = %agent,
                        status = %status,
                        body = %body,
                        "client error from agent"
                    );

                    let error_json = serde_json::json!({
                        "error": "ClientError",
                        "agent": agent,
                        "status": status,
                        "agent_response": body
                    });

                    let json_string = error_json.to_string();
                    let mut response =
                        Response::new(ResponseHandler::create_full_body(json_string));
                    *response.status_mut() = hyper::StatusCode::from_u16(*status)
                        .unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
                    response.headers_mut().insert(
                        hyper::header::CONTENT_TYPE,
                        hyper::header::HeaderValue::from_static("application/json"),
                    );
                    return Ok(response);
                }

                // Handle Talos auth errors with proper 401 status codes.
                if let AgentFilterChainError::Pipeline(PipelineError::Unauthorized(msg)) = &err {
                    warn!(error = %msg, "agent request unauthorized");
                    return Ok(ResponseHandler::create_unauthorized(msg));
                }

                build_error_chain_response(&err)
            }
        }
    }
    .instrument(request_span)
    .await
}

/// Parsed and validated agent request data.
struct AgentRequest {
    client_request: ProviderRequestType,
    /// True when the client used `/v1/responses`. Drives response translation:
    /// the egent's ChatCompletions output is wrapped with a translator that
    /// emits Responses API wire format back to the client.
    is_responses_api_client: bool,
    /// Original input items captured BEFORE state resolution merges prior turn.
    /// Fed to `ResponsesStateProcessor` so it can store the full conversation
    /// history (input + generated output) under the new response_id.
    original_input_items: Vec<InputItem>,
    messages: Vec<OpenAIMessage>,
    request_headers: hyper::HeaderMap,
    request_id: Option<String>,
}

/// Parse the incoming HTTP request, resolve the listener, and extract messages.
async fn parse_agent_request(
    request: Request<hyper::body::Incoming>,
    state: &AppState,
    request_id: &str,
    custom_attrs: &std::collections::HashMap<String, String>,
) -> Result<(AgentRequest, common::configuration::Listener, AgentSelector), AgentFilterChainError> {
    let agent_selector = AgentSelector::new(Arc::clone(&state.orchestrator_service));

    // Extract listener name from headers
    let listener_name = request
        .headers()
        .get("x-arch-agent-listener-name")
        .and_then(|name| name.to_str().ok());

    // Find the appropriate listener
    let listener = agent_selector.find_listener(listener_name, &state.listeners)?;

    get_active_span(|span| {
        span.update_name(listener.name.to_string());
        for (key, value) in custom_attrs {
            span.set_attribute(opentelemetry::KeyValue::new(key.clone(), value.clone()));
        }
    });

    info!(listener = %listener.name, "handling request");

    // Parse request body
    let full_path = request.uri().path().to_string();
    let request_path = full_path
        .strip_prefix("/agents")
        .unwrap_or(&full_path)
        .to_string();

    let request_headers = {
        let mut headers = request.headers().clone();
        headers.remove(common::consts::ENVOY_ORIGINAL_PATH_HEADER);

        if !headers.contains_key(common::consts::REQUEST_ID_HEADER) {
            if let Ok(val) = hyper::header::HeaderValue::from_str(request_id) {
                headers.insert(common::consts::REQUEST_ID_HEADER, val);
            }
        }

        headers
    };

    let chat_request_bytes = request.collect().await?.to_bytes();

    debug!(
        body = %String::from_utf8_lossy(&chat_request_bytes),
        "received request body"
    );

    let api_type =
        SupportedAPIsFromClient::from_endpoint(request_path.as_str()).ok_or_else(|| {
            warn!(path = %request_path, "unsupported endpoint");
            AgentFilterChainError::UnsupportedEndpoint(request_path.clone())
        })?;

    let client_request = ProviderRequestType::try_from((&chat_request_bytes[..], &api_type))
        .map_err(|err| {
            warn!(error = %err, "failed to parse request as ProviderRequestType");
            AgentFilterChainError::RequestParsing(format!("Failed to parse request: {}", err))
        })?;

    let is_responses_api_client =
        matches!(api_type, SupportedAPIsFromClient::OpenAIResponsesAPI(_));

    // Capture original input items for Responses API state storage. We extract
    // them BEFORE any `previous_response_id` merging so the stored row reflects
    // what the client sent this turn (the merge is applied to the live request
    // that goes to the egent, not to the persisted history root).
    let original_input_items = if is_responses_api_client {
        if let ProviderRequestType::ResponsesAPIRequest(ref r) = client_request {
            extract_input_items(&r.input)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let messages: Vec<OpenAIMessage> = client_request.get_messages();

    let request_id = request_headers
        .get(common::consts::REQUEST_ID_HEADER)
        .and_then(|val| val.to_str().ok())
        .map(|s| s.to_string());

    Ok((
        AgentRequest {
            client_request,
            is_responses_api_client,
            original_input_items,
            messages,
            request_headers,
            request_id,
        },
        listener,
        agent_selector,
    ))
}

/// Select agents via the orchestrator model and record selection metrics.
async fn select_and_build_agent_map(
    agent_selector: &AgentSelector,
    state: &AppState,
    messages: &[OpenAIMessage],
    listener: &common::configuration::Listener,
    request_id: Option<String>,
    requested_agent_id: Option<&str>,
) -> Result<
    (
        Vec<common::configuration::AgentFilterChain>,
        std::collections::HashMap<String, common::configuration::Agent>,
    ),
    AgentFilterChainError,
> {
    let agents = state
        .agents_list
        .as_ref()
        .ok_or(AgentFilterChainError::NoAgentsConfigured)?;
    let agent_map = agent_selector.create_agent_map(agents);

    let selection_start = Instant::now();
    let selected_agents = agent_selector
        .select_agents(messages, listener, request_id, requested_agent_id)
        .await?;

    let selection_elapsed_ms = selection_start.elapsed().as_secs_f64() * 1000.0;
    get_active_span(|span| {
        span.set_attribute(opentelemetry::KeyValue::new(
            "selection.listener",
            listener.name.clone(),
        ));
        span.set_attribute(opentelemetry::KeyValue::new(
            "selection.agent_count",
            selected_agents.len() as i64,
        ));
        span.set_attribute(opentelemetry::KeyValue::new(
            "selection.agents",
            selected_agents
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
        ));
        span.set_attribute(opentelemetry::KeyValue::new(
            "selection.determination_ms",
            format!("{:.2}", selection_elapsed_ms),
        ));
    });

    info!(
        count = selected_agents.len(),
        "selected agents for execution"
    );

    Ok((selected_agents, agent_map))
}

/// Execute the agent chain: run each selected agent sequentially, streaming
/// the final agent's response back to the client.
#[allow(clippy::too_many_arguments)]
async fn execute_agent_chain(
    selected_agents: &[common::configuration::AgentFilterChain],
    agent_map: &std::collections::HashMap<String, common::configuration::Agent>,
    client_request: ProviderRequestType,
    messages: Vec<OpenAIMessage>,
    request_headers: &hyper::HeaderMap,
    custom_attrs: &std::collections::HashMap<String, String>,
    // Responses-API support: when true, the last agent's ChatCompletions
    // response is translated to Responses API wire format before being
    // streamed back to the client. State context enables `previous_response_id`
    // continuity (stored to/postgres after the stream completes).
    is_responses_api_client: bool,
    original_input_items: Vec<InputItem>,
    state_storage: Option<&Arc<dyn StateStorage>>,
    request_id: String,
    llm_provider_url: String,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, AgentFilterChainError> {
    let mut pipeline_processor = PipelineProcessor::default();
    let response_handler = ResponseHandler::new();
    let mut current_messages = messages;
    let agent_count = selected_agents.len();

    for (agent_index, selected_agent) in selected_agents.iter().enumerate() {
        let agent_name = selected_agent.id.clone();
        let is_last_agent = agent_index == agent_count - 1;

        debug!(
            agent_index = agent_index + 1,
            total = agent_count,
            agent = %agent_name,
            "processing agent"
        );

        let chat_history = if selected_agent
            .input_filters
            .as_ref()
            .map(|f| !f.is_empty())
            .unwrap_or(false)
        {
            let filter_body = serde_json::json!({
                "model": client_request.model(),
                "messages": current_messages,
            });
            let filter_bytes =
                serde_json::to_vec(&filter_body).map_err(PipelineError::ParseError)?;

            let filtered_bytes = pipeline_processor
                .process_raw_filter_chain(
                    &filter_bytes,
                    selected_agent,
                    agent_map,
                    request_headers,
                    "/v1/chat/completions",
                )
                .await?;

            let filtered_body: serde_json::Value =
                serde_json::from_slice(&filtered_bytes).map_err(PipelineError::ParseError)?;
            serde_json::from_value(filtered_body["messages"].clone())
                .map_err(PipelineError::ParseError)?
        } else {
            current_messages.clone()
        };

        let agent = agent_map
            .get(&agent_name)
            .ok_or_else(|| AgentFilterChainError::AgentNotFound(agent_name.clone()))?;

        debug!(agent = %agent_name, "invoking agent");

        let agent_span = info_span!(
            "agent",
            agent_id = %agent_name,
            message_count = chat_history.len(),
        );

        // --- In-process Rig agent (fork) -------------------------------------
        // Agents configured with `type: rig` run their tool loop inside
        // brightstaff against plano's model gateway, instead of being proxied
        // to an external egent over HTTP. See `crates/rig_agent` + FORK.md.
        if agent.agent_type.as_deref() == Some("rig") {
            let reply = invoke_rig_agent(&chat_history, client_request.model(), &llm_provider_url)
                .instrument(agent_span.clone())
                .await?;

            if is_last_agent {
                if is_responses_api_client {
                    return Err(AgentFilterChainError::RequestParsing(
                        "in-process rig agent does not yet support the Responses API".into(),
                    ));
                }
                info!(agent = %agent_name, "completed in-process rig agent, returning response");
                return Ok(rig_chat_completion_response(&reply, client_request.model()));
            }

            debug!(agent = %agent_name, "collecting response from intermediate rig agent");
            let Some(last_message) = current_messages.pop() else {
                warn!(agent = %agent_name, "no messages in conversation history");
                return Err(AgentFilterChainError::EmptyHistory);
            };
            current_messages.push(OpenAIMessage {
                role: hermesllm::apis::openai::Role::Assistant,
                content: Some(hermesllm::apis::openai::MessageContent::Text(reply)),
                name: Some(agent_name.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
            current_messages.push(last_message);
            continue;
        }

        let llm_response = async {
            set_service_name(operation_component::AGENT);
            get_active_span(|span| {
                span.update_name(format!("{} /v1/chat/completions", agent_name));
                for (key, value) in custom_attrs {
                    span.set_attribute(opentelemetry::KeyValue::new(key.clone(), value.clone()));
                }
            });

            pipeline_processor
                .invoke_agent(
                    &chat_history,
                    client_request.clone(),
                    agent,
                    request_headers,
                )
                .await
        }
        .instrument(agent_span.clone())
        .await?;

        if is_last_agent {
            info!(
                agent = %agent_name,
                responses_api_client = is_responses_api_client,
                "completed agent chain, returning response"
            );

            // For Responses-API clients, translate the egent's ChatCompletions
            // output into Responses API wire format (streaming or non-streaming
            // depending on the egent's Content-Type). For ChatCompletions /
            // Messages clients, fall through to the existing passthrough path.
            if is_responses_api_client {
                return build_responses_api_response(
                    llm_response,
                    &client_request,
                    original_input_items,
                    state_storage,
                    request_id,
                )
                .instrument(agent_span)
                .await;
            }

            let orchestrator_span = tracing::Span::current();
            return async {
                response_handler
                    .create_streaming_response(
                        llm_response,
                        tracing::Span::current(),
                        orchestrator_span,
                    )
                    .await
                    .map_err(AgentFilterChainError::from)
            }
            .instrument(agent_span)
            .await;
        }

        debug!(agent = %agent_name, "collecting response from intermediate agent");
        let response_text = async { response_handler.collect_full_response(llm_response).await }
            .instrument(agent_span)
            .await?;

        info!(
            agent = %agent_name,
            response_len = response_text.len(),
            "agent completed, passing response to next agent"
        );

        let Some(last_message) = current_messages.pop() else {
            warn!(agent = %agent_name, "no messages in conversation history");
            return Err(AgentFilterChainError::EmptyHistory);
        };

        current_messages.push(OpenAIMessage {
            role: hermesllm::apis::openai::Role::Assistant,
            content: Some(hermesllm::apis::openai::MessageContent::Text(response_text)),
            name: Some(agent_name.clone()),
            tool_calls: None,
            tool_call_id: None,
        });

        current_messages.push(last_message);
    }

    Err(AgentFilterChainError::IncompleteChain)
}

/// Run an in-process Rig agent (`type: rig`) against plano's model gateway.
async fn invoke_rig_agent(
    chat_history: &[OpenAIMessage],
    model: &str,
    llm_provider_url: &str,
) -> Result<String, AgentFilterChainError> {
    set_service_name(operation_component::AGENT);
    let user_text = last_user_text(chat_history);
    // The gateway is reached loopback like the orchestrator; a placeholder
    // bearer is used unless the internal key is configured (see FORK.md).
    let api_key =
        std::env::var("PLANO_INTERNAL_KEY").unwrap_or_else(|_| "plano-internal".to_string());
    rig_agent::run_chat(&user_text, model, llm_provider_url, &api_key)
        .await
        .map_err(|e| AgentFilterChainError::RequestParsing(format!("in-process rig agent: {e}")))
}

/// Extract the last user message text from the chat history for the Rig prompt.
fn last_user_text(messages: &[OpenAIMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == hermesllm::apis::openai::Role::User)
        .and_then(|m| m.content.as_ref().map(|c| c.extract_text().to_string()))
        .unwrap_or_default()
}

/// Build a non-streaming OpenAI ChatCompletions response wrapping the Rig
/// agent's final text. (SSE for `stream:true` clients is a follow-up.)
fn rig_chat_completion_response(
    content: &str,
    model: &str,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = format!("chatcmpl-rig-{}", uuid::Uuid::new_v4().simple());
    let body = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    });
    let mut response = Response::new(rig_full(body.to_string()));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn rig_full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    http_body_util::Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// Build the final HTTP response for a Responses-API client by translating the
/// egent's ChatCompletions output (streaming or non-streaming) into Responses
/// API wire format. Optionally wraps the translator with `ResponsesStateProcessor`
/// so the new `response_id` + output items are persisted for `previous_response_id`
/// continuity on subsequent turns.
async fn build_responses_api_response(
    llm_response: reqwest::Response,
    client_request: &ProviderRequestType,
    original_input_items: Vec<InputItem>,
    state_storage: Option<&Arc<dyn StateStorage>>,
    request_id: String,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, AgentFilterChainError> {
    use hyper::StatusCode;

    let status = llm_response.status();
    let content_type = llm_response
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_encoding = llm_response
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // If the egent returned a non-2xx, pass the body through verbatim — the
    // translator cannot convert error JSON into a valid ResponsesAPIResponse.
    if !status.is_success() {
        debug!(
            status = %status,
            "egent returned non-success, passing body through without translation"
        );
        let orchestrator_span = tracing::Span::current();
        return ResponseHandler::new()
            .create_streaming_response(llm_response, tracing::Span::current(), orchestrator_span)
            .await
            .map_err(AgentFilterChainError::from);
    }

    // Detect streaming vs non-streaming from the egent's Content-Type.
    let is_streaming = content_type.contains("text/event-stream");

    // Copy response headers (Content-Type will be overridden below for the
    // translated output). For non-streaming translation we emit JSON; for
    // streaming translation we emit Responses API SSE.
    let mut response_builder = Response::builder().status(status);
    if let Some(headers) = response_builder.headers_mut() {
        for (k, v) in llm_response.headers().iter() {
            // Skip hop-by-hop / framing headers — `streaming::create_streaming_response`
            // sets Content-Length/Transfer-Encoding appropriately for the new body.
            if matches!(
                k.as_str(),
                "content-length" | "content-encoding" | "transfer-encoding" | "content-type"
            ) {
                continue;
            }
            headers.insert(k, v.clone());
        }
        headers.insert(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static(if is_streaming {
                "text/event-stream"
            } else {
                "application/json"
            }),
        );
    }

    // Build the processor chain. Data flows outer → inner:
    //   raw egent bytes → translator → [state processor →] observable → client
    //
    // The translator MUST run BEFORE the state processor because
    // `ResponsesStateProcessor::try_parse_response_chunk` parses its input as
    // a Responses API body (it looks for `ResponsesAPIStreamEvent::ResponseCompleted`
    // or `ResponsesAPIResponse`). Feeding it raw ChatCompletions bytes would
    // silently fail state capture. This mirrors the LLM gateway path where
    // translation happens in the WASM layer before bytes reach the processor.
    let translator = AgentResponsesTranslatorProcessor::new(is_streaming);

    let model = client_request.model().to_string();
    let provider = "egent".to_string();

    let base_processor = ObservableStreamProcessor::new(
        operation_component::AGENT.to_string(),
        format!("POST /v1/responses {}", model),
        Instant::now(),
        None,
    );

    // Inner of the chain: state processor wrapping the observable metrics
    // processor (when state storage is configured), or just the observable.
    let inner: Box<dyn StreamProcessor> =
        if let Some(store) = state_storage.filter(|_| !original_input_items.is_empty()) {
            Box::new(ResponsesStateProcessor::new(
                base_processor,
                store.clone(),
                original_input_items,
                model.clone(),
                provider,
                is_streaming,
                false, // is_openai_upstream — N/A for egent path
                content_encoding,
                request_id,
            ))
        } else {
            Box::new(base_processor)
        };

    // Outer of the chain: translator. Its output (Responses API bytes) feeds
    // into the inner state+observable stack.
    let chained = ChainedProcessor {
        outer: translator,
        inner,
    };

    let byte_stream = llm_response.bytes_stream();
    let streaming_response = streaming::create_streaming_response(byte_stream, chained);

    match response_builder.body(streaming_response.body) {
        Ok(response) => Ok(response),
        Err(err) => {
            let err_msg = format!("Failed to build Responses API response: {}", err);
            warn!(error = %err_msg);
            let mut internal_error = Response::new(crate::handlers::full(err_msg));
            *internal_error.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            Ok(internal_error)
        }
    }
}

/// Composite processor that pipes chunks through an outer processor and then
/// an inner processor. Used to stack the translator (outer) on top of the
/// state + observable stack (inner).
struct ChainedProcessor {
    outer: AgentResponsesTranslatorProcessor,
    inner: Box<dyn StreamProcessor>,
}

impl StreamProcessor for ChainedProcessor {
    fn process_chunk(&mut self, chunk: Bytes) -> Result<Option<Bytes>, String> {
        // Outer first (translate ChatCompletions → Responses), then forward
        // its output (if any) through the inner state+observable stack.
        match self.outer.process_chunk(chunk)? {
            Some(translated) => self.inner.process_chunk(translated),
            None => Ok(None),
        }
    }

    fn on_first_bytes(&mut self) {
        self.outer.on_first_bytes();
        self.inner.on_first_bytes();
    }

    fn on_complete(&mut self) {
        self.outer.on_complete();
        self.inner.on_complete();
    }

    fn on_error(&mut self, error: &str) {
        self.outer.on_error(error);
        self.inner.on_error(error);
    }

    fn take_billing_handle(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        // Billing lives in the observable processor (innermost).
        self.inner.take_billing_handle()
    }
}

async fn handle_agent_chat_inner(
    request: Request<hyper::body::Incoming>,
    state: Arc<AppState>,
    request_id: String,
    custom_attrs: std::collections::HashMap<String, String>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, AgentFilterChainError> {
    let (mut agent_req, listener, agent_selector) =
        parse_agent_request(request, &state, &request_id, &custom_attrs).await?;

    // --- Responses API state resolution (Change 4a) ------------------------
    //
    // If the client used `/v1/responses` and supplied a `previous_response_id`,
    // resolve the prior conversation state from postgres and merge it into the
    // current request's `input` so the egent sees the full multi-turn context.
    // Mirrors `handlers/llm/mod.rs::resolve_conversation_state` (Phase 2), but
    // simpler: agent "upstream" is always the ChatCompletions-only egent, so
    // `should_manage_state` is trivially true whenever the client is Responses.
    if agent_req.is_responses_api_client {
        let prev_id_opt: Option<String> =
            if let ProviderRequestType::ResponsesAPIRequest(ref r) = agent_req.client_request {
                r.previous_response_id.clone()
            } else {
                None
            };

        if let (Some(prev_id), Some(store)) = (prev_id_opt, state.state_storage.as_ref()) {
            let original_items = std::mem::take(&mut agent_req.original_input_items);
            match retrieve_and_combine_input(store.clone(), &prev_id, original_items).await {
                Ok(combined) => {
                    info!(
                        previous_response_id = %prev_id,
                        merged_items = combined.len(),
                        "merged conversation state into Responses API request"
                    );
                    if let ProviderRequestType::ResponsesAPIRequest(ref mut req) =
                        agent_req.client_request
                    {
                        use hermesllm::apis::openai_responses::InputParam;
                        req.input = InputParam::Items(combined.clone());
                        agent_req.original_input_items = combined;
                        // Re-extract messages so the orchestrator/egent sees the
                        // full history when picking + invoking the agent.
                        agent_req.messages = agent_req.client_request.get_messages();
                    }
                }
                Err(StateStorageError::NotFound(_)) => {
                    warn!(previous_response_id = %prev_id, "previous response_id not found");
                    return Err(AgentFilterChainError::RequestParsing(format!(
                        "Conversation state not found for previous_response_id: {}",
                        prev_id
                    )));
                }
                Err(e) => {
                    warn!(
                        previous_response_id = %prev_id,
                        error = %e,
                        "failed to retrieve conversation state, continuing stateless"
                    );
                    // Restore original items so state capture can still store at
                    // least this turn's input.
                    if let ProviderRequestType::ResponsesAPIRequest(ref r) =
                        agent_req.client_request
                    {
                        agent_req.original_input_items = extract_input_items(&r.input);
                    }
                }
            }
        }
    }

    // The client picks an agent directly via the `x-arch-agent-id` header. A
    // value matching a configured agent id bypasses the Plano-Orchestrator;
    // absence (or an unknown id) falls through to normal orchestration. Kept off
    // the `model` field so the existing model contract on the :8001 agent path
    // is unchanged for current callers (web/, eval) — only opt-in callers that
    // send the header change behavior.
    let requested_agent_id = agent_req
        .request_headers
        .get("x-arch-agent-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|id| !id.is_empty());

    let is_responses_api_client = agent_req.is_responses_api_client;
    let original_input_items = agent_req.original_input_items.clone();
    let req_id_str = agent_req.request_id.clone().unwrap_or_default();

    let (selected_agents, agent_map) = select_and_build_agent_map(
        &agent_selector,
        &state,
        &agent_req.messages,
        &listener,
        agent_req.request_id,
        requested_agent_id,
    )
    .await?;

    execute_agent_chain(
        &selected_agents,
        &agent_map,
        agent_req.client_request,
        agent_req.messages,
        &agent_req.request_headers,
        &custom_attrs,
        is_responses_api_client,
        original_input_items,
        state.state_storage.as_ref(),
        req_id_str,
        state.llm_provider_url.clone(),
    )
    .await
}
