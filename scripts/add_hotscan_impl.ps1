$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Add import for DexScreener
$oldImports = 'use crate::trading::pumpportal_api::PumpPortalTrader;'
$newImports = @'
use crate::trading::pumpportal_api::PumpPortalTrader;
use crate::dexscreener::{DexScreenerClient, HotScanConfig, HotToken};
'@
$content = $content.Replace($oldImports, $newImports)

# Add hot_scan function at the end
$hotScanFunc = @'


/// Scan DexScreener for hot tokens with Survivor Mode validation
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
) -> Result<()> {
    info!("=== HOT TOKEN SCANNER ===");
    info!("Filters: 5m change >= {:.1}%, ratio >= {:.2}, liq >= ${:.0}, mcap <= ${:.0}",
        min_m5, min_ratio, min_liquidity, max_mcap);

    if auto_buy {
        if dry_run {
            warn!("DRY-RUN: Auto-buy enabled but no real trades will execute");
        } else {
            warn!("AUTO-BUY ENABLED: Will buy {:.4} SOL of validated tokens!", buy_amount);
        }
    }

    // Initialize components
    let dex_client = DexScreenerClient::new();

    // Initialize Helius for holder checks (Survivor Mode)
    let helius_api_key = std::env::var("HELIUS_API_KEY")
        .unwrap_or_else(|_| config.rpc.endpoint.split("api-key=").last().unwrap_or("").to_string());
    let helius_client = if !helius_api_key.is_empty() {
        Some(HeliusClient::new(&helius_api_key))
    } else {
        warn!("No Helius API key - holder concentration checks disabled");
        None
    };

    // Initialize trader if auto_buy
    let trader = if auto_buy && !dry_run {
        let keypair_path = std::env::var("KEYPAIR_PATH")
            .unwrap_or_else(|_| "credentials/hot-trading/keypair.json".to_string());
        let keypair_data = std::fs::read_to_string(&keypair_path)?;
        let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)?;
        let keypair = Keypair::from_bytes(&secret_key)?;
        info!("Trader wallet: {}", keypair.pubkey());

        let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new(config.rpc.endpoint.clone()));
        Some((PumpPortalTrader::new(keypair.pubkey().to_string()), keypair, rpc_client))
    } else {
        None
    };

    // Initialize position manager
    let position_manager = crate::position::manager::PositionManager::new("data/positions.json".to_string());
    if let Err(e) = position_manager.load().await {
        warn!("Failed to load positions: {}", e);
    }

    // Track seen tokens to avoid duplicates
    let mut seen_tokens = std::collections::HashSet::new();

    let scan_config = HotScanConfig {
        min_m5_change: min_m5,
        min_buy_sell_ratio: min_ratio,
        min_buys_5m: 10,
        min_liquidity_usd: min_liquidity,
        min_market_cap: 10_000.0,
        max_market_cap: max_mcap,
        scan_profiles: true,
        scan_boosts: true,
        profile_limit: 30,
        boost_limit: 15,
    };

    loop {
        info!("\n--- SCANNING FOR HOT TOKENS ---");

        match dex_client.scan_hot_tokens(&scan_config).await {
            Ok(hot_tokens) => {
                if hot_tokens.is_empty() {
                    info!("No hot opportunities found matching criteria");
                } else {
                    info!("Found {} hot opportunities:", hot_tokens.len());

                    for token in &hot_tokens {
                        let boost_tag = if token.is_boosted { " [BOOSTED]" } else { "" };
                        info!(
                            "{}: {:.1}% 5m, {:.1}% 1h | B/S: {}/{} ({:.2}) | MCap: ${:.0} | Liq: ${:.0}{}",
                            token.symbol, token.m5_change, token.h1_change,
                            token.buys_5m, token.sells_5m, token.buy_sell_ratio,
                            token.market_cap, token.liquidity_usd, boost_tag
                        );
                        info!("  Mint: {}", token.mint);

                        // Skip if already seen/bought
                        if seen_tokens.contains(&token.mint) {
                            debug!("  Already processed, skipping");
                            continue;
                        }

                        // Check if we already have a position
                        if position_manager.has_position(&token.mint).await {
                            info!("  Already have position, skipping");
                            continue;
                        }

                        // SURVIVOR MODE: Check holder concentration
                        if let Some(ref helius) = helius_client {
                            info!("  Checking holder concentration...");
                            match helius.get_largest_holders(&token.mint, 10).await {
                                Ok(holders) => {
                                    if !holders.is_empty() {
                                        let total_supply: f64 = holders.iter().map(|h| h.amount).sum();
                                        let top_holder_pct = if total_supply > 0.0 {
                                            holders[0].amount / total_supply
                                        } else {
                                            0.0
                                        };

                                        if top_holder_pct > 0.50 {
                                            warn!(
                                                "  BLOCKED: Top holder owns {:.1}% (>{:.0}% threshold)",
                                                top_holder_pct * 100.0, 50.0
                                            );
                                            seen_tokens.insert(token.mint.clone());
                                            continue;
                                        }
                                        info!("  Holder check PASSED: top holder {:.1}%", top_holder_pct * 100.0);
                                    }
                                }
                                Err(e) => {
                                    warn!("  Failed to fetch holders: {} - proceeding anyway", e);
                                }
                            }
                        }

                        // Mark as seen
                        seen_tokens.insert(token.mint.clone());

                        // Execute buy if auto_buy enabled
                        if auto_buy {
                            if dry_run {
                                info!("  DRY-RUN: Would buy {:.4} SOL of {}", buy_amount, token.symbol);
                            } else if let Some((ref trader, ref keypair, ref rpc_client)) = trader {
                                info!("  BUYING {:.4} SOL of {}...", buy_amount, token.symbol);

                                let slippage = config.trading.slippage_bps as f64 / 100.0;
                                let priority_fee = config.trading.priority_fee_lamports as f64 / 1e9;

                                match trader.buy_local(&token.mint, buy_amount, slippage, priority_fee, keypair, rpc_client).await {
                                    Ok(sig) => {
                                        info!("  BUY SUCCESS: {}", sig);

                                        // Record position
                                        let position = crate::position::manager::Position {
                                            mint: token.mint.clone(),
                                            name: token.name.clone(),
                                            symbol: token.symbol.clone(),
                                            bonding_curve: String::new(), // Not available from DexScreener
                                            token_amount: (buy_amount / token.price_native) as u64,
                                            entry_price: token.price_native,
                                            total_cost_sol: buy_amount,
                                            entry_time: chrono::Utc::now(),
                                            entry_signature: sig.clone(),
                                            entry_type: crate::position::manager::EntryType::Momentum, // Hot scan entries
                                            quick_profit_taken: false,
                                            current_price: token.price_native,
                                            peak_price: token.price_native,
                                        };
                                        if let Err(e) = position_manager.open_position(position).await {
                                            error!("  Failed to record position: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("  BUY FAILED: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Scan failed: {}", e);
            }
        }

        if !watch {
            break;
        }

        info!("\nWaiting {} seconds until next scan...", interval);
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }

    Ok(())
}
'@

$content = $content + $hotScanFunc
Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Added hot_scan implementation"
