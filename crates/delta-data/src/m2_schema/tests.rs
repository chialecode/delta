use super::*;
use delta_core::parse_decimal_lossless;
use std::fs;
use std::path::{Path, PathBuf};

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

    // 4. Re-run open_or_init without fault: upgrades to current schema and is idempotent on repeat
    let store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
        .expect("upgrade to current schema");
    drop(store);
    assert_eq!(
        SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
        crate::storage::CURRENT_SCHEMA_VERSION
    );
    assert_eq!(
        SqliteStore::list_schema_4_tables_on_disk(&lib_dir)
            .unwrap()
            .len(),
        SCHEMA_4_TABLES.len()
    );

    // Repeat open_or_init to prove idempotency
    let _store_again = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
        .expect("idempotent current schema open");
    assert_eq!(
        SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
        crate::storage::CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn test_m2_a04_portfolio_deduplication_and_accounts_crud() {
    let tmp = TestTempDir::new();
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");

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
    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");

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

    // Cross-ID allocation using correction_group_id must also respect the 10-share cap (DATA-B19)
    let over_cg_err = store
        .link_plan_or_note_fill(
            None,
            Some("NOTE-1"),
            "CG-FILL-10",
            parse_decimal_lossless("5").unwrap(),
            "execution",
        )
        .unwrap_err();
    assert!(matches!(over_cg_err, StorageError::IntegrityFailed(_)));

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

    let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").expect("open store");
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

#[test]
fn test_data_b01_b09_b19_security_and_note_regressions() {
    let tmp = TestTempDir::new();
    let lib_dir = tmp.path().join("lib");
    let backup_dir = tmp.path().join("backup");
    let restore_dir = tmp.path().join("restore");
    let mut store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC").unwrap();

    // DATA-B09: case-variant tags must not collide on tag.id
    store
        .save_note_revision(
            "NOTE-TAG-1",
            "Upper tag",
            "freeform",
            true,
            "body 1",
            &StructuredNoteFields::default(),
            &["AAPL".into()],
        )
        .unwrap();
    store
        .save_note_revision(
            "NOTE-TAG-2",
            "Lower tag",
            "freeform",
            true,
            "body 2",
            &StructuredNoteFields::default(),
            &["aapl".into(), "AAPL".into()],
        )
        .unwrap();

    // DATA-B19a: `%` and `_` in short search_notes must be escaped, not wildcards
    let (pct_hits, _) = store.search_notes("%", false).unwrap();
    assert!(pct_hits.is_empty());
    let (under_hits, _) = store.search_notes("_", false).unwrap();
    assert!(under_hits.is_empty());

    // DATA-B19b: short external account ref masking must be idempotent across list_accounts
    store
        .upsert_account("ACC-SHORT", "Short", "bank", "test", Some("12"), "active")
        .unwrap();
    let listed = store.list_accounts().unwrap();
    let short = listed.iter().find(|a| a.id == "ACC-SHORT").unwrap();
    assert_eq!(short.external_ref_masked.as_deref(), Some("***12"));

    // DATA-B01: path traversal in backup manifest must be rejected by restore_consistent_backup_without_migration
    store.create_consistent_backup(&backup_dir).unwrap();
    let manifest_path = backup_dir.join("manifest.json");
    let mut manifest: crate::storage::LibraryManifest =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest
        .file_hashes
        .insert("../escape.txt".into(), "0".repeat(64));
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let err = SqliteStore::restore_consistent_backup_without_migration(&backup_dir, &restore_dir)
        .unwrap_err();
    assert!(matches!(err, StorageError::IntegrityFailed(_)));
    assert!(!restore_dir.exists());
}
