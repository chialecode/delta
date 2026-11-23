use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::Path;

use chrono::{NaiveDate, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text, ManualValuation};
use rusqlite::{params, Connection};

use crate::m2_schema::{
    mask_external_account_ref, AccountRecord, InstrumentSearchHit, PortfolioRecord,
    WatchlistGroupDto, FAIL_NEXT_SCHEMA_4_MIGRATION, SCHEMA_4_TABLES,
};
use crate::storage::{SqliteStore, StorageError};

impl SqliteStore {
    /// Create a genuine Schema 3 library on disk (dropping Schema 4 tables and setting `schema_version = 3`)
    /// so `M2-A-02` can exercise the 3 -> 4 migration, pre-migration backup, and fault injection.
    pub fn create_schema_3_library_for_test(
        root_dir: &Path,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<(), StorageError> {
        let prev_fault_4 = FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.replace(false));
        let prev_fault_5 = crate::m4_schema::swap_schema_5_migration_fault_for_test(false);
        let result = (|| -> Result<(), StorageError> {
            let mut store = Self::open_or_init(root_dir, book_currency, report_currency, timezone)?;
            let tx = store.conn_mut().savepoint()?;
            tx.execute_batch(
                r#"
                DROP TRIGGER IF EXISTS trg_training_session_monotonic_guard;
                DROP TRIGGER IF EXISTS trg_fee_profile_version_immutable_delete;
                DROP TRIGGER IF EXISTS trg_fee_profile_version_immutable_update;
                DROP TRIGGER IF EXISTS trg_practice_entry_append_only_update;
                DROP TRIGGER IF EXISTS trg_practice_entry_append_only_delete;
                DROP INDEX IF EXISTS idx_bar_training_port;
                "#,
            )?;
            for table in crate::m5_schema::SCHEMA_6_TABLES.iter().rev() {
                tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            }
            for table in crate::m4_schema::SCHEMA_5_TABLES.iter().rev() {
                tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            }
            for table in SCHEMA_4_TABLES {
                tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            }
            tx.execute("UPDATE library SET schema_version = 3", [])?;
            tx.commit()?;
            let backups_dir = store.root_dir().join("backups");
            if backups_dir.is_dir() {
                for entry in fs::read_dir(&backups_dir)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("before-schema-") && entry.path().is_dir() {
                        fs::remove_dir_all(entry.path())?;
                    }
                }
            }
            store.sync_manifest()?;
            Ok(())
        })();
        FAIL_NEXT_SCHEMA_4_MIGRATION.with(|flag| flag.set(prev_fault_4));
        let _ = crate::m4_schema::swap_schema_5_migration_fault_for_test(prev_fault_5);
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
        let mut stmt =
            conn.prepare("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)")?;
        for table in SCHEMA_4_TABLES {
            let exists: bool = stmt.query_row(params![table], |r| r.get(0))?;
            if exists {
                present.push((*table).to_string());
            }
        }
        Ok(present)
    }

    /// Verify and restore a consistent backup into `target_dir` WITHOUT running schema migrations,
    /// returning the restored `schema_version` (`M2-A-02`).
    /// Reuses `SqliteStore::restore_backup_staged` so path traversal, symlink/reparse point escape,
    /// SHA-256 hashes, SQLite integrity, identity, and row counts are validated before copying.
    pub fn restore_consistent_backup_without_migration(
        backup_dir: &Path,
        target_dir: &Path,
    ) -> Result<i64, StorageError> {
        Self::restore_backup_staged(backup_dir, target_dir, |manifest| {
            let restored_conn = Connection::open_with_flags(
                target_dir.join("delta.sqlite"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            let integrity: String =
                restored_conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            let fk_violations: i64 = restored_conn.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
                [],
                |r| r.get(0),
            )?;
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
            if !manifest.table_row_counts.is_empty() {
                let restored_counts = crate::storage::collect_library_table_counts(&restored_conn)?;
                if restored_counts != manifest.table_row_counts {
                    return Err(StorageError::IntegrityFailed(
                        "restored table row counts do not match manifest".into(),
                    ));
                }
            }
            Ok(schema_ver)
        })
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
        let mut stmt = self.conn().prepare_cached(
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
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
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
        let dedup_members: Vec<String> = member_accounts
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO portfolio_member (portfolio_id, account_id, valid_from, valid_to)
                 VALUES (?1, ?2, NULL, NULL)",
            )?;
            for acct_id in &dedup_members {
                stmt.execute(params![id, acct_id])?;
            }
        }

        tx.execute(
            "DELETE FROM portfolio_group_link WHERE parent_portfolio_id = ?1",
            params![id],
        )?;
        let dedup_children: Vec<String> = child_portfolios
            .iter()
            .filter(|c| c.as_str() != id)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO portfolio_group_link (parent_portfolio_id, child_portfolio_id)
                 VALUES (?1, ?2)",
            )?;
            for child_id in &dedup_children {
                stmt.execute(params![id, child_id])?;
            }
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
            .prepare_cached("SELECT id, name, created_at FROM portfolio ORDER BY id")?;
        let base_rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut members_by_pf: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let mut m_stmt = self.conn().prepare_cached(
                "SELECT portfolio_id, account_id FROM portfolio_member ORDER BY portfolio_id, account_id",
            )?;
            for row in
                m_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (pid, aid) = row?;
                members_by_pf.entry(pid).or_default().push(aid);
            }
        }

        let mut children_by_pf: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let mut c_stmt = self.conn().prepare_cached(
                "SELECT parent_portfolio_id, child_portfolio_id FROM portfolio_group_link ORDER BY parent_portfolio_id, child_portfolio_id",
            )?;
            for row in
                c_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (pid, cid) = row?;
                children_by_pf.entry(pid).or_default().push(cid);
            }
        }

        let mut out = Vec::with_capacity(base_rows.len());
        for (id, name, created_at) in base_rows {
            let member_accounts = members_by_pf.remove(&id).unwrap_or_default();
            let child_portfolios = children_by_pf.remove(&id).unwrap_or_default();
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

        let mut m_stmt = self
            .conn()
            .prepare_cached("SELECT account_id FROM portfolio_member WHERE portfolio_id = ?1")?;
        let mut c_stmt = self.conn().prepare_cached(
            "SELECT child_portfolio_id FROM portfolio_group_link WHERE parent_portfolio_id = ?1",
        )?;

        while let Some(curr_pid) = queue.pop_front() {
            if !visited_portfolios.insert(curr_pid.clone()) {
                continue;
            }
            for row in m_stmt.query_map(params![curr_pid], |r| r.get::<_, String>(0))? {
                resolved_accounts.insert(row?);
            }
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
        let scope_set: Option<BTreeSet<&str>> =
            account_scope.map(|s| s.iter().map(String::as_str).collect());
        let mut stmt = self.conn().prepare_cached(
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
                if !allowed.contains(account_id.as_str()) {
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
        let mut aliases_by_inst: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        if !needle.is_empty() {
            for a in &registry.aliases {
                if a.external_symbol.to_ascii_lowercase().contains(&needle)
                    || a.provider.to_ascii_lowercase().contains(&needle)
                {
                    aliases_by_inst
                        .entry(a.instrument_id.as_str())
                        .or_default()
                        .push((a.provider.as_str(), a.external_symbol.as_str()));
                }
            }
        }

        let mut hits = Vec::new();
        for inst in registry.instruments.values() {
            let matched_alias = aliases_by_inst
                .get(inst.id.as_str())
                .and_then(|v| v.first())
                .map(|(p, e)| ((*p).to_string(), (*e).to_string()));

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
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, name FROM watchlist_group ORDER BY sort_order ASC, id ASC",
        )?;
        let groups: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut items_by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let mut item_stmt = self.conn().prepare_cached(
                "SELECT group_id, instrument_id FROM watchlist_item ORDER BY group_id ASC, added_at ASC, instrument_id ASC",
            )?;
            for row in
                item_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (gid, iid) = row?;
                items_by_group.entry(gid).or_default().push(iid);
            }
        }

        let mut out = Vec::with_capacity(groups.len());
        for (id, name) in groups {
            let instruments = items_by_group.remove(&id).unwrap_or_default();
            out.push(WatchlistGroupDto {
                id,
                name,
                instruments,
            });
        }
        Ok(out)
    }
}
