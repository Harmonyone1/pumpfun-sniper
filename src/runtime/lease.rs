//! Atomic cross-process runtime lease (process exclusivity).
//!
//! The lease is a single file, `{credentials_dir}/runtime.lock`, created with
//! `create_new(true)`. That flag is the atomic exclusion primitive: the OS
//! guarantees exactly one caller can create the file. The file stores JSON
//! metadata (including a random nonce) identifying the owning process.
//!
//! Invariants:
//! - INV-RUN-001: at most one owner of mutable trading state per credentials_dir.
//! - INV-RUN-003: a stale lease is NEVER auto-stolen. Cleanup is a manual,
//!   explicit operator action.
//! - INV-RUN-004: a lease is removed on Drop only if the on-disk nonce matches
//!   the nonce this process created.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Current on-disk lease metadata schema version.
const LEASE_VERSION: u32 = 1;

/// File name of the canonical runtime lease within a `credentials_dir`.
const LOCK_FILE_NAME: &str = "runtime.lock";

/// Persisted metadata describing the process that currently owns the runtime.
///
/// Serialized as JSON into `{credentials_dir}/runtime.lock`. Contains no secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeLeaseMetadata {
    /// Schema version. Currently always [`LEASE_VERSION`] (1).
    pub version: u32,
    /// Random per-acquisition nonce used for ownership-safe release.
    pub nonce: String,
    /// OS process id of the owning process (informational; never trusted for
    /// automatic stealing).
    pub pid: u32,
    /// Human-readable command label that acquired the lease (e.g. "start").
    pub command: String,
    /// When the lease was acquired.
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// An owned runtime lease. Dropping it releases the lock (nonce-checked).
pub struct RuntimeLease {
    path: PathBuf,
    nonce: String,
}

/// Build the canonical lock path for a credentials directory.
fn lock_path(credentials_dir: impl AsRef<Path>) -> PathBuf {
    credentials_dir.as_ref().join(LOCK_FILE_NAME)
}

impl RuntimeLease {
    /// Atomically acquire the runtime lease for `credentials_dir`.
    ///
    /// Uses `OpenOptions::create_new` on `{credentials_dir}/runtime.lock` as the
    /// exclusion primitive. Fails closed if a lease already exists (whether its
    /// metadata is valid or malformed) and never deletes or steals an existing
    /// lock.
    pub fn acquire(
        credentials_dir: impl AsRef<Path>,
        command: impl Into<String>,
    ) -> Result<Self> {
        let dir = credentials_dir.as_ref();
        let path = lock_path(dir);

        // 1. Ensure the directory exists.
        fs::create_dir_all(dir).map_err(|e| {
            Error::RuntimeLock(format!(
                "failed to create credentials dir {}: {}",
                dir.display(),
                e
            ))
        })?;

        // 2. Atomic create_new — the exclusion primitive.
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Lease already held. Fail closed; never steal.
                return Err(Self::already_held_error(&path));
            }
            Err(e) => {
                return Err(Error::RuntimeLock(format!(
                    "failed to create runtime lock {}: {}",
                    path.display(),
                    e
                )));
            }
        };

        // 3. Build metadata.
        let nonce = Uuid::new_v4().to_string();
        let metadata = RuntimeLeaseMetadata {
            version: LEASE_VERSION,
            nonce: nonce.clone(),
            pid: std::process::id(),
            command: command.into(),
            started_at: chrono::Utc::now(),
        };

        // 4. Serialize + durably write.
        let json = serde_json::to_vec_pretty(&metadata).map_err(|e| {
            Error::RuntimeLock(format!("failed to serialize runtime lease metadata: {}", e))
        })?;

        let write_result = (|| -> std::io::Result<()> {
            file.write_all(&json)?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_result {
            // We created the file but could not fully write it. Remove the
            // partial file we own so it does not become a permanent stale lock,
            // then fail. Best-effort removal; ignore its error.
            let _ = fs::remove_file(&path);
            return Err(Error::RuntimeLock(format!(
                "failed to write runtime lock {}: {}",
                path.display(),
                e
            )));
        }

        Ok(RuntimeLease { path, nonce })
    }

    /// Read-only inspection of the current lease for `credentials_dir`.
    ///
    /// - Missing lock => `Ok(None)`.
    /// - Present + valid => `Ok(Some(metadata))`.
    /// - Present + malformed/unreadable => `Err` (fail closed; NOT treated as
    ///   absent).
    pub fn inspect(credentials_dir: impl AsRef<Path>) -> Result<Option<RuntimeLeaseMetadata>> {
        let path = lock_path(credentials_dir);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<RuntimeLeaseMetadata>(&bytes) {
                Ok(meta) => Ok(Some(meta)),
                Err(e) => Err(Error::RuntimeLock(format!(
                    "runtime lock {} exists but its metadata is invalid: {}",
                    path.display(),
                    e
                ))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::RuntimeLock(format!(
                "failed to read runtime lock {}: {}",
                path.display(),
                e
            ))),
        }
    }

    /// Explicitly release the lease (nonce-checked removal), consuming self.
    ///
    /// Drop performs the same removal, so calling this is optional; it exists to
    /// make ownership transitions explicit and testable.
    pub fn release(self) -> Result<()> {
        // Perform the nonce-checked removal, then let Drop run (it will observe
        // the file gone / not matching and do nothing further).
        self.nonce_checked_remove()
    }

    /// Build the error returned when an existing lock is encountered on acquire.
    ///
    /// Names the holder's pid/command/started_at when the metadata is valid;
    /// otherwise reports that the lock exists with invalid metadata. Never
    /// deletes the file.
    fn already_held_error(path: &Path) -> Error {
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<RuntimeLeaseMetadata>(&bytes) {
                Ok(meta) => Error::RuntimeLock(format!(
                    "runtime already owned: pid={} command={:?} started_at={} (lock: {}). \
                     Refusing to steal a live lease; if this is stale, remove {} manually.",
                    meta.pid,
                    meta.command,
                    meta.started_at.to_rfc3339(),
                    path.display(),
                    path.display()
                )),
                Err(_) => Error::RuntimeLock(format!(
                    "runtime lock {} exists but its metadata is invalid; failing closed. \
                     Refusing to steal; remove it manually if it is stale.",
                    path.display()
                )),
            },
            Err(_) => Error::RuntimeLock(format!(
                "runtime lock {} exists but could not be read; failing closed. \
                 Refusing to steal; remove it manually if it is stale.",
                path.display()
            )),
        }
    }

    /// Remove the lock file only if its on-disk nonce matches ours. Returns Ok
    /// even when nothing is removed (missing/mismatched/malformed) so callers
    /// and Drop are never surprised — except genuine IO errors on removal.
    fn nonce_checked_remove(&self) -> Result<()> {
        match fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<RuntimeLeaseMetadata>(&bytes) {
                Ok(meta) if meta.nonce == self.nonce => {
                    fs::remove_file(&self.path).map_err(|e| {
                        Error::RuntimeLock(format!(
                            "failed to remove owned runtime lock {}: {}",
                            self.path.display(),
                            e
                        ))
                    })?;
                    Ok(())
                }
                Ok(_) => {
                    // Different nonce: another process replaced/owns it. Never
                    // remove another process's file.
                    tracing::warn!(
                        path = %self.path.display(),
                        "runtime lock nonce no longer matches; leaving file in place (not our lease)"
                    );
                    Ok(())
                }
                Err(_) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        "runtime lock is malformed; leaving file in place (refusing to remove)"
                    );
                    Ok(())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone; nothing to do.
                Ok(())
            }
            Err(e) => Err(Error::RuntimeLock(format!(
                "failed to read runtime lock {} during release: {}",
                self.path.display(),
                e
            ))),
        }
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        // Best-effort, nonce-checked removal. Never panic in Drop.
        if let Err(e) = self.nonce_checked_remove() {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "failed to release runtime lock on drop"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_lock_with_nonce(dir: &Path, nonce: &str) {
        let meta = RuntimeLeaseMetadata {
            version: LEASE_VERSION,
            nonce: nonce.to_string(),
            pid: 999_999,
            command: "fake".to_string(),
            started_at: chrono::Utc::now(),
        };
        let json = serde_json::to_vec_pretty(&meta).unwrap();
        fs::write(dir.join(LOCK_FILE_NAME), json).unwrap();
    }

    #[test]
    fn test_first_runtime_lease_acquires() {
        let tmp = TempDir::new().unwrap();
        let lease = RuntimeLease::acquire(tmp.path(), "start").unwrap();
        assert!(tmp.path().join(LOCK_FILE_NAME).exists());
        // Metadata round-trips and matches.
        let meta = RuntimeLease::inspect(tmp.path()).unwrap().unwrap();
        assert_eq!(meta.version, LEASE_VERSION);
        assert_eq!(meta.command, "start");
        assert_eq!(meta.nonce, lease.nonce);
        assert_eq!(meta.pid, std::process::id());
    }

    #[test]
    fn test_second_runtime_lease_same_dir_fails() {
        let tmp = TempDir::new().unwrap();
        let _first = RuntimeLease::acquire(tmp.path(), "start").unwrap();
        let second = RuntimeLease::acquire(tmp.path(), "hot_scan");
        assert!(second.is_err());
        match second {
            Err(Error::RuntimeLock(_)) => {}
            other => panic!("expected RuntimeLock error, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_different_credentials_dirs_can_lock_independently() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let _la = RuntimeLease::acquire(a.path(), "start").unwrap();
        let _lb = RuntimeLease::acquire(b.path(), "start").unwrap();
        assert!(a.path().join(LOCK_FILE_NAME).exists());
        assert!(b.path().join(LOCK_FILE_NAME).exists());
    }

    #[test]
    fn test_drop_removes_matching_lock() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOCK_FILE_NAME);
        {
            let _lease = RuntimeLease::acquire(tmp.path(), "start").unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "matching lock should be removed on drop");
    }

    #[test]
    fn test_drop_does_not_remove_replaced_nonce() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOCK_FILE_NAME);
        let lease = RuntimeLease::acquire(tmp.path(), "start").unwrap();
        assert!(path.exists());

        // Simulate another process replacing the lock with a different nonce.
        write_lock_with_nonce(tmp.path(), "some-other-process-nonce");

        drop(lease);

        assert!(
            path.exists(),
            "lock owned by a different nonce must not be removed on drop"
        );
        let meta = RuntimeLease::inspect(tmp.path()).unwrap().unwrap();
        assert_eq!(meta.nonce, "some-other-process-nonce");
    }

    #[test]
    fn test_malformed_existing_lock_fails_closed() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(LOCK_FILE_NAME), b"not valid json at all").unwrap();

        // acquire must fail closed.
        let acquired = RuntimeLease::acquire(tmp.path(), "start");
        assert!(matches!(acquired, Err(Error::RuntimeLock(_))));

        // inspect must also fail closed (not treated as absent).
        let inspected = RuntimeLease::inspect(tmp.path());
        assert!(matches!(inspected, Err(Error::RuntimeLock(_))));

        // File is untouched.
        assert!(tmp.path().join(LOCK_FILE_NAME).exists());
    }

    #[test]
    fn test_stale_lock_is_not_auto_stolen() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOCK_FILE_NAME);
        // A valid-looking lock from a "dead" process with a random nonce.
        write_lock_with_nonce(tmp.path(), "stale-random-nonce");
        let before = fs::read(&path).unwrap();

        let acquired = RuntimeLease::acquire(tmp.path(), "start");
        assert!(
            matches!(acquired, Err(Error::RuntimeLock(_))),
            "stale lock must not be auto-stolen"
        );

        // File contents untouched (not stolen/rewritten).
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "stale lock file must be left byte-identical");
    }

    #[test]
    fn test_inspect_returns_metadata() {
        let tmp = TempDir::new().unwrap();

        // Missing => None.
        assert!(RuntimeLease::inspect(tmp.path()).unwrap().is_none());

        let lease = RuntimeLease::acquire(tmp.path(), "sell").unwrap();
        let meta = RuntimeLease::inspect(tmp.path()).unwrap().unwrap();
        assert_eq!(meta.command, "sell");
        assert_eq!(meta.nonce, lease.nonce);
        assert_eq!(meta.version, LEASE_VERSION);
    }

    #[test]
    fn test_release_removes_matching_lock() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOCK_FILE_NAME);
        let lease = RuntimeLease::acquire(tmp.path(), "start").unwrap();
        assert!(path.exists());
        lease.release().unwrap();
        assert!(!path.exists());
    }
}
