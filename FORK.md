# Plano (yudaprama fork)

This is a personal fork of [katanemo/plano](https://github.com/katanemo/plano).
It exists to ship features and workflow changes that are useful to the fork
maintainer but are not (yet) appropriate — or have not been accepted — upstream.

Upstream: <https://github.com/katanemo/plano>
Fork: <https://github.com/yudaprama/plano>

## Why a fork

Two reasons, in order of importance:

1. **Billing & usage tracking.** Adds an opt-in billing flow on top of the LLM
   gateway: API-key verification, per-user balance enforcement, per-request
   cost calculation, and an audit log. See the full feature guide at
   `docs/source/guides/billing.rst`.
2. **Fork-friendly release workflow.** The CI in upstream assumes the repo
   lives at `katanemo/plano`. The fork ships `latest` and release Docker
   images to GHCR under `ghcr.io/yudaprama/plano` and supports a fork-style
   tag scheme (e.g. `0.4.24-yuda.1`).

## What diverges from upstream

These commits are on the fork's `main` but not in `upstream/main` (as of
`ccf1af2a`):

| Commit    | Title                                                                          |
| --------- | ------------------------------------------------------------------------------ |
| `38242491` | Add billing flow and fork release workflows                                  |
| `cf0fd509` | Bugs / correctness concerns (hardening for the billing flow)                   |
| `3a0f8ee9` | README + docs index entry for the billing guide                                |
| `ccf1af2a` | Merge of the latest `upstream/main`                                            |
| `acba49fb` | Multi-key `access_key` (comma-separated credentials, one picked per request)   |
| `7a3ec4d6` | Multi-target model aliases with health-aware failover (429/5xx cooldown)       |
| `a4df5492` | Schema `anyOf` for `target`/`targets` + `cargo fmt` pass                       |

### 1. Billing flow (`38242491`)

New crate: `crates/brightstaff/src/billing/`

- `mod.rs` — `BillingService` wiring (Redis-backed balance + usage logging)
- `talos.rs` — Talos key-verification HTTP client
- `balance.rs` — Redis-backed balance read/decrement (`DECRBY` × 1e6)
- `cost.rs` — Per-model cost calculation (input/output tokens, cache discount)
- `verify_cache.rs` — TTL cache for Talos verification results

Plumbed into:

- `crates/brightstaff/src/handlers/llm/mod.rs` — verify → pre-check →
  forward → deduct → log
- `crates/brightstaff/src/streaming.rs` — same flow for streaming responses
- `crates/brightstaff/src/metrics/mod.rs` — `plano_billing_*` Prometheus
  metrics
- `crates/brightstaff/src/tracing/constants.rs` — new trace fields
  (`billing.actor_id`, `billing.cost`, ...)
- `crates/common/src/configuration.rs` — `billing` config block
- `config/plano_config_schema.yaml` — JSON schema for the new block
- `demos/getting_started/weather_forecast/config.yaml` — example config

Usage is logged to Talos for audit and analytics.

### 2. Hardening pass (`cf0fd509`)

The same author walked back over the billing code and fixed real issues.
Highlights:

- **Atomic deduction.** Pre-check + deduction split into a Lua-scripted
  Redis CAS, removing the "burst passes pre-check then over-deducts" race.
- **No more fire-and-forget.** Streaming deduction moved off
  `tokio::spawn` onto a tracked task; panics now log and don't silently
  drop revenue.
- **Mutex poisoning is logged**, not silently swallowed
  (`verify_cache.rs`).
- **Configurable Talos timeout** (was a hardcoded 2 s).
- **Removed dead `billing_balances` table** from the migration — balance
  lives only in Redis.
- **Fixed README path** to `billing.sql` (`docs/db_setup/...` →
  `docs/source/resources/db_setup/...`).

### 3. Docs pass (`3a0f8ee9`)

- New 700+ line guide: `docs/source/guides/billing.rst`
- README in the weather-forecast demo gets an "optional billing" section
- `docs/source/index.rst` toctree links the guide

### 4. Fork release workflows

Rewritten `.github/workflows/`:

- `docker-push-release.yml` — Pushes to `ghcr.io/${{ github.repository }}`
  on GitHub release **or** manual `workflow_dispatch` with a custom tag.
  This lets the fork publish `0.4.24-yuda.1` style tags without having to
  create an upstream-shaped GitHub release first.
- `docker-push-main.yml` — Pushes `latest` and `sha-<short>` on every push
  to `main`.
- `ci.yml`, `publish-binaries.yml`, `publish-pypi.yml` — Minor tweaks so
  the publish step targets the fork's namespace.

The CLI reads `PLANO_GITHUB_REPO` and `PLANO_DOCKER_IMAGE` from the
environment (`cli/planoai/consts.py`), with the upstream values as
defaults. To point the CLI at the fork's images, set:

```bash
export PLANO_GITHUB_REPO=yudaprama/plano
export PLANO_DOCKER_IMAGE=ghcr.io/yudaprama/plano:0.5.7
```

### 5. Multi-key `access_key` (`acba49fb`)

A provider's `access_key` may be a **comma-separated list** of credentials
instead of a single key. When several are configured, Plano picks one per
request so load spreads across the keys — handy for upstreams that rate-limit
per key (e.g. Ollama Cloud's `$OLLAMA_API_KEYS`).

```yaml
- model: ollama/gpt-oss:120b
  provider_interface: openai
  access_key: $OLLAMA_API_KEYS   # OLLAMA_API_KEYS="k1,k2,k3"
```

- Implemented in `crates/llm_gateway/src/stream_context.rs` (`pick_access_key`),
  at the point the upstream credential is resolved. A single key (no comma) is
  forwarded unchanged (whitespace trimmed).
- Selection is **random per request**, not strict round-robin: the choice is
  seeded from the host clock's nanoseconds via the proxy-wasm `get_current_time`
  hostcall, because `rand`/`getrandom` are unavailable in the WASM sandbox.
  Over many requests the keys are hit roughly evenly, but consecutive requests
  can repeat a key.
- Entries are trimmed and empty ones (e.g. a trailing comma) are skipped.
- Env expansion happens at config-render time (planoctl `expandEnvWithMap`), so
  the comma-separated env value lands verbatim in `access_key` before Plano
  splits it.

### 6. Multi-target model aliases with health-aware failover (`7a3ec4d6`)

A `model_aliases` entry may now list **multiple backend targets**. When more
than one is configured, brightstaff picks a healthy one per request and
automatically fails over to the next on a retryable upstream error (`429`,
`500`, `502`, `503`, `504`) or a connection error.

```yaml
model_aliases:
  kawai-pro-max:
    targets:
      - openai/gpt-4o
      - openai/gpt-4.1
      - anthropic/claude-3-7-sonnet  # NB: same provider_interface assumed
```

The single-target form is still supported (backward-compatible):

```yaml
model_aliases:
  kawai-pro-max:
    target: openai/gpt-4o
```

- Implemented in `crates/common/src/configuration.rs` (`ModelAlias::candidates`,
  `ModelAlias::primary`) and `crates/brightstaff/src/handlers/llm/mod.rs`
  (`resolve_alias_candidates`, the new attempt loop in `send_upstream`).
- Health tracking lives in `crates/brightstaff/src/handlers/llm/health.rs`
  (`ModelHealthTracker`): an in-memory, process-local `HashMap<model, cooldown>`.
  - Default cooldown **30 s**; capped at **300 s** so a hostile `Retry-After`
    can't park a backend indefinitely.
  - `Retry-After` is honored when it's an integer-seconds value; the HTTP-date
    form is **not** parsed (falls back to the default).
  - Candidates are partitioned into available-first (shuffled for load
    spreading) then cooled-down (shuffled as a last resort), so a request
    never hard-fails just because every backend is in cooldown.
  - A non-retryable response (incl. `4xx`) clears the cooldown for that backend.
- Interaction with the orchestrator/router: when the router explicitly selects
  a model (route override) or a session has a pinned decision, that single
  model is honored and **no** alias-level health fallback is attempted.
- Config schema (`config/plano_config_schema.yaml`) uses `anyOf` so either
  `target` or `targets` is accepted; `additionalProperties: false` is preserved.

Caveats:

- **In-memory and per-process.** Cooldown state is not shared across replicas
  (each replica learns independently). No persistence.
- **Same `provider_interface` assumed.** The request body is normalized once
  for the primary candidate, so all `targets` must share a provider interface
  (e.g. all OpenAI-compatible). This is documented in the struct but **not
  enforced in code** — mixing interfaces will silently misbehave.

### 7. In-process Rig agent (`type: rig`)

A top-level `agents[]` entry may set `type: rig`. Such an agent is **not** an
external HTTP service (unlike the egents, which brightstaff proxies to over
`/v1/chat/completions`); instead brightstaff runs its tool loop **in-process**
via the `rig` crate, against plano's own model gateway (`llm_provider_url`).

```yaml
agents:
  - id: egent_rig_demo
    url: http://127.0.0.1:9   # placeholder; never contacted
    type: rig
```

- New crate `crates/rig_agent` (native-only; depends on `rig`). Exposes
  `run_chat(user_text, model, gateway_root, api_key) -> Result<String>` which
  builds an OpenAI-compatible `rig` client pointed at `{llm_provider_url}/v1`
  and runs a tool loop. The PoC ships a single `current_time` tool.
- `crates/brightstaff/src/handlers/agents/orchestrator.rs::execute_agent_chain`
  branches on `agent.agent_type == Some("rig")` **before** the Envoy proxy
  path (`PipelineProcessor::invoke_agent`): it calls `rig_agent::run_chat` and,
  for the terminal agent, returns a non-streaming `chat.completion` JSON
  response built directly (mirrors `function_calling_chat_handler`). `llm_provider_url`
  is threaded through as a new param from `handle_agent_chat_inner`.
- `config/plano_config_schema.yaml` allows `type: rig` on `agents[]` items
  (previously `id`+`url` only, `additionalProperties: false`).
- The `url` is still required by the schema; it's a placeholder sinkhole
  (`http://127.0.0.1:9`) that Envoy clusters onto but brightstaff never
  contacts, because the rig branch short-circuits first.

Caveats / known PoC limits:

- **Non-streaming only.** The rig path returns a single `chat.completion`
  JSON. Clients sending `stream:true` (e.g. the web UI) are not yet supported
  — SSE `chat.completion.chunk` framing is a follow-up.
- **No Responses-API support.** A `type: rig` agent returns an error if the
  client used `/v1/responses`.
- **Auth to the gateway is unverified.** The rig client sends a bearer from
  `$PLANO_INTERNAL_KEY` (placeholder `plano-internal` if unset) to the same
  `llm_provider_url` the orchestrator already calls loopback. If that endpoint
  enforces `x-arch-internal-key`, a custom reqwest client must be wired in.
- **Extra dep weight.** `rig` brings `reqwest 0.13` alongside brightstaff's
  `reqwest 0.12` (two majors coexist) plus `async-openai`, growing compile time
  and binary size. Native binary only — never pulled into the WASM crates.

## Known concerns (carried forward from `cf0fd509`)

These are documented in the commit body but not yet fixed:

1. **Audit DB uses `NoTls`** (`crates/brightstaff/src/billing/mod.rs`).
   Consistent with the existing `state_storage` pattern, but credentials
   in the Postgres DSN transit in plaintext. Use a private network or
   switch to `tokio-postgres-rustls` for production.
2. **`f64` for monetary arithmetic** (`crates/brightstaff/src/billing/cost.rs`).
   Redis stores the value as integer × 1e6, which mitigates the worst of
   it, but intermediate `input_cost` / `output_cost` are still `f64`. At
   scale this accumulates. Consider a fixed-point type.
3. **No upstream-friendliness in the CLI defaults.** Earlier in the fork's
   life, `PLANO_GITHUB_REPO` and `PLANO_DOCKER_IMAGE` were hard-coded to
   `yudaprama/...`. The current code uses env-var fallbacks with
   `katanemo/...` as default — keep it that way if the fork ever opens a
   PR upstream.

## Syncing with upstream

```bash
git fetch upstream
git checkout main
git merge upstream/main   # or rebase, depending on taste
# resolve any conflicts in cli/planoai/consts.py, release scripts, etc.
```

A conflict in `cli/planoai/consts.py` is the most likely pain point —
upstream may have changed the version constant or added new env-var
fallbacks.

## Things that did NOT change

The WASM plugins (`prompt_gateway`, `llm_gateway`), the `hermesllm` crate,
and the Envoy template are upstream-identical. The fork adds the billing
module on top of `brightstaff`, patches the release workflows, (since
`7a3ec4d6`) extends `brightstaff`'s LLM proxy flow with multi-target alias
routing and health-aware failover, and (since §7) adds an in-process Rig
agent path (`crates/rig_agent` + a `type: rig` branch in `brightstaff` and
the config schema).
