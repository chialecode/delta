//! Business repository: events, postings, imports, journal, market data.
//! All money stays decimal TEXT; aggregation happens in delta-core Decimal.

use crate::error::{InfraError, InfraResult};
use crate::sqlite::migrate;
use chrono::{DateTime, SecondsFormat, Utc};
use delta_core::events::{Corrections, EconomicEvent, EventPayload};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId};
use delta_core::ledger::{currency_of, AccountLedger, FxResolver, InstrumentMap, LedgerEngine};
use delta_core::money::CurrencyCode;
use delta_core::valuation::{PriceQuote, PriceSource};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Canonical stored timestamp: UTC, fixed nanosecond width and `Z`, so
/// SQL string comparison and ordering equal chronological order.
pub fn canonical_ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// One library per folder (system-architecture §9). Its attachments and
/// backups sit in that folder, so a second library there would share them
/// and deleting either would remove the other's files.
pub(crate) fn ensure_sole_library(path: &Path) -> InfraResult<()> {
    let Some(dir) = path.parent().filter(|d| d.is_dir()) else {
        return Ok(());
    };
    for entry in std::fs::read_dir(dir)? {
        let other = entry?.path();
        if other != path
            && other.is_file()
            && other
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sqlite"))
        {
            return Err(InfraError::InvalidPath(format!(
                "{} already holds a library ({}); use a new folder",
                dir.display(),
                other.display()
            )));
        }
    }
    Ok(())
}

/// Insert one event inside the caller's transaction. Returns 0 when an
/// event with the same id, or the same account and source reference,
/// already exists, so callers that must write every row can check the count.
pub(crate) fn insert_event(
    tx: &Connection,
    e: &EconomicEvent,
    batch_id: Option<&str>,
) -> InfraResult<usize> {
    let payload = serde_json::to_string(&e.payload)?;
    Ok(tx.execute(
        "INSERT OR IGNORE INTO economic_event
         (id, account_id, occurred_at, recorded_at, seq, source_ref, correction_group, revision, reverses, payload, import_batch_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            e.id.0,
            e.account_id.0,
            canonical_ts(e.occurred_at),
            canonical_ts(e.recorded_at),
            e.seq,
            e.source_ref,
            e.correction_group,
            e.revision,
            e.reverses.as_ref().map(|r| r.0.clone()),
            payload,
            batch_id,
        ],
    )?)
}

/// Quote an FTS5 phrase so operators in user text stay literal.
pub fn escape_fts_query(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// Parse an RFC 3339 timestamp with any offset into the canonical form.
pub fn canonicalize_ts(raw: &str) -> InfraResult<String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| canonical_ts(t.with_timezone(&Utc)))
        .map_err(|e| InfraError::InvalidPath(format!("invalid timestamp {raw:?}: {e}")))
}

pub struct Library {
    pub id: String,
    pub book_currency: String,
    pub path: std::path::PathBuf,
    conn: Mutex<Connection>,
    /// Test hook: the next event write fails before commit.
    pub fail_next_event_write: AtomicBool,
    /// Test hook: the next journal save fails before commit.
    pub fail_next_journal: AtomicBool,
    /// Test hook: the next checkpoint insert fails before it switches.
    pub fail_next_checkpoint: AtomicBool,
}

impl Library {
    /// Create a new library database at `path` and run migrations.
    pub fn create(path: &Path, book_currency: &str) -> InfraResult<Self> {
        if path.exists() {
            return Err(InfraError::InvalidPath(format!(
                "library already exists: {}",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ensure_sole_library(path)?;
        let conn = crate::sqlite::open(path)?;
        Self::from_conn(conn, path, book_currency)
    }

    /// Open an existing library, running append-only migrations first.
    pub fn open(path: &Path) -> InfraResult<Self> {
        if !path.exists() {
            return Err(InfraError::NotFound(format!(
                "library not found: {}",
                path.display()
            )));
        }
        // Inspect without writes before WAL setup or migrations. An unrelated,
        // empty, corrupt or read-only file must never become a new DELTA library.
        if std::fs::metadata(path)?.permissions().readonly() {
            return Err(InfraError::Rejected("library is read-only".into()));
        }
        {
            let check =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let valid: i64 =
                check.query_row("SELECT count(*) FROM library_meta", [], |r| r.get(0))?;
            let integrity: String = check.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            if valid != 1 || integrity != "ok" {
                return Err(InfraError::Rejected("invalid DELTA library".into()));
            }
        }
        let mut conn = crate::sqlite::open(path)?;
        migrate::run(&mut conn)?;
        let (id, book) = conn
            .query_row(
                "SELECT id, book_currency FROM library_meta LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(|_| InfraError::NotFound("library_meta empty".into()))?;
        Ok(Self {
            id,
            book_currency: book,
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
            fail_next_event_write: AtomicBool::new(false),
            fail_next_journal: AtomicBool::new(false),
            fail_next_checkpoint: AtomicBool::new(false),
        })
    }

    fn from_conn(conn: Connection, path: &Path, book_currency: &str) -> InfraResult<Self> {
        let mut conn = conn;
        migrate::run(&mut conn)?;
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO library_meta (id, schema_version, book_currency, created_at) VALUES (?1, 1, ?2, ?3)",
            params![id, book_currency, Utc::now().to_rfc3339()],
        )?;
        Ok(Self {
            id,
            book_currency: book_currency.into(),
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
            fail_next_event_write: AtomicBool::new(false),
            fail_next_journal: AtomicBool::new(false),
            fail_next_checkpoint: AtomicBool::new(false),
        })
    }

    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let conn = self.conn.lock().expect("library lock");
        f(&conn)
    }

    // ---- accounts / instruments / assets ----------------------------------

    pub fn ensure_account(&self, id: &str, name: &str, kind: &str) -> InfraResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO account (id, name, type) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO NOTHING",
                params![id, name, kind],
            )?;
            Ok(())
        })
    }

    pub fn ensure_asset(&self, id: &str, kind: &str) -> InfraResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO asset (id, kind, symbol) VALUES (?1, ?2, ?1)
                 ON CONFLICT(id) DO NOTHING",
                params![id, kind],
            )?;
            Ok(())
        })
    }

    pub fn ensure_instrument(
        &self,
        id: &str,
        base: &str,
        quote: &str,
        venue: &str,
    ) -> InfraResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO instrument (id, kind, venue, base_asset_id, quote_asset_id) VALUES (?1, 'spot', ?2, ?3, ?4)
                 ON CONFLICT(id) DO NOTHING",
                params![id, venue, base, quote],
            )?;
            Ok(())
        })
    }

    // ---- events -------------------------------------------------------------

    /// Record events in one transaction; idempotent per event id and per
    /// (account, source_ref). Another account may use the same source id.
    pub fn record_events(
        &self,
        events: &[EconomicEvent],
        batch_id: Option<&str>,
    ) -> InfraResult<usize> {
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let mut inserted = 0;
            for e in events {
                inserted += insert_event(&tx, e, batch_id)?;
            }
            tx.commit()?;
            Ok(inserted)
        })
    }

    pub fn recorded_events(
        &self,
        account_ids: Option<&[String]>,
    ) -> InfraResult<Vec<EconomicEvent>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, account_id, occurred_at, recorded_at, seq, source_ref, correction_group, revision, reverses, payload
                 FROM economic_event ORDER BY occurred_at, seq",
            )?;
            let mut out = Vec::new();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let account: String = row.get(1)?;
                if let Some(filter) = account_ids {
                    if !filter.iter().any(|a| a == &account) {
                        continue;
                    }
                }
                let payload_str: String = row.get(9)?;
                out.push(EconomicEvent {
                    id: EventId(row.get(0)?),
                    account_id: AccountId(account),
                    occurred_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(2)?)
                        .map_err(|e| InfraError::InvalidPath(e.to_string()))?
                        .with_timezone(&Utc),
                    recorded_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
                        .map_err(|e| InfraError::InvalidPath(e.to_string()))?
                        .with_timezone(&Utc),
                    seq: row.get(4)?,
                    source_ref: row.get(5)?,
                    correction_group: row.get(6)?,
                    revision: row.get(7)?,
                    reverses: row.get::<_, Option<String>>(8)?.map(EventId),
                    payload: serde_json::from_str(&payload_str)?,
                });
            }
            Ok(out)
        })
    }

    /// Monotonic ledger revision: number of recorded events.
    pub fn ledger_revision(&self) -> InfraResult<i64> {
        self.with(|c| {
            c.query_row("SELECT COUNT(*) FROM economic_event", [], |r| r.get(0))
                .map_err(Into::into)
        })
    }

    // ---- engine rebuild -----------------------------------------------------

    /// Rebuild the ledger engine from recorded events (optionally bounded by
    /// `as_of`), using instruments/fx from the database.
    pub fn rebuild_engine(&self, as_of: Option<DateTime<Utc>>) -> InfraResult<LedgerEngine> {
        let book = CurrencyCode(self.book_currency.clone());
        let mut engine = LedgerEngine::new(book.clone());
        // Cash assets: fiat + stablecoin kinds.
        let cash_assets: HashSet<AssetId> = self.with(|c| {
            let mut stmt = c.prepare("SELECT id FROM asset WHERE kind IN ('fiat','stablecoin')")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok::<HashSet<AssetId>, rusqlite::Error>(
                rows.filter_map(|r| r.ok()).map(AssetId).collect(),
            )
        })?;
        for a in cash_assets {
            engine = engine.with_cash_asset(a);
        }
        let mut instruments = InstrumentMap::default();
        self.with(|c| {
            let mut stmt = c.prepare("SELECT id, base_asset_id FROM instrument")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, base): (String, String) = row?;
                instruments.0.insert(InstrumentId(id), AssetId(base));
            }
            Ok::<(), rusqlite::Error>(())
        })?;
        let events = self.recorded_events(None)?;
        let fx = DbFx { library: self };
        let view = Corrections::resolve(&events);
        let mut effective = view.effective_events(&events);
        if let Some(as_of) = as_of {
            effective.retain(|e| e.occurred_at < as_of);
        }
        engine.apply_all(&effective, &fx, &instruments);
        Ok(engine)
    }

    /// The per-account ledger state from the current rebuild.
    pub fn account_ledgers(
        &self,
        as_of: Option<DateTime<Utc>>,
    ) -> InfraResult<Vec<(AccountId, AccountLedger)>> {
        let engine = self.rebuild_engine(as_of)?;
        Ok(engine.accounts.into_iter().collect())
    }

    // ---- journal ------------------------------------------------------------

    pub fn save_journal(
        &self,
        title: &str,
        body: &str,
        tags: &[String],
        journal_id: Option<&str>,
        account_id: Option<&str>,
    ) -> InfraResult<String> {
        self.save_journal_fields(title, body, tags, journal_id, account_id, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save_journal_fields(
        &self,
        title: &str,
        body: &str,
        tags: &[String],
        journal_id: Option<&str>,
        account_id: Option<&str>,
        fields: Option<&str>,
        expected_revision: Option<i64>,
    ) -> InfraResult<String> {
        if self.fail_next_journal.swap(false, Ordering::SeqCst) {
            return Err(InfraError::Rejected(
                "journal write failed; draft was not saved".into(),
            ));
        }
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let id = match journal_id {
                Some(id) => {
                    let rev: i64 = tx.query_row(
                        "SELECT current_revision FROM journal_entry WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )?;
                    if expected_revision.is_some_and(|expected| expected != rev) {
                        return Err(InfraError::Rejected("note changed; reopen before saving (draft retained)".into()));
                    }
                    tx.execute(
                        "INSERT INTO journal_revision (journal_id, revision, body, saved_at) VALUES (?1, ?2, ?3, ?4)",
                        params![id, rev + 1, body, Utc::now().to_rfc3339()],
                    )?;
                    tx.execute(
                        "UPDATE journal_entry SET current_revision = ?2, title = ?3, account_id = COALESCE(?4, account_id) WHERE id = ?1",
                        params![id, rev + 1, title, account_id],
                    )?;
                    id.to_string()
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    tx.execute(
                        "INSERT INTO journal_entry (id, title, created_at, current_revision, account_id) VALUES (?1, ?2, ?3, 1, ?4)",
                        params![id, title, Utc::now().to_rfc3339(), account_id],
                    )?;
                    tx.execute(
                        "INSERT INTO journal_revision (journal_id, revision, body, saved_at) VALUES (?1, 1, ?2, ?3)",
                        params![id, body, Utc::now().to_rfc3339()],
                    )?;
                    id
                }
            };
            tx.execute("UPDATE journal_revision SET structured_fields=?2 WHERE journal_id=?1 AND revision=(SELECT current_revision FROM journal_entry WHERE id=?1)", params![id, fields])?;
            for tag in tags {
                tx.execute(
                    "INSERT OR IGNORE INTO journal_tag (journal_id, tag) VALUES (?1, ?2)",
                    params![id, tag],
                )?;
            }
            // One FTS row per journal: a new revision replaces the previous
            // body so old text cannot be retrieved (FR-JRN-01).
            tx.execute("DELETE FROM journal_fts WHERE journal_id = ?1", params![id])?;
            tx.execute(
                "INSERT INTO journal_fts (journal_id, title, body) VALUES (?1, ?2, ?3)",
                params![id, title, body],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }

    /// Chinese-capable search. The MATCH expression is a quoted phrase so
    /// operators in the raw query cannot change the search. `account_ids`
    /// restricts hits to those accounts.
    pub fn search_journal(
        &self,
        query: &str,
        limit: usize,
        account_ids: Option<&[String]>,
    ) -> InfraResult<Vec<(String, String, String)>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let phrase = escape_fts_query(query);
        // The account filter runs in SQL, before LIMIT, so other accounts'
        // notes cannot fill the window and hide in-scope hits. NULL = all.
        let scope: Option<String> = account_ids.map(serde_json::to_string).transpose()?;
        let like = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with(|c| {
            let mut out: Vec<(String, String, String)> = Vec::new();
            if query.chars().any(|ch| ch.is_ascii_alphanumeric()) {
                let mut stmt = c.prepare(
                    "SELECT j.id, j.title, jr.body FROM journal_fts f
                     JOIN journal_entry j ON j.id = f.journal_id
                     JOIN journal_revision jr ON jr.journal_id = j.id AND jr.revision = j.current_revision
                     WHERE journal_fts MATCH ?1
                       AND (?2 IS NULL OR j.account_id IN (SELECT value FROM json_each(?2)))
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![phrase, scope, limit_sql], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                for row in rows {
                    out.push(row?);
                }
            }
            // The default tokenizer does not segment CJK: match the current
            // revision as a substring instead.
            if out.len() < limit && !query.is_ascii() {
                let mut stmt = c.prepare(
                    "SELECT j.id, j.title, jr.body FROM journal_entry j
                     JOIN journal_revision jr ON jr.journal_id = j.id AND jr.revision = j.current_revision
                     WHERE (jr.body LIKE ?1 ESCAPE '\\' OR j.title LIKE ?1 ESCAPE '\\')
                       AND (?2 IS NULL OR j.account_id IN (SELECT value FROM json_each(?2)))
                     ORDER BY j.created_at DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![like, scope, limit_sql], |r| {
                    Ok::<(String, String, String), rusqlite::Error>((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, title, body) = row?;
                    if !out.iter().any(|(oid, _, _)| oid == &id) {
                        out.push((id, title, body));
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
            Ok(out)
        })
    }

    // ---- market data ----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_bar(
        &self,
        instrument: &str,
        session_date: &str,
        open_at: &str,
        close_at: &str,
        o: &str,
        h: &str,
        l: &str,
        c: &str,
        volume: &str,
        source: &str,
        adjustment: &str,
        dataset_version: &str,
    ) -> InfraResult<()> {
        let open_at = canonicalize_ts(open_at)?;
        let close_at = canonicalize_ts(close_at)?;
        self.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO bar
                 (instrument_id, timeframe, session_date, open_at, close_at, open, high, low, close, volume, source, adjustment, is_final, dataset_version)
                 VALUES (?1, '1d', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12)",
                params![instrument, session_date, open_at, close_at, o, h, l, c, volume, source, adjustment, dataset_version],
            )?;
            Ok(())
        })
    }

    pub fn upsert_fx(
        &self,
        base: &str,
        quote: &str,
        rate: &str,
        effective_at: &str,
        observed_at: &str,
        source: &str,
    ) -> InfraResult<()> {
        let effective_at = canonicalize_ts(effective_at)?;
        let observed_at = canonicalize_ts(observed_at)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO fx_rate (id, base, quote, rate, effective_at, observed_at, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![uuid::Uuid::new_v4().to_string(), base, quote, rate, effective_at, observed_at, source],
            )?;
            Ok(())
        })
    }

    /// Price source over bars: latest close for an instrument whose base asset
    /// is the requested asset, bounded by `at` (never reads the future).
    pub fn price_source(&self) -> DbPriceSource<'_> {
        DbPriceSource { library: self }
    }

    pub fn fx_resolver(&self) -> DbFx<'_> {
        DbFx { library: self }
    }
}

/// FX resolver over stored fx_rate rows (direct or inverse).
pub struct DbFx<'a> {
    library: &'a Library,
}

impl DbFx<'_> {
    /// Latest stored `base→quote` rate effective at or before `at`.
    fn latest(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        at: &str,
    ) -> Option<(rust_decimal::Decimal, DateTime<Utc>)> {
        let row: Option<(String, String)> = self.library.with(|c| {
            c.query_row(
                "SELECT rate, effective_at FROM fx_rate WHERE base=?1 AND quote=?2 AND effective_at<=?3
                 ORDER BY effective_at DESC, observed_at DESC LIMIT 1",
                params![base.0, quote.0, at],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten()
        });
        let (rate, effective_at) = row?;
        let effective_at = DateTime::parse_from_rfc3339(&effective_at)
            .ok()?
            .with_timezone(&Utc);
        Some((rate.parse().ok()?, effective_at))
    }
}

impl FxResolver for DbFx<'_> {
    fn rate(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        at: DateTime<Utc>,
    ) -> Option<(rust_decimal::Decimal, DateTime<Utc>)> {
        let at_s = canonical_ts(at);
        if let Some(direct) = self.latest(base, quote, &at_s) {
            return Some(direct);
        }
        let (inv, effective_at) = self.latest(quote, base, &at_s)?;
        let rate = rust_decimal::Decimal::ONE.checked_div(inv)?;
        Some((rate, effective_at))
    }
}

/// Price source over stored bars (bounded by valuation time).
pub struct DbPriceSource<'a> {
    library: &'a Library,
}

impl PriceSource for DbPriceSource<'_> {
    fn price(&self, asset: &AssetId, at: DateTime<Utc>) -> Option<PriceQuote> {
        // Valuation uses raw (unadjusted) closes only; adjusted series are
        // display/analysis views (financial-engine §3).
        let at_s = canonical_ts(at);
        self.library.with(|c| {
            c.query_row(
                "SELECT b.close, b.close_at, i.quote_asset_id FROM bar b
                     JOIN instrument i ON i.id = b.instrument_id
                     WHERE i.base_asset_id = ?1 AND b.close_at <= ?2 AND b.is_final = 1
                       AND b.adjustment = 'raw'
                     ORDER BY b.close_at DESC, b.dataset_version DESC LIMIT 1",
                params![asset.0, at_s],
                |r| {
                    let price_raw: String = r.get(0)?;
                    let close_at: String = r.get(1)?;
                    let quote: String = r.get(2)?;
                    let price: rust_decimal::Decimal = price_raw.parse().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            0,
                            price_raw.clone(),
                            rusqlite::types::Type::Text,
                        )
                    })?;
                    let as_of = DateTime::parse_from_rfc3339(&close_at)
                        .map_err(|_e| {
                            rusqlite::Error::InvalidColumnType(
                                1,
                                close_at.clone(),
                                rusqlite::types::Type::Text,
                            )
                        })?
                        .with_timezone(&Utc);
                    Ok(PriceQuote {
                        price,
                        currency: currency_of(&AssetId(quote)),
                        as_of,
                        stale: false,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
        })
    }
}

/// Rebuild an engine using a preloaded event list (for preview paths).
pub fn engine_from_events(
    events: &[EconomicEvent],
    book_currency: &CurrencyCode,
    instruments: &InstrumentMap,
    cash_assets: HashSet<AssetId>,
    fx: &dyn FxResolver,
) -> LedgerEngine {
    let mut engine = LedgerEngine::new(book_currency.clone());
    for a in cash_assets {
        engine = engine.with_cash_asset(a);
    }
    let view = Corrections::resolve(events);
    let effective = view.effective_events(events);
    engine.apply_all(&effective, fx, instruments);
    engine
}

/// Event payload helper for import mapping tests.
pub fn payload_type(e: &EconomicEvent) -> &'static str {
    match &e.payload {
        EventPayload::OpeningPosition { .. } => "opening_position",
        EventPayload::Buy { .. } => "buy",
        EventPayload::Sell { .. } => "sell",
        EventPayload::CashDeposit { .. } => "cash_deposit",
        EventPayload::CashWithdrawal { .. } => "cash_withdrawal",
        EventPayload::Transfer { .. } => "transfer",
        EventPayload::Fee { .. } => "fee",
        EventPayload::Interest { .. } => "interest",
        EventPayload::DividendCash { .. } => "dividend_cash",
        EventPayload::StockSplit { .. } => "stock_split",
    }
}
