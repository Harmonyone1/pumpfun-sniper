//! Price Action Analyzer
//!
//! Understand price structure, not just order flow.
//! Tracks VWAP, structure (higher lows/lower highs), and volatility.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use super::delta_tracker::RollingWindow;

/// Price action analysis result
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PriceAction {
    // Reference prices
    pub vwap_since_launch: f64,
    pub current_price: f64,
    pub price_vs_vwap: f64,

    // Structure
    pub local_high: f64,
    pub local_low: f64,
    pub drawdown_from_high: f64,
    pub higher_lows: bool,
    pub lower_highs: bool,

    // Timing
    pub time_to_first_pullback_ms: u64,
    pub time_since_local_high_ms: u64,

    // Volatility
    pub volatility_1m: f64,
    pub volatility_compression: bool,
    pub volatility_expansion: bool,
}

impl PriceAction {
    /// Check if price structure is bullish
    pub fn is_bullish(&self) -> bool {
        self.higher_lows && !self.lower_highs && self.price_vs_vwap > -10.0
    }

    /// Check if price structure is bearish
    pub fn is_bearish(&self) -> bool {
        self.lower_highs && !self.higher_lows
    }

    /// Get entry quality (0.0 to 1.0)
    pub fn entry_quality(&self) -> f64 {
        let mut quality: f64 = 0.5;

        // Bullish structure is good
        if self.higher_lows {
            quality += 0.2;
        }
        if self.lower_highs {
            quality -= 0.2;
        }

        // Near VWAP is good for entry
        if self.price_vs_vwap.abs() < 5.0 {
            quality += 0.1;
        } else if self.price_vs_vwap > 20.0 {
            quality -= 0.1; // Chasing
        }

        // Pullback from high is good entry
        if self.drawdown_from_high > 10.0 && self.drawdown_from_high < 30.0 {
            quality += 0.15;
        }

        // Volatility compression often precedes moves
        if self.volatility_compression {
            quality += 0.1;
        }

        quality.clamp(0.0, 1.0)
    }
}

/// Price record for tracking
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PriceRecord {
    timestamp: std::time::Instant,
    price: f64,
    volume: f64,
}

/// Price Action Analyzer
pub struct PriceActionAnalyzer {
    prices: VecDeque<PriceRecord>,
    volatility_window: RollingWindow,
    max_records: usize,

    // State
    local_highs: VecDeque<f64>,  // Swing highs for structure analysis
    local_lows: VecDeque<f64>,   // Swing lows for structure analysis
    all_time_high: f64,          // Overall max for drawdown calculation
    total_volume: f64,
    volume_weighted_price_sum: f64,
    launch_time: Option<std::time::Instant>,
    first_pullback_time: Option<std::time::Instant>,
    last_high_time: Option<std::time::Instant>,
}

impl PriceActionAnalyzer {
    /// Create a new price action analyzer
    pub fn new() -> Self {
        Self {
            prices: VecDeque::new(),
            volatility_window: RollingWindow::new(std::time::Duration::from_secs(60)),
            max_records: 1000,
            local_highs: VecDeque::new(),
            local_lows: VecDeque::new(),
            all_time_high: 0.0,
            total_volume: 0.0,
            volume_weighted_price_sum: 0.0,
            launch_time: None,
            first_pullback_time: None,
            last_high_time: None,
        }
    }

    /// Record a new price tick
    pub fn record_price(&mut self, price: f64, volume: f64) {
        let now = std::time::Instant::now();

        if self.launch_time.is_none() {
            self.launch_time = Some(now);
        }

        // Track all-time high for drawdown calculation
        if price > self.all_time_high {
            self.all_time_high = price;
            self.last_high_time = Some(now);
        }

        // Track VWAP
        self.total_volume += volume;
        self.volume_weighted_price_sum += price * volume;

        // Track volatility (log returns)
        if let Some(last) = self.prices.back() {
            if last.price > 0.0 {
                let log_return = (price / last.price).ln();
                self.volatility_window.add(log_return.powi(2));
            }
        }

        // Add record
        self.prices.push_back(PriceRecord {
            timestamp: now,
            price,
            volume,
        });

        // Maintain size limit
        while self.prices.len() > self.max_records {
            self.prices.pop_front();
        }

        // Update swing highs/lows for structure analysis
        self.update_extremes(price, now);
    }

    /// Update local highs and lows (swing point detection)
    fn update_extremes(&mut self, price: f64, now: std::time::Instant) {
        // Need at least 2 previous prices for swing detection
        if self.prices.len() < 2 {
            return;
        }

        let prices: Vec<f64> = self.prices.iter().map(|r| r.price).collect();
        let len = prices.len();

        // Check if we just formed a swing high (previous price > its neighbors)
        if len >= 3 {
            let prev_price = prices[len - 2];
            let before_prev = prices[len - 3];
            let curr_price = price;

            // Swing high: prev > before_prev AND prev > curr
            if prev_price > before_prev && prev_price > curr_price {
                // Only add if it's different from last recorded high
                if self.local_highs.back().map_or(true, |&h| (h - prev_price).abs() > 0.0001) {
                    self.local_highs.push_back(prev_price);
                    self.last_high_time = Some(now);
                    while self.local_highs.len() > 10 {
                        self.local_highs.pop_front();
                    }
                }
            }

            // Swing low: prev < before_prev AND prev < curr
            if prev_price < before_prev && prev_price < curr_price {
                // Only add if it's different from last recorded low
                if self.local_lows.back().map_or(true, |&l| (l - prev_price).abs() > 0.0001) {
                    self.local_lows.push_back(prev_price);
                    while self.local_lows.len() > 10 {
                        self.local_lows.pop_front();
                    }

                    // Track first pullback
                    if self.first_pullback_time.is_none() && self.local_highs.len() >= 1 {
                        self.first_pullback_time = Some(now);
                    }
                }
            }
        }
    }

    /// Analyze current price action
    pub fn analyze(&self) -> PriceAction {
        let current_price = self.prices.back().map(|r| r.price).unwrap_or(0.0);

        // VWAP
        let vwap = if self.total_volume > 0.0 {
            self.volume_weighted_price_sum / self.total_volume
        } else {
            current_price
        };

        let price_vs_vwap = if vwap > 0.0 {
            ((current_price - vwap) / vwap) * 100.0
        } else {
            0.0
        };

        // Local high/low from swing points (for structure analysis)
        let local_high_swing = self.local_highs.iter().cloned().fold(0.0_f64, f64::max);
        let local_low = self.local_lows.iter().cloned().fold(f64::MAX, f64::min);
        let local_low = if local_low == f64::MAX { current_price } else { local_low };

        // Use all_time_high for local_high (more accurate for drawdown)
        let local_high = if self.all_time_high > 0.0 { self.all_time_high } else { local_high_swing };

        // Drawdown from all-time high
        let drawdown_from_high = if self.all_time_high > 0.0 {
            ((self.all_time_high - current_price) / self.all_time_high) * 100.0
        } else {
            0.0
        };

        // Structure analysis
        let higher_lows = self.check_higher_lows();
        let lower_highs = self.check_lower_highs();

        // Timing
        let time_to_first_pullback_ms = match (self.launch_time, self.first_pullback_time) {
            (Some(launch), Some(pullback)) => pullback.duration_since(launch).as_millis() as u64,
            _ => 0,
        };

        let time_since_local_high_ms = self.last_high_time
            .map(|t| std::time::Instant::now().duration_since(t).as_millis() as u64)
            .unwrap_or(0);

        // Volatility
        let volatility_1m = self.volatility_window.std_dev().sqrt() * 100.0;
        let volatility_compression = self.check_volatility_compression();
        let volatility_expansion = self.check_volatility_expansion();

        PriceAction {
            vwap_since_launch: vwap,
            current_price,
            price_vs_vwap,
            local_high,
            local_low,
            drawdown_from_high,
            higher_lows,
            lower_highs,
            time_to_first_pullback_ms,
            time_since_local_high_ms,
            volatility_1m,
            volatility_compression,
            volatility_expansion,
        }
    }

    /// Check if we have higher lows (bullish)
    fn check_higher_lows(&self) -> bool {
        if self.local_lows.len() < 2 {
            return false;
        }
        let lows: Vec<_> = self.local_lows.iter().cloned().collect();
        lows.windows(2).all(|w| w[1] >= w[0])
    }

    /// Check if we have lower highs (bearish)
    fn check_lower_highs(&self) -> bool {
        if self.local_highs.len() < 2 {
            return false;
        }
        let highs: Vec<_> = self.local_highs.iter().cloned().collect();
        highs.windows(2).all(|w| w[1] <= w[0])
    }

    /// Check for volatility compression
    fn check_volatility_compression(&self) -> bool {
        // Compare recent volatility to historical
        if self.prices.len() < 20 {
            return false;
        }
        let recent_vol = self.volatility_window.std_dev();
        let avg_vol = self.volatility_window.average();
        recent_vol < avg_vol * 0.7
    }

    /// Check for volatility expansion
    fn check_volatility_expansion(&self) -> bool {
        if self.prices.len() < 20 {
            return false;
        }
        let recent_vol = self.volatility_window.std_dev();
        let avg_vol = self.volatility_window.average();
        recent_vol > avg_vol * 1.5
    }

    /// Reset the analyzer
    pub fn reset(&mut self) {
        self.prices.clear();
        self.local_highs.clear();
        self.local_lows.clear();
        self.total_volume = 0.0;
        self.volume_weighted_price_sum = 0.0;
        self.launch_time = None;
        self.first_pullback_time = None;
        self.last_high_time = None;
    }
}

impl Default for PriceActionAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vwap_calculation() {
        let mut analyzer = PriceActionAnalyzer::new();

        analyzer.record_price(10.0, 100.0);
        analyzer.record_price(12.0, 100.0);

        let pa = analyzer.analyze();
        // VWAP = (10*100 + 12*100) / 200 = 11
        assert!((pa.vwap_since_launch - 11.0).abs() < 0.01);
    }

    #[test]
    fn test_drawdown_calculation() {
        let mut analyzer = PriceActionAnalyzer::new();

        analyzer.record_price(100.0, 1.0);
        analyzer.record_price(90.0, 1.0);

        let pa = analyzer.analyze();
        assert!((pa.drawdown_from_high - 10.0).abs() < 0.1);
    }

    #[test]
    fn test_higher_lows_detection() {
        let mut analyzer = PriceActionAnalyzer::new();

        // Create pattern with two swing lows (second higher than first)
        // Swing low requires: price goes down, then up (the low is confirmed)
        analyzer.record_price(100.0, 1.0);  // Start
        analyzer.record_price(90.0, 1.0);   // Down (potential first low)
        analyzer.record_price(105.0, 1.0);  // Up - confirms 90 as first swing low
        analyzer.record_price(95.0, 1.0);   // Down (potential second low, higher than 90)
        analyzer.record_price(110.0, 1.0);  // Up - confirms 95 as second swing low

        let pa = analyzer.analyze();
        // Now we have swing lows [90, 95] - ascending = higher_lows
        assert!(pa.higher_lows);
    }

    #[test]
    fn test_entry_quality() {
        let mut pa = PriceAction::default();

        // Bad setup
        pa.lower_highs = true;
        pa.price_vs_vwap = 30.0;
        assert!(pa.entry_quality() < 0.5);

        // Good setup
        pa.lower_highs = false;
        pa.higher_lows = true;
        pa.price_vs_vwap = 0.0;
        pa.drawdown_from_high = 15.0;
        assert!(pa.entry_quality() > 0.7);
    }

    #[test]
    fn test_bullish_bearish() {
        let mut pa = PriceAction::default();

        pa.higher_lows = true;
        pa.lower_highs = false;
        pa.price_vs_vwap = 5.0;
        assert!(pa.is_bullish());
        assert!(!pa.is_bearish());

        pa.higher_lows = false;
        pa.lower_highs = true;
        assert!(!pa.is_bullish());
        assert!(pa.is_bearish());
    }
}
