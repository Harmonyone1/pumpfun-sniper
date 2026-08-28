//! Pending-execution recovery planner.
//!
//! Observes a submitted-but-unreconciled transaction through the
//! [`TradeReconciler`] and turns the reconciliation outcome into a deterministic
//! [`PendingRecoveryPlan`]. This module is deliberately side-effect free:
//!
//! - it MUST NOT mutate `PositionManager` or `PendingExecutionStore`;
//! - it MUST NOT submit or re-submit any trade;
//! - it only OBSERVES (via the reconciler) and PLANS.
//!
//! Economics are taken exclusively from confirmed transaction metadata carried
//! by the [`ReconciledFill`] — never from the requested intent (INV-TX-001). In
//! particular a buy's cost basis is the ACTUAL confirmed SOL spent, not the
//! requested SOL.

use chrono::Utc;

use crate::error::{Error, Result};
use crate::position::manager::Position;
use crate::trading::pending::{PendingExecution, PendingExecutionContext, PendingSellIntent};
use crate::trading::reconciliation::{
    ReconciledFill, ReconciliationOutcome, ReconciliationSide, TradeReconciler,
};

/// A deterministic, side-effect-free plan describing what a caller SHOULD do
/// with a pending execution, derived purely from a reconciliation outcome.
///
/// Producing a plan mutates nothing. Applying it (opening/closing positions,
/// removing the pending record) is the caller's responsibility.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingRecoveryPlan {
    /// The submitted transaction is confirmed to have FAILED on-chain. Safe to
    /// drop the pending record; no position/economic effect.
    ConfirmedFailure {
        pending: PendingExecution,
        error: String,
        observed_after_ms: u64,
    },
    /// A confirmed BUY fill. Carries the canonical position the caller should
    /// record (built from confirmed metadata, actual cost basis).
    ConfirmedBuy {
        pending: PendingExecution,
        fill: ReconciledFill,
        position: Position,
    },
    /// A confirmed SELL fill. Carries the confirmed sold amount / proceeds /
    /// price plus the original intent, for the caller to apply against the
    /// position. Full vs partial is NOT inferred from intent.
    ConfirmedSell {
        pending: PendingExecution,
        fill: ReconciledFill,
        sold_amount_raw: u64,
        received_sol: f64,
        actual_price_sol_per_token: f64,
        intent: PendingSellIntent,
    },
    /// The transaction could not be resolved (still not visible, timed out,
    /// transient RPC failure). The caller MUST keep the pending record; a
    /// timeout is not failure proof.
    Unresolved {
        pending: PendingExecution,
        reason: String,
        observed_after_ms: u64,
    },
}

/// Turn a reconciliation `outcome` for `pending` into a deterministic plan.
///
/// Pure: no I/O, no mutation. On a `ConfirmedFill`, the fill's identity
/// (wallet/mint/side) must match the pending record exactly — a mismatch means
/// we observed the wrong transaction and is a hard error, never a silent
/// mis-application to the wrong position.
pub fn plan_pending_outcome(
    pending: &PendingExecution,
    outcome: ReconciliationOutcome,
) -> Result<PendingRecoveryPlan> {
    match outcome {
        ReconciliationOutcome::ConfirmedFailure {
            signature: _,
            error,
            observed_after_ms,
        } => Ok(PendingRecoveryPlan::ConfirmedFailure {
            pending: pending.clone(),
            error,
            observed_after_ms,
        }),

        ReconciliationOutcome::Unresolved {
            signature: _,
            reason,
            observed_after_ms,
        } => Ok(PendingRecoveryPlan::Unresolved {
            pending: pending.clone(),
            reason,
            observed_after_ms,
        }),

        ReconciliationOutcome::ConfirmedFill(fill) => {
            // Identity validation: the observed fill must be for exactly this
            // wallet/mint/side. Anything else means we looked at the wrong tx.
            if fill.wallet != pending.wallet {
                return Err(Error::TransactionReconciliation(format!(
                    "reconciled fill wallet '{}' does not match pending '{}' wallet '{}'",
                    fill.wallet, pending.signature, pending.wallet
                )));
            }
            if fill.mint != pending.mint {
                return Err(Error::TransactionReconciliation(format!(
                    "reconciled fill mint '{}' does not match pending '{}' mint '{}'",
                    fill.mint, pending.signature, pending.mint
                )));
            }
            if fill.side != pending.side {
                return Err(Error::TransactionReconciliation(format!(
                    "reconciled fill side {:?} does not match pending '{}' side {:?}",
                    fill.side, pending.signature, pending.side
                )));
            }

            match &pending.context {
                PendingExecutionContext::Buy(buy) => {
                    // Defense in depth: side already matched, but keep the
                    // context/side agreement explicit.
                    debug_assert_eq!(pending.side, ReconciliationSide::Buy);
                    build_buy_plan(pending, fill, buy)
                }
                PendingExecutionContext::Sell(sell) => {
                    debug_assert_eq!(pending.side, ReconciliationSide::Sell);
                    build_sell_plan(pending, fill, sell.intent)
                }
            }
        }
    }
}

/// Build a confirmed-BUY plan (canonical Position from confirmed metadata).
fn build_buy_plan(
    pending: &PendingExecution,
    fill: ReconciledFill,
    ctx: &crate::trading::pending::PendingBuyContext,
) -> Result<PendingRecoveryPlan> {
    // Raw token amount actually received, must fit u64 and be nonzero.
    let raw = fill.token_amount_raw().ok_or_else(|| {
        Error::TransactionReconciliation(format!(
            "confirmed buy '{}' token amount does not fit u64",
            pending.signature
        ))
    })?;
    if raw == 0 {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed buy '{}' has zero token amount",
            pending.signature
        )));
    }

    let decimals = fill.token_decimals;

    // ACTUAL cost basis = SOL actually spent (buy => negative wallet delta).
    // Never the requested SOL.
    let cost = -fill.wallet_sol_delta_sol();
    if !cost.is_finite() || cost <= 0.0 {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed buy '{}' has non-positive or non-finite actual cost: {}",
            pending.signature, cost
        )));
    }

    let actual_entry_price = fill.effective_price_sol_per_token().ok_or_else(|| {
        Error::TransactionReconciliation(format!(
            "confirmed buy '{}' has no derivable entry price",
            pending.signature
        ))
    })?;
    if !actual_entry_price.is_finite() || actual_entry_price <= 0.0 {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed buy '{}' has non-positive or non-finite entry price: {}",
            pending.signature, actual_entry_price
        )));
    }

    // Entry time from confirmed block_time when available; else now.
    let entry_time = match fill.block_time {
        Some(ts) => chrono::DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(Utc::now),
        None => Utc::now(),
    };

    let position = Position {
        mint: pending.mint.clone(),
        name: ctx.name.clone(),
        symbol: ctx.symbol.clone(),
        bonding_curve: ctx.bonding_curve.clone(),
        token_amount: raw,
        token_decimals: Some(decimals),
        entry_price: actual_entry_price,
        total_cost_sol: cost,
        entry_time,
        entry_signature: fill.signature.clone(),
        entry_type: ctx.entry_type,
        quick_profit_taken: false,
        second_profit_taken: false,
        peak_price: actual_entry_price,
        current_price: actual_entry_price,
        kill_switch_triggered: false,
        kill_switch_reason: None,
        wallet_pubkey: fill.wallet.clone(),
        applied_exit_signatures: vec![],
    };

    Ok(PendingRecoveryPlan::ConfirmedBuy {
        pending: pending.clone(),
        fill,
        position,
    })
}

/// Build a confirmed-SELL plan (confirmed proceeds + original intent).
fn build_sell_plan(
    pending: &PendingExecution,
    fill: ReconciledFill,
    intent: PendingSellIntent,
) -> Result<PendingRecoveryPlan> {
    let sold_amount_raw = fill.token_amount_raw().ok_or_else(|| {
        Error::TransactionReconciliation(format!(
            "confirmed sell '{}' token amount does not fit u64",
            pending.signature
        ))
    })?;
    if sold_amount_raw == 0 {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed sell '{}' has zero token amount",
            pending.signature
        )));
    }

    // Net proceeds; negative allowed (fee-dominated sells), must be finite.
    let received_sol = fill.wallet_sol_delta_sol();
    if !received_sol.is_finite() {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed sell '{}' has non-finite net SOL",
            pending.signature
        )));
    }

    // Price may be zero/negative for a fee-dominated sell; require only finite.
    let actual_price = fill.effective_price_sol_per_token().ok_or_else(|| {
        Error::TransactionReconciliation(format!(
            "confirmed sell '{}' has no derivable price",
            pending.signature
        ))
    })?;
    if !actual_price.is_finite() {
        return Err(Error::TransactionReconciliation(format!(
            "confirmed sell '{}' has non-finite price",
            pending.signature
        )));
    }

    Ok(PendingRecoveryPlan::ConfirmedSell {
        pending: pending.clone(),
        fill,
        sold_amount_raw,
        received_sol,
        actual_price_sol_per_token: actual_price,
        intent,
    })
}

/// Observe a pending execution through the reconciler and plan its recovery.
///
/// A structural reconciler error (bad signature, RPC join failure, unparseable
/// metadata) propagates as `Err` so the caller keeps the pending record and
/// retries later — it is never confused with a confirmed failure.
pub async fn reconcile_pending_execution(
    reconciler: &TradeReconciler,
    pending: &PendingExecution,
) -> Result<PendingRecoveryPlan> {
    let outcome = reconciler
        .reconcile(
            &pending.signature,
            &pending.wallet,
            &pending.mint,
            pending.side,
        )
        .await?;
    plan_pending_outcome(pending, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::manager::EntryType;
    use crate::trading::pending::{PendingBuyContext, PendingSellContext};

    const WALLET: &str = "Wallet111111111111111111111111111111111";
    const MINT: &str = "Mint11111111111111111111111111111111111";
    const SIG: &str = "sigfill";

    fn buy_pending() -> PendingExecution {
        PendingExecution::buy(
            SIG.to_string(),
            MINT.to_string(),
            WALLET.to_string(),
            PendingBuyContext {
                name: "Test Token".to_string(),
                symbol: "TEST".to_string(),
                bonding_curve: "curve111".to_string(),
                entry_type: EntryType::StrongBuy,
                requested_sol: 0.5, // deliberately != actual cost below
            },
        )
    }

    fn sell_pending(intent: PendingSellIntent) -> PendingExecution {
        PendingExecution::sell(
            SIG.to_string(),
            MINT.to_string(),
            WALLET.to_string(),
            PendingSellContext {
                requested_amount: "50000000".to_string(),
                intent,
                reason: "test".to_string(),
            },
        )
    }

    /// A confirmed BUY fill: +50 tokens (6 decimals), -0.12 SOL spent.
    fn buy_fill() -> ReconciledFill {
        ReconciledFill {
            signature: SIG.to_string(),
            slot: 100,
            block_time: Some(1_700_000_000),
            wallet: WALLET.to_string(),
            mint: MINT.to_string(),
            side: ReconciliationSide::Buy,
            token_delta_raw: 50_000_000,
            token_decimals: 6,
            wallet_sol_delta_lamports: -120_000_000,
            fee_lamports: 5_000,
            reconciliation_wait_ms: 0,
        }
    }

    /// A confirmed SELL fill: -30 tokens (6 decimals), +0.095 SOL net.
    fn sell_fill() -> ReconciledFill {
        ReconciledFill {
            signature: SIG.to_string(),
            slot: 101,
            block_time: Some(1_700_000_001),
            wallet: WALLET.to_string(),
            mint: MINT.to_string(),
            side: ReconciliationSide::Sell,
            token_delta_raw: -30_000_000,
            token_decimals: 6,
            wallet_sol_delta_lamports: 95_000_000,
            fee_lamports: 5_000,
            reconciliation_wait_ms: 0,
        }
    }

    #[test]
    fn test_pending_buy_fill_builds_canonical_position() {
        let pending = buy_pending();
        let plan = plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(buy_fill()))
            .unwrap();
        match plan {
            PendingRecoveryPlan::ConfirmedBuy { position, .. } => {
                assert_eq!(position.mint, MINT);
                assert_eq!(position.name, "Test Token");
                assert_eq!(position.symbol, "TEST");
                assert_eq!(position.bonding_curve, "curve111");
                assert_eq!(position.token_amount, 50_000_000);
                assert_eq!(position.token_decimals, Some(6));
                assert_eq!(position.entry_signature, SIG);
                assert_eq!(position.entry_type, EntryType::StrongBuy);
                assert_eq!(position.wallet_pubkey, WALLET);
                assert!(!position.quick_profit_taken);
                assert!(!position.second_profit_taken);
                assert!(!position.kill_switch_triggered);
                assert!(position.kill_switch_reason.is_none());
                assert!(position.applied_exit_signatures.is_empty());
                // entry_price == effective price = 0.12 / 50 = 0.0024
                assert!((position.entry_price - 0.0024).abs() < 1e-9);
                assert!((position.peak_price - 0.0024).abs() < 1e-9);
                assert!((position.current_price - 0.0024).abs() < 1e-9);
                // entry_time from block_time.
                assert_eq!(position.entry_time.timestamp(), 1_700_000_000);
            }
            other => panic!("expected ConfirmedBuy, got {:?}", other),
        }
    }

    #[test]
    fn test_pending_buy_uses_actual_cost_not_requested_sol() {
        let pending = buy_pending(); // requested_sol = 0.5
        let plan = plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(buy_fill()))
            .unwrap();
        match plan {
            PendingRecoveryPlan::ConfirmedBuy { position, .. } => {
                // Actual cost = 0.12 SOL, NOT requested 0.5.
                assert!((position.total_cost_sol - 0.12).abs() < 1e-9);
            }
            other => panic!("expected ConfirmedBuy, got {:?}", other),
        }
    }

    #[test]
    fn test_pending_buy_wrong_wallet_is_error() {
        let pending = buy_pending();
        let mut fill = buy_fill();
        fill.wallet = "WrongWallet".to_string();
        let err =
            plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(fill)).unwrap_err();
        assert!(matches!(err, Error::TransactionReconciliation(_)));
    }

    #[test]
    fn test_pending_buy_wrong_mint_is_error() {
        let pending = buy_pending();
        let mut fill = buy_fill();
        fill.mint = "WrongMint".to_string();
        let err =
            plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(fill)).unwrap_err();
        assert!(matches!(err, Error::TransactionReconciliation(_)));
    }

    #[test]
    fn test_pending_buy_wrong_side_is_error() {
        // Pending is a BUY, but the observed fill is tagged Sell.
        let pending = buy_pending();
        let mut fill = buy_fill();
        fill.side = ReconciliationSide::Sell;
        let err =
            plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(fill)).unwrap_err();
        assert!(matches!(err, Error::TransactionReconciliation(_)));
    }

    fn assert_sell_preserves_intent(intent: PendingSellIntent) {
        let pending = sell_pending(intent);
        let plan =
            plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(sell_fill()))
                .unwrap();
        match plan {
            PendingRecoveryPlan::ConfirmedSell {
                sold_amount_raw,
                received_sol,
                actual_price_sol_per_token,
                intent: got_intent,
                ..
            } => {
                assert_eq!(got_intent, intent);
                assert_eq!(sold_amount_raw, 30_000_000);
                assert!((received_sol - 0.095).abs() < 1e-9);
                // 0.095 / 30 = 0.0031666...
                assert!((actual_price_sol_per_token - (0.095 / 30.0)).abs() < 1e-9);
            }
            other => panic!("expected ConfirmedSell, got {:?}", other),
        }
    }

    #[test]
    fn test_pending_sell_plan_preserves_quick_profit_intent() {
        assert_sell_preserves_intent(PendingSellIntent::QuickProfit);
    }

    #[test]
    fn test_pending_sell_plan_preserves_second_profit_intent() {
        assert_sell_preserves_intent(PendingSellIntent::SecondProfit);
    }

    #[test]
    fn test_pending_sell_plan_preserves_manual_intent() {
        assert_sell_preserves_intent(PendingSellIntent::Manual);
    }

    #[test]
    fn test_pending_sell_plan_preserves_kill_switch_intent() {
        assert_sell_preserves_intent(PendingSellIntent::KillSwitch);
    }

    #[test]
    fn test_pending_sell_allows_negative_net_sol() {
        let pending = sell_pending(PendingSellIntent::Full);
        // Fee-dominated sell: tokens leave, wallet SOL nets negative.
        let mut fill = sell_fill();
        fill.wallet_sol_delta_lamports = -5_000;
        let plan =
            plan_pending_outcome(&pending, ReconciliationOutcome::ConfirmedFill(fill)).unwrap();
        match plan {
            PendingRecoveryPlan::ConfirmedSell {
                received_sol,
                sold_amount_raw,
                ..
            } => {
                assert_eq!(sold_amount_raw, 30_000_000);
                assert!(received_sol < 0.0);
                assert!((received_sol - (-0.000005)).abs() < 1e-12);
            }
            other => panic!("expected ConfirmedSell, got {:?}", other),
        }
    }

    #[test]
    fn test_confirmed_failure_remains_failure_plan() {
        let pending = buy_pending();
        let outcome = ReconciliationOutcome::ConfirmedFailure {
            signature: SIG.to_string(),
            error: "InstructionError".to_string(),
            observed_after_ms: 42,
        };
        let plan = plan_pending_outcome(&pending, outcome).unwrap();
        match plan {
            PendingRecoveryPlan::ConfirmedFailure {
                pending: p,
                error,
                observed_after_ms,
            } => {
                assert_eq!(p, pending);
                assert_eq!(error, "InstructionError");
                assert_eq!(observed_after_ms, 42);
            }
            other => panic!("expected ConfirmedFailure, got {:?}", other),
        }
    }

    #[test]
    fn test_unresolved_remains_unresolved_plan() {
        let pending = sell_pending(PendingSellIntent::Full);
        let outcome = ReconciliationOutcome::Unresolved {
            signature: SIG.to_string(),
            reason: "timed out".to_string(),
            observed_after_ms: 15_000,
        };
        let plan = plan_pending_outcome(&pending, outcome).unwrap();
        match plan {
            PendingRecoveryPlan::Unresolved {
                pending: p,
                reason,
                observed_after_ms,
            } => {
                assert_eq!(p, pending);
                assert_eq!(reason, "timed out");
                assert_eq!(observed_after_ms, 15_000);
            }
            other => panic!("expected Unresolved, got {:?}", other),
        }
    }
}
