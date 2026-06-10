use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::Utc;
use stellar_xdr::curr::{self as xdr, Limits, ReadXdr, ScSymbol, ScVal, ScVec, StringM, WriteXdr};
use tracing::info;

use super::interest::{self, ReserveConfig, ReserveData};
use super::rpc::SorobanRpc;
use crate::mcp::types::{AssetRate, PoolRates};

/// Fetch pool rates from a Blend Capital lending pool on Stellar.
///
/// # Algorithm
///
/// 1. Read pool instance storage → backstop_rate, pool name
/// 2. Read ResList → list of reserve asset addresses
/// 3. For each reserve: read ResConfig + ResData
/// 4. Calculate rates using 3-segment interest model:
///    - Project ir_mod forward (decays when util < target)
///    - Project bRate/dRate forward (increases with interest accrual)
///    - borrow_apr = ir_mod × (r_base + r_one × util/target)
///    - supply_apr = borrow_apr × utilization × (1 - backstop_rate)
///    - borrow_apy = (1 + borrow_apr/365)^365 - 1
///    - supply_apy = (1 + supply_apr/52)^52 - 1
pub async fn fetch_pool_rates(rpc_url: &str, pool_id: &str) -> Result<PoolRates> {
    let rpc = SorobanRpc::new(rpc_url);
    let pool_addr = str_to_sc_address(pool_id)?;

    // ── Step 1: Read pool instance + ResList ──
    info!(pool_id, "Fetching pool metadata");

    let instance_key = xdr::LedgerKey::ContractData(xdr::LedgerKeyContractData {
        contract: pool_addr.clone(),
        key: ScVal::LedgerKeyContractInstance,
        durability: xdr::ContractDataDurability::Persistent,
    });
    let reslist_key = xdr::LedgerKey::ContractData(xdr::LedgerKeyContractData {
        contract: pool_addr.clone(),
        key: ScVal::Symbol(mk_symbol("ResList")),
        durability: xdr::ContractDataDurability::Persistent,
    });

    let pool_entries = rpc
        .get_ledger_entries(&[encode_key(&instance_key)?, encode_key(&reslist_key)?])
        .await
        .context("Failed to fetch pool instance")?;

    if pool_entries.entries.is_empty() {
        anyhow::bail!("No entries returned for pool {}", pool_id);
    }

    // Parse backstop_rate from instance storage
    let (backstop_rate, pool_name) = parse_pool_instance(&pool_entries.entries[0].xdr)?;

    // Parse reserve addresses from ResList
    let reslist_val = decode_entry_value(&pool_entries.entries[1].xdr)?;
    let reserve_addresses =
        extract_address_vec(&reslist_val).context("ResList is not a vec of addresses")?;

    info!(
        reserves = reserve_addresses.len(),
        backstop_rate = backstop_rate as f64 / 1e5,
        "Pool loaded"
    );

    // ── Step 2: Read ResConfig + ResData for each reserve ──
    let mut ledger_keys = Vec::new();
    for addr_str in &reserve_addresses {
        let asset_addr = str_to_sc_address(addr_str)?;
        ledger_keys.push(make_reserve_key(&pool_addr, &asset_addr, "ResConfig")?);
        ledger_keys.push(make_reserve_key(&pool_addr, &asset_addr, "ResData")?);
    }

    let encoded_keys: Vec<String> = ledger_keys.iter().map(encode_key).collect::<Result<_>>()?;
    let reserve_entries = rpc.get_ledger_entries(&encoded_keys).await?;

    // ── Step 3: Calculate rates for each reserve ──
    let mut assets = Vec::new();
    let mut idx = 0;

    for addr_str in &reserve_addresses {
        if idx + 1 >= reserve_entries.entries.len() {
            idx += 2;
            continue;
        }

        let config = match decode_entry_value(&reserve_entries.entries[idx].xdr)
            .and_then(|v| parse_reserve_config(&v))
        {
            Ok(c) => c,
            Err(_) => {
                idx += 2;
                continue;
            }
        };
        let data = match decode_entry_value(&reserve_entries.entries[idx + 1].xdr)
            .and_then(|v| parse_reserve_data(&v))
        {
            Ok(d) => d,
            Err(_) => {
                idx += 2;
                continue;
            }
        };
        idx += 2;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let rates = interest::calculate_rates(&config, &data, backstop_rate, now);
        let (symbol, _) = resolve_asset_info(addr_str);

        info!(
            asset = symbol,
            supply_apy = format!("{:.2}%", rates.supply_apy * 100.0),
            borrow_apy = format!("{:.2}%", rates.borrow_apy * 100.0),
            util = format!("{:.1}%", rates.utilization * 100.0),
            supplied = format!("{:.0}", rates.total_supplied),
            "Rate"
        );

        assets.push(AssetRate {
            asset_id: addr_str.clone(),
            asset_name: symbol.to_string(),
            decimals: config.decimals,
            supply_apr: rates.supply_apr,
            supply_apy: rates.supply_apy,
            borrow_apr: rates.borrow_apr,
            borrow_apy: rates.borrow_apy,
            utilization: rates.utilization,
            total_supplied: rates.total_supplied,
            total_borrowed: rates.total_borrowed,
        });
    }

    Ok(PoolRates {
        chain: "stellar".to_string(),
        protocol: "blend".to_string(),
        pool_id: pool_id.to_string(),
        pool_name,
        timestamp: Utc::now().to_rfc3339(),
        assets,
    })
}

// ── Pool instance parsing ────────────────────────────────────────────

/// Parse pool instance storage to extract backstop_rate and pool name.
/// Returns (backstop_rate_fixed7, pool_name).
fn parse_pool_instance(xdr_b64: &str) -> Result<(u32, String)> {
    let mut backstop_rate: u32 = 0;
    let mut pool_name = String::new();

    // Try XDR parsing first
    if let Ok(val) = decode_entry_value(xdr_b64) {
        if let ScVal::Map(Some(map)) = &val {
            for entry in map.iter() {
                if let ScVal::Symbol(sym) = &entry.key {
                    match sym_str(sym).as_str() {
                        "Config" => {
                            if let ScVal::Map(Some(cm)) = &entry.val {
                                for ce in cm.iter() {
                                    if let ScVal::Symbol(s) = &ce.key {
                                        let k = sym_str(s);
                                        if k == "bstop_rate" || k == "backstop_rate" {
                                            if let ScVal::U32(v) = ce.val {
                                                backstop_rate = v;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "Name" => {
                            if let ScVal::String(s) = &entry.val {
                                pool_name = s.to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Fallback: search raw bytes for bstop_rate
    if backstop_rate == 0 {
        let raw = BASE64.decode(xdr_b64).unwrap_or_default();
        if let Some(idx) = raw.windows(10).position(|w| w == b"bstop_rate") {
            let padded_end = (idx + 10 + 3) & !3;
            if padded_end + 8 <= raw.len() {
                let disc = u32::from_be_bytes(raw[padded_end..padded_end + 4].try_into().unwrap());
                let val =
                    u32::from_be_bytes(raw[padded_end + 4..padded_end + 8].try_into().unwrap());
                if disc == 3 {
                    // ScVal::U32
                    backstop_rate = val;
                }
            }
        }
    }

    Ok((backstop_rate, pool_name))
}

// ── XDR helpers ──────────────────────────────────────────────────────

fn mk_symbol(s: &str) -> ScSymbol {
    ScSymbol(StringM::try_from(s.as_bytes().to_vec()).expect("symbol too long"))
}

fn sym_str(sym: &ScSymbol) -> String {
    String::from_utf8_lossy(&sym.0).to_string()
}

fn encode_key(key: &xdr::LedgerKey) -> Result<String> {
    let bytes = key.to_xdr(Limits::none()).context("encode LedgerKey")?;
    Ok(BASE64.encode(&bytes))
}

fn decode_entry_value(xdr_b64: &str) -> Result<ScVal> {
    let bytes = BASE64.decode(xdr_b64).context("invalid base64")?;
    let data = xdr::LedgerEntryData::from_xdr(&bytes, Limits::none())
        .or_else(|_| {
            let entry = xdr::LedgerEntry::from_xdr(&bytes, Limits::none())?;
            Ok(entry.data)
        })
        .map_err(|e: xdr::Error| anyhow::anyhow!("parse entry XDR: {}", e))?;
    match data {
        xdr::LedgerEntryData::ContractData(cd) => Ok(cd.val),
        _ => anyhow::bail!("Entry is not ContractData"),
    }
}

fn str_to_sc_address(addr: &str) -> Result<xdr::ScAddress> {
    let strkey = stellar_strkey::Strkey::from_string(addr)
        .map_err(|e| anyhow::anyhow!("Invalid StrKey '{}': {:?}", addr, e))?;
    match strkey {
        stellar_strkey::Strkey::Contract(c) => Ok(xdr::ScAddress::Contract(xdr::Hash(c.0))),
        stellar_strkey::Strkey::PublicKeyEd25519(pk) => Ok(xdr::ScAddress::Account(
            xdr::AccountId(xdr::PublicKey::PublicKeyTypeEd25519(xdr::Uint256(pk.0))),
        )),
        _ => anyhow::bail!("Unsupported address type: {:?}", addr),
    }
}

fn sc_address_to_str(addr: &xdr::ScAddress) -> String {
    match addr {
        xdr::ScAddress::Account(account_id) => {
            let xdr::PublicKey::PublicKeyTypeEd25519(uint256) = &account_id.0;
            stellar_strkey::Strkey::PublicKeyEd25519(stellar_strkey::ed25519::PublicKey(uint256.0))
                .to_string()
                .as_str()
                .to_string()
        }
        xdr::ScAddress::Contract(hash) => {
            stellar_strkey::Strkey::Contract(stellar_strkey::Contract(hash.0))
                .to_string()
                .as_str()
                .to_string()
        }
    }
}

fn make_reserve_key(
    pool: &xdr::ScAddress,
    asset: &xdr::ScAddress,
    field: &str,
) -> Result<xdr::LedgerKey> {
    let vec_val = ScVal::Vec(Some(ScVec(
        vec![
            ScVal::Symbol(mk_symbol(field)),
            ScVal::Address(asset.clone()),
        ]
        .try_into()
        .map_err(|_| anyhow::anyhow!("VecM conversion failed"))?,
    )));
    Ok(xdr::LedgerKey::ContractData(xdr::LedgerKeyContractData {
        contract: pool.clone(),
        key: vec_val,
        durability: xdr::ContractDataDurability::Persistent,
    }))
}

fn extract_address_vec(val: &ScVal) -> Option<Vec<String>> {
    match val {
        ScVal::Vec(Some(vec)) => {
            let mut addrs = Vec::new();
            for item in vec.0.iter() {
                if let ScVal::Address(addr) = item {
                    addrs.push(sc_address_to_str(addr));
                } else {
                    return None;
                }
            }
            Some(addrs)
        }
        _ => None,
    }
}

// ── Reserve config/data parsing ──────────────────────────────────────

fn map_get<'a>(map: &'a xdr::ScMap, key: &str) -> Option<&'a ScVal> {
    map.iter().find_map(|e| {
        if let ScVal::Symbol(s) = &e.key {
            if sym_str(s) == key {
                return Some(&e.val);
            }
        }
        None
    })
}

fn parse_reserve_config(val: &ScVal) -> Result<ReserveConfig> {
    let map = match val {
        ScVal::Map(Some(m)) => m,
        _ => anyhow::bail!("ReserveConfig is not a map"),
    };
    let u32 = |k: &str| -> Result<u32> {
        match map_get(map, k) {
            Some(ScVal::U32(v)) => Ok(*v),
            _ => anyhow::bail!("missing or invalid '{}'", k),
        }
    };
    Ok(ReserveConfig {
        index: u32("index")?,
        decimals: u32("decimals")?,
        c_factor: u32("c_factor")?,
        l_factor: u32("l_factor")?,
        util: u32("util")?,
        max_util: u32("max_util")?,
        r_base: u32("r_base")?,
        r_one: u32("r_one")?,
        r_two: u32("r_two")?,
        r_three: u32("r_three")?,
        reactivity: u32("reactivity")?,
    })
}

fn parse_reserve_data(val: &ScVal) -> Result<ReserveData> {
    let map = match val {
        ScVal::Map(Some(m)) => m,
        _ => anyhow::bail!("ReserveData is not a map"),
    };
    let i128 = |k: &str| -> Result<i128> {
        match map_get(map, k) {
            Some(ScVal::I128(p)) => Ok(((p.hi as i128) << 64) | (p.lo as i128)),
            Some(ScVal::U128(p)) => Ok(((p.hi as i128) << 64) | (p.lo as i128)),
            Some(ScVal::I64(v)) => Ok(*v as i128),
            Some(ScVal::U64(v)) => Ok(*v as i128),
            _ => anyhow::bail!("missing or invalid '{}'", k),
        }
    };
    let u64 = |k: &str| -> Result<u64> {
        match map_get(map, k) {
            Some(ScVal::U64(v)) => Ok(*v),
            Some(ScVal::U32(v)) => Ok(*v as u64),
            Some(ScVal::I64(v)) => Ok(*v as u64),
            Some(ScVal::I128(p)) => Ok(p.lo),
            Some(ScVal::U128(p)) => Ok(p.lo),
            _ => anyhow::bail!("missing or invalid '{}'", k),
        }
    };
    Ok(ReserveData {
        b_supply: i128("b_supply")? as u128,
        d_supply: i128("d_supply")? as u128,
        interest_rate_modifier: u64("ir_mod")?,
        b_rate: i128("b_rate").unwrap_or(1_000_000_000_000) as u128,
        d_rate: i128("d_rate").unwrap_or(1_000_000_000_000) as u128,
        last_accrual: u64("last_time")?,
    })
}

// ── Asset info ───────────────────────────────────────────────────────

fn resolve_asset_info(address: &str) -> (&'static str, u32) {
    match address {
        // Blend Fixed Pool 资产
        "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA" => ("XLM", 7),
        "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75" => ("USDC", 7),
        "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV" => ("EURC", 7),
        // 通用资产
        "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC" => ("XLM", 7),
        "CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIUZ7VMZL4" => ("yUSDC", 7),
        "CAP5AMC2OHNVREOPMBF5TONHIXJHHNFX7VWQJ2LQX3K6ODGHYBU2THPQ" => ("BTC", 8),
        "CAUIQUFQ3IMTJ3HPQOYVSNKLJSMHOMSQF6DHLJHLGKZQXL7HBCHIW5YP" => ("ETH", 8),
        _ => ("unknown", 7),
    }
}
