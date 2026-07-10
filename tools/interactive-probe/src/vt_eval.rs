//! The virtual-terminal library evaluation: replay a captured byte stream
//! into both candidate headless grids and report what the choice actually
//! turns on — grid API ergonomics, resize behavior, damage/diff access, and
//! dependency weight.
//!
//! Feature-gated (`vt-eval`) and run once: keeping it off the default build
//! means neither candidate becomes a standing dependency before the
//! decision is made. The output is evidence for a written decision, not the
//! decision itself; a human reads the numbers and writes the paragraph.
//!
//! The two candidates differ in shape, which the code below shows more
//! plainly than prose can:
//!
//! - `alacritty_terminal` drives a `vte::ansi::Processor` over a `Term`
//!   that implements the ANSI `Handler` trait; the caller supplies a
//!   `Dimensions` impl and an `EventListener`, and reads damage as
//!   per-line `left..=right` column spans.
//! - `avt` takes `Vt::feed_str` and hands back a `Changes` value listing
//!   the changed line indices; the grid is read as `Vec<String>`.

use std::path::Path;
use std::time::Instant;

use crate::capture::read_capture;

/// What one candidate reported after eating the whole capture.
#[derive(Debug)]
pub struct VtRun {
    pub library: &'static str,
    /// Wall-clock to feed every chunk, without the recorded pacing —
    /// throughput, not latency.
    pub feed_micros: u128,
    /// Total damaged-line events observed across the replay. A library that
    /// reports damage per line lets the runtime send diffs instead of full
    /// screens; the count shows how granular that signal is.
    pub damage_events: usize,
    /// Whether damage is reported at all after each feed, or only as
    /// "everything changed".
    pub damage_granularity: &'static str,
    /// Non-blank lines in the final 80×24 viewport — a sanity check that
    /// both emulators actually rendered the same session.
    pub non_blank_lines: usize,
    /// The viewport after a resize to 120×40, to compare reflow behavior.
    pub non_blank_lines_after_resize: usize,
    pub resize_behavior: String,
}

pub fn run(capture_path: &Path, cols: u16, rows: u16) -> Result<(), crate::Failure> {
    let chunks = read_capture(capture_path).map_err(|err| {
        crate::Failure::new("vt_eval", 60, format!("reading the capture failed: {err}"))
    })?;
    let bytes: usize = chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    crate::print_step(
        "vt_eval_capture",
        "pass",
        &format!(
            "{} — {} chunks, {bytes} bytes, {:.1}s of recorded session",
            capture_path.display(),
            chunks.len(),
            chunks.last().map_or(0.0, |c| c.t_ns as f64 / 1e9),
        ),
    );

    let all: Vec<&[u8]> = chunks.iter().map(|chunk| chunk.bytes.as_slice()).collect();
    // `alacritty_terminal` eats raw bytes and cannot refuse the stream, so it
    // is reported first and unconditionally; only `avt` can abort, and when
    // it does the run is a failure rather than a shorter set of numbers.
    report_run(&eval_alacritty(&all, cols, rows));
    let avt = eval_avt(&all, cols, rows)
        .map_err(|detail| crate::Failure::new("vt_eval_avt", 61, detail))?;
    report_run(&avt);

    crate::print_step(
        "vt_eval",
        "pass",
        "both candidates replayed the capture; dependency weight is `cargo tree -p agent-bridge-interactive-probe --features vt-eval`, and the decision paragraph is written by hand from these numbers",
    );
    Ok(())
}

fn report_run(run: &VtRun) {
    crate::print_step(
        &format!("vt_eval_{}", run.library),
        "pass",
        &format!(
            "feed={}us damage_events={} damage={} viewport_non_blank={} after_resize={} resize={}",
            run.feed_micros,
            run.damage_events,
            run.damage_granularity,
            run.non_blank_lines,
            run.non_blank_lines_after_resize,
            run.resize_behavior,
        ),
    );
}

fn eval_alacritty(chunks: &[&[u8]], cols: u16, rows: u16) -> VtRun {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term, TermDamage};
    use alacritty_terminal::vte::ansi::Processor;

    // The library asks the embedder for its own size type; there is no
    // built-in one outside its test module.
    struct Size {
        columns: usize,
        screen_lines: usize,
    }
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.screen_lines
        }
        fn screen_lines(&self) -> usize {
            self.screen_lines
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    let size = Size {
        columns: cols as usize,
        screen_lines: rows as usize,
    };
    let mut term = Term::new(Config::default(), &size, VoidListener);
    let mut parser: Processor = Processor::new();

    let started = Instant::now();
    let mut damage_events = 0usize;
    for chunk in chunks {
        parser.advance(&mut term, chunk);
        // Damage accumulates until reset; sampling per chunk is what a
        // runtime pushing incremental screen diffs would do.
        match term.damage() {
            TermDamage::Full => damage_events += rows as usize,
            TermDamage::Partial(lines) => damage_events += lines.count(),
        }
        term.reset_damage();
    }
    let feed_micros = started.elapsed().as_micros();

    let non_blank = |term: &Term<VoidListener>| {
        let grid = term.grid();
        (0..grid.screen_lines())
            .filter(|line| {
                let row = &grid[Line(*line as i32)];
                (0..grid.columns()).any(|col| row[Column(col)].c != ' ')
            })
            .count()
    };
    let before = non_blank(&term);
    term.resize(Size {
        columns: 120,
        screen_lines: 40,
    });
    let after = non_blank(&term);

    VtRun {
        library: "alacritty_terminal",
        feed_micros,
        damage_events,
        damage_granularity: "per-line with left..=right column bounds (TermDamage::Partial)",
        non_blank_lines: before,
        non_blank_lines_after_resize: after,
        resize_behavior: format!(
            "Term::resize(Dimensions) in place; scrollback retained, grid reflowed to 120x40 ({after} non-blank lines)"
        ),
    }
}

/// `avt` consumes `&str`, not bytes: a UTF-8 boundary can split a chunk, so
/// the replay must reassemble codepoints the same way the PTY reader does.
/// That is itself an ergonomics finding, and it has a sharper edge — a byte
/// that can never be valid UTF-8 cannot be fed to `avt` at all, where
/// `alacritty_terminal` would consume it. Such a capture is therefore an
/// error, not a shorter replay: half a stream produces smaller damage and
/// feed numbers that would sit next to `alacritty_terminal`'s complete ones
/// and read as a comparison.
fn eval_avt(chunks: &[&[u8]], cols: u16, rows: u16) -> Result<VtRun, String> {
    let mut reassembler = crate::utf8::Reassembler::new();
    let mut vt = avt::Vt::builder()
        .size(cols as usize, rows as usize)
        .build();

    let started = Instant::now();
    let mut damage_events = 0usize;
    let mut fed = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        if reassembler.push(chunk).is_err() {
            return Err(format!(
                "the capture holds bytes that can never be valid UTF-8, at chunk {} of {} ({fed} bytes replayed). `avt` takes `&str`, so it cannot replay this stream at all — a finding in its own right, and one that leaves the two libraries with nothing comparable on this capture",
                index + 1,
                chunks.len(),
            ));
        }
        let text = reassembler.decoded();
        if text.len() > fed {
            let changes = vt.feed_str(&text[fed..]);
            damage_events += changes.lines.len();
            fed = text.len();
        }
    }
    let feed_micros = started.elapsed().as_micros();

    let non_blank = |vt: &avt::Vt| {
        vt.text()
            .iter()
            .filter(|line| !line.trim().is_empty())
            .count()
    };
    let before = non_blank(&vt);
    let resize_changes = vt.resize(120, 40).lines.len();
    let after = non_blank(&vt);

    Ok(VtRun {
        library: "avt",
        feed_micros,
        damage_events,
        damage_granularity: "changed line indices per feed (Changes.lines), no column bounds",
        non_blank_lines: before,
        non_blank_lines_after_resize: after,
        resize_behavior: format!(
            "Vt::resize returns the {resize_changes} changed lines; feed_str takes &str, so the caller owns UTF-8 reassembly"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_libraries_replay_a_valid_stream() {
        let chunks: &[&[u8]] = &[b"\x1b[2J\x1b[1;1Hhello ", "wörld 🌍".as_bytes()];
        assert_eq!(eval_alacritty(chunks, 80, 24).library, "alacritty_terminal");
        let avt = eval_avt(chunks, 80, 24).expect("a valid stream must replay");
        assert_eq!(avt.non_blank_lines, 1);
    }

    #[test]
    fn a_utf8_split_across_chunks_is_reassembled_not_rejected() {
        // The PTY splits codepoints at arbitrary byte boundaries; that is
        // normal, and must not be mistaken for an undecodable stream.
        let full = "héllo".as_bytes();
        let chunks: &[&[u8]] = &[&full[..2], &full[2..]];
        assert!(eval_avt(chunks, 80, 24).is_ok());
    }

    #[test]
    fn undecodable_bytes_abort_the_avt_replay_instead_of_shortening_it() {
        // 0xFF can never appear in UTF-8. A partial replay would report
        // smaller feed and damage numbers than alacritty_terminal's complete
        // one, and the two would be printed side by side as a comparison.
        let chunks: &[&[u8]] = &[b"fine so far", &[0xFF, 0xFE], b"never reached"];
        let err = eval_avt(chunks, 80, 24).expect_err("undecodable bytes must abort the replay");
        assert!(
            err.contains("chunk 2 of 3"),
            "must say how far it got: {err}"
        );
        assert!(
            err.contains("nothing comparable"),
            "must say why the numbers cannot be compared: {err}"
        );
    }
}
