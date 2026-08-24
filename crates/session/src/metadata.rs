//! What is known *about* a session, as distinct from what it is doing.
//!
//! The session's descriptive record: adapter, geometry, the three
//! lifecycle timestamps, how the child ended, and the byte counts.
//! Everything here is safe to print — it is shape and counters, never
//! content — which is what lets the containing types keep a readable
//! `Debug` without transitively printing session content.

use std::time::SystemTime;

use agent_bridge_pty::{Dimensions, ExitStatus};

/// A session's descriptive record, readable at any point in its life.
///
/// Fields fill in as the session reaches them: a session that never
/// produced output has no `started_at`, one that was killed after a drain
/// timeout has an [`ExitStatus::Killed`] rather than a code, and one that
/// failed at launch has neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    /// Name of the adapter hosting the session — the registry key.
    pub adapter: String,
    /// The terminal geometry the session currently holds.
    pub dimensions: Dimensions,
    /// When the registry entry came to exist.
    pub created_at: SystemTime,
    /// When first output was observed (`Running`), if it ever was.
    pub started_at: Option<SystemTime>,
    /// When the session reached `Closed`, once it has.
    pub closed_at: Option<SystemTime>,
    /// How the child ended, when that is known. `None` while live, and for
    /// a session that failed before a child existed.
    pub exit: Option<ExitStatus>,
    /// Bytes read from the child over the session's lifetime, before any
    /// control-sequence stripping.
    pub bytes_read: u64,
    /// Bytes written to the child's input over the session's lifetime.
    pub bytes_written: u64,
}

impl SessionMetadata {
    /// The exit code for the `lifecycle.session.closed` payload: present
    /// only when the child exited with one — a killed child has no code,
    /// and reporting the signal number as one would let "killed by
    /// SIGTERM" read as an exit the child chose.
    ///
    /// A code past `i32::MAX` — a Windows crash status such as
    /// `0xC0000005` — wraps to the conventional negative value rather
    /// than vanishing: the same reading the standard library gives such
    /// exits, and a caller searching for the status can recognize the
    /// bits where an absent code would tell it nothing.
    pub fn exit_code(&self) -> Option<i32> {
        match &self.exit {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "the wrap is the point: Windows crash statuses keep their bits"
            )]
            Some(ExitStatus::Code(code)) => Some(*code as i32),
            Some(ExitStatus::Killed(_)) | None => None,
        }
    }

    /// How long the session lived, create to close, in milliseconds — the
    /// `lifecycle.session.closed` payload's duration. `None` until closed,
    /// or if the wall clock moved backwards across the session's life.
    pub fn duration_ms(&self) -> Option<u64> {
        let closed_at = self.closed_at?;
        let lived = closed_at.duration_since(self.created_at).ok()?;
        u64::try_from(lived.as_millis()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn metadata() -> SessionMetadata {
        SessionMetadata {
            adapter: "fixture".to_string(),
            dimensions: Dimensions::DEFAULT,
            created_at: SystemTime::UNIX_EPOCH,
            started_at: None,
            closed_at: None,
            exit: None,
            bytes_read: 0,
            bytes_written: 0,
        }
    }

    #[test]
    fn a_killed_child_reports_no_exit_code() {
        let mut record = metadata();
        record.exit = Some(ExitStatus::Killed("SIGKILL".to_string()));
        assert_eq!(record.exit_code(), None);
        record.exit = Some(ExitStatus::Code(3));
        assert_eq!(record.exit_code(), Some(3));
    }

    #[test]
    fn a_windows_crash_status_keeps_its_bits_as_the_negative_reading() {
        // STATUS_ACCESS_VIOLATION does not fit an i32; dropping it would
        // make a crash indistinguishable from an unknown ending, so it
        // wraps to the value the platform's own tooling reports.
        let mut record = metadata();
        record.exit = Some(ExitStatus::Code(0xC000_0005));
        assert_eq!(record.exit_code(), Some(-1_073_741_819));
    }

    #[test]
    fn duration_spans_create_to_close() {
        let mut record = metadata();
        assert_eq!(record.duration_ms(), None, "unclosed sessions have none");
        record.closed_at = Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1500));
        assert_eq!(record.duration_ms(), Some(1500));
    }
}
