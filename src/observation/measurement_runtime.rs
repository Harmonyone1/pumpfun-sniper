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
    compute_participation_snapshot, MeasurementFailureCategory, ParticipationSnapshot, SnapshotClass,
    TradeObserved,
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
}
