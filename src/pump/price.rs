//! Price calculation utilities for pump.fun bonding curves

use crate::error::Result;
use super::accounts::BondingCurve;

/// Token decimals - pump.fun uses 6 decimals (not Solana's standard 9)
/// WARNING: This is the default, but should be read from mint for accuracy
pub const DEFAULT_TOKEN_DECIMALS: u8 = 6;

/// SOL decimals (lamports)
pub const SOL_DECIMALS: u8 = 9;

/// Calculate current token price in SOL from bonding curve state
pub fn calculate_price(curve: &BondingCurve) -> Result<f64> {
    curve.get_price()
}

/// Calculate price impact for a given buy amount
/// Returns (tokens_received, price_impact_percent)
pub fn calculate_buy_impact(
    curve: &BondingCurve,
    sol_amount: u64,
) -> Result<(u64, f64)> {
    let tokens = curve.calculate_buy_tokens(sol_amount)?;

    // Calculate effective price
    let effective_price = sol_amount as f64 / tokens as f64;

    // Calculate spot price
    let spot_price = curve.get_price()?;

    // Price impact = (effective_price - spot_price) / spot_price * 100
    let price_impact = ((effective_price - spot_price) / spot_price) * 100.0;

    Ok((tokens, price_impact))
}

/// Calculate price impact for a given sell amount
/// Returns (sol_received, price_impact_percent)
pub fn calculate_sell_impact(
    curve: &BondingCurve,
    token_amount: u64,
) -> Result<(u64, f64)> {
    let sol = curve.calculate_sell_sol(token_amount)?;

    // Calculate effective price
    let effective_price = sol as f64 / token_amount as f64;

    // Calculate spot price
    let spot_price = curve.get_price()?;

    // Price impact = (spot_price - effective_price) / spot_price * 100
    let price_impact = ((spot_price - effective_price) / spot_price) * 100.0;

    Ok((sol, price_impact))
}

/// Calculate minimum tokens to receive for a buy with slippage
pub fn calculate_min_tokens_with_slippage(
    expected_tokens: u64,
    slippage_bps: u32,
) -> u64 {
    // slippage_bps is in basis points (100 bps = 1%)
    let slippage_factor = 10000 - slippage_bps as u64;
    (expected_tokens * slippage_factor) / 10000
}

/// Calculate minimum SOL to receive for a sell with slippage
pub fn calculate_min_sol_with_slippage(
    expected_sol: u64,
    slippage_bps: u32,
) -> u64 {
    let slippage_factor = 10000 - slippage_bps as u64;
    (expected_sol * slippage_factor) / 10000
}

/// Calculate maximum SOL to spend for a buy with slippage
pub fn calculate_max_sol_with_slippage(
    expected_sol: u64,
    slippage_bps: u32,
) -> u64 {
    let slippage_factor = 10000 + slippage_bps as u64;
    (expected_sol * slippage_factor) / 10000
}

/// Convert lamports to SOL
pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / 10f64.powi(SOL_DECIMALS as i32)
}

/// Convert SOL to lamports
pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 10f64.powi(SOL_DECIMALS as i32)) as u64
}

/// Convert token amount to human-readable (with decimals)
pub fn tokens_to_human(amount: u64, decimals: u8) -> f64 {
    amount as f64 / 10f64.powi(decimals as i32)
}

/// Convert human-readable amount to token smallest units
pub fn human_to_tokens(amount: f64, decimals: u8) -> u64 {
    (amount * 10f64.powi(decimals as i32)) as u64
}

/// Format price for display
pub fn format_price(price: f64) -> String {
    if price < 0.000001 {
        format!("{:.10}", price)
    } else if price < 0.001 {
        format!("{:.8}", price)
    } else if price < 1.0 {
        format!("{:.6}", price)
    } else {
        format!("{:.4}", price)
    }
}

/// Calculate percentage change between two prices
pub fn calculate_percent_change(old_price: f64, new_price: f64) -> f64 {
    if old_price == 0.0 {
        return 0.0;
    }
    ((new_price - old_price) / old_price) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_curve() -> BondingCurve {
        BondingCurve::new_for_test(
            30_000_000_000,       // virtual_sol_reserves: 30 SOL
            1_000_000_000_000,    // virtual_token_reserves: 1M tokens
            0,                    // real_sol_reserves
            1_000_000_000_000,    // real_token_reserves
            1_000_000_000_000,    // token_total_supply
            false,                // complete
        )
    }

    #[test]
    fn test_slippage_calculation() {
        // 25% slippage (2500 bps)
        let expected = 1_000_000u64;
        let min_with_slippage = calculate_min_tokens_with_slippage(expected, 2500);
        assert_eq!(min_with_slippage, 750_000); // 75% of expected
    }

    #[test]
    fn test_lamports_conversion() {
        assert_eq!(lamports_to_sol(1_000_000_000), 1.0);
        assert_eq!(sol_to_lamports(1.0), 1_000_000_000);
    }

    #[test]
    fn test_percent_change() {
        assert_eq!(calculate_percent_change(100.0, 150.0), 50.0);
        assert_eq!(calculate_percent_change(100.0, 50.0), -50.0);
    }

    #[test]
    fn test_buy_impact() {
        let curve = test_curve();
        let (tokens, impact) = calculate_buy_impact(&curve, 1_000_000_000).unwrap();

        // Should receive some tokens
        assert!(tokens > 0);
        // Should have positive price impact (buying increases price)
        assert!(impact > 0.0);
    }
}
