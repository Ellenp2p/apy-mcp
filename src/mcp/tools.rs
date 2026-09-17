use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::chains::evm::rpc::RpcManager;
use crate::db::Database;
use crate::service::rates::RateService;
use crate::service::types::QueryRatesParams;

// ── Tool parameter types ─────────────────────────────────────────────

/// Request metadata (custom headers, etc.)
#[derive(Debug, Clone)]
pub struct RequestMetadata {
    pub custom_headers: Vec<(String, String)>,
}

/// Shared state across tool invocations
#[derive(Clone)]
pub struct ApyMcpTools {
    pub service: RateService,
}

impl ApyMcpTools {
    /// Create new tools instance with a default Blend pool
    pub fn new(pool_id: &str) -> Self {
        Self {
            service: RateService::new(pool_id),
        }
    }

    /// Create new tools instance with custom RPC manager
    pub fn with_rpc_manager(pool_id: &str, rpc: RpcManager) -> Self {
        Self {
            service: RateService::with_rpc_manager(pool_id, rpc),
        }
    }

    /// Create new tools instance with custom RPC manager and database
    pub fn with_rpc_manager_and_db(pool_id: &str, rpc: RpcManager, db: Database) -> Self {
        Self {
            service: RateService::with_rpc_manager_and_db(pool_id, rpc, db),
        }
    }

    /// Wrap an existing rate service
    pub fn from_service(service: RateService) -> Self {
        Self { service }
    }
}

#[tool_router(server_handler)]
impl ApyMcpTools {
    #[tool(description = "DeFi lending rate query tool. Actions:\n\
        - \"query\" (default): Query rates with filters (chain, asset, protocol, APY range, utilization)\n\
        - \"add\": Add a pool to monitoring (requires chain + pool_id)\n\
        - \"list\": List all monitored pools\n\
        Supports Aave V3 (EVM), Spark Savings (EVM, protocol \"spark\"), and Blend (Stellar). All parameters are optional for query.\n\
        Data is cached for 120 seconds by default. Set use_cache=false to force fresh data.")]
    async fn query_rates(
        &self,
        Parameters(params): Parameters<QueryRatesParams>,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> String {
        // Log custom headers if present
        if let Some(metadata) = ctx.extensions.get::<RequestMetadata>() {
            if !metadata.custom_headers.is_empty() {
                tracing::info!(
                    tool = "query_rates",
                    action = %params.action,
                    chain = ?params.chain,
                    asset = ?params.asset,
                    protocol = ?params.protocol,
                    custom_headers = ?metadata.custom_headers,
                    "Tool called"
                );
            }
        }

        match params.action.as_str() {
            "add" => self.handle_add_pool(&params).await,
            "list" => {
                let pools = self.service.list_pools().await;
                serde_json::to_string_pretty(&pools)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e))
            }
            _ => match self.service.query(&params).await {
                Ok(response) => serde_json::to_string_pretty(&response)
                    .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e)),
                Err(e) => format!("{{\"error\": \"{}\"}}", e),
            },
        }
    }
}

impl ApyMcpTools {
    /// Handle "add" action - add a pool to monitoring
    async fn handle_add_pool(&self, params: &QueryRatesParams) -> String {
        if params.chain.is_none() {
            return r#"{"error": "chain is required for add action"}"#.to_string();
        }
        if params.pool_id.is_none() {
            return r#"{"error": "pool_id is required for add action"}"#.to_string();
        }

        let status = self.service.add_pool(params).await;
        serde_json::to_string(&status).unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e))
    }
}
