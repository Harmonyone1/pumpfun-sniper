//! Liquidity & Slippage Analyzer
//!
//! Know if you can actually exit before entering.
//! Calculates slippage at different exit sizes using bonding curve math.

use serde::{Deserialize, Serialize};

/// Liquidity analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityAnalysis {
    // Current state
    /// Actual exit capacity in SOL
    pub effective_liquidity_sol: f64,
    /// Price impact coefficient (higher = steeper curve)
    pub curve_steepness: f64,

    // Slippage at different exit sizes
    /// Impact for 0.1 SOL exit
    pub slippage_at_0_1_sol: f64,
    /// Impact for 0.5 SOL exit
    pub slippage_at_0_5_sol: f64,
    /// Impact for 1.0 SOL exit
    pub slippage_at_1_sol: f64,

    // Exit feasibility
    /// Max exit with <10% slippage
    pub max_safe_exit_sol: f64,
    /// Can exit at all?
    pub exit_feasible: bool,

    // Calculated from bonding curve
    /// k constant (x * y = k)
    pub k_constant: u128,
    /// Current price per token
    pub price_per_token: f64,
    /// Current SOL reserves
    pub sol_reserves: f64,
    /// Current token reserves
    pub token_reserves: f64,
}

impl Default for LiquidityAnalysis {
    fn default() -> Self {
        Self {
            effective_liquidity_sol: 0.0,
            curve_steepness: 0.0,
            slippage_at_0_1_sol: 100.0, // Assume worst case
            slippage_at_0_5_sol: 100.0,
            slippage_at_1_sol: 100.0,
            max_safe_exit_sol: 0.0,
            exit_feasible: false,
            k_constant: 0,
            price_per_token: 0.0,
            sol_reserves: 0.0,
            token_reserves: 0.0,
        }
    }
}

impl LiquidityAnalysis {
    /// Check if we can safely exit a given position size
    pub fn can_safely_exit(&self, position_sol: f64, max_slippage_pct: f64) -> bool {
        self.exit_feasible && self.max_safe_exit_sol >= position_sol
            || self.calculate_slippage_for_exit(position_sol) <= max_slippage_pct
    }

    /// Calculate slippage for a specific exit size
    pub fn calculate_slippage_for_exit(&self, exit_sol: f64) -> f64 {
        if self.sol_reserves == 0.0 || self.token_reserves == 0.0 {
            return 100.0;
        }

        // Linear interpolation between known slippage points
        if exit_sol <= 0.1 {
            self.slippage_at_0_1_sol * (exit_sol / 0.1)
        } else if exit_sol <= 0.5 {
            let t = (exit_sol - 0.1) / 0.4;
            self.slippage_at_0_1_sol + t * (self.slippage_at_0_5_sol - self.slippage_at_0_1_sol)
        } else if exit_sol <= 1.0 {
            let t = (exit_sol - 0.5) / 0.5;
            self.slippage_at_0_5_sol + t * (self.slippage_at_1_sol - self.slippage_at_0_5_sol)
        } else {
            // Extrapolate for larger exits (slippage grows super-linearly)
            self.slippage_at_1_sol * (exit_sol / 1.0).powf(1.5)
        }
    }

    /// Get a risk assessment string
    pub fn risk_assessment(&self) -> &'static str {
        if !self.exit_feasible {
            "EXTREME - Cannot exit"
        } else if self.max_safe_exit_sol < 0.05 {
            "VERY HIGH - Minimal exit capacity"
        } else if self.max_safe_exit_sol < 0.2 {
            "HIGH - Limited exit capacity"
        } else if self.max_safe_exit_sol < 0.5 {
            "MODERATE - Adequate exit capacity"
        } else {
            "LOW - Good exit capacity"
        }
    }
}

/// Configuration for liquidity analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityConfig {
    /// Maximum acceptable slippage for "safe" exit
    pub max_safe_slippage_pct: f64,
    /// Minimum liquidity to consider token tradeable
    pub min_liquidity_sol: f64,
    /// Slippage threshold for exit impossible
    pub exit_impossible_slippage_pct: f64,
}

impl Default for LiquidityConfig {
    fn default() -> Self {
        Self {
            max_safe_slippage_pct: 10.0,
            min_liquidity_sol: 0.05,
            exit_impossible_slippage_pct: 50.0,
        }
    }
}

/// Liquidity analyzer using bonding curve math
pub struct LiquidityAnalyzer {
    config: LiquidityConfig,
}

impl LiquidityAnalyzer {
    /// Create a new liquidity analyzer
    pub fn new(config: LiquidityConfig) -> Self {
        Self { config }
    }

    /// Create with default config
    pub fn default_config() -> Self {
        Self::new(LiquidityConfig::default())
    }

    /// Analyze liquidity from bonding curve data
    pub fn analyze(&self, curve: &BondingCurveData) -> LiquidityAnalysis {
        let sol_reserves = curve.virtual_sol_reserves as f64 / 1e9;
        let token_reserves = curve.virtual_token_reserves as f64 / 1e6; // Assuming 6 decimals

        // Calculate k constant
        let k_constant = curve.virtual_sol_reserves as u128 * curve.virtual_token_reserves as u128;

        // Current price per token
        let price_per_token = if token_reserves > 0.0 {
            sol_reserves / token_reserves
        } else {
            0.0
        };

        // Calculate curve steepness (derivative of price impact)
        let curve_steepness = if sol_reserves > 0.0 && token_reserves > 0.0 {
            // For constant product, steepness = 2 * sol / (token^2)
            2.0 * sol_reserves / (token_reserves * token_reserves)
        } else {
            f64::MAX
        };

        // Calculate slippage at different exit sizes
        let slippage_0_1 = self.calculate_sell_slippage(sol_reserves, token_reserves, 0.1);
        let slippage_0_5 = self.calculate_sell_slippage(sol_reserves, token_reserves, 0.5);
        let slippage_1_0 = self.calculate_sell_slippage(sol_reserves, token_reserves, 1.0);

        // Find max safe exit (binary search)
        let max_safe_exit = self.find_max_safe_exit(sol_reserves, token_reserves);

        // Effective liquidity is roughly the SOL we can extract with <10% slippage
        let effective_liquidity = max_safe_exit.min(sol_reserves * 0.3);

        // Exit is feasible if we can get out minimum amount
        let exit_feasible = effective_liquidity >= self.config.min_liquidity_sol
            && slippage_0_1 < self.config.exit_impossible_slippage_pct;

        LiquidityAnalysis {
            effective_liquidity_sol: effective_liquidity,
            curve_steepness,
            slippage_at_0_1_sol: slippage_0_1,
            slippage_at_0_5_sol: slippage_0_5,
            slippage_at_1_sol: slippage_1_0,
            max_safe_exit_sol: max_safe_exit,
            exit_feasible,
            k_constant,
            price_per_token,
            sol_reserves,
            token_reserves,
        }
    }

    /// Analyze liquidity from simple reserves values
    pub fn analyze_simple(&self, sol_reserves: f64, token_reserves: f64) -> LiquidityAnalysis {
        // Calculate k constant (using u128 for precision)
        let k_constant = ((sol_reserves * 1e9) as u128) * ((token_reserves * 1e6) as u128);

        // Current price per token
        let price_per_token = if token_reserves > 0.0 {
            sol_reserves / token_reserves
        } else {
            0.0
        };

        // Calculate curve steepness
        let curve_steepness = if sol_reserves > 0.0 && token_reserves > 0.0 {
            2.0 * sol_reserves / (token_reserves * token_reserves)
        } else {
            f64::MAX
        };

        // Calculate slippage at different exit sizes
        let slippage_0_1 = self.calculate_sell_slippage(sol_reserves, token_reserves, 0.1);
        let slippage_0_5 = self.calculate_sell_slippage(sol_reserves, token_reserves, 0.5);
        let slippage_1_0 = self.calculate_sell_slippage(sol_reserves, token_reserves, 1.0);

        // Find max safe exit
        let max_safe_exit = self.find_max_safe_exit(sol_reserves, token_reserves);

        // Effective liquidity
        let effective_liquidity = max_safe_exit.min(sol_reserves * 0.3);

        // Exit feasibility
        let exit_feasible = effective_liquidity >= self.config.min_liquidity_sol
            && slippage_0_1 < self.config.exit_impossible_slippage_pct;

        LiquidityAnalysis {
            effective_liquidity_sol: effective_liquidity,
            curve_steepness,
            slippage_at_0_1_sol: slippage_0_1,
            slippage_at_0_5_sol: slippage_0_5,
            slippage_at_1_sol: slippage_1_0,
            max_safe_exit_sol: max_safe_exit,
            exit_feasible,
            k_constant,
            price_per_token,
            sol_reserves,
            token_reserves,
        }
    }

    /// Calculate slippage for selling tokens to get `target_sol` out
    ///
    /// For constant product AMM: x * y = k
    /// After selling `token_amount`:
    ///   new_token_reserve = old_token + token_amount
    ///   new_sol_reserve = k / new_token_reserve
    ///   sol_received = old_sol - new_sol_reserve
    ///   slippage = (expected_sol - sol_received) / expected_sol * 100
    fn calculate_sell_slippage(
        &self,
        sol_reserves: f64,
        token_reserves: f64,
        target_sol: f64,
    ) -> f64 {
        if sol_reserves <= 0.0 || token_reserves <= 0.0 {
            return 100.0;
        }

        // k constant
        let k = sol_reserves * token_reserves;

        // How many tokens do we need to sell to get target_sol?
        // new_sol = sol_reserves - target_sol
        // new_token = k / new_sol
        // tokens_to_sell = new_token - token_reserves

        let new_sol = sol_reserves - target_sol;
        if new_sol <= 0.0 {
            return 100.0; // Can't extract that much
        }

        let new_token = k / new_sol;
        let tokens_to_sell = new_token - token_reserves;

        if tokens_to_sell <= 0.0 {
            return 0.0; // Somehow negative? Return 0 slippage
        }

        // Expected price without slippage
        let expected_price = sol_reserves / token_reserves;
        let expected_sol_at_spot = tokens_to_sell * expected_price;

        // Actual SOL received is target_sol
        // Slippage = (expected - actual) / expected * 100
        let slippage = if expected_sol_at_spot > 0.0 {
            ((expected_sol_at_spot - target_sol) / expected_sol_at_spot) * 100.0
        } else {
            100.0
        };

        slippage.max(0.0).min(100.0)
    }

    /// Find maximum exit size with slippage under threshold (binary search)
    fn find_max_safe_exit(&self, sol_reserves: f64, token_reserves: f64) -> f64 {
        let mut low = 0.0;
        let mut high = sol_reserves * 0.5; // Max 50% of reserves

        // Binary search for max safe exit
        for _ in 0..20 {
            let mid = (low + high) / 2.0;
            let slippage = self.calculate_sell_slippage(sol_reserves, token_reserves, mid);

            if slippage <= self.config.max_safe_slippage_pct {
                low = mid;
            } else {
                high = mid;
            }
        }

        low
    }

    /// Quick check if a position can be exited
    pub fn can_exit(&self, curve: &BondingCurveData, position_sol: f64) -> bool {
        let analysis = self.analyze(curve);
        analysis.can_safely_exit(position_sol, self.config.max_safe_slippage_pct)
    }
}

/// Bonding curve data from pump.fun
#[derive(Debug, Clone, Default)]
pub struct BondingCurveData {
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub complete: bool,
}

impl BondingCurveData {
    /// Create from raw values
    pub fn new(
        virtual_sol: u64,
        virtual_token: u64,
        real_sol: u64,
        real_token: u64,
        complete: bool,
    ) -> Self {
        Self {
            virtual_sol_reserves: virtual_sol,
            virtual_token_reserves: virtual_token,
            real_sol_reserves: real_sol,
            real_token_reserves: real_token,
            complete,
        }
    }

    /// Calculate current price in SOL per token
    pub fn price_per_token(&self) -> f64 {
        if self.virtual_token_reserves == 0 {
            return 0.0;
        }
        (self.virtual_sol_reserves as f64 / 1e9) / (self.virtual_token_reserves as f64 / 1e6)
    }

    /// Calculate market cap in SOL
    pub fn market_cap_sol(&self, total_supply: u64) -> f64 {
        self.price_per_token() * (total_supply as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_curve(sol: u64, token: u64) -> BondingCurveData {
        BondingCurveData::new(sol, token, sol, token, false)
    }

    #[test]
    fn test_slippage_calculation_small_exit() {
        let analyzer = LiquidityAnalyzer::default_config();

        // 10 SOL and 1M tokens
        let curve = make_curve(10_000_000_000, 1_000_000_000_000);
        let analysis = analyzer.analyze(&curve);

        // Small exit should have low slippage
        assert!(
            analysis.slippage_at_0_1_sol < 5.0,
            "0.1 SOL exit should have <5% slippage"
        );
    }

    #[test]
    fn test_slippage_calculation_large_exit() {
        let analyzer = LiquidityAnalyzer::default_config();

        // 10 SOL and 1M tokens
        let curve = make_curve(10_000_000_000, 1_000_000_000_000);
        let analysis = analyzer.analyze(&curve);

        // Large exit should have more slippage
        assert!(
            analysis.slippage_at_1_sol > analysis.slippage_at_0_1_sol,
            "Larger exits should have more slippage"
        );
    }

    #[test]
    fn test_low_liquidity_detection() {
        let analyzer = LiquidityAnalyzer::default_config();

        // Very low liquidity: 0.01 SOL
        let curve = make_curve(10_000_000, 1_000_000_000_000);
        let analysis = analyzer.analyze(&curve);

        assert!(
            !analysis.exit_feasible,
            "Very low liquidity should not be feasible"
        );
    }

    #[test]
    fn test_max_safe_exit() {
        let analyzer = LiquidityAnalyzer::default_config();

        // 10 SOL
        let curve = make_curve(10_000_000_000, 1_000_000_000_000);
        let analysis = analyzer.analyze(&curve);

        assert!(
            analysis.max_safe_exit_sol > 0.0,
            "Should have some safe exit capacity"
        );
        assert!(
            analysis.max_safe_exit_sol < 5.0,
            "Safe exit should be less than half of reserves"
        );
    }

    #[test]
    fn test_can_safely_exit() {
        let analyzer = LiquidityAnalyzer::default_config();

        let curve = make_curve(10_000_000_000, 1_000_000_000_000);
        let analysis = analyzer.analyze(&curve);

        // Small position should be safe
        assert!(analysis.can_safely_exit(0.05, 10.0));

        // Very large position might not be safe
        // (depends on curve parameters)
    }

    #[test]
    fn test_risk_assessment() {
        let mut analysis = LiquidityAnalysis::default();

        analysis.exit_feasible = false;
        assert_eq!(analysis.risk_assessment(), "EXTREME - Cannot exit");

        analysis.exit_feasible = true;
        analysis.max_safe_exit_sol = 0.03;
        assert_eq!(
            analysis.risk_assessment(),
            "VERY HIGH - Minimal exit capacity"
        );

        analysis.max_safe_exit_sol = 0.1;
        assert_eq!(analysis.risk_assessment(), "HIGH - Limited exit capacity");

        analysis.max_safe_exit_sol = 0.3;
        assert_eq!(
            analysis.risk_assessment(),
            "MODERATE - Adequate exit capacity"
        );

        analysis.max_safe_exit_sol = 1.0;
        assert_eq!(analysis.risk_assessment(), "LOW - Good exit capacity");
    }

    #[test]
    fn test_price_per_token() {
        let curve = make_curve(10_000_000_000, 1_000_000_000_000); // 10 SOL, 1M tokens
        let price = curve.price_per_token();

        // 10 SOL / 1M tokens = 0.00001 SOL per token
        assert!((price - 0.00001).abs() < 0.000001);
    }
}
