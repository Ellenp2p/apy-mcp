pub mod evm;
pub mod stellar;

#[allow(dead_code)]
use anyhow::Result;
use async_trait::async_trait;

use crate::mcp::types::PoolRates;

/// Trait for chain-specific lending protocol providers
#[async_trait]
pub trait LendingProvider: Send + Sync {
    /// Chain identifier (e.g., "stellar", "sui", "evm")
    fn chain_name(&self) -> &str;

    /// Protocol identifier (e.g., "blend", "aave", "compound")
    fn protocol_name(&self) -> &str;

    /// Fetch current rates for a specific pool
    async fn get_pool_rates(&self, pool_id: &str) -> Result<PoolRates>;

    /// List all configured pools
    async fn list_pools(&self) -> Result<Vec<String>>;
}
