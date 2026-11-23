"""Everyday tier of the gate (the person's decision of 2026-11-14): about five minutes.

After a change inside a stage, after A's fixes, rework and in-stage additions, run this instead of
the full `verify-stage` runs; those run once, when B hands over a whole stage
(docs/dev-rules/quality-gates.md#gate-tiers). Steps, quiet and at below-normal priority (D-25):
docs check, `git diff --check`, `cargo fmt --check`, Python script unit tests when `scripts/`
changed, clippy on the crates changed since HEAD (plus workspace reverse dependents), and their
tests (`--workspace` when shared files changed, or with --all). It writes no evidence.
Standard library only. Run from the repository root:  python scripts/quick_gate.py [--all]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import tomllib
from pathlib import Path

from _quiet import git_lines, run_quiet

ROOT = Path(__file__).resolve().parents[1]
BUDGET_SECONDS = 300
STEP_TIMEOUT_SECONDS = 600
# Changes here reach every crate, so they are tested across the workspace.
SHARED = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "clippy.toml", "tests/fixtures/")


def changed_files(root: Path = ROOT) -> list[str]:
    """Return tracked diffs (including deletions) and untracked files relative to `root`."""
    names = git_lines(root, "diff", "--name-only", "HEAD") + git_lines(
        root, "ls-files", "--others", "--exclude-standard"
    )
    return sorted({name.replace("\\", "/") for name in names if name.strip()})


def _dependency_names(table: dict) -> set[str]:
    names: set[str] = set()
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        deps = table.get(section)
        if not isinstance(deps, dict):
            continue
        for alias, spec in deps.items():
            if isinstance(spec, dict) and isinstance(spec.get("package"), str):
                names.add(spec["package"])
            else:
                names.add(alias)
    return names


def workspace_graph(root: Path = ROOT) -> tuple[dict[str, str], dict[str, set[str]]]:
    """Return `(member_dir -> package_name, package_name -> reverse_dependent_packages)`."""
    manifest_path = root / "Cargo.toml"
    if not manifest_path.is_file():
        return {}, {}
    root_manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    members = root_manifest.get("workspace", {}).get("members", [])
    dir_to_pkg: dict[str, str] = {}
    raw_deps: dict[str, set[str]] = {}
    for member in members:
        member_posix = str(member).replace("\\", "/").strip("/")
        cargo_toml = root / member_posix / "Cargo.toml"
        if not cargo_toml.is_file():
            continue
        data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
        pkg_name = data.get("package", {}).get("name")
        if not isinstance(pkg_name, str) or not pkg_name:
            continue
        dir_to_pkg[member_posix] = pkg_name
        deps = _dependency_names(data)
        target_table = data.get("target")
        if isinstance(target_table, dict):
            for cfg_table in target_table.values():
                if isinstance(cfg_table, dict):
                    deps |= _dependency_names(cfg_table)
        raw_deps[pkg_name] = deps

    workspace_pkgs = set(dir_to_pkg.values())
    reverse_deps: dict[str, set[str]] = {pkg: set() for pkg in workspace_pkgs}
    for pkg, deps in raw_deps.items():
        for dep in deps & workspace_pkgs:
            if dep != pkg:
                reverse_deps[dep].add(pkg)
    return dir_to_pkg, reverse_deps


def crate_scope(files: list[str], everything: bool, root: Path = ROOT) -> list[str]:
    """Cargo arguments selecting changed crates and reverse dependents; empty if no Rust changed."""
    if everything or any(name == s or name.startswith(s) for name in files for s in SHARED):
        return ["--workspace"]

    dir_to_pkg, reverse_deps = workspace_graph(root)
    if not dir_to_pkg:
        return ["--workspace"] if any(name.startswith("crates/") for name in files) else []

    direct: set[str] = set()
    for name in files:
        matched = False
        for member_dir, pkg_name in dir_to_pkg.items():
            if name == member_dir or name.startswith(member_dir + "/"):
                direct.add(pkg_name)
                matched = True
                break
        if not matched and name.startswith("crates/") and name.count("/") >= 2:
            # Unknown or deleted crate layout under crates/: run full workspace to be safe.
            return ["--workspace"]

    if not direct:
        return []

    affected: set[str] = set(direct)
    queue = list(direct)
    while queue:
        current = queue.pop()
        for dependent in reverse_deps.get(current, ()):
            if dependent not in affected:
                affected.add(dependent)
                queue.append(dependent)

    if affected == set(dir_to_pkg.values()):
        return ["--workspace"]
    args: list[str] = []
    for crate in sorted(affected):
        args += ["-p", crate]
    return args


def scripts_changed(files: list[str], everything: bool) -> bool:
    """Whether Python script unit tests should run."""
    return everything or any(
        name.startswith("scripts/") and name.endswith((".py", ".json")) for name in files
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--all",
        action="store_true",
        help="lint and test the full workspace and all Python script unit tests",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    everything = args.all
    started = time.monotonic()
    steps: list[dict] = []

    def run(name: str, command: list[str]) -> bool:
        at = time.monotonic()
        print(f"[quick] {name} ...", flush=True)
        try:
            code = run_quiet(
                command,
                cwd=ROOT,
                timeout=STEP_TIMEOUT_SECONDS,
                capture=False,
                check=False,
            ).returncode
        except subprocess.TimeoutExpired:
            seconds = round(time.monotonic() - at)
            steps.append({"name": name, "ok": False, "seconds": seconds, "timedOut": True})
            print(f"[quick] {name}: timed out after {seconds} s", flush=True)
            return False
        seconds = round(time.monotonic() - at)
        steps.append({"name": name, "ok": code == 0, "seconds": seconds})
        print(
            f"[quick] {name}: {'passed' if code == 0 else f'failed (exit {code})'} ({seconds} s)",
            flush=True,
        )
        return code == 0

    def finish(ok: bool) -> int:
        seconds = round(time.monotonic() - started)
        print(
            json.dumps(
                {
                    "status": "passed" if ok else "failed",
                    "check": "quick-gate",
                    "seconds": seconds,
                    "budgetSeconds": BUDGET_SECONDS,
                    "overBudget": seconds > BUDGET_SECONDS,
                    "steps": steps,
                }
            )
        )
        if seconds > BUDGET_SECONDS:
            print(f"[quick] took {seconds} s, over the {BUDGET_SECONDS} s budget")
        return 0 if ok else 1

    try:
        files = changed_files(ROOT)
    except (RuntimeError, subprocess.TimeoutExpired) as exc:
        print(f"[quick] failed to detect changed files: {exc}", file=sys.stderr)
        return finish(False)

    scope = crate_scope(files, everything, ROOT)
    run_py_tests = scripts_changed(files, everything)

    for name, command in (
        ("docs", [sys.executable, "scripts/check_docs.py"]),
        ("git diff --check", ["git", "diff", "--check", "HEAD"]),
        ("cargo fmt --check", ["cargo", "fmt", "--all", "--check"]),
    ):
        if not run(name, command):
            return finish(False)

    if run_py_tests:
        if not run(
            "scripts unittest",
            [sys.executable, "-m", "unittest", "discover", "-s", "scripts", "-p", "test_*.py"],
        ):
            return finish(False)

    if not scope:
        print("[quick] no Rust changes since HEAD: clippy and tests skipped", flush=True)
        return finish(True)
    shown = " ".join(scope)
    if not run(
        f"clippy {shown}",
        ["cargo", "clippy", *scope, "--all-targets", "--locked", "--", "-D", "warnings"],
    ):
        return finish(False)
    return finish(run(f"tests {shown}", ["cargo", "test", *scope, "--locked"]))


if __name__ == "__main__":
    sys.exit(main())
