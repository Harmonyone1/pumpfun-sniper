//! Pump.fun instruction parsing
//!
//! Parses pump.fun instructions from transaction data.

use solana_sdk::pubkey::Pubkey;

use crate::error::{Error, Result};
use super::program::{InstructionType, match_discriminator};

/// Parsed pump.fun instruction
#[derive(Debug, Clone)]
pub enum PumpInstruction {
    Create(CreateInstruction),
    Buy(BuyInstruction),
    Sell(SellInstruction),
    Unknown(Vec<u8>),
}

impl PumpInstruction {
    /// Parse instruction from raw data
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(Error::InvalidInstruction(
                "Instruction data too short".to_string(),
            ));
        }

        match match_discriminator(data) {
            Some(InstructionType::Create) => {
                CreateInstruction::parse(&data[8..]).map(PumpInstruction::Create)
            }
            Some(InstructionType::Buy) => {
                BuyInstruction::parse(&data[8..]).map(PumpInstruction::Buy)
            }
            Some(InstructionType::Sell) => {
                SellInstruction::parse(&data[8..]).map(PumpInstruction::Sell)
            }
            _ => {
                // Return unknown instruction instead of error for graceful handling
                Ok(PumpInstruction::Unknown(data.to_vec()))
            }
        }
    }

    /// Get instruction type
    pub fn instruction_type(&self) -> Option<InstructionType> {
        match self {
            PumpInstruction::Create(_) => Some(InstructionType::Create),
            PumpInstruction::Buy(_) => Some(InstructionType::Buy),
            PumpInstruction::Sell(_) => Some(InstructionType::Sell),
            PumpInstruction::Unknown(_) => None,
        }
    }
}

/// Create token instruction data
#[derive(Debug, Clone)]
pub struct CreateInstruction {
    /// Token name
    pub name: String,
    /// Token symbol
    pub symbol: String,
    /// Metadata URI
    pub uri: String,
}

impl CreateInstruction {
    /// Parse from instruction data (after discriminator)
    pub fn parse(data: &[u8]) -> Result<Self> {
        // Create instruction format:
        // - name: String (4 bytes length + content)
        // - symbol: String (4 bytes length + content)
        // - uri: String (4 bytes length + content)

        let mut offset = 0;

        let name = read_string(data, &mut offset)?;
        let symbol = read_string(data, &mut offset)?;
        let uri = read_string(data, &mut offset)?;

        Ok(Self { name, symbol, uri })
    }
}

/// Buy tokens instruction data
#[derive(Debug, Clone)]
pub struct BuyInstruction {
    /// Amount of tokens to buy (in token smallest units)
    pub amount: u64,
    /// Maximum SOL to spend (slippage protection)
    pub max_sol_cost: u64,
}

impl BuyInstruction {
    /// Parse from instruction data (after discriminator)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(Error::InvalidInstruction(
                "Buy instruction data too short".to_string(),
            ));
        }

        let amount = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| Error::InvalidInstruction("Invalid amount".to_string()))?,
        );

        let max_sol_cost = u64::from_le_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| Error::InvalidInstruction("Invalid max_sol_cost".to_string()))?,
        );

        Ok(Self {
            amount,
            max_sol_cost,
        })
    }
}

/// Sell tokens instruction data
#[derive(Debug, Clone)]
pub struct SellInstruction {
    /// Amount of tokens to sell
    pub amount: u64,
    /// Minimum SOL to receive (slippage protection)
    pub min_sol_output: u64,
}

impl SellInstruction {
    /// Parse from instruction data (after discriminator)
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(Error::InvalidInstruction(
                "Sell instruction data too short".to_string(),
            ));
        }

        let amount = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| Error::InvalidInstruction("Invalid amount".to_string()))?,
        );

        let min_sol_output = u64::from_le_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| Error::InvalidInstruction("Invalid min_sol_output".to_string()))?,
        );

        Ok(Self {
            amount,
            min_sol_output,
        })
    }
}

/// Extracted accounts from a create instruction
#[derive(Debug, Clone)]
pub struct CreateAccounts {
    pub mint: Pubkey,
    pub mint_authority: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub global: Pubkey,
    pub mpl_token_metadata: Pubkey,
    pub metadata: Pubkey,
    pub user: Pubkey,
    pub system_program: Pubkey,
    pub token_program: Pubkey,
    pub associated_token_program: Pubkey,
    pub rent: Pubkey,
    pub event_authority: Pubkey,
    pub program: Pubkey,
}

impl CreateAccounts {
    /// Parse accounts from instruction account indices
    /// Requires the full account list from the transaction
    pub fn parse(accounts: &[Pubkey]) -> Result<Self> {
        if accounts.len() < 14 {
            return Err(Error::InvalidInstruction(format!(
                "Create instruction needs 14 accounts, got {}",
                accounts.len()
            )));
        }

        Ok(Self {
            mint: accounts[0],
            mint_authority: accounts[1],
            bonding_curve: accounts[2],
            associated_bonding_curve: accounts[3],
            global: accounts[4],
            mpl_token_metadata: accounts[5],
            metadata: accounts[6],
            user: accounts[7],
            system_program: accounts[8],
            token_program: accounts[9],
            associated_token_program: accounts[10],
            rent: accounts[11],
            event_authority: accounts[12],
            program: accounts[13],
        })
    }
}

/// Extracted accounts from a buy instruction
#[derive(Debug, Clone)]
pub struct BuyAccounts {
    pub global: Pubkey,
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub associated_user: Pubkey,
    pub user: Pubkey,
    pub system_program: Pubkey,
    pub token_program: Pubkey,
    pub rent: Pubkey,
    pub event_authority: Pubkey,
    pub program: Pubkey,
}

impl BuyAccounts {
    pub fn parse(accounts: &[Pubkey]) -> Result<Self> {
        if accounts.len() < 12 {
            return Err(Error::InvalidInstruction(format!(
                "Buy instruction needs 12 accounts, got {}",
                accounts.len()
            )));
        }

        Ok(Self {
            global: accounts[0],
            fee_recipient: accounts[1],
            mint: accounts[2],
            bonding_curve: accounts[3],
            associated_bonding_curve: accounts[4],
            associated_user: accounts[5],
            user: accounts[6],
            system_program: accounts[7],
            token_program: accounts[8],
            rent: accounts[9],
            event_authority: accounts[10],
            program: accounts[11],
        })
    }
}

/// Extracted accounts from a sell instruction
#[derive(Debug, Clone)]
pub struct SellAccounts {
    pub global: Pubkey,
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub associated_user: Pubkey,
    pub user: Pubkey,
    pub system_program: Pubkey,
    pub associated_token_program: Pubkey,
    pub token_program: Pubkey,
    pub event_authority: Pubkey,
    pub program: Pubkey,
}

impl SellAccounts {
    pub fn parse(accounts: &[Pubkey]) -> Result<Self> {
        if accounts.len() < 12 {
            return Err(Error::InvalidInstruction(format!(
                "Sell instruction needs 12 accounts, got {}",
                accounts.len()
            )));
        }

        Ok(Self {
            global: accounts[0],
            fee_recipient: accounts[1],
            mint: accounts[2],
            bonding_curve: accounts[3],
            associated_bonding_curve: accounts[4],
            associated_user: accounts[5],
            user: accounts[6],
            system_program: accounts[7],
            associated_token_program: accounts[8],
            token_program: accounts[9],
            event_authority: accounts[10],
            program: accounts[11],
        })
    }
}

/// Helper function to read a borsh-encoded string
fn read_string(data: &[u8], offset: &mut usize) -> Result<String> {
    if *offset + 4 > data.len() {
        return Err(Error::InvalidInstruction(
            "String length out of bounds".to_string(),
        ));
    }

    let len = u32::from_le_bytes(
        data[*offset..*offset + 4]
            .try_into()
            .map_err(|_| Error::InvalidInstruction("Invalid string length".to_string()))?,
    ) as usize;

    *offset += 4;

    if *offset + len > data.len() {
        return Err(Error::InvalidInstruction(
            "String content out of bounds".to_string(),
        ));
    }

    let s = String::from_utf8(data[*offset..*offset + len].to_vec())
        .map_err(|_| Error::InvalidInstruction("Invalid UTF-8 in string".to_string()))?;

    *offset += len;

    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pump::program::DISCRIMINATORS;

    #[test]
    fn test_parse_buy_instruction() {
        let mut data = Vec::new();
        data.extend_from_slice(&DISCRIMINATORS::BUY);
        data.extend_from_slice(&1000000u64.to_le_bytes()); // amount
        data.extend_from_slice(&500000000u64.to_le_bytes()); // max_sol_cost (0.5 SOL)

        let instruction = PumpInstruction::parse(&data).unwrap();

        if let PumpInstruction::Buy(buy) = instruction {
            assert_eq!(buy.amount, 1000000);
            assert_eq!(buy.max_sol_cost, 500000000);
        } else {
            panic!("Expected Buy instruction");
        }
    }

    #[test]
    fn test_parse_unknown_instruction() {
        let data = vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3];
        let instruction = PumpInstruction::parse(&data).unwrap();

        assert!(matches!(instruction, PumpInstruction::Unknown(_)));
    }
}
