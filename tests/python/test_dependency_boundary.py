"""R1-A-01: core stays free of UI, database and model transport dependencies."""

from __future__ import annotations

import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# SPDX notes for direct workspace dependencies. Adding a dependency requires
# an entry here so the inventory cannot drift silently.
LICENSES = {
    "rust_decimal": "MIT",
    "serde": "MIT OR Apache-2.0",
    "serde_json": "MIT OR Apache-2.0",
    "chrono": "MIT OR Apache-2.0",
    "chrono-tz": "MIT OR Apache-2.0",
    "thiserror": "MIT OR Apache-2.0",
    "anyhow": "MIT OR Apache-2.0",
    "uuid": "MIT OR Apache-2.0",
    "tracing": "MIT",
    "tracing-subscriber": "MIT",
    "tokio": "MIT",
    "tokio-util": "MIT",
    "reqwest": "MIT OR Apache-2.0",
    "futures": "MIT OR Apache-2.0",
    "eventsource-stream": "MIT OR Apache-2.0",
    "rusqlite": "MIT",
    "csv": "Unlicense OR MIT",
    "keyring": "MIT OR Apache-2.0",
    "directories": "MIT OR Apache-2.0",
    "proptest": "MIT OR Apache-2.0",
    "tempfile": "MIT OR Apache-2.0",
}

FORBIDDEN_IN_CORE = ("rusqlite", "reqwest", "tokio", "gpui", "gpui-kit", "keyring")


class DependencyBoundaryTest(unittest.TestCase):
    def test_r1_a_01_core_has_no_ui_db_or_model_transport(self):
        text = (ROOT / "crates" / "delta-core" / "Cargo.toml").read_text(encoding="utf-8")
        for name in FORBIDDEN_IN_CORE:
            self.assertNotIn(name, text)
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        members = workspace["workspace"]["members"]
        self.assertNotIn("workers/agent", members)
        deps = workspace["workspace"]["dependencies"]
        missing = sorted(set(deps) - set(LICENSES))
        self.assertEqual(missing, [], "license inventory is missing entries")

    def test_r1_a_01_python_worker_does_not_import_model_clients(self):
        text = (ROOT / "workers" / "python" / "delta_worker.py").read_text(encoding="utf-8")
        for token in ("openai", "api_key", "reqwest", "sqlite"):
            self.assertNotIn(token, text.lower())
