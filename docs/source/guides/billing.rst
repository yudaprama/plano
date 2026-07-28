Billing & Usage Tracking
========================

Plano tracks LLM usage and deducts credits from user balances via an **external metering pipeline**. Brightstaff stamps billing metadata on OTLP trace spans; a separate metering service extracts usage, calculates costs, and debits balances through Talos.

.. contents:: Table of Contents
   :local:
   :depth: 2

Overview
--------

The billing system provides:

- **Per-request cost calculation** based on token usage and model pricing
- **Prepaid credit balances** managed by Talos (PostgreSQL-backed)
- **Per-session usage tracking** with session ID attribution
- **Usage ledger** in Talos for compliance and analytics
- **Frontend dashboard** showing balance, daily spend, and per-session breakdown

Architecture
------------

Billing Flow
~~~~~~~~~~~~

.. code-block:: text

   Client Request
        ↓
   [1] Oathkeeper edge — verifies Kratos session, injects X-User-Id
        ↓
   [2] Brightstaff — stamps billing.actor_id + billing.model_alias on OTLP span
        ↓
   [3] LLM Request Forwarded to provider
        ↓
   [4] LLM Response — token usage in response
        ↓
   [5] Alloy — forwards OTLP spans to metering service (:4319)
        ↓
   [6] Metering service — extracts events, applies pricing, calculates cost
        ↓
   [7] Talos HTTP API — debits actor_balances + writes api_key_usage
        ↓
   Response to Client

Components
~~~~~~~~~~

**Brightstaff** (``crates/brightstaff/src/streaming.rs``)
   Stamps ``billing.actor_id`` and ``billing.model_alias`` on OTLP spans. Brightstaff no longer handles balance checks, cost calculation, or deduction — that moved to the external metering service.

**Metering service** (``metering/``)
   Receives OTLP spans from Alloy, extracts billable LLM-completion events, applies per-model pricing from ``pricing.yaml``, and debits the actor's balance via Talos HTTP API.

**Talos** (``talos/``)
   Manages ``actor_balances`` (credit storage) and ``api_key_usage`` (usage ledger). Provides self-service APIs for balance queries and usage history.

**Alloy**
   OTLP collector that forwards traces from brightstaff to the metering service.

Configuration
-------------

Pricing is configured in ``pricing.yaml`` (used by the metering service):

.. code-block:: yaml

   default_pricing:
     input_per_million: 5.0
     output_per_million: 15.0
     cache_discount: 1.0

   pricing:
     kawai-pro-max:
       input_per_million: 2.5
       output_per_million: 10.0
       cache_discount: 0.5
     claude-3-5-sonnet-20241022:
       input_per_million: 3.0
       output_per_million: 15.0
       cache_discount: 0.1

Environment variables (``.env``):

.. code-block:: bash

   METERING_PLANO_CONFIG=./plano_config.yaml  # pricing source
   TALOS_URL=http://localhost:4420             # Talos API endpoint

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
   Credits deducted = 38,750 (stored as integer micros in Talos)

How It Works
------------

Brightstaff's Role
~~~~~~~~~~~~~~~~~~

Brightstaff stamps billing metadata on OTLP spans so the metering service can attribute usage:

.. code-block:: rust

   // crates/brightstaff/src/streaming.rs
   fn prepare_billing(&mut self, _usage: &ExtractedUsage) {
       // Stamp actor_id for metering attribution
       otel.set_attribute(KeyValue::new("billing.actor_id", actor_id));
       // Stamp model alias for display in usage ledger
       otel.set_attribute(KeyValue::new("billing.model_alias", alias));
   }

Brightstaff does **not**:
- Verify API keys (handled by Oathkeeper + Talos)
- Check balances (handled by metering service)
- Deduct credits (handled by Talos via metering service)
- Write audit logs (handled by Talos ``api_key_usage`` table)

Metering Pipeline
~~~~~~~~~~~~~~~~~

The metering service (``metering/``) is an OTLP/gRPC trace receiver:

1. **Extract** — Parses billable spans from Alloy, extracting ``billing.actor_id``, ``llm.model``, token counts, and ``plano.session_id``
2. **Price** — Looks up per-model pricing from ``pricing.yaml``, calculates cost in micros
3. **Debit** — HTTP POST to Talos ``/v2alpha1/admin/usage:ingest`` with actor ID, cost, model, session ID
4. **Retry** — Failed debits are enqueued to egent-jobs for durable retry

Talos stores:
- ``actor_balances`` — per-actor credit balance (debit on ingest)
- ``api_key_usage`` — append-only usage ledger with ``session_id``, ``model``, ``cost_micros``, ``created_at``

Querying Usage
--------------

Self-Service API
~~~~~~~~~~~~~~~~

.. code-block:: bash

   # Get balance (remaining / quota in micros)
   curl -H "Authorization: Bearer $TOKEN" \
        $TALOS_URL/v2alpha1/self/actorBalance

   # Get usage history (per-session, per-model breakdown)
   curl -H "Authorization: Bearer $TOKEN" \
        $TALOS_URL/v2alpha1/self/usageHistory?limit=50

Response shapes:

.. code-block:: json

   // GET /v2alpha1/self/actorBalance
   { "quotaMicros": "10000000", "remainingMicros": "8500000" }

   // GET /v2alpha1/self/usageHistory
   {
     "records": [
       {
         "model": "kawai-pro-max",
         "costMicros": 38750,
         "createdAt": "2026-07-28T10:30:00Z",
         "sessionId": "sess_abc123",
         "usageType": "tokens"
       }
     ]
   }

Frontend
~~~~~~~~

The ``web/`` SPA displays usage in Settings → Usage:

- **Balance card** — shows remaining credits (``useBalance`` hook → ``/v2alpha1/self/actorBalance``)
- **Daily spend chart** — bar chart of cost per day (``useUsageHistory`` → bucketed by ``createdAt``)
- **Spend by session** — grouped by ``sessionId``, sorted by cost descending
- **Spend by type** — grouped by ``usageType`` (tokens, image_generation, etc.)

Source: ``web/src/lib/billing.ts``, ``web/src/components/settings-view.tsx``

See Also
--------

- :doc:`/concepts/agents` — Understanding agent routing and orchestration
- :doc:`/guides/state` — Conversation state management
- :doc:`/guides/observability/observability` — Monitoring and tracing
