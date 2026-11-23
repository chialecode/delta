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

import argparse
import http.client
import json
import re
import subprocess
import sys
import tomllib
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import date, datetime, timezone
from pathlib import Path

from _quiet import run_quiet

ROOT = Path(__file__).resolve().parents[1]
MIN_AGE_DAYS = 7
OSV_BATCH_SIZE = 500
USER_AGENT = "delta-dependency-inventory"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
STABLE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


class NotRun(Exception):
    """The check could not run; reported as not run, never as passed."""


def fetch(url: str, body: bytes | None = None) -> str:
    headers = {"User-Agent": USER_AGENT}
    if body is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=body, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode("utf-8")
    except (OSError, http.client.HTTPException, UnicodeDecodeError) as error:
        raise NotRun(f"{url}: {error}") from error


def fetch_json(url: str, body: bytes | None = None):
    raw = fetch(url, body)
    try:
        return json.loads(raw)
    except ValueError as error:
        raise NotRun(f"{url}: invalid JSON response: {error}") from error


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


def parse_review_by(raw: object) -> date | None:
    """Strictly parse a YYYY-MM-DD exception review date; invalid values return None."""
    if not isinstance(raw, str) or not ISO_DATE.match(raw):
        return None
    try:
        return date.fromisoformat(raw)
    except ValueError:
        return None


def direct_dependencies(root: Path = ROOT) -> list[tuple[str, str]]:
    """(name, locked version) of every crates.io package a workspace member depends on directly."""
    try:
        result = run_quiet(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            cwd=root,
            timeout=120,
            capture=True,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise NotRun(f"cargo metadata: {error}") from error
    if result.returncode != 0:
        raise NotRun(f"cargo metadata: {result.stderr.strip()[:500]}")
    try:
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
    except (ValueError, KeyError, TypeError) as error:
        raise NotRun(f"cargo metadata: unexpected payload: {error}") from error


def inventory() -> int:
    now = datetime.now(timezone.utc)

    def newer(item: tuple[str, str]) -> dict:
        name, current = item
        current_parts = stable(current) or (0, 0, 0)
        releases = []
        url = f"https://index.crates.io/{index_path(name)}"
        for line in fetch(url).splitlines():
            if not line.strip():
                continue
            try:
                entry = json.loads(line)
                vers = entry["vers"]
            except (ValueError, KeyError, TypeError) as error:
                raise NotRun(f"{url}: malformed index line: {error}") from error
            parts = stable(vers)
            if parts and parts > current_parts and not entry.get("yanked") and entry.get("pubtime"):
                try:
                    age = age_days(entry["pubtime"], now)
                except ValueError as error:
                    raise NotRun(f"{url}: invalid pubtime: {error}") from error
                releases.append((parts, vers, age))
        releases.sort(reverse=True)
        ripe = [release for release in releases if release[2] >= MIN_AGE_DAYS]
        same = next((r for r in ripe if compatible(r[0], current_parts)), None)
        breaking = next((r for r in ripe if not compatible(r[0], current_parts)), None)
        return {
            "name": name,
            "current": current,
            "compatible": same,
            "breaking": breaking,
            "newest": releases[0] if releases else None,
        }

    with ThreadPoolExecutor(max_workers=8) as pool:
        rows = list(pool.map(newer, direct_dependencies()))

    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))[
        "toolchain"
    ]["channel"]
    rust = {
        "name": "rust (rust-toolchain.toml)",
        "current": toolchain,
        "compatible": None,
        "breaking": None,
        "newest": None,
    }
    current_rust = stable(toolchain)
    if current_rust:
        releases = fetch_json("https://api.github.com/repos/rust-lang/rust/releases?per_page=20")
        if not isinstance(releases, list):
            raise NotRun("GitHub releases API returned unexpected payload")
        candidates = []
        try:
            for r in releases:
                tag = r.get("tag_name", "")
                parts = stable(tag)
                if parts and parts > current_rust and not r.get("prerelease") and r.get("published_at"):
                    candidates.append((parts, tag, age_days(r["published_at"], now)))
        except (AttributeError, ValueError, TypeError) as error:
            raise NotRun(f"GitHub releases API malformed entry: {error}") from error
        candidates.sort(reverse=True)
        rust["newest"] = candidates[0] if candidates else None
        rust["compatible"] = next((c for c in candidates if c[2] >= MIN_AGE_DAYS), None)
    rows.append(rust)

    due = [row for row in rows if row["compatible"] or row["breaking"]]
    print(
        f"[deps] newer stable versions at least {MIN_AGE_DAYS} days old (direct crates.io dependencies and the toolchain):"
    )
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
    print(
        json.dumps(
            {
                "status": "reported",
                "check": "dependency-inventory",
                "due": len(due),
                "waiting": len(waiting),
            }
        )
    )
    return 0


def query_osv_batches(packages: list[tuple[str, str]]) -> list[dict]:
    """Query OSV /v1/querybatch in chunks of at most OSV_BATCH_SIZE (API cap is 1000)."""
    all_results: list[dict] = []
    for start in range(0, len(packages), OSV_BATCH_SIZE):
        chunk = packages[start : start + OSV_BATCH_SIZE]
        queries = [
            {"package": {"name": name, "ecosystem": "crates.io"}, "version": version}
            for name, version in chunk
        ]
        payload = fetch_json(
            "https://api.osv.dev/v1/querybatch",
            json.dumps({"queries": queries}).encode("utf-8"),
        )
        if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
            raise NotRun("OSV querybatch returned unexpected payload")
        chunk_results = payload["results"]
        if len(chunk_results) != len(chunk):
            raise NotRun(f"OSV returned {len(chunk_results)} results for {len(chunk)} packages")
        all_results.extend(chunk_results)
    return all_results


def _as_list(value: object, label: str) -> list:
    if value is None:
        return []
    if not isinstance(value, list):
        raise NotRun(f"OSV returned non-list {label}")
    return value


def audit(root: Path = ROOT, today: date | None = None) -> int:
    exceptions = json.loads(
        (root / "scripts/deps-exceptions.json").read_text(encoding="utf-8")
    ).get("exceptions", [])
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    packages = sorted(
        {(p["name"], p["version"]) for p in lock.get("package", []) if p.get("source") == CRATES_IO}
    )
    results = query_osv_batches(packages)

    details: dict[str, dict] = {}
    # (advisory, ids, package, version); one per advisory and (package, version), aliases folded together.
    findings = []
    seen: set[tuple[str, str, str]] = set()
    for (name, version), result in zip(packages, results):
        if not isinstance(result, dict):
            raise NotRun(f"OSV returned non-object result for {name}@{version}")
        if result.get("next_page_token"):
            raise NotRun(f"OSV paged the results for {name}; the check does not follow pages")
        for hit in _as_list(result.get("vulns"), f"vulns for {name}@{version}"):
            if not isinstance(hit, dict) or not isinstance(hit.get("id"), str):
                raise NotRun(f"OSV returned malformed vuln entry for {name}@{version}")
            vuln_id = hit["id"]
            if (name, version, vuln_id) in seen:
                continue
            if vuln_id not in details:
                quoted = urllib.parse.quote(vuln_id, safe="")
                advisory_obj = fetch_json(f"https://api.osv.dev/v1/vulns/{quoted}")
                if not isinstance(advisory_obj, dict) or not isinstance(advisory_obj.get("id"), str):
                    raise NotRun(f"OSV returned malformed advisory for {vuln_id}")
                details[vuln_id] = advisory_obj
            advisory = details[vuln_id]
            aliases = _as_list(advisory.get("aliases"), f"aliases for {vuln_id}")
            if not all(isinstance(alias, str) for alias in aliases):
                raise NotRun(f"OSV returned non-string alias for {vuln_id}")
            ids = {advisory["id"], *aliases}
            seen.update((name, version, i) for i in ids)
            if not advisory.get("withdrawn"):
                findings.append((advisory, ids, name, version))

    today_date = today or date.today()
    open_count = informational = 0
    used_indices: set[int] = set()
    for advisory, ids, name, version in findings:
        affected_list = _as_list(advisory.get("affected"), f"affected for {advisory['id']}")
        # RustSec marks unmaintained / unsound advisories as informational on each affected entry.
        kind = None
        fixed_versions: set[str] = set()
        for affected in affected_list:
            if not isinstance(affected, dict):
                raise NotRun(f"OSV returned malformed affected entry for {advisory['id']}")
            db_specific = affected.get("database_specific")
            if kind is None and isinstance(db_specific, dict) and db_specific.get("informational"):
                kind = db_specific["informational"]
            for rng in _as_list(affected.get("ranges"), f"ranges for {advisory['id']}"):
                if not isinstance(rng, dict):
                    raise NotRun(f"OSV returned malformed range entry for {advisory['id']}")
                for event in _as_list(rng.get("events"), f"events for {advisory['id']}"):
                    if isinstance(event, dict) and isinstance(event.get("fixed"), str):
                        fixed_versions.add(event["fixed"])
        matched = [
            idx
            for idx, e in enumerate(exceptions)
            if e.get("advisory") in ids and e.get("package") == name
        ]
        used_indices.update(matched)
        fixed = sorted(fixed_versions) or ["no release yet"]
        line = f"[deps] {advisory['id']} {name}@{version}: {advisory.get('summary', '')}; fixed in {', '.join(fixed)}"
        if kind:
            informational += 1
            print(f"{line} (informational: {kind}; not a failure)")
        elif not matched:
            open_count += 1
            print(line, file=sys.stderr)

    expired = []
    for e in exceptions:
        parsed_review_by = parse_review_by(e.get("reviewBy"))
        if parsed_review_by is None or parsed_review_by < today_date:
            expired.append(e)
    for e in expired:
        print(
            f"[deps] exception {e.get('advisory')} ({e.get('package')}) has invalid or past reviewBy date {e.get('reviewBy')!r}",
            file=sys.stderr,
        )
    for idx, e in enumerate(exceptions):
        if idx not in used_indices:
            print(
                f"[deps] exception {e.get('advisory')} ({e.get('package')}) no longer matches an advisory: remove it"
            )
    ok = open_count == 0 and not expired
    print(
        json.dumps(
            {
                "status": "passed" if ok else "failed",
                "check": "dependency-audit",
                "packages": len(packages),
                "advisories": len(findings),
                "informational": informational,
                "open": open_count,
                "expiredExceptions": len(expired),
            }
        )
    )
    return 0 if ok else 1


class _CliParser(argparse.ArgumentParser):
    """Exit 1 (not 2) on invalid CLI flags so exit 2 stays reserved for network `not-run`."""

    def error(self, message: str):
        self.print_usage(sys.stderr)
        self.exit(1, f"{self.prog}: error: {message}\n")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = _CliParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--audit",
        action="store_true",
        help="audit Cargo.lock crates.io packages against OSV/RustSec",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    is_audit = args.audit
    try:
        return audit() if is_audit else inventory()
    except NotRun as error:
        print(f"[deps] not run: {error}", file=sys.stderr)
        print(
            json.dumps(
                {
                    "status": "not-run",
                    "check": "dependency-audit" if is_audit else "dependency-inventory",
                }
            )
        )
        return 2


if __name__ == "__main__":
    sys.exit(main())
