use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use http_body_util::BodyExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tower_service::Service;

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

    // Validate API key
    let api_key = state
        .db
        .validate_key(raw_key)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
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

/// Handle MCP POST requests
async fn mcp_post_handler(
    State(state): State<HttpState>,
    request: Request,
) -> Response {
    // Create a new service instance for each request (stateless mode)
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(["localhost", "127.0.0.1", "::1"]);

    let session_manager = Arc::new(LocalSessionManager::default());

    let service_factory = {
        let tools = state.tools.clone();
        move || -> Result<_, std::io::Error> { Ok(tools.clone()) }
    };

    let mut service = StreamableHttpService::new(service_factory, session_manager, config);

    // Forward the request to the MCP service
    let response = match service.call(request).await {
        Ok(response) => response,
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(
                http_body_util::Full::new(bytes::Bytes::from("Internal Server Error")).boxed(),
            )
            .unwrap(),
    };

    // Convert from BoxBody<Bytes, Infallible> to axum::body::Body
    let (parts, body) = response.into_parts();
    let body = body
        .map_err(|e: std::convert::Infallible| -> Box<dyn std::error::Error + Send + Sync> {
            match e {}
        })
        .boxed_unsync();
    let body = Body::new(body);
    Response::from_parts(parts, body)
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

/// Start the HTTP server
pub async fn start_http_server(
    addr: SocketAddr,
    tools: ApyMcpTools,
    db: Database,
    admin_token: Option<String>,
) -> anyhow::Result<()> {
    let state = HttpState {
        tools,
        db: db.clone(),
        admin_token: admin_token.clone(),
        rate_limiters: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };

    // Build admin routes (no auth middleware - they handle auth internally)
    let _admin_routes = crate::admin::admin_routes();

    // Build public routes (no auth needed)
    let public_routes = Router::new()
        .route("/health", get(health_handler))
        .route("/admin/keys", post(crate::admin::create_key).get(crate::admin::list_keys))
        .route("/admin/keys/{key_id}", delete(crate::admin::delete_key))
        .route("/admin/keys/{key_id}/deactivate", delete(crate::admin::deactivate_key))
        .route("/admin/keys/{key_id}/reactivate", post(crate::admin::reactivate_key))
        .route("/admin/stats", get(crate::admin::get_stats));

    // Build MCP routes (auth required)
    let mcp_routes = Router::new()
        .route("/mcp", axum::routing::post(mcp_post_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Combine all routes
    let app = Router::new()
        .merge(public_routes)
        .merge(mcp_routes)
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
