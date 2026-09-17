use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::{header::WWW_AUTHENTICATE, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
#[cfg(feature = "mcp")]
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use sqlx::SqlitePool;
use tower_http::cors::{Any, CorsLayer};

use crate::{db::Database, mcp::tools::ApyMcpTools};

/// Rate limiter entry
#[derive(Debug, Clone)]
pub(crate) struct RateLimiter {
    /// Request timestamps (sliding window)
    requests: Vec<std::time::Instant>,
    /// Max requests per minute
    limit: usize,
}

impl RateLimiter {
    fn new(limit: usize) -> Self {
        Self {
            requests: Vec::new(),
            limit,
        }
    }

    /// Check if a request is allowed and record it
    fn check_and_record(&mut self) -> bool {
        let now = std::time::Instant::now();
        let one_minute_ago = now - Duration::from_secs(60);

        // Remove old requests
        self.requests.retain(|t| *t > one_minute_ago);

        if self.requests.len() >= self.limit {
            false
        } else {
            self.requests.push(now);
            true
        }
    }
}

/// Shared application state for the HTTP server
#[derive(Clone)]
pub struct HttpState {
    /// MCP tools; `None` when built without the `mcp` feature or when
    /// `--enable-mcp=false` was passed.
    pub tools: Option<ApyMcpTools>,
    pub service: crate::service::rates::RateService,
    pub db: Database,
    pub admin_token: Option<String>,
    pub base_url: String,
    pub rate_limiters: Arc<tokio::sync::RwLock<std::collections::HashMap<String, RateLimiter>>>,
}

/// Extract custom headers from the request
fn extract_custom_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut custom = Vec::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        // Capture X-Poke-* headers and any other custom headers
        if name_str.starts_with("x-poke-") || name_str.starts_with("x-custom-") {
            if let Ok(v) = value.to_str() {
                custom.push((name_str.to_string(), v.to_string()));
            }
        }
    }
    custom
}

/// Apply the sliding-window rate limiter for a given key
async fn check_rate_limit(state: &HttpState, key: &str, limit: usize) -> Result<(), StatusCode> {
    let mut limiters = state.rate_limiters.write().await;
    let limiter = limiters
        .entry(key.to_string())
        .or_insert_with(|| RateLimiter::new(limit));
    if !limiter.check_and_record() {
        tracing::warn!(key = %key, "Rate limit exceeded");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(())
}

/// Build a 401 response with the RFC 9728 bearer challenge header so MCP
/// clients (opencode, VS Code, Claude Desktop) know how to start OAuth.
/// Browsers (Accept: text/html) get a friendly HTML page pointing at /docs
/// instead of a bare empty 401.
fn unauthorized_response(base_url: &str, accept_html: bool) -> Response {
    let challenge = format!(
        r#"Bearer resource="{base_url}/mcp", authorization_servers="{base_url}""#
    );
    if accept_html {
        let html = format!(
            r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>需要认证 — APY MCP</title><link rel="stylesheet" href="/style.css"></head>
<body><main class="container">
<section class="hero"><h1>这个端点需要认证</h1>
<p><code>{base_url}/mcp</code> 是 MCP 协议端点,供 AI 客户端通过 Streamable HTTP 连接,不支持浏览器直接访问。</p></section>
<section class="card doc">
<h2>接下来可以</h2>
<ol>
<li>查看 <a href="/docs">MCP 接入指南</a>,了解如何在 Claude / Cursor / VS Code 中配置客户端;</li>
<li>在 <a href="/">首页</a> 点 "GitHub 登录" 获取访问 token;</li>
<li>如果你只是要查利率,直接用公开的 <a href="/">网页查询</a> 或 <code>GET /api/v1/rates</code> REST API。</li>
</ol>
</section></main></body></html>"#
        );
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, challenge)],
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html,
        )
            .into_response();
    }
    (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, challenge)]).into_response()
}

/// True when the client likely is a browser asking for HTML.
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false)
}

/// API Key authentication middleware with rate limiting
async fn auth_middleware(
    State(state): State<HttpState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extract Authorization header
    let auth_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(header) => header,
        None => {
            tracing::debug!("Missing Authorization header - returning 401 challenge");
            return Ok(unauthorized_response(&state.base_url, wants_html(&headers)));
        }
    };

    // Extract Bearer token
    let raw_key = match auth_header.strip_prefix("Bearer ") {
        Some(key) => key,
        None => {
            tracing::debug!("Authorization header is not Bearer - returning 401 challenge");
            return Ok(unauthorized_response(&state.base_url, wants_html(&headers)));
        }
    };

    // Default rate limit for OAuth-authenticated requests (per user)
    const OAUTH_RATE_LIMIT: usize = 60;

    // 1. Check if it's an OAuth access token (from /oauth/token)
    let oauth_token = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT token, user_id, client_id, scope FROM oauth_access_tokens WHERE token = ? AND expires_at > ?",
    )
    .bind(raw_key)
    .bind(chrono::Utc::now().to_rfc3339())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to query oauth_access_tokens");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some((_token, user_id, _client_id, _scope)) = oauth_token {
        tracing::debug!(user_id = %user_id, "OAuth access token validated");

        // Resolve the GitHub user (login + UID) so both username and UID allowlists work
        let (gh_login, gh_id) = match sqlx::query_as::<_, (String, String)>(
            "SELECT login, id FROM oauth_users WHERE login = ? AND provider = 'github'",
        )
        .bind(&user_id)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to query oauth_users for GitHub user");
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
            Some((login, id)) => (login, id),
            None => (user_id.clone(), String::new()),
        };

        // Allowlist enforcement
        let allowed = state
            .db
            .is_github_allowed(&gh_login, &gh_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to check GitHub allowlist");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        if !allowed {
            tracing::warn!(login = %gh_login, github_id = %gh_id, "GitHub user not in allowlist - access denied");
            return Err(StatusCode::FORBIDDEN);
        }

        check_rate_limit(&state, &format!("oauth:{}", user_id), OAUTH_RATE_LIMIT).await?;

        let custom_headers = extract_custom_headers(&headers);
        request
            .extensions_mut()
            .insert(crate::mcp::tools::RequestMetadata { custom_headers });

        return Ok(next.run(request).await);
    }

    // 2. Check if it's a GitHub OAuth token (stored in oauth_users table)
    let github_user = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, login, name, email, avatar_url FROM oauth_users WHERE access_token = ? AND provider = 'github'",
    )
    .bind(raw_key)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to query oauth_users for GitHub token");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if let Some((github_id, login, _name, _email, _avatar)) = github_user {
        // GitHub OAuth token is valid - enforce allowlist (login OR UID)
        let allowed = state
            .db
            .is_github_allowed(&login, &github_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to check GitHub allowlist");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        if !allowed {
            tracing::warn!(login = %login, github_id = %github_id, "GitHub user not in allowlist - access denied");
            return Err(StatusCode::FORBIDDEN);
        }

        check_rate_limit(&state, &format!("github:{}", github_id), OAUTH_RATE_LIMIT).await?;

        // Extract custom headers for logging
        let custom_headers = extract_custom_headers(&headers);
        if !custom_headers.is_empty() {
            tracing::info!(
                github_user = %login,
                custom_headers = ?custom_headers,
                "MCP request with custom headers (OAuth)"
            );
        }

        request
            .extensions_mut()
            .insert(crate::mcp::tools::RequestMetadata { custom_headers });

        return Ok(next.run(request).await);
    }

    // 3. Not an OAuth token, try API key
    let api_key = match state.db.validate_key(raw_key).await {
        Ok(Some(key)) => key,
        Ok(None) => {
            tracing::debug!("No valid OAuth token or API key - returning 401 challenge");
            return Ok(unauthorized_response(&state.base_url, wants_html(&headers)));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to validate API key");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Check rate limit
    check_rate_limit(&state, &api_key.id, api_key.rate_limit as usize).await?;

    // Record usage in database
    let db = state.db.clone();
    let key_id = api_key.id.clone();
    tokio::spawn(async move {
        let _ = db.record_usage(&key_id).await;
    });

    // Extract custom headers for logging
    let custom_headers = extract_custom_headers(&headers);
    if !custom_headers.is_empty() {
        tracing::info!(
            key_id = %api_key.id,
            key_name = %api_key.name,
            user_id = ?api_key.user_id,
            custom_headers = ?custom_headers,
            "MCP request with custom headers"
        );
    }

    // Store API key info and custom headers in request extensions
    request.extensions_mut().insert(api_key);
    request
        .extensions_mut()
        .insert(crate::mcp::tools::RequestMetadata { custom_headers });

    Ok(next.run(request).await)
}

/// The effective version string:
/// - release builds: the git tag (vX.Y.Z) injected via `APY_MCP_RELEASE`
///   (set by `.github/workflows/release.yml`), so `/health` reports the real
///   deployed release.
/// - local/dev builds: the Cargo package version.
fn build_version() -> &'static str {
    option_env!("APY_MCP_RELEASE")
        .filter(|s| !s.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Health check endpoint
async fn health_handler() -> impl IntoResponse {
    let body = serde_json::json!({
        "status": "ok",
        "service": "apy-mcp",
        "version": build_version()
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

/// RFC 9728 - OAuth Protected Resource Metadata
/// VS Code needs this to discover how to authenticate with the MCP server
async fn protected_resource_metadata_handler(
    State(state): State<HttpState>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    Ok(axum::Json(serde_json::json!({
        "resource": format!("{}/mcp", state.base_url),
        "authorization_servers": [state.base_url],
        "scopes_supported": ["openid", "profile", "email"],
        "bearer_methods_supported": ["header"]
    })))
}

/// RFC 8414 - OAuth Server Metadata Discovery
/// VS Code and other MCP clients use this to discover OAuth endpoints
async fn oauth_metadata_handler(
    State(state): State<HttpState>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    Ok(axum::Json(serde_json::json!({
        "issuer": state.base_url,
        "authorization_endpoint": format!("{}/oauth/authorize", state.base_url),
        "token_endpoint": format!("{}/oauth/token", state.base_url),
        "registration_endpoint": format!("{}/oauth/register", state.base_url),
        "scopes_supported": ["openid", "profile", "email"],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
        "service_documentation": "https://github.com/Ellenp2p/apy-mcp",
        "code_challenge_methods_supported": ["S256"]
    })))
}

/// OAuth Authorization endpoint - returns login page
async fn oauth_authorize_handler(
    State(http_state): State<HttpState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Html<String>, StatusCode> {
    let client_id = params.get("client_id").cloned().unwrap_or_default();
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();
    let code_challenge = params.get("code_challenge").cloned().unwrap_or_default();
    let code_challenge_method = params
        .get("code_challenge_method")
        .cloned()
        .unwrap_or_default();
    let scope = params.get("scope").cloned().unwrap_or_default();
    tracing::info!(
        client_id = %client_id,
        has_code_challenge = !code_challenge.is_empty(),
        scope = %scope,
        "OAuth authorization page requested"
    );
    // Query available OAuth providers from database (only GitHub is supported)
    let providers = crate::oauth::OAuthProvider::list(&http_state.db.pool)
        .await
        .unwrap_or_default();
    let github_configured = providers
        .iter()
        .any(|p| p.name == "github" && p.is_active && p.client_id.is_some());

    // Build the GitHub login button
    let mut social_buttons = String::new();
    if github_configured {
        social_buttons.push_str(&format!(
            r#"<a href="/auth/github?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method={}&scope={}" style="display: flex; align-items: center; justify-content: center; gap: 8px; width: 100%; padding: 12px; border: none; border-radius: 8px; background: #24292e; color: white; font-size: 16px; cursor: pointer; text-decoration: none; margin-bottom: 8px; box-sizing: border-box;">🐙 GitHub</a>"#,
            urlencoding::encode(&client_id),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(&state),
            urlencoding::encode(&code_challenge),
            urlencoding::encode(&code_challenge_method),
            urlencoding::encode(&scope),
        ));
    }

    // If no providers configured, show a message
    let no_providers_msg = if social_buttons.is_empty() {
        r#"<div style="background: rgba(255,193,7,0.15); color: #ffc107; padding: 12px; border-radius: 8px; font-size: 14px; text-align: center;">No login providers configured. Please set up GitHub OAuth in the admin panel.</div>"#
    } else {
        ""
    };

    // Generate login page
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>APY MCP - Login</title>
    <style>
        body {{ font-family: -apple-system, sans-serif; background: #0f1117; color: #e4e6f0; display: flex; justify-content: center; align-items: center; min-height: 100vh; margin: 0; }}
        .card {{ background: #1a1d29; border: 1px solid #2d3148; border-radius: 12px; padding: 32px; width: 400px; }}
        h1 {{ margin: 0 0 24px 0; font-size: 24px; }}
        h1 span {{ color: #6c5ce7; }}
        .info {{ background: #0f1117; padding: 12px; border-radius: 8px; margin-bottom: 16px; font-size: 14px; color: #8b8fa3; }}
    </style>
</head>
<body>
    <div class="card">
        <h1>⚡ <span>APY</span> MCP Login</h1>
        <div class="info">
            <strong>Client ID:</strong> {client_id}<br>
            <strong>Scope:</strong> {scope}
        </div>
        {no_providers_msg}
        {social_buttons}
    </div>
</body>
</html>"#,
        client_id = client_id,
        scope = scope,
        social_buttons = social_buttons,
        no_providers_msg = no_providers_msg,
    );

    Ok(axum::response::Html(html))
}

/// RFC 7591 - Dynamic Client Registration
async fn oauth_register_handler(
    State(state): State<HttpState>,
    axum::extract::Json(req): axum::extract::Json<serde_json::Value>,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), StatusCode> {
    use sha2::{Digest, Sha256};

    // Generate client credentials
    let client_id = format!("mcp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let client_secret_raw = uuid::Uuid::new_v4().to_string();
    let mut hasher = Sha256::new();
    hasher.update(client_secret_raw.as_bytes());
    let client_secret_hash = hex::encode(hasher.finalize());

    // Extract redirect URIs from request
    let redirect_uris: Vec<String> = req["redirect_uris"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let client_name = req["client_name"].as_str().unwrap_or("MCP Client");

    // grant_types: honor the client's request but only keep grants we actually
    // support, and always include authorization_code + refresh_token (the AS
    // advertises both in metadata)
    const SUPPORTED_GRANTS: [&str; 2] = ["authorization_code", "refresh_token"];
    let mut grant_types: Vec<String> = req["grant_types"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|g| SUPPORTED_GRANTS.contains(g))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    for gt in SUPPORTED_GRANTS {
        if !grant_types.iter().any(|g| g == gt) {
            grant_types.push(gt.to_string());
        }
    }

    // Store in database
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO oauth_clients (client_id, client_secret, client_name, redirect_uris, grant_types, created_at)
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&client_id)
    .bind(&client_secret_hash)
    .bind(client_name)
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(serde_json::to_string(&grant_types).unwrap_or_default())
    .bind(&now)
    .execute(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(
        client_id = %client_id,
        client_name = %client_name,
        redirect_uris = ?redirect_uris,
        grant_types = ?grant_types,
        "OAuth client registered"
    );

    // Return credentials (client_secret shown once)
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret_raw,
            "client_name": client_name,
            "redirect_uris": redirect_uris,
            "grant_types": grant_types,
            "response_types": ["code"],
            "token_endpoint_auth_method": "client_secret_post"
        })),
    ))
}

/// Constant-time string comparison
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TTL tests mutate process-global env vars, so they must not run
    /// concurrently with each other.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap()
    }

    #[test]
    fn test_access_token_ttl_default() {
        let _g = env_guard();
        // No env var set -> default 24h
        std::env::remove_var("APY_MCP_TOKEN_TTL_MINUTES");
        assert_eq!(access_token_ttl().num_minutes(), 24 * 60);
        assert_eq!(access_token_ttl().num_seconds(), 24 * 3600);
    }

    #[test]
    fn test_access_token_ttl_custom() {
        let _g = env_guard();
        std::env::set_var("APY_MCP_TOKEN_TTL_MINUTES", "2");
        assert_eq!(access_token_ttl().num_minutes(), 2);
        assert_eq!(access_token_ttl().num_seconds(), 120);
        std::env::remove_var("APY_MCP_TOKEN_TTL_MINUTES");
    }

    #[test]
    fn test_access_token_ttl_min_floor() {
        let _g = env_guard();
        // Values below 1 are floored to 1 minute
        std::env::set_var("APY_MCP_TOKEN_TTL_MINUTES", "0");
        assert_eq!(access_token_ttl().num_minutes(), 1);
        std::env::remove_var("APY_MCP_TOKEN_TTL_MINUTES");
    }

    #[test]
    fn test_refresh_token_ttl_default() {
        let _g = env_guard();
        std::env::remove_var("APY_MCP_REFRESH_TTL_DAYS");
        assert_eq!(refresh_token_ttl().num_days(), 30);
    }

    #[test]
    fn test_refresh_token_ttl_custom() {
        let _g = env_guard();
        std::env::set_var("APY_MCP_REFRESH_TTL_DAYS", "7");
        assert_eq!(refresh_token_ttl().num_days(), 7);
        std::env::remove_var("APY_MCP_REFRESH_TTL_DAYS");
    }

    #[test]
    fn test_refresh_outlives_access() {
        let _g = env_guard();
        // Refresh TTL must always be >= access TTL so the client can rotate
        std::env::set_var("APY_MCP_TOKEN_TTL_MINUTES", "2");
        std::env::set_var("APY_MCP_REFRESH_TTL_DAYS", "1");
        assert!(refresh_token_ttl() > access_token_ttl());
        std::env::remove_var("APY_MCP_TOKEN_TTL_MINUTES");
        std::env::remove_var("APY_MCP_REFRESH_TTL_DAYS");
    }
}

/// Hash a client secret for storage
fn hash_client_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// OAuth 2.0 error response (RFC 6749 §5.2): JSON body + proper status code
type OAuthError = (StatusCode, axum::Json<serde_json::Value>);

/// Build a standard OAuth error response and log it
fn oauth_error(status: StatusCode, code: &str, description: &str) -> OAuthError {
    tracing::warn!(status = %status, error = code, description = description, "OAuth error");
    (
        status,
        axum::Json(serde_json::json!({
            "error": code,
            "error_description": description,
        })),
    )
}

/// RFC 7636 - PKCE S256 code challenge from a code_verifier
fn pkce_s256_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Extract OAuth client credentials from form body or Authorization header
fn extract_client_credentials(
    headers: &HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> Result<(String, String), OAuthError> {
    if let (Some(id), Some(secret)) = (params.get("client_id"), params.get("client_secret")) {
        Ok((id.clone(), secret.clone()))
    } else if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(basic) = auth_header.strip_prefix("Basic ") {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(basic) {
                Ok(decoded) => {
                    let decoded_str = String::from_utf8_lossy(&decoded);
                    let parts: Vec<&str> = decoded_str.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        Ok((parts[0].to_string(), parts[1].to_string()))
                    } else {
                        Err(oauth_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request",
                            "Malformed Basic authorization header",
                        ))
                    }
                }
                Err(_) => Err(oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Malformed Basic authorization header",
                )),
            }
        } else {
            Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Client credentials are missing",
            ))
        }
    } else if let Some(id) = params.get("client_id") {
        // Public client (no secret) - allowed for compatibility
        Ok((id.clone(), String::new()))
    } else {
        Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Client credentials are missing",
        ))
    }
}

/// Load or auto-register an OAuth client and verify its secret
async fn resolve_client(
    pool: &SqlitePool,
    client_id: &str,
    client_secret: &str,
) -> Result<(), OAuthError> {
    let existing = sqlx::query_as::<_, (String, String)>(
        "SELECT client_id, client_secret FROM oauth_clients WHERE client_id = ?",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to look up OAuth client");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Database error while resolving client",
        )
    })?;

    let secret_hash = hash_client_secret(client_secret);

    match existing {
        Some((_, stored_hash)) => {
            if !constant_time_eq(&secret_hash, &stored_hash) {
                tracing::warn!(client_id = client_id, "OAuth client secret mismatch");
                return Err(oauth_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_client",
                    "Client authentication failed (invalid client secret)",
                ));
            }
        }
        None => {
            // Auto-register for client compatibility (VS Code etc.)
            let now = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO oauth_clients (client_id, client_secret, client_name, redirect_uris, grant_types, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(client_id)
            .bind(&secret_hash)
            .bind("MCP Client")
            .bind("[]")
            .bind(r#"["authorization_code","refresh_token"]"#)
            .bind(&now)
            .execute(pool)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to auto-register OAuth client");
                oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Database error while registering client",
                )
            })?;
            tracing::info!(client_id = client_id, "Auto-registered OAuth client");
        }
    }
    Ok(())
}

/// Access token TTL, configurable via `APY_MCP_TOKEN_TTL_MINUTES` (default 24h).
/// Useful for testing the refresh flow without waiting a day: set it to a small
/// value (e.g. 1), watch the client auto-refresh, then restore 1440.
fn access_token_ttl() -> chrono::Duration {
    let minutes = std::env::var("APY_MCP_TOKEN_TTL_MINUTES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(24 * 60)
        .max(1);
    chrono::Duration::minutes(minutes)
}

/// Refresh token lifetime, configurable via `APY_MCP_REFRESH_TTL_DAYS`
/// (default 30 days). Refresh tokens live much longer than access tokens so
/// clients can silently rotate without forcing a re-login.
fn refresh_token_ttl() -> chrono::Duration {
    let days = std::env::var("APY_MCP_REFRESH_TTL_DAYS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .max(1);
    chrono::Duration::days(days)
}

/// Issue a new access token + refresh token pair.
/// Access token TTL is short (configurable); the refresh token outlives it
/// (default 30 days) so the client can rotate without re-authenticating.
async fn issue_tokens(
    pool: &SqlitePool,
    user_id: &str,
    client_id: &str,
    scope: Option<&str>,
) -> Result<(String, String, i64), OAuthError> {
    let access_token = format!("mcp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let refresh_token = format!("mrp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let access_ttl = access_token_ttl();
    // Refresh must never expire before the access token, otherwise rotation
    // silently breaks. Clamp it to be at least the access TTL.
    let refresh_ttl = refresh_token_ttl().max(access_ttl);
    let expires_in = access_ttl.num_seconds();
    let now = chrono::Utc::now().to_rfc3339();
    let expires_at = (chrono::Utc::now() + access_ttl).to_rfc3339();
    let refresh_expires_at = (chrono::Utc::now() + refresh_ttl).to_rfc3339();

    sqlx::query(
        "INSERT INTO oauth_access_tokens (token, user_id, client_id, scope, expires_at, created_at, refresh_token, refresh_expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&access_token)
    .bind(user_id)
    .bind(client_id)
    .bind(scope.unwrap_or(""))
    .bind(&expires_at)
    .bind(&now)
    .bind(&refresh_token)
    .bind(&refresh_expires_at)
    .execute(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "Failed to store access token");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Failed to issue tokens",
        )
    })?;

    tracing::info!(user_id = user_id, client_id = client_id, ttl_seconds = expires_in, "Access token issued");
    Ok((access_token, refresh_token, expires_in))
}

/// OAuth Token endpoint (RFC 6749 §3.2)
/// Supports both client_secret_post (form body) and client_secret_basic (Authorization header)
async fn oauth_token_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    axum::extract::Form(params): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<serde_json::Value>, OAuthError> {
    let grant_type = match params.get("grant_type") {
        Some(g) => g.clone(),
        None => {
            return Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Missing 'grant_type' parameter",
            ))
        }
    };
    let client_id_hint = params
        .get("client_id")
        .cloned()
        .unwrap_or_else(|| "<none>".to_string());
    tracing::info!(
        grant_type = %grant_type,
        client_id = %client_id_hint,
        "OAuth token request"
    );

    match grant_type.as_str() {
        "authorization_code" => {
            let code = match params.get("code") {
                Some(c) => c,
                None => {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "Missing 'code' parameter",
                    ))
                }
            };
            let (client_id, client_secret) = extract_client_credentials(&headers, &params)?;
            resolve_client(&state.db.pool, &client_id, &client_secret).await?;

            // Validate authorization code
            let auth_code = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
                "SELECT code, client_id, user_id, code_challenge, code_challenge_method FROM oauth_authorization_codes WHERE code = ? AND client_id = ? AND expires_at > ?",
            )
            .bind(code)
            .bind(&client_id)
            .bind(chrono::Utc::now().to_rfc3339())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to query authorization code");
                oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Database error while validating authorization code",
                )
            })?;

            let (_, _, user_id, code_challenge, code_challenge_method) =
                match auth_code {
                    Some(row) => row,
                    None => {
                        tracing::warn!(code = %code, "Invalid or expired authorization code");
                        return Err(oauth_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_grant",
                            "Invalid or expired authorization code",
                        ));
                    }
                };

            // Delete used code (single-use)
            sqlx::query("DELETE FROM oauth_authorization_codes WHERE code = ?")
                .bind(code)
                .execute(&state.db.pool)
                .await
                .ok();

            // PKCE verification (RFC 7636)
            if let Some(challenge) = code_challenge {
                if !challenge.is_empty() {
                    let method = code_challenge_method
                        .as_deref()
                        .filter(|m| !m.is_empty())
                        .unwrap_or("S256");
                    let verifier = match params.get("code_verifier") {
                        Some(v) => v,
                        None => {
                            return Err(oauth_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request",
                                "Missing 'code_verifier' parameter (PKCE required)",
                            ))
                        }
                    };
                    if method != "S256" {
                        return Err(oauth_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request",
                            "Unsupported PKCE method (only S256 is supported)",
                        ));
                    }
                    if !constant_time_eq(&pkce_s256_challenge(verifier), &challenge) {
                        tracing::warn!("PKCE code_verifier mismatch");
                        return Err(oauth_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_grant",
                            "PKCE verification failed (code_verifier mismatch)",
                        ));
                    }
                }
            }

            let (access_token, refresh_token, expires_in) =
                issue_tokens(&state.db.pool, &user_id, &client_id, None).await?;

            tracing::info!(user_id = %user_id, client_id = %client_id, "Authorization code exchanged for tokens");

            Ok(axum::Json(serde_json::json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "token_type": "Bearer",
                "expires_in": expires_in
            })))
        }
        "refresh_token" => {
            let refresh_token = match params.get("refresh_token") {
                Some(t) => t,
                None => {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "Missing 'refresh_token' parameter",
                    ))
                }
            };
            let (client_id, client_secret) = extract_client_credentials(&headers, &params)?;
            resolve_client(&state.db.pool, &client_id, &client_secret).await?;

            // Look up the row that owns this refresh token (refresh token has
            // its own, longer lifetime than the access token)
            let row = sqlx::query_as::<_, (String, String, String, Option<String>)>(
                "SELECT token, user_id, client_id, scope FROM oauth_access_tokens WHERE refresh_token = ? AND client_id = ? AND refresh_expires_at > ?",
            )
            .bind(refresh_token)
            .bind(&client_id)
            .bind(chrono::Utc::now().to_rfc3339())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to query refresh token");
                oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "Database error while validating refresh token",
                )
            })?;

            let (old_token, user_id, _, scope) = match row {
                Some(row) => row,
                None => {
                    tracing::warn!(client_id = %client_id, "Invalid or expired refresh token");
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "Invalid or expired refresh token",
                    ));
                }
            };

            // Rotate: revoke the old token + refresh token, issue a fresh pair
            sqlx::query("DELETE FROM oauth_access_tokens WHERE token = ?")
                .bind(&old_token)
                .execute(&state.db.pool)
                .await
                .ok();

            let (access_token, refresh_token, expires_in) =
                issue_tokens(&state.db.pool, &user_id, &client_id, scope.as_deref()).await?;

            tracing::info!(user_id = %user_id, client_id = %client_id, "Access token refreshed");

            Ok(axum::Json(serde_json::json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "token_type": "Bearer",
                "expires_in": expires_in
            })))
        }
        _ => Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("Unsupported grant_type '{}'", grant_type),
        )),
    }
}

/// JSON 405 for method mismatches (e.g. GET /oauth/token) so clients get a
/// machine-readable error instead of an empty body
async fn method_not_allowed_handler() -> (StatusCode, axum::Json<serde_json::Value>) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        axum::Json(serde_json::json!({
            "error": "method_not_allowed",
            "error_description": "The HTTP method is not allowed for this endpoint"
        })),
    )
}

/// GET /mcp when the binary was built without the "mcp" feature.
#[cfg(not(feature = "mcp"))]
async fn mcp_unavailable_handler() -> (StatusCode, axum::Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "mcp_not_available",
            "message": "This server was built without the 'mcp' feature; the MCP endpoint is not available.",
            "docs": "/docs"
        })),
    )
}

/// GET /mcp when the server was started with --enable-mcp=false.
#[cfg(feature = "mcp")]
async fn mcp_disabled_handler() -> (StatusCode, axum::Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "mcp_disabled",
            "message": "The MCP endpoint is disabled on this server (started with --enable-mcp=false).",
            "docs": "/docs"
        })),
    )
}

/// Minimal landing page served as fallback for non-web builds.
/// With the "web" feature, the embedded frontend (`crate::web`) is used instead.
#[cfg(not(feature = "web"))]
async fn index_handler() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>APY MCP</title>
    <style>
        body { font-family: -apple-system, sans-serif; background: #0f1117; color: #e4e6f0; display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
        .card { text-align: center; }
        h1 { font-size: 28px; margin-bottom: 8px; }
        h1 span { color: #6c5ce7; }
        p { color: #8b8fa3; font-size: 14px; }
        .badge { display: inline-block; margin-top: 16px; padding: 6px 16px; border-radius: 20px; background: rgba(0, 184, 148, 0.15); color: #00b894; font-size: 13px; }
        a { color: #6c5ce7; text-decoration: none; }
    </style>
</head>
<body>
    <div class="card">
        <h1>&#9889; <span>APY</span> MCP</h1>
        <p>DeFi lending rate aggregation service</p>
        <div class="badge">&#9989; Service running</div>
        <p style="margin-top: 24px;">Health check: <a href="/health">/health</a></p>
    </div>
</body>
</html>"#;
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

/// Static MCP usage guide for non-web builds (the web feature has the full
/// Dioxus SSR docs page at the same path).
#[cfg(not(feature = "web"))]
async fn docs_fallback_handler() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>APY MCP - Docs</title>
    <style>
        body { font-family: -apple-system, "Segoe UI", "PingFang SC", sans-serif; background: #0f1117; color: #e4e6f0; max-width: 720px; margin: 0 auto; padding: 40px 20px; line-height: 1.7; }
        h1 { font-size: 26px; } h1 span { color: #6c5ce7; }
        h2 { font-size: 18px; margin-top: 32px; color: #c9cbe0; }
        p, li { color: #8b8fa3; font-size: 14.5px; }
        code { background: #171a23; padding: 2px 8px; border-radius: 6px; font-size: 13px; }
        pre { background: #171a23; padding: 14px 16px; border-radius: 10px; overflow-x: auto; }
        pre code { padding: 0; background: none; }
        a { color: #6c5ce7; }
    </style>
</head>
<body>
    <h1>&#9889; <span>APY</span> MCP &mdash; 接入指南</h1>
    <p>MCP 端点:<code>&lt;base_url&gt;/mcp</code>(MCP Streamable HTTP,需要认证)</p>
    <h2>客户端配置(Claude Desktop / Cursor)</h2>
    <pre><code>{
  "mcpServers": {
    "apy-mcp": {
      "url": "https://your-host/mcp",
      "headers": { "Authorization": "Bearer &lt;API_KEY 或 OAuth token&gt;" }
    }
  }
}</code></pre>
    <h2>认证</h2>
    <p>人类用户:客户端首次连接会自动弹出 GitHub OAuth 登录。<br>
    服务器/脚本:用 admin token 调用 <code>POST /admin/keys</code> 创建 API key。</p>
    <h2>公开 REST API(无需认证)</h2>
    <p><code>GET /api/v1/rates?asset=USDC</code>,以及 <code>/api/v1/chains</code>、<code>/api/v1/protocols</code>、<code>/api/v1/pools</code>。</p>
    <p><a href="/">&larr; 返回首页</a></p>
</body>
</html>"#;
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}
/// Errors (4xx/5xx) at info, successful requests at debug to avoid noise
/// from unauthenticated client probes.
/// Log every HTTP request (method, path, status) for debugging.
/// Errors (4xx/5xx) at info, successful requests at debug to avoid noise.
async fn access_log_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    // Errors (4xx/5xx) and API/MCP calls at info; other requests at debug
    // to avoid noise from static assets.
    if status >= 400 || path.starts_with("/api/") || path.starts_with("/mcp") {
        tracing::info!(
            method = %method,
            path = %path,
            status = status,
            "HTTP request"
        );
    } else {
        tracing::debug!(
            method = %method,
            path = %path,
            status = status,
            "HTTP request"
        );
    }
    response
}

/// Start the HTTP server
pub async fn start_http_server(
    addr: SocketAddr,
    tools: Option<ApyMcpTools>,
    service: crate::service::rates::RateService,
    db: Database,
    admin_token: Option<String>,
    base_url: String,
) -> anyhow::Result<()> {
    let state = HttpState {
        tools: tools.clone(),
        service,
        db: db.clone(),
        admin_token: admin_token.clone(),
        base_url: base_url.clone(),
        rate_limiters: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };

    // Warm the rate cache in the background so the first page/API hit is fast.
    {
        let svc = state.service.clone();
        tokio::spawn(async move {
            let filters = crate::service::types::QueryRatesParams {
                action: "query".to_string(),
                chain: None,
                asset: None,
                protocol: None,
                pool_id: None,
                min_supply_apy: None,
                max_supply_apy: None,
                min_borrow_apy: None,
                max_borrow_apy: None,
                min_utilization: None,
                max_utilization: None,
                use_cache: true,
                cache_only: false,
            };
            match svc.query(&filters).await {
                Ok(resp) => tracing::info!(pools = resp.pools.len(), "prewarmed rate cache"),
                Err(e) => tracing::warn!(error = %e, "rate cache prewarm failed"),
            }
        });
    }

    // Build public routes (no auth needed)
    let public_routes = Router::new()
        .route("/health", get(health_handler))
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth_metadata_handler),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata_handler),
        )
        .route("/oauth/authorize", get(oauth_authorize_handler))
        .route("/oauth/register", post(oauth_register_handler))
        .route("/oauth/token", post(oauth_token_handler))
        // Alias: some clients (e.g. Poke) hardcode the token endpoint at
        // `{base_url}/token` (RFC 6749 default) instead of honoring the
        // metadata's `token_endpoint`. Serve both paths.
        .route("/token", post(oauth_token_handler))
        .route(
            "/admin/keys",
            post(crate::admin::create_key).get(crate::admin::list_keys),
        )
        .route("/admin/keys/{key_id}", delete(crate::admin::delete_key))
        .route(
            "/admin/keys/{key_id}/deactivate",
            delete(crate::admin::deactivate_key),
        )
        .route(
            "/admin/keys/{key_id}/reactivate",
            post(crate::admin::reactivate_key),
        )
        .route("/admin/stats", get(crate::admin::get_stats))
        .route(
            "/admin/oauth/providers",
            get(crate::admin::list_oauth_providers),
        )
        .route(
            "/admin/oauth/providers",
            post(crate::admin::create_oauth_provider),
        )
        .route(
            "/admin/oauth/providers/{id}",
            delete(crate::admin::delete_oauth_provider),
        )
        .route(
            "/admin/oauth/providers/{id}/deactivate",
            delete(crate::admin::deactivate_oauth_provider),
        )
        .route(
            "/admin/oauth/providers/{id}/reactivate",
            post(crate::admin::reactivate_oauth_provider),
        )
        .route(
            "/admin/rpc/providers",
            get(crate::admin::list_rpc_providers),
        )
        .route("/admin/rpc/status", get(crate::admin::get_rpc_status))
        .route(
            "/admin/github/allowlist",
            get(crate::admin::list_github_allowlist).post(crate::admin::add_github_allowlist),
        )
        .route(
            "/admin/github/allowlist/{value}",
            delete(crate::admin::remove_github_allowlist),
        );

    // Public MCP usage guide. With the "web" feature it's a Dioxus SSR page;
    // otherwise a small static page.
    #[cfg(feature = "web")]
    let public_routes = public_routes.route("/docs", get(crate::web::docs_handler));
    #[cfg(not(feature = "web"))]
    let public_routes = public_routes.route("/docs", get(docs_fallback_handler));

    // Build public, read-only API routes. Rate data is public (like the
    // protocols' own dashboards); responses are served from the shared cache.
    let api_routes = Router::new()
        .route("/api/v1/rates", get(crate::api::get_rates))
        .route("/api/v1/chains", get(crate::api::get_chains))
        .route("/api/v1/protocols", get(crate::api::get_protocols))
        .route("/api/v1/pools", get(crate::api::get_pools));

    // Build MCP routes. Three cases:
    // - built without the "mcp" feature  -> 404 "not compiled in"
    // - --enable-mcp=false               -> 404 "disabled on this server"
    // - otherwise                        -> the streamable MCP service (auth required)
    #[cfg(feature = "mcp")]
    let mcp_routes = match tools.clone() {
        Some(tools_for_service) => {
            let mut allowed_hosts: Vec<String> = vec![
                "localhost".into(),
                "127.0.0.1".into(),
                "::1".into(),
                "0.0.0.0".into(),
            ];
            // Extract hostname from base_url and add to allowed hosts
            if let Ok(url) = url::Url::parse(&base_url) {
                if let Some(host) = url.host_str() {
                    if !allowed_hosts.contains(&host.to_string()) {
                        allowed_hosts.push(host.to_string());
                    }
                }
            }
            tracing::info!("MCP allowed hosts: {:?}", allowed_hosts);

            let mcp_service: StreamableHttpService<
                crate::mcp::tools::ApyMcpTools,
                LocalSessionManager,
            > = StreamableHttpService::new(
                move || Ok(tools_for_service.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
            );

            Router::new()
                .nest_service("/mcp", mcp_service)
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ))
        }
        None => Router::new().route("/mcp", get(mcp_disabled_handler)),
    };
    #[cfg(not(feature = "mcp"))]
    let mcp_routes = Router::new().route("/mcp", get(mcp_unavailable_handler));

    // Build OAuth routes if configured
    let oauth_routes = crate::oauth::oauth_router_without_state();
    let oauth_callback_routes =
        crate::oauth::oauth_callback_router(db.pool.clone(), base_url.clone());

    // Combine all routes with CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers(Any);

    // Serve minimal landing page as fallback
    // Serve the embedded web frontend as fallback (non-web builds: minimal page)
    #[cfg(feature = "web")]
    let app = Router::new()
        .merge(public_routes)
        .merge(api_routes)
        .merge(mcp_routes)
        .merge(oauth_routes)
        .merge(oauth_callback_routes)
        .fallback(crate::web::fallback_handler)
        .method_not_allowed_fallback(method_not_allowed_handler)
        .layer(axum::middleware::from_fn(access_log_middleware))
        .layer(cors)
        .with_state(state);
    #[cfg(not(feature = "web"))]
    let app = Router::new()
        .merge(public_routes)
        .merge(api_routes)
        .merge(mcp_routes)
        .merge(oauth_routes)
        .merge(oauth_callback_routes)
        .fallback(get(index_handler))
        .method_not_allowed_fallback(method_not_allowed_handler)
        .layer(axum::middleware::from_fn(access_log_middleware))
        .layer(cors)
        .with_state(state);

    tracing::info!("Starting HTTP server on {}", addr);
    if admin_token.is_some() {
        tracing::info!("Admin API enabled with token authentication");
    } else {
        tracing::warn!("No admin token configured - admin API is open");
    }

    // Start the server
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
