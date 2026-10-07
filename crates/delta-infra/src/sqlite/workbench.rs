//! Desktop application queries and durable configuration. No rendering or
//! transport work here; all quantities remain decimal strings at the boundary.
use super::app::{CsvPreview, MarketBar};
use super::store::{canonical_ts, Library};
use crate::error::{InfraError, InfraResult};
use chrono::{DateTime, Utc};
use delta_app::ai::runtime::ModelConnectionConfig;
use delta_core::money::CurrencyCode;
use delta_core::{AccountId, ScopeSnapshot, ScopeView};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopSettings {
    pub connection: Option<ModelConnectionConfig>,
    pub connection_enabled: bool,
    /// Empty is deny-all; a new account never inherits permission.
    pub allowed_accounts: Vec<String>,
    pub selected_accounts: Vec<String>,
    pub start_at: String,
    pub end_at: String,
    pub chart: Option<ChartContext>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartContext {
    pub instrument: String,
    pub start_at: String,
    pub end_at: String,
    pub adjustment: String,
    pub dataset_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalDraft {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub account_id: String,
    pub revision: i64,
    pub chart: Option<ChartContext>,
}

#[derive(Debug, Clone)]
pub struct AccountView {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct PortfolioView {
    pub id: String,
    pub name: String,
    pub accounts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub id: String,
    pub account: String,
    pub at: String,
    pub display_at: String,
    pub detail: String,
    pub instrument: Option<String>,
    pub quantity: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChartWindow {
    pub context: ChartContext,
    pub source: String,
    pub bars: Vec<MarketBar>,
    pub total: usize,
    pub warmup: usize,
    pub markers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentLibrary {
    pub path: PathBuf,
}

pub fn read_recent(path: &Path) -> InfraResult<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let config: RecentLibrary = serde_json::from_slice(&std::fs::read(path)?)?;
    Ok(Some(config.path))
}

pub fn write_recent(path: &Path, library: &Path) -> InfraResult<()> {
    let value = RecentLibrary {
        path: library.canonicalize()?,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> InfraResult<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&value)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Header names in canonical order; input values are never parsed as floats.
pub const CSV_FIELDS: [&str; 9] = [
    "occurred_at",
    "type",
    "asset",
    "quantity",
    "price",
    "fee",
    "source_ref",
    "instrument",
    "quote_currency",
];

pub fn mapped_csv_identity(bytes: &[u8], columns: &[String], delimiter: u8) -> InfraResult<String> {
    Ok(format!(
        "r2:{:x}",
        Sha256::digest(
            [
                bytes,
                serde_json::to_string(&(columns, delimiter))?.as_bytes()
            ]
            .concat()
        )
    ))
}

fn encode_versioned<T: Serialize>(value: &T) -> InfraResult<String> {
    Ok(serde_json::to_string(
        &serde_json::json!({"schema_version":1,"value":value}),
    )?)
}
fn decode_versioned<T: serde::de::DeserializeOwned>(raw: &str) -> InfraResult<T> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    if value.get("schema_version").is_none() {
        // Transitional local R2 databases produced before the envelope existed.
        return Ok(serde_json::from_value(value)?);
    }
    if value["schema_version"] != 1 {
        return Err(InfraError::Rejected(
            "unsupported desktop data version".into(),
        ));
    }
    Ok(serde_json::from_value(value["value"].clone())?)
}

impl Library {
    pub fn desktop_settings(&self) -> InfraResult<DesktopSettings> {
        let raw: Option<String> = self.with(|c| {
            c.query_row(
                "SELECT value FROM desktop_settings WHERE id='workspace'",
                [],
                |r| r.get(0),
            )
            .optional()
        })?;
        raw.map(|s| decode_versioned(&s))
            .unwrap_or_else(|| Ok(DesktopSettings::default()))
    }

    pub fn save_desktop_settings(&self, settings: &DesktopSettings) -> InfraResult<()> {
        self.apply_desktop_settings(settings, &self.reporting_currency()?, &self.timezone()?)
    }

    pub fn apply_desktop_settings(
        &self,
        settings: &DesktopSettings,
        currency: &str,
        timezone: &str,
    ) -> InfraResult<()> {
        if currency.trim().len() < 3
            || !currency.bytes().all(|b| b.is_ascii_alphanumeric())
            || timezone.parse::<chrono_tz::Tz>().is_err()
        {
            return Err(InfraError::Rejected(
                "valid currency and IANA timezone required".into(),
            ));
        }
        if let Some(connection) = &settings.connection {
            crate::model::validate_saved_connection(connection)?;
        }
        let accounts = self.accounts()?;
        for id in settings
            .allowed_accounts
            .iter()
            .chain(&settings.selected_accounts)
        {
            if !accounts.iter().any(|a| &a.id == id) {
                return Err(InfraError::Rejected("unknown account in settings".into()));
            }
        }
        if !settings.start_at.is_empty() || !settings.end_at.is_empty() {
            valid_period(&settings.start_at, &settings.end_at)?;
        }
        if let Some(context) = &settings.chart {
            valid_period(&context.start_at, &context.end_at)?;
        }
        let json = encode_versioned(settings)?;
        self.with(|c| -> InfraResult<()> {
            let tx=c.unchecked_transaction()?;
            tx.execute("INSERT INTO desktop_settings(id,value) VALUES('workspace',?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value", params![json])?;
            tx.execute("UPDATE library_meta SET reporting_currency=?1,timezone=?2",params![currency.to_uppercase(),timezone])?;
            tx.commit()?;Ok(())
        })?;
        Ok(())
    }

    pub fn accounts(&self) -> InfraResult<Vec<AccountView>> {
        self.with(|c| {
            let mut statement = c.prepare("SELECT id,name,type FROM account ORDER BY name,id")?;
            let rows = statement.query_map([], |r| {
                Ok(AccountView {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    pub fn create_account(&self, name: &str, kind: &str) -> InfraResult<String> {
        if name.trim().is_empty()
            || !matches!(kind, "bank" | "brokerage" | "crypto" | "wallet" | "manual")
        {
            return Err(InfraError::Rejected(
                "account name and valid type required".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.ensure_account(&id, name.trim(), kind)?;
        Ok(id)
    }

    pub fn portfolios(&self) -> InfraResult<Vec<PortfolioView>> {
        self.with(|c| -> InfraResult<_> {
            let mut stmt=c.prepare("SELECT id,name FROM portfolio ORDER BY name,id")?;
            let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
            let mut out=Vec::new();
            for (id,name) in rows {
                let mut members=c.prepare("SELECT DISTINCT account_id FROM portfolio_member WHERE portfolio_id=?1 AND valid_to IS NULL ORDER BY account_id")?;
                let accounts=members.query_map(params![id],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
                out.push(PortfolioView{id,name,accounts});
            }
            Ok(out)
        })
    }

    pub fn save_portfolio(&self, name: &str, ids: &[String]) -> InfraResult<String> {
        let accounts = self.accounts()?;
        let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
        if name.trim().is_empty()
            || unique.is_empty()
            || unique
                .iter()
                .any(|id| !accounts.iter().any(|a| &a.id == *id))
        {
            return Err(InfraError::Rejected(
                "portfolio name and existing accounts required".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| -> InfraResult<()> {
            let tx=c.unchecked_transaction()?;
            tx.execute("INSERT INTO portfolio(id,name) VALUES(?1,?2)",params![id,name.trim()])?;
            for account in unique {tx.execute("INSERT INTO portfolio_member(portfolio_id,account_id,valid_from) VALUES(?1,?2,?3)",params![id,account,canonical_ts(Utc::now())])?;}
            tx.commit()?;Ok(())
        })?;
        Ok(id)
    }

    pub fn desktop_scope(
        &self,
        ids: &[String],
        start: &str,
        end: &str,
    ) -> InfraResult<ScopeSnapshot> {
        let (start, end) = valid_period(start, end)?;
        let all = self.accounts()?;
        if ids.is_empty() || ids.iter().any(|id| !all.iter().any(|a| &a.id == id)) {
            return Err(InfraError::Rejected(
                "select at least one existing account".into(),
            ));
        }
        Ok(ScopeSnapshot::freeze(
            uuid::Uuid::new_v4().to_string(),
            ids.iter().map(AccountId::new).collect(),
            start,
            end,
            CurrencyCode::new(self.reporting_currency()?),
            if ids.len() == 1 {
                ScopeView::SingleAccount
            } else {
                ScopeView::Portfolio
            },
            self.ledger_revision()?,
        ))
    }

    pub fn ledger_rows(&self, scope: &ScopeSnapshot) -> InfraResult<Vec<LedgerRow>> {
        let ids = scope
            .account_ids
            .iter()
            .map(|a| a.0.clone())
            .collect::<Vec<_>>();
        let timezone = self
            .timezone()?
            .parse::<chrono_tz::Tz>()
            .map_err(|_| InfraError::Rejected("invalid display timezone".into()))?;
        let events = self.recorded_events(Some(&ids))?;
        let mut effective = delta_core::pnl::effective(&events);
        effective.sort_by_key(|e| std::cmp::Reverse((e.occurred_at, e.seq)));
        Ok(effective
            .into_iter()
            .filter(|e| e.occurred_at >= scope.start_at && e.occurred_at < scope.end_at)
            .map(|e| {
                let (instrument, quantity) = match &e.payload {
                    delta_core::EventPayload::Buy {
                        instrument,
                        quantity,
                        ..
                    }
                    | delta_core::EventPayload::Sell {
                        instrument,
                        quantity,
                        ..
                    } => (
                        Some(instrument.0.clone()),
                        Some(quantity.normalize().to_string()),
                    ),
                    _ => (None, None),
                };
                LedgerRow {
                    id: e.id.0.clone(),
                    account: e.account_id.0.clone(),
                    at: canonical_ts(e.occurred_at),
                    display_at: e.occurred_at.with_timezone(&timezone).to_rfc3339(),
                    detail: serde_json::to_string(&e.payload).unwrap_or_default(),
                    instrument,
                    quantity,
                }
            })
            .collect())
    }

    /// Map a user-selected UTF-8 CSV by explicit headers. The mapping identity
    /// includes the original file hash, delimiter and columns, so byte changes
    /// cannot reuse a preview even when the normalized events are identical.
    pub fn preview_mapped_csv(
        &self,
        account: &str,
        bytes: &[u8],
        columns: &[String],
        delimiter: u8,
    ) -> InfraResult<CsvPreview> {
        if bytes.len() > 32 * 1024 * 1024 || columns.len() != 9 || !b",;\t".contains(&delimiter) {
            return Err(InfraError::Rejected(
                "CSV limit is 32 MiB; select nine header mappings and a supported separator".into(),
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| {
                InfraError::Rejected("CSV must be UTF-8; convert encoding explicitly".into())
            })?
            .trim_start_matches('\u{feff}');
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .from_reader(text.as_bytes());
        let headers = reader.headers()?.clone();
        let indexes = columns
            .iter()
            .map(|name| {
                let positions = headers
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| *h == name)
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                if positions.len() != 1 {
                    Err(InfraError::Rejected(format!(
                        "missing or ambiguous header: {name}"
                    )))
                } else {
                    Ok(positions[0])
                }
            })
            .collect::<InfraResult<Vec<_>>>()?;
        let mut writer = csv::Writer::from_writer(Vec::new());
        writer.write_record(CSV_FIELDS)?;
        for record in reader.records() {
            let record = record?;
            writer.write_record(indexes.iter().map(|i| record.get(*i).unwrap_or("")))?;
        }
        let normalized = String::from_utf8(
            writer
                .into_inner()
                .map_err(|e| InfraError::Rejected(e.to_string()))?,
        )
        .map_err(|e| InfraError::Rejected(e.to_string()))?;
        let preview = self.preview_csv(
            account,
            &normalized,
            &mapped_csv_identity(bytes, columns, delimiter)?,
        )?;
        // Keep original headers and row values, not the rearranged CSV, for review.
        let mut original = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .from_reader(text.as_bytes());
        let headers = original.headers()?.clone();
        let raw = original.records().map(|r| r.map(|r| serde_json::json!({"headers":headers.iter().collect::<Vec<_>>(),"values":r.iter().collect::<Vec<_>>()}).to_string())).collect::<Result<Vec<_>,_>>()?;
        self.with(|c| -> InfraResult<()> {
            let tx = c.unchecked_transaction()?;
            for (index, row) in raw.iter().enumerate() {
                tx.execute(
                    "UPDATE import_row SET raw_payload=?1 WHERE batch_id=?2 AND row_no=?3",
                    params![row, preview.batch_id, index as i64 + 1],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        Ok(preview)
    }

    pub fn verify_mapped_preview(
        &self,
        preview: &CsvPreview,
        account: &str,
        bytes: &[u8],
        columns: &[String],
        delimiter: u8,
    ) -> InfraResult<()> {
        let original_account: String = self.with(|c| {
            c.query_row(
                "SELECT account_id FROM import_batch WHERE id=?1",
                params![preview.batch_id],
                |r| r.get(0),
            )
        })?;
        if original_account != account
            || mapped_csv_identity(bytes, columns, delimiter)? != preview.mapping_version
        {
            return Err(InfraError::Rejected(
                "CSV file, mapping or account changed; preview again before committing".into(),
            ));
        }
        Ok(())
    }

    pub fn preview_details(&self, batch: &str) -> InfraResult<Vec<(i64, String, String)>> {
        self.with(|c| {
            let mut s=c.prepare("SELECT row_no,raw_payload,COALESCE(normalized_payload,'') FROM import_row WHERE batch_id=?1 ORDER BY row_no")?;
            let rows=s.query_map(params![batch],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
        })
    }

    /// Pin one immutable file dataset. Long series load at most 20k daily bars
    /// per requested date window, with an explicit total for the caller.
    pub fn chart_window(&self, context: &ChartContext) -> InfraResult<ChartWindow> {
        let (start, end) = valid_period(&context.start_at, &context.end_at)?;
        let mut context = context.clone();
        let dataset: Option<(String,String)>=self.with(|c| {
            c.query_row("SELECT d.id,d.source FROM dataset_version d WHERE (?1 IS NULL OR d.id=?1) AND EXISTS(SELECT 1 FROM bar b WHERE b.dataset_version=d.id AND b.instrument_id=?2 AND b.adjustment=?3) ORDER BY d.acquired_at DESC,d.rowid DESC LIMIT 1",params![context.dataset_version,context.instrument,context.adjustment],|r|Ok((r.get(0)?,r.get(1)?))).optional()
        })?;
        let Some((version, source)) = dataset else {
            return Ok(ChartWindow {
                context,
                source: String::new(),
                bars: vec![],
                total: 0,
                warmup: 0,
                markers: vec![],
            });
        };
        context.dataset_version = Some(version.clone());
        let (bars,total)=self.with(|c| -> InfraResult<_> {
            let total:i64=c.query_row("SELECT count(*) FROM bar WHERE instrument_id=?1 AND dataset_version=?2 AND adjustment=?3 AND open_at>=?4 AND close_at<=?5",params![context.instrument,version,context.adjustment,canonical_ts(start),canonical_ts(end)],|r|r.get(0))?;
            let mut s=c.prepare("SELECT session_date,open_at,close_at,open,high,low,close,volume FROM bar WHERE instrument_id=?1 AND dataset_version=?2 AND adjustment=?3 AND open_at>=?4 AND close_at<=?5 ORDER BY close_at LIMIT 20000")?;
            let rows=s.query_map(params![context.instrument,version,context.adjustment,canonical_ts(start),canonical_ts(end)],|r|Ok(MarketBar{instrument:context.instrument.clone(),session_date:r.get(0)?,open_at:r.get(1)?,close_at:r.get(2)?,open:r.get(3)?,high:r.get(4)?,low:r.get(5)?,close:r.get(6)?,volume:r.get(7)?,adjustment:context.adjustment.clone()}))?;
            Ok((rows.collect::<Result<Vec<_>,_>>()?,total as usize))
        })?;
        let prior=self.with(|c| -> InfraResult<Vec<MarketBar>> {
            let mut statement=c.prepare("SELECT session_date,open_at,close_at,open,high,low,close,volume FROM bar WHERE instrument_id=?1 AND dataset_version=?2 AND adjustment=?3 AND open_at<?4 AND close_at<=?4 ORDER BY close_at DESC LIMIT 19")?;
            let rows=statement.query_map(params![context.instrument,version,context.adjustment,canonical_ts(start)],|r|Ok(MarketBar{instrument:context.instrument.clone(),session_date:r.get(0)?,open_at:r.get(1)?,close_at:r.get(2)?,open:r.get(3)?,high:r.get(4)?,low:r.get(5)?,close:r.get(6)?,volume:r.get(7)?,adjustment:context.adjustment.clone()}))?;
            Ok(rows.collect::<Result<Vec<_>,_>>()?)
        })?;
        let warmup = if bars.is_empty() { 0 } else { prior.len() };
        let bars = if bars.is_empty() {
            bars
        } else {
            prior.into_iter().rev().chain(bars).collect()
        };
        // Markers carry evidence IDs. The UI can open the exact cited revision.
        let mut markers = Vec::new();
        for e in self.recorded_events(None)? {
            if e.occurred_at < start || e.occurred_at >= end {
                continue;
            }
            let matches = matches!(&e.payload,delta_core::EventPayload::Buy{instrument,..}|delta_core::EventPayload::Sell{instrument,..} if instrument.0==context.instrument);
            if matches {
                markers.push((
                    e.occurred_at.date_naive().to_string(),
                    format!("event:{}", e.id.0),
                ));
            }
        }
        for (id, _, _) in self.search_journal("", 1000, None)? {
            let draft = self.journal_draft(&id, None)?;
            if let Some(chart) = draft.chart {
                if chart.instrument == context.instrument {
                    markers.push((
                        chart.start_at[..10].to_string(),
                        format!("journal:{id}:r{}", draft.revision),
                    ));
                }
            }
        }
        Ok(ChartWindow {
            context,
            source,
            bars,
            total,
            warmup,
            markers,
        })
    }

    pub fn journal_draft(&self, id: &str, revision: Option<i64>) -> InfraResult<JournalDraft> {
        self.with(|c| {
            let (title,body,account_id,revision,fields):(String,String,String,i64,Option<String>)=c.query_row("SELECT j.title,r.body,COALESCE(j.account_id,''),r.revision,r.structured_fields FROM journal_entry j JOIN journal_revision r ON r.journal_id=j.id AND r.revision=COALESCE(?2,j.current_revision) WHERE j.id=?1",params![id,revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            let chart=fields.map(|s|decode_versioned(&s)).transpose()?;
            Ok(JournalDraft{id:Some(id.into()),title,body,account_id,revision,chart})
        })
    }

    pub fn save_journal_draft(&self, draft: &JournalDraft) -> InfraResult<JournalDraft> {
        if draft.title.trim().is_empty()
            || !self.accounts()?.iter().any(|a| a.id == draft.account_id)
        {
            return Err(InfraError::Rejected(
                "title and existing note account required".into(),
            ));
        }
        if let Some(context) = &draft.chart {
            valid_period(&context.start_at, &context.end_at)?;
        }
        let fields = draft.chart.as_ref().map(encode_versioned).transpose()?;
        let id = self.save_journal_fields(
            &draft.title,
            &draft.body,
            &[],
            draft.id.as_deref(),
            Some(&draft.account_id),
            fields.as_deref(),
            Some(draft.revision),
        )?;
        self.journal_draft(&id, None)
    }

    pub fn sessions(&self) -> InfraResult<Vec<(String, String)>> {
        self.with(|c| {
            let mut s=c.prepare("SELECT s.id,COALESCE((SELECT state FROM ai_run r WHERE r.session_id=s.id ORDER BY generation DESC LIMIT 1),'empty') FROM ai_session s WHERE library_id=?1 ORDER BY created_at DESC")?;
            let rows=s.query_map(params![self.id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
        })
    }
}

pub fn valid_period(start: &str, end: &str) -> InfraResult<(DateTime<Utc>, DateTime<Utc>)> {
    let parse = |raw: &str| {
        DateTime::parse_from_rfc3339(raw)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| InfraError::Rejected("dates must be RFC3339 with timezone".into()))
    };
    let start = parse(start)?;
    let end = parse(end)?;
    if start >= end {
        return Err(InfraError::Rejected("end must be after start".into()));
    }
    Ok((start, end))
}
