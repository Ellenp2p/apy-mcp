# AGENTS.md

Rust MCP server aggregating DeFi lending/borrowing rates. HTTP mode exposes
`query_rates` over MCP Streamable HTTP with GitHub OAuth auth.

## Project goals

- Aggregate lending/borrowing APY across chains and protocols behind one
  unified `query_rates` MCP tool (filter by chain, asset, protocol, APY range,
  utilization) — a single query surface for AI agents.
- Coverage today: Aave V3 (11 EVM chains), Spark Savings (spUSDC/spUSDT),
  Blend (Stellar). New protocols should plug in as a `LendingProvider`
  implementation and be wired into `src/mcp/tools.rs`, not special-cased.
- Rate data must be fresh and cheap to serve: rely on official protocol
  data sources (contract reads / official APIs), cache aggressively
  (60s SQLite), and keep fetch paths resilient (timeouts, per-source
  isolation so one failing source doesn't fail the whole query).
- Production deployment is a public HTTPS MCP endpoint with GitHub OAuth —
  auth and OAuth flows are invariants, not optional features.

## Commands

```bash
cargo check
cargo test            # unit tests (fast, offline)
cargo clippy
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
- `src/mcp/tools.rs` — wires all providers into the `query_rates` MCP tool
  (filters, actions `query`/`add`/`list`, 60s SQLite cache).
- `src/mcp/types.rs` — shared `PoolRates`/`AssetRate`.
- `src/http.rs` — axum HTTP server, `/mcp` auth middleware, OAuth endpoints.
- `src/main.rs` — CLI (`stdio`, `http`, `daemon`, `admin`); HTTP bootstrap.

## Protocol names (`query_rates` `protocol=`)

- `aave_v3` — Aave V3 (all supported EVM chains)
- `spark` — Spark Savings vaults (spUSDC/spUSDT), via the Spark Savings API
- `blend` — Stellar Blend pools
- `all` (default) — everything

Spark Savings data is Cloudflare-cached by the upstream API (~5 min refresh);
we additionally cache 60s in SQLite (`use_cache=false` forces fresh).

## HTTP / OAuth invariants (don't break)

- `/mcp` requires `Authorization: Bearer` (OAuth token, GitHub token, or API key).
- 401 responses MUST carry `WWW-Authenticate: Bearer resource=..., authorization_servers=...`
  (RFC 9728) — clients use it to start OAuth.
- Access tokens TTL 24h with rotating refresh tokens; PKCE (S256) is enforced
  on `/oauth/token`; client secrets are stored sha256 + constant-time compared.
- `--base-url` MUST be the real public HTTPS domain (used for OAuth callback,
  `.well-known` discovery endpoints, and the MCP Host allowlist). Wrong base-url
  breaks GitHub login and client OAuth discovery.

## Deployment / release

- `daemon` subcommand runs the HTTP server in the background: same args as
  `http`, plus required `--log <file>`; returns immediately.
  `pkill -f apy-mcp` to stop.
- Production HTTP requires `--base-url`, `--admin-token`, and GitHub OAuth
  client id/secret (+ optional `--allowed-github-users`).
- Release = push a `vX.Y.Z` tag; `.github/workflows/release.yml` builds 4
  platforms. **Linux must stay static musl** (`x86_64-unknown-linux-musl`):
  keep `reqwest` on `rustls-tls` + `default-features = false` — switching back
  to native-tls breaks the static build (glibc version errors on old servers).
- The release build auto-injects the tag into the binary as `APY_MCP_RELEASE`,
  so `/health` reports the real deployed version. Cargo.toml's `version` stays
  at 0.1.0 and is NOT the deployed version — read it from `/health` instead.

## Conventions

- `.env` is gitignored and `.opencode/opencode.json` denies agents reading it —
  never read or commit `.env`.
- SQLite database lives in `data/` (gitignored).
