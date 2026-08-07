//! What a caller asks for: the program to host, the terminal to host it in,
//! and the two deadlines the layer enforces on its behalf.
//!
//! Everything here is plain data. The spec is read once at spawn and never
//! consulted again, so a session that wants different geometry resizes the
//! live handle rather than editing what it asked for.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::env::EnvStrip;

/// How long a single write may go unaccepted before the layer stops waiting
/// and hands the unwritten bytes back.
///
/// Five seconds is long enough that a CLI busy rendering a frame is not
/// mistaken for one that has stopped reading, and short enough that a caller
/// learns about a wedged child while the operator is still watching. A
/// blocked write is never silently abandoned — the suffix comes back with
/// the error, and what to do about it is the caller's decision.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Terminal geometry, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    /// Width in columns.
    pub cols: u16,
    /// Height in rows.
    pub rows: u16,
}

impl Dimensions {
    /// 80×24 — the historical xterm default, and what a CLI that cannot read
    /// its own window size assumes it has. A session that names no geometry
    /// gets this, and the fact that it did is logged at spawn: a CLI
    /// rendering into 80 columns when the caller's terminal is 200 wide
    /// looks like a wrapping bug until you know which of the two decided.
    pub const DEFAULT: Self = Self { cols: 80, rows: 24 };
}

impl Default for Dimensions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl std::fmt::Display for Dimensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

/// One child, and the terminal to host it in.
///
/// Build it with [`SpawnSpec::new`] and adjust the fields that matter;
/// everything except the program has a defensible default, and the defaults
/// are what an interactive CLI expects rather than what is cheapest to
/// implement.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// The program to execute. Resolved against `PATH` by the OS when it is
    /// a bare name, which is deliberate: a caller that wants an exact binary
    /// passes an absolute path, and one that wants "whatever the operator
    /// installed" passes a name.
    pub program: PathBuf,
    /// Arguments, not including the program itself.
    pub args: Vec<OsString>,
    /// Working directory for the child. `None` inherits this process's,
    /// which is the only answer available before a caller has one to give.
    pub cwd: Option<PathBuf>,
    /// Environment entries merged *over* the layer's defaults, so a caller
    /// that names `TERM` gets its own value. Names not mentioned here keep
    /// the default, and names in neither are inherited from this process.
    pub env: Vec<(OsString, OsString)>,
    /// Variables removed from the composed environment immediately before
    /// exec, whatever their source — inherited, defaulted, or caller-named.
    /// The mechanism lives here; which names it should reject is a policy
    /// decision that belongs above this layer.
    pub strip: EnvStrip,
    /// Terminal geometry. `None` takes [`Dimensions::DEFAULT`].
    pub dimensions: Option<Dimensions>,
    /// Per-write deadline; see [`DEFAULT_WRITE_TIMEOUT`].
    pub write_timeout: Duration,
}

impl SpawnSpec {
    /// A spec for `program` with no arguments, an inherited working
    /// directory and environment, default geometry, and the default write
    /// deadline.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            strip: EnvStrip::default(),
            dimensions: None,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
        }
    }

    /// The geometry this spec asks for, and whether it had to be defaulted —
    /// the caller of this function is the one that logs the substitution, so
    /// the two answers travel together.
    pub(crate) fn resolved_dimensions(&self) -> (Dimensions, bool) {
        match self.dimensions {
            Some(dimensions) => (dimensions, false),
            None => (Dimensions::DEFAULT, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_spec_defers_geometry_rather_than_choosing_it() {
        // The distinction is the point: a spec that names 80×24 asked for
        // it, and one that names nothing is told what it got.
        let spec = SpawnSpec::new("/bin/echo");
        assert_eq!(spec.resolved_dimensions(), (Dimensions::DEFAULT, true));

        let asked = SpawnSpec {
            dimensions: Some(Dimensions::DEFAULT),
            ..SpawnSpec::new("/bin/echo")
        };
        assert_eq!(asked.resolved_dimensions(), (Dimensions::DEFAULT, false));
    }

    #[test]
    fn dimensions_render_the_way_a_terminal_is_spoken_about() {
        assert_eq!(
            Dimensions {
                cols: 120,
                rows: 40
            }
            .to_string(),
            "120x40"
        );
    }
}
