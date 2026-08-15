# apy-mcp

MCP (Model Context Protocol) server for DeFi lending rate aggregation across multiple blockchains.

Currently supported:
- **Stellar** — Blend Capital lending pools
- **EVM** — Aave V3 and Spark Savings (spUSDC/spUSDT vaults)

## Features

- Real-time lending/borrowing APY calculation from on-chain data
- Interest rate model projection (ir_mod decay, bRate/dRate accrual)
- Multi-asset pool support (USDC, XLM, EURC, etc.)
- Backstop rate integration
- **API Key management** with admin dashboard
- **GitHub OAuth login** (sole login method)
- **GitHub access control** — allowlist by GitHub username or UID (env var + Admin API)
- **Rate limiting** per API key / per OAuth user
- **Custom header support** (X-Poke-User-Id, etc.) with logging
- **SQLite database** for persistent storage

## Quick Start

### Build

```bash
cargo build --release
```

### Run as MCP server (stdio)

```bash
# Default pool
cargo run -- stdio

# Custom pool
cargo run -- stdio --pool-id CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD
```

### Configure in Claude Desktop

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "apy-mcp": {
      "command": "/path/to/apy-mcp",
      "args": ["stdio"]
    }
  }
}
```

## HTTP Server (Public Deployment)

### Run locally

```bash
# With admin token for management
cargo run -- http --addr 0.0.0.0:3000 --admin-token your-admin-secret

# With GitHub OAuth + access control
cargo run -- http \
  --addr 0.0.0.0:3000 \
  --base-url https://mcp.example.com \
  --admin-token your-admin-secret \
  --github-client-id YOUR_GITHUB_CLIENT_ID \
  --github-client-secret YOUR_GITHUB_CLIENT_SECRET \
  --allowed-github-users octocat,583231
```

> **Production**: `--admin-token` is REQUIRED (without it the admin API is open).
> `--base-url` must be your public HTTPS URL, otherwise OAuth redirects break.
> All EVM chains use **Alchemy** by default — set `ALCHEMY_KEY` (one key covers all 11 chains).

### Run in the background (daemon)

`daemon` starts the HTTP server detached from the terminal — logs go to the
file you specify and the command returns immediately:

```bash
# Equivalent to: nohup ./apy-mcp http ... > /var/log/apy-mcp.log 2>&1 &
./apy-mcp daemon --log /var/log/apy-mcp.log \
  --addr 0.0.0.0:3000 \
  --base-url https://mcp.example.com \
  --admin-token your-admin-secret \
  --github-client-id your_client_id \
  --github-client-secret your_client_secret \
  --allowed-github-users octocat,583231
```

All `http` options apply. Stop it with `pkill -f apy-mcp` or `kill <PID>`.

### Docker

```bash
# Build
docker build -t apy-mcp .

# Run with GitHub OAuth + access control
docker run -p 3000:3000 \
  -e BASE_URL=https://mcp.example.com \
  -e ADMIN_TOKEN=your-admin-secret \
  -e GITHUB_CLIENT_ID=your_client_id \
  -e GITHUB_CLIENT_SECRET=your_client_secret \
  -e ALLOWED_GITHUB_USERS=octocat,583231 \
  -e ALCHEMY_KEY=your_alchemy_key \
  apy-mcp

# Or use docker-compose
ADMIN_TOKEN=your-admin-secret \
BASE_URL=https://mcp.example.com \
GITHUB_CLIENT_ID=your_client_id \
GITHUB_CLIENT_SECRET=your_client_secret \
ALLOWED_GITHUB_USERS=octocat,583231 \
ALCHEMY_KEY=your_alchemy_key \
docker-compose up
```

## EVM RPC Providers

By default all 11 EVM chains (ethereum, polygon, arbitrum, optimism, avalanche, base,
gnosis, bnb, scroll, zksync, sonic) use **Alchemy** with a single key:

```bash
ALCHEMY_KEY=your_key cargo run -- http ...   # or --evm-provider-key your_key
```

- No `ALCHEMY_KEY` set → falls back to public RPCs (rate-limited, not for production).
- Override the provider: `--evm-provider public|infura|drpc|alchemy`.
- Override one chain only: `--evm-rpc-optimism https://...` (always wins over provider).
- Per-chain assignments via JSON config: `--evm-config config.json` (see `config.example.json`).

### Endpoints

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/health` | GET | No | Health check |
| `/mcp` | POST | API Key / OAuth | MCP Streamable HTTP endpoint |
| `/auth/github` | GET | No | Start GitHub OAuth flow |
| `/auth/github/callback` | GET | No | GitHub OAuth callback |
| `/auth/user` | GET | OAuth Token | Get current user info |
| `/admin/keys` | POST | Admin Token | Create new API key |
| `/admin/keys` | GET | Admin Token | List all API keys |
| `/admin/keys/{id}` | DELETE | Admin Token | Delete API key |
| `/admin/keys/{id}/deactivate` | DELETE | Admin Token | Deactivate API key |
| `/admin/keys/{id}/reactivate` | POST | Admin Token | Reactivate API key |
| `/admin/stats` | GET | Admin Token | Usage statistics |
| `/admin/github/allowlist` | GET | Admin Token | List GitHub allowlist entries |
| `/admin/github/allowlist` | POST | Admin Token | Add GitHub username/UID to allowlist |
| `/admin/github/allowlist/{value}` | DELETE | Admin Token | Remove entry from allowlist |

## GitHub OAuth Login

GitHub OAuth is the **only** login method. Configure it via CLI args or env vars:

```bash
# Environment variables
GITHUB_CLIENT_ID=your_client_id
GITHUB_CLIENT_SECRET=your_client_secret
```

The provider is created automatically at startup (`/auth/github` → GitHub → callback).
You can also manage providers from the admin panel, but only `github` is accepted by the
login flow.

Login flow:

```
http://localhost:3000/auth/github    → GitHub 登录
```

## GitHub Access Control (Allowlist)

Control **who** can log in / use the service by GitHub **username** or **UID**.
Enforced at two levels:
1. **Login** — OAuth callback denies users not in the allowlist.
2. **Requests** — `/mcp` access with a GitHub/OAuth token is denied (403) if the user
   is not in the allowlist.

> When the allowlist is **empty**, all GitHub users are allowed (open mode).
> For production you SHOULD populate it.

### 1. Static config (env var / CLI)

```bash
# Comma-separated usernames or numeric UIDs (auto-detected)
ALLOWED_GITHUB_USERS=octocat,583231,some-other-user
```

### 2. Dynamic management (Admin API)

```bash
# List
curl http://localhost:3000/admin/github/allowlist \
  -H "Authorization: Bearer your-admin-token"

# Add (username)
curl -X POST http://localhost:3000/admin/github/allowlist \
  -H "Authorization: Bearer your-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"value": "octocat", "kind": "username", "note": "core team"}'

# Add (UID)
curl -X POST http://localhost:3000/admin/github/allowlist \
  -H "Authorization: Bearer your-admin-token" \
  -H "Content-Type: application/json" \
  -d '{"value": "583231", "kind": "uid"}'

# Remove
curl -X DELETE http://localhost:3000/admin/github/allowlist/octocat \
  -H "Authorization: Bearer your-admin-token"
```

## API Key Management

### Create an API key

```bash
curl -X POST http://localhost:3000/admin/keys \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your-admin-secret" \
  -d '{
    "name": "my-app",
    "user_id": "user-123",
    "rate_limit": 100
  }'
```

Response:
```json
{
  "id": "uuid",
  "name": "my-app",
  "user_id": "user-123",
  "rate_limit": 100,
  "created_at": "2024-01-01T00:00:00Z",
  "api_key": "amcp_xxxxxxxxxxxxxxxx"
}
```

### CLI Management

```bash
# Create key
cargo run -- admin create-key --name "my-app" --user-id "user-123"

# List keys
cargo run -- admin list-keys

# Deactivate key
cargo run -- admin deactivate --key-id <key-id>

# Delete key
cargo run -- admin delete --key-id <key-id>
```

## Custom Headers (X-Poke-User-Id)

The server supports custom headers that are logged with each request:

```bash
curl -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "Authorization: Bearer your-api-key" \
  -H "X-Poke-User-Id: 00000000-0000-0000-0000-000000000000" \
  -H "X-Poke-Session-Id: my-session-123" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-03-26",
      "capabilities": {},
      "clientInfo": {"name": "test", "version": "1.0"}
    }
  }'
```

Headers starting with `X-Poke-*` or `X-Custom-*` are automatically captured and logged.

## MCP Tools

| Tool | Description |
|------|-------------|
| `get_blend_rates` | Query lending/borrowing rates for a Blend Capital pool |
| `get_all_rates` | Get rates for all monitored pools |
| `add_pool` | Add a pool to the monitoring list |

> Current production tool: `query_rates` with `protocol` filter
> (`aave_v3` | `spark` (Spark Savings) | `blend` | `all`).
> Spark Savings rates come from the official Spark Savings Data API
> (`api.spark.fi/v1/savings/{protocol}/{chain}/{token}`).

## Interest Rate Model

Blend uses a 3-segment interest rate curve:

```
borrow_apr = ir_mod × (r_base + r_one × util/target)    when util ≤ target
           = ir_mod × (r_base + r_one + r_two × ...)     when target < util ≤ 95%
           = ir_mod × (r_base + r_one + r_two + r_three × ...) when util > 95%

supply_apr = borrow_apr × utilization × (1 - backstop_rate)
borrow_apy = (1 + borrow_apr/365)^365 - 1
supply_apy = (1 + supply_apr/52)^52 - 1
```

## License

MIT
