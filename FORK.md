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

### 1. Billing flow (`38242491`)

New crate: `crates/brightstaff/src/billing/`

- `mod.rs` — `BillingService` wiring (Redis-backed balance + Postgres audit)
- `talos.rs` — Talos key-verification HTTP client
- `balance.rs` — Redis-backed balance read/decrement (`DECRBY` × 1e6)
- `cost.rs` — Per-model cost calculation (input/output tokens, cache discount)
- `verify_cache.rs` — TTL cache for Talos verification results

Plumbed into:

- `crates/brightstaff/src/handlers/llm/mod.rs` — verify → pre-check →
  forward → deduct → audit
- `crates/brightstaff/src/streaming.rs` — same flow for streaming responses
- `crates/brightstaff/src/metrics/mod.rs` — `plano_billing_*` Prometheus
  metrics
- `crates/brightstaff/src/tracing/constants.rs` — new trace fields
  (`billing.actor_id`, `billing.cost`, ...)
- `crates/common/src/configuration.rs` — `billing` config block
- `config/plano_config_schema.yaml` — JSON schema for the new block
- `demos/getting_started/weather_forecast/config.yaml` — example config

New database table: `billing_audit_log`
(migration: `docs/source/resources/db_setup/billing.sql`).

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
export PLANO_DOCKER_IMAGE=ghcr.io/yudaprama/plano:0.4.24
```

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
the Envoy template, and the core `brightstaff` LLM proxy flow are
upstream-identical. The fork only adds the billing module on top of
`brightstaff` and patches the release workflows.
