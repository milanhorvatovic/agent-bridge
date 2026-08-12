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

use agent_bridge_adapter_api::{EmitSpec, Template, TemplateValue};

/// Whether a field takes one value or a list of them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arity {
    Scalar,
    List,
}

/// One field an event type defines: its name, its arity, and whether a
/// record may omit it.
struct FieldRule {
    name: &'static str,
    arity: Arity,
    required: bool,
}

const fn required(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        required: true,
    }
}

const fn optional(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::Scalar,
        required: false,
    }
}

const fn optional_list(name: &'static str) -> FieldRule {
    FieldRule {
        name,
        arity: Arity::List,
        required: false,
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
            required("approval_id"),
            required("prompt"),
            optional("tool"),
            optional_list("options"),
        ],
    ),
    (
        "tool.call_started",
        &[required("call_id"), required("tool"), optional("command")],
    ),
    (
        "tool.call_completed",
        &[
            required("call_id"),
            optional("exit_code"),
            optional("duration_ms"),
        ],
    ),
    (
        "tool.call_failed",
        &[required("call_id"), required("reason")],
    ),
    ("tool.result", &[required("call_id"), required("content")]),
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
    }
    Ok(())
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
