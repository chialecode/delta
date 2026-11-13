//! Loopback market fixture (`M5-A-01`, `S-57`): serves Binance monthly kline archives with
//! `.CHECKSUM`, Binance mirror `/api/v3/klines` and OKX `/api/v5/market/history-candles` with the
//! same wire structure as the real public endpoints. Prices are a deterministic synthetic hourly
//! walk aggregated to 4h/1d, so every timeframe and both venues agree. Used by unit tests and by
//! `verify-stage --stage M5`; results from it are declared as fixture results, never as live ones.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use delta_core::Timeframe;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Binance BTCUSDT first kline (2017-08-17 04:00 UTC), measured against the live mirror.
pub const BINANCE_BTC_FIRST_OPEN_MS: i64 = 1_502_942_400_000;
/// OKX BTC-USDT history used by the fixture (first bar 2018-01-11 00:00 UTC).
pub const OKX_BTC_FIRST_OPEN_MS: i64 = 1_515_628_800_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureFault {
    /// HTTP 500 (a network/server failure).
    ServerError,
    /// HTTP 429 with `Retry-After: 0`.
    RateLimited,
    /// Serve the archive with a `.CHECKSUM` that does not match.
    ChecksumMismatch,
}

#[derive(Debug, Clone)]
struct FaultRule {
    path_contains: String,
    remaining: usize,
    fault: FixtureFault,
}

#[derive(Debug, Clone, Copy)]
struct HourBar {
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    volume: Decimal,
}

struct FixtureState {
    hours: Vec<HourBar>,
    faults: Vec<FaultRule>,
    unavailable_archive_months: Vec<String>,
    log: Vec<String>,
}

/// A running loopback server; dropping it stops accepting connections.
pub struct MarketFixtureServer {
    base_url: String,
    /// Exclusive end (ms) of finalized hourly data.
    end_ms: Arc<AtomicI64>,
    delay_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<FixtureState>>,
    requests: Arc<AtomicU64>,
}

impl MarketFixtureServer {
    /// Start serving BTC data from the Binance listing hour until `end` (floored to the hour).
    pub fn start(end: DateTime<Utc>) -> std::io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let end_ms = Timeframe::H1.floor(end).timestamp_millis();
        let hours = generate_hours(end_ms + Duration::days(40).num_milliseconds());
        let state = Arc::new(Mutex::new(FixtureState {
            hours,
            faults: Vec::new(),
            unavailable_archive_months: Vec::new(),
            log: Vec::new(),
        }));
        let server = Self {
            base_url,
            end_ms: Arc::new(AtomicI64::new(end_ms)),
            delay_ms: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            state,
            requests: Arc::new(AtomicU64::new(0)),
        };
        let (end_c, delay_c, stop_c, state_c, req_c) = (
            Arc::clone(&server.end_ms),
            Arc::clone(&server.delay_ms),
            Arc::clone(&server.stop),
            Arc::clone(&server.state),
            Arc::clone(&server.requests),
        );
        std::thread::spawn(move || {
            while !stop_c.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let (end_c, delay_c, state_c, req_c) = (
                            Arc::clone(&end_c),
                            Arc::clone(&delay_c),
                            Arc::clone(&state_c),
                            Arc::clone(&req_c),
                        );
                        std::thread::spawn(move || {
                            let _ = handle(socket, &end_c, &delay_c, &state_c, &req_c);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(server)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Provider base-URL overrides understood by `SyncRequest` / `PrepareTrainingDataRequest`.
    pub fn base_urls(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("binance".to_string(), self.base_url.clone()),
            ("binance_archive".to_string(), self.base_url.clone()),
            ("okx".to_string(), self.base_url.clone()),
        ])
    }

    pub fn end(&self) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(self.end_ms.load(Ordering::SeqCst)).unwrap()
    }

    /// Advance the finalized-data end (e.g. by one period for an incremental re-sync).
    pub fn advance(&self, by: Duration) {
        self.end_ms
            .fetch_add(by.num_milliseconds(), Ordering::SeqCst);
    }

    pub fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Fail the next `times` requests whose path contains `path_contains`.
    pub fn inject(&self, path_contains: &str, times: usize, fault: FixtureFault) {
        self.state.lock().unwrap().faults.push(FaultRule {
            path_contains: path_contains.to_string(),
            remaining: times,
            fault,
        });
    }

    pub fn clear_faults(&self) {
        self.state.lock().unwrap().faults.clear();
    }

    /// Answer 404 for this archive month (`YYYY-MM`), as for a month not yet published.
    pub fn mark_archive_unavailable(&self, month: &str) {
        self.state
            .lock()
            .unwrap()
            .unavailable_archive_months
            .push(month.to_string());
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn request_log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// The fixture's own aggregated bar for `open_ms` (for hand-checking tests).
    pub fn expected_bar(&self, timeframe: Timeframe, open_ms: i64) -> Option<[Decimal; 5]> {
        let state = self.state.lock().unwrap();
        aggregate(&state.hours, timeframe, open_ms, i64::MAX)
            .map(|b| [b.open, b.high, b.low, b.close, b.volume])
    }
}

impl Drop for MarketFixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn generate_hours(until_ms: i64) -> Vec<HourBar> {
    let count = ((until_ms - BINANCE_BTC_FIRST_OPEN_MS) / 3_600_000).max(0) as usize;
    let mut out = Vec::with_capacity(count);
    let mut seed: u64 = 0x5eed_b7c0_2017_0817;
    let mut close = Decimal::new(430_000, 2); // 4300.00
    for _ in 0..count {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let step = ((seed >> 33) % 161) as i64 - 80; // -80..=80 bps
        let open = close;
        let mut next = (open * Decimal::new(10_000 + step, 4)).round_dp(2);
        // Keep the walk in a plausible band without floats.
        if next < Decimal::new(300_000, 2) {
            next = open + Decimal::new(1_234, 2);
        }
        if next > Decimal::new(15_000_000, 2) {
            next = open - Decimal::new(1_234, 2);
        }
        let wiggle = Decimal::new(((seed >> 13) % 900) as i64 + 100, 2);
        let high = open.max(next) + wiggle;
        let low = (open.min(next) - wiggle).max(Decimal::new(1, 2));
        let volume = Decimal::new(((seed >> 7) % 90_000_000) as i64 + 1_000_000, 6);
        out.push(HourBar {
            open,
            high,
            low,
            close: next,
            volume,
        });
        close = next;
    }
    out
}

/// Aggregate hourly bars `[open_ms, open_ms + tf)` that open before `limit_ms`.
fn aggregate(
    hours: &[HourBar],
    timeframe: Timeframe,
    open_ms: i64,
    limit_ms: i64,
) -> Option<HourBar> {
    if open_ms < BINANCE_BTC_FIRST_OPEN_MS {
        return None;
    }
    let first = ((open_ms - BINANCE_BTC_FIRST_OPEN_MS) / 3_600_000) as usize;
    let n = (timeframe.millis() / 3_600_000) as usize;
    let mut agg: Option<HourBar> = None;
    for i in 0..n {
        let at = open_ms + (i as i64) * 3_600_000;
        if at >= limit_ms {
            break;
        }
        let h = *hours.get(first + i)?;
        agg = Some(match agg {
            None => h,
            Some(a) => HourBar {
                open: a.open,
                high: a.high.max(h.high),
                low: a.low.min(h.low),
                close: h.close,
                volume: a.volume + h.volume,
            },
        });
    }
    agg
}

/// First bar open of `timeframe` at or after the venue's listing.
fn first_open(timeframe: Timeframe, listing_ms: i64) -> i64 {
    let step = timeframe.millis();
    let floored = listing_ms.div_euclid(step) * step;
    if floored < listing_ms {
        floored + step
    } else {
        floored
    }
}

struct Served {
    open_ms: i64,
    bar: HourBar,
    final_bar: bool,
}

/// Bars of `timeframe` from `from_ms` (inclusive) to `to_ms` (exclusive). The bar in progress at
/// `end` is included (not final) only while its close lies in the real future.
fn bars_between(
    hours: &[HourBar],
    timeframe: Timeframe,
    listing_ms: i64,
    end_ms: i64,
    from_ms: i64,
    to_ms: i64,
) -> Vec<Served> {
    let step = timeframe.millis();
    let mut at = first_open(timeframe, listing_ms).max(from_ms.div_euclid(step) * step);
    if at < from_ms {
        at += step;
    }
    let now_ms = Utc::now().timestamp_millis();
    let mut out = Vec::new();
    while at < to_ms && at < end_ms {
        let final_bar = at + step <= end_ms;
        if !final_bar && at + step <= now_ms {
            break;
        }
        if let Some(bar) = aggregate(hours, timeframe, at, end_ms) {
            out.push(Served {
                open_ms: at,
                bar,
                final_bar,
            });
        }
        at += step;
    }
    out
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn respond(
    socket: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes())?;
    socket.write_all(body)?;
    socket.flush()
}

fn handle(
    mut socket: TcpStream,
    end_ms: &AtomicI64,
    delay_ms: &AtomicU64,
    state: &Mutex<FixtureState>,
    requests: &AtomicU64,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let n = socket.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let target = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    requests.fetch_add(1, Ordering::SeqCst);
    let delay = delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let end = end_ms.load(Ordering::SeqCst);
    let mut guard = state.lock().unwrap();
    guard.log.push(target.clone());
    let mut fault = None;
    for rule in guard.faults.iter_mut() {
        if rule.remaining > 0 && target.contains(&rule.path_contains) {
            rule.remaining -= 1;
            fault = Some(rule.fault);
            break;
        }
    }
    match fault {
        Some(FixtureFault::ServerError) => {
            drop(guard);
            return respond(
                &mut socket,
                "500 Internal Server Error",
                "text/plain",
                b"fixture fault",
            );
        }
        Some(FixtureFault::RateLimited) => {
            drop(guard);
            return respond(
                &mut socket,
                "429 Too Many Requests",
                "text/plain",
                b"rate limited",
            );
        }
        _ => {}
    }
    let checksum_fault = fault == Some(FixtureFault::ChecksumMismatch);

    if let Some(rest) = path.strip_prefix("/data/spot/monthly/klines/") {
        // {SYMBOL}/{interval}/{SYMBOL}-{interval}-{YYYY-MM}.zip[.CHECKSUM]
        let parts: Vec<&str> = rest.split('/').collect();
        let (Some(interval), Some(file)) = (parts.get(1), parts.get(2)) else {
            drop(guard);
            return respond(&mut socket, "404 Not Found", "text/plain", b"");
        };
        let Some(tf) = Timeframe::parse(interval) else {
            drop(guard);
            return respond(&mut socket, "404 Not Found", "text/plain", b"");
        };
        let is_checksum = file.ends_with(".CHECKSUM");
        let month_str = file
            .trim_end_matches(".CHECKSUM")
            .trim_end_matches(".zip")
            .rsplit('-')
            .take(2)
            .collect::<Vec<_>>();
        let month_key = format!(
            "{}-{}",
            month_str.get(1).unwrap_or(&""),
            month_str.first().unwrap_or(&"")
        );
        let Ok(month) = NaiveDate::parse_from_str(&format!("{month_key}-01"), "%Y-%m-%d") else {
            drop(guard);
            return respond(&mut socket, "404 Not Found", "text/plain", b"");
        };
        let month_start = Utc.from_utc_datetime(&month.and_hms_opt(0, 0, 0).unwrap());
        let next_month = month.checked_add_months(chrono::Months::new(1)).unwrap();
        let month_end = Utc.from_utc_datetime(&next_month.and_hms_opt(0, 0, 0).unwrap());
        // Archives exist only for complete months before the data end.
        if month_end.timestamp_millis() > end
            || month_end.timestamp_millis() <= BINANCE_BTC_FIRST_OPEN_MS
            || guard.unavailable_archive_months.contains(&month_key)
        {
            drop(guard);
            return respond(&mut socket, "404 Not Found", "text/plain", b"");
        }
        let served = bars_between(
            &guard.hours,
            tf,
            BINANCE_BTC_FIRST_OPEN_MS,
            end,
            month_start.timestamp_millis(),
            month_end.timestamp_millis(),
        );
        drop(guard);
        let micros = month.year() >= 2025;
        let mut csv = String::new();
        for s in served {
            let scale = if micros { 1000 } else { 1 };
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},0,100,0,0,0\n",
                s.open_ms * scale,
                s.bar.open,
                s.bar.high,
                s.bar.low,
                s.bar.close,
                s.bar.volume,
                (s.open_ms + tf.millis() - 1) * scale
            ));
        }
        let name = file.trim_end_matches(".CHECKSUM").trim_end_matches(".zip");
        let zip_bytes = build_zip(&format!("{name}.csv"), csv.as_bytes())?;
        if is_checksum {
            let mut hasher = Sha256::new();
            hasher.update(&zip_bytes);
            let mut digest = hex::encode(hasher.finalize());
            if checksum_fault {
                digest = "0".repeat(64);
            }
            let body = format!("{digest}  {name}.zip\n");
            return respond(&mut socket, "200 OK", "text/plain", body.as_bytes());
        }
        return respond(&mut socket, "200 OK", "application/zip", &zip_bytes);
    }

    if path == "/api/v3/klines" {
        let q = parse_query(query);
        let tf = q.get("interval").and_then(|v| Timeframe::parse(v));
        let Some(tf) = tf else {
            drop(guard);
            return respond(
                &mut socket,
                "400 Bad Request",
                "application/json",
                b"{\"code\":-1120}",
            );
        };
        let limit: usize = q
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(500)
            .min(1000);
        let from: i64 = q.get("startTime").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut served = bars_between(
            &guard.hours,
            tf,
            BINANCE_BTC_FIRST_OPEN_MS,
            end,
            from,
            i64::MAX,
        );
        drop(guard);
        served.truncate(limit);
        let rows: Vec<String> = served
            .iter()
            .map(|s| {
                format!(
                    "[{},\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",{},\"0\",100,\"0\",\"0\",\"0\"]",
                    s.open_ms,
                    s.bar.open,
                    s.bar.high,
                    s.bar.low,
                    s.bar.close,
                    s.bar.volume,
                    s.open_ms + tf.millis() - 1
                )
            })
            .collect();
        let body = format!("[{}]", rows.join(","));
        return respond(&mut socket, "200 OK", "application/json", body.as_bytes());
    }

    if path == "/api/v5/market/history-candles" {
        let q = parse_query(query);
        let tf = q.get("bar").and_then(|v| Timeframe::parse(v));
        let Some(tf) = tf else {
            drop(guard);
            return respond(
                &mut socket,
                "200 OK",
                "application/json",
                b"{\"code\":\"51000\",\"msg\":\"bar\",\"data\":[]}",
            );
        };
        let limit: usize = q
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(100)
            .min(100);
        let after: i64 = q
            .get("after")
            .and_then(|v| v.parse().ok())
            .unwrap_or(i64::MAX)
            .min(end.saturating_add(tf.millis()));
        let from = after
            .saturating_sub(tf.millis() * limit as i64)
            .max(OKX_BTC_FIRST_OPEN_MS);
        let mut served = bars_between(&guard.hours, tf, OKX_BTC_FIRST_OPEN_MS, end, from, after);
        drop(guard);
        served.reverse(); // newest first, like OKX
        served.truncate(limit);
        let rows: Vec<String> = served
            .iter()
            .map(|s| {
                format!(
                    "[\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"0\",\"0\",\"{}\"]",
                    s.open_ms,
                    s.bar.open,
                    s.bar.high,
                    s.bar.low,
                    s.bar.close,
                    s.bar.volume,
                    if s.final_bar { "1" } else { "0" }
                )
            })
            .collect();
        let body = format!(
            "{{\"code\":\"0\",\"msg\":\"\",\"data\":[{}]}}",
            rows.join(",")
        );
        return respond(&mut socket, "200 OK", "application/json", body.as_bytes());
    }
    drop(guard);
    respond(&mut socket, "404 Not Found", "text/plain", b"")
}

fn build_zip(name: &str, data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::default());
        writer
            .start_file(name, options)
            .map_err(std::io::Error::other)?;
        writer.write_all(data)?;
        writer.finish().map_err(std::io::Error::other)?;
    }
    Ok(cursor.into_inner())
}
