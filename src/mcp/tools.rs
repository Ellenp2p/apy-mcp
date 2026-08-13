use futures::future::join_all;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::chains::evm::rpc::RpcManager;
use crate::chains::evm::AaveProvider;
use crate::chains::stellar::BlendProvider;
use crate::chains::LendingProvider;
use crate::db::Database;
use crate::mcp::types::{AllRatesResponse, AssetRate, PoolRates};

// ── Tool parameter types ─────────────────────────────────────────────

/// Unified rate query parameters
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryRatesParams {
    /// Action: "query" (default), "add" (add a pool), "list" (list monitored pools)
    #[serde(default = "default_action")]
    pub action: String,

    /// Filter by chain name (e.g., "ethereum", "polygon", "stellar").
    /// If omitted, queries all available chains.
    pub chain: Option<String>,

    /// Filter by asset symbol (e.g., "USDC", "WETH", "WBTC").
    /// Matches partial names (e.g., "USD" matches "USDC", "USDT").
    pub asset: Option<String>,

    /// Filter by protocol: "aave_v3", "blend", or "all" (default).
    /// "all" queries both Aave V3 and Blend protocols.
    pub protocol: Option<String>,

    /// Blend Capital pool contract address (C... format).
    /// Required when protocol="blend" and querying a specific pool.
    /// Example: CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD
    pub pool_id: Option<String>,

    /// Minimum supply APY filter (0.0 - 1.0, e.g., 0.05 = 5%)
    pub min_supply_apy: Option<f64>,

    /// Maximum supply APY filter (0.0 - 1.0)
    pub max_supply_apy: Option<f64>,

    /// Minimum borrow APY filter (0.0 - 1.0)
    pub min_borrow_apy: Option<f64>,

    /// Maximum borrow APY filter (0.0 - 1.0)
    pub max_borrow_apy: Option<f64>,

    /// Minimum utilization filter (0.0 - 1.0)
    pub min_utilization: Option<f64>,

    /// Maximum utilization filter (0.0 - 1.0)
    pub max_utilization: Option<f64>,

    /// Whether to use cached data (default: true, cache TTL is 60 seconds).
    /// Set to false to force fresh data from chain.
    #[serde(default = "default_true")]
    pub use_cache: bool,
}

fn default_action() -> String {
    "query".to_string()
}

fn default_true() -> bool {
    true
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
    pub aave_provider: AaveProvider,
    pub monitored_pools: Arc<RwLock<Vec<String>>>,
    pub db: Option<Database>,
}

/// Default cache TTL in seconds
const DEFAULT_CACHE_TTL: i64 = 60;

#[derive(Clone)]
pub struct ApyMcpTools {
    pub state: AppState,
}

impl ApyMcpTools {
    /// Create new tools instance with a default Blend pool
    pub fn new(pool_id: &str) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::all_chains();
        Self {
            state: AppState {
                blend_provider: provider,
                aave_provider,
                monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
                db: None,
            },
        }
    }

    /// Create new tools instance with custom RPC manager
    pub fn with_rpc_manager(pool_id: &str, rpc: RpcManager) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::new(rpc, vec![]);
        Self {
            state: AppState {
                blend_provider: provider,
                aave_provider,
                monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
                db: None,
            },
        }
    }

    /// Create new tools instance with custom RPC manager and database
    pub fn with_rpc_manager_and_db(pool_id: &str, rpc: RpcManager, db: Database) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::new(rpc, vec![]);
        Self {
            state: AppState {
                blend_provider: provider,
                aave_provider,
                monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
                db: Some(db),
            },
        }
    }

    /// Fetch Aave rates with cache support
    async fn fetch_aave_rates_with_cache(
        &self,
        chain: &str,
        use_cache: bool,
    ) -> Result<PoolRates, anyhow::Error> {
        // Try cache first (unless use_cache is false)
        if use_cache {
            if let Some(ref db) = self.state.db {
                if let Ok(Some(cached_json)) = db.get_cached_rates(chain, DEFAULT_CACHE_TTL).await {
                    if let Ok(rates) = serde_json::from_str::<PoolRates>(&cached_json) {
                        tracing::debug!(chain = chain, "Returning cached rates");
                        return Ok(rates);
                    }
                }
            }
        }

        // Fetch fresh data
        let rates = self.state.aave_provider.fetch_chain_rates(chain).await?;

        // Store in cache (fire and forget)
        if let Some(ref db) = self.state.db {
            if let Ok(json) = serde_json::to_string(&rates) {
                let db = db.clone();
                let chain = chain.to_string();
                tokio::spawn(async move {
                    if let Err(e) = db.set_cached_rates(&chain, &json).await {
                        tracing::warn!(error = %e, "Failed to cache rates");
                    }
                });
            }
        }

        Ok(rates)
    }

    /// Apply filters to a list of asset rates
    fn apply_filters(assets: Vec<AssetRate>, params: &QueryRatesParams) -> Vec<AssetRate> {
        assets
            .into_iter()
            .filter(|a| {
                // Asset filter
                if let Some(ref filter) = params.asset {
                    if !a.asset_name.to_uppercase().contains(&filter.to_uppercase()) {
                        return false;
                    }
                }

                // Rate filters
                if let Some(min) = params.min_supply_apy {
                    if a.supply_apy < min {
                        return false;
                    }
                }
                if let Some(max) = params.max_supply_apy {
                    if a.supply_apy > max {
                        return false;
                    }
                }
                if let Some(min) = params.min_borrow_apy {
                    if a.borrow_apy < min {
                        return false;
                    }
                }
                if let Some(max) = params.max_borrow_apy {
                    if a.borrow_apy > max {
                        return false;
                    }
                }
                if let Some(min) = params.min_utilization {
                    if a.utilization < min {
                        return false;
                    }
                }
                if let Some(max) = params.max_utilization {
                    if a.utilization > max {
                        return false;
                    }
                }

                true
            })
            .collect()
    }
}

#[tool_router(server_handler)]
impl ApyMcpTools {
    #[tool(description = "DeFi lending rate query tool. Actions:\n\
        - \"query\" (default): Query rates with filters (chain, asset, protocol, APY range, utilization)\n\
        - \"add\": Add a pool to monitoring (requires chain + pool_id)\n\
        - \"list\": List all monitored pools\n\
        Supports Aave V3 (EVM) and Blend (Stellar). All parameters are optional for query.\n\
        Data is cached for 60 seconds by default. Set use_cache=false to force fresh data.")]
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
            "list" => self.handle_list_pools().await,
            _ => self.handle_query_rates(&params).await,
        }
    }
}

impl ApyMcpTools {
    /// Handle "add" action - add a pool to monitoring
    async fn handle_add_pool(&self, params: &QueryRatesParams) -> String {
        let chain = match &params.chain {
            Some(c) => c,
            None => return r#"{"error": "chain is required for add action"}"#.to_string(),
        };
        let pool_id = match &params.pool_id {
            Some(p) => p,
            None => return r#"{"error": "pool_id is required for add action"}"#.to_string(),
        };

        match chain.as_str() {
            "stellar" => {
                let mut pools = self.state.monitored_pools.write().await;
                if pools.contains(pool_id) {
                    format!(
                        r#"{{"success": true, "message": "Pool {} is already being monitored"}}"#,
                        pool_id
                    )
                } else {
                    pools.push(pool_id.clone());
                    format!(
                        r#"{{"success": true, "message": "Added pool {} to monitoring list"}}"#,
                        pool_id
                    )
                }
            }
            "ethereum" | "polygon" | "arbitrum" | "optimism" | "avalanche" | "base" | "gnosis"
            | "bnb" | "scroll" | "zksync" | "sonic" => {
                let mut pools = self.state.monitored_pools.write().await;
                let chain_key = format!("aave:{}", chain);
                if pools.contains(&chain_key) {
                    format!(
                        r#"{{"success": true, "message": "Aave on {} is already being monitored"}}"#,
                        chain
                    )
                } else {
                    pools.push(chain_key);
                    format!(
                        r#"{{"success": true, "message": "Added Aave on {} to monitoring list"}}"#,
                        chain
                    )
                }
            }
            _ => format!(
                r#"{{"success": false, "message": "Chain '{}' is not yet supported. Currently supported: stellar, ethereum, polygon, arbitrum, optimism, avalanche, base, gnosis, bnb, scroll, zksync, sonic"}}"#,
                chain
            ),
        }
    }

    /// Handle "list" action - list all monitored pools
    async fn handle_list_pools(&self) -> String {
        let pools = self.state.monitored_pools.read().await;
        serde_json::to_string_pretty(&*pools)
            .unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e))
    }

    /// Handle "query" action - query rates
    async fn handle_query_rates(&self, params: &QueryRatesParams) -> String {
        let protocol = params.protocol.as_deref().unwrap_or("all");
        let mut all_results = Vec::new();

        // ── Aave V3 queries ─────────────────────────────────────────
        if protocol == "all" || protocol == "aave_v3" {
            let chains = match &params.chain {
                Some(chain) => vec![chain.clone()],
                None => match self.state.aave_provider.list_pools().await {
                    Ok(chains) => chains,
                    Err(e) => return format!("{{\"error\": \"{}\"}}", e),
                },
            };

            // Query all chains concurrently
            let futures: Vec<_> = chains.into_iter().map(|chain| {
                let tools = self.clone();
                let use_cache = params.use_cache;
                let asset_filter = params.asset.clone();
                let p = QueryRatesParams {
                    action: "query".to_string(),
                    chain: None, // already resolved
                    asset: asset_filter,
                    protocol: None,
                    pool_id: None,
                    min_supply_apy: params.min_supply_apy,
                    max_supply_apy: params.max_supply_apy,
                    min_borrow_apy: params.min_borrow_apy,
                    max_borrow_apy: params.max_borrow_apy,
                    min_utilization: params.min_utilization,
                    max_utilization: params.max_utilization,
                    use_cache,
                };
                async move {
                    match tools.fetch_aave_rates_with_cache(&chain, use_cache).await {
                        Ok(mut rates) => {
                            // Apply filters to assets
                            rates.assets = Self::apply_filters(rates.assets, &p);
                            if !rates.assets.is_empty() {
                                Some(rates)
                            } else {
                                None
                            }
                        }
                        Err(e) => {
                            tracing::warn!(chain = %chain, error = %e, "Failed to fetch Aave rates");
                            None
                        }
                    }
                }
            }).collect();

            let results = join_all(futures).await;
            all_results.extend(results.into_iter().flatten());
        }

        // ── Blend queries ───────────────────────────────────────────
        if protocol == "all" || protocol == "blend" {
            let pools: Vec<String> = if let Some(ref pool_id) = params.pool_id {
                vec![pool_id.clone()]
            } else {
                let monitored = self.state.monitored_pools.read().await;
                monitored
                    .iter()
                    .filter(|p| !p.starts_with("aave:"))
                    .cloned()
                    .collect()
            };

            let futures: Vec<_> = pools.into_iter().map(|pool_id| {
                let provider = self.state.blend_provider.clone();
                async move {
                    match provider.get_pool_rates(&pool_id).await {
                        Ok(rates) => Some(rates),
                        Err(e) => {
                            tracing::warn!(pool_id = %pool_id, error = %e, "Failed to fetch Blend rates");
                            None
                        }
                    }
                }
            }).collect();

            let results = join_all(futures).await;
            let mut blend_results: Vec<_> = results.into_iter().flatten().collect();

            // Apply filters to Blend results
            for rates in &mut blend_results {
                rates.assets = Self::apply_filters(rates.assets.clone(), params);
            }
            blend_results.retain(|r| !r.assets.is_empty());

            all_results.extend(blend_results);
        }

        if all_results.is_empty() {
            return "{{\"error\": \"No results found matching the query parameters\"}}".to_string();
        }

        let response = AllRatesResponse {
            pools: all_results,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        };

        serde_json::to_string_pretty(&response)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e))
    }
}
