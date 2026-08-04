//! Checking a generated stream as it arrives.
//!
//! "No corruption over thirty minutes" is only a useful claim if its failure
//! is diagnosable. A run that hashed the whole stream and compared at the end
//! could say *that* something went wrong and never *what*: which line, which
//! byte, how far in, whether once or continuously. So verification is
//! incremental — every line is checked against the line it should have been,
//! the moment it arrives — and a fault carries its line number, its byte
//! offset within the line, and how far into the run it happened.
//!
//! Faults are counted rather than fatal. A stream that loses one line in ten
//! thousand and a stream that loses everything after minute four are both
//! "corrupted", and telling them apart is the entire value of the run; a
//! verifier that stopped at the first fault would turn every result into the
//! same sentence. The first several faults are kept in full, the rest are
//! counted, and the lane fails on the totals.
//!
//! ## What the terminal is allowed to do
//!
//! A POSIX PTY is a byte pipe: what the child wrote is what arrives, modulo
//! the line-terminator rewrite every terminal performs. Anything else is
//! corruption.
//!
//! ConPTY is not a pipe. It renders its child's output into a console screen
//! and re-emits a stream that reproduces that screen, which means repaints,
//! cursor motion, and re-sent lines are all normal traffic. Two consequences,
//! and both are recorded rather than hidden: a line may legitimately arrive
//! more than once (counted as a repaint), and a line may legitimately arrive
//! truncated as part of a partial repaint (counted the same way). Lines that
//! never arrive at all are *not* excused — a terminal that drops output
//! under sustained load is exactly the finding this lane exists to surface —
//! so a gap is a fault on every platform, and the count is the loss rate.
//!
//! The digest resynchronises across a gap by folding in the lines that went
//! missing. Without that, one lost line would fail every checksum for the
//! rest of the run and the run would report a single catastrophic fault
//! instead of one lost line.

use agent_bridge_fake_cli::generator::{Line, Rolling, parse_line, write_payload_line};

/// How many faults are described in full before the rest are only counted.
/// Enough to see whether a fault is isolated or a pattern; few enough that a
/// stream that broke entirely does not produce a million-line diagnostic.
const DETAILED_FAULTS: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    /// Payload lines checked against their expected content and matching.
    pub lines_verified: u64,
    /// Payload lines that never arrived.
    pub lines_lost: u64,
    /// Payload lines whose content was not what that line number should say.
    pub content_faults: u64,
    /// Checksum lines whose digest did not match the payload before them.
    pub checksum_faults: u64,
    /// Checksum lines that matched.
    pub checksums_verified: u64,
    /// Lines that arrived again, or arrived cut short, after their content
    /// had already been accounted for. Normal under a re-rendering terminal;
    /// impossible under a byte pipe, where they are counted as faults too.
    pub repaints: u64,
    /// Lines that are not part of the generated stream at all — banners,
    /// escape residue, whatever else the terminal says.
    pub unrecognized: u64,
    /// The first faults, in full.
    pub detail: Vec<String>,
}

impl Findings {
    pub fn faults(&self) -> u64 {
        self.lines_lost + self.content_faults + self.checksum_faults
    }

    pub fn clean(&self) -> bool {
        self.faults() == 0
    }

    /// One line summarising the run, whether it passed or not.
    pub fn summary(&self) -> String {
        format!(
            "{} lines verified, {} checksums verified, {} lost, {} content faults, \
             {} checksum faults, {} repaints, {} unrecognized lines",
            self.lines_verified,
            self.checksums_verified,
            self.lines_lost,
            self.content_faults,
            self.checksum_faults,
            self.repaints,
            self.unrecognized,
        )
    }
}

pub struct Verifier {
    line_bytes: usize,
    /// Whether the terminal under test re-renders rather than pipes.
    repainting_terminal: bool,
    next_seq: u64,
    rolling: Rolling,
    expected: String,
    findings: Findings,
}

impl Verifier {
    pub fn new(line_bytes: usize, repainting_terminal: bool) -> Self {
        Self {
            line_bytes,
            repainting_terminal,
            next_seq: 0,
            rolling: Rolling::new(),
            expected: String::with_capacity(line_bytes + 32),
            findings: Findings::default(),
        }
    }

    /// The terminal this build measures against re-renders its child's
    /// output (ConPTY does; a POSIX PTY does not).
    pub fn for_this_platform(line_bytes: usize) -> Self {
        Self::new(line_bytes, cfg!(windows))
    }

    pub fn findings(&self) -> &Findings {
        &self.findings
    }

    /// How many payload lines have been accounted for — verified, lost, or
    /// found faulty. What a run compares against what it asked for.
    pub fn accounted(&self) -> u64 {
        self.next_seq
    }

    /// Check one line of arrived output. `at_ns` is how far into the run it
    /// arrived, and appears in fault reports so a fault can be located in
    /// time as well as in the stream.
    pub fn feed(&mut self, line: &str, at_ns: u64) {
        match parse_line(line) {
            Some(Line::Payload { seq, payload }) => self.payload(seq, payload, at_ns),
            Some(Line::Checksum { covered, digest }) => self.checksum(covered, digest, at_ns),
            None => self.findings.unrecognized += 1,
        }
    }

    fn payload(&mut self, seq: u64, payload: &str, at_ns: u64) {
        if seq > self.next_seq {
            // Lines that never arrived. Fold their content in so the digest
            // keeps meaning something for the rest of the run.
            let lost = seq - self.next_seq;
            self.findings.lines_lost += lost;
            self.fault(format!(
                "lines {}..{} never arrived ({lost} lines) at {} ms into the run",
                self.next_seq,
                seq,
                at_ns / 1_000_000
            ));
            for missing in self.next_seq..seq {
                write_payload_line(missing, self.line_bytes, &mut self.expected);
                self.rolling.feed(&self.expected);
            }
            self.next_seq = seq;
        }

        write_payload_line(seq, self.line_bytes, &mut self.expected);
        if seq < self.next_seq {
            // Already accounted for. A re-rendering terminal is entitled to
            // say it again; a byte pipe is not.
            self.repeat(seq, payload, at_ns);
            return;
        }
        if payload_matches(&self.expected, payload) {
            self.rolling.feed(&self.expected);
            self.findings.lines_verified += 1;
            self.next_seq += 1;
            return;
        }
        if self.repainting_terminal && expected_payload(&self.expected).starts_with(payload) {
            // A partial repaint: the line was cut short, and the terminal
            // will send it again. Nothing is accounted for yet.
            self.findings.repaints += 1;
            return;
        }
        self.findings.content_faults += 1;
        let expected = expected_payload(&self.expected).to_string();
        self.fault(format!(
            "line {seq} differs at byte {} ({} ms into the run): expected {:?}, got {:?}",
            first_difference(&expected, payload),
            at_ns / 1_000_000,
            truncate(&expected),
            truncate(payload),
        ));
        // Account for the line anyway: the digest tracks what *should* have
        // been sent, so one bad line does not fail every checksum after it.
        self.rolling.feed(&self.expected);
        self.next_seq += 1;
    }

    fn repeat(&mut self, seq: u64, payload: &str, at_ns: u64) {
        let expected = expected_payload(&self.expected);
        let plausible = expected.starts_with(payload);
        if self.repainting_terminal && plausible {
            self.findings.repaints += 1;
            return;
        }
        self.findings.content_faults += 1;
        self.fault(if plausible {
            format!(
                "line {seq} arrived again at {} ms into the run — a byte pipe does not repeat itself",
                at_ns / 1_000_000
            )
        } else {
            format!(
                "line {seq} arrived again at {} ms into the run, and differently: expected {:?}, got {:?}",
                at_ns / 1_000_000,
                truncate(expected),
                truncate(payload),
            )
        });
    }

    fn checksum(&mut self, covered: u64, digest: u64, at_ns: u64) {
        // A checksum line landing anywhere but the stream's current position
        // is a re-rendering terminal saying something again — ground already
        // checked, nothing new. A byte pipe has no such excuse: the child
        // emits each checksum exactly once, in order, so an out-of-position
        // one there is corruption, the same judgement `repeat` passes on a
        // repeated payload line.
        if covered != self.next_seq {
            if self.repainting_terminal {
                self.findings.repaints += 1;
            } else {
                self.findings.checksum_faults += 1;
                self.fault(format!(
                    "a checksum over the first {covered} lines arrived while the stream stood \
                     at line {} ({} ms into the run) — a byte pipe does not repeat or reorder \
                     its checkpoints",
                    self.next_seq,
                    at_ns / 1_000_000,
                ));
            }
            return;
        }
        if digest == self.rolling.value() {
            self.findings.checksums_verified += 1;
            return;
        }
        self.findings.checksum_faults += 1;
        self.fault(format!(
            "checksum over the first {covered} lines does not match at {} ms into the run: \
             stream says {digest:016x}, the lines that arrived say {:016x}",
            at_ns / 1_000_000,
            self.rolling.value(),
        ));
    }

    fn fault(&mut self, detail: String) {
        if self.findings.detail.len() < DETAILED_FAULTS {
            self.findings.detail.push(detail);
        }
    }

    /// Finish: any line the stream promised and never delivered is lost.
    pub fn finish(mut self, lines_expected: u64) -> Findings {
        if self.next_seq < lines_expected {
            let lost = lines_expected - self.next_seq;
            self.findings.lines_lost += lost;
            self.fault(format!(
                "the run ended at line {} of {lines_expected} — {lost} lines never arrived",
                self.next_seq
            ));
        }
        self.findings
    }
}

/// The payload half of a rendered `L<seq> <payload>` line.
fn expected_payload(line: &str) -> &str {
    line.split_once(' ').map_or("", |(_, payload)| payload)
}

fn payload_matches(expected_line: &str, payload: &str) -> bool {
    expected_payload(expected_line) == payload
}

fn first_difference(expected: &str, got: &str) -> usize {
    expected
        .as_bytes()
        .iter()
        .zip(got.as_bytes())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| expected.len().min(got.len()))
}

/// Fault details name what differed; they are not a place to paste a
/// kilobyte of generated filler. The cut lands on a character boundary:
/// arrived text is lossily decoded, so a corrupt line can carry multi-byte
/// characters, and a fault report that panics mid-format would replace the
/// diagnosis with a crash.
fn truncate(text: &str) -> String {
    const KEEP: usize = 24;
    if text.len() <= KEEP {
        return text.to_string();
    }
    let cut = (0..=KEEP)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0);
    format!("{}…(+{} bytes)", &text[..cut], text.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_fake_cli::generator::{checksum_line, payload_line};

    const BYTES: usize = 16;

    /// Feed a well-formed stream of `lines` payload lines with a checksum
    /// every `checksum_every`, letting the caller corrupt it first.
    fn stream(lines: u64, checksum_every: u64) -> Vec<String> {
        let mut out = Vec::new();
        let mut rolling = Rolling::new();
        for seq in 0..lines {
            let line = payload_line(seq, BYTES);
            rolling.feed(&line);
            out.push(line);
            if checksum_every > 0 && (seq + 1) % checksum_every == 0 {
                out.push(checksum_line(seq + 1, rolling.value()));
            }
        }
        out
    }

    fn verify(lines: &[String], repainting: bool, expected: u64) -> Findings {
        let mut verifier = Verifier::new(BYTES, repainting);
        for (index, line) in lines.iter().enumerate() {
            verifier.feed(line, index as u64 * 1_000_000);
        }
        verifier.finish(expected)
    }

    #[test]
    fn an_intact_stream_is_clean() {
        let findings = verify(&stream(200, 50), false, 200);
        assert!(findings.clean(), "{}", findings.summary());
        assert_eq!(findings.lines_verified, 200);
        assert_eq!(findings.checksums_verified, 4);
    }

    #[test]
    fn a_lost_line_is_located_in_the_stream_and_in_time() {
        let mut lines = stream(200, 50);
        lines.remove(20); // line 20 never arrives
        let findings = verify(&lines, false, 200);
        assert_eq!(findings.lines_lost, 1);
        assert!(
            findings.detail[0].contains("lines 20..21"),
            "the fault must name the lines: {:?}",
            findings.detail
        );
        assert!(
            findings.detail[0].contains(" ms into the run"),
            "the fault must locate itself in time: {:?}",
            findings.detail
        );
    }

    /// The reason a lost line resynchronises the digest: without it, one
    /// gap turns every later checksum into a fault and the run reports a
    /// catastrophe instead of a dropped line.
    #[test]
    fn a_lost_line_does_not_poison_every_checksum_after_it() {
        let mut lines = stream(500, 50);
        lines.remove(20);
        let findings = verify(&lines, false, 500);
        assert_eq!(findings.lines_lost, 1);
        assert_eq!(findings.checksum_faults, 0);
        assert_eq!(findings.checksums_verified, 10);
    }

    #[test]
    fn a_changed_byte_is_reported_with_its_offset() {
        let mut lines = stream(100, 0);
        let corrupted = lines[40].clone();
        let mut bytes = corrupted.into_bytes();
        let at = bytes.len() - 3;
        bytes[at] = if bytes[at] == b'a' { b'b' } else { b'a' };
        lines[40] = String::from_utf8(bytes).expect("still ASCII");

        let findings = verify(&lines, false, 100);
        assert_eq!(findings.content_faults, 1);
        assert_eq!(findings.lines_lost, 0);
        let detail = &findings.detail[0];
        assert!(detail.contains("line 40 differs"), "{detail}");
        assert!(
            detail.contains(&format!("byte {}", BYTES - 3)),
            "the fault must name the byte offset: {detail}"
        );
    }

    #[test]
    fn a_truncated_stream_reports_what_never_came() {
        let findings = verify(&stream(100, 0), false, 500);
        assert_eq!(findings.lines_lost, 400);
        assert!(
            findings
                .detail
                .last()
                .unwrap()
                .contains("ended at line 100"),
            "{:?}",
            findings.detail
        );
    }

    #[test]
    fn faults_are_counted_past_the_point_they_stop_being_described() {
        let lines: Vec<String> = stream(1000, 0)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| index % 2 == 0)
            .map(|(_, line)| line)
            .collect();
        let findings = verify(&lines, false, 1000);
        assert_eq!(findings.lines_lost, 500, "half the stream is missing");
        assert_eq!(
            findings.detail.len(),
            DETAILED_FAULTS,
            "the detail must stay bounded while the count does not"
        );
    }

    #[test]
    fn a_repeated_line_is_a_repaint_on_a_rendering_terminal_and_a_fault_on_a_pipe() {
        let mut lines = stream(50, 0);
        lines.insert(10, payload_line(9, BYTES)); // line 9, said twice

        let repainting = verify(&lines, true, 50);
        assert!(repainting.clean(), "{}", repainting.summary());
        assert_eq!(repainting.repaints, 1);

        let piped = verify(&lines, false, 50);
        assert_eq!(piped.content_faults, 1);
        assert!(
            piped.detail[0].contains("does not repeat itself"),
            "{:?}",
            piped.detail
        );
    }

    #[test]
    fn a_cut_short_line_is_a_partial_repaint_on_a_rendering_terminal() {
        let mut lines = stream(50, 0);
        let full = payload_line(20, BYTES);
        lines.insert(20, full[..full.len() - 6].to_string());

        let repainting = verify(&lines, true, 50);
        assert!(repainting.clean(), "{}", repainting.summary());
        assert_eq!(repainting.repaints, 1);
        assert_eq!(repainting.lines_verified, 50, "the full line still arrives");

        // On a pipe the same traffic is two anomalies, and both are owed to
        // the reader: a truncated line arrived, and then a line the stream
        // had already said arrived again.
        let piped = verify(&lines, false, 50);
        assert_eq!(
            piped.content_faults, 2,
            "a byte pipe neither truncates nor repeats"
        );
    }

    /// A repaint must not be a hiding place: a re-sent line whose content is
    /// wrong is corruption on every platform.
    #[test]
    fn a_repeated_line_with_different_content_is_a_fault_even_when_repaints_are_allowed() {
        let mut lines = stream(50, 0);
        lines.insert(10, format!("L9 {}", "z".repeat(BYTES)));
        let findings = verify(&lines, true, 50);
        assert_eq!(findings.content_faults, 1);
        assert!(
            findings.detail[0].contains("and differently"),
            "{:?}",
            findings.detail
        );
    }

    #[test]
    fn a_wrong_checksum_is_reported_against_what_actually_arrived() {
        let mut lines = stream(100, 50);
        // The checksum line sits right after line 49.
        lines[50] = checksum_line(50, 0xdead_beef);
        let findings = verify(&lines, false, 100);
        assert_eq!(findings.checksum_faults, 1);
        assert_eq!(findings.lines_lost, 0);
        assert!(
            findings.detail[0].contains("deadbeef"),
            "the fault must quote what the stream claimed: {:?}",
            findings.detail
        );
    }

    /// A checksum line saying something the stream's position contradicts:
    /// on a re-rendering terminal that is a repaint of ground already
    /// checked; on a byte pipe it is impossible traffic, and letting it
    /// pass as a repaint would leave a clean verdict on a corrupt run.
    #[test]
    fn an_out_of_position_checksum_is_a_fault_on_a_pipe_and_a_repaint_on_a_rendering_terminal() {
        let mut lines = stream(100, 50);
        let duplicate = lines[50].clone(); // the C50 checkpoint, said again
        lines.push(duplicate);

        let repainting = verify(&lines, true, 100);
        assert!(repainting.clean(), "{}", repainting.summary());
        assert_eq!(repainting.repaints, 1);

        let piped = verify(&lines, false, 100);
        assert_eq!(piped.checksum_faults, 1);
        assert!(!piped.clean(), "impossible traffic must not verify clean");
        assert!(
            piped.detail[0].contains("does not repeat or reorder"),
            "{:?}",
            piped.detail
        );
    }

    /// Fault formatting must survive whatever bytes actually arrived — a
    /// lossily-decoded corrupt line can put a multi-byte character across
    /// the truncation point, and a fault report that panics mid-format
    /// replaces the diagnosis with a crash.
    #[test]
    fn fault_details_truncate_multi_byte_content_without_panicking() {
        // 1 + 3·k byte boundaries: byte 24 falls mid-character.
        let awkward = format!("x{}", "€".repeat(10));
        let cut = truncate(&awkward);
        assert!(cut.contains('…'), "long content must be summarised: {cut}");
        assert!(cut.starts_with("x€"), "the kept prefix survives: {cut}");

        // And end to end: a corrupt line carrying that content must produce
        // a fault, not a panic.
        let mut lines = stream(10, 0);
        lines[5] = format!("L5 {}", "€".repeat(20));
        let findings = verify(&lines, false, 10);
        assert_eq!(findings.content_faults, 1);
    }

    #[test]
    fn terminal_noise_is_neither_a_fault_nor_a_verified_line() {
        let mut lines = stream(20, 0);
        lines.insert(5, "Microsoft Windows [Version 10.0.22621.1]".to_string());
        lines.insert(9, String::new());
        let findings = verify(&lines, false, 20);
        assert!(findings.clean(), "{}", findings.summary());
        assert_eq!(findings.unrecognized, 2);
    }

    #[test]
    fn long_payloads_are_summarised_in_fault_details_not_pasted() {
        let long = "q".repeat(400);
        assert!(truncate(&long).contains("+376 bytes"));
        assert!(truncate("short").len() == 5);
    }
}
