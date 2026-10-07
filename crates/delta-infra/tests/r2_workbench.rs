use delta_app::ai::{
    runtime::{ModelConnectionConfig, Protocol, ProxyPolicy},
    session::SessionStore,
};
use delta_infra::sqlite::{
    app::{restore_backup, MarketFile},
    store::Library,
    workbench::*,
};
use std::sync::atomic::Ordering;

fn config() -> ModelConnectionConfig {
    ModelConnectionConfig {
        id: "desktop-primary".into(),
        protocol: Protocol::OpenaiResponses,
        base_url: "http://127.0.0.1:9123/v1".into(),
        model_id: "controlled-fixture".into(),
        credential_ref: "r2-test-reference".into(),
        context_window_tokens: 8192,
        connect_timeout_secs: 1,
        idle_timeout_secs: 1,
        request_timeout_secs: 3,
        proxy: ProxyPolicy::System,
    }
}
fn market() -> MarketFile {
    serde_json::from_str(include_str!("../../../tests/fixtures/r2-ohlcv.json")).unwrap()
}

#[test]
fn r2_w06_backup_rejects_nonempty_destination_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::create(&dir.path().join("source/library.sqlite"), "USD").unwrap();
    let dest = dir.path().join("backup");
    std::fs::create_dir_all(dest.join("attachments")).unwrap();
    std::fs::write(dest.join("manifest.json"), b"existing manifest").unwrap();
    std::fs::write(
        dest.join("attachments/existing.txt"),
        b"existing attachment",
    )
    .unwrap();
    assert!(library.backup_to(&dest).is_err());
    assert_eq!(
        std::fs::read(dest.join("manifest.json")).unwrap(),
        b"existing manifest"
    );
    assert_eq!(
        std::fs::read(dest.join("attachments/existing.txt")).unwrap(),
        b"existing attachment"
    );
    assert!(!dest.join("library.sqlite").exists());
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(library.backup_to(&empty).is_ok());
}
fn context() -> ChartContext {
    ChartContext {
        instrument: "NASDAQ:AAPL".into(),
        start_at: "2026-01-01T00:00:00Z".into(),
        end_at: "2026-02-01T00:00:00Z".into(),
        adjustment: "raw".into(),
        dataset_version: None,
    }
}

#[test]
fn r2_w01_restart_two_libraries_and_invalid_open_preserve_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a/library.sqlite");
    let b = dir.path().join("b/library.sqlite");
    let recent = dir.path().join("config/recent.json");
    let library = Library::create(&a, "USD").unwrap();
    let id = library.create_account("主账户", "brokerage").unwrap();
    let settings = DesktopSettings {
        selected_accounts: vec![id],
        start_at: "2026-01-01T00:00:00Z".into(),
        end_at: "2026-02-01T00:00:00Z".into(),
        ..Default::default()
    };
    library
        .apply_desktop_settings(&settings, "USD", "Asia/Shanghai")
        .unwrap();
    write_recent(&recent, &a).unwrap();
    drop(library);
    let reopened = Library::open(&read_recent(&recent).unwrap().unwrap()).unwrap();
    assert_eq!(reopened.timezone().unwrap(), "Asia/Shanghai");
    assert_eq!(reopened.accounts().unwrap().len(), 1);
    let second = Library::create(&b, "USD").unwrap();
    assert!(second.accounts().unwrap().is_empty());
    write_recent(&recent, &b).unwrap();
    assert_eq!(
        read_recent(&recent).unwrap().unwrap(),
        b.canonicalize().unwrap()
    );
    let broken = dir.path().join("broken.sqlite");
    std::fs::write(&broken, b"not a sqlite database").unwrap();
    assert!(Library::open(&broken).is_err());
    assert_eq!(std::fs::read(&broken).unwrap(), b"not a sqlite database");
    let alien = dir.path().join("alien.sqlite");
    rusqlite::Connection::open(&alien)
        .unwrap()
        .execute_batch("CREATE TABLE other(x TEXT)")
        .unwrap();
    let before = std::fs::read(&alien).unwrap();
    assert!(Library::open(&alien).is_err());
    assert_eq!(std::fs::read(&alien).unwrap(), before);
}

#[test]
fn r2_w02_mapped_csv_preview_golden_receipt_and_stale_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::create(&dir.path().join("library.sqlite"), "USD").unwrap();
    let account = library.create_account("golden", "brokerage").unwrap();
    let bytes = include_bytes!("../../../tests/fixtures/r2-ledger.csv");
    let columns = CSV_FIELDS.map(str::to_string);
    let preview = library
        .preview_mapped_csv(&account, bytes, &columns, b',')
        .unwrap();
    assert_eq!(preview.rows.len(), 3);
    assert_eq!(library.ledger_revision().unwrap(), 0);
    assert!(library
        .commit_preview(
            &preview.batch_id,
            "wrong-mapping",
            &preview.file_hash,
            &[1, 2, 3]
        )
        .is_err());
    let receipt = library
        .commit_preview(
            &preview.batch_id,
            &preview.mapping_version,
            &preview.file_hash,
            &[1, 2, 3],
        )
        .unwrap();
    assert_eq!(receipt.accepted, 3);
    assert!(library
        .preview_mapped_csv(&account, bytes, &columns, b',')
        .is_err());
    library.import_market_file(&market()).unwrap();
    let scope = library
        .desktop_scope(&[account], "2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z")
        .unwrap();
    let wealth = library.wealth(&scope, scope.end_at).unwrap();
    assert_eq!(wealth.known_net_worth.to_string(), "2068");
    let pnl = library.explain(&scope).unwrap();
    assert_eq!(pnl.total_pnl.unwrap().normalize().to_string(), "68");
    let lines = library.account_lines().unwrap();
    assert_eq!(
        lines.iter().find(|l| l.asset == "USD").unwrap().quantity,
        "1438"
    );
    assert_eq!(library.preview_details(&preview.batch_id).unwrap().len(), 3);
}

#[test]
fn r2_w03_atomic_immutable_market_and_venue_window() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::create(&dir.path().join("library.sqlite"), "USD").unwrap();
    let mut file = market();
    file.bars.last_mut().unwrap().high = "1".into();
    assert!(library.import_market_file(&file).is_err());
    assert_eq!(library.dataset_count(&file.dataset_version).unwrap(), 0);
    let mut file = market();
    library.import_market_file(&file).unwrap();
    library.import_market_file(&file).unwrap();
    file.bars[0].close = "104".into();
    assert!(library.import_market_file(&file).is_err());
    let window = library.chart_window(&context()).unwrap();
    assert_eq!(window.bars.len(), 31);
    assert_eq!(window.bars[30].close, "105");
    assert!(window.source.contains("synthetic"));
    let mut ctx = context();
    ctx.instrument = "KRAKEN:BTC/USDT".into();
    assert_eq!(library.chart_window(&ctx).unwrap().bars.len(), 1);
    ctx.instrument = "UNKNOWN:BTC/USDT".into();
    assert!(library.chart_window(&ctx).unwrap().bars.is_empty());
    ctx = context();
    ctx.end_at = "2026-01-10T20:00:00Z".into();
    assert_eq!(library.chart_window(&ctx).unwrap().bars.len(), 9);
}

#[test]
fn r2_w04_note_failure_revision_and_chart_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.sqlite");
    let library = Library::create(&path, "USD").unwrap();
    let account = library.create_account("note account", "brokerage").unwrap();
    let draft = JournalDraft {
        id: None,
        title: "复盘".into(),
        body: "原始理由".into(),
        account_id: account,
        revision: 0,
        chart: Some(context()),
    };
    let saved = library.save_journal_draft(&draft).unwrap();
    let id = saved.id.clone().unwrap();
    let old = library.journal_evidence_id(&id).unwrap();
    let mut edited = saved.clone();
    edited.body = "新理由".into();
    library.fail_next_journal.store(true, Ordering::SeqCst);
    assert!(library.save_journal_draft(&edited).is_err());
    assert_eq!(library.journal_draft(&id, None).unwrap().body, "原始理由");
    assert_eq!(library.save_journal_draft(&edited).unwrap().revision, 2);
    assert!(library.save_journal_draft(&saved).is_err());
    assert_eq!(library.open_evidence(&old).unwrap().body, "原始理由");
    drop(library);
    let reopened = Library::open(&path).unwrap();
    assert_eq!(
        reopened.journal_draft(&id, Some(1)).unwrap().chart,
        Some(context())
    );
    assert_eq!(reopened.journal_draft(&id, None).unwrap().body, "新理由");
}

#[test]
fn r2_w05_saved_connections_no_secrets_and_fail_closed_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.sqlite");
    let library = Library::create(&path, "USD").unwrap();
    let id = library.create_account("first", "brokerage").unwrap();
    let mut settings = DesktopSettings {
        connection: Some(config()),
        selected_accounts: vec![id.clone()],
        ..Default::default()
    };
    library.save_desktop_settings(&settings).unwrap();
    library.create_account("later", "crypto").unwrap();
    assert!(library
        .desktop_settings()
        .unwrap()
        .allowed_accounts
        .is_empty());
    for url in [
        "http://remote.example/v1",
        "https://secret@host.test/v1",
        "https://host.test/v1?token=secret",
        "https://host.test/v1#secret",
    ] {
        settings.connection.as_mut().unwrap().base_url = url.into();
        assert!(library.save_desktop_settings(&settings).is_err());
    }
    assert!(!serde_json::to_string(&library.desktop_settings().unwrap())
        .unwrap()
        .contains("secret"));
    drop(library);
    let reopened = Library::open(&path).unwrap();
    assert_eq!(
        reopened
            .desktop_settings()
            .unwrap()
            .connection
            .unwrap()
            .credential_ref,
        "r2-test-reference"
    );
}

#[test]
fn r2_w06_restore_validates_attachments_sessions_and_does_not_publish_failure() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::create(&dir.path().join("source/library.sqlite"), "USD").unwrap();
    let account = library.create_account("backup", "brokerage").unwrap();
    let session = library.create_session(&library.id).unwrap();
    let attachment = library
        .save_attachment(b"synthetic attachment", "text/plain")
        .unwrap()
        .0;
    let saved = library
        .save_journal_draft(&JournalDraft {
            id: None,
            title: "evidence".into(),
            body: "fixed text".into(),
            account_id: account,
            revision: 0,
            chart: Some(context()),
        })
        .unwrap();
    let evidence = library
        .journal_evidence_id(saved.id.as_ref().unwrap())
        .unwrap();
    let backup = dir.path().join("backup");
    library.backup_to(&backup).unwrap();
    let restored = dir.path().join("restored/library.sqlite");
    restore_backup(&backup, &restored).unwrap();
    let new = Library::open(&restored).unwrap();
    assert!(new.session_exists(&session).unwrap());
    assert_eq!(
        new.read_attachment(&attachment).unwrap(),
        b"synthetic attachment"
    );
    assert_eq!(new.open_evidence(&evidence).unwrap().body, "fixed text");
    assert!(restore_backup(&backup, &library.path).is_err());
    std::fs::write(backup.join("library.sqlite"), b"bad").unwrap();
    let failed = dir.path().join("failed/library.sqlite");
    assert!(restore_backup(&backup, &failed).is_err());
    assert!(!failed.exists());
    assert!(Library::open(&library.path).is_ok());
}
