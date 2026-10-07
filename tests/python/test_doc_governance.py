"""Negative documentation fixtures stay in temporary directories, never in docs."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("check_docs", ROOT / "scripts/check_docs.py")
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class DocumentationGovernanceTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # Only public documentation and its linked scripts/config/fixtures.
        for directory in ("docs", "scripts", "tests/fixtures", ".github"):
            shutil.copytree(ROOT / directory, self.root / directory,
                            ignore=shutil.ignore_patterns("__pycache__"))
        for path in ROOT.glob("*.md"):
            shutil.copy2(path, self.root / path.name)
        self.assertEqual(CHECKER.check(self.root)[0], [])

    def test_expired_stage_documents_fail(self):
        path = self.root / "docs/governance/document-registry.json"
        data = json.loads(path.read_text(encoding="utf-8"))
        data["currentStage"] = "R3"
        path.write_text(json.dumps(data), encoding="utf-8")
        errors = CHECKER.check(self.root)[0]
        self.assertTrue(any("retireAfter" in error for error in errors), errors)

    def test_local_path_in_code_fence_fails(self):
        path = self.root / "README.md"
        with path.open("a", encoding="utf-8") as stream:
            stream.write("\n```text\nX:/private/reference/image.png\n```\n")
        errors = CHECKER.check(self.root)[0]
        self.assertIn("README.md: machine-specific absolute path", errors)

    def test_missing_user_actions_and_broken_link_fail(self):
        (self.root / "USER-ACTIONS.md").unlink()
        errors = CHECKER.check(self.root)[0]
        self.assertTrue(any("missing file: USER-ACTIONS.md" in error for error in errors), errors)
        self.assertTrue(any("broken or escaping link" in error for error in errors), errors)
