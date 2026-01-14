//! Multi-wallet manager for distributing trades across multiple wallets
//!
//! Supports multiple selection strategies:
//! - round-robin: Alternate between wallets
//! - lowest-balance: Use wallet with lowest balance (spread risk)
//! - highest-balance: Use wallet with highest balance (consolidate)

use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// A trading wallet with its keypair and metadata
#[derive(Debug)]
pub struct TradingWallet {
    pub keypair: Keypair,
    pub name: String,
    pub path: String,
}

impl TradingWallet {
    pub fn pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.keypair.pubkey()
    }

    pub fn address(&self) -> String {
        self.keypair.pubkey().to_string()
    }
}

/// Selection strategy for choosing which wallet to use
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionStrategy {
    RoundRobin,
    LowestBalance,
    HighestBalance,
}

impl From<&str> for SelectionStrategy {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "lowest-balance" | "lowest_balance" => SelectionStrategy::LowestBalance,
            "highest-balance" | "highest_balance" => SelectionStrategy::HighestBalance,
            _ => SelectionStrategy::RoundRobin,
        }
    }
}

/// Manager for multiple trading wallets
pub struct MultiWalletManager {
    wallets: Vec<Arc<TradingWallet>>,
    strategy: SelectionStrategy,
    round_robin_index: AtomicUsize,
}

impl MultiWalletManager {
    /// Create a new multi-wallet manager from config paths
    pub fn new(wallet_paths: Vec<String>, strategy: &str) -> Result<Self> {
        let mut wallets = Vec::new();

        for (i, path) in wallet_paths.iter().enumerate() {
            match Self::load_wallet(path, i) {
                Ok(wallet) => {
                    info!(
                        "Loaded trading wallet {}: {} ({})",
                        wallet.name,
                        wallet.address(),
                        path
                    );
                    wallets.push(Arc::new(wallet));
                }
                Err(e) => {
                    warn!("Failed to load wallet from {}: {}", path, e);
                }
            }
        }

        if wallets.is_empty() {
            anyhow::bail!("No valid trading wallets loaded");
        }

        info!(
            "MultiWalletManager initialized with {} wallets, strategy: {}",
            wallets.len(),
            strategy
        );

        Ok(Self {
            wallets,
            strategy: SelectionStrategy::from(strategy),
            round_robin_index: AtomicUsize::new(0),
        })
    }

    /// Load a wallet from a keypair file
    fn load_wallet(path: &str, index: usize) -> Result<TradingWallet> {
        let keypair_data = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read keypair file: {}", path))?;
        let secret_key: Vec<u8> = serde_json::from_str(&keypair_data)
            .with_context(|| format!("Failed to parse keypair JSON: {}", path))?;
        let keypair = Keypair::from_bytes(&secret_key)
            .with_context(|| format!("Invalid keypair bytes: {}", path))?;

        let name = format!("wallet-{}", index + 1);

        Ok(TradingWallet {
            keypair,
            name,
            path: path.to_string(),
        })
    }

    /// Get the number of wallets
    pub fn wallet_count(&self) -> usize {
        self.wallets.len()
    }

    /// Get all wallets
    pub fn wallets(&self) -> &[Arc<TradingWallet>] {
        &self.wallets
    }

    /// Select the next wallet for trading based on the strategy
    pub fn select_wallet(&self, rpc_client: &RpcClient) -> Arc<TradingWallet> {
        match self.strategy {
            SelectionStrategy::RoundRobin => self.select_round_robin(),
            SelectionStrategy::LowestBalance => self.select_by_balance(rpc_client, false),
            SelectionStrategy::HighestBalance => self.select_by_balance(rpc_client, true),
        }
    }

    /// Round-robin selection
    fn select_round_robin(&self) -> Arc<TradingWallet> {
        let index = self.round_robin_index.fetch_add(1, Ordering::SeqCst) % self.wallets.len();
        self.wallets[index].clone()
    }

    /// Select wallet by balance
    fn select_by_balance(&self, rpc_client: &RpcClient, highest: bool) -> Arc<TradingWallet> {
        let mut best_wallet = self.wallets[0].clone();
        let mut best_balance = self.get_balance(rpc_client, &best_wallet);

        for wallet in &self.wallets[1..] {
            let balance = self.get_balance(rpc_client, wallet);
            let is_better = if highest {
                balance > best_balance
            } else {
                balance < best_balance
            };
            if is_better {
                best_balance = balance;
                best_wallet = wallet.clone();
            }
        }

        info!(
            "Selected {} ({}) with balance {:.4} SOL",
            best_wallet.name,
            &best_wallet.address()[..8],
            best_balance
        );

        best_wallet
    }

    /// Get wallet balance in SOL
    fn get_balance(&self, rpc_client: &RpcClient, wallet: &TradingWallet) -> f64 {
        match rpc_client.get_balance(&wallet.pubkey()) {
            Ok(lamports) => lamports as f64 / 1e9,
            Err(e) => {
                warn!(
                    "Failed to get balance for {}: {}",
                    wallet.address(),
                    e
                );
                0.0
            }
        }
    }

    /// Get total balance across all wallets
    pub fn total_balance(&self, rpc_client: &RpcClient) -> f64 {
        self.wallets
            .iter()
            .map(|w| self.get_balance(rpc_client, w))
            .sum()
    }

    /// Log status of all wallets
    pub fn log_status(&self, rpc_client: &RpcClient) {
        info!("=== Multi-Wallet Status ===");
        for wallet in &self.wallets {
            let balance = self.get_balance(rpc_client, wallet);
            info!(
                "  {} ({}): {:.4} SOL",
                wallet.name,
                &wallet.address()[..12],
                balance
            );
        }
        info!(
            "  Total: {:.4} SOL across {} wallets",
            self.total_balance(rpc_client),
            self.wallets.len()
        );
        info!("===========================");
    }

    /// Get a specific wallet by index
    pub fn get_wallet(&self, index: usize) -> Option<Arc<TradingWallet>> {
        self.wallets.get(index).cloned()
    }

    /// Find wallet by address
    pub fn find_by_address(&self, address: &str) -> Option<Arc<TradingWallet>> {
        self.wallets
            .iter()
            .find(|w| w.address() == address)
            .cloned()
    }
}
