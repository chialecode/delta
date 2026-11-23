use std::fs;
use std::path::Path;

use chrono::Utc;
use delta_core::{parse_decimal_lossless, to_canonical_text, EconomicEventType, LedgerEventInput};
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;

use crate::importers::fx_lookup::{
    load_all_ledger_events, load_book_fx_lookup, load_event_book_fx, persist_event_book_fx,
    prepare_event_book_fx,
};
use crate::importers::{
    preview_statement_import, source_payload_hash, source_record_kind, CommitImportReceipt,
    ImportError, ImportPreview,
};
use crate::storage::{sha256_file, SqliteStore, StorageError};

const INSERT_ECONOMIC_EVENT_SQL: &str = "INSERT INTO economic_event (
    id, account_id, type, occurred_at, sequence_no, recorded_at,
    asset_id, instrument_id, quantity, price, quote_currency,
    fee_amount, fee_currency, cost_known, counterparty_account_id,
    transfer_group_id, source_ref, correction_group_id, revision, active_in_correction_group
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)";

fn insert_economic_event(
    tx: &rusqlite::Transaction<'_>,
    ev: &LedgerEventInput,
    now: &str,
) -> Result<(), StorageError> {
    let mut stmt = tx.prepare_cached(INSERT_ECONOMIC_EVENT_SQL)?;
    stmt.execute(params![
        ev.event_id,
        ev.account_id,
        ev.event_type.as_str(),
        ev.occurred_at.to_rfc3339(),
        ev.sequence_no,
        now,
        ev.asset_id,
        ev.instrument_id,
        to_canonical_text(ev.quantity),
        ev.price.map(to_canonical_text),
        ev.quote_currency,
        to_canonical_text(ev.fee_amount),
        ev.fee_currency,
        i32::from(ev.cost_known),
        ev.counterparty_account_id,
        ev.transfer_group_id,
        ev.source_ref,
        ev.correction_group_id.as_deref().unwrap_or(&ev.event_id),
        ev.revision,
        i32::from(ev.active_in_correction_group),
    ])?;
    Ok(())
}

/// Commit a previewed import batch atomically in a single SQLite transaction:
/// 1. Re-verifies `file_hash` on disk first (`FileTamperedAfterPreview` if modified after preview).
/// 2. Rebuilds authoritative preview; if `already_imported_batch_id` is present, returns `idempotent_skipped = true`.
/// 3. Validates the combined ledger event replay (rejecting oversells in spot mode).
/// 4. Persists `import_batch`, `import_row`, `economic_event`, `journal_transaction`, `posting`,
///    `cost_lot`, `holding_projection`, and auto-subscriptions.
pub fn commit_import_preview(
    store: &mut SqliteStore,
    preview: &ImportPreview,
) -> Result<CommitImportReceipt, ImportError> {
    let current_hash = sha256_file(Path::new(&preview.file_path))?;
    if current_hash != preview.file_hash {
        return Err(ImportError::FileTamperedAfterPreview {
            path: preview.file_path.clone(),
            expected: preview.file_hash.clone(),
            actual: current_hash,
        });
    }

    // Rebuild the preview from authoritative inputs rather than trusting editable DTOs.
    let authoritative = preview_statement_import(
        store,
        &preview.account_id,
        preview.preset,
        Path::new(&preview.file_path),
    )?;
    if let Some(existing_id) = authoritative.already_imported_batch_id.as_ref() {
        let rev: i64 = store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(StorageError::from)?;
        return Ok(CommitImportReceipt {
            batch_id: existing_id.clone(),
            idempotent_skipped: true,
            committed_events: 0,
            suspected_duplicates_held: authoritative.suspected_duplicate_count,
            auto_subscribed_instruments: authoritative.auto_subscriptions,
            auto_subscribed_fx: authoritative.auto_fx_pairs,
            ledger_revision: rev,
        });
    }
    if authoritative != *preview {
        return Err(ImportError::StalePreview);
    }
    if preview.error_count > 0 {
        return Err(ImportError::ValidationErrorsPresent(preview.error_count));
    }

    let mut all_events = load_all_ledger_events(store)?;
    let new_events: Vec<LedgerEventInput> = preview
        .rows
        .iter()
        .filter(|r| r.validation_state == "valid")
        .filter_map(|r| r.normalized.clone())
        .collect();
    all_events.extend(new_events.clone());

    let book_ccy: String = store
        .conn()
        .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let next_rev: i64 = store
        .conn()
        .query_row(
            if new_events.is_empty() {
                "SELECT ledger_revision FROM library LIMIT 1"
            } else {
                "SELECT ledger_revision + 1 FROM library LIMIT 1"
            },
            [],
            |r| r.get(0),
        )
        .map_err(StorageError::from)?;

    let fx_map = load_book_fx_lookup(store, &book_ccy)?;
    let bindings = load_event_book_fx(store)?;
    let new_bindings = prepare_event_book_fx(store, &book_ccy, &new_events, &fx_map)?;
    let projection = delta_core::replay_ledger_events_with_bindings(
        &book_ccy,
        &all_events,
        &fx_map,
        &bindings,
        next_rev,
    )?;

    // Copy original import file into `imports/<file_hash>.dat` for consistent backup (S-11 / AC-09)
    let import_copy = store
        .root_dir()
        .join("imports")
        .join(format!("{}.dat", preview.file_hash));
    if !import_copy.exists() {
        fs::copy(&preview.file_path, &import_copy)?;
    }

    let reg = store.load_instrument_registry()?;
    let now = Utc::now().to_rfc3339();
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;

    tx.execute(
        "INSERT OR IGNORE INTO account (id, name, type, provider, status, created_at)
         VALUES (?1, ?1, 'spot', ?2, 'active', ?3)",
        params![preview.account_id, preview.preset.as_str(), now],
    )
    .map_err(StorageError::from)?;

    let counts_json = serde_json::json!({
        "valid": preview.valid_count,
        "errors": preview.error_count,
        "suspected_duplicates": preview.suspected_duplicate_count,
    })
    .to_string();

    tx.execute(
        "INSERT INTO import_batch (id, account_id, source, file_hash, mapping_version, status, counts_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'committed', ?6, ?7)",
        params![
            preview.batch_id,
            preview.account_id,
            preview.preset.as_str(),
            preview.file_hash,
            preview.mapping_version,
            counts_json,
            now,
        ],
    )
    .map_err(StorageError::from)?;

    {
        let mut row_stmt = tx
            .prepare_cached(
                "INSERT INTO import_row (id, batch_id, row_no, raw_payload, normalized_payload, fingerprint, validation_state, error_reason, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(StorageError::from)?;
        for row in &preview.rows {
            let norm_json = row
                .normalized
                .as_ref()
                .and_then(|n| serde_json::to_string(n).ok())
                .unwrap_or_default();
            let ev_id = if row.validation_state == "valid" {
                row.normalized.as_ref().map(|n| n.event_id.as_str())
            } else {
                None
            };
            row_stmt
                .execute(params![
                    format!("ROW:{}:{}", preview.batch_id, row.row_no),
                    preview.batch_id,
                    row.row_no as i64,
                    row.raw_payload,
                    norm_json,
                    row.fingerprint,
                    row.validation_state,
                    row.error_reason,
                    ev_id,
                ])
                .map_err(StorageError::from)?;
        }

        let mut src_stmt = tx
            .prepare_cached(
                "INSERT INTO source_record(provider, account_id, record_type, external_id, revision, payload_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(StorageError::from)?;
        for ev in &new_events {
            insert_economic_event(&tx, ev, &now)?;
            if let Some(source_id) = ev.source_ref.as_ref() {
                src_stmt
                    .execute(params![
                        preview.preset.as_str(),
                        ev.account_id,
                        source_record_kind(ev.event_type),
                        source_id,
                        ev.revision,
                        source_payload_hash(ev),
                    ])
                    .map_err(StorageError::from)?;
            }
        }
    }

    persist_event_book_fx(&tx, &new_bindings)?;
    if !new_events.is_empty() {
        persist_projection_in_tx(&tx, &projection, next_rev).map_err(StorageError::from)?;
    }

    let auto_reason = format!("import_batch:{}", preview.batch_id);
    {
        let mut sub_stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO subscription (instrument_id, venue, symbol, auto_reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(StorageError::from)?;
        for inst_id in &preview.auto_subscriptions {
            if let Some(inst) = reg.instruments.get(inst_id) {
                sub_stmt
                    .execute(params![inst.id, inst.venue, inst.symbol, auto_reason, now])
                    .map_err(StorageError::from)?;
            }
        }

        let mut fx_sub_stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO fx_subscription (base_currency, quote_currency, auto_reason, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(StorageError::from)?;
        for (base, quote) in &preview.auto_fx_pairs {
            fx_sub_stmt
                .execute(params![base, quote, auto_reason, now])
                .map_err(StorageError::from)?;
        }
    }

    tx.commit().map_err(StorageError::from)?;
    store.sync_manifest()?;

    Ok(CommitImportReceipt {
        batch_id: preview.batch_id.clone(),
        idempotent_skipped: false,
        committed_events: new_events.len(),
        suspected_duplicates_held: preview.suspected_duplicate_count,
        auto_subscribed_instruments: preview.auto_subscriptions.clone(),
        auto_subscribed_fx: preview.auto_fx_pairs.clone(),
        ledger_revision: next_rev,
    })
}

/// Amend a committed economic event via atomic reversal + replacement (`correction_group_id`, AC-03).
/// Also marks any `analysis_report` generated at an older `ledger_revision` as `stale`, and binds
/// any newly required foreign currency FX (e.g. non-zero foreign fee added during amendment).
pub fn amend_committed_event(
    store: &mut SqliteStore,
    original_event_id: &str,
    new_quantity: Decimal,
    new_price: Option<Decimal>,
    new_fee: Decimal,
) -> Result<i64, ImportError> {
    let all_events = load_all_ledger_events(store)?;
    let orig = all_events
        .iter()
        .find(|e| e.event_id == original_event_id && e.active_in_correction_group)
        .ok_or_else(|| {
            delta_core::LedgerError::NotFound(format!("active event {original_event_id}"))
        })?
        .clone();

    let cg_id = orig
        .correction_group_id
        .clone()
        .unwrap_or_else(|| format!("CG:{}", orig.event_id));
    let new_rev = orig.revision + 1;
    let replacement_id = format!("{}:REV:{new_rev}", orig.event_id);

    let mut updated_events = Vec::with_capacity(all_events.len() + 1);
    for mut ev in all_events {
        if ev.event_id == original_event_id {
            ev.active_in_correction_group = false;
        }
        updated_events.push(ev);
    }

    let replacement = LedgerEventInput {
        event_id: replacement_id.clone(),
        quantity: new_quantity,
        price: new_price,
        fee_amount: new_fee,
        cost_known: orig.cost_known
            || (orig.event_type == EconomicEventType::OpeningBalance && new_price.is_some()),
        correction_group_id: Some(cg_id.clone()),
        revision: new_rev,
        active_in_correction_group: true,
        ..orig.clone()
    };
    updated_events.push(replacement.clone());

    let book_ccy: String = store
        .conn()
        .query_row("SELECT book_currency FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let next_ledger_rev: i64 = store
        .conn()
        .query_row("SELECT ledger_revision + 1 FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .map_err(StorageError::from)?;
    let fx_map = load_book_fx_lookup(store, &book_ccy)?;
    let mut bindings = load_event_book_fx(store)?;
    for currency in [&orig.quote_currency, &orig.fee_currency, &orig.asset_id] {
        let up = currency.to_ascii_uppercase();
        if let Some(rate) = bindings.get(&(orig.event_id.clone(), up.clone())).copied() {
            bindings.insert((replacement_id.clone(), up), rate);
        }
    }
    // Bind any newly required foreign FX for `replacement` (e.g., amending 0 -> >0 foreign fee).
    let candidate_bindings = prepare_event_book_fx(
        store,
        &book_ccy,
        std::slice::from_ref(&replacement),
        &fx_map,
    )?;
    let mut extra_bindings = Vec::new();
    for b in candidate_bindings {
        if let std::collections::hash_map::Entry::Vacant(e) =
            bindings.entry((b.0.clone(), b.1.clone()))
        {
            e.insert(b.2);
            extra_bindings.push(b);
        }
    }

    let proj = delta_core::replay_ledger_events_with_bindings(
        &book_ccy,
        &updated_events,
        &fx_map,
        &bindings,
        next_ledger_rev,
    )?;

    let now = Utc::now().to_rfc3339();
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;

    tx.execute(
        "UPDATE economic_event SET active_in_correction_group = 0 WHERE id = ?1",
        params![original_event_id],
    )
    .map_err(StorageError::from)?;

    let rev_event_id = format!("{}:REVERSAL:{new_rev}", orig.event_id);
    let reversal_ev = LedgerEventInput {
        event_id: rev_event_id.clone(),
        event_type: EconomicEventType::Reversal,
        correction_group_id: Some(cg_id.clone()),
        revision: new_rev,
        active_in_correction_group: false,
        ..orig.clone()
    };
    insert_economic_event(&tx, &reversal_ev, &now)?;
    insert_economic_event(&tx, &replacement, &now)?;

    let original_tx: Option<String> = tx
        .query_row(
            "SELECT id FROM journal_transaction WHERE event_id = ?1 ORDER BY ledger_revision DESC LIMIT 1",
            params![original_event_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(StorageError::from)?;
    tx.execute(
        "INSERT INTO event_book_fx(event_id, currency, rate, source_date, dataset_version)
         SELECT ?1, currency, rate, source_date, dataset_version
         FROM event_book_fx
         WHERE event_id = ?2",
        params![replacement_id, original_event_id],
    )
    .map_err(StorageError::from)?;
    persist_event_book_fx(&tx, &extra_bindings)?;

    if let Some(original_tx) = original_tx {
        let reversal_tx = format!("TX:{rev_event_id}:LR:{next_ledger_rev}");
        tx.execute(
            "INSERT INTO journal_transaction (id, event_id, book_currency, posting_rule_version, reversal_of, ledger_revision)
             VALUES (?1, ?2, ?3, 'v2', ?4, ?5)",
            params![reversal_tx, rev_event_id, book_ccy, original_tx, next_ledger_rev],
        )
        .map_err(StorageError::from)?;
        let postings: Vec<(String, String, String, String, String, String)> = {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT id, ledger_account_id, category, native_asset_id, native_quantity, book_amount
                     FROM posting
                     WHERE transaction_id = ?1",
                )
                .map_err(StorageError::from)?;
            let rows = stmt
                .query_map(params![original_tx], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })
                .map_err(StorageError::from)?;
            let mut list = Vec::new();
            for row in rows {
                list.push(row.map_err(StorageError::from)?);
            }
            list
        };
        let mut ins_posting = tx
            .prepare_cached("INSERT INTO posting VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
            .map_err(StorageError::from)?;
        for (id, account, category, asset, quantity, amount) in postings {
            let quantity = parse_decimal_lossless(&quantity).map_err(StorageError::from)?;
            let amount = parse_decimal_lossless(&amount).map_err(StorageError::from)?;
            ins_posting
                .execute(params![
                    format!("{id}:REV:{new_rev}"),
                    reversal_tx,
                    account,
                    category,
                    asset,
                    to_canonical_text(-quantity),
                    to_canonical_text(-amount),
                ])
                .map_err(StorageError::from)?;
        }
    }

    persist_projection_in_tx(&tx, &proj, next_ledger_rev).map_err(StorageError::from)?;
    if let Some(source_id) = replacement.source_ref.as_ref() {
        tx.execute(
            "UPDATE source_record
             SET revision = ?1, payload_hash = ?2
             WHERE account_id = ?3
               AND record_type = ?4
               AND external_id = ?5
               AND provider IN (
                   SELECT b.source
                   FROM import_batch b
                   JOIN import_row r ON r.batch_id = b.id
                   JOIN economic_event e ON e.id = r.event_id
                   WHERE e.correction_group_id = ?6
               )",
            params![
                new_rev,
                source_payload_hash(&replacement),
                replacement.account_id,
                source_record_kind(replacement.event_type),
                source_id,
                cg_id,
            ],
        )
        .map_err(StorageError::from)?;
    }

    tx.execute(
        "UPDATE analysis_report SET status = 'stale' WHERE ledger_revision < ?1",
        params![next_ledger_rev],
    )
    .map_err(StorageError::from)?;

    tx.commit().map_err(StorageError::from)?;
    store.sync_manifest()?;
    Ok(next_ledger_rev)
}

fn persist_projection_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proj: &delta_core::LedgerProjectionResult,
    ledger_revision: i64,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "UPDATE library SET ledger_revision = ?1",
        params![ledger_revision],
    )?;
    tx.execute(
        "UPDATE analysis_report SET status = 'stale' WHERE ledger_revision < ?1",
        params![ledger_revision],
    )?;
    tx.execute("DELETE FROM cost_lot", [])?;
    tx.execute("DELETE FROM holding_projection", [])?;

    {
        let mut jtx_stmt = tx.prepare_cached(
            "INSERT INTO journal_transaction (id, event_id, book_currency, posting_rule_version, reversal_of, ledger_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut post_stmt = tx.prepare_cached(
            "INSERT INTO posting (id, transaction_id, ledger_account_id, category, native_asset_id, native_quantity, book_amount)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for jtx in &proj.transactions {
            let transaction_id = format!("{}:LR:{ledger_revision}", jtx.id);
            jtx_stmt.execute(params![
                transaction_id,
                jtx.event_id,
                jtx.book_currency,
                jtx.posting_rule_version,
                jtx.reversal_of,
                ledger_revision,
            ])?;
            for p in &jtx.postings {
                post_stmt.execute(params![
                    format!("{}:LR:{ledger_revision}", p.id),
                    transaction_id,
                    p.ledger_account_id,
                    p.category.as_str(),
                    p.native_asset_id,
                    to_canonical_text(p.native_quantity),
                    to_canonical_text(p.book_amount),
                ])?;
            }
        }
    }

    {
        let mut lot_stmt = tx.prepare_cached(
            "INSERT INTO cost_lot (
                id, account_id, asset_id, instrument_id, source_fill_id, acquired_at,
                original_quantity, remaining_quantity, original_cost_book, remaining_cost_book,
                cost_known, ledger_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        for lot in &proj.open_lots {
            lot_stmt.execute(params![
                lot.id,
                lot.account_id,
                lot.asset_id,
                lot.instrument_id,
                lot.source_fill_id,
                lot.acquired_at.to_rfc3339(),
                to_canonical_text(lot.original_quantity),
                to_canonical_text(lot.remaining_quantity),
                to_canonical_text(lot.original_cost_book),
                to_canonical_text(lot.remaining_cost_book),
                i32::from(lot.cost_known),
                ledger_revision,
            ])?;
        }
    }

    {
        let mut hold_stmt = tx.prepare_cached(
            "INSERT INTO holding_projection (account_id, asset_id, quantity, carrying_cost, cost_known, ledger_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for h in proj.holdings.values() {
            hold_stmt.execute(params![
                h.account_id,
                h.asset_id,
                to_canonical_text(h.quantity),
                to_canonical_text(h.carrying_cost),
                i32::from(h.cost_known),
                ledger_revision,
            ])?;
        }
    }
    Ok(())
}

pub fn resolve_held_import_row(
    store: &mut SqliteStore,
    row_id: &str,
    include: bool,
) -> Result<i64, ImportError> {
    let (payload, state, account): (String, String, String) = store
        .conn()
        .query_row(
            "SELECT r.normalized_payload, r.validation_state, b.account_id
             FROM import_row r
             JOIN import_batch b ON b.id = r.batch_id
             WHERE r.id = ?1",
            params![row_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(StorageError::from)?;
    let current_revision: i64 = store
        .conn()
        .query_row("SELECT ledger_revision FROM library", [], |r| r.get(0))
        .map_err(StorageError::from)?;
    if state == "valid" || state == "excluded_duplicate" {
        return Ok(current_revision);
    }
    if state != "suspected_duplicate" {
        return Err(ImportError::StalePreview);
    }
    if !include {
        store
            .conn()
            .execute(
                "UPDATE import_row
                 SET validation_state = 'excluded_duplicate', error_reason = 'user excluded duplicate'
                 WHERE id = ?1",
                params![row_id],
            )
            .map_err(StorageError::from)?;
        store.sync_manifest()?;
        return Ok(current_revision);
    }
    let ev: LedgerEventInput = serde_json::from_str(&payload).map_err(StorageError::from)?;
    if ev.account_id != account {
        return Err(ImportError::StalePreview);
    }
    let mut events = load_all_ledger_events(store)?;
    events.push(ev.clone());
    let book: String = store
        .conn()
        .query_row("SELECT book_currency FROM library", [], |r| r.get(0))
        .map_err(StorageError::from)?;
    let fx = load_book_fx_lookup(store, &book)?;
    let next_revision = current_revision + 1;
    let bindings = load_event_book_fx(store)?;
    let new_bindings = prepare_event_book_fx(store, &book, std::slice::from_ref(&ev), &fx)?;
    let projection = delta_core::replay_ledger_events_with_bindings(
        &book,
        &events,
        &fx,
        &bindings,
        next_revision,
    )?;
    let tx = store.conn_mut().transaction().map_err(StorageError::from)?;
    insert_economic_event(&tx, &ev, &Utc::now().to_rfc3339())?;
    persist_event_book_fx(&tx, &new_bindings)?;
    tx.execute(
        "UPDATE import_row
         SET validation_state = 'valid', event_id = ?1, error_reason = 'user confirmed distinct trade'
         WHERE id = ?2",
        params![ev.event_id, row_id],
    )
    .map_err(StorageError::from)?;
    persist_projection_in_tx(&tx, &projection, next_revision).map_err(StorageError::from)?;
    tx.commit().map_err(StorageError::from)?;
    store.sync_manifest()?;
    Ok(next_revision)
}
