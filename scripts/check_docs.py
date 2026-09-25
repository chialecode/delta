"""Dependency-free checks for DELTA's documented Markdown subset and traceability.

Run from any directory. --root supports isolated negative-fixture verification.
This is deliberately not a full Markdown parser or a product acceptance runner.
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


EXCLUDED = {
    ".git", ".local", ".venv", "node_modules", "target", "dist", "build",
    "vendor", "__pycache__", ".pytest_cache", ".worktrees",
}
REQ_FILE = "docs/requirements/product-requirements.md"
AC_FILE = "docs/engineering/delivery-plan.md"
KINDS = {
    "entry", "index", "rule", "requirements", "design", "plan", "status",
    "decision", "research", "evidence", "template",
}


def prose(text: str) -> str:
    """Remove fenced code; allow both backtick and tilde fences."""
    out, fence = [], None
    for line in text.splitlines():
        mark = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if fence is None and mark:
            fence = mark.group(1)
            continue
        if fence is not None:
            stripped = line.strip()
            if len(stripped) >= len(fence) and set(stripped) == {fence[0]}:
                fence = None
            continue
        out.append(line)
    return "\n".join(out)


def anchors(text: str) -> set[str]:
    content, found, counts = prose(text), set(), Counter()
    for heading in re.findall(r"^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$", content, re.M):
        heading = re.sub(r"<[^>]+>", "", heading)
        heading = re.sub(r"\[([^]]+)\]\([^)]*\)", r"\1", heading).lower()
        slug = "".join(c for c in heading if c.isalnum() or c in "_-" or c.isspace())
        slug = re.sub(r"\s", "-", slug)
        found.add(f"{slug}-{counts[slug]}" if counts[slug] else slug)
        counts[slug] += 1
    found.update(re.findall(r'<(?:a|h[1-6])\b[^>]*\bid=[\"\']([^\"\']+)[\"\']', content))
    return found


def check(root: Path) -> tuple[list[str], int, int]:
    root = root.resolve()
    errors: list[str] = []

    def safe_file(name: str) -> Path | None:
        if not isinstance(name, str) or not name or "\\" in name:
            errors.append(f"invalid repository path: {name!r}")
            return None
        path = (root / name).resolve()
        if not path.is_relative_to(root):
            errors.append(f"path escapes repository: {name}")
            return None
        if not path.is_file():
            errors.append(f"missing file: {name}")
            return None
        return path

    def read_json(name: str) -> dict:
        path = safe_file(name)
        if path is None:
            return {}
        try:
            value = json.loads(path.read_text(encoding="utf-8-sig"))
            if not isinstance(value, dict):
                raise ValueError("expected object")
            return value
        except (ValueError, OSError) as exc:
            errors.append(f"{name}: invalid JSON: {exc}")
            return {}

    documents = {}
    for directory, children, files in os.walk(root, followlinks=False):
        children[:] = [name for name in children if name not in EXCLUDED]
        for filename in files:
            if not filename.endswith(".md"):
                continue
            path = Path(directory) / filename
            relative = path.relative_to(root)
            if not path.resolve().is_relative_to(root):
                errors.append(f"Markdown symlink escapes repository: {relative}")
                continue
            documents[relative.as_posix()] = path.read_text(encoding="utf-8-sig")

    registry = read_json("docs/governance/document-registry.json")
    if registry.get("schemaVersion") != 1:
        errors.append("registry: expected schemaVersion 1")
    rows = registry.get("documents", [])
    registered = Counter()
    for row in rows:
        name = row.get("path", "")
        registered[name] += 1
        safe_file(name)
        for field in ("kind", "status", "owner", "scope", "reviewed", "basis"):
            if not isinstance(row.get(field), str) or not row[field].strip():
                errors.append(f"{name}: missing registry {field}")
        if row.get("status") not in {"active", "draft", "reference", "superseded"}:
            errors.append(f"{name}: invalid document status")
        if row.get("kind") not in KINDS:
            errors.append(f"{name}: invalid document kind")
        if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", row.get("reviewed", "")):
            errors.append(f"{name}: invalid reviewed date")
        if row.get("status") == "superseded":
            safe_file(row.get("supersededBy", ""))
    for name in sorted(documents.keys() - registered.keys()):
        errors.append(f"unregistered Markdown: {name}")
    for name in sorted(registered.keys() - documents.keys()):
        errors.append(f"registered path is not owned Markdown: {name}")
    for name, count in registered.items():
        if count != 1:
            errors.append(f"duplicate registry path: {name}")

    for name, text in documents.items():
        content = prose(text)
        if re.search(r"^\s*\[[^]]+\]:", content, re.M) or re.search(r"\[[^]]+\]\[[^]]*\]", content):
            errors.append(f"{name}: use inline links, reference-style links unsupported")
        if re.search(r"(?<![A-Za-z0-9])[A-Za-z]:[\\/]", content):
            errors.append(f"{name}: machine-specific absolute path")
        for match in re.finditer(r"!?\[[^]\n]+\]\((<[^>]+>|[^\s)]+)(?:\s+\"[^\"]*\")?\)", content):
            target = match.group(1).strip("<>")
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc:
                continue
            if "\\" in parsed.path or parsed.path.startswith("/"):
                errors.append(f"{name}: use portable relative link: {target}")
                continue
            dest = ((root / name).parent / unquote(parsed.path)).resolve() if parsed.path else root / name
            if not dest.is_relative_to(root) or not dest.is_file():
                errors.append(f"{name}: broken or escaping link: {target}")
                continue
            if parsed.fragment and dest.suffix.lower() == ".md":
                dest_text = documents.get(dest.relative_to(root).as_posix(), "")
                if unquote(parsed.fragment) not in anchors(dest_text):
                    errors.append(f"{name}: missing anchor: {target}")

    if documents.get("CLAUDE.md", "").strip() != "@AGENTS.md":
        errors.append("CLAUDE.md must only contain @AGENTS.md")

    req_text = documents.get(REQ_FILE, "")
    reqs = re.findall(r"^\|\s*((?:FR-[A-Z]+|NFR)-\d+)\s*\|", req_text, re.M)
    acs = re.findall(r"^\|\s*(AC-\d+)\s*\|", documents.get(AC_FILE, ""), re.M)
    for label, ids in (("requirement", reqs), ("acceptance", acs)):
        if not ids:
            errors.append(f"no {label} definitions")
        for key, count in Counter(ids).items():
            if count > 1:
                errors.append(f"duplicate {label}: {key}")
    priorities = dict(re.findall(r"^\|\s*(FR-[A-Z]+-\d+)\s*\|\s*(P[012])\s*\|", req_text, re.M))
    trace = read_json("docs/delivery/traceability.json")
    if trace.get("schemaVersion") != 1:
        errors.append("traceability: expected schemaVersion 1")
    tracked = Counter()
    for item in trace.get("requirements", []):
        rid = item.get("id", "")
        tracked[rid] += 1
        if rid not in reqs:
            errors.append(f"unknown traced requirement: {rid}")
        if item.get("stage") not in {"S0", "S1", "S2", "S3", "S4"}:
            errors.append(f"{rid}: invalid stage")
        if priorities.get(rid) == "P0" and item.get("stage") != "S1":
            errors.append(f"{rid}: P0 must remain S1")
        if item.get("status") not in {"planned", "in-progress", "implemented", "verified", "deferred"}:
            errors.append(f"{rid}: invalid implementation status")
        if not item.get("acceptance") or not item.get("design"):
            errors.append(f"{rid}: missing acceptance/design mapping")
        for aid in item.get("acceptance", []):
            if aid not in acs:
                errors.append(f"{rid}: unknown acceptance: {aid}")
        for path in item.get("design", []):
            safe_file(path)
        if item.get("status") == "verified" and not item.get("evidence"):
            errors.append(f"{rid}: verified without evidence")
        for path in item.get("evidence", []):
            safe_file(path)
    for rid in set(reqs) - tracked.keys():
        errors.append(f"untracked requirement: {rid}")
    for rid, count in tracked.items():
        if count > 1:
            errors.append(f"duplicate traced requirement: {rid}")

    # Once a round exists, its required cases and P0 coverage become enforceable.
    case_file = root / "docs/delivery/r1-cases.json"
    if case_file.exists():
        cases = read_json("docs/delivery/r1-cases.json")
        if cases.get("schemaVersion") != 1 or not cases.get("planVersion"):
            errors.append("R1 cases: missing schema/plan version")
        covered, case_ids, packages = set(), Counter(), set()
        plan_text = documents.get("docs/delivery/r1-execution-plan.md", "")
        for case in cases.get("cases", []):
            cid = case.get("id", "")
            case_ids[cid] += 1
            for key in ("precondition", "steps", "expected", "evidence"):
                if not case.get(key):
                    errors.append(f"{cid}: missing case {key}")
            if case.get("mode") not in {"automated", "manual", "live"}:
                errors.append(f"{cid}: invalid case mode")
            for rid in case.get("requirements", []):
                if rid not in reqs:
                    errors.append(f"{cid}: unknown requirement: {rid}")
                covered.add(rid)
            for aid in case.get("acceptance", []):
                if aid not in acs:
                    errors.append(f"{cid}: unknown AC: {aid}")
            for package in case.get("work_packages", []):
                packages.add(package)
                if package not in plan_text:
                    errors.append(f"{cid}: unknown work package: {package}")
        for cid, count in case_ids.items():
            if not re.fullmatch(r"R1-[AML]-\d{2}", cid) or count != 1:
                errors.append(f"invalid or duplicate case ID: {cid}")
        for rid, priority in priorities.items():
            if priority == "P0" and rid not in covered:
                errors.append(f"R1 misses P0: {rid}")
        for rid in reqs:
            if rid.startswith("NFR-") and rid not in covered:
                errors.append(f"R1 misses quality requirement: {rid}")
        for package in set(re.findall(r"R1-W\d{2}", plan_text)) - packages:
            errors.append(f"R1 package has no case: {package}")
        if not case_ids:
            errors.append("R1 has no executable case definitions")
    return errors, len(documents), len(reqs)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    try:
        errors, count, req_count = check(args.root)
    except (OSError, ValueError, TypeError, AttributeError) as exc:
        print(f"FAIL: invalid documentation input: {exc}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(f"FAIL: {message}" for message in errors), file=sys.stderr)
        return 1
    print(f"PASS: {count} Markdown documents; {req_count} requirements; registry, links and traceability valid.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
