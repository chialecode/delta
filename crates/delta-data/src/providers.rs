use crate::calendar::resolve_exchange_session_date;
use crate::registry::InstrumentRegistry;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::{
    parse_decimal_lossless, parse_json_raw_decimal, restore_yahoo_raw_dividend,
    restore_yahoo_raw_price, restore_yahoo_raw_volume, AdjustmentKind, BarRecord,
    CorporateActionRecord, CorporateActionType, DataQualityIssue, DirectDecimal, FxRateRecord,
    QualityStatus, TermsClass,
};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::time::Duration;
use thiserror::Error;

pub const DEFAULT_USER_AGENT: &str = "delta-data/0.1.0 (pure-rust; local-first)";

pub fn validate_provider_base_url(base: &str) -> Result<(), ProviderError> {
    let url = reqwest::Url::parse(base).map_err(|_| ProviderError::Parse {
        provider: "configuration".into(),
        detail: "invalid provider base URL".into(),
    })?;
    let loopback = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(ProviderError::Parse{provider:"configuration".into(),detail:"base URL must use HTTPS (HTTP only for loopback), without credentials, query or fragment".into()});
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error from {provider} ({status:?}): {message}")]
    Http {
        provider: String,
        status: Option<u16>,
        message: String,
        retryable: bool,
    },
    #[error("rate limited by {provider} (HTTP 429)")]
    RateLimited { provider: String },
    #[error("forbidden endpoint '{0}': api.binance.com is restricted (HTTP 451); use data-api.binance.vision or data.binance.vision")]
    ForbiddenEndpoint(String),
    #[error("unsupported product '{product}' on {provider}: {reason}")]
    UnsupportedProduct {
        provider: String,
        product: String,
        reason: String,
    },
    #[error("missing API token for provider {0} (see ACT-03)")]
    MissingCredentials(String),
    #[error("archive checksum mismatch for {file}: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("parse error in {provider}: {detail}")]
    Parse { provider: String, detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider: String,
    pub base_url: String,
    pub requires_key: bool,
    pub rate_limit_per_min: u32,
    pub terms_class: TermsClass,
    pub supports_equities: bool,
    pub supports_crypto: bool,
    pub supports_fx: bool,
    pub supports_corporate_actions: bool,
}

pub fn build_http_client(timeout_ms: u64) -> Result<Client, ProviderError> {
    Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(timeout_ms.min(5000)))
        .build()
        .map_err(|e| ProviderError::Http {
            provider: "reqwest".into(),
            status: None,
            message: e.to_string(),
            retryable: false,
        })
}

/// Execute an HTTP GET with bounded retries and exponential backoff on HTTP 429 / 5xx.
pub async fn get_text_with_retry(
    client: &Client,
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    max_retries: usize,
) -> Result<String, ProviderError> {
    let body = get_bytes_with_retry(client, provider, url, headers, max_retries).await?;
    String::from_utf8(body).map_err(|_| ProviderError::Parse {
        provider: provider.into(),
        detail: "invalid UTF-8 response".into(),
    })
}

pub async fn get_bytes_with_retry(
    client: &Client,
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    max_retries: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut attempt = 0usize;
    loop {
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(mut resp) => {
                let status = resp.status();
                let code = status.as_u16();
                if status.is_success() {
                    let mut body = Vec::new();
                    while let Some(chunk) = resp.chunk().await.map_err(|e| ProviderError::Http {
                        provider: provider.into(),
                        status: Some(code),
                        message: e.without_url().to_string(),
                        retryable: true,
                    })? {
                        if body.len() + chunk.len() > 16 * 1024 * 1024 {
                            return Err(ProviderError::Parse {
                                provider: provider.into(),
                                detail: "response exceeds 16 MiB".into(),
                            });
                        }
                        body.extend_from_slice(&chunk);
                    }
                    return Ok(body);
                }
                if code == 429 || (500..600).contains(&code) {
                    if attempt < max_retries {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(50 * (1 << attempt.min(5)))).await;
                        continue;
                    }
                    if code == 429 {
                        return Err(ProviderError::RateLimited {
                            provider: provider.to_string(),
                        });
                    }
                }
                return Err(ProviderError::Http {
                    provider: provider.to_string(),
                    status: Some(code),
                    message: format!("HTTP {code}"),
                    retryable: code == 429 || (500..600).contains(&code),
                });
            }
            Err(err) => {
                if attempt < max_retries {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(50 * (1 << attempt.min(5)))).await;
                    continue;
                }
                return Err(ProviderError::Http {
                    provider: provider.to_string(),
                    status: err.status().map(|s| s.as_u16()),
                    message: err.without_url().to_string(),
                    retryable: true,
                });
            }
        }
    }
}

// ============================================================================
// 1. OKX Public Spot REST Connector (`bar=1Dutc`, `confirm == "1"`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct OkxSpotProvider {
    pub base_url: String,
}

impl Default for OkxSpotProvider {
    fn default() -> Self {
        Self {
            base_url: "https://www.okx.com".to_string(),
        }
    }
}

impl OkxSpotProvider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "okx".to_string(),
            base_url: self.base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 600,
            terms_class: TermsClass::PublicOpen,
            supports_equities: false,
            supports_crypto: true,
            supports_fx: false,
            supports_corporate_actions: false,
        }
    }

    /// Parse OKX V5 `/api/v5/market/candles` or `/api/v5/market/history-candles` JSON payload.
    /// Only commits finalized bars where `confirm == "1"` (index 8).
    pub fn parse_candles_json(
        instrument_id: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct OkxEnvelope {
            code: String,
            #[serde(default)]
            msg: String,
            #[serde(default)]
            data: Vec<Vec<String>>,
        }
        let env: OkxEnvelope =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;
        if env.code != "0" {
            return Err(ProviderError::Parse {
                provider: "okx".into(),
                detail: format!("OKX error code {}: {}", env.code, env.msg),
            });
        }

        let mut bars = Vec::new();
        for row in env.data {
            if row.len() < 9 {
                return Err(ProviderError::Parse {
                    provider: "okx".into(),
                    detail: "candle lacks required fields or confirm flag".into(),
                });
            }
            let confirm = row[8].as_str();
            if confirm != "1" {
                // Skip unclosed current-day candle!
                continue;
            }
            let ts_ms: i64 = row[0].parse().map_err(|_| ProviderError::Parse {
                provider: "okx".into(),
                detail: format!("invalid timestamp {}", row[0]),
            })?;
            let open_at =
                Utc.timestamp_millis_opt(ts_ms)
                    .single()
                    .ok_or_else(|| ProviderError::Parse {
                        provider: "okx".into(),
                        detail: format!("out of range timestamp {ts_ms}"),
                    })?;
            let close_at = open_at + chrono::Duration::days(1);
            let session_date = resolve_exchange_session_date("CRYPTO_24_7", open_at);

            let open = parse_decimal_lossless(&row[1]).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;
            let high = parse_decimal_lossless(&row[2]).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;
            let low = parse_decimal_lossless(&row[3]).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;
            let close = parse_decimal_lossless(&row[4]).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;
            let volume = parse_decimal_lossless(&row[5]).map_err(|e| ProviderError::Parse {
                provider: "okx".into(),
                detail: e.to_string(),
            })?;

            bars.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:okx:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: "utc_24_7".to_string(),
                session_date,
                open_at,
                close_at,
                open,
                high,
                low,
                close,
                volume,
                source: "okx".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: dataset_version,
                valid_to_version: None,
                raw_payload_hash: raw_hash.clone(),
            });
        }
        bars.sort_by_key(|b| b.open_at);
        Ok(bars)
    }

    pub async fn fetch_daily_candles(
        &self,
        client: &Client,
        instrument_id: &str,
        inst_id_param: &str,
        after_cursor: Option<&str>,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, String), ProviderError> {
        let mut url = format!(
            "{}/api/v5/market/history-candles?instId={}&bar=1Dutc&limit=100",
            self.base_url.trim_end_matches('/'),
            inst_id_param
        );
        if let Some(after) = after_cursor {
            url.push_str(&format!("&after={after}"));
        }
        let body = get_text_with_retry(client, "okx", &url, &[], 2).await?;
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let bars = Self::parse_candles_json(instrument_id, &body, Some(hash), dataset_version)?;
        Ok((bars, body))
    }
}

// ============================================================================
// 2. Binance Public Vision Mirror & Archive Connector (ms / us timestamp + CHECKSUM)
// ============================================================================

#[derive(Debug, Clone)]
pub struct BinanceSpotProvider {
    pub rest_base_url: String,
    pub archive_base_url: String,
}

impl Default for BinanceSpotProvider {
    fn default() -> Self {
        Self {
            rest_base_url: "https://data-api.binance.vision".to_string(),
            archive_base_url: "https://data.binance.vision".to_string(),
        }
    }
}

impl BinanceSpotProvider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "binance".to_string(),
            base_url: self.rest_base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 1200,
            terms_class: TermsClass::PublicOpen,
            supports_equities: false,
            supports_crypto: true,
            supports_fx: false,
            supports_corporate_actions: false,
        }
    }

    pub fn validate_base_url(&self) -> Result<(), ProviderError> {
        if self.rest_base_url.contains("api.binance.com") {
            return Err(ProviderError::ForbiddenEndpoint(self.rest_base_url.clone()));
        }
        Ok(())
    }

    /// Convert a Binance timestamp (milliseconds before 2025, microseconds in/after 2025 in archives)
    /// into a UTC `DateTime<Utc>`.
    pub fn parse_binance_timestamp(raw_ts: i64) -> Result<DateTime<Utc>, ProviderError> {
        // Microsecond timestamps for 2000..2100 are >= 946_684_800_000_000 (>= 10^14).
        // Millisecond timestamps for 2000..2100 are in 946_684_800_000 .. 4_102_444_800_000 (10^11 .. 10^13).
        let millis = if raw_ts >= 100_000_000_000_000 {
            raw_ts / 1_000
        } else {
            raw_ts
        };
        Utc.timestamp_millis_opt(millis)
            .single()
            .ok_or_else(|| ProviderError::Parse {
                provider: "binance".into(),
                detail: format!("invalid binance timestamp {raw_ts}"),
            })
    }

    /// Parse Binance `/api/v3/klines` JSON array of arrays.
    pub fn parse_klines_json(
        instrument_id: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        now_utc: DateTime<Utc>,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        let rows: Vec<Vec<serde_json::Value>> =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "binance".into(),
                detail: e.to_string(),
            })?;
        let mut out = Vec::new();
        for row in rows {
            if row.len() < 7 {
                return Err(ProviderError::Parse {
                    provider: "binance".into(),
                    detail: "short kline row".into(),
                });
            }
            let open_ts = row[0].as_i64().ok_or_else(|| ProviderError::Parse {
                provider: "binance".into(),
                detail: "missing open_time".into(),
            })?;
            let close_ts = row[6].as_i64().ok_or_else(|| ProviderError::Parse {
                provider: "binance".into(),
                detail: "missing close_time".into(),
            })?;
            let open_at = Self::parse_binance_timestamp(open_ts)?;
            let close_raw = Self::parse_binance_timestamp(close_ts)?;
            // Only commit finalized daily bars whose close_time <= now_utc
            let is_final = close_raw <= now_utc;
            if !is_final {
                continue;
            }
            let close_at = open_at + chrono::Duration::days(1);
            let session_date = resolve_exchange_session_date("CRYPTO_24_7", open_at);

            let parse_val = |v: &serde_json::Value| -> Result<Decimal, ProviderError> {
                let s = v.as_str().ok_or_else(|| ProviderError::Parse {
                    provider: "binance".into(),
                    detail: "expected string decimal in kline".into(),
                })?;
                parse_decimal_lossless(s).map_err(|e| ProviderError::Parse {
                    provider: "binance".into(),
                    detail: e.to_string(),
                })
            };

            out.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:binance:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: "utc_24_7".to_string(),
                session_date,
                open_at,
                close_at,
                open: parse_val(&row[1])?,
                high: parse_val(&row[2])?,
                low: parse_val(&row[3])?,
                close: parse_val(&row[4])?,
                volume: parse_val(&row[5])?,
                source: "binance".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: dataset_version,
                valid_to_version: None,
                raw_payload_hash: raw_hash.clone(),
            });
        }
        out.sort_by_key(|b| b.open_at);
        Ok(out)
    }

    /// Verify `.CHECKSUM` (SHA-256) and parse a Binance `data.binance.vision` CSV or ZIP archive.
    /// Supports both pre-2025 millisecond timestamps and 2025+ microsecond timestamps!
    pub fn verify_and_parse_archive(
        instrument_id: &str,
        archive_bytes: &[u8],
        checksum_text: &str,
        is_zip: bool,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        if archive_bytes.len() > 16 * 1024 * 1024 {
            return Err(ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: "archive exceeds 16 MiB".into(),
            });
        }
        let expected_hash = checksum_text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut hasher = Sha256::new();
        hasher.update(archive_bytes);
        let actual_hash = hex::encode(hasher.finalize());
        if expected_hash != actual_hash {
            return Err(ProviderError::ChecksumMismatch {
                file: instrument_id.to_string(),
                expected: expected_hash,
                actual: actual_hash,
            });
        }

        let csv_text = if is_zip {
            let cursor = std::io::Cursor::new(archive_bytes);
            let mut archive = zip::ZipArchive::new(cursor).map_err(|e| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: e.to_string(),
            })?;
            if archive.len() != 1 {
                return Err(ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: "expected one CSV member".into(),
                });
            }
            let mut file = archive.by_index(0).map_err(|e| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: e.to_string(),
            })?;
            if file.size() > 16 * 1024 * 1024 || file.enclosed_name().is_none() {
                return Err(ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: "unsafe archive member".into(),
                });
            }
            let mut buf = String::new();
            file.read_to_string(&mut buf)
                .map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?;
            buf
        } else {
            String::from_utf8(archive_bytes.to_vec()).map_err(|e| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: e.to_string(),
            })?
        };

        Self::parse_archive_csv(instrument_id, &csv_text, Some(actual_hash), dataset_version)
    }

    /// Binance publishes immutable daily/monthly ZIPs with a SHA-256 sidecar.
    pub async fn fetch_monthly_archive(
        &self,
        client: &Client,
        instrument_id: &str,
        symbol: &str,
        month: NaiveDate,
        version: i64,
    ) -> Result<(Vec<BarRecord>, Vec<u8>, String), ProviderError> {
        let url = format!(
            "{}/data/spot/monthly/klines/{symbol}/1d/{symbol}-1d-{}.zip",
            self.archive_base_url.trim_end_matches('/'),
            month.format("%Y-%m")
        );
        let bytes = get_bytes_with_retry(client, "binance-archive", &url, &[], 2).await?;
        let checksum = get_text_with_retry(
            client,
            "binance-archive",
            &format!("{url}.CHECKSUM"),
            &[],
            2,
        )
        .await?;
        let bars = Self::verify_and_parse_archive(instrument_id, &bytes, &checksum, true, version)?;
        Ok((bars, bytes, checksum))
    }

    pub fn parse_archive_csv(
        instrument_id: &str,
        csv_text: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(csv_text.as_bytes());
        let mut out = Vec::new();
        for result in rdr.records() {
            let rec = result.map_err(|e| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: e.to_string(),
            })?;
            if rec.len() < 7 {
                return Err(ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: "short archive kline row".into(),
                });
            }
            let first_col = rec[0].trim();
            if first_col.eq_ignore_ascii_case("open_time") {
                continue;
            }
            let open_ts: i64 = first_col.parse().map_err(|_| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: format!("invalid archive open_time '{first_col}'"),
            })?;
            let open_at = Self::parse_binance_timestamp(open_ts)?;
            let close_at = open_at + chrono::Duration::days(1);
            let session_date = resolve_exchange_session_date("CRYPTO_24_7", open_at);

            out.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:binance:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: "utc_24_7".to_string(),
                session_date,
                open_at,
                close_at,
                open: parse_decimal_lossless(&rec[1]).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?,
                high: parse_decimal_lossless(&rec[2]).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?,
                low: parse_decimal_lossless(&rec[3]).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?,
                close: parse_decimal_lossless(&rec[4]).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?,
                volume: parse_decimal_lossless(&rec[5]).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })?,
                source: "binance".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: dataset_version,
                valid_to_version: None,
                raw_payload_hash: raw_hash.clone(),
            });
        }
        out.sort_by_key(|b| b.open_at);
        Ok(out)
    }

    pub async fn fetch_daily_klines(
        &self,
        client: &Client,
        instrument_id: &str,
        symbol_param: &str,
        start_time_ms: Option<i64>,
        now_utc: DateTime<Utc>,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, String), ProviderError> {
        self.validate_base_url()?;
        let mut url = format!(
            "{}/api/v3/klines?symbol={}&interval=1d&limit=100",
            self.rest_base_url.trim_end_matches('/'),
            symbol_param
        );
        if let Some(st) = start_time_ms {
            url.push_str(&format!("&startTime={st}"));
        }
        let body = get_text_with_retry(client, "binance", &url, &[], 2).await?;
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let bars =
            Self::parse_klines_json(instrument_id, &body, Some(hash), now_utc, dataset_version)?;
        Ok((bars, body))
    }
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
    /// Parse Coinbase Exchange `/products/USDT-USD/candles` JSON: `[[time, low, high, open, close, volume], ...]`.
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
            // Index 4 is close in Coinbase [time, low, high, open, close, volume]
            let close_rate =
                parse_json_raw_decimal(row[4].get()).map_err(|e| ProviderError::Parse {
                    provider: "coinbase".into(),
                    detail: e.to_string(),
                })?;
            out.push(FxRateRecord {
                id: format!("FX:USDT:USD:{effective_date}:coinbase:{dataset_version}"),
                base_currency: "USDT".to_string(),
                quote_currency: "USD".to_string(),
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

    /// Parse Bitstamp `/api/v2/ohlc/{usdcusd|usdtusd}/` JSON.
    pub fn parse_bitstamp_ohlc(
        base_currency: &str,
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
        let base = base_currency.to_ascii_uppercase();
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
            let rate = parse_decimal_lossless(&bar.close).map_err(|e| ProviderError::Parse {
                provider: "bitstamp".into(),
                detail: e.to_string(),
            })?;
            out.push(FxRateRecord {
                id: format!("FX:{base}:USD:{effective_date}:bitstamp:{dataset_version}"),
                base_currency: base.clone(),
                quote_currency: "USD".to_string(),
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
                detail: env.error.join("; "),
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
                                let rate = parse_decimal_lossless(close_str).map_err(|e| {
                                    ProviderError::Parse {
                                        provider: "kraken".into(),
                                        detail: e.to_string(),
                                    }
                                })?;
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
        let url = format!(
            "{}/products/USDT-USD/candles?granularity=86400",
            self.coinbase_base_url.trim_end_matches('/')
        );
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
            base_currency.to_ascii_lowercase(),
            quote_currency.to_ascii_lowercase()
        );
        let url = format!(
            "{}/api/v2/ohlc/{pair}/?step=86400&limit={limit}",
            self.bitstamp_base_url.trim_end_matches('/')
        );
        let body = get_text_with_retry(client, "bitstamp", &url, &[], 2).await?;
        Self::parse_bitstamp_ohlc(base_currency, &body, dataset_version)
    }

    pub async fn fetch_kraken_usdc_usd(
        &self,
        client: &Client,
        dataset_version: i64,
    ) -> Result<Vec<FxRateRecord>, ProviderError> {
        let url = format!(
            "{}/0/public/OHLC?pair=USDCUSD&interval=1440",
            self.kraken_base_url.trim_end_matches('/')
        );
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
                let rate =
                    parse_json_raw_decimal(raw_val.get()).map_err(|e| ProviderError::Parse {
                        provider: "frankfurter".into(),
                        detail: e.to_string(),
                    })?;
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
            // Range response: rates is { "2020-01-02": { "HKD": 7.78, "CNY": 6.96 } }
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
                    let q = quote.to_ascii_uppercase();
                    out.push(FxRateRecord {
                        id: format!("FX:{base}:{q}:{eff_date}:frankfurter:{dataset_version}"),
                        base_currency: base.clone(),
                        quote_currency: q,
                        rate: dir_dec.0,
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
        let syms = symbols.join(",");
        let url = format!(
            "{}/v1/{start}..{end}?base={base}&symbols={syms}",
            self.base_url.trim_end_matches('/')
        );
        let body = get_text_with_retry(client, "frankfurter", &url, &[], 2).await?;
        let rates = Self::parse_fx_json(&body, dataset_version)?;
        Ok((rates, body))
    }
}

// ============================================================================
// 5. Yahoo Finance Chart v8 Connector (S-08 Raw Price Restoration + Splits/Dividends)
// ============================================================================

#[derive(Debug, Clone)]
pub struct YahooChartV8Provider {
    pub base_url: String,
}

impl Default for YahooChartV8Provider {
    fn default() -> Self {
        Self {
            base_url: "https://query1.finance.yahoo.com".to_string(),
        }
    }
}

impl YahooChartV8Provider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "yahoo".to_string(),
            base_url: self.base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 120,
            terms_class: TermsClass::UnofficialPersonal,
            supports_equities: true,
            supports_crypto: false,
            supports_fx: false,
            supports_corporate_actions: true,
        }
    }

    /// Parse Yahoo Chart v8 JSON directly into `Decimal` (no `f64` intermediate) and restore
    /// canonical `raw` unadjusted prices & volumes by multiplying/dividing by subsequent split
    /// factors (S-08).
    pub fn parse_chart_v8_json(
        instrument_id: &str,
        venue: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, Vec<CorporateActionRecord>), ProviderError> {
        #[derive(Deserialize)]
        struct YahooSplit {
            date: i64,
            numerator: DirectDecimal,
            denominator: DirectDecimal,
        }
        #[derive(Deserialize)]
        struct YahooDividend {
            date: i64,
            amount: DirectDecimal,
        }
        #[derive(Deserialize, Default)]
        struct YahooEvents {
            #[serde(default)]
            splits: BTreeMap<String, YahooSplit>,
            #[serde(default)]
            dividends: BTreeMap<String, YahooDividend>,
        }
        #[derive(Deserialize)]
        struct YahooQuote {
            #[serde(default)]
            open: Vec<Option<DirectDecimal>>,
            #[serde(default)]
            high: Vec<Option<DirectDecimal>>,
            #[serde(default)]
            low: Vec<Option<DirectDecimal>>,
            #[serde(default)]
            close: Vec<Option<DirectDecimal>>,
            #[serde(default)]
            volume: Vec<Option<DirectDecimal>>,
        }
        #[derive(Deserialize)]
        struct YahooIndicators {
            #[serde(default)]
            quote: Vec<YahooQuote>,
        }
        #[derive(Deserialize)]
        struct YahooResult {
            #[serde(default)]
            timestamp: Vec<i64>,
            #[serde(default)]
            events: Option<YahooEvents>,
            indicators: YahooIndicators,
        }
        #[derive(Deserialize)]
        struct YahooChart {
            result: Option<Vec<YahooResult>>,
        }
        #[derive(Deserialize)]
        struct YahooEnvelope {
            chart: YahooChart,
        }

        let env: YahooEnvelope =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "yahoo".into(),
                detail: e.to_string(),
            })?;
        let res =
            env.chart
                .result
                .and_then(|mut v| v.pop())
                .ok_or_else(|| ProviderError::Parse {
                    provider: "yahoo".into(),
                    detail: "empty chart.result".into(),
                })?;

        let mut actions = Vec::new();
        let mut splits_list: Vec<(NaiveDate, Decimal)> = Vec::new();

        if let Some(ev) = &res.events {
            for sp in ev.splits.values() {
                if sp.denominator.0 <= Decimal::ZERO || sp.numerator.0 <= Decimal::ZERO {
                    return Err(ProviderError::Parse {
                        provider: "yahoo".into(),
                        detail: "invalid split ratio".into(),
                    });
                }
                let dt = provider_timestamp(sp.date)?;
                let ex_date = resolve_exchange_session_date(venue, dt);
                let ratio = (sp.numerator.0 / sp.denominator.0).normalize();
                splits_list.push((ex_date, ratio));
                actions.push(CorporateActionRecord {
                    id: format!("CA:SPLIT:{instrument_id}:{ex_date}:yahoo:{dataset_version}"),
                    instrument_id: instrument_id.to_string(),
                    action_type: CorporateActionType::Split,
                    ex_date,
                    pay_date: Some(ex_date),
                    ratio_or_amount: ratio,
                    currency: None,
                    announced_at: Some(dt),
                    source: "yahoo".to_string(),
                    factor_version: dataset_version,
                    raw_payload_hash: raw_hash.clone(),
                });
            }
            splits_list.sort_by_key(|(d, _)| *d);

            for div in ev.dividends.values() {
                let dt = provider_timestamp(div.date)?;
                let ex_date = resolve_exchange_session_date(venue, dt);
                // S-08: Yahoo dividend amounts are also split-adjusted by subsequent splits!
                let mut factor_after = Decimal::ONE;
                for (sp_date, sp_ratio) in &splits_list {
                    if *sp_date > ex_date {
                        factor_after *= *sp_ratio;
                    }
                }
                let raw_div_amt = restore_yahoo_raw_dividend(div.amount.0, factor_after);
                actions.push(CorporateActionRecord {
                    id: format!("CA:DIV:{instrument_id}:{ex_date}:yahoo:{dataset_version}"),
                    instrument_id: instrument_id.to_string(),
                    action_type: CorporateActionType::CashDividend,
                    ex_date,
                    pay_date: Some(ex_date),
                    ratio_or_amount: raw_div_amt,
                    currency: Some("USD".to_string()),
                    announced_at: Some(dt),
                    source: "yahoo".to_string(),
                    factor_version: dataset_version,
                    raw_payload_hash: raw_hash.clone(),
                });
            }
        }
        actions.sort_by_key(|a| (a.ex_date, a.id.clone()));

        let quote = res
            .indicators
            .quote
            .first()
            .ok_or_else(|| ProviderError::Parse {
                provider: "yahoo".into(),
                detail: "missing indicators.quote[0]".into(),
            })?;

        let mut bars = Vec::new();
        for (idx, ts) in res.timestamp.iter().enumerate() {
            let (Some(Some(o)), Some(Some(h)), Some(Some(l)), Some(Some(c)), Some(Some(v))) = (
                quote.open.get(idx),
                quote.high.get(idx),
                quote.low.get(idx),
                quote.close.get(idx),
                quote.volume.get(idx),
            ) else {
                continue;
            };
            let dt = provider_timestamp(*ts)?;
            let session_date = resolve_exchange_session_date(venue, dt);
            let cal_sess = crate::calendar::query_calendar_session(venue, session_date);
            let open_at = cal_sess.open_at.unwrap_or(dt);
            let close_at = cal_sess
                .close_at
                .unwrap_or_else(|| dt + chrono::Duration::hours(6));

            // Calculate cumulative split factor after session_date (S-08)
            let mut cumulative_split_factor = Decimal::ONE;
            for (sp_date, sp_ratio) in &splits_list {
                if *sp_date > session_date {
                    cumulative_split_factor *= *sp_ratio;
                }
            }

            let raw_open = restore_yahoo_raw_price(o.0, cumulative_split_factor);
            let raw_high = restore_yahoo_raw_price(h.0, cumulative_split_factor);
            let raw_low = restore_yahoo_raw_price(l.0, cumulative_split_factor);
            let raw_close = restore_yahoo_raw_price(c.0, cumulative_split_factor);
            let raw_vol = restore_yahoo_raw_volume(v.0, cumulative_split_factor);

            bars.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:yahoo:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: cal_sess.kind.as_str().to_string(),
                session_date,
                open_at,
                close_at,
                open: raw_open,
                high: raw_high,
                low: raw_low,
                close: raw_close,
                volume: raw_vol,
                source: "yahoo".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: dataset_version,
                valid_to_version: None,
                raw_payload_hash: raw_hash.clone(),
            });
        }
        bars.sort_by_key(|b| b.open_at);
        Ok((bars, actions))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_chart_v8(
        &self,
        client: &Client,
        instrument_id: &str,
        venue: &str,
        symbol: &str,
        period1: i64,
        period2: i64,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, Vec<CorporateActionRecord>, String), ProviderError> {
        let url = format!(
            "{}/v8/finance/chart/{symbol}?interval=1d&events=div,split&period1={period1}&period2={period2}",
            self.base_url.trim_end_matches('/')
        );
        let body = get_text_with_retry(client, "yahoo", &url, &[], 2).await?;
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let (bars, actions) =
            Self::parse_chart_v8_json(instrument_id, venue, &body, Some(hash), dataset_version)?;
        Ok((bars, actions, body))
    }
}

// ============================================================================
// 6. EastMoney `push2his` US Equity Connector (`fqt=0` Raw Price Verification)
// ============================================================================

#[derive(Debug, Clone)]
pub struct EastMoneyUsEquityProvider {
    pub base_url: String,
}

impl Default for EastMoneyUsEquityProvider {
    fn default() -> Self {
        Self {
            base_url: "https://push2his.eastmoney.com".to_string(),
        }
    }
}

impl EastMoneyUsEquityProvider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "eastmoney".to_string(),
            base_url: self.base_url.clone(),
            requires_key: false,
            rate_limit_per_min: 120,
            terms_class: TermsClass::UnofficialPersonal,
            supports_equities: true,
            supports_crypto: false,
            supports_fx: false,
            supports_corporate_actions: false,
        }
    }

    /// Parse EastMoney `push2his` kline JSON (`klines: ["YYYY-MM-DD,open,close,high,low,volume", ...]`).
    /// Note EastMoney column order: `f51=date, f52=open, f53=close, f54=high, f55=low, f56=volume`!
    pub fn parse_kline_json(
        instrument_id: &str,
        venue: &str,
        adjustment: AdjustmentKind,
        raw_json: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct EmData {
            #[serde(default)]
            klines: Vec<String>,
        }
        #[derive(Deserialize)]
        struct EmEnv {
            data: Option<EmData>,
        }
        let env: EmEnv = serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
            provider: "eastmoney".into(),
            detail: e.to_string(),
        })?;
        let mut bars = Vec::new();
        if let Some(data) = env.data {
            for line in data.klines {
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() < 6 {
                    return Err(ProviderError::Parse {
                        provider: "eastmoney".into(),
                        detail: "short kline row".into(),
                    });
                }
                let session_date =
                    NaiveDate::parse_from_str(parts[0].trim(), "%Y-%m-%d").map_err(|e| {
                        ProviderError::Parse {
                            provider: "eastmoney".into(),
                            detail: e.to_string(),
                        }
                    })?;
                let open = parse_decimal_lossless(parts[1]).map_err(|e| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: e.to_string(),
                })?;
                let close = parse_decimal_lossless(parts[2]).map_err(|e| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: e.to_string(),
                })?;
                let high = parse_decimal_lossless(parts[3]).map_err(|e| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: e.to_string(),
                })?;
                let low = parse_decimal_lossless(parts[4]).map_err(|e| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: e.to_string(),
                })?;
                let volume =
                    parse_decimal_lossless(parts[5]).map_err(|e| ProviderError::Parse {
                        provider: "eastmoney".into(),
                        detail: e.to_string(),
                    })?;

                let cal_sess = crate::calendar::query_calendar_session(venue, session_date);
                let open_at = cal_sess.open_at.ok_or_else(|| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: "calendar_unknown or non-trading session".into(),
                })?;
                let close_at = cal_sess.close_at.ok_or_else(|| ProviderError::Parse {
                    provider: "eastmoney".into(),
                    detail: "calendar_unknown or non-trading session".into(),
                })?;

                bars.push(BarRecord {
                    id: format!(
                        "BAR:{instrument_id}:1d:{}:eastmoney:{session_date}:{dataset_version}",
                        adjustment.as_str()
                    ),
                    instrument_id: instrument_id.to_string(),
                    timeframe: "1d".to_string(),
                    session: cal_sess.kind.as_str().to_string(),
                    session_date,
                    open_at,
                    close_at,
                    open,
                    high,
                    low,
                    close,
                    volume,
                    source: "eastmoney".to_string(),
                    adjustment,
                    is_final: true,
                    valid_from_version: dataset_version,
                    valid_to_version: None,
                    raw_payload_hash: raw_hash.clone(),
                });
            }
        }
        bars.sort_by_key(|b| b.open_at);
        Ok(bars)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_kline_raw(
        &self,
        client: &Client,
        instrument_id: &str,
        venue: &str,
        secid: &str,
        beg_yyyymmdd: &str,
        end_yyyymmdd: &str,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, String), ProviderError> {
        let url = format!(
            "{}/api/qt/stock/kline/get?secid={secid}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57,f58,f59,f60,f61&klt=101&fqt=0&beg={beg_yyyymmdd}&end={end_yyyymmdd}",
            self.base_url.trim_end_matches('/')
        );
        let body = get_text_with_retry(client, "eastmoney", &url, &[], 2).await?;
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let bars = Self::parse_kline_json(
            instrument_id,
            venue,
            AdjustmentKind::Raw,
            &body,
            Some(hash),
            dataset_version,
        )?;
        Ok((bars, body))
    }
}

// ============================================================================
// 7. Tiingo Official REST Connector (Token-configured primary + fixture support)
// ============================================================================

#[derive(Debug, Clone)]
pub struct TiingoEquityProvider {
    pub base_url: String,
}

impl Default for TiingoEquityProvider {
    fn default() -> Self {
        Self {
            base_url: "https://api.tiingo.com".to_string(),
        }
    }
}

impl TiingoEquityProvider {
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_daily(
        &self,
        client: &Client,
        token: &str,
        instrument_id: &str,
        venue: &str,
        symbol: &str,
        start: NaiveDate,
        end: NaiveDate,
        version: i64,
    ) -> Result<(Vec<BarRecord>, Vec<CorporateActionRecord>, String), ProviderError> {
        let url = format!(
            "{}/tiingo/daily/{symbol}/prices?startDate={start}&endDate={end}",
            self.base_url.trim_end_matches('/')
        );
        let auth = format!("Token {token}");
        let body =
            get_text_with_retry(client, "tiingo", &url, &[("Authorization", &auth)], 2).await?;
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        let (bars, actions) =
            Self::parse_daily_json(instrument_id, venue, &body, Some(hash), version)?;
        Ok((bars, actions, body))
    }
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "tiingo".to_string(),
            base_url: self.base_url.clone(),
            requires_key: true,
            rate_limit_per_min: 50,
            terms_class: TermsClass::OfficialFreePersonal,
            supports_equities: true,
            supports_crypto: false,
            supports_fx: false,
            supports_corporate_actions: true,
        }
    }

    pub fn parse_daily_json(
        instrument_id: &str,
        venue: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<(Vec<BarRecord>, Vec<CorporateActionRecord>), ProviderError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct TiingoRow {
            date: String,
            open: DirectDecimal,
            high: DirectDecimal,
            low: DirectDecimal,
            close: DirectDecimal,
            volume: DirectDecimal,
            div_cash: DirectDecimal,
            split_factor: DirectDecimal,
        }
        let rows: Vec<TiingoRow> =
            serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
                provider: "tiingo".into(),
                detail: e.to_string(),
            })?;
        let mut bars = Vec::new();
        let mut actions = Vec::new();

        for r in rows {
            let date_part = r.date.get(0..10).unwrap_or(&r.date);
            let session_date = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").map_err(|e| {
                ProviderError::Parse {
                    provider: "tiingo".into(),
                    detail: e.to_string(),
                }
            })?;
            let cal_sess = crate::calendar::query_calendar_session(venue, session_date);
            let open_at = cal_sess.open_at.ok_or_else(|| ProviderError::Parse {
                provider: "tiingo".into(),
                detail: "calendar_unknown or non-trading session".into(),
            })?;
            let close_at = cal_sess.close_at.ok_or_else(|| ProviderError::Parse {
                provider: "tiingo".into(),
                detail: "calendar_unknown or non-trading session".into(),
            })?;

            bars.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:tiingo:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: cal_sess.kind.as_str().to_string(),
                session_date,
                open_at,
                close_at,
                open: r.open.0,
                high: r.high.0,
                low: r.low.0,
                close: r.close.0,
                volume: r.volume.0,
                source: "tiingo".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: dataset_version,
                valid_to_version: None,
                raw_payload_hash: raw_hash.clone(),
            });

            if r.split_factor.0 != Decimal::ONE && !r.split_factor.0.is_zero() {
                actions.push(CorporateActionRecord {
                    id: format!("CA:SPLIT:{instrument_id}:{session_date}:tiingo:{dataset_version}"),
                    instrument_id: instrument_id.to_string(),
                    action_type: CorporateActionType::Split,
                    ex_date: session_date,
                    pay_date: Some(session_date),
                    ratio_or_amount: r.split_factor.0,
                    currency: None,
                    announced_at: Some(open_at),
                    source: "tiingo".to_string(),
                    factor_version: dataset_version,
                    raw_payload_hash: raw_hash.clone(),
                });
            }
            if !r.div_cash.0.is_zero() {
                actions.push(CorporateActionRecord {
                    id: format!("CA:DIV:{instrument_id}:{session_date}:tiingo:{dataset_version}"),
                    instrument_id: instrument_id.to_string(),
                    action_type: CorporateActionType::CashDividend,
                    ex_date: session_date,
                    pay_date: Some(session_date),
                    ratio_or_amount: r.div_cash.0,
                    currency: Some("USD".to_string()),
                    announced_at: Some(open_at),
                    source: "tiingo".to_string(),
                    factor_version: dataset_version,
                    raw_payload_hash: raw_hash.clone(),
                });
            }
        }
        Ok((bars, actions))
    }
}

// ============================================================================
// 8. Alpaca Market Data REST Connector (M1-EXT: `/v2/stocks/bars` + `/v1/corporate-actions`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct AlpacaEquityProvider {
    pub base_url: String,
}

impl Default for AlpacaEquityProvider {
    fn default() -> Self {
        Self {
            base_url: "https://data.alpaca.markets".to_string(),
        }
    }
}

impl AlpacaEquityProvider {
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider: "alpaca".to_string(),
            base_url: self.base_url.clone(),
            requires_key: true,
            rate_limit_per_min: 200,
            terms_class: TermsClass::OfficialFreePersonal,
            supports_equities: true,
            supports_crypto: false,
            supports_fx: false,
            supports_corporate_actions: true,
        }
    }

    /// Parse `/v1/corporate-actions` response (`corporate_actions.forward_splits` & `cash_dividends`).
    pub fn parse_corporate_actions_v1(
        instrument_id: &str,
        raw_json: &str,
        dataset_version: i64,
    ) -> Result<Vec<CorporateActionRecord>, ProviderError> {
        #[derive(Deserialize)]
        struct AlpacaSplit {
            ex_date: String,
            new_rate: DirectDecimal,
            old_rate: DirectDecimal,
        }
        #[derive(Deserialize)]
        struct AlpacaDividend {
            ex_date: String,
            payable_date: Option<String>,
            rate: DirectDecimal,
        }
        #[derive(Deserialize, Default)]
        struct AlpacaCaBody {
            #[serde(default)]
            forward_splits: Vec<AlpacaSplit>,
            #[serde(default)]
            cash_dividends: Vec<AlpacaDividend>,
        }
        #[derive(Deserialize)]
        struct AlpacaEnv {
            corporate_actions: AlpacaCaBody,
        }
        let env: AlpacaEnv = serde_json::from_str(raw_json).map_err(|e| ProviderError::Parse {
            provider: "alpaca".into(),
            detail: e.to_string(),
        })?;
        let mut out = Vec::new();
        for sp in env.corporate_actions.forward_splits {
            let ex_date = NaiveDate::parse_from_str(&sp.ex_date, "%Y-%m-%d").map_err(|e| {
                ProviderError::Parse {
                    provider: "alpaca".into(),
                    detail: e.to_string(),
                }
            })?;
            let ratio = (sp.new_rate.0 / sp.old_rate.0).normalize();
            out.push(CorporateActionRecord {
                id: format!("CA:SPLIT:{instrument_id}:{ex_date}:alpaca:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                action_type: CorporateActionType::Split,
                ex_date,
                pay_date: Some(ex_date),
                ratio_or_amount: ratio,
                currency: None,
                announced_at: None,
                source: "alpaca".to_string(),
                factor_version: dataset_version,
                raw_payload_hash: None,
            });
        }
        for div in env.corporate_actions.cash_dividends {
            let ex_date = NaiveDate::parse_from_str(&div.ex_date, "%Y-%m-%d").map_err(|e| {
                ProviderError::Parse {
                    provider: "alpaca".into(),
                    detail: e.to_string(),
                }
            })?;
            let pay_date = div
                .payable_date
                .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
                .or(Some(ex_date));
            out.push(CorporateActionRecord {
                id: format!("CA:DIV:{instrument_id}:{ex_date}:alpaca:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                action_type: CorporateActionType::CashDividend,
                ex_date,
                pay_date,
                ratio_or_amount: div.rate.0,
                currency: Some("USD".to_string()),
                announced_at: None,
                source: "alpaca".to_string(),
                factor_version: dataset_version,
                raw_payload_hash: None,
            });
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_corporate_actions_v1(
        &self,
        client: &Client,
        api_key: &str,
        api_secret: &str,
        instrument_id: &str,
        symbol: &str,
        start: NaiveDate,
        end: NaiveDate,
        dataset_version: i64,
    ) -> Result<(Vec<CorporateActionRecord>, String), ProviderError> {
        let url = format!(
            "{}/v1/corporate-actions?symbols={symbol}&types=forward_split,reverse_split,cash_dividend&start={start}&end={end}",
            self.base_url.trim_end_matches('/')
        );
        let headers = [
            ("APCA-API-KEY-ID", api_key),
            ("APCA-API-SECRET-KEY", api_secret),
        ];
        let body = get_text_with_retry(client, "alpaca", &url, &headers, 2).await?;
        let actions = Self::parse_corporate_actions_v1(instrument_id, &body, dataset_version)?;
        Ok((actions, body))
    }
}

// ============================================================================
// 9. Cross-Validation Engine (`price_mismatch` > 0.5%, `missing_corporate_action`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossValidationOutcome {
    pub quality: QualityStatus,
    pub issues: Vec<DataQualityIssue>,
}

/// Cross-validate primary vs secondary raw bars and check for unrecorded split jumps (Section 3.1 / POC-D04):
/// 1. Same trading day raw close relative difference > 0.5% -> `price_mismatch`.
/// 2. Adjacent trading day raw close ratio within ±5% of common split/reverse-split ratios
///    (2, 3, 4, 5, 10, 20) without a corresponding `CorporateActionRecord` (`Split`) -> `missing_corporate_action`.
pub fn cross_validate_bars_and_actions(
    instrument_id: &str,
    primary_bars: &[BarRecord],
    secondary_bars: &[BarRecord],
    actions: &[CorporateActionRecord],
    dataset_version: i64,
) -> CrossValidationOutcome {
    let mut issues = Vec::new();
    let sec_by_date: HashMap<NaiveDate, &BarRecord> =
        secondary_bars.iter().map(|b| (b.session_date, b)).collect();
    let split_dates: HashMap<NaiveDate, Decimal> = actions
        .iter()
        .filter(|a| a.action_type == CorporateActionType::Split)
        .map(|a| (a.ex_date, a.ratio_or_amount))
        .collect();

    let threshold = Decimal::new(5, 3); // 0.005 == 0.5%

    for p in primary_bars {
        if let Some(s) = sec_by_date.get(&p.session_date) {
            let max_c = p.close.max(s.close);
            if max_c > Decimal::ZERO {
                let diff = (p.close - s.close).abs();
                let rel_diff = diff / max_c;
                if rel_diff > threshold {
                    issues.push(DataQualityIssue {
                        id: format!(
                            "DQI:MISMATCH:{}:{}:{dataset_version}",
                            instrument_id, p.session_date
                        ),
                        dataset_version,
                        instrument_id: instrument_id.to_string(),
                        session_date: p.session_date,
                        kind: "price_mismatch".to_string(),
                        sources: format!("{},{}", p.source, s.source),
                        detail: format!(
                            "close mismatch > 0.5% on {}: {}={} vs {}={} (rel_diff={:.4})",
                            p.session_date, p.source, p.close, s.source, s.close, rel_diff
                        ),
                    });
                }
            }
        }
    }

    // Check adjacent trading days for split-like jumps missing a CorporateActionRecord
    let common_ratios = [
        Decimal::from(2),
        Decimal::from(3),
        Decimal::from(4),
        Decimal::from(5),
        Decimal::from(10),
        Decimal::from(20),
    ];
    let tol = Decimal::new(5, 2); // 0.05 == 5%

    let mut sorted_primary = primary_bars.to_vec();
    sorted_primary.sort_by_key(|b| b.session_date);
    for win in sorted_primary.windows(2) {
        let prev = &win[0];
        let curr = &win[1];
        if prev.close <= Decimal::ZERO || curr.close <= Decimal::ZERO {
            continue;
        }
        let ratio = if prev.close >= curr.close {
            prev.close / curr.close
        } else {
            curr.close / prev.close
        };
        for cand in &common_ratios {
            let rel_err = ((ratio - *cand).abs()) / *cand;
            if rel_err <= tol && !split_dates.contains_key(&curr.session_date) {
                issues.push(DataQualityIssue {
                    id: format!(
                        "DQI:MISSING_CA:{}:{}:{dataset_version}",
                        instrument_id, curr.session_date
                    ),
                    dataset_version,
                    instrument_id: instrument_id.to_string(),
                    session_date: curr.session_date,
                    kind: "missing_corporate_action".to_string(),
                    sources: curr.source.clone(),
                    detail: format!(
                        "unexplained ~{cand}:1 price jump from {} ({}) to {} ({}) without corporate_action record",
                        prev.close, prev.session_date, curr.close, curr.session_date
                    ),
                });
                break;
            }
        }
    }

    let quality = if issues.is_empty() {
        QualityStatus::Exact
    } else {
        QualityStatus::Warning
    };
    CrossValidationOutcome { quality, issues }
}

// ============================================================================
// 10. Read-Only `AccountProvider` Trait & Fixture Implementations (AC-17)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBalanceSnapshotDto {
    pub asset_id: String,
    pub quantity: Decimal,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSyncPageDto {
    pub provider: String,
    pub account_id: String,
    pub next_cursor: Option<String>,
    pub balances: Vec<AccountBalanceSnapshotDto>,
    #[serde(default)]
    pub events: Vec<delta_core::LedgerEventInput>,
}

pub trait AccountProvider {
    fn provider_name(&self) -> &str;
    fn fetch_account_page(
        &self,
        account_id: &str,
        cursor: Option<&str>,
    ) -> Result<AccountSyncPageDto, ProviderError>;
}

pub struct FixtureAccountProvider {
    pub provider: String,
    pub pages: Vec<AccountSyncPageDto>,
}

impl FixtureAccountProvider {
    /// Normalize synthetic recordings of the native data arrays from balance,
    /// fill, deposit and withdrawal endpoints. No keys or network calls are used.
    pub fn from_recorded_response(
        provider: &str,
        account_id: &str,
        body: &str,
    ) -> Result<Self, ProviderError> {
        let error = |detail: String| ProviderError::Parse {
            provider: provider.into(),
            detail,
        };
        if !matches!(provider, "okx" | "binance") {
            return Err(error("unsupported recorded account provider".into()));
        }
        #[derive(Deserialize)]
        struct Recording {
            as_of: DateTime<Utc>,
            balances: Vec<serde_json::Value>,
            fills: Vec<serde_json::Value>,
            deposits: Vec<serde_json::Value>,
            withdrawals: Vec<serde_json::Value>,
        }
        let recording: Recording = serde_json::from_str(body).map_err(|e| error(e.to_string()))?;
        let string = |row: &serde_json::Value, key: &str| -> Result<String, ProviderError> {
            row.get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| error(format!("missing string field {key}")))
        };
        let decimal = |row: &serde_json::Value, key: &str| -> Result<Decimal, ProviderError> {
            parse_decimal_lossless(&string(row, key)?).map_err(|e| error(e.to_string()))
        };
        let registry = InstrumentRegistry::new_seeded();
        let mut balances = Vec::new();
        for row in recording.balances {
            let (asset, quantity) = if provider == "okx" {
                (string(&row, "ccy")?, decimal(&row, "cashBal")?)
            } else {
                (
                    string(&row, "asset")?,
                    decimal(&row, "free")? + decimal(&row, "locked")?,
                )
            };
            if quantity < Decimal::ZERO {
                return Err(error("negative balance rejected in spot recording".into()));
            }
            balances.push(AccountBalanceSnapshotDto {
                asset_id: asset,
                quantity,
                as_of: recording.as_of,
            });
        }
        let mut csv = csv::Writer::from_writer(Vec::new());
        csv.write_record([
            "account",
            "event_type",
            "occurred_at",
            "timezone",
            "symbol",
            "side",
            "quantity",
            "price",
            "quote_currency",
            "fee_amount",
            "fee_currency",
            "source_id",
            "cost_known",
            "counterparty_account",
        ])
        .map_err(|e| error(e.to_string()))?;
        for row in recording.fills {
            let raw_symbol = string(
                &row,
                if provider == "okx" {
                    "instId"
                } else {
                    "symbol"
                },
            )?;
            let instrument = registry
                .resolve_symbol(Some(provider), &raw_symbol)
                .ok_or_else(|| error("unknown fill market in recording".into()))?;
            let market = &registry.instruments[&instrument];
            let side = if provider == "okx" {
                string(&row, "side")?
            } else {
                if row
                    .get("isBuyer")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| error("missing isBuyer".into()))?
                {
                    "buy"
                } else {
                    "sell"
                }
                .into()
            };
            if !matches!(side.as_str(), "buy" | "sell") {
                return Err(error("invalid fill side".into()));
            }
            let time = if provider == "okx" {
                string(&row, "ts")?
                    .parse::<i64>()
                    .map_err(|e| error(e.to_string()))?
            } else {
                row.get("time")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| error("missing fill time".into()))?
            };
            let at = Utc
                .timestamp_millis_opt(time)
                .single()
                .ok_or_else(|| error("invalid fill time".into()))?;
            if at > recording.as_of {
                return Err(error("fill occurs after recording cutoff".into()));
            }
            let raw_fee = decimal(
                &row,
                if provider == "okx" {
                    "fee"
                } else {
                    "commission"
                },
            )?;
            if (provider == "okx" && raw_fee > Decimal::ZERO)
                || (provider == "binance" && raw_fee < Decimal::ZERO)
            {
                return Err(error("rebate requires explicit income recording".into()));
            }
            let fee = if provider == "okx" { -raw_fee } else { raw_fee };
            let id = row
                .get(if provider == "okx" { "tradeId" } else { "id" })
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default();
            csv.write_record([
                account_id,
                side.as_str(),
                &at.to_rfc3339(),
                "UTC",
                &instrument,
                side.as_str(),
                &string(&row, if provider == "okx" { "fillSz" } else { "qty" })?,
                &string(&row, if provider == "okx" { "fillPx" } else { "price" })?,
                &market.quote_asset_id,
                &fee.to_string(),
                &string(
                    &row,
                    if provider == "okx" {
                        "feeCcy"
                    } else {
                        "commissionAsset"
                    },
                )?,
                &id,
                "true",
                "",
            ])
            .map_err(|e| error(e.to_string()))?;
        }
        for (outgoing, rows) in [(false, recording.deposits), (true, recording.withdrawals)] {
            for row in rows {
                let completed = if provider == "okx" {
                    row.get("state").and_then(|v| v.as_str()) == Some("2")
                } else {
                    row.get("status").and_then(|v| v.as_u64()) == Some(if outgoing { 6 } else { 1 })
                };
                if !completed {
                    continue;
                }
                let currency = string(&row, if provider == "okx" { "ccy" } else { "coin" })?;
                let quantity = decimal(&row, if provider == "okx" { "amt" } else { "amount" })?;
                let timestamp = string(
                    &row,
                    if provider == "okx" {
                        "ts"
                    } else {
                        if outgoing {
                            "applyTime"
                        } else {
                            "insertTime"
                        }
                    },
                )
                .or_else(|_| {
                    row.get("insertTime")
                        .and_then(|v| v.as_i64())
                        .map(|t| t.to_string())
                        .ok_or_else(|| error("missing funding timestamp".into()))
                })?;
                let at = timestamp
                    .parse::<i64>()
                    .ok()
                    .and_then(|t| Utc.timestamp_millis_opt(t).single())
                    .or_else(|| {
                        DateTime::parse_from_rfc3339(&timestamp)
                            .ok()
                            .map(|t| t.with_timezone(&Utc))
                    })
                    .or_else(|| {
                        if provider == "binance" {
                            chrono::NaiveDateTime::parse_from_str(&timestamp, "%Y-%m-%d %H:%M:%S")
                                .ok()
                                .map(|t| t.and_utc())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| error("invalid recorded funding timestamp".into()))?;
                if at > recording.as_of {
                    return Err(error("funding occurs after recording cutoff".into()));
                }
                let crypto = !delta_core::is_fiat_currency(&currency)
                    && !matches!(currency.as_str(), "USDT" | "USDC");
                let instrument = if crypto {
                    registry
                        .instruments
                        .values()
                        .find(|i| {
                            i.venue.eq_ignore_ascii_case(provider)
                                && i.base_asset_id == currency
                                && i.quote_asset_id == "USDT"
                        })
                        .map(|i| i.id.clone())
                        .ok_or_else(|| error("unknown funding asset market".into()))?
                } else {
                    currency.clone()
                };
                let event = match (outgoing, crypto) {
                    (true, true) => "position_transfer_out",
                    (false, true) => "opening_balance",
                    (true, false) => "withdrawal",
                    (false, false) => "deposit",
                };
                let quote = if crypto { "USD" } else { &currency };
                let id = row
                    .get(if outgoing {
                        if provider == "okx" {
                            "wdId"
                        } else {
                            "id"
                        }
                    } else {
                        "txId"
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                csv.write_record([
                    account_id,
                    event,
                    &at.to_rfc3339(),
                    "UTC",
                    &instrument,
                    "",
                    &quantity.to_string(),
                    "",
                    quote,
                    "0",
                    quote,
                    id,
                    if crypto && !outgoing { "false" } else { "true" },
                    if outgoing && crypto {
                        "external:recorded-wallet"
                    } else {
                        ""
                    },
                ])
                .map_err(|e| error(e.to_string()))?;
                if outgoing {
                    let fee_key = if provider == "okx" {
                        "fee"
                    } else {
                        "transactionFee"
                    };
                    if let Some(fee) = row.get(fee_key).and_then(|v| v.as_str()) {
                        let value =
                            parse_decimal_lossless(fee).map_err(|e| error(e.to_string()))?;
                        if value < Decimal::ZERO {
                            return Err(error("negative withdrawal fee".into()));
                        }
                        if value > Decimal::ZERO {
                            csv.write_record([
                                account_id,
                                "fee_only",
                                &at.to_rfc3339(),
                                "UTC",
                                &currency,
                                "",
                                fee,
                                "",
                                &currency,
                                "0",
                                &currency,
                                &format!("{id}:fee"),
                                "true",
                                "",
                            ])
                            .map_err(|e| error(e.to_string()))?;
                        }
                    }
                }
            }
        }
        let bytes = csv.into_inner().map_err(|e| error(e.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| error(e.to_string()))?;
        let batch = format!("recording:{}", hex::encode(Sha256::digest(body.as_bytes())));
        let rows = crate::importers::parse_csv_by_preset(
            crate::importers::StatementPreset::GenericCsv,
            account_id,
            &batch,
            &text,
            &registry,
        )
        .map_err(|e| error(e.to_string()))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.normalized.ok_or_else(|| {
                error(
                    row.error_reason
                        .unwrap_or_else(|| "invalid recorded event".into()),
                )
            })?);
        }
        Ok(Self {
            provider: provider.into(),
            pages: vec![AccountSyncPageDto {
                provider: provider.into(),
                account_id: account_id.into(),
                next_cursor: None,
                balances,
                events,
            }],
        })
    }
}

impl AccountProvider for FixtureAccountProvider {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn fetch_account_page(
        &self,
        account_id: &str,
        cursor: Option<&str>,
    ) -> Result<AccountSyncPageDto, ProviderError> {
        let idx: usize = cursor
            .map(|c| {
                c.parse().map_err(|_| ProviderError::Parse {
                    provider: self.provider.clone(),
                    detail: "invalid cursor".into(),
                })
            })
            .transpose()?
            .unwrap_or(0);
        let page = self
            .pages
            .get(idx)
            .cloned()
            .ok_or_else(|| ProviderError::Parse {
                provider: self.provider.clone(),
                detail: format!("no page for cursor {idx}"),
            })?;
        if page.account_id != account_id {
            return Err(ProviderError::Parse {
                provider: self.provider.clone(),
                detail: "account scope mismatch".into(),
            });
        }
        Ok(page)
    }
}

/// Compare observed account balance snapshots against ledger projected balances and emit
/// `ReconciliationIssueRecord`s without mutating ledger history (AC-17 / FR-DATA-05).
pub fn reconcile_balances(
    account_id: &str,
    ledger_cash: &BTreeMap<(String, String), Decimal>,
    ledger_holdings: &BTreeMap<(String, String), delta_core::HoldingProjection>,
    observed_snapshots: &[AccountBalanceSnapshotDto],
) -> Vec<crate::storage::ReconciliationIssueRecord> {
    let mut issues = Vec::new();
    let reg = InstrumentRegistry::new_seeded();

    for snap in observed_snapshots {
        let asset_upper = snap.asset_id.to_ascii_uppercase();
        let expected = if let Some(c) =
            ledger_cash.get(&(account_id.to_string(), asset_upper.clone()))
        {
            *c
        } else if let Some(h) = ledger_holdings.get(&(account_id.to_string(), asset_upper.clone()))
        {
            h.quantity
        } else if let Some(resolved) = reg.resolve_symbol(None, &asset_upper) {
            ledger_holdings
                .get(&(account_id.to_string(), resolved))
                .map(|h| h.quantity)
                .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        let diff = (snap.quantity - expected).normalize();
        if !diff.is_zero() {
            issues.push(crate::storage::ReconciliationIssueRecord {
                id: format!(
                    "RECON:{account_id}:{asset_upper}:{}",
                    snap.as_of.timestamp()
                ),
                account_id: account_id.to_string(),
                asset_id: asset_upper,
                as_of: snap.as_of.to_rfc3339(),
                expected: delta_core::to_canonical_text(expected),
                observed: delta_core::to_canonical_text(snap.quantity),
                difference: delta_core::to_canonical_text(diff),
                reason:
                    "observed balance differs from ledger projection; ledger preserved unmodified"
                        .to_string(),
                resolution_event_id: None,
            });
        }
    }
    issues
}

fn provider_timestamp(seconds: i64) -> Result<DateTime<Utc>, ProviderError> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .ok_or_else(|| ProviderError::Parse {
            provider: "market-data".into(),
            detail: "timestamp out of range".into(),
        })
}
