//! Portfolio Risk Governor
//!
//! Global capital control to prevent ruin from correlated losses.
//! Enforces max concurrent positions, exposure limits, and circuit breakers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::types::Position;
use super::delta_tracker::RollingWindow;

/// Portfolio risk blocks - reasons why new positions are blocked
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortfolioBlock {
    /// Maximum number of concurrent positions reached
    MaxPositionsReached { current: usize, max: usize },
    /// Maximum total exposure reached
    MaxExposureReached { current_sol: f64, max_sol: f64 },
    /// Circuit breaker triggered due to hourly losses
    CircuitBreakerTripped { loss_sol: f64, limit_sol: f64 },
    /// Too many consecutive losses
    ConsecutiveLossLimit { count: u32, limit: u32 },
    /// Daily loss limit reached
    DailyLossLimitReached { loss_sol: f64, limit_sol: f64 },
    /// Individual position size too large
    PositionTooLarge { requested_sol: f64, max_sol: f64 },
    /// Paused due to adverse conditions
    TradingPaused { reason: String, resume_in_secs: u64 },
}

impl PortfolioBlock {
    /// Get human-readable description
    pub fn description(&self) -> String {
        match self {
            PortfolioBlock::MaxPositionsReached { current, max } => {
                format!("Max positions reached: {}/{}", current, max)
            }
            PortfolioBlock::MaxExposureReached { current_sol, max_sol } => {
                format!("Max exposure reached: {:.3}/{:.3} SOL", current_sol, max_sol)
            }
            PortfolioBlock::CircuitBreakerTripped { loss_sol, limit_sol } => {
                format!(
                    "Circuit breaker: hourly loss {:.3} exceeds limit {:.3} SOL",
                    loss_sol, limit_sol
                )
            }
            PortfolioBlock::ConsecutiveLossLimit { count, limit } => {
                format!("Consecutive losses: {}/{}", count, limit)
            }
            PortfolioBlock::DailyLossLimitReached { loss_sol, limit_sol } => {
                format!("Daily loss limit: {:.3}/{:.3} SOL", loss_sol, limit_sol)
            }
            PortfolioBlock::PositionTooLarge { requested_sol, max_sol } => {
                format!(
                    "Position too large: {:.3} SOL exceeds max {:.3} SOL",
                    requested_sol, max_sol
                )
            }
            PortfolioBlock::TradingPaused { reason, resume_in_secs } => {
                format!("Trading paused: {} (resume in {}s)", reason, resume_in_secs)
            }
        }
    }
}

/// Current portfolio state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortfolioState {
    pub open_position_count: usize,
    pub total_exposure_sol: f64,
    pub unrealized_pnl_sol: f64,
    pub hourly_realized_pnl_sol: f64,
    pub daily_realized_pnl_sol: f64,
    pub consecutive_losses: u32,
    pub can_open_new: bool,
    pub reason_if_blocked: Option<String>,
}

/// Configuration for portfolio risk management
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioRiskConfig {
    /// Maximum concurrent open positions
    pub max_concurrent_positions: usize,
    /// Maximum total capital at risk
    pub max_exposure_sol: f64,
    /// Maximum single position size
    pub max_per_token_sol: f64,
    /// Stop trading if hourly loss exceeds this
    pub hourly_loss_limit_sol: f64,
    /// Hard stop for the day
    pub daily_loss_limit_sol: f64,
    /// Pause after N consecutive losses
    pub consecutive_loss_limit: u32,
    /// Cooldown after circuit breaker (seconds)
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for PortfolioRiskConfig {
    fn default() -> Self {
        Self {
            max_concurrent_positions: 5,
            max_exposure_sol: 2.0,
            max_per_token_sol: 0.5,
            hourly_loss_limit_sol: 0.5,
            daily_loss_limit_sol: 1.0,
            consecutive_loss_limit: 5,
            circuit_breaker_cooldown_secs: 300, // 5 minutes
        }
    }
}

/// Portfolio Risk Governor
pub struct PortfolioRiskGovernor {
    config: PortfolioRiskConfig,
    /// Current open positions
    positions: HashMap<String, Position>,
    /// Rolling window for hourly PnL
    hourly_pnl: RollingWindow,
    /// Daily PnL accumulator
    daily_pnl: f64,
    /// Consecutive loss counter
    consecutive_losses: u32,
    /// Trading pause state
    paused_until: Option<std::time::Instant>,
    pause_reason: Option<String>,
    /// Day start timestamp for daily reset
    day_start: chrono::DateTime<chrono::Utc>,
}

impl PortfolioRiskGovernor {
    /// Create a new portfolio risk governor
    pub fn new(config: PortfolioRiskConfig) -> Self {
        Self {
            config,
            positions: HashMap::new(),
            hourly_pnl: RollingWindow::new(std::time::Duration::from_secs(3600)), // 1 hour
            daily_pnl: 0.0,
            consecutive_losses: 0,
            paused_until: None,
            pause_reason: None,
            day_start: chrono::Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc(),
        }
    }

    /// Check if a new position can be opened
    pub fn can_open_position(&self, size_sol: f64) -> Result<(), PortfolioBlock> {
        // Check if trading is paused
        if let Some(paused_until) = self.paused_until {
            if std::time::Instant::now() < paused_until {
                let remaining = paused_until.duration_since(std::time::Instant::now());
                return Err(PortfolioBlock::TradingPaused {
                    reason: self.pause_reason.clone().unwrap_or_default(),
                    resume_in_secs: remaining.as_secs(),
                });
            }
        }

        // Check position count
        if self.positions.len() >= self.config.max_concurrent_positions {
            return Err(PortfolioBlock::MaxPositionsReached {
                current: self.positions.len(),
                max: self.config.max_concurrent_positions,
            });
        }

        // Check individual position size
        if size_sol > self.config.max_per_token_sol {
            return Err(PortfolioBlock::PositionTooLarge {
                requested_sol: size_sol,
                max_sol: self.config.max_per_token_sol,
            });
        }

        // Check total exposure
        let current_exposure: f64 = self.positions.values().map(|p| p.size_sol).sum();
        if current_exposure + size_sol > self.config.max_exposure_sol {
            return Err(PortfolioBlock::MaxExposureReached {
                current_sol: current_exposure,
                max_sol: self.config.max_exposure_sol,
            });
        }

        // Check circuit breaker (hourly losses)
        let hourly_loss = -self.hourly_pnl.sum();
        if hourly_loss > self.config.hourly_loss_limit_sol {
            return Err(PortfolioBlock::CircuitBreakerTripped {
                loss_sol: hourly_loss,
                limit_sol: self.config.hourly_loss_limit_sol,
            });
        }

        // Check daily loss limit
        let daily_loss = -self.daily_pnl;
        if daily_loss > self.config.daily_loss_limit_sol {
            return Err(PortfolioBlock::DailyLossLimitReached {
                loss_sol: daily_loss,
                limit_sol: self.config.daily_loss_limit_sol,
            });
        }

        // Check consecutive losses
        if self.consecutive_losses >= self.config.consecutive_loss_limit {
            return Err(PortfolioBlock::ConsecutiveLossLimit {
                count: self.consecutive_losses,
                limit: self.config.consecutive_loss_limit,
            });
        }

        Ok(())
    }

    /// Register a new position
    pub fn open_position(&mut self, position: Position) {
        self.positions.insert(position.mint.clone(), position);
    }

    /// Close a position and record PnL
    pub fn close_position(&mut self, mint: &str, pnl_sol: f64) {
        self.positions.remove(mint);
        self.record_pnl(pnl_sol);
    }

    /// Record PnL from a closed position
    pub fn record_pnl(&mut self, pnl_sol: f64) {
        // Add to rolling hourly window
        self.hourly_pnl.add(pnl_sol);

        // Add to daily accumulator
        self.daily_pnl += pnl_sol;

        // Update consecutive loss counter
        if pnl_sol < 0.0 {
            self.consecutive_losses += 1;

            // Check if we need to pause trading
            if self.consecutive_losses >= self.config.consecutive_loss_limit {
                self.pause_trading(
                    format!("{} consecutive losses", self.consecutive_losses),
                    self.config.circuit_breaker_cooldown_secs,
                );
            }
        } else {
            self.consecutive_losses = 0;
        }

        // Check circuit breaker
        let hourly_loss = -self.hourly_pnl.sum();
        if hourly_loss > self.config.hourly_loss_limit_sol {
            self.pause_trading(
                format!("Hourly loss {:.3} SOL", hourly_loss),
                self.config.circuit_breaker_cooldown_secs,
            );
        }
    }

    /// Pause trading for a duration
    pub fn pause_trading(&mut self, reason: String, duration_secs: u64) {
        tracing::warn!("Pausing trading: {} ({}s)", reason, duration_secs);
        self.paused_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(duration_secs));
        self.pause_reason = Some(reason);
    }

    /// Resume trading
    pub fn resume_trading(&mut self) {
        self.paused_until = None;
        self.pause_reason = None;
        tracing::info!("Trading resumed");
    }

    /// Reset consecutive losses counter
    pub fn reset_consecutive_losses(&mut self) {
        self.consecutive_losses = 0;
    }

    /// Check and reset daily counters if new day
    pub fn check_daily_reset(&mut self) {
        let now = chrono::Utc::now();
        let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        if today_start > self.day_start {
            tracing::info!("Daily reset: previous day PnL was {:.3} SOL", self.daily_pnl);
            self.daily_pnl = 0.0;
            self.day_start = today_start;
            self.consecutive_losses = 0;

            // Resume trading if it was paused due to daily limit
            if self.pause_reason.as_ref().map_or(false, |r| r.contains("daily")) {
                self.resume_trading();
            }
        }
    }

    /// Get current portfolio state
    pub fn get_state(&self) -> PortfolioState {
        let current_exposure: f64 = self.positions.values().map(|p| p.size_sol).sum();

        // Calculate unrealized PnL (would need current prices)
        let unrealized_pnl = 0.0; // TODO: Calculate from current prices

        let can_open = self.can_open_position(self.config.max_per_token_sol);

        PortfolioState {
            open_position_count: self.positions.len(),
            total_exposure_sol: current_exposure,
            unrealized_pnl_sol: unrealized_pnl,
            hourly_realized_pnl_sol: self.hourly_pnl.sum(),
            daily_realized_pnl_sol: self.daily_pnl,
            consecutive_losses: self.consecutive_losses,
            can_open_new: can_open.is_ok(),
            reason_if_blocked: can_open.err().map(|b| b.description()),
        }
    }

    /// Get a specific position
    pub fn get_position(&self, mint: &str) -> Option<&Position> {
        self.positions.get(mint)
    }

    /// Get all positions
    pub fn get_positions(&self) -> &HashMap<String, Position> {
        &self.positions
    }

    /// Get remaining capacity in SOL
    pub fn remaining_capacity(&self) -> f64 {
        let current_exposure: f64 = self.positions.values().map(|p| p.size_sol).sum();
        (self.config.max_exposure_sol - current_exposure).max(0.0)
    }

    /// Get remaining position slots
    pub fn remaining_slots(&self) -> usize {
        self.config.max_concurrent_positions.saturating_sub(self.positions.len())
    }

    /// Adjust position size based on portfolio constraints
    pub fn adjust_position_size(&self, requested_sol: f64) -> f64 {
        let mut size = requested_sol;

        // Cap at max per token
        size = size.min(self.config.max_per_token_sol);

        // Cap at remaining capacity
        size = size.min(self.remaining_capacity());

        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{TradingStrategy, ExitStyle};

    fn make_position(mint: &str, size_sol: f64) -> Position {
        Position {
            mint: mint.to_string(),
            entry_price: 0.001,
            entry_time: chrono::Utc::now(),
            size_sol,
            tokens_held: 100_000,
            strategy: TradingStrategy::SnipeAndScalp,
            exit_style: ExitStyle::default(),
            highest_price: 0.001,
            lowest_price: 0.001,
            exit_levels_hit: vec![],
        }
    }

    #[test]
    fn test_can_open_position_success() {
        let governor = PortfolioRiskGovernor::new(PortfolioRiskConfig::default());
        assert!(governor.can_open_position(0.1).is_ok());
    }

    #[test]
    fn test_max_positions_block() {
        let config = PortfolioRiskConfig {
            max_concurrent_positions: 2,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        governor.open_position(make_position("mint1", 0.1));
        governor.open_position(make_position("mint2", 0.1));

        let result = governor.can_open_position(0.1);
        assert!(matches!(result, Err(PortfolioBlock::MaxPositionsReached { .. })));
    }

    #[test]
    fn test_max_exposure_block() {
        let config = PortfolioRiskConfig {
            max_exposure_sol: 0.5,
            max_per_token_sol: 0.3,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        governor.open_position(make_position("mint1", 0.3));

        let result = governor.can_open_position(0.3);
        assert!(matches!(result, Err(PortfolioBlock::MaxExposureReached { .. })));
    }

    #[test]
    fn test_position_too_large() {
        let config = PortfolioRiskConfig {
            max_per_token_sol: 0.2,
            ..Default::default()
        };
        let governor = PortfolioRiskGovernor::new(config);

        let result = governor.can_open_position(0.3);
        assert!(matches!(result, Err(PortfolioBlock::PositionTooLarge { .. })));
    }

    #[test]
    fn test_consecutive_loss_tracking() {
        let config = PortfolioRiskConfig {
            consecutive_loss_limit: 3,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        // Record losses
        governor.record_pnl(-0.01);
        assert_eq!(governor.consecutive_losses, 1);

        governor.record_pnl(-0.01);
        assert_eq!(governor.consecutive_losses, 2);

        // Win resets counter
        governor.record_pnl(0.05);
        assert_eq!(governor.consecutive_losses, 0);
    }

    #[test]
    fn test_consecutive_loss_block() {
        let config = PortfolioRiskConfig {
            consecutive_loss_limit: 3,
            circuit_breaker_cooldown_secs: 60,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        // Record 3 losses
        governor.record_pnl(-0.01);
        governor.record_pnl(-0.01);
        governor.record_pnl(-0.01);

        // Should be paused now
        let result = governor.can_open_position(0.1);
        assert!(result.is_err());
    }

    #[test]
    fn test_close_position() {
        let mut governor = PortfolioRiskGovernor::new(PortfolioRiskConfig::default());

        governor.open_position(make_position("mint1", 0.1));
        assert_eq!(governor.positions.len(), 1);

        governor.close_position("mint1", 0.05);
        assert_eq!(governor.positions.len(), 0);
        assert!(governor.daily_pnl > 0.0);
    }

    #[test]
    fn test_remaining_capacity() {
        let config = PortfolioRiskConfig {
            max_exposure_sol: 1.0,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        assert_eq!(governor.remaining_capacity(), 1.0);

        governor.open_position(make_position("mint1", 0.3));
        assert!((governor.remaining_capacity() - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_adjust_position_size() {
        let config = PortfolioRiskConfig {
            max_per_token_sol: 0.2,
            max_exposure_sol: 0.5,
            ..Default::default()
        };
        let mut governor = PortfolioRiskGovernor::new(config);

        // Should cap at max per token
        assert_eq!(governor.adjust_position_size(0.5), 0.2);

        // Open a position
        governor.open_position(make_position("mint1", 0.2));

        // Should cap at remaining capacity
        assert_eq!(governor.adjust_position_size(0.5), 0.2); // Still capped at max per token
    }

    #[test]
    fn test_get_state() {
        let mut governor = PortfolioRiskGovernor::new(PortfolioRiskConfig::default());

        governor.open_position(make_position("mint1", 0.1));
        governor.record_pnl(-0.02);

        let state = governor.get_state();
        assert_eq!(state.open_position_count, 1);
        assert_eq!(state.total_exposure_sol, 0.1);
        assert_eq!(state.consecutive_losses, 1);
        assert!(state.can_open_new);
    }
}
