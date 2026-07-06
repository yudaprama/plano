use bytes::Bytes;
use common::configuration::ExposedModel;
use common::llm_providers::LlmProviders;
use http_body_util::combinators::BoxBody;
use hyper::{Response, StatusCode};
use std::sync::Arc;

use super::full;

pub async fn list_models(
    llm_providers: Arc<tokio::sync::RwLock<LlmProviders>>,
    exposed_models: Option<&Vec<ExposedModel>>,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let json = if let Some(models) = exposed_models {
        // Config-driven list: return exposed_models from plano_config.yaml as-is.
        let data: Vec<serde_json::Value> = models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "object": "model",
                    "created": 0,
                    "owned_by": m.owned_by,
                })
            })
            .collect();
        serde_json::json!({"object": "list", "data": data}).to_string()
    } else {
        // Fallback: derive from model_providers (excludes internal: true entries).
        let prov = llm_providers.read().await;
        match serde_json::to_string(&prov.to_models()) {
            Ok(j) => j,
            Err(_) => return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(full("{\"error\":\"Failed to serialize models\"}"))
                .unwrap(),
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(full(json))
        .unwrap()
}
