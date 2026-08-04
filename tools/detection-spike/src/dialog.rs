//! Menu-dialog detection over the materialized screen.
//!
//! A menu-style dialog is a *region*, not a line: a title somewhere in the
//! viewport with numbered options painted below it, one of them carrying a
//! selection caret. Line matchers can vote on the individual rows, but only
//! a detector that reads the region can say "a permission dialog is open,
//! these are its options, this one is selected" — which is exactly what the
//! runtime needs from a screen-side matcher, and what this prototype is
//! measured on. Its record shape — a title anchor plus positional field
//! extraction below it — is the concrete input to the screen matcher record
//! format the spike's report proposes.
//!
//! Detection is a pure function over the viewport rows of one evaluation
//! point. Appearance tracking across evaluation points (the same dialog
//! sitting open through several quiet periods is one dialog, not several)
//! belongs to the caller, which sees the sequence of screens.

use crate::patterns::{Cli, Role};

/// How many rows below the title anchor are scanned for options. Codex
/// paints a command preview between its approval question and the options,
/// so the window must span more than the options themselves; a full dialog
/// fits comfortably inside this many rows at both recorded sizes.
const OPTION_SCAN_ROWS: usize = 12;

/// One dialog the detector knows how to find: a literal title anchor plus
/// the accounting identity its detections are reported under. The option
/// grammar below is shared by every spec — what varies per dialog is only
/// the anchor.
#[derive(Debug)]
pub struct DialogSpec {
    /// Stable identifier, `<cli>/screen-dialog-<name>` — the key in every
    /// report row, beside the line-pattern ids.
    pub id: &'static str,
    pub cli: Cli,
    /// Pipeline-local classification, same vocabulary as the line patterns.
    pub class: &'static str,
    pub role: Role,
    /// Literal the title row must contain. The screen renders spaced text,
    /// so these are the phrasings a human reads, not the cursor-mashed
    /// variants the stripped byte stream carries.
    pub title: &'static str,
}

/// The dialogs the corpus scenarios actually open. Titles were read from
/// the rendered screens of the tuned versions (claude 2.1.201, codex
/// 0.145.0), like the line-pattern needles.
pub const DIALOGS: &[DialogSpec] = &[
    // The permission dialog the approval scenarios answer; the ground-truth
    // step log records one permission notification per opening.
    DialogSpec {
        id: "claude/screen-dialog-permission",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        title: "Do you want to proceed?",
    },
    // The startup trust screen: a full-viewport paint whose question is the
    // "Quick safety check" paragraph, with the yes/no menu several rows
    // below it. It paints in every claude fixture but the driver never
    // waits on it, so there is no per-event ground truth — ambient, like
    // the trust-option line pattern. On exit the TUI replays the region
    // with only the confirmed option, which the two-option menu grammar
    // correctly rejects.
    DialogSpec {
        id: "claude/screen-dialog-trust",
        cli: Cli::Claude,
        class: "dialog.trust",
        role: Role::Ambient,
        title: "Quick safety check",
    },
    // Codex opens every session on the workspace-trust prompt and the
    // driver waits for it, so it is anchored.
    DialogSpec {
        id: "codex/screen-dialog-trust",
        cli: Cli::Codex,
        class: "dialog.trust",
        role: Role::Anchored,
        title: "Do you trust the contents",
    },
    DialogSpec {
        id: "codex/screen-dialog-approval",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        title: "Would you like to run the following command?",
    },
];

/// The specs one CLI's replay evaluates.
pub fn for_cli(cli: Cli) -> Vec<&'static DialogSpec> {
    DIALOGS.iter().filter(|spec| spec.cli == cli).collect()
}

/// One numbered option extracted from a dialog region.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct DialogOption {
    pub number: u32,
    pub label: String,
    pub selected: bool,
}

/// One dialog found open on one screen.
#[derive(Debug)]
pub struct Detection {
    pub spec: &'static DialogSpec,
    /// Viewport row index of the title anchor.
    pub title_row: usize,
    pub options: Vec<DialogOption>,
}

/// Find every known dialog open on this screen. A title without a menu
/// below it is not a detection: the same words in scrollback, a response
/// quoting the question, or a confirmation echo after answering must not
/// read as an open dialog.
pub fn detect(specs: &[&'static DialogSpec], rows: &[String]) -> Vec<Detection> {
    let mut detections = Vec::new();
    for spec in specs {
        let Some(title_row) = rows.iter().position(|row| row.contains(spec.title)) else {
            continue;
        };
        let window_end = rows.len().min(title_row + 1 + OPTION_SCAN_ROWS);
        let options: Vec<DialogOption> = rows[title_row + 1..window_end]
            .iter()
            .filter_map(|row| parse_option(row))
            .collect();
        if is_menu(&options) {
            detections.push(Detection {
                spec,
                title_row,
                options,
            });
        }
    }
    detections
}

/// A menu is at least two options with strictly increasing numbers — the
/// increasing check keeps a stray numbered list elsewhere in the window
/// from completing a half-visible dialog.
fn is_menu(options: &[DialogOption]) -> bool {
    options.len() >= 2
        && options
            .windows(2)
            .all(|pair| pair[0].number < pair[1].number)
}

/// Parse one viewport row as a numbered dialog option: an optional
/// selection caret (`❯` claude, `›` codex), a number, a dot, a label.
/// Box-border cells around the row are part of the paint, not the option.
fn parse_option(row: &str) -> Option<DialogOption> {
    let trimmed = row.trim_matches(|c: char| c == '│' || c.is_whitespace());
    let (selected, rest) = match trimmed.strip_prefix(['❯', '›']) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let number: u32 = rest[..digits_end].parse().ok()?;
    let label = rest[digits_end..]
        .strip_prefix(". ")?
        .trim_end()
        .to_string();
    if label.is_empty() {
        return None;
    }
    Some(DialogOption {
        number,
        label,
        selected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    fn claude_specs() -> Vec<&'static DialogSpec> {
        for_cli(Cli::Claude)
    }

    fn codex_specs() -> Vec<&'static DialogSpec> {
        for_cli(Cli::Codex)
    }

    #[test]
    fn permission_dialog_is_detected_with_options_and_selection() {
        let rows = screen(&[
            "⏺ Bash(echo lifecycle-test)",
            "",
            "│ Do you want to proceed?                       │",
            "│ ❯ 1. Yes                                      │",
            "│   2. No, and tell Claude what to do (esc)     │",
            "",
        ]);
        let detections = detect(&claude_specs(), &rows);

        assert_eq!(detections.len(), 1);
        let dialog = &detections[0];
        assert_eq!(dialog.spec.id, "claude/screen-dialog-permission");
        assert_eq!(dialog.title_row, 2);
        assert_eq!(
            dialog.options,
            [
                DialogOption {
                    number: 1,
                    label: "Yes".to_string(),
                    selected: true,
                },
                DialogOption {
                    number: 2,
                    label: "No, and tell Claude what to do (esc)".to_string(),
                    selected: false,
                },
            ]
        );
    }

    #[test]
    fn options_beyond_an_intervening_preview_are_still_collected() {
        // Codex paints the command between the question and the menu; the
        // scan window must reach past it.
        let rows = screen(&[
            "Would you like to run the following command?",
            "",
            "  touch marker.txt",
            "",
            "› 1. Yes, proceed (y)",
            "  2. Yes, and don't ask again for this command",
            "  3. No (esc)",
        ]);
        let detections = detect(&codex_specs(), &rows);

        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].spec.id, "codex/screen-dialog-approval");
        assert_eq!(detections[0].options.len(), 3);
        assert!(detections[0].options[0].selected);
        assert_eq!(detections[0].options[2].label, "No (esc)");
    }

    #[test]
    fn a_title_without_a_menu_below_is_not_a_detection() {
        // The confirmation echo after answering repeats dialog wording with
        // no options under it — an open-dialog claim here would be false.
        let rows = screen(&["Do you want to proceed?", "You chose yes.", "⏺ done"]);
        assert!(detect(&claude_specs(), &rows).is_empty());
    }

    #[test]
    fn a_numbered_list_without_a_title_is_not_a_detection() {
        let rows = screen(&["⏺ Here are the steps:", "  1. First", "  2. Second"]);
        assert!(detect(&claude_specs(), &rows).is_empty());
    }

    #[test]
    fn non_increasing_numbers_do_not_complete_a_menu() {
        // A single real option plus a stray "1." from unrelated content must
        // not add up to a two-option menu.
        let rows = screen(&[
            "Do you want to proceed?",
            "  2. No",
            "  1. unrelated list entry",
        ]);
        assert!(detect(&claude_specs(), &rows).is_empty());
    }

    #[test]
    fn option_rows_parse_carets_numbers_and_borders() {
        assert_eq!(
            parse_option("│ ❯ 1. Yes                │"),
            Some(DialogOption {
                number: 1,
                label: "Yes".to_string(),
                selected: true,
            })
        );
        assert_eq!(
            parse_option("  3. No (esc)"),
            Some(DialogOption {
                number: 3,
                label: "No (esc)".to_string(),
                selected: false,
            })
        );
        assert_eq!(parse_option("1.no space after the dot"), None);
        assert_eq!(parse_option("❯ not numbered"), None);
        assert_eq!(parse_option("42. "), None, "empty label is not an option");
        assert_eq!(parse_option(""), None);
    }

    #[test]
    fn detection_only_uses_the_cli_s_own_specs() {
        let rows = screen(&[
            "Would you like to run the following command?",
            "› 1. Yes, proceed (y)",
            "  2. No (esc)",
        ]);
        assert!(
            detect(&claude_specs(), &rows).is_empty(),
            "a codex dialog must not fire through the claude specs"
        );
    }
}
