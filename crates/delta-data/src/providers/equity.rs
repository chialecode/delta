use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use delta_core::{
    parse_decimal_lossless, restore_yahoo_raw_dividend, restore_yahoo_raw_price,
    restore_yahoo_raw_volume, AdjustmentKind, BarRecord, CorporateActionRecord,
    CorporateActionType, DataQualityIssue, DirectDecimal, QualityStatus, TermsClass,
};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::calendar::resolve_exchange_session_date;
use crate::m4_schema::detect_split_jump_ratio;
use crate::providers::http::{build_provider_url, get_text_with_retry, provider_timestamp};
use crate::providers::{ProviderCapabilities, ProviderError};

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

            let mut cumulative_split_factor = Decimal::ONE;
            for (sp_date, sp_ratio) in &splits_list {
                if *sp_date > session_date {
                    cumulative_split_factor *= *sp_ratio;
                }
            }

            bars.push(BarRecord {
                id: format!("BAR:{instrument_id}:1d:raw:yahoo:{session_date}:{dataset_version}"),
                instrument_id: instrument_id.to_string(),
                timeframe: "1d".to_string(),
                session: cal_sess.kind.as_str().to_string(),
                session_date,
                open_at,
                close_at,
                open: restore_yahoo_raw_price(o.0, cumulative_split_factor),
                high: restore_yahoo_raw_price(h.0, cumulative_split_factor),
                low: restore_yahoo_raw_price(l.0, cumulative_split_factor),
                close: restore_yahoo_raw_price(c.0, cumulative_split_factor),
                volume: restore_yahoo_raw_volume(v.0, cumulative_split_factor),
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
        let p1 = period1.to_string();
        let p2 = period2.to_string();
        let url = build_provider_url(
            "yahoo",
            &self.base_url,
            &["v8", "finance", "chart", symbol],
            &[
                ("interval", "1d"),
                ("events", "div,split"),
                ("period1", &p1),
                ("period2", &p2),
            ],
            false,
        )?;
        let body = get_text_with_retry(client, "yahoo", &url, &[], 2).await?;
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
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
        let parse_dec = |s: &str| {
            parse_decimal_lossless(s).map_err(|e| ProviderError::Parse {
                provider: "eastmoney".into(),
                detail: e.to_string(),
            })
        };
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
                let open = parse_dec(parts[1])?;
                let close = parse_dec(parts[2])?;
                let high = parse_dec(parts[3])?;
                let low = parse_dec(parts[4])?;
                let volume = parse_dec(parts[5])?;

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
        let url = build_provider_url(
            "eastmoney",
            &self.base_url,
            &["api", "qt", "stock", "kline", "get"],
            &[
                ("secid", secid),
                ("fields1", "f1,f2,f3,f4,f5,f6"),
                ("fields2", "f51,f52,f53,f54,f55,f56,f57,f58,f59,f60,f61"),
                ("klt", "101"),
                ("fqt", "0"),
                ("beg", beg_yyyymmdd),
                ("end", end_yyyymmdd),
            ],
            false,
        )?;
        let body = get_text_with_retry(client, "eastmoney", &url, &[], 2).await?;
        let hash = hex::encode(Sha256::digest(body.as_bytes()));
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
        let start_s = start.to_string();
        let end_s = end.to_string();
        let url = build_provider_url(
            "tiingo",
            &self.base_url,
            &["tiingo", "daily", symbol, "prices"],
            &[("startDate", &start_s), ("endDate", &end_s)],
            false,
        )?;
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
                if r.split_factor.0 < Decimal::ZERO {
                    return Err(ProviderError::Parse {
                        provider: "tiingo".into(),
                        detail: "negative split_factor".into(),
                    });
                }
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
                if r.div_cash.0 < Decimal::ZERO {
                    return Err(ProviderError::Parse {
                        provider: "tiingo".into(),
                        detail: "negative div_cash".into(),
                    });
                }
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
        bars.sort_by_key(|b| b.open_at);
        actions.sort_by_key(|a| (a.ex_date, a.id.clone()));
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

    /// Parse `/v1/corporate-actions` response (`forward_splits`, `reverse_splits`, `cash_dividends`).
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
            reverse_splits: Vec<AlpacaSplit>,
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
        for sp in env
            .corporate_actions
            .forward_splits
            .into_iter()
            .chain(env.corporate_actions.reverse_splits)
        {
            if sp.old_rate.0 <= Decimal::ZERO || sp.new_rate.0 <= Decimal::ZERO {
                return Err(ProviderError::Parse {
                    provider: "alpaca".into(),
                    detail: "invalid split rate: old_rate and new_rate must be positive".into(),
                });
            }
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
            if div.rate.0 <= Decimal::ZERO {
                return Err(ProviderError::Parse {
                    provider: "alpaca".into(),
                    detail: "invalid cash dividend rate: must be positive".into(),
                });
            }
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
        out.sort_by_key(|a| (a.ex_date, a.id.clone()));
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
        let start_s = start.to_string();
        let end_s = end.to_string();
        let url = build_provider_url(
            "alpaca",
            &self.base_url,
            &["v1", "corporate-actions"],
            &[
                ("symbols", symbol),
                ("types", "forward_split,reverse_split,cash_dividend"),
                ("start", &start_s),
                ("end", &end_s),
            ],
            false,
        )?;
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

    let mut sorted_primary = primary_bars.to_vec();
    sorted_primary.sort_by_key(|b| b.session_date);
    for win in sorted_primary.windows(2) {
        let prev = &win[0];
        let curr = &win[1];
        if split_dates.contains_key(&curr.session_date) {
            continue;
        }
        if let Some(cand) = detect_split_jump_ratio(prev.close, curr.close) {
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
        }
    }

    let quality = if issues.is_empty() {
        QualityStatus::Exact
    } else {
        QualityStatus::Warning
    };
    CrossValidationOutcome { quality, issues }
}
