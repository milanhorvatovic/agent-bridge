//! UTF-8 emission fixture — the controlled child the UTF-8 probe spawns
//! under a PTY to drive multi-byte UTF-8 through a real terminal path with
//! write boundaries deliberately placed mid-codepoint. It emits the shared
//! corpus (`agent_bridge_probe_child::corpus`) one line per item, writing
//! each payload in several unbuffered slices split inside encoded
//! sequences — every write is one syscall, so each split is a boundary the
//! OS may deliver as its own chunk — and ends with a `utf8-end` report
//! carrying totals and an FNV-1a 64 checksum over exactly the payload
//! bytes it wrote. The spawning probe reassembles the stream and holds it
//! to that trailer.
//!
//! Two modes: `valid` emits the corpus alone; `invalid` appends one extra
//! line whose payload embeds bytes that can never be UTF-8 (a lone
//! continuation byte and an overlong pair) between valid neighbors, with a
//! write boundary inside the junk itself. What a terminal does with those
//! bytes is platform truth for the probe to record — this fixture's only
//! job is to put them on the wire exactly as specified.
//!
//! All output bypasses Rust's standard stdout. On POSIX its buffer would
//! merge the deliberately split writes back together; on Windows the
//! console writer re-encodes through UTF-16, which can neither hold a
//! partial UTF-8 sequence across a flush nor carry an invalid byte at all.
//! The fixture writes raw bytes instead — `write(2)` on POSIX, `WriteFile`
//! on Windows with the console code pages forced to UTF-8 first — so the
//! bytes on the wire are the bytes in the corpus.
//!
//! Unlike its sibling fixtures this child reads nothing and exits on its
//! own, so it needs neither a quit byte nor a watchdog: its entire output
//! is a few hundred bytes, far below any PTY buffer, so it can never block
//! forever against a vanished reader — it writes, reports, and exits.
//!
//! Exit codes: 0 done, 2 usage error, 3 the console code pages could not
//! be configured (Windows), 4 a write failed.

use agent_bridge_probe_child::corpus::{
    CorpusSummary, EVENT_UTF8_END, Fnv1a64, UTF8_MODE_INVALID, UTF8_MODE_VALID, corpus_line_lead,
    corpus_lines, split_at_offsets,
};
use agent_bridge_probe_child::{EVENT_READY, format_report};

/// The platform-specific fields the ready report carries — on Windows the
/// code pages before and after configuring, so a run records the exact
/// console contract it observed.
type TermFields = Vec<(&'static str, String)>;

#[derive(Clone, Copy)]
enum Mode {
    Valid,
    Invalid,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Valid => UTF8_MODE_VALID,
            Mode::Invalid => UTF8_MODE_INVALID,
        }
    }

    fn include_junk(self) -> bool {
        matches!(self, Mode::Invalid)
    }
}

fn main() {
    let mode = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("utf8-child: {message}");
            std::process::exit(2);
        }
    };
    let (session, term_fields) = match platform::configure() {
        Ok(configured) => configured,
        Err(detail) => {
            eprintln!("utf8-child: console setup failed: {detail}");
            std::process::exit(3);
        }
    };

    // Everything after this line is emitted under the reported
    // configuration — on Windows, with the code pages verified as UTF-8.
    let mut fields = vec![
        ("mode", mode.name().to_string()),
        ("os", std::env::consts::OS.to_string()),
        ("pid", std::process::id().to_string()),
    ];
    fields.extend(term_fields);
    let outcome = report(EVENT_READY, &fields)
        .and_then(|()| emit_corpus(mode))
        .and_then(|summary| report(EVENT_UTF8_END, &trailer_fields(&summary)));

    // The one exit gate for every path past configure: the console code
    // pages are shared state, and a human's real console must get its own
    // settings back even when a write just failed.
    platform::restore(&session);
    if let Err(err) = outcome {
        eprintln!("utf8-child: write failed: {err}");
        std::process::exit(4);
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Mode, String> {
    const USAGE: &str = "usage: utf8-child <valid|invalid>";
    let mut mode: Option<Mode> = None;
    for arg in args.by_ref() {
        match arg.as_str() {
            UTF8_MODE_VALID if mode.is_none() => mode = Some(Mode::Valid),
            UTF8_MODE_INVALID if mode.is_none() => mode = Some(Mode::Invalid),
            other => return Err(format!("unexpected argument: {other}. {USAGE}")),
        }
    }
    mode.ok_or_else(|| format!("a mode is required. {USAGE}"))
}

/// Emit every corpus line for `mode`, hashing and counting exactly the
/// payload bytes handed to the OS — the trailer must state what was
/// written, not what was planned, so a partial emission can never present
/// itself as a complete one.
fn emit_corpus(mode: Mode) -> std::io::Result<CorpusSummary> {
    let mut summary = CorpusSummary {
        items: 0,
        bytes: 0,
        chars: 0,
        fnv: 0,
    };
    let mut hash = Fnv1a64::new();
    for line in corpus_lines(mode.include_junk()) {
        platform::write_all_raw(corpus_line_lead(line.seq).as_bytes())?;
        for slice in split_at_offsets(&line.payload, &line.splits) {
            // Each slice is one syscall; the boundaries between them are
            // the mid-sequence splits under test.
            platform::write_all_raw(slice)?;
            hash.update(slice);
            summary.bytes += slice.len();
        }
        // Explicit \r\n, like the report lines: a corpus line must end
        // where the fixture says it does, not where post-processing puts it.
        platform::write_all_raw(b"\r\n")?;
        summary.items += 1;
        summary.chars += line.chars;
    }
    summary.fnv = hash.finish();
    Ok(summary)
}

fn trailer_fields(summary: &CorpusSummary) -> TermFields {
    vec![
        ("items", summary.items.to_string()),
        ("bytes", summary.bytes.to_string()),
        ("chars", summary.chars.to_string()),
        ("fnv", format!("{:016x}", summary.fnv)),
    ]
}

/// One raw write per report line, through the same path as the corpus, so
/// reports and corpus slices can never reorder against each other. Failures
/// propagate rather than exiting here: the exit belongs to `main`, after
/// the console restore that every path past configure must go through.
fn report(event: &str, fields: &[(&str, String)]) -> std::io::Result<()> {
    let mut line = format_report(event, fields).into_bytes();
    line.extend_from_slice(b"\r\n");
    platform::write_all_raw(&line)
}

#[cfg(unix)]
mod platform {
    use super::TermFields;

    /// Nothing to restore on POSIX: the fixture types nothing, reads
    /// nothing, and leaves the terminal exactly as it found it.
    pub struct Session;

    pub fn configure() -> Result<(Session, TermFields), String> {
        Ok((Session, Vec::new()))
    }

    pub fn restore(_session: &Session) {}

    /// Unbuffered write straight to the stdout fd: each call is one
    /// `write(2)`, so a split the fixture places lands on the wire as a
    /// real boundary instead of disappearing into a userspace buffer.
    pub fn write_all_raw(bytes: &[u8]) -> std::io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            // SAFETY: the pointer/length pair describes `rest`, which
            // outlives the call; write only reads from it.
            let wrote =
                unsafe { libc::write(libc::STDOUT_FILENO, rest.as_ptr().cast(), rest.len()) };
            if wrote < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            let wrote = usize::try_from(wrote).unwrap_or(0);
            if wrote == 0 {
                // A zero-byte result for a non-empty buffer makes no
                // progress; looping on it would hang the fixture.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "write made no progress",
                ));
            }
            rest = &rest[wrote..];
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleOutputCP, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCP,
        SetConsoleOutputCP,
    };

    use super::TermFields;

    /// UTF-8 as a console code page. Spelled locally: pulling a whole
    /// windows-sys feature in for one constant buys nothing.
    const CP_UTF8: u32 = 65001;

    /// The code pages found at startup, restored at exit — the console
    /// object is shared state, and a human's real console must get its own
    /// settings back.
    pub struct Session {
        output_cp: u32,
        input_cp: u32,
    }

    pub fn configure() -> Result<(Session, TermFields), String> {
        // SAFETY: reads console state only.
        let output_before = unsafe { GetConsoleOutputCP() };
        // SAFETY: as above.
        let input_before = unsafe { GetConsoleCP() };
        if output_before == 0 || input_before == 0 {
            return Err(format!(
                "no console code page — spawn this fixture under a ConPTY ({})",
                std::io::Error::last_os_error()
            ));
        }

        // Without CP_UTF8 the console interprets the raw bytes in whatever
        // legacy code page the image inherited, and the corpus arrives
        // transcoded before ConPTY even gets a say.
        // SAFETY: plain value call.
        if unsafe { SetConsoleOutputCP(CP_UTF8) } == 0 {
            return Err(format!(
                "SetConsoleOutputCP(CP_UTF8) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: plain value call.
        if unsafe { SetConsoleCP(CP_UTF8) } == 0 {
            // Half-configured: put the output side back before failing.
            // SAFETY: restoring the value read above.
            unsafe { SetConsoleOutputCP(output_before) };
            return Err(format!(
                "SetConsoleCP(CP_UTF8) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let session = Session {
            output_cp: output_before,
            input_cp: input_before,
        };
        // Report what the console actually holds, not what was requested —
        // a silently unapplied code page would make every checksum
        // mismatch downstream a mystery instead of a typed setup failure.
        // SAFETY: read-only calls, as above.
        let (output_now, input_now) = unsafe { (GetConsoleOutputCP(), GetConsoleCP()) };
        if output_now != CP_UTF8 || input_now != CP_UTF8 {
            restore(&session);
            return Err(format!(
                "the console did not apply CP_UTF8 (output {output_before}->{output_now}, input {input_before}->{input_now})"
            ));
        }
        Ok((
            session,
            vec![
                ("output_cp", format!("{output_before}->{output_now}")),
                ("input_cp", format!("{input_before}->{input_now}")),
            ],
        ))
    }

    pub fn restore(session: &Session) {
        // Best effort, mirroring the sibling fixtures.
        // SAFETY: plain value calls restoring values read at configure time.
        unsafe {
            SetConsoleOutputCP(session.output_cp);
            SetConsoleCP(session.input_cp);
        }
    }

    /// Raw bytes to the stdout handle via `WriteFile`, not Rust's
    /// console-aware stdout: its UTF-16 path can neither hold a partial
    /// UTF-8 sequence across a flush nor carry an invalid byte at all.
    /// With the output code page forced to UTF-8 the console reads these
    /// bytes as UTF-8 — whether it then passes them to the PTY byte-exact
    /// is precisely what the spawning probe measures.
    pub fn write_all_raw(bytes: &[u8]) -> std::io::Result<()> {
        // SAFETY: GetStdHandle only looks up a slot in the PEB.
        let handle: HANDLE = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(std::io::Error::other("no stdout handle"));
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
            let mut written: u32 = 0;
            // SAFETY: the buffer pointer/len describe `rest`, which
            // outlives the call; `written` is a valid out-pointer; no
            // overlapped I/O on a console handle.
            if unsafe {
                WriteFile(
                    handle,
                    rest.as_ptr(),
                    len,
                    &mut written,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if written == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "WriteFile made no progress",
                ));
            }
            let advanced = usize::try_from(written)
                .unwrap_or(rest.len())
                .min(rest.len());
            rest = &rest[advanced..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_probe_child::corpus::corpus_summary;
    use agent_bridge_probe_child::format_report;

    #[test]
    fn args_select_the_mode() {
        assert!(matches!(
            parse_args(["valid".to_string()].into_iter()),
            Ok(Mode::Valid)
        ));
        assert!(matches!(
            parse_args(["invalid".to_string()].into_iter()),
            Ok(Mode::Invalid)
        ));
    }

    #[test]
    fn a_mode_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["valid".to_string(), "invalid".to_string()].into_iter()).is_err());
    }

    #[test]
    fn mode_names_match_the_cli_spelling() {
        // The ready report echoes these names and the probe asserts on
        // them; they must be the exact strings the CLI accepts.
        assert_eq!(Mode::Valid.name(), "valid");
        assert_eq!(Mode::Invalid.name(), "invalid");
        assert!(Mode::Invalid.include_junk());
        assert!(!Mode::Valid.include_junk());
    }

    #[test]
    fn the_trailer_shape_fits_an_80_column_terminal() {
        // The probe spawns this fixture wide, but a human runs it in a
        // real 80-column shell; a report line reaching the width would
        // hard-wrap mid-`key=value` and never parse. Both real summaries
        // are checked — their field widths, not invented ceilings, are
        // what the wire carries.
        for include_junk in [false, true] {
            let line = format_report(
                EVENT_UTF8_END,
                &trailer_fields(&corpus_summary(include_junk)),
            );
            assert!(
                line.len() <= 78,
                "trailer overflows the 80-column budget ({} chars): {line}",
                line.len()
            );
        }
    }

    #[test]
    fn the_trailer_spells_the_checksum_as_16_hex_digits() {
        // A leading-zero hash must not shrink the field: the probe compares
        // the fixed-width spelling.
        let fields = trailer_fields(&CorpusSummary {
            items: 1,
            bytes: 2,
            chars: 3,
            fnv: 0xab,
        });
        let fnv = fields.iter().find(|(key, _)| *key == "fnv").unwrap();
        assert_eq!(fnv.1, "00000000000000ab");
    }
}
