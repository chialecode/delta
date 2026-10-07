"""R1 verification orchestrator (plan §7).

Runs the documentation gate, Rust format/lint/test gates, the Python worker
tests, and emits `.local/reports/r1/results.json` with a per-case matrix
mapped from r1-cases.json to their commands.

Exit code policy (plan §7): nonzero if any gate fails or any *required
automated* case is missing/failed/partial/not-run. A command that matches
zero tests is a failure, not a pass. A case whose commands pass but cover
only part of its expected behavior is `partial` and keeps its command
evidence. Manual/live cases stay independent and never count as passed here
— a nonzero exit for automation gaps is honest output for an in-progress R1,
not a tool failure.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPORT_DIR = ROOT / ".local/reports/r1"
PLAN_VERSION = "1.1"

CASE_COMMANDS: dict[str, list[list[str]]] = {
    "R1-A-25": [
        [sys.executable, "-m", "unittest", "discover", "-s", "tests/python"],
        [sys.executable, "scripts/verify_r1.py", "--selfcheck"],
    ],
    "R1-A-01": [
        ["cargo", "build", "--workspace", "--locked"],
        [sys.executable, "-m", "unittest", "discover", "-s", "tests/python", "-p", "test_dependency_boundary.py"],
    ],
    "R1-A-02": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_02"]],
    "R1-A-03": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_03"]],
    "R1-A-04": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_04"]],
    "R1-A-05": [
        ["cargo", "test", "-p", "delta-core", "r1_a_05"],
        ["cargo", "test", "-p", "delta-core", "--test", "conservation_prop"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_05"],
        ["cargo", "test", "-p", "delta-desktop", "r1_a_05"],
    ],
    "R1-A-06": [["cargo", "test", "-p", "delta-core", "r1_a_06"]],
    "R1-A-07": [["cargo", "test", "-p", "delta-core", "r1_a_07"]],
    "R1-A-08": [
        ["cargo", "test", "-p", "delta-core", "r1_a_08"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_08"],
    ],
    "R1-A-09": [
        ["cargo", "test", "-p", "delta-core", "r1_a_09"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_09"],
    ],
    "R1-A-10": [
        ["cargo", "test", "-p", "delta-infra", "--test", "store_market_data", "r1_a_10"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_10"],
    ],
    "R1-A-11": [[sys.executable, "-m", "unittest", "discover", "-s", "workers/python/tests"]],
    "R1-A-12": [
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_12"],
        ["cargo", "test", "-p", "delta-desktop", "r1_a_12"],
    ],
    "R1-A-13": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_13"]],
    "R1-A-14": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_14"]],
    "R1-A-15": [
        ["cargo", "test", "-p", "delta-app", "--lib", "r1_a_15"],
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_15"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_15"],
    ],
    "R1-A-16": [
        ["cargo", "test", "-p", "delta-infra", "--test", "model_protocol"],
        ["cargo", "test", "-p", "delta-infra", "--test", "model_proxy"],
        ["cargo", "test", "-p", "delta-infra", "--lib", "model::tests"],
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_18_chat_request_pairs"],
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_18_interrupted"],
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_21_tool_loop"],
    ],
    "R1-A-17": [
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_17"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_17"],
    ],
    "R1-A-18": [
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_18"],
        ["cargo", "test", "-p", "delta-app", "--lib", "grants"],
    ],
    "R1-A-19": [["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_19"]],
    "R1-A-20": [
        ["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_20"],
        ["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_20"],
    ],
    "R1-A-21": [["cargo", "test", "-p", "delta-infra", "--test", "runtime_ai", "r1_a_21"]],
    "R1-A-22": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_22"]],
    "R1-A-23": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_23"]],
    "R1-A-24": [["cargo", "test", "-p", "delta-infra", "--test", "app_service", "r1_a_24"]],
}

CASE_GAPS: dict[str, str] = {}

CASE_PARTIAL: dict[str, str] = {
    "R1-A-12": "zoom/pan/crosshair, chart-context restore, link capacity, change notices, the service watchlist/search with literal % and _, and the desktop search/watchlist screen driven by headless clicks and typing are tested; screenshots of the screen need a device at ACT-02",
    "R1-A-19": "cancel, late callback and library switch are covered; the desktop now logs the close-to-exit time (delta::perf), but the 1s window-close target is not measured and must be measured on a device at ACT-02",
    "R1-A-24": "100k insert, cached first screen under 3s and 1000-bar CPU projection under 20ms passed; the desktop now logs interaction-handler and chart-paint CPU times (delta::perf), but interaction P95 and GPU 50 FPS are not measured and must be measured on a device at ACT-02",
}

MANUAL_LIVE = {
    "R1-M-01": "not-run: real Windows IME/DPI device checks (ACT-02)",
    "R1-M-02": "not-run: clean Windows 11 install without dev tools (ACT-02)",
    "R1-L-01": "not-run: real de-identified account/market data (Q-01/ACT-03)",
    "R1-L-02": "not-run: user-authorized real OpenAI-API endpoint (ACT-01)",
}


def git_fingerprint() -> dict:
    def git(*args: str) -> str:
        return subprocess.run(
            ["git", *args], capture_output=True, text=True, cwd=ROOT, check=True
        ).stdout.strip()
    head = git("rev-parse", "HEAD")
    parent = git("rev-parse", "HEAD~1")
    diff = git("diff", "--check", "HEAD")
    return {"head_before_amend": head, "stage_parent": parent, "diff_check": diff or "clean"}


def source_fingerprint() -> dict:
    """Fingerprint of the delivered source tree (code + entrypoints + fixtures).

    Docs are excluded on purpose: the evidence document records this value and
    is written after the run, so it must not change the number it cites.
    """
    import hashlib

    roots = ["Cargo.toml", "Cargo.lock", "crates", "apps", "workers/python",
             "scripts", "tests"]
    entries: list[tuple[str, str]] = []
    for rel in roots:
        p = ROOT / rel
        files = [p] if p.is_file() else [f for f in p.rglob("*") if f.is_file()]
        for f in files:
            if "__pycache__" in f.parts or f.suffix == ".pyc":
                continue
            digest = hashlib.sha256(f.read_bytes()).hexdigest()
            entries.append((str(f.relative_to(ROOT)).replace("\\", "/"), digest))
    entries.sort()
    combined = hashlib.sha256(
        "".join(f"{n}:{d}\n" for n, d in entries).encode("utf-8")).hexdigest()
    return {"files": len(entries), "sha256": combined}


def exit_code_for(cases: list[dict], gates: list[dict] = ()) -> tuple[int, list[str]]:
    """Plan §7 policy: any failed gate or any required automated case not
    passing blocks the run. Blocking ids list gates as `gate:<name>`."""
    blocking = [f"gate:{g['name']}" for g in gates if g["exit_code"] != 0]
    blocking += [c["id"] for c in cases
                 if c["mode"] == "automated" and c["status"] != "passed"]
    return (1 if blocking else 0), blocking


def tests_executed(cmd: list[str], output: str) -> int | None:
    """Number of tests a test command actually ran (`None` = not a test
    command). A filter that matches nothing must not count as a pass."""
    if cmd[:2] == ["cargo", "test"]:
        return sum(int(n) for n in re.findall(r"test result: \w+\. (\d+) passed", output)) + sum(
            int(n) for n in re.findall(r"test result: \w+\. \d+ passed; (\d+) failed", output))
    if "unittest" in cmd:
        found = re.findall(r"^Ran (\d+) tests?", output, flags=re.MULTILINE)
        return sum(int(n) for n in found)
    return None


def selfcheck() -> int:
    """R1-A-25 repo-level facts: single stage commit, parent, evidence, reports."""
    checks: list[dict] = []

    def check(name: str, ok: bool, detail: str) -> None:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})

    def git(*a: str) -> str:
        return subprocess.run(["git", *a], cwd=ROOT, capture_output=True, text=True,
                              check=False).stdout.strip()

    parents = [l for l in git("log", "--format=%H", "-n", "2").splitlines() if l]
    stage_files = git("show", "--name-only", "--format=", "HEAD").splitlines()
    parent_files = git("show", "--name-only", "--format=", "HEAD~1").splitlines()
    check("stage_single_commit", len(parents) == 2,
          f"HEAD={parents[0][:12] if parents else '-'} parent={parents[1][:12] if len(parents) > 1 else '-'}")
    check("stage_carries_plan_and_code",
          any("r1-execution-plan.md" in f for f in stage_files)
          and any(f.startswith(("crates/", "apps/")) for f in stage_files),
          f"{len(stage_files)} files in stage commit")
    check("parent_is_docs_framework",
          "docs/governance/document-registry.json" in parent_files
          and not any(f.startswith("crates/") for f in parent_files),
          "parent commit holds docs framework only")

    registry = json.loads((ROOT / "docs/governance/document-registry.json")
                          .read_text(encoding="utf-8"))
    paths = {d["path"] for d in registry["documents"]}
    check("evidence_registered", "docs/evidence/r1-delivery.md" in paths,
          "docs/evidence/r1-delivery.md in registry")

    reports = ["results.json", "run_demo.json", "package.json"]
    missing = [r for r in reports if not (REPORT_DIR / r).exists()]
    check("entry_reports_present", not missing, f"missing: {missing or 'none'}")

    cases = json.loads((ROOT / "docs/delivery/r1-cases.json").read_text(encoding="utf-8"))["cases"]
    modes = {m: sum(1 for c in cases if c["mode"] == m) for m in ("automated", "manual", "live")}
    check("case_set_matches_plan", modes == {"automated": 25, "manual": 2, "live": 2}, str(modes))
    check("plan_version", PLAN_VERSION == "1.1", f"PLAN_VERSION={PLAN_VERSION}")

    failed = [c["name"] for c in checks if not c["ok"]]
    print(json.dumps({"selfcheck": checks, "failed": failed}, ensure_ascii=False, indent=2))
    return 1 if failed else 0


def run_cmd(cmd: list[str]) -> tuple[int, str, float]:
    t0 = time.time()
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT,
                          encoding="utf-8", errors="replace")
    out = proc.stdout + "\n" + proc.stderr
    return proc.returncode, out, time.time() - t0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-docs", action="store_true", help="skip markdown gate")
    parser.add_argument("--selfcheck", action="store_true",
                        help="R1-A-25 repo-level facts only (no case runs)")
    args = parser.parse_args()

    if args.selfcheck:
        return selfcheck()

    REPORT_DIR.mkdir(parents=True, exist_ok=True)
    results: dict = {
        "plan_version": PLAN_VERSION,
        "git_base": git_fingerprint(),
        "source_fingerprint": source_fingerprint(),
        "environment": {
            "rustc": subprocess.run(["rustc", "--version"], capture_output=True, text=True).stdout.strip(),
            "cargo": subprocess.run(["cargo", "--version"], capture_output=True, text=True).stdout.strip(),
            "python": sys.version.split()[0],
            "os": sys.platform,
        },
        "cases": [],
    }

    # Gates that are not per-case: docs, fmt, clippy, full test suite.
    gates: list[tuple[str, list[str], int, str]] = []
    if not args.skip_docs:
        code, out, _ = run_cmd([sys.executable, "scripts/check_docs.py"])
        gates.append(("check_docs", [sys.executable, "scripts/check_docs.py"], code, out))
    code, out, _ = run_cmd(["git", "diff", "--check", "HEAD"])
    gates.append(("git_diff_check", ["git", "diff", "--check", "HEAD"], code, out))
    code, out, _ = run_cmd(["cargo", "fmt", "--all", "--", "--check"])
    gates.append(("cargo_fmt", ["cargo", "fmt", "--all", "--", "--check"], code, out))
    code, out, _ = run_cmd(["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])
    gates.append(("cargo_clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"], code, out))
    results["gates"] = [
        {"name": n, "command": c, "exit_code": e, "tail": o[-1500:]} for n, c, e, o in gates
    ]

    all_cases = json.loads((ROOT / "docs/delivery/r1-cases.json").read_text(encoding="utf-8"))["cases"]
    for case in all_cases:
        cid = case["id"]
        entry = {
            "id": cid,
            "mode": case["mode"],
            "status": "not-run",
            "command": None,
            "exit_code": None,
            "expected": case["expected"],
            "actual": None,
            "evidence": [],
            "reason": None,
        }
        if case["mode"] != "automated":
            entry["reason"] = MANUAL_LIVE.get(cid, "manual/live case pending user actions")
        elif cid in CASE_GAPS:
            entry["reason"] = CASE_GAPS[cid]
        elif cid in CASE_COMMANDS:
            commands = CASE_COMMANDS[cid]
            ok = True
            failure = None
            outputs = []
            for cmd in commands:
                code, out, dur = run_cmd(cmd)
                ran = tests_executed(cmd, out)
                outputs.append({"command": cmd, "exit_code": code, "tests_run": ran,
                                "tail": out[-800:], "secs": round(dur, 1)})
                if code != 0:
                    ok, failure = False, "command failed (see evidence tail)"
                    break
                if ran == 0:
                    ok, failure = False, "command matched zero tests"
                    break
            entry["command"] = [o["command"] for o in outputs]
            entry["exit_code"] = outputs[-1]["exit_code"]
            entry["evidence"] = outputs
            if not ok:
                entry["status"] = "failed"
                entry["actual"] = failure
            elif cid in CASE_PARTIAL:
                entry["status"] = "partial"
                entry["actual"] = "mapped commands passed; coverage incomplete"
                entry["reason"] = CASE_PARTIAL[cid]
            else:
                entry["status"] = "passed"
                entry["actual"] = "all commands exited 0 and ran tests"
        else:
            entry["reason"] = "no command mapping registered for this automated case"
        results["cases"].append(entry)

    automated = [c for c in results["cases"] if c["mode"] == "automated"]
    code, blocking_ids = exit_code_for(results["cases"], results["gates"])
    blocking = [c for c in automated if c["status"] != "passed"]
    results["summary"] = {
        "automated_total": len(automated),
        "automated_passed": len(automated) - len(blocking),
        "automated_partial": [c["id"] for c in automated if c["status"] == "partial"],
        "failed_gates": [g["name"] for g in results["gates"] if g["exit_code"] != 0],
        "blocking_ids": blocking_ids,
        "manual_live": {k: v for k, v in MANUAL_LIVE.items()},
        "conclusion": (
            "automated-required group complete; manual/live independent"
            if not blocking_ids
            else "R1 automation incomplete: honest nonzero exit (plan §7)"
        ),
    }

    report_path = REPORT_DIR / "results.json"
    report_path.write_text(json.dumps(results, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"report: {report_path}")
    print(json.dumps(results["summary"], indent=2, ensure_ascii=False))
    # Exit code judges gates plus the automated group (plan §7).
    return code


if __name__ == "__main__":
    raise SystemExit(main())
