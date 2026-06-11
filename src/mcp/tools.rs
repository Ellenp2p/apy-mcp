use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::chains::stellar::BlendProvider;
use crate::chains::LendingProvider;
use crate::mcp::types::{AllRatesResponse, StatusResponse};

// ── Tool parameter types ─────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetBlendRatesParams {
    /// The Blend Capital pool contract address (C... format).
    /// Example: CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD
    pub pool_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddPoolParams {
    /// Chain identifier: "stellar", "sui", "evm", "aptos", "solana"
    pub chain: String,
    /// Pool contract address
    pub pool_id: String,
}

/// Request metadata (custom headers, etc.)
#[derive(Debug, Clone)]
pub struct RequestMetadata {
    pub custom_headers: Vec<(String, String)>,
}

// ── Tool router ──────────────────────────────────────────────────────

/// Shared state across tool invocations
#[derive(Clone)]
pub struct AppState {
    pub blend_provider: BlendProvider,
    pub monitored_pools: Arc<RwLock<Vec<String>>>,
}

#[derive(Clone)]
pub struct ApyMcpTools {
    pub state: AppState,
}

impl ApyMcpTools {
    /// Create new tools instance with a default Blend pool
    pub fn new(pool_id: &str) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        Self {
            state: AppState {
                blend_provider: provider,
                monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
            },
        }
    }
}

#[tool_router(server_handler)]
impl ApyMcpTools {
    #[tool(
        description = "Query lending/borrowing interest rates for a Blend Capital pool on Stellar. \
        Returns per-asset supply APY, borrow APY, utilization, and total supplied/borrowed amounts."
    )]
    async fn get_blend_rates(
        &self,
        Parameters(params): Parameters<GetBlendRatesParams>,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> String {
        // Log custom headers if present
        if let Some(metadata) = ctx.extensions.get::<RequestMetadata>() {
            if !metadata.custom_headers.is_empty() {
                tracing::info!(
                    tool = "get_blend_rates",
                    pool_id = %params.pool_id,
                    custom_headers = ?metadata.custom_headers,
                    "Tool called with custom headers"
                );
            }
        }

        match self
            .state
            .blend_provider
            .get_pool_rates(&params.pool_id)
            .await
        {
            Ok(rates) => serde_json::to_string_pretty(&rates)
                .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e)),
            Err(e) => {
                format!("{{\"error\": \"{}\"}}", e)
            }
        }
    }

    #[tool(
        description = "Get lending/borrowing rates for all monitored DeFi pools across all chains. \
        Returns a summary of all pools with their current interest rates."
    )]
    async fn get_all_rates(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> String {
        // Log custom headers if present
        if let Some(metadata) = ctx.extensions.get::<RequestMetadata>() {
            if !metadata.custom_headers.is_empty() {
                tracing::info!(
                    tool = "get_all_rates",
                    custom_headers = ?metadata.custom_headers,
                    "Tool called with custom headers"
                );
            }
        }

        let pools = self.state.monitored_pools.read().await;
        let mut results = Vec::new();

        for pool_id in pools.iter() {
            match self.state.blend_provider.get_pool_rates(pool_id).await {
                Ok(rates) => results.push(rates),
                Err(e) => {
                    tracing::warn!(pool_id = pool_id, error = %e, "Failed to fetch pool rates");
                }
            }
        }

        let response = AllRatesResponse {
            pools: results,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        };

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e))
    }

    #[tool(description = "Add a DeFi lending pool to the monitoring list. \
        Currently supports Stellar Blend pools. More chains will be added.")]
    async fn add_pool(
        &self,
        Parameters(params): Parameters<AddPoolParams>,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> String {
        // Log custom headers if present
        if let Some(metadata) = ctx.extensions.get::<RequestMetadata>() {
            if !metadata.custom_headers.is_empty() {
                tracing::info!(
                    tool = "add_pool",
                    chain = %params.chain,
                    pool_id = %params.pool_id,
                    custom_headers = ?metadata.custom_headers,
                    "Tool called with custom headers"
                );
            }
        }

        match params.chain.as_str() {
            "stellar" => {
                let mut pools = self.state.monitored_pools.write().await;
                if pools.contains(&params.pool_id) {
                    serde_json::to_string(&StatusResponse {
                        success: true,
                        message: format!("Pool {} is already being monitored", params.pool_id),
                    })
                } else {
                    pools.push(params.pool_id.clone());
                    serde_json::to_string(&StatusResponse {
                        success: true,
                        message: format!("Added pool {} to monitoring list", params.pool_id),
                    })
                }
            }
            _ => serde_json::to_string(&StatusResponse {
                success: false,
                message: format!(
                    "Chain '{}' is not yet supported. Currently supported: stellar",
                    params.chain
                ),
            }),
        }
        .unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e))
    }
}
