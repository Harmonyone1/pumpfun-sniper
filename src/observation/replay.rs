//! Deterministic, fail-closed replay (P1-001, packet sections 25-26).
//!
//! Reads an append-only observation JSONL run and validates it strictly:
//! interior corruption is an error (never silently skipped); only a single
//! non-newline-terminated trailing fragment may be ignored (crash recovery).

use std::collections::BTreeMap;
use std::path::Path;

use crate::observation::schema::{ObservationEnvelope, ObservationPayload};

/// A fully validated observation run.
pub struct ReplayRun {
    pub run_id: String,
    pub records: Vec<ObservationEnvelope>,
    /// The single schema version used by every line in the run (1, 2, or 3). A run
    /// file must use exactly one version; mixed versions are rejected.
    pub schema_version: u32,
    /// True when a single unterminated trailing fragment failed to parse and
    /// was ignored (crash recovery).
    pub trailing_partial_ignored: bool,
}

impl ReplayRun {
    /// Group records by candidate id, preserving global `seq` order within each
    /// candidate. Records that carry no candidate id are omitted.
    pub fn candidate_timelines(&self) -> BTreeMap<String, Vec<&ObservationEnvelope>> {
        let mut map: BTreeMap<String, Vec<&ObservationEnvelope>> = BTreeMap::new();
        for env in &self.records {
            if let Some(cid) = env.payload.candidate_id() {
                map.entry(cid.to_string()).or_default().push(env);
            }
        }
        map
    }
}

fn err<T>(msg: &str) -> crate::Result<T> {
    Err(crate::Error::Deserialization(msg.to_string()))
}

/// Read and strictly validate an observation run file.
///
/// Fail-closed rules:
/// - any interior (newline-terminated) malformed line => Err;
/// - schema_version not in {1, 2, 3} => Err (version 0 or >3 rejected);
/// - mixed schema versions within one run => Err (all lines must match);
/// - more than one distinct run_id => Err;
/// - seq must be exactly 0,1,2,... contiguous => Err on any gap;
/// - the first complete record must be a RunStarted payload => Err otherwise;
/// - at most one RunStarted => Err;
/// - if a RunFinished exists it must be the final complete record with nothing
///   after it => Err otherwise;
/// - a trailing fragment with NO terminating newline that fails to parse is
///   ignored and sets `trailing_partial_ignored=true`;
/// - a final non-newline line that DOES parse is accepted.
pub fn read_observation_run(path: impl AsRef<Path>) -> crate::Result<ReplayRun> {
    let contents = std::fs::read_to_string(path)?;

    // Split into physical lines while tracking whether each is newline
    // terminated. The last element after splitting on '\n' is the trailing
    // fragment (empty if the file ended with a newline).
    let ends_with_newline = contents.ends_with('\n');
    let raw_lines: Vec<&str> = contents.split('\n').collect();

    let mut records: Vec<ObservationEnvelope> = Vec::new();
    let mut trailing_partial_ignored = false;

    let last_idx = raw_lines.len().saturating_sub(1);
    for (i, line) in raw_lines.iter().enumerate() {
        let is_last_fragment = i == last_idx;

        // The final split element is the trailing fragment when the file does
        // NOT end with a newline. An empty final element (file ended in '\n')
        // is just the terminator artifact and is skipped.
        if is_last_fragment {
            if line.is_empty() {
                // File ended with newline (or is empty) — nothing to parse.
                continue;
            }
            if !ends_with_newline {
                // Non-newline-terminated trailing fragment.
                match serde_json::from_str::<ObservationEnvelope>(line) {
                    Ok(env) => records.push(env),
                    Err(_) => {
                        trailing_partial_ignored = true;
                    }
                }
                continue;
            }
            // ends_with_newline true but non-empty last => impossible with
            // split semantics, fall through to strict parse.
        }

        // Interior line (newline-terminated): must parse.
        if line.is_empty() {
            return err("interior empty line");
        }
        match serde_json::from_str::<ObservationEnvelope>(line) {
            Ok(env) => records.push(env),
            Err(_) => return err("interior malformed line"),
        }
    }

    let schema_version = validate(&records)?;

    let run_id = records
        .first()
        .map(|e| e.run_id.clone())
        .unwrap_or_default();

    Ok(ReplayRun {
        run_id,
        records,
        schema_version,
        trailing_partial_ignored,
    })
}

fn validate(records: &[ObservationEnvelope]) -> crate::Result<u32> {
    if records.is_empty() {
        return err("empty run");
    }

    // schema version, single run_id, contiguous seq. The run's version is taken
    // from the first line; it must be a supported version (1, 2, or 3) and every
    // subsequent line must match it exactly (no mixed schema-version runs).
    let schema_version = records[0].schema_version;
    if schema_version != 1 && schema_version != 2 && schema_version != 3 {
        return err("unsupported schema version");
    }
    let run_id = &records[0].run_id;
    for (expected_seq, env) in records.iter().enumerate() {
        if env.schema_version != schema_version {
            return err("schema version mismatch within run");
        }
        if &env.run_id != run_id {
            return err("run_id changed within run");
        }
        if env.seq != expected_seq as u64 {
            return err("non-contiguous sequence");
        }
    }

    // first complete record must be RunStarted.
    if !matches!(records[0].payload, ObservationPayload::RunStarted(_)) {
        return err("first record is not RunStarted");
    }

    // at most one RunStarted.
    let run_started_count = records
        .iter()
        .filter(|e| matches!(e.payload, ObservationPayload::RunStarted(_)))
        .count();
    if run_started_count > 1 {
        return err("multiple RunStarted records");
    }

    // if a RunFinished exists, it must be the final record.
    if let Some(pos) = records
        .iter()
        .position(|e| matches!(e.payload, ObservationPayload::RunFinished(_)))
    {
        if pos != records.len() - 1 {
            return err("record present after RunFinished");
        }
    }

    Ok(schema_version)
}

// ---------------------------------------------------------------------------
// Section 27 — Agent A replay tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::schema::{
        CandidateObservedRecord, InitialMarketRecord, MarketDataFailureKind,
        ObservationFailureCode, OutcomeSampleRecord, RunCompletion, RunFinishedRecord,
        RunStartedRecord,
    };
    use std::io::Write;

    fn envelope(seq: u64, run_id: &str, payload: ObservationPayload) -> ObservationEnvelope {
        envelope_v(2, seq, run_id, payload)
    }

    fn envelope_v(
        schema_version: u32,
        seq: u64,
        run_id: &str,
        payload: ObservationPayload,
    ) -> ObservationEnvelope {
        ObservationEnvelope {
            schema_version,
            run_id: run_id.to_string(),
            seq,
            recorded_at: chrono::Utc::now(),
            payload,
        }
    }

    fn run_started_payload() -> ObservationPayload {
        ObservationPayload::RunStarted(RunStartedRecord::new(
            "unknown".into(),
            None,
            "t".into(),
            900,
            64,
            None,
        ))
    }

    fn candidate(id: &str) -> ObservationPayload {
        ObservationPayload::CandidateObserved(CandidateObservedRecord {
            candidate_id: id.into(),
            signature: id.into(),
            mint: "mint".into(),
            creator: "creator".into(),
            bonding_curve: Some("bc".into()),
            tx_type: "create".into(),
            provider_initial_buy: Some(0.0),
            provider_v_tokens_in_bonding_curve: Some(0.0),
            provider_v_sol_in_bonding_curve_sol: Some(0.0),
            provider_market_cap_sol: Some(0.0),
            name: "n".into(),
            symbol: "s".into(),
            uri: "u".into(),
            duplicate: false,
            provider_create_shape: Some(crate::observation::schema::ProviderCreateShape::Full),
        })
    }

    fn write_lines(lines: &[String], trailing_newline: bool) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let joined = lines.join("\n");
        f.write_all(joined.as_bytes()).unwrap();
        if trailing_newline {
            f.write_all(b"\n").unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn line(env: &ObservationEnvelope) -> String {
        serde_json::to_string(env).unwrap()
    }

    #[test]
    fn test_replay_rejects_interior_malformed_line() {
        let good = line(&envelope(0, "r", run_started_payload()));
        let lines = vec![good, "{not json".to_string(), "irrelevant".to_string()];
        let f = write_lines(&lines, true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_replay_ignores_only_partial_final_fragment() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(1, "r", candidate("c1")));
        // No trailing newline; final fragment is junk.
        let lines = vec![l0, l1, "{partial".to_string()];
        let f = write_lines(&lines, false);
        let run = read_observation_run(f.path()).unwrap();
        assert!(run.trailing_partial_ignored);
        assert_eq!(run.records.len(), 2);
    }

    #[test]
    fn test_replay_v2_supported() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(1, "r", candidate("c1")));
        let f = write_lines(&[l0, l1], true);
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 2);
    }

    #[test]
    fn test_replay_v3_decision_point_buy_quote_supported() {
        let run_started = r#"{"schema_version":3,"run_id":"v3","seq":0,"recorded_at":"2026-08-31T00:00:00Z","payload":{"kind":"run_started","data":{"source_revision":"abc","working_tree_clean":true,"binary_version":"0.1.0","network":"solana-mainnet","entry_quote_lamports":1000000,"outcome_horizons_secs":[2,4,6],"snapshot_horizons_secs":[15,30],"return_model":"protocol_net_ex_network_v1","intake_seconds":600,"max_active_candidates":64,"discovery_universe":"pumpportal_create_identity_v2","outcome_universe":"canonical_sol_quote_exact_0_001_sol_v1","observation_rpc_concurrency_limit":24}}}"#;
        let delayed = r#"{"schema_version":3,"run_id":"v3","seq":1,"recorded_at":"2026-08-31T00:00:07Z","payload":{"kind":"decision_point_buy_quote","data":{"candidate_id":"sig","mint":"mint","nominal_decision_horizon_secs":6,"decision_time":"2026-08-31T00:00:06.200Z","request_started_at":"2026-08-31T00:00:06.210Z","quote_observed_at":"2026-08-31T00:00:06.260Z","decision_to_quote_start_lag_ms":10,"decision_to_quote_available_lag_ms":60,"buy_quote":{"side":"buy","venue":"pump_bonding_curve","quote_asset":"sol","base_decimals":6,"quote_decimals":9,"base_amount_raw":250000,"base_amount_ui":0.25,"quote_amount_raw":1000000,"expected_price_sol_per_token":0.004,"protocol_fee_bps":100,"creator_fee_bps":50,"lp_fee_bps":0,"slot":123,"quoted_at":"2026-08-31T00:00:06.230Z"},"buy_quote_failure":null,"buy_rpc_gate_wait_ms":2,"buy_rpc_call_duration_ms":48,"buy_quote_market_data_kind":null}}}"#;
        let run_finished = r#"{"schema_version":3,"run_id":"v3","seq":2,"recorded_at":"2026-08-31T00:00:08Z","payload":{"kind":"run_finished","data":{"completion":"complete","candidates_seen":1,"unique_candidates":1,"duplicate_candidate_events":0,"tracking_started":1,"tracking_skipped":0,"tracking_completed":1,"stream_connected_events":1,"stream_disconnect_events":0,"provider_errors":0,"unexpected_trade_events":0,"migrations_seen":0,"partial_new_token_events":0,"rpc_gate_peak_in_flight":1,"rpc_gate_acquisitions":1,"rpc_gate_wait_ms_total":2,"rpc_gate_wait_ms_max":2}}}"#;
        let f = write_lines(
            &[
                run_started.to_string(),
                delayed.to_string(),
                run_finished.to_string(),
            ],
            true,
        );
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 3);
        let timelines = run.candidate_timelines();
        assert_eq!(timelines.get("sig").unwrap().len(), 1);
        assert!(matches!(
            timelines.get("sig").unwrap()[0].payload,
            ObservationPayload::DecisionPointBuyQuote(_)
        ));
    }
    #[test]
    fn test_replay_rejects_mixed_versions() {
        // First line v2, second line v1 => mixed run rejected.
        let l0 = line(&envelope_v(2, 0, "r", run_started_payload()));
        let l1 = line(&envelope_v(1, 1, "r", candidate("c1")));
        let f = write_lines(&[l0, l1], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_replay_rejects_unsupported_version_high() {
        let l0 = line(&envelope_v(4, 0, "r", run_started_payload()));
        let f = write_lines(&[l0], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_replay_rejects_unsupported_version_zero() {
        let l0 = line(&envelope_v(0, 0, "r", run_started_payload()));
        let f = write_lines(&[l0], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    /// HARD AUDIT REQUIREMENT (packet section 15): a literal, hand-authored v1
    /// JSONL run — NOT serialized from the v2 structs — must replay as v1 with
    /// concrete provider values landing in `Some(...)` and no create shape.
    #[test]
    fn test_literal_v1_fixture_replays_as_some_none_shape() {
        let run_started = r#"{"schema_version":1,"run_id":"legacy","seq":0,"recorded_at":"2024-01-01T00:00:00Z","payload":{"kind":"run_started","data":{"source_revision":"abc123","working_tree_clean":true,"binary_version":"0.1.0","network":"solana-mainnet","entry_quote_lamports":1000000,"outcome_horizons_secs":[2,4],"snapshot_horizons_secs":[15,30],"return_model":"protocol_net_ex_network_v1","intake_seconds":900,"max_active_candidates":64}}}"#;
        let candidate = r#"{"schema_version":1,"run_id":"legacy","seq":1,"recorded_at":"2024-01-01T00:00:01Z","payload":{"kind":"candidate_observed","data":{"candidate_id":"sigA","signature":"sigA","mint":"MintA","creator":"CreatorA","bonding_curve":"BondingA","tx_type":"create","provider_initial_buy":1.25,"provider_v_tokens_in_bonding_curve":1000000.0,"provider_v_sol_in_bonding_curve_sol":30.0,"provider_market_cap_sol":42.0,"name":"Tok","symbol":"TK","uri":"ipfs://x","duplicate":false}}}"#;
        let run_finished = r#"{"schema_version":1,"run_id":"legacy","seq":2,"recorded_at":"2024-01-01T00:00:02Z","payload":{"kind":"run_finished","data":{"completion":"complete","candidates_seen":1,"unique_candidates":1,"duplicate_candidate_events":0,"tracking_started":1,"tracking_skipped":0,"tracking_completed":1,"stream_connected_events":1,"stream_disconnect_events":0,"provider_errors":0,"unexpected_trade_events":0,"migrations_seen":0}}}"#;

        let lines = vec![
            run_started.to_string(),
            candidate.to_string(),
            run_finished.to_string(),
        ];
        let f = write_lines(&lines, true);
        let run = read_observation_run(f.path()).unwrap();

        assert_eq!(run.schema_version, 1);

        // Find the candidate record and assert v1 backward-compat mapping.
        let cand = run
            .records
            .iter()
            .find_map(|e| match &e.payload {
                ObservationPayload::CandidateObserved(c) => Some(c),
                _ => None,
            })
            .expect("candidate record present");

        assert_eq!(cand.bonding_curve.as_deref(), Some("BondingA"));
        assert_eq!(cand.provider_initial_buy, Some(1.25));
        assert_eq!(cand.provider_v_tokens_in_bonding_curve, Some(1_000_000.0));
        assert_eq!(cand.provider_v_sol_in_bonding_curve_sol, Some(30.0));
        assert_eq!(cand.provider_market_cap_sol, Some(42.0));
        // v2-only fields absent in v1 => defaults.
        assert_eq!(cand.provider_create_shape, None);

        // RunStarted v1 line lacks the v2 universe fields => None via default.
        // It also lacks the RPC-gate limit (§13) => None.
        if let ObservationPayload::RunStarted(rs) = &run.records[0].payload {
            assert_eq!(rs.discovery_universe, None);
            assert_eq!(rs.outcome_universe, None);
            assert_eq!(rs.observation_rpc_concurrency_limit, None);
        } else {
            panic!("first record should be RunStarted");
        }

        // RunFinished v1 line lacks partial counter => 0, and lacks the RPC-gate
        // stats (§16) => None, all via serde default.
        if let ObservationPayload::RunFinished(rf) = &run.records[2].payload {
            assert_eq!(rf.partial_new_token_events, 0);
            assert_eq!(rf.rpc_gate_peak_in_flight, None);
            assert_eq!(rf.rpc_gate_acquisitions, None);
            assert_eq!(rf.rpc_gate_wait_ms_total, None);
            assert_eq!(rf.rpc_gate_wait_ms_max, None);
        } else {
            panic!("last record should be RunFinished");
        }
    }

    /// §35: a gated schema-v2 fixture (RunStarted carrying the RPC-gate limit,
    /// RunFinished carrying gate stats) replays cleanly, and the new telemetry
    /// round-trips through the replay reader.
    #[test]
    fn test_literal_v2_gated_fixture_replays() {
        let run_started = r#"{"schema_version":2,"run_id":"gated","seq":0,"recorded_at":"2026-08-30T00:00:00Z","payload":{"kind":"run_started","data":{"source_revision":"abc","working_tree_clean":true,"binary_version":"0.1.0","network":"solana-mainnet","entry_quote_lamports":1000000,"outcome_horizons_secs":[2,4],"snapshot_horizons_secs":[15,30],"return_model":"protocol_net_ex_network_v1","intake_seconds":600,"max_active_candidates":256,"discovery_universe":"pumpportal_create_identity_v2","outcome_universe":"canonical_sol_quote_exact_0_001_sol_v1","observation_rpc_concurrency_limit":24}}}"#;
        let run_finished = r#"{"schema_version":2,"run_id":"gated","seq":1,"recorded_at":"2026-08-30T00:00:02Z","payload":{"kind":"run_finished","data":{"completion":"complete","candidates_seen":1,"unique_candidates":1,"duplicate_candidate_events":0,"tracking_started":1,"tracking_skipped":0,"tracking_completed":1,"stream_connected_events":1,"stream_disconnect_events":0,"provider_errors":0,"unexpected_trade_events":0,"migrations_seen":0,"partial_new_token_events":0,"rpc_gate_peak_in_flight":24,"rpc_gate_acquisitions":500,"rpc_gate_wait_ms_total":1234,"rpc_gate_wait_ms_max":99}}}"#;
        let f = write_lines(&[run_started.to_string(), run_finished.to_string()], true);
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 2);

        if let ObservationPayload::RunStarted(rs) = &run.records[0].payload {
            assert_eq!(rs.observation_rpc_concurrency_limit, Some(24));
        } else {
            panic!("first record should be RunStarted");
        }
        if let ObservationPayload::RunFinished(rf) = &run.records[1].payload {
            assert_eq!(rf.rpc_gate_peak_in_flight, Some(24));
            assert_eq!(rf.rpc_gate_acquisitions, Some(500));
            assert_eq!(rf.rpc_gate_wait_ms_total, Some(1234));
            assert_eq!(rf.rpc_gate_wait_ms_max, Some(99));
            // Invariant: peak in flight never exceeds the configured limit.
            assert!(rf.rpc_gate_peak_in_flight.unwrap() <= 24);
        } else {
            panic!("last record should be RunFinished");
        }
    }

    /// §35: a pre-gate schema-v2 fixture (v2 records that predate this PR, so they
    /// carry NO gate telemetry keys) still replays, with the new fields => None.
    #[test]
    fn test_literal_v2_pre_gate_fixture_replays_none() {
        let run_started = r#"{"schema_version":2,"run_id":"pregate","seq":0,"recorded_at":"2026-08-30T00:00:00Z","payload":{"kind":"run_started","data":{"source_revision":"abc","working_tree_clean":true,"binary_version":"0.1.0","network":"solana-mainnet","entry_quote_lamports":1000000,"outcome_horizons_secs":[2,4],"snapshot_horizons_secs":[15,30],"return_model":"protocol_net_ex_network_v1","intake_seconds":600,"max_active_candidates":128,"discovery_universe":"pumpportal_create_identity_v2","outcome_universe":"canonical_sol_quote_exact_0_001_sol_v1"}}}"#;
        let run_finished = r#"{"schema_version":2,"run_id":"pregate","seq":1,"recorded_at":"2026-08-30T00:00:02Z","payload":{"kind":"run_finished","data":{"completion":"complete","candidates_seen":0,"unique_candidates":0,"duplicate_candidate_events":0,"tracking_started":0,"tracking_skipped":0,"tracking_completed":0,"stream_connected_events":1,"stream_disconnect_events":0,"provider_errors":0,"unexpected_trade_events":0,"migrations_seen":0,"partial_new_token_events":0}}}"#;
        let f = write_lines(&[run_started.to_string(), run_finished.to_string()], true);
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 2);
        if let ObservationPayload::RunStarted(rs) = &run.records[0].payload {
            assert_eq!(rs.observation_rpc_concurrency_limit, None);
        } else {
            panic!("first record should be RunStarted");
        }
        if let ObservationPayload::RunFinished(rf) = &run.records[1].payload {
            assert_eq!(rf.rpc_gate_peak_in_flight, None);
            assert_eq!(rf.rpc_gate_acquisitions, None);
        } else {
            panic!("last record should be RunFinished");
        }
    }

    #[test]
    fn test_replay_rejects_run_id_change() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(1, "OTHER", candidate("c1")));
        let f = write_lines(&[l0, l1], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_replay_rejects_sequence_gap() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l2 = line(&envelope(2, "r", candidate("c1")));
        let f = write_lines(&[l0, l2], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_replay_rejects_record_after_run_finished() {
        let finished = ObservationPayload::RunFinished(RunFinishedRecord {
            completion: RunCompletion::Complete,
            candidates_seen: 0,
            unique_candidates: 0,
            duplicate_candidate_events: 0,
            tracking_started: 0,
            tracking_skipped: 0,
            tracking_completed: 0,
            stream_connected_events: 0,
            stream_disconnect_events: 0,
            provider_errors: 0,
            unexpected_trade_events: 0,
            migrations_seen: 0,
            partial_new_token_events: 0,
            rpc_gate_peak_in_flight: None,
            rpc_gate_acquisitions: None,
            rpc_gate_wait_ms_total: None,
            rpc_gate_wait_ms_max: None,
        });
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(1, "r", finished));
        let l2 = line(&envelope(2, "r", candidate("c1")));
        let f = write_lines(&[l0, l1, l2], true);
        assert!(read_observation_run(f.path()).is_err());
    }

    #[test]
    fn test_candidate_timelines_preserve_global_seq_order() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(1, "r", candidate("A")));
        let l2 = line(&envelope(2, "r", candidate("B")));
        let l3 = line(&envelope(3, "r", candidate("A")));
        let f = write_lines(&[l0, l1, l2, l3], true);
        let run = read_observation_run(f.path()).unwrap();
        let timelines = run.candidate_timelines();
        let a = &timelines["A"];
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].seq, 1);
        assert_eq!(a[1].seq, 3);
        assert_eq!(timelines["B"].len(), 1);
    }

    // == P1-OBSERVATION-MARKET-DATA-TRUTH-001 replay (§13) ===================

    fn initial_market_with_kinds(buy_kind: Option<MarketDataFailureKind>) -> ObservationPayload {
        ObservationPayload::InitialMarket(InitialMarketRecord {
            candidate_id: "sig".into(),
            mint: "mint".into(),
            candidate_received_at: chrono::Utc::now(),
            snapshot: None,
            snapshot_failure: Some(ObservationFailureCode::MarketUnavailable),
            buy_quote: None,
            buy_quote_failure: Some(ObservationFailureCode::MarketUnavailable),
            initial_observation_attempts: 4,
            initial_snapshot_rpc_gate_wait_ms_total: Some(0),
            initial_snapshot_rpc_call_duration_ms_total: Some(120),
            initial_buy_rpc_gate_wait_ms_total: Some(0),
            initial_buy_rpc_call_duration_ms_total: Some(200),
            snapshot_market_data_kind: Some(MarketDataFailureKind::AccountMissing),
            buy_quote_market_data_kind: buy_kind,
        })
    }

    fn outcome_sample_with_kind() -> ObservationPayload {
        ObservationPayload::OutcomeSample(OutcomeSampleRecord {
            candidate_id: "sig".into(),
            mint: "mint".into(),
            horizon_secs: 15,
            due_at: chrono::Utc::now(),
            sampled_at: chrono::Utc::now(),
            sample_lag_ms: 42,
            sell_quote: None,
            sell_quote_failure: Some(ObservationFailureCode::MarketUnavailable),
            snapshot: None,
            snapshot_failure: None,
            protocol_net_ex_network_return_bps: None,
            sell_rpc_gate_wait_ms: Some(0),
            sell_rpc_call_duration_ms: Some(63),
            snapshot_rpc_gate_wait_ms: Some(0),
            snapshot_rpc_call_duration_ms: Some(90),
            sell_market_data_kind: Some(MarketDataFailureKind::QuoteMathUnexecutable),
            snapshot_market_data_kind: None,
        })
    }

    /// A v2 run carrying the new market-data subtype fields round-trips through the
    /// strict replay reader.
    #[test]
    fn test_replay_v2_market_data_kind_round_trip() {
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let l1 = line(&envelope(
            1,
            "r",
            initial_market_with_kinds(Some(MarketDataFailureKind::LiquidityOrReserveUnavailable)),
        ));
        let l2 = line(&envelope(2, "r", outcome_sample_with_kind()));
        let f = write_lines(&[l0, l1, l2], true);
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 2);

        let im = run.records.iter().find_map(|e| match &e.payload {
            ObservationPayload::InitialMarket(r) => Some(r),
            _ => None,
        });
        let im = im.expect("InitialMarket present");
        assert_eq!(
            im.snapshot_market_data_kind,
            Some(MarketDataFailureKind::AccountMissing)
        );
        assert_eq!(
            im.buy_quote_market_data_kind,
            Some(MarketDataFailureKind::LiquidityOrReserveUnavailable)
        );

        let os = run.records.iter().find_map(|e| match &e.payload {
            ObservationPayload::OutcomeSample(r) => Some(r),
            _ => None,
        });
        let os = os.expect("OutcomeSample present");
        assert_eq!(
            os.sell_market_data_kind,
            Some(MarketDataFailureKind::QuoteMathUnexecutable)
        );
        assert_eq!(os.snapshot_market_data_kind, None);
    }

    /// A pre-field v2 InitialMarket / OutcomeSample line (no *_market_data_kind keys)
    /// still replays, with the new fields defaulting to None.
    #[test]
    fn test_replay_v2_pre_field_market_data_kind_none() {
        let im = r#"{"schema_version":2,"run_id":"r","seq":1,"recorded_at":"2026-08-30T00:00:00Z","payload":{"kind":"initial_market","data":{"candidate_id":"sig","mint":"mint","candidate_received_at":"2026-08-30T00:00:00Z","snapshot":null,"snapshot_failure":"market_unavailable","buy_quote":null,"buy_quote_failure":"market_unavailable","initial_observation_attempts":4}}}"#;
        let os = r#"{"schema_version":2,"run_id":"r","seq":2,"recorded_at":"2026-08-30T00:00:00Z","payload":{"kind":"outcome_sample","data":{"candidate_id":"sig","mint":"mint","horizon_secs":15,"due_at":"2026-08-30T00:00:00Z","sampled_at":"2026-08-30T00:00:01Z","sample_lag_ms":1000,"sell_quote":null,"sell_quote_failure":"market_unavailable","snapshot":null,"snapshot_failure":null,"protocol_net_ex_network_return_bps":null}}}"#;
        let l0 = line(&envelope(0, "r", run_started_payload()));
        let f = write_lines(&[l0, im.to_string(), os.to_string()], true);
        let run = read_observation_run(f.path()).unwrap();
        assert_eq!(run.schema_version, 2);
        for e in &run.records {
            match &e.payload {
                ObservationPayload::InitialMarket(r) => {
                    assert_eq!(r.snapshot_market_data_kind, None);
                    assert_eq!(r.buy_quote_market_data_kind, None);
                }
                ObservationPayload::OutcomeSample(r) => {
                    assert_eq!(r.sell_market_data_kind, None);
                    assert_eq!(r.snapshot_market_data_kind, None);
                }
                _ => {}
            }
        }
    }
}
