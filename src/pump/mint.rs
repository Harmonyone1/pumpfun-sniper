//! Mint account utilities
//!
//! Handles dynamic reading of token decimals from mint accounts.
//! This is important because while pump.fun typically uses 6 decimals,
//! we should not assume this for all tokens.

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::{Error, Result};

// Cache for mint decimals to avoid repeated RPC calls
// Key: mint pubkey as string, Value: decimals
lazy_static::lazy_static! {
    static ref DECIMALS_CACHE: RwLock<HashMap<String, u8>> = RwLock::new(HashMap::new());
}

/// Default decimals for pump.fun tokens
pub const DEFAULT_DECIMALS: u8 = 6;

/// Get decimals for a mint, with caching
pub fn get_decimals(rpc_client: &RpcClient, mint: &Pubkey) -> Result<u8> {
    let mint_str = mint.to_string();

    // Check cache first
    {
        let cache = DECIMALS_CACHE
            .read()
            .map_err(|e| Error::Internal(format!("Failed to acquire cache read lock: {}", e)))?;

        if let Some(&decimals) = cache.get(&mint_str) {
            return Ok(decimals);
        }
    }

    // Fetch from RPC
    let decimals = fetch_decimals(rpc_client, mint)?;

    // Update cache
    {
        let mut cache = DECIMALS_CACHE
            .write()
            .map_err(|e| Error::Internal(format!("Failed to acquire cache write lock: {}", e)))?;

        cache.insert(mint_str, decimals);
    }

    Ok(decimals)
}

/// Fetch decimals directly from RPC without caching
pub fn fetch_decimals(rpc_client: &RpcClient, mint: &Pubkey) -> Result<u8> {
    let account = rpc_client
        .get_account(mint)
        .map_err(|e| Error::Rpc(format!("Failed to fetch mint account: {}", e)))?;

    // Parse SPL Token mint account
    // Mint account layout:
    // - mint_authority: Option<Pubkey> (36 bytes: 4 + 32)
    // - supply: u64 (8 bytes)
    // - decimals: u8 (1 byte)
    // - is_initialized: bool (1 byte)
    // - freeze_authority: Option<Pubkey> (36 bytes)

    if account.data.len() < 45 {
        return Err(Error::InvalidInstruction(
            "Mint account data too short".to_string(),
        ));
    }

    // Decimals is at offset 44 (after mint_authority option and supply)
    let decimals = account.data[44];

    Ok(decimals)
}

/// Get decimals with fallback to default if fetch fails
pub fn get_decimals_or_default(rpc_client: &RpcClient, mint: &Pubkey) -> u8 {
    get_decimals(rpc_client, mint).unwrap_or_else(|e| {
        tracing::warn!(
            "Failed to fetch decimals for {}: {}. Using default {}",
            mint,
            e,
            DEFAULT_DECIMALS
        );
        DEFAULT_DECIMALS
    })
}

/// Pre-populate cache with known decimals (for testing or known tokens)
pub fn set_cached_decimals(mint: &Pubkey, decimals: u8) -> Result<()> {
    let mut cache = DECIMALS_CACHE
        .write()
        .map_err(|e| Error::Internal(format!("Failed to acquire cache write lock: {}", e)))?;

    cache.insert(mint.to_string(), decimals);
    Ok(())
}

/// Clear the decimals cache
pub fn clear_cache() -> Result<()> {
    let mut cache = DECIMALS_CACHE
        .write()
        .map_err(|e| Error::Internal(format!("Failed to acquire cache write lock: {}", e)))?;

    cache.clear();
    Ok(())
}

/// Get cache size (for monitoring)
pub fn cache_size() -> Result<usize> {
    let cache = DECIMALS_CACHE
        .read()
        .map_err(|e| Error::Internal(format!("Failed to acquire cache read lock: {}", e)))?;

    Ok(cache.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_operations() {
        let mint = Pubkey::new_unique();

        // Set cached value
        set_cached_decimals(&mint, 9).unwrap();

        // Verify cache size
        assert_eq!(cache_size().unwrap(), 1);

        // Clear cache
        clear_cache().unwrap();
        assert_eq!(cache_size().unwrap(), 0);
    }

    #[test]
    fn test_default_decimals() {
        assert_eq!(DEFAULT_DECIMALS, 6);
    }
}
