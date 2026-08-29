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
    /// Market cap below minimum
    MarketCapBelowMinimum(f64),
    /// Bonding curve below minimum (too new)
    BondingCurveTooLow(f64),
    /// Bonding curve above maximum (too close to graduation)
    BondingCurveTooHigh(f64),
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
            FilterReason::MarketCapBelowMinimum(sol) => {
                write!(f, "market cap {:.2} SOL below minimum", sol)
            }
            FilterReason::BondingCurveTooLow(pct) => {
                write!(f, "bonding curve {:.1}% below minimum (too new)", pct)
            }
            FilterReason::BondingCurveTooHigh(pct) => {
                write!(f, "bonding curve {:.1}% above maximum (near graduation)", pct)
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

    /// Filter a token by name and symbol only.
    ///
    /// This is the canonical regex/name/symbol matching entry point. It lets the
    /// live PumpPortal path filter on the provider-validated name/symbol WITHOUT
    /// fabricating a slot or synthetic Pubkeys. Identity validation stays in the
    /// stream parser; this only inspects text.
    pub fn filter_name_symbol(&self, name: &str, symbol: &str) -> FilterResult {
        // Check if filtering is enabled
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        // Check blocked patterns first
        for pattern in &self.blocked_patterns {
            if pattern.is_match(name) || pattern.is_match(symbol) {
                debug!(
                    "Token {} ({}) blocked by pattern: {}",
                    name, symbol, pattern
                );
                return FilterResult::Filtered(FilterReason::BlockedName(pattern.to_string()));
            }
        }

        // Check name patterns (if any configured)
        if !self.name_patterns.is_empty() {
            let matches = self
                .name_patterns
                .iter()
                .any(|p| p.is_match(name) || p.is_match(symbol));

            if !matches {
                debug!(
                    "Token {} ({}) doesn't match required patterns",
                    name, symbol
                );
                return FilterResult::Filtered(FilterReason::NamePatternMismatch);
            }
        }

        // Note: Dev holdings and liquidity checks would require RPC calls
        // to fetch on-chain data. These are checked separately.

        debug!("Token {} ({}) passed filters", name, symbol);
        FilterResult::Pass
    }

    /// Filter a newly created token.
    ///
    /// Compatibility wrapper: `TokenFilter::filter` only ever consulted the token
    /// name and symbol, so it now delegates to `filter_name_symbol`.
    pub fn filter(&self, event: &TokenCreatedEvent) -> FilterResult {
        self.filter_name_symbol(&event.name, &event.symbol)
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

    /// Check market cap (in SOL equivalent)
    pub fn check_market_cap(&self, market_cap_sol: f64) -> FilterResult {
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        if self.config.min_market_cap_sol > 0.0 && market_cap_sol < self.config.min_market_cap_sol {
            return FilterResult::Filtered(FilterReason::MarketCapBelowMinimum(market_cap_sol));
        }

        FilterResult::Pass
    }

    /// Check bonding curve progress (0-100%)
    pub fn check_bonding_curve(&self, bonding_curve_pct: f64) -> FilterResult {
        if !self.config.enabled {
            return FilterResult::Pass;
        }

        // Check minimum (filter too new tokens)
        if self.config.min_bonding_curve_pct > 0.0 && bonding_curve_pct < self.config.min_bonding_curve_pct {
            return FilterResult::Filtered(FilterReason::BondingCurveTooLow(bonding_curve_pct));
        }

        // Check maximum (filter tokens too close to graduation)
        if self.config.max_bonding_curve_pct > 0.0 && bonding_curve_pct > self.config.max_bonding_curve_pct {
            return FilterResult::Filtered(FilterReason::BondingCurveTooHigh(bonding_curve_pct));
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

    /// Check all on-chain criteria including market cap and bonding curve
    pub fn check_on_chain_full(
        &self,
        dev_holdings_pct: f64,
        liquidity_sol: f64,
        market_cap_sol: Option<f64>,
        bonding_curve_pct: Option<f64>,
    ) -> FilterResult {
        // Basic checks
        if let FilterResult::Filtered(reason) = self.check_on_chain(dev_holdings_pct, liquidity_sol) {
            return FilterResult::Filtered(reason);
        }

        // Market cap check (if provided)
        if let Some(mcap) = market_cap_sol {
            if let FilterResult::Filtered(reason) = self.check_market_cap(mcap) {
                return FilterResult::Filtered(reason);
            }
        }

        // Bonding curve check (if provided)
        if let Some(bc_pct) = bonding_curve_pct {
            if let FilterResult::Filtered(reason) = self.check_bonding_curve(bc_pct) {
                return FilterResult::Filtered(reason);
            }
        }

        FilterResult::Pass
    }

    /// Get minimum bonding curve requirement (for external use)
    pub fn min_bonding_curve_pct(&self) -> f64 {
        self.config.min_bonding_curve_pct
    }

    /// Get minimum market cap requirement (for external use)
    pub fn min_market_cap_sol(&self) -> f64 {
        self.config.min_market_cap_sol
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
            ..crate::config::Config::default().filters
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

    #[test]
    fn test_filter_name_symbol_matches_existing_event_filter() {
        let filter = TokenFilter::new(test_config()).unwrap();

        // Passing case
        let ev = test_event("GoodToken", "GOOD");
        assert_eq!(
            filter.filter(&ev).is_pass(),
            filter.filter_name_symbol(&ev.name, &ev.symbol).is_pass()
        );

        // Blocked case
        let ev = test_event("ScamCoin", "SCAM");
        assert_eq!(
            filter.filter(&ev).is_filtered(),
            filter.filter_name_symbol(&ev.name, &ev.symbol).is_filtered()
        );
        assert!(filter.filter_name_symbol(&ev.name, &ev.symbol).is_filtered());
    }

    #[test]
    fn test_filter_name_symbol_blocked_pattern() {
        let filter = TokenFilter::new(test_config()).unwrap();
        // "rug" is a blocked pattern (case-insensitive).
        let result = filter.filter_name_symbol("SuperRugPull", "RUGX");
        assert!(result.is_filtered());
    }

    #[test]
    fn test_filter_name_symbol_required_pattern() {
        let mut config = test_config();
        // Require the name/symbol to contain "cat" (case-insensitive).
        config.name_patterns = vec!["(?i)cat".to_string()];
        let filter = TokenFilter::new(config).unwrap();

        // Matches required pattern -> pass
        assert!(filter.filter_name_symbol("CatCoin", "CAT").is_pass());
        // Does not match required pattern -> filtered
        assert!(filter.filter_name_symbol("DogCoin", "DOG").is_filtered());
    }
}
