//! Append-only observation recorder (P1-001, packet sections 23-24).
//!
//! Writes one compact JSON line per observation envelope. Append-only: never
//! seeks backward, never rewrites a prior line. Sequence numbers are strictly
//! contiguous starting at 0 (the automatic RunStarted record).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::observation::schema::{
    ObservationEnvelope, ObservationPayload, RunStartedRecord, OBSERVATION_SCHEMA_VERSION,
};

struct RecorderInner {
    run_id: String,
    next_seq: u64,
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

/// Append-only JSONL recorder. Cheaply cloneable; all clones share one file
/// handle behind a `tokio::sync::Mutex`.
#[derive(Clone)]
pub struct ObservationRecorder {
    inner: Arc<Mutex<RecorderInner>>,
}

impl ObservationRecorder {
    /// Create a new run file under `output_dir` and automatically append the
    /// `seq=0` RunStarted record.
    ///
    /// The output directory is created if absent. The file is opened with
    /// `create_new(true)` (no overwrite). The filename is
    /// `observation_<UTC compact>_<run_id>.jsonl`.
    pub async fn create(
        output_dir: impl AsRef<Path>,
        run_started: RunStartedRecord,
    ) -> crate::Result<Self> {
        let dir = output_dir.as_ref();
        tokio::fs::create_dir_all(dir).await?;

        let run_id = uuid::Uuid::new_v4().to_string();
        // Utc::now() is fine for a filename (no test relies on it being fixed).
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let filename = format!("observation_{stamp}_{run_id}.jsonl");
        let path = dir.join(filename);

        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await?;

        let recorder = Self {
            inner: Arc::new(Mutex::new(RecorderInner {
                run_id,
                next_seq: 0,
                file,
                path,
            })),
        };

        recorder
            .append(ObservationPayload::RunStarted(run_started))
            .await?;

        Ok(recorder)
    }

    /// Append one payload as a compact single-line JSON envelope. Returns the
    /// sequence number used. The sequence counter is only incremented after the
    /// full line (JSON + newline) is written and flushed.
    pub async fn append(&self, payload: ObservationPayload) -> crate::Result<u64> {
        let mut inner = self.inner.lock().await;
        let seq = inner.next_seq;

        let envelope = ObservationEnvelope {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            run_id: inner.run_id.clone(),
            seq,
            recorded_at: Utc::now(),
            payload,
        };

        // Compact serialization (serde_json::to_string is single-line).
        let json = serde_json::to_string(&envelope)?;

        inner.file.write_all(json.as_bytes()).await?;
        inner.file.write_all(b"\n").await?;
        inner.file.flush().await?;

        // Only advance after a fully successful line write.
        inner.next_seq = seq + 1;
        Ok(seq)
    }

    /// Flush and durably sync file data to disk.
    pub async fn sync_data(&self) -> crate::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.file.flush().await?;
        inner.file.sync_data().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Section 27 — Agent A recorder tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::replay::read_observation_run;
    use crate::observation::schema::{
        RunCompletion, RunFinishedRecord, StreamStateKind, StreamStateRecord,
    };
    use std::collections::HashSet;

    fn run_started() -> RunStartedRecord {
        RunStartedRecord::new("unknown".into(), None, "test-0.0.0".into(), 900, 64, None)
    }

    fn stream(kind: StreamStateKind) -> ObservationPayload {
        ObservationPayload::StreamState(StreamStateRecord {
            state: kind,
            category: None,
        })
    }

    async fn find_run_file(dir: &Path) -> PathBuf {
        let mut rd = tokio::fs::read_dir(dir).await.unwrap();
        let mut found = None;
        while let Some(e) = rd.next_entry().await.unwrap() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("observation_") && name.ends_with(".jsonl") {
                found = Some(e.path());
            }
        }
        found.expect("no observation file")
    }

    #[tokio::test]
    async fn test_recorder_creates_unique_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let r1 = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();
        let r2 = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();
        let p1 = r1.inner.lock().await.path.clone();
        let p2 = r2.inner.lock().await.path.clone();
        assert_ne!(p1, p2);
        assert!(p1
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".jsonl"));
    }

    #[tokio::test]
    async fn test_recorder_first_record_run_started_seq_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let r = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();
        r.sync_data().await.unwrap();
        let path = find_run_file(tmp.path()).await;
        let run = read_observation_run(&path).unwrap();
        assert_eq!(run.records[0].seq, 0);
        assert!(matches!(
            run.records[0].payload,
            ObservationPayload::RunStarted(_)
        ));
    }

    #[tokio::test]
    async fn test_recorder_sequences_are_contiguous_under_concurrent_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let r = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();

        let n = 100u64;
        let mut handles = Vec::new();
        for _ in 0..n {
            let rc = r.clone();
            handles.push(tokio::spawn(async move {
                rc.append(stream(StreamStateKind::Connected)).await.unwrap()
            }));
        }
        let mut seqs = HashSet::new();
        for h in handles {
            seqs.insert(h.await.unwrap());
        }
        // seq 0 was RunStarted; concurrent appends produced 1..=n.
        assert_eq!(seqs.len(), n as usize);
        let expected: HashSet<u64> = (1..=n).collect();
        assert_eq!(seqs, expected);
    }

    #[tokio::test]
    async fn test_recorder_compact_one_json_object_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        let r = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();
        r.append(stream(StreamStateKind::Connected)).await.unwrap();
        r.append(stream(StreamStateKind::Disconnected))
            .await
            .unwrap();
        r.sync_data().await.unwrap();

        let path = find_run_file(tmp.path()).await;
        let contents = std::fs::read_to_string(&path).unwrap();
        for line in contents.lines() {
            assert!(!line.is_empty());
            assert!(!line.contains('\n'));
            // Each line parses as exactly one JSON object.
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object());
        }
    }

    #[tokio::test]
    async fn test_recorder_sync_and_replay_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let r = ObservationRecorder::create(tmp.path(), run_started())
            .await
            .unwrap();
        r.append(stream(StreamStateKind::Connected)).await.unwrap();
        r.append(ObservationPayload::RunFinished(RunFinishedRecord {
            completion: RunCompletion::Complete,
            candidates_seen: 0,
            unique_candidates: 0,
            duplicate_candidate_events: 0,
            tracking_started: 0,
            tracking_skipped: 0,
            tracking_completed: 0,
            stream_connected_events: 1,
            stream_disconnect_events: 0,
            provider_errors: 0,
            unexpected_trade_events: 0,
            migrations_seen: 0,
            partial_new_token_events: 0,
            rpc_gate_peak_in_flight: None,
            rpc_gate_acquisitions: None,
            rpc_gate_wait_ms_total: None,
            rpc_gate_wait_ms_max: None,
        }))
        .await
        .unwrap();
        r.sync_data().await.unwrap();

        let path = find_run_file(tmp.path()).await;
        let run = read_observation_run(&path).unwrap();
        assert_eq!(run.records.len(), 3);
        assert!(!run.trailing_partial_ignored);
        assert_eq!(
            run.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
