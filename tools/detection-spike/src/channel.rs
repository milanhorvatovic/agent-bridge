//! The structured-side-channel classifier set and the shapes it reads.
//!
//! Configuration (c) has no text needles: its primary surfaces are typed —
//! hook payloads follow a documented schema and transcript records follow a
//! version-internal but structured one — so its "pattern set" is a table of
//! **structural classifiers**. A hook payload classifies by its event name
//! plus the presence of the fields the adapter flow would consume; a
//! transcript record classifies by its record type and, for message
//! records, per content block. A shape the table does not know — a new
//! event name, notification type, record type, or block type — is an
//! unrecognized emission: exactly the drift this configuration is measured
//! by. Malformed JSON is different: on committed fixtures it means corpus
//! corruption, and the loader treats it as an error, never a measurement.
//!
//! The table was tuned against **claude 2.1.201** (the evidence-base
//! version) plus the documented hook contract, and is left untouched for
//! the neighbouring versions so a vendor-side schema change surfaces as a
//! measured miss. Claude-only throughout: the corpus records side channels
//! for no other CLI.
//!
//! The same three roles as the text sets keep the accounting honest, and
//! the controls mirror the other configurations' instruments in reverse:
//! the **idle notification** — a structural miss in (a) and (b) because it
//! paints nothing — is a first-class anchored hook here, while the
//! **interrupt** — a plain screen paint in (a) and (b) — is this
//! configuration's control, because the Ctrl+C byte fires no hook at all.
//! The `fallback-` classifiers cover the surfaces the side channels
//! structurally cannot see (the trust dialog, the ask-degraded permission
//! dialog, the interrupted notice), detected on the screen by the same
//! machinery configuration (b) measures wholesale.

use serde::Deserialize;

use crate::patterns::Role;

/// One structural classifier: the accounting identity a channel emission is
/// reported under. The classification logic lives in [`classify_hook`] and
/// [`classify_record`]; what varies per classifier is only its identity.
pub struct ChannelSpec {
    /// Stable identifier, `claude/<channel>-<name>` — the key in every
    /// report row.
    pub id: &'static str,
    /// Pipeline-local classification, same vocabulary as the pattern sets.
    pub class: &'static str,
    pub role: Role,
}

/// The classifier set. Anchored classifiers have step-log ground truth;
/// ambient ones classify recurring structure with no per-event expectation
/// (session boundaries the driver never waits on, thinking blocks, the
/// transcript's setup records); the one control marks the surface this
/// configuration structurally lacks.
pub const CHANNEL_CLASSIFIERS: &[ChannelSpec] = &[
    // ----- hook channel -----------------------------------------------------
    ChannelSpec {
        id: "claude/hook-session-start",
        class: "lifecycle.session",
        role: Role::Ambient,
    },
    ChannelSpec {
        id: "claude/hook-session-end",
        class: "lifecycle.session",
        role: Role::Ambient,
    },
    ChannelSpec {
        id: "claude/hook-pre-tool-use",
        class: "tool.request",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/hook-post-tool-use",
        class: "tool.result",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/hook-stop",
        class: "lifecycle.turn",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/hook-notification-permission",
        class: "dialog.permission",
        role: Role::Anchored,
    },
    // First-class here, structurally invisible to the byte stream: the
    // stream and screen sets carry this event as their control.
    ChannelSpec {
        id: "claude/hook-notification-idle",
        class: "notice.idle",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/hook-pre-compact",
        class: "compact.notice",
        role: Role::Anchored,
    },
    // The mirror control: the Ctrl+C interrupt fires no hook in any
    // captured version, so this classifier exists to be red — its false
    // negatives measure the surface the hook channel cannot carry, which
    // is why the interrupted notice is a fallback surface below.
    ChannelSpec {
        id: "claude/hook-interrupt-signal",
        class: "session.interrupted",
        role: Role::Control,
    },
    // ----- transcript channel -----------------------------------------------
    ChannelSpec {
        id: "claude/transcript-user-prompt",
        class: "content.prompt",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/transcript-assistant-text",
        class: "content.response",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/transcript-assistant-thinking",
        class: "content.thinking",
        role: Role::Ambient,
    },
    ChannelSpec {
        id: "claude/transcript-tool-use",
        class: "tool.request",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/transcript-tool-result",
        class: "tool.result",
        role: Role::Anchored,
    },
    ChannelSpec {
        id: "claude/transcript-system-record",
        class: "transcript.system",
        role: Role::Ambient,
    },
    // The session-setup records (mode, snapshots, attachments, titles) the
    // tailer must know to skip — kept in the corpus at full fidelity for
    // exactly this denominator, so an unknown one would surface as drift.
    ChannelSpec {
        id: "claude/transcript-setup-record",
        class: "transcript.setup",
        role: Role::Ambient,
    },
    // ----- screen fallback --------------------------------------------------
    // The ask-degraded permission dialog: the hook announced a decision was
    // needed, the answer happened on the TUI — the dialog paint is the
    // fallback surface the screen must catch.
    ChannelSpec {
        id: "claude/fallback-dialog-permission",
        class: "dialog.permission",
        role: Role::Anchored,
    },
    // The first-run trust dialog paints before any hook fires; the driver
    // never waits on it, so it is ambient like configuration (b)'s twin.
    ChannelSpec {
        id: "claude/fallback-dialog-trust",
        class: "dialog.trust",
        role: Role::Ambient,
    },
    ChannelSpec {
        id: "claude/fallback-interrupted-notice",
        class: "session.interrupted",
        role: Role::Anchored,
    },
];

/// One hook stdin payload, as the capture rig recorded it verbatim. Every
/// field beyond the event name is optional at parse time — which fields an
/// event *requires* is classification, not deserialization.
#[derive(Debug, Default, Deserialize)]
pub struct HookPayload {
    pub hook_event_name: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// One transcript JSONL record. Only the structure the classifier and the
/// correlation walk read is modeled; everything else stays in the file.
#[derive(Debug, Default, Deserialize)]
pub struct TranscriptRecord {
    #[serde(rename = "type")]
    pub record_type: String,
    #[serde(default)]
    pub message: Option<TranscriptMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptMessage {
    pub content: TranscriptContent,
}

/// Message content is either one plain string (a typed prompt, a command
/// echo) or a list of typed blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TranscriptContent {
    Text(String),
    Blocks(Vec<TranscriptBlock>),
}

#[derive(Debug, Default, Deserialize)]
pub struct TranscriptBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    /// `tool_use` blocks carry their own id — the hook-correlation key.
    #[serde(default)]
    pub id: Option<String>,
    /// `tool_result` blocks name the `tool_use` they answer.
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// The non-message record types the tailer skips as session setup.
const SETUP_RECORD_TYPES: [&str; 6] = [
    "mode",
    "permission-mode",
    "file-history-snapshot",
    "attachment",
    "ai-title",
    "last-prompt",
];

fn require(field: &Option<impl Sized>, event: &str, name: &'static str) -> Result<(), String> {
    if field.is_some() {
        Ok(())
    } else {
        Err(format!("hook:{event}#missing-{name}"))
    }
}

/// Classify one hook payload. `Err` is the unmatched-sample text: an event
/// name the adapter contract does not know, a notification type outside
/// the observed set, or a known event missing a field the adapter flow
/// consumes (the tailer key, the approval identity, the decision inputs).
pub fn classify_hook(payload: &HookPayload) -> Result<&'static str, String> {
    let event = payload.hook_event_name.as_str();
    match event {
        "SessionStart" => {
            require(&payload.session_id, event, "session_id")?;
            require(&payload.source, event, "source")?;
            require(&payload.transcript_path, event, "transcript_path")?;
            Ok("claude/hook-session-start")
        }
        "SessionEnd" => {
            require(&payload.reason, event, "reason")?;
            Ok("claude/hook-session-end")
        }
        "PreToolUse" => {
            require(&payload.tool_use_id, event, "tool_use_id")?;
            require(&payload.tool_name, event, "tool_name")?;
            require(&payload.tool_input, event, "tool_input")?;
            Ok("claude/hook-pre-tool-use")
        }
        "PostToolUse" => {
            require(&payload.tool_use_id, event, "tool_use_id")?;
            require(&payload.tool_name, event, "tool_name")?;
            Ok("claude/hook-post-tool-use")
        }
        "Stop" => Ok("claude/hook-stop"),
        "PreCompact" => Ok("claude/hook-pre-compact"),
        "Notification" => {
            require(&payload.notification_type, event, "notification_type")?;
            require(&payload.message, event, "message")?;
            match payload.notification_type.as_deref() {
                Some("permission_prompt") => Ok("claude/hook-notification-permission"),
                Some("idle_prompt") => Ok("claude/hook-notification-idle"),
                Some(other) => Err(format!("hook:Notification/{other}")),
                None => unreachable!("presence checked above"),
            }
        }
        other => Err(format!("hook:{other}")),
    }
}

/// Classify one transcript record into its emissions: one per content block
/// for message records, one for the record itself otherwise. `Err` entries
/// are unmatched-sample texts for shapes the tailer would not know how to
/// map.
pub fn classify_record(record: &TranscriptRecord) -> Vec<Result<&'static str, String>> {
    let record_type = record.record_type.as_str();
    if SETUP_RECORD_TYPES.contains(&record_type) {
        return vec![Ok("claude/transcript-setup-record")];
    }
    match record_type {
        "system" => vec![Ok("claude/transcript-system-record")],
        "user" | "assistant" => {
            let Some(message) = &record.message else {
                return vec![Err(format!("transcript:{record_type}#missing-message"))];
            };
            match &message.content {
                TranscriptContent::Text(_) => vec![Ok(match record_type {
                    "user" => "claude/transcript-user-prompt",
                    _ => "claude/transcript-assistant-text",
                })],
                TranscriptContent::Blocks(blocks) if blocks.is_empty() => {
                    vec![Err(format!("transcript:{record_type}#empty-content"))]
                }
                TranscriptContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match (record_type, block.block_type.as_str()) {
                        ("user", "text") => Ok("claude/transcript-user-prompt"),
                        ("assistant", "text") => Ok("claude/transcript-assistant-text"),
                        ("assistant", "thinking") => Ok("claude/transcript-assistant-thinking"),
                        ("assistant", "tool_use") => Ok("claude/transcript-tool-use"),
                        ("user", "tool_result") => Ok("claude/transcript-tool-result"),
                        (_, other) => Err(format!("transcript:{record_type}/{other}")),
                    })
                    .collect(),
            }
        }
        other => vec![Err(format!("transcript:{other}"))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(event: &str) -> HookPayload {
        HookPayload {
            hook_event_name: event.to_string(),
            ..HookPayload::default()
        }
    }

    fn full_payload(event: &str) -> HookPayload {
        HookPayload {
            session_id: Some("95ba41ef".to_string()),
            transcript_path: Some("/tmp/95ba41ef.jsonl".to_string()),
            source: Some("startup".to_string()),
            reason: Some("exit".to_string()),
            tool_name: Some("Bash".to_string()),
            tool_use_id: Some("toolu_01".to_string()),
            tool_input: Some(serde_json::json!({"command": "true"})),
            notification_type: Some("permission_prompt".to_string()),
            message: Some("Claude needs your permission".to_string()),
            ..payload(event)
        }
    }

    #[test]
    fn every_recorded_hook_event_classifies() {
        let cases = [
            ("SessionStart", "claude/hook-session-start"),
            ("SessionEnd", "claude/hook-session-end"),
            ("PreToolUse", "claude/hook-pre-tool-use"),
            ("PostToolUse", "claude/hook-post-tool-use"),
            ("Stop", "claude/hook-stop"),
            ("PreCompact", "claude/hook-pre-compact"),
            ("Notification", "claude/hook-notification-permission"),
        ];
        for (event, id) in cases {
            assert_eq!(classify_hook(&full_payload(event)), Ok(id));
        }
    }

    #[test]
    fn notification_types_split_and_unknown_ones_are_drift() {
        let mut idle = full_payload("Notification");
        idle.notification_type = Some("idle_prompt".to_string());
        assert_eq!(classify_hook(&idle), Ok("claude/hook-notification-idle"));

        let mut other = full_payload("Notification");
        other.notification_type = Some("auth_prompt".to_string());
        assert_eq!(
            classify_hook(&other).unwrap_err(),
            "hook:Notification/auth_prompt"
        );
    }

    #[test]
    fn an_unknown_event_name_is_an_unrecognized_sample() {
        assert_eq!(
            classify_hook(&full_payload("PostToolUseFailure")).unwrap_err(),
            "hook:PostToolUseFailure"
        );
    }

    #[test]
    fn a_known_event_missing_a_consumed_field_is_drift_not_a_pass() {
        let mut start = full_payload("SessionStart");
        start.transcript_path = None;
        assert_eq!(
            classify_hook(&start).unwrap_err(),
            "hook:SessionStart#missing-transcript_path"
        );

        let mut pre = full_payload("PreToolUse");
        pre.tool_use_id = None;
        assert_eq!(
            classify_hook(&pre).unwrap_err(),
            "hook:PreToolUse#missing-tool_use_id"
        );

        // Stop consumes nothing beyond its name; the bare event passes.
        assert_eq!(classify_hook(&payload("Stop")), Ok("claude/hook-stop"));
    }

    fn record(json: serde_json::Value) -> TranscriptRecord {
        serde_json::from_value(json).expect("test record parses")
    }

    #[test]
    fn every_recorded_record_type_classifies() {
        for setup in SETUP_RECORD_TYPES {
            assert_eq!(
                classify_record(&record(serde_json::json!({"type": setup}))),
                [Ok("claude/transcript-setup-record")]
            );
        }
        assert_eq!(
            classify_record(&record(serde_json::json!({"type": "system"}))),
            [Ok("claude/transcript-system-record")]
        );
    }

    #[test]
    fn message_records_classify_per_block() {
        let assistant = record(serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "thinking", "thinking": "…"},
                {"type": "tool_use", "id": "toolu_01", "name": "Read", "input": {}},
                {"type": "text", "text": "done"},
            ]},
        }));
        assert_eq!(
            classify_record(&assistant),
            [
                Ok("claude/transcript-assistant-thinking"),
                Ok("claude/transcript-tool-use"),
                Ok("claude/transcript-assistant-text"),
            ]
        );

        let result = record(serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "ok"},
            ]},
        }));
        assert_eq!(
            classify_record(&result),
            [Ok("claude/transcript-tool-result")]
        );
    }

    #[test]
    fn string_content_is_one_prompt_emission() {
        let typed = record(serde_json::json!({
            "type": "user",
            "message": {"content": "Reply with exactly: ok"},
        }));
        assert_eq!(
            classify_record(&typed),
            [Ok("claude/transcript-user-prompt")]
        );
    }

    #[test]
    fn the_interrupt_marker_user_record_classifies_as_prompt_content() {
        // The interrupt scenario writes a user record whose list content
        // holds a text block — user-side content, not a tool result.
        let marker = record(serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]},
        }));
        assert_eq!(
            classify_record(&marker),
            [Ok("claude/transcript-user-prompt")]
        );
    }

    #[test]
    fn unknown_record_and_block_types_are_unrecognized_samples() {
        assert_eq!(
            classify_record(&record(serde_json::json!({"type": "queued-command"}))),
            [Err("transcript:queued-command".to_string())]
        );
        let odd_block = record(serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "server_tool_use"}]},
        }));
        assert_eq!(
            classify_record(&odd_block),
            [Err("transcript:assistant/server_tool_use".to_string())]
        );
        let bare = record(serde_json::json!({"type": "assistant"}));
        assert_eq!(
            classify_record(&bare),
            [Err("transcript:assistant#missing-message".to_string())]
        );
        let empty = record(serde_json::json!({
            "type": "assistant",
            "message": {"content": []},
        }));
        assert_eq!(
            classify_record(&empty),
            [Err("transcript:assistant#empty-content".to_string())]
        );
    }

    #[test]
    fn every_classification_target_is_in_the_table() {
        // classify_* return ids the report keys rows by; an id outside the
        // table would silently drop from the accounting.
        let known: Vec<&str> = CHANNEL_CLASSIFIERS.iter().map(|spec| spec.id).collect();
        let mut targets = vec![
            classify_hook(&full_payload("SessionStart")).unwrap(),
            classify_hook(&full_payload("SessionEnd")).unwrap(),
            classify_hook(&full_payload("PreToolUse")).unwrap(),
            classify_hook(&full_payload("PostToolUse")).unwrap(),
            classify_hook(&full_payload("Stop")).unwrap(),
            classify_hook(&full_payload("PreCompact")).unwrap(),
            classify_hook(&full_payload("Notification")).unwrap(),
        ];
        targets.extend(
            classify_record(&record(serde_json::json!({"type": "mode"})))
                .into_iter()
                .map(Result::unwrap),
        );
        for id in targets {
            assert!(
                known.contains(&id),
                "{id}: classifier missing from the table"
            );
        }
    }
}
