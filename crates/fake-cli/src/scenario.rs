//! Scenario schema and validation.
//!
//! A scenario is a JSON object with a `name` and a list of `steps`; each step
//! is discriminated by which key it carries — `emit`, `await_stdin`, or
//! `exit`:
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
    Emit {
        text: String,
        channel: Channel,
        byte_delay_ms: u64,
    },
    /// Block until exactly `expected` arrives on stdin, or fail the run:
    /// diverging input and closed stdin are a mismatch, `timeout_ms`
    /// elapsing is a timeout — each a non-zero exit with a diagnostic.
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

const STEP_KINDS: [&str; 3] = ["emit", "await_stdin", "exit"];

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
    reject_unknown_fields(index, "emit", fields, &["emit", "channel", "byte_delay_ms"])?;
    let text = match fields.get("emit") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => {
            return Err(format!(
                "step {index} (emit): \"emit\" must be a non-empty string"
            ));
        }
    };
    let channel = match fields.get("channel") {
        Some(Value::String(channel)) if channel == "stdout" => Channel::Stdout,
        Some(Value::String(channel)) if channel == "stderr" => {
            return Err(format!(
                "step {index} (emit): channel \"stderr\" is reserved until a scenario needs it — script \"stdout\""
            ));
        }
        Some(Value::String(channel)) => {
            return Err(format!(
                "step {index} (emit): unknown channel \"{channel}\" — the scripted channel is \"stdout\""
            ));
        }
        Some(_) => return Err(format!("step {index} (emit): \"channel\" must be a string")),
        None => {
            return Err(format!(
                "step {index} (emit): missing \"channel\" — every emit names its channel explicitly"
            ));
        }
    };
    let byte_delay_ms = match fields.get("byte_delay_ms") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| {
            format!(
                "step {index} (emit): \"byte_delay_ms\" must be a non-negative integer (milliseconds between successive bytes)"
            )
        })?,
    };
    Ok(Step::Emit {
        text,
        channel,
        byte_delay_ms,
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
            },
            "byte_delay_ms must default to 0 (no pacing)"
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
