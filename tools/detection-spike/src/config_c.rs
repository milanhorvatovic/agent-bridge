//! Pipeline configuration (c): the structured-side-channel pipeline.
//!
//! The recorded hook payloads and transcript JSONL — the channels the
//! planned runtime treats as primary — replay through structural
//! classification, while the byte stream serves only the **fallback
//! surfaces** the side channels cannot carry: the first-run trust dialog,
//! the ask-degraded permission dialog, and the interrupted notice, detected
//! on the materialized screen at the same evaluation points configuration
//! (b) uses. Every hook payload is one **emission**; every transcript
//! content block (and every non-message record) is one; every fallback
//! surface detection is one. That denominator is a different population
//! from both (a)'s stripped lines and (b)'s deduplicated screen rows — the
//! three ratios sit side by side in the summary and are never mixed,
//! because this configuration's residual risk lives in fallback coverage
//! and transcript-shape drift, and a mixed denominator would hide exactly
//! that.
//!
//! The transcript is read through the tailer's offset contract, following
//! the paths the `SessionStart` payloads advertise in order: a `/clear`
//! forks a new path, so the clear fixtures replay two files (the committed
//! `transcript.pre-clear.jsonl` and then `transcript.jsonl`) — skipping the
//! pre-clear turn would manufacture false negatives for content the
//! channel really carried. Hook and transcript sightings of the same tool
//! call are correlated by `tool_use_id`, never a synthesized id, and the
//! replay tracks how many `PreToolUse` decisions were ever pending at once:
//! the captured corpus serialises batched tool calls (`Pre(A) Post(A)
//! Pre(B) Post(B)`), so a depth above one is a contract-change finding,
//! not an expected shape.
//!
//! Claude-only: the corpus records side channels for no other CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::channel::{self, HookPayload, TranscriptContent, TranscriptRecord};
use crate::dialog;
use crate::pacing::PacedInput;
use crate::patterns::Cli;
use crate::screen;
use crate::tailer::Tailer;

/// The interrupted notice as the screen paints it — the same anchor the
/// screen pattern set carries, evaluated here as a fallback surface because
/// the interrupt fires no hook.
const INTERRUPTED_NOTICE: &str = "Interrupted · What should Claude do instead";

/// Channel counters of one fixture replay. `emissions` is the denominator
/// of the unrecognized ratio: hook events plus transcript blocks plus
/// fallback-surface detections.
#[derive(Debug, Default, serde::Serialize)]
pub struct ChannelStats {
    pub hook_events: u64,
    /// Transcript files replayed, in advertised-path order — 2 for the
    /// clear fixtures, 1 everywhere else.
    pub transcript_files: u64,
    pub transcript_records: u64,
    /// Content blocks of message records plus one per non-message record —
    /// the transcript's share of the emission denominator.
    pub transcript_blocks: u64,
    /// Evaluation points the fallback screen pass sampled; machinery, not
    /// denominator.
    pub fallback_eval_points: u64,
    /// Fallback-surface detections — the only screen-side emissions this
    /// configuration counts.
    pub fallback_detections: u64,
    pub emissions: u64,
    pub matched: u64,
    pub unrecognized: u64,
    /// Most `PreToolUse` decisions pending at once. The captured corpus
    /// never exceeds 1 — the serial-bracketing finding.
    pub max_pending_approvals: u64,
}

/// One tool call's cross-channel evidence, keyed by the CLI's own
/// `tool_use_id`. All four sightings present means the hook and transcript
/// channels correlated; anything less is a correlation failure the report
/// shows per id.
#[derive(Debug, serde::Serialize)]
pub struct ToolCorrelation {
    pub tool_use_id: String,
    pub pre_hook: bool,
    pub post_hook: bool,
    pub transcript_use: bool,
    pub transcript_result: bool,
}

impl ToolCorrelation {
    pub fn correlated(&self) -> bool {
        self.pre_hook && self.post_hook && self.transcript_use && self.transcript_result
    }
}

/// One fallback-surface appearance: a dialog arriving on screen with its
/// extracted options, or the interrupted notice (no options).
#[derive(Debug, serde::Serialize)]
pub struct FallbackSighting {
    pub id: &'static str,
    pub eval_point: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<dialog::DialogOption>,
}

/// Everything one replay of one fixture produced.
#[derive(Debug)]
pub struct ReplayOutcome {
    pub stats: ChannelStats,
    /// Emission-level firings per classifier id.
    pub pattern_hits: BTreeMap<&'static str, u64>,
    /// Distinct unrecognized shapes with occurrence counts — event names,
    /// notification types, record types, block types the table does not
    /// know.
    pub unmatched: BTreeMap<String, u64>,
    pub tool_pairs: Vec<ToolCorrelation>,
    pub fallback: Vec<FallbackSighting>,
}

/// The parsed side-channel input of one fixture.
#[derive(Debug)]
pub struct ChannelInput {
    pub hooks: Vec<HookPayload>,
    /// Transcript files in advertised-path order, each fully parsed.
    pub transcripts: Vec<Vec<TranscriptRecord>>,
}

/// Load one fixture's side-channel artifacts. Malformed JSON, a missing
/// artifact, or an advertised-path set that disagrees with the committed
/// files is an error — on committed fixtures those mean corpus corruption,
/// not drift, and measuring over them would be a lie.
pub fn load(dir: &Path) -> Result<ChannelInput, String> {
    let hooks_path = dir.join("hook-payloads.ndjson");
    let raw = fs::read_to_string(&hooks_path).map_err(|err| {
        format!(
            "{}: {err} — configuration (c) replays the structured side channels, \
             which only the claude corpus records",
            hooks_path.display()
        )
    })?;
    let mut hooks = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let payload: HookPayload = serde_json::from_str(line)
            .map_err(|err| format!("{}:{}: {err}", hooks_path.display(), index + 1))?;
        hooks.push(payload);
    }

    // The paths the SessionStart payloads advertise, in order. The record
    // lane commits at most one path switch (the `/clear` fork), so more
    // than two distinct paths cannot map onto the committed artifacts.
    let mut advertised: Vec<&str> = Vec::new();
    for payload in &hooks {
        if payload.hook_event_name != "SessionStart" {
            continue;
        }
        let path = payload.transcript_path.as_deref().ok_or_else(|| {
            format!(
                "{}: SessionStart without transcript_path — the content channel \
                 cannot be followed",
                hooks_path.display()
            )
        })?;
        if advertised.last() != Some(&path) {
            advertised.push(path);
        }
    }

    let main = dir.join("transcript.jsonl");
    let pre_clear = dir.join("transcript.pre-clear.jsonl");
    let artifacts = match (advertised.len(), pre_clear.is_file()) {
        (0, _) => {
            return Err(format!(
                "{}: no SessionStart advertises a transcript path",
                hooks_path.display()
            ));
        }
        (1, false) => vec![main],
        (1, true) => {
            return Err(format!(
                "{}: transcript.pre-clear.jsonl is committed but only one \
                 transcript path was advertised",
                dir.display()
            ));
        }
        (2, true) => vec![pre_clear, main],
        (2, false) => {
            return Err(format!(
                "{}: two transcript paths advertised but \
                 transcript.pre-clear.jsonl is missing",
                dir.display()
            ));
        }
        (count, _) => {
            return Err(format!(
                "{}: {count} distinct transcript paths advertised — the record \
                 lane commits at most one path switch",
                dir.display()
            ));
        }
    };

    let mut transcripts = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        if !artifact.is_file() {
            return Err(format!("{}: missing", artifact.display()));
        }
        let mut tailer = Tailer::follow(artifact.clone());
        let mut lines = Vec::new();
        loop {
            let batch = tailer.poll()?;
            if batch.is_empty() {
                break;
            }
            lines.extend(batch);
        }
        if tailer.pending() != 0 {
            return Err(format!(
                "{}: ends mid-record with {} unterminated byte(s)",
                artifact.display(),
                tailer.pending()
            ));
        }
        let mut records = Vec::with_capacity(lines.len());
        for (index, line) in lines.iter().enumerate() {
            let record: TranscriptRecord = serde_json::from_str(line)
                .map_err(|err| format!("{}:{}: {err}", artifact.display(), index + 1))?;
            records.push(record);
        }
        if records.is_empty() {
            return Err(format!("{}: empty transcript", artifact.display()));
        }
        transcripts.push(records);
    }

    Ok(ChannelInput { hooks, transcripts })
}

fn pair_mut<'pairs>(
    pairs: &'pairs mut Vec<ToolCorrelation>,
    tool_use_id: &str,
) -> &'pairs mut ToolCorrelation {
    match pairs
        .iter()
        .position(|pair| pair.tool_use_id == tool_use_id)
    {
        Some(index) => &mut pairs[index],
        None => {
            pairs.push(ToolCorrelation {
                tool_use_id: tool_use_id.to_string(),
                pre_hook: false,
                post_hook: false,
                transcript_use: false,
                transcript_result: false,
            });
            pairs.last_mut().expect("pair just pushed")
        }
    }
}

/// The identity a screen-side dialog detection is reported under in this
/// configuration's accounting.
fn fallback_id(dialog_id: &'static str) -> &'static str {
    match dialog_id {
        "claude/screen-dialog-permission" => "claude/fallback-dialog-permission",
        "claude/screen-dialog-trust" => "claude/fallback-dialog-trust",
        other => {
            panic!("dialog {other} has no fallback identity — configuration (c) is claude-only")
        }
    }
}

/// Replay one fixture's side channels plus the fallback screen pass. Errors
/// mean the byte stream could not replay through the virtual terminal —
/// channel-shape surprises are measurements, not errors, and load already
/// rejected corrupt artifacts.
pub fn replay(
    input: &ChannelInput,
    paced: &PacedInput,
    cols: u16,
    rows: u16,
) -> Result<ReplayOutcome, String> {
    let mut stats = ChannelStats::default();
    let mut pattern_hits: BTreeMap<&'static str, u64> = channel::CHANNEL_CLASSIFIERS
        .iter()
        .map(|spec| (spec.id, 0))
        .collect();
    let mut unmatched: BTreeMap<String, u64> = BTreeMap::new();
    let mut pairs: Vec<ToolCorrelation> = Vec::new();
    let mut sightings: Vec<FallbackSighting> = Vec::new();

    // --- hook channel ---
    let mut pending: BTreeSet<&str> = BTreeSet::new();
    for payload in &input.hooks {
        stats.hook_events += 1;
        match channel::classify_hook(payload) {
            Ok(id) => {
                stats.matched += 1;
                *pattern_hits.entry(id).or_insert(0) += 1;
            }
            Err(sample) => {
                stats.unrecognized += 1;
                *unmatched
                    .entry(crate::metrics::truncate_sample(&sample))
                    .or_insert(0) += 1;
            }
        }
        // Correlation and pending depth run on the raw id so a payload
        // that fails classification still leaves its correlation evidence.
        if let Some(tool_use_id) = payload.tool_use_id.as_deref() {
            match payload.hook_event_name.as_str() {
                "PreToolUse" => {
                    pair_mut(&mut pairs, tool_use_id).pre_hook = true;
                    pending.insert(tool_use_id);
                    stats.max_pending_approvals =
                        stats.max_pending_approvals.max(pending.len() as u64);
                }
                "PostToolUse" => {
                    pair_mut(&mut pairs, tool_use_id).post_hook = true;
                    pending.remove(tool_use_id);
                }
                _ => {}
            }
        }
    }

    // --- transcript channel ---
    stats.transcript_files = input.transcripts.len() as u64;
    for records in &input.transcripts {
        for record in records {
            stats.transcript_records += 1;
            for outcome in channel::classify_record(record) {
                stats.transcript_blocks += 1;
                match outcome {
                    Ok(id) => {
                        stats.matched += 1;
                        *pattern_hits.entry(id).or_insert(0) += 1;
                    }
                    Err(sample) => {
                        stats.unrecognized += 1;
                        *unmatched
                            .entry(crate::metrics::truncate_sample(&sample))
                            .or_insert(0) += 1;
                    }
                }
            }
            let Some(message) = &record.message else {
                continue;
            };
            let TranscriptContent::Blocks(blocks) = &message.content else {
                continue;
            };
            for block in blocks {
                match block.block_type.as_str() {
                    "tool_use" => {
                        if let Some(id) = block.id.as_deref() {
                            pair_mut(&mut pairs, id).transcript_use = true;
                        }
                    }
                    "tool_result" => {
                        if let Some(id) = block.tool_use_id.as_deref() {
                            pair_mut(&mut pairs, id).transcript_result = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // --- fallback screen pass: fallback surfaces only ---
    let points = screen::eval_points(paced, cols, rows)?;
    let specs = dialog::for_cli(Cli::Claude);
    let mut open_dialogs: BTreeSet<&'static str> = BTreeSet::new();
    let mut notice_open = false;
    for point in &points {
        stats.fallback_eval_points += 1;

        let detections = dialog::detect(&specs, &point.rows);
        let now_open: BTreeSet<&'static str> = detections
            .iter()
            .map(|detection| detection.spec.id)
            .collect();
        for detection in detections {
            if !open_dialogs.contains(detection.spec.id) {
                let id = fallback_id(detection.spec.id);
                stats.fallback_detections += 1;
                stats.matched += 1;
                *pattern_hits.entry(id).or_insert(0) += 1;
                sightings.push(FallbackSighting {
                    id,
                    eval_point: point.ordinal,
                    options: detection.options,
                });
            }
        }
        open_dialogs = now_open;

        let notice_shown = point
            .rows
            .iter()
            .any(|row| row.contains(INTERRUPTED_NOTICE));
        if notice_shown && !notice_open {
            stats.fallback_detections += 1;
            stats.matched += 1;
            *pattern_hits
                .entry("claude/fallback-interrupted-notice")
                .or_insert(0) += 1;
            sightings.push(FallbackSighting {
                id: "claude/fallback-interrupted-notice",
                eval_point: point.ordinal,
                options: Vec::new(),
            });
        }
        notice_open = notice_shown;
    }

    stats.emissions = stats.hook_events + stats.transcript_blocks + stats.fallback_detections;
    Ok(ReplayOutcome {
        stats,
        pattern_hits,
        unmatched,
        tool_pairs: pairs,
        fallback: sightings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::ChunkBoundary;
    use crate::screen::QUIET_GAP_NS;

    /// Build a paced input from (bytes, arrival-time) pairs.
    fn paced(chunks: &[(&[u8], u64)]) -> PacedInput {
        let mut bytes = Vec::new();
        let mut boundaries = Vec::new();
        for (chunk, monotonic_ns) in chunks {
            boundaries.push(ChunkBoundary {
                offset: bytes.len(),
                len: chunk.len(),
                monotonic_ns: *monotonic_ns,
            });
            bytes.extend_from_slice(chunk);
        }
        PacedInput {
            bytes,
            chunks: boundaries,
        }
    }

    fn quiet_stream() -> PacedInput {
        paced(&[(b"\x1b[1;1Hready", 0)])
    }

    fn hook(json: serde_json::Value) -> HookPayload {
        serde_json::from_value(json).expect("test payload parses")
    }

    fn tool_hooks(event: &str, tool_use_id: &str) -> HookPayload {
        hook(serde_json::json!({
            "hook_event_name": event,
            "session_id": "s",
            "transcript_path": "/tmp/s.jsonl",
            "tool_name": "Read",
            "tool_use_id": tool_use_id,
            "tool_input": {"file_path": "/etc/hosts"},
        }))
    }

    fn record(json: serde_json::Value) -> TranscriptRecord {
        serde_json::from_value(json).expect("test record parses")
    }

    fn input(hooks: Vec<HookPayload>, transcripts: Vec<Vec<TranscriptRecord>>) -> ChannelInput {
        ChannelInput { hooks, transcripts }
    }

    #[test]
    fn serial_bracketing_keeps_one_approval_pending() {
        let hooks = vec![
            tool_hooks("PreToolUse", "toolu_A"),
            tool_hooks("PostToolUse", "toolu_A"),
            tool_hooks("PreToolUse", "toolu_B"),
            tool_hooks("PostToolUse", "toolu_B"),
        ];
        let outcome = replay(&input(hooks, Vec::new()), &quiet_stream(), 80, 24).expect("replays");
        assert_eq!(outcome.stats.max_pending_approvals, 1);
        assert_eq!(outcome.tool_pairs.len(), 2);
    }

    #[test]
    fn concurrent_pre_hooks_would_be_measured_not_lost() {
        let hooks = vec![
            tool_hooks("PreToolUse", "toolu_A"),
            tool_hooks("PreToolUse", "toolu_B"),
            tool_hooks("PostToolUse", "toolu_A"),
            tool_hooks("PostToolUse", "toolu_B"),
        ];
        let outcome = replay(&input(hooks, Vec::new()), &quiet_stream(), 80, 24).expect("replays");
        assert_eq!(outcome.stats.max_pending_approvals, 2);
    }

    #[test]
    fn tool_calls_correlate_across_both_channels_by_tool_use_id() {
        let hooks = vec![
            tool_hooks("PreToolUse", "toolu_A"),
            tool_hooks("PostToolUse", "toolu_A"),
        ];
        let transcript = vec![
            record(serde_json::json!({
                "type": "assistant",
                "message": {"content": [
                    {"type": "tool_use", "id": "toolu_A", "name": "Read", "input": {}},
                ]},
            })),
            record(serde_json::json!({
                "type": "user",
                "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "toolu_A", "content": "ok"},
                ]},
            })),
        ];
        let outcome =
            replay(&input(hooks, vec![transcript]), &quiet_stream(), 80, 24).expect("replays");
        assert_eq!(outcome.tool_pairs.len(), 1);
        assert!(outcome.tool_pairs[0].correlated());
    }

    #[test]
    fn a_transcript_only_tool_call_is_an_uncorrelated_pair() {
        let transcript = vec![record(serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": "toolu_ghost", "name": "Read", "input": {}},
            ]},
        }))];
        let outcome = replay(
            &input(Vec::new(), vec![transcript]),
            &quiet_stream(),
            80,
            24,
        )
        .expect("replays");
        assert_eq!(outcome.tool_pairs.len(), 1);
        assert!(!outcome.tool_pairs[0].correlated());
        assert!(outcome.tool_pairs[0].transcript_use);
        assert!(!outcome.tool_pairs[0].pre_hook);
    }

    #[test]
    fn unknown_shapes_count_as_unrecognized_with_samples() {
        let hooks = vec![hook(serde_json::json!({"hook_event_name": "SubagentStop"}))];
        let transcript = vec![
            record(serde_json::json!({"type": "mode", "mode": "default"})),
            record(serde_json::json!({"type": "queued-command"})),
        ];
        let outcome =
            replay(&input(hooks, vec![transcript]), &quiet_stream(), 80, 24).expect("replays");

        assert_eq!(outcome.stats.hook_events, 1);
        assert_eq!(outcome.stats.transcript_blocks, 2);
        assert_eq!(outcome.stats.unrecognized, 2);
        assert_eq!(outcome.stats.matched, 1);
        assert_eq!(outcome.unmatched.get("hook:SubagentStop"), Some(&1));
        assert_eq!(outcome.unmatched.get("transcript:queued-command"), Some(&1));
    }

    #[test]
    fn emissions_are_the_sum_of_the_three_channel_populations() {
        let hooks = vec![hook(serde_json::json!({
            "hook_event_name": "Stop",
        }))];
        let transcript = vec![record(serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "thinking", "thinking": "…"},
                {"type": "text", "text": "done"},
            ]},
        }))];
        // One quiet period ending on the interrupted notice: one fallback
        // detection.
        let stream = paced(&[
            (b"\x1b[1;1Hworking", 0),
            (
                "\x1b[2;1H⎿  Interrupted · What should Claude do instead?".as_bytes(),
                QUIET_GAP_NS,
            ),
        ]);
        let outcome = replay(&input(hooks, vec![transcript]), &stream, 80, 24).expect("replays");

        assert_eq!(outcome.stats.hook_events, 1);
        assert_eq!(outcome.stats.transcript_blocks, 2);
        assert_eq!(outcome.stats.fallback_detections, 1);
        assert_eq!(outcome.stats.emissions, 4);
        assert_eq!(
            outcome.stats.matched + outcome.stats.unrecognized,
            outcome.stats.emissions
        );
        assert_eq!(
            outcome.pattern_hits["claude/fallback-interrupted-notice"],
            1
        );
    }

    #[test]
    fn a_fallback_surface_spanning_evaluation_points_counts_once() {
        let dialog_paint =
            b"\x1b[2;1H Do you want to proceed?\x1b[3;1H \xe2\x9d\xaf 1. Yes\x1b[4;1H   2. No";
        let stream = paced(&[
            (&dialog_paint[..], 0),
            // Still open at the next quiet boundary: same appearance.
            (b"\x1b[1;1H", QUIET_GAP_NS),
            (b"\x1b[2J\x1b[1;1Hdone", 2 * QUIET_GAP_NS),
        ]);
        let outcome = replay(&input(Vec::new(), Vec::new()), &stream, 80, 24).expect("replays");

        assert_eq!(outcome.stats.fallback_detections, 1);
        assert_eq!(outcome.pattern_hits["claude/fallback-dialog-permission"], 1);
        assert_eq!(outcome.fallback.len(), 1);
        assert_eq!(outcome.fallback[0].options.len(), 2);
    }

    #[test]
    fn every_classifier_id_appears_in_the_hit_map_even_at_zero() {
        let outcome =
            replay(&input(Vec::new(), Vec::new()), &quiet_stream(), 80, 24).expect("replays");
        for spec in channel::CHANNEL_CLASSIFIERS {
            assert!(
                outcome.pattern_hits.contains_key(spec.id),
                "{} missing from the hit map",
                spec.id
            );
        }
    }

    #[test]
    fn an_unreplayable_stream_is_an_error_not_a_short_outcome() {
        let stream = paced(&[(&[0xFF, 0xFE], 0)]);
        let err = replay(&input(Vec::new(), Vec::new()), &stream, 80, 24)
            .expect_err("undecodable bytes must abort");
        assert!(err.contains("UTF-8"), "must name the cause: {err}");
    }

    // --- load: path mapping against the committed artifacts ---

    struct TempFixture(std::path::PathBuf);

    impl TempFixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "detection-spike-config-c-{name}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create fixture dir");
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) {
            fs::write(self.0.join(name), content).expect("write artifact");
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session_start(path: &str) -> String {
        format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"s","source":"startup","transcript_path":"{path}"}}"#
        )
    }

    const A_RECORD: &str = r#"{"type":"mode","mode":"default"}"#;

    #[test]
    fn a_single_advertised_path_replays_the_main_transcript() {
        let fixture = TempFixture::new("single");
        fixture.write(
            "hook-payloads.ndjson",
            &format!("{}\n", session_start("/a")),
        );
        fixture.write("transcript.jsonl", &format!("{A_RECORD}\n"));

        let input = load(&fixture.0).expect("loads");
        assert_eq!(input.transcripts.len(), 1);
        assert_eq!(input.hooks.len(), 1);
    }

    #[test]
    fn a_path_switch_replays_the_pre_clear_file_first() {
        let fixture = TempFixture::new("switch");
        fixture.write(
            "hook-payloads.ndjson",
            &format!("{}\n{}\n", session_start("/a"), session_start("/b")),
        );
        fixture.write(
            "transcript.pre-clear.jsonl",
            &format!("{A_RECORD}\n{A_RECORD}\n"),
        );
        fixture.write("transcript.jsonl", &format!("{A_RECORD}\n"));

        let input = load(&fixture.0).expect("loads");
        assert_eq!(input.transcripts.len(), 2);
        assert_eq!(
            input.transcripts[0].len(),
            2,
            "the pre-clear file replays first"
        );
    }

    #[test]
    fn advertised_paths_and_committed_artifacts_must_agree() {
        let fixture = TempFixture::new("disagree");
        fixture.write(
            "hook-payloads.ndjson",
            &format!("{}\n{}\n", session_start("/a"), session_start("/b")),
        );
        fixture.write("transcript.jsonl", &format!("{A_RECORD}\n"));
        let err = load(&fixture.0).expect_err("missing pre-clear file must fail");
        assert!(err.contains("pre-clear"), "got: {err}");

        let fixture = TempFixture::new("stray");
        fixture.write(
            "hook-payloads.ndjson",
            &format!("{}\n", session_start("/a")),
        );
        fixture.write("transcript.jsonl", &format!("{A_RECORD}\n"));
        fixture.write("transcript.pre-clear.jsonl", &format!("{A_RECORD}\n"));
        let err = load(&fixture.0).expect_err("stray pre-clear file must fail");
        assert!(err.contains("only one"), "got: {err}");
    }

    #[test]
    fn malformed_artifacts_are_errors_naming_file_and_line() {
        let fixture = TempFixture::new("malformed");
        fixture.write("hook-payloads.ndjson", "not json\n");
        let err = load(&fixture.0).expect_err("malformed hook payload must fail");
        assert!(
            err.contains("hook-payloads.ndjson:1"),
            "must name file and line: {err}"
        );

        let fixture = TempFixture::new("truncated");
        fixture.write(
            "hook-payloads.ndjson",
            &format!("{}\n", session_start("/a")),
        );
        fixture.write("transcript.jsonl", "{\"type\":\"mode\"}");
        let err = load(&fixture.0).expect_err("unterminated transcript must fail");
        assert!(err.contains("mid-record"), "got: {err}");
    }
}
