//! Waiting on and counting fixture reports in a PTY output stream — the
//! bridge between [`crate::pty::OutputTracker`] and the report-line
//! protocol the fixture children speak (`agent_bridge_probe_child`).
//! Shared by every probe that spawns one of those fixtures, so a fix to
//! how reports are found under terminal decoration lands in all of them
//! at once.

use std::time::Duration;

use agent_bridge_probe_child::{Report, reports_in};

use crate::pty::{OutputTracker, strip_ansi};

/// Wait until the fixture has reported something matching `matches`, then
/// hand the matched report back. Reports are parsed from the ANSI-stripped
/// view of the output, so ConPTY's decoration never hides one.
pub fn wait_for_report(
    tracker: &mut OutputTracker,
    what: &str,
    matches: impl Fn(&Report) -> bool,
    timeout: Duration,
) -> Result<Report, String> {
    tracker.wait_for_text(
        what,
        |raw| reports_in(&strip_ansi(raw)).iter().any(&matches),
        timeout,
    )?;
    reports_in(&tracker.visible_text())
        .into_iter()
        .find(&matches)
        // Unreachable while the recent-output window (64 KiB) dwarfs a
        // fixture transcript (hundreds of bytes); stated rather than assumed.
        .ok_or_else(|| format!("{what} was trimmed from the output window before extraction"))
}

/// How many currently-visible reports match. Callers comparing this across
/// a resize should prefer matching on a monotonic `seq` field instead:
/// ConPTY repaints the screen after a resize, so one report line can appear
/// more than once in the stream.
pub fn count_reports(tracker: &OutputTracker, matches: impl Fn(&Report) -> bool) -> usize {
    reports_in(&tracker.visible_text())
        .iter()
        .filter(|report| matches(report))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firsttoken::FirstTokenClock;
    use crate::pty::{EndInfo, ReaderEvent};
    use agent_bridge_probe_child::{EVENT_BYTE, EVENT_INTERRUPT, EVENT_QUIT, EVENT_READY};
    use std::sync::mpsc;
    use std::time::Instant;

    /// A tracker whose stream carries `chunks` and then ends — the shape a
    /// completed fixture run leaves behind, so pumps return promptly
    /// instead of erroring on a disconnected channel.
    fn tracker_with(chunks: &[&str]) -> OutputTracker {
        let (tx, events) = mpsc::channel();
        for chunk in chunks {
            tx.send(ReaderEvent::Data {
                at: Instant::now(),
                bytes: chunk.as_bytes().to_vec(),
            })
            .unwrap();
        }
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None)
    }

    #[test]
    fn reports_are_found_under_ansi_decoration_and_split_chunks() {
        // ConPTY brackets output with escape sequences and a report can
        // arrive split across reads; neither may hide it.
        let mut tracker = tracker_with(&[
            "\x1b[2J\x1b[1;1Hprobe-child event=ready mode=raw pg",
            "id=42 isig=off\r\n",
        ]);
        let ready = wait_for_report(
            &mut tracker,
            "ready",
            |report| report.event == EVENT_READY,
            Duration::from_secs(5),
        )
        .expect("the ready report must be found");
        assert_eq!(ready.field("pgid"), Some("42"));
        assert_eq!(ready.field("isig"), Some("off"));
    }

    #[test]
    fn a_missing_report_times_out_instead_of_hanging() {
        let (tx, events) = mpsc::channel::<ReaderEvent>();
        let _keep_stream_open = tx;
        let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);
        let err = wait_for_report(
            &mut tracker,
            "the fixture's quit report",
            |report| report.event == EVENT_QUIT,
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(err.contains("within"), "unexpected error: {err}");
        assert!(
            err.contains("quit report"),
            "the awaited thing must be named: {err}"
        );
    }

    #[test]
    fn counting_matches_only_the_asked_for_reports() {
        let mut tracker = tracker_with(&[
            "probe-child event=byte hex=0x03\r\n",
            "probe-child event=byte hex=0x1b\r\n",
            "probe-child event=interrupt count=1 via=sigint-handler\r\n",
            "unrelated terminal noise\r\n",
        ]);
        tracker.pump(Duration::from_millis(100)).unwrap();
        assert_eq!(
            count_reports(&tracker, |report| report.event == EVENT_BYTE),
            2
        );
        assert_eq!(count_reports(&tracker, |report| report.is_byte(0x03)), 1);
        assert_eq!(
            count_reports(&tracker, |report| report.event == EVENT_INTERRUPT),
            1
        );
    }
}
