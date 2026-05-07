#!/usr/bin/env python3
"""Detect the active subscription context (personal/work) from orchestrator env vars.

Reads the env set by claude-personal / claude-work / opencode-personal / opencode-work
shell wrappers. Prints `personal`, `work`, or empty string. Always exits 0.

Use:
    ctx=$(python3 scripts/context.py)
    python3 scripts/bridge.py run "claude${ctx:+-$ctx}" -p "..."
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Optional


def _resolve(p: str) -> Optional[Path]:
    if not p:
        return None
    try:
        return Path(p).resolve()
    except OSError:
        return None


def _match_dir(env_key: str, sentinel_prefix: str) -> str:
    """Match `$<env_key>` against `$<sentinel_prefix>_PERSONAL_DIR` / `_WORK_DIR`."""
    target = _resolve(os.environ.get(env_key, ""))
    if target is None:
        return ""
    for ctx in ("personal", "work"):
        ref = _resolve(os.environ.get(f"{sentinel_prefix}_{ctx.upper()}_DIR", ""))
        if ref is not None and ref == target:
            return ctx
    # Fallback only considers the config directory name itself. Parent
    # directories such as `/Users/work/.claude` are unrelated to the profile and
    # must not trigger auto-routing.
    name = target.name.lower()
    has_personal = name == "personal" or name.endswith("-personal")
    has_work = name == "work" or name.endswith("-work")
    if has_personal and not has_work:
        return "personal"
    if has_work and not has_personal:
        return "work"
    # Either neither matched, or both did (e.g. ".../work/.foo-personal"); ambiguous → no auto-route.
    return ""


def detect() -> str:
    # 1. opencode wrappers (one-shot or `use-opencode-*`) set OPENCODE_PROFILE explicitly
    profile = os.environ.get("OPENCODE_PROFILE", "").strip()
    if profile in ("personal", "work"):
        return profile

    # 2. claude-personal / claude-work / cursor-personal / cursor-work wrappers
    #    set CLAUDE_CONFIG_DIR; compare against CLAUDE_PERSONAL_DIR / CLAUDE_WORK_DIR.
    ctx = _match_dir("CLAUDE_CONFIG_DIR", "CLAUDE")
    if ctx:
        return ctx

    # 3. opencode wrappers also set XDG_DATA_HOME; match share-personal /
    # share-work as a *whole path component* — substring matching would
    # falsely route paths like /Users/foo/myshare-personal/...
    xdg = _resolve(os.environ.get("XDG_DATA_HOME", ""))
    if xdg is not None:
        parts = [p.lower() for p in str(xdg).split(os.sep) if p]
        has_personal = any(p == "share-personal" for p in parts)
        has_work = any(p == "share-work" for p in parts)
        if has_personal and not has_work:
            return "personal"
        if has_work and not has_personal:
            return "work"

    return ""


if __name__ == "__main__":
    print(detect())
