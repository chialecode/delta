"""R1 打包入口：生成 Windows 分发目录与依赖/许可证清单。

用法：
    python scripts/package_r1.py [--skip-build]

产物（dist/r1/）：
    bin/delta-desktop.exe         release 构建产物（含哈希）
    workers/delta_worker.py       Python 指标 worker（stdio JSONL 协议 v1）
    VERSION.json                  版本、阶段父提交、源码指纹、工具链
    THIRD-PARTY-LICENSES.md       依赖许可证清单（cargo metadata 采集）
    README.txt                    运行方式与已知限制

同时把机器可读结果写入 `.local/reports/r1/package.json`。
注：Q-02（本项目自身许可证）尚未定案，本清单只采集第三方依赖事实，不构成授权结论。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist/r1"
REPORT_DIR = ROOT / ".local/reports/r1"


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True,
                          check=False).stdout.strip()


def rustc_version() -> str:
    return subprocess.run(["rustc", "--version"], capture_output=True, text=True,
                          check=False).stdout.strip()


def cargo_licenses() -> tuple[list[dict], str]:
    proc = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"],
                          cwd=ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        return [], (proc.stderr or "cargo metadata failed")[-800:]
    meta = json.loads(proc.stdout)
    rows = []
    for pkg in meta.get("packages", []):
        if pkg.get("source") is None:
            continue  # workspace members themselves
        rows.append({
            "name": pkg["name"],
            "version": pkg["version"],
            "license": pkg.get("license") or "UNSPECIFIED",
            "repository": pkg.get("repository") or "",
        })
    rows.sort(key=lambda r: r["name"].lower())
    return rows, ""


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--skip-build", action="store_true", help="复用已有 release 产物")
    args = ap.parse_args()

    exe = ROOT / "target/release/delta-desktop.exe"
    if not args.skip_build or not exe.exists():
        proc = subprocess.run(["cargo", "build", "--release", "--locked", "-p", "delta-desktop"],
                              cwd=ROOT, capture_output=True, text=True)
        if proc.returncode != 0:
            print((proc.stdout + proc.stderr)[-2000:], file=sys.stderr)
            return 1
    if not exe.exists():
        print(f"missing release artifact: {exe}", file=sys.stderr)
        return 1

    if DIST.exists():
        shutil.rmtree(DIST)
    (DIST / "bin").mkdir(parents=True)
    (DIST / "workers").mkdir(parents=True)
    shutil.copy2(exe, DIST / "bin/delta-desktop.exe")
    worker_src = ROOT / "workers/python/delta_worker.py"
    shutil.copy2(worker_src, DIST / "workers/delta_worker.py")

    licenses, lic_err = cargo_licenses()
    lines = ["# 第三方依赖许可证清单（R1 打包）", "",
             "来源：`cargo metadata --locked` 采集；许可证字段原样引用 crate 元数据。",
             "本项目自身许可证尚未定案（台账 Q-02），本文件不构成授权结论。", ""]
    if lic_err:
        lines += [f"采集失败：{lic_err}", ""]
    else:
        lines += ["| crate | version | license | repository |", "| --- | --- | --- | --- |"]
        lines += [f"| {r['name']} | {r['version']} | {r['license']} | {r['repository']} |"
                  for r in licenses]
    (DIST / "THIRD-PARTY-LICENSES.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    version = {
        "stage": "R1",
        "branch": git("rev-parse", "--abbrev-ref", "HEAD"),
        "head": git("rev-parse", "HEAD"),
        "stage_parent": git("rev-parse", "HEAD~1"),
        "rustc": rustc_version(),
        "python": sys.version.split()[0],
        "artifacts": {
            "bin/delta-desktop.exe": sha256(DIST / "bin/delta-desktop.exe"),
            "workers/delta_worker.py": sha256(DIST / "workers/delta_worker.py"),
        },
        "third_party_dependencies": len(licenses),
    }
    (DIST / "VERSION.json").write_text(
        json.dumps(version, ensure_ascii=False, indent=2), encoding="utf-8")

    readme = f"""DELTA R1 分发（本地验证用）
================================

1. bin/delta-desktop.exe 为 release 构建的桌面程序；Windows 首发目标。
   当前阶段为 POC：窗口、导航、图表交互使用内置合成数据，
   尚未连接真实账户、行情或模型端点。
2. workers/delta_worker.py 通过 stdin/stdout JSONL 与宿主通信（协议 v1）：
   {{"v":1,"id":"1","method":"handshake","params":{{}}}} /
   {{"v":1,"id":"2","method":"compute_indicators","params":{{...}}}}。
   依赖：Python {sys.version.split()[0]}（仅标准库）。
3. VERSION.json 记录本包对应的源码指纹（commit {version['head'][:12]}）。
4. 已知限制：导入/备份/导出/删除与真实数据接入尚未实现，
   见 docs/evidence/r1-delivery.md 的结果矩阵与集中台账。
"""
    (DIST / "README.txt").write_text(readme, encoding="utf-8")

    REPORT_DIR.mkdir(parents=True, exist_ok=True)
    report = {
        "entry": "package_r1.py",
        "dist": "dist/r1",
        "files": sorted(str(p.relative_to(ROOT)).replace("\\", "/")
                        for p in DIST.rglob("*") if p.is_file()),
        "version": version,
        "license_rows": len(licenses),
        "license_error": lic_err or None,
    }
    (REPORT_DIR / "package.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
