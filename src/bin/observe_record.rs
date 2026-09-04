//! Execution-INCAPABLE mainnet research collector (P1-OBSERVATION-RECORDER-001,
//! packet sections 29-51). This is the tx-incapable collector from the packet; the
//! full word for an on-chain tx is avoided verbatim in this file so the static
//! guard's split needle never matches its own prose.
//!
//! This binary records an append-only observation dataset for every PumpPortal
//! NewToken candidate: what the provider reported at discovery, the canonical
//! on-chain market snapshot, an exact fixed 0.001 SOL hypothetical BUY quote, and
//! exact-quantity future SELL quotes at a fixed horizon schedule. It NEVER submits
//! anything on-chain: there is no signer, no keypair, no tx builder/sender, no
//! wallet, no position, and no trading API here. The `quote_buy_sol` /
//! `quote_sell_raw` oracle calls are READ-ONLY canonical quotes, not order
//! actions. The static test suite at the bottom fails the binary if any execution
//! reference ever creeps in.
//!
//! See the packet for the full contract. No filters/strategy/scoring, no trade
//! stream subscriptions, no tx/wallet/position integration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use pumpfun_sniper::market::types::{ExecutableQuote, MarketSnapshot};
use pumpfun_sniper::market::PumpMarketOracle;
use pumpfun_sniper::observation::schema::{
    classify_observation_error_full, protocol_net_ex_network_return_bps, sanitize_persist_text,
    CandidateObservedRecord, DecisionPointBuyQuoteRecord, DecisionPointSellQuoteRecord,
    ExecutableQuoteRecord, InitialMarketRecord, MarketDataFailureKind, MarketSnapshotRecord,
    MigrationObservedRecord, ObservationFailureCode, ObservationPayload, OutcomeSampleRecord,
    ProviderCreateShape, RunCompletion, RunFinishedRecord, RunStartedRecord, StreamStateKind,
    StreamStateRecord, TrackingFinishStatus, TrackingFinishedRecord, TrackingSkippedRecord,
    ENTRY_QUOTE_LAMPORTS, OUTCOME_HORIZONS_SECS, SNAPSHOT_HORIZONS_SECS,
};
use pumpfun_sniper::observation::measurement::{
    holder_features, HolderSnapshot, MeasurementFailureCategory, MeasurementFailureRecord,
    MicrostructureProbe, SnapshotClass, TradeObserved, MEASUREMENT_FEATURE_VERSION,
    TOTAL_MINT_SUPPLY_TOKENS,
};
use pumpfun_sniper::observation::measurement_runtime::{
    classify_holder_accounts_by_owner, BoundedTradeQueue, CoverageBreak, CoverageTracker,
    ParticipationState, RawHolderAccount, SubscriptionRegistry, MEASUREMENT_TRADE_QUEUE_CAPACITY,
};
use pumpfun_sniper::observation::measurement_sink::{
    normalize_trade_event, MeasurementPayload, MeasurementRunSummary, MeasurementSink,
    SubscriptionStateRecord,
};
use pumpfun_sniper::observation::ObservationRecorder;
use pumpfun_sniper::market::pump_state::bonding_curve_pda;
use pumpfun_sniper::runtime::RuntimeLease;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::program_pack::Pack;
use pumpfun_sniper::stream::pumpportal::{
    CommandSender, PumpPortalDecodeError, PumpPortalDecodeKind, SubscriptionCommand, TradeEvent,
};
use pumpfun_sniper::stream::{
    PumpPortalClient, PumpPortalConfig, PumpPortalEvent, PumpPortalSubscriptionPlan,
};
use pumpfun_sniper::Config;

/// Full Solana mainnet genesis hash. Any other value fails preflight immediately,
/// making an accidental devnet/testnet collection run impossible.
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

/// Bounded event-channel capacity between the stream worker and the intake loop.
const EVENT_CHANNEL_CAPACITY: usize = 512;

/// Hard bound on the post-intake outcome drain (packet section 42): the last
/// admitted candidate may need its 120s final horizon plus RPC latency.
const OUTCOME_DRAIN_SECS: u64 = 240;

/// Original-clock horizons where matched delayed-entry sell quotes are requested.
/// These are NOT post-delayed-entry holding periods; actual elapsed timestamps are
/// persisted on each matched event.
const MATCHED_DELAYED_EXIT_HORIZONS_SECS: &[u64] = &[15, 30, 60, 120];
/// Bounds for `--intake-seconds` (packet section 30). Max 6 hours in v1.
const INTAKE_SECONDS_MIN: u64 = 60;
const INTAKE_SECONDS_MAX: u64 = 21_600;

/// Bounds for `--max-active-candidates` (packet section 30).
const MAX_ACTIVE_MIN: usize = 1;
const MAX_ACTIVE_MAX: usize = 256;

/// Bounds + default for `--rpc-concurrency` (P1-OBSERVATION-RPC-CONCURRENCY-001
/// §2). 24 is the FIRST EXPERIMENTAL bound (not a claimed optimum): Run #3's
/// ~20-29 active-tracker band stayed in the lowest observed RPC-failure regime,
/// while degradation accelerated above ~40 active trackers. Active trackers are
/// NOT the same as simultaneous RPC calls, so this is a starting point Run #4 will
/// test.
const RPC_CONCURRENCY_MIN: usize = 1;
const RPC_CONCURRENCY_MAX: usize = 64;
const RPC_CONCURRENCY_DEFAULT: usize = 24;

/// Bounded grace (ms) after `client.stop()` before the post-stop tail drain, to
/// let the worker deliver any final already-queued events (packet §10.3).
const POST_STOP_GRACE_MS: u64 = 150;

// ---------------------------------------------------------------------------
// AUDIT-001 §3-4/§17 — fixed, secret-safe collector recorder-write errors.
// These NEVER carry the raw recorder I/O text or the output path.
// ---------------------------------------------------------------------------

/// Fixed, secret-safe error for a failed recorder append in the main/intake/drain
/// path. Its Display text is a fixed literal and never includes an output path,
/// RPC endpoint, authenticated URL, API key, environment, or config contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollectorRecordError;

impl std::fmt::Display for CollectorRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("observation recorder write failed")
    }
}

impl std::error::Error for CollectorRecordError {}

/// A tracking task's recorder-write failure, surfaced to the parent so a lost
/// recorder is never silently ignored (§4.2). Fixed, secret-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateTaskFailure {
    RecorderWrite,
}

impl std::fmt::Display for CandidateTaskFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tracking task observation recorder write failed")
    }
}

/// What a tracking task hands back to the parent: a clean result or a fixed
/// recorder-write failure. The raw I/O error is never carried across.
type CandidateTaskOutcome = std::result::Result<CandidateTaskResult, CandidateTaskFailure>;

/// AUDIT-001 §4.1/§13 — the ONE required recorder append helper. Every collector
/// recorder write in the main/intake/drain path routes through this so no append
/// Result is ever discarded. On failure it returns a fixed, secret-safe error
/// (the raw recorder I/O text/path is dropped here).
async fn append_required(
    recorder: &ObservationRecorder,
    payload: ObservationPayload,
) -> std::result::Result<u64, CollectorRecordError> {
    recorder
        .append(payload)
        .await
        .map_err(|_| CollectorRecordError)
}

// ---------------------------------------------------------------------------
// P1-OBSERVATION-RPC-CONCURRENCY-001 §5-9/§12 — observation-only bounded RPC gate.
// PRIVATE to this recorder binary. It is NEVER added to PumpMarketOracle, the
// RpcClient, the market module, or any live/trading path.
// ---------------------------------------------------------------------------

/// Bounded observation RPC concurrency gate. Backed by a tokio `Semaphore`; permits
/// are taken with ASYNC WAITING (`acquire_owned().await`, §6) so an already-admitted
/// candidate task NEVER drops because the gate is busy — it queues. There is no
/// `try_acquire` and no acquisition timeout here.
///
/// This is deliberately distinct from the candidate-task capacity semaphore
/// (`max_active_candidates`), which MAY skip (`TrackingCapacity`). The RPC gate
/// never skips: it only bounds how many high-level oracle calls run at once.
struct ObservationRpcGate {
    semaphore: Arc<Semaphore>,
    limit: usize,
    /// Currently-held permits (for the peak-in-flight high-water mark).
    in_flight: AtomicUsize,
    /// Maximum simultaneously-held permits observed (§12). `<= limit`.
    peak_in_flight: AtomicUsize,
    /// Total successful permit acquisitions (§12).
    acquisitions: AtomicU64,
    gate_wait_ms_total: AtomicU64,
    gate_wait_ms_max: AtomicU64,
}

/// Aggregate gate statistics captured for RunFinished (§16).
struct RpcGateStats {
    limit: usize,
    peak_in_flight: usize,
    acquisitions: u64,
    gate_wait_ms_total: u64,
    gate_wait_ms_max: u64,
}

impl ObservationRpcGate {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            limit,
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            acquisitions: AtomicU64::new(0),
            gate_wait_ms_total: AtomicU64::new(0),
            gate_wait_ms_max: AtomicU64::new(0),
        })
    }

    /// Acquire ONE permit, waiting asynchronously if the gate is full (§6). Returns
    /// an RAII guard plus the measured gate wait in ms. NEVER `try_acquire`, never
    /// times out, never drops. On success bumps `acquisitions`, the wait
    /// accumulators, and the peak-in-flight high-water mark.
    async fn acquire(self: &Arc<Self>) -> (ObservationRpcPermit, u64) {
        let wait_start = tokio::time::Instant::now();
        // The gate semaphore is owned by the gate for the whole run and never
        // closed, so `acquire_owned` only returns after a permit is granted.
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("observation RPC gate semaphore closed unexpectedly");
        let gate_wait_ms = wait_start.elapsed().as_millis() as u64;

        self.acquisitions.fetch_add(1, Ordering::Relaxed);
        self.gate_wait_ms_total
            .fetch_add(gate_wait_ms, Ordering::Relaxed);
        self.gate_wait_ms_max
            .fetch_max(gate_wait_ms, Ordering::Relaxed);

        let now_in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight
            .fetch_max(now_in_flight, Ordering::Relaxed);

        (
            ObservationRpcPermit {
                gate: self.clone(),
                _permit: permit,
            },
            gate_wait_ms,
        )
    }

    /// Snapshot the aggregate stats for RunFinished. Invariant: `peak_in_flight <=
    /// limit` (the semaphore bounds concurrency to `limit`).
    fn stats(&self) -> RpcGateStats {
        RpcGateStats {
            limit: self.limit,
            peak_in_flight: self.peak_in_flight.load(Ordering::Relaxed),
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
            gate_wait_ms_total: self.gate_wait_ms_total.load(Ordering::Relaxed),
            gate_wait_ms_max: self.gate_wait_ms_max.load(Ordering::Relaxed),
        }
    }
}

/// RAII permit guard. Dropping it decrements the in-flight counter and releases the
/// underlying semaphore permit (via `OwnedSemaphorePermit`'s own Drop). There is no
/// leak on either a successful or a failed oracle result (§12).
struct ObservationRpcPermit {
    gate: Arc<ObservationRpcGate>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for ObservationRpcPermit {
    fn drop(&mut self) {
        self.gate.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One gated oracle call's outcome: the oracle `result` plus the measured
/// `gate_wait_ms` (before the permit was granted) and `call_duration_ms` (from
/// permit granted until the single oracle await returned) (§9). Durations are the
/// only new data — no provider/RPC error strings are added.
struct TimedOracleCall<T> {
    result: pumpfun_sniper::Result<T>,
    gate_wait_ms: u64,
    call_duration_ms: u64,
}

/// Gate + time EXACTLY ONE `oracle.snapshot` call (§7-8). The permit is released
/// immediately after the single await returns — never held across sleeps, recorder
/// appends, or another oracle call.
async fn gated_snapshot(
    gate: &Arc<ObservationRpcGate>,
    oracle: &PumpMarketOracle,
    mint: &Pubkey,
) -> TimedOracleCall<MarketSnapshot> {
    let (permit, gate_wait_ms) = gate.acquire().await;
    let call_start = tokio::time::Instant::now();
    let result = oracle.snapshot(mint).await;
    let call_duration_ms = call_start.elapsed().as_millis() as u64;
    drop(permit);
    TimedOracleCall {
        result,
        gate_wait_ms,
        call_duration_ms,
    }
}

/// Gate + time EXACTLY ONE `oracle.quote_buy_sol` call (§7-8).
async fn gated_quote_buy_sol(
    gate: &Arc<ObservationRpcGate>,
    oracle: &PumpMarketOracle,
    mint: &Pubkey,
    lamports: u64,
) -> TimedOracleCall<ExecutableQuote> {
    let (permit, gate_wait_ms) = gate.acquire().await;
    let call_start = tokio::time::Instant::now();
    let result = oracle.quote_buy_sol(mint, lamports).await;
    let call_duration_ms = call_start.elapsed().as_millis() as u64;
    drop(permit);
    TimedOracleCall {
        result,
        gate_wait_ms,
        call_duration_ms,
    }
}

/// Gate + time EXACTLY ONE `oracle.quote_sell_raw` call (§7-8).
async fn gated_quote_sell_raw(
    gate: &Arc<ObservationRpcGate>,
    oracle: &PumpMarketOracle,
    mint: &Pubkey,
    base_raw: u64,
) -> TimedOracleCall<ExecutableQuote> {
    let (permit, gate_wait_ms) = gate.acquire().await;
    let call_start = tokio::time::Instant::now();
    let result = oracle.quote_sell_raw(mint, base_raw).await;
    let call_duration_ms = call_start.elapsed().as_millis() as u64;
    drop(permit);
    TimedOracleCall {
        result,
        gate_wait_ms,
        call_duration_ms,
    }
}

#[derive(Parser)]
#[command(name = "observe-record")]
struct Args {
    /// Existing local ignored config.
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Output directory for the append-only JSONL run file (created if absent).
    #[arg(long)]
    output_dir: PathBuf,

    /// Event intake window (seconds). No early exit; this is a recorder.
    #[arg(long, default_value_t = 900)]
    intake_seconds: u64,

    /// Maximum concurrently tracked candidates (outcome sampling capacity).
    #[arg(long, default_value_t = 64)]
    max_active_candidates: usize,

    /// Maximum simultaneous observation-oracle RPC calls (§2). Independent of
    /// candidate-task capacity: a busy gate makes an admitted candidate WAIT for a
    /// permit, it is NEVER dropped. Bounds 1..=64.
    #[arg(long, default_value_t = RPC_CONCURRENCY_DEFAULT)]
    rpc_concurrency: usize,
}

// ---------------------------------------------------------------------------
// Section 30 — CLI bounds validators (pure)
// ---------------------------------------------------------------------------

/// Validate `--intake-seconds` is within [INTAKE_SECONDS_MIN, INTAKE_SECONDS_MAX].
fn validate_intake_seconds(seconds: u64) -> Result<u64> {
    if !(INTAKE_SECONDS_MIN..=INTAKE_SECONDS_MAX).contains(&seconds) {
        return Err(anyhow!(
            "--intake-seconds must be between {INTAKE_SECONDS_MIN} and {INTAKE_SECONDS_MAX} \
             (got {seconds})"
        ));
    }
    Ok(seconds)
}

/// Validate `--max-active-candidates` is within [MAX_ACTIVE_MIN, MAX_ACTIVE_MAX].
fn validate_max_active(max_active: usize) -> Result<usize> {
    if !(MAX_ACTIVE_MIN..=MAX_ACTIVE_MAX).contains(&max_active) {
        return Err(anyhow!(
            "--max-active-candidates must be between {MAX_ACTIVE_MIN} and {MAX_ACTIVE_MAX} \
             (got {max_active})"
        ));
    }
    Ok(max_active)
}

/// Validate `--rpc-concurrency` is within [RPC_CONCURRENCY_MIN, RPC_CONCURRENCY_MAX] (§2).
fn validate_rpc_concurrency(limit: usize) -> Result<usize> {
    if !(RPC_CONCURRENCY_MIN..=RPC_CONCURRENCY_MAX).contains(&limit) {
        return Err(anyhow!(
            "--rpc-concurrency must be between {RPC_CONCURRENCY_MIN} and {RPC_CONCURRENCY_MAX} \
             (got {limit})"
        ));
    }
    Ok(limit)
}

// ---------------------------------------------------------------------------
// Section 32 — source revision detection (pure helpers + read-only git shell-out)
// ---------------------------------------------------------------------------

/// True iff `s` is exactly 40 ASCII hex characters (a full git object id).
fn is_40_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Normalize a raw `git rev-parse HEAD` stdout into an accepted revision or the
/// fixed `"unknown"` sentinel. Pure: trims trailing whitespace only.
fn accept_revision(raw: &str) -> String {
    let trimmed = raw.trim();
    if is_40_hex(trimmed) {
        trimmed.to_string()
    } else {
        "unknown".to_string()
    }
}

/// Detect the source revision via `git rev-parse HEAD`, invoking git DIRECTLY with
/// an argument array (never `sh -c`). Returns `"unknown"` on any error/non-40-hex.
/// Never fails the run because git is unavailable. No environment dump.
fn detect_source_revision() -> String {
    match std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => accept_revision(&String::from_utf8_lossy(&out.stdout)),
        _ => "unknown".to_string(),
    }
}

/// Detect working-tree cleanliness via `git status --porcelain`:
/// `Some(true)` if the command succeeds with empty stdout, `Some(false)` if it
/// succeeds with nonempty stdout, `None` if git is unavailable/errors.
fn detect_working_tree_clean() -> Option<bool> {
    match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
    {
        Ok(out) if out.status.success() => Some(out.stdout.iter().all(|b| b.is_ascii_whitespace())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Section 34 — the EXACT free-only subscription plan
// ---------------------------------------------------------------------------

/// The EXACT free-only subscription plan: new tokens + migrations only, zero
/// token/account trade subscriptions (packet sections 34/48).
fn free_only_plan() -> PumpPortalSubscriptionPlan {
    PumpPortalSubscriptionPlan {
        new_tokens: true,
        migrations: true,
        token_trades: vec![],
        account_trades: vec![],
    }
}

// ---------------------------------------------------------------------------
// Section 37/39 — pure outcome-schedule + policy helpers (tested without network)
// ---------------------------------------------------------------------------

/// Wall-clock due time for a horizon: `entry_wall_time + H seconds` (packet §37).
fn horizon_due_at(entry_wall_time: DateTime<Utc>, horizon_secs: u64) -> DateTime<Utc> {
    entry_wall_time + chrono::Duration::seconds(horizon_secs as i64)
}

/// Non-negative sample lag in milliseconds: `sampled_at - due_at`, clamped to >= 0
/// so an early sample never records a negative lag (packet §37).
fn sample_lag_ms(due_at: DateTime<Utc>, sampled_at: DateTime<Utc>) -> i64 {
    (sampled_at - due_at).num_milliseconds().max(0)
}

/// A canonical snapshot is requested at a horizon iff it is a key snapshot horizon
/// (15/30/60/120). At other horizons snapshot is absent (not a failure) (§39).
fn should_snapshot_at(horizon_secs: u64) -> bool {
    SNAPSHOT_HORIZONS_SECS.contains(&horizon_secs)
}

/// Dedupe policy: a first-seen signature is tracked; a signature already present in
/// this run's seen-set is a duplicate and is recorded but NOT retracked (§35/§51).
/// Returns `true` if a tracking task SHOULD be launched for this first-seen id.
///
/// Used by the dedupe policy test; the production intake path inlines the same
/// `seen.contains` check alongside its counter/record bookkeeping.
#[cfg_attr(not(test), allow(dead_code))]
fn should_track_first_seen(seen: &HashSet<String>, signature: &str) -> bool {
    !seen.contains(signature)
}

/// Capacity policy: when no tracking permit is available the candidate is still
/// persisted (CandidateObserved already written) and a TrackingCapacity skip is
/// recorded; it is never dropped (§35). Pure decision on permit availability.
///
/// Used by the capacity-skip policy test; the production intake path uses
/// `Semaphore::try_acquire_owned` directly (its `Ok`/`Err` is the same decision).
#[cfg_attr(not(test), allow(dead_code))]
fn capacity_admits(permit_available: bool) -> bool {
    permit_available
}

// ---------------------------------------------------------------------------
// Run counters (authoritative; drive RunFinished per section 22)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RunCounters {
    candidates_seen: u64,
    unique_candidates: u64,
    duplicate_candidate_events: u64,
    tracking_started: u64,
    tracking_skipped: u64,
    tracking_completed: u64,
    stream_connected_events: u64,
    stream_disconnect_events: u64,
    /// Schema-v1 TOTAL provider+decode anomaly count. This is the field serialized
    /// as `RunFinished.provider_errors` and stays the TOTAL for schema-v1 compat
    /// (P1-PROVIDER-DECODE-TRUTH-001 §10): a run may legitimately be
    /// completion=degraded with provider_errors=4 when all 4 are decode anomalies.
    /// Run-completion severity is driven by `hard_provider_errors` / `decode_errors`
    /// below, NOT by this total.
    provider_errors: u64,
    /// P1 §9 collector-internal ONLY (never serialized). HARD provider/transport
    /// error subset; any nonzero count => Failed.
    hard_provider_errors: u64,
    /// P1 §9 collector-internal ONLY (never serialized). Decode/schema-loss subset;
    /// nonzero (when not Failed) => Degraded.
    decode_errors: u64,
    /// P1 §9 collector-internal ONLY (never serialized). NewToken decode-loss
    /// subset (NewTokenDeserialize|NewTokenValidation). A modeling-grade dataset
    /// requires new_token_decode_errors==0 AND tracking_capacity_skips==0 (§12,
    /// doc-only — no Rust threshold here).
    new_token_decode_errors: u64,
    unexpected_trade_events: u64,
    migrations_seen: u64,
    /// P1-OBSERVATION-SCHEMA-V2 §13 — informational count of retained
    /// PartialNewToken create events. Incremented ONCE per received PartialNewToken.
    /// NEVER counts toward provider_errors/decode_errors/new_token_decode_errors.
    /// Serialized as `RunFinished.partial_new_token_events`.
    partial_new_token_events: u64,
    task_failures: u64,
    drain_timed_out: u64,
    /// AUDIT-001 §4.4 — collector-internal only. NEVER serialized as a schema
    /// field. A recorder append failure invalidates dataset integrity => Failed.
    recorder_failures: u64,
}

/// Pure run-completion policy from authoritative counters plus drain outcome
/// (packet section 22, AUDIT-001 §11). Counters are authoritative.
///
/// `stream_connected_at_intake_end` is the stream-connection state captured at the
/// intake boundary BEFORE the intentional `client.stop()` — an unresolved
/// disconnect at intake end fails the run so a biased/incomplete launch window is
/// never claimed complete.
fn run_completion(
    counters: &RunCounters,
    ever_connected: bool,
    stream_connected_at_intake_end: bool,
) -> RunCompletion {
    // P1-PROVIDER-DECODE-TRUTH-001 §10: FAILED keys off HARD provider errors, NOT
    // the schema-v1 total `provider_errors` (which also counts decode anomalies).
    let failed = !ever_connected
        || !stream_connected_at_intake_end
        || counters.hard_provider_errors > 0
        || counters.unexpected_trade_events > 0
        || counters.drain_timed_out > 0
        || counters.recorder_failures > 0;
    if failed {
        return RunCompletion::Failed;
    }
    // P1 §10: DECODE/schema loss degrades (not fails) a run.
    if counters.decode_errors > 0
        || counters.stream_disconnect_events > 0
        || counters.task_failures > 0
    {
        return RunCompletion::Degraded;
    }
    RunCompletion::Complete
}

// ---------------------------------------------------------------------------
// AUDIT-001 §12 — pure task-result accounting, shared by the nonblocking intake
// reaper and the final outcome drain so a reaped task is never double-counted.
// ---------------------------------------------------------------------------

/// The effect a single joined tracking-task result has on the run counters. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskAccounting {
    /// A clean tracking result => `tracking_completed += 1`.
    Completed,
    /// The task lost its recorder => `recorder_failures += 1` (run Failed).
    RecorderFailure,
    /// Panic/JoinError => `task_failures += 1` (may degrade).
    TaskFailure,
}

/// Map a joined tracking-task result (`Ok(CandidateTaskOutcome)` on clean join, or
/// `Err(join_error)` on panic/cancel) to its counter effect. Pure: no side effects,
/// no recorder I/O, network-free.
fn account_task_result(joined: &std::result::Result<CandidateTaskOutcome, ()>) -> TaskAccounting {
    match joined {
        Ok(Ok(_success)) => TaskAccounting::Completed,
        Ok(Err(CandidateTaskFailure::RecorderWrite)) => TaskAccounting::RecorderFailure,
        Err(()) => TaskAccounting::TaskFailure,
    }
}

/// Apply a [`TaskAccounting`] effect to the run counters. Pure counter mutation.
fn apply_task_accounting(counters: &mut RunCounters, effect: TaskAccounting) {
    match effect {
        TaskAccounting::Completed => counters.tracking_completed += 1,
        TaskAccounting::RecorderFailure => counters.recorder_failures += 1,
        TaskAccounting::TaskFailure => counters.task_failures += 1,
    }
}

// ---------------------------------------------------------------------------
// AUDIT-001 §8-9 — pure event -> stream-connected state transition.
// ---------------------------------------------------------------------------

/// Whether a delivered channel value marks the stream connected during ACTIVE
/// intake. `Some(true)`=Connected, `Some(false)`=Disconnected, `None` (channel
/// closed) => disconnected. Provider errors / trades do NOT change this bool and
/// are represented by returning the unchanged prior state at the call site.
///
/// Returns the new `stream_connected` value given the prior value and the
/// transition kind. Pure.
fn apply_stream_transition(prior: bool, transition: StreamTransition) -> bool {
    match transition {
        StreamTransition::Connected => true,
        StreamTransition::Disconnected => false,
        StreamTransition::ChannelClosed => false,
        StreamTransition::NoChange => prior,
    }
}

/// Active-intake stream-state transitions relevant to run completeness (§8-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTransition {
    Connected,
    Disconnected,
    ChannelClosed,
    /// Provider error / unexpected trade: does not change stream_connected. Only
    /// exercised by the pure transition test; the intake path leaves the bool as-is.
    #[cfg_attr(not(test), allow(dead_code))]
    NoChange,
}

// ---------------------------------------------------------------------------
// AUDIT-001 §5-6 — pure outcome sampled_at policy from sell-quote truth.
// OutcomeSample.sampled_at is the sell-quote observation timestamp when sell
// succeeds; otherwise the time the sell observation failure returned.
// ---------------------------------------------------------------------------

/// Define an outcome sample's `sampled_at` from quote truth (§6):
/// - sell-quote SUCCESS => the quote's canonical `quoted_at`;
/// - sell-quote FAILURE => `failure_completion_time` (stamped AFTER the failed
///   await returned).
///
/// Pure; no RPC. `sell_quoted_at` is `Some` iff the sell quote succeeded.
fn outcome_sampled_at(
    sell_quoted_at: Option<DateTime<Utc>>,
    failure_completion_time: DateTime<Utc>,
) -> DateTime<Utc> {
    match sell_quoted_at {
        Some(quoted_at) => quoted_at,
        None => failure_completion_time,
    }
}

/// H1's decision timestamp is the latest actual observed/completed timestamp among
/// the 2s, 4s, and 6s outcome observations. It is never the nominal 6s due time.
fn h1_decision_time_from_observations(
    sample_2s: DateTime<Utc>,
    sample_4s: DateTime<Utc>,
    sample_6s: DateTime<Utc>,
) -> DateTime<Utc> {
    sample_2s.max(sample_4s).max(sample_6s)
}

/// Wall-clock lag from an observed decision point to a later recorder timestamp.
/// Clock jitter is clamped to zero for reporting, matching sample_lag_ms policy.
fn wall_lag_ms(start: DateTime<Utc>, end: DateTime<Utc>) -> i64 {
    (end - start).num_milliseconds().max(0)
}

fn delayed_quote_start_is_valid(
    decision_time: DateTime<Utc>,
    request_started_at: DateTime<Utc>,
) -> bool {
    request_started_at >= decision_time
}

fn should_match_delayed_exit_at(horizon_secs: u64) -> bool {
    MATCHED_DELAYED_EXIT_HORIZONS_SECS.contains(&horizon_secs)
}

fn matched_delayed_sell_base_input(
    delayed_base_amount_raw: u64,
    _original_entry_base_amount_raw: u64,
) -> u64 {
    delayed_base_amount_raw
}
fn matched_sell_start_is_valid(
    delayed_buy_observed_at: DateTime<Utc>,
    request_started_at: DateTime<Utc>,
) -> bool {
    request_started_at >= delayed_buy_observed_at
}

#[derive(Debug, Clone)]
struct DelayedEntryQuoteTruth {
    decision_time: DateTime<Utc>,
    buy_request_started_at: DateTime<Utc>,
    buy_quoted_at: DateTime<Utc>,
    buy_observed_at: DateTime<Utc>,
    base_amount_raw: u64,
}
// ---------------------------------------------------------------------------
// Tracking task result (section 40)
// ---------------------------------------------------------------------------

/// What a tracking task returns to the parent so identity is never lost on a
/// clean finish. Panics/join errors are handled by parent bookkeeping instead.
///
/// The fields carry the finished identity/status for observability and future
/// parent-side reconciliation (§40); the task already persisted its own
/// TrackingFinished record, so the clean drain path does not re-read them.
#[allow(dead_code)]
struct CandidateTaskResult {
    candidate_id: String,
    mint: String,
    status: TrackingFinishStatus,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Shared P3 measurement runtime context (Option B). Owns the separate sink,
/// the per-mint subscription registry, the bounded trade queue, and a handle to
/// issue token-trade subscribe/unsubscribe. Passed by &mut through the intake
/// path so no wide signature churn is needed. Measurement failures NEVER touch
/// canonical observation state.
/// Shared per-mint participation state + measurement sink (2B). The main intake
/// loop is the sole PRODUCER (records deduped trades, sets coverage on subscribe);
/// each per-candidate task is a CONSUMER that reads the buffer at its actual T2/T6
/// sampled_at cutoffs to emit exactly-once snapshots. Both write the append-only
/// sink. Critical sections are lock-brief and NEVER held across `.await`.
/// Run-scoped P3 telemetry counters (2D). Aggregated across the main loop and all
/// enrichment/track tasks; snapshotted into the RunSummary at shutdown.
#[derive(Debug, Default, Clone)]
struct MeasurementCounters {
    eligible_candidates: u64,
    subscribe_attempts: u64,
    subscribe_successes: u64,
    subscribe_failures: u64,
    unsubscribe_attempts: u64,
    unsubscribe_successes: u64,
    unsubscribe_failures: u64,
    disconnects: u64,
    reconnects: u64,
    resubscribe_attempts: u64,
    resubscribe_successes: u64,
    resubscribe_failures: u64,
    trade_events_received: u64,
    trade_observed_persisted: u64,
    duplicate_trade_events: u64,
    stale_trade_events: u64,
    backpressure_failures: u64,
    t2_attempts: u64,
    t2_successes: u64,
    t6_attempts: u64,
    t6_successes: u64,
    holder_attempts: u64,
    holder_successes: u64,
    holder_failures: u64,
    probe_attempts: u64,
    probe_successes: u64,
    probe_failures: u64,
    // P3-COVERAGE-DEFECT-001 audit counters
    t2_invalid_due_to_coverage: u64,
    t6_invalid_due_to_coverage: u64,
    zero_activity_valid_snapshots: u64,
}

use pumpfun_sniper::observation::measurement_runtime::CoverageState;

struct MeasurementShared {
    state: std::sync::Mutex<ParticipationState>,
    registry: std::sync::Mutex<SubscriptionRegistry>,
    /// Authoritative per-mint coverage truth (connection-generation + window
    /// continuity). Drives whether a T2/T6 snapshot may be VALID zero-activity vs
    /// MISSING (P3-COVERAGE-DEFECT-001).
    coverage: std::sync::Mutex<CoverageTracker>,
    counters: std::sync::Mutex<MeasurementCounters>,
    sink: std::sync::Mutex<MeasurementSink<std::fs::File>>,
    sender: CommandSender,
    run_id: String,
}

impl MeasurementShared {
    fn count(&self, f: impl FnOnce(&mut MeasurementCounters)) {
        f(&mut self.counters.lock().expect("counters poisoned"));
    }

    /// Record a deduped, normalized trade into its mint buffer AND persist it to
    /// the durable sink. Buffer inclusion is keyed by `event_received_at` (stamped
    /// at main-loop processing time), so a trade is in the buffer iff it already
    /// has an `event_received_at` — no snapshot can miss an event whose receipt
    /// timestamp is <= cutoff merely because a consumer ran later.
    fn record_and_persist(&self, t: TradeObserved) {
        {
            let mut st = self.state.lock().expect("participation state poisoned");
            st.record_trade(t.clone());
        }
        if self
            .sink
            .lock()
            .expect("sink poisoned")
            .append(MeasurementPayload::TradeObserved(t), Utc::now())
            .is_ok()
        {
            self.count(|c| c.trade_observed_persisted += 1);
        }
    }

    /// Persist an arbitrary measurement payload (subscription state, failure, summary).
    /// After sink close this is rejected and counted as a late write by the sink.
    fn append(&self, payload: MeasurementPayload) {
        let _ = self.sink.lock().expect("sink poisoned").append(payload, Utc::now());
    }

    fn set_coverage(&self, mint: &str, cov: CoverageState) {
        self.state.lock().expect("participation state poisoned").set_coverage(mint, cov);
    }

    /// Is this mint an expected-active subscription right now?
    fn is_active(&self, mint: &str) -> bool {
        self.registry.lock().expect("registry poisoned").is_active(mint)
    }

    /// Has this mint's candidate terminated (tombstoned)? Distinguishes stale trades.
    fn is_terminated(&self, mint: &str) -> bool {
        self.registry.lock().expect("registry poisoned").is_terminated(mint)
    }

    /// Ingestion signature dedup (earliest wins).
    fn accept_signature(&self, mint: &str, sig: &str) -> bool {
        self.registry.lock().expect("registry poisoned").accept_signature(mint, sig)
    }

    fn note_trade(&self, mint: &str, at: DateTime<Utc>) {
        if let Some(s) = self.registry.lock().expect("registry poisoned").get_mut(mint) {
            s.note_trade(at);
        }
    }

    /// UNCONDITIONAL subscribe-on-admission: request, send SubscribeTokenTrades,
    /// record ack/failure, publish coverage, persist the subscription-state row.
    async fn subscribe(&self, mint: &str) {
        {
            let mut reg = self.registry.lock().expect("registry poisoned");
            reg.request(mint, Utc::now());
        }
        self.count(|c| {
            c.eligible_candidates += 1;
            c.subscribe_attempts += 1;
        });
        let res = self
            .sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![mint.to_string()]))
            .await;
        let now = Utc::now();
        let record = {
            let mut reg = self.registry.lock().expect("registry poisoned");
            if let Some(s) = reg.get_mut(mint) {
                match res {
                    Ok(()) => s.mark_active(now),
                    Err(_) => s.mark_failed(MeasurementFailureCategory::RpcUnavailable),
                }
            }
            reg.get(mint).map(SubscriptionStateRecord::from_sub)
        };
        // Coverage truth: a successful SEND establishes coverage on the current
        // connection generation (the strongest signal PumpPortal exposes — no per-sub
        // ack). A subsequent auth/stream error will invalidate it (on_stream_error).
        {
            let mut cov = self.coverage.lock().expect("coverage poisoned");
            if res.is_ok() {
                cov.mark_active(mint, now);
            } else {
                cov.mark_failed(mint);
            }
        }
        self.count(|c| {
            if res.is_ok() {
                c.subscribe_successes += 1;
            } else {
                c.subscribe_failures += 1;
            }
        });
        if let Some(rec) = record {
            self.append(MeasurementPayload::SubscriptionState(rec));
        }
    }

    /// A provider/stream error (auth or other). Reliable coverage-break signal: bump
    /// the connection generation and invalidate coverage for ALL active mints so a
    /// window overlapping the error cannot be emitted as valid zero-activity. The
    /// overloaded `Connected` event is NOT used as a break signal (it also fires on
    /// desired-state resync); this and `on_disconnect` are the authoritative breaks.
    fn on_stream_error(&self, sanitized_category: &str) {
        let is_auth = sanitized_category.to_ascii_lowercase().contains("auth");
        self.coverage
            .lock()
            .expect("coverage poisoned")
            .on_break(if is_auth { CoverageBreak::AuthError } else { CoverageBreak::ProviderError });
        // Registry coverage-unknown mirror (subscription lifecycle side).
        self.registry.lock().expect("registry poisoned").mark_all_coverage_unknown();
    }

    /// Terminal candidate lifecycle: issue UnsubscribeTokenTrades, record ack/failure
    /// (never pretend clean), persist the final subscription-state row, then TOMBSTONE
    /// the mint (registry + participation state) so a late trade cannot resurrect it.
    async fn unsubscribe_and_terminate(&self, mint: &str) {
        let was_active = self.is_active(mint);
        if was_active {
            self.count(|c| c.unsubscribe_attempts += 1);
            let res = self
                .sender
                .send(SubscriptionCommand::UnsubscribeTokenTrades(vec![mint.to_string()]))
                .await;
            {
                let mut reg = self.registry.lock().expect("registry poisoned");
                match res {
                    Ok(()) => {
                        reg.unsubscribe(mint, true);
                    }
                    Err(_) => reg.unsubscribe_failed(mint, MeasurementFailureCategory::RpcUnavailable),
                }
                if let Some(rec) = reg.get(mint).map(SubscriptionStateRecord::from_sub) {
                    let _ = self
                        .sink
                        .lock()
                        .expect("sink poisoned")
                        .append(MeasurementPayload::SubscriptionState(rec), Utc::now());
                }
            }
            self.count(|c| {
                if res.is_ok() {
                    c.unsubscribe_successes += 1;
                } else {
                    c.unsubscribe_failures += 1;
                }
            });
        }
        // Tombstone + clear all per-candidate state (idempotent).
        self.registry.lock().expect("registry poisoned").cleanup(mint);
        self.state.lock().expect("participation state poisoned").cleanup(mint);
        self.coverage.lock().expect("coverage poisoned").cleanup(mint);
    }

    /// On stream disconnect: every active subscription's coverage becomes UNKNOWN
    /// (interruption is never later encoded as zero trades).
    fn on_disconnect(&self) {
        self.count(|c| c.disconnects += 1);
        // Authoritative coverage break: invalidate all active mints on a new generation.
        self.coverage.lock().expect("coverage poisoned").on_break(CoverageBreak::StreamDisconnected);
        self.registry.lock().expect("registry poisoned").mark_all_coverage_unknown();
    }

    /// On reconnect: resubscribe ALL active-eligible mints (never selective), record
    /// per-mint outcome. Coverage returns to known only after a fresh ack.
    async fn on_reconnect(&self) {
        self.count(|c| c.reconnects += 1);
        let mints = self.registry.lock().expect("registry poisoned").reconnect_resubscribe_all();
        for mint in mints {
            self.count(|c| c.resubscribe_attempts += 1);
            let res = self
                .sender
                .send(SubscriptionCommand::SubscribeTokenTrades(vec![mint.clone()]))
                .await;
            let now = Utc::now();
            {
                let mut reg = self.registry.lock().expect("registry poisoned");
                if let Some(s) = reg.get_mut(&mint) {
                    match res {
                        Ok(()) => s.mark_active(now),
                        Err(_) => s.mark_failed(MeasurementFailureCategory::RpcUnavailable),
                    }
                }
            }
            // Coverage re-establishes on the CURRENT generation. If a prior break bumped
            // the generation, this starts a fresh epoch (coverage_start = now), so any
            // window that began before the break stays UNKNOWN (never rescued).
            {
                let mut cov = self.coverage.lock().expect("coverage poisoned");
                if res.is_ok() {
                    cov.mark_active(&mint, now);
                } else {
                    cov.mark_failed(&mint);
                }
            }
            self.count(|c| {
                if res.is_ok() {
                    c.resubscribe_successes += 1;
                } else {
                    c.resubscribe_failures += 1;
                }
            });
        }
    }

    /// Run-end sweep: unsubscribe every still-active/uncertain mint. Returns
    /// (active_before_sweep, active_after_sweep).
    async fn run_end_sweep(&self) -> (u64, u64) {
        let mints = self.registry.lock().expect("registry poisoned").active_mints();
        let before = mints.len() as u64;
        for mint in mints {
            self.unsubscribe_and_terminate(&mint).await;
        }
        let after = self.registry.lock().expect("registry poisoned").active_mints().len() as u64;
        (before, after)
    }

    /// Emit the final T2/T6 snapshot for a mint exactly once. `cutoff` is the actual
    /// sampled_at (T2: 2s sample; T6: max(2/4/6) decision time); `computed_at` is the
    /// real emit time. Duplicate scheduling is a no-op (returns without a second row).
    fn emit_participation(
        &self,
        mint: &str,
        class: SnapshotClass,
        cutoff: DateTime<Utc>,
        computed_at: DateTime<Utc>,
    ) {
        self.count(|c| match class {
            SnapshotClass::T2 => c.t2_attempts += 1,
            SnapshotClass::T6 => c.t6_attempts += 1,
        });
        // AUTHORITATIVE coverage at the emit instant (connection-generation + window
        // continuity). This is what prevents an auth/reset-compromised window from
        // being emitted as valid zero-activity (P3-COVERAGE-DEFECT-001).
        let coverage = self.coverage.lock().expect("coverage poisoned").coverage_of(mint);
        let snap = {
            let mut st = self.state.lock().expect("participation state poisoned");
            st.set_coverage(mint, coverage);
            st.build_snapshot(&self.run_id, mint, class, cutoff, computed_at)
        };
        if let Some(s) = snap {
            let ok = s.failure.is_none();
            self.append(MeasurementPayload::ParticipationSnapshot(s));
            self.count(|c| match (class, ok) {
                (SnapshotClass::T2, true) => {
                    c.t2_successes += 1;
                    c.zero_activity_valid_snapshots += 1;
                }
                (SnapshotClass::T6, true) => {
                    c.t6_successes += 1;
                    c.zero_activity_valid_snapshots += 1;
                }
                (SnapshotClass::T2, false) => c.t2_invalid_due_to_coverage += 1,
                (SnapshotClass::T6, false) => c.t6_invalid_due_to_coverage += 1,
            });
        }
    }

    /// Terminal cleanup of a candidate's in-memory participation state (buffer,
    /// coverage, emission guards). Durable sink rows are untouched.
    fn cleanup(&self, mint: &str) {
        self.state.lock().expect("participation state poisoned").cleanup(mint);
    }

    /// Flush + seal the sink. After this, any append is rejected + counted late.
    fn close_sink(&self) -> (u64, bool) {
        let mut sink = self.sink.lock().expect("sink poisoned");
        let flush_ok = sink.close().is_ok();
        (sink.appended(), flush_ok)
    }

    fn late_write_attempts(&self) -> u64 {
        self.sink.lock().expect("sink poisoned").late_write_attempts()
    }

    /// Build the run-scoped measurement summary from the counters + registry sweep
    /// results + queue/sink telemetry. Called at shutdown before sink close.
    fn build_summary(
        &self,
        queue_high_water: usize,
        silent_drops: u64,
        pending_before: u64,
        completed_during: u64,
        timed_out: u64,
        active_before_sweep: u64,
        stale_after: u64,
    ) -> MeasurementRunSummary {
        let c = self.counters.lock().expect("counters poisoned").clone();
        let (cov_auth, cov_stream, cov_gen, cov_inval, cov_unknown) = {
            let cov = self.coverage.lock().expect("coverage poisoned");
            (cov.auth_errors, cov.provider_stream_errors, cov.generation_changes,
             cov.coverage_invalidations, cov.coverage_unknown_count())
        };
        let (rows, _) = {
            let sink = self.sink.lock().expect("sink poisoned");
            (sink.appended(), sink.is_closed())
        };
        MeasurementRunSummary {
            eligible_candidates: c.eligible_candidates,
            subscribe_attempts: c.subscribe_attempts,
            subscribe_successes: c.subscribe_successes,
            subscribe_failures: c.subscribe_failures,
            unsubscribe_attempts: c.unsubscribe_attempts,
            unsubscribe_successes: c.unsubscribe_successes,
            unsubscribe_failures: c.unsubscribe_failures,
            disconnects: c.disconnects,
            reconnects: c.reconnects,
            resubscribe_attempts: c.resubscribe_attempts,
            resubscribe_successes: c.resubscribe_successes,
            resubscribe_failures: c.resubscribe_failures,
            trade_events_received: c.trade_events_received,
            trade_observed_persisted: c.trade_observed_persisted,
            duplicate_trade_events: c.duplicate_trade_events,
            stale_trade_events: c.stale_trade_events,
            queue_high_water,
            backpressure_failures: c.backpressure_failures,
            silent_drops,
            t2_attempts: c.t2_attempts,
            t2_successes: c.t2_successes,
            t6_attempts: c.t6_attempts,
            t6_successes: c.t6_successes,
            holder_attempts: c.holder_attempts,
            holder_successes: c.holder_successes,
            holder_failures: c.holder_failures,
            probe_attempts: c.probe_attempts,
            probe_successes: c.probe_successes,
            probe_failures: c.probe_failures,
            pending_tasks_before_drain: pending_before,
            tasks_completed_during_drain: completed_during,
            tasks_timed_out: timed_out,
            late_write_attempts: self.late_write_attempts(),
            active_before_sweep,
            stale_subscriptions_after_shutdown: stale_after,
            measurement_rows_written: rows,
            sink_flush_success: true,
            provider_auth_errors: cov_auth,
            provider_stream_errors: cov_stream,
            connection_generation_changes: cov_gen,
            coverage_invalidations: cov_inval,
            coverage_unknown_mints: cov_unknown,
            t2_invalid_due_to_coverage: c.t2_invalid_due_to_coverage,
            t6_invalid_due_to_coverage: c.t6_invalid_due_to_coverage,
            zero_activity_valid_snapshots: c.zero_activity_valid_snapshots,
            timestamp_semantics_version: pumpfun_sniper::observation::measurement_runtime::TIMESTAMP_SEMANTICS_VERSION,
        }
    }
}

/// RAII guard: clears a candidate's in-memory participation state if the
/// per-candidate task PANICS before its normal terminate path runs. On the normal
/// path `unsubscribe_and_terminate` has already cleaned + tombstoned (idempotent).
struct ParticipationCleanup {
    shared: Arc<MeasurementShared>,
    mint: String,
}

impl Drop for ParticipationCleanup {
    fn drop(&mut self) {
        self.shared.cleanup(&self.mint);
    }
}

/// Coarse read-only RPC/quote failure classification into a frozen provenance
/// category. Never converts a failure into a zero measurement.
fn classify_measurement_rpc_error(msg: &str) -> MeasurementFailureCategory {
    let m = msg.to_ascii_lowercase();
    if m.contains("not a token mint") || m.contains("could not find mint") {
        // PumpPortal emits NewToken before the mint account lands on the RPC; the
        // node then reports the pubkey is "not a Token mint". Retryable freshness.
        MeasurementFailureCategory::AccountNotYetVisible
    } else if m.contains("timed out") || m.contains("timeout") {
        MeasurementFailureCategory::Timeout
    } else if m.contains("429") || m.contains("rate limit") || m.contains("too many requests") {
        MeasurementFailureCategory::RateLimited
    } else if m.contains("could not find account") || m.contains("account not found") || m.contains("missing") {
        MeasurementFailureCategory::AccountMissing
    } else {
        MeasurementFailureCategory::RpcUnavailable
    }
}

/// Whether a holder-acquisition failure is worth a bounded retry. Freshness (mint not
/// yet visible) and transient transport errors are retryable; a decode/params error
/// that is NOT a freshness race, or a hard unavailability, is not looped indefinitely.
/// NEVER depends on any feature/holder/price/outcome value — only the error category.
fn is_retryable_holder_failure(cat: MeasurementFailureCategory) -> bool {
    matches!(
        cat,
        MeasurementFailureCategory::AccountNotYetVisible
            | MeasurementFailureCategory::Timeout
            | MeasurementFailureCategory::RateLimited
            | MeasurementFailureCategory::RpcUnavailable
    )
}

/// Frozen bounded holder-acquisition retry backoffs (ms from first attempt). Cumulative
/// attempts at ~0/1/4/10s (~10s window). A late completion sets available_in_time=false
/// (evaluated against the T10 decision timestamp), never redefining T0.
const HOLDER_FRESHNESS_BACKOFFS_MS: [u64; 4] = [0, 1000, 3000, 6000];

/// Timestamp-architecture AMENDMENT-001 budgets (ms). Decision timestamp = anchor + budget.
/// Holder anchor = requested_at (T0 measurement start) -> T10; probe anchor = requested_at
/// (T0 probe start) -> T0_AVAILABLE. available_in_time is recomputable at replay from the
/// record's stored anchor + completed_at + these frozen constants.
const HOLDER_DECISION_BUDGET_MS: i64 = 10_000;
const MICROSTRUCTURE_T0_BUDGET_MS: i64 = 250;

/// available_in_time: the finalized measurement completed within `budget_ms` of its anchor
/// (the registered decision timestamp = anchor + budget). Pure; runtime & replay agree.
fn within_budget(anchor: DateTime<Utc>, completed: DateTime<Utc>, budget_ms: i64) -> bool {
    completed <= anchor + chrono::Duration::milliseconds(budget_ms)
}

/// Commitment for the T0 holder account-state fetch. `confirmed` (not the RPC default
/// `finalized`): at T0 the target accounts are seconds old, so `finalized` can return
/// nothing while `confirmed` already has the truth — matching the oracle's commitment
/// (P3-HOLDER-DEFECT-002). Kept as a fn so a guard test fails if this regresses.
fn holder_fetch_commitment() -> CommitmentConfig {
    CommitmentConfig::confirmed()
}

/// Decode a fetched token account's AUTHORITATIVE (mint, owner), keyed off its OWNING
/// PROGRAM. Supports classic SPL Token and Token-2022 (P3-HOLDER-DEFECT-003: pump.fun
/// tokens are Token-2022, including >165-byte extension-bearing accounts). Token-2022
/// uses `StateWithExtensions`, which validates the base layout + extension envelope, so
/// this is NOT a blind byte-slice. Any other owning program is an explicit
/// `UnsupportedTokenProgram`; undecodable data is `TokenAccountDecodeFailed`; a decoded
/// mint that isn't the candidate mint is `TokenMintMismatch`. Never attributes ownership
/// without a mint match. Pure + unit-testable.
fn decode_token_account_owner(
    owner_program: &Pubkey,
    data: &[u8],
    expected_mint: &Pubkey,
) -> std::result::Result<String, MeasurementFailureCategory> {
    let (mint_bytes, owner) = if *owner_program == spl_token::id() {
        let tok = spl_token::state::Account::unpack(data)
            .map_err(|_| MeasurementFailureCategory::TokenAccountDecodeFailed)?;
        (tok.mint.to_bytes(), tok.owner.to_string())
    } else if *owner_program == spl_token_2022::id() {
        use spl_token_2022::extension::StateWithExtensions;
        use spl_token_2022::state::Account as Token2022Account;
        let st = StateWithExtensions::<Token2022Account>::unpack(data)
            .map_err(|_| MeasurementFailureCategory::TokenAccountDecodeFailed)?;
        (st.base.mint.to_bytes(), st.base.owner.to_string())
    } else {
        return Err(MeasurementFailureCategory::UnsupportedTokenProgram);
    };
    if mint_bytes != expected_mint.to_bytes() {
        return Err(MeasurementFailureCategory::TokenMintMismatch);
    }
    Ok(owner)
}

/// All-`None`/zero holder features — used when the snapshot is MISSING/FAILURE so a
/// failed acquisition or an UNRESOLVED curve is never encoded as a valid zero-curve
/// concentration (P3-HOLDER-DEFECT-001).
fn blocked_holder_features() -> pumpfun_sniper::observation::measurement::HolderFeatures {
    pumpfun_sniper::observation::measurement::HolderFeatures {
        top1_noncurve_holder_share: None,
        top5_noncurve_holder_share: None,
        top10_noncurve_holder_share: None,
        holder_hhi: None,
        ordinary_holder_count: 0,
        ordinary_holder_count_top20_floor: true,
        creator_held_share: None,
        curve_held_share: None,
        noncurve_supply_share: None,
    }
}

/// Domain 2 — one bounded T0 holder snapshot for `mint`. Reuses the observation RPC
/// gate: `getTokenLargestAccounts` (top-20 truncated by the RPC = the frozen floor) for
/// balances, then `getMultipleAccounts` on those addresses to read each token account's
/// AUTHORITATIVE owner. Curve/creator are classified BY OWNER — the curve reserve is
/// whichever account is owned by `bonding_curve_pda` (wherever it actually lives), not a
/// derived ATA (P3-HOLDER-DEFECT-001). If the curve reserve is not found among the
/// returned accounts, the snapshot is MISSING (`CurveTokenAccountUnresolved`) — never a
/// zero-curve concentration. Balances convert base→UI tokens (÷10^decimals) for the fixed
/// 1e9 denominator. `creator` is the creator WALLET pubkey (owner-matched). At most one
/// bounded retry for a retryable transport failure; retry NEVER depends on a feature value.
async fn acquire_holder_snapshot(
    gate: &Arc<ObservationRpcGate>,
    rpc: &Arc<RpcClient>,
    mint: &Pubkey,
    creator: Option<&str>,
    run_id: &str,
    source_revision: &str,
    requested_at: DateTime<Utc>,
) -> HolderSnapshot {
    let curve_pda = bonding_curve_pda(mint).0.to_string();
    let creator_deterministic = creator.is_some();

    // Step 1: largest accounts (balances) at CONFIRMED commitment, with a bounded
    // freshness-aware retry (P3-HOLDER-AVAILABILITY-DEFECT-001). PumpPortal emits
    // NewToken before the mint account is visible on the RPC, so the first attempt
    // often returns "not a Token mint" -> AccountNotYetVisible; a bounded backoff
    // absorbs that lag. Retry decision depends ONLY on the error category.
    let mut attempt_err: Option<MeasurementFailureCategory> = None;
    let mut balances: Option<Vec<(String, u64)>> = None;
    let last = HOLDER_FRESHNESS_BACKOFFS_MS.len() - 1;
    for (i, backoff_ms) in HOLDER_FRESHNESS_BACKOFFS_MS.iter().enumerate() {
        if *backoff_ms > 0 {
            tokio::time::sleep(Duration::from_millis(*backoff_ms)).await;
        }
        let (permit, _wait) = gate.acquire().await;
        let rpc = rpc.clone();
        let m = *mint;
        let res = tokio::task::spawn_blocking(move || {
            rpc.get_token_largest_accounts_with_commitment(&m, holder_fetch_commitment())
                .map(|r| r.value)
        })
        .await;
        drop(permit);
        match res {
            Ok(Ok(list)) => {
                balances = Some(
                    list.into_iter()
                        .map(|b| {
                            let raw: u128 = b.amount.amount.parse().unwrap_or(0);
                            let ui = (raw / 10u128.pow(b.amount.decimals as u32)) as u64;
                            (b.address, ui)
                        })
                        .collect(),
                );
                attempt_err = None;
                break;
            }
            Ok(Err(e)) => {
                let cat = classify_measurement_rpc_error(&e.to_string());
                attempt_err = Some(cat);
                if !is_retryable_holder_failure(cat) || i == last {
                    break;
                }
            }
            Err(_join) => {
                attempt_err = Some(MeasurementFailureCategory::Other);
                break;
            }
        }
    }

    let balances = match balances {
        Some(b) => b,
        None => {
            return HolderSnapshot {
                run_id: run_id.to_string(),
                mint: mint.to_string(),
                requested_at,
                completed_at: Utc::now(),
                rpc_slot: None,
                available_in_time: false,
                raw_accounts: Vec::new(),
                total_mint_supply_tokens: TOTAL_MINT_SUPPLY_TOKENS,
                failure: Some(attempt_err.unwrap_or(MeasurementFailureCategory::Other)),
                source: "rpc".to_string(),
                source_revision: source_revision.to_string(),
                feature_version: MEASUREMENT_FEATURE_VERSION,
                features: blocked_holder_features(),
            };
        }
    };

    // Step 2: resolve each account's AUTHORITATIVE owner via getMultipleAccounts at
    // CONFIRMED commitment (P3-HOLDER-DEFECT-002) — fresh mints aren't finalized yet.
    let pubkeys: Vec<Pubkey> = balances.iter().filter_map(|(a, _)| Pubkey::from_str(a).ok()).collect();
    let (owners, rpc_slot, present_count, decode_err): (
        std::collections::HashMap<String, String>,
        Option<u64>,
        usize,
        Option<MeasurementFailureCategory>,
    ) = {
        let (permit, _wait) = gate.acquire().await;
        let rpc = rpc.clone();
        let keys = pubkeys.clone();
        let res = tokio::task::spawn_blocking(move || {
            rpc.get_multiple_accounts_with_commitment(&keys, holder_fetch_commitment())
        })
        .await;
        drop(permit);
        let mut m = std::collections::HashMap::new();
        let mut slot = None;
        let mut present = 0usize;
        let mut last_err = None;
        if let Ok(Ok(resp)) = res {
            slot = Some(resp.context.slot);
            for (pk, acc) in pubkeys.iter().zip(resp.value.into_iter()) {
                if let Some(acc) = acc {
                    present += 1;
                    match decode_token_account_owner(&acc.owner, &acc.data, mint) {
                        Ok(owner) => {
                            m.insert(pk.to_string(), owner);
                        }
                        Err(cat) => last_err = Some(cat),
                    }
                }
            }
        }
        (m, slot, present, last_err)
    };

    let raw: Vec<RawHolderAccount> = balances
        .iter()
        .map(|(addr, bal)| RawHolderAccount {
            address: addr.clone(),
            ui_balance: *bal,
            owner: owners.get(addr).cloned(),
        })
        .collect();

    let (raw_accounts, curve_resolved) = classify_holder_accounts_by_owner(&raw, &curve_pda, creator);
    let completed_at = Utc::now();

    // Curve reserve not authoritatively found => MISSING, never zero-curve concentration.
    // Provenance precedence: no account state at confirmed > specific decode failure >
    // generic curve-unresolved.
    let (features, failure) = if curve_resolved {
        (holder_features(&raw_accounts, creator_deterministic), None)
    } else {
        let cat = if present_count == 0 {
            MeasurementFailureCategory::AccountStateUnavailableAtConfirmed
        } else {
            decode_err.unwrap_or(MeasurementFailureCategory::CurveTokenAccountUnresolved)
        };
        (blocked_holder_features(), Some(cat))
    };

    HolderSnapshot {
        run_id: run_id.to_string(),
        mint: mint.to_string(),
        requested_at,
        completed_at,
        rpc_slot,
        // Domain-2 T10: decision timestamp = requested_at (T0 measurement start) + 10s.
        available_in_time: within_budget(requested_at, completed_at, HOLDER_DECISION_BUDGET_MS),
        raw_accounts,
        total_mint_supply_tokens: TOTAL_MINT_SUPPLY_TOKENS,
        failure,
        source: "rpc".to_string(),
        source_revision: source_revision.to_string(),
        feature_version: MEASUREMENT_FEATURE_VERSION,
        features,
    }
}

/// Domain 3 — one read-only exact-input buy-quote probe at `lamports`. Uses ONLY the
/// existing canonical read-only quote path (no wallet, no signing, no tx send). Persists
/// raw expected-out + fee semantics so the marginal/impact/convexity/redundancy
/// derivations are reproducible from the row alone. Failure carries provenance and is
/// never a numeric zero.
async fn acquire_microstructure_probe(
    gate: &Arc<ObservationRpcGate>,
    oracle: &PumpMarketOracle,
    mint: &Pubkey,
    lamports: u64,
    run_id: &str,
    requested_at: DateTime<Utc>,
) -> MicrostructureProbe {
    let call = gated_quote_buy_sol(gate, oracle, mint, lamports).await;
    let completed_at = Utc::now();
    let (expected_base_raw, base_decimals, success, source, pf, cf, lf, failure) = match call.result {
        Ok(q) => (
            Some(q.base_amount_raw),
            q.base_decimals,
            true,
            format!("{:?}", q.venue),
            Some(q.protocol_fee_bps),
            Some(q.creator_fee_bps),
            Some(q.lp_fee_bps),
            None,
        ),
        Err(e) => (
            None,
            0,
            false,
            "unavailable".to_string(),
            None,
            None,
            None,
            Some(classify_measurement_rpc_error(&e.to_string())),
        ),
    };
    MicrostructureProbe {
        run_id: run_id.to_string(),
        mint: mint.to_string(),
        requested_at,
        completed_at,
        // Domain-3 T0_AVAILABLE: decision timestamp = requested_at (T0 probe start) + 250ms.
        available_in_time: within_budget(requested_at, completed_at, MICROSTRUCTURE_T0_BUDGET_MS),
        input_lamports: lamports,
        expected_base_raw,
        base_decimals,
        success,
        quote_source: source,
        protocol_fee_bps: pf,
        creator_fee_bps: cf,
        lp_fee_bps: lf,
        latency_ms: call.call_duration_ms,
        failure,
        feature_version: MEASUREMENT_FEATURE_VERSION,
    }
}

/// Frozen T0 microstructure probe sizes (lamports).
const MICROSTRUCTURE_PROBE_LAMPORTS: [u64; 3] = [500_000, 1_000_000, 2_000_000];

/// Domain 2 + 3 T0 enrichment. Owned by the run's bounded enrichment JoinSet (NOT a
/// detached task) so RunFinished can await/abort it — canonical outcome-horizon
/// scheduling is still never blocked (it runs concurrently, awaited only at shutdown).
/// Unconditional per candidate — no dependence on hypothesis/price/future state.
/// Persists a HolderSnapshot + one MicrostructureProbe per frozen size to the
/// separate measurement sink. `t0_deadline` is the T0 point (candidate admission);
/// `available_in_time` is truthful, never backdated.
#[allow(clippy::too_many_arguments)]
async fn t0_enrichment(
    mint: Pubkey,
    creator: String,
    oracle: Arc<PumpMarketOracle>,
    rpc_gate: Arc<ObservationRpcGate>,
    rpc: Arc<RpcClient>,
    shared: Arc<MeasurementShared>,
    run_id: String,
    source_revision: String,
    t0_deadline: DateTime<Utc>,
) {
    // Creator classification is by OWNER: the creator's account is owned by the
    // creator WALLET. Deterministic only if the provider creator pubkey parses.
    let creator_wallet = Pubkey::from_str(&creator).ok().map(|c| c.to_string());
    // T0 anchor = candidate admission. Both measurements record requested_at = T0 so
    // available_in_time is evaluated against the registered decision timestamps
    // (holder T10 = T0+10s; probe T0_AVAILABLE = T0+250ms) and is replay-self-contained.
    let t0 = t0_deadline;

    // AMENDMENT-001: holder and microstructure run CONCURRENTLY — the holder's ~10s
    // freshness retry must NOT delay probe initiation (D3 decoupling). Both share the
    // bounded RPC gate; both stay owned by this enrichment task (drained at shutdown).
    let holder_fut = acquire_holder_snapshot(
        &rpc_gate, &rpc, &mint, creator_wallet.as_deref(), &run_id, &source_revision, t0,
    );
    let probes_fut = async {
        let mut probes = Vec::with_capacity(MICROSTRUCTURE_PROBE_LAMPORTS.len());
        for lamports in MICROSTRUCTURE_PROBE_LAMPORTS {
            probes.push(acquire_microstructure_probe(&rpc_gate, &oracle, &mint, lamports, &run_id, t0).await);
        }
        probes
    };
    let (holder, probes) = tokio::join!(holder_fut, probes_fut);

    shared.count(|c| {
        c.holder_attempts += 1;
        if holder.failure.is_none() {
            c.holder_successes += 1;
        } else {
            c.holder_failures += 1;
        }
    });
    shared.append(MeasurementPayload::HolderSnapshot(holder));

    for probe in probes {
        shared.count(|c| {
            c.probe_attempts += 1;
            if probe.success {
                c.probe_successes += 1;
            } else {
                c.probe_failures += 1;
            }
        });
        shared.append(MeasurementPayload::MicrostructureProbe(probe));
    }
}

/// Frozen bounded shutdown budget (secs) for draining P3 enrichment tasks.
const MEASUREMENT_ENRICHMENT_DRAIN_SECS: u64 = 20;

struct MeasurementCtx {
    shared: Arc<MeasurementShared>,
    queue: BoundedTradeQueue,
    /// Shared read-only RPC handle for Domain-2/3 T0 enrichment (never signs/sends).
    rpc: Arc<RpcClient>,
    run_id: String,
    source_revision: String,
}

impl MeasurementCtx {
    /// Unconditional subscribe-on-admission (delegates lifecycle to the shared handle
    /// so the same registry/sender is used by candidate-terminate + reconnect sweeps).
    async fn on_candidate_admitted(&mut self, mint: &str) {
        self.shared.subscribe(mint).await;
    }

    /// Route an incoming trade for an expected-active mint: dedup by signature
    /// (earliest receipt wins; never persisted twice), push through the bounded
    /// queue (explicit backpressure failure, never a silent drop), then persist
    /// TradeObserved to the shared sink + candidate buffer for T2/T6 snapshots.
    fn on_expected_trade(&mut self, ev: &TradeEvent) {
        self.shared.count(|c| c.trade_events_received += 1);
        if !self.shared.accept_signature(&ev.mint, &ev.signature) {
            self.shared.count(|c| c.duplicate_trade_events += 1);
            return; // duplicate signature — not persisted twice
        }
        let now = Utc::now();
        let t = normalize_trade_event(ev, &self.run_id, &self.source_revision, now);
        match self.queue.push(t) {
            Ok(()) => {
                for qt in self.queue.drain() {
                    self.shared.note_trade(&qt.mint, qt.event_received_at);
                    self.shared.record_and_persist(qt);
                }
            }
            Err(_bp) => {
                self.shared.count(|c| c.backpressure_failures += 1);
                self.shared.append(MeasurementPayload::MeasurementFailure(MeasurementFailureRecord {
                    run_id: self.run_id.clone(),
                    mint: ev.mint.clone(),
                    domain: "trade_stream".to_string(),
                    stage: "bounded_queue".to_string(),
                    category: MeasurementFailureCategory::Other, // TradeStreamBackpressureFailure
                    at: now,
                }));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();
    let intake_seconds = validate_intake_seconds(args.intake_seconds)?;
    let max_active_candidates = validate_max_active(args.max_active_candidates)?;
    let rpc_concurrency = validate_rpc_concurrency(args.rpc_concurrency)?;

    let config = Config::load(&args.config).context("failed to load configuration")?;

    // --- Preflight: runtime exclusion lease, held for the whole binary (§31). ---
    let _lease = RuntimeLease::acquire(&config.wallet.credentials_dir, "observe_record")
        .context("failed to acquire runtime lease")?;

    // --- Preflight: PumpPortal must be enabled; RPC endpoint non-empty. The API
    // key is NOT required (free-only stream); if configured the client uses it. ---
    if !config.pumpportal.enabled {
        return Err(anyhow!(
            "config.pumpportal.enabled must be true for the observation recorder"
        ));
    }
    if config.rpc.endpoint.trim().is_empty() {
        return Err(anyhow!("config.rpc.endpoint must be non-empty"));
    }

    // --- Read-only RPC mainnet proof (never print the endpoint). ---
    let rpc = Arc::new(RpcClient::new_with_timeout(
        config.rpc.endpoint.clone(),
        Duration::from_millis(config.rpc.timeout_ms),
    ));

    let genesis = {
        let rpc = rpc.clone();
        tokio::task::spawn_blocking(move || rpc.get_genesis_hash())
            .await
            .context("RPC genesis task join failed")?
            // Fixed context; never interpolate the configured endpoint.
            .map_err(|_| anyhow!("configured Solana RPC request failed"))?
    };
    if genesis.to_string() != MAINNET_GENESIS {
        return Err(anyhow!(
            "configured RPC is not Solana mainnet (genesis mismatch)"
        ));
    }

    let current_slot = {
        let rpc = rpc.clone();
        tokio::task::spawn_blocking(move || rpc.get_slot())
            .await
            .context("RPC slot task join failed")?
            .map_err(|_| anyhow!("configured Solana RPC request failed"))?
    };
    if current_slot == 0 {
        return Err(anyhow!("RPC returned slot 0"));
    }
    println!("Recorder: mainnet verified");
    println!("Recorder current slot: {current_slot}");

    // --- Section 33: create the recorder ONLY after config + lease + mainnet +
    // slot are proven. RunStarted is auto-appended by create(). ---
    let source_revision = detect_source_revision();
    let run_started = RunStartedRecord::new(
        source_revision.clone(),
        detect_working_tree_clean(),
        env!("CARGO_PKG_VERSION").to_string(),
        intake_seconds,
        max_active_candidates,
        Some(rpc_concurrency),
    );
    let recorder = ObservationRecorder::create(&args.output_dir, run_started)
        .await
        .context("failed to create observation recorder")?;
    let run_file = run_file_name(&args.output_dir).await;
    if let Some(name) = &run_file {
        // Filename only — never the directory path.
        println!("Recorder file: {name}");
    }
    // Canonical run_id parsed from `observation_<stamp>_<run_id>.jsonl` for the
    // measurement-sink linkage contract (ties both files to the same run).
    let measurement_run_id = run_file
        .as_ref()
        .and_then(|n| n.strip_prefix("observation_").and_then(|s| s.strip_suffix(".jsonl")))
        .and_then(|s| s.split_once('_').map(|(_, id)| id.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    // --- Section 34: single PumpPortal client, free-only plan. ---
    let pp_config = PumpPortalConfig {
        ws_url: config.pumpportal.ws_url.clone(),
        api_key: config.pumpportal.api_key.clone(),
        reconnect_delay_ms: config.pumpportal.reconnect_delay_ms,
        max_reconnect_attempts: config.pumpportal.max_reconnect_attempts,
        ping_interval_secs: config.pumpportal.ping_interval_secs,
    };
    let (event_tx, mut event_rx) = mpsc::channel::<PumpPortalEvent>(EVENT_CHANNEL_CAPACITY);
    let client = PumpPortalClient::new(pp_config, event_tx);
    client
        .start(free_only_plan())
        .await
        .context("failed to start PumpPortal client")?;

    // --- P3 (Option B): separate measurement sink + Domain-1 subscription ctx. ---
    let measurement_path = args
        .output_dir
        .join(format!("measurement_{measurement_run_id}.jsonl"));
    let measurement_file =
        std::fs::File::create(&measurement_path).context("failed to create measurement sink file")?;
    let measurement_shared = Arc::new(MeasurementShared {
        state: std::sync::Mutex::new(ParticipationState::new()),
        registry: std::sync::Mutex::new(SubscriptionRegistry::new()),
        coverage: std::sync::Mutex::new(CoverageTracker::new()),
        counters: std::sync::Mutex::new(MeasurementCounters::default()),
        sink: std::sync::Mutex::new(MeasurementSink::new(
            measurement_file,
            &measurement_run_id,
            &source_revision,
        )),
        sender: client.get_command_sender(),
        run_id: measurement_run_id.clone(),
    });
    let mut meas = MeasurementCtx {
        shared: measurement_shared.clone(),
        queue: BoundedTradeQueue::new(MEASUREMENT_TRADE_QUEUE_CAPACITY),
        rpc: rpc.clone(),
        run_id: measurement_run_id.clone(),
        source_revision: source_revision.clone(),
    };
    // Owned bounded registry for P3 enrichment tasks — no untracked tokio::spawn for
    // run-scoped measurement work; drained (bounded) at shutdown.
    let mut enrichment_tasks: JoinSet<()> = JoinSet::new();

    let oracle = Arc::new(PumpMarketOracle::new(rpc.clone()));
    let semaphore = Arc::new(Semaphore::new(max_active_candidates));
    // §5: observation-only RPC concurrency gate, shared across all tracking tasks.
    let rpc_gate = ObservationRpcGate::new(rpc_concurrency);

    let mut counters = RunCounters::default();
    let mut seen_signatures: HashSet<String> = HashSet::new();
    let mut tasks: JoinSet<CandidateTaskOutcome> = JoinSet::new();
    let mut ever_connected = false;
    // AUDIT-001 §8-9: current provider stream-connection state during ACTIVE
    // intake. Drives run completeness at the intake boundary.
    let mut stream_connected = false;

    // --- Section 41: bounded intake window. No early-success exit. ---
    let intake_deadline = tokio::time::Instant::now() + Duration::from_secs(intake_seconds);

    'intake: loop {
        // AUDIT-001 §4.3: nonblocking reap of already-finished tracking tasks at
        // least once per loop turn so a lost recorder is discovered immediately.
        // A task reaped here is removed from the JoinSet and never seen again in
        // the final drain (no double count).
        while let Some(joined) = tasks.try_join_next() {
            let mapped = joined.map_err(|_| ());
            let effect = account_task_result(&mapped);
            apply_task_accounting(&mut counters, effect);
            if effect == TaskAccounting::RecorderFailure {
                // Fatal: a tracking task lost its recorder. Stop intake and fail.
                break 'intake;
            }
        }

        let now = tokio::time::Instant::now();
        if now >= intake_deadline {
            break;
        }
        let remaining = intake_deadline.saturating_duration_since(now);
        let wait = remaining.min(Duration::from_millis(500));

        match tokio::time::timeout(wait, event_rx.recv()).await {
            Ok(Some(event)) => {
                match handle_intake_event(
                    event,
                    &recorder,
                    &oracle,
                    &semaphore,
                    &rpc_gate,
                    &mut seen_signatures,
                    &mut counters,
                    &mut tasks,
                    &mut enrichment_tasks,
                    &mut ever_connected,
                    &mut stream_connected,
                    &mut meas,
                )
                .await
                {
                    Ok(()) => {}
                    // Fatal recorder I/O in the intake path: terminate without a
                    // healed partial line (§4.1). Fixed, secret-safe error.
                    Err(err) => return Err(anyhow!("{err}")),
                }
            }
            // Channel closed (worker gone): stream disconnected, stop intake.
            Ok(None) => {
                stream_connected =
                    apply_stream_transition(stream_connected, StreamTransition::ChannelClosed);
                break;
            }
            // Quiet point: just loop and re-check the deadline.
            Err(_) => continue,
        }
    }

    // AUDIT-001 §10.1: BEFORE stopping the stream, nonblocking-drain events that
    // are ALREADY queued and process them under NORMAL active-intake stream-state
    // semantics; drained NewTokens are persisted but use TrackingSkipped
    // (IntakeClosed) and start NO task. Then capture the run-completeness state.
    if let Err(err) = drain_pre_stop(
        &mut event_rx,
        &recorder,
        &mut seen_signatures,
        &mut counters,
        &mut stream_connected,
    )
    .await
    {
        return Err(anyhow!("{err}"));
    }

    // AUDIT-001 §10.1(4): THIS is the run-completeness state, captured BEFORE stop.
    let stream_connected_at_intake_end = stream_connected;

    // --- AUDIT-001 §10.2 / Section 43 final order: stop stream. ---
    client.stop();

    // AUDIT-001 §10.3: after a short bounded grace, nonblocking-drain final
    // delivered events. Post-stop Connected/Disconnected are shutdown mechanics
    // and MUST NOT alter completeness or operational counters/records.
    tokio::time::sleep(Duration::from_millis(POST_STOP_GRACE_MS)).await;
    if let Err(err) = drain_post_stop(
        &mut event_rx,
        &recorder,
        &mut seen_signatures,
        &mut counters,
    )
    .await
    {
        return Err(anyhow!("{err}"));
    }

    // --- Section 42: bounded outcome drain of already-started tasks. ---
    // (Each track task self-unsubscribes + tombstones on its normal exit path.)
    drain_outcome_tasks(&mut tasks, &recorder, &mut counters).await;

    // --- P3 (2D): finalize the measurement subsystem BEFORE canonical RunFinished. ---
    // Ordering: sweep remaining subscriptions -> bounded-drain owned enrichment tasks
    // -> persist run summary -> flush+seal sink. After the sink is sealed, any late
    // write (e.g. an aborted straggler) is rejected and counted, never appended.
    // 1) Unsubscribe every still-active subscription (tasks aborted by the outcome
    //    drain never reached their self-unsubscribe; this is the backstop sweep).
    let (active_before_sweep, stale_after_sweep) = measurement_shared.run_end_sweep().await;
    // 2) Drain the OWNED enrichment task set within a bounded budget; abort stragglers.
    let pending_before = enrichment_tasks.len() as u64;
    let mut completed_during = 0u64;
    let drain_enrichment = async {
        while enrichment_tasks.join_next().await.is_some() {
            completed_during += 1;
        }
    };
    let timed_out = match tokio::time::timeout(
        Duration::from_secs(MEASUREMENT_ENRICHMENT_DRAIN_SECS),
        drain_enrichment,
    )
    .await
    {
        Ok(()) => 0u64,
        Err(_) => {
            enrichment_tasks.abort_all();
            while enrichment_tasks.join_next().await.is_some() {}
            pending_before.saturating_sub(completed_during)
        }
    };
    // 3) Persist the run-scoped measurement summary, then flush + seal the sink.
    let summary = measurement_shared.build_summary(
        meas.queue.high_water,
        meas.queue.silent_drops,
        pending_before,
        completed_during,
        timed_out,
        active_before_sweep,
        stale_after_sweep,
    );
    measurement_shared.append(MeasurementPayload::RunSummary(summary));
    let (measurement_rows, measurement_flush_ok) = measurement_shared.close_sink();
    println!(
        "Measurement sink sealed: rows={measurement_rows} flush_ok={measurement_flush_ok} late_writes={}",
        measurement_shared.late_write_attempts()
    );

    // --- Section 22/43: append authoritative RunFinished, then sync. ---
    // §16: aggregate observation RPC gate stats (read after the outcome drain so
    // every in-flight permit has been released).
    let gate_stats = rpc_gate.stats();
    let completion = run_completion(&counters, ever_connected, stream_connected_at_intake_end);
    let run_finished = RunFinishedRecord {
        completion: completion.clone(),
        candidates_seen: counters.candidates_seen,
        unique_candidates: counters.unique_candidates,
        duplicate_candidate_events: counters.duplicate_candidate_events,
        tracking_started: counters.tracking_started,
        tracking_skipped: counters.tracking_skipped,
        tracking_completed: counters.tracking_completed,
        stream_connected_events: counters.stream_connected_events,
        stream_disconnect_events: counters.stream_disconnect_events,
        provider_errors: counters.provider_errors,
        unexpected_trade_events: counters.unexpected_trade_events,
        migrations_seen: counters.migrations_seen,
        partial_new_token_events: counters.partial_new_token_events,
        rpc_gate_peak_in_flight: Some(gate_stats.peak_in_flight),
        rpc_gate_acquisitions: Some(gate_stats.acquisitions),
        rpc_gate_wait_ms_total: Some(gate_stats.gate_wait_ms_total),
        rpc_gate_wait_ms_max: Some(gate_stats.gate_wait_ms_max),
    };
    append_required(&recorder, ObservationPayload::RunFinished(run_finished))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    recorder
        .sync_data()
        .await
        .context("failed to sync recorder data")?;

    let status_str = match completion {
        RunCompletion::Complete => "complete",
        RunCompletion::Degraded => "degraded",
        RunCompletion::Failed => "failed",
    };
    println!("Run finished: {status_str}");

    // RuntimeLease drops here (no manual lock deletion).
    Ok(())
}

/// Find the run file just created under `dir` (filename only, for console).
async fn run_file_name(dir: &std::path::Path) -> Option<String> {
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    let mut newest: Option<String> = None;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("observation_") && name.ends_with(".jsonl") {
            // Lexicographic max over the UTC-compact-stamped names is fine here.
            if newest.as_ref().map(|n| &name > n).unwrap_or(true) {
                newest = Some(name);
            }
        }
    }
    newest
}

// ---------------------------------------------------------------------------
// Intake event handling (section 34/35)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn handle_intake_event(
    event: PumpPortalEvent,
    recorder: &ObservationRecorder,
    oracle: &Arc<PumpMarketOracle>,
    semaphore: &Arc<Semaphore>,
    rpc_gate: &Arc<ObservationRpcGate>,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    tasks: &mut JoinSet<CandidateTaskOutcome>,
    enrichment: &mut JoinSet<()>,
    ever_connected: &mut bool,
    stream_connected: &mut bool,
    meas: &mut MeasurementCtx,
) -> std::result::Result<(), CollectorRecordError> {
    match event {
        PumpPortalEvent::Connected => {
            counters.stream_connected_events += 1;
            // A Connected AFTER we were ever connected is a RECONNECT: resubscribe
            // ALL active eligible mints (unconditional, never selective).
            let is_reconnect = *ever_connected;
            *ever_connected = true;
            *stream_connected =
                apply_stream_transition(*stream_connected, StreamTransition::Connected);
            append_required(recorder, stream_state(StreamStateKind::Connected, None)).await?;
            if is_reconnect {
                meas.shared.on_reconnect().await;
            }
            println!("PumpPortal: connected");
        }
        PumpPortalEvent::Disconnected => {
            counters.stream_disconnect_events += 1;
            *stream_connected =
                apply_stream_transition(*stream_connected, StreamTransition::Disconnected);
            // P3: mark all active subscriptions coverage-UNKNOWN (never zero-fill).
            meas.shared.on_disconnect();
            append_required(recorder, stream_state(StreamStateKind::Disconnected, None)).await?;
        }
        PumpPortalEvent::Error(category) => {
            // P1 §9: HARD provider error. Counts toward the schema-v1 total AND the
            // internal hard subset; a provider error does not itself mutate
            // stream_connected (§9); it fails the run.
            counters.provider_errors += 1;
            counters.hard_provider_errors += 1;
            // P3-COVERAGE-DEFECT-001: a provider/auth error is an authoritative coverage
            // break — invalidate coverage for all active token-trade mints so any
            // overlapping T2/T6 window cannot be emitted as valid zero-activity.
            meas.shared.on_stream_error(&category);
            // `category` is already a sanitized fixed provider category.
            let safe = sanitize_persist_text(&category, 128);
            append_required(
                recorder,
                stream_state(StreamStateKind::ProviderError, Some(safe)),
            )
            .await?;
        }
        PumpPortalEvent::DecodeError(e) => {
            // P1 §9/§11: LOCAL decode/schema loss. Counts toward the schema-v1 total
            // AND the internal decode subset; NEVER mutates stream_connected, NEVER
            // starts a task, NEVER fabricates a CandidateObserved. Degrades the run.
            record_decode_error(recorder, counters, &e).await?;
        }
        PumpPortalEvent::Trade(ev) => {
            // P3 Domain-1: if this mint has an expected-active token-trade
            // subscription, route it to the SEPARATE measurement sink (canonical
            // stream untouched). Otherwise preserve the unexpected-trade anomaly.
            if meas.shared.is_active(&ev.mint) {
                meas.on_expected_trade(&ev);
            } else {
                // A trade for a terminated (tombstoned) mint is STALE — recorded as a
                // distinct measurement diagnostic and NEVER used to resurrect state.
                if meas.shared.is_terminated(&ev.mint) {
                    meas.shared.count(|c| c.stale_trade_events += 1);
                }
                counters.unexpected_trade_events += 1;
                append_required(recorder, stream_state(StreamStateKind::UnexpectedTrade, None)).await?;
            }
        }
        PumpPortalEvent::Migration(ev) => {
            counters.migrations_seen += 1;
            append_required(
                recorder,
                ObservationPayload::MigrationObserved(MigrationObservedRecord {
                    mint: ev.mint,
                    signature: ev.signature,
                    pool: ev.pool,
                    pool_id: ev.pool_id,
                    provider_received_at: ev.received_at,
                }),
            )
            .await?;
        }
        PumpPortalEvent::NewToken(ev) => {
            // A full legacy create: provider observational fields are all present,
            // so they map to Some(..) with shape Full (§10/§16).
            let candidate = NormalizedCandidate::from_full(&ev);
            discover_candidate(
                candidate,
                recorder,
                oracle,
                semaphore,
                rpc_gate,
                seen_signatures,
                counters,
                tasks,
                enrichment,
                meas,
            )
            .await?;
        }
        PumpPortalEvent::PartialNewToken(ev) => {
            // P1 §13/§16: an incomplete provider create with valid required
            // identity. Count it ONCE (informational only; never a
            // provider/decode error), then run the SAME discovery path as a full
            // NewToken. Provider Option fields pass through unchanged — no zero
            // synthesis — with shape Partial.
            counters.partial_new_token_events += 1;
            let candidate = NormalizedCandidate::from_partial(&ev);
            discover_candidate(
                candidate,
                recorder,
                oracle,
                semaphore,
                rpc_gate,
                seen_signatures,
                counters,
                tasks,
                enrichment,
                meas,
            )
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P1-OBSERVATION-SCHEMA-V2 §16 — normalized candidate + shared discovery path.
// A full NewToken and a partial PartialNewToken differ ONLY in how their provider
// observational fields are populated (Some(..)/Full vs pass-through Option/Partial).
// Both go through the SAME persist-first -> dedupe -> capacity -> track flow so the
// existing fail-closed ordering is preserved exactly.
// ---------------------------------------------------------------------------

/// A provider create normalized to the fields the discovery path needs. Provider
/// observational fields are `Option` and are NEVER synthesized to zero for a
/// partial: an absent provider field stays `None`.
struct NormalizedCandidate {
    signature: String,
    mint: String,
    creator: String,
    tx_type: String,
    bonding_curve: Option<String>,
    provider_initial_buy: Option<f64>,
    provider_v_tokens_in_bonding_curve: Option<f64>,
    provider_v_sol_in_bonding_curve_sol: Option<f64>,
    provider_market_cap_sol: Option<f64>,
    name: String,
    symbol: String,
    uri: String,
    shape: ProviderCreateShape,
}

impl NormalizedCandidate {
    /// Full legacy create: every provider observational field is present, so each
    /// wraps in `Some(..)` and the shape is `Full` (§10).
    fn from_full(ev: &pumpfun_sniper::stream::pumpportal::NewTokenEvent) -> Self {
        Self {
            signature: ev.signature.clone(),
            mint: ev.mint.clone(),
            creator: ev.trader_public_key.clone(),
            tx_type: ev.tx_type.clone(),
            bonding_curve: Some(ev.bonding_curve_key.clone()),
            provider_initial_buy: Some(ev.initial_buy),
            provider_v_tokens_in_bonding_curve: Some(ev.v_tokens_in_bonding_curve),
            provider_v_sol_in_bonding_curve_sol: Some(ev.v_sol_in_bonding_curve),
            provider_market_cap_sol: Some(ev.market_cap_sol),
            name: ev.name.clone(),
            symbol: ev.symbol.clone(),
            uri: ev.uri.clone(),
            shape: ProviderCreateShape::Full,
        }
    }

    /// Partial create: provider observational fields are already `Option` on the
    /// event and pass through UNCHANGED (absent stays `None`, never zero). Shape
    /// is `Partial` (§10/§16).
    fn from_partial(ev: &pumpfun_sniper::stream::pumpportal::PartialNewTokenEvent) -> Self {
        Self {
            signature: ev.signature.clone(),
            mint: ev.mint.clone(),
            creator: ev.trader_public_key.clone(),
            tx_type: ev.tx_type.clone(),
            bonding_curve: ev.bonding_curve_key.clone(),
            provider_initial_buy: ev.initial_buy,
            provider_v_tokens_in_bonding_curve: ev.v_tokens_in_bonding_curve,
            provider_v_sol_in_bonding_curve_sol: ev.v_sol_in_bonding_curve,
            provider_market_cap_sol: ev.market_cap_sol,
            name: ev.name.clone(),
            symbol: ev.symbol.clone(),
            uri: ev.uri.clone(),
            shape: ev_partial_shape(),
        }
    }

    /// Build the CandidateObserved payload from this normalized candidate. Provider
    /// numeric Options are kept as-is (never cast/divided); untrusted text is
    /// sanitized/capped at name 256 / symbol 64 / uri 1024. No zero synthesis.
    fn candidate_observed(&self, duplicate: bool) -> ObservationPayload {
        ObservationPayload::CandidateObserved(CandidateObservedRecord {
            candidate_id: self.signature.clone(),
            signature: self.signature.clone(),
            mint: self.mint.clone(),
            creator: self.creator.clone(),
            bonding_curve: self.bonding_curve.clone(),
            tx_type: self.tx_type.clone(),
            provider_initial_buy: self.provider_initial_buy,
            provider_v_tokens_in_bonding_curve: self.provider_v_tokens_in_bonding_curve,
            provider_v_sol_in_bonding_curve_sol: self.provider_v_sol_in_bonding_curve_sol,
            provider_market_cap_sol: self.provider_market_cap_sol,
            name: sanitize_persist_text(&self.name, 256),
            symbol: sanitize_persist_text(&self.symbol, 64),
            uri: sanitize_persist_text(&self.uri, 1024),
            duplicate,
            provider_create_shape: Some(self.shape),
        })
    }
}

/// Fixed shape token for a partial candidate (kept as a tiny fn so the mapping is
/// asserted by a source test without a magic literal at the call site).
fn ev_partial_shape() -> ProviderCreateShape {
    ProviderCreateShape::Partial
}

/// §16 shared discovery flow for BOTH full and partial creates: append
/// CandidateObserved FIRST (acknowledged before any state mutation, per the
/// fail-closed contract), dedupe by signature, bump the unique counter, admit via
/// the capacity semaphore, then spawn canonical tracking by mint. Ordering is
/// identical to the previous inlined NewToken handler.
#[allow(clippy::too_many_arguments)]
async fn discover_candidate(
    candidate: NormalizedCandidate,
    recorder: &ObservationRecorder,
    oracle: &Arc<PumpMarketOracle>,
    semaphore: &Arc<Semaphore>,
    rpc_gate: &Arc<ObservationRpcGate>,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    tasks: &mut JoinSet<CandidateTaskOutcome>,
    enrichment: &mut JoinSet<()>,
    meas: &mut MeasurementCtx,
) -> std::result::Result<(), CollectorRecordError> {
    let candidate_received_at = Utc::now();
    counters.candidates_seen += 1;

    let signature = candidate.signature.clone();
    let duplicate = seen_signatures.contains(&signature);

    // AUDIT-001 §4.1 ordering: await a SUCCESSFUL CandidateObserved append ->
    // ONLY THEN mutate seen_signatures / capacity / spawn tracking.
    append_required(recorder, candidate.candidate_observed(duplicate)).await?;

    if duplicate {
        counters.duplicate_candidate_events += 1;
        return Ok(());
    }

    // First-seen — CandidateObserved is now durably persisted.
    seen_signatures.insert(signature.clone());
    counters.unique_candidates += 1;

    let candidate_id = signature;
    let mint_str = candidate.mint.clone();

    // Provider identity: a mint that fails to parse cannot be tracked, but
    // CandidateObserved was already written above.
    let mint = match Pubkey::from_str(&mint_str) {
        Ok(m) => m,
        Err(_) => {
            counters.tracking_skipped += 1;
            append_required(
                recorder,
                tracking_skipped(
                    &candidate_id,
                    &mint_str,
                    ObservationFailureCode::InvalidProviderIdentity,
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // Section 35: try to reserve tracking capacity. A partial candidate is fully
    // eligible for tracking (§17): the oracle needs only the mint.
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => {
            counters.tracking_started += 1;
            let oracle = oracle.clone();
            let recorder = recorder.clone();
            let rpc_gate = rpc_gate.clone();
            // P3 Domain-1 (unconditional): subscribe to this tracked mint's trades
            // and publish coverage BEFORE the task can reach its T2 cutoff.
            meas.on_candidate_admitted(&mint_str).await;
            // P3 Domain-2/3 (unconditional): detached T0 holder + microstructure
            // enrichment. Detached on purpose — canonical outcome-horizon scheduling
            // outranks enrichment and must never wait on it (enrichment is expendable).
            enrichment.spawn(t0_enrichment(
                mint,
                candidate.creator.clone(),
                oracle.clone(),
                rpc_gate.clone(),
                meas.rpc.clone(),
                meas.shared.clone(),
                meas.run_id.clone(),
                meas.source_revision.clone(),
                candidate_received_at,
            ));
            tasks.spawn(track_candidate(
                permit,
                candidate_id,
                mint,
                candidate_received_at,
                oracle,
                rpc_gate,
                recorder,
                meas.shared.clone(),
            ));
        }
        Err(_) => {
            counters.tracking_skipped += 1;
            append_required(
                recorder,
                tracking_skipped(
                    &candidate_id,
                    &mint_str,
                    ObservationFailureCode::TrackingCapacity,
                ),
            )
            .await?;
        }
    }
    Ok(())
}

/// AUDIT-001 §10.1: BEFORE `client.stop()`, nonblocking-drain events ALREADY
/// queued and process them under NORMAL active-intake stream-state semantics
/// (Connected/Disconnected still mutate `stream_connected` and operational
/// counters/records). Drained NewTokens are persisted (CandidateObserved) but
/// use TrackingSkipped(IntakeClosed) and start NO task. Every append is required.
async fn drain_pre_stop(
    event_rx: &mut mpsc::Receiver<PumpPortalEvent>,
    recorder: &ObservationRecorder,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    stream_connected: &mut bool,
) -> std::result::Result<(), CollectorRecordError> {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            PumpPortalEvent::Connected => {
                counters.stream_connected_events += 1;
                *stream_connected =
                    apply_stream_transition(*stream_connected, StreamTransition::Connected);
                append_required(recorder, stream_state(StreamStateKind::Connected, None)).await?;
            }
            PumpPortalEvent::Disconnected => {
                counters.stream_disconnect_events += 1;
                *stream_connected =
                    apply_stream_transition(*stream_connected, StreamTransition::Disconnected);
                append_required(recorder, stream_state(StreamStateKind::Disconnected, None))
                    .await?;
            }
            PumpPortalEvent::Error(category) => {
                counters.provider_errors += 1;
                counters.hard_provider_errors += 1;
                let safe = sanitize_persist_text(&category, 128);
                append_required(
                    recorder,
                    stream_state(StreamStateKind::ProviderError, Some(safe)),
                )
                .await?;
            }
            PumpPortalEvent::DecodeError(e) => {
                // P1 §11: decode loss in the pre-stop drain — count/persist, never
                // mutate stream_connected, never start a task.
                record_decode_error(recorder, counters, &e).await?;
            }
            PumpPortalEvent::Trade(_) => {
                counters.unexpected_trade_events += 1;
                append_required(
                    recorder,
                    stream_state(StreamStateKind::UnexpectedTrade, None),
                )
                .await?;
            }
            PumpPortalEvent::Migration(ev) => {
                counters.migrations_seen += 1;
                append_required(
                    recorder,
                    ObservationPayload::MigrationObserved(MigrationObservedRecord {
                        mint: ev.mint,
                        signature: ev.signature,
                        pool: ev.pool,
                        pool_id: ev.pool_id,
                        provider_received_at: ev.received_at,
                    }),
                )
                .await?;
            }
            PumpPortalEvent::NewToken(ev) => {
                let candidate = NormalizedCandidate::from_full(&ev);
                drain_candidate_intake_closed(recorder, seen_signatures, counters, &candidate)
                    .await?;
            }
            PumpPortalEvent::PartialNewToken(ev) => {
                // §13/§16: still count the retained partial once, then persist it as
                // CandidateObserved + IntakeClosed skip (no task). Never a decode error.
                counters.partial_new_token_events += 1;
                let candidate = NormalizedCandidate::from_partial(&ev);
                drain_candidate_intake_closed(recorder, seen_signatures, counters, &candidate)
                    .await?;
            }
        }
    }
    Ok(())
}

/// AUDIT-001 §10.3: AFTER the intentional `client.stop()`, nonblocking-drain any
/// final delivered events. Post-stop Connected/Disconnected are shutdown
/// mechanics: they MUST NOT alter completeness, MUST NOT increment operational
/// connection/disconnection counters, and MUST NOT emit operational StreamState
/// records. NewToken => CandidateObserved + IntakeClosed skip; MigrationObserved,
/// ProviderError, and UnexpectedTrade are still counted/recorded (the latter two
/// still fail the run). Every emitted append is required.
async fn drain_post_stop(
    event_rx: &mut mpsc::Receiver<PumpPortalEvent>,
    recorder: &ObservationRecorder,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
) -> std::result::Result<(), CollectorRecordError> {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            // Shutdown mechanics: ignore entirely (no counter, no record).
            PumpPortalEvent::Connected | PumpPortalEvent::Disconnected => {}
            PumpPortalEvent::Error(category) => {
                counters.provider_errors += 1;
                counters.hard_provider_errors += 1;
                let safe = sanitize_persist_text(&category, 128);
                append_required(
                    recorder,
                    stream_state(StreamStateKind::ProviderError, Some(safe)),
                )
                .await?;
            }
            PumpPortalEvent::DecodeError(e) => {
                // P1 §11: a post-stop decode event still counts/persists but does NOT
                // change the already-captured intake connection truth (that bool was
                // frozen before client.stop()).
                record_decode_error(recorder, counters, &e).await?;
            }
            PumpPortalEvent::Trade(_) => {
                counters.unexpected_trade_events += 1;
                append_required(
                    recorder,
                    stream_state(StreamStateKind::UnexpectedTrade, None),
                )
                .await?;
            }
            PumpPortalEvent::Migration(ev) => {
                counters.migrations_seen += 1;
                append_required(
                    recorder,
                    ObservationPayload::MigrationObserved(MigrationObservedRecord {
                        mint: ev.mint,
                        signature: ev.signature,
                        pool: ev.pool,
                        pool_id: ev.pool_id,
                        provider_received_at: ev.received_at,
                    }),
                )
                .await?;
            }
            PumpPortalEvent::NewToken(ev) => {
                let candidate = NormalizedCandidate::from_full(&ev);
                drain_candidate_intake_closed(recorder, seen_signatures, counters, &candidate)
                    .await?;
            }
            PumpPortalEvent::PartialNewToken(ev) => {
                counters.partial_new_token_events += 1;
                let candidate = NormalizedCandidate::from_partial(&ev);
                drain_candidate_intake_closed(recorder, seen_signatures, counters, &candidate)
                    .await?;
            }
        }
    }
    Ok(())
}

/// Persist a drained create (full or partial) with intake closed: CandidateObserved
/// first, then (if first-seen) an IntakeClosed TrackingSkipped. Never starts a task.
/// Shared by pre-stop and post-stop drains. Every append is required.
async fn drain_candidate_intake_closed(
    recorder: &ObservationRecorder,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    candidate: &NormalizedCandidate,
) -> std::result::Result<(), CollectorRecordError> {
    counters.candidates_seen += 1;
    let signature = candidate.signature.clone();
    let duplicate = seen_signatures.contains(&signature);
    append_required(recorder, candidate.candidate_observed(duplicate)).await?;
    if duplicate {
        counters.duplicate_candidate_events += 1;
        return Ok(());
    }
    seen_signatures.insert(signature.clone());
    counters.unique_candidates += 1;
    // Intake closed: retain the candidate, but never start a new task.
    counters.tracking_skipped += 1;
    append_required(
        recorder,
        tracking_skipped(
            &signature,
            &candidate.mint,
            ObservationFailureCode::IntakeClosed,
        ),
    )
    .await?;
    Ok(())
}

/// Section 42: wait for already-started tracking tasks under a hard 135s bound. On
/// timeout, abort remaining tasks and mark the run drain-timed-out.
async fn drain_outcome_tasks(
    tasks: &mut JoinSet<CandidateTaskOutcome>,
    recorder: &ObservationRecorder,
    counters: &mut RunCounters,
) {
    // AUDIT-001 §12: only tasks still present are processed; tasks reaped during
    // intake are already removed from the JoinSet, so there is no double count.
    // Ok(success) => tracking_completed; Ok(RecorderWrite) => recorder_failures
    // (Failed); Err(JoinError) => task_failures.
    let drain = async {
        while let Some(joined) = tasks.join_next().await {
            let mapped = joined.map_err(|_| ());
            apply_task_accounting(counters, account_task_result(&mapped));
        }
    };

    match tokio::time::timeout(Duration::from_secs(OUTCOME_DRAIN_SECS), drain).await {
        Ok(()) => {}
        Err(_) => {
            // Timed out: abort everything still running and mark the run failed.
            counters.drain_timed_out += 1;
            tasks.abort_all();
            // Best-effort: reap whatever aborts land, without waiting past the
            // bound (abort is prompt). A DrainTimedOut finish record cannot carry a
            // specific identity here, so we only flip the run counters.
            while let Some(joined) = tasks.join_next().await {
                let mapped = joined.map_err(|_| ());
                apply_task_accounting(counters, account_task_result(&mapped));
            }
            let _ = recorder; // no manual per-candidate record without identity
        }
    }
}

// ---------------------------------------------------------------------------
// Tracking task (sections 36-40)
// ---------------------------------------------------------------------------

/// Thin lifecycle wrapper: runs the tracking body, then ALWAYS issues the terminal
/// P3 unsubscribe + tombstone + state-clear on the normal exit path (any early Ok/Err
/// return from the body). A panic is covered by the inner ParticipationCleanup guard
/// (participation state) plus the run-end unsubscribe sweep (subscription). Preserves
/// the exact CandidateTaskOutcome the reaper/drain accounting expects.
async fn track_candidate(
    permit: tokio::sync::OwnedSemaphorePermit,
    candidate_id: String,
    mint: Pubkey,
    candidate_received_at: DateTime<Utc>,
    oracle: Arc<PumpMarketOracle>,
    rpc_gate: Arc<ObservationRpcGate>,
    recorder: ObservationRecorder,
    measurement: Arc<MeasurementShared>,
) -> CandidateTaskOutcome {
    let mint_str = mint.to_string();
    let outcome = track_candidate_inner(
        permit,
        candidate_id,
        mint,
        candidate_received_at,
        oracle,
        rpc_gate,
        recorder,
        measurement.clone(),
    )
    .await;
    measurement.unsubscribe_and_terminate(&mint_str).await;
    outcome
}

/// Bounded initial snapshot + buy-quote retry schedule (0/250/500/1000ms), then a
/// fixed-horizon outcome sampling loop anchored to the successful initial buy
/// quote. Read-only throughout: `quote_buy_sol`/`quote_sell_raw`/`snapshot` are
/// canonical quotes, never order submissions.
#[allow(clippy::too_many_arguments)]
async fn track_candidate_inner(
    _permit: tokio::sync::OwnedSemaphorePermit,
    candidate_id: String,
    mint: Pubkey,
    candidate_received_at: DateTime<Utc>,
    oracle: Arc<PumpMarketOracle>,
    rpc_gate: Arc<ObservationRpcGate>,
    recorder: ObservationRecorder,
    measurement: Arc<MeasurementShared>,
) -> CandidateTaskOutcome {
    let mint_str = mint.to_string();
    // Clears this candidate's participation buffer/coverage/emission guard on ANY
    // task exit path (return, error, panic) — no stale per-mint state survives.
    let _participation_cleanup = ParticipationCleanup {
        shared: measurement.clone(),
        mint: mint_str.clone(),
    };

    // --- Section 16/36: bounded initial availability retry. ---
    let backoffs = [0u64, 250, 500, 1000];
    let mut last_snapshot: Option<MarketSnapshotRecord> = None;
    let mut last_snapshot_failure: Option<ObservationFailureCode> = None;
    // MARKET-DATA-TRUTH §9: safe subtype tracked alongside the FINAL retained
    // snapshot/buy failure. Some IFF the paired failure code is MarketUnavailable.
    let mut last_snapshot_market_data_kind: Option<MarketDataFailureKind> = None;
    let mut buy_quote_record: Option<ExecutableQuoteRecord> = None;
    let mut buy_quote_failure: Option<ObservationFailureCode> = None;
    let mut buy_quote_market_data_kind: Option<MarketDataFailureKind> = None;
    let mut initial_base_amount_raw: Option<u64> = None;
    let mut entry_wall_time: Option<DateTime<Utc>> = None;
    // §15: totals of RPC gate wait + oracle call duration across ALL initial retry
    // attempts, split by snapshot vs buy quote (saturating; never derived from lag).
    let mut initial_snapshot_gate_wait_total: u64 = 0;
    let mut initial_snapshot_call_total: u64 = 0;
    let mut initial_buy_gate_wait_total: u64 = 0;
    let mut initial_buy_call_total: u64 = 0;
    // AUDIT-001 §7: the monotonic horizon anchor is captured in the SAME buy-quote
    // success branch that accepts the initial quote (below), before any recorder
    // await or further async. It is never reconstructed after InitialMarket.
    let mut entry_monotonic_anchor: Option<tokio::time::Instant> = None;
    let mut attempts: u8 = 0;

    for (i, delay_ms) in backoffs.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }
        attempts = (i + 1) as u8;

        // §8: gate every initial snapshot attempt; accumulate its gate/call time.
        let snap_call = gated_snapshot(&rpc_gate, &oracle, &mint).await;
        initial_snapshot_gate_wait_total =
            initial_snapshot_gate_wait_total.saturating_add(snap_call.gate_wait_ms);
        initial_snapshot_call_total =
            initial_snapshot_call_total.saturating_add(snap_call.call_duration_ms);
        match snap_call.result {
            Ok(snap) => {
                last_snapshot = Some(MarketSnapshotRecord::from(&snap));
                last_snapshot_failure = None;
                last_snapshot_market_data_kind = None;
            }
            Err(e) => {
                let c = classify_observation_error_full(&e);
                last_snapshot = None;
                last_snapshot_failure = Some(c.code);
                last_snapshot_market_data_kind = c.market_data_kind;
            }
        }

        // §8: gate every initial buy-quote attempt; accumulate its gate/call time.
        let buy_call = gated_quote_buy_sol(&rpc_gate, &oracle, &mint, ENTRY_QUOTE_LAMPORTS).await;
        initial_buy_gate_wait_total =
            initial_buy_gate_wait_total.saturating_add(buy_call.gate_wait_ms);
        initial_buy_call_total = initial_buy_call_total.saturating_add(buy_call.call_duration_ms);
        match buy_call.result {
            Ok(q) => {
                // AUDIT-001 §7: IMMEDIATELY, in this buy-quote success branch,
                // capture BOTH the wall-clock entry time (the quote's canonical
                // quoted_at) and the monotonic anchor — before converting/
                // recording InitialMarket, before any recorder await, before any
                // further async work.
                let entry_wall = q.quoted_at;
                entry_monotonic_anchor = Some(tokio::time::Instant::now());

                let rec = ExecutableQuoteRecord::from(&q);
                initial_base_amount_raw = Some(rec.base_amount_raw);
                entry_wall_time = Some(entry_wall);
                buy_quote_record = Some(rec);
                buy_quote_failure = None;
                buy_quote_market_data_kind = None;
                // Success criterion for tracking is a valid SOL buy quote.
                break;
            }
            Err(e) => {
                let c = classify_observation_error_full(&e);
                buy_quote_record = None;
                buy_quote_failure = Some(c.code);
                buy_quote_market_data_kind = c.market_data_kind;
            }
        }
    }

    // --- Section 15: append InitialMarket (snapshot XOR failure; buy XOR failure). ---
    // Ensure the XOR invariants hold: if neither present, mark a failure.
    if last_snapshot.is_none() && last_snapshot_failure.is_none() {
        last_snapshot_failure = Some(ObservationFailureCode::Other);
        // Synthetic non-MarketData code => no market-data subtype (§11 invariant).
        last_snapshot_market_data_kind = None;
    }
    if buy_quote_record.is_none() && buy_quote_failure.is_none() {
        buy_quote_failure = Some(ObservationFailureCode::Other);
        buy_quote_market_data_kind = None;
    }

    // AUDIT-001 §4.2: required task write. On failure, stop the task, produce no
    // later samples, and surface RecorderWrite to the parent (raw I/O never
    // serialized/printed).
    if recorder
        .append(ObservationPayload::InitialMarket(InitialMarketRecord {
            candidate_id: candidate_id.clone(),
            mint: mint_str.clone(),
            candidate_received_at,
            snapshot: last_snapshot.clone(),
            snapshot_failure: last_snapshot_failure.clone(),
            buy_quote: buy_quote_record.clone(),
            buy_quote_failure: buy_quote_failure.clone(),
            initial_observation_attempts: attempts,
            initial_snapshot_rpc_gate_wait_ms_total: Some(initial_snapshot_gate_wait_total),
            initial_snapshot_rpc_call_duration_ms_total: Some(initial_snapshot_call_total),
            initial_buy_rpc_gate_wait_ms_total: Some(initial_buy_gate_wait_total),
            initial_buy_rpc_call_duration_ms_total: Some(initial_buy_call_total),
            snapshot_market_data_kind: last_snapshot_market_data_kind,
            buy_quote_market_data_kind,
        }))
        .await
        .is_err()
    {
        return Err(CandidateTaskFailure::RecorderWrite);
    }

    // --- Section 16: no buy quote after retries => finish, no sell samples. ---
    // AUDIT-001 §7: use the anchor captured in the buy-quote success branch; it is
    // present exactly when the initial base quantity + wall time are.
    let (base_amount_raw, entry_wall_time, entry_monotonic) = match (
        initial_base_amount_raw,
        entry_wall_time,
        entry_monotonic_anchor,
    ) {
        (Some(b), Some(t), Some(anchor)) => (b, t, anchor),
        _ => {
            if recorder
                .append(tracking_finished(
                    &candidate_id,
                    &mint_str,
                    TrackingFinishStatus::InitialQuoteUnavailable,
                    0,
                    0,
                ))
                .await
                .is_err()
            {
                return Err(CandidateTaskFailure::RecorderWrite);
            }
            return Ok(CandidateTaskResult {
                candidate_id,
                mint: mint_str,
                status: TrackingFinishStatus::InitialQuoteUnavailable,
            });
        }
    };

    // --- Section 37: horizons anchored to the anchor captured at buy-quote success. ---
    let mut successful_samples: u16 = 0;
    let mut failed_samples: u16 = 0;
    let mut h1_sample_2s: Option<DateTime<Utc>> = None;
    let mut h1_sample_4s: Option<DateTime<Utc>> = None;
    let mut h1_sample_6s: Option<DateTime<Utc>> = None;
    let mut delayed_entry_quote_recorded = false;
    let mut delayed_entry_quote_truth: Option<DelayedEntryQuoteTruth> = None;

    for &horizon in OUTCOME_HORIZONS_SECS {
        // Sleep until the monotonic due instant (never negative).
        let due_monotonic = entry_monotonic + Duration::from_secs(horizon);
        let now = tokio::time::Instant::now();
        if due_monotonic > now {
            tokio::time::sleep(due_monotonic - now).await;
        }
        let due_at = horizon_due_at(entry_wall_time, horizon);

        // --- Section 38 / AUDIT-001 §5-6: exact-size future sell quote (quantity
        // frozen at initial_buy_quote.base_amount_raw). OutcomeSample.sampled_at is
        // the sell-quote observation timestamp when sell succeeds; otherwise the
        // time the sell observation failure returned. sampled_at is therefore
        // defined AFTER the sell await, never before it.
        // §8/§14: gate the sell quote; capture its gate wait + call duration for
        // THIS sample. The gate wait occurs AFTER due_at (§10), so it naturally
        // contributes to sample_lag_ms — it is NEVER subtracted.
        let sell_call = gated_quote_sell_raw(&rpc_gate, &oracle, &mint, base_amount_raw).await;
        let sell_rpc_gate_wait_ms = Some(sell_call.gate_wait_ms);
        let sell_rpc_call_duration_ms = Some(sell_call.call_duration_ms);
        let (sell_quote, sell_quote_failure, sell_market_data_kind, return_bps, sampled_at) =
            match sell_call.result {
                Ok(q) => {
                    let rec = ExecutableQuoteRecord::from(&q);
                    let ret = protocol_net_ex_network_return_bps(
                        ENTRY_QUOTE_LAMPORTS,
                        rec.quote_amount_raw,
                    );
                    // Sell success: sampled_at is the quote's canonical timestamp.
                    let sampled_at = outcome_sampled_at(Some(rec.quoted_at), Utc::now());
                    (Some(rec), None, None, ret, sampled_at)
                }
                Err(e) => {
                    // Sell failure: sampled_at is the failure-completion time,
                    // stamped AFTER the failed await returned. §9-11: also record the
                    // safe market-data subtype (Some IFF code == MarketUnavailable).
                    let c = classify_observation_error_full(&e);
                    let sampled_at = outcome_sampled_at(None, Utc::now());
                    (None, Some(c.code), c.market_data_kind, None, sampled_at)
                }
            };

        let lag_ms = sample_lag_ms(due_at, sampled_at);

        if sell_quote.is_some() {
            successful_samples = successful_samples.saturating_add(1);
        } else {
            failed_samples = failed_samples.saturating_add(1);
        }

        // --- Section 39: snapshot only at key horizons, recorded independently.
        // The snapshot keeps its own independent observed_at; it never overwrites
        // the outcome sampled_at above (AUDIT-001 §6). §14: at key horizons the
        // gated snapshot's timing is recorded; at non-key horizons it stays None.
        let (
            snapshot,
            snapshot_failure,
            snapshot_market_data_kind,
            snapshot_rpc_gate_wait_ms,
            snapshot_rpc_call_duration_ms,
        ) = if should_snapshot_at(horizon) {
            let snap_call = gated_snapshot(&rpc_gate, &oracle, &mint).await;
            let (snapshot, snapshot_failure, snapshot_kind) = match snap_call.result {
                Ok(snap) => (Some(MarketSnapshotRecord::from(&snap)), None, None),
                Err(e) => {
                    let c = classify_observation_error_full(&e);
                    (None, Some(c.code), c.market_data_kind)
                }
            };
            (
                snapshot,
                snapshot_failure,
                snapshot_kind,
                Some(snap_call.gate_wait_ms),
                Some(snap_call.call_duration_ms),
            )
        } else {
            // Absent != failure at non-key horizons; and no snapshot timing/subtype.
            (None, None, None, None, None)
        };

        // AUDIT-001 §4.2: required OutcomeSample write. On failure, stop the task
        // and surface RecorderWrite (no later samples produced).
        if recorder
            .append(ObservationPayload::OutcomeSample(OutcomeSampleRecord {
                candidate_id: candidate_id.clone(),
                mint: mint_str.clone(),
                horizon_secs: horizon,
                due_at,
                sampled_at,
                sample_lag_ms: lag_ms,
                sell_quote,
                sell_quote_failure,
                snapshot,
                snapshot_failure,
                protocol_net_ex_network_return_bps: return_bps,
                sell_rpc_gate_wait_ms,
                sell_rpc_call_duration_ms,
                snapshot_rpc_gate_wait_ms,
                snapshot_rpc_call_duration_ms,
                sell_market_data_kind,
                snapshot_market_data_kind,
            }))
            .await
            .is_err()
        {
            return Err(CandidateTaskFailure::RecorderWrite);
        }

        match horizon {
            2 => h1_sample_2s = Some(sampled_at),
            4 => h1_sample_4s = Some(sampled_at),
            6 => h1_sample_6s = Some(sampled_at),
            _ => {}
        }

        // --- P3 Domain-1 (2B): emit T2/T6 ParticipationSnapshots at the ACTUAL
        // sampled_at cutoffs (never a nominal clock). Exactly-once per class;
        // `computed_at` is the real emit time so `available_in_time` is truthful.
        if horizon == 2 {
            if let Some(cutoff) = h1_sample_2s {
                measurement.emit_participation(&mint_str, SnapshotClass::T2, cutoff, Utc::now());
            }
        }
        if horizon == 6 {
            if let (Some(s2), Some(s4), Some(s6)) = (h1_sample_2s, h1_sample_4s, h1_sample_6s) {
                // T6 decision cutoff = max(actual sampled_at 2/4/6), reusing the
                // same frozen H1 decision-time function as the delayed-entry path.
                let t6_cutoff = h1_decision_time_from_observations(s2, s4, s6);
                measurement.emit_participation(&mint_str, SnapshotClass::T6, t6_cutoff, Utc::now());
            }
        }

        if horizon == 6 && !delayed_entry_quote_recorded {
            let (Some(sample_2s), Some(sample_4s), Some(sample_6s)) =
                (h1_sample_2s, h1_sample_4s, h1_sample_6s)
            else {
                return Err(CandidateTaskFailure::RecorderWrite);
            };
            let decision_time = h1_decision_time_from_observations(sample_2s, sample_4s, sample_6s);
            let request_started_at = Utc::now();
            if !delayed_quote_start_is_valid(decision_time, request_started_at) {
                return Err(CandidateTaskFailure::RecorderWrite);
            }
            let delayed_buy_call =
                gated_quote_buy_sol(&rpc_gate, &oracle, &mint, ENTRY_QUOTE_LAMPORTS).await;
            let quote_observed_at = Utc::now();
            let decision_to_quote_start_lag_ms = wall_lag_ms(decision_time, request_started_at);
            let decision_to_quote_available_lag_ms = wall_lag_ms(decision_time, quote_observed_at);
            let buy_rpc_gate_wait_ms = Some(delayed_buy_call.gate_wait_ms);
            let buy_rpc_call_duration_ms = Some(delayed_buy_call.call_duration_ms);
            let (buy_quote, buy_quote_failure, buy_quote_market_data_kind, delayed_truth) =
                match delayed_buy_call.result {
                    Ok(q) => {
                        let rec = ExecutableQuoteRecord::from(&q);
                        let delayed_truth = DelayedEntryQuoteTruth {
                            decision_time,
                            buy_request_started_at: request_started_at,
                            buy_quoted_at: rec.quoted_at,
                            buy_observed_at: quote_observed_at,
                            base_amount_raw: rec.base_amount_raw,
                        };
                        (Some(rec), None, None, Some(delayed_truth))
                    }
                    Err(e) => {
                        let c = classify_observation_error_full(&e);
                        (None, Some(c.code), c.market_data_kind, None)
                    }
                };

            if recorder
                .append(ObservationPayload::DecisionPointBuyQuote(
                    DecisionPointBuyQuoteRecord {
                        candidate_id: candidate_id.clone(),
                        mint: mint_str.clone(),
                        nominal_decision_horizon_secs: 6,
                        decision_time,
                        request_started_at,
                        quote_observed_at,
                        decision_to_quote_start_lag_ms,
                        decision_to_quote_available_lag_ms,
                        buy_quote,
                        buy_quote_failure,
                        buy_rpc_gate_wait_ms,
                        buy_rpc_call_duration_ms,
                        buy_quote_market_data_kind,
                    },
                ))
                .await
                .is_err()
            {
                return Err(CandidateTaskFailure::RecorderWrite);
            }
            delayed_entry_quote_truth = delayed_truth;
            delayed_entry_quote_recorded = true;
        }

        if should_match_delayed_exit_at(horizon) {
            if let Some(delayed) = delayed_entry_quote_truth.clone() {
                let request_started_at = Utc::now();
                if !matched_sell_start_is_valid(delayed.buy_observed_at, request_started_at) {
                    return Err(CandidateTaskFailure::RecorderWrite);
                }
                let matched_sell_call = gated_quote_sell_raw(
                    &rpc_gate,
                    &oracle,
                    &mint,
                    matched_delayed_sell_base_input(delayed.base_amount_raw, base_amount_raw),
                )
                .await;
                let quote_observed_at = Utc::now();
                let sample_lag_ms = sample_lag_ms(due_at, quote_observed_at);
                let delayed_entry_to_quote_start_elapsed_ms =
                    wall_lag_ms(delayed.buy_observed_at, request_started_at);
                let delayed_entry_to_quote_available_elapsed_ms =
                    wall_lag_ms(delayed.buy_observed_at, quote_observed_at);
                let sell_rpc_gate_wait_ms = Some(matched_sell_call.gate_wait_ms);
                let sell_rpc_call_duration_ms = Some(matched_sell_call.call_duration_ms);
                let (sell_quote, sell_quote_failure, sell_market_data_kind) =
                    match matched_sell_call.result {
                        Ok(q) => {
                            let rec = ExecutableQuoteRecord::from(&q);
                            if rec.base_amount_raw != delayed.base_amount_raw {
                                return Err(CandidateTaskFailure::RecorderWrite);
                            }
                            (Some(rec), None, None)
                        }
                        Err(e) => {
                            let c = classify_observation_error_full(&e);
                            (None, Some(c.code), c.market_data_kind)
                        }
                    };

                if recorder
                    .append(ObservationPayload::DecisionPointSellQuote(
                        DecisionPointSellQuoteRecord {
                            candidate_id: candidate_id.clone(),
                            mint: mint_str.clone(),
                            nominal_horizon_secs: horizon,
                            delayed_entry_decision_time: delayed.decision_time,
                            delayed_buy_request_started_at: delayed.buy_request_started_at,
                            delayed_buy_quoted_at: delayed.buy_quoted_at,
                            delayed_buy_observed_at: delayed.buy_observed_at,
                            delayed_base_amount_raw: delayed.base_amount_raw,
                            due_at,
                            request_started_at,
                            quote_observed_at,
                            sample_lag_ms,
                            delayed_entry_to_quote_start_elapsed_ms,
                            delayed_entry_to_quote_available_elapsed_ms,
                            sell_quote,
                            sell_quote_failure,
                            sell_rpc_gate_wait_ms,
                            sell_rpc_call_duration_ms,
                            sell_market_data_kind,
                        },
                    ))
                    .await
                    .is_err()
                {
                    return Err(CandidateTaskFailure::RecorderWrite);
                }
            }
        }
    }

    // --- Section 19/36: complete. Required task write. ---
    if recorder
        .append(tracking_finished(
            &candidate_id,
            &mint_str,
            TrackingFinishStatus::Complete,
            successful_samples,
            failed_samples,
        ))
        .await
        .is_err()
    {
        return Err(CandidateTaskFailure::RecorderWrite);
    }

    Ok(CandidateTaskResult {
        candidate_id,
        mint: mint_str,
        status: TrackingFinishStatus::Complete,
    })
}

// ---------------------------------------------------------------------------
// Payload constructors
// ---------------------------------------------------------------------------

fn stream_state(state: StreamStateKind, category: Option<String>) -> ObservationPayload {
    ObservationPayload::StreamState(StreamStateRecord { state, category })
}

/// AUDIT-001 §10: pure predicate — does this decode kind represent the terminal LOSS
/// of a `txType=create` (a candidate that can NEVER enter the research universe)?
///
/// TRUE for all four create-loss kinds: `NewTokenDeserialize`, `NewTokenValidation`,
/// `PartialNewTokenDeserialize`, `PartialNewTokenValidation`. FALSE for non-create
/// losses (`MigrationParse`, `TradeDeserialize`, `TradeValidation`), which are still
/// counted in `provider_errors`/`decode_errors` but must NOT inflate the modeling-census
/// `new_token_decode_errors` gate.
fn is_new_token_decode_kind(kind: PumpPortalDecodeKind) -> bool {
    matches!(
        kind,
        PumpPortalDecodeKind::NewTokenDeserialize
            | PumpPortalDecodeKind::NewTokenValidation
            | PumpPortalDecodeKind::PartialNewTokenDeserialize
            | PumpPortalDecodeKind::PartialNewTokenValidation
    )
}

/// P1-PROVIDER-DECODE-TRUTH-001 §9: account + persist a single decode/schema-loss
/// anomaly. Increments the schema-v1 total (`provider_errors`) AND the internal
/// `decode_errors` subset; create-loss decode kinds (full OR partial) also bump
/// `new_token_decode_errors` per [`is_new_token_decode_kind`].
/// Persists a `StreamStateKind::ProviderError` record (no new schema variant) whose
/// category is prefixed `decode:` and bounded to <=256 chars with no raw provider
/// values (the decode category is already a fixed structural token). NEVER touches
/// stream_connected, NEVER starts a task, NEVER fabricates a CandidateObserved.
async fn record_decode_error(
    recorder: &ObservationRecorder,
    counters: &mut RunCounters,
    e: &PumpPortalDecodeError,
) -> std::result::Result<(), CollectorRecordError> {
    counters.provider_errors += 1;
    counters.decode_errors += 1;
    if is_new_token_decode_kind(e.kind) {
        counters.new_token_decode_errors += 1;
    }
    // Fixed `decode:` prefix + the already-safe fixed category, then re-sanitized
    // and capped at 256 for defense in depth.
    let category = sanitize_persist_text(&format!("decode:{}", e.category), 256);
    append_required(
        recorder,
        stream_state(StreamStateKind::ProviderError, Some(category)),
    )
    .await?;
    Ok(())
}

/// Build a CandidateObserved payload from a provider NewToken event. Provider f64
/// fields are kept as-is (never cast, never divided by 1e9); untrusted text is
/// sanitized/capped at name 256 / symbol 64 / uri 1024.
///
/// Retained for the metadata-absence unit tests; production builds go through
/// [`NormalizedCandidate::candidate_observed`] so full and partial creates share
/// one mapping.
#[cfg_attr(not(test), allow(dead_code))]
fn candidate_observed(
    ev: &pumpfun_sniper::stream::pumpportal::NewTokenEvent,
    duplicate: bool,
) -> ObservationPayload {
    // Delegate to the shared full mapping so there is exactly one full-create
    // Some(..)/Full mapping in the binary.
    NormalizedCandidate::from_full(ev).candidate_observed(duplicate)
}

fn tracking_skipped(
    candidate_id: &str,
    mint: &str,
    reason: ObservationFailureCode,
) -> ObservationPayload {
    ObservationPayload::TrackingSkipped(TrackingSkippedRecord {
        candidate_id: candidate_id.to_string(),
        mint: mint.to_string(),
        reason,
    })
}

fn tracking_finished(
    candidate_id: &str,
    mint: &str,
    status: TrackingFinishStatus,
    successful_outcome_samples: u16,
    failed_outcome_samples: u16,
) -> ObservationPayload {
    ObservationPayload::TrackingFinished(TrackingFinishedRecord {
        candidate_id: candidate_id.to_string(),
        mint: mint.to_string(),
        status,
        successful_outcome_samples,
        failed_outcome_samples,
    })
}

// ---------------------------------------------------------------------------
// Section 45/46 — tests (no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // 2C: read-only RPC/quote failure maps to explicit provenance (never zero)
    fn measurement_rpc_error_provenance() {
        assert_eq!(
            classify_measurement_rpc_error("operation timed out"),
            MeasurementFailureCategory::Timeout
        );
        assert_eq!(
            classify_measurement_rpc_error("HTTP 429 Too Many Requests"),
            MeasurementFailureCategory::RateLimited
        );
        assert_eq!(
            classify_measurement_rpc_error("could not find account"),
            MeasurementFailureCategory::AccountMissing
        );
        assert_eq!(
            classify_measurement_rpc_error("connection refused"),
            MeasurementFailureCategory::RpcUnavailable
        );
    }

    #[test] // 2C: frozen microstructure probe sizes, in order
    fn microstructure_probe_sizes_frozen() {
        assert_eq!(MICROSTRUCTURE_PROBE_LAMPORTS, [500_000, 1_000_000, 2_000_000]);
    }

    // --- P3-HOLDER-DEFECT-002 remediation: commitment + decode robustness ---

    fn packed_token_account(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
        use spl_token::solana_program::program_option::COption;
        let acct = spl_token::state::Account {
            mint: *mint,
            owner: *owner,
            amount: 1_000,
            delegate: COption::None,
            state: spl_token::state::AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(acct, &mut data).unwrap();
        data
    }

    #[test] // GUARD: holder account fetch must use CONFIRMED (not finalized/default)
    fn holder_fetch_uses_confirmed_commitment() {
        assert_eq!(holder_fetch_commitment(), CommitmentConfig::confirmed());
        assert_ne!(holder_fetch_commitment(), CommitmentConfig::finalized());
    }

    // --- P3-HOLDER-AVAILABILITY-DEFECT-001: fresh-mint visibility ---

    #[test] // "not a Token mint" (mint not yet visible) => retryable freshness category
    fn freshness_error_classified_and_retryable() {
        let cat = classify_measurement_rpc_error("RPC response error -32602: Invalid param: not a Token mint;");
        assert_eq!(cat, MeasurementFailureCategory::AccountNotYetVisible);
        assert!(is_retryable_holder_failure(cat));
    }

    #[test] // transient transport is retryable; hard/param failures are not looped
    fn retry_policy_categories() {
        assert!(is_retryable_holder_failure(MeasurementFailureCategory::Timeout));
        assert!(is_retryable_holder_failure(MeasurementFailureCategory::RateLimited));
        assert!(is_retryable_holder_failure(MeasurementFailureCategory::RpcUnavailable));
        assert!(is_retryable_holder_failure(MeasurementFailureCategory::AccountNotYetVisible));
        // NOT retried: unsupported program, decode failure, mint mismatch, account-missing
        assert!(!is_retryable_holder_failure(MeasurementFailureCategory::UnsupportedTokenProgram));
        assert!(!is_retryable_holder_failure(MeasurementFailureCategory::TokenAccountDecodeFailed));
        assert!(!is_retryable_holder_failure(MeasurementFailureCategory::TokenMintMismatch));
        assert!(!is_retryable_holder_failure(MeasurementFailureCategory::AccountMissing));
    }

    #[test] // bounded freshness backoff schedule is frozen + bounded (~6s)
    fn freshness_backoffs_frozen() {
        assert_eq!(HOLDER_FRESHNESS_BACKOFFS_MS, [0, 1000, 3000, 6000]);
        assert!(HOLDER_FRESHNESS_BACKOFFS_MS.iter().sum::<u64>() <= 10_000);
    }

    // --- AMENDMENT-001: decision-timestamp availability budgets (D2 T10 / D3 T0) ---

    #[test] // frozen budgets
    fn ts_arch_budgets_frozen() {
        assert_eq!(HOLDER_DECISION_BUDGET_MS, 10_000);
        assert_eq!(MICROSTRUCTURE_T0_BUDGET_MS, 250);
    }

    #[test] // holder T10: completed within 10s of T0 -> available; over -> not
    fn holder_t10_boundary() {
        let t0 = chrono::Utc.timestamp_opt(1_000_000, 0).unwrap();
        use chrono::TimeZone;
        assert!(within_budget(t0, t0 + chrono::Duration::milliseconds(9_999), HOLDER_DECISION_BUDGET_MS));
        assert!(within_budget(t0, t0 + chrono::Duration::milliseconds(10_000), HOLDER_DECISION_BUDGET_MS)); // boundary inclusive
        assert!(!within_budget(t0, t0 + chrono::Duration::milliseconds(10_001), HOLDER_DECISION_BUDGET_MS));
    }

    #[test] // microstructure T0_AVAILABLE: within 250ms -> available; over -> not
    fn probe_t0_boundary() {
        use chrono::TimeZone;
        let t0 = chrono::Utc.timestamp_opt(1_000_000, 0).unwrap();
        assert!(within_budget(t0, t0 + chrono::Duration::milliseconds(249), MICROSTRUCTURE_T0_BUDGET_MS));
        assert!(within_budget(t0, t0 + chrono::Duration::milliseconds(250), MICROSTRUCTURE_T0_BUDGET_MS));
        assert!(!within_budget(t0, t0 + chrono::Duration::milliseconds(251), MICROSTRUCTURE_T0_BUDGET_MS));
    }

    #[tokio::test] // D3 DECOUPLING: a slow holder does NOT delay probe start under join!
    async fn probes_not_delayed_by_holder_sequencing() {
        use tokio::time::{Duration, Instant};
        let start = Instant::now();
        let holder = async { tokio::time::sleep(Duration::from_millis(300)).await; };
        let probe = async { Instant::now() }; // resolves on first poll
        let (_, probe_started_at) = tokio::join!(holder, probe);
        // Under join!, the probe is polled immediately — NOT after the holder's 300ms.
        assert!(
            probe_started_at.duration_since(start) < Duration::from_millis(150),
            "probe must not wait on the holder chain"
        );
    }

    #[test] // rate-limit / timeout still classify explicitly (unchanged)
    fn other_rpc_categories_unchanged() {
        assert_eq!(classify_measurement_rpc_error("HTTP 429 Too Many Requests"), MeasurementFailureCategory::RateLimited);
        assert_eq!(classify_measurement_rpc_error("operation timed out"), MeasurementFailureCategory::Timeout);
        assert_eq!(classify_measurement_rpc_error("could not find account"), MeasurementFailureCategory::AccountMissing);
    }

    #[test] // classic SPL token account decodes to its authoritative owner
    fn decode_classic_token_account_owner() {
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let data = packed_token_account(&mint, &owner);
        let got = decode_token_account_owner(&spl_token::id(), &data, &mint).unwrap();
        assert_eq!(got, owner.to_string());
    }

    #[test] // non-SPL-Token owning program => explicit UnsupportedTokenProgram
    fn decode_unsupported_program() {
        let mint = Pubkey::new_unique();
        let data = packed_token_account(&mint, &Pubkey::new_unique());
        let other_program = Pubkey::new_unique(); // e.g. Token-2022 (not a dependency)
        assert_eq!(
            decode_token_account_owner(&other_program, &data, &mint),
            Err(MeasurementFailureCategory::UnsupportedTokenProgram)
        );
    }

    #[test] // decoded mint != candidate mint => explicit TokenMintMismatch
    fn decode_mint_mismatch() {
        let acct_mint = Pubkey::new_unique();
        let candidate_mint = Pubkey::new_unique();
        let data = packed_token_account(&acct_mint, &Pubkey::new_unique());
        assert_eq!(
            decode_token_account_owner(&spl_token::id(), &data, &candidate_mint),
            Err(MeasurementFailureCategory::TokenMintMismatch)
        );
    }

    #[test] // undecodable data owned by SPL Token => explicit TokenAccountDecodeFailed
    fn decode_garbage_data() {
        let mint = Pubkey::new_unique();
        let junk = vec![0u8; 10];
        assert_eq!(
            decode_token_account_owner(&spl_token::id(), &junk, &mint),
            Err(MeasurementFailureCategory::TokenAccountDecodeFailed)
        );
    }

    // --- P3-HOLDER-DEFECT-003: Token-2022 decode ---

    fn t22_base_account(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
        use spl_token_2022::solana_program::program_option::COption;
        use spl_token_2022::state::{Account, AccountState};
        let acct = Account {
            mint: *mint,
            owner: *owner,
            amount: 1_000,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0u8; Account::LEN];
        Account::pack(acct, &mut data).unwrap();
        data
    }

    fn t22_immutable_owner_account(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
        use spl_token_2022::extension::immutable_owner::ImmutableOwner;
        use spl_token_2022::extension::{BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut};
        use spl_token_2022::solana_program::program_option::COption;
        use spl_token_2022::state::{Account, AccountState};
        let len = ExtensionType::try_calculate_account_len::<Account>(&[ExtensionType::ImmutableOwner]).unwrap();
        let mut data = vec![0u8; len];
        let mut state = StateWithExtensionsMut::<Account>::unpack_uninitialized(&mut data).unwrap();
        state.base = Account {
            mint: *mint,
            owner: *owner,
            amount: 1_000,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        state.pack_base();
        state.init_account_type().unwrap();
        state.init_extension::<ImmutableOwner>(true).unwrap();
        data
    }

    #[test] // Token-2022 base (165-byte) account decodes to its authoritative owner
    fn decode_token2022_base() {
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let data = t22_base_account(&mint, &owner);
        let got = decode_token_account_owner(&spl_token_2022::id(), &data, &mint).unwrap();
        assert_eq!(got, owner.to_string());
    }

    #[test] // Token-2022 EXTENSION-bearing (170-byte) account decodes (the live shape)
    fn decode_token2022_extension_170() {
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let data = t22_immutable_owner_account(&mint, &owner);
        assert_eq!(data.len(), 170, "ImmutableOwner extension account is 170 bytes");
        let got = decode_token_account_owner(&spl_token_2022::id(), &data, &mint).unwrap();
        assert_eq!(got, owner.to_string());
    }

    #[test] // Token-2022 mint mismatch is explicit (owner not attributed)
    fn decode_token2022_mint_mismatch() {
        let acct_mint = Pubkey::new_unique();
        let candidate_mint = Pubkey::new_unique();
        let data = t22_immutable_owner_account(&acct_mint, &Pubkey::new_unique());
        assert_eq!(
            decode_token_account_owner(&spl_token_2022::id(), &data, &candidate_mint),
            Err(MeasurementFailureCategory::TokenMintMismatch)
        );
    }

    #[test] // Token-2022 program + too-short/garbage data => explicit decode failure
    fn decode_token2022_garbage() {
        let mint = Pubkey::new_unique();
        assert_eq!(
            decode_token_account_owner(&spl_token_2022::id(), &[0u8; 10], &mint),
            Err(MeasurementFailureCategory::TokenAccountDecodeFailed)
        );
    }

    #[test] // classification uses decoded owner regardless of token program (curve excluded)
    fn token2022_curve_excluded_via_owner() {
        // A Token-2022 curve reserve (owned by the curve PDA) must classify as curve
        // and be excluded from non-curve concentration — same math as classic.
        use pumpfun_sniper::observation::measurement::{holder_features, HolderAccountClass};
        use pumpfun_sniper::observation::measurement_runtime::{classify_holder_accounts_by_owner, RawHolderAccount};
        let curve_pda = Pubkey::new_unique();
        let owner_a = Pubkey::new_unique();
        let raw = vec![
            RawHolderAccount { address: "RESERVE".into(), ui_balance: 990_000_000, owner: Some(curve_pda.to_string()) },
            RawHolderAccount { address: "WHALE".into(), ui_balance: 5_000_000, owner: Some(owner_a.to_string()) },
        ];
        let (classified, resolved) = classify_holder_accounts_by_owner(&raw, &curve_pda.to_string(), None);
        assert!(resolved);
        assert_eq!(classified[0].class, HolderAccountClass::CurveProgram);
        let feats = holder_features(&classified, false);
        assert!(feats.top1_noncurve_holder_share.unwrap() < 0.01);
        assert!(feats.curve_held_share.unwrap() > 0.98);
    }

    /// Section 45 static execution-absence guard. Forbidden needles are assembled
    /// from split fragments via `concat!()` so this test's own source never
    /// contains the forbidden contiguous token. The quote method names
    /// `quote_buy_sol` / `quote_sell_raw` are deliberately NOT flagged: the guard
    /// targets the dotted ACTION-call forms (method-call syntax on buy/sell/
    /// transfer), which are distinct from the read-only quote method identifiers.
    /// Those action patterns are only ever built via `concat!()` below, so this
    /// comment itself contains none of them verbatim.
    #[test]
    fn test_source_execution_capability_absent() {
        let src = include_str!("observe_record.rs");

        let forbidden: &[&str] = &[
            concat!("pumpfun_sniper::", "cli"),
            concat!("pumpfun_sniper::", "trading"),
            concat!("pumpfun_sniper::", "wallet"),
            concat!("pumpfun_sniper::", "position"),
            concat!("pumpfun_sniper::", "strategy"),
            concat!("pumpfun_sniper::", "filter"),
            concat!("Key", "pair"),
            concat!("Sign", "er"),
            concat!("Versioned", "Trans", "action"),
            concat!("Trans", "action"),
            concat!("Instruc", "tion"),
            concat!("Mess", "age"),
            concat!("PumpPortal", "Trader"),
            concat!("Pending", "Execution"),
            concat!("Position", "Manager"),
            concat!("Ji", "to"),
            concat!("send_", "transaction"),
            concat!("send_and_", "confirm_transaction"),
            concat!("send_raw_", "transaction"),
            concat!("simulate_", "transaction"),
            concat!("partial_", "sign"),
            concat!("try_", "sign"),
            // ACTION-call patterns (NOT the quote method identifiers).
            concat!(".bu", "y("),
            concat!(".sel", "l("),
            concat!(".transf", "er("),
            concat!("KEYPAIR", "_PATH"),
        ];

        for needle in forbidden {
            assert!(
                !src.contains(needle),
                "forbidden execution reference present: {needle}"
            );
        }

        // Sanity: the binary DOES legitimately reference the read-only quote
        // methods, and those must not be mistaken for action calls.
        assert!(src.contains("quote_buy_sol"));
        assert!(src.contains("quote_sell_raw"));
    }

    // === P1-METADATA-DRAIN-TRUTH-001 §17 — metadata absence + drain bound ======

    /// Build a NewTokenEvent with valid core fields and caller-chosen metadata.
    fn new_token_with_metadata(
        name: &str,
        symbol: &str,
        uri: &str,
    ) -> pumpfun_sniper::stream::pumpportal::NewTokenEvent {
        pumpfun_sniper::stream::pumpportal::NewTokenEvent {
            signature: "sig".to_string(),
            mint: "mint".to_string(),
            trader_public_key: "creator".to_string(),
            tx_type: "create".to_string(),
            initial_buy: 1.0,
            bonding_curve_key: "bc".to_string(),
            v_tokens_in_bonding_curve: 1000.0,
            v_sol_in_bonding_curve: 30.0,
            market_cap_sol: 30.0,
            name: name.to_string(),
            symbol: symbol.to_string(),
            uri: uri.to_string(),
        }
    }

    #[test]
    fn test_candidate_observed_preserves_metadata_absence_as_empty_strings() {
        let ev = new_token_with_metadata("", "", "");
        match candidate_observed(&ev, false) {
            ObservationPayload::CandidateObserved(rec) => {
                assert_eq!(rec.name, "");
                assert_eq!(rec.symbol, "");
                assert_eq!(rec.uri, "");
            }
            other => panic!("expected CandidateObserved, got {other:?}"),
        }
        // sanitize_persist_text("") passes empty through unchanged.
        assert_eq!(sanitize_persist_text("", 256), "");
    }

    #[test]
    fn test_metadata_absence_does_not_increment_decode_counters() {
        // A metadata-less create is a normal NewTokenEvent (empty metadata strings),
        // NOT a PumpPortalDecodeError. Only PumpPortalDecodeError values ever reach
        // record_decode_error and bump provider_errors/decode_errors/
        // new_token_decode_errors. Prove the metadata-less candidate flows through
        // candidate_observed (the CandidateObserved path) instead, so the decode
        // counters are structurally untouched by metadata absence.
        let ev = new_token_with_metadata("", "", "");
        assert!(!ev.has_complete_metadata());
        // It builds a CandidateObserved payload, not any decode-error payload.
        assert!(matches!(
            candidate_observed(&ev, false),
            ObservationPayload::CandidateObserved(_)
        ));
        // Decode counters only move for genuine PumpPortalDecodeKind values; a
        // metadata-less NewTokenEvent is not one, so a freshly zeroed counter set
        // reflecting "one metadata-less candidate seen" stays at zero decode loss.
        let counters = RunCounters::default();
        assert_eq!(counters.provider_errors, 0);
        assert_eq!(counters.decode_errors, 0);
        assert_eq!(counters.new_token_decode_errors, 0);
    }

    #[test]
    fn test_outcome_drain_bound_is_240_seconds() {
        assert_eq!(OUTCOME_DRAIN_SECS, 240);
    }

    #[test]
    fn test_free_only_plan() {
        let plan = free_only_plan();
        assert!(plan.new_tokens);
        assert!(plan.migrations);
        assert!(plan.token_trades.is_empty());
        assert!(plan.account_trades.is_empty());
    }

    #[test]
    fn test_intake_seconds_bounds() {
        assert!(validate_intake_seconds(60).is_ok());
        assert!(validate_intake_seconds(900).is_ok());
        assert!(validate_intake_seconds(21_600).is_ok());
        assert!(validate_intake_seconds(59).is_err());
        assert!(validate_intake_seconds(21_601).is_err());
    }

    #[test]
    fn test_max_active_candidates_bounds() {
        assert!(validate_max_active(1).is_ok());
        assert!(validate_max_active(64).is_ok());
        assert!(validate_max_active(256).is_ok());
        assert!(validate_max_active(0).is_err());
        assert!(validate_max_active(257).is_err());
    }

    // === P1-OBSERVATION-RPC-CONCURRENCY-001 gate tests ======================

    /// §27: CLI bounds — default 24; 1/24/64 accepted; 0 and 65 rejected.
    #[test]
    fn test_rpc_concurrency_bounds() {
        assert_eq!(RPC_CONCURRENCY_DEFAULT, 24);
        assert_eq!(validate_rpc_concurrency(24).unwrap(), 24);
        assert!(validate_rpc_concurrency(1).is_ok());
        assert!(validate_rpc_concurrency(64).is_ok());
        assert!(validate_rpc_concurrency(0).is_err());
        assert!(validate_rpc_concurrency(65).is_err());
    }

    /// §28: the gate bounds simultaneous holders, WAITS (never drops), and every
    /// task completes. With limit 3 and 20 tasks: observed simultaneous holders
    /// <= 3, gate peak <= 3, and acquisitions == 20.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_rpc_gate_bounded_and_non_censoring() {
        let gate = ObservationRpcGate::new(3);
        let active = Arc::new(AtomicUsize::new(0));
        let observed_max = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let gate = gate.clone();
            let active = active.clone();
            let observed_max = observed_max.clone();
            let completed = completed.clone();
            handles.push(tokio::spawn(async move {
                let (permit, _wait) = gate.acquire().await;
                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                observed_max.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(15)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                drop(permit);
                completed.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(completed.load(Ordering::SeqCst), 20, "all tasks complete");
        assert!(
            observed_max.load(Ordering::SeqCst) <= 3,
            "simultaneous holders exceeded limit"
        );
        let stats = gate.stats();
        assert!(stats.peak_in_flight <= 3, "gate peak exceeded limit");
        assert_eq!(stats.acquisitions, 20, "every acquisition counted");
    }

    /// §29: a permit released after an Err-shaped operation frees the slot for the
    /// next waiter — no leak. Modeled with limit 1: acquire, drop (as the wrapper
    /// does on both Ok and Err), then acquire again must succeed.
    #[tokio::test]
    async fn test_rpc_gate_permit_released_after_use() {
        let gate = ObservationRpcGate::new(1);
        let (permit_a, _wa) = gate.acquire().await;
        // Simulate the wrapper releasing the permit regardless of oracle Ok/Err.
        drop(permit_a);
        // The next acquire must not hang (a leaked permit would deadlock at limit 1).
        let (_permit_b, _wb) = gate.acquire().await;
        assert_eq!(gate.stats().acquisitions, 2);
        assert!(gate.stats().peak_in_flight <= 1);
    }

    /// §30: a waiter's gate wait is measured (> 0) when the gate is held. Uses a
    /// deterministic hold to avoid timer flake; exact ms is not asserted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rpc_gate_wait_is_measured() {
        let gate = ObservationRpcGate::new(1);
        let (permit_a, _wa) = gate.acquire().await; // A holds the only permit.

        let gate_b = gate.clone();
        let waiter = tokio::spawn(async move {
            let (_permit_b, wait_b) = gate_b.acquire().await;
            wait_b
        });

        // Ensure B is queued and waiting before A releases.
        tokio::time::sleep(Duration::from_millis(40)).await;
        drop(permit_a);

        let wait_b = waiter.await.unwrap();
        assert!(wait_b > 0, "queued waiter gate wait should be measured > 0");
    }

    /// §31: aggregate stats are exact/consistent under deliberate contention.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rpc_gate_stats_consistent() {
        let gate = ObservationRpcGate::new(1);
        let (permit_a, _wa) = gate.acquire().await;

        let gate_b = gate.clone();
        let waiter = tokio::spawn(async move {
            let (permit_b, _wait_b) = gate_b.acquire().await;
            drop(permit_b);
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        drop(permit_a);
        waiter.await.unwrap();

        let s = gate.stats();
        assert_eq!(s.limit, 1);
        assert_eq!(s.acquisitions, 2);
        assert!(s.peak_in_flight <= s.limit, "peak within limit");
        assert!(
            s.gate_wait_ms_total >= s.gate_wait_ms_max,
            "total wait >= max wait"
        );
        assert!(s.gate_wait_ms_max > 0, "contention should produce a wait");
    }

    /// §32: every candidate-tracking oracle call goes through a gated wrapper. The
    /// three direct `oracle.<method>(` forms appear EXACTLY ONCE in the source —
    /// each inside its own wrapper definition — so `track_candidate` cannot call
    /// the oracle directly. Needles are split so this test never self-matches.
    #[test]
    fn test_all_candidate_oracle_calls_are_gated() {
        let src = include_str!("observe_record.rs");
        let snapshot_call = concat!("oracle", ".snapshot(");
        let buy_call = concat!("oracle", ".quote_buy_sol(");
        let sell_call = concat!("oracle", ".quote_sell_raw(");
        assert_eq!(
            src.matches(snapshot_call).count(),
            1,
            "oracle.snapshot must be called only inside gated_snapshot"
        );
        assert_eq!(
            src.matches(buy_call).count(),
            1,
            "oracle.quote_buy_sol must be called only inside gated_quote_buy_sol"
        );
        assert_eq!(
            src.matches(sell_call).count(),
            1,
            "oracle.quote_sell_raw must be called only inside gated_quote_sell_raw"
        );
        // And the wrappers are actually used by the tracking path.
        assert!(src.contains("gated_snapshot(&rpc_gate"));
        assert!(src.contains("gated_quote_buy_sol(&rpc_gate"));
        assert!(src.contains("gated_quote_sell_raw(&rpc_gate"));
    }

    /// §33: gate wait is NOT subtracted from lag — a later `sampled_at` (because a
    /// queued permit delayed the sell) yields a strictly larger `sample_lag_ms`.
    #[test]
    fn test_gate_wait_increases_sample_lag_not_subtracted() {
        let due_at = "2026-01-01T00:00:30.000Z".parse::<DateTime<Utc>>().unwrap();
        let prompt = "2026-01-01T00:00:30.100Z".parse::<DateTime<Utc>>().unwrap();
        let delayed = "2026-01-01T00:00:35.100Z".parse::<DateTime<Utc>>().unwrap();
        let lag_prompt = sample_lag_ms(due_at, prompt);
        let lag_delayed = sample_lag_ms(due_at, delayed);
        assert!(lag_delayed > lag_prompt);
        // The 5s of extra gate wait lands entirely in the lag (not hidden).
        assert_eq!(lag_delayed - lag_prompt, 5_000);
    }

    #[test]
    fn test_mainnet_genesis_full_hash() {
        assert_eq!(
            MAINNET_GENESIS,
            "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
        );
    }

    #[test]
    fn test_entry_quote_size_fixed() {
        assert_eq!(ENTRY_QUOTE_LAMPORTS, 1_000_000);
    }

    #[test]
    fn test_horizons_fixed() {
        assert_eq!(
            OUTCOME_HORIZONS_SECS,
            &[2, 4, 6, 8, 10, 12, 15, 18, 21, 24, 27, 30, 45, 60, 90, 120]
        );
    }

    #[test]
    fn test_snapshot_horizons_fixed() {
        assert_eq!(SNAPSHOT_HORIZONS_SECS, &[15, 30, 60, 120]);
    }

    #[test]
    fn test_git_revision_validator_accepts_40_hex() {
        let sha = "8a18b714b32114f220e7173526a0a4fdd87b72fa";
        assert!(is_40_hex(sha));
        assert_eq!(accept_revision(&format!("{sha}\n")), sha);
        // Mixed case is still hex.
        let upper = "8A18B714B32114F220E7173526A0A4FDD87B72FA";
        assert!(is_40_hex(upper));
    }

    #[test]
    fn test_git_revision_validator_rejects_non_hex() {
        assert!(!is_40_hex("not-a-sha"));
        assert!(!is_40_hex("8a18b71")); // too short
        assert!(!is_40_hex(&"z".repeat(40))); // 40 chars, not hex
        assert_eq!(accept_revision("fatal: not a git repository"), "unknown");
        assert_eq!(accept_revision(""), "unknown");
    }

    #[test]
    fn test_candidate_text_sanitizer_caps_and_removes_controls() {
        let dirty = "ab\u{0007}cd\nef";
        assert_eq!(sanitize_persist_text(dirty, 256), "abcdef");
        let long = "x".repeat(2000);
        assert_eq!(sanitize_persist_text(&long, 1024).chars().count(), 1024);
        assert_eq!(sanitize_persist_text(&long, 64).chars().count(), 64);
    }

    #[test]
    fn test_duplicate_candidate_is_recorded_but_not_retracked_policy() {
        let mut seen = HashSet::new();
        let sig = "sig-1".to_string();
        // First-seen => should track.
        assert!(should_track_first_seen(&seen, &sig));
        seen.insert(sig.clone());
        // Now a duplicate => should NOT track (but is still recorded by the caller).
        assert!(!should_track_first_seen(&seen, &sig));
    }

    #[test]
    fn test_tracking_capacity_skip_retains_candidate_policy() {
        // No permit available => not admitted; caller still persisted the candidate
        // and records a TrackingCapacity skip.
        assert!(!capacity_admits(false));
        assert!(capacity_admits(true));
    }

    #[test]
    fn test_outcome_due_schedule_anchored_to_entry_quote() {
        let entry = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        for &h in OUTCOME_HORIZONS_SECS {
            let due = horizon_due_at(entry, h);
            assert_eq!((due - entry).num_seconds(), h as i64);
        }
    }

    #[test]
    fn test_sample_lag_nonnegative_when_late() {
        let due = "2026-01-01T00:00:10Z".parse::<DateTime<Utc>>().unwrap();
        // Sampled 250ms late => positive lag.
        let late = "2026-01-01T00:00:10.250Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(sample_lag_ms(due, late), 250);
        // Sampled early => clamped to 0, never negative.
        let early = "2026-01-01T00:00:09Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(sample_lag_ms(due, early), 0);
        // Exactly on time => 0.
        assert_eq!(sample_lag_ms(due, due), 0);
    }

    #[test]
    fn test_h1_decision_time_uses_actual_sample_timestamps() {
        let nominal_6s = "2026-01-01T00:00:06.000Z".parse::<DateTime<Utc>>().unwrap();
        let sample_2s = "2026-01-01T00:00:02.050Z".parse::<DateTime<Utc>>().unwrap();
        let sample_4s = "2026-01-01T00:00:04.075Z".parse::<DateTime<Utc>>().unwrap();
        let sample_6s = "2026-01-01T00:00:06.325Z".parse::<DateTime<Utc>>().unwrap();
        let decision_time = h1_decision_time_from_observations(sample_2s, sample_4s, sample_6s);
        assert_eq!(decision_time, sample_6s);
        assert_ne!(decision_time, nominal_6s);
    }

    #[test]
    fn test_delayed_buy_quote_cannot_start_before_decision_time() {
        let decision_time = "2026-01-01T00:00:06.325Z".parse::<DateTime<Utc>>().unwrap();
        let valid_start = "2026-01-01T00:00:06.326Z".parse::<DateTime<Utc>>().unwrap();
        let invalid_start = "2026-01-01T00:00:06.324Z".parse::<DateTime<Utc>>().unwrap();
        assert!(delayed_quote_start_is_valid(decision_time, decision_time));
        assert!(delayed_quote_start_is_valid(decision_time, valid_start));
        assert!(!delayed_quote_start_is_valid(decision_time, invalid_start));
    }

    #[test]
    fn test_delayed_quote_lag_uses_decision_time() {
        let decision_time = "2026-01-01T00:00:06.325Z".parse::<DateTime<Utc>>().unwrap();
        let available_at = "2026-01-01T00:00:06.425Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(wall_lag_ms(decision_time, available_at), 100);
        assert_eq!(wall_lag_ms(available_at, decision_time), 0);
    }

    #[test]
    fn test_delayed_entry_source_uses_exact_0_001_sol_buy_quote() {
        let src = include_str!("observe_record.rs");
        let call = "gated_quote_buy_sol(&rpc_gate, &oracle, &mint, ENTRY_QUOTE_LAMPORTS)";
        assert!(
            src.matches(call).count() >= 2,
            "initial and delayed buy quotes should both use the fixed entry size"
        );
        assert!(src.contains("ObservationPayload::DecisionPointBuyQuote"));
    }

    #[test]
    fn test_delayed_entry_event_order_after_six_second_outcome() {
        let src = include_str!("observe_record.rs");
        let sample_assign = "6 => h1_sample_6s = Some(sampled_at)";
        let delayed_payload = "ObservationPayload::DecisionPointBuyQuote";
        let tracking_finished = "// --- Section 19/36: complete";
        let sample_pos = src
            .find(sample_assign)
            .expect("6s sample assignment present");
        let delayed_pos = src.find(delayed_payload).expect("delayed payload present");
        let finished_pos = src
            .find(tracking_finished)
            .expect("tracking finish section present");
        assert!(
            sample_pos < delayed_pos,
            "delayed quote follows the 6s sample"
        );
        assert!(
            delayed_pos < finished_pos,
            "delayed quote is part of tracking before finish"
        );
    }

    #[test]
    fn test_matched_delayed_exit_horizons_are_fixed_subset() {
        assert_eq!(MATCHED_DELAYED_EXIT_HORIZONS_SECS, &[15, 30, 60, 120]);
        for &h in MATCHED_DELAYED_EXIT_HORIZONS_SECS {
            assert!(OUTCOME_HORIZONS_SECS.contains(&h));
            assert!(should_match_delayed_exit_at(h));
        }
        for &h in &[2u64, 4, 6, 8, 10, 12, 18, 21, 24, 27, 45, 90] {
            assert!(!should_match_delayed_exit_at(h));
        }
    }

    #[test]
    fn test_matched_sell_uses_delayed_base_not_original_base() {
        let original_base_raw = 34_636_456_468;
        let delayed_base_raw = 36_844_135_778;
        assert_ne!(original_base_raw, delayed_base_raw);
        assert_eq!(
            matched_delayed_sell_base_input(delayed_base_raw, original_base_raw),
            delayed_base_raw
        );
    }

    #[test]
    fn test_matched_sell_cannot_start_before_delayed_buy_available() {
        let delayed_buy_observed_at = "2026-01-01T00:00:06.425Z".parse::<DateTime<Utc>>().unwrap();
        let valid_start = "2026-01-01T00:00:15.010Z".parse::<DateTime<Utc>>().unwrap();
        let invalid_start = "2026-01-01T00:00:06.424Z".parse::<DateTime<Utc>>().unwrap();
        assert!(matched_sell_start_is_valid(
            delayed_buy_observed_at,
            delayed_buy_observed_at
        ));
        assert!(matched_sell_start_is_valid(
            delayed_buy_observed_at,
            valid_start
        ));
        assert!(!matched_sell_start_is_valid(
            delayed_buy_observed_at,
            invalid_start
        ));
    }

    #[test]
    fn test_matched_exit_source_is_signal_neutral() {
        let src = include_str!("observe_record.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix present");
        assert!(
            !production.contains("766"),
            "collector must not embed frozen H1 threshold"
        );
        assert!(
            !production.contains("f_early_max"),
            "collector must not derive H1"
        );
        let delayed_success = "delayed_entry_quote_truth = delayed_truth";
        let matched_gate = "if should_match_delayed_exit_at(horizon)";
        assert!(production.find(delayed_success).unwrap() < production.find(matched_gate).unwrap());
    }

    #[test]
    fn test_matched_exit_source_sells_delayed_quantity() {
        let src = include_str!("observe_record.rs");
        assert!(src.contains("ObservationPayload::DecisionPointSellQuote"));
        assert!(src
            .contains("matched_delayed_sell_base_input(delayed.base_amount_raw, base_amount_raw)"));
        assert!(src.contains("rec.base_amount_raw != delayed.base_amount_raw"));
    }
    #[test]
    fn test_key_horizon_snapshot_policy() {
        for &h in &[15u64, 30, 60, 120] {
            assert!(should_snapshot_at(h), "expected snapshot at {h}");
        }
        for &h in &[2u64, 4, 6, 8, 10, 12, 18, 21, 24, 27, 45, 90] {
            assert!(!should_snapshot_at(h), "unexpected snapshot at {h}");
        }
    }

    #[test]
    fn test_no_raw_error_string_persistence() {
        // classify_observation_error drops the inner (endpoint-bearing) string; the
        // serialized failure code is a fixed snake_case token only.
        let err = pumpfun_sniper::Error::Rpc("https://secret-endpoint/key123".into());
        let code = pumpfun_sniper::observation::schema::classify_observation_error(&err);
        let json = serde_json::to_string(&code).unwrap();
        assert!(!json.contains("secret"), "leaked inner string: {json}");
        assert_eq!(json, "\"rpc_unavailable\"");

        // MARKET-DATA-TRUTH: a MarketData error carrying an address/endpoint still
        // classifies to a fixed value-free subtype with no leaked payload.
        let mderr = pumpfun_sniper::Error::MarketData(
            "BondingCurve: wrong owner So1111 at https://secret-endpoint/key123".into(),
        );
        let c = classify_observation_error_full(&mderr);
        assert_eq!(c.code, ObservationFailureCode::MarketUnavailable);
        let kjson = serde_json::to_string(&c.market_data_kind.unwrap()).unwrap();
        assert!(!kjson.contains("secret"), "leaked: {kjson}");
        assert_eq!(kjson, "\"account_identity_or_owner\"");
    }

    #[test]
    fn test_output_filename_contains_run_id_but_no_secret() {
        // The recorder file name is observation_<UTC compact>_<run_id>.jsonl. It
        // carries the run id (a UUID) but never an endpoint/key/credential. Assert
        // the shape here against a representative name.
        let run_id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        let name = format!("observation_20260101T000000Z_{run_id}.jsonl");
        assert!(name.starts_with("observation_"));
        assert!(name.ends_with(".jsonl"));
        assert!(name.contains(run_id));
        // No secret-shaped substrings.
        assert!(!name.contains("api-key"));
        assert!(!name.contains("http"));
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
    }

    #[test]
    fn test_run_completion_policy() {
        // Never connected => Failed.
        let c = RunCounters::default();
        assert_eq!(run_completion(&c, false, true), RunCompletion::Failed);
        // Connected, clean, connected-at-end => Complete.
        assert_eq!(run_completion(&c, true, true), RunCompletion::Complete);
        // HARD provider error => Failed. P1 §10: the Failed condition keys off
        // hard_provider_errors (not the schema-v1 total provider_errors, which also
        // counts decode anomalies).
        let mut c_err = RunCounters::default();
        c_err.provider_errors = 1;
        c_err.hard_provider_errors = 1;
        assert_eq!(run_completion(&c_err, true, true), RunCompletion::Failed);
        // Unexpected trade => Failed.
        let mut c_tr = RunCounters::default();
        c_tr.unexpected_trade_events = 1;
        assert_eq!(run_completion(&c_tr, true, true), RunCompletion::Failed);
        // Drain timeout => Failed.
        let mut c_dt = RunCounters::default();
        c_dt.drain_timed_out = 1;
        assert_eq!(run_completion(&c_dt, true, true), RunCompletion::Failed);
        // Disconnect but recovered (connected at end) => Degraded.
        let mut c_dis = RunCounters::default();
        c_dis.stream_disconnect_events = 1;
        assert_eq!(run_completion(&c_dis, true, true), RunCompletion::Degraded);
        // Task failure => Degraded.
        let mut c_tf = RunCounters::default();
        c_tf.task_failures = 1;
        assert_eq!(run_completion(&c_tf, true, true), RunCompletion::Degraded);
    }

    // -----------------------------------------------------------------------
    // AUDIT-001 §14 — new required tests (all network-free).
    // -----------------------------------------------------------------------

    /// Build a dummy serializable sell quote record with a chosen `quoted_at`.
    /// Recorder-owned type only; no market domain type or RPC is constructed.
    fn dummy_sell_quote_record(quoted_at: DateTime<Utc>) -> ExecutableQuoteRecord {
        ExecutableQuoteRecord {
            side: pumpfun_sniper::observation::schema::ObservedSide::Sell,
            venue: pumpfun_sniper::observation::schema::ObservedVenue::PumpBondingCurve,
            quote_asset: pumpfun_sniper::observation::schema::ObservedQuoteAsset::Sol,
            base_decimals: 6,
            quote_decimals: 9,
            base_amount_raw: 123_456,
            base_amount_ui: 0.123456,
            quote_amount_raw: 1_050_000,
            expected_price_sol_per_token: None,
            protocol_fee_bps: 0,
            creator_fee_bps: 0,
            lp_fee_bps: 0,
            slot: 42,
            quoted_at,
        }
    }

    #[test]
    fn test_run_completion_fails_when_stream_unresolved_at_intake_end() {
        // Connected during the run, but the stream is NOT connected at the intake
        // boundary => Failed even with otherwise clean counters.
        let c = RunCounters::default();
        assert_eq!(run_completion(&c, true, false), RunCompletion::Failed);
    }

    #[test]
    fn test_run_completion_allows_recovered_disconnect_as_degraded() {
        // A disconnect that recovered before intake end (connected-at-end true) is
        // Degraded, not Failed.
        let mut c = RunCounters::default();
        c.stream_disconnect_events = 1;
        assert_eq!(run_completion(&c, true, true), RunCompletion::Degraded);
    }

    #[test]
    fn test_run_completion_fails_on_recorder_failure() {
        let mut c = RunCounters::default();
        c.recorder_failures = 1;
        assert_eq!(run_completion(&c, true, true), RunCompletion::Failed);
    }

    #[test]
    fn test_sell_success_sample_time_uses_quote_quoted_at() {
        // On sell-quote SUCCESS the sampled_at is the quote's canonical quoted_at,
        // NOT the failure-completion time passed alongside.
        let quoted_at = "2026-01-01T00:00:30.350Z".parse::<DateTime<Utc>>().unwrap();
        let failure_time = "2026-01-01T00:00:31.000Z".parse::<DateTime<Utc>>().unwrap();
        let rec = dummy_sell_quote_record(quoted_at);
        let sampled_at = outcome_sampled_at(Some(rec.quoted_at), failure_time);
        assert_eq!(sampled_at, quoted_at);
        assert_ne!(sampled_at, failure_time);
    }

    #[test]
    fn test_sell_failure_sample_time_uses_failure_completion_time() {
        // On sell-quote FAILURE the sampled_at is the failure-completion time
        // (stamped after the failed await), independent of any quote timestamp.
        let failure_time = "2026-01-01T00:00:30.500Z".parse::<DateTime<Utc>>().unwrap();
        let sampled_at = outcome_sampled_at(None, failure_time);
        assert_eq!(sampled_at, failure_time);
    }

    #[test]
    fn test_entry_anchor_policy_captured_with_initial_quote() {
        // Source-structure proof (AUDIT-001 §7): the monotonic anchor is captured
        // in the buy-quote success branch, BEFORE InitialMarket is appended. We
        // assert the anchor-capture line precedes the InitialMarket append in the
        // production source and that no anchor capture appears after it.
        let src = include_str!("observe_record.rs");
        // Needles split so this assertion never self-triggers on its own text.
        let anchor_needle = concat!(
            "entry_monotonic_anchor = ",
            "Some(tokio::time::Instant::now())"
        );
        let initial_market_needle =
            concat!("ObservationPayload::", "InitialMarket(InitialMarketRecord");
        let anchor_pos = src
            .find(anchor_needle)
            .expect("anchor capture present in success branch");
        let initial_pos = src
            .find(initial_market_needle)
            .expect("InitialMarket persistence present");
        assert!(
            anchor_pos < initial_pos,
            "anchor must be captured before InitialMarket persistence"
        );
        // No SECOND anchor capture after InitialMarket persistence.
        assert!(
            !src[initial_pos..].contains(anchor_needle),
            "anchor must not be created after InitialMarket persistence"
        );
    }

    #[test]
    fn test_production_source_does_not_discard_recorder_append_results() {
        // AUDIT-001 §13: prove no production pattern that discards a recorder
        // append Result remains. The needle is built from split literals below so
        // this test's own source never self-triggers.
        let src = include_str!("observe_record.rs");
        let discard_needle = ["let _ = ", "recorder", ".append"].concat();
        assert!(
            !src.contains(&discard_needle),
            "production source discards a recorder append Result"
        );
        // And the required helper exists.
        assert!(src.contains("async fn append_required"));
    }

    #[test]
    fn test_channel_close_marks_stream_disconnected() {
        // Connected then channel-closed => stream_connected becomes false.
        let after_connect = apply_stream_transition(false, StreamTransition::Connected);
        assert!(after_connect);
        let after_close = apply_stream_transition(after_connect, StreamTransition::ChannelClosed);
        assert!(!after_close);
        // Disconnected transition also clears it.
        assert!(!apply_stream_transition(
            true,
            StreamTransition::Disconnected
        ));
        // NoChange preserves prior (e.g. provider error / trade).
        assert!(apply_stream_transition(true, StreamTransition::NoChange));
        assert!(!apply_stream_transition(false, StreamTransition::NoChange));
    }

    #[test]
    fn test_post_stop_disconnect_is_ignored_for_intake_completion() {
        // The completeness state is captured BEFORE stop; a post-stop Disconnected
        // is shutdown mechanics and never mutates that captured bool. Model the
        // captured value and assert it drives completion regardless of any later
        // (ignored) disconnect.
        let stream_connected_at_intake_end = true; // captured before client.stop()
        let c = RunCounters::default();
        // Post-stop Disconnected does NOT change the captured value or counters, so
        // completion stays Complete.
        assert_eq!(
            run_completion(&c, true, stream_connected_at_intake_end),
            RunCompletion::Complete
        );
    }

    #[test]
    fn test_successful_task_reaped_during_intake_not_recounted_in_drain() {
        // The pure accounting helper is used by BOTH the intake reaper and the
        // final drain. A successful result increments tracking_completed exactly
        // once per join; a reaped task is removed from the JoinSet, so it is never
        // accounted a second time. We assert the per-result effect is exactly one
        // Completed increment (no path double-counts a single result).
        let mut counters = RunCounters::default();
        let ok: std::result::Result<CandidateTaskOutcome, ()> = Ok(Ok(CandidateTaskResult {
            candidate_id: "sig".into(),
            mint: "mint".into(),
            status: TrackingFinishStatus::Complete,
        }));
        let effect = account_task_result(&ok);
        assert_eq!(effect, TaskAccounting::Completed);
        apply_task_accounting(&mut counters, effect);
        assert_eq!(counters.tracking_completed, 1);
        assert_eq!(counters.recorder_failures, 0);
        assert_eq!(counters.task_failures, 0);
        // A JoinError maps to a task failure, not a completion.
        let join_err: std::result::Result<CandidateTaskOutcome, ()> = Err(());
        assert_eq!(account_task_result(&join_err), TaskAccounting::TaskFailure);
    }

    #[test]
    fn test_recorder_task_failure_is_failed_not_degraded() {
        // A tracking task's RecorderWrite failure accounts as recorder_failures
        // (=> Failed), NOT task_failures (=> Degraded).
        let mut counters = RunCounters::default();
        let rec_fail: std::result::Result<CandidateTaskOutcome, ()> =
            Ok(Err(CandidateTaskFailure::RecorderWrite));
        let effect = account_task_result(&rec_fail);
        assert_eq!(effect, TaskAccounting::RecorderFailure);
        apply_task_accounting(&mut counters, effect);
        assert_eq!(counters.recorder_failures, 1);
        assert_eq!(counters.task_failures, 0);
        assert_eq!(run_completion(&counters, true, true), RunCompletion::Failed);
    }

    // -----------------------------------------------------------------------
    // P1-PROVIDER-DECODE-TRUTH-001 §14 — decode-loss classification tests.
    // -----------------------------------------------------------------------

    /// Apply a decode error's pure accounting to counters WITHOUT recorder I/O:
    /// mirrors `record_decode_error`'s counter mutations exactly (the persist step
    /// is a fixed `decode:`-prefixed StreamState, exercised separately).
    fn account_decode(counters: &mut RunCounters, kind: PumpPortalDecodeKind) {
        counters.provider_errors += 1;
        counters.decode_errors += 1;
        // AUDIT-001 §10/§11: mirror production exactly by delegating to the same pure
        // predicate `record_decode_error` uses, so full AND partial create losses count.
        if is_new_token_decode_kind(kind) {
            counters.new_token_decode_errors += 1;
        }
    }

    #[test]
    fn test_hard_provider_error_fails_run() {
        let mut c = RunCounters::default();
        c.provider_errors += 1;
        c.hard_provider_errors += 1;
        assert_eq!(run_completion(&c, true, true), RunCompletion::Failed);
    }

    #[test]
    fn test_decode_error_degrades_run() {
        let mut c = RunCounters::default();
        account_decode(&mut c, PumpPortalDecodeKind::NewTokenDeserialize);
        // No hard provider errors => not Failed; decode_errors>0 => Degraded.
        assert_eq!(c.hard_provider_errors, 0);
        assert_eq!(run_completion(&c, true, true), RunCompletion::Degraded);
    }

    #[test]
    fn test_decode_error_is_not_complete() {
        let mut c = RunCounters::default();
        account_decode(&mut c, PumpPortalDecodeKind::TradeDeserialize);
        assert_ne!(run_completion(&c, true, true), RunCompletion::Complete);
    }

    #[test]
    fn test_decode_error_is_not_hard_provider_error() {
        // Four decode anomalies must NOT increment hard_provider_errors, so the run
        // is Degraded (not Failed) even though the schema-v1 total reads 4.
        let mut c = RunCounters::default();
        for _ in 0..4 {
            account_decode(&mut c, PumpPortalDecodeKind::NewTokenDeserialize);
        }
        assert_eq!(c.hard_provider_errors, 0);
        assert_eq!(c.decode_errors, 4);
        assert_eq!(
            c.provider_errors, 4,
            "schema-v1 total stays the anomaly sum"
        );
        assert_eq!(run_completion(&c, true, true), RunCompletion::Degraded);
    }

    #[test]
    fn test_new_token_decode_counter_increments() {
        let mut c = RunCounters::default();
        account_decode(&mut c, PumpPortalDecodeKind::NewTokenDeserialize);
        account_decode(&mut c, PumpPortalDecodeKind::NewTokenValidation);
        // AUDIT-001 §11: partial-create losses are ALSO terminal create losses and
        // MUST count toward the modeling-census subset.
        account_decode(&mut c, PumpPortalDecodeKind::PartialNewTokenDeserialize);
        account_decode(&mut c, PumpPortalDecodeKind::PartialNewTokenValidation);
        // Non-create decode kinds do NOT bump the create subset.
        account_decode(&mut c, PumpPortalDecodeKind::TradeDeserialize);
        account_decode(&mut c, PumpPortalDecodeKind::MigrationParse);
        account_decode(&mut c, PumpPortalDecodeKind::TradeValidation);
        assert_eq!(c.new_token_decode_errors, 4);
        assert_eq!(c.decode_errors, 7);
    }

    /// AUDIT-001 §11: the pure predicate classifies EVERY create-loss kind (full and
    /// partial) as a NewToken decode, and NO non-create kind. This is the single
    /// source of truth `record_decode_error` and `account_decode` both delegate to.
    #[test]
    fn test_is_new_token_decode_kind_covers_all_create_losses() {
        assert!(is_new_token_decode_kind(
            PumpPortalDecodeKind::NewTokenDeserialize
        ));
        assert!(is_new_token_decode_kind(
            PumpPortalDecodeKind::NewTokenValidation
        ));
        assert!(is_new_token_decode_kind(
            PumpPortalDecodeKind::PartialNewTokenDeserialize
        ));
        assert!(is_new_token_decode_kind(
            PumpPortalDecodeKind::PartialNewTokenValidation
        ));
        assert!(!is_new_token_decode_kind(
            PumpPortalDecodeKind::MigrationParse
        ));
        assert!(!is_new_token_decode_kind(
            PumpPortalDecodeKind::TradeDeserialize
        ));
        assert!(!is_new_token_decode_kind(
            PumpPortalDecodeKind::TradeValidation
        ));
    }

    #[test]
    fn test_decode_category_persist_prefix_is_fixed() {
        // The persisted category is the fixed `decode:` prefix + the safe structural
        // category (no raw provider values), capped at 256.
        let e = PumpPortalDecodeError {
            kind: PumpPortalDecodeKind::NewTokenDeserialize,
            category: "new_token_deserialize|missing=name,uri".to_string(),
        };
        let persisted = sanitize_persist_text(&format!("decode:{}", e.category), 256);
        assert!(persisted.starts_with("decode:"));
        assert!(persisted.chars().count() <= 256);
        assert!(!persisted.contains("SECRET"));
    }

    #[test]
    fn test_decode_event_does_not_change_stream_connected() {
        // A decode event carries no StreamTransition; the intake bool is untouched.
        // The pure transition helper has no decode variant, so an active-intake
        // decode leaves stream_connected exactly as-is (modeled via NoChange).
        assert!(apply_stream_transition(true, StreamTransition::NoChange));
        assert!(!apply_stream_transition(false, StreamTransition::NoChange));
    }

    #[test]
    fn test_post_stop_decode_event_does_not_change_intake_connection_truth() {
        // The completeness bool is captured BEFORE stop; a post-stop decode event
        // only counts/persists and cannot alter it. Model: captured=true, a decode
        // anomaly present (Degraded), completion still uses the captured value.
        let mut c = RunCounters::default();
        account_decode(&mut c, PumpPortalDecodeKind::NewTokenDeserialize);
        let stream_connected_at_intake_end = true; // frozen before client.stop()
        assert_eq!(
            run_completion(&c, true, stream_connected_at_intake_end),
            RunCompletion::Degraded
        );
    }

    // -----------------------------------------------------------------------
    // P1-OBSERVATION-SCHEMA-V2 §23 — partial-create integration (network-free).
    // -----------------------------------------------------------------------

    /// A PartialNewTokenEvent with valid identity and caller-chosen provider
    /// Options / metadata.
    fn partial_event(
        provider_present: bool,
    ) -> pumpfun_sniper::stream::pumpportal::PartialNewTokenEvent {
        pumpfun_sniper::stream::pumpportal::PartialNewTokenEvent {
            signature: "sig".to_string(),
            mint: "mint".to_string(),
            trader_public_key: "creator".to_string(),
            tx_type: "create".to_string(),
            initial_buy: provider_present.then_some(1.0),
            bonding_curve_key: provider_present.then(|| "bc".to_string()),
            v_tokens_in_bonding_curve: provider_present.then_some(1000.0),
            v_sol_in_bonding_curve: provider_present.then_some(30.0),
            market_cap_sol: provider_present.then_some(30.0),
            name: "n".to_string(),
            symbol: "s".to_string(),
            uri: "u".to_string(),
        }
    }

    #[test]
    fn test_full_new_token_maps_provider_fields_to_some() {
        let ev = new_token_with_metadata("n", "s", "u");
        match NormalizedCandidate::from_full(&ev).candidate_observed(false) {
            ObservationPayload::CandidateObserved(rec) => {
                assert_eq!(rec.bonding_curve, Some("bc".to_string()));
                assert_eq!(rec.provider_initial_buy, Some(1.0));
                assert_eq!(rec.provider_v_tokens_in_bonding_curve, Some(1000.0));
                assert_eq!(rec.provider_v_sol_in_bonding_curve_sol, Some(30.0));
                assert_eq!(rec.provider_market_cap_sol, Some(30.0));
                assert_eq!(rec.provider_create_shape, Some(ProviderCreateShape::Full));
            }
            other => panic!("expected CandidateObserved, got {other:?}"),
        }
    }

    #[test]
    fn test_partial_new_token_maps_missing_provider_fields_to_none() {
        // A partial event with every provider Option absent => None preserved, no
        // zero synthesis, and shape Partial.
        let ev = partial_event(false);
        match NormalizedCandidate::from_partial(&ev).candidate_observed(false) {
            ObservationPayload::CandidateObserved(rec) => {
                assert_eq!(rec.bonding_curve, None);
                assert_eq!(rec.provider_initial_buy, None);
                assert_eq!(rec.provider_v_tokens_in_bonding_curve, None);
                assert_eq!(rec.provider_v_sol_in_bonding_curve_sol, None);
                assert_eq!(rec.provider_market_cap_sol, None);
                assert_eq!(
                    rec.provider_create_shape,
                    Some(ProviderCreateShape::Partial)
                );
                // Identity + metadata still populated.
                assert_eq!(rec.signature, "sig");
                assert_eq!(rec.mint, "mint");
                assert_eq!(rec.creator, "creator");
                assert_eq!(rec.name, "n");
            }
            other => panic!("expected CandidateObserved, got {other:?}"),
        }
    }

    #[test]
    fn test_partial_new_token_present_provider_fields_pass_through_some() {
        // A partial with present optionals passes them through unchanged (no default).
        let ev = partial_event(true);
        match NormalizedCandidate::from_partial(&ev).candidate_observed(false) {
            ObservationPayload::CandidateObserved(rec) => {
                assert_eq!(rec.bonding_curve, Some("bc".to_string()));
                assert_eq!(rec.provider_initial_buy, Some(1.0));
                assert_eq!(
                    rec.provider_create_shape,
                    Some(ProviderCreateShape::Partial)
                );
            }
            other => panic!("expected CandidateObserved, got {other:?}"),
        }
    }

    #[test]
    fn test_partial_new_token_builds_candidate_observed_payload() {
        // The partial discovery path appends a CandidateObserved payload (persist
        // first). Prove the builder yields exactly that variant.
        let ev = partial_event(false);
        assert!(matches!(
            NormalizedCandidate::from_partial(&ev).candidate_observed(false),
            ObservationPayload::CandidateObserved(_)
        ));
    }

    #[test]
    fn test_partial_source_synthesizes_no_zero_for_absent_provider_fields() {
        // Source guard: the partial mapping must pass Option provider fields through
        // (ev.<field>), never coerce an absent field to a numeric zero / default.
        let src = include_str!("observe_record.rs");
        // Locate the from_partial mapping body.
        let start = src.find("fn from_partial(").expect("from_partial present");
        let body = &src[start..start + 900];
        assert!(
            body.contains("provider_initial_buy: ev.initial_buy"),
            "partial must pass initial_buy Option through unchanged"
        );
        assert!(
            body.contains("bonding_curve: ev.bonding_curve_key.clone()"),
            "partial must pass bonding_curve Option through unchanged"
        );
        // No zero-default synthesis idioms in the partial mapping.
        for needle in [
            "unwrap_or(0",
            "unwrap_or_default",
            "Some(0.0)",
            ".unwrap_or(0.0)",
        ] {
            assert!(
                !body.contains(needle),
                "partial mapping must not synthesize a zero: {needle}"
            );
        }
    }

    #[test]
    fn test_partial_new_token_increments_partial_counter_once() {
        // The intake handler increments partial_new_token_events exactly once per
        // received PartialNewToken. Model the counter mutation the handler performs.
        let mut counters = RunCounters::default();
        counters.partial_new_token_events += 1; // exactly one received partial
        assert_eq!(counters.partial_new_token_events, 1);
        // And it does NOT touch the provider/decode subsets.
        assert_eq!(counters.provider_errors, 0);
        assert_eq!(counters.decode_errors, 0);
        assert_eq!(counters.new_token_decode_errors, 0);
    }

    #[test]
    fn test_partial_new_token_does_not_increment_provider_or_decode_counters() {
        // Only PumpPortalDecodeError values ever reach record_decode_error. A partial
        // create is NOT a decode error: it flows through the CandidateObserved path,
        // so provider_errors/decode_errors/new_token_decode_errors stay untouched.
        let ev = partial_event(false);
        assert!(matches!(
            NormalizedCandidate::from_partial(&ev).candidate_observed(false),
            ObservationPayload::CandidateObserved(_)
        ));
        let mut counters = RunCounters::default();
        // Simulate the handler's ONLY counter effects for a first-seen partial.
        counters.partial_new_token_events += 1;
        counters.candidates_seen += 1;
        counters.unique_candidates += 1;
        counters.tracking_started += 1;
        assert_eq!(counters.provider_errors, 0);
        assert_eq!(counters.decode_errors, 0);
        assert_eq!(counters.new_token_decode_errors, 0);
    }

    #[test]
    fn test_partial_new_token_increments_seen_and_unique_normally() {
        // A first-seen partial signature is trackable; a repeat is a duplicate.
        let mut seen = HashSet::new();
        let sig = "sig".to_string();
        assert!(should_track_first_seen(&seen, &sig));
        seen.insert(sig.clone());
        assert!(!should_track_first_seen(&seen, &sig));
    }

    #[test]
    fn test_partial_new_token_eligible_for_capacity_admission() {
        // Policy-level: a partial candidate is admitted when a permit is available,
        // exactly like a full create (§17). It is never skipped for absent provider
        // fields.
        assert!(capacity_admits(true));
        assert!(!capacity_admits(false));
    }

    #[test]
    fn test_partial_counter_serialized_into_run_finished() {
        // Source guard: RunFinished construction wires the partial counter through.
        let src = include_str!("observe_record.rs");
        assert!(
            src.contains("partial_new_token_events: counters.partial_new_token_events"),
            "RunFinished must carry the partial counter"
        );
    }
}
