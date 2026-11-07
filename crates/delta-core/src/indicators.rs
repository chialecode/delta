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

/// Exponential Moving Average (EMA), seeded with the SMA of the first `period` bars.
pub fn compute_ema(
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
    if period == 0 {
        return out;
    }

    let k = 2.0 / ((period as f64) + 1.0);
    let mut prev_ema: Option<f64> = None;

    for (i, bar) in vis.iter().enumerate() {
        let value = if i + 1 < period {
            None
        } else if i + 1 == period {
            let seed: f64 = closes[0..period].iter().sum::<f64>() / (period as f64);
            prev_ema = Some(seed);
            Some(seed)
        } else {
            let prev = prev_ema.unwrap_or(closes[i]);
            let next = (closes[i] - prev) * k + prev;
            prev_ema = Some(next);
            Some(next)
        };
        out.push(IndicatorPoint {
            session_date: bar.session_date,
            close_at: bar.close_at,
            value,
        });
    }
    out
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
    let fast = compute_ema(bars, fast_period, visible_until);
    let slow = compute_ema(bars, slow_period, visible_until);

    let mut macd_vals: Vec<Option<f64>> = Vec::with_capacity(vis.len());
    for i in 0..vis.len() {
        match (fast[i].value, slow[i].value) {
            (Some(f), Some(s)) => macd_vals.push(Some(f - s)),
            _ => macd_vals.push(None),
        }
    }

    let k_sig = 2.0 / ((signal_period as f64) + 1.0);
    let mut valid_macd_window: Vec<f64> = Vec::new();
    let mut prev_sig: Option<f64> = None;
    let mut out = Vec::with_capacity(vis.len());

    for (i, bar) in vis.iter().enumerate() {
        let m_opt = macd_vals[i];
        let (sig_opt, hist_opt) = match m_opt {
            None => (None, None),
            Some(m) => {
                valid_macd_window.push(m);
                if valid_macd_window.len() < signal_period || signal_period == 0 {
                    (None, None)
                } else if valid_macd_window.len() == signal_period {
                    let seed = valid_macd_window.iter().sum::<f64>() / (signal_period as f64);
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

/// Wilder's RSI (default period = 14).
pub fn compute_rsi(
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
    if period == 0 || vis.is_empty() {
        return out;
    }

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

        if i < period {
            avg_gain += gain;
            avg_loss += loss;
            out.push(IndicatorPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                value: None,
            });
        } else if i == period {
            avg_gain = (avg_gain + gain) / (period as f64);
            avg_loss = (avg_loss + loss) / (period as f64);
            let rsi = if avg_loss == 0.0 {
                100.0
            } else {
                let rs = avg_gain / avg_loss;
                100.0 - (100.0 / (1.0 + rs))
            };
            out.push(IndicatorPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                value: Some(rsi),
            });
        } else {
            avg_gain = (avg_gain * ((period - 1) as f64) + gain) / (period as f64);
            avg_loss = (avg_loss * ((period - 1) as f64) + loss) / (period as f64);
            let rsi = if avg_loss == 0.0 {
                100.0
            } else {
                let rs = avg_gain / avg_loss;
                100.0 - (100.0 / (1.0 + rs))
            };
            out.push(IndicatorPoint {
                session_date: bar.session_date,
                close_at: bar.close_at,
                value: Some(rsi),
            });
        }
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
