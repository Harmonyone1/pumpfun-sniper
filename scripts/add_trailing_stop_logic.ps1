$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Add trailing stop logic between quick profit and take profit checks
$oldText = @'
                            continue; // Skip full TP check this cycle
                        }

                        // Check take profit (using entry type's target)
                        if pnl_pct >= tp_pct {
'@

$newText = @'
                            continue; // Skip full TP check this cycle
                        }

                        // TRAILING STOP: Lock in profits when price drops from peak
                        if price_feed_config.auto_sell.trailing_stop_enabled {
                            let trailing_activation = price_feed_config.auto_sell.trailing_stop_activation_pct;
                            let trailing_distance = price_feed_config.auto_sell.trailing_stop_distance_pct;

                            // Only check trailing stop if we've reached activation threshold
                            if pnl_pct >= trailing_activation || position.peak_price > position.entry_price * (1.0 + trailing_activation / 100.0) {
                                // Calculate how much price has dropped from peak
                                let peak_pnl_pct = if position.peak_price > 0.0 && position.entry_price > 0.0 {
                                    ((position.peak_price - position.entry_price) / position.entry_price) * 100.0
                                } else {
                                    0.0
                                };
                                let drop_from_peak_pct = if position.peak_price > 0.0 {
                                    ((position.peak_price - position.current_price) / position.peak_price) * 100.0
                                } else {
                                    0.0
                                };

                                // Trigger trailing stop if dropped more than threshold from peak
                                if drop_from_peak_pct >= trailing_distance && position.peak_price > position.entry_price {
                                    warn!(
                                        "TRAILING STOP triggered for {} - peaked at {:.1}% gain, now {:.1}% (dropped {:.1}% from peak)",
                                        position.mint, peak_pnl_pct, pnl_pct, drop_from_peak_pct
                                    );
                                    if let Some(ref trader) = price_feed_trader {
                                        let slippage = price_feed_config.trading.slippage_bps / 100;
                                        let priority_fee = price_feed_config.trading.priority_fee_lamports as f64 / 1e9;
                                        let sell_result = if price_feed_use_local {
                                            trader.sell_local(&position.mint, "100%", slippage, priority_fee, &price_feed_keypair, &price_feed_rpc).await
                                        } else {
                                            trader.sell(&position.mint, "100%", slippage, priority_fee).await
                                        };
                                        match sell_result {
                                            Ok(sig) => {
                                                info!("Trailing stop sell executed: {} (locked in {:.1}% profit)", sig, pnl_pct);
                                                if let Err(e) = price_feed_positions.close_position(
                                                    &position.mint,
                                                    position.token_amount,
                                                    0.0
                                                ).await {
                                                    error!("Failed to close position record: {}", e);
                                                }
                                            }
                                            Err(e) => error!("Trailing stop sell failed: {}", e),
                                        }
                                    }
                                    continue; // Skip other checks, we're exiting
                                }
                            }
                        }

                        // Check take profit (using entry type's target)
                        if pnl_pct >= tp_pct {
'@

$content = $content.Replace($oldText, $newText)

Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Added trailing stop logic to price feed loop"
