# apy-mcp

MCP (Model Context Protocol) server for DeFi lending rate aggregation across multiple blockchains.

Currently supported:
- **Stellar** — Blend Capital lending pools

## Features

- Real-time lending/borrowing APY calculation from on-chain data
- Interest rate model projection (ir_mod decay, bRate/dRate accrual)
- Multi-asset pool support (USDC, XLM, EURC, etc.)
- Backstop rate integration
- **API Key management** with admin dashboard
- **GitHub OAuth login** (一键登录)
- **Rate limiting** per API key
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

# With GitHub OAuth (optional)
cargo run -- http \
  --addr 0.0.0.0:3000 \
  --admin-token your-admin-secret \
  --github-client-id YOUR_GITHUB_CLIENT_ID \
  --github-client-secret YOUR_GITHUB_CLIENT_SECRET
```

### Docker

```bash
# Build
docker build -t apy-mcp .

# Run with admin token
docker run -p 3000:3000 -e ADMIN_TOKEN=your-admin-secret apy-mcp

# Run with GitHub OAuth
docker run -p 3000:3000 \
  -e ADMIN_TOKEN=your-admin-secret \
  -e GITHUB_CLIENT_ID=your_client_id \
  -e GITHUB_CLIENT_SECRET=your_client_secret \
  apy-mcp

# Or use docker-compose
ADMIN_TOKEN=your-admin-secret \
GITHUB_CLIENT_ID=your_client_id \
GITHUB_CLIENT_SECRET=your_client_secret \
docker-compose up
```

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

## OAuth Setup (RFC 7591 Dynamic Client Registration)

### 方式一：动态注册（推荐）

支持 RFC 7591 的 OAuth Provider 可以自动注册客户端，无需手动配置 `client_id` 和 `client_secret`。

```bash
# 通过 Admin API 动态添加 Provider
curl -X POST http://localhost:3000/admin/oauth/providers \
  -H "Authorization: Bearer your-admin-token" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "keycloak",
    "issuer": "https://your-keycloak.com/realms/master"
  }'
```

服务器会自动：
1. 发现 OAuth 端点 (RFC 8414)
2. 注册为客户端 (RFC 7591)
3. 存储 `client_id` 和 `client_secret`

### 方式二：手动配置

```bash
curl -X POST http://localhost:3000/admin/oauth/providers \
  -H "Authorization: Bearer your-admin-token" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "github",
    "auth_url": "https://github.com/login/oauth/authorize",
    "token_url": "https://github.com/login/oauth/access_token",
    "user_info_url": "https://api.github.com/user",
    "client_id": "YOUR_CLIENT_ID",
    "client_secret": "YOUR_CLIENT_SECRET",
    "scopes": "read:user,user:email"
  }'
```

### 管理 OAuth Providers

```bash
# 列出所有 Providers
curl http://localhost:3000/admin/oauth/providers \
  -H "Authorization: Bearer your-admin-token"

# 删除 Provider
curl -X DELETE http://localhost:3000/admin/oauth/providers/1 \
  -H "Authorization: Bearer your-admin-token"
```

### 登录

```
http://localhost:3000/auth/github    → GitHub 登录
http://localhost:3000/auth/google    → Google 登录
http://localhost:3000/auth/keycloak  → Keycloak 登录
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
