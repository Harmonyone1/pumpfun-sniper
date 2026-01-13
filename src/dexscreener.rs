// DexScreener API client for hot token discovery
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const DEXSCREENER_BASE: &str = "https://api.dexscreener.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenProfile {
    pub url: Option<String>,
    #[serde(rename = "chainId")]
    pub chain_id: String,
    #[serde(rename = "tokenAddress")]
    pub token_address: String,
    pub icon: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBoost {
    pub url: Option<String>,
    #[serde(rename = "chainId")]
    pub chain_id: String,
    #[serde(rename = "tokenAddress")]
    pub token_address: String,
    #[serde(rename = "totalAmount")]
    pub total_amount: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChange {
    pub m5: Option<f64>,
    pub h1: Option<f64>,
    pub h6: Option<f64>,
    pub h24: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Txns {
    pub m5: Option<TxnCount>,
    pub h1: Option<TxnCount>,
    pub h6: Option<TxnCount>,
    pub h24: Option<TxnCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxnCount {
    pub buys: u32,
    pub sells: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Liquidity {
    pub usd: Option<f64>,
    pub base: Option<f64>,
    pub quote: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub m5: Option<f64>,
    pub h1: Option<f64>,
    pub h6: Option<f64>,
    pub h24: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseToken {
    pub address: String,
    pub name: Option<String>,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DexPair {
    #[serde(rename = "chainId")]
    pub chain_id: String,
    #[serde(rename = "dexId")]
    pub dex_id: String,
    pub url: Option<String>,
    #[serde(rename = "pairAddress")]
    pub pair_address: String,
    #[serde(rename = "baseToken")]
    pub base_token: BaseToken,
    #[serde(rename = "priceNative")]
    pub price_native: Option<String>,
    #[serde(rename = "priceUsd")]
    pub price_usd: Option<String>,
    #[serde(rename = "priceChange")]
    pub price_change: Option<PriceChange>,
    pub txns: Option<Txns>,
    pub volume: Option<Volume>,
    pub liquidity: Option<Liquidity>,
    #[serde(rename = "marketCap")]
    pub market_cap: Option<f64>,
    #[serde(rename = "fdv")]
    pub fdv: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPairsResponse {
    pub pairs: Option<Vec<DexPair>>,
}

/// Hot token opportunity with calculated metrics
#[derive(Debug, Clone)]
pub struct HotToken {
    pub mint: String,
    pub symbol: String,
    pub name: String,
    pub price_native: f64,
    pub m5_change: f64,
    pub h1_change: f64,
    pub buys_5m: u32,
    pub sells_5m: u32,
    pub buy_sell_ratio: f64,
    pub market_cap: f64,
    pub liquidity_usd: f64,
    pub volume_h1: f64,
    pub is_boosted: bool,
    pub boost_amount: f64,
    pub dex_id: String,
}

impl HotToken {
    /// Check if this is a pump.fun token (mint ends with "pump")
    pub fn is_pumpfun(&self) -> bool {
        self.mint.ends_with("pump")
    }

    /// Check if this token meets hot criteria
    pub fn is_hot(&self, config: &HotScanConfig) -> bool {
        // Must be a pump.fun token (tradable via PumpPortal)
        if !self.is_pumpfun() {
            return false;
        }

        // Basic filters
        let basic = self.m5_change >= config.min_m5_change
            && self.buy_sell_ratio >= config.min_buy_sell_ratio
            && self.buys_5m >= config.min_buys_5m
            && self.liquidity_usd >= config.min_liquidity_usd
            && self.market_cap >= config.min_market_cap
            && self.market_cap <= config.max_market_cap;

        if !basic {
            return false;
        }

        // NEW: Avoid dead cat bounces (H1 too negative)
        if self.h1_change < config.min_h1_change {
            return false;
        }

        // NEW: Avoid buying tops (M5 too high)
        if self.m5_change > config.max_m5_change {
            return false;
        }

        // NEW: Minimum score threshold
        if self.score() < config.min_score {
            return false;
        }

        // NEW: Cap ratio influence (avoid wash trading)
        // Tokens with >10:1 ratio are suspicious
        if self.buy_sell_ratio > config.max_buy_sell_ratio {
            return false;
        }

        true
    }

    /// Score this token for ranking (higher = better opportunity)
    pub fn score(&self) -> f64 {
        let momentum_score = self.m5_change * 2.0 + self.h1_change;
        let activity_score = (self.buys_5m as f64 - self.sells_5m as f64).max(0.0) * 5.0;
        // Cap ratio influence at 5.0 to avoid manipulation
        let capped_ratio = self.buy_sell_ratio.min(5.0);
        let ratio_score = (capped_ratio - 1.0).max(0.0) * 20.0;
        let boost_score = if self.is_boosted { 10.0 } else { 0.0 };
        // Bonus for positive H1 (sustained momentum)
        let h1_bonus = if self.h1_change > 0.0 {
            self.h1_change * 0.5
        } else {
            0.0
        };

        momentum_score + activity_score + ratio_score + boost_score + h1_bonus
    }
}

/// Configuration for hot token scanning
#[derive(Debug, Clone)]
pub struct HotScanConfig {
    pub min_m5_change: f64,      // Minimum 5-minute price change %
    pub max_m5_change: f64,      // Maximum 5-minute change (avoid buying tops)
    pub min_h1_change: f64,      // Minimum H1 change (avoid dead cat bounces)
    pub min_buy_sell_ratio: f64, // Minimum buy/sell ratio
    pub max_buy_sell_ratio: f64, // Maximum ratio (avoid wash trading)
    pub min_buys_5m: u32,        // Minimum buys in 5 minutes
    pub min_liquidity_usd: f64,  // Minimum liquidity in USD
    pub min_market_cap: f64,     // Minimum market cap
    pub max_market_cap: f64,     // Maximum market cap (avoid too established)
    pub min_score: f64,          // Minimum score to consider
    pub scan_profiles: bool,     // Scan latest profiles
    pub scan_boosts: bool,       // Scan boosted tokens
    pub profile_limit: usize,    // How many profiles to check
    pub boost_limit: usize,      // How many boosts to check
}

impl Default for HotScanConfig {
    fn default() -> Self {
        Self {
            min_m5_change: 10.0,         // 10% gain in 5 minutes
            max_m5_change: 80.0,         // Don't buy after 80%+ pump (buying top)
            min_h1_change: -30.0,        // Reject if H1 < -30% (dead cat bounce)
            min_buy_sell_ratio: 1.3,     // 30% more buys than sells
            max_buy_sell_ratio: 10.0,    // Cap at 10:1 (avoid manipulation)
            min_buys_5m: 10,             // At least 10 buys in 5 min
            min_liquidity_usd: 10_000.0, // $10k liquidity
            min_market_cap: 20_000.0,    // $20k market cap minimum
            max_market_cap: 500_000.0,   // $500k max (avoid late entries)
            min_score: 50.0,             // Minimum score threshold
            scan_profiles: true,
            scan_boosts: true,
            profile_limit: 30,
            boost_limit: 15,
        }
    }
}

pub struct DexScreenerClient {
    client: reqwest::Client,
}

impl DexScreenerClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Fetch latest token profiles
    pub async fn get_latest_profiles(&self) -> Result<Vec<TokenProfile>> {
        let url = format!("{}/token-profiles/latest/v1", DEXSCREENER_BASE);
        let resp = self.client.get(&url).send().await?;
        let profiles: Vec<TokenProfile> = resp.json().await?;
        Ok(profiles)
    }

    /// Fetch top boosted tokens
    pub async fn get_top_boosts(&self) -> Result<Vec<TokenBoost>> {
        let url = format!("{}/token-boosts/top/v1", DEXSCREENER_BASE);
        let resp = self.client.get(&url).send().await?;
        let boosts: Vec<TokenBoost> = resp.json().await?;
        Ok(boosts)
    }

    /// Fetch token pairs/details
    pub async fn get_token_pairs(&self, mint: &str) -> Result<Option<DexPair>> {
        let url = format!("{}/latest/dex/tokens/{}", DEXSCREENER_BASE, mint);
        let resp = self.client.get(&url).send().await?;
        let data: TokenPairsResponse = resp.json().await?;

        // Prefer pumpswap/pumpfun pairs
        if let Some(pairs) = data.pairs {
            let pair = pairs
                .iter()
                .find(|p| p.dex_id == "pumpswap" || p.dex_id == "pumpfun")
                .or_else(|| pairs.first())
                .cloned();
            return Ok(pair);
        }
        Ok(None)
    }

    /// Convert DexPair to HotToken with metrics
    fn pair_to_hot_token(
        &self,
        mint: &str,
        pair: &DexPair,
        is_boosted: bool,
        boost_amount: f64,
    ) -> HotToken {
        let price_native = pair
            .price_native
            .as_ref()
            .and_then(|p| p.parse::<f64>().ok())
            .unwrap_or(0.0);

        let m5_change = pair
            .price_change
            .as_ref()
            .and_then(|pc| pc.m5)
            .unwrap_or(0.0);

        let h1_change = pair
            .price_change
            .as_ref()
            .and_then(|pc| pc.h1)
            .unwrap_or(0.0);

        let (buys_5m, sells_5m) = pair
            .txns
            .as_ref()
            .and_then(|t| t.m5.as_ref())
            .map(|m5| (m5.buys, m5.sells))
            .unwrap_or((0, 0));

        let buy_sell_ratio = if sells_5m > 0 {
            buys_5m as f64 / sells_5m as f64
        } else {
            buys_5m as f64
        };

        let liquidity_usd = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);

        let volume_h1 = pair.volume.as_ref().and_then(|v| v.h1).unwrap_or(0.0);

        HotToken {
            mint: mint.to_string(),
            symbol: pair
                .base_token
                .symbol
                .clone()
                .unwrap_or_else(|| "???".to_string()),
            name: pair
                .base_token
                .name
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
            price_native,
            m5_change,
            h1_change,
            buys_5m,
            sells_5m,
            buy_sell_ratio,
            market_cap: pair.market_cap.unwrap_or(0.0),
            liquidity_usd,
            volume_h1,
            is_boosted,
            boost_amount,
            dex_id: pair.dex_id.clone(),
        }
    }

    /// Scan for hot tokens using configured criteria
    pub async fn scan_hot_tokens(&self, config: &HotScanConfig) -> Result<Vec<HotToken>> {
        let mut hot_tokens = Vec::new();
        let mut seen_mints = std::collections::HashSet::new();

        // Scan latest profiles
        if config.scan_profiles {
            debug!("Scanning latest token profiles...");
            match self.get_latest_profiles().await {
                Ok(profiles) => {
                    let solana_profiles: Vec<_> = profiles
                        .into_iter()
                        .filter(|p| p.chain_id == "solana")
                        .take(config.profile_limit)
                        .collect();

                    info!(
                        "Checking {} Solana profiles from DexScreener",
                        solana_profiles.len()
                    );

                    for profile in solana_profiles {
                        if seen_mints.contains(&profile.token_address) {
                            continue;
                        }
                        seen_mints.insert(profile.token_address.clone());

                        if let Ok(Some(pair)) = self.get_token_pairs(&profile.token_address).await {
                            let hot =
                                self.pair_to_hot_token(&profile.token_address, &pair, false, 0.0);
                            if hot.is_hot(config) {
                                hot_tokens.push(hot);
                            }
                        }

                        // Rate limiting
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                Err(e) => warn!("Failed to fetch profiles: {}", e),
            }
        }

        // Scan boosted tokens
        if config.scan_boosts {
            debug!("Scanning boosted tokens...");
            match self.get_top_boosts().await {
                Ok(boosts) => {
                    let solana_boosts: Vec<_> = boosts
                        .into_iter()
                        .filter(|b| b.chain_id == "solana")
                        .take(config.boost_limit)
                        .collect();

                    info!("Checking {} boosted Solana tokens", solana_boosts.len());

                    for boost in solana_boosts {
                        if seen_mints.contains(&boost.token_address) {
                            continue;
                        }
                        seen_mints.insert(boost.token_address.clone());

                        if let Ok(Some(pair)) = self.get_token_pairs(&boost.token_address).await {
                            let boost_amount = boost.total_amount.unwrap_or(0.0);
                            let hot = self.pair_to_hot_token(
                                &boost.token_address,
                                &pair,
                                true,
                                boost_amount,
                            );
                            if hot.is_hot(config) {
                                hot_tokens.push(hot);
                            }
                        }

                        // Rate limiting
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                Err(e) => warn!("Failed to fetch boosts: {}", e),
            }
        }

        // Sort by score (best opportunities first)
        hot_tokens.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(hot_tokens)
    }

    /// Get detailed info for a specific token
    pub async fn get_token_info(&self, mint: &str) -> Result<Option<HotToken>> {
        if let Some(pair) = self.get_token_pairs(mint).await? {
            Ok(Some(self.pair_to_hot_token(mint, &pair, false, 0.0)))
        } else {
            Ok(None)
        }
    }
}

impl Default for DexScreenerClient {
    fn default() -> Self {
        Self::new()
    }
}
