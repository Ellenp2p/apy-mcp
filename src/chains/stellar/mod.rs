mod blend;
mod interest;
mod rpc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::chains::LendingProvider;
use crate::mcp::types::PoolRates;

/// Soroban RPC endpoint for Stellar mainnet
pub const SOROBAN_RPC_URL: &str = "https://mainnet.sorobanrpc.com";

/// Blend Capital pool provider for Stellar
#[derive(Clone)]
pub struct BlendProvider {
    rpc_url: String,
    /// List of pool contract addresses to monitor
    pools: Vec<String>,
}

impl BlendProvider {
    pub fn new(rpc_url: Option<String>, pools: Vec<String>) -> Self {
        Self {
            rpc_url: rpc_url.unwrap_or_else(|| SOROBAN_RPC_URL.to_string()),
            pools,
        }
    }

    /// Create a provider with the default Blend pool from the user's request
    pub fn default_with_pool(pool_id: &str) -> Self {
        Self::new(None, vec![pool_id.to_string()])
    }
}

#[async_trait]
impl LendingProvider for BlendProvider {
    fn chain_name(&self) -> &str {
        "stellar"
    }

    fn protocol_name(&self) -> &str {
        "blend"
    }

    async fn get_pool_rates(&self, pool_id: &str) -> Result<PoolRates> {
        info!(pool_id = pool_id, "Fetching Blend pool rates");
        blend::fetch_pool_rates(&self.rpc_url, pool_id).await
    }

    async fn list_pools(&self) -> Result<Vec<String>> {
        Ok(self.pools.clone())
    }
}
