use serde::{Deserialize, Serialize};

/// A single asset's lending/borrowing rates in a pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRate {
    /// Asset identifier (contract address or symbol)
    pub asset_id: String,
    /// Human-readable asset name/symbol
    pub asset_name: String,
    /// Number of decimals for the asset
    pub decimals: u32,
    /// Supply APR (annual percentage rate)
    pub supply_apr: f64,
    /// Supply APY (annual percentage yield, compounded)
    pub supply_apy: f64,
    /// Borrow APR
    pub borrow_apr: f64,
    /// Borrow APY
    pub borrow_apy: f64,
    /// Current utilization rate (0.0 - 1.0)
    pub utilization: f64,
    /// Total supplied (in human-readable units)
    pub total_supplied: f64,
    /// Total borrowed (in human-readable units)
    pub total_borrowed: f64,
}

/// Pool rates response containing all reserves
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRates {
    /// Chain name (stellar, sui, evm, etc.)
    pub chain: String,
    /// Protocol name (blend, aave, etc.)
    pub protocol: String,
    /// Pool contract address
    pub pool_id: String,
    /// Pool human-readable name
    pub pool_name: String,
    /// Timestamp of the data (ISO 8601)
    pub timestamp: String,
    /// Per-asset rates
    pub assets: Vec<AssetRate>,
}

/// Overview of all monitored pools across all chains
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllRatesResponse {
    pub pools: Vec<PoolRates>,
    pub fetched_at: String,
}

/// Status response for management operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub success: bool,
    pub message: String,
}
