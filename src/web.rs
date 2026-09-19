//! Web frontend: Dioxus SSR page + embedded static assets (rust-embed).
//!
//! `/` is rendered server-side on every request: the filter form submits GET
//! back to `/`, the handler runs the query through `RateService` (bounded by a
//! timeout so slow RPCs can't stall the page) and renders the results table
//! into the HTML. No client JS framework — only a tiny static script for the
//! OAuth redirect token capture.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use dioxus::prelude::*;
use rust_embed::RustEmbed;

use crate::http::HttpState;
use crate::service::types::{AllRatesResponse, AssetRate, PoolRates, QueryRatesParams};

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

/// How long the page render may wait for a fresh query (form submit) before
/// falling back to whatever is in the cache. Aave V3's per-chain RPC
/// timeouts add up if many chains have flaky public RPCs (no ALCHEMY_KEY);
/// 3s is enough for the healthy chains to come back, and the rest fall
/// through to the cache via `cached_fallback`.
const PAGE_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Cache-read budget for the initial page render (SQLite only, no network).
const CACHE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn mime_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

// ── SSR page ──────────────────────────────────────────────────────────

/// Everything the page needs for one render.
#[derive(Clone, PartialEq)]
struct PageData {
    filters: QueryRatesParams,
    response: AllRatesResponse,
    notice: Option<String>,
    from_cache_only: bool,
    sort: SortSpec,
}

fn fmt_pct(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

fn fmt_num(x: f64) -> String {
    if x >= 1e9 {
        format!("{:.2}B", x / 1e9)
    } else if x >= 1e6 {
        format!("{:.2}M", x / 1e6)
    } else if x >= 1e3 {
        format!("{:.2}K", x / 1e3)
    } else {
        format!("{:.2}", x)
    }
}

fn option_str(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// Compact human timestamp for the status line. The raw `fetched_at` is
/// RFC 3339 with nanosecond precision (`2026-09-16T15:23:51.965422400+00:00`)
/// which is unreadable in a status row — collapse it to `YYYY-MM-DD HH:MM`.
fn short_timestamp(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

// ── Sortable column headers ───────────────────────────────────────────

/// (sort_field, default_dir). When the user clicks a column that isn't
/// currently active, the page navigates with this default direction.
const SORTABLE_COLUMNS: &[(&str, &str)] = &[
    ("asset", "asc"),
    ("supply_apy", "desc"),
    ("borrow_apy", "desc"),
    ("utilization", "desc"),
];

/// Sort state passed into the page renderer.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SortSpec {
    field: &'static str,
    dir: &'static str,
}

const DEFAULT_SORT: SortSpec = SortSpec { field: "supply_apy", dir: "desc" };

fn parse_sort(raw: Option<&str>) -> Option<&'static str> {
    raw.and_then(|s| {
        if SORTABLE_COLUMNS.iter().any(|(f, _)| *f == s) {
            Some(match s {
                "asset" => "asset",
                "supply_apy" => "supply_apy",
                "borrow_apy" => "borrow_apy",
                "utilization" => "utilization",
                _ => unreachable!(),
            })
        } else {
            None
        }
    })
}

fn parse_dir(raw: Option<&str>) -> &'static str {
    match raw {
        Some("asc") => "asc",
        Some("desc") => "desc",
        _ => "desc",
    }
}

fn default_dir_for(field: &str) -> &'static str {
    SORTABLE_COLUMNS
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, d)| *d)
        .unwrap_or("desc")
}

/// When the user clicks a column header, decide which (field, dir) pair the
/// link should target: toggle the direction if it's already the active
/// column, otherwise switch to the column with its default direction.
fn sort_target(clicked: &'static str, active: SortSpec) -> (&'static str, &'static str) {
    if clicked == active.field {
        let new_dir = if active.dir == "asc" { "desc" } else { "asc" };
        (active.field, new_dir)
    } else {
        (clicked, default_dir_for(clicked))
    }
}

/// Build a query string that swaps the sort field/direction while
/// preserving every other active filter.
fn sort_url(filters: &QueryRatesParams, field: &str, dir: &str) -> String {
    let mut params: Vec<(&str, String)> = Vec::new();
    params.push(("sort", field.to_string()));
    params.push(("dir", dir.to_string()));
    if let Some(c) = filters.chain.as_deref() {
        if !c.is_empty() {
            params.push(("chain", c.to_string()));
        }
    }
    if let Some(p) = filters.protocol.as_deref() {
        if !p.is_empty() && p != "all" {
            params.push(("protocol", p.to_string()));
        }
    }
    if let Some(a) = filters.asset.as_deref() {
        if !a.is_empty() {
            params.push(("asset", a.to_string()));
        }
    }
    if let Some(v) = filters.min_supply_apy {
        params.push(("min_supply_apy", format!("{}", v * 100.0)));
    }
    if let Some(v) = filters.max_supply_apy {
        params.push(("max_supply_apy", format!("{}", v * 100.0)));
    }
    if let Some(v) = filters.min_utilization {
        params.push(("min_utilization", format!("{}", v * 100.0)));
    }
    let qs: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if qs.is_empty() {
        "/".to_string()
    } else {
        format!("/?{qs}")
    }
}

/// Sort the assets of one pool by the active sort column.
fn sort_assets(assets: &mut [AssetRate], field: &str, dir: &str) {
    let ascending = dir == "asc";
    assets.sort_by(|a, b| {
        let ord = match field {
            "asset" => a.asset_name.cmp(&b.asset_name),
            "supply_apy" => a
                .supply_apy
                .partial_cmp(&b.supply_apy)
                .unwrap_or(std::cmp::Ordering::Equal),
            "borrow_apy" => a
                .borrow_apy
                .partial_cmp(&b.borrow_apy)
                .unwrap_or(std::cmp::Ordering::Equal),
            "utilization" => a
                .utilization
                .partial_cmp(&b.utilization)
                .unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        };
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });
}

// ── Results: protocol tabs → chain subtabs → compact table ─────────────

struct ChainPanel {
    chain: String,
    pools: Vec<PoolRates>,
}

struct ProtoPanel {
    protocol: String,
    label: String,
    chains: Vec<ChainPanel>,
}

fn protocol_label(p: &str) -> &str {
    match p {
        "aave_v3" => "Aave V3",
        "spark" => "Spark",
        "blend" => "Blend",
        other => other,
    }
}

/// Group pools into protocol → chain panels. Asset ordering is the
/// caller's responsibility — apply `sort_assets` to each pool before
/// invoking this so user-selected sort survives.
fn group_chains(pools: Vec<PoolRates>) -> Vec<ChainPanel> {
    let mut chains: Vec<ChainPanel> = Vec::new();
    for pool in pools {
        if let Some(c) = chains.iter_mut().find(|c| c.chain == pool.chain) {
            c.pools.push(pool);
        } else {
            chains.push(ChainPanel {
                chain: pool.chain.clone(),
                pools: vec![pool],
            });
        }
    }
    chains
}

fn build_panels(pools: &[PoolRates]) -> Vec<ProtoPanel> {
    const PROTO_ORDER: [&str; 3] = ["aave_v3", "spark", "blend"];
    const CHAIN_ORDER: [&str; 12] = [
        "ethereum", "base", "arbitrum", "optimism", "polygon", "avalanche", "bnb", "scroll",
        "zksync", "sonic", "gnosis", "stellar",
    ];
    let mut panels = Vec::new();
    for &p in &PROTO_ORDER {
        let group: Vec<PoolRates> = pools.iter().filter(|x| x.protocol == p).cloned().collect();
        if group.is_empty() {
            continue;
        }
        let mut chains = group_chains(group);
        chains.sort_by_key(|c| {
            CHAIN_ORDER
                .iter()
                .position(|&x| x == c.chain)
                .unwrap_or(usize::MAX)
        });
        panels.push(ProtoPanel {
            protocol: p.to_string(),
            label: protocol_label(p).to_string(),
            chains,
        });
    }
    // Unknown protocols (new providers): one tab each, alphabetical.
    let mut extra: Vec<String> = pools
        .iter()
        .map(|p| p.protocol.clone())
        .filter(|p| !PROTO_ORDER.contains(&p.as_str()))
        .collect();
    extra.sort();
    extra.dedup();
    for p in extra {
        let group: Vec<PoolRates> = pools.iter().filter(|x| x.protocol == p).cloned().collect();
        let mut chains = group_chains(group);
        chains.sort_by(|a, b| a.chain.cmp(&b.chain));
        panels.push(ProtoPanel {
            label: p.clone(),
            protocol: p,
            chains,
        });
    }
    panels
}

/// Utilization shown as a gauge bar; color shifts as it gets high.
#[component]
fn Gauge(u: f64) -> Element {
    let pct = u * 100.0;
    let width = pct.clamp(0.0, 100.0);
    let cls = if u >= 0.85 {
        "gauge-fill lvl-high"
    } else if u >= 0.6 {
        "gauge-fill lvl-mid"
    } else {
        "gauge-fill"
    };
    rsx! {
        div { class: "util",
            div { class: "gauge",
                div { class: "{cls}", style: "width: {width:.1}%" }
            }
            span { class: "util-num", "{pct:.1}%" }
        }
    }
}

/// Build a local icon URL served by `/icon/<category>/<name>`. The route
/// in `crate::icons` looks the file up in `data/icons/`; on miss it pulls
/// from the upstream CDN (Trust Wallet for protocols/chains, 1inch for
/// tokens), writes to disk, and returns the bytes. So the browser always
/// loads icons from our own server — no CORS, no third-party cookies, and
/// the same address (e.g. `0xa0b8...` for USDC) only gets downloaded once
/// across all sessions.
fn protocol_icon_url(protocol: &str) -> Option<String> {
    Some(format!("/icon/protocol/{protocol}.png"))
}
fn chain_icon_url(chain: &str) -> Option<String> {
    Some(format!("/icon/chain/{chain}.png"))
}
fn asset_icon_url(_chain: &str, asset_id: &str) -> Option<String> {
    let addr = asset_id.trim_start_matches("0x").to_lowercase();
    if addr.len() != 40 || !addr.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("/icon/token/0x{addr}.png"))
}

/// Symbol-based fallback URL — the icon proxy looks up a canonical
/// Ethereum-mainnet logo for the ticker (USDC.e normalises to USDC, etc.).
/// Used as a chained `onerror` fallback in the asset row when the
/// address-based lookup 404s on bridged / alt-chain token instances.
fn asset_symbol_icon_url(asset_name: &str) -> Option<String> {
    // Strip non-ASCII junk (e.g. "USDC.e (Legacy)" → "USDC") and
    // normalise so USDC.e / usdc / USDC all hit the same table entry.
    let cleaned: String = asset_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    Some(format!("/icon/symbol/{}.png", cleaned.to_ascii_uppercase()))
}

/// 3-letter badge for the protocol fallback chip.
fn proto_label(p: &str) -> &'static str {
    match p {
        "aave_v3" => "AAV",
        "spark" => "SPK",
        "blend" => "BLN",
        other => {
            let mut out = String::with_capacity(3);
            for (i, c) in other.chars().enumerate() {
                if i >= 3 { break; }
                out.push(c.to_ascii_uppercase());
            }
            Box::leak(out.into_boxed_str())
        }
    }
}

/// 3-letter badge for the chain fallback chip.
fn chain_label(c: &str) -> &'static str {
    match c {
        "ethereum" => "ETH",
        "arbitrum" => "ARB",
        "optimism" => "OP",
        "polygon" => "POL",
        "avalanche" => "AVX",
        "sonic" => "SON",
        "gnosis" => "GNO",
        "base" => "BAS",
        "scroll" => "SCR",
        "zksync" => "ZK",
        "bnb" => "BNB",
        "stellar" => "XLM",
        other => {
            let mut out = String::with_capacity(3);
            for (i, c) in other.chars().enumerate() {
                if i >= 3 { break; }
                out.push(c.to_ascii_uppercase());
            }
            Box::leak(out.into_boxed_str())
        }
    }
}

/// Deterministic background color for the asset fallback chip — derived
/// from a quick hash of the asset id so the same token always gets the
/// same color across renders / swaps, while different tokens get
/// distinct colors.
fn asset_chip_color(asset_id: &str) -> String {
    let hash = asset_id.bytes().fold(0u32, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(b as u32)
    });
    let hue = hash % 360;
    format!("hsl({hue}, 62%, 52%)")
}

/// First printable ASCII character of the asset name, used as the asset
/// fallback chip letter. Falls back to '?' for empty / non-ASCII names.
fn asset_chip_letter(name: &str) -> char {
    name.chars()
        .find(|c| c.is_ascii_graphic())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?')
}

#[component]
fn AssetRow(a: AssetRate, pool_protocol: String, pool_chain: String) -> Element {
    let proto_url = protocol_icon_url(&pool_protocol);
    let chain_url = chain_icon_url(&pool_chain);
    let asset_url = asset_icon_url(&pool_chain, &a.asset_id);
    let asset_sym_url = asset_symbol_icon_url(&a.asset_name);
    let proto_fb = proto_label(&pool_protocol);
    let chain_fb = chain_label(&pool_chain);
    let asset_bg = asset_chip_color(&a.asset_id);
    let asset_letter = asset_chip_letter(&a.asset_name);
    rsx! {
        tr {
            td { class: "asset-cell",
                div { class: "asset-icons",
                    span { class: "ic proto-ic",
                        if let Some(u) = proto_url {
                            img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{pool_protocol}",
                                  "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex'" }
                        }
                        span { class: "ic-fb proto-fb", "data-proto": "{pool_protocol}",
                               title: "{pool_protocol}", "{proto_fb}" }
                    }
                    span { class: "ic chain-ic",
                        if let Some(u) = chain_url {
                            img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{pool_chain}",
                                  "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex'" }
                        }
                        span { class: "ic-fb chain-fb", "data-chain": "{pool_chain}",
                               title: "{pool_chain}", "{chain_fb}" }
                    }
                    span { class: "ic asset-ic",
                        // Chained fallback: address-based first, then
                        // symbol-based (canonical USDC/WETH/etc. logos
                        // from Trust Wallet Ethereum), then colored letter.
                        // The second <img> starts display:none and only
                        // shows up if the first one's onerror fires.
                        if let Some(u) = asset_url {
                            img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{a.asset_name}",
                                  "onerror": "this.style.display='none'; this.parentElement.querySelector('.ic-img-symbol').style.display='block';" }
                        }
                        if let Some(u) = asset_sym_url {
                            img { class: "ic-img ic-img-symbol", src: "{u}", loading: "lazy", alt: "{a.asset_name}",
                                  style: "display:none",
                                  "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex';" }
                        }
                        span { class: "ic-fb asset-fb",
                               style: "background: {asset_bg}",
                               title: "{a.asset_name}",
                               "{asset_letter}" }
                    }
                }
                span { class: "asset-name tip",
                    "{a.asset_name}"
                    span { class: "tip-box",
                        span { class: "tip-row", "总供应 ", b { "{fmt_num(a.total_supplied)}" } }
                        span { class: "tip-row", "总借出 ", b { "{fmt_num(a.total_borrowed)}" } }
                    }
                }
            }
            td { class: "num pos", "{fmt_pct(a.supply_apy)}" }
            td { class: "num neg", "{fmt_pct(a.borrow_apy)}" }
            td { Gauge { u: a.utilization } }
        }
    }
}

#[component]
fn SortHeader(
    label: &'static str,
    field: &'static str,
    active: SortSpec,
    href: String,
    align_right: bool,
) -> Element {
    let is_active = active.field == field;
    let indicator = if is_active {
        if active.dir == "asc" { "↑" } else { "↓" }
    } else {
        ""
    };
    let th_cls = if align_right { "col-num" } else { "" };
    let a_cls = if is_active { "sort-link active" } else { "sort-link" };
    let th_full_cls = if is_active {
        format!("{th_cls} sorted")
    } else {
        th_cls.to_string()
    };
    rsx! {
        th { class: "{th_full_cls}",
            a { class: "{a_cls}",
                href: "{href}",
                title: "按 {label} 排序",
                "hx-get": "{href}",
                "hx-target": "#results-region",
                "hx-swap": "outerHTML",
                "hx-indicator": "#results-region",
                "hx-scroll": "false",
                "hx-push-url": "true",
                "{label}"
                // Always render the indicator span (empty for inactive columns)
                // so column widths stay stable across sort changes — without this
                // the ↑/↓ appearing on a different header shifts the whole table
                // and the user sees the page jump by 1-2px.
                span { class: "sort-ind", "{indicator}" }
            }
        }
    }
}

#[component]
fn PoolTable(
    pool: PoolRates,
    show_pool_name: bool,
    sort: SortSpec,
    filters: QueryRatesParams,
) -> Element {
    let (asset_f, asset_d) = sort_target("asset", sort);
    let (supply_f, supply_d) = sort_target("supply_apy", sort);
    let (borrow_f, borrow_d) = sort_target("borrow_apy", sort);
    let (util_f, util_d) = sort_target("utilization", sort);
    rsx! {
        if show_pool_name {
            div { class: "pool-sub", "{pool.pool_name}" }
        }
        table {
            thead {
                tr {
                    SortHeader { label: "资产", field: "asset", active: sort, href: sort_url(&filters, asset_f, asset_d), align_right: false }
                    SortHeader { label: "Supply APY", field: "supply_apy", active: sort, href: sort_url(&filters, supply_f, supply_d), align_right: true }
                    SortHeader { label: "Borrow APY", field: "borrow_apy", active: sort, href: sort_url(&filters, borrow_f, borrow_d), align_right: true }
                    SortHeader { label: "Utilization", field: "utilization", active: sort, href: sort_url(&filters, util_f, util_d), align_right: false }
                }
            }
            tbody {
                for a in pool.assets.clone() {
                    AssetRow {
                        a: a,
                        pool_protocol: pool.protocol.clone(),
                        pool_chain: pool.chain.clone(),
                    }
                }
            }
        }
    }
}

/// Results grouped into protocol tabs with chain subtabs. JS adds the
/// `tabs-on` class to enable tab behavior; without JS everything renders
/// stacked and visible.
#[component]
fn ResultsTabs(pools: Vec<PoolRates>, sort: SortSpec, filters: QueryRatesParams) -> Element {
    let panels = build_panels(&pools);
    rsx! {
        div { class: "results",
            if panels.len() > 1 {
                div { class: "tabs",
                    for (i, p) in panels.iter().enumerate() {
                        {
                            let cls = format!("tab{}", if i == 0 { " active" } else { "" });
                            rsx! {
                                button { r#type: "button", class: "{cls}", "data-tab": "{p.protocol}",
                                    "{p.label}"
                                }
                            }
                        }
                    }
                }
            }
            for (i, p) in panels.into_iter().enumerate() {
                {
                    let cls = format!("tab-panel{}", if i == 0 { " active" } else { "" });
                    rsx! {
                        section { class: "{cls}", "data-panel": "{p.protocol}",
                            if p.chains.len() > 1 {
                                div { class: "subtabs",
                                    for (j, c) in p.chains.iter().enumerate() {
                                        {
                                            let cls = format!("subtab{}", if j == 0 { " active" } else { "" });
                                            rsx! {
                                                button { r#type: "button", class: "{cls}", "data-chain": "{c.chain}",
                                                    "{c.chain}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            for (j, c) in p.chains.into_iter().enumerate() {
                                {
                                    let cls = format!("subpanel{}", if j == 0 { " active" } else { "" });
                                    let multi = c.pools.len() > 1;
                                    rsx! {
                                        div { class: "{cls}", "data-chain": "{c.chain}",
                                            for pool in c.pools {
                                                PoolTable { pool: pool, show_pool_name: multi, sort: sort, filters: filters.clone() }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Flat global table — one row per asset across every protocol/chain,
/// sorted by the active `sort` field. Replaces the tabbed `ResultsTabs`
/// view when the user explicitly picks a non-default sort (clicks a
/// column header) so they see one ordered list across all protocols
/// rather than per-pool sort within each tab.
#[component]
fn ResultsGlobalTable(
    pools: Vec<PoolRates>,
    sort: SortSpec,
    filters: QueryRatesParams,
) -> Element {
    // Flatten every pool's assets into one list, tagging each with its
    // owning pool's protocol/chain so the row can render the right
    // icons and the sort can be globally consistent.
    let mut rows: Vec<(PoolRates, AssetRate)> = Vec::new();
    for pool in pools.iter() {
        for asset in pool.assets.iter() {
            rows.push((pool.clone(), asset.clone()));
        }
    }
    // Global sort: re-sort the flat list by the active field. Pulled
    // out into a small closure so the AssetRow rendering stays clean.
    let sort_field = sort.field;
    let sort_dir = sort.dir;
    rows.sort_by(|a, b| {
        let key = |pair: &(PoolRates, AssetRate)| -> std::cmp::Ordering {
            let asset = &pair.1;
            match sort_field {
                "asset" => asset.asset_name.cmp(&b.1.asset_name),
                "supply_apy" => asset
                    .supply_apy
                    .partial_cmp(&b.1.supply_apy)
                    .unwrap_or(std::cmp::Ordering::Equal),
                "borrow_apy" => asset
                    .borrow_apy
                    .partial_cmp(&b.1.borrow_apy)
                    .unwrap_or(std::cmp::Ordering::Equal),
                "utilization" => asset
                    .utilization
                    .partial_cmp(&b.1.utilization)
                    .unwrap_or(std::cmp::Ordering::Equal),
                _ => std::cmp::Ordering::Equal,
            }
        };
        if sort_dir == "asc" {
            key(a).cmp(&key(b))
        } else {
            key(b).cmp(&key(a))
        }
    });

    let (asset_f, asset_d) = sort_target("asset", sort);
    let (supply_f, supply_d) = sort_target("supply_apy", sort);
    let (borrow_f, borrow_d) = sort_target("borrow_apy", sort);
    let (util_f, util_d) = sort_target("utilization", sort);

    rsx! {
        div { class: "results global",
            div { class: "global-table",
                table {
                    thead {
                        tr {
                            th { "协议" }
                            th { "链" }
                            SortHeader { label: "资产", field: "asset", active: sort, href: sort_url(&filters, asset_f, asset_d), align_right: false }
                            SortHeader { label: "Supply APY", field: "supply_apy", active: sort, href: sort_url(&filters, supply_f, supply_d), align_right: true }
                            SortHeader { label: "Borrow APY", field: "borrow_apy", active: sort, href: sort_url(&filters, borrow_f, borrow_d), align_right: true }
                            SortHeader { label: "Utilization", field: "utilization", active: sort, href: sort_url(&filters, util_f, util_d), align_right: false }
                        }
                    }
                    tbody {
                        for (pool, asset) in rows {
                            GlobalAssetRow { pool: pool, asset: asset }
                        }
                    }
                }
            }
        }
    }
}

/// One row of the global table: protocol icon, chain icon, asset icon +
/// name (with the existing tooltip), and the three numeric cells.
#[component]
fn GlobalAssetRow(pool: PoolRates, asset: AssetRate) -> Element {
    let proto_url = protocol_icon_url(&pool.protocol);
    let chain_url = chain_icon_url(&pool.chain);
    let asset_url = asset_icon_url(&pool.chain, &asset.asset_id);
    let asset_sym_url = asset_symbol_icon_url(&asset.asset_name);
    let proto_fb = proto_label(&pool.protocol);
    let chain_fb = chain_label(&pool.chain);
    let asset_bg = asset_chip_color(&asset.asset_id);
    let asset_letter = asset_chip_letter(&asset.asset_name);
    rsx! {
        tr {
            td { class: "col-proto",
                span { class: "ic proto-ic",
                    if let Some(u) = proto_url {
                        img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{pool.protocol}",
                              "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex'" }
                    }
                    span { class: "ic-fb proto-fb", "data-proto": "{pool.protocol}",
                           title: "{pool.protocol}", "{proto_fb}" }
                }
            }
            td { class: "col-chain",
                span { class: "ic chain-ic",
                    if let Some(u) = chain_url {
                        img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{pool.chain}",
                              "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex'" }
                    }
                    span { class: "ic-fb chain-fb", "data-chain": "{pool.chain}",
                           title: "{pool.chain}", "{chain_fb}" }
                }
            }
            td { class: "asset-cell",
                div { class: "asset-icons",
                    span { class: "ic asset-ic",
                        if let Some(u) = asset_url {
                            img { class: "ic-img", src: "{u}", loading: "lazy", alt: "{asset.asset_name}",
                                  "onerror": "this.style.display='none'; this.parentElement.querySelector('.ic-img-symbol').style.display='block';" }
                        }
                        if let Some(u) = asset_sym_url {
                            img { class: "ic-img ic-img-symbol", src: "{u}", loading: "lazy", alt: "{asset.asset_name}",
                                  style: "display:none",
                                  "onerror": "this.style.display='none'; this.nextElementSibling.style.display='inline-flex';" }
                        }
                        span { class: "ic-fb asset-fb",
                               style: "background: {asset_bg}",
                               title: "{asset.asset_name}",
                               "{asset_letter}" }
                    }
                }
                span { class: "asset-name tip",
                    "{asset.asset_name}"
                    span { class: "tip-box",
                        span { class: "tip-row", "总供应 ", b { "{fmt_num(asset.total_supplied)}" } }
                        span { class: "tip-row", "总借出 ", b { "{fmt_num(asset.total_borrowed)}" } }
                    }
                }
            }
            td { class: "num pos", "{fmt_pct(asset.supply_apy)}" }
            td { class: "num neg", "{fmt_pct(asset.borrow_apy)}" }
            td { Gauge { u: asset.utilization } }
        }
    }
}
fn render_page(data: &PageData) -> String {
    let body = rsx! {
        body {
            header { class: "topbar",
                div { class: "brand", "⚡ APY" span { " MCP" } }
                nav { class: "topnav",
                    a { href: "/", "首页" }
                    a { href: "/docs", "使用文档" }
                }
                div { class: "auth-area",
                    a { class: "btn", href: "/auth/github", "🐙 GitHub 登录" }
                }
            }
            main { class: "container",
                section { class: "hero",
                    h1 { "跨链 DeFi 借贷利率，一站查询" }
                    p { "聚合 Aave V3、Spark Savings、Stellar Blend 的实时 Supply / Borrow APY，支持按链 / 资产 / 协议筛选" }
                }
                div { id: "results-region", class: "results-region",
                    ResultsSection { data: data.clone() }
                }
                footer { class: "footer",
                    a { href: "/docs", "接入文档" }
                    " · "
                    a { href: "/health", "/health" }
                    " · "
                    a { href: "/mcp", "/mcp" }
                    " · API: "
                    code { "GET /api/v1/rates" }
                    " (公开) · powered by "
                    a { href: "https://dioxuslabs.com", target: "_blank", rel: "noopener", "Dioxus SSR" }
                }
            }
            script { src: "/app.js", defer: true }
        }
    };

    html_shell("APY MCP — DeFi 借贷利率聚合", body)
}

/// Render just the swap-able region (`<div id="results-region">…</div>`) for
/// htmx partial refresh. Server returns this when the request includes
/// `HX-Request: true`. The full document chrome (topbar, hero, filter form,
/// footer, scripts, CSS) is omitted — htmx swaps the returned fragment into
/// the existing `#results-region` on the page.
fn render_results_only(data: &PageData) -> String {
    let element = rsx! {
        div { id: "results-region", class: "results-region",
            ResultsSection { data: data.clone() }
        }
    };
    dioxus::ssr::render_element(element)
}

/// Notice + status line + (empty OR tabs OR global table). This is the
/// region htmx swaps on sort/filter clicks; the rest of the page (form,
/// topbar, footer) stays put so user input isn't lost and there's no
/// scroll/flicker.
///
/// When the user clicks a sort header, `data.sort` carries the active
/// sort field. We detect a non-default sort and switch to a flat global
/// table that sorts every asset across every protocol/chain in one shot
/// — the tabbed view sorts assets within each pool only, which is useless
/// when the user wants to find the highest-APY asset globally.
#[component]
fn ResultsSection(data: PageData) -> Element {
    let mut pools = data.response.pools.clone();
    for pool in pools.iter_mut() {
        sort_assets(&mut pool.assets, data.sort.field, data.sort.dir);
    }
    let fetched = data.response.fetched_at.clone();
    let notice = data.notice.clone();
    let empty = pools.is_empty();
    let asset_count: usize = pools.iter().map(|p| p.assets.len()).sum();
    // The default landing view is the tabbed (protocol-grouped) view;
    // any explicit sort click flips to a single global table so the user
    // sees one ordered list across all protocols/chains.
    let is_default_sort = data.sort.field == DEFAULT_SORT.field
        && data.sort.dir == DEFAULT_SORT.dir;
    let mut unique_protocols = pools.iter().map(|p| p.protocol.clone()).collect::<Vec<_>>();
    unique_protocols.sort();
    unique_protocols.dedup();
    let mut unique_chains = pools.iter().map(|p| p.chain.clone()).collect::<Vec<_>>();
    unique_chains.sort();
    unique_chains.dedup();
    let timestamp = short_timestamp(&fetched);
    let sort = data.sort;
    let filters = data.filters;

    rsx! {
        if let Some(ref n) = notice {
            div { class: "notice error", "{n}" }
        }
        section { class: "card",
            if empty {
                div { class: "status err",
                    span { class: "status-dot" }
                    if notice.is_some() {
                        "数据预热中，请稍后刷新"
                    } else {
                        "没有匹配的结果（可能是所选链的 RPC 暂时不可用，或缓存已过期，稍后再试）"
                    }
                }
            } else {
                div { class: "status",
                    span { class: "status-dot" }
                    span { "Tracking " }
                    span { class: "status-num", "{asset_count}" }
                    span { " assets across " }
                    span { class: "status-num", "{unique_protocols.len()}" }
                    span { " protocols on " }
                    span { class: "status-num", "{unique_chains.len()}" }
                    span { " networks" }
                    span { class: "status-time", "{timestamp}" }
                    if data.from_cache_only {
                        span { class: "cache-pill", "缓存数据" }
                    }
                    if !is_default_sort {
                        a { class: "view-toggle", href: "/",
                            title: "返回按协议 / 链分组",
                            "按协议/链分组" }
                    }
                }
                if is_default_sort {
                    ResultsTabs { pools: pools, sort: sort, filters: filters }
                } else {
                    ResultsGlobalTable { pools: pools, sort: sort, filters: filters }
                }
            }
        }
    }
}

/// Wrap a rendered rsx `<body>` element in the standard HTML document shell.
fn html_shell(title: &str, body: Element) -> String {
    let html = dioxus::ssr::render_element(body);
    format!("<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\
             <title>{title}</title>\
             <link rel=\"stylesheet\" href=\"/style.css\">\
             <script src=\"/htmx.min.js\" defer></script>\
             </head>{html}</html>")
}

/// GET /docs — public usage guide for the MCP endpoint (no auth required).
pub async fn docs_handler(State(state): State<HttpState>) -> Response {
    let base = state.base_url.trim_end_matches('/').to_string();
    let mcp_url = format!("{base}/mcp");
    let enabled = state.tools.is_some();

    let client_config = format!(
        r#"{{
  "mcpServers": {{
    "apy-mcp": {{
      "url": "{mcp_url}",
      "headers": {{ "Authorization": "Bearer <API_KEY 或 OAuth token>" }}
    }}
  }}
}}"#
    );
    let claude_cmd = format!("claude mcp add --transport http apy-mcp {mcp_url}");
    let key_curl = format!(
        "curl -X POST {base}/admin/keys \\\n  -H \"Authorization: Bearer <ADMIN_TOKEN>\" \\\n  -H \"Content-Type: application/json\" \\\n  -d '{{\"name\":\"my-client\"}}'"
    );
    let api_curl = format!("curl \"{base}/api/v1/rates?asset=USDC\"");

    let body = rsx! {
        body {
            header { class: "topbar",
                div { class: "brand", "⚡ APY" span { " MCP" } }
                nav { class: "topnav",
                    a { href: "/", "首页" }
                    a { href: "/docs", "使用文档" }
                }
                div { class: "auth-area",
                    a { class: "btn", href: "/auth/github", "🐙 GitHub 登录" }
                }
            }
            main { class: "container",
                section { class: "hero",
                    h1 { "MCP 接入指南" }
                    p { "把本服务作为 MCP 工具接入你的 AI 客户端(Claude、Cursor、VS Code 等),让 AI 直接查询跨链 DeFi 借贷利率" }
                }
                section { class: "card doc",
                    h2 { "① MCP 端点" }
                    p { class: "endpoint",
                        code { "{mcp_url}" }
                        if enabled {
                            span { class: "pill ok", "已启用" }
                        } else {
                            span { class: "pill off", "未启用" }
                        }
                    }
                    p { "协议: MCP Streamable HTTP。未启用时,请确认启动时没有加 “--enable-mcp false”,且二进制编译时包含 " code { "mcp" } " feature(默认包含)。" }
                }
                section { class: "card doc",
                    h2 { "② 客户端接入" }
                    h3 { "Claude Code(命令行)" }
                    p { "在项目目录执行:" }
                    pre { code { "{claude_cmd}" } }
                    p { "首次连接会自动弹出浏览器完成 GitHub 登录,之后即可使用。" }
                    h3 { "Claude Desktop / Cursor / 其他客户端" }
                    p { "编辑客户端的 MCP 配置文件(如 Claude Desktop 的 " code { "claude_desktop_config.json" } "),加入:" }
                    pre { code { "{client_config}" } }
                }
                section { class: "card doc",
                    h2 { "③ 认证方式" }
                    h3 { "GitHub OAuth(推荐,人类用户)" }
                    ol {
                        li { "客户端首次连接时会自动发现 OAuth 配置并弹出浏览器;" }
                        li { "完成 GitHub 登录授权后,客户端自动获得 token;" }
                        li { "也可以在首页点 \"GitHub 登录\",手动把拿到的 token 填进客户端配置。" }
                    }
                    h3 { "API Key(服务器 / 脚本)" }
                    p { "用 admin token 创建长期有效的 API key:" }
                    pre { code { "{key_curl}" } }
                    p { "返回的 " code { "key" } " 即 Bearer token,放入上面配置的 " code { "Authorization" } " 头即可。" }
                }
                section { class: "card doc",
                    h2 { "④ 公开 REST API(无需认证)" }
                    p { "如果只需要程序化查询,不一定要走 MCP——以下接口完全公开:" }
                    pre { code { "{api_curl}" } }
                    p {
                        code { "GET /api/v1/rates" }
                        " 支持 " code { "chain" } " / " code { "asset" } " / " code { "protocol" } " / APY、utilization 范围筛选;"
                        "另有 " code { "/api/v1/chains" } "、" code { "/api/v1/protocols" } "、" code { "/api/v1/pools" } "。"
                    }
                }
                footer { class: "footer",
                    a { href: "/", "← 返回首页" }
                }
            }
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html_shell("使用文档 — APY MCP", body),
    )
        .into_response()
}

/// GET / — SSR query page. Query string = filters (form submits here).
pub async fn index_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(raw): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let mut filters = QueryRatesParams {
        action: "query".to_string(),
        chain: raw.get("chain").filter(|s| !s.is_empty()).cloned(),
        asset: raw.get("asset").filter(|s| !s.is_empty()).cloned(),
        protocol: raw.get("protocol").filter(|s| !s.is_empty()).cloned(),
        pool_id: None,
        min_supply_apy: raw.get("min_supply_apy").and_then(|s| s.parse::<f64>().ok()).map(|v| v / 100.0),
        max_supply_apy: raw.get("max_supply_apy").and_then(|s| s.parse::<f64>().ok()).map(|v| v / 100.0),
        min_borrow_apy: None,
        max_borrow_apy: None,
        min_utilization: raw.get("min_utilization").and_then(|s| s.parse::<f64>().ok()).map(|v| v / 100.0),
        max_utilization: None,
        use_cache: true,
        cache_only: false,
    };
    // Percent inputs come as percents (e.g. "5" = 5%); normalize empties
    if filters.protocol.as_deref() == Some("all") {
        filters.protocol = None;
    }

    let is_initial = raw.is_empty();
    let force_refresh = raw
        .get("force_refresh")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Cache policy:
    //   * Initial visit (`/` with no params) — never block on the network.
    //   * htmx sort-header click — pure re-order, just re-render the cache.
    //   * Form submit (`force_refresh=1`) — return cached data instantly
    //     and kick a background refresh; user sees data immediately, the
    //     next page load gets fresh data once the background task lands.
    let cache_only = is_initial || !force_refresh;
    filters.cache_only = cache_only;

    let (response, notice, from_cache_only) = if is_initial {
        initial_page_data(&state).await
    } else if cache_only {
        cached_fallback(&state, filters.clone(), None).await
    } else if let Some(cached) = cached_data_snapshot(&state).await {
        // force_refresh=1: serve cached instantly, refresh in background.
        let svc = state.service.clone();
        let bg_filters = filters.clone();
        tokio::spawn(async move {
            let mut f = bg_filters;
            f.cache_only = false;
            if let Err(e) = svc.query(&f).await {
                tracing::warn!(error = %e, "background rate refresh failed");
            }
        });
        (cached, Some("后台刷新中…".to_string()), true)
    } else {
        // Cache cold AND force_refresh: have to wait. Bounded timeout,
        // fall back to whatever the timeout returns (empty if both fail).
        let queried = tokio::time::timeout(PAGE_QUERY_TIMEOUT, state.service.query(&filters)).await;
        match queried {
            Ok(Ok(resp)) => (resp, None, false),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "page query failed");
                cached_fallback(&state, filters.clone(), Some(format!("查询失败:{e}"))).await
            }
            Err(_) => {
                tracing::warn!("page query timed out");
                cached_fallback(&state, filters.clone(), Some("查询超时,已显示缓存数据".to_string())).await
            }
        }
    };

    let sort_field = parse_sort(raw.get("sort").map(|s| s.as_str())).unwrap_or(DEFAULT_SORT.field);
    let sort_dir = parse_dir(raw.get("dir").map(|s| s.as_str()));
    let sort = SortSpec {
        field: sort_field,
        dir: sort_dir,
    };

    let page_data = PageData {
        filters,
        response,
        notice,
        from_cache_only,
        sort,
    };

    // htmx partial-refresh path: return just the swap-able region, no document
    // chrome. htmx picks up the fragment via HX-Request header and replaces
    // `#results-region` in place — no full reload, no scroll jump, filter
    // form state preserved.
    if is_htmx_request(&headers, &raw) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_results_only(&page_data),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_page(&page_data),
    )
        .into_response()
}

/// True when the caller asked for just the `#results-region` fragment.
/// Browsers using htmx set `HX-Request: true` automatically; we also accept
/// an explicit `_fragment=results` URL param so curl / no-JS clients can opt in.
fn is_htmx_request(
    headers: &HeaderMap,
    raw: &std::collections::HashMap<String, String>,
) -> bool {
    if headers
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    raw.get("_fragment")
        .map(|s| s == "results")
        .unwrap_or(false)
}

/// Initial visit: serve straight from the 120s cache without touching the
/// network. On a cold cache, kick a background refresh so the next load has
/// data, and tell the user to refresh shortly.
async fn initial_page_data(state: &HttpState) -> (AllRatesResponse, Option<String>, bool) {
    let filters = QueryRatesParams {
        action: "query".to_string(),
        chain: None,
        asset: None,
        protocol: None,
        pool_id: None,
        min_supply_apy: None,
        max_supply_apy: None,
        min_borrow_apy: None,
        max_borrow_apy: None,
        min_utilization: None,
        max_utilization: None,
        use_cache: true,
        cache_only: true,
    };
    let cached = tokio::time::timeout(CACHE_READ_TIMEOUT, state.service.query(&filters)).await;
    match cached {
        Ok(Ok(resp)) if !resp.pools.is_empty() => (resp, None, true),
        _ => {
            let svc = state.service.clone();
            tokio::spawn(async move {
                let mut f = filters;
                f.cache_only = false;
                if let Err(e) = svc.query(&f).await {
                    tracing::warn!(error = %e, "background rate refresh failed");
                }
            });
            (
                AllRatesResponse {
                    pools: Vec::new(),
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                },
                Some("首次加载：正在后台拉取数据，约 10 秒后刷新页面即可看到结果".to_string()),
                true,
            )
        }
    }
}

/// Serve unfiltered data straight from the cache (no network).
async fn cached_fallback(
    state: &HttpState,
    mut filters: QueryRatesParams,
    notice: Option<String>,
) -> (AllRatesResponse, Option<String>, bool) {
    filters.chain = None;
    filters.asset = None;
    filters.protocol = None;
    filters.min_supply_apy = None;
    filters.max_supply_apy = None;
    filters.min_utilization = None;
    filters.cache_only = true;
    match state.service.query(&filters).await {
        Ok(resp) if !resp.pools.is_empty() => (resp, notice, true),
        _ => (
            AllRatesResponse {
                pools: Vec::new(),
                fetched_at: chrono::Utc::now().to_rfc3339(),
            },
            notice,
            true,
        ),
    }
}

/// Quick read of the unfiltered cache snapshot — used by force_refresh=1
/// to serve cached data instantly while a background refresh runs. Returns
/// None if the cache is cold (caller should fall through to the real
/// query path with the 3s timeout).
async fn cached_data_snapshot(state: &HttpState) -> Option<AllRatesResponse> {
    let filters = QueryRatesParams {
        action: "query".to_string(),
        chain: None,
        asset: None,
        protocol: None,
        pool_id: None,
        min_supply_apy: None,
        max_supply_apy: None,
        min_borrow_apy: None,
        max_borrow_apy: None,
        min_utilization: None,
        max_utilization: None,
        use_cache: true,
        cache_only: true,
    };
    match tokio::time::timeout(CACHE_READ_TIMEOUT, state.service.query(&filters)).await {
        Ok(Ok(resp)) if !resp.pools.is_empty() => Some(resp),
        _ => None,
    }
}

/// Fallback for unmatched paths: static assets, or the SSR page for `/`.
pub async fn fallback_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return index_handler(State(state), headers, Query(q)).await;
    }
    if path.contains("..") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found",
        )
            .into_response();
    }
    asset_handler(Path(path.to_string())).await
}

/// GET /{file} — embedded static assets (app.js, style.css, ...).
pub async fn asset_handler(Path(path): Path<String>) -> Response {
    serve_file(&path).await
}

async fn serve_file(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime_type(path))],
            content.data.to_vec(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::types::AssetRate;

    pub(crate) fn asset(name: &str, supply_apy: f64) -> AssetRate {
        AssetRate {
            asset_id: name.to_string(),
            asset_name: name.to_string(),
            decimals: 8,
            supply_apr: supply_apy,
            supply_apy,
            borrow_apr: 0.05,
            borrow_apy: 0.05,
            utilization: 0.5,
            total_supplied: 1.0e6,
            total_borrowed: 5.0e5,
        }
    }

    pub(crate) fn pool(protocol: &str, chain: &str, assets: Vec<AssetRate>) -> PoolRates {
        PoolRates {
            chain: chain.to_string(),
            protocol: protocol.to_string(),
            pool_id: format!("{protocol}-{chain}"),
            pool_name: format!("{protocol} {chain}"),
            timestamp: String::new(),
            assets,
        }
    }

    #[test]
    fn test_build_panels_groups_and_sorts() {
        let pools = vec![
            pool(
                "aave_v3",
                "ethereum",
                vec![asset("USDT", 0.03), asset("USDC", 0.05), asset("WETH", 0.01)],
            ),
            pool("aave_v3", "base", vec![asset("USDC", 0.04)]),
            pool("aave_v3", "arbitrum", vec![asset("USDC", 0.045)]),
            pool("spark", "ethereum", vec![asset("USDC", 0.045)]),
            pool("blend", "stellar", vec![asset("XLM", 0.0)]),
        ];
        // build_panels no longer sorts assets — that's the page renderer's
        // job (it applies the active sort before calling build_panels). Sort
        // here so the assertion still proves the panels shape, not ordering.
        for pool in pools.iter() {
            sort_assets(&mut pool.assets.clone(), DEFAULT_SORT.field, DEFAULT_SORT.dir);
        }
        let mut pools = pools;
        for pool in pools.iter_mut() {
            sort_assets(&mut pool.assets, DEFAULT_SORT.field, DEFAULT_SORT.dir);
        }
        let panels = build_panels(&pools);

        // Protocol order: known protocols first, in PROTO_ORDER.
        let names: Vec<&str> = panels.iter().map(|p| p.protocol.as_str()).collect();
        assert_eq!(names, vec!["aave_v3", "spark", "blend"]);
        assert_eq!(panels[0].label, "Aave V3");

        // Chains follow CHAIN_ORDER, not insertion order.
        let chains: Vec<&str> = panels[0]
            .chains
            .iter()
            .map(|c| c.chain.as_str())
            .collect();
        assert_eq!(chains, vec!["ethereum", "base", "arbitrum"]);

        // Assets within a pool are sorted by supply APY descending.
        let asset_names: Vec<&str> = panels[0].chains[0].pools[0]
            .assets
            .iter()
            .map(|a| a.asset_name.as_str())
            .collect();
        assert_eq!(asset_names, vec!["USDC", "USDT", "WETH"]);
    }

    #[test]
    fn test_build_panels_skips_empty_protocols() {
        let pools = vec![pool("spark", "ethereum", vec![asset("USDC", 0.05)])];
        let panels = build_panels(&pools);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].protocol, "spark");
    }
}

#[cfg(test)]
mod render_tests {
    use super::tests::{asset, pool};
    use super::*;

    #[test]
    fn test_results_tabs_render_subtabs() {
        let pools = vec![
            pool("aave_v3", "ethereum", vec![asset("USDC", 0.05)]),
            pool("aave_v3", "base", vec![asset("USDC", 0.04)]),
            pool("spark", "ethereum", vec![asset("USDC", 0.045)]),
        ];
        let html = dioxus::ssr::render_element(rsx! {
            ResultsTabs { pools: pools, sort: DEFAULT_SORT, filters: QueryRatesParams { action: "query".into(), chain: None, asset: None, protocol: None, pool_id: None, min_supply_apy: None, max_supply_apy: None, min_borrow_apy: None, max_borrow_apy: None, min_utilization: None, max_utilization: None, use_cache: true, cache_only: false } }
        });
        // Protocol tab bar with both protocols
        assert!(html.contains(r#"data-tab="aave_v3""#), "missing aave tab: {html}");
        assert!(html.contains(r#"data-tab="spark""#), "missing spark tab: {html}");
        // Chain subtabs inside the aave panel
        assert!(html.contains(r#"class="subtab active""#), "missing active subtab: {html}");
        assert!(html.contains(r#"data-chain="ethereum""#), "missing ethereum subtab: {html}");
        assert!(html.contains(r#"data-chain="base""#), "missing base subtab: {html}");
        // Exactly one default-active panel/tab
        assert_eq!(html.matches("tab-panel active").count(), 1);
        assert_eq!(html.matches(r#"class="tab active""#).count(), 1);
        // Gauge + hover tooltip present
        assert!(html.contains("gauge-fill"), "missing gauge: {html}");
        assert!(html.contains("tip-box"), "missing tooltip: {html}");
        // Spark has a single chain -> no subtabs in its panel
        let spark_panel = html.split(r#"data-panel="spark""#).nth(1).unwrap();
        assert!(!spark_panel.contains("subtab"), "spark should not render subtabs");
    }

    #[test]
    fn test_short_timestamp_falls_back() {
        // Plain passthrough when the input is not RFC 3339.
        assert_eq!(short_timestamp("not a date"), "not a date");
    }
}
