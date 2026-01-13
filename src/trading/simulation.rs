//! Transaction simulation
//!
//! Pre-flight simulation of transactions before submission.

use solana_client::rpc_client::RpcClient;
use solana_sdk::transaction::Transaction;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// Simulation result
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Whether simulation succeeded
    pub success: bool,
    /// Error message if failed
    pub error: Option<String>,
    /// Compute units consumed
    pub compute_units: Option<u64>,
    /// Logs from simulation
    pub logs: Vec<String>,
}

/// Simulate a transaction before sending
pub async fn simulate_transaction(
    rpc_client: &RpcClient,
    transaction: &Transaction,
) -> Result<SimulationResult> {
    info!("Simulating transaction...");

    let result = rpc_client
        .simulate_transaction(transaction)
        .map_err(|e| Error::TransactionSimulation(e.to_string()))?;

    let success = result.value.err.is_none();
    let error = result.value.err.map(|e| e.to_string());
    let logs = result.value.logs.unwrap_or_default();
    let compute_units = result.value.units_consumed;

    if success {
        debug!("Simulation succeeded, compute units: {:?}", compute_units);
    } else {
        warn!("Simulation failed: {:?}", error);
        for log in &logs {
            debug!("  Log: {}", log);
        }
    }

    Ok(SimulationResult {
        success,
        error,
        compute_units,
        logs,
    })
}

/// Simulate a Jito bundle
pub async fn simulate_bundle(
    rpc_client: &RpcClient,
    transactions: &[Transaction],
) -> Result<Vec<SimulationResult>> {
    info!("Simulating bundle with {} transactions", transactions.len());

    let mut results = Vec::with_capacity(transactions.len());

    for (i, tx) in transactions.iter().enumerate() {
        debug!("Simulating transaction {} of {}", i + 1, transactions.len());
        let result = simulate_transaction(rpc_client, tx).await?;

        if !result.success {
            warn!(
                "Transaction {} simulation failed: {:?}",
                i + 1,
                result.error
            );
        }

        results.push(result);
    }

    Ok(results)
}

/// Check if all simulations passed
pub fn all_simulations_passed(results: &[SimulationResult]) -> bool {
    results.iter().all(|r| r.success)
}

/// Get total compute units from simulation results
pub fn total_compute_units(results: &[SimulationResult]) -> u64 {
    results.iter().filter_map(|r| r.compute_units).sum()
}
