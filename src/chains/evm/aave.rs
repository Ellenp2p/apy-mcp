use anyhow::{bail, Context, Result};
use primitive_types::H160;
use tracing::info;

use super::rpc::RpcManager;

// ── Aave V3 AaveProtocolDataProvider function selectors ────────────────
// keccak256("getAllReservesTokens()") = 0xb316ff89
const SELECTOR_GET_ALL_RESERVES_TOKENS: [u8; 4] = [0xb3, 0x16, 0xff, 0x89];

// keccak256("getReserveData(address)") = 0x35ea6a75
// Note: This is the DataProvider's getReserveData, NOT the Pool's.
const SELECTOR_GET_RESERVE_DATA: [u8; 4] = [0x35, 0xea, 0x6a, 0x75];

// ── Token data from getAllReservesTokens() ──────────────────────────────
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub symbol: String,
    pub address: H160,
}

// ── Reserve data from getReserveData(address) ───────────────────────────
/// Raw reserve data returned by AaveProtocolDataProvider.getReserveData()
#[derive(Debug, Clone)]
pub struct ReserveData {
    /// Unbacked amount (usually 0)
    pub unbacked: u128,
    /// Accrued to treasury (scaled)
    pub accrued_to_treasury_scaled: u128,
    /// Total aToken supply (scaled by 10^decimals)
    pub total_a_token: u128,
    /// Total stable debt (scaled by 10^decimals)
    pub total_stable_debt: u128,
    /// Total variable debt (scaled by 10^decimals)
    pub total_variable_debt: u128,
    /// Liquidity rate (APR in RAY = 10^27)
    pub liquidity_rate: u128,
    /// Variable borrow rate (APR in RAY = 10^27)
    pub variable_borrow_rate: u128,
    /// Stable borrow rate (APR in RAY = 10^27)
    pub stable_borrow_rate: u128,
    /// Average stable borrow rate (RAY)
    pub average_stable_borrow_rate: u128,
    /// Liquidity index (RAY)
    pub liquidity_index: u128,
    /// Variable borrow index (RAY)
    pub variable_borrow_index: u128,
    /// Last update timestamp
    pub last_update_timestamp: u64,
}

/// RAY = 10^27 (Aave's fixed-point precision)
const RAY: f64 = 1e27;

// ── Public API ─────────────────────────────────────────────────────────

/// Get all reserve tokens from AaveProtocolDataProvider
pub async fn get_all_reserves_tokens(
    rpc: &RpcManager,
    chain_name: &str,
    data_provider: H160,
) -> Result<Vec<TokenInfo>> {
    let data = SELECTOR_GET_ALL_RESERVES_TOKENS.to_vec();
    let result = rpc.call_contract(chain_name, data_provider, data).await?;

    let tokens = decode_token_data_array(&result)
        .context("Failed to decode getAllReservesTokens() response")?;

    info!(
        chain = chain_name,
        count = tokens.len(),
        "Fetched Aave reserves from DataProvider"
    );
    Ok(tokens)
}

/// Get reserve data for a specific asset from AaveProtocolDataProvider
pub async fn get_reserve_data(
    rpc: &RpcManager,
    chain_name: &str,
    data_provider: H160,
    asset: H160,
) -> Result<ReserveData> {
    let mut data = SELECTOR_GET_RESERVE_DATA.to_vec();
    // Append address argument (padded to 32 bytes)
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(asset.as_bytes());
    data.extend_from_slice(&word);

    let result = rpc
        .call_contract(chain_name, data_provider, data)
        .await
        .context(format!("Failed to call getReserveData for {:?}", asset))?;

    let reserve =
        decode_reserve_data(&result).context("Failed to decode getReserveData() response")?;
    Ok(reserve)
}

// ── Decoding helpers ───────────────────────────────────────────────────

/// Decode TokenData[] from getAllReservesTokens().
///
/// The Aave V3 AaveProtocolDataProvider returns `TokenData[]` where each tuple
/// is `(string symbol, address tokenAddress)`. Rather than relying on the
/// precise ABI tuple-head layout (which empirically fails on some chains
/// because the surrounding offset arithmetic doesn't quite match the
/// encoded data), we scan for valid 20-byte addresses and pair each one
/// with the nearest UTF-8 string of plausible length (1–64 chars) found in
/// a small window of words before or after the address.
fn decode_token_data_array(data: &[u8]) -> Result<Vec<TokenInfo>> {
    if data.len() < 64 {
        bail!("Response too short for TokenData[]");
    }

    let array_len = match read_u256_usize(data, 0) {
        Ok(n) => n,
        Err(_) => bail!("Cannot read array length"),
    };

    let num_words = data.len() / 32;
    if num_words < 2 {
        return Ok(Vec::new());
    }
    // Cap the address scan at array_len so we never return more entries than
    // the contract declared (defensive — a malformed response shouldn't pad
    // our pool count).
    let _ = array_len;

    // Scan for all candidate addresses: right-aligned 20 bytes in a 32-byte
    // word, with the first 12 bytes being zero padding and the address
    // itself non-zero and large enough to be a real token contract.
    let mut address_positions: Vec<(usize, H160)> = Vec::new();
    for word_idx in 1..num_words {
        let offset = word_idx * 32;
        if data[offset..offset + 12].iter().any(|&b| b != 0) {
            continue;
        }
        let addr = H160::from_slice(&data[offset + 12..offset + 32]);
        if addr == H160::zero() {
            continue;
        }
        let a = addr.as_bytes();
        // Filter out obviously-fake addresses (tiny values where bytes 0..2
        // are all zero — typical of small integers, offsets, and string
        // lengths being misread as addresses).
        if a[0] == 0 && a[1] == 0 && a[2] < 0x10 {
            continue;
        }
        address_positions.push((word_idx, addr));
    }

    // For each address, search a window of nearby words for a plausible
    // string (length word 1–64 followed by valid ASCII bytes). Try AFTER
    // first (matches the standard ABI tuple layout), then BEFORE if no
    // match — this catches cases where the encoder placed the string before
    // the address.
    let mut tokens: Vec<TokenInfo> = Vec::with_capacity(array_len);
    let mut used_string_words: Vec<usize> = Vec::new();

    for (addr_word, addr) in address_positions {
        if let Some((string_word, symbol)) =
            find_string_near(data, addr_word, num_words, &used_string_words)
        {
            used_string_words.push(string_word);
            used_string_words.push(string_word + 1);
            tokens.push(TokenInfo { symbol, address: addr });
        } else {
            tokens.push(TokenInfo {
                symbol: short_addr(addr),
                address: addr,
            });
        }
        if tokens.len() >= array_len {
            break;
        }
    }

    Ok(tokens)
}

/// Look in a 5-word window on either side of `addr_word` for a string
/// that hasn't already been paired with a previous address. Returns the
/// word index of the string-length word and the parsed symbol.
fn find_string_near(
    data: &[u8],
    addr_word: usize,
    num_words: usize,
    used: &[usize],
) -> Option<(usize, String)> {
    for direction in [1_i64, -1_i64] {
        for offset in 1..=5 {
            let len_word = if direction > 0 {
                addr_word.checked_add(offset)?
            } else {
                addr_word.checked_sub(offset)?
            };
            if len_word == 0 || len_word >= num_words {
                continue;
            }
            if used.contains(&len_word) {
                continue;
            }
            let len_offset = len_word * 32;
            let str_len = match read_u256_usize(data, len_offset) {
                Ok(v) if (1..=64).contains(&v) => v,
                _ => continue,
            };
            let str_start = (len_word + 1) * 32;
            if str_start + str_len > data.len() {
                continue;
            }
            let bytes = &data[str_start..str_start + str_len];
            if let Ok(s) = String::from_utf8(bytes.to_vec()) {
                if !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
                    return Some((len_word, s));
                }
            }
        }
    }
    None
}

fn push_fallback(tokens: &mut Vec<TokenInfo>, addr: H160) {
    tokens.push(TokenInfo {
        symbol: short_addr(addr),
        address: addr,
    });
}

fn short_addr(addr: H160) -> String {
    format!(
        "0x{}",
        hex::encode(addr.as_bytes())
            .chars()
            .take(8)
            .collect::<String>()
    )
}

/// Decode getReserveData() response
///
/// Returns 12 words:
///   [0]  unbacked
///   [1]  accruedToTreasuryScaled
///   [2]  totalAToken
///   [3]  totalStableDebt
///   [4]  totalVariableDebt
///   [5]  liquidityRate (APR in RAY)
///   [6]  variableBorrowRate (APR in RAY)
///   [7]  stableBorrowRate (APR in RAY)
///   [8]  averageStableBorrowRate
///   [9]  liquidityIndex
///   [10] variableBorrowIndex
///   [11] lastUpdateTimestamp (uint40, in last word)
pub fn decode_reserve_data(data: &[u8]) -> Result<ReserveData> {
    let expected_len = 12 * 32; // 12 words
    if data.len() < expected_len {
        bail!(
            "getReserveData response too short: {} bytes, expected {}",
            data.len(),
            expected_len
        );
    }

    Ok(ReserveData {
        unbacked: read_u256(data, 0)?,
        accrued_to_treasury_scaled: read_u256(data, 32)?,
        total_a_token: read_u256(data, 64)?,
        total_stable_debt: read_u256(data, 96)?,
        total_variable_debt: read_u256(data, 128)?,
        liquidity_rate: read_u256(data, 160)?,
        variable_borrow_rate: read_u256(data, 192)?,
        stable_borrow_rate: read_u256(data, 224)?,
        average_stable_borrow_rate: read_u256(data, 256)?,
        liquidity_index: read_u256(data, 288)?,
        variable_borrow_index: read_u256(data, 320)?,
        last_update_timestamp: read_u256(data, 348)? as u64, // uint40 in last 5 bytes
    })
}

// ── Low-level ABI helpers ──────────────────────────────────────────────

/// Read a u256 from ABI-encoded data at a byte offset
fn read_u256(data: &[u8], offset: usize) -> Result<u128> {
    if offset + 32 > data.len() {
        bail!("read_u256 out of bounds at offset {}", offset);
    }
    let mut val: u128 = 0;
    for &b in &data[offset..offset + 32] {
        val = val.checked_shl(8).unwrap_or(0) | b as u128;
    }
    Ok(val)
}

/// Read a usize from ABI-encoded data at a byte offset (for lengths/offsets)
fn read_u256_usize(data: &[u8], offset: usize) -> Result<usize> {
    let val = read_u256(data, offset)?;
    usize::try_from(val).context(format!("u256 value {} overflows usize", val))
}

/// Read raw bytes from ABI-encoded data at a byte offset
fn read_bytes(data: &[u8], offset: usize, len: usize) -> Result<Vec<u8>> {
    if offset + len > data.len() {
        bail!(
            "read_bytes out of bounds: offset={}, len={}, data_len={}",
            offset,
            len,
            data.len()
        );
    }
    Ok(data[offset..offset + len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_token_data_array_extracts_symbols() {
        // Construct a synthetic `getAllReservesTokens()` response with three
        // tuples: (string "USDC", address 0xA0b8...), (string "WETH", address
        // 0xC02a...), (string "WBTC", address 0x2260...). Verifies the decoder
        // walks the tuple head offsets rather than heuristically scanning
        // for addresses, which was producing `0xe6a93408`-style fallbacks
        // when strings were positioned before their corresponding address.
        let usdc_addr = H160::from_slice(&hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap());
        let weth_addr = H160::from_slice(&hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap());
        let wbtc_addr = H160::from_slice(&hex::decode("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap());

        let mut data = Vec::new();
        // word 0: array length = 3
        data.extend_from_slice(&[0u8; 31]);
        data.push(3);

        // words 1..4: tuple heads.
        // For 4-char symbols each tuple is 4 words = 128 bytes.
        //   tuple[0] starts at byte 0x80 (after the 4 head words)
        //   tuple[1] starts at byte 0x100
        //   tuple[2] starts at byte 0x180
        let head_offsets = [0x80u32, 0x100, 0x180];
        for &h in &head_offsets {
            data.extend_from_slice(&[0u8; 28]);
            data.extend_from_slice(&h.to_be_bytes());
        }

        // helper to write one tuple
        let mut write_tuple = |data: &mut Vec<u8>, addr: H160, symbol: &str| {
            // word 0: string offset relative to tuple start = 0x40
            data.extend_from_slice(&[0u8; 31]);
            data.push(0x40);
            // word 1: address (padded to 32 bytes)
            data.extend_from_slice(&[0u8; 12]);
            data.extend_from_slice(addr.as_bytes());
            // word 2: string length
            data.extend_from_slice(&[0u8; 31]);
            data.push(symbol.len() as u8);
            // word 3: string bytes (padded with zeros to 32)
            let mut bytes = vec![0u8; 32];
            for (i, b) in symbol.bytes().enumerate() {
                bytes[i] = b;
            }
            data.extend_from_slice(&bytes);
        };

        write_tuple(&mut data, usdc_addr, "USDC");
        write_tuple(&mut data, weth_addr, "WETH");
        write_tuple(&mut data, wbtc_addr, "WBTC");

        let tokens = decode_token_data_array(&data).expect("decode");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].symbol, "USDC");
        assert_eq!(tokens[0].address, usdc_addr);
        assert_eq!(tokens[1].symbol, "WETH");
        assert_eq!(tokens[1].address, weth_addr);
        assert_eq!(tokens[2].symbol, "WBTC");
        assert_eq!(tokens[2].address, wbtc_addr);
    }

    #[test]
    fn test_decode_token_data_array_falls_back_on_bad_length() {
        // Tuple with string length = 0 — decoder should fall back to the
        // short-address prefix rather than producing an empty symbol.
        let addr = H160::from_slice(&hex::decode("E6A9340895C5F75Cf2C0E6eD6e1AcE7Cdf6da1a8").unwrap());
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 31]);
        data.push(1); // 1 tuple
        // head[0] = 0x40 (after the length word + 1 head word)
        data.extend_from_slice(&[0u8; 28]);
        data.extend_from_slice(&0x40u32.to_be_bytes());
        // tuple: string_offset = 0x40
        data.extend_from_slice(&[0u8; 31]);
        data.push(0x40);
        // address
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(addr.as_bytes());
        // string length = 0
        data.extend_from_slice(&[0u8; 32]);
        // string bytes (empty)
        data.extend_from_slice(&[0u8; 32]);

        let tokens = decode_token_data_array(&data).expect("decode");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].address, addr);
        assert!(tokens[0].symbol.starts_with("0x"));
    }

    #[test]
    fn test_decode_reserve_data() {
        // Raw hex from Optimism AaveProtocolDataProvider.getReserveData(USDC)
        let raw_response = "0000000000000000000000000000000000000000000000000000000000000000\
                            00000000000000000000000000000000000000000000000000000000036c5c87\
                            00000000000000000000000000000000000000000000000c35b49130bca5400\
                            0000000000000000000000000000000000000000000000000000000000000000\
                            0000000000000000000000000000000000000000000000009125731e8f334000\
                            00000000000000000000000000000000000000000000005830e7d40e6d498d5e\
                            00000000000000000000000000000000000000000000007c3b0d90d8e4f4207c\
                            0000000000000000000000000000000000000000000000000000000000000000\
                            0000000000000000000000000000000000000000000000000000000000000000\
                            00000000000000000000000000000000000000000000000fb1e5bb93c5e800000\
                            000000000000000000000000000000000000000000000108e7c0c00d1c9c22e9\
                            000000000000000000000000000000000000000000000000000000006a08e5a3";

        let data = hex::decode(raw_response).unwrap();
        let reserve = decode_reserve_data(&data).unwrap();

        assert_eq!(reserve.unbacked, 0);
        assert!(reserve.total_a_token > 0, "total_a_token should be > 0");
        assert!(reserve.liquidity_rate > 0, "liquidity_rate should be > 0");
        assert!(
            reserve.variable_borrow_rate > 0,
            "variable_borrow_rate should be > 0"
        );

        // Verify rate calculations
        let ray: f64 = 1e27;
        let supply_apr = reserve.liquidity_rate as f64 / ray;
        let borrow_apr = reserve.variable_borrow_rate as f64 / ray;
        let supply_apy = supply_apr.exp() - 1.0;
        let borrow_apy = borrow_apr.exp() - 1.0;

        assert!(
            supply_apy > 0.0 && supply_apy < 0.5,
            "supply APY out of range: {}",
            supply_apy
        );
        assert!(
            borrow_apy > 0.0 && borrow_apy < 0.5,
            "borrow APY out of range: {}",
            borrow_apy
        );

        println!("USDC on Optimism:");
        println!("  Total Supply: {} (raw)", reserve.total_a_token);
        println!("  Supply APR: {:.4}%", supply_apr * 100.0);
        println!("  Borrow APR: {:.4}%", borrow_apr * 100.0);
        println!("  Supply APY: {:.4}%", supply_apy * 100.0);
        println!("  Borrow APY: {:.4}%", borrow_apy * 100.0);
    }

    #[tokio::test]
    #[ignore = "requires live RPC with a real Alchemy key (ALCHEMY_TEST_KEY env var)"]
    async fn test_live_optimism_usdc() {
        // Live-network test - requires ALCHEMY_TEST_KEY. Ignored by default in CI.
        let api_key = std::env::var("ALCHEMY_TEST_KEY").expect("ALCHEMY_TEST_KEY not set");
        let rpc = super::super::rpc::RpcManager::new();
        rpc.set_rpc_url(
            "optimism",
            &format!("https://opt-mainnet.g.alchemy.com/v2/{}", api_key),
        )
        .await;

        let provider =
            H160::from_slice(&hex::decode("69FA688f1Dc47d4B5d8029D5a35FB7a548310654").unwrap());
        let usdc =
            H160::from_slice(&hex::decode("0b2C639c533813f4Aa9D7837CAf62653d097Ff85").unwrap());

        // Step 1: Get all reserves
        let tokens = get_all_reserves_tokens(&rpc, "optimism", provider)
            .await
            .unwrap();
        println!("=== Optimism: {} reserves ===", tokens.len());
        for t in &tokens {
            println!("  {}: {:?}", t.symbol, t.address);
        }

        // Step 2: Get USDC reserve data
        let reserve = get_reserve_data(&rpc, "optimism", provider, usdc)
            .await
            .unwrap();

        let ray: f64 = 1e27;
        let total_supply = reserve.total_a_token as f64 / 1e6; // USDC = 6 decimals
        let total_borrow = (reserve.total_stable_debt + reserve.total_variable_debt) as f64 / 1e6;
        let util = if total_supply > 0.0 {
            total_borrow / total_supply
        } else {
            0.0
        };

        let supply_apr = reserve.liquidity_rate as f64 / ray;
        let borrow_apr = reserve.variable_borrow_rate as f64 / ray;
        let supply_apy = supply_apr.exp() - 1.0;
        let borrow_apy = borrow_apr.exp() - 1.0;

        println!("\n=== USDC on Optimism ===");
        println!("  Total Supply: {:.2} USDC", total_supply);
        println!("  Total Borrow: {:.2} USDC", total_borrow);
        println!("  Utilization:  {:.2}%", util * 100.0);
        println!("  Supply APR:   {:.4}%", supply_apr * 100.0);
        println!("  Borrow APR:   {:.4}%", borrow_apr * 100.0);
        println!("  Supply APY:   {:.4}%", supply_apy * 100.0);
        println!("  Borrow APY:   {:.4}%", borrow_apy * 100.0);
    }
}
