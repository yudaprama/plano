//! StreamProcessor that translates Chat Completions wire format (from the egent)
//! into OpenAI Responses API wire format (expected by the client) on the fly.
//!
//! The egents only speak Chat Completions (`/v1/chat/completions`). When a
//! client calls the agent listener with `/v1/responses`, brightstaff must
//! translate the egent's response back into Responses API format before
//! streaming it to the client. This processor does that translation
//! chunk-by-chunk inside the streaming pipeline.
//!
//! Two modes (selected at construction time from the egent's Content-Type):
//!
//! - **Streaming** (text/event-stream): each chunk is parsed into SSE events,
//!   run through `SseEvent::try_from` (ChatCompletions → Responses event
//!   mapping), and added to a `SseStreamBuffer` which manages the Responses
//!   API lifecycle (response.created → output_text.delta → response.completed).
//! - **Non-streaming** (application/json): chunks are buffered until they form
//!   a complete `ChatCompletionsResponse`, which is then converted to a
//!   `ResponsesAPIResponse` and emitted in one shot.

use bytes::Bytes;
use hermesllm::apis::openai::ChatCompletionsResponse;
use hermesllm::apis::openai_responses::ResponsesAPIResponse;
use hermesllm::apis::streaming_shapes::sse::{SseStreamBuffer, SseStreamBufferTrait};
use hermesllm::apis::streaming_shapes::sse_chunk_processor::SseChunkProcessor;
use hermesllm::apis::OpenAIApi;
use hermesllm::clients::endpoints::{SupportedAPIsFromClient, SupportedUpstreamAPIs};
use tracing::{debug, warn};

use crate::streaming::StreamProcessor;

/// StreamProcessor that translates Chat Completions bytes (from egent) into
/// Responses API bytes (for the client).
///
/// For Requests-API clients only. Passthrough for any other invocation is the
/// caller's responsibility — this processor assumes Responses output is wanted.
pub struct AgentResponsesTranslatorProcessor {
    /// True when the egent returned `Content-Type: text/event-stream`.
    is_streaming: bool,
    /// Parser/translator for SSE chunks (handles cross-chunk buffering).
    sse_chunk_processor: SseChunkProcessor,
    /// Stateful wire-format buffer that emits Responses API lifecycle events.
    sse_buffer: SseStreamBuffer,
    /// Accumulator for non-streaming JSON bodies until parse succeeds.
    non_stream_buffer: Vec<u8>,
    /// Cached client/upstream API pair (Responses client, ChatCompletions upstream).
    client_api: SupportedAPIsFromClient,
    upstream_api: SupportedUpstreamAPIs,
}

impl AgentResponsesTranslatorProcessor {
    /// Construct a translator. `is_streaming` should be derived from the
    /// egent's `Content-Type` response header (`text/event-stream` → true).
    pub fn new(is_streaming: bool) -> Self {
        let client_api = SupportedAPIsFromClient::OpenAIResponsesAPI(OpenAIApi::Responses);
        let upstream_api = SupportedUpstreamAPIs::OpenAIChatCompletions(OpenAIApi::ChatCompletions);
        let sse_buffer = match SseStreamBuffer::try_from((&client_api, &upstream_api)) {
            Ok(b) => b,
            Err(e) => {
                // The Responses↔ChatCompletions combination is always supported;
                // fall back to passthrough on the impossible error.
                warn!(error = %e, "failed to build SseStreamBuffer, falling back to passthrough");
                SseStreamBuffer::Passthrough(Default::default())
            }
        };
        Self {
            is_streaming,
            sse_chunk_processor: SseChunkProcessor::new(),
            sse_buffer,
            non_stream_buffer: Vec::new(),
            client_api,
            upstream_api,
        }
    }
}

impl StreamProcessor for AgentResponsesTranslatorProcessor {
    fn process_chunk(&mut self, chunk: Bytes) -> Result<Option<Bytes>, String> {
        if self.is_streaming {
            // Streaming: parse + translate SSE events, feed into Responses API
            // wire buffer, emit whatever lifecycle events it produces.
            let events = self.sse_chunk_processor.process_chunk(
                &chunk,
                &self.client_api,
                &self.upstream_api,
            )?;
            for event in events {
                self.sse_buffer.add_transformed_event(event);
            }
            let out = self.sse_buffer.to_bytes();
            if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Bytes::from(out)))
            }
        } else {
            // Non-streaming: accumulate JSON until we can parse a complete
            // ChatCompletionsResponse, then convert + emit as a single chunk.
            self.non_stream_buffer.extend_from_slice(&chunk);
            // Try to parse — egents sometimes omit `usage` (it's a required
            // field in hermesllm's struct). The Usage sub-struct also has
            // required u32 fields, so injecting `{}` isn't enough; we inject
            // a full zero-default shape when `usage` is absent.
            let parsed = serde_json::from_slice::<serde_json::Value>(&self.non_stream_buffer)
                .ok()
                .and_then(|mut v| {
                    if v.get("usage").is_none() {
                        v["usage"] = serde_json::json!({
                            "prompt_tokens": 0,
                            "completion_tokens": 0,
                            "total_tokens": 0,
                        });
                    }
                    serde_json::from_value::<ChatCompletionsResponse>(v).ok()
                });
            if let Some(cc) = parsed {
                let resp: ResponsesAPIResponse = cc
                    .try_into()
                    .map_err(|e| format!("translate ChatCompletions→Responses: {e}"))?;
                let bytes = serde_json::to_vec(&resp)
                    .map_err(|e| format!("serialize ResponsesAPIResponse: {e}"))?;
                self.non_stream_buffer.clear();
                Ok(Some(Bytes::from(bytes)))
            } else {
                // Incomplete JSON — wait for more bytes. If the buffer is
                // unusually large, something is wrong (egent sent malformed
                // body); log once and keep waiting rather than OOM.
                if self.non_stream_buffer.len() > 8 * 1024 * 1024 {
                    warn!(
                        buffered = self.non_stream_buffer.len(),
                        "non-streaming buffer exceeds 8 MiB without a valid ChatCompletionsResponse"
                    );
                }
                debug!(
                    buffered = self.non_stream_buffer.len(),
                    "non-streaming response incomplete, buffering"
                );
                Ok(None)
            }
        }
    }

    fn on_complete(&mut self) {
        // Diagnostics only — the StreamProcessor contract doesn't let us emit
        // bytes from on_complete, so any leftover non-stream buffer is dropped.
        // In practice the buffer is always empty here for success responses:
        // the egent's ChatCompletions JSON parses as soon as the body is
        // complete, well before the stream ends. A non-empty buffer indicates
        // either a premature disconnect or a malformed body — log it so the
        // "size=0 from client" symptom is debuggable.
        if !self.is_streaming && !self.non_stream_buffer.is_empty() {
            warn!(
                buffered = self.non_stream_buffer.len(),
                "non-streaming buffer unparsed at stream complete — dropped (egent returned malformed body or disconnected early?)"
            );
        }
    }

    fn on_error(&mut self, error: &str) {
        warn!(error = %error, "responses translator stream error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use hermesllm::apis::openai::{
        ChatCompletionsResponse, Choice, FinishReason, FunctionCall, ResponseMessage, Role,
        ToolCall, Usage,
    };

    fn make_cc_response(text: &str) -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "chatcmpl-test".to_string(),
            object: Some("chat.completion".to_string()),
            created: 1234567890,
            model: "kawai-pro-max".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ResponseMessage {
                    role: Role::Assistant,
                    content: Some(text.to_string()),
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: None,
                },
                finish_reason: Some(FinishReason::Stop),
                logprobs: None,
            }],
            usage: Usage::default(),
            system_fingerprint: None,
            service_tier: None,
            metadata: None,
        }
    }

    fn make_cc_response_with_tool_call(name: &str, args: &str) -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            id: "chatcmpl-tool".to_string(),
            object: Some("chat.completion".to_string()),
            created: 1234567890,
            model: "kawai-pro-max".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ResponseMessage {
                    role: Role::Assistant,
                    content: None,
                    refusal: None,
                    annotations: None,
                    audio: None,
                    function_call: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    }]),
                },
                finish_reason: Some(FinishReason::ToolCalls),
                logprobs: None,
            }],
            usage: Usage::default(),
            system_fingerprint: None,
            service_tier: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn test_non_streaming_single_chunk_translates() {
        let cc = make_cc_response("hello world");
        let bytes = serde_json::to_vec(&cc).unwrap();

        let mut p = AgentResponsesTranslatorProcessor::new(false);
        let out = p.process_chunk(Bytes::from(bytes)).unwrap();
        assert!(out.is_some(), "expected translated chunk on first call");
        let out_bytes = out.unwrap();
        let resp: ResponsesAPIResponse = serde_json::from_slice(&out_bytes).unwrap();
        // The Responses API output should contain the assistant text.
        let json = serde_json::to_value(&resp).unwrap();
        let output_text = json["output"][0]["content"][0]["text"].as_str().unwrap();
        assert!(output_text.contains("hello world"));
    }

    #[tokio::test]
    async fn test_non_streaming_multi_chunk_buffers_until_complete() {
        let cc = make_cc_response("chunked body");
        let bytes = serde_json::to_vec(&cc).unwrap();
        let mid = bytes.len() / 2;
        let (a, b) = bytes.split_at(mid);

        let mut p = AgentResponsesTranslatorProcessor::new(false);
        // First half: incomplete JSON → None
        let out1 = p.process_chunk(Bytes::from(a.to_vec())).unwrap();
        assert!(out1.is_none(), "first half should buffer");
        // Second half: complete JSON → Some(translated)
        let out2 = p.process_chunk(Bytes::from(b.to_vec())).unwrap();
        assert!(out2.is_some(), "second half should complete + emit");
    }

    #[tokio::test]
    async fn test_non_streaming_handles_missing_usage_field() {
        // The egent omits `usage` on simple tool-call responses. The translator
        // must inject a default rather than fail to deserialize.
        let body = br#"{"id":"x","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

        let mut p = AgentResponsesTranslatorProcessor::new(false);
        let out = p.process_chunk(Bytes::from(&body[..])).unwrap();
        assert!(out.is_some(), "must emit despite missing usage");
        let out_bytes = out.unwrap();
        let resp: ResponsesAPIResponse = serde_json::from_slice(&out_bytes).unwrap();
        assert_eq!(resp.object, "response");
    }

    #[tokio::test]
    async fn test_non_streaming_tool_call_translates_to_function_call_output() {
        let cc = make_cc_response_with_tool_call("get_weather", r#"{"city":"Tokyo"}"#);
        let bytes = serde_json::to_vec(&cc).unwrap();

        let mut p = AgentResponsesTranslatorProcessor::new(false);
        let out = p.process_chunk(Bytes::from(bytes)).unwrap().unwrap();
        let resp: ResponsesAPIResponse = serde_json::from_slice(&out).unwrap();
        let json = serde_json::to_value(&resp).unwrap();

        // Output array should contain a function_call item.
        let output = json["output"].as_array().expect("output is array");
        let fn_item = output
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("should contain a function_call output item");
        assert_eq!(fn_item["name"], "get_weather");
        assert_eq!(fn_item["arguments"], r#"{"city":"Tokyo"}"#);
    }

    #[test]
    fn test_streaming_sse_translates_lifecycle() {
        // Build a minimal ChatCompletions SSE stream with one delta + [DONE].
        let sse_input = b"data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";

        let mut p = AgentResponsesTranslatorProcessor::new(true);
        let out = p.process_chunk(Bytes::from(&sse_input[..])).unwrap();
        // Streaming buffer should have produced some Responses API SSE bytes.
        assert!(out.is_some(), "expected translated SSE output");
        let out_bytes = out.unwrap();
        let s = String::from_utf8_lossy(&out_bytes);
        // Should be Responses API wire format (event: response.* lines).
        assert!(
            s.contains("response.") || s.contains("data: "),
            "expected response.* events or data lines, got: {s}"
        );
    }

    /// End-to-end: translator output (Responses API bytes) feeds into
    /// ResponsesStateProcessor, which captures response_id + output and stores
    /// the conversation state after the stream completes. Mirrors the chain
    /// wiring in `build_responses_api_response`:
    ///   raw CC bytes → translator → Responses API bytes → state processor → observable
    #[tokio::test]
    async fn test_state_processor_captures_translated_response() {
        use crate::state::memory::MemoryConversationalStorage;
        use crate::state::response_state_processor::ResponsesStateProcessor;
        use crate::state::StateStorage;
        use crate::streaming::ObservableStreamProcessor;
        use hermesllm::apis::openai_responses::{
            InputContent, InputItem, InputMessage, MessageContent, MessageRole,
        };
        use std::sync::Arc;

        let store = Arc::new(MemoryConversationalStorage::new()) as Arc<dyn StateStorage>;

        // Original input items the client sent (one user text message).
        let original_input = vec![InputItem::Message(InputMessage {
            role: MessageRole::User,
            content: MessageContent::Items(vec![InputContent::InputText {
                text: "what's the weather?".to_string(),
            }]),
        })];

        // ---- Stage 1: translator converts CC → Responses API bytes. ----
        let mut translator = AgentResponsesTranslatorProcessor::new(false);
        let cc = make_cc_response("sunny, 22C");
        let cc_bytes = serde_json::to_vec(&cc).unwrap();
        let translated = translator
            .process_chunk(Bytes::from(cc_bytes))
            .expect("translator emits")
            .expect("translator produced bytes");
        let resp: ResponsesAPIResponse = serde_json::from_slice(&translated).unwrap();
        let response_id = resp.id.clone();

        // ---- Stage 2: feed translated bytes through state processor. ----
        // This mirrors how `ChainedProcessor` forwards translator output to
        // the inner state+observable stack.
        let observable = ObservableStreamProcessor::new(
            "test".to_string(),
            "test /v1/responses".to_string(),
            std::time::Instant::now(),
            None,
        );
        let mut state_proc = ResponsesStateProcessor::new(
            observable,
            store.clone(),
            original_input,
            "kawai-pro-max".to_string(),
            "egent".to_string(),
            false, // non-streaming
            false,
            None,
            "req-test".to_string(),
        );

        // State processor receives the TRANSLATED Responses API bytes.
        let out = state_proc
            .process_chunk(translated.clone())
            .expect("state processor forwards");
        assert!(out.is_some(), "observable should pass bytes through");

        // Tell the chain the stream is done — this triggers state storage.
        state_proc.on_complete();

        // Storage is fire-and-forget; poll briefly until the row appears.
        let mut stored = None;
        for _ in 0..50 {
            if let Ok(s) = store.get(&response_id).await {
                stored = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let state = stored.expect("conversation state should be persisted");
        assert_eq!(state.response_id, response_id);
        assert_eq!(state.model, "kawai-pro-max");
        // Input history should contain the original user message + the
        // generated output items converted back to inputs.
        assert!(
            !state.input_items.is_empty(),
            "expected merged input items, got {}",
            state.input_items.len()
        );
    }
}
