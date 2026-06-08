Billing & Usage Tracking
========================

Plano includes a built-in **prepaid billing system** that tracks LLM usage, calculates costs in real-time, and deducts credits from user balances. This enables you to build metered AI applications with usage-based pricing.

.. contents:: Table of Contents
   :local:
   :depth: 2

Overview
--------

The billing system provides:

- **Real-time cost calculation** based on token usage and model pricing
- **Prepaid credit balances** stored in Redis for sub-millisecond latency
- **API key verification** via Talos integration (optional external service)
- **Audit logging** to PostgreSQL for compliance and analytics
- **Pre-flight balance checks** to prevent over-spending
- **Per-model pricing configuration** with cache discount support
- **Prometheus metrics** for monitoring billing health

**Key Design Principles:**

- **Non-blocking**: Balance checks and deductions happen asynchronously
- **Fault-tolerant**: Failed deductions are logged but don't block requests
- **Precision**: Credits stored as integers (balance × 1,000,000) to avoid float rounding errors
- **Observable**: Every transaction is logged with full cost breakdown

Architecture
------------

Billing Flow
~~~~~~~~~~~~

.. code-block:: text

   Client Request
        ↓
   [1] API Key Verification (Talos + Cache)
        ↓
   [2] Balance Pre-Check (Redis)
        ↓
   [3] LLM Request Forwarded
        ↓
   [4] Token Usage Extracted
        ↓
   [5] Cost Calculation (Pricing Matrix)
        ↓
   [6] Credit Deduction (Redis DECRBY)
        ↓
   [7] Audit Log (PostgreSQL)
        ↓
   Response to Client

Components
~~~~~~~~~~

**BillingService** (``crates/brightstaff/src/billing/mod.rs``)
   Orchestrates billing lifecycle: verification, balance checks, cost calculation, deduction, and audit.

**BalanceService** (``crates/brightstaff/src/billing/balance.rs``)
   Manages Redis balance operations and PostgreSQL audit writes. Credits are stored as integers in Redis keys: ``plano:billing:balance:{actor_id}``.

**CostCalculator** (``crates/brightstaff/src/billing/cost.rs``)
   Computes cost from token usage using per-model pricing. Handles cached input tokens with configurable discounts.

**TalosClient** (``crates/brightstaff/src/billing/talos.rs``)
   Verifies API keys against an external Talos service (optional). Returns ``actor_id`` (user identifier) for balance lookups.

**VerifyCache** (``crates/brightstaff/src/billing/verify_cache.rs``)
   LRU cache for Talos responses (reduces external calls by ~90%).

**Integration Points**
   - ``crates/brightstaff/src/handlers/llm/mod.rs``: Pre-check before LLM routing
   - ``crates/brightstaff/src/streaming.rs``: Deduction after streaming responses

Configuration
-------------

Billing is configured in your ``config.yaml`` under the ``billing`` key:

.. code-block:: yaml

   billing:
     # Redis for balance storage (required)
     redis_url: "redis://localhost:6379"
     
     # PostgreSQL for audit logs (optional but recommended)
     audit_database_url: "postgresql://user:pass@host:5432/db"
     
     # Talos API key verification service (optional)
     talos_url: "https://talos.example.com"
     talos_admin_token: "admin_secret_token"
     talos_timeout_secs: 2
     
     # Cache settings
     verify_cache_ttl_secs: 300  # 5 minutes
     
     # Minimum balance required to process requests
     minimum_balance: 0.01  # $0.01 USD
     
     # Per-model pricing (cost per 1M tokens)
     pricing:
       "gpt-4o":
         input_per_million: 2.5
         output_per_million: 10.0
         cache_discount: 0.5  # 50% discount for cached input
       
       "claude-3-5-sonnet-20241022":
         input_per_million: 3.0
         output_per_million: 15.0
         cache_discount: 0.1  # 90% discount for cached input
     
     # Default pricing for models not in the matrix
     default_pricing:
       input_per_million: 5.0
       output_per_million: 15.0
       cache_discount: 1.0  # No cache discount

Configuration Reference
~~~~~~~~~~~~~~~~~~~~~~~

``redis_url`` (required)
   Redis connection string for balance storage. Format: ``redis://[user:pass@]host:port[/db]``.

``audit_database_url`` (optional)
   PostgreSQL connection string for audit logs. If omitted, audit logging is disabled (logs warning).

``talos_url`` (optional)
   Base URL of your Talos API key verification service. If omitted, ``actor_id`` must be provided via other means (e.g., headers).

``talos_admin_token`` (optional)
   Admin token for authenticating to Talos.

``talos_timeout_secs`` (optional, default: 2)
   HTTP timeout for Talos requests. If Talos is slow, requests return 503.

``verify_cache_ttl_secs`` (optional, default: 300)
   TTL for cached Talos verification responses (in seconds).

``minimum_balance`` (optional, default: 0.0)
   Minimum balance required to process a request. If balance < minimum, request is rejected with 402 Payment Required.

``pricing`` (optional)
   Map of model names to pricing configurations. Model names must match exactly (case-sensitive).

``default_pricing`` (required)
   Fallback pricing for models not in the ``pricing`` map.

Pricing Structure
~~~~~~~~~~~~~~~~~

Each pricing entry contains:

- ``input_per_million``: Cost per 1 million **input tokens** (USD)
- ``output_per_million``: Cost per 1 million **output tokens** (USD)
- ``cache_discount``: Multiplier for **cached input tokens** (0.0 = free, 1.0 = full price)

**Example Calculation:**

.. code-block:: text

   Model: gpt-4o
   Input tokens: 10,000 (5,000 cached)
   Output tokens: 2,000
   
   Input cost = (5,000 × $2.5/1M) + (5,000 × $2.5/1M × 0.5)
              = $0.0125 + $0.00625
              = $0.01875
   
   Output cost = 2,000 × $10.0/1M = $0.02
   
   Total cost = $0.03875
   Credits deducted = 38,750 (stored as integer)

Database Setup
--------------

Prerequisites
~~~~~~~~~~~~~

- PostgreSQL 12+ or Supabase
- Redis 6.0+
- Database credentials with CREATE TABLE privileges

Step 1: Create Audit Table
~~~~~~~~~~~~~~~~~~~~~~~~~~~

Run the SQL migration to create the ``billing_audit_log`` table:

.. code-block:: bash

   psql $DATABASE_URL -f docs/source/resources/db_setup/billing.sql

Or via Supabase Dashboard:

1. Navigate to **SQL Editor**
2. Copy contents of ``docs/source/resources/db_setup/billing.sql``
3. Execute query

**Table Schema:**

.. code-block:: sql

   CREATE TABLE billing_audit_log (
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

Step 2: Set Initial Balances
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Balances are stored in Redis. To set an initial balance for a user:

.. code-block:: bash

   redis-cli SET "plano:billing:balance:user_123" 10000000

This sets a balance of **$10.00** (stored as 10,000,000 credits).

**Balance Format:**

- Stored as **integer** (balance_usd × 1,000,000)
- Example: $5.50 → 5,500,000 credits
- Avoids floating-point precision errors

Step 3: Verify Setup
~~~~~~~~~~~~~~~~~~~~~

Check that the table was created:

.. code-block:: sql

   SELECT tablename FROM pg_tables WHERE tablename = 'billing_audit_log';

Test a balance lookup:

.. code-block:: bash

   redis-cli GET "plano:billing:balance:user_123"
   # Should return: "10000000"


How It Works
------------

Request Lifecycle
~~~~~~~~~~~~~~~~~

**1. API Key Verification**

When a request arrives with billing enabled, Plano extracts the API key (typically from ``Authorization`` header) and verifies it:

.. code-block:: rust

   let verify_response = billing_service.verify_key(api_key).await?;
   let actor_id = verify_response.actor_id;

- First checks LRU cache (default TTL: 5 minutes)
- If cache miss, calls Talos service
- Returns ``actor_id`` (user identifier) for balance lookup
- Invalid keys are rejected with 401 Unauthorized

**2. Balance Pre-Check**

Before forwarding the request to the LLM, Plano checks the user's balance:

.. code-block:: rust

   let balance = billing_service.check_balance(&actor_id).await?;
   if balance < billing_service.minimum_balance() {
       return Err(PaymentRequired); // 402 status
   }

- Reads from Redis: ``GET plano:billing:balance:{actor_id}``
- Compares against ``minimum_balance`` (configurable threshold)
- If insufficient, request is rejected immediately
- **Note:** Pre-check is not atomic with deduction (see Security Considerations)

**3. LLM Request Processing**

If balance is sufficient, the request is forwarded to the LLM provider. Plano waits for the response and extracts token usage.

**4. Cost Calculation**

After receiving the LLM response, Plano calculates the cost:

.. code-block:: rust

   let usage = extract_token_usage(&response);
   let cost = calculate_cost(&usage, model, &pricing, &default_pricing);

**Token Usage Sources:**

- **Reported:** Extracted from response ``usage`` field (OpenAI, Anthropic, etc.)
- **Estimated:** If no ``usage`` field, Plano estimates based on response length

**5. Credit Deduction**

Plano deducts credits from Redis atomically:

.. code-block:: bash

   DECRBY plano:billing:balance:user_123 38750

- Deduction is **integer-based** to avoid float precision errors
- If balance would go negative, deduction is **skipped** but audit log is still written
- Concurrent requests may over-deduct (see Race Conditions below)

**6. Audit Logging**

Every transaction is logged to PostgreSQL with full details:

.. code-block:: sql

   INSERT INTO billing_audit_log (
       actor_id, request_id, model, provider,
       prompt_tokens, completion_tokens, total_tokens,
       input_cost, output_cost, total_cost,
       balance_before, balance_after, usage_source
   ) VALUES (...);

- Audit happens asynchronously (fire-and-forget)
- If Postgres is unavailable, audit is lost but request succeeds
- ``balance_went_negative`` flag indicates if deduction was refused

Streaming Responses
~~~~~~~~~~~~~~~~~~~

For streaming LLM responses, billing is handled differently:

1. **Pre-check:** Same as non-streaming (checks balance before streaming starts)
2. **Streaming:** Response chunks are forwarded to client immediately
3. **Post-deduction:** After stream completes, deduction happens in background:

   .. code-block:: rust

      tokio::spawn(async move {
          billing_service.deduct_and_audit(...).await;
      });

**Important:** If ``brightstaff`` crashes during streaming, the deduction may be lost (see Known Limitations).

Monitoring & Metrics
--------------------

Plano exposes Prometheus metrics for billing health:

Verification Metrics
~~~~~~~~~~~~~~~~~~~~

``plano_billing_verification_total{status="cache_hit|cache_miss|error"}``
   Counter for API key verification attempts.

``plano_billing_verification_duration_seconds``
   Histogram of Talos verification latency.

Balance Metrics
~~~~~~~~~~~~~~~

``plano_billing_balance_check_total{status="success|error"}``
   Counter for balance check operations.

``plano_billing_balance_insufficient_total``
   Counter for requests rejected due to low balance (402 responses).

Deduction Metrics
~~~~~~~~~~~~~~~~~

``plano_billing_deduction_total{status="success|refused|error"}``
   Counter for credit deduction attempts.

   - ``success``: Deduction completed
   - ``refused``: Balance would go negative
   - ``error``: Redis or Postgres failure

``plano_billing_credits_deducted_total``
   Counter of total credits deducted (integer credits, not USD).

``plano_billing_cost_calculated_total{model, usage_source}``
   Counter of cost calculations by model and source (reported vs estimated).

Audit Metrics
~~~~~~~~~~~~~

``plano_billing_audit_write_total{status="success|error"}``
   Counter for audit log writes to PostgreSQL.

Grafana Dashboard
~~~~~~~~~~~~~~~~~

A pre-built Grafana dashboard is available at ``config/grafana/brightstaff_dashboard.json`` with panels for:

- Billing verification rate (cache hit ratio)
- Balance check latency
- Credits deducted per minute
- Top spending models
- Audit write success rate
- Failed deductions by reason

Security Considerations
-----------------------

Known Limitations
~~~~~~~~~~~~~~~~~

1. **Race Condition: Pre-check vs Deduction**

   The balance pre-check and deduction are **not atomic**. A burst of concurrent requests can all pass the pre-check, then over-deduct:

   .. code-block:: text

      Time  Request A         Request B         Redis Balance
      ────  ───────────────  ───────────────  ─────────────
      T0    Check: $10.00    —                $10.00
      T1    —                Check: $10.00    $10.00
      T2    LLM call ($8)    LLM call ($8)    $10.00
      T3    Deduct $8        —                $2.00
      T4    —                Deduct $8        -$6.00 ⚠️

   **Mitigation:** For strict enforcement, use Lua scripts in Redis to make check+deduct atomic. Current implementation is suitable for **soft gates** (occasional over-deduction is acceptable).

2. **Fire-and-Forget Deduction**

   Streaming responses spawn a background task for deduction:

   .. code-block:: rust

      tokio::spawn(async move {
          billing_service.deduct_and_audit(...).await;
      });

   If ``brightstaff`` crashes or the task panics, the charge is **lost**. This is a **revenue leak** for production billing.

   **Mitigation:** Use persistent job queue (e.g., Celery, SQS) for deduction tasks, or log deductions to a durable write-ahead log.

3. **Audit Database Uses NoTls**

   By default, the Postgres connection uses ``NoTls`` (plaintext). Credentials in the connection string transit unencrypted.

   **Mitigation:** Use TLS-enabled Postgres connections in production (e.g., Supabase with ``?sslmode=require``).

4. **Talos Timeout Hardcoded**

   If Talos is slow, every billing-enabled request returns **503 Service Unavailable** after 2 seconds.

   **Mitigation:** Make timeout configurable (currently ``talos_timeout_secs`` in config).

5. **Floating-Point Precision**

   Intermediate cost calculations use ``f64``, which can accumulate rounding errors at scale.

   **Mitigation:** Redis stores credits as integers (× 1,000,000). For higher precision, use fixed-point arithmetic crate (e.g., ``rust_decimal``).

Best Practices
~~~~~~~~~~~~~~

1. **Always enable audit logging** — Required for compliance, chargebacks, and analytics
2. **Monitor Talos cache hit ratio** — Should be >90% to avoid performance bottlenecks
3. **Set minimum_balance conservatively** — Prevents accidental over-spending
4. **Use Supabase or RDS** — Managed Postgres reduces operational burden
5. **Alert on negative balances** — Query ``balance_went_negative = true`` in audit logs
6. **Rotate Talos tokens regularly** — Limit blast radius of compromised credentials

Troubleshooting
---------------

Request Rejected with 402 Payment Required
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

**Cause:** User balance is below ``minimum_balance``.

**Solution:**

1. Check current balance:

   .. code-block:: bash

      redis-cli GET "plano:billing:balance:user_123"

2. Add credits:

   .. code-block:: bash

      redis-cli INCRBY "plano:billing:balance:user_123" 5000000  # Add $5.00

3. Verify in audit logs:

   .. code-block:: sql

      SELECT balance_after FROM billing_audit_log
      WHERE actor_id = 'user_123'
      ORDER BY created_at DESC LIMIT 1;

Request Rejected with 503 Service Unavailable
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

**Cause:** Talos verification timed out (default: 2s).

**Solution:**

1. Check Talos health:

   .. code-block:: bash

      curl -H "Authorization: Bearer $TALOS_ADMIN_TOKEN" \
           $TALOS_URL/health

2. Increase timeout in config:

   .. code-block:: yaml

      billing:
        talos_timeout_secs: 5

3. Monitor Talos latency:

   .. code-block:: promql

      histogram_quantile(0.99, plano_billing_verification_duration_seconds)

Audit Logs Not Written
~~~~~~~~~~~~~~~~~~~~~~~

**Cause:** ``audit_database_url`` is misconfigured or Postgres is unreachable.

**Solution:**

1. Check logs for connection errors:

   .. code-block:: bash

      docker logs plano-brightstaff | grep "billing audit database"

2. Test Postgres connection:

   .. code-block:: bash

      psql "$AUDIT_DATABASE_URL" -c "SELECT 1;"

3. Verify table exists:

   .. code-block:: sql

      \dt billing_audit_log

Negative Balances in Audit Logs
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

**Cause:** Concurrent requests passed pre-check but collectively over-deducted.

**Solution:**

1. Query affected users:

   .. code-block:: sql

      SELECT actor_id, balance_after
      FROM billing_audit_log
      WHERE balance_went_negative = true
      GROUP BY actor_id, balance_after;

2. Refund over-deducted amount:

   .. code-block:: bash

      redis-cli INCRBY "plano:billing:balance:user_123" 1000000  # Refund $1.00

3. Consider atomic check+deduct (see Race Conditions above).

Examples
--------

Basic Configuration
~~~~~~~~~~~~~~~~~~~

Minimal billing setup with default pricing:

.. code-block:: yaml

   billing:
     redis_url: "redis://localhost:6379"
     audit_database_url: "postgresql://postgres:pass@localhost:5432/plano"
     minimum_balance: 0.01
     
     default_pricing:
       input_per_million: 5.0
       output_per_million: 15.0
       cache_discount: 1.0

Multi-Model Pricing
~~~~~~~~~~~~~~~~~~~

Different pricing for GPT-4, Claude, and Gemini:

.. code-block:: yaml

   billing:
     redis_url: "redis://localhost:6379"
     audit_database_url: "postgresql://postgres:pass@localhost:5432/plano"
     
     pricing:
       "gpt-4o":
         input_per_million: 2.5
         output_per_million: 10.0
         cache_discount: 0.5
       
       "gpt-4o-mini":
         input_per_million: 0.15
         output_per_million: 0.6
         cache_discount: 0.5
       
       "claude-3-5-sonnet-20241022":
         input_per_million: 3.0
         output_per_million: 15.0
         cache_discount: 0.1
       
       "gemini-1.5-pro":
         input_per_million: 1.25
         output_per_million: 5.0
         cache_discount: 1.0
     
     default_pricing:
       input_per_million: 5.0
       output_per_million: 15.0
       cache_discount: 1.0

Talos Integration
~~~~~~~~~~~~~~~~~

With external API key verification:

.. code-block:: yaml

   billing:
     redis_url: "redis://localhost:6379"
     audit_database_url: "postgresql://postgres:pass@localhost:5432/plano"
     
     talos_url: "https://api.talos.example.com"
     talos_admin_token: "${TALOS_ADMIN_TOKEN}"
     talos_timeout_secs: 3
     verify_cache_ttl_secs: 600  # 10 minutes
     
     minimum_balance: 1.0  # $1.00 minimum
     
     default_pricing:
       input_per_million: 5.0
       output_per_million: 15.0
       cache_discount: 1.0

Querying Usage Analytics
~~~~~~~~~~~~~~~~~~~~~~~~~

Top spending users in the last 7 days:

.. code-block:: sql

   SELECT 
       actor_id,
       COUNT(*) as request_count,
       SUM(total_cost) as total_spent,
       SUM(prompt_tokens) as total_input_tokens,
       SUM(completion_tokens) as total_output_tokens
   FROM billing_audit_log
   WHERE created_at > NOW() - INTERVAL '7 days'
   GROUP BY actor_id
   ORDER BY total_spent DESC
   LIMIT 10;

Cost breakdown by model:

.. code-block:: sql

   SELECT 
       model,
       COUNT(*) as request_count,
       SUM(total_cost) as total_cost,
       AVG(total_cost) as avg_cost_per_request,
       SUM(CASE WHEN usage_source = 'estimated' THEN 1 ELSE 0 END) as estimated_count
   FROM billing_audit_log
   WHERE created_at > NOW() - INTERVAL '24 hours'
   GROUP BY model
   ORDER BY total_cost DESC;

Users with negative balance events:

.. code-block:: sql

   SELECT 
       actor_id,
       COUNT(*) as negative_event_count,
       MIN(balance_after) as lowest_balance,
       MAX(created_at) as last_occurrence
   FROM billing_audit_log
   WHERE balance_went_negative = true
   GROUP BY actor_id
   ORDER BY negative_event_count DESC;

See Also
--------

- :doc:`/concepts/agents` — Understanding agent routing and orchestration
- :doc:`/guides/state` — Conversation state management
- :doc:`/guides/observability/observability` — Monitoring and tracing
- :doc:`/resources/configuration_reference` — Full configuration options
- :doc:`/resources/db_setup/README` — Database setup instructions

