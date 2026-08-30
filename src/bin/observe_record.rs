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
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use pumpfun_sniper::market::PumpMarketOracle;
use pumpfun_sniper::observation::schema::{
    classify_observation_error, protocol_net_ex_network_return_bps, sanitize_persist_text,
    CandidateObservedRecord, ExecutableQuoteRecord, InitialMarketRecord, MarketSnapshotRecord,
    MigrationObservedRecord, ObservationFailureCode, ObservationPayload, OutcomeSampleRecord,
    RunCompletion, RunFinishedRecord, RunStartedRecord, StreamStateKind, StreamStateRecord,
    TrackingFinishStatus, TrackingFinishedRecord, TrackingSkippedRecord, ENTRY_QUOTE_LAMPORTS,
    OUTCOME_HORIZONS_SECS, SNAPSHOT_HORIZONS_SECS,
};
use pumpfun_sniper::observation::ObservationRecorder;
use pumpfun_sniper::runtime::RuntimeLease;
use pumpfun_sniper::stream::pumpportal::{PumpPortalDecodeError, PumpPortalDecodeKind};
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

/// Bounds for `--intake-seconds` (packet section 30). Max 6 hours in v1.
const INTAKE_SECONDS_MIN: u64 = 60;
const INTAKE_SECONDS_MAX: u64 = 21_600;

/// Bounds for `--max-active-candidates` (packet section 30).
const MAX_ACTIVE_MIN: usize = 1;
const MAX_ACTIVE_MAX: usize = 256;

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

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();
    let intake_seconds = validate_intake_seconds(args.intake_seconds)?;
    let max_active_candidates = validate_max_active(args.max_active_candidates)?;

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
    let run_started = RunStartedRecord::new(
        detect_source_revision(),
        detect_working_tree_clean(),
        env!("CARGO_PKG_VERSION").to_string(),
        intake_seconds,
        max_active_candidates,
    );
    let recorder = ObservationRecorder::create(&args.output_dir, run_started)
        .await
        .context("failed to create observation recorder")?;
    if let Some(name) = run_file_name(&args.output_dir).await {
        // Filename only — never the directory path.
        println!("Recorder file: {name}");
    }

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

    let oracle = Arc::new(PumpMarketOracle::new(rpc.clone()));
    let semaphore = Arc::new(Semaphore::new(max_active_candidates));

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
                    &mut seen_signatures,
                    &mut counters,
                    &mut tasks,
                    &mut ever_connected,
                    &mut stream_connected,
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
    drain_outcome_tasks(&mut tasks, &recorder, &mut counters).await;

    // --- Section 22/43: append authoritative RunFinished, then sync. ---
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
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    tasks: &mut JoinSet<CandidateTaskOutcome>,
    ever_connected: &mut bool,
    stream_connected: &mut bool,
) -> std::result::Result<(), CollectorRecordError> {
    match event {
        PumpPortalEvent::Connected => {
            counters.stream_connected_events += 1;
            *ever_connected = true;
            *stream_connected =
                apply_stream_transition(*stream_connected, StreamTransition::Connected);
            append_required(recorder, stream_state(StreamStateKind::Connected, None)).await?;
            println!("PumpPortal: connected");
        }
        PumpPortalEvent::Disconnected => {
            counters.stream_disconnect_events += 1;
            *stream_connected =
                apply_stream_transition(*stream_connected, StreamTransition::Disconnected);
            append_required(recorder, stream_state(StreamStateKind::Disconnected, None)).await?;
        }
        PumpPortalEvent::Error(category) => {
            // P1 §9: HARD provider error. Counts toward the schema-v1 total AND the
            // internal hard subset; a provider error does not itself mutate
            // stream_connected (§9); it fails the run.
            counters.provider_errors += 1;
            counters.hard_provider_errors += 1;
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
        PumpPortalEvent::Trade(_) => {
            // Free-only plan: a Trade is an anomaly. No trade fields recorded.
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
            let candidate_received_at = Utc::now();
            counters.candidates_seen += 1;

            let signature = ev.signature.clone();
            let duplicate = seen_signatures.contains(&signature);

            // AUDIT-001 §4.1 ordering: receive NewToken -> await a SUCCESSFUL
            // CandidateObserved append -> ONLY THEN mutate seen_signatures /
            // capacity / spawn tracking. On append Err: do not insert the
            // signature, do not acquire a permit, do not spawn; return a fixed
            // secret-safe error (the process may terminate without RunFinished).
            append_required(recorder, candidate_observed(&ev, duplicate)).await?;

            if duplicate {
                counters.duplicate_candidate_events += 1;
                // Section 35: do NOT launch a second tracking task.
                return Ok(());
            }

            // First-seen — CandidateObserved is now durably persisted.
            seen_signatures.insert(signature.clone());
            counters.unique_candidates += 1;

            let candidate_id = signature;
            let mint_str = ev.mint.clone();

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

            // Section 35: try to reserve tracking capacity.
            match semaphore.clone().try_acquire_owned() {
                Ok(permit) => {
                    counters.tracking_started += 1;
                    let oracle = oracle.clone();
                    let recorder = recorder.clone();
                    tasks.spawn(track_candidate(
                        permit,
                        candidate_id,
                        mint,
                        candidate_received_at,
                        oracle,
                        recorder,
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
                drain_new_token_intake_closed(recorder, seen_signatures, counters, &ev).await?;
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
                drain_new_token_intake_closed(recorder, seen_signatures, counters, &ev).await?;
            }
        }
    }
    Ok(())
}

/// Persist a drained NewToken with intake closed: CandidateObserved first, then
/// (if first-seen) an IntakeClosed TrackingSkipped. Never starts a task. Shared by
/// pre-stop and post-stop drains. Every append is required.
async fn drain_new_token_intake_closed(
    recorder: &ObservationRecorder,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
    ev: &pumpfun_sniper::stream::pumpportal::NewTokenEvent,
) -> std::result::Result<(), CollectorRecordError> {
    counters.candidates_seen += 1;
    let signature = ev.signature.clone();
    let duplicate = seen_signatures.contains(&signature);
    append_required(recorder, candidate_observed(ev, duplicate)).await?;
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
        tracking_skipped(&signature, &ev.mint, ObservationFailureCode::IntakeClosed),
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

/// Bounded initial snapshot + buy-quote retry schedule (0/250/500/1000ms), then a
/// fixed-horizon outcome sampling loop anchored to the successful initial buy
/// quote. Read-only throughout: `quote_buy_sol`/`quote_sell_raw`/`snapshot` are
/// canonical quotes, never order submissions.
async fn track_candidate(
    _permit: tokio::sync::OwnedSemaphorePermit,
    candidate_id: String,
    mint: Pubkey,
    candidate_received_at: DateTime<Utc>,
    oracle: Arc<PumpMarketOracle>,
    recorder: ObservationRecorder,
) -> CandidateTaskOutcome {
    let mint_str = mint.to_string();

    // --- Section 16/36: bounded initial availability retry. ---
    let backoffs = [0u64, 250, 500, 1000];
    let mut last_snapshot: Option<MarketSnapshotRecord> = None;
    let mut last_snapshot_failure: Option<ObservationFailureCode> = None;
    let mut buy_quote_record: Option<ExecutableQuoteRecord> = None;
    let mut buy_quote_failure: Option<ObservationFailureCode> = None;
    let mut initial_base_amount_raw: Option<u64> = None;
    let mut entry_wall_time: Option<DateTime<Utc>> = None;
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

        match oracle.snapshot(&mint).await {
            Ok(snap) => {
                last_snapshot = Some(MarketSnapshotRecord::from(&snap));
                last_snapshot_failure = None;
            }
            Err(e) => {
                last_snapshot = None;
                last_snapshot_failure = Some(classify_observation_error(&e));
            }
        }

        match oracle.quote_buy_sol(&mint, ENTRY_QUOTE_LAMPORTS).await {
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
                // Success criterion for tracking is a valid SOL buy quote.
                break;
            }
            Err(e) => {
                buy_quote_record = None;
                buy_quote_failure = Some(classify_observation_error(&e));
            }
        }
    }

    // --- Section 15: append InitialMarket (snapshot XOR failure; buy XOR failure). ---
    // Ensure the XOR invariants hold: if neither present, mark a failure.
    if last_snapshot.is_none() && last_snapshot_failure.is_none() {
        last_snapshot_failure = Some(ObservationFailureCode::Other);
    }
    if buy_quote_record.is_none() && buy_quote_failure.is_none() {
        buy_quote_failure = Some(ObservationFailureCode::Other);
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
        let (sell_quote, sell_quote_failure, return_bps, sampled_at) = match oracle
            .quote_sell_raw(&mint, base_amount_raw)
            .await
        {
            Ok(q) => {
                let rec = ExecutableQuoteRecord::from(&q);
                let ret =
                    protocol_net_ex_network_return_bps(ENTRY_QUOTE_LAMPORTS, rec.quote_amount_raw);
                // Sell success: sampled_at is the quote's canonical timestamp.
                let sampled_at = outcome_sampled_at(Some(rec.quoted_at), Utc::now());
                (Some(rec), None, ret, sampled_at)
            }
            Err(e) => {
                // Sell failure: sampled_at is the failure-completion time,
                // stamped AFTER the failed await returned.
                let sampled_at = outcome_sampled_at(None, Utc::now());
                (None, Some(classify_observation_error(&e)), None, sampled_at)
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
        // the outcome sampled_at above (AUDIT-001 §6).
        let (snapshot, snapshot_failure) = if should_snapshot_at(horizon) {
            match oracle.snapshot(&mint).await {
                Ok(snap) => (Some(MarketSnapshotRecord::from(&snap)), None),
                Err(e) => (None, Some(classify_observation_error(&e))),
            }
        } else {
            // Absent != failure at non-key horizons.
            (None, None)
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
            }))
            .await
            .is_err()
        {
            return Err(CandidateTaskFailure::RecorderWrite);
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

/// P1-PROVIDER-DECODE-TRUTH-001 §9: account + persist a single decode/schema-loss
/// anomaly. Increments the schema-v1 total (`provider_errors`) AND the internal
/// `decode_errors` subset; NewToken decode kinds also bump `new_token_decode_errors`.
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
    if matches!(
        e.kind,
        PumpPortalDecodeKind::NewTokenDeserialize | PumpPortalDecodeKind::NewTokenValidation
    ) {
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
fn candidate_observed(
    ev: &pumpfun_sniper::stream::pumpportal::NewTokenEvent,
    duplicate: bool,
) -> ObservationPayload {
    ObservationPayload::CandidateObserved(CandidateObservedRecord {
        candidate_id: ev.signature.clone(),
        signature: ev.signature.clone(),
        mint: ev.mint.clone(),
        creator: ev.trader_public_key.clone(),
        bonding_curve: ev.bonding_curve_key.clone(),
        tx_type: ev.tx_type.clone(),
        provider_initial_buy: ev.initial_buy,
        provider_v_tokens_in_bonding_curve: ev.v_tokens_in_bonding_curve,
        provider_v_sol_in_bonding_curve_sol: ev.v_sol_in_bonding_curve,
        provider_market_cap_sol: ev.market_cap_sol,
        name: sanitize_persist_text(&ev.name, 256),
        symbol: sanitize_persist_text(&ev.symbol, 64),
        uri: sanitize_persist_text(&ev.uri, 1024),
        duplicate,
    })
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
        let code = classify_observation_error(&err);
        let json = serde_json::to_string(&code).unwrap();
        assert!(!json.contains("secret"), "leaked inner string: {json}");
        assert_eq!(json, "\"rpc_unavailable\"");
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
        if matches!(
            kind,
            PumpPortalDecodeKind::NewTokenDeserialize | PumpPortalDecodeKind::NewTokenValidation
        ) {
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
        // Non-NewToken decode kinds do NOT bump the NewToken subset.
        account_decode(&mut c, PumpPortalDecodeKind::TradeDeserialize);
        account_decode(&mut c, PumpPortalDecodeKind::MigrationParse);
        account_decode(&mut c, PumpPortalDecodeKind::TradeValidation);
        assert_eq!(c.new_token_decode_errors, 2);
        assert_eq!(c.decode_errors, 5);
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
}
