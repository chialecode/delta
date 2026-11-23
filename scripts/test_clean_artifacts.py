"""Offline regressions for scripts/clean_artifacts.py."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import clean_artifacts


class CleanArtifactsTests(unittest.TestCase):
    def test_nested_scratch_under_old_logs_is_pruned_and_removed_cleanly(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            stage = root / ".local" / "reports" / "m3"
            old_logs = stage / "logs-100-1000"
            nested_scratch = old_logs / "licenses-11111111-2222-3333-4444-555555555555"
            new_logs = stage / "logs-200-2000"
            nested_scratch.mkdir(parents=True)
            new_logs.mkdir(parents=True)
            (nested_scratch / "a.txt").write_text("scratch", encoding="utf-8")
            (new_logs / "keep.txt").write_text("keep", encoding="utf-8")
            os.utime(old_logs, (1_700_000_000, 1_700_000_000))
            os.utime(new_logs, (1_800_000_000, 1_800_000_000))

            removals, _ = clean_artifacts.collect(False, root=root, cited="")
            paths = [p for _, p in removals]
            self.assertEqual(paths, [old_logs])
            for _, p in removals:
                clean_artifacts.remove_path(p, root=root)
            self.assertFalse(old_logs.exists())
            self.assertTrue(new_logs.is_dir())

    def test_dist_pruning_keeps_highest_semver_even_if_older_version_has_newer_mtime(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            dist = root / "dist"
            dist.mkdir()
            older_ver = dist / "delta-dev-setup-0.1.1-dev.20261109010101-x64.exe"
            older_sidecar = older_ver.with_suffix(".package.json")
            newer_ver = dist / "delta-dev-setup-0.1.1-dev.20261109020202-x64.exe"
            newer_sidecar = newer_ver.with_suffix(".package.json")
            for p in (older_ver, older_sidecar, newer_ver, newer_sidecar):
                p.write_bytes(b"x")
            # Touch older_ver to a newer mtime than newer_ver.
            os.utime(newer_ver, (1_700_000_000, 1_700_000_000))
            os.utime(older_ver, (1_900_000_000, 1_900_000_000))

            removals, _ = clean_artifacts.collect(False, root=root, cited="")
            removed_names = sorted(p.name for _, p in removals)
            self.assertEqual(removed_names, [older_ver.name, older_sidecar.name])

    def test_refuses_outside_root_and_links(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            base = Path(tmp)
            root = base / "repo"
            outside = base / "outside"
            root.mkdir()
            outside.mkdir()
            victim = outside / "keep.txt"
            victim.write_text("important", encoding="utf-8")

            self.assertFalse(clean_artifacts.is_safe_target(victim, root=root))
            self.assertFalse(clean_artifacts.is_safe_target(root, root=root))
            with self.assertRaises(RuntimeError):
                clean_artifacts.remove_path(victim, root=root)
            self.assertTrue(victim.is_file())

            # Fake link/junction detection must also block removal.
            local_tmp = root / ".local" / "tmp" / "linked"
            local_tmp.parent.mkdir(parents=True)
            local_tmp.mkdir()
            with patch.object(clean_artifacts, "is_link_or_junction", side_effect=lambda p: p == local_tmp):
                self.assertFalse(clean_artifacts.is_safe_target(local_tmp, root=root))
                with self.assertRaises(RuntimeError):
                    clean_artifacts.remove_path(local_tmp, root=root)

    def test_tracked_text_handles_filenames_with_spaces(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            doc = root / "docs" / "my note.md"
            doc.parent.mkdir(parents=True)
            doc.write_text("cited logs-123-456", encoding="utf-8")
            with patch.object(clean_artifacts, "git_lines", return_value=["docs/my note.md"]):
                text = clean_artifacts.tracked_text(root=root)
            self.assertIn("logs-123-456", text)


if __name__ == "__main__":
    unittest.main()
