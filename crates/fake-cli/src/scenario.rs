//! Scenario schema and validation.
//!
//! A scenario is a JSON object with a `name` and a list of `steps`; each step
//! is discriminated by which key it carries — `emit`, `generate`,
//! `await_stdin`, or `exit`:
//!
//! ```json
//! {
//!   "name": "approval-then-token",
//!   "steps": [
//!     { "emit": "Allow filesystem write? [y/N]\n", "channel": "stdout" },
//!     { "await_stdin": "y\n", "timeout_ms": 1000 },
//!     { "emit": "Writing file...\n", "channel": "stdout" },
//!     { "exit": 0 }
//!   ]
//! }
//! ```
//!
//! Parsed by hand from the JSON tree rather than via derive: the step kinds
//! are discriminated by key presence, and a derived untagged enum reports
//! every failure as "data did not match any variant" — useless to a scenario
//! author. Hand parsing lets every rejection name the step index and the
//! offending key, which the unit tests below hold it to.
//!
//! Validation is strict on purpose. Scenarios are committed conformance
//! fixtures shared across every OS lane; a typo that a lenient parser skips
//! over silently changes what a scenario asserts.

use serde_json::{Map, Value};

use crate::generator::DEFAULT_LINE_BYTES;

#[derive(Debug)]
pub struct Scenario {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Write `text`'s UTF-8 bytes to `channel`. With `byte_delay_ms > 0` the
    /// bytes go out one write per byte with that many milliseconds between
    /// successive bytes — pacing for streaming realism that never changes
    /// the bytes themselves.
    ///
    /// The one substitution: every `{ts}` in `text` is replaced, as the step
    /// starts writing, with a reading of the system monotonic clock. It is
    /// the sole scripted content that differs between runs, and it exists so
    /// a reader on the far side of the terminal can measure delivery latency
    /// against its own reading of the same clock.
    ///
    /// `repeat` writes the same text that many times, `repeat_interval_us`
    /// apart on an absolute schedule. A repeated `{ts}` is re-read each
    /// time — a stream of markers spaced far enough apart that each one
    /// measures a delivery rather than a queue is the reason repetition
    /// exists at all.
    Emit {
        text: String,
        channel: Channel,
        byte_delay_ms: u64,
        repeat: u64,
        repeat_interval_us: u64,
    },
    /// Emit `lines` generated payload lines, with a checksum line every
    /// `checksum_every` of them (`0` disables checksum lines), pacing each
    /// line onto a schedule `line_interval_us` apart (`0` emits as fast as
    /// the terminal accepts).
    ///
    /// The content is derived from the line number rather than carried in
    /// the scenario, which is what makes half an hour of continuous
    /// streaming expressible as one step. See [`crate::generator`] for the
    /// line shapes and the digest a reader checks them against.
    Generate {
        lines: u64,
        line_bytes: usize,
        checksum_every: u64,
        line_interval_us: u64,
        channel: Channel,
    },
    /// Block until exactly `expected` arrives on stdin, or fail the run:
    /// diverging input and closed stdin are a mismatch, `timeout_ms`
    /// elapsing is a timeout — each a non-zero exit with a diagnostic.
    /// Line terminators are matched as an equivalence class (`\n` ≡ `\r` ≡
    /// `\r\n`): what Enter delivers to a PTY-hosted child is the platform's
    /// choice — POSIX cooked mode rewrites CR to NL, ConPTY forwards the CR
    /// — so byte-exact terminators would make the scenario POSIX-only.
    /// Every other byte is exact.
    AwaitStdin { expected: String, timeout_ms: u64 },
    /// Flush the scripted output and exit the process with `code`.
    Exit { code: i32 },
}

/// The channels a step can script. Only stdout exists today; "stderr" is
/// rejected as reserved so a future scenario can claim it without any
/// already-committed scenario having quietly meant something else by it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Channel {
    Stdout,
}

const STEP_KINDS: [&str; 4] = ["emit", "generate", "await_stdin", "exit"];

/// The longest generated payload a scenario may ask for. A terminal that
/// reflows its output — ConPTY does — hard-wraps a line that exceeds the
/// terminal width, and a wrapped payload cannot be checked against the line
/// it was generated from. The cap is not the width (a scenario cannot know
/// it); it is the point past which no plausible probe terminal would hold a
/// line intact, so the rejection lands at authoring time rather than as an
/// unexplainable corruption report half an hour into a run.
const MAX_LINE_BYTES: u64 = 1024;

pub fn parse(text: &str) -> Result<Scenario, String> {
    let root: Value = serde_json::from_str(text).map_err(|err| format!("invalid JSON: {err}"))?;
    let Value::Object(root) = root else {
        return Err("the scenario must be a JSON object".into());
    };
    for key in root.keys() {
        if !matches!(key.as_str(), "name" | "steps") {
            return Err(format!(
                "unknown top-level field \"{key}\" — a scenario has \"name\" and \"steps\""
            ));
        }
    }
    let name = match root.get("name") {
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        Some(_) => return Err("\"name\" must be a non-empty string".into()),
        None => return Err("missing \"name\"".into()),
    };
    let steps = match root.get("steps") {
        Some(Value::Array(steps)) if !steps.is_empty() => steps
            .iter()
            .enumerate()
            .map(|(index, step)| parse_step(index, step))
            .collect::<Result<Vec<_>, _>>()?,
        Some(Value::Array(_)) => return Err("\"steps\" must not be empty".into()),
        Some(_) => return Err("\"steps\" must be an array".into()),
        None => return Err("missing \"steps\"".into()),
    };
    // The exit code is part of the script, never implicit: the run ends at an
    // explicit `exit` step and every step before it must be reachable.
    for (index, step) in steps.iter().enumerate() {
        let is_last = index + 1 == steps.len();
        match step {
            Step::Exit { .. } if !is_last => {
                return Err(format!(
                    "step {index}: \"exit\" before the final step leaves the steps after it unreachable"
                ));
            }
            Step::Exit { .. } => {}
            _ if is_last => {
                return Err(format!(
                    "step {index}: the final step must be \"exit\" — the exit code is scripted, never implicit"
                ));
            }
            _ => {}
        }
    }
    Ok(Scenario { name, steps })
}

fn parse_step(index: usize, step: &Value) -> Result<Step, String> {
    let Value::Object(fields) = step else {
        return Err(format!("step {index}: must be a JSON object"));
    };
    let found: Vec<&str> = STEP_KINDS
        .into_iter()
        .filter(|kind| fields.contains_key(*kind))
        .collect();
    match found.as_slice() {
        ["emit"] => parse_emit(index, fields),
        ["generate"] => parse_generate(index, fields),
        ["await_stdin"] => parse_await_stdin(index, fields),
        ["exit"] => parse_exit(index, fields),
        [] => Err(format!(
            "step {index}: unknown step kind — found {}; a step carries exactly one of {}",
            name_fields(fields.keys()),
            name_kinds(),
        )),
        many => Err(format!(
            "step {index}: ambiguous step — carries {}; a step carries exactly one of {}",
            name_fields(many.iter()),
            name_kinds(),
        )),
    }
}

fn parse_emit(index: usize, fields: &Map<String, Value>) -> Result<Step, String> {
    reject_unknown_fields(
        index,
        "emit",
        fields,
        &[
            "emit",
            "channel",
            "byte_delay_ms",
            "repeat",
            "repeat_interval_us",
        ],
    )?;
    let text = match fields.get("emit") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => {
            return Err(format!(
                "step {index} (emit): \"emit\" must be a non-empty string"
            ));
        }
    };
    let channel = parse_channel(index, "emit", fields)?;
    let byte_delay_ms = match fields.get("byte_delay_ms") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| {
            format!(
                "step {index} (emit): \"byte_delay_ms\" must be a non-negative integer (milliseconds between successive bytes)"
            )
        })?,
    };
    let repeat = optional_u64(index, "emit", fields, "repeat", 1)?;
    if repeat == 0 {
        return Err(format!(
            "step {index} (emit): \"repeat\" is 0 — a step that writes nothing should not be in the script"
        ));
    }
    Ok(Step::Emit {
        text,
        channel,
        byte_delay_ms,
        repeat,
        repeat_interval_us: optional_u64(index, "emit", fields, "repeat_interval_us", 0)?,
    })
}

fn parse_generate(index: usize, fields: &Map<String, Value>) -> Result<Step, String> {
    reject_unknown_fields(
        index,
        "generate",
        fields,
        &[
            "generate",
            "channel",
            "line_bytes",
            "checksum_every",
            "line_interval_us",
        ],
    )?;
    let lines = fields
        .get("generate")
        .and_then(Value::as_u64)
        .filter(|lines| *lines > 0)
        .ok_or_else(|| {
            format!("step {index} (generate): \"generate\" must be a positive line count")
        })?;
    let channel = parse_channel(index, "generate", fields)?;
    let line_bytes = optional_u64(
        index,
        "generate",
        fields,
        "line_bytes",
        DEFAULT_LINE_BYTES as u64,
    )?;
    if line_bytes == 0 {
        return Err(format!(
            "step {index} (generate): \"line_bytes\" is 0 — an empty payload renders as a \
             line ending in a bare space, which a terminal is entitled to trim away, and a \
             line that cannot survive the terminal cannot be verified behind one"
        ));
    }
    if line_bytes > MAX_LINE_BYTES {
        return Err(format!(
            "step {index} (generate): \"line_bytes\" is {line_bytes}, over the {MAX_LINE_BYTES} cap — \
             a terminal that reflows would wrap a line that long and no reader could check it"
        ));
    }
    Ok(Step::Generate {
        lines,
        line_bytes: line_bytes as usize,
        checksum_every: optional_u64(index, "generate", fields, "checksum_every", 0)?,
        line_interval_us: optional_u64(index, "generate", fields, "line_interval_us", 0)?,
        channel,
    })
}

fn parse_await_stdin(index: usize, fields: &Map<String, Value>) -> Result<Step, String> {
    reject_unknown_fields(index, "await_stdin", fields, &["await_stdin", "timeout_ms"])?;
    let expected = match fields.get("await_stdin") {
        Some(Value::String(expected)) if !expected.is_empty() => expected.clone(),
        _ => {
            return Err(format!(
                "step {index} (await_stdin): \"await_stdin\" must be a non-empty string"
            ));
        }
    };
    let timeout_ms = match fields.get("timeout_ms") {
        Some(value) => value.as_u64().filter(|ms| *ms > 0).ok_or_else(|| {
            format!("step {index} (await_stdin): \"timeout_ms\" must be a positive integer")
        })?,
        None => {
            return Err(format!(
                "step {index} (await_stdin): missing \"timeout_ms\" — an unbounded wait could hang a CI lane forever"
            ));
        }
    };
    Ok(Step::AwaitStdin {
        expected,
        timeout_ms,
    })
}

fn parse_exit(index: usize, fields: &Map<String, Value>) -> Result<Step, String> {
    reject_unknown_fields(index, "exit", fields, &["exit"])?;
    let code = fields
        .get("exit")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .ok_or_else(|| format!("step {index} (exit): \"exit\" must be an integer exit code"))?;
    Ok(Step::Exit { code })
}

/// The channel a writing step names. Every such step names it explicitly:
/// which surface a scenario writes to is a scripted fact, never a default.
fn parse_channel(index: usize, kind: &str, fields: &Map<String, Value>) -> Result<Channel, String> {
    match fields.get("channel") {
        Some(Value::String(channel)) if channel == "stdout" => Ok(Channel::Stdout),
        Some(Value::String(channel)) if channel == "stderr" => Err(format!(
            "step {index} ({kind}): channel \"stderr\" is reserved until a scenario needs it — script \"stdout\""
        )),
        Some(Value::String(channel)) => Err(format!(
            "step {index} ({kind}): unknown channel \"{channel}\" — the scripted channel is \"stdout\""
        )),
        Some(_) => Err(format!(
            "step {index} ({kind}): \"channel\" must be a string"
        )),
        None => Err(format!(
            "step {index} ({kind}): missing \"channel\" — every writing step names its channel explicitly"
        )),
    }
}

/// A tuning knob with a documented default. Absence means the default;
/// anything that is not a non-negative integer is a typo worth naming.
fn optional_u64(
    index: usize,
    kind: &str,
    fields: &Map<String, Value>,
    name: &str,
    default: u64,
) -> Result<u64, String> {
    match fields.get(name) {
        None => Ok(default),
        Some(value) => value.as_u64().ok_or_else(|| {
            format!("step {index} ({kind}): \"{name}\" must be a non-negative integer")
        }),
    }
}

fn reject_unknown_fields(
    index: usize,
    kind: &str,
    fields: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), String> {
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("step {index} ({kind}): unknown field \"{key}\""));
        }
    }
    Ok(())
}

fn name_fields<I, S>(keys: I) -> String
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    let named: Vec<String> = keys.map(|key| format!("\"{}\"", key.as_ref())).collect();
    if named.is_empty() {
        "no fields".to_string()
    } else {
        format!("field(s) {}", named.join(", "))
    }
}

fn name_kinds() -> String {
    STEP_KINDS
        .into_iter()
        .map(|kind| format!("\"{kind}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(json: &str) -> String {
        parse(json).expect_err("this scenario must be rejected")
    }

    #[test]
    fn the_reference_scenario_shape_parses() {
        let scenario = parse(
            r#"{
              "name": "approval-then-token",
              "steps": [
                { "emit": "Allow filesystem write? [y/N]\n", "channel": "stdout" },
                { "await_stdin": "y\n", "timeout_ms": 1000 },
                { "emit": "Writing file...\n", "channel": "stdout", "byte_delay_ms": 5 },
                { "exit": 0 }
              ]
            }"#,
        )
        .expect("the reference shape must parse");
        assert_eq!(scenario.name, "approval-then-token");
        assert_eq!(scenario.steps.len(), 4);
        assert_eq!(
            scenario.steps[0],
            Step::Emit {
                text: "Allow filesystem write? [y/N]\n".into(),
                channel: Channel::Stdout,
                byte_delay_ms: 0,
                repeat: 1,
                repeat_interval_us: 0,
            },
            "an emit defaults to one unpaced write"
        );
        assert_eq!(
            scenario.steps[1],
            Step::AwaitStdin {
                expected: "y\n".into(),
                timeout_ms: 1000,
            }
        );
        assert_eq!(
            scenario.steps[2],
            Step::Emit {
                text: "Writing file...\n".into(),
                channel: Channel::Stdout,
                byte_delay_ms: 5,
                repeat: 1,
                repeat_interval_us: 0,
            }
        );
        assert_eq!(scenario.steps[3], Step::Exit { code: 0 });
    }

    #[test]
    fn unknown_step_kind_rejected_with_named_key() {
        let err = parse_err(r#"{"name":"x","steps":[{"explode":true},{"exit":0}]}"#);
        assert!(err.contains("step 0"), "must name the step index: {err}");
        assert!(
            err.contains("\"explode\""),
            "must name the offending key: {err}"
        );
        assert!(
            err.contains("\"emit\"") && err.contains("\"await_stdin\"") && err.contains("\"exit\""),
            "must name the kinds a step can carry: {err}"
        );
    }

    #[test]
    fn a_step_with_no_fields_is_rejected() {
        let err = parse_err(r#"{"name":"x","steps":[{},{"exit":0}]}"#);
        assert!(err.contains("no fields"), "unexpected error: {err}");
    }

    #[test]
    fn ambiguous_step_kinds_are_rejected() {
        let err = parse_err(r#"{"name":"x","steps":[{"emit":"hi","channel":"stdout","exit":0}]}"#);
        assert!(err.contains("ambiguous"), "unexpected error: {err}");
        assert!(
            err.contains("\"emit\"") && err.contains("\"exit\""),
            "must name both carried kinds: {err}"
        );
    }

    #[test]
    fn unknown_field_on_a_step_is_named() {
        let err = parse_err(
            r#"{"name":"x","steps":[{"emit":"hi","channel":"stdout","color":"red"},{"exit":0}]}"#,
        );
        assert!(
            err.contains("\"color\""),
            "must name the unknown field: {err}"
        );
    }

    #[test]
    fn stderr_channel_is_reserved() {
        let err =
            parse_err(r#"{"name":"x","steps":[{"emit":"hi","channel":"stderr"},{"exit":0}]}"#);
        assert!(err.contains("reserved"), "unexpected error: {err}");
    }

    #[test]
    fn unknown_channels_are_rejected() {
        let err =
            parse_err(r#"{"name":"x","steps":[{"emit":"hi","channel":"socket"},{"exit":0}]}"#);
        assert!(err.contains("\"socket\""), "unexpected error: {err}");
    }

    #[test]
    fn emit_requires_an_explicit_channel() {
        let err = parse_err(r#"{"name":"x","steps":[{"emit":"hi"},{"exit":0}]}"#);
        assert!(err.contains("\"channel\""), "unexpected error: {err}");
    }

    #[test]
    fn byte_delay_must_be_a_non_negative_integer() {
        for value in ["\"5\"", "5.5", "-1"] {
            let err = parse_err(&format!(
                r#"{{"name":"x","steps":[{{"emit":"hi","channel":"stdout","byte_delay_ms":{value}}},{{"exit":0}}]}}"#
            ));
            assert!(
                err.contains("byte_delay_ms"),
                "byte_delay_ms={value} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn emit_carries_its_repetition_knobs() {
        let scenario = parse(
            r#"{"name":"markers","steps":[
                 {"emit": "M{ts}\n", "channel": "stdout", "repeat": 10000, "repeat_interval_us": 1000},
                 {"exit": 0}
               ]}"#,
        )
        .expect("a repeated emit must parse");
        assert_eq!(
            scenario.steps[0],
            Step::Emit {
                text: "M{ts}\n".into(),
                channel: Channel::Stdout,
                byte_delay_ms: 0,
                repeat: 10_000,
                repeat_interval_us: 1_000,
            }
        );
    }

    #[test]
    fn an_emit_that_repeats_zero_times_is_rejected() {
        let err = parse_err(
            r#"{"name":"x","steps":[{"emit":"hi","channel":"stdout","repeat":0},{"exit":0}]}"#,
        );
        assert!(err.contains("repeat"), "unexpected error: {err}");
    }

    #[test]
    fn generate_defaults_every_knob_but_the_line_count_and_channel() {
        let scenario = parse(
            r#"{"name":"soak","steps":[
                 {"generate": 1800000, "channel": "stdout"},
                 {"exit": 0}
               ]}"#,
        )
        .expect("a minimal generate step must parse");
        assert_eq!(
            scenario.steps[0],
            Step::Generate {
                lines: 1_800_000,
                line_bytes: DEFAULT_LINE_BYTES,
                checksum_every: 0,
                line_interval_us: 0,
                channel: Channel::Stdout,
            }
        );
    }

    #[test]
    fn generate_carries_its_knobs() {
        let scenario = parse(
            r#"{"name":"soak","steps":[
                 {"generate": 100, "channel": "stdout", "line_bytes": 96,
                  "checksum_every": 25, "line_interval_us": 1000},
                 {"exit": 0}
               ]}"#,
        )
        .expect("a fully specified generate step must parse");
        assert_eq!(
            scenario.steps[0],
            Step::Generate {
                lines: 100,
                line_bytes: 96,
                checksum_every: 25,
                line_interval_us: 1_000,
                channel: Channel::Stdout,
            }
        );
    }

    #[test]
    fn generate_requires_a_positive_line_count() {
        for value in ["0", "-1", "\"many\""] {
            let err = parse_err(&format!(
                r#"{{"name":"x","steps":[{{"generate":{value},"channel":"stdout"}},{{"exit":0}}]}}"#
            ));
            assert!(
                err.contains("generate"),
                "generate={value} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn generate_requires_an_explicit_channel() {
        let err = parse_err(r#"{"name":"x","steps":[{"generate":10},{"exit":0}]}"#);
        assert!(err.contains("\"channel\""), "unexpected error: {err}");
    }

    #[test]
    fn generate_rejects_an_empty_payload() {
        // "L7 " with nothing after the space: a terminal may trim the bare
        // trailing space, and the line stops parsing as a payload line.
        let err = parse_err(
            r#"{"name":"x","steps":[{"generate":10,"channel":"stdout","line_bytes":0},{"exit":0}]}"#,
        );
        assert!(err.contains("line_bytes"), "unexpected error: {err}");
        assert!(err.contains("trim"), "the rejection must say why: {err}");
    }

    #[test]
    fn generate_rejects_lines_no_terminal_could_hold() {
        let err = parse_err(
            r#"{"name":"x","steps":[{"generate":10,"channel":"stdout","line_bytes":4096},{"exit":0}]}"#,
        );
        assert!(err.contains("line_bytes"), "unexpected error: {err}");
        assert!(
            err.contains("reflow"),
            "the rejection must say why the cap exists: {err}"
        );
    }

    #[test]
    fn generate_rejects_unknown_fields() {
        let err = parse_err(
            r#"{"name":"x","steps":[{"generate":10,"channel":"stdout","line_delay_ms":5},{"exit":0}]}"#,
        );
        assert!(
            err.contains("\"line_delay_ms\""),
            "must name the unknown field: {err}"
        );
    }

    #[test]
    fn await_stdin_requires_a_timeout() {
        let err = parse_err(r#"{"name":"x","steps":[{"await_stdin":"y\n"},{"exit":0}]}"#);
        assert!(err.contains("timeout_ms"), "unexpected error: {err}");
    }

    #[test]
    fn await_stdin_timeout_must_be_positive() {
        let err =
            parse_err(r#"{"name":"x","steps":[{"await_stdin":"y\n","timeout_ms":0},{"exit":0}]}"#);
        assert!(err.contains("positive"), "unexpected error: {err}");
    }

    #[test]
    fn exit_code_must_fit_an_i32() {
        let err = parse_err(r#"{"name":"x","steps":[{"exit":99999999999}]}"#);
        assert!(err.contains("exit"), "unexpected error: {err}");
    }

    #[test]
    fn the_final_step_must_be_exit() {
        let err = parse_err(r#"{"name":"x","steps":[{"emit":"hi","channel":"stdout"}]}"#);
        assert!(err.contains("final step"), "unexpected error: {err}");
    }

    #[test]
    fn exit_before_the_end_leaves_unreachable_steps() {
        let err = parse_err(
            r#"{"name":"x","steps":[{"exit":0},{"emit":"hi","channel":"stdout"},{"exit":0}]}"#,
        );
        assert!(err.contains("unreachable"), "unexpected error: {err}");
    }

    #[test]
    fn steps_must_not_be_empty() {
        let err = parse_err(r#"{"name":"x","steps":[]}"#);
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn the_name_must_be_a_non_empty_string() {
        assert!(parse_err(r#"{"steps":[{"exit":0}]}"#).contains("name"));
        assert!(parse_err(r#"{"name":"","steps":[{"exit":0}]}"#).contains("name"));
    }

    #[test]
    fn unknown_top_level_fields_are_rejected() {
        let err = parse_err(r#"{"name":"x","steps":[{"exit":0}],"extra":1}"#);
        assert!(err.contains("\"extra\""), "unexpected error: {err}");
    }
}
