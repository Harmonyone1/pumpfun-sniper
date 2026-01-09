//! Chain Health Monitor
//!
//! Solana congestion awareness to avoid trading in bad conditions.
//! Monitors slot times, transaction failures, and priority fees.

use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;

use super::delta_tracker::RollingWindow;
use super::types::{ChainAction, CongestionLevel};

/// Chain state snapshot
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainState {
    pub avg_slot_time_ms: u64,
    pub tx_failure_rate: f64,
    pub priority_fee_lamports: u64,
    pub congestion_level: CongestionLevel,
    pub recommended_action: ChainAction,
}

/// Configuration for chain health monitoring
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainHealthConfig {
    pub enabled: bool,
    pub sample_interval_secs: u64,
    pub pause_on_severe: bool,
    pub exit_only_on_critical: bool,
    pub congestion_size_factor: f64,
}

impl Default for ChainHealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_secs: 10,
            pause_on_severe: true,
            exit_only_on_critical: true,
            congestion_size_factor: 0.5,
        }
    }
}

/// Chain Health Monitor
pub struct ChainHealth {
    config: ChainHealthConfig,
    recent_slot_times: RollingWindow,
    recent_tx_failures: RollingWindow,
    recent_priority_fees: RollingWindow,
    our_tx_count: u32,
    our_tx_failures: u32,
}

impl ChainHealth {
    /// Create a new chain health monitor
    pub fn new(config: ChainHealthConfig) -> Self {
        Self {
            config,
            recent_slot_times: RollingWindow::new(std::time::Duration::from_secs(300)),
            recent_tx_failures: RollingWindow::new(std::time::Duration::from_secs(300)),
            recent_priority_fees: RollingWindow::new(std::time::Duration::from_secs(300)),
            our_tx_count: 0,
            our_tx_failures: 0,
        }
    }

    /// Sample chain health metrics from RPC
    pub async fn sample(&mut self, rpc: &RpcClient) {
        if !self.config.enabled {
            return;
        }

        // Get recent performance samples
        if let Ok(samples) = rpc.get_recent_performance_samples(Some(5)).await {
            if !samples.is_empty() {
                let avg_slot_time = samples.iter()
                    .filter(|s| s.num_slots > 0)
                    .map(|s| {
                        (s.sample_period_secs as f64 / s.num_slots as f64) * 1000.0
                    })
                    .sum::<f64>() / samples.len() as f64;
                self.recent_slot_times.add(avg_slot_time);
            }
        }

        // Get recent priority fees
        // Note: This may not work on all RPC endpoints
        if let Ok(fees) = rpc.get_recent_prioritization_fees(&[]).await {
            if !fees.is_empty() {
                let avg_fee = fees.iter()
                    .map(|f| f.prioritization_fee)
                    .sum::<u64>() / fees.len() as u64;
                self.recent_priority_fees.add(avg_fee as f64);
            }
        }
    }

    /// Record a transaction result (for tracking our own failure rate)
    pub fn record_tx(&mut self, success: bool) {
        self.our_tx_count += 1;
        if !success {
            self.our_tx_failures += 1;
        }

        // Update rolling failure rate
        self.recent_tx_failures.add(if success { 0.0 } else { 1.0 });
    }

    /// Get current chain state
    pub fn get_state(&self) -> ChainState {
        let avg_slot_time = if self.recent_slot_times.count() > 0 {
            self.recent_slot_times.average() as u64
        } else {
            400 // Default Solana slot time
        };

        let failure_rate = if self.recent_tx_failures.count() > 0 {
            self.recent_tx_failures.average()
        } else {
            0.0
        };

        let priority_fee = if self.recent_priority_fees.count() > 0 {
            self.recent_priority_fees.latest() as u64
        } else {
            1000 // Default 1000 lamports
        };

        let congestion_level = self.calculate_congestion(avg_slot_time, failure_rate);
        let recommended_action = self.get_action(congestion_level, priority_fee);

        ChainState {
            avg_slot_time_ms: avg_slot_time,
            tx_failure_rate: failure_rate,
            priority_fee_lamports: priority_fee,
            congestion_level,
            recommended_action,
        }
    }

    /// Calculate congestion level
    fn calculate_congestion(&self, avg_slot_time: u64, failure_rate: f64) -> CongestionLevel {
        // Critical: Very high failure rate or extremely slow slots
        if failure_rate > 0.5 || avg_slot_time > 800 {
            return CongestionLevel::Critical;
        }

        // Severe: High failure rate or very slow slots
        if failure_rate > 0.3 || avg_slot_time > 650 {
            return CongestionLevel::Severe;
        }

        // High: Moderate failure rate or slow slots
        if failure_rate > 0.15 || avg_slot_time > 550 {
            return CongestionLevel::High;
        }

        // Elevated: Some issues
        if failure_rate > 0.05 || avg_slot_time > 450 {
            return CongestionLevel::Elevated;
        }

        CongestionLevel::Normal
    }

    /// Get recommended action for congestion level
    fn get_action(&self, level: CongestionLevel, priority_fee: u64) -> ChainAction {
        match level {
            CongestionLevel::Normal => ChainAction::ProceedNormally,

            CongestionLevel::Elevated => {
                ChainAction::IncreasePriorityFee {
                    to_lamports: priority_fee.saturating_mul(2).max(5000),
                }
            }

            CongestionLevel::High => {
                ChainAction::ReducePositionSize {
                    factor: self.config.congestion_size_factor,
                }
            }

            CongestionLevel::Severe => {
                if self.config.pause_on_severe {
                    ChainAction::PauseNewEntries
                } else {
                    ChainAction::ReducePositionSize {
                        factor: self.config.congestion_size_factor * 0.5,
                    }
                }
            }

            CongestionLevel::Critical => {
                if self.config.exit_only_on_critical {
                    ChainAction::ExitOnlyMode
                } else {
                    ChainAction::PauseNewEntries
                }
            }
        }
    }

    /// Check if new entries should be blocked
    pub fn should_block_entries(&self) -> bool {
        let state = self.get_state();
        matches!(
            state.recommended_action,
            ChainAction::PauseNewEntries | ChainAction::ExitOnlyMode
        )
    }

    /// Get position size multiplier for current conditions
    pub fn get_size_multiplier(&self) -> f64 {
        match self.get_state().recommended_action {
            ChainAction::ProceedNormally => 1.0,
            ChainAction::IncreasePriorityFee { .. } => 0.9,
            ChainAction::ReducePositionSize { factor } => factor,
            ChainAction::PauseNewEntries | ChainAction::ExitOnlyMode => 0.0,
        }
    }

    /// Get recommended priority fee
    pub fn get_priority_fee(&self) -> u64 {
        let state = self.get_state();
        match state.recommended_action {
            ChainAction::IncreasePriorityFee { to_lamports } => to_lamports,
            _ => state.priority_fee_lamports.max(1000),
        }
    }

    /// Reset monitoring state
    pub fn reset(&mut self) {
        self.our_tx_count = 0;
        self.our_tx_failures = 0;
    }
}

impl Default for ChainHealth {
    fn default() -> Self {
        Self::new(ChainHealthConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_congestion_levels() {
        let health = ChainHealth::default();

        // Normal
        assert_eq!(
            health.calculate_congestion(400, 0.0),
            CongestionLevel::Normal
        );

        // Elevated
        assert_eq!(
            health.calculate_congestion(460, 0.0),
            CongestionLevel::Elevated
        );

        // High
        assert_eq!(
            health.calculate_congestion(560, 0.0),
            CongestionLevel::High
        );

        // Severe
        assert_eq!(
            health.calculate_congestion(660, 0.0),
            CongestionLevel::Severe
        );

        // Critical
        assert_eq!(
            health.calculate_congestion(850, 0.0),
            CongestionLevel::Critical
        );
    }

    #[test]
    fn test_failure_rate_affects_congestion() {
        let health = ChainHealth::default();

        // High failure rate -> Critical even with normal slot time
        assert_eq!(
            health.calculate_congestion(400, 0.6),
            CongestionLevel::Critical
        );

        // Moderate failure rate -> Severe
        assert_eq!(
            health.calculate_congestion(400, 0.35),
            CongestionLevel::Severe
        );
    }

    #[test]
    fn test_get_action() {
        let health = ChainHealth::default();

        assert!(matches!(
            health.get_action(CongestionLevel::Normal, 1000),
            ChainAction::ProceedNormally
        ));

        assert!(matches!(
            health.get_action(CongestionLevel::Elevated, 1000),
            ChainAction::IncreasePriorityFee { .. }
        ));

        assert!(matches!(
            health.get_action(CongestionLevel::High, 1000),
            ChainAction::ReducePositionSize { .. }
        ));

        assert!(matches!(
            health.get_action(CongestionLevel::Severe, 1000),
            ChainAction::PauseNewEntries
        ));

        assert!(matches!(
            health.get_action(CongestionLevel::Critical, 1000),
            ChainAction::ExitOnlyMode
        ));
    }

    #[test]
    fn test_size_multiplier() {
        let mut health = ChainHealth::default();

        // Normal -> 1.0
        // Force normal state
        for _ in 0..10 {
            health.recent_slot_times.add(400.0);
            health.recent_tx_failures.add(0.0);
        }
        assert!((health.get_size_multiplier() - 1.0).abs() < 0.1);
    }

    #[test]
    fn test_record_tx() {
        let mut health = ChainHealth::default();

        health.record_tx(true);
        health.record_tx(true);
        health.record_tx(false);

        assert_eq!(health.our_tx_count, 3);
        assert_eq!(health.our_tx_failures, 1);
    }

    #[test]
    fn test_should_block_entries() {
        let mut health = ChainHealth::default();

        // Add normal conditions
        for _ in 0..10 {
            health.recent_slot_times.add(400.0);
            health.recent_tx_failures.add(0.0);
        }
        assert!(!health.should_block_entries());

        // Add critical conditions
        for _ in 0..20 {
            health.recent_slot_times.add(900.0);
            health.recent_tx_failures.add(1.0);
        }
        assert!(health.should_block_entries());
    }
}
