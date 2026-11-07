use super::*;
use chrono::Datelike;
use delta_data::get_text_with_retry;
use std::collections::BTreeSet;

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

impl AppService {
    /// The worker holds the same directory lock for its entire lifetime.
    pub fn start_market_sync(&self, request: SyncRequest, timeout: Duration) -> JobId {
        let paths = self.paths.clone();
        let lease = self._lock.clone();
        self.jobs
            .spawn_controlled_job("market_sync", timeout, move |cancel, state, deadline| {
                let mut worker = AppService::open_with_lease(paths, "USD", "USD", "UTC", lease)?;
                worker.sync_control = Some((cancel, deadline, state));
                worker.sync_market_data(&request).map(|_| ())
            })
    }

    fn check_sync_control(&self) -> Result<(), ErrorEnvelope> {
        if let Some((cancel, deadline, _)) = &self.sync_control {
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
    /// Synchronize subscriptions through the same service used by all consumers.
    /// Failure rolls back data, version publication and every cursor together.
    pub fn sync_market_data(
        &mut self,
        request: &SyncRequest,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
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
            if let Some(base) = request.base_urls.get(provider).cloned().or_else(|| {
                std::env::var(format!("DELTA_{}_BASE_URL", provider.to_ascii_uppercase())).ok()
            }) {
                delta_data::validate_provider_base_url(&base).map_err(sync_error)?;
            }
        }
        self.store
            .conn()
            .execute_batch("SAVEPOINT market_sync")
            .map_err(|e| ErrorEnvelope::new("sync_begin", e.to_string(), false))?;
        let mut result = self.sync_market_inner(request);
        // Cancellation/deadline and the durable commit share one state lock. A cancellation
        // accepted before RELEASE rolls everything back; after RELEASE the job is succeeded.
        let state = self
            .sync_control
            .as_ref()
            .map(|(_, _, state)| Arc::clone(state));
        let mut publication_guard = state.as_ref().map(|state| state.lock().unwrap());
        if result.is_ok() {
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
        }
        result
    }

    fn sync_market_inner(
        &mut self,
        request: &SyncRequest,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
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
            let mut stmt = self.store.conn().prepare("SELECT base_currency FROM fx_subscription UNION SELECT quote_currency FROM fx_subscription").map_err(sync_error)?;
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
        let dsv = self.store.create_dataset_version("public-sync", "public_open,unofficial_personal",
            &serde_json::json!({"start": request.start, "end": request.end, "instruments": subscribed}).to_string(),
            "exact", "reconstructed_history").map_err(sync_error)?;
        let version = dsv.version_num;
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
        let mut summary = SyncSummaryDto {
            dataset_version: dsv,
            bars_committed: 0,
            corporate_actions_committed: 0,
            fx_rates_committed: 0,
            quality_issues: Vec::new(),
        };
        let base_url = |provider: &str, default: String| {
            request
                .base_urls
                .get(provider)
                .cloned()
                .or_else(|| {
                    std::env::var(format!("DELTA_{}_BASE_URL", provider.to_ascii_uppercase())).ok()
                })
                .unwrap_or(default)
        };
        for instrument_id in &subscribed {
            self.check_sync_control()?;
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
            let start = old_cp
                .as_ref()
                .and_then(|cp| NaiveDate::parse_from_str(&cp.cursor, "%Y-%m-%d").ok())
                .and_then(|d| d.checked_sub_days(chrono::Days::new(7)))
                .map(|d| d.max(request.start))
                .unwrap_or(request.start);
            let mut bars = Vec::new();
            let mut actions = Vec::new();
            let mut actual_base;
            if provider == "okx" {
                let p = OkxSpotProvider {
                    base_url: base_url("okx", OkxSpotProvider::default().base_url),
                };
                actual_base = p.base_url.clone();
                let symbol = registry.external_symbol_for("okx", instrument_id).unwrap();
                let mut cursor = Some(end_at.timestamp_millis().to_string());
                loop {
                    self.check_sync_control()?;
                    let (page, body) = self
                        .runtime
                        .block_on(p.fetch_daily_candles(
                            &client,
                            instrument_id,
                            &symbol,
                            cursor.as_deref(),
                            version,
                        ))
                        .map_err(sync_error)?;
                    self.store
                        .store_raw_payload(provider, "application/json", &body)
                        .map_err(sync_error)?;
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
                p.rest_base_url = base_url("binance", p.rest_base_url);
                p.archive_base_url = base_url("binance_archive", p.archive_base_url);
                actual_base = p.rest_base_url.clone();
                let symbol = registry
                    .external_symbol_for("binance", instrument_id)
                    .unwrap();
                let mut rest_start = start;
                if old_cp.is_none() && start < now.date_naive().with_day(1).unwrap() {
                    let mut month = start.with_day(1).unwrap();
                    let current_month = now.date_naive().with_day(1).unwrap();
                    while month < request.end && month < current_month {
                        self.check_sync_control()?;
                        match self.runtime.block_on(p.fetch_monthly_archive(
                            &client,
                            instrument_id,
                            &symbol,
                            month,
                            version,
                        )) {
                            Ok((page, bytes, checksum)) => {
                                self.store
                                    .store_raw_bytes("binance-archive", "application/zip", &bytes)
                                    .map_err(sync_error)?;
                                self.store
                                    .store_raw_payload("binance-archive", "text/plain", &checksum)
                                    .map_err(sync_error)?;
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
                                summary.quality_issues.push(source_warning(
                                    instrument_id,
                                    month,
                                    version,
                                    "archive_unavailable_rest_fallback",
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
                        .runtime
                        .block_on(p.fetch_daily_klines(
                            &client,
                            instrument_id,
                            &symbol,
                            Some(cursor),
                            now,
                            version,
                        ))
                        .map_err(sync_error)?;
                    self.store
                        .store_raw_payload(provider, "application/json", &body)
                        .map_err(sync_error)?;
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
                    base_url: base_url("yahoo", YahooChartV8Provider::default().base_url),
                };
                actual_base = p.base_url.clone();
                let venue = if inst.venue == "NASDAQ" {
                    "XNAS"
                } else {
                    "XNYS"
                };
                // Yahoo adjusts against all subsequent splits: fetch through acquisition time,
                // then bound publication to the requested historical interval.
                let fetched = self.runtime.block_on(p.fetch_chart_v8(
                    &client,
                    instrument_id,
                    venue,
                    &inst.symbol,
                    start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                    now.timestamp(),
                    version,
                ));
                let em = EastMoneyUsEquityProvider {
                    base_url: base_url("eastmoney", EastMoneyUsEquityProvider::default().base_url),
                };
                let secid = registry
                    .external_symbol_for("eastmoney", instrument_id)
                    .unwrap();
                let alternate = self.runtime.block_on(em.fetch_kline_raw(
                    &client,
                    instrument_id,
                    venue,
                    &secid,
                    &start.format("%Y%m%d").to_string(),
                    &request.end.format("%Y%m%d").to_string(),
                    version,
                ));
                let mut public_error = None;
                match (fetched, alternate) {
                    (Ok((primary, ca, body)), Ok((secondary, em_body))) => {
                        self.store
                            .store_raw_payload("yahoo", "application/json", &body)
                            .map_err(sync_error)?;
                        self.store
                            .store_raw_payload("eastmoney", "application/json", &em_body)
                            .map_err(sync_error)?;
                        summary.quality_issues.extend(
                            cross_validate_bars_and_actions(
                                instrument_id,
                                &primary,
                                &secondary,
                                &ca,
                                version,
                            )
                            .issues,
                        );
                        bars = primary;
                        actions = ca;
                    }
                    (Ok((primary, ca, body)), Err(_)) => {
                        self.store
                            .store_raw_payload("yahoo", "application/json", &body)
                            .map_err(sync_error)?;
                        bars = primary;
                        actions = ca;
                        summary.quality_issues.push(source_warning(
                            instrument_id,
                            start,
                            version,
                            "eastmoney_unavailable",
                        ));
                    }
                    (Err(_), Ok((secondary, body))) => {
                        self.store
                            .store_raw_payload("eastmoney", "application/json", &body)
                            .map_err(sync_error)?;
                        bars = secondary;
                        summary.quality_issues.push(source_warning(
                            instrument_id,
                            start,
                            version,
                            "missing_corporate_action",
                        ));
                    }
                    (Err(e), Err(_)) => public_error = Some(e),
                }
                if let Some(token) = tiingo_token.as_deref() {
                    let p = TiingoEquityProvider {
                        base_url: base_url("tiingo", TiingoEquityProvider::default().base_url),
                    };
                    match self.runtime.block_on(p.fetch_daily(
                        &client,
                        token,
                        instrument_id,
                        venue,
                        &inst.symbol,
                        start,
                        request.end,
                        version,
                    )) {
                        Ok((primary, ca, body)) => {
                            if bars.is_empty() {
                                summary.quality_issues.push(source_warning(
                                    instrument_id,
                                    start,
                                    version,
                                    "yahoo_crosscheck_unavailable",
                                ));
                            }
                            summary.quality_issues.extend(
                                cross_validate_bars_and_actions(
                                    instrument_id,
                                    &primary,
                                    &bars,
                                    &ca,
                                    version,
                                )
                                .issues,
                            );
                            self.store
                                .store_raw_payload("tiingo", "application/json", &body)
                                .map_err(sync_error)?;
                            bars = primary;
                            actions = ca;
                            actual_base = p.base_url;
                        }
                        Err(_) => summary.quality_issues.push(source_warning(
                            instrument_id,
                            start,
                            version,
                            "tiingo_unavailable",
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
            let cp = SyncCheckpointRecord {
                provider: bars.last().unwrap().source.clone(),
                resource_kind: "bars".into(),
                resource_key: resource,
                connection_id: None,
                cursor: bars.last().unwrap().session_date.to_string(),
                last_committed_at: now.to_rfc3339(),
                calendar_version: Some(special_table().version_tag.clone()),
                base_url: Some(actual_base),
            };
            let (count, issues) = self
                .store
                .commit_bars_and_checkpoint(&bars, version, Some(&cp))
                .map_err(sync_error)?;
            summary.bars_committed += count;
            summary.quality_issues.extend(issues);
            summary.corporate_actions_committed += self
                .store
                .upsert_corporate_actions(&actions)
                .map_err(sync_error)?;
            let venue = if inst.venue == "NASDAQ" {
                "XNAS"
            } else if inst.venue == "NYSE" {
                "XNYS"
            } else {
                "CRYPTO_24_7"
            };
            let (_, gaps) = self
                .store
                .analyze_coverage_gaps(
                    venue,
                    instrument_id,
                    start,
                    request.end.pred_opt().unwrap(),
                    version,
                )
                .map_err(sync_error)?;
            for gap in gaps {
                summary.quality_issues.push(source_warning(
                    instrument_id,
                    gap.start_date,
                    version,
                    &gap.gap_kind,
                ));
            }
        }
        // Independent OKX/Binance markets are cross-checked when both are subscribed.
        for id in &subscribed {
            let inst = &registry.instruments[id];
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
        for id in &subscribed {
            currencies.insert(registry.instruments[id].quote_asset_id.clone());
        }
        for currency in currencies {
            self.check_sync_control()?;
            if currency == "USD" {
                continue;
            }
            if registry
                .assets
                .get(&currency)
                .is_some_and(|a| a.kind == delta_core::AssetKind::Crypto)
            {
                // Derive asset/USD from the completed asset/USDT bar and same-day USDT/USD.
                // Stablecoin rates are synchronized first by the separate pass below.
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
            let mut from = checkpoint
                .and_then(|cp| NaiveDate::parse_from_str(&cp.cursor, "%Y-%m-%d").ok())
                .and_then(|d| d.checked_sub_days(chrono::Days::new(4)))
                .map(|d| d.max(request.start))
                .unwrap_or(request.start);
            while from < request.end {
                self.check_sync_control()?;
                let to = (from + chrono::Duration::days(299)).min(request.end);
                let (mut rates, provider, endpoint) =
                    if matches!(currency.as_str(), "USDT" | "USDC") {
                        self.sync_stablecoin_page(
                            &currency,
                            from,
                            to,
                            version,
                            request,
                            &mut summary,
                        )?
                    } else {
                        let p = FrankfurterFxProvider {
                            base_url: base_url(
                                "frankfurter",
                                FrankfurterFxProvider::default().base_url,
                            ),
                        };
                        let (rates, body) = self
                            .runtime
                            .block_on(p.fetch_range(
                                &client,
                                "USD",
                                &[&currency],
                                from,
                                to,
                                version,
                            ))
                            .map_err(sync_error)?;
                        self.store
                            .store_raw_payload("frankfurter", "application/json", &body)
                            .map_err(sync_error)?;
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
                    return Err(ErrorEnvelope::new(
                        "missing_fx",
                        format!("no {currency}/USD rates"),
                        true,
                    ));
                }
                let cp = SyncCheckpointRecord {
                    provider,
                    resource_kind: "fx".into(),
                    resource_key: format!("{currency}/USD"),
                    connection_id: None,
                    cursor: rates.last().unwrap().effective_date.to_string(),
                    last_committed_at: now.to_rfc3339(),
                    calendar_version: None,
                    base_url: Some(endpoint.split('?').next().unwrap().into()),
                };
                summary.fx_rates_committed += self
                    .store
                    .upsert_fx_rates(&rates, Some(&cp))
                    .map_err(sync_error)?;
                from = to;
            }
        }
        self.sync_crypto_fee_fx(&registry, &subscribed, request, version, &mut summary)?;
        for id in &request.instruments {
            let instrument = &registry.instruments[id];
            self.store.conn().execute("INSERT OR IGNORE INTO subscription(instrument_id,venue,symbol,auto_reason,created_at) VALUES (?1,?2,?3,'explicit_sync',?4)",params![id,instrument.venue,instrument.symbol,now.to_rfc3339()]).map_err(sync_error)?;
        }
        for (base, quote) in &request.fx_pairs {
            self.store.conn().execute("INSERT OR IGNORE INTO fx_subscription(base_currency,quote_currency,auto_reason,created_at) VALUES (?1,?2,'explicit_sync',?3)",params![base.to_ascii_uppercase(),quote.to_ascii_uppercase(),now.to_rfc3339()]).map_err(sync_error)?;
        }
        let mut source_query=self.store.conn().prepare("SELECT DISTINCT source FROM bar WHERE valid_from_version<=?1 AND (valid_to_version IS NULL OR valid_to_version>?1) UNION SELECT DISTINCT source FROM fx_rate WHERE dataset_version<=?1 UNION SELECT DISTINCT source FROM corporate_action WHERE factor_version<=?1 ORDER BY 1").map_err(sync_error)?;
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
        let mut endpoint_query=self.store.conn().prepare("SELECT provider,base_url FROM sync_checkpoint WHERE last_committed_at=?1 ORDER BY provider,resource_key").map_err(sync_error)?;
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
            self.store.conn().execute("INSERT INTO provider_connection(id,provider,credential_ref,base_url,capabilities_json,status) VALUES (?1,?2,NULL,?3,?4,'connected') ON CONFLICT(id) DO UPDATE SET base_url=excluded.base_url,capabilities_json=excluded.capabilities_json,status=excluded.status",params![format!("public:{provider}"),provider,capability.base_url,encoded]).map_err(sync_error)?;
            capabilities.push(capability);
        }
        summary.dataset_version.coverage_json=serde_json::json!({"start":request.start,"end":request.end,"instruments":subscribed,"fx_pairs":request.fx_pairs,"sources":sources,"capabilities":capabilities}).to_string();
        self.store.conn().execute("UPDATE dataset_version SET source=?1,terms_class=?2,coverage_json=?3 WHERE version_num=?4",params![summary.dataset_version.source,summary.dataset_version.terms_class,summary.dataset_version.coverage_json,version]).map_err(sync_error)?;
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

    fn sync_stablecoin_page(
        &mut self,
        currency: &str,
        from: NaiveDate,
        to: NaiveDate,
        version: i64,
        request: &SyncRequest,
        summary: &mut SyncSummaryDto,
    ) -> Result<(Vec<FxRateRecord>, String, String), ErrorEnvelope> {
        let client = build_http_client(15000).map_err(sync_error)?;
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
            ("coinbase", format!("{}/products/USDT-USD/candles?granularity=86400&start={from}T00:00:00Z&end={to}T00:00:00Z", sync_base_url(request, "coinbase", defaults.coinbase_base_url).trim_end_matches('/')), "bitstamp", bs_url)
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
                .runtime
                .block_on(get_text_with_retry(&client, provider, endpoint, &[], 2))
                .map_err(sync_error)?;
            self.store
                .store_raw_payload(provider, "application/json", &body)
                .map_err(sync_error)?;
            let parsed = match provider {
                "coinbase" => {
                    StablecoinFxProvider::parse_coinbase_candles("USDT-USD", &body, version)
                }
                "bitstamp" => StablecoinFxProvider::parse_bitstamp_ohlc(currency, &body, version),
                _ => StablecoinFxProvider::parse_kraken_ohlc(currency, &body, version),
            }
            .map_err(sync_error)?;
            Ok(parsed
                .into_iter()
                .filter(|r| r.effective_date >= from && r.effective_date < to)
                .collect())
        };
        let primary = fetch(primary_provider, &primary_url);
        let secondary = fetch(secondary_provider, &secondary_url);
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
                            summary.quality_issues.push(source_warning(
                                currency,
                                rate.effective_date,
                                version,
                                "fx_price_mismatch",
                            ));
                        }
                    }
                }
                if secondary.is_empty() {
                    summary.quality_issues.push(source_warning(
                        currency,
                        from,
                        version,
                        "fx_crosscheck_outside_coverage",
                    ));
                }
                primary.extend(secondary);
                (primary, primary_provider, primary_url)
            }
            (Ok(primary), Err(_)) if !primary.is_empty() => {
                summary.quality_issues.push(source_warning(
                    currency,
                    from,
                    version,
                    "fx_crosscheck_unavailable",
                ));
                (primary, primary_provider, primary_url)
            }
            (_, Ok(secondary)) if !secondary.is_empty() => {
                summary.quality_issues.push(source_warning(
                    currency,
                    from,
                    version,
                    "fx_primary_unavailable",
                ));
                (secondary, secondary_provider, secondary_url)
            }
            (Err(error), _) => return Err(error),
            _ => {
                return Err(ErrorEnvelope::new(
                    "missing_fx",
                    format!("no completed {currency}/USD rates in requested interval"),
                    true,
                ))
            }
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
        let lookup: HashMap<_, _> = self
            .load_fx_rates(version)?
            .into_iter()
            .map(|r| {
                (
                    (r.base_currency, r.quote_currency, r.effective_date),
                    r.rate,
                )
            })
            .collect();
        let mut seen = BTreeSet::new();
        for id in subscribed {
            let instrument = &registry.instruments[id];
            if !registry
                .assets
                .get(&instrument.base_asset_id)
                .is_some_and(|a| a.kind == delta_core::AssetKind::Crypto)
                || !seen.insert(instrument.base_asset_id.clone())
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
                } else {
                    summary.quality_issues.push(source_warning(
                        id,
                        bar.session_date,
                        version,
                        "missing_fee_asset_fx",
                    ));
                }
            }
            summary.fx_rates_committed += self
                .store
                .upsert_fx_rates(&rates, None)
                .map_err(sync_error)?;
        }
        Ok(())
    }
}

fn sync_base_url(request: &SyncRequest, provider: &str, default: String) -> String {
    request
        .base_urls
        .get(provider)
        .cloned()
        .or_else(|| std::env::var(format!("DELTA_{}_BASE_URL", provider.to_ascii_uppercase())).ok())
        .unwrap_or(default)
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
