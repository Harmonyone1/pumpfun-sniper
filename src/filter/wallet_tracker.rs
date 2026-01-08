//! Wallet tracking for copy-trading
//!
//! Monitors specific wallet addresses and prioritizes their trades.

use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;
use tracing::info;

use crate::config::WalletTrackingConfig;
use crate::error::{Error, Result};
use crate::stream::decoder::TokenTradeEvent;

/// Wallet tracker for copy-trading
pub struct WalletTracker {
    config: WalletTrackingConfig,
    tracked_wallets: HashSet<Pubkey>,
}

impl WalletTracker {
    /// Create a new wallet tracker from config
    pub fn new(config: WalletTrackingConfig) -> Result<Self> {
        let tracked_wallets = config
            .wallets
            .iter()
            .map(|w| Pubkey::from_str(w))
            .collect::<std::result::Result<HashSet<_>, _>>()
            .map_err(|e| Error::Config(format!("Invalid wallet address: {}", e)))?;

        info!("Wallet tracker initialized with {} wallets", tracked_wallets.len());

        Ok(Self {
            config,
            tracked_wallets,
        })
    }

    /// Check if wallet tracking is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && !self.tracked_wallets.is_empty()
    }

    /// Check if a wallet is being tracked
    pub fn is_tracked(&self, wallet: &Pubkey) -> bool {
        self.tracked_wallets.contains(wallet)
    }

    /// Check if a trade event is from a tracked wallet
    pub fn is_tracked_trade(&self, event: &TokenTradeEvent) -> bool {
        if !self.is_enabled() {
            return false;
        }

        self.is_tracked(&event.trader)
    }

    /// Check if priority boost should be applied
    pub fn should_boost_priority(&self, wallet: &Pubkey) -> bool {
        self.config.priority_boost && self.is_tracked(wallet)
    }

    /// Add a wallet to track
    pub fn add_wallet(&mut self, wallet: Pubkey) {
        self.tracked_wallets.insert(wallet);
        info!("Added wallet to tracker: {}", wallet);
    }

    /// Remove a wallet from tracking
    pub fn remove_wallet(&mut self, wallet: &Pubkey) -> bool {
        let removed = self.tracked_wallets.remove(wallet);
        if removed {
            info!("Removed wallet from tracker: {}", wallet);
        }
        removed
    }

    /// Get all tracked wallets
    pub fn get_wallets(&self) -> Vec<Pubkey> {
        self.tracked_wallets.iter().cloned().collect()
    }

    /// Get number of tracked wallets
    pub fn wallet_count(&self) -> usize {
        self.tracked_wallets.len()
    }
}

/// Event when a tracked wallet makes a trade
#[derive(Debug, Clone)]
pub struct TrackedWalletEvent {
    /// The tracked wallet address
    pub wallet: Pubkey,
    /// The token being traded
    pub token: Pubkey,
    /// Is this a buy (true) or sell (false)
    pub is_buy: bool,
    /// Amount of tokens
    pub token_amount: u64,
    /// SOL amount
    pub sol_amount: u64,
    /// Transaction signature
    pub signature: String,
}

impl From<&TokenTradeEvent> for TrackedWalletEvent {
    fn from(event: &TokenTradeEvent) -> Self {
        Self {
            wallet: event.trader,
            token: event.mint,
            is_buy: event.is_buy,
            token_amount: event.token_amount,
            sol_amount: event.sol_amount,
            signature: event.signature.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> WalletTrackingConfig {
        WalletTrackingConfig {
            enabled: true,
            wallets: vec![
                "DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK".to_string(),
            ],
            priority_boost: true,
        }
    }

    #[test]
    fn test_wallet_tracking() {
        let tracker = WalletTracker::new(test_config()).unwrap();

        let tracked = Pubkey::from_str("DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK").unwrap();
        let not_tracked = Pubkey::new_unique();

        assert!(tracker.is_tracked(&tracked));
        assert!(!tracker.is_tracked(&not_tracked));
    }

    #[test]
    fn test_add_remove_wallet() {
        let mut tracker = WalletTracker::new(WalletTrackingConfig {
            enabled: true,
            wallets: vec![],
            priority_boost: true,
        })
        .unwrap();

        let wallet = Pubkey::new_unique();

        assert!(!tracker.is_tracked(&wallet));

        tracker.add_wallet(wallet);
        assert!(tracker.is_tracked(&wallet));

        tracker.remove_wallet(&wallet);
        assert!(!tracker.is_tracked(&wallet));
    }
}
