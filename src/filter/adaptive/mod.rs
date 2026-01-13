//! Adaptive filtering system for intelligent token analysis
//!
//! The adaptive filter replaces simple threshold-based filtering with
//! a multi-signal scoring system that considers wallet behavior, token
//! distribution, order flow, and pump.fun-specific patterns.

pub mod config;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::error::Result;
use crate::filter::cache::FilterCache;
use crate::filter::enrichment::EnrichmentService;
use crate::filter::scoring::{Recommendation, ScoringEngine, ScoringResult};
use crate::filter::signals::{Signal, SignalProvider, SignalType};
use crate::filter::types::SignalContext;

pub use config::AdaptiveFilterConfig;

/// The main adaptive filter coordinator
///
/// Manages signal providers, caching, scoring, and background enrichment.
pub struct AdaptiveFilter {
    /// Configuration
    config: AdaptiveFilterConfig,

    /// Signal providers for hot path (fast)
    hot_path_providers: Vec<Arc<dyn SignalProvider>>,

    /// Signal providers for background analysis
    background_providers: Vec<Arc<dyn SignalProvider>>,

    /// Shared cache for all providers
    cache: Arc<FilterCache>,

    /// Scoring engine
    scoring_engine: ScoringEngine,

    /// Optional enrichment service for fetching data from Helius
    enrichment: Option<Arc<EnrichmentService>>,

    /// Whether the filter is in degraded mode (some components failed)
    degraded_mode: Arc<RwLock<DegradedMode>>,
}

/// Tracks degraded mode state
#[derive(Default)]
pub struct DegradedMode {
    pub background_unavailable: bool,
    pub cache_cold: bool,
    pub known_actors_failed: bool,
    pub reason: Option<String>,
}

impl DegradedMode {
    /// Calculate confidence penalty for degraded mode
    /// Reduced penalties for aggressive pump.fun trading
    pub fn confidence_penalty(&self) -> f64 {
        let mut penalty = 1.0;

        if self.background_unavailable {
            penalty *= 0.95; // 5% reduction (was 30%)
        }
        if self.cache_cold {
            penalty *= 0.95; // 5% reduction (was 20%)
        }
        if self.known_actors_failed {
            penalty *= 0.95; // 5% reduction (was 10%)
        }

        penalty
    }

    /// Check if we're in any degraded mode
    pub fn is_degraded(&self) -> bool {
        self.background_unavailable || self.cache_cold || self.known_actors_failed
    }
}

impl AdaptiveFilter {
    /// Create a new adaptive filter with configuration
    pub async fn new(config: AdaptiveFilterConfig) -> Result<Self> {
        let cache = Arc::new(FilterCache::with_config(
            crate::filter::cache::CacheConfig {
                wallet_cache_size: config.cache.wallet_cache_size,
                wallet_cache_ttl_secs: config.cache.wallet_cache_ttl_secs,
                trade_flow_buffer_size: config.cache.trade_flow_buffer_size,
                ..Default::default()
            },
        ));

        // Load known actors
        cache
            .load_known_actors(
                Some(&config.known_actors.deployers_file),
                Some(&config.known_actors.snipers_file),
                Some(&config.known_actors.trusted_file),
            )
            .await;

        // Check if known actors loaded (files might not exist yet)
        let known_actors_failed = cache.wallet_cache_size() == 0
            && !std::path::Path::new(&config.known_actors.deployers_file).exists();

        // Initialize scoring engine with configured weights
        let mut scoring_engine = ScoringEngine::with_thresholds(config.thresholds.clone());
        scoring_engine.set_weights(config.signal_weights());

        // Initialize degraded mode tracking
        let degraded_mode = DegradedMode {
            background_unavailable: false, // Will be set if workers fail to start
            cache_cold: true,              // Starts cold
            known_actors_failed,
            reason: if known_actors_failed {
                Some("Known actors files not found".to_string())
            } else {
                None
            },
        };

        if degraded_mode.is_degraded() {
            tracing::warn!(
                background = %degraded_mode.background_unavailable,
                cache_cold = %degraded_mode.cache_cold,
                known_actors_failed = %degraded_mode.known_actors_failed,
                "Adaptive filter starting in degraded mode"
            );
        }

        Ok(Self {
            config,
            hot_path_providers: Vec::new(),
            background_providers: Vec::new(),
            cache,
            scoring_engine,
            enrichment: None,
            degraded_mode: Arc::new(RwLock::new(degraded_mode)),
        })
    }

    /// Set the enrichment service for fetching data from Helius
    pub fn set_enrichment(&mut self, service: Arc<EnrichmentService>) {
        self.enrichment = Some(service);
        tracing::info!("Enrichment service configured");
    }

    /// Register a signal provider
    pub fn register_provider(&mut self, provider: Arc<dyn SignalProvider>) {
        if provider.is_hot_path() {
            tracing::debug!(
                provider = provider.name(),
                "Registered hot-path signal provider"
            );
            self.hot_path_providers.push(provider);
        } else {
            tracing::debug!(
                provider = provider.name(),
                "Registered background signal provider"
            );
            self.background_providers.push(provider);
        }
    }

    /// Fast scoring using only hot-path providers (for sniping decisions)
    ///
    /// Target latency: <50ms (may be higher if enrichment is needed)
    pub async fn score_fast(&self, context: &SignalContext) -> ScoringResult {
        let start = Instant::now();

        // Check for protocol errors first (fail-closed)
        if context.mint.is_empty() {
            return ScoringResult::fail_closed("Empty mint address");
        }

        // Enrich token data if enrichment service is available
        if let Some(ref enrichment) = self.enrichment {
            if !self.cache.has_token_data(&context.mint) {
                let enriched = enrichment.enrich_token(context).await;
                if enriched {
                    // Mark cache as warming up (not cold anymore)
                    let mut degraded = self.degraded_mode.write().await;
                    if degraded.cache_cold && self.cache.total_cached_items() > 10 {
                        degraded.cache_cold = false;
                        tracing::info!("Cache warmed up, exiting cold mode");
                    }
                }
            }
        }

        // Collect signals from hot-path providers (parallel)
        let mut signals = Vec::new();

        // Add built-in fast signals (now with enriched data available)
        signals.extend(self.compute_builtin_hot_signals(context).await);

        // Add custom provider signals
        for provider in &self.hot_path_providers {
            let timeout = Duration::from_millis(provider.max_latency_ms());
            match tokio::time::timeout(timeout, provider.compute_token_signals(context)).await {
                Ok(provider_signals) => signals.extend(provider_signals),
                Err(_) => {
                    tracing::warn!(provider = provider.name(), "Hot-path provider timed out");
                    // Add a penalty signal for timeout
                    signals.push(Signal::unavailable(
                        SignalType::WalletHistory,
                        format!("Provider {} timed out", provider.name()),
                    ));
                }
            }
        }

        // Apply degraded mode adjustments
        let mut result = self.scoring_engine.score(signals);
        self.apply_degraded_mode_adjustments(&mut result).await;

        let elapsed = start.elapsed();
        tracing::debug!(
            mint = %context.mint,
            score = %result.score,
            recommendation = ?result.recommendation,
            latency_ms = %elapsed.as_millis(),
            signals = %result.signals.len(),
            "Fast scoring complete"
        );

        result
    }

    /// Full scoring with all providers (for detailed analysis)
    ///
    /// Target latency: <2s
    pub async fn score_full(&self, context: &SignalContext) -> ScoringResult {
        let start = Instant::now();

        // Check for protocol errors first (fail-closed)
        if context.mint.is_empty() {
            return ScoringResult::fail_closed("Empty mint address");
        }

        let mut signals = Vec::new();

        // Add built-in signals
        signals.extend(self.compute_builtin_hot_signals(context).await);

        // Collect from all providers with timeout
        let all_providers: Vec<_> = self
            .hot_path_providers
            .iter()
            .chain(self.background_providers.iter())
            .collect();

        for provider in all_providers {
            let timeout = Duration::from_millis(provider.max_latency_ms());
            match tokio::time::timeout(timeout, provider.compute_token_signals(context)).await {
                Ok(provider_signals) => signals.extend(provider_signals),
                Err(_) => {
                    tracing::warn!(
                        provider = provider.name(),
                        "Provider timed out during full scoring"
                    );
                }
            }
        }

        // Apply degraded mode adjustments
        let mut result = self.scoring_engine.score(signals);
        self.apply_degraded_mode_adjustments(&mut result).await;

        let elapsed = start.elapsed();
        tracing::debug!(
            mint = %context.mint,
            score = %result.score,
            recommendation = ?result.recommendation,
            latency_ms = %elapsed.as_millis(),
            signals = %result.signals.len(),
            "Full scoring complete"
        );

        result
    }

    /// Compute built-in hot-path signals
    async fn compute_builtin_hot_signals(&self, context: &SignalContext) -> Vec<Signal> {
        let mut signals = Vec::new();
        let start = Instant::now();

        // Known deployer check
        if self.cache.is_known_deployer(&context.creator).await {
            signals.push(
                Signal::extreme_risk(SignalType::KnownDeployer, "Known rug deployer")
                    .with_latency(start.elapsed())
                    .with_cached(true),
            );
        } else {
            signals.push(
                Signal::neutral(
                    SignalType::KnownDeployer,
                    "Creator not in deployer blacklist",
                )
                .with_latency(start.elapsed())
                .with_cached(true),
            );
        }

        // Known sniper check (creator being a sniper is suspicious)
        if self.cache.is_known_sniper(&context.creator).await {
            signals.push(
                Signal::new(
                    SignalType::KnownSniper,
                    -0.5,
                    0.9,
                    "Creator is a known sniper wallet",
                )
                .with_latency(start.elapsed())
                .with_cached(true),
            );
        }

        // Basic liquidity signal
        let liquidity_signal = self.compute_liquidity_signal(context);
        signals.push(liquidity_signal);

        // Basic name quality signal (simple heuristics)
        let name_signal = self.compute_name_quality_signal(context);
        signals.push(name_signal);

        // === Enriched data signals (from Helius API) ===

        // Mint authority check (CRITICAL - can mint more tokens)
        if let Some(mint_info) = self.cache.get_mint_info(&context.mint) {
            if mint_info.has_mint_authority() {
                signals.push(
                    Signal::extreme_risk(
                        SignalType::MintAuthority,
                        "FATAL: Mint authority active - creator can mint infinite tokens",
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            } else if mint_info.has_freeze_authority() {
                signals.push(
                    Signal::new(
                        SignalType::FreezeAuthority,
                        -0.7,
                        0.95,
                        "WARNING: Freeze authority active - creator can freeze accounts",
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            } else if mint_info.is_fully_renounced() {
                signals.push(
                    Signal::new(
                        SignalType::MintAuthority,
                        0.3,
                        0.9,
                        "All authorities renounced - safer token",
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            }
        }

        // Creator wallet history analysis
        if let Some(history) = self.cache.get_wallet(&context.creator) {
            // Check if creator is a rug deployer pattern
            if history.is_likely_rug_deployer() {
                signals.push(
                    Signal::extreme_risk(
                        SignalType::WalletHistory,
                        format!(
                            "Creator has deployed {} rugged tokens",
                            history.deployed_rug_count
                        ),
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            } else if history.is_likely_deployer() {
                // Multiple deploys but not necessarily rugs
                signals.push(
                    Signal::new(
                        SignalType::WalletHistory,
                        -0.3,
                        0.7,
                        format!(
                            "Creator has deployed {} tokens previously",
                            history.tokens_deployed
                        ),
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            }

            // Wallet age check - reduced penalty for pump.fun (all wallets are new)
            if history.is_new_wallet(7.0) {
                signals.push(
                    Signal::new(
                        SignalType::WalletAge,
                        -0.1, // Reduced from -0.4 - new wallets are normal on pump.fun
                        0.5,  // Lower confidence - this signal is less meaningful
                        format!(
                            "Creator wallet is new ({:.1} days old)",
                            history.age_days().unwrap_or(0.0)
                        ),
                    )
                    .with_latency(start.elapsed())
                    .with_cached(true),
                );
            } else if let Some(age) = history.age_days() {
                if age > 90.0 {
                    signals.push(
                        Signal::new(
                            SignalType::WalletAge,
                            0.2,
                            0.6,
                            format!("Creator wallet is established ({:.0} days old)", age),
                        )
                        .with_latency(start.elapsed())
                        .with_cached(true),
                    );
                }
            }

            // Win rate check (if they have trading history)
            if history.total_trades > 10 {
                let win_rate = history.win_rate();
                if win_rate > 0.6 {
                    signals.push(
                        Signal::new(
                            SignalType::WalletHistory,
                            0.2,
                            0.7,
                            format!(
                                "Creator has {:.0}% win rate over {} trades",
                                win_rate * 100.0,
                                history.total_trades
                            ),
                        )
                        .with_latency(start.elapsed())
                        .with_cached(true),
                    );
                }
            }
        }

        // Token holder distribution analysis
        if let Some(holders) = self.cache.get_holders(&context.mint) {
            if !holders.is_empty() {
                // Check top holder concentration
                let top_holder_pct = holders.first().map(|h| h.percentage).unwrap_or(0.0);
                let top_5_pct: f64 = holders.iter().take(5).map(|h| h.percentage).sum();

                if top_holder_pct > 50.0 {
                    signals.push(
                        Signal::new(
                            SignalType::HolderConcentration,
                            -0.6,
                            0.9,
                            format!("Highly concentrated: top holder has {:.1}%", top_holder_pct),
                        )
                        .with_latency(start.elapsed())
                        .with_cached(true),
                    );
                } else if top_5_pct > 70.0 {
                    signals.push(
                        Signal::new(
                            SignalType::HolderConcentration,
                            -0.4,
                            0.8,
                            format!("Top 5 holders control {:.1}%", top_5_pct),
                        )
                        .with_latency(start.elapsed())
                        .with_cached(true),
                    );
                } else if holders.len() > 10 && top_holder_pct < 20.0 {
                    signals.push(
                        Signal::new(
                            SignalType::HolderConcentration,
                            0.3,
                            0.7,
                            format!(
                                "Good distribution: {} holders, top holder {:.1}%",
                                holders.len(),
                                top_holder_pct
                            ),
                        )
                        .with_latency(start.elapsed())
                        .with_cached(true),
                    );
                }
            }
        }

        signals
    }

    /// Compute liquidity seeding signal
    fn compute_liquidity_signal(&self, context: &SignalContext) -> Signal {
        let market_cap = context.market_cap_sol;

        // Very low liquidity is suspicious
        if market_cap < 0.1 {
            return Signal::new(
                SignalType::LiquiditySeeding,
                -0.4,
                0.8,
                format!("Very low liquidity: {:.4} SOL", market_cap),
            );
        }

        // Normal range
        if market_cap >= 0.5 && market_cap <= 10.0 {
            return Signal::new(
                SignalType::LiquiditySeeding,
                0.2,
                0.7,
                format!("Normal liquidity: {:.2} SOL", market_cap),
            );
        }

        // High liquidity is slightly positive
        if market_cap > 10.0 {
            return Signal::new(
                SignalType::LiquiditySeeding,
                0.3,
                0.6,
                format!("High liquidity: {:.2} SOL", market_cap),
            );
        }

        Signal::neutral(
            SignalType::LiquiditySeeding,
            format!("Liquidity: {:.4} SOL", market_cap),
        )
    }

    /// Compute basic name quality signal
    fn compute_name_quality_signal(&self, context: &SignalContext) -> Signal {
        let name = &context.name;
        let symbol = &context.symbol;

        // Check for common scam patterns
        let scam_keywords = ["scam", "rug", "honeypot", "free", "airdrop", "1000x"];
        let name_lower = name.to_lowercase();
        let symbol_lower = symbol.to_lowercase();

        for keyword in scam_keywords {
            if name_lower.contains(keyword) || symbol_lower.contains(keyword) {
                return Signal::new(
                    SignalType::NameQuality,
                    -0.7,
                    0.9,
                    format!("Name contains suspicious keyword: {}", keyword),
                );
            }
        }

        // Check for very short or very long names
        if name.len() < 2 || symbol.len() < 2 {
            return Signal::new(SignalType::NameQuality, -0.3, 0.6, "Very short name/symbol");
        }

        if name.len() > 30 {
            return Signal::new(SignalType::NameQuality, -0.2, 0.5, "Unusually long name");
        }

        // Check for all caps (often spam)
        if name
            .chars()
            .filter(|c| c.is_alphabetic())
            .all(|c| c.is_uppercase())
            && name.len() > 4
        {
            return Signal::new(SignalType::NameQuality, -0.1, 0.4, "All caps name");
        }

        // Default neutral
        Signal::neutral(SignalType::NameQuality, "Name appears normal")
    }

    /// Apply degraded mode adjustments to scoring result
    async fn apply_degraded_mode_adjustments(&self, result: &mut ScoringResult) {
        let degraded = self.degraded_mode.read().await;

        if !degraded.is_degraded() {
            return;
        }

        let penalty = degraded.confidence_penalty();
        result.confidence *= penalty;

        // Minimal score offset if known actors failed (was -0.1, now -0.02)
        if degraded.known_actors_failed {
            result.score -= 0.02;
        }

        // Downgrade recommendations under low confidence
        // When uncertain: watch, don't trade
        if result.confidence < self.config.thresholds.min_confidence {
            if matches!(result.recommendation, Recommendation::Opportunity) {
                // Insufficient confidence for full position -> Observe
                result.recommendation = Recommendation::Observe;
                result.position_size_multiplier = 0.0;
            } else if matches!(result.recommendation, Recommendation::StrongBuy) {
                // Downgrade to Opportunity
                result.recommendation = Recommendation::Opportunity;
            } else if matches!(result.recommendation, Recommendation::Probe) {
                // Even probe needs some confidence -> Observe
                result.recommendation = Recommendation::Observe;
                result.position_size_multiplier = 0.0;
            }
        }

        result.summary = format!(
            "{} [DEGRADED MODE: conf penalty {:.0}%]",
            result.summary,
            (1.0 - penalty) * 100.0
        );
    }

    /// Get the shared cache
    pub fn cache(&self) -> &Arc<FilterCache> {
        &self.cache
    }

    /// Check if running in degraded mode
    pub async fn is_degraded(&self) -> bool {
        self.degraded_mode.read().await.is_degraded()
    }

    /// Mark cache as warm (after initial fill)
    pub async fn mark_cache_warm(&self) {
        self.degraded_mode.write().await.cache_cold = false;
    }

    /// Get configuration
    pub fn config(&self) -> &AdaptiveFilterConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_adaptive_filter_creation() {
        let config = AdaptiveFilterConfig::default();
        let filter = AdaptiveFilter::new(config).await.unwrap();

        // Should start in degraded mode (cache cold, files missing)
        assert!(filter.is_degraded().await);
    }

    #[tokio::test]
    async fn test_fast_scoring() {
        let config = AdaptiveFilterConfig::default();
        let filter = AdaptiveFilter::new(config).await.unwrap();

        let context = SignalContext::from_new_token(
            "TestMint123".to_string(),
            "Test Token".to_string(),
            "TEST".to_string(),
            "https://example.com/meta.json".to_string(),
            "Creator123".to_string(),
            "BondingCurve123".to_string(),
            1000,
            1_000_000_000,
            100_000_000,
            1.0,
        );

        let result = filter.score_fast(&context).await;

        // Should have some signals
        assert!(!result.signals.is_empty());

        // Score should be in valid range
        assert!(result.score >= -1.0 && result.score <= 1.0);
    }

    #[tokio::test]
    async fn test_scam_name_detection() {
        let config = AdaptiveFilterConfig::default();
        let filter = AdaptiveFilter::new(config).await.unwrap();

        let context = SignalContext::from_new_token(
            "ScamMint".to_string(),
            "FREE MONEY SCAM".to_string(),
            "SCAM".to_string(),
            "https://example.com/meta.json".to_string(),
            "Creator123".to_string(),
            "BondingCurve123".to_string(),
            1000,
            1_000_000_000,
            100_000_000,
            1.0,
        );

        let result = filter.score_fast(&context).await;

        // Should have negative score due to scam keywords
        assert!(result.score < 0.0);
    }

    #[tokio::test]
    async fn test_fail_closed() {
        let config = AdaptiveFilterConfig::default();
        let filter = AdaptiveFilter::new(config).await.unwrap();

        let context = SignalContext::from_new_token(
            "".to_string(), // Empty mint - should fail closed
            "Test".to_string(),
            "TST".to_string(),
            "uri".to_string(),
            "creator".to_string(),
            "curve".to_string(),
            0,
            0,
            0,
            0.0,
        );

        let result = filter.score_fast(&context).await;

        assert_eq!(result.score, -1.0);
        assert_eq!(result.recommendation, Recommendation::Avoid);
    }
}
