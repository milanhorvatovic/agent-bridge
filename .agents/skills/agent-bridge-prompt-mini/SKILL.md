---
name: agent-bridge-prompt-mini
description: Construct and validate orchestrator prompts that dispatch work through the agent-bridge-mini skill. Trigger when the user asks for help shaping or checking a prompt that drives one or more agents through the bridge — single-shot dispatches, fan-out patterns, multi-phase workflows, or anything in between.
allowed-tools: Read, Bash
license: MIT
metadata:
  version: 0.1.0
  author: Milan Horvatovič
  tags:
    - agent-orchestration
    - prompt-engineering
    - companion-skill
compatibility: Pairs with agent-bridge-mini. This skill produces prompt text only — for the dispatch itself, agent-bridge-mini must also be loaded. Read access to the agent-bridge-mini bundle (or shell access to run `bridge list` / `bridge show`) is recommended so profile names are verified live rather than against a stale snapshot.
---

# agent-bridge-prompt-mini

A companion skill for [`agent-bridge-mini`](../agent-bridge-mini/SKILL.md). Use it to *write* and *check* prompts that an orchestrator (any coding agent — Claude Code, codex, opencode, kimi, cursor, etc.) hands to the bridge dispatcher. This skill writes nothing to disk and runs no agent dispatches itself; it produces prompt text and validation findings that the user (or the orchestrator) acts on.

This skill does NOT replace `agent-bridge-mini`. To actually run a dispatch, both skills must be available — this one shapes the prompt, that one executes it. Use either alone is a misuse: this skill alone won't dispatch anything; that skill alone will dispatch whatever the orchestrator improvises.

## When to trigger this skill

- User asks how to write a prompt that drives one or more agents through the bridge.
- User pastes a draft prompt and asks "is this right?" / "what's wrong?" / "validate this" / "fix this".
- User describes a workload (single-shot, fan-out, multi-phase, fan-in, etc.) and wants help shaping it as a bridge-dispatch prompt.
- User mentions writing or checking a prompt for the bridge dispatcher.

## When NOT to use it

- For the dispatch itself — that's `agent-bridge-mini`.
- For prompts that don't involve the bridge.
- For interactive debugging of a *running* dispatch (use bridge logs / capture files directly).
- For general prompt engineering not tied to bridge mechanics.
- For evaluating whether a particular agent will succeed at a task — this skill judges the *prompt's shape*, not the chosen agent's fitness.

## Pre-flight: verify the bundle

Before constructing or validating a prompt, refresh your knowledge of what's actually installed. Stale assumptions about profile names, models, or capabilities are the most common source of bad prompts. In order of preference:

1. **Run `bridge list`** (if shell access is available) to enumerate live profiles and their default model/effort. The output is authoritative.
2. **Read `.agents/skills/agent-bridge-mini/assets/profiles/*.json`** (if file access is available) to inspect each profile directly — useful when the user references a feature like a `review` block or a `prompt_args` template and you need to check if that profile actually has it.
3. **Run `bridge show <agent>`** to confirm a single profile's resolved shape (model, effort, model_args, effort_args, review block, skill_format) before referencing those defaults in the constructed prompt.

The "Profile-name fallback" section below is *only* for when neither shell nor file access is available. Treat it as illustrative, not authoritative — bundles can rename, add, or remove profiles.

## The shape of an orchestrator prompt

The full shape has five elements. Not all are needed every time — see [Sizing the prompt](#sizing-the-prompt). Order matters: place the role assertion early, place the inner prompt with a clear boundary, place the report shape last.

1. **Skill invocation.** Start with `/agent-bridge-mini` so the orchestrator loads the bridge skill explicitly. More deterministic than relying on description matching. Always include this.

2. **Dispatcher-only assertion.** One sentence asserting the orchestrator must not perform the task itself: "Dispatcher only. Do not <do the task> yourself, do not <related side-effects>, do not <persist anything>." Place this *immediately* after the skill invocation — anything after the assertion is interpreted relative to that role; placing it later weakens it. Include whenever the orchestrator might be tempted to do the work itself.

3. **Phase definitions.** For each phase: the subcommand (`bridge run` vs `bridge review`), the agent or agents, the schedule (single dispatch / parallel within phase / serial across phases / fully parallel), and the inner prompt. Optional flags (`-m`, `-e`, `--no-context`, `--uuid`, `--output-dir`) belong here when they apply. Skip the phase framing for a single dispatch.

4. **Inner prompt.** The text passed to the agent via `-p` (or stdin). Wrap it in a blockquote (`> …`) so the boundary between "instructions to orchestrator" and "instructions to spawned agent" is unambiguous. Use second-person address ("you") inside the inner prompt so the spawned agent knows the work is for it. Use `/skill:<name>` canonical form for skill references; the bridge translates per agent.

5. **Final-report shape.** What you want back from the orchestrator: resolved agent names, exit codes, capture-file paths, summaries, etc. Caps the orchestrator at "report" rather than "do more work." Include for any multi-agent or multi-phase dispatch; optional for a single-shot run where the agent's stdout is itself the answer.

### Sizing the prompt

| Workload | Minimum elements |
|---|---|
| Single dispatch, single agent, agent's stdout is the answer | (1) skill invocation + (4) inner prompt |
| Single dispatch, single agent, agent will modify code or persist state | (1) + (2) dispatcher-only + (4) inner prompt |
| Multi-agent fan-out (parallel or serial) | (1) + (2) + (3) phases + (4) + (5) report |
| Multi-phase workflow (fan-out + fan-in, sequential phases, etc.) | All five, with explicit phase ordering |

Don't pad. A trivial single-shot prompt that's already short doesn't need a dispatcher-only sentence; a multi-phase fan-in prompt that's already long doesn't need redundant pseudocode.

## Subcommand decision

| Inner prompt is… | Use |
|---|---|
| Native code review only (no extra steps) | `bridge review <agent>` |
| Native code review with caller instructions (extra context, scope notes, follow-up actions) | `bridge review <agent> -p "<extras>"` |
| Anything else (free-form task, multi-skill workflow, Q&A, code generation, refactor, summary, etc.) | `bridge run <agent> -p "<prompt>"` |

**`bridge review -p` rule:** the bridge adds the per-provider `/review` framing itself. Pass `-p "<extras>"`, **not** `-p "/review <extras>"` — the latter double-prefixes on slash-command profiles.

## Operational flags

Orchestrators frequently miss these when constructing prompts. Surface them in templates whenever the user's intent calls for one:

- **`-p "<prompt>"`** — caller-supplied prompt. Required for `run`. Optional for `review` (omit to send the bare native review with no extras).
- **`-m <model>`** — override the profile's default model for one dispatch. Useful when the user wants a faster/cheaper variant, a stronger model for a hard problem, or a specific model ID per provider. **Format depends on the profile**: native CLIs take bare IDs (`claude-opus-4-7`, `gpt-5.5`, `kimi-k2.6`, `gemini-3-pro`); OpenCode-routed profiles (`*-via-opencode*`) take `<provider>/<model-id>` (e.g. `kimi-for-coding/kimi-k2-thinking`, `zai-coding-plan/glm-4.7`) — bare names error there. `bridge show <agent>` shows the profile's default to use as a template.
- **`-e <effort>`** — override the profile's default reasoning effort. Useful for "use maximum reasoning", "skip thinking for a fast pass", or any per-run knob the underlying CLI exposes. Effort vocabulary is per-CLI; verify supported values via `bridge show <agent>` before recommending one. **`-e ''` (empty string) suppresses the effort flag entirely** — escape hatch for models that don't take it (e.g. `claude-haiku-4-5`) without editing the profile.
- **`--no-context`** — disable auto-routing. Use when the user explicitly wants the literal profile name and not its per-context variant.
- **`--uuid <12-hex>`** — predetermine the run's capture-file UUID so the orchestrator knows the path before the dispatch starts.
- **`--output-dir <path>`** — predetermine the capture directory. Combined with `--uuid`, the orchestrator computes every capture path up front instead of grepping logs afterward.

## Skill reference syntax

Use `/skill:<name>` inside any inner prompt. The bridge rewrites per agent according to the profile's `skill_format`:

| Profile flavor | `/skill:foo` becomes |
|---|---|
| Slash-style (claude, cursor, opencode-routed profiles) | `/foo` |
| Dollar-style (codex) | `$foo` |
| Native passthrough (echo, native kimi) | `/skill:foo` |

Write `/skill:<name>` once and let the bridge translate. Do NOT pre-rewrite to agent-native forms — that breaks portability when the same prompt goes to a differently-configured agent.

The rewrite is word-boundary aware: `/skill:foo` embedded in a path or URL (e.g. `path/skill:foo`, `https://example.com/skill:foo`) is NOT rewritten, so file paths and URLs in inner prompts are safe. Plugin-namespaced names like `/skill:plugin:review` are supported — the colon-bearing name is captured whole and survives translation.

## Capture, cost, and privacy

**Capture-file convention.** State the convention; do *not* paste pseudocode:

> Use `--uuid` and `--output-dir` for every dispatch so capture paths are deterministic.

The orchestrator already knows the mechanics from `agent-bridge-mini`. Re-spelling the shell loop crowds out actual intent.

**Path derivation.** With `--uuid` and `--output-dir` both set, the orchestrator can compute every capture path up front without parsing banners or grepping `runs.log`: `<output-dir>/<uuid>-<resolved-agent>[-<sanitized-model>].{out,err,timeline}`. Two transformations to remember: auto-routing may turn `claude` into `claude-personal`/`claude-work` (pass `--no-context` for verbatim naming), and filename sanitization collapses any char outside `[A-Za-z0-9._-]` to `_` (so `zai-coding-plan/glm-5.1` becomes `zai-coding-plan_glm-5.1` in the filename). When prediction is impractical, `ls "$DIR/$UUID-"*.out` still finds the file — the UUID alone is unique within the runs dir.

**UUID reuse.** The bridge refuses to clobber a non-empty existing capture file at the same UUID — exit 2 with a clear error rather than overwriting prior output. Empty leftovers from an aborted prior run (touched but never written) DO get silently overwritten, so retry logic that re-uses a UUID after a Popen-failure works. For independent dispatches, generate fresh UUIDs (`secrets.token_hex(6)` in Python, equivalent in shell) — sequential or low-entropy UUIDs make the TOCTOU clobber check race-prone.

**Reading back captures.** When a later phase consumes a prior dispatch's output, prefer `bridge replay <uuid>` over `cat .out` — replay walks the `.timeline` sidecar and routes each chunk to stdout/stderr per its original FD, restoring chronological interleaving. This matters most for `merge_streams: true` profiles (a profile-level opt-in for CLIs whose visible output lives mostly on FD2 — opencode-routed profiles, `codex exec`), where `.out` holds both streams interleaved and `cat .out` mixes answer with diagnostic; replay is also the only way to recover the FD distinction on a merged capture. Verify whether a profile sets `merge_streams` via `bridge show <agent>`. `--tag` prefixes each chunk with `[stdout]`/`[stderr]` for single-sink inspection.

**Exit code semantics.** The bridge returns the underlying agent's exit code on success. Pre-flight failures (unknown agent, malformed profile, missing prompt, `--uuid` collision with an existing non-empty capture) exit `2` without recording a run. `FileNotFoundError` on the agent binary exits `127`; `PermissionError` exits `126`. State a failure policy in multi-phase prompts that names these distinctions: exit `2` is unrecoverable (typo / missing config / re-used UUID), `127`/`126` is environmental, and a non-zero agent exit may be retryable depending on the underlying CLI.

**No bridge-level timeout.** The bridge waits indefinitely for the subprocess. A hung agent hangs the bridge. If the orchestrator's failure policy includes "abort after N seconds", wrap each dispatch in `timeout(1)` or the orchestrator's own kill switch — the bridge intentionally doesn't impose a deadline because reasonable bounds vary across CLIs and tasks. SIGINT (Ctrl+C) is caught: the child gets SIGTERM, then SIGKILL after 2s, then the bridge exits 130 — but no audit record is written for an interrupted run.

**Audit trail.** Every reaching-Popen dispatch appends a JSONL line to `<tempdir>/agent-bridge-mini/runs.log` with `{ts, uuid, action, agent, requested_agent, context, model, effort, prompt, skills, command, exit, duration_s, output_stdout, output_stderr, output_timeline, merge_streams}`. Use it when the orchestrator needs to confirm what was actually dispatched (resolved agent, model/effort applied, capture paths) rather than what was asked for. Two field distinctions worth noting: `requested_agent` is what the caller typed and `agent` is what the bridge dispatched (they differ when auto-routing fired); the `command` field has the prompt redacted to `<prompt>` when it would otherwise appear in argv, but the dedicated `prompt` field is verbatim and unredacted — re-read the privacy note below. For `bridge review` runs, `prompt` is `"/review"` (slash-command profiles, no `-p`), `null` (review-block profiles like codex's default path), or the assembled framing as the agent saw it (with `-p`).

**Audit-record vs. on-disk asymmetry.** Two cases where `output_*` fields don't match on-disk reality, both worth handling defensively when the orchestrator pre-computed paths from `--uuid` + `--output-dir`:
- **Passthrough fallback.** If the runs dir is unwritable (read-only mount, points at a file, permission denied), the bridge runs the agent with inherited stdio and sets all `output_*` to `null`. The pre-computed path WILL NOT exist on disk. Check `output_stdout: null` (or absence of the file) before reading.
- **Popen-failure partial output.** When the agent binary is missing/unrunnable, the bridge unlinks only the empty capture files. Anything that streamed before the crash survives, but the audit record still shows `output_*: null` (the field advertises the managed contract, not "is a file there"). Glob `$DIR/$UUID-*` directly when partial output matters.

**Streaming expectations.** The bridge tees agent output verbatim — but many agents (e.g. `claude --print`) buffer their full response before emitting it, so the user sees nothing until completion. The `[bridge:run …]` banner prints immediately so the user knows the dispatch started; treat the banner as a human signal, not a parseable contract. Don't promise "incremental output" in an inner prompt unless the underlying agent is known to stream.

**Cost / concurrency footprint.** For multi-agent dispatches, *state the peak concurrency* in the prompt so the user knows what they're committing to. Example phrasing: "this is N agents × M phases at peak — verify your rate limits cover that." Fan-out is cheap to write but expensive to run; the dispatcher won't push back on quota, only the underlying providers will.

**Privacy.** **The bridge logs prompts verbatim to `runs.log`.** Anything in an inner prompt — credentials, internal documentation, customer data, API keys, secrets, private URLs — gets persisted to disk in plain text and forwarded to whichever agent's CLI runs. Treat the inner prompt as a *publishable* payload; if it contains anything sensitive, redact it before constructing the prompt. This is the single most important thing to verify in a draft.

**Bridge invocation form.** Templates use `bridge run …` / `bridge review …` assuming the user has aliased the dispatcher. If they haven't, the same call is `python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run …`. Either note the translation when constructing the prompt or use the full path verbatim — don't silently assume the alias exists.

## Validation checklist

Grouped by severity. When validating a draft, walk groups in order — must-fix first, then should-fix, then context-dependent.

### Must-fix (broken or unsafe dispatch)

1. **Profile names exist.** Cross-reference against live `bridge list` (or the profiles directory). Flag unknown agents — the dispatch will exit 2.
2. **Subcommand matches inner prompt content.** Free-form / multi-skill / non-review work → `bridge run`. Native review (with or without caller extras) → `bridge review` (with or without `-p`).
3. **No double-prefix on `bridge review -p`.** The `-p` payload must not start with `/review`.
4. **No sensitive data in the inner prompt.** Secrets persist in `runs.log` and reach external agents. This is non-negotiable; raise it as a stop-the-line finding even if everything else is perfect.
5. **Dispatcher-only assertion present** when the workload could be mistaken for a do-it-yourself task.

### Should-fix (clarity and reliability)

6. **`/skill:<name>` canonical form** in inner prompts (not pre-rewritten to agent-native forms).
7. **Verbatim boundary clear.** Inner prompts wrapped in blockquotes; no inline mixing of orchestrator-instructions and spawned-agent-instructions.
8. **Phase ordering explicit** for multi-phase workloads. State whether phases run concurrently, sequentially, or pipelined; ambiguity invites the orchestrator to improvise.
9. **Final-report shape specified** for multi-agent dispatches. Otherwise the orchestrator may keep working past dispatch.
10. **Operational flags applied where applicable** (`-m`, `-e`, `--no-context`, `--uuid`, `--output-dir`). Surface them when the user's intent implies a non-default model/effort, no-routing dispatch, or deterministic capture paths.
11. **No redundant pseudocode.** The bridge skill already documents argv construction, capture conventions, and auto-routing. Re-spelling shell loops in the prompt buries intent.

### Context-dependent (raise as a warning, not a blocker)

12. **Cost / concurrency footprint stated** for multi-agent parallel runs. N agents × M phases at peak — does that exceed the user's rate limits?
13. **Failure policy stated** for multi-phase workloads. What if phase-K dispatch fails — abort? continue with surviving captures? retry? The bridge has no built-in policy beyond exit codes; the orchestrator will improvise unless told.
14. **Subcommand-specific caveats flagged.** Some review-block profiles drop their default scope flag when a custom prompt is passed via `-p`; the resulting scope may differ from the default invocation. Flag when the user's intent depends on the default scope.
15. **Bridge invocation form** (alias vs full path) appropriate for the user's environment.

## Profile-name fallback

Use only if pre-flight verification is unavailable. This is illustrative — real bundles may add, remove, or rename profiles.

Typical bundled set:

- A local sanity-check profile (often `echo`-style) — useful for testing the dispatcher itself without an LLM call.
- One profile per supported native CLI (claude, codex, cursor, native kimi, etc.).
- Per-context variants (`-personal` / `-work` suffixes) for profiles that auto-route by environment.
- Router-fronted profiles (`<provider>-via-opencode` etc.) for accessing many providers through a single CLI.

Common typos and ambiguities to flag in user-supplied prompts:

- A bare provider name without a routing suffix may not be a valid profile (e.g. someone writes `glm` when the bundle only has `glm-via-opencode`). The bridge will exit 2 with `unknown agent: <name>`.
- A name that *is* a valid profile may not be the one the user intended (e.g. a native single-context CLI vs. a router-fronted multi-context variant). When auth, billing, or per-subscription routing matters, confirm the right variant.
- Auto-routing means the *resolved* name in the audit log may differ from the *requested* name. Don't force a suffix unless the user wants explicit control via `--no-context`.

## Templates

Each template is shown twice: with placeholders, then with a fully-filled illustrative example so the pattern is unambiguous. The filled examples are *generic* — substitute real agent names, prompts, and policies for your actual workload.

### Single dispatch, single agent

**Placeholder:**

```
/agent-bridge-mini Run `bridge run <agent> -p "<prompt>"` <optional flags>.
```

**Filled (illustrative):**

```
/agent-bridge-mini Run `bridge run <agent-A> -p "explain in two sentences what the file <path> does"`.
```

For a single dispatch where the agent's stdout *is* the answer, no further structure is needed.

### Single-phase fan-out

**Placeholder:**

```
/agent-bridge-mini Dispatcher only. Do not <do the task> yourself, do not <persist anything>.

Run `bridge <run|review>` against <agents> in <serial|parallel>. Pass each agent this prompt verbatim via -p:

> <inner prompt; second-person; /skill:<name> for skill refs>

Use --uuid + --output-dir for deterministic capture paths.
Report each agent's resolved name, exit code, and <other artifacts the user wants back>.
```

**Filled (illustrative):**

```
/agent-bridge-mini Dispatcher only. Do not answer the question yourself.

Run `bridge run` against <agent-A>, <agent-B> in parallel. Pass each agent this prompt verbatim via -p:

> you are an independent reviewer. Read the file <path> and produce three concrete suggestions for improving its readability. Be terse.

Use --uuid + --output-dir for deterministic capture paths.
Report each agent's resolved name, exit code, and capture-file paths. Note: this dispatches two agents at once — verify rate limits.
```

### Multi-phase, fan-out + fan-in

**Placeholder:**

```
/agent-bridge-mini Dispatcher only — fan out, collect, fan back in. Do not perform the task yourself.

PHASE 1 — `bridge <run|review>` in <parallel|serial>: <agents>. Pass each via -p:

> <phase-1 inner prompt>

PHASE 2 — once phase 1 has fully exited, `bridge <run|review>` <single agent or fan-out>. If this phase consumes phase-1 output, instruct the orchestrator to embed each capture (via `bridge replay <uuid>`, or `cat .out` for non-merged profiles where stdout-only is sufficient) preceded by a header like `=== <agent> ===` so the consuming agent can attribute. Pass via -p:

> <phase-2 inner prompt>

Use --uuid + --output-dir; share one scratch directory across phases.
Failure policy: <e.g. continue with surviving captures and mark missing in the report>.
Report <artifacts>.
```

**Filled (illustrative):**

```
/agent-bridge-mini Dispatcher only — fan out, collect, fan back in. Do not perform the task yourself.

PHASE 1 — `bridge run` in parallel: <agent-A>, <agent-B>, <agent-C>. Pass each via -p:

> you are an independent contributor. Propose one approach to <generic problem>. Be concrete; one paragraph.

PHASE 2 — once phase 1 has fully exited, `bridge run <agent-D>`. The orchestrator must embed all three phase-1 captures (via `bridge replay <uuid>` per phase-1 dispatch), each preceded by `=== <agent-name> ===`, before the inner prompt below. Pass via -p:

> here are three independent proposals. Synthesize a single recommendation that takes the strongest ideas from each, names any conflicts, and flags anything only one contributor caught.

Use --uuid + --output-dir; share one scratch directory across phases.
Failure policy: continue with surviving captures and mark missing agents in the phase-2 prompt header.
Concurrency: phase 1 dispatches three agents in parallel — verify rate limits.
Report each phase's resolved agent names and exit codes, the phase-2 capture path, and a one-paragraph summary of phase 2's synthesis (drawn from its capture, not your own analysis).
```

## Examples

### Validation report shape

When given a draft, produce findings keyed to the checklist groups. Schematic shape:

**Input draft:**

```
/agent-bridge-mini ask <wrong-name> and <ambiguous-name> to /review the changes and fix everything
```

**Output:**

```
Must-fix:
- Profile names: `<wrong-name>` is not a profile (suggested correction). `<ambiguous-name>` is valid but resolves to <variant X>; confirm whether the user meant <variant Y>.
- Subcommand: ambiguous. "Review the changes and fix everything" reads as caller instructions extending /review → use `bridge review -p "fix everything"`. The literal `/review` would double-prefix; drop it from the -p payload.
- Dispatcher-only assertion missing — orchestrator may attempt the fixes itself instead of dispatching.
- Privacy: confirm the changes don't include secrets — they will be persisted in runs.log.

Should-fix:
- Verbatim boundary unclear: the inner prompt is inline, not blockquoted.
- No final-report shape — orchestrator may keep working past dispatch.
- No --uuid / --output-dir; capture paths will be improvised and may collide on re-run.

Context-dependent:
- Concurrency: dispatching to two agents at once doubles peak rate-limit pressure on each provider.
- Failure policy: not stated; the orchestrator will improvise if one agent fails.

Suggested rewrite available on request.
```

### Constructed prompt shape

When the user describes a task without a draft, output a single fenced code block containing the complete prompt, ready to copy. Add at most one sentence of meta-explanation before the block; let the prompt speak for itself.

### Unified rewrite shape

When the user gives a draft and asks for a fix, output a single fenced code block with the corrected prompt. Optionally precede it with a short bulleted list (3–5 bullets max) naming what changed. Keep the bullets terse: "X moved earlier", "Y dropped", "Z added", not paragraphs.

## Output behavior

This skill produces one of three outputs. Pick by inferring from the user's input — don't volunteer multiple shapes when one was clearly requested:

- User pasted a draft and asked "is this right?" / "validate" / "what's wrong?" / "review this" → **validation report**.
- User pasted a draft and asked "fix this" / "improve" / "rewrite" / "correct" → **unified rewrite**.
- User described a task without a draft → **constructed prompt**.
- User pasted a draft without saying what to do with it → ask once, briefly: "validate, rewrite, or fresh draft?". Do not guess; do not produce all three.

Match the contract; deliver one output shape per turn.
