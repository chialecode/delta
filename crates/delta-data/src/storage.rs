use crate::calendar::{generate_calendar_range, query_calendar_session};
use crate::registry::InstrumentRegistry;
use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{
    parse_decimal_lossless, to_canonical_text, AdjustmentKind, BarRecord, CalendarSession,
    CalendarSessionKind, CorporateActionRecord, CorporateActionType, CoverageGap, DataQualityIssue,
    FxRateRecord,
};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: i64 = 5;

const CORE_LIBRARY_TABLES: &[&str] = &[
    "library",
    "account",
    "portfolio",
    "portfolio_member",
    "asset",
    "instrument",
    "instrument_alias",
    "subscription",
    "fx_subscription",
    "provider_connection",
    "import_batch",
    "import_row",
    "source_record",
    "sync_checkpoint",
    "balance_snapshot",
    "reconciliation_issue",
    "economic_event",
    "journal_transaction",
    "posting",
    "cost_lot",
    "holding_projection",
    "calendar_session",
    "dataset_version",
    "dataset_partition",
    "dataset_publication",
    "event_book_fx",
    "bar",
    "corporate_action",
    "fx_rate",
    "coverage_gap",
    "data_quality_issue",
    "raw_payload",
    "analysis_report",
];

pub(crate) fn collect_library_table_counts(
    conn: &Connection,
) -> Result<BTreeMap<String, u64>, StorageError> {
    let mut counts = BTreeMap::new();
    for table in CORE_LIBRARY_TABLES
        .iter()
        .chain(crate::m2_schema::SCHEMA_4_TABLES.iter())
        .chain(crate::m4_schema::SCHEMA_5_TABLES.iter())
    {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            params![table],
            |r| r.get(0),
        )?;
        if exists {
            let cnt: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                    r.get(0)
                })?;
            counts.insert((*table).to_string(), u64::try_from(cnt).unwrap_or(0));
        }
    }
    Ok(counts)
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("decimal error: {0}")]
    Decimal(#[from] delta_core::DecimalError),
    #[error("restore target directory '{0}' is not empty")]
    RestoreTargetNotEmpty(String),
    #[error("backup or manifest integrity check failed: {0}")]
    IntegrityFailed(String),
    #[error("migration failed on database: {0}")]
    MigrationFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryManifest {
    pub schema_version: i64,
    pub library_id: String,
    pub book_currency: String,
    pub report_currency: String,
    pub timezone: String,
    pub created_at: String,
    pub updated_at: String,
    pub file_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub table_row_counts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCheckpointRecord {
    pub provider: String,
    pub resource_kind: String,
    pub resource_key: String,
    pub connection_id: Option<String>,
    pub cursor: String,
    pub last_committed_at: String,
    pub calendar_version: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetVersionRecord {
    pub id: String,
    pub version_num: i64,
    pub manifest_hash: String,
    pub source: String,
    pub terms_class: String,
    pub acquired_at: String,
    pub coverage_json: String,
    pub quality: String,
    pub temporal_quality: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationIssueRecord {
    pub id: String,
    pub account_id: String,
    pub asset_id: String,
    pub as_of: String,
    pub expected: String,
    pub observed: String,
    pub difference: String,
    pub reason: String,
    pub resolution_event_id: Option<String>,
}

pub struct SqliteStore {
    root_dir: PathBuf,
    db_path: PathBuf,
    conn: Connection,
}

impl SqliteStore {
    /// A consistent, private query copy. The live writer is never held across a model request.
    pub fn query_snapshot(&self) -> Result<Self, StorageError> {
        let mut conn = Connection::open_in_memory()?;
        {
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut conn)?;
            backup.run_to_completion(256, std::time::Duration::from_millis(1), None)?;
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            conn,
            root_dir: self.root_dir.clone(),
            db_path: PathBuf::from(":memory:"),
        })
    }
}

impl SqliteStore {
    pub fn open_or_init(
        root_dir: &Path,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<Self, StorageError> {
        fs::create_dir_all(root_dir)?;
        for sub in [
            "raw",
            "imports",
            "attachments",
            "backups",
            "logs",
            "cache",
            "settings",
        ] {
            fs::create_dir_all(root_dir.join(sub))?;
        }
        let db_path = root_dir.join("delta.sqlite");
        if db_path.exists() {
            // Check if file is a valid SQLite header before migrating; refuse to overwrite corrupted DB
            let metadata = fs::metadata(&db_path)?;
            if metadata.len() > 0 && metadata.len() < 100 {
                return Err(StorageError::MigrationFailed(
                    "existing delta.sqlite is corrupted (invalid header size); refusing to overwrite"
                        .to_string(),
                ));
            }
        }

        let conn = Connection::open(&db_path)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        // Quick integrity check before migration
        let quick_check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if quick_check != "ok" {
            return Err(StorageError::IntegrityFailed(format!(
                "PRAGMA quick_check failed: {quick_check}"
            )));
        }

        let mut store = Self {
            root_dir: root_dir.to_path_buf(),
            db_path,
            conn,
        };
        store.run_migrations(book_currency, report_currency, timezone)?;
        store.sync_manifest()?;
        Ok(store)
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn load_instrument_registry(&self) -> Result<InstrumentRegistry, StorageError> {
        let mut registry = InstrumentRegistry::new_seeded();
        let mut stmt = self.conn.prepare("SELECT id,kind,venue,symbol,base_asset_id,quote_asset_id,lot_size,tick_size,multiplier FROM instrument")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;
        for row in rows {
            let (id, kind, venue, symbol, base, quote, lot, tick, multiplier) = row?;
            let kind = match kind.as_str() {
                "spotequity" => delta_core::InstrumentKind::SpotEquity,
                "spotetf" => delta_core::InstrumentKind::SpotEtf,
                "spotcrypto" => delta_core::InstrumentKind::SpotCrypto,
                _ => {
                    return Err(StorageError::IntegrityFailed(
                        "invalid instrument kind".into(),
                    ))
                }
            };
            registry.instruments.insert(
                id.clone(),
                delta_core::Instrument {
                    id,
                    kind,
                    venue,
                    symbol,
                    base_asset_id: base,
                    quote_asset_id: quote,
                    lot_size: parse_decimal_lossless(&lot)?,
                    tick_size: parse_decimal_lossless(&tick)?,
                    multiplier: parse_decimal_lossless(&multiplier)?,
                },
            );
        }
        registry.aliases.clear();
        let mut stmt = self.conn.prepare("SELECT provider,external_symbol,instrument_id FROM instrument_alias ORDER BY provider,external_symbol")?;
        for row in stmt.query_map([], |r| {
            Ok(delta_core::InstrumentAlias {
                provider: r.get(0)?,
                external_symbol: r.get(1)?,
                instrument_id: r.get(2)?,
                valid_from: None,
                valid_to: None,
            })
        })? {
            registry.aliases.push(row?);
        }
        Ok(registry)
    }

    /// Explicit metadata avoids guessing listing venues from ticker spelling.
    pub fn register_market_instrument(
        &mut self,
        venue: &str,
        symbol: &str,
        etf: bool,
    ) -> Result<String, StorageError> {
        let venue = venue.to_ascii_uppercase();
        let symbol = symbol.to_ascii_uppercase();
        if !matches!(venue.as_str(), "NASDAQ" | "NYSE" | "OKX" | "BINANCE")
            || symbol.is_empty()
            || !symbol
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '/'))
        {
            return Err(StorageError::IntegrityFailed(
                "invalid market metadata".into(),
            ));
        }
        let crypto = matches!(venue.as_str(), "OKX" | "BINANCE");
        let (base, quote) = if crypto {
            symbol.split_once('/').ok_or_else(|| {
                StorageError::IntegrityFailed("crypto symbol must be BASE/USDT".into())
            })?
        } else {
            (symbol.as_str(), "USD")
        };
        if crypto && quote != "USDT" {
            return Err(StorageError::IntegrityFailed(
                "M1 crypto registrations require USDT quote".into(),
            ));
        }
        let id = format!("{venue}:{symbol}");
        let registry = self.load_instrument_registry()?;
        if registry.instruments.contains_key(&id) {
            return Ok(id);
        }
        let tx = self.conn.savepoint()?;
        tx.execute(
            "INSERT INTO instrument VALUES (?1,?2,?3,?4,?5,?6,?7,'0.0001','1')",
            params![
                id,
                if crypto {
                    "spotcrypto"
                } else if etf {
                    "spotetf"
                } else {
                    "spotequity"
                },
                venue,
                symbol,
                if crypto { base.to_string() } else { id.clone() },
                quote,
                if crypto { "0.00000001" } else { "1" }
            ],
        )?;
        let aliases: Vec<(String, String)> = if crypto {
            vec![(
                venue.to_ascii_lowercase(),
                if venue == "OKX" {
                    format!("{base}-{quote}")
                } else {
                    format!("{base}{quote}")
                },
            )]
        } else {
            let prefix = if etf {
                "107"
            } else if venue == "NASDAQ" {
                "105"
            } else {
                "106"
            };
            let mut aliases = vec![
                ("futu".into(), format!("US.{symbol}")),
                ("longbridge".into(), format!("{symbol}.US")),
                ("eastmoney".into(), format!("{prefix}.{symbol}")),
            ];
            for provider in ["yahoo", "tiingo", "ibkr", "tiger", "schwab", "alpaca"] {
                aliases.push((provider.into(), symbol.clone()));
            }
            aliases
        };
        for (provider, external) in aliases {
            tx.execute("INSERT INTO instrument_alias(provider,external_symbol,instrument_id) VALUES (?1,?2,?3)",params![provider,external,id])?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Seal a version against later mutation and hash the actual financial inputs.
    pub fn publish_dataset_version(&mut self, version: i64) -> Result<String, StorageError> {
        self.ensure_unpublished(version)?;
        let tx = self.conn.savepoint()?;
        let mut hasher = Sha256::new();
        for (table, sql) in [
            ("bar", "SELECT instrument_id,timeframe,session,session_date,open_at,close_at,open,high,low,close,volume,source,adjustment,is_final,COALESCE(raw_payload_hash,'') FROM bar WHERE valid_from_version <= ?1 AND (valid_to_version IS NULL OR valid_to_version > ?1) ORDER BY instrument_id,timeframe,session_date,source,adjustment,open_at"),
            ("fx_rate", "SELECT base_currency,quote_currency,effective_date,rate,effective_at,observed_at,source,quality,COALESCE(carried_from_date,''),dataset_version FROM fx_rate WHERE dataset_version <= ?1 ORDER BY base_currency,quote_currency,effective_date,source,dataset_version,id"),
            ("corporate_action", "SELECT instrument_id,ex_date,action_type,ratio_or_amount,COALESCE(pay_date,''),COALESCE(currency,''),COALESCE(announced_at,''),source,factor_version,COALESCE(raw_payload_hash,'') FROM corporate_action WHERE factor_version <= ?1 ORDER BY instrument_id,ex_date,action_type,source,factor_version,id"),
            ("metadata", "SELECT source,terms_class,acquired_at,coverage_json,quality,temporal_quality FROM dataset_version WHERE version_num=?1"),
        ] {
            hasher.update(table.as_bytes());
            let mut stmt = tx.prepare(sql)?;
            let columns = stmt.column_count();
            let mut rows = stmt.query(params![version])?;
            while let Some(row) = rows.next()? {
                let mut values = Vec::with_capacity(columns);
                for column in 0..columns {
                    values.push(match row.get_ref(column)? {
                        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                        rusqlite::types::ValueRef::Text(value) => std::str::from_utf8(value).map_err(|_| StorageError::IntegrityFailed("invalid dataset UTF-8".into()))?.to_string(),
                        _ => return Err(StorageError::IntegrityFailed("invalid dataset value type".into())),
                    });
                }
                hasher.update(serde_json::to_vec(&values)?);
                hasher.update(b"\n");
            }
        }
        let hash = hex::encode(hasher.finalize());
        tx.execute(
            "UPDATE dataset_version SET manifest_hash=?1 WHERE version_num=?2",
            params![hash, version],
        )?;
        tx.execute("UPDATE dataset_partition SET checksum=?1 WHERE version_id=(SELECT id FROM dataset_version WHERE version_num=?2)", params![hash,version])?;
        tx.execute(
            "INSERT INTO dataset_publication VALUES (?1,?2)",
            params![version, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(hash)
    }

    fn ensure_unpublished(&self, version: i64) -> Result<(), StorageError> {
        let known: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dataset_version WHERE version_num=?1)",
            params![version],
            |r| r.get(0),
        )?;
        if !known || version <= 0 {
            return Err(StorageError::IntegrityFailed(
                "unknown dataset version".into(),
            ));
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='dataset_publication')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            let sealed: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dataset_publication WHERE version_num>=?1)",
                params![version],
                |r| r.get(0),
            )?;
            if sealed {
                return Err(StorageError::IntegrityFailed(
                    "published dataset is immutable".into(),
                ));
            }
        }
        Ok(())
    }

    fn run_migrations(
        &mut self,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<(), StorageError> {
        let has_library: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='library')",
            [],
            |r| r.get(0),
        )?;
        if has_library {
            let schema: i64 =
                self.conn
                    .query_row("SELECT schema_version FROM library LIMIT 1", [], |r| {
                        r.get(0)
                    })?;
            if schema > CURRENT_SCHEMA_VERSION {
                return Err(StorageError::MigrationFailed(
                    "database schema is newer than this build".into(),
                ));
            }
            if schema < CURRENT_SCHEMA_VERSION {
                let before = self
                    .root_dir
                    .join("backups")
                    .join(format!("before-schema-{schema}-{}", uuid::Uuid::new_v4()));
                self.create_consistent_backup(&before)?;
            }
        }
        let tx = self.conn.savepoint()?;
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS library (
                id TEXT PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                book_currency TEXT NOT NULL,
                report_currency TEXT NOT NULL,
                timezone TEXT NOT NULL,
                ledger_revision INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS account (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                type TEXT NOT NULL,
                provider TEXT NOT NULL,
                external_ref TEXT,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS portfolio (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS portfolio_member (
                portfolio_id TEXT NOT NULL,
                account_id TEXT NOT NULL,
                valid_from TEXT,
                valid_to TEXT,
                PRIMARY KEY (portfolio_id, account_id),
                FOREIGN KEY (portfolio_id) REFERENCES portfolio(id),
                FOREIGN KEY (account_id) REFERENCES account(id)
            );

            CREATE TABLE IF NOT EXISTS asset (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                symbol TEXT NOT NULL,
                network TEXT,
                contract_address TEXT,
                precision INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS instrument (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                base_asset_id TEXT NOT NULL,
                quote_asset_id TEXT NOT NULL,
                lot_size TEXT NOT NULL,
                tick_size TEXT NOT NULL,
                multiplier TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS instrument_alias (
                provider TEXT NOT NULL,
                external_symbol TEXT NOT NULL,
                instrument_id TEXT NOT NULL,
                valid_from TEXT,
                valid_to TEXT,
                PRIMARY KEY (provider, external_symbol),
                FOREIGN KEY (instrument_id) REFERENCES instrument(id)
            );

            CREATE TABLE IF NOT EXISTS subscription (
                instrument_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                auto_reason TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS fx_subscription (
                base_currency TEXT NOT NULL,
                quote_currency TEXT NOT NULL,
                auto_reason TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (base_currency, quote_currency)
            );

            CREATE TABLE IF NOT EXISTS provider_connection (
                id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                credential_ref TEXT,
                base_url TEXT NOT NULL,
                capabilities_json TEXT NOT NULL,
                status TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS import_batch (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                source TEXT NOT NULL,
                file_hash TEXT NOT NULL,
                mapping_version TEXT NOT NULL,
                status TEXT NOT NULL,
                counts_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (account_id) REFERENCES account(id)
            );
            CREATE INDEX IF NOT EXISTS idx_import_batch_acct_hash ON import_batch(account_id, file_hash);

            CREATE TABLE IF NOT EXISTS import_row (
                id TEXT PRIMARY KEY,
                batch_id TEXT NOT NULL,
                row_no INTEGER NOT NULL,
                raw_payload TEXT NOT NULL,
                normalized_payload TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                validation_state TEXT NOT NULL,
                error_reason TEXT,
                event_id TEXT,
                FOREIGN KEY (batch_id) REFERENCES import_batch(id)
            );
            CREATE INDEX IF NOT EXISTS idx_import_row_fingerprint ON import_row(fingerprint);

            CREATE TABLE IF NOT EXISTS source_record (
                provider TEXT NOT NULL,
                account_id TEXT NOT NULL,
                record_type TEXT NOT NULL,
                external_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                payload_hash TEXT NOT NULL,
                PRIMARY KEY (provider, account_id, record_type, external_id)
            );

            CREATE TABLE IF NOT EXISTS sync_checkpoint (
                provider TEXT NOT NULL,
                resource_kind TEXT NOT NULL,
                resource_key TEXT NOT NULL,
                connection_id TEXT,
                cursor TEXT NOT NULL,
                last_committed_at TEXT NOT NULL,
                calendar_version TEXT,
                base_url TEXT,
                PRIMARY KEY (provider, resource_kind, resource_key)
            );

            CREATE TABLE IF NOT EXISTS balance_snapshot (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                asset_id TEXT NOT NULL,
                quantity TEXT NOT NULL,
                as_of TEXT NOT NULL,
                source TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reconciliation_issue (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                asset_id TEXT NOT NULL,
                as_of TEXT NOT NULL,
                expected TEXT NOT NULL,
                observed TEXT NOT NULL,
                difference TEXT NOT NULL,
                reason TEXT NOT NULL,
                resolution_event_id TEXT
            );

            CREATE TABLE IF NOT EXISTS economic_event (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                type TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                sequence_no INTEGER NOT NULL,
                recorded_at TEXT NOT NULL,
                asset_id TEXT NOT NULL,
                instrument_id TEXT,
                quantity TEXT NOT NULL,
                price TEXT,
                quote_currency TEXT NOT NULL,
                fee_amount TEXT NOT NULL,
                fee_currency TEXT NOT NULL,
                cost_known INTEGER NOT NULL DEFAULT 1,
                counterparty_account_id TEXT,
                transfer_group_id TEXT,
                source_ref TEXT,
                correction_group_id TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 1,
                active_in_correction_group INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_event_acct_time ON economic_event(account_id, occurred_at);
            CREATE INDEX IF NOT EXISTS idx_event_correction_group ON economic_event(correction_group_id);

            CREATE TABLE IF NOT EXISTS journal_transaction (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL,
                book_currency TEXT NOT NULL,
                posting_rule_version TEXT NOT NULL,
                reversal_of TEXT,
                FOREIGN KEY (event_id) REFERENCES economic_event(id)
            );

            CREATE TABLE IF NOT EXISTS posting (
                id TEXT PRIMARY KEY,
                transaction_id TEXT NOT NULL,
                ledger_account_id TEXT NOT NULL,
                category TEXT NOT NULL,
                native_asset_id TEXT NOT NULL,
                native_quantity TEXT NOT NULL,
                book_amount TEXT NOT NULL,
                FOREIGN KEY (transaction_id) REFERENCES journal_transaction(id)
            );

            CREATE TABLE IF NOT EXISTS cost_lot (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                asset_id TEXT NOT NULL,
                instrument_id TEXT,
                source_fill_id TEXT NOT NULL,
                acquired_at TEXT NOT NULL,
                original_quantity TEXT NOT NULL,
                remaining_quantity TEXT NOT NULL,
                original_cost_book TEXT NOT NULL,
                remaining_cost_book TEXT NOT NULL,
                cost_known INTEGER NOT NULL,
                ledger_revision INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS holding_projection (
                account_id TEXT NOT NULL,
                asset_id TEXT NOT NULL,
                quantity TEXT NOT NULL,
                carrying_cost TEXT NOT NULL,
                cost_known INTEGER NOT NULL,
                ledger_revision INTEGER NOT NULL,
                PRIMARY KEY (account_id, asset_id)
            );

            CREATE TABLE IF NOT EXISTS calendar_session (
                venue TEXT NOT NULL,
                session_date TEXT NOT NULL,
                timezone TEXT NOT NULL,
                open_at TEXT,
                close_at TEXT,
                kind TEXT NOT NULL,
                version TEXT NOT NULL,
                PRIMARY KEY (venue, session_date)
            );

            CREATE TABLE IF NOT EXISTS dataset_version (
                id TEXT PRIMARY KEY,
                version_num INTEGER NOT NULL UNIQUE,
                manifest_hash TEXT NOT NULL,
                source TEXT NOT NULL,
                terms_class TEXT NOT NULL,
                acquired_at TEXT NOT NULL,
                coverage_json TEXT NOT NULL,
                quality TEXT NOT NULL,
                temporal_quality TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS dataset_partition (
                id TEXT PRIMARY KEY,
                version_id TEXT NOT NULL,
                instrument_range TEXT NOT NULL,
                time_range TEXT NOT NULL,
                location_kind TEXT NOT NULL,
                location TEXT NOT NULL,
                checksum TEXT NOT NULL,
                FOREIGN KEY (version_id) REFERENCES dataset_version(id)
            );

            CREATE TABLE IF NOT EXISTS bar (
                id TEXT PRIMARY KEY,
                instrument_id TEXT NOT NULL,
                timeframe TEXT NOT NULL,
                session TEXT NOT NULL,
                session_date TEXT NOT NULL,
                open_at TEXT NOT NULL,
                close_at TEXT NOT NULL,
                open TEXT NOT NULL,
                high TEXT NOT NULL,
                low TEXT NOT NULL,
                close TEXT NOT NULL,
                volume TEXT NOT NULL,
                source TEXT NOT NULL,
                adjustment TEXT NOT NULL,
                is_final INTEGER NOT NULL,
                valid_from_version INTEGER NOT NULL,
                valid_to_version INTEGER,
                raw_payload_hash TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_bar_lookup
                ON bar(instrument_id, timeframe, adjustment, source, open_at, valid_from_version, valid_to_version);
            CREATE INDEX IF NOT EXISTS idx_bar_session_date
                ON bar(instrument_id, session_date, valid_from_version, valid_to_version);

            CREATE TABLE IF NOT EXISTS corporate_action (
                id TEXT PRIMARY KEY,
                instrument_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                ex_date TEXT NOT NULL,
                pay_date TEXT,
                ratio_or_amount TEXT NOT NULL,
                currency TEXT,
                announced_at TEXT,
                source TEXT NOT NULL,
                factor_version INTEGER NOT NULL,
                raw_payload_hash TEXT,
                UNIQUE(instrument_id, action_type, ex_date, source, factor_version)
            );

            CREATE TABLE IF NOT EXISTS fx_rate (
                id TEXT PRIMARY KEY,
                base_currency TEXT NOT NULL,
                quote_currency TEXT NOT NULL,
                rate TEXT NOT NULL,
                effective_date TEXT NOT NULL,
                effective_at TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                source TEXT NOT NULL,
                quality TEXT NOT NULL,
                carried_from_date TEXT,
                dataset_version INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_fx_pair_date
                ON fx_rate(base_currency, quote_currency, effective_date, dataset_version);

            CREATE TABLE IF NOT EXISTS coverage_gap (
                id TEXT PRIMARY KEY,
                dataset_version INTEGER NOT NULL,
                instrument_id TEXT NOT NULL,
                start_date TEXT NOT NULL,
                end_date TEXT NOT NULL,
                gap_kind TEXT NOT NULL,
                resolution TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS data_quality_issue (
                id TEXT PRIMARY KEY,
                dataset_version INTEGER NOT NULL,
                instrument_id TEXT NOT NULL,
                session_date TEXT NOT NULL,
                kind TEXT NOT NULL,
                sources TEXT NOT NULL,
                detail TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS raw_payload (
                content_hash TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                acquired_at TEXT NOT NULL,
                content_type TEXT NOT NULL,
                body TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS analysis_report (
                id TEXT PRIMARY KEY,
                report_type TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                ledger_revision INTEGER NOT NULL,
                dataset_version INTEGER NOT NULL,
                calculation_version TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )?;

        let existing_lib: Option<String> = tx
            .query_row("SELECT id FROM library LIMIT 1", [], |row| row.get(0))
            .optional()?;
        if existing_lib.is_none() {
            let now = Utc::now().to_rfc3339();
            tx.execute(
                "INSERT INTO library (id, schema_version, book_currency, report_currency, timezone, ledger_revision, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    CURRENT_SCHEMA_VERSION,
                    book_currency.to_ascii_uppercase(),
                    report_currency.to_ascii_uppercase(),
                    timezone,
                    now,
                ],
            )?;
        }

        // Ordered v1 -> v2 migration; historical journal revisions remain addressable.
        let has_revision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('journal_transaction') WHERE name='ledger_revision')",[],|r|r.get(0))?;
        if !has_revision {
            tx.execute_batch("ALTER TABLE journal_transaction ADD COLUMN ledger_revision INTEGER NOT NULL DEFAULT 1")?;
        }
        tx.execute_batch("CREATE TABLE IF NOT EXISTS dataset_publication (version_num INTEGER PRIMARY KEY, published_at TEXT NOT NULL)")?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS event_book_fx (event_id TEXT NOT NULL, currency TEXT NOT NULL, rate TEXT NOT NULL, source_date TEXT NOT NULL, dataset_version INTEGER NOT NULL, PRIMARY KEY(event_id,currency), FOREIGN KEY(event_id) REFERENCES economic_event(id))")?;
        let schema: i64 = tx.query_row("SELECT schema_version FROM library LIMIT 1", [], |r| {
            r.get(0)
        })?;
        if schema > CURRENT_SCHEMA_VERSION {
            return Err(StorageError::MigrationFailed(
                "database schema is newer than this build".into(),
            ));
        }
        if schema < 3 {
            // Old per-side source keys must continue to reject a changed BUY/SELL
            // or deposit/withdrawal payload with the same source business ID.
            tx.execute_batch("UPDATE source_record SET record_type=CASE WHEN record_type IN ('buy','sell') THEN 'trade' WHEN record_type IN ('deposit','withdrawal','transfer_in','transfer_out') THEN 'cash_movement' WHEN record_type IN ('position_transfer_in','position_transfer_out') THEN 'position_transfer' ELSE record_type END; UPDATE analysis_report SET status='stale';")?;
            let old_id: String =
                tx.query_row("SELECT id FROM library LIMIT 1", [], |r| r.get(0))?;
            if old_id == "LIB-DEFAULT" {
                tx.execute(
                    "UPDATE library SET id=?1 WHERE id='LIB-DEFAULT'",
                    params![uuid::Uuid::new_v4().to_string()],
                )?;
            }
        }
        crate::m2_schema::apply_schema_4_ddl(&tx, schema)?;
        crate::m4_schema::apply_schema_5_ddl(&tx, schema)?;
        tx.execute(
            "UPDATE library SET schema_version=?1",
            params![CURRENT_SCHEMA_VERSION],
        )?;

        // Seed default InstrumentRegistry into asset, instrument, instrument_alias
        let reg = InstrumentRegistry::new_seeded();
        for asset in reg.assets.values() {
            tx.execute(
                "INSERT OR IGNORE INTO asset (id, kind, symbol, network, contract_address, precision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    asset.id,
                    format!("{:?}", asset.kind).to_ascii_lowercase(),
                    asset.symbol,
                    asset.network,
                    asset.contract_address,
                    asset.precision,
                ],
            )?;
        }
        for inst in reg.instruments.values() {
            tx.execute(
                "INSERT OR IGNORE INTO instrument (id, kind, venue, symbol, base_asset_id, quote_asset_id, lot_size, tick_size, multiplier)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    inst.id,
                    format!("{:?}", inst.kind).to_ascii_lowercase(),
                    inst.venue,
                    inst.symbol,
                    inst.base_asset_id,
                    inst.quote_asset_id,
                    to_canonical_text(inst.lot_size),
                    to_canonical_text(inst.tick_size),
                    to_canonical_text(inst.multiplier),
                ],
            )?;
        }
        for alias in &reg.aliases {
            tx.execute(
                "INSERT OR IGNORE INTO instrument_alias (provider, external_symbol, instrument_id, valid_from, valid_to)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    alias.provider,
                    alias.external_symbol,
                    alias.instrument_id,
                    alias.valid_from.map(|d| d.to_string()),
                    alias.valid_to.map(|d| d.to_string()),
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Populate `calendar_session` table for a given venue and date range.
    pub fn seed_calendar_range(
        &mut self,
        venue: &str,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<usize, StorageError> {
        let sessions = generate_calendar_range(venue, start, end);
        let tx = self.conn.savepoint()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO calendar_session (venue, session_date, timezone, open_at, close_at, kind, version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for s in &sessions {
                stmt.execute(params![
                    s.venue,
                    s.session_date.to_string(),
                    s.timezone,
                    s.open_at.map(|t| t.to_rfc3339()),
                    s.close_at.map(|t| t.to_rfc3339()),
                    s.kind.as_str(),
                    s.version,
                ])?;
            }
        }
        tx.commit()?;
        Ok(sessions.len())
    }

    /// Store raw provider HTTP response by content SHA-256 in both `raw_payload` table and `raw/<hash>.json`.
    pub fn store_raw_payload(
        &mut self,
        provider: &str,
        content_type: &str,
        body: &str,
    ) -> Result<String, StorageError> {
        self.store_raw_bytes(provider, content_type, body.as_bytes())
    }

    pub fn store_raw_bytes(
        &mut self,
        provider: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<String, StorageError> {
        let mut hasher = Sha256::new();
        hasher.update(body);
        let content_hash = hex::encode(hasher.finalize());
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO raw_payload (content_hash, provider, acquired_at, content_type, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![content_hash, provider, now, content_type, body],
        )?;
        let raw_file = self
            .root_dir
            .join("raw")
            .join(format!("{content_hash}.raw"));
        if !raw_file.exists() {
            fs::write(&raw_file, body)?;
        }
        Ok(content_hash)
    }

    /// Allocate the next monotonically increasing `dataset_version` (S-09).
    pub fn create_dataset_version(
        &mut self,
        source: &str,
        terms_class: &str,
        coverage_json: &str,
        quality: &str,
        temporal_quality: &str,
    ) -> Result<DatasetVersionRecord, StorageError> {
        let next_ver: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(version_num), 0) + 1 FROM dataset_version",
            [],
            |r| r.get(0),
        )?;
        let id = format!("DSV-{next_ver:06}");
        let acquired_at = Utc::now().to_rfc3339();
        let mut hasher = Sha256::new();
        hasher.update(
            format!("{id}:{source}:{terms_class}:{coverage_json}:{acquired_at}").as_bytes(),
        );
        let manifest_hash = hex::encode(hasher.finalize());

        let tx = self.conn.savepoint()?;
        tx.execute(
            "INSERT INTO dataset_version (id, version_num, manifest_hash, source, terms_class, acquired_at, coverage_json, quality, temporal_quality)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                next_ver,
                manifest_hash,
                source,
                terms_class,
                acquired_at,
                coverage_json,
                quality,
                temporal_quality,
            ],
        )?;
        // Record dataset_partition with location_kind = 'sqlite' (S-01)
        tx.execute(
            "INSERT INTO dataset_partition (id, version_id, instrument_range, time_range, location_kind, location, checksum)
             VALUES (?1, ?2, 'all', ?3, 'sqlite', 'delta.sqlite#bar', ?4)",
            params![
                format!("PART-{next_ver:06}"),
                id,
                coverage_json,
                manifest_hash,
            ],
        )?;
        tx.commit()?;

        Ok(DatasetVersionRecord {
            id,
            version_num: next_ver,
            manifest_hash,
            source: source.to_string(),
            terms_class: terms_class.to_string(),
            acquired_at,
            coverage_json: coverage_json.to_string(),
            quality: quality.to_string(),
            temporal_quality: temporal_quality.to_string(),
        })
    }

    /// Upsert bars with S-09 version interval tracking (`valid_from_version` / `valid_to_version`)
    /// and atomic `sync_checkpoint` progression in the SAME transaction!
    pub fn commit_bars_and_checkpoint(
        &mut self,
        bars: &[BarRecord],
        dataset_version: i64,
        checkpoint: Option<&SyncCheckpointRecord>,
    ) -> Result<(usize, Vec<DataQualityIssue>), StorageError> {
        self.ensure_unpublished(dataset_version)?;
        let tx = self.conn.savepoint()?;
        let mut inserted_or_revised = 0usize;
        let mut revision_issues = Vec::new();

        for bar in bars {
            // Only commit finalized bars (`is_final == true`) for daily sync
            if !bar.is_final {
                continue;
            }
            if bar.close <= Decimal::ZERO
                || bar.open <= Decimal::ZERO
                || bar.volume < Decimal::ZERO
                || bar.high < bar.open.max(bar.close)
                || bar.low > bar.open.min(bar.close)
                || bar.low > bar.high
                || bar.close_at <= bar.open_at
            {
                return Err(StorageError::IntegrityFailed(
                    "invalid finalized OHLCV bar".into(),
                ));
            }
            let open_str = bar.open_at.to_rfc3339();
            let adj_str = bar.adjustment.as_str();

            let existing: Option<(String, String, String, String, String, String)> = tx
                .query_row(
                    "SELECT id, open, high, low, close, volume FROM bar
                     WHERE instrument_id = ?1 AND timeframe = ?2 AND adjustment = ?3
                       AND source = ?4 AND open_at = ?5 AND valid_to_version IS NULL",
                    params![
                        bar.instrument_id,
                        bar.timeframe,
                        adj_str,
                        bar.source,
                        open_str
                    ],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()?;

            let new_open = to_canonical_text(bar.open);
            let new_high = to_canonical_text(bar.high);
            let new_low = to_canonical_text(bar.low);
            let new_close = to_canonical_text(bar.close);
            let new_vol = to_canonical_text(bar.volume);

            if let Some((old_id, old_o, old_h, old_l, old_c, old_v)) = existing {
                if old_o == new_open
                    && old_h == new_high
                    && old_l == new_low
                    && old_c == new_close
                    && old_v == new_vol
                {
                    // Identical content: idempotent skip without duplicating row
                    continue;
                }
                // Content changed in overlap window! Close old version interval and record data_revision (S-09)
                tx.execute(
                    "UPDATE bar SET valid_to_version = ?1 WHERE id = ?2",
                    params![dataset_version, old_id],
                )?;
                let issue = DataQualityIssue {
                    id: format!(
                        "DQI:REV:{}:{}:{dataset_version}",
                        bar.instrument_id, bar.session_date
                    ),
                    dataset_version,
                    instrument_id: bar.instrument_id.clone(),
                    session_date: bar.session_date,
                    kind: "data_revision".to_string(),
                    sources: bar.source.clone(),
                    detail: format!(
                        "bar revised at version {dataset_version}: close {old_c} -> {new_close}, volume {old_v} -> {new_vol}"
                    ),
                };
                tx.execute(
                    "INSERT OR REPLACE INTO data_quality_issue (id, dataset_version, instrument_id, session_date, kind, sources, detail)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        issue.id,
                        issue.dataset_version,
                        issue.instrument_id,
                        issue.session_date.to_string(),
                        issue.kind,
                        issue.sources,
                        issue.detail,
                    ],
                )?;
                revision_issues.push(issue);
            }

            let row_id = format!(
                "BAR:{}:{}:{}:{}:{}:{dataset_version}",
                bar.instrument_id, bar.timeframe, adj_str, bar.source, bar.session_date
            );
            tx.execute(
                "INSERT INTO bar (
                    id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, NULL, ?17)",
                params![
                    row_id,
                    bar.instrument_id,
                    bar.timeframe,
                    bar.session,
                    bar.session_date.to_string(),
                    open_str,
                    bar.close_at.to_rfc3339(),
                    new_open,
                    new_high,
                    new_low,
                    new_close,
                    new_vol,
                    bar.source,
                    adj_str,
                    if bar.is_final { 1 } else { 0 },
                    dataset_version,
                    bar.raw_payload_hash,
                ],
            )?;
            inserted_or_revised += 1;
        }

        if let Some(cp) = checkpoint {
            tx.execute(
                "INSERT OR REPLACE INTO sync_checkpoint (
                    provider, resource_kind, resource_key, connection_id, cursor,
                    last_committed_at, calendar_version, base_url
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    cp.provider,
                    cp.resource_kind,
                    cp.resource_key,
                    cp.connection_id,
                    cp.cursor,
                    cp.last_committed_at,
                    cp.calendar_version,
                    cp.base_url,
                ],
            )?;
        }

        tx.commit()?;
        Ok((inserted_or_revised, revision_issues))
    }

    /// Query bars at a specific `dataset_version` (`valid_from_version <= v AND (valid_to_version IS NULL OR v < valid_to_version)`)
    /// with optional `visible_until` replay truncation (S-09 & AC-11).
    pub fn query_bars_at_version(
        &self,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        source: Option<&str>,
        version: i64,
        visible_until: Option<DateTime<Utc>>,
    ) -> Result<Vec<BarRecord>, StorageError> {
        let adj_str = adjustment.as_str();
        let cutoff_str = visible_until.map(|dt| dt.to_rfc3339());
        let sql_with_source =
            "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
             FROM bar
             WHERE instrument_id = ?1
               AND timeframe = ?2
               AND adjustment = ?3
               AND source = ?4
               AND valid_from_version <= ?5
               AND (valid_to_version IS NULL OR ?5 < valid_to_version)
               AND (?6 IS NULL OR strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?6))
             ORDER BY open_at ASC";
        let sql_any_source =
            "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
             FROM bar
             WHERE instrument_id = ?1
               AND timeframe = ?2
               AND adjustment = ?3
               AND valid_from_version <= ?4
               AND (valid_to_version IS NULL OR ?4 < valid_to_version)
               AND (?5 IS NULL OR strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?5))
             ORDER BY open_at ASC";

        let parse_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Option<BarRecord>> {
            let s_date_str = r.get_ref(4)?.as_str()?;
            let open_str = r.get_ref(5)?.as_str()?;
            let close_str = r.get_ref(6)?.as_str()?;
            let o_str = r.get_ref(7)?.as_str()?;
            let h_str = r.get_ref(8)?.as_str()?;
            let l_str = r.get_ref(9)?.as_str()?;
            let c_str = r.get_ref(10)?.as_str()?;
            let v_str = r.get_ref(11)?.as_str()?;
            let adj_raw = r.get_ref(13)?.as_str()?;
            let is_fin: i64 = r.get(14)?;

            let close_at = DateTime::parse_from_rfc3339(close_str)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            if let Some(cutoff) = visible_until {
                if close_at > cutoff {
                    return Ok(None);
                }
            }
            let session_date = NaiveDate::parse_from_str(s_date_str, "%Y-%m-%d").map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let open_at = DateTime::parse_from_rfc3339(open_str)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

            let map_dec = |s: &str| {
                parse_decimal_lossless(s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            };

            Ok(Some(BarRecord {
                id: r.get(0)?,
                instrument_id: r.get(1)?,
                timeframe: r.get(2)?,
                session: r.get(3)?,
                session_date,
                open_at,
                close_at,
                open: map_dec(o_str)?,
                high: map_dec(h_str)?,
                low: map_dec(l_str)?,
                close: map_dec(c_str)?,
                volume: map_dec(v_str)?,
                source: r.get(12)?,
                adjustment: AdjustmentKind::parse(adj_raw).unwrap_or(AdjustmentKind::Raw),
                is_final: is_fin != 0,
                valid_from_version: r.get(15)?,
                valid_to_version: r.get(16)?,
                raw_payload_hash: r.get(17)?,
            }))
        };

        let mut out = Vec::new();
        if let Some(src) = source {
            let mut stmt = self.conn.prepare_cached(sql_with_source)?;
            let rows = stmt.query_map(
                params![instrument_id, timeframe, adj_str, src, version, cutoff_str],
                parse_row,
            )?;
            for row in rows {
                if let Some(rec) = row? {
                    out.push(rec);
                }
            }
        } else {
            let mut stmt = self.conn.prepare_cached(sql_any_source)?;
            let rows = stmt.query_map(
                params![instrument_id, timeframe, adj_str, version, cutoff_str],
                parse_row,
            )?;
            for row in rows {
                if let Some(rec) = row? {
                    out.push(rec);
                }
            }
        }
        Ok(out)
    }

    pub fn upsert_corporate_actions(
        &mut self,
        actions: &[CorporateActionRecord],
    ) -> Result<usize, StorageError> {
        for action in actions {
            self.ensure_unpublished(action.factor_version)?;
            let previous: Option<i64> = self
                .conn
                .query_row(
                    "SELECT factor_version FROM corporate_action WHERE id=?1",
                    params![action.id],
                    |r| r.get(0),
                )
                .optional()?;
            if previous.is_some_and(|version| version != action.factor_version) {
                return Err(StorageError::IntegrityFailed(
                    "company action ID belongs to another version".into(),
                ));
            }
            if action.ratio_or_amount < Decimal::ZERO
                || (action.action_type == CorporateActionType::Split
                    && action.ratio_or_amount.is_zero())
            {
                return Err(StorageError::IntegrityFailed(
                    "invalid company action amount".into(),
                ));
            }
        }
        let tx = self.conn.savepoint()?;
        let mut count = 0;
        for act in actions {
            tx.execute(
                "INSERT OR REPLACE INTO corporate_action (
                    id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    act.id,
                    act.instrument_id,
                    act.action_type.as_str(),
                    act.ex_date.to_string(),
                    act.pay_date.map(|d| d.to_string()),
                    to_canonical_text(act.ratio_or_amount),
                    act.currency,
                    act.announced_at.map(|t| t.to_rfc3339()),
                    act.source,
                    act.factor_version,
                    act.raw_payload_hash,
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn query_corporate_actions(
        &self,
        instrument_id: &str,
        max_factor_version: Option<i64>,
    ) -> Result<Vec<CorporateActionRecord>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
             FROM corporate_action
             WHERE instrument_id = ?1 AND (?2 IS NULL OR factor_version <= ?2)
               AND factor_version = (SELECT MAX(c.factor_version) FROM corporate_action c
                 WHERE c.instrument_id = corporate_action.instrument_id
                   AND c.action_type = corporate_action.action_type
                   AND c.ex_date = corporate_action.ex_date AND c.source = corporate_action.source
                   AND (?2 IS NULL OR c.factor_version <= ?2))
             ORDER BY ex_date ASC",
        )?;
        let rows = stmt.query_map(params![instrument_id, max_factor_version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, Option<String>>(10)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, inst, act_type, ex_str, pay_str, ratio_str, ccy, ann_str, src, fv, rh) = row?;
            out.push(CorporateActionRecord {
                id,
                instrument_id: inst,
                action_type: match act_type.as_str() {
                    "split" => CorporateActionType::Split,
                    "cash_dividend" => CorporateActionType::CashDividend,
                    _ => {
                        return Err(StorageError::IntegrityFailed(
                            "unknown company action type".into(),
                        ))
                    }
                },
                ex_date: NaiveDate::parse_from_str(&ex_str, "%Y-%m-%d").map_err(|_| {
                    StorageError::IntegrityFailed("invalid company action date".into())
                })?,
                pay_date: pay_str
                    .map(|s| {
                        NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| {
                            StorageError::IntegrityFailed("invalid company action pay date".into())
                        })
                    })
                    .transpose()?,
                ratio_or_amount: parse_decimal_lossless(&ratio_str)?,
                currency: ccy,
                announced_at: ann_str
                    .map(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .map_err(|_| {
                                StorageError::IntegrityFailed(
                                    "invalid company action announcement time".into(),
                                )
                            })
                    })
                    .transpose()?,
                source: src,
                factor_version: fv,
                raw_payload_hash: rh,
            });
        }
        Ok(out)
    }

    pub fn upsert_fx_rates(
        &mut self,
        rates: &[FxRateRecord],
        checkpoint: Option<&SyncCheckpointRecord>,
    ) -> Result<usize, StorageError> {
        for rate in rates {
            self.ensure_unpublished(rate.dataset_version)?;
            let previous: Option<i64> = self
                .conn
                .query_row(
                    "SELECT dataset_version FROM fx_rate WHERE id=?1",
                    params![rate.id],
                    |r| r.get(0),
                )
                .optional()?;
            if previous.is_some_and(|version| version != rate.dataset_version) {
                return Err(StorageError::IntegrityFailed(
                    "FX ID belongs to another version".into(),
                ));
            }
            if rate.rate <= Decimal::ZERO {
                return Err(StorageError::IntegrityFailed(
                    "FX rate must be positive".into(),
                ));
            }
        }
        let tx = self.conn.savepoint()?;
        let mut count = 0;
        for r in rates {
            tx.execute(
                "INSERT OR REPLACE INTO fx_rate (
                    id, base_currency, quote_currency, rate, effective_date,
                    effective_at, observed_at, source, quality, carried_from_date, dataset_version
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    r.id,
                    r.base_currency.to_ascii_uppercase(),
                    r.quote_currency.to_ascii_uppercase(),
                    to_canonical_text(r.rate),
                    r.effective_date.to_string(),
                    r.effective_at.to_rfc3339(),
                    r.observed_at.to_rfc3339(),
                    r.source,
                    r.quality.as_str(),
                    r.carried_from_date.map(|d| d.to_string()),
                    r.dataset_version,
                ],
            )?;
            count += 1;
        }
        if let Some(cp) = checkpoint {
            tx.execute(
                "INSERT OR REPLACE INTO sync_checkpoint (
                    provider, resource_kind, resource_key, connection_id, cursor,
                    last_committed_at, calendar_version, base_url
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    cp.provider,
                    cp.resource_kind,
                    cp.resource_key,
                    cp.connection_id,
                    cp.cursor,
                    cp.last_committed_at,
                    cp.calendar_version,
                    cp.base_url,
                ],
            )?;
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn get_sync_checkpoint(
        &self,
        provider: &str,
        resource_kind: &str,
        resource_key: &str,
    ) -> Result<Option<SyncCheckpointRecord>, StorageError> {
        self.conn
            .query_row(
                "SELECT provider, resource_kind, resource_key, connection_id, cursor,
                        last_committed_at, calendar_version, base_url
                 FROM sync_checkpoint
                 WHERE provider = ?1 AND resource_kind = ?2 AND resource_key = ?3",
                params![provider, resource_kind, resource_key],
                |r| {
                    Ok(SyncCheckpointRecord {
                        provider: r.get(0)?,
                        resource_kind: r.get(1)?,
                        resource_key: r.get(2)?,
                        connection_id: r.get(3)?,
                        cursor: r.get(4)?,
                        last_committed_at: r.get(5)?,
                        calendar_version: r.get(6)?,
                        base_url: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn record_quality_issues(
        &mut self,
        issues: &[DataQualityIssue],
    ) -> Result<(), StorageError> {
        let tx = self.conn.savepoint()?;
        for issue in issues {
            tx.execute(
                "INSERT OR REPLACE INTO data_quality_issue (
                    id, dataset_version, instrument_id, session_date, kind, sources, detail
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    issue.id,
                    issue.dataset_version,
                    issue.instrument_id,
                    issue.session_date.to_string(),
                    issue.kind,
                    issue.sources,
                    issue.detail,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_quality_issues(&self) -> Result<Vec<DataQualityIssue>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dataset_version, instrument_id, session_date, kind, sources, detail
             FROM data_quality_issue ORDER BY session_date ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let d_str: String = r.get(3)?;
            Ok(DataQualityIssue {
                id: r.get(0)?,
                dataset_version: r.get(1)?,
                instrument_id: r.get(2)?,
                session_date: NaiveDate::parse_from_str(&d_str, "%Y-%m-%d")
                    .unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
                kind: r.get(4)?,
                sources: r.get(5)?,
                detail: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Analyze calendar sessions vs stored bars in `[start, end]` to distinguish
    /// market closed (holiday/weekend), `coverage_gap` (`provider_missing`), and `calendar_unknown` (M1-W02).
    pub fn analyze_coverage_gaps(
        &mut self,
        venue: &str,
        instrument_id: &str,
        start: NaiveDate,
        end: NaiveDate,
        dataset_version: i64,
    ) -> Result<(Vec<CalendarSession>, Vec<CoverageGap>), StorageError> {
        let bars = self.query_bars_at_version(
            instrument_id,
            "1d",
            AdjustmentKind::Raw,
            None,
            dataset_version,
            None,
        )?;
        let present_dates: std::collections::HashSet<NaiveDate> =
            bars.into_iter().map(|b| b.session_date).collect();

        let mut sessions = Vec::new();
        let mut gaps = Vec::new();
        let mut cur = start;
        while cur <= end {
            let sess = query_calendar_session(venue, cur);
            match sess.kind {
                CalendarSessionKind::Closed => {
                    // Market closed: not a coverage gap!
                }
                CalendarSessionKind::CalendarUnknown => {
                    gaps.push(CoverageGap {
                        id: format!("GAP:UNKNOWN:{instrument_id}:{cur}"),
                        dataset_version,
                        instrument_id: instrument_id.to_string(),
                        start_date: cur,
                        end_date: cur,
                        gap_kind: "calendar_unknown".to_string(),
                        resolution: "unverified_calendar_range".to_string(),
                    });
                }
                CalendarSessionKind::Regular
                | CalendarSessionKind::EarlyClose
                | CalendarSessionKind::Crypto24x7 => {
                    if !present_dates.contains(&cur) {
                        gaps.push(CoverageGap {
                            id: format!("GAP:MISSING:{instrument_id}:{cur}"),
                            dataset_version,
                            instrument_id: instrument_id.to_string(),
                            start_date: cur,
                            end_date: cur,
                            gap_kind: "provider_missing".to_string(),
                            resolution: "open".to_string(),
                        });
                    }
                }
            }
            sessions.push(sess);
            match cur.checked_add_days(chrono::Days::new(1)) {
                Some(next) => cur = next,
                None => break,
            }
        }

        let tx = self.conn.savepoint()?;
        for gap in &gaps {
            tx.execute(
                "INSERT OR REPLACE INTO coverage_gap (id, dataset_version, instrument_id, start_date, end_date, gap_kind, resolution)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    gap.id,
                    gap.dataset_version,
                    gap.instrument_id,
                    gap.start_date.to_string(),
                    gap.end_date.to_string(),
                    gap.gap_kind,
                    gap.resolution,
                ],
            )?;
        }
        tx.commit()?;
        Ok((sessions, gaps))
    }

    /// Update `manifest.json` in `root_dir` with current file hashes.
    pub fn sync_manifest(&self) -> Result<LibraryManifest, StorageError> {
        let (lib_id, schema_ver, book_ccy, rep_ccy, tz, created_at): (
            String,
            i64,
            String,
            String,
            String,
            String,
        ) = self.conn.query_row(
            "SELECT id, schema_version, book_currency, report_currency, timezone, created_at FROM library LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;

        let mut file_hashes = BTreeMap::new();
        for (relative, path) in library_files(&self.root_dir)? {
            file_hashes.insert(relative, sha256_file(&path)?);
        }

        let table_row_counts = collect_library_table_counts(&self.conn)?;

        let manifest = LibraryManifest {
            schema_version: schema_ver,
            library_id: lib_id,
            book_currency: book_ccy,
            report_currency: rep_ccy,
            timezone: tz,
            created_at,
            updated_at: Utc::now().to_rfc3339(),
            file_hashes,
            table_row_counts,
        };
        let manifest_path = self.root_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        Ok(manifest)
    }

    /// Create an atomic consistent backup using the SQLite Backup API (`rusqlite::backup::Backup`)
    /// plus all referenced files (`raw/`, `imports/`, `attachments/`) and a verified `manifest.json` (AC-09).
    pub fn create_consistent_backup(
        &self,
        backup_dir: &Path,
    ) -> Result<LibraryManifest, StorageError> {
        if backup_dir.exists() {
            let mut entries = fs::read_dir(backup_dir)?;
            if entries.next().is_some() {
                return Err(StorageError::RestoreTargetNotEmpty(
                    backup_dir.display().to_string(),
                ));
            }
        }
        fs::create_dir_all(backup_dir)?;
        let target = fs::canonicalize(backup_dir)?;
        let source = fs::canonicalize(&self.root_dir)?;
        if ["raw", "imports", "attachments", "settings"]
            .iter()
            .any(|name| target.starts_with(source.join(name)))
        {
            return Err(StorageError::IntegrityFailed(
                "backup target cannot be inside referenced library files".into(),
            ));
        }
        for sub in ["raw", "imports", "attachments", "settings"] {
            fs::create_dir_all(backup_dir.join(sub))?;
        }

        let backup_db_path = backup_dir.join("delta.sqlite");
        let backup_table_counts = {
            let mut dst_conn = Connection::open(&backup_db_path)?;
            {
                let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst_conn)?;
                backup.run_to_completion(128, Duration::from_millis(10), None)?;
            }
            collect_library_table_counts(&dst_conn)?
        };

        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("delta.sqlite".to_string(), sha256_file(&backup_db_path)?);

        for (relative, source) in library_files(&self.root_dir)? {
            let target = backup_dir.join(&relative);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::copy(&source, &target)?;
            file_hashes.insert(relative, sha256_file(&target)?);
        }

        let mut manifest = self.sync_manifest()?;
        manifest.file_hashes = file_hashes;
        manifest.table_row_counts = backup_table_counts;
        let manifest_path = backup_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        Ok(manifest)
    }

    /// Restore a backup into `target_dir` after verifying all SHA-256 hashes, SQLite integrity,
    /// and foreign key constraints. Refuses if `target_dir` is non-empty (AC-09 / M1-W01).
    pub fn restore_consistent_backup(
        backup_dir: &Path,
        target_dir: &Path,
    ) -> Result<Self, StorageError> {
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
                "missing manifest.json in backup directory".to_string(),
            ));
        }
        let manifest: LibraryManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;

        if !manifest.file_hashes.contains_key("delta.sqlite") {
            return Err(StorageError::IntegrityFailed(
                "backup manifest must include delta.sqlite".into(),
            ));
        }

        // Verify all file hashes in backup_dir before copying
        for (rel_path, expected_hash) in &manifest.file_hashes {
            if rel_path.contains(':')
                || rel_path.contains('\\')
                || Path::new(rel_path)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(StorageError::IntegrityFailed(format!(
                    "path traversal rejected in backup manifest: {rel_path}"
                )));
            }
            let src_file = backup_dir.join(rel_path);
            let canonical_root = fs::canonicalize(backup_dir)?;
            if !fs::canonicalize(&src_file)?.starts_with(&canonical_root) {
                return Err(StorageError::IntegrityFailed(
                    "backup link escapes root".into(),
                ));
            }
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

        // Validate the source before touching the destination. Never initialize a missing DB.
        let source = Connection::open_with_flags(
            backup_dir.join("delta.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String = source.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let violations: i64 =
            source.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if integrity != "ok" || violations != 0 {
            return Err(StorageError::IntegrityFailed(
                "invalid backup database".into(),
            ));
        }
        let identity: (String, i64, String) = source.query_row(
            "SELECT id, schema_version, book_currency FROM library LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if identity
            != (
                manifest.library_id.clone(),
                manifest.schema_version,
                manifest.book_currency.clone(),
            )
        {
            return Err(StorageError::IntegrityFailed(
                "backup identity mismatch".into(),
            ));
        }
        if !manifest.table_row_counts.is_empty() {
            let source_counts = collect_library_table_counts(&source)?;
            if source_counts != manifest.table_row_counts {
                return Err(StorageError::IntegrityFailed(
                    "backup table row counts mismatch manifest".into(),
                ));
            }
        }
        drop(source);

        // Copy verified files to target_dir
        fs::create_dir_all(target_dir)?;
        for sub in [
            "raw",
            "imports",
            "attachments",
            "backups",
            "logs",
            "cache",
            "settings",
        ] {
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

        // Open and verify foreign_key_check and integrity_check
        let store = Self::open_or_init(
            target_dir,
            &manifest.book_currency,
            &manifest.report_currency,
            &manifest.timezone,
        )?;
        let fk_violations: i64 =
            store
                .conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        if fk_violations != 0 {
            return Err(StorageError::IntegrityFailed(format!(
                "restored database has {fk_violations} foreign key violations"
            )));
        }
        if !manifest.table_row_counts.is_empty() {
            let restored_counts = collect_library_table_counts(&store.conn)?;
            for (table, &expected) in &manifest.table_row_counts {
                let actual = restored_counts.get(table).copied().unwrap_or(0);
                if manifest.schema_version == CURRENT_SCHEMA_VERSION {
                    if actual != expected {
                        return Err(StorageError::IntegrityFailed(format!(
                            "restored table '{table}' count mismatch: expected {expected}, got {actual}"
                        )));
                    }
                } else if actual < expected {
                    return Err(StorageError::IntegrityFailed(format!(
                        "upgraded restored table '{table}' lost rows: expected >= {expected}, got {actual}"
                    )));
                }
            }
        }
        Ok(store)
    }
}

pub fn sha256_file(path: &Path) -> Result<String, StorageError> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn library_files(root: &Path) -> Result<Vec<(String, PathBuf)>, StorageError> {
    fn walk(
        root: &Path,
        directory: &Path,
        out: &mut Vec<(String, PathBuf)>,
    ) -> Result<(), StorageError> {
        if !directory.exists() {
            return Ok(());
        }
        for item in fs::read_dir(directory)? {
            let path = item?.path();
            if fs::symlink_metadata(&path)?.file_type().is_symlink()
                || !fs::canonicalize(&path)?.starts_with(fs::canonicalize(root)?)
            {
                return Err(StorageError::IntegrityFailed(
                    "library file escapes root".into(),
                ));
            }
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.is_file() {
                out.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    path,
                ));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    for dir in ["raw", "imports", "attachments", "settings"] {
        walk(root, &root.join(dir), &mut files)?;
    }
    let settings = root.join("settings.json");
    if settings.exists() {
        if !fs::canonicalize(&settings)?.starts_with(fs::canonicalize(root)?) {
            return Err(StorageError::IntegrityFailed(
                "settings link escapes root".into(),
            ));
        }
        files.push(("settings.json".into(), settings));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}
