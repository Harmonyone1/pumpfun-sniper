//! Shared data structures for the adaptive filtering system

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Wallet historical analysis data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletHistory {
    pub address: String,
    pub first_seen: Option<DateTime<Utc>>,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub total_volume_sol: f64,
    pub recent_trades: Vec<WalletTrade>,

    // Extended stats (populated from deeper analysis)
    #[serde(default)]
    pub tokens_deployed: u32,
    #[serde(default)]
    pub tokens_traded: u32,
    #[serde(default)]
    pub avg_holding_time_secs: u64,
    #[serde(default)]
    pub avg_position_size_sol: f64,

    // Behavioral patterns
    #[serde(default)]
    pub avg_time_to_first_buy_secs: Option<u64>, // After token launch
    #[serde(default)]
    pub sells_within_10_min: u32, // Sniper behavior indicator

    // Risk indicators
    #[serde(default)]
    pub deployed_rug_count: u32, // Tokens that went to ~0
    #[serde(default)]
    pub associated_wallets: Vec<String>, // Funding relationships
    #[serde(default)]
    pub cluster_id: Option<String>, // If part of coordinated group

    // Cache metadata
    pub fetched_at: DateTime<Utc>,
}

/// A single trade from wallet history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletTrade {
    pub signature: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub is_buy: bool,
    pub sol_amount: f64,
    pub token_mint: Option<String>,
    pub profit_sol: Option<f64>,
}

impl Default for WalletHistory {
    fn default() -> Self {
        Self {
            address: String::new(),
            first_seen: None,
            total_trades: 0,
            winning_trades: 0,
            total_volume_sol: 0.0,
            recent_trades: Vec::new(),
            tokens_deployed: 0,
            tokens_traded: 0,
            avg_holding_time_secs: 0,
            avg_position_size_sol: 0.0,
            avg_time_to_first_buy_secs: None,
            sells_within_10_min: 0,
            deployed_rug_count: 0,
            associated_wallets: Vec::new(),
            cluster_id: None,
            fetched_at: Utc::now(),
        }
    }
}

/// Token holder info from Helius API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHolderInfo {
    pub address: String,
    pub amount: u64,
    pub percentage: f64,
}

impl WalletHistory {
    /// Calculate wallet age in days
    pub fn age_days(&self) -> Option<f64> {
        self.first_seen.map(|first| {
            let duration = Utc::now() - first;
            duration.num_seconds() as f64 / 86400.0
        })
    }

    /// Calculate win rate
    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.winning_trades as f64 / self.total_trades as f64
    }

    /// Check if this looks like a sniper wallet
    pub fn is_likely_sniper(&self) -> bool {
        // High number of trades + quick sells
        self.total_trades > 50 && self.sells_within_10_min > 10
    }

    /// Check if this looks like a deployer
    pub fn is_likely_deployer(&self) -> bool {
        self.tokens_deployed > 0
    }

    /// Check if this looks like a rug deployer
    pub fn is_likely_rug_deployer(&self) -> bool {
        self.deployed_rug_count > 0 && self.tokens_deployed > 0
    }

    /// Check if wallet is new (less than N days old)
    pub fn is_new_wallet(&self, days: f64) -> bool {
        self.age_days().map(|age| age < days).unwrap_or(true)
    }

    /// Check if wallet has significant trading history
    pub fn has_history(&self) -> bool {
        self.total_trades > 0 || self.total_volume_sol > 0.0
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
    pub burst_intensity: f64,    // 0.0 to 1.0
    pub wash_trading_score: f64, // 0.0 to 1.0
    pub organic_score: f64,      // 0.0 to 1.0

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
    /// PumpPortal provider observational UI amount; not canonical raw units.
    pub initial_buy: f64,
    /// PumpPortal provider observational token reserve figure; not canonical raw units.
    pub v_tokens_in_bonding_curve: f64,
    /// PumpPortal provider observational SOL reserve figure; SOL, NOT lamports.
    pub v_sol_in_bonding_curve: f64,
    pub market_cap_sol: f64,
    pub timestamp: DateTime<Utc>,

    // Early detection data
    /// Bonding curve progress percentage (0-100%)
    pub bonding_curve_pct: Option<f64>,

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
        initial_buy: f64,
        v_tokens_in_bonding_curve: f64,
        v_sol_in_bonding_curve: f64,
        market_cap_sol: f64,
    ) -> Self {
        // Calculate bonding curve percentage from the provider's observational SOL
        // reserve figure (already SOL, NOT lamports).
        // pump.fun bonding curve completes at ~85 SOL, starts at ~30 SOL virtual
        // Progress = (provider_v_sol - 30) / (85 - 30) * 100
        let bc_pct = Self::calculate_bonding_curve_pct(v_sol_in_bonding_curve);

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
            bonding_curve_pct: Some(bc_pct),
            creator_history: None,
            token_distribution: None,
            recent_trades: None,
            order_flow: None,
        }
    }

    /// Bonding-curve progress heuristic from the provider's observational SOL figure.
    ///
    /// Input is the PumpPortal provider `v_sol_in_bonding_curve` value, which is
    /// already SOL (NOT lamports). There is intentionally NO `/1e9` conversion here.
    ///
    /// IMPORTANT: this is a provider heuristic only. It is NOT canonical
    /// PumpMarketOracle state and NOT executable market truth. Do not use it to
    /// authorize live-money entry/exit; the MPT executable quote remains the final
    /// market gate.
    ///
    /// pump.fun bonding curve: starts at ~30 SOL virtual, completes at ~85 SOL.
    /// If the input is non-finite, this returns a conservative `0.0`
    /// (unavailable-safe). Production stream validation should already reject
    /// non-finite provider values before they reach here.
    pub fn calculate_bonding_curve_pct(provider_v_sol: f64) -> f64 {
        if !provider_v_sol.is_finite() {
            return 0.0;
        }
        let progress = ((provider_v_sol - 30.0) / (85.0 - 30.0)) * 100.0;
        progress.clamp(0.0, 100.0)
    }

    /// Estimated token price as a provider observational reserve-ratio estimate.
    ///
    /// This operates directly on the provider observational f64 reserve figures
    /// (SOL over tokens). It is NOT a canonical or executable price and must never
    /// be treated as market truth; it is a rough provider ratio only. Returns
    /// `0.0` for a zero/non-finite denominator or any non-finite result.
    pub fn estimated_price(&self) -> f64 {
        let denom = self.v_tokens_in_bonding_curve;
        if denom == 0.0 || !denom.is_finite() {
            return 0.0;
        }
        let price = self.v_sol_in_bonding_curve / denom;
        if price.is_finite() {
            price
        } else {
            0.0
        }
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
        history.first_seen = Some(Utc::now() - chrono::Duration::days(30));
        let age = history.age_days().unwrap();
        assert!(age >= 29.9 && age <= 30.1);
    }

    fn sample_context(
        initial_buy: f64,
        v_tokens: f64,
        v_sol: f64,
    ) -> SignalContext {
        SignalContext::from_new_token(
            "mint".to_string(),
            "Test".to_string(),
            "TST".to_string(),
            "uri".to_string(),
            "creator".to_string(),
            "curve".to_string(),
            initial_buy,
            v_tokens,
            v_sol,
            1.0,
        )
    }

    #[test]
    fn test_signal_context_price() {
        // Provider observational reserve ratio: 10 SOL / 100 tokens = 0.1
        let ctx = sample_context(1000.0, 100.0, 10.0);
        let price = ctx.estimated_price();
        assert!((price - 0.1).abs() < 0.001);
    }

    #[test]
    fn test_provider_bonding_curve_30_sol_is_zero_percent() {
        assert_eq!(SignalContext::calculate_bonding_curve_pct(30.0), 0.0);
    }

    #[test]
    fn test_provider_bonding_curve_57_5_sol_is_fifty_percent() {
        // (57.5 - 30) / (85 - 30) * 100 = 50
        let pct = SignalContext::calculate_bonding_curve_pct(57.5);
        assert!((pct - 50.0).abs() < 1e-9, "expected 50%, got {}", pct);
    }

    #[test]
    fn test_provider_bonding_curve_85_sol_is_hundred_percent() {
        assert_eq!(SignalContext::calculate_bonding_curve_pct(85.0), 100.0);
    }

    #[test]
    fn test_provider_bonding_curve_fractional_sol_not_truncated() {
        // 31.75 SOL must yield a small positive pct, NOT zero (old /1e9 truncation bug).
        let pct = SignalContext::calculate_bonding_curve_pct(31.75);
        assert!(pct > 0.0, "fractional provider SOL truncated to zero: {}", pct);
        // Sanity: (31.75 - 30) / 55 * 100 ≈ 3.1818
        assert!((pct - 3.181818).abs() < 1e-4, "unexpected pct {}", pct);
    }

    #[test]
    fn test_provider_bonding_curve_nonfinite_is_zero() {
        // Documented conservative unavailable-safe behavior for non-finite input.
        assert_eq!(SignalContext::calculate_bonding_curve_pct(f64::NAN), 0.0);
        assert_eq!(SignalContext::calculate_bonding_curve_pct(f64::INFINITY), 0.0);
    }

    #[test]
    fn test_signal_context_preserves_fractional_provider_values() {
        let ctx = sample_context(31.75, 42.5, 31.75);
        // Fractional provider values must survive the bridge untruncated.
        assert_eq!(ctx.initial_buy, 31.75);
        assert_eq!(ctx.v_tokens_in_bonding_curve, 42.5);
        assert_eq!(ctx.v_sol_in_bonding_curve, 31.75);
        // Derived bonding-curve pct should reflect the fractional SOL, not zero.
        assert!(ctx.bonding_curve_pct.unwrap() > 0.0);
    }

    #[test]
    fn test_provider_estimated_price_is_observational_ratio() {
        // Direct reserve ratio on f64 fields: 15.5 SOL / 62.0 tokens = 0.25
        let ctx = sample_context(0.0, 62.0, 15.5);
        assert!((ctx.estimated_price() - 0.25).abs() < 1e-9);

        // Zero denominator returns 0.0 (not NaN/inf).
        let zero_denom = sample_context(0.0, 0.0, 15.5);
        assert_eq!(zero_denom.estimated_price(), 0.0);
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
