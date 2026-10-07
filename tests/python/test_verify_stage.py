"""Negative fixtures for current-stage identity, zero matches and stale builds."""
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import verify_stage as verify
import package_stage as package


class CurrentStageTest(unittest.TestCase):
    def setUp(self):
        self.config = verify.load_stage("R1", "1.2")

    def test_parent_and_branch_fail_closed(self):
        verify.validate_base(self.config, "codex/r1-workbench", self.config["stageParent"])
        for branch, parents in [("main", self.config["stageParent"]),
                                ("codex/r1-workbench", "a" * 40),
                                ("codex/r1-workbench", self.config["stageParent"] + " " + "b" * 40)]:
            with self.assertRaises(ValueError):
                verify.validate_base(self.config, branch, parents)
        with self.assertRaises(ValueError):
            verify.load_stage("R1", "1.2", "a" * 40)

    def test_zero_test_success_exit_is_still_failure(self):
        gate = verify.gate_result("rust", ["cargo", "test"], 0,
                                  "test result: ok. 0 passed; 0 failed; 0 ignored", 0)
        self.assertEqual(gate["exit_code"], 1)
        cases = verify.cases_for(self.config, [], [gate])
        self.assertTrue(all(c["status"] == "failed" for c in cases if c["mode"] == "automated"))
        self.assertTrue(all(c["status"] == "not-run" for c in cases if c["mode"] != "automated"))

    def test_missing_pattern_is_not_hidden_by_other_passing_tests(self):
        cases = verify.cases_for(self.config, ["unrelated_test"], [])
        self.assertTrue(all(c["status"] == "failed" for c in cases if c["mode"] == "automated"))

    def test_source_edit_or_old_verification_blocks_package(self):
        with self.assertRaises(ValueError):
            verify.require_unchanged({"sha256": "old"}, {"sha256": "new"})
        report = {"stage": "R1", "planVersion": "1.2", "stageParent": self.config["stageParent"],
                  "sourceFingerprint": {"sha256": "old"}, "exitCode": 0}
        with self.assertRaises(ValueError):
            package.verified_source(report, {"sha256": "new"}, self.config)
        package.verified_source(report, {"sha256": "old"}, self.config)
        report["exitCode"] = 1
        with self.assertRaises(ValueError):
            package.verified_source(report, {"sha256": "old"}, self.config)

    def test_version_and_path_are_validated(self):
        for stage, version in [("../R1", "1.2"), ("R1", "wrong")]:
            with self.assertRaises(ValueError):
                verify.load_stage(stage, version)

    def test_old_financial_and_permission_filters_remain_required(self):
        inherited = verify.inherited_assertions([])
        self.assertTrue(inherited)
        self.assertTrue(all(c["status"] == "failed" for c in inherited))
        self.assertTrue(any(c["filter"] == "r1_a_05" for c in inherited))
        self.assertTrue(any(c["filter"] == "r1_a_18" for c in inherited))


if __name__ == "__main__":
    unittest.main()
