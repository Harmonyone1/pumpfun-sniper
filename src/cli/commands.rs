//! CLI command implementations

use anyhow::Result;
use dialoguer::Confirm;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::stream::pumpportal::{PumpPortalClient, PumpPortalEvent};
#[cfg(feature = "shredstream")]
use crate::stream::shredstream::ShredStreamClient;
use crate::trading::pumpportal_api::PumpPortalTrader;

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
    let _rpc_client = solana_client::rpc_client::RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        std::time::Duration::from_millis(config.rpc.timeout_ms),
    );

    // Initialize trader based on configuration
    let pumpportal_trader = if config.pumpportal.use_for_trading {
        info!("Using PumpPortal API for trading");
        if config.pumpportal.api_key.is_empty() {
            info!("No API key configured - using Local API (sign transactions yourself)");
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

        // Start PumpPortal connection
        if let Err(e) = pumpportal_client.start(true, track_wallets).await {
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

    // Track wallets for copy trading
    let tracked_wallets: std::collections::HashSet<String> = config
        .wallet_tracking
        .wallets
        .iter()
        .cloned()
        .collect();

    info!("Starting price feed...");
    // Wrap trader in Arc for sharing across tasks
    let trader_arc: Option<std::sync::Arc<PumpPortalTrader>> = pumpportal_trader.map(std::sync::Arc::new);

    // Price feed runs in background, checking positions for TP/SL
    if config.auto_sell.enabled && !dry_run {
        let price_feed_config = config.clone();
        let price_feed_positions = position_manager.clone();
        let price_feed_trader = trader_arc.clone();
        tokio::spawn(async move {
            let poll_interval = std::time::Duration::from_millis(price_feed_config.auto_sell.price_poll_interval_ms);
            loop {
                tokio::time::sleep(poll_interval).await;
                // Check positions for TP/SL triggers
                for position in price_feed_positions.get_all_positions().await {
                    let current_price = position.current_price;
                    if current_price > 0.0 {
                        let pnl_pct = position.unrealized_pnl_pct();

                        // Check take profit
                        if pnl_pct >= price_feed_config.auto_sell.take_profit_pct {
                            info!(
                                "Take profit triggered for {} at {:.1}% gain",
                                position.mint, pnl_pct
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
                        // Check stop loss
                        else if pnl_pct <= -(price_feed_config.auto_sell.stop_loss_pct) {
                            warn!(
                                "Stop loss triggered for {} at {:.1}% loss",
                                position.mint, pnl_pct
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
                            "New token detected: {} ({}) - Mint: {}",
                            token.name, token.symbol, token.mint
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

                        // Execute buy
                        if !dry_run {
                            if let Some(ref trader) = trader_arc {
                                let mint = &token.mint;
                                let amount_sol = config.trading.buy_amount_sol;
                                let slippage_pct = config.trading.slippage_bps / 100;
                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                info!("Buying {} SOL of {} ({})...", amount_sol, token.symbol, mint);

                                match trader.buy(mint, amount_sol, slippage_pct, priority_fee).await {
                                    Ok(signature) => {
                                        info!("Buy successful! Signature: {}", signature);
                                        info!("View on Solscan: https://solscan.io/tx/{}", signature);

                                        // Record position (estimate token amount from bonding curve data)
                                        let estimated_price = if token.v_tokens_in_bonding_curve > 0 {
                                            token.v_sol_in_bonding_curve as f64 / token.v_tokens_in_bonding_curve as f64
                                        } else {
                                            0.000001 // fallback
                                        };

                                        let estimated_tokens = (amount_sol / estimated_price) as u64;

                                        let position = crate::position::manager::Position {
                                            mint: token.mint.clone(),
                                            name: token.name.clone(),
                                            symbol: token.symbol.clone(),
                                            bonding_curve: token.bonding_curve_key.clone(),
                                            token_amount: estimated_tokens,
                                            entry_price: estimated_price,
                                            total_cost_sol: amount_sol,
                                            entry_time: chrono::Utc::now(),
                                            entry_signature: signature.clone(),
                                            current_price: estimated_price,
                                        };

                                        if let Err(e) = position_manager.open_position(position).await {
                                            error!("Failed to record position: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Buy failed for {}: {}", token.symbol, e);
                                    }
                                }
                            }
                        } else {
                            info!("DRY-RUN: Would buy {} SOL of {}", config.trading.buy_amount_sol, token.mint);
                        }
                    }
                    PumpPortalEvent::Trade(trade) => {
                        // Check for tracked wallet trades (copy trading)
                        if config.wallet_tracking.enabled && tracked_wallets.contains(&trade.trader_public_key) {
                            info!(
                                "Tracked wallet {} {} {} tokens of {}",
                                trade.trader_public_key,
                                if trade.tx_type == "buy" { "bought" } else { "sold" },
                                trade.token_amount,
                                trade.mint
                            );

                            // Copy the trade if it's a buy
                            if trade.tx_type == "buy" && !dry_run {
                                if let Some(ref trader) = trader_arc {
                                    let slippage_pct = config.trading.slippage_bps / 100;
                                    let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                    info!("Copy trading: buying {} SOL of {}", config.trading.buy_amount_sol, trade.mint);
                                    match trader.buy(&trade.mint, config.trading.buy_amount_sol, slippage_pct, priority_fee).await {
                                        Ok(sig) => info!("Copy trade executed: {}", sig),
                                        Err(e) => error!("Copy trade failed: {}", e),
                                    }
                                }
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
