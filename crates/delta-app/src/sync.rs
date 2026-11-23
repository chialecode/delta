use super::*;
use chrono::Datelike;
use delta_data::get_text_with_retry;
use std::collections::BTreeSet;

const PLACEHOLDER_VERSION: i64 = 0;

#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub start: NaiveDate,
    /// Exclusive end date. Only completed bars are published.
    pub end: NaiveDate,
    pub instruments: Vec<String>,
    pub fx_pairs: Vec<(String, String)>,
    pub use_configured_credentials: bool,
    pub base_urls: BTreeMap<String, String>,
}

/// Raw provider payload buffered during Phase 1 (fetch, no SQLite write lock) and persisted
/// atomically during Phase 2 (`SAVEPOINT market_sync`).
enum RawSyncPayload {
    Text {
        provider: String,
        content_type: &'static str,
        body: String,
    },
    Bytes {
        provider: String,
        content_type: &'static str,
        body: Vec<u8>,
    },
}

struct PreparedInstrumentSync {
    instrument_id: String,
    calendar_venue: &'static str,
    start: NaiveDate,
    bars: Vec<BarRecord>,
    actions: Vec<CorporateActionRecord>,
    cross_checks: Vec<(Vec<BarRecord>, Vec<BarRecord>, Vec<CorporateActionRecord>)>,
    checkpoint: SyncCheckpointRecord,
}

struct PreparedFxBatch {
    rates: Vec<FxRateRecord>,
    checkpoint: SyncCheckpointRecord,
}

struct PreparedMarketSync {
    registry: delta_data::InstrumentRegistry,
    subscribed: BTreeSet<String>,
    now: DateTime<Utc>,
    raw_payloads: Vec<RawSyncPayload>,
    instruments: Vec<PreparedInstrumentSync>,
    fx_batches: Vec<PreparedFxBatch>,
    warnings: Vec<(String, NaiveDate, String)>,
}

impl AppService {
    /// Dropping an in-flight provider future closes its request; cancellation is
    /// checked during socket waits and retry backoff, not just between pages.
    pub(crate) fn sync_await<F, T>(&self, future: F) -> Result<T, delta_data::ProviderError>
    where
        F: std::future::Future<Output = Result<T, delta_data::ProviderError>>,
    {
        let Some(runtime) = self.runtime.as_ref() else {
            return Err(delta_data::ProviderError::Http {
                provider: "sync-control".into(),
                status: None,
                message: "async runtime unavailable in read-only snapshot".into(),
                retryable: false,
            });
        };
        let Some((cancel, deadline, _, _)) = &self.sync_control else {
            return runtime.block_on(future);
        };
        let cancel = Arc::clone(cancel);
        let deadline = *deadline;
        runtime.block_on(async move {
            tokio::pin!(future);
            loop {
                if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    return Err(delta_data::ProviderError::Http {
                        provider: "sync-control".into(),
                        status: None,
                        message: "sync cancelled before publication".into(),
                        retryable: false,
                    });
                }
                tokio::select! {
                    result = &mut future => return result,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        })
    }

    /// The worker holds the same directory lock for its entire lifetime.
    pub fn start_market_sync(&self, request: SyncRequest, timeout: Duration) -> JobId {
        let paths = self.paths.clone();
        let lease = self._lock.clone();
        self.jobs.spawn_controlled_job(
            "market_sync",
            timeout,
            move |cancel, state, deadline, progress| {
                let mut worker = AppService::open_with_lease(paths, "USD", "USD", "UTC", lease)?;
                worker.sync_control = Some((cancel, deadline, state, progress));
                worker.sync_market_data(&request).map(|_| ())
            },
        )
    }

    pub(crate) fn check_sync_control(&self) -> Result<(), ErrorEnvelope> {
        if let Some((cancel, deadline, _, _)) = &self.sync_control {
            if cancel.load(Ordering::SeqCst) || Instant::now() >= *deadline {
                return Err(ErrorEnvelope::new(
                    "sync_cancelled",
                    "sync cancelled before publication; no version committed",
                    true,
                ));
            }
        }
        Ok(())
    }

    fn sync_progress(&self, phase: String, summary: Option<&SyncSummaryDto>, published: bool) {
        if let Some((_, _, _, progress)) = &self.sync_control {
            let mut progress = progress.lock().unwrap_or_else(|e| e.into_inner());
            progress.phase = phase;
            if let Some(summary) = summary {
                progress.records_prepared = summary.bars_committed
                    + summary.corporate_actions_committed
                    + summary.fx_rates_committed;
                progress.records_published = if published {
                    progress.records_prepared
                } else {
                    0
                };
            }
        }
    }

    fn sync_progress_prepared(&self, phase: String, prepared: usize) {
        if let Some((_, _, _, progress)) = &self.sync_control {
            let mut progress = progress.lock().unwrap_or_else(|e| e.into_inner());
            progress.phase = phase;
            progress.records_prepared = prepared;
            progress.records_published = 0;
        }
    }

    /// Synchronize subscriptions through the same service used by all consumers.
    ///
    /// Runs in two phases (`APP-B03`):
    /// 1. **Fetch phase** (no SQLite write lock): resolves subscriptions/checkpoints and performs
    ///    all network I/O into memory so concurrent UI/note/ledger writes are never blocked.
    /// 2. **Commit phase** (short local `SAVEPOINT market_sync`): creates the dataset version,
    ///    writes raw payloads/bars/actions/FX/checkpoints, and publishes atomically. Failure or
    ///    cancellation rolls back every record and cursor together.
    pub fn sync_market_data(
        &mut self,
        request: &SyncRequest,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        self.sync_progress("准备数据资源".into(), None, false);
        if request.start >= request.end {
            return Err(ErrorEnvelope::new(
                "invalid_range",
                "sync requires start < end",
                false,
            ));
        }
        for provider in [
            "okx",
            "binance",
            "binance_archive",
            "coinbase",
            "bitstamp",
            "kraken",
            "frankfurter",
            "yahoo",
            "eastmoney",
            "tiingo",
        ] {
            if let Some(base) = sync_base_url_override(request, provider) {
                delta_data::validate_provider_base_url(&base).map_err(sync_error)?;
            }
        }

        // Phase 1: network fetch with zero SQLite write transaction held.
        let prepared = match self.fetch_market_data(request) {
            Ok(prepared) => prepared,
            Err(err) => {
                self.sync_progress("已停止，未发布本次数据".into(), None, false);
                return Err(err);
            }
        };
        if let Err(err) = self.check_sync_control() {
            self.sync_progress("已停止，未发布本次数据".into(), None, false);
            return Err(err);
        }

        // Phase 2: short local atomic commit inside SAVEPOINT market_sync.
        self.store
            .conn()
            .execute_batch("SAVEPOINT market_sync")
            .map_err(|e| ErrorEnvelope::new("sync_begin", e.to_string(), false))?;
        let mut result = self.commit_prepared_market_sync(request, prepared);
        // Cancellation/deadline and the durable commit share one state lock. A cancellation
        // accepted before RELEASE rolls everything back; after RELEASE the job is succeeded.
        let state = self
            .sync_control
            .as_ref()
            .map(|(_, _, state, _)| Arc::clone(state));
        let mut publication_guard = state
            .as_ref()
            .map(|state| state.lock().unwrap_or_else(|e| e.into_inner()));
        if result.is_ok() {
            self.sync_progress("正在原子提交".into(), result.as_ref().ok(), false);
            if let Err(error) = self.check_sync_control() {
                result = Err(error);
            }
        }
        let sql = if result.is_ok() {
            "RELEASE market_sync"
        } else {
            "ROLLBACK TO market_sync; RELEASE market_sync"
        };
        self.store
            .conn()
            .execute_batch(sql)
            .map_err(|e| ErrorEnvelope::new("sync_finish", e.to_string(), false))?;
        if result.is_ok() {
            if let Some(state) = publication_guard.as_mut() {
                **state = JobState::Succeeded;
            }
            self.sync_progress("已发布数据，正在收尾".into(), result.as_ref().ok(), true);
        } else {
            self.sync_progress("已停止，未发布本次数据".into(), None, false);
        }
        result
    }

    fn fetch_market_data(
        &self,
        request: &SyncRequest,
    ) -> Result<PreparedMarketSync, ErrorEnvelope> {
        let registry = self.store.load_instrument_registry().map_err(sync_error)?;
        let mut subscribed: BTreeSet<String> = request.instruments.iter().cloned().collect();
        {
            let mut stmt = self
                .store
                .conn()
                .prepare("SELECT instrument_id FROM subscription")
                .map_err(sync_error)?;
            for row in stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sync_error)?
            {
                subscribed.insert(row.map_err(sync_error)?);
            }
        }
        let mut currencies = BTreeSet::new();
        for (base, quote) in &request.fx_pairs {
            currencies.insert(base.to_ascii_uppercase());
            currencies.insert(quote.to_ascii_uppercase());
        }
        {
            let mut stmt = self
                .store
                .conn()
                .prepare(
                    "SELECT base_currency FROM fx_subscription UNION SELECT quote_currency FROM fx_subscription",
                )
                .map_err(sync_error)?;
            for currency in stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sync_error)?
            {
                currencies.insert(currency.map_err(sync_error)?);
            }
        }
        // Fee assets need their own market price; ECB cannot quote BNB/OKB/BTC.
        for currency in &currencies {
            if registry
                .assets
                .get(currency)
                .is_some_and(|a| a.kind == delta_core::AssetKind::Crypto)
            {
                let mut candidates: Vec<_> = registry
                    .instruments
                    .values()
                    .filter(|i| i.base_asset_id == *currency && i.quote_asset_id == "USDT")
                    .collect();
                candidates.sort_by_key(|i| (&i.venue, &i.id));
                let candidate = candidates
                    .iter()
                    .find(|i| subscribed.contains(&i.id))
                    .or_else(|| candidates.first())
                    .ok_or_else(|| {
                        ErrorEnvelope::new(
                            "missing_fee_market",
                            format!("register a {currency}/USDT market"),
                            false,
                        )
                    })?;
                subscribed.insert(candidate.id.clone());
            }
        }
        if subscribed.is_empty() && currencies.iter().all(|currency| currency == "USD") {
            return Err(ErrorEnvelope::new(
                "no_subscriptions",
                "import a statement or supply --symbol",
                false,
            ));
        }

        let client = build_http_client(15000).map_err(sync_error)?;
        let now = Utc::now();
        let tiingo_token = if request.use_configured_credentials {
            self.credentials
                .get_secret("tiingo", "default")
                .ok()
                .or_else(|| std::env::var("DELTA_TIINGO_API_KEY").ok())
                .filter(|s| !s.trim().is_empty())
        } else {
            None
        };
        let end_at = request.end.and_hms_opt(0, 0, 0).unwrap().and_utc();

        let mut raw_payloads: Vec<RawSyncPayload> = Vec::new();
        let mut prepared_instruments: Vec<PreparedInstrumentSync> = Vec::new();
        let mut fx_batches: Vec<PreparedFxBatch> = Vec::new();
        let mut warnings: Vec<(String, NaiveDate, String)> = Vec::new();
        let mut prepared_records = 0usize;

        for instrument_id in &subscribed {
            self.check_sync_control()?;
            self.sync_progress_prepared(format!("读取标的 {instrument_id}"), prepared_records);
            let inst = registry
                .instruments
                .get(instrument_id)
                .ok_or_else(|| ErrorEnvelope::new("unknown_instrument", instrument_id, false))?;
            let provider = if inst.venue == "OKX" {
                "okx"
            } else if inst.venue == "BINANCE" {
                "binance"
            } else if tiingo_token.is_some() {
                "tiingo"
            } else {
                "yahoo"
            };
            let resource = format!("{instrument_id}:1d:raw");
            let old_cp = self
                .store
                .get_sync_checkpoint(provider, "bars", &resource)
                .map_err(sync_error)?;
            let old_cursor_date = old_cp
                .as_ref()
                .and_then(|cp| NaiveDate::parse_from_str(&cp.cursor, "%Y-%m-%d").ok());
            // When requesting a historical backfill earlier than the stored watermark, honor
            // `request.start` instead of clamping past `request.end` (`APP-B10`).
            let start = old_cursor_date
                .and_then(|d| d.checked_sub_days(chrono::Days::new(7)))
                .filter(|&d| d < request.end)
                .map(|d| d.max(request.start))
                .unwrap_or(request.start);
            let mut bars = Vec::new();
            let mut actions = Vec::new();
            let mut cross_checks = Vec::new();
            let mut actual_base;

            if provider == "okx" {
                let p = OkxSpotProvider {
                    base_url: sync_base_url(request, "okx", OkxSpotProvider::default().base_url),
                };
                actual_base = p.base_url.clone();
                let symbol = registry
                    .external_symbol_for("okx", instrument_id)
                    .ok_or_else(|| {
                        ErrorEnvelope::new(
                            "missing_provider_symbol",
                            format!("no okx symbol alias registered for {instrument_id}"),
                            false,
                        )
                    })?;
                let mut cursor = Some(end_at.timestamp_millis().to_string());
                loop {
                    self.check_sync_control()?;
                    let (page, body) = self
                        .sync_await(p.fetch_daily_candles(
                            &client,
                            instrument_id,
                            &symbol,
                            cursor.as_deref(),
                            PLACEHOLDER_VERSION,
                        ))
                        .map_err(sync_error)?;
                    raw_payloads.push(RawSyncPayload::Text {
                        provider: provider.to_string(),
                        content_type: "application/json",
                        body,
                    });
                    if page.is_empty() {
                        break;
                    }
                    let oldest = page.iter().map(|b| b.open_at).min().unwrap();
                    let next = oldest.timestamp_millis().to_string();
                    if cursor.as_deref() == Some(&next) {
                        return Err(ErrorEnvelope::new(
                            "pagination_stalled",
                            "OKX cursor did not advance",
                            true,
                        ));
                    }
                    bars.extend(page);
                    if oldest.date_naive() <= start {
                        break;
                    }
                    cursor = Some(next);
                }
            } else if provider == "binance" {
                let mut p = BinanceSpotProvider::default();
                p.rest_base_url = sync_base_url(request, "binance", p.rest_base_url);
                p.archive_base_url = sync_base_url(request, "binance_archive", p.archive_base_url);
                actual_base = p.rest_base_url.clone();
                let symbol = registry
                    .external_symbol_for("binance", instrument_id)
                    .ok_or_else(|| {
                        ErrorEnvelope::new(
                            "missing_provider_symbol",
                            format!("no binance symbol alias registered for {instrument_id}"),
                            false,
                        )
                    })?;
                let mut rest_start = start;
                if old_cp.is_none() && start < now.date_naive().with_day(1).unwrap() {
                    let mut month = start.with_day(1).unwrap();
                    let current_month = now.date_naive().with_day(1).unwrap();
                    while month < request.end && month < current_month {
                        self.check_sync_control()?;
                        match self.sync_await(p.fetch_monthly_archive(
                            &client,
                            instrument_id,
                            &symbol,
                            month,
                            PLACEHOLDER_VERSION,
                        )) {
                            Ok((page, bytes, checksum)) => {
                                raw_payloads.push(RawSyncPayload::Bytes {
                                    provider: "binance-archive".into(),
                                    content_type: "application/zip",
                                    body: bytes,
                                });
                                raw_payloads.push(RawSyncPayload::Text {
                                    provider: "binance-archive".into(),
                                    content_type: "text/plain",
                                    body: checksum,
                                });
                                bars.extend(page);
                                month = month
                                    .checked_add_months(chrono::Months::new(1))
                                    .ok_or_else(|| {
                                        ErrorEnvelope::new(
                                            "invalid_range",
                                            "archive month overflow",
                                            false,
                                        )
                                    })?;
                                rest_start = month.min(request.end);
                                actual_base = format!("{};{}", p.archive_base_url, p.rest_base_url);
                            }
                            Err(error @ delta_data::ProviderError::ChecksumMismatch { .. }) => {
                                return Err(sync_error(error))
                            }
                            Err(
                                delta_data::ProviderError::Http { .. }
                                | delta_data::ProviderError::RateLimited { .. },
                            ) => {
                                warnings.push((
                                    instrument_id.clone(),
                                    month,
                                    "archive_unavailable_rest_fallback".into(),
                                ));
                                break;
                            }
                            Err(error) => return Err(sync_error(error)),
                        }
                    }
                }
                let mut cursor = rest_start
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis();
                loop {
                    if cursor >= end_at.timestamp_millis() {
                        break;
                    }
                    self.check_sync_control()?;
                    let (page, body) = self
                        .sync_await(p.fetch_daily_klines(
                            &client,
                            instrument_id,
                            &symbol,
                            Some(cursor),
                            now,
                            PLACEHOLDER_VERSION,
                        ))
                        .map_err(sync_error)?;
                    raw_payloads.push(RawSyncPayload::Text {
                        provider: provider.to_string(),
                        content_type: "application/json",
                        body,
                    });
                    if page.is_empty() {
                        break;
                    }
                    let last = page.iter().map(|b| b.open_at).max().unwrap();
                    let next = last.timestamp_millis() + 86400000;
                    if next <= cursor {
                        return Err(ErrorEnvelope::new(
                            "pagination_stalled",
                            "Binance cursor did not advance",
                            true,
                        ));
                    }
                    let count = page.len();
                    bars.extend(page);
                    if last >= end_at || count < 100 {
                        break;
                    }
                    cursor = next;
                }
            } else {
                let p = YahooChartV8Provider {
                    base_url: sync_base_url(
                        request,
                        "yahoo",
                        YahooChartV8Provider::default().base_url,
                    ),
                };
                actual_base = p.base_url.clone();
                let venue = venue_to_calendar(&inst.venue);
                // Yahoo adjusts against all subsequent splits: fetch through acquisition time,
                // then bound publication and cross-validation to the requested historical interval.
                let fetched = self.sync_await(p.fetch_chart_v8(
                    &client,
                    instrument_id,
                    venue,
                    &inst.symbol,
                    start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                    now.timestamp(),
                    PLACEHOLDER_VERSION,
                ));
                let em = EastMoneyUsEquityProvider {
                    base_url: sync_base_url(
                        request,
                        "eastmoney",
                        EastMoneyUsEquityProvider::default().base_url,
                    ),
                };
                let alternate = registry
                    .external_symbol_for("eastmoney", instrument_id)
                    .map(|secid| {
                        self.sync_await(em.fetch_kline_raw(
                            &client,
                            instrument_id,
                            venue,
                            &secid,
                            &start.format("%Y%m%d").to_string(),
                            &request.end.format("%Y%m%d").to_string(),
                            PLACEHOLDER_VERSION,
                        ))
                    });
                let mut public_error = None;
                match (fetched, alternate) {
                    (Ok((primary, ca, body)), Some(Ok((secondary, em_body)))) => {
                        raw_payloads.push(RawSyncPayload::Text {
                            provider: "yahoo".into(),
                            content_type: "application/json",
                            body,
                        });
                        raw_payloads.push(RawSyncPayload::Text {
                            provider: "eastmoney".into(),
                            content_type: "application/json",
                            body: em_body,
                        });
                        let primary_in_range =
                            filter_final_bars_in_range(&primary, start, request.end, now);
                        let secondary_in_range =
                            filter_final_bars_in_range(&secondary, start, request.end, now);
                        cross_checks.push((primary_in_range, secondary_in_range, ca.clone()));
                        bars = primary;
                        actions = ca;
                    }
                    (Ok((primary, ca, body)), Some(Err(_)) | None) => {
                        raw_payloads.push(RawSyncPayload::Text {
                            provider: "yahoo".into(),
                            content_type: "application/json",
                            body,
                        });
                        bars = primary;
                        actions = ca;
                        warnings.push((
                            instrument_id.clone(),
                            start,
                            "eastmoney_unavailable".into(),
                        ));
                    }
                    (Err(_), Some(Ok((secondary, body)))) => {
                        raw_payloads.push(RawSyncPayload::Text {
                            provider: "eastmoney".into(),
                            content_type: "application/json",
                            body,
                        });
                        bars = secondary;
                        warnings.push((
                            instrument_id.clone(),
                            start,
                            "missing_corporate_action".into(),
                        ));
                    }
                    (Err(e), Some(Err(_)) | None) => public_error = Some(e),
                }
                if let Some(token) = tiingo_token.as_deref() {
                    let p = TiingoEquityProvider {
                        base_url: sync_base_url(
                            request,
                            "tiingo",
                            TiingoEquityProvider::default().base_url,
                        ),
                    };
                    match self.sync_await(p.fetch_daily(
                        &client,
                        token,
                        instrument_id,
                        venue,
                        &inst.symbol,
                        start,
                        request.end,
                        PLACEHOLDER_VERSION,
                    )) {
                        Ok((primary, ca, body)) => {
                            if bars.is_empty() {
                                warnings.push((
                                    instrument_id.clone(),
                                    start,
                                    "yahoo_crosscheck_unavailable".into(),
                                ));
                            }
                            let primary_in_range =
                                filter_final_bars_in_range(&primary, start, request.end, now);
                            let existing_in_range =
                                filter_final_bars_in_range(&bars, start, request.end, now);
                            cross_checks.push((primary_in_range, existing_in_range, ca.clone()));
                            raw_payloads.push(RawSyncPayload::Text {
                                provider: "tiingo".into(),
                                content_type: "application/json",
                                body,
                            });
                            bars = primary;
                            actions = ca;
                            actual_base = p.base_url;
                        }
                        Err(_) => warnings.push((
                            instrument_id.clone(),
                            start,
                            "tiingo_unavailable".into(),
                        )),
                    }
                }
                if bars.is_empty() {
                    return Err(public_error.map(sync_error).unwrap_or_else(|| {
                        ErrorEnvelope::new(
                            "empty_sync",
                            "no equity provider returned completed bars",
                            true,
                        )
                    }));
                }
            }

            bars.retain(|b| {
                b.session_date >= start
                    && b.session_date < request.end
                    && b.close_at <= now
                    && b.is_final
            });
            bars.sort_by_key(|b| b.session_date);
            bars.dedup_by_key(|b| b.session_date);
            if bars.is_empty() {
                return Err(ErrorEnvelope::new(
                    "empty_sync",
                    format!("no completed bars for {instrument_id}"),
                    true,
                ));
            }
            let latest_bar_date = bars.last().unwrap().session_date;
            let cursor_date =
                old_cursor_date.map_or(latest_bar_date, |prev| prev.max(latest_bar_date));
            let cp = SyncCheckpointRecord {
                provider: bars.last().unwrap().source.clone(),
                resource_kind: "bars".into(),
                resource_key: resource,
                connection_id: None,
                cursor: cursor_date.to_string(),
                last_committed_at: now.to_rfc3339(),
                calendar_version: Some(special_table().version_tag.clone()),
                base_url: Some(actual_base),
            };
            prepared_records += bars.len() + actions.len();
            prepared_instruments.push(PreparedInstrumentSync {
                instrument_id: instrument_id.clone(),
                calendar_venue: venue_to_calendar(&inst.venue),
                start,
                bars,
                actions,
                cross_checks,
                checkpoint: cp,
            });
            self.sync_progress_prepared(format!("核对标的 {instrument_id}"), prepared_records);
        }

        for id in &subscribed {
            if let Some(inst) = registry.instruments.get(id) {
                currencies.insert(inst.quote_asset_id.clone());
            }
        }

        for currency in currencies {
            self.check_sync_control()?;
            self.sync_progress_prepared(format!("读取汇率 {currency}/USD"), prepared_records);
            if currency == "USD" {
                continue;
            }
            if registry
                .assets
                .get(&currency)
                .is_some_and(|a| a.kind == delta_core::AssetKind::Crypto)
            {
                // Derive asset/USD from the completed asset/USDT bar and same-day USDT/USD
                // during the commit phase once stablecoin rates are written.
                continue;
            }
            let fx_provider = if currency == "USDT" {
                "coinbase"
            } else if currency == "USDC" {
                "bitstamp"
            } else {
                "frankfurter"
            };
            let checkpoint = self
                .store
                .get_sync_checkpoint(fx_provider, "fx", &format!("{currency}/USD"))
                .map_err(sync_error)?;
            let mut fx_watermark = checkpoint
                .as_ref()
                .and_then(|cp| NaiveDate::parse_from_str(&cp.cursor, "%Y-%m-%d").ok());
            let mut from = fx_watermark
                .and_then(|d| d.checked_sub_days(chrono::Days::new(4)))
                .filter(|&d| d < request.end)
                .map(|d| d.max(request.start))
                .unwrap_or(request.start);
            while from < request.end {
                self.check_sync_control()?;
                let to = (from + chrono::Duration::days(299)).min(request.end);
                let (mut rates, provider, endpoint) =
                    if matches!(currency.as_str(), "USDT" | "USDC") {
                        self.fetch_stablecoin_page(
                            &client,
                            &currency,
                            from,
                            to,
                            request,
                            &mut raw_payloads,
                            &mut warnings,
                        )?
                    } else {
                        let p = FrankfurterFxProvider {
                            base_url: sync_base_url(
                                request,
                                "frankfurter",
                                FrankfurterFxProvider::default().base_url,
                            ),
                        };
                        let (rates, body) = self
                            .sync_await(p.fetch_range(
                                &client,
                                "USD",
                                &[&currency],
                                from,
                                to,
                                PLACEHOLDER_VERSION,
                            ))
                            .map_err(sync_error)?;
                        raw_payloads.push(RawSyncPayload::Text {
                            provider: "frankfurter".into(),
                            content_type: "application/json",
                            body,
                        });
                        (rates, "frankfurter".to_string(), p.base_url)
                    };
                rates.retain(|r| {
                    r.effective_date >= from && r.effective_date < to && r.effective_at <= now
                });
                rates.sort_by(|a, b| {
                    a.effective_date
                        .cmp(&b.effective_date)
                        .then_with(|| a.source.cmp(&b.source))
                });
                if rates.is_empty() {
                    // A short ECB holiday interval can contain no publication. Keep the
                    // watermark; a later request overlaps the last published date.
                    if provider == "frankfurter" && (to - from).num_days() <= 4 {
                        from = to;
                        continue;
                    }
                    // `S-49` ①: a stablecoin gap is a valuation gap. It is recorded (and ledger
                    // valuation that needs it stays "unavailable") but never blocks crypto bars.
                    if matches!(currency.as_str(), "USDT" | "USDC") {
                        warnings.push((currency.clone(), from, "missing_stablecoin_fx".into()));
                        from = to;
                        continue;
                    }
                    return Err(ErrorEnvelope::new(
                        "missing_fx",
                        format!("no {currency}/USD rates"),
                        true,
                    ));
                }
                let latest_rate_date = rates.last().unwrap().effective_date;
                let next_fx_cursor =
                    fx_watermark.map_or(latest_rate_date, |prev| prev.max(latest_rate_date));
                fx_watermark = Some(next_fx_cursor);
                let cp = SyncCheckpointRecord {
                    provider,
                    resource_kind: "fx".into(),
                    resource_key: format!("{currency}/USD"),
                    connection_id: None,
                    cursor: next_fx_cursor.to_string(),
                    last_committed_at: now.to_rfc3339(),
                    calendar_version: None,
                    base_url: Some(endpoint.split('?').next().unwrap().into()),
                };
                prepared_records += rates.len();
                fx_batches.push(PreparedFxBatch {
                    rates,
                    checkpoint: cp,
                });
                from = to;
            }
        }

        Ok(PreparedMarketSync {
            registry,
            subscribed,
            now,
            raw_payloads,
            instruments: prepared_instruments,
            fx_batches,
            warnings,
        })
    }

    fn commit_prepared_market_sync(
        &mut self,
        request: &SyncRequest,
        prepared: PreparedMarketSync,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        let PreparedMarketSync {
            registry,
            subscribed,
            now,
            raw_payloads,
            instruments,
            fx_batches,
            warnings,
        } = prepared;

        let dsv = self
            .store
            .create_dataset_version(
                "public-sync",
                "public_open,unofficial_personal",
                &serde_json::json!({"start": request.start, "end": request.end, "instruments": subscribed}).to_string(),
                "exact",
                "reconstructed_history",
            )
            .map_err(sync_error)?;
        let version = dsv.version_num;
        let mut summary = SyncSummaryDto {
            dataset_version: dsv,
            bars_committed: 0,
            corporate_actions_committed: 0,
            fx_rates_committed: 0,
            quality_issues: Vec::new(),
        };

        for payload in &raw_payloads {
            match payload {
                RawSyncPayload::Text {
                    provider,
                    content_type,
                    body,
                } => self.store.store_raw_payload(provider, content_type, body),
                RawSyncPayload::Bytes {
                    provider,
                    content_type,
                    body,
                } => self.store.store_raw_bytes(provider, content_type, body),
            }
            .map_err(sync_error)?;
        }

        for (instrument, date, kind) in warnings {
            summary
                .quality_issues
                .push(source_warning(&instrument, date, version, &kind));
        }

        for mut inst_sync in instruments {
            stamp_bars_version(&mut inst_sync.bars, version);
            stamp_actions_version(&mut inst_sync.actions, version);
            for (mut primary, mut secondary, mut actions) in inst_sync.cross_checks {
                stamp_bars_version(&mut primary, version);
                stamp_bars_version(&mut secondary, version);
                stamp_actions_version(&mut actions, version);
                summary.quality_issues.extend(
                    cross_validate_bars_and_actions(
                        &inst_sync.instrument_id,
                        &primary,
                        &secondary,
                        &actions,
                        version,
                    )
                    .issues,
                );
            }
            let (count, issues) = self
                .store
                .commit_bars_and_checkpoint(&inst_sync.bars, version, Some(&inst_sync.checkpoint))
                .map_err(sync_error)?;
            summary.bars_committed += count;
            summary.quality_issues.extend(issues);
            summary.corporate_actions_committed += self
                .store
                .upsert_corporate_actions(&inst_sync.actions)
                .map_err(sync_error)?;
            let (_, gaps) = self
                .store
                .analyze_coverage_gaps(
                    inst_sync.calendar_venue,
                    &inst_sync.instrument_id,
                    inst_sync.start,
                    request.end.pred_opt().unwrap(),
                    version,
                )
                .map_err(sync_error)?;
            for gap in gaps {
                summary.quality_issues.push(source_warning(
                    &inst_sync.instrument_id,
                    gap.start_date,
                    version,
                    &gap.gap_kind,
                ));
            }
        }

        // Independent OKX/Binance markets are cross-checked when both are subscribed.
        for id in &subscribed {
            let Some(inst) = registry.instruments.get(id) else {
                continue;
            };
            if inst.venue == "OKX" {
                let other = format!("BINANCE:{}", inst.symbol);
                if subscribed.contains(&other) {
                    let primary = self
                        .store
                        .query_bars_at_version(
                            id,
                            "1d",
                            AdjustmentKind::Raw,
                            Some("okx"),
                            version,
                            None,
                        )
                        .map_err(sync_error)?;
                    let secondary = self
                        .store
                        .query_bars_at_version(
                            &other,
                            "1d",
                            AdjustmentKind::Raw,
                            Some("binance"),
                            version,
                            None,
                        )
                        .map_err(sync_error)?;
                    summary.quality_issues.extend(
                        cross_validate_bars_and_actions(id, &primary, &secondary, &[], version)
                            .issues,
                    );
                }
            }
        }

        for mut batch in fx_batches {
            stamp_fx_version(&mut batch.rates, version);
            summary.fx_rates_committed += self
                .store
                .upsert_fx_rates(&batch.rates, Some(&batch.checkpoint))
                .map_err(sync_error)?;
        }

        self.sync_crypto_fee_fx(&registry, &subscribed, request, version, &mut summary)?;

        for id in &request.instruments {
            let instrument = registry
                .instruments
                .get(id)
                .ok_or_else(|| ErrorEnvelope::new("unknown_instrument", id, false))?;
            self.store
                .conn()
                .execute(
                    "INSERT OR IGNORE INTO subscription(instrument_id,venue,symbol,auto_reason,created_at) VALUES (?1,?2,?3,'explicit_sync',?4)",
                    params![id, instrument.venue, instrument.symbol, now.to_rfc3339()],
                )
                .map_err(sync_error)?;
        }
        for (base, quote) in &request.fx_pairs {
            self.store
                .conn()
                .execute(
                    "INSERT OR IGNORE INTO fx_subscription(base_currency,quote_currency,auto_reason,created_at) VALUES (?1,?2,'explicit_sync',?3)",
                    params![
                        base.to_ascii_uppercase(),
                        quote.to_ascii_uppercase(),
                        now.to_rfc3339()
                    ],
                )
                .map_err(sync_error)?;
        }
        let mut source_query = self
            .store
            .conn()
            .prepare(
                "SELECT DISTINCT source FROM bar WHERE valid_from_version<=?1 AND (valid_to_version IS NULL OR valid_to_version>?1) UNION SELECT DISTINCT source FROM fx_rate WHERE dataset_version<=?1 UNION SELECT DISTINCT source FROM corporate_action WHERE factor_version<=?1 ORDER BY 1",
            )
            .map_err(sync_error)?;
        let sources = source_query
            .query_map(params![version], |r| r.get::<_, String>(0))
            .map_err(sync_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sync_error)?;
        drop(source_query);
        let mut terms = BTreeSet::new();
        for source in &sources {
            let provider = source.split('+').next().unwrap();
            terms.insert(match provider {
                "yahoo" | "eastmoney" => "unofficial_personal",
                "tiingo" | "alpaca" => "official_free_personal",
                _ => "public_open",
            });
        }
        summary.dataset_version.source = sources.join(",");
        summary.dataset_version.terms_class = terms.into_iter().collect::<Vec<_>>().join(",");
        let mut endpoint_query = self
            .store
            .conn()
            .prepare(
                "SELECT provider,base_url FROM sync_checkpoint WHERE last_committed_at=?1 ORDER BY provider,resource_key",
            )
            .map_err(sync_error)?;
        let endpoints = endpoint_query
            .query_map(params![now.to_rfc3339()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sync_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sync_error)?;
        drop(endpoint_query);
        let mut capabilities = Vec::new();
        for (provider, endpoint) in endpoints {
            let mut capability = match provider.as_str() {
                "okx" => OkxSpotProvider::default().capabilities(),
                "binance" => BinanceSpotProvider::default().capabilities(),
                "frankfurter" => FrankfurterFxProvider::default().capabilities(),
                "yahoo" => YahooChartV8Provider::default().capabilities(),
                "eastmoney" => EastMoneyUsEquityProvider::default().capabilities(),
                "tiingo" => TiingoEquityProvider::default().capabilities(),
                _ => StablecoinFxProvider::default()
                    .capabilities_for(&provider)
                    .ok_or_else(|| {
                        ErrorEnvelope::new("unknown_provider", "provider capability missing", false)
                    })?,
            };
            capability.base_url = endpoint;
            let encoded = serde_json::to_string(&capability).map_err(sync_error)?;
            self.store
                .conn()
                .execute(
                    "INSERT INTO provider_connection(id,provider,credential_ref,base_url,capabilities_json,status) VALUES (?1,?2,NULL,?3,?4,'connected') ON CONFLICT(id) DO UPDATE SET base_url=excluded.base_url,capabilities_json=excluded.capabilities_json,status=excluded.status",
                    params![format!("public:{provider}"), provider, capability.base_url, encoded],
                )
                .map_err(sync_error)?;
            capabilities.push(capability);
        }
        summary.dataset_version.coverage_json = serde_json::json!({
            "start": request.start,
            "end": request.end,
            "instruments": subscribed,
            "fx_pairs": request.fx_pairs,
            "sources": sources,
            "capabilities": capabilities
        })
        .to_string();
        self.store
            .conn()
            .execute(
                "UPDATE dataset_version SET source=?1,terms_class=?2,coverage_json=?3 WHERE version_num=?4",
                params![
                    summary.dataset_version.source,
                    summary.dataset_version.terms_class,
                    summary.dataset_version.coverage_json,
                    version
                ],
            )
            .map_err(sync_error)?;
        self.store
            .record_quality_issues(&summary.quality_issues)
            .map_err(sync_error)?;
        if !summary.quality_issues.is_empty() {
            summary.dataset_version.quality = "warning".into();
            self.store
                .conn()
                .execute(
                    "UPDATE dataset_version SET quality='warning' WHERE version_num=?1",
                    params![version],
                )
                .map_err(sync_error)?;
        }
        summary.dataset_version.manifest_hash = self
            .store
            .publish_dataset_version(version)
            .map_err(sync_error)?;
        Ok(summary)
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_stablecoin_page(
        &self,
        client: &reqwest::Client,
        currency: &str,
        from: NaiveDate,
        to: NaiveDate,
        request: &SyncRequest,
        raw_payloads: &mut Vec<RawSyncPayload>,
        warnings: &mut Vec<(String, NaiveDate, String)>,
    ) -> Result<(Vec<FxRateRecord>, String, String), ErrorEnvelope> {
        let defaults = StablecoinFxProvider::default();
        let bitstamp = sync_base_url(request, "bitstamp", defaults.bitstamp_base_url);
        let bs_url = format!(
            "{}/api/v2/ohlc/{}usd/?step=86400&limit=300&start={}&end={}",
            bitstamp.trim_end_matches('/'),
            currency.to_ascii_lowercase(),
            from.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
            to.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
        );
        let (primary_provider, primary_url, secondary_provider, secondary_url) = if currency
            == "USDT"
        {
            (
                "coinbase",
                format!(
                    "{}/products/USDT-USD/candles?granularity=86400&start={from}T00:00:00Z&end={to}T00:00:00Z",
                    sync_base_url(request, "coinbase", defaults.coinbase_base_url)
                        .trim_end_matches('/')
                ),
                "bitstamp",
                bs_url,
            )
        } else {
            (
                "bitstamp",
                bs_url,
                "kraken",
                format!(
                    "{}/0/public/OHLC?pair=USDCUSD&interval=1440&since={}",
                    sync_base_url(request, "kraken", defaults.kraken_base_url)
                        .trim_end_matches('/'),
                    from.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
                ),
            )
        };
        let mut fetch = |provider: &str,
                         endpoint: &str|
         -> Result<Vec<FxRateRecord>, ErrorEnvelope> {
            self.check_sync_control()?;
            let body = self
                .sync_await(get_text_with_retry(client, provider, endpoint, &[], 2))
                .map_err(sync_error)?;
            let parsed = match provider {
                "coinbase" => StablecoinFxProvider::parse_coinbase_candles(
                    "USDT-USD",
                    &body,
                    PLACEHOLDER_VERSION,
                ),
                "bitstamp" => {
                    StablecoinFxProvider::parse_bitstamp_ohlc(currency, &body, PLACEHOLDER_VERSION)
                }
                _ => StablecoinFxProvider::parse_kraken_ohlc(currency, &body, PLACEHOLDER_VERSION),
            }
            .map_err(sync_error)?;
            raw_payloads.push(RawSyncPayload::Text {
                provider: provider.to_string(),
                content_type: "application/json",
                body,
            });
            Ok(parsed
                .into_iter()
                .filter(|r| r.effective_date >= from && r.effective_date < to)
                .collect())
        };
        let primary = fetch(primary_provider, &primary_url);
        let secondary = fetch(secondary_provider, &secondary_url);
        // Cancellation and deadline still abort the whole sync before any provider outcome is
        // interpreted as a data gap.
        self.check_sync_control()?;
        let (mut rates, provider, endpoint) = match (primary, secondary) {
            (Ok(mut primary), Ok(secondary)) if !primary.is_empty() => {
                for rate in &primary {
                    if let Some(other) = secondary
                        .iter()
                        .find(|r| r.effective_date == rate.effective_date)
                    {
                        if other.rate <= Decimal::ZERO
                            || rate.rate <= Decimal::ZERO
                            || (rate.rate - other.rate).abs() > rate.rate * dec_rate_tolerance()
                        {
                            warnings.push((
                                currency.to_string(),
                                rate.effective_date,
                                "fx_price_mismatch".into(),
                            ));
                        }
                    }
                }
                if secondary.is_empty() {
                    warnings.push((
                        currency.to_string(),
                        from,
                        "fx_crosscheck_outside_coverage".into(),
                    ));
                }
                primary.extend(secondary);
                (primary, primary_provider, primary_url)
            }
            (Ok(primary), Err(_)) if !primary.is_empty() => {
                warnings.push((
                    currency.to_string(),
                    from,
                    "fx_crosscheck_unavailable".into(),
                ));
                (primary, primary_provider, primary_url)
            }
            (_, Ok(secondary)) if !secondary.is_empty() => {
                warnings.push((currency.to_string(), from, "fx_primary_unavailable".into()));
                (secondary, secondary_provider, secondary_url)
            }
            // `S-49` ①: one source failing while the other is empty (or both empty/failing) is a
            // coverage gap for this interval, not a reason to abort the crypto bar sync. The
            // caller records `missing_stablecoin_fx`; no placeholder rate is produced.
            _ => return Ok((Vec::new(), primary_provider.into(), primary_url)),
        };
        rates.sort_by_key(|r| r.effective_date);
        Ok((rates, provider.into(), endpoint))
    }

    fn sync_crypto_fee_fx(
        &mut self,
        registry: &delta_data::InstrumentRegistry,
        subscribed: &BTreeSet<String>,
        request: &SyncRequest,
        version: i64,
        summary: &mut SyncSummaryDto,
    ) -> Result<(), ErrorEnvelope> {
        // `S-49` ② root cause: the stablecoin/USD rates synchronized earlier in this same sync
        // belong to the still-unpublished `version`; reading only published versions found none,
        // so every derived asset/USD rate was reported missing. Include the pending version.
        let lookup: HashMap<_, _> = self
            .load_fx_rates_with_pending(version, Some(version))?
            .into_iter()
            .map(|r| {
                (
                    (r.base_currency, r.quote_currency, r.effective_date),
                    r.rate,
                )
            })
            .collect();
        // Second root cause: de-duplicating by base asset processed only the first market
        // (BINANCE sorts before OKX). Every subscribed market contributes; a date is reported
        // missing only when no market could derive the asset/USD rate for it.
        let mut covered: BTreeSet<(String, NaiveDate)> = BTreeSet::new();
        let mut uncovered: BTreeMap<(String, NaiveDate), String> = BTreeMap::new();
        for id in subscribed {
            let Some(instrument) = registry.instruments.get(id) else {
                continue;
            };
            if !registry
                .assets
                .get(&instrument.base_asset_id)
                .is_some_and(|a| a.kind == delta_core::AssetKind::Crypto)
            {
                continue;
            }
            let bars = self
                .store
                .query_bars_at_version(id, "1d", AdjustmentKind::Raw, None, version, None)
                .map_err(sync_error)?;
            let mut rates = Vec::new();
            for bar in bars
                .into_iter()
                .filter(|b| b.session_date >= request.start && b.session_date < request.end)
            {
                if let Some(fx) = resolve_fx_rate_s07(
                    &instrument.quote_asset_id,
                    "USD",
                    bar.session_date,
                    true,
                    &lookup,
                ) {
                    covered.insert((instrument.base_asset_id.clone(), bar.session_date));
                    uncovered.remove(&(instrument.base_asset_id.clone(), bar.session_date));
                    rates.push(FxRateRecord {
                        id: format!(
                            "FX:{}:USD:{}:{}:{version}",
                            instrument.base_asset_id, bar.session_date, bar.source
                        ),
                        base_currency: instrument.base_asset_id.clone(),
                        quote_currency: "USD".into(),
                        rate: bar.close * fx.rate,
                        effective_date: bar.session_date,
                        effective_at: bar.close_at,
                        observed_at: Utc::now(),
                        source: format!("{}+stablecoin", bar.source),
                        quality: fx.quality,
                        carried_from_date: None,
                        dataset_version: version,
                    });
                } else if !covered.contains(&(instrument.base_asset_id.clone(), bar.session_date)) {
                    uncovered
                        .entry((instrument.base_asset_id.clone(), bar.session_date))
                        .or_insert_with(|| id.clone());
                }
            }
            summary.fx_rates_committed += self
                .store
                .upsert_fx_rates(&rates, None)
                .map_err(sync_error)?;
        }
        // Valuation-only (`S-49` ③): recorded against the fee asset, never against bar prices.
        for ((asset, date), _) in uncovered {
            summary.quality_issues.push(source_warning(
                &asset,
                date,
                version,
                "missing_fee_asset_fx",
            ));
        }
        Ok(())
    }
}

fn filter_final_bars_in_range(
    bars: &[BarRecord],
    start: NaiveDate,
    end: NaiveDate,
    now: DateTime<Utc>,
) -> Vec<BarRecord> {
    let mut out: Vec<BarRecord> = bars
        .iter()
        .filter(|b| {
            b.session_date >= start && b.session_date < end && b.close_at <= now && b.is_final
        })
        .cloned()
        .collect();
    out.sort_by_key(|b| b.session_date);
    out.dedup_by_key(|b| b.session_date);
    out
}

fn stamp_trailing_version(id: &mut String, version: i64) {
    if let Some(prefix) = id.strip_suffix(":0") {
        *id = format!("{prefix}:{version}");
    }
}

fn stamp_bars_version(bars: &mut [BarRecord], version: i64) {
    for bar in bars {
        bar.valid_from_version = version;
        stamp_trailing_version(&mut bar.id, version);
    }
}

fn stamp_actions_version(actions: &mut [CorporateActionRecord], version: i64) {
    for action in actions {
        action.factor_version = version;
        stamp_trailing_version(&mut action.id, version);
    }
}

fn stamp_fx_version(rates: &mut [FxRateRecord], version: i64) {
    for rate in rates {
        rate.dataset_version = version;
        stamp_trailing_version(&mut rate.id, version);
    }
}

fn sync_base_url_override(request: &SyncRequest, provider: &str) -> Option<String> {
    request
        .base_urls
        .get(provider)
        .cloned()
        .or_else(|| std::env::var(format!("DELTA_{}_BASE_URL", provider.to_ascii_uppercase())).ok())
}

fn sync_base_url(request: &SyncRequest, provider: &str, default: String) -> String {
    sync_base_url_override(request, provider).unwrap_or(default)
}

fn dec_rate_tolerance() -> Decimal {
    Decimal::new(5, 3)
}

fn sync_error(error: impl std::fmt::Display) -> ErrorEnvelope {
    ErrorEnvelope::new("sync_failed", error.to_string(), true)
}

fn source_warning(instrument: &str, date: NaiveDate, version: i64, kind: &str) -> DataQualityIssue {
    DataQualityIssue {
        id: format!("DQ:{instrument}:{date}:{version}:{kind}"),
        dataset_version: version,
        instrument_id: instrument.into(),
        session_date: date,
        kind: kind.into(),
        sources: "public-sync".into(),
        detail: kind.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_fixture::MarketFixtureServer;
    use crate::paths::{resolve_app_paths, BuildChannel};
    use delta_data::StructuredNoteFields;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    struct StaticStubServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl StaticStubServer {
        fn start(yahoo_body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let stop = Arc::new(AtomicBool::new(false));
            let stop_c = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                while !stop_c.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut buf = [0u8; 4096];
                            let n = stream.read(&mut buf).unwrap_or(0);
                            let req = String::from_utf8_lossy(&buf[..n]);
                            let path = req
                                .lines()
                                .next()
                                .and_then(|l| l.split_whitespace().nth(1))
                                .unwrap_or("/");
                            let (status, body) = if path.starts_with("/v8/finance/chart/") {
                                ("200 OK", yahoo_body)
                            } else if path.contains("/products/USDT-USD/candles") {
                                ("200 OK", "[]")
                            } else {
                                ("404 Not Found", "{}")
                            };
                            let resp = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(resp.as_bytes());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                base_url,
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for StaticStubServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn sync_phase1_does_not_hold_sqlite_write_lock_during_http_fetch() {
        apply_process_quiet_guards();
        let tmp_root = std::env::temp_dir().join(format!(
            "delta_sync_twophase_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&tmp_root);
        let data_dir = tmp_root.join("lib_data");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&data_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        let end_dt = NaiveDate::from_ymd_opt(2026, 1, 25)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let server = MarketFixtureServer::start(end_dt).unwrap();
        server.set_delay_ms(250);
        let stub = StaticStubServer::start("{}");
        let mut base_urls = server.base_urls();
        base_urls.insert("coinbase".into(), stub.base_url.clone());
        base_urls.insert("bitstamp".into(), stub.base_url.clone());
        base_urls.insert("kraken".into(), stub.base_url.clone());

        let req = SyncRequest {
            start: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 1, 6).unwrap(),
            instruments: vec!["OKX:BTC/USDT".into()],
            fx_pairs: vec![("USDT".into(), "USD".into())],
            use_configured_credentials: false,
            base_urls,
        };

        let job = svc.start_market_sync(req, Duration::from_secs(15));
        // While background sync is waiting on delayed HTTP responses, concurrent writes to SQLite
        // (e.g. saving a user note on the main service connection) must succeed immediately.
        std::thread::sleep(Duration::from_millis(60));
        svc.store_mut()
            .save_note_revision(
                "NOTE-CONCURRENT",
                "Saved during sync fetch",
                "general",
                true,
                "concurrent write must not hit SQLITE_BUSY",
                &StructuredNoteFields::default(),
                &[],
            )
            .expect("concurrent SQLite write should succeed while sync is in fetch phase");

        let snap = svc
            .jobs()
            .wait_finished(job, Duration::from_secs(15))
            .unwrap();
        assert_eq!(snap.state, JobState::Succeeded);
        assert!(svc.latest_dataset_version() >= 1);

        drop(svc);
        let _ = fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn sync_backfill_earlier_than_watermark_does_not_fail_or_regress_cursor() {
        apply_process_quiet_guards();
        let tmp_root = std::env::temp_dir().join(format!(
            "delta_sync_backfill_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&tmp_root);
        let data_dir = tmp_root.join("lib_data");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&data_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        let end_dt = NaiveDate::from_ymd_opt(2026, 1, 25)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let server = MarketFixtureServer::start(end_dt).unwrap();
        let stub = StaticStubServer::start("{}");
        let mut base_urls = server.base_urls();
        base_urls.insert("coinbase".into(), stub.base_url.clone());
        base_urls.insert("bitstamp".into(), stub.base_url.clone());
        base_urls.insert("kraken".into(), stub.base_url.clone());

        // First sync a newer window so the checkpoint watermark advances to 2026-01-19.
        let newer = SyncRequest {
            start: NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 1, 20).unwrap(),
            instruments: vec!["OKX:BTC/USDT".into()],
            fx_pairs: vec![("USDT".into(), "USD".into())],
            use_configured_credentials: false,
            base_urls: base_urls.clone(),
        };
        let s1 = svc.sync_market_data(&newer).unwrap();
        assert!(s1.bars_committed > 0);
        let cp_after_first = svc
            .store()
            .get_sync_checkpoint("okx", "bars", "OKX:BTC/USDT:1d:raw")
            .unwrap()
            .expect("checkpoint created");

        // Now backfill an earlier window before `watermark - 7 days`.
        let earlier = SyncRequest {
            start: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
            instruments: vec!["OKX:BTC/USDT".into()],
            fx_pairs: vec![("USDT".into(), "USD".into())],
            use_configured_credentials: false,
            base_urls,
        };
        let s2 = svc.sync_market_data(&earlier).unwrap();
        assert!(s2.bars_committed > 0);
        let cp_after_backfill = svc
            .store()
            .get_sync_checkpoint("okx", "bars", "OKX:BTC/USDT:1d:raw")
            .unwrap()
            .expect("checkpoint preserved");
        assert!(
            cp_after_backfill.cursor >= cp_after_first.cursor,
            "backfill must not regress stored watermark: {} < {}",
            cp_after_backfill.cursor,
            cp_after_first.cursor
        );

        drop(svc);
        let _ = fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn sync_equity_without_eastmoney_alias_degrades_gracefully_instead_of_panicking() {
        apply_process_quiet_guards();
        let tmp_root = std::env::temp_dir().join(format!(
            "delta_sync_no_em_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&tmp_root);
        let data_dir = tmp_root.join("lib_data");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&data_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        // Remove EastMoney alias for NASDAQ:AAPL to verify graceful degradation (`APP-B10`).
        svc.store()
            .conn()
            .execute(
                "DELETE FROM instrument_alias WHERE provider = 'eastmoney' AND instrument_id = 'NASDAQ:AAPL'",
                [],
            )
            .unwrap();
        let stub = StaticStubServer::start(include_str!(
            "../../../tests/fixtures/market_data/yahoo_aapl_2020_split.json"
        ));
        let mut base_urls = BTreeMap::new();
        base_urls.insert("yahoo".into(), stub.base_url.clone());
        base_urls.insert("eastmoney".into(), stub.base_url.clone());
        let req = SyncRequest {
            start: NaiveDate::from_ymd_opt(2020, 8, 25).unwrap(),
            end: NaiveDate::from_ymd_opt(2020, 9, 2).unwrap(),
            instruments: vec!["NASDAQ:AAPL".into()],
            fx_pairs: vec![],
            use_configured_credentials: false,
            base_urls,
        };
        let summary = svc.sync_market_data(&req).unwrap();
        assert!(summary.bars_committed > 0);
        assert!(summary
            .quality_issues
            .iter()
            .any(|q| q.kind == "eastmoney_unavailable"));

        drop(svc);
        let _ = fs::remove_dir_all(&tmp_root);
    }
}
