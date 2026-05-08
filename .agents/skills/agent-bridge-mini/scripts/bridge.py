#!/usr/bin/env python3
"""agent-bridge: run local coding agents from one place via shared profiles."""
from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path
from typing import Optional, Tuple, Union

try:
    import fcntl  # POSIX-only; bridge is macOS/Linux-targeted
except ImportError:
    fcntl = None  # type: ignore[assignment]

ROOT = Path(__file__).resolve().parent.parent
PROFILES_DIR = ROOT / "assets" / "profiles"
# Logs live under <tempdir>/<skill-name>/ rather than inside the bundle so:
#   1. read-only bundle installs (e.g. /opt or symlinked from a system path) still work
#   2. the bundle directory stays pristine for clean version control / symlinking
#   3. the OS handles eventual cleanup of stale logs
# `ROOT.name` is the skill dir name (typically `agent-bridge-mini`), so a renamed
# bundle gets its own log namespace automatically.
LOG_BASE = Path(tempfile.gettempdir()) / ROOT.name
LOG_FILE = LOG_BASE / "runs.log"
RUNS_DIR = LOG_BASE / "runs"

DEFAULT_MODEL_ARGS = ["--model", "{value}"]
DEFAULT_EFFORT_ARGS = ["--effort", "{value}"]
DEFAULT_SKILL_FORMAT = "/skill:{name}"
SKILL_REFERENCE_RE = re.compile(r"(?<![\w/])/skill:([\w:.-]+)")
_FILENAME_UNSAFE_RE = re.compile(r"[^A-Za-z0-9._-]+")
_CAPTURE_SUFFIXES = frozenset({".out", ".err", ".timeline"})
# Caller-supplied UUIDs must match the bridge's own auto-generated form
# (12 lowercase hex chars from uuid4().hex[:12]) so filenames stay consistent
# whether the orchestrator picked the UUID or the bridge did.
_CALLER_UUID_RE = re.compile(r"^[0-9a-f]{12}$")
# Strict allowlists so a typo (`models` for `model`, `prompt_arg` for
# `prompt_args`, `review.model` instead of `review.model_args`) fails at
# load time instead of being silently ignored.
_PROFILE_KEYS = frozenset({
    "command", "description", "prompt_mode", "prompt_args",
    "model", "effort", "model_args", "effort_args",
    "skill_format", "env", "cwd", "review", "merge_streams",
})
_REVIEW_KEYS = frozenset({"command", "model_args", "effort_args", "scope_default"})

sys.path.insert(0, str(Path(__file__).resolve().parent))
from context import detect as detect_context  # noqa: E402


def resolve_agent(requested: str, profiles: dict[str, dict]) -> tuple[str, str]:
    """Auto-route to the per-context variant when one exists.

    Returns (resolved_name, detected_context). If the requested name is already
    explicit (ends with -personal/-work), returns it unchanged. If no context
    is detected or no matching variant exists, falls back to the requested name.
    """
    if requested.endswith("-personal") or requested.endswith("-work"):
        return requested, ""
    ctx = detect_context()
    if not ctx:
        return requested, ""
    variant = f"{requested}-{ctx}"
    if variant in profiles:
        return variant, ctx
    return requested, ctx  # variant doesn't exist; native fallback


def _validate_arg_template(
    profile_name: str, field: str, value: object, *, allow_null: bool
) -> None:
    """Each *_args template must be None (when allowed) or a non-empty list[str]
    containing '{value}' in at least one element — otherwise the per-run override
    is silently dropped."""
    if value is None:
        if allow_null:
            return
        print(f"profile {profile_name}: '{field}' must be a non-empty list", file=sys.stderr)
        sys.exit(2)
    if not isinstance(value, list) or not value or not all(isinstance(x, str) for x in value):
        print(
            f"profile {profile_name}: '{field}' must be a non-empty list of strings",
            file=sys.stderr,
        )
        sys.exit(2)
    if not any("{value}" in arg for arg in value):
        print(
            f"profile {profile_name}: '{field}' must contain '{{value}}' "
            f"in at least one element (otherwise the override is silently dropped)",
            file=sys.stderr,
        )
        sys.exit(2)


def _check_unknown_keys(
    profile_name: str, where: str, keys, allowed: frozenset
) -> None:
    extra = set(keys) - allowed
    if extra:
        print(
            f"profile {profile_name}: unknown {where} key(s): {sorted(extra)}",
            file=sys.stderr,
        )
        sys.exit(2)


def load_profiles() -> dict[str, dict]:
    profiles: dict[str, dict] = {}
    for path in sorted(PROFILES_DIR.glob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as e:
            print(f"invalid JSON in {path.name}: {e}", file=sys.stderr)
            sys.exit(2)
        if not isinstance(data, dict):
            print(f"profile {path.name}: top-level value must be an object", file=sys.stderr)
            sys.exit(2)
        _check_unknown_keys(path.name, "top-level", data.keys(), _PROFILE_KEYS)
        cmd = data.get("command")
        if not isinstance(cmd, list) or not cmd or not all(isinstance(x, str) for x in cmd):
            print(
                f"profile {path.name}: 'command' is required and must be a non-empty list of strings",
                file=sys.stderr,
            )
            sys.exit(2)
        for field in ("model_args", "effort_args"):
            if field in data:
                _validate_arg_template(path.name, field, data[field], allow_null=True)
        if "prompt_args" in data:
            _validate_arg_template(path.name, "prompt_args", data["prompt_args"], allow_null=False)
        description = data.get("description")
        if description is not None and not isinstance(description, str):
            print(
                f"profile {path.name}: 'description' must be a string",
                file=sys.stderr,
            )
            sys.exit(2)
        if "prompt_mode" in data:
            mode = data["prompt_mode"]
            if mode not in ("stdin", "arg"):
                print(
                    f"profile {path.name}: 'prompt_mode' must be 'stdin' or 'arg' (got {mode!r})",
                    file=sys.stderr,
                )
                sys.exit(2)
        review = data.get("review")
        if review is not None:
            if not isinstance(review, dict):
                print(
                    f"profile {path.name}: 'review' must be an object",
                    file=sys.stderr,
                )
                sys.exit(2)
            _check_unknown_keys(path.name, "review", review.keys(), _REVIEW_KEYS)
            review_cmd = review.get("command")
            if (
                not isinstance(review_cmd, list)
                or not review_cmd
                or not all(isinstance(x, str) for x in review_cmd)
            ):
                print(
                    f"profile {path.name}: 'review.command' must be a non-empty list of strings",
                    file=sys.stderr,
                )
                sys.exit(2)
            for field in ("model_args", "effort_args"):
                if field in review:
                    _validate_arg_template(
                        f"{path.name} (review)", field, review[field], allow_null=True
                    )
            if "scope_default" in review:
                scope_default = review["scope_default"]
                if (
                    not isinstance(scope_default, list)
                    or not all(isinstance(x, str) for x in scope_default)
                ):
                    print(
                        f"profile {path.name}: 'review.scope_default' must be a list of strings",
                        file=sys.stderr,
                    )
                    sys.exit(2)
        if "env" in data:
            env = data["env"]
            if not isinstance(env, dict) or not all(
                isinstance(k, str) and isinstance(v, str) for k, v in env.items()
            ):
                print(
                    f"profile {path.name}: 'env' must be an object of string→string",
                    file=sys.stderr,
                )
                sys.exit(2)
        cwd = data.get("cwd")
        if cwd is not None and (not isinstance(cwd, str) or not cwd):
            print(
                f"profile {path.name}: 'cwd' must be a non-empty string",
                file=sys.stderr,
            )
            sys.exit(2)
        if "skill_format" in data:
            skill_format = data["skill_format"]
            if not isinstance(skill_format, str) or "{name}" not in skill_format:
                print(
                    f"profile {path.name}: 'skill_format' must be a string containing '{{name}}'",
                    file=sys.stderr,
                )
                sys.exit(2)
        if "merge_streams" in data and not isinstance(data["merge_streams"], bool):
            print(
                f"profile {path.name}: 'merge_streams' must be a boolean",
                file=sys.stderr,
            )
            sys.exit(2)
        # Reject contradictions: a default that can never be sent because the
        # corresponding *_args template is null. Otherwise every run errors.
        for default_field, args_field in (("model", "model_args"), ("effort", "effort_args")):
            val = data.get(default_field)
            if val is not None and not isinstance(val, str):
                print(
                    f"profile {path.name}: '{default_field}' must be a string",
                    file=sys.stderr,
                )
                sys.exit(2)
            if val and data.get(args_field, "<unset>") is None:
                print(
                    f"profile {path.name}: '{default_field}' is set but '{args_field}' is null — "
                    f"either remove the default or provide an args template",
                    file=sys.stderr,
                )
                sys.exit(2)
            # Symmetric check for the review block: top-level default + review *_args
            # explicitly null would make every `bridge review` error. The review
            # block only overrides flags, not defaults — if review must not send
            # the flag, drop the top-level default too.
            if val and review is not None and review.get(args_field, "<unset>") is None:
                print(
                    f"profile {path.name}: '{default_field}' is set but 'review.{args_field}' is null — "
                    f"every `bridge review` call would error; either remove the default "
                    f"or provide a review args template",
                    file=sys.stderr,
                )
                sys.exit(2)
        profiles[path.stem] = data
    return profiles


def render_args(template: list[str], value: str) -> list[str]:
    return [arg.replace("{value}", value) for arg in template]


def _apply_skill_format(prompt: str, skill_format: str) -> str:
    """Convenience wrapper over _process_skills for callers that only need the
    rewritten prompt. The live dispatch path calls _process_skills directly
    to also collect the skills list in the same pass."""
    rewritten, _ = _process_skills(prompt, skill_format)
    return rewritten


def _redact_prompt(cmd: list[str], redact_map: dict[int, str]) -> list[str]:
    """Return a copy of cmd with the elements at given indices replaced.

    `redact_map` maps each argv index that holds prompt-derived text to the
    string to log in its place. For whole-element prompts (`prompt_mode='arg'`)
    the replacement is the literal '<prompt>'. For `prompt_args` templates that
    inline the prompt into a flag (e.g. `--prompt={value}`), the replacement
    keeps the flag prefix and only swaps the rendered value, so the log shows
    `--prompt=<prompt>` rather than just `<prompt>`.
    """
    if not redact_map:
        return list(cmd)
    redacted = list(cmd)
    for i, value in redact_map.items():
        if 0 <= i < len(redacted):
            redacted[i] = value
    return redacted


def _resolve_or_die(
    requested: str, profiles: dict[str, dict], no_context: bool
) -> Optional[Tuple[str, str, dict]]:
    """Resolve the requested agent and return (resolved, ctx, profile), or None on error.

    Prints the unknown-agent error to stderr; the caller should exit with 2.
    """
    if no_context:
        resolved, ctx = requested, ""
    else:
        resolved, ctx = resolve_agent(requested, profiles)
    if resolved != requested:
        print(
            f"[context: '{requested}' → '{resolved}' (orchestrator: {ctx})]",
            file=sys.stderr,
        )
    if resolved not in profiles:
        print(f"unknown agent: {resolved}", file=sys.stderr)
        return None
    return resolved, ctx, profiles[resolved]


def _apply_model_effort(
    cmd: list[str],
    *,
    profile_name: str,
    model: Optional[str],
    effort: Optional[str],
    model_args_template: Optional[list[str]],
    effort_args_template: Optional[list[str]],
    label: str = "",
) -> Optional[int]:
    """Append rendered model/effort args. Returns None on success, exit code on error."""
    suffix = f" {label}" if label else ""
    if model:
        if model_args_template is None:
            print(
                f"profile '{profile_name}'{suffix} does not support --model (model_args is null)",
                file=sys.stderr,
            )
            return 2
        cmd.extend(render_args(model_args_template, model))
    if effort:
        if effort_args_template is None:
            print(
                f"profile '{profile_name}'{suffix} does not support --effort (effort_args is null)",
                file=sys.stderr,
            )
            return 2
        cmd.extend(render_args(effort_args_template, effort))
    return None


def _attach_prompt(
    cmd: list[str],
    *,
    profile_name: str,
    profile: dict,
    prompt: Optional[str],
    require_prompt: bool,
) -> Union[Tuple[Optional[str], dict[int, str], list[str]], int]:
    """Append the prompt to cmd according to the profile's prompt mode.

    Returns (stdin_data, redact_map, skills) on success, or an exit code on error.
    `redact_map` maps each argv index that holds prompt-derived text to the
    string to log in its place. `skills` is the deduped list of /skill:<name>
    names found in the original prompt (empty when no prompt or no refs);
    the bridge does the rewrite + extraction in a single pass via
    _process_skills so the audit log doesn't pay for a second walk.
    """
    mode = profile.get("prompt_mode", "stdin")
    prompt_args_template = profile.get("prompt_args")

    if prompt is None:
        if require_prompt:
            print(
                f"profile '{profile_name}' requires a prompt: pass -p/--prompt or pipe via stdin",
                file=sys.stderr,
            )
            return 2
        return None, {}, []

    prompt, skills = _process_skills(
        prompt, profile.get("skill_format", DEFAULT_SKILL_FORMAT)
    )

    if prompt_args_template is not None:
        before = len(cmd)
        rendered = render_args(prompt_args_template, prompt)
        cmd.extend(rendered)
        redact_map = {
            before + offset: raw.replace("{value}", "<prompt>")
            for offset, raw in enumerate(prompt_args_template)
            if "{value}" in raw
        }
        return None, redact_map, skills
    if mode == "stdin":
        return prompt, {}, skills
    if mode == "arg":
        if prompt.startswith("-"):
            cmd.append("--")
        cmd.append(prompt)
        return None, {len(cmd) - 1: "<prompt>"}, skills
    print(f"unknown prompt_mode: {mode}", file=sys.stderr)
    return 2


def _process_skills(prompt: str, skill_format: str) -> Tuple[str, list[str]]:
    """One pass over the prompt — returns (rewritten, skills_list).

    Used by the live dispatch path so the prompt is parsed exactly once for
    both the agent-facing rewrite and the audit log. The deduped skills list
    is in first-seen order. Refs embedded in URLs/paths (preceded by a word
    char or another slash) are excluded by SKILL_REFERENCE_RE.
    """
    seen: dict = {}

    def _replace(m: "re.Match") -> str:
        name = m.group(1)
        if name not in seen:
            seen[name] = None
        return skill_format.replace("{name}", name)

    rewritten = SKILL_REFERENCE_RE.sub(_replace, prompt)
    return rewritten, list(seen)


def _extract_skills(prompt: Optional[str]) -> list[str]:
    """Convenience wrapper over _process_skills for callers that only need the
    skills list (e.g. tests, external scripts). The live dispatch path calls
    _process_skills directly to avoid a second pass."""
    if not prompt:
        return []
    _, skills = _process_skills(prompt, DEFAULT_SKILL_FORMAT)
    return skills


def _sanitize_for_filename(s: str) -> str:
    """Coerce a string into a portable file-name fragment (alnum, dot, hyphen, underscore).
    Caps at 80 chars so very long model IDs don't blow past filesystem limits."""
    cleaned = _FILENAME_UNSAFE_RE.sub("_", s).strip("_") or "_"
    return cleaned[:80]


def _compose_capture_stem(
    run_uuid: str, resolved: str, model: Optional[str]
) -> str:
    """Compose `<uuid>-<agent>[-<model>]` (no extension). Used by
    `_build_output_paths` to name capture files. The refuse-to-clobber check
    works at UUID-prefix granularity (`_find_non_empty_capture_for_uuid`), so
    it doesn't depend on this stem — only on the leading `<uuid>-`."""
    parts = [run_uuid, _sanitize_for_filename(resolved)]
    if model:
        parts.append(_sanitize_for_filename(model))
    return "-".join(parts)


def _resolve_runs_dir_path(path: Path) -> Path:
    """Return an absolute, expanded runs-dir path for logs and captures.

    `Path.resolve()` also canonicalizes symlinks (e.g. macOS `/var` to
    `/private/var`), which is unnecessary churn for callers that already pass
    an absolute path. `abspath` gives relative `--output-dir` values a stable
    absolute base without rewriting existing absolute roots.
    """
    expanded = Path(os.path.expanduser(os.path.expandvars(str(path))))
    return Path(os.path.abspath(expanded))


def _find_non_empty_capture_for_uuid(
    runs_dir: Path, run_uuid: str
) -> Optional[Path]:
    """Find any non-empty bridge capture file already using `run_uuid`.

    The UUID is the stable lookup key for orchestrators (`$DIR/$UUID-*`), so a
    caller-supplied UUID cannot be reused for a different resolved agent/model
    stem either.
    """
    try:
        for path in runs_dir.glob(f"{run_uuid}-*"):
            if path.suffix not in _CAPTURE_SUFFIXES:
                continue
            try:
                if path.is_file() and path.stat().st_size > 0:
                    return path
            except OSError:
                pass
    except OSError:
        pass
    return None


def _build_output_paths(
    runs_dir: Path, run_uuid: str, resolved: str, model: Optional[str],
    *, merge_streams: bool = False,
) -> Optional[Tuple[Path, Optional[Path], Path]]:
    """Compute (stdout, stderr, timeline) capture paths and ensure runs_dir exists.

    Returns the 3-tuple on success, or None if runs_dir can't be created or any
    file can't be touched — caller falls back to passthrough so a read-only
    bundle install still runs.

    When `merge_streams` is true the second slot is None: there is no separate
    `.err` file because both FDs are written into `.out` in arrival order.
    `bridge replay` can still distinguish them via the `.timeline` labels.

    Files are touched up front so a write failure (full disk, permission race)
    is attributed to log creation here rather than misreported as a Popen
    failure later. `_streamed_run` still truncates via "wb" on open.
    """
    stem = _compose_capture_stem(run_uuid, resolved, model)
    try:
        runs_dir.mkdir(parents=True, exist_ok=True)
    except OSError as e:
        print(
            f"warning: could not create {runs_dir} ({e}); per-run capture will be skipped",
            file=sys.stderr,
        )
        return None
    stdout_path = runs_dir / f"{stem}.out"
    timeline_path = runs_dir / f"{stem}.timeline"
    stderr_path: Optional[Path] = None if merge_streams else runs_dir / f"{stem}.err"
    paths_to_touch = [stdout_path, timeline_path]
    if stderr_path is not None:
        paths_to_touch.append(stderr_path)
    for path in paths_to_touch:
        try:
            path.touch()
        except OSError as e:
            print(
                f"warning: could not create {path} ({e}); per-run capture will be skipped",
                file=sys.stderr,
            )
            return None
    return (stdout_path, stderr_path, timeline_path)


def _drop_unused_outputs(paths: Optional[Tuple[Path, Optional[Path], Path]]) -> None:
    """Best-effort unlink for touched-but-unused capture files.

    `_build_output_paths` touches the capture files up front to surface write
    errors early, but if the agent never reached its first write (Popen
    failure, file open failure mid-stream before any chunk arrived), the
    files linger empty. Delete only the empties so partial output from a
    mid-stream OSError is preserved. None entries (e.g. the stderr slot when
    `merge_streams` is on) are skipped.
    """
    if paths is None:
        return
    for path in paths:
        if path is None:
            continue
        try:
            if path.stat().st_size == 0:
                path.unlink()
        except OSError:
            pass


def _streamed_run(
    cmd: list[str],
    stdin_data: Optional[str],
    env: dict,
    cwd_path: Optional[Path],
    stdout_path: Path,
    stderr_path: Optional[Path],
    timeline_path: Path,
    *,
    merge_streams: bool = False,
) -> int:
    """Run cmd, tee stdout/stderr to caller's terminal AND to per-stream
    capture files. A `.timeline` sidecar records `<monotonic_ns> <stream>
    <byte_count>` per chunk so chronological interleaving can be reconstructed
    when needed. Returns the exit code.

    Normally each capture file is written by exactly one thread, so per-file
    locking isn't needed; the timeline file is shared and uses `timeline_lock`.

    When `merge_streams` is true the stderr tee writes into the same `.out`
    file as the stdout tee, in arrival order, with `timeline_lock` extended to
    cover the capture write so on-disk byte order matches timeline-file order
    (an invariant `bridge replay` relies on). `stderr_path` is ignored in that
    case (callers should pass None). Terminal fan-out is unchanged — stdout
    chunks still go to the caller's stdout, stderr chunks to the caller's
    stderr — only the on-disk capture is merged.

    Raises FileNotFoundError/PermissionError/OSError if Popen creation fails —
    the caller already handles those. The tee threads survive caller pipe-close
    (BrokenPipeError) so the per-run files stay complete even when downstream
    consumers drop early (e.g. `bridge run claude | head`).
    """
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=cwd_path,
    )

    if stdin_data is not None:
        # Thread the stdin write so a large prompt that exceeds the OS pipe
        # buffer can't deadlock against an agent that produces output before
        # consuming all of stdin.
        def _write_stdin() -> None:
            try:
                proc.stdin.write(stdin_data.encode())
            except (BrokenPipeError, OSError):
                pass
            finally:
                try:
                    proc.stdin.close()
                except OSError:
                    pass

        threading.Thread(target=_write_stdin, daemon=True).start()

    # `timeline_lock` always serializes timeline writes (two threads share that
    # file). In merge mode it ALSO covers the capture write, coupling them so
    # that timeline file order matches capture file order — `bridge replay`
    # relies on that invariant to allocate bytes correctly when both streams
    # share one .out file. In non-merge mode each capture file has a single
    # writer (no contention), so the lock only wraps the timeline write.
    timeline_lock = threading.Lock()
    try:
        # ExitStack ensures earlier-opened file handles are closed if a later
        # `open()` raises. The previous nested `try/finally` form leaked the
        # already-opened files in that case; the original `with stdout_path.open
        # ... as out_f, stderr_path.open ... as err_f, ...:` form was safe via
        # context-manager unwind, and ExitStack restores that behavior while
        # still allowing the conditional branch on `merge_streams`.
        with contextlib.ExitStack() as stack:
            out_f = stack.enter_context(stdout_path.open("wb"))
            timeline_f = stack.enter_context(timeline_path.open("wb"))
            if merge_streams:
                err_f = out_f  # alias; ExitStack closes the underlying fd via out_f
            elif stderr_path is not None:
                err_f = stack.enter_context(stderr_path.open("wb"))
            else:
                err_f = None

            def _tee(src, sink_buffer, capture_f, stream_label: str) -> None:
                while True:
                    # read1 returns as soon as any data is available (one
                    # underlying os.read), so slow-producer agents stream in
                    # real time. Plain read(4096) would block until 4096 bytes
                    # accumulate or EOF, defeating the "tee through to the
                    # terminal in real time" contract documented in README.md.
                    chunk = src.read1(4096)
                    if not chunk:
                        break
                    ts = time.monotonic_ns()
                    try:
                        sink_buffer.write(chunk)
                        sink_buffer.flush()
                    except (BrokenPipeError, OSError):
                        pass
                    # OSError (disk full, fd closed) must not kill the tee —
                    # we still need to drain the agent's pipe so it doesn't
                    # block, even if capture is lost.
                    if merge_streams:
                        # Coupled write: capture-then-timeline under one lock,
                        # so the on-disk order of bytes in .out matches the
                        # order of entries in .timeline. If the capture write
                        # fails (e.g. disk full), the timeline entry MUST be
                        # skipped — replay allocates N bytes per entry from
                        # .out, and a stale entry would mis-source the next
                        # chunk's bytes for the wrong stream. Non-merge mode
                        # tolerates this drift (documented), merge mode does
                        # not (replay invariant relies on it).
                        with timeline_lock:
                            capture_ok = False
                            if capture_f is not None:
                                try:
                                    capture_f.write(chunk)
                                    capture_f.flush()
                                    capture_ok = True
                                except OSError:
                                    pass
                            if capture_ok:
                                try:
                                    timeline_f.write(
                                        f"{ts} {stream_label} {len(chunk)}\n".encode("ascii")
                                    )
                                    timeline_f.flush()
                                except OSError:
                                    pass
                    else:
                        if capture_f is not None:
                            try:
                                capture_f.write(chunk)
                                capture_f.flush()
                            except OSError:
                                pass
                        with timeline_lock:
                            try:
                                timeline_f.write(
                                    f"{ts} {stream_label} {len(chunk)}\n".encode("ascii")
                                )
                                timeline_f.flush()
                            except OSError:
                                pass

            threads = [
                threading.Thread(
                    target=_tee,
                    args=(proc.stdout, sys.stdout.buffer, out_f, "stdout"),
                    daemon=True,
                ),
                threading.Thread(
                    target=_tee,
                    args=(proc.stderr, sys.stderr.buffer, err_f, "stderr"),
                    daemon=True,
                ),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

        return proc.wait()
    except KeyboardInterrupt:
        # SIGINT delivered via the controlling tty already reaches the child via
        # the foreground process group, so the child usually exits on its own.
        # When the signal reaches only us (e.g. the parent is signaled directly),
        # the child would otherwise become an orphan — terminate it explicitly,
        # then escalate to SIGKILL if it doesn't honor SIGTERM.
        try:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        except OSError:
            pass
        raise
    finally:
        # Close the pipe FDs even if a tee thread raised — Popen leaves them
        # open until the proc object is GC'd, which leaks descriptors in tests
        # and long-running parents.
        for stream in (proc.stdout, proc.stderr):
            try:
                if stream is not None:
                    stream.close()
            except OSError:
                pass


def _dispatch(
    *,
    action: str,
    requested: str,
    resolved: str,
    ctx: str,
    profile: dict,
    cmd: list[str],
    model: Optional[str],
    effort: Optional[str],
    prompt: Optional[str],
    skills: list[str],
    stdin_data: Optional[str],
    redact_map: dict[int, str],
    runs_dir: Optional[Path] = None,
    caller_uuid: Optional[str] = None,
) -> int:
    """Run the assembled command, append a JSONL log line, return the exit code."""
    # Defer the RUNS_DIR lookup so test patches of `bridge.RUNS_DIR` take effect
    # — Python evaluates default args at definition time, which would freeze the
    # original module value.
    if runs_dir is None:
        runs_dir = RUNS_DIR
    runs_dir = _resolve_runs_dir_path(runs_dir)
    env = os.environ.copy()
    for key, value in profile.get("env", {}).items():
        env[key] = os.path.expanduser(os.path.expandvars(value))

    cwd = profile.get("cwd")
    cwd_path = Path(os.path.expanduser(os.path.expandvars(cwd))) if cwd else None
    if cwd_path is not None and not cwd_path.is_dir():
        print(
            f"profile '{resolved}': cwd does not exist or is not a directory: {cwd_path}",
            file=sys.stderr,
        )
        return 2

    if caller_uuid is not None:
        run_uuid = caller_uuid
        # Refuse-to-clobber: if any non-empty capture already uses this UUID
        # prefix, the orchestrator picked a UUID that's already in use. Empty
        # leftovers (touched but never written) are fine to overwrite —
        # they're typically orphans from a prior aborted run.
        existing = _find_non_empty_capture_for_uuid(runs_dir, run_uuid)
        if existing is not None:
            print(
                f"--uuid {caller_uuid}: capture file already exists with content: {existing}",
                file=sys.stderr,
            )
            return 2
    else:
        run_uuid = uuid.uuid4().hex[:12]
    merge_streams = bool(profile.get("merge_streams", False))
    output_paths = _build_output_paths(
        runs_dir, run_uuid, resolved, model, merge_streams=merge_streams
    )

    banner_bits = [f"uuid={run_uuid}", f"agent={resolved}"]
    if model:
        banner_bits.append(f"model={model}")
    if effort:
        banner_bits.append(f"effort={effort}")
    if output_paths is not None:
        banner_bits.append(f"stdout={output_paths[0]}")
        if output_paths[1] is not None:
            banner_bits.append(f"stderr={output_paths[1]}")
        elif merge_streams:
            banner_bits.append("merged=true")
    print(f"[bridge:run {' '.join(banner_bits)}]", file=sys.stderr)

    # Wall-clock for the log stamp; monotonic for duration so a clock jump
    # (NTP correction, DST) never produces a negative duration_s.
    ts = time.time()
    start = time.monotonic()
    try:
        if output_paths is not None:
            exit_code = _streamed_run(
                cmd, stdin_data, env, cwd_path,
                stdout_path=output_paths[0],
                stderr_path=output_paths[1],
                timeline_path=output_paths[2],
                merge_streams=merge_streams,
            )
        else:
            # Fallback when per-run capture is unavailable: original passthrough
            # behavior. Caller's stdout/stderr inherit directly.
            run_kwargs = {"text": True, "env": env, "cwd": cwd_path}
            if stdin_data is None:
                run_kwargs["stdin"] = subprocess.DEVNULL
            else:
                run_kwargs["input"] = stdin_data
            result = subprocess.run(cmd, **run_kwargs)
            exit_code = result.returncode
    except FileNotFoundError:
        print(f"command not found: {cmd[0]}", file=sys.stderr)
        exit_code = 127
        _drop_unused_outputs(output_paths)
        output_paths = None
    except PermissionError:
        print(f"permission denied: {cmd[0]}", file=sys.stderr)
        exit_code = 126
        _drop_unused_outputs(output_paths)
        output_paths = None
    except OSError as e:
        print(f"could not run {cmd[0]}: {e}", file=sys.stderr)
        exit_code = 127
        _drop_unused_outputs(output_paths)
        output_paths = None
    duration = time.monotonic() - start

    print(
        f"[bridge:done uuid={run_uuid} exit={exit_code} duration={duration:.2f}s]",
        file=sys.stderr,
    )

    record = {
        "ts": round(ts, 3),
        "uuid": run_uuid,
        "action": action,
        "agent": resolved,
        "requested_agent": requested,
        "context": ctx,
        "model": model,
        "effort": effort,
        "prompt": prompt,
        "skills": skills,
        "command": _redact_prompt(cmd, redact_map),
        "exit": exit_code,
        "duration_s": round(duration, 3),
        "output_stdout": (str(output_paths[0]) if output_paths is not None else None),
        # output_stderr is null both when capture is unavailable AND when the
        # profile sets merge_streams (no separate .err file in that case).
        "output_stderr": (
            str(output_paths[1])
            if output_paths is not None and output_paths[1] is not None
            else None
        ),
        "output_timeline": (str(output_paths[2]) if output_paths is not None else None),
        "merge_streams": merge_streams,
    }
    try:
        # Ensure LOG_FILE.parent exists. Default RUNS_DIR creation already
        # populates LOG_BASE as a side effect, but `--output-dir <elsewhere>`
        # does not — without this, the first --output-dir run on a fresh
        # install (or post-tmp-purge) silently drops its audit record.
        LOG_FILE.parent.mkdir(parents=True, exist_ok=True)
        with LOG_FILE.open("a") as f:
            # O_APPEND atomically positions the offset, but Python's
            # BufferedWriter flushes in 8KB chunks — large prompts produce
            # multi-syscall writes that can interleave between concurrent
            # bridge processes. fcntl.flock serializes the whole record.
            # Best-effort: if fcntl is unavailable (Windows) or the FS doesn't
            # support advisory locks (rare on tmp), fall through to the
            # unsynchronized append rather than dropping the record.
            if fcntl is not None:
                try:
                    fcntl.flock(f.fileno(), fcntl.LOCK_EX)
                except OSError:
                    pass
            f.write(json.dumps(record) + "\n")
    except OSError as e:
        # Logging is best-effort: never let a log-write failure mask the
        # underlying agent's exit code.
        print(f"warning: could not append to runs.log ({e})", file=sys.stderr)

    return exit_code


def cmd_list(_args: argparse.Namespace) -> int:
    profiles = load_profiles()
    if not profiles:
        print(f"no profiles found in {PROFILES_DIR}", file=sys.stderr)
        return 0
    width = max(len(name) for name in profiles)
    for name, profile in profiles.items():
        suffix_parts = []
        if profile.get("model"):
            suffix_parts.append(f"model={profile['model']}")
        if profile.get("effort"):
            suffix_parts.append(f"effort={profile['effort']}")
        suffix = f" [{', '.join(suffix_parts)}]" if suffix_parts else ""
        print(f"{name:<{width}}  {profile.get('description') or ''}{suffix}")
    return 0


def cmd_show(args: argparse.Namespace) -> int:
    profiles = load_profiles()
    if args.agent not in profiles:
        print(f"unknown agent: {args.agent}", file=sys.stderr)
        return 2
    print(json.dumps(profiles[args.agent], indent=2))
    return 0


def _resolve_runs_dir_and_uuid(
    args: argparse.Namespace,
) -> Union[Tuple[Path, Optional[str]], int]:
    """Pull `--output-dir` and `--uuid` off args, validate them, and return
    (runs_dir, caller_uuid) on success or exit code 2 on validation failure."""
    runs_dir = (
        _resolve_runs_dir_path(Path(args.output_dir))
        if getattr(args, "output_dir", None)
        else RUNS_DIR
    )
    caller_uuid = getattr(args, "uuid", None)
    if caller_uuid is not None and not _CALLER_UUID_RE.match(caller_uuid):
        print(
            f"--uuid must be 12 lowercase hex characters (got {caller_uuid!r})",
            file=sys.stderr,
        )
        return 2
    return runs_dir, caller_uuid


def cmd_run(args: argparse.Namespace) -> int:
    profiles = load_profiles()
    requested = args.agent
    resolution = _resolve_or_die(requested, profiles, args.no_context)
    if resolution is None:
        return 2
    resolved, ctx, profile = resolution

    resolved_io = _resolve_runs_dir_and_uuid(args)
    if isinstance(resolved_io, int):
        return resolved_io
    runs_dir, caller_uuid = resolved_io

    cmd = list(profile["command"])

    model = args.model if args.model is not None else profile.get("model")
    effort = args.effort if args.effort is not None else profile.get("effort")
    err = _apply_model_effort(
        cmd,
        profile_name=resolved,
        model=model,
        effort=effort,
        model_args_template=profile.get("model_args", DEFAULT_MODEL_ARGS),
        effort_args_template=profile.get("effort_args", DEFAULT_EFFORT_ARGS),
    )
    if err is not None:
        return err

    prompt = args.prompt
    if prompt is None and not sys.stdin.isatty():
        prompt = sys.stdin.read()
    # Empty piped stdin is indistinguishable from "no prompt" from the agent's
    # perspective — treat it the same as a missing prompt so the bridge errors
    # cleanly rather than letting the underlying CLI hang or fail opaquely.
    if not prompt:
        prompt = None

    attached = _attach_prompt(
        cmd, profile_name=resolved, profile=profile, prompt=prompt, require_prompt=True
    )
    if isinstance(attached, int):
        return attached
    stdin_data, redact_map, skills = attached

    return _dispatch(
        action="run",
        requested=requested,
        resolved=resolved,
        ctx=ctx,
        profile=profile,
        cmd=cmd,
        model=model,
        effort=effort,
        prompt=prompt,
        skills=skills,
        stdin_data=stdin_data,
        redact_map=redact_map,
        runs_dir=runs_dir,
        caller_uuid=caller_uuid,
    )


def cmd_review(args: argparse.Namespace) -> int:
    profiles = load_profiles()
    requested = args.agent
    resolution = _resolve_or_die(requested, profiles, args.no_context)
    if resolution is None:
        return 2
    resolved, ctx, profile = resolution

    resolved_io = _resolve_runs_dir_and_uuid(args)
    if isinstance(resolved_io, int):
        return resolved_io
    runs_dir, caller_uuid = resolved_io

    # Caller-supplied review instructions are optional. -p wins; otherwise read
    # piped stdin. Empty stdin → no prompt (same handling as cmd_run).
    caller_prompt = args.prompt
    if caller_prompt is None and not sys.stdin.isatty():
        caller_prompt = sys.stdin.read()
    if not caller_prompt:
        caller_prompt = None

    review = profile.get("review")
    if review is not None:
        # Override path — e.g. `codex review --uncommitted`. The native review
        # subcommand derives scope from git; any caller-supplied instructions
        # are appended as the trailing positional argument
        # (`codex review … [PROMPT]`).
        cmd = list(review["command"])
        model_args_template = review.get("model_args", profile.get("model_args", DEFAULT_MODEL_ARGS))
        effort_args_template = review.get("effort_args", profile.get("effort_args", DEFAULT_EFFORT_ARGS))
        # `scope_default` is appended only when the caller didn't supply a
        # prompt — sidesteps codex's "scope flags are mutually exclusive with
        # [PROMPT]" rule. With no prompt we keep the default scope (e.g.
        # `--uncommitted`); with a prompt we drop it so the prompt is the
        # sole [PROMPT] positional. Profiles without a scope_default are
        # unaffected.
        scope_default = review.get("scope_default", [])
        # Synthesize an arg-mode profile for prompt attachment regardless of
        # the top-level prompt_mode — codex.json is "stdin" for the run path,
        # but `codex review` takes its prompt via argv. skill_format still
        # comes from the underlying profile so refs are translated correctly.
        attach_profile = {
            "prompt_mode": "arg",
            "skill_format": profile.get("skill_format", DEFAULT_SKILL_FORMAT),
        }
        attach_prompt = caller_prompt
    else:
        # Default path — main command + "/review" (optionally extended with
        # caller instructions: `/review focus on auth changes`). Slash-command
        # CLIs (claude, opencode-routed) accept extra text after `/review`
        # natively.
        cmd = list(profile["command"])
        model_args_template = profile.get("model_args", DEFAULT_MODEL_ARGS)
        effort_args_template = profile.get("effort_args", DEFAULT_EFFORT_ARGS)
        attach_profile = profile
        attach_prompt = (
            f"/review {caller_prompt}" if caller_prompt else "/review"
        )

    # Model/effort defaults always come from the profile, not the review block —
    # so `bridge review` matches `bridge run` defaults unless overridden by -m/-e.
    model = args.model if args.model is not None else profile.get("model")
    effort = args.effort if args.effort is not None else profile.get("effort")
    err = _apply_model_effort(
        cmd,
        profile_name=resolved,
        model=model,
        effort=effort,
        model_args_template=model_args_template,
        effort_args_template=effort_args_template,
        label="review",
    )
    if err is not None:
        return err

    # Inject the review block's `scope_default` between model/effort flags and
    # the prompt — only when the caller didn't supply a prompt. With a custom
    # prompt the scope is owned by the caller.
    if review is not None and caller_prompt is None:
        cmd.extend(scope_default)

    attached = _attach_prompt(
        cmd, profile_name=resolved, profile=attach_profile,
        prompt=attach_prompt, require_prompt=False,
    )
    if isinstance(attached, int):
        return attached
    stdin_data, redact_map, skills = attached
    # Only redact when the caller actually supplied review instructions; the
    # bare "/review" framing is internal, not user input, and keeping it
    # visible in `command` aids debugging.
    if caller_prompt is None:
        redact_map = {}

    return _dispatch(
        action="review",
        requested=requested,
        resolved=resolved,
        ctx=ctx,
        profile=profile,
        cmd=cmd,
        model=model,
        effort=effort,
        prompt=attach_prompt,
        skills=skills,
        stdin_data=stdin_data,
        redact_map=redact_map,
        runs_dir=runs_dir,
        caller_uuid=caller_uuid,
    )


def cmd_replay(args: argparse.Namespace) -> int:
    """Replay a prior run's captured output in chronological order.

    Walks the `.timeline` sidecar, reading N bytes per entry from the matching
    `.out`/`.err` capture file, and writes them to the caller's stdout/stderr
    in arrival order — the same interleaving the user originally saw on the
    terminal.

    Two capture shapes are supported:
      - Normal capture: bytes come from `.out` for stdout entries, `.err` for
        stderr entries. Timeline entries are sorted by ts before replay because
        the two tee threads write under separate locks (see README — timeline
        lines are NOT pre-sorted in non-merge mode).
      - Merged capture (profile had `merge_streams: true`): there is no `.err`
        file; both stdout and stderr entries are sourced from `.out` in
        timeline file order — the bridge holds a single lock around the
        capture+timeline writes when merging, so timeline order IS the
        on-disk byte order. Re-sorting by ts in this mode would scramble the
        byte allocation, so we explicitly skip the sort. The timeline labels
        still drive whether each chunk is replayed to the caller's stdout vs.
        stderr, restoring the FD distinction the merged file lost.

    `--tag` prefixes each chunk with `[stdout] ` / `[stderr] ` so the streams
    are visually distinguishable when piped to a single file.
    """
    runs_dir = (
        _resolve_runs_dir_path(Path(args.output_dir))
        if getattr(args, "output_dir", None)
        else _resolve_runs_dir_path(RUNS_DIR)
    )
    run_uuid = args.uuid
    if not _CALLER_UUID_RE.match(run_uuid):
        print(
            f"uuid must be 12 lowercase hex characters (got {run_uuid!r})",
            file=sys.stderr,
        )
        return 2

    captures_by_stem: dict[str, dict[str, Path]] = {}
    non_empty_stems: set[str] = set()
    try:
        for path in runs_dir.glob(f"{run_uuid}-*"):
            if path.suffix not in _CAPTURE_SUFFIXES:
                continue
            try:
                if not path.is_file():
                    continue
                size = path.stat().st_size
            except OSError:
                continue
            stem = path.with_suffix("").name
            captures_by_stem.setdefault(stem, {})[path.suffix] = path
            if size > 0:
                non_empty_stems.add(stem)
    except OSError as e:
        print(f"could not list {runs_dir}: {e}", file=sys.stderr)
        return 2

    selected_stem: Optional[str] = None
    if len(non_empty_stems) == 1:
        # Empty files from an aborted/retried run are explicitly allowed by
        # dispatch. Replay must ignore those stale stems rather than mixing
        # their .out/.err/.timeline files into the current capture.
        selected_stem = next(iter(non_empty_stems))
    elif len(non_empty_stems) > 1:
        stems = ", ".join(sorted(non_empty_stems))
        print(
            f"multiple non-empty capture stems found for uuid {run_uuid}: {stems}",
            file=sys.stderr,
        )
        return 2
    elif len(captures_by_stem) == 1:
        # Preserve replay for a successful run that produced no output at all.
        selected_stem = next(iter(captures_by_stem))
    elif len(captures_by_stem) > 1:
        stems = ", ".join(sorted(captures_by_stem))
        print(
            f"multiple empty capture stems found for uuid {run_uuid}: {stems}",
            file=sys.stderr,
        )
        return 2

    selected = captures_by_stem.get(selected_stem or "", {})
    timeline_path = selected.get(".timeline")
    out_path = selected.get(".out")
    err_path = selected.get(".err")

    if timeline_path is None:
        print(
            f"no .timeline file found for uuid {run_uuid} in {runs_dir}",
            file=sys.stderr,
        )
        return 2
    if out_path is None:
        print(
            f"no .out file found for uuid {run_uuid} in {runs_dir}",
            file=sys.stderr,
        )
        return 2

    entries: list[tuple[int, str, int]] = []
    try:
        with timeline_path.open("r", encoding="ascii", errors="replace") as f:
            for line in f:
                parts = line.strip().split()
                if len(parts) != 3:
                    continue
                try:
                    ts = int(parts[0])
                    n = int(parts[2])
                except ValueError:
                    continue
                stream = parts[1]
                if stream not in ("stdout", "stderr"):
                    continue
                # Negative `n` would make src.read(n) read ALL remaining bytes
                # (Python read(-1) semantics), scrambling byte allocation for
                # every subsequent entry. The bridge never writes negative
                # counts, so this only fires on hand-edited or corrupted
                # timelines — drop the entry rather than emit wrong content.
                if n < 0:
                    continue
                entries.append((ts, stream, n))
    except OSError as e:
        print(f"could not read {timeline_path}: {e}", file=sys.stderr)
        return 2

    # Merged-capture detection: absence of `.err` on the selected stem is the
    # normal contract for `merge_streams: true`. A retry may also leave behind
    # an empty `.err` from an earlier non-merged attempt with the same UUID and
    # stem. If the timeline contains stderr bytes and `.out` accounts for every
    # timeline byte, the empty `.err` is stale and this is a merged capture.
    merged = err_path is None
    # The `err_path is not None` clause is redundant with `not merged` but
    # preserves type narrowing for the `err_path.stat()` call below.
    if not merged and err_path is not None:
        stderr_bytes = sum(n for _ts, stream, n in entries if stream == "stderr")
        if stderr_bytes > 0:
            try:
                err_size = err_path.stat().st_size
                out_size = out_path.stat().st_size
            except OSError as e:
                print(f"could not stat replay capture files: {e}", file=sys.stderr)
                return 2
            total_bytes = sum(n for _ts, _stream, n in entries)
            if err_size == 0 and out_size == total_bytes:
                merged = True
                err_path = None
            elif err_size == 0:
                print(
                    f"stderr timeline entries exist but {err_path} is empty",
                    file=sys.stderr,
                )
                return 2
    if not merged:
        # Non-merge mode: each capture file has a single writer with its own
        # cursor, so re-sorting by ts only affects display order — and the
        # README documents that timeline lines are NOT pre-sorted because the
        # threads write under separate locks.
        entries.sort(key=lambda e: e[0])
    tag = bool(getattr(args, "tag", False))

    out_buf = sys.stdout.buffer
    err_buf = sys.stderr.buffer
    # ExitStack mirrors the cleanup pattern in `_streamed_run`: out_f opens
    # first, and if err_f's open raises, the stack still closes out_f.
    with contextlib.ExitStack() as stack:
        try:
            out_f = stack.enter_context(out_path.open("rb"))
        except OSError as e:
            print(f"could not open {out_path}: {e}", file=sys.stderr)
            return 2
        err_f = None
        if not merged and err_path is not None:
            try:
                err_f = stack.enter_context(err_path.open("rb"))
            except OSError as e:
                print(f"could not open {err_path}: {e}", file=sys.stderr)
                return 2

        for _ts, stream, n in entries:
            src = out_f if (merged or stream == "stdout") else err_f
            chunk = src.read(n) if src is not None else b""
            if not chunk:
                continue
            sink = out_buf if stream == "stdout" else err_buf
            try:
                if tag:
                    sink.write(f"[{stream}] ".encode("ascii"))
                sink.write(chunk)
                sink.flush()
            except (BrokenPipeError, OSError):
                pass
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        prog="bridge",
        description="Run local coding agents from one place via shared profiles.",
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("list", help="list available agent profiles").set_defaults(func=cmd_list)

    show = sub.add_parser("show", help="print a profile as JSON")
    show.add_argument("agent")
    show.set_defaults(func=cmd_show)

    def _add_capture_flags(p: argparse.ArgumentParser) -> None:
        p.add_argument(
            "--uuid",
            help=(
                "caller-supplied 12 lowercase hex chars used to name the capture "
                "files; lets the orchestrator predetermine output paths. Default: auto-generated."
            ),
        )
        p.add_argument(
            "--output-dir",
            help=(
                "override the per-run capture directory (default: "
                "$TMPDIR/agent-bridge-mini/runs/). Combined with --uuid, the "
                "orchestrator knows the exact output paths before the run starts."
            ),
        )

    run = sub.add_parser("run", help="run an agent profile")
    run.add_argument("agent")
    run.add_argument("-p", "--prompt", help="prompt to send (default: stdin if piped)")
    run.add_argument("-m", "--model", help="override the profile's default model")
    run.add_argument("-e", "--effort", help="override the profile's default reasoning effort")
    run.add_argument(
        "--no-context",
        action="store_true",
        help="disable auto-routing to per-orchestrator variants (use the named profile as-is)",
    )
    _add_capture_flags(run)
    run.set_defaults(func=cmd_run)

    review = sub.add_parser(
        "review",
        help="run a code review using the agent's native /review or review subcommand",
    )
    review.add_argument("agent")
    review.add_argument(
        "-p", "--prompt",
        help=(
            "optional review instructions. For slash-command profiles "
            "(claude, opencode-routed) extends the literal '/review' as "
            "'/review <prompt>'. For profiles with a native review block "
            "(codex) the prompt is appended as the trailing positional "
            "argument. If omitted, stdin is read when piped."
        ),
    )
    review.add_argument("-m", "--model", help="override the profile's default model")
    review.add_argument("-e", "--effort", help="override the profile's default reasoning effort")
    review.add_argument(
        "--no-context",
        action="store_true",
        help="disable auto-routing to per-orchestrator variants",
    )
    _add_capture_flags(review)
    review.set_defaults(func=cmd_review)

    replay = sub.add_parser(
        "replay",
        help=(
            "replay a prior run's captured output in chronological order, "
            "using the .timeline sidecar to interleave stdout and stderr"
        ),
    )
    replay.add_argument("uuid", help="12 lowercase hex chars identifying the run")
    replay.add_argument(
        "--output-dir",
        help=(
            "directory containing the capture files (default: same as bridge run)"
        ),
    )
    replay.add_argument(
        "--tag",
        action="store_true",
        help="prefix each chunk with [stdout] or [stderr] when replaying",
    )
    replay.set_defaults(func=cmd_replay)

    try:
        args = parser.parse_args()
        return args.func(args)
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
