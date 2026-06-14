use std::collections::HashMap;

/// Pre-defined RPC provider template
#[derive(Debug, Clone)]
pub struct ProviderTemplate {
    /// Provider name (e.g., "alchemy", "infura", "public")
    pub name: String,
    /// URL template with {chain} and {key} placeholders
    /// Example: "https://{chain}.g.alchemy.com/v2/{key}"
    pub url_template: String,
    /// Chain name mapping: our chain name → provider's chain name
    /// Example: ("optimism", "opt-mainnet")
    pub chain_mapping: HashMap<String, String>,
    /// Whether this provider requires an API key
    pub needs_key: bool,
}

impl ProviderTemplate {
    /// Build the final RPC URL for a given chain
    pub fn build_url(&self, chain_name: &str, api_key: Option<&str>) -> Option<String> {
        let provider_chain = self.chain_mapping.get(chain_name)?;

        let mut url = self.url_template.clone();
        url = url.replace("{chain}", provider_chain);

        if self.needs_key {
            match api_key {
                Some(key) if !key.is_empty() => {
                    url = url.replace("{key}", key);
                }
                _ => return None, // Key required but not provided
            }
        } else {
            url = url.replace("{key}", "");
        }

        Some(url)
    }

    /// Get all supported chain names
    pub fn supported_chains(&self) -> Vec<&str> {
        self.chain_mapping.keys().map(|s| s.as_str()).collect()
    }
}

/// Get all pre-defined provider templates
pub fn builtin_providers() -> Vec<ProviderTemplate> {
    vec![
        alchemy_template(),
        infura_template(),
        drpc_template(),
        public_template(),
    ]
}

/// Get a provider template by name
pub fn get_provider(name: &str) -> Option<ProviderTemplate> {
    builtin_providers().into_iter().find(|p| p.name == name)
}

/// Alchemy provider template
fn alchemy_template() -> ProviderTemplate {
    let mut chain_mapping = HashMap::new();
    chain_mapping.insert("ethereum".into(), "eth-mainnet".into());
    chain_mapping.insert("polygon".into(), "polygon-mainnet".into());
    chain_mapping.insert("arbitrum".into(), "arb-mainnet".into());
    chain_mapping.insert("optimism".into(), "opt-mainnet".into());
    chain_mapping.insert("avalanche".into(), "avax-mainnet".into());
    chain_mapping.insert("base".into(), "base-mainnet".into());
    chain_mapping.insert("gnosis".into(), "gnosis-mainnet".into());
    chain_mapping.insert("bnb".into(), "bsc-mainnet".into());
    chain_mapping.insert("scroll".into(), "scroll-mainnet".into());
    chain_mapping.insert("sonic".into(), "sonic-mainnet".into());

    ProviderTemplate {
        name: "alchemy".into(),
        url_template: "https://{chain}.g.alchemy.com/v2/{key}".into(),
        chain_mapping,
        needs_key: true,
    }
}

/// Infura provider template
fn infura_template() -> ProviderTemplate {
    let mut chain_mapping = HashMap::new();
    chain_mapping.insert("ethereum".into(), "mainnet".into());
    chain_mapping.insert("polygon".into(), "polygon-mainnet".into());
    chain_mapping.insert("arbitrum".into(), "arbitrum-mainnet".into());
    chain_mapping.insert("optimism".into(), "optimism-mainnet".into());
    chain_mapping.insert("avalanche".into(), "avalanche-mainnet".into());
    chain_mapping.insert("base".into(), "base-mainnet".into());

    ProviderTemplate {
        name: "infura".into(),
        url_template: "https://{chain}.infura.io/v3/{key}".into(),
        chain_mapping,
        needs_key: true,
    }
}

/// dRPC provider template
fn drpc_template() -> ProviderTemplate {
    let mut chain_mapping = HashMap::new();
    chain_mapping.insert("ethereum".into(), "ethereum".into());
    chain_mapping.insert("polygon".into(), "polygon".into());
    chain_mapping.insert("arbitrum".into(), "arbitrum".into());
    chain_mapping.insert("optimism".into(), "optimism".into());
    chain_mapping.insert("avalanche".into(), "avalanche".into());
    chain_mapping.insert("base".into(), "base".into());
    chain_mapping.insert("gnosis".into(), "gnosis".into());
    chain_mapping.insert("bnb".into(), "bsc".into());
    chain_mapping.insert("scroll".into(), "scroll".into());
    chain_mapping.insert("zksync".into(), "zksync-era".into());
    chain_mapping.insert("sonic".into(), "sonic".into());

    ProviderTemplate {
        name: "drpc".into(),
        url_template: "https://lb.drpc.org/ogrpc/{chain}/{key}".into(),
        chain_mapping,
        needs_key: true,
    }
}

/// Public (free) RPC provider — uses known public endpoints
pub fn public_template() -> ProviderTemplate {
    // Public RPCs don't use a template; they have fixed URLs per chain.
    // We store them as a special provider that builds URLs from a static map.
    let mut chain_mapping = HashMap::new();
    chain_mapping.insert("ethereum".into(), "https://eth.llamarpc.com".into());
    chain_mapping.insert("polygon".into(), "https://polygon.llamarpc.com".into());
    chain_mapping.insert("arbitrum".into(), "https://arb1.arbitrum.io/rpc".into());
    chain_mapping.insert("optimism".into(), "https://mainnet.optimism.io".into());
    chain_mapping.insert("avalanche".into(), "https://api.avax.network/ext/bc/C/rpc".into());
    chain_mapping.insert("base".into(), "https://mainnet.base.org".into());
    chain_mapping.insert("gnosis".into(), "https://rpc.gnosis.gateway.fm".into());
    chain_mapping.insert("bnb".into(), "https://bsc-dataseed.binance.org".into());
    chain_mapping.insert("scroll".into(), "https://rpc.scroll.io".into());
    chain_mapping.insert("zksync".into(), "https://mainnet.era.zksync.io".into());
    chain_mapping.insert("sonic".into(), "https://rpc.soniclabs.com".into());

    ProviderTemplate {
        name: "public".into(),
        url_template: "{chain}".into(), // chain_mapping values are full URLs
        chain_mapping,
        needs_key: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alchemy_url() {
        let alchemy = alchemy_template();
        let url = alchemy.build_url("ethereum", Some("my_key")).unwrap();
        assert_eq!(url, "https://eth-mainnet.g.alchemy.com/v2/my_key");
    }

    #[test]
    fn test_alchemy_optimism() {
        let alchemy = alchemy_template();
        let url = alchemy.build_url("optimism", Some("test_key")).unwrap();
        assert_eq!(url, "https://opt-mainnet.g.alchemy.com/v2/test_key");
    }

    #[test]
    fn test_alchemy_missing_key() {
        let alchemy = alchemy_template();
        let url = alchemy.build_url("ethereum", None);
        assert!(url.is_none());
    }

    #[test]
    fn test_public_no_key_needed() {
        let public = public_template();
        let url = public.build_url("ethereum", None).unwrap();
        assert_eq!(url, "https://eth.llamarpc.com");
    }

    #[test]
    fn test_infura() {
        let infura = infura_template();
        let url = infura.build_url("ethereum", Some("abc123")).unwrap();
        assert_eq!(url, "https://mainnet.infura.io/v3/abc123");
    }

    #[test]
    fn test_supported_chains() {
        let alchemy = alchemy_template();
        let chains = alchemy.supported_chains();
        assert!(chains.contains(&"ethereum"));
        assert!(chains.contains(&"optimism"));
        assert!(chains.len() >= 10);
    }

    #[test]
    fn test_unknown_chain() {
        let alchemy = alchemy_template();
        let url = alchemy.build_url("solana", Some("key"));
        assert!(url.is_none());
    }
}
