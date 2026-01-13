//! Execution Feedback Tracker
//!
//! Track fill quality to adjust confidence and detect adverse conditions.
//! Records slippage, latency, and fill rates.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use super::delta_tracker::RollingWindow;
use super::types::ExecutionRecord;

/// Execution quality metrics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionQuality {
    pub recent_avg_slippage: f64,
    pub recent_avg_latency_ms: u64,
    pub recent_fill_rate: f64,
    pub confidence_adjustment: f64,
    pub should_reduce_size: bool,
    pub should_pause_trading: bool,
}

/// Configuration for execution feedback
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionFeedbackConfig {
    pub enabled: bool,
    pub track_last_n: usize,
    pub slippage_penalty_threshold_pct: f64,
    pub fill_rate_penalty_threshold: f64,
    pub pause_on_severe_slippage: bool,
}

impl Default for ExecutionFeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            track_last_n: 50,
            slippage_penalty_threshold_pct: 5.0,
            fill_rate_penalty_threshold: 0.8,
            pause_on_severe_slippage: true,
        }
    }
}

/// Execution Feedback Tracker
pub struct ExecutionFeedback {
    config: ExecutionFeedbackConfig,
    executions: VecDeque<ExecutionRecord>,
    avg_slippage_pct: RollingWindow,
    avg_latency_ms: RollingWindow,
    fill_rate: RollingWindow,
}

impl ExecutionFeedback {
    /// Create a new execution feedback tracker
    pub fn new(config: ExecutionFeedbackConfig) -> Self {
        Self {
            config,
            executions: VecDeque::new(),
            avg_slippage_pct: RollingWindow::new(std::time::Duration::from_secs(3600)),
            avg_latency_ms: RollingWindow::new(std::time::Duration::from_secs(3600)),
            fill_rate: RollingWindow::new(std::time::Duration::from_secs(3600)),
        }
    }

    /// Record an execution
    pub fn record(&mut self, record: ExecutionRecord) {
        if !self.config.enabled {
            return;
        }

        // Add to rolling windows
        self.avg_slippage_pct.add(record.slippage_pct);
        self.avg_latency_ms.add(record.latency_ms as f64);
        self.fill_rate.add(if record.success { 1.0 } else { 0.0 });

        // Add to history
        self.executions.push_back(record);

        // Maintain size limit
        while self.executions.len() > self.config.track_last_n {
            self.executions.pop_front();
        }
    }

    /// Record a successful buy
    pub fn record_buy(
        &mut self,
        mint: &str,
        size_sol: f64,
        expected_price: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        let slippage = if expected_price > 0.0 {
            ((actual_price - expected_price) / expected_price) * 100.0
        } else {
            0.0
        };

        self.record(ExecutionRecord {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            side: super::types::Side::Buy,
            requested_size_sol: size_sol,
            filled_size_sol: size_sol,
            expected_price,
            actual_price,
            slippage_pct: slippage,
            latency_ms,
            success: true,
            failure_reason: None,
            tx_signature: Some(tx_sig.to_string()),
        });
    }

    /// Record a successful sell
    pub fn record_sell(
        &mut self,
        mint: &str,
        size_sol: f64,
        expected_price: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        let slippage = if expected_price > 0.0 {
            ((expected_price - actual_price) / expected_price) * 100.0
        } else {
            0.0
        };

        self.record(ExecutionRecord {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            side: super::types::Side::Sell,
            requested_size_sol: size_sol,
            filled_size_sol: size_sol,
            expected_price,
            actual_price,
            slippage_pct: slippage,
            latency_ms,
            success: true,
            failure_reason: None,
            tx_signature: Some(tx_sig.to_string()),
        });
    }

    /// Record a failed execution
    pub fn record_failure(
        &mut self,
        mint: &str,
        side: super::types::Side,
        size_sol: f64,
        latency_ms: u64,
        reason: &str,
    ) {
        self.record(ExecutionRecord {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            side,
            requested_size_sol: size_sol,
            filled_size_sol: 0.0,
            expected_price: 0.0,
            actual_price: 0.0,
            slippage_pct: 0.0,
            latency_ms,
            success: false,
            failure_reason: Some(reason.to_string()),
            tx_signature: None,
        });
    }

    /// Get current execution quality
    pub fn get_quality(&self) -> ExecutionQuality {
        let avg_slippage = self.avg_slippage_pct.average();
        let avg_latency = self.avg_latency_ms.average() as u64;
        let fill_rate = if self.fill_rate.count() > 0 {
            self.fill_rate.average()
        } else {
            1.0
        };

        // Calculate confidence adjustment based on slippage
        let slippage_adj: f64 = if avg_slippage > 10.0 {
            -0.3 // Severe slippage
        } else if avg_slippage > self.config.slippage_penalty_threshold_pct {
            -0.15
        } else if avg_slippage > 2.0 {
            -0.05
        } else {
            0.0
        };

        // Adjust for fill rate
        let fill_adj: f64 = if fill_rate < 0.5 {
            -0.2
        } else if fill_rate < self.config.fill_rate_penalty_threshold {
            -0.1
        } else {
            0.0
        };

        let confidence_adjustment = (slippage_adj + fill_adj).max(-0.3);

        // Determine if we should reduce size or pause
        let should_reduce = avg_slippage > self.config.slippage_penalty_threshold_pct;
        let should_pause =
            self.config.pause_on_severe_slippage && (fill_rate < 0.3 || avg_slippage > 15.0);

        ExecutionQuality {
            recent_avg_slippage: avg_slippage,
            recent_avg_latency_ms: avg_latency,
            recent_fill_rate: fill_rate,
            confidence_adjustment,
            should_reduce_size: should_reduce,
            should_pause_trading: should_pause,
        }
    }

    /// Get size reduction factor based on recent execution quality
    pub fn get_size_factor(&self) -> f64 {
        let quality = self.get_quality();

        if quality.should_pause_trading {
            return 0.0;
        }

        if quality.should_reduce_size {
            return 0.5;
        }

        // Gradual reduction based on slippage
        if quality.recent_avg_slippage > 10.0 {
            0.3
        } else if quality.recent_avg_slippage > 5.0 {
            0.6
        } else if quality.recent_avg_slippage > 2.0 {
            0.8
        } else {
            1.0
        }
    }

    /// Get recent execution history
    pub fn recent_executions(&self) -> &VecDeque<ExecutionRecord> {
        &self.executions
    }

    /// Get execution count
    pub fn execution_count(&self) -> usize {
        self.executions.len()
    }

    /// Get success rate
    pub fn success_rate(&self) -> f64 {
        if self.executions.is_empty() {
            return 1.0;
        }

        let success = self.executions.iter().filter(|e| e.success).count();
        success as f64 / self.executions.len() as f64
    }

    /// Get average slippage for successful executions
    pub fn avg_slippage(&self) -> f64 {
        let successful: Vec<_> = self.executions.iter().filter(|e| e.success).collect();

        if successful.is_empty() {
            return 0.0;
        }

        successful.iter().map(|e| e.slippage_pct).sum::<f64>() / successful.len() as f64
    }

    /// Clear execution history
    pub fn clear(&mut self) {
        self.executions.clear();
    }
}

impl Default for ExecutionFeedback {
    fn default() -> Self {
        Self::new(ExecutionFeedbackConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_execution() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_buy("mint", 0.1, 0.001, 0.00105, 100, "sig1");

        assert_eq!(feedback.execution_count(), 1);
        assert!((feedback.avg_slippage() - 5.0).abs() < 0.1);
    }

    #[test]
    fn test_slippage_calculation() {
        let mut feedback = ExecutionFeedback::default();

        // 10% slippage
        feedback.record_buy("mint", 0.1, 0.001, 0.0011, 100, "sig1");

        let quality = feedback.get_quality();
        assert!(quality.recent_avg_slippage > 9.0);
        assert!(quality.confidence_adjustment < 0.0);
    }

    #[test]
    fn test_fill_rate() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_buy("mint", 0.1, 0.001, 0.001, 100, "sig1");
        feedback.record_buy("mint", 0.1, 0.001, 0.001, 100, "sig2");
        feedback.record_failure("mint", super::super::types::Side::Buy, 0.1, 100, "Failed");

        assert!((feedback.success_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_size_factor() {
        let mut feedback = ExecutionFeedback::default();

        // Good execution -> full size
        feedback.record_buy("mint", 0.1, 0.001, 0.001, 100, "sig1");
        assert!((feedback.get_size_factor() - 1.0).abs() < 0.1);

        // High slippage -> reduced size
        feedback.record_buy("mint", 0.1, 0.001, 0.0013, 100, "sig2"); // 30% slippage

        let factor = feedback.get_size_factor();
        // Average slippage is now 15%, triggers should_reduce_size (returns 0.5)
        // Or with avg > 10%, returns 0.3 via gradual reduction path
        assert!(factor <= 0.5, "Expected factor <= 0.5, got {}", factor);
    }

    #[test]
    fn test_should_pause() {
        let mut feedback = ExecutionFeedback::default();

        // Record many failures
        for _ in 0..10 {
            feedback.record_failure("mint", super::super::types::Side::Buy, 0.1, 100, "Failed");
        }

        let quality = feedback.get_quality();
        // Fill rate should be 0
        assert!(quality.should_pause_trading);
    }

    #[test]
    fn test_confidence_adjustment() {
        let mut feedback = ExecutionFeedback::default();

        // Perfect execution
        feedback.record_buy("mint", 0.1, 0.001, 0.001, 50, "sig1");
        let quality = feedback.get_quality();
        assert!(quality.confidence_adjustment >= -0.05);

        // Bad execution
        let mut feedback2 = ExecutionFeedback::default();
        feedback2.record_buy("mint", 0.1, 0.001, 0.00115, 500, "sig1"); // 15% slippage
        let quality2 = feedback2.get_quality();
        assert!(quality2.confidence_adjustment < -0.1);
    }
}
