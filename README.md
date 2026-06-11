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

# Or use environment variable
ADMIN_TOKEN=your-admin-secret cargo run -- http
```

### Docker

```bash
# Build
docker build -t apy-mcp .

# Run with admin token
docker run -p 3000:3000 -e ADMIN_TOKEN=your-admin-secret apy-mcp

# Or use docker-compose
ADMIN_TOKEN=your-admin-secret docker-compose up
```

### Endpoints

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/health` | GET | No | Health check |
| `/mcp` | POST | API Key | MCP Streamable HTTP endpoint |
| `/admin/keys` | POST | Admin Token | Create new API key |
| `/admin/keys` | GET | Admin Token | List all API keys |
| `/admin/keys/{id}` | DELETE | Admin Token | Delete API key |
| `/admin/keys/{id}/deactivate` | DELETE | Admin Token | Deactivate API key |
| `/admin/keys/{id}/reactivate` | POST | Admin Token | Reactivate API key |
| `/admin/stats` | GET | Admin Token | Usage statistics |

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
