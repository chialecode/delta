//! Schema 6 (`S-56`): practice accounts with an append-only entry ledger (`S-52`), per-run chart
//! drawings (`S-54`), the frozen run timeframe / account / fill rule on `training_session`, and
//! the `abandoned` run status. Migration runs inside the caller's savepoint after the automatic
//! pre-migration consistency backup taken by `run_migrations`.

use std::cell::Cell;

use chrono::{DateTime, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::m4_schema::{SimFillRecord, SimOrderRecord};
use crate::storage::{SqliteStore, StorageError};

thread_local! {
    static FAIL_NEXT_SCHEMA_6_MIGRATION: Cell<bool> = const { Cell::new(false) };
}

pub fn set_schema_6_migration_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_SCHEMA_6_MIGRATION.with(|flag| flag.set(fail));
}

/// Tables added by schema 6, parents first.
pub const SCHEMA_6_TABLES: &[&str] = &[
    "practice_account",
    "practice_account_entry",
    "training_drawing",
];

const RUN_STATUS_CHECK_V5: &str =
    "CHECK (status IN ('preparing', 'active', 'paused', 'finished', 'damaged'))";
const RUN_STATUS_CHECK_V6: &str =
    "CHECK (status IN ('preparing', 'active', 'paused', 'finished', 'damaged', 'abandoned'))";

fn has_column(tx: &Connection, table: &str, column: &str) -> Result<bool, StorageError> {
    Ok(tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
        params![column],
        |r| r.get(0),
    )?)
}

/// Replace one CHECK clause in a table definition stored in `sqlite_master`. SQLite cannot alter
/// CHECK constraints; editing the stored definition is the documented route for a change that
/// does not alter the stored row format (it only widens the allowed status values).
fn rewrite_table_check(
    tx: &Connection,
    table: &str,
    from: &str,
    to: &str,
) -> Result<(), StorageError> {
    let sql: String = tx.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |r| r.get(0),
    )?;
    if sql.contains(to) {
        return Ok(());
    }
    if !sql.contains(from) {
        return Err(StorageError::MigrationFailed(format!(
            "unexpected {table} definition; status CHECK not found"
        )));
    }
    let version: i64 = tx.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    tx.execute_batch("PRAGMA writable_schema = ON;")?;
    let result = tx.execute(
        "UPDATE sqlite_master SET sql = ?1 WHERE type = 'table' AND name = ?2",
        params![sql.replace(from, to), table],
    );
    tx.execute_batch(&format!(
        "PRAGMA schema_version = {}; PRAGMA writable_schema = OFF;",
        version + 1
    ))?;
    result?;
    Ok(())
}

pub fn apply_schema_6_ddl(tx: &Connection, from_schema: i64) -> Result<(), StorageError> {
    for (column, ddl) in [
        ("timeframe", "ALTER TABLE training_session ADD COLUMN timeframe TEXT NOT NULL DEFAULT '1d' CHECK (timeframe IN ('1d', '4h', '1h'))"),
        ("practice_account_id", "ALTER TABLE training_session ADD COLUMN practice_account_id TEXT"),
        ("fill_rule_version", "ALTER TABLE training_session ADD COLUMN fill_rule_version TEXT"),
    ] {
        if !has_column(tx, "training_session", column)? {
            tx.execute_batch(ddl)?;
        }
    }
    rewrite_table_check(
        tx,
        "simulation_run",
        RUN_STATUS_CHECK_V5,
        RUN_STATUS_CHECK_V6,
    )?;
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS practice_account (
            id TEXT PRIMARY KEY,
            quote_currency TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS practice_account_entry (
            account_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 1),
            kind TEXT NOT NULL CHECK (kind IN ('deposit', 'withdrawal', 'run_allocation', 'run_settlement', 'delete_reversal')),
            amount TEXT NOT NULL,
            run_id TEXT,
            note TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            PRIMARY KEY (account_id, seq),
            FOREIGN KEY (account_id) REFERENCES practice_account(id)
        );
        CREATE INDEX IF NOT EXISTS idx_practice_entry_run
            ON practice_account_entry(run_id);

        -- Multi-timeframe training port (`S-50`): range scans by (instrument, timeframe, open_at)
        -- regardless of source, so per-step queries stay proportional to the window, not the
        -- full history of the instrument.
        CREATE INDEX IF NOT EXISTS idx_bar_training_port
            ON bar(instrument_id, timeframe, adjustment, open_at);

        CREATE TRIGGER IF NOT EXISTS trg_practice_entry_append_only_update
        BEFORE UPDATE ON practice_account_entry
        BEGIN
            SELECT RAISE(ABORT, 'practice_account_entry is append-only');
        END;
        CREATE TRIGGER IF NOT EXISTS trg_practice_entry_append_only_delete
        BEFORE DELETE ON practice_account_entry
        BEGIN
            SELECT RAISE(ABORT, 'practice_account_entry is append-only');
        END;

        CREATE TABLE IF NOT EXISTS training_drawing (
            drawing_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('ray', 'horizontal', 'segment')),
            anchors_json TEXT NOT NULL,
            color TEXT NOT NULL,
            line_style TEXT NOT NULL CHECK (line_style IN ('solid', 'dashed', 'dotted')),
            deleted INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (run_id, drawing_id),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );

        DROP TRIGGER IF EXISTS trg_training_session_monotonic_guard;
        CREATE TRIGGER trg_training_session_monotonic_guard
        BEFORE UPDATE ON training_session
        BEGIN
            SELECT CASE
                WHEN (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'preparing'
                     AND (NEW.redraw_count != OLD.redraw_count OR NEW.real_instrument_id != OLD.real_instrument_id)
                THEN RAISE(ABORT, 'training_session segment cannot be redrawn after start')
                WHEN (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'preparing'
                     AND (
                         NEW.current_step_index < OLD.current_step_index
                         OR strftime('%Y-%m-%dT%H:%M:%fZ', NEW.visible_until) < strftime('%Y-%m-%dT%H:%M:%fZ', OLD.visible_until)
                     )
                THEN RAISE(ABORT, 'training_session replay clock cannot move backwards')
                WHEN NEW.revealed = 1
                     AND (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) NOT IN ('finished', 'abandoned')
                THEN RAISE(ABORT, 'training_session can only be revealed after simulation_run has ended')
                WHEN NEW.timeframe != OLD.timeframe
                     AND (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'preparing'
                THEN RAISE(ABORT, 'training_session timeframe is frozen after start')
            END;
        END;
        "#,
    )?;
    if from_schema < 6 && FAIL_NEXT_SCHEMA_6_MIGRATION.with(|flag| flag.replace(false)) {
        return Err(StorageError::MigrationFailed(
            "injected mid-migration fault before schema 6 commit".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeAccountRecord {
    pub id: String,
    pub quote_currency: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PracticeEntryKind {
    Deposit,
    Withdrawal,
    RunAllocation,
    RunSettlement,
    DeleteReversal,
}

impl PracticeEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::Withdrawal => "withdrawal",
            Self::RunAllocation => "run_allocation",
            Self::RunSettlement => "run_settlement",
            Self::DeleteReversal => "delete_reversal",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        Ok(match raw {
            "deposit" => Self::Deposit,
            "withdrawal" => Self::Withdrawal,
            "run_allocation" => Self::RunAllocation,
            "run_settlement" => Self::RunSettlement,
            "delete_reversal" => Self::DeleteReversal,
            other => {
                return Err(StorageError::IntegrityFailed(format!(
                    "unknown practice entry kind '{other}'"
                )))
            }
        })
    }
}

/// One signed ledger line: deposits and settlements are positive, withdrawals and allocations
/// negative; reversals carry the sign that cancels the reversed line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeAccountEntryRecord {
    pub account_id: String,
    pub seq: i64,
    pub kind: PracticeEntryKind,
    pub amount: Decimal,
    pub run_id: Option<String>,
    pub note: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingDrawingRecord {
    pub drawing_id: String,
    pub run_id: String,
    pub kind: String,
    pub anchors_json: String,
    pub color: String,
    pub line_style: String,
    pub deleted: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl SqliteStore {
    /// Final raw bar coverage of one instrument/timeframe at a published dataset version:
    /// distinct bar count, first `open_at` and last `close_at` (RFC 3339 text).
    pub fn training_bar_coverage(
        &self,
        instrument_id: &str,
        timeframe: &str,
        dataset_version: i64,
    ) -> Result<(i64, Option<String>, Option<String>), StorageError> {
        Ok(self.conn().query_row(
            "SELECT COUNT(DISTINCT open_at), MIN(open_at), MAX(close_at) FROM bar
             WHERE instrument_id=?1 AND timeframe=?2 AND adjustment='raw' AND is_final=1
               AND valid_from_version<=?3 AND (valid_to_version IS NULL OR valid_to_version>?3)",
            params![instrument_id, timeframe, dataset_version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    pub fn get_practice_account_by_currency(
        &self,
        quote_currency: &str,
    ) -> Result<Option<PracticeAccountRecord>, StorageError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, quote_currency, name, created_at FROM practice_account WHERE quote_currency = ?1",
                params![quote_currency.to_ascii_uppercase()],
                |r| {
                    Ok(PracticeAccountRecord {
                        id: r.get(0)?,
                        quote_currency: r.get(1)?,
                        name: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_practice_accounts(&self) -> Result<Vec<PracticeAccountRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, quote_currency, name, created_at FROM practice_account ORDER BY quote_currency",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PracticeAccountRecord {
                id: r.get(0)?,
                quote_currency: r.get(1)?,
                name: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Create the account for `quote_currency` with its opening deposit, atomically.
    pub fn create_practice_account(
        &mut self,
        quote_currency: &str,
        opening_deposit: Decimal,
        at: DateTime<Utc>,
    ) -> Result<PracticeAccountRecord, StorageError> {
        let ccy = quote_currency.to_ascii_uppercase();
        let rec = PracticeAccountRecord {
            id: format!("PRACTICE:{ccy}"),
            quote_currency: ccy.clone(),
            name: format!("练习账户 {ccy}"),
            created_at: at.to_rfc3339(),
        };
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO practice_account (id, quote_currency, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![rec.id, rec.quote_currency, rec.name, rec.created_at],
        )?;
        tx.execute(
            "INSERT INTO practice_account_entry (account_id, seq, kind, amount, run_id, note, created_at)
             VALUES (?1, 1, 'deposit', ?2, NULL, '初始入金', ?3)",
            params![rec.id, to_canonical_text(opening_deposit), rec.created_at],
        )?;
        tx.commit()?;
        Ok(rec)
    }

    pub fn list_practice_entries(
        &self,
        account_id: &str,
    ) -> Result<Vec<PracticeAccountEntryRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, seq, kind, amount, run_id, note, created_at
             FROM practice_account_entry WHERE account_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (account_id, seq, kind, amount, run_id, note, created_at) = row?;
            out.push(PracticeAccountEntryRecord {
                account_id,
                seq,
                kind: PracticeEntryKind::parse(&kind)?,
                amount: parse_decimal_lossless(&amount)
                    .map_err(|e| StorageError::IntegrityFailed(e.to_string()))?,
                run_id,
                note,
                created_at,
            });
        }
        Ok(out)
    }

    /// Balance = sum of all entries (`S-52` conservation).
    pub fn practice_account_balance(&self, account_id: &str) -> Result<Decimal, StorageError> {
        Ok(self
            .list_practice_entries(account_id)?
            .iter()
            .map(|e| e.amount)
            .sum())
    }

    /// Append entries atomically with the next sequence numbers. The caller has validated the
    /// business rules; the store re-checks that the balance never goes negative.
    pub fn append_practice_entries(
        &mut self,
        account_id: &str,
        entries: &[(PracticeEntryKind, Decimal, Option<String>, String)],
        at: DateTime<Utc>,
    ) -> Result<Vec<PracticeAccountEntryRecord>, StorageError> {
        let tx = self.conn_mut().savepoint()?;
        let records = Self::append_practice_entries_in(&tx, account_id, entries, at)?;
        tx.commit()?;
        Ok(records)
    }

    pub(crate) fn append_practice_entries_in(
        tx: &Connection,
        account_id: &str,
        entries: &[(PracticeEntryKind, Decimal, Option<String>, String)],
        at: DateTime<Utc>,
    ) -> Result<Vec<PracticeAccountEntryRecord>, StorageError> {
        let mut seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM practice_account_entry WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        let mut balance = Decimal::ZERO;
        {
            let mut stmt =
                tx.prepare("SELECT amount FROM practice_account_entry WHERE account_id = ?1")?;
            for amount in stmt.query_map(params![account_id], |r| r.get::<_, String>(0))? {
                balance += parse_decimal_lossless(&amount?)
                    .map_err(|e| StorageError::IntegrityFailed(e.to_string()))?;
            }
        }
        let mut out = Vec::new();
        for (kind, amount, run_id, note) in entries {
            seq += 1;
            balance += *amount;
            if balance < Decimal::ZERO {
                return Err(StorageError::IntegrityFailed(format!(
                    "practice account {account_id} would go negative"
                )));
            }
            tx.execute(
                "INSERT INTO practice_account_entry (account_id, seq, kind, amount, run_id, note, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    account_id,
                    seq,
                    kind.as_str(),
                    to_canonical_text(*amount),
                    run_id,
                    note,
                    at.to_rfc3339()
                ],
            )?;
            out.push(PracticeAccountEntryRecord {
                account_id: account_id.to_string(),
                seq,
                kind: *kind,
                amount: *amount,
                run_id: run_id.clone(),
                note: note.clone(),
                created_at: at.to_rfc3339(),
            });
        }
        Ok(out)
    }

    /// Attach a newly created `preparing` run to its account and write the allocation entry in
    /// the same savepoint.
    pub fn allocate_practice_run(
        &mut self,
        run_id: &str,
        account_id: &str,
        amount: Decimal,
        at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        let busy: Option<String> = tx
            .query_row(
                "SELECT s.run_id FROM training_session s JOIN simulation_run r ON r.run_id = s.run_id
                 WHERE s.practice_account_id = ?1 AND r.status IN ('preparing', 'active', 'paused')
                   AND s.run_id != ?2 LIMIT 1",
                params![account_id, run_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = busy {
            return Err(StorageError::IntegrityFailed(format!(
                "PRACTICE_ACCOUNT_BUSY: run {other} is still open on {account_id}"
            )));
        }
        tx.execute(
            "UPDATE training_session SET practice_account_id = ?1 WHERE run_id = ?2",
            params![account_id, run_id],
        )?;
        Self::append_practice_entries_in(
            &tx,
            account_id,
            &[(
                PracticeEntryKind::RunAllocation,
                -amount,
                Some(run_id.to_string()),
                "开局划拨".to_string(),
            )],
            at,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_training_drawing(
        &mut self,
        d: &TrainingDrawingRecord,
    ) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO training_drawing (drawing_id, run_id, kind, anchors_json, color, line_style, deleted, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(run_id, drawing_id) DO UPDATE SET
                kind = excluded.kind, anchors_json = excluded.anchors_json, color = excluded.color,
                line_style = excluded.line_style, deleted = excluded.deleted, updated_at = excluded.updated_at",
            params![
                d.drawing_id,
                d.run_id,
                d.kind,
                d.anchors_json,
                d.color,
                d.line_style,
                if d.deleted { 1 } else { 0 },
                d.created_at,
                d.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn list_training_drawings(
        &self,
        run_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<TrainingDrawingRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT drawing_id, run_id, kind, anchors_json, color, line_style, deleted, created_at, updated_at
             FROM training_drawing WHERE run_id = ?1 AND (?2 OR deleted = 0)
             ORDER BY created_at, drawing_id",
        )?;
        let rows = stmt.query_map(params![run_id, include_deleted], |r| {
            Ok(TrainingDrawingRecord {
                drawing_id: r.get(0)?,
                run_id: r.get(1)?,
                kind: r.get(2)?,
                anchors_json: r.get(3)?,
                color: r.get(4)?,
                line_style: r.get(5)?,
                deleted: r.get::<_, i64>(6)? != 0,
                created_at: r.get(7)?,
                updated_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Rewrite the order/fill projection of a run from the authoritative simulation state
    /// (`m5-sim-2` modify and undo). Call inside the same savepoint as the step commit.
    pub fn replace_run_projection(
        &mut self,
        run_id: &str,
        orders: &[SimOrderRecord],
        fills: &[SimFillRecord],
    ) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        tx.execute("DELETE FROM sim_fill WHERE run_id = ?1", params![run_id])?;
        tx.execute("DELETE FROM sim_order WHERE run_id = ?1", params![run_id])?;
        for ord in orders {
            tx.execute(
                "INSERT INTO sim_order (
                    order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                    qty, limit_price, stop_price, parent_group_id, group_role,
                    status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    ord.order_id,
                    run_id,
                    ord.instrument_id,
                    ord.order_seq,
                    ord.order_type,
                    ord.side,
                    ord.tif,
                    to_canonical_text(ord.qty),
                    ord.limit_price.map(to_canonical_text),
                    ord.stop_price.map(to_canonical_text),
                    ord.parent_group_id,
                    ord.group_role,
                    ord.status.as_str(),
                    ord.reject_code,
                    ord.cancel_reason,
                    ord.submitted_at.to_rfc3339(),
                    ord.eligible_at.map(|dt| dt.to_rfc3339()),
                    ord.updated_at.to_rfc3339(),
                    ord.payload_json,
                ],
            )?;
        }
        for fill in fills {
            tx.execute(
                "INSERT INTO sim_fill (
                    fill_id, run_id, order_id, instrument_id, bar_close_at,
                    side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    fill.fill_id,
                    run_id,
                    fill.order_id,
                    fill.instrument_id,
                    fill.bar_close_at.to_rfc3339(),
                    fill.side,
                    to_canonical_text(fill.qty),
                    to_canonical_text(fill.raw_price),
                    to_canonical_text(fill.fill_price),
                    to_canonical_text(fill.fee_amount),
                    fill.fee_currency,
                    if fill.ambiguous { 1 } else { 0 },
                    fill.payload_json,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Downgrade a library to a genuine schema 5 (M4) layout for migration tests.
    pub fn downgrade_to_schema_5_for_test(&mut self) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        tx.execute_batch(
            "DROP TRIGGER IF EXISTS trg_practice_entry_append_only_update;
             DROP TRIGGER IF EXISTS trg_practice_entry_append_only_delete;
             DROP INDEX IF EXISTS idx_bar_training_port;",
        )?;
        for table in SCHEMA_6_TABLES.iter().rev() {
            tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
        tx.execute_batch("DROP TRIGGER IF EXISTS trg_training_session_monotonic_guard;")?;
        for column in ["fill_rule_version", "practice_account_id", "timeframe"] {
            if has_column(&tx, "training_session", column)? {
                tx.execute_batch(&format!(
                    "ALTER TABLE training_session DROP COLUMN {column}"
                ))?;
            }
        }
        rewrite_table_check(
            &tx,
            "simulation_run",
            RUN_STATUS_CHECK_V6,
            RUN_STATUS_CHECK_V5,
        )?;
        crate::m4_schema::recreate_schema_5_session_guard(&tx)?;
        tx.execute("UPDATE library SET schema_version = 5", [])?;
        tx.commit()?;
        let backups_dir = self.root_dir().join("backups");
        if backups_dir.is_dir() {
            for entry in std::fs::read_dir(&backups_dir)? {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("before-schema-")
                    && entry.path().is_dir()
                {
                    std::fs::remove_dir_all(entry.path())?;
                }
            }
        }
        self.sync_manifest()?;
        Ok(())
    }

    /// Create an empty genuine schema 5 library on disk (`M5-A-12`).
    pub fn create_schema_5_library_for_test(
        root_dir: &std::path::Path,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<(), StorageError> {
        let mut store = Self::open_or_init(root_dir, book_currency, report_currency, timezone)?;
        store.downgrade_to_schema_5_for_test()
    }
}

/// Entries of one run as (account_id, amount); empty when the run has no practice funding.
fn practice_entries_for_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Vec<(String, Decimal)>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT account_id, amount FROM practice_account_entry WHERE run_id = ?1 ORDER BY account_id, seq",
    )?;
    let rows = stmt.query_map(params![run_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (account, amount) = row?;
        out.push((
            account,
            parse_decimal_lossless(&amount)
                .map_err(|e| StorageError::IntegrityFailed(e.to_string()))?,
        ));
    }
    Ok(out)
}

/// Reversal amount appended when a practice-funded run is deleted: the negated sum of the
/// run's entries, so the account returns to its balance without that run (`S-52`).
pub(crate) fn practice_reversal_for_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<Decimal>, StorageError> {
    let entries = practice_entries_for_run(conn, run_id)?;
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(-entries.iter().map(|(_, a)| *a).sum::<Decimal>()))
}

/// Append the delete reversal entry (append-only; earlier entries stay) inside the delete
/// transaction.
pub(crate) fn append_delete_reversal_in(
    tx: &Connection,
    run_id: &str,
    at: DateTime<Utc>,
) -> Result<(), StorageError> {
    let entries = practice_entries_for_run(tx, run_id)?;
    let Some((account, _)) = entries.first() else {
        return Ok(());
    };
    let reversal = -entries.iter().map(|(_, a)| *a).sum::<Decimal>();
    if reversal.is_zero() {
        return Ok(());
    }
    SqliteStore::append_practice_entries_in(
        tx,
        account,
        &[(
            PracticeEntryKind::DeleteReversal,
            reversal,
            Some(run_id.to_string()),
            "删除本局冲销".to_string(),
        )],
        at,
    )
    .map(|_| ())
}
