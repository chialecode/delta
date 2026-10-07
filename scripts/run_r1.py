"""R1 启动入口：合成数据演示运行（不接触任何真实账户或个人数据）。

用法：
    python scripts/run_r1.py --demo [--seconds 8] [--keep-alive]

行为：
  1. 通过 Cargo 检查源码新鲜度并从锁文件构建 delta-desktop。
  2. 在 `.local/demo/` 下准备隔离的合成工作区（本次 POC 使用内置合成行，
     不读写个人数据库；目录仅用于记录运行事实）。
  3. 启动桌面程序，确认窗口事件循环存活 `--seconds` 秒后结束进程
     （`--keep-alive` 则不结束，交给人工观察）。
  4. 把实际结果写入 `.local/reports/r1/run_demo.json`，失败以非零码退出。

限制：本入口只证明“能构建、能启动、事件循环存活”；真实键鼠/IME/DPI 行为
属于人工项 R1-M-01，不能由本脚本替代。
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPORT_DIR = ROOT / ".local/reports/r1"
DEMO_DIR = ROOT / ".local/demo"
EXE = ROOT / "target/debug/delta-desktop.exe"


def build_if_needed() -> tuple[bool, str]:
    # Cargo's incremental freshness check is cheap, and an existing executable
    # alone is not evidence that it corresponds to the current source tree.
    proc = subprocess.run(
        ["cargo", "build", "--locked", "-p", "delta-desktop"],
        cwd=ROOT, capture_output=True, text=True,
    )
    ok = proc.returncode == 0 and EXE.exists()
    return ok, (proc.stdout + proc.stderr)[-1500:]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--demo", action="store_true", help="以合成数据启动桌面程序")
    ap.add_argument("--seconds", type=float, default=8.0, help="存活观察秒数")
    ap.add_argument("--keep-alive", action="store_true", help="观察后不结束进程")
    args = ap.parse_args()
    if not args.demo:
        print("usage: python scripts/run_r1.py --demo", file=sys.stderr)
        return 2

    REPORT_DIR.mkdir(parents=True, exist_ok=True)
    DEMO_DIR.mkdir(parents=True, exist_ok=True)
    fixture = ROOT / "tests/fixtures/ac02-events.json"
    if fixture.exists():
        shutil.copyfile(fixture, DEMO_DIR / "synthetic-events.json")

    ok, note = build_if_needed()
    report: dict = {
        "entry": "run_r1.py --demo",
        "artifact": str(EXE.relative_to(ROOT)).replace("\\", "/"),
        "built": ok,
        "build_note": note if not ok else "ok",
        "synthetic_fixture": "tests/fixtures/ac02-events.json",
        "observed_seconds": 0.0,
        "alive_after_wait": False,
        "exit_code": None,
        "kept_alive": bool(args.keep_alive),
        "limits": [
            "合成数据演示；未连接真实账户、行情或模型端点",
            "不替代 R1-M-01 人工 IME/DPI/交互检查",
        ],
    }
    if not ok:
        (REPORT_DIR / "run_demo.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print("build failed", file=sys.stderr)
        return 1

    proc = subprocess.Popen([str(EXE), "--demo"], cwd=str(DEMO_DIR))
    deadline = time.time() + args.seconds
    while time.time() < deadline and proc.poll() is None:
        time.sleep(0.25)
    report["observed_seconds"] = round(time.time() - (deadline - args.seconds), 2)
    report["alive_after_wait"] = proc.poll() is None
    if report["alive_after_wait"] and not args.keep_alive:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
    report["exit_code"] = proc.returncode
    (REPORT_DIR / "run_demo.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["alive_after_wait"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
