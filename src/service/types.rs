use serde::{Deserialize, Serialize};

/// A single asset's lending/borrowing rates in a pool
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Unified rate query parameters
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
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

    /// Filter by protocol: "aave_v3", "blend", "spark" (Spark Savings), or "all" (default).
    /// "all" queries all supported protocols.
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

    /// Whether to use cached data (default: true, cache TTL is 120 seconds).
    /// Set to false to force fresh data from chain.
    #[serde(default = "default_true")]
    pub use_cache: bool,

    /// Internal: only serve data already in the SQLite cache, never hit the
    /// network. Used by SSR so the index page never blocks on slow RPCs.
    /// Not deserializable from API/MCP requests (always false there).
    #[serde(skip)]
    pub cache_only: bool,
}

fn default_action() -> String {
    "query".to_string()
}

fn default_true() -> bool {
    true
}
