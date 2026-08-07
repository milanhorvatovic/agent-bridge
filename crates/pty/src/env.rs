//! The environment a hosted child is given, composed from four sources in
//! a fixed order.
//!
//! Interactive CLIs read their environment to decide how much of a terminal
//! they believe they are talking to, and the wrong answer is not a crash but
//! a quiet downgrade: no colour, no cursor addressing, sometimes a
//! non-interactive code path altogether. So the defaults below are set on
//! every spawn rather than left to whatever the operator's shell happened to
//! export — and a caller that knows better still wins, because the layer is
//! supplying a floor, not a policy.
//!
//! Composition is pure. It takes the inherited environment as an argument
//! rather than reading the process's own, and takes what the platform adds
//! rather than deciding it, so the whole table is unit-testable without a
//! terminal, a child, or a particular machine's shell — and so that this
//! file has no idea which operating system it is running on.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use crate::spec::Dimensions;

/// Which environment variables to remove immediately before exec.
///
/// This is the mechanism only. It exists at this layer because stripping has
/// to happen after composition and before exec, and there is no later place
/// to put it; *which* names deserve stripping is a security and correctness
/// policy that belongs above a byte pipe. A caller that supplies nothing
/// gets [`EnvStrip::default`], which removes nothing.
///
/// The predicate sees a name, never a value: a rule that inspected values
/// would be reading secrets in order to decide whether to drop them.
#[derive(Clone)]
pub struct EnvStrip(Arc<dyn Fn(&OsStr) -> bool + Send + Sync>);

impl EnvStrip {
    /// Strip every name the predicate accepts.
    pub fn new(predicate: impl Fn(&OsStr) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(predicate))
    }

    fn strips(&self, name: &OsStr) -> bool {
        (self.0)(name)
    }
}

impl Default for EnvStrip {
    fn default() -> Self {
        Self::new(|_| false)
    }
}

impl std::fmt::Debug for EnvStrip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A predicate has nothing printable about it, and a spec carrying
        // one still has to be debuggable.
        f.write_str("EnvStrip(<predicate>)")
    }
}

/// `TERM` tells a CLI which escape sequences it may emit. Absent, most
/// assume a terminal that cannot do anything, and the ones that guess assume
/// less than this.
const TERM: (&str, &str) = ("TERM", "xterm-256color");

/// A CLI that checks only `TERM` sees 256 colours; one that checks this too
/// gets 24-bit. Neither is required for correctness, and a session that
/// renders in the wrong palette looks broken to the person reading it.
const COLORTERM: (&str, &str) = ("COLORTERM", "truecolor");

/// The environment the child is exec'd with.
///
/// Later sources win over earlier ones: what this process inherited, then
/// this layer's terminal defaults, then what the caller named. The strip
/// predicate runs last and over everything, so a name it rejects cannot
/// reach the child by any of the three routes.
///
/// The result is ordered by name because a deterministic environment makes a
/// spawn reproducible and a test assertion readable; the OS itself does not
/// care.
pub(crate) fn compose(
    inherited: impl Iterator<Item = (OsString, OsString)>,
    platform_defaults: &[(OsString, OsString)],
    caller: &[(OsString, OsString)],
    dimensions: Dimensions,
    strip: &EnvStrip,
) -> Vec<(OsString, OsString)> {
    // Keyed by the *comparison* form of the name, valued by the name as it
    // was actually spelled plus its value. On Windows the two differ: the OS
    // matches environment names case-insensitively, so an inherited `Path`
    // and a caller's `PATH` are one variable and must not both survive —
    // while the spelling the child sees stays the one whoever won the merge
    // wrote.
    let mut composed: BTreeMap<OsString, (OsString, OsString)> = BTreeMap::new();
    let mut put = |name: OsString, value: OsString| {
        composed.insert(comparison_key(&name), (name, value));
    };

    for (name, value) in inherited {
        put(name, value);
    }
    for (name, value) in terminal_defaults(dimensions) {
        put(name, value);
    }
    // What the platform adds on top — a locale pair where the platform has
    // a locale convention, nothing where it does not. Supplied by the
    // caller rather than decided here, so this stays one composition rule
    // instead of one per operating system.
    for (name, value) in platform_defaults {
        put(name.clone(), value.clone());
    }
    for (name, value) in caller {
        put(name.clone(), value.clone());
    }

    composed
        .into_iter()
        // Tested against both spellings — the one the winning source wrote,
        // and the one the platform compares by. On Windows those differ, and
        // a rule naming `PATH` has to reject an inherited `Path`: the
        // operating system treats them as one variable, so a strip that
        // matched only the literal spelling would let a rejected name
        // through by luck of capitalisation.
        .filter(|(key, (name, _))| !strip.strips(name) && !strip.strips(key))
        .map(|(_, entry)| entry)
        .collect()
}

/// What this layer sets on every spawn unless the caller says otherwise.
fn terminal_defaults(dimensions: Dimensions) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from(TERM.0), OsString::from(TERM.1)),
        (OsString::from(COLORTERM.0), OsString::from(COLORTERM.1)),
        // Some CLIs read the geometry from here instead of asking the
        // kernel. Set once, at spawn: a later resize notifies the child
        // through the terminal, and an environment cannot be rewritten
        // under a running process anyway.
        (
            OsString::from("COLUMNS"),
            OsString::from(dimensions.cols.to_string()),
        ),
        (
            OsString::from("LINES"),
            OsString::from(dimensions.rows.to_string()),
        ),
    ]
}

/// The form two environment names are compared in on this platform.
fn comparison_key(name: &OsStr) -> OsString {
    if cfg!(windows) {
        // Lossy is safe for the comparison key alone: an unpaired surrogate
        // folds to the replacement character, which at worst makes two
        // already-unnameable variables collide. The spelling handed to the
        // child is the original, never this.
        OsString::from(name.to_string_lossy().to_uppercase())
    } else {
        name.to_os_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    fn lookup<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(key, _)| key == OsStr::new(name))
            .map(|(_, value)| value.as_os_str())
    }

    /// A stand-in for whatever the platform adds — the shape matters here,
    /// not which operating system is running the test.
    fn platform_defaults() -> Vec<(OsString, OsString)> {
        vec![entry("LC_ALL", "C.UTF-8")]
    }

    fn compose_for_test(
        inherited: &[(OsString, OsString)],
        caller: &[(OsString, OsString)],
        strip: &EnvStrip,
    ) -> Vec<(OsString, OsString)> {
        compose(
            inherited.iter().cloned(),
            &platform_defaults(),
            caller,
            Dimensions {
                cols: 120,
                rows: 40,
            },
            strip,
        )
    }

    #[test]
    fn env_defaults_present_unless_overridden() {
        // The whole contract in one pass: the defaults are set, the caller
        // outranks them, geometry reaches the two variables that carry it,
        // and an inherited value neither survives a default nor blocks one.
        let inherited = [entry("TERM", "dumb"), entry("HOME", "/home/tester")];
        let caller = [entry("COLORTERM", "16"), entry("MY_FLAG", "on")];
        let env = compose_for_test(&inherited, &caller, &EnvStrip::default());

        assert_eq!(lookup(&env, "TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(lookup(&env, "COLORTERM"), Some(OsStr::new("16")));
        assert_eq!(lookup(&env, "COLUMNS"), Some(OsStr::new("120")));
        assert_eq!(lookup(&env, "LINES"), Some(OsStr::new("40")));
        assert_eq!(lookup(&env, "MY_FLAG"), Some(OsStr::new("on")));
        assert_eq!(
            lookup(&env, "HOME"),
            Some(OsStr::new("/home/tester")),
            "an inherited variable this layer has no opinion about must survive"
        );

        assert_eq!(
            lookup(&env, "LC_ALL"),
            Some(OsStr::new("C.UTF-8")),
            "what the platform adds is set alongside the terminal defaults"
        );
    }

    #[test]
    fn a_caller_outranks_the_platforms_defaults_too() {
        // The platform's additions are defaults like any other, not a
        // second policy that outlives what the caller asked for.
        let env = compose_for_test(&[], &[entry("LC_ALL", "en_GB.UTF-8")], &EnvStrip::default());
        assert_eq!(lookup(&env, "LC_ALL"), Some(OsStr::new("en_GB.UTF-8")));
    }

    #[test]
    fn the_strip_hook_outranks_every_source_including_the_caller() {
        // Stripping is the last word by construction: a variable planted in
        // all three sources still must not reach the child, or the hook
        // would be a suggestion rather than a boundary.
        let strip = EnvStrip::new(|name| name == OsStr::new("LD_PRELOAD"));
        let env = compose_for_test(
            &[entry("LD_PRELOAD", "/tmp/inherited.so")],
            &[entry("LD_PRELOAD", "/tmp/caller.so")],
            &strip,
        );
        assert_eq!(lookup(&env, "LD_PRELOAD"), None);
        assert!(
            lookup(&env, "TERM").is_some(),
            "stripping one name must not disturb the rest"
        );
    }

    #[test]
    fn a_strip_rule_catches_the_spelling_the_platform_would_match() {
        // A rule naming the conventional spelling, against a variable
        // inherited under a different one. Windows considers them the same
        // variable, so the rule must reject it there; POSIX considers them
        // two, so it must not.
        let strip = EnvStrip::new(|name| name == OsStr::new("PATH"));
        let env = compose_for_test(&[entry("Path", "/inherited")], &[], &strip);
        if cfg!(windows) {
            assert_eq!(
                lookup(&env, "Path"),
                None,
                "one variable, and it was rejected"
            );
        } else {
            assert_eq!(lookup(&env, "Path"), Some(OsStr::new("/inherited")));
        }
    }

    #[test]
    fn the_default_hook_strips_nothing() {
        let env = compose_for_test(&[entry("ANYTHING", "kept")], &[], &EnvStrip::default());
        assert_eq!(lookup(&env, "ANYTHING"), Some(OsStr::new("kept")));
    }

    #[test]
    fn a_name_appears_once_however_the_platform_spells_it() {
        // Windows matches environment names case-insensitively, so a caller
        // writing `Path` is overriding the inherited `PATH` rather than
        // adding a second variable; POSIX has no such rule and both are real.
        let env = compose_for_test(
            &[entry("PATH", "/inherited")],
            &[entry("Path", "/caller")],
            &EnvStrip::default(),
        );
        if cfg!(windows) {
            assert_eq!(lookup(&env, "Path"), Some(OsStr::new("/caller")));
            assert_eq!(lookup(&env, "PATH"), None, "one variable, one entry");
        } else {
            assert_eq!(lookup(&env, "PATH"), Some(OsStr::new("/inherited")));
            assert_eq!(lookup(&env, "Path"), Some(OsStr::new("/caller")));
        }
    }

    #[test]
    fn composition_is_ordered_so_a_spawn_reproduces() {
        let env = compose_for_test(
            &[entry("ZZZ", "z"), entry("AAA", "a")],
            &[],
            &EnvStrip::default(),
        );
        let names: Vec<_> = env.iter().map(|(name, _)| name.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
