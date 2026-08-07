//! The contradictions this repository has had to correct more than once, and
//! the phrasings they came back in.
//!
//! They live in their own file so that the drift gate can scan everything
//! else. A scanner cannot read the file that spells out what it forbids
//! without flagging it, so exactly one file is exempt — and keeping that file
//! to the definitions means the rest of the runner, including its own header,
//! is held to the same rules as the documents it checks. The header was one
//! of the places the last recurrence reached, and it was the one place the
//! gate could not have caught it.

/// Returns a description if `text` re-pairs a reserved contradiction, else `None`.
pub fn reserved_pattern_hit(text: &str) -> Option<String> {
    // The protocol this runtime will expose reconnects a client to a live
    // session via a `session.attach` request; output missed while detached is
    // reported inside the replay payload (a gap marker), never as a dedicated
    // JSON-RPC error. A recurring design error re-introduced an error code
    // (-32004) for that gap — a file pairing the two is re-importing the
    // contradiction.
    let attach_error = format!("-{}", "32004");
    if text.contains(&attach_error) && text.contains("session.attach") {
        return Some(format!("{attach_error} paired with session.attach"));
    }
    // The runtime will reconstruct a terminal screen (a "virtual terminal")
    // from the output stream so clients get render-state, not raw bytes. That
    // reconstruction belongs to the stream/event layer; a recurring design
    // error assigned it to the PTY layer (the process-hosting layer), which
    // must stay a plain byte pipe.
    let lower = text.to_lowercase();
    if (lower.contains("virtual terminal") || lower.contains("screen state"))
        && lower.contains("pty layer")
    {
        return Some("virtual-terminal / screen-state described as PTY-layer-owned".to_string());
    }
    // `cargo xtask ci` is the check sequence, not the whole of CI. That was
    // true until the supply-chain gate became a PR-tier job of its own, and
    // the claim of equality had by then been written into seven places — the
    // house rules, the contributor guide, the README, the cargo alias, the
    // crate description, this file's own header, and a pull-request checkbox
    // a contributor is asked to tick. One change falsified all seven at once,
    // which is what makes it a contract worth gating rather than remembering.
    // The honest statement, and the one worth keeping, is that every check CI
    // runs is *a* task here — not that one task is all of CI.
    // ConPTY runs a console host process per terminal, and this runtime
    // deliberately keeps it *outside* the job object that contains the child:
    // terminating the job would take the terminal down with the child and
    // lose whatever output it still held. The host is released by closing the
    // pseudo-console instead. The opposite claim — that the job covers the
    // host too — is the obvious-sounding one, it was written into the design
    // contract, the risk register, and a crate manifest, and it survived the
    // first two corrections in the third place. Matched per line, and only in
    // the affirmative: a line that says the host is *not* in the job is the
    // correction, not the contradiction.
    for line in lower.lines() {
        if !line.contains("console host") {
            continue;
        }
        let puts_it_in_the_job = line.contains("into the job")
            || line.contains("in the job")
            || (line.contains("job object") && line.contains("including"));
        if puts_it_in_the_job && !says(line, "not") && !says(line, "outside") {
            return Some(
                "the ConPTY console host described as inside the job object — it is \
                 deliberately outside it, and released by closing the pseudo-console"
                    .to_string(),
            );
        }
    }
    for line in lower.lines() {
        let names_the_command = line.contains("xtask ci");
        if (names_the_command && CI_EQUALITY_CLAIMS.iter().any(|claim| line.contains(claim)))
            || CI_EQUALITY_CLAIMS_STANDALONE
                .iter()
                .any(|claim| line.contains(claim))
        {
            return Some(
                "`cargo xtask ci` claimed to be the whole CI run — the supply-chain gate is a \
                 PR-tier job it deliberately does not include"
                    .to_string(),
            );
        }
    }
    None
}

/// Whether `word` stands on its own in `line`, rather than sitting inside a
/// longer one.
///
/// `line.contains("not")` is also true of "nothing", "cannot", "another" and
/// "notice". In a negation guard that is the dangerous direction: a line
/// asserting the contradiction, in a sentence that happens to carry one of
/// those words, is waved through by the very check meant to catch it.
/// Splitting on non-letters and comparing whole tokens is the difference
/// between a guard and a coincidence.
fn says(line: &str, word: &str) -> bool {
    line.split(|c: char| !c.is_alphanumeric())
        .any(|token| token == word)
}

/// The ways the "one command is all of CI" claim has been phrased. Matched
/// per line rather than per file on purpose: "exactly what" is ordinary
/// English that appears in this repository for unrelated reasons, and only
/// means this when it shares a line with the command it makes a claim about.
const CI_EQUALITY_CLAIMS: &[&str] = &[
    "exactly what ci runs",
    "exactly what the ci",
    "exactly what the pr tier",
    "exactly what the pr-tier",
    "identical to what ci",
    "identical to what the pr",
    "cannot diverge",
    // The claim's quieter form: not "this is all of CI" but "this is all of
    // CI bar N". It rots the same way and is harder to notice, because it
    // sounds like precision — the first correction of the loud version left
    // this one standing in six files, and it was wrong the moment it was
    // written, since the benchmark lane was already outside `ci` too.
    "bar one job",
    "the one pr-tier check",
    "less the supply-chain gate",
    "except the supply-chain gate",
];

/// The same claim in the phrasing that does not name the command, because the
/// command sat in a fenced block above it. These read as a promise about the
/// whole of CI wherever they appear, so they need no companion token — and
/// they are the form the claim survived in after the explicit phrasings were
/// corrected, which is why matching only the explicit ones was half a fix.
const CI_EQUALITY_CLAIMS_STANDALONE: &[&str] = &[
    "green locally means green in ci",
    "green locally it is green in ci",
    "green locally and green in ci",
    // The same promise made about a machine rather than about a colour. It
    // sat in a workflow header through every earlier correction, because the
    // sweeps were looking for the word "green".
    "passes on your machine it passes here",
    "it is the identical logic",
    // The README's actual layout: the claim on its own line, introducing a
    // fenced block that held the command. Nothing on that line names the
    // command, so the line-scoped rules never saw it — and the test that
    // said otherwise had quietly moved the command up onto the claim's line.
    // These name the tier outright, so they are claims wherever they appear
    // and need no companion token.
    "identical to what the pr-tier ci runs",
    "identical to what ci runs",
    "exactly what the pr-tier ci runs",
    "exactly what the pr tier runs",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Each reserved pattern, in the phrasing that actually got written down
    /// and had to be corrected. These went untested until the third pattern
    /// was added; a gate nobody tests is a grep nobody has checked.
    #[test]
    fn each_reserved_pattern_catches_its_recurrence() {
        assert!(
            reserved_pattern_hit("the gap is reported as -32004 on session.attach").is_some(),
            "the attach error-code pairing must be caught"
        );
        assert!(
            reserved_pattern_hit("the PTY layer reconstructs a virtual terminal").is_some(),
            "virtual-terminal ownership must be caught"
        );
        assert!(
            reserved_pattern_hit("`cargo xtask ci` is exactly what the PR tier runs").is_some(),
            "the one-command-is-all-of-CI claim must be caught"
        );
        assert!(
            reserved_pattern_hit(
                "snapshot calls that bind the ConPTY console host into the job with the child"
            )
            .is_some(),
            "the console-host-inside-the-job claim must be caught"
        );
    }

    /// The negation guard has to read words, not letters. Each line below
    /// asserts the contradiction while carrying a word that merely *contains*
    /// "not" — and each was waved through by the first version of the guard,
    /// which is the failure that matters: a gate that misses the thing it
    /// exists for reports clean.
    #[test]
    fn a_word_containing_not_does_not_read_as_a_denial() {
        for text in [
            "the console host is in the job object, so nothing outlives the session",
            "the console host sits in the job and cannot break away",
            "another job object holds the console host, in the job with its child",
        ] {
            assert!(
                reserved_pattern_hit(text).is_some(),
                "this must be caught: {text}"
            );
        }
    }

    /// The corrections must stay sayable. A pattern that flagged the sentence
    /// putting the record straight would make the record impossible to keep.
    #[test]
    fn the_console_host_corrections_stay_legal() {
        for text in [
            "the host is deliberately *not* in the job: terminating it would take the terminal down",
            "that host is not put in the job, and is released by closing the pseudo-console",
            "the console host stays outside the job object that contains the child",
        ] {
            assert!(
                reserved_pattern_hit(text).is_none(),
                "this must stay legal: {text}"
            );
        }
    }

    /// Every phrasing the claim has actually taken across the repository, so
    /// a reworded reintroduction is caught rather than only the exact
    /// sentence that happened to be corrected.
    #[test]
    fn every_recorded_phrasing_of_the_ci_equality_claim_is_caught() {
        for text in [
            "run `cargo xtask ci` before pushing — it is exactly what CI runs.",
            "- [ ] `cargo xtask ci` is green locally (it is exactly what the PR tier runs).",
            "`cargo xtask ci` runs exactly what the CI workflow runs",
            // The README's real layout, unmodified: the claim stands alone
            // and the command sits in the fenced block beneath it. Appending
            // the command to this line — which an earlier version of this
            // test did — tests the rule against an input that never existed
            // and hides the fact that the true one was not caught.
            "One command, identical to what the PR-tier CI runs:\n\n```\ncargo xtask ci\n```",
            "It is **exactly what the PR-tier CI runs** — cargo xtask ci",
            "cargo xtask ci — so green locally and green in CI cannot diverge",
            // The quieter form: a count of what it leaves out, which was
            // wrong on the day it was written.
            "`cargo xtask ci` is the PR tier bar one job",
            "the one PR-tier check `cargo xtask ci` does not include",
            "`cargo xtask ci` is green locally (it is the PR tier, less the supply-chain gate)",
            // The two that survived correcting the explicit phrasings,
            // because the command sat in a fenced block above rather than on
            // the line making the promise.
            "…and the two gates below — so green locally means green in CI.",
            "…and the two layout/drift gates — so if it is green locally it is green in CI.",
            // The workflow-header form, which outlived several sweeps.
            "# same dev-task runner a contributor runs locally. If it passes on your machine it passes here.",
            "# runs `cargo xtask <task>` — it is the identical logic.",
        ] {
            assert!(
                reserved_pattern_hit(text).is_some(),
                "this phrasing must be caught: {text}"
            );
        }
    }

    /// The rule is line-scoped precisely so ordinary prose survives it. These
    /// are the shapes that must stay legal, including the honest replacement
    /// the claim was corrected to.
    #[test]
    fn the_ci_equality_rule_leaves_honest_prose_alone() {
        for text in [
            // The corrected statement: every check is *a* task, not one task
            // is all of CI.
            "Every check CI runs is a `cargo xtask` task, so local and CI cannot drift apart.",
            "`cargo xtask ci` is one task among several the PR tier invokes.",
            // "exactly what" used for something else entirely, on its own
            // line — the false positive a whole-file rule would produce.
            "A PTY that cannot be allocated is exactly what the probes exist to catch.",
            "cargo xtask ci",
        ] {
            assert!(
                reserved_pattern_hit(text).is_none(),
                "this must stay legal: {text}"
            );
        }
    }
}
