"""Shared D-25 quiet subprocess helpers for DELTA repository scripts.

Standard library only (Python 3.12+). Enforces no console window popup, below-normal
priority on Windows, null stdin, non-interactive git/color env vars, UTF-8 text decoding,
and explicit timeouts.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Mapping, Sequence

CREATE_NO_WINDOW = 0x08000000
BELOW_NORMAL_PRIORITY_CLASS = 0x00004000
QUIET_CREATION_FLAGS = (
    (CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS) if sys.platform == "win32" else 0
)

QUIET_ENV_DEFAULTS: dict[str, str] = {
    "GIT_TERMINAL_PROMPT": "0",
    "GCM_INTERACTIVE": "never",
    "NO_COLOR": "1",
    "CI": "1",
    "PYTHONUTF8": "1",
}


def quiet_env(extra: Mapping[str, str] | None = None) -> dict[str, str]:
    env = os.environ.copy()
    env.update(QUIET_ENV_DEFAULTS)
    if extra:
        env.update(extra)
    return env


def quiet_creation_flags(capture: bool) -> int:
    """Return Windows creationflags for D-25 quiet execution.

    When streaming directly to an interactive console (`capture=False` and stdout/stderr is a TTY),
    `CREATE_NO_WINDOW` detaches the child from the parent console and suppresses live output, so
    only `BELOW_NORMAL_PRIORITY_CLASS` is used. In captured or non-TTY invocations (IDE/agent/CI),
    `CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS` prevents console window popups.
    """
    if sys.platform != "win32":
        return 0
    if not capture and (sys.stdout.isatty() or sys.stderr.isatty()):
        return BELOW_NORMAL_PRIORITY_CLASS
    return CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS


def run_quiet(
    command: Sequence[str],
    *,
    cwd: Path,
    timeout: float | None = 300,
    capture: bool = False,
    check: bool = False,
    extra_env: Mapping[str, str] | None = None,
) -> subprocess.CompletedProcess:
    kwargs: dict = {
        "cwd": cwd,
        "stdin": subprocess.DEVNULL,
        "env": quiet_env(extra_env),
        "timeout": timeout,
        "check": check,
    }
    if sys.platform == "win32":
        kwargs["creationflags"] = quiet_creation_flags(capture)
    if capture:
        kwargs["capture_output"] = True
        kwargs["text"] = True
        kwargs["encoding"] = "utf-8"
        kwargs["errors"] = "replace"
    return subprocess.run(list(command), **kwargs)


def git_lines(cwd: Path, *args: str, timeout: float = 30) -> list[str]:
    """Run git quietly with unquoted UTF-8 paths and return non-empty output lines."""
    result = run_quiet(
        ["git", "-c", "core.quotepath=false", *args],
        cwd=cwd,
        timeout=timeout,
        capture=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()[:400]
        raise RuntimeError(f"git {' '.join(args)} failed (exit {result.returncode}): {detail}")
    return [line for line in result.stdout.splitlines() if line]
