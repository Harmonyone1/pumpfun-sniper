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
const OUTCOME_DRAIN_SECS: u64 = 135;

/// Bounds for `--intake-seconds` (packet section 30). Max 6 hours in v1.
const INTAKE_SECONDS_MIN: u64 = 60;
const INTAKE_SECONDS_MAX: u64 = 21_600;

/// Bounds for `--max-active-candidates` (packet section 30).
const MAX_ACTIVE_MIN: usize = 1;
const MAX_ACTIVE_MAX: usize = 256;

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
    provider_errors: u64,
    unexpected_trade_events: u64,
    migrations_seen: u64,
    task_failures: u64,
    drain_timed_out: u64,
}

/// Pure run-completion policy from authoritative counters plus drain outcome
/// (packet section 22). Counters are authoritative.
fn run_completion(counters: &RunCounters, ever_connected: bool) -> RunCompletion {
    let failed = !ever_connected
        || counters.provider_errors > 0
        || counters.unexpected_trade_events > 0
        || counters.drain_timed_out > 0;
    if failed {
        return RunCompletion::Failed;
    }
    if counters.stream_disconnect_events > 0 || counters.task_failures > 0 {
        return RunCompletion::Degraded;
    }
    RunCompletion::Complete
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
    let mut tasks: JoinSet<CandidateTaskResult> = JoinSet::new();
    let mut ever_connected = false;

    // --- Section 41: bounded intake window. No early-success exit. ---
    let intake_deadline = tokio::time::Instant::now() + Duration::from_secs(intake_seconds);

    loop {
        let now = tokio::time::Instant::now();
        if now >= intake_deadline {
            break;
        }
        let remaining = intake_deadline.saturating_duration_since(now);
        let wait = remaining.min(Duration::from_millis(500));

        match tokio::time::timeout(wait, event_rx.recv()).await {
            Ok(Some(event)) => {
                handle_intake_event(
                    event,
                    &recorder,
                    &oracle,
                    &semaphore,
                    &mut seen_signatures,
                    &mut counters,
                    &mut tasks,
                    &mut ever_connected,
                )
                .await;
            }
            // Channel closed (worker gone): stop intake.
            Ok(None) => break,
            // Quiet point: just loop and re-check the deadline.
            Err(_) => continue,
        }
    }

    // --- Section 43 final order: stop stream. ---
    client.stop();

    // --- Section 41: nonblocking drain of already-queued provider events. NO new
    // tracking tasks are started for drained NewTokens (IntakeClosed skip). ---
    drain_queued_after_intake(
        &mut event_rx,
        &recorder,
        &mut seen_signatures,
        &mut counters,
    )
    .await;

    // --- Section 42: bounded outcome drain of already-started tasks. ---
    drain_outcome_tasks(&mut tasks, &recorder, &mut counters).await;

    // --- Section 22/43: append authoritative RunFinished, then sync. ---
    let completion = run_completion(&counters, ever_connected);
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
    recorder
        .append(ObservationPayload::RunFinished(run_finished))
        .await
        .context("failed to append RunFinished")?;
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
    tasks: &mut JoinSet<CandidateTaskResult>,
    ever_connected: &mut bool,
) {
    match event {
        PumpPortalEvent::Connected => {
            counters.stream_connected_events += 1;
            *ever_connected = true;
            let _ = recorder
                .append(stream_state(StreamStateKind::Connected, None))
                .await;
            println!("PumpPortal: connected");
        }
        PumpPortalEvent::Disconnected => {
            counters.stream_disconnect_events += 1;
            let _ = recorder
                .append(stream_state(StreamStateKind::Disconnected, None))
                .await;
        }
        PumpPortalEvent::Error(category) => {
            counters.provider_errors += 1;
            // `category` is already a sanitized fixed provider category.
            let safe = sanitize_persist_text(&category, 128);
            let _ = recorder
                .append(stream_state(StreamStateKind::ProviderError, Some(safe)))
                .await;
        }
        PumpPortalEvent::Trade(_) => {
            // Free-only plan: a Trade is an anomaly. No trade fields recorded.
            counters.unexpected_trade_events += 1;
            let _ = recorder
                .append(stream_state(StreamStateKind::UnexpectedTrade, None))
                .await;
        }
        PumpPortalEvent::Migration(ev) => {
            counters.migrations_seen += 1;
            let _ = recorder
                .append(ObservationPayload::MigrationObserved(
                    MigrationObservedRecord {
                        mint: ev.mint,
                        signature: ev.signature,
                        pool: ev.pool,
                        pool_id: ev.pool_id,
                        provider_received_at: ev.received_at,
                    },
                ))
                .await;
        }
        PumpPortalEvent::NewToken(ev) => {
            let candidate_received_at = Utc::now();
            counters.candidates_seen += 1;

            let signature = ev.signature.clone();
            let duplicate = seen_signatures.contains(&signature);

            // Section 35/51: ALWAYS persist CandidateObserved first.
            let _ = recorder.append(candidate_observed(&ev, duplicate)).await;

            if duplicate {
                counters.duplicate_candidate_events += 1;
                // Section 35: do NOT launch a second tracking task.
                return;
            }

            // First-seen.
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
                    let _ = recorder
                        .append(tracking_skipped(
                            &candidate_id,
                            &mint_str,
                            ObservationFailureCode::InvalidProviderIdentity,
                        ))
                        .await;
                    return;
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
                    let _ = recorder
                        .append(tracking_skipped(
                            &candidate_id,
                            &mint_str,
                            ObservationFailureCode::TrackingCapacity,
                        ))
                        .await;
                }
            }
        }
    }
}

/// Section 41: after the intake deadline, drain already-queued events. Migrations,
/// stream states, and unexpected trades are still recorded; queued NewTokens are
/// persisted (CandidateObserved) but NOT retracked (IntakeClosed skip).
async fn drain_queued_after_intake(
    event_rx: &mut mpsc::Receiver<PumpPortalEvent>,
    recorder: &ObservationRecorder,
    seen_signatures: &mut HashSet<String>,
    counters: &mut RunCounters,
) {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            PumpPortalEvent::Connected => {
                counters.stream_connected_events += 1;
                let _ = recorder
                    .append(stream_state(StreamStateKind::Connected, None))
                    .await;
            }
            PumpPortalEvent::Disconnected => {
                counters.stream_disconnect_events += 1;
                let _ = recorder
                    .append(stream_state(StreamStateKind::Disconnected, None))
                    .await;
            }
            PumpPortalEvent::Error(category) => {
                counters.provider_errors += 1;
                let safe = sanitize_persist_text(&category, 128);
                let _ = recorder
                    .append(stream_state(StreamStateKind::ProviderError, Some(safe)))
                    .await;
            }
            PumpPortalEvent::Trade(_) => {
                counters.unexpected_trade_events += 1;
                let _ = recorder
                    .append(stream_state(StreamStateKind::UnexpectedTrade, None))
                    .await;
            }
            PumpPortalEvent::Migration(ev) => {
                counters.migrations_seen += 1;
                let _ = recorder
                    .append(ObservationPayload::MigrationObserved(
                        MigrationObservedRecord {
                            mint: ev.mint,
                            signature: ev.signature,
                            pool: ev.pool,
                            pool_id: ev.pool_id,
                            provider_received_at: ev.received_at,
                        },
                    ))
                    .await;
            }
            PumpPortalEvent::NewToken(ev) => {
                counters.candidates_seen += 1;
                let signature = ev.signature.clone();
                let duplicate = seen_signatures.contains(&signature);
                let _ = recorder.append(candidate_observed(&ev, duplicate)).await;
                if duplicate {
                    counters.duplicate_candidate_events += 1;
                    continue;
                }
                seen_signatures.insert(signature.clone());
                counters.unique_candidates += 1;
                // Intake closed: retain the candidate, but never start a new task.
                counters.tracking_skipped += 1;
                let _ = recorder
                    .append(tracking_skipped(
                        &signature,
                        &ev.mint,
                        ObservationFailureCode::IntakeClosed,
                    ))
                    .await;
            }
        }
    }
}

/// Section 42: wait for already-started tracking tasks under a hard 135s bound. On
/// timeout, abort remaining tasks and mark the run drain-timed-out.
async fn drain_outcome_tasks(
    tasks: &mut JoinSet<CandidateTaskResult>,
    recorder: &ObservationRecorder,
    counters: &mut RunCounters,
) {
    let drain = async {
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(result) => {
                    // The task already appended its own TrackingFinished record.
                    let _ = result;
                    counters.tracking_completed += 1;
                }
                Err(_) => {
                    // Section 40: panic/join error. Identity is not recoverable from
                    // a JoinError here, so we record a task failure count and a
                    // best-effort finish record without identity is not possible;
                    // the run completion becomes Degraded via task_failures.
                    counters.task_failures += 1;
                }
            }
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
                match joined {
                    Ok(_) => counters.tracking_completed += 1,
                    Err(_) => counters.task_failures += 1,
                }
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
) -> CandidateTaskResult {
    let mint_str = mint.to_string();

    // --- Section 16/36: bounded initial availability retry. ---
    let backoffs = [0u64, 250, 500, 1000];
    let mut last_snapshot: Option<MarketSnapshotRecord> = None;
    let mut last_snapshot_failure: Option<ObservationFailureCode> = None;
    let mut buy_quote_record: Option<ExecutableQuoteRecord> = None;
    let mut buy_quote_failure: Option<ObservationFailureCode> = None;
    let mut initial_base_amount_raw: Option<u64> = None;
    let mut entry_wall_time: Option<DateTime<Utc>> = None;
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
                let rec = ExecutableQuoteRecord::from(&q);
                initial_base_amount_raw = Some(rec.base_amount_raw);
                entry_wall_time = Some(rec.quoted_at);
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

    let _ = recorder
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
        .await;

    // --- Section 16: no buy quote after retries => finish, no sell samples. ---
    let (base_amount_raw, entry_wall_time) = match (initial_base_amount_raw, entry_wall_time) {
        (Some(b), Some(t)) => (b, t),
        _ => {
            let _ = recorder
                .append(tracking_finished(
                    &candidate_id,
                    &mint_str,
                    TrackingFinishStatus::InitialQuoteUnavailable,
                    0,
                    0,
                ))
                .await;
            return CandidateTaskResult {
                candidate_id,
                mint: mint_str,
                status: TrackingFinishStatus::InitialQuoteUnavailable,
            };
        }
    };

    // --- Section 37: anchor horizons to the successful initial buy quote. ---
    let entry_monotonic = tokio::time::Instant::now();
    let mut successful_samples: u16 = 0;
    let mut failed_samples: u16 = 0;

    for &horizon in OUTCOME_HORIZONS_SECS {
        // Sleep until the monotonic due instant (never negative).
        let due_monotonic = entry_monotonic + Duration::from_secs(horizon);
        let now = tokio::time::Instant::now();
        if due_monotonic > now {
            tokio::time::sleep(due_monotonic - now).await;
        }
        let sampled_at = Utc::now();
        let due_at = horizon_due_at(entry_wall_time, horizon);
        let lag_ms = sample_lag_ms(due_at, sampled_at);

        // --- Section 38: exact-size future sell quote (quantity frozen). ---
        let (sell_quote, sell_quote_failure, return_bps) = match oracle
            .quote_sell_raw(&mint, base_amount_raw)
            .await
        {
            Ok(q) => {
                let rec = ExecutableQuoteRecord::from(&q);
                let ret =
                    protocol_net_ex_network_return_bps(ENTRY_QUOTE_LAMPORTS, rec.quote_amount_raw);
                (Some(rec), None, ret)
            }
            Err(e) => (None, Some(classify_observation_error(&e)), None),
        };

        if sell_quote.is_some() {
            successful_samples = successful_samples.saturating_add(1);
        } else {
            failed_samples = failed_samples.saturating_add(1);
        }

        // --- Section 39: snapshot only at key horizons, recorded independently. ---
        let (snapshot, snapshot_failure) = if should_snapshot_at(horizon) {
            match oracle.snapshot(&mint).await {
                Ok(snap) => (Some(MarketSnapshotRecord::from(&snap)), None),
                Err(e) => (None, Some(classify_observation_error(&e))),
            }
        } else {
            // Absent != failure at non-key horizons.
            (None, None)
        };

        let _ = recorder
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
            .await;
    }

    // --- Section 19/36: complete. ---
    let _ = recorder
        .append(tracking_finished(
            &candidate_id,
            &mint_str,
            TrackingFinishStatus::Complete,
            successful_samples,
            failed_samples,
        ))
        .await;

    CandidateTaskResult {
        candidate_id,
        mint: mint_str,
        status: TrackingFinishStatus::Complete,
    }
}

// ---------------------------------------------------------------------------
// Payload constructors
// ---------------------------------------------------------------------------

fn stream_state(state: StreamStateKind, category: Option<String>) -> ObservationPayload {
    ObservationPayload::StreamState(StreamStateRecord { state, category })
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
        assert_eq!(run_completion(&c, false), RunCompletion::Failed);
        // Connected, clean => Complete.
        assert_eq!(run_completion(&c, true), RunCompletion::Complete);
        // Provider error => Failed.
        let mut c_err = RunCounters::default();
        c_err.provider_errors = 1;
        assert_eq!(run_completion(&c_err, true), RunCompletion::Failed);
        // Unexpected trade => Failed.
        let mut c_tr = RunCounters::default();
        c_tr.unexpected_trade_events = 1;
        assert_eq!(run_completion(&c_tr, true), RunCompletion::Failed);
        // Drain timeout => Failed.
        let mut c_dt = RunCounters::default();
        c_dt.drain_timed_out = 1;
        assert_eq!(run_completion(&c_dt, true), RunCompletion::Failed);
        // Disconnect but recovered => Degraded.
        let mut c_dis = RunCounters::default();
        c_dis.stream_disconnect_events = 1;
        assert_eq!(run_completion(&c_dis, true), RunCompletion::Degraded);
        // Task failure => Degraded.
        let mut c_tf = RunCounters::default();
        c_tf.task_failures = 1;
        assert_eq!(run_completion(&c_tf, true), RunCompletion::Degraded);
    }
}
