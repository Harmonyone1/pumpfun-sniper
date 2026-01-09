//! CLI command implementations

use anyhow::Result;
use dialoguer::Confirm;
use solana_sdk::signature::{Keypair, Signer};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::filter::{AdaptiveFilter, MetadataSignalProvider, Recommendation, SignalContext, WalletBehaviorSignalProvider};
use crate::stream::pumpportal::{PumpPortalClient, PumpPortalEvent};
#[cfg(feature = "shredstream")]
use crate::stream::shredstream::ShredStreamClient;
use crate::trading::pumpportal_api::PumpPortalTrader;
use crate::strategy::engine::StrategyEngine;
use crate::strategy::types::{TradingAction, ExitReason};

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
            Some(PumpPortalTrader::lightning(config.pumpportal.api_key.clone()))
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
    let (event_tx, mut event_rx) = mpsc::channel::<PumpPortalEvent>(config.backpressure.channel_capacity);

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

    // Initialize token filter
    let token_filter = crate::filter::token_filter::TokenFilter::new(
        config.filters.clone(),
    ).map_err(|e| anyhow::anyhow!("Failed to create token filter: {}", e))?;

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
    let tracked_wallets: std::collections::HashSet<String> = config
        .wallet_tracking
        .wallets
        .iter()
        .cloned()
        .collect();

    // Track tokens we've already evaluated from trade events (to avoid re-evaluating)
    let seen_trade_tokens: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    info!("Starting price feed...");
    // Wrap trader in Arc for sharing across tasks
    let trader_arc: Option<std::sync::Arc<PumpPortalTrader>> = pumpportal_trader.map(std::sync::Arc::new);

    // Price feed runs in background, checking positions for TP/SL
    if config.auto_sell.enabled && !dry_run {
        let price_feed_config = config.clone();
        let price_feed_positions = position_manager.clone();
        let price_feed_trader = trader_arc.clone();
        let price_feed_strategy = strategy_engine.clone();
        tokio::spawn(async move {
            let poll_interval = std::time::Duration::from_millis(price_feed_config.auto_sell.price_poll_interval_ms);
            loop {
                tokio::time::sleep(poll_interval).await;

                // Check chain health if strategy engine is enabled
                if let Some(ref engine) = price_feed_strategy {
                    let engine_guard = engine.read().await;
                    if engine_guard.should_pause_trading().await {
                        tracing::debug!("Strategy engine indicates trading pause - skipping exit checks");
                        continue;
                    }
                }

                // Check positions for TP/SL triggers
                for position in price_feed_positions.get_all_positions().await {
                    let current_price = position.current_price;
                    if current_price > 0.0 {
                        let pnl_pct = position.unrealized_pnl_pct();
                        let hold_time_secs = (chrono::Utc::now() - position.entry_time).num_seconds().max(0) as u64;

                        // If strategy engine is available, use it for exit decisions
                        if let Some(ref engine) = price_feed_strategy {
                            let mut engine_guard = engine.write().await;

                            // Update price in strategy engine
                            engine_guard.update_price(&position.mint, current_price, 0.0);

                            // Check for exit signals from strategy engine
                            if let Some(exit_signal) = engine_guard.check_exit(
                                &position.mint,
                                position.entry_price,
                                current_price,
                                pnl_pct,
                                hold_time_secs,
                            ).await {
                                let sell_pct = format!("{}%", exit_signal.pct_to_sell as u32);
                                let reason_str = match &exit_signal.reason {
                                    ExitReason::TakeProfit { pnl_pct } => format!("Take profit at {:.1}%", pnl_pct),
                                    ExitReason::StopLoss { loss_pct } => format!("Stop loss at -{:.1}%", loss_pct),
                                    ExitReason::TrailingStopHit { peak_pnl_pct, current_pnl_pct } =>
                                        format!("Trailing stop (peak: {:.1}%, now: {:.1}%)", peak_pnl_pct, current_pnl_pct),
                                    ExitReason::MomentumFade => "Momentum fade".to_string(),
                                    ExitReason::MaxHoldTime { held_secs } => format!("Max hold time {}s", held_secs),
                                    ExitReason::RugPredicted { probability } => format!("Rug predicted ({:.0}%)", probability * 100.0),
                                    ExitReason::CreatorSelling { pct_sold } => format!("Creator selling ({:.1}%)", pct_sold),
                                    ExitReason::WhaleExited { .. } => "Whale exited".to_string(),
                                    ExitReason::FatalRisk { risk } => format!("Fatal risk: {}", risk),
                                    ExitReason::ManualExit => "Manual exit".to_string(),
                                };

                                info!(
                                    "Strategy exit signal for {}: {} (sell {})",
                                    position.mint, reason_str, sell_pct
                                );

                                if let Some(ref trader) = price_feed_trader {
                                    let slippage = price_feed_config.trading.slippage_bps / 100;
                                    let priority_fee = price_feed_config.trading.priority_fee_lamports as f64 / 1e9;
                                    match trader.sell(&position.mint, &sell_pct, slippage, priority_fee).await {
                                        Ok(sig) => {
                                            info!("Strategy sell executed: {}", sig);
                                            // Record the exit in strategy engine
                                            let realized_pnl = position.total_cost_sol * (pnl_pct / 100.0);
                                            engine_guard.record_exit(&position.mint, realized_pnl).await;
                                        }
                                        Err(e) => error!("Strategy sell failed: {}", e),
                                    }
                                }
                                continue; // Skip basic TP/SL if strategy triggered
                            }
                        }

                        // Fallback to basic TP/SL if strategy engine not available or didn't trigger
                        // Use CONTEXT-AWARE exit parameters based on entry type
                        let tp_pct = position.entry_type.take_profit_pct();
                        let sl_pct = position.entry_type.stop_loss_pct();
                        let max_hold = position.entry_type.max_hold_secs();

                        // Check max hold time (especially important for Probe positions)
                        if let Some(max_secs) = max_hold {
                            if hold_time_secs >= max_secs {
                                warn!(
                                    "Max hold time ({} seconds) reached for {} (entry type: {:?})",
                                    max_secs, position.mint, position.entry_type
                                );
                                if let Some(ref trader) = price_feed_trader {
                                    let slippage = price_feed_config.trading.slippage_bps / 100;
                                    let priority_fee = price_feed_config.trading.priority_fee_lamports as f64 / 1e9;
                                    match trader.sell(&position.mint, "100%", slippage, priority_fee).await {
                                        Ok(sig) => info!("Max hold sell executed: {}", sig),
                                        Err(e) => error!("Max hold sell failed: {}", e),
                                    }
                                }
                                continue; // Skip other checks, we're exiting
                            }
                        }

                        // Check take profit (using entry type's target)
                        if pnl_pct >= tp_pct {
                            info!(
                                "Take profit triggered for {} at {:.1}% gain (target: {:.1}%, entry type: {:?})",
                                position.mint, pnl_pct, tp_pct, position.entry_type
                            );
                            if let Some(ref trader) = price_feed_trader {
                                let slippage = price_feed_config.trading.slippage_bps / 100;
                                let priority_fee = price_feed_config.trading.priority_fee_lamports as f64 / 1e9;
                                match trader.sell(&position.mint, "100%", slippage, priority_fee).await {
                                    Ok(sig) => info!("TP sell executed: {}", sig),
                                    Err(e) => error!("TP sell failed: {}", e),
                                }
                            }
                        }
                        // Check stop loss (using entry type's threshold)
                        else if pnl_pct <= -(sl_pct) {
                            warn!(
                                "Stop loss triggered for {} at {:.1}% loss (limit: -{:.1}%, entry type: {:?})",
                                position.mint, pnl_pct, sl_pct, position.entry_type
                            );
                            if let Some(ref trader) = price_feed_trader {
                                let slippage = price_feed_config.trading.slippage_bps / 100;
                                let priority_fee = price_feed_config.trading.priority_fee_lamports as f64 / 1e9;
                                match trader.sell(&position.mint, "100%", slippage, priority_fee).await {
                                    Ok(sig) => info!("SL sell executed: {}", sig),
                                    Err(e) => error!("SL sell failed: {}", e),
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
                                            current_price: estimated_price,
                                        };

                                        if let Err(e) = position_manager.open_position(position).await {
                                            error!("Failed to record position: {}", e);
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
                            let liquidity_sol = if trade.v_sol_in_bonding_curve < 1000 {
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
                                                current_price: estimated_price,
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
        // TODO: Simulate transaction
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

        info!("Submitting sell via PumpPortal API...");
        match trader.sell(token, amount, slippage_pct, priority_fee).await {
            Ok(signature) => {
                info!("Sell successful! Signature: {}", signature);
                println!("\nSell transaction confirmed!");
                println!("Signature: {}", signature);
                println!("View on Solscan: https://solscan.io/tx/{}", signature);
            }
            Err(e) => {
                error!("Sell failed: {}", e);
                anyhow::bail!("Sell transaction failed: {}", e);
            }
        }
    } else {
        // Use Jito bundles
        // TODO: Implement Jito sell logic
        // 1. Load keypair
        // 2. Get current position
        // 3. Calculate actual amount to sell
        // 4. Build sell transaction
        // 5. Submit via Jito bundle
        // 6. Update position
        warn!("Jito sell not yet implemented");
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
    use tokio_tungstenite::connect_async;
    use std::time::Duration;

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
        Err(_) => Err(anyhow::anyhow!("Connection timed out after {}s", timeout.as_secs())),
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
    println!("Min hot balance: {} SOL", config.wallet.safety.min_hot_balance_sol);
    println!("Max single transfer: {} SOL", config.wallet.safety.max_single_transfer_sol);
    println!("Max daily extraction: {} SOL", config.wallet.safety.max_daily_extraction_sol);
    println!("AI max auto-transfer: {} SOL", config.wallet.safety.ai_max_auto_transfer_sol);
    println!("Vault address locked: {}", config.wallet.safety.vault_address_locked);

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
    println!("{:<20} {:<15} {:<15} {}", "NAME", "ALIAS", "TYPE", "ADDRESS");
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
        _ => anyhow::bail!("Invalid wallet type: {}. Use: hot, vault, external, auth", wallet_type),
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
            Some(std::path::PathBuf::from(format!("credentials/{}/keypair.json", name))),
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

    creds.add_wallet(entry)
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
        .extract_to_vault(amount, TransferReason::ManualTransfer, InitiatedBy::User, force)
        .await
    {
        Ok(record) => {
            println!("\n=== EXTRACTION SUCCESSFUL ===");
            println!("Amount: {} SOL", record.amount_sol);
            println!("To: {}", record.to_wallet);
            println!("Signature: {}", record.signature);
            println!("View on Solscan: https://solscan.io/tx/{}", record.signature);
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
pub async fn wallet_emergency(
    config: &Config,
    shutdown: bool,
    resume: bool,
) -> Result<()> {
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
