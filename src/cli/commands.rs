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
use crate::trading::pumpportal_api::PumpPortalTrader;
use crate::trading::{
    reconcile_pending_execution, PendingBuyContext, PendingExecution, PendingExecutionStore,
    PendingSellContext, PendingSellIntent, ReconciliationOutcome, ReconciliationSide,
    TradeReconciler,
};
use crate::wallet::{ExecutionWalletRegistry, WalletOwnershipProbe};

/// Query actual token balance for a wallet and mint
/// Returns the token balance or 0 if not found
fn query_token_balance(
    rpc_client: &solana_client::rpc_client::RpcClient,
    wallet: &Pubkey,
    mint: &str,
) -> u64 {
    use solana_client::rpc_request::TokenAccountsFilter;

    let mint_pubkey = match Pubkey::from_str(mint) {
        Ok(pk) => pk,
        Err(_) => return 0,
    };

    // Try SPL Token program with Mint filter (works for both SPL and Token2022)
    if let Ok(accounts) =
        rpc_client.get_token_accounts_by_owner(wallet, TokenAccountsFilter::Mint(mint_pubkey))
    {
        for account in &accounts {
            if let solana_account_decoder::UiAccountData::Json(parsed) = &account.account.data {
                if let Some(info) = parsed.parsed.get("info") {
                    if let Some(token_amount) = info.get("tokenAmount") {
                        if let Some(amount_str) = token_amount.get("amount") {
                            if let Some(amount) = amount_str.as_str() {
                                let bal = amount.parse::<u64>().unwrap_or(0);
                                if bal > 0 {
                                    return bal;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: Try Token2022 program explicitly (pump.fun tokens use this)
    let token2022_program =
        Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();
    if let Ok(accounts) = rpc_client
        .get_token_accounts_by_owner(wallet, TokenAccountsFilter::ProgramId(token2022_program))
    {
        for account in &accounts {
            if let solana_account_decoder::UiAccountData::Json(parsed) = &account.account.data {
                if let Some(info) = parsed.parsed.get("info") {
                    if let Some(account_mint) = info.get("mint") {
                        if account_mint.as_str() == Some(mint) {
                            if let Some(token_amount) = info.get("tokenAmount") {
                                if let Some(amount_str) = token_amount.get("amount") {
                                    if let Some(amount) = amount_str.as_str() {
                                        let bal = amount.parse::<u64>().unwrap_or(0);
                                        if bal > 0 {
                                            return bal;
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

    0
}

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

/// Start the sniper bot
pub async fn start(config: &Config, dry_run: bool) -> Result<()> {
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

    // Connect to token detection source
    if config.pumpportal.enabled {
        info!("Connecting to PumpPortal WebSocket for token detection...");
        let pumpportal_config = crate::stream::pumpportal::PumpPortalConfig {
            ws_url: config.pumpportal.ws_url.clone(),
            reconnect_delay_ms: config.pumpportal.reconnect_delay_ms,
            max_reconnect_attempts: config.pumpportal.max_reconnect_attempts,
            ping_interval_secs: config.pumpportal.ping_interval_secs,
        };
        let pumpportal_client = PumpPortalClient::new(pumpportal_config, event_tx.clone());

        // Get tracked wallets from config
        let track_wallets = config.wallet_tracking.wallets.clone();

        // Start PumpPortal connection with trade monitoring
        // subscribe_new_tokens: true, subscribe_all_trades: true
        if let Err(e) = pumpportal_client.start(true, true, track_wallets).await {
            error!("PumpPortal connection error: {}", e);
        }
    } else {
        info!("Connecting to ShredStream for token detection...");
        // TODO: Connect to ShredStream when available
        warn!("ShredStream not yet implemented - enable PumpPortal in config");
    }

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

    let recovery_registry = ExecutionWalletRegistry::new(
        keypair.pubkey(),
        &recovery_local_wallets,
        recovery_lightning_wallet,
    );
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
        &recovery_registry,
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
        .filter(|p| legacy_recovery_required(p, &recovery_registry))
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
            if position_is_canonical_for_restore(&position, &recovery_registry) {
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

    // === IMPROVED POSITION MONITOR WITH LOCAL FALLBACK ===
    // Features: Trailing stop, no-movement exit, quick profit, retry with local fallback
    if config.auto_sell.enabled && !dry_run {
        let monitor_config = config.clone();
        let monitor_positions = position_manager.clone();
        let monitor_trader = trader_arc.clone();
        let monitor_keypair = keypair.clone();
        let monitor_rpc = rpc_client.clone();
        let monitor_use_local_api = use_local_api;
        // Transaction-truth wiring (§51): clone the already-initialized reconciler,
        // pending journal, halt flag and strategy engine into the monitor. No new
        // RPC/reconciler is constructed inside the loop.
        let monitor_reconciler = trade_reconciler.clone();
        let monitor_pending = pending_executions.clone();
        let monitor_entry_halt = new_entries_halted.clone();
        let monitor_engine = strategy_engine.clone();
        // Exact Lightning execution wallet, read-only. Local mode does not use this.
        let monitor_lightning_wallet: Option<Pubkey> = primary_execution_wallet;

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

                    // 1. Check stop loss FIRST (cut losses quickly)
                    if pnl_pct <= -sl_pct {
                        should_sell = true;
                        reason = format!("STOP LOSS at {:.1}% (limit: -{:.0}%)", pnl_pct, sl_pct);
                    }

                    // 2. Check trailing stop (only if in profit and dropped from peak)
                    if !should_sell && pnl_pct > 0.0 && drop_from_peak_pct >= trailing_stop_pct {
                        should_sell = true;
                        reason = format!(
                            "TRAILING STOP: dropped {:.1}% from peak (P&L: +{:.1}%)",
                            drop_from_peak_pct, pnl_pct
                        );
                    }

                    // 3. Check take profit
                    if !should_sell && pnl_pct >= tp_pct {
                        should_sell = true;
                        reason = format!("TAKE PROFIT at {:.1}% (target: {:.0}%)", pnl_pct, tp_pct);
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
                            }
                        }
                    }

                    // Execute sell if triggered
                    if should_sell {
                        // `current_price` is the DexScreener trigger/reference price ONLY.
                        // It is NOT the execution price and is never used to estimate proceeds.
                        warn!(
                            "AUTO-SELL TRIGGERED: {} ({}) - {} (trigger price {:.12} SOL/token)",
                            position.symbol, position.mint, reason, current_price
                        );

                        if let Some(ref trader) = monitor_trader {
                            let slippage = monitor_config.trading.slippage_bps / 100;
                            let priority_fee =
                                monitor_config.trading.priority_fee_lamports as f64 / 1e9;

                            // §53 PENDING GUARD: never submit a second sell for a mint that
                            // already has an unresolved submitted sell in the journal.
                            if let Some(pending) = monitor_pending
                                .get_for_mint(&position.mint, ReconciliationSide::Sell)
                                .await
                            {
                                error!(
                                    "Sell remains pending for {}: signature {}. Not submitting another sell (001C reconciliation required).",
                                    position.mint, pending.signature
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

                            // §55 EXACT WALLET GUARD: the position's recorded wallet must parse
                            // and must match the wallet controlled by THIS execution route.
                            let position_wallet = match Pubkey::from_str(position.wallet_pubkey.trim()) {
                                Ok(pk) => pk,
                                Err(e) => {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "Position wallet_pubkey empty/invalid for {} ({:?}): {} - no sell, new entries HALTED",
                                        position.mint, position.wallet_pubkey, e
                                    );
                                    continue;
                                }
                            };
                            if monitor_use_local_api {
                                if position_wallet != monitor_keypair.pubkey() {
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "Local sell wallet mismatch for {}: position {} != local {} - no sell, new entries HALTED",
                                        position.mint, position_wallet, monitor_keypair.pubkey()
                                    );
                                    continue;
                                }
                            } else {
                                match monitor_lightning_wallet {
                                    Some(lw) if lw == position_wallet => {}
                                    Some(lw) => {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        error!(
                                            "Lightning sell wallet mismatch for {}: position {} != lightning {} - no sell, new entries HALTED",
                                            position.mint, position_wallet, lw
                                        );
                                        continue;
                                    }
                                    None => {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        error!(
                                            "Lightning execution wallet unresolved; cannot validate sell route for {} - no sell, new entries HALTED",
                                            position.mint
                                        );
                                        continue;
                                    }
                                }
                            }

                            // Retry counter behavior preserved.
                            let attempts = sell_attempts.entry(position.mint.clone()).or_insert(0);
                            *attempts += 1;

                            if *attempts > 5 {
                                // Retry exhaustion: never close/remove wallet-owned risk. Leave
                                // OPEN/TRACKED, reset counter, let a later cycle try again.
                                error!(
                                    "AUTO-SELL UNRESOLVED for {} after 5 attempts - position remains OPEN/TRACKED",
                                    position.symbol
                                );
                                sell_attempts.remove(&position.mint);
                                continue;
                            }

                            // §56 ROUTE: no Lightning->Local fallback in the primary monitor.
                            // Local mode always sell_local with the local keypair; Lightning mode
                            // always sell() via Lightning for every attempt.
                            let sell_start = std::time::Instant::now();
                            let sell_result: Result<String, crate::error::Error> = if monitor_use_local_api {
                                info!("Attempting LOCAL API sell for {} (attempt {})", position.mint, attempts);
                                trader
                                    .sell_local(
                                        &position.mint,
                                        sell_pct,
                                        slippage,
                                        priority_fee,
                                        &monitor_keypair,
                                        &monitor_rpc,
                                    )
                                    .await
                            } else {
                                info!("Attempting Lightning API sell for {} (attempt {})", position.mint, attempts);
                                trader
                                    .sell(&position.mint, sell_pct, slippage, priority_fee)
                                    .await
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
                                    continue;
                                }
                            };

                            // §58 PERSIST SIGNATURE: submitted != executed.
                            info!("AUTO-SELL SUBMITTED: {} (sig {})", position.symbol, signature);
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
                                    requested_amount: sell_pct.to_string(),
                                    intent,
                                    reason: reason.clone(),
                                },
                            );
                            if let Err(e) = monitor_pending.upsert(pending_sell).await {
                                // Signature already exists on chain-side; persistence failed.
                                // Halt new entries but STILL reconcile the submitted signature.
                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                error!(
                                    "Failed to persist pending sell (sig {}): {} - new entries HALTED, still reconciling",
                                    signature, e
                                );
                            }

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
                                    continue;
                                }
                                Ok(ReconciliationOutcome::Unresolved { reason: unresolved_reason, .. }) => {
                                    // §61: KEEP pending, keep position + flags, halt new entries.
                                    // Do NOT clear sell-attempt state (pending guard prevents a
                                    // second submission on the next cycle).
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "AUTO-SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept, position kept, new entries HALTED",
                                        position.mint, signature, position.wallet_pubkey, unresolved_reason
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    // §61: structural observer failure is not tx-failure proof.
                                    monitor_entry_halt.store(true, Ordering::SeqCst);
                                    error!(
                                        "CRITICAL: sell reconciliation error for {} (sig {}): {} - pending kept, position kept, new entries HALTED",
                                        position.symbol, signature, e
                                    );
                                    continue;
                                }
                                Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                    // §62 identity validation at the live boundary.
                                    if fill.side != ReconciliationSide::Sell
                                        || fill.wallet != position.wallet_pubkey
                                        || fill.mint != position.mint
                                    {
                                        monitor_entry_halt.store(true, Ordering::SeqCst);
                                        error!(
                                            "CRITICAL: reconciled sell fill identity mismatch for sig {} (wallet/mint/side) - pending kept, position kept, new entries HALTED",
                                            signature
                                        );
                                        continue;
                                    }

                                    // §62/§63 economics via pure helper (validates decimals match,
                                    // nonzero raw, finite delta/price, and no oversell).
                                    let (actual_sold_raw, actual_received_sol, actual_exit_price) =
                                        match primary_sell_fill_values(&fill, &position) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                monitor_entry_halt.store(true, Ordering::SeqCst);
                                                error!(
                                                    "Reconciled sell fill validation failed for {} (sig {}): {} - pending kept, position kept, new entries HALTED",
                                                    position.mint, signature, e
                                                );
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
                                            error!(
                                                "Reconciled close failed for {} (sig {}): {} - pending kept, new entries HALTED",
                                                position.mint, signature, e
                                            );
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
                                }
                            }
                        }
                    }
                }
            }
        });
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

                        // Fail-closed new-entry halt gate. Independent of daily loss,
                        // strategy pause, and filters. Blocks NEW live buys when
                        // unresolved transaction state requires reconciliation.
                        if !dry_run && new_entries_halted.load(Ordering::SeqCst) {
                            warn!(
                                "New entries halted because unresolved transaction state requires reconciliation"
                            );
                            continue;
                        }

                        // Apply filters
                        if config.filters.enabled {
                            use crate::filter::token_filter::FilterResult;
                            use crate::stream::decoder::TokenCreatedEvent;
                            use std::str::FromStr;

                            // Convert NewTokenEvent to TokenCreatedEvent for filtering
                            let filter_event = TokenCreatedEvent {
                                signature: token.signature.clone(),
                                slot: 0, // Not available from PumpPortal
                                mint: solana_sdk::pubkey::Pubkey::from_str(&token.mint).unwrap_or_default(),
                                name: token.name.clone(),
                                symbol: token.symbol.clone(),
                                uri: token.uri.clone(),
                                bonding_curve: solana_sdk::pubkey::Pubkey::from_str(&token.bonding_curve_key).unwrap_or_default(),
                                associated_bonding_curve: solana_sdk::pubkey::Pubkey::default(),
                                creator: solana_sdk::pubkey::Pubkey::from_str(&token.trader_public_key).unwrap_or_default(),
                                timestamp: chrono::Utc::now(),
                            };

                            match token_filter.filter(&filter_event) {
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
                            // Calculate bonding curve % from virtual reserves
                            // v_sol_in_bonding_curve is in lamports (u64), convert to SOL
                            // Initial: ~30 SOL virtual, At graduation: ~85 SOL in curve
                            let v_sol = token.v_sol_in_bonding_curve as f64 / 1_000_000_000.0;
                            let bonding_curve_pct = if v_sol > 0.0 {
                                // Approximate: more SOL = more progress
                                // Full curve is ~85 SOL (starting from ~30 virtual)
                                ((v_sol - 30.0) / 55.0 * 100.0).clamp(0.0, 100.0)
                            } else {
                                0.0
                            };

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

                            // Build token analysis context for strategy engine
                            // Note: PumpPortal sends v_sol_in_bonding_curve as SOL, not lamports
                            // The value is typically ~30 SOL (virtual liquidity)
                            // For actual tradeable liquidity, we use initial_buy or market_cap
                            let liquidity_sol = if token.v_sol_in_bonding_curve < 1000 {
                                // Small value = already in SOL
                                token.v_sol_in_bonding_curve as f64
                            } else {
                                // Large value = lamports, convert to SOL
                                token.v_sol_in_bonding_curve as f64 / 1e9
                            };
                            let token_reserves = token.v_tokens_in_bonding_curve as f64;

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

                                // Use buy_local for Local API, buy for Lightning API
                                let buy_start = std::time::Instant::now();
                                let buy_result = if use_local_api {
                                    trader.buy_local(mint, final_amount_sol, slippage_pct, priority_fee, &keypair, &rpc_client).await
                                } else {
                                    trader.buy(mint, final_amount_sol, slippage_pct, priority_fee).await
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
                                        let pending = PendingExecution::buy(
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
                                        if let Err(e) = pending_executions.upsert(pending).await {
                                            // Serious state-integrity failure: the tx was already
                                            // sent. Halt new entries, still attempt immediate
                                            // reconciliation, never send another buy.
                                            new_entries_halted.store(true, Ordering::SeqCst);
                                            error!(
                                                "Failed to persist pending buy for {} (sig {}): {} - halting new entries; still reconciling",
                                                token.symbol, signature, e
                                            );
                                        }

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
                                                error!(
                                                    "BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept, new entries HALTED",
                                                    mint, signature, wallet_string, reason
                                                );
                                                continue;
                                            }
                                            Err(e) => {
                                                // Structural observer failure is not tx-failure proof.
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "CRITICAL: buy reconciliation error for {} (sig {}): {} - pending kept, new entries HALTED",
                                                    token.symbol, signature, e
                                                );
                                                continue;
                                            }
                                            Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                                // Validate exact identity at the live boundary.
                                                if fill.side != ReconciliationSide::Buy
                                                    || fill.wallet != wallet_string
                                                    || fill.mint != *mint
                                                {
                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                    error!(
                                                        "CRITICAL: reconciled buy fill identity mismatch for sig {} (wallet/mint/side) - pending kept, new entries HALTED",
                                                        signature
                                                    );
                                                    continue;
                                                }

                                                // Extract canonical fill economics via pure helper.
                                                let (token_amount_raw, _token_decimals, actual_cost_sol, actual_entry_price) =
                                                    match primary_buy_fill_values(&fill) {
                                                        Ok(v) => v,
                                                        Err(e) => {
                                                            new_entries_halted.store(true, Ordering::SeqCst);
                                                            error!(
                                                                "CRITICAL: reconciled buy fill conversion failed for sig {}: {} - pending kept, new entries HALTED",
                                                                signature, e
                                                            );
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
                                                        error!(
                                                            "Confirmed owned position could not be recorded for {} (sig {}): {} - pending kept, new entries HALTED",
                                                            token.symbol, signature, e
                                                        );
                                                        continue;
                                                    }
                                                };

                                                if newly_applied {
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

                                                        // Reconciled-success execution feedback:
                                                        // actual price, fill rate, latency, NO slippage.
                                                        let total_execution_latency_ms =
                                                            buy_start.elapsed().as_millis() as u64;
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
                                    let decision = evaluator.evaluate_sell(
                                        &trade.mint,
                                        &trade.trader_public_key,
                                        trade.token_amount as u64,
                                        sol_amount_sol,
                                        &trade.signature,
                                    );

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
                                            // §E2 PENDING GUARD: if a Sell for this mint is already
                                            // in flight, do NOT submit a second emergency sell. Keep
                                            // the kill-switch alert active and log the pending sig.
                                            if let Some(existing) = pending_executions
                                                .get_for_mint(&trade.mint, ReconciliationSide::Sell)
                                                .await
                                            {
                                                warn!(
                                                    "KILL-SWITCH: pending sell already in flight for {} (sig {}) - not submitting a second emergency sell; alert remains active",
                                                    &trade.mint[..12], existing.signature
                                                );
                                            } else if let Some(ref trader) = trader_arc {
                                                // §E3 EXACT ROUTE: resolve the exact execution route
                                                // and signer for the position's recorded wallet.
                                                // Empty/invalid wallet, unknown route, or missing
                                                // Local signer => no sell + halt new entries. No
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

                                                let slippage_pct = config.trading.slippage_bps / 100;
                                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                                let sell_start = std::time::Instant::now();
                                                let routed_sell: Option<Result<String, crate::error::Error>> =
                                                    match recovery_registry.route_for(&position_wallet) {
                                                        Some(crate::wallet::ExecutionRoute::Local) => {
                                                            // Exact local signer: primary keypair if its
                                                            // pubkey matches, else recovery multi-wallet.
                                                            if keypair.pubkey() == position_wallet {
                                                                info!(
                                                                    "KILL-SWITCH: Local sell for {} via primary keypair",
                                                                    &trade.mint[..12]
                                                                );
                                                                Some(trader.sell_local(
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
                                                                        Some(trader.sell_local(
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
                                                        Some(crate::wallet::ExecutionRoute::Lightning) => {
                                                            // Require a Lightning trader / API key. No
                                                            // Local fallback.
                                                            if config.pumpportal.api_key.is_empty() || use_local_api {
                                                                None
                                                            } else {
                                                                info!(
                                                                    "KILL-SWITCH: Lightning sell for {}",
                                                                    &trade.mint[..12]
                                                                );
                                                                Some(trader.sell(&trade.mint, "100%", slippage_pct, priority_fee).await)
                                                            }
                                                        }
                                                        None => None,
                                                    };

                                                let sell_result = match routed_sell {
                                                    Some(r) => r,
                                                    None => {
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "KILL-SWITCH: no exact signer/route for position {} wallet {} - no sell, new entries HALTED",
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
                                                if let Err(e) = pending_executions.upsert(pending_sell).await {
                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                    error!(
                                                        "KILL-SWITCH: failed to persist pending sell (sig {}): {} - new entries HALTED, still reconciling",
                                                        signature, e
                                                    );
                                                }

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
                                                        continue;
                                                    }
                                                    Ok(ReconciliationOutcome::Unresolved { reason: unresolved_reason, .. }) => {
                                                        // Keep pending, halt new entries, keep position.
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "KILL-SWITCH SELL UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept, position kept, new entries HALTED",
                                                            position.mint, signature, position.wallet_pubkey, unresolved_reason
                                                        );
                                                        continue;
                                                    }
                                                    Err(e) => {
                                                        // Structural observer failure is not tx-failure proof.
                                                        new_entries_halted.store(true, Ordering::SeqCst);
                                                        error!(
                                                            "CRITICAL: kill-switch sell reconciliation error for {} (sig {}): {} - pending kept, position kept, new entries HALTED",
                                                            &trade.mint[..12], signature, e
                                                        );
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
                                                            error!(
                                                                "CRITICAL: kill-switch fill identity mismatch for sig {} (wallet/mint/side) - pending kept, position kept, new entries HALTED",
                                                                signature
                                                            );
                                                            continue;
                                                        }

                                                        // Exact economics + decimals validation (also
                                                        // rejects oversell / decimals mismatch).
                                                        let (actual_sold_raw, actual_received_sol, actual_exit_price) =
                                                            match primary_sell_fill_values(&fill, &position) {
                                                                Ok(v) => v,
                                                                Err(e) => {
                                                                    new_entries_halted.store(true, Ordering::SeqCst);
                                                                    error!(
                                                                        "KILL-SWITCH fill validation failed for {} (sig {}): {} - pending kept, position kept, new entries HALTED",
                                                                        position.mint, signature, e
                                                                    );
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
                                                                error!(
                                                                    "KILL-SWITCH reconciled close failed for {} (sig {}): {} - pending kept, new entries HALTED",
                                                                    position.mint, signature, e
                                                                );
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
                    PumpPortalEvent::Connected => {
                        info!("Connected to token detection source");
                    }
                    PumpPortalEvent::Disconnected => {
                        warn!("Disconnected from token detection source");
                    }
                    PumpPortalEvent::Error(e) => {
                        error!("Token detection error: {}", e);
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

/// Manually sell a token position
pub async fn sell(
    config: &Config,
    token: &str,
    amount: &str,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    info!("Sell command: token={}, amount={}", token, amount);

    // Parse token address
    let _token_pubkey = solana_sdk::pubkey::Pubkey::try_from(token)
        .map_err(|e| anyhow::anyhow!("Invalid token address: {}", e))?;

    // Parse amount (can be percentage like "50%" or absolute)
    let is_percentage = amount.ends_with('%');
    let amount_value: f64 = if is_percentage {
        amount
            .trim_end_matches('%')
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid amount: {}", e))?
    } else {
        amount
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid amount: {}", e))?
    };

    if is_percentage && (amount_value <= 0.0 || amount_value > 100.0) {
        anyhow::bail!("Percentage must be between 0 and 100");
    }

    // Initialize RPC client for balance queries
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    // Determine which wallet to query for balance (Lightning or local)
    let balance_wallet = if !config.pumpportal.lightning_wallet.is_empty() {
        Pubkey::from_str(&config.pumpportal.lightning_wallet)?
    } else {
        // Fall back to local keypair
        let keypair_path = std::env::var("KEYPAIR_PATH")
            .unwrap_or_else(|_| "credentials/hot-trading/keypair.json".to_string());
        let keypair_data = std::fs::read_to_string(&keypair_path)?;
        let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
        let keypair = Keypair::from_bytes(&secret_key)?;
        keypair.pubkey()
    };

    // Initialize position manager
    let position_manager = std::sync::Arc::new(crate::position::manager::PositionManager::new(
        config.safety.clone(),
        Some(format!("{}/positions.json", config.wallet.credentials_dir)),
    ));
    if let Err(e) = position_manager.load().await {
        warn!("Could not load positions: {} (continuing anyway)", e);
    }

    // Load bought_mints cache
    let bought_mints_path = format!("{}/bought_mints.json", config.wallet.credentials_dir);
    let bought_mints: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>> = {
        if std::path::Path::new(&bought_mints_path).exists() {
            match std::fs::read_to_string(&bought_mints_path) {
                Ok(data) => {
                    if let Ok(mints) = serde_json::from_str::<std::collections::HashMap<String, i64>>(&data) {
                        std::sync::Arc::new(tokio::sync::Mutex::new(mints))
                    } else {
                        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
                    }
                }
                Err(_) => std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            }
        } else {
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
        }
    };
    let bought_mints_path = std::sync::Arc::new(bought_mints_path);

    // Get position info if we have it
    let position = position_manager.get_position(token).await;
    if let Some(ref pos) = position {
        println!("\nPosition found:");
        println!("  Symbol: {}", pos.symbol);
        println!("  Tokens: {}", pos.token_amount);
        println!("  Entry price: {:.10} SOL", pos.entry_price);
        println!("  Cost: {:.4} SOL", pos.total_cost_sol);
    }

    // Confirmation prompt (unless --force)
    if config.safety.require_sell_confirmation && !force {
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Sell {} of token {}? This cannot be undone.",
                amount, token
            ))
            .default(false)
            .interact()?;

        if !confirmed {
            info!("Sell cancelled by user");
            return Ok(());
        }
    }

    if dry_run {
        info!("DRY-RUN: Would sell {} of {}", amount, token);
        return Ok(());
    }

    // Execute sell based on configuration
    if config.pumpportal.use_for_trading {
        // Use PumpPortal API
        if config.pumpportal.api_key.is_empty() {
            anyhow::bail!("PumpPortal API key required for selling via Lightning API");
        }

        let trader = PumpPortalTrader::lightning(config.pumpportal.api_key.clone());
        let slippage_pct = config.trading.slippage_bps / 100;
        let priority_fee = config.trading.priority_fee_lamports as f64 / 1_000_000_000.0;

        // Query SOL balance BEFORE sell for real P&L
        let sol_before = rpc_client.get_balance(&balance_wallet).unwrap_or(0) as f64 / 1_000_000_000.0;
        info!("Balance before sell: {:.4} SOL", sol_before);

        info!("Submitting sell via PumpPortal API...");
        match trader.sell(token, amount, slippage_pct, priority_fee).await {
            Ok(signature) => {
                info!("Sell successful! Signature: {}", signature);
                println!("\nSell transaction confirmed!");
                println!("Signature: {}", signature);
                println!("View on Solscan: https://solscan.io/tx/{}", signature);

                // Wait for tx confirmation then query actual SOL received
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                let sol_after = rpc_client.get_balance(&balance_wallet).unwrap_or(0) as f64 / 1_000_000_000.0;
                let raw_received = (sol_after - sol_before).max(0.0);

                println!("Balance after sell: {:.4} SOL", sol_after);
                println!("SOL received (raw): {:.4} SOL", raw_received);

                // Update position manager and stats
                if let Some(ref pos) = position {
                    let is_full_sell = amount == "100%" || amount_value >= 100.0;
                    let tokens_sold = if is_full_sell {
                        pos.token_amount
                    } else if is_percentage {
                        (pos.token_amount as f64 * amount_value / 100.0) as u64
                    } else {
                        amount_value as u64
                    };

                    // Sanity check: received SOL shouldn't be more than 10x position cost
                    // If it is, the balance query likely failed (sol_before was 0)
                    let max_reasonable = pos.total_cost_sol * 10.0;
                    let actual_received = if raw_received > max_reasonable {
                        warn!(
                            "Balance query anomaly: before={:.4}, after={:.4}, diff={:.4} (max reasonable: {:.4}) - using estimate",
                            sol_before, sol_after, raw_received, max_reasonable
                        );
                        0.0 // Force fallback to estimate
                    } else {
                        raw_received
                    };

                    // Use actual received SOL, fallback to estimate if balance query failed
                    let received = if actual_received > 0.0 {
                        actual_received
                    } else {
                        // Estimate based on position price (use current_price if available, else entry_price)
                        let price = if pos.current_price > 0.0 { pos.current_price } else { pos.entry_price };
                        let estimated = (tokens_sold as f64 * price) * 0.98;
                        warn!("Balance query returned 0 or anomaly detected, using estimated received: {:.4} SOL", estimated);
                        estimated
                    };

                    let _ = position_manager
                        .close_position(token, tokens_sold, received)
                        .await;

                    // Persist position state immediately
                    if let Err(e) = position_manager.save().await {
                        warn!("Failed to persist position state: {}", e);
                    }

                    let cost_portion = if is_full_sell {
                        pos.total_cost_sol
                    } else {
                        pos.total_cost_sol * amount_value / 100.0
                    };
                    let pnl_sol = received - cost_portion;
                    let pnl_pct = (pnl_sol / cost_portion) * 100.0;

                    println!("\n=== TRADE CLOSED ===");
                    println!("  Cost: {:.4} SOL | Received: {:.4} SOL | P&L: {:+.4} SOL ({:+.1}%)",
                            cost_portion, received, pnl_sol, pnl_pct);

                    // Clean up bought_mints if position is fully closed
                    // Check if position still exists after close_position
                    let position_closed = position_manager.get_position(token).await.is_none();
                    if position_closed {
                        let _ = remove_bought_mint(&bought_mints, &bought_mints_path, token).await;
                        info!("Removed {} from bought_mints cache", token);
                    }
                } else {
                    // No position tracked - still clean up bought_mints
                    let removed = remove_bought_mint(&bought_mints, &bought_mints_path, token).await;
                    if removed {
                        info!("Removed {} from bought_mints cache", token);
                    }
                }
            }
            Err(e) => {
                error!("Sell failed: {}", e);
                anyhow::bail!("Sell transaction failed: {}", e);
            }
        }
    } else {
        // Use Jito bundles
        warn!("Jito sell not yet implemented - use PumpPortal Lightning API");
        anyhow::bail!("Jito sell not implemented. Set pumpportal.use_for_trading = true in config.toml");
    }

    Ok(())
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
        print!("PumpPortal WebSocket... ");
        match check_pumpportal(config).await {
            Ok(_) => println!("OK"),
            Err(e) => {
                println!("FAILED: {}", e);
                all_healthy = false;
            }
        }
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
    if config.pumpportal.use_for_trading {
        print!("PumpPortal Trading API... ");
        if config.pumpportal.api_key.is_empty() {
            println!("LOCAL MODE (no API key)");
        } else {
            println!("LIGHTNING MODE (API key configured)");
        }
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

async fn check_pumpportal(config: &Config) -> Result<()> {
    use std::time::Duration;
    use tokio_tungstenite::connect_async;

    let url = url::Url::parse(&config.pumpportal.ws_url)
        .map_err(|e| anyhow::anyhow!("Invalid WebSocket URL: {}", e))?;

    // Try to connect with timeout
    let connect_future = connect_async(url);
    let timeout = Duration::from_secs(5);

    match tokio::time::timeout(timeout, connect_future).await {
        Ok(Ok((ws, _))) => {
            // Successfully connected, close by dropping
            drop(ws);
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("WebSocket connection failed: {}", e)),
        Err(_) => Err(anyhow::anyhow!(
            "Connection timed out after {}s",
            timeout.as_secs()
        )),
    }
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

    let hotscan_registry = ExecutionWalletRegistry::new(
        keypair.pubkey(),
        &hotscan_local_wallets,
        hotscan_lightning_wallet,
    );
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
        &hotscan_registry,
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
        .filter(|p| legacy_recovery_required(p, &hotscan_registry))
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
        let monitor_trader = trader.clone();
        let monitor_keypair = keypair.clone();
        let monitor_rpc = rpc_client.clone();
        let monitor_dex = DexScreenerClient::new();
        let monitor_bought_mints = bought_mints.clone();
        let monitor_bought_mints_path = bought_mints_path.clone();
        let monitor_sold_mints = sold_mints.clone();
        let monitor_failed_mints = failed_mints.clone();
        let monitor_kill_switch = kill_switch_evaluator.clone();
        let monitor_helius = helius_client.clone();
        let monitor_use_local_api = use_local_api;
        let monitor_multi_wallet = multi_wallet.clone();
        // Determine which wallet to query for token balances
        let monitor_wallet = if use_local_api {
            keypair.pubkey()
        } else if !config.pumpportal.lightning_wallet.is_empty() {
            Pubkey::from_str(&config.pumpportal.lightning_wallet)
                .unwrap_or_else(|_| keypair.pubkey())
        } else {
            keypair.pubkey()
        };

        tokio::spawn(async move {
            info!("=== POSITION MONITOR STARTED ===");
            let poll_interval_ms = monitor_config.auto_sell.price_poll_interval_ms;
            info!("Features: Dynamic Trailing ({}%-{}%), Layered Exits ({}%/{}%/{}%), Kill-Switch, LOCAL FALLBACK",
                monitor_config.auto_sell.trailing_stop_base_pct,
                monitor_config.auto_sell.trailing_stop_tight_pct,
                monitor_config.auto_sell.quick_profit_pct,
                monitor_config.auto_sell.second_profit_pct,
                monitor_config.auto_sell.take_profit_pct
            );
            info!("Poll interval: {}ms", poll_interval_ms);
            if !monitor_use_local_api {
                info!(
                    "Using Lightning wallet for balance queries: {}",
                    monitor_wallet
                );
            }

            let mut sell_attempts: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();
            // Track confirmed positions (tx landed and ATA exists)
            let mut confirmed_positions: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            loop {
                tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;

                let positions = monitor_positions.get_all_positions().await;
                if positions.is_empty() {
                    continue;
                }

                // Fetch current prices from DexScreener with fallback handling
                for position in positions {
                    // Get current price from DexScreener with retry
                    let price_result = monitor_dex.get_token_info(&position.mint).await;

                    let current_price = match price_result {
                        Ok(Some(token_info)) => {
                            if token_info.price_native > 0.0 {
                                token_info.price_native
                            } else {
                                // Zero price from API - use last known price if available
                                if position.current_price > 0.0 {
                                    warn!("[{}] DexScreener returned 0 price, using last known: {:.10}",
                                          position.symbol, position.current_price);
                                    position.current_price
                                } else {
                                    continue;
                                }
                            }
                        }
                        Ok(None) => {
                            // Token not found on DexScreener - use last known price
                            if position.current_price > 0.0 {
                                warn!(
                                    "[{}] Not found on DexScreener, using last known price: {:.10}",
                                    position.symbol, position.current_price
                                );
                                position.current_price
                            } else {
                                warn!(
                                    "[{}] Not found on DexScreener and no last price - skipping",
                                    position.symbol
                                );
                                continue;
                            }
                        }
                        Err(e) => {
                            // API error - use last known price as fallback
                            if position.current_price > 0.0 {
                                warn!(
                                    "[{}] DexScreener error: {} - using last known price: {:.10}",
                                    position.symbol, e, position.current_price
                                );
                                position.current_price
                            } else {
                                error!(
                                    "[{}] DexScreener error and no fallback price: {}",
                                    position.symbol, e
                                );
                                continue;
                            }
                        }
                    };

                    // Update position price
                    monitor_positions
                        .update_price(&position.mint, current_price)
                        .await;

                    // Small delay between API calls to avoid rate limiting
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    // Get updated position with peak_price tracked
                    let position = match monitor_positions.get_position(&position.mint).await {
                        Some(p) => p,
                        None => continue,
                    };

                    // TX CONFIRMATION CHECK: Verify buy tx confirmed before allowing sells
                    if !confirmed_positions.contains(&position.mint) {
                        let position_age_secs = (chrono::Utc::now() - position.entry_time)
                            .num_seconds()
                            .max(0) as u64;

                        // First 5 seconds: just wait
                        if position_age_secs < 5 {
                            continue;
                        }

                        // After 5 seconds: check if we have tokens
                        // Use position's wallet_pubkey if available (multi-wallet), fallback to monitor_wallet
                        let check_wallet = if !position.wallet_pubkey.is_empty() {
                            Pubkey::from_str(&position.wallet_pubkey).unwrap_or(monitor_wallet)
                        } else {
                            monitor_wallet
                        };
                        let token_balance =
                            query_token_balance(&monitor_rpc, &check_wallet, &position.mint);

                        if token_balance > 0 {
                            info!(
                                "[{}] TX CONFIRMED - token balance: {}",
                                position.symbol, token_balance
                            );
                            confirmed_positions.insert(position.mint.clone());
                        } else if position_age_secs > 30 {
                            // After 30 seconds with no tokens, assume tx failed
                            warn!(
                                "[{}] TX LIKELY FAILED - no tokens after 30s, removing position (30min cooldown)",
                                position.symbol
                            );
                            let _ = monitor_positions.abandon_position(&position.mint).await;
                            let _ = remove_bought_mint(
                                &monitor_bought_mints,
                                &monitor_bought_mints_path,
                                &position.mint,
                            )
                            .await;
                            // Add to failed_mints with 30 minute cooldown to prevent repeated failures
                            {
                                let mut failed = monitor_failed_mints.lock().await;
                                failed.insert(position.mint.clone(), chrono::Utc::now().timestamp());
                                info!("[{}] Added to failed_mints blacklist (30min cooldown)", position.symbol);
                            }
                            continue;
                        } else {
                            // Still waiting for confirmation
                            continue;
                        }
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

                    // === KILL-SWITCH CHECK (HIGHEST PRIORITY) ===
                    // First check position flag (set by other systems)
                    if let Some(ks_reason) = monitor_positions.is_kill_switch_triggered(&position.mint).await {
                        should_sell = true;
                        reason = format!("KILL-SWITCH: {}", ks_reason);
                        warn!("KILL-SWITCH EXIT: {} - {}", position.symbol, ks_reason);
                    }
                    // Then actively evaluate kill-switch conditions
                    if !should_sell {
                        if let Some(ref evaluator) = monitor_kill_switch {
                            if let KillSwitchDecision::Exit(alert) = evaluator.should_exit(&position.mint) {
                                should_sell = true;
                                reason = format!("KILL-SWITCH: {} (urgency: {:?})", alert.reason, alert.urgency);
                                warn!("KILL-SWITCH EXIT: {} - {} [{:?}]", position.symbol, alert.reason, alert.urgency);
                            }
                        }
                    }

                    // 1. Stop loss
                    if !should_sell && pnl_pct <= -sl_pct {
                        should_sell = true;
                        reason = format!("STOP LOSS at {:.1}% (limit: -{:.0}%)", pnl_pct, sl_pct);
                    }

                    // 2. Trailing stop (only if in profit and dropped from peak)
                    // Now uses dynamic trailing stop percentage
                    if !should_sell && pnl_pct > 0.0 && drop_from_peak_pct >= trailing_stop_pct {
                        should_sell = true;
                        reason = format!(
                            "TRAILING STOP: dropped {:.1}% from peak (P&L: +{:.1}%, trail: {:.0}%)",
                            drop_from_peak_pct, pnl_pct, trailing_stop_pct
                        );
                    }

                    // 3. Take profit (final exit)
                    if !should_sell && pnl_pct >= tp_pct {
                        should_sell = true;
                        reason = format!("TAKE PROFIT at {:.1}% (target: {:.0}%)", pnl_pct, tp_pct);
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
                    }

                    // 6. No-movement exit
                    if !should_sell
                        && hold_time_secs >= no_movement_secs
                        && pnl_pct.abs() < no_movement_threshold
                    {
                        should_sell = true;
                        reason = format!("NO MOVEMENT: {:.1}% after {}s", pnl_pct, hold_time_secs);
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
                            }
                        }
                    }

                    // Execute sell
                    if should_sell {
                        warn!(
                            "AUTO-SELL TRIGGERED: {} ({}) - {}",
                            position.symbol, position.mint, reason
                        );

                        if let Some(ref trader) = monitor_trader {
                            let slippage = monitor_config.trading.slippage_bps / 100;
                            let priority_fee =
                                monitor_config.trading.priority_fee_lamports as f64 / 1e9;

                            let attempts = sell_attempts.entry(position.mint.clone()).or_insert(0);
                            *attempts += 1;

                            if *attempts > 5 {
                                // INV-POS-002: a failed sell must never make a wallet-owned
                                // position disappear from tracking. Do NOT abandon the position
                                // or drop it from bought-mints. Leave it OPEN/TRACKED, reset the
                                // retry counter, and let a later cycle retry. Reconciliation is a
                                // later packet.
                                error!(
                                    "AUTO-SELL UNRESOLVED for {} after 5 attempts - position remains OPEN/TRACKED",
                                    position.symbol
                                );
                                sell_attempts.remove(&position.mint);
                                continue;
                            }

                            // Query SOL balance BEFORE sell for real P&L tracking
                            // Use position's wallet if available (multi-wallet), fallback to monitor_wallet
                            let position_wallet = if !position.wallet_pubkey.is_empty() {
                                Pubkey::from_str(&position.wallet_pubkey).unwrap_or(monitor_wallet)
                            } else {
                                monitor_wallet
                            };
                            let sol_before = monitor_rpc
                                .get_balance(&position_wallet)
                                .unwrap_or(0) as f64
                                / 1_000_000_000.0;

                            // Determine the correct keypair for this position
                            // For multi-wallet, look up keypair by position's wallet_pubkey
                            let sell_keypair: std::sync::Arc<solana_sdk::signature::Keypair> = if !position.wallet_pubkey.is_empty() {
                                if let Some(ref mw) = monitor_multi_wallet {
                                    // Find wallet matching position's pubkey
                                    if let Some(wallet) = mw.find_by_address(&position.wallet_pubkey) {
                                        std::sync::Arc::new(
                                            solana_sdk::signature::Keypair::from_bytes(&wallet.keypair.to_bytes()).unwrap()
                                        )
                                    } else {
                                        warn!("[{}] Position wallet {} not found in multi-wallet, using primary",
                                              position.symbol, &position.wallet_pubkey[..8]);
                                        monitor_keypair.clone()
                                    }
                                } else {
                                    monitor_keypair.clone()
                                }
                            } else {
                                monitor_keypair.clone()
                            };

                            // For Local API mode, use local signing directly
                            // For Lightning mode, try Lightning first then fall back to local
                            let sell_result: std::result::Result<String, crate::error::Error> =
                                if monitor_use_local_api {
                                    // Local API mode: use local signing with correct wallet
                                    info!("Attempting Local API sell (attempt {}, wallet: {})",
                                          attempts, &sell_keypair.pubkey().to_string()[..8]);
                                    trader
                                        .sell_local(
                                            &position.mint,
                                            sell_pct,
                                            slippage,
                                            priority_fee,
                                            &sell_keypair,
                                            &monitor_rpc,
                                        )
                                        .await
                                } else if *attempts <= 3 {
                                    info!("Attempting Lightning API sell (attempt {})", attempts);
                                    trader
                                        .sell(&position.mint, sell_pct, slippage, priority_fee)
                                        .await
                                } else {
                                    warn!("Lightning failed 3x, trying LOCAL SIGNING fallback (attempt {})", attempts);
                                    trader
                                        .sell_local(
                                            &position.mint,
                                            sell_pct,
                                            slippage,
                                            priority_fee,
                                            &sell_keypair,
                                            &monitor_rpc,
                                        )
                                        .await
                                };

                            match sell_result {
                                Ok(sig) => {
                                    info!("AUTO-SELL EXECUTED: {} - {}", position.symbol, sig);
                                    sell_attempts.remove(&position.mint);

                                    // Wait for tx confirmation then query actual SOL received
                                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                                    let sol_after = monitor_rpc
                                        .get_balance(&position_wallet)
                                        .unwrap_or(0) as f64
                                        / 1_000_000_000.0;
                                    let raw_received = (sol_after - sol_before).max(0.0);

                                    // Sanity check: received SOL shouldn't be more than 10x position cost
                                    // If it is, the balance query likely failed - use estimate instead
                                    let max_reasonable = position.total_cost_sol * 10.0;
                                    let actual_received = if raw_received > max_reasonable {
                                        warn!(
                                            "[{}] Balance query anomaly: before={:.4}, after={:.4}, diff={:.4} - using estimate",
                                            position.symbol, sol_before, sol_after, raw_received
                                        );
                                        0.0 // Force fallback to estimate
                                    } else {
                                        raw_received
                                    };

                                    // Calculate trade metrics
                                    let hold_secs =
                                        (chrono::Utc::now() - position.entry_time).num_seconds();
                                    let price_change_pct = ((current_price - position.entry_price)
                                        / position.entry_price)
                                        * 100.0;

                                    if sell_pct == "50%" {
                                        // LAYER 1: Quick profit - sell 50%
                                        let sell_amount = position.token_amount / 2;
                                        // Use actual received SOL (fallback to estimate if 0)
                                        let received = if actual_received > 0.0 {
                                            actual_received
                                        } else {
                                            (sell_amount as f64 * current_price) * 0.98
                                        };
                                        let pnl_sol = received - (position.total_cost_sol / 2.0);
                                        let _ = monitor_positions
                                            .close_position(
                                                &position.mint,
                                                sell_amount,
                                                received,
                                            )
                                            .await;
                                        let _ = monitor_positions
                                            .mark_quick_profit_taken(&position.mint)
                                            .await;
                                        info!("=== LAYER 1 PROFIT TAKEN (50%) ===");
                                        info!(
                                            "  {} | Entry: {:.10} | Exit: {:.10} | Change: {:+.2}%",
                                            position.symbol,
                                            position.entry_price,
                                            current_price,
                                            price_change_pct
                                        );
                                        info!("  Tokens: {} | Received: {:.4} SOL | P&L: {:+.4} SOL | Hold: {}s",
                                              sell_amount, received, pnl_sol, hold_secs);
                                    } else if sell_pct == "25%" {
                                        // LAYER 2: Second profit - sell 25% of original (50% of remaining)
                                        let sell_amount = position.token_amount / 2; // Half of what's left
                                        let received = if actual_received > 0.0 {
                                            actual_received
                                        } else {
                                            (sell_amount as f64 * current_price) * 0.98
                                        };
                                        // Cost basis is proportional to remaining position
                                        let cost_ratio = sell_amount as f64 / position.token_amount as f64;
                                        let cost_basis = position.total_cost_sol * cost_ratio;
                                        let pnl_sol = received - cost_basis;
                                        let _ = monitor_positions
                                            .close_position(
                                                &position.mint,
                                                sell_amount,
                                                received,
                                            )
                                            .await;
                                        let _ = monitor_positions
                                            .mark_second_profit_taken(&position.mint)
                                            .await;
                                        info!("=== LAYER 2 PROFIT TAKEN (25%) ===");
                                        info!(
                                            "  {} | Entry: {:.10} | Exit: {:.10} | Change: {:+.2}%",
                                            position.symbol,
                                            position.entry_price,
                                            current_price,
                                            price_change_pct
                                        );
                                        info!("  Tokens: {} | Received: {:.4} SOL | P&L: {:+.4} SOL | Hold: {}s",
                                              sell_amount, received, pnl_sol, hold_secs);
                                    } else {
                                        // Use actual received SOL (fallback to estimate if 0)
                                        let received = if actual_received > 0.0 {
                                            actual_received
                                        } else {
                                            (position.token_amount as f64 * current_price) * 0.98
                                        };
                                        let pnl_sol = received - position.total_cost_sol;
                                        let pnl_pct = (pnl_sol / position.total_cost_sol) * 100.0;
                                        let _ = monitor_positions
                                            .close_position(
                                                &position.mint,
                                                position.token_amount,
                                                received,
                                            )
                                            .await;

                                        // Clean up bought_mints on successful full sell
                                        let _ = remove_bought_mint(
                                            &monitor_bought_mints,
                                            &monitor_bought_mints_path,
                                            &position.mint,
                                        )
                                        .await;

                                        // Add to sold_mints with 5-minute cooldown before re-entry
                                        // This prevents immediate re-buy at the top
                                        {
                                            let mut sold = monitor_sold_mints.lock().await;
                                            sold.insert(position.mint.clone(), chrono::Utc::now().timestamp());
                                            info!("[{}] Added to sold_mints (5min cooldown before re-entry)", position.symbol);
                                        }

                                        info!("=== TRADE CLOSED (Full) ===");
                                        info!(
                                            "  {} | Entry: {:.10} | Exit: {:.10} | Change: {:+.2}%",
                                            position.symbol,
                                            position.entry_price,
                                            current_price,
                                            price_change_pct
                                        );
                                        info!("  Cost: {:.4} SOL | Received: {:.4} SOL (actual) | P&L: {:+.4} SOL ({:+.1}%) | Hold: {}s",
                                              position.total_cost_sol, received, pnl_sol, pnl_pct, hold_secs);
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "AUTO-SELL FAILED for {} (attempt {}): {}",
                                        position.symbol, attempts, e
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

                            let buy_result = if use_local_api {
                                route_trader
                                    .buy_local(
                                        &token.mint,
                                        final_buy_amount,
                                        slippage,
                                        priority_fee,
                                        &trading_keypair,
                                        &rpc_client,
                                    )
                                    .await
                            } else {
                                route_trader
                                    .buy(&token.mint, final_buy_amount, slippage, priority_fee)
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
                                    let pending = PendingExecution::buy(
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
                                    if let Err(e) = pending_executions.upsert(pending).await {
                                        // The tx was already sent. Halt new entries, still
                                        // attempt reconciliation, never send another buy.
                                        new_entries_halted.store(true, Ordering::SeqCst);
                                        error!(
                                            "Failed to persist pending HotScan buy for {} (sig {}): {} - halting new entries; still reconciling",
                                            token.symbol, signature, e
                                        );
                                    }

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
                                            error!(
                                                "BUY UNRESOLVED for mint {} sig {} wallet {}: {} - pending kept, HotScan new entries HALTED",
                                                token.mint, signature, wallet_string, reason
                                            );
                                            break;
                                        }
                                        Err(e) => {
                                            // Structural observer failure is not tx-failure
                                            // proof. Keep pending, halt, no failed_mints.
                                            new_entries_halted.store(true, Ordering::SeqCst);
                                            error!(
                                                "CRITICAL: HotScan buy reconciliation error for {} (sig {}): {} - pending kept, new entries HALTED",
                                                token.symbol, signature, e
                                            );
                                            break;
                                        }
                                        Ok(ReconciliationOutcome::ConfirmedFill(fill)) => {
                                            // Validate exact identity at the live boundary.
                                            if fill.side != ReconciliationSide::Buy
                                                || fill.wallet != wallet_string
                                                || fill.mint != token.mint
                                            {
                                                new_entries_halted.store(true, Ordering::SeqCst);
                                                error!(
                                                    "CRITICAL: reconciled HotScan buy fill identity mismatch for sig {} (wallet/mint/side) - pending kept, new entries HALTED",
                                                    signature
                                                );
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
                                                    error!(
                                                        "CRITICAL: reconciled HotScan buy fill conversion failed for sig {}: {} - pending kept, new entries HALTED",
                                                        signature, e
                                                    );
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
                                                    error!(
                                                        "Confirmed owned HotScan position could not be recorded for {} (sig {}): {} - pending kept, new entries HALTED",
                                                        token.symbol, signature, e
                                                    );
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

    for pending in pending_store.all().await {
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
                sold_amount_raw,
                received_sol,
                intent,
                ..
            } => {
                match apply_recovered_sell(
                    positions,
                    &pending,
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
    sold_amount_raw: u64,
    received_sol: f64,
    intent: PendingSellIntent,
) -> crate::error::Result<()> {
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
        let decimals = match current.decimals.or(Some(fill.token_decimals)) {
            Some(d) => d,
            None => {
                summary.still_recovery_required += 1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::types::{TradingAction, TradingStrategy};

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
        apply_recovered_sell(&mgr, &pending, 100, 0.5, PendingSellIntent::Full)
            .await
            .unwrap();
        assert!(mgr.get_position(T_MINT).await.is_none(), "position should be fully closed");

        // Idempotent replay of the same exit signature must not error / double-count.
        apply_recovered_sell(&mgr, &pending, 100, 0.5, PendingSellIntent::Full)
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
        apply_recovered_sell(&mgr, &pending, 40, 0.3, PendingSellIntent::QuickProfit)
            .await
            .unwrap();
        let pos = mgr.get_position(T_MINT).await.expect("partial remains");
        assert!(pos.quick_profit_taken, "quick_profit flag must be set on partial recovery");
        assert!(!pos.second_profit_taken);
        assert_eq!(pos.token_amount, 60);

        // Idempotent replay: still no error, flag stays set.
        apply_recovered_sell(&mgr, &pending, 40, 0.3, PendingSellIntent::QuickProfit)
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
        apply_recovered_sell(&mgr, &pending, 25, 0.2, PendingSellIntent::SecondProfit)
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
        apply_recovered_sell(&mgr, &pending, 40, 0.3, PendingSellIntent::Manual)
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
}
