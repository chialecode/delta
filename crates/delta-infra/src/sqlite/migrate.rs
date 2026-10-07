//! Ordered schema migrations. Delivered migrations are append-only
//! (data-and-security.md: no in-place rewrites).

use crate::error::InfraError;
use rusqlite::Connection;

/// v1: initial S1 schema — library, accounts, instruments, events, postings,
/// imports, journal, market data, AI sessions.
const V1: &str = r#"
CREATE TABLE library_meta (
    id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    book_currency TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE account (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    provider TEXT,
    status TEXT NOT NULL DEFAULT 'active'
);
CREATE TABLE portfolio (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE portfolio_member (
    portfolio_id TEXT NOT NULL REFERENCES portfolio(id),
    account_id TEXT NOT NULL REFERENCES account(id),
    valid_from TEXT NOT NULL,
    valid_to TEXT,
    UNIQUE(portfolio_id, account_id, valid_from)
);
CREATE TABLE asset (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,              -- fiat|crypto|equity|other
    symbol TEXT NOT NULL,
    precision INTEGER NOT NULL DEFAULT 8
);
CREATE TABLE instrument (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,              -- spot
    venue TEXT NOT NULL,
    base_asset_id TEXT NOT NULL REFERENCES asset(id),
    quote_asset_id TEXT NOT NULL REFERENCES asset(id),
    lot_size TEXT NOT NULL DEFAULT '1',
    tick_size TEXT NOT NULL DEFAULT '0.01'
);
CREATE TABLE instrument_alias (
    provider TEXT NOT NULL,
    external_symbol TEXT NOT NULL,
    instrument_id TEXT NOT NULL REFERENCES instrument(id),
    valid_from TEXT NOT NULL,
    valid_to TEXT,
    PRIMARY KEY (provider, external_symbol, valid_from)
);
CREATE TABLE economic_event (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES account(id),
    occurred_at TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    seq INTEGER NOT NULL,
    source_ref TEXT,
    correction_group TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    reverses TEXT,
    payload TEXT NOT NULL,           -- JSON
    import_batch_id TEXT
);
CREATE INDEX idx_event_account_time ON economic_event(account_id, occurred_at, seq);
CREATE UNIQUE INDEX idx_event_source ON economic_event(source_ref) WHERE source_ref IS NOT NULL;
CREATE TABLE journal_transaction (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES economic_event(id),
    book_currency TEXT NOT NULL,
    posting_rule_version TEXT NOT NULL,
    reversal_of TEXT
);
CREATE TABLE posting (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id TEXT NOT NULL REFERENCES journal_transaction(id),
    category TEXT NOT NULL,
    sub TEXT,
    asset TEXT NOT NULL,
    native_quantity TEXT NOT NULL,
    book_amount TEXT NOT NULL
);
CREATE TABLE import_batch (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES account(id),
    source TEXT NOT NULL,
    file_hash TEXT NOT NULL,
    mapping_version TEXT NOT NULL,
    status TEXT NOT NULL,            -- preview|committed|failed
    row_count INTEGER NOT NULL DEFAULT 0,
    accepted_count INTEGER NOT NULL DEFAULT 0,
    rejected_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    committed_at TEXT,
    UNIQUE(account_id, file_hash, mapping_version)
);
CREATE TABLE import_row (
    id TEXT PRIMARY KEY,
    batch_id TEXT NOT NULL REFERENCES import_batch(id),
    row_no INTEGER NOT NULL,
    raw_payload TEXT NOT NULL,
    normalized_payload TEXT,
    validation_state TEXT NOT NULL,  -- valid|error|duplicate_suspect|excluded
    reason TEXT,
    event_id TEXT
);
CREATE TABLE balance_snapshot (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES account(id),
    asset TEXT NOT NULL,
    quantity TEXT NOT NULL,
    as_of TEXT NOT NULL,
    source TEXT NOT NULL
);
CREATE TABLE reconciliation_issue (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES account(id),
    asset TEXT NOT NULL,
    as_of TEXT NOT NULL,
    expected TEXT NOT NULL,
    observed TEXT NOT NULL,
    reason TEXT,
    resolution_event_id TEXT
);
CREATE TABLE manual_valuation (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES account(id),
    asset TEXT NOT NULL,
    value TEXT NOT NULL,
    currency TEXT NOT NULL,
    as_of TEXT NOT NULL,
    method TEXT
);
CREATE TABLE journal_entry (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    current_revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE journal_revision (
    journal_id TEXT NOT NULL REFERENCES journal_entry(id),
    revision INTEGER NOT NULL,
    body TEXT NOT NULL,
    structured_fields TEXT,
    saved_at TEXT NOT NULL,
    PRIMARY KEY (journal_id, revision)
);
CREATE TABLE journal_tag (
    journal_id TEXT NOT NULL REFERENCES journal_entry(id),
    tag TEXT NOT NULL,
    PRIMARY KEY (journal_id, tag)
);
CREATE TABLE journal_link (
    id TEXT PRIMARY KEY,
    journal_id TEXT NOT NULL REFERENCES journal_entry(id),
    target_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    relation TEXT,
    tombstone INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE attachment (
    id TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    mime_type TEXT,
    size INTEGER NOT NULL,
    relative_path TEXT NOT NULL
);
CREATE TABLE bar (
    instrument_id TEXT NOT NULL REFERENCES instrument(id),
    timeframe TEXT NOT NULL,
    session_date TEXT NOT NULL,
    open_at TEXT NOT NULL,
    close_at TEXT NOT NULL,
    open TEXT NOT NULL,
    high TEXT NOT NULL,
    low TEXT NOT NULL,
    close TEXT NOT NULL,
    volume TEXT NOT NULL,
    source TEXT NOT NULL,
    adjustment TEXT NOT NULL DEFAULT 'raw',
    is_final INTEGER NOT NULL DEFAULT 1,
    dataset_version TEXT NOT NULL,
    PRIMARY KEY (instrument_id, timeframe, session_date, adjustment, dataset_version)
);
CREATE TABLE corporate_action (
    id TEXT PRIMARY KEY,
    instrument_id TEXT NOT NULL REFERENCES instrument(id),
    action_type TEXT NOT NULL,
    ex_date TEXT,
    pay_date TEXT,
    ratio_or_amount TEXT,
    announced_at TEXT,
    source TEXT NOT NULL
);
CREATE TABLE fx_rate (
    id TEXT PRIMARY KEY,
    base TEXT NOT NULL,
    quote TEXT NOT NULL,
    rate TEXT NOT NULL,
    effective_at TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    source TEXT NOT NULL,
    dataset_version TEXT NOT NULL DEFAULT 'v0'
);
CREATE TABLE calendar_session (
    venue TEXT NOT NULL,
    session_date TEXT NOT NULL,
    timezone TEXT NOT NULL,
    open_at TEXT NOT NULL,
    close_at TEXT NOT NULL,
    kind TEXT NOT NULL,              -- full|half|holiday
    version TEXT NOT NULL,
    PRIMARY KEY (venue, session_date, version)
);
CREATE TABLE dataset_version (
    id TEXT PRIMARY KEY,
    manifest_hash TEXT NOT NULL,
    source TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    coverage TEXT,
    quality TEXT
);
CREATE TABLE analysis_result (
    id TEXT PRIMARY KEY,
    scope_snapshot TEXT NOT NULL,
    metric TEXT NOT NULL,
    value TEXT,
    quality TEXT NOT NULL,
    inputs_hash TEXT,
    calculation_version TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE analysis_report (
    id TEXT PRIMARY KEY,
    report_type TEXT NOT NULL,
    scope TEXT NOT NULL,
    model_ref TEXT,
    prompt_version TEXT,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    body TEXT
);
CREATE TABLE evidence_ref (
    report_id TEXT NOT NULL REFERENCES analysis_report(id),
    target_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    revision TEXT,
    sample_count INTEGER,
    PRIMARY KEY (report_id, target_type, target_id)
);
CREATE TABLE model_connection (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    protocol TEXT NOT NULL,
    base_url TEXT NOT NULL,
    model_name TEXT NOT NULL,
    capabilities TEXT,
    credential_ref TEXT NOT NULL
);
"#;

const V2_AI: &str = r#"
CREATE TABLE ai_session (
    id TEXT PRIMARY KEY,
    library_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1,
    active_checkpoint_id TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE ai_run (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES ai_session(id),
    generation INTEGER NOT NULL,
    scope_snapshot TEXT NOT NULL,
    tool_schema_hash TEXT NOT NULL,
    model_ref TEXT NOT NULL,
    budget TEXT NOT NULL,
    state TEXT NOT NULL,
    checkpoint_ref TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, generation)
);
CREATE TABLE ai_message (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES ai_session(id),
    run_id TEXT NOT NULL REFERENCES ai_run(id),
    sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL,
    connection_ref TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_ai_message_session ON ai_message(session_id, sequence);
CREATE TABLE ai_tool_call (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES ai_run(id),
    call_id TEXT NOT NULL,
    args_hash TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    result_ref TEXT,
    status TEXT NOT NULL,
    UNIQUE(run_id, call_id)
);
CREATE TABLE ai_checkpoint (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES ai_session(id),
    source_range TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    summary TEXT NOT NULL,
    model_ref TEXT NOT NULL,
    template_version TEXT NOT NULL,
    context_version TEXT NOT NULL,
    created_at TEXT NOT NULL
);
"#;

const V3_SEARCH: &str = r#"
CREATE VIRTUAL TABLE journal_fts USING fts5(journal_id UNINDEXED, title, body, tokenize = 'unicode61');
"#;

const V4_SETTINGS: &str = r#"
ALTER TABLE library_meta ADD COLUMN reporting_currency TEXT;
ALTER TABLE library_meta ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';
ALTER TABLE journal_entry ADD COLUMN account_id TEXT;
CREATE TABLE chart_context (
    id TEXT PRIMARY KEY,
    instrument_id TEXT NOT NULL,
    timeframe TEXT NOT NULL,
    start_at TEXT NOT NULL,
    end_at TEXT NOT NULL,
    adjustment TEXT NOT NULL,
    params TEXT NOT NULL,
    saved_at TEXT NOT NULL
);
CREATE TABLE note_template (
    id TEXT PRIMARY KEY,
    slot TEXT NOT NULL,
    body TEXT NOT NULL,
    UNIQUE(slot)
);
"#;

/// v5: source identity is per account; import receipts remember skips;
/// a scope change can continue in a child session; watch items are local.
const V5_ACCOUNT_SOURCE: &str = r#"
DROP INDEX IF EXISTS idx_event_source;
CREATE UNIQUE INDEX idx_event_account_source
    ON economic_event(account_id, source_ref) WHERE source_ref IS NOT NULL;
ALTER TABLE import_batch ADD COLUMN skipped_count INTEGER NOT NULL DEFAULT 0;
CREATE TABLE ai_scope_continuation (
    parent_session_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (parent_session_id, scope_key)
);
CREATE TABLE watch_item (
    instrument_id TEXT NOT NULL PRIMARY KEY,
    added_at TEXT NOT NULL
);
"#;

/// Applied migration versions in order.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("1_initial", V1),
    ("2_ai_sessions", V2_AI),
    ("3_journal_fts", V3_SEARCH),
    ("4_settings_scope_context", V4_SETTINGS),
    ("5_account_source_identity", V5_ACCOUNT_SOURCE),
    (
        "6_desktop_settings",
        "CREATE TABLE desktop_settings (id TEXT PRIMARY KEY, value TEXT NOT NULL);",
    ),
];

pub fn current_version(conn: &Connection) -> Result<i64, InfraError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migration (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL)",
    )
    .map_err(InfraError::Sqlite)?;
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migration",
        [],
        |r| r.get(0),
    )
    .map_err(InfraError::Sqlite)
}

/// Apply pending migrations inside transactions; failure leaves the database
/// at the previous version (data-and-security.md).
pub fn run(conn: &mut Connection) -> Result<(), InfraError> {
    let version = current_version(conn)?;
    let latest = MIGRATIONS.len() as i64;
    // A library written by a newer build is refused: this build would write
    // rows the newer schema does not expect (plan W09).
    if version > latest {
        return Err(InfraError::Migration(format!(
            "library schema v{version} is newer than this build (v{latest})"
        )));
    }
    if version > 0 && version < latest {
        backup_before_migration(conn, version)?;
    }
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if target <= version {
            continue;
        }
        let tx = conn.transaction().map_err(InfraError::Sqlite)?;
        tx.execute_batch(sql)
            .map_err(|e| InfraError::Migration(format!("v{target} {name}: {e}")))?;
        if *name == "4_settings_scope_context" {
            normalize_timestamps(&tx)?;
        }
        tx.execute(
            "INSERT INTO schema_migration (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![target, name, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(InfraError::Sqlite)?;
        tx.commit().map_err(InfraError::Sqlite)?;
        tracing::info!(version = target, name = name, "migration applied");
    }
    Ok(())
}

/// Consistent copy in the library's `backups/` folder before its first
/// pending migration (FR-OPS-01, system-architecture §9). A copy left by
/// an earlier failed attempt is kept as is.
fn backup_before_migration(conn: &Connection, version: i64) -> Result<(), InfraError> {
    let Some(db) = conn.path().filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    let db = std::path::Path::new(db);
    let dir = db
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("backups");
    std::fs::create_dir_all(&dir)?;
    let name = db
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "library".into());
    let backup = dir.join(format!("{name}.pre-migration-v{version}.sqlite"));
    if backup.exists() {
        return Ok(());
    }
    let escaped = backup
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{escaped}'"))
        .map_err(|e| InfraError::Migration(format!("backup before migration failed: {e}")))
}

/// Rewrite stored RFC3339 timestamps to UTC nanosecond `Z` form so string
/// order matches time order (F-18 / F-24). Unparseable values are left in
/// place and reported by the caller that reads them. R1 has no user data;
/// a development library created before v4 is normalized by this migration.
pub fn normalize_timestamps(conn: &Connection) -> Result<(), InfraError> {
    for (table, column) in [
        ("economic_event", "occurred_at"),
        ("economic_event", "recorded_at"),
        ("bar", "open_at"),
        ("bar", "close_at"),
        ("fx_rate", "effective_at"),
        ("fx_rate", "observed_at"),
        ("calendar_session", "open_at"),
        ("calendar_session", "close_at"),
        ("manual_valuation", "as_of"),
        ("balance_snapshot", "as_of"),
    ] {
        normalize_column(conn, table, column)?;
    }
    Ok(())
}

fn canonical_utc(raw: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|t| {
        t.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    })
}

fn normalize_column(conn: &Connection, table: &str, column: &str) -> Result<(), InfraError> {
    let sql = format!("SELECT rowid, {column} FROM {table}");
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(&sql).map_err(InfraError::Sqlite)?;
        let mapped = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(InfraError::Sqlite)?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(InfraError::Sqlite)?
    };
    let update = format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2");
    for (rowid, raw) in rows {
        if let Some(canon) = canonical_utc(&raw) {
            if canon != raw {
                conn.execute(&update, rusqlite::params![canon, rowid])
                    .map_err(InfraError::Sqlite)?;
            }
        }
    }
    Ok(())
}
