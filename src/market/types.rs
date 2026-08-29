//! Canonical market domain types — FROZEN CONTRACT (packet Section 13).
//!
//! These types are shared across the market oracle, quote math, and execution
//! layers. They intentionally separate the three distinct prices in MPT-001:
//! MARK (spot observation), EXECUTABLE QUOTE (exact-size pre-send), and the
//! CONFIRMED FILL (from transaction truth, not modeled here).

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

/// Canonical execution venue for a Pump-origin token.
///
/// There is deliberately no `DexScreener` variant: DexScreener is discovery /
/// observation only and never a venue an executable quote can be pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketVenue {
    /// Pre-graduation Pump bonding curve.
    PumpBondingCurve,
    /// Post-graduation canonical PumpSwap (PumpAmm) pool.
    PumpSwapCanonical,
}

/// Quote asset backing a market. Current SOL-only accounting supports `Sol`;
/// any other mint is `Unsupported` and fails closed for automated trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteAsset {
    /// SOL / wrapped-SOL quote asset.
    Sol,
    /// Any other (unsupported) quote mint. Carries the offending mint for
    /// diagnostics/alerting.
    Unsupported(Pubkey),
}

/// Side of a market operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketSide {
    Buy,
    Sell,
}

/// A fresh on-chain market observation (MARK price).
///
/// This is NOT a fill and NOT an executable-size quote. It carries the
/// dimensional context (venue, quote asset, decimals, slot, timestamp) required
/// by INV-MKT-004 so downstream code can never misread a bare number.
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub mint: Pubkey,
    pub venue: MarketVenue,
    pub quote_asset: QuoteAsset,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    /// None for an unsupported quote asset (no SOL/token price exists for it).
    pub mark_price_sol_per_token: Option<f64>,
    pub slot: u64,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
}

/// An exact-size, same-venue, pre-send executable quote.
///
/// `quote_amount_raw` is the raw protocol expectation at the fetched state; no
/// slippage tolerance is folded in.
#[derive(Debug, Clone)]
pub struct ExecutableQuote {
    pub mint: Pubkey,
    pub side: MarketSide,
    pub venue: MarketVenue,
    pub quote_asset: QuoteAsset,
    pub base_decimals: u8,
    pub quote_decimals: u8,

    /// Buy: expected raw base OUT. Sell: exact raw base IN.
    pub base_amount_raw: u64,

    /// Buy: exact raw quote IN. Sell: expected net raw quote OUT after fees.
    pub quote_amount_raw: u64,

    pub expected_price_sol_per_token: Option<f64>,

    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
    pub lp_fee_bps: u64,

    pub slot: u64,
    pub quoted_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutableQuote {
    /// True when this quote is priced in SOL.
    pub fn is_sol_pair(&self) -> bool {
        self.quote_asset == QuoteAsset::Sol
    }

    /// Quote amount expressed in whole SOL, or None if not a SOL pair.
    ///
    /// `quote_amount_raw` is denominated in lamports for a SOL pair; convert
    /// using the SOL lamport constant (1e9), which is explicitly allowed.
    pub fn expected_sol(&self) -> Option<f64> {
        if self.is_sol_pair() {
            Some(self.quote_amount_raw as f64 / 1_000_000_000.0)
        } else {
            None
        }
    }

    /// Base amount expressed in whole (UI) tokens using the token's decimals.
    ///
    /// Uses `10^base_decimals`, never a hardcoded 1_000_000 (Section 18).
    pub fn base_amount_ui(&self) -> f64 {
        self.base_amount_raw as f64 / 10_f64.powi(self.base_decimals as i32)
    }

    /// Age of this quote in milliseconds relative to now.
    pub fn age_ms(&self) -> i64 {
        (chrono::Utc::now() - self.quoted_at).num_milliseconds()
    }

    /// The expected SOL-per-token price at fetched state (if known).
    pub fn expected_price_sol_per_token(&self) -> Option<f64> {
        self.expected_price_sol_per_token
    }
}
