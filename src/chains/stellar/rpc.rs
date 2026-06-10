use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::debug;

/// A single ledger entry returned by getLedgerEntries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntryResult {
    pub key: String,
    pub xdr: String,
    #[serde(rename = "lastModifiedLedgerSeq")]
    pub last_modified_ledger_seq: Option<serde_json::Value>,
    #[serde(rename = "liveUntilLedgerSeq")]
    pub live_until_ledger_seq: Option<serde_json::Value>,
    #[serde(rename = "extXdr")]
    pub ext_xdr: Option<String>,
}

/// Response from getLedgerEntries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetLedgerEntriesResponse {
    pub entries: Vec<LedgerEntryResult>,
    #[serde(rename = "latestLedger")]
    pub latest_ledger: u64,
}

/// Response from getLatestLedger
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestLedgerResponse {
    pub id: String,
    pub sequence: u64,
}

/// Soroban RPC client
#[derive(Clone)]
pub struct SorobanRpc {
    client: Client,
    rpc_url: String,
}

impl SorobanRpc {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: Client::new(),
            rpc_url: rpc_url.to_string(),
        }
    }

    /// Make a JSON-RPC call to Soroban RPC
    async fn call_rpc<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        debug!(method = method, "Calling Soroban RPC");

        let resp = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .context("Failed to send RPC request")?;

        let status = resp.status();
        let text = resp.text().await.context("Failed to read RPC response")?;

        if !status.is_success() {
            anyhow::bail!("RPC request failed with status {}: {}", status, text);
        }

        let rpc_resp: Value =
            serde_json::from_str(&text).context("Failed to parse RPC response as JSON")?;

        if let Some(error) = rpc_resp.get("error") {
            anyhow::bail!("RPC error: {}", error);
        }

        let result = rpc_resp
            .get("result")
            .context("RPC response missing 'result' field")?;

        serde_json::from_value(result.clone()).map_err(|e| {
            tracing::debug!(result = %result, "RPC result that failed to deserialize");
            anyhow::anyhow!(
                "Failed to deserialize RPC result: {} (result was: {})",
                e,
                result
            )
        })
    }

    /// Fetch ledger entries by their XDR-encoded keys
    /// Keys should be base64-encoded XDR LedgerKey values
    pub async fn get_ledger_entries(&self, keys: &[String]) -> Result<GetLedgerEntriesResponse> {
        let params = json!({ "keys": keys });
        self.call_rpc("getLedgerEntries", params).await
    }

    /// Get the latest ledger sequence number
    pub async fn get_latest_ledger(&self) -> Result<LatestLedgerResponse> {
        self.call_rpc("getLatestLedger", json!({})).await
    }

    /// Simulate a transaction (for contract invocations)
    pub async fn simulate_transaction(&self, transaction_envelope: &str) -> Result<Value> {
        let params = json!({ "transaction": transaction_envelope });
        self.call_rpc("simulateTransaction", params).await
    }
}
