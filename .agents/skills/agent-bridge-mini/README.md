# agent-bridge-mini bundle

Self-contained dispatcher that runs any local coding agent CLI (`claude`, `codex`, `kimi`, `opencode`, …) through a JSON profile. Drop a profile in, call `bridge.py`, get the agent's output back. The bridge does not manage credentials, sessions, or models — each agent runs through its native CLI with whatever auth it already has configured.

This bundle is also the home of the [agent-bridge-mini skill](SKILL.md), which teaches AI coding agents how to use the dispatcher. It is the minimalistic implementation of the broader agent-bridge concept (the parent project).

## What's in the bundle

```
.agents/skills/agent-bridge-mini/
├── SKILL.md              # skill manifest (when to trigger, terse)
├── README.md             # this file — full setup docs
├── scripts/
│   ├── bridge.py         # dispatcher (stdlib only)
│   └── context.py        # detect orchestrator subscription (personal/work/empty)
├── assets/
│   └── profiles/         # one JSON profile template per agent
│       ├── claude.json
│       ├── claude-personal.json
│       ├── claude-work.json
│       ├── codex.json
│       ├── cursor.json
│       ├── echo.json
│       ├── glm-via-opencode.json
│       ├── glm-via-opencode-personal.json
│       ├── glm-via-opencode-work.json
│       ├── kimi.json
│       ├── kimi-via-opencode.json
│       ├── kimi-via-opencode-personal.json
│       └── kimi-via-opencode-work.json
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
- For Claude personal/work routing: per-subscription config dirs at `$HOME/.claude-personal` and `$HOME/.claude-work`. The bridge profiles set `CLAUDE_CONFIG_DIR` to pick which one — no separate `claude-personal` / `claude-work` binaries are needed.
- For OpenCode personal/work routing: a per-XDG-profile config under `~/.local/share-personal/opencode/` and `~/.local/share-work/opencode/`. The bridge profiles inject the right env vars; you don't need separate `opencode-*` binaries.

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
| `bridge.py run <agent> [-p PROMPT] [-m MODEL] [-e EFFORT] [--no-context] [--uuid HEX] [--output-dir DIR]` | Run an agent. If `-p` is omitted and stdin is piped, the prompt is read from stdin. `-m` / `-e` override the profile defaults for one run. `--no-context` skips per-orchestrator auto-routing. `--uuid` (12 lowercase hex chars) lets the orchestrator predetermine the capture-file UUID; `--output-dir` overrides where captures are written. |
| `bridge.py review <agent> [-p PROMPT] [-m MODEL] [-e EFFORT] [--no-context] [--uuid HEX] [--output-dir DIR]` | Run a code review using the agent's native `/review` slash command (claude / opencode-routed) or its native review subcommand (codex). `-p` (or piped stdin) attaches caller-supplied review instructions, routed natively per profile (extends `/review` for slash-command profiles, appended as the trailing positional for review-block profiles like codex). See [Code review](#code-review-bridge-review-agent) below. `--uuid` and `--output-dir` work the same as for `run`. |

The bridge exits with the agent's exit code. Failure modes that exit 2 (with a clear stderr message):

- Unknown agent (`bridge run nonsense`).
- Forcing a flag the profile doesn't support (e.g. `bridge run echo -e high`; `effort_args` is `null`).
- Calling `run` without `-p` or piped stdin — the bridge errors fast rather than letting the underlying CLI hang.
- A malformed profile JSON — bad JSON, missing/empty `command`, a `review` block missing its own `command`, a non-dict `env`, a non-string `cwd`, a `model`/`effort` default paired with its `*_args` set to null, or any `*_args` template that omits the `{value}` placeholder — fails at load time with the offending file name.
- `--uuid` not matching `^[0-9a-f]{12}$`, or naming a UUID whose target capture file already exists with content (refuse-to-clobber).

`bridge show` does NOT auto-route — it always prints the literal profile name you ask for. Only `run` and `review` resolve to per-context variants.

## Bundled profiles

| Name | Binary + base args | Default model | Default effort | Effort flag syntax |
| --- | --- | --- | --- | --- |
| `echo` | `cat` | — | — | n/a (no model/effort) |
| `claude` | `claude --print` | `claude-opus-4-7` | `xhigh` | `--effort {value}` — vocabulary is per model (see below) |
| `claude-personal` | `claude --print` (env→`$HOME/.claude-personal`) | `claude-opus-4-7` | `xhigh` | (same as `claude`, with `CLAUDE_CONFIG_DIR` pointed at the personal config) |
| `claude-work` | `claude --print` (env→`$HOME/.claude-work`) | `claude-opus-4-7` | `xhigh` | (same as `claude`, with `CLAUDE_CONFIG_DIR` pointed at the work config) |
| `codex` | `codex exec` | `gpt-5.5` | `high` | `-c model_reasoning_effort={value}` (low / medium / high / xhigh). Has a `review` block — `bridge review codex` invokes `codex review --uncommitted`. |
| `cursor` | `cursor-agent --print --output-format text` | `composer-2` | — | No effort flag in cursor-agent; reasoning is intrinsic to Composer 2 |
| `kimi` | `kimi --print` | `kimi-k2.6` | `thinking` | `--{value}` — accepts `thinking` (renders `--thinking`) or `no-thinking` (renders `--no-thinking`). Single-context — for per-subscription Kimi access, use `kimi-via-opencode-personal` / `kimi-via-opencode-work`. |
| `kimi-via-opencode` | `opencode run` | `kimi-for-coding/k2p6` | — | OpenCode has no effort flag for the kimi-for-coding provider — pick a thinking variant via `-m` if needed |
| `kimi-via-opencode-personal` | `opencode run` (with personal env) | `kimi-for-coding/k2p6` | — | Same as `kimi-via-opencode`, but injects the env vars that route OpenCode to the personal XDG profile. |
| `kimi-via-opencode-work` | `opencode run` (with work env) | `kimi-for-coding/k2p6` | — | Same idea, work XDG profile. |
| `glm-via-opencode` | `opencode run` | `zai-coding-plan/glm-5.1` | — | OpenCode controls reasoning via model variants, not a flag — `effort_args` is `null` here |
| `glm-via-opencode-personal` | `opencode run` (with personal env) | `zai-coding-plan/glm-5.1` | — | Same as `glm-via-opencode`, but injects the env vars that route OpenCode to the personal XDG profile (replacing the `opencode-personal` zsh function). |
| `glm-via-opencode-work` | `opencode run` (with work env) | `zai-coding-plan/glm-5.1` | — | Same idea, work XDG profile. |

Model IDs are best-guess based on each provider's current naming. If a CLI errors with "unknown model", edit the one string in the profile JSON.

### Switching binaries / subscriptions

The first element of `command` is the executable. To use a different binary — e.g. when you have separate installs for personal vs. work subscriptions, or a wrapper script — just point a profile at it:

```json
{
  "description": "Some custom Claude wrapper",
  "command": ["claude-wrapper", "--print"],
  ...
}
```

**Claude personal/work via env vars (not separate binaries).** The bundled `claude-personal` / `claude-work` profiles use `command: ["claude", "--print"]` and set `CLAUDE_CONFIG_DIR` via the profile's `env` field — they do *not* use separate `claude-personal` / `claude-work` binaries. The user's interactive `claude-personal` / `claude-work` zsh wrappers do the equivalent setup, but shell functions don't propagate to subprocesses, so the profiles replicate it directly.

**OpenCode personal/work via env vars (not separate binaries).** Same pattern: `opencode-personal` and `opencode-work` are zsh shell *functions* that set XDG/OpenCode env vars and exec the same `opencode` binary. Shell functions don't propagate to subprocess, so `glm-via-opencode-personal` and `glm-via-opencode-work` instead use `command: ["opencode", "run"]` and replicate the env-var setup via the profile's `env` field. The bridge expands `$HOME` / `~` / `$VAR` in env values before passing them down (so any `$NAME` you write in `env` is substituted from the parent process env — keep that in mind if a value contains a literal `$`).

Naming convention: profiles routed through OpenCode follow `<model>-via-opencode[-<auth>]` (e.g. `glm-via-opencode-personal`, `kimi-via-opencode`). Native CLI profiles use just the agent name plus optional auth suffix (e.g. `claude`, `claude-work`).

Common reasons to fork a profile:
- Different binary (`claude-work`, `opencode-personal`, custom wrapper script)
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
- Auth: the default `kimi-via-opencode` profile uses the default `opencode` config (`~/.local/share/opencode/auth.json`). For per-subscription auth, the bundled `kimi-via-opencode-personal` and `kimi-via-opencode-work` variants point at the per-XDG-profile auth where `kimi-for-coding` is actually registered.

### Routing any provider through OpenCode

OpenCode supports 75+ providers. Any of them is reachable by setting `command` to `["opencode", "run"]` and `model` to `<provider>/<model-id>`. The provider name has to match an entry in your `~/.local/share/opencode/auth.json` (or the per-XDG-profile equivalent). Examples for this setup:

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

### Adding a new agent

1. Drop a JSON file into the assets/profiles directory (e.g. `assets/profiles/echo.json` for a new agent named `echo`).
2. Set `command` to the agent's non-interactive invocation, *without* the model/effort flags.
3. Pick how the CLI receives the prompt: `prompt_mode: stdin` for stdin input, `prompt_mode: arg` for a positional argument, or `prompt_args: ["-p", "{value}"]` for prompt-via-flag CLIs.
4. Set `model` and `effort` to sensible defaults. Override `model_args` / `effort_args` only if the CLI uses non-default flag names (e.g. codex uses `-c model_reasoning_effort=...`).
5. `bridge.py list` confirms it's picked up.

The bridge never reads or writes the agent's auth — sign in to each CLI the way that CLI expects.

## Resolving personal/work context (automatic)

The bridge **auto-routes to the right per-subscription variant** based on which orchestrator launched it. Run `bridge run claude` from a `claude-personal` shell and you get `claude-personal` transparently. Profiles without per-subscription variants (`codex`, `cursor`, `echo`) just run as-is — there's nothing to fall back from.

### Detection

The bridge reads env vars set by the orchestrator's shell wrapper:

| Orchestrator | Env var read | Sentinel comparison |
| --- | --- | --- |
| `claude-personal` / `claude-work` / `cursor-personal/work` (IDE) | `CLAUDE_CONFIG_DIR` | `CLAUDE_PERSONAL_DIR` / `CLAUDE_WORK_DIR` |
| `opencode-personal` / `opencode-work` / `ocp` / `ocw` / `use-opencode-*` | `OPENCODE_PROFILE` | direct string match (`personal` / `work`) |

Resolution order: `OPENCODE_PROFILE` → `CLAUDE_CONFIG_DIR` → `XDG_DATA_HOME` substring → empty.

### Resolution rules

1. **Already-suffixed name** (`claude-personal`, `claude-work`, etc.) → used verbatim, no resolution attempted (user was explicit).
2. **Context detected and `<name>-<context>` exists** → silently routes to the variant. Prints `[context: 'X' → 'X-context' (orchestrator: ...)]` to stderr for transparency.
3. **No context detected OR no matching variant** → runs the requested profile unchanged (native fallback).

### Examples (current shell is orchestrated by `claude-personal`)

```sh
bridge run claude              # → routes to claude-personal
bridge run glm-via-opencode    # → routes to glm-via-opencode-personal
bridge run kimi-via-opencode   # → routes to kimi-via-opencode-personal
bridge run kimi                # → stays kimi (single-context profile)
bridge run codex               # → stays codex (no variants exist)
bridge run cursor              # → stays cursor (locked to Composer 2)
bridge run claude-work         # → claude-work (explicit, not rewritten)
```

For per-subscription Kimi access, route through OpenCode (`kimi-via-opencode`), not the native `kimi` CLI.

### Opt out

Pass `--no-context` to force the named profile verbatim regardless of orchestrator:

```sh
bridge run claude --no-context -p "..."  # always runs plain `claude`
```

### Direct context inspection

```sh
python3 .agents/skills/agent-bridge-mini/scripts/context.py
# prints: personal | work | (empty)
```

Useful for shell scripts that want to embed the context in their own logic, or for debugging "why did the bridge route to X?"

### Audit trail

Every run records `requested_agent` and `context` in `runs.log` alongside the resolved `agent`:

```json
{"ts": ..., "agent": "claude-personal", "requested_agent": "claude", "context": "personal", ...}
```

So you can always reconstruct what the caller asked for vs. what the bridge dispatched.

## Code review (`bridge review <agent>`)

The `review` subcommand provides a uniform entry point for code reviews across providers, hiding the per-provider mechanism:

```sh
bridge review claude              # auto-routes (claude-personal/work) and sends /review via stdin
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
- **Free-form text review** (paste a function, no git context): `bridge run claude-personal -p "Act as a reviewer. Review this: ..."` — manual prompt.

## Run log and per-run output

Every dispatched run produces these:

1. **A short UUID** (12 hex chars from `uuid4()`) that identifies the run end-to-end. Used in the banners, the per-run capture file name, and the `runs.log` entry.
2. **Three per-run capture files** at `<runs-dir>/<uuid>-<agent>[-<model>].{out,err,timeline}`. Default `<runs-dir>` is `<tempdir>/agent-bridge-mini/runs/` (see the file-tree section above for `<tempdir>` resolution); override per-run with `--output-dir <path>`. `.out` is the agent's stdout verbatim, `.err` is stderr verbatim, `.timeline` is a tiny ASCII sidecar (`<monotonic_ns> stdout|stderr <byte_count>` per kernel chunk) that lets a downstream tool reconstruct chronological interleaving when it matters. Both streams also tee through to the caller's terminal in real time. Two parallel `bridge run` invocations get different UUIDs and different files — they never overwrite each other.
3. **Two stderr banners** so you can see the run start and finish even when the agent itself stays quiet:
   ```
   [bridge:run uuid=345d4fd2cafa agent=claude-personal model=claude-opus-4-7 effort=xhigh stdout=/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-personal-claude-opus-4-7.out stderr=/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-personal-claude-opus-4-7.err]
   ... agent output streams here ...
   [bridge:done uuid=345d4fd2cafa exit=0 duration=1.42s]
   ```
   Banners go to stderr so they don't pollute stdout redirection (`bridge run claude > out.txt` still gives you only the agent's stdout). The `stdout=` and `stderr=` values are absolute paths so you can paste them straight into `cat`/`tail`.

The JSONL audit line in `runs.log` (at `<tempdir>/agent-bridge-mini/runs.log`) carries the UUID, the original prompt, the list of skill references found in it, and a back-reference to the capture file:

```json
{"ts": 1714972800.123, "uuid": "345d4fd2cafa", "action": "run", "agent": "claude-personal", "requested_agent": "claude", "context": "personal", "model": "claude-opus-4-7", "effort": "xhigh", "prompt": "/skill:review tweak this loop", "skills": ["review"], "command": ["claude", "--print", "--model", "claude-opus-4-7", "--effort", "xhigh"], "exit": 0, "duration_s": 1.42, "output_stdout": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-personal-claude-opus-4-7.out", "output_stderr": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-personal-claude-opus-4-7.err", "output_timeline": "/var/folders/.../T/agent-bridge-mini/runs/345d4fd2cafa-claude-personal-claude-opus-4-7.timeline"}
```

`prompt` holds the **original** prompt the caller provided — before skill_format rewriting, exactly as you typed it (or piped it in). `skills` is every `/skill:<name>` reference found in that prompt, deduplicated and in first-seen order (empty list when none). `bridge review` records `"prompt": "/review"` for the default path and `"prompt": null` for the review-block path (codex's native review derives scope from git, sends no prompt).

Pre-flight failures (unknown agent, missing prompt, malformed profile, missing `cwd`, `-m`/`-e` against a `null`-args profile) exit 2 *without* writing a log line or a capture file — only invocations that reached `subprocess.Popen` are recorded (including `FileNotFoundError`/`PermissionError`, which surface as exit 127/126).

`requested_agent` is what the caller asked for; `agent` is what the bridge actually dispatched after auto-routing. `action` is `"run"` or `"review"` so you can filter with `jq 'select(.action=="run")'`. To find the capture files for a specific run, grep by uuid: `jq -r 'select(.uuid=="345d4fd2cafa") | "\(.output_stdout)\n\(.output_stderr)"' runs.log`.

### Predicting capture-file paths (for orchestrators)

The full path is deterministic from the inputs:

```
<output-dir>/<uuid>-<resolved-agent>[-<sanitized-model>].{out,err,timeline}
```

Two rewrites the orchestrator must account for, because both can change the filename away from what the caller typed:

- **Auto-routing rewrites the agent name.** From a `claude-personal` shell, `bridge run claude` resolves to `claude-personal`, so the file is `<uuid>-claude-personal.out`, NOT `<uuid>-claude.out`. Pass `--no-context` for verbatim naming, or use the glob escape hatch below.
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
