//! The runtime lockfile: one live instance per name, and the operator-intent
//! signal a supervisor reads after the runtime exits.
//!
//! `runtime.lock` holds `{ pid, started_at, shutdown_intent }`. Its three
//! jobs: refuse a second concurrent instance (exit code 4), let `doctor`
//! detect a running one, and — through `shutdown_intent` — tell a supervisor
//! whether an exit was intended. The intent is written **before** the drain
//! begins on every operator path, so a kill between the signal and the exit
//! still carries the fact; it is removed only on a clean exit.
//!
//! Staleness is resolved by process liveness, not by presence alone: a lock
//! whose recorded pid is gone was left by a crash, and refusing to start
//! behind it would wedge every supervised restart. A lock whose pid is still
//! alive is a real second instance, and it is refused.

use std::path::{Path, PathBuf};

use serde_json::json;

/// The exit code a refused second instance ends with — the design's reserved
/// code for "another instance is already running".
pub const SECOND_INSTANCE_EXIT_CODE: i32 = 4;

/// A held lockfile. Dropping it does *not* remove the file — removal is a
/// deliberate act on a clean exit ([`Lockfile::remove`]), because a lock that
/// vanished on every drop could not survive the kill-during-drain the intent
/// exists to record.
#[derive(Debug)]
pub struct Lockfile {
    path: PathBuf,
    pid: u32,
    started_at: String,
}

/// Why a lock could not be acquired.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// A live instance already holds the lock for this name.
    #[error("another agent-bridge instance is already running (pid {pid}) at {path}")]
    AlreadyRunning {
        /// The live owner's pid.
        pid: u32,
        /// The lockfile that named it.
        path: PathBuf,
    },
    /// The lockfile or its directory could not be read or written.
    #[error("lockfile {path} could not be accessed: {source}")]
    Io {
        /// The lockfile involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

impl Lockfile {
    /// Acquire the lock at `path`, writing this process's pid and start time
    /// with no shutdown intent yet.
    ///
    /// If a lock already exists and its recorded pid is still alive, this is a
    /// real second instance and acquisition fails with
    /// [`LockError::AlreadyRunning`]. If the recorded pid is gone the lock is
    /// stale — a crashed instance's leftover — and it is overwritten, so a
    /// supervised restart is never blocked by a lock nothing holds.
    ///
    /// `started_at` is supplied by the caller (an RFC 3339 string) rather than
    /// read from a clock here, so the whole record is deterministic to test.
    pub fn acquire(path: &Path, started_at: String) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let lock = Self {
            path: path.to_path_buf(),
            pid: std::process::id(),
            started_at,
        };
        let io = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };

        // Write the whole record to a pid-unique temporary first, then publish
        // it by hard-linking that temporary at the lock path. The link both
        // *claims* the lock atomically — it fails if the path already exists, so
        // exactly one racer wins — and publishes a file that is already
        // complete, so a concurrent reader (a second instance's liveness check,
        // a supervisor, a kill landing mid-startup) never observes an empty or
        // half-written lock the way an open-then-write would leave one.
        let temp = path.with_extension(format!("lock.{}.tmp", lock.pid));
        std::fs::write(&temp, lock.body(&serde_json::Value::Null)).map_err(io)?;
        let outcome = loop {
            match std::fs::hard_link(&temp, path) {
                Ok(()) => break Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    match live_owner(path)? {
                        Some(pid) => {
                            break Err(LockError::AlreadyRunning {
                                pid,
                                path: path.to_path_buf(),
                            });
                        }
                        // Stale: the owner is gone. Remove and retry the link —
                        // if another racer reclaimed it first, the retry fails
                        // again and its liveness branch refuses. A remove that
                        // fails for any reason but "already gone" ends the
                        // attempt rather than spinning against a lock that
                        // cannot be cleared here.
                        None => match std::fs::remove_file(path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(source) => break Err(io(source)),
                        },
                    }
                }
                Err(source) => break Err(io(source)),
            }
        };
        // The link, or the failure, has been decided; the temporary has served
        // its purpose either way.
        let _ = std::fs::remove_file(&temp);
        outcome.map(|()| lock)
    }

    /// Record operator shutdown intent, atomically, before the drain begins.
    /// Called on every operator path — `runtime.shutdown`, SIGTERM, SIGINT,
    /// stdin EOF — and idempotent, so more than one of those firing at once
    /// leaves one consistent record.
    pub fn write_operator_intent(&self) -> Result<(), LockError> {
        self.write(serde_json::Value::String("operator".to_string()))
    }

    /// Remove the lockfile — the clean-exit act. A crash or a kill skips this,
    /// leaving the file (and its intent, if any) for a supervisor to read.
    ///
    /// Takes `&self` so it composes with the shared handle the operator-intent
    /// closure holds, and an already-absent file is not an error: a clean exit
    /// removing a lock a concurrent path already cleared is the same outcome
    /// either way.
    pub fn remove(&self) -> Result<(), LockError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LockError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// The lock's JSON line for the given `shutdown_intent`.
    fn body(&self, shutdown_intent: &serde_json::Value) -> String {
        format!(
            "{}\n",
            json!({
                "pid": self.pid,
                "started_at": self.started_at,
                "shutdown_intent": shutdown_intent,
            })
        )
    }

    /// Replace the record's `shutdown_intent` atomically: a temporary sibling
    /// written in full and renamed over the target, so a reader never sees a
    /// half-written lock and a kill mid-write cannot corrupt the live one. Only
    /// the lock's single holder calls this, so the shared temp name never races
    /// a second writer.
    fn write(&self, shutdown_intent: serde_json::Value) -> Result<(), LockError> {
        let temp = self.path.with_extension("lock.tmp");
        let io = |source| LockError::Io {
            path: self.path.clone(),
            source,
        };
        std::fs::write(&temp, self.body(&shutdown_intent)).map_err(io)?;
        std::fs::rename(&temp, &self.path).map_err(io)
    }
}

/// The pid recorded in the lock at `path` if that process is still alive, or
/// `None` when there is no lock or its owner is gone (a stale lock). A lock
/// whose contents cannot be read as a pid is treated as stale rather than
/// fatal: a corrupt lock should not wedge startup forever, and `doctor
/// --clean-lock` is the deliberate cleanup path.
fn live_owner(path: &Path) -> Result<Option<u32>, LockError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let Some(pid) = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("pid").and_then(serde_json::Value::as_u64))
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(None);
    };
    Ok(agent_bridge_pty::process_alive(pid).then_some(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent-bridge-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("runtime.lock")
    }

    #[test]
    fn acquire_writes_the_record_with_no_intent_yet() {
        let path = temp_lock("fresh");
        let lock = Lockfile::acquire(&path, "2026-08-26T00:00:00.000Z".into()).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["pid"], std::process::id());
        assert!(body["shutdown_intent"].is_null());
        lock.remove().unwrap();
        assert!(!path.exists(), "a clean exit removes the lock");
    }

    #[test]
    fn intent_is_written_atomically_and_survives_read() {
        let path = temp_lock("intent");
        let lock = Lockfile::acquire(&path, "2026-08-26T00:00:00.000Z".into()).unwrap();
        lock.write_operator_intent().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["shutdown_intent"], "operator");
        // No stray temp file left behind by the write.
        assert!(!path.with_extension("lock.tmp").exists());
    }

    #[test]
    fn a_live_owner_is_refused_and_a_stale_lock_is_reclaimed() {
        let path = temp_lock("contended");
        // Our own pid is live, so a second acquire against the same file is a
        // real second instance and must be refused with the owner named.
        let _held = Lockfile::acquire(&path, "t".into()).unwrap();
        match Lockfile::acquire(&path, "t".into()) {
            Err(LockError::AlreadyRunning { pid, .. }) => assert_eq!(pid, std::process::id()),
            other => panic!("a live owner must be refused, got {other:?}"),
        }

        // A lock naming a pid that cannot exist is stale; acquiring over it
        // succeeds rather than wedging a restart.
        std::fs::write(
            &path,
            json!({ "pid": 0, "started_at": "t", "shutdown_intent": null }).to_string(),
        )
        .unwrap();
        let reclaimed = Lockfile::acquire(&path, "t".into());
        assert!(reclaimed.is_ok(), "a stale lock must be reclaimable");
        reclaimed.unwrap().remove().unwrap();
    }
}
