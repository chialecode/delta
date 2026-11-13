"""Dependency inventory and vulnerability check (decision of 2026-11-14, dev-rules/quality-gates.md#dependencies).

    python scripts/deps.py           inventory: run when a stage starts and again before the stage hand-over
    python scripts/deps.py --audit   known vulnerabilities: a failure blocks the stage hand-over

The inventory lists every direct crates.io dependency of the workspace with a newer, not yanked, stable
version at least MIN_AGE_DAYS old (the same age as Dependabot's cooldown in .github/dependabot.yml), split
into compatible updates (`cargo update` can take them) and breaking ones (Cargo.toml has to change), plus
the newest Rust release of that age against rust-toolchain.toml. It only reports; what to upgrade is
decided in the stage plan.

The audit asks OSV (https://osv.dev, which carries the RustSec advisory database) about every crates.io
package in Cargo.lock. It exits 1 on a vulnerability not listed in scripts/deps-exceptions.json, or on an
exception past its reviewBy date. RustSec's informational advisories (unmaintained, unsound) are listed
but do not fail. Exit 2 means the check could not run (no network): record it as not run, never as passed.

Standard library only. Run from the repository root.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tomllib
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import date, datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIN_AGE_DAYS = 7
USER_AGENT = "delta-dependency-inventory"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
STABLE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


class NotRun(Exception):
    """The check could not run; reported as not run, never as passed."""


def fetch(url: str, body: bytes | None = None):
    headers = {"User-Agent": USER_AGENT}
    if body is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=body, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode("utf-8")
    except OSError as error:
        raise NotRun(f"{url}: {error}") from error


def stable(version: str) -> tuple[int, int, int] | None:
    match = STABLE.match(version)
    return tuple(int(part) for part in match.groups()) if match else None  # type: ignore[return-value]


def compatible(a: tuple[int, int, int], b: tuple[int, int, int]) -> bool:
    """Whether Cargo's default (caret) requirement treats the two versions as compatible."""
    if a[0] != 0 or b[0] != 0:
        return a[0] == b[0]
    if a[1] != 0 or b[1] != 0:
        return a[1] == b[1]
    return a[2] == b[2]


def index_path(name: str) -> str:
    """Path of a crate in the crates.io sparse index."""
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def age_days(stamp: str, now: datetime) -> int:
    return (now - datetime.fromisoformat(stamp.replace("Z", "+00:00"))).days


def direct_dependencies() -> list[tuple[str, str]]:
    """(name, locked version) of every crates.io package a workspace member depends on directly."""
    result = subprocess.run(["cargo", "metadata", "--format-version", "1"], cwd=ROOT, capture_output=True,
                            text=True, encoding="utf-8", check=False)
    if result.returncode != 0:
        raise NotRun(f"cargo metadata: {result.stderr.strip()[:500]}")
    metadata = json.loads(result.stdout)
    members = set(metadata["workspace_members"])
    found: set[tuple[str, str]] = set()
    for node in metadata["resolve"]["nodes"]:
        if node["id"] not in members:
            continue
        for dep in node["deps"]:
            pkg = dep["pkg"]
            if pkg.startswith(CRATES_IO + "#"):
                name, _, version = pkg.split("#", 1)[1].rpartition("@")
                found.add((name, version))
    return sorted(found)


def inventory() -> int:
    now = datetime.now(timezone.utc)

    def newer(item: tuple[str, str]) -> dict:
        name, current = item
        current_parts = stable(current) or (0, 0, 0)
        releases = []
        for line in fetch(f"https://index.crates.io/{index_path(name)}").splitlines():
            entry = json.loads(line)
            parts = stable(entry["vers"])
            if parts and parts > current_parts and not entry.get("yanked") and entry.get("pubtime"):
                releases.append((parts, entry["vers"], age_days(entry["pubtime"], now)))
        releases.sort(reverse=True)
        ripe = [release for release in releases if release[2] >= MIN_AGE_DAYS]
        same = next((r for r in ripe if compatible(r[0], current_parts)), None)
        breaking = next((r for r in ripe if not compatible(r[0], current_parts)), None)
        return {"name": name, "current": current, "compatible": same, "breaking": breaking,
                "newest": releases[0] if releases else None}

    with ThreadPoolExecutor(max_workers=8) as pool:
        rows = list(pool.map(newer, direct_dependencies()))

    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]["channel"]
    rust = {"name": "rust (rust-toolchain.toml)", "current": toolchain, "compatible": None, "breaking": None, "newest": None}
    current_rust = stable(toolchain)
    if current_rust:
        releases = json.loads(fetch("https://api.github.com/repos/rust-lang/rust/releases?per_page=20"))
        candidates = sorted(((stable(r["tag_name"]), r["tag_name"], age_days(r["published_at"], now)) for r in releases
                             if stable(r["tag_name"]) and not r["prerelease"] and r.get("published_at")), reverse=True)
        candidates = [c for c in candidates if c[0] > current_rust]
        rust["newest"] = candidates[0] if candidates else None
        rust["compatible"] = next((c for c in candidates if c[2] >= MIN_AGE_DAYS), None)
    rows.append(rust)

    due = [row for row in rows if row["compatible"] or row["breaking"]]
    print(f"[deps] newer stable versions at least {MIN_AGE_DAYS} days old (direct crates.io dependencies and the toolchain):")
    for row in due:
        parts = []
        if row["compatible"]:
            parts.append(f"{row['compatible'][1]} (compatible, {row['compatible'][2]} days)")
        if row["breaking"]:
            parts.append(f"{row['breaking'][1]} (breaking, {row['breaking'][2]} days)")
        print(f"  {row['name']} {row['current']} -> {', '.join(parts)}")
    if not due:
        print("  none")
    waiting = [row for row in rows if row["newest"] and row["newest"][2] < MIN_AGE_DAYS]
    if waiting:
        print(f"[deps] newer, but younger than {MIN_AGE_DAYS} days (not due yet):")
        for row in waiting:
            print(f"  {row['name']} {row['current']} -> {row['newest'][1]} ({row['newest'][2]} days)")
    print(json.dumps({"status": "reported", "check": "dependency-inventory", "due": len(due), "waiting": len(waiting)}))
    return 0


def audit() -> int:
    exceptions = json.loads((ROOT / "scripts/deps-exceptions.json").read_text(encoding="utf-8")).get("exceptions", [])
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    packages = sorted({(p["name"], p["version"]) for p in lock.get("package", []) if p.get("source") == CRATES_IO})
    queries = [{"package": {"name": name, "ecosystem": "crates.io"}, "version": version} for name, version in packages]
    results = json.loads(fetch("https://api.osv.dev/v1/querybatch", json.dumps({"queries": queries}).encode()))["results"]
    if len(results) != len(packages):
        raise NotRun(f"OSV returned {len(results)} results for {len(packages)} packages")

    details: dict[str, dict] = {}
    findings = []  # (advisory, package, version); one per advisory and package, aliases folded together
    seen: set[tuple[str, str]] = set()
    for (name, version), result in zip(packages, results):
        if result.get("next_page_token"):
            raise NotRun(f"OSV paged the results for {name}; the check does not follow pages")
        for hit in result.get("vulns", []):
            if (name, hit["id"]) in seen:
                continue
            if hit["id"] not in details:
                details[hit["id"]] = json.loads(fetch(f"https://api.osv.dev/v1/vulns/{hit['id']}"))
            advisory = details[hit["id"]]
            ids = {advisory["id"], *advisory.get("aliases", [])}
            seen.update((name, i) for i in ids)
            if not advisory.get("withdrawn"):
                findings.append((advisory, ids, name, version))

    today = date.today().isoformat()
    open_count = informational = 0
    used: set[str] = set()
    for advisory, ids, name, version in findings:
        # RustSec marks unmaintained / unsound advisories as informational on each affected entry.
        kind = next((a["database_specific"]["informational"] for a in advisory.get("affected", [])
                     if (a.get("database_specific") or {}).get("informational")), None)
        matched = [e for e in exceptions if e.get("advisory") in ids]
        used.update(e["advisory"] for e in matched)
        fixed = sorted({event["fixed"] for affected in advisory.get("affected", []) for r in affected.get("ranges", [])
                        for event in r.get("events", []) if "fixed" in event}) or ["no release yet"]
        line = f"[deps] {advisory['id']} {name}@{version}: {advisory.get('summary', '')}; fixed in {', '.join(fixed)}"
        if kind:
            informational += 1
            print(f"{line} (informational: {kind}; not a failure)")
        elif not matched:
            open_count += 1
            print(line, file=sys.stderr)
    expired = [e for e in exceptions if not e.get("reviewBy") or e["reviewBy"] < today]
    for e in expired:
        print(f"[deps] exception {e.get('advisory')} ({e.get('package')}) passed its reviewBy date {e.get('reviewBy')!r}", file=sys.stderr)
    for e in exceptions:
        if e.get("advisory") not in used:
            print(f"[deps] exception {e.get('advisory')} ({e.get('package')}) no longer matches an advisory: remove it")
    ok = open_count == 0 and not expired
    print(json.dumps({"status": "passed" if ok else "failed", "check": "dependency-audit", "packages": len(packages),
                      "advisories": len(findings), "informational": informational, "open": open_count,
                      "expiredExceptions": len(expired)}))
    return 0 if ok else 1


def main() -> int:
    is_audit = "--audit" in sys.argv[1:]
    try:
        return audit() if is_audit else inventory()
    except NotRun as error:
        print(f"[deps] not run: {error}", file=sys.stderr)
        print(json.dumps({"status": "not-run", "check": "dependency-audit" if is_audit else "dependency-inventory"}))
        return 2


if __name__ == "__main__":
    sys.exit(main())
