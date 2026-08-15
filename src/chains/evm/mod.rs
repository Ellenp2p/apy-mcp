pub mod aave;
pub mod interest;
pub mod providers;
pub mod rpc;
pub mod savings;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use primitive_types::H160;
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

    /// Fetch rates for a specific chain using AaveProtocolDataProvider
    pub async fn fetch_chain_rates(&self, chain_name: &str) -> Result<PoolRates> {
        let config = self
            .rpc
            .get_chain(chain_name)
            .await
            .context(format!("Chain '{}' not found", chain_name))?;
        fetch_protocol_rates(
            &self.rpc,
            chain_name,
            config.aave_data_provider,
            "aave_v3",
            &format!("Aave V3 {}", capitalize(chain_name)),
        )
        .await
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

/// A Spark Savings vault to monitor
#[derive(Debug, Clone)]
struct SavingsVault {
    protocol: String,
    chain: String,
    token: String,
}

/// Spark Savings (Savings Vaults V2) provider backed by the official
/// Savings Data API (api.spark.fi). Vaults are ERC-4626 savings vaults such
/// as spUSDC / spUSDT; the reported APY already reflects the latest block.
#[derive(Clone)]
pub struct SparkSavingsProvider {
    vaults: Vec<SavingsVault>,
}

impl SparkSavingsProvider {
    /// Provider monitoring all currently exposed Spark Savings vaults
    pub fn all_vaults() -> Self {
        Self {
            vaults: vec![
                SavingsVault {
                    protocol: "spark".to_string(),
                    chain: "mainnet".to_string(),
                    token: "usdc".to_string(),
                },
                SavingsVault {
                    protocol: "spark".to_string(),
                    chain: "mainnet".to_string(),
                    token: "usdt".to_string(),
                },
            ],
        }
    }

    /// Fetch current savings data for one vault (identified by token symbol)
    async fn fetch(&self, token: &str) -> Result<PoolRates> {
        let token = token.to_lowercase();
        let vault = self
            .vaults
            .iter()
            .find(|v| v.token == token)
            .ok_or_else(|| anyhow::anyhow!("no savings vault for token '{}'", token))?;

        let data = savings::fetch_savings_rate(&vault.protocol, &vault.chain, &vault.token).await?;

        let apy: f64 = data
            .apy
            .parse()
            .context(format!("invalid 'apy' in Spark savings response: '{}'", data.apy))?;
        let tvl: f64 = data
            .tvl
            .parse()
            .context(format!("invalid 'tvl' in Spark savings response: '{}'", data.tvl))?;

        info!(
            asset = %data.asset.symbol,
            apy = format!("{:.2}%", apy * 100.0),
            tvl = format!("{:.0}", tvl),
            "Spark Savings rate"
        );

        // The API reports the effective annual yield (APY). Derive the nominal
        // APR so the pair stays consistent with the rest of the codebase,
        // where supply_apy = exp(supply_apr) - 1.
        let supply_apr = (1.0 + apy).ln();

        Ok(PoolRates {
            chain: "ethereum".to_string(),
            protocol: "spark".to_string(),
            pool_id: vault.token.clone(),
            pool_name: format!("Spark Savings {}", data.asset.symbol),
            timestamp: chrono::Utc::now().to_rfc3339(),
            assets: vec![AssetRate {
                asset_id: data.vault.address,
                asset_name: data.asset.symbol,
                decimals: data.asset.decimals,
                supply_apr,
                supply_apy: apy,
                borrow_apr: 0.0,
                borrow_apy: 0.0,
                utilization: 0.0,
                total_supplied: tvl,
                total_borrowed: 0.0,
            }],
        })
    }
}

#[async_trait]
impl LendingProvider for SparkSavingsProvider {
    fn chain_name(&self) -> &str {
        "evm"
    }

    fn protocol_name(&self) -> &str {
        "spark_savings"
    }

    async fn get_pool_rates(&self, pool_id: &str) -> Result<PoolRates> {
        self.fetch(pool_id).await
    }

    async fn list_pools(&self) -> Result<Vec<String>> {
        Ok(self.vaults.iter().map(|v| v.token.clone()).collect())
    }
}

/// Shared rate-fetching logic for any Aave-compatible protocol (Aave V3, Spark,
/// or other forks). Reads reserves via the protocol's data provider contract
/// (getAllReservesTokens + getReserveData) and applies the same interest math.
async fn fetch_protocol_rates(
    rpc: &RpcManager,
    chain_name: &str,
    data_provider: H160,
    protocol: &str,
    pool_name: &str,
) -> Result<PoolRates> {
    info!(chain = chain_name, provider = ?data_provider, protocol = protocol, "Fetching rates from DataProvider");

    // Step 1: Get all reserve tokens (symbol + address)
    let tokens = aave::get_all_reserves_tokens(rpc, chain_name, data_provider).await?;
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
            let rpc = rpc.clone();
            let chain = chain_name.to_string();
            let addr = token.address;
            let symbol = token.symbol.clone();
            async move {
                match aave::get_reserve_data(&rpc, &chain, data_provider, addr).await {
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
        protocol = protocol,
        assets = assets.len(),
        "Fetched rates"
    );

    Ok(PoolRates {
        chain: chain_name.to_string(),
        protocol: protocol.to_string(),
        pool_id: format!("0x{}", hex::encode(data_provider.as_bytes())),
        pool_name: pool_name.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        assets,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires live Spark Savings Data API"]
    async fn test_live_spark_savings() {
        let provider = SparkSavingsProvider::all_vaults();
        let pools = provider.list_pools().await.unwrap();
        assert_eq!(pools, vec!["usdc", "usdt"]);

        for pool in pools {
            let rates = provider.get_pool_rates(&pool).await.unwrap();
            assert_eq!(rates.protocol, "spark");
            assert_eq!(rates.assets.len(), 1);
            let asset = &rates.assets[0];
            assert!(asset.supply_apy > 0.0, "empty savings APY for {}", pool);
            assert!(asset.total_supplied > 0.0);
            println!(
                "Spark Savings {}: APY={:.2}% TVL={:.0} {}",
                rates.pool_name,
                asset.supply_apy * 100.0,
                asset.total_supplied,
                asset.asset_name
            );
        }
    }
}
