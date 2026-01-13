//! Credential management for wallets
//!
//! Loads wallet registry from wallets.json and manages keypair access.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

use super::types::{WalletEntry, WalletRegistry, WalletType};

/// Manages wallet credentials and registry
pub struct CredentialManager {
    /// Base directory for credentials
    credentials_dir: PathBuf,

    /// Wallet entries by name
    wallets: HashMap<String, WalletEntry>,

    /// Cached keypairs (loaded on demand)
    loaded_keypairs: HashMap<String, Keypair>,
}

impl CredentialManager {
    /// Load credential manager from directory
    ///
    /// Expects wallets.json in the credentials directory.
    pub fn load(credentials_dir: &Path) -> Result<Self> {
        let wallets_path = credentials_dir.join("wallets.json");

        let registry = if wallets_path.exists() {
            let content = std::fs::read_to_string(&wallets_path)
                .map_err(|e| Error::Config(format!("Failed to read wallets.json: {}", e)))?;

            serde_json::from_str::<WalletRegistry>(&content)
                .map_err(|e| Error::Config(format!("Failed to parse wallets.json: {}", e)))?
        } else {
            warn!("wallets.json not found, creating empty registry");
            WalletRegistry::default()
        };

        let wallets: HashMap<String, WalletEntry> = registry
            .wallets
            .into_iter()
            .map(|w| (w.name.clone(), w))
            .collect();

        info!("Loaded {} wallet entries", wallets.len());

        Ok(Self {
            credentials_dir: credentials_dir.to_path_buf(),
            wallets,
            loaded_keypairs: HashMap::new(),
        })
    }

    /// Get wallet entry by name
    pub fn get_wallet(&self, name: &str) -> Option<&WalletEntry> {
        self.wallets.get(name)
    }

    /// Get wallet address by name
    ///
    /// For keypair-based wallets with AUTO_DERIVED, derives from keypair.
    pub fn get_address(&mut self, name: &str) -> Result<solana_sdk::pubkey::Pubkey> {
        let wallet = self
            .wallets
            .get(name)
            .ok_or_else(|| Error::Config(format!("Wallet not found: {}", name)))?;

        if wallet.address == "AUTO_DERIVED" {
            // Need to load keypair to derive address
            let keypair = self.get_keypair(name)?;
            Ok(keypair.pubkey())
        } else {
            // Parse stored address
            wallet
                .address
                .parse()
                .map_err(|e| Error::Config(format!("Invalid address for {}: {}", name, e)))
        }
    }

    /// Load and cache keypair for a wallet
    ///
    /// Fails for external wallets (no keypair).
    pub fn get_keypair(&mut self, name: &str) -> Result<&Keypair> {
        // Check if already loaded
        if self.loaded_keypairs.contains_key(name) {
            return Ok(self.loaded_keypairs.get(name).unwrap());
        }

        let wallet = self
            .wallets
            .get(name)
            .ok_or_else(|| Error::Config(format!("Wallet not found: {}", name)))?;

        let keypair_path = wallet.keypair_path.as_ref().ok_or_else(|| {
            Error::Config(format!("Wallet {} is external, no keypair available", name))
        })?;

        // Resolve relative path
        let full_path = if keypair_path.is_absolute() {
            keypair_path.clone()
        } else {
            self.credentials_dir
                .parent()
                .unwrap_or(Path::new("."))
                .join(keypair_path)
        };

        debug!("Loading keypair from: {:?}", full_path);

        // Validate permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&full_path) {
                let mode = metadata.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(Error::InsecureKeypair(format!(
                        "Keypair {} has insecure permissions {:o}. Run 'chmod 600 {}'",
                        name,
                        mode & 0o777,
                        full_path.display()
                    )));
                }
            }
        }

        // Load keypair
        let keypair_bytes = std::fs::read(&full_path).map_err(|e| {
            Error::InvalidKeypair(format!("Failed to read keypair for {}: {}", name, e))
        })?;

        let keypair_json: Vec<u8> = serde_json::from_slice(&keypair_bytes).map_err(|e| {
            Error::InvalidKeypair(format!("Failed to parse keypair JSON for {}: {}", name, e))
        })?;

        let keypair = Keypair::from_bytes(&keypair_json).map_err(|e| {
            Error::InvalidKeypair(format!("Invalid keypair bytes for {}: {}", name, e))
        })?;

        self.loaded_keypairs.insert(name.to_string(), keypair);
        Ok(self.loaded_keypairs.get(name).unwrap())
    }

    /// List all wallet entries
    pub fn list_wallets(&self) -> Vec<&WalletEntry> {
        self.wallets.values().collect()
    }

    /// Add a new wallet to the registry
    pub fn add_wallet(&mut self, entry: WalletEntry) -> Result<()> {
        if self.wallets.contains_key(&entry.name) {
            return Err(Error::Config(format!(
                "Wallet already exists: {}",
                entry.name
            )));
        }

        self.wallets.insert(entry.name.clone(), entry);
        self.save_registry()?;

        Ok(())
    }

    /// Remove a wallet from the registry
    pub fn remove_wallet(&mut self, name: &str) -> Result<()> {
        if self.wallets.remove(name).is_none() {
            return Err(Error::Config(format!("Wallet not found: {}", name)));
        }

        self.loaded_keypairs.remove(name);
        self.save_registry()?;

        Ok(())
    }

    /// Save registry to wallets.json
    fn save_registry(&self) -> Result<()> {
        let registry = WalletRegistry {
            version: "1.0".to_string(),
            wallets: self.wallets.values().cloned().collect(),
        };

        let json = serde_json::to_string_pretty(&registry)
            .map_err(|e| Error::Config(format!("Failed to serialize registry: {}", e)))?;

        let wallets_path = self.credentials_dir.join("wallets.json");
        std::fs::write(&wallets_path, json)
            .map_err(|e| Error::Config(format!("Failed to write wallets.json: {}", e)))?;

        info!("Saved wallet registry");
        Ok(())
    }

    /// Validate permissions on all keypair files
    ///
    /// Returns list of warnings for files with insecure permissions.
    pub fn validate_permissions(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            for (name, wallet) in &self.wallets {
                if let Some(keypair_path) = &wallet.keypair_path {
                    let full_path = if keypair_path.is_absolute() {
                        keypair_path.clone()
                    } else {
                        self.credentials_dir
                            .parent()
                            .unwrap_or(Path::new("."))
                            .join(keypair_path)
                    };

                    if let Ok(metadata) = std::fs::metadata(&full_path) {
                        let mode = metadata.permissions().mode();
                        if mode & 0o077 != 0 {
                            warnings.push(format!(
                                "Wallet '{}' keypair has insecure permissions {:o}",
                                name,
                                mode & 0o777
                            ));
                        }
                    }
                }
            }
        }

        #[cfg(not(unix))]
        {
            // On Windows, we can't check Unix permissions
            // Could add Windows ACL checks here
        }

        warnings
    }

    /// Get all wallets of a specific type
    pub fn wallets_by_type(&self, wallet_type: WalletType) -> Vec<&WalletEntry> {
        self.wallets
            .values()
            .filter(|w| w.wallet_type == wallet_type)
            .collect()
    }

    /// Get the primary hot wallet (first hot wallet found)
    pub fn primary_hot_wallet(&self) -> Option<&WalletEntry> {
        self.wallets_by_type(WalletType::Hot).into_iter().next()
    }

    /// Get the primary vault (first vault or external found)
    pub fn primary_vault(&self) -> Option<&WalletEntry> {
        self.wallets_by_type(WalletType::Vault)
            .into_iter()
            .next()
            .or_else(|| {
                self.wallets_by_type(WalletType::External)
                    .into_iter()
                    .next()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_empty_registry() {
        let dir = tempdir().unwrap();
        let manager = CredentialManager::load(dir.path()).unwrap();
        assert!(manager.list_wallets().is_empty());
    }

    #[test]
    fn test_wallet_lookup() {
        let dir = tempdir().unwrap();

        // Create a wallets.json
        let registry = r#"{
            "version": "1.0",
            "wallets": [
                {
                    "name": "test-wallet",
                    "alias": "Test",
                    "type": "external",
                    "keypair_path": null,
                    "address": "11111111111111111111111111111111",
                    "created_at": "2025-01-01T00:00:00Z",
                    "notes": ""
                }
            ]
        }"#;

        std::fs::write(dir.path().join("wallets.json"), registry).unwrap();

        let manager = CredentialManager::load(dir.path()).unwrap();
        assert!(manager.get_wallet("test-wallet").is_some());
        assert!(manager.get_wallet("nonexistent").is_none());
    }
}
