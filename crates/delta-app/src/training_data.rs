//! Default BTC training data (`S-48`): one job prepares `BINANCE:BTC/USDT` (primary, from its
//! 2017-08-17 listing) and `OKX:BTC/USDT` (cross-check) for `1d`, `4h` and `1h`. Every
//! (instrument, timeframe) batch publishes atomically as its own `dataset_version`; a failed or
//! cancelled batch rolls back alone, already published batches stay, and a retry resumes from
//! each batch's watermark (`sync_checkpoint`). No placeholder bar is ever produced.

use super::*;
use chrono::{Datelike, TimeZone};
use delta_core::{quality_kind_impact, QualityImpact, Timeframe};
use delta_data::{CandidatePoolQuery, CandidateQualityFilter};
use std::collections::BTreeSet;

pub const DEFAULT_TRAINING_INSTRUMENT: &str = "BINANCE:BTC/USDT";
pub const CROSSCHECK_TRAINING_INSTRUMENT: &str = "OKX:BTC/USDT";
/// Binance BTCUSDT first kline, measured 2026-11-14 against `data-api.binance.vision`.
pub const BINANCE_BTC_LISTING_MS: i64 = 1_502_942_400_000;
/// Bars re-read below each watermark so late revisions are noticed (`S-09`).
const OVERLAP_BARS: i64 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareTrainingDataRequest {
    pub instruments: Vec<String>,
    pub timeframes: Vec<Timeframe>,
    /// Earliest date to prepare; `None` = full available history.
    pub from: Option<NaiveDate>,
    /// Provider base-URL overrides (`binance`, `binance_archive`, `okx`) for fixtures.
    pub base_urls: BTreeMap<String, String>,
}

impl Default for PrepareTrainingDataRequest {
    fn default() -> Self {
        Self {
            instruments: vec![
                DEFAULT_TRAINING_INSTRUMENT.to_string(),
                CROSSCHECK_TRAINING_INSTRUMENT.to_string(),
            ],
            timeframes: Timeframe::ALL.to_vec(),
            from: None,
            base_urls: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingDataBatchDto {
    pub instrument_id: String,
    pub timeframe: Timeframe,
    /// `published`, `up_to_date`, `failed` or `cancelled`.
    pub status: String,
    pub dataset_version: Option<i64>,
    pub bars_committed: usize,
    pub first_open_at: Option<DateTime<Utc>>,
    pub last_close_at: Option<DateTime<Utc>>,
    pub archive_months: usize,
    pub rest_pages: usize,
    pub intraday_gaps: usize,
    pub quality_issues: usize,
    pub cross_source_compared: usize,
    pub cross_source_deviations: usize,
    pub max_cross_source_deviation_bps: Option<String>,
    pub elapsed_ms: u128,
    pub error: Option<ErrorEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareTrainingDataSummaryDto {
    pub batches: Vec<TrainingDataBatchDto>,
    pub published_versions: Vec<i64>,
    pub failed: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingDataTimeframeStatusDto {
    pub timeframe: Timeframe,
    pub bar_count: usize,
    pub first_open_at: Option<DateTime<Utc>>,
    pub last_close_at: Option<DateTime<Utc>>,
    /// The last finalized bar is more than one period behind the current time.
    pub stale: bool,
    pub candidate_ready: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingDataStatusDto {
    pub instrument_id: String,
    pub dataset_version: Option<i64>,
    pub timeframes: Vec<TrainingDataTimeframeStatusDto>,
    /// No startable daily window for the default instrument: prepare automatically.
    pub needs_preparation: bool,
    /// Data exists but is behind: update in the background without blocking new runs.
    pub needs_update: bool,
}

/// Raw provider payload kept in memory during the fetch phase and stored at commit.
enum RawTrainingPayload {
    Bytes {
        provider: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    },
    Text {
        provider: &'static str,
        content_type: &'static str,
        body: String,
    },
}

/// Everything one batch fetched from the network, before any library write.
struct FetchedTrainingBatch {
    provider: &'static str,
    fetch_from: DateTime<Utc>,
    had_watermark: bool,
    checkpoint: SyncCheckpointRecord,
    bars: Vec<BarRecord>,
    raw_payloads: Vec<RawTrainingPayload>,
}

fn batch_error(code: &str, error: impl std::fmt::Display, retryable: bool) -> ErrorEnvelope {
    ErrorEnvelope::new(code, error.to_string(), retryable)
}

fn provider_error(error: delta_data::ProviderError) -> ErrorEnvelope {
    let retryable = !matches!(
        error,
        delta_data::ProviderError::ChecksumMismatch { .. }
            | delta_data::ProviderError::ForbiddenEndpoint(_)
    );
    let code = match &error {
        delta_data::ProviderError::RateLimited { .. } => "rate_limited",
        delta_data::ProviderError::ChecksumMismatch { .. } => "archive_checksum_mismatch",
        delta_data::ProviderError::Http { .. } => "network_unavailable",
        _ => "provider_failed",
    };
    ErrorEnvelope::new(code, error.to_string(), retryable)
}

fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap()
}

fn at_midnight(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
}

impl AppService {
    /// Start the preparation job (`S-48`): low priority, cancellable, timed out, with progress.
    pub fn start_prepare_training_data(
        &self,
        request: PrepareTrainingDataRequest,
        timeout: Duration,
    ) -> JobId {
        let paths = self.paths.clone();
        let lease = self._lock.clone();
        self.jobs.spawn_controlled_job(
            "prepare_training_data",
            timeout,
            move |cancel, state, deadline, progress| {
                let mut worker = AppService::open_with_lease(paths, "USD", "USD", "UTC", lease)?;
                worker.sync_control = Some((cancel, deadline, state, progress));
                let summary = worker.prepare_training_data(&request)?;
                if summary.failed > 0 {
                    let first = summary
                        .batches
                        .iter()
                        .find_map(|b| b.error.clone())
                        .unwrap_or_else(|| {
                            ErrorEnvelope::new("training_data_failed", "batch failed", true)
                        });
                    return Err(ErrorEnvelope::new(
                        first.code,
                        format!(
                            "{} 批未完成（已发布 {} 批保留）：{}",
                            summary.failed,
                            summary.published_versions.len(),
                            first.message
                        ),
                        true,
                    ));
                }
                Ok(())
            },
        )
    }

    fn prepare_progress(&self, phase: String, prepared: usize, published: usize) {
        if let Some((_, _, _, progress)) = &self.sync_control {
            let mut progress = progress.lock().unwrap();
            progress.phase = phase;
            progress.records_prepared = prepared;
            progress.records_published = published;
        }
    }

    /// Prepare every requested (instrument, timeframe) batch in dependency order: primary
    /// before cross-check, daily first so a default run can start as early as possible.
    pub fn prepare_training_data(
        &mut self,
        request: &PrepareTrainingDataRequest,
    ) -> Result<PrepareTrainingDataSummaryDto, ErrorEnvelope> {
        let started = Instant::now();
        for base in request.base_urls.values() {
            delta_data::validate_provider_base_url(base).map_err(sync_error_local)?;
        }
        let registry = self
            .store
            .load_instrument_registry()
            .map_err(sync_error_local)?;
        for id in &request.instruments {
            let inst = registry
                .instruments
                .get(id)
                .ok_or_else(|| ErrorEnvelope::new("unknown_instrument", id.clone(), false))?;
            if !matches!(inst.venue.as_str(), "BINANCE" | "OKX") {
                return Err(ErrorEnvelope::new(
                    "unsupported_training_source",
                    format!("{id}: training data preparation covers Binance/OKX crypto spot"),
                    false,
                ));
            }
        }
        let mut order: Vec<(String, Timeframe)> = Vec::new();
        for tf in Timeframe::ALL {
            if !request.timeframes.contains(&tf) {
                continue;
            }
            let mut ids = request.instruments.clone();
            ids.sort_by_key(|id| (!id.starts_with("BINANCE:"), id.clone()));
            for id in ids {
                order.push((id, tf));
            }
        }
        let client = build_http_client(20000).map_err(sync_error_local)?;
        let mut summary = PrepareTrainingDataSummaryDto {
            batches: Vec::new(),
            published_versions: Vec::new(),
            failed: 0,
            elapsed_ms: 0,
        };
        let total = order.len();
        for (index, (id, tf)) in order.into_iter().enumerate() {
            if self.check_sync_control().is_err() {
                break;
            }
            let at = Instant::now();
            self.prepare_progress(
                format!("准备训练数据 {}/{} · {id} {tf}", index + 1, total),
                summary.batches.iter().map(|b| b.bars_committed).sum(),
                summary.published_versions.len(),
            );
            let mut batch = TrainingDataBatchDto {
                instrument_id: id.clone(),
                timeframe: tf,
                status: "failed".into(),
                dataset_version: None,
                bars_committed: 0,
                first_open_at: None,
                last_close_at: None,
                archive_months: 0,
                rest_pages: 0,
                intraday_gaps: 0,
                quality_issues: 0,
                cross_source_compared: 0,
                cross_source_deviations: 0,
                max_cross_source_deviation_bps: None,
                elapsed_ms: 0,
                error: None,
            };
            match self.prepare_one_batch(&client, request, &registry, &id, tf, &mut batch) {
                Ok(()) => {
                    if let Some(v) = batch.dataset_version {
                        summary.published_versions.push(v);
                    }
                }
                Err(error) => {
                    batch.status = if error.code == "sync_cancelled" {
                        "cancelled".into()
                    } else {
                        "failed".into()
                    };
                    batch.dataset_version = None;
                    batch.bars_committed = 0;
                    batch.error = Some(error);
                    summary.failed += 1;
                }
            }
            batch.elapsed_ms = at.elapsed().as_millis();
            summary.batches.push(batch);
        }
        summary.elapsed_ms = started.elapsed().as_millis();
        self.prepare_progress(
            if summary.failed == 0 {
                "训练数据已就绪".into()
            } else {
                format!("{} 批未完成，可重试", summary.failed)
            },
            summary.batches.iter().map(|b| b.bars_committed).sum(),
            summary.published_versions.len(),
        );
        Ok(summary)
    }

    /// One atomic batch. The network fetch runs first without touching the library, so the
    /// write lock is held only for the short commit (dataset version, raw payloads, bars,
    /// quality and publication) inside one savepoint. Holding it across minutes of downloads
    /// made every other write (start a run, advance a bar, transfer) fail with
    /// `database is locked` while preparation ran.
    fn prepare_one_batch(
        &mut self,
        client: &reqwest::Client,
        request: &PrepareTrainingDataRequest,
        registry: &delta_data::InstrumentRegistry,
        instrument_id: &str,
        timeframe: Timeframe,
        batch: &mut TrainingDataBatchDto,
    ) -> Result<(), ErrorEnvelope> {
        let Some(fetched) =
            self.fetch_training_batch(client, request, registry, instrument_id, timeframe, batch)?
        else {
            batch.status = "up_to_date".into();
            return Ok(());
        };
        self.store
            .conn()
            .execute_batch("SAVEPOINT training_batch")
            .map_err(|e| batch_error("sync_begin", e, false))?;
        let mut result =
            self.commit_training_batch(fetched, registry, instrument_id, timeframe, batch);
        // Cancellation and publication share the job-state lock, exactly like `market_sync`.
        let state = self
            .sync_control
            .as_ref()
            .map(|(_, _, state, _)| Arc::clone(state));
        let _guard = state.as_ref().map(|state| state.lock().unwrap());
        if result.is_ok() {
            if let Err(error) = self.check_sync_control() {
                result = Err(error);
            }
        }
        let keep = matches!(result, Ok(true));
        let sql = if keep {
            "RELEASE training_batch"
        } else {
            "ROLLBACK TO training_batch; RELEASE training_batch"
        };
        self.store
            .conn()
            .execute_batch(sql)
            .map_err(|e| batch_error("sync_finish", e, false))?;
        result.map(|_| ())
    }

    /// Fetch phase (no library writes): `Ok(None)` when the batch is already up to date.
    fn fetch_training_batch(
        &self,
        client: &reqwest::Client,
        request: &PrepareTrainingDataRequest,
        registry: &delta_data::InstrumentRegistry,
        instrument_id: &str,
        timeframe: Timeframe,
        batch: &mut TrainingDataBatchDto,
    ) -> Result<Option<FetchedTrainingBatch>, ErrorEnvelope> {
        let inst = &registry.instruments[instrument_id];
        let provider = if inst.venue == "OKX" {
            "okx"
        } else {
            "binance"
        };
        let resource = format!("{instrument_id}:{timeframe}:raw");
        let checkpoint = self
            .store
            .get_sync_checkpoint(provider, "bars", &resource)
            .map_err(sync_error_local)?;
        let watermark = checkpoint.as_ref().and_then(|cp| {
            DateTime::parse_from_rfc3339(&cp.cursor)
                .map(|d| d.with_timezone(&Utc))
                .ok()
                .or_else(|| {
                    NaiveDate::parse_from_str(&cp.cursor, "%Y-%m-%d")
                        .ok()
                        .map(at_midnight)
                })
        });
        let now = Utc::now();
        // Newest bar that can be final now opens one period before the current bar.
        let newest_final_open = timeframe.floor(now) - timeframe.duration();
        if let Some(mark) = watermark {
            if mark >= newest_final_open {
                return Ok(None);
            }
        }
        let floor_from = request.from.map(at_midnight).unwrap_or_else(|| {
            DateTime::<Utc>::from_timestamp_millis(BINANCE_BTC_LISTING_MS).unwrap()
        });
        let fetch_from = match watermark {
            Some(mark) => (mark - timeframe.duration() * OVERLAP_BARS as i32).max(floor_from),
            None => floor_from,
        };
        // The dataset version is created in the commit phase; storage stamps row ids and
        // `valid_from_version` from it, so parsed bars carry a placeholder.
        let version = 0;
        let mut raw_payloads: Vec<RawTrainingPayload> = Vec::new();
        let base = |name: &str, default: String| {
            request
                .base_urls
                .get(name)
                .cloned()
                .or_else(|| {
                    std::env::var(format!("DELTA_{}_BASE_URL", name.to_ascii_uppercase())).ok()
                })
                .unwrap_or(default)
        };
        let mut bars: Vec<BarRecord> = Vec::new();
        let endpoint;
        if provider == "binance" {
            let mut p = BinanceSpotProvider::default();
            p.rest_base_url = base("binance", p.rest_base_url);
            p.archive_base_url = base("binance_archive", p.archive_base_url);
            endpoint = format!("{};{}", p.archive_base_url, p.rest_base_url);
            let symbol = registry
                .external_symbol_for("binance", instrument_id)
                .ok_or_else(|| ErrorEnvelope::new("unknown_symbol", instrument_id, false))?;
            let current_month = month_start(now.date_naive());
            let mut rest_start = fetch_from;
            // Archives for complete months on a first preparation (immutable, checksummed).
            if watermark.is_none() {
                let mut month = month_start(fetch_from.date_naive());
                while month < current_month {
                    self.check_sync_control()?;
                    match self.sync_await(p.fetch_monthly_archive_tf(
                        client,
                        instrument_id,
                        &symbol,
                        month,
                        version,
                        timeframe,
                    )) {
                        Ok((page, bytes, checksum)) => {
                            raw_payloads.push(RawTrainingPayload::Bytes {
                                provider: "binance-archive",
                                content_type: "application/zip",
                                body: bytes,
                            });
                            raw_payloads.push(RawTrainingPayload::Text {
                                provider: "binance-archive",
                                content_type: "text/plain",
                                body: checksum,
                            });
                            bars.extend(page);
                            batch.archive_months += 1;
                            month = month.checked_add_months(chrono::Months::new(1)).unwrap();
                            rest_start = at_midnight(month).max(fetch_from);
                        }
                        Err(delta_data::ProviderError::Http {
                            status: Some(404), ..
                        }) => {
                            // Not (yet) published: the REST mirror covers the rest.
                            break;
                        }
                        Err(error) => return Err(provider_error(error)),
                    }
                }
            }
            let mut cursor = rest_start.timestamp_millis();
            loop {
                self.check_sync_control()?;
                if cursor > newest_final_open.timestamp_millis() {
                    break;
                }
                let (page, body) = self
                    .sync_await(p.fetch_klines(
                        client,
                        instrument_id,
                        &symbol,
                        Some(cursor),
                        now,
                        version,
                        timeframe,
                        1000,
                    ))
                    .map_err(provider_error)?;
                batch.rest_pages += 1;
                let raw_opens: Vec<i64> = serde_json::from_str::<Vec<serde_json::Value>>(&body)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r.get(0).and_then(serde_json::Value::as_i64))
                            .collect()
                    })
                    .unwrap_or_default();
                raw_payloads.push(RawTrainingPayload::Text {
                    provider: "binance",
                    content_type: "application/json",
                    body,
                });
                // The newest raw row drives the cursor, so rows the parser skips (off-grid or
                // not yet final) at the end of a page never stall or silently end pagination.
                let Some(last_raw) = raw_opens.iter().copied().max() else {
                    break;
                };
                let next = last_raw + 1;
                if next <= cursor {
                    return Err(ErrorEnvelope::new(
                        "pagination_stalled",
                        "Binance cursor did not advance",
                        true,
                    ));
                }
                bars.extend(page);
                if raw_opens.len() < 1000 {
                    break;
                }
                cursor = next;
            }
        } else {
            let p = OkxSpotProvider {
                base_url: base("okx", OkxSpotProvider::default().base_url),
            };
            endpoint = p.base_url.clone();
            let symbol = registry
                .external_symbol_for("okx", instrument_id)
                .ok_or_else(|| ErrorEnvelope::new("unknown_symbol", instrument_id, false))?;
            let mut cursor: Option<String> = None;
            loop {
                self.check_sync_control()?;
                let (page, body) = self
                    .sync_await(p.fetch_candles(
                        client,
                        instrument_id,
                        &symbol,
                        cursor.as_deref(),
                        version,
                        timeframe,
                    ))
                    .map_err(provider_error)?;
                batch.rest_pages += 1;
                // The oldest row (confirmed or not) drives the cursor.
                let oldest_raw = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v["data"].as_array().and_then(|rows| {
                            rows.iter()
                                .filter_map(|r| r[0].as_str().and_then(|s| s.parse::<i64>().ok()))
                                .min()
                        })
                    });
                raw_payloads.push(RawTrainingPayload::Text {
                    provider: "okx",
                    content_type: "application/json",
                    body,
                });
                let Some(oldest) = oldest_raw else {
                    break;
                };
                if cursor.as_deref() == Some(oldest.to_string().as_str()) {
                    return Err(ErrorEnvelope::new(
                        "pagination_stalled",
                        "OKX cursor did not advance",
                        true,
                    ));
                }
                bars.extend(page);
                if oldest <= fetch_from.timestamp_millis() {
                    break;
                }
                cursor = Some(oldest.to_string());
            }
        }
        bars.retain(|b| {
            b.is_final
                && b.open_at >= fetch_from
                && b.close_at <= now
                && timeframe.is_aligned(b.open_at)
        });
        bars.sort_by_key(|b| b.open_at);
        bars.dedup_by_key(|b| b.open_at);
        if bars.is_empty() {
            if watermark.is_some() {
                return Ok(None);
            }
            return Err(ErrorEnvelope::new(
                "empty_training_data",
                format!("no completed {timeframe} bars for {instrument_id}"),
                true,
            ));
        }
        for pair in bars.windows(2) {
            if pair[1].open_at != pair[0].close_at {
                batch.intraday_gaps += 1;
            }
        }
        let last = bars.last().unwrap();
        let cursor = if timeframe == Timeframe::D1 {
            last.session_date.to_string()
        } else {
            last.open_at.to_rfc3339()
        };
        let cp = SyncCheckpointRecord {
            provider: provider.to_string(),
            resource_kind: "bars".into(),
            resource_key: resource,
            connection_id: None,
            cursor,
            last_committed_at: now.to_rfc3339(),
            calendar_version: Some(special_table().version_tag.clone()),
            base_url: Some(endpoint),
        };
        Ok(Some(FetchedTrainingBatch {
            provider,
            fetch_from,
            had_watermark: watermark.is_some(),
            checkpoint: cp,
            bars,
            raw_payloads,
        }))
    }

    /// Commit phase, inside the caller's savepoint: create the dataset version, store raw
    /// payloads, bars and quality, then publish. Returns `Ok(true)` when a version was
    /// published, `Ok(false)` when only the unchanged overlap was re-fetched.
    fn commit_training_batch(
        &mut self,
        fetched: FetchedTrainingBatch,
        registry: &delta_data::InstrumentRegistry,
        instrument_id: &str,
        timeframe: Timeframe,
        batch: &mut TrainingDataBatchDto,
    ) -> Result<bool, ErrorEnvelope> {
        let FetchedTrainingBatch {
            provider,
            fetch_from,
            had_watermark,
            checkpoint: cp,
            bars,
            raw_payloads,
        } = fetched;
        let inst = &registry.instruments[instrument_id];
        let version = self
            .store
            .create_dataset_version(
                &format!("training-data:{provider}"),
                "public_open",
                &serde_json::json!({"instrument": instrument_id, "timeframe": timeframe, "from": fetch_from}).to_string(),
                "exact",
                "reconstructed_history",
            )
            .map_err(sync_error_local)?
            .version_num;
        for payload in &raw_payloads {
            match payload {
                RawTrainingPayload::Bytes {
                    provider,
                    content_type,
                    body,
                } => self.store.store_raw_bytes(provider, content_type, body),
                RawTrainingPayload::Text {
                    provider,
                    content_type,
                    body,
                } => self.store.store_raw_payload(provider, content_type, body),
            }
            .map_err(sync_error_local)?;
        }
        let first_open = bars.first().unwrap().open_at;
        let last_close = bars.last().unwrap().close_at;
        let (count, mut issues) = self
            .store
            .commit_bars_and_checkpoint(&bars, version, Some(&cp))
            .map_err(sync_error_local)?;
        // Only the overlap was re-fetched and nothing changed: no new version is published.
        if count == 0 && issues.is_empty() && had_watermark {
            batch.status = "up_to_date".into();
            return Ok(false);
        }
        if timeframe == Timeframe::D1 {
            let (_, gaps) = self
                .store
                .analyze_coverage_gaps(
                    "CRYPTO_24_7",
                    instrument_id,
                    first_open.date_naive(),
                    (last_close - chrono::Duration::nanoseconds(1)).date_naive(),
                    version,
                )
                .map_err(sync_error_local)?;
            for gap in gaps {
                issues.push(DataQualityIssue {
                    id: format!(
                        "DQ:{instrument_id}:{}:{version}:{}",
                        gap.start_date, gap.gap_kind
                    ),
                    dataset_version: version,
                    instrument_id: instrument_id.to_string(),
                    session_date: gap.start_date,
                    kind: gap.gap_kind.clone(),
                    sources: "training-data".into(),
                    detail: gap.gap_kind,
                });
            }
        }
        // Cross-check the verification venue against the primary at the same timeframe; the
        // deviations are price-affecting and recorded against the verification instrument.
        if inst.venue == "OKX" {
            let primary_id = format!("BINANCE:{}", inst.symbol);
            if registry.instruments.contains_key(&primary_id) {
                let primary = self
                    .store
                    .query_bars_at_version(
                        &primary_id,
                        timeframe.as_str(),
                        AdjustmentKind::Raw,
                        Some("binance"),
                        version,
                        None,
                    )
                    .map_err(sync_error_local)?;
                let by_open: HashMap<DateTime<Utc>, Decimal> =
                    primary.iter().map(|b| (b.open_at, b.close)).collect();
                let threshold = Decimal::new(5, 3);
                let mut max_rel = Decimal::ZERO;
                let mut flagged_dates = BTreeSet::new();
                for bar in &bars {
                    if let Some(other) = by_open.get(&bar.open_at) {
                        batch.cross_source_compared += 1;
                        let denom = bar.close.max(*other);
                        if denom <= Decimal::ZERO {
                            continue;
                        }
                        let rel = (bar.close - *other).abs() / denom;
                        max_rel = max_rel.max(rel);
                        if rel > threshold {
                            batch.cross_source_deviations += 1;
                            if flagged_dates.insert(bar.session_date) {
                                issues.push(DataQualityIssue {
                                    id: format!(
                                        "DQI:MISMATCH:{instrument_id}:{timeframe}:{}:{version}",
                                        bar.session_date
                                    ),
                                    dataset_version: version,
                                    instrument_id: instrument_id.to_string(),
                                    session_date: bar.session_date,
                                    kind: "price_mismatch".into(),
                                    sources: "okx,binance".into(),
                                    detail: format!(
                                        "{timeframe} close {} vs binance {} at {}",
                                        bar.close, other, bar.open_at
                                    ),
                                });
                            }
                        }
                    }
                }
                if batch.cross_source_compared > 0 {
                    batch.max_cross_source_deviation_bps = Some(to_canonical_text(
                        (max_rel * Decimal::from(10_000)).round_dp(2),
                    ));
                }
            }
        }
        debug_assert!(issues.iter().all(|i| quality_kind_impact(&i.kind)
            == QualityImpact::AffectsPrice
            || i.kind == "data_revision"));
        self.store
            .record_quality_issues(&issues)
            .map_err(sync_error_local)?;
        let source = if provider == "binance" {
            "binance"
        } else {
            "okx"
        };
        let coverage = serde_json::json!({
            "kind": "training_data",
            "instrument": instrument_id,
            "timeframe": timeframe,
            "first_open_at": first_open,
            "last_close_at": last_close,
            "bars": count,
            "archive_months": batch.archive_months,
            "rest_pages": batch.rest_pages,
            "gaps": batch.intraday_gaps,
        })
        .to_string();
        self.store
            .conn()
            .execute(
                "UPDATE dataset_version SET source=?1, coverage_json=?2, quality=?3 WHERE version_num=?4",
                params![
                    source,
                    coverage,
                    if issues.is_empty() { "exact" } else { "warning" },
                    version
                ],
            )
            .map_err(sync_error_local)?;
        self.store
            .publish_dataset_version(version)
            .map_err(sync_error_local)?;
        batch.status = "published".into();
        batch.dataset_version = Some(version);
        batch.bars_committed = count;
        batch.first_open_at = Some(first_open);
        batch.last_close_at = Some(last_close);
        batch.quality_issues = issues.len();
        Ok(true)
    }

    /// Readiness of the default training data (`S-48` trigger): counts at the latest published
    /// version and, if asked, whether a default-length window can be drawn per timeframe.
    pub fn training_data_status(
        &self,
        instrument_id: &str,
        check_pool: bool,
    ) -> Result<TrainingDataStatusDto, ErrorEnvelope> {
        let version: Option<i64> = self
            .store
            .conn()
            .query_row(
                "SELECT MAX(version_num) FROM dataset_publication",
                [],
                |r| r.get(0),
            )
            .map_err(sync_error_local)?;
        let now = Utc::now();
        let mut timeframes = Vec::new();
        for tf in Timeframe::ALL {
            let (count, first, last): (i64, Option<String>, Option<String>) = match version {
                Some(v) => self
                    .store
                    .training_bar_coverage(instrument_id, tf.as_str(), v)
                    .map_err(sync_error_local)?,
                None => (0, None, None),
            };
            let parse = |s: Option<String>| {
                s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc))
            };
            let last_close = parse(last);
            let stale = last_close.is_none_or(|c| c + tf.duration() < now);
            let candidate_ready = match (check_pool, version) {
                (true, Some(v)) if count > 0 => {
                    let pool = self
                        .store
                        .query_training_candidate_pool(&CandidatePoolQuery {
                            dataset_version: v,
                            market_category: "crypto_spot".into(),
                            instrument_id: Some(instrument_id.to_string()),
                            start_date: None,
                            end_date: None,
                            warmup_bars: 0,
                            history_len: 120,
                            train_len: 250,
                            reveal_len: 60,
                            timeframe: tf.as_str().into(),
                            quality_filter: CandidateQualityFilter::PriceAffectingOnly,
                        })
                        .map_err(sync_error_local)?;
                    Some(!pool.candidates.is_empty())
                }
                (true, _) => Some(false),
                _ => None,
            };
            timeframes.push(TrainingDataTimeframeStatusDto {
                timeframe: tf,
                bar_count: count as usize,
                first_open_at: parse(first),
                last_close_at: last_close,
                stale,
                candidate_ready,
            });
        }
        let daily = &timeframes[0];
        let needs_preparation = if check_pool {
            daily.candidate_ready != Some(true)
        } else {
            daily.bar_count < 430
        };
        let needs_update =
            !needs_preparation && timeframes.iter().any(|t| t.bar_count > 0 && t.stale);
        Ok(TrainingDataStatusDto {
            instrument_id: instrument_id.to_string(),
            dataset_version: version,
            timeframes,
            needs_preparation,
            needs_update,
        })
    }
}

fn sync_error_local(error: impl std::fmt::Display) -> ErrorEnvelope {
    ErrorEnvelope::new("sync_failed", error.to_string(), true)
}
