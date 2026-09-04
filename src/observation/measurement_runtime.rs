//! P3 measurement pilot — runtime SUPPORT components (AMENDMENT-002).
//!
//! Network-free, deterministic building blocks for the Domain-1 acquisition path
//! that the amendment elevated to first-class: a bounded NO-SILENT-DROP trade
//! queue, a per-mint subscription/coverage state machine, reconnect-restore-all,
//! and a coverage-aware ParticipationSnapshot wrapper (active-zero-trades vs
//! missing/failure). These are the highest-risk correctness surfaces; the live
//! `observe_record.rs` async hookup (issuing SubscriptionCommands, dispatching
//! Trade events into buffers, calling snapshot/holder/probe in track_candidate,
//! RunFinished flush) consumes these but is NOT in this module.
//!
//! No trading/signing/tx/strategy. Subscription attempts are UNCONDITIONAL.

use chrono::{DateTime, Utc};

use super::measurement::{
    compute_participation_snapshot, curve_implied_tokens_out, redundancy_compare,
    HolderAccount, HolderAccountClass, MeasurementFailureCategory, ParticipationSnapshot,
    RedundancyAudit, SnapshotClass, TradeObserved,
};

/// Frozen bounded capacity for the per-run Domain-1 trade-event queue. Chosen to
/// buffer bursts without unbounded growth; frozen before any pilot run.
pub const MEASUREMENT_TRADE_QUEUE_CAPACITY: usize = 4096;

/// Timestamp-architecture AMENDMENT-001 (frozen). `timestamp_semantics_version = 1`.
/// SOURCE CUTOFF and DECISION TIMESTAMP are separate: a feature's `available_in_time`
/// is evaluated against its registered decision timestamp (anchor + budget), never the
/// raw source cutoff. Feature formulas / trade inclusion are UNCHANGED.
pub const TIMESTAMP_SEMANTICS_VERSION: u32 = 2;
/// Domain-1 participation: T{2,6}_AVAILABLE = source cutoff + this budget (ms).
pub const PARTICIPATION_FINALIZATION_BUDGET_MS: i64 = 5;

// ---------------------------------------------------------------------------
// Bounded no-silent-drop queue
// ---------------------------------------------------------------------------

/// Explicit backpressure failure (never a silent drop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackpressureFailure {
    pub capacity: usize,
    pub depth_at_failure: usize,
}

/// Bounded FIFO for trade events. On overflow it does NOT overwrite-oldest and
/// does NOT drop-newest — it returns an explicit `BackpressureFailure` and counts
/// it. Tracks depth + high-water + overflow count for pilot telemetry.
#[derive(Debug, Clone)]
pub struct BoundedTradeQueue {
    buf: std::collections::VecDeque<TradeObserved>,
    capacity: usize,
    pub high_water: usize,
    pub overflow_count: u64,
    pub silent_drops: u64, // invariant: MUST remain 0
}

impl BoundedTradeQueue {
    pub fn new(capacity: usize) -> Self {
        Self { buf: std::collections::VecDeque::with_capacity(capacity), capacity, high_water: 0, overflow_count: 0, silent_drops: 0 }
    }
    pub fn depth(&self) -> usize {
        self.buf.len()
    }
    /// Push or return an explicit backpressure failure. Never silently drops.
    pub fn push(&mut self, t: TradeObserved) -> Result<(), BackpressureFailure> {
        if self.buf.len() >= self.capacity {
            self.overflow_count += 1;
            return Err(BackpressureFailure { capacity: self.capacity, depth_at_failure: self.buf.len() });
        }
        self.buf.push_back(t);
        if self.buf.len() > self.high_water {
            self.high_water = self.buf.len();
        }
        Ok(())
    }
    /// Drain all buffered events (e.g. into a candidate buffer at snapshot time).
    pub fn drain(&mut self) -> Vec<TradeObserved> {
        self.buf.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// Per-mint subscription + coverage state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionPhase {
    Requested,
    Active,
    Failed,
    Unsubscribed,
}

/// Coverage truth for a candidate's trade stream at snapshot time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageState {
    /// Subscription active and stream coverage known -> zero trades is LEGITIMATE zero-activity.
    Active,
    /// Subscription failed -> snapshot is MISSING/FAILURE, never zero.
    SubscriptionFailed,
    /// Stream interrupted/uncertain coverage -> MISSING/FAILURE, never zero.
    StreamCoverageUnknown,
}

impl CoverageState {
    /// Map runtime coverage to a frozen pure-layer failure category (None = usable/zero-activity).
    pub fn failure_category(self) -> Option<MeasurementFailureCategory> {
        match self {
            CoverageState::Active => None,
            CoverageState::SubscriptionFailed => Some(MeasurementFailureCategory::Other), // runtime SubscriptionFailed
            CoverageState::StreamCoverageUnknown => Some(MeasurementFailureCategory::StreamEventMissing),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MintSubscription {
    pub mint: String,
    pub phase: SubscriptionPhase,
    pub requested_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub failure: Option<MeasurementFailureCategory>,
    pub retry_count: u32,
    pub last_trade_received_at: Option<DateTime<Utc>>,
    pub unsubscribe_requested: bool,
    pub unsubscribe_confirmed: bool,
    pub reconnect_resubscribe_count: u32,
    pub coverage_known: bool,
}

impl MintSubscription {
    pub fn requested(mint: &str, at: DateTime<Utc>) -> Self {
        Self {
            mint: mint.to_string(), phase: SubscriptionPhase::Requested, requested_at: at, acknowledged_at: None,
            failure: None, retry_count: 0, last_trade_received_at: None, unsubscribe_requested: false,
            unsubscribe_confirmed: false, reconnect_resubscribe_count: 0, coverage_known: false,
        }
    }
    pub fn mark_active(&mut self, at: DateTime<Utc>) {
        self.phase = SubscriptionPhase::Active;
        self.acknowledged_at = Some(at);
        self.coverage_known = true;
    }
    pub fn mark_failed(&mut self, cat: MeasurementFailureCategory) {
        self.phase = SubscriptionPhase::Failed;
        self.failure = Some(cat);
        self.coverage_known = false;
    }
    pub fn note_trade(&mut self, at: DateTime<Utc>) {
        self.last_trade_received_at = Some(at);
    }
    pub fn mark_unsubscribed(&mut self, confirmed: bool) {
        self.unsubscribe_requested = true;
        self.unsubscribe_confirmed = confirmed;
        self.phase = SubscriptionPhase::Unsubscribed;
    }
    pub fn coverage_state(&self) -> CoverageState {
        match self.phase {
            SubscriptionPhase::Active => {
                if self.coverage_known { CoverageState::Active } else { CoverageState::StreamCoverageUnknown }
            }
            SubscriptionPhase::Failed => CoverageState::SubscriptionFailed,
            SubscriptionPhase::Requested | SubscriptionPhase::Unsubscribed => CoverageState::StreamCoverageUnknown,
        }
    }
}

/// Registry of per-mint subscriptions + trade buffers. Cleanup leaves no stale state.
#[derive(Debug, Default)]
pub struct SubscriptionRegistry {
    subs: std::collections::BTreeMap<String, MintSubscription>,
    buffers: std::collections::BTreeMap<String, Vec<TradeObserved>>,
    seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Tombstone of mints whose candidate lifecycle has terminated. Survives
    /// `cleanup` so a late trade is classified STALE (never resurrects state).
    terminated: std::collections::BTreeSet<String>,
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// UNCONDITIONAL subscribe request for a candidate mint (no inputs beyond the mint).
    pub fn request(&mut self, mint: &str, at: DateTime<Utc>) {
        self.subs.insert(mint.to_string(), MintSubscription::requested(mint, at));
        self.buffers.entry(mint.to_string()).or_default();
    }
    pub fn get_mut(&mut self, mint: &str) -> Option<&mut MintSubscription> {
        self.subs.get_mut(mint)
    }
    pub fn get(&self, mint: &str) -> Option<&MintSubscription> {
        self.subs.get(mint)
    }
    /// True if `sig` is NEW for `mint` (first receipt) — records it. A duplicate
    /// signature returns false so it is persisted at most once (earliest wins by
    /// receive order). Ingestion-time dedup complements the pure order-independent
    /// dedup used at snapshot derivation.
    pub fn accept_signature(&mut self, mint: &str, sig: &str) -> bool {
        self.seen.entry(mint.to_string()).or_default().insert(sig.to_string())
    }
    /// Is this mint an expected-active subscription (Requested or Active)?
    pub fn is_active(&self, mint: &str) -> bool {
        matches!(
            self.subs.get(mint).map(|s| s.phase),
            Some(SubscriptionPhase::Requested) | Some(SubscriptionPhase::Active)
        )
    }
    /// Route a normalized trade into its candidate buffer (drop-free; caller has
    /// already accepted it from the bounded queue). Updates last_trade.
    pub fn route_trade(&mut self, t: TradeObserved) {
        let at = t.event_received_at;
        let mint = t.mint.clone();
        self.buffers.entry(mint.clone()).or_default().push(t);
        if let Some(s) = self.subs.get_mut(&mint) {
            s.note_trade(at);
        }
    }
    pub fn buffer(&self, mint: &str) -> &[TradeObserved] {
        self.buffers.get(mint).map(|v| v.as_slice()).unwrap_or(&[])
    }
    /// Mark an unsubscribe as requested (and optionally confirmed) for a mint.
    /// Returns whether the mint was active/requested before this call. The actual
    /// provider command is issued by the caller; this only records lifecycle truth.
    pub fn unsubscribe(&mut self, mint: &str, confirmed: bool) -> bool {
        let was_active = self.is_active(mint);
        if let Some(s) = self.subs.get_mut(mint) {
            s.mark_unsubscribed(confirmed);
        }
        was_active
    }

    /// Record an unsubscribe failure explicitly (provider rejected/no ack). Keeps
    /// the request flag but does not pretend the candidate was cleanly removed.
    pub fn unsubscribe_failed(&mut self, mint: &str, cat: MeasurementFailureCategory) {
        if let Some(s) = self.subs.get_mut(mint) {
            s.unsubscribe_requested = true;
            s.unsubscribe_confirmed = false;
            s.failure = Some(cat);
        }
    }

    /// On disconnect: coverage for every active/requested subscription becomes
    /// UNKNOWN (the interruption is never later encoded as zero trades). Does NOT
    /// change the phase — a resubscribe on reconnect restores known coverage.
    pub fn mark_all_coverage_unknown(&mut self) {
        for s in self.subs.values_mut() {
            if matches!(s.phase, SubscriptionPhase::Active | SubscriptionPhase::Requested) {
                s.coverage_known = false;
            }
        }
    }

    /// True if this mint's candidate has terminated (tombstoned). Distinguishes a
    /// STALE trade (was tracked, now gone) from a never-subscribed unexpected trade.
    pub fn is_terminated(&self, mint: &str) -> bool {
        self.terminated.contains(mint)
    }

    /// Terminal cleanup: remove all state + buffer for a candidate and TOMBSTONE the
    /// mint (no stale state; late trades cannot resurrect it).
    pub fn cleanup(&mut self, mint: &str) {
        self.subs.remove(mint);
        self.buffers.remove(mint);
        self.seen.remove(mint);
        self.terminated.insert(mint.to_string());
    }
    /// Mints still active (for reconnect restore). Unconditional set.
    pub fn active_mints(&self) -> Vec<String> {
        self.subs
            .values()
            .filter(|s| matches!(s.phase, SubscriptionPhase::Active | SubscriptionPhase::Requested))
            .map(|s| s.mint.clone())
            .collect()
    }
    /// On reconnect: return ALL active-eligible mints to resubscribe (identical logic,
    /// never selective) and bump their reconnect counter.
    pub fn reconnect_resubscribe_all(&mut self) -> Vec<String> {
        let mints = self.active_mints();
        for m in &mints {
            if let Some(s) = self.subs.get_mut(m) {
                s.reconnect_resubscribe_count += 1;
                s.coverage_known = false; // coverage uncertain until resubscribed/acked
            }
        }
        mints
    }
}

// ---------------------------------------------------------------------------
// Coverage-aware participation snapshot
// ---------------------------------------------------------------------------

/// Build a T2/T6 ParticipationSnapshot honoring coverage truth: an ACTIVE stream
/// with zero eligible trades yields a legitimate zero-activity snapshot; a failed
/// subscription or unknown coverage yields a MISSING/FAILURE snapshot (failure set),
/// never encoded as zero activity.
#[allow(clippy::too_many_arguments)]
pub fn coverage_aware_snapshot(
    coverage: CoverageState,
    run_id: &str,
    mint: &str,
    class: SnapshotClass,
    cutoff: DateTime<Utc>,
    computed_at: DateTime<Utc>,
    decision_deadline: DateTime<Utc>,
    buffered: &[TradeObserved],
) -> ParticipationSnapshot {
    let mut snap = compute_participation_snapshot(run_id, mint, class, cutoff, computed_at, decision_deadline, buffered);
    if let Some(cat) = coverage.failure_category() {
        snap.failure = Some(cat); // MISSING/FAILURE, not zero-activity
    }
    snap
}

/// Frozen: subscribe attempt is UNCONDITIONAL (takes nothing; depends on nothing).
pub fn should_attempt_subscription() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Coverage truth (P3-COVERAGE-DEFECT-001): connection-generation + window
// continuity. "Subscription command sent" is NOT proof of coverage — a provider
// auth/stream error or disconnect invalidates coverage for all active mints, and a
// snapshot may only be VALID zero-activity if coverage was continuously known from
// the mint's initial establishment through the emit instant. Pure + testable.
// ---------------------------------------------------------------------------

/// A coverage-breaking runtime event (reliable signals only — NOT the overloaded
/// `Connected`, which the live client re-emits on every subscription resync).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageBreak {
    AuthError,
    ProviderError,
    StreamDisconnected,
}

#[derive(Debug, Clone)]
struct MintCoverage {
    /// Frozen at initial coverage establishment (first successful subscribe send).
    window_start: Option<DateTime<Utc>>,
    /// Generation on which this mint currently has known coverage.
    covered_generation: Option<u64>,
    /// Start of the CURRENT unbroken coverage epoch (reset to None on any break).
    coverage_start: Option<DateTime<Utc>>,
    /// Subscribe send failed outright.
    failed: bool,
}

/// Authoritative per-mint coverage-truth driven by a monotonic connection generation.
#[derive(Debug, Default)]
pub struct CoverageTracker {
    generation: u64,
    mints: std::collections::BTreeMap<String, MintCoverage>,
    pub auth_errors: u64,
    pub provider_stream_errors: u64,
    pub generation_changes: u64,
    pub coverage_invalidations: u64,
}

impl CoverageTracker {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Successful subscribe/resubscribe SEND for a mint on the current generation.
    /// `window_start` is frozen at the FIRST establishment; `coverage_start` is set to
    /// `at` when (re)entering active on the current generation, and is NOT reset by a
    /// same-generation resync (preserves continuity across desired-state replays).
    pub fn mark_active(&mut self, mint: &str, at: DateTime<Utc>) {
        let gen = self.generation;
        let e = self.mints.entry(mint.to_string()).or_insert(MintCoverage {
            window_start: None,
            covered_generation: None,
            coverage_start: None,
            failed: false,
        });
        e.failed = false;
        if e.window_start.is_none() {
            e.window_start = Some(at);
        }
        let already_active_this_gen = e.covered_generation == Some(gen) && e.coverage_start.is_some();
        if !already_active_this_gen {
            e.covered_generation = Some(gen);
            e.coverage_start = Some(at);
        }
    }

    /// Subscribe send failed outright (never became active).
    pub fn mark_failed(&mut self, mint: &str) {
        let e = self.mints.entry(mint.to_string()).or_insert(MintCoverage {
            window_start: None,
            covered_generation: None,
            coverage_start: None,
            failed: true,
        });
        e.failed = true;
        e.covered_generation = None;
        e.coverage_start = None;
    }

    /// A coverage break: bump the connection generation and invalidate coverage for
    /// EVERY currently-covered mint (they must be resubscribed on the new generation
    /// to regain coverage). An interruption+restore therefore cannot be rescued.
    pub fn on_break(&mut self, kind: CoverageBreak) {
        self.generation += 1;
        self.generation_changes += 1;
        match kind {
            CoverageBreak::AuthError => self.auth_errors += 1,
            CoverageBreak::ProviderError | CoverageBreak::StreamDisconnected => {
                self.provider_stream_errors += 1
            }
        }
        for c in self.mints.values_mut() {
            if c.covered_generation.is_some() || c.coverage_start.is_some() {
                c.covered_generation = None;
                c.coverage_start = None;
                self.coverage_invalidations += 1;
            }
        }
    }

    /// Coverage truth for a mint at the current (emit) instant. VALID (Active) only if:
    /// it is covered on the CURRENT generation AND the current coverage epoch began at
    /// or before the mint's initial establishment (i.e. no break since the window
    /// started). Otherwise UNKNOWN; an outright send failure is SubscriptionFailed.
    pub fn coverage_of(&self, mint: &str) -> CoverageState {
        match self.mints.get(mint) {
            None => CoverageState::StreamCoverageUnknown,
            Some(c) if c.failed => CoverageState::SubscriptionFailed,
            Some(c) => {
                let continuous = c.covered_generation == Some(self.generation)
                    && matches!((c.coverage_start, c.window_start), (Some(cs), Some(ws)) if cs <= ws);
                if continuous {
                    CoverageState::Active
                } else {
                    CoverageState::StreamCoverageUnknown
                }
            }
        }
    }

    /// Count mints currently not in known-good coverage (for telemetry).
    pub fn coverage_unknown_count(&self) -> u64 {
        self.mints
            .keys()
            .filter(|m| self.coverage_of(m) != CoverageState::Active)
            .count() as u64
    }

    pub fn cleanup(&mut self, mint: &str) {
        self.mints.remove(mint);
    }
}

// ---------------------------------------------------------------------------
// Domain 2/3 (2C): deterministic classification + redundancy glue (pure).
// ---------------------------------------------------------------------------

/// A largest-account row enriched with its on-chain SPL token-account OWNER, already
/// converted to UI-token balance (base / 10^decimals). `owner` is `None` when the
/// account could not be decoded (classified Unknown, never fabricated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHolderAccount {
    pub address: String,
    pub ui_balance: u64,
    pub owner: Option<String>,
}

/// Deterministically classify holders by AUTHORITATIVE token-account OWNER — NOT by
/// any derived-address / ATA assumption and NOT by balance size (P3-HOLDER-DEFECT-001):
/// - owner == the bonding-curve PDA  => CurveProgram (the reserve, wherever it lives)
/// - owner == the creator wallet     => Creator (only if `creator` is known)
/// - owner known but neither         => Ordinary
/// - owner undecodable               => Unknown (never fabricated as ordinary/curve)
///
/// Returns the classified accounts plus `curve_resolved` = whether the curve reserve
/// was authoritatively found among them. If it was not, the caller MUST treat
/// curve-exclusion features as MISSING (never zero-fill the curve share).
pub fn classify_holder_accounts_by_owner(
    accounts: &[RawHolderAccount],
    curve_pda: &str,
    creator: Option<&str>,
) -> (Vec<HolderAccount>, bool) {
    let mut curve_resolved = false;
    let classified = accounts
        .iter()
        .map(|a| {
            let class = match a.owner.as_deref() {
                Some(o) if o == curve_pda => {
                    curve_resolved = true;
                    HolderAccountClass::CurveProgram
                }
                Some(o) if creator == Some(o) => HolderAccountClass::Creator,
                Some(_) => HolderAccountClass::Ordinary,
                None => HolderAccountClass::Unknown,
            };
            HolderAccount { address: a.address.clone(), raw_balance_tokens: a.ui_balance, class }
        })
        .collect();
    (classified, curve_resolved)
}

/// Redundancy audit for one microstructure probe: compare the probe's total expected
/// UI tokens out to the bonding-curve-implied UI tokens out for the same SOL input
/// (ex-fee reference from `curve_implied_tokens_out`). Returns `None` when inputs are
/// unusable. This is the audited derivation path (executability check, NOT an
/// expectancy/outcome signal), reproducible from the persisted raw probe + reserves.
pub fn microstructure_redundancy(
    expected_base_raw: u64,
    input_lamports: u64,
    base_decimals: u8,
    v_sol: f64,
    v_tokens: f64,
) -> Option<RedundancyAudit> {
    if input_lamports == 0 {
        return None;
    }
    let probe_ui = expected_base_raw as f64 / 10f64.powi(base_decimals as i32);
    let sol_in = input_lamports as f64 / 1_000_000_000.0;
    let curve_ui = curve_implied_tokens_out(v_sol, v_tokens, sol_in)?;
    Some(redundancy_compare(probe_ui, curve_ui))
}

// ---------------------------------------------------------------------------
// Shared per-mint participation state (2B): produced by the main intake loop,
// consumed at T2/T6 by the per-candidate task. Pure + deterministic + sink-free
// so it is fully unit-testable; the bin wraps it in a Mutex behind the sink.
// ---------------------------------------------------------------------------

/// Stable idempotence key: (mint, snapshot-class discriminant).
fn snapshot_class_key(class: SnapshotClass) -> u8 {
    match class {
        SnapshotClass::T2 => 2,
        SnapshotClass::T6 => 6,
    }
}

/// Per-mint runtime state needed to derive T2/T6 ParticipationSnapshots:
/// the accumulated TradeObserved buffer, the coverage truth at snapshot time,
/// and a once-only emission guard. NO cross-mint / cross-run contamination:
/// every entry is mint-keyed and dropped on candidate cleanup.
#[derive(Debug, Default)]
pub struct ParticipationState {
    buffers: std::collections::BTreeMap<String, Vec<TradeObserved>>,
    coverage: std::collections::BTreeMap<String, CoverageState>,
    emitted: std::collections::BTreeSet<(String, u8)>,
}

impl ParticipationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an accepted (already ingestion-deduped) trade to its mint buffer.
    /// The pure snapshot layer additionally applies order-independent earliest-wins
    /// dedup at derivation, so a stray duplicate here cannot inflate features.
    pub fn record_trade(&mut self, t: TradeObserved) {
        self.buffers.entry(t.mint.clone()).or_default().push(t);
    }

    /// Set/refresh coverage truth for a mint (subscribe ack/failure/reconnect).
    pub fn set_coverage(&mut self, mint: &str, cov: CoverageState) {
        self.coverage.insert(mint.to_string(), cov);
    }

    /// Coverage for a mint; a mint with no recorded coverage is UNKNOWN (never Active).
    pub fn coverage_of(&self, mint: &str) -> CoverageState {
        self.coverage.get(mint).copied().unwrap_or(CoverageState::StreamCoverageUnknown)
    }

    pub fn buffer_of(&self, mint: &str) -> &[TradeObserved] {
        self.buffers.get(mint).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Build the final T2/T6 snapshot for a mint ONCE. Returns `None` if a final
    /// snapshot of this class was already emitted (duplicate scheduling / retry).
    /// `decision_deadline == cutoff` (frozen: a computation completing after the
    /// decision cutoff cannot back the corresponding decision timestamp), so
    /// `available_in_time` reflects real-time computability, never a backdate.
    pub fn build_snapshot(
        &mut self,
        run_id: &str,
        mint: &str,
        class: SnapshotClass,
        cutoff: DateTime<Utc>,
        computed_at: DateTime<Utc>,
    ) -> Option<ParticipationSnapshot> {
        let key = (mint.to_string(), snapshot_class_key(class));
        if self.emitted.contains(&key) {
            return None; // final snapshot of this class already emitted
        }
        self.emitted.insert(key);
        let cov = self.coverage_of(mint);
        let buf = self.buffers.get(mint).map(|v| v.as_slice()).unwrap_or(&[]);
        // Timestamp architecture AMENDMENT-001: source cutoff is UNCHANGED (feature/trade
        // inclusion still filter by `cutoff`); the DECISION timestamp is T{2,6}_AVAILABLE =
        // cutoff + PARTICIPATION_FINALIZATION_BUDGET_MS, so available_in_time reflects the
        // registered decision timestamp, not the raw cutoff.
        let decision_deadline = cutoff + chrono::Duration::milliseconds(PARTICIPATION_FINALIZATION_BUDGET_MS);
        Some(coverage_aware_snapshot(cov, run_id, mint, class, cutoff, computed_at, decision_deadline, buf))
    }

    /// True if a final snapshot of this class was already emitted for the mint.
    pub fn already_emitted(&self, mint: &str, class: SnapshotClass) -> bool {
        self.emitted.contains(&(mint.to_string(), snapshot_class_key(class)))
    }

    /// Terminal cleanup: drop ALL participation state for a mint (buffer, coverage,
    /// emission guards). Leaves no stale candidate mapping.
    pub fn cleanup(&mut self, mint: &str) {
        self.buffers.remove(mint);
        self.coverage.remove(mint);
        self.emitted.retain(|(m, _)| m != mint);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::measurement::{holder_features, TradeSide};
    use chrono::TimeZone;

    fn ts(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }
    fn trade(sig: &str, mint: &str, recv: i64) -> TradeObserved {
        TradeObserved {
            schema_version: 4, normalization_version: 1, run_id: "r".into(), mint: mint.into(),
            signature: sig.into(), trader_public_key: "W".into(), tx_type: "buy".into(), side: TradeSide::Buy,
            token_amount_ui: 1000.0, sol_amount: 1.0, bonding_curve_key: "BC".into(),
            v_tokens_in_bonding_curve: 1e9, v_sol_in_bonding_curve: 30.0, market_cap_sol: 28.0,
            event_received_at: ts(recv), source: "pumpportal".into(), source_revision: "260a65d".into(),
        }
    }

    #[test] // bounded queue accepts up to capacity
    fn q_accepts_to_capacity() {
        let mut q = BoundedTradeQueue::new(2);
        assert!(q.push(trade("a", "M", 1)).is_ok());
        assert!(q.push(trade("b", "M", 1)).is_ok());
        assert_eq!(q.depth(), 2);
        assert_eq!(q.high_water, 2);
    }

    #[test] // overflow => explicit backpressure failure, NO silent drop
    fn q_overflow_explicit() {
        let mut q = BoundedTradeQueue::new(1);
        assert!(q.push(trade("a", "M", 1)).is_ok());
        let err = q.push(trade("b", "M", 1)).unwrap_err();
        assert_eq!(err.capacity, 1);
        assert_eq!(q.overflow_count, 1);
        assert_eq!(q.silent_drops, 0); // invariant
        assert_eq!(q.depth(), 1); // newest not dropped-in, oldest not overwritten
    }

    #[test] // drain empties and preserves order
    fn q_drain() {
        let mut q = BoundedTradeQueue::new(4);
        q.push(trade("a", "M", 1)).unwrap();
        q.push(trade("b", "M", 2)).unwrap();
        let d = q.drain();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].signature, "a");
        assert_eq!(q.depth(), 0);
    }

    #[test] // subscription lifecycle transitions are explicit
    fn sub_lifecycle() {
        let mut s = MintSubscription::requested("M", ts(0));
        assert_eq!(s.phase, SubscriptionPhase::Requested);
        s.mark_active(ts(1));
        assert_eq!(s.phase, SubscriptionPhase::Active);
        assert!(s.coverage_known);
        s.note_trade(ts(2));
        assert_eq!(s.last_trade_received_at, Some(ts(2)));
        s.mark_unsubscribed(true);
        assert_eq!(s.phase, SubscriptionPhase::Unsubscribed);
        assert!(s.unsubscribe_confirmed);
    }

    #[test] // failed subscription => not coverage-known
    fn sub_failed_coverage() {
        let mut s = MintSubscription::requested("M", ts(0));
        s.mark_failed(MeasurementFailureCategory::Timeout);
        assert_eq!(s.coverage_state(), CoverageState::SubscriptionFailed);
        assert_eq!(s.failure, Some(MeasurementFailureCategory::Timeout));
    }

    #[test] // registry request is unconditional + routes trades to buffers
    fn registry_route() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        reg.get_mut("M").unwrap().mark_active(ts(1));
        reg.route_trade(trade("a", "M", 2));
        reg.route_trade(trade("b", "M", 3));
        assert_eq!(reg.buffer("M").len(), 2);
        assert_eq!(reg.get("M").unwrap().last_trade_received_at, Some(ts(3)));
    }

    #[test] // cleanup leaves no stale state or buffer
    fn registry_cleanup() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        reg.route_trade(trade("a", "M", 1));
        reg.cleanup("M");
        assert!(reg.get("M").is_none());
        assert_eq!(reg.buffer("M").len(), 0);
    }

    #[test] // reconnect restores ALL active mints (never selective) + bumps counter
    fn reconnect_restores_all() {
        let mut reg = SubscriptionRegistry::new();
        for m in ["A", "B", "C"] {
            reg.request(m, ts(0));
            reg.get_mut(m).unwrap().mark_active(ts(1));
        }
        let restored = reg.reconnect_resubscribe_all();
        assert_eq!(restored.len(), 3);
        assert_eq!(reg.get("A").unwrap().reconnect_resubscribe_count, 1);
        assert!(!reg.get("A").unwrap().coverage_known); // uncertain until re-acked
    }

    #[test] // active stream + zero trades => legitimate zero-activity (failure None, buy_count 0)
    fn coverage_active_zero_is_legit() {
        let snap = coverage_aware_snapshot(CoverageState::Active, "r", "M", SnapshotClass::T6, ts(6), ts(6), ts(9), &[]);
        assert!(snap.failure.is_none());
        assert_eq!(snap.features.buy_count, 0);
    }

    #[test] // subscription failed => MISSING/FAILURE snapshot, not zero
    fn coverage_failed_is_missing() {
        let snap = coverage_aware_snapshot(CoverageState::SubscriptionFailed, "r", "M", SnapshotClass::T6, ts(6), ts(6), ts(9), &[]);
        assert!(snap.failure.is_some());
    }

    #[test] // unknown coverage => MISSING/FAILURE (StreamEventMissing)
    fn coverage_unknown_is_missing() {
        let snap = coverage_aware_snapshot(CoverageState::StreamCoverageUnknown, "r", "M", SnapshotClass::T2, ts(2), ts(2), ts(3), &[trade("a", "M", 1)]);
        assert_eq!(snap.failure, Some(MeasurementFailureCategory::StreamEventMissing));
    }

    #[test] // coverage-aware snapshot still honors the T2 cutoff on the buffer
    fn coverage_snapshot_respects_cutoff() {
        let buf = vec![trade("a", "M", 1), trade("b", "M", 9)];
        let snap = coverage_aware_snapshot(CoverageState::Active, "r", "M", SnapshotClass::T2, ts(2), ts(2), ts(9), &buf);
        assert_eq!(snap.features.buy_count, 1); // only recv=1 <= cutoff 2
    }

    #[test] // subscription attempt is unconditional
    fn attempt_unconditional() {
        assert!(should_attempt_subscription());
    }

    #[test] // frozen queue capacity
    fn frozen_capacity() {
        assert_eq!(MEASUREMENT_TRADE_QUEUE_CAPACITY, 4096);
    }

    #[test] // ingestion signature dedup: earliest wins, duplicate rejected; is_active
    fn accept_signature_dedup() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        assert!(reg.is_active("M"));
        assert!(reg.accept_signature("M", "sig1")); // new
        assert!(!reg.accept_signature("M", "sig1")); // duplicate rejected
        assert!(reg.accept_signature("M", "sig2")); // different new
        reg.cleanup("M");
        assert!(!reg.is_active("M"));
        assert!(reg.accept_signature("M", "sig1")); // cleared after cleanup
    }

    // --- 2B: ParticipationState (shared per-mint buffer + T2/T6 emission) ---

    #[test] // per-mint buffer isolates candidates; A's trades never enter B
    fn ps_buffer_isolates_mints() {
        let mut ps = ParticipationState::new();
        ps.record_trade(trade("a", "A", 1));
        ps.record_trade(trade("b", "A", 2));
        ps.record_trade(trade("c", "B", 1));
        assert_eq!(ps.buffer_of("A").len(), 2);
        assert_eq!(ps.buffer_of("B").len(), 1);
        assert!(ps.buffer_of("A").iter().all(|t| t.mint == "A"));
        assert!(ps.buffer_of("B").iter().all(|t| t.mint == "B"));
    }

    #[test] // duplicate signature => earliest receipt retained at snapshot derivation
    fn ps_duplicate_sig_earliest_retained() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        ps.record_trade(trade("dup", "M", 1)); // earliest
        ps.record_trade(trade("dup", "M", 5)); // later duplicate
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T6, ts(9), ts(9)).unwrap();
        assert_eq!(snap.source_event_count, 2);
        assert_eq!(snap.deduped_event_count, 1); // earliest wins, one kept
    }

    #[test] // T2 uses the provided actual sampled_at cutoff; post-cutoff trade excluded
    fn ps_t2_cutoff_excludes_post() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        ps.record_trade(trade("a", "M", 1)); // <= cutoff 2
        ps.record_trade(trade("b", "M", 9)); // > cutoff 2 => excluded
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).unwrap();
        assert_eq!(snap.cutoff_timestamp, ts(2));
        assert_eq!(snap.features.buy_count, 1); // only the pre-cutoff trade
    }

    #[test] // pre-cutoff received trade is included even if recorded/processed later in call order
    fn ps_pre_cutoff_included_regardless_of_record_order() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        // record the post-cutoff trade FIRST, the pre-cutoff trade LATER: inclusion
        // depends on event_received_at, not record/processing order.
        ps.record_trade(trade("late", "M", 9));
        ps.record_trade(trade("early", "M", 1));
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        assert_eq!(snap.features.buy_count, 1);
    }

    #[test] // active stream + zero trades => VALID zero-activity snapshot (not missing)
    fn ps_active_zero_is_valid() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).unwrap();
        assert!(snap.failure.is_none());
        assert_eq!(snap.features.buy_count, 0);
        assert_eq!(snap.features.unique_buyers, 0);
    }

    #[test] // subscription failure => MISSING/FAILURE, never encoded as zero
    fn ps_failed_is_missing() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::SubscriptionFailed);
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        assert!(snap.failure.is_some());
    }

    #[test] // no recorded coverage => UNKNOWN => MISSING/FAILURE (not silent-zero)
    fn ps_unknown_coverage_is_missing() {
        let mut ps = ParticipationState::new();
        assert_eq!(ps.coverage_of("M"), CoverageState::StreamCoverageUnknown);
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).unwrap();
        assert_eq!(snap.failure, Some(MeasurementFailureCategory::StreamEventMissing));
    }

    #[test] // zero-activity (active) is distinguishable from acquisition failure
    fn ps_zero_vs_failure_distinguishable() {
        let mut a = ParticipationState::new();
        a.set_coverage("M", CoverageState::Active);
        let mut f = ParticipationState::new();
        f.set_coverage("M", CoverageState::SubscriptionFailed);
        let za = a.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        let fa = f.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        assert!(za.failure.is_none() && za.features.buy_count == 0);
        assert!(fa.failure.is_some());
        assert_ne!(za.failure.is_some(), fa.failure.is_some());
    }

    #[test] // final T2 emitted once; duplicate scheduling does not re-emit
    fn ps_t2_emitted_once() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).is_some());
        assert!(ps.already_emitted("M", SnapshotClass::T2));
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).is_none()); // idempotent
    }

    #[test] // final T6 emitted once; T2 and T6 are independent classes
    fn ps_t6_emitted_once_independent_of_t2() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).is_some());
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).is_some()); // distinct class
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).is_none()); // T6 idempotent
    }

    #[test] // runtime snapshot == pure-layer replay from same buffer + cutoff (T2 and T6)
    fn ps_runtime_equals_pure_replay() {
        let buf = vec![trade("a", "M", 1), trade("b", "M", 3), trade("c", "M", 9)];
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        for t in &buf {
            ps.record_trade(t.clone());
        }
        // T2
        let rt2 = ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(5)).unwrap();
        let pure2 =
            coverage_aware_snapshot(CoverageState::Active, "r", "M", SnapshotClass::T2, ts(2), ts(5), ts(2), &buf);
        assert_eq!(rt2.features.buy_count, pure2.features.buy_count);
        assert_eq!(rt2.deduped_event_count, pure2.deduped_event_count);
        assert_eq!(rt2.cutoff_timestamp, pure2.cutoff_timestamp);
        assert_eq!(rt2.available_in_time, pure2.available_in_time);
        // T6
        let rt6 = ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        let pure6 =
            coverage_aware_snapshot(CoverageState::Active, "r", "M", SnapshotClass::T6, ts(6), ts(6), ts(6), &buf);
        assert_eq!(rt6.features.buy_count, pure6.features.buy_count);
        assert_eq!(rt6.features.net_quote_flow_sol, pure6.features.net_quote_flow_sol);
        assert_eq!(rt6.available_in_time, pure6.available_in_time);
    }

    #[test] // available_in_time is truthful: computed after the decision cutoff => false
    fn ps_available_in_time_truthful() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        // computed strictly after cutoff => not available in time (no backdating)
        let late = ps.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(7)).unwrap();
        assert!(!late.available_in_time);
        let mut ps2 = ParticipationState::new();
        ps2.set_coverage("M", CoverageState::Active);
        let on_time = ps2.build_snapshot("r", "M", SnapshotClass::T6, ts(6), ts(6)).unwrap();
        assert!(on_time.available_in_time);
    }

    #[test] // cleanup removes buffer, coverage, and emission guard for the candidate
    fn ps_cleanup_removes_state() {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        ps.record_trade(trade("a", "M", 1));
        assert!(ps.build_snapshot("r", "M", SnapshotClass::T2, ts(2), ts(2)).is_some());
        ps.cleanup("M");
        assert_eq!(ps.buffer_of("M").len(), 0);
        assert_eq!(ps.coverage_of("M"), CoverageState::StreamCoverageUnknown);
        assert!(!ps.already_emitted("M", SnapshotClass::T2)); // emission guard cleared
    }

    // --- AMENDMENT-001: T2/T6_AVAILABLE = cutoff + 5ms (decision timestamp) ---

    #[test] // budget constant + semantics version frozen
    fn ts_arch_constants_frozen() {
        assert_eq!(PARTICIPATION_FINALIZATION_BUDGET_MS, 5);
        assert_eq!(TIMESTAMP_SEMANTICS_VERSION, 2);
    }

    fn ps_active(cutoff: DateTime<Utc>, computed_at: DateTime<Utc>, class: SnapshotClass) -> ParticipationSnapshot {
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        ps.build_snapshot("r", "M", class, cutoff, computed_at).unwrap()
    }

    #[test] // computed +0.3ms after cutoff -> available (was false under strict <=cutoff)
    fn avail_t2_plus_300us_true() {
        let c = ts(2);
        assert!(ps_active(c, c + chrono::Duration::microseconds(300), SnapshotClass::T2).available_in_time);
    }
    #[test] // computed +4.9ms -> available true; boundary +5ms exact -> true
    fn avail_t2_within_budget_true() {
        let c = ts(2);
        assert!(ps_active(c, c + chrono::Duration::microseconds(4900), SnapshotClass::T2).available_in_time);
        assert!(ps_active(c, c + chrono::Duration::milliseconds(5), SnapshotClass::T2).available_in_time);
    }
    #[test] // computed +5.1ms -> NOT available
    fn avail_t2_over_budget_false() {
        let c = ts(2);
        assert!(!ps_active(c, c + chrono::Duration::microseconds(5100), SnapshotClass::T2).available_in_time);
    }
    #[test] // T6 same boundary behavior
    fn avail_t6_boundary() {
        let c = ts(6);
        assert!(ps_active(c, c + chrono::Duration::microseconds(4000), SnapshotClass::T6).available_in_time);
        assert!(!ps_active(c, c + chrono::Duration::milliseconds(6), SnapshotClass::T6).available_in_time);
    }
    #[test] // +5ms budget does NOT admit post-cutoff trades (source cutoff unchanged)
    fn avail_budget_does_not_admit_post_cutoff_trades() {
        let c = ts(2);
        let mut ps = ParticipationState::new();
        ps.set_coverage("M", CoverageState::Active);
        ps.record_trade(trade("a", "M", 1)); // <= cutoff (ts 1)
        // a trade received AFTER cutoff (even within the +5ms availability budget) must
        // NOT enter the features — inclusion is still event_received_at <= source cutoff.
        ps.record_trade(trade("b", "M", 3)); // > cutoff ts 2
        let snap = ps.build_snapshot("r", "M", SnapshotClass::T2, c, c + chrono::Duration::milliseconds(3)).unwrap();
        assert!(snap.available_in_time);
        assert_eq!(snap.features.buy_count, 1); // only the pre-cutoff trade
    }

    #[test] // cleanup of one candidate does not disturb another
    fn ps_cleanup_isolated() {
        let mut ps = ParticipationState::new();
        ps.record_trade(trade("a", "A", 1));
        ps.record_trade(trade("b", "B", 1));
        ps.cleanup("A");
        assert_eq!(ps.buffer_of("A").len(), 0);
        assert_eq!(ps.buffer_of("B").len(), 1);
    }

    // --- 2C: holder classification + microstructure redundancy (pure) ---

    fn raw(addr: &str, bal: u64, owner: Option<&str>) -> RawHolderAccount {
        RawHolderAccount { address: addr.into(), ui_balance: bal, owner: owner.map(|s| s.to_string()) }
    }

    #[test] // classify by OWNER: curve reserve (owned by curve PDA), creator, ordinary
    fn classify_by_owner_curve_creator_ordinary() {
        // Reproduces the 2E live shape: the curve reserve's ADDRESS is unrelated to
        // any ATA derivation — only its OWNER (the curve PDA) identifies it.
        let accts = vec![
            raw("RESERVE_ADDR_NOT_AN_ATA", 793_100_000, Some("CURVE_PDA")),
            raw("CREATOR_ATA", 6_000_000, Some("CREATOR_WALLET")),
            raw("WHALE_ATA", 900_000, Some("SOME_WALLET")),
        ];
        let (out, curve_resolved) = classify_holder_accounts_by_owner(&accts, "CURVE_PDA", Some("CREATOR_WALLET"));
        assert!(curve_resolved);
        assert_eq!(out[0].class, HolderAccountClass::CurveProgram); // matched by owner, not address
        assert_eq!(out[1].class, HolderAccountClass::Creator);
        assert_eq!(out[2].class, HolderAccountClass::Ordinary);
    }

    #[test] // creator unknown => no Creator label; undecodable owner => Unknown
    fn classify_by_owner_creator_blocked_and_unknown() {
        let accts = vec![
            raw("R", 1, Some("CURVE_PDA")),
            raw("C", 2, Some("CREATOR_WALLET")),
            raw("U", 3, None),
        ];
        let (out, curve_resolved) = classify_holder_accounts_by_owner(&accts, "CURVE_PDA", None);
        assert!(curve_resolved);
        assert!(out.iter().all(|a| a.class != HolderAccountClass::Creator));
        assert_eq!(out[2].class, HolderAccountClass::Unknown);
    }

    // --- P3-HOLDER-DEFECT-001 remediation regression (from the 2E live shape) ---

    #[test] // 1/3/8/9/10: authoritative owner match classifies the curve reserve as
    // curve/program, and the ~99.65% reserve is EXCLUDED from non-curve top-k shares
    fn remediation_curve_reserve_excluded_from_noncurve() {
        // 2E live shape (curve reserve ~99.65% supply + creator) plus one small
        // ordinary holder so the non-curve population is non-empty.
        let accts = vec![
            raw("RESERVE", 996_479_081, Some("CURVE_PDA")), // the top1 that WAS mislabeled ordinary
            raw("CREATOR_ATA", 3_000_000, Some("CREATOR_WALLET")),
            raw("WHALE_ATA", 2_000_000, Some("WALLET_X")),
        ];
        let (classified, curve_resolved) = classify_holder_accounts_by_owner(&accts, "CURVE_PDA", Some("CREATOR_WALLET"));
        assert!(curve_resolved);
        // The old ATA assumption would have left RESERVE as Ordinary -> top1 ~0.9965.
        assert_eq!(classified[0].class, HolderAccountClass::CurveProgram);
        let feats = holder_features(&classified, true);
        // Curve reserve EXCLUDED from non-curve top-k: top1 is the whale (~0.002), not 0.9965.
        assert!(feats.top1_noncurve_holder_share.unwrap() < 0.01, "curve reserve must be excluded from non-curve top1");
        assert!(feats.top5_noncurve_holder_share.unwrap() < 0.01);
        assert!(feats.top10_noncurve_holder_share.unwrap() < 0.01);
        // curve_held_share reflects the RESOLVED reserve, not zero.
        assert!(feats.curve_held_share.unwrap() > 0.99);
    }

    #[test] // 6/7: unresolved curve => caller must treat as MISSING, never zero curve share
    fn remediation_unresolved_curve_flag() {
        // No account is owned by the curve PDA (e.g. reserve not among returned rows).
        let accts = vec![
            raw("A", 500_000_000, Some("WALLET_A")),
            raw("B", 400_000_000, Some("WALLET_B")),
        ];
        let (_classified, curve_resolved) = classify_holder_accounts_by_owner(&accts, "CURVE_PDA", None);
        assert!(!curve_resolved, "curve must be reported UNRESOLVED so caller emits MISSING/FAILURE, not zero curve share");
    }

    // --- P3-COVERAGE-DEFECT-001: connection-generation coverage truth ---

    #[test] // clean subscribe, no break => ACTIVE_KNOWN (valid zero-activity allowed)
    fn cov_clean_active() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0));
        assert_eq!(c.coverage_of("M"), CoverageState::Active);
        assert_eq!(c.generation(), 0);
    }

    #[test] // send failure => SubscriptionFailed (never Active)
    fn cov_send_failed() {
        let mut c = CoverageTracker::new();
        c.mark_failed("M");
        assert_eq!(c.coverage_of("M"), CoverageState::SubscriptionFailed);
    }

    #[test] // auth error invalidates all active mints -> UNKNOWN; generation bumps
    fn cov_auth_error_invalidates() {
        let mut c = CoverageTracker::new();
        c.mark_active("A", ts(0));
        c.mark_active("B", ts(0));
        c.on_break(CoverageBreak::AuthError);
        assert_eq!(c.generation(), 1);
        assert_eq!(c.coverage_of("A"), CoverageState::StreamCoverageUnknown);
        assert_eq!(c.coverage_of("B"), CoverageState::StreamCoverageUnknown);
        assert_eq!(c.auth_errors, 1);
        assert_eq!(c.coverage_invalidations, 2);
    }

    #[test] // stream disconnect invalidates; generation change tracked
    fn cov_disconnect_invalidates() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0));
        c.on_break(CoverageBreak::StreamDisconnected);
        assert_eq!(c.coverage_of("M"), CoverageState::StreamCoverageUnknown);
        assert_eq!(c.generation_changes, 1);
    }

    #[test] // same-generation resync does NOT reset coverage_start (continuity preserved)
    fn cov_resync_preserves_continuity() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0)); // window_start=0, coverage_start=0
        c.mark_active("M", ts(5)); // resync same gen -> coverage_start stays 0
        assert_eq!(c.coverage_of("M"), CoverageState::Active);
    }

    #[test] // CRITICAL CASE: subscribe@0, break@0.8, resubscribe@1.5 -> T2 NOT valid
    fn cov_interruption_then_restore_is_unknown() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0)); // window_start=0
        c.on_break(CoverageBreak::AuthError); // gen1, invalidate
        c.mark_active("M", ts(2)); // restore on gen1, coverage_start=2 > window_start 0
        // evaluated at emit: covered on current gen BUT epoch started after window_start
        assert_eq!(c.coverage_of("M"), CoverageState::StreamCoverageUnknown);
    }

    #[test] // failed resubscribe after break remains unknown/failed
    fn cov_failed_resubscribe() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0));
        c.on_break(CoverageBreak::AuthError);
        c.mark_failed("M");
        assert_eq!(c.coverage_of("M"), CoverageState::SubscriptionFailed);
    }

    #[test] // T2 valid before break, T6 invalid after break (evaluated at emit time)
    fn cov_t2_valid_t6_missing_across_break() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0));
        // T2 emitted at ~2s, before any break:
        assert_eq!(c.coverage_of("M"), CoverageState::Active); // T2 valid
        c.on_break(CoverageBreak::AuthError); // break between T2 and T6
        // T6 emitted at ~6s, after the break, no resubscribe:
        assert_eq!(c.coverage_of("M"), CoverageState::StreamCoverageUnknown); // T6 missing
    }

    #[test] // cleanup removes coverage; a fresh candidate later is independent
    fn cov_cleanup() {
        let mut c = CoverageTracker::new();
        c.mark_active("M", ts(0));
        c.cleanup("M");
        assert_eq!(c.coverage_of("M"), CoverageState::StreamCoverageUnknown);
    }

    #[test] // PILOT REPRODUCTION: subscribes + repeated auth errors + no trades =>
    // NO valid zero-activity snapshots (the exact B2 failure shape)
    fn cov_pilot_auth_storm_no_valid_zero() {
        let mut c = CoverageTracker::new();
        // 3 candidates subscribed, then the provider auth-errors (as in the pilot).
        for m in ["A", "B", "C"] {
            c.mark_active(m, ts(0));
        }
        // Provider rejects the metered stream repeatedly; connection never cleanly drops.
        for _ in 0..5 {
            c.on_break(CoverageBreak::AuthError);
        }
        // Even if the client resyncs/resubscribes on a later generation:
        c.mark_active("A", ts(3));
        for m in ["A", "B", "C"] {
            assert_eq!(
                c.coverage_of(m),
                CoverageState::StreamCoverageUnknown,
                "auth-storm window must be UNKNOWN, never valid zero-activity"
            );
        }
        assert!(c.auth_errors >= 5);
        assert_eq!(c.coverage_unknown_count(), 3);
    }

    #[test] // 15: no balance-size heuristic — the biggest balance is NOT auto-curve
    fn remediation_no_balance_heuristic() {
        // Biggest balance is an ordinary wallet; the small one is the real curve.
        let accts = vec![
            raw("BIG_WALLET", 900_000_000, Some("WALLET_X")),
            raw("SMALL_RESERVE", 10_000_000, Some("CURVE_PDA")),
        ];
        let (out, curve_resolved) = classify_holder_accounts_by_owner(&accts, "CURVE_PDA", None);
        assert!(curve_resolved);
        assert_eq!(out[0].class, HolderAccountClass::Ordinary); // biggest is NOT curve
        assert_eq!(out[1].class, HolderAccountClass::CurveProgram); // owner decides, not size
    }

    #[test] // redundancy: probe matching curve-implied within tolerance => Redundant
    fn redundancy_matches_curve() {
        // curve_implied_tokens_out(v_sol=30, v_tokens=1_073_000_000, sol_in=0.5)
        let sol_in = 0.5;
        let curve_ui = curve_implied_tokens_out(30.0, 1_073_000_000.0, sol_in).unwrap();
        // build a probe whose expected UI tokens ~= curve_ui (6 decimals)
        let expected_base_raw = (curve_ui * 1e6) as u64;
        let audit =
            microstructure_redundancy(expected_base_raw, 500_000_000, 6, 30.0, 1_073_000_000.0).unwrap();
        assert!(audit.within_tolerance);
        assert_eq!(audit.class, super::super::measurement::RedundancyClass::Redundant);
    }

    #[test] // redundancy: large divergence => NonRedundant; zero input => None
    fn redundancy_divergent_and_guarded() {
        let audit =
            microstructure_redundancy(1, 500_000_000, 6, 30.0, 1_073_000_000.0).unwrap();
        assert!(!audit.within_tolerance);
        assert_eq!(audit.class, super::super::measurement::RedundancyClass::NonRedundant);
        assert!(microstructure_redundancy(1000, 0, 6, 30.0, 1e9).is_none());
    }

    // --- 2D: subscription lifecycle (unsubscribe, coverage-unknown, tombstone) ---

    #[test] // unsubscribe clears active state and reports prior activity
    fn unsub_clears_active() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        reg.get_mut("M").unwrap().mark_active(ts(1));
        assert!(reg.is_active("M"));
        assert!(reg.unsubscribe("M", true));
        assert!(!reg.is_active("M"));
        assert!(reg.get("M").unwrap().unsubscribe_confirmed);
    }

    #[test] // unsubscribe failure is explicit; not pretended clean
    fn unsub_failure_explicit() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        reg.get_mut("M").unwrap().mark_active(ts(1));
        reg.unsubscribe_failed("M", MeasurementFailureCategory::Timeout);
        let s = reg.get("M").unwrap();
        assert!(s.unsubscribe_requested);
        assert!(!s.unsubscribe_confirmed);
        assert_eq!(s.failure, Some(MeasurementFailureCategory::Timeout));
    }

    #[test] // disconnect marks active candidates coverage-unknown (never zero-filled)
    fn disconnect_marks_coverage_unknown() {
        let mut reg = SubscriptionRegistry::new();
        for m in ["A", "B"] {
            reg.request(m, ts(0));
            reg.get_mut(m).unwrap().mark_active(ts(1));
        }
        reg.mark_all_coverage_unknown();
        assert_eq!(reg.get("A").unwrap().coverage_state(), CoverageState::StreamCoverageUnknown);
        assert_eq!(reg.get("B").unwrap().coverage_state(), CoverageState::StreamCoverageUnknown);
        // reconnect resubscribe leaves them still uncertain until re-acked
        let restored = reg.reconnect_resubscribe_all();
        assert_eq!(restored.len(), 2);
        // re-ack restores known coverage from that point forward
        reg.get_mut("A").unwrap().mark_active(ts(9));
        assert_eq!(reg.get("A").unwrap().coverage_state(), CoverageState::Active);
    }

    #[test] // cleanup tombstones the mint so a late trade is STALE, never resurrected
    fn cleanup_tombstones_mint() {
        let mut reg = SubscriptionRegistry::new();
        reg.request("M", ts(0));
        reg.get_mut("M").unwrap().mark_active(ts(1));
        reg.cleanup("M");
        assert!(!reg.is_active("M"));
        assert!(reg.is_terminated("M"));
        // a never-seen mint is neither active nor terminated
        assert!(!reg.is_active("Z"));
        assert!(!reg.is_terminated("Z"));
    }
}
