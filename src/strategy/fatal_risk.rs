//! Fatal Risk Engine - Kill Switch
//!
//! Immediate rejection of toxic tokens before any analysis.
//! If ANY fatal flag triggers, the token is rejected with no further processing.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

use crate::filter::FilterCache;

/// Fatal risk flags that trigger immediate rejection
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FatalRisk {
    // === Creator risks ===
    /// Creator can mint more tokens
    MintAuthorityActive,

    /// Creator can freeze accounts
    FreezeAuthorityActive,

    /// Creator is in known rug deployer blacklist
    KnownRugDeployer { prior_rugs: u32 },

    /// Creator sold significant portion early
    CreatorDumpedEarly { pct_sold: f64, within_secs: u64 },

    // === Liquidity risks ===
    /// Liquidity dropped suddenly
    LiquidityCollapsed { drop_pct: f64 },

    /// Cannot exit without extreme slippage
    ExitImpossible {
        min_exit_sol: f64,
        slippage_pct: f64,
    },

    /// Liquidity too low for minimum position
    InsufficientLiquidity {
        available_sol: f64,
        required_sol: f64,
    },

    // === Pattern risks ===
    /// Confirmed wash trading (>80% score)
    WashTradingConfirmed { wash_pct: f64 },

    /// Known rug pattern detected
    BundledRugPattern {
        pattern_name: String,
        confidence: f64,
    },

    /// Sell transactions failing (honeypot)
    HoneypotDetected { failed_sells: u32 },

    /// Token is already rugged
    AlreadyRugged { price_drop_pct: f64 },

    // === Chain risks ===
    /// Chain congestion too severe
    ChainCongestionCritical,

    /// Token blacklisted
    TokenBlacklisted { reason: String },
}

impl FatalRisk {
    /// Get human-readable description
    pub fn description(&self) -> String {
        match self {
            FatalRisk::MintAuthorityActive => {
                "Creator retains mint authority - can create unlimited tokens".to_string()
            }
            FatalRisk::FreezeAuthorityActive => {
                "Creator retains freeze authority - can freeze your tokens".to_string()
            }
            FatalRisk::KnownRugDeployer { prior_rugs } => {
                format!("Known rug deployer with {} prior rugs", prior_rugs)
            }
            FatalRisk::CreatorDumpedEarly {
                pct_sold,
                within_secs,
            } => {
                format!(
                    "Creator dumped {:.1}% within {}s of launch",
                    pct_sold, within_secs
                )
            }
            FatalRisk::LiquidityCollapsed { drop_pct } => {
                format!("Liquidity collapsed by {:.1}%", drop_pct)
            }
            FatalRisk::ExitImpossible {
                min_exit_sol,
                slippage_pct,
            } => {
                format!(
                    "Cannot exit {:.3} SOL without {:.1}% slippage",
                    min_exit_sol, slippage_pct
                )
            }
            FatalRisk::InsufficientLiquidity {
                available_sol,
                required_sol,
            } => {
                format!(
                    "Liquidity {:.3} SOL below minimum {:.3} SOL",
                    available_sol, required_sol
                )
            }
            FatalRisk::WashTradingConfirmed { wash_pct } => {
                format!("Wash trading confirmed at {:.1}%", wash_pct)
            }
            FatalRisk::BundledRugPattern {
                pattern_name,
                confidence,
            } => {
                format!(
                    "Rug pattern '{}' detected ({:.0}% confidence)",
                    pattern_name,
                    confidence * 100.0
                )
            }
            FatalRisk::HoneypotDetected { failed_sells } => {
                format!(
                    "Honeypot detected - {} sell transactions failed",
                    failed_sells
                )
            }
            FatalRisk::AlreadyRugged { price_drop_pct } => {
                format!(
                    "Token already rugged - price dropped {:.1}%",
                    price_drop_pct
                )
            }
            FatalRisk::ChainCongestionCritical => {
                "Solana chain congestion is critical - cannot execute safely".to_string()
            }
            FatalRisk::TokenBlacklisted { reason } => {
                format!("Token blacklisted: {}", reason)
            }
        }
    }

    /// Check if this is a creator-related risk
    pub fn is_creator_risk(&self) -> bool {
        matches!(
            self,
            FatalRisk::MintAuthorityActive
                | FatalRisk::FreezeAuthorityActive
                | FatalRisk::KnownRugDeployer { .. }
                | FatalRisk::CreatorDumpedEarly { .. }
        )
    }

    /// Check if this is a liquidity-related risk
    pub fn is_liquidity_risk(&self) -> bool {
        matches!(
            self,
            FatalRisk::LiquidityCollapsed { .. }
                | FatalRisk::ExitImpossible { .. }
                | FatalRisk::InsufficientLiquidity { .. }
        )
    }
}

/// Configuration for fatal risk checking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FatalRiskConfig {
    /// Check for active mint authority
    pub check_mint_authority: bool,
    /// Check for active freeze authority
    pub check_freeze_authority: bool,
    /// Max creator dump percentage before fatal
    pub max_creator_dump_pct: f64,
    /// Time window for creator dump check (seconds)
    pub max_creator_dump_secs: u64,
    /// Minimum exit liquidity in SOL
    pub min_exit_liquidity_sol: f64,
    /// Max slippage percentage before exit impossible
    pub max_exit_slippage_pct: f64,
    /// Wash trading threshold for fatal flag
    pub wash_trading_threshold: f64,
    /// Price drop threshold for already rugged
    pub rug_price_drop_threshold: f64,
}

impl Default for FatalRiskConfig {
    fn default() -> Self {
        Self {
            check_mint_authority: true,
            check_freeze_authority: true,
            max_creator_dump_pct: 20.0,
            max_creator_dump_secs: 60,
            min_exit_liquidity_sol: 0.05,
            max_exit_slippage_pct: 50.0,
            wash_trading_threshold: 0.8,
            rug_price_drop_threshold: 80.0,
        }
    }
}

/// Fatal Risk Engine - checks for immediate rejection conditions
pub struct FatalRiskEngine {
    config: FatalRiskConfig,
    cache: Option<Arc<FilterCache>>,
    /// Additional blacklisted tokens
    blacklisted_tokens: HashSet<String>,
    /// Known rug deployer addresses
    known_rug_deployers: HashSet<String>,
}

impl FatalRiskEngine {
    /// Create a new fatal risk engine with cache
    pub fn with_cache(config: FatalRiskConfig, cache: Arc<FilterCache>) -> Self {
        Self {
            config,
            cache: Some(cache),
            blacklisted_tokens: HashSet::new(),
            known_rug_deployers: HashSet::new(),
        }
    }

    /// Create a new fatal risk engine without cache
    pub fn new(config: FatalRiskConfig) -> Self {
        Self {
            config,
            cache: None,
            blacklisted_tokens: HashSet::new(),
            known_rug_deployers: HashSet::new(),
        }
    }

    /// Set the filter cache
    pub fn set_cache(&mut self, cache: Arc<FilterCache>) {
        self.cache = Some(cache);
    }

    /// Add a known rug deployer address
    pub fn add_rug_deployer(&mut self, address: String) {
        self.known_rug_deployers.insert(address);
    }

    /// Add a token to the blacklist
    pub fn blacklist_token(&mut self, mint: String, reason: String) {
        tracing::warn!("Blacklisting token {}: {}", mint, reason);
        self.blacklisted_tokens.insert(mint);
    }

    /// Check all fatal risk conditions
    pub async fn check(&self, context: &FatalRiskContext) -> Option<FatalRisk> {
        // Check blacklist first (fastest)
        if self.blacklisted_tokens.contains(&context.mint) {
            return Some(FatalRisk::TokenBlacklisted {
                reason: "Previously blacklisted".to_string(),
            });
        }

        // Check known deployer from local set
        if self.known_rug_deployers.contains(&context.creator) {
            return Some(FatalRisk::KnownRugDeployer { prior_rugs: 0 });
        }

        // Check known deployer blacklist from cache
        if let Some(cache) = &self.cache {
            if cache.is_known_deployer(&context.creator).await {
                if let Some(history) = cache.get_wallet(&context.creator) {
                    if history.deployed_rug_count > 0 {
                        return Some(FatalRisk::KnownRugDeployer {
                            prior_rugs: history.deployed_rug_count,
                        });
                    }
                }
                // In blacklist but no history - still fatal
                return Some(FatalRisk::KnownRugDeployer { prior_rugs: 0 });
            }
        }

        // Check mint authority
        if self.config.check_mint_authority && context.mint_authority_active {
            return Some(FatalRisk::MintAuthorityActive);
        }

        // Check freeze authority
        if self.config.check_freeze_authority && context.freeze_authority_active {
            return Some(FatalRisk::FreezeAuthorityActive);
        }

        // Check creator dump
        if let Some((pct_sold, within_secs)) = context.creator_sell_info {
            if pct_sold > self.config.max_creator_dump_pct
                && within_secs < self.config.max_creator_dump_secs
            {
                return Some(FatalRisk::CreatorDumpedEarly {
                    pct_sold,
                    within_secs,
                });
            }
        }

        // Check liquidity
        if context.effective_liquidity_sol < self.config.min_exit_liquidity_sol {
            return Some(FatalRisk::InsufficientLiquidity {
                available_sol: context.effective_liquidity_sol,
                required_sol: self.config.min_exit_liquidity_sol,
            });
        }

        // Check exit slippage
        if context.exit_slippage_pct > self.config.max_exit_slippage_pct {
            return Some(FatalRisk::ExitImpossible {
                min_exit_sol: context.min_position_sol,
                slippage_pct: context.exit_slippage_pct,
            });
        }

        // Check liquidity collapse
        if let Some(drop_pct) = context.liquidity_drop_pct {
            if drop_pct > 50.0 {
                return Some(FatalRisk::LiquidityCollapsed { drop_pct });
            }
        }

        // Check wash trading
        if context.wash_trading_score > self.config.wash_trading_threshold {
            return Some(FatalRisk::WashTradingConfirmed {
                wash_pct: context.wash_trading_score * 100.0,
            });
        }

        // Check honeypot
        if context.failed_sell_count > 2 {
            return Some(FatalRisk::HoneypotDetected {
                failed_sells: context.failed_sell_count,
            });
        }

        // Check already rugged
        if context.price_drop_from_ath > self.config.rug_price_drop_threshold {
            return Some(FatalRisk::AlreadyRugged {
                price_drop_pct: context.price_drop_from_ath,
            });
        }

        // Check chain congestion
        if context.chain_congestion_critical {
            return Some(FatalRisk::ChainCongestionCritical);
        }

        // No fatal risks found
        None
    }

    /// Quick check for known bad actors only (for hot path)
    pub async fn quick_check(&self, mint: &str, creator: &str) -> Option<FatalRisk> {
        // Check blacklist
        if self.blacklisted_tokens.contains(mint) {
            return Some(FatalRisk::TokenBlacklisted {
                reason: "Previously blacklisted".to_string(),
            });
        }

        // Check known deployer from local set
        if self.known_rug_deployers.contains(creator) {
            return Some(FatalRisk::KnownRugDeployer { prior_rugs: 0 });
        }

        // Check known deployer from cache
        if let Some(cache) = &self.cache {
            if cache.is_known_deployer(creator).await {
                return Some(FatalRisk::KnownRugDeployer { prior_rugs: 0 });
            }
        }

        None
    }
}

/// Context for fatal risk checking
#[derive(Debug, Clone, Default)]
pub struct FatalRiskContext {
    pub mint: String,
    pub creator: String,

    // Authority flags
    pub mint_authority_active: bool,
    pub freeze_authority_active: bool,

    // Creator behavior
    /// (pct_sold, within_secs) if creator has sold
    pub creator_sell_info: Option<(f64, u64)>,

    // Liquidity
    pub effective_liquidity_sol: f64,
    pub exit_slippage_pct: f64,
    pub min_position_sol: f64,
    pub liquidity_drop_pct: Option<f64>,

    // Patterns
    pub wash_trading_score: f64,
    pub failed_sell_count: u32,
    pub price_drop_from_ath: f64,

    // Chain
    pub chain_congestion_critical: bool,
}

impl FatalRiskContext {
    /// Create a new context for a token
    pub fn new(mint: String, creator: String) -> Self {
        Self {
            mint,
            creator,
            ..Default::default()
        }
    }

    /// Set authority flags
    pub fn with_authorities(mut self, mint_active: bool, freeze_active: bool) -> Self {
        self.mint_authority_active = mint_active;
        self.freeze_authority_active = freeze_active;
        self
    }

    /// Set liquidity info
    pub fn with_liquidity(mut self, liquidity_sol: f64, exit_slippage: f64, min_pos: f64) -> Self {
        self.effective_liquidity_sol = liquidity_sol;
        self.exit_slippage_pct = exit_slippage;
        self.min_position_sol = min_pos;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache() -> Arc<FilterCache> {
        Arc::new(FilterCache::new())
    }

    #[tokio::test]
    async fn test_mint_authority_fatal() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_authorities(true, false);

        let result = engine.check(&context).await;
        assert!(matches!(result, Some(FatalRisk::MintAuthorityActive)));
    }

    #[tokio::test]
    async fn test_freeze_authority_fatal() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_authorities(false, true);

        let result = engine.check(&context).await;
        assert!(matches!(result, Some(FatalRisk::FreezeAuthorityActive)));
    }

    #[tokio::test]
    async fn test_known_deployer_fatal() {
        let cache = make_cache();
        cache.add_known_deployer("bad_creator".to_string()).await;

        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "bad_creator".to_string());

        let result = engine.check(&context).await;
        assert!(matches!(result, Some(FatalRisk::KnownRugDeployer { .. })));
    }

    #[tokio::test]
    async fn test_insufficient_liquidity_fatal() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_liquidity(0.01, 5.0, 0.1); // Only 0.01 SOL liquidity

        let result = engine.check(&context).await;
        assert!(matches!(
            result,
            Some(FatalRisk::InsufficientLiquidity { .. })
        ));
    }

    #[tokio::test]
    async fn test_exit_impossible_fatal() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_liquidity(1.0, 60.0, 0.1); // 60% slippage

        let result = engine.check(&context).await;
        assert!(matches!(result, Some(FatalRisk::ExitImpossible { .. })));
    }

    #[tokio::test]
    async fn test_wash_trading_fatal() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let mut context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_liquidity(1.0, 5.0, 0.1);
        context.wash_trading_score = 0.85;

        let result = engine.check(&context).await;
        assert!(matches!(
            result,
            Some(FatalRisk::WashTradingConfirmed { .. })
        ));
    }

    #[tokio::test]
    async fn test_no_fatal_risks() {
        let cache = make_cache();
        let engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        let context = FatalRiskContext::new("mint".to_string(), "creator".to_string())
            .with_authorities(false, false)
            .with_liquidity(1.0, 5.0, 0.1);

        let result = engine.check(&context).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_blacklist() {
        let cache = make_cache();
        let mut engine = FatalRiskEngine::with_cache(FatalRiskConfig::default(), cache);

        engine.blacklist_token("bad_mint".to_string(), "Test".to_string());

        let context = FatalRiskContext::new("bad_mint".to_string(), "creator".to_string());
        let result = engine.check(&context).await;
        assert!(matches!(result, Some(FatalRisk::TokenBlacklisted { .. })));
    }

    #[test]
    fn test_fatal_risk_descriptions() {
        let risk = FatalRisk::MintAuthorityActive;
        assert!(!risk.description().is_empty());
        assert!(risk.is_creator_risk());
        assert!(!risk.is_liquidity_risk());

        let risk = FatalRisk::ExitImpossible {
            min_exit_sol: 0.1,
            slippage_pct: 60.0,
        };
        assert!(!risk.is_creator_risk());
        assert!(risk.is_liquidity_risk());
    }
}
