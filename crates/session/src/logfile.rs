//! The per-session log stream: one NDJSON file per session, in the
//! runtime's log-record shape.
//!
//! A dedicated writer rather than a per-session `tracing` subscriber:
//! filtering one global subscriber per session is the awkward path, and the
//! session log's contract is a *file shape*, not a tracing layer. `tracing`
//! still carries the runtime-log side; this file carries only the
//! session's own stream — lifecycle entries at `info`, the event-metadata
//! mirror at `debug`, payloads only when the operator opted in
//! (`logs.mirror_payloads`, default off).
//!
//! Logging is never load-bearing: the writer runs on its own
//! thread behind a bounded channel, a full channel drops the record and
//! counts the drop, and a write error degrades the writer to discard — the
//! session never stalls on its own diary.

use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::id::SessionId;

/// Version of the *log record* schema — separate from the event stream's
/// `schema_version`, which versions a different contract.
pub const LOG_SCHEMA_VERSION: u32 = 1;

/// Records a slow disk may hold in flight before the writer starts
/// dropping. Small deliberately: slack for a hiccup, not a buffer the
/// session's memory budget has to carry.
const CHANNEL_CAPACITY: usize = 256;

/// A record's severity — the log contract's four levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// The session's log stream: hand it records; it owns the file.
pub(crate) struct SessionLog {
    session_id: String,
    sender: Option<SyncSender<String>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Records the bounded channel could not take. The loss is on the
    /// record here the same way the stream reader counts its dropped
    /// incidents: reporting degrades, the session does not.
    dropped: u64,
}

impl SessionLog {
    /// Open `sessions/<session_id>.log` under `log_dir`, creating the
    /// directory as needed, and start the writer thread.
    pub(crate) fn open(log_dir: &Path, session_id: &SessionId) -> std::io::Result<Self> {
        // Owner-only from creation: with `mirror_payloads` opted in the
        // log can carry session content, and a umask-derived mode would
        // hand it to every local account. On Windows the protection is
        // the profile directory's inherited ACL — the platform's own
        // owner-scoping mechanism — which is why the log directory
        // belongs under a user-private location there.
        let dir = log_dir.join("sessions");
        let mut dir_builder = std::fs::DirBuilder::new();
        dir_builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir_builder.mode(0o700);
        }
        dir_builder.create(&dir)?;
        // Creation-time modes only govern creation: a log root reused
        // from an earlier run — or pre-made by tooling — keeps whatever
        // permissions it had, so the owner-only contract is asserted on
        // the existing directory too, not assumed from the builder.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(format!("{session_id}.log"));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        // Same for the file: the open-time mode applies only when this
        // call created it. A path that already existed is tightened on
        // the handle itself — no window between check and change —
        // before a single record lands.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel::<String>(CHANNEL_CAPACITY);
        let thread_name = format!("session-log-{session_id}");
        let warn_path = path.clone();
        let writer = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut file = std::io::BufWriter::new(file);
                // Once a write fails the writer degrades to discard for
                // good: it keeps draining the channel so senders never
                // notice, and the one warning below is the whole
                // announcement — repeating it per record would be
                // event-spam on a disk that is already full.
                let mut discarding = false;
                while let Ok(line) = receiver.recv() {
                    if discarding {
                        continue;
                    }
                    let mut result = file.write_all(line.as_bytes());
                    // Drain what a burst already queued before paying for
                    // the flush: one syscall per batch rather than per
                    // record, with the same durability point — the flush
                    // still lands before the writer waits for anything
                    // new.
                    while result.is_ok()
                        && let Ok(line) = receiver.try_recv()
                    {
                        result = file.write_all(line.as_bytes());
                    }
                    if let Err(error) = result.and_then(|()| file.flush()) {
                        discarding = true;
                        tracing::warn!(
                            %error,
                            path = %warn_path.display(),
                            "session log write failed; degrading to discard"
                        );
                    }
                }
                let _ = file.flush();
            })?;
        Ok(Self {
            session_id: session_id.to_string(),
            sender: Some(sender),
            writer: Some(writer),
            dropped: 0,
        })
    }

    /// Append one record in the runtime's log-record shape.
    pub(crate) fn record(&mut self, level: LogLevel, event: &str, fields: Map<String, Value>) {
        let record = json!({
            "ts": rfc3339_millis(SystemTime::now()),
            "level": level.as_str(),
            "component": "session",
            "session_id": self.session_id,
            "event": event,
            "schema_version": LOG_SCHEMA_VERSION,
            "fields": Value::Object(fields),
        });
        let mut line = record.to_string();
        line.push('\n');
        let Some(sender) = &self.sender else { return };
        match sender.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped += 1;
            }
        }
    }

    /// Close the stream: no further records, the buffered tail flushed.
    ///
    /// Blocking (it joins the writer thread), so the actor calls it from a
    /// blocking context. Part of the `Closed` cleanup invariants — "log
    /// closed" means this returned, not that it was hopefully about to.
    pub(crate) fn close(mut self) {
        if self.dropped > 0 {
            tracing::warn!(
                session_id = %self.session_id,
                dropped = self.dropped,
                "session log dropped records under backlog"
            );
        }
        drop(self.sender.take());
        if let Some(writer) = self.writer.take()
            && writer.join().is_err()
        {
            tracing::error!(session_id = %self.session_id, "session log writer panicked");
        }
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // A dropped log (actor died some way other than its close path)
        // still releases the thread; the explicit `close` is the flushed,
        // joined ending the cleanup invariants assert.
        drop(self.sender.take());
    }
}

/// The last representable instant: RFC 3339's `date-fullyear` is exactly
/// four digits.
const MAX_RFC3339_SECONDS: u64 = 253_402_300_799;

/// A [`SystemTime`] as the record's `ts`: RFC 3339, millisecond resolution,
/// always UTC.
///
/// A deliberate copy of the same twenty lines the bus's stamping site and
/// the dev-task runner each carry: this crate sits *below* `core` in the
/// dependency direction, so it cannot borrow the bus's, and a calendar
/// dependency for one output shape is a larger surface than the arithmetic.
/// Each copy is pinned by its own tests.
pub(crate) fn rfc3339_millis(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    if since_epoch.as_secs() > MAX_RFC3339_SECONDS {
        return "9999-12-31T23:59:59.999Z".to_owned();
    }
    let seconds = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let second_of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    )
}

/// Days since 1970-01-01 to a proleptic-Gregorian (year, month, day) —
/// Howard Hinnant's `civil_from_days`, restricted to the post-epoch range
/// the caller guarantees.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let (year_offset, month) = if shifted_month < 10 {
        (0, shifted_month + 3)
    } else {
        (1, shifted_month - 9)
    };
    (year_of_era + era * 400 + year_offset, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_format_as_rfc3339_millis() {
        let at = |seconds: u64| rfc3339_millis(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(1_704_067_200), "2024-01-01T00:00:00.000Z");
        // A leap day under the every-400 exception, and the clamp past the
        // four-digit-year range.
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(MAX_RFC3339_SECONDS + 1), "9999-12-31T23:59:59.999Z");
    }

    #[cfg(unix)]
    #[test]
    fn logs_are_owner_only_from_creation() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("agent-bridge-log-perm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = SessionId::new();
        let log = SessionLog::open(&dir, &id).expect("open must succeed");
        let sessions = dir.join("sessions");
        let file = sessions.join(format!("{id}.log"));
        let dir_mode = std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the sessions directory is not owner-only");
        assert_eq!(file_mode, 0o600, "the session log is not owner-only");
        log.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_reused_log_root_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "agent-bridge-log-reuse-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = SessionId::new();
        let file = sessions.join(format!("{id}.log"));
        std::fs::write(&file, b"").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let log = SessionLog::open(&dir, &id).expect("open must succeed");
        let dir_mode = std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "a reused sessions directory kept its loose mode"
        );
        assert_eq!(file_mode, 0o600, "a reused session log kept its loose mode");
        log.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_land_in_the_log_record_shape() {
        let dir =
            std::env::temp_dir().join(format!("agent-bridge-log-test-{}", std::process::id()));
        let session_id = SessionId::new();
        let mut log = SessionLog::open(&dir, &session_id).expect("the log must open");
        let mut fields = Map::new();
        fields.insert("adapter".into(), json!("fixture"));
        log.record(LogLevel::Info, "lifecycle.session.created", fields);
        log.close();

        // The location is contract: sessions/<session_id>.log
        // under the log directory.
        let path = dir.join("sessions").join(format!("{session_id}.log"));
        let text = std::fs::read_to_string(&path).expect("the file must exist");
        let line = text.lines().next().expect("one record was written");
        let record: Value = serde_json::from_str(line).expect("the record is JSON");
        assert_eq!(record["level"], "info");
        assert_eq!(record["component"], "session");
        assert_eq!(record["session_id"], session_id.to_string());
        assert_eq!(record["event"], "lifecycle.session.created");
        assert_eq!(record["schema_version"], LOG_SCHEMA_VERSION);
        assert_eq!(record["fields"]["adapter"], "fixture");
        assert!(record["ts"].as_str().unwrap().ends_with('Z'));
        std::fs::remove_dir_all(&dir).ok();
    }
}
