//! Canonical byte decoders for current Pump / PumpSwap on-chain accounts.
//!
//! MPT-001 Agent B (packet Sections 7–10). This module intentionally does NOT
//! reuse the legacy `src/pump/accounts.rs` decoder. Legacy decode used
//! `BorshDeserialize::try_from_slice` on a prefix layout, which broke once Pump
//! appended new tail fields (`creator`, `is_mayhem_mode`, `is_cashback_coin`,
//! `quote_mint`). Here we parse bytes explicitly at fixed offsets with checked
//! slicing, so appended-field backward compatibility is deterministic.
//!
//! Live-money parsing: every offset is taken directly from packet Section 7.
//! Bool bytes accept ONLY 0/1. Trailing padding is allowed. A field that is
//! present but too short is a hard error — never silently defaulted.

use std::str::FromStr;

use solana_sdk::pubkey::Pubkey;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// B1 — Program / mint constants (packet Section 8 / 6). No config, no secrets.
// ---------------------------------------------------------------------------

/// Pump bonding-curve program.
pub const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// PumpSwap (PumpAmm) program.
pub const PUMP_AMM_PROGRAM_ID_STR: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// Pump fee program (owns FeeConfig PDAs).
pub const FEE_PROGRAM_ID_STR: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
/// Wrapped SOL mint.
pub const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
/// SPL Token program.
pub const SPL_TOKEN_PROGRAM_ID_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// SPL Token-2022 program.
pub const TOKEN_2022_PROGRAM_ID_STR: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Pump program id.
pub fn pump_program_id() -> Pubkey {
    Pubkey::from_str(PUMP_PROGRAM_ID_STR).expect("valid Pump program id constant")
}

/// PumpSwap program id.
pub fn pump_amm_program_id() -> Pubkey {
    Pubkey::from_str(PUMP_AMM_PROGRAM_ID_STR).expect("valid PumpSwap program id constant")
}

/// Fee program id.
pub fn fee_program_id() -> Pubkey {
    Pubkey::from_str(FEE_PROGRAM_ID_STR).expect("valid fee program id constant")
}

/// Wrapped SOL mint.
pub fn wsol_mint() -> Pubkey {
    Pubkey::from_str(WSOL_MINT_STR).expect("valid wSOL mint constant")
}

/// SPL Token program id.
pub fn spl_token_program_id() -> Pubkey {
    Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_STR).expect("valid SPL Token program id constant")
}

/// SPL Token-2022 program id.
pub fn token_2022_program_id() -> Pubkey {
    Pubkey::from_str(TOKEN_2022_PROGRAM_ID_STR).expect("valid Token-2022 program id constant")
}

/// True if `owner` is one of the two accepted SPL token program ids.
pub fn is_token_program(owner: &Pubkey) -> bool {
    *owner == spl_token_program_id() || *owner == token_2022_program_id()
}

// ---------------------------------------------------------------------------
// Anchor discriminators (packet Section 7).
// ---------------------------------------------------------------------------

/// BondingCurve account discriminator.
pub const BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];
/// PumpSwap Pool account discriminator.
pub const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];
/// FeeConfig account discriminator.
pub const FEE_CONFIG_DISCRIMINATOR: [u8; 8] = [0x8f, 0x34, 0x92, 0xbb, 0xdb, 0x7b, 0x4c, 0x9b];

// ---------------------------------------------------------------------------
// Low-level checked byte readers. All little-endian. All bounds-checked.
// ---------------------------------------------------------------------------

fn err(msg: impl Into<String>) -> Error {
    Error::MarketData(msg.into())
}

fn check_discriminator(data: &[u8], expected: &[u8; 8], what: &str) -> Result<()> {
    if data.len() < 8 {
        return Err(err(format!("{what}: account data shorter than discriminator")));
    }
    if &data[..8] != expected {
        return Err(err(format!("{what}: wrong discriminator")));
    }
    Ok(())
}

fn read_u8(data: &[u8], off: usize, what: &str) -> Result<u8> {
    data.get(off)
        .copied()
        .ok_or_else(|| err(format!("{what}: truncated u8 at offset {off}")))
}

fn read_bool(data: &[u8], off: usize, what: &str) -> Result<bool> {
    match read_u8(data, off, what)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(err(format!(
            "{what}: invalid bool byte {other} at offset {off}"
        ))),
    }
}

fn read_u16(data: &[u8], off: usize, what: &str) -> Result<u16> {
    let bytes = data
        .get(off..off + 2)
        .ok_or_else(|| err(format!("{what}: truncated u16 at offset {off}")))?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u32(data: &[u8], off: usize, what: &str) -> Result<u32> {
    let bytes = data
        .get(off..off + 4)
        .ok_or_else(|| err(format!("{what}: truncated u32 at offset {off}")))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u64(data: &[u8], off: usize, what: &str) -> Result<u64> {
    let bytes = data
        .get(off..off + 8)
        .ok_or_else(|| err(format!("{what}: truncated u64 at offset {off}")))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u128(data: &[u8], off: usize, what: &str) -> Result<u128> {
    let bytes = data
        .get(off..off + 16)
        .ok_or_else(|| err(format!("{what}: truncated u128 at offset {off}")))?;
    Ok(u128::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_i128(data: &[u8], off: usize, what: &str) -> Result<i128> {
    let bytes = data
        .get(off..off + 16)
        .ok_or_else(|| err(format!("{what}: truncated i128 at offset {off}")))?;
    Ok(i128::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_pubkey(data: &[u8], off: usize, what: &str) -> Result<Pubkey> {
    let bytes = data
        .get(off..off + 32)
        .ok_or_else(|| err(format!("{what}: truncated pubkey at offset {off}")))?;
    let arr: [u8; 32] = bytes.try_into().unwrap();
    Ok(Pubkey::new_from_array(arr))
}

// ---------------------------------------------------------------------------
// B2/B3 — PumpBondingCurveState (packet Section 7.1)
// ---------------------------------------------------------------------------

/// Decoded Pump bonding-curve account state.
///
/// Offsets after the 8-byte discriminator (packet Section 7.1):
/// - 8   u64    virtual_token_reserves
/// - 16  u64    virtual_quote_reserves
/// - 24  u64    real_token_reserves
/// - 32  u64    real_quote_reserves
/// - 40  u64    token_total_supply
/// - 48  bool   complete            (end of historic mandatory prefix)
/// - 49  Pubkey creator
/// - 81  bool   is_mayhem_mode      (appended; default false)
/// - 82  bool   is_cashback_coin    (appended; default false)
/// - 83  Pubkey quote_mint          (appended; default Pubkey::default())
/// - 115 end
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpBondingCurveState {
    pub virtual_token_reserves: u64,
    pub virtual_quote_reserves: u64,
    pub real_token_reserves: u64,
    pub real_quote_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub quote_mint: Pubkey,
}

/// End of the historically mandatory prefix (through `complete` at offset 48).
const BC_PREFIX_END: usize = 49;
const BC_CREATOR_END: usize = 81;
const BC_MAYHEM_END: usize = 82;
const BC_CASHBACK_END: usize = 83;
const BC_QUOTE_MINT_END: usize = 115;

impl PumpBondingCurveState {
    /// Decode from raw account data, validating the discriminator.
    ///
    /// Requires the mandatory prefix through offset 48 (`complete`). The `creator`
    /// field is part of the current canonical layout and is required whenever the
    /// account extends past the prefix. Missing appended `is_mayhem_mode` /
    /// `is_cashback_coin` default to `false`; missing `quote_mint` defaults to
    /// `Pubkey::default()`. A field that IS present but truncated is a hard error.
    pub fn decode(data: &[u8]) -> Result<Self> {
        check_discriminator(data, &BONDING_CURVE_DISCRIMINATOR, "BondingCurve")?;

        // Mandatory prefix through `complete` (offset 48, one byte => end 49).
        if data.len() < BC_PREFIX_END {
            return Err(err(
                "BondingCurve: account shorter than mandatory prefix (through complete@48)",
            ));
        }

        let virtual_token_reserves = read_u64(data, 8, "BondingCurve.virtual_token_reserves")?;
        let virtual_quote_reserves = read_u64(data, 16, "BondingCurve.virtual_quote_reserves")?;
        let real_token_reserves = read_u64(data, 24, "BondingCurve.real_token_reserves")?;
        let real_quote_reserves = read_u64(data, 32, "BondingCurve.real_quote_reserves")?;
        let token_total_supply = read_u64(data, 40, "BondingCurve.token_total_supply")?;
        let complete = read_bool(data, 48, "BondingCurve.complete")?;

        // `creator` (offset 49..81). If any bytes of it are present, all must be.
        // A curve account that stops exactly at the prefix (len 49) predates the
        // creator field; default it. Any partial creator => error.
        let creator = if data.len() <= BC_PREFIX_END {
            Pubkey::default()
        } else {
            read_pubkey(data, BC_PREFIX_END, "BondingCurve.creator")?
        };

        // Appended is_mayhem_mode @81. Missing => false. Present-but-invalid => error.
        let is_mayhem_mode = if data.len() <= BC_CREATOR_END {
            false
        } else {
            read_bool(data, BC_CREATOR_END, "BondingCurve.is_mayhem_mode")?
        };

        // Appended is_cashback_coin @82. Missing => false.
        let is_cashback_coin = if data.len() <= BC_MAYHEM_END {
            false
        } else {
            read_bool(data, BC_MAYHEM_END, "BondingCurve.is_cashback_coin")?
        };

        // Appended quote_mint @83..115. Missing => default. Partial => error.
        let quote_mint = if data.len() <= BC_CASHBACK_END {
            Pubkey::default()
        } else {
            read_pubkey(data, BC_CASHBACK_END, "BondingCurve.quote_mint")?
        };

        let _ = BC_QUOTE_MINT_END; // documents full layout end (115).

        Ok(Self {
            virtual_token_reserves,
            virtual_quote_reserves,
            real_token_reserves,
            real_quote_reserves,
            token_total_supply,
            complete,
            creator,
            is_mayhem_mode,
            is_cashback_coin,
            quote_mint,
        })
    }

    /// Owner-check helper: the account owner must be the Pump program.
    pub fn validate_owner(owner: &Pubkey) -> Result<()> {
        if *owner == pump_program_id() {
            Ok(())
        } else {
            Err(err(format!(
                "BondingCurve: wrong owner {owner}, expected Pump program {}",
                PUMP_PROGRAM_ID_STR
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// B2/B3 — PumpSwapPoolState (packet Section 7.2)
// ---------------------------------------------------------------------------

/// Decoded PumpSwap (PumpAmm) pool account state.
///
/// Offsets after the 8-byte discriminator (packet Section 7.2):
/// - 8   u8     pool_bump
/// - 9   u16    index
/// - 11  Pubkey creator
/// - 43  Pubkey base_mint
/// - 75  Pubkey quote_mint
/// - 107 Pubkey lp_mint
/// - 139 Pubkey pool_base_token_account
/// - 171 Pubkey pool_quote_token_account
/// - 203 u64    lp_supply
/// - 211 Pubkey coin_creator
/// - 243 bool   is_mayhem_mode
/// - 244 bool   is_cashback_coin     (end of historic complete prefix, 245)
/// - 245 i128   virtual_quote_reserves (appended; default 0; partial => error)
/// - 261 end
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpSwapPoolState {
    pub pool_bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub virtual_quote_reserves: i128,
}

/// End of the historic complete prefix (through `is_cashback_coin` @244).
const POOL_PREFIX_END: usize = 245;
const POOL_FULL_END: usize = 261;

impl PumpSwapPoolState {
    /// Decode from raw account data, validating the discriminator.
    ///
    /// Requires the complete historical prefix through offset 244
    /// (`is_cashback_coin`). If the account lacks the appended `i128`
    /// `virtual_quote_reserves`, it defaults to 0. A partially-present i128
    /// (present but fewer than 16 bytes remaining) is a hard error — the packet
    /// forbids interpreting a partial signed field.
    pub fn decode(data: &[u8]) -> Result<Self> {
        check_discriminator(data, &POOL_DISCRIMINATOR, "Pool")?;

        if data.len() < POOL_PREFIX_END {
            return Err(err(
                "Pool: account shorter than complete prefix (through is_cashback_coin@244)",
            ));
        }

        let pool_bump = read_u8(data, 8, "Pool.pool_bump")?;
        let index = read_u16(data, 9, "Pool.index")?;
        let creator = read_pubkey(data, 11, "Pool.creator")?;
        let base_mint = read_pubkey(data, 43, "Pool.base_mint")?;
        let quote_mint = read_pubkey(data, 75, "Pool.quote_mint")?;
        let lp_mint = read_pubkey(data, 107, "Pool.lp_mint")?;
        let pool_base_token_account = read_pubkey(data, 139, "Pool.pool_base_token_account")?;
        let pool_quote_token_account = read_pubkey(data, 171, "Pool.pool_quote_token_account")?;
        let lp_supply = read_u64(data, 203, "Pool.lp_supply")?;
        let coin_creator = read_pubkey(data, 211, "Pool.coin_creator")?;
        let is_mayhem_mode = read_bool(data, 243, "Pool.is_mayhem_mode")?;
        let is_cashback_coin = read_bool(data, 244, "Pool.is_cashback_coin")?;

        // Appended i128 @245..261.
        // - Exactly at prefix end (len == 245, or trailing < any i128 bytes): field absent => 0.
        // - Field fully present (>= 16 bytes remaining): parse it.
        // - Field partially present (1..16 bytes remaining): REJECT.
        let remaining = data.len().saturating_sub(POOL_PREFIX_END);
        let virtual_quote_reserves = if remaining == 0 {
            0
        } else if remaining >= 16 {
            read_i128(data, POOL_PREFIX_END, "Pool.virtual_quote_reserves")?
        } else {
            return Err(err(format!(
                "Pool.virtual_quote_reserves: partially-present i128 ({remaining} of 16 bytes); refusing to interpret"
            )));
        };

        let _ = POOL_FULL_END; // documents full layout end (261).

        Ok(Self {
            pool_bump,
            index,
            creator,
            base_mint,
            quote_mint,
            lp_mint,
            pool_base_token_account,
            pool_quote_token_account,
            lp_supply,
            coin_creator,
            is_mayhem_mode,
            is_cashback_coin,
            virtual_quote_reserves,
        })
    }

    /// Owner-check helper: the account owner must be the PumpSwap program.
    pub fn validate_owner(owner: &Pubkey) -> Result<()> {
        if *owner == pump_amm_program_id() {
            Ok(())
        } else {
            Err(err(format!(
                "Pool: wrong owner {owner}, expected PumpSwap program {}",
                PUMP_AMM_PROGRAM_ID_STR
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// B2/B3 — MintState (packet Section 7.3)
// ---------------------------------------------------------------------------

/// Decoded fields from the common SPL / Token-2022 Mint base layout.
///
/// - supply u64 @ offset 36
/// - decimals u8 @ offset 44
///
/// The base Mint layout is 82 bytes; we require at least through `decimals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MintState {
    pub supply: u64,
    pub decimals: u8,
}

/// Minimum bytes to cover the base Mint layout through `decimals` (offset 44).
const MINT_MIN_LEN: usize = 45;

impl MintState {
    /// Decode supply/decimals from the base Mint layout. No discriminator: SPL
    /// mints are not Anchor accounts. Do NOT assume 6 decimals.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < MINT_MIN_LEN {
            return Err(err(format!(
                "Mint: account too short for base layout ({} < {MINT_MIN_LEN})",
                data.len()
            )));
        }
        let supply = read_u64(data, 36, "Mint.supply")?;
        let decimals = read_u8(data, 44, "Mint.decimals")?;
        Ok(Self { supply, decimals })
    }

    /// Owner-check helper: owner must be SPL Token or Token-2022.
    pub fn validate_owner(owner: &Pubkey) -> Result<()> {
        if is_token_program(owner) {
            Ok(())
        } else {
            Err(err(format!(
                "Mint: wrong owner {owner}, expected SPL Token or Token-2022"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// B2/B3 — TokenAccountState (packet Section 7.4)
// ---------------------------------------------------------------------------

/// Decoded fields from the common SPL / Token-2022 token-account base layout.
///
/// - mint Pubkey @ 0
/// - owner Pubkey @ 32
/// - amount u64 @ 64
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenAccountState {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

/// Minimum bytes through `amount` (offset 64, u64 => 72).
const TOKEN_ACCOUNT_MIN_LEN: usize = 72;

impl TokenAccountState {
    /// Decode mint/owner/amount from the base token-account layout.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < TOKEN_ACCOUNT_MIN_LEN {
            return Err(err(format!(
                "TokenAccount: account too short for base layout ({} < {TOKEN_ACCOUNT_MIN_LEN})",
                data.len()
            )));
        }
        let mint = read_pubkey(data, 0, "TokenAccount.mint")?;
        let owner = read_pubkey(data, 32, "TokenAccount.owner")?;
        let amount = read_u64(data, 64, "TokenAccount.amount")?;
        Ok(Self {
            mint,
            owner,
            amount,
        })
    }

    /// Owner-program check helper: program must be SPL Token or Token-2022.
    pub fn validate_program(program: &Pubkey) -> Result<()> {
        if is_token_program(program) {
            Ok(())
        } else {
            Err(err(format!(
                "TokenAccount: wrong program {program}, expected SPL Token or Token-2022"
            )))
        }
    }

    /// Validate the decoded account matches an expected pool vault identity:
    /// exact mint and exact owner (the Pool PDA).
    pub fn validate(&self, expected_mint: &Pubkey, expected_owner: &Pubkey) -> Result<()> {
        if self.mint != *expected_mint {
            return Err(err(format!(
                "TokenAccount: mint mismatch (got {}, expected {expected_mint})",
                self.mint
            )));
        }
        if self.owner != *expected_owner {
            return Err(err(format!(
                "TokenAccount: owner mismatch (got {}, expected {expected_owner})",
                self.owner
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// B4 — FeeConfigState (packet Section 9)
// ---------------------------------------------------------------------------

/// Flat fee components (bps) decoded from a FeeConfig / FeeTier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFees {
    pub lp_fee_bps: u64,
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
}

/// A single dynamic fee tier keyed by a market-cap (lamports) threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedFeeTier {
    pub threshold: u128,
    pub fees: DecodedFees,
}

/// Decoded FeeConfig account (packet Section 9). Borsh-compatible manual decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeConfigState {
    pub bump: u8,
    pub admin: Pubkey,
    pub flat_fees: DecodedFees,
    pub fee_tiers: Vec<DecodedFeeTier>,
}

/// Max single fee component in bps (100%). Any component above this is corrupt.
const MAX_FEE_BPS: u64 = 10_000;

fn read_fees(data: &[u8], off: usize, what: &str) -> Result<DecodedFees> {
    let lp_fee_bps = read_u64(data, off, what)?;
    let protocol_fee_bps = read_u64(data, off + 8, what)?;
    let creator_fee_bps = read_u64(data, off + 16, what)?;
    for (name, v) in [
        ("lp_fee_bps", lp_fee_bps),
        ("protocol_fee_bps", protocol_fee_bps),
        ("creator_fee_bps", creator_fee_bps),
    ] {
        if v > MAX_FEE_BPS {
            return Err(err(format!(
                "{what}: {name}={v} exceeds {MAX_FEE_BPS} bps"
            )));
        }
    }
    Ok(DecodedFees {
        lp_fee_bps,
        protocol_fee_bps,
        creator_fee_bps,
    })
}

impl FeeConfigState {
    /// Decode a FeeConfig account, validating the discriminator, tier bounds, and
    /// monotonic-nondecreasing thresholds. Layout after the 8-byte discriminator:
    ///
    /// ```text
    /// u8  bump
    /// Pubkey admin
    /// Fees flat_fees { u64 lp, u64 protocol, u64 creator }
    /// u32 fee_tiers_length
    /// repeated FeeTier { u128 threshold, Fees fees }
    /// ```
    ///
    /// Rejects: zero tiers, a vector length exceeding remaining bytes, a truncated
    /// entry, any fee component > 10_000 bps, and non-monotonic thresholds. Chain
    /// data is validated, never silently sorted.
    pub fn decode(data: &[u8]) -> Result<Self> {
        check_discriminator(data, &FEE_CONFIG_DISCRIMINATOR, "FeeConfig")?;

        let mut off = 8usize;
        let bump = read_u8(data, off, "FeeConfig.bump")?;
        off += 1;
        let admin = read_pubkey(data, off, "FeeConfig.admin")?;
        off += 32;
        let flat_fees = read_fees(data, off, "FeeConfig.flat_fees")?;
        off += 24; // three u64s
        let fee_tiers_length = read_u32(data, off, "FeeConfig.fee_tiers_length")? as usize;
        off += 4;

        if fee_tiers_length == 0 {
            return Err(err(
                "FeeConfig: zero fee tiers is invalid for a canonical tiered quote",
            ));
        }

        // Each FeeTier is u128 (16) + 3*u64 (24) = 40 bytes. Guard the declared
        // length against remaining bytes BEFORE looping (reject absurd lengths).
        const TIER_SIZE: usize = 16 + 24;
        let remaining = data.len().saturating_sub(off);
        let needed = fee_tiers_length
            .checked_mul(TIER_SIZE)
            .ok_or_else(|| err("FeeConfig: fee_tiers_length overflows byte requirement"))?;
        if needed > remaining {
            return Err(err(format!(
                "FeeConfig: declared {fee_tiers_length} tiers need {needed} bytes but only {remaining} remain"
            )));
        }

        let mut fee_tiers = Vec::with_capacity(fee_tiers_length);
        let mut prev_threshold: Option<u128> = None;
        for i in 0..fee_tiers_length {
            let threshold = read_u128(data, off, "FeeConfig.tier.threshold")?;
            off += 16;
            let fees = read_fees(data, off, "FeeConfig.tier.fees")?;
            off += 24;

            if let Some(prev) = prev_threshold {
                if threshold < prev {
                    return Err(err(format!(
                        "FeeConfig: non-monotonic tier thresholds at index {i} ({threshold} < {prev})"
                    )));
                }
            }
            prev_threshold = Some(threshold);

            fee_tiers.push(DecodedFeeTier { threshold, fees });
        }

        Ok(Self {
            bump,
            admin,
            flat_fees,
            fee_tiers,
        })
    }
}

// ---------------------------------------------------------------------------
// B5 — PDA helpers (packet Section 8)
// ---------------------------------------------------------------------------

/// Bonding-curve PDA: seeds ["bonding-curve", base_mint] under the Pump program.
pub fn bonding_curve_pda(base_mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"bonding-curve", base_mint.as_ref()],
        &pump_program_id(),
    )
}

/// Pump canonical pool creator authority: seeds ["pool-authority", base_mint]
/// under the Pump program.
pub fn pump_pool_authority_pda(base_mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"pool-authority", base_mint.as_ref()],
        &pump_program_id(),
    )
}

/// Canonical PumpSwap pool PDA: seeds ["pool", 0u16 le, pump_pool_authority,
/// base_mint, quote_mint] under the PumpSwap program.
pub fn canonical_pool_pda(
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    pump_pool_authority: &Pubkey,
) -> (Pubkey, u8) {
    let index_le = 0u16.to_le_bytes();
    Pubkey::find_program_address(
        &[
            b"pool",
            &index_le,
            pump_pool_authority.as_ref(),
            base_mint.as_ref(),
            quote_mint.as_ref(),
        ],
        &pump_amm_program_id(),
    )
}

/// Pump FeeConfig PDA: seeds ["fee_config", PUMP_PROGRAM_ID] under the fee program.
pub fn pump_fee_config_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"fee_config", pump_program_id().as_ref()],
        &fee_program_id(),
    )
}

/// PumpSwap FeeConfig PDA: seeds ["fee_config", PUMP_AMM_PROGRAM_ID] under the fee program.
pub fn pumpswap_fee_config_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"fee_config", pump_amm_program_id().as_ref()],
        &fee_program_id(),
    )
}

// ---------------------------------------------------------------------------
// B6 — Tests. Synthetic byte arrays only; no RPC/network.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    // --- BondingCurve builders ----------------------------------------------

    /// Build a full current bonding-curve account (len 115).
    fn build_bonding_curve(
        vtok: u64,
        vquote: u64,
        rtok: u64,
        rquote: u64,
        supply: u64,
        complete: bool,
        creator: Pubkey,
        mayhem: bool,
        cashback: bool,
        quote_mint: Pubkey,
    ) -> Vec<u8> {
        let mut d = Vec::with_capacity(115);
        d.extend_from_slice(&BONDING_CURVE_DISCRIMINATOR);
        d.extend_from_slice(&vtok.to_le_bytes());
        d.extend_from_slice(&vquote.to_le_bytes());
        d.extend_from_slice(&rtok.to_le_bytes());
        d.extend_from_slice(&rquote.to_le_bytes());
        d.extend_from_slice(&supply.to_le_bytes());
        d.push(complete as u8);
        d.extend_from_slice(creator.as_ref());
        d.push(mayhem as u8);
        d.push(cashback as u8);
        d.extend_from_slice(quote_mint.as_ref());
        assert_eq!(d.len(), 115);
        d
    }

    #[test]
    fn test_decode_current_bonding_curve_with_quote_mint() {
        let creator = pk(7);
        let qmint = pk(9);
        let data = build_bonding_curve(
            1_000_000_000_000,
            30_000_000_000,
            800_000_000_000,
            10_000_000_000,
            1_000_000_000_000_000,
            false,
            creator,
            true,
            false,
            qmint,
        );
        let s = PumpBondingCurveState::decode(&data).unwrap();
        assert_eq!(s.virtual_token_reserves, 1_000_000_000_000);
        assert_eq!(s.virtual_quote_reserves, 30_000_000_000);
        assert_eq!(s.real_token_reserves, 800_000_000_000);
        assert_eq!(s.real_quote_reserves, 10_000_000_000);
        assert_eq!(s.token_total_supply, 1_000_000_000_000_000);
        assert!(!s.complete);
        assert_eq!(s.creator, creator);
        assert!(s.is_mayhem_mode);
        assert!(!s.is_cashback_coin);
        assert_eq!(s.quote_mint, qmint);
    }

    #[test]
    fn test_decode_legacy_bonding_curve_defaults_appended_sol_fields() {
        // Legacy account truncated right after `complete` (len 49): no creator,
        // no appended tail. creator/quote_mint default; flags false.
        let mut short = build_bonding_curve(
            5, 6, 7, 8, 9, true, pk(3), true, true, pk(4),
        );
        short.truncate(49);
        let s = PumpBondingCurveState::decode(&short).unwrap();
        assert!(s.complete);
        assert_eq!(s.creator, Pubkey::default());
        assert!(!s.is_mayhem_mode);
        assert!(!s.is_cashback_coin);
        assert_eq!(s.quote_mint, Pubkey::default());

        // Truncated at offset 81 (creator present, no appended flags/quote_mint).
        let mut mid = build_bonding_curve(
            5, 6, 7, 8, 9, false, pk(3), true, true, pk(4),
        );
        mid.truncate(81);
        let s2 = PumpBondingCurveState::decode(&mid).unwrap();
        assert_eq!(s2.creator, pk(3));
        assert!(!s2.is_mayhem_mode);
        assert!(!s2.is_cashback_coin);
        assert_eq!(s2.quote_mint, Pubkey::default());
    }

    #[test]
    fn test_bonding_curve_wrong_discriminator_rejected() {
        let mut data = build_bonding_curve(
            1, 2, 3, 4, 5, false, pk(1), false, false, pk(2),
        );
        data[0] ^= 0xFF;
        assert!(PumpBondingCurveState::decode(&data).is_err());
    }

    #[test]
    fn test_bonding_curve_invalid_bool_rejected() {
        let mut data = build_bonding_curve(
            1, 2, 3, 4, 5, false, pk(1), false, false, pk(2),
        );
        data[48] = 2; // complete byte invalid
        assert!(PumpBondingCurveState::decode(&data).is_err());
    }

    #[test]
    fn test_bonding_curve_trailing_padding_allowed() {
        let mut data = build_bonding_curve(
            1, 2, 3, 4, 5, false, pk(1), false, false, pk(2),
        );
        data.extend_from_slice(&[0u8; 16]);
        assert!(PumpBondingCurveState::decode(&data).is_ok());
    }

    // --- Pool builders ------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn build_pool(
        bump: u8,
        index: u16,
        creator: Pubkey,
        base_mint: Pubkey,
        quote_mint: Pubkey,
        lp_mint: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        lp_supply: u64,
        coin_creator: Pubkey,
        mayhem: bool,
        cashback: bool,
        virtual_quote: Option<i128>,
    ) -> Vec<u8> {
        let mut d = Vec::with_capacity(261);
        d.extend_from_slice(&POOL_DISCRIMINATOR);
        d.push(bump);
        d.extend_from_slice(&index.to_le_bytes());
        d.extend_from_slice(creator.as_ref());
        d.extend_from_slice(base_mint.as_ref());
        d.extend_from_slice(quote_mint.as_ref());
        d.extend_from_slice(lp_mint.as_ref());
        d.extend_from_slice(base_vault.as_ref());
        d.extend_from_slice(quote_vault.as_ref());
        d.extend_from_slice(&lp_supply.to_le_bytes());
        d.extend_from_slice(coin_creator.as_ref());
        d.push(mayhem as u8);
        d.push(cashback as u8);
        assert_eq!(d.len(), 245);
        if let Some(v) = virtual_quote {
            d.extend_from_slice(&v.to_le_bytes());
            assert_eq!(d.len(), 261);
        }
        d
    }

    #[test]
    fn test_decode_current_pool_virtual_quote_i128() {
        let data = build_pool(
            254,
            0,
            pk(1),
            pk(2),
            pk(3),
            pk(4),
            pk(5),
            pk(6),
            123_456,
            pk(7),
            false,
            true,
            Some(-5_000_000_000i128),
        );
        let p = PumpSwapPoolState::decode(&data).unwrap();
        assert_eq!(p.pool_bump, 254);
        assert_eq!(p.index, 0);
        assert_eq!(p.creator, pk(1));
        assert_eq!(p.base_mint, pk(2));
        assert_eq!(p.quote_mint, pk(3));
        assert_eq!(p.lp_mint, pk(4));
        assert_eq!(p.pool_base_token_account, pk(5));
        assert_eq!(p.pool_quote_token_account, pk(6));
        assert_eq!(p.lp_supply, 123_456);
        assert_eq!(p.coin_creator, pk(7));
        assert!(!p.is_mayhem_mode);
        assert!(p.is_cashback_coin);
        assert_eq!(p.virtual_quote_reserves, -5_000_000_000i128);
    }

    #[test]
    fn test_decode_legacy_pool_defaults_virtual_quote_zero() {
        let data = build_pool(
            1, 0, pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 1, pk(7), false, false, None,
        );
        assert_eq!(data.len(), 245);
        let p = PumpSwapPoolState::decode(&data).unwrap();
        assert_eq!(p.virtual_quote_reserves, 0);
    }

    #[test]
    fn test_partial_virtual_quote_field_rejected() {
        let mut data = build_pool(
            1, 0, pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 1, pk(7), false, false, None,
        );
        // Append only 8 of the 16 i128 bytes => partial field must be rejected.
        data.extend_from_slice(&[0u8; 8]);
        assert_eq!(data.len(), 253);
        assert!(PumpSwapPoolState::decode(&data).is_err());
    }

    #[test]
    fn test_pool_full_i128_after_padding_note() {
        // A field fully present plus extra trailing padding is fine (>=16 remain).
        let mut data = build_pool(
            1, 0, pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 1, pk(7), false, false, Some(42),
        );
        data.extend_from_slice(&[0u8; 8]); // trailing padding after full i128
        let p = PumpSwapPoolState::decode(&data).unwrap();
        assert_eq!(p.virtual_quote_reserves, 42);
    }

    #[test]
    fn test_pool_wrong_owner_validation_helper() {
        assert!(PumpSwapPoolState::validate_owner(&pump_amm_program_id()).is_ok());
        assert!(PumpSwapPoolState::validate_owner(&pump_program_id()).is_err());
        assert!(PumpBondingCurveState::validate_owner(&pump_program_id()).is_ok());
        assert!(PumpBondingCurveState::validate_owner(&pump_amm_program_id()).is_err());
    }

    // --- Mint / TokenAccount ------------------------------------------------

    fn build_mint(supply: u64, decimals: u8, len: usize) -> Vec<u8> {
        let mut d = vec![0u8; len];
        d[36..44].copy_from_slice(&supply.to_le_bytes());
        d[44] = decimals;
        d
    }

    #[test]
    fn test_decode_spl_mint_supply_decimals() {
        // Full 82-byte SPL mint layout.
        let data = build_mint(1_000_000_000_000_000, 9, 82);
        let m = MintState::decode(&data).unwrap();
        assert_eq!(m.supply, 1_000_000_000_000_000);
        assert_eq!(m.decimals, 9);
        assert!(MintState::validate_owner(&spl_token_program_id()).is_ok());
    }

    #[test]
    fn test_decode_token2022_mint_base_layout() {
        // Token-2022 mints share the base layout; extra extension bytes trail.
        let data = build_mint(500_000, 6, 165);
        let m = MintState::decode(&data).unwrap();
        assert_eq!(m.supply, 500_000);
        assert_eq!(m.decimals, 6);
        assert!(MintState::validate_owner(&token_2022_program_id()).is_ok());
        assert!(MintState::validate_owner(&pump_program_id()).is_err());
    }

    #[test]
    fn test_mint_too_short_rejected() {
        assert!(MintState::decode(&[0u8; 44]).is_err());
    }

    fn build_token_account(mint: Pubkey, owner: Pubkey, amount: u64, len: usize) -> Vec<u8> {
        let mut d = vec![0u8; len.max(72)];
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d
    }

    #[test]
    fn test_decode_token_account_exact_mint_owner() {
        let mint = pk(11);
        let owner = pk(12);
        let data = build_token_account(mint, owner, 777, 165);
        let t = TokenAccountState::decode(&data).unwrap();
        assert_eq!(t.mint, mint);
        assert_eq!(t.owner, owner);
        assert_eq!(t.amount, 777);
        assert!(t.validate(&mint, &owner).is_ok());
        assert!(t.validate(&pk(99), &owner).is_err());
        assert!(t.validate(&mint, &pk(99)).is_err());
        assert!(TokenAccountState::validate_program(&spl_token_program_id()).is_ok());
        assert!(TokenAccountState::validate_program(&pump_program_id()).is_err());
    }

    // --- FeeConfig ----------------------------------------------------------

    fn push_fees(d: &mut Vec<u8>, lp: u64, protocol: u64, creator: u64) {
        d.extend_from_slice(&lp.to_le_bytes());
        d.extend_from_slice(&protocol.to_le_bytes());
        d.extend_from_slice(&creator.to_le_bytes());
    }

    fn build_fee_config(
        bump: u8,
        admin: Pubkey,
        flat: (u64, u64, u64),
        tiers: &[(u128, (u64, u64, u64))],
        declared_len: Option<u32>,
    ) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&FEE_CONFIG_DISCRIMINATOR);
        d.push(bump);
        d.extend_from_slice(admin.as_ref());
        push_fees(&mut d, flat.0, flat.1, flat.2);
        let len = declared_len.unwrap_or(tiers.len() as u32);
        d.extend_from_slice(&len.to_le_bytes());
        for (threshold, fees) in tiers {
            d.extend_from_slice(&threshold.to_le_bytes());
            push_fees(&mut d, fees.0, fees.1, fees.2);
        }
        d
    }

    #[test]
    fn test_decode_fee_config_tiers() {
        let data = build_fee_config(
            255,
            pk(1),
            (100, 50, 25),
            &[
                (0u128, (100, 50, 25)),
                (1_000_000_000u128, (80, 40, 20)),
                (10_000_000_000u128, (60, 30, 10)),
            ],
            None,
        );
        let fc = FeeConfigState::decode(&data).unwrap();
        assert_eq!(fc.bump, 255);
        assert_eq!(fc.admin, pk(1));
        assert_eq!(fc.flat_fees.lp_fee_bps, 100);
        assert_eq!(fc.flat_fees.protocol_fee_bps, 50);
        assert_eq!(fc.flat_fees.creator_fee_bps, 25);
        assert_eq!(fc.fee_tiers.len(), 3);
        assert_eq!(fc.fee_tiers[0].threshold, 0);
        assert_eq!(fc.fee_tiers[2].threshold, 10_000_000_000);
        assert_eq!(fc.fee_tiers[2].fees.protocol_fee_bps, 30);
    }

    #[test]
    fn test_fee_config_zero_tiers_rejected() {
        let data = build_fee_config(1, pk(1), (10, 10, 10), &[], None);
        assert!(FeeConfigState::decode(&data).is_err());
    }

    #[test]
    fn test_fee_config_truncated_tier_rejected() {
        // Declare 3 tiers but only supply 2 => needed > remaining.
        let data = build_fee_config(
            1,
            pk(1),
            (10, 10, 10),
            &[(0u128, (1, 1, 1)), (5u128, (1, 1, 1))],
            Some(3),
        );
        assert!(FeeConfigState::decode(&data).is_err());
    }

    #[test]
    fn test_fee_config_absurd_length_rejected() {
        let data = build_fee_config(
            1,
            pk(1),
            (10, 10, 10),
            &[(0u128, (1, 1, 1))],
            Some(1_000_000),
        );
        assert!(FeeConfigState::decode(&data).is_err());
    }

    #[test]
    fn test_fee_config_nonmonotonic_rejected() {
        let data = build_fee_config(
            1,
            pk(1),
            (10, 10, 10),
            &[
                (0u128, (1, 1, 1)),
                (10_000u128, (1, 1, 1)),
                (5_000u128, (1, 1, 1)), // decreases => reject
            ],
            None,
        );
        assert!(FeeConfigState::decode(&data).is_err());
    }

    #[test]
    fn test_fee_config_component_over_10000_rejected() {
        let data = build_fee_config(
            1,
            pk(1),
            (10_001, 10, 10),
            &[(0u128, (1, 1, 1))],
            None,
        );
        assert!(FeeConfigState::decode(&data).is_err());
    }

    #[test]
    fn test_fee_config_equal_thresholds_allowed_monotonic() {
        // Nondecreasing permits equal adjacent thresholds.
        let data = build_fee_config(
            1,
            pk(1),
            (10, 10, 10),
            &[(100u128, (1, 1, 1)), (100u128, (2, 2, 2))],
            None,
        );
        assert!(FeeConfigState::decode(&data).is_ok());
    }

    // --- PDA fixtures -------------------------------------------------------

    #[test]
    fn test_bonding_curve_pda_fixture() {
        let base = pk(42);
        let (pda, bump) = bonding_curve_pda(&base);
        // Determinism + independent recomputation with explicit program id.
        let (expected, ebump) = Pubkey::find_program_address(
            &[b"bonding-curve", base.as_ref()],
            &pump_program_id(),
        );
        assert_eq!(pda, expected);
        assert_eq!(bump, ebump);
        assert_ne!(pda, Pubkey::default());
    }

    #[test]
    fn test_canonical_pool_pda_is_index_zero_and_pump_authority_creator() {
        let base = pk(2);
        let quote = wsol_mint();
        let (authority, _) = pump_pool_authority_pda(&base);
        let (pool, _) = canonical_pool_pda(&base, &quote, &authority);

        // Recompute with index 0 (u16 LE) explicitly and confirm equality.
        let index_le = 0u16.to_le_bytes();
        let (expected, _) = Pubkey::find_program_address(
            &[
                b"pool",
                &index_le,
                authority.as_ref(),
                base.as_ref(),
                quote.as_ref(),
            ],
            &pump_amm_program_id(),
        );
        assert_eq!(pool, expected);

        // A pool derived with a different (nonzero) index must differ, proving the
        // canonical derivation is pinned to index 0.
        let one_le = 1u16.to_le_bytes();
        let (other, _) = Pubkey::find_program_address(
            &[
                b"pool",
                &one_le,
                authority.as_ref(),
                base.as_ref(),
                quote.as_ref(),
            ],
            &pump_amm_program_id(),
        );
        assert_ne!(pool, other);
    }

    #[test]
    fn test_pump_and_amm_fee_config_pdas_are_distinct() {
        let (pump_fc, _) = pump_fee_config_pda();
        let (amm_fc, _) = pumpswap_fee_config_pda();
        assert_ne!(pump_fc, amm_fc);
        assert_ne!(pump_fc, Pubkey::default());
        assert_ne!(amm_fc, Pubkey::default());
    }
}
