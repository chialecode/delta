use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::America::New_York;
use delta_core::{CalendarSession, CalendarSessionKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub const SPECIAL_TABLE_JSON: &str = include_str!("calendar_special_xnys.json");

pub const VERIFIED_START: NaiveDate = match NaiveDate::from_ymd_opt(1990, 1, 2) {
    Some(d) => d,
    None => panic!("invalid start date"),
};

pub const VERIFIED_END: NaiveDate = match NaiveDate::from_ymd_opt(2027, 12, 31) {
    Some(d) => d,
    None => panic!("invalid end date"),
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpecialClosingEntry {
    pub date: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpecialEarlyCloseEntry {
    pub date: String,
    pub close_et: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpecialCalendarTable {
    pub schema_version: u32,
    pub venue: String,
    pub aliases: Vec<String>,
    pub adhoc_closings: Vec<SpecialClosingEntry>,
    pub adhoc_early_closes: Vec<SpecialEarlyCloseEntry>,
}

pub struct ParsedSpecialTable {
    pub sha256: String,
    pub version_tag: String,
    pub adhoc_closings: HashSet<NaiveDate>,
    pub adhoc_early_closes: HashMap<NaiveDate, NaiveTime>,
}

static SPECIAL_TABLE: OnceLock<ParsedSpecialTable> = OnceLock::new();

pub fn special_table() -> &'static ParsedSpecialTable {
    SPECIAL_TABLE.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(SPECIAL_TABLE_JSON.as_bytes());
        let sha256 = hex::encode(hasher.finalize());
        let version_tag = format!("xnys-v1-{}", &sha256[..12]);

        let raw: SpecialCalendarTable = serde_json::from_str(SPECIAL_TABLE_JSON)
            .expect("valid embedded calendar_special_xnys.json");
        let mut adhoc_closings = HashSet::new();
        for item in raw.adhoc_closings {
            let d = NaiveDate::parse_from_str(&item.date, "%Y-%m-%d")
                .expect("valid adhoc closing date");
            adhoc_closings.insert(d);
        }
        let mut adhoc_early_closes = HashMap::new();
        for item in raw.adhoc_early_closes {
            let d = NaiveDate::parse_from_str(&item.date, "%Y-%m-%d")
                .expect("valid adhoc early close date");
            let t = NaiveTime::parse_from_str(&item.close_et, "%H:%M:%S")
                .expect("valid adhoc early close time");
            adhoc_early_closes.insert(d, t);
        }
        ParsedSpecialTable {
            sha256,
            version_tag,
            adhoc_closings,
            adhoc_early_closes,
        }
    })
}

/// Anonymous Gregorian algorithm for Easter Sunday in a given year.
pub fn easter_sunday(year: i32) -> NaiveDate {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = ((h + l - 7 * m + 114) / 31) as u32;
    let day = (((h + l - 7 * m + 114) % 31) + 1) as u32;
    NaiveDate::from_ymd_opt(year, month, day).expect("valid Easter date")
}

pub fn good_friday(year: i32) -> NaiveDate {
    easter_sunday(year)
        .checked_sub_days(chrono::Days::new(2))
        .expect("valid Good Friday")
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month start");
    let offset = (7 + weekday.num_days_from_monday() as i64
        - first.weekday().num_days_from_monday() as i64)
        % 7;
    let day = 1 + offset as u32 + (n - 1) * 7;
    NaiveDate::from_ymd_opt(year, month, day).expect("valid nth weekday")
}

fn last_monday_of_may(year: i32) -> NaiveDate {
    let may31 = NaiveDate::from_ymd_opt(year, 5, 31).expect("valid May 31");
    let offset = (7 + may31.weekday().num_days_from_monday() as i64
        - Weekday::Mon.num_days_from_monday() as i64)
        % 7;
    may31
        .checked_sub_days(chrono::Days::new(offset as u64))
        .expect("valid Memorial Day")
}

fn nearest_workday(date: NaiveDate) -> NaiveDate {
    match date.weekday() {
        Weekday::Sat => date
            .checked_sub_days(chrono::Days::new(1))
            .expect("Friday before Saturday"),
        Weekday::Sun => date
            .checked_add_days(chrono::Days::new(1))
            .expect("Monday after Sunday"),
        _ => date,
    }
}

/// Check if a weekday in `1990..=2027` is a regular or ad-hoc full holiday on XNYS/XNAS.
pub fn is_us_equity_holiday(date: NaiveDate) -> bool {
    let year = date.year();
    let table = special_table();
    if table.adhoc_closings.contains(&date) {
        return true;
    }

    // 1. New Year's Day (sunday_to_monday observance)
    let jan1 = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    let nyd_observed = if jan1.weekday() == Weekday::Sun {
        NaiveDate::from_ymd_opt(year, 1, 2).unwrap()
    } else {
        jan1
    };
    if date == nyd_observed {
        return true;
    }

    // 2. MLK Day (3rd Monday in January, year >= 1998)
    if year >= 1998 && date == nth_weekday_of_month(year, 1, Weekday::Mon, 3) {
        return true;
    }

    // 3. Presidents' Day (3rd Monday in February, year >= 1971)
    if date == nth_weekday_of_month(year, 2, Weekday::Mon, 3) {
        return true;
    }

    // 4. Good Friday
    if date == good_friday(year) {
        return true;
    }

    // 5. Memorial Day (last Monday in May)
    if date == last_monday_of_may(year) {
        return true;
    }

    // 6. Juneteenth (June 19, nearest workday, year >= 2022)
    if year >= 2022 {
        let jun19 = NaiveDate::from_ymd_opt(year, 6, 19).unwrap();
        if date == nearest_workday(jun19) {
            return true;
        }
    }

    // 7. Independence Day (July 4, nearest workday)
    let jul4 = NaiveDate::from_ymd_opt(year, 7, 4).unwrap();
    if date == nearest_workday(jul4) {
        return true;
    }

    // 8. Labor Day (1st Monday in September)
    if date == nth_weekday_of_month(year, 9, Weekday::Mon, 1) {
        return true;
    }

    // 9. Thanksgiving Day (4th Thursday in November)
    if date == nth_weekday_of_month(year, 11, Weekday::Thu, 4) {
        return true;
    }

    // 10. Christmas Day (Dec 25, nearest workday)
    let dec25 = NaiveDate::from_ymd_opt(year, 12, 25).unwrap();
    if date == nearest_workday(dec25) {
        return true;
    }

    false
}

/// If `date` is an early close session on XNYS/XNAS, returns the local ET close time
/// (`14:00:00` before 1993, `13:00:00` in/after 1993).
pub fn us_equity_early_close_time(date: NaiveDate) -> Option<NaiveTime> {
    let year = date.year();
    let table = special_table();
    if let Some(t) = table.adhoc_early_closes.get(&date) {
        return Some(*t);
    }

    let std_early = if year < 1993 {
        NaiveTime::from_hms_opt(14, 0, 0).unwrap()
    } else {
        NaiveTime::from_hms_opt(13, 0, 0).unwrap()
    };

    // 1. Independence Day early closes (since 1995)
    if year >= 1995 && date.month() == 7 {
        if date.day() == 3 {
            match date.weekday() {
                Weekday::Mon | Weekday::Tue | Weekday::Thu => return Some(std_early),
                Weekday::Wed if year >= 2013 => return Some(std_early),
                _ => {}
            }
        }
        if date.day() == 5 && year < 2013 && date.weekday() == Weekday::Fri {
            return Some(std_early);
        }
    }

    // 2. Day after Thanksgiving (Black Friday, observed since 1992)
    if year >= 1992 && date.month() == 11 {
        let thanksgiving = nth_weekday_of_month(year, 11, Weekday::Thu, 4);
        let black_friday = thanksgiving.checked_add_days(chrono::Days::new(1)).unwrap();
        if date == black_friday {
            return Some(std_early);
        }
    }

    // 3. Christmas Eve (Dec 24 on Mon, Tue, Wed, Thu)
    if date.month() == 12
        && date.day() == 24
        && matches!(
            date.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu
        )
    {
        return Some(std_early);
    }

    None
}

/// Query a single calendar session for `venue` (`XNYS`, `XNAS`, `NYSE`, `NASDAQ`, or `CRYPTO_24_7`).
pub fn query_calendar_session(venue: &str, session_date: NaiveDate) -> CalendarSession {
    let norm_venue = venue.to_ascii_uppercase();
    if norm_venue == "CRYPTO_24_7" || norm_venue == "OKX" || norm_venue == "BINANCE" {
        let open_at = Utc.from_utc_datetime(&session_date.and_hms_opt(0, 0, 0).unwrap());
        let next_day = session_date
            .checked_add_days(chrono::Days::new(1))
            .unwrap_or(session_date);
        let close_at = Utc.from_utc_datetime(&next_day.and_hms_opt(0, 0, 0).unwrap());
        return CalendarSession {
            venue: "CRYPTO_24_7".to_string(),
            session_date,
            timezone: "UTC".to_string(),
            open_at: Some(open_at),
            close_at: Some(close_at),
            kind: CalendarSessionKind::Crypto24x7,
            version: "crypto-utc-v1".to_string(),
        };
    }

    let canonical_venue = if norm_venue == "XNAS" || norm_venue == "NASDAQ" {
        "XNAS"
    } else {
        "XNYS"
    };
    let table = special_table();

    if session_date < VERIFIED_START || session_date > VERIFIED_END {
        return CalendarSession {
            venue: canonical_venue.to_string(),
            session_date,
            timezone: "America/New_York".to_string(),
            open_at: None,
            close_at: None,
            kind: CalendarSessionKind::CalendarUnknown,
            version: table.version_tag.clone(),
        };
    }

    if matches!(session_date.weekday(), Weekday::Sat | Weekday::Sun)
        || is_us_equity_holiday(session_date)
    {
        return CalendarSession {
            venue: canonical_venue.to_string(),
            session_date,
            timezone: "America/New_York".to_string(),
            open_at: None,
            close_at: None,
            kind: CalendarSessionKind::Closed,
            version: table.version_tag.clone(),
        };
    }

    let open_local = session_date.and_hms_opt(9, 30, 0).unwrap();
    let (close_local, kind) = match us_equity_early_close_time(session_date) {
        Some(early_time) => (
            session_date.and_time(early_time),
            CalendarSessionKind::EarlyClose,
        ),
        None => (
            session_date.and_hms_opt(16, 0, 0).unwrap(),
            CalendarSessionKind::Regular,
        ),
    };

    let open_utc = New_York
        .from_local_datetime(&open_local)
        .single()
        .expect("unambiguous NY open time")
        .with_timezone(&Utc);
    let close_utc = New_York
        .from_local_datetime(&close_local)
        .single()
        .expect("unambiguous NY close time")
        .with_timezone(&Utc);

    CalendarSession {
        venue: canonical_venue.to_string(),
        session_date,
        timezone: "America/New_York".to_string(),
        open_at: Some(open_utc),
        close_at: Some(close_utc),
        kind,
        version: table.version_tag.clone(),
    }
}

/// Generate all calendar sessions in `[start, end]` inclusive for `venue`.
pub fn generate_calendar_range(
    venue: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> Vec<CalendarSession> {
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        out.push(query_calendar_session(venue, cur));
        match cur.checked_add_days(chrono::Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    out
}

/// Convert a UTC timestamp to the exchange's local `session_date` (S-06: US equities always use
/// `America/New_York` local date, never inferred blindly from UTC date).
pub fn resolve_exchange_session_date(venue: &str, timestamp_utc: DateTime<Utc>) -> NaiveDate {
    let norm = venue.to_ascii_uppercase();
    if norm == "CRYPTO_24_7" || norm == "OKX" || norm == "BINANCE" {
        timestamp_utc.date_naive()
    } else {
        timestamp_utc.with_timezone(&New_York).date_naive()
    }
}
