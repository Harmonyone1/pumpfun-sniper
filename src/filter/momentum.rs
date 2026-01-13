//! Momentum Validator
//!
//! Validates that tokens show real market activity before entry.
//! Tokens must demonstrate: Volume → Price response → Volatility → Trade opportunity
//!
//! If a token is not moving, it should not be traded.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Configuration for momentum validation
#[derive(Debug, Clone)]
pub struct MomentumConfig {
    /// Minimum observation time before entry allowed (seconds)
    pub min_observation_secs: u64,
    /// Maximum observation time before token is dropped (seconds)
    pub max_observation_secs: u64,
    /// Minimum number of trades required
    pub min_trade_count: u32,
    /// Minimum volume in SOL required
    pub min_volume_sol: f64,
    /// Minimum price change percentage (absolute) to show movement
    pub min_price_change_pct: f64,
    /// Minimum number of unique traders
    pub min_unique_traders: u32,
    /// Minimum buy ratio (buys / total trades)
    pub min_buy_ratio: f64,
    /// Minimum volatility (std dev of price changes)
    pub min_volatility: f64,
    /// SURVIVOR: Maximum holder concentration (top holder as % of supply)
    pub max_holder_concentration: f64,
    /// SURVIVOR: Minimum survival ratio (current_price / peak_price)
    pub min_survival_ratio: f64,
    /// SURVIVOR: Second-wave window (last X% of observation to check for continued buying)
    pub second_wave_window_pct: f64,
    /// SURVIVOR: Minimum buy ratio in second wave (must have recent buying activity)
    pub min_second_wave_ratio: f64,
}

impl Default for MomentumConfig {
    fn default() -> Self {
        Self {
            min_observation_secs: 60,       // SURVIVOR: Wait 60s for snipers to exit
            max_observation_secs: 180,     // SURVIVOR: 3 min max observation
            min_trade_count: 10,           // SURVIVOR: Need substantial activity
            min_volume_sol: 2.0,           // SURVIVOR: Real volume required
            min_price_change_pct: 5.0,     // SURVIVOR: Must survive (positive price)
            min_unique_traders: 5,         // SURVIVOR: Need trader distribution
            min_buy_ratio: 0.55,           // SURVIVOR: Majority buying
            min_volatility: 0.01,           // SURVIVOR: Active trading required
            max_holder_concentration: 0.50, // SURVIVOR: No whale dominance (max 50%)
            min_survival_ratio: 0.70,       // SURVIVOR: Price >= 70% of peak
            second_wave_window_pct: 0.30,   // SURVIVOR: Check last 30% of observation
            min_second_wave_ratio: 0.40,    // SURVIVOR: At least 40% buys in recent window
        }
    }
}

/// A single trade event for momentum tracking
#[derive(Debug, Clone)]
pub struct TradeEvent {
    pub timestamp: Instant,
    pub is_buy: bool,
    pub sol_amount: f64,
    pub token_amount: f64,
    pub price: f64,  // SOL per token
    pub trader: String,
}

/// Momentum metrics for a token
#[derive(Debug, Clone, Default)]
pub struct MomentumMetrics {
    pub observation_started: Option<Instant>,
    pub trade_count: u32,
    pub buy_count: u32,
    pub sell_count: u32,
    pub total_volume_sol: f64,
    pub unique_traders: u32,
    pub first_price: f64,
    pub last_price: f64,
    pub high_price: f64,
    pub low_price: f64,
    pub price_change_pct: f64,
    pub volatility: f64,
    pub buy_ratio: f64,
    pub observation_secs: f64,
    // DATA-DRIVEN: Volume-weighted metrics
    pub buy_volume_sol: f64,
    pub sell_volume_sol: f64,
    pub volume_buy_ratio: f64, // buy_volume / (buy_volume + sell_volume)
    pub net_flow_sol: f64,     // buy_volume - sell_volume
    // SURVIVOR: Survival metrics
    pub survival_ratio: f64,       // current_price / peak_price
    pub holder_concentration: f64, // top holder as % of supply (set externally)
    pub holder_data_fetched: bool, // whether holder data has been fetched
    pub second_wave_buy_ratio: f64, // buy ratio in last 30% of observation window
}

impl MomentumMetrics {
    /// Check if metrics meet entry thresholds
    /// DATA-DRIVEN: Updated to use volume-weighted ratios and require positive momentum
    pub fn meets_thresholds(&self, config: &MomentumConfig) -> bool {
        // Must have observed long enough
        if self.observation_secs < config.min_observation_secs as f64 {
            return false;
        }

        // Must have enough trades
        if self.trade_count < config.min_trade_count {
            return false;
        }

        // Must have enough volume
        if self.total_volume_sol < config.min_volume_sol {
            return false;
        }

        // DATA-DRIVEN: Require POSITIVE price movement (not just absolute change)
        // Buying into a -5% dump is bad even though abs(change) > 2%
        if self.price_change_pct < config.min_price_change_pct {
            return false;
        }

        // Must have enough unique traders
        if self.unique_traders < config.min_unique_traders {
            return false;
        }

        // DATA-DRIVEN: Use VOLUME-weighted buy ratio instead of trade count
        // 3 tiny buys shouldn't outweigh 1 large sell
        if self.volume_buy_ratio < config.min_buy_ratio {
            return false;
        }

        // DATA-DRIVEN: Require positive net flow (more buying than selling by volume)
        if self.net_flow_sol < 0.0 {
            return false;
        }

        // Must have volatility (not flat)
        if self.volatility < config.min_volatility {
            return false;
        }

        // SURVIVOR: Price must hold from peak (not in freefall)
        if self.survival_ratio < config.min_survival_ratio {
            return false;
        }

        // SURVIVOR: Require holder data to be fetched before entry
        if !self.holder_data_fetched {
            return false;
        }

        // SURVIVOR: No whale dominance (holder concentration check)
        if self.holder_concentration > config.max_holder_concentration {
            return false;
        }

        // SURVIVOR: Must have buying activity in second wave (not just early pump)
        // Only check if we've observed long enough for a meaningful second wave
        if self.observation_secs >= config.min_observation_secs as f64 {
            if self.second_wave_buy_ratio < config.min_second_wave_ratio {
                return false;
            }
        }

        true
    }

    /// Get human-readable status
    pub fn status_string(&self, config: &MomentumConfig) -> String {
        let mut missing = Vec::new();

        if self.observation_secs < config.min_observation_secs as f64 {
            missing.push(format!("obs:{:.0}s<{}s", self.observation_secs, config.min_observation_secs));
        }
        if self.trade_count < config.min_trade_count {
            missing.push(format!("trades:{}<{}", self.trade_count, config.min_trade_count));
        }
        if self.total_volume_sol < config.min_volume_sol {
            missing.push(format!("vol:{:.2}<{:.2}", self.total_volume_sol, config.min_volume_sol));
        }
        // DATA-DRIVEN: Check for positive price change
        if self.price_change_pct < config.min_price_change_pct {
            missing.push(format!("price:{:+.1}%<+{:.1}%", self.price_change_pct, config.min_price_change_pct));
        }
        if self.unique_traders < config.min_unique_traders {
            missing.push(format!("traders:{}<{}", self.unique_traders, config.min_unique_traders));
        }
        // DATA-DRIVEN: Show volume-weighted buy ratio
        if self.volume_buy_ratio < config.min_buy_ratio {
            missing.push(format!("vol_buy:{:.0}%<{:.0}%", self.volume_buy_ratio * 100.0, config.min_buy_ratio * 100.0));
        }
        // DATA-DRIVEN: Show net flow requirement
        if self.net_flow_sol < 0.0 {
            missing.push(format!("net_flow:{:+.2}SOL<0", self.net_flow_sol));
        }
        if self.volatility < config.min_volatility {
            missing.push(format!("volatility:{:.4}<{:.4}", self.volatility, config.min_volatility));
        }
        // SURVIVOR: Survival ratio check
        if self.survival_ratio < config.min_survival_ratio {
            missing.push(format!("survival:{:.0}%<{:.0}%", self.survival_ratio * 100.0, config.min_survival_ratio * 100.0));
        }
        // SURVIVOR: Holder data must be fetched
        if !self.holder_data_fetched {
            missing.push("holder_data:pending".to_string());
        } else if self.holder_concentration > config.max_holder_concentration {
            // SURVIVOR: Holder concentration check (only if data fetched)
            missing.push(format!("whale:{:.0}%>{:.0}%", self.holder_concentration * 100.0, config.max_holder_concentration * 100.0));
        }
        // SURVIVOR: Second wave check (only when observation complete)
        if self.observation_secs >= config.min_observation_secs as f64 {
            if self.second_wave_buy_ratio < config.min_second_wave_ratio {
                missing.push(format!("2nd_wave:{:.0}%<{:.0}%", self.second_wave_buy_ratio * 100.0, config.min_second_wave_ratio * 100.0));
            }
        }

        if missing.is_empty() {
            "READY".to_string()
        } else {
            format!("WAITING: {}", missing.join(", "))
        }
    }
}

/// Watched token state
#[derive(Debug)]
struct WatchedToken {
    #[allow(dead_code)]
    mint: String,
    symbol: String,
    name: String,
    bonding_curve: String,
    started: Instant,
    trades: Vec<TradeEvent>,
    traders: std::collections::HashSet<String>,
    #[allow(dead_code)]
    initial_market_cap: f64,
    /// SURVIVOR: Top holder concentration (set via Helius API)
    holder_concentration: f64,
    /// SURVIVOR: Whether holder data has been fetched (must be true before entry)
    holder_data_fetched: bool,
}

impl WatchedToken {
    fn new(mint: String, symbol: String, name: String, bonding_curve: String, initial_market_cap: f64) -> Self {
        Self {
            mint,
            symbol,
            name,
            bonding_curve,
            started: Instant::now(),
            trades: Vec::new(),
            traders: std::collections::HashSet::new(),
            initial_market_cap,
            holder_concentration: 0.0, // Set via set_holder_concentration()
            holder_data_fetched: false, // Will be set true when Helius data arrives
        }
    }

    fn add_trade(&mut self, trade: TradeEvent) {
        self.traders.insert(trade.trader.clone());
        self.trades.push(trade);
    }

    fn calculate_metrics(&self) -> MomentumMetrics {
        if self.trades.is_empty() {
            return MomentumMetrics {
                observation_started: Some(self.started),
                observation_secs: self.started.elapsed().as_secs_f64(),
                ..Default::default()
            };
        }

        let buy_count = self.trades.iter().filter(|t| t.is_buy).count() as u32;
        let sell_count = self.trades.iter().filter(|t| !t.is_buy).count() as u32;
        let total_volume: f64 = self.trades.iter().map(|t| t.sol_amount).sum();

        // DATA-DRIVEN: Calculate volume-weighted metrics
        let buy_volume_sol: f64 = self.trades.iter()
            .filter(|t| t.is_buy)
            .map(|t| t.sol_amount)
            .sum();
        let sell_volume_sol: f64 = self.trades.iter()
            .filter(|t| !t.is_buy)
            .map(|t| t.sol_amount)
            .sum();
        let volume_buy_ratio = if total_volume > 0.0 {
            buy_volume_sol / total_volume
        } else {
            0.0
        };
        let net_flow_sol = buy_volume_sol - sell_volume_sol;

        let first_price = self.trades.first().map(|t| t.price).unwrap_or(0.0);
        let last_price = self.trades.last().map(|t| t.price).unwrap_or(0.0);

        let high_price = self.trades.iter().map(|t| t.price).fold(0.0_f64, f64::max);
        let low_price = self.trades.iter().map(|t| t.price).fold(f64::MAX, f64::min);

        // SURVIVOR: Calculate survival ratio (how well price held from peak)
        let survival_ratio = if high_price > 0.0 {
            last_price / high_price
        } else {
            0.0
        };

        let price_change_pct = if first_price > 0.0 {
            ((last_price - first_price) / first_price) * 100.0
        } else {
            0.0
        };

        // Calculate volatility as coefficient of variation (std_dev / mean)
        // This gives a scale-independent measure that works for tiny token prices
        let volatility = if self.trades.len() > 1 {
            let prices: Vec<f64> = self.trades.iter().map(|t| t.price).collect();
            let mean = prices.iter().sum::<f64>() / prices.len() as f64;
            if mean > 0.0 {
                let variance = prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / prices.len() as f64;
                variance.sqrt() / mean  // Coefficient of variation
            } else {
                0.0
            }
        } else {
            0.0
        };

        let buy_ratio = if self.trades.is_empty() {
            0.0
        } else {
            buy_count as f64 / self.trades.len() as f64
        };

        // SURVIVOR: Calculate second-wave buy ratio (activity in last 30% of observation)
        let observation_elapsed = self.started.elapsed();
        let second_wave_threshold = Duration::from_secs_f64(
            observation_elapsed.as_secs_f64() * 0.70 // Start of last 30%
        );
        let second_wave_trades: Vec<_> = self.trades.iter()
            .filter(|t| t.timestamp.duration_since(self.started) >= second_wave_threshold)
            .collect();
        let second_wave_buy_ratio = if second_wave_trades.is_empty() {
            0.0 // No trades in second wave = 0% buys
        } else {
            let sw_buys = second_wave_trades.iter().filter(|t| t.is_buy).count();
            sw_buys as f64 / second_wave_trades.len() as f64
        };

        MomentumMetrics {
            observation_started: Some(self.started),
            trade_count: self.trades.len() as u32,
            buy_count,
            sell_count,
            total_volume_sol: total_volume,
            unique_traders: self.traders.len() as u32,
            first_price,
            last_price,
            high_price,
            low_price,
            price_change_pct,
            volatility,
            buy_ratio,
            observation_secs: self.started.elapsed().as_secs_f64(),
            // DATA-DRIVEN: New volume-weighted metrics
            buy_volume_sol,
            sell_volume_sol,
            volume_buy_ratio,
            net_flow_sol,
            // SURVIVOR
            survival_ratio,
            holder_concentration: self.holder_concentration,
            holder_data_fetched: self.holder_data_fetched,
            second_wave_buy_ratio,
        }
    }
}

/// Momentum validation result
#[derive(Debug, Clone)]
pub enum MomentumStatus {
    /// Token is being observed, not ready for entry
    Observing {
        metrics: MomentumMetrics,
        reason: String,
    },
    /// Token shows momentum, ready for entry
    Ready {
        metrics: MomentumMetrics,
    },
    /// Token expired without showing momentum
    Expired {
        metrics: MomentumMetrics,
    },
    /// Token not found in watchlist
    NotWatched,
}

/// Momentum Validator - tracks tokens and validates activity before entry
pub struct MomentumValidator {
    config: MomentumConfig,
    watchlist: RwLock<HashMap<String, WatchedToken>>,
}

impl MomentumValidator {
    pub fn new(config: MomentumConfig) -> Self {
        Self {
            config,
            watchlist: RwLock::new(HashMap::new()),
        }
    }

    /// Add a token to the watchlist
    pub async fn watch_token(
        &self,
        mint: &str,
        symbol: &str,
        name: &str,
        bonding_curve: &str,
        initial_market_cap: f64,
    ) {
        let mut watchlist = self.watchlist.write().await;

        if watchlist.contains_key(mint) {
            debug!("Token {} already being watched", symbol);
            return;
        }

        info!(
            "Watching token for momentum: {} ({}) - will observe for {}s before entry",
            symbol, mint, self.config.min_observation_secs
        );

        watchlist.insert(
            mint.to_string(),
            WatchedToken::new(
                mint.to_string(),
                symbol.to_string(),
                name.to_string(),
                bonding_curve.to_string(),
                initial_market_cap,
            ),
        );
    }

    /// Record a trade event for a watched token
    pub async fn record_trade(
        &self,
        mint: &str,
        is_buy: bool,
        sol_amount: f64,
        token_amount: f64,
        trader: &str,
    ) {
        let mut watchlist = self.watchlist.write().await;

        if let Some(token) = watchlist.get_mut(mint) {
            let price = if token_amount > 0.0 {
                sol_amount / token_amount
            } else {
                0.0
            };

            token.add_trade(TradeEvent {
                timestamp: Instant::now(),
                is_buy,
                sol_amount,
                token_amount,
                price,
                trader: trader.to_string(),
            });

            debug!(
                "Recorded {} for {}: {:.4} SOL (trades: {}, vol: {:.2} SOL)",
                if is_buy { "BUY" } else { "SELL" },
                token.symbol,
                sol_amount,
                token.trades.len(),
                token.trades.iter().map(|t| t.sol_amount).sum::<f64>()
            );
        }
    }

    /// Set holder concentration for a watched token (from Helius API)
    pub async fn set_holder_concentration(&self, mint: &str, concentration: f64) {
        let mut watchlist = self.watchlist.write().await;
        if let Some(token) = watchlist.get_mut(mint) {
            token.holder_concentration = concentration;
            token.holder_data_fetched = true;
            debug!(
                "Set holder concentration for {}: {:.1}%",
                token.symbol, concentration * 100.0
            );
        }
    }

    /// Check if a token is ready for entry
    pub async fn check_momentum(&self, mint: &str) -> MomentumStatus {
        let watchlist = self.watchlist.read().await;

        let token = match watchlist.get(mint) {
            Some(t) => t,
            None => return MomentumStatus::NotWatched,
        };

        let metrics = token.calculate_metrics();
        let elapsed = token.started.elapsed();

        // Check if expired
        if elapsed > Duration::from_secs(self.config.max_observation_secs) {
            return MomentumStatus::Expired { metrics };
        }

        // Check if ready
        if metrics.meets_thresholds(&self.config) {
            return MomentumStatus::Ready { metrics };
        }

        // Still observing
        let reason = metrics.status_string(&self.config);
        MomentumStatus::Observing { metrics, reason }
    }

    /// Remove a token from the watchlist (after entry or expiration)
    pub async fn remove_token(&self, mint: &str) {
        let mut watchlist = self.watchlist.write().await;
        if let Some(token) = watchlist.remove(mint) {
            debug!("Removed {} from momentum watchlist", token.symbol);
        }
    }

    /// Get all watched tokens and their status
    pub async fn get_all_status(&self) -> Vec<(String, String, MomentumStatus)> {
        let watchlist = self.watchlist.read().await;
        let mut results = Vec::new();

        for (mint, token) in watchlist.iter() {
            let metrics = token.calculate_metrics();
            let elapsed = token.started.elapsed();

            let status = if elapsed > Duration::from_secs(self.config.max_observation_secs) {
                MomentumStatus::Expired { metrics }
            } else if metrics.meets_thresholds(&self.config) {
                MomentumStatus::Ready { metrics }
            } else {
                let reason = metrics.status_string(&self.config);
                MomentumStatus::Observing { metrics, reason }
            };

            results.push((mint.clone(), token.symbol.clone(), status));
        }

        results
    }

    /// Clean up expired tokens
    pub async fn cleanup_expired(&self) -> Vec<String> {
        let mut watchlist = self.watchlist.write().await;
        let mut expired = Vec::new();

        watchlist.retain(|mint, token| {
            let elapsed = token.started.elapsed();
            if elapsed > Duration::from_secs(self.config.max_observation_secs) {
                let metrics = token.calculate_metrics();
                warn!(
                    "Token {} expired without momentum: trades={}, vol={:.2} SOL, price_chg={:.1}%",
                    token.symbol, metrics.trade_count, metrics.total_volume_sol, metrics.price_change_pct
                );
                expired.push(mint.clone());
                false
            } else {
                true
            }
        });

        expired
    }

    /// Check if a token is being watched
    pub async fn is_watching(&self, mint: &str) -> bool {
        let watchlist = self.watchlist.read().await;
        let result = watchlist.contains_key(mint);
        if !result && !watchlist.is_empty() {
            // Log for debugging - trade for unknown token
            tracing::debug!("is_watching check failed: mint={}, watchlist has {} tokens: {:?}",
                mint, watchlist.len(), watchlist.keys().collect::<Vec<_>>());
        }
        result
    }

    /// Get count of watched tokens
    pub async fn watched_count(&self) -> usize {
        let watchlist = self.watchlist.read().await;
        watchlist.len()
    }

    /// Get token info if watched
    pub async fn get_token_info(&self, mint: &str) -> Option<(String, String, String)> {
        let watchlist = self.watchlist.read().await;
        watchlist.get(mint).map(|t| (t.symbol.clone(), t.name.clone(), t.bonding_curve.clone()))
    }
}

impl Default for MomentumValidator {
    fn default() -> Self {
        Self::new(MomentumConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_momentum_config_defaults() {
        let config = MomentumConfig::default();
        assert_eq!(config.min_observation_secs, 5);
        assert_eq!(config.min_trade_count, 3);
        assert!(config.min_volume_sol > 0.0);
    }

    #[tokio::test]
    async fn test_watch_and_record() {
        let validator = MomentumValidator::default();

        validator.watch_token("mint1", "TEST", "Test Token", "curve1", 30.0).await;
        assert!(validator.is_watching("mint1").await);

        // Record some trades
        validator.record_trade("mint1", true, 0.1, 1000.0, "trader1").await;
        validator.record_trade("mint1", true, 0.2, 2000.0, "trader2").await;

        let status = validator.check_momentum("mint1").await;
        matches!(status, MomentumStatus::Observing { .. });
    }

    #[test]
    fn test_metrics_thresholds() {
        let config = MomentumConfig {
            min_observation_secs: 0,  // No wait for testing
            min_trade_count: 2,
            min_volume_sol: 0.1,
            min_price_change_pct: 1.0,
            min_unique_traders: 5,         // SURVIVOR: 5 traders
            min_buy_ratio: 0.55,           // SURVIVOR: 55% buy volume
            min_volatility: 0.001,
            ..Default::default()
        };

        let metrics = MomentumMetrics {
            observation_secs: 10.0,
            trade_count: 5,
            buy_count: 4,
            sell_count: 1,
            total_volume_sol: 0.5,
            unique_traders: 3,
            price_change_pct: 10.0,
            volatility: 0.01,
            buy_ratio: 0.8,
            ..Default::default()
        };

        assert!(metrics.meets_thresholds(&config));
    }
}


