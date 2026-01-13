//! Auto-sell logic for take-profit and stop-loss
//!
//! WARNING: TP/SL is best-effort, not guaranteed. At 1-second polling,
//! fast rugs can gap through your stop-loss before detection. This is
//! unavoidable without sub-second polling (expensive) or on-chain
//! stop-loss mechanisms (not available on pump.fun).

use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::AutoSellConfig;
use crate::error::Result;
use crate::position::manager::{Position, PositionManager};
use crate::position::price_feed::PriceUpdate;

/// Auto-sell trigger type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TriggerType {
    /// Take profit triggered
    TakeProfit,
    /// Stop loss triggered
    StopLoss,
}

/// Auto-sell event
#[derive(Debug, Clone)]
pub struct AutoSellEvent {
    /// Token mint
    pub mint: Pubkey,
    /// Trigger type
    pub trigger: TriggerType,
    /// Entry price
    pub entry_price: f64,
    /// Current price
    pub current_price: f64,
    /// P&L percentage
    pub pnl_pct: f64,
    /// Amount to sell (may be partial for take-profit)
    pub sell_amount: u64,
    /// Total position size
    pub total_amount: u64,
}

/// Auto-seller that monitors positions and triggers sells
pub struct AutoSeller {
    config: AutoSellConfig,
    position_manager: Arc<PositionManager>,
}

impl AutoSeller {
    pub fn new(config: AutoSellConfig, position_manager: Arc<PositionManager>) -> Self {
        Self {
            config,
            position_manager,
        }
    }

    /// Start the auto-sell monitor
    pub async fn start(
        &self,
        mut price_rx: mpsc::Receiver<PriceUpdate>,
        sell_tx: mpsc::Sender<AutoSellEvent>,
    ) -> Result<()> {
        if !self.config.enabled {
            info!("Auto-sell disabled");
            return Ok(());
        }

        info!(
            "Auto-sell enabled: TP={}%, SL={}%",
            self.config.take_profit_pct, self.config.stop_loss_pct
        );

        let config = self.config.clone();
        let position_manager = self.position_manager.clone();

        tokio::spawn(async move {
            while let Some(update) = price_rx.recv().await {
                // Get position for this token
                let position = match position_manager
                    .get_position(&update.mint.to_string())
                    .await
                {
                    Some(p) => p,
                    None => continue, // No position, skip
                };

                // Update price in position manager
                position_manager
                    .update_price(&update.mint.to_string(), update.price)
                    .await;

                // Check for triggers
                if let Some(event) = Self::check_triggers(&config, &position, update.price) {
                    info!(
                        "Auto-sell triggered for {}: {:?} at {}% P&L",
                        update.mint, event.trigger, event.pnl_pct
                    );

                    if sell_tx.send(event).await.is_err() {
                        error!("Failed to send auto-sell event");
                        break;
                    }
                }
            }

            info!("Auto-sell monitor stopped");
        });

        Ok(())
    }

    /// Check if any triggers should fire
    fn check_triggers(
        config: &AutoSellConfig,
        position: &Position,
        current_price: f64,
    ) -> Option<AutoSellEvent> {
        let entry_price = position.entry_price;
        let pnl_pct = ((current_price - entry_price) / entry_price) * 100.0;

        // Check take-profit
        if pnl_pct >= config.take_profit_pct {
            let sell_amount = if config.partial_take_profit {
                // Sell half on first TP
                position.token_amount / 2
            } else {
                position.token_amount
            };

            return Some(AutoSellEvent {
                mint: Pubkey::default(), // Will be filled by caller
                trigger: TriggerType::TakeProfit,
                entry_price,
                current_price,
                pnl_pct,
                sell_amount,
                total_amount: position.token_amount,
            });
        }

        // Check stop-loss
        if pnl_pct <= -config.stop_loss_pct {
            return Some(AutoSellEvent {
                mint: Pubkey::default(),
                trigger: TriggerType::StopLoss,
                entry_price,
                current_price,
                pnl_pct,
                sell_amount: position.token_amount, // Always sell all on SL
                total_amount: position.token_amount,
            });
        }

        None
    }

    /// Manually check a position for triggers
    pub async fn check_position(&self, mint: &str) -> Option<AutoSellEvent> {
        let position = self.position_manager.get_position(mint).await?;

        Self::check_triggers(&self.config, &position, position.current_price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AutoSellConfig {
        AutoSellConfig {
            enabled: true,
            take_profit_pct: 50.0,
            stop_loss_pct: 30.0,
            partial_take_profit: false,
            price_poll_interval_ms: 1000,
        }
    }

    fn test_position(entry_price: f64, current_price: f64) -> Position {
        Position {
            mint: "test".to_string(),
            name: "Test".to_string(),
            symbol: "TEST".to_string(),
            bonding_curve: "curve".to_string(),
            token_amount: 1_000_000,
            entry_price,
            total_cost_sol: 0.01,
            entry_time: chrono::Utc::now(),
            entry_signature: "sig".to_string(),
            current_price,
        }
    }

    #[test]
    fn test_take_profit_trigger() {
        let config = test_config();
        let position = test_position(0.0001, 0.00016); // +60%

        let event = AutoSeller::check_triggers(&config, &position, 0.00016);

        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.trigger, TriggerType::TakeProfit);
        assert!(event.pnl_pct >= 50.0);
    }

    #[test]
    fn test_stop_loss_trigger() {
        let config = test_config();
        let position = test_position(0.0001, 0.00006); // -40%

        let event = AutoSeller::check_triggers(&config, &position, 0.00006);

        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.trigger, TriggerType::StopLoss);
        assert!(event.pnl_pct <= -30.0);
    }

    #[test]
    fn test_no_trigger() {
        let config = test_config();
        let position = test_position(0.0001, 0.00012); // +20%

        let event = AutoSeller::check_triggers(&config, &position, 0.00012);

        assert!(event.is_none());
    }

    #[test]
    fn test_partial_take_profit() {
        let mut config = test_config();
        config.partial_take_profit = true;

        let position = test_position(0.0001, 0.00016); // +60%
        let event = AutoSeller::check_triggers(&config, &position, 0.00016).unwrap();

        // Should sell half
        assert_eq!(event.sell_amount, 500_000);
    }
}
