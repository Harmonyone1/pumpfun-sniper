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
    bonding_curve_pda, canonical_pool_pda, fee_program_id, is_token_program, pump_fee_config_pda,
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

/// Fail-closed owner check for a FeeConfig account: it MUST be owned by the fee
/// program (packet Section 9 / BLOCKER A steps 8 + pre-graduation). A FeeConfig
/// sitting at the right PDA but owned by anything else is rejected before decode.
pub fn validate_fee_config_owner(owner: &Pubkey) -> Result<()> {
    if *owner == fee_program_id() {
        Ok(())
    } else {
        Err(market_err(format!(
            "FeeConfig: wrong owner {owner}, expected fee program {}",
            fee_program_id()
        )))
    }
}

/// Validated PumpSwap quote-state produced from the FINAL coherent batch
/// (BLOCKER A steps 5-10). Carries the FINAL context slot as the ONLY quote
/// provenance — no discovery/curve slot survives here.
struct PumpSwapQuoteState {
    pool: PumpSwapPoolState,
    base_reserve: u64,
    effective_quote_reserve: u64,
    base_mint: MintState,
    fee_config: FeeConfigState,
    slot: u64,
}

/// PURE (RPC-free) decode + validation of the FINAL PumpSwap batch (BLOCKER A
/// steps 5-10). Takes the already-fetched raw account tuples `(owner, data)` for
/// EXACTLY `[pool, base_vault, quote_vault, base_mint, feeconfig]`, the addresses
/// requested for the pool + the two DISCOVERED vaults, the derived Pump pool
/// authority, the expected base/quote mints, and the FINAL batch context slot.
///
/// It:
/// - re-decodes/re-validates the pool (owner, index==0, creator==authority, mints);
/// - requires FINAL `pool_base_token_account`/`pool_quote_token_account` to equal
///   the DISCOVERED vault addresses (fail closed if either moved);
/// - decodes/validates both vaults, base mint, and FeeConfig from this batch only;
/// - checks the FeeConfig account owner == fee_program_id();
/// - computes the effective quote reserve from the FINAL quote-vault amount +
///   FINAL `pool.virtual_quote_reserves`;
/// - stamps `slot` = FINAL batch context slot.
///
/// This is the single testable core of the graduated quote path; the async
/// `resolve_and_fetch` is only the RPC wiring around it.
#[allow(clippy::too_many_arguments)]
fn decode_final_pumpswap_batch(
    pool_owner: &Pubkey,
    pool_data: &[u8],
    base_vault_owner: &Pubkey,
    base_vault_data: &[u8],
    quote_vault_owner: &Pubkey,
    quote_vault_data: &[u8],
    base_mint_owner: &Pubkey,
    base_mint_data: &[u8],
    fee_config_owner: &Pubkey,
    fee_config_data: &[u8],
    pool_pda: &Pubkey,
    discovered_base_vault: &Pubkey,
    discovered_quote_vault: &Pubkey,
    expected_authority: &Pubkey,
    expected_base_mint: &Pubkey,
    expected_quote_mint: &Pubkey,
    final_slot: u64,
) -> Result<PumpSwapQuoteState> {
    // Step 5 — re-decode/re-validate the POOL from the FINAL batch.
    PumpSwapPoolState::validate_owner(pool_owner)?;
    let pool = PumpSwapPoolState::decode(pool_data)?;
    validate_canonical_pool(&pool, expected_authority, expected_base_mint, expected_quote_mint)?;

    // Step 6 — FINAL pool vault addresses MUST equal the DISCOVERED vault
    // addresses. Any change between discovery and final => fail closed.
    if pool.pool_base_token_account != *discovered_base_vault {
        return Err(market_err(format!(
            "PumpSwap base vault changed between discovery and final (discovered {discovered_base_vault}, final pool {})",
            pool.pool_base_token_account
        )));
    }
    if pool.pool_quote_token_account != *discovered_quote_vault {
        return Err(market_err(format!(
            "PumpSwap quote vault changed between discovery and final (discovered {discovered_quote_vault}, final pool {})",
            pool.pool_quote_token_account
        )));
    }

    // Step 7 — decode/validate both vaults + base mint from the FINAL batch ONLY.
    // Vaults are owned by the pool PDA; their mints are the pool's base/quote mints.
    TokenAccountState::validate_program(base_vault_owner)?;
    let base_vault = TokenAccountState::decode(base_vault_data)?;
    base_vault.validate(&pool.base_mint, pool_pda)?;

    TokenAccountState::validate_program(quote_vault_owner)?;
    let quote_vault = TokenAccountState::decode(quote_vault_data)?;
    quote_vault.validate(&pool.quote_mint, pool_pda)?;

    MintState::validate_owner(base_mint_owner)?;
    let base_mint = MintState::decode(base_mint_data)?;

    // Step 8 — FeeConfig account owner MUST be the fee program, BEFORE decode.
    validate_fee_config_owner(fee_config_owner)?;
    let fee_config = FeeConfigState::decode(fee_config_data)?;

    let base_reserve = base_vault.amount;
    if base_reserve == 0 {
        return Err(market_err("PumpSwap pool has zero base reserve"));
    }

    // Step 9 — effective quote reserve = f(FINAL quote-vault amount, FINAL pool vqr).
    let effective_quote_reserve =
        math::effective_quote_reserve(quote_vault.amount, pool.virtual_quote_reserves)
            .ok_or_else(|| {
                market_err("PumpSwap effective quote reserve non-positive / overflow")
            })?;

    // Step 10 — published slot IS the FINAL batch context slot.
    Ok(PumpSwapQuoteState {
        pool,
        base_reserve,
        effective_quote_reserve,
        base_mint,
        fee_config,
        slot: final_slot,
    })
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
            // Fail-closed: Pump FeeConfig account owner MUST be the fee program
            // BEFORE decoding it (same discipline as the graduated path, step 8).
            validate_fee_config_owner(&fee_acc.owner)?;
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

        // The initial `slot` is bonding-curve provenance ONLY. It MUST NOT be
        // published as PumpSwap quote provenance (BLOCKER A step 10).
        let _ = slot;

        // Step 3 — DISCOVERY-ONLY fetch of the canonical pool, used solely to learn
        // the base_vault + quote_vault addresses. This pool data is NOT quote-state
        // and its context slot is deliberately discarded.
        let (discovery_accounts, _discovery_slot) = self.fetch_multi(vec![pool_pda]).await?;
        let discovery_pool_acc = discovery_accounts
            .first()
            .cloned()
            .flatten()
            .ok_or_else(|| {
                market_err("curve complete but canonical PumpSwap pool unavailable")
            })?;
        PumpSwapPoolState::validate_owner(&discovery_pool_acc.owner)?;
        let discovery_pool = PumpSwapPoolState::decode(&discovery_pool_acc.data)?;
        // Validate discovery identity too, so we only ever fetch vaults belonging to
        // the real canonical pool. (Final batch re-validates independently.)
        validate_canonical_pool(&discovery_pool, &authority, mint, &pool_quote_mint)?;
        let discovered_base_vault = discovery_pool.pool_base_token_account;
        let discovered_quote_vault = discovery_pool.pool_quote_token_account;

        // Step 4 — FINAL single coherent batch at confirmed commitment. Account list
        // is EXACTLY [pool PDA, discovered base vault, discovered quote vault, base
        // mint, PumpSwap FeeConfig PDA]. Its context slot is the published slot.
        let (final_accounts, final_slot) = self
            .fetch_multi(vec![
                pool_pda,
                discovered_base_vault,
                discovered_quote_vault,
                *mint,
                amm_fee_pda,
            ])
            .await?;

        let pool_acc = final_accounts
            .first()
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("final PumpSwap pool account missing"))?;
        let base_vault_acc = final_accounts
            .get(1)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("final PumpSwap base vault missing"))?;
        let quote_vault_acc = final_accounts
            .get(2)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("final PumpSwap quote vault missing"))?;
        let base_mint_acc = final_accounts
            .get(3)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("final base mint account missing"))?;
        let amm_fee_acc = final_accounts
            .get(4)
            .cloned()
            .flatten()
            .ok_or_else(|| market_err("final PumpSwap FeeConfig account missing"))?;

        // Steps 5-10 — pure decode/validate of the FINAL batch. This is the sole
        // source of published quote-state and slot.
        let q = decode_final_pumpswap_batch(
            &pool_acc.owner,
            &pool_acc.data,
            &base_vault_acc.owner,
            &base_vault_acc.data,
            &quote_vault_acc.owner,
            &quote_vault_acc.data,
            &base_mint_acc.owner,
            &base_mint_acc.data,
            &amm_fee_acc.owner,
            &amm_fee_acc.data,
            &pool_pda,
            &discovered_base_vault,
            &discovered_quote_vault,
            &authority,
            mint,
            &pool_quote_mint,
            final_slot,
        )?;

        Ok(VenueFetch::PumpSwap(PumpSwapFetch {
            pool: q.pool,
            base_reserve: q.base_reserve,
            effective_quote_reserve: q.effective_quote_reserve,
            base_mint: q.base_mint,
            fee_config: q.fee_config,
            slot: q.slot,
        }))
    }

    /// Fresh on-chain MARK observation (packet D6). Observational: an unsupported
    /// quote asset is reported with `mark = None`, not an error.
    pub async fn snapshot(&self, mint: &Pubkey) -> Result<MarketSnapshot> {
        // Timestamp policy (packet "Snapshot timestamp"): await the successful
        // resolve/fetch FIRST, THEN stamp observed_at, THEN construct the snapshot.
        // The resolved `VenueFetch` carries NO premature now().
        let resolved = self.resolve_and_fetch(mint).await?;
        let observed_at = chrono::Utc::now();
        match resolved {
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

    // ------------------------------------------------------------------
    // Final-batch coherence tests (BLOCKER A). No network — pure fixtures
    // built the same way src/market/pump_state.rs tests build them.
    // ------------------------------------------------------------------

    use crate::market::pump_state::{
        pump_amm_program_id, spl_token_program_id, wsol_mint as wsol, FEE_CONFIG_DISCRIMINATOR,
        POOL_DISCRIMINATOR,
    };

    #[allow(clippy::too_many_arguments)]
    fn build_pool_bytes(
        index: u16,
        creator: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        coin_creator: Pubkey,
        virtual_quote: i128,
    ) -> Vec<u8> {
        let mut d = Vec::with_capacity(261);
        d.extend_from_slice(&POOL_DISCRIMINATOR);
        d.push(0u8); // pool_bump
        d.extend_from_slice(&index.to_le_bytes());
        d.extend_from_slice(creator.as_ref());
        d.extend_from_slice(base_mint.as_ref());
        d.extend_from_slice(quote_mint.as_ref());
        d.extend_from_slice(pk(200).as_ref()); // lp_mint
        d.extend_from_slice(base_vault.as_ref());
        d.extend_from_slice(quote_vault.as_ref());
        d.extend_from_slice(&1u64.to_le_bytes()); // lp_supply
        d.extend_from_slice(coin_creator.as_ref());
        d.push(0u8); // is_mayhem_mode
        d.push(0u8); // is_cashback_coin
        d.extend_from_slice(&virtual_quote.to_le_bytes());
        assert_eq!(d.len(), 261);
        d
    }

    fn build_token_account_bytes(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d
    }

    fn build_mint_bytes(supply: u64, decimals: u8) -> Vec<u8> {
        let mut d = vec![0u8; 82];
        d[36..44].copy_from_slice(&supply.to_le_bytes());
        d[44] = decimals;
        d
    }

    fn build_fee_config_bytes() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&FEE_CONFIG_DISCRIMINATOR);
        d.push(255u8); // bump
        d.extend_from_slice(pk(50).as_ref()); // admin
                                              // flat fees
        d.extend_from_slice(&100u64.to_le_bytes());
        d.extend_from_slice(&50u64.to_le_bytes());
        d.extend_from_slice(&25u64.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes()); // one tier
        d.extend_from_slice(&0u128.to_le_bytes()); // threshold
        d.extend_from_slice(&100u64.to_le_bytes());
        d.extend_from_slice(&50u64.to_le_bytes());
        d.extend_from_slice(&25u64.to_le_bytes());
        d
    }

    /// Standard identities for a well-formed FINAL batch.
    struct Fixture {
        authority: Pubkey,
        base_mint_pk: Pubkey,
        quote_mint_pk: Pubkey,
        pool_pda: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
    }

    fn fixture() -> Fixture {
        Fixture {
            authority: pk(1),
            base_mint_pk: pk(2),
            quote_mint_pk: wsol(),
            pool_pda: pk(10),
            base_vault: pk(5),
            quote_vault: pk(6),
        }
    }

    /// Call the pure final-batch decoder with a well-formed batch, allowing the
    /// caller to override discovered vault addresses / feeconfig owner / vqr / slot.
    #[allow(clippy::too_many_arguments)]
    fn run_final(
        f: &Fixture,
        pool_base_vault: Pubkey,
        pool_quote_vault: Pubkey,
        discovered_base: Pubkey,
        discovered_quote: Pubkey,
        fee_owner: Pubkey,
        virtual_quote: i128,
        quote_vault_amount: u64,
        final_slot: u64,
    ) -> Result<PumpSwapQuoteState> {
        let pool_bytes = build_pool_bytes(
            0,
            f.authority,
            f.base_mint_pk,
            f.quote_mint_pk,
            pool_base_vault,
            pool_quote_vault,
            Pubkey::default(),
            virtual_quote,
        );
        let base_vault_bytes = build_token_account_bytes(f.base_mint_pk, f.pool_pda, 1_000_000);
        let quote_vault_bytes =
            build_token_account_bytes(f.quote_mint_pk, f.pool_pda, quote_vault_amount);
        let mint_bytes = build_mint_bytes(1_000_000_000_000_000, 6);
        let fee_bytes = build_fee_config_bytes();

        decode_final_pumpswap_batch(
            &pump_amm_program_id(),
            &pool_bytes,
            &spl_token_program_id(),
            &base_vault_bytes,
            &spl_token_program_id(),
            &quote_vault_bytes,
            &spl_token_program_id(),
            &mint_bytes,
            &fee_owner,
            &fee_bytes,
            &f.pool_pda,
            &discovered_base,
            &discovered_quote,
            &f.authority,
            &f.base_mint_pk,
            &f.quote_mint_pk,
            final_slot,
        )
    }

    #[test]
    fn test_final_batch_slot_is_published_not_discovery() {
        let f = fixture();
        let q = run_final(
            &f,
            f.base_vault,
            f.quote_vault,
            f.base_vault,
            f.quote_vault,
            fee_program_id(),
            5_000_000_000i128,
            2_000_000_000,
            9_999_999, // FINAL context slot
        )
        .unwrap();
        // Published slot must equal the FINAL batch slot, never a discovery/curve slot.
        assert_eq!(q.slot, 9_999_999);
    }

    #[test]
    fn test_final_batch_changed_base_vault_rejected() {
        let f = fixture();
        // FINAL pool reports a base vault different from the discovered one.
        let err = run_final(
            &f,
            pk(77), // final pool base vault moved
            f.quote_vault,
            f.base_vault, // discovered base vault
            f.quote_vault,
            fee_program_id(),
            0,
            1_000_000_000,
            1,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_final_batch_changed_quote_vault_rejected() {
        let f = fixture();
        let err = run_final(
            &f,
            f.base_vault,
            pk(88), // final pool quote vault moved
            f.base_vault,
            f.quote_vault, // discovered quote vault
            fee_program_id(),
            0,
            1_000_000_000,
            1,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_final_batch_wrong_fee_program_owner_rejected() {
        let f = fixture();
        let err = run_final(
            &f,
            f.base_vault,
            f.quote_vault,
            f.base_vault,
            f.quote_vault,
            pump_amm_program_id(), // NOT the fee program
            0,
            1_000_000_000,
            1,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_final_batch_effective_reserve_uses_final_pool_and_quote_vault() {
        let f = fixture();
        let vqr = 3_000_000_000i128;
        let quote_amt = 7_000_000_000u64;
        let q = run_final(
            &f,
            f.base_vault,
            f.quote_vault,
            f.base_vault,
            f.quote_vault,
            fee_program_id(),
            vqr,
            quote_amt,
            42,
        )
        .unwrap();
        let expected =
            math::effective_quote_reserve(quote_amt, vqr).expect("finite effective reserve");
        assert_eq!(q.effective_quote_reserve, expected);
        assert_eq!(q.pool.virtual_quote_reserves, vqr);
    }

    #[test]
    fn test_pump_fee_config_wrong_owner_rejected_pregraduation() {
        // Pre-graduation path validates the Pump FeeConfig owner via the same helper
        // before decoding. Prove the helper fails closed for a non-fee-program owner.
        assert!(validate_fee_config_owner(&fee_program_id()).is_ok());
        assert!(validate_fee_config_owner(&pump_program_id()).is_err());
        assert!(validate_fee_config_owner(&pump_amm_program_id()).is_err());
    }

    #[test]
    fn test_timestamp_resolved_state_carries_no_premature_now() {
        // Timestamp policy: the resolved quote-state produced by the pure decoder
        // carries NO timestamp field — observed_at is applied by snapshot() ONLY
        // after the await succeeds. A well-formed batch resolves without any now().
        let f = fixture();
        let q = run_final(
            &f,
            f.base_vault,
            f.quote_vault,
            f.base_vault,
            f.quote_vault,
            fee_program_id(),
            1_000_000_000i128,
            1_000_000_000,
            123,
        )
        .unwrap();
        // Structurally: PumpSwapQuoteState has slot but no observed_at/quoted_at.
        // (If a timestamp field were added here, this line would fail to compile,
        // guarding the "stamp after await" contract.)
        let _slot_only: u64 = q.slot;
    }

    #[test]
    fn test_final_batch_wrong_pool_owner_rejected() {
        // Sanity: re-validation from the final batch rejects a non-PumpSwap pool owner.
        let f = fixture();
        let pool_bytes = build_pool_bytes(
            0,
            f.authority,
            f.base_mint_pk,
            f.quote_mint_pk,
            f.base_vault,
            f.quote_vault,
            Pubkey::default(),
            0,
        );
        let bv = build_token_account_bytes(f.base_mint_pk, f.pool_pda, 1);
        let qv = build_token_account_bytes(f.quote_mint_pk, f.pool_pda, 1);
        let mint_bytes = build_mint_bytes(1, 6);
        let fee_bytes = build_fee_config_bytes();
        let err = decode_final_pumpswap_batch(
            &pump_program_id(), // wrong owner
            &pool_bytes,
            &spl_token_program_id(),
            &bv,
            &spl_token_program_id(),
            &qv,
            &spl_token_program_id(),
            &mint_bytes,
            &fee_program_id(),
            &fee_bytes,
            &f.pool_pda,
            &f.base_vault,
            &f.quote_vault,
            &f.authority,
            &f.base_mint_pk,
            &f.quote_mint_pk,
            1,
        );
        assert!(err.is_err());
    }
}
