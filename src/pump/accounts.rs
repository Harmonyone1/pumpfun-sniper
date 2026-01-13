//! Pump.fun account structures
//!
//! # WARNING: These structures may change without notice
//! Pump.fun has modified their account layouts in the past.
//! If deserialization fails, these structures may need updating.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::pubkey::Pubkey;

use super::program::ACCOUNT_DISCRIMINATORS;
use crate::error::{Error, Result};

/// BondingCurve account - stores the bonding curve state for a token
///
/// This account holds:
/// - Virtual reserves used for price calculation
/// - Real reserves (actual SOL and tokens held)
/// - Whether the bonding curve has completed (migrated to Raydium)
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct BondingCurve {
    /// Account discriminator (first 8 bytes)
    /// Should match ACCOUNT_DISCRIMINATORS::BONDING_CURVE
    _discriminator: [u8; 8],

    /// Virtual SOL reserves for price calculation
    /// This is larger than real_sol_reserves and affects pricing
    pub virtual_sol_reserves: u64,

    /// Virtual token reserves for price calculation
    pub virtual_token_reserves: u64,

    /// Real SOL reserves (actual SOL held in bonding curve)
    pub real_sol_reserves: u64,

    /// Real token reserves (actual tokens held in bonding curve)
    pub real_token_reserves: u64,

    /// Total supply of the token
    pub token_total_supply: u64,

    /// Whether the bonding curve is complete (migrated to Raydium)
    pub complete: bool,
}

impl BondingCurve {
    /// Create a new BondingCurve for testing
    #[cfg(test)]
    pub fn new_for_test(
        virtual_sol_reserves: u64,
        virtual_token_reserves: u64,
        real_sol_reserves: u64,
        real_token_reserves: u64,
        token_total_supply: u64,
        complete: bool,
    ) -> Self {
        Self {
            _discriminator: ACCOUNT_DISCRIMINATORS::BONDING_CURVE,
            virtual_sol_reserves,
            virtual_token_reserves,
            real_sol_reserves,
            real_token_reserves,
            token_total_supply,
            complete,
        }
    }

    /// Deserialize from account data
    pub fn try_from_slice(data: &[u8]) -> Result<Self> {
        // Check minimum length
        if data.len() < 8 {
            return Err(Error::BondingCurveDecode(
                "Account data too short".to_string(),
            ));
        }

        // Verify discriminator
        let discriminator: [u8; 8] = data[..8]
            .try_into()
            .map_err(|_| Error::BondingCurveDecode("Invalid discriminator".to_string()))?;

        if discriminator != ACCOUNT_DISCRIMINATORS::BONDING_CURVE {
            return Err(Error::BondingCurveDecode(format!(
                "Wrong discriminator: expected {:?}, got {:?}",
                ACCOUNT_DISCRIMINATORS::BONDING_CURVE,
                discriminator
            )));
        }

        // Deserialize
        Self::try_from_slice_unchecked(data)
            .map_err(|e| Error::BondingCurveDecode(format!("Borsh decode failed: {}", e)))
    }

    /// Deserialize without checking discriminator (for performance)
    pub fn try_from_slice_unchecked(data: &[u8]) -> std::result::Result<Self, borsh::io::Error> {
        BorshDeserialize::try_from_slice(data)
    }

    /// Calculate current token price in SOL
    /// price = virtual_sol_reserves / virtual_token_reserves
    pub fn get_price(&self) -> Result<f64> {
        if self.virtual_token_reserves == 0 {
            return Err(Error::PriceOverflow);
        }

        Ok(self.virtual_sol_reserves as f64 / self.virtual_token_reserves as f64)
    }

    /// Calculate how many tokens you get for a given SOL amount
    /// Uses constant product formula: x * y = k
    pub fn calculate_buy_tokens(&self, sol_amount: u64) -> Result<u64> {
        if self.virtual_sol_reserves == 0 || self.virtual_token_reserves == 0 {
            return Err(Error::PriceOverflow);
        }

        // New SOL reserves after buy
        let new_sol_reserves = self
            .virtual_sol_reserves
            .checked_add(sol_amount)
            .ok_or(Error::PriceOverflow)?;

        // k = virtual_sol * virtual_token
        let k = (self.virtual_sol_reserves as u128)
            .checked_mul(self.virtual_token_reserves as u128)
            .ok_or(Error::PriceOverflow)?;

        // new_token_reserves = k / new_sol_reserves
        let new_token_reserves = k
            .checked_div(new_sol_reserves as u128)
            .ok_or(Error::PriceOverflow)?;

        // Tokens received = old_token_reserves - new_token_reserves
        let tokens_out = (self.virtual_token_reserves as u128)
            .checked_sub(new_token_reserves)
            .ok_or(Error::PriceOverflow)?;

        Ok(tokens_out as u64)
    }

    /// Calculate how much SOL you get for selling tokens
    pub fn calculate_sell_sol(&self, token_amount: u64) -> Result<u64> {
        if self.virtual_sol_reserves == 0 || self.virtual_token_reserves == 0 {
            return Err(Error::PriceOverflow);
        }

        // New token reserves after sell
        let new_token_reserves = self
            .virtual_token_reserves
            .checked_add(token_amount)
            .ok_or(Error::PriceOverflow)?;

        // k = virtual_sol * virtual_token
        let k = (self.virtual_sol_reserves as u128)
            .checked_mul(self.virtual_token_reserves as u128)
            .ok_or(Error::PriceOverflow)?;

        // new_sol_reserves = k / new_token_reserves
        let new_sol_reserves = k
            .checked_div(new_token_reserves as u128)
            .ok_or(Error::PriceOverflow)?;

        // SOL received = old_sol_reserves - new_sol_reserves
        let sol_out = (self.virtual_sol_reserves as u128)
            .checked_sub(new_sol_reserves)
            .ok_or(Error::PriceOverflow)?;

        Ok(sol_out as u64)
    }
}

/// Global configuration account
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct Global {
    _discriminator: [u8; 8],
    pub initialized: bool,
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub initial_virtual_token_reserves: u64,
    pub initial_virtual_sol_reserves: u64,
    pub initial_real_token_reserves: u64,
    pub token_total_supply: u64,
    pub fee_basis_points: u64,
}

impl Global {
    pub fn try_from_slice(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(Error::BondingCurveDecode(
                "Account data too short".to_string(),
            ));
        }

        let discriminator: [u8; 8] = data[..8]
            .try_into()
            .map_err(|_| Error::BondingCurveDecode("Invalid discriminator".to_string()))?;

        if discriminator != ACCOUNT_DISCRIMINATORS::GLOBAL {
            return Err(Error::BondingCurveDecode(format!(
                "Wrong discriminator for Global: expected {:?}, got {:?}",
                ACCOUNT_DISCRIMINATORS::GLOBAL,
                discriminator
            )));
        }

        Self::try_from_slice_unchecked(data)
            .map_err(|e| Error::BondingCurveDecode(format!("Borsh decode failed: {}", e)))
    }

    pub fn try_from_slice_unchecked(data: &[u8]) -> std::result::Result<Self, borsh::io::Error> {
        BorshDeserialize::try_from_slice(data)
    }
}

/// Token metadata extracted from create instruction
#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub mint: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub creator: Pubkey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bonding_curve_price() {
        let curve = BondingCurve {
            _discriminator: ACCOUNT_DISCRIMINATORS::BONDING_CURVE,
            virtual_sol_reserves: 30_000_000_000, // 30 SOL in lamports
            virtual_token_reserves: 1_000_000_000_000, // 1 trillion smallest units
            real_sol_reserves: 0,
            real_token_reserves: 1_000_000_000_000,
            token_total_supply: 1_000_000_000_000,
            complete: false,
        };

        let price = curve.get_price().unwrap();
        // Price = 30_000_000_000 / 1_000_000_000_000 = 0.03
        // This is lamports per token unit (not SOL per token)
        assert!((price - 0.03).abs() < 0.001);
    }

    #[test]
    fn test_buy_calculation() {
        let curve = BondingCurve {
            _discriminator: ACCOUNT_DISCRIMINATORS::BONDING_CURVE,
            virtual_sol_reserves: 30_000_000_000,
            virtual_token_reserves: 1_000_000_000_000,
            real_sol_reserves: 0,
            real_token_reserves: 1_000_000_000_000,
            token_total_supply: 1_000_000_000_000,
            complete: false,
        };

        // Buy with 1 SOL (1_000_000_000 lamports)
        let tokens = curve.calculate_buy_tokens(1_000_000_000).unwrap();
        // Should get approximately 32,258 tokens (with slippage from constant product)
        assert!(tokens > 30_000_000_000 && tokens < 35_000_000_000);
    }
}
