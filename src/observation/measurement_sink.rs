//! P3 measurement pilot — separate append-only measurement sink (Option B).
//!
//! Measurement events are written to their OWN `measurement_<run>.jsonl`, never
//! into the canonical ObservationPayload stream — the audited observation core is
//! untouched. Every line is a `MeasurementEnvelope` carrying the linkage contract
//! (run_id, mint, event_type, measurement_schema_version, source_revision, seq,
//! emitted_at) so it always ties back to the canonical run.
//!
//! Also: PumpPortal `TradeEvent` -> audited `TradeObserved` normalization, and the
//! expected-vs-unexpected trade dispatch decision (pure, testable). No trading/
//! signing/tx. If any measurement event ever required mutating canonical state,
//! that coupling is NOT introduced here.

use std::io::Write;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::measurement::{
    HolderSnapshot, MeasurementFailureRecord, MicrostructureProbe, ParticipationSnapshot, TradeObserved,
    TradeSide,
};
use super::measurement_runtime::MintSubscription;
use crate::stream::pumpportal::TradeEvent;

/// Measurement-sink schema version (independent of OBSERVATION_SCHEMA_VERSION).
pub const MEASUREMENT_SINK_SCHEMA_VERSION: u32 = 1;

/// A per-mint subscription-state snapshot persisted for coverage/backpressure audit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionStateRecord {
    pub mint: String,
    pub phase: String,
    pub requested_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub retry_count: u32,
    pub reconnect_resubscribe_count: u32,
    pub coverage_known: bool,
    pub unsubscribe_requested: bool,
    pub unsubscribe_confirmed: bool,
}

impl SubscriptionStateRecord {
    pub fn from_sub(s: &MintSubscription) -> Self {
        Self {
            mint: s.mint.clone(),
            phase: format!("{:?}", s.phase),
            requested_at: s.requested_at,
            acknowledged_at: s.acknowledged_at,
            retry_count: s.retry_count,
            reconnect_resubscribe_count: s.reconnect_resubscribe_count,
            coverage_known: s.coverage_known,
            unsubscribe_requested: s.unsubscribe_requested,
            unsubscribe_confirmed: s.unsubscribe_confirmed,
        }
    }
}

/// The payload union persisted to the measurement sink.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum MeasurementPayload {
    TradeObserved(TradeObserved),
    ParticipationSnapshot(ParticipationSnapshot),
    HolderSnapshot(HolderSnapshot),
    MicrostructureProbe(MicrostructureProbe),
    MeasurementFailure(MeasurementFailureRecord),
    SubscriptionState(SubscriptionStateRecord),
}

impl MeasurementPayload {
    pub fn event_type(&self) -> &'static str {
        match self {
            MeasurementPayload::TradeObserved(_) => "TradeObserved",
            MeasurementPayload::ParticipationSnapshot(_) => "ParticipationSnapshot",
            MeasurementPayload::HolderSnapshot(_) => "HolderSnapshot",
            MeasurementPayload::MicrostructureProbe(_) => "MicrostructureProbe",
            MeasurementPayload::MeasurementFailure(_) => "MeasurementFailure",
            MeasurementPayload::SubscriptionState(_) => "SubscriptionState",
        }
    }
    /// Candidate/mint linkage (best-effort from the payload).
    pub fn mint(&self) -> String {
        match self {
            MeasurementPayload::TradeObserved(t) => t.mint.clone(),
            MeasurementPayload::ParticipationSnapshot(p) => p.mint.clone(),
            MeasurementPayload::HolderSnapshot(h) => h.mint.clone(),
            MeasurementPayload::MicrostructureProbe(m) => m.mint.clone(),
            MeasurementPayload::MeasurementFailure(f) => f.mint.clone(),
            MeasurementPayload::SubscriptionState(s) => s.mint.clone(),
        }
    }
}

/// One append-only line in `measurement_<run>.jsonl`. Carries the full linkage
/// contract back to the canonical run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementEnvelope {
    pub run_id: String,
    pub mint: String,
    pub event_type: String,
    pub measurement_schema_version: u32,
    pub source_revision: String,
    pub seq: u64,
    pub emitted_at: DateTime<Utc>,
    pub payload: MeasurementPayload,
}

/// Append-only writer for measurement events. Generic over the underlying writer
/// so tests use an in-memory buffer and production uses a File. Never touches the
/// canonical observation stream.
pub struct MeasurementSink<W: Write> {
    writer: W,
    run_id: String,
    source_revision: String,
    seq: u64,
}

impl<W: Write> MeasurementSink<W> {
    pub fn new(writer: W, run_id: &str, source_revision: &str) -> Self {
        Self { writer, run_id: run_id.to_string(), source_revision: source_revision.to_string(), seq: 0 }
    }
    /// Append one measurement event as a JSON line. Returns the assigned seq.
    pub fn append(&mut self, payload: MeasurementPayload, emitted_at: DateTime<Utc>) -> std::io::Result<u64> {
        let seq = self.seq;
        let env = MeasurementEnvelope {
            run_id: self.run_id.clone(),
            mint: payload.mint(),
            event_type: payload.event_type().to_string(),
            measurement_schema_version: MEASUREMENT_SINK_SCHEMA_VERSION,
            source_revision: self.source_revision.clone(),
            seq,
            emitted_at,
            payload,
        };
        let line = serde_json::to_string(&env).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.seq += 1;
        Ok(seq)
    }
    pub fn appended(&self) -> u64 {
        self.seq
    }
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// PumpPortal TradeEvent -> audited TradeObserved
// ---------------------------------------------------------------------------

/// Normalize a PumpPortal `TradeEvent` into the audited `TradeObserved`. Persists
/// source-supported fields only; no chain slot/block_time/wallet-balance invented.
pub fn normalize_trade_event(
    ev: &TradeEvent,
    run_id: &str,
    source_revision: &str,
    event_received_at: DateTime<Utc>,
) -> TradeObserved {
    TradeObserved {
        schema_version: super::schema::OBSERVATION_SCHEMA_VERSION,
        normalization_version: super::measurement::MEASUREMENT_NORMALIZATION_VERSION,
        run_id: run_id.to_string(),
        mint: ev.mint.clone(),
        signature: ev.signature.clone(),
        trader_public_key: ev.trader_public_key.clone(),
        tx_type: ev.tx_type.clone(),
        side: TradeSide::from_tx_type(&ev.tx_type),
        token_amount_ui: ev.token_amount,
        sol_amount: ev.sol_amount,
        bonding_curve_key: ev.bonding_curve_key.clone(),
        v_tokens_in_bonding_curve: ev.v_tokens_in_bonding_curve,
        v_sol_in_bonding_curve: ev.v_sol_in_bonding_curve,
        market_cap_sol: ev.market_cap_sol,
        event_received_at,
        source: "pumpportal".to_string(),
        source_revision: source_revision.to_string(),
    }
}

/// Dispatch decision for an incoming trade: is its mint an expected active
/// subscription (route + persist) or truly unexpected (preserve anomaly)?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeRouting {
    ExpectedActive,
    Unexpected,
}

/// Pure classifier: a trade is Expected iff the mint has a tracked subscription
/// that is not in a terminal (Unsubscribed) state. Does NOT depend on any
/// hypothesis/price/outcome.
pub fn classify_trade(is_tracked_active: bool) -> TradeRouting {
    if is_tracked_active {
        TradeRouting::ExpectedActive
    } else {
        TradeRouting::Unexpected
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::measurement::MEASUREMENT_NORMALIZATION_VERSION;
    use chrono::TimeZone;

    fn ts(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }
    fn tev() -> TradeEvent {
        TradeEvent {
            signature: "SIG".into(), mint: "MINT".into(), trader_public_key: "W1".into(), tx_type: "buy".into(),
            token_amount: 35000.0, sol_amount: 0.5, bonding_curve_key: "BC".into(),
            v_tokens_in_bonding_curve: 1e9, v_sol_in_bonding_curve: 30.0, market_cap_sol: 28.0,
        }
    }

    #[test]
    fn normalize_maps_source_fields() {
        let t = normalize_trade_event(&tev(), "run1", "260a65d", ts(5));
        assert_eq!(t.mint, "MINT");
        assert_eq!(t.trader_public_key, "W1");
        assert_eq!(t.side, TradeSide::Buy);
        assert_eq!(t.token_amount_ui, 35000.0);
        assert_eq!(t.sol_amount, 0.5);
        assert_eq!(t.event_received_at, ts(5));
        assert_eq!(t.normalization_version, MEASUREMENT_NORMALIZATION_VERSION);
        assert_eq!(t.source, "pumpportal");
    }

    #[test]
    fn envelope_roundtrip_carries_linkage() {
        let t = normalize_trade_event(&tev(), "run1", "260a65d", ts(5));
        let env = MeasurementEnvelope {
            run_id: "run1".into(), mint: "MINT".into(), event_type: "TradeObserved".into(),
            measurement_schema_version: MEASUREMENT_SINK_SCHEMA_VERSION, source_revision: "260a65d".into(),
            seq: 7, emitted_at: ts(9), payload: MeasurementPayload::TradeObserved(t),
        };
        let j = serde_json::to_string(&env).unwrap();
        let back: MeasurementEnvelope = serde_json::from_str(&j).unwrap();
        assert_eq!(env, back);
        // linkage fields present in the wire form
        assert!(j.contains("\"run_id\":\"run1\""));
        assert!(j.contains("\"source_revision\":\"260a65d\""));
        assert!(j.contains("\"seq\":7"));
        assert!(j.contains("\"event_type\":\"TradeObserved\""));
    }

    #[test]
    fn sink_appends_increasing_seq_one_line_each() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = MeasurementSink::new(&mut buf, "run1", "260a65d");
            let t = normalize_trade_event(&tev(), "run1", "260a65d", ts(5));
            assert_eq!(sink.append(MeasurementPayload::TradeObserved(t.clone()), ts(6)).unwrap(), 0);
            assert_eq!(sink.append(MeasurementPayload::TradeObserved(t), ts(7)).unwrap(), 1);
            assert_eq!(sink.appended(), 2);
            sink.flush().unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 2);
        // each line is a valid envelope with its seq
        let e0: MeasurementEnvelope = serde_json::from_str(lines[0]).unwrap();
        let e1: MeasurementEnvelope = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e0.seq, 0);
        assert_eq!(e1.seq, 1);
        assert_eq!(e0.run_id, "run1");
    }

    #[test]
    fn dispatch_expected_vs_unexpected() {
        assert_eq!(classify_trade(true), TradeRouting::ExpectedActive);
        assert_eq!(classify_trade(false), TradeRouting::Unexpected);
    }

    #[test]
    fn payload_event_type_and_mint() {
        let t = normalize_trade_event(&tev(), "r", "rev", ts(1));
        let p = MeasurementPayload::TradeObserved(t);
        assert_eq!(p.event_type(), "TradeObserved");
        assert_eq!(p.mint(), "MINT");
    }

    #[test]
    fn sink_schema_version_frozen() {
        assert_eq!(MEASUREMENT_SINK_SCHEMA_VERSION, 1);
    }
}
