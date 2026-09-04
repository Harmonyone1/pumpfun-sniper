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
    /// Terminal cleanup: remove all state + buffer for a candidate (no stale state).
    pub fn cleanup(&mut self, mint: &str) {
        self.subs.remove(mint);
        self.buffers.remove(mint);
        self.seen.remove(mint);
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
// Domain 2/3 (2C): deterministic classification + redundancy glue (pure).
// ---------------------------------------------------------------------------

/// Deterministically classify token-account holders from a `getTokenLargestAccounts`
/// result already converted to UI-token balances (base / 10^decimals). curve/program
/// and creator are matched ONLY by their deterministic associated-token-account
/// addresses — never by a balance heuristic. `creator_ata == None` means creator
/// attribution is BLOCKED (ambiguous), so no account is labeled Creator. Everything
/// unmatched is Ordinary; Unknown is never fabricated here.
pub fn classify_holder_accounts(
    largest: &[(String, u64)],
    curve_ata: &str,
    creator_ata: Option<&str>,
) -> Vec<HolderAccount> {
    largest
        .iter()
        .map(|(addr, bal)| {
            let class = if addr == curve_ata {
                HolderAccountClass::CurveProgram
            } else if creator_ata == Some(addr.as_str()) {
                HolderAccountClass::Creator
            } else {
                HolderAccountClass::Ordinary
            };
            HolderAccount { address: addr.clone(), raw_balance_tokens: *bal, class }
        })
        .collect()
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
        Some(coverage_aware_snapshot(cov, run_id, mint, class, cutoff, computed_at, cutoff, buf))
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
    use super::super::measurement::TradeSide;
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

    #[test] // curve/creator matched by deterministic ATA, everything else Ordinary
    fn classify_curve_creator_ordinary() {
        let largest = vec![
            ("CURVE".to_string(), 800_000_000u64),
            ("CREATOR".to_string(), 100_000_000u64),
            ("WALLET1".to_string(), 60_000_000u64),
        ];
        let out = classify_holder_accounts(&largest, "CURVE", Some("CREATOR"));
        assert_eq!(out[0].class, HolderAccountClass::CurveProgram);
        assert_eq!(out[1].class, HolderAccountClass::Creator);
        assert_eq!(out[2].class, HolderAccountClass::Ordinary);
        assert_eq!(out[0].raw_balance_tokens, 800_000_000);
    }

    #[test] // creator BLOCKED (None) => no account labeled Creator
    fn classify_creator_blocked() {
        let largest = vec![("CURVE".to_string(), 1u64), ("X".to_string(), 2u64)];
        let out = classify_holder_accounts(&largest, "CURVE", None);
        assert!(out.iter().all(|a| a.class != HolderAccountClass::Creator));
        assert_eq!(out[1].class, HolderAccountClass::Ordinary);
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
}
