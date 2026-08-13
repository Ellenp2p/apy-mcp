/// Aave V3 Variable Interest Rate Model
///
/// The variable borrow rate is calculated based on utilization:
///
///   utilization = total_debt / total_supply
///
///   if util <= optimal_utilization:
///     variableRate = base_rate + (util / optimal_util) * slope1
///
///   else:
///     variableRate = base_rate + slope1 + ((util - optimal_util) / (1 - optimal_util)) * slope2
///
/// All rate values are scaled by 1e27 (RAY) in the contract.
/// Supply rate = borrow_rate * util * (1 - reserve_factor)

/// RAY = 1e27 (Aave's fixed-point precision)
const RAY: f64 = 1e27;

/// WAD = 1e18
const WAD: f64 = 1e18;

/// Aave V3 default interest rate strategy parameters
/// These are the typical values used by Aave V3
#[derive(Clone)]
pub struct InterestRateParams {
    /// Base variable borrow rate (scaled by RAY)
    pub base_variable_borrow_rate: f64,
    /// Slope 1 - rate increase up to optimal utilization (scaled by RAY)
    pub variable_rate_slope1: f64,
    /// Slope 2 - rate increase above optimal utilization (scaled by RAY)
    pub variable_rate_slope2: f64,
    /// Optimal utilization rate (scaled by RAY, typically 0.8 * RAY)
    pub optimal_utilization: f64,
}

/// Default interest rate parameters for different asset categories
pub fn default_rate_params() -> Vec<(&'static str, InterestRateParams)> {
    vec![
        // Stablecoins (USDC, USDT, DAI, etc.)
        (
            "stablecoin",
            InterestRateParams {
                base_variable_borrow_rate: 0.0 * RAY,
                variable_rate_slope1: 0.04 * RAY, // 4%
                variable_rate_slope2: 0.75 * RAY, // 75%
                optimal_utilization: 0.90 * RAY,  // 90%
            },
        ),
        // ETH/STETH
        (
            "eth",
            InterestRateParams {
                base_variable_borrow_rate: 0.01 * RAY, // 1%
                variable_rate_slope1: 0.033 * RAY,     // 3.3%
                variable_rate_slope2: 0.80 * RAY,      // 80%
                optimal_utilization: 0.90 * RAY,       // 90%
            },
        ),
        // BTC (WBTC, cbBTC, etc.)
        (
            "btc",
            InterestRateParams {
                base_variable_borrow_rate: 0.0 * RAY,
                variable_rate_slope1: 0.04 * RAY, // 4%
                variable_rate_slope2: 0.75 * RAY, // 75%
                optimal_utilization: 0.45 * RAY,  // 45%
            },
        ),
        // Other volatile assets (LINK, AAVE, UNI, etc.)
        (
            "volatile",
            InterestRateParams {
                base_variable_borrow_rate: 0.0 * RAY,
                variable_rate_slope1: 0.05 * RAY, // 5%
                variable_rate_slope2: 0.80 * RAY, // 80%
                optimal_utilization: 0.65 * RAY,  // 65%
            },
        ),
        // Low utilization assets (crvCVX, GHO, etc.)
        (
            "low_util",
            InterestRateParams {
                base_variable_borrow_rate: 0.0 * RAY,
                variable_rate_slope1: 0.03 * RAY, // 3%
                variable_rate_slope2: 0.80 * RAY, // 80%
                optimal_utilization: 0.80 * RAY,  // 80%
            },
        ),
    ]
}

/// Calculate variable borrow rate from utilization and rate parameters
pub fn calculate_variable_borrow_rate(
    utilization: f64, // 0.0 - 1.0
    params: &InterestRateParams,
) -> f64 {
    let util_ray = utilization * RAY;
    let optimal = params.optimal_utilization;

    if util_ray <= optimal {
        // Below optimal utilization
        // rate = base + (util / optimal) * slope1
        let util_ratio = util_ray / optimal;
        params.base_variable_borrow_rate + util_ratio * params.variable_rate_slope1
    } else {
        // Above optimal utilization
        // rate = base + slope1 + ((util - optimal) / (1 - optimal)) * slope2
        let excess_util = (util_ray - optimal) / (RAY - optimal);
        params.base_variable_borrow_rate
            + params.variable_rate_slope1
            + excess_util * params.variable_rate_slope2
    }
}

/// Calculate supply rate from borrow rate and utilization
pub fn calculate_supply_rate(
    variable_borrow_rate: f64, // scaled by RAY
    utilization: f64,          // 0.0 - 1.0
    reserve_factor: f64,       // 0.0 - 1.0
) -> f64 {
    // supply_rate = borrow_rate * utilization * (1 - reserve_factor)
    variable_borrow_rate * utilization * (1.0 - reserve_factor)
}

/// Convert RAY-scaled rate to APR (annual percentage rate, decimal)
pub fn ray_to_apr(rate_ray: f64) -> f64 {
    rate_ray / RAY
}

/// Convert APR (decimal) to APY: APY = e^APR - 1
///
/// Note: Aave's liquidityRate IS the APR (not per-second rate).
/// Directly use exp(APR) - 1, do NOT multiply by seconds_per_year.
pub fn apr_to_apy(apr: f64) -> f64 {
    apr.exp() - 1.0
}

/// Convert RAY-scaled rate to APY (annual percentage yield)
pub fn ray_to_apy(rate_ray: f64) -> f64 {
    let apr = rate_ray / RAY;
    apr_to_apy(apr)
}

/// Convert RAY-scaled rate to percentage
pub fn ray_to_percent(rate_ray: f64) -> f64 {
    (rate_ray / RAY) * 100.0
}

/// Get rate parameters for a token symbol
pub fn get_rate_params_for_token(symbol: &str) -> InterestRateParams {
    let symbol_lower = symbol.to_lowercase();

    // Match by symbol patterns
    if symbol_lower.contains("usdc")
        || symbol_lower.contains("usdt")
        || symbol_lower.contains("dai")
        || symbol_lower.contains("lusd")
        || symbol_lower.contains("frax")
        || symbol_lower.contains("gho")
        || symbol_lower.contains("eurs")
    {
        default_rate_params()[0].1.clone() // stablecoin
    } else if symbol_lower.contains("eth")
        || symbol_lower.contains("steth")
        || symbol_lower.contains("weth")
        || symbol_lower.contains("reth")
        || symbol_lower.contains("cbeth")
        || symbol_lower.contains("wsteth")
    {
        default_rate_params()[1].1.clone() // eth
    } else if symbol_lower.contains("btc")
        || symbol_lower.contains("wbtc")
        || symbol_lower.contains("cbtc")
    {
        default_rate_params()[2].1.clone() // btc
    } else if symbol_lower.contains("link")
        || symbol_lower.contains("aave")
        || symbol_lower.contains("uni")
        || symbol_lower.contains("mk")
    {
        default_rate_params()[3].1.clone() // volatile
    } else {
        default_rate_params()[4].1.clone() // low_util
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stablecoin_rates() {
        let params = default_rate_params()[0].1.clone(); // stablecoin
        let util = 0.5; // 50% utilization

        let borrow_rate = calculate_variable_borrow_rate(util, &params);
        let supply_rate = calculate_supply_rate(borrow_rate, util, 0.1); // 10% reserve factor

        let borrow_apy = ray_to_apy(borrow_rate);
        let supply_apy = ray_to_apy(supply_rate);

        println!("Stablecoin at 50% utilization:");
        println!("  Borrow APY: {:.2}%", borrow_apy * 100.0);
        println!("  Supply APY: {:.2}%", supply_apy * 100.0);

        assert!(borrow_apy > 0.0 && borrow_apy < 0.5);
        assert!(supply_apy > 0.0 && supply_apy < 0.5);
    }

    #[test]
    fn test_eth_rates() {
        let params = default_rate_params()[1].1.clone(); // eth
        let util = 0.7; // 70% utilization

        let borrow_rate = calculate_variable_borrow_rate(util, &params);
        let supply_rate = calculate_supply_rate(borrow_rate, util, 0.1);

        let borrow_apy = ray_to_apy(borrow_rate);
        let supply_apy = ray_to_apy(supply_rate);

        println!("ETH at 70% utilization:");
        println!("  Borrow APY: {:.2}%", borrow_apy * 100.0);
        println!("  Supply APY: {:.2}%", supply_apy * 100.0);

        assert!(borrow_apy > 0.0 && borrow_apy < 1.0);
    }

    #[test]
    fn test_above_optimal() {
        let params = default_rate_params()[0].1.clone(); // stablecoin
        let util = 0.95; // 95% utilization (above 90% optimal)

        let borrow_rate = calculate_variable_borrow_rate(util, &params);
        let borrow_apy = ray_to_apy(borrow_rate);

        println!("Stablecoin at 95% utilization (above optimal):");
        println!("  Borrow APY: {:.2}%", borrow_apy * 100.0);

        // Should be significantly higher than at optimal
        let borrow_rate_optimal = calculate_variable_borrow_rate(0.90, &params);
        let borrow_apy_optimal = ray_to_apy(borrow_rate_optimal);
        assert!(borrow_apy > borrow_apy_optimal);
    }

    #[test]
    fn test_rate_curve() {
        let params = default_rate_params()[0].1.clone(); // stablecoin

        println!("\n  Aave V3 Stablecoin Rate Curve (optimal=90%):");
        println!(
            "  {:>8}  {:>12}  {:>12}  {:>12}",
            "Util%", "BorrowAPY", "SupplyAPY", "Diff"
        );
        println!("  {}", "-".repeat(48));

        for util_pct in [10, 20, 30, 40, 50, 60, 70, 80, 85, 88, 90, 92, 95, 98, 100] {
            let util = util_pct as f64 / 100.0;
            let borrow_rate = calculate_variable_borrow_rate(util, &params);
            let supply_rate = calculate_supply_rate(borrow_rate, util, 0.1);
            let borrow_apy = ray_to_apy(borrow_rate);
            let supply_apy = ray_to_apy(supply_rate);

            println!(
                "  {:>7}%  {:>11.2}%  {:>11.2}%  {:>11.2}%",
                util_pct,
                borrow_apy * 100.0,
                supply_apy * 100.0,
                (borrow_apy - supply_apy) * 100.0
            );
        }
    }
}
