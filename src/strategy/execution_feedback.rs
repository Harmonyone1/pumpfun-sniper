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
    /// None when there are no observations of that kind (never a fake 0/perfect).
    pub recent_avg_slippage: Option<f64>,
    pub recent_avg_latency_ms: Option<u64>,
    pub recent_fill_rate: Option<f64>,
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

        // Add to rolling windows. Unknown slippage is NOT a 0% sample — only add
        // it when a real priced slippage exists. Latency and fill outcome are
        // always real observations.
        if let Some(slippage_pct) = record.slippage_pct {
            self.avg_slippage_pct.add(slippage_pct);
        }
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
            Some(((actual_price - expected_price) / expected_price) * 100.0)
        } else {
            None
        };

        self.record(ExecutionRecord {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            side: super::types::Side::Buy,
            requested_size_sol: size_sol,
            filled_size_sol: size_sol,
            expected_price: Some(expected_price),
            actual_price: Some(actual_price),
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
            Some(((expected_price - actual_price) / expected_price) * 100.0)
        } else {
            None
        };

        self.record(ExecutionRecord {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            side: super::types::Side::Sell,
            requested_size_sol: size_sol,
            filled_size_sol: size_sol,
            expected_price: Some(expected_price),
            actual_price: Some(actual_price),
            slippage_pct: slippage,
            latency_ms,
            success: true,
            failure_reason: None,
            tx_signature: Some(tx_sig.to_string()),
        });
    }

    /// Record a verified successful execution when the real fill price is UNKNOWN.
    /// Records success + latency + fill-rate but NO slippage sample.
    pub fn record_verified_success_unpriced(
        &mut self,
        mint: &str,
        side: super::types::Side,
        size_sol: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        self.record(ExecutionRecord::success_unpriced(
            mint.to_string(),
            side,
            size_sol,
            size_sol,
            latency_ms,
            tx_sig.to_string(),
        ));
    }

    /// Record a reconciled successful execution whose actual fill price is KNOWN
    /// but whose reference/expected price is not. Records success + latency +
    /// fill-rate and the actual price, but NO slippage sample.
    pub fn record_reconciled_success(
        &mut self,
        mint: &str,
        side: super::types::Side,
        requested_size_sol: f64,
        filled_size_sol: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        let rec = ExecutionRecord::success_reconciled_unquoted(
            mint.to_string(),
            side,
            requested_size_sol,
            filled_size_sol,
            actual_price,
            latency_ms,
            tx_sig.to_string(),
        );
        self.record(rec);
    }

    /// Record a reconciled successful execution that also had a real same-venue
    /// pre-send executable QUOTE. Unlike `record_reconciled_success`, this DOES
    /// add a drift sample (quote-to-fill execution drift, spec Section 26.4) in
    /// addition to fill-rate + latency. `expected_price` must be finite > 0.
    pub fn record_reconciled_quoted_success(
        &mut self,
        mint: &str,
        side: super::types::Side,
        requested_size_sol: f64,
        filled_size_sol: f64,
        expected_price: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        let rec = ExecutionRecord::success_reconciled_quoted(
            mint.to_string(),
            side,
            requested_size_sol,
            filled_size_sol,
            expected_price,
            actual_price,
            latency_ms,
            tx_sig.to_string(),
        );
        self.record(rec);
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
            expected_price: None,
            actual_price: None,
            slippage_pct: None,
            latency_ms,
            success: false,
            failure_reason: Some(reason.to_string()),
            tx_signature: None,
        });
    }

    /// Get current execution quality. Unknown observations stay None and never
    /// incur a penalty OR a reward.
    pub fn get_quality(&self) -> ExecutionQuality {
        let avg_slippage = if self.avg_slippage_pct.count() > 0 {
            Some(self.avg_slippage_pct.average())
        } else {
            None
        };
        let avg_latency = if self.avg_latency_ms.count() > 0 {
            Some(self.avg_latency_ms.average() as u64)
        } else {
            None
        };
        let fill_rate = if self.fill_rate.count() > 0 {
            Some(self.fill_rate.average())
        } else {
            None
        };

        // Confidence penalty only on KNOWN slippage.
        let slippage_adj: f64 = match avg_slippage {
            Some(s) if s > 10.0 => -0.3, // Severe slippage
            Some(s) if s > self.config.slippage_penalty_threshold_pct => -0.15,
            Some(s) if s > 2.0 => -0.05,
            _ => 0.0,
        };

        // Confidence penalty only on KNOWN fill rate.
        let fill_adj: f64 = match fill_rate {
            Some(f) if f < 0.5 => -0.2,
            Some(f) if f < self.config.fill_rate_penalty_threshold => -0.1,
            _ => 0.0,
        };

        let confidence_adjustment = (slippage_adj + fill_adj).max(-0.3);

        // Reduce/pause only react to KNOWN severe slippage / fill rate.
        let should_reduce =
            matches!(avg_slippage, Some(s) if s > self.config.slippage_penalty_threshold_pct);
        let should_pause = self.config.pause_on_severe_slippage
            && (matches!(fill_rate, Some(f) if f < 0.3)
                || matches!(avg_slippage, Some(s) if s > 15.0));

        ExecutionQuality {
            recent_avg_slippage: avg_slippage,
            recent_avg_latency_ms: avg_latency,
            recent_fill_rate: fill_rate,
            confidence_adjustment,
            should_reduce_size: should_reduce,
            should_pause_trading: should_pause,
        }
    }

    /// Get size reduction factor based on recent execution quality.
    /// Returns 1.0 when there is no negative execution evidence (incl. unknown slippage).
    pub fn get_size_factor(&self) -> f64 {
        let quality = self.get_quality();

        if quality.should_pause_trading {
            return 0.0;
        }

        if quality.should_reduce_size {
            return 0.5;
        }

        // Gradual reduction based on KNOWN slippage; unknown => full size.
        match quality.recent_avg_slippage {
            Some(s) if s > 10.0 => 0.3,
            Some(s) if s > 5.0 => 0.6,
            Some(s) if s > 2.0 => 0.8,
            _ => 1.0,
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

    /// Get success rate. None when there are no records.
    pub fn success_rate(&self) -> Option<f64> {
        if self.executions.is_empty() {
            return None;
        }

        let success = self.executions.iter().filter(|e| e.success).count();
        Some(success as f64 / self.executions.len() as f64)
    }

    /// Get average slippage over successful PRICED executions only.
    /// None when there are no priced successful records.
    pub fn avg_slippage(&self) -> Option<f64> {
        let samples: Vec<f64> = self
            .executions
            .iter()
            .filter(|e| e.success)
            .filter_map(|e| e.slippage_pct)
            .collect();

        if samples.is_empty() {
            return None;
        }

        Some(samples.iter().sum::<f64>() / samples.len() as f64)
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
        assert!((feedback.avg_slippage().unwrap() - 5.0).abs() < 0.1);
    }

    #[test]
    fn test_slippage_calculation() {
        let mut feedback = ExecutionFeedback::default();

        // 10% slippage
        feedback.record_buy("mint", 0.1, 0.001, 0.0011, 100, "sig1");

        let quality = feedback.get_quality();
        assert!(quality.recent_avg_slippage.unwrap() > 9.0);
        assert!(quality.confidence_adjustment < 0.0);
    }

    #[test]
    fn test_fill_rate() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_buy("mint", 0.1, 0.001, 0.001, 100, "sig1");
        feedback.record_buy("mint", 0.1, 0.001, 0.001, 100, "sig2");
        feedback.record_failure("mint", super::super::types::Side::Buy, 0.1, 100, "Failed");

        assert!((feedback.success_rate().unwrap() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_unpriced_success_does_not_create_zero_slippage_sample() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_verified_success_unpriced(
            "mint",
            super::super::types::Side::Buy,
            0.1,
            120,
            "sig1",
        );

        assert_eq!(feedback.execution_count(), 1);

        let quality = feedback.get_quality();
        assert_eq!(quality.recent_fill_rate, Some(1.0));
        assert!(quality.recent_avg_latency_ms.is_some());
        assert_eq!(quality.recent_avg_slippage, None);
        assert_eq!(feedback.avg_slippage(), None);
        assert!((feedback.get_size_factor() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reconciled_success_records_actual_price_without_slippage() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_reconciled_success(
            "mint",
            super::super::types::Side::Buy,
            0.1,
            0.09,
            0.00123,
            150,
            "sig1",
        );

        assert_eq!(feedback.execution_count(), 1);

        let rec = feedback.recent_executions().front().unwrap();
        assert!(rec.success);
        assert_eq!(rec.expected_price, None);
        assert_eq!(rec.actual_price, Some(0.00123));
        assert_eq!(rec.slippage_pct, None);
        assert!((rec.requested_size_sol - 0.1).abs() < f64::EPSILON);
        assert!((rec.filled_size_sol - 0.09).abs() < f64::EPSILON);

        let quality = feedback.get_quality();
        assert_eq!(quality.recent_fill_rate, Some(1.0));
        assert_eq!(quality.recent_avg_slippage, None);
        assert_eq!(feedback.avg_slippage(), None);
    }

    #[test]
    fn test_reconciled_quoted_success_records_drift_sample() {
        let mut feedback = ExecutionFeedback::default();

        // Buy fill 3% worse than quote => +3% drift sample.
        feedback.record_reconciled_quoted_success(
            "mint",
            super::super::types::Side::Buy,
            0.1,
            0.1,
            100.0,
            103.0,
            120,
            "sig1",
        );

        assert_eq!(feedback.execution_count(), 1);
        let rec = feedback.recent_executions().front().unwrap();
        assert_eq!(rec.expected_price, Some(100.0));
        assert_eq!(rec.actual_price, Some(103.0));
        let drift = rec.slippage_pct.expect("quoted record must have drift");
        assert!((drift - 3.0).abs() < 1e-9, "got {}", drift);

        // Unlike the unquoted variant, this DOES create a slippage/drift sample.
        assert!((feedback.avg_slippage().unwrap() - 3.0).abs() < 1e-9);
        let quality = feedback.get_quality();
        assert_eq!(quality.recent_fill_rate, Some(1.0));
        assert!(quality.recent_avg_slippage.is_some());
    }

    #[test]
    fn test_failure_does_not_create_slippage_sample() {
        let mut feedback = ExecutionFeedback::default();

        feedback.record_failure("mint", super::super::types::Side::Buy, 0.1, 100, "Failed");

        assert_eq!(feedback.success_rate(), Some(0.0));
        assert_eq!(feedback.get_quality().recent_fill_rate, Some(0.0));
        assert_eq!(feedback.avg_slippage(), None);
        assert_eq!(feedback.get_quality().recent_avg_slippage, None);
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
