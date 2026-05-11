# agent-bridge-mini bundle

Self-contained dispatcher that runs any local coding agent CLI (`claude`, `codex`, `kimi`, `opencode`, …) through a JSON profile. Drop a profile in, call `bridge.py`, get the agent's output back. The bridge does not manage credentials, sessions, or models — each agent runs through its native CLI with whatever auth it already has configured.

This bundle is also the home of the [agent-bridge-mini skill](SKILL.md), which teaches AI coding agents how to use the dispatcher. It is the minimalistic implementation of the broader agent-bridge concept (the parent project).

## What's in the bundle

```
.agents/skills/agent-bridge-mini/
├── SKILL.md              # skill manifest (when to trigger, terse)
├── README.md             # this file — full setup docs
├── scripts/
│   └── bridge.py         # dispatcher (stdlib only)
├── assets/
│   └── profiles/         # one JSON profile template per agent
│       ├── claude.json
│       ├── codex.json
│       ├── cursor.json
│       ├── echo.json
│       ├── gemini.json
│       ├── glm-via-opencode.json
│       ├── kimi.json
│       ├── kimi-via-opencode.json
│       └── perplexity-via-opencode.json
└── tests/
    └── test_bridge.py    # unit tests (stdlib unittest)
```

**Where runs.log + per-run capture files live.** Both are written to a per-skill directory under the OS temp dir — **not** inside the bundle:

- macOS: `$TMPDIR/agent-bridge-mini/runs.log` (typically `/var/folders/.../T/agent-bridge-mini/`) and `$TMPDIR/agent-bridge-mini/runs/<uuid>-<agent>[-<model>].{out,err,timeline}`
- Linux: `/tmp/agent-bridge-mini/runs.log` and `/tmp/agent-bridge-mini/runs/<uuid>-<agent>[-<model>].{out,err,timeline}`

Each run produces three capture files: `.out` (agent stdout), `.err` (agent stderr), and `.timeline` (one ASCII line per kernel chunk: `<monotonic_ns> stdout|stderr <byte_count>` — for reconstructing chronological interleaving). Override the runs directory per-run with `--output-dir <path>`; predetermine the UUID with `--uuid <12-hex-chars>`. The directory name (`agent-bridge-mini`) is taken from the bundle dir name, so a renamed bundle gets its own log namespace automatically. The bridge prints absolute paths in the `[bridge:run …]` stderr banner so they're immediately copy-paste-able.

This out-of-bundle layout means:
- read-only bundle installs (under `/opt`, system-wide skills, signed bundles) still work — there's nothing to write next to the script
- the bundle stays clean for version control / symlinking
- the OS handles eventual cleanup of stale logs when temp gets purged (macOS `/var/folders` purges periodically; `/tmp` clears on reboot on most Linux distros)

If you want the audit trail to survive longer than a tmp purge, copy `runs.log` somewhere else periodically — there is no built-in rotation or persistence.

## Requirements

- Python 3.9+
- The native CLI for each agent you want to run, installed and authenticated separately (e.g. `claude`, `codex`, `kimi`, `opencode`).

## Quick start

All commands below run from the repo root.

```sh
# list available profiles
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py list

# show a profile
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py show claude

# sanity check (no agent needed — pipes prompt to cat)
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run echo -p "hello"

# run an agent with an inline prompt
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run claude -p "summarize this repo"

# or pipe the prompt in
echo "write a hello world in rust" | python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run codex

# orchestration: predetermine UUID + output dir so the caller knows
# the capture-file paths before the bridge runs
DIR=$(mktemp -d)
UUID=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run echo \
  --uuid "$UUID" --output-dir "$DIR" -p "hi"
ls "$DIR"   # $UUID-echo.out / .err / .timeline
```

If the path is too long, alias it:

```sh
alias bridge='python3 .agents/skills/agent-bridge-mini/scripts/bridge.py'
bridge list
bridge run claude -p "..."
```

## Commands

| Command | Description |
| --- | --- |
| `bridge.py list` | List all profiles, including their default model and effort. |
| `bridge.py show <agent>` | Print a profile as JSON. |
| `bridge.py run <agent> [-p PROMPT] [-m MODEL] [-e EFFORT] [--uuid HEX] [--output-dir DIR]` | Run an agent. If `-p` is omitted and stdin is piped, the prompt is read from stdin. `-m` / `-e` override the profile defaults for one run. `--uuid` (12 lowercase hex chars) lets the orchestrator predetermine the capture-file UUID; `--output-dir` overrides where captures are written. |
| `bridge.py review <agent> [-p PROMPT] [-m MODEL] [-e EFFORT] [--uuid HEX] [--output-dir DIR]` | Run a code review using the agent's native `/review` slash command (claude / opencode-routed) or its native review subcommand (codex). `-p` (or piped stdin) attaches caller-supplied review instructions, routed natively per profile (extends `/review` for slash-command profiles, appended as the trailing positional for review-block profiles like codex). See [Code review](#code-review-bridge-review-agent) below. `--uuid` and `--output-dir` work the same as for `run`. |
| `bridge.py replay <uuid> [--output-dir DIR] [--tag]` | Reconstruct a prior run's chronological output from its `.timeline` + `.out`/`.err` capture files. Stdout-labeled chunks are written to the caller's stdout, stderr-labeled chunks to stderr — restoring the original FD distinction even when the profile used `merge_streams` (in which case both streams share one on-disk `.out`). `--tag` prefixes each chunk with `[stdout]` / `[stderr]` for visibility when piped to a single sink. See [Replaying a prior run](#replaying-a-prior-run-bridge-replay-uuid) below. |

The bridge exits with the agent's exit code. Failure modes that exit 2 (with a clear stderr message):

- Unknown agent (`bridge run nonsense`).
- Forcing a flag the profile doesn't support (e.g. `bridge run echo -e high`; `effort_args` is `null`).
- Calling `run` without `-p` or piped stdin — the bridge errors fast rather than letting the underlying CLI hang.
- A malformed profile JSON — bad JSON, missing/empty `command`, a `review` block missing its own `command`, a non-dict `env`, a non-string `cwd`, a `model`/`effort` default paired with its `*_args` set to null, or any `*_args` template that omits the `{value}` placeholder — fails at load time with the offending file name.
- `--uuid` not matching `^[0-9a-f]{12}$`, or naming a UUID whose target capture file already exists with content (refuse-to-clobber).

## Bundled profiles

| Name | Binary + base args | Default model | Default effort | Effort flag syntax |
| --- | --- | --- | --- | --- |
| `echo` | `cat` | — | — | n/a (no model/effort) |
| `claude` | `claude --print` | `claude-opus-4-7` | `xhigh` | `--effort {value}` — vocabulary is per model (see below) |
| `codex` | `codex exec` | `gpt-5.5` | `high` | `-c model_reasoning_effort={value}` (low / medium / high / xhigh). Has a `review` block — `bridge review codex` invokes `codex review --uncommitted`. |
| `cursor` | `cursor-agent --print --output-format text` | `composer-2` | — | No effort flag in cursor-agent; reasoning is intrinsic to Composer 2 |
| `gemini` | `gemini` | `gemini-3-pro` | — | No effort flag in gemini CLI; reasoning isn't user-tunable |
| `kimi` | `kimi --print` | `kimi-k2.6` | `thinking` | `--{value}` — accepts `thinking` (renders `--thinking`) or `no-thinking` (renders `--no-thinking`) |
| `kimi-via-opencode` | `opencode run` | `kimi-for-coding/k2p6` | — | OpenCode has no effort flag for the kimi-for-coding provider — pick a thinking variant via `-m` if needed |
| `glm-via-opencode` | `opencode run` | `zai-coding-plan/glm-5.1` | — | OpenCode controls reasoning via model variants, not a flag — `effort_args` is `null` here |
| `perplexity-via-opencode` | `opencode run` | `perplexity-ai/sonar-pro` | — | No effort flag — pick a model variant for reasoning depth |

Model IDs are best-guess based on each provider's current naming. If a CLI errors with "unknown model", edit the one string in the profile JSON.

### Switching binaries

The first element of `command` is the executable. To use a different binary — e.g. a wrapper script or a sibling install — just point a profile at it:

```json
{
  "description": "Some custom Claude wrapper",
  "command": ["claude-wrapper", "--print"],
  ...
}
```

Naming convention: profiles routed through OpenCode follow `<model>-via-opencode` (e.g. `glm-via-opencode`, `kimi-via-opencode`). Native-CLI profiles use just the agent name (e.g. `claude`, `codex`).

Common reasons to fork a profile:
- Different binary (custom wrapper script)
- Same binary but different defaults (e.g. a `claude-fast` profile that defaults to Sonnet + max instead of Opus + xhigh)
- Different `cwd` or `env` (e.g. point one OpenCode profile at a project-specific config dir)

Delete any bundled variant you don't have on PATH — it will fail with `command not found: ...` on first run otherwise.

### Available Claude models

| Model ID | Effort levels |
| --- | --- |
| `claude-opus-4-7` (default) | `low` / `medium` / `high` / `xhigh` / `max` |
| `claude-sonnet-4-6` | `low` / `medium` / `high` / `max` |
| `claude-haiku-4-5` | none — Haiku does not take an effort flag. To run it, either edit the profile to remove the `effort` default (recommended) or pass `-e ''` per run (empty effort skips the flag — works as an escape hatch but easy to forget) |

Examples:
```sh
bridge run claude -p "..." -m claude-sonnet-4-6 -e high
bridge run claude -p "..." -m claude-opus-4-7   -e max
```

### Available GLM models (via OpenCode)

This setup uses the **Z.AI Coding Plan** tier, so OpenCode's provider ID is `zai-coding-plan`, and the `--model` flag takes `zai-coding-plan/<model-id>`:

| Display name | OpenCode model string |
| --- | --- |
| GLM-5.1 (default) | `zai-coding-plan/glm-5.1` |
| GLM-5-Turbo | `zai-coding-plan/glm-5-turbo` |
| GLM-5V-Turbo | `zai-coding-plan/glm-5v-turbo` |
| GLM-4.5-Air | `zai-coding-plan/glm-4.5-air` |
| GLM-4.7 | `zai-coding-plan/glm-4.7` |

```sh
bridge run glm-via-opencode -p "..." -m zai-coding-plan/glm-4.7
bridge run glm-via-opencode -p "..." -m zai-coding-plan/glm-4.5-air
```

If you also have a direct Z.AI API tier registered, the prefix would be `zai/` instead. Confirm with `cat ~/.local/share/opencode/auth.json` (look at the top-level keys) or `opencode /models`.

OpenCode controls reasoning via model variants, not a CLI flag, so this profile sets `effort_args: null` — `bridge run glm-via-opencode -e ...` will error.

### Kimi thinking mode

`kimi-k2.6` supports thinking-mode toggling via two CLI flags:

| Effort value | Renders | Effect |
| --- | --- | --- |
| `thinking` (default) | `--thinking` | Reasons before answering — slower but better on complex problems. |
| `no-thinking` | `--no-thinking` | Skips the reasoning step — faster, lighter. |

```sh
# default — thinking on
bridge run kimi -p "Refactor this loop for clarity."

# disable thinking for a quick pass
bridge run kimi -p "What does this regex match?" -e no-thinking
```

Note: some Kimi model variants (e.g. `kimi-k2-thinking`) always think and ignore the flag. K2.6 honors both settings.

### Kimi via OpenCode

This setup reaches Kimi via the `kimi-for-coding` provider (Moonshot's coding-plan tier). The bundled `kimi-via-opencode` profile uses `opencode run --model kimi-for-coding/k2p6`. Two reasons to use this instead of the native `kimi` profile:

- You already have OpenCode set up for billing/auth and don't want a second CLI.
- You want to mix Kimi calls with other OpenCode-routed models in the same session/log.

Supported model IDs (via OpenCode's `kimi-for-coding` provider):

| OpenCode model string | Notes |
| --- | --- |
| `kimi-for-coding/k2p5` | Kimi K2.5 |
| `kimi-for-coding/k2p6` (default) | Kimi K2.6 — togglable thinking, but OpenCode can't toggle (see caveat below) |
| `kimi-for-coding/kimi-k2-thinking` | Always-thinking variant — ignores any thinking flag by design |

Other Kimi IDs (e.g. `kimi-k2.6`, `kimi-k2-thinking-turbo`) are not registered in this provider and will error.

Caveats:
- OpenCode does NOT expose Kimi's `--thinking` toggle. To force thinking, override the model to `kimi-for-coding/kimi-k2-thinking` (`bridge run kimi-via-opencode -p "..." -m kimi-for-coding/kimi-k2-thinking`). The native `kimi` profile is still the cleanest way to flip thinking on/off mid-session.
- Auth: `kimi-via-opencode` uses the default `opencode` config (`~/.local/share/opencode/auth.json`); `kimi-for-coding` must be registered there.

### Routing any provider through OpenCode

OpenCode supports 75+ providers. Any of them is reachable by setting `command` to `["opencode", "run"]` and `model` to `<provider>/<model-id>`. The provider name has to match an entry in your `~/.local/share/opencode/auth.json`. Examples for this setup:

```json
{ "model": "anthropic/claude-opus-4-7" }
{ "model": "openai/gpt-5.5" }
{ "model": "kimi-for-coding/k2p6" }
{ "model": "zai-coding-plan/glm-4.7" }
```

Use `opencode /models` (after `/connect`) to discover the exact strings your local install accepts.

### Cursor (Composer 2) via cursor-agent

The `cursor` profile is **locked to Composer 2 (classic)** — `--model composer-2` is baked into the command, and `model_args` is `null` so `bridge run cursor -m <other>` errors out clearly. There's no reasoning/effort slider for Composer 2 (`effort_args: null`).

```sh
bridge run cursor -p "..."           # always composer-2 (classic)
bridge run cursor -p "..." -m foo    # exit 2: profile does not support --model
```

Cursor's CLI uses `--print` for non-interactive output and accepts the prompt as a positional argument. The bundled profile uses `prompt_mode: "arg"`, so prompts that start with `-` are safely separated with `--`.

**Why classic and not `composer-2-fast`?** The two variants have the same intelligence per Cursor's docs — `-fast` is a pure latency optimization. Locking to the classic variant is the conservative default; if you want the fast variant later, edit the `--model` value in `cursor.json`.

**Why locked at all?** `cursor-agent` has a known bug ([forum thread](https://forum.cursor.com/t/cursor-cli-agent-model-ignores-reasoning-thinking-level-suffix/159748)) where the `--model` flag silently strips reasoning/thinking suffixes (e.g. `gpt-5.3-codex-xhigh` → `gpt-5.3-codex`). Until Cursor fixes it, exposing model overrides for this profile would offer broken-by-default routing. Composer 2 has no suffixes, so locking it sidesteps the issue.

If you want a separate profile that exposes other Cursor-routed models (with the suffix caveat), copy `cursor.json` to `cursor-other.json`, drop the baked `--model`, restore `model_args: ["--model", "{value}"]`, and accept that reasoning levels won't apply.

### Available Codex models

`gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`. Pick any of these with `-m` (e.g. `bridge run codex -p "..." -m gpt-5.4-mini -e medium`).

## Selecting model and effort per run

```sh
# use the profile defaults
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run claude -p "..."

# override the model for this run only
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run claude -p "..." -m claude-opus-4-7

# override the reasoning effort
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run codex  -p "..." -e xhigh
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run claude -p "..." -e high
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run kimi   -p "..." -e no-thinking

# both at once
python3 .agents/skills/agent-bridge-mini/scripts/bridge.py run codex -p "..." -m gpt-5 -e high
```

To change the default for an agent permanently, edit the `model` / `effort` fields in its profile JSON.

## Profile schema

Each agent is a JSON template file under assets/profiles. The filename (minus `.json`) is the agent name.

```json
{
  "description": "<human-readable label>",
  "command": ["<binary>", "<arg>", "..."],
  "prompt_mode": "stdin",
  "model": "default-model-id",
  "effort": "medium",
  "model_args": ["--model", "{value}"],
  "effort_args": ["--effort", "{value}"],
  "prompt_args": ["-p", "{value}"],
  "env": { "OPTIONAL_VAR": "value" },
  "cwd": "optional/working/dir"
}
```

| Field | Required | Description |
| --- | --- | --- |
| `command` | yes | Base argv list. The bridge runs this verbatim, then appends `model_args`, `effort_args`, and finally the prompt (per `prompt_args` or `prompt_mode`). |
| `prompt_mode` | no (default `stdin`) | `stdin` pipes the prompt into the process; `arg` appends the prompt as the last argv element. Ignored when `prompt_args` is set. |
| `prompt_args` | no | Argv template for CLIs that take the prompt through a value-bearing flag. When set, takes precedence over `prompt_mode`. |
| `review` | no | Optional override for the `bridge review <agent>` subcommand. Block has `command` / `model_args` / `effort_args` / optional `scope_default` — when present, the bridge uses them instead of the profile's main command and sends no framing prompt by default (the underlying CLI's native review extracts scope from git). When absent, `bridge review` uses the profile's main command + sends `/review` as the prompt. Caller-supplied review text via `bridge review … -p "<text>"` (or piped stdin) is appended as the trailing positional argument for review-block profiles or extends the slash framing as `/review <text>` for the default path. `scope_default` is only added when no caller prompt is supplied; bundled `codex.json` therefore runs `codex review --uncommitted` by default, but `bridge review codex -p "<text>"` runs `codex review "<text>"`. Default model/effort always come from the top-level profile (so `bridge review` matches `bridge run` defaults unless overridden by `-m`/`-e`); the review block only overrides the *flags* used to pass them. |
| `description` | no | Shown by `bridge.py list`. |
| `model` | no | Default model ID for this agent. Used unless overridden by `bridge run -m`. |
| `effort` | no | Default reasoning effort for this agent. Used unless overridden by `bridge run -e`. |
| `model_args` | no (default `["--model", "{value}"]`) | Argv template added when a model is set. `{value}` is replaced by the model. Set to `null` to disable model selection for this profile. |
| `effort_args` | no (default `["--effort", "{value}"]`) | Argv template added when effort is set. `{value}` is replaced by the effort. Set to `null` to disable effort selection. |
| `skill_format` | no (default `"/skill:{name}"`) | Per-agent template applied to **every** `/skill:<name>` reference in the prompt (leading or in the body). The lookbehind `(?<![\w/])` skips embedded refs like `path/skill:foo`, so URLs and paths are safe. `{name}` is replaced by the captured skill name. Use `"/{name}"` for Claude/OpenCode/Cursor, `"${name}"` for Codex, or omit it for native Kimi (which already uses `/skill:<name>`). Must contain `{name}`. The rewrite scope matches the `skills` field in `runs.log` exactly. |
| `env` | no | Extra env vars merged onto the inherited environment. Values pass through `os.path.expandvars` then `expanduser`, so `$HOME` / `~` / `$VAR` work — be aware that any `$NAME` in the value will be substituted from the parent process env. |
| `cwd` | no | Working directory for the agent. Tilde (`~`) is expanded. |
| `merge_streams` | no (default `false`) | When `true`, the agent's stdout and stderr are interleaved into a single `.out` capture file in arrival order; no separate `.err` file is created. The terminal display still routes stdout chunks to stdout and stderr chunks to stderr, and the `.timeline` sidecar still labels each chunk by its original FD — `bridge replay` uses those labels to restore the FD distinction. Useful for CLIs that render the bulk of their visible output to FD2 (opencode-routed profiles, `codex exec`), where the conventional split between "answer" and "noise" doesn't match the actual stream usage. The audit log's `output_stderr` is `null` and `merge_streams: true` is recorded so consumers can tell merged from non-merged captures apart. |

### Adding a new agent

1. Drop a JSON file into the assets/profiles directory (e.g. `assets/profiles/echo.json` for a new agent named `echo`).
2. Set `command` to the agent's non-interactive invocation, *without* the model/effort flags.
3. Pick how the CLI receives the prompt: `prompt_mode: stdin` for stdin input, `prompt_mode: arg` for a positional argument, or `prompt_args: ["-p", "{value}"]` for prompt-via-flag CLIs.
4. Set `model` and `effort` to sensible defaults. Override `model_args` / `effort_args` only if the CLI uses non-default flag names (e.g. codex uses `-c model_reasoning_effort=...`).
5. `bridge.py list` confirms it's picked up.

The bridge never reads or writes the agent's auth — sign in to each CLI the way that CLI expects.

## Code review (`bridge review <agent>`)

The `review` subcommand provides a uniform entry point for code reviews across providers, hiding the per-provider mechanism:

```sh
bridge review claude              # /review via claude (PR-aware)
bridge review codex               # codex review --uncommitted -c model=… -c model_reasoning_effort=…
bridge review glm-via-opencode    # opencode run --model zai-coding-plan/glm-5.1 "/review"
bridge review kimi-via-opencode   # opencode run --model kimi-for-coding/k2p6 "/review"
```

### How it picks the right invocation

The bridge looks at the resolved profile's `review` block:

| Profile has `review` block? | Default behavior (no `-p`) | With caller `-p "<text>"` |
| --- | --- | --- |
| **Yes** (only `codex.json` currently) | Uses `review.command` instead of the main command. Applies `review.model_args` / `review.effort_args` overrides and appends `review.scope_default` when present (for bundled codex, `--uncommitted`). Sends NO prompt (codex's native review derives scope from git). | Same command with `review.scope_default` omitted, then `<text>` appended as the trailing positional argument (`codex review "<text>"` for bundled codex). |
| **No** (claude, opencode-routed, etc.) | Uses the profile's main command. Applies normal `model_args` / `effort_args`. Sends `/review` via the profile's normal `prompt_mode` (stdin for claude/kimi, arg for opencode-routed). | Same channel, but the prompt becomes `/review <text>` — the slash-command CLI accepts extra context after `/review` natively. |

`/skill:<name>` references inside `<text>` are translated per the profile's `skill_format`, same as `bridge run`. The audit log's `prompt` field stores the assembled prompt as the agent received it (so `/review …` for slash-command profiles, the bare caller text for review-block profiles). Caller-supplied text in `command` is redacted to `<prompt>`; the bare `/review` framing without `-p` stays visible to keep the log readable.

### Cross-provider review pattern

Get four independent reviews of the same changes by looping. Don't redirect to ad-hoc filenames — pass `--uuid` and `--output-dir` so the orchestrator owns the paths up front:

```sh
DIR=$(mktemp -d)
for agent in claude codex glm-via-opencode kimi-via-opencode; do
  UUID=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
  bridge review $agent --uuid "$UUID" --output-dir "$DIR" >/dev/null 2>&1
done

# Each review's stdout/stderr is at $DIR/<uuid>-<agent>[-<model>].{out,err}.
ls "$DIR"/*.out
```

Different providers catch different issues. In practice this caught logging bugs (Codex), a missing-`command` `KeyError` (GLM), and stylistic concerns the others didn't (Kimi).

### Cross-provider review-and-fix (each agent runs end-to-end in its own context)

When you want each agent to review *and* address findings inside its own context (instead of having the orchestrator collect and merge), pass the workflow as the review prompt — the bridge routes it natively to each provider:

```sh
PROMPT='address all the findings; create multiple Git commits grouped by context; do not add Co-Authored-By trailers; do not push to origin'

DIR=$(mktemp -d)
for agent in claude codex glm-via-opencode kimi-via-opencode; do
  UUID=$(python3 -c 'import secrets; print(secrets.token_hex(6))')
  bridge review "$agent" -p "$PROMPT" --uuid "$UUID" --output-dir "$DIR"
done
```

Each agent sees its native review entry plus the caller's instructions: claude/glm/kimi receive `/review address all the findings…`, codex receives `codex review "address all the findings…"`. For bundled codex, the default `--uncommitted` scope is dropped whenever a custom prompt is supplied; put the intended scope in the prompt text. Run them serially (as above) when later agents should build on earlier commits, or wrap the loop with branch resets if you want each agent to start from the same baseline.

### When to bypass `bridge review` and run directly

- **PR-specific Claude review:** `bridge run claude -p "/review 123"` — passes the PR number to `/review`.
- **Codex review against a base branch or commit:** `codex review --base main` / `codex review --commit HEAD~1` — the profile only wraps `--uncommitted`.
- **Free-form text review** (paste a function, no git context): `bridge run claude -p "Act as a reviewer. Review this: ..."` — manual prompt.

## Replaying a prior run (`bridge replay <uuid>`)

A bridge run produces three capture files: `.out` (stdout), `.err` (stderr), and `.timeline` (one ASCII line per kernel chunk: `<monotonic_ns> stdout|stderr <byte_count>`). `bridge replay` walks the timeline and reconstructs the chronological output by reading N bytes per entry from the matching capture file:

```sh
# default: route each chunk to caller's stdout/stderr per its FD label
bridge replay 345d4fd2cafa
bridge replay 345d4fd2cafa --output-dir /custom/runs

# tagged: prefix each chunk with [stdout]/[stderr] for inspection in a single sink
# (stderr chunks still go to FD2 — combine with `2>&1` to put both in one file)
bridge replay 345d4fd2cafa --tag > merged.txt 2>&1
```

Two situations where this is useful:

1. **Reviewing a prior run after the fact.** The original terminal interleaving is preserved by the `.timeline` sidecar; `replay` puts it back together exactly as the user originally saw it. `cat .out .err` can't do this — the streams arrive in unrelated orders.
2. **Reading a `merge_streams: true` capture without losing FD distinction.** When a profile sets `merge_streams`, both streams write into one `.out` file in arrival order (no `.err` file exists). The `.timeline` still labels each chunk by its original FD, so `replay` routes stdout chunks to caller stdout and stderr chunks to caller stderr — restoring the distinction the merged on-disk file lost.

Failure modes:
- `uuid` arg not 12 lowercase hex chars → exit 2.
- No `.timeline` file for that UUID in the runs dir → exit 2 with a message naming the dir.
- No `.out` file (highly unusual; would mean the run never wrote anything) → exit 2.
- Multiple non-empty capture stems share the UUID (e.g. a re-used UUID resolved to a different agent/model the second time and both runs left content on disk) → exit 2 listing the conflicting stems. Replay refuses to guess which run you meant.
- No non-empty stems but multiple empty stems share the UUID (rare; both runs aborted before any byte was written) → exit 2 listing the stems.
- `.err` is empty AND the timeline contains stderr entries AND `.out` size doesn't match the total timeline byte count → exit 2. The state is internally inconsistent — neither a normal capture (would need a non-empty `.err`) nor a merged retry that overwrote the same stem (would need `.out` to hold every timeline byte). Replay refuses rather than emit garbled output.

Replay reads the timeline as ASCII, then reads N bytes per entry from the appropriate capture file. Sorting strategy depends on capture shape:

- **Normal capture** — entries are sorted by ts before replay. The two tee threads write timeline lines under separate locks, so file order isn't necessarily chronological; sorting by ts recovers it (see [`.timeline` semantics](#output-timing-and-stability-semantics) below).
- **Merged capture (`merge_streams: true`)** — entries are walked in file order, NOT sorted. The bridge holds a single lock around the capture+timeline writes when merging, so timeline file order IS the on-disk byte order; re-sorting by ts would scramble byte allocation across the merged `.out`. Replay detects this mode by the **absence** of `.err` (the bridge skips creating it in merge mode). An empty `.err` from a normal run with no stderr output is treated as non-merged, which is the safe classification — merge mode never leaves an empty `.err` behind.

Bytes that go past the file's actual size (e.g. the timeline overcounts) are silently skipped — replay doesn't try to validate timeline/capture consistency.

### Why merging streams isn't the default

The conventional FD split (stdout = the answer, stderr = diagnostics) is honored by some agent CLIs (`claude --print`, native `kimi --print`, `echo`) and not others. Profiles that route through OpenCode (`glm-via-opencode`, `kimi-via-opencode`) and `codex exec` write their UI rendering, tool calls, file diffs, and progress narration to FD2 — material that often *is* the content the user wants to read. For those profiles, the on-disk `.err` file is large and `.out` is small, which can be surprising.

`merge_streams: true` is opt-in because:

- It changes the on-disk capture contract for that profile (no `.err` file). External tooling that grep'd `*.err` will need to read `*.out` instead.
- The audit log's `output_stderr` becomes `null`. Consumers who relied on that field being a path will need to handle `null` — `merge_streams: true` in the same record signals why.
- Profiles where the FD split *is* meaningful (claude, native kimi) lose the diagnostic-only `.err` file under merging, which makes log triage harder for those agents.

When you do enable it on a profile (typically the opencode-routed or codex profiles), `bridge replay` is the recommended way to read back the chronological output with the FDs visually distinguished — `cat .out` works too but loses the stream labels.

## Run log and per-run output

Every dispatched run produces these:

1. **A short UUID** (12 hex chars from `uuid4()`) that identifies the run end-to-end. Used in the banners, the per-run capture file name, and the `runs.log` entry.
2. **Three per-run capture files** at `<runs-dir>/<uuid>-<agent>[-<model>].{out,err,timeline}`. Default `<runs-dir>` is `<tempdir>/agent-bridge-mini/runs/` (see the file-tree section above for `<tempdir>` resolution); override per-run with `--output-dir <path>`. `.out` is the agent's stdout verbatim, `.err` is stderr verbatim, `.timeline` is a tiny ASCII sidecar (`<monotonic_ns> stdout|stderr <byte_count>` per kernel chunk) that lets a downstream tool reconstruct chronological interleaving when it matters. Both streams also tee through to the caller's terminal in real time. Two parallel `bridge run` invocations get different UUIDs and different files — they never overwrite each other.
3. **Two stderr banners** so you can see the run start and finish even when the agent itself stays quiet:
   ```
   [bridge:run uuid=345d4fd2cafa agent=claude model=claude-opus-4-7 effort=xhigh stdout=/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-claude-opus-4-7.out stderr=/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-claude-opus-4-7.err]
   ... agent output streams here ...
   [bridge:done uuid=345d4fd2cafa exit=0 duration=1.42s]
   ```
   Banners go to stderr so they don't pollute stdout redirection (`bridge run claude > out.txt` still gives you only the agent's stdout). The `stdout=` and `stderr=` values are absolute paths so you can paste them straight into `cat`/`tail`.

The JSONL audit line in `runs.log` (at `<tempdir>/agent-bridge-mini/runs.log`) carries the UUID, the original prompt, the list of skill references found in it, and a back-reference to the capture file:

```json
{"ts": 1714972800.123, "uuid": "345d4fd2cafa", "action": "run", "agent": "claude", "model": "claude-opus-4-7", "effort": "xhigh", "prompt": "/skill:review tweak this loop", "skills": ["review"], "command": ["claude", "--print", "--model", "claude-opus-4-7", "--effort", "xhigh"], "exit": 0, "duration_s": 1.42, "output_stdout": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-claude-opus-4-7.out", "output_stderr": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-claude-opus-4-7.err", "output_timeline": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-claude-opus-4-7.timeline", "merge_streams": false}
```

When the profile sets `merge_streams: true`, `output_stderr` is `null` (no separate `.err` file is created) and `merge_streams` is `true`.

`prompt` holds the **original** prompt the caller provided — before skill_format rewriting, exactly as you typed it (or piped it in). `skills` is every `/skill:<name>` reference found in that prompt, deduplicated and in first-seen order (empty list when none). `bridge review` records `"prompt": "/review"` for the default path and `"prompt": null` for the review-block path (codex's native review derives scope from git, sends no prompt).

Pre-flight failures (unknown agent, missing prompt, malformed profile, missing `cwd`, `-m`/`-e` against a `null`-args profile) exit 2 *without* writing a log line or a capture file — only invocations that reached `subprocess.Popen` are recorded (including `FileNotFoundError`/`PermissionError`, which surface as exit 127/126).

`agent` is the profile the bridge dispatched. `action` is `"run"` or `"review"` so you can filter with `jq 'select(.action=="run")'`. To find the capture files for a specific run, grep by uuid: `jq -r 'select(.uuid=="345d4fd2cafa") | "\(.output_stdout)\n\(.output_stderr)"' runs.log`.

### Predicting capture-file paths (for orchestrators)

The full path is deterministic from the inputs:

```
<output-dir>/<uuid>-<agent>[-<sanitized-model>].{out,err,timeline}
```

**Filename sanitization** applies to BOTH the agent name and the model name: every char outside `[A-Za-z0-9._-]` is collapsed to `_`, and each component is truncated at 80 chars. So `zai-coding-plan/glm-5.1` becomes `zai-coding-plan_glm-5.1` in the filename. Agent names use the safe charset by convention, so the agent component is usually a no-op; model components routinely contain `/` and trigger the rewrite. The `<model>` component is omitted entirely when the profile has no model (e.g. `echo`, `cursor`).

**Robust glob (always works regardless of sanitization):**

```sh
ls "$DIR/$UUID-"*.out   # the UUID alone is unique; one file per run
```

**Empty leftovers don't block reuse.** If a prior aborted run touched files with the same UUID but never wrote to them (size 0), the bridge silently overwrites — only non-empty existing files trigger refuse-to-clobber.

**Partial cleanup is asymmetric.** On a Popen-style failure (FileNotFoundError, PermissionError, OSError), the bridge unlinks only the empty capture files. So if the agent wrote to stdout before crashing, `.out` survives but `.err` and `.timeline` are gone. The audit record's `output_stdout` / `output_stderr` / `output_timeline` are all set to `null` regardless — they advertise the bridge-managed contract, not "is the file currently on disk." If you need the surviving partial output, glob `$DIR/$UUID-*` directly rather than trusting the audit record.

**`--output-dir` path expansion.** The path passes through `os.path.expandvars` then `expanduser`, so `~/runs` and `$HOME/orchestrator-runs/$JOB_ID` both work. The dir is created with `parents=True` if missing. If it's unwritable (read-only mount, permission denied), the bridge prints a one-line stderr warning, falls back to inherited stdio (no capture files), and sets all three `output_*` fields to `null` in the audit record — the agent still runs. **In passthrough mode, `--uuid` is still recorded in the audit log but no capture files exist** — paths the orchestrator pre-computed from the UUID won't be on disk. Detect this by checking `output_stdout: null` in the audit record before reading the expected files.

### Output, timing, and stability semantics

**`.timeline` semantics.** Each line is `<monotonic_ns> stdout|stderr <byte_count>` recording one `read1(4096)` chunk (one kernel read) — NOT one output line. A long agent line is split across multiple entries; many short lines collapse into one.

Pre-sort behavior depends on capture mode:

- **Non-merge** (default): two tee threads write timeline entries under separate locks, so lines may appear out of ts order under contention. Consumers must `sort -n` to recover chronological order. Each capture file (`.out`, `.err`) has a single writer with its own cursor, so re-sorting affects only display order, not byte allocation.
- **Merged** (`merge_streams: true`): a single lock covers both the capture write and the timeline write, so timeline file order IS the chronological order AND matches the byte order of the merged `.out`. Do NOT `sort -n` a merged-capture timeline if you plan to allocate bytes from `.out` per entry — the file-order invariant is the contract `bridge replay` relies on, and re-sorting would scramble byte allocation.

**Timestamp basis.** The `.timeline` uses `time.monotonic_ns()` (no wall-clock anchor; not comparable across processes or with the audit record). The audit record's `ts` is wall-clock (`time.time()`, seconds since epoch); `duration_s` is monotonic-derived (immune to NTP / DST jumps). Don't try to correlate `.timeline` entries with audit `ts` — different clock domains.

**No agent timeout.** The bridge waits indefinitely for the subprocess to exit. A hung agent hangs the bridge. If you need bounded execution, wrap the call in `timeout(1)` or your orchestrator's own kill switch — the bridge intentionally doesn't impose a deadline because reasonable bounds vary widely across CLIs and tasks.

**`--uuid` entropy expectation.** The clobber check ("does a non-empty file with this UUID already exist?") is TOCTOU-racy: two parallel processes can both pass the check before either writes. With cryptographically random UUIDs (`secrets.token_hex(6)` → 48 bits), collision probability is ~2^-48 — never observed in practice. If you generate UUIDs sequentially or from low-entropy sources (timestamp, counter), the race becomes real; don't.

**The stderr banner is human-readable, not a parseable contract.** `[bridge:run uuid=… stdout=… stderr=…]` and `[bridge:done uuid=… exit=… duration=…s]` exist so a human watching the terminal can see what's happening; their format may evolve. Orchestrators should use `--uuid` + `--output-dir` to predetermine paths, or query `runs.log` (which IS a stable JSON contract) — never grep the banner.

### Platform notes and edge cases

**Signal handling.** SIGINT (Ctrl+C) is caught explicitly: the bridge sends SIGTERM to the child, escalates to SIGKILL after a 2-second wait, then exits 130. The audit record is NOT written for SIGINT — the run was interrupted before completion, and capture files keep whatever was already streamed (no rollback). Other signals (SIGTERM, SIGHUP, SIGQUIT) use Python defaults: they propagate to the process group, the child likely dies, the bridge dies, and no audit record is written.

**File permissions on capture files.** The bridge creates `.out`, `.err`, `.timeline`, and `runs.log` with the parent process's umask (typically `0o022` → `-rw-r--r--`). It does NOT chmod after creation. If you need stricter permissions for sensitive prompts or output, set umask in the calling shell or wrap the invocation: `(umask 0077; bridge run ...)`.

**`--output-dir` pointing at a file.** When `--output-dir` resolves to an existing file (not a directory), `Path.mkdir` raises `FileExistsError`, the bridge prints a one-line warning, falls back to passthrough mode, and sets all `output_*` fields to `null` — same outcome as "unwritable dir."

**Windows.** Bridge is macOS-tested and should work on Linux. On Windows, `fcntl` is unavailable and the bridge silently degrades to unsynchronized `runs.log` appends — concurrent dispatchers writing prompts >8KB may interleave records. Path handling, env-var expansion, `subprocess.Popen` with PIPE, and the threaded tee should all be portable, but nothing is tested on Windows; treat as best-effort.

**Memory and disk bounds.** No limit on prompt size, agent output, or `runs.log` size. The prompt is held in memory once (subprocess stdin write) and also persisted verbatim in `runs.log` (no redaction in the `prompt` field). Agent output is streamed in 4096-byte chunks rather than buffered, so multi-GB outputs don't grow parent-process memory — but they DO grow `<output-dir>` and may fill the disk. There is no log rotation; if you run high-volume orchestration, truncate `runs.log` and clean `<output-dir>` periodically.

**TTY-requiring agents.** The bridge passes `subprocess.PIPE` for stdin/stdout/stderr — never a TTY. Agents that check `isatty(0)` and switch to non-interactive mode work fine. Agents that REQUIRE a TTY (some interactive REPLs) will either error out, refuse to start, or read directly from `/dev/tty` (bypassing the bridge's stdin pipe entirely). Pick a profile that uses the agent's documented non-interactive mode (e.g., `claude --print`, `kimi --print`, `codex exec`, `cursor-agent --print --output-format text`) — all bundled profiles already do this.

**Read-only or non-writable temp.** If the per-skill runs directory can't be created (e.g. tmp is full or mounted read-only), the bridge prints a one-line warning, falls back to inherited stdio (no capture files), and sets `output_stdout` / `output_stderr` / `output_timeline` to `null` in the JSON record. The agent still runs. Because captures default to OS tmp rather than the bundle, a read-only bundle install (under `/opt`, signed bundle, etc.) is no longer a blocker.

**Prompt redaction in `command`.** When the prompt would otherwise appear in argv (`prompt_mode: "arg"` profiles such as cursor or opencode-routed agents, or `prompt_args` profiles), the bridge replaces it with `"<prompt>"` in the logged `command` so it's not duplicated next to the dedicated `prompt` field. `prompt_mode: "stdin"` prompts are sent over stdin and never enter argv to begin with. Internal prompts like the literal `/review` string sent by `bridge review` are NOT redacted in `command`. Note: the per-run capture file under `runs/` contains the agent's *output*, not its prompt — but if the agent echoes the prompt back, that text is preserved verbatim.

**Privacy note.** The dedicated `prompt` field stores the prompt verbatim — there is no redaction. If you treat prompts as sensitive (PII, internal docs, secrets), grep/strip the field before sharing `runs.log`, or truncate it (`: > runs.log`) periodically. The bundle keeps `runs.log` and `runs/` under the local `.gitignore` by default, but anything you commit explicitly will publish prompts.

**Argv separator for `prompt_mode: "arg"`.** When the prompt starts with `-`, the bridge inserts a literal `--` before it (`["...", "--", "-h"]`), so the underlying CLI treats it as a positional argument rather than a flag. The `--` is visible in `runs.log` as part of `command`.

**No log rotation.** Both `runs.log` and `runs/` grow until the OS purges tmp. Truncate (`: > $TMPDIR/agent-bridge-mini/runs.log`) or wipe (`rm $TMPDIR/agent-bridge-mini/runs/*.log`) manually if you want them gone sooner — or copy them out to a persistent location if you want them to survive a tmp purge. Nothing the bridge writes lives inside the bundle anymore, so `git status` on the bundle stays clean by construction.

## Running tests

Unit tests live in `tests/` and use stdlib `unittest`. Run from the bundle root:

```sh
cd .agents/skills/agent-bridge-mini
python3 -m unittest discover -s tests
```

Coverage is intentionally minimal (the dispatcher is small, most behavior is exercised by the bundled profiles in real use). Add a test alongside any new validation rule or argv-construction tweak.

## Skill for AI agents

[`SKILL.md`](SKILL.md) is the trigger-focused manifest that teaches an AI coding agent when and how to use this bridge. The bundle lives under `.agents/skills/` (provider-agnostic) rather than `.claude/skills/`, so:

- **Claude Code** does NOT auto-discover it from this path. To make Claude Code load it, either symlink `.claude/skills/agent-bridge-mini` → `../../.agents/skills/agent-bridge-mini`, or copy the bundle into `.claude/skills/`. Alternatively, an agent can load `SKILL.md` directly via the `Read` tool.
- **Other agents** (codex, opencode, kimi, etc.) — point them at `.agents/skills/agent-bridge-mini/SKILL.md` via their own context-loading mechanism. The plain-markdown format is portable.
