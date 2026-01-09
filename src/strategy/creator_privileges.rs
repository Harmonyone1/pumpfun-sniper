//! Creator Privilege Checker
//!
//! Detect creator capability, not just behavior.
//! Checks for mint authority, freeze authority, and other dangerous privileges.

use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Token creator privileges
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreatorPrivileges {
    /// Creator can mint more tokens
    pub mint_authority_active: bool,
    /// Creator can freeze accounts
    pub freeze_authority_active: bool,
    /// Creator can update token metadata
    pub metadata_update_authority: bool,
    /// All authorities have been renounced
    pub authority_renounced: bool,
    /// List of remaining privileges
    pub remaining_privileges: Vec<Privilege>,
    /// The mint authority address if active
    pub mint_authority: Option<String>,
    /// The freeze authority address if active
    pub freeze_authority: Option<String>,
}

impl CreatorPrivileges {
    /// Check if any dangerous authorities are active
    pub fn has_dangerous_authorities(&self) -> bool {
        self.mint_authority_active || self.freeze_authority_active
    }

    /// Get a risk score (0.0 = safe, 1.0 = maximum risk)
    pub fn risk_score(&self) -> f64 {
        let mut score: f64 = 0.0;

        if self.mint_authority_active {
            score += 0.5; // Major risk
        }
        if self.freeze_authority_active {
            score += 0.4; // Major risk
        }
        if self.metadata_update_authority {
            score += 0.1; // Minor risk
        }

        score.min(1.0)
    }

    /// Get a signal value for scoring (-1.0 to +1.0)
    pub fn signal_value(&self) -> f64 {
        if self.mint_authority_active {
            return -1.0; // Fatal
        }
        if self.freeze_authority_active {
            return -0.9; // Very bad
        }
        if self.authority_renounced {
            return 0.3; // Good sign
        }
        if self.metadata_update_authority {
            return -0.1; // Minor concern
        }
        0.0 // Neutral
    }

    /// Get human-readable summary
    pub fn summary(&self) -> String {
        if self.authority_renounced {
            return "All authorities renounced (safe)".to_string();
        }

        let mut issues = vec![];
        if self.mint_authority_active {
            issues.push("MINT AUTHORITY ACTIVE");
        }
        if self.freeze_authority_active {
            issues.push("FREEZE AUTHORITY ACTIVE");
        }
        if self.metadata_update_authority {
            issues.push("Metadata update authority active");
        }

        if issues.is_empty() {
            "No dangerous authorities detected".to_string()
        } else {
            issues.join(", ")
        }
    }
}

/// Types of privileges a creator can have
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    /// Can mint additional tokens
    MintTokens,
    /// Can freeze token accounts
    FreezeAccounts,
    /// Can update token metadata
    UpdateMetadata,
    /// Can close token accounts
    CloseAccounts,
    /// Unknown privilege type
    Unknown(String),
}

impl Privilege {
    /// Get risk level (0-10)
    pub fn risk_level(&self) -> u8 {
        match self {
            Privilege::MintTokens => 10,      // Critical
            Privilege::FreezeAccounts => 9,   // Very high
            Privilege::CloseAccounts => 7,    // High
            Privilege::UpdateMetadata => 3,   // Low
            Privilege::Unknown(_) => 5,       // Medium (unknown)
        }
    }
}

/// Configuration for privilege checking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivilegeCheckConfig {
    /// Fail if mint authority is active
    pub reject_mint_authority: bool,
    /// Fail if freeze authority is active
    pub reject_freeze_authority: bool,
    /// Timeout for RPC calls in milliseconds
    pub rpc_timeout_ms: u64,
}

impl Default for PrivilegeCheckConfig {
    fn default() -> Self {
        Self {
            reject_mint_authority: true,
            reject_freeze_authority: true,
            rpc_timeout_ms: 5000,
        }
    }
}

/// Creator privilege checker
pub struct CreatorPrivilegeChecker {
    config: PrivilegeCheckConfig,
}

impl CreatorPrivilegeChecker {
    /// Create a new privilege checker
    pub fn new(config: PrivilegeCheckConfig) -> Self {
        Self { config }
    }

    /// Create with default config
    pub fn default_config() -> Self {
        Self::new(PrivilegeCheckConfig::default())
    }

    /// Check creator privileges for a token mint
    pub async fn check(
        &self,
        rpc: &RpcClient,
        mint_address: &str,
    ) -> Result<CreatorPrivileges, PrivilegeCheckError> {
        let mint_pubkey = Pubkey::from_str(mint_address)
            .map_err(|e| PrivilegeCheckError::InvalidAddress(e.to_string()))?;

        // Fetch mint account
        let account = rpc
            .get_account(&mint_pubkey)
            .await
            .map_err(|e| PrivilegeCheckError::RpcError(e.to_string()))?;

        // Parse mint data
        // SPL Token Mint layout:
        // - mint_authority (36 bytes): COption<Pubkey> (4 byte tag + 32 byte pubkey)
        // - supply (8 bytes): u64
        // - decimals (1 byte): u8
        // - is_initialized (1 byte): bool
        // - freeze_authority (36 bytes): COption<Pubkey>
        //
        // Total minimum: 82 bytes

        if account.data.len() < 82 {
            return Err(PrivilegeCheckError::InvalidMintData(
                "Account data too short for SPL mint".to_string(),
            ));
        }

        let data = &account.data;

        // Parse mint authority (COption<Pubkey>)
        let mint_authority_active = data[0..4] == [1, 0, 0, 0]; // Some variant
        let mint_authority = if mint_authority_active {
            Some(bs58::encode(&data[4..36]).into_string())
        } else {
            None
        };

        // Parse freeze authority (starts at byte 46: 36 + 8 + 1 + 1)
        let freeze_authority_offset = 46;
        let freeze_authority_active =
            data[freeze_authority_offset..freeze_authority_offset + 4] == [1, 0, 0, 0];
        let freeze_authority = if freeze_authority_active {
            Some(bs58::encode(&data[freeze_authority_offset + 4..freeze_authority_offset + 36]).into_string())
        } else {
            None
        };

        // Build privileges list
        let mut remaining_privileges = vec![];
        if mint_authority_active {
            remaining_privileges.push(Privilege::MintTokens);
        }
        if freeze_authority_active {
            remaining_privileges.push(Privilege::FreezeAccounts);
        }

        let authority_renounced = !mint_authority_active && !freeze_authority_active;

        // TODO: Check metadata update authority from Metaplex metadata account
        let metadata_update_authority = false; // Would need additional RPC call

        Ok(CreatorPrivileges {
            mint_authority_active,
            freeze_authority_active,
            metadata_update_authority,
            authority_renounced,
            remaining_privileges,
            mint_authority,
            freeze_authority,
        })
    }

    /// Quick check without RPC (from cached data)
    pub fn check_from_data(&self, mint_data: &[u8]) -> Result<CreatorPrivileges, PrivilegeCheckError> {
        if mint_data.len() < 82 {
            return Err(PrivilegeCheckError::InvalidMintData(
                "Data too short".to_string(),
            ));
        }

        let mint_authority_active = mint_data[0..4] == [1, 0, 0, 0];
        let mint_authority = if mint_authority_active {
            Some(bs58::encode(&mint_data[4..36]).into_string())
        } else {
            None
        };

        let freeze_authority_offset = 46;
        let freeze_authority_active =
            mint_data[freeze_authority_offset..freeze_authority_offset + 4] == [1, 0, 0, 0];
        let freeze_authority = if freeze_authority_active {
            Some(bs58::encode(&mint_data[freeze_authority_offset + 4..freeze_authority_offset + 36]).into_string())
        } else {
            None
        };

        let mut remaining_privileges = vec![];
        if mint_authority_active {
            remaining_privileges.push(Privilege::MintTokens);
        }
        if freeze_authority_active {
            remaining_privileges.push(Privilege::FreezeAccounts);
        }

        Ok(CreatorPrivileges {
            mint_authority_active,
            freeze_authority_active,
            metadata_update_authority: false,
            authority_renounced: !mint_authority_active && !freeze_authority_active,
            remaining_privileges,
            mint_authority,
            freeze_authority,
        })
    }

    /// Should this token be rejected based on privileges?
    pub fn should_reject(&self, privileges: &CreatorPrivileges) -> Option<String> {
        if self.config.reject_mint_authority && privileges.mint_authority_active {
            return Some("Mint authority is active - creator can mint unlimited tokens".to_string());
        }
        if self.config.reject_freeze_authority && privileges.freeze_authority_active {
            return Some("Freeze authority is active - creator can freeze your tokens".to_string());
        }
        None
    }
}

/// Errors during privilege checking
#[derive(Debug, Clone)]
pub enum PrivilegeCheckError {
    InvalidAddress(String),
    RpcError(String),
    InvalidMintData(String),
    Timeout,
}

impl std::fmt::Display for PrivilegeCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrivilegeCheckError::InvalidAddress(e) => write!(f, "Invalid address: {}", e),
            PrivilegeCheckError::RpcError(e) => write!(f, "RPC error: {}", e),
            PrivilegeCheckError::InvalidMintData(e) => write!(f, "Invalid mint data: {}", e),
            PrivilegeCheckError::Timeout => write!(f, "Request timed out"),
        }
    }
}

impl std::error::Error for PrivilegeCheckError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_privileges_dangerous_check() {
        let mut privileges = CreatorPrivileges::default();
        assert!(!privileges.has_dangerous_authorities());

        privileges.mint_authority_active = true;
        assert!(privileges.has_dangerous_authorities());

        privileges.mint_authority_active = false;
        privileges.freeze_authority_active = true;
        assert!(privileges.has_dangerous_authorities());
    }

    #[test]
    fn test_privileges_risk_score() {
        let mut privileges = CreatorPrivileges::default();
        assert_eq!(privileges.risk_score(), 0.0);

        privileges.mint_authority_active = true;
        assert_eq!(privileges.risk_score(), 0.5);

        privileges.freeze_authority_active = true;
        assert_eq!(privileges.risk_score(), 0.9);

        privileges.metadata_update_authority = true;
        assert_eq!(privileges.risk_score(), 1.0); // Capped at 1.0
    }

    #[test]
    fn test_privileges_signal_value() {
        let mut privileges = CreatorPrivileges::default();
        assert_eq!(privileges.signal_value(), 0.0);

        privileges.mint_authority_active = true;
        assert_eq!(privileges.signal_value(), -1.0);

        privileges.mint_authority_active = false;
        privileges.authority_renounced = true;
        assert_eq!(privileges.signal_value(), 0.3);
    }

    #[test]
    fn test_privilege_risk_levels() {
        assert_eq!(Privilege::MintTokens.risk_level(), 10);
        assert_eq!(Privilege::FreezeAccounts.risk_level(), 9);
        assert_eq!(Privilege::UpdateMetadata.risk_level(), 3);
    }

    #[test]
    fn test_check_from_data_no_authorities() {
        let checker = CreatorPrivilegeChecker::default_config();

        // Build mock mint data with no authorities
        let mut data = vec![0u8; 82];
        // Mint authority: None (tag = 0)
        data[0..4].copy_from_slice(&[0, 0, 0, 0]);
        // Freeze authority: None (tag = 0)
        data[46..50].copy_from_slice(&[0, 0, 0, 0]);

        let result = checker.check_from_data(&data).unwrap();
        assert!(!result.mint_authority_active);
        assert!(!result.freeze_authority_active);
        assert!(result.authority_renounced);
    }

    #[test]
    fn test_check_from_data_with_mint_authority() {
        let checker = CreatorPrivilegeChecker::default_config();

        // Build mock mint data with mint authority
        let mut data = vec![0u8; 82];
        // Mint authority: Some (tag = 1)
        data[0..4].copy_from_slice(&[1, 0, 0, 0]);
        // Pubkey (32 bytes) - just fill with test data
        data[4..36].copy_from_slice(&[1u8; 32]);

        let result = checker.check_from_data(&data).unwrap();
        assert!(result.mint_authority_active);
        assert!(result.mint_authority.is_some());
        assert!(!result.authority_renounced);
    }

    #[test]
    fn test_should_reject() {
        let checker = CreatorPrivilegeChecker::default_config();

        let mut privileges = CreatorPrivileges::default();
        assert!(checker.should_reject(&privileges).is_none());

        privileges.mint_authority_active = true;
        assert!(checker.should_reject(&privileges).is_some());

        privileges.mint_authority_active = false;
        privileges.freeze_authority_active = true;
        assert!(checker.should_reject(&privileges).is_some());
    }

    #[test]
    fn test_summary() {
        let mut privileges = CreatorPrivileges::default();
        privileges.authority_renounced = true;
        assert!(privileges.summary().contains("safe"));

        privileges.authority_renounced = false;
        privileges.mint_authority_active = true;
        assert!(privileges.summary().contains("MINT AUTHORITY"));
    }
}
