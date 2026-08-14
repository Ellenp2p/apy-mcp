//! Spark Savings Data API client.
//!
//! Spark Savings Vaults V2 are ERC-4626 yield-bearing vaults (spUSDC, spUSDT)
//! that accrue a continuous savings rate. The official read-only API exposes
//! current and historic savings data:
//! <https://api.spark.fi/v1/savings/{protocol}/{chain}/{token}>

use anyhow::{Context, Result};
use serde::Deserialize;

/// Base URL of the Spark Savings Data API
pub const SPARK_SAVINGS_API_URL: &str = "https://api.spark.fi";

/// Vault / underlying asset token info
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub address: String,
    pub decimals: u32,
    pub symbol: String,
    pub name: String,
}

/// Current savings data for a vault
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavingsData {
    /// Share token received by depositors (e.g. spUSDC)
    pub vault: TokenInfo,
    /// Underlying asset deposited (e.g. USDC)
    pub asset: TokenInfo,
    /// Annual percentage yield as a decimal fraction (0.0365 = 3.65%)
    pub apy: String,
    /// Total value locked in units of the underlying asset
    pub tvl: String,
    /// Number of unique depositors
    pub users: u64,
    /// Maximum total deposits allowed
    pub deposit_cap: String,
}

/// Envelope returned by the API
#[derive(Debug, Clone, Deserialize)]
pub struct SavingsApiResponse {
    pub data: SavingsData,
}

/// Fetch the current savings rate for a vault.
///
/// `protocol` is `spark` or `sky`, `chain` is `mainnet`,
/// `token` is the underlying asset symbol (`usdc`, `usdt`).
pub async fn fetch_savings_rate(
    protocol: &str,
    chain: &str,
    token: &str,
) -> Result<SavingsData> {
    let url = format!("{SPARK_SAVINGS_API_URL}/v1/savings/{protocol}/{chain}/{token}");
    let client = reqwest::Client::new();
    let resp: SavingsApiResponse = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .context("Failed to fetch Spark savings rate")?
        .json()
        .await
        .context("Failed to parse Spark savings response")?;
    Ok(resp.data)
}
