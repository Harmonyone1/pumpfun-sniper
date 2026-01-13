//! Randomization Module
//!
//! Adversarial resistance through unpredictable behavior.
//! Prevents pattern detection by adding jitter to timing and sizing.

use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::types::TradingStrategy;

/// Randomization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RandomizationConfig {
    pub enabled: bool,

    // Entry jitter
    pub entry_delay_min_ms: u64,
    pub entry_delay_max_ms: u64,
    pub entry_size_jitter_pct: f64,

    // Exit jitter
    pub exit_delay_min_ms: u64,
    pub exit_delay_max_ms: u64,
    pub exit_size_jitter_pct: f64,

    // Strategy mixing
    pub strategy_entropy: f64, // 0.0 = deterministic, 1.0 = random
    pub skip_probability: f64, // Probability to randomly skip a trade

    // Timing
    pub vary_check_interval: bool,
    pub check_interval_jitter_pct: f64,
}

impl Default for RandomizationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            entry_delay_min_ms: 50,
            entry_delay_max_ms: 200,
            entry_size_jitter_pct: 5.0,
            exit_delay_min_ms: 25,
            exit_delay_max_ms: 100,
            exit_size_jitter_pct: 3.0,
            strategy_entropy: 0.1,
            skip_probability: 0.02,
            vary_check_interval: true,
            check_interval_jitter_pct: 10.0,
        }
    }
}

/// Randomizer for adversarial resistance
pub struct Randomizer {
    config: RandomizationConfig,
    rng: StdRng,
}

impl Randomizer {
    /// Create a new randomizer with optional seed
    pub fn new(config: RandomizationConfig, seed: Option<u64>) -> Self {
        let rng = match seed {
            Some(s) => StdRng::seed_from_u64(s),
            None => StdRng::from_entropy(),
        };
        Self { config, rng }
    }

    /// Create randomizer from entropy (random seed)
    pub fn from_entropy(config: RandomizationConfig) -> Self {
        Self::new(config, None)
    }

    /// Check if randomization is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Randomize entry delay
    pub fn jitter_entry_delay(&mut self) -> Duration {
        if !self.config.enabled {
            return Duration::ZERO;
        }

        let delay_ms = self
            .rng
            .gen_range(self.config.entry_delay_min_ms..=self.config.entry_delay_max_ms);
        Duration::from_millis(delay_ms)
    }

    /// Randomize exit delay
    pub fn jitter_exit_delay(&mut self) -> Duration {
        if !self.config.enabled {
            return Duration::ZERO;
        }

        let delay_ms = self
            .rng
            .gen_range(self.config.exit_delay_min_ms..=self.config.exit_delay_max_ms);
        Duration::from_millis(delay_ms)
    }

    /// Randomize position size for entry
    pub fn jitter_entry_size(&mut self, base_size: f64) -> f64 {
        if !self.config.enabled {
            return base_size;
        }

        let jitter = self.config.entry_size_jitter_pct / 100.0;
        let factor = self.rng.gen_range((1.0 - jitter)..=(1.0 + jitter));
        base_size * factor
    }

    /// Randomize position size for exit
    pub fn jitter_exit_size(&mut self, base_size: f64) -> f64 {
        if !self.config.enabled {
            return base_size;
        }

        let jitter = self.config.exit_size_jitter_pct / 100.0;
        let factor = self.rng.gen_range((1.0 - jitter)..=(1.0 + jitter));
        base_size * factor
    }

    /// Should we randomly skip this trade for unpredictability?
    pub fn should_skip_randomly(&mut self) -> bool {
        if !self.config.enabled {
            return false;
        }

        self.rng.gen::<f64>() < self.config.skip_probability
    }

    /// Mix strategy selection with entropy
    pub fn select_strategy_with_entropy(
        &mut self,
        recommended: TradingStrategy,
        alternatives: &[TradingStrategy],
    ) -> TradingStrategy {
        if !self.config.enabled || alternatives.is_empty() {
            return recommended;
        }

        if self.rng.gen::<f64>() < self.config.strategy_entropy {
            // Randomly pick an alternative
            alternatives
                .choose(&mut self.rng)
                .cloned()
                .unwrap_or(recommended)
        } else {
            recommended
        }
    }

    /// Randomize check interval for unpredictable polling
    pub fn jitter_interval(&mut self, base_interval: Duration) -> Duration {
        if !self.config.enabled || !self.config.vary_check_interval {
            return base_interval;
        }

        let jitter = self.config.check_interval_jitter_pct / 100.0;
        let factor = self.rng.gen_range((1.0 - jitter)..=(1.0 + jitter));
        Duration::from_secs_f64(base_interval.as_secs_f64() * factor)
    }

    /// Get full jittered entry parameters
    pub fn jitter_entry(&mut self, base_size: f64) -> JitteredEntry {
        JitteredEntry {
            delay: self.jitter_entry_delay(),
            size: self.jitter_entry_size(base_size),
            should_skip: self.should_skip_randomly(),
        }
    }

    /// Get full jittered exit parameters
    pub fn jitter_exit(&mut self, base_size: f64) -> JitteredExit {
        JitteredExit {
            delay: self.jitter_exit_delay(),
            size: self.jitter_exit_size(base_size),
        }
    }

    /// Generate a random factor within a percentage range
    pub fn random_factor(&mut self, variance_pct: f64) -> f64 {
        if !self.config.enabled {
            return 1.0;
        }

        let variance = variance_pct / 100.0;
        self.rng.gen_range((1.0 - variance)..=(1.0 + variance))
    }

    /// Generate a random delay within a range
    pub fn random_delay(&mut self, min_ms: u64, max_ms: u64) -> Duration {
        if !self.config.enabled {
            return Duration::ZERO;
        }

        let delay_ms = self.rng.gen_range(min_ms..=max_ms);
        Duration::from_millis(delay_ms)
    }

    /// Decide whether to take an action with given probability
    pub fn should_act(&mut self, probability: f64) -> bool {
        if !self.config.enabled {
            return true;
        }

        self.rng.gen::<f64>() < probability
    }

    /// Get random priority fee multiplier
    pub fn jitter_priority_fee(&mut self, base_fee: u64, variance_pct: f64) -> u64 {
        if !self.config.enabled {
            return base_fee;
        }

        let variance = variance_pct / 100.0;
        let factor = self.rng.gen_range((1.0 - variance)..=(1.0 + variance));
        (base_fee as f64 * factor) as u64
    }

    /// Reset the RNG with a new seed
    pub fn reseed(&mut self, seed: u64) {
        self.rng = StdRng::seed_from_u64(seed);
    }
}

impl Default for Randomizer {
    fn default() -> Self {
        Self::from_entropy(RandomizationConfig::default())
    }
}

/// Jittered entry parameters
#[derive(Debug, Clone)]
pub struct JitteredEntry {
    pub delay: Duration,
    pub size: f64,
    pub should_skip: bool,
}

/// Jittered exit parameters
#[derive(Debug, Clone)]
pub struct JitteredExit {
    pub delay: Duration,
    pub size: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_with_seed() {
        let config = RandomizationConfig::default();
        let mut r1 = Randomizer::new(config.clone(), Some(12345));
        let mut r2 = Randomizer::new(config, Some(12345));

        // Same seed should produce same results
        let delay1 = r1.jitter_entry_delay();
        let delay2 = r2.jitter_entry_delay();
        assert_eq!(delay1, delay2);
    }

    #[test]
    fn test_entry_delay_range() {
        let config = RandomizationConfig {
            entry_delay_min_ms: 50,
            entry_delay_max_ms: 200,
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        for _ in 0..100 {
            let delay = randomizer.jitter_entry_delay();
            assert!(delay.as_millis() >= 50);
            assert!(delay.as_millis() <= 200);
        }
    }

    #[test]
    fn test_size_jitter_range() {
        let config = RandomizationConfig {
            entry_size_jitter_pct: 10.0,
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        for _ in 0..100 {
            let size = randomizer.jitter_entry_size(1.0);
            assert!(size >= 0.9);
            assert!(size <= 1.1);
        }
    }

    #[test]
    fn test_skip_probability() {
        let config = RandomizationConfig {
            skip_probability: 0.5, // 50% skip
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        let mut skips = 0;
        let iterations = 1000;
        for _ in 0..iterations {
            if randomizer.should_skip_randomly() {
                skips += 1;
            }
        }

        // Should be approximately 50% (allow 10% variance)
        let skip_rate = skips as f64 / iterations as f64;
        assert!(skip_rate > 0.4);
        assert!(skip_rate < 0.6);
    }

    #[test]
    fn test_disabled_randomization() {
        let config = RandomizationConfig {
            enabled: false,
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        // Disabled should return base values
        assert_eq!(randomizer.jitter_entry_delay(), Duration::ZERO);
        assert_eq!(randomizer.jitter_entry_size(0.5), 0.5);
        assert!(!randomizer.should_skip_randomly());
    }

    #[test]
    fn test_interval_jitter() {
        let config = RandomizationConfig {
            vary_check_interval: true,
            check_interval_jitter_pct: 20.0,
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        let base = Duration::from_secs(10);
        for _ in 0..100 {
            let jittered = randomizer.jitter_interval(base);
            // 20% jitter means 8-12 seconds
            assert!(jittered.as_secs_f64() >= 8.0);
            assert!(jittered.as_secs_f64() <= 12.0);
        }
    }

    #[test]
    fn test_strategy_entropy() {
        let config = RandomizationConfig {
            strategy_entropy: 1.0, // Always random
            ..Default::default()
        };
        let mut randomizer = Randomizer::new(config, Some(42));

        let recommended = TradingStrategy::MomentumSurfing;
        let alternatives = vec![
            TradingStrategy::SnipeAndScalp,
            TradingStrategy::WhaleFollowing,
        ];

        let mut selected_alternatives = 0;
        for _ in 0..100 {
            let selected =
                randomizer.select_strategy_with_entropy(recommended.clone(), &alternatives);
            if !matches!(selected, TradingStrategy::MomentumSurfing) {
                selected_alternatives += 1;
            }
        }

        // With 100% entropy, all should be alternatives
        assert_eq!(selected_alternatives, 100);
    }

    #[test]
    fn test_jitter_entry_combined() {
        let config = RandomizationConfig::default();
        let mut randomizer = Randomizer::new(config, Some(42));

        let jittered = randomizer.jitter_entry(0.5);
        assert!(jittered.delay.as_millis() >= 50);
        assert!(jittered.delay.as_millis() <= 200);
        assert!(jittered.size >= 0.475); // 5% jitter
        assert!(jittered.size <= 0.525);
    }

    #[test]
    fn test_priority_fee_jitter() {
        let config = RandomizationConfig::default();
        let mut randomizer = Randomizer::new(config, Some(42));

        let base_fee: u64 = 10000;
        for _ in 0..100 {
            let jittered = randomizer.jitter_priority_fee(base_fee, 10.0);
            assert!(jittered >= 9000);
            assert!(jittered <= 11000);
        }
    }

    #[test]
    fn test_reseed() {
        let config = RandomizationConfig::default();
        let mut randomizer = Randomizer::new(config, Some(42));

        let delay1 = randomizer.jitter_entry_delay();

        randomizer.reseed(42);
        let delay2 = randomizer.jitter_entry_delay();

        assert_eq!(delay1, delay2);
    }
}
