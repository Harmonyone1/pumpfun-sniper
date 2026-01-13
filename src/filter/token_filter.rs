//! Token filtering logic
//!
//! Filters new tokens based on configurable criteria to avoid
//! sniping low-quality or suspicious tokens.

use regex::Regex;
use tracing::debug;

use crate::config::FilterConfig;
use crate::error::{Error, Result};
use crate::stream::decoder::TokenCreatedEvent;

/// Reason why a token was filtered
#[derive(Debug, Clone)]
pub enum FilterReason {
    /// Filtering is disabled
    Disabled,
    /// Token name matches blocked pattern
    BlockedName(String),
    /// Token name doesn't match required pattern
    NamePatternMismatch,
    /// Dev holdings exceed maximum
    DevHoldingsExceeded(f64),
    /// Liquidity below minimum
    LiquidityBelowMinimum(f64),
    /// Custom filter failed
    Custom(String),
}

impl std::fmt::Display for FilterReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterReason::Disabled => write!(f, "filtering disabled"),
            FilterReason::BlockedName(pattern) => {
                write!(f, "name matches blocked pattern: {}", pattern)
            }
            FilterReason::NamePatternMismatch => write!(f, "name doesn't match required patterns"),
            FilterReason::DevHoldingsExceeded(pct) => write!(f, "dev holdings {}% exceed max", pct),
            FilterReason::LiquidityBelowMinimum(sol) => {
                write!(f, "liquidity {} SOL below minimum", sol)
            }
            FilterReason::Custom(reason) => write!(f, "{}", reason),
        }
    }
}

/// Filter result
#[derive(Debug, Clone)]
pub enum FilterResult {
    /// Token passed all filters
    Pass,
    /// Token was filtered
    Filtered(FilterReason),
}

impl FilterResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, FilterResult::Pass)
    }

    pub fn is_filtered(&self) -> bool {
        matches!(self, FilterResult::Filtered(_))
    }
}

/// Token filter based on configuration
pub struct TokenFilter {
    config: FilterConfig,
    name_patterns: Vec<Regex>,
    blocked_patterns: Vec<Regex>,
}

impl TokenFilter {
    /// Create a new token filter from config
    pub fn new(config: FilterConfig) -> Result<Self> {
        let name_patterns = config
            .name_patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::InvalidRegex(e.to_string()))?;

        let blocked_patterns = config
            .blocked_patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::InvalidRegex(e.to_string()))?;

        Ok(Self {
            config,
            name_patterns,
            blocked_patterns,
        })
    }

    /// Filter a newly created token
    pub fn filter(&self, event: &TokenCreatedEvent) -> FilterResult {
        // Check if filtering is enabled
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        // Check blocked patterns first
        for pattern in &self.blocked_patterns {
            if pattern.is_match(&event.name) || pattern.is_match(&event.symbol) {
                debug!(
                    "Token {} ({}) blocked by pattern: {}",
                    event.name, event.symbol, pattern
                );
                return FilterResult::Filtered(FilterReason::BlockedName(pattern.to_string()));
            }
        }

        // Check name patterns (if any configured)
        if !self.name_patterns.is_empty() {
            let matches = self
                .name_patterns
                .iter()
                .any(|p| p.is_match(&event.name) || p.is_match(&event.symbol));

            if !matches {
                debug!(
                    "Token {} ({}) doesn't match required patterns",
                    event.name, event.symbol
                );
                return FilterResult::Filtered(FilterReason::NamePatternMismatch);
            }
        }

        // Note: Dev holdings and liquidity checks would require RPC calls
        // to fetch on-chain data. These are checked separately.

        debug!("Token {} ({}) passed filters", event.name, event.symbol);
        FilterResult::Pass
    }

    /// Check dev holdings percentage
    /// Returns FilterResult based on dev holdings
    pub fn check_dev_holdings(&self, dev_holdings_pct: f64) -> FilterResult {
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        if dev_holdings_pct > self.config.max_dev_holdings_pct {
            return FilterResult::Filtered(FilterReason::DevHoldingsExceeded(dev_holdings_pct));
        }

        FilterResult::Pass
    }

    /// Check initial liquidity
    /// Returns FilterResult based on liquidity
    pub fn check_liquidity(&self, liquidity_sol: f64) -> FilterResult {
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        if liquidity_sol < self.config.min_liquidity_sol {
            return FilterResult::Filtered(FilterReason::LiquidityBelowMinimum(liquidity_sol));
        }

        FilterResult::Pass
    }

    /// Check all on-chain criteria
    pub fn check_on_chain(&self, dev_holdings_pct: f64, liquidity_sol: f64) -> FilterResult {
        if let FilterResult::Filtered(reason) = self.check_dev_holdings(dev_holdings_pct) {
            return FilterResult::Filtered(reason);
        }

        if let FilterResult::Filtered(reason) = self.check_liquidity(liquidity_sol) {
            return FilterResult::Filtered(reason);
        }

        FilterResult::Pass
    }

    /// Is filtering enabled?
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn test_config() -> FilterConfig {
        FilterConfig {
            enabled: true,
            min_liquidity_sol: 0.0,
            max_dev_holdings_pct: 20.0,
            name_patterns: vec![],
            // Use case-insensitive regex patterns
            blocked_patterns: vec!["(?i)scam".to_string(), "(?i)rug".to_string()],
        }
    }

    fn test_event(name: &str, symbol: &str) -> TokenCreatedEvent {
        TokenCreatedEvent {
            signature: "test".to_string(),
            slot: 0,
            mint: Pubkey::new_unique(),
            name: name.to_string(),
            symbol: symbol.to_string(),
            uri: "https://example.com".to_string(),
            bonding_curve: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_blocked_pattern() {
        let filter = TokenFilter::new(test_config()).unwrap();
        let event = test_event("ScamCoin", "SCAM");

        let result = filter.filter(&event);
        assert!(result.is_filtered());
    }

    #[test]
    fn test_pass_filter() {
        let filter = TokenFilter::new(test_config()).unwrap();
        let event = test_event("GoodToken", "GOOD");

        let result = filter.filter(&event);
        assert!(result.is_pass());
    }

    #[test]
    fn test_dev_holdings_check() {
        let filter = TokenFilter::new(test_config()).unwrap();

        assert!(filter.check_dev_holdings(15.0).is_pass());
        assert!(filter.check_dev_holdings(25.0).is_filtered());
    }
}
