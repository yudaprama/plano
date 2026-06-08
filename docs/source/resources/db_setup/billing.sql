-- Billing audit log for real-time prepaid credit deduction.
-- Run this in the same PostgreSQL/Supabase database used by billing.audit_database_url.
-- Note: balances are stored in Redis (integer credits * 1_000_000), not in this database.

CREATE TABLE IF NOT EXISTS billing_audit_log (
    id BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    model TEXT NOT NULL,
    provider TEXT NOT NULL,
    prompt_tokens BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    total_tokens BIGINT NOT NULL DEFAULT 0,
    cached_input_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    input_per_million DOUBLE PRECISION NOT NULL,
    output_per_million DOUBLE PRECISION NOT NULL,
    input_cost DOUBLE PRECISION NOT NULL,
    output_cost DOUBLE PRECISION NOT NULL,
    total_cost DOUBLE PRECISION NOT NULL,
    balance_before DOUBLE PRECISION NOT NULL,
    balance_after DOUBLE PRECISION NOT NULL,
    balance_went_negative BOOLEAN NOT NULL DEFAULT FALSE,
    usage_source TEXT NOT NULL CHECK (usage_source IN ('reported', 'estimated')),
    is_streaming BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX IF NOT EXISTS idx_billing_audit_log_actor_created
    ON billing_audit_log (actor_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_billing_audit_log_request_id
    ON billing_audit_log (request_id);
