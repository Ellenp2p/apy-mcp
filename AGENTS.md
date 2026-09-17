# AGENTS.md

Rust server aggregating DeFi lending/borrowing rates. It serves three surfaces
from one binary: an embedded **web UI** (`/`, static assets), a **Web API**
(`/api/v1/*`), and an optional **MCP** `query_rates` tool over MCP Streamable
HTTP at `/mcp`. Auth for `/api/*` and `/mcp` is GitHub OAuth (or API keys).

## Project goals

- Aggregate lending/borrowing APY across chains and protocols behind one
  unified query surface (filter by chain, asset, protocol, APY range,
  utilization), exposed both as REST (`/api/v1/rates`) and as the MCP
  `query_rates` tool — a single query core (`RateService`) drives both.
- Coverage today: Aave V3 (11 EVM chains), Spark Savings (spUSDC/spUSDT),
  Blend (Stellar). New protocols should plug in as a `LendingProvider`
  implementation and be wired into `src/service/rates.rs`, not special-cased.
- Rate data must be fresh and cheap to serve: rely on official protocol
  data sources (contract reads / official APIs), cache aggressively
  (120s SQLite), and keep fetch paths resilient (timeouts, per-source
  isolation so one failing source doesn't fail the whole query).
- Production deployment is a public HTTPS server with GitHub OAuth —
  auth and OAuth flows are invariants, not optional features.

## Commands

```bash
cargo check
cargo test            # unit tests (fast, offline)
cargo clippy
```

Cargo features (both on by default):
```bash
cargo build                                  # mcp + web
cargo build --no-default-features            # core API only (no /mcp, no web UI)
cargo build --no-default-features --features web
cargo build --no-default-features --features mcp
```

Live network tests are `#[ignore]`d and hit real RPCs/APIs (may be flaky):
```bash
cargo test test_live_ -- --ignored --nocapture
```

## Architecture

- `src/chains/{evm,stellar}/` — protocol providers; each implements the
  `LendingProvider` trait (`src/chains/mod.rs`): `chain_name`, `protocol_name`,
  `get_pool_rates`, `list_pools`.
  - `evm/aave.rs` — Aave V3 contract reads (`getAllReservesTokens` /
    `getReserveData` via `eth_call`, hand-rolled ABI decode).
  - `evm/savings.rs` + `SparkSavingsProvider` — Spark Savings (spUSDC/spUSDT)
    via the official REST API `api.spark.fi/v1/savings/{protocol}/{chain}/{token}`.
  - `stellar/blend.rs` — Blend pools via Soroban `getLedgerEntries` (XDR decode).
- `src/service/` — protocol-agnostic query core.
  - `rates.rs` — `RateService`: owns the providers + `monitored_pools` + DB
    cache; `query`/`add_pool`/`list_pools` (filters, 120s SQLite cache).
  - `types.rs` — shared `PoolRates`/`AssetRate`/`AllRatesResponse` and
    `QueryRatesParams` (used by both REST and MCP).
- `src/api.rs` — REST handlers for `/api/v1/{rates,chains,protocols,pools}`.
- `src/mcp/tools.rs` — `#[tool_router]` shell over `RateService` (only with the
  `mcp` feature; the JSON output shape of `query_rates` must stay compatible).
- `src/web.rs` — the `/` page, rendered with **Dioxus SSR** (`#[component]`s +
  `dioxus::ssr::render_element`; `web` feature only). Initial visit serves
  straight from the 120s cache (never blocks on RPC; cold cache kicks a
  background refresh and shows a "refresh shortly" notice). The filter form
  submits GET to `/` and the handler re-renders with fresh results (8s
  timeout, falls back to cached data). Sort-header clicks and filter-form
  submits go through **htmx** (embedded as `web/htmx.min.js`, ~50KB,
  rust-embedded) and swap the `<div id="results-region">` fragment in
  place — no full reload, no scroll jump, filter form state preserved. Server
  detects `HX-Request: true` (or `?_fragment=results` for curl/no-JS) and
  returns only the swap region instead of the full document. The form
  carries a hidden `force_refresh=1` so htmx form submits run a fresh query
  (with 8s timeout + cache fallback) while sort-header clicks — which don't
  include `force_refresh` — re-render straight from the 120s cache.
  `src/http.rs` prewarms the cache in a background task at startup.
  `web/` keeps `style.css`, `htmx.min.js`, and a tiny `app.js` for the OAuth
  redirect token capture.
- `src/http.rs` — axum server: public routes (web assets, health, OAuth,
  admin), authenticated `/api/v1/*`, `/mcp` (feature-gated + `--enable-mcp`
  runtime switch), OAuth callback.
- `src/main.rs` — CLI (`stdio` [mcp feature only], `http`, `daemon`, `admin`);
  HTTP bootstrap.

## Cargo features

- `mcp` (default) — the MCP tool + `/mcp` endpoint + `stdio` subcommand.
  Without it, `GET /mcp` returns 404 `mcp_not_available`.
- `web` (default) — the embedded frontend. Without it, `/` serves a minimal
  placeholder page.
- With `mcp` built in, `--enable-mcp=false` (or `ENABLE_MCP=false`) disables
  `/mcp` at runtime (404 `mcp_disabled`). `daemon` respawns preserve the flag.

## Protocol names (`protocol=` / `?protocol=`)

- `aave_v3` — Aave V3 (all supported EVM chains)
- `spark` — Spark Savings vaults (spUSDC/spUSDT), via the Spark Savings API
- `blend` — Stellar Blend pools
- `all` (default) — everything

Spark Savings data is Cloudflare-cached by the upstream API (~5 min refresh);
we additionally cache 120s in SQLite (`use_cache=false` forces fresh).

## HTTP / OAuth invariants (don't break)

- `/api/v1/*` is public read-only (rate data is public), served from the
  shared 120s SQLite cache. `/mcp` requires `Authorization: Bearer` (OAuth
  token, GitHub token, or API key). Web pages (`/`, static assets) are public.
- 401 responses MUST carry `WWW-Authenticate: Bearer resource=..., authorization_servers=...`
  (RFC 9728) — clients use it to start OAuth. Browsers (Accept: text/html) get
  an HTML hint page linking to `/docs` instead of an empty body; `/docs` is a
  public MCP usage guide (Dioxus SSR with `web`, static page without).
- Access tokens TTL 24h with rotating refresh tokens; PKCE (S256) is enforced
  on `/oauth/token`; client secrets are stored sha256 + constant-time compared.
- `--base-url` MUST be the real public HTTPS domain (used for OAuth callback,
  `.well-known` discovery endpoints, and the MCP Host allowlist). Wrong base-url
  breaks GitHub login and client OAuth discovery.
- GitHub web login redirects to `/?oauth=success&...&token=<github token>`;
  the SPA stores that token and uses it as the Bearer for `/api/v1/*`
  (stored GitHub tokens are valid Bearer credentials, see auth middleware).

## Deployment / release

- `daemon` subcommand runs the HTTP server in the background: same args as
  `http`, plus required `--log <file>`; returns immediately.
  `pkill -f apy-mcp` to stop (on Windows: `taskkill //F //IM apy-mcp.exe`).
- Production HTTP requires `--base-url`, `--admin-token`, and GitHub OAuth
  client id/secret (+ optional `--allowed-github-users`).
- Release = push a `vX.Y.Z` tag; `.github/workflows/release.yml` builds 4
  platforms. **Linux must stay static musl** (`x86_64-unknown-linux-musl`):
  keep `reqwest` on `rustls-tls` + `default-features = false` — switching back
  to native-tls breaks the static build (glibc version errors on old servers).
  New dependencies must not pull in OpenSSL.
- The release build auto-injects the tag into the binary as `APY_MCP_RELEASE`,
  so `/health` reports the real deployed version. Cargo.toml's `version` stays
  at 0.1.0 and is NOT the deployed version — read it from `/health` instead.

## Conventions

- `.env` is gitignored and `.opencode/opencode.json` denies agents reading it —
  never read or commit `.env`.
- SQLite database lives in `data/` (gitignored).
- `web/` assets are embedded by `rust-embed` at compile time — after editing
  them, `cargo build` is required for changes to show up in the binary.
