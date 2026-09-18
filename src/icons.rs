//! Local icon cache + proxy.
//!
//! The web UI needs protocol / chain / token logos. We want the browser to
//! load everything from our own server (single origin, no CORS, no third-
//! party tracking) but we don't want to bundle hundreds of PNGs in the
//! binary. Compromise: serve icons on demand from a local disk cache,
//! downloading from upstream CDNs the first time they're requested and
//! re-using the file on every subsequent request.
//!
//! Routes:
//!   GET /icon/<category>/<name>   e.g. /icon/token/0xa0b8...eb48.png
//!
//! The `<img onerror>` JS fallback in the page handles the case where the
//! local cache missed and the upstream also 404'd (the server returns 404
//! in that case and the browser swaps to the colored-letter chip).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use sha3::{Digest, Keccak256};
use tokio::sync::RwLock;

use crate::http::HttpState;

/// Allowed categories — anything else gets a 400 to avoid arbitrary file
/// reads under `data/icons/`. The category is also the upstream CDNs we
/// query (see `upstream_url`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconCategory {
    Protocol,
    Chain,
    Token,
    /// `GET /icon/symbol/{name}.png` — fallback for tokens where the
    /// address-based lookup 404s (bridged assets, etc.). Served from a
    /// hardcoded table of well-known tickers → canonical Ethereum logos.
    /// The AssetRow emits a second `<img>` with this URL as a chained
    /// `onerror` fallback, so the user still sees the right icon for
    /// "USDC.e on Arbitrum" even though 1inch + Trust Wallet don't have
    /// that specific bridged address.
    Symbol,
}

impl IconCategory {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "protocol" => Some(Self::Protocol),
            "chain" => Some(Self::Chain),
            "token" => Some(Self::Token),
            "symbol" => Some(Self::Symbol),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Chain => "chain",
            Self::Token => "token",
            Self::Symbol => "symbol",
        }
    }
}

/// Canonical logo URL for a token symbol, used as a fallback when
/// address-based lookups 404 (bridged USDC, alt-chain USDe, etc.). The
/// list is intentionally small — these are the symbols users will
/// actually search for; the long tail falls through to the colored
/// letter chip.
const SYMBOL_CANONICAL_LOGO: &[(&str, &str)] = &[
    (
        "USDC",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48/logo.png",
    ),
    (
        "USDT",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xdAC17F958D2ee523a2206206994597C13D831ec7/logo.png",
    ),
    (
        "WETH",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xC02aa39b223FE8d0A0e5C4F27eAD9083C756Cc2/logo.png",
    ),
    (
        "WBTC",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599/logo.png",
    ),
    (
        "DAI",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x6B175474E89094C44Da98b954EedeAC495271d0F/logo.png",
    ),
    (
        "WSTETH",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x7f39C581F595B53c5cb19bD0b3f8dA6b935EdF66/logo.png",
    ),
    (
        "WEETH",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xCd5fE23C85820F7B72D0926FC9b05b837E008137/logo.png",
    ),
    (
        "RETH",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xae78736Cd615f374D3085123A210448E74Fc6393/logo.png",
    ),
    (
        "FRAX",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x853d955aCEf822Db058eb8505191caB3AA218F86/logo.png",
    ),
    (
        "LUSD",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x5f98805A4E8be255a32880FDeC7F6728C6568bA0/logo.png",
    ),
    (
        "GHO",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x40D16FC0246aD3160Ccc09B8D0D3E2C28A80bBa3/logo.png",
    ),
    (
        "AAVE",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x7Fc66500c84A76Ad7e9c93437bFc2904B42e2DFFE/logo.png",
    ),
    (
        "LINK",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x514910771AF9Ca656af840dff83E8264EcF986CA/logo.png",
    ),
    (
        "ARB",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0xB50721BCf8d664c30412Cfbc6cf7a15145234ad1/logo.png",
    ),
    (
        "OP",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x4200000000000000000000000000000000000042/logo.png",
    ),
    (
        "MAI",
        "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/ethereum/assets/0x8D6CeBD42485D54300d23C044fB1b2c5Bf725f19/logo.png",
    ),
];

/// In-memory cache of disk-relative paths we've already verified exist.
/// Lets us skip the `metadata()` syscall on the hot path.
#[derive(Default)]
struct CacheIndex {
    hits: std::collections::HashSet<String>,
}

#[derive(Clone)]
pub struct IconCache {
    root: PathBuf,
    index: Arc<RwLock<CacheIndex>>,
}

impl IconCache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            index: Arc::new(RwLock::new(CacheIndex::default())),
        }
    }

    /// On-disk path for `category/name`. We allow letters, digits, `.`,
    /// and lowercase `0x` prefix on token names; anything else returns
    /// None to keep callers from poking at `..` or weird filesystem chars.
    fn disk_path(&self, category: IconCategory, name: &str) -> Option<PathBuf> {
        let safe = match category {
            IconCategory::Token => {
                // "0x" + 40 hex chars = 42.
                if !name.starts_with("0x") || name.len() != 42 {
                    return None;
                }
                name.to_ascii_lowercase()
            }
            IconCategory::Symbol => {
                // "USDC", "USDC.e", "WETH" — uppercase ASCII letters + digits +
                // a few separators. Strip the .png suffix and the trailing
                // .e (so USDC.e maps to USDC's canonical logo).
                let stripped = name.trim_end_matches(".png");
                let canonical = stripped.trim_end_matches(".e");
                if canonical.is_empty()
                    || !canonical
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.')
                {
                    return None;
                }
                canonical.to_ascii_uppercase()
            }
            IconCategory::Protocol | IconCategory::Chain => {
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return None;
                }
                name.to_string()
            }
        };
        let dir = self.root.join(category.as_str());
        Some(dir.join(safe))
    }

    /// Read the file from disk if it's there. Returns the bytes.
    async fn read(&self, category: IconCategory, name: &str) -> Option<Vec<u8>> {
        let path = self.disk_path(category, name)?;
        let key = format!("{}/{}", category.as_str(), name);
        {
            let idx = self.index.read().await;
            if !idx.hits.contains(&key) {
                drop(idx);
                // First-time check: stat the file. If missing, fall through to fetch.
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return None;
                }
                let mut idx = self.index.write().await;
                idx.hits.insert(key);
            }
        }
        tokio::fs::read(&path).await.ok()
    }

    /// Download from upstream, save to disk, return the bytes.
    ///
    /// For tokens we try a chain of upstreams: 1inch first (covers ~70% of
    /// common EVM tokens with lowercase addresses), then Trust Wallet
    /// with the EIP-55 checksum (covers the long tail). All upstream
    /// requests are fired in parallel so the wall-clock latency is the
    /// fastest one, not the sum. The first 2xx response wins; the bytes
    /// are cached under the lowercase name so the next browser request
    /// hits disk.
    async fn fetch_and_cache(
        &self,
        category: IconCategory,
        name: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<Vec<u8>> {
        let urls = upstream_urls(category, name);
        if urls.is_empty() {
            anyhow::bail!("no upstream URL for {}/{}", category.as_str(), name);
        }

        // 1inch first (covers most popular tokens). Sequential, fast.
        if let Some(bytes) = try_fetch(client, &urls[0]).await {
            return self.persist(category, name, bytes).await;
        }

        // Trust Wallet fallback chain: 11 EVM chains, all in parallel with
        // a 2s deadline each. The first 200 wins; if all 404, we give up
        // and the browser's onerror fires the colored-letter fallback.
        let tw_urls = urls.iter().skip(1).cloned().collect::<Vec<_>>();
        if tw_urls.is_empty() {
            anyhow::bail!("only 1inch was tried, it 404'd");
        }
        let futures = tw_urls
            .iter()
            .map(|u| {
                let client = client.clone();
                let url = u.clone();
                async move {
                    let r = client.get(&url).timeout(Duration::from_secs(2)).send().await;
                    (url, try_extract_bytes(r).await)
                }
            })
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futures).await;
        for (url, bytes) in results {
            if let Some(b) = bytes {
                tracing::debug!(url = %url, "icon resolved via fallback");
                return self.persist(category, name, b).await;
            }
        }
        anyhow::bail!(
            "all upstreams failed for {}/{} (1inch + {} TW chains)",
            category.as_str(),
            name,
            tw_urls.len()
        )
    }

    async fn persist(
        &self,
        category: IconCategory,
        name: &str,
        bytes: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let path = self
            .disk_path(category, name)
            .ok_or_else(|| anyhow::anyhow!("invalid name"))?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let _ = tokio::fs::write(&path, &bytes).await;
        let mut idx = self.index.write().await;
        idx.hits.insert(format!("{}/{}", category.as_str(), name));
        Ok(bytes)
    }
}

/// Try one upstream URL. Returns the bytes on 2xx, None on 4xx/timeout/error.
async fn try_fetch(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    try_extract_bytes(Ok(resp)).await
}

async fn try_extract_bytes(
    resp: Result<reqwest::Response, reqwest::Error>,
) -> Option<Vec<u8>> {
    let resp = resp.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Build the upstream URL list for each category, in priority order.
/// The icon proxy tries them in sequence; the first 2xx response wins.
///
///   protocol — Trust Wallet `dapps/<slug>.png` (aave.com, www.spark.fi).
///              Blend isn't in the dapps tree → empty → 404 → letter.
///   chain    — Trust Wallet `blockchains/<folder>/info/logo.png`. Folder
///              mapping matches the one in `crate::web::chain_icon_url`.
///   token    — 1inch first (lowercase, covers ~70% of common tokens),
///              then Trust Wallet with the EIP-55 checksummed address
///              (covers the long tail; Aave V3 ABI decoding gives us
///              lowercase hex, so we re-checksum here).
///   symbol   — Hardcoded table of well-known tickers → canonical TW
///              logo. Hits when address-based lookups 404 (bridged USDC,
///              alt-chain USDe, etc.). USDC.e normalises to USDC.
fn upstream_urls(category: IconCategory, name: &str) -> Vec<String> {
    let tw = "https://raw.githubusercontent.com/trustwallet/assets/master";
    match category {
        IconCategory::Protocol => {
            let slug = match name {
                "aave_v3" => "aave.com",
                "spark" => "www.spark.fi",
                _ => return Vec::new(),
            };
            vec![format!("{tw}/dapps/{slug}.png")]
        }
        IconCategory::Chain => {
            let folder = match name {
                "ethereum" | "arbitrum" | "optimism" | "polygon" | "sonic" | "base"
                | "scroll" | "zksync" | "stellar" => name,
                "avalanche" => "avalanchec",
                "gnosis" => "xdai",
                "bnb" => "binance",
                _ => return Vec::new(),
            };
            vec![format!("{tw}/blockchains/{folder}/info/logo.png")]
        }
        IconCategory::Token => {
            // name is the lowercase 0x... address: "0x" + 40 hex chars = 42.
            if !name.starts_with("0x") || name.len() != 42 {
                return Vec::new();
            }
            // 1inch first (covers ~70% of common tokens, accepts lowercase
            // address). Trust Wallet fallback tries every EVM chain we
            // support, in parallel — the icon proxy picks the first 200,
            // and if all 404 the browser's onerror fires the colored-
            // letter fallback. EIP-55 checksum because TW's tree is
            // keyed by checksum-cased address.
            let checksummed = to_eip55_checksum(name);
            let mut urls = vec![format!("https://tokens.1inch.io/{name}.png")];
            for chain in &[
                "ethereum",
                "arbitrum",
                "optimism",
                "polygon",
                "avalanche",
                "sonic",
                "gnosis",
                "base",
                "scroll",
                "zksync",
                "binance",
            ] {
                let folder = match *chain {
                    "avalanche" => "avalanchec",
                    "gnosis" => "xdai",
                    "binance" => "binance",
                    other => other,
                };
                urls.push(format!(
                    "{tw}/blockchains/{folder}/assets/{checksummed}/logo.png"
                ));
            }
            urls
        }
        IconCategory::Symbol => {
            // name already had .png stripped + USDC.e→USDC normalization
            // applied by disk_path. Look up the canonical URL.
            SYMBOL_CANONICAL_LOGO
                .iter()
                .find(|(sym, _)| *sym == name)
                .map(|(_, url)| url.to_string())
                .into_iter()
                .collect()
        }
    }
}

/// Convert a lowercase `0x...` EVM address into its EIP-55 checksum form.
/// Each hex letter is uppercased iff the corresponding nibble of
/// keccak256(lowercase_address) is >= 8 — this is what Trust Wallet's
/// tree is keyed on.
fn to_eip55_checksum(lower: &str) -> String {
    let hex_part = lower.trim_start_matches("0x");
    let mut hasher = Keccak256::new();
    hasher.update(hex_part.as_bytes());
    let hash = hasher.finalize();
    let mut out = String::with_capacity(2 + hex_part.len());
    out.push_str("0x");
    for (i, c) in hex_part.chars().enumerate() {
        let nibble = hash[i / 2];
        let high = (nibble >> 4) & 0x0f;
        let out_char = if c.is_ascii_digit() {
            c
        } else if high >= 8 {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        };
        out.push(out_char);
    }
    out
}

/// HTTP handler: GET /icon/{category}/{name}.
pub async fn icon_handler(
    State(state): State<HttpState>,
    Path((category, name)): Path<(String, String)>,
) -> Response {
    let cat = match IconCategory::from_str(&category) {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "unknown category").into_response(),
    };

    // The URL ends in `.png` (browser cache hint). The icon's identifier
    // is the name without the extension — strip it so disk_path and
    // upstream_url see a clean `ethereum` / `0xa0b8...` / `aave_v3`.
    let name = name.strip_suffix(".png").unwrap_or(&name).to_string();

    // Hot path: serve from disk cache.
    if let Some(bytes) = state.icons.read(cat, &name).await {
        return png_response(bytes);
    }

    // Cold path: download from upstream CDN, save, serve.
    match state.icons.fetch_and_cache(cat, &name, &state.http_client).await {
        Ok(bytes) => png_response(bytes),
        Err(e) => {
            tracing::warn!(category = cat.as_str(), name = %name, error = %e, "icon fetch failed");
            (StatusCode::NOT_FOUND, "icon not found").into_response()
        }
    }
}

fn png_response(bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .body(Body::from(bytes))
        .unwrap()
}
