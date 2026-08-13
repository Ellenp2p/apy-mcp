pub mod aave;
pub mod interest;
pub mod providers;
pub mod rpc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use tracing::info;

use crate::chains::LendingProvider;
use crate::mcp::types::{AssetRate, PoolRates};

use self::rpc::RpcManager;

/// RAY = 10^27 (Aave's fixed-point precision)
const RAY: f64 = 1e27;

/// Maximum concurrent RPC calls for fetching reserve data
const MAX_CONCURRENT_RPC: usize = 8;

/// Aave V3 provider for EVM chains using AaveProtocolDataProvider
#[derive(Clone)]
pub struct AaveProvider {
    rpc: RpcManager,
    /// Chains to query (empty = all supported chains)
    chains: Vec<String>,
}

impl AaveProvider {
    pub fn new(rpc: RpcManager, chains: Vec<String>) -> Self {
        Self { rpc, chains }
    }

    /// Create a provider with default RPC URLs
    pub fn default_with_chains(chains: Vec<String>) -> Self {
        Self::new(RpcManager::new(), chains)
    }

    /// Create a provider for all chains
    pub fn all_chains() -> Self {
        Self::new(RpcManager::new(), vec![])
    }

    /// Get a reference to the underlying RPC manager
    pub fn get_rpc_manager(&self) -> &RpcManager {
        &self.rpc
    }

    /// Fetch rates for a specific chain using AaveProtocolDataProvider (with concurrency)
    pub async fn fetch_chain_rates(&self, chain_name: &str) -> Result<PoolRates> {
        let config = self
            .rpc
            .get_chain(chain_name)
            .await
            .context(format!("Chain '{}' not found", chain_name))?;

        let provider = config.aave_data_provider;
        info!(chain = chain_name, provider = ?provider, "Fetching Aave V3 rates from DataProvider");

        // Step 1: Get all reserve tokens (symbol + address)
        let tokens = aave::get_all_reserves_tokens(&self.rpc, chain_name, provider).await?;
        info!(
            chain = chain_name,
            count = tokens.len(),
            "Got reserve tokens"
        );

        // Step 2: Get reserve data for ALL tokens concurrently (with bounded concurrency)
        let mut all_results = Vec::with_capacity(tokens.len());

        // Process in batches of MAX_CONCURRENT_RPC
        for chunk in tokens.chunks(MAX_CONCURRENT_RPC) {
            let chunk_futures: Vec<_> = chunk.iter().map(|token| {
                let rpc = self.rpc.clone();
                let chain = chain_name.to_string();
                let addr = token.address;
                let symbol = token.symbol.clone();
                async move {
                    match aave::get_reserve_data(&rpc, &chain, provider, addr).await {
                        Ok(data) => Some((symbol, addr, data)),
                        Err(e) => {
                            tracing::warn!(chain = chain, asset = %symbol, error = %e, "Failed to fetch reserve data");
                            None
                        }
                    }
                }
            }).collect();

            let results = join_all(chunk_futures).await;
            all_results.extend(results);
        }

        // Step 3: Process results
        let mut assets = Vec::new();
        for result in all_results {
            let (symbol, addr, reserve) = match result {
                Some((s, a, d)) => (s, a, d),
                None => continue,
            };

            // Skip tokens with zero supply
            if reserve.total_a_token == 0 {
                continue;
            }

            let total_supply = reserve.total_a_token as f64;
            let total_borrow = (reserve.total_stable_debt + reserve.total_variable_debt) as f64;
            let utilization = if total_supply > 0.0 {
                (total_borrow / total_supply).min(1.0)
            } else {
                0.0
            };

            let supply_apr = reserve.liquidity_rate as f64 / RAY;
            let borrow_apr = reserve.variable_borrow_rate as f64 / RAY;
            let supply_apy = supply_apr.exp() - 1.0;
            let borrow_apy = borrow_apr.exp() - 1.0;

            let decimals = estimate_decimals(&symbol);
            let decimal_adjustment = 10f64.powi(decimals as i32);
            let total_supplied = total_supply / decimal_adjustment;
            let total_borrowed = total_borrow / decimal_adjustment;

            assets.push(AssetRate {
                asset_id: format!("0x{}", hex::encode(addr.as_bytes())),
                asset_name: symbol,
                decimals: decimals as u32,
                supply_apr,
                supply_apy,
                borrow_apr,
                borrow_apy,
                utilization,
                total_supplied,
                total_borrowed,
            });
        }

        info!(
            chain = chain_name,
            assets = assets.len(),
            "Fetched Aave V3 rates"
        );

        Ok(PoolRates {
            chain: chain_name.to_string(),
            protocol: "aave_v3".to_string(),
            pool_id: format!("0x{}", hex::encode(provider.as_bytes())),
            pool_name: format!("Aave V3 {}", capitalize(chain_name)),
            timestamp: chrono::Utc::now().to_rfc3339(),
            assets,
        })
    }
}

#[async_trait]
impl LendingProvider for AaveProvider {
    fn chain_name(&self) -> &str {
        "evm"
    }

    fn protocol_name(&self) -> &str {
        "aave_v3"
    }

    async fn get_pool_rates(&self, pool_id: &str) -> Result<PoolRates> {
        // pool_id is the chain name for Aave
        self.fetch_chain_rates(pool_id).await
    }

    async fn list_pools(&self) -> Result<Vec<String>> {
        if self.chains.is_empty() {
            Ok(self
                .rpc
                .list_chains()
                .await
                .into_iter()
                .map(|c| c.name)
                .collect())
        } else {
            Ok(self.chains.clone())
        }
    }
}

/// Capitalize first letter
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

/// Estimate decimals from token symbol (common tokens)
fn estimate_decimals(symbol: &str) -> u8 {
    match symbol.to_uppercase().as_str() {
        "USDC" | "USDT" | "DAI" | "FRAX" | "LUSD" | "GHO" | "SUSD" | "MAI" | "EURS" => 6,
        "WBTC" | "CBTC" => 8,
        "WETH" | "WSTETH" | "RETH" | "STETH" | "LINK" | "AAVE" | "UNI" | "OP" | "MKR" => 18,
        _ => 18, // default to 18 decimals
    }
}
