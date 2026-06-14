use anyhow::{Context, Result};
use primitive_types::H160;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::providers;

/// Chain configuration with RPC URL
#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub name: String,
    pub rpc_url: String,
    /// AaveProtocolDataProvider contract address for this chain
    pub aave_data_provider: H160,
}

/// Per-chain provider assignment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainProviderAssignment {
    pub provider: String,
    pub api_key: Option<String>,
}

/// RPC health status for a chain
#[derive(Debug, Clone, Serialize)]
pub struct ChainHealthStatus {
    pub chain: String,
    pub rpc_url: String,
    pub provider: String,
    pub healthy: bool,
    pub block_number: Option<u64>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

/// Supported EVM chains with Aave V3 AaveProtocolDataProvider deployments
pub fn default_chain_configs() -> Vec<ChainConfig> {
    vec![
        ChainConfig {
            chain_id: 1,
            name: "ethereum".to_string(),
            rpc_url: "https://eth.llamarpc.com".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("0a16f2FCC0D44FaE41cc54e079281D84A363bECD").unwrap()),
        },
        ChainConfig {
            chain_id: 137,
            name: "polygon".to_string(),
            rpc_url: "https://polygon.llamarpc.com".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("243Aa95cAC2a25651eda86e80bEe66114413c43b").unwrap()),
        },
        ChainConfig {
            chain_id: 42161,
            name: "arbitrum".to_string(),
            rpc_url: "https://arb1.arbitrum.io/rpc".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("69FA688f1Dc47d4B5d8029D5a35FB7a548310654").unwrap()),
        },
        ChainConfig {
            chain_id: 10,
            name: "optimism".to_string(),
            rpc_url: "https://mainnet.optimism.io".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("69FA688f1Dc47d4B5d8029D5a35FB7a548310654").unwrap()),
        },
        ChainConfig {
            chain_id: 43114,
            name: "avalanche".to_string(),
            rpc_url: "https://api.avax.network/ext/bc/C/rpc".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("69FA688f1Dc47d4B5d8029D5a35FB7a548310654").unwrap()),
        },
        ChainConfig {
            chain_id: 8453,
            name: "base".to_string(),
            rpc_url: "https://mainnet.base.org".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("d82a47fdebce5b02a5a39c85d4af4f60b89f4544").unwrap()),
        },
        ChainConfig {
            chain_id: 100,
            name: "gnosis".to_string(),
            rpc_url: "https://rpc.gnosis.gateway.fm".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("501B4c19dd9C2e06E94dA7b6D5Ed4ddA013EC741").unwrap()),
        },
        ChainConfig {
            chain_id: 56,
            name: "bnb".to_string(),
            rpc_url: "https://bsc-dataseed.binance.org".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("43d6d4d6493d1d9A70e00B5bba9F76E4D33aE57E").unwrap()),
        },
        ChainConfig {
            chain_id: 534352,
            name: "scroll".to_string(),
            rpc_url: "https://rpc.scroll.io".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("DC3c19C892B90dB8B486F1Ba63e48Ee8b85F6aE8").unwrap()),
        },
        ChainConfig {
            chain_id: 324,
            name: "zksync".to_string(),
            rpc_url: "https://mainnet.era.zksync.io".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("E39Da74E2fDe81aA6829CC37B8cD51E1B209b0f2").unwrap()),
        },
        ChainConfig {
            chain_id: 146,
            name: "sonic".to_string(),
            rpc_url: "https://rpc.soniclabs.com".to_string(),
            aave_data_provider: H160::from_slice(&hex::decode("c0a344397cfa89dF1e1d3e4fb330834D789cF2CD").unwrap()),
        },
    ]
}

/// JSON-RPC request
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: Vec<serde_json::Value>,
}

/// JSON-RPC response
#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

/// JSON-RPC error
#[derive(Deserialize, Debug)]
struct JsonRpcError {
    message: String,
}

/// RPC client manager for multiple chains
#[derive(Clone)]
pub struct RpcManager {
    configs: Arc<RwLock<HashMap<String, ChainConfig>>>,
    /// Per-chain provider assignments (overrides default)
    chain_providers: Arc<RwLock<HashMap<String, ChainProviderAssignment>>>,
    /// Global default provider name + API key
    default_provider: Arc<RwLock<Option<(String, Option<String>)>>>,
    http_client: reqwest::Client,
}

impl RpcManager {
    pub fn new() -> Self {
        let mut configs = HashMap::new();
        for chain in default_chain_configs() {
            configs.insert(chain.name.clone(), chain);
        }
        Self {
            configs: Arc::new(RwLock::new(configs)),
            chain_providers: Arc::new(RwLock::new(HashMap::new())),
            default_provider: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
        }
    }

    /// Override RPC URL for a specific chain (highest priority)
    pub async fn set_rpc_url(&self, chain_name: &str, rpc_url: &str) {
        let mut configs = self.configs.write().await;
        if let Some(config) = configs.get_mut(chain_name) {
            config.rpc_url = rpc_url.to_string();
            tracing::info!(chain = chain_name, url = rpc_url, "Updated RPC URL (direct override)");
        }
    }

    /// Set the global default provider (applies to all chains unless overridden)
    pub async fn set_default_provider(&self, provider_name: &str, api_key: Option<String>) {
        let mut dp = self.default_provider.write().await;
        *dp = Some((provider_name.to_string(), api_key));
        tracing::info!(provider = provider_name, "Set global default EVM provider");
    }

    /// Set provider for a specific chain (overrides global default)
    pub async fn set_chain_provider(&self, chain_name: &str, provider_name: &str, api_key: Option<String>) {
        let mut cp = self.chain_providers.write().await;
        cp.insert(
            chain_name.to_string(),
            ChainProviderAssignment {
                provider: provider_name.to_string(),
                api_key,
            },
        );
        tracing::info!(chain = chain_name, provider = provider_name, "Set chain provider");
    }

    /// Apply provider templates to resolve final RPC URLs.
    /// Call this after all set_* methods, before serving requests.
    pub async fn apply_providers(&self) {
        let default = self.default_provider.read().await;
        let chain_overrides = self.chain_providers.read().await;
        let mut configs = self.configs.write().await;

        for (chain_name, config) in configs.iter_mut() {
            // 1. Check per-chain override
            let resolved = chain_overrides.get(chain_name).cloned().or_else(|| {
                default.as_ref().map(|(name, key)| {
                    ChainProviderAssignment {
                        provider: name.clone(),
                        api_key: key.clone(),
                    }
                })
            });

            if let Some(ref assignment) = resolved {
                if let Some(template) = providers::get_provider(&assignment.provider) {
                    if let Some(url) = template.build_url(chain_name, assignment.api_key.as_deref()) {
                        config.rpc_url = url;
                    }
                }
            }
        }
    }

    /// Get chain config by name
    pub async fn get_chain(&self, chain_name: &str) -> Option<ChainConfig> {
        let configs = self.configs.read().await;
        configs.get(chain_name).cloned()
    }

    /// Get all chain configs
    pub async fn list_chains(&self) -> Vec<ChainConfig> {
        let configs = self.configs.read().await;
        configs.values().cloned().collect()
    }

    /// Get current provider assignment for a chain
    pub async fn get_chain_provider(&self, chain_name: &str) -> Option<ChainProviderAssignment> {
        let cp = self.chain_providers.read().await;
        cp.get(chain_name).cloned()
    }

    /// Get the global default provider
    pub async fn get_default_provider(&self) -> Option<(String, Option<String>)> {
        let dp = self.default_provider.read().await;
        dp.clone()
    }

    /// Check health of a single chain's RPC (eth_blockNumber)
    pub async fn check_chain_health(&self, chain_name: &str) -> ChainHealthStatus {
        let config = match self.get_chain(chain_name).await {
            Some(c) => c,
            None => {
                return ChainHealthStatus {
                    chain: chain_name.to_string(),
                    rpc_url: String::new(),
                    provider: String::new(),
                    healthy: false,
                    block_number: None,
                    latency_ms: None,
                    error: Some("Chain not found".into()),
                };
            }
        };

        let provider_name = {
            let cp = self.chain_providers.read().await;
            let dp = self.default_provider.read().await;
            cp.get(chain_name)
                .map(|a| a.provider.clone())
                .or_else(|| dp.as_ref().map(|(n, _)| n.clone()))
                .unwrap_or_else(|| "direct".into())
        };

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "eth_blockNumber",
            params: vec![],
        };

        let start = std::time::Instant::now();
        let result = self
            .http_client
            .post(&config.rpc_url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;

        let latency = start.elapsed().as_millis() as u64;

        match result {
            Ok(resp) => match resp.json::<JsonRpcResponse>().await {
                Ok(json) => {
                    if let Some(error) = json.error {
                        ChainHealthStatus {
                            chain: chain_name.to_string(),
                            rpc_url: config.rpc_url,
                            provider: provider_name,
                            healthy: false,
                            block_number: None,
                            latency_ms: Some(latency),
                            error: Some(error.message),
                        }
                    } else if let Some(result) = json.result {
                        let block_str = result.as_str().unwrap_or("0x0");
                        let block = u64::from_str_radix(block_str.trim_start_matches("0x"), 16).unwrap_or(0);
                        ChainHealthStatus {
                            chain: chain_name.to_string(),
                            rpc_url: config.rpc_url,
                            provider: provider_name,
                            healthy: true,
                            block_number: Some(block),
                            latency_ms: Some(latency),
                            error: None,
                        }
                    } else {
                        ChainHealthStatus {
                            chain: chain_name.to_string(),
                            rpc_url: config.rpc_url,
                            provider: provider_name,
                            healthy: false,
                            block_number: None,
                            latency_ms: Some(latency),
                            error: Some("No result".into()),
                        }
                    }
                }
                Err(e) => ChainHealthStatus {
                    chain: chain_name.to_string(),
                    rpc_url: config.rpc_url,
                    provider: provider_name,
                    healthy: false,
                    block_number: None,
                    latency_ms: Some(latency),
                    error: Some(format!("Parse error: {}", e)),
                },
            },
            Err(e) => ChainHealthStatus {
                chain: chain_name.to_string(),
                rpc_url: config.rpc_url,
                provider: provider_name,
                healthy: false,
                block_number: None,
                latency_ms: Some(latency),
                error: Some(e.to_string()),
            },
        }
    }

    /// Check health of all chains concurrently
    pub async fn check_all_chains_health(&self) -> Vec<ChainHealthStatus> {
        let chain_names: Vec<String> = {
            let configs = self.configs.read().await;
            configs.keys().cloned().collect()
        };

        let mut handles = Vec::new();
        for name in chain_names {
            let rpc = self.clone();
            handles.push(tokio::spawn(async move {
                rpc.check_chain_health(&name).await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            if let Ok(status) = handle.await {
                results.push(status);
            }
        }
        results
    }

    /// Call a contract function (generic EVM call)
    pub async fn call_contract(
        &self,
        chain_name: &str,
        to: H160,
        data: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let config = self
            .get_chain(chain_name)
            .await
            .context(format!("Chain '{}' not found", chain_name))?;

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "eth_call",
            params: vec![
                serde_json::json!({
                    "to": format!("0x{}", hex::encode(to.as_bytes())),
                    "data": format!("0x{}", hex::encode(&data)),
                }),
                serde_json::json!("latest"),
            ],
        };

        let response: JsonRpcResponse = self
            .http_client
            .post(&config.rpc_url)
            .json(&request)
            .send()
            .await
            .context("Failed to send RPC request")?
            .json()
            .await
            .context("Failed to parse RPC response")?;

        if let Some(error) = response.error {
            anyhow::bail!("RPC error: {}", error.message);
        }

        let result_str = response
            .result
            .context("No result in RPC response")?
            .as_str()
            .context("Result is not a string")?
            .to_string();

        // Remove "0x" prefix and decode hex
        let hex_str = result_str.strip_prefix("0x").unwrap_or(&result_str);
        let bytes = hex::decode(hex_str).context("Failed to decode hex result")?;

        Ok(bytes)
    }
}

/// Encode a function call with selector and arguments
pub fn encode_call(selector: &[u8], args: &[Vec<u8>]) -> Vec<u8> {
    let mut data = selector.to_vec();
    for arg in args {
        data.extend_from_slice(arg);
    }
    data
}

/// Encode a uint256 argument
pub fn encode_uint256(value: u128) -> Vec<u8> {
    let mut word = [0u8; 32];
    let bytes = value.to_be_bytes();
    word[32 - bytes.len()..].copy_from_slice(&bytes);
    word.to_vec()
}

/// Encode an address argument (padded to 32 bytes)
pub fn encode_address(addr: &H160) -> Vec<u8> {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(addr.as_bytes());
    word.to_vec()
}
