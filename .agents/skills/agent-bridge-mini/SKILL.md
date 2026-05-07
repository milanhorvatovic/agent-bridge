---
name: agent-bridge-mini
description: Dispatches a one-shot prompt to another local coding agent (claude, codex, kimi, opencode) via the bridge dispatcher. Trigger on specific-model requests, second opinions, or cross-agent comparisons.
allowed-tools: Bash
compatibility: Requires Python 3.9+ and at least one supported native CLI installed and authenticated (claude, codex, cursor-agent, kimi, or opencode). Auto-routing to per-subscription variants reads OPENCODE_PROFILE, CLAUDE_CONFIG_DIR (compared against CLAUDE_PERSONAL_DIR / CLAUDE_WORK_DIR), or as a last resort matches "share-personal"/"share-work" in XDG_DATA_HOME — typically set by zsh wrappers in ~/.zshrc. Without any of these, the bridge falls back to generic profiles. macOS-tested.
license: MIT
metadata:
  version: 0.5.0
  author: Milan Horvatovič
  tags:
    - agent-orchestration
    - cli-dispatcher
    - multi-provider
---

# agent-bridge-mini

The minimalistic implementation of the agent-bridge concept. A small Python dispatcher at `scripts/bridge.py` runs other local coding agent CLIs via JSON profile templates under assets/profiles. Use this skill to delegate a task to another agent without leaving the current session.

The bridge does NOT manage credentials — each underlying agent CLI must already be installed and signed in. The bridge just shells out and captures the result.

## When to trigger this skill

- User asks for a specific model: "ask kimi to ...", "have codex try ...", "run this through GLM".
- You want a second opinion from a different provider on a hard problem.
- You want to compare how two agents answer the same prompt.
- The user references "the bridge", `bridge.py`, or the assets/profiles directory.

## When NOT to use it

- For interactive, multi-turn work — the bridge is one-shot only. No session resumption.
- When the user wants *you* (the current agent) to do the work.
- For streaming output — the bridge waits for the agent to finish before returning.

## How to invoke

Paths shown here are **skill-relative**. The dispatcher is `scripts/bridge.py` and profile templates live under assets/profiles. From outside the bundle, prefix with the skill's deployed location (in this repo: `.agents/skills/agent-bridge-mini/`).

```sh
# list available agent profiles (shows default model + effort)
python3 scripts/bridge.py list

# show a profile's JSON
python3 scripts/bridge.py show <agent>

# run with an inline prompt (uses the profile's default model + effort)
python3 scripts/bridge.py run <agent> -p "<prompt>"

# run with a piped prompt (useful for long or multi-line prompts)
echo "<prompt>" | python3 scripts/bridge.py run <agent>
cat some-file.md | python3 scripts/bridge.py run <agent>

# override model and/or reasoning effort for one run
python3 scripts/bridge.py run <agent> -p "<prompt>" -m <model-id> -e <effort>

# run a code review using the agent's native /review or review subcommand
python3 scripts/bridge.py review <agent>

# attach extra review instructions (extends '/review' for slash-command profiles;
# appended as the trailing positional for native review subcommands like codex)
python3 scripts/bridge.py review <agent> -p "<extra instructions>"
echo "<extra instructions>" | python3 scripts/bridge.py review <agent>
```

For interactive sessions, alias it with the deployment prefix:
```sh
alias bridge='python3 .agents/skills/agent-bridge-mini/scripts/bridge.py'
```

Exit code is the underlying agent's exit code. Unknown agent → exit 2. Forcing `-m` or `-e` on a profile that doesn't support it → exit 2 with a clear error. A malformed profile JSON (missing/empty `command`, bad JSON, non-dict `env`, non-string `cwd`, a `model`/`effort` default with the matching `*_args` set to null, or any `*_args` template that omits the `{value}` placeholder) → exit 2 at load time with the offending file name. Calling `run` without `-p`/stdin → exit 2 rather than hanging.

Every dispatched run gets:
- A 12-char hex **UUID** that identifies it across the audit log and the capture files. The orchestrator can either let the bridge auto-generate it or pass `--uuid <hex>` to predetermine it (so it can compute the capture-file paths before the bridge even starts).
- **Three per-run capture files** at `<runs-dir>/<uuid>-<agent>[-<model>].{out,err,timeline}` — split stdout/stderr (verbatim agent bytes, ideal for `diff` / `cmp` / `grep`) plus a tiny `.timeline` sidecar that records `<monotonic_ns> stdout|stderr <byte_count>` per chunk for chronological reconstruction. The default `<runs-dir>` is `<tempdir>/agent-bridge-mini/runs/` (where `<tempdir>` is `tempfile.gettempdir()` — `/var/folders/.../T/` on macOS, `/tmp` on Linux); override it per-run with `--output-dir <path>`. Both the terminal sees stdout/stderr in real time AND the files capture them; parallel runs get different UUIDs and never overwrite each other.
- **Two stderr banners**: `[bridge:run uuid=… agent=… model=… effort=… stdout=<path> stderr=<path>]` at start and `[bridge:done uuid=… exit=… duration=…s]` at end. The path values are absolute so the user can paste them directly into `cat`/`tail`. Stderr-only so they don't pollute stdout redirection.
- A **JSONL audit line** in `<tempdir>/agent-bridge-mini/runs.log` with `{ts, uuid, action, agent, requested_agent, context, model, effort, prompt, skills, command, exit, duration_s, output_stdout, output_stderr, output_timeline}` — `prompt` is the original (pre-skill_format-rewrite) prompt the caller provided, `skills` is every `/skill:<name>` reference found in it (deduped, in order, empty list when none), each `output_*` is the absolute path to the corresponding capture file (or `null` if the runs-dir couldn't be created — in which case the bridge falls back to inherited stdio). Note: the dedicated `prompt` field is NOT redacted; if prompts are sensitive, treat `runs.log` accordingly. Logs live under tmp rather than inside the bundle, so a read-only install still works and the bundle's `git status` stays clean.

Pre-flight failures (unknown agent, missing prompt, bad profile, missing `cwd`) exit 2 without logging or writing a capture file — only invocations that reached `subprocess.Popen` are recorded. Tail it or pipe through `jq` to inspect history. **The user's prompt is redacted from the logged `command` (replaced with `"<prompt>"`)** when it would otherwise appear in argv (`prompt_mode: "arg"` or `prompt_args` profiles); stdin-mode prompts never enter argv to begin with. The log has no rotation — truncate manually if it grows large. `bridge show` does NOT auto-route; it always prints the literal profile name you ask for.

## Output handling: control UUIDs and dirs via flags, not shell redirects

**Do not redirect bridge output to ad-hoc filenames.** Every `bridge run` already writes UUID-namespaced capture files (`.out` / `.err` / `.timeline`). Re-using stable paths collides on re-run, hides which run produced which output, and discards the UUID — the only stable identifier tying the files to the audit log.

For orchestrators that need to know exactly where output lands, use **`--uuid <hex>` + `--output-dir <path>`**: the orchestrator generates the UUID and picks the dir, so it can compute the capture-file paths before the bridge even starts. No banner-parsing, no `runs.log` lookup.

**❌ Don't (ambiguous, clobbers on re-run, fights the bridge):**

```sh
# BAD — these filenames have no UUID; a second invocation overwrites them.
echo "$PROMPT" | python3 scripts/bridge.py run <agent-a> > out-a.txt 2> err-a.txt &
echo "$PROMPT" | python3 scripts/bridge.py run <agent-b> > out-b.txt 2> err-b.txt &
wait
```

**✅ Do (orchestrator owns the UUIDs and knows every capture-file path up front):**

```sh
DIR=$(mktemp -d)
UUID_A=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
UUID_B=$(python3 -c 'import secrets; print(secrets.token_hex(6))')

# Suppress terminal noise; the capture files already have stdout/stderr verbatim.
echo "$PROMPT" | python3 scripts/bridge.py run <agent-a> \
  --uuid "$UUID_A" --output-dir "$DIR" >/dev/null 2>&1 &
echo "$PROMPT" | python3 scripts/bridge.py run <agent-b> \
  --uuid "$UUID_B" --output-dir "$DIR" >/dev/null 2>&1 &
wait

# The orchestrator already knows the paths:
cat "$DIR/$UUID_A-<agent-a>".out
diff "$DIR/$UUID_A-<agent-a>".out "$DIR/$UUID_B-<agent-b>".out
```

`--uuid` must be 12 lowercase hex chars (matches `uuid.uuid4().hex[:12]`). The bridge refuses to clobber a non-empty existing capture file, so a UUID collision exits 2 instead of overwriting prior output.

The capture is split into three files: `.out` (agent stdout, verbatim), `.err` (agent stderr, verbatim), and `.timeline` (one ASCII line per kernel chunk: `<monotonic_ns> stdout|stderr <byte_count>`) so chronological interleaving can be reconstructed if needed (`sort -n` on `.timeline`, then walk `.out`/`.err` byte offsets). For most orchestration tasks (diff, grep, cmp), `.out` and `.err` are all you'll touch.

If the orchestrator just wants to fire-and-forget without picking UUIDs, omit `--uuid`/`--output-dir` and read the bridge-chosen paths back from `runs.log` (`output_stdout` / `output_stderr` / `output_timeline` fields) using a `--argjson since` filter on `ts`.

### Predicting capture-file paths

The full path is deterministic from the inputs:

```
<output-dir>/<uuid>-<resolved-agent>[-<sanitized-model>].{out,err,timeline}
```

Two transformations the orchestrator needs to know about, because both can change the filename away from what the caller typed:

- **Auto-routing rewrites the agent name.** From a `claude-personal` shell, `bridge run claude` resolves to `claude-personal`, so the file is `<uuid>-claude-personal.out`, NOT `<uuid>-claude.out`. Two ways out: pass `--no-context` for verbatim naming, or use the glob escape hatch below.
- **Filename sanitization** applies to BOTH the resolved agent name and the model name: every char outside `[A-Za-z0-9._-]` is collapsed to `_`, and each component is truncated at 80 chars. So `zai-coding-plan/glm-5.1` becomes `zai-coding-plan_glm-5.1` in the filename. Agent names use the safe charset by convention, so the agent component is usually a no-op; model components routinely contain `/` and trigger the rewrite. The `<model>` component is omitted entirely when the profile has no model (e.g. `echo`, `cursor`).

**Robust glob (always works regardless of resolution / sanitization):**

```sh
ls "$DIR/$UUID-"*.out   # the UUID alone is unique; one file per run
```

**Empty leftovers don't block reuse.** If a prior aborted run touched files with the same UUID but never wrote to them (size 0), the bridge silently overwrites — only non-empty existing files trigger refuse-to-clobber.

**Partial cleanup is asymmetric.** On a Popen-style failure (FileNotFoundError, PermissionError, OSError), the bridge unlinks only the empty capture files. So if the agent wrote to stdout before crashing, `.out` survives but `.err` and `.timeline` are gone. The audit record's `output_stdout` / `output_stderr` / `output_timeline` are all set to `null` regardless — they advertise the bridge-managed contract, not "is the file currently on disk." If you need the surviving partial output, glob `$DIR/$UUID-*` directly rather than trusting the audit record.

**`--output-dir` path expansion.** The path passes through `os.path.expandvars` then `expanduser`, so `~/runs` and `$HOME/orchestrator-runs/$JOB_ID` both work. The dir is created with `parents=True` if missing. If it's unwritable (read-only mount, permission denied), the bridge prints a one-line stderr warning, falls back to inherited stdio (no capture files), and sets all three `output_*` fields to `null` in the audit record — the agent still runs. **In passthrough mode, `--uuid` is still recorded in the audit log but no capture files exist** — paths the orchestrator pre-computed from the UUID won't be on disk. Detect this by checking `output_stdout: null` in the audit record before reading the expected files.

### Output, timing, and stability semantics

**`.timeline` semantics.** Each line is `<monotonic_ns> stdout|stderr <byte_count>` recording one `read1(4096)` chunk (one kernel read) — NOT one output line. A long agent line is split across multiple entries; many short lines collapse into one. Lines are NOT pre-sorted by timestamp because two tee threads can capture timestamps in order and write entries in reverse order under lock contention; consumers must `sort -n` to recover chronological order.

**Timestamp basis.** The `.timeline` uses `time.monotonic_ns()` (no wall-clock anchor; not comparable across processes or with the audit record). The audit record's `ts` is wall-clock (`time.time()`, seconds since epoch); `duration_s` is monotonic-derived (immune to NTP / DST jumps). Don't try to correlate `.timeline` entries with audit `ts` — different clock domains.

**No agent timeout.** The bridge waits indefinitely for the subprocess to exit. A hung agent hangs the bridge. If you need bounded execution, wrap the call in `timeout(1)` or your orchestrator's own kill switch — the bridge intentionally doesn't impose a deadline because reasonable bounds vary widely across CLIs and tasks.

**`--uuid` entropy expectation.** The clobber check ("does a non-empty file with this UUID already exist?") is TOCTOU-racy: two parallel processes can both pass the check before either writes. With cryptographically random UUIDs (`secrets.token_hex(6)` → 48 bits), collision probability is ~2^-48 — never observed in practice. If you generate UUIDs sequentially or from low-entropy sources (timestamp, counter), the race becomes real; don't.

**`context` field reflects the bridge's resolution, not the caller's shell.** The audit record's `context` is `""` whenever auto-routing didn't happen — including when the caller was IN a `claude-personal` shell but passed `--no-context`. The field tells you what the bridge did, not what was available.

**The stderr banner is human-readable, not a parseable contract.** `[bridge:run uuid=… stdout=… stderr=…]` and `[bridge:done uuid=… exit=… duration=…s]` exist so a human watching the terminal can see what's happening; their format may evolve. Orchestrators should use `--uuid` + `--output-dir` to predetermine paths, or query `runs.log` (which IS a stable JSON contract) — never grep the banner.

### Platform notes and edge cases

**Signal handling.** SIGINT (Ctrl+C) is caught explicitly: the bridge sends SIGTERM to the child, escalates to SIGKILL after a 2-second wait, then exits 130. The audit record is NOT written for SIGINT — the run was interrupted before completion, and capture files keep whatever was already streamed (no rollback). Other signals (SIGTERM, SIGHUP, SIGQUIT) use Python defaults: they propagate to the process group, the child likely dies, the bridge dies, and no audit record is written.

**File permissions on capture files.** The bridge creates `.out`, `.err`, `.timeline`, and `runs.log` with the parent process's umask (typically `0o022` → `-rw-r--r--`). It does NOT chmod after creation. If you need stricter permissions for sensitive prompts or output, set umask in the calling shell or wrap the invocation: `(umask 0077; bridge run ...)`.

**`--output-dir` pointing at a file.** When `--output-dir` resolves to an existing file (not a directory), `Path.mkdir` raises `FileExistsError`, the bridge prints a one-line warning, falls back to passthrough mode, and sets all `output_*` fields to `null` — same outcome as "unwritable dir."

**Windows.** Bridge is macOS-tested and should work on Linux. On Windows, `fcntl` is unavailable and the bridge silently degrades to unsynchronized `runs.log` appends — concurrent dispatchers writing prompts >8KB may interleave records. Path handling, env-var expansion, `subprocess.Popen` with PIPE, and the threaded tee should all be portable, but nothing is tested on Windows; treat as best-effort.

**Memory and disk bounds.** No limit on prompt size, agent output, or `runs.log` size. The prompt is held in memory once (subprocess stdin write) and also persisted verbatim in `runs.log` (no redaction in the `prompt` field). Agent output is streamed in 4096-byte chunks rather than buffered, so multi-GB outputs don't grow parent-process memory — but they DO grow `<output-dir>` and may fill the disk. There is no log rotation; if you run high-volume orchestration, truncate `runs.log` and clean `<output-dir>` periodically.

**TTY-requiring agents.** The bridge passes `subprocess.PIPE` for stdin/stdout/stderr — never a TTY. Agents that check `isatty(0)` and switch to non-interactive mode work fine. Agents that REQUIRE a TTY (some interactive REPLs) will either error out, refuse to start, or read directly from `/dev/tty` (bypassing the bridge's stdin pipe entirely). Pick a profile that uses the agent's documented non-interactive mode (e.g., `claude --print`, `kimi --print`, `codex exec`, `cursor-agent --print --output-format text`) — all bundled profiles already do this.

## Bundled profiles (current set)

| Name | Binary | Default model | Default effort | Effort vocabulary |
| --- | --- | --- | --- | --- |
| `echo` | `cat` | — | — | n/a |
| `claude` | `claude` | `claude-opus-4-7` | `xhigh` | per model (see below) |
| `claude-personal` | `claude` (env→`$HOME/.claude-personal`) | `claude-opus-4-7` | `xhigh` | per model |
| `claude-work` | `claude` (env→`$HOME/.claude-work`) | `claude-opus-4-7` | `xhigh` | per model |
| `codex` | `codex` | `gpt-5.5` | `high` | `low` / `medium` / `high` / `xhigh` |
| `cursor` | `cursor-agent` | `composer-2` (locked, baked into command) | — | Profile is locked to Composer 2 classic. `-m` and `-e` both error (model_args / effort_args are null). Composer 2 has no reasoning slider; the locked-down design avoids cursor-agent's known suffix-stripping bug for non-Composer models. |
| `kimi` | `kimi` | `kimi-k2.6` | `thinking` | `thinking` (→`--thinking`) / `no-thinking` (→`--no-thinking`) — single-context, no auto-route |
| `kimi-via-opencode` | `opencode` | `kimi-for-coding/k2p6` | — | n/a — pick a thinking variant via `-m kimi-for-coding/kimi-k2-thinking` |
| `kimi-via-opencode-personal` | `opencode` (env→personal XDG) | `kimi-for-coding/k2p6` | — | n/a (model variant) |
| `kimi-via-opencode-work` | `opencode` (env→work XDG) | `kimi-for-coding/k2p6` | — | n/a (model variant) |
| `glm-via-opencode` | `opencode` | `zai-coding-plan/glm-5.1` | — | n/a (model variant) |
| `glm-via-opencode-personal` | `opencode` (env→personal XDG) | `zai-coding-plan/glm-5.1` | — | n/a (model variant) |
| `glm-via-opencode-work` | `opencode` (env→work XDG) | `zai-coding-plan/glm-5.1` | — | n/a (model variant) |

Always run `python3 scripts/bridge.py list` to see the actual current set — profiles may have been edited since this skill was written.

The `*-personal` / `*-work` variants exist because the user keeps separate auth contexts. Both Claude and OpenCode use the same single binary (`claude`, `opencode`) with per-subscription env vars injected by the bridge — `CLAUDE_CONFIG_DIR` for Claude, `OPENCODE_PROFILE` / `XDG_DATA_HOME` / etc. for OpenCode. (The user's interactive `claude-personal` / `claude-work` / `opencode-personal` / `opencode-work` zsh wrappers do the same env-var setup, but shell functions don't propagate to subprocesses, so the profiles replicate them.) Pick the variant that matches the auth/billing context the user wants. If unsure, ask which subscription to use, or default to the generic `claude` / `glm-via-opencode` profiles.

Naming convention: profiles routed through OpenCode are named `<model>-via-opencode[-<auth>]` (e.g. `glm-via-opencode-personal`, `kimi-via-opencode`). Native-CLI profiles use the agent name + optional auth suffix (e.g. `claude-work`).

**Claude available models and effort levels:**
- `claude-opus-4-7` — `low` / `medium` / `high` / `xhigh` / `max`
- `claude-sonnet-4-6` — `low` / `medium` / `high` / `max` (no `xhigh`)
- `claude-haiku-4-5` — does NOT support effort; the profile's default `xhigh` will fail. Either edit the profile to drop the default, use a Haiku-specific profile, or pass `-e ''` per run (empty effort suppresses the flag).

**Routing through OpenCode (this user's setup):** Provider names match what's registered in `~/.local/share/opencode/auth.json` (or the per-XDG-profile equivalent). For this user:
- `kimi-for-coding/` — Kimi K2.6 etc. (Moonshot Coding Plan tier)
- `zai-coding-plan/` — Z.AI GLM models (Coding Plan tier)
- `anthropic/`, `openai/` — registered on default config; routable but use the native `claude` / `codex` profiles when possible.

To prefer Kimi via OpenCode rather than the native `kimi` CLI, use the `kimi-via-opencode` profile (or override any opencode-based profile with `-m kimi-for-coding/k2p6`). Trade-off: OpenCode does NOT expose Kimi's `--thinking` toggle — pick a thinking model variant instead.

**Kimi available models (via OpenCode):** the `kimi-for-coding` provider only registers these three IDs — anything else (e.g. `kimi-k2.6`, `kimi-k2-thinking-turbo`) will error.
- `kimi-for-coding/k2p5` — Kimi K2.5
- `kimi-for-coding/k2p6` (default) — Kimi K2.6, togglable thinking (but OpenCode can't toggle)
- `kimi-for-coding/kimi-k2-thinking` — always-thinking variant, ignores any thinking flag

**GLM available models (via OpenCode):** prefix is `zai-coding-plan/` for this user's Coding Plan tier. Direct-API tier would be `zai/`; verify with `cat ~/.local/share/opencode/auth.json` if uncertain.
- `zai-coding-plan/glm-5.1` (default)
- `zai-coding-plan/glm-5-turbo`
- `zai-coding-plan/glm-5v-turbo`
- `zai-coding-plan/glm-4.5-air`
- `zai-coding-plan/glm-4.7`

GLM has no CLI effort flag in OpenCode — reasoning is controlled by picking a model variant. Passing `-e` to this profile errors.

**Kimi thinking mode (K2.6):** toggled via two flags, exposed through this skill's effort field:
- `-e thinking` (default) → kimi runs with `--thinking` (slow, reasons first)
- `-e no-thinking` → kimi runs with `--no-thinking` (fast, no reasoning step)
- Some always-thinking model variants (e.g. `kimi-k2-thinking`) ignore the flag.

**Codex available models:** `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`. Pass any of these via `-m` to override the profile default. Effort vocabulary is uniform across Codex models: `low` / `medium` / `high` / `xhigh`.

## Resolving personal/work context (automatic)

The bridge **automatically routes to the right subscription variant** when called from an orchestrator. You don't need to think about it — `bridge run claude` from a `claude-personal` shell becomes `bridge run claude-personal` transparently.

### How it decides

The bridge reads env vars set by the orchestrator's shell wrapper:

| Orchestrator | Env signal | Match against |
| --- | --- | --- |
| `claude-personal` / `claude-work` (and `cursor-personal/work` IDE) | `CLAUDE_CONFIG_DIR` | `CLAUDE_PERSONAL_DIR` / `CLAUDE_WORK_DIR` |
| `opencode-personal` / `opencode-work` / `ocp` / `ocw` / `use-opencode-*` | `OPENCODE_PROFILE` | direct string match |

Resolution rules:

1. If the requested name already ends in `-personal` or `-work`, it's used as-is (the user was explicit).
2. If a context is detected and `<requested>-<context>` exists, the bridge silently routes to it and prints `[context: 'X' → 'X-context' (orchestrator: ...)]` to stderr.
3. If no context is detected OR no matching variant exists, the bridge runs the requested profile unchanged (native fallback).

### Effect per profile

| Profile | Has variants? | Behavior under auto-route |
| --- | --- | --- |
| `claude`, `glm-via-opencode`, `kimi-via-opencode` | yes | routes to `-personal` / `-work` when context matches |
| `codex`, `cursor`, `echo`, `kimi` | no | always runs as-is — no resolution |
| `claude-personal`, `claude-work`, etc. | (already explicit) | runs as-is — no resolution |

### Disabling auto-resolution

Pass `--no-context` to force the named profile to be used verbatim:

```sh
python3 scripts/bridge.py run claude --no-context -p "..."  # runs plain `claude` even from claude-personal
```

### Direct context inspection

`scripts/context.py` prints the detected context (`personal`, `work`, or empty). Useful for shell scripts or debugging:

```sh
python3 scripts/context.py   # → personal / work / (empty)
```

## Profile schema (for adding a new agent)

Drop a JSON file into `assets/profiles/<name>.json`:

```json
{
  "description": "human-readable label",
  "command": ["binary", "arg", "..."],
  "prompt_mode": "stdin",
  "model": "default-model-id",
  "effort": "medium",
  "model_args": ["--model", "{value}"],
  "effort_args": ["--effort", "{value}"],
  "prompt_args": ["-p", "{value}"],
  "env": { "OPTIONAL_VAR": "value" },
  "cwd": "optional/path"
}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `command` | yes | Base argv. The bridge runs this, then appends `model_args`, `effort_args`, and the prompt. Do NOT bake `--model` / `--effort` into `command` — set them via the dedicated fields so CLI overrides work. |
| `prompt_mode` | no (default `stdin`) | `stdin` pipes the prompt to the process; `arg` appends it as the final argv element. Ignored when `prompt_args` is set. |
| `prompt_args` | no | Template for CLIs that take the prompt through a value-bearing flag. When set, takes precedence over `prompt_mode`. |
| `review` | no | Optional override for `bridge review <agent>`. When present, replaces `command` / `model_args` / `effort_args` for the review action and may declare `scope_default`; the bridge sends no framing prompt by default (the underlying CLI's native review derives scope from git). When absent, `bridge review` uses the profile's main command + sends `/review` as the prompt. **Caller-supplied review instructions** (`bridge review <agent> -p "<text>"` or piped stdin) are routed natively per profile: for review-block profiles they're appended as the trailing positional argument; for slash-command profiles they extend the framing as `/review <text>` over the profile's normal prompt mode. `scope_default` is only added when no caller prompt is supplied; bundled `codex.json` therefore runs `codex review --uncommitted` by default, but `bridge review codex -p "<text>"` runs `codex review "<text>"`. Default model/effort always come from the top-level profile (so `bridge review` matches `bridge run` defaults unless overridden by `-m`/`-e`); the review block only overrides the *flags* used to pass them. |
| `model` | no | Default model ID. Override per run with `-m`. |
| `effort` | no | Default reasoning effort. Override per run with `-e`. |
| `model_args` | no (default `["--model", "{value}"]`) | Template for the model flag. `{value}` is replaced by the model ID. Set to `null` to disable model selection. |
| `effort_args` | no (default `["--effort", "{value}"]`) | Template for the effort flag. Set to `null` to disable effort selection (e.g. when the CLI uses model variants instead). |
| `description` | no | Shown by `bridge.py list`. |
| `skill_format` | no (default `"/skill:{name}"`) | Per-agent template applied to **every** `/skill:<name>` reference in the prompt (word-boundary aware: embedded refs like `path/skill:foo` are skipped). The captured `<name>` replaces `{name}`; everything else is preserved verbatim. Set to `"/{name}"` for Claude/OpenCode/Cursor (which use `/<name>`), `"${name}"` for Codex (which uses `$<name>`), or leave unset for native Kimi (which already uses `/skill:<name>`). Must contain `{name}`. |
| `env` | no | Extra env vars merged on top of the inherited environment. Values pass through `os.path.expandvars` then `expanduser`, so `$HOME` / `~` / `$VAR` work — be aware that any `$NAME` in the value will be substituted from the parent process env. |
| `cwd` | no | Working directory; `~` is expanded. |

For `prompt_mode`: pick `stdin` if the CLI reads stdin in one-shot mode, `arg` if it takes the prompt as a positional argument. For CLIs that take prompt via a value-bearing flag, set `prompt_args` instead.

**Skill reference translation (`/skill:<name>`):** A canonical input form lets one orchestrator address skills uniformly across agents. The bridge rewrites **every** `/skill:<name>` reference in the prompt — leading prefix and body — using the profile's `skill_format`:

| Profile | `skill_format` | `do /skill:review first then /skill:simplify` becomes |
| --- | --- | --- |
| `claude*`, `cursor`, `glm-via-opencode*`, `kimi-via-opencode*` | `/{name}` | `do /review first then /simplify` |
| `codex` | `${name}` | `do $review first then $simplify` |
| `kimi` (native), `echo` | unset → default `/skill:{name}` | `do /skill:review first then /skill:simplify` (passthrough) |

Word-boundary aware: a `/skill:foo` embedded inside a path or URL (preceded by a word char or another `/`, e.g. `path/skill:foo`) is **not** rewritten — the lookbehind in the regex protects it. Plugin-namespaced names like `/skill:plugin:review` are supported (the colon-bearing name is captured whole). The rewrite scope matches the `skills` field in `runs.log` exactly: every reference the audit log reports as present in the prompt is also a reference the bridge actually translated for the agent.

The first element of `command` is the binary — change it to swap to a different installed CLI (e.g. `claude-personal`, `opencode-work`, a wrapper script). One profile per (binary, defaults) combo; no inheritance.

## Common patterns

**Delegating a coding task to another agent:**
```sh
python3 scripts/bridge.py run kimi -p "Write a Rust function that reverses a UTF-8 string safely."
```

**Second opinion on a diff with extra reasoning:**
```sh
git diff HEAD~1 | python3 scripts/bridge.py run claude -e high
```

**Hard problem on Codex with maximum reasoning:**
```sh
python3 scripts/bridge.py run codex -p "Audit this auth flow for IDOR." -e xhigh
```

**Cross-provider code review (one command per provider):**
```sh
python3 scripts/bridge.py review claude              # /review via claude (PR-aware, auto-routes to claude-personal)
python3 scripts/bridge.py review codex               # codex review --uncommitted (native subcommand)
python3 scripts/bridge.py review glm-via-opencode    # /review via opencode + GLM-5.1
python3 scripts/bridge.py review kimi-via-opencode   # /review via opencode + Kimi K2.6
```

The `review` subcommand auto-selects the right invocation per provider — slash command for Claude/OpenCode-routed profiles, native `codex review --uncommitted` for Codex.

**Cross-provider review with caller instructions** (each agent runs end-to-end in its own context: review + address findings + commit, per the prompt body):
```sh
python3 scripts/bridge.py review claude -p "address all findings, create per-fix commits, don't push"
python3 scripts/bridge.py review codex  -p "address all findings, create per-fix commits, don't push"
```

For slash-command profiles (claude, glm-via-opencode, kimi-via-opencode), the prompt becomes `/review address all findings, …` over the profile's normal stdin/argv. For codex's review-block profile, it becomes `codex review "address all findings, …"`; bundled codex drops the default `--uncommitted` scope whenever a custom prompt is supplied, so put the intended scope in the prompt text.

**Cross-provider review for-loop (run all four against the same change set):**
```sh
DIR=$(mktemp -d)
for agent in claude codex glm-via-opencode kimi-via-opencode; do
  UUID=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
  python3 scripts/bridge.py review $agent \
    --uuid "$UUID" --output-dir "$DIR" >/dev/null 2>&1
done

# Each review's output lands at <DIR>/<uuid>-<agent>[-<model>].{out,err}.
ls "$DIR"/*.out
```

**For PR-specific review with Claude:**
```sh
python3 scripts/bridge.py run claude -p "/review 123"
```

**For codex review against a base branch / commit (not uncommitted):**
```sh
codex review --base main          # bridge profile only wraps --uncommitted; run codex directly otherwise
codex review --commit HEAD~1
```

**Comparing two agents on the same prompt:**
```sh
BRIDGE='python3 scripts/bridge.py'
PROMPT="Refactor this function for clarity: $(cat src/foo.py)"
DIR=$(mktemp -d)
UUID_A=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
UUID_B=$(python3 -c 'import secrets; print(secrets.token_hex(6))')

echo "$PROMPT" | $BRIDGE run codex \
  --uuid "$UUID_A" --output-dir "$DIR" >/dev/null 2>&1
echo "$PROMPT" | $BRIDGE run glm-via-opencode \
  --uuid "$UUID_B" --output-dir "$DIR" >/dev/null 2>&1

# Paths are deterministic from (uuid, agent[, model]):
diff "$DIR/$UUID_A-codex"*.out "$DIR/$UUID_B-glm-via-opencode"*.out
```

## Caveats

- **Auth is not the bridge's problem.** If `kimi` or `opencode` errors with an auth message, fix that CLI's login separately — don't try to inject env vars through the profile to work around it.
- **Model IDs may need adjustment.** Profile defaults are best-guess strings (`claude-opus-4-7`, `gpt-5.5`, `kimi-k2.6`, `zai-coding-plan/glm-5.1`, `kimi-for-coding/k2p6`). If the CLI errors "unknown model", either pass `-m <correct-id>` for one run or edit the profile JSON to change the default.
- **Effort vocabulary is per-CLI.** `low`/`medium`/`high` are NOT universal — Codex also has `minimal` and `xhigh`; Kimi's effort is a `thinking` / `no-thinking` toggle, not a level. Pick a value the underlying CLI accepts.
- **Streaming depends on the agent.** The bridge tees the agent's stdout/stderr in real time, so anything the agent prints incrementally reaches the terminal as it lands. But many agents (e.g. `claude --print`) buffer their full response before emitting it — in that case the user still sees nothing until completion. The `[bridge:run …]` banner prints immediately so the user knows something started.
- **Exit code is meaningful.** Non-zero from the underlying agent surfaces as the bridge's exit code; treat it like any other failed shell command.
