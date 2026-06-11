use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// OAuth provider configuration
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

/// OAuth configuration for all providers
#[derive(Clone)]
pub struct OAuthConfig {
    pub providers: std::collections::HashMap<String, OAuthProviderConfig>,
    pub base_url: String,
}

/// Combined state for OAuth routes
#[derive(Clone)]
pub struct OAuthState {
    pub config: OAuthConfig,
    pub pool: SqlitePool,
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

/// Initialize OAuth tables in the database
pub async fn init_oauth_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
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
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
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
    let provider = state
        .config
        .providers
        .get(&provider_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let csrf_token = generate_csrf_token();
    let redirect_uri = format!("{}/auth/{}/callback", state.config.base_url, provider_name);

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

    let auth_url = build_auth_url(provider, &csrf_token, &redirect_uri);

    Ok(Redirect::to(&auth_url))
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

    let provider = state
        .config
        .providers
        .get(&provider_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Exchange code for access token
    let access_token = exchange_code(provider, &code).await?;

    // Get user info
    let oauth_user = get_user_info(provider, &access_token).await?;

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

/// List available OAuth providers
async fn list_providers(
    State(_state): State<crate::http::HttpState>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    // This endpoint is a placeholder - actual provider listing would need OAuth config
    Ok(axum::Json(serde_json::json!({
        "providers": [],
        "login_base_url": "/auth",
        "note": "Configure OAuth providers via CLI or environment variables"
    })))
}

/// Build the OAuth router (without state - will be nested with main router)
pub fn oauth_router_without_state() -> Router<crate::http::HttpState> {
    Router::new()
        .route("/auth/user", get(get_current_user))
        .route("/auth/providers", get(list_providers))
}

/// Build the OAuth callback router (with OAuthState)
pub fn oauth_callback_router(config: OAuthConfig, pool: SqlitePool) -> Router {
    let state = OAuthState { config, pool };

    Router::new()
        .route("/auth/{provider}", get(oauth_auth))
        .route("/auth/{provider}/callback", get(oauth_callback))
        .with_state(state)
}
