//! Production application services shared by the desktop UI and the AI host.
//! Settings, CSV import, file market data, notes, backup and account lines
//! all go through this module (plan v1.1 W02–W10).

use crate::error::{InfraError, InfraResult};
use crate::sqlite::store::{
    canonical_ts, canonicalize_ts, ensure_sole_library, insert_event, Library,
};
use chrono::{DateTime, Utc};
use delta_core::events::{EconomicEvent, EventPayload, Fee, TransferKind};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId, TransferGroupId};
use delta_core::ledger::AccountLedger;
use delta_core::money::{parse_decimal, CurrencyCode};
use delta_core::scope::ScopeSnapshot;
use delta_core::valuation::{
    apply_manual_marks, reconcile_balances, BalanceObservation, ManualKind, ManualMark,
    ValuationSummary,
};
use delta_core::{pnl, valuation};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn app_ts(raw: &str) -> InfraResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| InfraError::Rejected(format!("invalid timestamp: {e}")))
}

// ---- settings, accounts, portfolios ------------------------------------------

impl Library {
    pub fn set_reporting_currency(&self, ccy: &str) -> InfraResult<()> {
        let code = CurrencyCode::new(ccy).0;
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "UPDATE library_meta SET reporting_currency = ?1",
                params![code],
            )?;
            Ok(())
        })
    }

    pub fn reporting_currency(&self) -> InfraResult<String> {
        self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT COALESCE(reporting_currency, book_currency) FROM library_meta LIMIT 1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
    }

    pub fn set_timezone(&self, tz: &str) -> InfraResult<()> {
        if tz.trim().is_empty() {
            return Err(InfraError::Rejected("timezone is required".into()));
        }
        self.with(|c| -> InfraResult<_> {
            c.execute("UPDATE library_meta SET timezone = ?1", params![tz])?;
            Ok(())
        })
    }

    pub fn timezone(&self) -> InfraResult<String> {
        self.with(|c| -> InfraResult<_> {
            c.query_row("SELECT timezone FROM library_meta LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(Into::into)
        })
    }

    pub fn ensure_portfolio(&self, id: &str, name: &str) -> InfraResult<()> {
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO portfolio (id, name) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING",
                params![id, name],
            )?;
            Ok(())
        })
    }

    /// Active membership is unique. A second insert of the same account is
    /// rejected so a portfolio cannot count the account twice.
    pub fn add_portfolio_member(
        &self,
        portfolio_id: &str,
        account_id: &str,
        valid_from: &str,
    ) -> InfraResult<()> {
        self.with(|c| -> InfraResult<_> {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM portfolio_member WHERE portfolio_id = ?1 AND account_id = ?2 AND valid_to IS NULL",
                params![portfolio_id, account_id],
                |r| r.get(0),
            )?;
            if n > 0 {
                return Err(InfraError::Rejected(format!(
                    "account {account_id} is already in portfolio {portfolio_id}"
                )));
            }
            c.execute(
                "INSERT INTO portfolio_member (portfolio_id, account_id, valid_from) VALUES (?1, ?2, ?3)",
                params![portfolio_id, account_id, valid_from],
            )?;
            Ok(())
        })
    }

    pub fn portfolio_accounts(&self, portfolio_id: &str) -> InfraResult<Vec<String>> {
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT account_id FROM portfolio_member WHERE portfolio_id = ?1 AND valid_to IS NULL ORDER BY account_id",
            )?;
            let rows = stmt.query_map(params![portfolio_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }
}

// ---- CSV preview / commit / correction ----------------------------------------

#[derive(Debug, Clone)]
pub struct CsvRowView {
    pub row_no: i64,
    pub state: String,
    pub reason: String,
    pub source_ref: String,
}

#[derive(Debug, Clone)]
pub struct CsvPreview {
    pub batch_id: String,
    pub file_hash: String,
    pub mapping_version: String,
    pub rows: Vec<CsvRowView>,
}

#[derive(Debug, Clone)]
pub struct CommitReceipt {
    pub accepted: i64,
    pub skipped: i64,
    pub rejected: i64,
}

impl Library {
    pub fn preview_csv(
        &self,
        account_id: &str,
        csv_text: &str,
        mapping_version: &str,
    ) -> InfraResult<CsvPreview> {
        let file_hash = sha256_hex(csv_text.as_bytes());
        let batch_id = uuid::Uuid::new_v4().to_string();
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(csv_text.as_bytes());
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let index = self.account_import_index(account_id)?;
        for (idx, rec) in reader.records().enumerate() {
            let rec = rec?;
            let row_no = (idx + 1) as i64;
            let raw = rec.iter().collect::<Vec<_>>().join(",");
            let (mut state, mut reason, source_ref, mut normalized) =
                classify_csv_row(account_id, &rec, &index)?;
            // A source reference identifies one record; a repeat inside the
            // same file would be dropped by the unique index on commit.
            if state != "error" && !seen.insert(source_ref.clone()) {
                state = "error".into();
                reason = "source_ref repeats an earlier row in this file".into();
                normalized = String::new();
            }
            rows.push((row_no, raw, normalized, state, reason, source_ref));
        }
        let batch = batch_id.clone();
        let stored: Vec<CsvRowView> = self.with(|c| -> InfraResult<_> {
            let tx = c.unchecked_transaction()?;
            // One batch per account, file and mapping. A committed one is the
            // import itself; an uncommitted one is replaced by this preview.
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT id, status FROM import_batch WHERE account_id = ?1 AND file_hash = ?2 AND mapping_version = ?3",
                    params![account_id, file_hash, mapping_version],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((old, status)) = existing {
                if status != "preview" {
                    return Err(InfraError::Rejected(format!(
                        "this account already imported this file (batch {old})"
                    )));
                }
                tx.execute("DELETE FROM import_row WHERE batch_id = ?1", params![old])?;
                tx.execute("DELETE FROM import_batch WHERE id = ?1", params![old])?;
            }
            tx.execute(
                "INSERT INTO import_batch (id, account_id, source, file_hash, mapping_version, status, row_count, created_at)
                 VALUES (?1, ?2, 'csv', ?3, ?4, 'preview', ?5, ?6)",
                params![
                    batch,
                    account_id,
                    file_hash,
                    mapping_version,
                    rows.len() as i64,
                    Utc::now().to_rfc3339()
                ],
            )?;
            let mut out = Vec::new();
            for (row_no, raw, normalized, state, reason, source_ref) in &rows {
                let id = uuid::Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO import_row (id, batch_id, row_no, raw_payload, normalized_payload, validation_state, reason)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![id, batch, row_no, raw, normalized, state, reason],
                )?;
                out.push(CsvRowView {
                    row_no: *row_no,
                    state: state.clone(),
                    reason: reason.clone(),
                    source_ref: source_ref.clone(),
                });
            }
            tx.commit()?;
            Ok(out)
        })?;
        Ok(CsvPreview {
            batch_id,
            file_hash,
            mapping_version: mapping_version.into(),
            rows: stored,
        })
    }

    /// Same account and source id are already in the ledger. Content
    /// fingerprints of other files are suspects until the caller skips them
    /// or accepts them as their own source.
    fn account_import_index(&self, account_id: &str) -> InfraResult<AccountImports> {
        let events = self.recorded_events(Some(&[account_id.to_string()]))?;
        let mut index = AccountImports::default();
        for event in events {
            if let Some(source) = event.source_ref.clone() {
                index.sources.insert(source.clone());
                if let Some(fp) = event_fingerprint(&event) {
                    index.fingerprints.entry(fp).or_insert(source);
                }
            }
        }
        Ok(index)
    }

    /// Write the valid rows in `accept_rows`. Rows already imported for this
    /// account are skipped and explained. A content-fingerprint suspect is
    /// not decided here.
    pub fn commit_preview(
        &self,
        batch_id: &str,
        mapping_version: &str,
        file_hash: &str,
        accept_rows: &[i64],
    ) -> InfraResult<CommitReceipt> {
        self.commit_import(batch_id, mapping_version, file_hash, accept_rows, &[], &[])
    }

    /// Skip suspected cross-file duplicates and commit the accepted valid rows
    /// in one transaction.
    pub fn skip_suspected_rows(
        &self,
        batch_id: &str,
        mapping_version: &str,
        file_hash: &str,
        accept_rows: &[i64],
        skip_rows: &[i64],
    ) -> InfraResult<CommitReceipt> {
        self.commit_import(
            batch_id,
            mapping_version,
            file_hash,
            accept_rows,
            skip_rows,
            &[],
        )
    }

    /// Write suspected duplicates under their own source reference. They are
    /// not merged into the earlier source.
    pub fn accept_independent_sources(
        &self,
        batch_id: &str,
        mapping_version: &str,
        file_hash: &str,
        accept_rows: &[i64],
        independent_rows: &[i64],
    ) -> InfraResult<CommitReceipt> {
        self.commit_import(
            batch_id,
            mapping_version,
            file_hash,
            accept_rows,
            &[],
            independent_rows,
        )
    }

    fn commit_import(
        &self,
        batch_id: &str,
        mapping_version: &str,
        file_hash: &str,
        accept_rows: &[i64],
        skip_rows: &[i64],
        independent_rows: &[i64],
    ) -> InfraResult<CommitReceipt> {
        if self
            .fail_next_event_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(InfraError::Rejected(
                "commit failed before the transaction; nothing was applied".into(),
            ));
        }
        let (status, stored_hash, stored_mapping, account): (String, String, String, String) =
            self.with(|c| -> InfraResult<_> {
                c.query_row(
                    "SELECT status, file_hash, mapping_version, account_id FROM import_batch WHERE id = ?1",
                    params![batch_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map_err(|_| InfraError::NotFound(format!("preview {batch_id} not found")))
            })?;
        if stored_hash != file_hash || stored_mapping != mapping_version {
            return Err(InfraError::Rejected(
                "preview is stale; file hash or mapping changed".into(),
            ));
        }
        if status == "committed" {
            let (accepted, skipped, rejected): (i64, i64, i64) = self.with(|c| -> InfraResult<_> {
                c.query_row(
                    "SELECT accepted_count, skipped_count, rejected_count FROM import_batch WHERE id = ?1",
                    params![batch_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(InfraError::Sqlite)
            })?;
            return Ok(CommitReceipt {
                accepted,
                skipped,
                rejected,
            });
        }
        let pending: Vec<(i64, String, String)> = self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT row_no, validation_state, normalized_payload FROM import_row WHERE batch_id = ?1 ORDER BY row_no",
            )?;
            let rows = stmt.query_map(params![batch_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(InfraError::Sqlite)
        })?;
        // Another import since the preview can turn a valid row into a
        // suspect or an imported source; that decision belongs to the user.
        let index = self.account_import_index(&account)?;
        for (row_no, state, payload) in &pending {
            if state == "error" {
                continue;
            }
            let now = classify_payload(&account, payload, &index)?;
            if &now != state {
                return Err(InfraError::Rejected(format!(
                    "preview is stale; row {row_no} is now {now}; preview the file again"
                )));
            }
        }
        let mut events = Vec::new();
        let mut accepted = 0i64;
        let mut skipped = 0i64;
        let mut rejected = 0i64;
        for (row_no, state, payload) in &pending {
            let skip = skip_rows.contains(row_no);
            let independent = independent_rows.contains(row_no);
            let selected = accept_rows.contains(row_no);
            match state.as_str() {
                "error" => rejected += 1,
                "already_imported" => {
                    if selected || independent {
                        return Err(InfraError::Rejected(format!(
                            "row {row_no} is already imported for this account and cannot be accepted again"
                        )));
                    }
                    skipped += 1;
                }
                "duplicate_suspect" => {
                    if skip && independent {
                        return Err(InfraError::Rejected(format!(
                            "row {row_no} cannot be both skipped and accepted"
                        )));
                    }
                    if !skip && !independent {
                        return Err(InfraError::Rejected(format!(
                            "row {row_no} is a suspected duplicate and needs skip_suspected_rows or accept_independent_sources"
                        )));
                    }
                    if independent {
                        events.push((*row_no, row_to_event(account.as_str(), payload, *row_no)?));
                        accepted += 1;
                    } else {
                        skipped += 1;
                    }
                }
                "valid" if independent => {
                    return Err(InfraError::Rejected(format!(
                        "row {row_no} is not a suspected duplicate"
                    )));
                }
                "valid" if skip || !selected => skipped += 1,
                "valid" => {
                    events.push((*row_no, row_to_event(account.as_str(), payload, *row_no)?));
                    accepted += 1;
                }
                _ => rejected += 1,
            }
        }
        // Events, their count check and the batch status commit together; a
        // row the ledger cannot take aborts the whole batch, so the receipt
        // never counts an event that was not written.
        self.with(|c| -> InfraResult<_> {
            let tx = c.unchecked_transaction()?;
            for (row_no, event) in &events {
                if insert_event(&tx, event, Some(batch_id))? != 1 {
                    return Err(InfraError::Rejected(format!(
                        "row {row_no}: source_ref {} already exists for this account; nothing was imported",
                        event.source_ref.as_deref().unwrap_or("")
                    )));
                }
            }
            let switched = tx.execute(
                "UPDATE import_batch SET status = 'committed', accepted_count = ?2, skipped_count = ?3, rejected_count = ?4, committed_at = ?5
                 WHERE id = ?1 AND status = 'preview'",
                params![batch_id, accepted, skipped, rejected, Utc::now().to_rfc3339()],
            )?;
            if switched != 1 {
                return Err(InfraError::Rejected(
                    "preview was committed concurrently".into(),
                ));
            }
            tx.commit()?;
            Ok(())
        })?;
        Ok(CommitReceipt {
            accepted,
            skipped,
            rejected,
        })
    }

    /// Replace one cash-flow amount with a higher revision in the same
    /// correction group. The original row stays readable.
    pub fn correct_cash_amount(&self, event_id: &str, new_amount: &str) -> InfraResult<String> {
        let amount = parse_decimal(new_amount).map_err(|e| InfraError::Rejected(e.to_string()))?;
        let original = self
            .recorded_events(None)?
            .into_iter()
            .find(|e| e.id.0 == event_id)
            .ok_or_else(|| InfraError::NotFound(format!("event {event_id}")))?;
        let group = original
            .correction_group
            .clone()
            .unwrap_or_else(|| format!("corr-{event_id}"));
        let payload = match original.payload {
            EventPayload::CashDeposit { asset, .. } => EventPayload::CashDeposit { asset, amount },
            EventPayload::CashWithdrawal { asset, .. } => {
                EventPayload::CashWithdrawal { asset, amount }
            }
            EventPayload::DividendCash {
                cash_asset,
                instrument,
                ..
            } => EventPayload::DividendCash {
                cash_asset,
                instrument,
                amount,
            },
            _ => {
                return Err(InfraError::Rejected(
                    "only cash amounts can be corrected by this path".into(),
                ))
            }
        };
        // Group tag, replacement and report staleness commit together
        // (data-model: correction in one transaction). The next revision
        // follows the group's latest, so correcting the same original twice
        // supersedes the first correction instead of colliding with it.
        self.with(|c| -> InfraResult<_> {
            let tx = c.unchecked_transaction()?;
            if original.correction_group.is_none() {
                tx.execute(
                    "UPDATE economic_event SET correction_group = ?2 WHERE id = ?1",
                    params![event_id, group],
                )?;
            }
            let latest: i64 = tx.query_row(
                "SELECT COALESCE(MAX(revision), 0) FROM economic_event WHERE correction_group = ?1",
                params![group],
                |r| r.get(0),
            )?;
            let revision = latest.max(original.revision) + 1;
            let replacement = EconomicEvent {
                id: EventId(uuid::Uuid::new_v4().to_string()),
                account_id: original.account_id.clone(),
                occurred_at: original.occurred_at,
                recorded_at: Utc::now(),
                seq: original.seq + 1,
                source_ref: Some(format!("{group}#r{revision}")),
                correction_group: Some(group.clone()),
                revision,
                reverses: None,
                payload,
            };
            if insert_event(&tx, &replacement, None)? != 1 {
                return Err(InfraError::Rejected(format!(
                    "correction revision {revision} of {group} already exists"
                )));
            }
            tx.execute(
                "UPDATE analysis_report SET status = 'stale' WHERE status = 'active'",
                [],
            )?;
            tx.commit()?;
            Ok(replacement.id.0)
        })
    }

    pub fn save_active_report(&self, body: &str, scope: &str) -> InfraResult<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO analysis_report (id, report_type, scope, status, created_at, body) VALUES (?1, 'review', ?2, 'active', ?3, ?4)",
                params![id, scope, Utc::now().to_rfc3339(), body],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    pub fn report_status(&self, id: &str) -> InfraResult<String> {
        self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT status FROM analysis_report WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
    }

    pub fn event_payload_json(&self, event_id: &str) -> InfraResult<String> {
        self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT payload FROM economic_event WHERE id = ?1",
                params![event_id],
                |r| r.get(0),
            )
            .map_err(|_| InfraError::NotFound(event_id.into()))
        })
    }
}

#[derive(Default)]
struct AccountImports {
    sources: std::collections::HashSet<String>,
    fingerprints: std::collections::HashMap<String, String>,
}

fn classify_csv_row(
    account_id: &str,
    rec: &csv::StringRecord,
    index: &AccountImports,
) -> InfraResult<(String, String, String, String)> {
    let field = |i: usize| rec.get(i).unwrap_or("").trim().to_string();
    let occurred = field(0);
    let kind = field(1);
    let asset = field(2);
    let quantity = field(3);
    let price = field(4);
    let fee = field(5);
    let source_ref = field(6);
    let instrument = field(7);
    let quote = field(8);
    if source_ref.is_empty() || occurred.is_empty() || kind.is_empty() {
        return Ok((
            "error".into(),
            "missing occurred_at, type or source_ref".into(),
            source_ref,
            String::new(),
        ));
    }
    if app_ts(&occurred).is_err() {
        return Ok((
            "error".into(),
            "occurred_at is not RFC3339".into(),
            source_ref,
            String::new(),
        ));
    }
    if parse_decimal(&quantity).is_err() {
        return Ok((
            "error".into(),
            "quantity is not a decimal or overflows".into(),
            source_ref,
            String::new(),
        ));
    }
    if !price.is_empty() && parse_decimal(&price).is_err() {
        return Ok((
            "error".into(),
            "price overflows or is not a decimal".into(),
            source_ref,
            String::new(),
        ));
    }
    if !fee.is_empty() && parse_decimal(&fee).is_err() {
        return Ok((
            "error".into(),
            "fee overflows or is not a decimal".into(),
            source_ref,
            String::new(),
        ));
    }
    if !matches!(
        kind.as_str(),
        "deposit" | "withdrawal" | "buy" | "sell" | "dividend" | "fee"
    ) {
        return Ok((
            "error".into(),
            format!("unknown type {kind}"),
            source_ref,
            String::new(),
        ));
    }
    let payload = serde_json::json!({
        "account_id": account_id,
        "occurred_at": occurred,
        "type": kind,
        "asset": asset,
        "quantity": quantity,
        "price": price,
        "fee": fee,
        "source_ref": source_ref,
        "instrument": instrument,
        "quote_currency": quote,
    });
    if index.sources.contains(&source_ref) {
        return Ok((
            "already_imported".into(),
            "already imported for this account".into(),
            source_ref,
            payload.to_string(),
        ));
    }
    let symbol = if instrument.is_empty() {
        asset.as_str()
    } else {
        instrument.as_str()
    };
    if let Some(fp) = csv_fingerprint(&occurred, symbol, &quantity, &price, &fee) {
        if let Some(prior) = index.fingerprints.get(&fp) {
            return Ok((
                "duplicate_suspect".into(),
                format!(
                    "suspected duplicate of source {prior}: same time, symbol, quantity, price and fee in another file"
                ),
                source_ref,
                payload.to_string(),
            ));
        }
    }
    Ok((
        "valid".into(),
        String::new(),
        source_ref,
        payload.to_string(),
    ))
}

/// Classify a previewed row again from its stored payload.
fn classify_payload(
    account_id: &str,
    payload: &str,
    index: &AccountImports,
) -> InfraResult<String> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let rec = csv::StringRecord::from(vec![
        field("occurred_at"),
        field("type"),
        field("asset"),
        field("quantity"),
        field("price"),
        field("fee"),
        field("source_ref"),
        field("instrument"),
        field("quote_currency"),
    ]);
    Ok(classify_csv_row(account_id, &rec, index)?.0)
}

fn norm_decimal(raw: &str) -> Option<String> {
    parse_decimal(raw).ok().map(|d| d.normalize().to_string())
}

fn csv_fingerprint(
    occurred: &str,
    symbol: &str,
    quantity: &str,
    price: &str,
    fee: &str,
) -> Option<String> {
    let when = canonical_ts(app_ts(occurred).ok()?);
    let qty = norm_decimal(quantity)?;
    let price = if price.is_empty() {
        String::new()
    } else {
        norm_decimal(price)?
    };
    let fee = if fee.is_empty() {
        "0".into()
    } else {
        norm_decimal(fee)?
    };
    Some(format!("{when}|{symbol}|{qty}|{price}|{fee}"))
}

fn event_fingerprint(event: &EconomicEvent) -> Option<String> {
    let when = canonical_ts(event.occurred_at);
    let (symbol, qty, price, fee) = match &event.payload {
        EventPayload::Buy {
            instrument,
            quantity,
            price,
            fees,
            ..
        }
        | EventPayload::Sell {
            instrument,
            quantity,
            price,
            fees,
            ..
        } => (
            instrument.0.clone(),
            quantity.normalize().to_string(),
            price.normalize().to_string(),
            fee_sum(fees),
        ),
        EventPayload::CashDeposit { asset, amount }
        | EventPayload::CashWithdrawal { asset, amount } => (
            asset.0.clone(),
            amount.normalize().to_string(),
            String::new(),
            "0".into(),
        ),
        EventPayload::DividendCash {
            instrument, amount, ..
        } => (
            instrument.0.clone(),
            amount.normalize().to_string(),
            String::new(),
            "0".into(),
        ),
        EventPayload::Fee { asset, amount, .. } | EventPayload::Interest { asset, amount } => (
            asset.0.clone(),
            amount.normalize().to_string(),
            String::new(),
            "0".into(),
        ),
        EventPayload::Transfer {
            asset,
            principal,
            fee,
            ..
        } => (
            asset.0.clone(),
            principal.normalize().to_string(),
            String::new(),
            fee.as_ref()
                .map(|f| f.amount.normalize().to_string())
                .unwrap_or_else(|| "0".into()),
        ),
        EventPayload::OpeningPosition { .. } | EventPayload::StockSplit { .. } => return None,
    };
    Some(format!("{when}|{symbol}|{qty}|{price}|{fee}"))
}

fn fee_sum(fees: &[Fee]) -> String {
    let mut total = Decimal::ZERO;
    for fee in fees {
        total += fee.amount;
    }
    total.normalize().to_string()
}

fn row_to_event(account_id: &str, payload: &str, row_no: i64) -> InfraResult<EconomicEvent> {
    let v: serde_json::Value = serde_json::from_str(payload)?;
    let kind = v["type"].as_str().unwrap_or("");
    let asset = AssetId(v["asset"].as_str().unwrap_or("").into());
    let qty = parse_decimal(v["quantity"].as_str().unwrap_or("0"))
        .map_err(|e| InfraError::Rejected(e.to_string()))?;
    let occurred = app_ts(v["occurred_at"].as_str().unwrap_or(""))?;
    let source_ref = v["source_ref"].as_str().map(|s| s.to_string());
    let quote = CurrencyCode::new(v["quote_currency"].as_str().unwrap_or("USD"));
    let instrument = InstrumentId(
        v["instrument"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("UNKNOWN")
            .into(),
    );
    let price = if v["price"].as_str().unwrap_or("").is_empty() {
        Decimal::ZERO
    } else {
        parse_decimal(v["price"].as_str().unwrap_or("0"))
            .map_err(|e| InfraError::Rejected(e.to_string()))?
    };
    let fee_amt = v["fee"].as_str().unwrap_or("");
    let fees = if fee_amt.is_empty() {
        Vec::new()
    } else {
        vec![Fee {
            asset: AssetId(quote.0.clone()),
            amount: parse_decimal(fee_amt).map_err(|e| InfraError::Rejected(e.to_string()))?,
            category: "commission".into(),
        }]
    };
    let body = match kind {
        "deposit" => EventPayload::CashDeposit { asset, amount: qty },
        "withdrawal" => EventPayload::CashWithdrawal { asset, amount: qty },
        "buy" => EventPayload::Buy {
            instrument,
            quantity: qty,
            price,
            quote_currency: quote,
            fees,
        },
        "sell" => EventPayload::Sell {
            instrument,
            quantity: qty,
            price,
            quote_currency: quote,
            fees,
        },
        "dividend" => EventPayload::DividendCash {
            cash_asset: asset,
            amount: qty,
            instrument,
        },
        "fee" => EventPayload::Fee {
            asset,
            amount: qty,
            category: "commission".into(),
        },
        other => return Err(InfraError::Rejected(format!("unknown type {other}"))),
    };
    Ok(EconomicEvent {
        id: EventId(uuid::Uuid::new_v4().to_string()),
        account_id: AccountId(account_id.into()),
        occurred_at: occurred,
        recorded_at: Utc::now(),
        seq: row_no,
        source_ref,
        correction_group: None,
        revision: 0,
        reverses: None,
        payload: body,
    })
}

// ---- market files, calendar, chart context -----------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketFile {
    pub dataset_version: String,
    pub source: String,
    #[serde(default)]
    pub bars: Vec<MarketBar>,
    #[serde(default)]
    pub fx: Vec<MarketFx>,
    #[serde(default)]
    pub sessions: Vec<MarketSession>,
    #[serde(default)]
    pub actions: Vec<MarketAction>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketBar {
    pub instrument: String,
    pub session_date: String,
    pub open_at: String,
    pub close_at: String,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    #[serde(default = "raw_adj")]
    pub adjustment: String,
}

fn raw_adj() -> String {
    "raw".into()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketFx {
    pub base: String,
    pub quote: String,
    pub rate: String,
    pub effective_at: String,
    pub observed_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketSession {
    pub venue: String,
    pub session_date: String,
    pub timezone: String,
    pub open_at: String,
    pub close_at: String,
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketAction {
    pub id: String,
    pub instrument: String,
    pub action_type: String,
    pub ratio_or_amount: String,
    pub pay_date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Open,
    Half,
    Holiday,
    Closed,
    Missing,
}

impl Library {
    pub fn import_market_file(&self, file: &MarketFile) -> InfraResult<()> {
        // Validate before opening the transaction, then publish all records as
        // one immutable dataset. A version id cannot silently change meaning.
        if file.dataset_version.trim().is_empty() || file.source.trim().is_empty() {
            return Err(InfraError::Rejected(
                "dataset version and source are required".into(),
            ));
        }
        let hash = sha256_hex(&serde_json::to_vec(file)?);
        let decimal = |s: &str| parse_decimal(s).map_err(|e| InfraError::Rejected(e.to_string()));
        let mut seen = std::collections::HashSet::new();
        for bar in &file.bars {
            if !bar.instrument.contains(':')
                || !seen.insert((&bar.instrument, &bar.session_date, &bar.adjustment))
            {
                return Err(InfraError::Rejected(
                    "venue-qualified instrument and unique daily bar required".into(),
                ));
            }
            let (open, close) = super::workbench::valid_period(&bar.open_at, &bar.close_at)?;
            let date = chrono::NaiveDate::parse_from_str(&bar.session_date, "%Y-%m-%d")
                .map_err(|_| InfraError::Rejected("invalid session date".into()))?;
            if date < open.date_naive() - chrono::Duration::days(1) || date > close.date_naive() {
                return Err(InfraError::Rejected(
                    "session date outside bar interval".into(),
                ));
            }
            let (o, h, l, c, v) = (
                decimal(&bar.open)?,
                decimal(&bar.high)?,
                decimal(&bar.low)?,
                decimal(&bar.close)?,
                decimal(&bar.volume)?,
            );
            if l <= Decimal::ZERO
                || l > o
                || l > c
                || h < o
                || h < c
                || h < l
                || v < Decimal::ZERO
                || !matches!(bar.adjustment.as_str(), "raw" | "split" | "total_return")
            {
                return Err(InfraError::Rejected("invalid OHLCV or adjustment".into()));
            }
        }
        for fx in &file.fx {
            if decimal(&fx.rate)? <= Decimal::ZERO {
                return Err(InfraError::Rejected("FX must be positive".into()));
            }
            app_ts(&fx.effective_at)?;
            app_ts(&fx.observed_at)?;
        }
        for session in &file.sessions {
            app_ts(&session.open_at)?;
            app_ts(&session.close_at)?;
        }
        self.with(|c| -> InfraResult<_> {
            let tx=c.unchecked_transaction()?;
            let prior:Option<String>=tx.query_row("SELECT manifest_hash FROM dataset_version WHERE id=?1",params![file.dataset_version],|r|r.get(0)).optional()?;
            if let Some(prior)=prior {
                if prior==hash { return Ok(()); }
                return Err(InfraError::Rejected("dataset version already exists with different content; use a new version".into()));
            }
            tx.execute("INSERT INTO dataset_version(id,manifest_hash,source,acquired_at,quality) VALUES(?1,?2,?3,?4,'file')",params![file.dataset_version,hash,file.source,Utc::now().to_rfc3339()])?;
            for bar in &file.bars {
                let (base,quote,venue)=split_instrument(&bar.instrument);
                tx.execute("INSERT OR IGNORE INTO asset(id,kind,symbol) VALUES(?1,'equity',?1)",params![base])?;
                tx.execute("INSERT OR IGNORE INTO asset(id,kind,symbol) VALUES(?1,'fiat',?1)",params![quote])?;
                tx.execute("INSERT OR IGNORE INTO instrument(id,kind,venue,base_asset_id,quote_asset_id) VALUES(?1,'spot',?2,?3,?4)",params![bar.instrument,venue,base,quote])?;
                tx.execute("INSERT INTO bar(instrument_id,timeframe,session_date,open_at,close_at,open,high,low,close,volume,source,adjustment,dataset_version) VALUES(?1,'1d',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![bar.instrument,bar.session_date,canonicalize_ts(&bar.open_at)?,canonicalize_ts(&bar.close_at)?,bar.open,bar.high,bar.low,bar.close,bar.volume,file.source,bar.adjustment,file.dataset_version])?;
            }
            for fx in &file.fx {
                tx.execute("INSERT INTO fx_rate(id,base,quote,rate,effective_at,observed_at,source,dataset_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![uuid::Uuid::new_v4().to_string(),fx.base,fx.quote,fx.rate,canonicalize_ts(&fx.effective_at)?,canonicalize_ts(&fx.observed_at)?,file.source,file.dataset_version])?;
            }
            for session in &file.sessions {
                tx.execute("INSERT INTO calendar_session(venue,session_date,timezone,open_at,close_at,kind,version) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![session.venue,session.session_date,session.timezone,canonicalize_ts(&session.open_at)?,canonicalize_ts(&session.close_at)?,session.kind,file.dataset_version])?;
            }
            for action in &file.actions {
                tx.execute("INSERT OR IGNORE INTO corporate_action(id,instrument_id,action_type,pay_date,ratio_or_amount,source) VALUES(?1,?2,?3,?4,?5,?6)",params![action.id,action.instrument,action.action_type,action.pay_date,action.ratio_or_amount,file.source])?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn dataset_count(&self, id: &str) -> InfraResult<i64> {
        self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT COUNT(*) FROM bar WHERE dataset_version = ?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
    }

    pub fn closes_visible(
        &self,
        instrument: &str,
        visible_until: &str,
        adjustment: &str,
    ) -> InfraResult<Vec<String>> {
        let until = crate::sqlite::store::canonicalize_ts(visible_until)?;
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT close FROM bar WHERE instrument_id = ?1 AND adjustment = ?2 AND close_at <= ?3 ORDER BY close_at",
            )?;
            let rows = stmt.query_map(params![instrument, adjustment, until], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    pub fn classify_session(&self, venue: &str, at: DateTime<Utc>) -> InfraResult<SessionStatus> {
        let at_s = canonical_ts(at);
        let row: Option<(String, String, String)> = self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT kind, open_at, close_at FROM calendar_session
                 WHERE venue = ?1 AND open_at <= ?2 AND close_at > ?2
                 ORDER BY version DESC LIMIT 1",
                params![venue, at_s],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(InfraError::Sqlite)
        })?;
        if let Some((kind, _, _)) = row {
            return Ok(match kind.as_str() {
                "holiday" => SessionStatus::Holiday,
                "half" => SessionStatus::Half,
                "full" => SessionStatus::Open,
                _ => SessionStatus::Closed,
            });
        }
        if venue.eq_ignore_ascii_case("CRYPTO") {
            return Ok(SessionStatus::Open);
        }
        let date = at.date_naive().to_string();
        let known: Option<String> = self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT kind FROM calendar_session WHERE venue = ?1 AND session_date = ?2 ORDER BY version DESC LIMIT 1",
                params![venue, date],
                |r| r.get(0),
            )
            .optional()
            .map_err(InfraError::Sqlite)
        })?;
        Ok(match known.as_deref() {
            Some("holiday") => SessionStatus::Holiday,
            Some("half") | Some("full") => SessionStatus::Closed,
            Some(_) => SessionStatus::Closed,
            None => SessionStatus::Missing,
        })
    }

    pub fn save_chart_context(
        &self,
        instrument: &str,
        timeframe: &str,
        start_at: &str,
        end_at: &str,
        adjustment: &str,
        params_json: &str,
    ) -> InfraResult<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO chart_context (id, instrument_id, timeframe, start_at, end_at, adjustment, params, saved_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, instrument, timeframe, start_at, end_at, adjustment, params_json, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    pub fn latest_chart_context(
        &self,
        instrument: &str,
    ) -> InfraResult<Option<(String, String, String, String)>> {
        self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT timeframe, start_at, end_at, adjustment, params FROM chart_context WHERE instrument_id = ?1 ORDER BY saved_at DESC LIMIT 1",
                params![instrument],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(Into::into)
        })
    }
}

fn split_instrument(id: &str) -> (String, String, String) {
    let (venue, pair) = id.split_once(':').unwrap_or(("UNKNOWN", id));
    if let Some((base, quote)) = pair.split_once('/') {
        (base.into(), quote.into(), venue.into())
    } else {
        (pair.into(), "USD".into(), venue.into())
    }
}

// ---- notes, templates, links, attachments ------------------------------------

#[derive(Debug, Clone)]
pub struct NoteSession {
    pub id: Option<String>,
    pub title: String,
    pub draft: String,
    pub saved: String,
    pub account_id: String,
    pub saved_ok: bool,
    pub error: Option<String>,
}

impl NoteSession {
    pub fn new(title: &str, account_id: &str) -> Self {
        Self {
            id: None,
            title: title.into(),
            draft: String::new(),
            saved: String::new(),
            account_id: account_id.into(),
            saved_ok: false,
            error: None,
        }
    }

    pub fn edit(&mut self, body: &str) {
        self.draft = body.into();
        self.saved_ok = false;
    }

    pub fn autosave(&mut self, library: &Library) -> InfraResult<()> {
        match library.save_journal(
            &self.title,
            &self.draft,
            &[],
            self.id.as_deref(),
            Some(&self.account_id),
        ) {
            Ok(id) => {
                self.id = Some(id);
                self.saved = self.draft.clone();
                self.saved_ok = true;
                self.error = None;
                Ok(())
            }
            Err(e) => {
                self.saved_ok = false;
                self.error = Some(e.to_string());
                Err(e)
            }
        }
    }
}

impl Library {
    pub fn save_template(&self, slot: &str, body: &str) -> InfraResult<()> {
        if !matches!(slot, "prefix" | "middle" | "suffix") {
            return Err(InfraError::Rejected("unknown template slot".into()));
        }
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO note_template (id, slot, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT(slot) DO UPDATE SET body = excluded.body",
                params![uuid::Uuid::new_v4().to_string(), slot, body],
            )?;
            Ok(())
        })
    }

    pub fn render_note(&self, body: &str) -> InfraResult<String> {
        let slot = |name: &str, c: &Connection| -> InfraResult<String> {
            Ok(c.query_row(
                "SELECT body FROM note_template WHERE slot = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
        };
        self.with(|c| -> InfraResult<_> {
            let prefix = slot("prefix", c)?;
            let middle = slot("middle", c)?;
            let suffix = slot("suffix", c)?;
            Ok(format!("{prefix}{body}{middle}{suffix}"))
        })
    }

    pub fn allocate_link(
        &self,
        journal_id: &str,
        target_id: &str,
        quantity: &str,
        capacity: &str,
    ) -> InfraResult<()> {
        let qty = parse_decimal(quantity).map_err(|e| InfraError::Rejected(e.to_string()))?;
        let cap = parse_decimal(capacity).map_err(|e| InfraError::Rejected(e.to_string()))?;
        if qty <= Decimal::ZERO || cap <= Decimal::ZERO {
            return Err(InfraError::Rejected(
                "allocation and capacity must be positive".into(),
            ));
        }
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT relation FROM journal_link WHERE target_id = ?1 AND tombstone = 0",
            )?;
            let rows = stmt.query_map(params![target_id], |r| r.get::<_, String>(0))?;
            let mut used = Decimal::ZERO;
            for row in rows {
                let raw = row?;
                if let Ok(n) = parse_decimal(&raw) {
                    used = used
                        .checked_add(n)
                        .ok_or_else(|| InfraError::Rejected("allocation overflow".into()))?;
                }
            }
            let next = used
                .checked_add(qty)
                .ok_or_else(|| InfraError::Rejected("allocation overflow".into()))?;
            if next > cap {
                return Err(InfraError::Rejected(format!(
                    "allocation {next} exceeds capacity {cap}"
                )));
            }
            c.execute(
                "INSERT INTO journal_link (id, journal_id, target_type, target_id, relation) VALUES (?1, ?2, 'fill', ?3, ?4)",
                params![uuid::Uuid::new_v4().to_string(), journal_id, target_id, quantity],
            )?;
            Ok(())
        })
    }

    /// Notes whose linked event was later corrected. The link stays; the
    /// caller must show that the source changed.
    pub fn link_notices(&self, journal_id: &str) -> InfraResult<Vec<String>> {
        let targets: Vec<String> = self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT target_id FROM journal_link WHERE journal_id = ?1 AND tombstone = 0",
            )?;
            let rows = stmt.query_map(params![journal_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(InfraError::Sqlite)
        })?;
        let mut notes = Vec::new();
        for target in targets {
            let revised: i64 = self.with(|c| -> InfraResult<_> {
                c.query_row(
                    "SELECT COALESCE(MAX(revision), 0) FROM economic_event
                     WHERE id = ?1
                        OR correction_group = (SELECT correction_group FROM economic_event WHERE id = ?1)",
                    params![target],
                    |r| r.get(0),
                )
                .map_err(InfraError::Sqlite)
            })?;
            if revised > 0 {
                notes.push(format!(
                    "linked event {target} was corrected; reopen the source before relying on it"
                ));
            }
        }
        Ok(notes)
    }

    /// Folder that holds this library's attachment files.
    pub fn attachments_path(&self) -> PathBuf {
        attachment_dir(&self.path)
    }

    pub fn save_attachment(&self, bytes: &[u8], mime: &str) -> InfraResult<(String, String)> {
        let hash = sha256_hex(bytes);
        let dir = attachment_dir(&self.path);
        fs::create_dir_all(&dir)?;
        let final_path = dir.join(&hash);
        let tmp = dir.join(format!("{hash}.partial"));
        fs::write(&tmp, bytes)?;
        let written = fs::read(&tmp)?;
        if sha256_hex(&written) != hash {
            let _ = fs::remove_file(&tmp);
            return Err(InfraError::Rejected(
                "attachment write did not match its hash".into(),
            ));
        }
        fs::rename(&tmp, &final_path)?;
        let relative = format!("attachments/{hash}");
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO attachment (id, hash, mime_type, size, relative_path) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, hash, mime, bytes.len() as i64, relative],
            )?;
            Ok(())
        })?;
        Ok((id, hash))
    }

    pub fn read_attachment(&self, id: &str) -> InfraResult<Vec<u8>> {
        let (hash, relative): (String, String) = self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT hash, relative_path FROM attachment WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| InfraError::NotFound(format!("attachment {id}")))
        })?;
        let path = attachment_dir(&self.path).join(attachment_name(&relative)?);
        if !path.is_file() {
            return Err(InfraError::NotFound(format!(
                "attachment file missing: {relative}"
            )));
        }
        let bytes = fs::read(&path)?;
        if sha256_hex(&bytes) != hash {
            return Err(InfraError::Rejected(
                "attachment hash does not match stored bytes".into(),
            ));
        }
        Ok(bytes)
    }
}

/// `attachments/` in the library's own folder. A folder holds one library
/// (system-architecture §9; `ensure_sole_library`), so deleting or backing
/// up a library never touches another library's files.
fn attachment_dir(library_path: &Path) -> PathBuf {
    library_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("attachments")
}

/// Stored and archived attachment paths are `attachments/<plain name>`.
fn attachment_name(relative: &str) -> InfraResult<&str> {
    let name = relative.strip_prefix("attachments/").unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
        return Err(InfraError::InvalidPath(
            "attachment path escapes the library".into(),
        ));
    }
    Ok(name)
}

// ---- valuation helpers used by UI and the host --------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLine {
    pub account: String,
    pub asset: String,
    pub quantity: String,
    pub cost: String,
}

impl Library {
    pub fn account_lines(&self) -> InfraResult<Vec<AccountLine>> {
        self.account_lines_at(None)
    }

    /// Lines from events up to and including `as_of` (`None` = all).
    pub fn account_lines_at(&self, as_of: Option<DateTime<Utc>>) -> InfraResult<Vec<AccountLine>> {
        let engine = self.rebuild_engine(as_of.map(just_after))?;
        let mut lines = Vec::new();
        for (account, ledger) in &engine.accounts {
            push_lines(&mut lines, account, ledger);
        }
        lines.sort_by(|a, b| (&a.account, &a.asset).cmp(&(&b.account, &b.asset)));
        Ok(lines)
    }

    /// One rebuild written as balance rows so the first screen does not
    /// rebuild the ledger again.
    pub fn refresh_balance_cache(&self) -> InfraResult<()> {
        let engine = self.rebuild_engine(None)?;
        let as_of = canonical_ts(Utc::now());
        self.with(|c| -> InfraResult<_> {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM balance_snapshot WHERE source = 'cache'", [])?;
            for (account, ledger) in &engine.accounts {
                for (asset, qty) in &ledger.cash {
                    if *qty == Decimal::ZERO {
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO balance_snapshot (id, account_id, asset, quantity, as_of, source)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'cache')",
                        params![
                            uuid::Uuid::new_v4().to_string(),
                            account.0,
                            asset.0,
                            qty.normalize().to_string(),
                            as_of
                        ],
                    )?;
                }
                for (asset, holding) in &ledger.holdings {
                    if holding.quantity() == Decimal::ZERO {
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO balance_snapshot (id, account_id, asset, quantity, as_of, source)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'cache')",
                        params![
                            uuid::Uuid::new_v4().to_string(),
                            account.0,
                            asset.0,
                            holding.quantity().normalize().to_string(),
                            as_of
                        ],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Read the cached balance rows only. This is the first-screen path.
    pub fn cached_lines(&self) -> InfraResult<Vec<AccountLine>> {
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT account_id, asset, quantity FROM balance_snapshot WHERE source = 'cache' ORDER BY account_id, asset",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(AccountLine {
                    account: r.get(0)?,
                    asset: r.get(1)?,
                    quantity: r.get(2)?,
                    cost: "—".into(),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(InfraError::Sqlite)
        })
    }

    pub fn diagnostics_bundle(&self) -> InfraResult<String> {
        let tz = self.timezone()?;
        let ccy = self.reporting_currency()?;
        Ok(format!(
            "timezone={tz}\nreporting_currency={ccy}\nbook_currency={}\ncredentials=omitted\n",
            self.book_currency
        ))
    }

    pub fn portfolio_value(
        &self,
        scope: &ScopeSnapshot,
        as_of: DateTime<Utc>,
    ) -> InfraResult<ValuationSummary> {
        let engine = self.rebuild_engine(Some(just_after(as_of)))?;
        let prices = self.price_source();
        let fx = self.fx_resolver();
        valuation::value_accounts(
            &engine,
            as_of,
            &prices,
            &fx,
            &scope.reporting_currency,
            &|id| scope.covers_account(id),
        )
        .map_err(|e| InfraError::Rejected(e.to_string()))
    }

    pub fn add_manual_mark(&self, mark: &ManualMark) -> InfraResult<()> {
        self.ensure_account(mark.account_id.as_str(), mark.account_id.as_str(), "manual")?;
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO manual_valuation (id, account_id, asset, value, currency, as_of, method)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    mark.account_id.0,
                    mark.asset.0,
                    mark.value.to_string(),
                    mark.currency.0,
                    canonical_ts(mark.as_of),
                    match mark.kind {
                        ManualKind::Asset => "asset",
                        ManualKind::Liability => "liability",
                    }
                ],
            )?;
            Ok(())
        })
    }

    pub fn wealth(
        &self,
        scope: &ScopeSnapshot,
        as_of: DateTime<Utc>,
    ) -> InfraResult<delta_core::valuation::WealthView> {
        let summary = self.portfolio_value(scope, as_of)?;
        let marks = self.manual_marks()?;
        let fx = self.fx_resolver();
        apply_manual_marks(
            &summary,
            &marks,
            as_of,
            &fx,
            &scope.reporting_currency,
            &|id| scope.covers_account(id),
        )
        .map_err(|e| InfraError::Rejected(e.to_string()))
    }

    fn manual_marks(&self) -> InfraResult<Vec<ManualMark>> {
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT account_id, asset, value, currency, as_of, method FROM manual_valuation ORDER BY as_of, rowid",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (account, asset, value, currency, as_of, method) = row?;
                let kind = if method == "liability" {
                    ManualKind::Liability
                } else {
                    ManualKind::Asset
                };
                out.push(ManualMark {
                    account_id: AccountId(account),
                    asset: AssetId(asset),
                    value: parse_decimal(&value).map_err(|e| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            e.to_string(),
                        )))
                    })?,
                    currency: CurrencyCode::new(currency),
                    as_of: DateTime::parse_from_rfc3339(&as_of)
                        .map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(e.to_string()),
                            ))
                        })?
                        .with_timezone(&Utc),
                    kind,
                    valid_until: None,
                });
            }
            Ok(out)
        })
    }

    pub fn observe_balance(
        &self,
        account: &str,
        asset: &str,
        quantity: &str,
        as_of: &str,
    ) -> InfraResult<()> {
        parse_decimal(quantity).map_err(|e| InfraError::Rejected(e.to_string()))?;
        let as_of = crate::sqlite::store::canonicalize_ts(as_of)?;
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO balance_snapshot (id, account_id, asset, quantity, as_of, source) VALUES (?1, ?2, ?3, ?4, ?5, 'manual')",
                params![uuid::Uuid::new_v4().to_string(), account, asset, quantity, as_of],
            )?;
            Ok(())
        })
    }

    /// Each observation is compared with the ledger at its own `as_of`.
    /// Cached first-screen rows are derived from the ledger, not observed.
    pub fn reconcile(
        &self,
        scope: &ScopeSnapshot,
    ) -> InfraResult<Vec<delta_core::valuation::BalanceDiff>> {
        let observations = self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT account_id, asset, quantity, as_of FROM balance_snapshot
                 WHERE source != 'cache' ORDER BY as_of, rowid",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (account, asset, qty, as_of) = row?;
                out.push(BalanceObservation {
                    account_id: AccountId(account),
                    asset: AssetId(asset),
                    quantity: parse_decimal(&qty).map_err(|e| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            e.to_string(),
                        )))
                    })?,
                    as_of: DateTime::parse_from_rfc3339(&as_of)
                        .map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(e.to_string()),
                            ))
                        })?
                        .with_timezone(&Utc),
                });
            }
            Ok(out)
        })?;
        let mut diffs = Vec::new();
        for chunk in observations.chunk_by(|a, b| a.as_of == b.as_of) {
            let engine = self.rebuild_engine(Some(just_after(chunk[0].as_of)))?;
            diffs.extend(
                reconcile_balances(&engine, chunk, &|id| scope.covers_account(id))
                    .map_err(|e| InfraError::Rejected(e.to_string()))?,
            );
        }
        Ok(diffs)
    }

    pub fn explain(&self, scope: &ScopeSnapshot) -> InfraResult<pnl::PeriodPnl> {
        let start_engine = self.rebuild_engine(Some(scope.start_at))?;
        let end_engine = self.rebuild_engine(Some(scope.end_at))?;
        let events = self.recorded_events(Some(
            &scope
                .account_ids
                .iter()
                .map(|a| a.0.clone())
                .collect::<Vec<_>>(),
        ))?;
        let period: Vec<_> = events
            .into_iter()
            .filter(|e| e.occurred_at >= scope.start_at && e.occurred_at < scope.end_at)
            .collect();
        let prices = self.price_source();
        let fx = self.fx_resolver();
        pnl::period_pnl(
            &start_engine,
            &end_engine,
            &period,
            &scope.account_ids,
            scope.start_at,
            scope.end_at,
            &prices,
            &fx,
            &scope.reporting_currency,
        )
        .map_err(|e| InfraError::Rejected(e.to_string()))
    }

    /// Open a persisted evidence id: a tool result, an event, or one journal revision.
    pub fn open_evidence(&self, evidence_id: &str) -> InfraResult<OpenedEvidence> {
        if let Some(rest) = evidence_id.strip_prefix("event:") {
            let body = self.event_payload_json(rest)?;
            return Ok(OpenedEvidence {
                evidence_id: evidence_id.into(),
                target_type: "event".into(),
                target_id: rest.into(),
                revision: None,
                body,
            });
        }
        if let Some(rest) = evidence_id.strip_prefix("journal:") {
            // A note is evidence only at the revision that was cited; the
            // current text may say something else (context-state §2).
            let Some((journal_id, revision)) = rest.split_once(":r") else {
                return Err(InfraError::NotFound(format!(
                    "{evidence_id} names no revision"
                )));
            };
            let (journal_id, revision) = (journal_id.to_string(), revision.to_string());
            let body: String = self.with(|c| -> InfraResult<_> {
                c.query_row(
                    "SELECT body FROM journal_revision WHERE journal_id = ?1 AND revision = ?2",
                    params![journal_id, revision],
                    |r| r.get(0),
                )
                .map_err(|_| InfraError::NotFound(evidence_id.into()))
            })?;
            return Ok(OpenedEvidence {
                evidence_id: evidence_id.into(),
                target_type: "journal".into(),
                target_id: journal_id,
                revision: Some(revision),
                body,
            });
        }
        if evidence_id.starts_with("res:") {
            let (metric, body): (String, String) = self.with(|c| -> InfraResult<_> {
                c.query_row(
                    "SELECT metric, value FROM analysis_result WHERE id = ?1",
                    params![evidence_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| InfraError::NotFound(evidence_id.into()))
            })?;
            return Ok(OpenedEvidence {
                evidence_id: evidence_id.into(),
                target_type: "result".into(),
                target_id: evidence_id.into(),
                revision: None,
                body: format!("{metric}\n{body}"),
            });
        }
        Err(InfraError::NotFound(format!(
            "unknown evidence id {evidence_id}"
        )))
    }

    /// Store a report and the evidence it may cite. Each ref keeps its target
    /// type and, for a note, the revision that was current.
    pub fn save_report_evidence(
        &self,
        body: &str,
        scope: &str,
        refs: &[String],
    ) -> InfraResult<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| -> InfraResult<_> {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO analysis_report (id, report_type, scope, status, created_at, body) VALUES (?1, 'analysis', ?2, 'active', ?3, ?4)",
                params![id, scope, Utc::now().to_rfc3339(), body],
            )?;
            for evidence in refs {
                let (target_type, target_id, revision) = evidence_target(evidence);
                tx.execute(
                    "INSERT OR REPLACE INTO evidence_ref (report_id, target_type, target_id, revision, sample_count)
                     VALUES (?1, ?2, ?3, ?4, NULL)",
                    params![id, target_type, target_id, revision],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        Ok(id)
    }

    pub fn journal_evidence_id(&self, journal_id: &str) -> InfraResult<String> {
        let rev: i64 = self.with(|c| -> InfraResult<_> {
            c.query_row(
                "SELECT current_revision FROM journal_entry WHERE id = ?1",
                params![journal_id],
                |r| r.get(0),
            )
            .map_err(|_| InfraError::NotFound(journal_id.into()))
        })?;
        Ok(format!("journal:{journal_id}:r{rev}"))
    }

    pub fn store_analysis_result(
        &self,
        id: &str,
        scope_snapshot: &str,
        metric: &str,
        value: &str,
        quality: &str,
    ) -> InfraResult<()> {
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "INSERT INTO analysis_result (id, scope_snapshot, metric, value, quality, inputs_hash, calculation_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?1, 'r1', ?6)
                 ON CONFLICT(id) DO NOTHING",
                params![id, scope_snapshot, metric, value, quality, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Add an instrument to the watchlist. The id must name a known
    /// instrument (venue and pair included), and adding it twice is a no-op.
    pub fn add_watch(&self, instrument_id: &str) -> InfraResult<()> {
        self.with(|c| -> InfraResult<_> {
            let known: i64 = c.query_row(
                "SELECT COUNT(*) FROM instrument WHERE id = ?1",
                params![instrument_id],
                |r| r.get(0),
            )?;
            if known == 0 {
                return Err(InfraError::Rejected(format!(
                    "unknown instrument: {instrument_id}"
                )));
            }
            c.execute(
                "INSERT INTO watch_item (instrument_id, added_at) VALUES (?1, ?2)
                 ON CONFLICT(instrument_id) DO NOTHING",
                params![instrument_id, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Remove an instrument from the watchlist; removing one that is not
    /// watched is a no-op.
    pub fn remove_watch(&self, instrument_id: &str) -> InfraResult<()> {
        self.with(|c| -> InfraResult<_> {
            c.execute(
                "DELETE FROM watch_item WHERE instrument_id = ?1",
                params![instrument_id],
            )?;
            Ok(())
        })
    }

    pub fn watchlist(&self) -> InfraResult<Vec<String>> {
        self.with(|c| -> InfraResult<_> {
            let mut stmt =
                c.prepare("SELECT instrument_id FROM watch_item ORDER BY instrument_id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Instrument ids whose id or base asset contains `query`. Identity is the
    /// full id, so a search for AAPL does not return another venue's lookalike.
    /// `%`, `_` and `\` in the query are literal characters, not wildcards.
    pub fn search_instruments(&self, query: &str) -> InfraResult<Vec<String>> {
        Ok(self
            .search_instrument_hits(query)?
            .into_iter()
            .map(|hit| hit.id)
            .collect())
    }

    /// Like [`Self::search_instruments`], with the venue and the pair of each
    /// hit so a screen can tell the same symbol on two venues apart. A blank
    /// query lists every instrument.
    pub fn search_instrument_hits(&self, query: &str) -> InfraResult<Vec<InstrumentHit>> {
        let like = like_contains(query);
        self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT id, venue, base_asset_id, quote_asset_id FROM instrument
                 WHERE id LIKE ?1 ESCAPE '\\' OR base_asset_id LIKE ?1 ESCAPE '\\'
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![like], |r| {
                Ok(InstrumentHit {
                    id: r.get(0)?,
                    venue: r.get(1)?,
                    base: r.get(2)?,
                    quote: r.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }
}

/// One instrument search result: the full id plus its venue and pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentHit {
    pub id: String,
    pub venue: String,
    pub base: String,
    pub quote: String,
}

/// `%query%` for a `LIKE ... ESCAPE '\'` clause, with the wildcards of the
/// query escaped so that `%` and `_` match themselves.
fn like_contains(query: &str) -> String {
    let mut pattern = String::from("%");
    for c in query.trim().chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedEvidence {
    pub evidence_id: String,
    pub target_type: String,
    pub target_id: String,
    pub revision: Option<String>,
    pub body: String,
}

fn evidence_target(evidence_id: &str) -> (String, String, Option<String>) {
    if let Some(rest) = evidence_id.strip_prefix("event:") {
        return ("event".into(), rest.into(), None);
    }
    if let Some(rest) = evidence_id.strip_prefix("journal:") {
        if let Some((id, rev)) = rest.split_once(":r") {
            return ("journal".into(), id.into(), Some(rev.into()));
        }
        return ("journal".into(), rest.into(), None);
    }
    ("result".into(), evidence_id.into(), None)
}

/// Inclusive cut-off for `rebuild_engine`, which keeps events strictly
/// before its bound.
fn just_after(at: DateTime<Utc>) -> DateTime<Utc> {
    at.checked_add_signed(chrono::Duration::nanoseconds(1))
        .unwrap_or(at)
}

fn push_lines(lines: &mut Vec<AccountLine>, account: &AccountId, ledger: &AccountLedger) {
    for (asset, qty) in &ledger.cash {
        if *qty == Decimal::ZERO {
            continue;
        }
        lines.push(AccountLine {
            account: account.0.clone(),
            asset: asset.0.clone(),
            quantity: qty.normalize().to_string(),
            cost: "—".into(),
        });
    }
    for (asset, holding) in &ledger.holdings {
        if holding.quantity() == Decimal::ZERO {
            continue;
        }
        lines.push(AccountLine {
            account: account.0.clone(),
            asset: asset.0.clone(),
            quantity: holding.quantity().normalize().to_string(),
            cost: holding
                .carrying_cost()
                .map(|c| c.normalize().to_string())
                .unwrap_or_else(|| "unknown".into()),
        });
    }
}

/// Build the synthetic library the desktop account page reads.
pub fn seed_synthetic_demo(path: &Path) -> InfraResult<Library> {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    let lib = Library::create(path, "USD")?;
    lib.ensure_asset("USD", "fiat")?;
    lib.ensure_asset("USDT", "stablecoin")?;
    lib.ensure_asset("AAPL", "equity")?;
    lib.ensure_asset("BTC", "crypto")?;
    lib.ensure_asset("BNB", "crypto")?;
    lib.ensure_account("acc-us", "US brokerage", "broker")?;
    lib.ensure_account("acc-crypto", "Crypto", "exchange")?;
    lib.ensure_account("acc-wallet", "Wallet", "wallet")?;
    lib.ensure_instrument("NASDAQ:AAPL", "AAPL", "USD", "NASDAQ")?;
    lib.ensure_instrument("BINANCE:BNBUSDT", "BNB", "USDT", "BINANCE")?;
    lib.ensure_instrument("BINANCE:BTCUSDT", "BTC", "USDT", "BINANCE")?;
    lib.upsert_fx(
        "USDT",
        "USD",
        "1",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
        "file",
    )?;
    lib.upsert_fx(
        "BNB",
        "USDT",
        "60000",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
        "file",
    )?;
    let events = vec![
        cash(
            "acc-us",
            "us-dep",
            "2026-01-02T00:00:00Z",
            1,
            "USD",
            "2000",
            true,
        ),
        trade(
            "acc-us",
            "us-buy",
            "2026-01-03T00:00:00Z",
            2,
            true,
            "NASDAQ:AAPL",
            "10",
            "100",
            "USD",
            Some("1"),
        ),
        trade(
            "acc-us",
            "us-sell",
            "2026-01-04T00:00:00Z",
            3,
            false,
            "NASDAQ:AAPL",
            "4",
            "110",
            "USD",
            Some("1"),
        ),
        cash(
            "acc-crypto",
            "cx-dep",
            "2026-04-01T00:00:00Z",
            1,
            "USDT",
            "20000",
            true,
        ),
        trade(
            "acc-crypto",
            "cx-bnb",
            "2026-04-01T00:00:00Z",
            2,
            true,
            "BINANCE:BNBUSDT",
            "0.01",
            "60000",
            "USDT",
            None,
        ),
        trade_bnb_fee("acc-crypto", "cx-btc", "2026-04-02T00:00:00Z", 3),
        btc_move("acc-crypto", "cx-out", "2026-04-03T00:00:00Z", 4, true),
        btc_move("acc-wallet", "cx-in", "2026-04-03T00:00:00Z", 5, false),
    ];
    lib.record_events(&events, None)?;
    Ok(lib)
}

fn cash(
    account: &str,
    id: &str,
    at: &str,
    seq: i64,
    asset: &str,
    amount: &str,
    deposit: bool,
) -> EconomicEvent {
    let amount = parse_decimal(amount).expect("demo decimal");
    let payload = if deposit {
        EventPayload::CashDeposit {
            asset: AssetId(asset.into()),
            amount,
        }
    } else {
        EventPayload::CashWithdrawal {
            asset: AssetId(asset.into()),
            amount,
        }
    };
    demo_event(account, id, at, seq, None, payload)
}

#[allow(clippy::too_many_arguments)]
fn trade(
    account: &str,
    id: &str,
    at: &str,
    seq: i64,
    buy: bool,
    instrument: &str,
    qty: &str,
    price: &str,
    quote: &str,
    fee: Option<&str>,
) -> EconomicEvent {
    let fees = fee
        .map(|f| {
            vec![Fee {
                asset: AssetId(quote.into()),
                amount: parse_decimal(f).expect("fee"),
                category: "commission".into(),
            }]
        })
        .unwrap_or_default();
    let payload = if buy {
        EventPayload::Buy {
            instrument: InstrumentId(instrument.into()),
            quantity: parse_decimal(qty).expect("qty"),
            price: parse_decimal(price).expect("price"),
            quote_currency: CurrencyCode::new(quote),
            fees,
        }
    } else {
        EventPayload::Sell {
            instrument: InstrumentId(instrument.into()),
            quantity: parse_decimal(qty).expect("qty"),
            price: parse_decimal(price).expect("price"),
            quote_currency: CurrencyCode::new(quote),
            fees,
        }
    };
    demo_event(account, id, at, seq, Some(id), payload)
}

fn btc_move(account: &str, id: &str, at: &str, seq: i64, outbound: bool) -> EconomicEvent {
    let (kind, counterparty) = if outbound {
        (TransferKind::Out, "acc-wallet")
    } else {
        (TransferKind::In, "acc-crypto")
    };
    demo_event(
        account,
        id,
        at,
        seq,
        Some(id),
        EventPayload::Transfer {
            kind,
            group: TransferGroupId("btcmove".into()),
            counterparty: AccountId(counterparty.into()),
            asset: AssetId("BTC".into()),
            principal: parse_decimal("0.5").expect("qty"),
            fee: None,
        },
    )
}

fn trade_bnb_fee(account: &str, id: &str, at: &str, seq: i64) -> EconomicEvent {
    demo_event(
        account,
        id,
        at,
        seq,
        Some(id),
        EventPayload::Buy {
            instrument: InstrumentId("BINANCE:BTCUSDT".into()),
            quantity: parse_decimal("0.5").unwrap(),
            price: parse_decimal("20000").unwrap(),
            quote_currency: CurrencyCode::new("USDT"),
            fees: vec![Fee {
                asset: AssetId("BNB".into()),
                amount: parse_decimal("0.001").unwrap(),
                category: "platform".into(),
            }],
        },
    )
}

fn demo_event(
    account: &str,
    id: &str,
    at: &str,
    seq: i64,
    source: Option<&str>,
    payload: EventPayload,
) -> EconomicEvent {
    let at = app_ts(at).expect("demo time");
    EconomicEvent {
        id: EventId(id.into()),
        account_id: AccountId(account.into()),
        occurred_at: at,
        recorded_at: at,
        seq,
        source_ref: source.map(|s| s.to_string()),
        correction_group: None,
        revision: 0,
        reverses: None,
        payload,
    }
}

pub fn load_demo_lines(dir: &Path) -> InfraResult<Vec<AccountLine>> {
    let lib = seed_synthetic_demo(&dir.join("demo.sqlite"))?;
    lib.account_lines()
}

// ---- backup, export, delete, credentials --------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackupManifest {
    pub files: Vec<BackupFile>,
    pub note: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackupFile {
    pub path: String,
    pub sha256: String,
}

impl Library {
    pub fn backup_to(&self, dest: &Path) -> InfraResult<BackupManifest> {
        if dest.exists() && fs::read_dir(dest)?.next().is_some() {
            return Err(InfraError::Rejected(
                "backup destination directory must be empty".into(),
            ));
        }
        fs::create_dir_all(dest)?;
        let db_path = dest.join("library.sqlite");
        if db_path.exists() {
            return Err(InfraError::Rejected(
                "backup destination already has a library".into(),
            ));
        }
        let escaped = db_path
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "''");
        self.with(|src| -> InfraResult<_> {
            src.execute_batch(&format!("VACUUM INTO '{escaped}'"))?;
            Ok(())
        })?;
        let mut files = vec![BackupFile {
            path: "library.sqlite".into(),
            sha256: sha256_hex(&fs::read(&db_path)?),
        }];
        let attachments = attachment_dir(&self.path);
        if attachments.is_dir() {
            let out_dir = dest.join("attachments");
            fs::create_dir_all(&out_dir)?;
            for entry in fs::read_dir(&attachments)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let name = entry.file_name();
                    let bytes = fs::read(entry.path())?;
                    fs::write(out_dir.join(&name), &bytes)?;
                    files.push(BackupFile {
                        path: format!("attachments/{}", name.to_string_lossy()),
                        sha256: sha256_hex(&bytes),
                    });
                }
            }
        }
        let manifest = BackupManifest {
            files,
            note: "OS credentials are not part of this backup".into(),
        };
        fs::write(
            dest.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(manifest)
    }

    pub fn export_to(&self, dest: &Path) -> InfraResult<()> {
        fs::create_dir_all(dest)?;
        let lines = self.account_lines()?;
        let mut csv_out = csv::Writer::from_writer(Vec::new());
        csv_out.write_record(["account", "asset", "quantity", "cost"])?;
        for line in &lines {
            csv_out.write_record([&line.account, &line.asset, &line.quantity, &line.cost])?;
        }
        let csv_bytes = csv_out
            .into_inner()
            .map_err(|e| InfraError::Rejected(e.to_string()))?;
        fs::write(dest.join("accounts.csv"), csv_bytes)?;
        let events = self.recorded_events(None)?;
        fs::write(
            dest.join("events.json"),
            serde_json::to_vec_pretty(&events)?,
        )?;
        let notes = self.with(|c| -> InfraResult<_> {
            let mut stmt = c.prepare(
                "SELECT j.id, j.title, jr.body FROM journal_entry j
                 JOIN journal_revision jr ON jr.journal_id = j.id AND jr.revision = j.current_revision
                 ORDER BY j.created_at",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<Vec<(String, String, String)>, _>>()
                .map_err(InfraError::Sqlite)
        })?;
        let mut md = String::from("# Notes\n\n");
        if notes.is_empty() {
            md.push_str("No notes.\n");
        }
        for (id, title, body) in notes {
            md.push_str(&format!("## {title}\n\n{body}\n\n_id: {id}_\n\n"));
        }
        md.push_str(&format!(
            "\nQuantities are decimal strings in the asset's own unit. Cost is FIFO carrying cost in the book currency {}.\n",
            self.book_currency
        ));
        fs::write(dest.join("notes.md"), md)?;
        Ok(())
    }
}

pub fn restore_backup(src: &Path, dest_sqlite: &Path) -> InfraResult<()> {
    if dest_sqlite.exists() {
        return Err(InfraError::Rejected(
            "refusing to overwrite an existing library".into(),
        ));
    }
    ensure_sole_library(dest_sqlite)?;
    let parent = dest_sqlite
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| InfraError::InvalidPath("restore needs a new directory".into()))?;
    if parent.exists()
        && (fs::symlink_metadata(parent)?.file_type().is_symlink()
            || fs::read_dir(parent)?.next().is_some())
    {
        return Err(InfraError::Rejected(
            "restore destination directory must be empty".into(),
        ));
    }
    let manifest: BackupManifest = serde_json::from_slice(&fs::read(src.join("manifest.json"))?)?;
    let mut names = std::collections::HashSet::new();
    for entry in &manifest.files {
        if !names.insert(entry.path.clone()) {
            return Err(InfraError::BackupVerification(
                "duplicate manifest entry".into(),
            ));
        }
        if entry.path != "library.sqlite" {
            attachment_name(&entry.path)?;
        }
        let path = src.join(&entry.path);
        if fs::symlink_metadata(&path)?.file_type().is_symlink()
            || (entry.path.starts_with("attachments/")
                && fs::symlink_metadata(src.join("attachments"))?
                    .file_type()
                    .is_symlink())
        {
            return Err(InfraError::InvalidPath(
                "backup links are not allowed".into(),
            ));
        }
        if sha256_hex(&fs::read(&path)?) != entry.sha256 {
            return Err(InfraError::BackupVerification(format!(
                "hash mismatch for {}",
                entry.path
            )));
        }
    }
    if !names.contains("library.sqlite") {
        return Err(InfraError::BackupVerification(
            "manifest does not list library.sqlite".into(),
        ));
    }
    let root = parent.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(root)?;
    let stage = root.join(format!(".delta-restore-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage)?;
    let result = (|| -> InfraResult<()> {
        let name = dest_sqlite
            .file_name()
            .ok_or_else(|| InfraError::InvalidPath("restore filename required".into()))?;
        for entry in &manifest.files {
            let dest = if entry.path == "library.sqlite" {
                stage.join(name)
            } else {
                stage.join(&entry.path)
            };
            if let Some(dir) = dest.parent() {
                fs::create_dir_all(dir)?;
            }
            // Verify the copied bytes too: the source can change during copy.
            fs::copy(src.join(&entry.path), &dest)?;
            if sha256_hex(&fs::read(&dest)?) != entry.sha256 {
                return Err(InfraError::BackupVerification(
                    "backup changed during restore".into(),
                ));
            }
        }
        {
            let library = Library::open(&stage.join(name))?;
            let valid = library.with(|c| -> InfraResult<bool> {
                let mut s = c.prepare("PRAGMA foreign_key_check")?;
                let valid = s.query([])?.next()?.is_none();
                Ok(valid)
            })?;
            if !valid {
                return Err(InfraError::BackupVerification(
                    "broken database references".into(),
                ));
            }
            let ids = library.with(|c| -> InfraResult<Vec<String>> {
                let mut s = c.prepare("SELECT id FROM attachment")?;
                let rows = s.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })?;
            for id in ids {
                library.read_attachment(&id)?;
            }
            library.with(|c| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)"))?;
        }
        if parent.exists() {
            fs::remove_dir(parent)?;
        } // empty only; never recursive
        fs::rename(&stage, parent)?;
        Ok(())
    })();
    if result.is_err() {
        // `stage` was exclusively created above under the selected parent and
        // contains only verified copies. No user directory is recursively removed.
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

pub fn delete_isolated(path: &Path, confirm: &str, tasks_open: bool) -> InfraResult<()> {
    if tasks_open {
        return Err(InfraError::Rejected(
            "close running tasks before deleting the library".into(),
        ));
    }
    let canon = path
        .canonicalize()
        .map_err(|_| InfraError::NotFound(format!("library not found: {}", path.display())))?;
    if confirm == "cancel" || confirm != canon.to_string_lossy() {
        return Err(InfraError::Rejected(
            "delete cancelled; confirmation did not match the library path".into(),
        ));
    }
    fs::remove_file(&canon)?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", canon.display()));
        if side.exists() {
            let _ = fs::remove_file(side);
        }
    }
    let att = attachment_dir(&canon);
    if att.is_dir() {
        fs::remove_dir_all(att)?;
    }
    Ok(())
}

pub fn store_os_credential(reference: &str, secret: &str) -> Result<(), String> {
    if reference.trim().is_empty() || secret.is_empty() {
        return Err("credential reference and secret are required".into());
    }
    let entry = keyring::Entry::new("delta", reference).map_err(|e| safe_keyring(e.to_string()))?;
    entry
        .set_password(secret)
        .map_err(|e| safe_keyring(e.to_string()))
}

pub fn read_os_credential(reference: &str) -> Result<String, String> {
    let entry = keyring::Entry::new("delta", reference).map_err(|e| safe_keyring(e.to_string()))?;
    entry
        .get_password()
        .map_err(|e| safe_keyring(e.to_string()))
}

pub fn delete_os_credential(reference: &str) -> Result<(), String> {
    let entry = keyring::Entry::new("delta", reference).map_err(|e| safe_keyring(e.to_string()))?;
    entry
        .delete_credential()
        .map_err(|e| safe_keyring(e.to_string()))
}

fn safe_keyring(msg: String) -> String {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("http") || lower.contains("key=") {
        "credential store rejected the request".into()
    } else {
        msg.chars().take(180).collect()
    }
}

/// Environment passed to the indicator worker. Model secrets are removed.
pub fn sanitize_worker_env(vars: &[(&str, &str)]) -> Vec<(String, String)> {
    vars.iter()
        .filter(|(k, _)| {
            let u = k.to_ascii_uppercase();
            !u.contains("KEY")
                && !u.contains("SECRET")
                && !u.contains("TOKEN")
                && !u.contains("PASSWORD")
                && !u.contains("CREDENTIAL")
        })
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// The model client does not load developer instruction files.
pub fn implicit_config_files() -> &'static [&'static str] {
    &[]
}
