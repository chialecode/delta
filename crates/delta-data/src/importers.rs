use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use delta_core::{parse_decimal_lossless, to_canonical_text, EconomicEventType, LedgerEventInput};
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::storage::{sha256_file, SqliteStore, StorageError};

mod commit;
mod csv_presets;
mod funding;
mod fx_lookup;
mod ibkr_xml;

pub use commit::{amend_committed_event, commit_import_preview, resolve_held_import_row};
pub(crate) use csv_presets::parse_csv_by_preset;
pub use fx_lookup::{
    load_all_ledger_events, load_book_fx_lookup, load_book_fx_lookup_at_version, load_event_book_fx,
};

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

pub(crate) fn normalize_csv_header(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('\u{feff}')
        .to_ascii_lowercase()
}

pub(crate) fn parse_optional_fee(raw: &str, neg_msg: &str) -> Result<Decimal, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Decimal::ZERO);
    }
    let fee = parse_decimal_lossless(trimmed).map_err(|e| e.to_string())?;
    if fee < Decimal::ZERO {
        return Err(neg_msg.to_string());
    }
    Ok(fee)
}

fn resolve_local_datetime(tz: Tz, ndt: NaiveDateTime) -> Result<DateTime<Utc>, String> {
    match tz.from_local_datetime(&ndt) {
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        // DST fall-back ambiguous local hour: deterministically pick the earlier (pre-transition) instant.
        chrono::LocalResult::Ambiguous(earliest, _) => Ok(earliest.with_timezone(&Utc)),
        chrono::LocalResult::None => Err(format!(
            "non-existent local time '{ndt}' during DST gap in timezone '{tz}'"
        )),
    }
}

pub(crate) fn parse_timestamp_with_tz(raw: &str, tz_name: &str) -> Result<DateTime<Utc>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty timestamp".into());
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    let tz: Tz = tz_name
        .parse()
        .map_err(|_| format!("unknown timezone '{tz_name}'"))?;

    // Check 8-digit compact date `YYYYMMDD` (e.g. IBKR `tradeDate`) BEFORE numeric Unix epochs.
    if trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(nd) = NaiveDate::parse_from_str(trimmed, "%Y%m%d") {
            let ndt = nd.and_hms_opt(16, 0, 0).unwrap();
            return resolve_local_datetime(tz, ndt);
        }
        return Err(format!("invalid YYYYMMDD date '{trimmed}'"));
    }

    // Disambiguate Unix epoch seconds (9-10 digits), milliseconds (12-14 digits), and microseconds (15+ digits).
    if let Ok(val) = trimmed.parse::<i64>() {
        let dt_opt = if val >= 100_000_000_000_000 {
            DateTime::from_timestamp_micros(val)
        } else if (100_000_000_000..100_000_000_000_000).contains(&val) {
            Utc.timestamp_millis_opt(val).single()
        } else if (100_000_000..10_000_000_000).contains(&val) {
            Utc.timestamp_opt(val, 0).single()
        } else {
            None
        };
        if let Some(dt) = dt_opt {
            return Ok(dt);
        }
        return Err(format!("invalid numeric timestamp '{trimmed}'"));
    }

    let dt_formats = [
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y%m%d;%H%M%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
    ];
    for fmt in dt_formats {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return resolve_local_datetime(tz, ndt);
        }
    }
    let date_formats = ["%Y-%m-%d", "%Y/%m/%d"];
    for fmt in date_formats {
        if let Ok(nd) = NaiveDate::parse_from_str(trimmed, fmt) {
            let ndt = nd.and_hms_opt(16, 0, 0).unwrap();
            return resolve_local_datetime(tz, ndt);
        }
    }
    Err(format!("invalid timestamp '{trimmed}'"))
}

pub(crate) fn compute_content_fingerprint(ev: &LedgerEventInput) -> String {
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

pub(crate) fn wrap_preview_row(
    row_no: usize,
    batch_id: &str,
    raw_payload: String,
    parsed: Result<LedgerEventInput, String>,
) -> PreviewRow {
    match parsed {
        Ok(ev) => {
            let fingerprint = compute_content_fingerprint(&ev);
            PreviewRow {
                row_no,
                raw_payload,
                normalized: Some(ev),
                fingerprint,
                validation_state: "valid".to_string(),
                error_reason: None,
            }
        }
        Err(reason) => PreviewRow {
            row_no,
            raw_payload,
            normalized: None,
            fingerprint: format!("ERR:{batch_id}:{row_no}"),
            validation_state: "error".to_string(),
            error_reason: Some(reason),
        },
    }
}

pub(crate) fn source_record_kind(kind: EconomicEventType) -> &'static str {
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

pub(crate) fn source_payload_hash(ev: &LedgerEventInput) -> String {
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

pub(crate) fn verify_csv_headers(
    preset: StatementPreset,
    actual_headers: &csv::StringRecord,
    required: &[&str],
    allowed: &[&str],
) -> Result<HashMap<String, usize>, ImportError> {
    let allowed_set: HashSet<String> = allowed.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut unknown = Vec::new();
    let mut index_map = HashMap::new();

    for (idx, h) in actual_headers.iter().enumerate() {
        let norm = normalize_csv_header(h);
        if !allowed_set.contains(&norm) {
            unknown.push(h.trim().trim_start_matches('\u{feff}').to_string());
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

/// Compute inverse exchange rate for `Sell` cash-to-cash -> `CashExchange` conversion
/// such that `new_qty * inv <= old_qty` is strictly guaranteed (never overdrawing a full cash balance).
fn safe_inverse_exchange_rate(old_qty: Decimal, new_qty: Decimal, price: Decimal) -> Decimal {
    let raw_inv = Decimal::ONE / price;
    let mut inv = raw_inv
        .round_dp_with_strategy(12, rust_decimal::RoundingStrategy::ToZero)
        .normalize();
    if inv.is_zero() {
        inv = raw_inv
            .round_dp_with_strategy(18, rust_decimal::RoundingStrategy::ToZero)
            .normalize();
    }
    if inv.is_zero() {
        inv = raw_inv.normalize();
    }
    if let Some(spent) = new_qty.checked_mul(inv) {
        if spent > old_qty {
            let ulp = Decimal::new(1, inv.scale());
            if inv > ulp {
                inv = (inv - ulp).normalize();
            }
        }
    }
    inv
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

    let existing_batch: Option<String> = store
        .conn()
        .query_row(
            "SELECT id FROM import_batch WHERE account_id = ?1 AND file_hash = ?2 AND status = 'committed' LIMIT 1",
            params![account_id, file_hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(StorageError::from)?;

    let mut existing_fingerprints: HashSet<String> = HashSet::new();
    {
        let mut stmt = store
            .conn()
            .prepare_cached(
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
        ibkr_xml::parse_ibkr_flex_xml(account_id, &batch_id, &raw_content, &reg)?
    } else {
        csv_presets::parse_csv_by_preset(preset, account_id, &batch_id, &raw_content, &reg)?
    };

    let mut valid_count = 0usize;
    let mut error_count = 0usize;
    let mut suspected_duplicate_count = 0usize;
    let mut auto_subs = BTreeSet::new();
    let mut auto_fx = BTreeSet::new();
    let mut seen_source_ids = HashSet::new();
    let mut source_lookup_stmt = store
        .conn()
        .prepare_cached(
            "SELECT payload_hash FROM source_record
             WHERE provider = ?1 AND account_id = ?2 AND record_type = ?3 AND external_id = ?4
             ORDER BY revision DESC LIMIT 1",
        )
        .map_err(StorageError::from)?;

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
            } else if delta_core::is_cash_or_stablecoin_currency(&event.asset_id)
                && delta_core::is_cash_or_stablecoin_currency(&event.quote_currency)
                && !delta_core::normalize_cash_currency_code(&event.asset_id).eq_ignore_ascii_case(
                    &delta_core::normalize_cash_currency_code(&event.quote_currency),
                )
            {
                if event.event_type == EconomicEventType::Buy {
                    event.asset_id = delta_core::normalize_cash_currency_code(&event.asset_id);
                    event.quote_currency =
                        delta_core::normalize_cash_currency_code(&event.quote_currency);
                    event.event_type = EconomicEventType::CashExchange;
                } else if event.event_type == EconomicEventType::Sell {
                    if let Some(price) = event.price.filter(|p| *p > Decimal::ZERO) {
                        let old_qty = event.quantity;
                        let old_asset = delta_core::normalize_cash_currency_code(&event.asset_id);
                        let old_quote =
                            delta_core::normalize_cash_currency_code(&event.quote_currency);
                        let new_qty = (old_qty * price).normalize();
                        event.asset_id = old_quote;
                        event.quote_currency = old_asset;
                        event.quantity = new_qty;
                        event.price = Some(safe_inverse_exchange_rate(old_qty, new_qty, price));
                        event.event_type = EconomicEventType::CashExchange;
                    }
                }
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
                let existing: Option<String> = source_lookup_stmt
                    .query_row(
                        params![
                            preset.as_str(),
                            account_id,
                            source_record_kind(ev.event_type),
                            source_id,
                        ],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(StorageError::from)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use delta_core::{FxRateRecord, QualityStatus};
    use std::path::PathBuf;

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "delta_importers_{prefix}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create isolated temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_data_b02_bom_header_still_rejects_multi_account_csv() {
        let tmp = TestTempDir::new("b02_bom");
        let store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
        let csv_path = tmp.path().join("bom_multi.csv");
        let csv = "\u{feff}account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n\
                   ACC-1,deposit,2025-01-02 10:00:00,UTC,USD,,1000,1,USD,0,USD,S1\n\
                   ACC-2,deposit,2025-01-03 10:00:00,UTC,USD,,1000,1,USD,0,USD,S2\n";
        fs::write(&csv_path, csv).unwrap();
        let err = preview_statement_import(&store, "ACC-1", StatementPreset::GenericCsv, &csv_path)
            .unwrap_err();
        assert!(matches!(err, ImportError::InvalidHeader { .. }));
    }

    #[test]
    fn test_data_b03_timestamp_yyyymmdd_epochs_and_dst_fallback() {
        // 8-digit IBKR tradeDate must parse as 2025-01-15 in ET (21:00Z), NOT 1970-01-01!
        let dt_ibkr = parse_timestamp_with_tz("20250115", "America/New_York").unwrap();
        assert_eq!(
            dt_ibkr.date_naive(),
            NaiveDate::from_ymd_opt(2025, 1, 15).unwrap()
        );

        // Unix seconds, milliseconds, and microseconds.
        let dt_sec = parse_timestamp_with_tz("1700000000", "UTC").unwrap();
        let dt_ms = parse_timestamp_with_tz("1700000000000", "UTC").unwrap();
        let dt_us = parse_timestamp_with_tz("1700000000000000", "UTC").unwrap();
        assert_eq!(dt_sec, dt_ms);
        assert_eq!(dt_ms, dt_us);
        assert!(parse_timestamp_with_tz("12345", "UTC").is_err());

        // DST fall-back ambiguous hour (2024-11-03 01:30 America/New_York) resolves deterministically.
        let dt_dst = parse_timestamp_with_tz("2024-11-03 01:30:00", "America/New_York").unwrap();
        assert_eq!(dt_dst.to_rfc3339(), "2024-11-03T05:30:00+00:00");
    }

    #[test]
    fn test_data_b05_full_balance_cash_sell_does_not_overshoot() {
        let tmp = TestTempDir::new("b05_fx_sell");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
        let dsv = store
            .create_dataset_version("frankfurter", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let d = NaiveDate::from_ymd_opt(2025, 2, 3).unwrap();
        store
            .upsert_fx_rates(
                &[FxRateRecord {
                    id: format!("FX:USD:CNY:{d}:frankfurter:{dsv}"),
                    base_currency: "USD".into(),
                    quote_currency: "CNY".into(),
                    rate: Decimal::from(7),
                    effective_date: d,
                    effective_at: Utc.with_ymd_and_hms(2025, 2, 3, 16, 0, 0).unwrap(),
                    observed_at: Utc::now(),
                    source: "frankfurter".into(),
                    quality: QualityStatus::Exact,
                    carried_from_date: None,
                    dataset_version: dsv,
                }],
                None,
            )
            .unwrap();
        store.publish_dataset_version(dsv).unwrap();

        let csv_path = tmp.path().join("sell_all_usd.csv");
        let csv = "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n\
                   ACC-FX,deposit,2025-02-03 09:00:00,UTC,USD,,1000,1,USD,0,USD,DEP1\n\
                   ACC-FX,sell,2025-02-03 10:00:00,UTC,USD,sell,1000,7,CNY,0,USD,FX1\n";
        fs::write(&csv_path, csv).unwrap();
        let preview =
            preview_statement_import(&store, "ACC-FX", StatementPreset::GenericCsv, &csv_path)
                .unwrap();
        assert_eq!(preview.valid_count, 2);
        let receipt = commit_import_preview(&mut store, &preview).unwrap();
        assert_eq!(receipt.committed_events, 2);
    }

    #[test]
    fn test_data_b10_futu_funding_symbol_and_row_level_errors() {
        let tmp = TestTempDir::new("b10_funding");
        let store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
        let csv_path = tmp.path().join("futu_funding.csv");
        let csv =
            "account,type,time,amount,currency,fee,transactionid,counterpartyaccount,symbol\n\
                   ACC-F,dividend,2025-03-01 09:00:00,50.00,USD,0,DIV1,,US.AAPL\n\
                   ACC-F,bogus_kind,2025-03-02 09:00:00,10.00,USD,0,BAD1,,\n";
        fs::write(&csv_path, csv).unwrap();
        let preview =
            preview_statement_import(&store, "ACC-F", StatementPreset::FutuCsv, &csv_path).unwrap();
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.valid_count, 1);
        assert_eq!(preview.error_count, 1);
        let div_ev = preview.rows[0].normalized.as_ref().unwrap();
        assert_eq!(div_ev.instrument_id.as_deref(), Some("NASDAQ:AAPL"));
        assert_eq!(div_ev.asset_id, "NASDAQ:AAPL");
        assert_eq!(preview.rows[1].validation_state, "error");
    }

    #[test]
    fn test_data_b11_ibkr_positive_tax_refund_rejected_and_dividend_instrument_set() {
        let tmp = TestTempDir::new("b11_ibkr");
        let store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
        let xml_path = tmp.path().join("ibkr_edge.xml");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FlexQueryResponse>
  <FlexStatements count="1">
    <FlexStatement accountId="U1">
      <CashTransactions>
        <CashTransaction transactionID="DIV1" dateTime="2025-04-01 09:00:00" type="Dividends" symbol="AAPL" amount="12.50" currency="usd" />
        <CashTransaction transactionID="TAXREF1" dateTime="2025-04-02 09:00:00" type="Withholding Tax" symbol="AAPL" amount="3.00" currency="USD" />
      </CashTransactions>
    </FlexStatement>
  </FlexStatements>
</FlexQueryResponse>"#;
        fs::write(&xml_path, xml).unwrap();
        let preview =
            preview_statement_import(&store, "ACC-IB", StatementPreset::IbkrFlexXml, &xml_path)
                .unwrap();
        assert_eq!(preview.valid_count, 1);
        assert_eq!(preview.error_count, 1);
        let div_ev = preview.rows[0].normalized.as_ref().unwrap();
        assert_eq!(div_ev.instrument_id.as_deref(), Some("NASDAQ:AAPL"));
        assert_eq!(div_ev.quote_currency, "USD");
        assert!(preview.rows[1]
            .error_reason
            .as_deref()
            .unwrap()
            .contains("refund or rebate"));
    }

    #[test]
    fn test_data_b13_and_b21i_amend_foreign_fee_binds_fx_and_tamper_error() {
        let tmp = TestTempDir::new("b13_b21i");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();
        let dsv = store
            .create_dataset_version("frankfurter", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let d = NaiveDate::from_ymd_opt(2025, 3, 10).unwrap();
        store
            .upsert_fx_rates(
                &[FxRateRecord {
                    id: format!("FX:USD:HKD:{d}:frankfurter:{dsv}"),
                    base_currency: "USD".into(),
                    quote_currency: "HKD".into(),
                    rate: Decimal::from(8),
                    effective_date: d,
                    effective_at: Utc.with_ymd_and_hms(2025, 3, 10, 16, 0, 0).unwrap(),
                    observed_at: Utc::now(),
                    source: "frankfurter".into(),
                    quality: QualityStatus::Exact,
                    carried_from_date: None,
                    dataset_version: dsv,
                }],
                None,
            )
            .unwrap();
        store.publish_dataset_version(dsv).unwrap();

        let csv_path = tmp.path().join("amend_fee.csv");
        let csv = "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n\
                   ACC-A,deposit,2025-03-10 08:00:00,UTC,USD,,10000,1,USD,0,USD,D1\n\
                   ACC-A,deposit,2025-03-10 08:01:00,UTC,HKD,,1000,1,HKD,0,HKD,D2\n\
                   ACC-A,buy,2025-03-10 10:00:00,UTC,NASDAQ:AAPL,buy,10,150,USD,0,HKD,B1\n";
        fs::write(&csv_path, csv).unwrap();
        let preview =
            preview_statement_import(&store, "ACC-A", StatementPreset::GenericCsv, &csv_path)
                .unwrap();

        // DATA-B21i: tampering the file after preview must return FileTamperedAfterPreview (not StalePreview).
        fs::write(&csv_path, format!("{csv}# tampered\n")).unwrap();
        let tamper_err = commit_import_preview(&mut store, &preview).unwrap_err();
        assert!(matches!(
            tamper_err,
            ImportError::FileTamperedAfterPreview { .. }
        ));

        // Restore file and commit cleanly.
        fs::write(&csv_path, csv).unwrap();
        commit_import_preview(&mut store, &preview).unwrap();
        let buy_id = preview.rows[2]
            .normalized
            .as_ref()
            .unwrap()
            .event_id
            .clone();

        // DATA-B13: amending 0 HKD fee -> 16 HKD fee must bind HKD/USD FX on the replacement event!
        amend_committed_event(
            &mut store,
            &buy_id,
            Decimal::from(10),
            Some(Decimal::from(150)),
            Decimal::from(16),
        )
        .unwrap();
        let bindings = load_event_book_fx(&store).unwrap();
        let rep_id = format!("{buy_id}:REV:2");
        assert!(
            bindings.contains_key(&(rep_id, "HKD".to_string())),
            "replacement event must freeze newly needed HKD FX binding"
        );
    }
}
