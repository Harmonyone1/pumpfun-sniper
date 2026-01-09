//! Delta Tracker - Temporal Delta Tracking
//!
//! Track changes over time, not just snapshots.
//! Uses rolling windows for trend detection and momentum analysis.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::types::Trend;

/// Rolling window for time-series data
#[derive(Debug, Clone)]
pub struct RollingWindow {
    samples: VecDeque<(Instant, f64)>,
    window_duration: Duration,
    max_samples: usize,
}

impl RollingWindow {
    /// Create a new rolling window with specified duration
    pub fn new(window_duration: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            window_duration,
            max_samples: 1000, // Prevent unbounded growth
        }
    }

    /// Create with custom max samples
    pub fn with_max_samples(window_duration: Duration, max_samples: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            window_duration,
            max_samples,
        }
    }

    /// Add a new sample
    pub fn add(&mut self, value: f64) {
        let now = Instant::now();
        self.samples.push_back((now, value));

        // Remove old samples
        self.prune();

        // Cap at max samples
        while self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    /// Remove samples older than window duration
    fn prune(&mut self) {
        let cutoff = Instant::now() - self.window_duration;
        while let Some((time, _)) = self.samples.front() {
            if *time < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get the delta (latest - oldest) within the window
    pub fn delta(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }

        let oldest = self.samples.front().map(|(_, v)| *v).unwrap_or(0.0);
        let latest = self.samples.back().map(|(_, v)| *v).unwrap_or(0.0);
        latest - oldest
    }

    /// Get the sum of all samples
    pub fn sum(&self) -> f64 {
        self.samples.iter().map(|(_, v)| v).sum()
    }

    /// Get the average of all samples
    pub fn average(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.sum() / self.samples.len() as f64
    }

    /// Get the latest value
    pub fn latest(&self) -> f64 {
        self.samples.back().map(|(_, v)| *v).unwrap_or(0.0)
    }

    /// Get the oldest value still in window
    pub fn oldest(&self) -> f64 {
        self.samples.front().map(|(_, v)| *v).unwrap_or(0.0)
    }

    /// Get the minimum value in window
    pub fn min(&self) -> f64 {
        self.samples.iter().map(|(_, v)| *v).fold(f64::MAX, f64::min)
    }

    /// Get the maximum value in window
    pub fn max(&self) -> f64 {
        self.samples.iter().map(|(_, v)| *v).fold(f64::MIN, f64::max)
    }

    /// Calculate trend based on slope
    pub fn trend(&self, threshold: f64) -> Trend {
        let slope = self.slope();
        Trend::from_slope(slope, threshold)
    }

    /// Calculate slope (rate of change per second)
    pub fn slope(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }

        let (first_time, first_val) = self.samples.front().unwrap();
        let (last_time, last_val) = self.samples.back().unwrap();

        let duration = last_time.duration_since(*first_time).as_secs_f64();
        if duration < 0.001 {
            return 0.0;
        }

        (last_val - first_val) / duration
    }

    /// Calculate velocity (first derivative - rate of change)
    pub fn velocity(&self) -> f64 {
        self.slope()
    }

    /// Calculate acceleration (second derivative - change in rate)
    pub fn acceleration(&self) -> f64 {
        if self.samples.len() < 3 {
            return 0.0;
        }

        // Split window in half and compare slopes
        let mid = self.samples.len() / 2;

        // First half slope
        let first_half: Vec<_> = self.samples.iter().take(mid).collect();
        let first_slope = if first_half.len() >= 2 {
            let duration = first_half.last().unwrap().0.duration_since(first_half.first().unwrap().0).as_secs_f64();
            if duration > 0.001 {
                (first_half.last().unwrap().1 - first_half.first().unwrap().1) / duration
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Second half slope
        let second_half: Vec<_> = self.samples.iter().skip(mid).collect();
        let second_slope = if second_half.len() >= 2 {
            let duration = second_half.last().unwrap().0.duration_since(second_half.first().unwrap().0).as_secs_f64();
            if duration > 0.001 {
                (second_half.last().unwrap().1 - second_half.first().unwrap().1) / duration
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Total duration
        let total_duration = self.samples.back().unwrap().0
            .duration_since(self.samples.front().unwrap().0)
            .as_secs_f64();

        if total_duration > 0.001 {
            (second_slope - first_slope) / (total_duration / 2.0)
        } else {
            0.0
        }
    }

    /// Get number of samples
    pub fn count(&self) -> usize {
        self.samples.len()
    }

    /// Check if window is empty
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Get standard deviation
    pub fn std_dev(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }

        let avg = self.average();
        let variance = self.samples.iter()
            .map(|(_, v)| (v - avg).powi(2))
            .sum::<f64>() / (self.samples.len() - 1) as f64;

        variance.sqrt()
    }
}

/// Delta metrics for a token
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeltaMetrics {
    // Holder changes
    pub holder_count_delta_1m: i32,
    pub holder_count_delta_5m: i32,
    pub top_holder_pct_delta: f64,

    // Volume changes
    pub volume_delta_30s: f64,
    pub net_flow_delta_1m: f64,

    // Quality changes
    pub organic_score_trend: Trend,
    pub distribution_entropy_delta: f64,

    // Momentum
    pub buy_momentum: f64,
    pub sell_pressure_building: bool,

    // Price changes
    pub price_velocity: f64,
    pub price_acceleration: f64,

    // Computed from trends
    pub overall_trend: Trend,
    pub momentum_score: f64,
}

impl DeltaMetrics {
    /// Get a risk-adjusted momentum score (-1.0 to +1.0)
    pub fn momentum_signal(&self) -> f64 {
        let mut signal = 0.0;

        // Volume momentum
        signal += self.volume_delta_30s.signum() * 0.2;

        // Buy/sell pressure
        signal += self.buy_momentum * 0.3;
        if self.sell_pressure_building {
            signal -= 0.2;
        }

        // Organic trend
        match self.organic_score_trend {
            Trend::StronglyImproving => signal += 0.2,
            Trend::Improving => signal += 0.1,
            Trend::Deteriorating => signal -= 0.1,
            Trend::StronglyDeteriorating => signal -= 0.2,
            Trend::Stable => {}
        }

        // Price momentum
        if self.price_velocity > 0.0 && self.price_acceleration > 0.0 {
            signal += 0.1; // Accelerating upward
        } else if self.price_velocity < 0.0 && self.price_acceleration < 0.0 {
            signal -= 0.1; // Accelerating downward
        }

        signal.clamp(-1.0, 1.0)
    }

    /// Check if conditions are deteriorating
    pub fn is_deteriorating(&self) -> bool {
        self.organic_score_trend.is_negative()
            || self.sell_pressure_building
            || (self.price_velocity < 0.0 && self.price_acceleration < 0.0)
    }
}

/// Delta tracker for multiple metrics
pub struct DeltaTracker {
    /// Rolling windows by metric name
    windows: HashMap<String, RollingWindow>,
    /// Default window duration
    default_duration: Duration,
}

impl DeltaTracker {
    /// Create a new delta tracker
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            default_duration: Duration::from_secs(60), // 1 minute default
        }
    }

    /// Create with custom default duration
    pub fn with_duration(duration: Duration) -> Self {
        Self {
            windows: HashMap::new(),
            default_duration: duration,
        }
    }

    /// Get or create a window for a metric
    pub fn get_window(&mut self, name: &str) -> &mut RollingWindow {
        let duration = self.default_duration;
        self.windows.entry(name.to_string())
            .or_insert_with(|| RollingWindow::new(duration))
    }

    /// Get a window with specific duration
    pub fn get_window_with_duration(&mut self, name: &str, duration: Duration) -> &mut RollingWindow {
        self.windows.entry(name.to_string())
            .or_insert_with(|| RollingWindow::new(duration))
    }

    /// Record a metric value
    pub fn record(&mut self, name: &str, value: f64) {
        self.get_window(name).add(value);
    }

    /// Get delta for a metric
    pub fn delta(&mut self, name: &str) -> f64 {
        self.get_window(name).delta()
    }

    /// Get trend for a metric
    pub fn trend(&mut self, name: &str, threshold: f64) -> Trend {
        self.get_window(name).trend(threshold)
    }

    /// Compute delta metrics for a token
    pub fn compute_metrics(&mut self, token_mint: &str) -> DeltaMetrics {
        let prefix = |s: &str| format!("{}:{}", token_mint, s);

        let holder_1m = self.get_window_with_duration(&prefix("holders"), Duration::from_secs(60));
        let holder_count_delta_1m = holder_1m.delta() as i32;

        let holder_5m = self.get_window_with_duration(&prefix("holders_5m"), Duration::from_secs(300));
        let holder_count_delta_5m = holder_5m.delta() as i32;

        let top_holder = self.get_window(&prefix("top_holder_pct"));
        let top_holder_pct_delta = top_holder.delta();

        let volume_30s = self.get_window_with_duration(&prefix("volume"), Duration::from_secs(30));
        let volume_delta_30s = volume_30s.delta();

        let net_flow = self.get_window(&prefix("net_flow"));
        let net_flow_delta_1m = net_flow.delta();

        let organic = self.get_window(&prefix("organic_score"));
        let organic_score_trend = organic.trend(0.05);

        let entropy = self.get_window(&prefix("entropy"));
        let distribution_entropy_delta = entropy.delta();

        let buy_pct = self.get_window(&prefix("buy_pct"));
        let buy_momentum = buy_pct.velocity();
        let sell_pressure_building = buy_pct.latest() < 0.4 && buy_pct.velocity() < 0.0;

        let price = self.get_window(&prefix("price"));
        let price_velocity = price.velocity();
        let price_acceleration = price.acceleration();

        // Calculate overall trend
        let overall_trend = if organic_score_trend.is_positive() && price_velocity > 0.0 {
            if price_acceleration > 0.0 {
                Trend::StronglyImproving
            } else {
                Trend::Improving
            }
        } else if organic_score_trend.is_negative() || sell_pressure_building {
            if price_velocity < 0.0 && price_acceleration < 0.0 {
                Trend::StronglyDeteriorating
            } else {
                Trend::Deteriorating
            }
        } else {
            Trend::Stable
        };

        let metrics = DeltaMetrics {
            holder_count_delta_1m,
            holder_count_delta_5m,
            top_holder_pct_delta,
            volume_delta_30s,
            net_flow_delta_1m,
            organic_score_trend,
            distribution_entropy_delta,
            buy_momentum,
            sell_pressure_building,
            price_velocity,
            price_acceleration,
            overall_trend,
            momentum_score: 0.0, // Set below
        };

        DeltaMetrics {
            momentum_score: metrics.momentum_signal(),
            ..metrics
        }
    }

    /// Clear all windows
    pub fn clear(&mut self) {
        self.windows.clear();
    }

    /// Clear windows for a specific token
    pub fn clear_token(&mut self, token_mint: &str) {
        let prefix = format!("{}:", token_mint);
        self.windows.retain(|k, _| !k.starts_with(&prefix));
    }
}

impl Default for DeltaTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_rolling_window_add_and_delta() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        window.add(10.0);
        window.add(15.0);
        window.add(20.0);

        assert_eq!(window.delta(), 10.0);
        assert_eq!(window.latest(), 20.0);
        assert_eq!(window.oldest(), 10.0);
    }

    #[test]
    fn test_rolling_window_average() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        window.add(10.0);
        window.add(20.0);
        window.add(30.0);

        assert_eq!(window.average(), 20.0);
    }

    #[test]
    fn test_rolling_window_sum() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        window.add(10.0);
        window.add(20.0);
        window.add(30.0);

        assert_eq!(window.sum(), 60.0);
    }

    #[test]
    fn test_rolling_window_min_max() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        window.add(15.0);
        window.add(10.0);
        window.add(20.0);

        assert_eq!(window.min(), 10.0);
        assert_eq!(window.max(), 20.0);
    }

    #[test]
    fn test_rolling_window_trend() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        // Add increasing values
        for i in 0..10 {
            window.add(i as f64 * 10.0);
            sleep(Duration::from_millis(10));
        }

        let trend = window.trend(0.1);
        assert!(trend.is_positive());
    }

    #[test]
    fn test_delta_tracker_record_and_delta() {
        let mut tracker = DeltaTracker::new();

        tracker.record("test", 10.0);
        tracker.record("test", 20.0);
        tracker.record("test", 30.0);

        assert_eq!(tracker.delta("test"), 20.0);
    }

    #[test]
    fn test_delta_metrics_momentum_signal() {
        let mut metrics = DeltaMetrics::default();

        // Positive momentum
        metrics.buy_momentum = 0.5;
        metrics.volume_delta_30s = 100.0;
        metrics.organic_score_trend = Trend::Improving;

        let signal = metrics.momentum_signal();
        assert!(signal > 0.0);

        // Negative momentum
        metrics.buy_momentum = -0.5;
        metrics.sell_pressure_building = true;
        metrics.organic_score_trend = Trend::Deteriorating;

        let signal = metrics.momentum_signal();
        assert!(signal < 0.0);
    }

    #[test]
    fn test_delta_metrics_is_deteriorating() {
        let mut metrics = DeltaMetrics::default();
        assert!(!metrics.is_deteriorating());

        metrics.sell_pressure_building = true;
        assert!(metrics.is_deteriorating());

        metrics.sell_pressure_building = false;
        metrics.organic_score_trend = Trend::Deteriorating;
        assert!(metrics.is_deteriorating());
    }

    #[test]
    fn test_rolling_window_std_dev() {
        let mut window = RollingWindow::new(Duration::from_secs(60));

        // Same values = 0 std dev
        window.add(10.0);
        window.add(10.0);
        window.add(10.0);
        assert!(window.std_dev() < 0.001);

        // Different values
        let mut window2 = RollingWindow::new(Duration::from_secs(60));
        window2.add(1.0);
        window2.add(2.0);
        window2.add(3.0);
        assert!(window2.std_dev() > 0.5);
    }
}
