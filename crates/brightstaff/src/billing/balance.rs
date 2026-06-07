use redis::AsyncCommands;
use tokio_postgres::Client as PgClient;

use super::cost::{CostBreakdown, TokenUsage};
use crate::metrics as bs_metrics;

const BALANCE_KEY_PREFIX: &str = "plano:billing:balance:";

fn balance_key(actor_id: &str) -> String {
    format!("{BALANCE_KEY_PREFIX}{actor_id}")
}

pub struct BalanceService {
    redis: redis::aio::MultiplexedConnection,
    pg: Option<PgClient>,
}

impl BalanceService {
    pub fn new(redis: redis::aio::MultiplexedConnection, pg: Option<PgClient>) -> Self {
        Self { redis, pg }
    }

    /// Check the current balance for an actor. Returns 0.0 if no balance exists.
    pub async fn check_balance(&mut self, actor_id: &str) -> Result<f64, String> {
        let key = balance_key(actor_id);
        let raw: Option<i64> = self
            .redis
            .get(&key)
            .await
            .map_err(|e| format!("Redis GET failed: {e}"))?;

        // Balance stored as integer (credits * 1_000_000)
        Ok(raw.unwrap_or(0) as f64 / 1_000_000.0)
    }

    /// Deduct credits from an actor's balance and log the audit entry.
    /// Returns (balance_before, balance_after).
    pub async fn deduct_and_audit(
        &mut self,
        actor_id: &str,
        cost: &CostBreakdown,
        usage: &TokenUsage,
        model: &str,
        provider: &str,
        request_id: &str,
        is_streaming: bool,
    ) -> Result<(f64, f64), String> {
        let key = balance_key(actor_id);

        // Atomic decrement
        let after_raw: i64 = self
            .redis
            .decr(&key, cost.credits_deducted)
            .await
            .map_err(|e| format!("Redis DECRBY failed: {e}"))?;

        let balance_after = after_raw as f64 / 1_000_000.0;
        let balance_before = (after_raw + cost.credits_deducted) as f64 / 1_000_000.0;

        let went_negative = after_raw < 0;
        if went_negative {
            bs_metrics::record_billing_balance_negative();
        }

        // Fire-and-forget audit log
        let usage_source = cost.usage_source.as_str();
        if let Err(e) = self
            .write_audit_log(
                actor_id,
                request_id,
                model,
                provider,
                usage,
                cost,
                balance_before,
                balance_after,
                went_negative,
                usage_source,
                is_streaming,
            )
            .await
        {
            tracing::warn!(actor_id = %actor_id, error = %e, "audit log write failed (non-fatal)");
        }

        bs_metrics::record_billing_deduction(model, usage_source);
        Ok((balance_before, balance_after))
    }

    async fn write_audit_log(
        &self,
        actor_id: &str,
        request_id: &str,
        model: &str,
        provider: &str,
        usage: &TokenUsage,
        cost: &CostBreakdown,
        balance_before: f64,
        balance_after: f64,
        went_negative: bool,
        usage_source: &str,
        is_streaming: bool,
    ) -> Result<(), String> {
        let client = self.pg.as_ref().ok_or("PostgreSQL not connected")?;

        client
            .execute(
                "INSERT INTO billing_audit_log \
                 (actor_id, request_id, model, provider, \
                  prompt_tokens, completion_tokens, total_tokens, cached_input_tokens, reasoning_tokens, \
                  input_per_million, output_per_million, \
                  input_cost, output_cost, total_cost, \
                  balance_before, balance_after, balance_went_negative, \
                  usage_source, is_streaming) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
                &[
                    &actor_id,
                    &request_id,
                    &model,
                    &provider,
                    &usage.prompt_tokens,
                    &usage.completion_tokens,
                    &usage.total_tokens,
                    &usage.cached_input_tokens,
                    &usage.reasoning_tokens,
                    &cost.input_per_million,
                    &cost.output_per_million,
                    &cost.input_cost,
                    &cost.output_cost,
                    &cost.total_cost,
                    &balance_before,
                    &balance_after,
                    &went_negative,
                    &usage_source,
                    &is_streaming,
                ],
            )
            .await
            .map_err(|e| format!("PostgreSQL INSERT failed: {e}"))?;

        Ok(())
    }
}
