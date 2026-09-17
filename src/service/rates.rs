use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::chains::evm::rpc::RpcManager;
use crate::chains::evm::{AaveProvider, SparkSavingsProvider};
use crate::chains::stellar::BlendProvider;
use crate::chains::LendingProvider;
use crate::db::Database;
use crate::service::types::{AllRatesResponse, AssetRate, PoolRates, QueryRatesParams, StatusResponse};

/// Default cache TTL in seconds
/// Default cache TTL in seconds (2 minutes — the web UI polls the API and
/// upstream sources like Spark refresh ~every 5 min anyway)
const DEFAULT_CACHE_TTL: i64 = 120;

/// Protocol-agnostic rate query service shared by the MCP tool and the HTTP API.
#[derive(Clone)]
pub struct RateService {
    pub blend_provider: BlendProvider,
    pub aave_provider: AaveProvider,
    pub savings_provider: SparkSavingsProvider,
    pub monitored_pools: Arc<RwLock<Vec<String>>>,
    pub db: Option<Database>,
}

impl RateService {
    /// Create new service instance with a default Blend pool
    pub fn new(pool_id: &str) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::all_chains();
        let savings_provider = SparkSavingsProvider::all_vaults();
        Self {
            blend_provider: provider,
            aave_provider,
            savings_provider,
            monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
            db: None,
        }
    }

    /// Create new service instance with custom RPC manager
    pub fn with_rpc_manager(pool_id: &str, rpc: RpcManager) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::new(rpc, vec![]);
        let savings_provider = SparkSavingsProvider::all_vaults();
        Self {
            blend_provider: provider,
            aave_provider,
            savings_provider,
            monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
            db: None,
        }
    }

    /// Create new service instance with custom RPC manager and database
    pub fn with_rpc_manager_and_db(pool_id: &str, rpc: RpcManager, db: Database) -> Self {
        let provider = BlendProvider::default_with_pool(pool_id);
        let aave_provider = AaveProvider::new(rpc, vec![]);
        let savings_provider = SparkSavingsProvider::all_vaults();
        Self {
            blend_provider: provider,
            aave_provider,
            savings_provider,
            monitored_pools: Arc::new(RwLock::new(vec![pool_id.to_string()])),
            db: Some(db),
        }
    }

    /// Fetch Aave rates with cache support
    async fn fetch_aave_rates_with_cache(
        &self,
        chain: &str,
        use_cache: bool,
        cache_only: bool,
    ) -> Result<PoolRates, anyhow::Error> {
        // Try cache first (unless use_cache is false)
        if use_cache {
            if let Some(ref db) = self.db {
                if let Ok(Some(cached_json)) = db.get_cached_rates(chain, DEFAULT_CACHE_TTL).await {
                    if let Ok(rates) = serde_json::from_str::<PoolRates>(&cached_json) {
                        tracing::debug!(chain = chain, "Returning cached rates");
                        return Ok(rates);
                    }
                }
            }
        }

        if cache_only {
            anyhow::bail!("chain '{}' not in cache", chain);
        }

        // Fetch fresh data
        let rates = self.aave_provider.fetch_chain_rates(chain).await?;

        // Store in cache (fire and forget)
        if let Some(ref db) = self.db {
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

    /// Fetch Spark Savings rates with cache support
    async fn fetch_savings_with_cache(
        &self,
        token: &str,
        use_cache: bool,
        cache_only: bool,
    ) -> Result<PoolRates, anyhow::Error> {
        let cache_key = format!("spark_savings:{}", token.to_lowercase());
        // Try cache first (unless use_cache is false)
        if use_cache {
            if let Some(ref db) = self.db {
                if let Ok(Some(cached_json)) =
                    db.get_cached_rates(&cache_key, DEFAULT_CACHE_TTL).await
                {
                    if let Ok(rates) = serde_json::from_str::<PoolRates>(&cached_json) {
                        tracing::debug!(token = token, "Returning cached Spark Savings rate");
                        return Ok(rates);
                    }
                }
            }
        }

        if cache_only {
            anyhow::bail!("Spark vault '{}' not in cache", token);
        }

        // Fetch fresh data
        let rates = self.savings_provider.get_pool_rates(token).await?;

        // Store in cache (fire and forget)
        if let Some(ref db) = self.db {
            if let Ok(json) = serde_json::to_string(&rates) {
                let db = db.clone();
                let key = cache_key.clone();
                tokio::spawn(async move {
                    if let Err(e) = db.set_cached_rates(&key, &json).await {
                        tracing::warn!(error = %e, "Failed to cache Spark Savings rate");
                    }
                });
            }
        }

        Ok(rates)
    }

    /// Fetch Blend (Stellar) pool rates with cache support
    async fn fetch_blend_with_cache(
        &self,
        pool_id: &str,
        use_cache: bool,
        cache_only: bool,
    ) -> Result<PoolRates, anyhow::Error> {
        let cache_key = format!("blend:{}", pool_id);
        if use_cache {
            if let Some(ref db) = self.db {
                if let Ok(Some(cached_json)) =
                    db.get_cached_rates(&cache_key, DEFAULT_CACHE_TTL).await
                {
                    if let Ok(rates) = serde_json::from_str::<PoolRates>(&cached_json) {
                        tracing::debug!(pool_id = pool_id, "Returning cached Blend rate");
                        return Ok(rates);
                    }
                }
            }
        }

        if cache_only {
            anyhow::bail!("Blend pool '{}' not in cache", pool_id);
        }

        let rates = self.blend_provider.get_pool_rates(pool_id).await?;

        if let Some(ref db) = self.db {
            if let Ok(json) = serde_json::to_string(&rates) {
                let db = db.clone();
                let key = cache_key.clone();
                tokio::spawn(async move {
                    if let Err(e) = db.set_cached_rates(&key, &json).await {
                        tracing::warn!(error = %e, "Failed to cache Blend rate");
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

    /// Query rates across supported protocols, applying filters.
    pub async fn query(&self, params: &QueryRatesParams) -> anyhow::Result<AllRatesResponse> {
        let started = std::time::Instant::now();
        let protocol = params.protocol.as_deref().unwrap_or("all");
        let mut all_results = Vec::new();

        // ── Aave V3 queries ─────────────────────────────────────────
        if protocol == "all" || protocol == "aave_v3" {
            let chains = match &params.chain {
                Some(chain) => vec![chain.clone()],
                None => self.aave_provider.list_pools().await?,
            };

            // Query all chains concurrently
            let futures: Vec<_> = chains.into_iter().map(|chain| {
                let service = self.clone();
                let use_cache = params.use_cache;
                let cache_only = params.cache_only;
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
                    cache_only: params.cache_only,
                };
                async move {
                    match service
                        .fetch_aave_rates_with_cache(&chain, use_cache, cache_only)
                        .await
                    {
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
                            // Cache-only misses (SSR) are routine, not warnings
                            if cache_only {
                                tracing::debug!(chain = %chain, error = %e, "Chain not in cache (SSR)");
                            } else {
                                tracing::warn!(chain = %chain, error = %e, "Failed to fetch Aave rates");
                            }
                            None
                        }
                    }
                }
            }).collect();

            let results = join_all(futures).await;
            all_results.extend(results.into_iter().flatten());
        }

        // ── Spark Savings queries ("spark" = Spark Savings) ─────────
        if protocol == "all" || protocol == "spark" {
            // Spark Savings vaults live on Ethereum mainnet
            let chain_ok = match &params.chain {
                Some(c) => matches!(c.as_str(), "ethereum" | "mainnet" | "evm"),
                None => true,
            };

            if chain_ok {
                let vaults: Vec<String> = if let Some(ref pool_id) = params.pool_id {
                    vec![pool_id.clone()]
                } else {
                    self.savings_provider
                        .list_pools()
                        .await
                        .unwrap_or_default()
                };

                let futures: Vec<_> = vaults.into_iter().map(|vault| {
                    let service = self.clone();
                    let use_cache = params.use_cache;
                    let cache_only = params.cache_only;
                    let p = QueryRatesParams {
                        action: "query".to_string(),
                        chain: None, // already resolved
                        asset: params.asset.clone(),
                        protocol: None,
                        pool_id: None,
                        min_supply_apy: params.min_supply_apy,
                        max_supply_apy: params.max_supply_apy,
                        min_borrow_apy: params.min_borrow_apy,
                        max_borrow_apy: params.max_borrow_apy,
                        min_utilization: params.min_utilization,
                        max_utilization: params.max_utilization,
                        use_cache,
                        cache_only: params.cache_only,
                    };
                    async move {
                        match service.fetch_savings_with_cache(&vault, use_cache, cache_only).await {
                            Ok(mut rates) => {
                                rates.assets = Self::apply_filters(rates.assets, &p);
                                if !rates.assets.is_empty() {
                                    Some(rates)
                                } else {
                                    None
                                }
                            }
                            Err(e) => {
                                if cache_only {
                                    tracing::debug!(vault = %vault, error = %e, "Spark vault not in cache (SSR)");
                                } else {
                                    tracing::warn!(vault = %vault, error = %e, "Failed to fetch Spark Savings rate");
                                }
                                None
                            }
                        }
                    }
                }).collect();

                let results = join_all(futures).await;
                all_results.extend(results.into_iter().flatten());
            }
        }

        // ── Blend queries ───────────────────────────────────────────
        if protocol == "all" || protocol == "blend" {
            let pools: Vec<String> = if let Some(ref pool_id) = params.pool_id {
                vec![pool_id.clone()]
            } else {
                let monitored = self.monitored_pools.read().await;
                monitored
                    .iter()
                    .filter(|p| !p.starts_with("aave:") && !p.starts_with("spark:"))
                    .cloned()
                    .collect()
            };

            let futures: Vec<_> = pools.into_iter().map(|pool_id| {
                let service = self.clone();
                let cache_only = params.cache_only;
                async move {
                    match service.fetch_blend_with_cache(&pool_id, params.use_cache, cache_only).await {
                        Ok(rates) => Some(rates),
                        Err(e) => {
                            if cache_only {
                                tracing::debug!(pool_id = %pool_id, error = %e, "Blend pool not in cache (SSR)");
                            } else {
                                tracing::warn!(pool_id = %pool_id, error = %e, "Failed to fetch Blend rates");
                            }
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
            return Err(anyhow::anyhow!(
                "No results found matching the query parameters"
            ));
        }

        let pool_count = all_results.len();
        let asset_count: usize = all_results.iter().map(|p| p.assets.len()).sum();
        tracing::info!(
            protocol = protocol,
            chain = params.chain.as_deref().unwrap_or("all"),
            pools = pool_count,
            assets = asset_count,
            ms = started.elapsed().as_millis() as u64,
            "rate query completed"
        );

        Ok(AllRatesResponse {
            pools: all_results,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Add a pool to monitoring.
    pub async fn add_pool(&self, params: &QueryRatesParams) -> StatusResponse {
        let chain = match &params.chain {
            Some(c) => c.clone(),
            None => {
                return StatusResponse {
                    success: false,
                    message: "chain is required for add action".to_string(),
                }
            }
        };
        let pool_id = match &params.pool_id {
            Some(p) => p.clone(),
            None => {
                return StatusResponse {
                    success: false,
                    message: "pool_id is required for add action".to_string(),
                }
            }
        };

        match chain.as_str() {
            "stellar" => {
                let mut pools = self.monitored_pools.write().await;
                if pools.contains(&pool_id) {
                    StatusResponse {
                        success: true,
                        message: format!("Pool {} is already being monitored", pool_id),
                    }
                } else {
                    pools.push(pool_id.clone());
                    StatusResponse {
                        success: true,
                        message: format!("Added pool {} to monitoring list", pool_id),
                    }
                }
            }
            "ethereum" | "polygon" | "arbitrum" | "optimism" | "avalanche" | "base" | "gnosis"
            | "bnb" | "scroll" | "zksync" | "sonic" => {
                let protocol = params.protocol.as_deref().unwrap_or("aave_v3");
                // Only Aave uses the monitored_pools list for EVM
                if protocol == "spark" {
                    return StatusResponse {
                        success: false,
                        message: "Spark Savings vaults are fixed (usdc, usdt) and cannot be added via this action".to_string(),
                    };
                }
                let mut pools = self.monitored_pools.write().await;
                let chain_key = format!("aave:{}", chain);
                if pools.contains(&chain_key) {
                    StatusResponse {
                        success: true,
                        message: format!("Aave on {} is already being monitored", chain),
                    }
                } else {
                    pools.push(chain_key);
                    StatusResponse {
                        success: true,
                        message: format!("Added Aave on {} to monitoring list", chain),
                    }
                }
            }
            _ => StatusResponse {
                success: false,
                message: format!(
                    "Chain '{}' is not yet supported. Currently supported: stellar, ethereum, polygon, arbitrum, optimism, avalanche, base, gnosis, bnb, scroll, zksync, sonic",
                    chain
                ),
            },
        }
    }

    /// List all monitored pools.
    pub async fn list_pools(&self) -> Vec<String> {
        self.monitored_pools.read().await.clone()
    }
}
