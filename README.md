# apy-mcp

MCP (Model Context Protocol) server for DeFi lending rate aggregation across multiple blockchains.

Currently supported:
- **Stellar** — Blend Capital lending pools

## Features

- Real-time lending/borrowing APY calculation from on-chain data
- Interest rate model projection (ir_mod decay, bRate/dRate accrual)
- Multi-asset pool support (USDC, XLM, EURC, etc.)
- Backstop rate integration

## Quick Start

### Build

```bash
cargo build --release
```

### Run as MCP server (stdio)

```bash
cargo run
```

### Configure in Claude Desktop

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "apy-mcp": {
      "command": "/path/to/apy-mcp",
      "args": []
    }
  }
}
```

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
