$content = Get-Content 'D:\pumpfun\src\cli\commands.rs' -Raw

# Find the DATA-DRIVEN ENTRY section and add momentum validation
$oldCode = @'
                            // Quick liquidity check
                            if liquidity_sol < 0.05 {
                                info!("Token {} rejected: liquidity {:.4} SOL too low", trade.mint, liquidity_sol);
                                continue;
                            }

                            // Use configured buy amount for trade-based entries
                            let final_amount_sol = config.trading.buy_amount_sol;

                            info!(
                                "Trade signal: BUY {:.4} SOL of {} (liquidity: {:.4} SOL) - DATA-DRIVEN ENTRY",
                                final_amount_sol, trade.mint, liquidity_sol
                            );
'@

$newCode = @'
                            // Quick liquidity check
                            if liquidity_sol < 0.05 {
                                info!("Token {} rejected: liquidity {:.4} SOL too low", trade.mint, liquidity_sol);
                                continue;
                            }

                            // SURVIVOR MODE: Only follow whale trades if token passed momentum validation
                            // This prevents following whales into unvalidated tokens (no holder data, no observation window)
                            let momentum_status = momentum_validator.check_momentum(&trade.mint).await;
                            match momentum_status {
                                crate::filter::momentum::MomentumStatus::Ready { metrics: _ } => {
                                    info!(
                                        "DATA-DRIVEN ENTRY approved - {} passed momentum validation (holder data, observation window)",
                                        &trade.mint[..12]
                                    );
                                }
                                crate::filter::momentum::MomentumStatus::NotWatched => {
                                    debug!(
                                        "DATA-DRIVEN ENTRY skipped - {} not in momentum watchlist",
                                        &trade.mint[..12]
                                    );
                                    continue;
                                }
                                crate::filter::momentum::MomentumStatus::Observing { metrics, reason } => {
                                    info!(
                                        "DATA-DRIVEN ENTRY blocked - {} still observing: {} (survival: {:.0}%, holders: {})",
                                        &trade.mint[..12], reason, metrics.survival_ratio * 100.0,
                                        if metrics.holder_data_fetched { "ready" } else { "pending" }
                                    );
                                    continue;
                                }
                                crate::filter::momentum::MomentumStatus::Expired { metrics: _ } => {
                                    debug!(
                                        "DATA-DRIVEN ENTRY skipped - {} expired without passing validation",
                                        &trade.mint[..12]
                                    );
                                    continue;
                                }
                            }

                            // Use configured buy amount for trade-based entries
                            let final_amount_sol = config.trading.buy_amount_sol;

                            info!(
                                "Trade signal: BUY {:.4} SOL of {} (liquidity: {:.4} SOL) - VALIDATED DATA-DRIVEN ENTRY",
                                final_amount_sol, trade.mint, liquidity_sol
                            );
'@

$content = $content.Replace($oldCode, $newCode)
Set-Content 'D:\pumpfun\src\cli\commands.rs' $content -NoNewline
Write-Output "Fixed DATA-DRIVEN ENTRY to require momentum validation"
