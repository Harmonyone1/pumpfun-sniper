//! CLI command implementations

use anyhow::Result;
use dialoguer::Confirm;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::runtime::RuntimeLease;
use crate::stream::pumpportal::{CommandSender, PumpPortalSubscriptionPlan, SubscriptionCommand};
use crate::filter::{
    AdaptiveFilter, HeliusClient, KillSwitchDecision, KillSwitchEvaluator, MetadataSignalProvider,
    Recommendation, SignalContext, SmartMoneySignalProvider, WalletBehaviorSignalProvider,
    WalletProfiler, WalletProfilerConfig,
};
use crate::filter::signals::EarlyMomentumSignalProvider;
use crate::strategy::engine::StrategyEngine;
use crate::strategy::types::TradingAction;
use crate::stream::pumpportal::{PumpPortalClient, PumpPortalEvent};
#[cfg(feature = "shredstream")]
use crate::stream::shredstream::ShredStreamClient;
use crate::market::{MarketVenue, PumpMarketOracle};
use crate::trading::pumpportal_api::{PoolType, PumpPortalTrader};
use crate::trading::{
    reconcile_pending_execution, PendingBuyContext, PendingExecution, PendingExecutionStore,
    PendingSellContext, PendingSellIntent, ReconciliationOutcome, ReconciliationSide,
    TradeReconciler,
};
use crate::wallet::{ExecutionWalletRegistry, WalletOwnershipProbe};

fn persist_bought_mints(path: &str, map: &std::collections::HashMap<String, i64>) {
    match serde_json::to_string_pretty(map) {
        Ok(data) => {
            if let Err(err) = std::fs::write(path, data) {
                warn!("Failed to persist bought_mints cache: {}", err);
            }
        }
        Err(err) => warn!("Failed to serialize bought_mints cache: {}", err),
    }
}

async fn remove_bought_mint(
    store: &Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>>,
    path: &Arc<String>,
    mint: &str,
) -> bool {
    let mut guard = store.lock().await;
    let removed = guard.remove(mint).is_some();
    if removed {
        persist_bought_mints(path, &*guard);
    }
    removed
}

/// Route-pin a PumpPortal submission to the SAME venue an executable market quote
/// was computed on (MPT-001 Agent E3 / packet Sections 5, 17). A Pump bonding-curve
/// quote must submit `pool="pump"`; a canonical PumpSwap quote must submit
/// `pool="pump-amm"`. `Auto` is never produced here — a quoted path must not let the
/// router pick a different venue than the one whose price was confirmed.
fn pumpportal_pool_for_venue(venue: MarketVenue) -> PoolType {
    match venue {
        MarketVenue::PumpBondingCurve => PoolType::Pump,
        MarketVenue::PumpSwapCanonical => PoolType::PumpAmm,
    }
}

/// Convert a final SOL buy size to exact `u64` lamports for a market quote
/// (MPT-001 Agent E2). Fails closed on non-finite, non-positive, or overflowing
/// input: `floor(sol * 1e9)` with checked bounds and NO f64 rounding tricks.
///
/// Returns `None` (reject, do not submit) when the value cannot be represented as a
/// valid positive lamport amount.
fn sol_to_lamports_exact(sol: f64) -> Option<u64> {
    if !sol.is_finite() || sol <= 0.0 {
        return None;
    }
    let lamports = (sol * 1_000_000_000.0).floor();
    // floor of a finite positive value is finite and >= 0; guard the u64 range.
    if !lamports.is_finite() || lamports < 1.0 || lamports > u64::MAX as f64 {
        return None;
    }
    Some(lamports as u64)
}

/// Format an exact raw token amount as a decimal UI string (MPT-001 Agent F4).
///
/// Pure integer formatting — NO f64, so no precision loss or scientific notation.
/// The integer part is `raw / 10^decimals`; the fractional part is the remainder,
/// left-padded to `decimals` digits (preserving leading fractional zeros) and then
/// trimmed of trailing zeros. When the value is an exact multiple of `10^decimals`
/// the result has no decimal point at all.
///
/// Examples (decimals = 6): `1_234_567 => "1.234567"`, `500_000 => "0.5"`,
/// `2_000_000 => "2"`, `1 => "0.000001"`.
fn raw_token_amount_to_decimal_string(raw: u64, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let scale = 10u128.pow(decimals as u32);
    let raw = raw as u128;
    let int_part = raw / scale;
    let frac_part = raw % scale;
    if frac_part == 0 {
        return int_part.to_string();
    }
    // Zero-pad the fractional remainder to exactly `decimals` digits so leading
    // fractional zeros are preserved, then trim trailing zeros.
    let mut frac = format!("{:0width$}", frac_part, width = decimals as usize);
    while frac.ends_with('0') {
        frac.pop();
    }
    format!("{}.{}", int_part, frac)
}

/// Resolve the exact raw token amount for a percentage sell layer (MPT-001 Agent
/// F3). Integer division only: `"50%" => raw/2`, `"25%" => raw/4`, `"100%" => raw`.
/// Returns `None` (do not sell) when the layer is unknown or the exact raw size
/// would be zero — a price sell is never submitted for a zero-size quote.
fn layer_raw_amount(total_raw: u64, sell_pct: &str) -> Option<u64> {
    let amount = match sell_pct {
        "100%" => total_raw,
        "50%" => total_raw / 2,
        "25%" => total_raw / 4,
        _ => return None,
    };
    if amount == 0 {
        None
    } else {
        Some(amount)
    }
}

/// Convert a decimal UI token amount string to an exact raw `u64` using the
/// token's decimals (MPT-001 Agent I1). This is the inverse of
/// `raw_token_amount_to_decimal_string` for a normal manual numeric sell amount.
///
/// Pure string/integer math — NO f64, so no precision loss. Rejects:
/// - scientific notation (`e`/`E`);
/// - a leading sign (negative or explicit `+`);
/// - empty input, a bare `.`, or multiple `.`;
/// - non-digit characters;
/// - more fractional digits than `decimals` UNLESS every excess digit is `0`;
/// - integer/multiply/add overflow of `u64`.
///
/// Examples (decimals = 6): `"1.234567" => 1_234_567`, `"0.5" => 500_000`,
/// `"2" => 2_000_000`, `"1.2300" => 1_230_000` (trailing zeros ok),
/// `"1.2345678"` rejected (nonzero 7th/8th fractional digit).
fn decimal_token_amount_to_raw(input: &str, decimals: u8) -> crate::error::Result<u64> {
    use crate::error::Error;
    let s = input.trim();
    if s.is_empty() {
        return Err(Error::MarketData(
            "empty token amount is not a valid decimal".to_string(),
        ));
    }
    // Reject scientific notation and any sign; only [0-9] and a single '.' allowed.
    if s.contains('e') || s.contains('E') {
        return Err(Error::MarketData(format!(
            "scientific notation is not accepted for a token amount: '{}'",
            input
        )));
    }
    if s.starts_with('-') || s.starts_with('+') {
        return Err(Error::MarketData(format!(
            "signed token amount is not accepted: '{}'",
            input
        )));
    }

    let (int_str, frac_str) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // A bare "." or a value with more than one "." (split_once leaves a '.' in frac).
    if frac_str.contains('.') {
        return Err(Error::MarketData(format!(
            "malformed token amount (multiple decimal points): '{}'",
            input
        )));
    }
    if int_str.is_empty() && frac_str.is_empty() {
        return Err(Error::MarketData(format!(
            "malformed token amount: '{}'",
            input
        )));
    }
    // Treat an empty integer part (".5") as "0".
    let int_str = if int_str.is_empty() { "0" } else { int_str };
    if !int_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::MarketData(format!(
            "non-digit in integer part of token amount: '{}'",
            input
        )));
    }
    if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::MarketData(format!(
            "non-digit in fractional part of token amount: '{}'",
            input
        )));
    }

    let decimals = decimals as usize;
    // Split the fractional digits into the part that scales into raw units and any
    // excess beyond the token's precision. Excess is only allowed if all zeros.
    let (frac_used, frac_excess) = if frac_str.len() > decimals {
        frac_str.split_at(decimals)
    } else {
        (frac_str, "")
    };
    if frac_excess.bytes().any(|b| b != b'0') {
        return Err(Error::MarketData(format!(
            "token amount '{}' has more than {} fractional decimals",
            input, decimals
        )));
    }
    // Right-pad the used fractional digits to exactly `decimals` so it lines up with
    // 10^decimals scaling.
    let mut frac_scaled = frac_used.to_string();
    while frac_scaled.len() < decimals {
        frac_scaled.push('0');
    }

    let int_val: u64 = int_str
        .parse()
        .map_err(|_| Error::MarketData(format!("integer part overflows u64: '{}'", input)))?;
    let scale = 10u64
        .checked_pow(decimals as u32)
        .ok_or_else(|| Error::MarketData(format!("decimals {} overflow scale", decimals)))?;
    let frac_val: u64 = if frac_scaled.is_empty() {
        0
    } else {
        frac_scaled
            .parse()
            .map_err(|_| Error::MarketData(format!("fractional part overflows u64: '{}'", input)))?
    };
    int_val
        .checked_mul(scale)
        .and_then(|hi| hi.checked_add(frac_val))
        .ok_or_else(|| Error::MarketData(format!("token amount overflows u64: '{}'", input)))
}

/// Resolve an arbitrary manual sell percentage to an EXACT raw proportion of a
/// position's raw balance (MPT-001 Agent I1). Integer math only (u128): the
/// percentage string is parsed to a fixed-point value scaled by `10^6` percent-
/// decimals, then `total_raw * pct_scaled / (100 * 10^6)` with flooring. Rejects a
/// zero result (nothing to sell). The percentage validity range is enforced by the
/// caller; this only needs a well-formed non-negative decimal string.
fn percent_of_raw(total_raw: u64, pct_str: &str) -> crate::error::Result<u64> {
    use crate::error::Error;
    const PCT_DECIMALS: u8 = 6;
    // Reuse the exact decimal->raw parser to get pct scaled by 10^6, rejecting
    // scientific notation / signs / excess precision consistently.
    let pct_scaled = decimal_token_amount_to_raw(pct_str, PCT_DECIMALS)?;
    let denom: u128 = 100u128 * 10u128.pow(PCT_DECIMALS as u32);
    let raw = (total_raw as u128)
        .checked_mul(pct_scaled as u128)
        .map(|hi| hi / denom)
        .ok_or_else(|| Error::MarketData("percentage proportion overflow".to_string()))?;
    if raw == 0 {
        return Err(Error::MarketData(format!(
            "percentage {}% of {} raw resolves to zero tokens",
            pct_str, total_raw
        )));
    }
    if raw > u64::MAX as u128 {
        return Err(Error::MarketData(
            "percentage proportion exceeds u64".to_string(),
        ));
    }
    Ok(raw as u64)
}

/// The exact raw amount + venue-pinned pool a manual sell will submit, decided
/// AFTER a fresh executable quote is obtained (MPT-001 Agent I3/I5). A normal
/// manual sell may only submit when a same-venue SOL quote exists; otherwise it
/// must refuse (no Auto, no oracle bypass — `--force` only skips the human
/// prompt). This pure decision function makes that requirement testable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManualSellDecision {
    /// A fresh executable quote confirmed the venue; submit this pinned pool.
    Submit { pool: PoolType },
    /// No usable same-venue SOL quote; refuse the normal manual sell.
    Refuse,
}

/// I5(5)/(6)/(7): decide whether a manual sell may submit, given the fresh quote.
/// Requires `Some(quote)` that is a SOL pair; pins the pool to the quoted venue.
fn manual_sell_decision(quote: Option<&crate::market::ExecutableQuote>) -> ManualSellDecision {
    match quote {
        Some(q) if q.is_sol_pair() => ManualSellDecision::Submit {
            pool: pumpportal_pool_for_venue(q.venue),
        },
        _ => ManualSellDecision::Refuse,
    }
}

/// BLOCKER B — validate the FINAL exact-size manual-sell quote fetched
/// immediately before live submission (the second of the two-quote semantics).
///
/// The PREVIEW quote shown to the human is display-only and may be arbitrarily
/// stale by the time confirmation returns. This pure helper validates the FRESH
/// final quote against the exact intended sale and derives the pinned pool from
/// the FINAL quote venue (never the preview, never `Auto`).
///
/// Checks (all must hold, else `Err(Error::MarketData(..))` => refuse the sell):
/// - `quote.mint == expected_mint`
/// - `quote.side == MarketSide::Sell`
/// - `quote.base_amount_raw == expected_raw` (exact size)
/// - `quote.is_sol_pair()` (supported SOL quote asset)
/// - `expected_price_sol_per_token` is `Some(finite > 0)`
///
/// On success returns the pool pinned to the FINAL venue: a token that graduated
/// during the human delay (preview Pump -> final PumpSwap) yields
/// `PoolType::PumpAmm` here, without a second confirmation.
fn validate_final_manual_sell_quote(
    expected_mint: &Pubkey,
    expected_raw: u64,
    quote: &crate::market::ExecutableQuote,
) -> crate::error::Result<PoolType> {
    if quote.mint != *expected_mint {
        return Err(crate::error::Error::MarketData(format!(
            "final manual-sell quote mint {} does not match intended mint {}; refusing sell",
            quote.mint, expected_mint
        )));
    }
    if quote.side != crate::market::MarketSide::Sell {
        return Err(crate::error::Error::MarketData(format!(
            "final manual-sell quote side is {:?}, expected Sell; refusing sell",
            quote.side
        )));
    }
    if quote.base_amount_raw != expected_raw {
        return Err(crate::error::Error::MarketData(format!(
            "final manual-sell quote base size {} raw does not match intended size {} raw; refusing sell",
            quote.base_amount_raw, expected_raw
        )));
    }
    if !quote.is_sol_pair() {
        return Err(crate::error::Error::MarketData(format!(
            "final manual-sell quote for {} is not a supported SOL pair (venue {:?}); refusing sell",
            expected_mint, quote.venue
        )));
    }
    match quote.expected_price_sol_per_token {
        Some(p) if p.is_finite() && p > 0.0 => {}
        other => {
            return Err(crate::error::Error::MarketData(format!(
                "final manual-sell quote has no finite positive expected price ({:?}); refusing sell",
                other
            )));
        }
    }
    Ok(pumpportal_pool_for_venue(quote.venue))
}

/// Quote-to-fill execution drift for a SELL, as a percentage (MPT-001 Agent I4).
/// Positive means the fill was WORSE than the quote: `(expected - actual)/expected
/// * 100`. Returns `None` when `expected` is not finite/positive (never fabricated)
/// or `actual` is not finite. This mirrors the strategy-side sell drift definition;
/// the manual path only DISPLAYS it (no StrategyEngine present).
fn manual_sell_drift_pct(expected: f64, actual: f64) -> Option<f64> {
    if !expected.is_finite() || expected <= 0.0 || !actual.is_finite() {
        return None;
    }
    Some((expected - actual) / expected * 100.0)
}

/// Which fresh-MARK rule produced a price-exit candidate (MPT-001 Agent F2). The
/// candidate is identified from the fresh on-chain mark, but a price-based sell is
/// only submitted after the SAME condition is re-confirmed against the exact-size
/// EXECUTABLE QUOTE price. Time/no-movement categories do not depend on price, so
/// they are confirmed by the mere existence of a valid same-venue SOL quote.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PriceExitCategory {
    /// P&L from entry <= -stop_loss_pct.
    StopLoss { entry_price: f64, sl_pct: f64 },
    /// In profit AND dropped >= trailing_stop_pct from peak.
    TrailingStop {
        entry_price: f64,
        peak_price: f64,
        trailing_pct: f64,
    },
    /// P&L from entry >= take_profit_pct.
    TakeProfit { entry_price: f64, tp_pct: f64 },
    /// Partial: P&L from entry >= quick_profit_pct (and below take-profit).
    QuickProfit {
        entry_price: f64,
        qp_pct: f64,
        tp_pct: f64,
    },
    /// Time-based (no-movement / max-hold). Price-independent.
    TimeBased,
}

impl PriceExitCategory {
    /// Re-evaluate the exit condition against an arbitrary price (MPT-001 Agent F2).
    /// For price-based categories the same rule that fired on the mark must still
    /// hold against `price` (the executable quote's expected SOL/token). Time-based
    /// categories are price-independent and always confirm here — the wall-clock
    /// condition was already established from the tracked position.
    fn confirms_at(&self, price: f64) -> bool {
        if !price.is_finite() || price <= 0.0 {
            return false;
        }
        let pnl = |entry: f64| {
            if entry > 0.0 {
                ((price - entry) / entry) * 100.0
            } else {
                0.0
            }
        };
        match *self {
            PriceExitCategory::StopLoss { entry_price, sl_pct } => pnl(entry_price) <= -sl_pct,
            PriceExitCategory::TrailingStop {
                entry_price,
                peak_price,
                trailing_pct,
            } => {
                let pnl_pct = pnl(entry_price);
                let drop = if peak_price > 0.0 {
                    ((peak_price - price) / peak_price) * 100.0
                } else {
                    0.0
                };
                pnl_pct > 0.0 && drop >= trailing_pct
            }
            PriceExitCategory::TakeProfit { entry_price, tp_pct } => pnl(entry_price) >= tp_pct,
            PriceExitCategory::QuickProfit {
                entry_price,
                qp_pct,
                tp_pct,
            } => {
                let pnl_pct = pnl(entry_price);
                pnl_pct >= qp_pct && pnl_pct < tp_pct
            }
            PriceExitCategory::TimeBased => true,
        }
    }
}

/// Start the sniper bot
pub async fn start(config: &Config, dry_run: bool) -> Result<()> {
    // D1 / INV-RUN-001/002: acquire the exclusive runtime lease for this
    // credentials_dir BEFORE opening any stream, loading PositionManager /
    // PendingExecutionStore, recovering wallets, or submitting any transaction.
    // Held for the entire function lifetime; nonce-checked Drop releases it. No
    // environment override.
    let _runtime_lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "start")
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    if dry_run {
        warn!("Running in DRY-RUN mode - no real trades will be executed");
    }

    info!("Starting pump.fun sniper bot...");
    info!(
        "Buy amount: {} SOL, Slippage: {}bps",
        config.trading.buy_amount_sol, config.trading.slippage_bps
    );

    // Initialize components
    info!("Initializing RPC client...");
    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    ));

    // MPT-001 E1: authoritative market oracle over the SAME blocking RPC client.
    // No background task; fresh coherent chain observation per call. Used to gate
    // and route-pin the primary strategy-driven buy path below.
    let market_oracle = Arc::new(PumpMarketOracle::new(rpc_client.clone()));

    // Load keypair for local signing
    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| "credentials/hot-trading/keypair.json".to_string());
    let keypair_data = std::fs::read_to_string(&keypair_path)?;
    let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
    let keypair = Arc::new(Keypair::from_bytes(&secret_key)?);
    info!("Loaded keypair: {}", keypair.pubkey());

    // Initialize trader based on configuration
    // Force Local API if configured (0.5% fee vs 1% for Lightning)
    let use_local_api = config.pumpportal.api_key.is_empty() || config.pumpportal.force_local_api;

    // Exact execution-wallet resolution. Lightning and Local execution wallets are
    // NOT interchangeable (INV-WALLET-001). We never fall back to the local keypair
    // for a Lightning execution wallet.
    let primary_execution_wallet: Option<Pubkey> = if use_local_api {
        // Local mode signs with the local keypair.
        Some(keypair.pubkey())
    } else if !dry_run && config.pumpportal.use_for_trading {
        // Live Lightning mode: the configured lightning wallet must be present and
        // valid. Fail closed BEFORE any listening/trading if it is not.
        let lw = config.pumpportal.lightning_wallet.trim();
        if lw.is_empty() {
            return Err(anyhow::anyhow!(
                "Lightning execution requires config.pumpportal.lightning_wallet to be set; refusing to trade"
            ));
        }
        match Pubkey::from_str(lw) {
            Ok(pk) => {
                info!("Lightning execution wallet: {}", pk);
                Some(pk)
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Lightning execution wallet is not a valid Pubkey: {}",
                    e
                ));
            }
        }
    } else {
        // Dry-run or non-trading: attempt to parse for logging, but only warn.
        let lw = config.pumpportal.lightning_wallet.trim();
        if lw.is_empty() {
            warn!("No lightning_wallet configured (dry-run/non-trading) - execution wallet unresolved");
            None
        } else {
            match Pubkey::from_str(lw) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    warn!("lightning_wallet is not a valid Pubkey (dry-run/non-trading): {}", e);
                    None
                }
            }
        }
    };

    let pumpportal_trader = if config.pumpportal.use_for_trading {
        info!("Using PumpPortal API for trading");
        if use_local_api {
            if config.pumpportal.force_local_api {
                info!("Force Local API enabled - using Local API (0.5% fee, saving 0.5% per trade)");
            } else {
                info!("No API key configured - using Local API (sign + send locally)");
            }
            Some(PumpPortalTrader::local())
        } else {
            info!("Using Lightning API (1% fee) - consider force_local_api=true to save 0.5%");
            Some(PumpPortalTrader::lightning(
                config.pumpportal.api_key.clone(),
            ))
        }
    } else {
        info!("Using Jito bundles for trading");
        None
    };

    // Initialize Jito client (for bundle submission if not using PumpPortal)
    if !config.pumpportal.use_for_trading {
        info!("Initializing Jito client...");
        // TODO: Initialize Jito client
    }

    // Set up event channel
    let (event_tx, mut event_rx) =
        mpsc::channel::<PumpPortalEvent>(config.backpressure.channel_capacity);

    // D2 / INV-EVT-001: construct the single PumpPortal client for this runtime
    // now, but DO NOT open the socket yet. `PumpPortalClient::new` does not
    // connect — only `start(plan)` does — so we can clone its command sender into
    // the position monitor here while deferring stream startup until AFTER
    // PositionManager load, pending/legacy recovery, strategy restore and
    // kill-switch init (see the "D2/D3/D4" block just before the event loop).
    //
    // D5: one start runtime => one socket => one retained Option<CommandSender>.
    // D12: shared data-stream readiness flag. Connected => true; Disconnected /
    // Error => false. New-entry admission (when the feed is enabled) additionally
    // requires this to be true; exits are NEVER gated on it.
    let data_stream_ready = Arc::new(AtomicBool::new(false));
    let (pumpportal_client, pumpportal_command_sender): (
        Option<Arc<PumpPortalClient>>,
        Option<CommandSender>,
    ) = if config.pumpportal.enabled {
        let pumpportal_config = crate::stream::pumpportal::PumpPortalConfig {
            ws_url: config.pumpportal.ws_url.clone(),
            api_key: config.pumpportal.api_key.clone(),
            reconnect_delay_ms: config.pumpportal.reconnect_delay_ms,
            max_reconnect_attempts: config.pumpportal.max_reconnect_attempts,
            ping_interval_secs: config.pumpportal.ping_interval_secs,
        };
        let client = Arc::new(PumpPortalClient::new(pumpportal_config, event_tx.clone()));
        let sender = client.get_command_sender();
        (Some(client), Some(sender))
    } else {
        info!("Connecting to ShredStream for token detection...");
        // TODO: Connect to ShredStream when available
        warn!("ShredStream not yet implemented - enable PumpPortal in config");
        (None, None)
    };

    // Initialize position manager
    info!("Loading positions...");
    let position_manager = std::sync::Arc::new(crate::position::manager::PositionManager::new(
        config.safety.clone(),
        Some(format!("{}/positions.json", config.wallet.credentials_dir)),
    ));
    position_manager
        .load()
        .await
        .map_err(|e| anyhow::anyhow!(
            "Failed to load persisted positions; refusing to start with unknown ownership state: {}",
            e
        ))?;

    // Initialize the trade reconciler (001A). Uses the default reviewed config
    // (250ms polling / 15s timeout) - do not change here.
    let trade_reconciler = Arc::new(TradeReconciler::new(rpc_client.clone()));

    // === D1: recovery-only exact controlled-wallet registry + ownership probe ===
    // This registry exists ONLY so restart recovery can recognize positions
    // created previously (including by HotScan multi-wallet mode). It is NOT used
    // for primary new-buy selection. Local signing authority and Lightning wallet
    // authority are distinct (INV-WALLET-001); we never fall back between them.
    //
    // Local set = primary local keypair + every successfully loaded
    // MultiWalletManager local wallet (when trading_wallets is non-empty).
    let mut recovery_local_wallets: Vec<Pubkey> = Vec::new();
    let recovery_multi_wallet = if !config.wallet.trading_wallets.is_empty() {
        match crate::wallet::MultiWalletManager::new(
            config.wallet.trading_wallets.clone(),
            &config.wallet.selection_strategy,
        ) {
            Ok(mw) => {
                info!(
                    "Recovery registry: multi-wallet recognized with {} wallet(s)",
                    mw.wallet_count()
                );
                for w in mw.wallets() {
                    recovery_local_wallets.push(w.pubkey());
                }
                Some(Arc::new(mw))
            }
            Err(e) => {
                // Fail closed: configured multi-wallets that will not load leave
                // recovery unable to recognize prior positions.
                return Err(anyhow::anyhow!(
                    "Failed to load configured trading_wallets for recovery registry; refusing to start: {}",
                    e
                ));
            }
        }
    } else {
        None
    };
    let _ = &recovery_multi_wallet; // used later by kill-switch/hotscan agents

    // Strictly parse the configured Lightning wallet, if present. An invalid
    // non-empty Lightning wallet fails closed (INV-WALLET-001/002).
    let recovery_lightning_wallet: Option<Pubkey> = {
        let lw = config.pumpportal.lightning_wallet.trim();
        if lw.is_empty() {
            None
        } else {
            match Pubkey::from_str(lw) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Configured lightning_wallet is not a valid Pubkey; refusing to start recovery/trading: {}",
                        e
                    ));
                }
            }
        }
    };

    // Arc so the primary auto-sell monitor (spawned) can share the exact same
    // route authority the synchronous startup recovery and event kill-switch use.
    let recovery_registry_arc = Arc::new(ExecutionWalletRegistry::new(
        keypair.pubkey(),
        &recovery_local_wallets,
        recovery_lightning_wallet,
    ));
    let recovery_registry: &ExecutionWalletRegistry = &recovery_registry_arc;
    let recovery_probe = WalletOwnershipProbe::new(rpc_client.clone());

    // Initialize the persistent pending-execution journal. A submitted signature is
    // submission identity, not fill proof (INV-TX-001); the journal records
    // in-flight signatures so unresolved state survives restarts.
    let pending_path = format!(
        "{}/pending_executions.json",
        config.wallet.credentials_dir
    );
    let pending_executions = Arc::new(PendingExecutionStore::new(pending_path));
    pending_executions.load().await?;
    pending_executions.ensure_writable().await?;

    // Fail-closed halt flag for NEW entries. Set when unresolved transaction or
    // ownership state remains AFTER recovery (D5). No operator override.
    let new_entries_halted = Arc::new(AtomicBool::new(false));

    // === D5: startup transaction/position recovery ===
    // Atomic PositionManager is already loaded (fail-closed) and the pending
    // journal is loaded + ensured writable above. Now:
    //   1. recover the pending journal against confirmed chain state (D2);
    //   2. recover legacy/incomplete positions from chain evidence (D3/D4);
    //   3. halt NEW entries iff any unresolved pending OR recovery-required /
    //      unroutable position remains. If recovery fully succeeds, entries may
    //      resume.
    let pending_summary =
        recover_pending_store(&trade_reconciler, &pending_executions, &position_manager).await?;
    info!(
        "Pending recovery: recovered={}, confirmed_failures_removed={}, still_unresolved={}, accounting_conflicts={}",
        pending_summary.recovered,
        pending_summary.confirmed_failures_removed,
        pending_summary.still_unresolved,
        pending_summary.accounting_conflicts
    );

    let legacy_summary = recover_legacy_positions(
        &trade_reconciler,
        &recovery_probe,
        recovery_registry,
        &position_manager,
    )
    .await?;
    info!(
        "Legacy recovery: recovered={}, resolved_zero={}, still_recovery_required={}",
        legacy_summary.recovered, legacy_summary.resolved_zero, legacy_summary.still_recovery_required
    );

    // Re-inspect remaining positions after recovery for any that are still
    // recovery-required or unroutable (defense in depth beyond the counters).
    let post_recovery_positions = position_manager.get_all_positions().await;
    let residual_blocked = post_recovery_positions
        .iter()
        .filter(|p| legacy_recovery_required(p, recovery_registry))
        .count();

    if !pending_summary.fully_resolved()
        || !legacy_summary.fully_resolved()
        || residual_blocked > 0
    {
        error!(
            "New entries HALTED after recovery: unresolved_pending={}, legacy_unresolved={}, residual_blocked_positions={}",
            pending_summary.still_unresolved, legacy_summary.still_recovery_required, residual_blocked
        );
        new_entries_halted.store(true, Ordering::SeqCst);
    } else {
        info!("Startup recovery complete: transaction/position truth restored; new entries may resume.");
    }

    // Initialize kill-switch evaluator
    let kill_switch_evaluator = if config.smart_money.kill_switches.enabled {
        info!("Initializing kill-switch evaluator...");
        let evaluator = Arc::new(KillSwitchEvaluator::new(
            config.smart_money.kill_switches.clone(),
            config.smart_money.holder_watcher.clone(),
        ));
        info!(
            "Kill-switches enabled: deployer_sell={}, top_holder_sell={}",
            config.smart_money.kill_switches.deployer_sell_any,
            config.smart_money.kill_switches.top_holder_sell
        );
        Some(evaluator)
    } else {
        info!("Kill-switches disabled");
        None
    };

    // Initialize token filter
    let token_filter = crate::filter::token_filter::TokenFilter::new(config.filters.clone())
        .map_err(|e| anyhow::anyhow!("Failed to create token filter: {}", e))?;

    // Initialize Helius client and WalletProfiler for smart money signals
    let (helius_client, wallet_profiler) = if config.smart_money.enabled {
        if let Some(helius) = HeliusClient::from_rpc_url(&config.rpc.endpoint) {
            info!("Smart money signals ENABLED - Helius client initialized");
            let helius_arc = Arc::new(helius);
            let profiler = Arc::new(WalletProfiler::new(
                helius_arc.clone(),
                WalletProfilerConfig::default(),
            ));
            (Some(helius_arc), Some(profiler))
        } else {
            warn!("Smart money enabled but Helius API key not found in RPC URL");
            (None, None)
        }
    } else {
        (None, None)
    };

    // Initialize adaptive filter if enabled
    let adaptive_filter = if config.adaptive_filter.enabled {
        info!("Initializing adaptive filter...");
        let mut filter = AdaptiveFilter::new(config.adaptive_filter.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create adaptive filter: {}", e))?;

        // Register signal providers
        let metadata_provider = Arc::new(MetadataSignalProvider::new());
        filter.register_provider(metadata_provider);

        let wallet_provider = Arc::new(WalletBehaviorSignalProvider::new(filter.cache().clone()));
        filter.register_provider(wallet_provider);

        // Register early momentum signal provider
        let early_momentum = Arc::new(EarlyMomentumSignalProvider::new(
            config.early_detection.clone(),
        ));
        filter.register_provider(early_momentum);

        // Register smart money signal provider if profiler available
        if let Some(ref profiler) = wallet_profiler {
            let smart_money = Arc::new(SmartMoneySignalProvider::new(profiler.clone()));
            filter.register_provider(smart_money);
            info!("Smart money signal provider registered");
        }

        let provider_count = if wallet_profiler.is_some() { 4 } else { 3 };
        if filter.is_degraded().await {
            warn!("Adaptive filter running in degraded mode - some signals may be unavailable");
        } else {
            info!("Adaptive filter initialized with {} providers", provider_count);
        }

        Some(filter)
    } else {
        info!("Adaptive filter disabled - using basic filtering only");
        None
    };

    // Initialize strategy engine if enabled
    let strategy_engine = if config.strategy.enabled {
        info!("Initializing aggressive strategy engine...");
        let mut engine = StrategyEngine::new(config.strategy.clone());

        // Share filter cache with strategy engine if available
        if let Some(ref filter) = adaptive_filter {
            engine.set_filter_cache(filter.cache().clone());
        }

        info!(
            "Strategy engine initialized: default_strategy={}, max_positions={}, max_exposure={} SOL",
            config.strategy.default_strategy,
            config.strategy.portfolio_risk.max_concurrent_positions,
            config.strategy.portfolio_risk.max_exposure_sol
        );

        Some(Arc::new(tokio::sync::RwLock::new(engine)))
    } else {
        info!("Strategy engine disabled - using basic mode");
        None
    };

    // === D6: StrategyEngine restart rebuild ===
    // AFTER engine creation and BEFORE monitor/event processing, restore the
    // daily realized P&L safety floor and rebuild portfolio exposure from the
    // canonical PositionManager positions. State restore ONLY — no execution
    // feedback / chain-health / slippage / latency samples (INV-STRAT-002).
    // A position that cannot be restored halts new entries (INV-STRAT-001).
    if let Some(ref engine) = strategy_engine {
        let stats = position_manager.get_daily_stats().await;
        {
            let mut guard = engine.write().await;
            if !guard.restore_daily_realized_pnl(stats.net_pnl_sol).await {
                warn!(
                    "Daily realized P&L restore skipped (non-finite value {})",
                    stats.net_pnl_sol
                );
            } else {
                info!(
                    "Restored daily realized P&L safety floor: {} SOL",
                    stats.net_pnl_sol
                );
            }
        }

        let mut restored = 0usize;
        let mut unrestorable = 0usize;
        for position in position_manager.get_all_positions().await {
            if position_is_canonical_for_restore(&position, recovery_registry) {
                let strategy_position = manager_position_to_strategy_position(
                    &position,
                    config.strategy.default_strategy.clone(),
                );
                engine.write().await.record_entry(strategy_position).await;
                restored += 1;
            } else {
                unrestorable += 1;
            }
        }
        info!(
            "Strategy exposure rebuilt: {} canonical position(s) restored, {} not restorable",
            restored, unrestorable
        );
        if unrestorable > 0 {
            error!(
                "New entries HALTED: {} position(s) could not be restored into strategy exposure",
                unrestorable
            );
            new_entries_halted.store(true, Ordering::SeqCst);
        }
    }

    // Track wallets for copy trading
    let tracked_wallets: std::collections::HashSet<String> =
        config.wallet_tracking.wallets.iter().cloned().collect();

    info!("Starting price feed...");
    // Wrap trader in Arc for sharing across tasks
    let trader_arc: Option<std::sync::Arc<PumpPortalTrader>> =
        pumpportal_trader.map(std::sync::Arc::new);

    // === B4: INDEPENDENT PER-POSITION EXIT TRADER HANDLES ===
    // `trader_arc` above encodes the NEW-BUY execution mode (force_local_api etc).
    // Exits must NOT be governed by the new-buy mode: an existing Lightning-owned
    // position must be able to exit via Lightning even when force_local_api routes
    // new buys Local, and an additional-local recovered wallet must be able to
    // exit Local even when new buys use Lightning. So we build exit trader handles
    // keyed purely on which credentials exist (INV-WALLET-001/003):
    //   - Local exit available iff PumpPortal trading is enabled;
    //   - Lightning exit available iff PumpPortal trading is enabled AND an API
    //     key is configured (non-empty).
    let primary_exit_local_trader: Option<Arc<PumpPortalTrader>> =
        if config.pumpportal.use_for_trading {
            Some(Arc::new(PumpPortalTrader::local()))
        } else {
            None
        };
    let primary_exit_lightning_trader: Option<Arc<PumpPortalTrader>> =
        if config.pumpportal.use_for_trading && !config.pumpportal.api_key.trim().is_empty() {
            Some(Arc::new(PumpPortalTrader::lightning(
                config.pumpportal.api_key.clone(),
            )))
        } else {
            None
        };

    // === B7: shared same-mint primary sell coordinator ===
    // ONE instance, cloned into BOTH the primary auto-sell monitor and the event
    // kill-switch so those two concurrent sell producers cannot race and submit
    // two sells for the same mint before either signature is journaled.
    let active_sell_mints: ActiveSellMints =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // === B6: halt NEW entries when any canonical position is operationally
    // unexitable with the CURRENT exit credentials/routes. Strategy exposure was
    // already restored above for canonical positions (D6); this is an independent
    // count that does not touch exposure. Log mint + public wallet + reason only.
    {
        let local_exit_available = primary_exit_local_trader.is_some();
        let lightning_exit_available = primary_exit_lightning_trader.is_some();
        let mut unexitable = 0usize;
        for position in position_manager.get_all_positions().await {
            if !position_has_operational_exit_route(
                &position,
                recovery_registry,
                local_exit_available,
                lightning_exit_available,
            ) {
                unexitable += 1;
                let reason = if position.token_decimals.is_none() {
                    "unknown token decimals"
                } else if position.wallet_pubkey.parse::<Pubkey>().is_err() {
                    "invalid recorded wallet"
                } else {
                    "no routable exit trader for wallet's route"
                };
                error!(
                    "Operationally unexitable position: mint {} wallet {} - {}",
                    position.mint, position.wallet_pubkey, reason
                );
            }
        }
        if unexitable > 0 {
            error!(
                "New entries HALTED: {} canonical position(s) cannot be exited with current credentials/routes",
                unexitable
            );
            new_entries_halted.store(true, Ordering::SeqCst);
        }
    }

    // === IMPROVED POSITION MONITOR WITH LOCAL FALLBACK ===
    // Features: Trailing stop, no-movement exit, quick profit, retry with local fallback
    if config.auto_sell.enabled && !dry_run {
        let monitor_config = config.clone();
        let monitor_positions = position_manager.clone();
        let monitor_keypair = keypair.clone();
        let monitor_rpc = rpc_client.clone();
        // Transaction-truth wiring (§51): clone the already-initialized reconciler,
        // pending journal, halt flag and strategy engine into the monitor. No new
        // RPC/reconciler is constructed inside the loop.
        let monitor_reconciler = trade_reconciler.clone();
        let monitor_pending = pending_executions.clone();
        let monitor_entry_halt = new_entries_halted.clone();
        let monitor_engine = strategy_engine.clone();
        // B11: EXACT per-position routing handles. The monitor no longer uses the
        // new-buy execution mode as authority for an existing position. It clones
        // the recovery registry (route authority), the recovery multi-wallet
        // (additional-local signers), the primary keypair, the independent exit
        // trader handles, and the shared same-mint sell coordinator.
        let monitor_registry = recovery_registry_arc.clone();
        let monitor_multi_wallet = recovery_multi_wallet.clone();
        let monitor_exit_local = primary_exit_local_trader.clone();
        let monitor_exit_lightning = primary_exit_lightning_trader.clone();
        let monitor_active_sells = active_sell_mints.clone();
        // MPT-001 Agent F1: authoritative market oracle for the primary price-exit
        // path. Every monitor cycle fetches a FRESH on-chain mark and, before any
        // price-based sell, an exact-size same-venue executable quote. Never a
        // DexScreener / stale-current_price fallback for exit authorization.
        let monitor_oracle = market_oracle.clone();
        // D8: the primary auto-sell full-close path lives in this monitor task, so
        // it needs the single runtime command sender to request an
        // UnsubscribeTokenTrades after a durable fully_closed close. Unsubscribe
        // failure is logged only and never alters economic truth.
        let monitor_command_sender = pumpportal_command_sender.clone();

        tokio::spawn(async move {
            info!("=== POSITION MONITOR STARTED ===");
            info!("Features: Trailing Stop (5%), Quick Profit, LOCAL FALLBACK (No-Movement Exit DISABLED)");

            // Track sell attempts for retry logic
            let mut sell_attempts: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                let positions = monitor_positions.get_all_positions().await;
                if positions.is_empty() {
                    continue;
                }

                for position in positions {
                    // MPT-001 Agent F1: fetch a FRESH on-chain mark before any
                    // price logic. A valid SOL mark updates the Position (mark +
                    // peak) via PositionManager::update_price; we then re-read the
                    // updated Position so peak/trailing state reflects this mark.
                    //
                    // On market error we do NOT fall back to the stale persisted
                    // `current_price` to authorize a price sell (INV-MKT-012): skip
                    // this position for the cycle and halt NEW entries because the
                    // position is not operationally priceable. No DexScreener.
                    let mint_pubkey = match Pubkey::from_str(position.mint.trim()) {
                        Ok(pk) => pk,
                        Err(e) => {
                            monitor_entry_halt.store(true, Ordering::SeqCst);
                            error!(
                                "Monitor: position mint '{}' does not parse ({}) - no price exit, new entries HALTED",
                                position.mint, e
                            );
                            continue;
                        }
                    };
                    let fresh_mark = match monitor_oracle.snapshot(&mint_pubkey).await {
                        Ok(snap) => match snap.mark_price_sol_per_token {
                            Some(m) if m.is_finite() && m > 0.0 => m,
                            _ => {
                                // Unsupported quote asset / no usable SOL mark. Not
                                // operationally priceable: halt new entries, keep
                                // monitoring, never trigger a price sell on stale data.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                warn!(
                                    "Monitor: no fresh SOL mark for {} ({}) - no price exit this cycle, new entries HALTED",
                                    position.symbol, position.mint
                                );
                                continue;
                            }
                        },
                        Err(e) => {
                            monitor_entry_halt.store(true, Ordering::SeqCst);
                            warn!(
                                "Monitor: market snapshot failed for {} ({}): {} - no price exit this cycle (no stale fallback), new entries HALTED",
                                position.symbol, position.mint, e
                            );
                            continue;
                        }
                    };
                    monitor_positions
                        .update_price(&position.mint, fresh_mark)
                        .await;
                    // Re-read the updated Position so peak/trailing reflect the mark.
                    let position = match monitor_positions.get_position(&position.mint).await {
                        Some(p) => p,
                        None => continue, // closed concurrently; nothing to do
                    };

                    // `current_price` is now the latest FRESH on-chain mark (Agent F9).
                    let current_price = position.current_price;
                    if current_price <= 0.0 {
                        continue;
                    }

                    // Calculate P&L from entry
                    let pnl_pct = if position.entry_price > 0.0 {
                        ((current_price - position.entry_price) / position.entry_price) * 100.0
                    } else {
                        0.0
                    };

                    // Calculate drop from peak (for trailing stop)
                    let peak_price = if position.peak_price > 0.0 {
                        position.peak_price
                    } else {
                        position.entry_price
                    };
                    let drop_from_peak_pct = if peak_price > 0.0 {
                        ((peak_price - current_price) / peak_price) * 100.0
                    } else {
                        0.0
                    };

                    let hold_time_secs = (chrono::Utc::now() - position.entry_time)
                        .num_seconds()
                        .max(0) as u64;

                    // Get entry-type-specific thresholds
                    let tp_pct = position.entry_type.take_profit_pct();
                    let sl_pct = position.entry_type.stop_loss_pct();
                    let quick_profit_pct = position.entry_type.quick_profit_pct();
                    let max_hold = position.entry_type.max_hold_secs();

                    // Trailing stop: 5% drop from peak (only if we're in profit)
                    let trailing_stop_pct = 5.0;
                    // No-movement exit: DISABLED (was causing exits before pumps)
                    let no_movement_secs = 999999u64;
                    let no_movement_threshold = 0.0;

                    let mut should_sell = false;
                    let mut sell_pct = "100%";
                    let mut reason = String::new();
                    // MPT-001 Agent F2: capture WHICH rule fired so the exact same
                    // condition can be re-confirmed against the executable-quote
                    // price before any sell is submitted.
                    let mut exit_category: Option<PriceExitCategory> = None;

                    // 1. Check stop loss FIRST (cut losses quickly)
                    if pnl_pct <= -sl_pct {
                        should_sell = true;
                        reason = format!("STOP LOSS at {:.1}% (limit: -{:.0}%)", pnl_pct, sl_pct);
                        exit_category = Some(PriceExitCategory::StopLoss {
                            entry_price: position.entry_price,
                            sl_pct,
                        });
                    }

                    // 2. Check trailing stop (only if in profit and dropped from peak)
                    if !should_sell && pnl_pct > 0.0 && drop_from_peak_pct >= trailing_stop_pct {
                        should_sell = true;
                        reason = format!(
                            "TRAILING STOP: dropped {:.1}% from peak (P&L: +{:.1}%)",
                            drop_from_peak_pct, pnl_pct
                        );
                        exit_category = Some(PriceExitCategory::TrailingStop {
                            entry_price: position.entry_price,
                            peak_price,
                            trailing_pct: trailing_stop_pct,
                        });
                    }

                    // 3. Check take profit
                    if !should_sell && pnl_pct >= tp_pct {
                        should_sell = true;
                        reason = format!("TAKE PROFIT at {:.1}% (target: {:.0}%)", pnl_pct, tp_pct);
                        exit_category = Some(PriceExitCategory::TakeProfit {
                            entry_price: position.entry_price,
                            tp_pct,
                        });
                    }

                    // 4. Check quick profit (partial exit)
                    if !should_sell
                        && !position.quick_profit_taken
                        && pnl_pct >= quick_profit_pct
                        && pnl_pct < tp_pct
                    {
                        should_sell = true;
                        sell_pct = "50%";
                        reason = format!("QUICK PROFIT at {:.1}% - selling 50%", pnl_pct);
                        exit_category = Some(PriceExitCategory::QuickProfit {
                            entry_price: position.entry_price,
                            qp_pct: quick_profit_pct,
                            tp_pct,
                        });
                    }

                    // 5. Check no-movement exit (60s with <2% move either way)
                    if !should_sell
                        && hold_time_secs >= no_movement_secs
                        && pnl_pct.abs() < no_movement_threshold
                    {
                        should_sell = true;
                        reason = format!(
                            "NO MOVEMENT: {:.1}% after {}s - exiting stale position",
                            pnl_pct, hold_time_secs
                        );
                        exit_category = Some(PriceExitCategory::TimeBased);
                    }

                    // 6. Check max hold time last (safety net)
                    if !should_sell {
                        if let Some(max_secs) = max_hold {
                            if hold_time_secs >= max_secs {
                                should_sell = true;
                                reason = format!(
                                    "MAX HOLD TIME ({} secs) P&L: {:.1}%",
                                    max_secs, pnl_pct
                                );
                                exit_category = Some(PriceExitCategory::TimeBased);
                            }
                        }
                    }

                    // Execute sell if triggered
                    if should_sell {
                        // MPT-001 Agent F9: `current_price` is the latest FRESH on-chain
                        // MARK. It only identifies a CANDIDATE here; it is never the
                        // execution price and never authorizes a sell on its own — the
                        // exact-size executable quote below must re-confirm (F2).
                        warn!(
                            "AUTO-SELL CANDIDATE: {} ({}) - {} (fresh mark {:.12} SOL/token)",
                            position.symbol, position.mint, reason, current_price
                        );

                        {
                            let slippage = monitor_config.trading.slippage_bps / 100;
                            let priority_fee =
                                monitor_config.trading.priority_fee_lamports as f64 / 1e9;

                            // B9 step (1): pending Buy AND Sell block an automatic exit. If a
                            // confirmed buy mutated in-memory state but its durable save failed,
                            // the pending Buy remains alongside the Position; selling now could
                            // close it and let restart recovery re-open the stale pending buy.
                            let pending_buy = monitor_pending
                                .get_for_mint(&position.mint, ReconciliationSide::Buy)
                                .await;
                            let pending_sell = monitor_pending
                                .get_for_mint(&position.mint, ReconciliationSide::Sell)
                                .await;
                            if let Some(sig) = pending_blocks_automatic_sell(
                                pending_buy.as_ref(),
                                pending_sell.as_ref(),
                            ) {
                                error!(
                                    "Automatic sell blocked for {}: pending Buy/Sell in flight (sig {}). Not submitting (001C reconciliation required).",
                                    position.mint, sig
                                );
                                continue;
                            }

                            // §54 LEGACY GUARD: a position without confirmed token decimals is
                            // not canonical. Do not reconciled-sell it. Fail closed until 001C.
                            if position.token_decimals.is_none() {
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "Legacy/unmigrated position has unknown token decimals; 001C reconciliation required (mint {})",
                                    position.mint
                                );
                                continue;
                            }

                            // §55 EXACT WALLET GUARD: the position's recorded wallet must parse.
                            let position_wallet = match Pubkey::from_str(position.wallet_pubkey.trim()) {
                                Ok(pk) if !position.wallet_pubkey.trim().is_empty() => pk,
                                _ => {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "Position wallet_pubkey empty/invalid for {} ({:?}) - no sell, new entries HALTED",
                                        position.mint, position.wallet_pubkey
                                    );
                                    continue;
                                }
                            };

                            // B9 step (2)/(3): reserve the mint for a primary sell. If another
                            // producer (kill-switch) already holds it, skip this cycle.
                            if !try_reserve_sell_mint(&monitor_active_sells, &position.mint) {
                                info!(
                                    "Auto-sell skipped for {}: another primary sell reservation is active for this mint",
                                    position.mint
                                );
                                continue;
                            }

                            // B9 step (4): re-check pending Buy/Sell AFTER reserving. If one
                            // appeared in the window, release + skip (no submission).
                            let recheck_buy = monitor_pending
                                .get_for_mint(&position.mint, ReconciliationSide::Buy)
                                .await;
                            let recheck_sell = monitor_pending
                                .get_for_mint(&position.mint, ReconciliationSide::Sell)
                                .await;
                            if let Some(sig) = pending_blocks_automatic_sell(
                                recheck_buy.as_ref(),
                                recheck_sell.as_ref(),
                            ) {
                                release_sell_mint(&monitor_active_sells, &position.mint);
                                error!(
                                    "Automatic sell aborted for {} after reservation: pending Buy/Sell appeared (sig {}) - reservation released",
                                    position.mint, sig
                                );
                                continue;
                            }

                            // MPT-001 Agent F3/F6: compute the EXACT raw amount for
                            // the intended layer (50%/25%/100% via integer division)
                            // and fetch the FINAL same-venue executable quote INSIDE
                            // the reservation, immediately before send. This quote is
                            // both the trigger re-confirmation (F2) and the execution
                            // reference (F7) — never reused across cycles or layers.
                            // A quote failure means nothing was submitted: release the
                            // reservation, keep the position, halt new entries.
                            let token_decimals = match position.token_decimals {
                                Some(d) => d,
                                None => {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    release_sell_mint(&monitor_active_sells, &position.mint);
                                    error!(
                                        "Auto-sell: position {} lost token decimals before quote - no sell, reservation released, new entries HALTED",
                                        position.mint
                                    );
                                    continue;
                                }
                            };
                            let intended_raw =
                                match layer_raw_amount(position.token_amount, sell_pct) {
                                    Some(r) => r,
                                    None => {
                                        release_sell_mint(&monitor_active_sells, &position.mint);
                                        warn!(
                                            "Auto-sell: exact raw amount for layer {} of {} raw {} is zero/unknown - no sell, reservation released",
                                            sell_pct, position.mint, position.token_amount
                                        );
                                        continue;
                                    }
                                };
                            let sell_quote = match monitor_oracle
                                .quote_sell_raw(&mint_pubkey, intended_raw)
                                .await
                            {
                                Ok(q) => q,
                                Err(e) => {
                                    // Market unsupported/unavailable at exact size:
                                    // nothing submitted. Release + halt new entries.
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    release_sell_mint(&monitor_active_sells, &position.mint);
                                    warn!(
                                        "Auto-sell: no executable sell quote for {} ({} raw): {} - no sell, reservation released, new entries HALTED",
                                        position.symbol, intended_raw, e
                                    );
                                    continue;
                                }
                            };
                            // F2: require the SAME SOL venue as the mark.
                            if !sell_quote.is_sol_pair() {
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                release_sell_mint(&monitor_active_sells, &position.mint);
                                warn!(
                                    "Auto-sell: executable quote for {} is not a SOL pair - no sell, reservation released, new entries HALTED",
                                    position.symbol
                                );
                                continue;
                            }
                            // F2: the price condition must STILL hold against the
                            // exact-size executable quote price, not just the mark. If
                            // the position can no longer be liquidated at the trigger
                            // condition, do not sell — release and keep monitoring.
                            let exec_price = match sell_quote.expected_price_sol_per_token {
                                Some(p) if p.is_finite() && p > 0.0 => p,
                                _ => {
                                    release_sell_mint(&monitor_active_sells, &position.mint);
                                    warn!(
                                        "Auto-sell: executable quote for {} has no usable price - no sell, reservation released",
                                        position.symbol
                                    );
                                    continue;
                                }
                            };
                            let confirmed = exit_category
                                .map(|c| c.confirms_at(exec_price))
                                .unwrap_or(false);
                            if !confirmed {
                                release_sell_mint(&monitor_active_sells, &position.mint);
                                info!(
                                    "Auto-sell NOT confirmed for {}: mark triggered '{}' but executable quote price {:.12} SOL/token no longer meets the condition - no sell, reservation released",
                                    position.symbol, reason, exec_price
                                );
                                continue;
                            }
                            // F5: pin the execution venue to the quoted venue (no Auto).
                            let sell_pool = pumpportal_pool_for_venue(sell_quote.venue);
                            // F4: submit THIS exact decimal token amount (derived from
                            // the exact raw quoted size) instead of "50%"/"100%", so
                            // quote input, route and submitted amount are the same size.
                            let submit_amount =
                                raw_token_amount_to_decimal_string(intended_raw, token_decimals);
                            info!(
                                "Auto-sell CONFIRMED for {}: venue={:?} pool={:?} raw={} amount={} exec_price={:.12} quote_slot={}",
                                position.symbol,
                                sell_quote.venue,
                                sell_pool,
                                intended_raw,
                                submit_amount,
                                exec_price,
                                sell_quote.slot
                            );

                            // Retry counter behavior preserved.
                            let attempts = sell_attempts.entry(position.mint.clone()).or_insert(0);
                            *attempts += 1;

                            if *attempts > 5 {
                                // Retry exhaustion: never close/remove wallet-owned risk. Leave
                                // OPEN/TRACKED, reset counter, release reservation, let a later
                                // cycle try again.
                                error!(
                                    "AUTO-SELL UNRESOLVED for {} after 5 attempts - position remains OPEN/TRACKED",
                                    position.symbol
                                );
                                sell_attempts.remove(&position.mint);
                                release_sell_mint(&monitor_active_sells, &position.mint);
                                continue;
                            }

                            // B9 step (5)/(6) + B11: resolve the EXACT route/signer for THIS
                            // position's recorded wallet via the recovery registry (not the
                            // new-buy mode), then submit through the matching independent exit
                            // trader ONLY. No Lightning->Local fallback. Unknown route / missing
                            // signer / missing trader => no sell + release + halt new entries.
                            let sell_start = std::time::Instant::now();
                            let sell_result: Result<String, crate::error::Error> =
                                match monitor_registry.route_for(&position_wallet) {
                                    Some(crate::wallet::ExecutionRoute::Local) => {
                                        // Exact local signer: primary keypair if its pubkey matches,
                                        // else the recovery multi-wallet's wallet for that address.
                                        let local_trader = match monitor_exit_local {
                                            Some(ref t) => t,
                                            None => {
                                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                                release_sell_mint(&monitor_active_sells, &position.mint);
                                                error!(
                                                    "Local exit unavailable (no Local trader) for {} wallet {} - no sell, reservation released, new entries HALTED",
                                                    position.mint, position_wallet
                                                );
                                                continue;
                                            }
                                        };
                                        if monitor_keypair.pubkey() == position_wallet {
                                            info!("Attempting LOCAL sell for {} via primary keypair (attempt {})", position.mint, attempts);
                                            local_trader
                                                .sell_local_with_pool(
                                                    &position.mint,
                                                    &submit_amount,
                                                    slippage,
                                                    priority_fee,
                                                    &monitor_keypair,
                                                    &monitor_rpc,
                                                    sell_pool,
                                                )
                                                .await
                                        } else if let Some(tw) = monitor_multi_wallet
                                            .as_ref()
                                            .and_then(|mw| mw.find_by_address(&position.wallet_pubkey))
                                        {
                                            info!("Attempting LOCAL sell for {} via recovery wallet {} (attempt {})", position.mint, position.wallet_pubkey, attempts);
                                            local_trader
                                                .sell_local_with_pool(
                                                    &position.mint,
                                                    &submit_amount,
                                                    slippage,
                                                    priority_fee,
                                                    &tw.keypair,
                                                    &monitor_rpc,
                                                    sell_pool,
                                                )
                                                .await
                                        } else {
                                            monitor_entry_halt.store(true, Ordering::SeqCst);
                                            release_sell_mint(&monitor_active_sells, &position.mint);
                                            error!(
                                                "No exact Local signer for {} wallet {} - no sell, reservation released, new entries HALTED",
                                                position.mint, position_wallet
                                            );
                                            continue;
                                        }
                                    }
                                    Some(crate::wallet::ExecutionRoute::Lightning) => {
                                        let lightning_trader = match monitor_exit_lightning {
                                            Some(ref t) => t,
                                            None => {
                                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                                release_sell_mint(&monitor_active_sells, &position.mint);
                                                error!(
                                                    "Lightning exit unavailable (no Lightning trader/API key) for {} wallet {} - no sell, reservation released, new entries HALTED",
                                                    position.mint, position_wallet
                                                );
                                                continue;
                                            }
                                        };
                                        info!("Attempting Lightning sell for {} (attempt {})", position.mint, attempts);
                                        lightning_trader
                                            .sell_with_pool(
                                                &position.mint,
                                                &submit_amount,
                                                slippage,
                                                priority_fee,
                                                sell_pool,
                                            )
                                            .await
                                    }
                                    None => {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        release_sell_mint(&monitor_active_sells, &position.mint);
                                        error!(
                                            "No exact route for {} wallet {} - no sell, reservation released, new entries HALTED",
                                            position.mint, position_wallet
                                        );
                                        continue;
                                    }
                                };

                            let signature = match sell_result {
                                Ok(sig) => sig,
                                Err(e) => {
                                    // §57 SUBMISSION FAILURE: no signature, no pending. Record a
                                    // symmetric execution failure sample; keep position open.
                                    let provider_latency_ms = sell_start.elapsed().as_millis() as u64;
                                    error!(
                                        "AUTO-SELL SUBMISSION FAILED for {} (attempt {}): {} ({}ms)",
                                        position.symbol, attempts, e, provider_latency_ms
                                    );
                                    if let Some(ref engine) = monitor_engine {
                                        engine.write().await.record_tx_failure(
                                            &position.mint,
                                            false,
                                            position.total_cost_sol,
                                            provider_latency_ms,
                                            &e.to_string(),
                                        ).await;
                                    }
                                    // B10: provider error / no signature => release reservation.
                                    release_sell_mint(&monitor_active_sells, &position.mint);
                                    continue;
                                }
                            };

                            // §58 PERSIST SIGNATURE: submitted != executed.
                            info!("AUTO-SELL SUBMITTED: {} (sig {})", position.symbol, signature);
                            // MPT-001 Agent F4: intent (QuickProfit/Full) still comes
                            // from the layer, but the pending context stores the EXACT
                            // submitted decimal amount string that was actually sent.
                            let intent = if sell_pct == "50%" {
                                PendingSellIntent::QuickProfit
                            } else {
                                PendingSellIntent::Full
                            };
                            let pending_sell = PendingExecution::sell(
                                signature.clone(),
                                position.mint.clone(),
                                position.wallet_pubkey.clone(),
                                PendingSellContext {
                                    requested_amount: submit_amount.clone(),
                                    intent,
                                    reason: reason.clone(),
                                },
                            );
                            // AUDIT-002 A5: retain the exact pending record + whether the
                            // first journal write persisted, so ambiguous/confirmed-unapplied
                            // outcomes can retry durable persistence before relying on restart
                            // recovery.
                            let pending_sell_persisted = match monitor_pending.upsert(pending_sell.clone()).await {
                                Ok(()) => true,
                                Err(e) => {
                                    // Signature already exists on chain-side; persistence failed.
                                    // Halt new entries but STILL reconcile the submitted signature.
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "Failed to persist pending sell (sig {}): {} - new entries HALTED, still reconciling",
                                        signature, e
                                    );
                                    false
                                }
                            };

                            // §59 RECONCILE: no fixed sleep, no estimated-proceeds fallback.
                            let outcome = monitor_reconciler
                                .reconcile(
                                    &signature,
                                    &position.wallet_pubkey,
                                    &position.mint,
                                    ReconciliationSide::Sell,
                                )
                                .await;

                            match outcome {
                                Ok(ReconciliationOutcome::ConfirmedFailure { error, observed_after_ms, .. }) => {
                                    // §60: remove pending, record failure, keep position.
                                    match monitor_pending.remove(&signature).await {
                                        Ok(_) => {}
                                        Err(e) => {
                                            monitor_entry_halt.store(true, Ordering::SeqCst);
                                            error!(
                                                "Failed to remove pending sell after ConfirmedFailure (sig {}): {} - new entries HALTED",
                                                signature, e
                                            );
                                        }
                                    }
                                    let total_sell_latency_ms = sell_start.elapsed().as_millis() as u64;
                                    error!(
                                        "AUTO-SELL CONFIRMED FAILED for {} (sig {}): {} ({}ms observed) - position remains OPEN/TRACKED",
                                        position.symbol, signature, error, observed_after_ms
                                    );
                                    if let Some(ref engine) = monitor_engine {
                                        engine.write().await.record_tx_failure(
                                            &position.mint,
                                            false,
                                            position.total_cost_sol,
                                            total_sell_latency_ms,
                                            &error,
                                        ).await;
                                    }
                                    // Do not mark quick profit; do not record_exit/partial. Later
                                    // monitor cycle may retry.
                                    // B10: ConfirmedFailure => pending removed, release reservation.
                                    release_sell_mint(&monitor_active_sells, &position.mint);
                                    continue;
                                }
                                Ok(ReconciliationOutcome::Unresolved { reason: unresolved_reason, .. }) => {
                                    // §61: KEEP pending, keep position + flags, halt new entries.
                                    // Do NOT clear sell-attempt state (pending guard prevents a
                                    // second submission on the next cycle).
                                    // B10: Unresolved => KEEP the reservation (do NOT release);
                                    // the same-mint sell stays owned until the signature resolves.
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    // AUDIT-002 A5: if the initial journal write failed, retry
                                    // durable persistence before leaving this arm so the in-flight
                                    // signature survives a crash. Reservation kept regardless.
                                    if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                        error!(
                                            "AUTO-SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                            position.mint, signature, position.wallet_pubkey, unresolved_reason
                                        );
                                    } else {
                                        error!(
                                            "CRITICAL: AUTO-SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                            position.mint, signature, position.wallet_pubkey, unresolved_reason, signature
                                        );
                                    }
                                    continue;
                                }
                                Err(e) => {
                                    // §61: structural observer failure is not tx-failure proof.
                                    // B10: structural reconciler Err => KEEP the reservation.
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    // AUDIT-002 A5: same durability retry rule as Unresolved.
                                    if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                        error!(
                                            "CRITICAL: sell reconciliation error for {} (sig {}): {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                            position.symbol, signature, e
                                        );
                                    } else {
                                        error!(
                                            "CRITICAL: sell reconciliation error for {} (sig {}): {} - AND pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                            position.symbol, signature, e, signature
                                        );
                                    }
                                    continue;
                                }
                                Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                    // §62 identity validation at the live boundary.
                                    if fill.side != ReconciliationSide::Sell
                                        || fill.wallet != position.wallet_pubkey
                                        || fill.mint != position.mint
                                    {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        // AUDIT-002 A5: confirmed-but-unapplied. Retry durability;
                                        // keep reservation regardless.
                                        if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                            error!(
                                                "CRITICAL: reconciled sell fill identity mismatch for sig {} (wallet/mint/side) - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                signature
                                            );
                                        } else {
                                            error!(
                                                "CRITICAL: reconciled sell fill identity mismatch for sig {} (wallet/mint/side) - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                signature, signature
                                            );
                                        }
                                        continue;
                                    }

                                    // §62/§63 economics via pure helper (validates decimals match,
                                    // nonzero raw, finite delta/price, and no oversell).
                                    let (actual_sold_raw, actual_received_sol, actual_exit_price) =
                                        match primary_sell_fill_values(&fill, &position) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                                // AUDIT-002 A5: confirmed-but-unapplied. Retry
                                                // durability; keep reservation regardless.
                                                if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                                    error!(
                                                        "Reconciled sell fill validation failed for {} (sig {}): {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                        position.mint, signature, e
                                                    );
                                                } else {
                                                    error!(
                                                        "CRITICAL: reconciled sell fill validation failed for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                        position.mint, signature, e, signature
                                                    );
                                                }
                                                continue;
                                            }
                                        };

                                    // §64 pre-mutation capture, then idempotent reconciled close.
                                    let pre_close_cost = position.total_cost_sol;
                                    let pre_close_tokens = position.token_amount;

                                    let close_result = match monitor_positions
                                        .close_position_reconciled(
                                            &position.mint,
                                            &signature,
                                            actual_sold_raw,
                                            actual_received_sol,
                                        )
                                        .await
                                    {
                                        Ok(r) => r,
                                        Err(e) => {
                                            // §64: PositionAccounting error -> keep pending,
                                            // halt, no strategy P&L.
                                            monitor_entry_halt.store(true, Ordering::SeqCst);
                                            // AUDIT-002 A5: confirmed-but-unapplied. Retry
                                            // durability; keep reservation regardless.
                                            if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                                error!(
                                                    "Reconciled close failed for {} (sig {}): {} - pending kept (durable), reservation kept, new entries HALTED",
                                                    position.mint, signature, e
                                                );
                                            } else {
                                                error!(
                                                    "CRITICAL: reconciled close failed for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                    position.mint, signature, e, signature
                                                );
                                            }
                                            continue;
                                        }
                                    };

                                    sell_attempts.remove(&position.mint);

                                    // §65 partial vs full comes from the ACTUAL fill.
                                    let fully_closed = close_result.fully_closed;
                                    let already_applied = close_result.already_applied;
                                    let hold_secs =
                                        (chrono::Utc::now() - position.entry_time).num_seconds();

                                    // §66 quick-profit flag: only if intent QuickProfit, first
                                    // application, and the position still exists (partial).
                                    if intent == PendingSellIntent::QuickProfit
                                        && !already_applied
                                        && !fully_closed
                                    {
                                        let _ = monitor_positions
                                            .mark_quick_profit_taken(&position.mint)
                                            .await;
                                    }

                                    // §67/§68 strategy governor + §69 execution feedback, only on
                                    // first application.
                                    if !already_applied {
                                        if let Some(ref engine) = monitor_engine {
                                            let total_sell_latency_ms =
                                                sell_start.elapsed().as_millis() as u64;
                                            if fully_closed {
                                                // §68 full exit (idempotent portfolio behavior).
                                                engine.write().await.record_exit(
                                                    &position.mint,
                                                    close_result.pnl_sol,
                                                ).await;
                                            } else {
                                                // §67 partial exit updates exposure + realized P&L.
                                                let ok = engine.write().await.record_partial_exit(
                                                    &position.mint,
                                                    close_result.remaining_cost_sol,
                                                    close_result.remaining_amount,
                                                    close_result.pnl_sol,
                                                ).await;
                                                if !ok {
                                                    warn!(
                                                        "Strategy governor lacks position {} for partial exit; PositionManager result unchanged",
                                                        position.mint
                                                    );
                                                }
                                            }

                                            // §69 execution feedback: requested proxy = pre-close
                                            // cost basis scaled by actual sold ratio.
                                            let requested_proxy = if pre_close_tokens > 0 {
                                                pre_close_cost
                                                    * (actual_sold_raw as f64 / pre_close_tokens as f64)
                                            } else {
                                                pre_close_cost
                                            };
                                            // MPT-001 Agent F7: this exit had a real
                                            // same-venue pre-send executable quote, so
                                            // record quote-to-fill drift against the
                                            // EXECUTABLE QUOTE expected price (never the
                                            // mark / stale current_price / DexScreener).
                                            // Fill remains P&L truth. If the quote had no
                                            // usable expected price, fall back to the
                                            // existing unquoted feedback (no fabrication).
                                            match sell_quote.expected_price_sol_per_token {
                                                Some(expected_price)
                                                    if expected_price.is_finite()
                                                        && expected_price > 0.0 =>
                                                {
                                                    engine.write().await.record_reconciled_quoted_execution(
                                                        &position.mint,
                                                        false,
                                                        requested_proxy,
                                                        actual_received_sol,
                                                        expected_price,
                                                        actual_exit_price,
                                                        total_sell_latency_ms,
                                                        &signature,
                                                    ).await;
                                                }
                                                _ => {
                                                    engine.write().await.record_reconciled_execution(
                                                        &position.mint,
                                                        false,
                                                        requested_proxy,
                                                        actual_received_sol,
                                                        actual_exit_price,
                                                        total_sell_latency_ms,
                                                        &signature,
                                                    ).await;
                                                }
                                            }
                                        }
                                    }

                                    // §71 confirmed logging (no estimated language).
                                    if fully_closed {
                                        info!("=== AUTO-SELL CONFIRMED (Full) ===");
                                    } else {
                                        info!("=== AUTO-SELL CONFIRMED (Partial) ===");
                                    }
                                    info!(
                                        "  {} (sig {}) | sold_raw={} decimals={} net_sol_delta={:+.9} exit_price={:.12} SOL/token | realized P&L: {:+.9} SOL | recon_wait={}ms | hold={}s{}",
                                        position.symbol,
                                        signature,
                                        actual_sold_raw,
                                        fill.token_decimals,
                                        actual_received_sol,
                                        actual_exit_price,
                                        close_result.pnl_sol,
                                        fill.reconciliation_wait_ms,
                                        hold_secs,
                                        if already_applied { " (already applied; idempotent)" } else { "" }
                                    );

                                    // §70 remove pending LAST.
                                    if let Err(e) = monitor_pending.remove(&signature).await {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        error!(
                                            "Failed to remove pending sell after confirmed fill (sig {}): {} - new entries HALTED; position state already applied",
                                            signature, e
                                        );
                                    }
                                    // B10: ConfirmedFill applied + pending removed LAST => release
                                    // the same-mint reservation.
                                    release_sell_mint(&monitor_active_sells, &position.mint);

                                    // D8 / INV-EVT-013: after a durable FULL close,
                                    // request UnsubscribeTokenTrades for this mint on the
                                    // single runtime command sender. Partial close keeps the
                                    // subscription. Failure is logged only and never alters
                                    // economic position truth.
                                    if full_close_requests_unsubscribe(fully_closed) {
                                        if !send_subscription_command(
                                            &monitor_command_sender,
                                            SubscriptionCommand::UnsubscribeTokenTrades(vec![
                                                position.mint.clone(),
                                            ]),
                                        )
                                        .await
                                        {
                                            warn!(
                                                "Auto-sell full close: could not request token-trade unsubscribe for {} (no effect on position truth)",
                                                position.mint
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // === D2 / D3 / D4: open the single PumpPortal socket AFTER all canonical
    // local state is loaded (PositionManager, pending/legacy recovery, strategy
    // restore, kill-switch init, monitor spawned). No market/trade event intake
    // happens before this point. ===
    if let Some(ref client) = pumpportal_client {
        // D3: open-position mints get token-trade subscriptions; tracked wallets
        // become account-trade subscriptions ONLY when wallet tracking is enabled.
        let open_position_mints: Vec<String> = position_manager
            .get_all_positions()
            .await
            .into_iter()
            .map(|p| p.mint)
            .collect();
        let configured_tracked_wallets = config.wallet_tracking.wallets.clone();
        let mut plan = build_initial_subscription_plan(
            &open_position_mints,
            &configured_tracked_wallets,
            config.wallet_tracking.enabled,
        );

        // D4: Data API credential behavior. Token/account trade streams are
        // authenticated. If no key is configured we must NOT request any trade
        // subscription; we drop them from the plan and (for a live run) halt NEW
        // entries. Existing-position price monitoring/exits remain fully active.
        // force_local_api does NOT bypass this Data API rule.
        let api_key = config.pumpportal.api_key.clone();
        let key_missing = api_key.trim().is_empty();
        if key_missing && (!plan.token_trades.is_empty() || !plan.account_trades.is_empty()) {
            warn!(
                "PumpPortal Data API key not configured: dropping {} token-trade and {} account-trade subscription(s). \
                 Price-based exit monitoring remains available, but provider trade kill-switch and account tracking are unavailable.",
                plan.token_trades.len(),
                plan.account_trades.len()
            );
            plan.token_trades.clear();
            plan.account_trades.clear();
            if !dry_run {
                new_entries_halted.store(true, Ordering::SeqCst);
            }
        }

        // D4 (additional): any NEW live position would need its own authenticated
        // token-trade subscription, so a live run with the feed enabled and no key
        // is entry-disabled even when there are no initial positions. This keeps
        // the runtime exit-capable but entry-disabled.
        if missing_data_key_halts_new_entries(dry_run, config.pumpportal.enabled, &api_key) {
            new_entries_halted.store(true, Ordering::SeqCst);
            info!(
                "Live run without PumpPortal Data API key: NEW entries halted (exit-capable, entry-disabled). \
                 Configure pumpportal.api_key to enable position-scoped trade monitoring and new entries."
            );
        }

        info!(
            "Opening PumpPortal stream: new_tokens={}, migrations={}, token_trades={}, account_trades={} (base only; key never logged)",
            plan.new_tokens,
            plan.migrations,
            plan.token_trades.len(),
            plan.account_trades.len()
        );

        // D2/D5: one socket per start runtime. Dry-run may use the free
        // new-token/migration stream without a key. `start(plan)` validates the
        // plan up front and returns immediately (it spawns its own connect loop).
        if let Err(e) = client.start(plan).await {
            error!("PumpPortal connection error: {}", e);
        }
    }

    info!("Bot started. Listening for new tokens...");

    // Main event loop
    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    PumpPortalEvent::NewToken(token) => {
                        info!(
                            "New token detected: {} ({}) - Mint: {} | v_sol={} market_cap={}",
                            token.name, token.symbol, token.mint,
                            token.v_sol_in_bonding_curve,
                            token.market_cap_sol
                        );

                        // P1-METADATA-DRAIN-TRUTH-001 §9: candidate-specific metadata
                        // gate. A metadata-less create is now a supported provider
                        // variant (empty name/symbol/uri), NOT a DecodeError — but the
                        // live strategy/filter inputs (filter_name_symbol, SignalContext)
                        // require provider name/symbol/uri, so an incomplete candidate
                        // must never flow into filter/scoring/sizing/quote/submission.
                        // Skip THIS candidate only (fail-closed per-candidate) in BOTH
                        // dry-run and live. This does NOT touch data_stream_ready or
                        // new_entries_halted and does not halt the bot or affect exits.
                        if !token.has_complete_metadata() {
                            warn!("New token metadata unavailable; skipping candidate");
                            continue;
                        }

                        // Fail-closed new-entry admission gate (D12). Independent of
                        // daily loss, strategy pause, and filters. A NEW live buy is
                        // admitted only when entries are not halted AND — when the
                        // PumpPortal feed is enabled — the data stream is ready
                        // (Connected, subscriptions replayed). Provider disconnect /
                        // error / missing Data API key therefore blocks new entries.
                        // Exits are NEVER gated on this. Dry-run is exempt (free feed).
                        if !dry_run
                            && !new_entry_admitted(
                                new_entries_halted.load(Ordering::SeqCst),
                                data_stream_ready.load(Ordering::SeqCst),
                                config.pumpportal.enabled,
                            )
                        {
                            warn!(
                                "New entries blocked: halted={} data_stream_ready={} (unresolved state, or provider feed not ready/unauthenticated)",
                                new_entries_halted.load(Ordering::SeqCst),
                                data_stream_ready.load(Ordering::SeqCst)
                            );
                            continue;
                        }

                        // Apply filters
                        if config.filters.enabled {
                            use crate::filter::token_filter::FilterResult;

                            // C1 / BLOCKER B: run the name/symbol regex filter directly
                            // on provider name+symbol. Do NOT fabricate a
                            // TokenCreatedEvent with slot=0 and Pubkey::default()
                            // identities: the stream parser already validated provider
                            // identity before emitting NewToken, and the name/symbol
                            // filter needs neither slot nor Pubkey.
                            match token_filter.filter_name_symbol(&token.name, &token.symbol) {
                                FilterResult::Pass => {
                                    info!("Token {} passed name/symbol filters", token.symbol);
                                }
                                FilterResult::Filtered(reason) => {
                                    info!("Token {} filtered out: {}", token.symbol, reason);
                                    continue;
                                }
                            }

                            // Check liquidity (from market cap estimate)
                            let liquidity_sol = token.market_cap_sol;
                            if liquidity_sol < config.filters.min_liquidity_sol {
                                info!(
                                    "Token {} filtered: liquidity {:.4} SOL < min {:.4} SOL",
                                    token.symbol, liquidity_sol, config.filters.min_liquidity_sol
                                );
                                continue;
                            }

                            // Check market cap minimum (for established tokens)
                            if config.filters.min_market_cap_sol > 0.0 && token.market_cap_sol < config.filters.min_market_cap_sol {
                                info!(
                                    "Token {} filtered: market cap {:.2} SOL < min {:.2} SOL (too new)",
                                    token.symbol, token.market_cap_sol, config.filters.min_market_cap_sol
                                );
                                continue;
                            }

                            // Check bonding curve progress (for established tokens)
                            // C2 / BLOCKER A: v_sol_in_bonding_curve is a provider
                            // observational SOL value (f64, SOL NOT lamports). Use the
                            // shared provider heuristic directly on that SOL value — no
                            // /1e9 conversion. This heuristic stays observational; the
                            // MPT oracle remains executable/market truth.
                            let bonding_curve_pct =
                                SignalContext::calculate_bonding_curve_pct(token.v_sol_in_bonding_curve);

                            if config.filters.min_bonding_curve_pct > 0.0 && bonding_curve_pct < config.filters.min_bonding_curve_pct {
                                info!(
                                    "Token {} filtered: bonding curve {:.1}% < min {:.1}% (too new)",
                                    token.symbol, bonding_curve_pct, config.filters.min_bonding_curve_pct
                                );
                                continue;
                            }

                            if config.filters.max_bonding_curve_pct > 0.0 && bonding_curve_pct > config.filters.max_bonding_curve_pct {
                                info!(
                                    "Token {} filtered: bonding curve {:.1}% > max {:.1}% (near graduation)",
                                    token.symbol, bonding_curve_pct, config.filters.max_bonding_curve_pct
                                );
                                continue;
                            }

                            info!(
                                "Token {} passed all filters: mcap={:.2} SOL, bonding={:.1}%",
                                token.symbol, token.market_cap_sol, bonding_curve_pct
                            );
                        }

                        // Check daily loss limit
                        if position_manager.is_daily_loss_limit_reached().await {
                            warn!("Daily loss limit reached - skipping buy");
                            continue;
                        }

                        // Check strategy engine constraints (if enabled)
                        if let Some(ref engine) = strategy_engine {
                            let engine_guard = engine.read().await;

                            // Check if trading should be paused
                            if engine_guard.should_pause_trading().await {
                                let chain_state = engine_guard.get_chain_state().await;
                                warn!(
                                    "Strategy engine paused trading: congestion={:?}",
                                    chain_state.congestion_level
                                );
                                continue;
                            }

                            // Check portfolio limits
                            let portfolio_state = engine_guard.get_portfolio_state().await;
                            if !portfolio_state.can_open_new {
                                warn!(
                                    "Portfolio limit reached: {} positions, {} SOL exposure - {:?}",
                                    portfolio_state.open_position_count,
                                    portfolio_state.total_exposure_sol,
                                    portfolio_state.reason_if_blocked
                                );
                                continue;
                            }
                        }

                        // Apply adaptive filter scoring if enabled
                        // Track both position multiplier AND recommendation for context-aware exits
                        let (position_multiplier, entry_recommendation, entry_confidence) = if let Some(ref filter) = adaptive_filter {
                            // Create signal context from token event
                            let signal_context = SignalContext::from_new_token(
                                token.mint.clone(),
                                token.name.clone(),
                                token.symbol.clone(),
                                token.uri.clone(),
                                token.trader_public_key.clone(),
                                token.bonding_curve_key.clone(),
                                // C3 / BLOCKER A: pass the provider observational f64
                                // values DIRECTLY (from_new_token now takes f64). No
                                // flooring/truncation. These are NOT canonical reserves.
                                token.initial_buy,
                                token.v_tokens_in_bonding_curve,
                                token.v_sol_in_bonding_curve,
                                token.market_cap_sol,
                            );

                            // Score the token
                            let result = filter.score_fast(&signal_context).await;

                            info!(
                                "Adaptive filter: {} score={:.2} risk={:.2} confidence={:.2} recommendation={:?}",
                                token.symbol, result.score, result.risk_score, result.confidence, result.recommendation
                            );

                            // Log individual signals for debugging
                            for signal in &result.signals {
                                tracing::debug!(
                                    signal_type = %signal.signal_type,
                                    value = %signal.value,
                                    confidence = %signal.confidence,
                                    reason = %signal.reason,
                                    "Signal contribution"
                                );
                            }

                            // Check recommendation using new confidence regime model
                            // When information is weak, the system watches — not trades
                            match result.recommendation {
                                Recommendation::Avoid => {
                                    warn!(
                                        "Token {} marked AVOID by adaptive filter: {}",
                                        token.symbol, result.summary
                                    );
                                    continue;
                                }
                                Recommendation::Observe => {
                                    // OBSERVE = watch only, don't trade
                                    // This is the key change: uncertainty means NO trading
                                    info!(
                                        "Token {} marked OBSERVE (insufficient data/confidence): {}",
                                        token.symbol, result.summary
                                    );
                                    continue;
                                }
                                Recommendation::Probe => {
                                    // PROBE = micro-position for learning only
                                    // 5% position size, quick scalp exit
                                    info!(
                                        "Token {} in PROBE mode (learning position): {}",
                                        token.symbol, result.summary
                                    );
                                    // Continue to trading with reduced size
                                }
                                Recommendation::Opportunity => {
                                    // Standard buy opportunity
                                    info!(
                                        "Token {} marked OPPORTUNITY by adaptive filter: {}",
                                        token.symbol, result.summary
                                    );
                                }
                                Recommendation::StrongBuy => {
                                    info!(
                                        "Token {} marked STRONG BUY by adaptive filter: {}",
                                        token.symbol, result.summary
                                    );
                                }
                            }

                            // Pass the REAL calibrated confidence [0,1] separately from the
                            // position-size multiplier. Never reuse the multiplier as confidence.
                            (result.position_size_multiplier, result.recommendation, result.confidence)
                        } else {
                            // Default if adaptive filter disabled: neutral confidence, not a multiplier.
                            (1.0, Recommendation::Opportunity, 0.5)
                        };

                        // Strategy engine evaluation (if enabled)
                        let (strategy_entry, strategy_size) = if let Some(ref engine) = strategy_engine {
                            let mut engine_guard = engine.write().await;

                            // Build token analysis context for strategy engine.
                            // C4 / BLOCKER A: v_sol_in_bonding_curve is provider
                            // observational SOL only (never lamports). Use it directly;
                            // there is no dual SOL/lamport interpretation. This value is
                            // NOT promoted to canonical liquidity: market_data_ready=false
                            // below means the StrategyEngine cannot authorize an Enter on
                            // these provider observational placeholders.
                            let liquidity_sol = token.v_sol_in_bonding_curve;
                            let token_reserves = token.v_tokens_in_bonding_curve;

                            // PLACEHOLDER order flow. A brand-new token event has no real
                            // trade history, so these are NOT measured values. organic_score
                            // must not derive from position_multiplier (that was circular).
                            // market_data_ready=false below ensures these placeholders cannot
                            // authorize an Enter.
                            let order_flow = crate::strategy::regime::OrderFlowAnalysis {
                                organic_score: 0.5, // neutral placeholder, not measured
                                wash_trading_score: 0.0,
                                buy_sell_ratio: 1.0,
                                early_sell_pressure: 0.0,
                                burst_detected: false,
                                burst_intensity: 0.0,
                            };

                            // Create token distribution from available data
                            let distribution = crate::strategy::regime::TokenDistribution {
                                holder_count: 1,
                                top_holder_pct: 100.0,
                                top_10_holders_pct: 100.0,
                                deployer_holdings_pct: 0.0,
                                sniper_holdings_pct: 0.0,
                                gini_coefficient: 1.0,
                            };

                            // Create creator behavior
                            let creator_behavior = crate::strategy::regime::CreatorBehavior {
                                selling_consistently: false,
                                total_sold_pct: 0.0,
                                avg_sell_interval_secs: 0,
                                sell_count: 0,
                            };

                            // Create minimal price action
                            let price_action = crate::strategy::price_action::PriceAction::default();

                            // Evaluate entry using strategy engine
                            let analysis_ctx = crate::strategy::engine::TokenAnalysisContext {
                                mint: token.mint.clone(),
                                creator: token.trader_public_key.clone(),
                                order_flow,
                                distribution,
                                creator_behavior,
                                price_action,
                                sol_reserves: liquidity_sol,
                                token_reserves,
                                confidence_score: entry_confidence,
                                // Placeholder order-flow/distribution/creator inputs above are
                                // NOT measured market data, so the strategy may not Enter on them.
                                market_data_ready: false,
                            };

                            let eval = engine_guard.evaluate_entry(&analysis_ctx).await;

                            // Only an explicit Enter authorizes a buy. Hold and every other
                            // action are abstentions — they must NOT fall through to a buy.
                            match strategy_entry_size(&eval.decision.action) {
                                Some(size_sol) => {
                                    info!(
                                        "Strategy engine: ENTER {} size: {:.4} SOL",
                                        token.symbol, size_sol
                                    );
                                    (true, size_sol)
                                }
                                None => {
                                    match &eval.decision.action {
                                        TradingAction::FatalReject { reason } => warn!(
                                            "Strategy engine: FATAL REJECT for {}: {}",
                                            token.symbol, reason
                                        ),
                                        TradingAction::Skip { reason } => info!(
                                            "Strategy engine: SKIP {}: {}",
                                            token.symbol, reason
                                        ),
                                        other => info!(
                                            "Strategy engine: abstain (no entry) for {}: {:?}",
                                            token.symbol, other
                                        ),
                                    }
                                    (false, 0.0)
                                }
                            }
                        } else {
                            // No strategy engine - use adaptive filter multiplier
                            (true, config.trading.buy_amount_sol * position_multiplier)
                        };

                        // Skip if strategy engine rejected
                        if !strategy_entry {
                            continue;
                        }

                        let final_amount_sol = strategy_size;

                        // Execute buy
                        if !dry_run {
                            if let Some(ref trader) = trader_arc {
                                let mint = &token.mint;
                                let slippage_pct = config.trading.slippage_bps / 100;
                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                // Apply entry delay for adversarial resistance
                                if let Some(ref engine) = strategy_engine {
                                    let delay = engine.read().await.get_entry_delay().await;
                                    if delay.as_millis() > 0 {
                                        tracing::debug!("Applying entry delay: {}ms", delay.as_millis());
                                        tokio::time::sleep(delay).await;
                                    }
                                }

                                // Pre-send PositionManager capacity/risk check (INV-POS-008).
                                // Must run immediately before submission and is separate from
                                // record_confirmed_position (which runs post-fill).
                                if let Err(e) = position_manager.can_open_position(final_amount_sol).await {
                                    warn!(
                                        "PositionManager pre-trade risk check blocked {}: {}",
                                        token.symbol, e
                                    );
                                    continue;
                                }

                                // Compute entry_type from the adaptive recommendation BEFORE
                                // building any pending context or position.
                                let entry_type = match entry_recommendation {
                                    Recommendation::StrongBuy => crate::position::manager::EntryType::StrongBuy,
                                    Recommendation::Opportunity => crate::position::manager::EntryType::Opportunity,
                                    Recommendation::Probe => crate::position::manager::EntryType::Probe,
                                    _ => crate::position::manager::EntryType::Legacy,
                                };

                                info!("Buying {} SOL of {} ({})...", final_amount_sol, token.symbol, mint);

                                // MPT-001 E2: authoritative market-admission gate immediately
                                // before submission. Convert the final SOL size to exact lamports
                                // (fail closed on non-finite/nonpositive/overflow) and fetch a
                                // fresh, same-venue executable buy quote. A quote error is a
                                // market-admission failure (SOL-pair/venue gate), NOT a fill-rate
                                // failure: no transaction, no ExecutionRecord failure, no pending,
                                // just skip this candidate.
                                let exact_lamports = match sol_to_lamports_exact(final_amount_sol) {
                                    Some(l) => l,
                                    None => {
                                        warn!(
                                            "Market gate: rejecting buy of {} - unrepresentable SOL size {} for lamport quote",
                                            token.symbol, final_amount_sol
                                        );
                                        continue;
                                    }
                                };
                                let mint_pubkey = match Pubkey::from_str(mint) {
                                    Ok(pk) => pk,
                                    Err(e) => {
                                        warn!(
                                            "Market gate: rejecting buy of {} - invalid mint {}: {}",
                                            token.symbol, mint, e
                                        );
                                        continue;
                                    }
                                };
                                let buy_quote = match market_oracle
                                    .quote_buy_sol(&mint_pubkey, exact_lamports)
                                    .await
                                {
                                    Ok(q) => q,
                                    Err(e) => {
                                        // MarketData / UnsupportedQuoteMint: not a supported SOL
                                        // market at an executable size. Do NOT record a tx failure.
                                        warn!(
                                            "Market gate: no executable buy quote for {} ({} lamports): {} - skipping (no transaction submitted)",
                                            token.symbol, exact_lamports, e
                                        );
                                        continue;
                                    }
                                };
                                let buy_pool = pumpportal_pool_for_venue(buy_quote.venue);
                                info!(
                                    "Market gate PASSED for {}: venue={:?} pool={:?} expected_base_raw={} expected_price={:?} quote_slot={}",
                                    token.symbol,
                                    buy_quote.venue,
                                    buy_pool,
                                    buy_quote.base_amount_raw,
                                    buy_quote.expected_price_sol_per_token,
                                    buy_quote.slot
                                );

                                // Use buy_local for Local API, buy for Lightning API.
                                // MPT-001 E4: route-pinned to the quoted venue (no Auto). Same
                                // mint / final SOL amount / configured slippage+priority as before.
                                let buy_start = std::time::Instant::now();
                                let buy_result = if use_local_api {
                                    trader.buy_local_with_pool(mint, final_amount_sol, slippage_pct, priority_fee, &keypair, &rpc_client, buy_pool).await
                                } else {
                                    trader.buy_with_pool(mint, final_amount_sol, slippage_pct, priority_fee, buy_pool).await
                                };
                                // Provider submission/response latency (NOT chain-finality latency).
                                let buy_latency_ms = buy_start.elapsed().as_millis() as u64;

                                match buy_result {
                                    Ok(signature) => {
                                        // A returned signature is submission identity, NOT fill
                                        // proof (INV-TX-001). Do not call this "successful".
                                        info!("BUY SUBMITTED: {} - signature {}", token.symbol, signature);
                                        info!("View on Solscan: https://solscan.io/tx/{}", signature);

                                        // Resolve the exact execution wallet. Never invent a local
                                        // wallet on a live path.
                                        let execution_wallet = match primary_execution_wallet {
                                            Some(pk) => pk,
                                            None => {
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "STRUCTURAL: buy submitted for {} (sig {}) but execution wallet is unresolved; halting new entries",
                                                    token.symbol, signature
                                                );
                                                continue;
                                            }
                                        };
                                        let wallet_string = execution_wallet.to_string();

                                        // Persist the submitted signature BEFORE treating it as
                                        // filled (INV-TX-015).
                                        let pending_buy = PendingExecution::buy(
                                            signature.clone(),
                                            token.mint.clone(),
                                            wallet_string.clone(),
                                            PendingBuyContext {
                                                name: token.name.clone(),
                                                symbol: token.symbol.clone(),
                                                bonding_curve: token.bonding_curve_key.clone(),
                                                entry_type,
                                                requested_sol: final_amount_sol,
                                            },
                                        );
                                        // AUDIT-002 A4: retain the exact pending buy + whether the
                                        // first journal write persisted, so ambiguous/confirmed-
                                        // unapplied outcomes can retry durable persistence before
                                        // relying on restart recovery.
                                        let pending_buy_persisted = match pending_executions.upsert(pending_buy.clone()).await {
                                            Ok(()) => true,
                                            Err(e) => {
                                                // Serious state-integrity failure: the tx was already
                                                // sent. Halt new entries, still attempt immediate
                                                // reconciliation, never send another buy.
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "Failed to persist pending buy for {} (sig {}): {} - halting new entries; still reconciling",
                                                    token.symbol, signature, e
                                                );
                                                false
                                            }
                                        };

                                        // Reconcile the submitted signature. No sleep before the call.
                                        let outcome = trade_reconciler
                                            .reconcile(
                                                &signature,
                                                &wallet_string,
                                                mint,
                                                ReconciliationSide::Buy,
                                            )
                                            .await;

                                        match outcome {
                                            Ok(ReconciliationOutcome::ConfirmedFailure { error, observed_after_ms, .. }) => {
                                                // Real fill-rate failure sample.
                                                match pending_executions.remove(&signature).await {
                                                    Ok(_) => {}
                                                    Err(e) => {
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "Failed to remove pending buy after ConfirmedFailure (sig {}): {} - halting new entries",
                                                            signature, e
                                                        );
                                                    }
                                                }
                                                let total_latency_ms = buy_start.elapsed().as_millis() as u64;
                                                error!(
                                                    "BUY CONFIRMED FAILED for {} (sig {}): {} ({}ms observed)",
                                                    token.symbol, signature, error, observed_after_ms
                                                );
                                                if let Some(ref engine) = strategy_engine {
                                                    engine.write().await.record_tx_failure(
                                                        mint, true, final_amount_sol, total_latency_ms, &error,
                                                    ).await;
                                                }
                                                continue;
                                            }
                                            Ok(ReconciliationOutcome::Unresolved { reason, .. }) => {
                                                // Timeout/observation gap is NOT a failed fill
                                                // (INV-TX-014). Keep pending, halt new entries.
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                // AUDIT-002 A4: retry durable persistence if the
                                                // initial write failed.
                                                if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                    error!(
                                                        "BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept (durable), new entries HALTED",
                                                        mint, signature, wallet_string, reason
                                                    );
                                                } else {
                                                    error!(
                                                        "CRITICAL: BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                        mint, signature, wallet_string, reason, signature
                                                    );
                                                }
                                                continue;
                                            }
                                            Err(e) => {
                                                // Structural observer failure is not tx-failure proof.
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                // AUDIT-002 A4: same durability retry rule as Unresolved.
                                                if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                    error!(
                                                        "CRITICAL: buy reconciliation error for {} (sig {}): {} - pending kept (durable), new entries HALTED",
                                                        token.symbol, signature, e
                                                    );
                                                } else {
                                                    error!(
                                                        "CRITICAL: buy reconciliation error for {} (sig {}): {} - AND pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                        token.symbol, signature, e, signature
                                                    );
                                                }
                                                continue;
                                            }
                                            Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                                // Validate exact identity at the live boundary.
                                                if fill.side != ReconciliationSide::Buy
                                                    || fill.wallet != wallet_string
                                                    || fill.mint != *mint
                                                {
                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                    // AUDIT-002 A4: confirmed-but-unapplied. Retry durability.
                                                    if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                        error!(
                                                            "CRITICAL: reconciled buy fill identity mismatch for sig {} (wallet/mint/side) - pending kept (durable), new entries HALTED",
                                                            signature
                                                        );
                                                    } else {
                                                        error!(
                                                            "CRITICAL: reconciled buy fill identity mismatch for sig {} (wallet/mint/side) - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                            signature, signature
                                                        );
                                                    }
                                                    continue;
                                                }

                                                // Extract canonical fill economics via pure helper.
                                                let (token_amount_raw, _token_decimals, actual_cost_sol, actual_entry_price) =
                                                    match primary_buy_fill_values(&fill) {
                                                        Ok(v) => v,
                                                        Err(e) => {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            // AUDIT-002 A4: confirmed-but-unapplied. Retry durability.
                                                            if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                                error!(
                                                                    "CRITICAL: reconciled buy fill conversion failed for sig {}: {} - pending kept (durable), new entries HALTED",
                                                                    signature, e
                                                                );
                                                            } else {
                                                                error!(
                                                                    "CRITICAL: reconciled buy fill conversion failed for sig {}: {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                    signature, e, signature
                                                                );
                                                            }
                                                            continue;
                                                        }
                                                    };

                                                let entry_time = fill
                                                    .block_time
                                                    .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
                                                    .unwrap_or_else(chrono::Utc::now);

                                                info!(
                                                    "BUY CONFIRMED: {} (sig {}) raw_tokens={} decimals={} cost={:.9} SOL price={:.12} SOL/token wait={}ms",
                                                    token.symbol,
                                                    signature,
                                                    token_amount_raw,
                                                    fill.token_decimals,
                                                    actual_cost_sol,
                                                    actual_entry_price,
                                                    fill.reconciliation_wait_ms
                                                );

                                                // Canonical confirmed-owned position from actuals.
                                                let position = crate::position::manager::Position {
                                                    mint: token.mint.clone(),
                                                    name: token.name.clone(),
                                                    symbol: token.symbol.clone(),
                                                    bonding_curve: token.bonding_curve_key.clone(),
                                                    token_amount: token_amount_raw,
                                                    token_decimals: Some(fill.token_decimals),
                                                    entry_price: actual_entry_price,
                                                    total_cost_sol: actual_cost_sol,
                                                    entry_time,
                                                    entry_signature: fill.signature.clone(),
                                                    entry_type,
                                                    quick_profit_taken: false,
                                                    second_profit_taken: false,
                                                    peak_price: actual_entry_price,
                                                    current_price: actual_entry_price,
                                                    kill_switch_triggered: false,
                                                    kill_switch_reason: None,
                                                    wallet_pubkey: fill.wallet.clone(),
                                                    applied_exit_signatures: vec![],
                                                };

                                                // Record confirmed ownership. NOT open_position.
                                                let newly_applied = match position_manager
                                                    .record_confirmed_position(position)
                                                    .await
                                                {
                                                    Ok(applied) => applied,
                                                    Err(e) => {
                                                        // Confirmed on-chain but could not record.
                                                        // Keep pending, halt; do not pretend failure.
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        // AUDIT-002 A4: confirmed-but-unapplied. Retry durability.
                                                        if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                            error!(
                                                                "Confirmed owned position could not be recorded for {} (sig {}): {} - pending kept (durable), new entries HALTED",
                                                                token.symbol, signature, e
                                                            );
                                                        } else {
                                                            error!(
                                                                "CRITICAL: confirmed owned position could not be recorded for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                token.symbol, signature, e, signature
                                                            );
                                                        }
                                                        continue;
                                                    }
                                                };

                                                if newly_applied {
                                                    // D6 / INV-EVT-012: ONLY after the confirmed fill
                                                    // was durably recorded, request a dynamic
                                                    // SubscribeTokenTrades for this mint on the single
                                                    // runtime command sender. If the feed is enabled
                                                    // and a Data API key is configured but the
                                                    // subscribe cannot be sent, the position stays
                                                    // owned/tracked and price monitoring stays active,
                                                    // but NEW entries halt and we log that provider
                                                    // trade kill-switch coverage is unavailable for
                                                    // this position. Never erase the position; no
                                                    // second socket.
                                                    if confirmed_buy_closes_readiness_until_sync(
                                                        config.pumpportal.enabled,
                                                        &config.pumpportal.api_key,
                                                    ) {
                                                        // C5: a dynamic subscription is
                                                        // required. IMMEDIATELY before
                                                        // sending SubscribeTokenTrades,
                                                        // close the new-entry readiness
                                                        // gate so no SECOND live entry can
                                                        // start while this owned position's
                                                        // provider trade subscription is not
                                                        // yet synchronized. Readiness only
                                                        // reopens when the client re-emits
                                                        // Connected after the desired
                                                        // registry is synchronized (Agent A
                                                        // semantics). We never set readiness
                                                        // true locally.
                                                        data_stream_ready.store(false, Ordering::SeqCst);
                                                        if send_subscription_command(
                                                            &pumpportal_command_sender,
                                                            SubscriptionCommand::SubscribeTokenTrades(
                                                                vec![token.mint.clone()],
                                                            ),
                                                        )
                                                        .await
                                                        {
                                                            info!(
                                                                "Provider trade subscription requested for confirmed position {}",
                                                                &token.mint[..12]
                                                            );
                                                        } else {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            error!(
                                                                "Confirmed position {} recorded, but provider trade subscription could not be sent - position kept + price monitor active, provider trade kill-switch UNAVAILABLE for it, NEW entries HALTED",
                                                                &token.mint[..12]
                                                            );
                                                        }
                                                    }

                                                    // Kill-switch monitoring for the new position.
                                                    if let Some(ref evaluator) = kill_switch_evaluator {
                                                        let creator = token.trader_public_key.clone();
                                                        evaluator.watch_position(&token.mint, &creator, vec![]);
                                                        info!(
                                                            "Kill-switch monitoring active for {} (creator: {})",
                                                            &token.mint[..12], &creator[..8]
                                                        );
                                                    }

                                                    // Strategy portfolio record_entry using actuals.
                                                    if let Some(ref engine) = strategy_engine {
                                                        let strategy_position = crate::strategy::types::Position {
                                                            mint: token.mint.clone(),
                                                            entry_price: actual_entry_price,
                                                            entry_time,
                                                            size_sol: actual_cost_sol,
                                                            tokens_held: token_amount_raw,
                                                            strategy: config.strategy.default_strategy.clone(),
                                                            exit_style: crate::strategy::types::ExitStyle::default(),
                                                            highest_price: actual_entry_price,
                                                            lowest_price: actual_entry_price,
                                                            exit_levels_hit: vec![],
                                                        };
                                                        engine.write().await.record_entry(strategy_position).await;

                                                        // MPT-001 E5: reconciled-success execution
                                                        // feedback, recorded only AFTER the confirmed
                                                        // position was recorded. Fill remains P&L
                                                        // truth; the pre-send executable quote is the
                                                        // execution reference for quote-to-fill drift.
                                                        // Use the quoted API when the same-venue quote
                                                        // produced a finite expected price; otherwise
                                                        // keep the unquoted call (never fabricate one).
                                                        let total_execution_latency_ms =
                                                            buy_start.elapsed().as_millis() as u64;
                                                        match buy_quote.expected_price_sol_per_token {
                                                            Some(expected_price) => {
                                                                engine.write().await.record_reconciled_quoted_execution(
                                                                    mint,
                                                                    true,
                                                                    final_amount_sol,
                                                                    actual_cost_sol,
                                                                    expected_price,
                                                                    actual_entry_price,
                                                                    total_execution_latency_ms,
                                                                    &signature,
                                                                ).await;
                                                            }
                                                            None => {
                                                                engine.write().await.record_reconciled_execution(
                                                                    mint,
                                                                    true,
                                                                    final_amount_sol,
                                                                    actual_cost_sol,
                                                                    actual_entry_price,
                                                                    total_execution_latency_ms,
                                                                    &signature,
                                                                ).await;
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    // Idempotent: same signature already applied.
                                                    info!(
                                                        "Confirmed buy for {} (sig {}) was already applied; skipping duplicate strategy entry",
                                                        token.symbol, signature
                                                    );
                                                }

                                                // Remove pending LAST, after all state applied.
                                                if let Err(e) = pending_executions.remove(&signature).await {
                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                    error!(
                                                        "Failed to remove pending buy after confirmed fill (sig {}): {} - halting new entries; position retained",
                                                        signature, e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Provider submission error: no signature, no pending.
                                        error!("Buy submission failed for {}: {}", token.symbol, e);
                                        if let Some(ref engine) = strategy_engine {
                                            engine.write().await.record_tx_failure(
                                                mint, true, final_amount_sol, buy_latency_ms, &e.to_string(),
                                            ).await;
                                        }
                                    }
                                }
                            }
                        } else {
                            info!(
                                "DRY-RUN: Would buy {} SOL of {} (strategy size)",
                                final_amount_sol, token.mint
                            );
                        }
                    }
                    PumpPortalEvent::Trade(trade) => {
                        // PumpPortal TradeEvent.sol_amount is ALREADY in SOL (see
                        // stream::pumpportal). Do NOT divide by 1e9.
                        let sol_amount_sol = trade.sol_amount;

                        // Log all trades for visibility
                        info!(
                            "Trade: {} {} {:.6} SOL on {} (mcap: {:.2})",
                            &trade.trader_public_key[..8],
                            trade.tx_type,
                            sol_amount_sol,
                            &trade.mint[..12],
                            trade.market_cap_sol
                        );

                        // KILL-SWITCH: Check sells on tokens we hold
                        if trade.tx_type == "sell" {
                            // Check if we have a position in this token. Use a fresh
                            // canonical snapshot (E3 parses its exact wallet identity).
                            let our_position = position_manager.get_position(&trade.mint).await;

                            if let Some(position) = our_position {
                                if let Some(ref evaluator) = kill_switch_evaluator {
                                    // D10 / INV-EVT-015: PumpPortal `token_amount` is a
                                    // provider UI quantity, NOT canonical raw token units,
                                    // so it must never be cast to raw and fed to the
                                    // holder-quantity sell path. Use the identity-only
                                    // provider evaluator (deployer sell-any Immediate Exit).
                                    // The provider token amount stays informational only.
                                    let decision = evaluator.evaluate_provider_sell_identity(
                                        &trade.mint,
                                        &trade.trader_public_key,
                                        &trade.signature,
                                    );
                                    let _provider_ui_token_amount = trade.token_amount; // informational only; not raw

                                    if let KillSwitchDecision::Exit(alert) = decision {
                                        warn!(
                                            "KILL-SWITCH TRIGGERED for {}: {} - AUTO-SELLING",
                                            &trade.mint[..12], alert.reason
                                        );

                                        // Execute emergency sell if not dry run
                                        if dry_run {
                                            warn!(
                                                "DRY-RUN: Kill-switch would sell 100% of {} (reason: {})",
                                                &trade.mint[..12], alert.reason
                                            );
                                        } else {
                                            // §E2 / B8 PENDING GUARD: a pending Buy OR Sell for this
                                            // mint blocks an emergency sell. Keep the alert active.
                                            let ks_pending_buy = pending_executions
                                                .get_for_mint(&trade.mint, ReconciliationSide::Buy)
                                                .await;
                                            let ks_pending_sell = pending_executions
                                                .get_for_mint(&trade.mint, ReconciliationSide::Sell)
                                                .await;
                                            if let Some(sig) = pending_blocks_automatic_sell(
                                                ks_pending_buy.as_ref(),
                                                ks_pending_sell.as_ref(),
                                            ) {
                                                warn!(
                                                    "KILL-SWITCH: pending Buy/Sell already in flight for {} (sig {}) - not submitting a second emergency sell; alert remains active",
                                                    &trade.mint[..12], sig
                                                );
                                            } else {
                                                // §E3 / B12 EXACT ROUTE: resolve the exact execution
                                                // route and signer for the position's recorded wallet
                                                // via the recovery registry — NOT the new-buy mode.
                                                // Empty/invalid wallet, unknown route, or missing
                                                // trader/signer => no sell + halt new entries. No
                                                // Lightning->Local fallback (INV-WALLET-001/003).
                                                let position_wallet = match Pubkey::from_str(
                                                    position.wallet_pubkey.trim(),
                                                ) {
                                                    Ok(pk) if !position.wallet_pubkey.trim().is_empty() => pk,
                                                    _ => {
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "KILL-SWITCH: position {} has empty/invalid wallet_pubkey '{}' - no sell, new entries HALTED",
                                                            &trade.mint[..12], position.wallet_pubkey
                                                        );
                                                        continue;
                                                    }
                                                };

                                                // B9 step (2)/(3): reserve the mint via the SHARED
                                                // coordinator. If the primary monitor already owns an
                                                // in-flight sell for this mint, skip (no second sell).
                                                if !try_reserve_sell_mint(&active_sell_mints, &trade.mint) {
                                                    warn!(
                                                        "KILL-SWITCH: primary sell reservation already active for {} - not submitting a second emergency sell",
                                                        &trade.mint[..12]
                                                    );
                                                    continue;
                                                }

                                                // B9 step (4): re-check pending Buy/Sell after reserving.
                                                let ks_recheck_buy = pending_executions
                                                    .get_for_mint(&trade.mint, ReconciliationSide::Buy)
                                                    .await;
                                                let ks_recheck_sell = pending_executions
                                                    .get_for_mint(&trade.mint, ReconciliationSide::Sell)
                                                    .await;
                                                if let Some(sig) = pending_blocks_automatic_sell(
                                                    ks_recheck_buy.as_ref(),
                                                    ks_recheck_sell.as_ref(),
                                                ) {
                                                    release_sell_mint(&active_sell_mints, &trade.mint);
                                                    warn!(
                                                        "KILL-SWITCH: pending Buy/Sell appeared for {} after reservation (sig {}) - reservation released, no sell",
                                                        &trade.mint[..12], sig
                                                    );
                                                    continue;
                                                }

                                                let slippage_pct = config.trading.slippage_bps / 100;
                                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                                // MPT-001 Agent F8: the kill-switch is a
                                                // NON-price emergency risk exit. It must never
                                                // be blocked by market-oracle availability, so it
                                                // keeps the existing transaction-truth emergency
                                                // route with `PoolType::Auto` and UNQUOTED
                                                // execution feedback. No expected price / slippage
                                                // is fabricated (INV-MKT-013 / Section 26.5).
                                                warn!(
                                                    "KILL-SWITCH UNQUOTED EMERGENCY ROUTE for {} (reason: {}) - Auto pool, transaction-truth feedback only",
                                                    &trade.mint[..12], alert.reason
                                                );

                                                let sell_start = std::time::Instant::now();
                                                let routed_sell: Option<Result<String, crate::error::Error>> =
                                                    match recovery_registry.route_for(&position_wallet) {
                                                        Some(crate::wallet::ExecutionRoute::Local) => {
                                                            // Exact local signer + independent Local exit
                                                            // trader ONLY.
                                                            match primary_exit_local_trader {
                                                                None => None,
                                                                Some(ref local_trader) => {
                                                                    if keypair.pubkey() == position_wallet {
                                                                        info!(
                                                                            "KILL-SWITCH: Local sell for {} via primary keypair",
                                                                            &trade.mint[..12]
                                                                        );
                                                                        Some(local_trader.sell_local(
                                                                            &trade.mint,
                                                                            "100%",
                                                                            slippage_pct,
                                                                            priority_fee,
                                                                            &keypair,
                                                                            &rpc_client,
                                                                        ).await)
                                                                    } else if let Some(ref mw) = recovery_multi_wallet {
                                                                        match mw.find_by_address(&position.wallet_pubkey) {
                                                                            Some(tw) => {
                                                                                info!(
                                                                                    "KILL-SWITCH: Local sell for {} via recovery wallet {}",
                                                                                    &trade.mint[..12], position.wallet_pubkey
                                                                                );
                                                                                Some(local_trader.sell_local(
                                                                                    &trade.mint,
                                                                                    "100%",
                                                                                    slippage_pct,
                                                                                    priority_fee,
                                                                                    &tw.keypair,
                                                                                    &rpc_client,
                                                                                ).await)
                                                                            }
                                                                            None => None,
                                                                        }
                                                                    } else {
                                                                        None
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        Some(crate::wallet::ExecutionRoute::Lightning) => {
                                                            // Independent Lightning exit trader ONLY.
                                                            // No Local fallback.
                                                            match primary_exit_lightning_trader {
                                                                None => None,
                                                                Some(ref lightning_trader) => {
                                                                    info!(
                                                                        "KILL-SWITCH: Lightning sell for {}",
                                                                        &trade.mint[..12]
                                                                    );
                                                                    Some(lightning_trader.sell(&trade.mint, "100%", slippage_pct, priority_fee).await)
                                                                }
                                                            }
                                                        }
                                                        None => None,
                                                    };

                                                let sell_result = match routed_sell {
                                                    Some(r) => r,
                                                    None => {
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        release_sell_mint(&active_sell_mints, &trade.mint);
                                                        error!(
                                                            "KILL-SWITCH: no exact signer/route/trader for position {} wallet {} - no sell, reservation released, new entries HALTED",
                                                            &trade.mint[..12], position.wallet_pubkey
                                                        );
                                                        continue;
                                                    }
                                                };

                                                // §E4 SUBMIT / PERSIST / RECONCILE.
                                                let signature = match sell_result {
                                                    Ok(sig) => sig,
                                                    Err(e) => {
                                                        // Provider error: no pending. Record a strategy
                                                        // sell failure if enabled. Position stays.
                                                        let provider_latency_ms = sell_start.elapsed().as_millis() as u64;
                                                        error!(
                                                            "KILL-SWITCH SELL SUBMISSION FAILED for {}: {} ({}ms) - position remains OPEN/TRACKED",
                                                            &trade.mint[..12], e, provider_latency_ms
                                                        );
                                                        if let Some(ref engine) = strategy_engine {
                                                            engine.write().await.record_tx_failure(
                                                                &position.mint,
                                                                false,
                                                                position.total_cost_sol,
                                                                provider_latency_ms,
                                                                &e.to_string(),
                                                            ).await;
                                                        }
                                                        // B10: provider error / no signature => release.
                                                        release_sell_mint(&active_sell_mints, &trade.mint);
                                                        continue;
                                                    }
                                                };

                                                warn!(
                                                    "KILL-SWITCH SELL SUBMITTED: {} - sig: {}",
                                                    alert.reason, signature
                                                );

                                                let pending_sell = PendingExecution::sell(
                                                    signature.clone(),
                                                    position.mint.clone(),
                                                    position.wallet_pubkey.clone(),
                                                    PendingSellContext {
                                                        requested_amount: "100%".to_string(),
                                                        intent: PendingSellIntent::KillSwitch,
                                                        reason: alert.reason.clone(),
                                                    },
                                                );
                                                // AUDIT-002 A6: retain the exact pending record +
                                                // whether the first journal write persisted.
                                                let pending_sell_persisted = match pending_executions.upsert(pending_sell.clone()).await {
                                                    Ok(()) => true,
                                                    Err(e) => {
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "KILL-SWITCH: failed to persist pending sell (sig {}): {} - new entries HALTED, still reconciling",
                                                            signature, e
                                                        );
                                                        false
                                                    }
                                                };

                                                let outcome = trade_reconciler
                                                    .reconcile(
                                                        &signature,
                                                        &position.wallet_pubkey,
                                                        &position.mint,
                                                        ReconciliationSide::Sell,
                                                    )
                                                    .await;

                                                match outcome {
                                                    Ok(ReconciliationOutcome::ConfirmedFailure { error, observed_after_ms, .. }) => {
                                                        // Remove pending, record failure, keep position.
                                                        if let Err(e) = pending_executions.remove(&signature).await {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            error!(
                                                                "KILL-SWITCH: failed to remove pending after ConfirmedFailure (sig {}): {} - new entries HALTED",
                                                                signature, e
                                                            );
                                                        }
                                                        let latency_ms = sell_start.elapsed().as_millis() as u64;
                                                        error!(
                                                            "KILL-SWITCH SELL CONFIRMED FAILED for {} (sig {}): {} ({}ms observed) - position remains OPEN/TRACKED",
                                                            &trade.mint[..12], signature, error, observed_after_ms
                                                        );
                                                        if let Some(ref engine) = strategy_engine {
                                                            engine.write().await.record_tx_failure(
                                                                &position.mint,
                                                                false,
                                                                position.total_cost_sol,
                                                                latency_ms,
                                                                &error,
                                                            ).await;
                                                        }
                                                        // B10: ConfirmedFailure => pending removed, release.
                                                        release_sell_mint(&active_sell_mints, &trade.mint);
                                                        continue;
                                                    }
                                                    Ok(ReconciliationOutcome::Unresolved { reason: unresolved_reason, .. }) => {
                                                        // B10: Unresolved => KEEP pending AND KEEP the
                                                        // reservation; halt new entries, keep position.
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        // AUDIT-002 A6: retry durability if the initial
                                                        // write failed. Reservation kept regardless.
                                                        if retry_pending_durability_if_needed(&pending_executions, &pending_sell, pending_sell_persisted).await {
                                                            error!(
                                                                "KILL-SWITCH SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                                position.mint, signature, position.wallet_pubkey, unresolved_reason
                                                            );
                                                        } else {
                                                            error!(
                                                                "CRITICAL: KILL-SWITCH SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                position.mint, signature, position.wallet_pubkey, unresolved_reason, signature
                                                            );
                                                        }
                                                        continue;
                                                    }
                                                    Err(e) => {
                                                        // B10: structural reconciler Err => KEEP the
                                                        // reservation (not tx-failure proof).
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        // AUDIT-002 A6: same durability retry as Unresolved.
                                                        if retry_pending_durability_if_needed(&pending_executions, &pending_sell, pending_sell_persisted).await {
                                                            error!(
                                                                "CRITICAL: kill-switch sell reconciliation error for {} (sig {}): {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                                &trade.mint[..12], signature, e
                                                            );
                                                        } else {
                                                            error!(
                                                                "CRITICAL: kill-switch sell reconciliation error for {} (sig {}): {} - AND pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                &trade.mint[..12], signature, e, signature
                                                            );
                                                        }
                                                        continue;
                                                    }
                                                    Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                                        // Identity validation at the live boundary
                                                        // (exact wallet/mint/side).
                                                        if fill.side != ReconciliationSide::Sell
                                                            || fill.wallet != position.wallet_pubkey
                                                            || fill.mint != position.mint
                                                        {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            // AUDIT-002 A6: confirmed-but-unapplied.
                                                            // Retry durability; keep reservation.
                                                            if retry_pending_durability_if_needed(&pending_executions, &pending_sell, pending_sell_persisted).await {
                                                                error!(
                                                                    "CRITICAL: kill-switch fill identity mismatch for sig {} (wallet/mint/side) - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                                    signature
                                                                );
                                                            } else {
                                                                error!(
                                                                    "CRITICAL: kill-switch fill identity mismatch for sig {} (wallet/mint/side) - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                    signature, signature
                                                                );
                                                            }
                                                            continue;
                                                        }

                                                        // Exact economics + decimals validation (also
                                                        // rejects oversell / decimals mismatch).
                                                        let (actual_sold_raw, actual_received_sol, actual_exit_price) =
                                                            match primary_sell_fill_values(&fill, &position) {
                                                                Ok(v) => v,
                                                                Err(e) => {
                                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                                    // AUDIT-002 A6: confirmed-but-unapplied.
                                                                    if retry_pending_durability_if_needed(&pending_executions, &pending_sell, pending_sell_persisted).await {
                                                                        error!(
                                                                            "KILL-SWITCH fill validation failed for {} (sig {}): {} - pending kept (durable), reservation kept, position kept, new entries HALTED",
                                                                            position.mint, signature, e
                                                                        );
                                                                    } else {
                                                                        error!(
                                                                            "CRITICAL: KILL-SWITCH fill validation failed for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, position kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                            position.mint, signature, e, signature
                                                                        );
                                                                    }
                                                                    continue;
                                                                }
                                                            };

                                                        let pre_close_cost = position.total_cost_sol;
                                                        let pre_close_tokens = position.token_amount;

                                                        let close_result = match position_manager
                                                            .close_position_reconciled(
                                                                &position.mint,
                                                                &signature,
                                                                actual_sold_raw,
                                                                actual_received_sol,
                                                            )
                                                            .await
                                                        {
                                                            Ok(r) => r,
                                                            Err(e) => {
                                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                                // AUDIT-002 A6: confirmed-but-unapplied.
                                                                if retry_pending_durability_if_needed(&pending_executions, &pending_sell, pending_sell_persisted).await {
                                                                    error!(
                                                                        "KILL-SWITCH reconciled close failed for {} (sig {}): {} - pending kept (durable), reservation kept, new entries HALTED",
                                                                        position.mint, signature, e
                                                                    );
                                                                } else {
                                                                    error!(
                                                                        "CRITICAL: KILL-SWITCH reconciled close failed for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Reservation kept, new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                                        position.mint, signature, e, signature
                                                                    );
                                                                }
                                                                continue;
                                                            }
                                                        };

                                                        // Actual fill decides full vs partial.
                                                        let fully_closed = close_result.fully_closed;
                                                        let already_applied = close_result.already_applied;
                                                        let latency_ms = sell_start.elapsed().as_millis() as u64;

                                                        if !already_applied {
                                                            if let Some(ref engine) = strategy_engine {
                                                                if fully_closed {
                                                                    engine.write().await.record_exit(
                                                                        &position.mint,
                                                                        close_result.pnl_sol,
                                                                    ).await;
                                                                } else {
                                                                    let ok = engine.write().await.record_partial_exit(
                                                                        &position.mint,
                                                                        close_result.remaining_cost_sol,
                                                                        close_result.remaining_amount,
                                                                        close_result.pnl_sol,
                                                                    ).await;
                                                                    if !ok {
                                                                        warn!(
                                                                            "Strategy governor lacks position {} for kill-switch partial exit; PositionManager result unchanged",
                                                                            position.mint
                                                                        );
                                                                    }
                                                                }

                                                                let requested_proxy = if pre_close_tokens > 0 {
                                                                    pre_close_cost
                                                                        * (actual_sold_raw as f64 / pre_close_tokens as f64)
                                                                } else {
                                                                    pre_close_cost
                                                                };
                                                                engine.write().await.record_reconciled_execution(
                                                                    &position.mint,
                                                                    false,
                                                                    requested_proxy,
                                                                    actual_received_sol,
                                                                    actual_exit_price,
                                                                    latency_ms,
                                                                    &signature,
                                                                ).await;
                                                            }
                                                        }

                                                        // Full exit: unwatch evaluator. Partial: KEEP
                                                        // watching the remaining position.
                                                        if kill_switch_unwatch_on_close(fully_closed) {
                                                            info!("=== KILL-SWITCH SELL CONFIRMED (Full) ===");
                                                            evaluator.unwatch_position(&trade.mint);
                                                            // D8 / INV-EVT-013: primary event
                                                            // kill-switch FULL close => request
                                                            // UnsubscribeTokenTrades on the single
                                                            // runtime sender. Failure logged only.
                                                            if full_close_requests_unsubscribe(fully_closed)
                                                                && !send_subscription_command(
                                                                    &pumpportal_command_sender,
                                                                    SubscriptionCommand::UnsubscribeTokenTrades(
                                                                        vec![trade.mint.clone()],
                                                                    ),
                                                                )
                                                                .await
                                                            {
                                                                warn!(
                                                                    "Kill-switch full close: could not request token-trade unsubscribe for {} (no effect on position truth)",
                                                                    &trade.mint[..12]
                                                                );
                                                            }
                                                        } else {
                                                            info!("=== KILL-SWITCH SELL CONFIRMED (Partial) ===");
                                                        }
                                                        info!(
                                                            "  {} (sig {}) | sold_raw={} decimals={} net_sol_delta={:+.9} exit_price={:.12} SOL/token | realized P&L: {:+.9} SOL{}",
                                                            &trade.mint[..12],
                                                            signature,
                                                            actual_sold_raw,
                                                            fill.token_decimals,
                                                            actual_received_sol,
                                                            actual_exit_price,
                                                            close_result.pnl_sol,
                                                            if already_applied { " (already applied; idempotent)" } else { "" }
                                                        );

                                                        // Remove pending LAST.
                                                        if let Err(e) = pending_executions.remove(&signature).await {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            error!(
                                                                "KILL-SWITCH: failed to remove pending after confirmed fill (sig {}): {} - new entries HALTED; position state already applied",
                                                                signature, e
                                                            );
                                                        }
                                                        // B10: ConfirmedFill applied + pending removed
                                                        // LAST => release the same-mint reservation.
                                                        release_sell_mint(&active_sell_mints, &trade.mint);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Check for tracked wallet trades (copy trading).
                        if config.wallet_tracking.enabled && tracked_wallets.contains(&trade.trader_public_key) {
                            info!(
                                "Tracked wallet {} {} {:.4} SOL of {}",
                                trade.trader_public_key,
                                if trade.tx_type == "buy" { "bought" } else { "sold" },
                                sol_amount_sol,
                                trade.mint
                            );

                            // Automatic copy execution is DISABLED by P0: the old path could
                            // acquire wallet tokens without creating a fully reconciled managed
                            // position. Tracked-wallet signals are still observed/logged; they
                            // must go through the canonical entry + reconciliation pipeline
                            // before any automatic buy is re-enabled. auto_copy_trade config is
                            // left unchanged; this is a runtime safety gate.
                            if config.wallet_tracking.auto_copy_trade && trade.tx_type == "buy" {
                                info!(
                                    "Tracked-wallet buy observed; automatic copy execution is disabled by P0 \
                                     until it passes canonical entry + reconciliation."
                                );
                            }
                        }

                        // NOTE: the previous "one significant buy => auto-buy" bypass has been
                        // removed. It sent a buy after a single trade with only a minimal
                        // liquidity check and an estimated token quantity, bypassing the
                        // canonical decision gate. A trade event may update/log state, but this
                        // packet does not contain the canonical observation engine.
                    }
                    PumpPortalEvent::Migration(event) => {
                        // D9 / this-P0 boundary: log mint/pool only. Do NOT mutate
                        // PositionManager, do NOT mark any position graduated, no
                        // transaction. The market oracle remains the canonical venue
                        // resolver; provider migration price/liquidity is not market
                        // truth. Persisting migrations is a later recorder phase.
                        info!(
                            "Token migration observed: mint={} pool={:?} pool_id={:?} sig={:?}",
                            event.mint, event.pool, event.pool_id, event.signature
                        );
                    }
                    PumpPortalEvent::Connected => {
                        // D12 / INV-EVT-007: the client emits Connected only after the
                        // desired subscription registry has been replayed on the single
                        // socket. Mark the data stream ready => NEW entries may be
                        // admitted again (subject to new_entries_halted).
                        info!("Connected to token detection source");
                        data_stream_ready.store(true, Ordering::SeqCst);
                    }
                    PumpPortalEvent::Disconnected => {
                        // D12: no NEW entries until Connected again. Exits are NOT gated
                        // on data-stream readiness, so price monitoring/exits continue.
                        warn!("Disconnected from token detection source");
                        data_stream_ready.store(false, Ordering::SeqCst);
                    }
                    PumpPortalEvent::Error(e) => {
                        // D12 / INV-EVT-008: a provider RPC/WebSocket error is NOT a
                        // trading/transaction failure. Log the sanitized reason, halt
                        // NEW entries when running live, and mark the data stream not
                        // ready. Do NOT clear positions; existing price exits continue.
                        error!("Token detection stream error (not a transaction failure): {}", e);
                        data_stream_ready.store(false, Ordering::SeqCst);
                        if !dry_run {
                            new_entries_halted.store(true, Ordering::SeqCst);
                        }
                    }
                    PumpPortalEvent::DecodeError(_) => {
                        // AUDIT-001 §7: a local provider-message decode/schema loss is a
                        // dropped candidate, NOT a transport outage — so do NOT falsify
                        // data_stream_ready. But the live ingestion contract is now
                        // incomplete (at least one candidate was lost), so do NOT keep
                        // authorizing fresh capital: halt NEW live entries (sticky, live
                        // only). Do not clear positions, stop exits, or submit anything.
                        tracing::warn!("PumpPortal decode/schema loss; halting new live entries");
                        if !dry_run {
                            new_entries_halted.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal");
                // Save positions before exit
                if let Err(e) = position_manager.save().await {
                    error!("Failed to save positions: {}", e);
                }
                break;
            }
        }
    }

    Ok(())
}

// ===========================================================================
// AGENT H — MANUAL SELL TRANSACTION TRUTH (H1-H8)
//
// The manual `sell()` command is exact-wallet and transaction-reconciled. It
// never manufactures proceeds from a market-price estimate (INV-TX-007), never
// polls wallet SOL before/after as attribution (INV-TX-006 is satisfied by the
// reconciled fill's exact wallet SOL delta), never assumes six decimals, and
// never falls back between Local and Lightning signing authority
// (INV-WALLET-001/002/003). A submitted signature is submission identity, not
// fill proof (INV-TX-001).
// ===========================================================================

/// The exact wallet chosen for a manual sell, resolved BEFORE any submission.
///
/// - `Tracked` = a canonical tracked Position selected the wallet by its exact
///   recorded `wallet_pubkey`.
/// - `Untracked` = no tracked Position; a single positive on-chain holder among
///   controlled wallets selected the wallet. There is no authoritative cost
///   basis, so no P&L may be computed (H7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualSellWalletChoice {
    Tracked(Pubkey),
    Untracked(Pubkey),
}

impl ManualSellWalletChoice {
    fn wallet(&self) -> Pubkey {
        match self {
            ManualSellWalletChoice::Tracked(w) | ManualSellWalletChoice::Untracked(w) => *w,
        }
    }
    fn is_tracked(&self) -> bool {
        matches!(self, ManualSellWalletChoice::Tracked(_))
    }
}

/// H1 pure guard: may a manual sell be submitted for this mint given the current
/// pending journal state? An unresolved pending Buy OR Sell for the same mint
/// blocks another manual transaction (do not submit a second one). Returns the
/// blocking signature when blocked.
fn manual_sell_pending_block(
    pending_buy: Option<&PendingExecution>,
    pending_sell: Option<&PendingExecution>,
) -> Option<String> {
    if let Some(p) = pending_sell {
        return Some(p.signature.clone());
    }
    if let Some(p) = pending_buy {
        return Some(p.signature.clone());
    }
    None
}

/// The outcome of resolving an untracked token's controlled-wallet ownership (H2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManualUntrackedResolution {
    /// Exactly one controlled wallet holds a positive balance. Carries the proven
    /// raw balance + decimals so a manual percentage/numeric amount resolves to an
    /// exact raw size from proven on-chain ownership (MPT-001 Agent I1).
    Single(crate::wallet::WalletTokenState),
    /// Held in multiple controlled wallets => ambiguous, refuse.
    Ambiguous(Vec<Pubkey>),
    /// No controlled wallet holds a positive balance => refuse.
    NoHolder,
}

/// H2 pure mapping from an ownership-probe holder resolution to the manual-sell
/// decision for an UNTRACKED token. Multiple holders are ambiguous and MUST be
/// refused (INV-WALLET-004); zero holders refuse; exactly one resolves to that
/// exact wallet. There is NO preference for Lightning merely because a Lightning
/// wallet string exists — the wallet is chosen by proven on-chain ownership.
fn manual_untracked_resolution(
    resolution: &crate::wallet::OwnedHolderResolution,
) -> ManualUntrackedResolution {
    use crate::wallet::OwnedHolderResolution;
    match resolution {
        OwnedHolderResolution::None => ManualUntrackedResolution::NoHolder,
        OwnedHolderResolution::Single(state) => {
            ManualUntrackedResolution::Single(state.clone())
        }
        OwnedHolderResolution::Multiple(states) => {
            ManualUntrackedResolution::Ambiguous(states.iter().map(|s| s.wallet).collect())
        }
    }
}

/// C4/AUDIT-002 A1: retry durable persistence of an ALREADY-SUBMITTED pending
/// record when the initial post-signature journal write failed and the outcome
/// remains ambiguous or confirmed-but-unapplied (Unresolved / structural
/// reconciler error / confirmed-fill identity/validation/application failure).
///
/// This is used on EVERY live submit path (primary buy / primary auto-sell /
/// event kill-switch sell / HotScan buy / HotScan sell / manual sell). It
/// ensures the store's persistence directory is writable, then upserts. Any
/// error is returned so the caller can escalate to a CRITICAL, do-not-resubmit
/// report that preserves the public signature.
///
/// Because the transaction is already submitted, this helper must NEVER:
/// submit/re-submit; replace a conflicting same-signature record; remove another
/// pending record; or guess economic state. It only makes the exact pending
/// record durable.
///
/// Single-process coordination note (AUDIT-002 A13): persistent trading state is
/// currently single-process coordinated. Do not run manual sell / HotScan /
/// start concurrently against the same credentials_dir. Cross-process file
/// locking is not implemented.
async fn ensure_pending_execution_durable(
    store: &PendingExecutionStore,
    pending: &PendingExecution,
) -> crate::error::Result<()> {
    store.ensure_writable().await?;
    store.upsert(pending.clone()).await
}

/// AUDIT-002 A10: retry durable pending persistence only when the initial
/// post-signature journal write failed. Returns whether the pending record is
/// durable after this call:
/// - `initially_persisted == true` => already durable => `true` (no retry);
/// - initially failed + retry Ok => `true`;
/// - initially failed + retry Err => `false`.
///
/// The caller retains all policy decisions (halt flag, reservation lifetime,
/// continue/break/error). This helper never submits a transaction, never mutates
/// positions, and logs no secrets.
async fn retry_pending_durability_if_needed(
    store: &PendingExecutionStore,
    pending: &PendingExecution,
    initially_persisted: bool,
) -> bool {
    if initially_persisted {
        return true;
    }
    ensure_pending_execution_durable(store, pending).await.is_ok()
}

/// AUDIT-002 A11: pure reconciliation-outcome state for an already-submitted
/// transaction, used to decide whether a durable pending record is still
/// required for restart recovery when the initial journal write failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmittedOutcomeState {
    /// Chain proves no economic fill; no durable pending needed.
    ConfirmedFailure,
    /// Confirmed fill whose economic state was durably applied; no durable
    /// pending needed for restart economic recovery.
    ConfirmedApplied,
    /// Ambiguous (timeout/observation gap); durable pending required.
    Unresolved,
    /// Structural observer/reconciler error; durable pending required.
    StructuralError,
    /// Confirmed fill but identity/validation/application incomplete; durable
    /// pending required so the confirmed-but-unapplied fill is restart-recoverable.
    ConfirmedUnapplied,
}

/// AUDIT-002 A11: given whether the initial post-signature journal write
/// succeeded and the resolved outcome, decide whether a durability retry is
/// required. Persisted outcomes never require a retry; terminal outcomes
/// (ConfirmedFailure / ConfirmedApplied) never require an invented durable
/// record; ambiguous / confirmed-unapplied outcomes require durability when the
/// initial write failed. Pure and deterministic (no network / store).
fn pending_durability_required(
    initially_persisted: bool,
    state: SubmittedOutcomeState,
) -> bool {
    if initially_persisted {
        return false;
    }
    match state {
        SubmittedOutcomeState::ConfirmedFailure | SubmittedOutcomeState::ConfirmedApplied => false,
        SubmittedOutcomeState::Unresolved
        | SubmittedOutcomeState::StructuralError
        | SubmittedOutcomeState::ConfirmedUnapplied => true,
    }
}

/// C4 pure decision: given whether the initial post-signature journal write
/// succeeded and the resolved outcome kind, decide what pending action the
/// manual sell handler must take. Testable without any network or store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualOutcomeKind {
    ConfirmedFill,
    ConfirmedFailure,
    Unresolved,
    StructuralError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualPendingAction {
    /// Confirmed fill applied durably (or untracked): remove pending as usual.
    RemovePending,
    /// Confirmed failure: remove pending only if it was persisted; otherwise
    /// no failure record to invent.
    RemoveIfPersisted,
    /// Ambiguous outcome and pending already durable: keep as-is.
    KeepDurable,
    /// Ambiguous outcome but initial write failed: must re-persist durably.
    RetryDurable,
}

fn manual_pending_action(
    initial_persisted: bool,
    outcome: ManualOutcomeKind,
) -> ManualPendingAction {
    match outcome {
        ManualOutcomeKind::ConfirmedFill => ManualPendingAction::RemovePending,
        ManualOutcomeKind::ConfirmedFailure => ManualPendingAction::RemoveIfPersisted,
        ManualOutcomeKind::Unresolved | ManualOutcomeKind::StructuralError => {
            if initial_persisted {
                ManualPendingAction::KeepDurable
            } else {
                ManualPendingAction::RetryDurable
            }
        }
    }
}

/// Manually sell a token position — exact wallet, transaction-reconciled (H1-H8).
pub async fn sell(
    config: &Config,
    token: &str,
    amount: &str,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    // AUDIT-002 A13 — operational constraint (documentation only):
    // Persistent trading state is currently single-process coordinated. Do not
    // run manual sell / HotScan / start concurrently against the same
    // credentials_dir. Cross-process file locking is not implemented. The
    // ActiveSellMints reservation coordinates tasks inside one start() process
    // only; positions.json and pending_executions.json have no cross-process lock.
    //
    // E2 / INV-RUN-001/002: acquire the exclusive runtime lease for this
    // credentials_dir BEFORE any pending recovery or PositionManager mutation.
    // Held for the entire function lifetime. Applies EVEN in dry-run.
    let _runtime_lease =
        RuntimeLease::acquire(&config.wallet.credentials_dir, manual_sell_lease_label())
            .map_err(|e| anyhow::anyhow!("{}", e))?;

    info!("Sell command: token={}, amount={}", token, amount);

    // Parse token address.
    let token_pubkey = solana_sdk::pubkey::Pubkey::try_from(token)
        .map_err(|e| anyhow::anyhow!("Invalid token address: {}", e))?;

    // Parse the amount shape. A percentage ("50%") is resolved to an EXACT raw
    // proportion of the position later, once decimals/balance are known (I1). A
    // numeric UI amount is NOT parsed through f64 here — it is converted to raw
    // exactly via `decimal_token_amount_to_raw` after decimals are resolved (I1).
    // The percentage magnitude is parsed only to validate/derive the proportion.
    let is_percentage = amount.ends_with('%');
    if is_percentage {
        let v: f64 = amount
            .trim_end_matches('%')
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid percentage amount: {}", e))?;
        if v <= 0.0 || v > 100.0 {
            anyhow::bail!("Percentage must be between 0 and 100");
        }
    }

    if !config.pumpportal.use_for_trading {
        // Jito manual sell is not implemented; there is no reconciled path for it.
        anyhow::bail!(
            "Jito sell not implemented. Set pumpportal.use_for_trading = true in config.toml"
        );
    }

    // === H1: fail-closed initialization ===
    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    ));

    // Load the primary local keypair (exact local signing authority).
    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| "credentials/hot-trading/keypair.json".to_string());
    let keypair_data = std::fs::read_to_string(&keypair_path)?;
    let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
    let keypair = Arc::new(Keypair::from_bytes(&secret_key)?);

    // Position manager — fail closed on load error (no warn-and-continue).
    let position_manager = Arc::new(crate::position::manager::PositionManager::new(
        config.safety.clone(),
        Some(format!("{}/positions.json", config.wallet.credentials_dir)),
    ));
    position_manager.load().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to load persisted positions; refusing manual sell with unknown ownership state: {}",
            e
        )
    })?;

    // Trade reconciler + ownership probe (canonical 001A observer + C primitives).
    let trade_reconciler = TradeReconciler::new(rpc_client.clone());
    let ownership_probe = WalletOwnershipProbe::new(rpc_client.clone());

    // Pending-execution journal — load + ensure writable (fail closed).
    let pending_path = format!("{}/pending_executions.json", config.wallet.credentials_dir);
    let pending_store = PendingExecutionStore::new(pending_path);
    pending_store.load().await?;
    pending_store.ensure_writable().await?;

    // Recovery-only MultiWalletManager so an exact prior HotScan multi-wallet
    // signer can be recognized. Fail closed if configured wallets will not load.
    let mut recovery_local_wallets: Vec<Pubkey> = Vec::new();
    let recovery_multi_wallet = if !config.wallet.trading_wallets.is_empty() {
        match crate::wallet::MultiWalletManager::new(
            config.wallet.trading_wallets.clone(),
            &config.wallet.selection_strategy,
        ) {
            Ok(mw) => {
                for w in mw.wallets() {
                    recovery_local_wallets.push(w.pubkey());
                }
                Some(Arc::new(mw))
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to load configured trading_wallets; refusing manual sell: {}",
                    e
                ));
            }
        }
    } else {
        None
    };

    // Strictly parse the configured Lightning wallet if present (fail closed).
    let lightning_wallet: Option<Pubkey> = {
        let lw = config.pumpportal.lightning_wallet.trim();
        if lw.is_empty() {
            None
        } else {
            match Pubkey::from_str(lw) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Configured lightning_wallet is not a valid Pubkey; refusing manual sell: {}",
                        e
                    ));
                }
            }
        }
    };

    let registry =
        ExecutionWalletRegistry::new(keypair.pubkey(), &recovery_local_wallets, lightning_wallet);

    // H1: run pending recovery BEFORE submitting a new sell. This reconciles any
    // in-flight signatures against the chain and applies/removes them per plan.
    let _ = recover_pending_store(&trade_reconciler, &pending_store, &position_manager).await?;

    // H1: if the same mint still has an unresolved pending Buy or Sell after
    // recovery, do NOT submit another manual transaction.
    let pending_buy = pending_store
        .get_for_mint(token, ReconciliationSide::Buy)
        .await;
    let pending_sell = pending_store
        .get_for_mint(token, ReconciliationSide::Sell)
        .await;
    if let Some(sig) =
        manual_sell_pending_block(pending_buy.as_ref(), pending_sell.as_ref())
    {
        anyhow::bail!(
            "An unresolved pending transaction (signature {}) already exists for this mint; \
             refusing to submit another manual sell until it is reconciled.",
            sig
        );
    }

    // bought_mints cache (noncanonical metadata; only ever removed on proof).
    let bought_mints_path = format!("{}/bought_mints.json", config.wallet.credentials_dir);
    let bought_mints: Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>> = {
        if std::path::Path::new(&bought_mints_path).exists() {
            match std::fs::read_to_string(&bought_mints_path) {
                Ok(data) => match serde_json::from_str::<std::collections::HashMap<String, i64>>(
                    &data,
                ) {
                    Ok(mints) => Arc::new(tokio::sync::Mutex::new(mints)),
                    Err(_) => Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                },
                Err(_) => Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            }
        } else {
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
        }
    };
    let bought_mints_path = Arc::new(bought_mints_path);

    // === H2: resolve the EXACT execution wallet ===
    // I1: for an UNTRACKED sell there is no canonical Position, so the exact raw
    // balance + decimals come from the ownership probe. Captured here so a manual
    // percentage/numeric amount resolves to an exact raw size (never through f64).
    let mut untracked_balance: Option<crate::wallet::WalletTokenState> = None;
    let mut tracked_position = position_manager.get_position(token).await;
    let wallet_choice: ManualSellWalletChoice = match tracked_position.clone() {
        Some(pos) if !position_requires_recovery(&pos) => {
            // Tracked canonical Position: use its EXACT recorded wallet. A route
            // must exist for it (else refuse — no guessing).
            let wallet = pos.wallet_pubkey.parse::<Pubkey>().map_err(|e| {
                anyhow::anyhow!("Tracked position wallet is not a valid Pubkey: {}", e)
            })?;
            if registry.route_for(&wallet).is_none() {
                anyhow::bail!(
                    "Tracked position wallet {} has no controlled execution route; refusing manual sell.",
                    wallet
                );
            }
            ManualSellWalletChoice::Tracked(wallet)
        }
        Some(_pos) => {
            // Tracked LEGACY position (decimals None / invalid wallet). Attempt
            // legacy recovery for this mint FIRST; if it remains recovery-required,
            // refuse rather than guess units/cost (INV-POS-002/003).
            info!(
                "Manual sell: tracked position for {} is legacy/incomplete; attempting chain recovery",
                token
            );
            let _ = recover_legacy_positions(
                &trade_reconciler,
                &ownership_probe,
                &registry,
                &position_manager,
            )
            .await?;

            // Re-read; only proceed if it is now fully canonical AND routable.
            match position_manager.get_position(token).await {
                Some(p) if !legacy_recovery_required(&p, &registry) => {
                    let wallet = p.wallet_pubkey.parse::<Pubkey>().map_err(|e| {
                        anyhow::anyhow!(
                            "Recovered position wallet is not a valid Pubkey: {}",
                            e
                        )
                    })?;
                    tracked_position = Some(p);
                    ManualSellWalletChoice::Tracked(wallet)
                }
                _ => {
                    anyhow::bail!(
                        "Position for {} remains recovery-required after chain recovery; \
                         refusing manual sell rather than guessing units/cost.",
                        token
                    );
                }
            }
        }
        None => {
            // No tracked Position: probe ALL controlled wallets for the mint.
            let resolution = ownership_probe
                .find_positive_holders(&registry, token_pubkey)
                .await
                .map_err(|e| anyhow::anyhow!("Ownership probe failed: {}", e))?;
            match manual_untracked_resolution(&resolution) {
                ManualUntrackedResolution::Single(state) => {
                    let wallet = state.wallet;
                    untracked_balance = Some(state);
                    ManualSellWalletChoice::Untracked(wallet)
                }
                ManualUntrackedResolution::Ambiguous(wallets) => {
                    for w in &wallets {
                        println!("  controlled wallet holding {}: {}", token, w);
                    }
                    anyhow::bail!(
                        "Token is held in multiple controlled wallets; manual sell is ambiguous. \
                         Explicit wallet selection is required but this CLI does not provide it yet."
                    );
                }
                ManualUntrackedResolution::NoHolder => {
                    anyhow::bail!(
                        "No controlled wallet holds a positive balance of {}; nothing to sell.",
                        token
                    );
                }
            }
        }
    };

    let execution_wallet = wallet_choice.wallet();
    let route = match registry.route_for(&execution_wallet) {
        Some(r) => r,
        None => {
            anyhow::bail!(
                "Resolved wallet {} has no controlled execution route; refusing manual sell.",
                execution_wallet
            );
        }
    };

    if let Some(ref pos) = tracked_position {
        println!("\nPosition found:");
        println!("  Symbol: {}", pos.symbol);
        println!("  Tokens (raw): {}", pos.token_amount);
        println!("  Entry price: {:.10} SOL", pos.entry_price);
        println!("  Cost: {:.4} SOL", pos.total_cost_sol);
    }
    println!("  Execution wallet: {} ({:?})", execution_wallet, route);

    // === I1: resolve the EXACT raw amount + token decimals BEFORE quoting/prompt ===
    // Tracked: proportion (percentage) or exact numeric conversion uses the tracked
    // raw token_amount and the position's confirmed decimals. Untracked: the proven
    // wallet raw balance + decimals from the ownership probe. No f64 for absolute
    // token amounts (I1); percentages resolve to an exact raw proportion.
    let (base_total_raw, token_decimals): (u64, u8) = match &tracked_position {
        Some(pos) => {
            let decimals = pos.token_decimals.ok_or_else(|| {
                anyhow::anyhow!(
                    "Tracked position for {} has no canonical decimals; refusing manual sell.",
                    token
                )
            })?;
            (pos.token_amount, decimals)
        }
        None => {
            let state = untracked_balance.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Untracked sell for {} has no proven wallet balance; refusing manual sell.",
                    token
                )
            })?;
            let decimals = state.decimals.ok_or_else(|| {
                anyhow::anyhow!(
                    "Untracked wallet balance for {} has no decimals; refusing manual sell.",
                    token
                )
            })?;
            (state.raw_amount, decimals)
        }
    };

    let resolved_raw: u64 = if is_percentage {
        percent_of_raw(base_total_raw, amount.trim_end_matches('%'))
            .map_err(|e| anyhow::anyhow!("Failed to resolve percentage sell amount: {}", e))?
    } else {
        let raw = decimal_token_amount_to_raw(amount, token_decimals)
            .map_err(|e| anyhow::anyhow!("Failed to convert token amount to raw: {}", e))?;
        if raw == 0 {
            anyhow::bail!("Resolved token amount is zero; nothing to sell.");
        }
        if raw > base_total_raw {
            anyhow::bail!(
                "Requested {} tokens exceeds available balance ({} raw); refusing manual sell.",
                amount,
                base_total_raw
            );
        }
        raw
    };

    // Exact decimal amount string derived from the resolved raw size — this is the
    // SAME size that is quoted, route-pinned and submitted (I2/I3).
    let submit_amount = raw_token_amount_to_decimal_string(resolved_raw, token_decimals);

    // === I2 / BLOCKER B(2): fetch a PREVIEW exact-size sell quote for DISPLAY ONLY ===
    // Build a PumpMarketOracle inline from the manual RPC client (Agent G pattern).
    // No DexScreener. This first quote is a PREVIEW shown to the human; it is NOT
    // used to pin the venue or submit. By the time the human confirms it may be
    // arbitrarily stale, so a second FINAL quote is fetched below immediately
    // before live submit (two-quote semantics). A normal manual sell still
    // requires a successful same-venue SOL quote to even display; --force never
    // bypasses either quote (it only skips the human prompt in H4).
    let market_oracle = Arc::new(PumpMarketOracle::new(rpc_client.clone()));
    let preview_quote = match market_oracle
        .quote_sell_raw(&token_pubkey, resolved_raw)
        .await
    {
        Ok(q) => q,
        Err(e) => {
            // I3: quote unavailable => refuse the normal manual sell (no submit).
            anyhow::bail!(
                "No executable market quote for {} at exact size {} raw ({}): refusing manual sell. \
                 This is a market-admission failure, not a submission. {}",
                token,
                resolved_raw,
                submit_amount,
                e
            );
        }
    };

    // I3: the PREVIEW must itself be a same-venue SOL quote for the sell to be
    // admissible at all; the pool shown here is preview-only and is re-derived
    // from the FINAL quote before submission (never Auto, never the preview).
    let preview_decision = manual_sell_decision(Some(&preview_quote));
    let preview_pool = match preview_decision {
        ManualSellDecision::Submit { pool } => pool,
        ManualSellDecision::Refuse => {
            anyhow::bail!(
                "Market quote for {} is not a supported SOL pair (venue {:?}); refusing manual sell \
                 (unsupported quote mint). Kill-switch/emergency is a separate path.",
                token,
                preview_quote.venue
            );
        }
    };

    // I2 display: venue, exact raw + UI amount, fresh mark, expected net SOL out,
    // expected executable price, and the quote slot. Then the existing prompt.
    let fresh_mark = match market_oracle.snapshot(&token_pubkey).await {
        Ok(snap) => snap.mark_price_sol_per_token,
        Err(_) => None,
    };
    println!("\n=== EXECUTABLE SELL QUOTE (PREVIEW) ===");
    println!("  Venue: {:?} (pool {:?})", preview_quote.venue, preview_pool);
    println!("  Exact raw amount: {}", resolved_raw);
    println!("  Exact UI amount: {}", submit_amount);
    match fresh_mark {
        Some(m) => println!("  Fresh mark: {:.12} SOL/token", m),
        None => println!("  Fresh mark: unavailable"),
    }
    match preview_quote.expected_sol() {
        Some(sol) => println!("  Expected protocol net SOL out: {:.9} SOL", sol),
        None => println!("  Expected protocol net SOL out: unavailable"),
    }
    match preview_quote.expected_price_sol_per_token {
        Some(p) => println!("  Expected executable price: {:.12} SOL/token", p),
        None => println!("  Expected executable price: unavailable"),
    }
    println!("  Quote slot: {}", preview_quote.slot);
    println!("  (preview only — a fresh quote is taken immediately before submit)");

    // === H4: preserve the manual confirmation prompt + dry-run no-send ===
    if config.safety.require_sell_confirmation && !force {
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Sell {} of token {} from wallet {}? This cannot be undone.",
                amount, token, execution_wallet
            ))
            .default(false)
            .interact()?;
        if !confirmed {
            info!("Sell cancelled by user");
            return Ok(());
        }
    }

    if dry_run {
        // BLOCKER B(5): DRY-RUN returns HERE, after confirmation and before any
        // FINAL quote. No second quote is fetched and nothing is submitted. The
        // pool reported is the PREVIEW pool (display only) — a dry-run never pins
        // a live venue.
        info!(
            "DRY-RUN: Would sell {} (raw {}) of {} from {} via preview pool {:?} \
             (a fresh FINAL quote would be taken before a LIVE submit)",
            submit_amount, resolved_raw, token, execution_wallet, preview_pool
        );
        return Ok(());
    }

    // === BLOCKER B(6)/(8)/(9): LIVE ONLY — FINAL exact-size quote immediately before submit ===
    // The human has confirmed and this is a LIVE sell. Re-quote the EXACT same raw
    // size RIGHT NOW so the venue/price used for submission is fresh, not the
    // possibly-stale preview. --force never skips this. There is NO sleep,
    // DexScreener, extra snapshot, or unrelated network call between this final
    // quote and the submit below.
    let final_quote = match market_oracle
        .quote_sell_raw(&token_pubkey, resolved_raw)
        .await
    {
        Ok(q) => q,
        Err(e) => {
            // Final quote unavailable => refuse the sell (no submit). Nothing was
            // reserved or submitted; the tracked position is untouched.
            anyhow::bail!(
                "No FINAL executable market quote for {} at exact size {} raw ({}) immediately before \
                 submit: refusing manual sell (fail closed). {}",
                token,
                resolved_raw,
                submit_amount,
                e
            );
        }
    };

    // BLOCKER B(8): validate the FINAL quote (mint/side/exact raw/SOL pair/finite
    // positive price) and derive the pinned pool from the FINAL venue. On any
    // failure we refuse (no submit); no reservation was taken so none is released.
    let sell_pool = match validate_final_manual_sell_quote(&token_pubkey, resolved_raw, &final_quote)
    {
        Ok(pool) => pool,
        Err(e) => {
            anyhow::bail!(
                "FINAL manual-sell quote validation failed for {}: {}; refusing manual sell (no submit).",
                token,
                e
            );
        }
    };

    // BLOCKER B: if the token graduated during the human delay the FINAL venue may
    // differ from the preview (e.g. Pump -> PumpSwap => PoolType::PumpAmm). We use
    // the FINAL venue and LOG the change; we do NOT re-prompt (a second prompt
    // would let this fresh quote go stale again).
    if final_quote.venue != preview_quote.venue {
        info!(
            "Manual sell: venue changed between preview and final quote for {} \
             (preview {:?}/{:?} -> final {:?}/{:?}); submitting on FINAL venue without re-confirmation",
            token, preview_quote.venue, preview_pool, final_quote.venue, sell_pool
        );
    }

    // BLOCKER B(11): quote-to-fill drift references the FINAL quote's expected
    // price, never the preview's.
    let expected_quote_price = final_quote.expected_price_sol_per_token;

    // === H3/I3: exact route submission, venue-pinned to the FINAL quoted venue ===
    // Submit the EXACT decimal amount derived from the resolved raw size (I3), and
    // pin the pool to the quoted venue (never Auto for a normal manual sell). Quote
    // input, route and submitted amount are therefore the same intended size.
    let slippage_pct = config.trading.slippage_bps / 100;
    let priority_fee = config.trading.priority_fee_lamports as f64 / 1_000_000_000.0;

    let submit_result: crate::error::Result<String> = match route {
        crate::wallet::ExecutionRoute::Local => {
            // Resolve the EXACT local signer for this wallet: primary keypair or a
            // recovery MultiWallet signer. No fallback (INV-WALLET-003).
            if execution_wallet == keypair.pubkey() {
                info!(
                    "Manual sell: local submission via primary keypair (pool {:?})",
                    sell_pool
                );
                pumpportal_local_trader()
                    .sell_local_with_pool(
                        token,
                        &submit_amount,
                        slippage_pct,
                        priority_fee,
                        &keypair,
                        &rpc_client,
                        sell_pool,
                    )
                    .await
            } else {
                match recovery_multi_wallet
                    .as_ref()
                    .and_then(|mw| mw.find_by_address(&execution_wallet.to_string()))
                {
                    Some(tw) => {
                        info!(
                            "Manual sell: local submission via recovery wallet {} (pool {:?})",
                            execution_wallet, sell_pool
                        );
                        pumpportal_local_trader()
                            .sell_local_with_pool(
                                token,
                                &submit_amount,
                                slippage_pct,
                                priority_fee,
                                &tw.keypair,
                                &rpc_client,
                                sell_pool,
                            )
                            .await
                    }
                    None => {
                        anyhow::bail!(
                            "No exact local signer for wallet {}; refusing manual sell (no fallback).",
                            execution_wallet
                        );
                    }
                }
            }
        }
        crate::wallet::ExecutionRoute::Lightning => {
            // Lightning requires an API key AND the exact configured Lightning
            // wallet. No Local fallback (INV-WALLET-001/002).
            if config.pumpportal.api_key.is_empty() {
                anyhow::bail!("PumpPortal API key required for a Lightning manual sell.");
            }
            info!(
                "Manual sell: Lightning submission (wallet {}, pool {:?})",
                execution_wallet, sell_pool
            );
            PumpPortalTrader::lightning(config.pumpportal.api_key.clone())
                .sell_with_pool(token, &submit_amount, slippage_pct, priority_fee, sell_pool)
                .await
        }
    };

    // === H5: submit => pending => reconcile ===
    let signature = match submit_result {
        Ok(sig) => sig,
        Err(e) => {
            // Provider error before a signature: nothing to reconcile, no pending,
            // tracked position unchanged.
            error!("Manual sell submission failed: {}", e);
            anyhow::bail!("Manual sell submission failed: {}", e);
        }
    };

    info!("SELL SUBMITTED: {} (sig {})", token, signature);
    println!("\nSELL SUBMITTED");
    println!("Signature: {}", signature);
    println!("View on Solscan: https://solscan.io/tx/{}", signature);

    // Persist the pending record BEFORE treating the trade as filled (INV-TX-001).
    let pending = PendingExecution::sell(
        signature.clone(),
        token.to_string(),
        execution_wallet.to_string(),
        PendingSellContext {
            requested_amount: submit_amount.clone(), // exact submitted decimal amount (I3)
            intent: PendingSellIntent::Manual,
            reason: "manual".to_string(),
        },
    );
    // C3: a post-signature journal write failure must NOT short-circuit immediate
    // reconciliation. The transaction is already submitted; returning here would
    // leave a live, unreconciled signature with no durable record. Record whether
    // the initial persist succeeded and continue to reconcile either way.
    let initial_persisted = match pending_store.upsert(pending.clone()).await {
        Ok(()) => true,
        Err(e) => {
            error!(
                "submitted signature {} but pending journal write failed: {} - continuing immediate reconciliation",
                signature, e
            );
            false
        }
    };

    // Reconcile: exact wallet/mint/Sell. No sleep, no balance polling, no estimate.
    let outcome = trade_reconciler
        .reconcile(
            &signature,
            &execution_wallet.to_string(),
            token,
            ReconciliationSide::Sell,
        )
        .await;

    match outcome {
        Ok(ReconciliationOutcome::ConfirmedFailure {
            error,
            observed_after_ms,
            ..
        }) => {
            // C5: chain proves failure. If a pending record was persisted, remove
            // it. If the initial persist never succeeded, there is no pending
            // failure record to invent. No economic state mutation either way.
            if initial_persisted {
                pending_store.remove(&signature).await?;
            }
            error!(
                "Manual sell CONFIRMED FAILED (sig {}): {} ({}ms observed)",
                signature, error, observed_after_ms
            );
            anyhow::bail!(
                "Manual sell transaction confirmed FAILED on-chain (signature {}): {}",
                signature,
                error
            );
        }
        Ok(ReconciliationOutcome::Unresolved { reason, .. }) => {
            // C4: KEEP pending; report UNRESOLVED including the signature; no
            // Position mutation; do NOT tell the user to immediately retry. If the
            // initial post-signature journal write failed, the pending record is
            // NOT durable yet — retry durable persistence before returning so a
            // restart can recover this in-flight signature.
            if !initial_persisted {
                match ensure_pending_execution_durable(&pending_store, &pending).await {
                    Ok(()) => {
                        error!(
                            "Manual sell UNRESOLVED (sig {}): {} - initial journal write failed but pending is now durable",
                            signature, reason
                        );
                    }
                    Err(persist_err) => {
                        error!(
                            "CRITICAL: manual sell UNRESOLVED (sig {}): {} - AND pending journal is NOT durable: {}",
                            signature, reason, persist_err
                        );
                        anyhow::bail!(
                            "CRITICAL: manual sell outcome is UNRESOLVED for signature {} and the pending \
                             journal is NOT durable ({}). The transaction was submitted; do NOT resubmit. \
                             Preserve and investigate signature {} before taking any further action. \
                             Reason: {}",
                            signature,
                            persist_err,
                            signature,
                            reason
                        );
                    }
                }
            } else {
                error!(
                    "Manual sell UNRESOLVED (sig {}): {} - pending kept, position unchanged",
                    signature, reason
                );
            }
            anyhow::bail!(
                "Manual sell outcome is UNRESOLVED for signature {}. The transaction was submitted \
                 but its on-chain result could not be confirmed. It remains recorded as pending; \
                 do NOT resubmit. Investigate the signature before taking further action. Reason: {}",
                signature,
                reason
            );
        }
        Err(e) => {
            // C4: a structural observer error is NOT tx-failure proof. Keep
            // pending. If the initial journal write failed, retry durable
            // persistence before returning (same policy as Unresolved).
            if !initial_persisted {
                match ensure_pending_execution_durable(&pending_store, &pending).await {
                    Ok(()) => {
                        error!(
                            "Manual sell reconciliation error (sig {}): {} - initial journal write failed but pending is now durable",
                            signature, e
                        );
                    }
                    Err(persist_err) => {
                        error!(
                            "CRITICAL: manual sell reconciliation error (sig {}): {} - AND pending journal is NOT durable: {}",
                            signature, e, persist_err
                        );
                        anyhow::bail!(
                            "CRITICAL: manual sell outcome is UNRESOLVED for signature {} (reconciliation \
                             observer error: {}) and the pending journal is NOT durable ({}). The \
                             transaction was submitted; do NOT resubmit. Preserve and investigate \
                             signature {} before taking any further action.",
                            signature,
                            e,
                            persist_err,
                            signature
                        );
                    }
                }
            } else {
                error!(
                    "Manual sell reconciliation error (sig {}): {} - pending kept, position unchanged",
                    signature, e
                );
            }
            anyhow::bail!(
                "Manual sell outcome is UNRESOLVED for signature {} (reconciliation observer error): {}. \
                 It remains recorded as pending; do NOT resubmit.",
                signature,
                e
            );
        }
        Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
            // C6: identity/fill validation happens AFTER a confirmed fill. If it
            // fails we must keep the pending record for restart recovery. When the
            // initial journal write failed, retry durable persistence before
            // returning the error so the confirmed-but-unapplied fill is not lost.
            if fill.side != ReconciliationSide::Sell
                || fill.wallet != execution_wallet.to_string()
                || fill.mint != token
            {
                if !initial_persisted {
                    if let Err(persist_err) =
                        ensure_pending_execution_durable(&pending_store, &pending).await
                    {
                        anyhow::bail!(
                            "CRITICAL: reconciled manual sell fill identity mismatch for signature {} \
                             AND pending journal is NOT durable ({}); confirmed fill is unapplied. Do \
                             NOT resubmit; preserve and investigate signature {}.",
                            signature,
                            persist_err,
                            signature
                        );
                    }
                }
                anyhow::bail!(
                    "Reconciled manual sell fill identity mismatch for signature {}; pending kept, \
                     position unchanged.",
                    signature
                );
            }

            // AUDIT-002 A9: the confirmed fill is identity-validated. The early
            // raw/net validation MUST NOT bypass durability recovery for a
            // TRACKED confirmed-but-unapplied sell when the initial pending
            // persistence failed. For a tracked position, primary_sell_fill_values
            // below is the authoritative raw/net/price validation boundary (it
            // already rejects a raw that does not fit u64, a zero raw, and a
            // non-finite net SOL) and its failure arm attempts durable
            // persistence. So we do NOT duplicate raw/net validation ahead of it.
            //
            // The UI-only display values below are non-authoritative and never
            // gate application; the untracked branch performs its own explicit
            // representation checks where a u64 raw is actually reported.
            let ui_sold = fill.token_amount_ui();
            // Economic (effective) price; a fee-dominated sale may be zero/negative.
            let effective_price = fill.effective_price_sol_per_token().unwrap_or(0.0);

            if let Some(pos) = tracked_position {
                // === H6: tracked Position application ===
                // Validate fill decimals vs Position before mutation.
                let (validated_sold_raw, validated_net_sol, validated_price) =
                    match primary_sell_fill_values(&fill, &pos) {
                        Ok(v) => v,
                        Err(e) => {
                            // C6: fill validation failed AFTER a confirmed fill.
                            // Keep pending for restart recovery; re-persist if the
                            // initial journal write failed.
                            if !initial_persisted {
                                if let Err(persist_err) =
                                    ensure_pending_execution_durable(&pending_store, &pending).await
                                {
                                    anyhow::bail!(
                                        "CRITICAL: reconciled sell fill validation failed (sig {}): {} \
                                         AND pending journal is NOT durable ({}); confirmed fill is \
                                         unapplied. Do NOT resubmit; preserve and investigate signature {}.",
                                        signature,
                                        e,
                                        persist_err,
                                        signature
                                    );
                                }
                            }
                            anyhow::bail!(
                                "Reconciled sell fill validation failed (sig {}): {}; pending kept.",
                                signature,
                                e
                            );
                        }
                    };

                let close_result = match position_manager
                    .close_position_reconciled(
                        token,
                        &signature,
                        validated_sold_raw,
                        validated_net_sol,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        // C6: close application failed AFTER a confirmed fill.
                        // Keep pending for restart recovery; re-persist if the
                        // initial journal write failed.
                        if !initial_persisted {
                            if let Err(persist_err) =
                                ensure_pending_execution_durable(&pending_store, &pending).await
                            {
                                anyhow::bail!(
                                    "CRITICAL: reconciled close failed (sig {}): {} AND pending journal \
                                     is NOT durable ({}); confirmed fill is unapplied. Do NOT resubmit; \
                                     preserve and investigate signature {}.",
                                    signature,
                                    e,
                                    persist_err,
                                    signature
                                );
                            }
                        }
                        anyhow::bail!(
                            "Reconciled close failed (sig {}): {}; pending kept.",
                            signature,
                            e
                        );
                    }
                };

                // Remove pending LAST, after durable application.
                pending_store.remove(&signature).await?;

                println!("\n=== MANUAL SELL CONFIRMED ===");
                println!("  Signature: {}", signature);
                println!("  Raw sold: {}", validated_sold_raw);
                println!("  UI sold: {:.6}", ui_sold);
                println!("  Net SOL (wallet delta): {:+.6} SOL", validated_net_sol);
                println!("  Effective price: {:.10} SOL/token", validated_price);
                // I4: expected quote price, actual fill price, quote-to-fill drift.
                match expected_quote_price {
                    Some(p) => println!("  Expected quote price: {:.10} SOL/token", p),
                    None => println!("  Expected quote price: unavailable"),
                }
                println!("  Actual fill price: {:.10} SOL/token", validated_price);
                match expected_quote_price.and_then(|p| manual_sell_drift_pct(p, validated_price)) {
                    Some(d) => println!("  Quote-to-fill drift: {:+.4}% (positive = worse)", d),
                    None => println!("  Quote-to-fill drift: unavailable"),
                }
                println!("  Realized P&L: {:+.6} SOL", close_result.pnl_sol);
                if close_result.fully_closed {
                    println!("  Position fully closed.");
                    // Remove bought_mint only on a full close.
                    let _ = remove_bought_mint(&bought_mints, &bought_mints_path, token).await;
                } else {
                    println!(
                        "  Partial exit — remaining tracked raw: {}, remaining cost: {:.6} SOL",
                        close_result.remaining_amount, close_result.remaining_cost_sol
                    );
                }
            } else {
                // === H7: untracked token application ===
                // No authoritative cost basis: do NOT create fake P&L, do NOT call
                // PositionManager close. Report actuals and state P&L unavailable.
                // AUDIT-002 A9: there is no PositionManager state to apply for an
                // untracked confirmed fill, so no durable pending is required for
                // restart economic recovery. Report confirmed actuals. If a u64
                // raw amount cannot be represented we return an explicit
                // representation error — we do NOT invent a fake u64 and do NOT
                // claim the transaction is unresolved when the chain fill is
                // confirmed. The pending record is removed LAST below.
                let net_sol = fill.wallet_sol_delta_sol();
                // Report the raw magnitude via the fill's own representation. When
                // it does not fit u64, surface the wider on-chain magnitude as a
                // string rather than fabricating a u64.
                let raw_sold_display: String = match fill.token_amount_raw() {
                    Some(raw) => raw.to_string(),
                    None => format!("{} (raw magnitude exceeds u64)", fill.token_amount_ui()),
                };
                println!("\n=== MANUAL SELL CONFIRMED (untracked) ===");
                println!("  Signature: {}", signature);
                println!("  Raw sold: {}", raw_sold_display);
                println!("  UI sold: {:.6}", ui_sold);
                println!("  Net SOL (wallet delta): {:+.6} SOL", net_sol);
                println!("  Effective price: {:.10} SOL/token", effective_price);
                // I4: expected quote price, actual fill price, quote-to-fill drift.
                match expected_quote_price {
                    Some(p) => println!("  Expected quote price: {:.10} SOL/token", p),
                    None => println!("  Expected quote price: unavailable"),
                }
                println!("  Actual fill price: {:.10} SOL/token", effective_price);
                match expected_quote_price.and_then(|p| manual_sell_drift_pct(p, effective_price)) {
                    Some(d) => println!("  Quote-to-fill drift: {:+.4}% (positive = worse)", d),
                    None => println!("  Quote-to-fill drift: unavailable"),
                }
                println!(
                    "Realized P&L unavailable: token was not tracked with a canonical cost basis."
                );

                // Probe the EXACT wallet's current balance. If proven zero, the
                // bought_mints cache entry may be removed; if nonzero, leave it.
                // Pending may still be removed after a confirmed fill even if this
                // probe fails (the cache is noncanonical metadata).
                match ownership_probe.probe(execution_wallet, token_pubkey).await {
                    Ok(state) if state.raw_amount == 0 => {
                        let _ =
                            remove_bought_mint(&bought_mints, &bought_mints_path, token).await;
                        info!("Untracked sell: proven zero balance, removed {} from bought_mints", token);
                    }
                    Ok(state) => {
                        info!(
                            "Untracked sell: {} raw remains in wallet {} - leaving bought_mints cache",
                            state.raw_amount, execution_wallet
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Untracked sell: post-fill balance probe failed: {} - leaving bought_mints cache",
                            e
                        );
                    }
                }

                // Remove pending LAST (confirmed fill is durable regardless of cache).
                pending_store.remove(&signature).await?;
            }
        }
    }

    Ok(())
}

/// The Local-API PumpPortal trader used for manual local submission. Kept as a
/// tiny constructor so the manual-sell route reads clearly; it holds no key
/// material (the exact signer keypair is passed per call to `sell_local`).
fn pumpportal_local_trader() -> PumpPortalTrader {
    PumpPortalTrader::local()
}

/// Show current positions and P&L
pub async fn status(config: &Config) -> Result<()> {
    info!("Loading positions...");

    // TODO: Load positions from persistence
    // TODO: Fetch current prices
    // TODO: Calculate P&L

    println!("\n=== SNIPER BOT STATUS ===\n");

    // Placeholder output
    println!("Positions: 0");
    println!("Total Value: 0.00 SOL");
    println!("Total P&L: 0.00 SOL (0.00%)");
    println!("\nDaily Stats:");
    println!("  Trades: 0");
    println!("  Wins: 0");
    println!("  Losses: 0");
    println!(
        "  Daily Loss Used: 0.00 / {} SOL",
        config.safety.daily_loss_limit_sol
    );

    println!("\n=== OPEN POSITIONS ===\n");
    println!("No open positions.");

    Ok(())
}

/// Show current configuration (secrets masked)
pub fn show_config(config: &Config) -> Result<()> {
    println!("{}", config.masked_display());
    Ok(())
}

/// Check system health
pub async fn health(config: &Config) -> Result<()> {
    println!("\n=== SYSTEM HEALTH CHECK ===\n");

    let mut all_healthy = true;

    // Check RPC
    print!("RPC Endpoint... ");
    match check_rpc(config).await {
        Ok(latency) => println!("OK ({}ms)", latency),
        Err(e) => {
            println!("FAILED: {}", e);
            all_healthy = false;
        }
    }

    // Check PumpPortal (if enabled)
    if config.pumpportal.enabled {
        // E6.2: validate the base URL + configured api-key placement using the
        // stream helper. The returned URL may embed the secret, so it is NEVER
        // printed or logged.
        print!("PumpPortal URL config... ");
        match crate::stream::pumpportal::build_connection_url(
            &config.pumpportal.ws_url,
            &config.pumpportal.api_key,
        ) {
            Ok(_authenticated_url) => {
                // Do NOT print _authenticated_url; it may contain the api-key.
                println!("OK (base URL valid, api-key placement checked)");
            }
            Err(e) => {
                // build_connection_url errors are built from the sanitized base
                // only and never contain the secret.
                println!("FAILED: {}", e);
                all_healthy = false;
            }
        }

        // E6.3/E6.4 + C7/BLOCKER E: connection-only live socket check gated by an
        // explicit tri-state runtime-lock policy. An inspect Err (lock present but
        // malformed/unreadable) FAILS CLOSED: skip the socket AND mark unhealthy —
        // it is NEVER treated as "no runtime". We never open a second socket while
        // a runtime (known or unknown) may own the single PumpPortal connection,
        // and health never deletes the lock.
        let inspect_result = RuntimeLease::inspect(&config.wallet.credentials_dir);
        let lock_policy = health_lock_policy(&inspect_result);
        let api_key_present = !config.pumpportal.api_key.trim().is_empty();
        print!("PumpPortal live socket... ");
        match lock_policy {
            HealthLockPolicy::SkipActive => {
                println!(
                    "SKIPPED live socket check: active runtime owns the single PumpPortal connection"
                );
            }
            HealthLockPolicy::SkipUnhealthy => {
                // Runtime lock exists but is unknown/malformed. Fail closed: do not
                // open a socket, do not delete the lock, and mark health unhealthy.
                println!(
                    "SKIPPED live socket check: runtime lock exists but is unknown/unreadable and must be inspected (no socket opened, lock not modified)"
                );
                all_healthy = false;
            }
            HealthLockPolicy::AllowSocket => {
                if !health_should_open_socket(false, api_key_present) {
                    // No key: the free new-token/migration endpoint could still be
                    // reached, but a connection-only check with no key proves nothing
                    // about trade credentials, so we skip opening a socket here.
                    println!("SKIPPED (no api-key configured)");
                } else {
                    match check_pumpportal(config).await {
                        Ok(_) => println!("endpoint reachable"),
                        Err(e) => {
                            println!("FAILED: {}", e);
                            all_healthy = false;
                        }
                    }
                }
            }
        }

        // AUDIT-004: the authoritative Data-capability line lives HERE, in the
        // PumpPortal-ENABLED section, because Data/event streaming is independent of
        // the trade-EXECUTION route. `pumpportal.enabled=true` +
        // `use_for_trading=false` means PumpPortal is the DATA provider while Jito
        // executes, so this line must still print. It is gated only by
        // `health_should_report_pumpportal_data(config.pumpportal.enabled)` (i.e. by
        // being inside this block). `api_key_present` is already computed above.
        println!(
            "PumpPortal Data API... {}",
            health_data_capability_line(api_key_present)
        );
    } else {
        println!("PumpPortal... DISABLED");
    }

    // Check ShredStream (if PumpPortal disabled and shredstream feature enabled)
    #[cfg(feature = "shredstream")]
    if !config.pumpportal.enabled {
        print!("ShredStream... ");
        match check_shredstream(config).await {
            Ok(_) => println!("OK"),
            Err(e) => {
                println!("FAILED: {}", e);
                all_healthy = false;
            }
        }
    }

    #[cfg(not(feature = "shredstream"))]
    if !config.pumpportal.enabled {
        println!("ShredStream... DISABLED (feature not compiled)");
    }

    // Check Jito (if not using PumpPortal for trading)
    if !config.pumpportal.use_for_trading {
        print!("Jito Block Engine... ");
        match check_jito(config).await {
            Ok(latency) => println!("OK ({}ms)", latency),
            Err(e) => {
                println!("FAILED: {}", e);
                all_healthy = false;
            }
        }
    } else {
        println!("Jito... SKIPPED (using PumpPortal for trading)");
    }

    // Check PumpPortal API (if using for trading)
    //
    // BLOCKER B: report the Data-API capability and the trade-EXECUTION route
    // SEPARATELY, mirroring start()'s real routing
    // (`use_local_api = api_key.is_empty() || force_local_api`). API-key presence
    // authenticates the Data API but does NOT by itself imply Lightning execution:
    // a configured key with `force_local_api=true` still executes LOCAL. This block
    // is report-only and does not affect start()'s actual route/wallet selection.
    if config.pumpportal.use_for_trading {
        let api_key_present = !config.pumpportal.api_key.trim().is_empty();
        let force_local_api = config.pumpportal.force_local_api;
        let mode = health_execution_mode(api_key_present, force_local_api);

        print!("PumpPortal Trading API... ");
        println!(
            "{}",
            health_execution_line(mode, api_key_present, force_local_api)
        );
        // AUDIT-004: this block reports ONLY the transaction execution route. The
        // Data-capability line is printed once in the `pumpportal.enabled` section
        // above, since Data capability is independent of the execution route.
    }

    // Check keypair
    print!("Keypair... ");
    match check_keypair().await {
        Ok(balance) => println!("OK (balance: {} SOL)", balance),
        Err(e) => {
            println!("FAILED: {}", e);
            all_healthy = false;
        }
    }

    println!();
    if all_healthy {
        println!("All systems healthy!");
    } else {
        println!("Some systems are unhealthy. Check the errors above.");
    }

    Ok(())
}

async fn check_rpc(config: &Config) -> Result<u64> {
    use std::time::Instant;

    let client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    let start = Instant::now();
    client.get_slot()?;
    let latency = start.elapsed().as_millis() as u64;

    Ok(latency)
}

#[cfg(feature = "shredstream")]
async fn check_shredstream(_config: &Config) -> Result<()> {
    // TODO: Implement ShredStream health check
    // For now, just return OK
    Ok(())
}

/// Connection-only PumpPortal health check.
///
/// Opens ONE socket using the authenticated URL (built internally via the stream
/// helper so the api-key is placed correctly), then immediately closes it. The
/// authenticated URL is NEVER logged or printed. This proves only that the
/// endpoint is reachable — it does NOT authorize or consume any (metered) trade
/// subscription. Callers must gate this behind `health_should_open_socket`.
async fn check_pumpportal(config: &Config) -> Result<()> {
    use std::time::Duration;
    use tokio_tungstenite::connect_async;

    // Build the authenticated URL internally. Never log/print the returned Url.
    let url = crate::stream::pumpportal::build_connection_url(
        &config.pumpportal.ws_url,
        &config.pumpportal.api_key,
    )
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Try to connect with timeout. On any error, surface only the sanitized base
    // (never the authenticated Url, which carries the secret).
    let connect_future = connect_async(url);
    let timeout = Duration::from_secs(5);

    match tokio::time::timeout(timeout, connect_future).await {
        Ok(Ok((ws, _))) => {
            // Successfully connected, close immediately by dropping. Do NOT send
            // any subscribe message: health must not consume metered trade events.
            drop(ws);
            Ok(())
        }
        // C8 / section 3: NEVER surface the raw tungstenite/connect error string
        // on connection failure — it may contain the authenticated request URL or
        // api-key. Use a fixed sanitized message. The underlying error `e` is
        // intentionally dropped (not logged) here.
        Ok(Err(_e)) => Err(anyhow::anyhow!(
            "PumpPortal WebSocket connection failed for configured base endpoint"
        )),
        Err(_) => Err(anyhow::anyhow!(
            "Connection timed out after {}s",
            timeout.as_secs()
        )),
    }
}

/// E6 pure policy helper: whether a connection-only health check may open a live
/// PumpPortal socket. When another runtime is active it owns the single
/// PumpPortal connection, so health must NEVER open a second socket. A
/// connection-only check with no api-key proves nothing about trade credentials,
/// so we also skip when no key is present.
fn health_should_open_socket(active_runtime: bool, api_key_present: bool) -> bool {
    if active_runtime {
        return false;
    }
    api_key_present
}

/// C7 / BLOCKER E: tri-state runtime-lock policy for the health live-socket check.
///
/// `RuntimeLease::inspect()` returns:
///   - `Ok(None)`      => lock is KNOWN ABSENT; a live endpoint socket check MAY be
///                        considered (still subject to api-key presence).
///   - `Ok(Some(_))`   => an active runtime owns the single connection; SKIP the
///                        live socket check (its runtime owns the socket).
///   - `Err(_)`        => the lock EXISTS but is malformed/unreadable, i.e. UNKNOWN.
///                        This must FAIL CLOSED: skip the live socket check AND mark
///                        health unhealthy. An inspect `Err` is NEVER interpreted as
///                        "no runtime". Never open a socket, never delete the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthLockPolicy {
    /// Lock known absent: the live socket check may proceed (key permitting).
    AllowSocket,
    /// Active runtime owns the connection: skip the live socket check.
    SkipActive,
    /// Lock present but unknown/malformed: skip the check and mark unhealthy.
    SkipUnhealthy,
}

/// Pure classifier mapping a `RuntimeLease::inspect` result to a health policy (C7).
fn health_lock_policy<T, E>(inspect_result: &std::result::Result<Option<T>, E>) -> HealthLockPolicy {
    match inspect_result {
        Ok(None) => HealthLockPolicy::AllowSocket,
        Ok(Some(_)) => HealthLockPolicy::SkipActive,
        Err(_) => HealthLockPolicy::SkipUnhealthy,
    }
}

/// BLOCKER B: the trade-EXECUTION route reported by `health()`.
///
/// This is a REPORT-ONLY mirror of the route `start()` actually selects. It must
/// never influence `start()`'s real routing/wallet selection — it only exists so
/// the health report tells the operator the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthExecutionMode {
    /// LOCAL transaction API + local signing wallet (0.5% provider fee).
    Local,
    /// PumpPortal Lightning transaction API (1% provider fee).
    Lightning,
}

/// BLOCKER B pure helper: compute the trade-execution route EXACTLY as `start()`
/// does. `start()` uses:
///
/// ```text
/// let use_local_api = api_key.is_empty() || force_local_api;
/// ```
///
/// so the execution mode is the boolean COMPLEMENT of `use_local_api`:
///   - api_key absent                        => use_local_api=true  => Local
///   - api_key present && force_local_api     => use_local_api=true  => Local
///   - api_key present && !force_local_api    => use_local_api=false => Lightning
///
/// Crucially, api-key PRESENCE alone never implies Lightning: after RET-001 the
/// key authenticates the Data API independently of the trade route, so a key with
/// `force_local_api=true` still executes LOCAL. This is a pure function of two
/// booleans and never touches or derives the secret value.
fn health_execution_mode(api_key_present: bool, force_local_api: bool) -> HealthExecutionMode {
    // Mirror of start()'s `use_local_api = api_key.is_empty() || force_local_api`.
    let use_local_api = !api_key_present || force_local_api;
    if use_local_api {
        HealthExecutionMode::Local
    } else {
        HealthExecutionMode::Lightning
    }
}

/// BLOCKER B pure formatter for the health trade-EXECUTION line. Driven by the
/// computed `HealthExecutionMode` PLUS `api_key_present` (never the secret). The
/// AUDIT-003 fix: the Local branch must know key presence so it never claims a
/// "Data API credential configured" when no key exists. `force_local_api` alone
/// is NOT sufficient to justify that claim — a Local route reached because the key
/// is absent (`force_local_api` may still be set in a default config) must print
/// the no-key text.
fn health_execution_line(
    mode: HealthExecutionMode,
    api_key_present: bool,
    force_local_api: bool,
) -> &'static str {
    match mode {
        HealthExecutionMode::Local => {
            // Only claim a configured Data credential when a key actually exists.
            // The credential claim is gated on `api_key_present`, not on
            // `force_local_api`, so the default empty-key case can never lie.
            if api_key_present && force_local_api {
                "Execution: LOCAL MODE (force_local_api; Data API credential configured; Local Transaction API 0.5%)"
            } else {
                "Execution: LOCAL MODE (no API key; Local Transaction API 0.5%)"
            }
        }
        HealthExecutionMode::Lightning => {
            "Execution: LIGHTNING MODE (Lightning Transaction API 1%)"
        }
    }
}

/// BLOCKER B pure formatter for the SEPARATE Data-capability line reported by
/// `health()`. Independent of the execution route: it reflects only whether an
/// api-key is configured to authenticate the Data API. Never contains the secret.
fn health_data_capability_line(api_key_present: bool) -> &'static str {
    if api_key_present {
        "Data: authenticated/metered token+account trade streams available"
    } else {
        "Data: new-token/migration only; trade subscriptions unavailable"
    }
}

/// AUDIT-004 pure predicate: whether `health()` should report the PumpPortal
/// Data-API capability line. Data/event streaming is independent of the trade
/// EXECUTION route, so this depends ONLY on `pumpportal.enabled` (the data/event
/// provider switch), never on `use_for_trading` (the execution route). Display
/// only; changes no execution/route/subscription/readiness behavior.
fn health_should_report_pumpportal_data(pumpportal_enabled: bool) -> bool {
    pumpportal_enabled
}

async fn check_jito(_config: &Config) -> Result<u64> {
    // TODO: Implement Jito health check
    // For now, just return placeholder latency
    Ok(50)
}

async fn check_keypair() -> Result<f64> {
    // TODO: Implement keypair check
    // Load keypair, check balance
    Ok(0.0)
}

// =============================================================================
// Wallet Management Commands
// =============================================================================

/// Show wallet status (all wallets, balances)
pub async fn wallet_status(config: &Config) -> Result<()> {
    use crate::wallet::credentials::CredentialManager;
    use std::path::Path;

    println!("\n=== WALLET STATUS ===\n");

    let creds_path = Path::new(&config.wallet.credentials_dir);
    let mut creds = CredentialManager::load(creds_path)
        .map_err(|e| anyhow::anyhow!("Failed to load credentials: {}", e))?;

    let rpc_client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    // Collect wallet data into owned structures to avoid borrow conflicts
    let wallets: Vec<_> = creds.list_wallets().into_iter().cloned().collect();

    for wallet in wallets {
        print!("{} ({}): ", wallet.alias, wallet.name);

        // Get address
        let address = match creds.get_address(&wallet.name) {
            Ok(addr) => addr.to_string(),
            Err(_) => wallet.address.clone(),
        };

        // Get balance for non-auth wallets
        if wallet.wallet_type != crate::wallet::WalletType::Auth {
            if let Ok(addr) = address.parse::<solana_sdk::pubkey::Pubkey>() {
                match rpc_client.get_balance(&addr) {
                    Ok(lamports) => {
                        let sol = lamports as f64 / 1_000_000_000.0;
                        println!("{:.4} SOL", sol);
                    }
                    Err(e) => println!("(balance fetch failed: {})", e),
                }
            } else {
                println!("(invalid address)");
            }
        } else {
            println!("(auth only)");
        }

        println!("  Type: {:?}", wallet.wallet_type);
        println!("  Address: {}", address);
        if !wallet.notes.is_empty() {
            println!("  Notes: {}", wallet.notes);
        }
        println!();
    }

    // Show safety limits
    println!("=== SAFETY LIMITS ===\n");
    println!(
        "Min hot balance: {} SOL",
        config.wallet.safety.min_hot_balance_sol
    );
    println!(
        "Max single transfer: {} SOL",
        config.wallet.safety.max_single_transfer_sol
    );
    println!(
        "Max daily extraction: {} SOL",
        config.wallet.safety.max_daily_extraction_sol
    );
    println!(
        "AI max auto-transfer: {} SOL",
        config.wallet.safety.ai_max_auto_transfer_sol
    );
    println!(
        "Vault address locked: {}",
        config.wallet.safety.vault_address_locked
    );

    Ok(())
}

/// List all configured wallets
pub async fn wallet_list(config: &Config) -> Result<()> {
    use crate::wallet::credentials::CredentialManager;
    use std::path::Path;

    let creds_path = Path::new(&config.wallet.credentials_dir);
    let creds = CredentialManager::load(creds_path)
        .map_err(|e| anyhow::anyhow!("Failed to load credentials: {}", e))?;

    println!("\n=== CONFIGURED WALLETS ===\n");
    println!(
        "{:<20} {:<15} {:<15} {}",
        "NAME", "ALIAS", "TYPE", "ADDRESS"
    );
    println!("{}", "-".repeat(80));

    for wallet in creds.list_wallets() {
        let addr_display = if wallet.address.len() > 20 {
            format!("{}...", &wallet.address[..20])
        } else {
            wallet.address.clone()
        };

        println!(
            "{:<20} {:<15} {:<15} {}",
            wallet.name,
            wallet.alias,
            format!("{:?}", wallet.wallet_type),
            addr_display
        );
    }

    println!();
    Ok(())
}

/// Add a new wallet
pub async fn wallet_add(
    config: &Config,
    name: &str,
    alias: &str,
    wallet_type: &str,
    address: Option<String>,
    generate: bool,
) -> Result<()> {
    use crate::wallet::credentials::CredentialManager;
    use crate::wallet::types::{WalletEntry, WalletType};
    use chrono::Utc;
    use std::path::Path;

    // E3 / INV-RUN-001/002: exclude a running trading process before mutating the
    // credential registry/files. Held for the whole function.
    let _runtime_lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "wallet_add")
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let creds_path = Path::new(&config.wallet.credentials_dir);
    let mut creds = CredentialManager::load(creds_path)
        .map_err(|e| anyhow::anyhow!("Failed to load credentials: {}", e))?;

    // Validate name
    if name.contains(' ') || name.chars().any(|c| c.is_uppercase()) {
        anyhow::bail!("Wallet name must be lowercase with no spaces");
    }

    // Parse wallet type
    let wtype = match wallet_type.to_lowercase().as_str() {
        "hot" => WalletType::Hot,
        "vault" => WalletType::Vault,
        "external" => WalletType::External,
        "auth" => WalletType::Auth,
        _ => anyhow::bail!(
            "Invalid wallet type: {}. Use: hot, vault, external, auth",
            wallet_type
        ),
    };

    // Validate requirements
    if wtype == WalletType::External && address.is_none() {
        anyhow::bail!("External wallets require --address");
    }

    if (wtype == WalletType::Hot || wtype == WalletType::Vault) && !generate && address.is_none() {
        anyhow::bail!("Hot/vault wallets require --generate or --address");
    }

    let (keypair_path, final_address) = if generate {
        use solana_sdk::signer::Signer;

        // Generate new keypair
        let wallet_dir = creds_path.join(name);
        std::fs::create_dir_all(&wallet_dir)?;

        let keypair_file = wallet_dir.join("keypair.json");

        // Generate keypair
        let keypair = solana_sdk::signature::Keypair::new();
        let keypair_bytes: Vec<u8> = keypair.to_bytes().to_vec();

        // Save keypair
        let keypair_json = serde_json::to_string(&keypair_bytes)?;
        std::fs::write(&keypair_file, keypair_json)?;

        // Set permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&keypair_file)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&keypair_file, perms)?;
        }

        let address_str = keypair.pubkey().to_string();
        info!("Generated new keypair: {}", address_str);

        (
            Some(std::path::PathBuf::from(format!(
                "credentials/{}/keypair.json",
                name
            ))),
            "AUTO_DERIVED".to_string(),
        )
    } else {
        (None, address.unwrap_or_else(|| "AUTO_DERIVED".to_string()))
    };

    let entry = WalletEntry {
        name: name.to_string(),
        alias: alias.to_string(),
        wallet_type: wtype,
        keypair_path,
        address: final_address.clone(),
        created_at: Utc::now(),
        notes: String::new(),
    };

    creds
        .add_wallet(entry)
        .map_err(|e| anyhow::anyhow!("Failed to add wallet: {}", e))?;

    println!("Wallet '{}' added successfully!", name);
    if final_address != "AUTO_DERIVED" {
        println!("Address: {}", final_address);
    }

    Ok(())
}

/// Extract SOL to vault
pub async fn wallet_extract(
    config: &Config,
    amount: f64,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    use crate::wallet::manager::{WalletManager, WalletManagerConfig};
    use crate::wallet::safety::WalletSafetyConfig;
    use crate::wallet::types::{InitiatedBy, TransferReason};
    use dialoguer::Confirm;

    // E3 / INV-RUN-001/002: exclude a running trading process before any
    // controlled-wallet balance move. Held for the whole function.
    let _runtime_lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "wallet_extract")
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    info!("Extracting {} SOL to vault", amount);

    if dry_run {
        println!("\n=== DRY RUN ===");
        println!("Would extract {} SOL to vault", amount);
        println!("Vault: {}", config.wallet.vault_wallet);
        return Ok(());
    }

    // Build wallet manager config
    let wallet_config = WalletManagerConfig {
        hot_wallet_name: config.wallet.hot_wallet.clone(),
        vault_wallet_name: config.wallet.vault_wallet.clone(),
        credentials_dir: config.wallet.credentials_dir.clone(),
        safety: WalletSafetyConfig {
            min_hot_balance_sol: config.wallet.safety.min_hot_balance_sol,
            max_single_transfer_sol: config.wallet.safety.max_single_transfer_sol,
            max_daily_extraction_sol: config.wallet.safety.max_daily_extraction_sol,
            confirm_above_sol: config.wallet.safety.confirm_above_sol,
            emergency_threshold_sol: config.wallet.safety.emergency_threshold_sol,
            vault_address_locked: config.wallet.safety.vault_address_locked,
            ai_max_auto_transfer_sol: config.wallet.safety.ai_max_auto_transfer_sol,
        },
    };

    let rpc_client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    let wallet_manager = WalletManager::new(wallet_config, rpc_client)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create wallet manager: {}", e))?;

    // Confirmation prompt
    if !force && amount > config.wallet.safety.confirm_above_sol {
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Extract {} SOL to vault? This cannot be undone.",
                amount
            ))
            .default(false)
            .interact()?;

        if !confirmed {
            info!("Extraction cancelled by user");
            return Ok(());
        }
    }

    // Execute extraction
    match wallet_manager
        .extract_to_vault(
            amount,
            TransferReason::ManualTransfer,
            InitiatedBy::User,
            force,
        )
        .await
    {
        Ok(record) => {
            println!("\n=== EXTRACTION SUCCESSFUL ===");
            println!("Amount: {} SOL", record.amount_sol);
            println!("To: {}", record.to_wallet);
            println!("Signature: {}", record.signature);
            println!(
                "View on Solscan: https://solscan.io/tx/{}",
                record.signature
            );
        }
        Err(e) => {
            error!("Extraction failed: {}", e);
            anyhow::bail!("Extraction failed: {}", e);
        }
    }

    Ok(())
}

/// View transfer history
pub async fn wallet_history(config: &Config, limit: usize) -> Result<()> {
    println!("\n=== TRANSFER HISTORY ===\n");

    // Load history from file if it exists
    let history_path = format!("{}/transfer_history.json", config.wallet.credentials_dir);

    if let Ok(content) = std::fs::read_to_string(&history_path) {
        let history: crate::wallet::types::TransferHistory =
            serde_json::from_str(&content).unwrap_or_default();

        if history.transfers.is_empty() {
            println!("No transfer history found.");
        } else {
            println!(
                "{:<12} {:<10} {:<15} {:<15} {:<10}",
                "DATE", "AMOUNT", "FROM", "TO", "REASON"
            );
            println!("{}", "-".repeat(65));

            for record in history.transfers.iter().take(limit) {
                println!(
                    "{:<12} {:<10.4} {:<15} {:<15} {:<10}",
                    record.timestamp.format("%Y-%m-%d"),
                    record.amount_sol,
                    if record.from_wallet.len() > 12 {
                        format!("{}...", &record.from_wallet[..12])
                    } else {
                        record.from_wallet.clone()
                    },
                    if record.to_wallet.len() > 12 {
                        format!("{}...", &record.to_wallet[..12])
                    } else {
                        record.to_wallet.clone()
                    },
                    format!("{}", record.reason)
                );
            }
        }
    } else {
        println!("No transfer history found.");
    }

    println!();
    Ok(())
}

/// View/manage AI proposals
pub async fn wallet_proposals(
    _config: &Config,
    approve: Option<String>,
    reject: Option<String>,
) -> Result<()> {
    println!("\n=== AI PROPOSALS ===\n");

    // TODO: Implement proposal management
    // This requires integration with the running bot instance

    if let Some(id) = approve {
        println!("Approving proposal: {}", id);
        println!("(Not yet implemented - requires running bot instance)");
    } else if let Some(id) = reject {
        println!("Rejecting proposal: {}", id);
        println!("(Not yet implemented - requires running bot instance)");
    } else {
        println!("No pending proposals.");
        println!("\nTo approve a proposal: snipe wallet proposals --approve <ID>");
        println!("To reject a proposal: snipe wallet proposals --reject <ID>");
    }

    Ok(())
}

/// Emergency actions
pub async fn wallet_emergency(config: &Config, shutdown: bool, resume: bool) -> Result<()> {
    if shutdown {
        warn!("=== EMERGENCY SHUTDOWN ===");
        warn!("Activating emergency lock - all trading operations will be paused");

        // TODO: Signal running bot instance to shutdown
        // For now, just create a lock file
        let lock_file = format!("{}/emergency.lock", config.wallet.credentials_dir);
        std::fs::write(&lock_file, chrono::Utc::now().to_rfc3339())?;

        println!("\nEmergency lock activated!");
        println!("Lock file created: {}", lock_file);
        println!("\nTo resume operations: snipe wallet emergency --resume");
    } else if resume {
        info!("=== RESUMING OPERATIONS ===");

        let lock_file = format!("{}/emergency.lock", config.wallet.credentials_dir);
        if std::path::Path::new(&lock_file).exists() {
            std::fs::remove_file(&lock_file)?;
            println!("Emergency lock deactivated!");
            println!("Operations may now resume.");
        } else {
            println!("No emergency lock found - operations are not locked.");
        }
    } else {
        // Check status
        let lock_file = format!("{}/emergency.lock", config.wallet.credentials_dir);
        if std::path::Path::new(&lock_file).exists() {
            let lock_time = std::fs::read_to_string(&lock_file)?;
            println!("EMERGENCY LOCK ACTIVE since {}", lock_time);
            println!("\nTo resume: snipe wallet emergency --resume");
        } else {
            println!("No emergency lock active - operations are normal.");
            println!("\nTo activate emergency lock: snipe wallet emergency --shutdown");
        }
    }

    Ok(())
}

/// Transfer SOL between wallets
pub async fn wallet_transfer(
    config: &Config,
    from: &str,
    to: &str,
    amount: f64,
    force: bool,
) -> Result<()> {
    use solana_sdk::signature::Signer;
    use std::str::FromStr;

    // E3 / INV-RUN-001/002: exclude a running trading process before any
    // controlled-wallet balance move. Held for the whole function.
    let _runtime_lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "wallet_transfer")
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    info!(
        "Initiating transfer of {} SOL from {} to {}",
        amount, from, to
    );

    // Load source wallet
    let from_path = format!("{}/{}/keypair.json", config.wallet.credentials_dir, from);
    if !std::path::Path::new(&from_path).exists() {
        anyhow::bail!("Source wallet '{}' not found", from);
    }

    let from_data = std::fs::read_to_string(&from_path)?;
    let from_secret: Vec<u8> = serde_json::from_str(&from_data)?;
    let from_keypair = solana_sdk::signature::Keypair::from_bytes(&from_secret)?;

    // Determine destination address
    let to_pubkey = if to.len() >= 32 && to.len() <= 44 {
        // Looks like a base58 address
        solana_sdk::pubkey::Pubkey::from_str(to)?
    } else {
        // It's a wallet name - load the pubkey
        let to_path = format!("{}/{}/keypair.json", config.wallet.credentials_dir, to);
        if !std::path::Path::new(&to_path).exists() {
            anyhow::bail!("Destination wallet '{}' not found", to);
        }
        let to_data = std::fs::read_to_string(&to_path)?;
        let to_secret: Vec<u8> = serde_json::from_str(&to_data)?;
        let to_keypair = solana_sdk::signature::Keypair::from_bytes(&to_secret)?;
        to_keypair.pubkey()
    };

    // Check safety limits
    if amount > config.wallet.safety.max_single_transfer_sol {
        anyhow::bail!(
            "Transfer amount {} SOL exceeds max_single_transfer_sol limit of {} SOL",
            amount,
            config.wallet.safety.max_single_transfer_sol
        );
    }

    // Confirmation
    if !force && amount > config.wallet.safety.confirm_above_sol {
        use dialoguer::Confirm;
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Transfer {} SOL from {} to {}?",
                amount, from, to_pubkey
            ))
            .interact()?;

        if !confirmed {
            println!("Transfer cancelled.");
            return Ok(());
        }
    }

    // Execute transfer
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    let lamports = (amount * 1e9) as u64;
    let balance = rpc_client.get_balance(&from_keypair.pubkey())?;

    if balance < lamports + 5000 {
        anyhow::bail!(
            "Insufficient balance: have {} SOL, need {} SOL + fees",
            balance as f64 / 1e9,
            amount
        );
    }

    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[solana_sdk::system_instruction::transfer(
            &from_keypair.pubkey(),
            &to_pubkey,
            lamports,
        )],
        Some(&from_keypair.pubkey()),
        &[&from_keypair],
        recent_blockhash,
    );

    let sig = rpc_client.send_and_confirm_transaction(&tx)?;
    info!("Transfer successful: {}", sig);
    println!("Transferred {} SOL from {} to {}", amount, from, to_pubkey);
    println!("Signature: {}", sig);

    Ok(())
}

/// Scan existing tokens for opportunities
pub async fn scan(
    _config: &Config,
    min_liquidity: f64,
    max_liquidity: f64,
    min_volume: f64,
    limit: usize,
    auto_buy: bool,
    _buy_amount: f64,
    format: &str,
    watch: bool,
    interval: u64,
) -> Result<()> {
    use crate::dexscreener::{DexScreenerClient, HotScanConfig};

    info!(
        "Starting token scan (liquidity: {}-{} SOL, volume >= {} SOL)",
        min_liquidity, max_liquidity, min_volume
    );

    let client = DexScreenerClient::new();
    let scan_config = HotScanConfig {
        min_liquidity_usd: min_liquidity * 150.0, // Rough SOL to USD conversion
        max_market_cap: max_liquidity * 150.0 * 100.0, // Max liquidity implies max mcap
        ..Default::default()
    };

    loop {
        let tokens = client.scan_hot_tokens(&scan_config).await?;
        let tokens: Vec<_> = tokens.into_iter().take(limit).collect();

        if format == "json" {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &tokens
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "mint": t.mint,
                                "symbol": t.symbol,
                                "name": t.name,
                                "m5_change": t.m5_change,
                                "liquidity_usd": t.liquidity_usd,
                                "market_cap": t.market_cap,
                                "score": t.score()
                            })
                        })
                        .collect::<Vec<_>>()
                )?
            );
        } else {
            println!("\n{:=<80}", "");
            println!("Found {} tokens matching criteria:", tokens.len());
            println!("{:-<80}", "");

            for (i, token) in tokens.iter().enumerate() {
                println!(
                    "{}. {} ({}) | M5: {:+.1}% | MCap: ${:.0}k | Liq: ${:.0}k | Score: {:.1}",
                    i + 1,
                    token.symbol,
                    &token.mint[..8],
                    token.m5_change,
                    token.market_cap / 1000.0,
                    token.liquidity_usd / 1000.0,
                    token.score()
                );
            }

            if auto_buy && !tokens.is_empty() {
                warn!("AUTO-BUY enabled - this is AGGRESSIVE mode!");
                // TODO: Implement auto-buy logic
                // If scan auto-buy becomes executable, it MUST acquire RuntimeLease before any state/wallet mutation.
            }
        }

        if !watch {
            break;
        }

        info!("Waiting {} seconds until next scan...", interval);
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }

    Ok(())
}

/// Scan DexScreener for hot tokens with momentum
pub async fn hot_scan(
    config: &Config,
    min_m5: f64,
    min_ratio: f64,
    min_liquidity: f64,
    max_mcap: f64,
    auto_buy: bool,
    buy_amount: f64,
    dry_run: bool,
    watch: bool,
    interval: u64,
    jito: bool, // Enable Jito bundles for Local API trades
) -> Result<()> {
    use crate::dexscreener::{DexScreenerClient, HotScanConfig};
    use solana_sdk::signature::Signer;

    // E1 / INV-RUN-001/002: acquire the exclusive runtime lease for this
    // credentials_dir BEFORE PositionManager / pending / wallet initialization.
    // Held for the entire function lifetime. This applies EVEN in dry-run because
    // HotScan startup recovery can mutate persistent state.
    let _runtime_lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "hot_scan")
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    info!("=== HOT TOKEN SCANNER ===");
    info!(
        "Criteria: M5 >= {:.1}%, Ratio >= {:.1}, Liq >= ${:.0}, MCap <= ${:.0}",
        min_m5, min_ratio, min_liquidity, max_mcap
    );

    if auto_buy {
        warn!("AUTO-BUY enabled with {} SOL per trade", buy_amount);
        if dry_run {
            warn!("DRY-RUN mode - no real trades will be executed");
        }
    }

    // Jito bundle support note
    if jito {
        warn!("--jito flag enabled: Jito bundles will be used for Local API trades (requires config.pumpportal.use_local_api = true)");
        warn!("Note: Jito bundles provide MEV protection and faster confirmation, but require tip configuration in config.toml");
    }

    // Load wallets - support multi-wallet if configured
    let multi_wallet = if !config.wallet.trading_wallets.is_empty() {
        match crate::wallet::MultiWalletManager::new(
            config.wallet.trading_wallets.clone(),
            &config.wallet.selection_strategy,
        ) {
            Ok(mw) => {
                info!("Multi-wallet mode ENABLED with {} wallets", mw.wallet_count());
                Some(std::sync::Arc::new(mw))
            }
            Err(e) => {
                warn!("Failed to initialize multi-wallet: {} - falling back to single wallet", e);
                None
            }
        }
    } else {
        None
    };

    // Fall back to single keypair if multi-wallet not configured
    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| format!("{}/hot-trading/keypair.json", config.wallet.credentials_dir));
    let keypair_data = std::fs::read_to_string(&keypair_path)?;
    let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
    let keypair = std::sync::Arc::new(solana_sdk::signature::Keypair::from_bytes(&secret_key)?);

    if multi_wallet.is_some() {
        info!("Primary wallet (fallback): {}", keypair.pubkey());
    } else {
        info!("Signing wallet: {}", keypair.pubkey());
    }

    // Initialize RPC
    let rpc_client = std::sync::Arc::new(solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    ));

    // MPT-001 Agent G: authoritative market oracle for the HotScan auto-BUY gate.
    // hot_scan is a standalone entry point (not start()), so the oracle is
    // constructed here from the HotScan RPC client and used directly in the buy
    // loop (which runs in this function body, not a spawned task). DexScreener
    // stays discovery/display only; no DexScreener price may substitute for a
    // fresh executable quote at submit time (G1/G2).
    let market_oracle = Arc::new(PumpMarketOracle::new(rpc_client.clone()));

    // Initialize trader - Force Local API if configured (0.5% fee vs 1% for Lightning)
    let use_local_api = config.pumpportal.api_key.is_empty() || config.pumpportal.force_local_api;
    let trader = if config.pumpportal.use_for_trading {
        if use_local_api {
            if config.pumpportal.force_local_api {
                info!("Force Local API enabled - using Local API (0.5% fee)");
            } else {
                info!("Using Local API (sign + send locally)");
            }
            info!("Trading wallet: {}", keypair.pubkey());
            Some(std::sync::Arc::new(
                crate::trading::pumpportal_api::PumpPortalTrader::local(),
            ))
        } else {
            info!("Using Lightning API (1% fee) - consider force_local_api=true to save 0.5%");
            if !config.pumpportal.lightning_wallet.is_empty() {
                info!(
                    "Lightning wallet (for trading & balance): {}",
                    config.pumpportal.lightning_wallet
                );
            }
            Some(std::sync::Arc::new(
                crate::trading::pumpportal_api::PumpPortalTrader::lightning(
                    config.pumpportal.api_key.clone(),
                ),
            ))
        }
    } else {
        None
    };

    // Initialize position manager for tracking
    let position_manager = std::sync::Arc::new(crate::position::manager::PositionManager::new(
        config.safety.clone(),
        Some(format!("{}/positions.json", config.wallet.credentials_dir)),
    ));
    // Fail closed: refuse to start HotScan with unknown ownership state (F1).
    position_manager.load().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to load persisted positions; refusing to start HotScan with unknown ownership state: {}",
            e
        )
    })?;

    // === F1: HotScan canonical persistence + startup recovery ===
    // Replicates the primary start() transaction-truth init inside HotScan so
    // that a HotScan restart recovers in-flight signatures and canonicalizes
    // legacy positions before any new buy. A submitted signature is submission
    // identity, not fill proof (INV-TX-001); the pending journal is the source of
    // truth for unresolved state across restarts.

    // Reviewed default reconciler config (250ms polling / 15s timeout).
    let trade_reconciler = std::sync::Arc::new(TradeReconciler::new(rpc_client.clone()));

    // Build the exact controlled-wallet registry. Local set = primary local
    // keypair + every successfully loaded MultiWalletManager local wallet. Local
    // signing authority and Lightning wallet authority are distinct
    // (INV-WALLET-001); we never fall back between them (INV-WALLET-002).
    let mut hotscan_local_wallets: Vec<Pubkey> = Vec::new();
    if let Some(ref mw) = multi_wallet {
        for w in mw.wallets() {
            hotscan_local_wallets.push(w.pubkey());
        }
    }

    // Strictly parse the configured Lightning wallet, if present. An invalid
    // non-empty Lightning wallet fails closed for recovery/trading startup.
    let hotscan_lightning_wallet: Option<Pubkey> = {
        let lw = config.pumpportal.lightning_wallet.trim();
        if lw.is_empty() {
            None
        } else {
            match Pubkey::from_str(lw) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Configured lightning_wallet is not a valid Pubkey; refusing to start HotScan recovery/trading: {}",
                        e
                    ));
                }
            }
        }
    };

    let hotscan_registry = std::sync::Arc::new(ExecutionWalletRegistry::new(
        keypair.pubkey(),
        &hotscan_local_wallets,
        hotscan_lightning_wallet,
    ));
    let hotscan_probe = WalletOwnershipProbe::new(rpc_client.clone());

    // Shared pending-execution journal at the SAME credentials path as start().
    let hotscan_pending_path =
        format!("{}/pending_executions.json", config.wallet.credentials_dir);
    let pending_executions =
        std::sync::Arc::new(PendingExecutionStore::new(hotscan_pending_path));
    pending_executions.load().await?;
    pending_executions.ensure_writable().await?;

    // Fail-closed halt flag for NEW HotScan entries. Set when unresolved
    // transaction or ownership state remains AFTER recovery. Existing safe exits
    // (the monitor task) continue regardless of this flag.
    let new_entries_halted = std::sync::Arc::new(AtomicBool::new(false));

    // 1. recover the pending journal against confirmed chain state;
    // 2. recover legacy/incomplete positions from chain evidence;
    // 3. halt NEW entries iff any unresolved pending OR recovery-required /
    //    unroutable position remains.
    let hotscan_pending_summary =
        recover_pending_store(&trade_reconciler, &pending_executions, &position_manager).await?;
    info!(
        "HotScan pending recovery: recovered={}, confirmed_failures_removed={}, still_unresolved={}, accounting_conflicts={}",
        hotscan_pending_summary.recovered,
        hotscan_pending_summary.confirmed_failures_removed,
        hotscan_pending_summary.still_unresolved,
        hotscan_pending_summary.accounting_conflicts
    );

    let hotscan_legacy_summary = recover_legacy_positions(
        &trade_reconciler,
        &hotscan_probe,
        hotscan_registry.as_ref(),
        &position_manager,
    )
    .await?;
    info!(
        "HotScan legacy recovery: recovered={}, resolved_zero={}, still_recovery_required={}",
        hotscan_legacy_summary.recovered,
        hotscan_legacy_summary.resolved_zero,
        hotscan_legacy_summary.still_recovery_required
    );

    // Re-inspect remaining positions for any still recovery-required/unroutable.
    let hotscan_post_recovery_positions = position_manager.get_all_positions().await;
    let hotscan_residual_blocked = hotscan_post_recovery_positions
        .iter()
        .filter(|p| legacy_recovery_required(p, hotscan_registry.as_ref()))
        .count();

    if !hotscan_pending_summary.fully_resolved()
        || !hotscan_legacy_summary.fully_resolved()
        || hotscan_residual_blocked > 0
    {
        error!(
            "HotScan NEW entries HALTED after recovery: unresolved_pending={}, legacy_unresolved={}, residual_blocked_positions={}",
            hotscan_pending_summary.still_unresolved,
            hotscan_legacy_summary.still_recovery_required,
            hotscan_residual_blocked
        );
        new_entries_halted.store(true, Ordering::SeqCst);
    } else {
        info!("HotScan startup recovery complete: transaction/position truth restored; new entries may resume.");
    }

    // === F2: explicit route-capable trader handles ===
    // `trader` above remains the active handle (used for pool readiness). Make the
    // route-capable handles explicit so the live buy path (and Agent G's sell
    // path) never conflate Local signing with Lightning submission. No new API.
    // Local trader: available whenever PumpPortal trading is enabled.
    let hotscan_local_trader: Option<std::sync::Arc<crate::trading::pumpportal_api::PumpPortalTrader>> =
        if config.pumpportal.use_for_trading {
            Some(std::sync::Arc::new(
                crate::trading::pumpportal_api::PumpPortalTrader::local(),
            ))
        } else {
            None
        };
    // Lightning trader: available ONLY when an API key is configured.
    let hotscan_lightning_trader: Option<std::sync::Arc<crate::trading::pumpportal_api::PumpPortalTrader>> =
        if config.pumpportal.use_for_trading && !config.pumpportal.api_key.is_empty() {
            Some(std::sync::Arc::new(
                crate::trading::pumpportal_api::PumpPortalTrader::lightning(
                    config.pumpportal.api_key.clone(),
                ),
            ))
        } else {
            None
        };
    // Referenced by the live buy path below and reused by Agent G.
    let _ = (&hotscan_local_trader, &hotscan_lightning_trader);

    // === C1: halt NEW HotScan entries when any canonical position is
    // operationally unexitable with the CURRENT exit credentials/routes. This
    // mirrors the primary start() B6 check but uses the HotScan-scoped route
    // handles/registry. Positions are kept; safe routed exits still proceed.
    // In particular, an existing Lightning position + configured Lightning
    // wallet but MISSING api_key (=> no lightning trader) HALTS new entries.
    // Log mint + public wallet + reason only; no secrets.
    {
        let local_exit_available = hotscan_local_trader.is_some();
        let lightning_exit_available = hotscan_lightning_trader.is_some();
        let mut unexitable = 0usize;
        for position in position_manager.get_all_positions().await {
            if !position_has_operational_exit_route(
                &position,
                hotscan_registry.as_ref(),
                local_exit_available,
                lightning_exit_available,
            ) {
                unexitable += 1;
                let reason = if position.token_decimals.is_none() {
                    "unknown token decimals"
                } else if position.wallet_pubkey.parse::<Pubkey>().is_err() {
                    "invalid recorded wallet"
                } else {
                    "no routable exit trader for wallet's route"
                };
                error!(
                    "HotScan operationally unexitable position: mint {} wallet {} - {}",
                    position.mint, position.wallet_pubkey, reason
                );
            }
        }
        if unexitable > 0 {
            error!(
                "HotScan NEW entries HALTED: {} canonical position(s) cannot be exited with current credentials/routes",
                unexitable
            );
            new_entries_halted.store(true, Ordering::SeqCst);
        }
    }

    // Initialize smart money wallet profiler and Helius client (if enabled)
    let (helius_client, wallet_profiler) = if config.smart_money.enabled {
        use crate::filter::helius::HeliusClient;
        use crate::filter::smart_money::wallet_profiler::{WalletProfiler, WalletProfilerConfig};

        if let Some(helius) = HeliusClient::from_rpc_url(&config.rpc.endpoint) {
            info!("Smart money wallet profiler ENABLED - analyzing creators before buy");
            let helius_arc = std::sync::Arc::new(helius);
            let profiler = std::sync::Arc::new(WalletProfiler::new(
                helius_arc.clone(),
                WalletProfilerConfig::default(),
            ));
            (Some(helius_arc), Some(profiler))
        } else {
            warn!("Smart money enabled but Helius API key not found in RPC URL - profiler disabled");
            (None, None)
        }
    } else {
        info!("Smart money wallet profiler disabled");
        (None, None)
    };

    // Track already-bought mints this session with persistence (mint -> timestamp)
    // TTL: Remove entries older than 24 hours to allow re-buying of rebounding tokens
    const BOUGHT_MINTS_TTL_HOURS: i64 = 24;
    let bought_mints_path = format!("{}/bought_mints.json", config.wallet.credentials_dir);
    let bought_mints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>> = {
        // Load from file if exists and prune stale entries
        let now = chrono::Utc::now().timestamp();
        let ttl_secs = BOUGHT_MINTS_TTL_HOURS * 3600;
        let loaded: std::collections::HashMap<String, i64> =
            if std::path::Path::new(&bought_mints_path).exists() {
                match std::fs::read_to_string(&bought_mints_path) {
                    Ok(data) => {
                        // Try new format (HashMap with timestamps)
                        if let Ok(map) =
                            serde_json::from_str::<std::collections::HashMap<String, i64>>(&data)
                        {
                            let before = map.len();
                            let pruned: std::collections::HashMap<String, i64> = map
                                .into_iter()
                                .filter(|(_, ts)| now - ts < ttl_secs)
                                .collect();
                            let removed = before - pruned.len();
                            if removed > 0 {
                                info!(
                                    "Pruned {} stale entries from bought_mints (TTL: {}h)",
                                    removed, BOUGHT_MINTS_TTL_HOURS
                                );
                            }
                            info!("Loaded {} bought mints from session state", pruned.len());
                            pruned
                        } else if let Ok(mints) = serde_json::from_str::<Vec<String>>(&data) {
                            // Migrate old format (Vec<String>) to new format with current timestamp
                            info!("Migrating {} bought mints from legacy format", mints.len());
                            mints.into_iter().map(|m| (m, now)).collect()
                        } else {
                            std::collections::HashMap::new()
                        }
                    }
                    Err(_) => std::collections::HashMap::new(),
                }
            } else {
                std::collections::HashMap::new()
            };
        std::sync::Arc::new(tokio::sync::Mutex::new(loaded))
    };
    let bought_mints_path = std::sync::Arc::new(bought_mints_path);

    // Track recently sold mints with cooldown (5 minutes before re-entry allowed)
    // This prevents buying back at the top immediately after selling
    const SOLD_MINTS_COOLDOWN_SECS: i64 = 300; // 5 minutes
    let sold_mints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Track failed mints (buys that didn't land tokens) with longer cooldown
    // This prevents repeatedly trying to buy tokens that consistently fail
    const FAILED_MINTS_COOLDOWN_SECS: i64 = 1800; // 30 minutes
    let failed_mints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Initialize kill-switch evaluator for smart money exits
    let kill_switch_evaluator: Option<std::sync::Arc<KillSwitchEvaluator>> =
        if config.smart_money.enabled && config.smart_money.kill_switches.enabled {
            info!("Initializing kill-switch evaluator for hot_scan...");
            let evaluator = std::sync::Arc::new(KillSwitchEvaluator::new(
                config.smart_money.kill_switches.clone(),
                config.smart_money.holder_watcher.clone(),
            ));
            info!(
                "Kill-switches ENABLED: deployer_sell={}, top_holder_sell={}",
                config.smart_money.kill_switches.deployer_sell_any,
                config.smart_money.kill_switches.top_holder_sell
            );
            Some(evaluator)
        } else {
            info!("Kill-switches disabled in hot_scan mode");
            None
        };

    let dex_client = DexScreenerClient::new();
    let scan_config = HotScanConfig {
        min_m5_change: min_m5,
        min_buy_sell_ratio: min_ratio,
        min_liquidity_usd: min_liquidity,
        max_market_cap: max_mcap,
        ..Default::default()
    };

    // === POSITION MONITOR BACKGROUND TASK ===
    if config.auto_sell.enabled && !dry_run {
        let monitor_config = config.clone();
        let monitor_positions = position_manager.clone();
        let monitor_keypair = keypair.clone();
        let monitor_rpc = rpc_client.clone();
        let monitor_dex = DexScreenerClient::new();
        let monitor_bought_mints = bought_mints.clone();
        let monitor_bought_mints_path = bought_mints_path.clone();
        let monitor_sold_mints = sold_mints.clone();
        let monitor_kill_switch = kill_switch_evaluator.clone();
        let monitor_multi_wallet = multi_wallet.clone();
        // MPT-001 Agent H: authoritative market oracle for the HotScan price-exit
        // path. Every monitor cycle fetches a FRESH on-chain mark and, before any
        // price-based sell, an exact-size same-venue executable quote. DexScreener
        // is discovery/observation only and never authorizes an exit (H1/H2/H4).
        let monitor_oracle = market_oracle.clone();
        // === AGENT G: transaction-truth wiring for the HotScan SELL path ===
        // Clone the already-initialized reconciler, shared pending journal, exact
        // wallet registry, route-capable trader handles (Agent F), and the
        // new-entry halt flag into the monitor task. The monitor now resolves the
        // EXACT execution route per position (no global route, no Lightning->Local
        // fallback) and reconciles every exit before touching position state.
        let monitor_reconciler = trade_reconciler.clone();
        let monitor_pending = pending_executions.clone();
        let monitor_registry = hotscan_registry.clone();
        let monitor_local_trader = hotscan_local_trader.clone();
        let monitor_lightning_trader = hotscan_lightning_trader.clone();
        let monitor_entry_halt = new_entries_halted.clone();

        tokio::spawn(async move {
            info!("=== POSITION MONITOR STARTED ===");
            let poll_interval_ms = monitor_config.auto_sell.price_poll_interval_ms;
            info!("Features: Dynamic Trailing ({}%-{}%), Layered Exits ({}%/{}%/{}%), Kill-Switch, exact-wallet reconciled exits",
                monitor_config.auto_sell.trailing_stop_base_pct,
                monitor_config.auto_sell.trailing_stop_tight_pct,
                monitor_config.auto_sell.quick_profit_pct,
                monitor_config.auto_sell.second_profit_pct,
                monitor_config.auto_sell.take_profit_pct
            );
            info!("Poll interval: {}ms", poll_interval_ms);

            let mut sell_attempts: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();

            loop {
                tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;

                let positions = monitor_positions.get_all_positions().await;
                if positions.is_empty() {
                    continue;
                }

                for position in positions {
                    // MPT-001 Agent H1/H2/H4: fetch a FRESH on-chain mark before any
                    // price logic. DexScreener is discovery/observation ONLY — it may
                    // be LOGGED but can never authorize an exit or feed the position
                    // mark / executable price / realized P&L. A valid SOL mark updates
                    // the Position (mark + peak) via PositionManager::update_price; we
                    // then re-read the updated Position so peak/trailing reflect it.
                    //
                    // On any market error (RPC/decode/unsupported quote) we do NOT fall
                    // back to the stale persisted `current_price` and we do NOT fall
                    // back to DexScreener to authorize a price sell (INV-MKT-012 /
                    // Section 16): skip this position for the cycle and halt NEW entries
                    // because the position is not operationally priceable.
                    let mint_pubkey = match Pubkey::from_str(position.mint.trim()) {
                        Ok(pk) => pk,
                        Err(e) => {
                            monitor_entry_halt.store(true, Ordering::SeqCst);
                            error!(
                                "[{}] position mint '{}' does not parse ({}) - no price exit, HotScan new entries HALTED",
                                position.symbol, position.mint, e
                            );
                            continue;
                        }
                    };

                    // DexScreener observation ONLY (non-authoritative). Logged for
                    // visibility; never used as mark/executable price/P&L (H4).
                    if let Ok(Some(token_info)) = monitor_dex.get_token_info(&position.mint).await {
                        if token_info.price_native > 0.0 {
                            info!(
                                "[{}] DexScreener observation (non-authoritative): {:.10}",
                                position.symbol, token_info.price_native
                            );
                        }
                    }

                    let fresh_mark = match monitor_oracle.snapshot(&mint_pubkey).await {
                        Ok(snap) => match snap.mark_price_sol_per_token {
                            Some(m) if m.is_finite() && m > 0.0 => m,
                            _ => {
                                // Unsupported quote asset / no usable SOL mark. Not
                                // operationally priceable: halt new entries, keep the
                                // position, never trigger a price sell on stale data.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                warn!(
                                    "[{}] no fresh SOL mark for {} - no price exit this cycle, HotScan new entries HALTED",
                                    position.symbol, position.mint
                                );
                                continue;
                            }
                        },
                        Err(e) => {
                            monitor_entry_halt.store(true, Ordering::SeqCst);
                            warn!(
                                "[{}] market snapshot failed for {}: {} - no price exit this cycle (no stale/Dex fallback), HotScan new entries HALTED",
                                position.symbol, position.mint, e
                            );
                            continue;
                        }
                    };

                    // Update position mark/peak from the FRESH on-chain mark (H2).
                    monitor_positions
                        .update_price(&position.mint, fresh_mark)
                        .await;

                    // Get updated position with peak_price tracked.
                    let position = match monitor_positions.get_position(&position.mint).await {
                        Some(p) => p,
                        None => continue,
                    };

                    // `current_price` is now the latest FRESH on-chain mark (H2/H4).
                    let current_price = position.current_price;
                    if current_price <= 0.0 {
                        continue;
                    }

                    // G1: The old post-buy TX-confirmation polling (the tracking
                    // HashSet, the 5s wait, the token-balance poll, and the 30s
                    // likely-failed abandon path) is GONE. After Agent F, a real
                    // HotScan position exists ONLY after a ConfirmedFill; legacy
                    // positions are handled by startup recovery or remain blocked.
                    // The monitor never re-derives fill truth from a balance poll.

                    // Calculate P&L from entry
                    let pnl_pct = if position.entry_price > 0.0 {
                        ((current_price - position.entry_price) / position.entry_price) * 100.0
                    } else {
                        0.0
                    };

                    // Calculate drop from peak (for trailing stop)
                    let peak_price = if position.peak_price > 0.0 {
                        position.peak_price
                    } else {
                        position.entry_price
                    };
                    let drop_from_peak_pct = if peak_price > 0.0 {
                        ((peak_price - current_price) / peak_price) * 100.0
                    } else {
                        0.0
                    };

                    let hold_time_secs = (chrono::Utc::now() - position.entry_time)
                        .num_seconds()
                        .max(0) as u64;

                    // Get entry-type-specific thresholds
                    let tp_pct = position.entry_type.take_profit_pct();
                    let sl_pct = position.entry_type.stop_loss_pct();
                    let quick_profit_pct = position.entry_type.quick_profit_pct();
                    let max_hold = position.entry_type.max_hold_secs();

                    // Log position status periodically
                    if hold_time_secs % 15 == 0 {
                        info!(
                            "[{}] Price: {:.10} | P&L: {:+.1}% | Peak: {:+.1}% | Hold: {}s",
                            position.symbol,
                            current_price,
                            pnl_pct,
                            if peak_price > position.entry_price {
                                ((peak_price - position.entry_price) / position.entry_price) * 100.0
                            } else {
                                0.0
                            },
                            hold_time_secs
                        );
                    }

                    // Get config values for layered exits
                    let no_movement_secs = monitor_config.auto_sell.no_movement_secs;
                    let no_movement_threshold = monitor_config.auto_sell.no_movement_threshold_pct;
                    let second_profit_pct = monitor_config.auto_sell.second_profit_pct;

                    // === DYNAMIC TRAILING STOP ===
                    // Tighten trailing stop as profit grows to prevent round-tripping
                    let trailing_stop_pct = if monitor_config.auto_sell.dynamic_trailing_enabled {
                        if pnl_pct >= 25.0 {
                            monitor_config.auto_sell.trailing_stop_tight_pct  // 3% at high gains
                        } else if pnl_pct >= 15.0 {
                            monitor_config.auto_sell.trailing_stop_medium_pct // 4% at medium gains
                        } else {
                            monitor_config.auto_sell.trailing_stop_base_pct   // 5% base
                        }
                    } else {
                        5.0 // Fixed trailing stop if dynamic disabled
                    };

                    let mut should_sell = false;
                    let mut sell_pct = "100%";
                    let mut reason = String::new();
                    // MPT-001 Agent H3/F2: capture WHICH price rule fired so the exact
                    // same condition can be re-confirmed against the executable-quote
                    // price before any sell is submitted. Kill-switch triggers leave
                    // this None: they are NOT price-triggered and follow the F8
                    // emergency policy (may fall back to unquoted Auto).
                    let mut exit_category: Option<PriceExitCategory> = None;
                    // True only for a kill-switch / risk trigger (no price category).
                    let mut is_kill_switch = false;

                    // === KILL-SWITCH CHECK (HIGHEST PRIORITY) ===
                    // First check position flag (set by other systems)
                    if let Some(ks_reason) = monitor_positions.is_kill_switch_triggered(&position.mint).await {
                        should_sell = true;
                        is_kill_switch = true;
                        reason = format!("KILL-SWITCH: {}", ks_reason);
                        warn!("KILL-SWITCH EXIT: {} - {}", position.symbol, ks_reason);
                    }
                    // Then actively evaluate kill-switch conditions
                    if !should_sell {
                        if let Some(ref evaluator) = monitor_kill_switch {
                            if let KillSwitchDecision::Exit(alert) = evaluator.should_exit(&position.mint) {
                                should_sell = true;
                                is_kill_switch = true;
                                reason = format!("KILL-SWITCH: {} (urgency: {:?})", alert.reason, alert.urgency);
                                warn!("KILL-SWITCH EXIT: {} - {} [{:?}]", position.symbol, alert.reason, alert.urgency);
                            }
                        }
                    }

                    // 1. Stop loss
                    if !should_sell && pnl_pct <= -sl_pct {
                        should_sell = true;
                        reason = format!("STOP LOSS at {:.1}% (limit: -{:.0}%)", pnl_pct, sl_pct);
                        exit_category = Some(PriceExitCategory::StopLoss {
                            entry_price: position.entry_price,
                            sl_pct,
                        });
                    }

                    // 2. Trailing stop (only if in profit and dropped from peak)
                    // Now uses dynamic trailing stop percentage
                    if !should_sell && pnl_pct > 0.0 && drop_from_peak_pct >= trailing_stop_pct {
                        should_sell = true;
                        reason = format!(
                            "TRAILING STOP: dropped {:.1}% from peak (P&L: +{:.1}%, trail: {:.0}%)",
                            drop_from_peak_pct, pnl_pct, trailing_stop_pct
                        );
                        exit_category = Some(PriceExitCategory::TrailingStop {
                            entry_price: position.entry_price,
                            peak_price,
                            trailing_pct: trailing_stop_pct,
                        });
                    }

                    // 3. Take profit (final exit)
                    if !should_sell && pnl_pct >= tp_pct {
                        should_sell = true;
                        reason = format!("TAKE PROFIT at {:.1}% (target: {:.0}%)", pnl_pct, tp_pct);
                        exit_category = Some(PriceExitCategory::TakeProfit {
                            entry_price: position.entry_price,
                            tp_pct,
                        });
                    }

                    // 4. Quick profit - FIRST LAYER (50% sell at quick_profit_pct)
                    if !should_sell
                        && !position.quick_profit_taken
                        && pnl_pct >= quick_profit_pct
                        && pnl_pct < second_profit_pct
                    {
                        should_sell = true;
                        sell_pct = "50%";
                        reason = format!("LAYER 1: Quick profit at {:.1}% - selling 50%", pnl_pct);
                        // Confirms when pnl >= quick_profit_pct and < second_profit_pct.
                        exit_category = Some(PriceExitCategory::QuickProfit {
                            entry_price: position.entry_price,
                            qp_pct: quick_profit_pct,
                            tp_pct: second_profit_pct,
                        });
                    }

                    // 5. Second profit - SECOND LAYER (25% sell at second_profit_pct)
                    if !should_sell
                        && position.quick_profit_taken
                        && !position.second_profit_taken
                        && pnl_pct >= second_profit_pct
                        && pnl_pct < tp_pct
                    {
                        should_sell = true;
                        sell_pct = "25%";
                        reason = format!("LAYER 2: Second profit at {:.1}% - selling 25%", pnl_pct);
                        // Reuse the QuickProfit predicate with the second-profit band:
                        // confirms when pnl >= second_profit_pct and < take_profit_pct.
                        exit_category = Some(PriceExitCategory::QuickProfit {
                            entry_price: position.entry_price,
                            qp_pct: second_profit_pct,
                            tp_pct,
                        });
                    }

                    // 6. No-movement exit
                    if !should_sell
                        && hold_time_secs >= no_movement_secs
                        && pnl_pct.abs() < no_movement_threshold
                    {
                        should_sell = true;
                        reason = format!("NO MOVEMENT: {:.1}% after {}s", pnl_pct, hold_time_secs);
                        exit_category = Some(PriceExitCategory::TimeBased);
                    }

                    // 7. Max hold time
                    if !should_sell {
                        if let Some(max_secs) = max_hold {
                            if hold_time_secs >= max_secs {
                                should_sell = true;
                                reason = format!(
                                    "MAX HOLD TIME ({} secs) P&L: {:.1}%",
                                    max_secs, pnl_pct
                                );
                                exit_category = Some(PriceExitCategory::TimeBased);
                            }
                        }
                    }

                    // === AGENT G: reconciled HotScan exit with EXACT wallet route ===
                    // The kill-switch trigger already funnels into this same
                    // `should_sell` block (G9), so it receives the identical
                    // reconciled path — there is no separate estimated kill-switch
                    // exit.
                    if should_sell {
                        warn!(
                            "AUTO-SELL TRIGGERED: {} ({}) - {}",
                            position.symbol, position.mint, reason
                        );

                        let slippage = monitor_config.trading.slippage_bps / 100;
                        let priority_fee =
                            monitor_config.trading.priority_fee_lamports as f64 / 1e9;

                        // Keep the existing max retry count (5). Exceeding it leaves the
                        // position OPEN/TRACKED (INV-POS-001); a failed submission must
                        // never make a wallet-owned position disappear.
                        {
                            let attempts = sell_attempts.entry(position.mint.clone()).or_insert(0);
                            *attempts += 1;
                            if *attempts > 5 {
                                error!(
                                    "AUTO-SELL UNRESOLVED for {} after 5 attempts - position remains OPEN/TRACKED",
                                    position.symbol
                                );
                                sell_attempts.remove(&position.mint);
                                continue;
                            }
                        }
                        let attempt_no = *sell_attempts.get(&position.mint).unwrap_or(&1);

                        // G2 / C2 PENDING GUARD: if a Buy OR a Sell for this mint is
                        // already in flight, do NOT submit a new sell. A pending Buy
                        // can be a confirmed-fill whose durable position save failed;
                        // selling before it reconciles could close a position that
                        // restart recovery would then re-open from the stale pending
                        // buy. Either pending => keep the position, no submission.
                        let pending_buy = monitor_pending
                            .get_for_mint(&position.mint, ReconciliationSide::Buy)
                            .await;
                        let pending_sell = monitor_pending
                            .get_for_mint(&position.mint, ReconciliationSide::Sell)
                            .await;
                        if let Some(sig) = pending_blocks_automatic_sell(
                            pending_buy.as_ref(),
                            pending_sell.as_ref(),
                        ) {
                            warn!(
                                "[{}] pending transaction already in flight (sig {}) - not submitting a new exit; position kept",
                                position.symbol, sig
                            );
                            continue;
                        }

                        // G3 CANONICAL POSITION REQUIREMENT: token_decimals Some, wallet
                        // Pubkey valid, and an exact registry route. No wallet fallback.
                        if position.token_decimals.is_none() {
                            monitor_entry_halt.store(true, Ordering::SeqCst);
                            error!(
                                "[{}] position has unknown token_decimals - no sell, HotScan new entries HALTED",
                                position.symbol
                            );
                            continue;
                        }
                        let position_wallet = match Pubkey::from_str(position.wallet_pubkey.trim()) {
                            Ok(pk) if !position.wallet_pubkey.trim().is_empty() => pk,
                            _ => {
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "[{}] empty/invalid wallet_pubkey '{}' - no sell, HotScan new entries HALTED",
                                    position.symbol, position.wallet_pubkey
                                );
                                continue;
                            }
                        };
                        let route = match monitor_registry.route_for(&position_wallet) {
                            Some(r) => r,
                            None => {
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "[{}] wallet {} has no route in registry - no sell, HotScan new entries HALTED",
                                    position.symbol, position.wallet_pubkey
                                );
                                continue;
                            }
                        };

                        // G5 INTENT MAPPING (records QuickProfit/SecondProfit/Full;
                        // does NOT itself change the size — the exact raw layer amount
                        // computed below is the authoritative submitted size).
                        let intent = hotscan_sell_intent_for_layer(sell_pct);

                        // MPT-001 Agent H3/F3/F6: compute the EXACT raw amount for the
                        // intended layer (100%/50%/25% via integer division) and fetch
                        // the FINAL same-venue executable quote immediately before send.
                        // For a price-based exit this quote is BOTH the trigger
                        // re-confirmation (F2) AND the execution reference (F7) — never
                        // reused across cycles or layers. A quote failure means nothing
                        // was submitted: keep the position; halt new entries when the
                        // market is unsupported/unavailable.
                        //
                        // Kill-switch (F8): NOT price-triggered. Prefer a fresh quote +
                        // pinned venue; if the oracle fails, fall back to the existing
                        // emergency Auto/unquoted route (never blocked by the oracle,
                        // never fabricating an expected price).
                        let token_decimals = match position.token_decimals {
                            Some(d) => d,
                            None => {
                                // Already guarded above; defensive re-check before quote.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "[{}] position lost token_decimals before quote - no sell, HotScan new entries HALTED",
                                    position.symbol
                                );
                                continue;
                            }
                        };
                        let intended_raw = match layer_raw_amount(position.token_amount, sell_pct) {
                            Some(r) => r,
                            None => {
                                warn!(
                                    "[{}] exact raw amount for layer {} of raw {} is zero/unknown - no sell",
                                    position.symbol, sell_pct, position.token_amount
                                );
                                continue;
                            }
                        };

                        // Extract a usable same-venue SOL executable quote (the ONLY
                        // carrier of the executable price + pinned venue). `None` when
                        // the quote is missing, not a SOL pair, or has no usable price:
                        // fatal for a price exit, degrades to unquoted emergency for a
                        // kill-switch. `quote_slot` is retained for logging only.
                        let mut quote_slot: Option<u64> = None;
                        let exec_quote: Option<HotScanExecQuote> = match monitor_oracle
                            .quote_sell_raw(&mint_pubkey, intended_raw)
                            .await
                        {
                            Ok(quote) if quote.is_sol_pair() => {
                                match quote.expected_price_sol_per_token {
                                    Some(p) if p.is_finite() && p > 0.0 => {
                                        quote_slot = Some(quote.slot);
                                        Some(HotScanExecQuote {
                                            exec_price_sol_per_token: p,
                                            venue: quote.venue,
                                        })
                                    }
                                    _ => None,
                                }
                            }
                            Ok(_) => None, // quote returned but not a supported SOL pair
                            Err(e) => {
                                if is_kill_switch {
                                    warn!(
                                        "KILL-SWITCH UNQUOTED EMERGENCY ROUTE: {} - sell quote unavailable ({}); selling {} via Auto (no expected price fabricated)",
                                        position.symbol, e, sell_pct
                                    );
                                } else {
                                    warn!(
                                        "[{}] no executable sell quote ({} raw): {} - no price sell, HotScan new entries HALTED",
                                        position.symbol, intended_raw, e
                                    );
                                }
                                None
                            }
                        };

                        // Resolve (submit_amount, sell_pool) via the pure authorizer.
                        // A PRICE exit MUST be quote-confirmed and venue-pinned
                        // (H3/F2/F4/F5); a KILL-SWITCH (F8) prefers the quoted+pinned
                        // exit but degrades to the unquoted Auto route (never blocked).
                        let (submit_amount, sell_pool) = match hotscan_exit_decision(
                            is_kill_switch,
                            exit_category,
                            exec_quote,
                            intended_raw,
                            token_decimals,
                            sell_pct,
                        ) {
                            HotScanExitDecision::QuotedSell { submit_amount, pool } => {
                                if is_kill_switch {
                                    info!(
                                        "KILL-SWITCH quoted exit for {}: venue={:?} pool={:?} raw={} amount={} exec_price={:.12} quote_slot={:?}",
                                        position.symbol,
                                        exec_quote.map(|q| q.venue),
                                        pool,
                                        intended_raw,
                                        submit_amount,
                                        exec_quote.map(|q| q.exec_price_sol_per_token).unwrap_or(0.0),
                                        quote_slot
                                    );
                                } else {
                                    info!(
                                        "[{}] exit CONFIRMED: venue={:?} pool={:?} raw={} amount={} exec_price={:.12} quote_slot={:?}",
                                        position.symbol,
                                        exec_quote.map(|q| q.venue),
                                        pool,
                                        intended_raw,
                                        submit_amount,
                                        exec_quote.map(|q| q.exec_price_sol_per_token).unwrap_or(0.0),
                                        quote_slot
                                    );
                                }
                                (submit_amount, pool)
                            }
                            HotScanExitDecision::EmergencyUnquoted { submit_amount, pool } => {
                                // F8: only reachable for a kill-switch with no usable
                                // quote. The oracle-Err case was already logged above;
                                // cover the not-a-SOL-pair / no-price cases here.
                                warn!(
                                    "KILL-SWITCH UNQUOTED EMERGENCY ROUTE: {} - no usable executable quote; selling {} via Auto (no expected price fabricated)",
                                    position.symbol, submit_amount
                                );
                                (submit_amount, pool)
                            }
                            HotScanExitDecision::NoSell => {
                                if is_kill_switch {
                                    // Not reachable (kill-switch never yields NoSell), but
                                    // fail closed defensively.
                                    continue;
                                }
                                // Price exit not authorized: either no usable executable
                                // quote, or the executable quote no longer meets the
                                // trigger. Nothing submitted; keep the position. Halt new
                                // entries only when the market was not usably priceable.
                                if exec_quote.is_none() {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    warn!(
                                        "[{}] no usable executable SOL quote - no price sell, HotScan new entries HALTED",
                                        position.symbol
                                    );
                                } else {
                                    info!(
                                        "[{}] exit NOT confirmed: mark triggered '{}' but executable quote price {:.12} SOL/token no longer meets the condition - no sell",
                                        position.symbol,
                                        reason,
                                        exec_quote.map(|q| q.exec_price_sol_per_token).unwrap_or(0.0)
                                    );
                                }
                                continue;
                            }
                        };

                        // G4 EXACT ROUTE per position. Ignore any global route. NO
                        // Lightning-attempts-1-3-then-Local, NO primary signer fallback,
                        // NO unwrap_or(monitor_wallet).
                        let routed_sell: Option<Result<String, crate::error::Error>> = match route {
                            crate::wallet::ExecutionRoute::Local => {
                                // Local position => use the Local trader + EXACT local
                                // signer (primary keypair if its pubkey matches, else the
                                // recovery MultiWalletManager). Missing signer => no sell.
                                match monitor_local_trader {
                                    Some(ref local_trader) => {
                                        if monitor_keypair.pubkey() == position_wallet {
                                            info!(
                                                "[{}] Local sell (attempt {}) via primary keypair",
                                                position.symbol, attempt_no
                                            );
                                            Some(
                                                local_trader
                                                    .sell_local_with_pool(
                                                        &position.mint,
                                                        &submit_amount,
                                                        slippage,
                                                        priority_fee,
                                                        &monitor_keypair,
                                                        &monitor_rpc,
                                                        sell_pool,
                                                    )
                                                    .await,
                                            )
                                        } else if let Some(ref mw) = monitor_multi_wallet {
                                            match mw.find_by_address(&position.wallet_pubkey) {
                                                Some(tw) => {
                                                    info!(
                                                        "[{}] Local sell (attempt {}) via recovery wallet {}",
                                                        position.symbol, attempt_no, position.wallet_pubkey
                                                    );
                                                    Some(
                                                        local_trader
                                                            .sell_local_with_pool(
                                                                &position.mint,
                                                                &submit_amount,
                                                                slippage,
                                                                priority_fee,
                                                                &tw.keypair,
                                                                &monitor_rpc,
                                                                sell_pool,
                                                            )
                                                            .await,
                                                    )
                                                }
                                                None => None,
                                            }
                                        } else {
                                            None
                                        }
                                    }
                                    None => None,
                                }
                            }
                            crate::wallet::ExecutionRoute::Lightning => {
                                // Lightning position => Lightning trader ONLY. No Local
                                // fallback (INV-WALLET-001/003).
                                match monitor_lightning_trader {
                                    Some(ref lightning_trader) => {
                                        info!(
                                            "[{}] Lightning sell (attempt {})",
                                            position.symbol, attempt_no
                                        );
                                        Some(
                                            lightning_trader
                                                .sell_with_pool(
                                                    &position.mint,
                                                    &submit_amount,
                                                    slippage,
                                                    priority_fee,
                                                    sell_pool,
                                                )
                                                .await,
                                        )
                                    }
                                    None => None,
                                }
                            }
                        };

                        let sell_result = match routed_sell {
                            Some(r) => r,
                            None => {
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "[{}] no exact signer/trader for wallet {} route {:?} - no sell, HotScan new entries HALTED",
                                    position.symbol, position.wallet_pubkey, route
                                );
                                continue;
                            }
                        };

                        // G5 SUBMISSION: on provider error keep the position; on signature
                        // log SUBMITTED (not EXECUTED), persist pending, then reconcile.
                        let signature = match sell_result {
                            Ok(sig) => sig,
                            Err(e) => {
                                error!(
                                    "AUTO-SELL SUBMISSION FAILED for {} (attempt {}): {} - position remains OPEN/TRACKED",
                                    position.symbol, attempt_no, e
                                );
                                continue;
                            }
                        };

                        info!("AUTO-SELL SUBMITTED: {} (sig {})", position.symbol, signature);

                        let pending_sell = PendingExecution::sell(
                            signature.clone(),
                            position.mint.clone(),
                            position.wallet_pubkey.clone(),
                            PendingSellContext {
                                // MPT-001 Agent H3/F4: the pending context stores the
                                // EXACT submitted amount string that was actually sent
                                // (a quoted decimal size, or the emergency percentage on
                                // an unquoted kill-switch fallback), not a fixed "25%".
                                requested_amount: submit_amount.clone(),
                                intent,
                                reason: reason.clone(),
                            },
                        );
                        // AUDIT-002 A8: retain the exact pending sell + whether the first
                        // journal write persisted. HotScan has no shared primary
                        // reservation, so no reservation behavior is added here.
                        let pending_sell_persisted = match monitor_pending.upsert(pending_sell.clone()).await {
                            Ok(()) => true,
                            Err(e) => {
                                // Signature already exists chain-side; persistence failed.
                                // Halt new entries but STILL reconcile the submitted signature.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "[{}] failed to persist pending sell (sig {}): {} - HotScan new entries HALTED, still reconciling",
                                    position.symbol, signature, e
                                );
                                false
                            }
                        };

                        // Reconcile: no fixed sleep, no estimated-proceeds fallback.
                        let outcome = monitor_reconciler
                            .reconcile(
                                &signature,
                                &position.wallet_pubkey,
                                &position.mint,
                                ReconciliationSide::Sell,
                            )
                            .await;

                        match outcome {
                            Ok(ReconciliationOutcome::ConfirmedFailure {
                                error, observed_after_ms, ..
                            }) => {
                                // Remove pending, keep position, do NOT mark any layer.
                                if let Err(e) = monitor_pending.remove(&signature).await {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "[{}] failed to remove pending sell after ConfirmedFailure (sig {}): {} - HotScan new entries HALTED",
                                        position.symbol, signature, e
                                    );
                                }
                                error!(
                                    "AUTO-SELL CONFIRMED FAILED for {} (sig {}): {} ({}ms observed) - position remains OPEN/TRACKED",
                                    position.symbol, signature, error, observed_after_ms
                                );
                                continue;
                            }
                            Ok(ReconciliationOutcome::Unresolved { reason: unresolved_reason, .. }) => {
                                // KEEP pending, keep position + flags, halt new entries.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                // AUDIT-002 A8: retry durability if the initial write failed.
                                if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                    error!(
                                        "AUTO-SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept (durable), position kept, HotScan new entries HALTED",
                                        position.mint, signature, position.wallet_pubkey, unresolved_reason
                                    );
                                } else {
                                    error!(
                                        "CRITICAL: AUTO-SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Position kept, HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                        position.mint, signature, position.wallet_pubkey, unresolved_reason, signature
                                    );
                                }
                                continue;
                            }
                            Err(e) => {
                                // Structural observer failure is not tx-failure proof.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                // AUDIT-002 A8: same durability retry rule as Unresolved.
                                if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                    error!(
                                        "CRITICAL: HotScan sell reconciliation error for {} (sig {}): {} - pending kept (durable), position kept, HotScan new entries HALTED",
                                        position.symbol, signature, e
                                    );
                                } else {
                                    error!(
                                        "CRITICAL: HotScan sell reconciliation error for {} (sig {}): {} - AND pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. Position kept, HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                        position.symbol, signature, e, signature
                                    );
                                }
                                continue;
                            }
                            Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                // Identity validation at the live boundary.
                                if fill.side != ReconciliationSide::Sell
                                    || fill.wallet != position.wallet_pubkey
                                    || fill.mint != position.mint
                                {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    // AUDIT-002 A8: confirmed-but-unapplied. Retry durability.
                                    if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                        error!(
                                            "CRITICAL: reconciled HotScan sell fill identity mismatch for sig {} - pending kept (durable), position kept, HotScan new entries HALTED",
                                            signature
                                        );
                                    } else {
                                        error!(
                                            "CRITICAL: reconciled HotScan sell fill identity mismatch for sig {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Position kept, HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                            signature, signature
                                        );
                                    }
                                    continue;
                                }

                                // G6 economics via the pure fill validator (decimals match,
                                // nonzero raw, finite delta/price, no oversell). Negative net
                                // proceeds are allowed.
                                let (actual_sold_raw, actual_received_sol, actual_exit_price) =
                                    match primary_sell_fill_values(&fill, &position) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            monitor_entry_halt.store(true, Ordering::SeqCst);
                                            // AUDIT-002 A8: confirmed-but-unapplied. Retry durability.
                                            if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                                error!(
                                                    "[{}] reconciled sell fill validation failed (sig {}): {} - pending kept (durable), position kept, HotScan new entries HALTED",
                                                    position.symbol, signature, e
                                                );
                                            } else {
                                                error!(
                                                    "CRITICAL: [{}] reconciled sell fill validation failed (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. Position kept, HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                    position.symbol, signature, e, signature
                                                );
                                            }
                                            continue;
                                        }
                                    };

                                // G6 actual reconciled close (idempotent via receipt ledger).
                                let close_result = match monitor_positions
                                    .close_position_reconciled(
                                        &position.mint,
                                        &signature,
                                        actual_sold_raw,
                                        actual_received_sol,
                                    )
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(e) => {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        // AUDIT-002 A8: confirmed-but-unapplied. Retry durability.
                                        if retry_pending_durability_if_needed(&monitor_pending, &pending_sell, pending_sell_persisted).await {
                                            error!(
                                                "[{}] reconciled close failed (sig {}): {} - pending kept (durable), HotScan new entries HALTED",
                                                position.symbol, signature, e
                                            );
                                        } else {
                                            error!(
                                                "CRITICAL: [{}] reconciled close failed (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                position.symbol, signature, e, signature
                                            );
                                        }
                                        continue;
                                    }
                                };

                                sell_attempts.remove(&position.mint);

                                // G7/G8 full vs partial comes from the ACTUAL fill.
                                let fully_closed = close_result.fully_closed;
                                let already_applied = close_result.already_applied;
                                let hold_secs =
                                    (chrono::Utc::now() - position.entry_time).num_seconds();

                                // G7 LAYER MARKERS: after a confirmed actual PARTIAL close,
                                // apply the intent's marker. Even on idempotent replay a
                                // missing marker may be applied while the position remains.
                                // Never mark on failure/unresolved (handled above).
                                if !fully_closed {
                                    match intent {
                                        PendingSellIntent::QuickProfit => {
                                            let _ = monitor_positions
                                                .mark_quick_profit_taken(&position.mint)
                                                .await;
                                        }
                                        PendingSellIntent::SecondProfit => {
                                            let _ = monitor_positions
                                                .mark_second_profit_taken(&position.mint)
                                                .await;
                                        }
                                        _ => {}
                                    }
                                }

                                // G8 FULL-EXIT CACHE/COOLDOWN: only when the ACTUAL close is
                                // fully_closed. A requested "100%" that only partially fills
                                // must NOT remove bought_mint or mark the sold cooldown.
                                if hotscan_full_exit_removes_cache(fully_closed) {
                                    let _ = remove_bought_mint(
                                        &monitor_bought_mints,
                                        &monitor_bought_mints_path,
                                        &position.mint,
                                    )
                                    .await;
                                    {
                                        let mut sold = monitor_sold_mints.lock().await;
                                        sold.insert(
                                            position.mint.clone(),
                                            chrono::Utc::now().timestamp(),
                                        );
                                        info!(
                                            "[{}] Added to sold_mints (5min cooldown before re-entry)",
                                            position.symbol
                                        );
                                    }
                                }

                                if fully_closed {
                                    info!("=== AUTO-SELL CONFIRMED (Full) ===");
                                } else {
                                    info!("=== AUTO-SELL CONFIRMED (Partial) ===");
                                }
                                info!(
                                    "  {} (sig {}) | sold_raw={} decimals={} net_sol_delta={:+.9} exit_price={:.12} SOL/token | realized P&L: {:+.9} SOL | recon_wait={}ms | hold={}s{}",
                                    position.symbol,
                                    signature,
                                    actual_sold_raw,
                                    fill.token_decimals,
                                    actual_received_sol,
                                    actual_exit_price,
                                    close_result.pnl_sol,
                                    fill.reconciliation_wait_ms,
                                    hold_secs,
                                    if already_applied { " (already applied; idempotent)" } else { "" }
                                );

                                // Remove pending LAST, after durable position application.
                                if let Err(e) = monitor_pending.remove(&signature).await {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "[{}] failed to remove pending sell after confirmed fill (sig {}): {} - HotScan new entries HALTED; position state already applied",
                                        position.symbol, signature, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Main scan loop
    loop {
        println!("\n{:=<80}", "");
        println!("Scanning DexScreener for hot tokens...");

        let hot_tokens = dex_client.scan_hot_tokens(&scan_config).await?;

        if hot_tokens.is_empty() {
            println!("No tokens matching criteria found.");
        } else {
            println!("Found {} hot tokens:", hot_tokens.len());
            println!("{:-<80}", "");

            for (i, token) in hot_tokens.iter().take(10).enumerate() {
                let boost_indicator = if token.is_boosted { " [BOOSTED]" } else { "" };
                println!(
                    "{}. {} ({}) | M5: {:+.1}% H1: {:+.1}% | Ratio: {:.1} | MCap: ${:.0}k | Liq: ${:.0}k | Score: {:.1}{}",
                    i + 1,
                    token.symbol,
                    &token.mint[..8],
                    token.m5_change,
                    token.h1_change,
                    token.buy_sell_ratio,
                    token.market_cap / 1000.0,
                    token.liquidity_usd / 1000.0,
                    token.score(),
                    boost_indicator
                );
            }

            // Auto-buy logic
            if auto_buy {
                // PRE-TRADE VALIDATION: Check if we can trade at all
                if position_manager.is_daily_loss_limit_reached().await {
                    warn!("TRADING PAUSED: Daily loss limit reached. Monitoring positions only.");
                } else {
                    let mut bought = bought_mints.lock().await;

                    for token in hot_tokens.iter().take(3) {
                        if bought.contains_key(&token.mint) {
                            info!("Skipping {} - already bought this session", token.symbol);
                            continue;
                        }

                        // Check sold_mints cooldown (5 minutes after selling)
                        {
                            let sold = sold_mints.lock().await;
                            if let Some(&sold_at) = sold.get(&token.mint) {
                                let now = chrono::Utc::now().timestamp();
                                let elapsed = now - sold_at;
                                if elapsed < SOLD_MINTS_COOLDOWN_SECS {
                                    let remaining = SOLD_MINTS_COOLDOWN_SECS - elapsed;
                                    info!("Skipping {} - sold {}s ago, cooldown {}s remaining",
                                          token.symbol, elapsed, remaining);
                                    continue;
                                }
                            }
                        }

                        // Check failed_mints cooldown (30 minutes after failed buy)
                        {
                            let failed = failed_mints.lock().await;
                            if let Some(&failed_at) = failed.get(&token.mint) {
                                let now = chrono::Utc::now().timestamp();
                                let elapsed = now - failed_at;
                                if elapsed < FAILED_MINTS_COOLDOWN_SECS {
                                    let remaining_mins = (FAILED_MINTS_COOLDOWN_SECS - elapsed) / 60;
                                    info!("Skipping {} - failed buy {}m ago, cooldown {}m remaining",
                                          token.symbol, elapsed / 60, remaining_mins);
                                    continue;
                                }
                            }
                        }

                        // Check if we already have a position
                        if position_manager.get_position(&token.mint).await.is_some() {
                            info!("Skipping {} - already have position", token.symbol);
                            continue;
                        }

                        // F3: the pre-send PositionManager risk check is moved to
                        // immediately before submission and now uses `final_buy_amount`
                        // (the size actually sent after the creator multiplier), so it
                        // cannot authorize a larger send than was checked.

                        // F1: fail-closed halt on unresolved transaction/ownership state.
                        // Existing safe exits (the monitor task) are unaffected.
                        if new_entries_halted.load(Ordering::SeqCst) {
                            warn!(
                                "Skipping {} - HotScan new entries are HALTED (unresolved transaction/position truth)",
                                token.symbol
                            );
                            break;
                        }

                        info!(
                            "AUTO-BUY candidate: {} ({}) score={:.1}",
                            token.symbol,
                            token.mint,
                            token.score()
                        );

                        // POOL READINESS CHECK: Verify pump.fun pool exists before buying
                        if let Some(ref trader) = trader {
                            if !trader.check_pool_ready(&token.mint).await {
                                warn!(
                                    "Skipping {} - pool not ready (may be too new)",
                                    token.symbol
                                );
                                continue;
                            }
                        }

                        // SMART MONEY CHECK: Analyze token creator's past performance
                        let final_buy_amount = if let (Some(ref helius), Some(ref profiler)) = (&helius_client, &wallet_profiler) {
                            match helius.get_token_creator(&token.mint).await {
                                Ok(creator) => {
                                    match profiler.get_or_compute(&creator).await {
                                        Ok(profile) => {
                                            // Check if creator should be avoided
                                            if profile.should_avoid() {
                                                warn!(
                                                    "Skipping {} - creator {} is {:?} (should avoid)",
                                                    token.symbol, &creator[..8], profile.alpha_score.category
                                                );
                                                continue;
                                            }

                                            // Adjust position size based on alpha score
                                            let alpha_multiplier = if profile.is_elite() {
                                                info!(
                                                    "[{}] ELITE creator {} | Win: {:.0}% | R: {:.1}x | Alpha: {:.2} -> 1.5x size",
                                                    token.symbol,
                                                    &creator[..8],
                                                    profile.win_rate * 100.0,
                                                    profile.avg_r_multiple,
                                                    profile.alpha_score.value
                                                );
                                                1.5 // 50% more for elite wallets
                                            } else if profile.win_rate >= 0.5 {
                                                info!(
                                                    "[{}] Good creator {} | Win: {:.0}% | Alpha: {:.2} -> 1.0x size",
                                                    token.symbol, &creator[..8], profile.win_rate * 100.0, profile.alpha_score.value
                                                );
                                                1.0 // Normal for decent wallets
                                            } else {
                                                info!(
                                                    "[{}] Weak creator {} | Win: {:.0}% | Alpha: {:.2} -> 0.7x size",
                                                    token.symbol, &creator[..8], profile.win_rate * 100.0, profile.alpha_score.value
                                                );
                                                0.7 // 30% less for weak wallets
                                            };

                                            buy_amount * alpha_multiplier
                                        }
                                        Err(e) => {
                                            warn!("Could not profile creator for {}: {} - using default size", token.symbol, e);
                                            buy_amount
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Could not get creator for {}: {} - using default size", token.symbol, e);
                                    buy_amount
                                }
                            }
                        } else {
                            buy_amount // No profiler, use default
                        };

                        if dry_run {
                            warn!(
                                "DRY-RUN: Would buy {:.4} SOL of {}",
                                final_buy_amount, token.symbol
                            );
                            bought.insert(token.mint.clone(), chrono::Utc::now().timestamp());
                            // Persist bought_mints to disk (with timestamps)
                            persist_bought_mints(&*bought_mints_path, &*bought);
                            continue;
                        }

                        {
                            let slippage = config.trading.slippage_bps / 100;
                            let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                            // Select the local signer for this trade (multi-wallet or
                            // single). This is only the LOCAL signing authority; in
                            // Lightning mode the execution wallet is NOT this keypair.
                            let (trading_keypair, wallet_name) = if let Some(ref mw) = multi_wallet {
                                let selected = mw.select_wallet(&rpc_client);
                                let name = selected.name.clone();
                                let kp = std::sync::Arc::new(
                                    solana_sdk::signature::Keypair::from_bytes(&selected.keypair.to_bytes()).unwrap()
                                );
                                (kp, name)
                            } else {
                                (keypair.clone(), "default".to_string())
                            };

                            // F4: resolve the EXACT execution wallet. Local => selected
                            // signer's Pubkey. Lightning => exact configured Lightning
                            // wallet (never a MultiWallet keypair). No fallback.
                            let use_lightning = !use_local_api;
                            let execution_wallet = match hotscan_execution_wallet(
                                use_lightning,
                                trading_keypair.pubkey(),
                                hotscan_lightning_wallet,
                            ) {
                                Ok(pk) => pk,
                                Err(e) => {
                                    new_entries_halted.store(true, Ordering::SeqCst);
                                    error!(
                                        "STRUCTURAL: cannot resolve HotScan execution wallet for {}: {} - halting new entries",
                                        token.symbol, e
                                    );
                                    break;
                                }
                            };
                            let wallet_string = execution_wallet.to_string();

                            // F3: pre-send PositionManager risk check using the FINAL
                            // amount actually being sent (after the creator multiplier),
                            // immediately before submission.
                            let risk_amount = hotscan_risk_check_amount(final_buy_amount);
                            if let Err(e) = position_manager.can_open_position(risk_amount).await {
                                warn!(
                                    "Cannot open position for {} at final size {:.4} SOL: {} - stopping buy loop",
                                    token.symbol, risk_amount, e
                                );
                                break; // Stop trying to buy more tokens
                            }

                            // F4: pick the route-capable trader handle explicitly.
                            let route_trader = if use_lightning {
                                match hotscan_lightning_trader {
                                    Some(ref t) => t,
                                    None => {
                                        new_entries_halted.store(true, Ordering::SeqCst);
                                        error!(
                                            "STRUCTURAL: Lightning route active for {} but no Lightning trader (API key empty) - halting new entries",
                                            token.symbol
                                        );
                                        break;
                                    }
                                }
                            } else {
                                match hotscan_local_trader {
                                    Some(ref t) => t,
                                    None => {
                                        warn!(
                                            "PumpPortal trading disabled - skipping buy for {}",
                                            token.symbol
                                        );
                                        break;
                                    }
                                }
                            };

                            info!(
                                "Buying {:.4} SOL of {} via {} (execution wallet: {})",
                                final_buy_amount,
                                token.symbol,
                                if use_lightning { "Lightning" } else { "Local API" },
                                if use_lightning {
                                    wallet_string.clone()
                                } else {
                                    format!("{} ({})", wallet_string, wallet_name)
                                }
                            );

                            // === MPT-001 Agent G: authoritative market-admission gate ===
                            // Immediately before the LIVE auto-buy submit, fetch a fresh,
                            // exact-size, same-venue executable quote. The DexScreener fields
                            // used above (candidate filtering / momentum ranking / display) can
                            // NEVER substitute for this quote (G1). A quote error is a
                            // market-admission failure (UnsupportedQuoteMint / no curve-or-pool
                            // MarketData), NOT a fill-rate failure: no transaction, no
                            // ExecutionRecord failure, no failed_mints blacklist, no pending —
                            // just skip this candidate (G2).
                            let exact_lamports = match sol_to_lamports_exact(final_buy_amount) {
                                Some(l) => l,
                                None => {
                                    warn!(
                                        "Market gate: rejecting HotScan buy of {} - unrepresentable SOL size {} for lamport quote",
                                        token.symbol, final_buy_amount
                                    );
                                    continue;
                                }
                            };
                            let mint_pubkey = match Pubkey::from_str(&token.mint) {
                                Ok(pk) => pk,
                                Err(e) => {
                                    warn!(
                                        "Market gate: rejecting HotScan buy of {} - invalid mint {}: {}",
                                        token.symbol, token.mint, e
                                    );
                                    continue;
                                }
                            };
                            let buy_quote_result = market_oracle
                                .quote_buy_sol(&mint_pubkey, exact_lamports)
                                .await;
                            // Pure, network-free classification of the quote result into a
                            // route-pinned submit decision or a no-submit market skip (G2/G3).
                            let buy_pool = match hotscan_buy_decision(&buy_quote_result) {
                                HotScanBuyDecision::Submit(pool) => pool,
                                HotScanBuyDecision::SkipMarketUnsupported => {
                                    // market unsupported: not a transaction failure.
                                    if let Err(e) = buy_quote_result {
                                        warn!(
                                            "Market gate: market unsupported for HotScan buy of {} ({} lamports): {} - skipping (no transaction submitted, not blacklisted)",
                                            token.symbol, exact_lamports, e
                                        );
                                    }
                                    continue;
                                }
                            };
                            // Safe: Submit(_) implies the quote was Ok.
                            let buy_quote = buy_quote_result.expect("submit decision implies Ok quote");
                            info!(
                                "Market gate PASSED for HotScan buy of {}: venue={:?} pool={:?} expected_base_raw={} expected_price={:?} quote_slot={}",
                                token.symbol,
                                buy_quote.venue,
                                buy_pool,
                                buy_quote.base_amount_raw,
                                buy_quote.expected_price_sol_per_token,
                                buy_quote.slot
                            );

                            // MPT-001 Agent G3: route-pinned to the quoted venue (no Auto). Same
                            // mint / final SOL amount / configured slippage+priority as before.
                            let buy_start = std::time::Instant::now();
                            let buy_result = if use_local_api {
                                route_trader
                                    .buy_local_with_pool(
                                        &token.mint,
                                        final_buy_amount,
                                        slippage,
                                        priority_fee,
                                        &trading_keypair,
                                        &rpc_client,
                                        buy_pool,
                                    )
                                    .await
                            } else {
                                route_trader
                                    .buy_with_pool(
                                        &token.mint,
                                        final_buy_amount,
                                        slippage,
                                        priority_fee,
                                        buy_pool,
                                    )
                                    .await
                            };

                            match buy_result {
                                Err(e) => {
                                    error!("BUY FAILED to submit for {}: {}", token.symbol, e);
                                    continue;
                                }
                                Ok(signature) => {
                                    // A returned signature is submission identity, NOT
                                    // fill proof (INV-TX-001). Do not call this executed.
                                    info!(
                                        "BUY SUBMITTED: {} - signature {}",
                                        token.symbol, signature
                                    );
                                    info!("View on Solscan: https://solscan.io/tx/{}", signature);

                                    // Persist the submitted signature BEFORE treating it as
                                    // filled. Do NOT add bought_mints yet.
                                    let pending_buy = PendingExecution::buy(
                                        signature.clone(),
                                        token.mint.clone(),
                                        wallet_string.clone(),
                                        PendingBuyContext {
                                            name: token.name.clone(),
                                            symbol: token.symbol.clone(),
                                            // Not available from DexScreener; stays empty.
                                            bonding_curve: String::new(),
                                            entry_type:
                                                crate::position::manager::EntryType::Opportunity,
                                            requested_sol: final_buy_amount,
                                        },
                                    );
                                    // AUDIT-002 A7: retain the exact pending buy + whether the
                                    // first journal write persisted.
                                    let pending_buy_persisted = match pending_executions.upsert(pending_buy.clone()).await {
                                        Ok(()) => true,
                                        Err(e) => {
                                            // The tx was already sent. Halt new entries, still
                                            // attempt reconciliation, never send another buy.
                                            new_entries_halted.store(true, Ordering::SeqCst);
                                            error!(
                                                "Failed to persist pending HotScan buy for {} (sig {}): {} - halting new entries; still reconciling",
                                                token.symbol, signature, e
                                            );
                                            false
                                        }
                                    };

                                    // Reconcile the submitted signature. No sleep.
                                    let outcome = trade_reconciler
                                        .reconcile(
                                            &signature,
                                            &wallet_string,
                                            &token.mint,
                                            ReconciliationSide::Buy,
                                        )
                                        .await;

                                    match outcome {
                                        Ok(ReconciliationOutcome::ConfirmedFailure {
                                            error,
                                            observed_after_ms,
                                            ..
                                        }) => {
                                            // Real on-chain failure: remove pending, no
                                            // position, add failed_mints cooldown, do NOT
                                            // add bought_mints.
                                            if let Err(e) =
                                                pending_executions.remove(&signature).await
                                            {
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "Failed to remove pending HotScan buy after ConfirmedFailure (sig {}): {} - halting new entries",
                                                    signature, e
                                                );
                                            }
                                            error!(
                                                "BUY CONFIRMED FAILED for {} (sig {}): {} ({}ms observed)",
                                                token.symbol, signature, error, observed_after_ms
                                            );
                                            {
                                                let mut failed = failed_mints.lock().await;
                                                failed.insert(
                                                    token.mint.clone(),
                                                    chrono::Utc::now().timestamp(),
                                                );
                                                info!(
                                                    "[{}] Added to failed_mints blacklist after confirmed failure ({}min cooldown)",
                                                    token.symbol,
                                                    FAILED_MINTS_COOLDOWN_SECS / 60
                                                );
                                            }
                                            continue;
                                        }
                                        Ok(ReconciliationOutcome::Unresolved { reason, .. }) => {
                                            // Ambiguous outcome (timeout/observation gap) is
                                            // NOT a failed fill. Keep pending, halt new
                                            // entries, no position, no failed_mints.
                                            new_entries_halted.store(true, Ordering::SeqCst);
                                            // AUDIT-002 A7: retry durability before break.
                                            if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                error!(
                                                    "BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept (durable), HotScan new entries HALTED",
                                                    token.mint, signature, wallet_string, reason
                                                );
                                            } else {
                                                error!(
                                                    "CRITICAL: BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. HotScan new entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                    token.mint, signature, wallet_string, reason, signature
                                                );
                                            }
                                            break;
                                        }
                                        Err(e) => {
                                            // Structural observer failure is not tx-failure
                                            // proof. Keep pending, halt, no failed_mints.
                                            new_entries_halted.store(true, Ordering::SeqCst);
                                            // AUDIT-002 A7: same durability retry rule as Unresolved.
                                            if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                error!(
                                                    "CRITICAL: HotScan buy reconciliation error for {} (sig {}): {} - pending kept (durable), new entries HALTED",
                                                    token.symbol, signature, e
                                                );
                                            } else {
                                                error!(
                                                    "CRITICAL: HotScan buy reconciliation error for {} (sig {}): {} - AND pending journal is NOT durable; restart recovery is NOT guaranteed until persistence succeeds. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                    token.symbol, signature, e, signature
                                                );
                                            }
                                            break;
                                        }
                                        Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                            // Validate exact identity at the live boundary.
                                            if fill.side != ReconciliationSide::Buy
                                                || fill.wallet != wallet_string
                                                || fill.mint != token.mint
                                            {
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                // AUDIT-002 A7: confirmed-but-unapplied. Retry durability.
                                                if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                    error!(
                                                        "CRITICAL: reconciled HotScan buy fill identity mismatch for sig {} (wallet/mint/side) - pending kept (durable), new entries HALTED",
                                                        signature
                                                    );
                                                } else {
                                                    error!(
                                                        "CRITICAL: reconciled HotScan buy fill identity mismatch for sig {} (wallet/mint/side) - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                        signature, signature
                                                    );
                                                }
                                                break;
                                            }

                                            // Extract canonical fill economics.
                                            let (
                                                token_amount_raw,
                                                _decimals,
                                                actual_cost_sol,
                                                actual_entry_price,
                                            ) = match primary_buy_fill_values(&fill) {
                                                Ok(v) => v,
                                                Err(e) => {
                                                    new_entries_halted
                                                        .store(true, Ordering::SeqCst);
                                                    // AUDIT-002 A7: confirmed-but-unapplied. Retry durability.
                                                    if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                        error!(
                                                            "CRITICAL: reconciled HotScan buy fill conversion failed for sig {}: {} - pending kept (durable), new entries HALTED",
                                                            signature, e
                                                        );
                                                    } else {
                                                        error!(
                                                            "CRITICAL: reconciled HotScan buy fill conversion failed for sig {}: {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                            signature, e, signature
                                                        );
                                                    }
                                                    break;
                                                }
                                            };

                                            let entry_time = fill
                                                .block_time
                                                .and_then(|ts| {
                                                    chrono::DateTime::<chrono::Utc>::from_timestamp(
                                                        ts, 0,
                                                    )
                                                })
                                                .unwrap_or_else(chrono::Utc::now);

                                            info!(
                                                "BUY CONFIRMED: {} (sig {}) raw_tokens={} decimals={} cost={:.9} SOL price={:.12} SOL/token",
                                                token.symbol,
                                                signature,
                                                token_amount_raw,
                                                fill.token_decimals,
                                                actual_cost_sol,
                                                actual_entry_price
                                            );

                                            // Canonical confirmed-owned position from
                                            // actuals. bonding_curve empty (unavailable).
                                            let position = crate::position::manager::Position {
                                                mint: token.mint.clone(),
                                                name: token.name.clone(),
                                                symbol: token.symbol.clone(),
                                                bonding_curve: String::new(),
                                                token_amount: token_amount_raw,
                                                token_decimals: Some(fill.token_decimals),
                                                entry_price: actual_entry_price,
                                                total_cost_sol: actual_cost_sol,
                                                entry_time,
                                                entry_signature: fill.signature.clone(),
                                                entry_type:
                                                    crate::position::manager::EntryType::Opportunity,
                                                quick_profit_taken: false,
                                                second_profit_taken: false,
                                                peak_price: actual_entry_price,
                                                current_price: actual_entry_price,
                                                kill_switch_triggered: false,
                                                kill_switch_reason: None,
                                                wallet_pubkey: fill.wallet.clone(),
                                                applied_exit_signatures: vec![],
                                            };

                                            // Record confirmed ownership. NOT open_position.
                                            match position_manager
                                                .record_confirmed_position(position)
                                                .await
                                            {
                                                Ok(_newly_applied) => {}
                                                Err(e) => {
                                                    // Confirmed on-chain but could not
                                                    // record. Keep pending, halt; do not
                                                    // add bought_mints.
                                                    new_entries_halted
                                                        .store(true, Ordering::SeqCst);
                                                    // AUDIT-002 A7: confirmed-but-unapplied. Retry durability.
                                                    if retry_pending_durability_if_needed(&pending_executions, &pending_buy, pending_buy_persisted).await {
                                                        error!(
                                                            "Confirmed owned HotScan position could not be recorded for {} (sig {}): {} - pending kept (durable), new entries HALTED",
                                                            token.symbol, signature, e
                                                        );
                                                    } else {
                                                        error!(
                                                            "CRITICAL: confirmed owned HotScan position could not be recorded for {} (sig {}): {} - confirmed fill is unapplied AND pending journal is NOT durable; restart recovery is NOT guaranteed. New entries HALTED. Do NOT resubmit; preserve and investigate signature {}.",
                                                            token.symbol, signature, e, signature
                                                        );
                                                    }
                                                    break;
                                                }
                                            }

                                            // Only AFTER confirmed state: add bought_mints.
                                            bought.insert(
                                                token.mint.clone(),
                                                chrono::Utc::now().timestamp(),
                                            );
                                            persist_bought_mints(&*bought_mints_path, &*bought);

                                            // === SET UP KILL-SWITCH MONITORING ===
                                            if let Some(ref evaluator) = kill_switch_evaluator {
                                                if let Some(ref helius) = helius_client {
                                                    let creator = match helius
                                                        .get_token_creator(&token.mint)
                                                        .await
                                                    {
                                                        Ok(c) => {
                                                            info!("[{}] Creator for kill-switch: {}", token.symbol, &c[..8]);
                                                            c
                                                        }
                                                        Err(e) => {
                                                            warn!("[{}] Could not get creator: {} - using empty", token.symbol, e);
                                                            String::new()
                                                        }
                                                    };

                                                    let holders = match helius
                                                        .get_token_holders(&token.mint, 10)
                                                        .await
                                                    {
                                                        Ok(h) => {
                                                            info!("[{}] Fetched {} top holders for kill-switch monitoring", token.symbol, h.len());
                                                            h.into_iter()
                                                                .map(|hi| (hi.address, hi.amount, hi.percentage))
                                                                .collect::<Vec<_>>()
                                                        }
                                                        Err(e) => {
                                                            warn!("[{}] Could not get holders: {} - monitoring creator only", token.symbol, e);
                                                            vec![]
                                                        }
                                                    };

                                                    evaluator.watch_position(
                                                        &token.mint,
                                                        &creator,
                                                        holders,
                                                    );
                                                    info!(
                                                        "[{}] Kill-switch monitoring ACTIVE (creator: {}, holders: tracked)",
                                                        token.symbol,
                                                        if creator.is_empty() { "unknown" } else { &creator[..8] }
                                                    );
                                                }
                                            }

                                            // Remove pending LAST, after all state applied.
                                            if let Err(e) =
                                                pending_executions.remove(&signature).await
                                            {
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "Failed to remove pending HotScan buy after confirmed fill (sig {}): {} - halting new entries; position retained",
                                                    signature, e
                                                );
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

        if !watch {
            break;
        }

        // === TELEMETRY & MONITORING ===
        let positions = position_manager.get_all_positions().await;
        let daily_stats = position_manager.get_daily_stats().await;

        // Session Stats Summary
        println!("\n{:=<80}", "");
        println!("SESSION STATS:");
        println!(
            "  Total Trades: {} | Open Positions: {}",
            daily_stats.total_trades,
            positions.len()
        );
        println!(
            "  Wins: {} | Losses: {} | Win Rate: {:.1}%",
            daily_stats.winning_trades,
            daily_stats.losing_trades,
            daily_stats.win_rate()
        );
        println!(
            "  Profit: {:.4} SOL | Loss: {:.4} SOL | Net P&L: {:.4} SOL",
            daily_stats.total_profit_sol, daily_stats.total_loss_sol, daily_stats.net_pnl_sol
        );

        // Position Details
        if !positions.is_empty() {
            println!("\n--- Open Positions: {} ---", positions.len());
            let mut total_unrealized = 0.0;
            for pos in &positions {
                let hold_time = (chrono::Utc::now() - pos.entry_time).num_seconds();
                let pnl_pct = pos.unrealized_pnl_pct();
                total_unrealized += pos.unrealized_pnl();
                println!(
                    "  {} | Entry: {:.10} | P&L: {:+.1}% | Hold: {}s | TP: {:.0}% SL: -{:.0}%",
                    pos.symbol,
                    pos.entry_price,
                    pnl_pct,
                    hold_time,
                    pos.entry_type.take_profit_pct(),
                    pos.entry_type.stop_loss_pct()
                );
            }
            println!("  Total Unrealized P&L: {:+.4} SOL", total_unrealized);
        }

        // Remaining capacity
        let remaining_capacity = position_manager.remaining_position_capacity().await;
        let remaining_loss = position_manager.remaining_daily_loss().await;
        println!(
            "\n  Remaining Position Capacity: {:.4} SOL",
            remaining_capacity
        );
        println!("  Remaining Daily Loss Buffer: {:.4} SOL", remaining_loss);
        println!("{:=<80}", "");

        info!("Next scan in {} seconds...", interval);
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }

    Ok(())
}

/// Strategy entry policy in one place: ONLY an explicit `Enter` authorizes a buy.
/// Every other `TradingAction` (Hold, Skip, FatalReject, Exit, Pause) is an
/// abstention and returns `None`.
fn strategy_entry_size(action: &TradingAction) -> Option<f64> {
    match action {
        TradingAction::Enter { size_sol, .. } => Some(*size_sol),
        _ => None,
    }
}

/// Pure predicate: does this loaded position lack canonical ownership state and
/// therefore require recovery before NEW entries may resume? True when
/// `token_decimals` is unknown, the wallet pubkey is empty, or the wallet pubkey
/// does not parse as a Solana `Pubkey`. Does not mutate the position.
fn position_requires_recovery(position: &crate::position::manager::Position) -> bool {
    position.token_decimals.is_none()
        || position.wallet_pubkey.is_empty()
        || position
            .wallet_pubkey
            .parse::<solana_sdk::pubkey::Pubkey>()
            .is_err()
}

/// Pure resolver for the exact HotScan BUY execution wallet (F4).
///
/// - Local active mode: the execution wallet is the selected local signer's
///   Pubkey (a selected MultiWallet signer or the primary keypair). The signer
///   is chosen by the caller; we simply echo its identity.
/// - Lightning active mode: the execution wallet MUST be the exact configured
///   Lightning wallet. A MultiWallet keypair is NEVER used as the Lightning
///   execution wallet, because PumpPortal Lightning owns the execution wallet
///   (INV-WALLET-001). No fallback (INV-WALLET-002): an active Lightning route
///   without a configured Lightning wallet is a hard error.
fn hotscan_execution_wallet(
    use_lightning: bool,
    selected_local_signer: Pubkey,
    configured_lightning_wallet: Option<Pubkey>,
) -> crate::error::Result<Pubkey> {
    use crate::error::Error;
    if use_lightning {
        configured_lightning_wallet.ok_or_else(|| {
            Error::TransactionReconciliation(
                "hotscan_execution_wallet: Lightning route active but no configured Lightning wallet"
                    .to_string(),
            )
        })
    } else {
        Ok(selected_local_signer)
    }
}

/// Pure helper: the amount that the HotScan pre-send risk check must use is the
/// FINAL buy amount (after the creator multiplier), NOT the base amount (F3/F7).
fn hotscan_risk_check_amount(final_buy_amount: f64) -> f64 {
    final_buy_amount
}

/// MPT-001 Agent G: the pre-send decision for a HotScan auto-BUY after the fresh
/// executable market quote (G1/G2/G3). A scanner (DexScreener) price can NEVER
/// become this decision — only the on-chain quote result can. `Submit` carries the
/// route-pinned pool (Pump=>Pump, PumpSwap=>PumpAmm) derived from the quote venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotScanBuyDecision {
    /// The market quote succeeded: submit the buy pinned to this pool (never Auto).
    Submit(PoolType),
    /// The market quote failed (UnsupportedQuoteMint / MarketData). No transaction
    /// is submitted and this is NOT a fill-rate/transaction failure, so the mint is
    /// NOT blacklisted and no ExecutionRecord failure is recorded (G2).
    SkipMarketUnsupported,
}

/// Pure classifier mapping a fresh `quote_buy_sol` result to a HotScan buy
/// decision (G2/G3), extracted so the gate is testable without any network. On a
/// successful quote the venue is route-pinned via `pumpportal_pool_for_venue`; on
/// any quote error the decision is a no-submit market-admission skip.
fn hotscan_buy_decision(
    quote: &crate::error::Result<crate::market::ExecutableQuote>,
) -> HotScanBuyDecision {
    match quote {
        Ok(q) => HotScanBuyDecision::Submit(pumpportal_pool_for_venue(q.venue)),
        Err(_) => HotScanBuyDecision::SkipMarketUnsupported,
    }
}

/// Pure fill-validation boundary for the primary buy path. Extracts the canonical
/// economics from a reconciled BUY fill without mutating any state.
///
/// Returns `(raw_tokens, decimals, positive_actual_cost_sol, positive_actual_price)`.
/// The actual cost is `-wallet_sol_delta_sol()` (a buy spends SOL). The fee is NOT
/// added again — the wallet SOL delta already includes all economic effects.
fn primary_buy_fill_values(
    fill: &crate::trading::ReconciledFill,
) -> crate::error::Result<(u64, u8, f64, f64)> {
    use crate::error::Error;

    if fill.side != ReconciliationSide::Buy {
        return Err(Error::TransactionReconciliation(
            "primary_buy_fill_values: fill side is not Buy".to_string(),
        ));
    }

    let token_amount_raw = fill.token_amount_raw().ok_or_else(|| {
        Error::TransactionReconciliation(
            "primary_buy_fill_values: raw token amount does not fit in u64".to_string(),
        )
    })?;
    if token_amount_raw == 0 {
        return Err(Error::TransactionReconciliation(
            "primary_buy_fill_values: raw token amount is zero".to_string(),
        ));
    }

    if fill.token_amount_ui() <= 0.0 {
        return Err(Error::TransactionReconciliation(
            "primary_buy_fill_values: UI token amount is not positive".to_string(),
        ));
    }

    let wallet_delta = fill.wallet_sol_delta_sol();
    if !(wallet_delta < 0.0) {
        return Err(Error::TransactionReconciliation(
            "primary_buy_fill_values: buy wallet SOL delta is not negative".to_string(),
        ));
    }
    let actual_cost_sol = -wallet_delta;

    let actual_entry_price = fill.effective_price_sol_per_token().ok_or_else(|| {
        Error::TransactionReconciliation(
            "primary_buy_fill_values: effective price unavailable".to_string(),
        )
    })?;
    if !actual_entry_price.is_finite() || actual_entry_price <= 0.0 {
        return Err(Error::TransactionReconciliation(
            "primary_buy_fill_values: effective price is not finite/positive".to_string(),
        ));
    }

    Ok((token_amount_raw, fill.token_decimals, actual_cost_sol, actual_entry_price))
}

/// Pure fill-validation boundary for the primary auto-sell path. Extracts the
/// canonical sell economics from a reconciled SELL fill without mutating state.
///
/// Returns `(actual_sold_raw, net_received_sol, actual_exit_price)`.
///
/// Validation:
/// - fill side is Sell;
/// - fill decimals exactly match the position's confirmed decimals;
/// - raw token amount is nonzero and fits in u64;
/// - net wallet SOL delta is finite (NEGATIVE is allowed — a fee-dominated sale);
/// - effective price is finite and positive;
/// - sold raw amount does NOT exceed the tracked position amount (INV-POS-011),
///   rejected BEFORE any position mutation.
fn primary_sell_fill_values(
    fill: &crate::trading::ReconciledFill,
    position: &crate::position::manager::Position,
) -> crate::error::Result<(u64, f64, f64)> {
    use crate::error::Error;

    if fill.side != ReconciliationSide::Sell {
        return Err(Error::TransactionReconciliation(
            "primary_sell_fill_values: fill side is not Sell".to_string(),
        ));
    }

    match position.token_decimals {
        Some(d) if d == fill.token_decimals => {}
        Some(d) => {
            return Err(Error::TransactionReconciliation(format!(
                "primary_sell_fill_values: decimals mismatch (position {} != fill {})",
                d, fill.token_decimals
            )));
        }
        None => {
            return Err(Error::TransactionReconciliation(
                "primary_sell_fill_values: position has unknown token decimals".to_string(),
            ));
        }
    }

    let actual_sold_raw = fill.token_amount_raw().ok_or_else(|| {
        Error::TransactionReconciliation(
            "primary_sell_fill_values: raw token amount does not fit in u64".to_string(),
        )
    })?;
    if actual_sold_raw == 0 {
        return Err(Error::TransactionReconciliation(
            "primary_sell_fill_values: raw token amount is zero".to_string(),
        ));
    }

    // INV-POS-011: never subtract more raw tokens than the position tracks.
    // Rejected here, before any mutation.
    if actual_sold_raw > position.token_amount {
        return Err(Error::TransactionReconciliation(format!(
            "primary_sell_fill_values: oversell ({} sold > {} tracked)",
            actual_sold_raw, position.token_amount
        )));
    }

    let actual_received_sol = fill.wallet_sol_delta_sol();
    if !actual_received_sol.is_finite() {
        return Err(Error::TransactionReconciliation(
            "primary_sell_fill_values: net SOL delta is not finite".to_string(),
        ));
    }

    // Per §62/§63 the sell exit price need only be FINITE; a fee-dominated confirmed
    // sale can produce a zero/negative economic price and must NOT be rejected here.
    let actual_exit_price = fill.effective_price_sol_per_token().ok_or_else(|| {
        Error::TransactionReconciliation(
            "primary_sell_fill_values: effective price unavailable".to_string(),
        )
    })?;
    if !actual_exit_price.is_finite() {
        return Err(Error::TransactionReconciliation(
            "primary_sell_fill_values: effective price is not finite".to_string(),
        ));
    }

    Ok((actual_sold_raw, actual_received_sol, actual_exit_price))
}

/// AGENT E — pure full/partial decision for the primary event kill-switch exit.
///
/// The ACTUAL reconciled close result decides whether the position is fully
/// closed. Only a full close unwatches the kill-switch evaluator; a partial
/// close keeps the evaluator watching the remaining position. No threshold or
/// market-price input is involved.
fn kill_switch_unwatch_on_close(fully_closed: bool) -> bool {
    fully_closed
}

// ===========================================================================
// AGENT E — pure runtime-ownership classifier for CLI commands.
//
// Returns true for commands that mutate persistent trading state / credentials /
// controlled-wallet balances and therefore MUST hold the exclusive runtime lease
// (INV-RUN-001/002). Read-only commands (status/config/health/wallet
// status/list/history) and the emergency-control command (INV-RUN-006) return
// false — emergency must stay callable while a bot holds the lease.
// ===========================================================================
fn command_requires_runtime_lease(command: &str) -> bool {
    matches!(
        command,
        "start"
            | "hot_scan"
            | "sell"
            | "wallet_add"
            | "wallet_extract"
            | "wallet_transfer"
    )
}

/// E8(2): the exact runtime-lease command label used by the manual `sell` handler.
/// Factored so tests assert the label without acquiring a lease.
fn manual_sell_lease_label() -> &'static str {
    "sell"
}

// ===========================================================================
// AGENT D — pure decision helpers for authenticated position-scoped events.
// These carry no I/O so the D13 tests need no socket / network.
// ===========================================================================

/// D3: build the initial PumpPortal subscription plan for a `start()` runtime.
///
/// - `new_tokens` and `migrations` are always requested (free streams).
/// - `account_trades` = configured tracked wallets, but ONLY when wallet
///   tracking is enabled (otherwise empty). Never "all trades".
/// - `token_trades` = every currently-open canonical Position mint.
///
/// The client validates/deduplicates pubkeys; we still de-duplicate here so the
/// plan is minimal and never subscribes an all-trades abstraction.
fn build_initial_subscription_plan(
    open_position_mints: &[String],
    tracked_wallets: &[String],
    wallet_tracking_enabled: bool,
) -> PumpPortalSubscriptionPlan {
    fn dedup(input: &[String]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for k in input {
            let k = k.trim();
            if k.is_empty() {
                continue;
            }
            if seen.insert(k.to_string()) {
                out.push(k.to_string());
            }
        }
        out
    }

    let account_trades = if wallet_tracking_enabled {
        dedup(tracked_wallets)
    } else {
        Vec::new()
    };

    PumpPortalSubscriptionPlan {
        new_tokens: true,
        migrations: true,
        token_trades: dedup(open_position_mints),
        account_trades,
    }
}

/// D12: pure new-entry admission predicate.
///
/// When the PumpPortal feed is enabled, a NEW live entry is admitted only when
/// entries are not halted AND the data stream is ready (Connected). When the
/// feed is disabled, data-stream readiness is not a gate. Exits are NEVER routed
/// through this predicate.
fn new_entry_admitted(new_entries_halted: bool, data_stream_ready: bool, feed_enabled: bool) -> bool {
    if new_entries_halted {
        return false;
    }
    if feed_enabled {
        data_stream_ready
    } else {
        true
    }
}

/// D4: whether NEW entries must be halted at startup purely because a live run
/// has the PumpPortal feed enabled but no Data API key. A missing key means no
/// token/account trade subscription can be opened, so any future position could
/// not get its provider trade kill-switch coverage — but existing-position price
/// monitoring and exits stay available. `force_local_api` does NOT bypass this.
fn missing_data_key_halts_new_entries(
    dry_run: bool,
    pumpportal_enabled: bool,
    api_key: &str,
) -> bool {
    !dry_run && pumpportal_enabled && api_key.trim().is_empty()
}

/// D6: whether a confirmed, durably-recorded position requires a dynamic
/// token-trade subscription. It does whenever the feed is enabled and a Data API
/// key is configured (trade streams are authenticated). Without a key we cannot
/// subscribe; the caller then halts new entries and keeps price monitoring.
fn confirmed_position_requires_subscription(feed_enabled: bool, api_key: &str) -> bool {
    feed_enabled && !api_key.trim().is_empty()
}

/// C5: whether a confirmed durable buy must close the new-entry readiness gate
/// before requesting its required token-trade subscription. This is exactly the
/// case where a dynamic subscription is required (feed enabled + Data API key
/// configured), i.e. `confirmed_position_requires_subscription`. When true, the
/// caller stores `data_stream_ready = false` immediately BEFORE sending
/// `SubscribeTokenTrades([mint])`; readiness is only reopened when the client
/// re-emits `Connected` after the desired registry is actually synchronized. This
/// prevents a second live entry while the first owned position's provider trade
/// subscription is not yet on the wire. If the send is rejected, readiness stays
/// false and new entries stay halted (the caller never sets readiness true locally).
fn confirmed_buy_closes_readiness_until_sync(feed_enabled: bool, api_key: &str) -> bool {
    confirmed_position_requires_subscription(feed_enabled, api_key)
}

/// D8: full close requests an unsubscribe; a partial close keeps the
/// subscription. Actual reconciled `fully_closed` controls.
fn full_close_requests_unsubscribe(fully_closed: bool) -> bool {
    fully_closed
}

/// D6/D8: best-effort send of a single subscription command on the runtime's one
/// command sender. Returns true iff the command was accepted by the channel. A
/// failure NEVER alters economic position truth — callers decide policy (D6
/// halts new entries on a required subscribe failure; D8 only logs).
async fn send_subscription_command(
    sender: &Option<CommandSender>,
    cmd: SubscriptionCommand,
) -> bool {
    match sender {
        Some(tx) => tx.send(cmd).await.is_ok(),
        None => false,
    }
}

/// AGENT G — pure mapping of a HotScan requested layer string to the durable
/// `PendingSellIntent` (G5). "50%" => QuickProfit, "25%" => SecondProfit, and any
/// full/"100%"/other request => Full. This does NOT change the requested amount
/// that is actually submitted; reconciliation accounts what was ACTUALLY sold.
fn hotscan_sell_intent_for_layer(sell_pct: &str) -> PendingSellIntent {
    match sell_pct {
        "50%" => PendingSellIntent::QuickProfit,
        "25%" => PendingSellIntent::SecondProfit,
        _ => PendingSellIntent::Full,
    }
}

/// AGENT G — pure full-exit cache/cooldown decision (G8). The bought-mint cache
/// is removed and the sold-mint cooldown is added ONLY when the ACTUAL reconciled
/// close fully closed the position. A requested "100%" that only partially fills
/// must NOT remove the bought-mint or mark a full-exit cooldown. Actual fill
/// controls; the requested amount is irrelevant here.
fn hotscan_full_exit_removes_cache(fully_closed: bool) -> bool {
    fully_closed
}

/// AGENT G — the exact-route action chosen for a HotScan exit (G4). Mirrors the
/// live routing decision so it can be tested purely. A `Local` position resolves
/// to the Local trader with an exact signer (primary or recovery multi-wallet); a
/// `Lightning` position resolves to the Lightning trader ONLY. There is NO
/// Lightning->Local fallback, NO primary-signer fallback, and an unknown/absent
/// route or missing signer yields `NoRoute` (no sell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotScanSellAction {
    LocalPrimary,
    LocalRecovery,
    Lightning,
    NoRoute,
}

/// Pure classifier for the HotScan exit route (G4).
///
/// - `route` is the registry route for the position's EXACT recorded wallet.
/// - `local_trader_available` / `lightning_trader_available` reflect which
///   route-capable trader handles exist.
/// - `primary_signer_matches` is true iff the primary local keypair's pubkey is
///   the position wallet.
/// - `recovery_signer_present` is true iff the recovery MultiWalletManager owns an
///   exact signer for the position wallet.
///
/// A Lightning route NEVER maps to a Local action (no fallback), and a Local route
/// NEVER maps to Lightning.
fn hotscan_sell_action(
    route: crate::wallet::ExecutionRoute,
    local_trader_available: bool,
    lightning_trader_available: bool,
    primary_signer_matches: bool,
    recovery_signer_present: bool,
) -> HotScanSellAction {
    match route {
        crate::wallet::ExecutionRoute::Local => {
            if !local_trader_available {
                HotScanSellAction::NoRoute
            } else if primary_signer_matches {
                HotScanSellAction::LocalPrimary
            } else if recovery_signer_present {
                HotScanSellAction::LocalRecovery
            } else {
                HotScanSellAction::NoRoute
            }
        }
        crate::wallet::ExecutionRoute::Lightning => {
            if lightning_trader_available {
                HotScanSellAction::Lightning
            } else {
                HotScanSellAction::NoRoute
            }
        }
    }
}

// ===========================================================================
// MPT-001 Agent H — HotScan price-exit market-truth pure decision
// ===========================================================================

/// The exact-size executable sell quote as consumed by the HotScan exit
/// authorizer (H3). A price exit is authorized ONLY from a usable same-venue SOL
/// quote (this is the ONLY carrier of the executable price + pinned venue);
/// DexScreener / stale `current_price` / the mark can never appear here.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HotScanExecQuote {
    /// Executable expected SOL/token price at the quoted exact size. This is the
    /// price the exit condition is re-confirmed against (F2) — never the mark.
    exec_price_sol_per_token: f64,
    /// Venue the quote came from; the sell is pinned to it (F5), never Auto.
    venue: MarketVenue,
}

/// The authoritative outcome of the HotScan price-exit gate (H1/H2/H3/F2/F5/F8).
#[derive(Debug, Clone, PartialEq)]
enum HotScanExitDecision {
    /// Do not submit any sell this cycle (mark unavailable, quote unavailable for
    /// a price exit, or the executable quote no longer meets the trigger). The
    /// position is kept.
    NoSell,
    /// A quote-confirmed, venue-pinned price exit (or a quoted kill-switch exit):
    /// submit exactly `submit_amount` decimal tokens via `pool`.
    QuotedSell { submit_amount: String, pool: PoolType },
    /// F8 emergency ONLY: kill-switch with no usable executable quote. Submit the
    /// original percentage string via Auto, unquoted, no fabricated price.
    EmergencyUnquoted { submit_amount: String, pool: PoolType },
}

/// Pure HotScan exit authorizer (H3 + F2/F5/F8), extracted so it is unit-testable
/// with no network. It intentionally has NO access to DexScreener or to a stale
/// `current_price`: authorization can only flow from `exec_quote`, the fresh
/// exact-size executable quote.
///
/// - `is_kill_switch`: the trigger was a kill-switch/risk exit (not price-based).
/// - `exit_category`: the price rule that fired against the fresh MARK (None for a
///   kill-switch). Re-confirmed against the EXECUTABLE quote price here.
/// - `exec_quote`: `Some` only when a fresh same-venue SOL quote with a usable
///   price exists; `None` means no usable executable quote this cycle.
/// - `intended_raw` / `token_decimals`: the exact quoted layer size, formatted for
///   submission via `raw_token_amount_to_decimal_string`.
/// - `emergency_pct`: the original layer string ("100%"/"50%"/"25%") used ONLY for
///   the unquoted kill-switch fallback amount.
fn hotscan_exit_decision(
    is_kill_switch: bool,
    exit_category: Option<PriceExitCategory>,
    exec_quote: Option<HotScanExecQuote>,
    intended_raw: u64,
    token_decimals: u8,
    emergency_pct: &str,
) -> HotScanExitDecision {
    match exec_quote {
        Some(q) => {
            let pool = pumpportal_pool_for_venue(q.venue);
            let amount = raw_token_amount_to_decimal_string(intended_raw, token_decimals);
            if is_kill_switch {
                // Kill-switch with a usable quote: quoted + pinned emergency exit.
                HotScanExitDecision::QuotedSell {
                    submit_amount: amount,
                    pool,
                }
            } else {
                // F2: the trigger must STILL hold against the executable quote
                // price, not merely the mark that identified the candidate.
                let confirmed = exit_category
                    .map(|c| c.confirms_at(q.exec_price_sol_per_token))
                    .unwrap_or(false);
                if confirmed {
                    HotScanExitDecision::QuotedSell {
                        submit_amount: amount,
                        pool,
                    }
                } else {
                    HotScanExitDecision::NoSell
                }
            }
        }
        None => {
            if is_kill_switch {
                // F8: never blocked by an oracle outage. Unquoted Auto route.
                HotScanExitDecision::EmergencyUnquoted {
                    submit_amount: emergency_pct.to_string(),
                    pool: PoolType::Auto,
                }
            } else {
                // Price exit with no usable executable quote: nothing submitted.
                HotScanExitDecision::NoSell
            }
        }
    }
}

// ===========================================================================
// AGENT D — STARTUP RECOVERY + STRATEGY REBUILD (D1-D8)
//
// All helpers below are startup/recovery-only. They NEVER submit transactions;
// they only OBSERVE (through the accepted reconciler / ownership probe) and
// APPLY the resulting deterministic plan to durable state. A pending record is
// only ever removed AFTER its confirmed economic state is durably applied, or
// after a confirmed on-chain failure (INV-REC-002). An observer failure keeps
// pending state (INV-REC-003).
// ===========================================================================

/// Counts summarizing the result of a pending-store startup recovery pass (D2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// Confirmed buys/sells whose economics were durably applied.
    pub recovered: usize,
    /// Pending records dropped because the tx confirmed FAILED on-chain.
    pub confirmed_failures_removed: usize,
    /// Pending records kept because the tx could not be resolved (or a
    /// structural observer error occurred, or a ConfirmedBuy conflicted).
    pub still_unresolved: usize,
    /// ConfirmedBuy records that could not be recorded due to an accounting
    /// conflict (also counted in `still_unresolved`, pending kept).
    pub accounting_conflicts: usize,
}

impl RecoverySummary {
    /// True when no unresolved pending state remains after this pass.
    fn fully_resolved(&self) -> bool {
        self.still_unresolved == 0
    }
}

/// Recover the pending-execution journal against confirmed chain state (D2).
///
/// Uses the Agent-B planner (`reconcile_pending_execution`). NEVER submits a
/// transaction. Per plan:
/// - `ConfirmedFailure` => remove pending, no position mutation, no fake
///   execution feedback on startup.
/// - `ConfirmedBuy` => `record_confirmed_position` (idempotent success is fine),
///   conflict keeps pending + counts unresolved; remove pending LAST.
/// - `ConfirmedSell` => apply exact sold raw + received SOL via
///   `close_position_reconciled`; reapply missing QuickProfit/SecondProfit
///   markers even on idempotent replay; remove pending LAST.
/// - `Unresolved`/structural `Err` => KEEP pending, count unresolved.
///
/// A pending record is NEVER deleted merely because its Position is absent.
async fn recover_pending_store(
    reconciler: &TradeReconciler,
    pending_store: &PendingExecutionStore,
    positions: &crate::position::manager::PositionManager,
) -> crate::error::Result<RecoverySummary> {
    use crate::trading::PendingRecoveryPlan;

    let mut summary = RecoverySummary::default();

    // B1: deterministic recovery order. `all()` is HashMap-backed and returns an
    // arbitrary order; process oldest-submitted first (signature tie-break) so
    // recovery is reproducible.
    let mut pending_items = pending_store.all().await;
    sort_pending_for_recovery(&mut pending_items);

    for pending in pending_items {
        let plan = match reconcile_pending_execution(reconciler, &pending).await {
            Ok(plan) => plan,
            Err(e) => {
                // INV-REC-003: an observer failure cannot erase pending state.
                warn!(
                    "Pending recovery observer error for sig {} (mint {}): {} - keeping pending",
                    pending.signature, pending.mint, e
                );
                summary.still_unresolved += 1;
                continue;
            }
        };

        match plan {
            PendingRecoveryPlan::ConfirmedFailure {
                pending,
                error,
                observed_after_ms,
            } => {
                info!(
                    "Pending {} (mint {}) confirmed FAILED on-chain after {}ms: {} - removing pending, no position mutation",
                    pending.signature, pending.mint, observed_after_ms, error
                );
                // No position mutation; NO fake execution-feedback sample.
                pending_store.remove(&pending.signature).await?;
                summary.confirmed_failures_removed += 1;
            }

            PendingRecoveryPlan::ConfirmedBuy {
                pending, position, ..
            } => {
                match positions.record_confirmed_position(position).await {
                    Ok(_idempotent_or_new) => {
                        // Ok(false) = same signature already present (idempotent
                        // success). Either way the position state is durable;
                        // remove pending LAST.
                        pending_store.remove(&pending.signature).await?;
                        summary.recovered += 1;
                        info!(
                            "Recovered confirmed BUY for mint {} (sig {})",
                            pending.mint, pending.signature
                        );
                    }
                    Err(e) => {
                        // Accounting conflict: keep pending, count unresolved.
                        summary.accounting_conflicts += 1;
                        summary.still_unresolved += 1;
                        error!(
                            "Recovered confirmed BUY for mint {} (sig {}) conflicts with tracked state: {} - keeping pending",
                            pending.mint, pending.signature, e
                        );
                    }
                }
            }

            PendingRecoveryPlan::ConfirmedSell {
                pending,
                fill,
                sold_amount_raw,
                received_sol,
                intent,
                ..
            } => {
                match apply_recovered_sell(
                    positions,
                    &pending,
                    fill.token_decimals,
                    sold_amount_raw,
                    received_sol,
                    intent,
                )
                .await
                {
                    Ok(()) => {
                        pending_store.remove(&pending.signature).await?;
                        summary.recovered += 1;
                    }
                    Err(e) => {
                        summary.still_unresolved += 1;
                        error!(
                            "Recovered confirmed SELL for mint {} (sig {}) could not be applied: {} - keeping pending",
                            pending.mint, pending.signature, e
                        );
                    }
                }
            }

            PendingRecoveryPlan::Unresolved {
                pending,
                reason,
                observed_after_ms,
            } => {
                // INV-TX-003 / INV-REC-002: unresolved stays pending.
                summary.still_unresolved += 1;
                warn!(
                    "Pending {} (mint {}) unresolved after {}ms: {} - keeping pending",
                    pending.signature, pending.mint, observed_after_ms, reason
                );
            }
        }
    }

    Ok(summary)
}

/// Apply a recovered confirmed SELL to durable position state (D2 detail).
///
/// - If a Position exists, validate its decimals against the fill before close.
///   (We validate against the pending Sell context's known decimals only via the
///   position; the fill decimals were already identity-checked by the planner.)
/// - `close_position_reconciled` is idempotent via the durable receipt ledger,
///   so it succeeds even if the Position is absent (full-exit replay).
/// - Reapply a missing QuickProfit/SecondProfit marker when a partial position
///   remains, EVEN if the close result was `already_applied`.
async fn apply_recovered_sell(
    positions: &crate::position::manager::PositionManager,
    pending: &PendingExecution,
    fill_token_decimals: u8,
    sold_amount_raw: u64,
    received_sol: f64,
    intent: PendingSellIntent,
) -> crate::error::Result<()> {
    // B2: if an open Position exists, its confirmed entry decimals MUST equal the
    // recovered sell fill decimals BEFORE we close it. A mismatch or unknown
    // decimals means the tracked cost basis and the on-chain fill disagree; fail
    // closed and KEEP the pending so restart recovery retries (never close on
    // ambiguous accounting). If the Position is absent, the durable full-exit
    // receipt replay is allowed (close_position_reconciled is idempotent).
    if let Some(open) = positions.get_position(&pending.mint).await {
        match open.token_decimals {
            Some(d) if d == fill_token_decimals => {}
            other => {
                return Err(crate::error::Error::PositionAccounting(format!(
                    "recovered sell decimals mismatch for mint {} (sig {}): position decimals {:?} != fill decimals {}",
                    pending.mint, pending.signature, other, fill_token_decimals
                )));
            }
        }
    }

    let close_result = positions
        .close_position_reconciled(&pending.mint, &pending.signature, sold_amount_raw, received_sol)
        .await?;

    // Reapply layer marker when a partial position still remains. Idempotent
    // replay (already_applied) must still repair a missing flag (D2, D8).
    if !close_result.fully_closed {
        if positions.get_position(&pending.mint).await.is_some() {
            match intent {
                PendingSellIntent::QuickProfit => {
                    positions.mark_quick_profit_taken(&pending.mint).await?;
                }
                PendingSellIntent::SecondProfit => {
                    positions.mark_second_profit_taken(&pending.mint).await?;
                }
                // Full / Manual / KillSwitch add no profit-layer marker.
                PendingSellIntent::Full
                | PendingSellIntent::Manual
                | PendingSellIntent::KillSwitch => {}
            }
        }
    }

    info!(
        "Recovered confirmed SELL for mint {} (sig {}): sold {} raw, net {} SOL, full_close={}",
        pending.mint, pending.signature, sold_amount_raw, received_sol, close_result.fully_closed
    );
    Ok(())
}

/// Counts summarizing legacy-position chain recovery (D3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyRecoverySummary {
    /// Positions canonicalized via `migrate_position_from_confirmed_entry`.
    pub recovered: usize,
    /// Positions removed because the controlled wallet proved a zero balance.
    pub resolved_zero: usize,
    /// Positions that remain recovery-required / unroutable after this pass.
    pub still_recovery_required: usize,
}

impl LegacyRecoverySummary {
    /// True when no recovery-required / unroutable position remains.
    fn fully_resolved(&self) -> bool {
        self.still_recovery_required == 0
    }
}

/// Pure predicate (D3): does this position need legacy chain recovery OR is it
/// operationally blocked because its (valid) wallet has no route in `registry`?
///
/// Recovery-required when `token_decimals` is unknown, or the wallet pubkey is
/// empty/invalid. Additionally blocked (returns true) when the wallet parses but
/// the registry does not own/route it — a canonical-looking position we cannot
/// actually execute against.
fn legacy_recovery_required(
    position: &crate::position::manager::Position,
    registry: &crate::wallet::ExecutionWalletRegistry,
) -> bool {
    if position_requires_recovery(position) {
        return true;
    }
    // Wallet parses (position_requires_recovery already proved that). If the
    // registry has no route for it, the position is unroutable => blocked.
    match position.wallet_pubkey.parse::<Pubkey>() {
        Ok(pk) => registry.route_for(&pk).is_none(),
        Err(_) => true,
    }
}

/// Recover legacy/incomplete positions from chain evidence (D3/D4).
///
/// For each position that is recovery-required or unroutable, find its current
/// owning wallet via the ownership probe across registry wallets, then reconcile
/// the original entry transaction and canonicalize (migrate) or resolve-zero.
/// Only ConfirmedFill canonicalizes; anything ambiguous keeps the position
/// recovery-required (INV-POS-002/003/004).
async fn recover_legacy_positions(
    reconciler: &TradeReconciler,
    ownership: &crate::wallet::WalletOwnershipProbe,
    registry: &crate::wallet::ExecutionWalletRegistry,
    positions: &crate::position::manager::PositionManager,
) -> crate::error::Result<LegacyRecoverySummary> {
    use crate::wallet::OwnedHolderResolution;

    let mut summary = LegacyRecoverySummary::default();

    for position in positions.get_all_positions().await {
        if !legacy_recovery_required(&position, registry) {
            continue;
        }

        let mint_pk = match position.mint.parse::<Pubkey>() {
            Ok(pk) => pk,
            Err(e) => {
                warn!(
                    "Legacy recovery: mint {} is not a valid Pubkey: {} - keeping recovery-required",
                    position.mint, e
                );
                summary.still_recovery_required += 1;
                continue;
            }
        };

        // Determine the current owning wallet (D3 ownership rules).
        let holders = match ownership.find_positive_holders(registry, mint_pk).await {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    "Legacy recovery: ownership probe failed for mint {}: {} - keeping recovery-required",
                    position.mint, e
                );
                summary.still_recovery_required += 1;
                continue;
            }
        };

        // (selected_wallet, proven_zero)
        let (selected_wallet, proven_zero): (Option<Pubkey>, bool) = match holders {
            OwnedHolderResolution::Single(state) => (Some(state.wallet), false),
            OwnedHolderResolution::Multiple(_) => {
                // Ambiguous: do not merge, do not choose (INV-WALLET-004).
                warn!(
                    "Legacy recovery: mint {} held in multiple controlled wallets - ambiguous, keeping recovery-required",
                    position.mint
                );
                summary.still_recovery_required += 1;
                continue;
            }
            OwnedHolderResolution::None => {
                // Proven zero ONLY if the existing wallet parses, the registry
                // owns/routes it, and a probe of THAT exact wallet proves zero.
                match position.wallet_pubkey.parse::<Pubkey>() {
                    Ok(existing) if registry.owns(&existing) => {
                        match ownership.probe(existing, mint_pk).await {
                            Ok(state) if state.raw_amount == 0 => (Some(existing), true),
                            Ok(_) => {
                                // Non-zero now but not surfaced as a positive holder
                                // above: treat as unresolved rather than guessing.
                                summary.still_recovery_required += 1;
                                continue;
                            }
                            Err(e) => {
                                warn!(
                                    "Legacy recovery: zero-proof probe failed for mint {} wallet {}: {} - keeping recovery-required",
                                    position.mint, existing, e
                                );
                                summary.still_recovery_required += 1;
                                continue;
                            }
                        }
                    }
                    _ => {
                        // Identity was unknown and all wallets show zero: do NOT
                        // assume the position belonged to one of them (INV-POS-004).
                        summary.still_recovery_required += 1;
                        continue;
                    }
                }
            }
        };

        let wallet = match selected_wallet {
            Some(w) => w,
            None => {
                summary.still_recovery_required += 1;
                continue;
            }
        };

        // D4: reconcile the ORIGINAL entry transaction for this exact wallet.
        let outcome = match reconciler
            .reconcile(
                &position.entry_signature,
                &wallet.to_string(),
                &position.mint,
                ReconciliationSide::Buy,
            )
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!(
                    "Legacy recovery: entry reconcile RPC error for mint {}: {} - keeping recovery-required",
                    position.mint, e
                );
                summary.still_recovery_required += 1;
                continue;
            }
        };

        let fill = match outcome {
            ReconciliationOutcome::ConfirmedFill(fill) => fill,
            _ => {
                // ConfirmedFailure / Unresolved: do not guess.
                summary.still_recovery_required += 1;
                warn!(
                    "Legacy recovery: entry tx for mint {} not a confirmed fill - keeping recovery-required",
                    position.mint
                );
                continue;
            }
        };

        // Require exact wallet/mint/Buy, nonzero original raw, cost>0, price>0.
        let original_raw = fill.token_amount_raw().unwrap_or(0);
        let original_cost = -fill.wallet_sol_delta_sol();
        let original_price = fill.effective_price_sol_per_token().unwrap_or(0.0);
        let identity_ok = fill.wallet == wallet.to_string()
            && fill.mint == position.mint
            && fill.side == ReconciliationSide::Buy;
        if !identity_ok
            || original_raw == 0
            || !(original_cost.is_finite() && original_cost > 0.0)
            || !(original_price.is_finite() && original_price > 0.0)
        {
            summary.still_recovery_required += 1;
            warn!(
                "Legacy recovery: entry fill for mint {} failed exact-economics validation - keeping recovery-required",
                position.mint
            );
            continue;
        }

        if proven_zero {
            // Current balance proven zero: resolve without inventing P&L.
            match positions
                .resolve_zero_balance_position(&position.mint, &position.entry_signature)
                .await
            {
                Ok(true) => {
                    summary.resolved_zero += 1;
                    info!(
                        "Legacy recovery: mint {} proven zero on-chain, position resolved (no P&L invented)",
                        position.mint
                    );
                }
                Ok(false) => summary.still_recovery_required += 1,
                Err(e) => {
                    summary.still_recovery_required += 1;
                    warn!(
                        "Legacy recovery: resolve_zero for mint {} failed: {} - keeping recovery-required",
                        position.mint, e
                    );
                }
            }
            continue;
        }

        // Re-probe the selected wallet's CURRENT balance to decide migrate vs zero.
        let current = match ownership.probe(wallet, mint_pk).await {
            Ok(state) => state,
            Err(e) => {
                summary.still_recovery_required += 1;
                warn!(
                    "Legacy recovery: current-balance probe failed for mint {}: {} - keeping recovery-required",
                    position.mint, e
                );
                continue;
            }
        };

        if current.raw_amount > original_raw {
            // Ambiguous additional transfer/buy: do not canonicalize.
            summary.still_recovery_required += 1;
            warn!(
                "Legacy recovery: mint {} current raw {} exceeds original entry raw {} - ambiguous, keeping halted",
                position.mint, current.raw_amount, original_raw
            );
            continue;
        }

        if current.raw_amount == 0 {
            match positions
                .resolve_zero_balance_position(&position.mint, &position.entry_signature)
                .await
            {
                Ok(true) => summary.resolved_zero += 1,
                Ok(false) => summary.still_recovery_required += 1,
                Err(e) => {
                    summary.still_recovery_required += 1;
                    warn!(
                        "Legacy recovery: resolve_zero for mint {} failed: {} - keeping recovery-required",
                        position.mint, e
                    );
                }
            }
            continue;
        }

        // 0 < current <= original: canonicalize with prorated remaining cost.
        //
        // B3: the CURRENT probed account decimals must EXACTLY equal the original
        // confirmed-entry fill decimals. No fallback to the entry decimals when
        // the probe reports None or a different value — that would migrate an
        // account whose on-chain decimals disagree with the tracked cost basis.
        // Mismatch/None => keep recovery-required, do NOT migrate.
        let decimals = match current.decimals {
            Some(d) if d == fill.token_decimals => d,
            other => {
                summary.still_recovery_required += 1;
                warn!(
                    "Legacy recovery: mint {} current account decimals {:?} != confirmed entry decimals {} - keeping recovery-required, not migrating",
                    position.mint, other, fill.token_decimals
                );
                continue;
            }
        };
        match positions
            .migrate_position_from_confirmed_entry(
                &position.mint,
                &position.entry_signature,
                &wallet.to_string(),
                original_raw,
                current.raw_amount,
                decimals,
                original_cost,
                original_price,
            )
            .await
        {
            Ok(_) => {
                summary.recovered += 1;
                info!(
                    "Legacy recovery: mint {} canonicalized from confirmed entry (raw {} of {})",
                    position.mint, current.raw_amount, original_raw
                );
            }
            Err(e) => {
                summary.still_recovery_required += 1;
                warn!(
                    "Legacy recovery: migrate for mint {} failed: {} - keeping recovery-required",
                    position.mint, e
                );
            }
        }
    }

    Ok(summary)
}

/// Pure conversion (D6): build the strategy-engine `Position` used to restore
/// portfolio exposure from a canonical PositionManager position. State restore
/// ONLY — size_sol is the REMAINING total cost, tokens_held is the RAW token
/// amount. Never records execution feedback / chain health / slippage.
fn manager_position_to_strategy_position(
    position: &crate::position::manager::Position,
    default_strategy: crate::strategy::types::TradingStrategy,
) -> crate::strategy::types::Position {
    // highest = max(peak_price, entry_price); lowest = the meaningful lower of
    // current/entry (a positive finite current price is meaningful, else entry).
    let highest_price = position.peak_price.max(position.entry_price);
    let lowest_price = if position.current_price.is_finite() && position.current_price > 0.0 {
        position.current_price.min(position.entry_price)
    } else {
        position.entry_price
    };
    crate::strategy::types::Position {
        mint: position.mint.clone(),
        entry_price: position.entry_price,
        entry_time: position.entry_time,
        size_sol: position.total_cost_sol,
        tokens_held: position.token_amount,
        strategy: default_strategy,
        exit_style: crate::strategy::types::ExitStyle::default(),
        highest_price,
        lowest_price,
        exit_levels_hit: vec![],
    }
}

/// Pure predicate (D6): a canonical position eligible for strategy-exposure
/// restore has known decimals, a valid wallet, and a route in the registry.
fn position_is_canonical_for_restore(
    position: &crate::position::manager::Position,
    registry: &crate::wallet::ExecutionWalletRegistry,
) -> bool {
    if position.token_decimals.is_none() {
        return false;
    }
    match position.wallet_pubkey.parse::<Pubkey>() {
        Ok(pk) => registry.route_for(&pk).is_some(),
        Err(_) => false,
    }
}

// ===========================================================================
// AGENT B — shared primary startup / exit coordination helpers (B1/B5/B7/B8)
// ===========================================================================

/// B1: deterministically order pending recovery items by submission time, then
/// signature. `PendingExecutionStore::all()` is HashMap-backed and returns an
/// arbitrary order; startup recovery must be reproducible so that same-mint or
/// same-wallet in-flight submissions are always resolved oldest-first.
fn sort_pending_for_recovery(items: &mut [PendingExecution]) {
    items.sort_by(|a, b| {
        a.submitted_at
            .cmp(&b.submitted_at)
            .then_with(|| a.signature.cmp(&b.signature))
    });
}

/// B8: pure predicate mirroring the "pending Buy OR Sell blocks an automatic
/// exit" decision for a mint. Either an unresolved Buy or an unresolved Sell for
/// the same mint must prevent a new automatic sell submission. Returns the
/// blocking signature (Sell preferred for logging) when blocked.
fn pending_blocks_automatic_sell(
    pending_buy: Option<&PendingExecution>,
    pending_sell: Option<&PendingExecution>,
) -> Option<String> {
    if let Some(p) = pending_sell {
        return Some(p.signature.clone());
    }
    if let Some(p) = pending_buy {
        return Some(p.signature.clone());
    }
    None
}

/// B5: pure operational-exit-route predicate. A position can actually be exited
/// by an automatic producer iff ALL hold:
/// - decimals are known (canonical accounting);
/// - the recorded wallet parses to a valid pubkey;
/// - the recovery registry has a route for that wallet;
/// - the route's execution trader handle is actually available
///   (Local => `local_trader_available`, Lightning => `lightning_trader_available`).
fn position_has_operational_exit_route(
    position: &crate::position::manager::Position,
    registry: &crate::wallet::ExecutionWalletRegistry,
    local_trader_available: bool,
    lightning_trader_available: bool,
) -> bool {
    if position.token_decimals.is_none() {
        return false;
    }
    let wallet = match position.wallet_pubkey.parse::<Pubkey>() {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    match registry.route_for(&wallet) {
        Some(crate::wallet::ExecutionRoute::Local) => local_trader_available,
        Some(crate::wallet::ExecutionRoute::Lightning) => lightning_trader_available,
        None => false,
    }
}

// --- B7: shared same-mint primary sell coordinator -------------------------

/// B7: process-wide set of mints with an in-flight primary sell reservation.
/// Shared (cloned) into BOTH the primary auto-sell monitor and the event
/// kill-switch so the two concurrent sell producers cannot submit two sells for
/// the same mint before either signature is journaled.
type ActiveSellMints = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

/// B7: attempt to reserve `mint` for a primary sell. Returns true iff the mint
/// was newly reserved. A mint already reserved returns false (another producer
/// owns the in-flight sell). A poisoned lock fails closed (false): we never
/// submit a sell we cannot coordinate.
fn try_reserve_sell_mint(active: &ActiveSellMints, mint: &str) -> bool {
    match active.lock() {
        Ok(mut set) => set.insert(mint.to_string()),
        Err(_) => false,
    }
}

/// B7: release a mint's primary sell reservation. A poisoned lock is ignored
/// (the reservation cannot be observed as free again, which is fail-closed).
fn release_sell_mint(active: &ActiveSellMints, mint: &str) {
    if let Ok(mut set) = active.lock() {
        set.remove(mint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::types::{TradingAction, TradingStrategy};

    // --- MPT-001 Agent E7: pure route-mapping + lamport-conversion tests -----

    #[test]
    fn test_pump_venue_maps_to_pump_pool() {
        assert_eq!(
            pumpportal_pool_for_venue(MarketVenue::PumpBondingCurve),
            PoolType::Pump
        );
    }

    #[test]
    fn test_pumpswap_venue_maps_to_pumpamm_pool() {
        assert_eq!(
            pumpportal_pool_for_venue(MarketVenue::PumpSwapCanonical),
            PoolType::PumpAmm
        );
    }

    #[test]
    fn test_quoted_venue_mapping_never_auto() {
        // A quote-referenced buy must pin the exact quoted venue, never Auto.
        for venue in [MarketVenue::PumpBondingCurve, MarketVenue::PumpSwapCanonical] {
            assert_ne!(pumpportal_pool_for_venue(venue), PoolType::Auto);
        }
    }

    #[test]
    fn test_sol_to_lamports_rejects_invalid() {
        assert_eq!(sol_to_lamports_exact(f64::NAN), None);
        assert_eq!(sol_to_lamports_exact(f64::INFINITY), None);
        assert_eq!(sol_to_lamports_exact(f64::NEG_INFINITY), None);
        assert_eq!(sol_to_lamports_exact(-1.0), None);
        assert_eq!(sol_to_lamports_exact(0.0), None);
        // Sub-lamport positive amounts floor to zero and are rejected.
        assert_eq!(sol_to_lamports_exact(1e-10), None);
        // Overflow beyond u64 lamports is rejected.
        assert_eq!(sol_to_lamports_exact(2.0e10), None);
    }

    #[test]
    fn test_sol_to_lamports_floors_correctly() {
        assert_eq!(sol_to_lamports_exact(1.0), Some(1_000_000_000));
        assert_eq!(sol_to_lamports_exact(0.05), Some(50_000_000));
        // Floors, never rounds up.
        assert_eq!(sol_to_lamports_exact(0.000_000_001_9), Some(1));
    }

    // --- MPT-001 Agent F10: primary price-exit truth pure tests -------------

    #[test]
    fn test_raw_token_amount_to_decimal_string() {
        assert_eq!(raw_token_amount_to_decimal_string(1_234_567, 6), "1.234567");
        assert_eq!(raw_token_amount_to_decimal_string(500_000, 6), "0.5");
        // Leading fractional zeros preserved.
        assert_eq!(raw_token_amount_to_decimal_string(1, 6), "0.000001");
        assert_eq!(raw_token_amount_to_decimal_string(1_050_000, 6), "1.05");
        // Never scientific notation, arbitrary decimals honored (not hardcoded 6).
        assert_eq!(raw_token_amount_to_decimal_string(1, 9), "0.000000001");
    }

    #[test]
    fn test_raw_amount_format_zero_fraction() {
        // Exact multiple of 10^decimals => integer, no decimal point.
        assert_eq!(raw_token_amount_to_decimal_string(2_000_000, 6), "2");
        assert_eq!(raw_token_amount_to_decimal_string(0, 6), "0");
        // decimals == 0 => raw integer verbatim.
        assert_eq!(raw_token_amount_to_decimal_string(42, 0), "42");
    }

    #[test]
    fn test_partial_layer_raw_amount_50() {
        assert_eq!(layer_raw_amount(1_000_000, "50%"), Some(500_000));
        // Integer division (floor), never rounds up.
        assert_eq!(layer_raw_amount(1_000_001, "50%"), Some(500_000));
        assert_eq!(layer_raw_amount(1_000_000, "100%"), Some(1_000_000));
        // Zero-size layer is not submittable.
        assert_eq!(layer_raw_amount(1, "50%"), None);
        assert_eq!(layer_raw_amount(0, "100%"), None);
        // Unknown layer string.
        assert_eq!(layer_raw_amount(1_000_000, "33%"), None);
    }

    #[test]
    fn test_partial_layer_raw_amount_25() {
        assert_eq!(layer_raw_amount(1_000_000, "25%"), Some(250_000));
        assert_eq!(layer_raw_amount(1_000_003, "25%"), Some(250_000));
        assert_eq!(layer_raw_amount(3, "25%"), None);
    }

    #[test]
    fn test_mark_candidate_requires_executable_confirmation() {
        // A take-profit candidate identified from the mark must ALSO hold against
        // the executable-quote price. If the quote price is below target, it does
        // not confirm even though the mark triggered.
        let cat = PriceExitCategory::TakeProfit {
            entry_price: 1.0,
            tp_pct: 50.0,
        };
        // Mark at +60% triggered the candidate; quote at +10% must NOT confirm.
        assert!(!cat.confirms_at(1.10));
        // Quote still at/above +50% confirms.
        assert!(cat.confirms_at(1.55));
    }

    #[test]
    fn test_quote_below_stop_threshold_confirms_exit() {
        // Stop-loss at -20%. An executable quote price below the threshold confirms.
        let cat = PriceExitCategory::StopLoss {
            entry_price: 1.0,
            sl_pct: 20.0,
        };
        assert!(cat.confirms_at(0.75)); // -25% <= -20% => confirm
        assert!(!cat.confirms_at(0.90)); // -10% not past stop => no confirm
        // Non-finite / nonpositive prices never authorize.
        assert!(!cat.confirms_at(0.0));
        assert!(!cat.confirms_at(f64::NAN));
    }

    #[test]
    fn test_mark_trigger_but_quote_not_trigger_does_not_sell() {
        // Trailing stop: peak 2.0, entry 1.0, trailing 5%. A quote price that is
        // still within 5% of peak (and in profit) does NOT confirm the exit, so no
        // sell is submitted even though a marginal mark may have triggered.
        let cat = PriceExitCategory::TrailingStop {
            entry_price: 1.0,
            peak_price: 2.0,
            trailing_pct: 5.0,
        };
        // Quote at 1.98 => only 1% off peak => no confirm.
        assert!(!cat.confirms_at(1.98));
        // Quote at 1.80 => 10% off peak, still in profit => confirm.
        assert!(cat.confirms_at(1.80));
    }

    #[test]
    fn test_price_exit_uses_pinned_pump_pool() {
        // A Pump-venue sell quote must pin PoolType::Pump (no Auto).
        let pool = pumpportal_pool_for_venue(MarketVenue::PumpBondingCurve);
        assert_eq!(pool, PoolType::Pump);
        assert_ne!(pool, PoolType::Auto);
    }

    #[test]
    fn test_price_exit_uses_pinned_pumpamm_pool() {
        let pool = pumpportal_pool_for_venue(MarketVenue::PumpSwapCanonical);
        assert_eq!(pool, PoolType::PumpAmm);
        assert_ne!(pool, PoolType::Auto);
    }

    #[test]
    fn test_kill_switch_can_fallback_unquoted_without_faking_slippage() {
        // The kill-switch emergency route uses PoolType::Auto and carries NO
        // fabricated expected price. Model the F8 decision: when the oracle is
        // unavailable the emergency path proceeds with Auto and a None expected
        // price (unquoted feedback), never a synthesized slippage.
        let oracle_available = false;
        let emergency_pool = if oracle_available {
            pumpportal_pool_for_venue(MarketVenue::PumpBondingCurve)
        } else {
            PoolType::Auto
        };
        assert_eq!(emergency_pool, PoolType::Auto);
        let fabricated_expected_price: Option<f64> = None;
        assert!(fabricated_expected_price.is_none());
    }

    #[test]
    fn test_quoted_buy_feedback_preserves_expected_price() {
        // The value threaded into record_reconciled_quoted_execution is the quote's
        // expected price verbatim (only present when Some); it is not derived from the
        // lamport size or the fill. This mirrors the E5 branch selection.
        let expected: Option<f64> = Some(0.000_030_5);
        let chosen = expected; // Some => quoted path uses this exact value
        assert_eq!(chosen, Some(0.000_030_5));

        let none_case: Option<f64> = None;
        assert!(none_case.is_none()); // None => unquoted path, no fabricated price
    }

    // --- MPT-001 Agent G5: HotScan buy market-truth pure tests --------------

    fn hotscan_test_buy_quote(venue: MarketVenue) -> crate::market::ExecutableQuote {
        crate::market::ExecutableQuote {
            mint: Pubkey::new_unique(),
            side: crate::market::MarketSide::Buy,
            venue,
            quote_asset: crate::market::QuoteAsset::Sol,
            base_decimals: 6,
            quote_decimals: 9,
            base_amount_raw: 1_000_000,
            quote_amount_raw: 50_000_000,
            expected_price_sol_per_token: Some(0.000_05),
            protocol_fee_bps: 100,
            creator_fee_bps: 0,
            lp_fee_bps: 0,
            slot: 42,
            quoted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_hotscan_scanner_price_cannot_become_execution_reference() {
        // G1: the buy decision is driven ONLY by the fresh on-chain quote result,
        // never by a DexScreener/scanner price. A scanner price cannot construct a
        // Submit decision; only an Ok(quote) can. With no quote (Err), the sole
        // possible decision is a no-submit market skip, regardless of any scanner
        // momentum/price the candidate carried.
        let no_quote: crate::error::Result<crate::market::ExecutableQuote> =
            Err(crate::error::Error::MarketData("no curve/pool state".into()));
        assert_eq!(
            hotscan_buy_decision(&no_quote),
            HotScanBuyDecision::SkipMarketUnsupported
        );
    }

    #[test]
    fn test_hotscan_pump_quote_pins_pump() {
        // G3: a Pump bonding-curve buy quote pins PoolType::Pump (never Auto).
        let quote = Ok(hotscan_test_buy_quote(MarketVenue::PumpBondingCurve));
        let decision = hotscan_buy_decision(&quote);
        assert_eq!(decision, HotScanBuyDecision::Submit(PoolType::Pump));
        assert_ne!(decision, HotScanBuyDecision::Submit(PoolType::Auto));
    }

    #[test]
    fn test_hotscan_pumpswap_quote_pins_pumpamm() {
        // G3: a canonical PumpSwap buy quote pins PoolType::PumpAmm (never Auto).
        let quote = Ok(hotscan_test_buy_quote(MarketVenue::PumpSwapCanonical));
        let decision = hotscan_buy_decision(&quote);
        assert_eq!(decision, HotScanBuyDecision::Submit(PoolType::PumpAmm));
        assert_ne!(decision, HotScanBuyDecision::Submit(PoolType::Auto));
    }

    #[test]
    fn test_hotscan_unsupported_quote_mint_is_no_submit() {
        // G2: an UnsupportedQuoteMint (or MarketData) quote produces a no-submit
        // market-admission skip — NOT a Submit decision, so no transaction and no
        // blacklist/fill-rate failure downstream.
        let unsupported: crate::error::Result<crate::market::ExecutableQuote> = Err(
            crate::error::Error::UnsupportedQuoteMint("USDC-quoted".into()),
        );
        let decision = hotscan_buy_decision(&unsupported);
        assert_eq!(decision, HotScanBuyDecision::SkipMarketUnsupported);
        assert!(!matches!(decision, HotScanBuyDecision::Submit(_)));
    }

    #[test]
    fn test_strategy_entry_size_enter_returns_size() {
        let action = TradingAction::Enter {
            mint: "m".to_string(),
            size_sol: 0.1,
            strategy: TradingStrategy::MomentumSurfing,
        };
        assert_eq!(strategy_entry_size(&action), Some(0.1));
    }

    #[test]
    fn test_strategy_entry_size_hold_is_none() {
        assert_eq!(strategy_entry_size(&TradingAction::Hold), None);
    }

    #[test]
    fn test_strategy_entry_size_skip_is_none() {
        assert_eq!(
            strategy_entry_size(&TradingAction::Skip {
                reason: "x".to_string()
            }),
            None
        );
    }

    #[test]
    fn test_strategy_entry_size_fatal_reject_is_none() {
        assert_eq!(
            strategy_entry_size(&TradingAction::FatalReject {
                reason: "x".to_string()
            }),
            None
        );
    }

    fn recovery_test_position() -> crate::position::manager::Position {
        crate::position::manager::Position {
            mint: "Mint1111111111111111111111111111111111111111".to_string(),
            name: "Test Token".to_string(),
            symbol: "TEST".to_string(),
            bonding_curve: "Curve111111111111111111111111111111111111111".to_string(),
            token_amount: 1_000_000,
            token_decimals: Some(6),
            entry_price: 0.00000001,
            total_cost_sol: 0.01,
            entry_time: chrono::Utc::now(),
            entry_signature: "sig".to_string(),
            entry_type: crate::position::manager::EntryType::Legacy,
            quick_profit_taken: false,
            second_profit_taken: false,
            peak_price: 0.00000001,
            current_price: 0.0,
            kill_switch_triggered: false,
            kill_switch_reason: None,
            wallet_pubkey: "So11111111111111111111111111111111111111112".to_string(),
            applied_exit_signatures: vec![],
        }
    }

    #[test]
    fn test_position_requires_recovery_canonical_is_false() {
        let p = recovery_test_position();
        assert!(!position_requires_recovery(&p));
    }

    #[test]
    fn test_position_requires_recovery_none_decimals_is_true() {
        let mut p = recovery_test_position();
        p.token_decimals = None;
        assert!(position_requires_recovery(&p));
    }

    #[test]
    fn test_position_requires_recovery_empty_wallet_is_true() {
        let mut p = recovery_test_position();
        p.wallet_pubkey = String::new();
        assert!(position_requires_recovery(&p));
    }

    #[test]
    fn test_position_requires_recovery_invalid_wallet_is_true() {
        let mut p = recovery_test_position();
        p.wallet_pubkey = "not-a-pubkey".to_string();
        assert!(position_requires_recovery(&p));
    }

    fn synthetic_buy_fill() -> crate::trading::ReconciledFill {
        // 6-decimal fixture is ONLY a test input; the helper must read decimals
        // from the fill, not hard-code them.
        crate::trading::ReconciledFill {
            signature: "sig-buy".to_string(),
            slot: 100,
            block_time: Some(1_700_000_000),
            wallet: "WalletPubkey1111111111111111111111111111111".to_string(),
            mint: "Mint1111111111111111111111111111111111111111".to_string(),
            side: ReconciliationSide::Buy,
            // +1_500_000 raw tokens received.
            token_delta_raw: 1_500_000,
            token_decimals: 6,
            // Wallet spent 0.03 SOL net (negative delta). 0.03 SOL = 30_000_000 lamports.
            wallet_sol_delta_lamports: -30_000_000,
            fee_lamports: 5_000,
            reconciliation_wait_ms: 250,
        }
    }

    #[test]
    fn test_primary_buy_fill_values_use_raw_amount_decimals_and_wallet_delta() {
        let fill = synthetic_buy_fill();
        let (raw, decimals, cost, price) =
            primary_buy_fill_values(&fill).expect("valid buy fill");

        // Raw amount preserved exactly.
        assert_eq!(raw, 1_500_000);
        // Decimals come from the fill, not hard-coded logic.
        assert_eq!(decimals, 6);
        // Cost uses the wallet SOL delta (0.03 SOL), and the fee is NOT added again.
        assert!((cost - 0.03).abs() < 1e-12, "cost was {}", cost);
        assert!(cost < 0.03 + fill.fee_sol());
        // Price = cost / UI amount = 0.03 / 1.5 = 0.02.
        assert!((price - 0.02).abs() < 1e-12, "price was {}", price);
    }

    #[test]
    fn test_primary_buy_fill_rejects_wrong_side() {
        let mut fill = synthetic_buy_fill();
        fill.side = ReconciliationSide::Sell;
        // A sell fill passed into the buy helper must error.
        assert!(primary_buy_fill_values(&fill).is_err());
    }

    // === Agent F (HotScan BUY) helper tests ===

    fn f_local_signer() -> Pubkey {
        // Distinct valid base58 pubkey standing in for a selected local signer
        // (e.g. a MultiWallet wallet or the primary keypair).
        Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap()
    }

    fn f_lightning_wallet() -> Pubkey {
        // A DIFFERENT valid pubkey standing in for the configured Lightning wallet.
        Pubkey::from_str("Vote111111111111111111111111111111111111111").unwrap()
    }

    #[test]
    fn test_hotscan_lightning_wallet_is_configured_lightning_not_local() {
        let local = f_local_signer();
        let lightning = f_lightning_wallet();
        // Lightning active mode: execution wallet MUST be the configured Lightning
        // wallet, NOT the selected local signer.
        let resolved =
            hotscan_execution_wallet(true, local, Some(lightning)).expect("resolves");
        assert_eq!(resolved, lightning);
        assert_ne!(resolved, local);
    }

    #[test]
    fn test_hotscan_lightning_active_without_wallet_is_error_no_fallback() {
        let local = f_local_signer();
        // Lightning active but no configured Lightning wallet: hard error, never
        // falls back to the local signer (INV-WALLET-002).
        assert!(hotscan_execution_wallet(true, local, None).is_err());
    }

    #[test]
    fn test_hotscan_local_wallet_is_selected_signer() {
        let local = f_local_signer();
        let lightning = f_lightning_wallet();
        // Local active mode: execution wallet is the selected local signer, and the
        // configured Lightning wallet (even if present) is irrelevant.
        let resolved =
            hotscan_execution_wallet(false, local, Some(lightning)).expect("resolves");
        assert_eq!(resolved, local);
        assert_ne!(resolved, lightning);
    }

    #[test]
    fn test_hotscan_risk_check_uses_final_amount() {
        // The pre-send risk check must use the FINAL amount (after the creator
        // multiplier), not the base amount.
        let base = 0.02_f64;
        let final_amount = base * 1.5; // elite creator 1.5x
        assert_eq!(hotscan_risk_check_amount(final_amount), final_amount);
        assert_ne!(hotscan_risk_check_amount(final_amount), base);
    }

    #[test]
    fn test_hotscan_confirmed_fill_position_stores_raw_and_decimals() {
        // A confirmed HotScan fill must produce a canonical Position carrying RAW
        // token units and the fill's actual decimals (INV-TX-005) — never an
        // estimate or a hard-coded 6-decimal normalization.
        let fill = synthetic_buy_fill();
        let (raw, _decimals, cost, price) =
            primary_buy_fill_values(&fill).expect("valid buy fill");

        let position = crate::position::manager::Position {
            mint: fill.mint.clone(),
            name: "n".to_string(),
            symbol: "S".to_string(),
            bonding_curve: String::new(),
            token_amount: raw,
            token_decimals: Some(fill.token_decimals),
            entry_price: price,
            total_cost_sol: cost,
            entry_time: chrono::Utc::now(),
            entry_signature: fill.signature.clone(),
            entry_type: crate::position::manager::EntryType::Opportunity,
            quick_profit_taken: false,
            second_profit_taken: false,
            peak_price: price,
            current_price: price,
            kill_switch_triggered: false,
            kill_switch_reason: None,
            wallet_pubkey: fill.wallet.clone(),
            applied_exit_signatures: vec![],
        };

        // Stores RAW units exactly, decimals from the fill, and bonding_curve empty.
        assert_eq!(position.token_amount, 1_500_000);
        assert_eq!(position.token_decimals, Some(6));
        assert!(position.bonding_curve.is_empty());
        assert_eq!(position.entry_type, crate::position::manager::EntryType::Opportunity);
        assert_eq!(position.wallet_pubkey, fill.wallet);
    }

    /// Pure classifier mirroring the live buy-block decision: bought_mints is only
    /// added for a `ConfirmedFill`; an `Unresolved` (or failure) outcome must not
    /// mark it (F5/F7).
    fn hotscan_should_add_bought_mint(outcome: &ReconciliationOutcome) -> bool {
        matches!(outcome, ReconciliationOutcome::ConfirmedFill(_))
    }

    #[test]
    fn test_hotscan_unresolved_does_not_mark_bought_mints() {
        let unresolved = ReconciliationOutcome::Unresolved {
            signature: "sig".to_string(),
            reason: "timeout".to_string(),
            observed_after_ms: 15_000,
        };
        assert!(!hotscan_should_add_bought_mint(&unresolved));

        let confirmed = ReconciliationOutcome::ConfirmedFill(synthetic_buy_fill());
        assert!(hotscan_should_add_bought_mint(&confirmed));
    }

    /// Synthetic SELL fill: 100 raw tokens removed (negative delta), decimals 6,
    /// positive net SOL received by default.
    fn synthetic_sell_fill() -> crate::trading::ReconciledFill {
        crate::trading::ReconciledFill {
            signature: "sig-sell".to_string(),
            slot: 200,
            block_time: Some(1_700_000_100),
            wallet: "WalletPubkey1111111111111111111111111111111".to_string(),
            mint: "Mint1111111111111111111111111111111111111111".to_string(),
            side: ReconciliationSide::Sell,
            // -100 raw tokens (sold). token_amount_raw() takes unsigned_abs.
            token_delta_raw: -100,
            token_decimals: 6,
            // Received 0.05 SOL net (positive delta) = 50_000_000 lamports.
            wallet_sol_delta_lamports: 50_000_000,
            fee_lamports: 5_000,
            reconciliation_wait_ms: 250,
        }
    }

    /// Synthetic reconciled Position holding 100 raw tokens with decimals Some(6).
    fn synthetic_sell_position() -> crate::position::manager::Position {
        crate::position::manager::Position {
            mint: "Mint1111111111111111111111111111111111111111".to_string(),
            name: "Test".to_string(),
            symbol: "TST".to_string(),
            bonding_curve: "bc".to_string(),
            token_amount: 100,
            token_decimals: Some(6),
            entry_price: 0.01,
            total_cost_sol: 1.0,
            entry_time: chrono::Utc::now(),
            entry_signature: "sig-buy".to_string(),
            entry_type: crate::position::manager::EntryType::default(),
            quick_profit_taken: false,
            second_profit_taken: false,
            peak_price: 0.01,
            current_price: 0.01,
            kill_switch_triggered: false,
            kill_switch_reason: None,
            wallet_pubkey: "WalletPubkey1111111111111111111111111111111".to_string(),
            applied_exit_signatures: vec![],
        }
    }

    #[test]
    fn test_primary_sell_fill_rejects_decimal_mismatch() {
        let mut fill = synthetic_sell_fill();
        fill.token_decimals = 9;
        let position = synthetic_sell_position(); // Some(6)
        assert!(primary_sell_fill_values(&fill, &position).is_err());
    }

    #[test]
    fn test_primary_sell_fill_accepts_negative_net_sol() {
        let mut fill = synthetic_sell_fill();
        // Fee-dominated confirmed sale: net wallet SOL delta is negative.
        fill.wallet_sol_delta_lamports = -1_000;
        let position = synthetic_sell_position();
        let (sold_raw, net_sol, price) =
            primary_sell_fill_values(&fill, &position).expect("negative net SOL is allowed");
        assert_eq!(sold_raw, 100);
        // Helper returns the negative delta unclamped.
        assert!(net_sol < 0.0, "net_sol was {}", net_sol);
        // Fee-dominated sale: price is finite but may be negative; not clamped.
        assert!(price.is_finite());
    }

    #[test]
    fn test_primary_sell_fill_rejects_oversell_before_position_mutation() {
        let mut fill = synthetic_sell_fill();
        // Position holds 100 raw; fill claims 101 sold.
        fill.token_delta_raw = -101;
        let position = synthetic_sell_position();
        assert!(primary_sell_fill_values(&fill, &position).is_err());
    }

    // ======================================================================
    // AGENT G — HotScan SELL transaction-truth helper tests (G11)
    // ======================================================================

    #[test]
    fn test_g_route_local_exact_signer() {
        // G4: a Local-route position with the primary signer resolves to the Local
        // trader via the primary keypair; a Local route with only a recovery signer
        // resolves via the recovery multi-wallet. Neither ever becomes Lightning.
        assert_eq!(
            hotscan_sell_action(
                crate::wallet::ExecutionRoute::Local,
                true,  // local trader available
                true,  // lightning trader available (irrelevant for Local)
                true,  // primary signer matches
                false, // recovery signer absent
            ),
            HotScanSellAction::LocalPrimary
        );
        assert_eq!(
            hotscan_sell_action(
                crate::wallet::ExecutionRoute::Local,
                true,
                true,
                false, // primary does NOT match
                true,  // recovery signer present
            ),
            HotScanSellAction::LocalRecovery
        );
        // Local route but no exact signer anywhere => no sell.
        assert_eq!(
            hotscan_sell_action(
                crate::wallet::ExecutionRoute::Local,
                true,
                true,
                false,
                false,
            ),
            HotScanSellAction::NoRoute
        );
    }

    #[test]
    fn test_g_route_lightning_no_local_fallback() {
        // G4/INV-WALLET-001/003: a Lightning-route position uses the Lightning
        // trader ONLY. Even when a local trader and local signers are available,
        // the action is NEVER a Local one, and with no Lightning trader it is
        // NoRoute (never a silent Local fallback).
        assert_eq!(
            hotscan_sell_action(
                crate::wallet::ExecutionRoute::Lightning,
                true, // local trader available — must be ignored
                true, // lightning trader available
                true, // primary signer matches — must be ignored
                true, // recovery signer present — must be ignored
            ),
            HotScanSellAction::Lightning
        );
        let no_lightning = hotscan_sell_action(
            crate::wallet::ExecutionRoute::Lightning,
            true,  // local trader available
            false, // NO lightning trader
            true,  // primary signer matches
            true,  // recovery signer present
        );
        assert_eq!(no_lightning, HotScanSellAction::NoRoute);
        assert_ne!(no_lightning, HotScanSellAction::LocalPrimary);
        assert_ne!(no_lightning, HotScanSellAction::LocalRecovery);
    }

    #[test]
    fn test_g_registry_routes_match_recorded_wallet() {
        // The registry itself decides the route from the EXACT recorded wallet.
        use crate::wallet::ExecutionRoute;
        let local = f_local_signer();
        let lightning = f_lightning_wallet();
        let registry = ExecutionWalletRegistry::new(local, &[], Some(lightning));
        assert_eq!(registry.route_for(&local), Some(ExecutionRoute::Local));
        assert_eq!(registry.route_for(&lightning), Some(ExecutionRoute::Lightning));
        // An unknown wallet has no route => the live path halts, no sell.
        let unknown =
            Pubkey::from_str("Stake11111111111111111111111111111111111111").unwrap();
        assert_eq!(registry.route_for(&unknown), None);
    }

    #[test]
    fn test_g_intent_mapping() {
        // G5: "50%" => QuickProfit, "25%" => SecondProfit, full/"100%"/other => Full.
        assert_eq!(
            hotscan_sell_intent_for_layer("50%"),
            PendingSellIntent::QuickProfit
        );
        assert_eq!(
            hotscan_sell_intent_for_layer("25%"),
            PendingSellIntent::SecondProfit
        );
        assert_eq!(hotscan_sell_intent_for_layer("100%"), PendingSellIntent::Full);
        assert_eq!(hotscan_sell_intent_for_layer("full"), PendingSellIntent::Full);
    }

    #[test]
    fn test_g_requested_full_but_partial_fill_keeps_bought_mints() {
        // G8: the ACTUAL close result controls the full-exit cache/cooldown. A
        // requested "100%" that only partially fills (fully_closed == false) must
        // NOT remove the bought-mint or mark a full-exit cooldown. Only an actual
        // full close does.
        assert!(!hotscan_full_exit_removes_cache(false));
        assert!(hotscan_full_exit_removes_cache(true));
        // Intent mapping of the requested "100%" is still Full; the amount is
        // irrelevant to the cache decision — actual fill controls.
        assert_eq!(hotscan_sell_intent_for_layer("100%"), PendingSellIntent::Full);
    }

    #[test]
    fn test_g_confirmed_negative_net_sell_accepted() {
        // G6: a confirmed fee-dominated HotScan sale (negative net wallet SOL
        // delta) is accepted by the exact fill validator and is NOT clamped.
        let mut fill = synthetic_sell_fill();
        fill.wallet_sol_delta_lamports = -2_000; // net negative proceeds
        let position = synthetic_sell_position();
        let (sold_raw, net_sol, price) =
            primary_sell_fill_values(&fill, &position).expect("negative net sell accepted");
        assert_eq!(sold_raw, 100);
        assert!(net_sol < 0.0, "net_sol was {}", net_sol);
        assert!(price.is_finite());
    }

    // ======================================================================
    // MPT-001 Agent H — HotScan EXIT market-truth pure tests (H5)
    // ======================================================================

    // The HotScan exit authorizer takes ONLY the fresh exact-size executable
    // quote (`HotScanExecQuote`) as the price/venue source. It has no field for
    // DexScreener or a stale `current_price`, so those observations are
    // structurally incapable of authorizing an exit. The two tests below assert
    // that authorization tracks the EXECUTABLE quote, not any external mark.

    #[test]
    fn test_h_dexscreener_observation_cannot_authorize_exit() {
        // H5(1): a DexScreener "price" is only an observation. Regardless of how
        // bullish it looks, with NO usable executable quote a price exit is NOT
        // authorized (the authorizer never even sees a Dex value).
        let dexscreener_price_native = 999.0; // wildly optimistic Dex reading
        let _ = dexscreener_price_native; // never fed to the authorizer
        let decision = hotscan_exit_decision(
            false, // price-triggered (not kill-switch)
            Some(PriceExitCategory::TakeProfit {
                entry_price: 1.0,
                tp_pct: 50.0,
            }),
            None, // NO executable quote — Dex cannot substitute
            1_000_000,
            6,
            "100%",
        );
        assert_eq!(decision, HotScanExitDecision::NoSell);
    }

    #[test]
    fn test_h_stale_current_price_cannot_authorize_exit() {
        // H5(2): even if a stale `current_price` (the mark) would satisfy the
        // trigger, authorization is re-confirmed against the EXECUTABLE quote. A
        // quote price that no longer meets the condition => no sell.
        let stale_mark_would_trigger = 1.60; // +60% on the stale mark
        let _ = stale_mark_would_trigger; // not an authorizer input
        let decision = hotscan_exit_decision(
            false,
            Some(PriceExitCategory::TakeProfit {
                entry_price: 1.0,
                tp_pct: 50.0,
            }),
            // Executable quote says only +10% is actually achievable — below TP.
            Some(HotScanExecQuote {
                exec_price_sol_per_token: 1.10,
                venue: MarketVenue::PumpBondingCurve,
            }),
            1_000_000,
            6,
            "100%",
        );
        assert_eq!(decision, HotScanExitDecision::NoSell);
    }

    #[test]
    fn test_h_exact_quick_profit_quote_size() {
        // H5(3): QuickProfit ("50%") authorizes exactly half the raw balance,
        // submitted as the exact decimal string (not "50%").
        let raw = layer_raw_amount(1_000_000, "50%").expect("50% of 1_000_000");
        assert_eq!(raw, 500_000);
        let decision = hotscan_exit_decision(
            false,
            Some(PriceExitCategory::QuickProfit {
                entry_price: 1.0,
                qp_pct: 20.0,
                tp_pct: 50.0,
            }),
            Some(HotScanExecQuote {
                exec_price_sol_per_token: 1.30, // +30%: in the quick-profit band
                venue: MarketVenue::PumpBondingCurve,
            }),
            raw,
            6,
            "50%",
        );
        assert_eq!(
            decision,
            HotScanExitDecision::QuotedSell {
                submit_amount: "0.5".to_string(),
                pool: PoolType::Pump,
            }
        );
    }

    #[test]
    fn test_h_exact_second_profit_quote_size() {
        // H5(4): SecondProfit ("25%") authorizes exactly a quarter of the raw
        // balance, submitted as the exact decimal string (not "25%"). The
        // second-profit band is modeled with QuickProfit(qp=second_pct, tp).
        let raw = layer_raw_amount(1_000_000, "25%").expect("25% of 1_000_000");
        assert_eq!(raw, 250_000);
        let decision = hotscan_exit_decision(
            false,
            Some(PriceExitCategory::QuickProfit {
                entry_price: 1.0,
                qp_pct: 40.0, // second_profit_pct
                tp_pct: 80.0, // take_profit_pct
            }),
            Some(HotScanExecQuote {
                exec_price_sol_per_token: 1.50, // +50%: in the second-profit band
                venue: MarketVenue::PumpSwapCanonical,
            }),
            raw,
            6,
            "25%",
        );
        assert_eq!(
            decision,
            HotScanExitDecision::QuotedSell {
                submit_amount: "0.25".to_string(),
                pool: PoolType::PumpAmm,
            }
        );
    }

    #[test]
    fn test_h_full_quote_size() {
        // H5(5): a full ("100%") exit authorizes the entire raw balance as its
        // exact decimal string.
        let raw = layer_raw_amount(2_000_000, "100%").expect("100% of 2_000_000");
        assert_eq!(raw, 2_000_000);
        let decision = hotscan_exit_decision(
            false,
            Some(PriceExitCategory::TakeProfit {
                entry_price: 1.0,
                tp_pct: 50.0,
            }),
            Some(HotScanExecQuote {
                exec_price_sol_per_token: 1.60, // +60%: meets take-profit
                venue: MarketVenue::PumpBondingCurve,
            }),
            raw,
            6,
            "100%",
        );
        assert_eq!(
            decision,
            HotScanExitDecision::QuotedSell {
                submit_amount: "2".to_string(),
                pool: PoolType::Pump,
            }
        );
    }

    #[test]
    fn test_h_quoted_venue_pinned() {
        // H5(6): the submitted route is pinned to the QUOTED venue, never Auto,
        // for an authorized price exit — Pump curve => Pump, PumpSwap => PumpAmm.
        let tp = PriceExitCategory::TakeProfit {
            entry_price: 1.0,
            tp_pct: 50.0,
        };
        for (venue, expect_pool) in [
            (MarketVenue::PumpBondingCurve, PoolType::Pump),
            (MarketVenue::PumpSwapCanonical, PoolType::PumpAmm),
        ] {
            let decision = hotscan_exit_decision(
                false,
                Some(tp),
                Some(HotScanExecQuote {
                    exec_price_sol_per_token: 1.60,
                    venue,
                }),
                1_000_000,
                6,
                "100%",
            );
            match decision {
                HotScanExitDecision::QuotedSell { pool, .. } => {
                    assert_eq!(pool, expect_pool);
                    assert_ne!(pool, PoolType::Auto);
                }
                other => panic!("expected QuotedSell, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_h_drift_uses_quote_not_mark() {
        // H5(7): the quote-to-fill drift baseline is the EXECUTABLE quote price,
        // never the mark. The C-layer sell-drift helper is `(expected - actual) /
        // expected * 100`. Using the quote (1.20) vs. a hypothetical mark (1.50)
        // yields materially different drift; the executable quote is authoritative.
        use crate::strategy::types::{ExecutionRecord, Side};
        let quote_expected = 1.20_f64;
        let mark = 1.50_f64;
        let actual_fill = 1.14_f64; // 5% worse than the executable quote

        let drift_vs_quote =
            ExecutionRecord::quote_to_fill_drift_pct(Side::Sell, quote_expected, actual_fill)
                .expect("finite drift");
        // 5% worse than the quote.
        assert!((drift_vs_quote - 5.0).abs() < 1e-6, "drift_vs_quote={}", drift_vs_quote);

        let drift_vs_mark =
            ExecutionRecord::quote_to_fill_drift_pct(Side::Sell, mark, actual_fill)
                .expect("finite drift");
        // The mark-based number is very different — proving the baseline choice matters.
        assert!(
            (drift_vs_quote - drift_vs_mark).abs() > 10.0,
            "quote {} vs mark {} baselines must diverge",
            drift_vs_quote,
            drift_vs_mark
        );
    }

    // ======================================================================
    // AGENT D — startup recovery + strategy rebuild helper tests (D8)
    // ======================================================================

    use crate::config::SafetyConfig;
    use crate::position::manager::{EntryType, PositionManager};
    use crate::wallet::ExecutionWalletRegistry;

    /// In-memory (non-persisted) manager for recovery-helper tests.
    fn mem_manager() -> PositionManager {
        let cfg = SafetyConfig {
            require_sell_confirmation: false,
            max_position_sol: 1_000.0,
            daily_loss_limit_sol: 1_000.0,
            keypair_balance_warning_sol: 0.0,
        };
        PositionManager::new(cfg, None)
    }

    /// A canonical (decimals known, valid wallet) manager Position holding
    /// `raw` tokens with the given wallet/entry signature.
    fn canonical_position(mint: &str, wallet: &str, sig: &str, raw: u64) -> crate::position::manager::Position {
        crate::position::manager::Position {
            mint: mint.to_string(),
            name: "T".to_string(),
            symbol: "T".to_string(),
            bonding_curve: "bc".to_string(),
            token_amount: raw,
            token_decimals: Some(6),
            entry_price: 0.001,
            total_cost_sol: 1.0,
            entry_time: chrono::Utc::now(),
            entry_signature: sig.to_string(),
            entry_type: EntryType::Opportunity,
            quick_profit_taken: false,
            second_profit_taken: false,
            peak_price: 0.002,
            current_price: 0.0015,
            kill_switch_triggered: false,
            kill_switch_reason: None,
            wallet_pubkey: wallet.to_string(),
            applied_exit_signatures: vec![],
        }
    }

    fn sell_pending(mint: &str, wallet: &str, sig: &str, intent: PendingSellIntent) -> PendingExecution {
        PendingExecution::sell(
            sig.to_string(),
            mint.to_string(),
            wallet.to_string(),
            PendingSellContext {
                requested_amount: "50%".to_string(),
                intent,
                reason: "test".to_string(),
            },
        )
    }

    const T_MINT: &str = "Mint1111111111111111111111111111111111111111";

    /// D8: a recovered FULL exit removes the position and is idempotent on replay.
    #[tokio::test]
    async fn test_recovered_full_exit_removes_pending_idempotently() {
        let mgr = mem_manager();
        let wallet = Pubkey::new_unique().to_string();
        mgr.record_confirmed_position(canonical_position(T_MINT, &wallet, "entry-sig", 100))
            .await
            .unwrap();

        let pending = sell_pending(T_MINT, &wallet, "exit-full", PendingSellIntent::Full);
        // Sell the ENTIRE 100 raw => full close.
        apply_recovered_sell(&mgr, &pending, 6, 100, 0.5, PendingSellIntent::Full)
            .await
            .unwrap();
        assert!(mgr.get_position(T_MINT).await.is_none(), "position should be fully closed");

        // Idempotent replay of the same exit signature must not error / double-count.
        apply_recovered_sell(&mgr, &pending, 6, 100, 0.5, PendingSellIntent::Full)
            .await
            .unwrap();
        assert!(mgr.get_position(T_MINT).await.is_none());
    }

    /// D8: a recovered PARTIAL exit reapplies a missing QuickProfit marker,
    /// even on idempotent replay.
    #[tokio::test]
    async fn test_recovered_partial_reapplies_quick_profit_marker() {
        let mgr = mem_manager();
        let wallet = Pubkey::new_unique().to_string();
        mgr.record_confirmed_position(canonical_position(T_MINT, &wallet, "entry-sig", 100))
            .await
            .unwrap();

        let pending = sell_pending(T_MINT, &wallet, "exit-quick", PendingSellIntent::QuickProfit);
        apply_recovered_sell(&mgr, &pending, 6, 40, 0.3, PendingSellIntent::QuickProfit)
            .await
            .unwrap();
        let pos = mgr.get_position(T_MINT).await.expect("partial remains");
        assert!(pos.quick_profit_taken, "quick_profit flag must be set on partial recovery");
        assert!(!pos.second_profit_taken);
        assert_eq!(pos.token_amount, 60);

        // Idempotent replay: still no error, flag stays set.
        apply_recovered_sell(&mgr, &pending, 6, 40, 0.3, PendingSellIntent::QuickProfit)
            .await
            .unwrap();
        assert!(mgr.get_position(T_MINT).await.unwrap().quick_profit_taken);
    }

    /// D8: a recovered PARTIAL SecondProfit exit reapplies the second flag.
    #[tokio::test]
    async fn test_recovered_partial_reapplies_second_profit_marker() {
        let mgr = mem_manager();
        let wallet = Pubkey::new_unique().to_string();
        mgr.record_confirmed_position(canonical_position(T_MINT, &wallet, "entry-sig", 100))
            .await
            .unwrap();

        let pending = sell_pending(T_MINT, &wallet, "exit-second", PendingSellIntent::SecondProfit);
        apply_recovered_sell(&mgr, &pending, 6, 25, 0.2, PendingSellIntent::SecondProfit)
            .await
            .unwrap();
        let pos = mgr.get_position(T_MINT).await.expect("partial remains");
        assert!(pos.second_profit_taken, "second_profit flag must be set");
        assert!(!pos.quick_profit_taken);
    }

    /// D8: Full/Manual/KillSwitch partial exits add NO profit-layer marker.
    #[tokio::test]
    async fn test_recovered_partial_full_intent_adds_no_marker() {
        let mgr = mem_manager();
        let wallet = Pubkey::new_unique().to_string();
        mgr.record_confirmed_position(canonical_position(T_MINT, &wallet, "entry-sig", 100))
            .await
            .unwrap();
        let pending = sell_pending(T_MINT, &wallet, "exit-manual", PendingSellIntent::Manual);
        apply_recovered_sell(&mgr, &pending, 6, 40, 0.3, PendingSellIntent::Manual)
            .await
            .unwrap();
        let pos = mgr.get_position(T_MINT).await.unwrap();
        assert!(!pos.quick_profit_taken);
        assert!(!pos.second_profit_taken);
    }

    fn single_wallet_registry(wallet: &str) -> ExecutionWalletRegistry {
        let primary = wallet.parse::<Pubkey>().unwrap();
        ExecutionWalletRegistry::new(primary, &[], None)
    }

    /// D8: strategy-restore conversion uses REMAINING cost and RAW tokens.
    #[test]
    fn test_strategy_restore_uses_remaining_cost_and_raw_tokens() {
        let wallet = Pubkey::new_unique().to_string();
        let mut pos = canonical_position(T_MINT, &wallet, "entry-sig", 60);
        // Simulate a partial exit already applied: remaining cost < original.
        pos.total_cost_sol = 0.6;
        let sp = manager_position_to_strategy_position(
            &pos,
            crate::strategy::types::TradingStrategy::Adaptive,
        );
        assert_eq!(sp.tokens_held, 60, "tokens_held must be the RAW token amount");
        assert!((sp.size_sol - 0.6).abs() < 1e-12, "size_sol must be REMAINING total cost");
        assert_eq!(sp.mint, T_MINT);
        // highest = max(peak, entry) = max(0.002, 0.001) = 0.002.
        assert!((sp.highest_price - 0.002).abs() < 1e-12);
        // lowest = min(current, entry) = min(0.0015, 0.001) = 0.001.
        assert!((sp.lowest_price - 0.001).abs() < 1e-12);
        assert!(sp.exit_levels_hit.is_empty());
    }

    /// D8: a canonical position with a routable wallet is restore-eligible; one
    /// whose wallet is unknown to the registry is not.
    #[test]
    fn test_position_is_canonical_for_restore_route_gating() {
        let wallet = Pubkey::new_unique().to_string();
        let reg = single_wallet_registry(&wallet);
        let ok = canonical_position(T_MINT, &wallet, "s", 10);
        assert!(position_is_canonical_for_restore(&ok, &reg));

        // Different (unrouted) wallet => not restorable.
        let other = Pubkey::new_unique().to_string();
        let unrouted = canonical_position(T_MINT, &other, "s", 10);
        assert!(!position_is_canonical_for_restore(&unrouted, &reg));

        // Unknown decimals => not restorable.
        let mut no_dec = ok.clone();
        no_dec.token_decimals = None;
        assert!(!position_is_canonical_for_restore(&no_dec, &reg));
    }

    /// D8: legacy_recovery_required flags unknown decimals, invalid wallets, and
    /// canonical-but-unroutable positions.
    #[test]
    fn test_legacy_recovery_required_classification() {
        let wallet = Pubkey::new_unique().to_string();
        let reg = single_wallet_registry(&wallet);

        // Canonical + routable => not required.
        let ok = canonical_position(T_MINT, &wallet, "s", 10);
        assert!(!legacy_recovery_required(&ok, &reg));

        // Unknown decimals => required.
        let mut no_dec = ok.clone();
        no_dec.token_decimals = None;
        assert!(legacy_recovery_required(&no_dec, &reg));

        // Valid but unrouted wallet => blocked (required).
        let unrouted = canonical_position(T_MINT, &Pubkey::new_unique().to_string(), "s", 10);
        assert!(legacy_recovery_required(&unrouted, &reg));

        // Empty wallet => required.
        let mut empty = ok.clone();
        empty.wallet_pubkey = String::new();
        assert!(legacy_recovery_required(&empty, &reg));
    }

    /// D8: multi-holder ownership resolution is ambiguous and is NOT reduced to a
    /// single actionable wallet (mirrors the recover_legacy_positions guard).
    #[test]
    fn test_multi_wallet_legacy_holder_stays_unresolved() {
        use crate::wallet::{OwnedHolderResolution, WalletTokenState};
        let mint = Pubkey::new_unique();
        let states = vec![
            WalletTokenState {
                wallet: Pubkey::new_unique(),
                mint,
                raw_amount: 10,
                decimals: Some(6),
                token_account_count: 1,
            },
            WalletTokenState {
                wallet: Pubkey::new_unique(),
                mint,
                raw_amount: 20,
                decimals: Some(6),
                token_account_count: 1,
            },
        ];
        // The recovery path treats Multiple as ambiguous: no single wallet chosen.
        let resolution = OwnedHolderResolution::Multiple(states);
        let selected: Option<Pubkey> = match resolution {
            OwnedHolderResolution::Single(s) => Some(s.wallet),
            OwnedHolderResolution::Multiple(_) | OwnedHolderResolution::None => None,
        };
        assert!(selected.is_none(), "multi-holder must not resolve to a single wallet");
    }

    // === AGENT E — primary event kill-switch sell ===

    /// E5: a kill-switch pending sell round-trips its intent as
    /// `PendingSellIntent::KillSwitch` with the exact `"100%"` request.
    #[test]
    fn test_kill_switch_pending_intent_round_trips() {
        let wallet = Pubkey::new_unique().to_string();
        let pending = PendingExecution::sell(
            "ks-sig".to_string(),
            T_MINT.to_string(),
            wallet.clone(),
            PendingSellContext {
                requested_amount: "100%".to_string(),
                intent: PendingSellIntent::KillSwitch,
                reason: "smart-money dump".to_string(),
            },
        );
        assert_eq!(pending.side, ReconciliationSide::Sell);
        assert_eq!(pending.mint, T_MINT);
        assert_eq!(pending.wallet, wallet);
        pending.validate().expect("kill-switch pending is structurally valid");
        match &pending.context {
            crate::trading::PendingExecutionContext::Sell(ctx) => {
                assert_eq!(ctx.intent, PendingSellIntent::KillSwitch);
                assert_eq!(ctx.requested_amount, "100%");
            }
            _ => panic!("expected Sell context"),
        }

        // JSON round-trip preserves the KillSwitch variant.
        let json = serde_json::to_string(&pending).unwrap();
        let back: PendingExecution = serde_json::from_str(&json).unwrap();
        match back.context {
            crate::trading::PendingExecutionContext::Sell(ctx) => {
                assert_eq!(ctx.intent, PendingSellIntent::KillSwitch);
            }
            _ => panic!("expected Sell context after round-trip"),
        }
    }

    /// E5: the pure full/partial decision helper. A full actual close unwatches
    /// the evaluator; a partial close does NOT.
    #[test]
    fn test_kill_switch_unwatch_only_on_full_close() {
        assert!(kill_switch_unwatch_on_close(true), "full close must unwatch");
        assert!(!kill_switch_unwatch_on_close(false), "partial close keeps watching");
    }

    /// E3: the kill-switch route selection resolves Local for the primary wallet
    /// and yields no route (=> no sell + halt) for an unknown wallet, with no
    /// Lightning->Local fallback.
    #[test]
    fn test_kill_switch_route_selection() {
        let primary = Pubkey::new_unique();
        let lightning = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[], Some(lightning));

        assert_eq!(reg.route_for(&primary), Some(crate::wallet::ExecutionRoute::Local));
        assert_eq!(
            reg.route_for(&lightning),
            Some(crate::wallet::ExecutionRoute::Lightning)
        );
        // Unknown wallet => None => no sell, halt new entries.
        assert_eq!(reg.route_for(&Pubkey::new_unique()), None);
    }

    // ===================================================================
    // AGENT H — manual sell helper tests (H9)
    // ===================================================================

    fn wts(wallet: Pubkey, mint: Pubkey, raw: u64) -> crate::wallet::WalletTokenState {
        crate::wallet::WalletTokenState {
            wallet,
            mint,
            raw_amount: raw,
            decimals: Some(6),
            token_account_count: 1,
        }
    }

    /// H9: a tracked canonical Position selects its EXACT recorded wallet even
    /// when a global Lightning wallet is configured — there is no Lightning
    /// preference. The tracked wallet routes Local because an exact local signer
    /// exists for it.
    #[test]
    fn test_manual_sell_tracked_wallet_over_global_lightning() {
        let primary = Pubkey::new_unique();
        let lightning = Pubkey::new_unique();
        let registry = ExecutionWalletRegistry::new(primary, &[], Some(lightning));

        let mut pos = recovery_test_position();
        pos.wallet_pubkey = primary.to_string();
        pos.token_decimals = Some(6);

        // Canonical (not recovery-required) => exact tracked wallet chosen.
        assert!(!position_requires_recovery(&pos));
        let wallet: Pubkey = pos.wallet_pubkey.parse().unwrap();
        let choice = ManualSellWalletChoice::Tracked(wallet);
        assert_eq!(choice.wallet(), primary);
        assert!(choice.is_tracked());
        // The tracked wallet is Local (not the configured Lightning wallet).
        assert_eq!(
            registry.route_for(&choice.wallet()),
            Some(crate::wallet::ExecutionRoute::Local)
        );
        assert_ne!(choice.wallet(), lightning);
    }

    /// H9: an untracked token held by exactly one controlled wallet resolves to
    /// that exact wallet.
    #[test]
    fn test_manual_sell_untracked_single_holder_resolves() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let resolution =
            crate::wallet::OwnedHolderResolution::Single(wts(wallet, mint, 500));
        match manual_untracked_resolution(&resolution) {
            ManualUntrackedResolution::Single(state) => {
                assert_eq!(state.wallet, wallet);
                assert_eq!(state.raw_amount, 500);
                assert_eq!(state.decimals, Some(6));
            }
            other => panic!("expected Single, got {:?}", other),
        }
    }

    /// H9: an untracked token held in multiple controlled wallets is ambiguous
    /// and rejected (no wallet chosen, no cost-basis merge).
    #[test]
    fn test_manual_sell_untracked_multiple_holders_rejected() {
        let w1 = Pubkey::new_unique();
        let w2 = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let resolution = crate::wallet::OwnedHolderResolution::Multiple(vec![
            wts(w1, mint, 100),
            wts(w2, mint, 200),
        ]);
        match manual_untracked_resolution(&resolution) {
            ManualUntrackedResolution::Ambiguous(wallets) => {
                assert_eq!(wallets.len(), 2);
                assert!(wallets.contains(&w1) && wallets.contains(&w2));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    /// H9: an untracked token with no positive holder refuses the sell.
    #[test]
    fn test_manual_sell_untracked_no_holder_refused() {
        let resolution = crate::wallet::OwnedHolderResolution::None;
        assert_eq!(
            manual_untracked_resolution(&resolution),
            ManualUntrackedResolution::NoHolder
        );
    }

    /// H9: the untracked confirmed-fill path is the `Untracked` wallet choice,
    /// which carries no cost basis — the caller therefore takes the explicit
    /// "Realized P&L unavailable" branch (no PositionManager close).
    #[test]
    fn test_manual_sell_untracked_choice_has_no_pnl_basis() {
        let wallet = Pubkey::new_unique();
        let choice = ManualSellWalletChoice::Untracked(wallet);
        assert_eq!(choice.wallet(), wallet);
        assert!(
            !choice.is_tracked(),
            "an untracked choice must not be treated as tracked (no cost basis => no P&L)"
        );
    }

    /// H9: an unresolved pending Sell for the same mint blocks another manual
    /// sell, and the blocking signature is surfaced.
    #[test]
    fn test_manual_sell_pending_sell_blocks() {
        let sell = PendingExecution::sell(
            "sellsig".to_string(),
            "mint".to_string(),
            "wallet".to_string(),
            PendingSellContext {
                requested_amount: "100%".to_string(),
                intent: PendingSellIntent::Manual,
                reason: "manual".to_string(),
            },
        );
        assert_eq!(
            manual_sell_pending_block(None, Some(&sell)),
            Some("sellsig".to_string())
        );
    }

    /// H9: an unresolved pending Buy for the same mint also blocks a manual sell.
    #[test]
    fn test_manual_sell_pending_buy_blocks() {
        let buy = PendingExecution::buy(
            "buysig".to_string(),
            "mint".to_string(),
            "wallet".to_string(),
            PendingBuyContext {
                requested_sol: 0.05,
                name: "n".to_string(),
                symbol: "s".to_string(),
                bonding_curve: "bc".to_string(),
                entry_type: crate::position::manager::EntryType::Opportunity,
            },
        );
        assert_eq!(
            manual_sell_pending_block(Some(&buy), None),
            Some("buysig".to_string())
        );
    }

    /// H9: no pending Buy or Sell => not blocked.
    #[test]
    fn test_manual_sell_no_pending_not_blocked() {
        assert_eq!(manual_sell_pending_block(None, None), None);
    }

    // ===================================================================
    // AGENT B — primary startup / exit coordination tests (B13)
    // ===================================================================

    fn new_active_sell_mints() -> ActiveSellMints {
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()))
    }

    /// B7: the shared same-mint sell coordinator lets exactly one producer reserve
    /// a mint. A second reservation of the SAME mint fails until it is released.
    #[test]
    fn test_primary_sell_reservation_blocks_second_same_mint() {
        let active = new_active_sell_mints();
        // First producer reserves the mint.
        assert!(try_reserve_sell_mint(&active, T_MINT));
        // Second producer (e.g. kill-switch) for the same mint is blocked.
        assert!(!try_reserve_sell_mint(&active, T_MINT));
        // A DIFFERENT mint is independently reservable.
        let other_mint = "Mint2222222222222222222222222222222222222222";
        assert!(try_reserve_sell_mint(&active, other_mint));
    }

    /// B7: releasing a mint's reservation allows a later retry to reserve it again.
    #[test]
    fn test_primary_sell_reservation_release_allows_retry() {
        let active = new_active_sell_mints();
        assert!(try_reserve_sell_mint(&active, T_MINT));
        assert!(!try_reserve_sell_mint(&active, T_MINT));
        release_sell_mint(&active, T_MINT);
        // After release the mint is free to reserve again.
        assert!(try_reserve_sell_mint(&active, T_MINT));
    }

    /// B5: an additional-local (multi-wallet) position has an operational exit
    /// route iff a Local exit trader is available. Registry routes the wallet
    /// Local because it is in the local set.
    #[test]
    fn test_operational_exit_route_additional_local_wallet() {
        let primary = Pubkey::new_unique();
        let additional = Pubkey::new_unique();
        // Registry recognizes an ADDITIONAL local wallet (multi-wallet recovery).
        let registry = ExecutionWalletRegistry::new(primary, &[additional], None);
        let pos = canonical_position(T_MINT, &additional.to_string(), "s", 100);
        assert_eq!(
            registry.route_for(&additional),
            Some(crate::wallet::ExecutionRoute::Local)
        );
        // Local trader available => operational.
        assert!(position_has_operational_exit_route(&pos, &registry, true, false));
        // No Local trader => NOT operational (cannot actually sign/submit).
        assert!(!position_has_operational_exit_route(&pos, &registry, false, false));

        // An unknown wallet has no route => never operational.
        let unknown = canonical_position(T_MINT, &Pubkey::new_unique().to_string(), "s", 100);
        assert!(!position_has_operational_exit_route(&unknown, &registry, true, true));
    }

    /// B5: a Lightning-route position has an operational exit route iff a Lightning
    /// exit trader is available. A local trader does NOT satisfy a Lightning route.
    #[test]
    fn test_operational_exit_route_lightning_requires_trader() {
        let primary = Pubkey::new_unique();
        let lightning = Pubkey::new_unique();
        let registry = ExecutionWalletRegistry::new(primary, &[], Some(lightning));
        let pos = canonical_position(T_MINT, &lightning.to_string(), "s", 100);
        assert_eq!(
            registry.route_for(&lightning),
            Some(crate::wallet::ExecutionRoute::Lightning)
        );
        // Lightning trader available => operational.
        assert!(position_has_operational_exit_route(&pos, &registry, false, true));
        // No Lightning trader (even with a local trader) => NOT operational; no
        // Lightning->Local fallback.
        assert!(!position_has_operational_exit_route(&pos, &registry, true, false));

        // Unknown decimals => never operational even with a trader.
        let mut no_dec = pos.clone();
        no_dec.token_decimals = None;
        assert!(!position_has_operational_exit_route(&no_dec, &registry, true, true));
    }

    /// B1: pending recovery is ordered by submission time, then signature. Build
    /// records out of order and confirm the sort is chronological with a signature
    /// tie-break.
    #[test]
    fn test_pending_recovery_sort_is_chronological() {
        use chrono::{Duration, Utc};
        let base = Utc::now();
        let mk = |sig: &str, offset_ms: i64| {
            let mut p = PendingExecution::sell(
                sig.to_string(),
                T_MINT.to_string(),
                "wallet".to_string(),
                PendingSellContext {
                    requested_amount: "100%".to_string(),
                    intent: PendingSellIntent::Full,
                    reason: "r".to_string(),
                },
            );
            p.submitted_at = base + Duration::milliseconds(offset_ms);
            p
        };
        // Two share a submitted_at (tie => signature order "sig-a" < "sig-b").
        let mut items = vec![
            mk("sig-late", 100),
            mk("sig-b", 0),
            mk("sig-a", 0),
        ];
        sort_pending_for_recovery(&mut items);
        let order: Vec<&str> = items.iter().map(|p| p.signature.as_str()).collect();
        assert_eq!(order, vec!["sig-a", "sig-b", "sig-late"]);
    }

    /// B1 explicit tie-break naming coverage (submission time first, then signature).
    #[test]
    fn test_pending_recovery_order_is_submission_time_then_signature() {
        use chrono::{Duration, Utc};
        let base = Utc::now();
        let mk = |sig: &str, offset_ms: i64| {
            let mut p = PendingExecution::sell(
                sig.to_string(),
                T_MINT.to_string(),
                "wallet".to_string(),
                PendingSellContext {
                    requested_amount: "100%".to_string(),
                    intent: PendingSellIntent::Full,
                    reason: "r".to_string(),
                },
            );
            p.submitted_at = base + Duration::milliseconds(offset_ms);
            p
        };
        // Earlier submitted_at wins even when its signature sorts later.
        let mut items = vec![mk("zzz-earlier", -10), mk("aaa-later", 10)];
        sort_pending_for_recovery(&mut items);
        assert_eq!(items[0].signature, "zzz-earlier");
        assert_eq!(items[1].signature, "aaa-later");
    }

    /// B2: a recovered confirmed SELL must be rejected BEFORE close when the open
    /// Position's decimals disagree with the fill decimals. The position is NOT
    /// closed (kept for restart recovery). If the Position is absent, the durable
    /// full-exit receipt replay is still allowed.
    #[tokio::test]
    async fn test_recovered_sell_decimal_mismatch_is_rejected_before_close() {
        let mgr = mem_manager();
        let wallet = Pubkey::new_unique().to_string();
        // Position tracked with decimals Some(6).
        mgr.record_confirmed_position(canonical_position(T_MINT, &wallet, "entry-sig", 100))
            .await
            .unwrap();

        let pending = sell_pending(T_MINT, &wallet, "exit-mismatch", PendingSellIntent::Full);
        // Fill decimals 9 != position decimals 6 => must fail closed, no close.
        let res = apply_recovered_sell(&mgr, &pending, 9, 100, 0.5, PendingSellIntent::Full).await;
        assert!(res.is_err(), "decimal mismatch must be rejected");
        assert!(
            mgr.get_position(T_MINT).await.is_some(),
            "position must NOT be closed on decimal mismatch"
        );

        // Matching decimals (6) proceed to a full close.
        apply_recovered_sell(&mgr, &pending, 6, 100, 0.5, PendingSellIntent::Full)
            .await
            .unwrap();
        assert!(mgr.get_position(T_MINT).await.is_none());
    }

    /// B8: a pending Buy (or Sell) for the mint blocks a primary automatic sell
    /// submission — exercised via the pure predicate both producers use.
    #[test]
    fn test_primary_pending_buy_blocks_sell_submission() {
        let buy = PendingExecution::buy(
            "buysig".to_string(),
            T_MINT.to_string(),
            "wallet".to_string(),
            PendingBuyContext {
                requested_sol: 0.05,
                name: "n".to_string(),
                symbol: "s".to_string(),
                bonding_curve: "bc".to_string(),
                entry_type: crate::position::manager::EntryType::Opportunity,
            },
        );
        // Pending Buy alone blocks (surfaces the buy signature).
        assert_eq!(
            pending_blocks_automatic_sell(Some(&buy), None),
            Some("buysig".to_string())
        );
        let sell = PendingExecution::sell(
            "sellsig".to_string(),
            T_MINT.to_string(),
            "wallet".to_string(),
            PendingSellContext {
                requested_amount: "100%".to_string(),
                intent: PendingSellIntent::Full,
                reason: "r".to_string(),
            },
        );
        // Pending Sell alone blocks (Sell signature preferred when both present).
        assert_eq!(
            pending_blocks_automatic_sell(None, Some(&sell)),
            Some("sellsig".to_string())
        );
        assert_eq!(
            pending_blocks_automatic_sell(Some(&buy), Some(&sell)),
            Some("sellsig".to_string())
        );
        // Neither => not blocked.
        assert_eq!(pending_blocks_automatic_sell(None, None), None);
    }

    // ===================================================================
    // AGENT C — HotScan / manual pending-state boundary tests (C8)
    // ===================================================================

    /// C2: a pending Buy for a mint blocks the HotScan automatic sell exactly as a
    /// pending Sell does. The HotScan sell guard uses the same shared predicate, so
    /// a confirmed-buy whose durable position save failed cannot be closed out from
    /// under an in-flight buy (which restart recovery would otherwise re-open).
    #[test]
    fn test_hotscan_pending_buy_blocks_sell() {
        let buy = PendingExecution::buy(
            "hotscan-buysig".to_string(),
            T_MINT.to_string(),
            "wallet".to_string(),
            PendingBuyContext {
                requested_sol: 0.05,
                name: "n".to_string(),
                symbol: "s".to_string(),
                bonding_curve: "bc".to_string(),
                entry_type: crate::position::manager::EntryType::Opportunity,
            },
        );
        // Pending Buy alone => HotScan must NOT submit a new sell; surfaces the sig.
        assert_eq!(
            pending_blocks_automatic_sell(Some(&buy), None),
            Some("hotscan-buysig".to_string())
        );
        // No pending at all => not blocked (sell may proceed via routed path).
        assert_eq!(pending_blocks_automatic_sell(None, None), None);
    }

    /// C1: an existing Lightning-route position with a configured Lightning wallet
    /// but a MISSING api_key (=> no Lightning trader handle) is operationally
    /// unexitable, which must HALT new HotScan entries. The HotScan C1 block halts
    /// on any position without an operational exit route; this exercises the pure
    /// predicate it uses with `lightning_trader_available = false`.
    #[test]
    fn test_hotscan_existing_lightning_position_without_trader_halts_new_entries() {
        let primary = Pubkey::new_unique();
        let lightning = Pubkey::new_unique();
        // Registry recognizes the Lightning wallet (configured lightning_wallet).
        let registry = ExecutionWalletRegistry::new(primary, &[], Some(lightning));
        let pos = canonical_position(T_MINT, &lightning.to_string(), "s", 100);
        assert_eq!(
            registry.route_for(&lightning),
            Some(crate::wallet::ExecutionRoute::Lightning)
        );

        // Missing api_key => no Lightning trader => NOT operationally exitable.
        let local_trader_available = true; // PumpPortal trading on
        let lightning_trader_available = false; // api_key empty => no lightning trader
        assert!(
            !position_has_operational_exit_route(
                &pos,
                &registry,
                local_trader_available,
                lightning_trader_available,
            ),
            "Lightning position without a Lightning trader must be unexitable"
        );

        // The HotScan C1 halt decision: any unexitable canonical position halts new
        // entries. Mirror that count here.
        let positions = vec![pos];
        let unexitable = positions
            .iter()
            .filter(|p| {
                !position_has_operational_exit_route(
                    p,
                    &registry,
                    local_trader_available,
                    lightning_trader_available,
                )
            })
            .count();
        assert!(
            unexitable > 0,
            "at least one unexitable position => new entries must halt"
        );

        // Sanity: with the Lightning trader available it WOULD be exitable and would
        // not force a halt on its own.
        assert!(position_has_operational_exit_route(&positions[0], &registry, true, true));
    }

    /// C3: after a signature, a failed pending journal write must NOT skip immediate
    /// reconciliation. The pure decision helper confirms that regardless of the
    /// initial persist result the handler still resolves an outcome; and that on a
    /// confirmed fill the pending is removed as usual even if the first write failed.
    #[test]
    fn test_manual_post_signature_journal_failure_still_requires_reconciliation_path() {
        // A confirmed fill after an initial-write failure => still just remove
        // pending (durable economic truth is the applied close + receipt ledger).
        assert_eq!(
            manual_pending_action(false, ManualOutcomeKind::ConfirmedFill),
            ManualPendingAction::RemovePending
        );
        // A confirmed fill after a successful initial write => remove pending too.
        assert_eq!(
            manual_pending_action(true, ManualOutcomeKind::ConfirmedFill),
            ManualPendingAction::RemovePending
        );
        // Neither confirmed-fill case is "return before reconciliation": the only
        // reason a fill action exists is that reconciliation ran to completion.
    }

    /// C4: an ambiguous manual outcome (Unresolved or structural error) whose
    /// initial post-signature journal write FAILED must retry durable persistence
    /// before returning. A durable initial write instead keeps pending as-is.
    #[test]
    fn test_manual_unresolved_requires_pending_durability() {
        // Initial write failed => must retry durable persistence.
        assert_eq!(
            manual_pending_action(false, ManualOutcomeKind::Unresolved),
            ManualPendingAction::RetryDurable
        );
        assert_eq!(
            manual_pending_action(false, ManualOutcomeKind::StructuralError),
            ManualPendingAction::RetryDurable
        );
        // Initial write succeeded => pending already durable, keep it.
        assert_eq!(
            manual_pending_action(true, ManualOutcomeKind::Unresolved),
            ManualPendingAction::KeepDurable
        );
        assert_eq!(
            manual_pending_action(true, ManualOutcomeKind::StructuralError),
            ManualPendingAction::KeepDurable
        );
    }

    /// C4/C6: `ensure_manual_pending_durable` actually persists a pending record so
    /// a later reload observes it — this is the durable-retry primitive the
    /// ambiguous-outcome branches call. Exercised against a real temp-dir store.
    #[tokio::test]
    async fn test_ensure_manual_pending_durable_persists() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pf_manual_durable_{}_{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let path = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        let pending = PendingExecution::sell(
            "durable-sig".to_string(),
            T_MINT.to_string(),
            "wallet".to_string(),
            PendingSellContext {
                requested_amount: "100%".to_string(),
                intent: PendingSellIntent::Manual,
                reason: "manual".to_string(),
            },
        );

        ensure_pending_execution_durable(&store, &pending)
            .await
            .expect("durable retry must succeed for a writable path");

        // Reload from disk: the pending record survived (durable).
        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        assert!(
            reloaded.get("durable-sig").await.is_some(),
            "pending must be durable after ensure_manual_pending_durable"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// C5: a ConfirmedFailure whose initial post-signature write never succeeded
    /// does NOT need a pending failure record invented; if it WAS persisted, remove
    /// it. No pending state is fabricated on a proven on-chain failure.
    #[test]
    fn test_manual_confirmed_failure_does_not_require_pending_after_initial_write_failure() {
        // Initial write never succeeded => nothing to remove, no record to invent.
        assert_eq!(
            manual_pending_action(false, ManualOutcomeKind::ConfirmedFailure),
            ManualPendingAction::RemoveIfPersisted
        );
        // Initial write succeeded => remove the persisted pending record.
        assert_eq!(
            manual_pending_action(true, ManualOutcomeKind::ConfirmedFailure),
            ManualPendingAction::RemoveIfPersisted
        );
    }

    // ===================================================================
    // AUDIT-002 A11 — symmetric post-signature durability tests. These are
    // pure/helper-level and require no network. `pending_durability_required`
    // is the single decision applied on every live submit path (primary buy,
    // primary auto-sell, event kill-switch sell, HotScan buy, HotScan sell,
    // manual sell) once the reconciliation outcome is known.
    // ===================================================================

    /// A11: an Unresolved outcome whose initial pending write FAILED requires a
    /// durability retry; the same outcome after a successful write does not.
    #[test]
    fn test_pending_durability_action_unresolved_requires_retry_after_initial_failure() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::Unresolved));
        assert!(!pending_durability_required(true, SubmittedOutcomeState::Unresolved));
    }

    /// A11: a structural reconciler error whose initial write FAILED requires a
    /// durability retry (same rule as Unresolved).
    #[test]
    fn test_pending_durability_action_structural_error_requires_retry_after_initial_failure() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::StructuralError));
        assert!(!pending_durability_required(true, SubmittedOutcomeState::StructuralError));
    }

    /// A11: a confirmed-but-unapplied fill (identity/validation/application
    /// failure) whose initial write FAILED requires a durability retry so the
    /// confirmed fill is restart-recoverable.
    #[test]
    fn test_pending_durability_action_confirmed_unapplied_requires_retry_after_initial_failure() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::ConfirmedUnapplied));
        assert!(!pending_durability_required(true, SubmittedOutcomeState::ConfirmedUnapplied));
    }

    /// A11: a confirmed fill whose economic state was durably applied does NOT
    /// require an additional durable pending record — the applied close/receipt
    /// (or record_confirmed_position) is the authoritative durable truth.
    #[test]
    fn test_pending_durability_action_confirmed_applied_does_not_require_retry() {
        assert!(!pending_durability_required(false, SubmittedOutcomeState::ConfirmedApplied));
        assert!(!pending_durability_required(true, SubmittedOutcomeState::ConfirmedApplied));
        // And a ConfirmedFailure never requires an invented durable record.
        assert!(!pending_durability_required(false, SubmittedOutcomeState::ConfirmedFailure));
        assert!(!pending_durability_required(true, SubmittedOutcomeState::ConfirmedFailure));
    }

    /// A11: primary buy — an Unresolved outcome after an initial persist failure
    /// is NOT terminal; without a durability retry the submitted signature is not
    /// restart-recoverable, so the decision demands a retry.
    #[test]
    fn test_primary_buy_unresolved_after_initial_persist_failure_is_not_terminal_without_retry() {
        // initial persist failed + Unresolved => must retry (not terminal).
        assert!(pending_durability_required(false, SubmittedOutcomeState::Unresolved));
        // If the retry had already made it durable (modeled as initially_persisted),
        // no further durability action is required.
        assert!(!pending_durability_required(true, SubmittedOutcomeState::Unresolved));
    }

    /// A11: primary auto-sell — a confirmed fill whose local application failed
    /// (confirmed-but-unapplied) after an initial persist failure requires a
    /// durability retry.
    #[test]
    fn test_primary_sell_confirmed_apply_failure_after_initial_persist_failure_requires_retry() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::ConfirmedUnapplied));
    }

    /// A11: HotScan buy — Unresolved after an initial persist failure requires a
    /// durability retry before break.
    #[test]
    fn test_hotscan_buy_unresolved_after_initial_persist_failure_requires_retry() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::Unresolved));
    }

    /// A11: HotScan sell — a confirmed-but-unapplied close failure after an
    /// initial persist failure requires a durability retry.
    #[test]
    fn test_hotscan_sell_confirmed_apply_failure_after_initial_persist_failure_requires_retry() {
        assert!(pending_durability_required(false, SubmittedOutcomeState::ConfirmedUnapplied));
    }

    /// A11/A9: a manual TRACKED confirmed-but-unapplied sell whose early fill
    /// validation fails must go through the durability path when the initial
    /// pending write failed (modeled as ConfirmedUnapplied), rather than bypassing
    /// it via an early return.
    #[test]
    fn test_manual_tracked_early_fill_validation_failure_requires_pending_durability() {
        // Tracked confirmed fill that fails validation with no durable pending =>
        // must retry durable persistence.
        assert!(pending_durability_required(false, SubmittedOutcomeState::ConfirmedUnapplied));
        // A durable initial write needs no retry.
        assert!(!pending_durability_required(true, SubmittedOutcomeState::ConfirmedUnapplied));
    }

    /// A10/A11: the retry helper actually makes an initially-unpersisted pending
    /// record durable against a real temp-dir store, and short-circuits to `true`
    /// when it was already persisted.
    #[tokio::test]
    async fn test_retry_pending_durability_if_needed_persists_when_initial_failed() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pf_retry_durable_{}_{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let path = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        let pending = PendingExecution::sell(
            "retry-durable-sig".to_string(),
            T_MINT.to_string(),
            "wallet".to_string(),
            PendingSellContext {
                requested_amount: "100%".to_string(),
                intent: PendingSellIntent::Manual,
                reason: "manual".to_string(),
            },
        );

        // Already persisted => true without touching the store.
        assert!(retry_pending_durability_if_needed(&store, &pending, true).await);

        // Initially failed => retry persists it durably.
        assert!(retry_pending_durability_if_needed(&store, &pending, false).await);

        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        assert!(
            reloaded.get("retry-durable-sig").await.is_some(),
            "pending must be durable after retry_pending_durability_if_needed"
        );

        let _ = std::fs::remove_file(&path);
    }

    // --- MPT-001 Agent I5: manual-sell quote/route-truth pure tests -----------

    fn manual_test_sell_quote(venue: MarketVenue) -> crate::market::ExecutableQuote {
        crate::market::ExecutableQuote {
            mint: Pubkey::new_unique(),
            side: crate::market::MarketSide::Sell,
            venue,
            quote_asset: crate::market::QuoteAsset::Sol,
            base_decimals: 6,
            quote_decimals: 9,
            base_amount_raw: 1_234_567,
            quote_amount_raw: 40_000_000,
            expected_price_sol_per_token: Some(0.000_032),
            protocol_fee_bps: 100,
            creator_fee_bps: 0,
            lp_fee_bps: 0,
            slot: 77,
            quoted_at: chrono::Utc::now(),
        }
    }

    fn manual_unsupported_sell_quote() -> crate::market::ExecutableQuote {
        crate::market::ExecutableQuote {
            quote_asset: crate::market::QuoteAsset::Unsupported(Pubkey::new_unique()),
            expected_price_sol_per_token: None,
            ..manual_test_sell_quote(MarketVenue::PumpSwapCanonical)
        }
    }

    #[test]
    fn test_manual_decimal_ui_amount_exact_conversion() {
        // I5(1): a numeric UI amount converts to raw EXACTLY via token decimals.
        assert_eq!(decimal_token_amount_to_raw("1.234567", 6).unwrap(), 1_234_567);
        assert_eq!(decimal_token_amount_to_raw("0.5", 6).unwrap(), 500_000);
        assert_eq!(decimal_token_amount_to_raw("2", 6).unwrap(), 2_000_000);
        assert_eq!(decimal_token_amount_to_raw(".5", 6).unwrap(), 500_000);
        // Trailing-zero excess beyond decimals is accepted (all-zero excess).
        assert_eq!(decimal_token_amount_to_raw("1.2300", 6).unwrap(), 1_230_000);
    }

    #[test]
    fn test_manual_rejects_precision_beyond_decimals() {
        // I5(2): more fractional digits than decimals with a NONZERO excess digit
        // is rejected (would silently truncate real precision).
        assert!(decimal_token_amount_to_raw("1.2345678", 6).is_err());
        assert!(decimal_token_amount_to_raw("0.0000001", 6).is_err());
    }

    #[test]
    fn test_manual_rejects_scientific_notation() {
        // I5(3): scientific notation is never accepted for a token amount.
        assert!(decimal_token_amount_to_raw("1e6", 6).is_err());
        assert!(decimal_token_amount_to_raw("1E6", 6).is_err());
        assert!(decimal_token_amount_to_raw("1.5e3", 6).is_err());
        // Signed / malformed inputs also reject.
        assert!(decimal_token_amount_to_raw("-1.0", 6).is_err());
        assert!(decimal_token_amount_to_raw("", 6).is_err());
        assert!(decimal_token_amount_to_raw("1.2.3", 6).is_err());
    }

    #[test]
    fn test_manual_percentage_raw_derivation() {
        // I5(4): a percentage resolves to an EXACT raw proportion of the position.
        assert_eq!(percent_of_raw(1_000_000, "100").unwrap(), 1_000_000);
        assert_eq!(percent_of_raw(1_000_000, "50").unwrap(), 500_000);
        assert_eq!(percent_of_raw(1_000_000, "25").unwrap(), 250_000);
        assert_eq!(percent_of_raw(1_234_567, "10").unwrap(), 123_456); // floored
        // Zero-resulting proportion refuses (nothing to sell).
        assert!(percent_of_raw(1, "1").is_err());
    }

    #[test]
    fn test_manual_quote_precedes_submit_decision() {
        // I5(5): the submit decision REQUIRES Some(quote); absence => Refuse. This
        // encodes that a fresh quote must be obtained before any submit choice.
        assert_eq!(manual_sell_decision(None), ManualSellDecision::Refuse);
        let q = manual_test_sell_quote(MarketVenue::PumpBondingCurve);
        assert!(matches!(
            manual_sell_decision(Some(&q)),
            ManualSellDecision::Submit { .. }
        ));
    }

    #[test]
    fn test_manual_venue_pin() {
        // I5(6): the submitted pool is pinned to the quoted venue (never Auto).
        let pump = manual_test_sell_quote(MarketVenue::PumpBondingCurve);
        assert_eq!(
            manual_sell_decision(Some(&pump)),
            ManualSellDecision::Submit {
                pool: PoolType::Pump
            }
        );
        let amm = manual_test_sell_quote(MarketVenue::PumpSwapCanonical);
        assert_eq!(
            manual_sell_decision(Some(&amm)),
            ManualSellDecision::Submit {
                pool: PoolType::PumpAmm
            }
        );
        for v in [MarketVenue::PumpBondingCurve, MarketVenue::PumpSwapCanonical] {
            let q = manual_test_sell_quote(v);
            assert_ne!(
                manual_sell_decision(Some(&q)),
                ManualSellDecision::Submit {
                    pool: PoolType::Auto
                }
            );
        }
    }

    #[test]
    fn test_manual_unsupported_quote_mint_refuses() {
        // I5(7): a non-SOL (unsupported) quote mint refuses the normal manual sell.
        let q = manual_unsupported_sell_quote();
        assert_eq!(manual_sell_decision(Some(&q)), ManualSellDecision::Refuse);
    }

    #[test]
    fn test_manual_sell_drift_guards_expected() {
        // I4: drift only computed for finite positive expected; never fabricated.
        assert_eq!(manual_sell_drift_pct(0.0, 1.0), None);
        assert_eq!(manual_sell_drift_pct(f64::NAN, 1.0), None);
        assert_eq!(manual_sell_drift_pct(1.0, f64::NAN), None);
        // Sell: fill worse than quote => positive drift.
        let d = manual_sell_drift_pct(100.0, 97.0).unwrap();
        assert!((d - 3.0).abs() < 1e-9);
        // Price improvement => negative drift.
        let d2 = manual_sell_drift_pct(100.0, 101.0).unwrap();
        assert!((d2 + 1.0).abs() < 1e-9);
    }

    // --- BLOCKER B: FINAL manual-sell quote validation (two-quote semantics) ---

    /// Fully-specified FINAL-quote fixture (struct literal) so each test can pin
    /// mint / raw size / side / venue / expected price exactly.
    fn final_sell_quote(
        mint: Pubkey,
        base_amount_raw: u64,
        side: crate::market::MarketSide,
        venue: MarketVenue,
        quote_asset: crate::market::QuoteAsset,
        expected_price_sol_per_token: Option<f64>,
    ) -> crate::market::ExecutableQuote {
        crate::market::ExecutableQuote {
            mint,
            side,
            venue,
            quote_asset,
            base_decimals: 6,
            quote_decimals: 9,
            base_amount_raw,
            quote_amount_raw: 40_000_000,
            expected_price_sol_per_token,
            protocol_fee_bps: 100,
            creator_fee_bps: 0,
            lp_fee_bps: 0,
            slot: 12_345,
            quoted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_final_quote_requires_sell_side() {
        // B(8): a Buy-side final quote is rejected (must be Sell).
        let mint = Pubkey::new_unique();
        let q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Buy,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        assert!(validate_final_manual_sell_quote(&mint, 1_000, &q).is_err());
    }

    #[test]
    fn test_final_quote_requires_exact_mint() {
        // B(8): a final quote for a DIFFERENT mint is rejected.
        let intended = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let q = final_sell_quote(
            other,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        assert!(validate_final_manual_sell_quote(&intended, 1_000, &q).is_err());
        // Same mint (control) passes.
        let ok = final_sell_quote(
            intended,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        assert!(validate_final_manual_sell_quote(&intended, 1_000, &ok).is_ok());
    }

    #[test]
    fn test_final_quote_requires_exact_raw_amount() {
        // B(8): a final quote sized differently than the resolved raw is rejected.
        let mint = Pubkey::new_unique();
        let q = final_sell_quote(
            mint,
            999,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        assert!(validate_final_manual_sell_quote(&mint, 1_000, &q).is_err());
    }

    #[test]
    fn test_final_quote_requires_sol_pair() {
        // B(8): a non-SOL (unsupported) quote asset is rejected.
        let mint = Pubkey::new_unique();
        let q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpSwapCanonical,
            crate::market::QuoteAsset::Unsupported(Pubkey::new_unique()),
            Some(0.001),
        );
        assert!(validate_final_manual_sell_quote(&mint, 1_000, &q).is_err());
    }

    #[test]
    fn test_final_quote_requires_finite_positive_price() {
        // B(8): expected price must be Some(finite > 0). None / 0 / NaN / Inf all reject.
        let mint = Pubkey::new_unique();
        for price in [None, Some(0.0), Some(-1.0), Some(f64::NAN), Some(f64::INFINITY), Some(f64::NEG_INFINITY)] {
            let q = final_sell_quote(
                mint,
                1_000,
                crate::market::MarketSide::Sell,
                MarketVenue::PumpBondingCurve,
                crate::market::QuoteAsset::Sol,
                price,
            );
            assert!(
                validate_final_manual_sell_quote(&mint, 1_000, &q).is_err(),
                "price {:?} must be rejected",
                price
            );
        }
        // A finite positive price passes.
        let ok = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.000_001),
        );
        assert!(validate_final_manual_sell_quote(&mint, 1_000, &ok).is_ok());
    }

    #[test]
    fn test_final_quote_pump_venue_pins_pump_pool() {
        // B(9): a valid Pump final quote pins PoolType::Pump (never Auto).
        let mint = Pubkey::new_unique();
        let q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        let pool = validate_final_manual_sell_quote(&mint, 1_000, &q).unwrap();
        assert_eq!(pool, PoolType::Pump);
        assert_ne!(pool, PoolType::Auto);
    }

    #[test]
    fn test_final_quote_pumpswap_venue_pins_pumpamm_pool() {
        // B(9): a valid PumpSwap final quote pins PoolType::PumpAmm (never Auto).
        let mint = Pubkey::new_unique();
        let q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpSwapCanonical,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        let pool = validate_final_manual_sell_quote(&mint, 1_000, &q).unwrap();
        assert_eq!(pool, PoolType::PumpAmm);
        assert_ne!(pool, PoolType::Auto);
    }

    #[test]
    fn test_preview_pump_final_pumpswap_submits_pumpamm() {
        // B: token graduated during the human delay. Preview was Pump but the
        // FINAL quote is PumpSwap; the derived pool MUST come from the FINAL quote
        // (=> PumpAmm), regardless of the preview venue, with no re-confirmation.
        let mint = Pubkey::new_unique();
        // Preview venue is Pump (display-only) — asserted here for intent clarity.
        let preview = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(0.001),
        );
        assert_eq!(pumpportal_pool_for_venue(preview.venue), PoolType::Pump);
        // FINAL quote is PumpSwap; the pool derived from the FINAL quote is PumpAmm.
        let final_q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpSwapCanonical,
            crate::market::QuoteAsset::Sol,
            Some(0.002),
        );
        let pool = validate_final_manual_sell_quote(&mint, 1_000, &final_q).unwrap();
        assert_eq!(pool, PoolType::PumpAmm);
        // The derived pool ignores the preview venue entirely.
        assert_ne!(pool, pumpportal_pool_for_venue(preview.venue));
    }

    #[test]
    fn test_drift_reference_uses_final_not_preview_quote() {
        // B(11): the quote-to-fill drift reference is the FINAL quote's expected
        // price, never the preview's. Given preview.expected != final.expected, the
        // drift is computed from final.expected.
        let mint = Pubkey::new_unique();
        let preview = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(200.0), // preview price (stale)
        );
        let final_q = final_sell_quote(
            mint,
            1_000,
            crate::market::MarketSide::Sell,
            MarketVenue::PumpBondingCurve,
            crate::market::QuoteAsset::Sol,
            Some(100.0), // fresh final price actually used for drift
        );
        // Mirror the handler: expected_quote_price is taken from the FINAL quote.
        let expected_quote_price = final_q.expected_price_sol_per_token;
        assert_ne!(
            expected_quote_price,
            preview.expected_price_sol_per_token
        );
        let actual_fill_price = 97.0;
        let drift = expected_quote_price
            .and_then(|p| manual_sell_drift_pct(p, actual_fill_price))
            .unwrap();
        // From FINAL (100 -> 97) drift is +3%. From the preview (200) it would be
        // +51.5%; assert we got the FINAL-derived value.
        assert!((drift - 3.0).abs() < 1e-9);
        let preview_drift = preview
            .expected_price_sol_per_token
            .and_then(|p| manual_sell_drift_pct(p, actual_fill_price))
            .unwrap();
        assert!((drift - preview_drift).abs() > 1.0);
    }

    // =======================================================================
    // AGENT D (D13) — authenticated position-scoped event wiring, pure tests.
    // No network / no socket: they exercise the extracted pure helpers only.
    // =======================================================================

    // (1) missing data key on a LIVE run => new-entry stream gate is false.
    #[test]
    fn test_d13_missing_data_key_live_blocks_new_entries() {
        assert!(missing_data_key_halts_new_entries(false, true, ""));
        assert!(missing_data_key_halts_new_entries(false, true, "   "));
        // With a key present it does not halt on this rule.
        assert!(!missing_data_key_halts_new_entries(false, true, "KEY"));
        // The new-entry admission predicate then reflects the halt.
        assert!(!new_entry_admitted(true, true, true));
    }

    // (2) dry-run free-only plan without key is allowed (no forced halt, plan has
    //     no trade subscriptions to require a key).
    #[test]
    fn test_d13_dry_run_free_only_plan_without_key_allowed() {
        // Dry-run never halted by the missing-key rule.
        assert!(!missing_data_key_halts_new_entries(true, true, ""));
        // A free-only plan (no open positions, tracking disabled) carries no trade
        // subscriptions, so it needs no key.
        let plan = build_initial_subscription_plan(&[], &["w".to_string()], false);
        assert!(plan.new_tokens && plan.migrations);
        assert!(plan.token_trades.is_empty());
        assert!(plan.account_trades.is_empty());
        // Dry-run admission is not gated on data-stream readiness.
        assert!(new_entry_admitted(false, false, true) == false); // live-shape gate
    }

    // (3) initial plan contains open-position mints (deduplicated).
    #[test]
    fn test_d13_initial_plan_contains_open_position_mints() {
        let mints = vec!["mintA".to_string(), "mintB".to_string(), "mintA".to_string()];
        let plan = build_initial_subscription_plan(&mints, &[], false);
        assert_eq!(plan.token_trades, vec!["mintA".to_string(), "mintB".to_string()]);
    }

    // (4) initial plan contains tracked wallets ONLY when tracking is enabled.
    #[test]
    fn test_d13_initial_plan_tracked_wallets_only_when_enabled() {
        let wallets = vec!["w1".to_string(), "w2".to_string(), "w1".to_string()];
        let enabled = build_initial_subscription_plan(&[], &wallets, true);
        assert_eq!(enabled.account_trades, vec!["w1".to_string(), "w2".to_string()]);
        let disabled = build_initial_subscription_plan(&[], &wallets, false);
        assert!(disabled.account_trades.is_empty());
    }

    // (5) no all-trades plan: new/migration are booleans, trade sets are explicit
    //     key lists, and an empty inputs plan never fabricates keys.
    #[test]
    fn test_d13_no_all_trades_plan() {
        let plan = build_initial_subscription_plan(&[], &[], true);
        assert!(plan.token_trades.is_empty());
        assert!(plan.account_trades.is_empty());
        // Even with tracking enabled but no wallets, no keys are invented.
    }

    // ==================================================================
    // AGENT C (C9) — provider unit bridge + readiness + health tests
    // ==================================================================

    fn c9_filter_config() -> crate::config::FilterConfig {
        crate::config::FilterConfig {
            enabled: true,
            blocked_patterns: vec!["(?i)scam".to_string()],
            name_patterns: vec![],
            ..crate::config::Config::default().filters
        }
    }

    // C9.1: the live name/symbol filter path requires no fabricated slot or
    // default Pubkey. `filter_name_symbol` takes only &str name+symbol and must
    // match the prior event-based filter for the same name/symbol — proving the
    // adapter (slot=0 + Pubkey::default) is unnecessary.
    #[test]
    fn test_provider_name_filter_requires_no_slot_or_default_pubkey_adapter() {
        use crate::filter::token_filter::TokenFilter;
        use crate::stream::decoder::TokenCreatedEvent;
        let filter = TokenFilter::new(c9_filter_config()).unwrap();

        // name/symbol-only API: no slot, no Pubkey needed.
        assert!(filter.filter_name_symbol("ScamCoin", "SCAM").is_filtered());
        assert!(filter.filter_name_symbol("GoodToken", "GOOD").is_pass());

        // Equivalence with the legacy event wrapper for the SAME name/symbol,
        // regardless of the event's slot/Pubkey identity fields.
        let ev = TokenCreatedEvent {
            signature: "x".to_string(),
            slot: 999,
            mint: solana_sdk::pubkey::Pubkey::new_unique(),
            name: "GoodToken".to_string(),
            symbol: "GOOD".to_string(),
            uri: String::new(),
            bonding_curve: solana_sdk::pubkey::Pubkey::new_unique(),
            associated_bonding_curve: solana_sdk::pubkey::Pubkey::new_unique(),
            creator: solana_sdk::pubkey::Pubkey::new_unique(),
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(
            filter.filter(&ev).is_pass(),
            filter.filter_name_symbol("GoodToken", "GOOD").is_pass()
        );
    }

    // C9.2: the bonding-curve filter uses provider SOL directly (no /1e9). A
    // provider value of 57.5 SOL is 50% progress; a lamport-style 57.5e9 would
    // clamp to 100% under the correct SOL heuristic (proving no /1e9 divide).
    #[test]
    fn test_provider_bonding_curve_filter_uses_sol_not_lamports() {
        // 30 SOL => 0%, 57.5 SOL => 50%, 85 SOL => 100%.
        assert!((SignalContext::calculate_bonding_curve_pct(30.0) - 0.0).abs() < 1e-9);
        assert!((SignalContext::calculate_bonding_curve_pct(57.5) - 50.0).abs() < 1e-9);
        assert!((SignalContext::calculate_bonding_curve_pct(85.0) - 100.0).abs() < 1e-9);
        // If this had a /1e9 divide, 57.5 SOL would map to ~0% (near curve start).
        assert!(SignalContext::calculate_bonding_curve_pct(57.5) > 1.0);
    }

    // C9.3: fractional provider values survive the SignalContext bridge with no
    // flooring/truncation (the old `as u64` path truncated 31.75 -> 31).
    #[test]
    fn test_signal_context_bridge_preserves_fractional_provider_values() {
        let ctx = SignalContext::from_new_token(
            "mint".to_string(),
            "n".to_string(),
            "s".to_string(),
            "u".to_string(),
            "creator".to_string(),
            "bc".to_string(),
            1.25,  // initial_buy
            2.5,   // v_tokens_in_bonding_curve
            31.75, // v_sol_in_bonding_curve
            9.9,   // market_cap_sol
        );
        assert_eq!(ctx.initial_buy, 1.25);
        assert_eq!(ctx.v_tokens_in_bonding_curve, 2.5);
        assert_eq!(ctx.v_sol_in_bonding_curve, 31.75);
        assert_eq!(ctx.market_cap_sol, 9.9);
    }

    // C9.4: the strategy placeholder liquidity is provider observational SOL used
    // DIRECTLY (no dual SOL/lamport branch). Mirror the production expression.
    #[test]
    fn test_provider_strategy_placeholder_vsol_is_direct_sol() {
        for v_sol in [12.5_f64, 31.75, 850.0, 30_000.0] {
            let liquidity_sol = v_sol; // production: `let liquidity_sol = token.v_sol_in_bonding_curve;`
            assert_eq!(liquidity_sol, v_sol, "no /1e9, no <1000 branch");
        }
    }

    // C9.5: a confirmed durable buy closes the readiness gate before subscribing
    // exactly when a dynamic subscription is required (feed + key). No key or
    // disabled feed => no readiness close (nothing to subscribe).
    #[test]
    fn test_confirmed_buy_subscription_closes_readiness_until_sync() {
        assert!(confirmed_buy_closes_readiness_until_sync(true, "KEY"));
        assert!(!confirmed_buy_closes_readiness_until_sync(true, ""));
        assert!(!confirmed_buy_closes_readiness_until_sync(false, "KEY"));
        // Consistent with the required-subscription decision it gates.
        assert_eq!(
            confirmed_buy_closes_readiness_until_sync(true, "KEY"),
            confirmed_position_requires_subscription(true, "KEY")
        );
    }

    // C9.6: readiness reopens ONLY on a Connected event (post-sync), never set
    // true locally after a subscribe. Model the Connected/Disconnected policy the
    // main loop applies to the shared flag.
    #[test]
    fn test_connected_event_reopens_readiness_after_sync_policy() {
        let ready = std::sync::atomic::AtomicBool::new(true);
        // C5: closing the gate before subscribe.
        ready.store(false, Ordering::SeqCst);
        assert!(!ready.load(Ordering::SeqCst));
        // A rejected send must NOT reopen readiness.
        // (No local store(true) exists on the reject path.)
        assert!(!ready.load(Ordering::SeqCst));
        // Only the Connected handler reopens it (after desired sync).
        ready.store(true, Ordering::SeqCst); // Connected handler
        assert!(ready.load(Ordering::SeqCst));
    }

    // C9.7: a valid ACTIVE runtime lock blocks the health live socket check.
    #[test]
    fn test_health_valid_active_lock_blocks_socket() {
        let dir = tempfile::tempdir().unwrap();
        let _lease = RuntimeLease::acquire(dir.path(), "start").expect("acquires");
        let inspect = RuntimeLease::inspect(dir.path());
        assert_eq!(health_lock_policy(&inspect), HealthLockPolicy::SkipActive);
        // SkipActive never opens a socket.
        assert_ne!(health_lock_policy(&inspect), HealthLockPolicy::AllowSocket);
    }

    // C9.8: a malformed/unreadable runtime lock fails closed: skip socket AND
    // mark unhealthy. inspect Err is never "no runtime".
    #[test]
    fn test_health_malformed_lock_blocks_socket() {
        // Synthesize an inspect Err via a typed error result.
        let err: std::result::Result<Option<()>, String> = Err("malformed".to_string());
        assert_eq!(health_lock_policy(&err), HealthLockPolicy::SkipUnhealthy);
        // Never AllowSocket on unknown lock state.
        assert_ne!(health_lock_policy(&err), HealthLockPolicy::AllowSocket);
    }

    // C9.9: a known-absent lock MAY open the socket when a key is present.
    #[test]
    fn test_health_known_absent_lock_may_open_socket_when_key_present() {
        let dir = tempfile::tempdir().unwrap();
        let inspect = RuntimeLease::inspect(dir.path()); // Ok(None): no lock file
        assert_eq!(health_lock_policy(&inspect), HealthLockPolicy::AllowSocket);
        // AllowSocket + key present => open; AllowSocket + no key => skip.
        assert!(health_should_open_socket(false, true));
        assert!(!health_should_open_socket(false, false));
    }

    // C9.10: the sanitized health connect error never contains a fake api key or
    // an authenticated URL. Assert on the fixed message the C8 path emits.
    #[test]
    fn test_health_socket_error_text_contains_no_fake_api_key() {
        let fake_key = "SECRET_API_KEY_ABC123";
        // This is the exact fixed message the C8 connect-failure path returns.
        let sanitized = "PumpPortal WebSocket connection failed for configured base endpoint";
        assert!(!sanitized.contains(fake_key));
        assert!(!sanitized.contains("api-key"));
        assert!(!sanitized.to_lowercase().contains("wss://"));
        assert!(!sanitized.contains('?')); // no query string
    }

    // (6) provider disconnect blocks new entries but does NOT gate the exit
    //     policy helper (which is readiness-agnostic).
    #[test]
    fn test_d13_disconnect_blocks_entries_not_exits() {
        // Feed enabled + not ready => no new entry.
        assert!(!new_entry_admitted(false, false, true));
        // Feed enabled + ready => admitted.
        assert!(new_entry_admitted(false, true, true));
        // Exit-policy helper is independent of stream readiness: a full close still
        // requests an unsubscribe regardless of data_stream_ready.
        assert!(full_close_requests_unsubscribe(true));
        assert!(!full_close_requests_unsubscribe(false));
    }

    // (7) confirmed position requires a dynamic token subscription (pure decision)
    //     when the feed is enabled AND a key is configured.
    #[test]
    fn test_d13_confirmed_position_requires_subscription() {
        assert!(confirmed_position_requires_subscription(true, "KEY"));
        // No key => cannot subscribe (authenticated stream).
        assert!(!confirmed_position_requires_subscription(true, ""));
        // Feed disabled => no subscription needed.
        assert!(!confirmed_position_requires_subscription(false, "KEY"));
    }

    // (8) partial close keeps the subscription.
    #[test]
    fn test_d13_partial_close_keeps_subscription() {
        assert!(!full_close_requests_unsubscribe(false));
    }

    // (9) full close requests unsubscribe.
    #[test]
    fn test_d13_full_close_requests_unsubscribe() {
        assert!(full_close_requests_unsubscribe(true));
    }

    // (10) provider trade kill switch uses identity-only helper semantics: with no
    //      command sender, a best-effort send fails closed (returns false) and the
    //      caller keeps the position — the send helper never panics or blocks.
    #[tokio::test]
    async fn test_d13_provider_identity_send_without_sender_fails_closed() {
        let none: Option<CommandSender> = None;
        let sent = send_subscription_command(
            &none,
            SubscriptionCommand::SubscribeTokenTrades(vec!["mint".to_string()]),
        )
        .await;
        assert!(!sent, "no sender => command not accepted, must fail closed");

        // NOTE: the previous live-channel half built a raw
        // `mpsc::Sender<SubscriptionCommand>` and passed it as `CommandSender`.
        // After Agent A, `CommandSender` is a clonable struct (not an mpsc alias)
        // whose fields are private to the stream module and whose `send()` requires
        // a started client + live worker (network). That half is dropped here; the
        // `None => fails closed` assertion above is the load-bearing invariant.
    }

    // === AGENT E8 — runtime-ownership + health secret-safety tests ==========

    // (1) start and HotScan lease the same dir => second acquire conflicts.
    #[test]
    fn test_e8_start_and_hotscan_same_dir_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let lease = RuntimeLease::acquire(dir.path(), "start").expect("first acquires");
        let err = RuntimeLease::acquire(dir.path(), "hot_scan");
        assert!(
            err.is_err(),
            "second runtime in same credentials_dir must fail closed"
        );
        drop(lease);
        // After release the dir is free again.
        let _reacquired = RuntimeLease::acquire(dir.path(), "hot_scan")
            .expect("acquires after prior lease dropped");
    }

    // (2) manual sell uses the exact "sell" command label, and that label is what
    //     lands in the lease metadata.
    #[test]
    fn test_e8_manual_sell_lease_command_label() {
        assert_eq!(manual_sell_lease_label(), "sell");
        let dir = tempfile::tempdir().unwrap();
        let _lease =
            RuntimeLease::acquire(dir.path(), manual_sell_lease_label()).expect("acquires");
        let meta = RuntimeLease::inspect(dir.path())
            .expect("inspect ok")
            .expect("lease present");
        assert_eq!(meta.command, "sell");
    }

    // (3) wallet transfer (and the other mutating commands) classified as
    //     requiring the exclusive runtime lease.
    #[test]
    fn test_e8_wallet_transfer_classified_mutating() {
        for cmd in [
            "start",
            "hot_scan",
            "sell",
            "wallet_add",
            "wallet_extract",
            "wallet_transfer",
        ] {
            assert!(
                command_requires_runtime_lease(cmd),
                "{cmd} must require the runtime lease"
            );
        }
    }

    // (4) wallet emergency + read-only commands classified NON-exclusive.
    #[test]
    fn test_e8_emergency_and_readonly_non_exclusive() {
        for cmd in [
            "wallet_emergency",
            "status",
            "config",
            "health",
            "wallet_status",
            "wallet_list",
            "wallet_history",
            "scan",
        ] {
            assert!(
                !command_requires_runtime_lease(cmd),
                "{cmd} must NOT require the runtime lease"
            );
        }
    }

    // (5) health policy helper: when a runtime is active, never open a second
    //     PumpPortal socket (regardless of key presence).
    #[test]
    fn test_e8_health_active_runtime_avoids_second_socket() {
        assert!(!health_should_open_socket(true, true));
        assert!(!health_should_open_socket(true, false));
        // No active runtime: may open a socket only if a key exists.
        assert!(health_should_open_socket(false, true));
        assert!(!health_should_open_socket(false, false));
    }

    // (6) health capability line never contains a sample API key.
    #[test]
    fn test_e8_health_capability_line_has_no_secret() {
        let sample_key = "super-secret-sample-api-key-1234567890";
        // AUDIT-003 B3: the older redundant pumpportal_capability_line was removed;
        // the retained authoritative capability formatter is
        // health_data_capability_line. Presence path is driven by a bool, so the
        // secret cannot leak into it.
        let present = health_data_capability_line(true);
        let absent = health_data_capability_line(false);
        assert!(!present.contains(sample_key));
        assert!(!absent.contains(sample_key));
        assert_eq!(
            present,
            "Data: authenticated/metered token+account trade streams available"
        );
        assert_eq!(
            absent,
            "Data: new-token/migration only; trade subscriptions unavailable"
        );
    }

    // === BLOCKER B — health execution-route vs Data-capability truth ==========

    // (B1) No api-key => LOCAL execution (start(): use_local_api = api_key.is_empty()).
    #[test]
    fn test_health_execution_mode_no_key_is_local() {
        assert_eq!(
            health_execution_mode(false, false),
            HealthExecutionMode::Local
        );
        // force_local_api is irrelevant when there is no key.
        assert_eq!(
            health_execution_mode(false, true),
            HealthExecutionMode::Local
        );
    }

    // (B2) api-key present + force_local_api=true => LOCAL execution. This is the
    //      exact bug BLOCKER B fixes: a key used only for the Data API while the
    //      trade route is pinned Local.
    #[test]
    fn test_health_execution_mode_key_plus_force_local_is_local() {
        assert_eq!(
            health_execution_mode(true, true),
            HealthExecutionMode::Local
        );
    }

    // (B3) api-key present + force_local_api=false => LIGHTNING execution.
    #[test]
    fn test_health_execution_mode_key_without_force_is_lightning() {
        assert_eq!(
            health_execution_mode(true, false),
            HealthExecutionMode::Lightning
        );
    }

    // (B4) a configured key with force_local_api=true yields LOCAL execution AND a
    //      Data-available capability line: the Data credential is independent of the
    //      execution route, and key presence never by itself implies Lightning.
    #[test]
    fn test_data_capability_key_does_not_imply_lightning_execution() {
        let api_key_present = true;
        let force_local_api = true;

        // Execution route is LOCAL despite the key being present.
        assert_eq!(
            health_execution_mode(api_key_present, force_local_api),
            HealthExecutionMode::Local
        );

        // Data capability is AVAILABLE because the key authenticates the Data API,
        // independent of the (Local) execution route.
        let data_line = health_data_capability_line(api_key_present);
        assert_eq!(
            data_line,
            "Data: authenticated/metered token+account trade streams available"
        );

        // Sanity: the Local execution line must NOT claim Lightning.
        let exec_line = health_execution_line(
            health_execution_mode(api_key_present, force_local_api),
            api_key_present,
            force_local_api,
        );
        assert!(exec_line.contains("LOCAL MODE"));
        assert!(!exec_line.contains("LIGHTNING"));
    }

    // (B5) the formatted health lines never contain the api-key. Formatting is a
    //      pure function of booleans + the computed mode, so a sample secret can
    //      never leak into either the execution or the Data line.
    #[test]
    fn test_health_execution_mode_text_contains_no_api_key() {
        let sample_key = "super-secret-sample-api-key-XYZ-9876543210";
        let api_key_present = true; // a key IS set in this scenario

        for force_local_api in [true, false] {
            let mode = health_execution_mode(api_key_present, force_local_api);
            let exec_line = health_execution_line(mode, api_key_present, force_local_api);
            let data_line = health_data_capability_line(api_key_present);

            assert!(
                !exec_line.contains(sample_key),
                "execution line leaked the api-key"
            );
            assert!(
                !data_line.contains(sample_key),
                "data line leaked the api-key"
            );
        }
    }

    // (B5.1) no key + force false => LOCAL/no-key text.
    #[test]
    fn test_health_execution_line_no_key_force_false_says_no_key() {
        let mode = health_execution_mode(false, false);
        let line = health_execution_line(mode, false, false);
        assert!(line.contains("LOCAL MODE"));
        assert!(line.contains("no API key"));
        assert!(!line.contains("Data API credential"));
    }

    // (B5.2) no key + force TRUE => STILL LOCAL/no-key text (the AUDIT-003 bug).
    #[test]
    fn test_health_execution_line_no_key_force_true_says_no_key() {
        let mode = health_execution_mode(false, true);
        let line = health_execution_line(mode, false, true);
        assert!(line.contains("LOCAL MODE"));
        assert!(line.contains("no API key"));
    }

    // (B5.3) no key + force TRUE must NEVER claim a Data API credential.
    #[test]
    fn test_health_execution_line_no_key_force_true_never_claims_data_credential() {
        let mode = health_execution_mode(false, true);
        let line = health_execution_line(mode, false, true);
        assert!(
            !line.contains("Data API credential"),
            "no-key line falsely claimed a Data API credential"
        );
    }

    // (B5.4) key present + force TRUE => LOCAL, and here the credential claim is
    //        legitimate because a key actually exists.
    #[test]
    fn test_health_execution_line_key_force_true_reports_local() {
        let mode = health_execution_mode(true, true);
        let line = health_execution_line(mode, true, true);
        assert!(line.contains("LOCAL MODE"));
        assert!(line.contains("force_local_api"));
        assert!(line.contains("Data API credential configured"));
        assert!(!line.contains("LIGHTNING"));
    }

    // (B5.5) key present + no force => LIGHTNING.
    #[test]
    fn test_health_execution_line_key_no_force_reports_lightning() {
        let mode = health_execution_mode(true, false);
        let line = health_execution_line(mode, true, false);
        assert!(line.contains("LIGHTNING MODE"));
        assert!(!line.contains("LOCAL"));
    }

    // AUDIT-004 (B6): output policy — health() emits exactly ONE live
    //      data-capability print site, and that site lives under the PumpPortal
    //      ENABLED section, NOT the `use_for_trading` execution block. Data/event
    //      capability is independent of the trade-execution route, so the print
    //      must be gated by `pumpportal.enabled`. Enforce both facts by scanning
    //      this module's own source, split across fragments so the assertions do
    //      not count themselves.
    #[test]
    fn test_health_data_capability_print_site_is_single() {
        let src = include_str!("commands.rs");
        // The removed AUDIT-003 duplicate helper must not be reintroduced.
        let helper = concat!("pumpportal_", "capability_line(");
        assert!(
            !src.contains(helper),
            "the removed duplicate capability helper reappeared"
        );
        // Exactly one live print site feeds the authoritative Data line.
        let prefix = concat!("PumpPortal Data ", "API... {}");
        let data_line_prints = src.matches(prefix).count();
        assert_eq!(
            data_line_prints, 1,
            "expected exactly one health data-capability print site, found {}",
            data_line_prints
        );
    }

    // AUDIT-004: prove SEMANTICS, not just "one print site" — the sole live Data
    //      print site must sit inside the `if config.pumpportal.enabled {` block
    //      and OUTSIDE the `if config.pumpportal.use_for_trading {` block. This is
    //      the structural replacement for the AUDIT-003 test that only asserted the
    //      print was inside `use_for_trading`.
    #[test]
    fn test_health_data_capability_is_tied_to_pumpportal_enabled_not_trading_route() {
        // The pure predicate depends ONLY on `enabled`, never on the exec route.
        assert!(health_should_report_pumpportal_data(true));
        assert!(!health_should_report_pumpportal_data(false));

        let src = include_str!("commands.rs");
        let prefix = concat!("PumpPortal Data ", "API... {}");
        let print_at = src
            .find(prefix)
            .expect("expected a live Data-capability print site");

        // Byte offset of the enabled-section guard that must precede the print.
        let enabled_guard = concat!("if config.pumpportal.", "enabled {");
        let enabled_at = src
            .find(enabled_guard)
            .expect("expected the pumpportal.enabled health guard");
        assert!(
            enabled_at < print_at,
            "Data print site must be inside the pumpportal.enabled section"
        );

        // Byte offset of the trading-route guard in health(). The Data print must
        // NOT live after it (it belongs to the enabled section that precedes it).
        let trading_guard = concat!("if config.pumpportal.", "use_for_trading {");
        // There are several `use_for_trading` guards in the file; find the one that
        // opens the health() Trading-API block by anchoring on its distinctive
        // downstream marker.
        let trading_marker = "PumpPortal Trading API... ";
        let trading_marker_at = src
            .find(trading_marker)
            .expect("expected the health() Trading API block");
        // The Data print site precedes the Trading API block entirely.
        assert!(
            print_at < trading_marker_at,
            "Data print site must precede (not live inside) the Trading API block"
        );
        // And the trading guard exists (route reporting retained).
        assert!(
            src[..trading_marker_at].contains(trading_guard),
            "expected the use_for_trading guard ahead of the Trading API block"
        );
    }

    // AUDIT-004: enabled + Jito execution (use_for_trading=false) still reports the
    //      PumpPortal Data capability, and the execution route is NOT PumpPortal.
    #[test]
    fn test_health_jito_execution_still_reports_pumpportal_data_when_enabled() {
        let pumpportal_enabled = true;
        let use_for_trading = false;
        // Data capability is driven by `enabled`, so it reports even with Jito exec.
        assert!(health_should_report_pumpportal_data(pumpportal_enabled));
        // Execution route is NOT PumpPortal in this config: the Trading-API block is
        // gated on `use_for_trading`, which is false here, so no PumpPortal exec line.
        assert!(!use_for_trading);
    }

    // AUDIT-004: disabled PumpPortal never reports Data capability, even if an
    //      unused api-key happens to be configured.
    #[test]
    fn test_health_disabled_pumpportal_does_not_report_data_capability() {
        assert!(!health_should_report_pumpportal_data(false));
    }

    // (6b) masked config display (Agent B hardened) exposes neither the
    //      PumpPortal api_key nor the Helius key embedded in the RPC URL.
    #[test]
    fn test_e8_masked_config_hides_keys() {
        let mut config = Config::default();
        config.pumpportal.api_key = "super-secret-sample-api-key-1234567890".to_string();
        config.rpc.endpoint =
            "https://mainnet.helius-rpc.com/?api-key=helius-secret-abcdef".to_string();
        let display = config.masked_display();
        assert!(
            !display.contains("super-secret-sample-api-key-1234567890"),
            "masked config must not contain the PumpPortal api_key"
        );
        assert!(
            !display.contains("helius-secret-abcdef"),
            "masked config must not contain the Helius RPC query key"
        );
    }

    // (7) generic scan auto_buy remains a no-op: no submit/trader.buy path was
    //     introduced, and the guard comment is present in this source file.
    #[test]
    fn test_e8_scan_auto_buy_remains_no_op() {
        let src = include_str!("commands.rs");
        assert!(
            src.contains(
                "If scan auto-buy becomes executable, it MUST acquire RuntimeLease before any state/wallet mutation."
            ),
            "scan auto-buy guard comment must be present"
        );
        // The generic `scan` fn ignores buy_amount (bound as `_buy_amount`) — no
        // executable buy path exists.
        assert!(
            src.contains("_buy_amount: f64,"),
            "generic scan must keep buy amount unused (no submit path)"
        );
    }
}
