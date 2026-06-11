use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// OAuth provider configuration (for backward compatibility with CLI args)
#[derive(Clone, Debug)]
pub struct OAuthProviderConfig {
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
    pub auth_url: String,
    pub token_url: String,
    pub user_info_url: String,
    pub scopes: Vec<String>,
}

/// OAuth configuration for all providers (for backward compatibility)
#[derive(Clone)]
pub struct OAuthConfig {
    pub providers: std::collections::HashMap<String, OAuthProviderConfig>,
    pub base_url: String,
}

/// Combined state for OAuth routes
#[derive(Clone)]
pub struct OAuthState {
    pub pool: SqlitePool,
    pub base_url: String,
}

/// Generic OAuth user info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthUser {
    pub id: String,
    pub provider: String,
    pub login: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}

/// Query parameters for OAuth callback
#[derive(Debug, Deserialize)]
pub struct OAuthCallback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// OAuth provider stored in database (RFC 7591 + RFC 8414)
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OAuthProvider {
    pub id: i64,
    pub name: String,
    pub issuer: String,
    pub auth_url: String,
    pub token_url: String,
    pub user_info_url: Option<String>,
    pub registration_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: String,
    pub is_dynamic: bool,
    pub is_active: bool,
    pub created_at: String,
}

/// RFC 8414 - OAuth Server Metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
    pub response_types_supported: Option<Vec<String>>,
    pub grant_types_supported: Option<Vec<String>>,
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// RFC 7591 - Dynamic Client Registration Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRegistrationRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: Option<String>,
}

/// RFC 7591 - Dynamic Client Registration Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: Option<String>,
    pub client_id_issued_at: Option<u64>,
    pub client_secret_expires_at: Option<u64>,
}

/// Initialize OAuth tables in the database
pub async fn init_oauth_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS oauth_providers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            issuer TEXT NOT NULL,
            auth_url TEXT NOT NULL,
            token_url TEXT NOT NULL,
            user_info_url TEXT,
            registration_url TEXT,
            client_id TEXT,
            client_secret TEXT,
            scopes TEXT DEFAULT '',
            is_dynamic INTEGER DEFAULT 0,
            is_active INTEGER DEFAULT 1,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS oauth_sessions (
            csrf_token TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            redirect_to TEXT,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS oauth_users (
            id TEXT NOT NULL,
            provider TEXT NOT NULL,
            login TEXT NOT NULL,
            name TEXT,
            email TEXT,
            avatar_url TEXT,
            access_token TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_login TEXT NOT NULL,
            PRIMARY KEY (id, provider)
        );

        CREATE TABLE IF NOT EXISTS oauth_clients (
            client_id TEXT PRIMARY KEY,
            client_secret TEXT NOT NULL,
            client_name TEXT NOT NULL,
            redirect_uris TEXT DEFAULT '[]',
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS oauth_authorization_codes (
            code TEXT PRIMARY KEY,
            client_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            redirect_uri TEXT,
            scope TEXT,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS oauth_access_tokens (
            token TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            client_id TEXT NOT NULL,
            scope TEXT,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

impl OAuthProvider {
    /// Discover OAuth server metadata (RFC 8414)
    pub async fn discover_metadata(issuer: &str) -> Result<OAuthServerMetadata, reqwest::Error> {
        let client = reqwest::Client::new();

        // Try RFC 8414 well-known URL first
        let well_known_url = format!(
            "{}/.well-known/oauth-authorization-server",
            issuer.trim_end_matches('/')
        );

        let response = client
            .get(&well_known_url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if response.status().is_success() {
            let metadata: OAuthServerMetadata = response.json().await?;
            return Ok(metadata);
        }

        // Fallback: try OpenID Connect discovery
        let oidc_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );

        let response = client
            .get(&oidc_url)
            .header("Accept", "application/json")
            .send()
            .await?;

        let metadata: OAuthServerMetadata = response.json().await?;
        Ok(metadata)
    }

    /// Register client dynamically (RFC 7591)
    pub async fn register_client(
        registration_url: &str,
        request: &ClientRegistrationRequest,
    ) -> Result<ClientRegistrationResponse, reqwest::Error> {
        let client = reqwest::Client::new();

        let response = client
            .post(registration_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(request)
            .send()
            .await?;

        let reg_response: ClientRegistrationResponse = response.json().await?;
        Ok(reg_response)
    }

    /// Create provider with dynamic registration
    pub async fn create_with_dynamic_registration(
        pool: &SqlitePool,
        name: &str,
        issuer: &str,
        scopes: &[String],
    ) -> Result<Self, sqlx::Error> {
        // Discover metadata
        let metadata = Self::discover_metadata(issuer)
            .await
            .map_err(|e| sqlx::Error::Configuration(Box::new(e)))?;

        let registration_url = metadata.registration_endpoint.clone();

        // Register client if registration endpoint exists
        let (client_id, client_secret) = if let Some(ref reg_url) = registration_url {
            let reg_request = ClientRegistrationRequest {
                client_name: format!("APY MCP - {}", name),
                redirect_uris: vec![format!("/auth/{}/callback", name)],
                grant_types: vec!["authorization_code".to_string()],
                response_types: vec!["code".to_string()],
                scope: Some(scopes.join(" ")),
            };

            match Self::register_client(reg_url, &reg_request).await {
                Ok(resp) => (Some(resp.client_id), resp.client_secret),
                Err(e) => {
                    tracing::warn!(error = %e, "Dynamic client registration failed");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let now = chrono::Utc::now().to_rfc3339();
        let scopes_str = scopes.join(",");

        sqlx::query(
            r#"
            INSERT INTO oauth_providers (name, issuer, auth_url, token_url, user_info_url, registration_url, client_id, client_secret, scopes, is_dynamic, is_active, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?)
            "#,
        )
        .bind(name)
        .bind(issuer)
        .bind(&metadata.authorization_endpoint)
        .bind(&metadata.token_endpoint)
        .bind(&metadata.userinfo_endpoint)
        .bind(&registration_url)
        .bind(&client_id)
        .bind(&client_secret)
        .bind(&scopes_str)
        .bind(&now)
        .execute(pool)
        .await?;

        // Fetch the created provider
        Self::get_by_name(pool, name)
            .await?
            .ok_or_else(|| sqlx::Error::RowNotFound)
    }

    /// Create provider with manual configuration
    pub async fn create_manual(
        pool: &SqlitePool,
        name: &str,
        auth_url: &str,
        token_url: &str,
        user_info_url: Option<&str>,
        client_id: &str,
        client_secret: &str,
        scopes: &[String],
    ) -> Result<Self, sqlx::Error> {
        let now = chrono::Utc::now().to_rfc3339();
        let scopes_str = scopes.join(",");
        let issuer = auth_url
            .splitn(2, '/')
            .nth(2)
            .unwrap_or("")
            .to_string();

        sqlx::query(
            r#"
            INSERT INTO oauth_providers (name, issuer, auth_url, token_url, user_info_url, client_id, client_secret, scopes, is_dynamic, is_active, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 1, ?)
            "#,
        )
        .bind(name)
        .bind(&issuer)
        .bind(auth_url)
        .bind(token_url)
        .bind(user_info_url)
        .bind(client_id)
        .bind(client_secret)
        .bind(&scopes_str)
        .bind(&now)
        .execute(pool)
        .await?;

        // Fetch the created provider
        Self::get_by_name(pool, name)
            .await?
            .ok_or_else(|| sqlx::Error::RowNotFound)
    }

    /// List all providers
    pub async fn list(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let providers = sqlx::query_as::<_, Self>(
            "SELECT * FROM oauth_providers ORDER BY name",
        )
        .fetch_all(pool)
        .await?;
        Ok(providers)
    }

    /// Get provider by name
    pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<Self>, sqlx::Error> {
        let provider = sqlx::query_as::<_, Self>(
            "SELECT * FROM oauth_providers WHERE name = ? AND is_active = 1",
        )
        .bind(name)
        .fetch_optional(pool)
        .await?;
        Ok(provider)
    }

    /// Delete provider
    pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM oauth_providers WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deactivate provider
    pub async fn deactivate(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE oauth_providers SET is_active = 0 WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Reactivate provider
    pub async fn reactivate(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE oauth_providers SET is_active = 1 WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Generate a random CSRF token
fn generate_csrf_token() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(uuid::Uuid::new_v4().to_string());
    hasher.update(chrono::Utc::now().to_rfc3339());
    hex::encode(hasher.finalize())
}

/// Build OAuth authorization URL for any provider
fn build_auth_url(provider: &OAuthProviderConfig, csrf_token: &str, redirect_uri: &str) -> String {
    let scopes = provider.scopes.join(" ");
    let redirect = urlencoding::encode(redirect_uri);

    format!(
        "{}?client_id={}&redirect_uri={}&scope={}&state={}&response_type=code",
        provider.auth_url,
        provider.client_id,
        redirect,
        urlencoding::encode(&scopes),
        csrf_token
    )
}

/// Exchange authorization code for access token
async fn exchange_code(
    provider: &OAuthProviderConfig,
    code: &str,
) -> Result<String, StatusCode> {
    let client = reqwest::Client::new();

    let mut params = std::collections::HashMap::new();
    params.insert("client_id", provider.client_id.as_str());
    params.insert("client_secret", provider.client_secret.as_str());
    params.insert("code", code);
    params.insert("grant_type", "authorization_code");

    let token_response = client
        .post(&provider.token_url)
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let token_body: serde_json::Value = token_response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Try different token field names (access_token for most, id_token for some)
    let access_token = token_body
        .get("access_token")
        .or_else(|| token_body.get("id_token"))
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok(access_token.to_string())
}

/// Get user info from provider
async fn get_user_info(
    provider: &OAuthProviderConfig,
    access_token: &str,
) -> Result<OAuthUser, StatusCode> {
    let client = reqwest::Client::new();

    let response = match provider.name.as_str() {
        "github" => {
            client
                .get(&provider.user_info_url)
                .header("Authorization", format!("Bearer {}", access_token))
                .header("User-Agent", "apy-mcp")
                .send()
                .await
        }
        "google" => {
            client
                .get(&provider.user_info_url)
                .header("Authorization", format!("Bearer {}", access_token))
                .send()
                .await
        }
        _ => {
            // Custom provider - try standard Bearer token
            client
                .get(&provider.user_info_url)
                .header("Authorization", format!("Bearer {}", access_token))
                .send()
                .await
        }
    }
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let user_info: serde_json::Value = response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Parse user info based on provider
    let user = match provider.name.as_str() {
        "github" => OAuthUser {
            id: user_info["id"].as_i64().unwrap_or(0).to_string(),
            provider: "github".to_string(),
            login: user_info["login"].as_str().unwrap_or("").to_string(),
            name: user_info["name"].as_str().map(|s| s.to_string()),
            email: user_info["email"].as_str().map(|s| s.to_string()),
            avatar_url: user_info["avatar_url"].as_str().map(|s| s.to_string()),
        },
        "google" => OAuthUser {
            id: user_info["sub"].as_str().unwrap_or("").to_string(),
            provider: "google".to_string(),
            login: user_info["email"].as_str().unwrap_or("").to_string(),
            name: user_info["name"].as_str().map(|s| s.to_string()),
            email: user_info["email"].as_str().map(|s| s.to_string()),
            avatar_url: user_info["picture"].as_str().map(|s| s.to_string()),
        },
        _ => {
            // Custom provider - try common fields
            let id = user_info["id"]
                .as_str()
                .or_else(|| user_info["sub"].as_str())
                .or_else(|| user_info["user_id"].as_str())
                .unwrap_or("")
                .to_string();

            let login = user_info["login"]
                .as_str()
                .or_else(|| user_info["username"].as_str())
                .or_else(|| user_info["email"].as_str())
                .unwrap_or("")
                .to_string();

            OAuthUser {
                id,
                provider: provider.name.clone(),
                login,
                name: user_info["name"]
                    .as_str()
                    .or_else(|| user_info["display_name"].as_str())
                    .map(|s| s.to_string()),
                email: user_info["email"].as_str().map(|s| s.to_string()),
                avatar_url: user_info["avatar_url"]
                    .as_str()
                    .or_else(|| user_info["picture"].as_str())
                    .or_else(|| user_info["profile_image_url"].as_str())
                    .map(|s| s.to_string()),
            }
        }
    };

    Ok(user)
}

/// Start OAuth flow for any provider
async fn oauth_auth(
    State(state): State<OAuthState>,
    Path(provider_name): Path<String>,
) -> Result<Redirect, StatusCode> {
    // Look up provider from database
    let provider = OAuthProvider::get_by_name(&state.pool, &provider_name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some(client_id) = &provider.client_id {
        let csrf_token = generate_csrf_token();
        let redirect_uri = format!("{}/auth/{}/callback", state.base_url, provider_name);

        // Store CSRF token in database
        let now = chrono::Utc::now().to_rfc3339();
        let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();

        sqlx::query(
            "INSERT INTO oauth_sessions (csrf_token, provider, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&csrf_token)
        .bind(&provider_name)
        .bind(&now)
        .bind(&expires)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let scopes = provider.scopes.split(',').collect::<Vec<_>>().join(" ");
        let redirect = urlencoding::encode(&redirect_uri);
        let auth_url = format!(
            "{}?client_id={}&redirect_uri={}&scope={}&state={}&response_type=code",
            provider.auth_url, client_id, redirect, urlencoding::encode(&scopes), csrf_token
        );

        Ok(Redirect::to(&auth_url))
    } else {
        // Provider not configured with client credentials
        Ok(Redirect::to("/?error=provider_not_configured"))
    }
}

/// Handle OAuth callback for any provider
async fn oauth_callback(
    State(state): State<OAuthState>,
    Path(provider_name): Path<String>,
    Query(callback): Query<OAuthCallback>,
) -> Result<Response, StatusCode> {
    // Check for error
    if let Some(error) = callback.error {
        tracing::warn!(error = %error, provider = %provider_name, "OAuth callback error");
        return Ok(Redirect::to("/?error=oauth_denied").into_response());
    }

    let (code, csrf_state) = match (callback.code, callback.state) {
        (Some(code), Some(state)) => (code, state),
        _ => {
            return Ok(Redirect::to("/?error=missing_params").into_response());
        }
    };

    // Validate CSRF token and provider
    let session = sqlx::query_as::<_, (String, String)>(
        "SELECT csrf_token, provider FROM oauth_sessions WHERE csrf_token = ? AND expires_at > ?",
    )
    .bind(&csrf_state)
    .bind(chrono::Utc::now().to_rfc3339())
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match session {
        Some((_, stored_provider)) if stored_provider == provider_name => {
            // Valid session
        }
        _ => {
            return Ok(Redirect::to("/?error=invalid_state").into_response());
        }
    }

    // Delete used CSRF token
    sqlx::query("DELETE FROM oauth_sessions WHERE csrf_token = ?")
        .bind(&csrf_state)
        .execute(&state.pool)
        .await
        .ok();

    // Look up provider from database
    let provider = OAuthProvider::get_by_name(&state.pool, &provider_name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let client_id = provider.client_id.as_ref().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let client_secret = provider.client_secret.as_ref().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Exchange code for access token
    let token_client = reqwest::Client::new();
    let mut params = std::collections::HashMap::new();
    params.insert("client_id", client_id.as_str());
    params.insert("client_secret", client_secret.as_str());
    params.insert("code", code.as_str());
    params.insert("grant_type", "authorization_code");

    let token_response = token_client
        .post(&provider.token_url)
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let token_body: serde_json::Value = token_response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let access_token = token_body
        .get("access_token")
        .or_else(|| token_body.get("id_token"))
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Get user info
    let user_info_url = provider.user_info_url.as_ref().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let user_response = token_client
        .get(user_info_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let user_info: serde_json::Value = user_response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Parse user info (try common fields)
    let id = user_info["id"]
        .as_str()
        .or_else(|| user_info["sub"].as_str())
        .or_else(|| user_info["user_id"].as_str())
        .unwrap_or("")
        .to_string();

    let login = user_info["login"]
        .as_str()
        .or_else(|| user_info["username"].as_str())
        .or_else(|| user_info["email"].as_str())
        .unwrap_or("")
        .to_string();

    let oauth_user = OAuthUser {
        id,
        provider: provider_name.clone(),
        login,
        name: user_info["name"]
            .as_str()
            .or_else(|| user_info["display_name"].as_str())
            .map(|s| s.to_string()),
        email: user_info["email"].as_str().map(|s| s.to_string()),
        avatar_url: user_info["avatar_url"]
            .as_str()
            .or_else(|| user_info["picture"].as_str())
            .or_else(|| user_info["profile_image_url"].as_str())
            .map(|s| s.to_string()),
    };

    // Store or update user in database
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO oauth_users (id, provider, login, name, email, avatar_url, access_token, created_at, last_login)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id, provider) DO UPDATE SET
            login = excluded.login,
            name = excluded.name,
            email = excluded.email,
            avatar_url = excluded.avatar_url,
            access_token = excluded.access_token,
            last_login = excluded.last_login
        "#,
    )
    .bind(&oauth_user.id)
    .bind(&oauth_user.provider)
    .bind(&oauth_user.login)
    .bind(&oauth_user.name)
    .bind(&oauth_user.email)
    .bind(&oauth_user.avatar_url)
    .bind(&access_token)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(
        user_id = %oauth_user.id,
        login = %oauth_user.login,
        provider = %provider_name,
        "OAuth login successful"
    );

    // Redirect to success page
    let redirect_url = format!(
        "/?oauth=success&provider={}&user={}",
        urlencoding::encode(&provider_name),
        urlencoding::encode(&oauth_user.login)
    );
    Ok(Redirect::to(&redirect_url).into_response())
}

/// Get current user from access token (for API)
pub async fn get_current_user(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Check if it's an OAuth access token
    let user = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, provider, login, name, email, avatar_url FROM oauth_users WHERE access_token = ?",
    )
    .bind(token)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match user {
        Some((id, provider, login, name, email, avatar_url)) => Ok(axum::Json(serde_json::json!({
            "id": id,
            "provider": provider,
            "login": login,
            "name": name,
            "email": email,
            "avatar_url": avatar_url
        }))),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// List available OAuth providers from database
async fn list_providers(
    State(state): State<crate::http::HttpState>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let providers = OAuthProvider::list(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let providers: Vec<serde_json::Value> = providers
        .iter()
        .filter(|p| p.is_active && p.client_id.is_some())
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "auth_url": format!("/auth/{}", p.name),
                "is_dynamic": p.is_dynamic,
            })
        })
        .collect();

    Ok(axum::Json(serde_json::json!({
        "providers": providers,
        "login_base_url": "/auth"
    })))
}

/// Build the OAuth router (without state - will be nested with main router)
pub fn oauth_router_without_state() -> Router<crate::http::HttpState> {
    Router::new()
        .route("/auth/user", get(get_current_user))
        .route("/auth/providers", get(list_providers))
}

/// Build the OAuth callback router (with OAuthState)
pub fn oauth_callback_router(pool: SqlitePool, base_url: String) -> Router<crate::http::HttpState> {
    let state = OAuthState { pool, base_url };

    Router::new()
        .route("/auth/{provider}", get(oauth_auth))
        .route("/auth/{provider}/callback", get(oauth_callback))
        .with_state(state)
}
