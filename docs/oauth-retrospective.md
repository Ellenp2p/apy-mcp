# OAuth 接入实战：从 405 到稳定自动刷新

> apy-mcp 是 Rust 写的 MCP 服务，用 OAuth 2.0（GitHub 登录）保护 `/mcp` 端点。
> 本文记录在接入各类 MCP 客户端（opencode / Poke）过程中踩的坑和最终实现，
> 核心结论也适用于任何自建 OAuth 资源服务器。

## 一、整个过程踩了什么坑

### 1. 401 不带 `WWW-Authenticate` → 客户端永远无法重新认证

最早的实现里，`/mcp` 认证失败直接返回裸 `401`。MCP 客户端（opencode 用的官方 SDK）
在收到 401 后，需要从 **`WWW-Authenticate` 响应头**里发现 OAuth 服务器
（RFC 9728）。没有这个头，客户端就"卡死"在 401，无法发起登录。

```
// 必须这样返回，客户端才知道去哪里认证
WWW-Authenticate: Bearer resource="https://apy-mcp.wgb5445.com/mcp", authorization_servers="https://apy-mcp.wgb5445.com"
```

**教训**：资源服务器的 401 响应必须携带 RFC 9728 挑战头，这是客户端启动 OAuth 流程的"路标"。

### 2. 报 405 排查了整整一轮

客户端报 `POST /oauth/token → 405`。服务器端测试完全正常（POST 没问题），
一度怀疑是客户端 bug。最后靠**给服务器加请求日志**才定位：请求的 path 是
**`/token` 而不是 `/oauth/token`**。

原因：**Poke** 这类客户端在刷新 token 时，不读 metadata 里声明的
`token_endpoint`，而是**硬编码 `{base_url}/token`**（RFC 6749 的默认回退路径）。
授权码兑换那一步读了 metadata 所以成功，刷新这步走了硬编码路径就 405。

**教训**：
- 先加日志再排查，别靠猜。一个 `method/path/status` 的访问日志就解决了。
- 对"非标准"客户端，兼容性优先：把 token 端点同时挂在 `/oauth/token` 和 `/token` 两个路径。

### 3. Refresh token 生命周期设计错误

最初 access token 和 refresh token 共用同一个 `expires_at`（24h）。
结果 access 过期后 refresh token 也失效，客户端被**强制重新登录**——
refresh token 形同虚设，完全违背 OAuth 语义。

```
// 正确设计：refresh 必须比 access 活得久
access token:  24h（短，需要频繁验证）
refresh token: 30 天（长，客户端用它静默续期，每次刷新轮换新的）
```

**教训**：refresh token 的生命周期必须独立于 access token，否则"自动刷新"就是个摆设。

### 4. 部署升级把存量用户踢下线

加了 `refresh_expires_at` 列后，老行该字段为 `NULL`，`NULL > now` 为 false → 所有旧 refresh token 立刻失效。

**教训**：加列迁移后要**回填**存量数据（`UPDATE ... SET refresh_expires_at = expires_at + 30 days`），
否则每次发版都强制用户重新登录。

### 5. glibc 版本问题

GitHub Actions 的 `ubuntu-latest` 已是 glibc 2.39，老服务器（Ubuntu 22.04 = 2.35）
直接报 `GLIBC_2.39 not found`。改用 **musl 静态编译**后彻底解决。

**教训**：发布 Rust CLI 到未知服务器，直接上 `x86_64-unknown-linux-musl` 静态编译，
不用赌对方的 glibc 版本。代价是 TLS 要从 native-tls 换 rustls。

### 6. 版本永远是 0.1.0

`/health` 返回 `env!("CARGO_PKG_VERSION")`，而 Cargo.toml 的 version 从没随发版更新过，
所以永远显示 0.1.0，看不出部署的是哪个版本。

**教训**：发版构建时用环境变量把 git tag 注入二进制（`APY_MCP_RELEASE`），
`/health` 用 `option_env!` 读取，本地开发回退 Cargo 版本。

## 二、最终实现（现状）

### 认证流程

```
MCP 客户端 → POST /mcp（无 token）→ 401 + WWW-Authenticate
  → 客户端发现 OAuth 服务器（/.well-known/oauth-authorization-server）
  → 动态注册客户端（POST /oauth/register，RFC 7591）
  → 浏览器 GitHub 授权（GET /oauth/authorize → /auth/github → 回调）
  → 换 token（POST /oauth/token，authorization_code + PKCE S256）
  → 之后每个请求带 Bearer token
  → access 过期 → 客户端自动刷新（grant_type=refresh_token）→ 轮换新 pair
```

### 关键端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/.well-known/oauth-authorization-server` | GET | OAuth 元数据（RFC 8414） |
| `/.well-known/oauth-protected-resource` | GET | 资源元数据（RFC 9728） |
| `/oauth/authorize` | GET | GitHub 登录页 |
| `/oauth/register` | POST | 动态客户端注册（RFC 7591） |
| `/oauth/token` `/token` | POST | token 交换 / 刷新（RFC 6749） |

### 安全设计

- **PKCE S256** 强制（登录页 → 授权码 → token 兑换全链路传递）
- client secret sha256 存储 + 常数时间比较
- access token 24h / refresh token 30 天，刷新即轮换
- 所有 OAuth 错误返回标准 JSON（`{"error": "...", "error_description": "..."}`）
- 401 始终带 RFC 9728 挑战头

### 运维

- Token 存 SQLite（`data/apy-mcp.db`，WAL 模式），重启不掉线
- TTL 可配置：`APY_MCP_TOKEN_TTL_MINUTES`（access）、`APY_MCP_REFRESH_TTL_DAYS`（refresh）
- `/health` 返回真实发版版本（构建时注入 git tag）
- 访问日志 `method/path/status`，4xx/5xx 记 INFO

## 三、一句话总结

> **遵循规范 + 宽容实现 + 日志先行。**
> 规范决定"应该怎么做"（401 挑战头、refresh 生命周期、标准错误），
> 宽容实现兜住"客户端实际怎么做"（/token 别名、兼容路径），
> 日志让一切问题可定位。
