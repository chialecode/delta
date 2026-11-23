use std::collections::BTreeMap;

use chrono::{NaiveDate, TimeZone, Utc};
use delta_core::{
    parse_decimal_lossless, parse_json_raw_decimal, DirectDecimal, FxRateRecord, QualityStatus,
    TermsClass,
};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::providers::http::{
    build_provider_url, get_text_with_retry, provider_timestamp, sanitize_untrusted_text,
};
use crate::providers::{ProviderCapabilities, ProviderError};

fn ensure_positive_fx(provider: &str, rate: Decimal) -> Result<Decimal, ProviderError> {
    if rate <= Decimal::ZERO {
        return Err(ProviderError::Parse {
            provider: provider.into(),
            detail: format!("non-positive FX rate '{rate}'"),
        });
    }
    Ok(rate)
}

fn parse_pair_currencies(provider: &str, product: &str) -> Result<(String, String), ProviderError> {
    let parts: Vec<&str> = product.split(['-', '/']).collect();
    if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
        return Err(ProviderError::Parse {
            provider: provider.into(),
            detail: format!(
                "invalid currency pair '{}'",
                sanitize_untrusted_text(product, 64)
            ),
        });
    }
    Ok((
        parts[0].trim().to_ascii_uppercase(),
        parts[1].trim().to_ascii_uppercase(),
    ))
}

// ============================================================================
// 3. Stablecoin FX Providers (Coinbase USDT-USD, Bitstamp usdcusd/usdtusd, Kraken USDCUSD)
// ============================================================================

#[derive(Debug, Clone)]
pub struct StablecoinFxProvider {
    pub coinbase_base_url: String,
    pub bitstamp_base_url: String,
    pub kraken_base_url: String,
}

impl Default for StablecoinFxProvider {
    fn default() -> Self {
        Self {
            coinbase_base_url: "https://api.exchange.coinbase.com".to_string(),
            bitstamp_base_url: "https://www.bitstamp.net".to_string(),
            kraken_base_url: "https://api.kraken.com".to_string(),
        }
    }
}

impl StablecoinFxProvider {
    pub fn capabilities_for(&self, provider: &str) -> Option<ProviderCapabilities> {
        let base_url = match provider {
            "coinbase" => &self.coinbase_base_url,
            "bitstamp" => &self.bitstamp_base_url,
            "kraken" => &self.kraken_base_url,
            _ => return None,
        };
        Some(ProviderCapabilities {
            provider: provider.into(),
            base_url: base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 0,
            terms_class: TermsClass::PublicOpen,
            supports_equities: false,
            supports_crypto: false,
            supports_fx: true,
            supports_corporate_actions: false,
        })
    }

    /// Parse Coinbase Exchange `/products/{BASE}-{QUOTE}/candles` JSON:
    /// `[[time, low, high, open, close, volume], ...]`.
    /// Explicitly rejects `USDC-USD` per S-03 / Section 3.1 (Coinbase has no `USDC-USD` product).
    pub fn parse_coinbase_candles(
        product: &str,
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        if product.eq_ignore_ascii_case("USDC-USD") || product.eq_ignore_ascii_case("USDC/USD") {
            return Err(ProviderError::UnsupportedProduct {
                provider: "coinbase".into(),
                product: product.to_string(),
                reason: "Coinbase Exchange has no USDC-USD market (404); use Bitstamp + Kraken"
                    .into(),
            });
        }
        let (base, quote) = parse_pair_currencies("coinbase", product)?;
        let rows: Vec<Vec<Box<serde_json::value::RawValue>>> = serde_json::from_str(raw_json)
            .map_err(|e| ProviderError::Parse {
                provider: "coinbase".into(),
                detail: e.to_string(),
            })?;
        let mut out = Vec::new();
        let observed_at = Utc::now();
        for row in rows {
            if row.len() < 5 {
                return Err(ProviderError::Parse {
                    provider: "coinbase".into(),
                    detail: "short candle row".into(),
                });
            }
            let ts_sec: i64 = row[0]
                .get()
                .trim()
                .parse()
                .map_err(|_| ProviderError::Parse {
                    provider: "coinbase".into(),
                    detail: "invalid timestamp".into(),
                })?;
            let open_at = provider_timestamp(ts_sec)?;
            let effective_date = open_at.date_naive();
            if open_at + chrono::Duration::days(1) > observed_at {
                continue;
            }
            let close_rate = ensure_positive_fx(
                "coinbase",
                parse_json_raw_decimal(row[4].get()).map_err(|e| ProviderError::Parse {
                    provider: "coinbase".into(),
                    detail: e.to_string(),
                })?,
            )?;
            out.push(FxRateRecord {
                id: format!("FX:{base}:{quote}:{effective_date}:coinbase:{dataset_version}"),
                base_currency: base.clone(),
                quote_currency: quote.clone(),
                rate: close_rate,
                effective_date,
                effective_at: open_at + chrono::Duration::days(1),
                observed_at,
                source: "coinbase".to_string(),
                quality: QualityStatus::Exact,
                carried_from_date: None,
                dataset_version,
            });
        }
        out.sort_by_key(|r| r.effective_date);
        Ok(out)
    }

    /// Parse Bitstamp `/api/v2/ohlc/{base}{quote}/` JSON with explicit quote currency.
    pub fn parse_bitstamp_ohlc_pair(
        base_currency: &str,
        quote_currency: &str,
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct BitstampBar {
            timestamp: String,
            close: String,
        }
        #[derive(Deserialize)]
        struct BitstampData {
            ohlc: Vec<BitstampBar>,
        }
        #[derive(Deserialize)]
        struct BitstampEnv {
            data: BitstampData,
        }
        let env: BitstampEnv =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "bitstamp".into(),
                detail: e.to_string(),
            })?;
        let mut out = Vec::new();
        let observed_at = Utc::now();
        let base = base_currency.trim().to_ascii_uppercase();
        let quote = quote_currency.trim().to_ascii_uppercase();
        if base.is_empty() || quote.is_empty() {
            return Err(ProviderError::Parse {
                provider: "bitstamp".into(),
                detail: "base_currency and quote_currency must be non-empty".into(),
            });
        }
        for bar in env.data.ohlc {
            let ts: i64 = bar.timestamp.parse().map_err(|_| ProviderError::Parse {
                provider: "bitstamp".into(),
                detail: "invalid timestamp".into(),
            })?;
            let open_at = provider_timestamp(ts)?;
            let effective_date = open_at.date_naive();
            if open_at + chrono::Duration::days(1) > observed_at {
                continue;
            }
            let rate = ensure_positive_fx(
                "bitstamp",
                parse_decimal_lossless(&bar.close).map_err(|e| ProviderError::Parse {
                    provider: "bitstamp".into(),
                    detail: e.to_string(),
                })?,
            )?;
            out.push(FxRateRecord {
                id: format!("FX:{base}:{quote}:{effective_date}:bitstamp:{dataset_version}"),
                base_currency: base.clone(),
                quote_currency: quote.clone(),
                rate,
                effective_date,
                effective_at: open_at + chrono::Duration::days(1),
                observed_at,
                source: "bitstamp".to_string(),
                quality: QualityStatus::Exact,
                carried_from_date: None,
                dataset_version,
            });
        }
        out.sort_by_key(|r| r.effective_date);
        Ok(out)
    }

    /// Parse Bitstamp `/api/v2/ohlc/{usdcusd|usdtusd}/` JSON (defaults quote to `"USD"`).
    pub fn parse_bitstamp_ohlc(
        base_currency: &str,
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        Self::parse_bitstamp_ohlc_pair(base_currency, "USD", raw_json, dataset_version)
    }

    /// Parse Kraken `/0/public/OHLC?pair=USDCUSD&interval=1440` JSON (recent ~720 bars cross-check).
    pub fn parse_kraken_ohlc(
        base_currency: &str,
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct KrakenEnv {
            error: Vec<String>,
            result: Option<BTreeMap<String, serde_json::Value>>,
        }
        let env: KrakenEnv = serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
            provider: "kraken".into(),
            detail: e.to_string(),
        })?;
        if !env.error.is_empty() {
            return Err(ProviderError::Parse {
                provider: "kraken".into(),
                detail: sanitize_untrusted_text(&env.error.join("; "), 200),
            });
        }
        let mut out = Vec::new();
        let observed_at = Utc::now();
        let base = base_currency.to_ascii_uppercase();
        if let Some(map) = env.result {
            for (k, v) in map {
                if k == "last" {
                    continue;
                }
                if let Some(arr) = v.as_array() {
                    for item in arr {
                        if let Some(cols) = item.as_array() {
                            if cols.len() >= 5 {
                                let ts = cols[0].as_i64().ok_or_else(|| ProviderError::Parse {
                                    provider: "kraken".into(),
                                    detail: "invalid candle timestamp".into(),
                                })?;
                                let open_at = provider_timestamp(ts)?;
                                let effective_date = open_at.date_naive();
                                if open_at + chrono::Duration::days(1) > observed_at {
                                    continue;
                                }
                                let close_str =
                                    cols[4].as_str().ok_or_else(|| ProviderError::Parse {
                                        provider: "kraken".into(),
                                        detail: "invalid candle close".into(),
                                    })?;
                                let rate = ensure_positive_fx(
                                    "kraken",
                                    parse_decimal_lossless(close_str).map_err(|e| {
                                        ProviderError::Parse {
                                            provider: "kraken".into(),
                                            detail: e.to_string(),
                                        }
                                    })?,
                                )?;
                                out.push(FxRateRecord {
                                    id: format!(
                                        "FX:{base}:USD:{effective_date}:kraken:{dataset_version}"
                                    ),
                                    base_currency: base.clone(),
                                    quote_currency: "USD".to_string(),
                                    rate,
                                    effective_date,
                                    effective_at: open_at + chrono::Duration::days(1),
                                    observed_at,
                                    source: "kraken".to_string(),
                                    quality: QualityStatus::Exact,
                                    carried_from_date: None,
                                    dataset_version,
                                });
                            }
                        }
                    }
                }
            }
        }
        out.sort_by_key(|r| r.effective_date);
        Ok(out)
    }

    pub async fn fetch_coinbase_usdt_usd(
        &self,
        client: &Client,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        let url = build_provider_url(
            "coinbase",
            &self.coinbase_base_url,
            &["products", "USDT-USD", "candles"],
            &[("granularity", "86400")],
            false,
        )?;
        let body = get_text_with_retry(client, "coinbase", &url, &[], 2).await?;
        Self::parse_coinbase_candles("USDT-USD", &body, dataset_version)
    }

    pub async fn fetch_bitstamp_pair(
        &self,
        client: &Client,
        base_currency: &str,
        quote_currency: &str,
        limit: usize,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        let pair = format!(
            "{}{}",
            base_currency.trim().to_ascii_lowercase(),
            quote_currency.trim().to_ascii_lowercase()
        );
        let limit_str = limit.to_string();
        let url = build_provider_url(
            "bitstamp",
            &self.bitstamp_base_url,
            &["api", "v2", "ohlc", &pair],
            &[("step", "86400"), ("limit", &limit_str)],
            true,
        )?;
        let body = get_text_with_retry(client, "bitstamp", &url, &[], 2).await?;
        Self::parse_bitstamp_ohlc_pair(base_currency, quote_currency, &body, dataset_version)
    }

    pub async fn fetch_kraken_usdc_usd(
        &self,
        client: &Client,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        let url = build_provider_url(
            "kraken",
            &self.kraken_base_url,
            &["0", "public", "OHLC"],
            &[("pair", "USDCUSD"), ("interval", "1440")],
            false,
        )?;
        let body = get_text_with_retry(client, "kraken", &url, &[], 2).await?;
        Self::parse_kraken_ohlc("USDC", &body, dataset_version)
    }
}

// ============================================================================
// 4. Frankfurter Fiat FX Provider (ECB Reference Rates, USD/CNY from 2005+)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FrankfurterFxProvider {
    pub base_url: String,
}

impl Default for FrankfurterFxProvider {
    fn default() -> Self {
        Self {
            base_url: "https://api.frankfurter.dev".to_string(),
        }
    }
}

impl FrankfurterFxProvider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "frankfurter".to_string(),
            base_url: self.base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 300,
            terms_class: TermsClass::PublicOpen,
            supports_equities: false,
            supports_crypto: false,
            supports_fx: true,
            supports_corporate_actions: false,
        }
    }

    /// Parse Frankfurter `/v1/latest` or `/v1/{date}` or `/v1/{start}..{end}` JSON directly into `Decimal`
    /// without `f64` intermediate (S-08).
    pub fn parse_fx_json(
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct SingleOrRange {
            base: String,
            date: Option<String>,
            rates: BTreeMap<String, Box<serde_json::value::RawValue>>,
        }
        let parsed: SingleOrRange =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "frankfurter".into(),
                detail: e.to_string(),
            })?;

        let observed_at = Utc::now();
        let mut out = Vec::new();
        let base = parsed.base.to_ascii_uppercase();

        if let Some(single_date_str) = parsed.date {
            let eff_date =
                NaiveDate::parse_from_str(&single_date_str, "%Y-%m-%d").map_err(|e| {
                    ProviderError::Parse {
                        provider: "frankfurter".into(),
                        detail: e.to_string(),
                    }
                })?;
            let eff_at = Utc.from_utc_datetime(&eff_date.and_hms_opt(16, 0, 0).unwrap());
            for (quote, raw_val) in parsed.rates {
                let rate = ensure_positive_fx(
                    "frankfurter",
                    parse_json_raw_decimal(raw_val.get()).map_err(|e| ProviderError::Parse {
                        provider: "frankfurter".into(),
                        detail: e.to_string(),
                    })?,
                )?;
                let q = quote.to_ascii_uppercase();
                out.push(FxRateRecord {
                    id: format!("FX:{base}:{q}:{eff_date}:frankfurter:{dataset_version}"),
                    base_currency: base.clone(),
                    quote_currency: q,
                    rate,
                    effective_date: eff_date,
                    effective_at: eff_at,
                    observed_at,
                    source: "frankfurter".to_string(),
                    quality: QualityStatus::Exact,
                    carried_from_date: None,
                    dataset_version,
                });
            }
        } else {
            for (date_str, day_raw) in parsed.rates {
                let eff_date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").map_err(|e| {
                    ProviderError::Parse {
                        provider: "frankfurter".into(),
                        detail: e.to_string(),
                    }
                })?;
                let eff_at = Utc.from_utc_datetime(&eff_date.and_hms_opt(16, 0, 0).unwrap());
                let day_map: BTreeMap<String, DirectDecimal> = serde_json::from_str(day_raw.get())
                    .map_err(|e| ProviderError::Parse {
                        provider: "frankfurter".into(),
                        detail: e.to_string(),
                    })?;
                for (quote, dir_dec) in day_map {
                    let rate = ensure_positive_fx("frankfurter", dir_dec.0)?;
                    let q = quote.to_ascii_uppercase();
                    out.push(FxRateRecord {
                        id: format!("FX:{base}:{q}:{eff_date}:frankfurter:{dataset_version}"),
                        base_currency: base.clone(),
                        quote_currency: q,
                        rate,
                        effective_date: eff_date,
                        effective_at: eff_at,
                        observed_at,
                        source: "frankfurter".to_string(),
                        quality: QualityStatus::Exact,
                        carried_from_date: None,
                        dataset_version,
                    });
                }
            }
        }
        out.sort_by_key(|r| (r.effective_date, r.quote_currency.clone()));
        Ok(out)
    }

    pub async fn fetch_range(
        &self,
        client: &Client,
        base: &str,
        symbols: &[&str],
        start: NaiveDate,
        end: NaiveDate,
        dataset_version: i64,
    ) -> Result<(Vec<FxRateRecord>, String), ProviderError> {
        let range_seg = format!("{start}..{end}");
        let syms = symbols.join(",");
        let url = build_provider_url(
            "frankfurter",
            &self.base_url,
            &["v1", &range_seg],
            &[("base", base), ("symbols", &syms)],
            false,
        )?;
        let body = get_text_with_retry(client, "frankfurter", &url, &[], 2).await?;
        let rates = Self::parse_fx_json(&body, dataset_version)?;
        Ok((rates, body))
    }
}
