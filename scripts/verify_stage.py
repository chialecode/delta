"""Current-stage gates and behavior evidence; manual/live results never inferred.

The original R1 case definitions remain part of the consolidated R1 scope. Their executable assertions are
checked against the full workspace run; only their stage-specific Git/report
selfcheck is replaced by the parameterized current-stage check.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

import verify_r1 as legacy

ROOT = Path(__file__).resolve().parents[1]


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True,
                          text=True, check=True).stdout.strip()


def load_stage(stage: str, version: str, parent: str | None = None) -> dict:
    if not re.fullmatch(r"M(?:0|[1-9][0-9]*)", stage):
        raise ValueError("invalid stage")
    config = json.loads((ROOT / f"docs/delivery/{stage.lower()}-stage-cases.json").read_text(encoding="utf-8"))
    if config["stage"] != stage or config["planVersion"] != version:
        raise ValueError("stage/plan version mismatch")
    if parent is not None and config["stageParent"] != parent:
        raise ValueError("requested parent differs from the approved stage")
    return config


def validate_base(config: dict, branch: str, parents: str, history: str | None = None) -> None:
    if branch != config["branch"]:
        raise ValueError("stage branch mismatch")
    if history is None:
        if parents.split() != [config["stageParent"]]:
            raise ValueError("stage parent mismatch (or merge commit)")
        return
    # Published review fixes append commits. Validate the entire linear chain
    # back to the approved base instead of weakening the base requirement.
    rows = [line.split() for line in history.splitlines() if line.strip()]
    if not rows or any(len(row) != 2 for row in rows):
        raise ValueError("stage history must be nonempty and linear")
    if rows[0][1:] != parents.split():
        raise ValueError("stage history does not match HEAD parents")
    if any(left[1] != right[0] for left, right in zip(rows, rows[1:])):
        raise ValueError("stage history is disconnected")
    if rows[-1][1] != config["stageParent"]:
        raise ValueError("stage history does not reach approved base")


def validate_checkout(config: dict) -> None:
    validate_base(config, git("branch", "--show-current"),
                  git("show", "-s", "--format=%P", "HEAD"),
                  git("rev-list", "--parents", "HEAD", "^" + config["stageParent"]))


def source_fingerprint(config: dict) -> dict:
    # Match .gitattributes text/eol=lf, so Windows and a clean checkout agree.
    roots = ["Cargo.toml", "Cargo.lock", "crates", "apps", "workers/python", "scripts", "tests",
             f"docs/delivery/{config['stage'].lower()}-stage-cases.json"]
    entries = []
    for rel in roots:
        path = ROOT / rel
        for file in ([path] if path.is_file() else path.rglob("*")):
            if not file.is_file() or "__pycache__" in file.parts or file.suffix == ".pyc":
                continue
            raw = file.read_bytes()
            if b"\0" not in raw:
                raw = raw.replace(b"\r\n", b"\n")
            entries.append((file.relative_to(ROOT).as_posix(), hashlib.sha256(raw).hexdigest()))
    entries.sort()
    digest = hashlib.sha256("".join(f"{name}:{sha}\n" for name, sha in entries).encode()).hexdigest()
    return {"files": len(entries), "sha256": digest,
            "algorithm": "stage-source-v2: sorted path:sha256, Git LF text, code/scripts/fixtures/current case mapping"}


def require_unchanged(before: dict, after: dict) -> None:
    if before != after:
        raise ValueError("source changed during verification/build; artifact is stale")


def gate_result(name: str, command: list[str], code: int, output: str, seconds: float) -> dict:
    count = legacy.tests_executed(command, output)
    return {"name": name, "command": command, "exit_code": code if code else (1 if count == 0 else 0),
            "tests_executed": count, "seconds": round(seconds, 3)}


def passed_names(output: str) -> list[str]:
    return re.findall(r"^test (\S+) \.\.\. ok$", output, re.MULTILINE)


def cases_for(config: dict, names: list[str], gates: list[dict]) -> list[dict]:
    cases = []
    for definition in config["cases"]:
        case = dict(definition)
        if case["mode"] != "automated":
            case.update(status="not-run", actual="条件与步骤只在 USER-ACTIONS.md 维护")
        else:
            missing = [p for p in case["testPatterns"] if not any(p in n for n in names)]
            failures = [g["name"] for g in gates if g["exit_code"]]
            case.update(status="failed" if missing or failures else "passed",
                        matchedTests=[n for n in names if any(p in n for p in case["testPatterns"])],
                        actual={"missingPatterns": missing, "failedGates": failures})
        cases.append(case)
    return cases


def inherited_assertions(names: list[str], output: str = "") -> list[dict]:
    """Preserve each R1 Cargo test command's coverage without rerunning suites."""
    results = []
    for case, commands in legacy.CASE_COMMANDS.items():
        for cmd in commands:
            if cmd[:2] != ["cargo", "test"]:
                continue
            # A positional filter follows -p package / --test suite / --lib.
            positional = []
            index = 2
            while index < len(cmd):
                if cmd[index] in ("-p", "--package", "--test"):
                    index += 2
                elif cmd[index].startswith("-"):
                    index += 1
                else:
                    positional.append(cmd[index]); index += 1
            pattern = positional[-1] if positional else None
            matches = [n for n in names if pattern is None or pattern in n]
            suite = cmd[cmd.index("--test") + 1] if "--test" in cmd else None
            suite_seen = suite is None or bool(re.search(r"Running tests[\\/]" + re.escape(suite) + r"\.rs", output))
            results.append({"case": case, "originalCommand": cmd, "filter": pattern,
                            "matched": len(matches), "suite": suite, "suiteSeen": suite_seen,
                            "status": "passed" if matches and suite_seen else "failed"})
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", default="M0")
    parser.add_argument("--plan-version", default="1.0")
    parser.add_argument("--parent")
    args = parser.parse_args()
    config = load_stage(args.stage, args.plan_version, args.parent)
    validate_checkout(config)
    before = source_fingerprint(config)
    report_dir = ROOT / f".local/reports/{args.stage.lower()}"
    report_dir.mkdir(parents=True, exist_ok=True)
    commands = [
        ("docs", [sys.executable, "scripts/check_docs.py"]),
        ("diff", ["git", "diff", "--check"]),
        ("format", ["cargo", "fmt", "--all", "--", "--check"]),
        ("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]),
        ("rust", ["cargo", "test", "--workspace", "--locked"]),
        ("python", [sys.executable, "-m", "unittest", "discover", "-s", "tests/python"]),
        ("worker", [sys.executable, "-m", "unittest", "discover", "-s", "workers/python/tests"]),
        ("build", ["cargo", "build", "--workspace", "--locked"]),
    ]
    gates, names = [], []
    rust_output = ""
    for name, command in commands:
        code, output, seconds = legacy.run_cmd(command)
        (report_dir / f"{name}.log").write_text(output, encoding="utf-8")
        result = gate_result(name, command, code, output, seconds)
        gates.append(result)
        if name == "rust":
            names = passed_names(output)
            rust_output = output
        print(f"{name}: {'passed' if not result['exit_code'] else 'failed'} ({seconds:.1f}s)", flush=True)
    require_unchanged(before, source_fingerprint(config))
    cases = cases_for(config, names, gates)
    inherited = inherited_assertions(names, rust_output)
    code, blocking = legacy.exit_code_for(cases, gates)
    if any(c["status"] != "passed" for c in inherited):
        code = 1; blocking.append("inherited-r1-assertions")
    exe = ROOT / "target/debug/delta-desktop.exe"
    report = {"stage": args.stage, "planVersion": args.plan_version,
              "headBeforeAmend": git("rev-parse", "HEAD"), "stageParent": config["stageParent"],
              "sourceFingerprint": before, "gates": gates, "cases": cases,
              "inheritedAssertions": inherited, "inheritedManualGaps": legacy.CASE_PARTIAL,
              "debugArtifact": {"sha256": sha256(exe)} if exe.exists() and not gates[-1]["exit_code"] else None,
              "blocking": blocking, "exitCode": code}
    (report_dir / "verification.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"{args.stage} verification exit={code}; evidence={report_dir.relative_to(ROOT)}/verification.json")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
