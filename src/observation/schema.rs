//! Observation schema (P1-001, packet sections 7-22).
//!
//! Every recorded JSONL line is one [`ObservationEnvelope`]. All record types
//! here are recorder-owned, serializable, and independent of the live market
//! domain types — the live types are mapped IN via `From` conversions so the
//! research boundary never depends on internal reserve/venue fabrication.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::market::types::{ExecutableQuote, MarketSide, MarketSnapshot, MarketVenue, QuoteAsset};

// ---------------------------------------------------------------------------
// Section 7 — schema version + fixed v1 constants
// ---------------------------------------------------------------------------

/// Schema version for the observation dataset. Bumped only on incompatible
/// changes; no silent incompatible changes are permitted.
pub const OBSERVATION_SCHEMA_VERSION: u32 = 2;

/// Fixed hypothetical entry size = 0.001 SOL. Not configurable in v1.
pub const ENTRY_QUOTE_LAMPORTS: u64 = 1_000_000;

/// Fixed v1 outcome (sell-quote) horizon schedule, in seconds. Not configurable.
pub const OUTCOME_HORIZONS_SECS: &[u64] =
    &[2, 4, 6, 8, 10, 12, 15, 18, 21, 24, 27, 30, 45, 60, 90, 120];

/// Horizons at which a canonical `snapshot()` is additionally requested.
pub const SNAPSHOT_HORIZONS_SECS: &[u64] = &[15, 30, 60, 120];

// ---------------------------------------------------------------------------
// Section 11 — text safety helper
// ---------------------------------------------------------------------------

/// Sanitize untrusted provider text before persistence.
///
/// Strips ASCII/Unicode control characters, then truncates to at most `max`
/// characters (char boundaries, not bytes). Does NOT normalize unicode beyond
/// removing control characters. Pure function.
pub fn sanitize_persist_text(input: &str, max: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

// ---------------------------------------------------------------------------
// Section 8 — envelope + payload
// ---------------------------------------------------------------------------

/// One physical JSONL line. Serialized compact (single line, no pretty print).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationEnvelope {
    pub schema_version: u32,
    pub run_id: String,
    pub seq: u64,
    pub recorded_at: DateTime<Utc>,
    pub payload: ObservationPayload,
}

/// Stable tagged payload. `kind` selects the variant; `data` carries the record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ObservationPayload {
    RunStarted(RunStartedRecord),
    StreamState(StreamStateRecord),
    CandidateObserved(CandidateObservedRecord),
    InitialMarket(InitialMarketRecord),
    OutcomeSample(OutcomeSampleRecord),
    MigrationObserved(MigrationObservedRecord),
    TrackingSkipped(TrackingSkippedRecord),
    TrackingFinished(TrackingFinishedRecord),
    RunFinished(RunFinishedRecord),
}

impl ObservationPayload {
    /// Extract the candidate id for payloads that carry one, for grouping.
    pub fn candidate_id(&self) -> Option<&str> {
        match self {
            ObservationPayload::CandidateObserved(r) => Some(&r.candidate_id),
            ObservationPayload::InitialMarket(r) => Some(&r.candidate_id),
            ObservationPayload::OutcomeSample(r) => Some(&r.candidate_id),
            ObservationPayload::TrackingSkipped(r) => Some(&r.candidate_id),
            ObservationPayload::TrackingFinished(r) => Some(&r.candidate_id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Section 9 — RunStarted
// ---------------------------------------------------------------------------

/// Run provenance metadata. NEVER carries RPC/WS URL, API key, credentials dir,
/// keypair path, wallet address, or environment dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStartedRecord {
    pub source_revision: String,
    pub working_tree_clean: Option<bool>,
    pub binary_version: String,
    /// Exactly "solana-mainnet".
    pub network: String,
    pub entry_quote_lamports: u64,
    pub outcome_horizons_secs: Vec<u64>,
    pub snapshot_horizons_secs: Vec<u64>,
    /// Exactly "protocol_net_ex_network_v1".
    pub return_model: String,
    pub intake_seconds: u64,
    pub max_active_candidates: usize,

    /// Schema-v2 research universe descriptors. Absent (=> None) in v1 records.
    #[serde(default)]
    pub discovery_universe: Option<String>,
    #[serde(default)]
    pub outcome_universe: Option<String>,

    /// P1-OBSERVATION-RPC-CONCURRENCY-001 §13: the configured observation RPC
    /// concurrency gate limit for this run. Observation-process metadata only.
    /// Absent (=> None) in v1 and pre-gate v2 records; a gated recorder run writes
    /// `Some(limit)`. Never carries RPC URL / provider / API key.
    #[serde(default)]
    pub observation_rpc_concurrency_limit: Option<usize>,
}

impl RunStartedRecord {
    /// Construct a RunStarted record with the fixed v1 schedule/constants and
    /// the mandated `return_model`/`network` string literals baked in.
    ///
    /// `observation_rpc_concurrency_limit` is the observation RPC gate bound for
    /// this run (`None` for generic/legacy construction; the gated recorder passes
    /// `Some(limit)`).
    pub fn new(
        source_revision: String,
        working_tree_clean: Option<bool>,
        binary_version: String,
        intake_seconds: u64,
        max_active_candidates: usize,
        observation_rpc_concurrency_limit: Option<usize>,
    ) -> Self {
        Self {
            source_revision,
            working_tree_clean,
            binary_version,
            network: "solana-mainnet".to_string(),
            entry_quote_lamports: ENTRY_QUOTE_LAMPORTS,
            outcome_horizons_secs: OUTCOME_HORIZONS_SECS.to_vec(),
            snapshot_horizons_secs: SNAPSHOT_HORIZONS_SECS.to_vec(),
            return_model: "protocol_net_ex_network_v1".to_string(),
            intake_seconds,
            max_active_candidates,
            discovery_universe: Some("pumpportal_create_identity_v2".to_string()),
            outcome_universe: Some("canonical_sol_quote_exact_0_001_sol_v1".to_string()),
            observation_rpc_concurrency_limit,
        }
    }
}

// ---------------------------------------------------------------------------
// Section 10 — StreamState
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamStateKind {
    Connected,
    Disconnected,
    ProviderError,
    UnexpectedTrade,
}

/// Stream control state. `category` is a sanitized fixed provider category only
/// (never raw provider JSON). For `UnexpectedTrade`, `category` is `None` and no
/// trade fields are recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStateRecord {
    pub state: StreamStateKind,
    pub category: Option<String>,
}

// ---------------------------------------------------------------------------
// Section 11 — CandidateObserved
// ---------------------------------------------------------------------------

/// Explicit provenance shape of a retained provider create event.
///
/// `Full` = the legacy strict NewToken shape (all provider observational fields
/// present). `Partial` = a create with valid required identity but one or more
/// absent provider observational fields (schema-v2 retention). Historical v1
/// records carry no shape and deserialize to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCreateShape {
    Full,
    Partial,
}

/// One record for EVERY provider NewToken event received (including duplicates).
///
/// Provider numeric fields are observational only: never treated as canonical
/// reserves, never cast to integer, never divided by 1e9.
///
/// Schema v2: the provider observational fields are `Option`. Absence maps to
/// `None` (never a numeric or empty-string sentinel). Historical v1 lines carry
/// concrete numbers/strings and deserialize into `Some(value)` automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateObservedRecord {
    /// candidate_id == provider NewToken signature (no second identity).
    pub candidate_id: String,
    pub signature: String,
    pub mint: String,
    /// creator == provider `trader_public_key`.
    pub creator: String,
    #[serde(default)]
    pub bonding_curve: Option<String>,
    pub tx_type: String,

    #[serde(default)]
    pub provider_initial_buy: Option<f64>,
    #[serde(default)]
    pub provider_v_tokens_in_bonding_curve: Option<f64>,
    #[serde(default)]
    pub provider_v_sol_in_bonding_curve_sol: Option<f64>,
    #[serde(default)]
    pub provider_market_cap_sol: Option<f64>,

    pub name: String,
    pub symbol: String,
    pub uri: String,

    /// true for signatures previously seen in this run.
    pub duplicate: bool,

    /// Schema-v2 explicit provider shape. `None` for historical v1 records.
    #[serde(default)]
    pub provider_create_shape: Option<ProviderCreateShape>,
}

// ---------------------------------------------------------------------------
// Section 12 — failure codes + classifier
// ---------------------------------------------------------------------------

/// Fixed observation failure categories. NEVER carries raw RPC / provider error
/// strings (lower layers may embed configured endpoint text).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationFailureCode {
    RpcUnavailable,
    MarketUnavailable,
    UnsupportedQuoteAsset,
    DecodeOrValidation,
    InvalidProviderIdentity,
    TrackingCapacity,
    TrackingTaskFailure,
    DrainTimeout,
    /// Added from the beginning per packet section 41: candidate drained after
    /// the intake deadline closed, so no tracking task is launched.
    IntakeClosed,
    Other,
}

/// Classify a crate error into a fixed failure code, DISCARDING all inner
/// strings (which may carry configured endpoint text). The error's
/// `to_string()` is never serialized into recorder data.
pub fn classify_observation_error(err: &crate::Error) -> ObservationFailureCode {
    use crate::Error;
    match err {
        Error::Rpc(_) | Error::RpcTimeout(_) | Error::RpcConnection(_) => {
            ObservationFailureCode::RpcUnavailable
        }
        Error::UnsupportedQuoteMint(_) => ObservationFailureCode::UnsupportedQuoteAsset,
        Error::MarketData(_) => ObservationFailureCode::MarketUnavailable,
        Error::BondingCurveDecode(_)
        | Error::InvalidInstruction(_)
        | Error::Serialization(_)
        | Error::Deserialization(_) => ObservationFailureCode::DecodeOrValidation,
        _ => ObservationFailureCode::Other,
    }
}

// ---------------------------------------------------------------------------
// Section 13 — observed market snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedVenue {
    PumpBondingCurve,
    PumpSwapCanonical,
}

impl From<MarketVenue> for ObservedVenue {
    fn from(v: MarketVenue) -> Self {
        match v {
            MarketVenue::PumpBondingCurve => ObservedVenue::PumpBondingCurve,
            MarketVenue::PumpSwapCanonical => ObservedVenue::PumpSwapCanonical,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedQuoteAsset {
    Sol,
    Unsupported { mint: String },
}

impl From<QuoteAsset> for ObservedQuoteAsset {
    fn from(q: QuoteAsset) -> Self {
        match q {
            QuoteAsset::Sol => ObservedQuoteAsset::Sol,
            QuoteAsset::Unsupported(pubkey) => ObservedQuoteAsset::Unsupported {
                mint: pubkey.to_string(),
            },
        }
    }
}

/// Recorder-owned serializable snapshot. No raw reserve fabrication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshotRecord {
    pub venue: ObservedVenue,
    pub quote_asset: ObservedQuoteAsset,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub mark_price_sol_per_token: Option<f64>,
    pub slot: u64,
    pub observed_at: DateTime<Utc>,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
}

impl From<&MarketSnapshot> for MarketSnapshotRecord {
    fn from(s: &MarketSnapshot) -> Self {
        Self {
            venue: s.venue.into(),
            quote_asset: s.quote_asset.into(),
            base_decimals: s.base_decimals,
            quote_decimals: s.quote_decimals,
            mark_price_sol_per_token: s.mark_price_sol_per_token,
            slot: s.slot,
            observed_at: s.observed_at,
            is_mayhem_mode: s.is_mayhem_mode,
            is_cashback_coin: s.is_cashback_coin,
        }
    }
}

// ---------------------------------------------------------------------------
// Section 14 — observed executable quote
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedSide {
    Buy,
    Sell,
}

impl From<MarketSide> for ObservedSide {
    fn from(s: MarketSide) -> Self {
        match s {
            MarketSide::Buy => ObservedSide::Buy,
            MarketSide::Sell => ObservedSide::Sell,
        }
    }
}

/// Recorder-owned serializable executable quote. No slippage tolerance, priority
/// fee, Jito tip, or modeled fill is added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutableQuoteRecord {
    pub side: ObservedSide,
    pub venue: ObservedVenue,
    pub quote_asset: ObservedQuoteAsset,
    pub base_decimals: u8,
    pub quote_decimals: u8,

    pub base_amount_raw: u64,
    pub base_amount_ui: f64,
    pub quote_amount_raw: u64,

    pub expected_price_sol_per_token: Option<f64>,

    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
    pub lp_fee_bps: u64,

    pub slot: u64,
    pub quoted_at: DateTime<Utc>,
}

impl From<&ExecutableQuote> for ExecutableQuoteRecord {
    fn from(q: &ExecutableQuote) -> Self {
        Self {
            side: q.side.into(),
            venue: q.venue.into(),
            quote_asset: q.quote_asset.into(),
            base_decimals: q.base_decimals,
            quote_decimals: q.quote_decimals,
            base_amount_raw: q.base_amount_raw,
            base_amount_ui: q.base_amount_ui(),
            quote_amount_raw: q.quote_amount_raw,
            expected_price_sol_per_token: q.expected_price_sol_per_token,
            protocol_fee_bps: q.protocol_fee_bps,
            creator_fee_bps: q.creator_fee_bps,
            lp_fee_bps: q.lp_fee_bps,
            slot: q.slot,
            quoted_at: q.quoted_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Section 15 — InitialMarket
// ---------------------------------------------------------------------------

/// For each first-seen candidate tracking task.
///
/// Invariants (unless a prior fatal identity failure prevents both, in which
/// case a `TrackingSkipped` is recorded instead of this record):
/// - `snapshot.is_some()` XOR `snapshot_failure.is_some()`
/// - `buy_quote.is_some()` XOR `buy_quote_failure.is_some()`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialMarketRecord {
    pub candidate_id: String,
    pub mint: String,
    pub candidate_received_at: DateTime<Utc>,

    pub snapshot: Option<MarketSnapshotRecord>,
    pub snapshot_failure: Option<ObservationFailureCode>,

    pub buy_quote: Option<ExecutableQuoteRecord>,
    pub buy_quote_failure: Option<ObservationFailureCode>,

    pub initial_observation_attempts: u8,

    /// P1-OBSERVATION-RPC-CONCURRENCY-001 §15: totals (across ALL initial retry
    /// attempts) of observation RPC gate wait and oracle call duration, split by
    /// snapshot vs buy-quote. Observation-process metadata only. Absent (=> None)
    /// in v1 and pre-gate v2 records; a gated recorder run writes `Some(total)`.
    #[serde(default)]
    pub initial_snapshot_rpc_gate_wait_ms_total: Option<u64>,
    #[serde(default)]
    pub initial_snapshot_rpc_call_duration_ms_total: Option<u64>,
    #[serde(default)]
    pub initial_buy_rpc_gate_wait_ms_total: Option<u64>,
    #[serde(default)]
    pub initial_buy_rpc_call_duration_ms_total: Option<u64>,
}

// ---------------------------------------------------------------------------
// Section 17 + 18 — OutcomeSample + return math
// ---------------------------------------------------------------------------

/// Outcome sample at a fixed horizon.
///
/// `snapshot`/`snapshot_failure` are populated only at snapshot horizons
/// (15/30/60/120); at other horizons both are `None` (absence is not failure).
/// `protocol_net_ex_network_return_bps` is present only when the sell quote
/// succeeded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeSampleRecord {
    pub candidate_id: String,
    pub mint: String,

    pub horizon_secs: u64,
    pub due_at: DateTime<Utc>,
    pub sampled_at: DateTime<Utc>,
    pub sample_lag_ms: i64,

    pub sell_quote: Option<ExecutableQuoteRecord>,
    pub sell_quote_failure: Option<ObservationFailureCode>,

    pub snapshot: Option<MarketSnapshotRecord>,
    pub snapshot_failure: Option<ObservationFailureCode>,

    pub protocol_net_ex_network_return_bps: Option<i64>,

    /// P1-OBSERVATION-RPC-CONCURRENCY-001 §14: observation RPC gate wait and oracle
    /// call duration for THIS sample's sell quote. A gated run writes `Some(...)`
    /// regardless of sell success/failure. Measured directly from monotonic time,
    /// NEVER derived from `sample_lag_ms`. Absent (=> None) in v1/pre-gate records.
    #[serde(default)]
    pub sell_rpc_gate_wait_ms: Option<u64>,
    #[serde(default)]
    pub sell_rpc_call_duration_ms: Option<u64>,

    /// Same, for the key-horizon snapshot. `Some(...)` only at snapshot horizons
    /// (15/30/60/120) in a gated run, regardless of snapshot success/failure;
    /// `None` at non-snapshot horizons and in v1/pre-gate records.
    #[serde(default)]
    pub snapshot_rpc_gate_wait_ms: Option<u64>,
    #[serde(default)]
    pub snapshot_rpc_call_duration_ms: Option<u64>,
}

/// Protocol-net-ex-network return in basis points (10_000 bps = 100%).
///
/// Computed purely with unsigned/signed 128-bit integer arithmetic — no `f64`.
///
/// ```text
/// ratio_bps  = future_sell * 10_000 / initial_buy   (u128, integer division)
/// return_bps = ratio_bps - 10_000                   (i128 -> i64)
/// ```
///
/// ROUNDING: the intermediate `future_sell * 10_000 / initial_buy` uses integer
/// (floor) division, so the ratio is rounded toward zero (i.e. toward negative
/// return magnitude for positive inputs). Returns `None` when `initial` is not
/// strictly positive.
///
/// Examples: 1_000_000 -> 1_100_000 = +1000; -> 920_000 = -800; -> 1_200_000 = +2000.
pub fn protocol_net_ex_network_return_bps(
    initial_buy_quote_lamports: u64,
    future_sell_quote_lamports: u64,
) -> Option<i64> {
    if initial_buy_quote_lamports == 0 {
        return None;
    }
    let initial = initial_buy_quote_lamports as u128;
    let future = future_sell_quote_lamports as u128;
    let ratio_bps = future.checked_mul(10_000)? / initial;
    let return_bps = ratio_bps as i128 - 10_000_i128;
    i64::try_from(return_bps).ok()
}

// ---------------------------------------------------------------------------
// Section 19 — TrackingFinished
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackingFinishStatus {
    Complete,
    InitialQuoteUnavailable,
    CapacitySkipped,
    TaskFailed,
    DrainTimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingFinishedRecord {
    pub candidate_id: String,
    pub mint: String,
    pub status: TrackingFinishStatus,
    pub successful_outcome_samples: u16,
    pub failed_outcome_samples: u16,
}

// ---------------------------------------------------------------------------
// Section 20 — TrackingSkipped
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingSkippedRecord {
    pub candidate_id: String,
    pub mint: String,
    pub reason: ObservationFailureCode,
}

// ---------------------------------------------------------------------------
// Section 21 — MigrationObserved
// ---------------------------------------------------------------------------

/// Provider migration fields are observational only. They are NOT turned into
/// canonical venue truth, and no quote is generated because of a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationObservedRecord {
    pub mint: String,
    pub signature: Option<String>,
    pub pool: Option<String>,
    pub pool_id: Option<String>,
    pub provider_received_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Section 22 — RunFinished
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunCompletion {
    Complete,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFinishedRecord {
    pub completion: RunCompletion,

    pub candidates_seen: u64,
    pub unique_candidates: u64,
    pub duplicate_candidate_events: u64,

    pub tracking_started: u64,
    pub tracking_skipped: u64,
    pub tracking_completed: u64,

    pub stream_connected_events: u64,
    pub stream_disconnect_events: u64,
    pub provider_errors: u64,
    pub unexpected_trade_events: u64,

    pub migrations_seen: u64,

    /// Schema-v2 informational counter: number of retained PartialNewToken
    /// create events. Does NOT overlap provider_errors/decode counters. v1
    /// replay defaults to 0.
    #[serde(default)]
    pub partial_new_token_events: u64,

    /// P1-OBSERVATION-RPC-CONCURRENCY-001 §16: aggregate observation RPC gate
    /// statistics for the run. Observation-process metadata only. A gated recorder
    /// run writes all four as `Some(...)`; v1 and pre-gate v2 replay => `None`.
    /// Invariant when present: `rpc_gate_peak_in_flight <=
    /// RunStarted.observation_rpc_concurrency_limit`.
    #[serde(default)]
    pub rpc_gate_peak_in_flight: Option<usize>,
    #[serde(default)]
    pub rpc_gate_acquisitions: Option<u64>,
    #[serde(default)]
    pub rpc_gate_wait_ms_total: Option<u64>,
    #[serde(default)]
    pub rpc_gate_wait_ms_max: Option<u64>,
}

// ---------------------------------------------------------------------------
// Section 27 — Agent A tests (schema / return math / classifier)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_return_bps_plus_10_percent() {
        assert_eq!(
            protocol_net_ex_network_return_bps(1_000_000, 1_100_000),
            Some(1000)
        );
    }

    #[test]
    fn test_return_bps_minus_8_percent() {
        assert_eq!(
            protocol_net_ex_network_return_bps(1_000_000, 920_000),
            Some(-800)
        );
    }

    #[test]
    fn test_return_bps_plus_20_percent() {
        assert_eq!(
            protocol_net_ex_network_return_bps(1_000_000, 1_200_000),
            Some(2000)
        );
    }

    #[test]
    fn test_return_bps_zero_initial_none() {
        assert_eq!(protocol_net_ex_network_return_bps(0, 1_000_000), None);
    }

    #[test]
    fn test_horizon_schedule_contains_exact_15_30_60_120() {
        for h in [15u64, 30, 60, 120] {
            assert!(OUTCOME_HORIZONS_SECS.contains(&h), "missing {h}");
            assert!(SNAPSHOT_HORIZONS_SECS.contains(&h), "missing snapshot {h}");
        }
        assert_eq!(SNAPSHOT_HORIZONS_SECS, &[15, 30, 60, 120]);
    }

    #[test]
    fn test_horizon_schedule_strictly_increasing() {
        for w in OUTCOME_HORIZONS_SECS.windows(2) {
            assert!(w[0] < w[1], "not strictly increasing at {:?}", w);
        }
    }

    #[test]
    fn test_error_classifier_drops_inner_rpc_string() {
        let err = crate::Error::Rpc("https://secret-endpoint.example/key123".into());
        let code = classify_observation_error(&err);
        assert_eq!(code, ObservationFailureCode::RpcUnavailable);
        let json = serde_json::to_string(&code).unwrap();
        assert!(!json.contains("secret"), "leaked inner string: {json}");
        assert_eq!(json, "\"rpc_unavailable\"");
    }

    #[test]
    fn test_serialized_failure_has_no_raw_error_message() {
        // Every failure code serializes to a fixed snake_case token, never a
        // free-form message.
        let cases = [
            (
                crate::Error::MarketData("endpoint https://secret".into()),
                ObservationFailureCode::MarketUnavailable,
            ),
            (
                crate::Error::UnsupportedQuoteMint("mint-secret".into()),
                ObservationFailureCode::UnsupportedQuoteAsset,
            ),
            (
                crate::Error::BondingCurveDecode("secret-bytes".into()),
                ObservationFailureCode::DecodeOrValidation,
            ),
            (
                crate::Error::Internal("secret-internal".into()),
                ObservationFailureCode::Other,
            ),
        ];
        for (err, expected) in cases {
            let code = classify_observation_error(&err);
            assert_eq!(code, expected);
            let json = serde_json::to_string(&code).unwrap();
            assert!(!json.contains("secret"), "leaked inner string: {json}");
            assert!(!json.contains(' '), "message-shaped serialization: {json}");
        }
    }

    #[test]
    fn test_sanitize_persist_text_caps_and_removes_controls() {
        let dirty = "ab\u{0007}cd\nef";
        assert_eq!(sanitize_persist_text(dirty, 256), "abcdef");
        let long: String = "x".repeat(1000);
        assert_eq!(sanitize_persist_text(&long, 64).chars().count(), 64);
    }

    // -- Schema v2 (packet section 22) --------------------------------------

    fn full_candidate() -> CandidateObservedRecord {
        CandidateObservedRecord {
            candidate_id: "sig".into(),
            signature: "sig".into(),
            mint: "mint".into(),
            creator: "creator".into(),
            bonding_curve: Some("bc".into()),
            tx_type: "create".into(),
            provider_initial_buy: Some(1.5),
            provider_v_tokens_in_bonding_curve: Some(2.5),
            provider_v_sol_in_bonding_curve_sol: Some(3.5),
            provider_market_cap_sol: Some(4.5),
            name: "n".into(),
            symbol: "s".into(),
            uri: "u".into(),
            duplicate: false,
            provider_create_shape: Some(ProviderCreateShape::Full),
        }
    }

    #[test]
    fn test_schema_version_is_2() {
        assert_eq!(OBSERVATION_SCHEMA_VERSION, 2);
    }

    #[test]
    fn test_full_candidate_serializes_some_provider_values() {
        let json = serde_json::to_string(&full_candidate()).unwrap();
        assert!(json.contains("\"provider_initial_buy\":1.5"), "{json}");
        assert!(
            json.contains("\"provider_v_tokens_in_bonding_curve\":2.5"),
            "{json}"
        );
        assert!(
            json.contains("\"provider_v_sol_in_bonding_curve_sol\":3.5"),
            "{json}"
        );
        assert!(json.contains("\"provider_market_cap_sol\":4.5"), "{json}");
        assert!(json.contains("\"bonding_curve\":\"bc\""), "{json}");
    }

    #[test]
    fn test_partial_candidate_none_provider_values_round_trip() {
        let mut rec = full_candidate();
        rec.bonding_curve = None;
        rec.provider_initial_buy = None;
        rec.provider_v_tokens_in_bonding_curve = None;
        rec.provider_v_sol_in_bonding_curve_sol = None;
        rec.provider_market_cap_sol = None;
        rec.provider_create_shape = Some(ProviderCreateShape::Partial);

        let json = serde_json::to_string(&rec).unwrap();
        let back: CandidateObservedRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bonding_curve, None);
        assert_eq!(back.provider_initial_buy, None);
        assert_eq!(back.provider_v_tokens_in_bonding_curve, None);
        assert_eq!(back.provider_v_sol_in_bonding_curve_sol, None);
        assert_eq!(back.provider_market_cap_sol, None);
    }

    #[test]
    fn test_provider_create_shape_round_trip() {
        assert_eq!(
            serde_json::to_string(&ProviderCreateShape::Full).unwrap(),
            "\"full\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderCreateShape::Partial).unwrap(),
            "\"partial\""
        );
        let full: ProviderCreateShape = serde_json::from_str("\"full\"").unwrap();
        let partial: ProviderCreateShape = serde_json::from_str("\"partial\"").unwrap();
        assert_eq!(full, ProviderCreateShape::Full);
        assert_eq!(partial, ProviderCreateShape::Partial);
    }

    #[test]
    fn test_run_started_writes_exact_universe_strings() {
        let rec = RunStartedRecord::new("rev".into(), None, "bin".into(), 900, 64, None);
        assert_eq!(
            rec.discovery_universe.as_deref(),
            Some("pumpportal_create_identity_v2")
        );
        assert_eq!(
            rec.outcome_universe.as_deref(),
            Some("canonical_sol_quote_exact_0_001_sol_v1")
        );
        // Return model is unchanged.
        assert_eq!(rec.return_model, "protocol_net_ex_network_v1");
    }

    #[test]
    fn test_run_finished_partial_counter_round_trip() {
        let rec = RunFinishedRecord {
            completion: RunCompletion::Complete,
            candidates_seen: 3,
            unique_candidates: 3,
            duplicate_candidate_events: 0,
            tracking_started: 3,
            tracking_skipped: 0,
            tracking_completed: 3,
            stream_connected_events: 1,
            stream_disconnect_events: 0,
            provider_errors: 0,
            unexpected_trade_events: 0,
            migrations_seen: 0,
            partial_new_token_events: 7,
            rpc_gate_peak_in_flight: None,
            rpc_gate_acquisitions: None,
            rpc_gate_wait_ms_total: None,
            rpc_gate_wait_ms_max: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"partial_new_token_events\":7"), "{json}");
        let back: RunFinishedRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.partial_new_token_events, 7);
    }

    // -- RPC concurrency gate telemetry (packet §34) ------------------------

    #[test]
    fn test_schema_version_still_2_with_gate_fields() {
        // Gate telemetry is backward-compatible v2 metadata; NOT a v3 bump.
        assert_eq!(OBSERVATION_SCHEMA_VERSION, 2);
    }

    #[test]
    fn test_run_started_rpc_concurrency_limit_round_trip() {
        let rec = RunStartedRecord::new("rev".into(), None, "bin".into(), 600, 256, Some(24));
        assert_eq!(rec.observation_rpc_concurrency_limit, Some(24));
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains("\"observation_rpc_concurrency_limit\":24"),
            "{json}"
        );
        let back: RunStartedRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.observation_rpc_concurrency_limit, Some(24));
    }

    #[test]
    fn test_run_started_missing_rpc_limit_deserializes_none() {
        // A pre-gate v2 (or v1) RunStarted line has no gate-limit key => None.
        let line = r#"{"source_revision":"rev","working_tree_clean":null,"binary_version":"bin","network":"solana-mainnet","entry_quote_lamports":1000000,"outcome_horizons_secs":[15],"snapshot_horizons_secs":[15],"return_model":"protocol_net_ex_network_v1","intake_seconds":600,"max_active_candidates":128}"#;
        let back: RunStartedRecord = serde_json::from_str(line).unwrap();
        assert_eq!(back.observation_rpc_concurrency_limit, None);
        assert_eq!(back.discovery_universe, None);
    }

    fn sample_with_gate(horizon: u64, snapshot_gate: Option<(u64, u64)>) -> OutcomeSampleRecord {
        OutcomeSampleRecord {
            candidate_id: "sig".into(),
            mint: "mint".into(),
            horizon_secs: horizon,
            due_at: Utc::now(),
            sampled_at: Utc::now(),
            sample_lag_ms: 42,
            sell_quote: None,
            sell_quote_failure: Some(ObservationFailureCode::RpcUnavailable),
            snapshot: None,
            snapshot_failure: None,
            protocol_net_ex_network_return_bps: None,
            sell_rpc_gate_wait_ms: Some(11),
            sell_rpc_call_duration_ms: Some(22),
            snapshot_rpc_gate_wait_ms: snapshot_gate.map(|(w, _)| w),
            snapshot_rpc_call_duration_ms: snapshot_gate.map(|(_, d)| d),
        }
    }

    #[test]
    fn test_outcome_sample_sell_gate_timing_round_trip() {
        let rec = sample_with_gate(15, Some((5, 7)));
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"sell_rpc_gate_wait_ms\":11"), "{json}");
        assert!(json.contains("\"sell_rpc_call_duration_ms\":22"), "{json}");
        let back: OutcomeSampleRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sell_rpc_gate_wait_ms, Some(11));
        assert_eq!(back.sell_rpc_call_duration_ms, Some(22));
        assert_eq!(back.snapshot_rpc_gate_wait_ms, Some(5));
        assert_eq!(back.snapshot_rpc_call_duration_ms, Some(7));
    }

    #[test]
    fn test_outcome_sample_non_key_horizon_snapshot_timing_none() {
        // Sell timing present; snapshot timing None at a non-key horizon.
        let rec = sample_with_gate(20, None);
        let back: OutcomeSampleRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back.sell_rpc_gate_wait_ms, Some(11));
        assert_eq!(back.snapshot_rpc_gate_wait_ms, None);
        assert_eq!(back.snapshot_rpc_call_duration_ms, None);
    }

    #[test]
    fn test_outcome_sample_missing_gate_timing_deserializes_none() {
        // A historical/pre-gate OutcomeSample line has none of the new keys => None.
        let line = r#"{"candidate_id":"sig","mint":"mint","horizon_secs":15,"due_at":"2026-08-30T00:00:00Z","sampled_at":"2026-08-30T00:00:01Z","sample_lag_ms":1000,"sell_quote":null,"sell_quote_failure":"rpc_unavailable","snapshot":null,"snapshot_failure":null,"protocol_net_ex_network_return_bps":null}"#;
        let back: OutcomeSampleRecord = serde_json::from_str(line).unwrap();
        assert_eq!(back.sell_rpc_gate_wait_ms, None);
        assert_eq!(back.sell_rpc_call_duration_ms, None);
        assert_eq!(back.snapshot_rpc_gate_wait_ms, None);
        assert_eq!(back.snapshot_rpc_call_duration_ms, None);
    }

    #[test]
    fn test_initial_market_gate_totals_round_trip() {
        let rec = InitialMarketRecord {
            candidate_id: "sig".into(),
            mint: "mint".into(),
            candidate_received_at: Utc::now(),
            snapshot: None,
            snapshot_failure: Some(ObservationFailureCode::RpcUnavailable),
            buy_quote: None,
            buy_quote_failure: Some(ObservationFailureCode::RpcUnavailable),
            initial_observation_attempts: 4,
            initial_snapshot_rpc_gate_wait_ms_total: Some(10),
            initial_snapshot_rpc_call_duration_ms_total: Some(20),
            initial_buy_rpc_gate_wait_ms_total: Some(30),
            initial_buy_rpc_call_duration_ms_total: Some(40),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: InitialMarketRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.initial_snapshot_rpc_gate_wait_ms_total, Some(10));
        assert_eq!(back.initial_snapshot_rpc_call_duration_ms_total, Some(20));
        assert_eq!(back.initial_buy_rpc_gate_wait_ms_total, Some(30));
        assert_eq!(back.initial_buy_rpc_call_duration_ms_total, Some(40));
        // Attempt count is unchanged in meaning.
        assert_eq!(back.initial_observation_attempts, 4);
    }

    #[test]
    fn test_initial_market_missing_gate_totals_deserializes_none() {
        let line = r#"{"candidate_id":"sig","mint":"mint","candidate_received_at":"2026-08-30T00:00:00Z","snapshot":null,"snapshot_failure":"rpc_unavailable","buy_quote":null,"buy_quote_failure":"rpc_unavailable","initial_observation_attempts":4}"#;
        let back: InitialMarketRecord = serde_json::from_str(line).unwrap();
        assert_eq!(back.initial_snapshot_rpc_gate_wait_ms_total, None);
        assert_eq!(back.initial_buy_rpc_call_duration_ms_total, None);
    }

    #[test]
    fn test_run_finished_gate_stats_round_trip() {
        let rec = RunFinishedRecord {
            completion: RunCompletion::Complete,
            candidates_seen: 3,
            unique_candidates: 3,
            duplicate_candidate_events: 0,
            tracking_started: 3,
            tracking_skipped: 0,
            tracking_completed: 3,
            stream_connected_events: 1,
            stream_disconnect_events: 0,
            provider_errors: 0,
            unexpected_trade_events: 0,
            migrations_seen: 0,
            partial_new_token_events: 0,
            rpc_gate_peak_in_flight: Some(24),
            rpc_gate_acquisitions: Some(500),
            rpc_gate_wait_ms_total: Some(1234),
            rpc_gate_wait_ms_max: Some(99),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: RunFinishedRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rpc_gate_peak_in_flight, Some(24));
        assert_eq!(back.rpc_gate_acquisitions, Some(500));
        assert_eq!(back.rpc_gate_wait_ms_total, Some(1234));
        assert_eq!(back.rpc_gate_wait_ms_max, Some(99));
    }

    #[test]
    fn test_run_finished_missing_gate_stats_deserializes_none() {
        let line = r#"{"completion":"complete","candidates_seen":3,"unique_candidates":3,"duplicate_candidate_events":0,"tracking_started":3,"tracking_skipped":0,"tracking_completed":3,"stream_connected_events":1,"stream_disconnect_events":0,"provider_errors":0,"unexpected_trade_events":0,"migrations_seen":0}"#;
        let back: RunFinishedRecord = serde_json::from_str(line).unwrap();
        assert_eq!(back.partial_new_token_events, 0);
        assert_eq!(back.rpc_gate_peak_in_flight, None);
        assert_eq!(back.rpc_gate_acquisitions, None);
        assert_eq!(back.rpc_gate_wait_ms_total, None);
        assert_eq!(back.rpc_gate_wait_ms_max, None);
    }
}
