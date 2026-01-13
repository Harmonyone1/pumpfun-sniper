//! Transaction building for pump.fun trades

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use std::str::FromStr;

use crate::config::TradingConfig;
use crate::error::{Error, Result};
use crate::pump::price::{calculate_max_sol_with_slippage, calculate_min_sol_with_slippage};
use crate::pump::program::{DISCRIMINATORS, PUMP_PROGRAM_ID};

/// Transaction builder for pump.fun trades
pub struct TransactionBuilder {
    config: TradingConfig,
}

impl TransactionBuilder {
    pub fn new(config: TradingConfig) -> Self {
        Self { config }
    }

    /// Build a buy transaction
    pub fn build_buy(
        &self,
        payer: &Keypair,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_token_account: &Pubkey,
        token_amount: u64,
        max_sol_cost: u64,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> Result<Transaction> {
        // Build buy instruction data
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&DISCRIMINATORS::BUY);
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&max_sol_cost.to_le_bytes());

        // Build accounts list for buy instruction
        // Order matters! Must match pump.fun program expectations
        let accounts = vec![
            AccountMeta::new_readonly(global_account()?, false), // global
            AccountMeta::new(fee_recipient()?, false),           // fee_recipient
            AccountMeta::new_readonly(*mint, false),             // mint
            AccountMeta::new(*bonding_curve, false),             // bonding_curve
            AccountMeta::new(*associated_bonding_curve, false),  // associated_bonding_curve
            AccountMeta::new(*user_token_account, false),        // associated_user
            AccountMeta::new(payer.pubkey(), true),              // user (signer)
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // system_program
            AccountMeta::new_readonly(spl_token::ID, false),     // token_program
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::ID, false), // rent
            AccountMeta::new_readonly(event_authority()?, false), // event_authority
            AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),  // program
        ];

        let buy_instruction = Instruction {
            program_id: *PUMP_PROGRAM_ID,
            accounts,
            data,
        };

        // Build transaction
        let transaction = Transaction::new_signed_with_payer(
            &[buy_instruction],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        Ok(transaction)
    }

    /// Build a sell transaction
    pub fn build_sell(
        &self,
        payer: &Keypair,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_token_account: &Pubkey,
        token_amount: u64,
        min_sol_output: u64,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> Result<Transaction> {
        // Build sell instruction data
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&DISCRIMINATORS::SELL);
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&min_sol_output.to_le_bytes());

        // Build accounts list for sell instruction
        let accounts = vec![
            AccountMeta::new_readonly(global_account()?, false), // global
            AccountMeta::new(fee_recipient()?, false),           // fee_recipient
            AccountMeta::new_readonly(*mint, false),             // mint
            AccountMeta::new(*bonding_curve, false),             // bonding_curve
            AccountMeta::new(*associated_bonding_curve, false),  // associated_bonding_curve
            AccountMeta::new(*user_token_account, false),        // associated_user
            AccountMeta::new(payer.pubkey(), true),              // user (signer)
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // system_program
            AccountMeta::new_readonly(spl_associated_token_account::ID, false), // associated_token_program
            AccountMeta::new_readonly(spl_token::ID, false),                    // token_program
            AccountMeta::new_readonly(event_authority()?, false),               // event_authority
            AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),                 // program
        ];

        let sell_instruction = Instruction {
            program_id: *PUMP_PROGRAM_ID,
            accounts,
            data,
        };

        let transaction = Transaction::new_signed_with_payer(
            &[sell_instruction],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        Ok(transaction)
    }

    /// Build a buy transaction with tip for Jito bundle
    pub fn build_buy_with_tip(
        &self,
        payer: &Keypair,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_token_account: &Pubkey,
        token_amount: u64,
        max_sol_cost: u64,
        tip_account: &Pubkey,
        tip_lamports: u64,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> Result<Transaction> {
        // Build buy instruction
        let mut buy_data = Vec::with_capacity(24);
        buy_data.extend_from_slice(&DISCRIMINATORS::BUY);
        buy_data.extend_from_slice(&token_amount.to_le_bytes());
        buy_data.extend_from_slice(&max_sol_cost.to_le_bytes());

        let buy_accounts = vec![
            AccountMeta::new_readonly(global_account()?, false),
            AccountMeta::new(fee_recipient()?, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(*associated_bonding_curve, false),
            AccountMeta::new(*user_token_account, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::ID, false),
            AccountMeta::new_readonly(event_authority()?, false),
            AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),
        ];

        let buy_instruction = Instruction {
            program_id: *PUMP_PROGRAM_ID,
            accounts: buy_accounts,
            data: buy_data,
        };

        // Build tip instruction (SOL transfer to Jito tip account)
        let tip_instruction =
            system_instruction::transfer(&payer.pubkey(), tip_account, tip_lamports);

        // Combine: buy first, then tip
        let transaction = Transaction::new_signed_with_payer(
            &[buy_instruction, tip_instruction],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        Ok(transaction)
    }

    /// Calculate max SOL cost with slippage
    pub fn calculate_max_cost(&self, expected_cost: u64) -> u64 {
        calculate_max_sol_with_slippage(expected_cost, self.config.slippage_bps)
    }

    /// Calculate min SOL output with slippage
    pub fn calculate_min_output(&self, expected_output: u64) -> u64 {
        calculate_min_sol_with_slippage(expected_output, self.config.slippage_bps)
    }
}

/// Get the global config account address
/// This is a PDA derived from the pump.fun program
fn global_account() -> Result<Pubkey> {
    // TODO: Derive actual PDA or use known address
    // For now, return a placeholder
    Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf")
        .map_err(|e| Error::Config(format!("Invalid global account: {}", e)))
}

/// Get the fee recipient account address
fn fee_recipient() -> Result<Pubkey> {
    // TODO: Derive actual address or use known address
    Pubkey::from_str("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM")
        .map_err(|e| Error::Config(format!("Invalid fee recipient: {}", e)))
}

/// Get the event authority account address
fn event_authority() -> Result<Pubkey> {
    // TODO: Derive actual PDA
    Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")
        .map_err(|e| Error::Config(format!("Invalid event authority: {}", e)))
}

/// Derive associated token account address
pub fn derive_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address(wallet, mint)
}

/// Derive bonding curve PDA
pub fn derive_bonding_curve(mint: &Pubkey) -> Result<(Pubkey, u8)> {
    let seeds = &[b"bonding-curve", mint.as_ref()];
    Ok(Pubkey::find_program_address(seeds, &PUMP_PROGRAM_ID))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_ata() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ata = derive_ata(&wallet, &mint);

        // ATA should be deterministic
        assert_eq!(ata, derive_ata(&wallet, &mint));
    }

    #[test]
    fn test_slippage_calculation() {
        let config = TradingConfig {
            buy_amount_sol: 0.05,
            slippage_bps: 2500, // 25%
            priority_fee_lamports: 100000,
            simulate_before_send: false,
        };
        let builder = TransactionBuilder::new(config);

        let expected = 1_000_000_000u64; // 1 SOL
        let max_cost = builder.calculate_max_cost(expected);

        // With 25% slippage, max cost should be 1.25 SOL
        assert_eq!(max_cost, 1_250_000_000);
    }
}
