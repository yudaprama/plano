# CLAUDE.md

Plano is an AI-native proxy server and data plane for agentic applications, built on Envoy proxy. It centralizes agent orchestration, LLM routing, observability, and safety guardrails as an out-of-process dataplane.

> **This is the `yudaprama/plano` fork of [katanemo/plano](https://github.com/katanemo/plano).**
> See [FORK.md](FORK.md) for what diverges from upstream (billing flow, fork release workflows), the commits ahead of `upstream/main`, known concerns, and how to sync.

## Build & Test Commands

```bash
# Rust — WASM plugins (must target wasm32-wasip1)
cd crates && cargo build --release --target=wasm32-wasip1 -p llm_gateway -p prompt_gateway

# Rust — brightstaff binary (native target)
cd crates && cargo build --release -p brightstaff

# Rust — tests, format, lint
cd crates && cargo test --lib
cd crates && cargo fmt --all -- --check
cd crates && cargo clippy --locked --all-targets --all-features -- -D warnings

# Python CLI
cd cli && uv sync && uv run pytest -v

# JS/TS (Turbo monorepo)
npm run build && npm run lint && npm run typecheck

# Pre-commit (fmt, clippy, cargo test, black, yaml)
pre-commit run --all-files

# Docker
docker build -t katanemo/plano:latest .
```

E2E tests require a Docker image and API keys: `tests/e2e/run_e2e_tests.sh`

## Architecture

```
Client → Envoy (prompt_gateway.wasm → llm_gateway.wasm) → Agents/LLM Providers
                              ↕
                         brightstaff (native binary: state, routing, signals, tracing)
```

### Crates (crates/)

- **prompt_gateway** (WASM) — Proxy-WASM filter for prompt processing, guardrails, filter chains
- **llm_gateway** (WASM) — Proxy-WASM filter for LLM request/response handling and routing
- **brightstaff** (native) — Core server: handlers, router, signals, state, tracing
- **common** (lib) — Shared: config, HTTP, routing, rate limiting, tokenizer, PII, tracing
- **hermesllm** (lib) — LLM API translation between providers. Key types: `ProviderId`, `ProviderRequest`, `ProviderResponse`, `ProviderStreamResponse`

### Python CLI (cli/planoai/)

Entry point: `main.py`. Built with `rich-click`. Commands: `up`, `down`, `build`, `logs`, `trace`, `init`, `cli_agent`.

### Config (config/)

- `plano_config_schema.yaml` — JSON Schema for validating user configs
- `envoy.template.yaml` — Jinja2 template → Envoy config
- `supervisord.conf` — Process supervisor for Envoy + brightstaff

### JS Apps (apps/, packages/)

Turbo monorepo with Next.js 16 / React 19. Not part of the core proxy.

## WASM Plugin Rules

Code in `prompt_gateway` and `llm_gateway` runs in Envoy's WASM sandbox:

- **No std networking/filesystem** — use proxy-wasm host calls only
- **No tokio/async** — synchronous, callback-driven. `Action::Pause` / `Action::Continue` for flow control
- **Lifecycle**: `RootContext` → `on_configure`, `create_http_context`; `HttpContext` → `on_http_request/response_headers/body`
- **HTTP callouts**: `dispatch_http_call()` → store context in `callouts: RefCell<HashMap<u32, CallContext>>` → match in `on_http_call_response()`
- **Config**: `Rc`-wrapped, loaded once in `on_configure()` via `serde_yaml::from_slice()`
- **Dependencies must be no_std compatible** (e.g., `governor` with `features = ["no_std"]`)
- **Crate type**: `cdylib` → produces `.wasm`

## Adding a New LLM Provider

1. Add variant to `ProviderId` in `crates/hermesllm/src/providers/id.rs` + `TryFrom<&str>`
2. Create request/response types in `crates/hermesllm/src/apis/` if non-OpenAI format
3. Add variant to `ProviderRequestType`/`ProviderResponseType` enums, update all match arms
4. Add models to `crates/hermesllm/src/providers/provider_models.yaml`
5. Update `SupportedUpstreamAPIs` mapping if needed

## Release Process

Releases follow the **metering pattern**: push to `main` → workflow builds binaries → creates GitHub Release with `v${{ github.run_number }}` tag.

**Workflow:** `.github/workflows/publish-binaries.yml`
- Triggers on push to `main`
- Builds WASM plugins (ubuntu) + brightstaff binaries (linux amd64/arm64, darwin arm64)
- Creates release with all assets + CHECKSUMS.txt

**Disabled workflows** (manual trigger only):
- `ci.yml`, `docker-push-main.yml`, `docker-push-release.yml`, `publish-pypi.yml`, `static.yml`

**To release:** Just push to `main`. The workflow auto-creates a release at `https://github.com/yudaprama/plano/releases/tag/vN`.

**Version in ai-orchestration/main.go:** Update `version` constant to match the release tag (e.g., `v13`).

## Workflow Preferences

- **Commits:** Short one-line messages. Commit directly to `main` (no feature branches).
- **Upstream:** Do not look at or fetch the `upstream` remote (katonemo/plano) unless I explicitly ask you to. Focus only on this fork's `origin` remote.

## Key Conventions

- Rust edition 2021, `cargo fmt`, `cargo clippy -D warnings`
- Python: Black. Rust errors: `thiserror` with `#[from]`
- API keys from env vars or `.env`, never hardcoded
- Provider dispatch: `ProviderRequestType`/`ProviderResponseType` enums implementing `ProviderRequest`/`ProviderResponse` traits
