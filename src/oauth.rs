use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// OAuth configuration
#[derive(Clone)]
pub struct OAuthConfig {
    pub github_client_id: String,
    pub github_client_secret: String,
    pub redirect_uri: String,
}

/// Combined state for OAuth routes
#[derive(Clone)]
pub struct OAuthState {
    pub config: OAuthConfig,
    pub pool: SqlitePool,
}

/// GitHub user info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub id: u64,
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
            redirect_to TEXT,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_users (
            id INTEGER PRIMARY KEY,
            login TEXT NOT NULL,
            name TEXT,
            email TEXT,
            avatar_url TEXT,
            access_token TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_login TEXT NOT NULL
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

/// Start GitHub OAuth flow
async fn github_auth(State(state): State<OAuthState>) -> Result<Redirect, StatusCode> {
    let csrf_token = generate_csrf_token();

    // Store CSRF token in database
    let now = chrono::Utc::now().to_rfc3339();
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();

    sqlx::query(
        "INSERT INTO oauth_sessions (csrf_token, created_at, expires_at) VALUES (?, ?, ?)",
    )
    .bind(&csrf_token)
    .bind(&now)
    .bind(&expires)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Build GitHub OAuth URL
    let redirect_uri = urlencoding::encode(&state.config.redirect_uri);
    let auth_url = format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read:user,user:email&state={}",
        state.config.github_client_id, redirect_uri, csrf_token
    );

    Ok(Redirect::to(&auth_url))
}

/// Handle GitHub OAuth callback
async fn github_callback(
    State(state): State<OAuthState>,
    Query(callback): Query<OAuthCallback>,
) -> Result<Response, StatusCode> {
    // Check for error
    if let Some(error) = callback.error {
        tracing::warn!(error = %error, "OAuth callback error");
        return Ok(Redirect::to("/?error=oauth_denied").into_response());
    }

    let (code, csrf_state) = match (callback.code, callback.state) {
        (Some(code), Some(state)) => (code, state),
        _ => {
            return Ok(Redirect::to("/?error=missing_params").into_response());
        }
    };

    // Validate CSRF token
    let session = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT csrf_token, redirect_to FROM oauth_sessions WHERE csrf_token = ? AND expires_at > ?",
    )
    .bind(&csrf_state)
    .bind(chrono::Utc::now().to_rfc3339())
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if session.is_none() {
        return Ok(Redirect::to("/?error=invalid_state").into_response());
    }

    // Delete used CSRF token
    sqlx::query("DELETE FROM oauth_sessions WHERE csrf_token = ?")
        .bind(&csrf_state)
        .execute(&state.pool)
        .await
        .ok();

    // Exchange code for access token
    let client = reqwest::Client::new();
    let token_response = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", &state.config.github_client_id),
            ("client_secret", &state.config.github_client_secret),
            ("code", &code),
        ])
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let token_body: serde_json::Value = token_response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let access_token = token_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Get user info from GitHub
    let user_response = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", "apy-mcp")
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let github_user: GitHubUser = user_response
        .json()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Store or update user in database
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO github_users (id, login, name, email, avatar_url, access_token, created_at, last_login)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            login = excluded.login,
            name = excluded.name,
            email = excluded.email,
            avatar_url = excluded.avatar_url,
            access_token = excluded.access_token,
            last_login = excluded.last_login
        "#,
    )
    .bind(github_user.id as i64)
    .bind(&github_user.login)
    .bind(&github_user.name)
    .bind(&github_user.email)
    .bind(&github_user.avatar_url)
    .bind(access_token)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(
        user_id = github_user.id,
        login = %github_user.login,
        "GitHub OAuth login successful"
    );

    // Redirect to success page with user info
    let redirect_url = format!(
        "/?oauth=success&user={}",
        urlencoding::encode(&github_user.login)
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

    // Check if it's a GitHub access token
    let user = sqlx::query_as::<_, (i64, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, login, name, email, avatar_url FROM github_users WHERE access_token = ?",
    )
    .bind(token)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match user {
        Some((id, login, name, email, avatar_url)) => Ok(axum::Json(serde_json::json!({
            "id": id,
            "login": login,
            "name": name,
            "email": email,
            "avatar_url": avatar_url,
            "provider": "github"
        }))),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Build the OAuth router (without state - will be nested with main router)
pub fn oauth_router_without_state() -> Router<crate::http::HttpState> {
    Router::new()
        .route("/auth/user", get(get_current_user))
}

/// Build the OAuth callback router (with OAuthState)
pub fn oauth_callback_router(config: OAuthConfig, pool: SqlitePool) -> Router {
    let state = OAuthState { config, pool };

    Router::new()
        .route("/auth/github", get(github_auth))
        .route("/auth/github/callback", get(github_callback))
        .with_state(state)
}
