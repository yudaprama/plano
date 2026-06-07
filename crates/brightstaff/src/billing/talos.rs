use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::time::Instant;

use crate::metrics as bs_metrics;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    pub credential: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub is_valid: bool,
    pub actor_id: Option<String>,
    pub key_id: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

pub struct TalosClient {
    http: Client,
    base_url: String,
    admin_token: Option<String>,
}

impl TalosClient {
    pub fn new(base_url: String, admin_token: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url,
            admin_token,
        }
    }

    pub async fn verify(&self, credential: &str) -> Result<VerifyResponse, String> {
        let started = Instant::now();
        let url = format!(
            "{}/v2alpha1/admin/apiKeys:verify",
            self.base_url.trim_end_matches('/')
        );

        let mut req = self.http.post(&url).json(&VerifyRequest {
            credential: credential.to_string(),
        });

        if let Some(ref token) = self.admin_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.map_err(|e| {
            bs_metrics::record_billing_talos_duration(started.elapsed());
            format!("Talos request failed: {e}")
        })?;
        bs_metrics::record_billing_talos_duration(started.elapsed());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Talos returned {status}: {body}"));
        }

        let parsed = resp
            .json::<VerifyResponse>()
            .await
            .map_err(|e| format!("Talos response parse error: {e}"))?;

        bs_metrics::record_billing_verification(if parsed.is_valid { "valid" } else { "invalid" });
        Ok(parsed)
    }
}
