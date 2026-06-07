pub mod balance;
pub mod cost;
pub mod talos;
pub mod verify_cache;

use common::configuration::BillingConfig;
use std::collections::HashMap;
use tokio_postgres::Client as PgClient;
use tokio_postgres::NoTls;

use crate::metrics as bs_metrics;
use balance::BalanceService;
use cost::{CostBreakdown, TokenUsage};
use talos::{TalosClient, VerifyResponse};
use verify_cache::VerifyCache;

pub struct BillingService {
    talos: TalosClient,
    cache: VerifyCache,
    balance: tokio::sync::Mutex<BalanceService>,
    pricing: HashMap<String, common::configuration::ModelPricing>,
    default_pricing: common::configuration::ModelPricing,
    minimum_balance: f64,
}

impl BillingService {
    pub async fn new(
        config: &BillingConfig,
        audit_database_url: Option<String>,
    ) -> Result<Self, String> {
        let redis_conn = redis::Client::open(config.redis_url.as_str())
            .map_err(|e| format!("Redis client creation failed: {e}"))?;
        let redis = redis_conn
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| format!("Redis connection failed: {e}"))?;

        let pg = connect_audit_database(audit_database_url).await?;

        Ok(Self {
            talos: TalosClient::new(config.talos_url.clone(), config.talos_admin_token.clone()),
            cache: VerifyCache::new(1024, config.verify_cache_ttl_secs),
            balance: tokio::sync::Mutex::new(BalanceService::new(redis, pg)),
            pricing: config.pricing.clone(),
            default_pricing: config.default_pricing.clone(),
            minimum_balance: config.minimum_balance,
        })
    }

    /// Verify an API key via Talos (with LRU cache).
    pub async fn verify_key(&self, credential: &str) -> Result<VerifyResponse, String> {
        if let Some(cached) = self.cache.get(credential) {
            bs_metrics::record_billing_verification("cache_hit");
            return Ok(cached);
        }

        let response = self.talos.verify(credential).await?;

        if response.is_valid {
            self.cache.insert(credential, response.clone());
        }

        Ok(response)
    }

    /// Check if an actor has sufficient balance.
    pub async fn check_balance(&self, actor_id: &str) -> Result<f64, String> {
        let mut svc = self.balance.lock().await;
        svc.check_balance(actor_id).await
    }

    pub fn minimum_balance(&self) -> f64 {
        self.minimum_balance
    }

    pub fn pricing(&self) -> &HashMap<String, common::configuration::ModelPricing> {
        &self.pricing
    }

    pub fn default_pricing(&self) -> &common::configuration::ModelPricing {
        &self.default_pricing
    }

    /// Deduct credits and write audit log (called in fire-and-forget spawned task).
    pub async fn deduct_and_audit(
        &self,
        actor_id: &str,
        cost: &CostBreakdown,
        usage: &TokenUsage,
        model: &str,
        provider: &str,
        request_id: &str,
        is_streaming: bool,
    ) -> Result<(f64, f64), String> {
        let mut svc = self.balance.lock().await;
        svc.deduct_and_audit(
            actor_id,
            cost,
            usage,
            model,
            provider,
            request_id,
            is_streaming,
        )
        .await
    }
}

async fn connect_audit_database(database_url: Option<String>) -> Result<Option<PgClient>, String> {
    let Some(database_url) = database_url else {
        tracing::warn!("billing audit database URL not configured; audit rows will not be written");
        return Ok(None);
    };

    let (client, connection) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .map_err(|e| format!("billing audit database connection failed: {e}"))?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::warn!(error = %e, "billing audit database connection error");
        }
    });

    Ok(Some(client))
}
