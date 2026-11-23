"""Offline regressions for scripts/check_docs.py."""

from __future__ import annotations

import json
import tempfile
import unittest
import unittest.mock
from pathlib import Path

import check_docs


def _build_minimal_repo(root: Path, extra_docs: dict[str, str] | None = None) -> None:
    docs: dict[str, str] = {
        "AGENTS.md": "# Entry\n",
        "CLAUDE.md": "@AGENTS.md\n",
        "docs/requirements/product-requirements.md": (
            "# Requirements\n\n| ID | Priority |\n| --- | --- |\n| FR-CORE-01 | P0 |\n"
        ),
        "docs/engineering/delivery-plan.md": (
            "# Plan\n\n| AC | Description |\n| --- | --- |\n| AC-01 | Check |\n"
        ),
        "docs/design/design.md": "# Design\n\nRef FR-CORE-01 and AC-01.\n",
        "docs/delivery/stage-plan.md": "# Stage Plan\n\nWork package M1-W01.\n",
    }
    if extra_docs:
        docs.update(extra_docs)

    registry_docs = []
    for rel, content in docs.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        kind = "entry"
        if rel.endswith("product-requirements.md"):
            kind = "requirements"
        elif rel.endswith("delivery-plan.md") or rel.endswith("stage-plan.md"):
            kind = "plan"
        elif rel.startswith("docs/design/"):
            kind = "design"
        registry_docs.append(
            {
                "path": rel,
                "kind": kind,
                "status": "active",
                "owner": "team",
                "scope": "repo",
                "reviewed": "2026-11-21",
                "basis": "test",
            }
        )

    gov = root / "docs" / "governance"
    gov.mkdir(parents=True, exist_ok=True)
    (gov / "document-registry.json").write_text(
        json.dumps({"schemaVersion": 1, "currentStage": "M1", "documents": registry_docs}),
        encoding="utf-8",
    )
    deliv = root / "docs" / "delivery"
    deliv.mkdir(parents=True, exist_ok=True)
    (deliv / "traceability.json").write_text(
        json.dumps(
            {
                "schemaVersion": 1,
                "requirements": [
                    {
                        "id": "FR-CORE-01",
                        "stage": "M2",
                        "status": "planned",
                        "acceptance": ["AC-01"],
                        "design": ["docs/design/design.md"],
                    }
                ],
            }
        ),
        encoding="utf-8",
    )


class CheckDocsTests(unittest.TestCase):
    def test_prose_closing_fence_requires_at_most_three_leading_spaces(self):
        sample = (
            "Before\n"
            "```md\n"
            "    ```\n"
            "still inside [ref][link]\n"
            "```\n"
            "After\n"
        )
        stripped = check_docs.prose(sample)
        self.assertIn("Before", stripped)
        self.assertIn("After", stripped)
        self.assertNotIn("still inside", stripped)

    def test_md_drive_path_reported_exactly_once(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            fake_drive = "C" + ":\\" + "secret_path\\doc.txt"
            _build_minimal_repo(root, {"docs/design/design.md": f"# Design\n\n{fake_drive}\n"})
            errors, _, _ = check_docs.check(root)
            hits = [e for e in errors if "machine-specific absolute path" in e]
            self.assertEqual(hits, ["docs/design/design.md: machine-specific absolute path"])

    def test_non_utf8_file_still_scanned_for_secrets(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = Path(tmp)
            _build_minimal_repo(root)
            fake_token = "ghp" + "_" + "A" * 24
            (root / "notes.txt").write_bytes(b"\xff\xfe bad bytes " + fake_token.encode("ascii"))
            errors, _, _ = check_docs.check(root)
            self.assertTrue(any("notes.txt: invalid UTF-8 encoding" in e for e in errors), errors)
            self.assertTrue(any("notes.txt: forbidden GitHub token" in e for e in errors), errors)

    def test_escaping_non_md_symlink_rejected(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            base = Path(tmp)
            root = base / "repo"
            root.mkdir()
            _build_minimal_repo(root)
            outside = base / "outside.txt"
            outside.write_text("outside", encoding="utf-8")
            link = root / "escape.txt"
            try:
                link.symlink_to(outside)
                errors, _, _ = check_docs.check(root)
            except OSError:
                # Non-elevated Windows hosts without Developer Mode cannot create symlinks:
                # simulate an escaping symlink via Path.resolve on the target file.
                link.write_text("placeholder", encoding="utf-8")
                orig_resolve = Path.resolve

                def fake_resolve(self, strict=False):
                    if self.name == "escape.txt":
                        return orig_resolve(outside, strict=strict)
                    return orig_resolve(self, strict=strict)

                with unittest.mock.patch.object(Path, "resolve", fake_resolve):
                    errors, _, _ = check_docs.check(root)
            self.assertTrue(
                any("escape.txt: symlink escapes repository" in e for e in errors), errors
            )


if __name__ == "__main__":
    unittest.main()
