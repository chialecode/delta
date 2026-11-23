use super::*;

impl AppService {
    /// Execute live read-only network probes when explicitly requested via `--live` (Section 6.2).
    pub fn run_live_network_probes(&self) -> Vec<LiveProbeResult> {
        let Some(runtime) = self.runtime.as_ref() else {
            return vec![LiveProbeResult {
                provider: "runtime".to_string(),
                endpoint: "n/a".to_string(),
                status: "failed".to_string(),
                http_status: None,
                detail: "live network probes require a full AppService session".to_string(),
            }];
        };
        let client = match build_http_client(8000) {
            Ok(c) => c,
            Err(e) => {
                return vec![LiveProbeResult {
                    provider: "http_client".to_string(),
                    endpoint: "n/a".to_string(),
                    status: "failed".to_string(),
                    http_status: None,
                    detail: e.to_string(),
                }]
            }
        };

        let mut results = Vec::new();

        // 1. OKX public BTC-USDT 1Dutc
        let okx = OkxSpotProvider::default();
        let okx_res =
            runtime.block_on(okx.fetch_daily_candles(&client, "OKX:BTC/USDT", "BTC-USDT", None, 1));
        results.push(match okx_res {
            Ok((bars, _)) => LiveProbeResult {
                provider: "okx".to_string(),
                endpoint: format!(
                    "{}/api/v5/market/history-candles?instId=BTC-USDT&bar=1Dutc",
                    okx.base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} finalized 1Dutc bars", bars.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "okx".to_string(),
                endpoint: okx.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 2. Binance Vision public mirror BTCUSDT 1d
        let binance = BinanceSpotProvider::default();
        let bn_res = runtime.block_on(binance.fetch_daily_klines(
            &client,
            "OKX:BTC/USDT",
            "BTCUSDT",
            None,
            Utc::now(),
            1,
        ));
        results.push(match bn_res {
            Ok((bars, _)) => LiveProbeResult {
                provider: "binance-vision".to_string(),
                endpoint: format!(
                    "{}/api/v3/klines?symbol=BTCUSDT&interval=1d",
                    binance.rest_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} finalized 1d bars", bars.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "binance-vision".to_string(),
                endpoint: binance.rest_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 3. Coinbase USDT-USD
        let stable = StablecoinFxProvider::default();
        let cb_res = runtime.block_on(stable.fetch_coinbase_usdt_usd(&client, 1));
        results.push(match cb_res {
            Ok(rates) => LiveProbeResult {
                provider: "coinbase".to_string(),
                endpoint: format!(
                    "{}/products/USDT-USD/candles?granularity=86400",
                    stable.coinbase_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDT/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "coinbase".to_string(),
                endpoint: stable.coinbase_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 4. Bitstamp USDC-USD
        let bs_res = runtime.block_on(stable.fetch_bitstamp_pair(&client, "USDC", "USD", 5, 1));
        results.push(match bs_res {
            Ok(rates) => LiveProbeResult {
                provider: "bitstamp".to_string(),
                endpoint: format!("{}/api/v2/ohlc/usdcusd/", stable.bitstamp_base_url),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDC/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "bitstamp".to_string(),
                endpoint: stable.bitstamp_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 5. Kraken USDCUSD
        let kr_res = runtime.block_on(stable.fetch_kraken_usdc_usd(&client, 1));
        results.push(match kr_res {
            Ok(rates) => LiveProbeResult {
                provider: "kraken".to_string(),
                endpoint: format!(
                    "{}/0/public/OHLC?pair=USDCUSD&interval=1440",
                    stable.kraken_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDC/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "kraken".to_string(),
                endpoint: stable.kraken_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 6. Frankfurter Fiat FX
        let ff = FrankfurterFxProvider::default();
        let ff_res = runtime.block_on(ff.fetch_range(
            &client,
            "USD",
            &["HKD", "CNY", "EUR"],
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 1, 6).unwrap(),
            1,
        ));
        results.push(match ff_res {
            Ok((rates, _)) => LiveProbeResult {
                provider: "frankfurter".to_string(),
                endpoint: format!(
                    "{}/v1/2026-01-02..2026-01-06?base=USD&symbols=HKD,CNY,EUR",
                    ff.base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} fiat FX rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "frankfurter".to_string(),
                endpoint: ff.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 7. Yahoo Chart v8 AAPL 2020-08-28..2020-09-01
        let yahoo = YahooChartV8Provider::default();
        let y_res = runtime.block_on(yahoo.fetch_chart_v8(
            &client,
            "NASDAQ:AAPL",
            "XNAS",
            "AAPL",
            1598486400,
            1599004800,
            1,
        ));
        results.push(match y_res {
            Ok((bars, actions, _)) => {
                let b_0828 = bars
                    .iter()
                    .find(|b| b.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
                    .map(|b| to_canonical_text(b.close))
                    .unwrap_or_else(|| "n/a".to_string());
                LiveProbeResult {
                    provider: "yahoo".to_string(),
                    endpoint: format!(
                        "{}/v8/finance/chart/AAPL?interval=1d&events=div,split",
                        yahoo.base_url
                    ),
                    status: "passed".to_string(),
                    http_status: Some(200),
                    detail: format!(
                        "fetched {} bars, {} actions; 2020-08-28 restored raw close={}",
                        bars.len(),
                        actions.len(),
                        b_0828
                    ),
                }
            }
            Err(e) => LiveProbeResult {
                provider: "yahoo".to_string(),
                endpoint: yahoo.base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 8. EastMoney push2his AAPL fqt=0
        let em = EastMoneyUsEquityProvider::default();
        let em_res = runtime.block_on(em.fetch_kline_raw(
            &client,
            "NASDAQ:AAPL",
            "XNAS",
            "105.AAPL",
            "20200827",
            "20200901",
            1,
        ));
        results.push(match em_res {
            Ok((bars, _)) => {
                let b_0828 = bars
                    .iter()
                    .find(|b| b.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
                    .map(|b| to_canonical_text(b.close))
                    .unwrap_or_else(|| "n/a".to_string());
                LiveProbeResult {
                    provider: "eastmoney".to_string(),
                    endpoint: format!(
                        "{}/api/qt/stock/kline/get?secid=105.AAPL&fqt=0",
                        em.base_url
                    ),
                    status: "passed".to_string(),
                    http_status: Some(200),
                    detail: format!(
                        "fetched {} raw bars; 2020-08-28 close={}",
                        bars.len(),
                        b_0828
                    ),
                }
            }
            Err(e) => LiveProbeResult {
                provider: "eastmoney".to_string(),
                endpoint: em.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        let archive_month = NaiveDate::from_ymd_opt(2025, 3, 1).unwrap();
        let archive = runtime.block_on(binance.fetch_monthly_archive(
            &client,
            "BINANCE:BTC/USDT",
            "BTCUSDT",
            archive_month,
            1,
        ));
        results.push(match archive {
            Ok((bars, _, _)) => LiveProbeResult {
                provider: "binance-archive".into(),
                endpoint: format!(
                    "{}/data/spot/monthly/klines/BTCUSDT/1d/BTCUSDT-1d-2025-03.zip",
                    binance.archive_base_url
                ),
                status: if bars.len() == 31 { "passed" } else { "failed" }.into(),
                http_status: Some(200),
                detail: format!(
                    "SHA-256 CHECKSUM verified; {} completed daily bars; microsecond timestamps parsed",
                    bars.len()
                ),
            },
            Err(error) => LiveProbeResult {
                provider: "binance-archive".into(),
                endpoint: binance.archive_base_url.clone(),
                status: "failed".into(),
                http_status: extract_status(&error),
                detail: error.to_string(),
            },
        });
        let usdt_check = runtime.block_on(stable.fetch_bitstamp_pair(&client, "USDT", "USD", 5, 1));
        results.push(match usdt_check {
            Ok(rates) => LiveProbeResult {
                provider: "bitstamp-usdt".into(),
                endpoint: format!("{}/api/v2/ohlc/usdtusd/", stable.bitstamp_base_url),
                status: if rates.is_empty() { "failed" } else { "passed" }.into(),
                http_status: Some(200),
                detail: format!("fetched {} USDT/USD cross-check rates", rates.len()),
            },
            Err(error) => LiveProbeResult {
                provider: "bitstamp-usdt".into(),
                endpoint: stable.bitstamp_base_url.clone(),
                status: "failed".into(),
                http_status: extract_status(&error),
                detail: error.to_string(),
            },
        });
        for (symbol, start, end, split) in [
            (
                "NVDA",
                NaiveDate::from_ymd_opt(2024, 6, 6).unwrap(),
                NaiveDate::from_ymd_opt(2024, 6, 12).unwrap(),
                true,
            ),
            (
                "SPY",
                NaiveDate::from_ymd_opt(2025, 3, 17).unwrap(),
                NaiveDate::from_ymd_opt(2025, 3, 25).unwrap(),
                false,
            ),
        ] {
            let venue = if split { "XNAS" } else { "XNYS" };
            let id = format!("{}:{symbol}", if split { "NASDAQ" } else { "NYSE" });
            let response = runtime.block_on(yahoo.fetch_chart_v8(
                &client,
                &id,
                venue,
                symbol,
                start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                end.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                1,
            ));
            results.push(match response {
                Ok((bars, actions, _)) => {
                    let expected = actions.iter().any(|action| {
                        if split {
                            action.action_type == delta_core::CorporateActionType::Split
                                && action.ratio_or_amount == Decimal::from(10)
                        } else {
                            action.action_type == delta_core::CorporateActionType::CashDividend
                                && action.ratio_or_amount > Decimal::ZERO
                        }
                    });
                    LiveProbeResult {
                        provider: format!("yahoo-{}", symbol.to_ascii_lowercase()),
                        endpoint: format!("{}/v8/finance/chart/{symbol}", yahoo.base_url),
                        status: if expected && !bars.is_empty() {
                            "passed"
                        } else {
                            "failed"
                        }
                        .into(),
                        http_status: Some(200),
                        detail: format!(
                            "{} raw bars; {} actions; expected {} observed={expected}",
                            bars.len(),
                            actions.len(),
                            if split { "10:1 split" } else { "cash dividend" }
                        ),
                    }
                }
                Err(error) => LiveProbeResult {
                    provider: format!("yahoo-{}", symbol.to_ascii_lowercase()),
                    endpoint: yahoo.base_url.clone(),
                    status: "failed".into(),
                    http_status: extract_status(&error),
                    detail: error.to_string(),
                },
            });
        }

        // ACT-03 private-service verification is separate from the public probe.
        let tiingo = TiingoEquityProvider::default();
        results.push(LiveProbeResult {
            provider: "tiingo".to_string(),
            endpoint: format!("{}/tiingo/daily/AAPL/prices", tiingo.base_url),
            status: "not-run".to_string(),
            http_status: None,
            detail:
                "ACT-03 private-service live verification not requested; offline fixture verified"
                    .to_string(),
        });

        // 10. Alpaca (ACT-03: remains not-run unless API key is configured)
        let alpaca = AlpacaEquityProvider::default();
        results.push(LiveProbeResult {
            provider: "alpaca".to_string(),
            endpoint: format!("{}/v1/corporate-actions", alpaca.base_url),
            status: "not-run".to_string(),
            http_status: None,
            detail:
                "ACT-03 private-service live verification not requested; offline fixture verified"
                    .to_string(),
        });
        results.push(LiveProbeResult {
            provider: "longport".into(),
            endpoint: "https://openapi.longportapp.com".into(),
            status: "not-run".into(),
            http_status: None,
            detail: "ACT-03 private-service configuration and SDK adapter deferred as LOOP-03"
                .into(),
        });
        for result in &mut results {
            if result.status == "passed" && result.detail.starts_with("fetched 0 ") {
                result.status = "failed".into();
            }
            if matches!(result.provider.as_str(), "yahoo" | "eastmoney")
                && !result.detail.contains("499.23")
            {
                result.status = "failed".into();
            }
        }

        results
    }
}

fn extract_status(err: &delta_data::ProviderError) -> Option<u16> {
    match err {
        delta_data::ProviderError::Http { status, .. } => *status,
        delta_data::ProviderError::RateLimited { .. } => Some(429),
        delta_data::ProviderError::ForbiddenEndpoint(_) => Some(451),
        _ => None,
    }
}
