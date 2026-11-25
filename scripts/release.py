"""Weekly release planner and bundle helper for `.github/workflows/release.yml` (Issue #23).

Standard library only (Python 3.12+). Run from the repository root:

    python -B scripts/release.py --plan
    python -B scripts/release.py --bundle --version 0.1.2 --release-dir <dir> --out-dir dist/release [--previous-tag v0.1.1]

`--plan` derives the next `0.1.N` release version using the same tag rules as
`crates/delta-app/src/version.rs` and decides whether `HEAD` has commits since the latest
release tag. `--bundle` copies the signed `delta-cli release-build` outputs into a clean
artifact directory, verifies metadata against the embedded public key, and writes
`VERSION.txt`, `SHA256SUMS`, and `notes.md`. The private key is never read here.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASE_KEY_JSON = REPO_ROOT / "crates/delta-app/src/release-signing-key.json"
REPO_SLUG = "chialecode/delta"
RELEASE_TAG_RE = re.compile(r"^v0\.1\.(\d+)$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
HEX128_RE = re.compile(r"^[0-9a-f]{128}$")
MAX_NOTES_COMMITS = 200


class ReleaseError(RuntimeError):
    pass


def _git(root: Path, *args: str) -> str:
    env = dict(os.environ)
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GCM_INTERACTIVE"] = "never"
    result = subprocess.run(
        ["git", *args],
        cwd=root,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ReleaseError(f"git {' '.join(args)} failed (exit {result.returncode}): {detail}")
    return result.stdout


def parse_release_tags(tags: list[str]) -> list[int]:
    """Return sorted patch numbers N from `v0.1.N` tags.

    Matches `crates/delta-app/src/version.rs`: any tag starting with `v` followed by a digit
    must be a valid `v0.1.N`; other tags (e.g. stage markers) are ignored.
    """
    patches: list[int] = []
    for raw in tags:
        tag = raw.strip()
        if not tag.startswith("v") or len(tag) < 2 or not tag[1].isdigit():
            continue
        match = RELEASE_TAG_RE.fullmatch(tag)
        if match is None:
            raise ReleaseError(f"invalid release tag {tag!r}: release tags must be v0.1.N")
        patches.append(int(match.group(1)))
    patches.sort()
    return patches


def next_release_version(tags: list[str]) -> tuple[str, str]:
    """Return `(next_version, previous_tag)` from git tags."""
    patches = parse_release_tags(tags)
    if not patches:
        return "0.1.1", ""
    latest = patches[-1]
    next_patch = max(latest + 1, 1)
    return f"0.1.{next_patch}", f"v0.1.{latest}"


def compute_plan(root: Path = REPO_ROOT) -> dict[str, object]:
    """Determine whether `HEAD` has unreleased commits and what version to cut."""
    tags = _git(root, "tag", "--list").splitlines()
    version, previous_tag = next_release_version(tags)
    tag = f"v{version}"

    if previous_tag:
        count_raw = _git(root, "rev-list", "--count", f"{previous_tag}..HEAD").strip()
    else:
        count_raw = _git(root, "rev-list", "--count", "HEAD").strip()
    try:
        commit_count = int(count_raw)
    except ValueError as exc:
        raise ReleaseError(f"unexpected commit count: {count_raw!r}") from exc

    should_release = commit_count > 0
    if not should_release:
        reason = f"no commits since {previous_tag}" if previous_tag else "repository has no commits"
    elif previous_tag:
        reason = f"{commit_count} commit(s) since {previous_tag}"
    else:
        reason = f"initial release with {commit_count} commit(s)"

    return {
        "release": should_release,
        "version": version,
        "tag": tag,
        "previous_tag": previous_tag,
        "commit_count": commit_count,
        "reason": reason,
    }


def write_github_outputs(plan: dict[str, object], output_path: Path) -> None:
    lines = [
        f"release={'true' if plan['release'] else 'false'}",
        f"version={plan['version']}",
        f"tag={plan['tag']}",
        f"previous_tag={plan['previous_tag']}",
        f"commit_count={plan['commit_count']}",
        f"reason={plan['reason']}",
    ]
    with open(output_path, "a", encoding="utf-8", newline="\n") as handle:
        handle.write("\n".join(lines) + "\n")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        while chunk := handle.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def embedded_public_key_hex(root: Path = REPO_ROOT) -> str:
    key_json = root / "crates/delta-app/src/release-signing-key.json"
    data = json.loads(key_json.read_text(encoding="utf-8"))
    pub = str(data.get("public_key_hex", "")).strip()
    if not HEX64_RE.fullmatch(pub):
        raise ReleaseError("embedded release public_key_hex is missing or invalid")
    return pub


def render_release_notes(
    version: str,
    previous_tag: str,
    commits: list[str],
    installer_name: str,
    installer_sha256: str,
) -> str:
    tag = f"v{version}"
    lines = [
        f"# DELTA {version}",
        "",
        "Windows x64 per-user NSIS 安装包（不申请管理员权限；正式版自动更新经 Ed25519 签名校验）。",
        "",
    ]
    if previous_tag == "v0.1.1":
        lines.extend(
            [
                "## 从 v0.1.1 升级说明",
                "",
                "本版本起安装包改为 NSIS（D-39）。已安装 v0.1.1（Inno Setup）的用户请先在 Windows「设置 → 应用 → 已安装的应用」中卸载旧版 DELTA，再运行本版本安装程序；卸载只移除程序文件，不会删除资料库与用户数据。",
                "",
            ]
        )
    lines.extend(["## 变更摘要", ""])
    if commits:
        shown = commits[:MAX_NOTES_COMMITS]
        lines.extend(f"- {entry}" for entry in shown)
        if len(commits) > MAX_NOTES_COMMITS:
            lines.append(f"- （其余 {len(commits) - MAX_NOTES_COMMITS} 条提交见下方对比链接）")
    else:
        lines.append("- 本周例行发布。")
    if previous_tag:
        lines.extend(
            [
                "",
                f"完整变更：https://github.com/{REPO_SLUG}/compare/{previous_tag}...{tag}",
            ]
        )
    lines.extend(
        [
            "",
            "## 安装包校验",
            "",
            f"- 文件：`{installer_name}`",
            f"- SHA-256：`{installer_sha256}`",
            "- 说明：安装包未做 Authenticode 代码签名，Windows SmartScreen 可能提示「Windows 已保护你的电脑」，仅在核对官方发布页与 SHA-256 后选择「更多信息 → 仍要运行」。",
            "",
        ]
    )
    return "\n".join(lines)


def collect_commit_subjects(root: Path, previous_tag: str) -> list[str]:
    rev_range = f"{previous_tag}..HEAD" if previous_tag else "HEAD"
    raw = _git(root, "log", "--no-merges", "--format=%s (%h)", rev_range)
    return [line.strip() for line in raw.splitlines() if line.strip()]


def build_bundle(
    version: str,
    release_dir: Path,
    out_dir: Path,
    *,
    previous_tag: str = "",
    root: Path = REPO_ROOT,
) -> dict[str, object]:
    """Assemble and validate the signed release bundle for artifact upload and publishing."""
    if not re.fullmatch(r"0\.1\.[1-9]\d*", version):
        raise ReleaseError(f"invalid release version {version!r}: expected 0.1.N (N >= 1)")

    installer_name = f"delta-setup-{version}-x64.exe"
    sig_name = f"{installer_name}.sig"
    manifest_src = release_dir / "latest.json"
    installer_src = release_dir / installer_name
    sig_src = release_dir / sig_name
    pubkey_src = release_dir / "public-key.txt"

    for required in (manifest_src, installer_src, sig_src):
        if not required.is_file() or required.stat().st_size == 0:
            raise ReleaseError(f"missing or empty signed release file: {required.name}")

    manifest = json.loads(manifest_src.read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 1:
        raise ReleaseError(f"unexpected manifest schema_version: {manifest.get('schema_version')!r}")
    if manifest.get("channel") != "release":
        raise ReleaseError(f"unexpected manifest channel: {manifest.get('channel')!r}")
    if manifest.get("version") != version:
        raise ReleaseError(f"manifest version {manifest.get('version')!r} != {version!r}")
    if manifest.get("file") != installer_name:
        raise ReleaseError(f"manifest file {manifest.get('file')!r} != {installer_name!r}")

    installer_size = installer_src.stat().st_size
    installer_sha256 = sha256_file(installer_src)
    if manifest.get("size") != installer_size:
        raise ReleaseError(
            f"manifest size {manifest.get('size')!r} != actual size {installer_size}"
        )
    if manifest.get("sha256") != installer_sha256:
        raise ReleaseError("manifest sha256 does not match installer file")

    signature = sig_src.read_text(encoding="utf-8").strip()
    if not HEX128_RE.fullmatch(signature):
        raise ReleaseError(f"invalid Ed25519 signature format in {sig_name}")

    embedded_pub = embedded_public_key_hex(root)
    if pubkey_src.is_file():
        actual_pub = pubkey_src.read_text(encoding="utf-8").strip()
        if actual_pub != embedded_pub:
            raise ReleaseError("release public-key.txt does not match embedded release-signing-key.json")

    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    shutil.copy2(installer_src, out_dir / installer_name)
    (out_dir / sig_name).write_bytes((signature + "\n").encode("utf-8"))
    shutil.copy2(manifest_src, out_dir / "latest.json")

    checksum_files = (installer_name, sig_name, "latest.json")
    sums_lines = [f"{sha256_file(out_dir / name)}  {name}" for name in checksum_files]
    (out_dir / "SHA256SUMS").write_bytes(("\n".join(sums_lines) + "\n").encode("utf-8"))
    (out_dir / "VERSION.txt").write_bytes(
        f"version={version}\nchannel=release\n".encode("utf-8")
    )

    commits = collect_commit_subjects(root, previous_tag)
    notes = render_release_notes(
        version, previous_tag, commits, installer_name, installer_sha256
    )
    (out_dir / "notes.md").write_bytes(notes.encode("utf-8"))

    return {
        "status": "bundled",
        "version": version,
        "channel": "release",
        "installer": installer_name,
        "size": installer_size,
        "sha256": installer_sha256,
        "files": [
            installer_name,
            sig_name,
            "latest.json",
            "SHA256SUMS",
            "VERSION.txt",
            "notes.md",
        ],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--plan", action="store_true", help="compute weekly release plan from git tags")
    mode.add_argument("--bundle", action="store_true", help="assemble and validate signed release bundle")
    parser.add_argument("--version", help="release version 0.1.N (for --bundle)")
    parser.add_argument("--previous-tag", default="", help="previous release tag (for --bundle notes)")
    parser.add_argument("--release-dir", type=Path, help="output directory from delta-cli release-build")
    parser.add_argument("--out-dir", type=Path, help="target bundle directory (for --bundle)")
    args = parser.parse_args(argv)

    try:
        if args.plan:
            plan = compute_plan(REPO_ROOT)
            gh_out = os.environ.get("GITHUB_OUTPUT", "").strip()
            if gh_out:
                write_github_outputs(plan, Path(gh_out))
            print(json.dumps(plan, ensure_ascii=False))
            return 0
        if not args.version or not args.release_dir or not args.out_dir:
            raise ReleaseError("--bundle requires --version, --release-dir and --out-dir")
        result = build_bundle(
            args.version,
            args.release_dir,
            args.out_dir,
            previous_tag=args.previous_tag.strip(),
            root=REPO_ROOT,
        )
        print(json.dumps(result, ensure_ascii=False))
        return 0
    except (ReleaseError, OSError, ValueError) as exc:
        print(f"release: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
