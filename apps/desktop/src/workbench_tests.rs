use super::*;
use crate::tasks::{Operation, TaskGate};
use delta_app::contracts::CancelToken;
use delta_infra::sqlite::workbench::*;
use gpui_kit::{gpui::WindowHandle, test::TestWindowExt, TestAppContext};
use std::sync::atomic::Ordering;

struct Ui<'a> {
    cx: &'a mut TestAppContext,
    handle: WindowHandle<Workspace>,
}
impl<'a> Ui<'a> {
    fn new(cx: &'a mut TestAppContext, lib: Arc<Library>, recent: std::path::PathBuf) -> Self {
        cx.update(gpui_kit::init);
        let handle = cx.add_window(move |w, cx| {
            let mut view = Workspace::with_library(Some(lib), vec![], w, cx);
            view.business.demo = false;
            view.business.recent = recent;
            view
        });
        let mut ui = Self { cx, handle };
        ui.step(|_, _| {});
        ui
    }
    fn step(&mut self, f: impl FnOnce(&mut Window, &mut App)) {
        self.cx
            .update_window(self.handle.into(), |_, w, cx| f(w, cx))
            .unwrap();
        self.cx.run_until_parked();
        self.cx
            .update_window(self.handle.into(), |_, w, cx| w.render_frame(cx))
            .unwrap();
    }
    fn click(&mut self, id: impl Into<SharedString>) {
        let id = id.into();
        self.step(move |w, cx| w.click(id, cx));
    }
    fn type_in(&mut self, id: &'static str, text: &str) {
        let text = text.to_string();
        self.step(move |w, cx| {
            w.click(id, cx);
            w.press("secondary-a", cx);
            w.press("backspace", cx);
            w.input(&text, cx);
        });
    }
    fn state<R>(
        &mut self,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
    ) -> R {
        self.handle.update(self.cx, f).unwrap()
    }
}

#[gpui_kit::test]
fn r2_w02_desktop_csv_preview_commit_and_service_totals(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("config/recent.json"));
    ui.click("nav-accounts");
    ui.type_in("account-name", "合成账户");
    ui.click("create-account");
    let account = lib.accounts().unwrap().first().unwrap().id.clone();
    assert_eq!(
        lib.desktop_settings().unwrap().selected_accounts,
        vec![account]
    );
    ui.type_in("scope-start", "2026-01-01T00:00:00Z");
    ui.type_in("scope-end", "2026-02-01T00:00:00Z");
    ui.click("apply-scope");
    let csv =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/r2-ledger.csv");
    ui.click("nav-import");
    ui.type_in("csv-path", &csv.to_string_lossy());
    ui.click("preview-csv");
    assert_eq!(lib.ledger_revision().unwrap(), 0);
    ui.state(|s, _, _| {
        assert_eq!(s.business.preview.as_ref().unwrap().rows.len(), 3);
        assert!(s.status.contains("预览就绪"), "{}", s.status);
    });
    ui.click("commit-csv");
    ui.state(|s, _, _| assert!(s.status.contains("新增 3"), "{}", s.status));
    let market =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/r2-ohlcv.json");
    ui.click("nav-chart");
    ui.type_in("market-path", &market.to_string_lossy());
    ui.click("import-market");
    ui.type_in("chart-id", "NASDAQ:AAPL");
    ui.click("load-chart");
    ui.state(|s, _, cx| {
        assert_eq!(s.chart.read(cx).candles.len(), 31);
        assert_eq!(s.chart.read(cx).ma[19], Some(105.));
        assert_eq!(
            s.business
                .state
                .as_ref()
                .unwrap()
                .wealth
                .as_ref()
                .unwrap()
                .known_net_worth
                .to_string(),
            "2068"
        );
    });
    ui.click("save-chart-context");
    assert_eq!(
        lib.desktop_settings().unwrap().chart.unwrap().instrument,
        "NASDAQ:AAPL"
    );
}

#[gpui_kit::test]
fn r2_w04_desktop_write_failure_keeps_buffer_and_old_evidence(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = lib.create_account("notes", "brokerage").unwrap();
    lib.save_desktop_settings(&DesktopSettings {
        selected_accounts: vec![account],
        ..Default::default()
    })
    .unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("config/recent.json"));
    ui.click("nav-journal");
    ui.type_in("note-title", "独立复盘");
    ui.type_in("note-body", "中文原始理由");
    ui.click("save-note");
    let id = ui.state(|s, _, _| s.business.note.as_ref().unwrap().id.clone().unwrap());
    let reference = lib.journal_evidence_id(&id).unwrap();
    ui.type_in("note-body", "修改后的中文理由");
    lib.fail_next_journal.store(true, Ordering::SeqCst);
    ui.click("save-note");
    ui.state(|s, _, cx| {
        assert!(s.business.dirty);
        assert_eq!(s.business.body.read(cx).value(), "修改后的中文理由");
        assert!(s.status.contains("失败"));
    });
    assert_eq!(lib.open_evidence(&reference).unwrap().body, "中文原始理由");
    ui.click("save-note");
    ui.state(|s, w, cx| s.start_operation(Operation::Evidence(reference.clone()), w, cx));
    ui.step(|_, _| {});
    ui.state(|s, _, _| assert_eq!(s.business.evidence.as_ref().unwrap().body, "中文原始理由"));
    ui.click("open-evidence-note");
    ui.state(|s, _, cx| {
        assert_eq!(s.business.note.as_ref().unwrap().revision, 1);
        assert_eq!(s.business.body.read(cx).value(), "中文原始理由");
    });
}

#[gpui_kit::test]
fn r2_w01_desktop_switch_and_restore_use_persistent_library(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("first/library.sqlite"), "USD").unwrap());
    lib.create_account("first", "brokerage").unwrap();
    let second = dir.path().join("second/library.sqlite");
    let recent = dir.path().join("config/recent.json");
    let mut ui = Ui::new(cx, lib.clone(), recent.clone());
    ui.click("nav-settings");
    ui.type_in("library-path", &second.to_string_lossy());
    ui.click("create-library");
    ui.state(|s, _, _| {
        assert_eq!(s.library.as_ref().unwrap().accounts().unwrap().len(), 0);
        assert!(!s.business.demo);
    });
    assert_eq!(
        read_recent(&recent).unwrap().unwrap(),
        second.canonicalize().unwrap()
    );
    ui.click("nav-recovery");
    let backup = dir.path().join("backup");
    ui.type_in("backup-path", &backup.to_string_lossy());
    ui.click("backup");
    assert!(backup.join("manifest.json").is_file());
    let restored = dir.path().join("restored/library.sqlite");
    ui.type_in("restore-source", &backup.to_string_lossy());
    ui.type_in("restore-path", &restored.to_string_lossy());
    ui.click("restore");
    assert!(restored.exists());
    ui.state(|s, _, _| assert_eq!(s.library.as_ref().unwrap().path, restored));
    assert_eq!(lib.accounts().unwrap().len(), 1);
}

#[test]
fn r2_w01_cancel_generation_and_commit_receipt() {
    let mut gate = TaskGate::default();
    let (epoch, serial, token) = gate.begin();
    gate.switch();
    assert!(token.is_cancelled());
    assert!(!gate.accepts(epoch, serial));
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = lib.create_account("test", "brokerage").unwrap();
    let preview = lib
        .preview_mapped_csv(
            &account,
            include_bytes!("../../../tests/fixtures/r2-ledger.csv"),
            &CSV_FIELDS.map(str::to_string),
            b',',
        )
        .unwrap();
    let path = dir.path().join("input.csv");
    std::fs::write(
        &path,
        include_bytes!("../../../tests/fixtures/r2-ledger.csv"),
    )
    .unwrap();
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(Operation::Commit {
        path: path.clone(),
        account: account.clone(),
        columns: CSV_FIELDS.map(str::to_string).to_vec(),
        delimiter: b',',
        preview: preview.clone(),
        rows: vec![1, 2, 3],
        suspects: vec![],
        independent: false
    }
    .execute(lib.clone(), cancelled)
    .is_err());
    assert_eq!(lib.ledger_revision().unwrap(), 0);
    let token = CancelToken::new();
    let result = Operation::Commit {
        path: path.clone(),
        account: account.clone(),
        columns: CSV_FIELDS.map(str::to_string).to_vec(),
        delimiter: b',',
        preview,
        rows: vec![1, 2, 3],
        suspects: vec![],
        independent: false,
    }
    .execute(lib.clone(), token.clone())
    .unwrap();
    token.cancel();
    assert!(result.message.contains("新增 3"));
    assert!(lib.ledger_revision().unwrap() > 0);
}

#[gpui_kit::test]
fn r2_w05_desktop_settings_grants_and_connection_revoke(cx: &mut TestAppContext) {
    use delta_app::ai::runtime::{ModelConnectionConfig, Protocol, ProxyPolicy};
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = lib.create_account("授权测试", "brokerage").unwrap();
    let config = ModelConnectionConfig {
        id: "desktop-primary".into(),
        protocol: Protocol::OpenaiResponses,
        base_url: "http://127.0.0.1:9/v1".into(),
        model_id: "controlled".into(),
        credential_ref: "synthetic-ref".into(),
        context_window_tokens: 8192,
        connect_timeout_secs: 1,
        idle_timeout_secs: 1,
        request_timeout_secs: 2,
        proxy: ProxyPolicy::System,
    };
    lib.save_desktop_settings(&DesktopSettings {
        connection: Some(config),
        connection_enabled: true,
        selected_accounts: vec![account.clone()],
        allowed_accounts: vec![account.clone()],
        ..Default::default()
    })
    .unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    let grants = ui.state(|s, _, _| s.business.grants.clone());
    let lease = grants
        .admit(
            "desktop-primary",
            std::slice::from_ref(&account),
            &CancelToken::new(),
        )
        .unwrap();
    ui.click("nav-ai");
    ui.click(format!("grant-{account}"));
    assert!(lease.revocation().is_some());
    assert!(lib.desktop_settings().unwrap().allowed_accounts.is_empty());
    ui.click(format!("grant-{account}"));
    let lease = grants
        .admit(
            "desktop-primary",
            std::slice::from_ref(&account),
            &CancelToken::new(),
        )
        .unwrap();
    ui.click("revoke-ai");
    assert!(lease.revocation().is_some());
    assert!(!lib.desktop_settings().unwrap().connection_enabled);
}

/// Real desktop send button -> task executor -> Rust HTTP client -> production
/// host -> SQLite -> evidence button. Only the credential source is synthetic.
#[gpui_kit::test]
fn r2_w05_desktop_chat_controlled_endpoint_opens_validated_evidence(cx: &mut TestAppContext) {
    use delta_app::ai::runtime::{ModelConnectionConfig, Protocol, ProxyPolicy};
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut captured = Vec::new();
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(8)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            let request = loop {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let len: usize = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + len {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[end + 4..end + 4 + len],
                        )
                        .unwrap();
                    }
                }
            };
            captured.push(request.clone());
            let body = if turn == 0 {
                let args = serde_json::json!({"scope":{"account_ids":["controlled-account"],"start_at":"2026-01-01T00:00:00Z","end_at":"2026-02-01T00:00:00Z","reporting_currency":"USD"},"as_of":"2026-02-01T00:00:00Z"});
                format!("event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"controlled-1\"}}}}\n\n",serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"desktop-call","name":"get_portfolio_summary","arguments":args.to_string()}}))
            } else {
                let output = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["type"] == "function_call_output")
                    .unwrap();
                let receipt: serde_json::Value =
                    serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
                let evidence = receipt["evidence_refs"][0]
                    .as_str()
                    .unwrap_or_else(|| panic!("controlled tool receipt: {receipt}"));
                format!("event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"controlled-2\"}}}}\n\n",serde_json::json!({"type":"response.output_text.delta","delta":format!("净资产 2068，证据 {evidence}。仅合成样本。 ")}))
            };
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        }
        captured
    });
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = "controlled-account".to_string();
    lib.ensure_account(&account, "合成 AI", "brokerage")
        .unwrap();
    let p = lib
        .preview_mapped_csv(
            &account,
            include_bytes!("../../../tests/fixtures/r2-ledger.csv"),
            &CSV_FIELDS.map(str::to_string),
            b',',
        )
        .unwrap();
    lib.commit_preview(&p.batch_id, &p.mapping_version, &p.file_hash, &[1, 2, 3])
        .unwrap();
    lib.import_market_file(
        &serde_json::from_str(include_str!("../../../tests/fixtures/r2-ohlcv.json")).unwrap(),
    )
    .unwrap();
    let config = ModelConnectionConfig {
        id: "desktop-primary".into(),
        protocol: Protocol::OpenaiResponses,
        base_url: format!("http://{address}/v1"),
        model_id: "controlled".into(),
        credential_ref: "synthetic-ref".into(),
        context_window_tokens: 8192,
        connect_timeout_secs: 2,
        idle_timeout_secs: 5,
        request_timeout_secs: 8,
        proxy: ProxyPolicy::System,
    };
    lib.save_desktop_settings(&DesktopSettings {
        connection: Some(config),
        connection_enabled: true,
        selected_accounts: vec![account.clone()],
        allowed_accounts: vec![account],
        start_at: "2026-01-01T00:00:00Z".into(),
        end_at: "2026-02-01T00:00:00Z".into(),
        ..Default::default()
    })
    .unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    ui.state(|s, _, _| {
        s.business.credentials = Arc::new(delta_infra::model::StaticCredentials {
            key: "synthetic-key".into(),
        })
    });
    ui.click("nav-ai");
    ui.click("send-ai");
    let reference = ui.state(|s, _, _| {
        assert!(
            s.business.transcript.contains("Succeeded"),
            "{}",
            s.business.transcript
        );
        s.business.refs[0].clone()
    });
    ui.click(format!("ai-evidence-{reference}"));
    ui.state(|s, _, _| assert!(s.business.evidence.as_ref().unwrap().body.contains("2068")));
    assert!(lib.desktop_settings().unwrap().session_id.is_some());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[gpui_kit::test]
fn r2_w02_changed_file_and_mapping_cannot_commit(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = lib.create_account("safe import", "brokerage").unwrap();
    lib.save_desktop_settings(&DesktopSettings {
        selected_accounts: vec![account],
        ..Default::default()
    })
    .unwrap();
    let path = dir.path().join("input.csv");
    let original = include_bytes!("../../../tests/fixtures/r2-ledger.csv");
    std::fs::write(&path, original).unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    ui.click("nav-import");
    ui.type_in("csv-path", &path.to_string_lossy());
    ui.click("preview-csv");
    let mut edited = original.to_vec();
    edited.extend_from_slice(b"\n");
    std::fs::write(&path, &edited).unwrap();
    ui.click("commit-csv");
    assert_eq!(lib.ledger_revision().unwrap(), 0);
    ui.state(|s, _, _| assert!(s.status.contains("changed")));
    ui.type_in("mapping", "changed");
    ui.state(|s, _, _| assert!(s.business.preview.is_none()));
}

#[gpui_kit::test]
fn r2_w01_library_switch_clears_connection_and_note_close_is_guarded(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("one/library.sqlite"), "USD").unwrap());
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    ui.click("nav-journal");
    ui.type_in("note-title", "unsaved");
    ui.state(|s, _, cx| assert!(!s.may_close(cx)));
    ui.click("discard-note");
    ui.state(|s, _, cx| assert!(s.may_close(cx)));
    ui.click("nav-settings");
    ui.type_in("model-url", "https://old.invalid/v1");
    ui.type_in("model-id", "old");
    ui.type_in("credential-ref", "old-reference");
    ui.type_in(
        "library-path",
        &dir.path().join("two/library.sqlite").to_string_lossy(),
    );
    ui.click("create-library");
    ui.state(|s, _, cx| {
        assert!(s.field_value("model-url", cx).is_empty());
        assert!(s.field_value("credential-ref", cx).is_empty());
        assert!(!s.business.enabled);
    });
}

#[gpui_kit::test]
fn r2_w03_middle_window_has_ma_warmup_and_evidence_click(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::create(&dir.path().join("library.sqlite"), "USD").unwrap());
    let account = lib.create_account("chart", "brokerage").unwrap();
    let p = lib
        .preview_mapped_csv(
            &account,
            include_bytes!("../../../tests/fixtures/r2-ledger.csv"),
            &CSV_FIELDS.map(str::to_string),
            b',',
        )
        .unwrap();
    lib.commit_preview(&p.batch_id, &p.mapping_version, &p.file_hash, &[1, 2, 3])
        .unwrap();
    lib.import_market_file(
        &serde_json::from_str(include_str!("../../../tests/fixtures/r2-ohlcv.json")).unwrap(),
    )
    .unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    ui.click("nav-chart");
    ui.type_in("chart-id", "NASDAQ:AAPL");
    ui.type_in("chart-start", "2026-01-20T00:00:00Z");
    ui.click("load-chart");
    let evidence = ui.state(|s, _, cx| {
        let chart = s.chart.read(cx);
        assert_eq!(chart.offset, 19);
        assert_eq!(chart.ma[chart.offset], Some(105.));
        assert_eq!(chart.dates[chart.offset], "2026-01-20");
        chart.markers[0].1.clone()
    });
    ui.state(|s, _, cx| {
        s.chart.update(cx, |_, cx| {
            cx.emit(crate::chart::ChartEvidence(evidence.clone()))
        })
    });
    ui.step(|_, _| {});
    ui.state(|s, _, _| assert_eq!(s.business.evidence.as_ref().unwrap().evidence_id, evidence));
    ui.click("chart-zoom-out");
    ui.click("chart-left");
    ui.state(|s, _, cx| {
        let chart = s.chart.read(cx);
        assert_eq!(chart.offset, 19);
        assert_eq!(chart.visible, 12);
    });
}

#[gpui_kit::test]
fn r2_w02_named_portfolio_deduplicates_accounts_and_survives_restart(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.sqlite");
    let lib = Arc::new(Library::create(&path, "USD").unwrap());
    let bank = lib.create_account("Bank", "bank").unwrap();
    let wallet = lib.create_account("Wallet", "wallet").unwrap();
    let p = lib
        .preview_mapped_csv(
            &bank,
            include_bytes!("../../../tests/fixtures/r2-ledger.csv"),
            &CSV_FIELDS.map(str::to_string),
            b',',
        )
        .unwrap();
    lib.commit_preview(&p.batch_id, &p.mapping_version, &p.file_hash, &[1, 2, 3])
        .unwrap();
    lib.import_market_file(
        &serde_json::from_str(include_str!("../../../tests/fixtures/r2-ohlcv.json")).unwrap(),
    )
    .unwrap();
    lib.save_desktop_settings(&DesktopSettings {
        selected_accounts: vec![bank.clone(), wallet.clone()],
        start_at: "2026-01-01T00:00:00Z".into(),
        end_at: "2026-02-01T00:00:00Z".into(),
        ..Default::default()
    })
    .unwrap();
    let mut ui = Ui::new(cx, lib.clone(), dir.path().join("recent.json"));
    ui.click("nav-accounts");
    ui.type_in("portfolio-name", "全部资产");
    ui.click("save-portfolio");
    let group = lib.portfolios().unwrap().pop().unwrap();
    assert_eq!(group.accounts.len(), 2);
    ui.click(format!("scope-{bank}"));
    ui.click("apply-scope");
    ui.click(format!("portfolio-{}", group.id));
    ui.state(|s, _, _| {
        assert_eq!(
            s.business
                .state
                .as_ref()
                .unwrap()
                .wealth
                .as_ref()
                .unwrap()
                .known_net_worth
                .to_string(),
            "2068"
        )
    });
    let duplicate = lib
        .save_portfolio("去重", &[bank.clone(), bank, wallet])
        .unwrap();
    let reopened = Library::open(&path).unwrap();
    assert_eq!(
        reopened
            .portfolios()
            .unwrap()
            .iter()
            .find(|p| p.id == duplicate)
            .unwrap()
            .accounts
            .len(),
        2
    );
    assert!(reopened
        .desktop_settings()
        .unwrap()
        .allowed_accounts
        .is_empty());
}
