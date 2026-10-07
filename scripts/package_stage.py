"""Build a current-source local Windows package. No skip-build or external publish."""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

import verify_stage as verify

ROOT = Path(__file__).resolve().parents[1]


def license_inventory(destination: Path) -> list[dict]:
    proc = subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT,
                          capture_output=True, text=True, encoding="utf-8", errors="replace", check=True)
    rows = []
    for package in json.loads(proc.stdout)["packages"]:
        if package.get("source") is None:
            continue
        root = Path(package["manifest_path"]).parent
        files = {f for f in root.iterdir() if f.is_file() and f.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"))}
        if package.get("license_file"):
            file = root / package["license_file"]
            if file.is_file():
                files.add(file)
        copied = []
        for file in sorted(files):
            target = destination / f"licenses/{package['name']}-{package['version']}" / file.name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(file, target)
            copied.append(target.relative_to(destination).as_posix())
        rows.append({"name": package["name"], "version": package["version"],
                     "license": package.get("license") or "UNSPECIFIED",
                     "repository": package.get("repository") or "", "files": copied})
    return sorted(rows, key=lambda row: (row["name"], row["version"]))


def verified_source(report: dict, fingerprint: dict, config: dict) -> None:
    if report.get("exitCode") != 0 or report.get("sourceFingerprint") != fingerprint:
        raise ValueError("run verify_stage.py successfully on current sources before packaging")
    if (report.get("stage"), report.get("planVersion"), report.get("stageParent")) != (config["stage"], config["planVersion"], config["stageParent"]):
        raise ValueError("verification belongs to another stage")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", default="R1")
    parser.add_argument("--plan-version", default="1.2")
    parser.add_argument("--parent")
    args = parser.parse_args()
    config = verify.load_stage(args.stage, args.plan_version, args.parent)
    verify.validate_base(config, verify.git("branch", "--show-current"), verify.git("show", "-s", "--format=%P", "HEAD"))
    before = verify.source_fingerprint(config)
    reports = ROOT / f".local/reports/{args.stage.lower()}"
    verified_source(json.loads((reports / "verification.json").read_text(encoding="utf-8")), before, config)
    build = subprocess.run(["cargo", "build", "--release", "--locked", "-p", "delta-desktop"], cwd=ROOT,
                           capture_output=True, text=True, encoding="utf-8", errors="replace")
    (reports / "release-build.log").write_text(build.stdout + build.stderr, encoding="utf-8")
    if build.returncode:
        print(build.stderr[-4000:], file=sys.stderr)
        return build.returncode
    verify.require_unchanged(before, verify.source_fingerprint(config))
    exe = ROOT / "target/release/delta-desktop.exe"
    # Never delete an existing package: source hash + binary hash identifies this build.
    destination = ROOT / f"dist/{args.stage.lower()}-{before['sha256'][:12]}-{verify.sha256(exe)[:12]}"
    if destination.exists():
        raise ValueError(f"package already exists; preserved: {destination}")
    (destination / "bin").mkdir(parents=True)
    shutil.copy2(exe, destination / "bin/delta-desktop.exe")
    (destination / "samples").mkdir()
    for name in ("r2-ledger.csv", "r2-ohlcv.json"):
        shutil.copy2(ROOT / "tests/fixtures" / name, destination / "samples" / name)
    (destination / "workers").mkdir()
    shutil.copy2(ROOT / "workers/python/delta_worker.py", destination / "workers/delta_worker.py")
    licenses = license_inventory(destination)
    lines = [f"# {args.stage} third-party inventory", "", "Local evaluation only. Project license: unresolved Q-02. Metadata includes target/dev/transitive crates; it is not a legal clearance.", "", "| crate | version | SPDX metadata | copied notices | repository |", "| --- | --- | --- | --- | --- |"]
    for row in licenses:
        notices = ", ".join(f"[{Path(f).name}]({f})" for f in row["files"]) or "No packaged notice found; review before distribution"
        lines.append(f"| {row['name']} | {row['version']} | {row['license']} | {notices} | {row['repository']} |")
    (destination / "THIRD-PARTY-LICENSES.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (destination / "licenses.json").write_text(json.dumps(licenses, ensure_ascii=False, indent=2), encoding="utf-8")
    for name, flags in (("Start-DELTA.cmd", ""), ("Demo.cmd", "--demo")):
        (destination / name).write_text(f'@echo off\n"%~dp0bin\\delta-desktop.exe" {flags}\n', encoding="ascii")
    (destination / "README.txt").write_text(f"""DELTA {args.stage} / plan {args.plan_version} — local Windows evaluation

Start-DELTA.cmd opens your last library, or Settings on first use. Demo.cmd is
explicit synthetic visual mode. No credentials or personal libraries included.
The desktop ledger/chart/notes/backup path requires no Python or Rust toolchain.
The optional workers/delta_worker.py indicator process needs Python 3.11+;
TA-Lib is optional and is not bundled. Clean-machine/device checks: not-run.

Synthetic round trip:
1. Settings: enter a NEW library.sqlite path, click 创建空白库.
2. 资产: enter an account name, type brokerage, click 创建账户.
   Set scope start 2026-01-01T00:00:00Z, end 2026-02-01T00:00:00Z, apply.
3. 导入: choose samples/r2-ledger.csv; keep the supplied nine headers/comma.
   Preview three valid rows, confirm. Cash is 1438, AAPL quantity is 6.
4. 图表: import samples/r2-ohlcv.json. Load NASDAQ:AAPL, same dates.
   Asset value is USD 2068; period PnL is 68. Prices are synthetic, not live.
5. Save chart context. 笔记: title/body, save, reopen; evidence pins revisions.
6. 备份: enter a new directory, create backup. Export to another empty directory.
   Restore the backup into a NEW directory/library.sqlite; original stays intact.
7. Close and restart: same persistent library reopens. Unsaved notes prevent close
   until saved or explicitly discarded. Corrupt/alien libraries are rejected.

AI is optional: Settings stores connection metadata and a credential reference;
secrets go only to the OS credential store. The user must explicitly grant accounts
on AI and apply scope before sending. Real endpoint/account authorization and costs
remain USER-ACTIONS ACT-01/03. Offline tests use a loopback controlled endpoint.
No network service is required for the synthetic financial round trip above.

VERSION.json records the source hash and pre-amend commit, not a recursive final
commit hash. Third-party notices are copied where present; missing notices and
project license Q-02 require review before any external distribution. This command
does not publish, sign, install, push, or create a PR.
""", encoding="utf-8")
    verify.require_unchanged(before, verify.source_fingerprint(config))
    artifacts = {f.relative_to(destination).as_posix(): verify.sha256(f) for f in sorted(destination.rglob("*")) if f.is_file()}
    version = {"stage": args.stage, "planVersion": args.plan_version, "stageParent": config["stageParent"],
               "headBeforeAmend": verify.git("rev-parse", "HEAD"), "sourceFingerprint": before,
               "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
               "artifacts": artifacts, "thirdPartyDependencies": len(licenses),
               "dependenciesWithoutNotice": [r["name"] + "@" + r["version"] for r in licenses if not r["files"]]}
    (destination / "VERSION.json").write_text(json.dumps(version, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    report = {"dist": destination.relative_to(ROOT).as_posix(), "version": version, "status": "passed"}
    (reports / "package.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"dist": report["dist"], "source": before["sha256"], "exe": artifacts["bin/delta-desktop.exe"],
                      "dependencies": len(licenses), "missingNotices": len(version["dependenciesWithoutNotice"])}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
