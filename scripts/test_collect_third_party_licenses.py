"""Offline regressions for license selection, obligation coverage and cache identity."""

import concurrent.futures
import hashlib
import http.client
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import collect_third_party_licenses as licenses


MIT = b"""Copyright (c) Fixture authors.
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files, to deal in the Software.
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software. THE SOFTWARE IS PROVIDED AS IS.
"""
ISC = b"""Copyright (c) Fixture authors.
Permission to use, copy, modify, and/or distribute this software for any purpose
with or without fee is hereby granted, provided that the above copyright notice
and this permission notice appear in all copies. THE SOFTWARE IS PROVIDED AS IS.
"""
APACHE = b"Apache License Version 2.0. Definitions.\n" + b"Standard fixture license.\n" * 20


class LicenseAuditTests(unittest.TestCase):
    def test_exact_identifiers_and_mandatory_conjunctions(self):
        expected = {
            "CC0-1.0 OR MIT-0 OR Apache-2.0": ("Apache-2.0",),
            "(Apache-2.0 OR MIT) AND BSD-3-Clause": ("MIT", "BSD-3-Clause"),
            "(MIT OR Apache-2.0) AND NCSA": ("MIT", "NCSA"),
            "Apache-2.0 AND ISC": ("Apache-2.0", "ISC"),
            "(MIT OR Apache-2.0) AND Unicode-3.0": ("MIT", "Unicode-3.0"),
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT": ("MIT",),
            "MIT / Apache-2.0": ("MIT",),
            "MIT-0": ("MIT-0",),
        }
        for expression, chosen in expected.items():
            with self.subTest(expression=expression):
                self.assertEqual(licenses.selected_licenses(expression), chosen)
        for expression in ("MIT AND GPL-3.0-only", "MIT AND", "(MIT OR ISC", "MIT$", "MIT OR )"):
            with self.subTest(expression=expression), self.assertRaises(ValueError):
                licenses.selected_licenses(expression)

    def test_notice_alone_does_not_satisfy_mandatory_license(self):
        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            base = Path(directory)
            source = base / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("[package]\nname='fixture'\n", encoding="utf-8")
            (source / "LICENSE").write_bytes(APACHE)
            (source / "NOTICE").write_text("Copyright Fixture", encoding="utf-8")
            package = {"id": "fixture-id", "name": "fixture", "version": "1.0.0",
                       "license": "Apache-2.0 AND ISC", "manifest_path": str(source / "Cargo.toml")}
            output = base / "output"
            output.mkdir()
            with patch.object(licenses, "CACHE", base / "cache"), patch.object(licenses, "fetch", return_value=ISC):
                result = licenses.collect(package, output)
            self.assertEqual(result["selected_obligations"], ["Apache-2.0", "ISC"])
            self.assertTrue(result["obligation_files"]["ISC"])
            self.assertTrue(result["standard_text_with_source_fallback"])
            originals = result["original_files"]
            self.assertTrue(any(r["file"] == "NOTICE" for r in originals))
            self.assertTrue(any(r["file"] == "original-source.zip" for r in originals))
            self.assertTrue(any(r["origin_type"] == "standard-license-text" for r in originals))

    def test_cache_is_bound_to_source_and_rejects_tampering(self):
        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            base = Path(directory)
            source = base / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("fixture", encoding="utf-8")
            package = {"id": "fixture-id", "name": "fixture", "version": "1.0.0",
                       "license": "MIT", "manifest_path": str(source / "Cargo.toml")}
            output = base / "one"
            output.mkdir()
            with patch.object(licenses, "CACHE", base / "cache"), patch.object(licenses, "fetch", return_value=MIT):
                first = licenses.collect(package, output)
                (source / "Cargo.toml").write_text("changed source", encoding="utf-8")
                output = base / "two"
                output.mkdir()
                second = licenses.collect(package, output)
                self.assertNotEqual(first["source_id"], second["source_id"])
                cached = base / "cache" / second["source_id"] / "MIT" / "record.json"
                record = json.loads(cached.read_text(encoding="utf-8"))
                record["sha256"] = hashlib.sha256(b"tampered").hexdigest()
                cached.write_text(json.dumps(record), encoding="utf-8")
                output = base / "three"
                output.mkdir()
                with self.assertRaisesRegex(ValueError, "identity/hash"):
                    licenses.collect(package, output)

    def test_license_file_fallback_when_license_is_null(self):
        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            base = Path(directory)
            source = base / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("[package]\nname='custom'\n", encoding="utf-8")
            (source / "CUSTOM-TERMS.txt").write_text("Custom license terms.\n", encoding="utf-8")
            package = {
                "id": "custom-id",
                "name": "custom",
                "version": "0.1.0",
                "license": None,
                "license_file": "CUSTOM-TERMS.txt",
                "manifest_path": str(source / "Cargo.toml"),
            }
            output = base / "out"
            output.mkdir()
            result = licenses.collect(package, output)
            self.assertEqual(result["selected_obligations"], ["LicenseRef-file:CUSTOM-TERMS.txt"])
            self.assertFalse(result["standard_text_with_source_fallback"])
            self.assertTrue((output / result["directory"] / "CUSTOM-TERMS.txt").is_file())

            bad_pkg = dict(package, license_file="../escape.txt")
            bad_out = base / "out2"
            bad_out.mkdir()
            with self.assertRaises(ValueError):
                licenses.collect(bad_pkg, bad_out)

    def test_fetch_catches_http_exception_and_atomic_cache_survives_concurrency(self):
        licenses.fetch.cache_clear()
        with patch.object(
            licenses.urllib.request,
            "urlopen",
            side_effect=http.client.IncompleteRead(b"truncated"),
        ):
            self.assertIsNone(licenses.fetch("https://example.invalid/license"))

        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            cache = Path(directory) / "cache" / "sid" / "MIT"
            record = {
                "origin_type": "standard-license-text",
                "url": "https://example.invalid",
                "source_id": "sid",
                "spdx": "MIT",
                "sha256": hashlib.sha256(MIT).hexdigest(),
            }

            def worker(_):
                licenses._write_cache_atomic(cache, record, MIT)
                hit = licenses._read_cache(cache, "sid", "MIT", "pkg")
                self.assertIsNotNone(hit)
                self.assertEqual(hit[1], MIT)

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                list(pool.map(worker, range(24)))
            leftovers = [p.name for p in cache.iterdir() if p.name not in ("record.json", "text.txt")]
            self.assertEqual(leftovers, [])


if __name__ == "__main__":
    unittest.main()
