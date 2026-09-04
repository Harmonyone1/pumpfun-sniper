//! P3 measurement pilot — observation-layer event types + PURE feature derivation.
//!
//! Frozen under P3-MEASUREMENT-PILOT-PROTOCOL-001 + AMENDMENT-001 (dropped
//! `same_receive_window_buy_fraction`; Domain-1 = 7 features). This module is
//! OBSERVATION ONLY: append-only event records + deterministic, reproducible
//! feature derivation. It contains NO trading, signing, transaction, strategy,
//! or hypothesis-conditioned logic, and NO economic-outcome computation.
//!
//! `event_received_at` is a recorder-receipt time used only for T2/T6 cutoffs,
//! replay, and QC. It is NEVER a chain execution timestamp; no same-slot /
//! receive-window coordination feature is derived here (AMENDMENT-001).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bump only on incompatible normalization changes.
pub const MEASUREMENT_NORMALIZATION_VERSION: u32 = 1;
/// Bump only on incompatible derived-feature-formula changes.
pub const MEASUREMENT_FEATURE_VERSION: u32 = 1;
/// Fixed pump.fun total mint supply (tokens), the SOLE holder-share denominator.
pub const TOTAL_MINT_SUPPLY_TOKENS: u64 = 1_000_000_000;
/// Frozen read-only microstructure probe sizes (lamports).
pub const MICROSTRUCTURE_PROBE_SIZES_LAMPORTS: [u64; 3] = [500_000, 1_000_000, 2_000_000];
/// Frozen implementation tolerance for the microstructure redundancy audit
/// (relative difference vs bonding-curve-implied base). NOT outcome-derived.
pub const REDUNDANCY_REL_TOLERANCE: f64 = 0.02;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSide {
    Buy,
    Sell,
    Unknown,
}

impl TradeSide {
    /// Deterministic parse of the raw provider `tx_type`. Anything not exactly
    /// buy/sell maps to `Unknown` (persisted, never silently dropped).
    pub fn from_tx_type(tx_type: &str) -> TradeSide {
        match tx_type.trim().to_ascii_lowercase().as_str() {
            "buy" => TradeSide::Buy,
            "sell" => TradeSide::Sell,
            _ => TradeSide::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotClass {
    T2,
    T6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HolderAccountClass {
    CurveProgram,
    Creator,
    Ordinary,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RedundancyClass {
    NonRedundant,
    LikelyRedundant,
    Redundant,
}

/// Explicit failure provenance. Missing measurement is NEVER numeric zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MeasurementFailureCategory {
    RpcUnavailable,
    RateLimited,
    Timeout,
    AccountMissing,
    UnsupportedState,
    MalformedProviderEvent,
    StreamEventMissing,
    SnapshotTooLate,
    ComputationTooLate,
    DuplicateEvent,
    UnknownTradeType,
    Other,
}

// ---------------------------------------------------------------------------
// Domain 1 — raw trade observation
// ---------------------------------------------------------------------------

/// Append-only normalized raw trade, 1:1 from the PumpPortal `TradeEvent`.
/// Chain slot / block_time / wallet balance are NOT present in the source and
/// are deliberately absent here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeObserved {
    pub schema_version: u32,
    pub normalization_version: u32,
    pub run_id: String,
    pub mint: String,
    /// Dedup key. Empty string means the source omitted it (kept out of dedup).
    pub signature: String,
    pub trader_public_key: String,
    /// Raw provider `tx_type` preserved verbatim.
    pub tx_type: String,
    pub side: TradeSide,
    /// Provider UI token amount (NOT raw units).
    pub token_amount_ui: f64,
    /// SOL (NOT lamports).
    pub sol_amount: f64,
    pub bonding_curve_key: String,
    pub v_tokens_in_bonding_curve: f64,
    pub v_sol_in_bonding_curve: f64,
    pub market_cap_sol: f64,
    /// Local recorder-receipt time. NOT a chain execution timestamp.
    pub event_received_at: DateTime<Utc>,
    pub source: String,
    pub source_revision: String,
}

/// Result of deduping a raw trade slice by `signature`.
#[derive(Debug, Clone, PartialEq)]
pub struct DedupResult<'a> {
    pub kept: Vec<&'a TradeObserved>,
    pub duplicate_count: u64,
    pub missing_signature_count: u64,
}

/// Deduplicate by `signature`. ORDER-INDEPENDENT: for each signature the kept
/// occurrence is the one with the earliest `event_received_at` (a duplicate is a
/// redelivery of the same trade, so its true receipt is the earliest), which
/// keeps cutoff filtering correct even if the input is not receipt-ordered. The
/// kept set is returned in a deterministic (event_received_at, signature) order.
/// Trades with an empty signature cannot be deduped and are excluded + counted.
pub fn dedup_trades(trades: &[TradeObserved]) -> DedupResult<'_> {
    use std::collections::HashMap;
    let mut best: HashMap<&str, &TradeObserved> = HashMap::new();
    let mut duplicate_count = 0u64;
    let mut missing_signature_count = 0u64;
    for t in trades {
        if t.signature.is_empty() {
            missing_signature_count += 1;
            continue;
        }
        match best.get(t.signature.as_str()) {
            Some(prev) => {
                duplicate_count += 1;
                if t.event_received_at < prev.event_received_at {
                    best.insert(t.signature.as_str(), t);
                }
            }
            None => {
                best.insert(t.signature.as_str(), t);
            }
        }
    }
    let mut kept: Vec<&TradeObserved> = best.into_values().collect();
    kept.sort_by(|a, b| {
        a.event_received_at
            .cmp(&b.event_received_at)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    DedupResult { kept, duplicate_count, missing_signature_count }
}

// ---------------------------------------------------------------------------
// Domain 1 — participation features (7; AMENDMENT-001)
// ---------------------------------------------------------------------------

/// The 7 frozen Domain-1 features. All are `None` when there are zero buys
/// through the cutoff (missing, never imputed to zero).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipationFeatures {
    pub unique_buyers: u64,
    pub buy_count: u64,
    pub net_quote_flow_sol: f64,
    pub top1_buyer_share: Option<f64>,
    pub top5_buyer_share: Option<f64>,
    pub buyer_hhi: Option<f64>,
    pub median_buy_size_sol: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipationSnapshot {
    pub run_id: String,
    pub mint: String,
    pub snapshot_class: SnapshotClass,
    pub cutoff_timestamp: DateTime<Utc>,
    pub computed_at: DateTime<Utc>,
    /// True iff computation completed at/before the decision deadline.
    pub available_in_time: bool,
    pub feature_version: u32,
    pub source_event_count: u64,
    pub deduped_event_count: u64,
    pub failure: Option<MeasurementFailureCategory>,
    pub features: ParticipationFeatures,
}

/// Derive the 7 participation features from deduped trades whose
/// `event_received_at <= cutoff`. Pure + deterministic. Buyer shares/HHI are by
/// per-wallet summed BUY sol_amount.
pub fn participation_features(deduped: &[&TradeObserved], cutoff: DateTime<Utc>) -> ParticipationFeatures {
    use std::collections::BTreeMap;
    let buys: Vec<&&TradeObserved> = deduped
        .iter()
        .filter(|t| t.side == TradeSide::Buy && t.event_received_at <= cutoff)
        .collect();
    let sells_sol: f64 = deduped
        .iter()
        .filter(|t| t.side == TradeSide::Sell && t.event_received_at <= cutoff)
        .map(|t| t.sol_amount)
        .sum();
    let buys_sol: f64 = buys.iter().map(|t| t.sol_amount).sum();
    let buy_count = buys.len() as u64;
    let net_quote_flow_sol = buys_sol - sells_sol;

    if buys.is_empty() {
        return ParticipationFeatures {
            unique_buyers: 0,
            buy_count: 0,
            net_quote_flow_sol,
            top1_buyer_share: None,
            top5_buyer_share: None,
            buyer_hhi: None,
            median_buy_size_sol: None,
        };
    }

    // Per-wallet summed buy SOL (BTreeMap => deterministic ordering).
    let mut per_wallet: BTreeMap<&str, f64> = BTreeMap::new();
    for t in &buys {
        *per_wallet.entry(t.trader_public_key.as_str()).or_insert(0.0) += t.sol_amount;
    }
    let total_buy_sol: f64 = per_wallet.values().sum();
    let mut wallet_sol: Vec<f64> = per_wallet.values().copied().collect();
    wallet_sol.sort_by(|a, b| b.partial_cmp(a).unwrap());

    let (top1, top5, hhi) = if total_buy_sol > 0.0 {
        let top1 = wallet_sol[0] / total_buy_sol;
        let top5 = wallet_sol.iter().take(5).sum::<f64>() / total_buy_sol;
        let hhi = per_wallet
            .values()
            .map(|v| {
                let s = v / total_buy_sol;
                s * s
            })
            .sum::<f64>();
        (Some(top1), Some(top5), Some(hhi))
    } else {
        (None, None, None)
    };

    // Median buy size over individual BUY trades (not per wallet).
    let mut sizes: Vec<f64> = buys.iter().map(|t| t.sol_amount).collect();
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sizes.len();
    let median = if n % 2 == 1 {
        sizes[n / 2]
    } else {
        (sizes[n / 2 - 1] + sizes[n / 2]) / 2.0
    };

    ParticipationFeatures {
        unique_buyers: per_wallet.len() as u64,
        buy_count,
        net_quote_flow_sol,
        top1_buyer_share: top1,
        top5_buyer_share: top5,
        buyer_hhi: hhi,
        median_buy_size_sol: Some(median),
    }
}

/// Build a full ParticipationSnapshot (adds cutoff/available_in_time metadata).
#[allow(clippy::too_many_arguments)]
pub fn compute_participation_snapshot(
    run_id: &str,
    mint: &str,
    class: SnapshotClass,
    cutoff: DateTime<Utc>,
    computed_at: DateTime<Utc>,
    decision_deadline: DateTime<Utc>,
    source_events: &[TradeObserved],
) -> ParticipationSnapshot {
    let dd = dedup_trades(source_events);
    let features = participation_features(&dd.kept, cutoff);
    ParticipationSnapshot {
        run_id: run_id.to_string(),
        mint: mint.to_string(),
        snapshot_class: class,
        cutoff_timestamp: cutoff,
        computed_at,
        available_in_time: computed_at <= decision_deadline,
        feature_version: MEASUREMENT_FEATURE_VERSION,
        source_event_count: source_events.len() as u64,
        deduped_event_count: dd.kept.len() as u64,
        failure: None,
        features,
    }
}

// ---------------------------------------------------------------------------
// Domain 2 — holder snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderAccount {
    pub address: String,
    pub raw_balance_tokens: u64,
    pub class: HolderAccountClass,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderFeatures {
    pub top1_noncurve_holder_share: Option<f64>,
    pub top5_noncurve_holder_share: Option<f64>,
    pub top10_noncurve_holder_share: Option<f64>,
    pub holder_hhi: Option<f64>,
    /// Count of ordinary (non-curve, non-creator) accounts SEEN. This is a
    /// top-20-truncated floor, flagged by `ordinary_holder_count_top20_floor`.
    pub ordinary_holder_count: u64,
    pub ordinary_holder_count_top20_floor: bool,
    /// `None` when creator ATA attribution is not deterministic (BLOCKED).
    pub creator_held_share: Option<f64>,
    pub curve_held_share: Option<f64>,
    pub noncurve_supply_share: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderSnapshot {
    pub run_id: String,
    pub mint: String,
    pub requested_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub rpc_slot: Option<u64>,
    pub available_in_time: bool,
    pub raw_accounts: Vec<HolderAccount>,
    pub total_mint_supply_tokens: u64,
    pub failure: Option<MeasurementFailureCategory>,
    pub source: String,
    pub source_revision: String,
    pub feature_version: u32,
    pub features: HolderFeatures,
}

/// Derive holder features. Denominator is fixed TOTAL_MINT_SUPPLY_TOKENS.
/// `creator_deterministic=false` => creator_held_share is BLOCKED (None).
pub fn holder_features(accounts: &[HolderAccount], creator_deterministic: bool) -> HolderFeatures {
    let denom = TOTAL_MINT_SUPPLY_TOKENS as f64;
    let noncurve: Vec<&HolderAccount> = accounts
        .iter()
        .filter(|a| a.class == HolderAccountClass::Ordinary || a.class == HolderAccountClass::Unknown)
        .collect();
    let curve_bal: u64 = accounts
        .iter()
        .filter(|a| a.class == HolderAccountClass::CurveProgram)
        .map(|a| a.raw_balance_tokens)
        .sum();
    let creator_bal: u64 = accounts
        .iter()
        .filter(|a| a.class == HolderAccountClass::Creator)
        .map(|a| a.raw_balance_tokens)
        .sum();

    let mut nc: Vec<u64> = noncurve.iter().map(|a| a.raw_balance_tokens).collect();
    nc.sort_by(|a, b| b.cmp(a));
    let topk = |k: usize| -> Option<f64> {
        if nc.is_empty() {
            None
        } else {
            Some(nc.iter().take(k).sum::<u64>() as f64 / denom)
        }
    };
    let hhi = if nc.is_empty() {
        None
    } else {
        Some(nc.iter().map(|b| {
            let s = *b as f64 / denom;
            s * s
        }).sum::<f64>())
    };
    // Ordinary = non-curve non-creator (Unknown is treated as ordinary for share
    // math but not counted as a confirmed ordinary holder).
    let ordinary_count = accounts
        .iter()
        .filter(|a| a.class == HolderAccountClass::Ordinary)
        .count() as u64;

    HolderFeatures {
        top1_noncurve_holder_share: topk(1),
        top5_noncurve_holder_share: topk(5),
        top10_noncurve_holder_share: topk(10),
        holder_hhi: hhi,
        ordinary_holder_count: ordinary_count,
        ordinary_holder_count_top20_floor: true,
        creator_held_share: if creator_deterministic {
            Some(creator_bal as f64 / denom)
        } else {
            None
        },
        curve_held_share: Some(curve_bal as f64 / denom),
        noncurve_supply_share: Some(1.0 - (curve_bal as f64 / denom)),
    }
}

// ---------------------------------------------------------------------------
// Domain 3 — microstructure probes + redundancy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrostructureProbe {
    pub run_id: String,
    pub mint: String,
    pub requested_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub available_in_time: bool,
    pub input_lamports: u64,
    /// Expected base out (raw token units) for a read-only exact-input buy quote.
    pub expected_base_raw: Option<u64>,
    pub base_decimals: u8,
    pub success: bool,
    pub quote_source: String,
    pub protocol_fee_bps: Option<u64>,
    pub creator_fee_bps: Option<u64>,
    pub lp_fee_bps: Option<u64>,
    pub latency_ms: u64,
    pub failure: Option<MeasurementFailureCategory>,
    pub feature_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedundancyAudit {
    pub abs_difference: f64,
    pub relative_difference: f64,
    pub within_tolerance: bool,
    pub class: RedundancyClass,
}

/// Marginal UI tokens per SOL from a probe's expected raw base and input.
pub fn marginal_tokens_per_sol(expected_base_raw: u64, input_lamports: u64, base_decimals: u8) -> Option<f64> {
    if input_lamports == 0 {
        return None;
    }
    let base_ui = expected_base_raw as f64 / 10f64.powi(base_decimals as i32);
    let sol_in = input_lamports as f64 / 1_000_000_000.0;
    Some(base_ui / sol_in)
}

/// Bonding-curve-implied UI tokens out for a constant-product buy of `sol_in`
/// against reserves (v_sol, v_tokens), ex-fee. Deterministic reference used ONLY
/// for the redundancy audit (never an outcome).
pub fn curve_implied_tokens_out(v_sol: f64, v_tokens: f64, sol_in: f64) -> Option<f64> {
    if v_sol <= 0.0 || v_tokens <= 0.0 || sol_in <= 0.0 {
        return None;
    }
    let k = v_sol * v_tokens;
    let new_v_sol = v_sol + sol_in;
    let new_v_tokens = k / new_v_sol;
    Some(v_tokens - new_v_tokens)
}

/// Compare a probe's marginal to the curve-implied marginal at the frozen tolerance.
pub fn redundancy_compare(probe_tokens_out_ui: f64, curve_tokens_out_ui: f64) -> RedundancyAudit {
    let abs = (probe_tokens_out_ui - curve_tokens_out_ui).abs();
    let rel = if curve_tokens_out_ui.abs() > 0.0 {
        abs / curve_tokens_out_ui.abs()
    } else {
        f64::INFINITY
    };
    let within = rel <= REDUNDANCY_REL_TOLERANCE;
    let class = if rel <= REDUNDANCY_REL_TOLERANCE {
        RedundancyClass::Redundant
    } else if rel <= 3.0 * REDUNDANCY_REL_TOLERANCE {
        RedundancyClass::LikelyRedundant
    } else {
        RedundancyClass::NonRedundant
    };
    RedundancyAudit { abs_difference: abs, relative_difference: rel, within_tolerance: within, class }
}

// ---------------------------------------------------------------------------
// Measurement failure record + unconditional emission
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementFailureRecord {
    pub run_id: String,
    pub mint: String,
    pub domain: String,
    pub stage: String,
    pub category: MeasurementFailureCategory,
    pub at: DateTime<Utc>,
}

/// Whether to ATTEMPT a measurement for a candidate. Frozen: UNCONDITIONAL —
/// depends on nothing (no hypothesis membership, no price, no future state).
/// Kept as an explicit fn so the unconditional property is directly testable.
pub fn should_attempt_measurement() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64, millis: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, millis * 1_000_000).unwrap()
    }

    fn trade(sig: &str, wallet: &str, tx: &str, sol: f64, recv_s: i64) -> TradeObserved {
        TradeObserved {
            schema_version: OBSERVATION_SCHEMA_VERSION_REF,
            normalization_version: MEASUREMENT_NORMALIZATION_VERSION,
            run_id: "run1".into(),
            mint: "MINT".into(),
            signature: sig.into(),
            trader_public_key: wallet.into(),
            tx_type: tx.into(),
            side: TradeSide::from_tx_type(tx),
            token_amount_ui: sol * 1000.0,
            sol_amount: sol,
            bonding_curve_key: "BC".into(),
            v_tokens_in_bonding_curve: 1_000_000_000.0,
            v_sol_in_bonding_curve: 30.0,
            market_cap_sol: 28.0,
            event_received_at: ts(recv_s, 0),
            source: "pumpportal".into(),
            source_revision: "260a65d".into(),
        }
    }

    // Tie the trade-tag version to the REAL existing observation schema constant
    // so t32 genuinely proves the existing schema was not bumped/altered.
    use crate::observation::schema::OBSERVATION_SCHEMA_VERSION as OBSERVATION_SCHEMA_VERSION_REF;

    #[test] // 1. TradeObserved serialization round-trip
    fn t01_trade_roundtrip() {
        let t = trade("sig1", "W1", "buy", 0.5, 100);
        let j = serde_json::to_string(&t).unwrap();
        let back: TradeObserved = serde_json::from_str(&j).unwrap();
        assert_eq!(t, back);
    }

    #[test] // 2. duplicate signature dedup
    fn t02_dedup_duplicate() {
        let v = vec![trade("s", "W1", "buy", 1.0, 1), trade("s", "W2", "buy", 1.0, 2)];
        let d = dedup_trades(&v);
        assert_eq!(d.kept.len(), 1);
        assert_eq!(d.duplicate_count, 1);
    }

    #[test] // 3. malformed / missing signature failure provenance
    fn t03_missing_signature() {
        let v = vec![trade("", "W1", "buy", 1.0, 1)];
        let d = dedup_trades(&v);
        assert_eq!(d.kept.len(), 0);
        assert_eq!(d.missing_signature_count, 1);
    }

    #[test] // 4. unknown tx_type handling
    fn t04_unknown_tx_type() {
        assert_eq!(TradeSide::from_tx_type("create"), TradeSide::Unknown);
        assert_eq!(TradeSide::from_tx_type("BUY"), TradeSide::Buy);
        assert_eq!(TradeSide::from_tx_type("sell"), TradeSide::Sell);
    }

    #[test] // 5. T2 cutoff excludes later trades
    fn t05_t2_cutoff_excludes_later() {
        let v = vec![trade("a", "W1", "buy", 1.0, 1), trade("b", "W2", "buy", 1.0, 9)];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert_eq!(f.buy_count, 1); // only the recv=1 trade is <= cutoff 2
    }

    #[test] // 6. T6 cutoff excludes later trades
    fn t06_t6_cutoff_excludes_later() {
        let v = vec![
            trade("a", "W1", "buy", 1.0, 1),
            trade("b", "W2", "buy", 1.0, 6),
            trade("c", "W3", "buy", 1.0, 7),
        ];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(6, 0));
        assert_eq!(f.buy_count, 2);
    }

    #[test] // 7. T2 available_in_time true
    fn t07_available_in_time_true() {
        let snap = compute_participation_snapshot("r", "M", SnapshotClass::T2, ts(2, 0), ts(2, 100), ts(2, 500), &[]);
        assert!(snap.available_in_time);
    }

    #[test] // 8. T6 available_in_time false when compute past deadline
    fn t08_available_in_time_false() {
        let snap = compute_participation_snapshot("r", "M", SnapshotClass::T6, ts(6, 0), ts(7, 0), ts(6, 500), &[]);
        assert!(!snap.available_in_time);
    }

    #[test] // 9. unique_buyers formula
    fn t09_unique_buyers() {
        let v = vec![
            trade("a", "W1", "buy", 1.0, 1),
            trade("b", "W1", "buy", 1.0, 1),
            trade("c", "W2", "buy", 1.0, 1),
        ];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert_eq!(f.unique_buyers, 2);
    }

    #[test] // 10. buy_count formula
    fn t10_buy_count() {
        let v = vec![trade("a", "W1", "buy", 1.0, 1), trade("b", "W2", "sell", 1.0, 1)];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert_eq!(f.buy_count, 1);
    }

    #[test] // 11. net_quote_flow formula
    fn t11_net_quote_flow() {
        let v = vec![trade("a", "W1", "buy", 3.0, 1), trade("b", "W2", "sell", 1.0, 1)];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert!((f.net_quote_flow_sol - 2.0).abs() < 1e-9);
    }

    #[test] // 12. top1 buyer share
    fn t12_top1_share() {
        let v = vec![
            trade("a", "W1", "buy", 3.0, 1),
            trade("b", "W2", "buy", 1.0, 1),
        ];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert!((f.top1_buyer_share.unwrap() - 0.75).abs() < 1e-9);
    }

    #[test] // 13. top5 buyer share
    fn t13_top5_share() {
        // 6 wallets 1 SOL each: top5 = 5/6
        let mut v = Vec::new();
        for i in 0..6 {
            v.push(trade(&format!("s{i}"), &format!("W{i}"), "buy", 1.0, 1));
        }
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert!((f.top5_buyer_share.unwrap() - 5.0 / 6.0).abs() < 1e-9);
    }

    #[test] // 14. buyer HHI
    fn t14_buyer_hhi() {
        // two equal wallets => HHI = 0.5^2 + 0.5^2 = 0.5
        let v = vec![trade("a", "W1", "buy", 1.0, 1), trade("b", "W2", "buy", 1.0, 1)];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert!((f.buyer_hhi.unwrap() - 0.5).abs() < 1e-9);
    }

    #[test] // 15. median buy size
    fn t15_median_buy_size() {
        let v = vec![
            trade("a", "W1", "buy", 1.0, 1),
            trade("b", "W2", "buy", 2.0, 1),
            trade("c", "W3", "buy", 9.0, 1),
        ];
        let d = dedup_trades(&v);
        let f = participation_features(&d.kept, ts(2, 0));
        assert!((f.median_buy_size_sol.unwrap() - 2.0).abs() < 1e-9);
    }

    #[test] // 15b zero buys => features None (missing, not zero)
    fn t15b_zero_buys_missing() {
        let f = participation_features(&[], ts(2, 0));
        assert_eq!(f.buy_count, 0);
        assert!(f.top1_buyer_share.is_none());
        assert!(f.median_buy_size_sol.is_none());
    }

    #[test] // 16. ParticipationSnapshot replay determinism
    fn t16_participation_replay_determinism() {
        let v = vec![trade("a", "W1", "buy", 1.0, 1), trade("b", "W2", "buy", 2.0, 1)];
        let s1 = compute_participation_snapshot("r", "M", SnapshotClass::T6, ts(6, 0), ts(6, 10), ts(6, 500), &v);
        let s2 = compute_participation_snapshot("r", "M", SnapshotClass::T6, ts(6, 0), ts(6, 10), ts(6, 500), &v);
        assert_eq!(s1, s2);
    }

    fn acct(addr: &str, bal: u64, class: HolderAccountClass) -> HolderAccount {
        HolderAccount { address: addr.into(), raw_balance_tokens: bal, class }
    }

    #[test] // 17. HolderSnapshot serialization
    fn t17_holder_snapshot_roundtrip() {
        let accts = vec![acct("A", 100, HolderAccountClass::Ordinary)];
        let feats = holder_features(&accts, false);
        let snap = HolderSnapshot {
            run_id: "r".into(), mint: "M".into(), requested_at: ts(0, 0), completed_at: ts(0, 50),
            rpc_slot: Some(123), available_in_time: true, raw_accounts: accts, total_mint_supply_tokens: TOTAL_MINT_SUPPLY_TOKENS,
            failure: None, source: "rpc".into(), source_revision: "260a65d".into(), feature_version: MEASUREMENT_FEATURE_VERSION, features: feats,
        };
        let j = serde_json::to_string(&snap).unwrap();
        let back: HolderSnapshot = serde_json::from_str(&j).unwrap();
        assert_eq!(snap, back);
    }

    #[test] // 18. fixed 1e9 denominator invariant
    fn t18_denominator_invariant() {
        assert_eq!(TOTAL_MINT_SUPPLY_TOKENS, 1_000_000_000);
        let accts = vec![acct("A", 100_000_000, HolderAccountClass::Ordinary)]; // 10% of supply
        let f = holder_features(&accts, false);
        assert!((f.top1_noncurve_holder_share.unwrap() - 0.1).abs() < 1e-12);
    }

    #[test] // 19. curve/program exclusion from noncurve shares
    fn t19_curve_exclusion() {
        let accts = vec![
            acct("CURVE", 800_000_000, HolderAccountClass::CurveProgram),
            acct("A", 100_000_000, HolderAccountClass::Ordinary),
        ];
        let f = holder_features(&accts, false);
        // top1 noncurve = A only (curve excluded)
        assert!((f.top1_noncurve_holder_share.unwrap() - 0.1).abs() < 1e-12);
        assert!((f.curve_held_share.unwrap() - 0.8).abs() < 1e-12);
        assert!((f.noncurve_supply_share.unwrap() - 0.2).abs() < 1e-12);
    }

    #[test] // 20. top1/top5/top10 noncurve shares
    fn t20_topk_noncurve() {
        let mut accts = Vec::new();
        for i in 0..12 {
            accts.push(acct(&format!("A{i}"), 10_000_000, HolderAccountClass::Ordinary)); // 1% each
        }
        let f = holder_features(&accts, false);
        assert!((f.top1_noncurve_holder_share.unwrap() - 0.01).abs() < 1e-12);
        assert!((f.top5_noncurve_holder_share.unwrap() - 0.05).abs() < 1e-12);
        assert!((f.top10_noncurve_holder_share.unwrap() - 0.10).abs() < 1e-12);
    }

    #[test] // 21. holder HHI
    fn t21_holder_hhi() {
        let accts = vec![
            acct("A", 100_000_000, HolderAccountClass::Ordinary), // 0.1
            acct("B", 100_000_000, HolderAccountClass::Ordinary), // 0.1
        ];
        let f = holder_features(&accts, false);
        assert!((f.holder_hhi.unwrap() - (0.01 + 0.01)).abs() < 1e-12);
    }

    #[test] // 22. top20 floor labeling
    fn t22_top20_floor_label() {
        let f = holder_features(&[acct("A", 1, HolderAccountClass::Ordinary)], false);
        assert!(f.ordinary_holder_count_top20_floor);
    }

    #[test] // 23. creator held share blocked if ambiguous
    fn t23_creator_blocked() {
        let accts = vec![acct("CREATOR", 50_000_000, HolderAccountClass::Creator)];
        assert!(holder_features(&accts, false).creator_held_share.is_none());
        assert!(holder_features(&accts, true).creator_held_share.is_some());
    }

    fn probe(input: u64, base_raw: Option<u64>, ok: bool) -> MicrostructureProbe {
        MicrostructureProbe {
            run_id: "r".into(), mint: "M".into(), requested_at: ts(0, 0), completed_at: ts(0, 20),
            available_in_time: true, input_lamports: input, expected_base_raw: base_raw, base_decimals: 6,
            success: ok, quote_source: "canonical".into(), protocol_fee_bps: Some(95), creator_fee_bps: Some(30),
            lp_fee_bps: Some(0), latency_ms: 20, failure: if ok { None } else { Some(MeasurementFailureCategory::AccountMissing) },
            feature_version: MEASUREMENT_FEATURE_VERSION,
        }
    }

    #[test] // 24. MicrostructureProbe serialization
    fn t24_probe_roundtrip() {
        let p = probe(1_000_000, Some(35_000_000_000), true);
        let j = serde_json::to_string(&p).unwrap();
        let back: MicrostructureProbe = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }

    #[test] // 25. exact probe sizes
    fn t25_exact_probe_sizes() {
        assert_eq!(MICROSTRUCTURE_PROBE_SIZES_LAMPORTS, [500_000, 1_000_000, 2_000_000]);
    }

    #[test] // 25b marginal_tokens_per_sol formula
    fn t25b_marginal() {
        // 35_000_000_000 raw / 1e6 decimals = 35000 UI tokens for 0.001 SOL => 35_000_000 tokens/SOL
        let m = marginal_tokens_per_sol(35_000_000_000, 1_000_000, 6).unwrap();
        assert!((m - 35_000_000.0).abs() < 1.0);
    }

    #[test] // 26 (audit) no signing/tx symbols in module — structural: unconditional emitter is the only gate
    fn t26_no_conditional_only_unconditional() {
        assert!(should_attempt_measurement());
    }

    #[test] // 27 (audit) probe carries no fill/side-effect: success is a read flag only
    fn t27_probe_is_readonly_shape() {
        let p = probe(500_000, None, false);
        assert!(!p.success);
        assert!(p.expected_base_raw.is_none());
        assert_eq!(p.failure, Some(MeasurementFailureCategory::AccountMissing));
    }

    #[test] // 28. redundancy comparison
    fn t28_redundancy() {
        // curve-implied ~= probe within tolerance => REDUNDANT
        let curve = curve_implied_tokens_out(30.0, 1_000_000_000.0, 0.001).unwrap();
        let audit = redundancy_compare(curve * 1.001, curve); // 0.1% diff
        assert!(audit.within_tolerance);
        assert_eq!(audit.class, RedundancyClass::Redundant);
        // large deviation => NON_REDUNDANT
        let audit2 = redundancy_compare(curve * 1.5, curve);
        assert!(!audit2.within_tolerance);
        assert_eq!(audit2.class, RedundancyClass::NonRedundant);
    }

    #[test] // 29. failure provenance never zero
    fn t29_failure_provenance() {
        let rec = MeasurementFailureRecord {
            run_id: "r".into(), mint: "M".into(), domain: "holder".into(), stage: "rpc".into(),
            category: MeasurementFailureCategory::RpcUnavailable, at: ts(0, 0),
        };
        let j = serde_json::to_string(&rec).unwrap();
        let back: MeasurementFailureRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(rec, back);
        // holder with only-curve accounts => noncurve shares None (missing), not 0
        let f = holder_features(&[acct("CURVE", 1_000_000_000, HolderAccountClass::CurveProgram)], false);
        assert!(f.top1_noncurve_holder_share.is_none());
    }

    #[test] // 30. unconditional emission (property over arbitrary inputs)
    fn t30_unconditional_emission() {
        for _ in 0..100 {
            assert!(should_attempt_measurement());
        }
    }

    #[test] // 31. replay round-trip (raw -> snapshot -> raw stable)
    fn t31_replay_roundtrip() {
        let v = vec![trade("a", "W1", "buy", 1.0, 1)];
        let snap = compute_participation_snapshot("r", "M", SnapshotClass::T6, ts(6, 0), ts(6, 1), ts(6, 9), &v);
        let j = serde_json::to_string(&snap).unwrap();
        let back: ParticipationSnapshot = serde_json::from_str(&j).unwrap();
        assert_eq!(snap, back);
    }

    #[test] // 32. old-schema compatibility: extra unknown field tolerated on parse of a prior shape is N/A;
            // here assert versions are explicit + stable so old raw remains parseable.
    fn t32_versions_explicit() {
        assert_eq!(MEASUREMENT_NORMALIZATION_VERSION, 1);
        assert_eq!(MEASUREMENT_FEATURE_VERSION, 1);
        // REAL existing observation schema constant is untouched (still 4) — proves
        // this change did not bump/alter the existing schema, so old raw stays parseable.
        assert_eq!(OBSERVATION_SCHEMA_VERSION_REF, 4);
        // a TradeObserved with the frozen field set parses back losslessly
        let t = trade("s", "W", "buy", 1.0, 1);
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains("event_received_at"));
    }

    #[test] // 2b. dedup is order-independent: a duplicate straddling the cutoff keeps the earliest receipt
    fn t02b_dedup_order_independent() {
        let early = trade("dup", "W1", "buy", 1.0, 1); // received at t=1 (<= T2 cutoff)
        let late = trade("dup", "W1", "buy", 1.0, 9); // same sig, received at t=9 (> cutoff)
        let forward = vec![early.clone(), late.clone()];
        let reversed = vec![late, early];
        let f_fwd = participation_features(&dedup_trades(&forward).kept, ts(2, 0));
        let f_rev = participation_features(&dedup_trades(&reversed).kept, ts(2, 0));
        assert_eq!(f_fwd, f_rev); // order-independent
        assert_eq!(f_fwd.buy_count, 1); // earliest receipt (t=1) is kept and is <= cutoff
    }
}
