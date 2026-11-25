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
  6. .local/reports/m3/packages       build/makensis logs: newest per identity
  7. .local/tmp/*                     temporary files
  --cargo                             also the whole target/ and .local/target-packages/

Uses only the Python 3.12 standard library. Never touches tracked files or workers/.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
import zipfile
from pathlib import Path

from _quiet import git_lines

ROOT = Path(__file__).resolve().parent.parent
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
IDENTITIES = ("delta-dev-test", "delta-test", "delta-dev", "delta")
SEMVER_CORE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def is_link_or_junction(path: Path) -> bool:
    return path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction())


def is_safe_target(path: Path, root: Path = ROOT) -> bool:
    """Reject symlinks, directory junctions, ancestors that are links, or paths outside `root`."""
    resolved_root = root.resolve()
    try:
        rel = path.relative_to(root)
    except ValueError:
        try:
            rel = path.relative_to(resolved_root)
        except ValueError:
            return False
    if not rel.parts:
        return False
    current = root
    for part in rel.parts:
        current = current / part
        if is_link_or_junction(current):
            return False
    resolved = path.resolve(strict=False)
    return resolved != resolved_root and resolved.is_relative_to(resolved_root)


def size_of(path: Path) -> int:
    if is_link_or_junction(path):
        return 0
    if path.is_file():
        return path.stat().st_size
    total = 0
    for dirpath, dirnames, filenames in os.walk(path, followlinks=False):
        base = Path(dirpath)
        dirnames[:] = [d for d in dirnames if not is_link_or_junction(base / d)]
        for filename in filenames:
            item = base / filename
            try:
                if not is_link_or_junction(item):
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


def tracked_text(root: Path = ROOT) -> str:
    """Tracked Markdown/JSON text, used to keep artifacts that documents still cite."""
    files = git_lines(root, "ls-files", "--", "*.md", "*.json")
    return "\n".join(
        (root / name).read_text(encoding="utf-8", errors="replace")
        for name in files
        if (root / name).is_file()
    )


def identity_of(name: str) -> str | None:
    for identity in IDENTITIES:  # longest first: delta-dev-test before delta-dev/delta
        rest = name.removeprefix(f"{identity}-setup-")
        if rest != name and rest[:1].isdigit():
            return identity
    return None


def installer_sort_key(path: Path, identity: str) -> tuple:
    """Sort key by SemVer 2.0.0 version (release > prerelease) with mtime tiebreak."""
    mtime = path.stat().st_mtime
    stem = path.name.removeprefix(f"{identity}-setup-").removesuffix("-x64.exe")
    without_build = stem.split("+", 1)[0]
    core_str, _, pre_str = without_build.partition("-")
    match = SEMVER_CORE.match(core_str)
    if not match:
        return (0, 0, 0, 0, 0, (), mtime)
    major, minor, patch = (int(part) for part in match.groups())
    if not pre_str:
        return (1, major, minor, patch, 1, (), mtime)
    pre_key = tuple(
        (0, int(seg)) if seg.isdigit() else (1, seg) for seg in pre_str.split(".")
    )
    return (1, major, minor, patch, 0, pre_key, mtime)


def newest(paths: list[Path]) -> Path | None:
    return max(paths, key=lambda p: p.stat().st_mtime, default=None)


def newest_installer(paths: list[Path], identity: str) -> Path | None:
    return max(paths, key=lambda p: installer_sort_key(p, identity), default=None)


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


def prune_descendants(
    removals: list[tuple[str, Path]], root: Path = ROOT
) -> list[tuple[str, Path]]:
    """Drop duplicates, unsafe links/paths, and entries already under a scheduled parent dir."""
    safe: list[tuple[str, Path]] = []
    seen: set[Path] = set()
    for reason, path in removals:
        if not is_safe_target(path, root):
            continue
        norm = path.resolve(strict=False)
        if norm in seen:
            continue
        seen.add(norm)
        safe.append((reason, path))

    scheduled_dirs = {
        path.resolve(strict=False)
        for _, path in safe
        if path.is_dir() and not is_link_or_junction(path)
    }
    pruned: list[tuple[str, Path]] = []
    for reason, path in safe:
        norm = path.resolve(strict=False)
        if any(parent in scheduled_dirs for parent in norm.parents):
            continue
        pruned.append((reason, path))
    return pruned


def collect(
    include_cargo: bool, root: Path = ROOT, cited: str | None = None
) -> tuple[list[tuple[str, Path]], list[Path]]:
    removals: list[tuple[str, Path]] = []
    cited_text = tracked_text(root) if cited is None else cited
    cited_fingerprints = set(re.findall(r"\b[0-9a-f]{64}\b", cited_text))

    target = root / "target"
    if include_cargo:
        for path in (target, root / ".local" / "target-packages"):
            if path.exists() and not is_link_or_junction(path):
                removals.append(("cargo build cache", path))
    elif target.is_dir() and not is_link_or_junction(target):
        for path in sorted(target.glob("**/incremental")):
            if path.is_dir() and not is_link_or_junction(path) and path.parent.name in ("debug", "release"):
                removals.append(("incremental cache", path))

    dist = root / "dist"
    if dist.is_dir() and not is_link_or_junction(dist):
        installer_groups: dict[str, list[Path]] = {}
        for path in dist.glob("*-x64.exe"):
            if is_link_or_junction(path):
                continue
            identity = identity_of(path.name)
            if identity:
                installer_groups.setdefault(identity, []).append(path)
        for identity, paths in installer_groups.items():
            keep = newest_installer(paths, identity)
            for path in paths:
                if path != keep:
                    removals.append(("older installer", path))
                    sidecar = path.with_suffix(".package.json")
                    if sidecar.exists() and not is_link_or_junction(sidecar):
                        removals.append(("older installer", sidecar))

    reports = root / ".local" / "reports"
    if reports.is_dir() and not is_link_or_junction(reports):
        for stage in sorted(p for p in reports.iterdir() if p.is_dir() and not is_link_or_junction(p)):
            logs = [p for p in stage.glob("logs-*") if p.is_dir() and not is_link_or_junction(p)]
            keep = newest(logs)
            for path in logs:
                if path != keep and path.name not in cited_text:
                    removals.append(("older gate logs", path))
        for path in sorted(reports.rglob("*")):
            if (
                path.is_dir()
                and not is_link_or_junction(path)
                and (
                    re.fullmatch(rf"licenses-{UUID}", path.name)
                    or re.fullmatch(rf"(\.staging|stage)-.+-{UUID}", path.name)
                )
            ):
                removals.append(("packaging scratch", path))

        portable = reports / "m2" / "packages"
        if portable.is_dir() and not is_link_or_junction(portable):
            latest: set[str] = set()
            index = portable / "package-artifacts.json"
            if index.is_file() and not is_link_or_junction(index):
                text = index.read_text("utf-8", errors="replace")
                latest = set(re.findall(UUID, text))
            # Gate reruns rebuild identical packages: per (product, form, cited fingerprint)
            # keep only the newest; uncited fingerprints keep nothing but the latest run.
            portable_groups: dict[tuple[str, str, str], list[Path]] = {}
            for path in sorted(portable.iterdir()):
                if is_link_or_junction(path):
                    continue
                match = re.search(UUID, path.name)
                if not match or path.name.startswith("licenses-") or match.group(0) in latest:
                    continue
                fingerprint = fingerprint_of_package(path)
                if fingerprint not in cited_fingerprints:
                    removals.append(("superseded M2 portable package", path))
                    continue
                product = "delta-dev" if path.name.startswith("delta-dev-") else "delta"
                form = "dir" if path.is_dir() else "zip"
                portable_groups.setdefault((product, form, fingerprint), []).append(path)
            for paths in portable_groups.values():
                keep = newest(paths)
                removals.extend(
                    ("duplicate M2 portable package", p) for p in paths if p != keep
                )

        m3_packages = reports / "m3" / "packages"
        if m3_packages.is_dir() and not is_link_or_junction(m3_packages):
            log_groups: dict[tuple[str, ...], list[Path]] = {}
            for path in m3_packages.glob("*-log.json"):
                if is_link_or_junction(path):
                    continue
                match = re.match(r"(build|makensis)-(delta(?:-dev)?(?:-test)?)-", path.name)
                if match:
                    log_groups.setdefault(match.groups(), []).append(path)
            for paths in log_groups.values():
                keep = newest(paths)
                removals.extend(("older build log", p) for p in paths if p != keep)

    tmp = root / ".local" / "tmp"
    if tmp.is_dir() and not is_link_or_junction(tmp):
        removals.extend(
            ("temporary", p) for p in sorted(tmp.iterdir()) if not is_link_or_junction(p)
        )

    removals = prune_descendants(removals, root)
    removed = {path.resolve(strict=False) for _, path in removals}
    review: list[Path] = []
    local = root / ".local"
    if local.is_dir() and not is_link_or_junction(local):
        for path in sorted(local.iterdir()):
            if path.resolve(strict=False) not in removed and path.name not in (
                "reports",
                "tmp",
                "target-packages",
                # The pinned NSIS build (scripts/fetch_nsis.py); package-build needs it.
                "tools",
            ):
                review.append(path)
    return removals, review


def remove_path(path: Path, root: Path = ROOT) -> None:
    if not is_safe_target(path, root):
        raise RuntimeError(f"refusing to delete unsafe or linked path: {path}")
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--apply", action="store_true", help="delete (default: dry run)")
    parser.add_argument("--cargo", action="store_true", help="also remove target/ entirely")
    args = parser.parse_args(argv)

    removals, review = collect(args.cargo)
    total = 0
    for reason, path in removals:
        size = size_of(path)
        total += size
        print(
            f"{'remove' if args.apply else 'would remove'}  {human(size):>10}  {reason}: "
            f"{path.relative_to(ROOT).as_posix()}"
        )
        if args.apply:
            remove_path(path, ROOT)
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
