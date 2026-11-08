use crate::registry::InstrumentRegistry;
use crate::storage::{sha256_file, SqliteStore, StorageError};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use delta_core::{parse_decimal_lossless, to_canonical_text, EconomicEventType, LedgerEventInput};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader as XmlReader;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    #[error("unrecognized or invalid header signature for preset '{preset}': {detail}")]
    InvalidHeader { preset: String, detail: String },
    #[error("unknown columns rejected in strict import mode: {0:?}")]
    UnknownColumns(Vec<String>),
    #[error("missing required columns for preset '{preset}': {missing:?}")]
    MissingColumns {
        preset: String,
        missing: Vec<String>,
    },
    #[error("import file '{path}' was modified after preview (expected hash {expected}, actual {actual})")]
    FileTamperedAfterPreview {
        path: String,
        expected: String,
        actual: String,
    },
    #[error(
        "import batch contains {0} validation errors; fix or exclude invalid rows before commit"
    )]
    ValidationErrorsPresent(usize),
    #[error("preview no longer matches the current file, mapping or ledger; preview again")]
    StalePreview,
    #[error("ledger validation failed during import commit: {0}")]
    LedgerValidation(#[from] delta_core::LedgerError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementPreset {
    FutuCsv,
    OkxCsv,
    GenericCsv,
    IbkrFlexXml,
    BinanceCsv,
    // Extension presets (M1-EXT)
    TigerCsv,
    LongbridgeCsv,
    SchwabCsv,
    EastMoneyCsv,
    IbkrActivityCsv,
}

impl StatementPreset {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FutuCsv => "futu_csv",
            Self::OkxCsv => "okx_csv",
            Self::GenericCsv => "generic_csv",
            Self::IbkrFlexXml => "ibkr_flex_xml",
            Self::BinanceCsv => "binance_csv",
            Self::TigerCsv => "tiger_csv",
            Self::LongbridgeCsv => "longbridge_csv",
            Self::SchwabCsv => "schwab_csv",
            Self::EastMoneyCsv => "eastmoney_csv",
            Self::IbkrActivityCsv => "ibkr_activity_csv",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "futu" | "futu_csv" => Some(Self::FutuCsv),
            "okx" | "okx_csv" => Some(Self::OkxCsv),
            "generic" | "generic_csv" => Some(Self::GenericCsv),
            "ibkr_flex" | "ibkr_flex_xml" | "flex" => Some(Self::IbkrFlexXml),
            "binance" | "binance_csv" => Some(Self::BinanceCsv),
            "tiger" | "tiger_csv" => Some(Self::TigerCsv),
            "longbridge" | "longbridge_csv" => Some(Self::LongbridgeCsv),
            "schwab" | "schwab_csv" => Some(Self::SchwabCsv),
            "eastmoney" | "eastmoney_csv" => Some(Self::EastMoneyCsv),
            "ibkr_activity" | "ibkr_activity_csv" => Some(Self::IbkrActivityCsv),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewRow {
    pub row_no: usize,
    pub raw_payload: String,
    pub normalized: Option<LedgerEventInput>,
    pub fingerprint: String,
    pub validation_state: String,
    pub error_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPreview {
    pub batch_id: String,
    pub account_id: String,
    pub preset: StatementPreset,
    pub file_path: String,
    pub file_hash: String,
    pub mapping_version: String,
    pub already_imported_batch_id: Option<String>,
    pub rows: Vec<PreviewRow>,
    pub valid_count: usize,
    pub error_count: usize,
    pub suspected_duplicate_count: usize,
    pub auto_subscriptions: Vec<String>,
    pub auto_fx_pairs: Vec<(String, String)>,
}

fn parse_timestamp_with_tz(raw: &str, tz_name: &str) -> Result<DateTime<Utc>, String> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(ms) = trimmed.parse::<i64>() {
        if let Some(dt) = Utc.timestamp_millis_opt(ms).single() {
            return Ok(dt);
        }
    }
    let formats = [
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y%m%d;%H%M%S",
        "%Y-%m-%d",
    ];
    let tz: Tz = tz_name
        .parse()
        .map_err(|_| format!("unknown timezone '{tz_name}'"))?;
    for fmt in formats {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            if let Some(local_dt) = tz.from_local_datetime(&ndt).single() {
                return Ok(local_dt.with_timezone(&Utc));
            }
        }
        if let Ok(nd) = NaiveDate::parse_from_str(trimmed, fmt) {
            let ndt = nd.and_hms_opt(16, 0, 0).unwrap();
            if let Some(local_dt) = tz.from_local_datetime(&ndt).single() {
                return Ok(local_dt.with_timezone(&Utc));
            }
        }
    }
    Err(format!("invalid timestamp '{trimmed}'"))
}

fn compute_content_fingerprint(ev: &LedgerEventInput) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        format!(
            "{}:{}:{}:{}:{}:{}",
            ev.account_id,
            ev.event_type.as_str(),
            ev.occurred_at.to_rfc3339(),
            ev.asset_id,
            to_canonical_text(ev.quantity),
            ev.price.map(to_canonical_text).unwrap_or_default()
        )
        .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

fn source_record_kind(kind: EconomicEventType) -> &'static str {
    match kind {
        EconomicEventType::Buy | EconomicEventType::Sell => "trade",
        EconomicEventType::Deposit
        | EconomicEventType::Withdrawal
        | EconomicEventType::TransferIn
        | EconomicEventType::TransferOut => "cash_movement",
        EconomicEventType::PositionTransferIn | EconomicEventType::PositionTransferOut => {
            "position_transfer"
        }
        other => other.as_str(),
    }
}

fn source_payload_hash(ev: &LedgerEventInput) -> String {
    let payload = serde_json::to_string(&(
        ev.event_type,
        ev.occurred_at,
        &ev.asset_id,
        &ev.instrument_id,
        to_canonical_text(ev.quantity),
        ev.price.map(to_canonical_text),
        &ev.quote_currency,
        to_canonical_text(ev.fee_amount),
        &ev.fee_currency,
        ev.cost_known,
        &ev.counterparty_account_id,
        &ev.transfer_group_id,
    ))
    .unwrap();
    hex::encode(Sha256::digest(payload.as_bytes()))
}

fn verify_csv_headers(
    preset: StatementPreset,
    actual_headers: &csv::StringRecord,
    required: &[&str],
    allowed: &[&str],
) -> Result<HashMap<String, usize>, ImportError> {
    let allowed_set: HashSet<String> = allowed.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut unknown = Vec::new();
    let mut index_map = HashMap::new();

    for (idx, h) in actual_headers.iter().enumerate() {
        let norm = h.trim().trim_start_matches('\u{feff}').to_ascii_lowercase();
        if !allowed_set.contains(&norm) {
            unknown.push(h.trim().to_string());
        }
        if index_map.insert(norm.clone(), idx).is_some() {
            return Err(ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: format!("duplicate column '{norm}'"),
            });
        }
    }
    if !unknown.is_empty() {
        return Err(ImportError::UnknownColumns(unknown));
    }

    let mut missing = Vec::new();
    for req in required {
        if !index_map.contains_key(&req.to_ascii_lowercase()) {
            missing.push((*req).to_string());
        }
    }
    if !missing.is_empty() {
        return Err(ImportError::MissingColumns {
            preset: preset.as_str().to_string(),
            missing,
        });
    }
    Ok(index_map)
}

/// Preview a statement file (CSV or IBKR Flex XML) against `SqliteStore`, performing strict header
/// verification, Decimal parsing, idempotency check, and cross-file duplicate detection.
pub fn preview_statement_import(
    store: &SqliteStore,
    account_id: &str,
    preset: StatementPreset,
    file_path: &Path,
) -> Result<ImportPreview, ImportError> {
    let file_hash = sha256_file(file_path)?;
    let raw_content = fs::read_to_string(file_path)?;
    let reg = store.load_instrument_registry()?;

    // Check if identical file_hash was already committed for this account (idempotent skip)
    let existing_batch: Option<String> = store
        .conn()
        .query_row(
            "SELECT id FROM import_batch WHERE account_id = ?1 AND file_hash = ?2 AND status = 'committed' LIMIT 1",
            params![account_id, file_hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(StorageError::from)?;

    // Collect existing content fingerprints from OTHER committed batches for cross-file duplicate check
    let mut existing_fingerprints: HashSet<String> = HashSet::new();
    {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT r.fingerprint FROM import_row r
                 JOIN import_batch b ON r.batch_id = b.id
                 WHERE b.account_id = ?1 AND b.file_hash != ?2 AND b.status = 'committed'",
            )
            .map_err(StorageError::from)?;
        let rows = stmt
            .query_map(params![account_id, file_hash], |r| r.get::<_, String>(0))
            .map_err(StorageError::from)?;
        for fp in rows {
            existing_fingerprints.insert(fp.map_err(StorageError::from)?);
        }
    }

    let batch_id = format!("BATCH:{account_id}:{}", &file_hash[..12]);
    let mut rows = if preset == StatementPreset::IbkrFlexXml {
        parse_ibkr_flex_xml(account_id, &batch_id, &raw_content, &reg)?
    } else {
        parse_csv_by_preset(preset, account_id, &batch_id, &raw_content, &reg)?
    };

    let mut valid_count = 0usize;
    let mut error_count = 0usize;
    let mut suspected_duplicate_count = 0usize;
    let mut auto_subs = BTreeSet::new();
    let mut auto_fx = BTreeSet::new();
    let mut seen_source_ids = HashSet::new();

    for row in &mut rows {
        if let Some(event) = &mut row.normalized {
            event.event_id = format!("EV:{batch_id}:{}", row.row_no);
            event.correction_group_id = Some(format!("CG:{}", event.event_id));
            event.source_ref = event.source_ref.take().filter(|id| !id.trim().is_empty());
            if let Some(instrument) = event
                .instrument_id
                .as_ref()
                .and_then(|id| reg.instruments.get(id))
            {
                event.asset_id = instrument.base_asset_id.clone();
            }
            row.fingerprint = compute_content_fingerprint(event);
        }
        if row.normalized.as_ref().is_some_and(|ev| {
            matches!(
                ev.event_type,
                EconomicEventType::Buy | EconomicEventType::Sell
            ) && ev.instrument_id.is_none()
        }) {
            row.validation_state = "error".into();
            row.error_reason =
                Some("unknown market symbol; register its listing venue before import".into());
        }
        if row.validation_state == "error" {
            error_count += 1;
            continue;
        }
        if let Some(ev) = row.normalized.as_ref() {
            if let Some(source_id) = ev.source_ref.as_ref() {
                if !seen_source_ids.insert((source_record_kind(ev.event_type), source_id.clone())) {
                    row.validation_state = "error".into();
                    row.error_reason = Some("duplicate source business ID in one file".into());
                    error_count += 1;
                    continue;
                }
                let existing: Option<String> = store.conn().query_row("SELECT payload_hash FROM source_record WHERE provider=?1 AND account_id=?2 AND record_type=?3 AND external_id=?4 ORDER BY revision DESC LIMIT 1",params![preset.as_str(),account_id,source_record_kind(ev.event_type),source_id],|r|r.get(0)).optional().map_err(StorageError::from)?;
                if let Some(hash) = existing {
                    if hash == source_payload_hash(ev) {
                        row.validation_state = "source_duplicate".into();
                        row.error_reason =
                            Some("same source business ID and content already committed".into());
                    } else {
                        row.validation_state = "error".into();
                        row.error_reason =
                            Some("source business ID changed; use an explicit correction".into());
                        error_count += 1;
                    }
                    continue;
                }
            }
        }
        if row
            .normalized
            .as_ref()
            .is_some_and(|ev| ev.source_ref.is_none())
            && existing_fingerprints.contains(&row.fingerprint)
        {
            row.validation_state = "suspected_duplicate".to_string();
            row.error_reason = Some(
                "same timestamp, asset, quantity, and price found in a previous import file; queued for manual review"
                    .to_string(),
            );
            suspected_duplicate_count += 1;
        } else {
            valid_count += 1;
        }
        if let Some(ev) = &row.normalized {
            if let Some(inst) = &ev.instrument_id {
                auto_subs.insert(inst.clone());
            }
            let q = ev.quote_currency.to_ascii_uppercase();
            if q != "USD" {
                auto_fx.insert((q, "USD".to_string()));
            }
            let f = ev.fee_currency.to_ascii_uppercase();
            if ev.fee_amount > Decimal::ZERO && f != "USD" {
                auto_fx.insert((f, "USD".to_string()));
            }
        }
    }

    Ok(ImportPreview {
        batch_id,
        account_id: account_id.to_string(),
        preset,
        file_path: file_path.display().to_string(),
        file_hash,
        mapping_version: format!("{}-v2", preset.as_str()),
        already_imported_batch_id: existing_batch,
        rows,
        valid_count,
        error_count,
        suspected_duplicate_count,
        auto_subscriptions: auto_subs.into_iter().collect(),
        auto_fx_pairs: auto_fx.into_iter().collect(),
    })
}

pub(crate) fn parse_csv_by_preset(
    preset: StatementPreset,
    account_id: &str,
    batch_id: &str,
    raw_content: &str,
    reg: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(raw_content.as_bytes());
    let headers = rdr.headers()?.clone();
    let normalized: HashSet<_> = headers
        .iter()
        .map(|h| h.trim().trim_start_matches('\u{feff}').to_ascii_lowercase())
        .collect();
    if let Some(account_column) = headers
        .iter()
        .position(|h| matches!(h.trim().to_ascii_lowercase().as_str(), "account" | "账户"))
    {
        let mut source_accounts = HashSet::new();
        let mut scan = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .from_reader(raw_content.as_bytes());
        for row in scan.records() {
            let row = row?;
            if let Some(source) = row.get(account_column).filter(|s| !s.is_empty()) {
                source_accounts.insert(source.to_string());
            }
        }
        if source_accounts.len() > 1 {
            return Err(ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: "one import file must belong to one bound account".into(),
            });
        }
    }
    let funding = match preset {
        StatementPreset::FutuCsv => normalized.contains("amount") || normalized.contains("金额"),
        StatementPreset::OkxCsv => normalized.contains("currency") || normalized.contains("币种"),
        StatementPreset::BinanceCsv => normalized.contains("coin") && !normalized.contains("pair"),
        _ => false,
    };
    if funding {
        return parse_funding_csv(preset, account_id, batch_id, raw_content, reg);
    }

    match preset {
        StatementPreset::GenericCsv => {
            let required = [
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
            ];
            let allowed = [
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
                "cost_known",
                "counterparty_account",
                "source_id",
            ];
            let map = verify_csv_headers(preset, &headers, &required, &allowed)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let ev_type_str = get("event_type").to_ascii_lowercase();
                    let side_str = get("side").to_ascii_lowercase();
                    let ev_type = match ev_type_str.as_str() {
                        "opening_balance" | "opening" => EconomicEventType::OpeningBalance,
                        "buy" => EconomicEventType::Buy,
                        "sell" => EconomicEventType::Sell,
                        "trade" => match side_str.as_str() {
                            "buy" => EconomicEventType::Buy,
                            "sell" => EconomicEventType::Sell,
                            _ => return Err(format!("unsupported trade side '{side_str}'")),
                        },
                        "deposit" => EconomicEventType::Deposit,
                        "withdrawal" => EconomicEventType::Withdrawal,
                        "transfer_out" => EconomicEventType::TransferOut,
                        "transfer_in" => EconomicEventType::TransferIn,
                        "position_transfer_out" => EconomicEventType::PositionTransferOut,
                        "position_transfer_in" => EconomicEventType::PositionTransferIn,
                        "cash_dividend" | "dividend" => EconomicEventType::CashDividend,
                        "interest" => EconomicEventType::Interest,
                        "fee_only" | "fee" => EconomicEventType::FeeOnly,
                        "split" => EconomicEventType::Split,
                        other => return Err(format!("unsupported event_type '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("occurred_at"), get("timezone"))?;
                    let sym_raw = get("symbol");
                    let inst_id = reg.resolve_symbol(None, sym_raw);
                    let asset_id = inst_id
                        .as_ref()
                        .and_then(|id| reg.instruments.get(id))
                        .map(|inst| inst.base_asset_id.clone())
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get("quantity")).map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let price_str = get("price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let fee_str = get("fee_amount");
                    let fee = if fee_str.is_empty() {
                        Decimal::ZERO
                    } else {
                        parse_decimal_lossless(fee_str).map_err(|e| e.to_string())?
                    };
                    if fee < Decimal::ZERO {
                        return Err("generic fees must be non-negative; rebates need an explicit income event".into());
                    }
                    let cost_known_str = get("cost_known");
                    let cost_known = match cost_known_str.to_ascii_lowercase().as_str() {
                        "" | "true" | "1" | "yes" => true,
                        "false" | "0" | "no" => false,
                        other => return Err(format!("invalid cost_known '{other}'")),
                    };
                    let cp = {
                        let s = get("counterparty_account");
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        }
                    };
                    let src_id = get("source_id");
                    let ev_id = if src_id.is_empty() {
                        format!("EV:{batch_id}:{row_no}")
                    } else {
                        format!("EV:{account_id}:{src_id}:{row_no}")
                    };
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: get("quote_currency").to_ascii_uppercase(),
                        fee_amount: fee.abs(),
                        fee_currency: get("fee_currency").to_ascii_uppercase(),
                        cost_known,
                        counterparty_account_id: cp,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: if src_id.is_empty() {
                            None
                        } else {
                            Some(src_id.to_string())
                        },
                    })
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(out)
        }
        StatementPreset::FutuCsv => {
            // Supports both Chinese and English Futu headers
            let is_zh = headers
                .iter()
                .any(|h| h.contains("成交数量") || h.contains("方向"));
            let map = if is_zh {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "账户",
                        "方向",
                        "代码",
                        "成交时间",
                        "成交数量",
                        "成交价格",
                        "币种",
                        "佣金",
                        "平台使用费",
                        "流水号",
                    ],
                    &[
                        "账户",
                        "方向",
                        "代码",
                        "名称",
                        "成交时间",
                        "成交数量",
                        "成交价格",
                        "币种",
                        "佣金",
                        "平台使用费",
                        "交收费",
                        "证监会规费",
                        "交易活动费",
                        "流水号",
                    ],
                )?
            } else {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "account",
                        "side",
                        "symbol",
                        "tradetime",
                        "quantity",
                        "price",
                        "currency",
                        "commission",
                        "platformfee",
                        "tradeid",
                    ],
                    &[
                        "account",
                        "side",
                        "symbol",
                        "name",
                        "tradetime",
                        "quantity",
                        "price",
                        "currency",
                        "commission",
                        "platformfee",
                        "settlementfee",
                        "regulatoryfee",
                        "activityfee",
                        "tradeid",
                    ],
                )?
            };
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get_either = |zh: &str, en: &str| -> &str {
                    let key = if is_zh { zh } else { en };
                    map.get(key).and_then(|&i| rec.get(i)).unwrap_or("").trim()
                };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let side_raw = get_either("方向", "side").to_ascii_lowercase();
                    let ev_type = if side_raw.contains("买") || side_raw == "buy" {
                        EconomicEventType::Buy
                    } else if side_raw.contains("卖") || side_raw == "sell" {
                        EconomicEventType::Sell
                    } else if side_raw.contains("入金") || side_raw == "deposit" {
                        EconomicEventType::Deposit
                    } else {
                        return Err(format!("unsupported Futu side '{side_raw}'"));
                    };
                    let occurred_at = parse_timestamp_with_tz(
                        get_either("成交时间", "tradetime"),
                        "America/New_York",
                    )?;
                    let sym_raw = get_either("代码", "symbol");
                    let inst_id = reg.resolve_symbol(Some("futu"), sym_raw);
                    let asset_id = inst_id
                        .clone()
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get_either("成交数量", "quantity"))
                        .map_err(|e| e.to_string())?;
                    let price_str = get_either("成交价格", "price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let ccy = get_either("币种", "currency").to_ascii_uppercase();

                    // Sum all Futu fee components (commission + platform + settlement + reg + activity)
                    let mut total_fee = Decimal::ZERO;
                    for (zh_f, en_f) in [
                        ("佣金", "commission"),
                        ("平台使用费", "platformfee"),
                        ("交收费", "settlementfee"),
                        ("证监会规费", "regulatoryfee"),
                        ("交易活动费", "activityfee"),
                    ] {
                        let v = get_either(zh_f, en_f);
                        if !v.is_empty() {
                            let expense = parse_decimal_lossless(v).map_err(|e| e.to_string())?;
                            if expense < Decimal::ZERO {
                                return Err(
                                    "negative Futu fee requires an explicit rebate income row"
                                        .into(),
                                );
                            }
                            total_fee += expense;
                        }
                    }
                    let trade_id = get_either("流水号", "tradeid");
                    let ev_id = format!("EV:{account_id}:FUTU:{trade_id}:{row_no}");
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: ccy.clone(),
                        fee_amount: total_fee.normalize(),
                        fee_currency: ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(trade_id.to_string()),
                    })
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(out)
        }
        StatementPreset::OkxCsv => {
            let is_zh = headers
                .iter()
                .any(|h| h.contains("账单ID") || h.contains("交易对"));
            let map = if is_zh {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "账户",
                        "账单id",
                        "时间",
                        "交易对",
                        "类型",
                        "方向",
                        "成交数量",
                        "成交均价",
                        "计价币种",
                        "手续费",
                        "手续费币种",
                    ],
                    &[
                        "账户",
                        "账单id",
                        "时间",
                        "交易对",
                        "类型",
                        "方向",
                        "成交数量",
                        "成交均价",
                        "计价币种",
                        "手续费",
                        "手续费币种",
                    ],
                )?
            } else {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "account",
                        "billid",
                        "timestamp",
                        "instrument",
                        "type",
                        "side",
                        "size",
                        "price",
                        "quotecurrency",
                        "fee",
                        "feecurrency",
                    ],
                    &[
                        "account",
                        "billid",
                        "timestamp",
                        "instrument",
                        "type",
                        "side",
                        "size",
                        "price",
                        "quotecurrency",
                        "fee",
                        "feecurrency",
                    ],
                )?
            };
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get_either = |zh: &str, en: &str| -> &str {
                    let key = if is_zh { zh } else { en };
                    map.get(key).and_then(|&i| rec.get(i)).unwrap_or("").trim()
                };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let type_raw = get_either("类型", "type").to_ascii_lowercase();
                    let side_raw = get_either("方向", "side").to_ascii_lowercase();
                    let ev_type = if type_raw.contains("充") || type_raw == "deposit" {
                        EconomicEventType::Deposit
                    } else if type_raw.contains("提") || type_raw == "withdrawal" {
                        EconomicEventType::Withdrawal
                    } else if side_raw.contains("买") || side_raw == "buy" {
                        EconomicEventType::Buy
                    } else if side_raw.contains("卖") || side_raw == "sell" {
                        EconomicEventType::Sell
                    } else {
                        return Err(format!("unsupported OKX type/side '{type_raw}/{side_raw}'"));
                    };
                    let occurred_at =
                        parse_timestamp_with_tz(get_either("时间", "timestamp"), "UTC")?;
                    let inst_raw = get_either("交易对", "instrument");
                    let inst_id = reg.resolve_symbol(Some("okx"), inst_raw);
                    let base_asset = inst_id
                        .as_ref()
                        .and_then(|id| reg.instruments.get(id))
                        .map(|i| i.base_asset_id.clone())
                        .unwrap_or_else(|| {
                            inst_raw
                                .split(['-', '/'])
                                .next()
                                .unwrap_or(inst_raw)
                                .to_ascii_uppercase()
                        });
                    let qty = parse_decimal_lossless(get_either("成交数量", "size"))
                        .map_err(|e| e.to_string())?;
                    let price_str = get_either("成交均价", "price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let quote_ccy = get_either("计价币种", "quotecurrency").to_ascii_uppercase();
                    let fee_raw = get_either("手续费", "fee");
                    let fee = if fee_raw.is_empty() {
                        Decimal::ZERO
                    } else {
                        let expense = parse_decimal_lossless(fee_raw).map_err(|e| e.to_string())?;
                        if expense < Decimal::ZERO {
                            return Err(
                                "negative OKX CSV fee requires an explicit rebate income row"
                                    .into(),
                            );
                        }
                        expense
                    };
                    let fee_ccy = get_either("手续费币种", "feecurrency").to_ascii_uppercase();
                    if matches!(
                        ev_type,
                        EconomicEventType::Deposit | EconomicEventType::Withdrawal
                    ) && base_asset != quote_ccy
                    {
                        return Err("crypto funding requires the independent currency/amount funding template".into());
                    }
                    let bill_id = get_either("账单id", "billid");
                    let ev_id = format!("EV:{account_id}:OKX:{bill_id}:{row_no}");

                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id: base_asset,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: quote_ccy,
                        fee_amount: fee,
                        fee_currency: fee_ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(bill_id.to_string()),
                    })
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(out)
        }
        StatementPreset::BinanceCsv => {
            let required = [
                "date(utc)",
                "pair",
                "side",
                "price",
                "executed",
                "amount",
                "fee",
                "fee coin",
            ];
            let mut allowed = required.to_vec();
            allowed.extend(["account", "tradeid"]);
            let map = verify_csv_headers(preset, &headers, &required, &allowed)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let side_str = get("side").to_ascii_lowercase();
                    let ev_type = match side_str.as_str() {
                        "buy" => EconomicEventType::Buy,
                        "sell" => EconomicEventType::Sell,
                        "deposit" => EconomicEventType::Deposit,
                        "withdrawal" => EconomicEventType::Withdrawal,
                        other => return Err(format!("unsupported Binance side '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("date(utc)"), "UTC")?;
                    let pair_raw = get("pair");
                    let inst_id = reg.resolve_symbol(Some("binance"), pair_raw);
                    let (base_asset, quote_ccy) = if let Some(id) = &inst_id {
                        let inst = reg.instruments.get(id).unwrap();
                        (inst.base_asset_id.clone(), inst.quote_asset_id.clone())
                    } else {
                        (pair_raw.to_ascii_uppercase(), "USDT".to_string())
                    };
                    let qty = parse_decimal_lossless(get("executed")).map_err(|e| e.to_string())?;
                    let price_str = get("price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let fee = parse_decimal_lossless(get("fee")).map_err(|e| e.to_string())?;
                    if fee < Decimal::ZERO {
                        return Err(
                            "negative Binance fee requires an explicit rebate income row".into(),
                        );
                    }
                    if matches!(
                        ev_type,
                        EconomicEventType::Deposit | EconomicEventType::Withdrawal
                    ) && base_asset != quote_ccy
                    {
                        return Err(
                            "crypto funding requires the independent coin/amount funding template"
                                .into(),
                        );
                    }
                    let fee_ccy = get("fee coin").to_ascii_uppercase();
                    let tid = get("tradeid");
                    let ev_id = format!("EV:{account_id}:BN:{tid}:{row_no}");
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id: base_asset,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: quote_ccy,
                        fee_amount: fee,
                        fee_currency: fee_ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(tid.to_string()),
                    })
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(out)
        }
        // Extension presets (M1-EXT): Tiger, Longbridge, Schwab, EastMoney, IBKR Activity CSV
        StatementPreset::TigerCsv
        | StatementPreset::LongbridgeCsv
        | StatementPreset::SchwabCsv
        | StatementPreset::EastMoneyCsv
        | StatementPreset::IbkrActivityCsv => {
            let required = [
                "account", "datetime", "symbol", "action", "quantity", "price", "currency", "fee",
                "tradeid",
            ];
            let map = verify_csv_headers(preset, &headers, &required, &required)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let act = get("action").to_ascii_lowercase();
                    let ev_type = match act.as_str() {
                        "buy" | "买入" | "证券买入" => EconomicEventType::Buy,
                        "sell" | "卖出" | "证券卖出" => EconomicEventType::Sell,
                        "dividend" | "分红" => EconomicEventType::CashDividend,
                        "deposit" | "入金" => EconomicEventType::Deposit,
                        other => return Err(format!("unsupported action '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("datetime"), "America/New_York")?;
                    let sym_raw = get("symbol");
                    let inst_id = reg.resolve_symbol(None, sym_raw);
                    let asset_id = inst_id
                        .clone()
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get("quantity")).map_err(|e| e.to_string())?;
                    let p_str = get("price");
                    let price = if p_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(p_str).map_err(|e| e.to_string())?)
                    };
                    let ccy = get("currency").to_ascii_uppercase();
                    let fee = parse_decimal_lossless(get("fee")).map_err(|e| e.to_string())?;
                    if fee < Decimal::ZERO {
                        return Err(
                            "negative expense fee requires an explicit rebate income row".into(),
                        );
                    }
                    let tid = get("tradeid");
                    let ev_id = format!("EV:{account_id}:{}:{tid}:{row_no}", preset.as_str());
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: ccy.clone(),
                        fee_amount: fee,
                        fee_currency: ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(tid.to_string()),
                    })
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(out)
        }
        StatementPreset::IbkrFlexXml => unreachable!(),
    }
}

// Funding presets translate into the same strict CSV engine as trades. The v2
// templates use non-negative expense fees; signed exchange REST bills are a
// separate protocol and rebates must be explicit income rows.
fn parse_funding_csv(
    preset: StatementPreset,
    account_id: &str,
    batch_id: &str,
    content: &str,
    registry: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(content.as_bytes());
    let headers = reader.headers()?.clone();
    let zh = headers.iter().any(|h| h.contains("币种"));
    let (required, allowed): (&[&str], &[&str]) = match (preset, zh) {
        (StatementPreset::FutuCsv, false) => (
            &["type", "time", "amount", "currency"],
            &[
                "account",
                "type",
                "time",
                "amount",
                "currency",
                "fee",
                "transactionid",
                "counterpartyaccount",
                "symbol",
            ],
        ),
        (StatementPreset::FutuCsv, true) => (
            &["类型", "时间", "金额", "币种"],
            &[
                "账户",
                "类型",
                "时间",
                "金额",
                "币种",
                "手续费",
                "流水号",
                "对方账户",
                "代码",
            ],
        ),
        (StatementPreset::OkxCsv, false) => (
            &["timestamp", "type", "currency", "amount"],
            &[
                "account",
                "billid",
                "timestamp",
                "type",
                "currency",
                "amount",
                "fee",
                "feecurrency",
                "counterpartyaccount",
            ],
        ),
        (StatementPreset::OkxCsv, true) => (
            &["时间", "类型", "币种", "数量"],
            &[
                "账户",
                "账单id",
                "时间",
                "类型",
                "币种",
                "数量",
                "手续费",
                "手续费币种",
                "对方账户",
            ],
        ),
        (StatementPreset::BinanceCsv, _) => (
            &["date(utc)", "type", "coin", "amount"],
            &[
                "account",
                "date(utc)",
                "type",
                "coin",
                "amount",
                "fee",
                "fee coin",
                "transactionid",
                "counterpartyaccount",
            ],
        ),
        _ => unreachable!(),
    };
    let map = verify_csv_headers(preset, &headers, required, allowed)?;
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record([
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
    ])?;
    let mut original_lines = Vec::new();
    for record in reader.records() {
        let record = record?;
        let get = |key: &str| {
            map.get(key)
                .and_then(|&i| record.get(i))
                .unwrap_or("")
                .trim()
        };
        let choose = |en: &str, cn: &str| if zh { get(cn) } else { get(en) };
        let currency = match preset {
            StatementPreset::BinanceCsv => get("coin"),
            _ => choose("currency", "币种"),
        }
        .to_ascii_uppercase();
        let amount = choose(
            "amount",
            if preset == StatementPreset::FutuCsv {
                "金额"
            } else {
                "数量"
            },
        );
        let timestamp = match preset {
            StatementPreset::BinanceCsv => get("date(utc)"),
            StatementPreset::OkxCsv => choose("timestamp", "时间"),
            _ => choose("time", "时间"),
        };
        let kind = choose("type", "类型").to_ascii_lowercase();
        let crypto = !delta_core::is_fiat_currency(&currency)
            && !matches!(currency.as_str(), "USDT" | "USDC");
        let provider = if preset == StatementPreset::OkxCsv {
            "OKX"
        } else {
            "BINANCE"
        };
        let market = registry.instruments.values().find(|i| {
            i.venue == provider && i.base_asset_id == currency && i.quote_asset_id == "USDT"
        });
        let quantity = parse_decimal_lossless(amount).map_err(|e| ImportError::InvalidHeader {
            preset: preset.as_str().into(),
            detail: e.to_string(),
        })?;
        let event_type = match kind.as_str() {
            "deposit" | "充值" | "入金" => {
                if crypto {
                    "opening_balance"
                } else {
                    "deposit"
                }
            }
            "withdrawal" | "withdraw" | "提现" | "提币" | "出金" => {
                if crypto {
                    "position_transfer_out"
                } else {
                    "withdrawal"
                }
            }
            "transfer_out" | "转出" => {
                if crypto {
                    "position_transfer_out"
                } else {
                    "transfer_out"
                }
            }
            "transfer_in" | "转入" => {
                if crypto {
                    "position_transfer_in"
                } else {
                    "transfer_in"
                }
            }
            "dividend" | "分红" => "cash_dividend",
            "interest" | "利息" => "interest",
            "fee" | "手续费" => "fee_only",
            other => {
                return Err(ImportError::InvalidHeader {
                    preset: preset.as_str().into(),
                    detail: format!("unsupported funding type '{other}'"),
                })
            }
        };
        if quantity <= Decimal::ZERO
            && !matches!(
                event_type,
                "withdrawal" | "position_transfer_out" | "transfer_out" | "fee_only"
            )
        {
            return Err(ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: "funding inflow must be positive".into(),
            });
        }
        if crypto
            && !matches!(
                event_type,
                "opening_balance" | "position_transfer_in" | "position_transfer_out" | "fee_only"
            )
        {
            return Err(ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: "crypto income requires an explicit priced ledger event".into(),
            });
        }
        let symbol = if crypto {
            market
                .map(|i| i.id.clone())
                .ok_or_else(|| ImportError::InvalidHeader {
                    preset: preset.as_str().into(),
                    detail: format!(
                        "register a {provider}:{currency}/USDT market before importing funding"
                    ),
                })?
        } else {
            currency.clone()
        };
        let source = match preset {
            StatementPreset::OkxCsv => choose("billid", "账单id"),
            _ => choose("transactionid", "流水号"),
        };
        let cp = choose("counterpartyaccount", "对方账户");
        let destination = if cp.is_empty() && event_type == "position_transfer_out" {
            format!("external:{}", preset.as_str())
        } else {
            cp.to_string()
        };
        let quote = if crypto && event_type != "fee_only" {
            "USD"
        } else {
            &currency
        };
        let timezone = if preset == StatementPreset::FutuCsv {
            "America/New_York"
        } else {
            "UTC"
        };
        writer.write_record([
            account_id,
            event_type,
            timestamp,
            timezone,
            &symbol,
            "",
            &to_canonical_text(quantity.abs()),
            "",
            quote,
            "0",
            quote,
            source,
            if crypto && event_type == "opening_balance" {
                "false"
            } else {
                "true"
            },
            &destination,
        ])?;
        let raw = serde_json::to_string(&record.iter().collect::<Vec<_>>()).unwrap();
        original_lines.push(raw.clone());
        let fee_raw = choose("fee", "手续费");
        if !fee_raw.is_empty() {
            let fee = parse_decimal_lossless(fee_raw).map_err(|e| ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: e.to_string(),
            })?;
            if fee < Decimal::ZERO {
                return Err(ImportError::InvalidHeader {
                    preset: preset.as_str().into(),
                    detail: "negative funding fee needs an explicit rebate income row".into(),
                });
            }
            if fee > Decimal::ZERO {
                let fee_currency = match preset {
                    StatementPreset::BinanceCsv => get("fee coin"),
                    _ => choose("feecurrency", "手续费币种"),
                };
                let fee_currency = if fee_currency.is_empty() {
                    currency.clone()
                } else {
                    fee_currency.to_ascii_uppercase()
                };
                let fee_id = if source.is_empty() {
                    String::new()
                } else {
                    format!("{source}:fee")
                };
                writer.write_record([
                    account_id,
                    "fee_only",
                    timestamp,
                    timezone,
                    &fee_currency,
                    "",
                    &to_canonical_text(fee),
                    "",
                    &fee_currency,
                    "0",
                    &fee_currency,
                    &fee_id,
                    "true",
                    "",
                ])?;
                original_lines.push(raw);
            }
        }
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| ImportError::Io(e.into_error()))?;
    let translated = String::from_utf8(bytes).map_err(|e| ImportError::InvalidHeader {
        preset: preset.as_str().into(),
        detail: e.to_string(),
    })?;
    let mut rows = parse_csv_by_preset(
        StatementPreset::GenericCsv,
        account_id,
        batch_id,
        &translated,
        registry,
    )?;
    for (row, raw) in rows.iter_mut().zip(original_lines) {
        row.raw_payload = raw;
    }
    Ok(rows)
}

/// Pure Rust IBKR Flex Query XML parser (`<Trade>`, `<CashTransaction>`, `<CorporateAction>`, `<Transfer>`).
fn parse_ibkr_flex_xml(
    account_id: &str,
    batch_id: &str,
    xml_content: &str,
    reg: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut reader = XmlReader::from_str(xml_content);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut row_no = 0usize;
    let mut source_accounts = HashSet::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Empty(e)) | Ok(XmlEvent::Start(e)) => {
                let name_bytes = e.name();
                let xml_error = |detail: String| ImportError::InvalidHeader {
                    preset: "ibkr_flex_xml".into(),
                    detail,
                };
                let tag = std::str::from_utf8(name_bytes.as_ref())
                    .map_err(|e| xml_error(e.to_string()))?;
                let mut attrs = BTreeMap::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| xml_error(e.to_string()))?;
                    let k = std::str::from_utf8(attr.key.as_ref())
                        .map_err(|e| xml_error(e.to_string()))?
                        .to_string();
                    let v = attr
                        .unescape_value()
                        .map_err(|e| xml_error(e.to_string()))?
                        .to_string();
                    attrs.insert(k, v);
                }
                if let Some(source) = attrs.get("accountId").filter(|s| !s.is_empty()) {
                    source_accounts.insert(source.clone());
                    if source_accounts.len() > 1 {
                        return Err(xml_error(
                            "multi-account Flex files must be split before binding an account"
                                .into(),
                        ));
                    }
                }
                if !matches!(
                    tag,
                    "Trade" | "CashTransaction" | "CorporateAction" | "Transfer"
                ) {
                    continue;
                }
                row_no += 1;
                let raw_line = format!("{tag} {:?}", attrs);
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    match tag {
                        "Trade" => {
                            let side_str = attrs
                                .get("buySell")
                                .map(|s| s.to_ascii_uppercase())
                                .unwrap_or_default();
                            let ev_type = if side_str == "BUY" {
                                EconomicEventType::Buy
                            } else if side_str == "SELL" {
                                EconomicEventType::Sell
                            } else {
                                return Err(format!("invalid IBKR Trade buySell '{side_str}'"));
                            };
                            let dt_raw = attrs
                                .get("dateTime")
                                .or_else(|| attrs.get("tradeDate"))
                                .ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let qty = parse_decimal_lossless(
                                attrs.get("quantity").ok_or("missing quantity")?,
                            )
                            .map_err(|e| e.to_string())?
                            .abs();
                            let price = parse_decimal_lossless(
                                attrs.get("tradePrice").ok_or("missing tradePrice")?,
                            )
                            .map_err(|e| e.to_string())?;
                            let ccy = attrs
                                .get("currency")
                                .cloned()
                                .unwrap_or_else(|| "USD".to_string());
                            let comm_str =
                                attrs.get("ibCommission").map(|s| s.as_str()).unwrap_or("0");
                            let commission =
                                parse_decimal_lossless(comm_str).map_err(|e| e.to_string())?;
                            if commission > Decimal::ZERO {
                                return Err(
                                    "positive IBKR commission is a rebate; record explicit income"
                                        .into(),
                                );
                            }
                            let fee = -commission;
                            let fee_ccy = attrs
                                .get("ibCommissionCurrency")
                                .cloned()
                                .unwrap_or_else(|| ccy.clone());
                            let tid = attrs.get("tradeID").cloned().unwrap_or_default();
                            let ev_id = format!("EV:{account_id}:IBKR:TRD:{tid}:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: ev_type,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: qty,
                                price: Some(price),
                                quote_currency: ccy,
                                fee_amount: fee,
                                fee_currency: fee_ccy,
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: Some(tid),
                            })
                        }
                        "CashTransaction" => {
                            let ctype = attrs
                                .get("type")
                                .map(|s| s.to_ascii_lowercase())
                                .unwrap_or_default();
                            let amt = parse_decimal_lossless(
                                attrs.get("amount").ok_or("missing amount")?,
                            )
                            .map_err(|e| e.to_string())?;
                            let ccy = attrs
                                .get("currency")
                                .cloned()
                                .unwrap_or_else(|| "USD".to_string());
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let (ev_type, asset_id) = if ctype.contains("dividend") {
                                let sym = attrs.get("symbol").map(|s| s.as_str()).unwrap_or(&ccy);
                                let inst = reg.resolve_symbol(Some("ibkr"), sym);
                                (
                                    EconomicEventType::CashDividend,
                                    inst.unwrap_or_else(|| sym.to_ascii_uppercase()),
                                )
                            } else if ctype.contains("interest") && amt > Decimal::ZERO {
                                (EconomicEventType::Interest, ccy.clone())
                            } else if ctype.contains("fee")
                                || ctype.contains("tax")
                                || (ctype.contains("interest") && amt < Decimal::ZERO)
                            {
                                (EconomicEventType::FeeOnly, ccy.clone())
                            } else if ctype.contains("deposit") || ctype.contains("withdrawal") {
                                (
                                    if amt > Decimal::ZERO {
                                        EconomicEventType::Deposit
                                    } else {
                                        EconomicEventType::Withdrawal
                                    },
                                    ccy.clone(),
                                )
                            } else if ctype == "deposit" && amt >= Decimal::ZERO {
                                (EconomicEventType::Deposit, ccy.clone())
                            } else {
                                return Err(format!(
                                    "unsupported IBKR cash transaction type '{ctype}'"
                                ));
                            };
                            if ev_type == EconomicEventType::CashDividend && amt <= Decimal::ZERO {
                                return Err("negative dividend requires an explicit withholding/tax fee row".into());
                            }
                            let tid = attrs.get("transactionID").cloned().unwrap_or_default();
                            let ev_id = format!("EV:{account_id}:IBKR:CASH:{tid}:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: ev_type,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: None,
                                quantity: amt.abs(),
                                price: None,
                                quote_currency: ccy.clone(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: ccy,
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: Some(tid),
                            })
                        }
                        "CorporateAction" => {
                            if !attrs
                                .get("type")
                                .is_some_and(|s| matches!(s.as_str(), "FS" | "RS" | "split"))
                            {
                                return Err("only explicit split corporate actions are supported in this Flex template".into());
                            }
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let ratio =
                                parse_decimal_lossless(attrs.get("ratio").ok_or("missing ratio")?)
                                    .map_err(|e| e.to_string())?;
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let ev_id = format!("EV:{account_id}:IBKR:CA:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: EconomicEventType::Split,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: ratio,
                                price: None,
                                quote_currency: "USD".to_string(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: "USD".to_string(),
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: attrs.get("actionID").cloned(),
                            })
                        }
                        "Transfer" => {
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let signed_qty = parse_decimal_lossless(
                                attrs.get("quantity").ok_or("missing quantity")?,
                            )
                            .map_err(|e| e.to_string())?;
                            let qty = signed_qty.abs();
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let incoming = match attrs
                                .get("direction")
                                .map(|s| s.to_ascii_uppercase())
                                .as_deref()
                            {
                                Some("IN" | "INCOMING") => true,
                                Some("OUT" | "OUTGOING") => false,
                                Some(other) => {
                                    return Err(format!("invalid transfer direction '{other}'"))
                                }
                                None => signed_qty > Decimal::ZERO,
                            };
                            let cp = attrs
                                .get("counterpartyAccount")
                                .cloned()
                                .filter(|s| !s.is_empty());
                            let external_in = incoming && cp.is_none();
                            let cp = if !incoming && cp.is_none() {
                                Some("external:ibkr".into())
                            } else {
                                cp
                            };
                            let ev_id = format!("EV:{account_id}:IBKR:XFER:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: if external_in {
                                    EconomicEventType::OpeningBalance
                                } else if incoming {
                                    EconomicEventType::PositionTransferIn
                                } else {
                                    EconomicEventType::PositionTransferOut
                                },
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: qty,
                                price: None,
                                quote_currency: "USD".to_string(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: "USD".to_string(),
                                cost_known: !external_in,
                                counterparty_account_id: cp,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: attrs.get("transferID").cloned(),
                            })
                        }
                        _ => unreachable!(),
                    }
                })();
                match parsed {
                    Ok(ev) => {
                        let fp = compute_content_fingerprint(&ev);
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: Some(ev),
                            fingerprint: fp,
                            validation_state: "valid".to_string(),
                            error_reason: None,
                        });
                    }
                    Err(reason) => {
                        out.push(PreviewRow {
                            row_no,
                            raw_payload: raw_line,
                            normalized: None,
                            fingerprint: format!("ERR:{batch_id}:{row_no}"),
                            validation_state: "error".to_string(),
                            error_reason: Some(reason),
                        });
                    }
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(ImportError::InvalidHeader {
                    preset: "ibkr_flex_xml".to_string(),
                    detail: e.to_string(),
                })
            }
            _ => {}
        }
    }
    if row_no == 0 {
        return Err(ImportError::InvalidHeader {
            preset: "ibkr_flex_xml".to_string(),
            detail: "no Flex statements/trades found".to_string(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitImportReceipt {
    pub batch_id: String,
    pub idempotent_skipped: bool,
    pub committed_events: usize,
    pub suspected_duplicates_held: usize,
    pub auto_subscribed_instruments: Vec<String>,
    pub auto_subscribed_fx: Vec<(String, String)>,
    pub ledger_revision: i64,
}

/// Commit a previewed import batch atomically in a single SQLite transaction:
/// 1. Re-verifies `file_hash` on disk (rejecting if tampered after preview).
/// 2. If `already_imported_batch_id` is present, returns `idempotent_skipped = true` without inserting duplicates (AC-01).
/// 3. Validates the combined ledger event replay (rejecting oversells in spot mode!).
/// 4. Persists `import_batch`, `import_row`, `economic_event`, `journal_transaction`, `posting`,
///    `cost_lot`, `holding_projection`, and auto-subscriptions (`subscription`, `fx_subscription`).
pub fn commit_import_preview(
    store: &mut SqliteStore,
    preview: &ImportPreview,
) -> Result<CommitImportReceipt, ImportError> {
    // Rebuild the preview from authoritative inputs rather than trusting editable DTOs.
    let authoritative = preview_statement_import(
        store,
        &preview.account_id,
        preview.preset,
        Path::new(&preview.file_path),
    )?;
    if let Some(existing_id) = authoritative.already_imported_batch_id.as_ref() {
        let rev: i64 = store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(StorageError::from)?;
        return Ok(CommitImportReceipt {
            batch_id: existing_id.clone(),
            idempotent_skipped: true,
            committed_events: 0,
            suspected_duplicates_held: authoritative.suspected_duplicate_count,
            auto_subscribed_instruments: authoritative.auto_subscriptions,
            auto_subscribed_fx: authoritative.auto_fx_pairs,
            ledger_revision: rev,
        });
    }
    if authoritative != *preview {
        return Err(ImportError::StalePreview);
    }
    if preview.error_count > 0 {
        return Err(ImportError::ValidationErrorsPresent(preview.error_count));
    }

    let current_hash = sha256_file(Path::new(&preview.file_path))?;
    if current_hash != preview.file_hash {
        return Err(ImportError::FileTamperedAfterPreview {
            path: preview.file_path.clone(),
            expected: preview.file_hash.clone(),
            actual: current_hash,
        });
    }

    if let Some(existing_id) = &preview.already_imported_batch_id {
        let rev: i64 = store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(StorageError::from)?;
        return Ok(CommitImportReceipt {
            batch_id: existing_id.clone(),
            idempotent_skipped: true,
            committed_events: 0,
            suspected_duplicates_held: preview.suspected_duplicate_count,
            auto_subscribed_instruments: preview.auto_subscriptions.clone(),
            auto_subscribed_fx: preview.auto_fx_pairs.clone(),
            ledger_revision: rev,
        });
    }

    // Load existing committed events + new valid events to verify spot ledger invariants before committing
    let mut all_events = load_all_ledger_events(store)?;
    let new_events: Vec<LedgerEventInput> = preview
        .rows
        .iter()
        .filter(|r| r.validation_state == "valid")
        .filter_map(|r| r.normalized.clone())
        .collect();
    all_events.extend(new_events.clone());

    let book_ccy: String = store
        .conn()
        .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let next_rev: i64 = store
        .conn()
        .query_row(
            if new_events.is_empty() {
                "SELECT ledger_revision FROM library LIMIT 1"
            } else {
                "SELECT ledger_revision + 1 FROM library LIMIT 1"
            },
            [],
            |r| r.get(0),
        )
        .map_err(StorageError::from)?;

    let fx_map = load_book_fx_lookup(store, &book_ccy)?;
    let bindings = load_event_book_fx(store)?;
    let new_bindings = prepare_event_book_fx(store, &book_ccy, &new_events, &fx_map)?;
    let projection = delta_core::replay_ledger_events_with_bindings(
        &book_ccy,
        &all_events,
        &fx_map,
        &bindings,
        next_rev,
    )?;

    // Copy original import file into `imports/<file_hash>.dat` for consistent backup (S-11 / AC-09)
    let import_copy = store
        .root_dir()
        .join("imports")
        .join(format!("{}.dat", preview.file_hash));
    if !import_copy.exists() {
        fs::copy(&preview.file_path, &import_copy)?;
    }

    let reg = store.load_instrument_registry()?;
    let now = Utc::now().to_rfc3339();
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;

    // Ensure account exists
    tx.execute(
        "INSERT OR IGNORE INTO account (id, name, type, provider, status, created_at)
         VALUES (?1, ?1, 'spot', ?2, 'active', ?3)",
        params![preview.account_id, preview.preset.as_str(), now],
    )
    .map_err(StorageError::from)?;

    let counts_json = serde_json::json!({
        "valid": preview.valid_count,
        "errors": preview.error_count,
        "suspected_duplicates": preview.suspected_duplicate_count,
    })
    .to_string();

    tx.execute(
        "INSERT INTO import_batch (id, account_id, source, file_hash, mapping_version, status, counts_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'committed', ?6, ?7)",
        params![
            preview.batch_id,
            preview.account_id,
            preview.preset.as_str(),
            preview.file_hash,
            preview.mapping_version,
            counts_json,
            now,
        ],
    )
    .map_err(StorageError::from)?;

    for row in &preview.rows {
        let norm_json = row
            .normalized
            .as_ref()
            .and_then(|n| serde_json::to_string(n).ok())
            .unwrap_or_default();
        let ev_id = if row.validation_state == "valid" {
            row.normalized.as_ref().map(|n| n.event_id.clone())
        } else {
            None
        };
        tx.execute(
            "INSERT INTO import_row (id, batch_id, row_no, raw_payload, normalized_payload, fingerprint, validation_state, error_reason, event_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                format!("ROW:{}:{}", preview.batch_id, row.row_no),
                preview.batch_id,
                row.row_no as i64,
                row.raw_payload,
                norm_json,
                row.fingerprint,
                row.validation_state,
                row.error_reason,
                ev_id,
            ],
        )
        .map_err(StorageError::from)?;
    }

    for ev in &new_events {
        tx.execute(
            "INSERT INTO economic_event (
                id, account_id, type, occurred_at, sequence_no, recorded_at,
                asset_id, instrument_id, quantity, price, quote_currency,
                fee_amount, fee_currency, cost_known, counterparty_account_id,
                transfer_group_id, source_ref, correction_group_id, revision, active_in_correction_group
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                ev.event_id,
                ev.account_id,
                ev.event_type.as_str(),
                ev.occurred_at.to_rfc3339(),
                ev.sequence_no,
                now,
                ev.asset_id,
                ev.instrument_id,
                to_canonical_text(ev.quantity),
                ev.price.map(to_canonical_text),
                ev.quote_currency,
                to_canonical_text(ev.fee_amount),
                ev.fee_currency,
                if ev.cost_known { 1 } else { 0 },
                ev.counterparty_account_id,
                ev.transfer_group_id,
                ev.source_ref,
                ev.correction_group_id.as_deref().unwrap_or(&ev.event_id),
                ev.revision,
                if ev.active_in_correction_group { 1 } else { 0 },
            ],
        )
        .map_err(StorageError::from)?;
        if let Some(source_id) = ev.source_ref.as_ref() {
            tx.execute("INSERT INTO source_record(provider,account_id,record_type,external_id,revision,payload_hash) VALUES (?1,?2,?3,?4,?5,?6)",params![preview.preset.as_str(),ev.account_id,source_record_kind(ev.event_type),source_id,ev.revision,source_payload_hash(ev)]).map_err(StorageError::from)?;
        }
    }

    // Persist rebuilt projection & balanced journal transactions
    persist_event_book_fx(&tx, &new_bindings)?;
    if !new_events.is_empty() {
        persist_projection_in_tx(&tx, &projection, next_rev).map_err(StorageError::from)?;
    }

    // Register auto-subscriptions (Section 4.1 of system-architecture.md)
    for inst_id in &preview.auto_subscriptions {
        if let Some(inst) = reg.instruments.get(inst_id) {
            tx.execute(
                "INSERT OR IGNORE INTO subscription (instrument_id, venue, symbol, auto_reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    inst.id,
                    inst.venue,
                    inst.symbol,
                    format!("import_batch:{}", preview.batch_id),
                    now,
                ],
            )
            .map_err(StorageError::from)?;
        }
    }
    for (base, quote) in &preview.auto_fx_pairs {
        tx.execute(
            "INSERT OR IGNORE INTO fx_subscription (base_currency, quote_currency, auto_reason, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                base,
                quote,
                format!("import_batch:{}", preview.batch_id),
                now,
            ],
        )
        .map_err(StorageError::from)?;
    }

    tx.commit().map_err(StorageError::from)?;
    store.sync_manifest()?;

    Ok(CommitImportReceipt {
        batch_id: preview.batch_id.clone(),
        idempotent_skipped: false,
        committed_events: new_events.len(),
        suspected_duplicates_held: preview.suspected_duplicate_count,
        auto_subscribed_instruments: preview.auto_subscriptions.clone(),
        auto_subscribed_fx: preview.auto_fx_pairs.clone(),
        ledger_revision: next_rev,
    })
}

/// Amend a committed economic event via atomic reversal + replacement (`correction_group_id`, AC-03).
/// Also marks any `analysis_report` generated at an older `ledger_revision` as `stale`!
pub fn amend_committed_event(
    store: &mut SqliteStore,
    original_event_id: &str,
    new_quantity: Decimal,
    new_price: Option<Decimal>,
    new_fee: Decimal,
) -> Result<i64, ImportError> {
    let all_events = load_all_ledger_events(store)?;
    let orig = all_events
        .iter()
        .find(|e| e.event_id == original_event_id && e.active_in_correction_group)
        .ok_or_else(|| {
            delta_core::LedgerError::NotFound(format!("active event {original_event_id}"))
        })?
        .clone();

    let cg_id = orig
        .correction_group_id
        .clone()
        .unwrap_or_else(|| format!("CG:{}", orig.event_id));
    let new_rev = orig.revision + 1;
    let replacement_id = format!("{}:REV:{new_rev}", orig.event_id);

    let mut updated_events = Vec::new();
    for mut ev in all_events {
        if ev.event_id == original_event_id {
            ev.active_in_correction_group = false;
        }
        updated_events.push(ev);
    }

    let replacement = LedgerEventInput {
        event_id: replacement_id.clone(),
        quantity: new_quantity,
        price: new_price,
        fee_amount: new_fee,
        cost_known: orig.cost_known
            || (orig.event_type == EconomicEventType::OpeningBalance && new_price.is_some()),
        correction_group_id: Some(cg_id.clone()),
        revision: new_rev,
        active_in_correction_group: true,
        ..orig.clone()
    };
    updated_events.push(replacement.clone());

    let book_ccy: String = store
        .conn()
        .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let next_ledger_rev: i64 = store
        .conn()
        .query_row("SELECT ledger_revision + 1 FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let fx_map = load_book_fx_lookup(store, &book_ccy)?;
    let mut bindings = load_event_book_fx(store)?;
    for currency in [&orig.quote_currency, &orig.fee_currency] {
        if let Some(rate) = bindings
            .get(&(orig.event_id.clone(), currency.to_ascii_uppercase()))
            .copied()
        {
            bindings.insert(
                (replacement_id.clone(), currency.to_ascii_uppercase()),
                rate,
            );
        }
    }
    let proj = delta_core::replay_ledger_events_with_bindings(
        &book_ccy,
        &updated_events,
        &fx_map,
        &bindings,
        next_ledger_rev,
    )?;

    let now = Utc::now().to_rfc3339();
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;

    // Mark original event inactive in correction group (kept for audit trail)
    tx.execute(
        "UPDATE economic_event SET active_in_correction_group = 0 WHERE id = ?1",
        params![original_event_id],
    )
    .map_err(StorageError::from)?;

    // Insert reversal event for audit trail
    let rev_event_id = format!("{}:REVERSAL:{new_rev}", orig.event_id);
    tx.execute(
        "INSERT INTO economic_event (
            id, account_id, type, occurred_at, sequence_no, recorded_at,
            asset_id, instrument_id, quantity, price, quote_currency,
            fee_amount, fee_currency, cost_known, counterparty_account_id,
            transfer_group_id, source_ref, correction_group_id, revision, active_in_correction_group
         ) VALUES (?1, ?2, 'reversal', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 0)",
        params![
            rev_event_id,
            orig.account_id,
            orig.occurred_at.to_rfc3339(),
            orig.sequence_no,
            now,
            orig.asset_id,
            orig.instrument_id,
            to_canonical_text(orig.quantity),
            orig.price.map(to_canonical_text),
            orig.quote_currency,
            to_canonical_text(orig.fee_amount),
            orig.fee_currency,
            if orig.cost_known { 1 } else { 0 },
            orig.counterparty_account_id,
            orig.transfer_group_id,
            orig.source_ref,
            cg_id,
            new_rev,
        ],
    )
    .map_err(StorageError::from)?;

    // Insert replacement event
    tx.execute(
        "INSERT INTO economic_event (
            id, account_id, type, occurred_at, sequence_no, recorded_at,
            asset_id, instrument_id, quantity, price, quote_currency,
            fee_amount, fee_currency, cost_known, counterparty_account_id,
            transfer_group_id, source_ref, correction_group_id, revision, active_in_correction_group
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, 1)",
        params![
            replacement.event_id,
            replacement.account_id,
            replacement.event_type.as_str(),
            replacement.occurred_at.to_rfc3339(),
            replacement.sequence_no,
            now,
            replacement.asset_id,
            replacement.instrument_id,
            to_canonical_text(replacement.quantity),
            replacement.price.map(to_canonical_text),
            replacement.quote_currency,
            to_canonical_text(replacement.fee_amount),
            replacement.fee_currency,
            if replacement.cost_known { 1 } else { 0 },
            replacement.counterparty_account_id,
            replacement.transfer_group_id,
            replacement.source_ref,
            cg_id,
            new_rev,
        ],
    )
    .map_err(StorageError::from)?;

    let original_tx: Option<String> = tx.query_row("SELECT id FROM journal_transaction WHERE event_id=?1 ORDER BY ledger_revision DESC LIMIT 1",params![original_event_id],|r|r.get(0)).optional().map_err(StorageError::from)?;
    tx.execute("INSERT INTO event_book_fx(event_id,currency,rate,source_date,dataset_version) SELECT ?1,currency,rate,source_date,dataset_version FROM event_book_fx WHERE event_id=?2", params![replacement_id,original_event_id]).map_err(StorageError::from)?;
    if let Some(original_tx) = original_tx {
        let reversal_tx = format!("TX:{rev_event_id}:LR:{next_ledger_rev}");
        tx.execute("INSERT INTO journal_transaction (id,event_id,book_currency,posting_rule_version,reversal_of,ledger_revision) VALUES (?1,?2,?3,'v2',?4,?5)",params![reversal_tx,rev_event_id,book_ccy,original_tx,next_ledger_rev]).map_err(StorageError::from)?;
        let mut stmt = tx.prepare("SELECT id,ledger_account_id,category,native_asset_id,native_quantity,book_amount FROM posting WHERE transaction_id=?1").map_err(StorageError::from)?;
        let rows = stmt
            .query_map(params![original_tx], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(StorageError::from)?;
        for row in rows {
            let (id, account, category, asset, quantity, amount) =
                row.map_err(StorageError::from)?;
            let quantity = parse_decimal_lossless(&quantity).map_err(StorageError::from)?;
            let amount = parse_decimal_lossless(&amount).map_err(StorageError::from)?;
            tx.execute(
                "INSERT INTO posting VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    format!("{id}:REV:{new_rev}"),
                    reversal_tx,
                    account,
                    category,
                    asset,
                    to_canonical_text(-quantity),
                    to_canonical_text(-amount)
                ],
            )
            .map_err(StorageError::from)?;
        }
    }
    persist_projection_in_tx(&tx, &proj, next_ledger_rev).map_err(StorageError::from)?;
    if let Some(source_id) = replacement.source_ref.as_ref() {
        tx.execute("UPDATE source_record SET revision=?1,payload_hash=?2 WHERE account_id=?3 AND record_type=?4 AND external_id=?5 AND provider IN (SELECT b.source FROM import_batch b JOIN import_row r ON r.batch_id=b.id JOIN economic_event e ON e.id=r.event_id WHERE e.correction_group_id=?6)",params![new_rev,source_payload_hash(&replacement),replacement.account_id,source_record_kind(replacement.event_type),source_id,cg_id]).map_err(StorageError::from)?;
    }

    // Mark prior analysis_report entries as stale (AC-03 / NFR-02)
    tx.execute(
        "UPDATE analysis_report SET status = 'stale' WHERE ledger_revision < ?1",
        params![next_ledger_rev],
    )
    .map_err(StorageError::from)?;

    tx.commit().map_err(StorageError::from)?;
    store.sync_manifest()?;
    Ok(next_ledger_rev)
}

fn persist_projection_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proj: &delta_core::LedgerProjectionResult,
    ledger_revision: i64,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "UPDATE library SET ledger_revision = ?1",
        params![ledger_revision],
    )?;
    tx.execute(
        "UPDATE analysis_report SET status='stale' WHERE ledger_revision < ?1",
        params![ledger_revision],
    )?;
    tx.execute("DELETE FROM cost_lot", [])?;
    tx.execute("DELETE FROM holding_projection", [])?;

    for jtx in &proj.transactions {
        let transaction_id = format!("{}:LR:{ledger_revision}", jtx.id);
        tx.execute(
            "INSERT INTO journal_transaction (id, event_id, book_currency, posting_rule_version, reversal_of, ledger_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                transaction_id,
                jtx.event_id,
                jtx.book_currency,
                jtx.posting_rule_version,
                jtx.reversal_of,
                ledger_revision,
            ],
        )?;
        for p in &jtx.postings {
            tx.execute(
                "INSERT INTO posting (id, transaction_id, ledger_account_id, category, native_asset_id, native_quantity, book_amount)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    format!("{}:LR:{ledger_revision}",p.id),
                    transaction_id,
                    p.ledger_account_id,
                    p.category.as_str(),
                    p.native_asset_id,
                    to_canonical_text(p.native_quantity),
                    to_canonical_text(p.book_amount),
                ],
            )?;
        }
    }

    for lot in &proj.open_lots {
        tx.execute(
            "INSERT INTO cost_lot (
                id, account_id, asset_id, instrument_id, source_fill_id, acquired_at,
                original_quantity, remaining_quantity, original_cost_book, remaining_cost_book,
                cost_known, ledger_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                lot.id,
                lot.account_id,
                lot.asset_id,
                lot.instrument_id,
                lot.source_fill_id,
                lot.acquired_at.to_rfc3339(),
                to_canonical_text(lot.original_quantity),
                to_canonical_text(lot.remaining_quantity),
                to_canonical_text(lot.original_cost_book),
                to_canonical_text(lot.remaining_cost_book),
                if lot.cost_known { 1 } else { 0 },
                ledger_revision,
            ],
        )?;
    }

    for h in proj.holdings.values() {
        tx.execute(
            "INSERT INTO holding_projection (account_id, asset_id, quantity, carrying_cost, cost_known, ledger_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                h.account_id,
                h.asset_id,
                to_canonical_text(h.quantity),
                to_canonical_text(h.carrying_cost),
                if h.cost_known { 1 } else { 0 },
                ledger_revision,
            ],
        )?;
    }
    Ok(())
}

pub fn load_all_ledger_events(store: &SqliteStore) -> Result<Vec<LedgerEventInput>, StorageError> {
    let mut stmt = store.conn().prepare(
        "SELECT id, account_id, type, occurred_at, sequence_no, asset_id, instrument_id,
                quantity, price, quote_currency, fee_amount, fee_currency, cost_known,
                counterparty_account_id, transfer_group_id, correction_group_id, revision,
                active_in_correction_group, source_ref
         FROM economic_event
         ORDER BY occurred_at ASC, sequence_no ASC, id ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, String>(11)?,
            r.get::<_, i64>(12)? != 0,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, Option<String>>(14)?,
            r.get::<_, String>(15)?,
            r.get::<_, i64>(16)?,
            r.get::<_, i64>(17)? != 0,
            r.get::<_, Option<String>>(18)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            id,
            acct,
            t_str,
            occ_str,
            seq,
            asset,
            inst,
            q_str,
            p_str,
            q_ccy,
            f_str,
            f_ccy,
            ck,
            cp,
            tg,
            cg,
            rev,
            active,
            sref,
        ) = row?;
        let ev_type = match t_str.as_str() {
            "opening_balance" => EconomicEventType::OpeningBalance,
            "buy" => EconomicEventType::Buy,
            "sell" => EconomicEventType::Sell,
            "deposit" => EconomicEventType::Deposit,
            "withdrawal" => EconomicEventType::Withdrawal,
            "transfer_out" => EconomicEventType::TransferOut,
            "transfer_in" => EconomicEventType::TransferIn,
            "position_transfer_out" => EconomicEventType::PositionTransferOut,
            "position_transfer_in" => EconomicEventType::PositionTransferIn,
            "cash_dividend" => EconomicEventType::CashDividend,
            "interest" => EconomicEventType::Interest,
            "fee_only" => EconomicEventType::FeeOnly,
            "split" => EconomicEventType::Split,
            "reversal" => EconomicEventType::Reversal,
            _ => {
                return Err(StorageError::IntegrityFailed(
                    "unknown ledger event type".into(),
                ))
            }
        };
        let occurred_at = DateTime::parse_from_rfc3339(&occ_str)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| StorageError::IntegrityFailed("invalid ledger timestamp".into()))?;
        let quantity = parse_decimal_lossless(&q_str)?;
        let price = match p_str {
            Some(s) if !s.is_empty() => Some(parse_decimal_lossless(&s)?),
            _ => None,
        };
        let fee_amount = parse_decimal_lossless(&f_str)?;
        out.push(LedgerEventInput {
            event_id: id,
            account_id: acct,
            event_type: ev_type,
            occurred_at,
            sequence_no: seq,
            asset_id: asset,
            instrument_id: inst,
            quantity,
            price,
            quote_currency: q_ccy,
            fee_amount,
            fee_currency: f_ccy,
            cost_known: ck,
            counterparty_account_id: cp,
            transfer_group_id: tg,
            correction_group_id: Some(cg),
            revision: rev,
            active_in_correction_group: active,
            source_ref: sref,
        });
    }
    Ok(out)
}

pub fn load_book_fx_lookup(
    store: &SqliteStore,
    book_currency: &str,
) -> Result<HashMap<(String, NaiveDate), Decimal>, StorageError> {
    load_book_fx_lookup_at_version(store, book_currency, i64::MAX)
}

pub fn load_book_fx_lookup_at_version(
    store: &SqliteStore,
    book_currency: &str,
    version: i64,
) -> Result<HashMap<(String, NaiveDate), Decimal>, StorageError> {
    let mut stmt = store.conn().prepare(
        "SELECT f.base_currency, f.quote_currency, f.rate, f.effective_date FROM fx_rate f JOIN dataset_publication p ON p.version_num=f.dataset_version WHERE f.dataset_version <= ?1 ORDER BY f.dataset_version ASC, CASE WHEN f.base_currency='USDT' AND f.source='coinbase' THEN 2 WHEN f.base_currency='USDC' AND f.source='bitstamp' THEN 2 WHEN f.source='frankfurter' THEN 2 ELSE 1 END ASC, f.source DESC",
    )?;
    let rows = stmt.query_map(params![version], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut map = HashMap::new();
    let book = book_currency.to_ascii_uppercase();
    let mut quotes = HashMap::new();
    let mut currencies = BTreeSet::new();
    let mut dates = BTreeSet::new();
    for row in rows {
        let (base, quote, r_str, d_str) = row?;
        let date = NaiveDate::parse_from_str(&d_str, "%Y-%m-%d")
            .map_err(|_| StorageError::IntegrityFailed("invalid FX date".into()))?;
        let rate = parse_decimal_lossless(&r_str)?;
        if rate <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed("non-positive FX rate".into()));
        }
        currencies.insert(base.to_ascii_uppercase());
        currencies.insert(quote.to_ascii_uppercase());
        dates.insert(date);
        quotes.insert(
            (base.to_ascii_uppercase(), quote.to_ascii_uppercase(), date),
            rate,
        );
        if quote.eq_ignore_ascii_case(&book) {
            map.insert((base.to_ascii_uppercase(), date), rate);
        } else if base.eq_ignore_ascii_case(&book) && !rate.is_zero() {
            let inv = (Decimal::ONE / rate)
                .round_dp_with_strategy(10, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
                .normalize();
            map.insert((quote.to_ascii_uppercase(), date), inv);
        }
    }
    for date in dates {
        for currency in &currencies {
            if let Some(found) = delta_core::resolve_fx_rate_s07(
                currency,
                &book,
                date,
                !delta_core::is_fiat_currency(currency) || !delta_core::is_fiat_currency(&book),
                &quotes,
            ) {
                map.insert((currency.clone(), date), found.rate);
            }
        }
    }
    Ok(map)
}

pub fn load_event_book_fx(
    store: &SqliteStore,
) -> Result<HashMap<(String, String), Decimal>, StorageError> {
    let mut stmt = store
        .conn()
        .prepare("SELECT event_id,currency,rate FROM event_book_fx")?;
    let mut bindings = HashMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (event, currency, raw) = row?;
        let rate = parse_decimal_lossless(&raw)?;
        if rate <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "invalid event FX binding".into(),
            ));
        }
        bindings.insert((event, currency), rate);
    }
    Ok(bindings)
}

type EventFxBinding = (String, String, Decimal, NaiveDate, i64);

fn prepare_event_book_fx(
    store: &SqliteStore,
    book: &str,
    events: &[LedgerEventInput],
    rates: &HashMap<(String, NaiveDate), Decimal>,
) -> Result<Vec<EventFxBinding>, StorageError> {
    let version: i64 = store.conn().query_row(
        "SELECT COALESCE(MAX(version_num),0) FROM dataset_publication",
        [],
        |r| r.get(0),
    )?;
    let mut out = Vec::new();
    for event in events {
        for currency in [&event.quote_currency, &event.fee_currency] {
            if currency == &event.fee_currency
                && event.fee_amount.is_zero()
                && currency != &event.quote_currency
            {
                continue;
            }
            let currency = currency.to_ascii_uppercase();
            if currency.eq_ignore_ascii_case(book) {
                continue;
            }
            let date = event.occurred_at.date_naive();
            let found = (0..=4).find_map(|days| {
                if days > 0 && !delta_core::is_fiat_currency(&currency) {
                    return None;
                }
                let day = date.checked_sub_days(chrono::Days::new(days))?;
                rates.get(&(currency.clone(), day)).map(|rate| (*rate, day))
            });
            if let Some((rate, source_date)) = found {
                if !out
                    .iter()
                    .any(|(id, ccy, _, _, _)| id == &event.event_id && ccy == &currency)
                {
                    out.push((event.event_id.clone(), currency, rate, source_date, version));
                }
            }
        }
    }
    Ok(out)
}

fn persist_event_book_fx(
    tx: &rusqlite::Transaction<'_>,
    bindings: &[EventFxBinding],
) -> Result<(), StorageError> {
    for (event, currency, rate, date, version) in bindings {
        tx.execute("INSERT OR IGNORE INTO event_book_fx(event_id,currency,rate,source_date,dataset_version) VALUES (?1,?2,?3,?4,?5)", params![event,currency,to_canonical_text(*rate),date.to_string(),version])?;
    }
    Ok(())
}

fn insert_economic_event(
    tx: &rusqlite::Transaction<'_>,
    ev: &LedgerEventInput,
    now: &str,
) -> Result<(), StorageError> {
    tx.execute(
            "INSERT INTO economic_event (
                id, account_id, type, occurred_at, sequence_no, recorded_at,
                asset_id, instrument_id, quantity, price, quote_currency,
                fee_amount, fee_currency, cost_known, counterparty_account_id,
                transfer_group_id, source_ref, correction_group_id, revision, active_in_correction_group
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                ev.event_id,
                ev.account_id,
                ev.event_type.as_str(),
                ev.occurred_at.to_rfc3339(),
                ev.sequence_no,
                now,
                ev.asset_id,
                ev.instrument_id,
                to_canonical_text(ev.quantity),
                ev.price.map(to_canonical_text),
                ev.quote_currency,
                to_canonical_text(ev.fee_amount),
                ev.fee_currency,
                if ev.cost_known { 1 } else { 0 },
                ev.counterparty_account_id,
                ev.transfer_group_id,
                ev.source_ref,
                ev.correction_group_id.as_deref().unwrap_or(&ev.event_id),
                ev.revision,
                if ev.active_in_correction_group { 1 } else { 0 },
            ],
        )
        .map_err(StorageError::from)?;
    Ok(())
}

pub fn resolve_held_import_row(
    store: &mut SqliteStore,
    row_id: &str,
    include: bool,
) -> Result<i64, ImportError> {
    let (payload,state,account): (String,String,String) = store.conn().query_row("SELECT r.normalized_payload,r.validation_state,b.account_id FROM import_row r JOIN import_batch b ON b.id=r.batch_id WHERE r.id=?1",params![row_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(StorageError::from)?;
    let current_revision: i64 = store
        .conn()
        .query_row("SELECT ledger_revision FROM library", [], |r| r.get(0))
        .map_err(StorageError::from)?;
    if state == "valid" || state == "excluded_duplicate" {
        return Ok(current_revision);
    }
    if state != "suspected_duplicate" {
        return Err(ImportError::StalePreview);
    }
    if !include {
        store.conn().execute("UPDATE import_row SET validation_state='excluded_duplicate',error_reason='user excluded duplicate' WHERE id=?1",params![row_id]).map_err(StorageError::from)?;
        return Ok(current_revision);
    }
    let ev: LedgerEventInput = serde_json::from_str(&payload).map_err(StorageError::from)?;
    if ev.account_id != account {
        return Err(ImportError::StalePreview);
    }
    let mut events = load_all_ledger_events(store)?;
    events.push(ev.clone());
    let book: String = store
        .conn()
        .query_row("SELECT book_currency FROM library", [], |r| r.get(0))
        .map_err(StorageError::from)?;
    let fx = load_book_fx_lookup(store, &book)?;
    let next_revision = current_revision + 1;
    let bindings = load_event_book_fx(store)?;
    let new_bindings = prepare_event_book_fx(store, &book, std::slice::from_ref(&ev), &fx)?;
    let projection = delta_core::replay_ledger_events_with_bindings(
        &book,
        &events,
        &fx,
        &bindings,
        next_revision,
    )?;
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;
    insert_economic_event(&tx, &ev, &Utc::now().to_rfc3339())?;
    persist_event_book_fx(&tx, &new_bindings)?;
    tx.execute("UPDATE import_row SET validation_state='valid',event_id=?1,error_reason='user confirmed distinct trade' WHERE id=?2",params![ev.event_id,row_id]).map_err(StorageError::from)?;
    persist_projection_in_tx(&tx, &projection, next_revision).map_err(StorageError::from)?;
    tx.commit().map_err(StorageError::from)?;
    Ok(next_revision)
}
