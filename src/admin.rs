use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};

/// Request to create a new API key
#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    /// Human-readable name for this key
    pub name: String,
    /// Optional user identifier
    pub user_id: Option<String>,
    /// Rate limit (requests per minute), defaults to 100
    pub rate_limit: Option<i32>,
}

/// Response when creating a new API key
#[derive(Debug, Serialize)]
pub struct CreateKeyResponse {
    pub id: String,
    pub name: String,
    pub user_id: Option<String>,
    pub rate_limit: i32,
    pub created_at: String,
    /// The raw API key - only shown once!
    pub api_key: String,
}

/// Validate admin token from headers
fn validate_admin_token(
    headers: &HeaderMap,
    expected_token: &Option<String>,
) -> Result<(), StatusCode> {
    // If no admin token configured, allow all requests (development mode)
    let Some(expected_token) = expected_token else {
        return Ok(());
    };

    // Extract Authorization header
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Validate Bearer token
    if auth_header
        .strip_prefix("Bearer ")
        .is_some_and(|t| t == expected_token)
    {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Create a new API key
pub async fn create_key(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    axum::extract::Json(req): axum::extract::Json<CreateKeyRequest>,
) -> Result<(StatusCode, axum::extract::Json<CreateKeyResponse>), StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let (api_key, raw_key) = state
        .db
        .create_key(&req.name, req.user_id.as_deref(), req.rate_limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let response = CreateKeyResponse {
        id: api_key.id,
        name: api_key.name,
        user_id: api_key.user_id,
        rate_limit: api_key.rate_limit,
        created_at: api_key.created_at,
        api_key: raw_key,
    };

    Ok((StatusCode::CREATED, axum::extract::Json(response)))
}

/// List all API keys
pub async fn list_keys(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::extract::Json<Vec<crate::db::ApiKey>>, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let keys = state
        .db
        .list_keys()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Don't expose key hashes
    let keys: Vec<_> = keys
        .into_iter()
        .map(|mut k| {
            k.key_hash = "[REDACTED]".to_string();
            k
        })
        .collect();

    Ok(axum::extract::Json(keys))
}

/// Get usage statistics
pub async fn get_stats(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::extract::Json<Vec<crate::db::ApiKeyStats>>, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let stats = state
        .db
        .get_stats()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(axum::extract::Json(stats))
}

/// Deactivate an API key
pub async fn deactivate_key(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    state
        .db
        .deactivate_key(&key_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Reactivate an API key
pub async fn reactivate_key(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    state
        .db
        .reactivate_key(&key_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Delete an API key permanently
pub async fn delete_key(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    state
        .db
        .delete_key(&key_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

// ── OAuth Provider Management ────────────────────────────────────────

/// Request to create an OAuth provider
#[derive(Debug, Deserialize)]
pub struct CreateOAuthProviderRequest {
    /// Provider name (only "github" is supported for login)
    pub name: String,
    /// OAuth issuer URL (for dynamic registration) OR auth URL (for manual)
    pub issuer: Option<String>,
    /// Manual configuration (if not using dynamic registration)
    pub auth_url: Option<String>,
    pub token_url: Option<String>,
    pub user_info_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Comma-separated scopes
    pub scopes: Option<String>,
}

/// List all OAuth providers
pub async fn list_oauth_providers(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let providers = crate::oauth::OAuthProvider::list(&state.db.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(axum::Json(serde_json::json!({
        "providers": providers
    })))
}

/// Create an OAuth provider (dynamic or manual)
pub async fn create_oauth_provider(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    axum::extract::Json(req): axum::extract::Json<CreateOAuthProviderRequest>,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let scopes: Vec<String> = req
        .scopes
        .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let provider = if let Some(issuer) = &req.issuer {
        // Dynamic registration (RFC 7591)
        crate::oauth::OAuthProvider::create_with_dynamic_registration(
            &state.db.pool,
            &req.name,
            issuer,
            &scopes,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "Failed to create OAuth provider");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    } else if let (Some(auth_url), Some(token_url), Some(client_id), Some(client_secret)) = (
        &req.auth_url,
        &req.token_url,
        &req.client_id,
        &req.client_secret,
    ) {
        // Manual configuration
        crate::oauth::OAuthProvider::create_manual(
            &state.db.pool,
            &req.name,
            auth_url,
            token_url,
            req.user_info_url.as_deref(),
            client_id,
            client_secret,
            &scopes,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "Failed to create OAuth provider");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "id": provider.id,
            "name": provider.name,
            "auth_url": provider.auth_url,
            "is_dynamic": provider.is_dynamic,
            "client_id": provider.client_id,
        })),
    ))
}

/// Delete an OAuth provider
pub async fn delete_oauth_provider(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    crate::oauth::OAuthProvider::delete(&state.db.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Deactivate an OAuth provider
pub async fn deactivate_oauth_provider(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    crate::oauth::OAuthProvider::deactivate(&state.db.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Reactivate an OAuth provider
pub async fn reactivate_oauth_provider(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    crate::oauth::OAuthProvider::reactivate(&state.db.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

// ── RPC Provider Management ──────────────────────────────────────────

/// List all available RPC providers
pub async fn list_rpc_providers(
    State(_state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    validate_admin_token(&headers, &_state.admin_token)?;

    let providers = crate::chains::evm::providers::builtin_providers();
    let provider_list: Vec<serde_json::Value> = providers
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "needs_key": p.needs_key,
                "supported_chains": p.supported_chains(),
            })
        })
        .collect();

    // Also include current config
    let default_provider = _state
        .tools
        .state
        .aave_provider
        .get_rpc_manager()
        .get_default_provider()
        .await;

    Ok(axum::Json(serde_json::json!({
        "providers": provider_list,
        "default": default_provider.map(|(name, _)| name),
    })))
}

/// Check RPC health status for all chains
pub async fn get_rpc_status(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    // Get the RPC manager from AaveProvider
    let rpc = state.tools.state.aave_provider.get_rpc_manager();
    let statuses = rpc.check_all_chains_health().await;

    Ok(axum::Json(serde_json::json!({
        "chains": statuses
    })))
}

// ── GitHub Allowlist Management ──────────────────────────────────────

/// Request to add an entry to the GitHub allowlist
#[derive(Debug, Deserialize)]
pub struct AddAllowlistRequest {
    /// GitHub username or numeric UID
    pub value: String,
    /// Optional kind: "username" or "uid" (auto-detected when omitted)
    pub kind: Option<String>,
    /// Optional note
    pub note: Option<String>,
}

/// Detect whether a value is a GitHub UID (all digits) or a username
fn detect_allowlist_kind(value: &str) -> String {
    if value.parse::<u64>().is_ok() {
        "uid".to_string()
    } else {
        "username".to_string()
    }
}

/// List all GitHub allowlist entries
pub async fn list_github_allowlist(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let entries = state
        .db
        .list_allowlist()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(axum::Json(serde_json::json!({
        "entries": entries
    })))
}

/// Add an entry to the GitHub allowlist
pub async fn add_github_allowlist(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    axum::extract::Json(req): axum::extract::Json<AddAllowlistRequest>,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    let value = req.value.trim();
    if value.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let kind = req
        .kind
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| detect_allowlist_kind(value));

    state
        .db
        .add_allowlist(value, &kind, req.note.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!(value = %value, kind = %kind, "GitHub allowlist entry added");
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({ "value": value, "kind": kind })),
    ))
}

/// Remove an entry from the GitHub allowlist
pub async fn remove_github_allowlist(
    State(state): State<crate::http::HttpState>,
    headers: HeaderMap,
    Path(value): Path<String>,
) -> Result<StatusCode, StatusCode> {
    validate_admin_token(&headers, &state.admin_token)?;

    if !state
        .db
        .remove_allowlist(&value)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    tracing::info!(value = %value, "GitHub allowlist entry removed");
    Ok(StatusCode::OK)
}
