"""Offline regressions for scripts/quick_gate.py and scripts/_quiet.py."""

from __future__ import annotations

import io
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import patch

import _quiet
import quick_gate


def _write_workspace(root: Path) -> None:
    (root / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["crates/core-a", "crates/app-b", "crates/ui-c", "crates/cli-d"]\n',
        encoding="utf-8",
    )
    manifests = {
        "crates/core-a": '[package]\nname = "core-a"\n',
        "crates/app-b": '[package]\nname = "app-b"\n[dependencies]\ncore-a = { path = "../core-a" }\n',
        "crates/ui-c": '[package]\nname = "ui-c"\n[dependencies]\napp-b = { path = "../app-b" }\n',
        "crates/cli-d": (
            '[package]\nname = "cli-d"\n[dependencies]\n'
            'app-b = { path = "../app-b" }\nui-c = { path = "../ui-c" }\n'
        ),
    }
    for rel, content in manifests.items():
        crate_dir = root / rel
        crate_dir.mkdir(parents=True, exist_ok=True)
        (crate_dir / "Cargo.toml").write_text(content, encoding="utf-8")


class QuickGateTests(unittest.TestCase):
    def test_deleted_rust_file_still_kept_by_changed_files_and_scoped(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            _write_workspace(root)
            # Deleted file does NOT exist on disk; changed_files must keep it.
            with patch.object(
                quick_gate,
                "git_lines",
                side_effect=[["crates/cli-d/src/deleted.rs"], []],
            ):
                files = quick_gate.changed_files(root)
            self.assertEqual(files, ["crates/cli-d/src/deleted.rs"])
            self.assertEqual(quick_gate.crate_scope(files, False, root), ["-p", "cli-d"])

    def test_reverse_dependents_and_shared_files_expand_scope(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            _write_workspace(root)
            self.assertEqual(
                quick_gate.crate_scope(["crates/cli-d/src/main.rs"], False, root),
                ["-p", "cli-d"],
            )
            self.assertEqual(
                quick_gate.crate_scope(["crates/ui-c/src/lib.rs"], False, root),
                ["-p", "cli-d", "-p", "ui-c"],
            )
            self.assertEqual(
                quick_gate.crate_scope(["crates/app-b/src/lib.rs"], False, root),
                ["-p", "app-b", "-p", "cli-d", "-p", "ui-c"],
            )
            # Touching root dependency reaches every workspace crate -> collapses to --workspace.
            self.assertEqual(
                quick_gate.crate_scope(["crates/core-a/src/lib.rs"], False, root),
                ["--workspace"],
            )
            self.assertEqual(
                quick_gate.crate_scope(["tests/fixtures/sample.json"], False, root),
                ["--workspace"],
            )
            self.assertEqual(
                quick_gate.crate_scope(["docs/README.md"], False, root),
                [],
            )
            self.assertEqual(
                quick_gate.crate_scope(["docs/README.md"], True, root),
                ["--workspace"],
            )

    def test_scripts_changed_detection(self):
        self.assertTrue(quick_gate.scripts_changed(["scripts/deps.py"], False))
        self.assertTrue(quick_gate.scripts_changed(["scripts/deps-exceptions.json"], False))
        self.assertFalse(quick_gate.scripts_changed(["docs/README.md"], False))
        self.assertTrue(quick_gate.scripts_changed([], True))

    def test_unknown_cli_flag_rejected(self):
        err = io.StringIO()
        with redirect_stderr(err), self.assertRaises(SystemExit) as ctx:
            quick_gate.parse_args(["--all-crates"])
        self.assertNotEqual(ctx.exception.code, 0)

    def test_quiet_helper_applies_d25_defaults(self):
        captured: dict = {}

        def fake_run(cmd, **kwargs):
            captured["cmd"] = cmd
            captured["kwargs"] = kwargs
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

        with patch.object(_quiet.subprocess, "run", side_effect=fake_run):
            _quiet.run_quiet(["git", "status"], cwd=Path("."), capture=True, timeout=15)
        kwargs = captured["kwargs"]
        self.assertIs(kwargs["stdin"], subprocess.DEVNULL)
        self.assertEqual(kwargs["timeout"], 15)
        self.assertEqual(kwargs["encoding"], "utf-8")
        self.assertEqual(kwargs["env"]["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(kwargs["env"]["GCM_INTERACTIVE"], "never")
        if sys.platform == "win32":
            self.assertEqual(
                kwargs["creationflags"],
                _quiet.CREATE_NO_WINDOW | _quiet.BELOW_NORMAL_PRIORITY_CLASS,
            )
            # Interactive console with capture=False must omit CREATE_NO_WINDOW so live output shows.
            with patch.object(_quiet.sys.stdout, "isatty", return_value=True):
                self.assertEqual(
                    _quiet.quiet_creation_flags(capture=False),
                    _quiet.BELOW_NORMAL_PRIORITY_CLASS,
                )
                self.assertEqual(
                    _quiet.quiet_creation_flags(capture=True),
                    _quiet.CREATE_NO_WINDOW | _quiet.BELOW_NORMAL_PRIORITY_CLASS,
                )
            with (
                patch.object(_quiet.sys.stdout, "isatty", return_value=False),
                patch.object(_quiet.sys.stderr, "isatty", return_value=False),
            ):
                self.assertEqual(
                    _quiet.quiet_creation_flags(capture=False),
                    _quiet.CREATE_NO_WINDOW | _quiet.BELOW_NORMAL_PRIORITY_CLASS,
                )


if __name__ == "__main__":
    unittest.main()
