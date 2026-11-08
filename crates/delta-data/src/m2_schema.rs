#![allow(clippy::too_many_arguments)]
use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use chrono::{NaiveDate, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text, AdjustmentKind, ManualValuation};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::{sha256_file, LibraryManifest, SqliteStore, StorageError};

thread_local! {
    static FAIL_NEXT_SCHEMA_4_MIGRATION: Cell<bool> = const { Cell::new(false) };
    static FAIL_NEXT_NOTE_SAVE: Cell<bool> = const { Cell::new(false) };
    static FAIL_NEXT_AI_COMPACTION: Cell<bool> = const { Cell::new(false) };
}

pub fn set_migration_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.set(fail));
}

pub fn set_note_save_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_NOTE_SAVE.with(|flag| flag.set(fail));
}

pub fn set_ai_compaction_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_AI_COMPACTION.with(|flag| flag.set(fail));
}

pub const SCHEMA_4_TABLES: &[&str] = &[
    "portfolio_group_link",
    "manual_valuation",
    "watchlist_group",
    "watchlist_item",
    "trade_plan",
    "plan_fill_link",
    "chart_context",
    "note",
    "note_revision",
    "note_link",
    "attachment",
    "tag",
    "note_tag",
    "note_fts",
    "model_connection",
    "ai_session",
    "ai_run",
    "ai_message",
    "ai_tool_call",
    "ai_checkpoint",
];

/// Apply all Schema 4 DDL inside the caller's migration savepoint (`M2-W01`, `M2-A-02`, `S-17`, `S-21`).
pub fn apply_schema_4_ddl(tx: &Connection, from_schema: i64) -> Result<(), StorageError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS portfolio_group_link (
            parent_portfolio_id TEXT NOT NULL,
            child_portfolio_id TEXT NOT NULL,
            PRIMARY KEY (parent_portfolio_id, child_portfolio_id)
        );

        CREATE TABLE IF NOT EXISTS manual_valuation (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            asset_or_liability_id TEXT NOT NULL,
            is_liability INTEGER NOT NULL DEFAULT 0,
            value TEXT NOT NULL,
            currency TEXT NOT NULL,
            as_of TEXT NOT NULL,
            method TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_manual_valuation_acct_date
            ON manual_valuation(account_id, as_of);

        CREATE TABLE IF NOT EXISTS watchlist_group (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS watchlist_item (
            group_id TEXT NOT NULL,
            instrument_id TEXT NOT NULL,
            added_at TEXT NOT NULL,
            PRIMARY KEY (group_id, instrument_id)
        );

        CREATE TABLE IF NOT EXISTS trade_plan (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            instrument_id TEXT,
            thesis TEXT NOT NULL,
            entry_rule TEXT NOT NULL,
            invalidation TEXT NOT NULL,
            risk_budget TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS plan_fill_link (
            plan_id TEXT NOT NULL,
            event_id TEXT NOT NULL,
            allocated_quantity TEXT NOT NULL,
            PRIMARY KEY (plan_id, event_id)
        );

        CREATE TABLE IF NOT EXISTS chart_context (
            id TEXT PRIMARY KEY,
            instrument_id TEXT NOT NULL,
            venue TEXT NOT NULL,
            timeframe TEXT NOT NULL,
            visible_start_date TEXT NOT NULL,
            visible_end_date TEXT NOT NULL,
            anchor_date TEXT,
            adjustment TEXT NOT NULL,
            indicators_json TEXT NOT NULL,
            dataset_version INTEGER,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS note (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            current_revision INTEGER NOT NULL DEFAULT 1,
            allow_model_access INTEGER NOT NULL DEFAULT 1,
            template_kind TEXT NOT NULL DEFAULT 'freeform',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS note_revision (
            id TEXT PRIMARY KEY,
            note_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            body_markdown TEXT NOT NULL,
            structured_json TEXT NOT NULL,
            saved_at TEXT NOT NULL,
            UNIQUE(note_id, revision)
        );

        CREATE TABLE IF NOT EXISTS note_link (
            id TEXT PRIMARY KEY,
            note_id TEXT NOT NULL,
            target_type TEXT NOT NULL,
            target_id TEXT NOT NULL,
            target_revision INTEGER NOT NULL DEFAULT 1,
            allocated_quantity TEXT,
            relation TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_note_link_target
            ON note_link(target_type, target_id);

        CREATE TABLE IF NOT EXISTS attachment (
            id TEXT PRIMARY KEY,
            note_id TEXT NOT NULL,
            sha256_hash TEXT NOT NULL,
            file_name TEXT NOT NULL,
            mime_type TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            relative_path TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS tag (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS note_tag (
            note_id TEXT NOT NULL,
            tag_id TEXT NOT NULL,
            PRIMARY KEY (note_id, tag_id)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(
            note_id UNINDEXED,
            title,
            body_markdown,
            tags_text,
            tokenize='trigram'
        );

        CREATE TABLE IF NOT EXISTS model_connection (
            id TEXT PRIMARY KEY,
            protocol TEXT NOT NULL,
            base_url TEXT NOT NULL,
            model_id TEXT NOT NULL,
            credential_ref TEXT,
            context_window INTEGER NOT NULL,
            capabilities_json TEXT NOT NULL,
            timeouts_json TEXT NOT NULL,
            budget_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ai_session (
            id TEXT PRIMARY KEY,
            library_id TEXT NOT NULL,
            schema_version INTEGER NOT NULL DEFAULT 1,
            active_checkpoint_id TEXT,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ai_run (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            scope_json TEXT NOT NULL,
            dataset_version INTEGER NOT NULL,
            ledger_revision INTEGER NOT NULL,
            tool_schema_hash TEXT NOT NULL,
            model_connection_id TEXT NOT NULL,
            budget_json TEXT NOT NULL,
            state TEXT NOT NULL,
            checkpoint_ref TEXT,
            report_json TEXT,
            usage_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ai_message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            turn_id INTEGER NOT NULL,
            sequence INTEGER NOT NULL,
            kind TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ai_message_session_seq
            ON ai_message(session_id, sequence);

        CREATE TABLE IF NOT EXISTS ai_tool_call (
            id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            call_id TEXT NOT NULL,
            args_hash TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            args_json TEXT NOT NULL,
            result_json TEXT,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(run_id, call_id)
        );

        CREATE TABLE IF NOT EXISTS ai_checkpoint (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            source_start_seq INTEGER NOT NULL,
            source_end_seq INTEGER NOT NULL,
            source_hash TEXT NOT NULL,
            summary_text TEXT NOT NULL,
            model_ref TEXT NOT NULL,
            template_version TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )?;

    if from_schema < 4 && FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.replace(false)) {
        return Err(StorageError::MigrationFailed(
            "injected mid-migration fault before commit".into(),
        ));
    }

    Ok(())
}

// ============================================================================
// Accounts, Portfolios & Manual Valuations DTOs (`M2-W02`, `M2-A-04`, `M2-A-05`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: String,
    pub name: String,
    pub account_type: String,
    pub provider: String,
    pub external_ref_masked: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioRecord {
    pub id: String,
    pub name: String,
    pub member_accounts: Vec<String>,
    pub child_portfolios: Vec<String>,
    pub created_at: String,
}

pub fn mask_external_account_ref(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        Some(format!("***{trimmed}"))
    } else {
        let last4: String = chars[chars.len() - 4..].iter().collect();
        Some(format!("***{last4}"))
    }
}

// ============================================================================
// Watchlist & Instrument Search DTOs (`M2-W05`, `M2-A-10`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentSearchHit {
    pub instrument_id: String,
    pub venue: String,
    pub symbol: String,
    pub kind: String,
    pub base_asset_id: String,
    pub quote_asset_id: String,
    pub matched_alias: Option<(String, String)>,
    pub venue_vs_pair_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchlistGroupDto {
    pub id: String,
    pub name: String,
    pub instruments: Vec<String>,
}

// ============================================================================
// Trade Plans, Chart Contexts, Notes, Autosave & Attachments (`M2-W06`, `M2-A-11`, `M2-A-12`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteTemplateKind {
    Freeform,
    PreTrade,
    DuringTrade,
    PostTrade,
}

impl NoteTemplateKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Freeform => "freeform",
            Self::PreTrade => "pre_trade",
            Self::DuringTrade => "during_trade",
            Self::PostTrade => "post_trade",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pre_trade" | "pretrade" => Self::PreTrade,
            "during_trade" | "duringtrade" => Self::DuringTrade,
            "post_trade" | "posttrade" => Self::PostTrade,
            _ => Self::Freeform,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredNoteFields {
    #[serde(default = "default_structured_schema_version")]
    pub schema_version: u32,
    pub thesis: String,
    pub invalidation: String,
    pub planned_risk: String,
    pub actual_execution: String,
    pub emotion: String,
    pub conclusion: String,
}

fn default_structured_schema_version() -> u32 {
    1
}

impl Default for StructuredNoteFields {
    fn default() -> Self {
        Self {
            schema_version: 1,
            thesis: String::new(),
            invalidation: String::new(),
            planned_risk: String::new(),
            actual_execution: String::new(),
            emotion: String::new(),
            conclusion: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutosaveStatus {
    Editing,
    Saving,
    Saved { saved_at: String, revision: i64 },
    SaveFailed { error: String, retryable: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteLinkDto {
    pub id: String,
    pub note_id: String,
    pub target_type: String,
    pub target_id: String,
    pub target_revision: i64,
    pub current_target_revision: i64,
    pub is_target_amended: bool,
    pub allocated_quantity: Option<Decimal>,
    pub relation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartContextDto {
    pub id: String,
    pub instrument_id: String,
    pub venue: String,
    pub timeframe: String,
    pub visible_start_date: NaiveDate,
    pub visible_end_date: NaiveDate,
    pub anchor_date: Option<NaiveDate>,
    pub adjustment: AdjustmentKind,
    pub indicators_json: String,
    pub dataset_version: Option<i64>,
    pub latest_dataset_version: i64,
    pub has_newer_dataset_version: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDto {
    pub id: String,
    pub note_id: String,
    pub sha256_hash: String,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub relative_path: String,
    pub exists_on_disk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteDto {
    pub id: String,
    pub title: String,
    pub current_revision: i64,
    pub allow_model_access: bool,
    pub template_kind: String,
    pub body_markdown: String,
    pub structured: StructuredNoteFields,
    pub tags: Vec<String>,
    pub links: Vec<NoteLinkDto>,
    pub attachments: Vec<AttachmentDto>,
    pub saved_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradePlanDto {
    pub id: String,
    pub title: String,
    pub instrument_id: Option<String>,
    pub thesis: String,
    pub entry_rule: String,
    pub invalidation: String,
    pub risk_budget: String,
    pub status: String,
    pub linked_fills: Vec<(String, Decimal)>,
}

/// Honest autosave state machine (`S-18`, `M2-A-12`):
/// Never reports `Saved` unless the SQLite revision write commits; retains `body_buffer` on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEditorSession {
    pub note_id: String,
    pub title_buffer: String,
    pub template_kind: String,
    pub body_buffer: String,
    pub structured_buffer: StructuredNoteFields,
    pub tags_buffer: Vec<String>,
    pub allow_model_access: bool,
    pub status: AutosaveStatus,
    pub last_persisted_revision: Option<i64>,
}

impl NoteEditorSession {
    pub fn new(
        note_id: impl Into<String>,
        title: impl Into<String>,
        template_kind: NoteTemplateKind,
        allow_model_access: bool,
    ) -> Self {
        Self {
            note_id: note_id.into(),
            title_buffer: title.into(),
            template_kind: template_kind.as_str().to_string(),
            body_buffer: String::new(),
            structured_buffer: StructuredNoteFields::default(),
            tags_buffer: Vec::new(),
            allow_model_access,
            status: AutosaveStatus::Editing,
            last_persisted_revision: None,
        }
    }

    pub fn begin_edit(&mut self, new_body: String) {
        self.body_buffer = new_body;
        self.status = AutosaveStatus::Editing;
    }

    pub fn flush_autosave(&mut self, store: &mut SqliteStore) -> Result<i64, StorageError> {
        self.status = AutosaveStatus::Saving;
        match store.save_note_revision(
            &self.note_id,
            &self.title_buffer,
            &self.template_kind,
            self.allow_model_access,
            &self.body_buffer,
            &self.structured_buffer,
            &self.tags_buffer,
        ) {
            Ok(rev) => {
                let saved_at = Utc::now().to_rfc3339();
                self.last_persisted_revision = Some(rev);
                self.status = AutosaveStatus::Saved {
                    saved_at,
                    revision: rev,
                };
                Ok(rev)
            }
            Err(err) => {
                self.status = AutosaveStatus::SaveFailed {
                    error: err.to_string(),
                    retryable: true,
                };
                Err(err)
            }
        }
    }
}

// ============================================================================
// AI Session, Run, Message, ToolCall & Checkpoint Records (`M2-W07`, `S-21`, `M2-A-15`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConnectionRecord {
    pub id: String,
    pub protocol: String,
    pub base_url: String,
    pub model_id: String,
    pub credential_ref: Option<String>,
    pub context_window: i64,
    pub capabilities_json: String,
    pub timeouts_json: String,
    pub budget_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiSessionRecord {
    pub id: String,
    pub library_id: String,
    pub schema_version: i64,
    pub active_checkpoint_id: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiRunRecord {
    pub id: String,
    pub session_id: String,
    pub generation: i64,
    pub scope_json: String,
    pub dataset_version: i64,
    pub ledger_revision: i64,
    pub tool_schema_hash: String,
    pub model_connection_id: String,
    pub budget_json: String,
    pub state: String,
    pub checkpoint_ref: Option<String>,
    pub report_json: Option<String>,
    pub usage_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiMessageRecord {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_id: i64,
    pub sequence: i64,
    pub kind: String,
    pub payload_json: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiToolCallRecord {
    pub id: String,
    pub run_id: String,
    pub call_id: String,
    pub args_hash: String,
    pub tool_name: String,
    pub args_json: String,
    pub result_json: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiCheckpointRecord {
    pub id: String,
    pub session_id: String,
    pub source_start_seq: i64,
    pub source_end_seq: i64,
    pub source_hash: String,
    pub summary_text: String,
    pub model_ref: String,
    pub template_version: String,
    pub created_at: String,
}

// ============================================================================
// SqliteStore M2 Implementations
// ============================================================================

impl SqliteStore {
    /// Create a genuine Schema 3 library on disk (dropping Schema 4 tables and setting `schema_version = 3`)
    /// so `M2-A-02` can exercise the 3 -> 4 migration, pre-migration backup, and fault injection.
    pub fn create_schema_3_library_for_test(
        root_dir: &Path,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<(), StorageError> {
        let prev_fault = FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.replace(false));
        let result = (|| -> Result<(), StorageError> {
            let mut store = Self::open_or_init(root_dir, book_currency, report_currency, timezone)?;
            let tx = store.conn_mut().savepoint()?;
            for table in SCHEMA_4_TABLES {
                tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            }
            tx.execute("UPDATE library SET schema_version = 3", [])?;
            tx.commit()?;
            store.sync_manifest()?;
            Ok(())
        })();
        FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.set(prev_fault));
        result
    }

    /// Open `delta.sqlite` read-only without running migrations and read `schema_version` on disk (`M2-A-02`).
    pub fn verify_schema_version_on_disk(root_dir: &Path) -> Result<i64, StorageError> {
        let db_path = root_dir.join("delta.sqlite");
        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let ver: i64 = conn.query_row("SELECT schema_version FROM library LIMIT 1", [], |r| {
            r.get(0)
        })?;
        Ok(ver)
    }

    /// List which Schema 4 tables are currently present in `sqlite_master` on disk (`M2-A-02`).
    pub fn list_schema_4_tables_on_disk(root_dir: &Path) -> Result<Vec<String>, StorageError> {
        let db_path = root_dir.join("delta.sqlite");
        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut present = Vec::new();
        for table in SCHEMA_4_TABLES {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                params![table],
                |r| r.get(0),
            )?;
            if exists {
                present.push((*table).to_string());
            }
        }
        Ok(present)
    }

    /// Verify and restore a consistent backup into `target_dir` WITHOUT running schema migrations,
    /// returning the restored `schema_version` (`M2-A-02`).
    pub fn restore_consistent_backup_without_migration(
        backup_dir: &Path,
        target_dir: &Path,
    ) -> Result<i64, StorageError> {
        if target_dir.exists() {
            let mut entries = fs::read_dir(target_dir)?;
            if entries.next().is_some() {
                return Err(StorageError::RestoreTargetNotEmpty(
                    target_dir.display().to_string(),
                ));
            }
        }

        let manifest_path = backup_dir.join("manifest.json");
        if !manifest_path.exists() {
            return Err(StorageError::IntegrityFailed(
                "missing manifest.json in backup directory".into(),
            ));
        }
        let manifest: LibraryManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
        if !manifest.file_hashes.contains_key("delta.sqlite") {
            return Err(StorageError::IntegrityFailed(
                "backup manifest must include delta.sqlite".into(),
            ));
        }

        for (rel_path, expected_hash) in &manifest.file_hashes {
            let src_file = backup_dir.join(rel_path);
            if !src_file.is_file() {
                return Err(StorageError::IntegrityFailed(format!(
                    "missing file referenced in backup manifest: {rel_path}"
                )));
            }
            let actual_hash = sha256_file(&src_file)?;
            if &actual_hash != expected_hash {
                return Err(StorageError::IntegrityFailed(format!(
                    "hash mismatch for {rel_path}: expected {expected_hash}, got {actual_hash}"
                )));
            }
        }

        fs::create_dir_all(target_dir)?;
        for sub in ["raw", "imports", "attachments", "backups", "logs", "cache"] {
            fs::create_dir_all(target_dir.join(sub))?;
        }
        for rel_path in manifest.file_hashes.keys() {
            let src_file = backup_dir.join(rel_path);
            let dst_file = target_dir.join(rel_path);
            if let Some(parent) = dst_file.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_file, &dst_file)?;
        }
        fs::copy(&manifest_path, target_dir.join("manifest.json"))?;

        let restored_conn = Connection::open_with_flags(
            target_dir.join("delta.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let integrity: String =
            restored_conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let fk_violations: i64 =
            restored_conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if integrity != "ok" || fk_violations != 0 {
            return Err(StorageError::IntegrityFailed(
                "restored backup failed integrity or foreign_key_check".into(),
            ));
        }
        let schema_ver: i64 =
            restored_conn.query_row("SELECT schema_version FROM library LIMIT 1", [], |r| {
                r.get(0)
            })?;
        if schema_ver != manifest.schema_version {
            return Err(StorageError::IntegrityFailed(
                "restored schema_version does not match manifest".into(),
            ));
        }
        Ok(schema_ver)
    }

    // ------------------------------------------------------------------------
    // Accounts, Portfolios & Manual Valuations (`M2-W02`, `M2-A-04`, `M2-A-05`)
    // ------------------------------------------------------------------------

    pub fn upsert_account(
        &mut self,
        id: &str,
        name: &str,
        account_type: &str,
        provider: &str,
        external_ref: Option<&str>,
        status: &str,
    ) -> Result<AccountRecord, StorageError> {
        let normalized_type = account_type.trim().to_ascii_lowercase();
        if !matches!(
            normalized_type.as_str(),
            "bank" | "broker" | "exchange" | "wallet" | "manual" | "spot"
        ) {
            return Err(StorageError::IntegrityFailed(format!(
                "unsupported account_type '{account_type}'"
            )));
        }
        let now = Utc::now().to_rfc3339();
        let masked = mask_external_account_ref(external_ref);
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO account (id, name, type, provider, external_ref, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 type = excluded.type,
                 provider = excluded.provider,
                 external_ref = excluded.external_ref,
                 status = excluded.status",
            params![id, name, normalized_type, provider, masked, status, now],
        )?;
        let created_at: String = tx.query_row(
            "SELECT created_at FROM account WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(AccountRecord {
            id: id.to_string(),
            name: name.to_string(),
            account_type: normalized_type,
            provider: provider.to_string(),
            external_ref_masked: masked,
            status: status.to_string(),
            created_at,
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, type, provider, external_ref, status, created_at
             FROM account
             ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let ext: Option<String> = r.get(4)?;
            Ok(AccountRecord {
                id: r.get(0)?,
                name: r.get(1)?,
                account_type: r.get(2)?,
                provider: r.get(3)?,
                external_ref_masked: mask_external_account_ref(ext.as_deref()),
                status: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert_portfolio(
        &mut self,
        id: &str,
        name: &str,
        member_accounts: &[String],
        child_portfolios: &[String],
    ) -> Result<PortfolioRecord, StorageError> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO portfolio (id, name, created_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            params![id, name, now],
        )?;
        tx.execute(
            "DELETE FROM portfolio_member WHERE portfolio_id = ?1",
            params![id],
        )?;
        let mut dedup_members: Vec<String> = member_accounts
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        dedup_members.sort();
        for acct_id in &dedup_members {
            tx.execute(
                "INSERT INTO portfolio_member (portfolio_id, account_id, valid_from, valid_to)
                 VALUES (?1, ?2, NULL, NULL)",
                params![id, acct_id],
            )?;
        }

        tx.execute(
            "DELETE FROM portfolio_group_link WHERE parent_portfolio_id = ?1",
            params![id],
        )?;
        let mut dedup_children: Vec<String> = child_portfolios
            .iter()
            .filter(|c| c.as_str() != id)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        dedup_children.sort();
        for child_id in &dedup_children {
            tx.execute(
                "INSERT INTO portfolio_group_link (parent_portfolio_id, child_portfolio_id)
                 VALUES (?1, ?2)",
                params![id, child_id],
            )?;
        }
        let created_at: String = tx.query_row(
            "SELECT created_at FROM portfolio WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(PortfolioRecord {
            id: id.to_string(),
            name: name.to_string(),
            member_accounts: dedup_members,
            child_portfolios: dedup_children,
            created_at,
        })
    }

    pub fn list_portfolios(&self) -> Result<Vec<PortfolioRecord>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT id, name, created_at FROM portfolio ORDER BY id")?;
        let base_rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(base_rows.len());
        for (id, name, created_at) in base_rows {
            let mut m_stmt = self.conn().prepare(
                "SELECT account_id FROM portfolio_member WHERE portfolio_id = ?1 ORDER BY account_id",
            )?;
            let member_accounts: Vec<String> = m_stmt
                .query_map(params![id], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;

            let mut c_stmt = self.conn().prepare(
                "SELECT child_portfolio_id FROM portfolio_group_link WHERE parent_portfolio_id = ?1 ORDER BY child_portfolio_id",
            )?;
            let child_portfolios: Vec<String> = c_stmt
                .query_map(params![id], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;

            out.push(PortfolioRecord {
                id,
                name,
                member_accounts,
                child_portfolios,
                created_at,
            });
        }
        Ok(out)
    }

    /// Resolve a portfolio (and any nested child portfolios) to a sorted, deduplicated list
    /// of `account_id`s with cycle prevention (`M2-A-04`).
    pub fn resolve_portfolio_accounts(
        &self,
        portfolio_id: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut visited_portfolios = BTreeSet::new();
        let mut resolved_accounts = BTreeSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(portfolio_id.to_string());

        while let Some(curr_pid) = queue.pop_front() {
            if !visited_portfolios.insert(curr_pid.clone()) {
                continue;
            }
            let mut m_stmt = self
                .conn()
                .prepare("SELECT account_id FROM portfolio_member WHERE portfolio_id = ?1")?;
            for row in m_stmt.query_map(params![curr_pid], |r| r.get::<_, String>(0))? {
                resolved_accounts.insert(row?);
            }

            let mut c_stmt = self.conn().prepare(
                "SELECT child_portfolio_id FROM portfolio_group_link WHERE parent_portfolio_id = ?1",
            )?;
            for row in c_stmt.query_map(params![curr_pid], |r| r.get::<_, String>(0))? {
                let child = row?;
                if !visited_portfolios.contains(&child) {
                    queue.push_back(child);
                }
            }
        }

        Ok(resolved_accounts.into_iter().collect())
    }

    pub fn upsert_manual_valuation(&mut self, mv: &ManualValuation) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO manual_valuation (id, account_id, asset_or_liability_id, is_liability, value, currency, as_of, method, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 account_id = excluded.account_id,
                 asset_or_liability_id = excluded.asset_or_liability_id,
                 is_liability = excluded.is_liability,
                 value = excluded.value,
                 currency = excluded.currency,
                 as_of = excluded.as_of,
                 method = excluded.method",
            params![
                mv.id,
                mv.account_id,
                mv.asset_or_liability_id,
                i32::from(mv.is_liability),
                to_canonical_text(mv.value),
                mv.currency.to_ascii_uppercase(),
                mv.as_of.to_string(),
                mv.method,
                now,
            ],
        )?;
        tx.execute("UPDATE analysis_report SET status = 'stale'", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_manual_valuations(
        &self,
        account_scope: Option<&[String]>,
    ) -> Result<Vec<ManualValuation>, StorageError> {
        let scope_set: Option<BTreeSet<String>> =
            account_scope.map(|s| s.iter().cloned().collect());
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, asset_or_liability_id, is_liability, value, currency, as_of, method
             FROM manual_valuation
             ORDER BY as_of ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, account_id, asset_id, is_liability, val_str, currency, as_of_str, method) =
                row?;
            if let Some(ref allowed) = scope_set {
                if !allowed.contains(&account_id) {
                    continue;
                }
            }
            let as_of = NaiveDate::parse_from_str(&as_of_str, "%Y-%m-%d").map_err(|e| {
                StorageError::IntegrityFailed(format!("invalid manual_valuation as_of date: {e}"))
            })?;
            out.push(ManualValuation {
                id,
                account_id,
                asset_or_liability_id: asset_id,
                is_liability,
                value: parse_decimal_lossless(&val_str)?,
                currency,
                as_of,
                method,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // Watchlist & Instrument Search (`M2-W05`, `M2-A-10`)
    // ------------------------------------------------------------------------

    pub fn search_instruments(
        &self,
        query: &str,
    ) -> Result<Vec<InstrumentSearchHit>, StorageError> {
        let needle = query.trim().to_ascii_lowercase();
        let registry = self.load_instrument_registry()?;
        let mut hits = Vec::new();

        for inst in registry.instruments.values() {
            let matched_alias = registry
                .aliases
                .iter()
                .find(|a| {
                    a.instrument_id == inst.id
                        && (!needle.is_empty()
                            && (a.external_symbol.to_ascii_lowercase().contains(&needle)
                                || a.provider.to_ascii_lowercase().contains(&needle)))
                })
                .map(|a| (a.provider.clone(), a.external_symbol.clone()));

            let direct_match = needle.is_empty()
                || inst.id.to_ascii_lowercase().contains(&needle)
                || inst.symbol.to_ascii_lowercase().contains(&needle)
                || inst.venue.to_ascii_lowercase().contains(&needle);

            if !direct_match && matched_alias.is_none() {
                continue;
            }

            let kind_str = match inst.kind {
                delta_core::InstrumentKind::SpotEquity => "spotequity",
                delta_core::InstrumentKind::SpotEtf => "spotetf",
                delta_core::InstrumentKind::SpotCrypto => "spotcrypto",
            };
            let venue_vs_pair_label = match inst.kind {
                delta_core::InstrumentKind::SpotEquity => format!(
                    "{} · {} (Spot Equity, {})",
                    inst.venue, inst.symbol, inst.quote_asset_id
                ),
                delta_core::InstrumentKind::SpotEtf => format!(
                    "{} · {} (Spot ETF, {})",
                    inst.venue, inst.symbol, inst.quote_asset_id
                ),
                delta_core::InstrumentKind::SpotCrypto => format!(
                    "{} · {} (Spot Crypto Pair, {} -> {})",
                    inst.venue, inst.symbol, inst.base_asset_id, inst.quote_asset_id
                ),
            };

            hits.push(InstrumentSearchHit {
                instrument_id: inst.id.clone(),
                venue: inst.venue.clone(),
                symbol: inst.symbol.clone(),
                kind: kind_str.to_string(),
                base_asset_id: inst.base_asset_id.clone(),
                quote_asset_id: inst.quote_asset_id.clone(),
                matched_alias,
                venue_vs_pair_label,
            });
        }
        hits.sort_by(|a, b| a.instrument_id.cmp(&b.instrument_id));
        Ok(hits)
    }

    pub fn upsert_watchlist_group(
        &mut self,
        id: &str,
        name: &str,
        sort_order: i64,
    ) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO watchlist_group (id, name, sort_order, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, sort_order = excluded.sort_order",
            params![id, name, sort_order, now],
        )?;
        Ok(())
    }

    pub fn add_watchlist_item(
        &mut self,
        group_id: &str,
        instrument_id: &str,
    ) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT OR IGNORE INTO watchlist_item (group_id, instrument_id, added_at)
             VALUES (?1, ?2, ?3)",
            params![group_id, instrument_id, now],
        )?;
        Ok(())
    }

    pub fn list_watchlist_groups(&self) -> Result<Vec<WatchlistGroupDto>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT id, name FROM watchlist_group ORDER BY sort_order ASC, id ASC")?;
        let groups: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(groups.len());
        for (id, name) in groups {
            let mut item_stmt = self.conn().prepare(
                "SELECT instrument_id FROM watchlist_item WHERE group_id = ?1 ORDER BY added_at ASC, instrument_id ASC",
            )?;
            let instruments: Vec<String> = item_stmt
                .query_map(params![id], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            out.push(WatchlistGroupDto {
                id,
                name,
                instruments,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // Trade Plans, Chart Contexts, Notes, Revisions, Links & FTS5 (`M2-W06`)
    // ------------------------------------------------------------------------

    pub fn save_chart_context(&mut self, ctx: &ChartContextDto) -> Result<String, StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO chart_context (
                id, instrument_id, venue, timeframe, visible_start_date, visible_end_date,
                anchor_date, adjustment, indicators_json, dataset_version, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                instrument_id = excluded.instrument_id,
                venue = excluded.venue,
                timeframe = excluded.timeframe,
                visible_start_date = excluded.visible_start_date,
                visible_end_date = excluded.visible_end_date,
                anchor_date = excluded.anchor_date,
                adjustment = excluded.adjustment,
                indicators_json = excluded.indicators_json,
                dataset_version = excluded.dataset_version",
            params![
                ctx.id,
                ctx.instrument_id,
                ctx.venue,
                ctx.timeframe,
                ctx.visible_start_date.to_string(),
                ctx.visible_end_date.to_string(),
                ctx.anchor_date.map(|d| d.to_string()),
                ctx.adjustment.as_str(),
                ctx.indicators_json,
                ctx.dataset_version,
                now,
            ],
        )?;
        Ok(ctx.id.clone())
    }

    pub fn get_chart_context(&self, id: &str) -> Result<ChartContextDto, StorageError> {
        let latest_dsv: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(version_num), 0) FROM dataset_version",
            [],
            |r| r.get(0),
        )?;
        let row = self.conn().query_row(
            "SELECT instrument_id, venue, timeframe, visible_start_date, visible_end_date,
                    anchor_date, adjustment, indicators_json, dataset_version
             FROM chart_context WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            },
        )?;
        let (
            instrument_id,
            venue,
            timeframe,
            start_s,
            end_s,
            anchor_s,
            adj_s,
            indicators_json,
            dataset_version,
        ) = row;
        let visible_start_date = NaiveDate::parse_from_str(&start_s, "%Y-%m-%d").map_err(|e| {
            StorageError::IntegrityFailed(format!("invalid chart_context start_date: {e}"))
        })?;
        let visible_end_date = NaiveDate::parse_from_str(&end_s, "%Y-%m-%d").map_err(|e| {
            StorageError::IntegrityFailed(format!("invalid chart_context end_date: {e}"))
        })?;
        let anchor_date = match anchor_s {
            Some(s) => Some(NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|e| {
                StorageError::IntegrityFailed(format!("invalid chart_context anchor_date: {e}"))
            })?),
            None => None,
        };
        let adjustment = AdjustmentKind::parse(&adj_s).ok_or_else(|| {
            StorageError::IntegrityFailed(format!("invalid chart_context adjustment '{adj_s}'"))
        })?;
        let has_newer_dataset_version = dataset_version.is_some_and(|v| latest_dsv > v);

        Ok(ChartContextDto {
            id: id.to_string(),
            instrument_id,
            venue,
            timeframe,
            visible_start_date,
            visible_end_date,
            anchor_date,
            adjustment,
            indicators_json,
            dataset_version,
            latest_dataset_version: latest_dsv,
            has_newer_dataset_version,
        })
    }

    pub fn upsert_trade_plan(&mut self, plan: &TradePlanDto) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO trade_plan (id, title, instrument_id, thesis, entry_rule, invalidation, risk_budget, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                instrument_id = excluded.instrument_id,
                thesis = excluded.thesis,
                entry_rule = excluded.entry_rule,
                invalidation = excluded.invalidation,
                risk_budget = excluded.risk_budget,
                status = excluded.status",
            params![
                plan.id,
                plan.title,
                plan.instrument_id,
                plan.thesis,
                plan.entry_rule,
                plan.invalidation,
                plan.risk_budget,
                plan.status,
                now,
            ],
        )?;
        Ok(())
    }

    /// Link a fill (`economic_event`) to a `trade_plan` and/or `note` while enforcing that total
    /// `allocated_quantity` across links does not exceed the fill's quantity (`M2-A-11`).
    pub fn link_plan_or_note_fill(
        &mut self,
        plan_id: Option<&str>,
        note_id: Option<&str>,
        event_id: &str,
        allocated_quantity: Decimal,
        relation: &str,
    ) -> Result<(), StorageError> {
        if allocated_quantity <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "allocated quantity must be positive".into(),
            ));
        }

        let tx = self.conn_mut().savepoint()?;
        let fill_row: Option<(String, i64)> = tx
            .query_row(
                "SELECT quantity, revision FROM economic_event
                 WHERE (id = ?1 OR correction_group_id = ?1) AND active_in_correction_group = 1
                 ORDER BY revision DESC LIMIT 1",
                params![event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (fill_qty_str, current_rev) = fill_row.ok_or_else(|| {
            StorageError::IntegrityFailed(format!("fill event '{event_id}' not found"))
        })?;
        let fill_qty = parse_decimal_lossless(&fill_qty_str)?.abs();

        if let Some(pid) = plan_id {
            let mut stmt = tx.prepare(
                "SELECT allocated_quantity FROM plan_fill_link WHERE event_id = ?1 AND plan_id != ?2",
            )?;
            let mut existing_sum = Decimal::ZERO;
            for r in stmt.query_map(params![event_id, pid], |r| r.get::<_, String>(0))? {
                existing_sum += parse_decimal_lossless(&r?)?;
            }
            drop(stmt);
            if existing_sum + allocated_quantity > fill_qty {
                return Err(StorageError::IntegrityFailed(
                    "allocated quantity exceeds fill quantity".into(),
                ));
            }
            tx.execute(
                "INSERT INTO plan_fill_link (plan_id, event_id, allocated_quantity)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(plan_id, event_id) DO UPDATE SET allocated_quantity = excluded.allocated_quantity",
                params![pid, event_id, to_canonical_text(allocated_quantity)],
            )?;
        }

        if let Some(nid) = note_id {
            let mut stmt = tx.prepare(
                "SELECT allocated_quantity FROM note_link
                 WHERE target_type = 'event' AND target_id = ?1 AND allocated_quantity IS NOT NULL",
            )?;
            let mut existing_sum = Decimal::ZERO;
            for r in stmt.query_map(params![event_id], |r| r.get::<_, String>(0))? {
                existing_sum += parse_decimal_lossless(&r?)?;
            }
            drop(stmt);
            if existing_sum + allocated_quantity > fill_qty {
                return Err(StorageError::IntegrityFailed(
                    "allocated quantity exceeds fill quantity".into(),
                ));
            }
            let link_id = format!("NL:{nid}:{event_id}:{}", uuid::Uuid::new_v4());
            tx.execute(
                "INSERT INTO note_link (id, note_id, target_type, target_id, target_revision, allocated_quantity, relation)
                 VALUES (?1, ?2, 'event', ?3, ?4, ?5, ?6)",
                params![
                    link_id,
                    nid,
                    event_id,
                    current_rev,
                    to_canonical_text(allocated_quantity),
                    relation,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn add_note_link(
        &mut self,
        note_id: &str,
        target_type: &str,
        target_id: &str,
        target_revision: i64,
        allocated_quantity: Option<Decimal>,
        relation: &str,
    ) -> Result<String, StorageError> {
        let id = format!("NL:{note_id}:{target_type}:{target_id}");
        self.conn().execute(
            "INSERT OR REPLACE INTO note_link (id, note_id, target_type, target_id, target_revision, allocated_quantity, relation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                note_id,
                target_type,
                target_id,
                target_revision,
                allocated_quantity.map(to_canonical_text),
                relation,
            ],
        )?;
        Ok(id)
    }

    pub fn save_note_revision(
        &mut self,
        note_id: &str,
        title: &str,
        template_kind: &str,
        allow_model_access: bool,
        body_markdown: &str,
        structured: &StructuredNoteFields,
        tags: &[String],
    ) -> Result<i64, StorageError> {
        if FAIL_NEXT_NOTE_SAVE.with(|flag| flag.replace(false)) {
            return Err(StorageError::Io(std::io::Error::other(
                "injected note save disk failure",
            )));
        }

        let now = Utc::now().to_rfc3339();
        let structured_json = serde_json::to_string(structured)?;
        let tx = self.conn_mut().savepoint()?;

        let prev_rev: i64 = tx.query_row(
            "SELECT COALESCE(MAX(revision), 0) FROM note_revision WHERE note_id = ?1",
            params![note_id],
            |r| r.get(0),
        )?;
        let next_rev = prev_rev + 1;

        tx.execute(
            "INSERT INTO note (id, title, current_revision, allow_model_access, template_kind, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                current_revision = excluded.current_revision,
                allow_model_access = excluded.allow_model_access,
                template_kind = excluded.template_kind,
                updated_at = excluded.updated_at",
            params![
                note_id,
                title,
                next_rev,
                i32::from(allow_model_access),
                template_kind,
                now,
            ],
        )?;

        let rev_id = format!("{note_id}:R{next_rev}");
        tx.execute(
            "INSERT INTO note_revision (id, note_id, revision, body_markdown, structured_json, saved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![rev_id, note_id, next_rev, body_markdown, structured_json, now],
        )?;

        tx.execute("DELETE FROM note_tag WHERE note_id = ?1", params![note_id])?;
        let mut dedup_tags: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        dedup_tags.sort();
        for tag_name in &dedup_tags {
            let tag_id = format!("TAG:{}", tag_name.to_ascii_lowercase());
            tx.execute(
                "INSERT INTO tag (id, name) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
                params![tag_id, tag_name],
            )?;
            let actual_tag_id: String = tx.query_row(
                "SELECT id FROM tag WHERE name = ?1",
                params![tag_name],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO note_tag (note_id, tag_id) VALUES (?1, ?2)",
                params![note_id, actual_tag_id],
            )?;
        }

        let tags_text = dedup_tags.join(" ");
        tx.execute("DELETE FROM note_fts WHERE note_id = ?1", params![note_id])?;
        tx.execute(
            "INSERT INTO note_fts (note_id, title, body_markdown, tags_text)
             VALUES (?1, ?2, ?3, ?4)",
            params![note_id, title, body_markdown, tags_text],
        )?;

        tx.commit()?;
        Ok(next_rev)
    }

    pub fn get_note(&self, note_id: &str) -> Result<NoteDto, StorageError> {
        let (title, current_revision, allow_int, template_kind): (String, i64, i64, String) =
            self.conn().query_row(
                "SELECT title, current_revision, allow_model_access, template_kind
                 FROM note WHERE id = ?1",
                params![note_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        let (body_markdown, structured_json, saved_at): (String, String, String) =
            self.conn().query_row(
                "SELECT body_markdown, structured_json, saved_at
                 FROM note_revision
                 WHERE note_id = ?1 AND revision = ?2",
                params![note_id, current_revision],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let structured: StructuredNoteFields =
            serde_json::from_str(&structured_json).unwrap_or_default();

        let mut tag_stmt = self.conn().prepare(
            "SELECT t.name FROM tag t
             JOIN note_tag nt ON nt.tag_id = t.id
             WHERE nt.note_id = ?1
             ORDER BY t.name ASC",
        )?;
        let tags: Vec<String> = tag_stmt
            .query_map(params![note_id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut link_stmt = self.conn().prepare(
            "SELECT id, target_type, target_id, target_revision, allocated_quantity, relation
             FROM note_link WHERE note_id = ?1 ORDER BY id ASC",
        )?;
        let raw_links: Vec<(String, String, String, i64, Option<String>, String)> = link_stmt
            .query_map(params![note_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut links = Vec::with_capacity(raw_links.len());
        for (id, target_type, target_id, target_revision, alloc_opt, relation) in raw_links {
            let current_target_revision = if target_type == "event" {
                let grp_opt: Option<String> = self
                    .conn()
                    .query_row(
                        "SELECT correction_group_id FROM economic_event WHERE id = ?1 LIMIT 1",
                        params![target_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let lookup_grp = grp_opt.unwrap_or_else(|| target_id.clone());
                self.conn()
                    .query_row(
                        "SELECT COALESCE(MAX(revision), ?2) FROM economic_event
                         WHERE correction_group_id = ?1 OR id = ?1",
                        params![lookup_grp, target_revision],
                        |r| r.get(0),
                    )
                    .unwrap_or(target_revision)
            } else {
                target_revision
            };
            let allocated_quantity = match alloc_opt {
                Some(s) => Some(parse_decimal_lossless(&s)?),
                None => None,
            };
            links.push(NoteLinkDto {
                id,
                note_id: note_id.to_string(),
                target_type,
                target_id,
                target_revision,
                current_target_revision,
                is_target_amended: current_target_revision > target_revision,
                allocated_quantity,
                relation,
            });
        }

        let mut att_stmt = self.conn().prepare(
            "SELECT id, sha256_hash, file_name, mime_type, size_bytes, relative_path
             FROM attachment WHERE note_id = ?1 ORDER BY created_at ASC, id ASC",
        )?;
        let raw_atts: Vec<(String, String, String, String, i64, String)> = att_stmt
            .query_map(params![note_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut attachments = Vec::with_capacity(raw_atts.len());
        for (id, sha256_hash, file_name, mime_type, size_bytes, relative_path) in raw_atts {
            let exists_on_disk = self.root_dir().join(&relative_path).is_file();
            attachments.push(AttachmentDto {
                id,
                note_id: note_id.to_string(),
                sha256_hash,
                file_name,
                mime_type,
                size_bytes: size_bytes.max(0) as u64,
                relative_path,
                exists_on_disk,
            });
        }

        Ok(NoteDto {
            id: note_id.to_string(),
            title,
            current_revision,
            allow_model_access: allow_int != 0,
            template_kind,
            body_markdown,
            structured,
            tags,
            links,
            attachments,
            saved_at,
        })
    }

    pub fn list_notes(&self, only_model_accessible: bool) -> Result<Vec<NoteDto>, StorageError> {
        let sql = if only_model_accessible {
            "SELECT id FROM note WHERE allow_model_access = 1 ORDER BY updated_at DESC, id ASC"
        } else {
            "SELECT id FROM note ORDER BY updated_at DESC, id ASC"
        };
        let mut stmt = self.conn().prepare(sql)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get_note(&id)?);
        }
        Ok(out)
    }

    pub fn add_note_attachment(
        &mut self,
        note_id: &str,
        file_name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentDto, StorageError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha256_hash = hex::encode(hasher.finalize());
        let relative_path = format!("attachments/{sha256_hash}");
        let disk_path = self.root_dir().join(&relative_path);
        if let Some(parent) = disk_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !disk_path.exists() {
            fs::write(&disk_path, bytes)?;
        }
        let id = format!("ATT:{note_id}:{sha256_hash}");
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT OR REPLACE INTO attachment (id, note_id, sha256_hash, file_name, mime_type, size_bytes, relative_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                note_id,
                sha256_hash,
                file_name,
                mime_type,
                bytes.len() as i64,
                relative_path,
                now,
            ],
        )?;
        self.sync_manifest()?;
        Ok(AttachmentDto {
            id,
            note_id: note_id.to_string(),
            sha256_hash,
            file_name: file_name.to_string(),
            mime_type: mime_type.to_string(),
            size_bytes: bytes.len() as u64,
            relative_path,
            exists_on_disk: true,
        })
    }

    /// Search notes using FTS5 `trigram` for `>= 3` characters and bounded `LIKE` fallback for
    /// 1-char and 2-char queries (`S-17`, `M2-K05`, `M2-A-12`).
    pub fn search_notes(
        &self,
        query: &str,
        only_model_accessible: bool,
    ) -> Result<(Vec<NoteDto>, String), StorageError> {
        let trimmed = query.trim();
        let char_count = trimmed.chars().count();
        if char_count == 0 {
            return Ok((self.list_notes(only_model_accessible)?, "empty_all".into()));
        }

        let (matched_ids, mode): (Vec<String>, String) = if char_count >= 3 {
            let escaped = format!("\"{}\"", trimmed.replace('"', "\"\""));
            let mut stmt = self.conn().prepare(
                "SELECT n.id
                 FROM note_fts f
                 JOIN note n ON n.id = f.note_id
                 WHERE note_fts MATCH ?1
                   AND (?2 = 0 OR n.allow_model_access = 1)
                 ORDER BY n.id ASC",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![escaped, i32::from(only_model_accessible)], |r| {
                    r.get(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (ids, "fts5_trigram".to_string())
        } else {
            let pattern = format!("%{trimmed}%");
            let mut stmt = self.conn().prepare(
                "SELECT n.id
                 FROM note_fts f
                 JOIN note n ON n.id = f.note_id
                 WHERE (f.title LIKE ?1 OR f.body_markdown LIKE ?1 OR f.tags_text LIKE ?1)
                   AND (?2 = 0 OR n.allow_model_access = 1)
                 ORDER BY n.id ASC",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![pattern, i32::from(only_model_accessible)], |r| {
                    r.get(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (ids, "bounded_like_fallback".to_string())
        };

        let mut notes = Vec::with_capacity(matched_ids.len());
        for id in matched_ids {
            notes.push(self.get_note(&id)?);
        }
        Ok((notes, mode))
    }

    /// Candidate IDs are bounded before full note bodies and links are loaded by the host.
    pub fn search_note_candidate_ids(
        &self,
        query: &str,
        only_model_accessible: bool,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let query = query.trim();
        let (sql, pattern) = if query.is_empty() {
            ("SELECT n.id FROM note n WHERE (?2=0 OR n.allow_model_access=1) AND ?1='' ORDER BY n.id LIMIT ?3", String::new())
        } else if query.chars().count() >= 3 {
            ("SELECT n.id FROM note_fts f JOIN note n ON n.id=f.note_id WHERE note_fts MATCH ?1 AND (?2=0 OR n.allow_model_access=1) ORDER BY n.id LIMIT ?3",
                format!("\"{}\"", query.replace('"', "\"\"")))
        } else {
            ("SELECT n.id FROM note_fts f JOIN note n ON n.id=f.note_id WHERE (f.title LIKE ?1 ESCAPE '\\' OR f.body_markdown LIKE ?1 ESCAPE '\\' OR f.tags_text LIKE ?1 ESCAPE '\\') AND (?2=0 OR n.allow_model_access=1) ORDER BY n.id LIMIT ?3",
                format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
        };
        let mut statement = self.conn().prepare(sql)?;
        let rows = statement.query_map(
            params![
                pattern,
                i32::from(only_model_accessible),
                limit.min(10_001) as i64
            ],
            |row| row.get(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    // ------------------------------------------------------------------------
    // AI Session, Run, Message, ToolCall & Checkpoint Storage (`M2-W07`, `S-21`)
    // ------------------------------------------------------------------------

    pub fn upsert_model_connection(
        &mut self,
        conn: &ModelConnectionRecord,
    ) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO model_connection (
                id, protocol, base_url, model_id, credential_ref,
                context_window, capabilities_json, timeouts_json, budget_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                protocol = excluded.protocol,
                base_url = excluded.base_url,
                model_id = excluded.model_id,
                credential_ref = excluded.credential_ref,
                context_window = excluded.context_window,
                capabilities_json = excluded.capabilities_json,
                timeouts_json = excluded.timeouts_json,
                budget_json = excluded.budget_json,
                updated_at = excluded.updated_at",
            params![
                conn.id,
                conn.protocol,
                conn.base_url,
                conn.model_id,
                conn.credential_ref,
                conn.context_window,
                conn.capabilities_json,
                conn.timeouts_json,
                conn.budget_json,
                conn.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_model_connection(&self, id: &str) -> Result<ModelConnectionRecord, StorageError> {
        self.conn()
            .query_row(
                "SELECT id, protocol, base_url, model_id, credential_ref,
                        context_window, capabilities_json, timeouts_json, budget_json, updated_at
                 FROM model_connection WHERE id = ?1",
                params![id],
                |r| {
                    Ok(ModelConnectionRecord {
                        id: r.get(0)?,
                        protocol: r.get(1)?,
                        base_url: r.get(2)?,
                        model_id: r.get(3)?,
                        credential_ref: r.get(4)?,
                        context_window: r.get(5)?,
                        capabilities_json: r.get(6)?,
                        timeouts_json: r.get(7)?,
                        budget_json: r.get(8)?,
                        updated_at: r.get(9)?,
                    })
                },
            )
            .map_err(StorageError::from)
    }

    pub fn list_model_connections(&self) -> Result<Vec<ModelConnectionRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, protocol, base_url, model_id, credential_ref,
                    context_window, capabilities_json, timeouts_json, budget_json, updated_at
             FROM model_connection ORDER BY updated_at DESC, id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ModelConnectionRecord {
                id: r.get(0)?,
                protocol: r.get(1)?,
                base_url: r.get(2)?,
                model_id: r.get(3)?,
                credential_ref: r.get(4)?,
                context_window: r.get(5)?,
                capabilities_json: r.get(6)?,
                timeouts_json: r.get(7)?,
                budget_json: r.get(8)?,
                updated_at: r.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn create_ai_session(&mut self, session_id: &str) -> Result<AiSessionRecord, StorageError> {
        let lib_id: String = self
            .conn()
            .query_row("SELECT id FROM library LIMIT 1", [], |r| r.get(0))?;
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO ai_session (id, library_id, schema_version, active_checkpoint_id, status, created_at, updated_at)
             VALUES (?1, ?2, 1, NULL, 'active', ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at",
            params![session_id, lib_id, now],
        )?;
        self.get_ai_session(session_id)
    }

    pub fn get_ai_session(&self, session_id: &str) -> Result<AiSessionRecord, StorageError> {
        self.conn()
            .query_row(
                "SELECT id, library_id, schema_version, active_checkpoint_id, status, created_at, updated_at
                 FROM ai_session WHERE id = ?1",
                params![session_id],
                |r| {
                    Ok(AiSessionRecord {
                        id: r.get(0)?,
                        library_id: r.get(1)?,
                        schema_version: r.get(2)?,
                        active_checkpoint_id: r.get(3)?,
                        status: r.get(4)?,
                        created_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                },
            )
            .map_err(StorageError::from)
    }

    pub fn create_ai_run(&mut self, run: &AiRunRecord) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO ai_run (
                id, session_id, generation, scope_json, dataset_version, ledger_revision,
                tool_schema_hash, model_connection_id, budget_json, state,
                checkpoint_ref, report_json, usage_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                run.id,
                run.session_id,
                run.generation,
                run.scope_json,
                run.dataset_version,
                run.ledger_revision,
                run.tool_schema_hash,
                run.model_connection_id,
                run.budget_json,
                run.state,
                run.checkpoint_ref,
                run.report_json,
                run.usage_json,
                run.created_at,
                run.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_ai_run_state(
        &mut self,
        run_id: &str,
        state: &str,
        report_json: Option<&str>,
        usage_json: Option<&str>,
    ) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "UPDATE ai_run
             SET state = ?2,
                 report_json = COALESCE(?3, report_json),
                 usage_json = COALESCE(?4, usage_json),
                 updated_at = ?5
             WHERE id = ?1",
            params![run_id, state, report_json, usage_json, now],
        )?;
        Ok(())
    }

    pub fn get_ai_run(&self, run_id: &str) -> Result<AiRunRecord, StorageError> {
        self.conn()
            .query_row(
                "SELECT id, session_id, generation, scope_json, dataset_version, ledger_revision,
                        tool_schema_hash, model_connection_id, budget_json, state,
                        checkpoint_ref, report_json, usage_json, created_at, updated_at
                 FROM ai_run WHERE id = ?1",
                params![run_id],
                |r| {
                    Ok(AiRunRecord {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        generation: r.get(2)?,
                        scope_json: r.get(3)?,
                        dataset_version: r.get(4)?,
                        ledger_revision: r.get(5)?,
                        tool_schema_hash: r.get(6)?,
                        model_connection_id: r.get(7)?,
                        budget_json: r.get(8)?,
                        state: r.get(9)?,
                        checkpoint_ref: r.get(10)?,
                        report_json: r.get(11)?,
                        usage_json: r.get(12)?,
                        created_at: r.get(13)?,
                        updated_at: r.get(14)?,
                    })
                },
            )
            .map_err(StorageError::from)
    }

    pub fn append_ai_message(&mut self, msg: &AiMessageRecord) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO ai_message (id, session_id, run_id, turn_id, sequence, kind, payload_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                msg.id,
                msg.session_id,
                msg.run_id,
                msg.turn_id,
                msg.sequence,
                msg.kind,
                msg.payload_json,
                msg.status,
                msg.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_ai_messages(&self, session_id: &str) -> Result<Vec<AiMessageRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, session_id, run_id, turn_id, sequence, kind, payload_json, status, created_at
             FROM ai_message WHERE session_id = ?1 ORDER BY sequence ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(AiMessageRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                run_id: r.get(2)?,
                turn_id: r.get(3)?,
                sequence: r.get(4)?,
                kind: r.get(5)?,
                payload_json: r.get(6)?,
                status: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Record an AI tool call with idempotent `(run_id, call_id)` semantics (`S-21`):
    /// - If `(run_id, call_id)` already exists with the same `args_hash`, returns existing `result_json`.
    /// - If `(run_id, call_id)` exists with a different `args_hash`, returns `Err(StorageError::IntegrityFailed)`.
    pub fn record_ai_tool_call(
        &mut self,
        tc: &AiToolCallRecord,
    ) -> Result<Option<String>, StorageError> {
        let existing: Option<(String, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT args_hash, result_json FROM ai_tool_call WHERE run_id = ?1 AND call_id = ?2",
                params![tc.run_id, tc.call_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if let Some((existing_hash, existing_res)) = existing {
            if existing_hash != tc.args_hash {
                return Err(StorageError::IntegrityFailed(
                    "duplicate tool call_id with mismatched arguments".into(),
                ));
            }
            return Ok(existing_res);
        }

        self.conn().execute(
            "INSERT INTO ai_tool_call (id, run_id, call_id, args_hash, tool_name, args_json, result_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                tc.id,
                tc.run_id,
                tc.call_id,
                tc.args_hash,
                tc.tool_name,
                tc.args_json,
                tc.result_json,
                tc.status,
                tc.created_at,
            ],
        )?;
        Ok(tc.result_json.clone())
    }

    /// Atomically insert an `ai_checkpoint` and switch `ai_session.active_checkpoint_id` (`S-21`, `M2-A-15`).
    /// Fault injection via `FAIL_NEXT_AI_COMPACTION` aborts the savepoint and preserves the previous pointer.
    pub fn commit_ai_checkpoint_atomic(
        &mut self,
        cp: &AiCheckpointRecord,
    ) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO ai_checkpoint (
                id, session_id, source_start_seq, source_end_seq,
                source_hash, summary_text, model_ref, template_version, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cp.id,
                cp.session_id,
                cp.source_start_seq,
                cp.source_end_seq,
                cp.source_hash,
                cp.summary_text,
                cp.model_ref,
                cp.template_version,
                cp.created_at,
            ],
        )?;

        if FAIL_NEXT_AI_COMPACTION.with(|flag| flag.replace(false)) {
            return Err(StorageError::IntegrityFailed(
                "injected AI checkpoint compaction failure before pointer switch".into(),
            ));
        }

        tx.execute(
            "UPDATE ai_session SET active_checkpoint_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![cp.session_id, cp.id, cp.created_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_ai_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<AiCheckpointRecord, StorageError> {
        self.conn()
            .query_row(
                "SELECT id, session_id, source_start_seq, source_end_seq,
                        source_hash, summary_text, model_ref, template_version, created_at
                 FROM ai_checkpoint WHERE id = ?1",
                params![checkpoint_id],
                |r| {
                    Ok(AiCheckpointRecord {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        source_start_seq: r.get(2)?,
                        source_end_seq: r.get(3)?,
                        source_hash: r.get(4)?,
                        summary_text: r.get(5)?,
                        model_ref: r.get(6)?,
                        template_version: r.get(7)?,
                        created_at: r.get(8)?,
                    })
                },
            )
            .map_err(StorageError::from)
    }

    /// Mark any unfinished `running` or `queued` AI runs and `streaming` messages as `interrupted`
    /// on library startup (`S-21`).
    pub fn mark_interrupted_runs_on_startup(&mut self) -> Result<usize, StorageError> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn_mut().savepoint()?;
        let affected_runs = tx.execute(
            "UPDATE ai_run SET state = 'interrupted', updated_at = ?1
             WHERE state IN ('queued', 'running', 'streaming', 'calling_tool')",
            params![now],
        )?;
        tx.execute(
            "UPDATE ai_message SET status = 'interrupted'
             WHERE status IN ('streaming', 'pending')",
            [],
        )?;
        tx.commit()?;
        Ok(affected_runs)
    }
}

/// Execute the `M2-K05` SQLite FTS5 `trigram` + 1-2 char bounded fallback spike and return a JSON report.
pub fn run_m2_k05_fts5_trigram_spike(temp_dir: &Path) -> Result<serde_json::Value, StorageError> {
    let mut store = SqliteStore::open_or_init(temp_dir, "USD", "USD", "UTC")?;
    let structured = StructuredNoteFields {
        schema_version: 1,
        thesis: "苹果拆股复盘与NVDA突破".into(),
        invalidation: "跌破MA20".into(),
        planned_risk: "1%".into(),
        actual_execution: "按计划买入AAPL".into(),
        emotion: "冷静".into(),
        conclusion: "复盘总结良好".into(),
    };
    store.save_note_revision(
        "NOTE-K05-1",
        "苹果拆股复盘笔记",
        "post_trade",
        true,
        "今日完成苹果拆股复盘，验证 split adjusted K线与成交锚点。",
        &structured,
        &["复盘".into(), "AAPL".into()],
    )?;
    store.save_note_revision(
        "NOTE-K05-PRIVATE",
        "私密复盘笔记",
        "freeform",
        false,
        "这段苹果拆股复盘笔记不开放给AI模型读取。",
        &structured,
        &["复盘".into()],
    )?;

    let (hits_1char, mode_1char) = store.search_notes("复", false)?;
    let (hits_2char, mode_2char) = store.search_notes("复盘", false)?;
    let (hits_cjk_phrase, mode_cjk_phrase) = store.search_notes("苹果拆股复盘", false)?;
    let (hits_en, mode_en) = store.search_notes("split", false)?;
    let (hits_ai_filtered, _) = store.search_notes("苹果拆股复盘", true)?;

    Ok(serde_json::json!({
        "spike_id": "M2-K05",
        "fts5_tokenizer": "trigram",
        "short_query_fallback": "bounded_like_fallback",
        "queries": {
            "one_char_cjk": { "query": "复", "mode": mode_1char, "hits": hits_1char.len() },
            "two_char_cjk": { "query": "复盘", "mode": mode_2char, "hits": hits_2char.len() },
            "three_plus_cjk": { "query": "苹果拆股复盘", "mode": mode_cjk_phrase, "hits": hits_cjk_phrase.len() },
            "english_token": { "query": "split", "mode": mode_en, "hits": hits_en.len() },
            "model_access_filtered": { "query": "苹果拆股复盘", "only_model_accessible": true, "hits": hits_ai_filtered.len() }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestTempDir(PathBuf);
    impl TestTempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "delta_m2_schema_test_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
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
    fn test_m2_a02_schema_4_migration_backup_fault_injection_and_idempotency() {
        let tmp = TestTempDir::new();
        let lib_dir = tmp.path().join("lib_v3");
        let restore_dir = tmp.path().join("restored_v3");

        // 1. Create a genuine Schema 3 library on disk
        SqliteStore::create_schema_3_library_for_test(&lib_dir, "USD", "USD", "UTC")
            .expect("create schema 3 library");
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            3
        );
        assert!(SqliteStore::list_schema_4_tables_on_disk(&lib_dir)
            .unwrap()
            .is_empty());

        // 2. Inject mid-migration fault before commit and attempt open_or_init
        set_migration_fault_injection_for_test(true);
        let err = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
            .err()
            .expect("fault injection must fail migration");
        assert!(matches!(err, StorageError::MigrationFailed(_)));

        // Verify transaction rolled back cleanly: still schema 3, no partial Schema 4 tables
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            3
        );
        assert!(SqliteStore::list_schema_4_tables_on_disk(&lib_dir)
            .unwrap()
            .is_empty());

        // 3. Verify pre-migration backup was created and restores cleanly to Schema 3
        let backups_dir = lib_dir.join("backups");
        let backup_entry = fs::read_dir(&backups_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("before-schema-3-")
            })
            .expect("pre-migration backup directory must exist");
        let restored_ver = SqliteStore::restore_consistent_backup_without_migration(
            &backup_entry.path(),
            &restore_dir,
        )
        .expect("restore pre-migration backup");
        assert_eq!(restored_ver, 3);

        // 4. Re-run open_or_init without fault: upgrades to Schema 4 and is idempotent on repeat
        let store =
            SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC").expect("upgrade to schema 4");
        drop(store);
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            4
        );
        assert_eq!(
            SqliteStore::list_schema_4_tables_on_disk(&lib_dir)
                .unwrap()
                .len(),
            SCHEMA_4_TABLES.len()
        );

        // Repeat open_or_init to prove idempotency
        let _store_again = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
            .expect("idempotent schema 4 open");
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            4
        );
    }

    #[test]
    fn test_m2_a04_portfolio_deduplication_and_accounts_crud() {
        let tmp = TestTempDir::new();
        let mut store =
            SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");

        let acct1 = store
            .upsert_account(
                "ACC-IBKR",
                "Interactive Brokers",
                "broker",
                "ibkr",
                Some("U12345678"),
                "active",
            )
            .expect("upsert ACC-IBKR");
        assert_eq!(acct1.external_ref_masked.as_deref(), Some("***5678"));

        store
            .upsert_account("ACC-OKX", "OKX Spot", "exchange", "okx", None, "active")
            .expect("upsert ACC-OKX");
        store
            .upsert_account(
                "ACC-BANK",
                "Checking",
                "bank",
                "chase",
                Some("9876"),
                "active",
            )
            .expect("upsert ACC-BANK");

        // Overlapping child portfolios (PF-A has ACC-IBKR & ACC-OKX; PF-B has ACC-OKX & ACC-BANK)
        // Plus cycle PF-A <-> PF-B to verify cycle safety
        store
            .upsert_portfolio(
                "PF-A",
                "Growth",
                &["ACC-IBKR".into(), "ACC-OKX".into()],
                &["PF-B".into()],
            )
            .unwrap();
        store
            .upsert_portfolio(
                "PF-B",
                "Core",
                &["ACC-OKX".into(), "ACC-BANK".into()],
                &["PF-A".into()],
            )
            .unwrap();
        store
            .upsert_portfolio(
                "PF-ROOT",
                "All Combined",
                &["ACC-IBKR".into()],
                &["PF-A".into(), "PF-B".into()],
            )
            .unwrap();

        let resolved = store
            .resolve_portfolio_accounts("PF-ROOT")
            .expect("resolve portfolio accounts");
        assert_eq!(
            resolved,
            vec![
                "ACC-BANK".to_string(),
                "ACC-IBKR".to_string(),
                "ACC-OKX".to_string()
            ]
        );
    }

    #[test]
    fn test_m2_a11_trade_plan_fill_allocation_bounds_and_chart_context_roundtrip() {
        let tmp = TestTempDir::new();
        let mut store =
            SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");

        store
            .upsert_account("ACC-1", "Main", "broker", "ibkr", None, "active")
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO economic_event (
                    id, account_id, type, occurred_at, sequence_no, recorded_at,
                    asset_id, instrument_id, quantity, price, quote_currency,
                    fee_amount, fee_currency, cost_known, correction_group_id,
                    revision, active_in_correction_group
                 ) VALUES (
                    'EV-FILL-10', 'ACC-1', 'buy', '2026-01-05T15:00:00Z', 1, '2026-01-05T15:00:00Z',
                    'ASSET:EQUITY:NASDAQ:AAPL', 'NASDAQ:AAPL', '10', '180', 'USD',
                    '1', 'USD', 1, 'CG-FILL-10', 1, 1
                 )",
                [],
            )
            .unwrap();

        let plan = TradePlanDto {
            id: "PLAN-AAPL-1".into(),
            title: "AAPL Breakout".into(),
            instrument_id: Some("NASDAQ:AAPL".into()),
            thesis: "Post-earnings consolidation breakout".into(),
            entry_rule: "Close > 180".into(),
            invalidation: "Close < 172".into(),
            risk_budget: "500 USD".into(),
            status: "active".into(),
            linked_fills: Vec::new(),
        };
        store.upsert_trade_plan(&plan).unwrap();

        let structured = StructuredNoteFields::default();
        store
            .save_note_revision(
                "NOTE-1",
                "AAPL Fill Review",
                "during_trade",
                true,
                "Partial fill linked to plan.",
                &structured,
                &["AAPL".into()],
            )
            .unwrap();

        // Allocate 6 shares out of 10 to PLAN-AAPL-1 and NOTE-1
        store
            .link_plan_or_note_fill(
                Some("PLAN-AAPL-1"),
                Some("NOTE-1"),
                "EV-FILL-10",
                parse_decimal_lossless("6").unwrap(),
                "execution",
            )
            .expect("allocate 6 shares");

        // Attempting to allocate 5 more shares (6 + 5 = 11 > 10) must be rejected
        let over_err = store
            .link_plan_or_note_fill(
                None,
                Some("NOTE-1"),
                "EV-FILL-10",
                parse_decimal_lossless("5").unwrap(),
                "execution",
            )
            .unwrap_err();
        assert!(matches!(over_err, StorageError::IntegrityFailed(_)));

        // ChartContext round-trip and dataset_version staleness detection
        let dsv1 = store
            .create_dataset_version("tiingo", "official_free_personal", "{}", "exact", "exact")
            .unwrap();
        let ctx = ChartContextDto {
            id: "CTX-AAPL-1".into(),
            instrument_id: "NASDAQ:AAPL".into(),
            venue: "NASDAQ".into(),
            timeframe: "1d".into(),
            visible_start_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            visible_end_date: NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
            anchor_date: Some(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap()),
            adjustment: AdjustmentKind::SplitAdjusted,
            indicators_json: r#"["MA(20)","EMA(50)"]"#.into(),
            dataset_version: Some(dsv1.version_num),
            latest_dataset_version: dsv1.version_num,
            has_newer_dataset_version: false,
        };
        store.save_chart_context(&ctx).unwrap();

        // Create dataset version 2 -> saved ChartContext should report has_newer_dataset_version = true
        let dsv2 = store
            .create_dataset_version("tiingo", "official_free_personal", "{}", "exact", "exact")
            .unwrap();
        let loaded_ctx = store.get_chart_context("CTX-AAPL-1").unwrap();
        assert_eq!(loaded_ctx.dataset_version, Some(dsv1.version_num));
        assert_eq!(loaded_ctx.latest_dataset_version, dsv2.version_num);
        assert!(loaded_ctx.has_newer_dataset_version);

        // Amend fill event (revision 2 in CG-FILL-10) -> note link reports is_target_amended = true
        store
            .conn()
            .execute(
                "UPDATE economic_event SET active_in_correction_group = 0 WHERE id = 'EV-FILL-10'",
                [],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO economic_event (
                    id, account_id, type, occurred_at, sequence_no, recorded_at,
                    asset_id, instrument_id, quantity, price, quote_currency,
                    fee_amount, fee_currency, cost_known, correction_group_id,
                    revision, active_in_correction_group
                 ) VALUES (
                    'EV-FILL-10-R2', 'ACC-1', 'buy', '2026-01-05T15:00:00Z', 1, '2026-01-06T10:00:00Z',
                    'ASSET:EQUITY:NASDAQ:AAPL', 'NASDAQ:AAPL', '10', '179.5', 'USD',
                    '1', 'USD', 1, 'CG-FILL-10', 2, 1
                 )",
                [],
            )
            .unwrap();

        let note = store.get_note("NOTE-1").unwrap();
        assert_eq!(note.links.len(), 1);
        assert_eq!(note.links[0].target_revision, 1);
        assert_eq!(note.links[0].current_target_revision, 2);
        assert!(note.links[0].is_target_amended);
    }

    #[test]
    fn test_m2_a12_notes_autosave_honesty_templates_fts5_search_and_attachments() {
        let tmp = TestTempDir::new();
        let spike = run_m2_k05_fts5_trigram_spike(tmp.path()).expect("run M2-K05 spike");
        assert_eq!(
            spike["queries"]["one_char_cjk"]["mode"],
            "bounded_like_fallback"
        );
        assert_eq!(spike["queries"]["one_char_cjk"]["hits"], 2);
        assert_eq!(
            spike["queries"]["two_char_cjk"]["mode"],
            "bounded_like_fallback"
        );
        assert_eq!(spike["queries"]["two_char_cjk"]["hits"], 2);
        assert_eq!(spike["queries"]["three_plus_cjk"]["mode"], "fts5_trigram");
        assert_eq!(spike["queries"]["three_plus_cjk"]["hits"], 2);
        assert_eq!(spike["queries"]["english_token"]["mode"], "fts5_trigram");
        assert_eq!(spike["queries"]["english_token"]["hits"], 1);
        assert_eq!(spike["queries"]["model_access_filtered"]["hits"], 1);

        let mut store =
            SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");
        let mut editor = NoteEditorSession::new(
            "NOTE-AUTOSAVE-1",
            "Autosave Honesty Test",
            NoteTemplateKind::PreTrade,
            true,
        );
        editor.begin_edit("Initial draft v1".into());
        assert_eq!(editor.status, AutosaveStatus::Editing);

        let rev1 = editor.flush_autosave(&mut store).expect("save rev 1");
        assert_eq!(rev1, 1);
        assert!(matches!(
            editor.status,
            AutosaveStatus::Saved { revision: 1, .. }
        ));

        // Inject write failure on second edit -> must transition to SaveFailed and retain buffer
        editor.begin_edit("Unsaved draft v2 after disk error".into());
        set_note_save_fault_injection_for_test(true);
        let save_err = editor.flush_autosave(&mut store).unwrap_err();
        assert!(matches!(save_err, StorageError::Io(_)));
        assert!(matches!(
            editor.status,
            AutosaveStatus::SaveFailed {
                retryable: true,
                ..
            }
        ));
        assert_eq!(editor.body_buffer, "Unsaved draft v2 after disk error");
        assert_eq!(
            store.get_note("NOTE-AUTOSAVE-1").unwrap().current_revision,
            1
        );

        // Retry after fault clears -> saves revision 2
        let rev2 = editor.flush_autosave(&mut store).expect("retry save rev 2");
        assert_eq!(rev2, 2);
        assert_eq!(
            store.get_note("NOTE-AUTOSAVE-1").unwrap().body_markdown,
            "Unsaved draft v2 after disk error"
        );

        // Content-addressed attachment + missing-file degradation check
        let att = store
            .add_note_attachment(
                "NOTE-AUTOSAVE-1",
                "chart.png",
                "image/png",
                b"fake-png-bytes-1234",
            )
            .expect("add attachment");
        assert!(att.exists_on_disk);

        // Remove underlying attachment file to verify graceful missing-file detection
        fs::remove_file(store.root_dir().join(&att.relative_path)).unwrap();
        let reloaded = store.get_note("NOTE-AUTOSAVE-1").unwrap();
        assert_eq!(reloaded.attachments.len(), 1);
        assert!(!reloaded.attachments[0].exists_on_disk);
    }
}
