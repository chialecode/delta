use crate::model::BarRecord;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndicatorPoint {
    pub session_date: NaiveDate,
    pub close_at: DateTime<Utc>,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacdPoint {
    pub session_date: NaiveDate,
    pub close_at: DateTime<Utc>,
    pub macd: Option<f64>,
    pub signal: Option<f64>,
    pub histogram: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BollingerPoint {
    pub session_date: NaiveDate,
    pub close_at: DateTime<Utc>,
    pub middle: Option<f64>,
    pub upper: Option<f64>,
    pub lower: Option<f64>,
}

/// Filter bars by `visible_until` and `is_final == true` (Financial Engine Sec 6 / AC-11).
/// Any unclosed bar or bar whose `close_at > visible_until` is excluded so future sentinel
/// values cannot leak into current indicator calculations.
pub fn filter_visible_final_bars(
    bars: &[BarRecord],
    visible_until: Option<DateTime<Utc>>,
) -> Vec<&BarRecord> {
    let mut filtered: Vec<&BarRecord> = bars
        .iter()
        .filter(|b| {
            if !b.is_final {
                return false;
            }
            match visible_until {
                Some(cutoff) => b.close_at <= cutoff,
                None => true,
            }
        })
        .collect();
    filtered.sort_by_key(|b| b.open_at);
    filtered
}

/// Simple Moving Average (SMA). Warmup points before `period` are `None`.
pub fn compute_sma(
    bars: &[BarRecord],
    period: usize,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<IndicatorPoint> {
    let vis = filter_visible_final_bars(bars, visible_until);
    let closes: Vec<f64> = vis
        .iter()
        .map(|b| b.close.to_f64().unwrap_or(0.0))
        .collect();
    let mut out = Vec::with_capacity(vis.len());

    for (i, bar) in vis.iter().enumerate() {
        let value = if period == 0 || i + 1 < period {
            None
        } else {
            let sum: f64 = closes[i + 1 - period..=i].iter().sum();
            Some(sum / (period as f64))
        };
        out.push(IndicatorPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            value,
        });
    }
    out
}

fn compute_ema_values(closes: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(closes.len());
    if period == 0 {
        return out;
    }
    let k = 2.0 / ((period as f64) + 1.0);
    let mut prev_ema: Option<f64> = None;

    for (i, &close) in closes.iter().enumerate() {
        let value = if i + 1 < period {
            None
        } else if i + 1 == period {
            let seed: f64 = closes[0..period].iter().sum::<f64>() / (period as f64);
            prev_ema = Some(seed);
            Some(seed)
        } else {
            let prev = prev_ema.unwrap_or(close);
            let next = (close - prev) * k + prev;
            prev_ema = Some(next);
            Some(next)
        };
        out.push(value);
    }
    out
}

/// Exponential Moving Average (EMA), seeded with the SMA of the first `period` bars.
pub fn compute_ema(
    bars: &[BarRecord],
    period: usize,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<IndicatorPoint> {
    if period == 0 {
        return Vec::new();
    }
    let vis = filter_visible_final_bars(bars, visible_until);
    let closes: Vec<f64> = vis
        .iter()
        .map(|b| b.close.to_f64().unwrap_or(0.0))
        .collect();
    let values = compute_ema_values(&closes, period);
    vis.into_iter()
        .zip(values)
        .map(|(bar, value)| IndicatorPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            value,
        })
        .collect()
}

/// MACD (fast_period = 12, slow_period = 26, signal_period = 9 by default).
pub fn compute_macd(
    bars: &[BarRecord],
    fast_period: usize,
    slow_period: usize,
    signal_period: usize,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<MacdPoint> {
    if fast_period == 0 || slow_period == 0 || signal_period == 0 {
        return Vec::new();
    }
    let vis = filter_visible_final_bars(bars, visible_until);
    let closes: Vec<f64> = vis
        .iter()
        .map(|b| b.close.to_f64().unwrap_or(0.0))
        .collect();
    let fast = compute_ema_values(&closes, fast_period);
    let slow = compute_ema_values(&closes, slow_period);

    let k_sig = 2.0 / ((signal_period as f64) + 1.0);
    let mut valid_macd_count = 0_usize;
    let mut seed_sum = 0.0_f64;
    let mut prev_sig: Option<f64> = None;
    let mut out = Vec::with_capacity(vis.len());

    for (i, bar) in vis.iter().enumerate() {
        let m_opt = match (fast[i], slow[i]) {
            (Some(f), Some(s)) => Some(f - s),
            _ => None,
        };
        let (sig_opt, hist_opt) = match m_opt {
            None => (None, None),
            Some(m) => {
                valid_macd_count += 1;
                if valid_macd_count < signal_period {
                    seed_sum += m;
                    (None, None)
                } else if valid_macd_count == signal_period {
                    seed_sum += m;
                    let seed = seed_sum / (signal_period as f64);
                    prev_sig = Some(seed);
                    (Some(seed), Some(m - seed))
                } else {
                    let prev = prev_sig.unwrap_or(m);
                    let next = (m - prev) * k_sig + prev;
                    prev_sig = Some(next);
                    (Some(next), Some(m - next))
                }
            }
        };
        out.push(MacdPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            macd: m_opt,
            signal: sig_opt,
            histogram: hist_opt,
        });
    }
    out
}

fn rsi_from_avgs(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 {
        if avg_gain == 0.0 {
            50.0
        } else {
            100.0
        }
    } else {
        let rs = avg_gain / avg_loss;
        100.0 - (100.0 / (1.0 + rs))
    }
}

/// Wilder's RSI (default period = 14).
pub fn compute_rsi(
    bars: &[BarRecord],
    period: usize,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<IndicatorPoint> {
    if period == 0 {
        return Vec::new();
    }
    let vis = filter_visible_final_bars(bars, visible_until);
    if vis.is_empty() {
        return Vec::new();
    }
    let closes: Vec<f64> = vis
        .iter()
        .map(|b| b.close.to_f64().unwrap_or(0.0))
        .collect();
    let mut out = Vec::with_capacity(vis.len());

    let mut avg_gain = 0.0;
    let mut avg_loss = 0.0;

    for (i, bar) in vis.iter().enumerate() {
        if i == 0 {
            out.push(IndicatorPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                value: None,
            });
            continue;
        }
        let change = closes[i] - closes[i - 1];
        let gain = if change > 0.0 { change } else { 0.0 };
        let loss = if change < 0.0 { -change } else { 0.0 };

        let value = if i < period {
            avg_gain += gain;
            avg_loss += loss;
            None
        } else if i == period {
            avg_gain = (avg_gain + gain) / (period as f64);
            avg_loss = (avg_loss + loss) / (period as f64);
            Some(rsi_from_avgs(avg_gain, avg_loss))
        } else {
            avg_gain = (avg_gain * ((period - 1) as f64) + gain) / (period as f64);
            avg_loss = (avg_loss * ((period - 1) as f64) + loss) / (period as f64);
            Some(rsi_from_avgs(avg_gain, avg_loss))
        };
        out.push(IndicatorPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            value,
        });
    }
    out
}

/// Extension indicator (M1-EXT): Wilder's Average True Range (ATR).
pub fn compute_atr(
    bars: &[BarRecord],
    period: usize,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<IndicatorPoint> {
    let vis = filter_visible_final_bars(bars, visible_until);
    let mut out = Vec::with_capacity(vis.len());
    if period == 0 || vis.is_empty() {
        return out;
    }

    let mut trs = Vec::with_capacity(vis.len());
    for (i, bar) in vis.iter().enumerate() {
        let high = bar.high.to_f64().unwrap_or(0.0);
        let low = bar.low.to_f64().unwrap_or(0.0);
        let tr = if i == 0 {
            high - low
        } else {
            let prev_close = vis[i - 1].close.to_f64().unwrap_or(0.0);
            (high - low)
                .max((high - prev_close).abs())
                .max((low - prev_close).abs())
        };
        trs.push(tr);
    }

    let mut prev_atr = 0.0;
    for (i, bar) in vis.iter().enumerate() {
        let val = if i + 1 < period {
            None
        } else if i + 1 == period {
            let seed = trs[0..period].iter().sum::<f64>() / (period as f64);
            prev_atr = seed;
            Some(seed)
        } else {
            let next = (prev_atr * ((period - 1) as f64) + trs[i]) / (period as f64);
            prev_atr = next;
            Some(next)
        };
        out.push(IndicatorPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            value: val,
        });
    }
    out
}

/// Extension indicator (M1-EXT): Bollinger Bands (SMA ± std_dev_mult * σ).
pub fn compute_bollinger_bands(
    bars: &[BarRecord],
    period: usize,
    std_dev_mult: f64,
    visible_until: Option<DateTime<Utc>>,
) -> Vec<BollingerPoint> {
    let vis = filter_visible_final_bars(bars, visible_until);
    let closes: Vec<f64> = vis
        .iter()
        .map(|b| b.close.to_f64().unwrap_or(0.0))
        .collect();
    let mut out = Vec::with_capacity(vis.len());

    for (i, bar) in vis.iter().enumerate() {
        if period == 0 || i + 1 < period {
            out.push(BollingerPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                middle: None,
                upper: None,
                lower: None,
            });
        } else {
            let slice = &closes[i + 1 - period..=i];
            let mean = slice.iter().sum::<f64>() / (period as f64);
            let variance = slice.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (period as f64);
            let std_dev = variance.sqrt();
            out.push(BollingerPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                middle: Some(mean),
                upper: Some(mean + std_dev_mult * std_dev),
                lower: Some(mean - std_dev_mult * std_dev),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AdjustmentKind;
    use chrono::TimeZone;
    use rust_decimal::Decimal;

    fn bar(day: u32, close: i64) -> BarRecord {
        let d = NaiveDate::from_ymd_opt(2026, 1, day).unwrap();
        let px = Decimal::from(close);
        BarRecord {
            id: format!("B-{day}"),
            instrument_id: "TEST".into(),
            timeframe: "1d".into(),
            session: "regular".into(),
            session_date: d,
            open_at: Utc.with_ymd_and_hms(2026, 1, day, 14, 30, 0).unwrap(),
            close_at: Utc.with_ymd_and_hms(2026, 1, day, 21, 0, 0).unwrap(),
            open: px,
            high: px,
            low: px,
            close: px,
            volume: Decimal::from(100),
            source: "test".into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        }
    }

    #[test]
    fn flat_series_rsi_is_neutral_50_and_rising_is_100() {
        let flat: Vec<BarRecord> = (1..=6).map(|d| bar(d, 100)).collect();
        let rsi_flat = compute_rsi(&flat, 3, None);
        assert_eq!(rsi_flat[2].value, None);
        assert_eq!(rsi_flat[3].value, Some(50.0));
        assert_eq!(rsi_flat[5].value, Some(50.0));

        let rising: Vec<BarRecord> = (1..=6).map(|d| bar(d, 100 + d as i64)).collect();
        let rsi_rising = compute_rsi(&rising, 3, None);
        assert_eq!(rsi_rising[3].value, Some(100.0));
        assert_eq!(rsi_rising[5].value, Some(100.0));
    }

    #[test]
    fn macd_single_pass_matches_ema_difference_and_signal() {
        let bars: Vec<BarRecord> = (1..=10).map(|d| bar(d, 100 + (d as i64) * 2)).collect();
        let macd = compute_macd(&bars, 2, 3, 2, None);
        let fast = compute_ema(&bars, 2, None);
        let slow = compute_ema(&bars, 3, None);
        assert_eq!(macd.len(), 10);
        for i in 0..10 {
            match (fast[i].value, slow[i].value) {
                (Some(f), Some(s)) => {
                    assert!((macd[i].macd.unwrap() - (f - s)).abs() < 1e-12);
                }
                _ => assert!(macd[i].macd.is_none()),
            }
        }
        assert!(macd[3].signal.is_some());
        assert!((macd[3].histogram.unwrap()).abs() < 1e-12);
    }
}
