//! The interactive-CLI probe's entry point. Each subcommand is one lane;
//! every lane prints machine-readable step lines to stdout and exits
//! non-zero with a step-identifying code on the first hard failure, so CI
//! asserts the exit status and a human reads the log.
//!
//! ```text
//! interactive-probe standin   [--first-token-ms N] [--timeout-secs N]
//! interactive-probe probe     [--claude-bin PATH] [--model NAME]
//!                             [--first-token-ms N] [--capture PATH] [--keep-workdir]
//! interactive-probe fourpoint [same flags as probe]
//! interactive-probe cleanup   [same flags as probe]
//! interactive-probe record    --script PATH --out DIR [--cols N] [--rows N]
//!                             [--cli-bin PATH] [--cli-version LABEL] [--model NAME]
//!                             [--install TEXT] [--first-token-ms N] [--keep-workdir]
//! interactive-probe vt-eval   <capture.ndjson>            (feature `vt-eval`)
//! interactive-probe hook-forward --endpoint ENDPOINT      (invoked by the CLI, not humans)
//! ```

// This binary legitimately owns stdout — its step lines *are* its output,
// and `hook-forward` must print the decision JSON the hooked CLI reads.
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

use agent_bridge_interactive_probe::record::RecordConfig;
use agent_bridge_interactive_probe::rig::ProbeConfig;
use agent_bridge_interactive_probe::standin::StandinLaneConfig;
use agent_bridge_interactive_probe::{
    COLS, Failure, ROWS, cleanup, fourpoint, platform_report, print_step, record, rig, standin,
};

const USAGE: &str = "usage: interactive-probe <standin|probe|fourpoint|cleanup|record|vt-eval|hook-forward> [options]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(subcommand) = args.first() else {
        eprintln!("interactive-probe: {USAGE}");
        exit(2);
    };

    // The hook-forward lane is a CLI-invoked helper, not a probe run: it
    // must stay silent on stdout except for its decision JSON, so it
    // reports nothing else and never prints the platform banner.
    if subcommand == "hook-forward" {
        match parse_endpoint(&args[1..]) {
            Ok(endpoint) => exit(agent_bridge_interactive_probe::hooks::hook_forward(
                &endpoint,
            )),
            Err(message) => {
                eprintln!("interactive-probe: {message}");
                exit(2);
            }
        }
    }

    println!("interactive-probe {}", platform_report());
    let result = match subcommand.as_str() {
        "standin" => parse_standin(&args[1..]).and_then(|config| standin::run_lane(&config)),
        "probe" => parse_probe(&args[1..]).and_then(|config| rig::run_probe(&config)),
        "fourpoint" => parse_probe(&args[1..]).and_then(|config| fourpoint::run(&config)),
        "cleanup" => parse_probe(&args[1..]).and_then(|config| cleanup::run(&config)),
        "record" => parse_record(&args[1..]).and_then(|config| record::run(&config)),
        "vt-eval" => run_vt_eval(&args[1..]),
        other => {
            eprintln!("interactive-probe: unknown subcommand '{other}'. {USAGE}");
            exit(2);
        }
    };

    match result {
        Ok(()) => println!("interactive-probe mode={subcommand} result=pass"),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "interactive-probe: step {} failed: {}",
                failure.step, failure.detail
            );
            exit(failure.code);
        }
    }
}

/// A usage error surfaced as a `Failure` so lanes share one exit path;
/// code 2 is the conventional usage-error status.
fn usage(message: impl Into<String>) -> Failure {
    Failure::new("args", 2, message)
}

fn parse_endpoint(args: &[String]) -> Result<String, String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--endpoint" {
            return iter
                .next()
                .cloned()
                .ok_or_else(|| "--endpoint needs a value".to_string());
        }
    }
    Err("hook-forward needs --endpoint <path-or-pipe-name>".to_string())
}

fn parse_standin(args: &[String]) -> Result<StandinLaneConfig, Failure> {
    let mut config = StandinLaneConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--first-token-ms" => config.first_token_ms = next_number(&mut iter, arg)?,
            "--timeout-secs" => config.timeout = Duration::from_secs(next_number(&mut iter, arg)?),
            other => return Err(usage(format!("unknown standin option: {other}"))),
        }
    }
    Ok(config)
}

fn parse_probe(args: &[String]) -> Result<ProbeConfig, Failure> {
    let mut config = ProbeConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--claude-bin" => config.claude_bin = next_value(&mut iter, arg)?,
            "--model" => config.model = Some(next_value(&mut iter, arg)?),
            "--first-token-ms" => config.first_token_ms = next_number(&mut iter, arg)?,
            "--capture" => config.capture_to = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--keep-workdir" => config.keep_workdir = true,
            other => return Err(usage(format!("unknown probe option: {other}"))),
        }
    }
    Ok(config)
}

fn parse_record(args: &[String]) -> Result<RecordConfig, Failure> {
    let mut config = RecordConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--script" => config.script = PathBuf::from(next_value(&mut iter, arg)?),
            "--out" => config.out = PathBuf::from(next_value(&mut iter, arg)?),
            "--cols" => config.cols = next_dimension(&mut iter, arg)?,
            "--rows" => config.rows = next_dimension(&mut iter, arg)?,
            "--cli-bin" => config.cli_bin = Some(next_value(&mut iter, arg)?),
            "--cli-version" => config.cli_version = Some(next_value(&mut iter, arg)?),
            "--model" => config.model = Some(next_value(&mut iter, arg)?),
            "--install" => config.install = Some(next_value(&mut iter, arg)?),
            "--first-token-ms" => config.first_token_ms = next_number(&mut iter, arg)?,
            "--keep-workdir" => config.keep_workdir = true,
            other => return Err(usage(format!("unknown record option: {other}"))),
        }
    }
    if config.script.as_os_str().is_empty() {
        return Err(usage("record needs --script <scenario.json>"));
    }
    if config.out.as_os_str().is_empty() {
        return Err(usage("record needs --out <fixture-dir>"));
    }
    Ok(config)
}

fn next_dimension<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<u16, Failure> {
    let raw = next_value(iter, flag)?;
    raw.parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| usage(format!("invalid {flag} value: {raw}")))
}

fn next_value<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<String, Failure> {
    iter.next()
        .cloned()
        .ok_or_else(|| usage(format!("{flag} needs a value")))
}

fn next_number<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<u64, Failure> {
    let raw = next_value(iter, flag)?;
    raw.parse()
        .map_err(|_| usage(format!("invalid {flag} value: {raw}")))
}

#[cfg(feature = "vt-eval")]
fn run_vt_eval(args: &[String]) -> Result<(), Failure> {
    let capture = args.first().ok_or_else(|| {
        usage("vt-eval needs a capture path: interactive-probe vt-eval <capture.ndjson>")
    })?;
    agent_bridge_interactive_probe::vt_eval::run(&PathBuf::from(capture), COLS, ROWS)
}

#[cfg(not(feature = "vt-eval"))]
fn run_vt_eval(_args: &[String]) -> Result<(), Failure> {
    let _ = (COLS, ROWS);
    Err(Failure::new(
        "vt_eval",
        2,
        "rebuild with --features vt-eval: the virtual-terminal candidates are not in the default build",
    ))
}
