//! Deterministic, fail-closed replay (P1-001, packet sections 25-26).
//!
//! Reads an append-only observation JSONL run and validates it strictly:
//! interior corruption is an error (never silently skipped); only a single
//! non-newline-terminated trailing fragment may be ignored (crash recovery).

use std::collections::BTreeMap;
use std::path::Path;

use crate::observation::schema::{
    ObservationEnvelope, ObservationPayload, OBSERVATION_SCHEMA_VERSION,
};

/// A fully validated observation run.
pub struct ReplayRun {
    pub run_id: String,
    pub records: Vec<ObservationEnvelope>,
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
/// - schema_version != 1 => Err;
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

    validate(&records)?;

    let run_id = records
        .first()
        .map(|e| e.run_id.clone())
        .unwrap_or_default();

    Ok(ReplayRun {
        run_id,
        records,
        trailing_partial_ignored,
    })
}

fn validate(records: &[ObservationEnvelope]) -> crate::Result<()> {
    if records.is_empty() {
        return err("empty run");
    }

    // schema version, single run_id, contiguous seq.
    let run_id = &records[0].run_id;
    for (expected_seq, env) in records.iter().enumerate() {
        if env.schema_version != OBSERVATION_SCHEMA_VERSION {
            return err("schema version mismatch");
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

    Ok(())
}

// ---------------------------------------------------------------------------
// Section 27 — Agent A replay tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::schema::{
        CandidateObservedRecord, RunCompletion, RunFinishedRecord, RunStartedRecord,
    };
    use std::io::Write;

    fn envelope(seq: u64, run_id: &str, payload: ObservationPayload) -> ObservationEnvelope {
        ObservationEnvelope {
            schema_version: 1,
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
        ))
    }

    fn candidate(id: &str) -> ObservationPayload {
        ObservationPayload::CandidateObserved(CandidateObservedRecord {
            candidate_id: id.into(),
            signature: id.into(),
            mint: "mint".into(),
            creator: "creator".into(),
            bonding_curve: "bc".into(),
            tx_type: "create".into(),
            provider_initial_buy: 0.0,
            provider_v_tokens_in_bonding_curve: 0.0,
            provider_v_sol_in_bonding_curve_sol: 0.0,
            provider_market_cap_sol: 0.0,
            name: "n".into(),
            symbol: "s".into(),
            uri: "u".into(),
            duplicate: false,
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
    fn test_replay_rejects_schema_mismatch() {
        let mut env = envelope(0, "r", run_started_payload());
        env.schema_version = 2;
        let f = write_lines(&[line(&env)], true);
        assert!(read_observation_run(f.path()).is_err());
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
}
