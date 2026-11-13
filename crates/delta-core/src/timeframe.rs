//! Bar timeframes for training data (`S-50`): crypto `1d`/`4h`/`1h`, US equity/ETF `1d` only.
//! Every crypto bar is aligned to UTC (1h on the hour, 4h at 00/04/08/12/16/20, 1d at 00:00).

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum Timeframe {
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "4h")]
    H4,
    #[default]
    #[serde(rename = "1d")]
    D1,
}

impl Timeframe {
    pub const ALL: [Timeframe; 3] = [Timeframe::D1, Timeframe::H4, Timeframe::H1];

    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::H1 => "1h",
            Timeframe::H4 => "4h",
            Timeframe::D1 => "1d",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1h" | "60m" => Some(Timeframe::H1),
            "4h" | "240m" => Some(Timeframe::H4),
            "1d" | "d" | "day" | "1dutc" => Some(Timeframe::D1),
            _ => None,
        }
    }

    pub fn duration(self) -> Duration {
        match self {
            Timeframe::H1 => Duration::hours(1),
            Timeframe::H4 => Duration::hours(4),
            Timeframe::D1 => Duration::days(1),
        }
    }

    pub fn millis(self) -> i64 {
        self.duration().num_milliseconds()
    }

    pub fn is_intraday(self) -> bool {
        self != Timeframe::D1
    }

    /// Binance `interval` parameter and archive path segment.
    pub fn binance_interval(self) -> &'static str {
        self.as_str()
    }

    /// OKX `bar` parameter. Daily must use the UTC variant (`1Dutc`); 4H boundaries coincide in
    /// UTC and UTC+8, 1H is unambiguous.
    pub fn okx_bar(self) -> &'static str {
        match self {
            Timeframe::H1 => "1H",
            Timeframe::H4 => "4H",
            Timeframe::D1 => "1Dutc",
        }
    }

    /// True when `open_at` sits on this timeframe's UTC grid.
    pub fn is_aligned(self, open_at: DateTime<Utc>) -> bool {
        if open_at.nanosecond() != 0 || open_at.second() != 0 || open_at.minute() != 0 {
            return false;
        }
        match self {
            Timeframe::H1 => true,
            Timeframe::H4 => open_at.hour().is_multiple_of(4),
            Timeframe::D1 => open_at.hour() == 0,
        }
    }

    /// Floor a UTC instant to the start of the enclosing bar of this timeframe.
    pub fn floor(self, at: DateTime<Utc>) -> DateTime<Utc> {
        let ms = at.timestamp_millis();
        let step = self.millis();
        let floored = ms.div_euclid(step) * step;
        DateTime::<Utc>::from_timestamp_millis(floored).unwrap_or(at)
    }

    /// Only timeframes strictly larger than `self` can be viewed as an aggregate of `self` bars.
    pub fn higher_views(self) -> Vec<Timeframe> {
        match self {
            Timeframe::H1 => vec![Timeframe::H4, Timeframe::D1],
            Timeframe::H4 => vec![Timeframe::D1],
            Timeframe::D1 => Vec::new(),
        }
    }

    /// Number of `self` bars that make up one bar of `higher` (`None` if not an aggregate).
    pub fn bars_per(self, higher: Timeframe) -> Option<i64> {
        if higher.millis() <= self.millis() || higher.millis() % self.millis() != 0 {
            return None;
        }
        Some(higher.millis() / self.millis())
    }

    /// UTC calendar date that a bar opening at `open_at` belongs to.
    pub fn session_date(self, open_at: DateTime<Utc>) -> NaiveDate {
        open_at.date_naive()
    }
}

impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One higher-timeframe bar aggregated from base bars (`S-50` "view higher timeframe").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatedBar {
    pub open_at: DateTime<Utc>,
    pub close_at: DateTime<Utc>,
    pub open: rust_decimal::Decimal,
    pub high: rust_decimal::Decimal,
    pub low: rust_decimal::Decimal,
    pub close: rust_decimal::Decimal,
    pub volume: rust_decimal::Decimal,
    /// Number of base bars that went into this aggregate.
    pub base_bar_count: usize,
    /// `false` while the higher bar is still forming (its last base bar is not yet visible).
    pub complete: bool,
}

/// Aggregate already truncated base bars into `higher` bars. Only the bars passed in are used,
/// so a caller that passes the host-truncated visible bars can never leak a future price.
pub fn aggregate_bars(
    bars: &[crate::BarRecord],
    base: Timeframe,
    higher: Timeframe,
) -> Vec<AggregatedBar> {
    let Some(per) = base.bars_per(higher) else {
        return Vec::new();
    };
    let mut out: Vec<AggregatedBar> = Vec::new();
    for b in bars {
        let bucket = higher.floor(b.open_at);
        match out.last_mut() {
            Some(last) if last.open_at == bucket => {
                last.high = last.high.max(b.high);
                last.low = last.low.min(b.low);
                last.close = b.close;
                last.volume += b.volume;
                last.base_bar_count += 1;
                last.complete = b.close_at >= last.close_at;
            }
            _ => out.push(AggregatedBar {
                open_at: bucket,
                close_at: bucket + higher.duration(),
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
                volume: b.volume,
                base_bar_count: 1,
                complete: b.close_at >= bucket + higher.duration(),
            }),
        }
    }
    for a in &mut out {
        // A bucket with a missing base bar is still complete once its period has closed.
        a.complete = a.complete || a.base_bar_count as i64 == per;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_and_alignment() {
        assert_eq!(Timeframe::parse("4H"), Some(Timeframe::H4));
        assert_eq!(Timeframe::parse("1Dutc"), Some(Timeframe::D1));
        assert_eq!(Timeframe::parse("15m"), None);
        let t = Utc.with_ymd_and_hms(2017, 8, 17, 4, 0, 0).unwrap();
        assert!(Timeframe::H1.is_aligned(t));
        assert!(Timeframe::H4.is_aligned(t));
        assert!(!Timeframe::D1.is_aligned(t));
        let t2 = Utc.with_ymd_and_hms(2017, 8, 17, 5, 0, 0).unwrap();
        assert!(!Timeframe::H4.is_aligned(t2));
        assert_eq!(
            Timeframe::H4.floor(t2),
            Utc.with_ymd_and_hms(2017, 8, 17, 4, 0, 0).unwrap()
        );
        assert_eq!(Timeframe::H1.bars_per(Timeframe::D1), Some(24));
        assert_eq!(Timeframe::H4.bars_per(Timeframe::D1), Some(6));
        assert_eq!(Timeframe::D1.bars_per(Timeframe::H4), None);
        assert_eq!(
            serde_json::to_string(&Timeframe::H4).unwrap(),
            "\"4h\"".to_string()
        );
    }

    #[test]
    fn aggregate_marks_the_forming_bar_incomplete() {
        use crate::{AdjustmentKind, BarRecord};
        use rust_decimal::Decimal;
        let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let bars: Vec<BarRecord> = (0..6)
            .map(|i| {
                let open_at = start + Duration::hours(i);
                let px = Decimal::from(100 + i);
                BarRecord {
                    id: format!("b{i}"),
                    instrument_id: "X".into(),
                    timeframe: "1h".into(),
                    session: "24x7".into(),
                    session_date: open_at.date_naive(),
                    open_at,
                    close_at: open_at + Duration::hours(1),
                    open: px,
                    high: px + Decimal::ONE,
                    low: px - Decimal::ONE,
                    close: px,
                    volume: Decimal::ONE,
                    source: "t".into(),
                    adjustment: AdjustmentKind::Raw,
                    is_final: true,
                    valid_from_version: 1,
                    valid_to_version: None,
                    raw_payload_hash: None,
                }
            })
            .collect();
        let agg = aggregate_bars(&bars, Timeframe::H1, Timeframe::H4);
        assert_eq!(agg.len(), 2);
        assert!(agg[0].complete);
        assert_eq!(agg[0].open, Decimal::from(100));
        assert_eq!(agg[0].high, Decimal::from(104));
        assert_eq!(agg[0].low, Decimal::from(99));
        assert_eq!(agg[0].close, Decimal::from(103));
        assert_eq!(agg[0].volume, Decimal::from(4));
        assert!(!agg[1].complete, "only 2 of 4 hours are visible");
        assert_eq!(agg[1].close, Decimal::from(105));
    }
}
