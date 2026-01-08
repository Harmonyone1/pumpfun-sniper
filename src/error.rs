//! Error types for the sniper bot

use thiserror::Error;

/// Result type alias using our custom Error
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for the sniper bot
#[derive(Error, Debug)]
pub enum Error {
    // Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Missing environment variable: {0}")]
    MissingEnvVar(String),

    #[error("Invalid keypair: {0}")]
    InvalidKeypair(String),

    #[error("Insecure keypair permissions: {0}")]
    InsecureKeypair(String),

    // RPC errors
    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("RPC timeout after {0}ms")]
    RpcTimeout(u64),

    #[error("RPC connection failed: {0}")]
    RpcConnection(String),

    // ShredStream errors
    #[error("ShredStream connection failed: {0}")]
    ShredStreamConnection(String),

    #[error("ShredStream disconnected")]
    ShredStreamDisconnected,

    #[error("ShredStream decode error: {0}")]
    ShredStreamDecode(String),

    // Pump.fun protocol errors
    #[error("Invalid pump.fun instruction: {0}")]
    InvalidInstruction(String),

    #[error("Bonding curve decode failed: {0}")]
    BondingCurveDecode(String),

    #[error("Price calculation overflow")]
    PriceOverflow,

    #[error("Unknown instruction discriminator: {0:?}")]
    UnknownDiscriminator(Vec<u8>),

    // Trading errors
    #[error("Transaction build failed: {0}")]
    TransactionBuild(String),

    #[error("Transaction simulation failed: {0}")]
    TransactionSimulation(String),

    #[error("Transaction send failed: {0}")]
    TransactionSend(String),

    #[error("Slippage exceeded: expected {expected}, got {actual}")]
    SlippageExceeded { expected: u64, actual: u64 },

    // Jito errors
    #[error("Jito bundle submission failed: {0}")]
    JitoBundleSubmission(String),

    #[error("Jito bundle rejected: {0}")]
    JitoBundleRejected(String),

    #[error("Jito tip account not found")]
    JitoTipAccountNotFound,

    // Position management errors
    #[error("Position not found: {0}")]
    PositionNotFound(String),

    #[error("Position persistence failed: {0}")]
    PositionPersistence(String),

    // Safety limit errors
    #[error("Safety limit exceeded: {0}")]
    SafetyLimitExceeded(String),

    #[error("Daily loss limit reached: lost {lost}SOL, limit is {limit}SOL")]
    DailyLossLimitReached { lost: f64, limit: f64 },

    #[error("Max position size exceeded: current {current}SOL + buy {buy}SOL > max {max}SOL")]
    MaxPositionExceeded { current: f64, buy: f64, max: f64 },

    // Wallet management errors
    #[error("Wallet not found: {0}")]
    WalletNotFound(String),

    #[error("Wallet transfer failed: {0}")]
    WalletTransfer(String),

    #[error("Insufficient balance: {available}SOL available, {required}SOL required")]
    InsufficientBalance { available: f64, required: f64 },

    #[error("Emergency lock active: {0}")]
    EmergencyLockActive(String),

    #[error("AI authority exceeded: {0}")]
    AiAuthorityExceeded(String),

    #[error("Vault withdrawal blocked")]
    VaultWithdrawalBlocked,

    // Filter errors
    #[error("Token filtered: {reason}")]
    TokenFiltered { reason: String },

    #[error("Invalid regex pattern: {0}")]
    InvalidRegex(String),

    // Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    // I/O errors
    #[error("I/O error: {0}")]
    Io(String),

    // Generic errors
    #[error("Internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

impl Error {
    /// Check if this error is retryable (transient)
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Rpc(_)
                | Error::RpcTimeout(_)
                | Error::RpcConnection(_)
                | Error::ShredStreamDisconnected
                | Error::TransactionSend(_)
                | Error::JitoBundleSubmission(_)
        )
    }

    /// Check if this error is a safety violation
    pub fn is_safety_violation(&self) -> bool {
        matches!(
            self,
            Error::SafetyLimitExceeded(_)
                | Error::DailyLossLimitReached { .. }
                | Error::MaxPositionExceeded { .. }
                | Error::InsecureKeypair(_)
                | Error::EmergencyLockActive(_)
                | Error::AiAuthorityExceeded(_)
                | Error::VaultWithdrawalBlocked
                | Error::InsufficientBalance { .. }
        )
    }
}

// Conversion from solana_client errors
impl From<solana_client::client_error::ClientError> for Error {
    fn from(e: solana_client::client_error::ClientError) -> Self {
        Error::Rpc(e.to_string())
    }
}

// Conversion from serde_json errors
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serialization(e.to_string())
    }
}

// Conversion from I/O errors
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
