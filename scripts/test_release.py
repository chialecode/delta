"""Offline unit tests for scripts/release.py and .github/workflows/release.yml contracts."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

import release

REPO_ROOT = Path(__file__).resolve().parent.parent


def _git(cwd: Path, *args: str) -> str:
    env = dict(os.environ)
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GCM_INTERACTIVE"] = "never"
    env["GIT_AUTHOR_NAME"] = "Test"
    env["GIT_AUTHOR_EMAIL"] = "test@example.com"
    env["GIT_COMMITTER_NAME"] = "Test"
    env["GIT_COMMITTER_EMAIL"] = "test@example.com"
    proc = subprocess.run(
        ["git", *args],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
    )
    return proc.stdout


class ReleasePlannerTests(unittest.TestCase):
    def test_next_release_version_matches_rust_tag_rules(self):
        self.assertEqual(release.next_release_version([]), ("0.1.1", ""))
        self.assertEqual(release.next_release_version(["m2-merged", "v0.1.0"]), ("0.1.1", "v0.1.0"))
        self.assertEqual(
            release.next_release_version(["v0.1.1", "v0.1.9", "v0.1.10", "stage-m5"]),
            ("0.1.11", "v0.1.10"),
        )
        for bad in ("v0.1", "v1.0.0", "v0.2.0", "v0.1.2-rc.1", "v0.1.x"):
            with self.subTest(tag=bad), self.assertRaises(release.ReleaseError):
                release.next_release_version([bad])

    def test_compute_plan_detects_commits_since_last_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _git(repo, "init")
            (repo / "README.md").write_text("hello\n", encoding="utf-8")
            _git(repo, "add", "README.md")
            _git(repo, "commit", "-m", "feat: initial")

            initial = release.compute_plan(repo)
            self.assertTrue(initial["release"])
            self.assertEqual(initial["version"], "0.1.1")
            self.assertEqual(initial["tag"], "v0.1.1")
            self.assertEqual(initial["previous_tag"], "")
            self.assertEqual(initial["commit_count"], 1)

            _git(repo, "tag", "v0.1.1")
            no_changes = release.compute_plan(repo)
            self.assertFalse(no_changes["release"])
            self.assertEqual(no_changes["version"], "0.1.2")
            self.assertEqual(no_changes["previous_tag"], "v0.1.1")
            self.assertEqual(no_changes["commit_count"], 0)

            (repo / "README.md").write_text("hello 2\n", encoding="utf-8")
            _git(repo, "add", "README.md")
            _git(repo, "commit", "-m", "fix: second change")
            with_changes = release.compute_plan(repo)
            self.assertTrue(with_changes["release"])
            self.assertEqual(with_changes["version"], "0.1.2")
            self.assertEqual(with_changes["tag"], "v0.1.2")
            self.assertEqual(with_changes["previous_tag"], "v0.1.1")
            self.assertEqual(with_changes["commit_count"], 1)

            gh_out = repo / "github_output.txt"
            release.write_github_outputs(with_changes, gh_out)
            raw = gh_out.read_bytes()
            self.assertNotIn(b"\r\n", raw)
            text = raw.decode("utf-8")
            self.assertIn("release=true\n", text)
            self.assertIn("version=0.1.2\n", text)
            self.assertIn("previous_tag=v0.1.1\n", text)


class ReleaseBundleTests(unittest.TestCase):
    def _make_fake_release(self, root: Path, version: str, body: bytes) -> tuple[Path, str]:
        rel_dir = root / f"delta-{version}"
        rel_dir.mkdir(parents=True, exist_ok=True)
        installer_name = f"delta-setup-{version}-x64.exe"
        (rel_dir / installer_name).write_bytes(body)
        sha = hashlib.sha256(body).hexdigest()
        manifest = {
            "schema_version": 1,
            "channel": "release",
            "version": version,
            "file": installer_name,
            "size": len(body),
            "sha256": sha,
            "published_at": "2026-11-25T00:00:00Z",
        }
        (rel_dir / "latest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
        (rel_dir / f"{installer_name}.sig").write_text("a" * 128, encoding="utf-8")
        (rel_dir / "public-key.txt").write_text(
            release.embedded_public_key_hex(REPO_ROOT), encoding="utf-8"
        )
        return rel_dir, sha

    def test_bundle_validates_writes_lf_checksums_and_d39_migration_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fake_repo = tmp_path / "repo"
            fake_repo.mkdir()
            _git(fake_repo, "init")
            key_rel = Path("crates/delta-app/src/release-signing-key.json")
            (fake_repo / key_rel).parent.mkdir(parents=True, exist_ok=True)
            (fake_repo / key_rel).write_bytes((REPO_ROOT / key_rel).read_bytes())
            _git(fake_repo, "add", ".")
            _git(fake_repo, "commit", "-m", "feat: v0.1.1 baseline")
            _git(fake_repo, "tag", "v0.1.1")
            (fake_repo / "CHANGELOG.txt").write_text("v0.1.2 change\n", encoding="utf-8")
            _git(fake_repo, "add", "CHANGELOG.txt")
            _git(fake_repo, "commit", "-m", "feat(installer): switch to NSIS")

            rel_dir, sha = self._make_fake_release(tmp_path, "0.1.2", b"fake-nsis-installer")
            out_dir = tmp_path / "dist-release"
            result = release.build_bundle(
                "0.1.2",
                rel_dir,
                out_dir,
                previous_tag="v0.1.1",
                root=fake_repo,
            )
            self.assertEqual(result["status"], "bundled")
            self.assertEqual(result["sha256"], sha)
            for name in ("VERSION.txt", "SHA256SUMS", "notes.md", "delta-setup-0.1.2-x64.exe.sig"):
                self.assertNotIn(b"\r\n", (out_dir / name).read_bytes(), f"{name} must use LF")
            self.assertEqual(
                (out_dir / "VERSION.txt").read_text(encoding="utf-8"),
                "version=0.1.2\nchannel=release\n",
            )
            notes = (out_dir / "notes.md").read_text(encoding="utf-8")
            self.assertIn("从 v0.1.1 升级说明", notes)
            self.assertIn("feat(installer): switch to NSIS", notes)
            self.assertIn(sha, notes)
            sums = (out_dir / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(sums), 3)
            for line in sums:
                expected_hash, filename = line.split("  ", 1)
                self.assertEqual(release.sha256_file(out_dir / filename), expected_hash)

    def test_bundle_rejects_tampered_hash_wrong_channel_and_bad_pubkey(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            rel_dir, _ = self._make_fake_release(tmp_path, "0.1.2", b"installer-v1")
            # Tamper installer bytes after manifest was written.
            (rel_dir / "delta-setup-0.1.2-x64.exe").write_bytes(b"installer-v2")
            with self.assertRaisesRegex(release.ReleaseError, "sha256"):
                release.build_bundle("0.1.2", rel_dir, tmp_path / "out1", root=REPO_ROOT)

            rel_dir2, _ = self._make_fake_release(tmp_path / "c2", "0.1.2", b"installer")
            manifest = json.loads((rel_dir2 / "latest.json").read_text(encoding="utf-8"))
            manifest["channel"] = "test"
            (rel_dir2 / "latest.json").write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaisesRegex(release.ReleaseError, "channel"):
                release.build_bundle("0.1.2", rel_dir2, tmp_path / "out2", root=REPO_ROOT)

            rel_dir3, _ = self._make_fake_release(tmp_path / "c3", "0.1.2", b"installer")
            (rel_dir3 / "public-key.txt").write_text("0" * 64, encoding="utf-8")
            with self.assertRaisesRegex(release.ReleaseError, "public-key.txt"):
                release.build_bundle("0.1.2", rel_dir3, tmp_path / "out3", root=REPO_ROOT)


class ReleaseWorkflowContractTests(unittest.TestCase):
    def test_release_workflow_enforces_security_and_policy_boundaries(self):
        wf_path = REPO_ROOT / ".github/workflows/release.yml"
        self.assertTrue(wf_path.is_file(), "missing .github/workflows/release.yml")
        text = wf_path.read_text(encoding="utf-8")
        self.assertNotIn("\r\n", text, "release.yml must use LF line endings")

        # Never triggered by push or pull_request.
        on_section = text.split("\npermissions:", 1)[0]
        self.assertIn("schedule:", on_section)
        self.assertIn("workflow_dispatch:", on_section)
        self.assertNotRegex(on_section, r"(?m)^\s*(push|pull_request|pull_request_target):")
        self.assertIn("\npermissions: {}\n", text)

        # Every `uses:` action must be pinned to a full 40-char SHA and listed in
        # `.github/actions-policy.json` (plus the two artifact actions pending user-authorized
        # remote allowlist sync under #23; tighten once `.github/actions-policy.json` is synced).
        policy = json.loads((REPO_ROOT / ".github/actions-policy.json").read_text(encoding="utf-8"))
        pending_release_actions = {
            "actions/upload-artifact@*",
            "actions/download-artifact@*",
        }
        allowed = set(policy["selectedActions"]["patterns_allowed"]) | pending_release_actions
        uses = re.findall(r"uses:\s*([^\s#]+)", text)
        self.assertGreaterEqual(len(uses), 4)
        for ref in uses:
            match = re.fullmatch(r"([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)@([0-9a-f]{40})", ref)
            self.assertIsNotNone(match, f"action not pinned to 40-char SHA: {ref}")
            action_name = match.group(1)
            self.assertIn(
                f"{action_name}@*",
                allowed,
                f"action {action_name} is not in .github/actions-policy.json or #23 pending list",
            )

        # Split job blocks by 2-space job headers.
        jobs_text = text.split("\njobs:\n", 1)[1]
        job_blocks: dict[str, str] = {}
        parts = re.split(r"(?m)^  ([a-z0-9_-]+):\n", jobs_text)
        for idx in range(1, len(parts), 2):
            job_blocks[parts[idx]] = parts[idx + 1]
        self.assertEqual(set(job_blocks.keys()), {"plan", "check", "build", "publish"})

        # Secret UPDATE_SIGNING_KEY_PEM must appear ONLY inside the signing step of `build`.
        self.assertNotIn("UPDATE_SIGNING_KEY_PEM", job_blocks["plan"])
        self.assertNotIn("UPDATE_SIGNING_KEY_PEM", job_blocks["check"])
        self.assertNotIn("UPDATE_SIGNING_KEY_PEM", job_blocks["publish"])
        self.assertIn("environment: release", job_blocks["build"])
        self.assertIn("secrets.UPDATE_SIGNING_KEY_PEM", job_blocks["build"])
        # Must only reference `secrets.UPDATE_SIGNING_KEY_PEM` once (in the signing step).
        self.assertEqual(job_blocks["build"].count("secrets.UPDATE_SIGNING_KEY_PEM"), 1)
        # Ensure secret is not in build's job-level env (appears only after `steps:`).
        before_steps = job_blocks["build"].split("    steps:\n", 1)[0]
        self.assertNotIn("UPDATE_SIGNING_KEY_PEM", before_steps)

        # Publish job: gated on main + RELEASE_PUBLISH == '1', runs NO repo checkout or repo code.
        pub = job_blocks["publish"]
        self.assertIn("vars.RELEASE_PUBLISH == '1'", pub)
        self.assertIn("github.ref == 'refs/heads/main'", pub)
        self.assertIn("contents: write", pub)
        self.assertNotIn("actions/checkout", pub)
        self.assertNotIn("scripts/", pub)
        self.assertNotIn("cargo ", pub)
        self.assertNotIn("delta-cli", pub)


if __name__ == "__main__":
    unittest.main()
