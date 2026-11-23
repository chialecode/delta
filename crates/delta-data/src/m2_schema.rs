#![allow(clippy::too_many_arguments)]
use std::cell::Cell;
use std::path::Path;

use chrono::{NaiveDate, Utc};
use delta_core::AdjustmentKind;
use rusqlite::Connection;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::storage::{SqliteStore, StorageError};

mod accounts;
mod ai;
pub(crate) mod notes;

#[cfg(test)]
#[path = "m2_schema/tests.rs"]
mod tests;

thread_local! {
    pub(crate) static FAIL_NEXT_SCHEMA_4_MIGRATION: Cell<bool> = const { Cell::new(false) };
    pub(crate) static FAIL_NEXT_NOTE_SAVE: Cell<bool> = const { Cell::new(false) };
    pub(crate) static FAIL_NEXT_AI_COMPACTION: Cell<bool> = const { Cell::new(false) };
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
        CREATE INDEX IF NOT EXISTS idx_note_link_note
            ON note_link(note_id);

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
        CREATE INDEX IF NOT EXISTS idx_attachment_note
            ON attachment(note_id);

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
        CREATE INDEX IF NOT EXISTS idx_ai_run_session
            ON ai_run(session_id);

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
        CREATE INDEX IF NOT EXISTS idx_ai_checkpoint_session
            ON ai_checkpoint(session_id);
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
    if let Some(rest) = trimmed.strip_prefix("***") {
        let chars: Vec<char> = rest.chars().collect();
        return if chars.len() <= 4 {
            Some(trimmed.to_string())
        } else {
            let last4: String = chars[chars.len() - 4..].iter().collect();
            Some(format!("***{last4}"))
        };
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
