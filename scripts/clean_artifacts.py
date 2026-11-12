"""Remove rebuildable or superseded local build artifacts, keeping only the newest.

Dry run by default; pass --apply to delete. Only the categories below are ever removed;
everything else under .local/ is listed with its size for a human decision
(docs/dev-rules/local-files-and-references.md#build-artifacts).

  1. target/**/incremental            rustc incremental cache (rebuilt on demand)
  2. dist/                            installers: newest per identity (with .package.json)
  3. .local/reports/<stage>/logs-*    gate logs: newest per stage + any cited in tracked docs
  4. .local/reports/m2/packages       M2 portable packages: latest run + fingerprints cited
                                      in tracked docs (archived stage results)
  5. licenses-<uuid>/, stage-*/       packaging scratch directories
  6. .local/reports/m3/packages       build/ISCC logs: newest per identity
  7. .local/tmp/*                     temporary files
  --cargo                             also the whole target/ and .local/target-packages/

Uses only the Python 3.12 standard library. Never touches tracked files or workers/.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
IDENTITIES = ("delta-dev-test", "delta-test", "delta-dev", "delta")


def size_of(path: Path) -> int:
    if path.is_file():
        return path.stat().st_size
    total = 0
    for item in path.rglob("*"):
        try:
            if item.is_file() and not item.is_symlink():
                total += item.stat().st_size
        except OSError:
            pass
    return total


def human(size: int) -> str:
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.1f} {unit}" if unit != "B" else f"{int(value)} B"
        value /= 1024
    return f"{size} B"


def tracked_text() -> str:
    """Tracked Markdown/JSON text, used to keep artifacts that documents still cite."""
    files = subprocess.run(
        ["git", "ls-files", "--", "*.md", "*.json"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return "\n".join(
        (ROOT / name).read_text(encoding="utf-8", errors="replace")
        for name in files
        if (ROOT / name).is_file()
    )


def identity_of(name: str) -> str | None:
    for identity in IDENTITIES:  # longest first: delta-dev-test before delta-dev/delta
        rest = name.removeprefix(f"{identity}-setup-")
        if rest != name and rest[:1].isdigit():
            return identity
    return None


def newest(paths: list[Path]) -> Path | None:
    return max(paths, key=lambda p: p.stat().st_mtime, default=None)


def fingerprint_of_package(path: Path) -> str:
    try:
        if path.is_dir():
            manifest = json.loads((path / "package-manifest.json").read_text("utf-8"))
        else:
            with zipfile.ZipFile(path) as archive:
                name = next(n for n in archive.namelist() if n.endswith("package-manifest.json"))
                manifest = json.loads(archive.read(name))
        return str(manifest.get("source_fingerprint", ""))
    except (OSError, ValueError, StopIteration, KeyError, zipfile.BadZipFile):
        return ""


def collect(include_cargo: bool) -> tuple[list[tuple[str, Path]], list[Path]]:
    removals: list[tuple[str, Path]] = []
    cited = tracked_text()
    cited_fingerprints = set(re.findall(r"\b[0-9a-f]{64}\b", cited))

    target = ROOT / "target"
    if include_cargo:
        for path in (target, ROOT / ".local" / "target-packages"):
            if path.exists():
                removals.append(("cargo build cache", path))
    elif target.is_dir():
        for path in sorted(target.glob("**/incremental")):
            if path.is_dir() and path.parent.name in ("debug", "release"):
                removals.append(("incremental cache", path))

    dist = ROOT / "dist"
    if dist.is_dir():
        groups: dict[str, list[Path]] = {}
        for path in dist.glob("*-x64.exe"):
            identity = identity_of(path.name)
            if identity:
                groups.setdefault(identity, []).append(path)
        for paths in groups.values():
            keep = newest(paths)
            for path in paths:
                if path != keep:
                    removals.append(("older installer", path))
                    sidecar = path.with_suffix(".package.json")
                    if sidecar.exists():
                        removals.append(("older installer", sidecar))

    reports = ROOT / ".local" / "reports"
    if reports.is_dir():
        for stage in sorted(p for p in reports.iterdir() if p.is_dir()):
            logs = [p for p in stage.glob("logs-*") if p.is_dir()]
            keep = newest(logs)
            for path in logs:
                if path != keep and path.name not in cited:
                    removals.append(("older gate logs", path))
        for path in sorted(reports.rglob("*")):
            if path.is_dir() and (
                re.fullmatch(rf"licenses-{UUID}", path.name)
                or re.fullmatch(rf"(\.staging|stage)-.+-{UUID}", path.name)
            ):
                removals.append(("packaging scratch", path))

        portable = reports / "m2" / "packages"
        if portable.is_dir():
            latest: set[str] = set()
            index = portable / "package-artifacts.json"
            if index.is_file():
                text = index.read_text("utf-8", errors="replace")
                latest = set(re.findall(UUID, text))
            # Gate reruns rebuild identical packages: per (product, form, cited fingerprint)
            # keep only the newest; uncited fingerprints keep nothing but the latest run.
            groups: dict[tuple[str, str, str], list[Path]] = {}
            for path in sorted(portable.iterdir()):
                match = re.search(UUID, path.name)
                if not match or path.name.startswith("licenses-") or match.group(0) in latest:
                    continue
                fingerprint = fingerprint_of_package(path)
                if fingerprint not in cited_fingerprints:
                    removals.append(("superseded M2 portable package", path))
                    continue
                product = "delta-dev" if path.name.startswith("delta-dev-") else "delta"
                form = "dir" if path.is_dir() else "zip"
                groups.setdefault((product, form, fingerprint), []).append(path)
            for paths in groups.values():
                keep = newest(paths)
                removals.extend(
                    ("duplicate M2 portable package", p) for p in paths if p != keep
                )

        m3_packages = reports / "m3" / "packages"
        if m3_packages.is_dir():
            groups = {}
            for path in m3_packages.glob("*-log.json"):
                match = re.match(r"(build|iscc)-(delta(?:-dev)?(?:-test)?)-", path.name)
                if match:
                    groups.setdefault(match.groups(), []).append(path)
            for paths in groups.values():
                keep = newest(paths)
                removals.extend(("older build log", p) for p in paths if p != keep)

    tmp = ROOT / ".local" / "tmp"
    if tmp.is_dir():
        removals.extend(("temporary", p) for p in sorted(tmp.iterdir()))

    removed = {path for _, path in removals}
    review: list[Path] = []
    local = ROOT / ".local"
    if local.is_dir():
        for path in sorted(local.iterdir()):
            if path not in removed and path.name not in ("reports", "tmp", "target-packages"):
                review.append(path)
    return removals, review


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--apply", action="store_true", help="delete (default: dry run)")
    parser.add_argument("--cargo", action="store_true", help="also remove target/ entirely")
    args = parser.parse_args()

    removals, review = collect(args.cargo)
    total = 0
    for reason, path in removals:
        size = size_of(path)
        total += size
        print(f"{'remove' if args.apply else 'would remove'}  {human(size):>10}  {reason}: "
              f"{path.relative_to(ROOT).as_posix()}")
        if args.apply:
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink()
    verb = "freed" if args.apply else "would free"
    print(f"{verb} {human(total)} in {len(removals)} item(s)")
    if review:
        print("\nkept for a human decision (not build artifacts of the current stage):")
        for path in review:
            print(f"  {human(size_of(path)):>10}  {path.relative_to(ROOT).as_posix()}")
    if not args.apply and removals:
        print("\nrun with --apply to delete")
    return 0


if __name__ == "__main__":
    sys.exit(main())
