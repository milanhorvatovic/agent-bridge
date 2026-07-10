//! The stand-in interactive fixture and its PR-tier lane.
//!
//! The fixture (`interactive-standin`, this package's second binary) is a
//! minimal interactive TUI: it clears the screen and paints a banner the
//! moment it starts, then waits on stdin and repaints or exits on command.
//! That is exactly the shape the live lane drives — paint, await input,
//! repaint, quit — minus the credentials and nondeterminism, which is what
//! makes it runnable on every PR on all three OSes.
//!
//! The banner embeds the `COLUMNS`/`LINES` values the child received, so the
//! lane's banner assertion doubles as proof that the composed child
//! environment actually arrived.
//!
//! The child reads lines, not raw keys: line input is what a PTY delivers
//! by default on every platform, and going raw would buy no extra coverage
//! while costing a terminal-mode dependency. "Interactive" here means
//! paint → await → repaint, not keystroke granularity.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtyPair};

use crate::firsttoken::FirstTokenClock;
use crate::pty::{OutputTracker, SharedWriter, alloc_pty, spawn_reader, teardown, wait_child};
use crate::rig::compose_child_env;
use crate::{COLS, Failure, ROWS, print_step};

pub const BANNER_MARKER: &str = "AGENT-BRIDGE-STANDIN";
pub const BYE_MARKER: &str = "AGENT-BRIDGE-STANDIN-BYE";
pub const CMD_REPAINT: &str = "repaint";
pub const CMD_QUIT: &str = "quit";

/// The banner the fixture paints and the lane asserts on. `cols`/`rows`
/// are strings because the child reports whatever its environment says —
/// including its absence — and the lane judges the value.
pub fn banner_line(paint: u32, cols: &str, rows: &str) -> String {
    format!("{BANNER_MARKER} paint={paint} cols={cols} rows={rows}")
}

/// The fixture child's whole life. Returns the process exit code.
pub fn child_main() -> i32 {
    let cols = std::env::var("COLUMNS").unwrap_or_else(|_| "unset".to_string());
    let rows = std::env::var("LINES").unwrap_or_else(|_| "unset".to_string());
    let mut paint = 1;
    paint_banner(paint, &cols, &rows);

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            return 1; // stdin read error: the terminal is gone
        };
        match line.trim() {
            CMD_REPAINT => {
                paint += 1;
                paint_banner(paint, &cols, &rows);
            }
            CMD_QUIT => {
                println!("{BYE_MARKER}");
                let _ = std::io::stdout().flush();
                return 0;
            }
            _ => {} // echo noise, empty lines: not a command
        }
    }
    0 // end-of-input: the master side closed — a clean end for a fixture
}

fn paint_banner(paint: u32, cols: &str, rows: &str) {
    // Clear screen + cursor home, banner, then a prompt line: the smallest
    // output that still looks like a repainting TUI to the reader.
    print!("\x1b[2J\x1b[1;1H{}\r\n> ", banner_line(paint, cols, rows));
    let _ = std::io::stdout().flush();
}

pub struct StandinLaneConfig {
    pub first_token_ms: u64,
    pub timeout: Duration,
}

impl Default for StandinLaneConfig {
    fn default() -> Self {
        Self {
            // The interactive-CLI first-token budget. A tiny Rust child
            // beats it by orders of magnitude on healthy runners; the
            // shared default keeps the lanes comparable.
            first_token_ms: 2_000,
            timeout: Duration::from_secs(10),
        }
    }
}

/// The PR-tier lane: spawn the fixture under a PTY with the same composed
/// child environment the live rig uses, assert the first paint arrives
/// within the first-token budget, drive a repaint, and shut down cleanly.
pub fn run_lane(config: &StandinLaneConfig) -> Result<(), Failure> {
    let (pair, alloc_ms) = alloc_pty(COLS, ROWS, config.timeout)
        .map_err(|detail| Failure::new("alloc", 20, detail))?;
    print_step(
        "alloc",
        "pass",
        &format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let standin = sibling_standin_path().map_err(|detail| Failure::new("spawn", 21, detail))?;
    let mut command = CommandBuilder::new(&standin);
    // The same composition the live rig uses: nothing inherited, an
    // allowlist carried, the terminal defaults forced. The banner will
    // prove COLUMNS/LINES arrived.
    command.env_clear();
    for (key, value) in compose_child_env(COLS, ROWS, std::env::vars()) {
        command.env(key, value);
    }
    let spawned_at = Instant::now();
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| Failure::new("spawn", 21, format!("child spawn failed: {err:#}")))?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    print_step(
        "spawn",
        "pass",
        &format!(
            "spawned `{}` pid={}",
            standin.display(),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
        ),
    );

    let reader = master.try_clone_reader().map_err(|err| {
        Failure::new(
            "first_paint",
            22,
            format!("cloning the reader failed: {err:#}"),
        )
    })?;
    let writer = SharedWriter::new(master.take_writer().map_err(|err| {
        Failure::new(
            "first_paint",
            22,
            format!("taking the writer failed: {err:#}"),
        )
    })?);
    let queries = Arc::new(AtomicU32::new(0));
    let events = spawn_reader(reader, writer.clone(), queries);
    let mut tracker = OutputTracker::new(events, FirstTokenClock::new(spawned_at), None);

    let banner1 = banner_line(1, &COLS.to_string(), &ROWS.to_string());
    tracker
        .wait_for_text(
            "first banner paint",
            |text| text.contains(&banner1),
            config.timeout,
        )
        .map_err(|detail| Failure::new("first_paint", 22, detail))?;
    // Unreachable panic: the wait above returned only after a chunk arrived.
    let latency = tracker.clock.launch_latency().unwrap();
    if latency.as_millis() as u64 > config.first_token_ms {
        return Err(Failure::new(
            "first_paint",
            22,
            format!(
                "first output byte took {}ms, over the {}ms budget",
                latency.as_millis(),
                config.first_token_ms
            ),
        ));
    }
    print_step(
        "first_paint",
        "pass",
        &format!(
            "first byte in {}ms (budget {}ms); banner shows the composed {COLS}x{ROWS} environment",
            latency.as_millis(),
            config.first_token_ms
        ),
    );

    writer
        .type_line(CMD_REPAINT, Duration::from_millis(50))
        .map(|_| ())
        .map_err(|err| {
            Failure::new(
                "repaint",
                23,
                format!("writing the repaint command failed: {err}"),
            )
        })?;
    let banner2 = banner_line(2, &COLS.to_string(), &ROWS.to_string());
    tracker
        .wait_for_text(
            "repaint banner",
            |text| text.contains(&banner2),
            config.timeout,
        )
        .map_err(|detail| Failure::new("repaint", 23, detail))?;
    print_step("repaint", "pass", "fixture repainted on input");

    writer
        .type_line(CMD_QUIT, Duration::from_millis(50))
        .map(|_| ())
        .map_err(|err| {
            Failure::new(
                "quit",
                24,
                format!("writing the quit command failed: {err}"),
            )
        })?;
    tracker
        .wait_for_text(
            "goodbye marker",
            |text| text.contains(BYE_MARKER),
            config.timeout,
        )
        .map_err(|detail| Failure::new("quit", 24, detail))?;
    print_step("quit", "pass", "goodbye marker observed");

    let exit_detail = wait_child(child.as_mut(), config.timeout)
        .map_err(|detail| Failure::new("child_exit", 25, detail))?;
    print_step("child_exit", "pass", &exit_detail);

    let (events, _, end) = tracker.into_teardown_parts();
    let teardown_detail = teardown(master, &events, end, config.timeout)
        .map_err(|detail| Failure::new("teardown", 26, detail))?;
    print_step("teardown", "pass", &teardown_detail);
    Ok(())
}

/// The fixture binary sits next to this one — cargo builds every bin target
/// of the package into the same directory.
fn sibling_standin_path() -> Result<std::path::PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    let standin = dir.join(format!(
        "interactive-standin{}",
        std::env::consts::EXE_SUFFIX
    ));
    if standin.exists() {
        Ok(standin)
    } else {
        Err(format!(
            "fixture binary not found at {} — build it first: \
             cargo build --package agent-bridge-interactive-probe --bins",
            standin.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_carries_paint_count_and_dimensions() {
        assert_eq!(
            banner_line(3, "80", "24"),
            "AGENT-BRIDGE-STANDIN paint=3 cols=80 rows=24"
        );
    }

    #[test]
    fn successive_paints_are_distinguishable() {
        // The repaint assertion relies on paint=1 never matching paint=2.
        assert!(!banner_line(2, "80", "24").contains(&banner_line(1, "80", "24")));
    }
}
