use redis::AsyncCommands;
use tokio_postgres::Client as PgClient;

use super::cost::{CostBreakdown, TokenUsage};
use crate::metrics as bs_metrics;

const BALANCE_KEY_PREFIX: &str = "plano:billing:balance:";

fn balance_key(actor_id: &str) -> String {
    format!("{BALANCE_KEY_PREFIX}{actor_id}")
}

/// Lua script: atomically deduct credits only if the balance would remain >= 0.
/// Returns: {success (0 or 1), balance_before, balance_after}
/// If success == 0, the deduction was refused (insufficient balance).
const DEDUCT_GUARD_SCRIPT: &str = r#"
local key = KEYS[1]
local amount = tonumber(ARGV[1])
local balance = tonumber(redis.call('GET', key)) or 0
if balance >= amount then
    redis.call('DECRBY', key, amount)
    return {1, balance, balance - amount}
else
    return {0, balance, balance - amount}
end
"#;

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

    /// Deduct credits atomically: refuses if balance would go negative.
    /// Logs the audit entry regardless of outcome (for reconciliation).
    /// Returns (balance_before, balance_after, was_deducted).
    pub async fn deduct_and_audit(
        &mut self,
        actor_id: &str,
        cost: &CostBreakdown,
        usage: &TokenUsage,
        model: &str,
        provider: &str,
        request_id: &str,
        is_streaming: bool,
    ) -> Result<(f64, f64, bool), String> {
        let key = balance_key(actor_id);

        let script = redis::Script::new(DEDUCT_GUARD_SCRIPT);
        let result: Vec<i64> = script
            .key(&key)
            .arg(cost.credits_deducted)
            .invoke_async(&mut self.redis)
            .await
            .map_err(|e| format!("Redis Lua deduct failed: {e}"))?;

        let was_deducted = result[0] == 1;
        let balance_before = result[1] as f64 / 1_000_000.0;
        let balance_after = result[2] as f64 / 1_000_000.0;

        if !was_deducted {
            bs_metrics::record_billing_balance_negative();
            tracing::warn!(
                actor_id = %actor_id,
                balance_credits = balance_before,
                deduct_credits = cost.credits_deducted as f64 / 1_000_000.0,
                "deduction refused — balance would go negative"
            );
        }

        // Audit log (always written for reconciliation)
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
                !was_deducted,
                usage_source,
                is_streaming,
            )
            .await
        {
            tracing::warn!(actor_id = %actor_id, error = %e, "audit log write failed (non-fatal)");
        }

        if was_deducted {
            bs_metrics::record_billing_deduction(model, usage_source);
        }
        Ok((balance_before, balance_after, was_deducted))
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
