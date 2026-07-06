use bytes::Bytes;
use common::llm_providers::LlmProviders;
use http_body_util::combinators::BoxBody;
use hyper::{Response, StatusCode};
use std::sync::Arc;

use super::full;

// Branded model list returned to API clients. Backend providers (zai-coding,
// venice, openrouter, ollama) are intentionally hidden — clients use the
// kawai-* aliases which route to the appropriate backend via model_aliases.
const KAWAI_MODELS_JSON: &str = r#"{"object":"list","data":[
  {"id":"kawai-auto","object":"model","created":0,"owned_by":"kawai"},
  {"id":"kawai-pro-max","object":"model","created":0,"owned_by":"kawai"},
  {"id":"kawai-flash","object":"model","created":0,"owned_by":"kawai"}
]}"#;

pub async fn list_models(
    _llm_providers: Arc<tokio::sync::RwLock<LlmProviders>>,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(full(KAWAI_MODELS_JSON.to_string()))
        .unwrap()
}
