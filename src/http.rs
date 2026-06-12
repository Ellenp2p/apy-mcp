use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tower_http::cors::{CorsLayer, Any};

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
    pub tools: ApyMcpTools,
    pub db: Database,
    pub admin_token: Option<String>,
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

/// API Key authentication middleware with rate limiting
async fn auth_middleware(
    State(state): State<HttpState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extract Authorization header
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Extract Bearer token
    let raw_key = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

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

        let custom_headers = extract_custom_headers(&headers);
        request
            .extensions_mut()
            .insert(crate::mcp::tools::RequestMetadata {
                custom_headers,
            });

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

    if let Some((_id, login, _name, _email, _avatar)) = github_user {
        // GitHub OAuth token is valid, allow request
        tracing::debug!(login = %login, "GitHub OAuth token validated");

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
            .insert(crate::mcp::tools::RequestMetadata {
                custom_headers,
            });

        return Ok(next.run(request).await);
    }

    // 3. Not an OAuth token, try API key
    let api_key = state
        .db
        .validate_key(raw_key)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to validate API key");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Check rate limit
    let mut limiters = state.rate_limiters.write().await;
    let limiter = limiters
        .entry(api_key.id.clone())
        .or_insert_with(|| RateLimiter::new(api_key.rate_limit as usize));

    if !limiter.check_and_record() {
        tracing::warn!(
            key_id = %api_key.id,
            key_name = %api_key.name,
            "Rate limit exceeded"
        );
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

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
        .insert(crate::mcp::tools::RequestMetadata {
            custom_headers,
        });

    Ok(next.run(request).await)
}

/// Health check endpoint
async fn health_handler() -> impl IntoResponse {
    let body = serde_json::json!({
        "status": "ok",
        "service": "apy-mcp",
        "version": env!("CARGO_PKG_VERSION")
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

/// RFC 9728 - OAuth Protected Resource Metadata
/// VS Code needs this to discover how to authenticate with the MCP server
async fn protected_resource_metadata_handler() -> Result<axum::Json<serde_json::Value>, StatusCode> {
    Ok(axum::Json(serde_json::json!({
        "resource": "http://localhost:3000/mcp",
        "authorization_servers": [
            {
                "issuer": "http://localhost:3000",
                "authorization_endpoint": "http://localhost:3000/oauth/authorize",
                "token_endpoint": "http://localhost:3000/oauth/token",
                "registration_endpoint": "http://localhost:3000/oauth/register"
            }
        ],
        "scopes_supported": ["openid", "profile", "email"],
        "bearer_methods_supported": ["header"]
    })))
}

/// RFC 8414 - OAuth Server Metadata Discovery
/// VS Code and other MCP clients use this to discover OAuth endpoints
async fn oauth_metadata_handler(
    State(_state): State<HttpState>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let base_url = "http://localhost:3000"; // TODO: make configurable

    Ok(axum::Json(serde_json::json!({
        "issuer": base_url,
        "authorization_endpoint": format!("{}/oauth/authorize", base_url),
        "token_endpoint": format!("{}/oauth/token", base_url),
        "registration_endpoint": format!("{}/oauth/register", base_url),
        "scopes_supported": ["openid", "profile", "email"],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
        "service_documentation": "https://github.com/Ellenp2p/apy-mcp",
        "code_challenge_methods_supported": ["S256"]
    })))
}

/// User registration page
async fn register_page_handler() -> Result<axum::response::Html<String>, StatusCode> {
    let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>APY MCP - Register</title>
    <style>
        body { font-family: -apple-system, sans-serif; background: #0f1117; color: #e4e6f0; display: flex; justify-content: center; align-items: center; min-height: 100vh; margin: 0; }
        .card { background: #1a1d29; border: 1px solid #2d3148; border-radius: 12px; padding: 32px; width: 400px; }
        h1 { margin: 0 0 24px 0; font-size: 24px; }
        h1 span { color: #6c5ce7; }
        input { width: 100%; padding: 12px; border: 1px solid #2d3148; border-radius: 8px; background: #0f1117; color: #e4e6f0; font-size: 14px; margin-bottom: 12px; box-sizing: border-box; }
        input:focus { outline: none; border-color: #6c5ce7; }
        button { width: 100%; padding: 12px; border: none; border-radius: 8px; background: #6c5ce7; color: white; font-size: 16px; cursor: pointer; }
        button:hover { background: #7c6ef7; }
        .link { text-align: center; margin-top: 16px; font-size: 14px; }
        .link a { color: #6c5ce7; text-decoration: none; }
        .link a:hover { text-decoration: underline; }
        .error { background: rgba(231,76,60,0.2); color: #e74c3c; padding: 10px; border-radius: 8px; margin-bottom: 16px; font-size: 14px; display: none; }
        .success { background: rgba(0,184,148,0.2); color: #00b894; padding: 10px; border-radius: 8px; margin-bottom: 16px; font-size: 14px; display: none; }
    </style>
</head>
<body>
    <div class="card">
        <h1>⚡ <span>APY</span> MCP Register</h1>
        <div class="error" id="error"></div>
        <div class="success" id="success"></div>
        <form id="registerForm">
            <input type="text" id="username" placeholder="Username (min 3 chars)" required minlength="3">
            <input type="email" id="email" placeholder="Email (optional)">
            <input type="password" id="password" placeholder="Password (min 6 chars)" required minlength="6">
            <input type="password" id="password2" placeholder="Confirm Password" required>
            <button type="submit">Register</button>
        </form>
        <div class="link">
            Already have an account? <a href="/oauth/authorize">Login</a>
        </div>
    </div>
    <script>
        document.getElementById('registerForm').addEventListener('submit', async (e) => {
            e.preventDefault();
            const errorEl = document.getElementById('error');
            const successEl = document.getElementById('success');
            errorEl.style.display = 'none';
            successEl.style.display = 'none';

            const username = document.getElementById('username').value;
            const email = document.getElementById('email').value;
            const password = document.getElementById('password').value;
            const password2 = document.getElementById('password2').value;

            if (password !== password2) {
                errorEl.textContent = 'Passwords do not match';
                errorEl.style.display = 'block';
                return;
            }

            try {
                const response = await fetch('/auth/register', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ username, email: email || null, password })
                });

                const data = await response.json();

                if (response.ok) {
                    successEl.textContent = 'Registration successful! You can now login.';
                    successEl.style.display = 'block';
                    document.getElementById('registerForm').reset();
                } else {
                    errorEl.textContent = data.error || 'Registration failed';
                    errorEl.style.display = 'block';
                }
            } catch (error) {
                errorEl.textContent = 'Network error: ' + error.message;
                errorEl.style.display = 'block';
            }
        });
    </script>
</body>
</html>"#;

    Ok(axum::response::Html(html.to_string()))
}

/// User registration endpoint
async fn user_register_handler(
    State(state): State<HttpState>,
    axum::extract::Json(req): axum::extract::Json<serde_json::Value>,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), StatusCode> {
    let username = req["username"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let password = req["password"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let email = req["email"].as_str();

    if username.len() < 3 {
        return Ok((StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({
            "error": "Username must be at least 3 characters"
        }))));
    }

    if password.len() < 6 {
        return Ok((StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({
            "error": "Password must be at least 6 characters"
        }))));
    }

    // Check if user already exists
    let existing = crate::oauth::User::get_by_username(&state.db.pool, username)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if existing.is_some() {
        return Ok((StatusCode::CONFLICT, axum::Json(serde_json::json!({
            "error": "Username already exists"
        }))));
    }

    // Create user
    let user = crate::oauth::User::create(&state.db.pool, username, password, email)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(user_id = user.id, username = %username, "User registered");

    Ok((StatusCode::CREATED, axum::Json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "message": "User registered successfully"
    }))))
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
    let scope = params.get("scope").cloned().unwrap_or_default();
    let error = params.get("error").cloned().unwrap_or_default();

    let error_msg = if error == "invalid_credentials" {
        r#"<div style="background: rgba(231,76,60,0.2); color: #e74c3c; padding: 10px; border-radius: 8px; margin-bottom: 16px; font-size: 14px;">Invalid username or password</div>"#
    } else {
        ""
    };

    // Query available OAuth providers from database
    let providers = crate::oauth::OAuthProvider::list(&http_state.db.pool)
        .await
        .unwrap_or_default();
    let active_providers: Vec<_> = providers
        .iter()
        .filter(|p| p.is_active && p.client_id.is_some())
        .collect();

    // Build social login buttons
    let mut social_buttons = String::new();
    for provider in &active_providers {
        let (icon, bg_color) = match provider.name.as_str() {
            "github" => ("🐙", "#24292e"),
            "google" => ("🔍", "#4285f4"),
            _ => ("🔐", "#6c5ce7"),
        };
        social_buttons.push_str(&format!(
            r#"<a href="/auth/{}" style="display: flex; align-items: center; justify-content: center; gap: 8px; width: 100%; padding: 12px; border: none; border-radius: 8px; background: {}; color: white; font-size: 16px; cursor: pointer; text-decoration: none; margin-bottom: 8px; box-sizing: border-box;">{} {}</a>"#,
            provider.name, bg_color, icon, capitalize(&provider.name)
        ));
    }

    let separator = if !social_buttons.is_empty() {
        r#"<div style="text-align: center; margin: 16px 0; color: #8b8fa3; font-size: 14px;">─── or ───</div>"#
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
        input {{ width: 100%; padding: 12px; border: 1px solid #2d3148; border-radius: 8px; background: #0f1117; color: #e4e6f0; font-size: 14px; margin-bottom: 12px; box-sizing: border-box; }}
        input:focus {{ outline: none; border-color: #6c5ce7; }}
        button {{ width: 100%; padding: 12px; border: none; border-radius: 8px; background: #6c5ce7; color: white; font-size: 16px; cursor: pointer; }}
        button:hover {{ background: #7c6ef7; }}
        .link {{ text-align: center; margin-top: 16px; font-size: 14px; }}
        .link a {{ color: #6c5ce7; text-decoration: none; }}
        .link a:hover {{ text-decoration: underline; }}
    </style>
</head>
<body>
    <div class="card">
        <h1>⚡ <span>APY</span> MCP Login</h1>
        <div class="info">
            <strong>Client ID:</strong> {client_id}<br>
            <strong>Scope:</strong> {scope}
        </div>
        {error_msg}
        {social_buttons}
        {separator}
        <form method="POST" action="/oauth/authorize">
            <input type="hidden" name="client_id" value="{client_id}">
            <input type="hidden" name="redirect_uri" value="{redirect_uri}">
            <input type="hidden" name="state" value="{state}">
            <input type="hidden" name="code_challenge" value="{code_challenge}">
            <input type="hidden" name="scope" value="{scope}">
            <input type="text" name="username" placeholder="Username" required>
            <input type="password" name="password" placeholder="Password" required>
            <button type="submit">Login & Authorize</button>
        </form>
        <div class="link">
            Don't have an account? <a href="/auth/register">Register</a>
        </div>
    </div>
</body>
</html>"#,
        client_id = client_id,
        scope = scope,
        redirect_uri = redirect_uri,
        state = state,
        code_challenge = code_challenge,
        error_msg = error_msg,
        social_buttons = social_buttons,
        separator = separator,
    );

    Ok(axum::response::Html(html))
}

/// Capitalize first letter of a string
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

/// Handle OAuth authorization form submission
async fn oauth_authorize_post_handler(
    State(state): State<HttpState>,
    axum::extract::Form(params): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Redirect, StatusCode> {
    let client_id = params.get("client_id").cloned().unwrap_or_default();
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let state_param = params.get("state").cloned().unwrap_or_default();
    let code_challenge = params.get("code_challenge").cloned().unwrap_or_default();
    let username = params.get("username").cloned().unwrap_or_default();
    let password = params.get("password").cloned().unwrap_or_default();

    // Authenticate user
    let user = crate::oauth::User::authenticate(&state.db.pool, &username, &password)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let user = match user {
        Some(u) => u,
        None => {
            // Build error URL
            let error_url = format!(
                "/oauth/authorize?client_id={}&redirect_uri={}&state={}&code_challenge={}&scope={}&error=invalid_credentials",
                urlencoding::encode(&client_id),
                urlencoding::encode(&redirect_uri),
                urlencoding::encode(&state_param),
                urlencoding::encode(&code_challenge),
                urlencoding::encode(&params.get("scope").cloned().unwrap_or_default())
            );
            return Ok(axum::response::Redirect::to(&error_url));
        }
    };

    // Generate authorization code
    let code = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();

    // Store authorization code
    sqlx::query(
        "INSERT INTO oauth_authorization_codes (code, client_id, user_id, redirect_uri, scope, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&code)
    .bind(&client_id)
    .bind(&user.username)
    .bind(&redirect_uri)
    .bind(params.get("scope").cloned().unwrap_or_default())
    .bind(&expires)
    .bind(&now)
    .execute(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Build redirect URL
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    let redirect_url = format!(
        "{}{}code={}&state={}",
        redirect_uri, separator, code, state_param
    );

    tracing::info!(client_id = %client_id, username = %user.username, "OAuth authorization granted");

    Ok(axum::response::Redirect::to(&redirect_url))
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
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let client_name = req["client_name"]
        .as_str()
        .unwrap_or("MCP Client");

    // Store in database
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO oauth_clients (client_id, client_secret, client_name, redirect_uris, created_at)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(&client_id)
    .bind(&client_secret_hash)
    .bind(client_name)
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(&now)
    .execute(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(client_id = %client_id, client_name = %client_name, "OAuth client registered");

    // Return credentials (client_secret shown once)
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret_raw,
            "client_name": client_name,
            "redirect_uris": redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "client_secret_post"
        })),
    ))
}

/// OAuth Token endpoint
/// Supports both client_secret_post (form body) and client_secret_basic (Authorization header)
async fn oauth_token_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    axum::extract::Form(params): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let grant_type = params.get("grant_type").ok_or(StatusCode::BAD_REQUEST)?;

    match grant_type.as_str() {
        "authorization_code" => {
            let code = params.get("code").ok_or(StatusCode::BAD_REQUEST)?;

            // Support both client_secret_post and client_secret_basic
            // First try form body, then fall back to Authorization header
            let (client_id, client_secret) = if let (Some(id), Some(secret)) =
                (params.get("client_id"), params.get("client_secret"))
            {
                // client_secret_post: credentials in form body
                (id.clone(), secret.clone())
            } else if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                // client_secret_basic: credentials in Authorization header (Base64 encoded)
                if let Some(basic) = auth_header.strip_prefix("Basic ") {
                    use base64::Engine;
                    match base64::engine::general_purpose::STANDARD.decode(basic) {
                        Ok(decoded) => {
                            let decoded_str = String::from_utf8_lossy(&decoded);
                            let parts: Vec<&str> = decoded_str.splitn(2, ':').collect();
                            if parts.len() == 2 {
                                (parts[0].to_string(), parts[1].to_string())
                            } else {
                                return Err(StatusCode::BAD_REQUEST);
                            }
                        }
                        Err(_) => return Err(StatusCode::BAD_REQUEST),
                    }
                } else {
                    return Err(StatusCode::BAD_REQUEST);
                }
            } else if let Some(id) = params.get("client_id") {
                // client_id in form body but no secret
                (id.clone(), String::new())
            } else {
                return Err(StatusCode::BAD_REQUEST);
            };

            // Check if client exists, if not auto-register (for VS Code compatibility)
            use sha2::{Digest, Sha256};
            let existing_client = sqlx::query_as::<_, (String, String)>(
                "SELECT client_id, client_secret FROM oauth_clients WHERE client_id = ?",
            )
            .bind(&client_id)
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            if existing_client.is_none() {
                // Auto-register client
                let now = chrono::Utc::now().to_rfc3339();
                let mut hasher = Sha256::new();
                hasher.update(client_secret.as_bytes());
                let secret_hash = hex::encode(hasher.finalize());

                sqlx::query(
                    "INSERT INTO oauth_clients (client_id, client_secret, client_name, redirect_uris, created_at) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&client_id)
                .bind(&secret_hash)
                .bind("VS Code MCP Client")
                .bind("[]")
                .bind(&now)
                .execute(&state.db.pool)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                tracing::info!(client_id = %client_id, "Auto-registered OAuth client");
            }

            // Validate authorization code
            let auth_code = sqlx::query_as::<_, (String, String, String)>(
                "SELECT code, client_id, user_id FROM oauth_authorization_codes WHERE code = ? AND client_id = ? AND expires_at > ?",
            )
            .bind(code)
            .bind(&client_id)
            .bind(chrono::Utc::now().to_rfc3339())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            match auth_code {
                Some((_, _, user_id)) => {
                    // Delete used code
                    sqlx::query("DELETE FROM oauth_authorization_codes WHERE code = ?")
                        .bind(code)
                        .execute(&state.db.pool)
                        .await
                        .ok();

                    // Generate access token
                    let access_token = format!("mcp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));

                    // Store access token
                    let now = chrono::Utc::now().to_rfc3339();
                    let expires_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
                    let result = sqlx::query(
                        "INSERT INTO oauth_access_tokens (token, user_id, client_id, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
                    )
                    .bind(&access_token)
                    .bind(&user_id)
                    .bind(&client_id)
                    .bind(&expires_at)
                    .bind(&now)
                    .execute(&state.db.pool)
                    .await;

                    if let Err(e) = &result {
                        tracing::error!(error = %e, "Failed to store access token");
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }

                    tracing::info!(user_id = %user_id, "Access token issued");

                    Ok(axum::Json(serde_json::json!({
                        "access_token": access_token,
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })))
                }
                None => {
                    tracing::warn!(code = %code, "Invalid or expired authorization code");
                    Err(StatusCode::UNAUTHORIZED)
                }
            }
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Start the HTTP server
pub async fn start_http_server(
    addr: SocketAddr,
    tools: ApyMcpTools,
    db: Database,
    admin_token: Option<String>,
) -> anyhow::Result<()> {
    let state = HttpState {
        tools: tools.clone(),
        db: db.clone(),
        admin_token: admin_token.clone(),
        rate_limiters: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };

    // Build public routes (no auth needed)
    let public_routes = Router::new()
        .route("/health", get(health_handler))
        .route("/.well-known/oauth-authorization-server", get(oauth_metadata_handler))
        .route("/.well-known/oauth-protected-resource", get(protected_resource_metadata_handler))
        .route("/oauth/authorize", get(oauth_authorize_handler).post(oauth_authorize_post_handler))
        .route("/oauth/register", post(oauth_register_handler))
        .route("/oauth/token", post(oauth_token_handler))
        .route("/auth/register", get(register_page_handler).post(user_register_handler))
        .route("/admin/keys", post(crate::admin::create_key).get(crate::admin::list_keys))
        .route("/admin/keys/{key_id}", delete(crate::admin::delete_key))
        .route("/admin/keys/{key_id}/deactivate", delete(crate::admin::deactivate_key))
        .route("/admin/keys/{key_id}/reactivate", post(crate::admin::reactivate_key))
        .route("/admin/stats", get(crate::admin::get_stats))
        .route("/admin/oauth/providers", get(crate::admin::list_oauth_providers))
        .route("/admin/oauth/providers", post(crate::admin::create_oauth_provider))
        .route("/admin/oauth/providers/{id}", delete(crate::admin::delete_oauth_provider))
        .route("/admin/oauth/providers/{id}/deactivate", delete(crate::admin::deactivate_oauth_provider))
        .route("/admin/oauth/providers/{id}/reactivate", post(crate::admin::reactivate_oauth_provider));

    // Build MCP service (like official example)
    let tools_for_service = tools.clone();
    let mcp_service: StreamableHttpService<crate::mcp::tools::ApyMcpTools, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(tools_for_service.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );

    // Build MCP routes (auth required) - use nest_service like official example
    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Build OAuth routes if configured
    let oauth_routes = crate::oauth::oauth_router_without_state();
    let base_url = format!("http://localhost:{}", addr.port());
    let oauth_callback_routes = crate::oauth::oauth_callback_router(db.pool.clone(), base_url);

    // Combine all routes with CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .merge(public_routes)
        .merge(mcp_routes)
        .merge(oauth_routes)
        .merge(oauth_callback_routes)
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
