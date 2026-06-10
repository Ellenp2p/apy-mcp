/// Blend Capital 3-segment interest rate model.
///
/// The borrow rate is calculated based on utilization:
///
///   utilization = total_liabilities / total_supply
///
///   if util <= target_util:
///     IR = (r_base + r_one * util / target_util) * modifier
///
///   elif util <= 0.95:
///     IR = (r_base + r_one + r_two * (util - target) / (0.95 - target)) * modifier
///
///   else:
///     IR = (r_base + r_one + r_two + r_three * (util - 0.95) / 0.05) * modifier
///
/// All rate values are stored as fixed-point with 7 decimal places (1e7 = 100%).

const SCALAR_7: f64 = 1e7;
const IR_MOD_SCALAR: f64 = 1e7; // interestRateModifier uses 7 decimals
const FIXED_95_PERCENT: u64 = 9_500_000;
const FIXED_5_PERCENT: u64 = 500_000;

/// Configuration parameters for a Blend reserve
#[derive(Debug, Clone)]
pub struct ReserveConfig {
    /// Reserve index in the pool
    pub index: u32,
    /// Asset decimals
    pub decimals: u32,
    /// Collateral factor (fixed 7 decimals)
    pub c_factor: u32,
    /// Liability factor (fixed 7 decimals)
    pub l_factor: u32,
    /// Target utilization (fixed 7 decimals)
    pub util: u32,
    /// Max utilization (fixed 7 decimals)
    pub max_util: u32,
    /// Base rate (fixed 7 decimals)
    pub r_base: u32,
    /// Rate slope 1 (fixed 7 decimals)
    pub r_one: u32,
    /// Rate slope 2 (fixed 7 decimals)
    pub r_two: u32,
    /// Rate slope 3 (fixed 7 decimals)
    pub r_three: u32,
    /// Reactivity coefficient
    pub reactivity: u32,
}

/// Live data for a Blend reserve
#[derive(Debug, Clone)]
pub struct ReserveData {
    /// Total bToken supply (raw, needs b_rate conversion for actual amount)
    pub b_supply: u128,
    /// Total dToken supply (raw, needs d_rate conversion for actual amount)
    pub d_supply: u128,
    /// Interest rate modifier (fixed 7 decimals)
    pub interest_rate_modifier: u64,
    /// bToken exchange rate (fixed 12 decimals for V2)
    pub b_rate: u128,
    /// dToken exchange rate (fixed 12 decimals for V2)
    pub d_rate: u128,
    /// Last accrual timestamp
    pub last_accrual: u64,
}

/// Computed interest rates for a reserve
#[derive(Debug, Clone)]
pub struct InterestRates {
    /// Borrow APR (as a decimal, e.g. 0.05 = 5%)
    pub borrow_apr: f64,
    /// Borrow APY (daily compounding)
    pub borrow_apy: f64,
    /// Supply APR (as a decimal)
    pub supply_apr: f64,
    /// Supply APY (weekly compounding)
    pub supply_apy: f64,
    /// Current utilization (0.0 - 1.0)
    pub utilization: f64,
    /// Total supply in underlying asset units (e.g. USDC)
    pub total_supplied: f64,
    /// Total borrowed in underlying asset units
    pub total_borrowed: f64,
}

/// Calculate the current borrow APR based on the 3-segment model.
fn calc_borrow_apr(config: &ReserveConfig, data: &ReserveData) -> f64 {
    // Convert bTokens/dTokens to actual asset amounts using exchange rates
    let b_supply_tokens = to_token_amount(data.b_supply, data.b_rate, config.decimals);
    let d_supply_tokens = to_token_amount(data.d_supply, data.d_rate, config.decimals);

    if b_supply_tokens == 0 {
        return to_float(config.r_base as u64, 7);
    }

    let cur_util = div_ceil(d_supply_tokens, b_supply_tokens, 10_000_000u64);
    let target_util = config.util as u64;
    let ir_modifier = data.interest_rate_modifier;

    let cur_ir: u64;

    if cur_util <= target_util {
        // Segment 1: below target utilization
        let util_scalar = div_ceil(cur_util, target_util, 10_000_000u64);
        let base_rate =
            mul_ceil(util_scalar, config.r_one as u64, 10_000_000u64) + config.r_base as u64;
        cur_ir = mul_ceil(base_rate, ir_modifier, 10_000_000u64);
    } else if cur_util <= FIXED_95_PERCENT {
        // Segment 2: between target and 95%
        let util_scalar = div_ceil(
            cur_util - target_util,
            FIXED_95_PERCENT - target_util,
            10_000_000u64,
        );
        let base_rate = mul_ceil(util_scalar, config.r_two as u64, 10_000_000u64)
            + config.r_one as u64
            + config.r_base as u64;
        cur_ir = mul_ceil(base_rate, ir_modifier, 10_000_000u64);
    } else {
        // Segment 3: above 95% (kink)
        let util_scalar = div_ceil(cur_util - FIXED_95_PERCENT, FIXED_5_PERCENT, 10_000_000u64);
        let extra_rate = mul_ceil(util_scalar, config.r_three as u64, 10_000_000u64);
        let base_rate =
            extra_rate + config.r_two as u64 + config.r_one as u64 + config.r_base as u64;
        cur_ir = mul_ceil(base_rate, ir_modifier, 10_000_000u64);
    }

    to_float(cur_ir, 7)
}

/// Project ir_mod, bRate, dRate forward to current time.
///
/// Matches the Blend SDK's `accrue()` logic:
///   ir_mod decreases when util < target, increases when util > target
///   bRate/dRate increase based on borrow APR accrual
pub fn project_to_now(
    data: &ReserveData,
    config: &ReserveConfig,
    now_timestamp: u64,
) -> (u64, u128, u128) {
    let delta = now_timestamp.saturating_sub(data.last_accrual);
    if delta == 0 || data.b_supply == 0 {
        return (data.interest_rate_modifier, data.b_rate, data.d_rate);
    }

    let seconds_per_year: u128 = 31_536_000;
    let scalar_7: u128 = 10_000_000;

    // ── Step 1: Project ir_mod ──
    let target = config.util as u64;
    let cur_util = calc_current_util(data);
    let reactivity = config.reactivity as u128;

    let mut new_ir_mod = data.interest_rate_modifier as u128;

    if cur_util < target {
        // util below target → ir_mod decreases
        let util_dif = (target - cur_util) as u128; // already in fixed-7
        let util_error = delta as u128 * util_dif; // seconds × fixed-7
                                                   // reactivity is also fixed-7, so divide by 10^7 twice:
                                                   // rate_dif = util_error × reactivity / 10^7 / 10^7
        let rate_dif = (util_error * reactivity + scalar_7 * scalar_7 - 1) / (scalar_7 * scalar_7);
        new_ir_mod = new_ir_mod.saturating_sub(rate_dif);
        // Floor: ir_mod min = 0.1 (1_000_000 in fixed-7)
        if new_ir_mod < 1_000_000 {
            new_ir_mod = 1_000_000;
        }
    } else if cur_util > target {
        // util above target → ir_mod increases
        let util_dif = (cur_util - target) as u128;
        let util_error = delta as u128 * util_dif;
        let rate_dif = util_error * reactivity / (scalar_7 * scalar_7);
        new_ir_mod += rate_dif;
        // Cap: ir_mod max = 10.0 (100_000_000 in fixed-7)
        if new_ir_mod > 100_000_000 {
            new_ir_mod = 100_000_000;
        }
    }

    // ── Step 2: Calculate borrow APR with projected ir_mod ──
    let projected_data = ReserveData {
        interest_rate_modifier: new_ir_mod as u64,
        ..data.clone()
    };
    let borrow_apr = calc_borrow_apr(config, &projected_data);

    // ── Step 3: Project bRate/dRate ──
    let apr_fixed = (borrow_apr * 10_000_000.0) as u128;
    let accrual = data.b_rate * apr_fixed * delta as u128 / (10_000_000 * seconds_per_year);

    let new_b_rate = data.b_rate + accrual;
    let new_d_rate = data.d_rate + accrual;

    (new_ir_mod as u64, new_b_rate, new_d_rate)
}

/// Calculate current utilization from raw bSupply/dSupply and rates
fn calc_current_util(data: &ReserveData) -> u64 {
    if data.b_supply == 0 {
        return 0;
    }
    // util = (dSupply × dRate) / (bSupply × bRate) in fixed-7
    let numerator = data.d_supply.saturating_mul(data.d_rate);
    let denominator = data.b_supply.saturating_mul(data.b_rate);
    if denominator == 0 {
        return 0;
    }
    (numerator * 10_000_000 / denominator) as u64
}

/// Convert raw token amount using exchange rate.
///
/// `raw_amount` is the bToken/dToken quantity (e.g. bSupply)
/// `rate` is the exchange rate (e.g. bRate, starting at 10^rateDecimals)
/// `asset_decimals` is the number of decimals for the underlying asset (e.g. 7 for USDC)
///
/// For V2: rateDecimals=12, so rate starts at 10^12 (=1.0 in fixed-point)
/// Result: raw_amount * rate / 10^rateDecimals / 10^asset_decimals (in human units)
fn to_token_amount(raw_amount: u128, rate: u128, asset_decimals: u32) -> u64 {
    let rate_scaler = 10u128.pow(12); // V2 uses 12 decimals for rate
    let asset_scaler = 10u128.pow(asset_decimals);
    // amount_in_units = raw_amount * rate / rate_scaler
    let amount = raw_amount.saturating_mul(rate) / rate_scaler;
    // Return as raw with asset decimals (so we can use it in fixed-7 math)
    (amount / asset_scaler * 10_000_000 + amount % asset_scaler * 10_000_000 / asset_scaler) as u64
}

/// Calculate all interest rates for a reserve.
///
/// `backstop_rate` is the portion of interest that goes to the backstop (fixed 7 decimals).
/// `now_timestamp` is the current Unix timestamp for accrual projection.
pub fn calculate_rates(
    config: &ReserveConfig,
    data: &ReserveData,
    backstop_rate: u32,
    now_timestamp: u64,
) -> InterestRates {
    // Project ir_mod, bRate, dRate to current time
    let (projected_ir_mod, projected_b_rate, projected_d_rate) =
        project_to_now(data, config, now_timestamp);

    // Convert bTokens/dTokens to actual asset amounts using exchange rates
    let total_supplied = to_token_amount(data.b_supply, projected_b_rate, config.decimals);
    let total_borrowed = to_token_amount(data.d_supply, projected_d_rate, config.decimals);

    // Utilization
    let utilization = if total_supplied == 0 {
        0.0
    } else {
        total_borrowed as f64 / total_supplied as f64
    };

    // Borrow APR (using projected ir_mod)
    let projected_data = ReserveData {
        interest_rate_modifier: projected_ir_mod,
        ..data.clone()
    };
    let borrow_apr = calc_borrow_apr(config, &projected_data);

    // Supply APR = borrow_apr * utilization * (1 - backstop_rate)
    let backstop_rate_f = to_float(backstop_rate as u64, 7);
    let supply_apr = borrow_apr * utilization * (1.0 - backstop_rate_f);

    // APY: borrow compounds daily, supply compounds weekly
    let borrow_apy = apr_to_apy(borrow_apr, 365);
    let supply_apy = apr_to_apy(supply_apr, 52);

    InterestRates {
        borrow_apr,
        borrow_apy,
        supply_apr,
        supply_apy,
        utilization,
        total_supplied: total_supplied as f64 / 10_000_000.0,
        total_borrowed: total_borrowed as f64 / 10_000_000.0,
    }
}

/// Convert APR to APY with given compounding periods per year
fn apr_to_apy(apr: f64, periods_per_year: u32) -> f64 {
    let n = periods_per_year as f64;
    (1.0 + apr / n).powf(n) - 1.0
}

/// Fixed-point multiply with ceiling: (a * b) / scalar
fn mul_ceil(a: u64, b: u64, scalar: u64) -> u64 {
    let product = (a as u128) * (b as u128);
    ((product + scalar as u128 - 1) / scalar as u128) as u64
}

/// Fixed-point divide with ceiling: (a * scalar) / b
fn div_ceil(a: u64, b: u64, scalar: u64) -> u64 {
    if b == 0 {
        return 0;
    }
    let numerator = (a as u128) * (scalar as u128);
    ((numerator + b as u128 - 1) / b as u128) as u64
}

/// Convert fixed-point value with given decimals to float
fn to_float(value: u64, decimals: u32) -> f64 {
    let divisor = 10u64.pow(decimals) as f64;
    value as f64 / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default test config (same as Blend V2 typical pool)
    fn make_test_config() -> ReserveConfig {
        ReserveConfig {
            index: 0,
            decimals: 7,
            c_factor: 9_000_000,  // 0.9
            l_factor: 10_000_000, // 1.0
            util: 9_000_000,      // 90%
            max_util: 10_000_000, // 100%
            r_base: 100_000,      // 1%
            r_one: 4_000_000,     // 40%
            r_two: 50_000_000,    // 500%
            r_three: 200_000_000, // 2000%
            reactivity: 100,
        }
    }

    // ── 基础利率模型测试 ──────────────────────────────────────────────

    #[test]
    fn test_below_target_util() {
        let config = make_test_config();
        let data = ReserveData {
            b_supply: 1_000_000_000_000,
            d_supply: 500_000_000_000,
            interest_rate_modifier: 10_000_000,
            b_rate: 1_000_000_000_000, // 1.0
            d_rate: 1_000_000_000_000,
            last_accrual: 0,
        };
        let rates = calculate_rates(&config, &data, 0, 0);
        assert!(rates.borrow_apr > 0.0);
        assert!(rates.borrow_apr < 0.5);
        assert!(rates.utilization > 0.49 && rates.utilization < 0.51);
    }

    #[test]
    fn test_above_target_util() {
        let config = make_test_config();
        let data = ReserveData {
            b_supply: 1_000_000_000_000,
            d_supply: 920_000_000_000,
            interest_rate_modifier: 10_000_000,
            b_rate: 1_000_000_000_000,
            d_rate: 1_000_000_000_000,
            last_accrual: 0,
        };
        let rates = calculate_rates(&config, &data, 0, 0);
        assert!(rates.borrow_apr > 0.04);
    }

    #[test]
    fn test_kink_above_95() {
        let config = make_test_config();
        let data = ReserveData {
            b_supply: 1_000_000_000_000,
            d_supply: 980_000_000_000,
            interest_rate_modifier: 10_000_000,
            b_rate: 1_000_000_000_000,
            d_rate: 1_000_000_000_000,
            last_accrual: 0,
        };
        let rates = calculate_rates(&config, &data, 0, 0);
        assert!(rates.borrow_apr > 1.0);
    }

    #[test]
    fn test_zero_supply() {
        let config = make_test_config();
        let data = ReserveData {
            b_supply: 0,
            d_supply: 0,
            interest_rate_modifier: 10_000_000,
            b_rate: 1_000_000_000_000,
            d_rate: 1_000_000_000_000,
            last_accrual: 0,
        };
        let rates = calculate_rates(&config, &data, 0, 0);
        assert_eq!(rates.borrow_apr, to_float(config.r_base as u64, 7));
        assert!(rates.utilization < 0.001);
    }

    // ── Blend USDC 实际链上数据测试 ───────────────────────────────────

    /// USDC on Blend Capital mainnet pool (CCW67T...MI75)
    ///
    /// 完整计算公式:
    ///   1. projected_ir_mod = ir_mod + Δt × (target - util) × reactivity / 10^14
    ///   2. projected_bRate = bRate + bRate × borrowAPR × Δt / 31536000
    ///   3. total_supplied = bSupply × projected_bRate / 10^12
    ///   4. total_borrowed = dSupply × projected_dRate / 10^12
    ///   5. utilization = total_borrowed / total_supplied
    ///   6. borrow_apr = projected_ir_mod × (r_base + r_one × util/target)
    ///   7. supply_apr = borrow_apr × utilization × (1 - backstop_rate)
    ///   8. borrow_apy = (1 + borrow_apr/365)^365 - 1
    ///   9. supply_apy = (1 + supply_apr/52)^52 - 1
    #[test]
    fn test_usdc_onchain_rates() {
        let config = ReserveConfig {
            index: 1,
            decimals: 7,
            c_factor: 9_500_000,
            l_factor: 9_500_000,
            util: 8_000_000, // target 80%
            max_util: 9_000_000,
            r_base: 300_000,     // 3.0%
            r_one: 400_000,      // 4.0%
            r_two: 1_200_000,    // 12.0%
            r_three: 50_000_000, // 500%
            reactivity: 20,
        };

        let data = ReserveData {
            b_supply: 435_952_303_572_293,
            d_supply: 311_934_385_694_036,
            interest_rate_modifier: 19_040_382, // 1.904
            b_rate: 1_015_872_236_207,          // ≈1.0159
            d_rate: 1_003_746_572_908,          // ≈1.0037
            last_accrual: 1_781_023_807,
        };

        let backstop_rate: u32 = 2_000_000; // 20% (from pool instance storage)
        let now = data.last_accrual + 3600; // simulate 1 hour later
        let rates = calculate_rates(&config, &data, backstop_rate, now);

        println!("═══════════════════════════════════════════════════════════════");
        println!("  Blend USDC Pool 利率计算");
        println!("═══════════════════════════════════════════════════════════════");
        println!();
        println!("  链上参数:");
        println!(
            "    r_base={}  r_one={}  target={}  backstop={:.1}%",
            to_float(config.r_base as u64, 7) * 100.0,
            to_float(config.r_one as u64, 7) * 100.0,
            to_float(config.util as u64, 7) * 100.0,
            backstop_rate as f64 / 1e7 * 100.0
        );
        println!(
            "    ir_mod={:.4}  util={:.2}%",
            data.interest_rate_modifier as f64 / 1e7,
            rates.utilization * 100.0
        );
        println!();
        println!("  计算:");
        println!("    borrow_apr = ir_mod × (r_base + r_one × util/target)");
        println!(
            "              = {:.4} × ({:.2}% + {:.2}% × {:.4})",
            data.interest_rate_modifier as f64 / 1e7,
            to_float(config.r_base as u64, 7) * 100.0,
            to_float(config.r_one as u64, 7) * 100.0,
            rates.utilization / to_float(config.util as u64, 7)
        );
        println!("              = {:.2}%", rates.borrow_apr * 100.0);
        println!("    supply_apr = borrow_apr × util × (1 - backstop)");
        println!(
            "              = {:.2}% × {:.1}% × (1 - {:.0}%)",
            rates.borrow_apr * 100.0,
            rates.utilization * 100.0,
            backstop_rate as f64 / 1e7 * 100.0
        );
        println!("              = {:.2}%", rates.supply_apr * 100.0);
        println!(
            "    borrow_apy = (1 + {:.2}%/365)^365 - 1 = {:.2}%",
            rates.borrow_apr * 100.0,
            rates.borrow_apy * 100.0
        );
        println!(
            "    supply_apy = (1 + {:.2}%/52)^52 - 1  = {:.2}%",
            rates.supply_apr * 100.0,
            rates.supply_apy * 100.0
        );
        println!();
        println!("  结果:");
        println!("    总供应: {:.2} USDC", rates.total_supplied);
        println!("    总借出: {:.2} USDC", rates.total_borrowed);
        // Supply APY >= Supply APR (复利)

        assert!(
            rates.supply_apy >= rates.supply_apr,
            "Supply APY ({:.4}%) should be >= Supply APR ({:.4}%)",
            rates.supply_apy * 100.0,
            rates.supply_apr * 100.0
        );

        // Borrow APY 应该高于 Borrow APR (因为日复利)
        assert!(
            rates.borrow_apy >= rates.borrow_apr,
            "Borrow APY ({:.4}%) should be >= Borrow APR ({:.4}%)",
            rates.borrow_apy * 100.0,
            rates.borrow_apr * 100.0
        );
    }

    // ── 不同利用率下的利率曲线测试 ─────────────────────────────────────

    #[test]
    fn test_interest_rate_curve() {
        let config = make_test_config();
        let base_supply: u128 = 1_000_000_000; // 100 tokens

        println!("\n  利率曲线 (target=90%, r_base=1%, r_one=40%):");
        println!(
            "  {:>8}  {:>10}  {:>10}  {:>10}",
            "Util%", "BorrowAPR", "BorrowAPY", "SupplyAPR"
        );
        println!("  {}", "-".repeat(42));

        for util_pct in [10, 20, 30, 40, 50, 60, 70, 80, 85, 90, 92, 95, 98, 100] {
            let b_supply = base_supply;
            let d_supply = base_supply * util_pct as u128 / 100;
            let data = ReserveData {
                b_supply,
                d_supply,
                interest_rate_modifier: 10_000_000,
                b_rate: 1_000_000_000_000,
                d_rate: 1_000_000_000_000,
                last_accrual: 0,
            };
            let rates = calculate_rates(&config, &data, 0, 0);
            println!(
                "  {:>7}%  {:>9.4}%  {:>9.4}%  {:>9.4}%",
                util_pct,
                rates.borrow_apr * 100.0,
                rates.borrow_apy * 100.0,
                rates.supply_apr * 100.0
            );
        }
    }

    // ── Backstop rate 影响测试 ────────────────────────────────────────

    #[test]
    fn test_backstop_rate_effect() {
        let config = make_test_config();
        let data = ReserveData {
            b_supply: 1_000_000_000_000,
            d_supply: 700_000_000_000,
            interest_rate_modifier: 10_000_000,
            b_rate: 1_000_000_000_000,
            d_rate: 1_000_000_000_000,
            last_accrual: 0,
        };

        println!("\n  Backstop Rate 对 Supply APR 的影响 (util=70%):");
        println!(
            "  {:>12}  {:>12}  {:>12}",
            "Backstop%", "SupplyAPR", "SupplyAPY"
        );
        println!("  {}", "-".repeat(38));

        for bs_pct in [0, 5, 10, 20, 50, 100] {
            let backstop = bs_pct * 100_000; // 7 decimals
            let rates = calculate_rates(&config, &data, backstop, 0);
            println!(
                "  {:>11}%  {:>11.4}%  {:>11.4}%",
                bs_pct,
                rates.supply_apr * 100.0,
                rates.supply_apy * 100.0
            );
        }
    }

    // ── APR vs APY 复利计算验证 ───────────────────────────────────────

    #[test]
    fn test_apr_to_apy_conversion() {
        println!("\n  APR → APY 转换 (复利效果):");
        println!(
            "  {:>8}  {:>12}  {:>12}  {:>8}",
            "APR", "BorrowAPY", "SupplyAPY", "Diff%"
        );
        println!("  {}", "-".repeat(44));

        for apr in [0.01, 0.05, 0.10, 0.15, 0.20, 0.50, 1.0] {
            let borrow_apy = apr_to_apy(apr, 365); // 日复利
            let supply_apy = apr_to_apy(apr, 52); // 周复利
            println!(
                "  {:>7.1}%  {:>11.4}%  {:>11.4}%  {:>7.4}%",
                apr * 100.0,
                borrow_apy * 100.0,
                supply_apy * 100.0,
                (borrow_apy - supply_apy) * 100.0
            );
        }
    }
}
