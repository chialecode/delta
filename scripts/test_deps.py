"""Offline regressions for the dependency inventory and OSV vulnerability audit."""

from __future__ import annotations

import http.client
import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import date
from pathlib import Path
from unittest.mock import patch

import deps


class VersionRuleTests(unittest.TestCase):
    def test_stable_versions_only(self):
        self.assertEqual(deps.stable("1.43.0"), (1, 43, 0))
        for version in ("1.0.0-rc.1", "1.0", "1.0.0+build"):
            self.assertIsNone(deps.stable(version))

    def test_caret_compatibility(self):
        self.assertTrue(deps.compatible((1, 2, 0), (1, 9, 3)))
        self.assertFalse(deps.compatible((1, 2, 0), (2, 0, 0)))
        self.assertTrue(deps.compatible((0, 37, 1), (0, 37, 5)))
        self.assertFalse(deps.compatible((0, 37, 5), (0, 41, 0)))
        self.assertFalse(deps.compatible((0, 0, 3), (0, 0, 4)))

    def test_sparse_index_paths(self):
        self.assertEqual(deps.index_path("a"), "1/a")
        self.assertEqual(deps.index_path("cc"), "2/cc")
        self.assertEqual(deps.index_path("syn"), "3/s/syn")
        self.assertEqual(deps.index_path("Serde"), "se/rd/serde")

    def test_parse_review_by_strict_iso_date(self):
        self.assertEqual(deps.parse_review_by("2026-12-01"), date(2026, 12, 1))
        for bad in (None, "", "never", "TBD", "2026-13-01", "2026-1-1", 20261201):
            with self.subTest(raw=bad):
                self.assertIsNone(deps.parse_review_by(bad))


class AuditTests(unittest.TestCase):
    def _make_repo(self, base: Path, packages: list[tuple[str, str]], exceptions: list[dict]) -> Path:
        (base / "scripts").mkdir(parents=True, exist_ok=True)
        (base / "scripts" / "deps-exceptions.json").write_text(
            json.dumps({"exceptions": exceptions}), encoding="utf-8"
        )
        lock_blocks = [
            f'[[package]]\nname = "{name}"\nversion = "{ver}"\nsource = "{deps.CRATES_IO}"\n'
            for name, ver in packages
        ]
        (base / "Cargo.lock").write_text("\n".join(lock_blocks), encoding="utf-8")
        return base

    def test_osv_batches_are_chunked_under_api_limit(self):
        packages = [(f"pkg-{i:04d}", "1.0.0") for i in range(1120)]
        seen_sizes: list[int] = []

        def fake_fetch_json(url: str, body: bytes | None = None):
            payload = json.loads(body.decode("utf-8"))
            seen_sizes.append(len(payload["queries"]))
            return {"results": [{} for _ in payload["queries"]]}

        with patch.object(deps, "fetch_json", side_effect=fake_fetch_json):
            results = deps.query_osv_batches(packages)
        self.assertEqual(seen_sizes, [500, 500, 120])
        self.assertEqual(len(results), 1120)

    def test_audit_reports_both_vulnerable_versions_and_dedups_aliases_per_version(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = self._make_repo(
                Path(tmp),
                [("foo", "0.1.0"), ("foo", "0.2.0")],
                [],
            )
            batch = [
                {"vulns": [{"id": "RUSTSEC-2026-0001"}, {"id": "GHSA-aaaa-bbbb-cccc"}]},
                {"vulns": [{"id": "RUSTSEC-2026-0001"}]},
            ]
            advisory = {
                "id": "RUSTSEC-2026-0001",
                "aliases": ["GHSA-aaaa-bbbb-cccc"],
                "summary": "example flaw",
                "affected": [{"ranges": [{"events": [{"fixed": "0.3.0"}]}]}],
            }
            out, err = io.StringIO(), io.StringIO()
            with (
                patch.object(deps, "query_osv_batches", return_value=batch),
                patch.object(deps, "fetch_json", return_value=advisory),
                redirect_stdout(out),
                redirect_stderr(err),
            ):
                code = deps.audit(root=root, today=date(2026, 11, 21))
            self.assertEqual(code, 1)
            report = json.loads(out.getvalue().strip().splitlines()[-1])
            self.assertEqual(report["advisories"], 2)
            self.assertEqual(report["open"], 2)
            self.assertIn("foo@0.1.0", err.getvalue())
            self.assertIn("foo@0.2.0", err.getvalue())

    def test_audit_exception_requires_matching_package_and_valid_unexpired_review_by(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = self._make_repo(
                Path(tmp),
                [("crate-a", "1.0.0"), ("crate-b", "1.0.0")],
                [
                    {
                        "advisory": "RUSTSEC-2026-0002",
                        "package": "crate-a",
                        "reason": "waived for a",
                        "reviewBy": "2026-12-01",
                    },
                    {
                        "advisory": "RUSTSEC-2026-9999",
                        "package": "crate-z",
                        "reason": "malformed date",
                        "reviewBy": "never",
                    },
                ],
            )
            batch = [
                {"vulns": [{"id": "RUSTSEC-2026-0002"}]},
                {"vulns": [{"id": "RUSTSEC-2026-0002"}]},
            ]
            advisory = {"id": "RUSTSEC-2026-0002", "summary": "shared flaw", "affected": []}
            out, err = io.StringIO(), io.StringIO()
            with (
                patch.object(deps, "query_osv_batches", return_value=batch),
                patch.object(deps, "fetch_json", return_value=advisory),
                redirect_stdout(out),
                redirect_stderr(err),
            ):
                code = deps.audit(root=root, today=date(2026, 11, 21))
            self.assertEqual(code, 1)
            report = json.loads(out.getvalue().strip().splitlines()[-1])
            # crate-a matched its exception; crate-b did NOT; malformed reviewBy counted as expired.
            self.assertEqual(report["open"], 1)
            self.assertEqual(report["expiredExceptions"], 1)
            self.assertIn("crate-b@1.0.0", err.getvalue())
            self.assertNotIn("crate-a@1.0.0", err.getvalue())

    def test_http_exception_and_invalid_json_raise_not_run(self):
        with patch.object(
            deps.urllib.request, "urlopen", side_effect=http.client.IncompleteRead(b"partial")
        ):
            with self.assertRaises(deps.NotRun):
                deps.fetch("https://example.invalid")
        with patch.object(deps, "fetch", return_value="<html>502</html>"):
            with self.assertRaises(deps.NotRun):
                deps.fetch_json("https://example.invalid")

    def test_osv_null_and_non_list_fields_handled_or_raise_not_run(self):
        with tempfile.TemporaryDirectory(prefix="delta-script-test-") as tmp:
            root = self._make_repo(Path(tmp), [("foo", "1.0.0")], [])
            out = io.StringIO()
            with (
                patch.object(deps, "query_osv_batches", return_value=[{"vulns": None}]),
                redirect_stdout(out),
            ):
                self.assertEqual(deps.audit(root=root, today=date(2026, 11, 21)), 0)

            with patch.object(deps, "query_osv_batches", return_value=[{"vulns": "bad"}]):
                with self.assertRaises(deps.NotRun):
                    deps.audit(root=root, today=date(2026, 11, 21))

            advisory = {"id": "RUSTSEC-2026-0005", "aliases": None, "affected": None}
            out, err = io.StringIO(), io.StringIO()
            with (
                patch.object(
                    deps,
                    "query_osv_batches",
                    return_value=[{"vulns": [{"id": "RUSTSEC-2026-0005"}]}],
                ),
                patch.object(deps, "fetch_json", return_value=advisory),
                redirect_stdout(out),
                redirect_stderr(err),
            ):
                self.assertEqual(deps.audit(root=root, today=date(2026, 11, 21)), 1)

    def test_unknown_cli_flag_exits_1_not_0_or_2(self):
        err = io.StringIO()
        with redirect_stderr(err), self.assertRaises(SystemExit) as ctx:
            deps.parse_args(["--audti"])
        self.assertEqual(ctx.exception.code, 1)


if __name__ == "__main__":
    unittest.main()
