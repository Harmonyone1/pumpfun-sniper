//! Integer-only protocol quote math for Pump / PumpSwap (packet Sections 9-12).
//!
//! LIVE-MONEY MATH. All protocol reserve/fee arithmetic is exact `u128` checked
//! integer arithmetic — never `f64` (INV-MKT-015). `f64` appears only in the
//! final normalized *display* conversion (`normalized_mark_sol_per_token`).
//!
//! Every fee component rounds with CEIL (Section 11). Every multiply/add/div is
//! checked and returns `Option`; nothing panics.

/// The three fee components in basis points, as decoded from an on-chain
/// `FeeConfig` tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeComponents {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
}

/// A single fee tier: applies when market cap crosses `threshold` per the tier
/// selection rule in `calculate_fee_tier`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeTier {
    pub threshold: u128,
    pub fees: FeeComponents,
}

/// Checked ceiling division: `ceil(a / b)`. Returns `None` on `b == 0` or
/// overflow.
pub fn ceil_div(a: u128, b: u128) -> Option<u128> {
    if b == 0 {
        return None;
    }
    // a/b rounded up == (a + b - 1) / b, computed with checked add.
    let num = a.checked_add(b.checked_sub(1)?)?;
    Some(num / b)
}

/// Fee amount = `ceil(amount * bps / 10_000)` (Section 11 — every fee CEILs).
pub fn fee_amount(amount: u128, bps: u64) -> Option<u128> {
    let product = amount.checked_mul(bps as u128)?;
    ceil_div(product, 10_000)
}

/// Select the active fee tier for a given market cap (Section 9).
///
/// - `market_cap < tiers[0].threshold` => `tiers[0]`;
/// - otherwise the highest tier whose `threshold <= market_cap`.
///
/// Returns `None` if `tiers` is empty.
pub fn calculate_fee_tier(tiers: &[FeeTier], market_cap: u128) -> Option<&FeeTier> {
    let first = tiers.first()?;
    if market_cap < first.threshold {
        return Some(first);
    }
    // Highest tier whose threshold does not exceed market cap.
    let mut selected = first;
    for tier in tiers {
        if tier.threshold <= market_cap {
            selected = tier;
        }
    }
    Some(selected)
}

/// Pump bonding-curve market cap (Section 10).
///
/// `market_cap = virtual_quote * supply / virtual_token`, all `u128` checked.
/// The caller supplies `supply` (1e15 raw for non-mayhem, actual mint supply for
/// mayhem) — this function does not decide mayhem.
pub fn bonding_market_cap(
    virtual_quote_reserves: u128,
    supply: u128,
    virtual_token_reserves: u128,
) -> Option<u128> {
    if virtual_token_reserves == 0 {
        return None;
    }
    let numerator = virtual_quote_reserves.checked_mul(supply)?;
    Some(numerator / virtual_token_reserves)
}

/// PumpSwap effective quote reserve (Section 11.3, INV-MKT-010).
///
/// `effective = i128(raw_quote_vault) + virtual_quote_reserves` using checked
/// signed addition. Requires the result to be `> 0` and `<= u64::MAX`, else
/// `None`.
pub fn effective_quote_reserve(raw_quote_vault: u64, virtual_quote_reserves: i128) -> Option<u64> {
    let effective = (raw_quote_vault as i128).checked_add(virtual_quote_reserves)?;
    if effective <= 0 || effective > u64::MAX as i128 {
        return None;
    }
    Some(effective as u64)
}

/// PumpSwap canonical pool market cap (Section 10).
///
/// `market_cap = effective_quote * base_supply / base_reserve`, all checked.
pub fn pumpswap_market_cap(
    effective_quote: u128,
    base_supply: u128,
    base_reserve: u128,
) -> Option<u128> {
    if base_reserve == 0 {
        return None;
    }
    let numerator = effective_quote.checked_mul(base_supply)?;
    Some(numerator / base_reserve)
}

/// Pump SELL quote — net raw quote out after protocol + creator fees (11.1).
///
/// ```text
/// raw_quote_out = floor(base_in * virtual_quote / (virtual_token + base_in))
/// protocol_fee  = ceil(raw_quote_out * protocol_bps / 10_000)
/// creator_fee   = ceil(raw_quote_out * creator_bps / 10_000)   [caller passes 0 for default creator]
/// net           = raw_quote_out - protocol_fee - creator_fee
/// ```
///
/// Returns `None` if the summed fees exceed `raw_quote_out` (fee > output is
/// rejected).
pub fn pump_sell_net_quote_out(
    base_in: u128,
    virtual_token_reserves: u128,
    virtual_quote_reserves: u128,
    protocol_bps: u64,
    creator_bps: u64,
) -> Option<u128> {
    let denom = virtual_token_reserves.checked_add(base_in)?;
    if denom == 0 {
        return None;
    }
    let raw_quote_out = base_in.checked_mul(virtual_quote_reserves)? / denom;

    let protocol_fee = fee_amount(raw_quote_out, protocol_bps)?;
    let creator_fee = fee_amount(raw_quote_out, creator_bps)?;
    let total_fee = protocol_fee.checked_add(creator_fee)?;
    if total_fee > raw_quote_out {
        return None;
    }
    Some(raw_quote_out - total_fee)
}

/// Pump BUY quote — expected raw base out for exact quote input (11.2).
///
/// ```text
/// effective_quote_in = floor((quote_in - 1) * 10_000 / (10_000 + total_fee_bps))
/// base_out           = floor(effective_quote_in * virtual_token / (virtual_quote + effective_quote_in))
/// base_out           = min(base_out, real_token_reserves)
/// ```
///
/// Fails closed for `quote_in <= 1` or empty/depleted reserves.
pub fn pump_buy_base_out(
    quote_in: u128,
    virtual_token_reserves: u128,
    virtual_quote_reserves: u128,
    real_token_reserves: u128,
    total_fee_bps: u64,
) -> Option<u128> {
    if quote_in <= 1
        || virtual_token_reserves == 0
        || virtual_quote_reserves == 0
        || real_token_reserves == 0
    {
        return None;
    }
    let numer = (quote_in - 1).checked_mul(10_000)?;
    let effective_quote_in = numer / (10_000u128.checked_add(total_fee_bps as u128)?);
    if effective_quote_in == 0 {
        return None;
    }
    let denom = virtual_quote_reserves.checked_add(effective_quote_in)?;
    if denom == 0 {
        return None;
    }
    let base_out = effective_quote_in.checked_mul(virtual_token_reserves)? / denom;
    let capped = base_out.min(real_token_reserves);
    if capped == 0 {
        return None;
    }
    Some(capped)
}

/// PumpSwap SELL quote — net raw quote out after LP/protocol/creator fees (11.4).
///
/// ```text
/// raw_quote_out = floor(effective_quote * base_in / (base_reserve + base_in))
/// ```
///
/// Each fee CEILs independently; `net = raw - lp - protocol - creator`. Returns
/// `None` if the summed fees exceed `raw_quote_out`.
pub fn pumpswap_sell_net_quote_out(
    base_in: u128,
    base_reserve: u128,
    effective_quote_reserve: u128,
    lp_bps: u64,
    protocol_bps: u64,
    creator_bps: u64,
) -> Option<u128> {
    let denom = base_reserve.checked_add(base_in)?;
    if denom == 0 {
        return None;
    }
    let raw_quote_out = effective_quote_reserve.checked_mul(base_in)? / denom;

    let lp_fee = fee_amount(raw_quote_out, lp_bps)?;
    let protocol_fee = fee_amount(raw_quote_out, protocol_bps)?;
    let creator_fee = fee_amount(raw_quote_out, creator_bps)?;
    let total_fee = lp_fee.checked_add(protocol_fee)?.checked_add(creator_fee)?;
    if total_fee > raw_quote_out {
        return None;
    }
    Some(raw_quote_out - total_fee)
}

/// PumpSwap BUY quote — expected raw base out for exact quote input (11.5).
///
/// ```text
/// effective_quote_in = floor(quote_in * 10_000 / (10_000 + total_fee_bps))
/// base_out           = floor(base_reserve * effective_quote_in / (effective_quote_reserve + effective_quote_in))
/// ```
pub fn pumpswap_buy_base_out(
    quote_in: u128,
    base_reserve: u128,
    effective_quote_reserve: u128,
    total_fee_bps: u64,
) -> Option<u128> {
    if quote_in == 0 || base_reserve == 0 || effective_quote_reserve == 0 {
        return None;
    }
    let numer = quote_in.checked_mul(10_000)?;
    let effective_quote_in = numer / (10_000u128.checked_add(total_fee_bps as u128)?);
    if effective_quote_in == 0 {
        return None;
    }
    let denom = effective_quote_reserve.checked_add(effective_quote_in)?;
    if denom == 0 {
        return None;
    }
    let base_out = base_reserve.checked_mul(effective_quote_in)? / denom;
    if base_out == 0 {
        return None;
    }
    Some(base_out)
}

/// Normalized MARK price in SOL per whole token (Section 12).
///
/// ```text
/// base_ui   = base_reserve_raw / 10^base_decimals
/// quote_sol = quote_reserve_lamports / 1e9
/// mark      = quote_sol / base_ui
/// ```
///
/// `f64` is used ONLY here for the display conversion. Returns `None` unless the
/// result is finite and `> 0` with `base_ui > 0`.
pub fn normalized_mark_sol_per_token(
    base_reserve_raw: u128,
    base_decimals: u8,
    quote_reserve_lamports: u128,
) -> Option<f64> {
    let base_ui = base_reserve_raw as f64 / 10_f64.powi(base_decimals as i32);
    if !(base_ui > 0.0) {
        return None;
    }
    let quote_sol = quote_reserve_lamports as f64 / 1_000_000_000.0;
    let mark = quote_sol / base_ui;
    if mark.is_finite() && mark > 0.0 {
        Some(mark)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // A5 DOCUMENTED FIXTURES (exact expected integer outputs)
    //
    // --- PUMP fixture (bonding curve) ---
    // virtual_token_reserves = 1_073_000_000_000_000
    // virtual_quote_reserves =    30_000_000_000  (lamports)
    // real_token_reserves    =   793_100_000_000_000
    //
    // SELL base_in = 1_000_000_000, protocol=100bps, creator=50bps:
    //   raw_quote_out = 1e9*30e9/(1_073_000_000_000_000+1e9) = 27_958
    //   protocol_fee  = ceil(27_958*100/10000) = 280
    //   creator_fee   = ceil(27_958*50/10000)  = 140
    //   net_quote_out = 27_958 - 280 - 140      = 27_538
    //
    // BUY quote_in = 1_000_000_000, total_fee_bps=150:
    //   effective_quote_in = (1e9-1)*10000/10150 = 985_221_673
    //   base_out           = 985_221_673*1_073e12/(30e9+985_221_673) = 34_117_646_995_895
    //
    // --- PUMPSWAP fixture (canonical pool) ---
    // base_reserve            = 200_000_000_000
    // effective_quote_reserve =  50_000_000_000
    //
    // SELL base_in = 5_000_000, lp=20, protocol=30, creator=5 bps:
    //   raw_quote_out = 50e9*5e6/(200e9+5e6) = 1_249_968
    //   lp=2500 protocol=3750 creator=625   net = 1_243_093
    //   (creator default omitted) net_noc   = 1_243_718
    //
    // BUY quote_in = 1_000_000_000, total_fee_bps=55:
    //   effective_quote_in = 1e9*10000/10055 = 994_530_084
    //   base_out           = 200e9*994_530_084/(50e9+994_530_084) = 3_900_536_321
    //
    // NORMALIZED MARK: base_reserve=200e9 decimals=6 quote=50e9 lamports:
    //   mark = (50e9/1e9)/(200e9/1e6) = 0.00025 SOL/token
    //   raw ratio 50e9/200e9 = 0.25  (differs by 10^6 = decimals factor)
    // =====================================================================

    const PUMP_VT: u128 = 1_073_000_000_000_000;
    const PUMP_VQ: u128 = 30_000_000_000;
    const PUMP_RTR: u128 = 793_100_000_000_000;

    const PS_BASE_RES: u128 = 200_000_000_000;
    const PS_EFF_QUOTE: u128 = 50_000_000_000;

    #[test]
    fn test_fee_uses_ceiling_rounding() {
        // 1 * 1 bps / 10000 = 0.0001 -> ceil = 1 (not 0).
        assert_eq!(fee_amount(1, 1), Some(1));
        // 27_958 * 100 / 10000 = 279.58 -> ceil = 280.
        assert_eq!(fee_amount(27_958, 100), Some(280));
        // Exact division does not round up: 10000 * 100 / 10000 = 100.
        assert_eq!(fee_amount(10_000, 100), Some(100));
        // Zero bps -> zero fee.
        assert_eq!(fee_amount(27_958, 0), Some(0));
    }

    #[test]
    fn test_fee_tier_uses_highest_threshold_not_exceeding_market_cap() {
        let tiers = vec![
            FeeTier {
                threshold: 0,
                fees: FeeComponents {
                    lp_fee_bps: 20,
                    protocol_fee_bps: 100,
                    creator_fee_bps: 5,
                },
            },
            FeeTier {
                threshold: 1_000,
                fees: FeeComponents {
                    lp_fee_bps: 15,
                    protocol_fee_bps: 80,
                    creator_fee_bps: 5,
                },
            },
            FeeTier {
                threshold: 10_000,
                fees: FeeComponents {
                    lp_fee_bps: 10,
                    protocol_fee_bps: 50,
                    creator_fee_bps: 5,
                },
            },
        ];
        // Below first threshold uses first (here threshold 0 == market_cap 0 anyway).
        assert_eq!(calculate_fee_tier(&tiers, 0).unwrap().threshold, 0);
        // Between thresholds -> highest not exceeding.
        assert_eq!(calculate_fee_tier(&tiers, 999).unwrap().threshold, 0);
        assert_eq!(calculate_fee_tier(&tiers, 1_000).unwrap().threshold, 1_000);
        assert_eq!(calculate_fee_tier(&tiers, 5_000).unwrap().threshold, 1_000);
        assert_eq!(calculate_fee_tier(&tiers, 10_000).unwrap().threshold, 10_000);
        assert_eq!(
            calculate_fee_tier(&tiers, u128::MAX).unwrap().threshold,
            10_000
        );
        // Below-first-threshold branch.
        let tiers2 = vec![FeeTier {
            threshold: 500,
            fees: FeeComponents {
                lp_fee_bps: 1,
                protocol_fee_bps: 1,
                creator_fee_bps: 0,
            },
        }];
        assert_eq!(calculate_fee_tier(&tiers2, 100).unwrap().threshold, 500);
        // Empty -> None.
        assert!(calculate_fee_tier(&[], 100).is_none());
    }

    #[test]
    fn test_effective_quote_reserve_positive_virtual() {
        // 40e9 raw + 10e9 virtual = 50e9.
        assert_eq!(
            effective_quote_reserve(40_000_000_000, 10_000_000_000),
            Some(50_000_000_000)
        );
    }

    #[test]
    fn test_effective_quote_reserve_negative_virtual() {
        // 40e9 raw + (-5e9) virtual = 35e9 (signed checked add).
        assert_eq!(
            effective_quote_reserve(40_000_000_000, -5_000_000_000),
            Some(35_000_000_000)
        );
    }

    #[test]
    fn test_effective_quote_reserve_rejects_nonpositive() {
        // raw fully cancelled by virtual -> 0 rejected.
        assert_eq!(effective_quote_reserve(10, -10), None);
        // net negative rejected.
        assert_eq!(effective_quote_reserve(10, -11), None);
    }

    #[test]
    fn test_pump_sell_constant_product_then_independent_fees() {
        // raw = 27_958, protocol_fee = 280, creator_fee = 140, net = 27_538.
        let net = pump_sell_net_quote_out(1_000_000_000, PUMP_VT, PUMP_VQ, 100, 50).unwrap();
        assert_eq!(net, 27_538);
        // Independent CEIL: verify each component against fee_amount directly.
        assert_eq!(fee_amount(27_958, 100), Some(280));
        assert_eq!(fee_amount(27_958, 50), Some(140));
        // Creator-default (0 bps) omits the creator fee entirely.
        let net_no_creator =
            pump_sell_net_quote_out(1_000_000_000, PUMP_VT, PUMP_VQ, 100, 0).unwrap();
        assert_eq!(net_no_creator, 27_958 - 280);
    }

    #[test]
    fn test_pump_sell_rejects_fees_exceeding_output() {
        // Force fee bps summing beyond output: with 6000+6000 bps on a tiny raw,
        // ceil fees exceed the raw output -> None.
        // Tiny base_in gives a small raw_quote_out where CEIL fees dominate.
        let out = pump_sell_net_quote_out(1, PUMP_VT, PUMP_VQ, 6000, 6000);
        // raw = floor(1*30e9/(1_073e12+1)) = 0 ; fees 0; net 0 -> Some(0) actually.
        // Use a raw>0 case where combined bps > 10000 forces fee>output:
        // raw_quote_out here is 0, so test a constructed larger raw instead.
        assert_eq!(out, Some(0));
        // Construct explicit fee>output rejection with small reserves.
        // base_in large vs tiny virtual_token so raw≈virtual_quote; fees 9000+9000.
        let rejected = pump_sell_net_quote_out(1_000_000, 1, 1_000, 9000, 9000);
        // raw = 1_000_000*1000/(1+1_000_000)=999 ; lp+cr fees = 900+900=1800>999 -> None.
        assert_eq!(rejected, None);
    }

    #[test]
    fn test_pump_buy_quote_is_integer_only() {
        let base_out = pump_buy_base_out(1_000_000_000, PUMP_VT, PUMP_VQ, PUMP_RTR, 150).unwrap();
        assert_eq!(base_out, 34_117_646_995_895);
        // Fail-closed inputs.
        assert_eq!(pump_buy_base_out(0, PUMP_VT, PUMP_VQ, PUMP_RTR, 150), None);
        assert_eq!(pump_buy_base_out(1, PUMP_VT, PUMP_VQ, PUMP_RTR, 150), None);
        assert_eq!(pump_buy_base_out(1_000_000_000, 0, PUMP_VQ, PUMP_RTR, 150), None);
        assert_eq!(pump_buy_base_out(1_000_000_000, PUMP_VT, PUMP_VQ, 0, 150), None);
    }

    #[test]
    fn test_pumpswap_sell_uses_effective_quote_reserve() {
        // net (lp+protocol+creator) = 1_243_093.
        let net = pumpswap_sell_net_quote_out(5_000_000, PS_BASE_RES, PS_EFF_QUOTE, 20, 30, 5)
            .unwrap();
        assert_eq!(net, 1_243_093);
        // A different effective quote reserve must change the raw output — proves
        // effective (not raw vault) reserve is what drives the quote.
        let net_bigger =
            pumpswap_sell_net_quote_out(5_000_000, PS_BASE_RES, PS_EFF_QUOTE * 2, 20, 30, 5)
                .unwrap();
        assert!(net_bigger > net);
    }

    #[test]
    fn test_pumpswap_sell_creator_default_omits_creator_fee() {
        // With creator_bps = 0 (default creator) the creator fee is omitted:
        // net = raw - lp - protocol = 1_243_718.
        let net = pumpswap_sell_net_quote_out(5_000_000, PS_BASE_RES, PS_EFF_QUOTE, 20, 30, 0)
            .unwrap();
        assert_eq!(net, 1_243_718);
        // And it is exactly the creator fee (625) larger than the with-creator case.
        let with_creator =
            pumpswap_sell_net_quote_out(5_000_000, PS_BASE_RES, PS_EFF_QUOTE, 20, 30, 5).unwrap();
        assert_eq!(net - with_creator, 625);
    }

    #[test]
    fn test_pumpswap_buy_quote_uses_total_fee_bps() {
        let base_out =
            pumpswap_buy_base_out(1_000_000_000, PS_BASE_RES, PS_EFF_QUOTE, 55).unwrap();
        assert_eq!(base_out, 3_900_536_321);
        // Higher total fee bps -> less base out.
        let base_out_more =
            pumpswap_buy_base_out(1_000_000_000, PS_BASE_RES, PS_EFF_QUOTE, 500).unwrap();
        assert!(base_out_more < base_out);
        // Fail-closed inputs.
        assert_eq!(pumpswap_buy_base_out(0, PS_BASE_RES, PS_EFF_QUOTE, 55), None);
        assert_eq!(pumpswap_buy_base_out(1_000_000_000, 0, PS_EFF_QUOTE, 55), None);
    }

    #[test]
    fn test_normalized_mark_respects_base_decimals() {
        // base=200e9 raw, 6 decimals, quote=50e9 lamports -> 0.00025 SOL/token.
        let mark = normalized_mark_sol_per_token(200_000_000_000, 6, 50_000_000_000).unwrap();
        assert!((mark - 0.00025).abs() < 1e-15);
        // Different decimals change the normalized price by the decimal factor.
        // More decimals => the same raw reserve is fewer whole tokens => higher
        // SOL/whole-token price. 9 decimals: base_ui = 200e9/1e9 = 200 => 50/200 = 0.25.
        let mark9 = normalized_mark_sol_per_token(200_000_000_000, 9, 50_000_000_000).unwrap();
        assert!((mark9 - 0.25).abs() < 1e-12);
        // 10^(9-6) = 1000x difference (mark9 is 1000x mark).
        assert!((mark9 / mark - 1000.0).abs() < 1e-6);
        // Zero base -> None.
        assert!(normalized_mark_sol_per_token(0, 6, 50_000_000_000).is_none());
    }

    #[test]
    fn test_raw_ratio_is_not_normalized_price() {
        // Raw reserve ratio (lamports per raw base unit).
        let raw_ratio = PS_EFF_QUOTE as f64 / PS_BASE_RES as f64; // 0.25
        let normalized =
            normalized_mark_sol_per_token(PS_BASE_RES, 6, PS_EFF_QUOTE).unwrap(); // 0.00025
        assert!((raw_ratio - 0.25).abs() < 1e-12);
        // They differ by exactly 10^base_decimals / 1e9 lamport factor.
        // raw_ratio / normalized = 1e9 / 10^6 = 1000.
        assert!((raw_ratio / normalized - 1000.0).abs() < 1e-6);
        // Concretely: the two are NOT equal (INV-MKT-003).
        assert!((raw_ratio - normalized).abs() > 1e-6);
    }

    #[test]
    fn test_bonding_market_cap_matches_reference() {
        // virtual_quote * 1e15 / virtual_token = 27_958_993_476.
        let mc = bonding_market_cap(PUMP_VQ, 1_000_000_000_000_000, PUMP_VT).unwrap();
        assert_eq!(mc, 27_958_993_476);
        assert!(bonding_market_cap(PUMP_VQ, 1_000_000_000_000_000, 0).is_none());
    }

    #[test]
    fn test_ceil_div_edges() {
        assert_eq!(ceil_div(10, 3), Some(4));
        assert_eq!(ceil_div(9, 3), Some(3));
        assert_eq!(ceil_div(0, 3), Some(0));
        assert_eq!(ceil_div(5, 0), None);
    }
}
