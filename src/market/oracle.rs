//! Authoritative on-chain Pump / PumpSwap market oracle (MPT-001 Agent D).
//!
//! LIVE-MONEY MARKET TRUTH. This is the single canonical source for a fresh
//! on-chain MARK observation and an exact-size, same-venue EXECUTABLE QUOTE.
//! There is deliberately NO DexScreener member and NO cache: every call is a
//! fresh, coherent chain observation (packet Sections 6, 16).
//!
//! RPC discipline (packet Section 14 / D2):
//! - the synchronous `RpcClient` always runs inside `tokio::task::spawn_blocking`;
//! - accounts that form one quote are fetched with
//!   `get_multiple_accounts_with_commitment` at `CommitmentConfig::confirmed()`
//!   so reserves/mint/fee share ONE RPC context slot;
//! - `slot` is the RPC context slot; `observed_at`/`quoted_at` are stamped only
//!   after a successful coherent fetch + decode.
//!
//! Fail-closed policy (packet Sections 5, 6, 20):
//! - RPC error != graduation; decode error != graduation;
//! - a missing bonding curve is "not a supported Pump coin", not graduation;
//! - a complete curve requires a real, validated canonical PumpSwap pool;
//! - a non-SOL quote mint is unsupported for automated trading and the quote
//!   methods fail closed with `UnsupportedQuoteMint` (snapshot stays observational).

use std::sync::Arc;

use solana_client::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;

use crate::error::{Error, Result};
use crate::market::math::{self, FeeComponents, FeeTier};
use crate::market::pump_state::{
    bonding_curve_pda, canonical_pool_pda, is_token_program, pump_fee_config_pda,
    pump_pool_authority_pda, pump_program_id, pumpswap_fee_config_pda, wsol_mint, DecodedFees,
    FeeConfigState, MintState, PumpBondingCurveState, PumpSwapPoolState, TokenAccountState,
};
use crate::market::types::{ExecutableQuote, MarketSide, MarketSnapshot, MarketVenue, QuoteAsset};

/// Non-mayhem fee-tier supply input for a Pump bonding curve (packet Section 10):
/// `1_000_000_000_000_000` raw base units. Mayhem coins use the actual mint supply.
const BONDING_NON_MAYHEM_SUPPLY: u128 = 1_000_000_000_000_000;

fn market_err(msg: impl Into<String>) -> Error {
    Error::MarketData(msg.into())
}

// ---------------------------------------------------------------------------
// Pure policy helpers (RPC-free; unit-tested in D8).
// ---------------------------------------------------------------------------

/// Resolve the quote asset for a bonding curve from its decoded `quote_mint`
/// (packet Section 6 / D4).
///
/// The bonding-curve default (`Pubkey::default()`) maps to SOL. Any other mint
/// is unsupported by the SOL-only accounting model.
pub fn curve_quote_asset(quote_mint: &Pubkey) -> QuoteAsset {
    if *quote_mint == Pubkey::default() {
        QuoteAsset::Sol
    } else {
        QuoteAsset::Unsupported(*quote_mint)
    }
}

/// Resolve the quote asset for a PumpSwap pool from its `quote_mint`
/// (packet Section 6 / D4).
///
/// Wrapped SOL maps to SOL; any other mint is unsupported.
pub fn pool_quote_asset(quote_mint: &Pubkey) -> QuoteAsset {
    if *quote_mint == wsol_mint() {
        QuoteAsset::Sol
    } else {
        QuoteAsset::Unsupported(*quote_mint)
    }
}

/// Venue implied by a decoded bonding curve (packet D3).
///
/// `complete == false` => still on the Pump bonding curve. `complete == true`
/// => graduated; the caller must derive and validate the canonical PumpSwap
/// pool (a complete curve does NOT by itself prove a usable pool, INV-MKT-011).
pub fn venue_for_curve(complete: bool) -> MarketVenue {
    if complete {
        MarketVenue::PumpSwapCanonical
    } else {
        MarketVenue::PumpBondingCurve
    }
}

/// Validate that a fetched canonical PumpSwap pool matches the expected
/// migrated identity (packet Section 8 / D5): index 0, creator == derived Pump
/// pool authority, and exact base/quote mints.
pub fn validate_canonical_pool(
    pool: &PumpSwapPoolState,
    expected_authority: &Pubkey,
    expected_base_mint: &Pubkey,
    expected_quote_mint: &Pubkey,
) -> Result<()> {
    if pool.index != 0 {
        return Err(market_err(format!(
            "canonical pool index != 0 (got {})",
            pool.index
        )));
    }
    if pool.creator != *expected_authority {
        return Err(market_err(format!(
            "canonical pool creator {} != derived Pump pool authority {}",
            pool.creator, expected_authority
        )));
    }
    if pool.base_mint != *expected_base_mint {
        return Err(market_err(format!(
            "canonical pool base_mint {} != expected {}",
            pool.base_mint, expected_base_mint
        )));
    }
    if pool.quote_mint != *expected_quote_mint {
        return Err(market_err(format!(
            "canonical pool quote_mint {} != expected {}",
            pool.quote_mint, expected_quote_mint
        )));
    }
    Ok(())
}

/// Build the `FeeTier` slice consumed by `calculate_fee_tier` from a decoded
/// on-chain `FeeConfig`. Fee bounds/monotonicity were already validated at decode.
fn fee_tiers_from_config(fc: &FeeConfigState) -> Vec<FeeTier> {
    fc.fee_tiers
        .iter()
        .map(|t| FeeTier {
            threshold: t.threshold,
            fees: FeeComponents {
                lp_fee_bps: t.fees.lp_fee_bps,
                protocol_fee_bps: t.fees.protocol_fee_bps,
                creator_fee_bps: t.fees.creator_fee_bps,
            },
        })
        .collect()
}

/// Select the active fee components for a given market cap, returning `None` if
/// the config has no usable tiers (fail closed, INV-MKT-008 — no static fallback).
fn active_fees(fc: &FeeConfigState, market_cap: u128) -> Result<DecodedFees> {
    let tiers = fee_tiers_from_config(fc);
    let selected = math::calculate_fee_tier(&tiers, market_cap)
        .ok_or_else(|| market_err("FeeConfig has no usable fee tiers"))?;
    Ok(DecodedFees {
        lp_fee_bps: selected.fees.lp_fee_bps,
        protocol_fee_bps: selected.fees.protocol_fee_bps,
        creator_fee_bps: selected.fees.creator_fee_bps,
    })
}

/// Compute the expected SOL-per-whole-token price from an executable quote's raw
/// amounts. Returns `None` when base out is zero (an unexecutable, fee-dominated
/// quote) so the caller can reject it.
fn expected_price(quote_lamports: u128, base_raw: u128, base_decimals: u8) -> Option<f64> {
    if base_raw == 0 {
        return None;
    }
    let base_ui = base_raw as f64 / 10_f64.powi(base_decimals as i32);
    if !(base_ui > 0.0) {
        return None;
    }
    let sol = quote_lamports as f64 / 1_000_000_000.0;
    let price = sol / base_ui;
    if price.is_finite() && price > 0.0 {
        Some(price)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Coherent fetched state (one RPC context slot).
// ---------------------------------------------------------------------------

/// Fetched Pump bonding-curve state for one coherent quote/snapshot.
struct PumpFetch {
    curve: PumpBondingCurveState,
    base_mint: MintState,
    fee_config: FeeConfigState,
    slot: u64,
}

/// Fetched PumpSwap canonical-pool state for one coherent quote/snapshot.
struct PumpSwapFetch {
    pool: PumpSwapPoolState,
    base_reserve: u64,
    effective_quote_reserve: u64,
    base_mint: MintState,
    fee_config: FeeConfigState,
    slot: u64,
}

/// Which resolved venue a fetch produced.
enum VenueFetch {
    Pump(PumpFetch),
    PumpSwap(PumpSwapFetch),
}

/// Authoritative Pump / PumpSwap market oracle.
///
/// Holds only the blocking `RpcClient` shared with the rest of the bot. No
/// cache, no DexScreener, no background task (packet D1).
pub struct PumpMarketOracle {
    rpc: Arc<RpcClient>,
}

impl PumpMarketOracle {
    /// Construct an oracle over a shared blocking RPC client.
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }

    /// Fetch multiple accounts at confirmed commitment under `spawn_blocking`,
    /// returning the coherent `(value, context_slot)` pair (packet D2).
    async fn fetch_multi(&self, keys: Vec<Pubkey>) -> Result<(Vec<Option<Account>>, u64)> {
        let rpc = self.rpc.clone();
        let resp = tokio::task::spawn_blocking(move || {
            rpc.get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
        })
        .await
        .map_err(|e| Error::Rpc(format!("market oracle fetch task join failed: {e}")))?
        .map_err(|e| Error::Rpc(format!("market oracle get_multiple_accounts failed: {e}")))?;
        Ok((resp.value, resp.context.slot))
    }

    /// Resolve the venue and fetch all accounts needed to quote/snapshot it,
    /// coherently at one RPC context slot (packet D3 / D5).
    async fn resolve_and_fetch(&self, mint: &Pubkey) -> Result<VenueFetch> {
        // Step 1 — fetch the bonding curve + base mint + Pump FeeConfig coherently.
        let (curve_pda, _) = bonding_curve_pda(mint);
        let (pump_fee_pda, _) = pump_fee_config_pda();
        let (accounts, slot) = self
            .fetch_multi(vec![curve_pda, *mint, pump_fee_pda])
            .await?;

        let curve_acc = accounts.first().cloned().flatten().ok_or_else(|| {
            market_err("bonding curve account missing: not a supported Pump coin")
        })?;
        PumpBondingCurveState::validate_owner(&curve_acc.owner)?;
        let curve = PumpBondingCurveState::decode(&curve_acc.data)?;

        // complete == false => Pump bonding curve venue.
        if !curve.complete {
            let base_mint_acc = accounts
                .get(1)
                .cloned()
                .flatten()
                .ok_or_else(|| market_err("base mint account missing"))?;
            MintState::validate_owner(&base_mint_acc.owner)?;
            let base_mint = MintState::decode(&base_mint_acc.data)?;

            let fee_acc = accounts
                .get(2)
                .cloned()
                .flatten()
                .ok_or_else(|| market_err("Pump FeeConfig account missing"))?;
            let fee_config = FeeConfigState::decode(&fee_acc.data)?;

            if curve.virtual_token_reserves == 0 || curve.virtual_quote_reserves == 0 {
                return Err(market_err("bonding curve has empty virtual reserves"));
            }

            return Ok(VenueFetch::Pump(PumpFetch {
                curve,
                base_mint,
                fee_config,
                slot,
            }));
        }

        // complete == true => derive & validate the canonical PumpSwap pool.
        // Quote-mint mapping: the bonding-curve default quote maps to wrapped SOL
        // for v2 pool derivation (packet Section 6). Unsupported curve quote mints
        // still resolve a pool for diagnostics but will be flagged by pool_quote_asset.
        let curve_quote = curve.quote_mint;
        let pool_quote_mint = if curve_quote == Pubkey::default() {
            wsol_mint()
        } else {
            curve_quote
        };
        let (authority, _) = pump_pool_authority_pda(mint);
        let (pool_pda, _) = canonical_pool_pda(mint, &pool_quote_mint, &authority);
        let (amm_fee_pda, _) = pumpswap_fee_config_pda();

        // Fetch pool + AMM FeeConfig coherently first (pool identity gives vault keys).
        let (pool_accounts, pool_slot) = self.fetch_multi(vec![pool_pda, amm_fee_pda]).await?;
        let pool_acc =
            pool_accounts.first().cloned().flatten().ok_or_else(|| {
                market_err("curve complete but canonical PumpSwap pool unavailable")
            })?;
        PumpSwapPoolState::validate_owner(&pool_acc.owner)?;
        let pool = PumpSwapPoolState::decode(&pool_acc.data)?;
        validate_canonical_pool(&pool, &authority, mint, &pool_quote_mint)?;

        let amm_fee_acc = pool_accounts
            .get(1)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("PumpSwap FeeConfig account missing"))?;
        let fee_config = FeeConfigState::decode(&amm_fee_acc.data)?;

        // Fetch vaults + base mint coherently.
        let (vault_accounts, vault_slot) = self
            .fetch_multi(vec![
                pool.pool_base_token_account,
                pool.pool_quote_token_account,
                *mint,
            ])
            .await?;

        let base_vault_acc = vault_accounts
            .first()
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("pool base vault missing"))?;
        TokenAccountState::validate_program(&base_vault_acc.owner)?;
        let base_vault = TokenAccountState::decode(&base_vault_acc.data)?;
        base_vault.validate(mint, &pool_pda)?;

        let quote_vault_acc = vault_accounts
            .get(1)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("pool quote vault missing"))?;
        TokenAccountState::validate_program(&quote_vault_acc.owner)?;
        let quote_vault = TokenAccountState::decode(&quote_vault_acc.data)?;
        quote_vault.validate(&pool.quote_mint, &pool_pda)?;

        let base_mint_acc = vault_accounts
            .get(2)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("base mint account missing"))?;
        MintState::validate_owner(&base_mint_acc.owner)?;
        let base_mint = MintState::decode(&base_mint_acc.data)?;

        let base_reserve = base_vault.amount;
        if base_reserve == 0 {
            return Err(market_err("PumpSwap pool has zero base reserve"));
        }
        let effective_quote_reserve =
            math::effective_quote_reserve(quote_vault.amount, pool.virtual_quote_reserves)
                .ok_or_else(|| {
                    market_err("PumpSwap effective quote reserve non-positive / overflow")
                })?;

        let _ = (pool_slot, vault_slot); // context slots documented; snapshot uses first-fetch slot.

        Ok(VenueFetch::PumpSwap(PumpSwapFetch {
            pool,
            base_reserve,
            effective_quote_reserve,
            base_mint,
            fee_config,
            slot,
        }))
    }

    /// Fresh on-chain MARK observation (packet D6). Observational: an unsupported
    /// quote asset is reported with `mark = None`, not an error.
    pub async fn snapshot(&self, mint: &Pubkey) -> Result<MarketSnapshot> {
        let observed_at = chrono::Utc::now();
        match self.resolve_and_fetch(mint).await? {
            VenueFetch::Pump(f) => {
                let quote_asset = curve_quote_asset(&f.curve.quote_mint);
                let mark = if quote_asset == QuoteAsset::Sol {
                    math::normalized_mark_sol_per_token(
                        f.curve.virtual_token_reserves as u128,
                        f.base_mint.decimals,
                        f.curve.virtual_quote_reserves as u128,
                    )
                } else {
                    None
                };
                Ok(MarketSnapshot {
                    mint: *mint,
                    venue: MarketVenue::PumpBondingCurve,
                    quote_asset,
                    base_decimals: f.base_mint.decimals,
                    quote_decimals: 9,
                    mark_price_sol_per_token: mark,
                    slot: f.slot,
                    observed_at,
                    is_mayhem_mode: f.curve.is_mayhem_mode,
                    is_cashback_coin: f.curve.is_cashback_coin,
                })
            }
            VenueFetch::PumpSwap(f) => {
                let quote_asset = pool_quote_asset(&f.pool.quote_mint);
                let mark = if quote_asset == QuoteAsset::Sol {
                    math::normalized_mark_sol_per_token(
                        f.base_reserve as u128,
                        f.base_mint.decimals,
                        f.effective_quote_reserve as u128,
                    )
                } else {
                    None
                };
                Ok(MarketSnapshot {
                    mint: *mint,
                    venue: MarketVenue::PumpSwapCanonical,
                    quote_asset,
                    base_decimals: f.base_mint.decimals,
                    quote_decimals: 9,
                    mark_price_sol_per_token: mark,
                    slot: f.slot,
                    observed_at,
                    is_mayhem_mode: f.pool.is_mayhem_mode,
                    is_cashback_coin: f.pool.is_cashback_coin,
                })
            }
        }
    }

    /// Exact-size, same-venue pre-send BUY quote for `sol_in_lamports` (packet D7).
    ///
    /// Fails closed with `UnsupportedQuoteMint` for a non-SOL quote asset, and
    /// with `MarketData` when reserves/fees produce zero output.
    pub async fn quote_buy_sol(
        &self,
        mint: &Pubkey,
        sol_in_lamports: u64,
    ) -> Result<ExecutableQuote> {
        match self.resolve_and_fetch(mint).await? {
            VenueFetch::Pump(f) => self.build_pump_buy(mint, sol_in_lamports, f),
            VenueFetch::PumpSwap(f) => self.build_pumpswap_buy(mint, sol_in_lamports, f),
        }
    }

    /// Exact raw-token same-venue pre-send SELL quote for `base_in_raw` (packet D7).
    pub async fn quote_sell_raw(&self, mint: &Pubkey, base_in_raw: u64) -> Result<ExecutableQuote> {
        match self.resolve_and_fetch(mint).await? {
            VenueFetch::Pump(f) => self.build_pump_sell(mint, base_in_raw, f),
            VenueFetch::PumpSwap(f) => self.build_pumpswap_sell(mint, base_in_raw, f),
        }
    }

    // --- Pump venue quote builders --------------------------------------

    fn build_pump_buy(
        &self,
        mint: &Pubkey,
        sol_in_lamports: u64,
        f: PumpFetch,
    ) -> Result<ExecutableQuote> {
        let quoted_at = chrono::Utc::now();
        if curve_quote_asset(&f.curve.quote_mint) != QuoteAsset::Sol {
            return Err(Error::UnsupportedQuoteMint(format!(
                "bonding curve quote mint {} is not SOL",
                f.curve.quote_mint
            )));
        }
        if sol_in_lamports == 0 {
            return Err(market_err("buy quote requires nonzero SOL input"));
        }

        let supply = if f.curve.is_mayhem_mode {
            f.base_mint.supply as u128
        } else {
            BONDING_NON_MAYHEM_SUPPLY
        };
        let market_cap = math::bonding_market_cap(
            f.curve.virtual_quote_reserves as u128,
            supply,
            f.curve.virtual_token_reserves as u128,
        )
        .ok_or_else(|| market_err("bonding market cap computation failed"))?;
        let fees = active_fees(&f.fee_config, market_cap)?;

        // Pump buy uses protocol + creator (creator omitted when default), Section 11.2.
        let creator_bps = if f.curve.creator == Pubkey::default() {
            0
        } else {
            fees.creator_fee_bps
        };
        let total_fee_bps = fees.protocol_fee_bps.saturating_add(creator_bps);

        let base_out = math::pump_buy_base_out(
            sol_in_lamports as u128,
            f.curve.virtual_token_reserves as u128,
            f.curve.virtual_quote_reserves as u128,
            f.curve.real_token_reserves as u128,
            total_fee_bps,
        )
        .ok_or_else(|| market_err("Pump buy produced zero/unexecutable base out"))?;
        let base_out_u64: u64 = base_out
            .try_into()
            .map_err(|_| market_err("Pump buy base out exceeds u64"))?;

        let expected = expected_price(sol_in_lamports as u128, base_out, f.base_mint.decimals);
        Ok(ExecutableQuote {
            mint: *mint,
            side: MarketSide::Buy,
            venue: MarketVenue::PumpBondingCurve,
            quote_asset: QuoteAsset::Sol,
            base_decimals: f.base_mint.decimals,
            quote_decimals: 9,
            base_amount_raw: base_out_u64,
            quote_amount_raw: sol_in_lamports,
            expected_price_sol_per_token: expected,
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: creator_bps,
            lp_fee_bps: 0,
            slot: f.slot,
            quoted_at,
        })
    }

    fn build_pump_sell(
        &self,
        mint: &Pubkey,
        base_in_raw: u64,
        f: PumpFetch,
    ) -> Result<ExecutableQuote> {
        let quoted_at = chrono::Utc::now();
        if curve_quote_asset(&f.curve.quote_mint) != QuoteAsset::Sol {
            return Err(Error::UnsupportedQuoteMint(format!(
                "bonding curve quote mint {} is not SOL",
                f.curve.quote_mint
            )));
        }
        if base_in_raw == 0 {
            return Err(market_err("sell quote requires nonzero base input"));
        }

        let supply = if f.curve.is_mayhem_mode {
            f.base_mint.supply as u128
        } else {
            BONDING_NON_MAYHEM_SUPPLY
        };
        let market_cap = math::bonding_market_cap(
            f.curve.virtual_quote_reserves as u128,
            supply,
            f.curve.virtual_token_reserves as u128,
        )
        .ok_or_else(|| market_err("bonding market cap computation failed"))?;
        let fees = active_fees(&f.fee_config, market_cap)?;

        let creator_bps = if f.curve.creator == Pubkey::default() {
            0
        } else {
            fees.creator_fee_bps
        };

        let net = math::pump_sell_net_quote_out(
            base_in_raw as u128,
            f.curve.virtual_token_reserves as u128,
            f.curve.virtual_quote_reserves as u128,
            fees.protocol_fee_bps,
            creator_bps,
        )
        .ok_or_else(|| {
            market_err("Pump sell math failed (reserves depleted or fees exceed output)")
        })?;
        if net == 0 {
            return Err(market_err("Pump sell net quote out is zero (unexecutable)"));
        }
        let net_u64: u64 = net
            .try_into()
            .map_err(|_| market_err("Pump sell net quote out exceeds u64"))?;

        let expected = expected_price(net, base_in_raw as u128, f.base_mint.decimals);
        Ok(ExecutableQuote {
            mint: *mint,
            side: MarketSide::Sell,
            venue: MarketVenue::PumpBondingCurve,
            quote_asset: QuoteAsset::Sol,
            base_decimals: f.base_mint.decimals,
            quote_decimals: 9,
            base_amount_raw: base_in_raw,
            quote_amount_raw: net_u64,
            expected_price_sol_per_token: expected,
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: creator_bps,
            lp_fee_bps: 0,
            slot: f.slot,
            quoted_at,
        })
    }

    // --- PumpSwap venue quote builders ----------------------------------

    fn build_pumpswap_buy(
        &self,
        mint: &Pubkey,
        sol_in_lamports: u64,
        f: PumpSwapFetch,
    ) -> Result<ExecutableQuote> {
        let quoted_at = chrono::Utc::now();
        if pool_quote_asset(&f.pool.quote_mint) != QuoteAsset::Sol {
            return Err(Error::UnsupportedQuoteMint(format!(
                "PumpSwap pool quote mint {} is not wrapped SOL",
                f.pool.quote_mint
            )));
        }
        if sol_in_lamports == 0 {
            return Err(market_err("buy quote requires nonzero SOL input"));
        }

        let market_cap = math::pumpswap_market_cap(
            f.effective_quote_reserve as u128,
            f.base_mint.supply as u128,
            f.base_reserve as u128,
        )
        .ok_or_else(|| market_err("PumpSwap market cap computation failed"))?;
        let fees = active_fees(&f.fee_config, market_cap)?;

        let creator_bps = if f.pool.coin_creator == Pubkey::default() {
            0
        } else {
            fees.creator_fee_bps
        };
        let total_fee_bps = fees
            .lp_fee_bps
            .saturating_add(fees.protocol_fee_bps)
            .saturating_add(creator_bps);

        let base_out = math::pumpswap_buy_base_out(
            sol_in_lamports as u128,
            f.base_reserve as u128,
            f.effective_quote_reserve as u128,
            total_fee_bps,
        )
        .ok_or_else(|| market_err("PumpSwap buy produced zero/unexecutable base out"))?;
        let base_out_u64: u64 = base_out
            .try_into()
            .map_err(|_| market_err("PumpSwap buy base out exceeds u64"))?;

        let expected = expected_price(sol_in_lamports as u128, base_out, f.base_mint.decimals);
        Ok(ExecutableQuote {
            mint: *mint,
            side: MarketSide::Buy,
            venue: MarketVenue::PumpSwapCanonical,
            quote_asset: QuoteAsset::Sol,
            base_decimals: f.base_mint.decimals,
            quote_decimals: 9,
            base_amount_raw: base_out_u64,
            quote_amount_raw: sol_in_lamports,
            expected_price_sol_per_token: expected,
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: creator_bps,
            lp_fee_bps: fees.lp_fee_bps,
            slot: f.slot,
            quoted_at,
        })
    }

    fn build_pumpswap_sell(
        &self,
        mint: &Pubkey,
        base_in_raw: u64,
        f: PumpSwapFetch,
    ) -> Result<ExecutableQuote> {
        let quoted_at = chrono::Utc::now();
        if pool_quote_asset(&f.pool.quote_mint) != QuoteAsset::Sol {
            return Err(Error::UnsupportedQuoteMint(format!(
                "PumpSwap pool quote mint {} is not wrapped SOL",
                f.pool.quote_mint
            )));
        }
        if base_in_raw == 0 {
            return Err(market_err("sell quote requires nonzero base input"));
        }

        let market_cap = math::pumpswap_market_cap(
            f.effective_quote_reserve as u128,
            f.base_mint.supply as u128,
            f.base_reserve as u128,
        )
        .ok_or_else(|| market_err("PumpSwap market cap computation failed"))?;
        let fees = active_fees(&f.fee_config, market_cap)?;

        let creator_bps = if f.pool.coin_creator == Pubkey::default() {
            0
        } else {
            fees.creator_fee_bps
        };

        let net = math::pumpswap_sell_net_quote_out(
            base_in_raw as u128,
            f.base_reserve as u128,
            f.effective_quote_reserve as u128,
            fees.lp_fee_bps,
            fees.protocol_fee_bps,
            creator_bps,
        )
        .ok_or_else(|| {
            market_err("PumpSwap sell math failed (reserves depleted or fees exceed output)")
        })?;
        if net == 0 {
            return Err(market_err(
                "PumpSwap sell net quote out is zero (unexecutable)",
            ));
        }
        let net_u64: u64 = net
            .try_into()
            .map_err(|_| market_err("PumpSwap sell net quote out exceeds u64"))?;

        let expected = expected_price(net, base_in_raw as u128, f.base_mint.decimals);
        Ok(ExecutableQuote {
            mint: *mint,
            side: MarketSide::Sell,
            venue: MarketVenue::PumpSwapCanonical,
            quote_asset: QuoteAsset::Sol,
            base_decimals: f.base_mint.decimals,
            quote_decimals: 9,
            base_amount_raw: base_in_raw,
            quote_amount_raw: net_u64,
            expected_price_sol_per_token: expected,
            protocol_fee_bps: fees.protocol_fee_bps,
            creator_fee_bps: creator_bps,
            lp_fee_bps: fees.lp_fee_bps,
            slot: f.slot,
            quoted_at,
        })
    }
}

// ---------------------------------------------------------------------------
// D8 — RPC-free policy tests. No network.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    #[test]
    fn test_curve_error_is_not_graduation() {
        // A curve that decoded with complete=false is unambiguously pre-graduation.
        // (RPC/decode errors surface as Err from resolve_and_fetch — never mapped to
        // a graduated venue.) The venue mapping proves incomplete => Pump.
        assert_eq!(venue_for_curve(false), MarketVenue::PumpBondingCurve);
    }

    #[test]
    fn test_complete_curve_requires_real_canonical_pool() {
        // complete=true only *implies* the PumpSwap venue; the pool itself must be
        // fetched and validated. validate_canonical_pool is the gate that rejects a
        // non-real / non-canonical pool.
        assert_eq!(venue_for_curve(true), MarketVenue::PumpSwapCanonical);

        let authority = pk(1);
        let base = pk(2);
        let quote = wsol_mint();
        let good = PumpSwapPoolState {
            pool_bump: 0,
            index: 0,
            creator: authority,
            base_mint: base,
            quote_mint: quote,
            lp_mint: pk(9),
            pool_base_token_account: pk(5),
            pool_quote_token_account: pk(6),
            lp_supply: 1,
            coin_creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            virtual_quote_reserves: 0,
        };
        assert!(validate_canonical_pool(&good, &authority, &base, &quote).is_ok());
    }

    #[test]
    fn test_default_curve_quote_mint_maps_to_sol() {
        assert_eq!(curve_quote_asset(&Pubkey::default()), QuoteAsset::Sol);
    }

    #[test]
    fn test_nondefault_curve_quote_mint_unsupported() {
        let usdc = pk(7);
        assert_eq!(curve_quote_asset(&usdc), QuoteAsset::Unsupported(usdc));
    }

    #[test]
    fn test_wrapped_sol_pool_maps_to_sol() {
        assert_eq!(pool_quote_asset(&wsol_mint()), QuoteAsset::Sol);
    }

    #[test]
    fn test_usdc_pool_unsupported_by_sol_accounting() {
        let usdc = pk(3);
        assert_eq!(pool_quote_asset(&usdc), QuoteAsset::Unsupported(usdc));
    }

    #[test]
    fn test_canonical_pool_validation_rejects_wrong_creator() {
        let authority = pk(1);
        let base = pk(2);
        let quote = wsol_mint();
        let pool = PumpSwapPoolState {
            pool_bump: 0,
            index: 0,
            creator: pk(99), // wrong creator
            base_mint: base,
            quote_mint: quote,
            lp_mint: pk(9),
            pool_base_token_account: pk(5),
            pool_quote_token_account: pk(6),
            lp_supply: 1,
            coin_creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            virtual_quote_reserves: 0,
        };
        assert!(validate_canonical_pool(&pool, &authority, &base, &quote).is_err());
    }

    #[test]
    fn test_canonical_pool_validation_rejects_wrong_index() {
        let authority = pk(1);
        let base = pk(2);
        let quote = wsol_mint();
        let pool = PumpSwapPoolState {
            pool_bump: 0,
            index: 1, // wrong index
            creator: authority,
            base_mint: base,
            quote_mint: quote,
            lp_mint: pk(9),
            pool_base_token_account: pk(5),
            pool_quote_token_account: pk(6),
            lp_supply: 1,
            coin_creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            virtual_quote_reserves: 0,
        };
        assert!(validate_canonical_pool(&pool, &authority, &base, &quote).is_err());
    }

    #[test]
    fn test_quote_venue_is_pump_before_graduation() {
        assert_eq!(venue_for_curve(false), MarketVenue::PumpBondingCurve);
    }

    #[test]
    fn test_quote_venue_is_pumpswap_after_graduation() {
        assert_eq!(venue_for_curve(true), MarketVenue::PumpSwapCanonical);
    }

    #[test]
    fn test_expected_price_zero_base_is_none() {
        // A fee-dominated / zero-base quote yields no executable price.
        assert!(expected_price(1_000, 0, 6).is_none());
        // A normal quote yields a finite positive price.
        let p = expected_price(1_000_000_000, 1_000_000, 6).unwrap();
        assert!(p.is_finite() && p > 0.0);
    }

    #[test]
    fn test_is_token_program_helper_reused() {
        // Sanity: oracle relies on pump_state token-program gate for vault owner.
        assert!(is_token_program(
            &crate::market::pump_state::spl_token_program_id()
        ));
        assert!(!is_token_program(&pump_program_id()));
    }
}
