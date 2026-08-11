//! The reader against a real terminal, end to end.
//!
//! Every scenario allocates an actual pseudo-terminal, runs an actual child
//! in it, and drives [`agent_bridge_stream::StreamReader`] over the bridge a
//! session would use. Nothing is mocked: the decode policy's component
//! tests live beside the reader, but whether that policy holds against what
//! an operating system's terminal actually delivers can only be asked here.
//!
//! The child inside the terminal is this same binary re-invoked with a role
//! argument — the pattern the terminal crate's integration suite
//! established, and the reason this target runs its own `main` instead of
//! the test harness.

// This target owns its own stdout twice over: the scenario runner reports
// results on it, and the fixture half writes its bytes into the terminal
// through it. Both are the output, not a diagnostic.
#![allow(clippy::disallowed_macros)]

use std::io::Write;
use std::time::Duration;

use agent_bridge_pty::{Dimensions, SpawnSpec, spawn};
use agent_bridge_stream::{
    EncodingIncident, PtyChunkSource, ReaderConfig, ReaderEnd, ReaderOutputs, ReaderReport,
    StreamReader,
};
use tokio::sync::mpsc;

/// The line every fixture ends with, so the driver knows when to close the
/// terminal — which is what ends the stream portably: a pseudo-console
/// holds its output open until the handle drops, however dead the child is.
const DONE: &str = "fixture-done";

/// Characters of one, two, three, and four bytes — the corpus the UTF-8
/// probe established across all three OSes.
const CORPUS: &str = "héllo 🌍 — ascii, 2-byte é, 3-byte —, 4-byte 🌍";

/// How long a scenario waits before calling the run stuck. Generous next to
/// what the fixtures do — they finish in milliseconds — because a loaded
/// build machine is not a failing implementation.
const PATIENCE: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(role) = args.first() {
        fixture(role);
    }

    type Check = fn() -> Result<String, String>;
    let common: &[(&str, Check)] = &[
        ("continuous_stream_decodes_end_to_end", continuous_stream),
        ("split_multibyte_output_decodes_intact", split_multibyte),
    ];
    // ConPTY substitutes undecodable bytes with U+FFFD before they ever
    // reach the terminal's master side (measured by the UTF-8 probe), so
    // the runtime's own replacement path cannot be exercised against a real
    // terminal on Windows — the component tests beside the reader cover the
    // policy there, byte-identically on every OS.
    #[cfg(unix)]
    let posix: &[(&str, Check)] = &[("garbage_is_replaced_reported_and_survived", garbage_burst)];
    #[cfg(not(unix))]
    let posix: &[(&str, Check)] = &[];
    let scenarios = [common, posix].concat();

    let mut failed = 0;
    for (name, check) in &scenarios {
        let (status, detail) = match check() {
            Ok(detail) => ("pass", detail),
            Err(detail) => {
                failed += 1;
                ("fail", detail)
            }
        };
        println!(
            "reader-pty step={name} status={status} detail=\"{}\"",
            detail.replace(['"', '\n', '\r'], " ")
        );
    }
    println!("reader-pty scenarios={} failed={failed}", scenarios.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Run the named fixture role inside the terminal and exit. Never returns.
fn fixture(role: &str) -> ! {
    let mut out = std::io::stdout();
    match role {
        "continuous" => {
            // Line by line, each flushed: a steady stream of small writes,
            // which is what interactive output looks like.
            for line in 0..200 {
                println!("line-{line:03}");
            }
        }
        "dribble" => {
            // One byte at a time, each flushed: the parent cannot choose
            // where a read boundary falls, but a child that dribbles makes
            // the boundaries fall everywhere, including inside characters.
            for byte in CORPUS.as_bytes() {
                let _ = out.write_all(std::slice::from_ref(byte));
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(1));
            }
            println!();
        }
        "garbage" => {
            // Four undecodable runs inside one write — inside one second by
            // any measure — between valid neighbours that must survive.
            let _ = out.write_all(b"garbage-begin\nok \xFF a \xFF b \xFF c \xFF ok\n");
            let _ = out.flush();
        }
        other => {
            println!("error unknown-role={other}");
            std::process::exit(2);
        }
    }
    println!("{DONE}");
    let _ = out.flush();
    std::process::exit(0);
}

/// Everything one run produced, gathered for assertions.
struct Outcome {
    /// The decoded text feed, concatenated.
    text: String,
    /// The raw tee, concatenated.
    raw: Vec<u8>,
    /// Every incident, in order.
    incidents: Vec<EncodingIncident>,
    report: ReaderReport,
}

impl Outcome {
    /// The text as a person would read it, for marker assertions — a
    /// console brackets even trivial output with control sequences.
    fn visible(&self) -> String {
        strip_ansi(&self.text)
    }

    /// The never-silent equation, asserted after every run.
    fn check_accounts(&self) -> Result<(), String> {
        let stats = &self.report.stats;
        if stats.bytes_in != stats.text_bytes_out + stats.bytes_replaced {
            return Err(format!(
                "bytes went missing: {} in, {} out as text, {} replaced",
                stats.bytes_in, stats.text_bytes_out, stats.bytes_replaced
            ));
        }
        if stats.bytes_in != self.raw.len() as u64 {
            return Err(format!(
                "the raw tee saw {} bytes of {} read",
                self.raw.len(),
                stats.bytes_in
            ));
        }
        Ok(())
    }
}

/// Spawn this binary in the named role and read everything it says through
/// a real reader over a real terminal.
fn drive(role: &str) -> Result<Outcome, String> {
    let own = std::env::current_exe().map_err(|err| format!("cannot find this binary: {err}"))?;
    let mut spec = SpawnSpec::new(own);
    spec.args.push(role.into());
    // Wide, so the console does not reflow a report line mid-field.
    spec.dimensions = Some(Dimensions {
        cols: 200,
        rows: 50,
    });
    let spawned = spawn(&spec).map_err(|err| format!("spawn failed: {err}"))?;
    let source = PtyChunkSource::spawn(spawned.output, format!("reader-pty-{role}"))
        .map_err(|err| format!("the bridge thread failed to start: {err}"))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|err| format!("no runtime: {err}"))?;
    runtime.block_on(async move {
        let (text_tx, mut text_rx) = mpsc::channel(64);
        let (vt_tx, mut vt_rx) = mpsc::channel(64);
        let (incident_tx, mut incident_rx) = mpsc::channel(64);
        let reader = StreamReader::new(
            ReaderConfig::default(),
            ReaderOutputs {
                text: text_tx,
                vt: Some(vt_tx),
                incidents: incident_tx,
            },
        );
        let task = tokio::spawn(reader.run(source));

        let mut text = String::new();
        let mut raw: Vec<u8> = Vec::new();
        let mut incidents = Vec::new();
        // Held until the fixture says it is done: dropping the handle is
        // what ends the stream on a pseudo-console, and dropping it earlier
        // would cut the fixture off mid-say.
        let mut pty = Some(spawned.pty);
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let (mut text_open, mut vt_open, mut incidents_open) = (true, true, true);
        while text_open || vt_open || incidents_open {
            if pty.is_some() && strip_ansi(&text).contains(DONE) {
                drop(pty.take());
            }
            tokio::select! {
                chunk = text_rx.recv(), if text_open => match chunk {
                    Some(chunk) => text.push_str(&chunk),
                    None => text_open = false,
                },
                bytes = vt_rx.recv(), if vt_open => match bytes {
                    Some(bytes) => raw.extend(bytes),
                    None => vt_open = false,
                },
                incident = incident_rx.recv(), if incidents_open => match incident {
                    Some(incident) => incidents.push(incident),
                    None => incidents_open = false,
                },
                () = tokio::time::sleep_until(deadline) => {
                    return Err(format!("the stream never ended; text ends: …{}", tail(&text)));
                }
            }
        }
        let report = task
            .await
            .map_err(|err| format!("the reader panicked: {err}"))?;
        Ok(Outcome {
            text,
            raw,
            incidents,
            report,
        })
    })
}

fn continuous_stream() -> Result<String, String> {
    let outcome = drive("continuous")?;
    let visible = outcome.visible();
    for marker in ["line-000", "line-123", "line-199", DONE] {
        if !visible.contains(marker) {
            return Err(format!("`{marker}` never arrived"));
        }
    }
    if !outcome.incidents.is_empty() {
        return Err(format!(
            "clean output must raise no incidents, got {:?}",
            outcome.incidents
        ));
    }
    outcome.check_accounts()?;
    if !matches!(outcome.report.end, ReaderEnd::Stream(_)) {
        return Err(format!(
            "the run ended abnormally: {:?}",
            outcome.report.end
        ));
    }
    let stats = &outcome.report.stats;
    Ok(format!(
        "bytes_in={} chunks_out={} stalls={}",
        stats.bytes_in, stats.chunks_out, stats.stall_count
    ))
}

fn split_multibyte() -> Result<String, String> {
    let outcome = drive("dribble")?;
    let visible = outcome.visible();
    if !visible.contains(CORPUS) {
        return Err(format!(
            "the corpus did not survive byte-by-byte delivery; got: {}",
            visible.replace(['\r', '\n'], " ")
        ));
    }
    // The bar: splits are the terminal layer's to repair, and repairing
    // them must never masquerade as an error here.
    if !outcome.incidents.is_empty() {
        return Err(format!(
            "a split invented an incident: {:?}",
            outcome.incidents
        ));
    }
    if outcome.report.stats.bytes_replaced != 0 {
        return Err("a split was counted as a replacement".to_string());
    }
    outcome.check_accounts()?;
    Ok(format!("bytes_in={}", outcome.report.stats.bytes_in))
}

#[cfg(unix)]
fn garbage_burst() -> Result<String, String> {
    let outcome = drive("garbage")?;
    let visible = outcome.visible();
    if !visible.contains(DONE) {
        return Err("the session did not survive the garbage".to_string());
    }
    let replaced = outcome.text.matches('\u{FFFD}').count();
    if replaced != 4 {
        return Err(format!(
            "4 bytes went in undecodable, {replaced} came out replaced"
        ));
    }
    let replacements: Vec<_> = outcome
        .incidents
        .iter()
        .filter(|incident| matches!(incident, EncodingIncident::Replacement { .. }))
        .collect();
    let bursts: Vec<_> = outcome
        .incidents
        .iter()
        .filter_map(|incident| match incident {
            EncodingIncident::Burst { count, .. } => Some(*count),
            EncodingIncident::Replacement { .. } => None,
        })
        .collect();
    // Four runs within a second: two individual reports, the rest one
    // burst — never four events.
    if replacements.len() != 2 || bursts != vec![2] {
        return Err(format!(
            "expected 2 replacements and one burst of 2, got {:?}",
            outcome.incidents
        ));
    }
    if outcome.report.stats.bytes_replaced != 4 {
        return Err(format!(
            "bytes_replaced says {}, four bytes were garbage",
            outcome.report.stats.bytes_replaced
        ));
    }
    outcome.check_accounts()?;
    Ok(format!(
        "replacements={} burst_count=2 bytes_in={}",
        replacements.len(),
        outcome.report.stats.bytes_in
    ))
}

/// The last of the visible text, for a failure message.
fn tail(text: &str) -> String {
    let visible = strip_ansi(text).replace(['\r', '\n'], " ");
    let start = visible
        .char_indices()
        .rev()
        .nth(120)
        .map_or(0, |(at, _)| at);
    visible[start..].to_string()
}

/// Drop escape sequences so an assertion sees what a person would read.
/// The same reduction the terminal crate's suite uses, for the same reason:
/// a console brackets even trivial output with cursor and colour control.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // A control sequence runs to its final byte.
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            }
            // An operating-system command runs to a bell or a string
            // terminator.
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' && chars.clone().next() == Some('\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Anything else is a two-character escape, both consumed.
            _ => {}
        }
    }
    out
}
