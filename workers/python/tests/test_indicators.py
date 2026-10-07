"""Indicator reference tests (R1-A-11): hand-computed vectors with TA-Lib
semantics, warmup-None rules and future-data causality (AC-11)."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import delta_worker as dw  # noqa: E402


class IndicatorVectorTests(unittest.TestCase):
    def test_sma3_hand_computed(self):
        out = dw.sma([1, 2, 3, 4, 5], 3)
        self.assertEqual(out[0:2], [None, None], "warmup must be None")
        self.assertAlmostEqual(out[2], 2.0)
        self.assertAlmostEqual(out[3], 3.0)
        self.assertAlmostEqual(out[4], 4.0)

    def test_ema5_ta_lib_seed(self):
        # Seed = SMA(values[0..5]) = 3; k = 2/(5+1) = 1/3.
        # values[5] = 6 → EMA5 = 3 + (6-3)/3 = 4；values[6] = 7 → 4 + 3/3 = 5。
        out = dw.ema([1, 2, 3, 4, 5, 6, 7, 8], 5)
        self.assertIsNone(out[3])
        self.assertAlmostEqual(out[4], 3.0)
        self.assertAlmostEqual(out[5], 3 + (6 - 3) / 3, places=10)
        self.assertAlmostEqual(out[6], 4 + (7 - 4) / 3, places=10)

    def test_rsi14_monotonic_up_is_100(self):
        values = [100.0 + i for i in range(20)]
        out = dw.rsi(values, 14)
        self.assertIsNone(out[13], "warmup (period) must be None")
        self.assertAlmostEqual(out[14], 100.0)
        self.assertAlmostEqual(out[19], 100.0)

    def test_rsi_mixed_hand_computed_seed(self):
        # First 14 deltas: 10 up-moves of +1, 4 down-moves of -1
        # avg_gain = 10/14, avg_loss = 4/14 → RSI = 100 - 100/(1+10/4) = 71.43
        values = [0.0]
        for i in range(14):
            values.append(values[-1] + (1 if i % 10 < 7 else -1))
        out = dw.rsi(values, 14)
        # 实际序列：11 个 +1、3 个 -1 → RSI = 100 - 100/(1 + 11/3)。
        self.assertAlmostEqual(out[14], 100 - 100 / (1 + 11 / 3), places=6)

    def test_macd_warmup_and_first_value(self):
        # TA-Lib 0.6.4 publishes line, signal and histogram together.
        # fast=3, slow=6, signal=2 → first index is 6, and the seed is 1.5.
        values = [float(i) for i in range(40)]
        line, signal, hist = dw.macd(values, 3, 6, 2)
        self.assertIsNone(line[5], "warmup stays empty through the slow lookback")
        self.assertAlmostEqual(line[6], 1.5)
        self.assertAlmostEqual(signal[6], 1.5)
        self.assertAlmostEqual(hist[6], 0.0)

    def test_future_sentinel_causality_ac11(self):
        values = [10.0 + ((-1) ** i) * (i % 3) for i in range(60)]
        base_rsi = dw.rsi(values, 14)
        base_ema = dw.ema(values, 12)
        tampered = list(values)
        for i in range(50, 60):
            tampered[i] = 1e9  # extreme sentinel in the "future"
        tampered_rsi = dw.rsi(tampered, 14)
        tampered_ema = dw.ema(tampered, 12)
        for i in range(0, 50):
            self.assertEqual(base_rsi[i], tampered_rsi[i], f"rsi[{i}] affected by future data")
            self.assertEqual(base_ema[i], tampered_ema[i], f"ema[{i}] affected by future data")

    def test_r1_a_11_matches_pinned_talib_execution(self):
        fixture = json.loads(
            (Path(__file__).parent / "fixtures" / "talib_reference.json").read_text(
                encoding="utf-8"
            )
        )
        source = fixture["source"]
        self.assertEqual(source["package"], "TA-Lib")
        self.assertEqual(source["package_version"], "0.6.8")
        self.assertIn("0.6.4", source["ta_lib_c_version"])
        self.assertIn("pypi.org/project/TA-Lib/0.6.8", source["pypi"])
        self.assertIn("executing this package", source["note"])
        closes = [float(v) for v in fixture["closes"]]
        self._match(dw.sma(closes, fixture["sma_period"]), fixture["sma"])
        self._match(dw.ema(closes, fixture["ema_period"]), fixture["ema"])
        self._match(dw.rsi(closes, fixture["rsi_period"]), fixture["rsi"])
        line, signal, hist = dw.macd(
            closes,
            fixture["macd"]["fast"],
            fixture["macd"]["slow"],
            fixture["macd"]["signal"],
        )
        self._match(line, fixture["macd_line"])
        self._match(signal, fixture["macd_signal"])
        self._match(hist, fixture["macd_hist"])
        # Warmup from the recorded TA-Lib run, not a hand formula.
        self.assertIsNone(fixture["sma"][fixture["sma_period"] - 2])
        self.assertIsNotNone(fixture["sma"][fixture["sma_period"] - 1])
        self.assertIsNone(fixture["rsi"][fixture["rsi_period"] - 1])
        self.assertIsNotNone(fixture["rsi"][fixture["rsi_period"]])
        self.assertGreater(sum(v is not None for v in fixture["macd_signal"]), 0)

    def _match(self, got, expected):
        self.assertEqual(len(got), len(expected))
        for i, (actual, want) in enumerate(zip(got, expected)):
            if want is None:
                self.assertIsNone(actual, f"index {i}")
            else:
                self.assertIsNotNone(actual, f"index {i}")
                self.assertAlmostEqual(actual, want, delta=1e-8, msg=f"index {i}")

    def test_r1_a_11_visible_until_ignores_a_future_sentinel(self):
        fixture = json.loads(
            (Path(__file__).parent / "fixtures" / "talib_reference.json").read_text(
                encoding="utf-8"
            )
        )
        closes = [float(v) for v in fixture["closes"]]
        visible = fixture["visible_until"]
        poisoned = list(closes) + [1e12]
        cut = dw.compute_indicators(
            {
                "closes": poisoned,
                "visible_until": visible,
                "indicators": [{"name": "sma", "period": fixture["sma_period"]}],
            }
        )
        series = cut["indicators"]["sma"][str(fixture["sma_period"])]
        self.assertEqual(len(series), visible)
        self.assertEqual(series, dw.sma(closes[:visible], fixture["sma_period"]))


class ProtocolTests(unittest.TestCase):
    def _roundtrip(self, lines: list[str]) -> list[dict]:
        proc = subprocess.run(
            [sys.executable, str(Path(__file__).resolve().parents[1] / "delta_worker.py")],
            input="\n".join(lines) + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]

    def test_handshake_and_error_paths(self):
        out = self._roundtrip(
            [
                json.dumps({"v": 1, "id": "a", "method": "handshake", "params": {}}),
                json.dumps({"v": 1, "id": "b", "method": "nope", "params": {}}),
                "not-json",
            ]
        )
        self.assertEqual(out[0]["result"]["protocol"], 1)
        self.assertEqual(out[1]["error"]["code"], "INVALID_ARGUMENT")
        self.assertEqual(out[2]["error"]["code"], "INVALID_ARGUMENT")
        # stdout stays protocol-only; no payload echo.
        self.assertNotIn("not-json", proc_text(out))

    def test_malformed_messages_do_not_kill_the_worker(self):
        lines = [
            "[1, 2]",
            "42",
            json.dumps({"v": 1, "id": "p", "method": "compute_indicators", "params": [1]}),
            json.dumps({"v": 1, "id": "s", "method": "compute_indicators",
                        "params": {"closes": [1, 2, 3], "indicators": ["sma"]}}),
            json.dumps({"v": 1, "id": "n", "method": "compute_indicators",
                        "params": {"closes": [1, "x", 3], "indicators": []}}),
            '{"v": 1, "id": "f", "method": "compute_indicators", '
            '"params": {"closes": [1, NaN, 3], "indicators": []}}',
            json.dumps({"v": 1, "id": "ok", "method": "handshake", "params": {}}),
        ]
        out = self._roundtrip(lines)
        self.assertEqual(len(out), len(lines), "one response per message")
        for msg in out[:-1]:
            self.assertEqual(msg["error"]["code"], "INVALID_ARGUMENT", msg)
        self.assertEqual([m["id"] for m in out[2:6]], ["p", "s", "n", "f"])
        self.assertEqual(out[-1]["result"]["protocol"], 1)

    def test_compute_indicators_via_stdio(self):
        req = {
            "v": 1,
            "id": "r1",
            "method": "compute_indicators",
            "params": {
                "closes": [1, 2, 3, 4, 5],
                "indicators": [{"name": "sma", "period": 3}],
            },
        }
        out = self._roundtrip([json.dumps(req)])
        self.assertEqual(out[0]["id"], "r1")
        self.assertEqual(out[0]["result"]["indicators"]["sma"]["3"][2], 2.0)


def proc_text(messages: list[dict]) -> str:
    return json.dumps(messages)


if __name__ == "__main__":
    unittest.main()
