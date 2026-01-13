//! SOL transfer execution
//!
//! Handles the actual on-chain transfer of SOL between wallets.

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use tracing::{debug, info};

use crate::error::{Error, Result};

/// Transfer executor for SOL transfers
pub struct TransferExecutor {
    rpc_client: RpcClient,
}

impl TransferExecutor {
    /// Create a new transfer executor
    pub fn new(rpc_client: RpcClient) -> Self {
        Self { rpc_client }
    }

    /// Execute a SOL transfer
    ///
    /// # Arguments
    /// * `from_keypair` - Keypair of the source wallet
    /// * `to_address` - Destination address
    /// * `amount_lamports` - Amount in lamports (1 SOL = 1_000_000_000 lamports)
    ///
    /// # Returns
    /// Transaction signature on success
    pub fn transfer(
        &self,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        amount_lamports: u64,
    ) -> Result<Signature> {
        debug!(
            "Executing transfer: {} lamports from {} to {}",
            amount_lamports,
            from_keypair.pubkey(),
            to_address
        );

        // Create transfer instruction
        let instruction =
            system_instruction::transfer(&from_keypair.pubkey(), to_address, amount_lamports);

        // Get recent blockhash
        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| Error::TransactionBuild(format!("Failed to get blockhash: {}", e)))?;

        // Build and sign transaction
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&from_keypair.pubkey()),
            &[from_keypair],
            blockhash,
        );

        // Send and confirm transaction
        let signature = self
            .rpc_client
            .send_and_confirm_transaction(&transaction)
            .map_err(|e| Error::TransactionSend(format!("Transfer failed: {}", e)))?;

        info!(
            "Transfer complete: {} lamports to {} (sig: {})",
            amount_lamports, to_address, signature
        );

        Ok(signature)
    }

    /// Execute a SOL transfer with amount in SOL
    pub fn transfer_sol(
        &self,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        amount_sol: f64,
    ) -> Result<Signature> {
        let amount_lamports = sol_to_lamports(amount_sol);
        self.transfer(from_keypair, to_address, amount_lamports)
    }

    /// Get balance of an address in lamports
    pub fn get_balance(&self, address: &Pubkey) -> Result<u64> {
        self.rpc_client
            .get_balance(address)
            .map_err(|e| Error::Rpc(format!("Failed to get balance: {}", e)))
    }

    /// Get balance of an address in SOL
    pub fn get_balance_sol(&self, address: &Pubkey) -> Result<f64> {
        let lamports = self.get_balance(address)?;
        Ok(lamports_to_sol(lamports))
    }

    /// Simulate a transfer (dry run)
    pub fn simulate_transfer(
        &self,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        amount_lamports: u64,
    ) -> Result<()> {
        debug!(
            "Simulating transfer: {} lamports from {} to {}",
            amount_lamports,
            from_keypair.pubkey(),
            to_address
        );

        // Check balance
        let balance = self.get_balance(&from_keypair.pubkey())?;
        if balance < amount_lamports {
            return Err(Error::TransactionBuild(format!(
                "Insufficient balance: {} lamports < {} lamports",
                balance, amount_lamports
            )));
        }

        // Estimate fee
        let instruction =
            system_instruction::transfer(&from_keypair.pubkey(), to_address, amount_lamports);

        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| Error::TransactionBuild(format!("Failed to get blockhash: {}", e)))?;

        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&from_keypair.pubkey()),
            &[from_keypair],
            blockhash,
        );

        // Simulate
        let result = self
            .rpc_client
            .simulate_transaction(&transaction)
            .map_err(|e| Error::TransactionBuild(format!("Simulation failed: {}", e)))?;

        if let Some(err) = result.value.err {
            return Err(Error::TransactionBuild(format!(
                "Simulation error: {:?}",
                err
            )));
        }

        debug!("Simulation successful");
        Ok(())
    }
}

/// Convert SOL to lamports
pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000.0) as u64
}

/// Convert lamports to SOL
pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sol_lamports_conversion() {
        assert_eq!(sol_to_lamports(1.0), 1_000_000_000);
        assert_eq!(sol_to_lamports(0.5), 500_000_000);
        assert_eq!(sol_to_lamports(0.001), 1_000_000);

        assert_eq!(lamports_to_sol(1_000_000_000), 1.0);
        assert_eq!(lamports_to_sol(500_000_000), 0.5);
        assert_eq!(lamports_to_sol(1_000_000), 0.001);
    }
}
