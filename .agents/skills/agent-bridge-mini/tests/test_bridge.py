from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


SCRIPTS_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS_DIR))

spec = importlib.util.spec_from_file_location("bridge", SCRIPTS_DIR / "bridge.py")
assert spec is not None
bridge = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(bridge)


class AttachPromptTests(unittest.TestCase):
    def test_stdin_mode_requires_prompt_when_requested(self) -> None:
        cmd = ["cat"]

        with patch("sys.stderr"):
            result = bridge._attach_prompt(
                cmd,
                profile_name="echo",
                profile={"prompt_mode": "stdin"},
                prompt=None,
                require_prompt=True,
            )

        self.assertEqual(result, 2)
        self.assertEqual(cmd, ["cat"])

    def test_arg_mode_separates_leading_dash_prompt(self) -> None:
        cmd = ["codex", "exec", "--model", "gpt-5.5"]

        result = bridge._attach_prompt(
            cmd,
            profile_name="codex",
            profile={"prompt_mode": "arg"},
            prompt="--help",
            require_prompt=True,
        )

        self.assertEqual(result, (None, {5: "<prompt>"}, []))
        self.assertEqual(cmd, ["codex", "exec", "--model", "gpt-5.5", "--", "--help"])

    def test_arg_mode_keeps_regular_prompt_shape(self) -> None:
        cmd = ["opencode", "run"]

        result = bridge._attach_prompt(
            cmd,
            profile_name="glm-via-opencode",
            profile={"prompt_mode": "arg"},
            prompt="review this",
            require_prompt=True,
        )

        self.assertEqual(result, (None, {2: "<prompt>"}, []))
        self.assertEqual(cmd, ["opencode", "run", "review this"])

    def test_prompt_args_template_redaction_preserves_flag_prefix(self) -> None:
        """When a prompt_args element inlines the prompt (e.g. --prompt={value}),
        the redaction map must keep the flag prefix and only swap the value."""
        cmd = ["agent"]

        result = bridge._attach_prompt(
            cmd,
            profile_name="x",
            profile={"prompt_args": ["--prompt={value}", "--mode", "single"]},
            prompt="secret prompt",
            require_prompt=True,
        )

        self.assertEqual(result, (None, {1: "--prompt=<prompt>"}, []))
        self.assertEqual(cmd, ["agent", "--prompt=secret prompt", "--mode", "single"])
        # And confirm that running the redaction yields a useful log line.
        self.assertEqual(
            bridge._redact_prompt(cmd, result[1]),
            ["agent", "--prompt=<prompt>", "--mode", "single"],
        )


class RedactPromptTests(unittest.TestCase):
    def test_empty_map_returns_copy(self) -> None:
        cmd = ["claude", "--print", "secret"]
        result = bridge._redact_prompt(cmd, {})
        self.assertEqual(result, cmd)
        self.assertIsNot(result, cmd)

    def test_replaces_at_indices(self) -> None:
        cmd = ["codex", "exec", "secret-prompt"]
        result = bridge._redact_prompt(cmd, {2: "<prompt>"})
        self.assertEqual(result, ["codex", "exec", "<prompt>"])
        self.assertEqual(cmd[2], "secret-prompt")  # original unchanged

    def test_out_of_range_indices_are_ignored(self) -> None:
        cmd = ["a", "b"]
        result = bridge._redact_prompt(cmd, {5: "<prompt>", -1: "<prompt>"})
        self.assertEqual(result, ["a", "b"])

    def test_per_index_replacement_value(self) -> None:
        cmd = ["agent", "--prompt=foo", "--other=bar"]
        result = bridge._redact_prompt(
            cmd, {1: "--prompt=<prompt>", 2: "<other>"}
        )
        self.assertEqual(result, ["agent", "--prompt=<prompt>", "<other>"])


class DispatchTests(unittest.TestCase):
    def _start_patches(self, tmp_path: Path):
        patches = [
            patch.object(bridge, "LOG_FILE", tmp_path / "runs.log"),
            patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
            patch("sys.stderr"),  # silence [bridge:run]/[bridge:done] banners
        ]
        for p in patches:
            p.start()
        self.addCleanup(lambda: [p.stop() for p in reversed(patches)])

    def test_passes_no_stdin_data_when_no_prompt_is_sent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._start_patches(Path(tmp))
            with patch.object(bridge, "_streamed_run", return_value=0) as run:
                exit_code = bridge._dispatch(
                    action="run",
                    requested="codex",
                    resolved="codex",
                    ctx="",
                    profile={},
                    cmd=["codex", "exec", "prompt"],
                    model=None,
                    effort=None,
                    prompt=None,
                    skills=[],
                    stdin_data=None,
                    redact_map={2: "<prompt>"},
                )

        self.assertEqual(exit_code, 0)
        run.assert_called_once()
        # _streamed_run signature: (cmd, stdin_data, env, cwd_path,
        #                           stdout_path, stderr_path, timeline_path)
        args, _ = run.call_args
        self.assertIsNone(args[1])

    def test_passes_prompt_text_when_stdin_data_is_set(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._start_patches(Path(tmp))
            with patch.object(bridge, "_streamed_run", return_value=0) as run:
                exit_code = bridge._dispatch(
                    action="run",
                    requested="claude",
                    resolved="claude",
                    ctx="",
                    profile={},
                    cmd=["claude", "--print"],
                    model=None,
                    effort=None,
                    prompt="prompt",
                    skills=[],
                    stdin_data="prompt",
                    redact_map={},
                )

        self.assertEqual(exit_code, 0)
        run.assert_called_once()
        args, _ = run.call_args
        self.assertEqual(args[1], "prompt")

    def test_returns_2_when_cwd_does_not_exist(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with patch.object(bridge, "LOG_FILE", Path(tmp) / "runs.log"), patch.object(
                bridge, "RUNS_DIR", Path(tmp) / "runs"
            ), patch("sys.stderr"):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={"cwd": "/nonexistent/path"},
                    cmd=["echo", "hi"],
                    model=None,
                    effort=None,
                    prompt=None,
                    skills=[],
                    stdin_data=None,
                    redact_map={},
                )
        self.assertEqual(exit_code, 2)

    def test_log_record_includes_uuid_output_paths_and_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            runs_dir = tmp_path / "runs"
            self._start_patches(tmp_path)
            with patch.object(bridge, "_streamed_run", return_value=0):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="claude",
                    resolved="claude",
                    ctx="",
                    profile={},
                    cmd=["claude", "--print"],
                    model="claude-opus-4-7",
                    effort="xhigh",
                    prompt="explain X",
                    skills=[],
                    stdin_data="explain X",
                    redact_map={},
                )
            record = json.loads(log_path.read_text().strip())

        self.assertEqual(exit_code, 0)
        self.assertIn("uuid", record)
        self.assertEqual(len(record["uuid"]), 12)
        # output_* are absolute paths (copy-pasteable from the banner / log
        # without resolving a relative root).
        stem = f"{record['uuid']}-claude-claude-opus-4-7"
        self.assertEqual(record["output_stdout"], str(runs_dir / f"{stem}.out"))
        self.assertEqual(record["output_stderr"], str(runs_dir / f"{stem}.err"))
        self.assertEqual(record["output_timeline"], str(runs_dir / f"{stem}.timeline"))
        self.assertEqual(record["prompt"], "explain X")
        # merge_streams is recorded on EVERY audit line — false for the
        # default profile, true for `merge_streams: true` profiles. README
        # and SKILL.md document the field as always present so consumers can
        # tell merged from non-merged captures apart without a missing-key
        # branch.
        self.assertIn("merge_streams", record)
        self.assertFalse(record["merge_streams"])

    def test_runs_dir_is_created_lazily(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            runs_dir = tmp_path / "runs"
            self.assertFalse(runs_dir.exists())
            self._start_patches(tmp_path)
            with patch.object(bridge, "_streamed_run", return_value=0):
                bridge._dispatch(
                    action="run",
                    requested="echo",
                    resolved="echo",
                    ctx="",
                    profile={},
                    cmd=["cat"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                )
            self.assertTrue(runs_dir.is_dir())


class DispatchPopenFailureCleanupTests(unittest.TestCase):
    """When _streamed_run raises before any output is captured, the touched
    empty capture files (.out / .err / .timeline) must be unlinked and the
    audit record's output_* fields must all be null — otherwise users tail
    empty files the bridge advertised."""

    def _run_dispatch(
        self, exc: BaseException, expected_exit: int
    ) -> tuple[Path, dict]:
        tmp_path = Path(tempfile.mkdtemp())
        log_path = tmp_path / "runs.log"
        runs_dir = tmp_path / "runs"
        with patch.object(bridge, "LOG_FILE", log_path), patch.object(
            bridge, "RUNS_DIR", runs_dir
        ), patch("sys.stderr"):
            with patch.object(bridge, "_streamed_run", side_effect=exc):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["nonexistent-binary-xyz"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                )
        self.assertEqual(exit_code, expected_exit)
        record = json.loads(log_path.read_text().strip())
        return runs_dir, record

    def _assert_output_paths_null(self, record: dict) -> None:
        self.assertIsNone(record["output_stdout"])
        self.assertIsNone(record["output_stderr"])
        self.assertIsNone(record["output_timeline"])

    def test_file_not_found_drops_empty_capture_files(self) -> None:
        runs_dir, record = self._run_dispatch(FileNotFoundError(), 127)
        self._assert_output_paths_null(record)
        # The runs/ dir may exist (created by _build_output_paths) but it must
        # contain no capture files — every touched empty one was unlinked.
        leftovers = list(runs_dir.iterdir()) if runs_dir.exists() else []
        self.assertEqual(leftovers, [])

    def test_permission_error_drops_empty_capture_files(self) -> None:
        _, record = self._run_dispatch(PermissionError(), 126)
        self._assert_output_paths_null(record)

    def test_oserror_drops_empty_capture_files(self) -> None:
        _, record = self._run_dispatch(OSError("disk full"), 127)
        self._assert_output_paths_null(record)

    def test_non_empty_capture_file_is_kept(self) -> None:
        """If _streamed_run wrote something to one of the three capture files
        before raising OSError, the partial content should be preserved
        (st_size > 0 → no unlink)."""
        tmp_path = Path(tempfile.mkdtemp())
        log_path = tmp_path / "runs.log"
        runs_dir = tmp_path / "runs"

        def write_then_raise(
            cmd, stdin_data, env, cwd_path, stdout_path, stderr_path,
            timeline_path, *, merge_streams=False,
        ):
            stdout_path.write_bytes(b"partial stdout\n")
            raise OSError("mid-stream failure")

        with patch.object(bridge, "LOG_FILE", log_path), patch.object(
            bridge, "RUNS_DIR", runs_dir
        ), patch("sys.stderr"):
            with patch.object(bridge, "_streamed_run", side_effect=write_then_raise):
                bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                )
        # The audit record drops output_* on any Popen-style failure (we can't
        # tell from the exception alone whether writes happened). But the
        # partial file with content stays on disk; its empty siblings (.err,
        # .timeline) get unlinked.
        leftovers = sorted(p.name for p in runs_dir.iterdir())
        self.assertEqual(len(leftovers), 1)
        self.assertTrue(leftovers[0].endswith(".out"))
        self.assertEqual(
            (runs_dir / leftovers[0]).read_bytes(), b"partial stdout\n"
        )


class RunsLogAppendIsParseableTests(unittest.TestCase):
    """Smoke test for the fcntl.flock-bracketed append path: a single-threaded
    write must still produce a parseable JSONL record (no double-locking,
    no truncation)."""

    def test_single_write_produces_one_jsonl_record(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", tmp_path / "runs"
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", return_value=0
            ):
                bridge._dispatch(
                    action="run",
                    requested="echo",
                    resolved="echo",
                    ctx="",
                    profile={},
                    cmd=["cat"],
                    model=None,
                    effort=None,
                    prompt="x" * 16384,  # > BufferedWriter default chunk
                    skills=[],
                    stdin_data="x" * 16384,
                    redact_map={},
                )
            lines = log_path.read_text().splitlines()
        self.assertEqual(len(lines), 1)
        record = json.loads(lines[0])
        self.assertEqual(record["prompt"], "x" * 16384)


class ProcessSkillsTests(unittest.TestCase):
    """The single-pass core both `_apply_skill_format` and `_extract_skills`
    wrap, and that `_attach_prompt` calls directly on the live path."""

    def test_returns_rewritten_prompt_and_skill_list(self) -> None:
        rewritten, skills = bridge._process_skills(
            "/skill:foo and /skill:bar", "/{name}"
        )
        self.assertEqual(rewritten, "/foo and /bar")
        self.assertEqual(skills, ["foo", "bar"])

    def test_dedupes_in_first_seen_order(self) -> None:
        rewritten, skills = bridge._process_skills(
            "/skill:foo /skill:bar /skill:foo", "/{name}"
        )
        self.assertEqual(rewritten, "/foo /bar /foo")
        self.assertEqual(skills, ["foo", "bar"])

    def test_passthrough_when_no_refs(self) -> None:
        rewritten, skills = bridge._process_skills("just text", "/{name}")
        self.assertEqual(rewritten, "just text")
        self.assertEqual(skills, [])


class DispatchDoesNotReparsePromptTests(unittest.TestCase):
    """Regression guard: _dispatch must NOT call _extract_skills — the live
    path already collected `skills` in _attach_prompt and forwards it. Calling
    again would double-parse the prompt."""

    def test_skills_param_is_used_verbatim(self) -> None:
        sentinel = ["x-from-attach", "y-from-attach"]
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = [
                patch.object(bridge, "LOG_FILE", log_path),
                patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
                patch("sys.stderr"),
                # If _dispatch ever calls _extract_skills, this would override
                # the param and the assertion below would fail.
                patch.object(
                    bridge, "_extract_skills", side_effect=AssertionError(
                        "_dispatch must not call _extract_skills"
                    )
                ),
            ]
            for p in patches:
                p.start()
            try:
                with patch.object(bridge, "_streamed_run", return_value=0):
                    bridge._dispatch(
                        action="run",
                        requested="echo",
                        resolved="echo",
                        ctx="",
                        profile={},
                        cmd=["cat"],
                        model=None,
                        effort=None,
                        prompt="/skill:something",  # has a ref, but skills is forced
                        skills=sentinel,
                        stdin_data="...",
                        redact_map={},
                    )
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())
        self.assertEqual(record["skills"], sentinel)


class ExtractSkillsTests(unittest.TestCase):
    def test_returns_empty_when_prompt_is_none(self) -> None:
        self.assertEqual(bridge._extract_skills(None), [])

    def test_returns_empty_when_no_skill_references(self) -> None:
        self.assertEqual(bridge._extract_skills("just a normal prompt"), [])

    def test_extracts_leading_prefix(self) -> None:
        self.assertEqual(
            bridge._extract_skills("/skill:review tweak this"), ["review"]
        )

    def test_extracts_multiple_references_in_order(self) -> None:
        self.assertEqual(
            bridge._extract_skills(
                "use /skill:foo first, then call /skill:bar"
            ),
            ["foo", "bar"],
        )

    def test_deduplicates(self) -> None:
        self.assertEqual(
            bridge._extract_skills("/skill:foo and /skill:bar and /skill:foo again"),
            ["foo", "bar"],
        )

    def test_keeps_plugin_namespaced_names(self) -> None:
        self.assertEqual(
            bridge._extract_skills("/skill:plugin:review here"),
            ["plugin:review"],
        )

    def test_ignores_skill_substring_inside_word(self) -> None:
        # "xyz/skill:foo" — the slash is preceded by a word char, so it must NOT match.
        self.assertEqual(bridge._extract_skills("xyz/skill:foo"), [])

    def test_log_record_includes_skills_field(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = [
                patch.object(bridge, "LOG_FILE", log_path),
                patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
                patch("sys.stderr"),
            ]
            for p in patches:
                p.start()
            try:
                with patch.object(bridge, "_streamed_run", return_value=0):
                    bridge._dispatch(
                        action="run",
                        requested="echo",
                        resolved="echo",
                        ctx="",
                        profile={},
                        cmd=["cat"],
                        model=None,
                        effort=None,
                        prompt="/skill:review check /skill:foo",
                        skills=["review", "foo"],
                        stdin_data="...",
                        redact_map={},
                    )
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())
        self.assertEqual(record["skills"], ["review", "foo"])


class SanitizeFilenameTests(unittest.TestCase):
    def test_replaces_slashes_in_model_id(self) -> None:
        self.assertEqual(
            bridge._sanitize_for_filename("zai-coding-plan/glm-5.1"),
            "zai-coding-plan_glm-5.1",
        )

    def test_truncates_overly_long_input(self) -> None:
        self.assertEqual(len(bridge._sanitize_for_filename("x" * 200)), 80)

    def test_all_unsafe_input_collapses_to_underscore(self) -> None:
        self.assertEqual(bridge._sanitize_for_filename("///"), "_")


class StreamedRunTests(unittest.TestCase):
    """Smoke-test _streamed_run end-to-end against `cat` / `sh -c` (always available)."""

    def _paths(self, tmp: str) -> tuple[Path, Path, Path]:
        return (Path(tmp) / "x.out", Path(tmp) / "x.err", Path(tmp) / "x.timeline")

    def test_stdin_routes_to_stdout_capture(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path, stderr_path, timeline_path = self._paths(tmp)
            exit_code = bridge._streamed_run(
                cmd=["cat"],
                stdin_data="hello bridge",
                env=os.environ.copy(),
                cwd_path=None,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                timeline_path=timeline_path,
            )
            self.assertEqual(exit_code, 0)
            self.assertEqual(stdout_path.read_text(), "hello bridge")
            # cat (no input on stderr) → stderr capture stays empty.
            self.assertEqual(stderr_path.read_bytes(), b"")
            # Timeline must record exactly one stdout chunk.
            timeline_lines = timeline_path.read_text().splitlines()
            self.assertEqual(len(timeline_lines), 1)
            ts, stream, length = timeline_lines[0].split()
            self.assertTrue(ts.isdigit())
            self.assertEqual(stream, "stdout")
            self.assertEqual(length, str(len("hello bridge")))

    def test_stderr_routes_to_stderr_capture(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path, stderr_path, timeline_path = self._paths(tmp)
            bridge._streamed_run(
                cmd=["sh", "-c", "echo to_err >&2"],
                stdin_data=None,
                env=os.environ.copy(),
                cwd_path=None,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                timeline_path=timeline_path,
            )
            self.assertEqual(stdout_path.read_bytes(), b"")
            self.assertEqual(stderr_path.read_text(), "to_err\n")
            self.assertIn("stderr", timeline_path.read_text())

    def test_returns_nonzero_exit_on_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path, stderr_path, timeline_path = self._paths(tmp)
            exit_code = bridge._streamed_run(
                cmd=["sh", "-c", "exit 7"],
                stdin_data=None,
                env=os.environ.copy(),
                cwd_path=None,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                timeline_path=timeline_path,
            )
            self.assertEqual(exit_code, 7)

    def test_chunks_arrive_before_eof(self) -> None:
        """Slow producers must stream in real time. Plain `read(4096)` would
        accumulate the full output before returning the first chunk; `read1`
        delivers each kernel write as a separate timeline entry."""
        producer = (
            "import sys, time;"
            "sys.stdout.write('a'); sys.stdout.flush(); time.sleep(0.2);"
            "sys.stdout.write('b'); sys.stdout.flush()"
        )
        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path, stderr_path, timeline_path = self._paths(tmp)
            bridge._streamed_run(
                cmd=[sys.executable, "-c", producer],
                stdin_data=None,
                env=os.environ.copy(),
                cwd_path=None,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                timeline_path=timeline_path,
            )
            # Two flushed writes → two timeline entries on stdout.
            stdout_entries = [
                line for line in timeline_path.read_text().splitlines()
                if " stdout " in line
            ]
            self.assertEqual(
                len(stdout_entries), 2,
                f"expected real-time streaming (2 chunks), got {stdout_entries!r}",
            )
            self.assertEqual(stdout_path.read_bytes(), b"ab")


class LoadProfilesValidationTests(unittest.TestCase):
    """Misconfigured profiles must fail at load time, not at run time."""

    def _run_load_with_profile(self, payload: dict) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "_zztest.json").write_text(json.dumps(payload))
            with patch.object(bridge, "PROFILES_DIR", tmp_path), patch("sys.stderr"):
                try:
                    bridge.load_profiles()
                except SystemExit as e:
                    return int(e.code or 0)
        return 0

    def test_env_must_be_string_dict(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "env": ["x"]}), 2
        )
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "env": {"K": 1}}), 2
        )

    def test_explicit_null_env_is_rejected(self) -> None:
        # `"env": null` would otherwise crash _dispatch (`None.items()`) at
        # run time — fail at load instead.
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "env": None}), 2
        )

    def test_cwd_must_be_string(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "cwd": ["x"]}), 2
        )

    def test_cwd_must_not_be_empty(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "cwd": ""}), 2
        )

    def test_review_block_contradictory_model_default_is_rejected(self) -> None:
        # Top-level model + review.model_args=null = every `bridge review` errors.
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "model": "x",
                    "review": {"command": ["cat"], "model_args": None},
                }
            ),
            2,
        )

    def test_review_block_contradictory_effort_default_is_rejected(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "effort": "high",
                    "review": {"command": ["cat"], "effort_args": None},
                }
            ),
            2,
        )

    def test_review_block_without_default_can_null_args(self) -> None:
        # No top-level default → review.model_args=null is fine (no flag to send).
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "model_args": None},
                }
            ),
            0,
        )

    def test_default_with_null_args_template_is_rejected(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "model": "foo", "model_args": None}
            ),
            2,
        )
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "effort": "high", "effort_args": None}
            ),
            2,
        )

    def test_prompt_args_must_contain_value_placeholder(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "prompt_args": ["-p"]}
            ),
            2,
        )

    def test_model_args_must_contain_value_placeholder(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "model_args": ["--model"]}
            ),
            2,
        )

    def test_effort_args_must_contain_value_placeholder(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "effort_args": ["--effort"]}
            ),
            2,
        )

    def test_review_model_args_must_contain_value_placeholder(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "model_args": ["--model"]},
                }
            ),
            2,
        )

    def test_description_must_be_string(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "description": 123}),
            2,
        )

    def test_command_elements_must_be_strings(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat", 123]}),
            2,
        )

    def test_review_command_elements_must_be_strings(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat", 123]},
                }
            ),
            2,
        )

    def test_model_must_be_string(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "model": 123}),
            2,
        )

    def test_effort_must_be_string(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "effort": 123}),
            2,
        )

    def test_unknown_top_level_key_is_rejected(self) -> None:
        # A typo like `models` (vs `model`) must fail at load time, not be
        # silently ignored — otherwise users debug "feature didn't work" forever.
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "models": "x"}), 2
        )

    def test_unknown_review_key_is_rejected(self) -> None:
        # `review.model` is silently ignored by cmd_review (defaults always come
        # from the top level); reject it so users don't think it works.
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "model": "x"},
                }
            ),
            2,
        )

    def test_well_formed_profile_loads(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "ok.json").write_text(
                json.dumps({"command": ["cat"], "env": {"K": "v"}, "cwd": "."})
            )
            with patch.object(bridge, "PROFILES_DIR", tmp_path):
                profiles = bridge.load_profiles()
        self.assertIn("ok", profiles)

    def test_bundled_codex_profile_uses_stdin_prompt_mode(self) -> None:
        profiles = bridge.load_profiles()

        self.assertEqual(profiles["codex"]["prompt_mode"], "stdin")

    def test_bundled_cursor_profile_uses_positional_prompt_mode(self) -> None:
        profiles = bridge.load_profiles()

        self.assertEqual(profiles["cursor"]["prompt_mode"], "arg")
        self.assertNotIn("prompt_args", profiles["cursor"])
        self.assertIn("--print", profiles["cursor"]["command"])

    def test_bundled_cursor_profile_separates_leading_dash_prompt(self) -> None:
        profiles = bridge.load_profiles()
        cmd = list(profiles["cursor"]["command"])

        result = bridge._attach_prompt(
            cmd,
            profile_name="cursor",
            profile=profiles["cursor"],
            prompt="--help",
            require_prompt=True,
        )

        self.assertEqual(result, (None, {len(cmd) - 1: "<prompt>"}, []))
        self.assertEqual(cmd[-2:], ["--", "--help"])


class SkillFormatTests(unittest.TestCase):
    def test_no_prefix_passes_through(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("just a normal prompt", "/{name}"),
            "just a normal prompt",
        )

    def test_claude_style_strips_skill_prefix(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("/skill:review tweak this", "/{name}"),
            "/review tweak this",
        )

    def test_codex_style_uses_dollar_prefix(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("/skill:review tweak this", "${name}"),
            "$review tweak this",
        )

    def test_kimi_default_is_passthrough(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("/skill:review tweak", bridge.DEFAULT_SKILL_FORMAT),
            "/skill:review tweak",
        )

    def test_skill_only_prompt_has_no_trailing_separator(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("/skill:review", "/{name}"),
            "/review",
        )

    def test_plugin_namespaced_skill_name_is_preserved(self) -> None:
        self.assertEqual(
            bridge._apply_skill_format("/skill:plugin:review body", "/{name}"),
            "/plugin:review body",
        )

    def test_rewrites_every_occurrence_not_just_leading(self) -> None:
        """Audit (`skills` field) and rewrite must agree on what counts as a
        reference — both find every word-boundary `/skill:<name>`, not just the
        leading prefix. Otherwise the agent receives a half-translated prompt."""
        self.assertEqual(
            bridge._apply_skill_format(
                "/skill:foo and then /skill:bar please", "/{name}"
            ),
            "/foo and then /bar please",
        )
        self.assertEqual(
            bridge._apply_skill_format(
                "do /skill:foo first, then /skill:bar", "${name}"
            ),
            "do $foo first, then $bar",
        )

    def test_does_not_rewrite_skill_substring_inside_word(self) -> None:
        # Same lookbehind protection as _extract_skills — `path/skill:foo` is
        # left alone so embedded paths and URLs aren't mangled.
        self.assertEqual(
            bridge._apply_skill_format("see path/skill:foo here", "/{name}"),
            "see path/skill:foo here",
        )

    def test_attach_prompt_applies_skill_format_in_arg_mode(self) -> None:
        cmd = ["opencode", "run"]
        result = bridge._attach_prompt(
            cmd,
            profile_name="glm-via-opencode",
            profile={"prompt_mode": "arg", "skill_format": "/{name}"},
            prompt="/skill:review tweak",
            require_prompt=True,
        )
        self.assertEqual(result, (None, {2: "<prompt>"}, ["review"]))
        self.assertEqual(cmd, ["opencode", "run", "/review tweak"])

    def test_attach_prompt_applies_skill_format_in_stdin_mode(self) -> None:
        cmd = ["codex", "exec"]
        result = bridge._attach_prompt(
            cmd,
            profile_name="codex",
            profile={"prompt_mode": "stdin", "skill_format": "${name}"},
            prompt="/skill:review the diff",
            require_prompt=True,
        )
        self.assertEqual(result, ("$review the diff", {}, ["review"]))
        self.assertEqual(cmd, ["codex", "exec"])


class SkillFormatValidationTests(unittest.TestCase):
    def _run_load_with_profile(self, payload: dict) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "_zztest.json").write_text(json.dumps(payload))
            with patch.object(bridge, "PROFILES_DIR", tmp_path), patch("sys.stderr"):
                try:
                    bridge.load_profiles()
                except SystemExit as e:
                    return int(e.code or 0)
        return 0

    def test_skill_format_must_be_string(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "skill_format": 1}), 2
        )

    def test_skill_format_must_contain_name_placeholder(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "skill_format": "/foo"}), 2
        )

    def test_valid_skill_format_loads(self) -> None:
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "skill_format": "/{name}"}), 0
        )

    def test_explicit_null_skill_format_is_rejected(self) -> None:
        # An explicit `"skill_format": null` would otherwise crash _process_skills
        # at runtime (None.replace) — fail fast at load time instead.
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "skill_format": None}), 2
        )

    def test_explicit_null_prompt_mode_is_rejected(self) -> None:
        # `"prompt_mode": null` would otherwise reach _attach_prompt as an
        # unknown mode and exit 2 at run time — reject it at load time.
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "prompt_mode": None}), 2
        )


class ResolveAgentTests(unittest.TestCase):
    def test_explicit_personal_name_is_unchanged(self) -> None:
        with patch.object(bridge, "detect_context", return_value="work"):
            resolved, ctx = bridge.resolve_agent(
                "claude-personal", {"claude": {}, "claude-personal": {}, "claude-work": {}}
            )
        self.assertEqual((resolved, ctx), ("claude-personal", ""))

    def test_no_context_returns_requested_unchanged(self) -> None:
        with patch.object(bridge, "detect_context", return_value=""):
            resolved, ctx = bridge.resolve_agent("claude", {"claude": {}, "claude-personal": {}})
        self.assertEqual((resolved, ctx), ("claude", ""))

    def test_routes_to_variant_when_present(self) -> None:
        with patch.object(bridge, "detect_context", return_value="personal"):
            resolved, ctx = bridge.resolve_agent("claude", {"claude": {}, "claude-personal": {}})
        self.assertEqual((resolved, ctx), ("claude-personal", "personal"))

    def test_falls_back_when_variant_missing(self) -> None:
        with patch.object(bridge, "detect_context", return_value="work"):
            resolved, ctx = bridge.resolve_agent("codex", {"codex": {}})
        self.assertEqual((resolved, ctx), ("codex", "work"))


class CmdReviewTests(unittest.TestCase):
    def _ns(self, **overrides) -> SimpleNamespace:
        defaults = dict(
            agent="agent", prompt=None, model=None, effort=None, no_context=True,
        )
        defaults.update(overrides)
        return SimpleNamespace(**defaults)

    def _patches(self, tmp_path: Path, profile: dict):
        """Common patch stack: log/runs paths, silenced stderr, stubbed stdin
        (TTY-on so the bridge doesn't try to read from the test runner's fd 0)
        and patched profile loader. Each test still picks how to stub
        _streamed_run."""
        return [
            patch.object(bridge, "LOG_FILE", tmp_path / "runs.log"),
            patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
            patch("sys.stderr"),
            patch.object(bridge.sys.stdin, "isatty", return_value=True),
            patch.object(bridge, "load_profiles", return_value={"agent": profile}),
        ]

    def test_review_block_inherits_profile_args_when_omitted(self) -> None:
        """If the review block omits model_args/effort_args, the profile's main
        settings should be used (not the hard-coded defaults)."""
        profile = {
            "command": ["agent"],
            "model": "m1",
            "model_args": ["--modell", "{value}"],
            "effort": "high",
            "effort_args": ["--eff", "{value}"],
            "review": {"command": ["agent", "review"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                exit_code = bridge.cmd_review(self._ns())
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()

        self.assertEqual(exit_code, 0)
        run.assert_called_once()
        # _streamed_run signature: (cmd, stdin_data, env, cwd_path,
        #                           stdout_path, stderr_path, timeline_path)
        pos_args, _ = run.call_args
        self.assertEqual(
            pos_args[0],
            ["agent", "review", "--modell", "m1", "--eff", "high"],
        )

    def test_review_with_prompt_appends_positional_for_review_block(self) -> None:
        """codex-style: caller -p <text> appends as the trailing positional
        argument (`codex review --uncommitted <text>`)."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "review": {"command": ["codex", "review", "--uncommitted"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                exit_code = bridge.cmd_review(
                    self._ns(prompt="focus on auth changes")
                )
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        self.assertEqual(exit_code, 0)
        pos_args, _ = run.call_args
        # Caller prompt appended as the trailing positional arg.
        self.assertEqual(
            pos_args[0],
            ["codex", "review", "--uncommitted", "focus on auth changes"],
        )
        # Stdin must NOT be set — codex review reads its prompt from argv.
        self.assertIsNone(pos_args[1])
        # Audit log: prompt field carries the caller's text; command redacts it.
        self.assertEqual(record["prompt"], "focus on auth changes")
        self.assertEqual(
            record["command"],
            ["codex", "review", "--uncommitted", "<prompt>"],
        )

    def test_review_without_prompt_review_block_unchanged(self) -> None:
        """Backwards compat: bridge review codex (no -p) still sends no prompt
        and audits prompt:null."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "review": {"command": ["codex", "review", "--uncommitted"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns())
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        pos_args, _ = run.call_args
        self.assertEqual(pos_args[0], ["codex", "review", "--uncommitted"])
        self.assertIsNone(pos_args[1])
        self.assertIsNone(record["prompt"])
        self.assertEqual(
            record["command"], ["codex", "review", "--uncommitted"]
        )

    def test_review_with_prompt_extends_review_keyword_stdin_mode(self) -> None:
        """claude-style: caller -p <text> extends the slash command via stdin
        as `/review <text>`. Audit captures the assembled prompt; command
        stays clean (stdin doesn't enter argv)."""
        profile = {
            "command": ["claude", "--print"],
            "prompt_mode": "stdin",
            "skill_format": "/{name}",
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns(prompt="address all findings"))
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        pos_args, _ = run.call_args
        # cmd is unchanged (stdin mode); stdin_data carries `/review <text>`.
        self.assertEqual(pos_args[0], ["claude", "--print"])
        self.assertEqual(pos_args[1], "/review address all findings")
        self.assertEqual(record["prompt"], "/review address all findings")
        self.assertEqual(record["command"], ["claude", "--print"])

    def test_review_with_prompt_extends_review_keyword_arg_mode(self) -> None:
        """opencode-routed: caller -p <text> extends the slash command via
        argv as `/review <text>`; the prompt portion is redacted from the
        logged command."""
        profile = {
            "command": ["opencode", "run"],
            "prompt_mode": "arg",
            "skill_format": "/{name}",
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns(prompt="check the new tee thread"))
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        pos_args, _ = run.call_args
        self.assertEqual(
            pos_args[0],
            ["opencode", "run", "/review check the new tee thread"],
        )
        self.assertIsNone(pos_args[1])
        self.assertEqual(
            record["prompt"], "/review check the new tee thread"
        )
        self.assertEqual(
            record["command"], ["opencode", "run", "<prompt>"],
        )

    def test_review_caller_prompt_translates_skill_references(self) -> None:
        """`/skill:foo` in the caller prompt must be rewritten per profile —
        same contract as cmd_run, so a single canonical input form works
        across the run/review subcommands."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "skill_format": "${name}",
            "review": {"command": ["codex", "review", "--uncommitted"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(
                    self._ns(prompt="apply /skill:simplify after review")
                )
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        pos_args, _ = run.call_args
        # The agent receives the rewritten form ($simplify); the audit prompt
        # field keeps the caller's original text (pre-rewrite).
        self.assertEqual(
            pos_args[0],
            ["codex", "review", "--uncommitted",
             "apply $simplify after review"],
        )
        self.assertEqual(record["prompt"], "apply /skill:simplify after review")
        self.assertEqual(record["skills"], ["simplify"])

    def test_review_empty_piped_stdin_is_treated_as_no_prompt(self) -> None:
        """An empty pipe must not be confused with a real prompt (matches
        cmd_run's empty-stdin handling)."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "review": {"command": ["codex", "review", "--uncommitted"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            patches = [
                patch.object(bridge, "LOG_FILE", tmp_path / "runs.log"),
                patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
                patch("sys.stderr"),
                patch.object(bridge.sys.stdin, "isatty", return_value=False),
                patch.object(bridge.sys.stdin, "read", return_value=""),
                patch.object(
                    bridge, "load_profiles", return_value={"agent": profile}
                ),
                patch.object(bridge, "_streamed_run", return_value=0),
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns())
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()
            record = json.loads(log_path.read_text().strip())

        pos_args, _ = run.call_args
        self.assertEqual(pos_args[0], ["codex", "review", "--uncommitted"])
        self.assertIsNone(record["prompt"])

    def test_review_piped_stdin_is_used_when_no_dash_p(self) -> None:
        """If -p is omitted but stdin is piped, the caller prompt comes from
        stdin (matches cmd_run)."""
        profile = {
            "command": ["claude", "--print"],
            "prompt_mode": "stdin",
            "skill_format": "/{name}",
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            patches = [
                patch.object(bridge, "LOG_FILE", tmp_path / "runs.log"),
                patch.object(bridge, "RUNS_DIR", tmp_path / "runs"),
                patch("sys.stderr"),
                patch.object(bridge.sys.stdin, "isatty", return_value=False),
                patch.object(
                    bridge.sys.stdin, "read", return_value="from piped stdin"
                ),
                patch.object(
                    bridge, "load_profiles", return_value={"agent": profile}
                ),
                patch.object(bridge, "_streamed_run", return_value=0),
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns())
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()

        pos_args, _ = run.call_args
        self.assertEqual(pos_args[1], "/review from piped stdin")

    def test_scope_default_is_appended_when_no_caller_prompt(self) -> None:
        """`scope_default` keeps the no-prompt invocation working — for codex
        that means `codex review --uncommitted` so behavior matches the
        pre-prompt era. The flag lands AFTER model/effort flags so codex's
        argparser sees it before any positional."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "model": "gpt-5.5",
            "effort": "high",
            "review": {
                "command": ["codex", "review"],
                "scope_default": ["--uncommitted"],
                "model_args": ["-c", "model={value}"],
                "effort_args": ["-c", "model_reasoning_effort={value}"],
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns())
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()

        pos_args, _ = run.call_args
        self.assertEqual(
            pos_args[0],
            ["codex", "review",
             "-c", "model=gpt-5.5",
             "-c", "model_reasoning_effort=high",
             "--uncommitted"],
        )

    def test_scope_default_is_dropped_when_caller_prompt_given(self) -> None:
        """The whole point of scope_default: when the caller hands codex a
        custom prompt, the scope flag MUST be omitted (codex review treats
        --uncommitted as mutually exclusive with [PROMPT])."""
        profile = {
            "command": ["codex", "exec"],
            "prompt_mode": "stdin",
            "model": "gpt-5.5",
            "effort": "high",
            "review": {
                "command": ["codex", "review"],
                "scope_default": ["--uncommitted"],
                "model_args": ["-c", "model={value}"],
                "effort_args": ["-c", "model_reasoning_effort={value}"],
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                bridge.cmd_review(self._ns(prompt="address findings"))
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()

        pos_args, _ = run.call_args
        self.assertNotIn("--uncommitted", pos_args[0])
        self.assertEqual(pos_args[0][-1], "address findings")
        self.assertEqual(pos_args[0][1], "review")  # subcommand still there

    def test_review_block_without_skill_format_does_not_crash(self) -> None:
        """If a review-block profile has no `skill_format`, the synthesized
        attach_profile must fall back to the default (passthrough) — passing
        `None` would crash `_process_skills` on any /skill: ref."""
        profile = {
            "command": ["agent"],
            "prompt_mode": "stdin",
            "review": {"command": ["agent", "review"]},
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            patches = self._patches(tmp_path, profile) + [
                patch.object(bridge, "_streamed_run", return_value=0)
            ]
            started = [p.start() for p in patches]
            try:
                exit_code = bridge.cmd_review(self._ns(prompt="apply /skill:foo"))
                run = started[-1]
            finally:
                for p in reversed(patches):
                    p.stop()

        self.assertEqual(exit_code, 0)
        pos_args, _ = run.call_args
        # No skill_format → default passthrough → /skill:foo unchanged.
        self.assertEqual(
            pos_args[0], ["agent", "review", "apply /skill:foo"]
        )

    def test_bundled_codex_review_layout(self) -> None:
        """Sanity check the on-disk codex.json: review.command + scope_default
        produce the same arg sequence as the prior `["codex","review",
        "--uncommitted"]` baked-in form when no prompt is given. Guards
        against profile drift breaking the no-prompt invocation."""
        profiles = bridge.load_profiles()
        codex = profiles["codex"]
        self.assertEqual(codex["review"]["command"], ["codex", "review"])
        self.assertEqual(codex["review"]["scope_default"], ["--uncommitted"])


class ScopeDefaultValidationTests(unittest.TestCase):
    """`scope_default` must be a list of strings; anything else fails at load."""

    def _run_load_with_profile(self, payload: dict) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "_zztest.json").write_text(json.dumps(payload))
            with patch.object(bridge, "PROFILES_DIR", tmp_path), patch("sys.stderr"):
                try:
                    bridge.load_profiles()
                except SystemExit as e:
                    return int(e.code or 0)
        return 0

    def test_scope_default_must_be_list(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "scope_default": "x"},
                }
            ),
            2,
        )

    def test_scope_default_elements_must_be_strings(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "scope_default": ["ok", 1]},
                }
            ),
            2,
        )

    def test_explicit_null_scope_default_is_rejected(self) -> None:
        # `"scope_default": null` would otherwise crash cmd_review
        # (`cmd.extend(None)` → TypeError) when no caller prompt is given.
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "scope_default": None},
                }
            ),
            2,
        )

    def test_scope_default_empty_list_is_valid(self) -> None:
        # Empty list = no default scope, but the field is present (e.g. user
        # explicitly cleared it). Should still load.
        self.assertEqual(
            self._run_load_with_profile(
                {
                    "command": ["cat"],
                    "review": {"command": ["cat"], "scope_default": []},
                }
            ),
            0,
        )


class ContextOutputTests(unittest.TestCase):
    def test_outputs_trailing_newline(self) -> None:
        import subprocess as sp

        result = sp.run(
            [sys.executable, str(SCRIPTS_DIR / "context.py")],
            capture_output=True,
            text=True,
            env={},
        )
        self.assertEqual(result.stdout, "\n")


class ContextDetectTests(unittest.TestCase):
    def setUp(self) -> None:
        # Isolate every test from the developer's actual environment.
        self.env_patcher = patch.dict(
            "os.environ",
            {},
            clear=True,
        )
        self.env_patcher.start()
        # Re-import context.py freshly so the module-level cwd doesn't matter.
        import importlib
        global context_mod
        context_spec = importlib.util.spec_from_file_location(
            "context_under_test", SCRIPTS_DIR / "context.py"
        )
        assert context_spec is not None and context_spec.loader is not None
        context_mod = importlib.util.module_from_spec(context_spec)
        context_spec.loader.exec_module(context_mod)

    def tearDown(self) -> None:
        self.env_patcher.stop()

    def test_opencode_profile_takes_precedence(self) -> None:
        os_env = sys.modules["os"].environ
        os_env["OPENCODE_PROFILE"] = "personal"
        os_env["CLAUDE_CONFIG_DIR"] = "/tmp/work-config"
        os_env["CLAUDE_WORK_DIR"] = "/tmp/work-config"
        self.assertEqual(context_mod.detect(), "personal")

    def test_unknown_opencode_profile_is_ignored(self) -> None:
        sys.modules["os"].environ["OPENCODE_PROFILE"] = "weird"
        self.assertEqual(context_mod.detect(), "")

    def test_claude_dir_substring_does_not_falsely_match_username(self) -> None:
        # /Users/personal-foo/.config should NOT match — substring is in a username.
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "personal-foo-something"
            fake.mkdir()
            sys.modules["os"].environ["CLAUDE_CONFIG_DIR"] = str(fake)
            self.assertEqual(context_mod.detect(), "")

    def test_claude_dir_boundary_match_works(self) -> None:
        # Path with `.claude-personal` as a component should match.
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / ".claude-personal"
            fake.mkdir()
            sys.modules["os"].environ["CLAUDE_CONFIG_DIR"] = str(fake)
            self.assertEqual(context_mod.detect(), "personal")

    def test_claude_dir_parent_work_component_does_not_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "work" / ".claude"
            fake.mkdir(parents=True)
            sys.modules["os"].environ["CLAUDE_CONFIG_DIR"] = str(fake)
            self.assertEqual(context_mod.detect(), "")

    def test_xdg_share_personal_suffix_matches(self) -> None:
        sys.modules["os"].environ["XDG_DATA_HOME"] = "/Users/foo/.local/share-personal"
        self.assertEqual(context_mod.detect(), "personal")

    def test_xdg_substring_does_not_falsely_match(self) -> None:
        # /Users/foo/myshare-personal must NOT match — `share-personal` only
        # counts when it is a whole path component.
        sys.modules["os"].environ["XDG_DATA_HOME"] = "/Users/foo/myshare-personal"
        self.assertEqual(context_mod.detect(), "")


class MainTests(unittest.TestCase):
    def test_keyboard_interrupt_returns_130(self) -> None:
        with patch.object(bridge, "cmd_list", side_effect=KeyboardInterrupt):
            with patch("sys.argv", ["bridge", "list"]):
                exit_code = bridge.main()
        self.assertEqual(exit_code, 130)


class CallerSuppliedUuidTests(unittest.TestCase):
    """`--uuid <hex>` lets the orchestrator predetermine the run UUID so it
    can compute the capture-file paths before the bridge even starts."""

    def test_caller_uuid_is_used_for_capture_filenames(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            runs_dir = tmp_path / "runs"
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", runs_dir
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", return_value=0
            ):
                bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    caller_uuid="deadbeef0123",
                )
            record = json.loads(log_path.read_text().strip())
        self.assertEqual(record["uuid"], "deadbeef0123")
        self.assertEqual(
            record["output_stdout"], str(runs_dir / "deadbeef0123-agent.out")
        )

    def test_caller_uuid_invalid_format_is_rejected(self) -> None:
        ns = SimpleNamespace(output_dir=None, uuid="not-hex!")
        with patch("sys.stderr"):
            result = bridge._resolve_runs_dir_and_uuid(ns)
        self.assertEqual(result, 2)

    def test_caller_uuid_uppercase_is_rejected(self) -> None:
        # `uuid.uuid4().hex[:12]` always yields lowercase; require the same
        # so capture-file names are case-consistent across runs and shells.
        ns = SimpleNamespace(output_dir=None, uuid="DEADBEEF0123")
        with patch("sys.stderr"):
            result = bridge._resolve_runs_dir_and_uuid(ns)
        self.assertEqual(result, 2)

    def test_caller_uuid_clobber_is_rejected(self) -> None:
        """If a caller-supplied UUID matches an existing non-empty capture
        file, abort with exit 2 rather than silently overwriting prior output."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            runs_dir = tmp_path / "runs"
            runs_dir.mkdir()
            (runs_dir / "deadbeef0123-agent.out").write_bytes(b"prior content")
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", runs_dir
            ), patch("sys.stderr"):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    caller_uuid="deadbeef0123",
                )
            self.assertEqual(exit_code, 2)
            # Prior content must remain intact.
            self.assertEqual(
                (runs_dir / "deadbeef0123-agent.out").read_bytes(), b"prior content"
            )

    def test_caller_uuid_reuse_across_capture_stems_is_rejected(self) -> None:
        """The UUID namespace is shared across all agents/models in a runs dir;
        reusing it would make `$DIR/$UUID-*` lookups ambiguous."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            runs_dir = tmp_path / "runs"
            runs_dir.mkdir()
            (runs_dir / "deadbeef0123-other-agent.out").write_bytes(
                b"prior content"
            )
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", runs_dir
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", side_effect=AssertionError(
                    "dispatch should stop before launching the agent"
                )
            ):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    caller_uuid="deadbeef0123",
                )
            self.assertEqual(exit_code, 2)
            self.assertFalse(log_path.exists())

    def test_empty_caller_uuid_target_is_overwritable(self) -> None:
        """Empty leftovers from a prior aborted run shouldn't block reuse —
        only non-empty existing files are protected."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            runs_dir = tmp_path / "runs"
            runs_dir.mkdir()
            (runs_dir / "deadbeef0123-agent.out").touch()
            (runs_dir / "deadbeef0123-agent.err").touch()
            (runs_dir / "deadbeef0123-agent.timeline").touch()
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", runs_dir
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", return_value=0
            ):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    caller_uuid="deadbeef0123",
                )
        self.assertEqual(exit_code, 0)


class OutputDirOverrideTests(unittest.TestCase):
    """`--output-dir <path>` overrides the default RUNS_DIR for capture files
    only — runs.log keeps its standard location."""

    def test_capture_files_land_in_overridden_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            default_runs = tmp_path / "default-runs"
            override = tmp_path / "my-orchestrator-dir"
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", default_runs
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", return_value=0
            ):
                bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    runs_dir=override,
                    caller_uuid="cafef00d1234",
                )
            record = json.loads(log_path.read_text().strip())
            # Captures land in the override; default RUNS_DIR is never created.
            self.assertEqual(
                record["output_stdout"], str(override / "cafef00d1234-agent.out")
            )
            self.assertTrue(override.is_dir())
            self.assertFalse(default_runs.exists())

    def test_relative_output_dir_is_resolved_before_logging(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "runs.log"
            old_cwd = Path.cwd()
            try:
                os.chdir(tmp_path)
                resolved_io = bridge._resolve_runs_dir_and_uuid(
                    SimpleNamespace(output_dir="captures", uuid="abc123abc123")
                )
                self.assertNotIsInstance(resolved_io, int)
                runs_dir, caller_uuid = resolved_io
                with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                    bridge, "RUNS_DIR", tmp_path / "default-runs"
                ), patch("sys.stderr"), patch.object(
                    bridge, "_streamed_run", return_value=0
                ):
                    bridge._dispatch(
                        action="run",
                        requested="agent",
                        resolved="agent",
                        ctx="",
                        profile={},
                        cmd=["agent"],
                        model=None,
                        effort=None,
                        prompt="hi",
                        skills=[],
                        stdin_data="hi",
                        redact_map={},
                        runs_dir=runs_dir,
                        caller_uuid=caller_uuid,
                    )
            finally:
                os.chdir(old_cwd)
            record = json.loads(log_path.read_text().strip())
            expected = runs_dir / "abc123abc123-agent.out"
            self.assertEqual(record["output_stdout"], str(expected))
            self.assertTrue(Path(record["output_stdout"]).is_absolute())


class CmdListTests(unittest.TestCase):
    def test_missing_description_renders_as_empty_not_none(self) -> None:
        """`profile.get('description', '')` returns None (not '') when the
        key is present with a null value; coerce so `bridge list` never
        prints the literal string 'None'."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "agent.json").write_text(
                json.dumps({"command": ["cat"]})
            )
            captured: list[str] = []
            with patch.object(bridge, "PROFILES_DIR", tmp_path), patch(
                "builtins.print", side_effect=lambda *a, **k: captured.append(" ".join(str(x) for x in a))
            ):
                bridge.cmd_list(SimpleNamespace())
            self.assertTrue(captured)
            self.assertNotIn("None", captured[0])


class AuditLogParentDirTests(unittest.TestCase):
    """`--output-dir <elsewhere>` doesn't go through LOG_BASE, so on a fresh
    install (or post-tmp-purge) the audit-log parent dir won't exist yet.
    The bridge must create it lazily — otherwise the first such run silently
    loses its audit record."""

    def test_audit_log_lands_when_log_parent_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            # LOG_FILE points into a subdir that does NOT exist yet.
            log_dir = tmp_path / "never-created" / "logs"
            log_path = log_dir / "runs.log"
            override = tmp_path / "captures"
            self.assertFalse(log_dir.exists())
            with patch.object(bridge, "LOG_FILE", log_path), patch.object(
                bridge, "RUNS_DIR", tmp_path / "ignored"
            ), patch("sys.stderr"), patch.object(
                bridge, "_streamed_run", return_value=0
            ):
                bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data="hi",
                    redact_map={},
                    runs_dir=override,
                    caller_uuid="cafe12345678",
                )
            self.assertTrue(log_path.is_file())
            record = json.loads(log_path.read_text().strip())
            self.assertEqual(record["uuid"], "cafe12345678")


class CmdRunPromptTests(unittest.TestCase):
    """End-to-end-ish test for empty-prompt handling in cmd_run."""

    def test_empty_piped_stdin_is_rejected(self) -> None:
        ns = SimpleNamespace(
            agent="echo", prompt=None, model=None, effort=None, no_context=True
        )
        # stdin must look piped (isatty → False) AND deliver empty content.
        with patch.object(bridge.sys.stdin, "isatty", return_value=False), patch.object(
            bridge.sys.stdin, "read", return_value=""
        ), patch("sys.stderr"):
            exit_code = bridge.cmd_run(ns)
        self.assertEqual(exit_code, 2)


class MergeStreamsValidationTests(unittest.TestCase):
    """`merge_streams` must be a boolean — typos like `"true"` should fail at load."""

    def _run_load_with_profile(self, payload: dict) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "_zztest.json").write_text(json.dumps(payload))
            with patch.object(bridge, "PROFILES_DIR", tmp_path), patch("sys.stderr"):
                try:
                    bridge.load_profiles()
                except SystemExit as e:
                    return int(e.code or 0)
        return 0

    def test_string_value_rejected(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "merge_streams": "true"}
            ),
            2,
        )

    def test_int_value_rejected(self) -> None:
        # bool is a subclass of int, so isinstance(True, int) is True; we
        # explicitly check for bool to keep the schema strict.
        self.assertEqual(
            self._run_load_with_profile({"command": ["cat"], "merge_streams": 1}),
            2,
        )

    def test_true_loads(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "merge_streams": True}
            ),
            0,
        )

    def test_false_loads(self) -> None:
        self.assertEqual(
            self._run_load_with_profile(
                {"command": ["cat"], "merge_streams": False}
            ),
            0,
        )


class StreamedRunMergeStreamsTests(unittest.TestCase):
    """When merge_streams is on: stderr chunks land in the .out file in arrival
    order, no separate .err file is opened by the bridge, and the timeline
    still records the original FD per chunk so `bridge replay` can route them
    back to the right caller stream."""

    def test_both_streams_land_in_stdout_capture(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path = Path(tmp) / "x.out"
            timeline_path = Path(tmp) / "x.timeline"
            # `printf` to stdout then redirect a second `printf` to stderr —
            # ordered so the merged file contains both in deterministic order.
            bridge._streamed_run(
                cmd=[
                    "sh", "-c",
                    "printf 'on_stdout'; printf 'on_stderr' >&2",
                ],
                stdin_data=None,
                env=os.environ.copy(),
                cwd_path=None,
                stdout_path=stdout_path,
                stderr_path=None,
                timeline_path=timeline_path,
                merge_streams=True,
            )
            merged = stdout_path.read_bytes()
            self.assertIn(b"on_stdout", merged)
            self.assertIn(b"on_stderr", merged)
            # No separate .err file is opened by the bridge in merge mode.
            self.assertFalse((Path(tmp) / "x.err").exists())
            # Timeline still records both labels distinctly so replay can
            # restore the original FD distinction.
            timeline = timeline_path.read_text()
            self.assertIn(" stdout ", timeline)
            self.assertIn(" stderr ", timeline)
            # Byte-allocation invariant: in merge mode the sum of timeline
            # byte counts MUST equal the `.out` file size, because `bridge
            # replay` reads N bytes per timeline entry and routes them by
            # label. Any drift (capture write succeeded but timeline missed
            # it, or vice versa) would mis-source bytes for the next entry.
            timeline_bytes = sum(
                int(parts[2])
                for parts in (line.split() for line in timeline.splitlines())
                if len(parts) == 3
            )
            self.assertEqual(timeline_bytes, len(merged))


class StreamedRunMergeStreamsFailureTests(unittest.TestCase):
    """[regression for F1] In merge mode, if the capture write raises
    OSError (e.g. disk full), the matching timeline entry MUST be skipped.
    Otherwise `bridge replay` would read N bytes from `.out` for that
    entry and consume the NEXT chunk's bytes, mis-sourcing every
    subsequent stream.

    Non-merge mode is allowed to drift — the README documents that
    `replay` silently skips reads past EOF — but merge mode commits to
    the stronger invariant `timeline_bytes == out_size`.
    """

    def test_failed_capture_write_skips_timeline_entry(self) -> None:
        # We can't easily inject an OSError into a real Popen pipe, so
        # exercise the post-condition directly: drive `_streamed_run` with
        # a wrapped `Path.open` that makes every other `.out` write raise.
        # The invariant we assert — `sum(timeline byte counts) == .out
        # size` — fails if the fix is reverted, regardless of the exact
        # interleaving the OS picks for the two tee threads.
        real_path_open = Path.open

        def wrapping_open(self, *args, **kwargs):
            f = real_path_open(self, *args, **kwargs)
            if self.suffix == ".out":
                state = {"calls": 0}
                real_write = f.write

                def flaky_write(data):
                    state["calls"] += 1
                    # Drop every second `.out` write — simulates a flush
                    # storm under disk pressure. The FIX guarantees the
                    # paired timeline entry is also dropped, keeping the
                    # invariant.
                    if state["calls"] % 2 == 0:
                        raise OSError(28, "No space left on device")
                    return real_write(data)

                f.write = flaky_write
            return f

        with tempfile.TemporaryDirectory() as tmp, patch("sys.stdout"), patch(
            "sys.stderr"
        ):
            stdout_path = Path(tmp) / "x.out"
            timeline_path = Path(tmp) / "x.timeline"
            with patch.object(Path, "open", wrapping_open):
                bridge._streamed_run(
                    cmd=[
                        "sh", "-c",
                        # Several alternating writes to give the flaky
                        # writer a chance to fail at least once.
                        "for i in 1 2 3 4 5; do "
                        "printf 'oo'; printf 'ee' >&2; "
                        "done",
                    ],
                    stdin_data=None,
                    env=os.environ.copy(),
                    cwd_path=None,
                    stdout_path=stdout_path,
                    stderr_path=None,
                    timeline_path=timeline_path,
                    merge_streams=True,
                )
            out_size = stdout_path.stat().st_size
            timeline_bytes = sum(
                int(parts[2])
                for parts in (
                    line.split()
                    for line in timeline_path.read_text().splitlines()
                )
                if len(parts) == 3
            )
            # The whole point: drift is forbidden in merge mode.
            self.assertEqual(timeline_bytes, out_size)


class DispatchHonorsMergeStreamsTests(unittest.TestCase):
    """The audit log's output_stderr must be null when merge_streams is on,
    and the merge_streams flag itself must be persisted so consumers of
    runs.log can tell merged from non-merged captures apart."""

    def test_log_record_has_null_stderr_and_merge_flag_true(self) -> None:
        tmp_path = Path(tempfile.mkdtemp())
        log_path = tmp_path / "runs.log"
        runs_dir = tmp_path / "runs"

        def fake_run(
            cmd, stdin_data, env, cwd_path, stdout_path, stderr_path,
            timeline_path, *, merge_streams=False,
        ):
            # Sanity: dispatch must propagate merge_streams down.
            assert merge_streams is True
            assert stderr_path is None
            stdout_path.write_bytes(b"merged content")
            timeline_path.write_bytes(b"1 stdout 14\n")
            return 0

        with patch.object(bridge, "LOG_FILE", log_path), patch.object(
            bridge, "RUNS_DIR", runs_dir
        ), patch("sys.stderr"):
            with patch.object(bridge, "_streamed_run", side_effect=fake_run):
                exit_code = bridge._dispatch(
                    action="run",
                    requested="agent",
                    resolved="agent",
                    ctx="",
                    profile={"merge_streams": True},
                    cmd=["agent"],
                    model=None,
                    effort=None,
                    prompt="hi",
                    skills=[],
                    stdin_data=None,
                    redact_map={},
                )
        self.assertEqual(exit_code, 0)
        record = json.loads(log_path.read_text().strip())
        self.assertIsNone(record["output_stderr"])
        self.assertIsNotNone(record["output_stdout"])
        self.assertIsNotNone(record["output_timeline"])
        self.assertTrue(record["merge_streams"])


if __name__ == "__main__":
    unittest.main()
