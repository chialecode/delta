"""Everyday tier of the gate (the person's decision of 2026-11-14): about five minutes.

After a change inside a stage, after A's fixes, rework and in-stage additions, run this instead of
the full `verify-stage` runs; those run once, when B hands over a whole stage
(docs/dev-rules/quality-gates.md#gate-tiers). Steps, quiet and at below-normal priority (D-25):
docs check, `git diff --check`, `cargo fmt --check`, clippy on the crates changed since HEAD, and
their tests (`--workspace` when shared files changed, or with --all). It writes no evidence.
Standard library only. Run from the repository root:  python scripts/quick_gate.py [--all]
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGET_SECONDS = 300
BELOW_NORMAL = 0x00004000 if sys.platform == "win32" else 0
# Changes here reach every crate, so they are tested across the workspace.
SHARED = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "clippy.toml", "tests/fixtures/")


def changed_files() -> list[str]:
    def git(*args: str) -> list[str]:
        out = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=False).stdout
        return [line.strip() for line in out.splitlines() if line.strip()]

    names = git("diff", "--name-only", "HEAD") + git("ls-files", "--others", "--exclude-standard")
    return sorted({name for name in names if (ROOT / name).exists()})


def crate_scope(files: list[str], everything: bool) -> list[str]:
    """Cargo arguments selecting the crates to lint and test; empty when nothing Rust changed."""
    if everything or any(name == s or name.startswith(s) for name in files for s in SHARED):
        return ["--workspace"]
    crates = sorted({name.split("/")[1] for name in files if name.startswith("crates/") and name.count("/") >= 2})
    args: list[str] = []
    for crate in crates:
        args += ["-p", crate]
    return args


def main() -> int:
    everything = "--all" in sys.argv[1:]
    started = time.monotonic()
    steps: list[dict] = []

    def run(name: str, command: list[str]) -> bool:
        at = time.monotonic()
        print(f"[quick] {name} ...", flush=True)
        code = subprocess.run(command, cwd=ROOT, creationflags=BELOW_NORMAL, check=False).returncode
        seconds = round(time.monotonic() - at)
        steps.append({"name": name, "ok": code == 0, "seconds": seconds})
        print(f"[quick] {name}: {'passed' if code == 0 else f'failed (exit {code})'} ({seconds} s)", flush=True)
        return code == 0

    def finish(ok: bool) -> int:
        seconds = round(time.monotonic() - started)
        print(json.dumps({"status": "passed" if ok else "failed", "check": "quick-gate", "seconds": seconds,
                          "budgetSeconds": BUDGET_SECONDS, "overBudget": seconds > BUDGET_SECONDS, "steps": steps}))
        if seconds > BUDGET_SECONDS:
            print(f"[quick] took {seconds} s, over the {BUDGET_SECONDS} s budget")
        return 0 if ok else 1

    files = changed_files()
    scope = crate_scope(files, everything)
    for name, command in (
        ("docs", [sys.executable, "scripts/check_docs.py"]),
        ("git diff --check", ["git", "diff", "--check", "HEAD"]),
        ("cargo fmt --check", ["cargo", "fmt", "--all", "--check"]),
    ):
        if not run(name, command):
            return finish(False)
    if not scope:
        print("[quick] no Rust changes since HEAD: clippy and tests skipped", flush=True)
        return finish(True)
    shown = " ".join(scope)
    if not run(f"clippy {shown}", ["cargo", "clippy", *scope, "--all-targets", "--locked", "--", "-D", "warnings"]):
        return finish(False)
    return finish(run(f"tests {shown}", ["cargo", "test", *scope, "--locked"]))


if __name__ == "__main__":
    sys.exit(main())
