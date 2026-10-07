"""R1-A-25 契约测试：验证器自身的覆盖面与退出码策略（负向夹具）。

这些测试不判断产品行为，只保证“缺项不能通过”的规则真实生效：
用例集里每个自动用例都必须有命令映射或缺项理由，人工/live 必须被显式登记，
退出码策略在缺项时返回非零。
"""

from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load_verifier():
    spec = importlib.util.spec_from_file_location(
        "verify_r1", ROOT / "scripts" / "verify_r1.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)  # module-level only; main() is not called
    return module


class VerifyContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.verifier = load_verifier()
        cls.cases = json.loads(
            (ROOT / "docs/delivery/r1-cases.json").read_text(encoding="utf-8"))["cases"]

    def test_every_automated_case_is_mapped_or_has_a_reason(self):
        unmapped = [
            c["id"] for c in self.cases
            if c["mode"] == "automated"
            and c["id"] not in self.verifier.CASE_COMMANDS
            and c["id"] not in self.verifier.CASE_GAPS
        ]
        self.assertEqual(unmapped, [], "automated cases without command or gap reason")

    def test_manual_and_live_cases_are_declared(self):
        declared = [
            c["id"] for c in self.cases
            if c["mode"] != "automated" and c["id"] in self.verifier.MANUAL_LIVE
        ]
        total = [c["id"] for c in self.cases if c["mode"] != "automated"]
        self.assertEqual(sorted(declared), sorted(total))

    def test_gap_reasons_are_not_empty(self):
        for cid, reason in self.verifier.CASE_GAPS.items():
            with self.subTest(case=cid):
                self.assertTrue(reason.strip(), f"{cid} has an empty reason")

    def test_exit_code_is_nonzero_when_a_required_case_is_missing(self):
        """Negative fixture: a not-run automated case must block the run."""
        code, blocking = self.verifier.exit_code_for([
            {"id": "R1-A-01", "mode": "automated", "status": "passed"},
            {"id": "R1-A-99", "mode": "automated", "status": "not-run"},
            {"id": "R1-M-01", "mode": "manual", "status": "not-run"},
        ])
        self.assertEqual(code, 1)
        self.assertEqual(blocking, ["R1-A-99"])

    def test_exit_code_is_zero_when_all_required_cases_pass(self):
        code, blocking = self.verifier.exit_code_for([
            {"id": "R1-A-01", "mode": "automated", "status": "passed"},
            {"id": "R1-L-01", "mode": "live", "status": "not-run"},
        ])
        self.assertEqual((code, blocking), (0, []))

    def test_failed_gate_blocks_even_when_cases_pass(self):
        code, blocking = self.verifier.exit_code_for(
            [{"id": "R1-A-01", "mode": "automated", "status": "passed"}],
            [{"name": "cargo_clippy", "exit_code": 101}],
        )
        self.assertEqual((code, blocking), (1, ["gate:cargo_clippy"]))

    def test_partial_case_blocks(self):
        code, blocking = self.verifier.exit_code_for([
            {"id": "R1-A-05", "mode": "automated", "status": "partial"},
        ])
        self.assertEqual((code, blocking), (1, ["R1-A-05"]))

    def test_zero_matched_tests_are_detected(self):
        cargo = ["cargo", "test", "-p", "x", "nothing"]
        empty = "test result: ok. 0 passed; 0 failed; 0 ignored\n" * 3
        self.assertEqual(self.verifier.tests_executed(cargo, empty), 0)
        some = empty + "test result: ok. 4 passed; 0 failed; 0 ignored\n"
        self.assertEqual(self.verifier.tests_executed(cargo, some), 4)
        unit = [sys.executable, "-m", "unittest", "discover"]
        self.assertEqual(self.verifier.tests_executed(unit, "\nRan 0 tests in 0.0s\n"), 0)
        self.assertIsNone(self.verifier.tests_executed(["cargo", "build"], ""))

    def test_partial_reasons_are_not_empty_and_mapped(self):
        for cid, reason in self.verifier.CASE_PARTIAL.items():
            with self.subTest(case=cid):
                self.assertTrue(reason.strip())
                self.assertIn(cid, self.verifier.CASE_COMMANDS)

    def test_plan_version_matches_the_approved_plan(self):
        self.assertEqual(self.verifier.PLAN_VERSION, "1.1")


if __name__ == "__main__":
    sys.exit(unittest.main())
