"""DELTA Python worker: stdio JSONL protocol for indicator computation.

- stdout carries protocol messages only; diagnostics go to stderr (sanitized).
- Decimal-exact finance never crosses this boundary: indicator arrays are
  floats with documented warmup (None) semantics (financial-engine §6).
- This worker holds no credentials and receives no model secrets.
"""

from __future__ import annotations

import json
import math
import sys

PROTOCOL_VERSION = 1
MAX_MESSAGE_BYTES = 4 * 1024 * 1024
MAX_POINTS = 1_000_000


def sma(values: list[float], period: int) -> list[float | None]:
    if period <= 0:
        raise ValueError("period must be positive")
    out: list[float | None] = [None] * len(values)
    if period > len(values):
        return out
    run = 0.0
    for i, v in enumerate(values):
        run += v
        if i >= period:
            run -= values[i - period]
        if i >= period - 1:
            out[i] = run / period
    return out


def ema(values: list[float], period: int) -> list[float | None]:
    """TA-Lib semantics: seed with the SMA of the first `period` values."""
    if period <= 0:
        raise ValueError("period must be positive")
    out: list[float | None] = [None] * len(values)
    if len(values) < period:
        return out
    k = 2.0 / (period + 1)
    seed = sum(values[:period]) / period
    out[period - 1] = seed
    prev = seed
    for i in range(period, len(values)):
        prev = prev + k * (values[i] - prev)
        out[i] = prev
    return out


def _ema_seeded_at(values: list[float], period: int, start_idx: int) -> list[float | None]:
    """TA-Lib INT_EMA: the seed is the SMA of `period` bars ending at `start_idx`.

    A caller that starts past this period's own lookback reseeds instead of
    continuing an EMA from the first bar. MACD does that for the fast line.
    """
    out: list[float | None] = [None] * len(values)
    lookback = period - 1
    if start_idx < lookback:
        start_idx = lookback
    if start_idx >= len(values):
        return out
    seed_at = start_idx - lookback
    prev = sum(values[seed_at : seed_at + period]) / period
    today = seed_at + period
    k = 2.0 / (period + 1)
    while today <= start_idx:
        prev = prev + k * (values[today] - prev)
        today += 1
    out[start_idx] = prev
    idx = start_idx
    while today < len(values):
        prev = prev + k * (values[today] - prev)
        today += 1
        idx += 1
        out[idx] = prev
    return out


def macd(
    values: list[float], fast: int = 12, slow: int = 26, signal: int = 9
) -> tuple[list[float | None], list[float | None], list[float | None]]:
    """TA-Lib MACD: both EMAs are seeded at the slow lookback, and the
    published line, signal and histogram all begin `signal` periods later.
    """
    if fast <= 0 or slow <= 0 or signal <= 0:
        raise ValueError("period must be positive")
    if slow < fast:
        fast, slow = slow, fast
    n = len(values)
    line: list[float | None] = [None] * n
    signal_line: list[float | None] = [None] * n
    hist: list[float | None] = [None] * n
    lookback_slow = slow - 1
    lookback_signal = signal - 1
    first = lookback_slow + lookback_signal
    if n <= first:
        return line, signal_line, hist
    fast_ema = _ema_seeded_at(values, fast, lookback_slow)
    slow_ema = _ema_seeded_at(values, slow, lookback_slow)
    diff = [
        fast_ema[i] - slow_ema[i]
        for i in range(lookback_slow, n)
        if fast_ema[i] is not None and slow_ema[i] is not None
    ]
    sig = _ema_seeded_at(diff, signal, lookback_signal)
    for j, value in enumerate(diff):
        i = lookback_slow + j
        if i < first or sig[j] is None:
            continue
        line[i] = value
        signal_line[i] = sig[j]
        hist[i] = value - sig[j]
    return line, signal_line, hist


def rsi(values: list[float], period: int = 14) -> list[float | None]:
    """TA-Lib RSI: Wilder smoothing seeded with simple averages."""
    out: list[float | None] = [None] * len(values)
    if len(values) < period + 1:
        return out
    gains = 0.0
    losses = 0.0
    for i in range(1, period + 1):
        diff = values[i] - values[i - 1]
        if diff > 0:
            gains += diff
        else:
            losses -= diff
    avg_gain = gains / period
    avg_loss = losses / period
    if avg_loss == 0:
        out[period] = 100.0 if avg_gain > 0 else 50.0
    else:
        out[period] = 100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    for i in range(period + 1, len(values)):
        diff = values[i] - values[i - 1]
        gain = diff if diff > 0 else 0.0
        loss = -diff if diff < 0 else 0.0
        avg_gain = (avg_gain * (period - 1) + gain) / period
        avg_loss = (avg_loss * (period - 1) + loss) / period
        if avg_loss == 0:
            out[i] = 100.0 if avg_gain > 0 else 50.0
        else:
            out[i] = 100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    return out


INDICATORS = {"sma": sma, "ema": ema, "rsi": rsi, "macd": macd}


def compute_indicators(params: dict) -> dict:
    closes = params.get("closes")
    if not isinstance(closes, list) or len(closes) > MAX_POINTS:
        raise ValueError("closes missing or too large")
    if not all(isinstance(c, (int, float)) and not isinstance(c, bool) for c in closes):
        raise ValueError("closes must be numbers")
    values = [float(c) for c in closes]
    if not all(math.isfinite(v) for v in values):
        raise ValueError("closes must be finite")
    visible = params.get("visible_until")
    if visible is not None:
        visible = int(visible)
        if visible < 0 or visible > len(values):
            raise ValueError("visible_until out of range")
        # Points at and after the cutoff are invisible, including sentinels.
        values = values[:visible]
    requested = params.get("indicators", [])
    if not isinstance(requested, list):
        raise ValueError("indicators must be a list")
    out: dict[str, dict] = {}
    for spec in requested:
        if not isinstance(spec, dict):
            raise ValueError("indicator spec must be an object")
        name = spec.get("name")
        period = int(spec.get("period", 14))
        if name == "macd":
            line, signal, hist = macd(
                values,
                int(spec.get("fast", 12)),
                int(spec.get("slow", 26)),
                int(spec.get("signal", 9)),
            )
            out["macd"] = {"macd": line, "signal": signal, "hist": hist}
        elif name in ("sma", "ema", "rsi"):
            fn = INDICATORS[name]
            out[name] = {str(period): fn(values, period)}
        else:
            raise ValueError(f"unknown indicator {name!r}")
    return {"points": len(values), "indicators": out}


def handle(method: str, params: dict) -> dict:
    if method == "handshake":
        return {"protocol": PROTOCOL_VERSION, "capabilities": ["compute_indicators"]}
    if method == "compute_indicators":
        return compute_indicators(params)
    raise ValueError(f"unknown method {method!r}")


def error_line(msg_id, code: str, message: str) -> str:
    return json.dumps({"v": 1, "id": msg_id, "error": {"code": code, "message": message}})


def main() -> int:
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        if len(raw.encode("utf-8")) > MAX_MESSAGE_BYTES:
            print(json.dumps({"v": 1, "id": None, "error": {"code": "TOO_LARGE"}}), flush=True)
            continue
        msg_id = None
        try:
            msg = json.loads(raw)
            if not isinstance(msg, dict):
                raise ValueError("message must be a JSON object")
            msg_id = msg.get("id")
            if msg_id is not None and not isinstance(msg_id, (str, int)):
                msg_id = None
                raise ValueError("id must be a string or integer")
            params = msg.get("params") or {}
            if not isinstance(params, dict):
                raise ValueError("params must be an object")
            result = handle(msg.get("method", ""), params)
            print(json.dumps({"v": 1, "id": msg_id, "result": result}, allow_nan=False), flush=True)
        except (ValueError, TypeError, KeyError, OverflowError) as exc:
            # Never echo raw input; only the request id is recovered.
            print(error_line(msg_id, "INVALID_ARGUMENT", str(exc)[:200]), flush=True)
        except Exception as exc:  # noqa: BLE001 - one bad message must not kill the worker
            print(f"worker internal error: {type(exc).__name__}", file=sys.stderr, flush=True)
            print(error_line(msg_id, "INTERNAL", "internal worker error"), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
