//! The report-line protocol between this package's fixture children
//! (`utf8-child`, `tree-child`) and the probes that spawn them.
//!
//! The fixture's stdout is a PTY slave, so its reports travel through the
//! terminal to the probe reading the master — decorated with whatever escape
//! sequences the terminal adds (ConPTY brackets everything). The protocol is
//! therefore line-oriented and survives ANSI-stripping: one report per line,
//! space-separated `key=value` fields, no spaces inside values.
//!
//! This module is shared by both sides — the fixture formats with it, the
//! probe parses with it — so the two can never disagree about a field name
//! or a byte spelling. The UTF-8 corpus and its own line format live in
//! [`corpus`], shared for the same reason.

pub mod corpus;

/// Every report line starts with this token; anything else on the terminal
/// (echo noise, escape-sequence residue) is ignored by the parser. One
/// prefix for every fixture in this package: the prefix names the protocol,
/// not the binary.
pub const REPORT_PREFIX: &str = "probe-child";

/// Written by the probe to end the fixture: the fixture exits 0 without
/// reporting it as data. Anything but 0x03 would do; `q` reads naturally in
/// a captured log.
pub const QUIT_BYTE: u8 = b'q';

/// Written by the probe to tell the tree-child fixture to grow its process
/// tree. Deliberately a separate step from startup: on Windows the probe
/// must bind the root to its job object *before* any descendant exists, or
/// the descendants would spawn outside the job and the membership
/// assertions would be racing the fixture.
pub const TREE_BYTE: u8 = b't';

/// The terminal is configured, the interrupt handler installed, and the
/// read loop about to start — the probe must not write before this arrives.
pub const EVENT_READY: &str = "ready";

/// One byte arrived on stdin as ordinary data.
pub const EVENT_BYTE: &str = "byte";

/// The interrupt handler fired: SIGINT on POSIX, the console ctrl handler
/// on Windows. Which one is in the `via` field, [`INTERRUPT_VIA`].
pub const EVENT_INTERRUPT: &str = "interrupt";

/// The quit byte arrived; the fixture is exiting 0. Carries `bytes` and
/// `interrupts` totals so a run is summarized in its last line.
pub const EVENT_QUIT: &str = "quit";

/// Stdin reached end-of-file — the master side closed. A clean end for a
/// fixture, exit 0.
pub const EVENT_EOF: &str = "eof";

/// The watchdog deadline passed with no quit byte; the fixture exits 9
/// rather than outlive an orphaned run.
pub const EVENT_WATCHDOG: &str = "watchdog";

/// The answer to [`TREE_BYTE`]: the tree-child fixture has grown its tree.
/// Carries `ingroup` (the PID of the descendant sharing the root's process
/// group / job) and `escape` — either the PID of the descendant that left
/// the root's process group via `setsid` (POSIX), or [`ESCAPE_DENIED`] when
/// the OS refused the breakaway (the expected Windows outcome under a job
/// object without breakaway permission).
pub const EVENT_TREE: &str = "tree";

/// A polite-termination request was observed and deliberately survived: the
/// stubborn fixture's SIGTERM handler fired (POSIX) or its console ctrl
/// handler swallowed a ctrl event (Windows). Carries `count` and `via`
/// ([`TERM_VIA`]). Only the stubborn mode reports these — the clean mode
/// keeps the default disposition and simply dies.
pub const EVENT_TERM: &str = "term";

/// The `escape` field value when the OS denied the escape attempt at spawn.
pub const ESCAPE_DENIED: &str = "denied";

/// What delivered the survived polite termination on this platform. One
/// value per build, mirroring [`INTERRUPT_VIA`].
pub const TERM_VIA: &str = if cfg!(windows) {
    "console-ctrl-handler"
} else {
    "sigterm-handler"
};

/// What delivered the interrupt on this platform. One value per build: the
/// probe and the fixture always run on the same OS.
pub const INTERRUPT_VIA: &str = if cfg!(windows) {
    "console-ctrl-handler"
} else {
    "sigint-handler"
};

/// The one spelling of a byte both sides use, e.g. `0x03`.
pub fn byte_hex(byte: u8) -> String {
    format!("0x{byte:02x}")
}

/// Format one report line (without the terminating newline). Field values
/// must not contain whitespace — the parser splits on it.
pub fn format_report(event: &str, fields: &[(&str, String)]) -> String {
    debug_assert!(
        fields
            .iter()
            .all(|(_, value)| !value.contains(char::is_whitespace)),
        "report field values must not contain whitespace"
    );
    let mut line = format!("{REPORT_PREFIX} event={event}");
    for (key, value) in fields {
        line.push_str(&format!(" {key}={value}"));
    }
    line
}

/// One parsed report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub event: String,
    fields: Vec<(String, String)>,
}

impl Report {
    /// Parse a single line; `None` for anything that is not a report. The
    /// line is expected to be ANSI-stripped already; stray `\r` and
    /// surrounding whitespace are tolerated.
    pub fn parse(line: &str) -> Option<Self> {
        let mut tokens = line.split_whitespace();
        if tokens.next() != Some(REPORT_PREFIX) {
            return None;
        }
        let mut event = None;
        let mut fields = Vec::new();
        for token in tokens {
            let (key, value) = token.split_once('=')?;
            if key == "event" {
                event = Some(value.to_string());
            } else {
                fields.push((key.to_string(), value.to_string()));
            }
        }
        Some(Self {
            event: event?,
            fields,
        })
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Is this a data report of exactly `byte`?
    pub fn is_byte(&self, byte: u8) -> bool {
        self.event == EVENT_BYTE && self.field("hex") == Some(byte_hex(byte).as_str())
    }
}

/// Renders back to the wire shape (minus the prefix), so a probe's step
/// detail can quote a report verbatim.
impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event={}", self.event)?;
        for (key, value) in &self.fields {
            write!(f, " {key}={value}")?;
        }
        Ok(())
    }
}

/// Every report found in a block of (ANSI-stripped) terminal text, in order.
pub fn reports_in(text: &str) -> Vec<Report> {
    text.lines().filter_map(Report::parse).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_round_trip() {
        let line = format_report(
            EVENT_INTERRUPT,
            &[
                ("count", "1".to_string()),
                ("via", INTERRUPT_VIA.to_string()),
            ],
        );
        let report = Report::parse(&line).expect("a formatted line must parse");
        assert_eq!(report.event, EVENT_INTERRUPT);
        assert_eq!(report.field("count"), Some("1"));
        assert_eq!(report.field("via"), Some(INTERRUPT_VIA));
    }

    #[test]
    fn non_report_lines_are_ignored() {
        assert_eq!(Report::parse(""), None);
        assert_eq!(Report::parse("some terminal noise"), None);
        assert_eq!(Report::parse("probe-childish event=ready"), None);
        // A prefix with no event field is residue, not a report.
        assert_eq!(Report::parse("probe-child"), None);
    }

    #[test]
    fn carriage_returns_and_padding_are_tolerated() {
        // The PTY delivers \r\n line endings; the OS may also indent after
        // a repaint. Neither may hide a report.
        let report = Report::parse("  probe-child event=byte hex=0x03\r").unwrap();
        assert!(report.is_byte(0x03));
        assert!(!report.is_byte(0x71));
    }

    #[test]
    fn byte_spelling_is_two_digit_lowercase_hex() {
        assert_eq!(byte_hex(0x03), "0x03");
        assert_eq!(byte_hex(0xff), "0xff");
    }

    #[test]
    fn reports_are_extracted_from_surrounding_noise_in_order() {
        let text = "boot noise\r\nprobe-child event=ready mode=raw\r\n> \r\nprobe-child event=byte hex=0x03\r\n";
        let reports = reports_in(text);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].event, EVENT_READY);
        assert!(reports[1].is_byte(0x03));
    }

    #[test]
    fn display_round_trips_through_parse() {
        let line = format_report(EVENT_READY, &[("mode", "raw".to_string())]);
        let report = Report::parse(&line).unwrap();
        assert_eq!(format!("{REPORT_PREFIX} {report}"), line);
    }

    #[test]
    fn the_quit_byte_is_not_the_interrupt_byte() {
        // The whole probe hinges on 0x03 being observed in isolation; a quit
        // byte that collided with it would corrupt every scenario.
        assert_ne!(QUIT_BYTE, 0x03);
    }

    #[test]
    fn the_control_bytes_are_distinct() {
        // A tree request that collided with the quit byte would end a run
        // instead of growing a tree, and one that collided with 0x03 would
        // grow a tree every time a probe tested the interrupt byte.
        assert_ne!(TREE_BYTE, QUIT_BYTE);
        assert_ne!(TREE_BYTE, 0x03);
    }

    #[test]
    fn the_denied_escape_marker_cannot_be_mistaken_for_a_pid() {
        // The `escape` field carries either a PID or this marker; a marker
        // that parsed as a number would let a denial masquerade as a live
        // escapee.
        assert!(ESCAPE_DENIED.parse::<u32>().is_err());
        assert!(!ESCAPE_DENIED.contains(char::is_whitespace));
    }
}
