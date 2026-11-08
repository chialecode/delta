use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::{
    compute_ema, compute_macd, compute_rsi, compute_sma, filter_visible_final_bars, AdjustmentKind,
    BarRecord, IndicatorPoint, MacdPoint,
};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerKind {
    BuyFill,
    SellFill,
    Note,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartMarkerDto {
    pub id: String,
    pub kind: MarkerKind,
    pub session_date: NaiveDate,
    pub bar_index: usize,
    pub price: Decimal,
    pub label: String,
    pub target_id: String,
    pub target_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartMarkerLayout {
    pub marker: ChartMarkerDto,
    pub x_ratio: f32,
    pub y_ratio: f32,
    pub stack_slot: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndicatorConfig {
    pub sma_periods: Vec<usize>,
    pub ema_periods: Vec<usize>,
    pub macd: Option<(usize, usize, usize)>,
    pub rsi_period: Option<usize>,
}

impl Default for IndicatorConfig {
    fn default() -> Self {
        Self {
            sma_periods: vec![5, 20],
            ema_periods: vec![12, 26],
            macd: Some((12, 26, 9)),
            rsi_period: Some(14),
        }
    }
}

pub fn parse_indicator_config(json: &str) -> Result<IndicatorConfig, crate::ErrorEnvelope> {
    let invalid = || {
        crate::ErrorEnvelope::new(
            "INVALID_INDICATORS",
            "指标参数须为有效配置；周期为 1～1000，MACD 快线须小于慢线",
            false,
        )
    };
    let value: serde_json::Value = serde_json::from_str(json).map_err(|_| invalid())?;
    let config: IndicatorConfig = if let Some(legacy) = value.as_array() {
        let mut config = IndicatorConfig {
            sma_periods: vec![],
            ema_periods: vec![],
            macd: None,
            rsi_period: None,
        };
        for item in legacy {
            let name = item.as_str().ok_or_else(invalid)?.to_ascii_uppercase();
            if let Some(period) = name.strip_prefix("SMA") {
                config
                    .sma_periods
                    .push(period.parse().map_err(|_| invalid())?);
            } else if let Some(period) = name.strip_prefix("EMA") {
                config
                    .ema_periods
                    .push(period.parse().map_err(|_| invalid())?);
            } else if let Some(period) = name.strip_prefix("RSI") {
                config.rsi_period = Some(period.parse().map_err(|_| invalid())?);
            } else {
                return Err(invalid());
            }
        }
        config
    } else {
        serde_json::from_value(value).map_err(|_| invalid())?
    };
    if config.sma_periods.len() > 8
        || config.ema_periods.len() > 8
        || config
            .sma_periods
            .iter()
            .chain(&config.ema_periods)
            .any(|&p| !(1..=1000).contains(&p))
        || config.rsi_period.is_some_and(|p| !(1..=1000).contains(&p))
        || config.macd.is_some_and(|(fast, slow, signal)| {
            fast == 0 || fast >= slow || slow > 1000 || signal == 0 || signal > 1000
        })
    {
        return Err(invalid());
    }
    Ok(config)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LodCandleDto {
    pub start_bar_index: usize,
    pub end_bar_index: usize,
    pub session_date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub has_gap_before: bool,
    pub is_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KlineWindowSliceDto {
    pub instrument_id: String,
    pub venue: String,
    pub timeframe: String,
    pub adjustment: AdjustmentKind,
    pub visible_start_index: usize,
    pub visible_end_index: usize,
    pub total_visible_bars: usize,
    pub bars: Vec<BarRecord>,
    pub lod_candles: Vec<LodCandleDto>,
    pub sma_series: Vec<(usize, Vec<IndicatorPoint>)>,
    pub ema_series: Vec<(usize, Vec<IndicatorPoint>)>,
    pub macd_series: Vec<MacdPoint>,
    pub rsi_series: Vec<IndicatorPoint>,
    pub markers: Vec<ChartMarkerLayout>,
    pub gap_indices: Vec<usize>,
    pub has_incomplete_tail: bool,
    pub is_stale: bool,
    pub timezone_label: String,
}

/// Last visible close versus the previous visible session, computed host-side in `Decimal` so
/// views only format it (DESIGN: UI never derives financial numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCloseChangeDto {
    pub session_date: NaiveDate,
    pub close: Decimal,
    pub previous_close: Option<Decimal>,
    pub change: Option<Decimal>,
    /// Percentage change rounded to two decimals; `None` without a non-zero previous close.
    pub change_pct: Option<Decimal>,
}

impl KlineWindowSliceDto {
    pub fn last_close_change(&self) -> Option<LastCloseChangeDto> {
        let last = self.bars.last()?;
        let previous_close = self
            .bars
            .len()
            .checked_sub(2)
            .and_then(|index| self.bars.get(index))
            .map(|bar| bar.close);
        let change = previous_close.map(|previous| (last.close - previous).normalize());
        let change_pct = previous_close
            .filter(|previous| !previous.is_zero())
            .and_then(|previous| {
                change.map(|change| {
                    (change * Decimal::from(100) / previous)
                        .round_dp(2)
                        .normalize()
                })
            });
        Some(LastCloseChangeDto {
            session_date: last.session_date,
            close: last.close,
            previous_close,
            change,
            change_pct,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CachedHistoryKey {
    bars_len: usize,
    first_id: String,
    last_id: String,
    last_close_str: String,
    visible_until: Option<DateTime<Utc>>,
    config: IndicatorConfig,
}

struct CachedHistoryData {
    key: CachedHistoryKey,
    source_bars: Vec<BarRecord>,
    vis_bars: Vec<BarRecord>,
    has_incomplete_tail: bool,
    sma_full: Vec<(usize, Vec<IndicatorPoint>)>,
    ema_full: Vec<(usize, Vec<IndicatorPoint>)>,
    macd_full: Vec<MacdPoint>,
    rsi_full: Vec<IndicatorPoint>,
}

thread_local! {
    static HISTORY_CACHE: RefCell<Option<CachedHistoryData>> = const { RefCell::new(None) };
}

/// Pure Rust K-line viewport state machine (`S-16`, zero GPUI dependency).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KlineViewport {
    pub total_bars: usize,
    pub visible_start: usize,
    pub visible_end: usize,
    pub min_visible_bars: usize,
    pub max_visible_bars: usize,
    pub crosshair_index: Option<usize>,
    pub crosshair_price: Option<f64>,
    pub indicator_config: IndicatorConfig,
    #[serde(default)]
    pub requested_anchor_date: Option<NaiveDate>,
    #[serde(default)]
    pub requested_window: Option<(NaiveDate, NaiveDate)>,
}

impl KlineViewport {
    pub fn new(total_bars: usize, initial_window: usize) -> Self {
        let min_visible_bars = 10;
        let max_visible_bars = 5000;
        if total_bars == 0 {
            return Self {
                total_bars: 0,
                visible_start: 0,
                visible_end: 0,
                min_visible_bars,
                max_visible_bars,
                crosshair_index: None,
                crosshair_price: None,
                indicator_config: IndicatorConfig::default(),
                requested_anchor_date: None,
                requested_window: None,
            };
        }
        let min_b = min_visible_bars.min(total_bars).max(1);
        let max_b = max_visible_bars.min(total_bars).max(min_b);
        let len = initial_window.clamp(min_b, max_b).min(total_bars);
        let visible_start = total_bars.saturating_sub(len);
        Self {
            total_bars,
            visible_start,
            visible_end: total_bars,
            min_visible_bars,
            max_visible_bars,
            crosshair_index: None,
            crosshair_price: None,
            indicator_config: IndicatorConfig::default(),
            requested_anchor_date: None,
            requested_window: None,
        }
    }

    pub fn visible_range(&self) -> Range<usize> {
        self.visible_start..self.visible_end
    }

    pub fn visible_len(&self) -> usize {
        self.visible_end.saturating_sub(self.visible_start)
    }

    pub fn zoom(&mut self, factor: f64, anchor_ratio: f64) {
        if self.total_bars == 0 || !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor_ratio = anchor_ratio.clamp(0.0, 1.0);
        let cur_len = self.visible_len().max(1) as f64;
        let min_len = self.min_visible_bars.min(self.total_bars).max(1);
        let max_len = self.max_visible_bars.min(self.total_bars).max(min_len);
        let new_len = ((cur_len * factor).round() as usize).clamp(min_len, max_len);

        let anchor_pos = (self.visible_start as f64) + cur_len * anchor_ratio;
        let raw_start = (anchor_pos - (new_len as f64) * anchor_ratio).round() as isize;
        let max_start = self.total_bars.saturating_sub(new_len) as isize;
        let new_start = raw_start.clamp(0, max_start) as usize;

        self.visible_start = new_start;
        self.visible_end = new_start + new_len;
    }

    pub fn pan(&mut self, delta_bars: isize) {
        if self.total_bars == 0 {
            return;
        }
        let len = self.visible_len().max(1).min(self.total_bars);
        let max_start = self.total_bars.saturating_sub(len) as isize;
        let new_start = ((self.visible_start as isize) + delta_bars).clamp(0, max_start) as usize;
        self.visible_start = new_start;
        self.visible_end = new_start + len;
    }

    pub fn jump_to_date(&mut self, bars: &[BarRecord], target_date: NaiveDate) -> Option<usize> {
        if bars.is_empty() || self.total_bars == 0 {
            return None;
        }
        let limit = bars.len().min(self.total_bars);
        let search_slice = &bars[..limit];
        let idx = match search_slice.binary_search_by_key(&target_date, |b| b.session_date) {
            Ok(i) => i,
            Err(i) => i.min(limit.saturating_sub(1)),
        };

        let len = self.visible_len().max(1).min(self.total_bars);
        let half = len / 2;
        let max_start = self.total_bars.saturating_sub(len);
        let start = idx.saturating_sub(half).min(max_start);

        self.visible_start = start;
        self.visible_end = start + len;
        self.crosshair_index = Some(idx);
        Some(idx)
    }

    pub fn move_crosshair(&mut self, bar_index: Option<usize>, price: Option<f64>) {
        self.crosshair_index = bar_index.filter(|&i| i < self.total_bars);
        self.crosshair_price = price.filter(|p| p.is_finite());
    }

    pub fn hit_test_bar(&self, x_ratio: f64) -> Option<usize> {
        let len = self.visible_len();
        if len == 0 || !x_ratio.is_finite() || !(0.0..=1.0).contains(&x_ratio) {
            return None;
        }
        let offset = ((x_ratio * (len as f64)).floor() as usize).min(len - 1);
        Some(self.visible_start + offset)
    }

    pub fn hit_test_marker<'a>(
        &self,
        layouts: &'a [ChartMarkerLayout],
        x_ratio: f32,
        y_ratio: f32,
        tolerance: f32,
    ) -> Option<&'a ChartMarkerDto> {
        if !x_ratio.is_finite() || !y_ratio.is_finite() || !tolerance.is_finite() || tolerance < 0.0
        {
            return None;
        }
        let mut best: Option<(f32, &'a ChartMarkerDto)> = None;
        for layout in layouts {
            let dx = layout.x_ratio - x_ratio;
            let dy = layout.y_ratio - y_ratio;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= tolerance {
                match best {
                    None => best = Some((dist, &layout.marker)),
                    Some((best_dist, _)) if dist < best_dist => {
                        best = Some((dist, &layout.marker));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(_, marker)| marker)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_window_slice(
        &self,
        instrument_id: &str,
        venue: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        all_bars: &[BarRecord],
        visible_until: Option<DateTime<Utc>>,
        raw_markers: &[ChartMarkerDto],
        expected_gap_dates: &[NaiveDate],
        max_pixel_columns: usize,
    ) -> KlineWindowSliceDto {
        let key = CachedHistoryKey {
            bars_len: all_bars.len(),
            first_id: all_bars.first().map(|b| b.id.clone()).unwrap_or_default(),
            last_id: all_bars.last().map(|b| b.id.clone()).unwrap_or_default(),
            last_close_str: all_bars
                .last()
                .map(|b| b.close.to_string())
                .unwrap_or_default(),
            visible_until,
            config: self.indicator_config.clone(),
        };

        HISTORY_CACHE.with(|cell| {
            let mut cache_opt = cell.borrow_mut();
            let needs_recompute = match cache_opt.as_ref() {
                Some(cached) => cached.key != key || cached.source_bars.as_slice() != all_bars,
                None => true,
            };

            if needs_recompute {
                let has_incomplete_tail = all_bars.iter().any(|b| {
                    !b.is_final
                        && match visible_until {
                            Some(cutoff) => b.open_at <= cutoff || b.close_at <= cutoff,
                            None => true,
                        }
                });

                let vis_refs = filter_visible_final_bars(all_bars, visible_until);
                let vis_bars: Vec<BarRecord> = vis_refs.into_iter().cloned().collect();

                let sma_full: Vec<(usize, Vec<IndicatorPoint>)> = self
                    .indicator_config
                    .sma_periods
                    .iter()
                    .map(|&p| (p, compute_sma(&vis_bars, p, visible_until)))
                    .collect();

                let ema_full: Vec<(usize, Vec<IndicatorPoint>)> = self
                    .indicator_config
                    .ema_periods
                    .iter()
                    .map(|&p| (p, compute_ema(&vis_bars, p, visible_until)))
                    .collect();

                let macd_full = match self.indicator_config.macd {
                    Some((fast, slow, sig)) => {
                        compute_macd(&vis_bars, fast, slow, sig, visible_until)
                    }
                    None => Vec::new(),
                };

                let rsi_full = match self.indicator_config.rsi_period {
                    Some(p) => compute_rsi(&vis_bars, p, visible_until),
                    None => Vec::new(),
                };

                *cache_opt = Some(CachedHistoryData {
                    key,
                    source_bars: all_bars.to_vec(),
                    vis_bars,
                    has_incomplete_tail,
                    sma_full,
                    ema_full,
                    macd_full,
                    rsi_full,
                });
            }

            let cached = cache_opt.as_ref().expect("history cache populated");
            let total_vis = cached.vis_bars.len();
            let end = self.visible_end.min(total_vis);
            let start = self.visible_start.min(end);
            let bars = cached.vis_bars[start..end].to_vec();

            let sma_series = cached
                .sma_full
                .iter()
                .map(|(p, series)| (*p, series[start..end].to_vec()))
                .collect();

            let ema_series = cached
                .ema_full
                .iter()
                .map(|(p, series)| (*p, series[start..end].to_vec()))
                .collect();

            let macd_series = if cached.macd_full.len() >= end {
                cached.macd_full[start..end].to_vec()
            } else {
                Vec::new()
            };

            let rsi_series = if cached.rsi_full.len() >= end {
                cached.rsi_full[start..end].to_vec()
            } else {
                Vec::new()
            };

            let is_crypto = venue.eq_ignore_ascii_case("CRYPTO")
                || venue.eq_ignore_ascii_case("BINANCE")
                || instrument_id.contains("USDT")
                || instrument_id.contains("BTC");
            let max_normal_day_diff = if is_crypto { 1 } else { 4 };

            let mut gap_indices = Vec::new();
            let mut gap_set = HashSet::new();
            for i in 1..bars.len() {
                let prev_date = bars[i - 1].session_date;
                let curr_date = bars[i].session_date;
                let day_diff = (curr_date - prev_date).num_days();
                let has_expected_gap = expected_gap_dates
                    .iter()
                    .any(|&gd| gd > prev_date && gd < curr_date);
                if day_diff > max_normal_day_diff || has_expected_gap {
                    let global_idx = start + i;
                    gap_indices.push(global_idx);
                    gap_set.insert(global_idx);
                }
            }

            let cols = max_pixel_columns.max(1);
            let mut lod_candles = Vec::new();
            if bars.len() <= cols {
                for (local_idx, b) in bars.iter().enumerate() {
                    let global_idx = start + local_idx;
                    lod_candles.push(LodCandleDto {
                        start_bar_index: global_idx,
                        end_bar_index: global_idx,
                        session_date: b.session_date,
                        open: b.open.to_f64().unwrap_or(0.0),
                        high: b.high.to_f64().unwrap_or(0.0),
                        low: b.low.to_f64().unwrap_or(0.0),
                        close: b.close.to_f64().unwrap_or(0.0),
                        volume: b.volume.to_f64().unwrap_or(0.0),
                        has_gap_before: gap_set.contains(&global_idx),
                        is_incomplete: !b.is_final,
                    });
                }
            } else {
                for c in 0..cols {
                    let b_start = c * bars.len() / cols;
                    let b_end = ((c + 1) * bars.len() / cols).min(bars.len());
                    if b_start >= b_end {
                        continue;
                    }
                    let bucket = &bars[b_start..b_end];
                    let first = &bucket[0];
                    let last = &bucket[bucket.len() - 1];
                    let open = first.open.to_f64().unwrap_or(0.0);
                    let close = last.close.to_f64().unwrap_or(0.0);
                    let mut high = f64::NEG_INFINITY;
                    let mut low = f64::INFINITY;
                    let mut volume = 0.0;
                    let mut has_gap_before = false;
                    let mut is_incomplete = false;

                    for (offset, b) in bucket.iter().enumerate() {
                        let global_idx = start + b_start + offset;
                        let h = b.high.to_f64().unwrap_or(0.0);
                        let l = b.low.to_f64().unwrap_or(0.0);
                        if h > high {
                            high = h;
                        }
                        if l < low {
                            low = l;
                        }
                        volume += b.volume.to_f64().unwrap_or(0.0);
                        if gap_set.contains(&global_idx) {
                            has_gap_before = true;
                        }
                        if !b.is_final {
                            is_incomplete = true;
                        }
                    }

                    lod_candles.push(LodCandleDto {
                        start_bar_index: start + b_start,
                        end_bar_index: start + b_end - 1,
                        session_date: last.session_date,
                        open,
                        high,
                        low,
                        close,
                        volume,
                        has_gap_before,
                        is_incomplete,
                    });
                }
            }

            let mut low_min = f64::INFINITY;
            let mut high_max = f64::NEG_INFINITY;
            for b in &bars {
                let l = b.low.to_f64().unwrap_or(0.0);
                let h = b.high.to_f64().unwrap_or(0.0);
                if l < low_min {
                    low_min = l;
                }
                if h > high_max {
                    high_max = h;
                }
            }
            if !low_min.is_finite() || !high_max.is_finite() {
                low_min = 0.0;
                high_max = 1.0;
            }
            let price_span = (high_max - low_min).max(1e-9);

            let mut stack_counts: HashMap<usize, u32> = HashMap::new();
            let mut markers = Vec::new();
            for m in raw_markers {
                if m.bar_index < start || m.bar_index >= end || m.bar_index >= total_vis {
                    continue;
                }
                if let Some(cutoff) = visible_until {
                    if m.session_date > cutoff.date_naive() {
                        continue;
                    }
                }
                let slot = stack_counts.entry(m.bar_index).or_insert(0);
                let current_slot = *slot;
                *slot += 1;

                let x_ratio = if bars.len() <= 1 {
                    0.5
                } else {
                    ((m.bar_index - start) as f32) / ((bars.len() - 1) as f32)
                }
                .clamp(0.0, 1.0);

                let p = m.price.to_f64().unwrap_or(low_min);
                let y_ratio = (((p - low_min) / price_span) as f32).clamp(0.0, 1.0);

                markers.push(ChartMarkerLayout {
                    marker: m.clone(),
                    x_ratio,
                    y_ratio,
                    stack_slot: current_slot,
                });
            }

            let timezone_label = if is_crypto {
                "UTC".to_string()
            } else {
                "America/New_York (ET) / UTC".to_string()
            };

            KlineWindowSliceDto {
                instrument_id: instrument_id.to_string(),
                venue: venue.to_string(),
                timeframe: timeframe.to_string(),
                adjustment,
                visible_start_index: start,
                visible_end_index: end,
                total_visible_bars: bars.len(),
                bars,
                lod_candles,
                sma_series,
                ema_series,
                macd_series,
                rsi_series,
                markers,
                gap_indices,
                has_incomplete_tail: cached.has_incomplete_tail,
                is_stale: false,
                timezone_label,
            }
        })
    }
}

/// Run the `M2-K02` 100,000-bar scripted zoom/pan benchmark (300 frames, 1,000 visible bars).
/// Returns `(p50_us, p95_us, total_bars)`.
pub fn synthetic_benchmark_bars(total_bars: usize) -> Vec<BarRecord> {
    let base_date = NaiveDate::from_ymd_opt(1900, 1, 1).expect("valid date");
    let mut bars = Vec::with_capacity(total_bars);

    for i in 0..total_bars {
        let d = base_date + chrono::Days::new(i as u64);
        let open_at = Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).expect("valid hms"));
        let close_at = Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).expect("valid hms"));
        let price = Decimal::from(100 + ((i % 250) as i64));
        bars.push(BarRecord {
            id: format!("BENCH-{i}"),
            instrument_id: "NASDAQ:AAPL".to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date: d,
            open_at,
            close_at,
            open: price,
            high: price + Decimal::TWO,
            low: price - Decimal::ONE,
            close: price + Decimal::ONE,
            volume: Decimal::from(10_000 + (i % 500) as i64),
            source: "bench".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }
    bars
}

pub fn run_m2_k02_viewport_bench() -> (u64, u64, usize) {
    let total_bars = 100_000usize;
    let bars = synthetic_benchmark_bars(total_bars);

    let mut vp = KlineViewport::new(total_bars, 1_000);
    // Warm up full-history indicator cache once before scripted 300-frame viewport interaction loop
    let _ = vp.build_window_slice(
        "NASDAQ:AAPL",
        "NASDAQ",
        "1d",
        AdjustmentKind::Raw,
        &bars,
        None,
        &[],
        &[],
        320,
    );

    let mut frame_durations_us = Vec::with_capacity(300);
    for frame in 0..300 {
        let t0 = Instant::now();
        if frame % 3 == 0 {
            vp.pan(-25);
        } else if frame % 3 == 1 {
            vp.zoom(0.95, 0.5);
        } else {
            vp.zoom(1.0526315, 0.5);
        }
        let slice = vp.build_window_slice(
            "NASDAQ:AAPL",
            "NASDAQ",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            None,
            &[],
            &[],
            320,
        );
        std::hint::black_box(&slice);
        frame_durations_us.push(t0.elapsed().as_micros() as u64);
    }

    frame_durations_us.sort_unstable();
    let p50_us = frame_durations_us[frame_durations_us.len() * 50 / 100];
    let p95_us = frame_durations_us[frame_durations_us.len() * 95 / 100];
    (p50_us, p95_us, total_bars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_core::parse_decimal_lossless;

    #[test]
    fn last_close_change_is_decimal_and_honest_without_previous_session() {
        let mut bars = synthetic_benchmark_bars(2);
        bars[0].close = parse_decimal_lossless("110").unwrap();
        bars[1].close = parse_decimal_lossless("105").unwrap();
        let mut slice = KlineWindowSliceDto {
            instrument_id: "NASDAQ:AAPL".into(),
            venue: "NASDAQ".into(),
            timeframe: "1d".into(),
            adjustment: AdjustmentKind::Raw,
            visible_start_index: 0,
            visible_end_index: 2,
            total_visible_bars: 2,
            bars,
            lod_candles: Vec::new(),
            sma_series: Vec::new(),
            ema_series: Vec::new(),
            macd_series: Vec::new(),
            rsi_series: Vec::new(),
            markers: Vec::new(),
            gap_indices: Vec::new(),
            has_incomplete_tail: false,
            is_stale: false,
            timezone_label: "UTC".into(),
        };
        let change = slice.last_close_change().unwrap();
        assert_eq!(change.close.to_string(), "105");
        assert_eq!(change.change.unwrap().to_string(), "-5");
        assert_eq!(change.change_pct.unwrap().to_string(), "-4.55");
        slice.bars.remove(0);
        let single = slice.last_close_change().unwrap();
        assert_eq!((single.change, single.change_pct), (None, None));
        slice.bars.clear();
        assert!(slice.last_close_change().is_none());
    }

    #[test]
    fn test_m2_a10_kline_viewport_100k_bars_window_invariance_and_future_sentinel() {
        let base_date = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let mut bars = Vec::with_capacity(505);
        let gap_skip_date = base_date + chrono::Days::new(200);

        let mut day_cursor = 0u64;
        for idx in 0..500usize {
            let mut d = base_date + chrono::Days::new(day_cursor);
            if d == gap_skip_date {
                // Inject a 6-day trading gap
                day_cursor += 6;
                d = base_date + chrono::Days::new(day_cursor);
            }
            day_cursor += 1;

            let open_at = Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap());
            let close_at = Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap());
            let close_price = Decimal::from(100 + ((idx * 3) % 70) as i64);
            bars.push(BarRecord {
                id: format!("BAR-{idx}"),
                instrument_id: "NASDAQ:AAPL".to_string(),
                timeframe: "1d".to_string(),
                session: "regular".to_string(),
                session_date: d,
                open_at,
                close_at,
                open: close_price - Decimal::ONE,
                high: close_price + Decimal::TWO,
                low: close_price - Decimal::TWO,
                close: close_price,
                volume: Decimal::from(5_000 + idx as i64),
                source: "test".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            });
        }

        let cutoff_date = bars[499].session_date;
        let visible_until =
            Some(Utc.from_utc_datetime(&cutoff_date.and_hms_opt(21, 0, 0).unwrap()));

        // Add an unclosed bar at cutoff_date (is_final = false)
        bars.push(BarRecord {
            id: "BAR-UNCLOSED".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date: cutoff_date,
            open_at: Utc.from_utc_datetime(&cutoff_date.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&cutoff_date.and_hms_opt(21, 0, 0).unwrap()),
            open: Decimal::from(888_888),
            high: Decimal::from(888_888),
            low: Decimal::from(888_888),
            close: Decimal::from(888_888),
            volume: Decimal::from(100),
            source: "test".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: false,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        });

        // Add an extreme future sentinel bar after visible_until (999_999_999)
        let future_date = cutoff_date + chrono::Days::new(1);
        bars.push(BarRecord {
            id: "BAR-FUTURE-SENTINEL".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date: future_date,
            open_at: Utc.from_utc_datetime(&future_date.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&future_date.and_hms_opt(21, 0, 0).unwrap()),
            open: Decimal::from(999_999_999_i64),
            high: Decimal::from(999_999_999_i64),
            low: Decimal::from(999_999_999_i64),
            close: Decimal::from(999_999_999_i64),
            volume: Decimal::from(999_999_999_i64),
            source: "test".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        });

        let target_idx = 220usize;
        let target_date = bars[target_idx].session_date;

        let raw_markers = vec![
            ChartMarkerDto {
                id: "M-BUY-1".to_string(),
                kind: MarkerKind::BuyFill,
                session_date: target_date,
                bar_index: target_idx,
                price: parse_decimal_lossless("125.50").unwrap(),
                label: "Buy 10 @ 125.50".to_string(),
                target_id: "FILL-1".to_string(),
                target_revision: 1,
            },
            ChartMarkerDto {
                id: "M-NOTE-1".to_string(),
                kind: MarkerKind::Note,
                session_date: target_date,
                bar_index: target_idx,
                price: parse_decimal_lossless("126.00").unwrap(),
                label: "Earnings Note".to_string(),
                target_id: "NOTE-1".to_string(),
                target_revision: 2,
            },
            ChartMarkerDto {
                id: "M-FUTURE".to_string(),
                kind: MarkerKind::Plan,
                session_date: future_date,
                bar_index: 500,
                price: Decimal::from(999_999_999_i64),
                label: "Future Sentinel Marker".to_string(),
                target_id: "PLAN-FUTURE".to_string(),
                target_revision: 1,
            },
        ];

        // Window A: [0..350] (includes warmup at index 0 and target_idx = 220)
        let mut vp_a = KlineViewport::new(500, 350);
        vp_a.visible_start = 0;
        vp_a.visible_end = 350;
        let slice_a = vp_a.build_window_slice(
            "NASDAQ:AAPL",
            "NASDAQ",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            visible_until,
            &raw_markers,
            &[gap_skip_date],
            120,
        );

        // Window B: [150..280] (narrower window also covering target_idx = 220)
        let mut vp_b = KlineViewport::new(500, 130);
        vp_b.visible_start = 150;
        vp_b.visible_end = 280;
        let slice_b = vp_b.build_window_slice(
            "NASDAQ:AAPL",
            "NASDAQ",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            visible_until,
            &raw_markers,
            &[gap_skip_date],
            120,
        );

        // Verify warmup points at index 0 are None in Window A
        assert_eq!(slice_a.sma_series[0].1[0].value, None);
        assert_eq!(slice_a.ema_series[0].1[0].value, None);
        assert_eq!(slice_a.macd_series[0].macd, None);
        assert_eq!(slice_a.rsi_series[0].value, None);

        // Verify bit-for-bit indicator invariance at target_date between Window A and Window B
        let idx_in_a = target_idx;
        let idx_in_b = target_idx - 150;
        assert_eq!(
            slice_a.bars[idx_in_a].session_date,
            slice_b.bars[idx_in_b].session_date
        );
        for ((p_a, s_a), (p_b, s_b)) in slice_a.sma_series.iter().zip(slice_b.sma_series.iter()) {
            assert_eq!(p_a, p_b);
            assert_eq!(
                s_a[idx_in_a].value.unwrap().to_bits(),
                s_b[idx_in_b].value.unwrap().to_bits()
            );
        }
        for ((p_a, s_a), (p_b, s_b)) in slice_a.ema_series.iter().zip(slice_b.ema_series.iter()) {
            assert_eq!(p_a, p_b);
            assert_eq!(
                s_a[idx_in_a].value.unwrap().to_bits(),
                s_b[idx_in_b].value.unwrap().to_bits()
            );
        }
        assert_eq!(
            slice_a.macd_series[idx_in_a].macd.unwrap().to_bits(),
            slice_b.macd_series[idx_in_b].macd.unwrap().to_bits()
        );
        assert_eq!(
            slice_a.macd_series[idx_in_a].signal.unwrap().to_bits(),
            slice_b.macd_series[idx_in_b].signal.unwrap().to_bits()
        );
        assert_eq!(
            slice_a.rsi_series[idx_in_a].value.unwrap().to_bits(),
            slice_b.rsi_series[idx_in_b].value.unwrap().to_bits()
        );

        // Verify future sentinel (999_999_999) and unclosed bar (888_888) did not leak into tail window
        let vp_tail = KlineViewport::new(500, 100);
        let slice_tail = vp_tail.build_window_slice(
            "NASDAQ:AAPL",
            "NASDAQ",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            visible_until,
            &raw_markers,
            &[gap_skip_date],
            100,
        );
        assert!(slice_tail.has_incomplete_tail);
        assert_eq!(slice_tail.total_visible_bars, 100);
        assert!(slice_tail
            .bars
            .iter()
            .all(|b| b.close < Decimal::from(10_000) && b.is_final));
        assert!(slice_tail.sma_series[0].1.last().unwrap().value.unwrap() < 1000.0);
        assert!(slice_tail.markers.iter().all(|m| m.marker.id != "M-FUTURE"));

        // Verify gap detection & LOD bucketing in slice_a (350 bars into 120 columns)
        assert!(!slice_a.gap_indices.is_empty());
        assert_eq!(slice_a.lod_candles.len(), 120);
        assert!(slice_a.lod_candles.iter().any(|c| c.has_gap_before));

        // Verify marker stacking & hit testing in slice_b
        assert_eq!(slice_b.markers.len(), 2);
        assert_eq!(slice_b.markers[0].stack_slot, 0);
        assert_eq!(slice_b.markers[1].stack_slot, 1);
        let hit = vp_b
            .hit_test_marker(
                &slice_b.markers,
                slice_b.markers[0].x_ratio,
                slice_b.markers[0].y_ratio,
                0.05,
            )
            .expect("marker hit");
        assert_eq!(hit.id, "M-BUY-1");

        // Verify zoom, pan, jump_to_date centering (M2-A-11), crosshair & bar hit-test
        let mut vp_nav = KlineViewport::new(500, 100);
        assert_eq!(vp_nav.visible_range(), 400..500);
        vp_nav.pan(-50);
        assert_eq!(vp_nav.visible_range(), 350..450);
        vp_nav.zoom(0.5, 0.5);
        assert_eq!(vp_nav.visible_len(), 50);

        let jumped = vp_nav
            .jump_to_date(&bars[..500], target_date)
            .expect("jump to target_date");
        assert_eq!(jumped, target_idx);
        assert_eq!(vp_nav.crosshair_index, Some(target_idx));
        let center = (vp_nav.visible_start + vp_nav.visible_end) / 2;
        assert!((center as isize - target_idx as isize).abs() <= 2);

        vp_nav.move_crosshair(Some(225), Some(130.5));
        assert_eq!(vp_nav.crosshair_index, Some(225));
        assert_eq!(vp_nav.crosshair_price, Some(130.5));
        assert!(vp_nav.hit_test_bar(0.5).is_some());

        // Verify 100k-bar scripted benchmark (M2-K02)
        let (p50_us, p95_us, total_bench_bars) = run_m2_k02_viewport_bench();
        assert_eq!(total_bench_bars, 100_000);
        assert!(p50_us <= p95_us);
        assert!(p95_us < 50_000);
    }
}
