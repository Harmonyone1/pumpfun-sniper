//! Configuration loading and validation

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

// Re-export adaptive filter config
pub use crate::filter::adaptive::config::AdaptiveFilterConfig;
// Re-export holder watcher and kill switch configs
pub use crate::filter::holder_watcher::HolderWatcherConfig;
pub use crate::filter::kill_switch::KillSwitchConfig;
// Re-export strategy config
pub use crate::strategy::engine::StrategyEngineConfig;

/// Main configuration structure
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcConfig,
    pub jito: JitoConfig,
    pub shredstream: ShredStreamConfig,
    pub pumpportal: PumpPortalConfig,
    pub backpressure: BackpressureConfig,
    pub trading: TradingConfig,
    pub filters: FilterConfig,
    pub wallet_tracking: WalletTrackingConfig,
    pub auto_sell: AutoSellConfig,
    pub safety: SafetyConfig,
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub adaptive_filter: AdaptiveFilterConfig,
    #[serde(default)]
    pub strategy: StrategyEngineConfig,
    #[serde(default)]
    pub smart_money: SmartMoneyConfig,
    #[serde(default)]
    pub early_detection: EarlyDetectionConfig,
}

/// Smart money detection and kill-switch configuration
#[derive(Debug, Clone, Deserialize)]
pub struct SmartMoneyConfig {
    /// Enable smart money features
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Kill-switch configuration
    #[serde(default)]
    pub kill_switches: KillSwitchConfig,

    /// Holder watcher configuration
    #[serde(default)]
    pub holder_watcher: HolderWatcherConfig,
}

impl Default for SmartMoneyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            kill_switches: KillSwitchConfig::default(),
            holder_watcher: HolderWatcherConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    #[serde(default = "default_rpc_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_ws_endpoint")]
    pub ws_endpoint: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JitoConfig {
    #[serde(default = "default_jito_url")]
    pub block_engine_url: String,
    #[serde(default = "default_jito_regions")]
    pub regions: Vec<String>,
    #[serde(default = "default_tip_percentile")]
    pub tip_percentile: u32,
    #[serde(default = "default_min_tip")]
    pub min_tip_lamports: u64,
    #[serde(default = "default_max_tip")]
    pub max_tip_lamports: u64,
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShredStreamConfig {
    #[serde(default = "default_shredstream_url")]
    pub grpc_url: String,
    #[serde(default = "default_reconnect_delay_ms")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PumpPortalConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_pumpportal_ws_url")]
    pub ws_url: String,
    #[serde(default = "default_reconnect_delay_ms")]
    pub reconnect_delay_ms: u64,
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: u32,
    #[serde(default = "default_ping_interval_secs")]
    pub ping_interval_secs: u64,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_true")]
    pub use_for_trading: bool,
    /// Lightning wallet address (the wallet tied to the API key)
    #[serde(default)]
    pub lightning_wallet: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackpressureConfig {
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
    #[serde(default = "default_drop_policy")]
    pub drop_policy: DropPolicy,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DropPolicy {
    OldestNonPriority,
    Newest,
    Block,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    #[serde(default = "default_buy_amount_sol")]
    pub buy_amount_sol: f64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u32,
    #[serde(default = "default_priority_fee")]
    pub priority_fee_lamports: u64,
    #[serde(default)]
    pub simulate_before_send: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub min_liquidity_sol: f64,
    #[serde(default = "default_max_dev_holdings")]
    pub max_dev_holdings_pct: f64,
    #[serde(default)]
    pub name_patterns: Vec<String>,
    #[serde(default)]
    pub blocked_patterns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletTrackingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub wallets: Vec<String>,
    #[serde(default = "default_true")]
    pub priority_boost: bool,
    /// Minimum SOL trade size to trigger alert
    #[serde(default = "default_min_trade_sol")]
    pub min_trade_sol: f64,
    /// Auto-buy when tracked wallet buys
    #[serde(default)]
    pub auto_copy_trade: bool,
}

fn default_min_trade_sol() -> f64 { 0.5 }

/// Early detection configuration for pre-pump signals
#[derive(Debug, Clone, Deserialize)]
pub struct EarlyDetectionConfig {
    /// Enable early detection features
    #[serde(default = "default_true")]
    pub enabled: bool,

    // Volume spike detection
    #[serde(default = "default_true")]
    pub volume_spike_enabled: bool,
    /// Volume increase ratio to trigger signal (3.0 = 3x normal)
    #[serde(default = "default_volume_spike_ratio")]
    pub volume_spike_ratio: f64,
    /// Time window for volume measurement (seconds)
    #[serde(default = "default_volume_window_secs")]
    pub volume_window_secs: u64,

    // Accumulation pattern detection
    #[serde(default = "default_true")]
    pub accumulation_enabled: bool,
    /// Buy/sell ratio threshold for accumulation signal
    #[serde(default = "default_accumulation_ratio")]
    pub accumulation_buy_ratio: f64,
    /// Minimum unique buyers for accumulation signal
    #[serde(default = "default_min_unique_buyers")]
    pub min_unique_buyers: u32,

    // First trades analysis
    #[serde(default = "default_true")]
    pub first_trades_enabled: bool,
    /// Number of first trades to analyze
    #[serde(default = "default_first_trades_count")]
    pub first_trades_count: u32,
    /// Whale buy threshold in SOL
    #[serde(default = "default_whale_buy_threshold")]
    pub whale_buy_threshold_sol: f64,
    /// Track if creator is buying back
    #[serde(default = "default_true")]
    pub creator_buying_back: bool,

    // Bonding curve position
    /// Maximum bonding curve % to consider for entry
    #[serde(default = "default_max_bonding_curve")]
    pub max_bonding_curve_pct: f64,
    /// Bonus score for very early entries
    #[serde(default = "default_early_entry_bonus")]
    pub early_entry_bonus: f64,
}

fn default_volume_spike_ratio() -> f64 { 3.0 }
fn default_volume_window_secs() -> u64 { 60 }
fn default_accumulation_ratio() -> f64 { 4.0 }
fn default_min_unique_buyers() -> u32 { 5 }
fn default_first_trades_count() -> u32 { 10 }
fn default_whale_buy_threshold() -> f64 { 1.0 }
fn default_max_bonding_curve() -> f64 { 30.0 }
fn default_early_entry_bonus() -> f64 { 0.2 }

impl Default for EarlyDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            volume_spike_enabled: true,
            volume_spike_ratio: 3.0,
            volume_window_secs: 60,
            accumulation_enabled: true,
            accumulation_buy_ratio: 4.0,
            min_unique_buyers: 5,
            first_trades_enabled: true,
            first_trades_count: 10,
            whale_buy_threshold_sol: 1.0,
            creator_buying_back: true,
            max_bonding_curve_pct: 30.0,
            early_entry_bonus: 0.2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AutoSellConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,
    #[serde(default)]
    pub partial_take_profit: bool,
    #[serde(default = "default_price_poll_interval_ms")]
    pub price_poll_interval_ms: u64,
    /// Enable trailing stop to lock in profits
    #[serde(default = "default_true")]
    pub trailing_stop_enabled: bool,
    /// Profit % to activate trailing stop (e.g., 10 = activate at 10% profit)
    #[serde(default = "default_trailing_activation")]
    pub trailing_stop_activation_pct: f64,
    /// Distance from peak to trigger sell (e.g., 15 = sell if drops 15% from peak)
    #[serde(default = "default_trailing_distance")]
    pub trailing_stop_distance_pct: f64,

    // === LAYERED EXITS ===
    /// Quick profit: sell 50% at this level (first layer)
    #[serde(default = "default_quick_profit_pct")]
    pub quick_profit_pct: f64,
    /// Second profit: sell 25% at this level (second layer)
    #[serde(default = "default_second_profit_pct")]
    pub second_profit_pct: f64,
    /// No-movement exit: exit if price moves less than this % after no_movement_secs
    #[serde(default = "default_no_movement_threshold")]
    pub no_movement_threshold_pct: f64,
    /// Seconds before triggering no-movement exit
    #[serde(default = "default_no_movement_secs")]
    pub no_movement_secs: u64,

    // === DYNAMIC TRAILING STOP ===
    /// Enable dynamic trailing stop that tightens as profit grows
    #[serde(default = "default_true")]
    pub dynamic_trailing_enabled: bool,
    /// Base trailing stop % (used when P&L < 15%)
    #[serde(default = "default_trailing_base")]
    pub trailing_stop_base_pct: f64,
    /// Medium trailing stop % (used when P&L 15-25%)
    #[serde(default = "default_trailing_medium")]
    pub trailing_stop_medium_pct: f64,
    /// Tight trailing stop % (used when P&L > 25%)
    #[serde(default = "default_trailing_tight")]
    pub trailing_stop_tight_pct: f64,
}

fn default_quick_profit_pct() -> f64 { 4.0 }
fn default_second_profit_pct() -> f64 { 8.0 }
fn default_no_movement_threshold() -> f64 { 2.0 }
fn default_no_movement_secs() -> u64 { 120 }
fn default_trailing_base() -> f64 { 5.0 }
fn default_trailing_medium() -> f64 { 4.0 }
fn default_trailing_tight() -> f64 { 3.0 }

fn default_trailing_activation() -> f64 {
    10.0
}
fn default_trailing_distance() -> f64 {
    15.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "default_true")]
    pub require_sell_confirmation: bool,
    #[serde(default = "default_max_position_sol")]
    pub max_position_sol: f64,
    #[serde(default = "default_daily_loss_limit")]
    pub daily_loss_limit_sol: f64,
    #[serde(default = "default_keypair_balance_warning")]
    pub keypair_balance_warning_sol: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    /// Hot wallet name (from wallets.json)
    #[serde(default = "default_hot_wallet")]
    pub hot_wallet: String,

    /// Vault wallet name (from wallets.json)
    #[serde(default = "default_vault_wallet")]
    pub vault_wallet: String,

    /// Credentials directory
    #[serde(default = "default_credentials_dir")]
    pub credentials_dir: String,

    /// Safety limits for wallet operations
    #[serde(default)]
    pub safety: WalletSafetyConfig,

    /// Automatic extraction settings
    #[serde(default)]
    pub extraction: ExtractionConfig,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            hot_wallet: default_hot_wallet(),
            vault_wallet: default_vault_wallet(),
            credentials_dir: default_credentials_dir(),
            safety: WalletSafetyConfig::default(),
            extraction: ExtractionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletSafetyConfig {
    /// Minimum SOL to keep in hot wallet
    #[serde(default = "default_min_hot_balance")]
    pub min_hot_balance_sol: f64,

    /// Maximum single transfer to vault
    #[serde(default = "default_max_single_transfer")]
    pub max_single_transfer_sol: f64,

    /// Maximum total daily extraction
    #[serde(default = "default_max_daily_extraction")]
    pub max_daily_extraction_sol: f64,

    /// Require confirmation above this amount
    #[serde(default = "default_confirm_above")]
    pub confirm_above_sol: f64,

    /// Emergency threshold - pause trading below this
    #[serde(default = "default_emergency_threshold")]
    pub emergency_threshold_sol: f64,

    /// Lock vault address (prevent changes)
    #[serde(default = "default_true")]
    pub vault_address_locked: bool,

    /// Maximum AI can auto-execute
    #[serde(default = "default_ai_max_auto_transfer")]
    pub ai_max_auto_transfer_sol: f64,
}

impl Default for WalletSafetyConfig {
    fn default() -> Self {
        Self {
            min_hot_balance_sol: default_min_hot_balance(),
            max_single_transfer_sol: default_max_single_transfer(),
            max_daily_extraction_sol: default_max_daily_extraction(),
            confirm_above_sol: default_confirm_above(),
            emergency_threshold_sol: default_emergency_threshold(),
            vault_address_locked: true,
            ai_max_auto_transfer_sol: default_ai_max_auto_transfer(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractionConfig {
    /// Enable automatic extraction
    #[serde(default = "default_true")]
    pub auto_extract: bool,

    /// Extract when profit exceeds this threshold
    #[serde(default = "default_profit_threshold")]
    pub profit_threshold_sol: f64,

    /// Percentage of profits to extract
    #[serde(default = "default_profit_percentage")]
    pub profit_percentage: f64,

    /// Extract excess when balance exceeds ceiling
    #[serde(default = "default_balance_ceiling")]
    pub balance_ceiling_sol: f64,

    /// Minimum time between extractions (seconds)
    #[serde(default = "default_min_extraction_interval")]
    pub min_extraction_interval_secs: u64,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            auto_extract: true,
            profit_threshold_sol: default_profit_threshold(),
            profit_percentage: default_profit_percentage(),
            balance_ceiling_sol: default_balance_ceiling(),
            min_extraction_interval_secs: default_min_extraction_interval(),
        }
    }
}

// Default value functions
fn default_rpc_endpoint() -> String {
    std::env::var("RPC_ENDPOINT").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into())
}

fn default_ws_endpoint() -> String {
    std::env::var("RPC_WS_ENDPOINT").unwrap_or_else(|_| "wss://api.mainnet-beta.solana.com".into())
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_max_retries() -> u32 {
    3
}

fn default_jito_url() -> String {
    std::env::var("JITO_BLOCK_ENGINE_URL")
        .unwrap_or_else(|_| "https://ny.mainnet.block-engine.jito.wtf".into())
}

fn default_jito_regions() -> Vec<String> {
    vec!["ny".into(), "amsterdam".into()]
}

fn default_tip_percentile() -> u32 {
    50
}

fn default_min_tip() -> u64 {
    10000
}

fn default_max_tip() -> u64 {
    1000000
}

fn default_retry_attempts() -> u32 {
    3
}

fn default_retry_base_delay_ms() -> u64 {
    50
}

fn default_shredstream_url() -> String {
    std::env::var("SHREDSTREAM_GRPC_URL").unwrap_or_else(|_| "http://127.0.0.1:10000".into())
}

fn default_pumpportal_ws_url() -> String {
    "wss://pumpportal.fun/api/data".into()
}

fn default_reconnect_delay_ms() -> u64 {
    1000
}

fn default_max_reconnect_attempts() -> u32 {
    10
}

fn default_ping_interval_secs() -> u64 {
    30
}

fn default_channel_capacity() -> usize {
    10000
}

fn default_drop_policy() -> DropPolicy {
    DropPolicy::OldestNonPriority
}

fn default_buy_amount_sol() -> f64 {
    0.05
}

fn default_slippage_bps() -> u32 {
    2500
}

fn default_priority_fee() -> u64 {
    100000
}

fn default_max_dev_holdings() -> f64 {
    20.0
}

fn default_take_profit_pct() -> f64 {
    50.0
}

fn default_stop_loss_pct() -> f64 {
    30.0
}

fn default_price_poll_interval_ms() -> u64 {
    1000
}

fn default_max_position_sol() -> f64 {
    0.5
}

fn default_daily_loss_limit() -> f64 {
    1.0
}

fn default_keypair_balance_warning() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

// Wallet config defaults
fn default_hot_wallet() -> String {
    "hot-trading".to_string()
}

fn default_vault_wallet() -> String {
    "vault-robinhood".to_string()
}

fn default_credentials_dir() -> String {
    "credentials".to_string()
}

fn default_min_hot_balance() -> f64 {
    0.1
}

fn default_max_single_transfer() -> f64 {
    5.0
}

fn default_max_daily_extraction() -> f64 {
    10.0
}

fn default_confirm_above() -> f64 {
    1.0
}

fn default_emergency_threshold() -> f64 {
    0.05
}

fn default_ai_max_auto_transfer() -> f64 {
    0.5
}

fn default_profit_threshold() -> f64 {
    0.2
}

fn default_profit_percentage() -> f64 {
    50.0
}

fn default_balance_ceiling() -> f64 {
    2.0
}

fn default_min_extraction_interval() -> u64 {
    3600
}

impl Config {
    /// Load configuration from file and environment variables
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        let settings = config::Config::builder()
            // Start with defaults
            .set_default("rpc.endpoint", default_rpc_endpoint())?
            .set_default("rpc.ws_endpoint", default_ws_endpoint())?
            .set_default("rpc.timeout_ms", default_timeout_ms() as i64)?
            .set_default("rpc.max_retries", default_max_retries() as i64)?
            // Load from file if exists
            .add_source(config::File::from(path).required(false))
            // Override with environment variables (prefix SNIPER_)
            .add_source(
                config::Environment::with_prefix("SNIPER")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .context("Failed to build configuration")?;

        let config: Config = settings
            .try_deserialize()
            .context("Failed to deserialize configuration")?;

        // Validate configuration
        config.validate()?;

        Ok(config)
    }

    /// Validate configuration values
    fn validate(&self) -> Result<()> {
        // Validate Jito regions (max 2)
        if self.jito.regions.len() > 2 {
            anyhow::bail!(
                "Maximum 2 Jito regions allowed, got {}",
                self.jito.regions.len()
            );
        }

        // Validate trading amounts
        if self.trading.buy_amount_sol <= 0.0 {
            anyhow::bail!("buy_amount_sol must be positive");
        }

        if self.trading.slippage_bps > 10000 {
            anyhow::bail!("slippage_bps cannot exceed 10000 (100%)");
        }

        // Validate safety limits
        if self.safety.max_position_sol <= 0.0 {
            anyhow::bail!("max_position_sol must be positive");
        }

        if self.safety.daily_loss_limit_sol <= 0.0 {
            anyhow::bail!("daily_loss_limit_sol must be positive");
        }

        // Validate auto-sell percentages
        if self.auto_sell.enabled {
            if self.auto_sell.take_profit_pct <= 0.0 {
                anyhow::bail!("take_profit_pct must be positive");
            }
            if self.auto_sell.stop_loss_pct <= 0.0 || self.auto_sell.stop_loss_pct >= 100.0 {
                anyhow::bail!("stop_loss_pct must be between 0 and 100");
            }
        }

        // Validate filter patterns (compile regex to check)
        for pattern in &self.filters.name_patterns {
            regex::Regex::new(pattern)
                .with_context(|| format!("Invalid name_pattern regex: {}", pattern))?;
        }

        for pattern in &self.filters.blocked_patterns {
            regex::Regex::new(pattern)
                .with_context(|| format!("Invalid blocked_pattern regex: {}", pattern))?;
        }

        // Validate wallet addresses
        for wallet in &self.wallet_tracking.wallets {
            if wallet.len() < 32 || wallet.len() > 44 {
                anyhow::bail!("Invalid wallet address: {}", wallet);
            }
        }

        // Warn about backpressure policy
        if self.backpressure.drop_policy == DropPolicy::Block {
            tracing::warn!(
                "Backpressure drop_policy is 'block' - this is NOT recommended for real-time trading"
            );
        }

        Ok(())
    }

    /// Get masked configuration for display (hide secrets)
    pub fn masked_display(&self) -> String {
        format!(
            r#"Configuration:
  RPC:
    endpoint: {}
    timeout: {}ms
  Jito:
    block_engine: {}
    regions: {:?}
    tip_percentile: {}
  PumpPortal:
    enabled: {}
    ws_url: {}
    use_for_trading: {}
    api_key: {}
  Trading:
    buy_amount: {} SOL
    slippage: {}bps
  Filters:
    enabled: {}
    min_liquidity: {} SOL
    max_dev_holdings: {}%
  Auto-Sell:
    enabled: {}
    take_profit: {}%
    stop_loss: {}%
  Safety:
    max_position: {} SOL
    daily_loss_limit: {} SOL
"#,
            mask_url(&self.rpc.endpoint),
            self.rpc.timeout_ms,
            mask_url(&self.jito.block_engine_url),
            self.jito.regions,
            self.jito.tip_percentile,
            self.pumpportal.enabled,
            self.pumpportal.ws_url,
            self.pumpportal.use_for_trading,
            if self.pumpportal.api_key.is_empty() {
                "(not set)"
            } else {
                "***"
            },
            self.trading.buy_amount_sol,
            self.trading.slippage_bps,
            self.filters.enabled,
            self.filters.min_liquidity_sol,
            self.filters.max_dev_holdings_pct,
            self.auto_sell.enabled,
            self.auto_sell.take_profit_pct,
            self.auto_sell.stop_loss_pct,
            self.safety.max_position_sol,
            self.safety.daily_loss_limit_sol,
        )
    }
}

/// Mask URL for display (hide API keys in query params)
fn mask_url(url: &str) -> String {
    if let Some(idx) = url.find('?') {
        format!("{}?***", &url[..idx])
    } else {
        url.to_string()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc: RpcConfig {
                endpoint: default_rpc_endpoint(),
                ws_endpoint: default_ws_endpoint(),
                timeout_ms: default_timeout_ms(),
                max_retries: default_max_retries(),
            },
            jito: JitoConfig {
                block_engine_url: default_jito_url(),
                regions: default_jito_regions(),
                tip_percentile: default_tip_percentile(),
                min_tip_lamports: default_min_tip(),
                max_tip_lamports: default_max_tip(),
                retry_attempts: default_retry_attempts(),
                retry_base_delay_ms: default_retry_base_delay_ms(),
            },
            shredstream: ShredStreamConfig {
                grpc_url: default_shredstream_url(),
                reconnect_delay_ms: default_reconnect_delay_ms(),
                max_reconnect_attempts: default_max_reconnect_attempts(),
            },
            pumpportal: PumpPortalConfig {
                enabled: true,
                ws_url: default_pumpportal_ws_url(),
                reconnect_delay_ms: default_reconnect_delay_ms(),
                max_reconnect_attempts: default_max_reconnect_attempts(),
                ping_interval_secs: default_ping_interval_secs(),
                api_key: String::new(),
                use_for_trading: true,
                lightning_wallet: String::new(),
            },
            backpressure: BackpressureConfig {
                channel_capacity: default_channel_capacity(),
                drop_policy: default_drop_policy(),
            },
            trading: TradingConfig {
                buy_amount_sol: default_buy_amount_sol(),
                slippage_bps: default_slippage_bps(),
                priority_fee_lamports: default_priority_fee(),
                simulate_before_send: false,
            },
            filters: FilterConfig {
                enabled: true,
                min_liquidity_sol: 0.0,
                max_dev_holdings_pct: default_max_dev_holdings(),
                name_patterns: vec![],
                blocked_patterns: vec![],
            },
            wallet_tracking: WalletTrackingConfig {
                enabled: false,
                wallets: vec![],
                priority_boost: true,
                min_trade_sol: default_min_trade_sol(),
                auto_copy_trade: false,
            },
            auto_sell: AutoSellConfig {
                enabled: true,
                take_profit_pct: default_take_profit_pct(),
                stop_loss_pct: default_stop_loss_pct(),
                partial_take_profit: false,
                price_poll_interval_ms: default_price_poll_interval_ms(),
                trailing_stop_enabled: true,
                trailing_stop_activation_pct: default_trailing_activation(),
                trailing_stop_distance_pct: default_trailing_distance(),
                quick_profit_pct: default_quick_profit_pct(),
                second_profit_pct: default_second_profit_pct(),
                no_movement_threshold_pct: default_no_movement_threshold(),
                no_movement_secs: default_no_movement_secs(),
                dynamic_trailing_enabled: true,
                trailing_stop_base_pct: default_trailing_base(),
                trailing_stop_medium_pct: default_trailing_medium(),
                trailing_stop_tight_pct: default_trailing_tight(),
            },
            safety: SafetyConfig {
                require_sell_confirmation: true,
                max_position_sol: default_max_position_sol(),
                daily_loss_limit_sol: default_daily_loss_limit(),
                keypair_balance_warning_sol: default_keypair_balance_warning(),
            },
            wallet: WalletConfig::default(),
            adaptive_filter: AdaptiveFilterConfig::default(),
            strategy: StrategyEngineConfig::default(),
            smart_money: SmartMoneyConfig::default(),
            early_detection: EarlyDetectionConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.filters.enabled);
        assert_eq!(config.trading.slippage_bps, 2500);
        assert_eq!(config.safety.max_position_sol, 0.5);
    }

    #[test]
    fn test_drop_policy_deserialize() {
        let json = r#""oldest_non_priority""#;
        let policy: DropPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy, DropPolicy::OldestNonPriority);
    }

    #[test]
    fn test_mask_url() {
        assert_eq!(
            mask_url("https://api.example.com?key=secret"),
            "https://api.example.com?***"
        );
        assert_eq!(
            mask_url("https://api.example.com"),
            "https://api.example.com"
        );
    }
}
