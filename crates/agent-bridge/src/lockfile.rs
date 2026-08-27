//! The runtime lockfile: one live instance per name, held by an operating-system
//! lock, and the operator-intent signal a supervisor reads after the runtime
//! exits.
//!
//! Contention is decided by the operating system, not by comparing process
//! ids. The runtime opens `runtime.lock` and takes an exclusive lock on it for
//! its whole life; a second instance's attempt fails while the first holds it,
//! and — the property a pid scheme cannot match — the lock is released the
//! instant the holder dies, cleanly or not. That removes the failure modes a
//! pid-based scheme carries: a stale lock never blocks a restart, a recycled
//! pid never reads as a live runtime, and there is no window in which a
//! half-written or empty lock is observed, because a would-be second instance
//! never inspects the contents to reach its decision.
//!
//! The contents — `{ pid, started_at, shutdown_intent }` — are for a supervisor
//! or `doctor` to read *after* the runtime exits, never for acquisition.
//! `shutdown_intent` is written before the drain on every operator path, so a
//! kill between the signal and the exit still carries the fact; on a clean exit
//! the record is emptied rather than the file unlinked, so the lock guards the
//! inode for the whole process lifetime — an empty file then reads as a clean
//! exit, a populated one as a crash or kill.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

/// The exit code a refused second instance ends with — the design's reserved
/// code for "another instance is already running".
pub const SECOND_INSTANCE_EXIT_CODE: i32 = 4;

/// A held lock. It owns the open, locked file: holding it holds the lock, and
/// dropping it — on a clean exit or a crash the operating system unwinds —
/// releases the lock for the next instance. Removal of the on-disk file is a
/// separate, deliberate act on a clean exit ([`Lockfile::remove`]).
#[derive(Debug)]
pub struct Lockfile {
    /// The open, exclusively-locked file. Its lifetime *is* the lock's.
    file: File,
    path: PathBuf,
    pid: u32,
    started_at: String,
}

/// Why a lock could not be acquired.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// A live instance already holds the lock for this name.
    #[error("another agent-bridge instance is already running (holds {path})")]
    AlreadyRunning {
        /// The lockfile another instance holds.
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
    /// Acquire the lock at `path` by opening it and taking an exclusive
    /// operating-system lock, then writing this process's pid and start time
    /// with no shutdown intent yet.
    ///
    /// If another live instance holds the lock this fails with
    /// [`LockError::AlreadyRunning`]. A lock left by a crashed instance does not
    /// block acquisition: the operating system released it when that process
    /// died, so this open takes it cleanly. `started_at` is supplied by the
    /// caller (an RFC 3339 string) so the whole record is deterministic to test.
    pub fn acquire(path: &Path, started_at: String) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let io = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        let Some(file) = platform::open_locked(path).map_err(io)? else {
            return Err(LockError::AlreadyRunning {
                path: path.to_path_buf(),
            });
        };
        let lock = Self {
            file,
            path: path.to_path_buf(),
            pid: std::process::id(),
            started_at,
        };
        lock.write(&serde_json::Value::Null).map_err(io)?;
        Ok(lock)
    }

    /// Record operator shutdown intent before the drain begins. Called on every
    /// operator path — `runtime.shutdown`, SIGTERM, SIGINT, stdin EOF — and
    /// returns its failure so the caller can decide rather than swallow it.
    pub fn write_operator_intent(&self) -> Result<(), LockError> {
        self.write(&serde_json::Value::String("operator".to_string()))
            .map_err(|source| LockError::Io {
                path: self.path.clone(),
                source,
            })
    }

    /// Mark a clean exit by emptying the record, without unlinking the file. A
    /// crash or a kill skips this, leaving the record (and its intent, if any)
    /// for a supervisor to read; after a clean exit the file is present but
    /// empty.
    ///
    /// Emptying rather than unlinking is deliberate: the exclusion is the
    /// operating-system lock on this file's inode, and unlinking the path while
    /// the process still holds that lock would let a second instance recreate
    /// the path and lock a *new* inode before this one exits — two live owners.
    /// Keeping the inode stable holds the lock for the whole process lifetime.
    /// Takes `&self` so it composes with the shared handle the operator-intent
    /// closure holds.
    pub fn clear(&self) -> Result<(), LockError> {
        self.file.set_len(0).map_err(|source| LockError::Io {
            path: self.path.clone(),
            source,
        })
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

    /// Rewrite the held file's contents in place. In place rather than a
    /// temp-and-rename, because a rename would leave the lock attached to the
    /// old inode and hand the freshly-published name to a second instance
    /// unlocked; a reader only ever inspects the contents after the runtime has
    /// exited, when the write is complete.
    fn write(&self, shutdown_intent: &serde_json::Value) -> std::io::Result<()> {
        let body = self.body(shutdown_intent);
        let mut file = &self.file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(body.as_bytes())?;
        file.flush()
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    /// Open `path` and take an exclusive advisory lock on it. `Ok(None)` means
    /// another live instance holds the lock. The lock is tied to the open file
    /// description, so it releases when the file is closed — including when the
    /// kernel closes it as it tears down a crashed process.
    pub(super) fn open_locked(path: &Path) -> std::io::Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Never truncate on open: an existing lock is only ever left by a
            // crashed instance, and its contents are rewritten deliberately
            // after the lock is held, not blanked on the way in.
            .truncate(false)
            .open(path)?;
        // SAFETY: `flock` takes an owned descriptor and a flag and touches no
        // memory. `LOCK_NB` makes it non-blocking, so a held lock returns
        // `EWOULDBLOCK` rather than parking.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked == 0 {
            return Ok(Some(file));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        Err(error)
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    /// `FILE_SHARE_READ | FILE_SHARE_DELETE`, and deliberately not
    /// `FILE_SHARE_WRITE`: the record stays readable by a supervisor or
    /// `doctor` and the file deletable on a clean exit, while the *write* access
    /// a second instance's open requests is not shared — so that open fails with
    /// a sharing violation and the lock stays single-writer. There is no
    /// separate lock call — this exclusion *is* the lock, released when the
    /// handle closes, which the OS does as it tears down a crashed process.
    /// Sharing read matters: the whole point of the file's contents is to be
    /// read, and denying it would fail even a plain reader on Windows.
    const FILE_SHARE_READ_DELETE: u32 = 0x0000_0001 | 0x0000_0004;
    /// `ERROR_SHARING_VIOLATION` — what a second instance's open returns while
    /// the first holds the file.
    const ERROR_SHARING_VIOLATION: i32 = 32;

    /// Open `path` for write-exclusive access, readable by others. `Ok(None)`
    /// means another live instance holds it.
    pub(super) fn open_locked(path: &Path) -> std::io::Result<Option<File>> {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Preserve a crashed instance's contents; the record is rewritten
            // deliberately once the file is held, not blanked on open.
            .truncate(false)
            .share_mode(FILE_SHARE_READ_DELETE)
            .open(path)
        {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
            Err(error) => Err(error),
        }
    }
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

    fn read_record(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn acquire_writes_the_record_and_intent_updates_in_place() {
        let path = temp_lock("basic");
        let lock = Lockfile::acquire(&path, "2026-08-27T00:00:00.000Z".into()).unwrap();
        let body = read_record(&path);
        assert_eq!(body["pid"], std::process::id());
        assert!(body["shutdown_intent"].is_null());

        lock.write_operator_intent().unwrap();
        assert_eq!(read_record(&path)["shutdown_intent"], "operator");

        lock.clear().unwrap();
        assert!(
            std::fs::read(&path).unwrap().is_empty(),
            "a clean exit empties the record without unlinking the file"
        );
    }

    #[test]
    fn a_held_lock_refuses_a_second_and_releasing_it_frees_the_name() {
        let path = temp_lock("contended");
        let first = Lockfile::acquire(&path, "t".into()).unwrap();

        // A second acquisition while the first is held is refused — the OS lock
        // conflicts even from within this same process, on a distinct handle.
        match Lockfile::acquire(&path, "t".into()) {
            Err(LockError::AlreadyRunning { .. }) => {}
            other => panic!("a held lock must refuse a second, got {other:?}"),
        }

        // Dropping the first releases the OS lock, exactly as a crash would.
        drop(first);

        // The name is now free, so a fresh instance — a restart — takes it. This
        // is the behaviour a pid scheme got wrong: no stale lock, no pid-reuse
        // false positive blocking the restart.
        let restart = Lockfile::acquire(&path, "t".into());
        assert!(
            restart.is_ok(),
            "releasing the lock must free the name for a restart"
        );
        restart.unwrap().clear().unwrap();
    }

    #[test]
    fn a_cleared_lock_still_excludes_until_the_holder_drops() {
        let path = temp_lock("cleared");
        let lock = Lockfile::acquire(&path, "t".into()).unwrap();
        lock.clear().unwrap();

        // The record is emptied, but the file — and the OS lock on its inode —
        // is still held, so a second instance is still refused. This is the
        // property unlinking on a clean exit would break: it would free the
        // inode and let a second instance lock a freshly-created one before the
        // first process exits.
        match Lockfile::acquire(&path, "t".into()) {
            Err(LockError::AlreadyRunning { .. }) => {}
            other => panic!("a cleared-but-held lock must still refuse a second, got {other:?}"),
        }

        // Only once the holder drops does the name free up again.
        drop(lock);
        Lockfile::acquire(&path, "t".into())
            .expect("the dropped holder frees the name")
            .clear()
            .unwrap();
    }
}
