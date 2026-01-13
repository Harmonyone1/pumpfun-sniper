//! Transaction decoder for processing ShredStream data
//!
//! Decodes raw transaction data and extracts pump.fun instructions.

use solana_sdk::pubkey::Pubkey;
use tracing::debug;

use crate::error::Result;
use crate::pump::instruction::{
    BuyAccounts, BuyInstruction, CreateAccounts, CreateInstruction, PumpInstruction, SellAccounts,
    SellInstruction,
};
use crate::pump::program::{match_discriminator, InstructionType, PUMP_PROGRAM_ID};

/// Decoded pump.fun event
#[derive(Debug, Clone)]
pub enum PumpEvent {
    /// New token created
    TokenCreated(TokenCreatedEvent),
    /// Token bought
    TokenBought(TokenTradeEvent),
    /// Token sold
    TokenSold(TokenTradeEvent),
}

/// Token creation event
#[derive(Debug, Clone)]
pub struct TokenCreatedEvent {
    /// Transaction signature
    pub signature: String,
    /// Slot number
    pub slot: u64,
    /// Token mint address
    pub mint: Pubkey,
    /// Token name
    pub name: String,
    /// Token symbol
    pub symbol: String,
    /// Metadata URI
    pub uri: String,
    /// Bonding curve address
    pub bonding_curve: Pubkey,
    /// Associated bonding curve (token account)
    pub associated_bonding_curve: Pubkey,
    /// Creator address
    pub creator: Pubkey,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Token trade event (buy or sell)
#[derive(Debug, Clone)]
pub struct TokenTradeEvent {
    /// Transaction signature
    pub signature: String,
    /// Slot number
    pub slot: u64,
    /// Token mint address
    pub mint: Pubkey,
    /// Bonding curve address
    pub bonding_curve: Pubkey,
    /// Trader address
    pub trader: Pubkey,
    /// Token amount
    pub token_amount: u64,
    /// SOL amount (max for buy, min for sell)
    pub sol_amount: u64,
    /// Is this a buy (true) or sell (false)
    pub is_buy: bool,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Decoder for pump.fun transactions
pub struct PumpDecoder;

impl PumpDecoder {
    /// Decode a raw transaction and extract pump.fun events
    pub fn decode_transaction(
        signature: &str,
        slot: u64,
        instruction_data: &[u8],
        accounts: &[Pubkey],
    ) -> Result<Option<PumpEvent>> {
        // Parse instruction
        let instruction = PumpInstruction::parse(instruction_data)?;

        match instruction {
            PumpInstruction::Create(create) => {
                Self::decode_create_event(signature, slot, create, accounts)
            }
            PumpInstruction::Buy(buy) => Self::decode_buy_event(signature, slot, buy, accounts),
            PumpInstruction::Sell(sell) => Self::decode_sell_event(signature, slot, sell, accounts),
            PumpInstruction::Unknown(_) => {
                debug!("Unknown pump.fun instruction in tx {}", signature);
                Ok(None)
            }
        }
    }

    /// Decode create token event
    fn decode_create_event(
        signature: &str,
        slot: u64,
        instruction: CreateInstruction,
        accounts: &[Pubkey],
    ) -> Result<Option<PumpEvent>> {
        let create_accounts = CreateAccounts::parse(accounts)?;

        Ok(Some(PumpEvent::TokenCreated(TokenCreatedEvent {
            signature: signature.to_string(),
            slot,
            mint: create_accounts.mint,
            name: instruction.name,
            symbol: instruction.symbol,
            uri: instruction.uri,
            bonding_curve: create_accounts.bonding_curve,
            associated_bonding_curve: create_accounts.associated_bonding_curve,
            creator: create_accounts.user,
            timestamp: chrono::Utc::now(),
        })))
    }

    /// Decode buy event
    fn decode_buy_event(
        signature: &str,
        slot: u64,
        instruction: BuyInstruction,
        accounts: &[Pubkey],
    ) -> Result<Option<PumpEvent>> {
        let buy_accounts = BuyAccounts::parse(accounts)?;

        Ok(Some(PumpEvent::TokenBought(TokenTradeEvent {
            signature: signature.to_string(),
            slot,
            mint: buy_accounts.mint,
            bonding_curve: buy_accounts.bonding_curve,
            trader: buy_accounts.user,
            token_amount: instruction.amount,
            sol_amount: instruction.max_sol_cost,
            is_buy: true,
            timestamp: chrono::Utc::now(),
        })))
    }

    /// Decode sell event
    fn decode_sell_event(
        signature: &str,
        slot: u64,
        instruction: SellInstruction,
        accounts: &[Pubkey],
    ) -> Result<Option<PumpEvent>> {
        let sell_accounts = SellAccounts::parse(accounts)?;

        Ok(Some(PumpEvent::TokenSold(TokenTradeEvent {
            signature: signature.to_string(),
            slot,
            mint: sell_accounts.mint,
            bonding_curve: sell_accounts.bonding_curve,
            trader: sell_accounts.user,
            token_amount: instruction.amount,
            sol_amount: instruction.min_sol_output,
            is_buy: false,
            timestamp: chrono::Utc::now(),
        })))
    }

    /// Check if instruction data is a pump.fun create instruction
    pub fn is_create_instruction(data: &[u8]) -> bool {
        match_discriminator(data) == Some(InstructionType::Create)
    }

    /// Check if instruction data is a pump.fun buy instruction
    pub fn is_buy_instruction(data: &[u8]) -> bool {
        match_discriminator(data) == Some(InstructionType::Buy)
    }

    /// Check if instruction data is a pump.fun sell instruction
    pub fn is_sell_instruction(data: &[u8]) -> bool {
        match_discriminator(data) == Some(InstructionType::Sell)
    }
}

/// Extract pump.fun instructions from a full transaction
pub fn extract_pump_instructions(
    accounts: &[Pubkey],
    instructions: &[(usize, Vec<u8>)], // (program_index, data)
) -> Vec<(Vec<u8>, Vec<Pubkey>)> {
    let pump_program = *PUMP_PROGRAM_ID;

    instructions
        .iter()
        .filter_map(|(program_idx, data)| {
            // Check if this instruction is for pump.fun program
            if *program_idx < accounts.len() && accounts[*program_idx] == pump_program {
                Some((data.clone(), accounts.to_vec()))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pump::program::DISCRIMINATORS;

    #[test]
    fn test_is_create_instruction() {
        let mut data = Vec::new();
        data.extend_from_slice(&DISCRIMINATORS::CREATE);
        data.extend_from_slice(&[0; 100]); // Padding

        assert!(PumpDecoder::is_create_instruction(&data));
        assert!(!PumpDecoder::is_buy_instruction(&data));
    }

    #[test]
    fn test_is_buy_instruction() {
        let mut data = Vec::new();
        data.extend_from_slice(&DISCRIMINATORS::BUY);
        data.extend_from_slice(&1000u64.to_le_bytes());
        data.extend_from_slice(&500u64.to_le_bytes());

        assert!(PumpDecoder::is_buy_instruction(&data));
        assert!(!PumpDecoder::is_create_instruction(&data));
    }
}
