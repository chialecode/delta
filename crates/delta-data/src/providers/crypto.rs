use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::{parse_decimal_lossless, AdjustmentKind, BarRecord, TermsClass, Timeframe};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::calendar::resolve_exchange_session_date;
use crate::providers::http::{
    build_provider_url, get_bytes_with_retry, get_text_with_retry, provider_bar_id,
    read_single_csv_member, sanitize_untrusted_text,
};
use crate::providers::{ProviderCapabilities, ProviderError};

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
        Self::parse_candles_json_tf(
            instrument_id,
            raw_json,
            raw_hash,
            dataset_version,
            Timeframe::D1,
        )
    }

    /// Timeframe-aware OKX candle parser (`S-50`): only confirmed, UTC-aligned bars are kept.
    pub fn parse_candles_json_tf(
        instrument_id: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
        timeframe: Timeframe,
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
                detail: format!(
                    "OKX error code {}: {}",
                    sanitize_untrusted_text(&env.code, 32),
                    sanitize_untrusted_text(&env.msg, 200)
                ),
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
            if row[8].as_str() != "1" {
                continue;
            }
            let ts_ms: i64 = row[0].parse().map_err(|_| ProviderError::Parse {
                provider: "okx".into(),
                detail: format!("invalid timestamp {}", sanitize_untrusted_text(&row[0], 64)),
            })?;
            let open_at =
                Utc.timestamp_millis_opt(ts_ms)
                    .single()
                    .ok_or_else(|| ProviderError::Parse {
                        provider: "okx".into(),
                        detail: format!("out of range timestamp {ts_ms}"),
                    })?;
            if !timeframe.is_aligned(open_at) {
                continue;
            }
            let close_at = open_at + timeframe.duration();
            let session_date = resolve_exchange_session_date("CRYPTO_24_7", open_at);

            let parse_dec = |s: &str| {
                parse_decimal_lossless(s).map_err(|e| ProviderError::Parse {
                    provider: "okx".into(),
                    detail: e.to_string(),
                })
            };

            bars.push(BarRecord {
                id: provider_bar_id(
                    instrument_id,
                    timeframe,
                    "okx",
                    session_date,
                    open_at,
                    dataset_version,
                ),
                instrument_id: instrument_id.to_string(),
                timeframe: timeframe.as_str().to_string(),
                session: "utc_24_7".to_string(),
                session_date,
                open_at,
                close_at,
                open: parse_dec(&row[1])?,
                high: parse_dec(&row[2])?,
                low: parse_dec(&row[3])?,
                close: parse_dec(&row[4])?,
                volume: parse_dec(&row[5])?,
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
        self.fetch_candles(
            client,
            instrument_id,
            inst_id_param,
            after_cursor,
            dataset_version,
            Timeframe::D1,
        )
        .await
    }

    /// `history-candles` page strictly older than `after_cursor` (milliseconds).
    pub async fn fetch_candles(
        &self,
        client: &Client,
        instrument_id: &str,
        inst_id_param: &str,
        after_cursor: Option<&str>,
        dataset_version: i64,
        timeframe: Timeframe,
    ) -> Result<(Vec<BarRecord>, String), ProviderError> {
        let mut query = vec![
            ("instId", inst_id_param),
            ("bar", timeframe.okx_bar()),
            ("limit", "100"),
        ];
        if let Some(after) = after_cursor {
            query.push(("after", after));
        }
        let url = build_provider_url(
            "okx",
            &self.base_url,
            &["api", "v5", "market", "history-candles"],
            &query,
            false,
        )?;
        let body = get_text_with_retry(client, "okx", &url, &[], 2).await?;
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        let bars = Self::parse_candles_json_tf(
            instrument_id,
            &body,
            Some(hash),
            dataset_version,
            timeframe,
        )?;
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
        for base in [&self.rest_base_url, &self.archive_base_url] {
            if base.to_ascii_lowercase().contains("api.binance.com") {
                return Err(ProviderError::ForbiddenEndpoint(base.clone()));
            }
        }
        Ok(())
    }

    /// Convert a Binance timestamp (milliseconds before 2025, microseconds in/after 2025 in archives)
    /// into a UTC `DateTime<Utc>`, preserving full microsecond precision.
    pub fn parse_binance_timestamp(raw_ts: i64) -> Result<DateTime<Utc>, ProviderError> {
        if raw_ts >= 100_000_000_000_000 {
            DateTime::from_timestamp_micros(raw_ts)
        } else {
            Utc.timestamp_millis_opt(raw_ts).single()
        }
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
        Self::parse_klines_json_tf(
            instrument_id,
            raw_json,
            raw_hash,
            now_utc,
            dataset_version,
            Timeframe::D1,
        )
    }

    /// Timeframe-aware `/api/v3/klines` parser (`S-50`).
    pub fn parse_klines_json_tf(
        instrument_id: &str,
        raw_json: &str,
        raw_hash: Option<String>,
        now_utc: DateTime<Utc>,
        dataset_version: i64,
        timeframe: Timeframe,
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
            if close_raw > now_utc {
                continue;
            }
            if !timeframe.is_aligned(open_at) {
                continue;
            }
            let close_at = open_at + timeframe.duration();
            if close_at > now_utc {
                continue;
            }
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
                id: provider_bar_id(
                    instrument_id,
                    timeframe,
                    "binance",
                    session_date,
                    open_at,
                    dataset_version,
                ),
                instrument_id: instrument_id.to_string(),
                timeframe: timeframe.as_str().to_string(),
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
    pub fn verify_and_parse_archive(
        instrument_id: &str,
        archive_bytes: &[u8],
        checksum_text: &str,
        is_zip: bool,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        Self::verify_and_parse_archive_tf(
            instrument_id,
            archive_bytes,
            checksum_text,
            is_zip,
            dataset_version,
            Timeframe::D1,
        )
    }

    /// Timeframe-aware archive verification and parsing (`S-48`, `S-50`).
    pub fn verify_and_parse_archive_tf(
        instrument_id: &str,
        archive_bytes: &[u8],
        checksum_text: &str,
        is_zip: bool,
        dataset_version: i64,
        timeframe: Timeframe,
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
        let actual_hash = hex::encode(Sha256::digest(archive_bytes));
        if expected_hash != actual_hash {
            return Err(ProviderError::ChecksumMismatch {
                file: instrument_id.to_string(),
                expected: expected_hash,
                actual: actual_hash,
            });
        }

        let csv_text = if is_zip {
            read_single_csv_member(archive_bytes)?
        } else {
            String::from_utf8(archive_bytes.to_vec()).map_err(|e| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: e.to_string(),
            })?
        };

        Self::parse_archive_csv_tf(
            instrument_id,
            &csv_text,
            Some(actual_hash),
            dataset_version,
            timeframe,
        )
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
        self.fetch_monthly_archive_tf(client, instrument_id, symbol, month, version, Timeframe::D1)
            .await
    }

    pub async fn fetch_monthly_archive_tf(
        &self,
        client: &Client,
        instrument_id: &str,
        symbol: &str,
        month: NaiveDate,
        version: i64,
        timeframe: Timeframe,
    ) -> Result<(Vec<BarRecord>, Vec<u8>, String), ProviderError> {
        self.validate_base_url()?;
        let interval = timeframe.binance_interval();
        let zip_name = format!("{symbol}-{interval}-{}.zip", month.format("%Y-%m"));
        let checksum_name = format!("{zip_name}.CHECKSUM");
        let url = build_provider_url(
            "binance-archive",
            &self.archive_base_url,
            &[
                "data", "spot", "monthly", "klines", symbol, interval, &zip_name,
            ],
            &[],
            false,
        )?;
        let checksum_url = build_provider_url(
            "binance-archive",
            &self.archive_base_url,
            &[
                "data",
                "spot",
                "monthly",
                "klines",
                symbol,
                interval,
                &checksum_name,
            ],
            &[],
            false,
        )?;
        let bytes = get_bytes_with_retry(client, "binance-archive", &url, &[], 2).await?;
        let checksum =
            get_text_with_retry(client, "binance-archive", &checksum_url, &[], 2).await?;
        let bars = Self::verify_and_parse_archive_tf(
            instrument_id,
            &bytes,
            &checksum,
            true,
            version,
            timeframe,
        )?;
        Ok((bars, bytes, checksum))
    }

    pub fn parse_archive_csv(
        instrument_id: &str,
        csv_text: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
    ) -> Result<Vec<BarRecord>, ProviderError> {
        Self::parse_archive_csv_tf(
            instrument_id,
            csv_text,
            raw_hash,
            dataset_version,
            Timeframe::D1,
        )
    }

    pub fn parse_archive_csv_tf(
        instrument_id: &str,
        csv_text: &str,
        raw_hash: Option<String>,
        dataset_version: i64,
        timeframe: Timeframe,
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
            let first_col = rec[0].trim_start_matches('\u{feff}').trim();
            if first_col.eq_ignore_ascii_case("open_time") {
                continue;
            }
            let open_ts: i64 = first_col.parse().map_err(|_| ProviderError::Parse {
                provider: "binance-archive".into(),
                detail: format!(
                    "invalid archive open_time '{}'",
                    sanitize_untrusted_text(first_col, 64)
                ),
            })?;
            let open_at = Self::parse_binance_timestamp(open_ts)?;
            if !timeframe.is_aligned(open_at) {
                continue;
            }
            let close_at = open_at + timeframe.duration();
            let session_date = resolve_exchange_session_date("CRYPTO_24_7", open_at);

            let parse_dec = |s: &str| {
                parse_decimal_lossless(s).map_err(|e| ProviderError::Parse {
                    provider: "binance-archive".into(),
                    detail: e.to_string(),
                })
            };

            out.push(BarRecord {
                id: provider_bar_id(
                    instrument_id,
                    timeframe,
                    "binance",
                    session_date,
                    open_at,
                    dataset_version,
                ),
                instrument_id: instrument_id.to_string(),
                timeframe: timeframe.as_str().to_string(),
                session: "utc_24_7".to_string(),
                session_date,
                open_at,
                close_at,
                open: parse_dec(&rec[1])?,
                high: parse_dec(&rec[2])?,
                low: parse_dec(&rec[3])?,
                close: parse_dec(&rec[4])?,
                volume: parse_dec(&rec[5])?,
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
        self.fetch_klines(
            client,
            instrument_id,
            symbol_param,
            start_time_ms,
            now_utc,
            dataset_version,
            Timeframe::D1,
            100,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_klines(
        &self,
        client: &Client,
        instrument_id: &str,
        symbol_param: &str,
        start_time_ms: Option<i64>,
        now_utc: DateTime<Utc>,
        dataset_version: i64,
        timeframe: Timeframe,
        limit: u32,
    ) -> Result<(Vec<BarRecord>, String), ProviderError> {
        self.validate_base_url()?;
        let limit_str = limit.clamp(1, 1000).to_string();
        let st_str = start_time_ms.map(|st| st.to_string());
        let mut query = vec![
            ("symbol", symbol_param),
            ("interval", timeframe.binance_interval()),
            ("limit", limit_str.as_str()),
        ];
        if let Some(ref st) = st_str {
            query.push(("startTime", st.as_str()));
        }
        let url = build_provider_url(
            "binance",
            &self.rest_base_url,
            &["api", "v3", "klines"],
            &query,
            false,
        )?;
        let body = get_text_with_retry(client, "binance", &url, &[], 2).await?;
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
        let bars = Self::parse_klines_json_tf(
            instrument_id,
            &body,
            Some(hash),
            now_utc,
            dataset_version,
            timeframe,
        )?;
        Ok((bars, body))
    }
}
