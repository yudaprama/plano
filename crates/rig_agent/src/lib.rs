//! In-process Rig agent, embedded in brightstaff.
//!
//! Unlike the egents (separate Go HTTP services that plano proxies to over
//! `/v1/chat/completions`), this runs the Rig tool loop directly inside the
//! brightstaff process. brightstaff's `execute_agent_chain` short-circuits the
//! usual Envoy proxy path for any configured agent with `type: rig` and calls
//! [`run_chat`] instead.
//!
//! The model is sourced from plano's existing model gateway (brightstaff's
//! `llm_provider_url`, the same endpoint the Plano-Orchestrator calls), so the
//! agent reuses plano's model aliases, health tracking, and routing.
//!
//! PoC scope: a single `current_time` tool validates that the in-process tool
//! loop works end-to-end through the gateway. A real agent's purpose plugs in
//! by replacing the preamble + tool set below.

use chrono::Utc;
use rig::client::AgentClientExt;
use rig::completion::Prompt;
use rig::providers::openai;
use rig::tool::{DynamicTool, ToolOutput};
use serde_json::json;
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum RigAgentError {
    #[error("failed to build rig client: {0}")]
    ClientBuild(String),
    #[error("rig agent prompt failed: {0}")]
    Prompt(String),
}

pub type Result<T> = std::result::Result<T, RigAgentError>;

const PREAMBLE: &str = "\
You are an in-process Rig agent running inside plano's brightstaff binary. \
You have tools available. Use them when the user's request matches a tool's \
purpose, then answer concisely using the tool result. If no tool applies, \
answer directly.";

/// Run the in-process Rig agent against plano's model gateway.
///
/// - `user_text`: the assembled prompt for this turn (brightstaff extracts it
///   from the incoming request's last user message).
/// - `model`: the model alias to send to the gateway (carried from the client
///   request, e.g. `kawai-pro-max`).
/// - `gateway_root`: brightstaff's `llm_provider_url` — the host root with no
///   trailing `/v1` (rig appends `/v1/chat/completions` itself).
/// - `api_key`: bearer sent to the gateway. The gateway is reached loopback the
///   same way the orchestrator reaches it, so a placeholder is usually fine.
///
/// Returns the agent's final assistant text after the tool loop completes.
pub async fn run_chat(
    user_text: &str,
    model: &str,
    gateway_root: &str,
    api_key: &str,
) -> Result<String> {
    let base_url = format!("{}/v1", gateway_root.trim_end_matches('/'));
    debug!(%base_url, %model, "rig agent calling plano model gateway");

    let client = openai::Client::builder()
        .base_url(base_url)
        .api_key(api_key.to_string())
        .build()
        .map_err(|e| RigAgentError::ClientBuild(e.to_string()))?;

    let agent = client
        .agent(model)
        .preamble(PREAMBLE)
        .dynamic_tools(current_time_tool())
        .max_tokens(4096)
        .default_max_turns(3)
        .build();

    let response = agent
        .prompt(user_text)
        .await
        .map_err(|e| RigAgentError::Prompt(e.to_string()))?;

    debug!(response_len = response.len(), "rig agent completed");
    Ok(response)
}

/// Build the PoC `current_time` tool: returns the current UTC time as an
/// RFC3339 string. Takes no arguments.
fn current_time_tool() -> Vec<DynamicTool> {
    vec![DynamicTool::new(
        "current_time",
        "Get the current UTC date and time as an RFC3339 string. \
         Use for any question about what time or day it is now.",
        json!({
            "type": "object",
            "properties": {}
        }),
        |_ctx, _args| {
            Box::pin(async move {
                let now = Utc::now().to_rfc3339();
                Ok(ToolOutput::json(json!(now)))
            })
        },
    )]
}

#[allow(dead_code)]
fn warn_unused() {
    warn!("rig_agent loaded");
}
