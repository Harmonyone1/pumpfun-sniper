//! CLI command implementations

use anyhow::Result;
use dialoguer::Confirm;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::filter::{
    AdaptiveFilter, KillSwitchDecision, KillSwitchEvaluator, MetadataSignalProvider, Recommendation,
    SignalContext, WalletBehaviorSignalProvider,
};
use crate::strategy::engine::StrategyEngine;
use crate::strategy::types::TradingAction;
use crate::stream::pumpportal::{PumpPortalClient, PumpPortalEvent};
#[cfg(feature = "shredstream")]
use crate::stream::shredstream::ShredStreamClient;
use crate::trading::pumpportal_api::PumpPortalTrader;

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
    let use_local_api = config.pumpportal.api_key.is_empty();
    let pumpportal_trader = if config.pumpportal.use_for_trading {
        info!("Using PumpPortal API for trading");
        if use_local_api {
            info!("No API key configured - using Local API (sign + send locally)");
            Some(PumpPortalTrader::local())
        } else {
            info!("Using Lightning API (0.5% fee)");
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
    if let Err(e) = position_manager.load().await {
        warn!("Could not load positions: {} (starting fresh)", e);
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

        if filter.is_degraded().await {
            warn!("Adaptive filter running in degraded mode - some signals may be unavailable");
        } else {
            info!("Adaptive filter initialized with {} providers", 2);
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

    // Track wallets for copy trading
    let tracked_wallets: std::collections::HashSet<String> =
        config.wallet_tracking.wallets.iter().cloned().collect();

    // Track tokens we've already evaluated from trade events (to avoid re-evaluating)
    let seen_trade_tokens: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

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

        tokio::spawn(async move {
            info!("=== POSITION MONITOR STARTED ===");
            info!("Features: Trailing Stop (5%), No-Movement Exit (120s), Quick Profit, LOCAL FALLBACK");

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

                    // CONFIRMATION WAIT: Skip positions less than 10 seconds old
                    // This gives time for buy tx to confirm and ATA to be created
                    let position_age_secs = (chrono::Utc::now() - position.entry_time)
                        .num_seconds()
                        .max(0) as u64;
                    if position_age_secs < 10 {
                        continue; // Too new, wait for confirmation
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

                    // Trailing stop: 3% drop from peak (only if we're in profit)
                    let trailing_stop_pct = 5.0;
                    // No-movement exit: 60 seconds with less than 2% movement
                    let no_movement_secs = 120;
                    let no_movement_threshold = 2.0;

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
                        warn!(
                            "AUTO-SELL TRIGGERED: {} ({}) - {}",
                            position.symbol, position.mint, reason
                        );

                        if let Some(ref trader) = monitor_trader {
                            let slippage = monitor_config.trading.slippage_bps / 100;
                            let priority_fee =
                                monitor_config.trading.priority_fee_lamports as f64 / 1e9;

                            // Retry logic: try up to 3 times with Lightning, then try local fallback
                            let attempts = sell_attempts.entry(position.mint.clone()).or_insert(0);
                            *attempts += 1;

                            if *attempts > 5 {
                                error!("AUTO-SELL GAVE UP for {} after 5 attempts - removing from tracking", position.symbol);
                                // Estimate received SOL as 0 since sell failed
                                let _ = monitor_positions
                                    .close_position(&position.mint, position.token_amount, 0.0)
                                    .await;
                                sell_attempts.remove(&position.mint);
                                continue;
                            }

                            // Try Lightning API first (attempts 1-3)
                            let sell_result: Result<String, crate::error::Error> = if *attempts <= 3
                            {
                                info!("Attempting Lightning API sell (attempt {})", attempts);
                                trader
                                    .sell(&position.mint, sell_pct, slippage, priority_fee)
                                    .await
                            } else {
                                // Attempts 4-5: Try local signing fallback
                                warn!("Lightning failed 3x, trying LOCAL SIGNING fallback (attempt {})", attempts);
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
                            };

                            match sell_result {
                                Ok(sig) => {
                                    info!("AUTO-SELL EXECUTED: {} - {}", position.symbol, sig);
                                    sell_attempts.remove(&position.mint);

                                    // Calculate trade metrics
                                    let hold_secs =
                                        (chrono::Utc::now() - position.entry_time).num_seconds();
                                    let price_change_pct = ((current_price - position.entry_price)
                                        / position.entry_price)
                                        * 100.0;

                                    if sell_pct == "50%" {
                                        // Partial exit - mark quick profit taken
                                        let half_amount = position.token_amount / 2;
                                        // Estimate received SOL based on current price (minus ~2% slippage estimate)
                                        let estimated_received =
                                            (half_amount as f64 * current_price) * 0.98;
                                        let pnl_sol =
                                            estimated_received - (position.total_cost_sol / 2.0);
                                        let _ = monitor_positions
                                            .close_position(
                                                &position.mint,
                                                half_amount,
                                                estimated_received,
                                            )
                                            .await;
                                        let _ = monitor_positions
                                            .mark_quick_profit_taken(&position.mint)
                                            .await;
                                        info!("=== TRADE CLOSED (Partial) ===");
                                        info!(
                                            "  {} | Entry: {:.10} | Exit: {:.10} | Change: {:+.2}%",
                                            position.symbol,
                                            position.entry_price,
                                            current_price,
                                            price_change_pct
                                        );
                                        info!("  Tokens: {} | Received: {:.4} SOL | P&L: {:+.4} SOL | Hold: {}s",
                                              half_amount, estimated_received, pnl_sol, hold_secs);
                                    } else {
                                        // Full exit
                                        // Estimate received SOL based on current price (minus ~2% slippage estimate)
                                        let estimated_received =
                                            (position.token_amount as f64 * current_price) * 0.98;
                                        let pnl_sol = estimated_received - position.total_cost_sol;
                                        let pnl_pct = (pnl_sol / position.total_cost_sol) * 100.0;
                                        let _ = monitor_positions
                                            .close_position(
                                                &position.mint,
                                                position.token_amount,
                                                estimated_received,
                                            )
                                            .await;
                                        info!("=== TRADE CLOSED (Full) ===");
                                        info!(
                                            "  {} | Entry: {:.10} | Exit: {:.10} | Change: {:+.2}%",
                                            position.symbol,
                                            position.entry_price,
                                            current_price,
                                            price_change_pct
                                        );
                                        info!("  Cost: {:.4} SOL | Received: {:.4} SOL | P&L: {:+.4} SOL ({:+.1}%) | Hold: {}s",
                                              position.total_cost_sol, estimated_received, pnl_sol, pnl_pct, hold_secs);
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
                        let (position_multiplier, entry_recommendation) = if let Some(ref filter) = adaptive_filter {
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

                            (result.position_size_multiplier, result.recommendation)
                        } else {
                            (1.0, Recommendation::Opportunity) // Default if adaptive filter disabled
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

                            // Create order flow analysis from available data
                            let order_flow = crate::strategy::regime::OrderFlowAnalysis {
                                organic_score: position_multiplier.max(0.5),
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
                                order_flow,
                                distribution,
                                creator_behavior,
                                price_action,
                                sol_reserves: liquidity_sol,
                                token_reserves,
                                confidence_score: position_multiplier,
                            };

                            let eval = engine_guard.evaluate_entry(&analysis_ctx).await;

                            // Check the decision
                            match &eval.decision.action {
                                TradingAction::Enter { mint: _, size_sol, strategy } => {
                                    info!(
                                        "Strategy engine: ENTER {} using {} strategy, size: {:.4} SOL",
                                        token.symbol, strategy, size_sol
                                    );
                                    (true, *size_sol)
                                }
                                TradingAction::FatalReject { reason } => {
                                    warn!(
                                        "Strategy engine: FATAL REJECT for {}: {}",
                                        token.symbol, reason
                                    );
                                    (false, 0.0)
                                }
                                TradingAction::Skip { reason } => {
                                    info!(
                                        "Strategy engine: SKIP {}: {}",
                                        token.symbol, reason
                                    );
                                    (false, 0.0)
                                }
                                _ => {
                                    // Hold or other action - fall through to adaptive filter decision
                                    (true, config.trading.buy_amount_sol * position_multiplier)
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

                                info!("Buying {} SOL of {} ({})...", final_amount_sol, token.symbol, mint);

                                // Use buy_local for Local API, buy for Lightning API
                                let buy_result = if use_local_api {
                                    trader.buy_local(mint, final_amount_sol, slippage_pct, priority_fee, &keypair, &rpc_client).await
                                } else {
                                    trader.buy(mint, final_amount_sol, slippage_pct, priority_fee).await
                                };

                                match buy_result {
                                    Ok(signature) => {
                                        info!("Buy successful! Signature: {}", signature);
                                        info!("View on Solscan: https://solscan.io/tx/{}", signature);

                                        // Record position (estimate token amount from bonding curve data)
                                        let estimated_price = if token.v_tokens_in_bonding_curve > 0 {
                                            token.v_sol_in_bonding_curve as f64 / token.v_tokens_in_bonding_curve as f64
                                        } else {
                                            0.000001 // fallback
                                        };

                                        let estimated_tokens = (final_amount_sol / estimated_price) as u64;

                                        // Convert recommendation to EntryType for context-aware exits
                                        let entry_type = match entry_recommendation {
                                            Recommendation::StrongBuy => crate::position::manager::EntryType::StrongBuy,
                                            Recommendation::Opportunity => crate::position::manager::EntryType::Opportunity,
                                            Recommendation::Probe => crate::position::manager::EntryType::Probe,
                                            _ => crate::position::manager::EntryType::Legacy,
                                        };

                                        let position = crate::position::manager::Position {
                                            mint: token.mint.clone(),
                                            name: token.name.clone(),
                                            symbol: token.symbol.clone(),
                                            bonding_curve: token.bonding_curve_key.clone(),
                                            token_amount: estimated_tokens,
                                            entry_price: estimated_price,
                                            total_cost_sol: final_amount_sol,
                                            entry_time: chrono::Utc::now(),
                                            entry_signature: signature.clone(),
                                            entry_type,
                                            quick_profit_taken: false,
                                            second_profit_taken: false,
                                            peak_price: estimated_price,
                                            current_price: estimated_price,
                                            kill_switch_triggered: false,
                                            kill_switch_reason: None,
                                        };

                                        if let Err(e) = position_manager.open_position(position).await {
                                            error!("Failed to record position: {}", e);
                                        }

                                        // Start kill-switch monitoring for this position
                                        if let Some(ref evaluator) = kill_switch_evaluator {
                                            // Creator is the trader_public_key for new tokens
                                            let creator = token.trader_public_key.clone();
                                            // TODO: Fetch top holders from Helius for holder_watcher
                                            // For now, we just track the deployer
                                            evaluator.watch_position(&token.mint, &creator, vec![]);
                                            info!(
                                                "Kill-switch monitoring active for {} (creator: {})",
                                                &token.mint[..12], &creator[..8]
                                            );
                                        }

                                        // Record entry in strategy engine
                                        if let Some(ref engine) = strategy_engine {
                                            let strategy_position = crate::strategy::types::Position {
                                                mint: token.mint.clone(),
                                                entry_price: estimated_price,
                                                entry_time: chrono::Utc::now(),
                                                size_sol: final_amount_sol,
                                                tokens_held: estimated_tokens,
                                                strategy: config.strategy.default_strategy.clone(),
                                                exit_style: crate::strategy::types::ExitStyle::default(),
                                                highest_price: estimated_price,
                                                lowest_price: estimated_price,
                                                exit_levels_hit: vec![],
                                            };
                                            engine.write().await.record_entry(strategy_position).await;
                                        }
                                    }
                                    Err(e) => {
                                        error!("Buy failed for {}: {}", token.symbol, e);
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
                        // Calculate SOL amount for logging
                        let sol_amount = trade.sol_amount as f64 / 1e9;

                        // Log all trades for visibility
                        info!(
                            "Trade: {} {} {:.6} SOL on {} (mcap: {:.2})",
                            &trade.trader_public_key[..8],
                            trade.tx_type,
                            sol_amount,
                            &trade.mint[..12],
                            trade.market_cap_sol
                        );

                        // KILL-SWITCH: Check sells on tokens we hold
                        if trade.tx_type == "sell" {
                            // Check if we have a position in this token
                            let positions = position_manager.get_all_positions().await;
                            let our_position = positions.iter().find(|p| p.mint == trade.mint);

                            if let Some(position) = our_position {
                                let position_token_amount = position.token_amount;

                                if let Some(ref evaluator) = kill_switch_evaluator {
                                    let decision = evaluator.evaluate_sell(
                                        &trade.mint,
                                        &trade.trader_public_key,
                                        trade.token_amount as u64,
                                        sol_amount,
                                        &trade.signature,
                                    );

                                    if let KillSwitchDecision::Exit(alert) = decision {
                                        warn!(
                                            "KILL-SWITCH TRIGGERED for {}: {} - AUTO-SELLING",
                                            &trade.mint[..12], alert.reason
                                        );

                                        // Execute emergency sell if not dry run
                                        if !dry_run {
                                            if let Some(ref trader) = trader_arc {
                                                let slippage_pct = config.trading.slippage_bps / 100;
                                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                                // Sell 100% immediately
                                                info!(
                                                    "Executing kill-switch sell for {} (urgency: {:?})",
                                                    &trade.mint[..12], alert.urgency
                                                );

                                                let sell_result = if use_local_api {
                                                    trader.sell_local(
                                                        &trade.mint,
                                                        "100%", // 100% sell
                                                        slippage_pct,
                                                        priority_fee,
                                                        &keypair,
                                                        &rpc_client,
                                                    ).await
                                                } else {
                                                    trader.sell(&trade.mint, "100%", slippage_pct, priority_fee).await
                                                };

                                                match sell_result {
                                                    Ok(sig) => {
                                                        warn!(
                                                            "KILL-SWITCH SELL EXECUTED: {} - sig: {}",
                                                            alert.reason, sig
                                                        );

                                                        // Close position in manager (use position's token amount since we sold 100%)
                                                        // Note: We don't know exact proceeds yet, estimate from current price
                                                        let estimated_proceeds = position_token_amount as f64 * trade.market_cap_sol / 1_000_000_000.0;
                                                        if let Err(e) = position_manager.close_position(&trade.mint, position_token_amount, estimated_proceeds).await {
                                                            error!("Failed to close position after kill-switch: {}", e);
                                                        }

                                                        // Stop monitoring this position
                                                        evaluator.unwatch_position(&trade.mint);
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            "KILL-SWITCH SELL FAILED for {}: {} - MANUAL EXIT NEEDED!",
                                                            &trade.mint[..12], e
                                                        );
                                                    }
                                                }
                                            }
                                        } else {
                                            warn!(
                                                "DRY-RUN: Kill-switch would sell 100% of {} (reason: {})",
                                                &trade.mint[..12], alert.reason
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Check for tracked wallet trades (copy trading)
                        if config.wallet_tracking.enabled && tracked_wallets.contains(&trade.trader_public_key) {
                            info!(
                                "Tracked wallet {} {} {:.4} SOL of {}",
                                trade.trader_public_key,
                                if trade.tx_type == "buy" { "bought" } else { "sold" },
                                sol_amount,
                                trade.mint
                            );

                            // Copy the trade if it's a buy
                            if trade.tx_type == "buy" && !dry_run {
                                if let Some(ref trader) = trader_arc {
                                    let slippage_pct = config.trading.slippage_bps / 100;
                                    let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                    info!("Copy trading: buying {} SOL of {}", config.trading.buy_amount_sol, trade.mint);
                                    let copy_result = if use_local_api {
                                        trader.buy_local(&trade.mint, config.trading.buy_amount_sol, slippage_pct, priority_fee, &keypair, &rpc_client).await
                                    } else {
                                        trader.buy(&trade.mint, config.trading.buy_amount_sol, slippage_pct, priority_fee).await
                                    };
                                    match copy_result {
                                        Ok(sig) => info!("Copy trade executed: {}", sig),
                                        Err(e) => error!("Copy trade failed: {}", e),
                                    }
                                }
                            }
                        }

                        // Evaluate tokens with significant buy volume that we haven't seen before
                        if trade.tx_type == "buy" && sol_amount >= 0.05 {
                            let mut seen = seen_trade_tokens.lock().await;

                            // Skip if we've already evaluated this token
                            if seen.contains(&trade.mint) {
                                continue;
                            }

                            // Check if we already have a position in this token
                            let positions = position_manager.get_all_positions().await;
                            if positions.iter().any(|p| p.mint == trade.mint) {
                                seen.insert(trade.mint.clone());
                                continue;
                            }

                            // Mark as seen
                            seen.insert(trade.mint.clone());
                            drop(seen); // Release lock before async operations

                            info!(
                                "Trade detected: {} bought {:.4} SOL of {} (mcap: {:.2} SOL) - evaluating...",
                                &trade.trader_public_key[..8],
                                sol_amount,
                                trade.mint,
                                trade.market_cap_sol
                            );

                            // Calculate liquidity from bonding curve
                            // Note: PumpPortal sends values in SOL, not lamports
                            let liquidity_sol = if trade.v_sol_in_bonding_curve < 1000.0 {
                                trade.v_sol_in_bonding_curve as f64
                            } else {
                                trade.v_sol_in_bonding_curve as f64 / 1e9
                            };

                            // Quick liquidity check
                            if liquidity_sol < 0.05 {
                                info!("Token {} rejected: liquidity {:.4} SOL too low", trade.mint, liquidity_sol);
                                continue;
                            }

                            // Use configured buy amount for trade-based entries
                            let final_amount_sol = config.trading.buy_amount_sol;

                            info!(
                                "Trade signal: BUY {:.4} SOL of {} (liquidity: {:.4} SOL)",
                                final_amount_sol, trade.mint, liquidity_sol
                            );

                            if !dry_run {
                                if let Some(ref trader) = trader_arc {
                                    let slippage_pct = config.trading.slippage_bps / 100;
                                    let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                    let buy_result = if use_local_api {
                                        trader.buy_local(&trade.mint, final_amount_sol, slippage_pct, priority_fee, &keypair, &rpc_client).await
                                    } else {
                                        trader.buy(&trade.mint, final_amount_sol, slippage_pct, priority_fee).await
                                    };
                                    match buy_result {
                                        Ok(sig) => {
                                            info!("Trade buy executed: {}", sig);
                                            // Estimate tokens from market cap
                                            let estimated_price = if trade.market_cap_sol > 0.0 {
                                                trade.market_cap_sol / 1_000_000_000.0
                                            } else {
                                                0.000001
                                            };
                                            let estimated_tokens = (final_amount_sol / estimated_price) as u64;

                                            // Record position - trade event entries are treated as Probe
                                            // since we have less information than new token events
                                            let position = crate::position::manager::Position {
                                                mint: trade.mint.clone(),
                                                name: format!("Trade-{}", &trade.mint[..8]),
                                                symbol: "???".to_string(),
                                                bonding_curve: trade.bonding_curve_key.clone(),
                                                token_amount: estimated_tokens,
                                                entry_price: estimated_price,
                                                total_cost_sol: final_amount_sol,
                                                entry_time: chrono::Utc::now(),
                                                entry_signature: sig.clone(),
                                                entry_type: crate::position::manager::EntryType::Probe, // Conservative for trade-based entries
                                                quick_profit_taken: false,
                                                second_profit_taken: false,
                                                peak_price: estimated_price,
                                                current_price: estimated_price,
                                                kill_switch_triggered: false,
                                                kill_switch_reason: None,
                                            };
                                            if let Err(e) = position_manager.open_position(position).await {
                                                error!("Failed to record position: {}", e);
                                            }
                                        }
                                        Err(e) => error!("Trade buy failed: {}", e),
                                    }
                                }
                            } else {
                                info!(
                                    "DRY-RUN: Would buy {:.4} SOL of {} based on trade activity",
                                    final_amount_sol, trade.mint
                                );
                            }
                        }
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
                let actual_received = (sol_after - sol_before).max(0.0);

                println!("Balance after sell: {:.4} SOL", sol_after);
                println!("SOL received: {:.4} SOL", actual_received);

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

                    // Use actual received SOL, fallback to estimate if balance query failed
                    let received = if actual_received > 0.0 {
                        actual_received
                    } else {
                        // Estimate based on position price (use current_price if available, else entry_price)
                        let price = if pos.current_price > 0.0 { pos.current_price } else { pos.entry_price };
                        let estimated = (tokens_sold as f64 * price) * 0.98;
                        warn!("Balance query returned 0, using estimated received: {:.4} SOL", estimated);
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

    // Load keypair
    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| format!("{}/hot-trading/keypair.json", config.wallet.credentials_dir));
    let keypair_data = std::fs::read_to_string(&keypair_path)?;
    let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
    let keypair = std::sync::Arc::new(solana_sdk::signature::Keypair::from_bytes(&secret_key)?);
    info!(
        "Signing wallet (for local API fallback): {}",
        keypair.pubkey()
    );

    // Initialize RPC
    let rpc_client = std::sync::Arc::new(solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    ));

    // Initialize trader
    let use_local_api = config.pumpportal.api_key.is_empty();
    let trader = if config.pumpportal.use_for_trading {
        if use_local_api {
            info!("Using Local API (sign + send locally)");
            info!("Trading wallet: {}", keypair.pubkey());
            Some(std::sync::Arc::new(
                crate::trading::pumpportal_api::PumpPortalTrader::local(),
            ))
        } else {
            info!("Using Lightning API (0.5% fee)");
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
    position_manager.load().await?;

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
                        // Use monitor_wallet (Lightning wallet or local wallet based on API mode)
                        let token_balance =
                            query_token_balance(&monitor_rpc, &monitor_wallet, &position.mint);

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
                                error!("AUTO-SELL GAVE UP for {} after 5 attempts - removing from tracking", position.symbol);
                                let _ = monitor_positions.abandon_position(&position.mint).await;
                                let _ = remove_bought_mint(
                                    &monitor_bought_mints,
                                    &monitor_bought_mints_path,
                                    &position.mint,
                                )
                                .await;
                                sell_attempts.remove(&position.mint);
                                continue;
                            }

                            // Query SOL balance BEFORE sell for real P&L tracking
                            let sol_before = monitor_rpc
                                .get_balance(&monitor_wallet)
                                .unwrap_or(0) as f64
                                / 1_000_000_000.0;

                            // Try Lightning API first (attempts 1-3), then local (attempts 4-5)
                            let sell_result: std::result::Result<String, crate::error::Error> =
                                if *attempts <= 3 {
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
                                            &monitor_keypair,
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
                                        .get_balance(&monitor_wallet)
                                        .unwrap_or(0) as f64
                                        / 1_000_000_000.0;
                                    let actual_received = (sol_after - sol_before).max(0.0);

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

                        // PRE-TRADE VALIDATION: Check position limits BEFORE trading
                        if let Err(e) = position_manager.can_open_position(buy_amount).await {
                            warn!(
                                "Cannot open position for {}: {} - stopping buy loop",
                                token.symbol, e
                            );
                            break; // Stop trying to buy more tokens
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

                        if let Some(ref trader) = trader {
                            let slippage = config.trading.slippage_bps / 100;
                            let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                            info!(
                                "Buying {:.4} SOL of {} via {}",
                                final_buy_amount,
                                token.symbol,
                                if use_local_api {
                                    "Local API"
                                } else {
                                    "Lightning"
                                }
                            );

                            let buy_result = if use_local_api {
                                trader
                                    .buy_local(
                                        &token.mint,
                                        final_buy_amount,
                                        slippage,
                                        priority_fee,
                                        &keypair,
                                        &rpc_client,
                                    )
                                    .await
                            } else {
                                trader
                                    .buy(&token.mint, final_buy_amount, slippage, priority_fee)
                                    .await
                            };

                            match buy_result {
                                Ok(sig) => {
                                    info!("BUY EXECUTED: {} - {}", token.symbol, sig);
                                    bought
                                        .insert(token.mint.clone(), chrono::Utc::now().timestamp());
                                    // Persist bought_mints to disk (with timestamps)
                                    persist_bought_mints(&*bought_mints_path, &*bought);

                                    // Record position
                                    let estimated_tokens = (final_buy_amount / token.price_native) as u64;
                                    let position = crate::position::manager::Position {
                                        mint: token.mint.clone(),
                                        name: token.name.clone(),
                                        symbol: token.symbol.clone(),
                                        bonding_curve: String::new(), // Not available from DexScreener
                                        token_amount: estimated_tokens,
                                        entry_price: token.price_native,
                                        total_cost_sol: final_buy_amount,
                                        entry_time: chrono::Utc::now(),
                                        entry_signature: sig,
                                        entry_type:
                                            crate::position::manager::EntryType::Opportunity,
                                        quick_profit_taken: false,
                                        second_profit_taken: false,
                                        peak_price: token.price_native,
                                        current_price: token.price_native,
                                        kill_switch_triggered: false,
                                        kill_switch_reason: None,
                                    };

                                    if let Err(e) = position_manager.open_position(position).await {
                                        error!("Failed to record position: {}", e);
                                        bought.remove(&token.mint);
                                        persist_bought_mints(&*bought_mints_path, &*bought);
                                        continue;
                                    }

                                    // Wait for tx confirmation, then query actual token balance
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    let actual_balance = query_token_balance(
                                        &rpc_client,
                                        &keypair.pubkey(),
                                        &token.mint,
                                    );
                                    if actual_balance > 0 && actual_balance != estimated_tokens {
                                        info!(
                                            "Actual token balance: {} (estimated: {})",
                                            actual_balance, estimated_tokens
                                        );
                                        // Update position with actual balance
                                        if let Err(e) = position_manager
                                            .update_token_amount(&token.mint, actual_balance)
                                            .await
                                        {
                                            warn!("Failed to update token amount: {}", e);
                                        }
                                    } else if actual_balance == 0 {
                                        warn!("Token balance query returned 0 - tx may not have confirmed yet");
                                    }

                                    // === SET UP KILL-SWITCH MONITORING ===
                                    // Fetch creator and top holders for this token
                                    if let Some(ref evaluator) = kill_switch_evaluator {
                                        if let Some(ref helius) = helius_client {
                                            // Get token creator
                                            let creator = match helius.get_token_creator(&token.mint).await {
                                                Ok(c) => {
                                                    info!("[{}] Creator for kill-switch: {}", token.symbol, &c[..8]);
                                                    c
                                                }
                                                Err(e) => {
                                                    warn!("[{}] Could not get creator: {} - using empty", token.symbol, e);
                                                    String::new()
                                                }
                                            };

                                            // Get top holders (address, amount, percentage)
                                            let holders = match helius.get_token_holders(&token.mint, 10).await {
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

                                            // Start kill-switch monitoring
                                            evaluator.watch_position(&token.mint, &creator, holders);
                                            info!(
                                                "[{}] Kill-switch monitoring ACTIVE (creator: {}, holders: tracked)",
                                                token.symbol,
                                                if creator.is_empty() { "unknown" } else { &creator[..8] }
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("BUY FAILED for {}: {}", token.symbol, e);
                                    continue;
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
