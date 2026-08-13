//! The emit mapping: which events a pattern record may construct, and what
//! its field templates must provide.
//!
//! The vocabulary is closed on both axes. A record emits one of the event
//! types listed here — the ones a text pattern can honestly produce — and
//! fills only the fields that event defines, with a template vocabulary of
//! exactly `uuid4()`, `matches.<group>`, and verbatim literals. Everything
//! else fails the pack at load, because a pack that validates is a pack
//! whose every record could construct its event; match time is too late to
//! find out otherwise.

use agent_bridge_adapter_api::{Captures, EmitSpec, Template, TemplateValue};
use agent_bridge_events::{
    ApprovalPrompt, EventBody, EventKind, ToolCallCompleted, ToolCallFailed, ToolCallStarted,
    ToolResult,
};

/// Whether a field takes one value or a list of them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arity {
    Scalar,
    List,
}

/// What a scalar field's rendered value must be — against the payload
/// field's *actual* type, because "a number" is not one range: an exit
/// code is a signed 32-bit value and a duration is an unsigned 64-bit
/// one, and validating both against some third range would pass literals
/// the renderer must then drop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    Text,
    /// A literal must parse as `i32` at load; `uuid4()` can never be a
    /// number and is rejected outright; a capture group is the one case
    /// load time cannot decide, so it keeps the runtime drop-and-warn
    /// fallback.
    Int32,
    /// As above, against `u64`.
    Uint64,
}

/// One field an event type defines: its name, its shape, and whether a
/// record may omit it.
struct FieldRule {
    name: &'static str,
    arity: Arity,
    value: Value,
    required: bool,
    /// What the field's identity discipline is, if it names an id.
    id: IdRule,
}

/// How an identifier field may be filled. Ids exist to be distinct or to
/// pair, and a template can be wrong for either job at load time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdRule {
    /// Not an identifier: any template.
    Any,
    /// A fresh identity: minted (`uuid4()`) or carried from the matcher,
    /// never a compile-time literal — a fixed id names every match at
    /// once, which is to say none of them.
    MintOrCarry,
    /// A closing reference: carried only. A generated id pairs with
    /// nothing and a fixed one can only pretend to.
    CarryOnly,
}

const fn required(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Text,
        required: true,
        id: IdRule::Any,
    }
}

const fn required_identity(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Text,
        required: true,
        id: IdRule::MintOrCarry,
    }
}

const fn required_correlation(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Text,
        required: true,
        id: IdRule::CarryOnly,
    }
}

const fn optional(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Text,
        required: false,
        id: IdRule::Any,
    }
}

const fn optional_int32(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Int32,
        required: false,
        id: IdRule::Any,
    }
}

const fn optional_uint64(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        value: Value::Uint64,
        required: false,
        id: IdRule::Any,
    }
}

const fn optional_list(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::List,
        value: Value::Text,
        required: false,
        id: IdRule::Any,
    }
}

/// The event types a pattern record may emit, each with its field rules.
///
/// Deliberately a subset of the published taxonomy: lifecycle transitions,
/// stream tokens, and error events belong to the layers that own them, and a
/// pack that could emit them would let a pattern impersonate the runtime.
/// The approval prompt's `approval_id` is listed as a field here even though
/// it rides the envelope, because the record's author has to say where the
/// id comes from — in practice always `{{ uuid4() }}`.
const EMITTABLE: &[(&str, &[FieldRule])] = &[
    (
        "prompt.approval_required",
        &[
            required_identity("approval_id"),
            required("prompt"),
            optional("tool"),
            optional_list("options"),
        ],
    ),
    (
        "tool.call_started",
        &[
            required_identity("call_id"),
            required("tool"),
            optional("command"),
        ],
    ),
    (
        "tool.call_completed",
        &[
            required_correlation("call_id"),
            optional_int32("exit_code"),
            optional_uint64("duration_ms"),
        ],
    ),
    (
        "tool.call_failed",
        &[required_correlation("call_id"), required("reason")],
    ),
    (
        "tool.result",
        &[required_correlation("call_id"), required("content")],
    ),
];

/// Holds an emit spec to the table above. The error names the offending
/// field or type; the caller attaches the record name.
pub(crate) fn validate_emit_spec(spec: &EmitSpec) -> Result<(), String> {
    let Some((_, rules)) = EMITTABLE
        .iter()
        .find(|(event_type, _)| *event_type == spec.event_type)
    else {
        let known = EMITTABLE
            .iter()
            .map(|(event_type, _)| *event_type)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "a pattern record cannot emit `{}` — it can emit: {known}",
            spec.event_type
        ));
    };
    for rule in rules.iter().filter(|rule| rule.required) {
        if !spec.fields.contains_key(rule.name) {
            return Err(format!(
                "`{}` requires the `{}` field",
                spec.event_type, rule.name
            ));
        }
    }
    for (name, value) in &spec.fields {
        let Some(rule) = rules.iter().find(|rule| rule.name == name.as_str()) else {
            return Err(format!("`{}` has no `{name}` field", spec.event_type));
        };
        if rule.required
            && let TemplateValue::One(Template::Literal(text)) = value
            && text.is_empty()
        {
            return Err(format!(
                "`{}.{name}` is required and an empty literal does not fill it",
                spec.event_type
            ));
        }
        let arity_holds = match rule.arity {
            Arity::Scalar => matches!(value, TemplateValue::One(_)),
            Arity::List => matches!(value, TemplateValue::Many(_)),
        };
        if !arity_holds {
            let expected = match rule.arity {
                Arity::Scalar => "one value",
                Arity::List => "a list",
            };
            return Err(format!("`{}.{name}` takes {expected}", spec.event_type));
        }
        match rule.id {
            IdRule::Any => {}
            IdRule::MintOrCarry => {
                if let TemplateValue::One(Template::Literal(_)) = value {
                    return Err(format!(
                        "`{}.{name}` identifies one specific match and must be `uuid4()` or \
                         a carried `matches.<group>` — a fixed id names every match at once",
                        spec.event_type
                    ));
                }
            }
            IdRule::CarryOnly => {
                if let TemplateValue::One(template) = value
                    && !matches!(template, Template::Group(_))
                {
                    return Err(format!(
                        "`{}.{name}` closes an earlier event and must carry its id through \
                         `matches.<group>` — a generated or fixed value can pair with nothing",
                        spec.event_type
                    ));
                }
            }
        }
        if rule.value != Value::Text
            && let TemplateValue::One(template) = value
        {
            let fits = |text: &str| match rule.value {
                Value::Int32 => text.trim().parse::<i32>().is_ok(),
                Value::Uint64 => text.trim().parse::<u64>().is_ok(),
                Value::Text => true,
            };
            match template {
                Template::Literal(text) if !fits(text) => {
                    return Err(format!("`{}.{name}` cannot hold `{text}`", spec.event_type));
                }
                Template::Uuid4 => {
                    return Err(format!(
                        "`{}.{name}` is a number and `uuid4()` can never render one",
                        spec.event_type
                    ));
                }
                // A capture group is decided at match time; the renderer
                // drops an unparseable value and says so in the log.
                Template::Literal(_) | Template::Group(_) => {}
            }
        }
    }
    Ok(())
}

/// The capture groups an emit spec's *required* fields read — the ones a
/// match must actually have filled, non-empty, for the event to be worth
/// emitting. A required field rendering empty is an unanswerable approval
/// or an id that names nothing, so the engine vetoes the match instead
/// and lets the next candidate — or the degradation path — speak.
pub(crate) fn required_groups(spec: &EmitSpec) -> Vec<String> {
    let Some((_, rules)) = EMITTABLE
        .iter()
        .find(|(event_type, _)| *event_type == spec.event_type)
    else {
        return Vec::new();
    };
    spec.fields
        .iter()
        .filter(|(name, _)| {
            rules
                .iter()
                .any(|rule| rule.name == name.as_str() && rule.required)
        })
        .filter_map(|(_, value)| match value {
            TemplateValue::One(Template::Group(group)) => Some(group.clone()),
            _ => None,
        })
        .collect()
}

/// The capture groups an emit spec reads — what the matcher must actually
/// capture for the record to construct its event.
pub(crate) fn groups_read(spec: &EmitSpec) -> impl Iterator<Item = &str> {
    spec.fields
        .values()
        .flat_map(|value| match value {
            TemplateValue::One(template) => std::slice::from_ref(template),
            TemplateValue::Many(templates) => templates.as_slice(),
        })
        .filter_map(|template| match template {
            Template::Group(group) => Some(group.as_str()),
            Template::Uuid4 | Template::Literal(_) => None,
        })
}

// ---------------------------------------------------------------------------
// Match time: a validated spec plus a match's captures becomes an event.
// ---------------------------------------------------------------------------

/// Constructs the event a winning match emits.
///
/// Infallible by construction: every spec that reaches here passed
/// [`validate_emit_spec`] at load, and the engine's pre-emission check
/// ([`required_groups`]) vetoed any match whose required captures are
/// absent or empty — so an unfilled group reaching this point belongs to
/// an *optional* field, and renders as the empty string. The two
/// numeric tool-call fields are the one soft spot: a template can render
/// text a number field cannot hold, which no load-time check can rule out
/// once a group is involved, so an unparseable value drops the optional
/// field and says so in the log rather than dropping the event.
pub(crate) fn render_event(spec: &EmitSpec, captures: &Captures) -> EventBody {
    let scalar = |name: &str| -> Option<String> {
        match spec.fields.get(name) {
            Some(TemplateValue::One(template)) => render(template, captures),
            _ => None,
        }
    };
    let required = |name: &str| -> String {
        scalar(name).expect("validated at load and vetoed at match: required fields are filled")
    };
    match spec.event_type.as_str() {
        "prompt.approval_required" => {
            let mut prompt = ApprovalPrompt::new(required("prompt"));
            if let Some(tool) = scalar("tool") {
                prompt = prompt.tool(tool);
            }
            if let Some(TemplateValue::Many(templates)) = spec.fields.get("options") {
                prompt = prompt.options(
                    templates
                        .iter()
                        .map(|template| render(template, captures).unwrap_or_default()),
                );
            }
            EventBody::approval_required(required("approval_id"), prompt)
        }
        "tool.call_started" => EventBody::new(EventKind::ToolCallStarted(ToolCallStarted {
            call_id: required("call_id"),
            tool: required("tool"),
            command: scalar("command"),
        })),
        "tool.call_completed" => EventBody::new(EventKind::ToolCallCompleted(ToolCallCompleted {
            call_id: required("call_id"),
            exit_code: numeric(spec, "exit_code", scalar("exit_code")),
            duration_ms: numeric(spec, "duration_ms", scalar("duration_ms")),
        })),
        "tool.call_failed" => EventBody::new(EventKind::ToolCallFailed(ToolCallFailed {
            call_id: required("call_id"),
            reason: required("reason"),
        })),
        "tool.result" => EventBody::new(EventKind::ToolResult(ToolResult {
            call_id: required("call_id"),
            content: required("content"),
        })),
        other => unreachable!("validated at load: `{other}` is not an emittable type"),
    }
}

/// One rendered field that must parse as a number, or nothing plus a log
/// line saying which field of which event dropped.
fn numeric<N: std::str::FromStr>(
    spec: &EmitSpec,
    name: &str,
    rendered: Option<String>,
) -> Option<N> {
    let rendered = rendered?;
    match rendered.trim().parse() {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(
                event_type = spec.event_type.as_str(),
                field = name,
                "emit template rendered a non-numeric value; dropping the field"
            );
            None
        }
    }
}

/// `None` means the capture the template reads is absent — which for an
/// optional field is the honest rendering: the payload's `Option` exists
/// to say "not supplied", and `Some("")` would erase the difference
/// between absent and explicitly empty. Required fields never see the
/// `None`: the engine vetoed that match before rendering.
fn render(template: &Template, captures: &Captures) -> Option<String> {
    match template {
        Template::Uuid4 => Some(uuid::Uuid::new_v4().to_string()),
        Template::Group(group) => captures.get(group).map(str::to_string),
        Template::Literal(text) => Some(text.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_adapter_api::PatternRecord;

    fn emits_of(yaml: &str) -> EmitSpec {
        let records: Vec<PatternRecord> = serde_norway::from_str(yaml).expect("well-formed record");
        records.into_iter().next().expect("one record").emits
    }

    fn approval_record(fields: &str) -> EmitSpec {
        emits_of(&format!(
            r#"
- name: probe
  matcher: {{ type: regex, source: 'x' }}
  emits:
    event_type: prompt.approval_required
    fields:
{fields}
"#
        ))
    }

    #[test]
    fn the_approval_shape_validates() {
        let spec = approval_record(
            "      approval_id: '{{ uuid4() }}'\n      prompt: '{{ matches.prompt }}'\n      \
             tool: bash\n      options: ['y', 'n']",
        );
        validate_emit_spec(&spec).expect("the canonical approval record");
        assert_eq!(groups_read(&spec).collect::<Vec<_>>(), vec!["prompt"]);
    }

    #[test]
    fn unlisted_event_types_are_rejected_naming_the_alternatives() {
        let spec = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: 'x' }
  emits:
    event_type: lifecycle.session.closed
"#,
        );
        let error = validate_emit_spec(&spec).expect_err("a pack must not emit lifecycle");
        assert!(error.contains("lifecycle.session.closed"));
        assert!(error.contains("prompt.approval_required"));
    }

    #[test]
    fn missing_required_fields_are_rejected_by_name() {
        let spec = approval_record("      approval_id: '{{ uuid4() }}'");
        let error = validate_emit_spec(&spec).expect_err("prompt is required");
        assert!(error.contains("`prompt`"), "got: {error}");
    }

    #[test]
    fn fields_the_event_does_not_define_are_rejected() {
        let spec = approval_record(
            "      approval_id: '{{ uuid4() }}'\n      prompt: p\n      severity: high",
        );
        let error = validate_emit_spec(&spec).expect_err("severity is not a field");
        assert!(error.contains("severity"), "got: {error}");
    }

    #[test]
    fn rendering_the_approval_record_builds_the_sealed_prompt() {
        let spec = approval_record(
            "      approval_id: '{{ uuid4() }}'\n      prompt: '{{ matches.prompt }}'\n      \
             tool: bash\n      options: ['y', 'n']",
        );
        let captures = Captures::new().with("prompt", "Allow filesystem write?");
        let body = render_event(&spec, &captures);
        let approval_id = body.approval_id.expect("the envelope id is set");
        uuid::Uuid::parse_str(&approval_id).expect("uuid4() renders a v4 uuid");
        let EventKind::PromptApprovalRequired(payload) = body.kind else {
            panic!("wrong kind: {:?}", body.kind);
        };
        assert_eq!(payload.prompt, "Allow filesystem write?");
        assert_eq!(payload.tool.as_deref(), Some("bash"));
        assert_eq!(
            payload.options,
            Some(vec!["y".to_string(), "n".to_string()])
        );
    }

    #[test]
    fn each_match_renders_a_fresh_uuid() {
        let spec = approval_record("      approval_id: '{{ uuid4() }}'\n      prompt: p");
        let captures = Captures::new();
        let first = render_event(&spec, &captures).approval_id;
        let second = render_event(&spec, &captures).approval_id;
        assert_ne!(first, second);
    }

    /// An optional field whose capture is absent is omitted, not rendered
    /// empty: the payload's `Option` exists to say "not supplied", and
    /// `Some("")` would erase the difference. A required field never
    /// reaches rendering unfilled — the engine vetoes that match first.
    #[test]
    fn an_unfilled_optional_field_is_omitted() {
        let spec = approval_record(
            "      approval_id: '{{ uuid4() }}'\n      prompt: a prompt\n      \
             tool: '{{ matches.absent }}'",
        );
        let body = render_event(&spec, &Captures::new());
        let EventKind::PromptApprovalRequired(payload) = body.kind else {
            panic!("wrong kind");
        };
        assert_eq!(payload.tool, None, "absent capture, omitted field");

        // A capture that matched *empty* is supplied, and stays.
        let explicit = render_event(&spec, &Captures::new().with("absent", ""));
        let EventKind::PromptApprovalRequired(payload) = explicit.kind else {
            panic!("wrong kind");
        };
        assert_eq!(payload.tool.as_deref(), Some(""));
    }

    /// A required field filled with an empty literal is a spec that could
    /// only ever emit hollow events, and fails the pack at load.
    #[test]
    fn empty_required_literals_fail_the_pack() {
        let spec = approval_record("      approval_id: '{{ uuid4() }}'\n      prompt: ''");
        let error = validate_emit_spec(&spec).expect_err("an empty prompt fills nothing");
        assert!(error.contains("prompt"), "got: {error}");

        let empty_reason = emits_of(
            "- name: probe\n  matcher: { type: regex, source: '(?P<id>.+)' }\n  emits:\n    \
             event_type: tool.call_failed\n    fields:\n      call_id: '{{ matches.id }}'\n      \
             reason: ''\n",
        );
        let error = validate_emit_spec(&empty_reason).expect_err("an empty reason says nothing");
        assert!(error.contains("reason"), "got: {error}");
    }

    /// An identifier names one specific match: minted or carried, never a
    /// compile-time literal that names every match at once.
    #[test]
    fn identity_ids_reject_fixed_literals() {
        let fixed_approval = approval_record("      approval_id: fixed\n      prompt: a prompt");
        let error = validate_emit_spec(&fixed_approval).expect_err("a fixed approval id");
        assert!(error.contains("approval_id"), "got: {error}");

        let fixed_start = emits_of(
            "- name: probe\n  matcher: { type: regex, source: 'x' }\n  emits:\n    \
             event_type: tool.call_started\n    fields:\n      call_id: fixed\n      tool: t\n",
        );
        let error = validate_emit_spec(&fixed_start).expect_err("a fixed start id");
        assert!(error.contains("call_id"), "got: {error}");

        let carried_start = emits_of(
            "- name: probe\n  matcher: { type: regex, source: '(?P<id>.+)' }\n  emits:\n    \
             event_type: tool.call_started\n    fields:\n      call_id: '{{ matches.id }}'\n      \
             tool: t\n",
        );
        validate_emit_spec(&carried_start).expect("minted and carried start ids both load");
    }

    #[test]
    fn a_non_numeric_rendering_drops_the_optional_field_not_the_event() {
        let spec = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: '(?P<code>.*)' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      exit_code: '{{ matches.code }}'
"#,
        );
        let numeric = render_event(&spec, &Captures::new().with("code", "0"));
        let EventKind::ToolCallCompleted(payload) = numeric.kind else {
            panic!("wrong kind");
        };
        assert_eq!(payload.exit_code, Some(0));

        let textual = render_event(&spec, &Captures::new().with("code", "whoops"));
        let EventKind::ToolCallCompleted(payload) = textual.kind else {
            panic!("wrong kind");
        };
        assert_eq!(payload.exit_code, None, "the field drops, the event stays");
    }

    #[test]
    fn numeric_fields_reject_impossible_templates_at_load() {
        let bad_literal = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: 'x' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      duration_ms: nope
"#,
        );
        let error = validate_emit_spec(&bad_literal).expect_err("`nope` is not a number");
        assert!(error.contains("duration_ms"), "got: {error}");

        let uuid_number = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: 'x' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      exit_code: '{{ uuid4() }}'
"#,
        );
        let error = validate_emit_spec(&uuid_number).expect_err("a uuid is never a number");
        assert!(error.contains("exit_code"), "got: {error}");

        // Each numeric field is held to its own payload type: a duration
        // is unsigned, an exit code is 32-bit signed.
        let negative_duration = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: 'x' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      duration_ms: '-1'
"#,
        );
        let error = validate_emit_spec(&negative_duration).expect_err("a duration is unsigned");
        assert!(error.contains("duration_ms"), "got: {error}");

        // A numeric literal in range, a negative exit code (signed by
        // contract), a duration beyond i64, and a capture group all load:
        // three are checked here, the group is the renderer's runtime call.
        let fine = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: '(?P<code>.*)' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      exit_code: '-13'
      duration_ms: '{{ matches.code }}'
"#,
        );
        validate_emit_spec(&fine).expect("signed exit code and group both load");
        let huge_duration = emits_of(
            r#"
- name: probe
  matcher: { type: regex, source: 'x' }
  emits:
    event_type: tool.call_completed
    fields:
      call_id: '{{ matches.code }}'
      duration_ms: '18446744073709551615'
"#,
        );
        validate_emit_spec(&huge_duration).expect("u64::MAX is a valid duration");
    }

    /// A closing lifecycle event correlates to its start, so its id must
    /// be carried through a capture: a generated id pairs with nothing
    /// and a fixed one can only pretend to.
    #[test]
    fn closing_lifecycle_ids_must_be_carried_captures() {
        for (event_type, extra) in [
            ("tool.call_completed", ""),
            ("tool.call_failed", "\n      reason: because"),
            ("tool.result", "\n      content: text"),
        ] {
            let generated = emits_of(&format!(
                "- name: probe\n  matcher: {{ type: regex, source: 'x' }}\n  emits:\n    \
                 event_type: {event_type}\n    fields:\n      call_id: '{{{{ uuid4() }}}}'{extra}\n",
            ));
            let error = validate_emit_spec(&generated)
                .expect_err("a generated closing id pairs with nothing");
            assert!(error.contains("call_id"), "got: {error}");

            let fixed = emits_of(&format!(
                "- name: probe\n  matcher: {{ type: regex, source: 'x' }}\n  emits:\n    \
                 event_type: {event_type}\n    fields:\n      call_id: fixed{extra}\n",
            ));
            assert!(
                validate_emit_spec(&fixed).is_err(),
                "a literal id can only pretend"
            );

            let carried = emits_of(&format!(
                "- name: probe\n  matcher: {{ type: regex, source: 'x' }}\n  emits:\n    \
                 event_type: {event_type}\n    fields:\n      call_id: '{{{{ matches.id }}}}'{extra}\n",
            ));
            validate_emit_spec(&carried).expect("a carried id is the pairable one");
        }
    }

    #[test]
    fn arity_is_enforced_both_ways() {
        let listy_prompt =
            approval_record("      approval_id: '{{ uuid4() }}'\n      prompt: ['a', 'b']");
        assert!(validate_emit_spec(&listy_prompt).is_err());

        let scalar_options = approval_record(
            "      approval_id: '{{ uuid4() }}'\n      prompt: p\n      options: y",
        );
        assert!(validate_emit_spec(&scalar_options).is_err());
    }
}
