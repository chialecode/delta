"""Collect license obligations for the complete locked dependency graph.

Original notices are retained. Missing texts use a pinned SPDX text plus the
locked source archive, explicitly labelled as a fallback. Downloads are data.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import functools
import hashlib
import json
import re
import shutil
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / ".local" / "license-cache-v2"
# SPDX license-list-data v3.28.0, dereferenced annotated tag; never mutable main.
SPDX_COMMIT = "c4a7237ec8f4654e867546f9f409749300f1bf4c"
PREFERENCES = ("MIT", "Apache-2.0", "Unicode-3.0", "BSD-3-Clause", "ISC",
               "BSD-2-Clause", "MPL-2.0", "Zlib", "0BSD", "CC0-1.0", "NCSA",
               "BSL-1.0", "MIT-0")


def selected_licenses(expression: str) -> tuple[str, ...]:
    """Select one offered OR branch while preserving every AND obligation."""
    normalized = expression.replace("/", " OR ")  # legacy Cargo syntax
    tokens = re.findall(r"\(|\)|[A-Za-z0-9.+-]+", normalized)
    if re.sub(r"\s+", "", normalized) != "".join(tokens):
        raise ValueError(f"invalid SPDX expression: {expression}")
    offset = 0

    def atom() -> list[tuple[str, ...]]:
        nonlocal offset
        if offset >= len(tokens):
            raise ValueError(f"incomplete SPDX expression: {expression}")
        token = tokens[offset]
        offset += 1
        if token == "(":
            branches = either()
            if offset >= len(tokens) or tokens[offset] != ")":
                raise ValueError(f"unclosed SPDX expression: {expression}")
            offset += 1
            return branches
        if token in ("AND", "OR", "WITH", ")"):
            raise ValueError(f"unexpected SPDX operator: {expression}")
        if offset < len(tokens) and tokens[offset] == "WITH":
            offset += 1
            if offset >= len(tokens) or tokens[offset] in ("AND", "OR", "WITH", "(", ")"):
                raise ValueError(f"missing SPDX exception: {expression}")
            token += " WITH " + tokens[offset]
            offset += 1
        return [(token,)]

    def both() -> list[tuple[str, ...]]:
        nonlocal offset
        branches = atom()
        while offset < len(tokens) and tokens[offset] == "AND":
            offset += 1
            right = atom()
            branches = [tuple(dict.fromkeys(a + b)) for a in branches for b in right]
            if len(branches) > 128:
                raise ValueError("SPDX expression has too many branches")
        return branches

    def either() -> list[tuple[str, ...]]:
        nonlocal offset
        branches = both()
        while offset < len(tokens) and tokens[offset] == "OR":
            offset += 1
            branches += both()
            if len(branches) > 128:
                raise ValueError("SPDX expression has too many branches")
        return branches

    branches = either()
    if offset != len(tokens):
        raise ValueError(f"unexpected SPDX token: {expression}")
    offered = [branch for branch in branches if all(item in PREFERENCES for item in branch)]
    if not offered:
        raise ValueError(f"license selection is not documented: {expression}")
    return min(offered, key=lambda branch: (sum(PREFERENCES.index(item) for item in branch), len(branch), branch))


def selected_license(expression: str) -> str:
    return " AND ".join(selected_licenses(expression))


@functools.lru_cache(maxsize=2048)
def fetch(url: str) -> bytes | None:
    request = urllib.request.Request(url, headers={"User-Agent": "delta-license-collector/2.0"})
    try:
        with urllib.request.urlopen(request, timeout=12) as response:
            content = response.read(1024 * 1024 + 1)
        if len(content) > 1024 * 1024 or b"<html" in content[:512].lower():
            return None
        return content
    except (OSError, urllib.error.URLError):
        return None


def local_licenses(directory: Path) -> list[Path]:
    files: list[Path] = []
    for path in directory.iterdir():
        name = path.name.lower()
        if path.is_symlink():
            continue
        if path.is_file() and name.startswith(("license", "licence", "copying", "copyright", "notice")):
            files.append(path)
        elif path.is_dir() and name in ("licenses", "licences", "license"):
            files.extend(p for p in path.rglob("*") if p.is_file() and not p.is_symlink())
    return sorted(files)


def source_identity(package: dict, source: Path) -> dict:
    vcs = source / ".cargo_vcs_info.json"
    checksums = source / ".cargo-checksum.json"
    return {"package_id": package["id"], "source": package.get("source"),
            "manifest_sha256": hashlib.sha256((source / "Cargo.toml").read_bytes()).hexdigest(),
            "vcs": json.loads(vcs.read_text(encoding="utf-8")) if vcs.is_file() else None,
            "package_checksum": json.loads(checksums.read_text(encoding="utf-8")).get("package") if checksums.is_file() else None}


def upstream_urls(package: dict, binding: dict, selection: str) -> list[str]:
    repository = (package.get("repository") or "").removesuffix(".git").rstrip("/")
    parsed = urllib.parse.urlparse(repository)
    vcs = binding.get("vcs") or {}
    commit = vcs.get("git", {}).get("sha1", "")
    parts = parsed.path.strip("/").split("/")
    if parsed.hostname != "github.com" or len(parts) != 2 or not re.fullmatch(r"[a-fA-F0-9]{40}", commit):
        return []
    folder = PurePosixPath(vcs.get("path_in_vcs", ""))
    if folder.is_absolute() or ".." in folder.parts:
        return []
    folders = list(dict.fromkeys([folder, *folder.parents, PurePosixPath(".")]))
    filenames = [f"LICENSE-{selection}", f"LICENSE-{selection}.txt", "LICENSE", "LICENSE.md",
                 "LICENSE.txt", "LICENCE", "LICENCE.md", "COPYING"]
    if selection == "Apache-2.0":
        filenames.insert(0, "LICENSE-APACHE")
    return [f"https://raw.githubusercontent.com/{parts[0]}/{parts[1]}/{commit}/"
            f"{str(parent / filename).removeprefix('./')}" for parent in folders for filename in filenames]


def matches(content: bytes, selection: str) -> bool:
    text = content.decode("utf-8", errors="replace").lower()
    if len(text) < 200:
        return False
    if selection == "MIT":
        return "permission is hereby granted" in text and "above copyright notice" in text and "permission notice" in text
    if selection == "MIT-0":
        return "permission is hereby granted" in text and "above copyright notice" not in text
    if selection == "Apache-2.0":
        return "apache license" in text and "version 2.0" in text and "definitions" in text
    if selection == "MPL-2.0":
        return "mozilla public license" in text and "2.0" in text and "covered software" in text
    if selection in ("BSD-2-Clause", "BSD-3-Clause"):
        base = "redistribution and use in source and binary forms" in text and "this list of conditions" in text
        third = "neither the name" in text or "names of its contributors" in text
        return base and (third if selection == "BSD-3-Clause" else not third)
    if selection == "ISC":
        return "permission to use, copy, modify" in text and "provided that the above copyright notice" in text
    if selection == "0BSD":
        return "permission to use, copy, modify" in text and "with or without fee" in text and "provided that" not in text
    if selection == "Unicode-3.0":
        return "unicode" in text and ("license v3" in text or "license v. 3" in text) and "permission" in text
    if selection == "Zlib":
        return "origin of this software" in text and "altered source versions" in text and "must not" in text
    if selection == "CC0-1.0":
        return "creative commons" in text and "affirmer" in text and "waiver" in text
    if selection == "NCSA":
        return "ncsa" in text and "redistributions" in text and "neither" in text
    if selection == "BSL-1.0":
        return "boost software license" in text and "1.0" in text and "permission" in text
    return False


def source_archive(source: Path, package: dict, binding: dict, target: Path) -> dict:
    archive = source.parents[2] / "cache" / source.parent.name / f"{package['name']}-{package['version']}.crate"
    checksum = binding["package_checksum"]
    if checksum and archive.is_file() and hashlib.sha256(archive.read_bytes()).hexdigest() == checksum:
        destination = target / "original-source.crate"
        shutil.copyfile(archive, destination)
        origin = "locked-registry-archive"
    else:
        destination = target / "original-source.zip"
        with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, strict_timestamps=False) as bundle:
            for path in sorted(source.rglob("*")):
                if path.is_file() and not path.is_symlink():
                    bundle.write(path, path.relative_to(source).as_posix())
        origin = "locked-source-directory"
    return {"file": destination.name, "origin_type": origin,
            "sha256": hashlib.sha256(destination.read_bytes()).hexdigest()}


def collect(package: dict, output: Path) -> dict:
    expression = package.get("license") or ""
    selections = selected_licenses(expression)
    source = Path(package["manifest_path"]).parent
    binding = source_identity(package, source)
    source_id = hashlib.sha256(json.dumps(binding, sort_keys=True).encode()).hexdigest()
    key = f"{package['name']}-{package['version']}-{source_id[:12]}"
    target = output / key
    target.mkdir(parents=True)
    records: list[dict] = []
    coverage: dict[str, list[str]] = {selection: [] for selection in selections}
    for path in local_licenses(source):
        relative = path.relative_to(source)
        destination = target / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        content = path.read_bytes()
        destination.write_bytes(content)
        records.append({"file": relative.as_posix(), "origin_type": "locked-source-license-or-notice",
                        "sha256": hashlib.sha256(content).hexdigest()})
        for selection in selections:
            if matches(content, selection):
                coverage[selection].append(relative.as_posix())
    standard_fallback = False
    for selection in selections:
        if coverage[selection]:
            continue
        cache = CACHE / source_id / selection
        cache.mkdir(parents=True, exist_ok=True)
        cached = cache / "record.json"
        if cached.is_file():
            record = json.loads(cached.read_text(encoding="utf-8"))
            content = (cache / "text.txt").read_bytes()
            if record["source_id"] != source_id or record["spdx"] != selection or hashlib.sha256(content).hexdigest() != record["sha256"]:
                raise ValueError(f"cached license identity/hash differs: {key}")
        else:
            content = None
            url = ""
            origin_type = "upstream-commit-license"
            for url in upstream_urls(package, binding, selection):
                candidate = fetch(url)
                if candidate is not None and matches(candidate, selection):
                    content = candidate
                    break
            if content is None:
                url = f"https://raw.githubusercontent.com/spdx/license-list-data/{SPDX_COMMIT}/text/{selection}.txt"
                content = fetch(url)
                origin_type = "standard-license-text"
            if content is None or not matches(content, selection):
                raise ValueError(f"license obligation could not be collected: {key} ({selection})")
            record = {"origin_type": origin_type, "url": url, "source_id": source_id, "spdx": selection,
                      "sha256": hashlib.sha256(content).hexdigest(),
                      "retrieved_at": dt.datetime.now(dt.timezone.utc).isoformat()}
            temporary = cache / f"record-{uuid.uuid4()}.json"
            (cache / "text.txt").write_bytes(content)
            temporary.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
            temporary.replace(cached)
        filename = f"collected-{selection}.txt"
        (target / filename).write_bytes(content)
        records.append({"file": filename, **record})
        coverage[selection].append(filename)
        standard_fallback |= record["origin_type"] == "standard-license-text"
    if standard_fallback:
        records.append(source_archive(source, package, binding, target))
    attribution = {"name": package["name"], "version": package["version"],
                   "license_expression": expression, "selected_license": " AND ".join(selections),
                   "selected_obligations": list(selections), "obligation_files": coverage,
                   "authors": package.get("authors", []), "repository": package.get("repository"),
                   "source": package.get("source"), "source_binding": binding, "source_id": source_id,
                   "directory": key, "standard_text_with_source_fallback": standard_fallback,
                   "original_files": records}
    (target / "ATTRIBUTION.json").write_text(json.dumps(attribution, ensure_ascii=False, indent=2), encoding="utf-8")
    return attribution


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    metadata_bytes = args.metadata.read_bytes()
    data = json.loads(metadata_bytes)
    workspace = set(data["workspace_members"])
    packages = [p for p in data["packages"] if p["id"] not in workspace]
    args.output.mkdir(parents=True, exist_ok=False)
    results, errors = [], []
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        futures = {pool.submit(collect, package, args.output): package["name"] for package in packages}
        for future in concurrent.futures.as_completed(futures):
            try:
                results.append(future.result())
            except Exception as error:
                errors.append(str(error))
    manifest = {"schema_version": 2, "metadata_sha256": hashlib.sha256(metadata_bytes).hexdigest(),
                "spdx_license_list_commit": SPDX_COMMIT,
                "collected_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                "packages": sorted(results, key=lambda p: (p["name"], p["version"], p["source_id"])), "errors": sorted(errors)}
    (args.output / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"collected": len(results), "expected": len(packages), "errors": sorted(errors)}, ensure_ascii=False))
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
