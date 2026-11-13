"""Offline regressions for the dependency inventory's version rules."""

import unittest

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


if __name__ == "__main__":
    unittest.main()
