//! Shared data structures for the adaptive filtering system

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Wallet historical analysis data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletHistory {
    pub address: String,
    pub first_transaction: Option<DateTime<Utc>>,
    pub total_transactions: u64,
    pub pump_fun_transactions: u64,

    // Trading statistics
    pub tokens_deployed: u32,
    pub tokens_traded: u32,
    pub win_rate: f64, // 0.0 to 1.0
    pub avg_holding_time_secs: u64,
    pub avg_position_size_sol: f64,

    // Behavioral patterns
    pub avg_time_to_first_buy_secs: Option<u64>, // After token launch
    pub sells_within_10_min: u32,                // Sniper behavior indicator
    pub avg_profit_on_win: f64,
    pub avg_loss_on_loss: f64,

    // Risk indicators
    pub deployed_rug_count: u32,          // Tokens that went to ~0
    pub associated_wallets: Vec<String>,  // Funding relationships
    pub cluster_id: Option<String>,       // If part of coordinated group

    // Cache metadata
    pub fetched_at: DateTime<Utc>,
}

impl Default for WalletHistory {
    fn default() -> Self {
        Self {
            address: String::new(),
            first_transaction: None,
            total_transactions: 0,
            pump_fun_transactions: 0,
            tokens_deployed: 0,
            tokens_traded: 0,
            win_rate: 0.0,
            avg_holding_time_secs: 0,
            avg_position_size_sol: 0.0,
            avg_time_to_first_buy_secs: None,
            sells_within_10_min: 0,
            avg_profit_on_win: 0.0,
            avg_loss_on_loss: 0.0,
            deployed_rug_count: 0,
            associated_wallets: Vec::new(),
            cluster_id: None,
            fetched_at: Utc::now(),
        }
    }
}

impl WalletHistory {
    /// Calculate wallet age in days
    pub fn age_days(&self) -> Option<f64> {
        self.first_transaction.map(|first| {
            let duration = Utc::now() - first;
            duration.num_seconds() as f64 / 86400.0
        })
    }

    /// Check if this looks like a sniper wallet
    pub fn is_likely_sniper(&self) -> bool {
        // High number of pump.fun trades + quick sells
        self.pump_fun_transactions > 50 && self.sells_within_10_min > 10
    }

    /// Check if this looks like a deployer
    pub fn is_likely_deployer(&self) -> bool {
        self.tokens_deployed > 0
    }

    /// Check if this looks like a rug deployer
    pub fn is_likely_rug_deployer(&self) -> bool {
        self.deployed_rug_count > 0 && self.tokens_deployed > 0
    }
}

/// Token holder distribution analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenDistribution {
    pub mint: String,
    pub total_supply: u64,
    pub holder_count: u32,

    // Concentration metrics
    pub top_holder_pct: f64,     // Largest holder %
    pub top_5_holders_pct: f64,  // Top 5 combined %
    pub top_10_holders_pct: f64, // Top 10 combined %
    pub gini_coefficient: f64,   // 0 = equal distribution, 1 = concentrated

    // Distribution categories
    pub deployer_holdings_pct: f64,
    pub sniper_holdings_pct: f64, // Known snipers combined
    pub retail_holdings_pct: f64, // Small holders combined

    // Holder details (top holders only)
    pub holders: Vec<HolderInfo>,

    // Cache metadata
    pub fetched_at: DateTime<Utc>,
}

impl Default for TokenDistribution {
    fn default() -> Self {
        Self {
            mint: String::new(),
            total_supply: 0,
            holder_count: 0,
            top_holder_pct: 0.0,
            top_5_holders_pct: 0.0,
            top_10_holders_pct: 0.0,
            gini_coefficient: 0.0,
            deployer_holdings_pct: 0.0,
            sniper_holdings_pct: 0.0,
            retail_holdings_pct: 0.0,
            holders: Vec::new(),
            fetched_at: Utc::now(),
        }
    }
}

impl TokenDistribution {
    /// Check if distribution is highly concentrated (risky)
    pub fn is_concentrated(&self) -> bool {
        self.top_holder_pct > 50.0 || self.top_5_holders_pct > 70.0 || self.gini_coefficient > 0.8
    }
}

/// Information about a token holder
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolderInfo {
    pub address: String,
    pub balance: u64,
    pub pct_of_supply: f64,
    pub acquisition_time: Option<DateTime<Utc>>,
    pub wallet_type: Option<WalletType>,
}

/// Classification of wallet types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalletType {
    /// Token deployer/creator
    Deployer,
    /// Known sniper bot
    Sniper,
    /// Large holder (whale)
    Whale,
    /// Small retail holder
    Retail,
    /// Exchange wallet
    Exchange,
    /// Smart contract
    Contract,
    /// Unknown
    Unknown,
}

/// Order flow analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderFlowAnalysis {
    pub mint: String,
    pub analysis_window_secs: u64,

    // Volume metrics
    pub buy_volume_sol: f64,
    pub sell_volume_sol: f64,
    pub net_flow_sol: f64,
    pub buy_sell_ratio: f64,

    // Velocity metrics
    pub trades_per_minute: f64,
    pub avg_trade_size_sol: f64,
    pub trade_size_variance: f64,

    // Pattern detection
    pub burst_detected: bool,
    pub burst_intensity: f64, // 0.0 to 1.0
    pub wash_trading_score: f64, // 0.0 to 1.0
    pub organic_score: f64,   // 0.0 to 1.0

    // Timing analysis
    pub early_sell_pressure: f64, // Sells in first 5 min as ratio
    pub sustained_buying: bool,

    // Cache metadata
    pub analyzed_at: DateTime<Utc>,
}

impl Default for OrderFlowAnalysis {
    fn default() -> Self {
        Self {
            mint: String::new(),
            analysis_window_secs: 0,
            buy_volume_sol: 0.0,
            sell_volume_sol: 0.0,
            net_flow_sol: 0.0,
            buy_sell_ratio: 1.0,
            trades_per_minute: 0.0,
            avg_trade_size_sol: 0.0,
            trade_size_variance: 0.0,
            burst_detected: false,
            burst_intensity: 0.0,
            wash_trading_score: 0.0,
            organic_score: 0.5,
            early_sell_pressure: 0.0,
            sustained_buying: false,
            analyzed_at: Utc::now(),
        }
    }
}

/// Wallet cluster (coordinated wallets)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletCluster {
    pub cluster_id: String,
    pub wallets: Vec<String>,
    pub cluster_type: ClusterType,
    pub total_volume_sol: f64,
    pub common_funding_sources: Vec<String>,
    pub behavioral_correlation: f64, // 0.0 to 1.0
}

/// Types of wallet clusters
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterType {
    /// Coordinated sniping operation
    SniperRing,
    /// Deployer with multiple wallets
    DeployerCluster,
    /// Wash trading group
    WashTraders,
    /// Market maker
    MarketMaker,
    /// Unknown coordination
    Unknown,
}

/// A trade record for order flow analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub trader: String,
    pub is_buy: bool,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub timestamp: DateTime<Utc>,
    pub time_since_launch_ms: u64,
    pub signature: String,
}

/// Context provided to signal providers for new token analysis
#[derive(Debug, Clone)]
pub struct SignalContext {
    // From PumpPortal WebSocket
    pub mint: String,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub creator: String,
    pub bonding_curve: String,
    pub initial_buy: u64,
    pub v_tokens_in_bonding_curve: u64,
    pub v_sol_in_bonding_curve: u64,
    pub market_cap_sol: f64,
    pub timestamp: DateTime<Utc>,

    // Enriched data (may be None in hot path)
    pub creator_history: Option<WalletHistory>,
    pub token_distribution: Option<TokenDistribution>,
    pub recent_trades: Option<Vec<TradeRecord>>,
    pub order_flow: Option<OrderFlowAnalysis>,
}

impl SignalContext {
    /// Create a new context from a PumpPortal new token event
    pub fn from_new_token(
        mint: String,
        name: String,
        symbol: String,
        uri: String,
        creator: String,
        bonding_curve: String,
        initial_buy: u64,
        v_tokens_in_bonding_curve: u64,
        v_sol_in_bonding_curve: u64,
        market_cap_sol: f64,
    ) -> Self {
        Self {
            mint,
            name,
            symbol,
            uri,
            creator,
            bonding_curve,
            initial_buy,
            v_tokens_in_bonding_curve,
            v_sol_in_bonding_curve,
            market_cap_sol,
            timestamp: Utc::now(),
            creator_history: None,
            token_distribution: None,
            recent_trades: None,
            order_flow: None,
        }
    }

    /// Calculate estimated token price from bonding curve
    pub fn estimated_price(&self) -> f64 {
        if self.v_tokens_in_bonding_curve == 0 {
            return 0.0;
        }
        self.v_sol_in_bonding_curve as f64 / self.v_tokens_in_bonding_curve as f64
    }
}

/// Context for trade signal analysis (monitoring trades on a token)
#[derive(Debug, Clone)]
pub struct TradeSignalContext {
    pub mint: String,
    pub trader: String,
    pub is_buy: bool,
    pub token_amount: u64,
    pub sol_amount: u64,
    pub market_cap_sol: f64,
    pub time_since_launch: Duration,
    pub trader_history: Option<WalletHistory>,
    pub all_trades: Vec<TradeRecord>,
}

/// Context for position reassessment
#[derive(Debug, Clone)]
pub struct PositionSignalContext {
    pub mint: String,
    pub entry_time: DateTime<Utc>,
    pub entry_price: f64,
    pub current_price: f64,
    pub position_size_sol: f64,
    pub unrealized_pnl_pct: f64,
    pub recent_trades: Vec<TradeRecord>,
    pub holder_distribution: Option<TokenDistribution>,
    pub order_flow: Option<OrderFlowAnalysis>,
}

/// Position reassessment result
#[derive(Debug, Clone)]
pub struct ReassessmentResult {
    pub mint: String,
    pub previous_score: f64,
    pub current_score: f64,
    pub score_delta: f64,
    pub current_risk: f64,
    pub action: ReassessmentAction,
    pub reason: String,
}

/// Actions that can be taken after reassessment
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReassessmentAction {
    /// Keep position as-is
    Hold,
    /// Reduce position size
    ReducePosition { target_pct: f64 },
    /// Exit position entirely
    Exit,
    /// Increase position (rare - conditions improved)
    IncreasePosition { multiplier: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wallet_history_age() {
        let mut history = WalletHistory::default();
        history.first_transaction = Some(Utc::now() - chrono::Duration::days(30));
        let age = history.age_days().unwrap();
        assert!(age >= 29.9 && age <= 30.1);
    }

    #[test]
    fn test_signal_context_price() {
        let ctx = SignalContext::from_new_token(
            "mint".to_string(),
            "Test".to_string(),
            "TST".to_string(),
            "uri".to_string(),
            "creator".to_string(),
            "curve".to_string(),
            1000,
            1_000_000_000,
            100_000_000,
            1.0,
        );
        let price = ctx.estimated_price();
        assert!((price - 0.1).abs() < 0.001);
    }

    #[test]
    fn test_token_distribution_concentrated() {
        let mut dist = TokenDistribution::default();
        dist.top_holder_pct = 60.0;
        assert!(dist.is_concentrated());

        dist.top_holder_pct = 30.0;
        dist.gini_coefficient = 0.9;
        assert!(dist.is_concentrated());
    }
}
