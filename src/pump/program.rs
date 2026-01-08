//! Pump.fun program constants and discriminators
//!
//! # WARNING: These constants may change without notice
//! Pump.fun has historically modified their program behavior.
//! If transactions start failing or parsing breaks, these values
//! may need to be updated.
//!
//! # How discriminators are calculated
//! Anchor uses the first 8 bytes of SHA-256("global:<instruction_name>")
//! as the instruction discriminator.

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Pump.fun program ID
/// WARNING: This may change if pump.fun deploys a new program version
pub const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

lazy_static::lazy_static! {
    /// Pump.fun program ID as Pubkey
    pub static ref PUMP_PROGRAM_ID: Pubkey =
        Pubkey::from_str(PUMP_PROGRAM_ID_STR).expect("Invalid pump program ID");
}

/// Instruction discriminators (first 8 bytes of instruction data)
/// Calculated as: SHA-256("global:<instruction_name>")[0..8]
#[allow(non_snake_case)]
pub mod DISCRIMINATORS {
    /// Create token instruction discriminator
    /// SHA-256("global:create")[0..8]
    pub const CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];

    /// Buy tokens instruction discriminator
    /// SHA-256("global:buy")[0..8]
    pub const BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

    /// Sell tokens instruction discriminator
    /// SHA-256("global:sell")[0..8]
    pub const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

    /// Initialize instruction discriminator (if exists)
    pub const INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];

    /// Withdraw instruction discriminator
    pub const WITHDRAW: [u8; 8] = [183, 18, 70, 156, 148, 109, 161, 34];
}

/// Account discriminators (first 8 bytes of account data)
/// Used to identify account types when parsing
#[allow(non_snake_case)]
pub mod ACCOUNT_DISCRIMINATORS {
    /// BondingCurve account discriminator
    pub const BONDING_CURVE: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

    /// Global config account discriminator
    pub const GLOBAL: [u8; 8] = [167, 232, 232, 177, 200, 108, 114, 127];

    /// Volume tracker account discriminator
    pub const VOLUME_TRACKER: [u8; 8] = [202, 42, 246, 43, 142, 190, 30, 255];
}

/// Jito tip accounts - use one of these for bundle tips
/// Tip should be in the LAST transaction of your bundle
/// Do NOT use Address Lookup Tables for tip accounts
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Get a random Jito tip account
pub fn get_random_tip_account() -> Pubkey {
    use rand::Rng;
    let idx = rand::thread_rng().gen_range(0..JITO_TIP_ACCOUNTS.len());
    Pubkey::from_str(JITO_TIP_ACCOUNTS[idx]).expect("Invalid Jito tip account")
}

/// Check if a discriminator matches an instruction type
pub fn match_discriminator(data: &[u8]) -> Option<InstructionType> {
    if data.len() < 8 {
        return None;
    }

    let discriminator: [u8; 8] = data[..8].try_into().ok()?;

    match discriminator {
        DISCRIMINATORS::CREATE => Some(InstructionType::Create),
        DISCRIMINATORS::BUY => Some(InstructionType::Buy),
        DISCRIMINATORS::SELL => Some(InstructionType::Sell),
        DISCRIMINATORS::INITIALIZE => Some(InstructionType::Initialize),
        DISCRIMINATORS::WITHDRAW => Some(InstructionType::Withdraw),
        _ => None,
    }
}

/// Pump.fun instruction types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionType {
    Create,
    Buy,
    Sell,
    Initialize,
    Withdraw,
}

impl std::fmt::Display for InstructionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstructionType::Create => write!(f, "create"),
            InstructionType::Buy => write!(f, "buy"),
            InstructionType::Sell => write!(f, "sell"),
            InstructionType::Initialize => write!(f, "initialize"),
            InstructionType::Withdraw => write!(f, "withdraw"),
        }
    }
}

/// Calculate instruction discriminator from name
/// This follows Anchor's convention: SHA-256("global:<name>")[0..8]
pub fn calculate_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};

    let preimage = format!("global:{}", name);
    let hash = Sha256::digest(preimage.as_bytes());

    let mut discriminator = [0u8; 8];
    discriminator.copy_from_slice(&hash[..8]);
    discriminator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discriminator_calculation() {
        // Verify our hardcoded discriminators match the calculation
        assert_eq!(calculate_discriminator("create"), DISCRIMINATORS::CREATE);
        assert_eq!(calculate_discriminator("buy"), DISCRIMINATORS::BUY);
        assert_eq!(calculate_discriminator("sell"), DISCRIMINATORS::SELL);
    }

    #[test]
    fn test_match_discriminator() {
        let create_data = [24, 30, 200, 40, 5, 28, 7, 119, 0, 0];
        assert_eq!(
            match_discriminator(&create_data),
            Some(InstructionType::Create)
        );

        let buy_data = [102, 6, 61, 18, 1, 218, 235, 234, 0, 0];
        assert_eq!(match_discriminator(&buy_data), Some(InstructionType::Buy));

        let unknown_data = [0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(match_discriminator(&unknown_data), None);
    }

    #[test]
    fn test_program_id() {
        assert_eq!(
            PUMP_PROGRAM_ID.to_string(),
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
        );
    }
}
