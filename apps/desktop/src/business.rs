//! Workbench inputs and projections. All blocking operations live in tasks.
use super::*;
use crate::tasks::{self, Content, Operation, Snapshot, TaskGate, Update};
use delta_app::{
    ai::{
        grants::Grants,
        runtime::{ModelConnectionConfig, Protocol, ProxyPolicy},
        session::SessionStore,
    },
    contracts::CancelToken,
};
use delta_infra::sqlite::{
    app::{restore_backup, CsvPreview},
    workbench::*,
};
use gpui_kit::component::{
    button::Button,
    input::{Textarea, TextareaState},
    Disableable,
};
use gpui_kit::gpui::{Div, PathPromptOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

pub(super) struct Business {
    pub demo: bool,
    pub fields: BTreeMap<&'static str, Entity<InputState>>,
    pub body: Entity<TextareaState>,
    pub state: Option<Snapshot>,
    pub gate: TaskGate,
    pub busy: bool,
    pub ai_busy: bool,
    pub ai_generation: u64,
    pub ai_cancel: CancelToken,
    pub grants: Grants,
    pub credentials: Arc<dyn delta_infra::model::CredentialSource>,
    pub recent: PathBuf,
    pub recent_writer: Arc<std::sync::Mutex<u64>>,
    pub preview: Option<CsvPreview>,
    pub details: Vec<(i64, String, String)>,
    pub accepted: BTreeSet<i64>,
    pub independent: bool,
    pub preview_page: usize,
    pub event_page: usize,
    pub chart_data: Option<ChartWindow>,
    pub note: Option<JournalDraft>,
    pub dirty: bool,
    pub transcript: String,
    pub evidence: Option<delta_infra::sqlite::app::OpenedEvidence>,
    pub refs: Vec<String>,
    pub protocol: Protocol,
    pub enabled: bool,
    pub allowed: BTreeSet<String>,
    pub selected: BTreeSet<String>,
    pub session: Option<String>,
}

impl Business {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let specifications = [
            ("library-path", "资料库文件路径", ""),
            ("account-name", "账户名称", ""),
            (
                "account-kind",
                "bank / brokerage / crypto / wallet / manual",
                "brokerage",
            ),
            ("portfolio-name", "保存所选账户为命名组合", ""),
            ("csv-path", "UTF-8 CSV 路径", ""),
            (
                "mapping",
                "按字段顺序填写 CSV 表头，以逗号分隔",
                "occurred_at,type,asset,quantity,price,fee,source_ref,instrument,quote_currency",
            ),
            ("delimiter", "分隔符：comma / semicolon / tab", "comma"),
            ("balance-asset", "资产或币种", "USD"),
            ("balance-qty", "来源余额", ""),
            (
                "balance-date",
                "来源快照时间 RFC3339",
                "2026-01-31T00:00:00Z",
            ),
            ("correction-id", "待更正现金事件 ID", ""),
            ("correction-amount", "更正后金额", ""),
            ("market-path", "OHLCV JSON 文件路径", ""),
            ("chart-id", "场所:交易对，例如 NASDAQ:AAPL", ""),
            ("chart-start", "窗口开始 RFC3339", "2026-01-01T00:00:00Z"),
            ("chart-end", "窗口结束 RFC3339", "2026-02-01T00:00:00Z"),
            ("note-title", "笔记标题", ""),
            ("note-query", "搜索笔记正文或标题", ""),
            ("link-event", "成交事件 ID", ""),
            ("link-qty", "关联数量", ""),
            ("link-capacity", "成交数量（只读参考）", ""),
            ("scope-start", "分析开始 RFC3339", "2000-01-01T00:00:00Z"),
            ("scope-end", "分析结束 RFC3339", "2026-10-08T00:00:00Z"),
            ("currency", "报表币种", "USD"),
            ("timezone", "显示时区", "Asia/Shanghai"),
            ("model-url", "API 根地址 https://…", ""),
            ("model-id", "模型 ID", ""),
            ("credential-ref", "系统凭据引用名称", ""),
            ("secret", "API key（只存系统凭据库）", ""),
            ("context-window", "上下文 token 上限", "8192"),
            (
                "prompt",
                "只读复盘问题",
                "请解释当前范围的损益，并列出数据缺口和证据。",
            ),
            ("backup-path", "新备份目录", ""),
            ("export-path", "空导出目录", ""),
            ("restore-source", "备份目录（含 manifest.json）", ""),
            ("restore-path", "恢复到新目录中的 library.sqlite", ""),
        ];
        let fields: BTreeMap<_, _> = specifications
            .into_iter()
            .map(|(id, placeholder, value)| {
                (
                    id,
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(placeholder)
                            .default_value(value)
                            .masked(id == "secret")
                    }),
                )
            })
            .collect();
        for key in ["note-title", "note-query"] {
            cx.subscribe(
                &fields[key],
                |this: &mut Workspace, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.update_dirty(cx);
                        cx.notify();
                    }
                },
            )
            .detach();
        }
        for key in ["csv-path", "mapping", "delimiter"] {
            cx.subscribe(
                &fields[key],
                |this: &mut Workspace, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.business.preview = None;
                        this.business.details.clear();
                        this.business.accepted.clear();
                        cx.notify();
                    }
                },
            )
            .detach();
        }
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("写下交易理由、证据与复盘…")
                .rows(9)
        });
        cx.subscribe(&body, |this: &mut Workspace, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.update_dirty(cx);
                cx.notify();
            }
        })
        .detach();
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("DELTA");
        Self {
            demo: false,
            fields,
            body,
            state: None,
            gate: TaskGate::default(),
            busy: false,
            ai_busy: false,
            ai_generation: 0,
            ai_cancel: CancelToken::new(),
            grants: Grants::new(),
            credentials: Arc::new(delta_infra::model::OsCredentials),
            recent: root.join("desktop.json"),
            recent_writer: Arc::new(std::sync::Mutex::new(0)),
            preview: None,
            details: Vec::new(),
            accepted: BTreeSet::new(),
            independent: false,
            preview_page: 0,
            event_page: 0,
            chart_data: None,
            note: None,
            dirty: false,
            transcript: String::new(),
            evidence: None,
            refs: Vec::new(),
            protocol: Protocol::OpenaiResponses,
            enabled: false,
            allowed: BTreeSet::new(),
            selected: BTreeSet::new(),
            session: None,
        }
    }
}

impl Drop for Business {
    fn drop(&mut self) {
        self.gate.cancel.cancel();
        self.ai_cancel.cancel();
        self.grants.restrict_accounts(Vec::<String>::new());
    }
}

impl Workspace {
    pub(crate) fn may_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.business.dirty {
            self.page = Page::Journal;
            self.status = "笔记尚未保存，请保存或点击「放弃未保存编辑」后关闭".into();
            cx.notify();
            false
        } else {
            true
        }
    }
    fn update_dirty(&mut self, cx: &App) {
        let note = self.business.note.as_ref();
        self.business.dirty = self.business.body.read(cx).value().as_ref()
            != note.map(|n| n.body.as_str()).unwrap_or("")
            || self.field_value("note-title", cx) != note.map(|n| n.title.as_str()).unwrap_or("");
    }
    pub(super) fn field_value(&self, id: &str, cx: &App) -> String {
        self.business.fields[id].read(cx).value().to_string()
    }
    fn set_field(
        &self,
        id: &str,
        value: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = value.into();
        self.business.fields[id].update(cx, |state, cx| state.set_value(value, window, cx));
    }
    fn field(&self, id: &'static str, title: &str) -> Div {
        column()
            .gap_1()
            .min_w(px(230.))
            .flex_1()
            .child(label(title, 11., MUTED))
            .child(
                Input::new(&self.business.fields[id])
                    .id(id)
                    .disabled(self.business.busy),
            )
    }
    fn action(
        &self,
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        enabled: bool,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Button {
        let id = id.into();
        Button::new(id.clone())
            .label(title)
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, window, cx| f(this, window, cx)))
    }
    fn title(&self, text: &str) -> Div {
        label(text, 20., INK)
    }
    fn form(&self) -> Div {
        column().p_4().gap_3().w_full().min_w(px(720.))
    }
    fn ready(&self) -> bool {
        self.library.is_some() && !self.business.busy
    }

    pub(super) fn start_operation(
        &mut self,
        op: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.business.busy {
            self.status = "请等待当前本地操作结束，或取消后重试".into();
            return;
        }
        let Some(library) = self.library.clone() else {
            self.status = "请先创建或打开资料库".into();
            return;
        };
        let (epoch, serial, cancel) = self.business.gate.begin();
        self.business.busy = true;
        self.status = "处理中…".into();
        let background = cx
            .background_executor()
            .spawn(async move { op.execute(library, cancel) });
        cx.spawn_in(window, async move |view, cx| {
            let result = background.await;
            let _ = view.update_in(cx, |this, window, cx| {
                if !this.business.gate.accepts(epoch, serial) {
                    return;
                }
                this.business.busy = false;
                match result {
                    Ok(update) => this.apply_update(update, window, cx),
                    Err(e) => this.status = format!("操作失败：{e}。输入已保留，可修正后重试"),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_update(&mut self, update: Update, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = update.snapshot {
            self.apply_snapshot(state, false, window, cx);
        }
        self.status = update.message;
        match update.content {
            Content::None => {}
            Content::Permissions(settings) => {
                self.business.enabled = settings.connection_enabled;
                self.business.allowed = settings.allowed_accounts.iter().cloned().collect();
                self.business
                    .grants
                    .restrict_accounts(settings.allowed_accounts.clone());
                if settings.connection_enabled {
                    self.business.grants.restore_connection("desktop-primary");
                } else {
                    self.business.grants.revoke_connection("desktop-primary");
                }
                if let Some(state) = &mut self.business.state {
                    state.settings = *settings;
                }
            }
            Content::Preview(preview, details) => {
                self.business.accepted = preview
                    .rows
                    .iter()
                    .filter(|r| r.state == "valid")
                    .map(|r| r.row_no)
                    .collect();
                self.business.preview = Some(preview);
                self.business.details = details;
                self.business.preview_page = 0;
            }
            Content::Chart(data) => {
                self.install_chart(data, cx);
            }
            Content::Note(note) => {
                self.set_field("note-title", &note.title, window, cx);
                self.business.body.update(cx, |input, cx| {
                    input.set_value(note.body.clone(), window, cx)
                });
                self.business.note = Some(note);
                self.business.dirty = false;
            }
            Content::Evidence(evidence) => {
                self.business.evidence = Some(evidence);
            }
            Content::Transcript(id, text, refs) => {
                self.business.transcript = text;
                self.business.refs = refs;
                self.business.session = Some(id);
            }
        }
    }

    pub(super) fn apply_snapshot(
        &mut self,
        state: Snapshot,
        initial: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lines = state
            .lines
            .iter()
            .map(|l| {
                (
                    l.account.clone(),
                    l.asset.clone(),
                    l.quantity.clone(),
                    l.cost.clone(),
                )
            })
            .collect();
        self.hits = state.instruments.clone();
        self.watch = state.watch.clone();
        self.business.selected = state.settings.selected_accounts.iter().cloned().collect();
        if initial {
            self.business.allowed = state.settings.allowed_accounts.iter().cloned().collect();
            self.business.enabled = state.settings.connection_enabled;
            self.business
                .grants
                .restrict_accounts(state.settings.allowed_accounts.clone());
            self.business.session = state.settings.session_id.clone();
            for (key, value) in [
                ("scope-start", &state.settings.start_at),
                ("scope-end", &state.settings.end_at),
                ("currency", &state.currency),
                ("timezone", &state.timezone),
            ] {
                self.set_field(key, value, window, cx);
            }
            if let Some(connection) = &state.settings.connection {
                self.business.protocol = connection.protocol;
                for (key, value) in [
                    ("model-url", &connection.base_url),
                    ("model-id", &connection.model_id),
                    ("credential-ref", &connection.credential_ref),
                ] {
                    self.set_field(key, value, window, cx);
                }
                self.set_field(
                    "context-window",
                    connection.context_window_tokens.to_string(),
                    window,
                    cx,
                );
                if !self.business.enabled {
                    self.business.grants.revoke_connection(&connection.id);
                }
            }
            if let Some(context) = state.settings.chart.clone() {
                self.set_chart_fields(&context, window, cx);
            }
        }
        self.business.state = Some(state);
        let query = self.search.read(cx).value().to_string();
        self.run_search(&query);
    }

    pub(super) fn startup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let args = std::env::args().collect::<Vec<_>>();
        if let Some(i) = args.iter().position(|a| a == "--config-dir") {
            if let Some(path) = args.get(i + 1) {
                self.business.recent = PathBuf::from(path).join("desktop.json");
            }
        }
        if args.iter().any(|a| a == "--demo") {
            self.open_library(None, true, false, window, cx);
            return;
        }
        if let Some(i) = args.iter().position(|a| a == "--library") {
            if let Some(path) = args.get(i + 1) {
                self.open_library(Some(PathBuf::from(path)), false, false, window, cx);
                return;
            }
        }
        let startup_epoch = self.business.gate.epoch;
        let config = self.business.recent.clone();
        let work = cx
            .background_executor()
            .spawn(async move { read_recent(&config) });
        cx.spawn_in(window, async move |view, cx| {
            let result = work.await;
            let _ = view.update_in(cx, |this, w, cx| {
                if this.business.gate.epoch != startup_epoch {
                    return;
                }
                match result {
                    Ok(Some(path)) => this.open_library(Some(path), false, false, w, cx),
                    Ok(None) => {
                        this.page = Page::Settings;
                        this.status =
                            "欢迎使用 DELTA。创建或打开本地资料库；也可进入显式演示模式".into();
                    }
                    Err(e) => {
                        this.page = Page::Settings;
                        this.status = format!("读取上次资料库失败：{e}。请选择文件；原配置未覆盖");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_library(
        &mut self,
        path: Option<PathBuf>,
        demo: bool,
        create: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.business.dirty {
            self.status = "笔记有未保存内容，请先保存或明确放弃编辑".into();
            return;
        }
        self.business.gate.switch();
        self.business.ai_cancel.cancel();
        self.business.grants.restrict_accounts(Vec::<String>::new());
        let (epoch, serial, cancel) = self.business.gate.begin();
        self.business.busy = true;
        self.business.ai_busy = false;
        self.status = "正在打开资料库…".into();
        let recent = self.business.recent.clone();
        let background = cx.background_executor().spawn(async move {
            let result = (|| -> anyhow::Result<_> {
                if cancel.is_cancelled() {
                    return Err(anyhow::anyhow!("打开已取消"));
                }
                let library = if demo {
                    let dir = std::env::temp_dir()
                        .join("delta-r2-demo")
                        .join(uuid::Uuid::new_v4().to_string());
                    std::fs::create_dir_all(&dir)?;
                    let lib = seed_synthetic_demo(&dir.join("library.sqlite"))?;
                    lib.import_market_file(&serde_json::from_str(include_str!(
                        "../../../tests/fixtures/r2-ohlcv.json"
                    ))?)?;
                    lib.save_desktop_settings(&DesktopSettings {
                        selected_accounts: vec!["acc-us".into()],
                        start_at: "2026-01-01T00:00:00Z".into(),
                        end_at: "2026-02-01T00:00:00Z".into(),
                        chart: Some(ChartContext {
                            instrument: "NASDAQ:AAPL".into(),
                            start_at: "2026-01-01T00:00:00Z".into(),
                            end_at: "2026-02-01T00:00:00Z".into(),
                            adjustment: "raw".into(),
                            dataset_version: None,
                        }),
                        ..Default::default()
                    })?;
                    lib
                } else {
                    let path = path.ok_or_else(|| anyhow::anyhow!("请选择资料库文件"))?;
                    if create {
                        Library::create(&path, "USD")?
                    } else {
                        Library::open(&path)?
                    }
                };
                library.mark_interrupted_runs()?;
                let snapshot = tasks::snapshot(&library)?;
                Ok((Arc::new(library), snapshot))
            })();
            result
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = background.await;
            let _ = view.update_in(cx, |this, window, cx| {
                if !this.business.gate.accepts(epoch, serial) {
                    return;
                }
                this.business.busy = false;
                match result {
                    Ok((library, state)) => {
                        this.business.demo = demo;
                        this.business.grants = Grants::new();
                        this.business.preview = None;
                        this.business.details.clear();
                        this.business.note = None;
                        this.business.dirty = false;
                        this.business.chart_data = None;
                        this.business.transcript.clear();
                        this.business.evidence = None;
                        this.business.refs.clear();
                        this.chart.update(cx, |chart, cx| {
                            *chart = ChartState::new(0);
                            cx.notify();
                        });
                        this.set_field("library-path", library.path.to_string_lossy(), window, cx);
                        for key in [
                            "secret",
                            "note-query",
                            "model-url",
                            "model-id",
                            "credential-ref",
                            "prompt",
                            "csv-path",
                            "market-path",
                            "chart-id",
                            "balance-qty",
                            "correction-id",
                            "correction-amount",
                            "link-event",
                            "link-qty",
                            "link-capacity",
                            "backup-path",
                            "export-path",
                            "restore-source",
                            "restore-path",
                            "account-name",
                            "portfolio-name",
                        ] {
                            this.set_field(key, "", window, cx);
                        }
                        this.business.protocol = Protocol::OpenaiResponses;
                        this.set_field("context-window", "8192", window, cx);
                        this.search.update(cx, |s, cx| s.set_value("", window, cx));
                        this.set_field("note-title", "", window, cx);
                        this.business
                            .body
                            .update(cx, |body, cx| body.set_value("", window, cx));
                        this.library = Some(library.clone());
                        this.apply_snapshot(state, true, window, cx);
                        this.page = Page::Home;
                        this.status = if demo {
                            "显式演示模式 · 使用临时合成库".into()
                        } else {
                            "资料库已打开".into()
                        };
                        if !demo {
                            let p = library.path.clone();
                            let writer = this.business.recent_writer.clone();
                            let result = cx.background_executor().spawn(async move {
                                let mut latest = writer.lock().unwrap_or_else(|e| e.into_inner());
                                if epoch < *latest {
                                    return Ok(());
                                }
                                write_recent(&recent, &p)?;
                                *latest = epoch;
                                Ok::<_, delta_infra::error::InfraError>(())
                            });
                            cx.spawn_in(window, async move |view, cx| {
                                if let Err(e) = result.await {
                                    let _ = view.update_in(cx, |this, _, cx| {
                                        if this.business.gate.epoch == epoch {
                                            this.status =
                                                format!("库已打开，但启动路径保存失败：{e}");
                                            cx.notify();
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                        if let Some(context) = this
                            .business
                            .state
                            .as_ref()
                            .and_then(|s| s.settings.chart.clone())
                        {
                            this.start_operation(Operation::Chart(context), window, cx);
                        }
                    }
                    Err(e) => {
                        this.status = format!("打开失败：{e}；现有资料库保持不变");
                        if let Some(state) = &this.business.state {
                            this.business
                                .grants
                                .restrict_accounts(state.settings.allowed_accounts.clone());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_path(
        &mut self,
        key: &'static str,
        directory: bool,
        new_file: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.business.gate.epoch;
        if new_file {
            let receiver = cx.prompt_for_new_path(&std::env::temp_dir(), Some("library.sqlite"));
            cx.spawn_in(window, async move |view, cx| {
                if let Ok(Ok(Some(path))) = receiver.await {
                    let _ = view.update_in(cx, |this, w, cx| {
                        if this.business.gate.epoch == epoch {
                            this.set_field(key, path.to_string_lossy(), w, cx);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        } else {
            let receiver = cx.prompt_for_paths(PathPromptOptions {
                files: !directory,
                directories: directory,
                multiple: false,
                prompt: None,
            });
            cx.spawn_in(window, async move |view, cx| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    if let Some(path) = paths.first() {
                        let _ = view.update_in(cx, |this, w, cx| {
                            if this.business.gate.epoch == epoch {
                                this.set_field(key, path.to_string_lossy(), w, cx);
                                cx.notify();
                            }
                        });
                    }
                }
            })
            .detach();
        }
    }

    fn settings_from_inputs(&self, cx: &App) -> anyhow::Result<DesktopSettings> {
        let mut settings = self
            .business
            .state
            .as_ref()
            .map(|s| s.settings.clone())
            .unwrap_or_default();
        settings.selected_accounts = self.business.selected.iter().cloned().collect();
        settings.allowed_accounts = self.business.allowed.iter().cloned().collect();
        settings.start_at = self.field_value("scope-start", cx);
        settings.end_at = self.field_value("scope-end", cx);
        settings.connection_enabled = self.business.enabled;
        settings.session_id = self.business.session.clone();
        if !self.field_value("model-url", cx).is_empty() {
            settings.connection = Some(ModelConnectionConfig {
                id: "desktop-primary".into(),
                protocol: self.business.protocol,
                base_url: self.field_value("model-url", cx),
                model_id: self.field_value("model-id", cx),
                credential_ref: self.field_value("credential-ref", cx),
                context_window_tokens: self.field_value("context-window", cx).parse()?,
                connect_timeout_secs: 10,
                idle_timeout_secs: 30,
                request_timeout_secs: 120,
                proxy: ProxyPolicy::System,
            });
        } else {
            settings.connection = None;
            settings.connection_enabled = false;
        }
        Ok(settings)
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() {
            return;
        }
        match self.settings_from_inputs(cx) {
            Ok(mut settings) => {
                // A configuration/scope change always starts a clean context. Read
                // history remains available through an explicit history action.
                self.business.preview = None;
                self.business.details.clear();
                settings.session_id = None;
                self.business.session = None;
                self.business.ai_generation += 1;
                let persisted = self
                    .business
                    .state
                    .as_ref()
                    .map(|s| s.settings.allowed_accounts.as_slice())
                    .unwrap_or(&[]);
                let reduced = settings
                    .allowed_accounts
                    .iter()
                    .filter(|id| persisted.contains(id))
                    .cloned()
                    .collect::<Vec<_>>();
                self.business.grants.restrict_accounts(reduced);
                if !settings.connection_enabled {
                    self.business.grants.revoke_connection("desktop-primary");
                }
                self.business.ai_cancel.cancel();
                self.start_operation(
                    Operation::Settings(
                        Box::new(settings),
                        self.field_value("currency", cx),
                        self.field_value("timezone", cx),
                    ),
                    window,
                    cx,
                );
            }
            Err(e) => self.status = format!("设置格式错误：{e}"),
        };
    }

    fn set_chart_fields(&self, context: &ChartContext, w: &mut Window, cx: &mut Context<Self>) {
        for (id, value) in [
            ("chart-id", &context.instrument),
            ("chart-start", &context.start_at),
            ("chart-end", &context.end_at),
        ] {
            self.set_field(id, value, w, cx);
        }
    }
    pub(super) fn select_instrument(&mut self, id: String, w: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Chart;
        self.set_field("chart-id", id, w, cx);
        self.load_chart(w, cx);
    }
    fn chart_context(&self, cx: &App) -> ChartContext {
        ChartContext {
            instrument: self.field_value("chart-id", cx),
            start_at: self.field_value("chart-start", cx),
            end_at: self.field_value("chart-end", cx),
            adjustment: "raw".into(),
            dataset_version: None,
        }
    }
    fn load_chart(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.start_operation(Operation::Chart(self.chart_context(cx)), w, cx);
    }
    fn install_chart(&mut self, data: ChartWindow, cx: &mut Context<Self>) {
        let candles = data
            .bars
            .iter()
            .map(|bar| crate::chart::Candle {
                open: bar.open.parse().unwrap_or(0.),
                high: bar.high.parse().unwrap_or(0.),
                low: bar.low.parse().unwrap_or(0.),
                close: bar.close.parse().unwrap_or(0.),
                volume: bar.volume.parse().unwrap_or(0.),
            })
            .collect::<Vec<_>>();
        self.chart.update(cx, |state, cx| {
            *state = ChartState::from_candles(candles);
            state.min_offset = data.warmup;
            state.offset = data.warmup;
            state.visible = (data.bars.len() - data.warmup).min(1000);
            state.dates = data.bars.iter().map(|b| b.session_date.clone()).collect();
            state.markers = data
                .markers
                .iter()
                .filter_map(|(date, id)| {
                    state
                        .dates
                        .iter()
                        .position(|d| d == date)
                        .map(|i| (i, id.clone()))
                })
                .collect();
            cx.notify();
        });
        self.business.chart_data = Some(data);
    }

    fn save_note(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let account = self
            .business
            .note
            .as_ref()
            .map(|n| n.account_id.clone())
            .or_else(|| self.business.selected.iter().next().cloned())
            .unwrap_or_default();
        let prior = self.business.note.clone();
        let draft = JournalDraft {
            id: prior.as_ref().and_then(|n| n.id.clone()),
            title: self.field_value("note-title", cx),
            body: self.business.body.read(cx).value().to_string(),
            account_id: account,
            revision: prior.as_ref().map(|n| n.revision).unwrap_or(0),
            chart: prior
                .and_then(|n| n.chart)
                .or_else(|| self.current_chart_context(cx)),
        };
        self.start_operation(Operation::SaveNote(draft), w, cx);
    }
    fn current_chart_context(&self, cx: &App) -> Option<ChartContext> {
        let data = self.business.chart_data.as_ref()?;
        let chart = self.chart.read(cx);
        let start = data.bars.get(chart.offset)?;
        let end = data.bars.get(
            (chart.offset + chart.visible)
                .min(data.bars.len())
                .saturating_sub(1),
        )?;
        let mut context = data.context.clone();
        context.start_at = start.open_at.clone();
        context.end_at = end.close_at.clone();
        Some(context)
    }

    fn send_ai(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.business.ai_busy || !self.ready() {
            return;
        }
        let Some(library) = self.library.clone() else {
            return;
        };
        // Only persisted, explicitly applied scope is eligible for sending.
        let Some(state) = &self.business.state else {
            return;
        };
        let mut settings = state.settings.clone();
        settings.session_id = self.business.session.clone();
        if !settings.connection_enabled || settings.connection.is_none() {
            self.status = "请在设置中配置并启用连接，明确授权账户后保存".into();
            return;
        }
        self.business.ai_generation += 1;
        let generation = self.business.ai_generation;
        let epoch = self.business.gate.epoch;
        let grants = self.business.grants.clone();
        let cancel = CancelToken::new();
        self.business.ai_cancel = cancel.clone();
        self.business.ai_busy = true;
        self.business.transcript = "生成中，尚未校验…".into();
        self.business.refs.clear();
        let prompt = self.field_value("prompt", cx);
        let credentials = self.business.credentials.clone();
        let background = cx.background_executor().spawn(async move {
            tasks::run_ai(library, settings, prompt, grants, cancel, credentials)
        });
        cx.spawn_in(w, async move |view, cx| {
            let result = background.await;
            let _ = view.update_in(cx, |this, w, cx| {
                if this.business.gate.epoch != epoch {
                    return;
                }
                this.business.ai_busy = false;
                if this.business.ai_generation != generation {
                    this.status = "旧范围运行已终止，历史状态已保存".into();
                    this.start_operation(Operation::Refresh, w, cx);
                    return;
                }
                match result {
                    Ok(out) => {
                        this.business.session = Some(out.session_id.clone());
                        this.business.transcript = format!("{:?}\n{}", out.state, out.report_text);
                        this.business.refs = out.evidence_refs;
                        this.status = format!("AI 运行结束：{:?}", out.state);
                        this.start_operation(Operation::Session(out.session_id), w, cx);
                    }
                    Err(e) => {
                        this.business.transcript = format!("运行失败或中断：{e}");
                        this.status = "AI 未成功；已提交历史可在会话列表中打开".into();
                        this.start_operation(Operation::Refresh, w, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn settings_page(&self, cx: &mut Context<Self>) -> Div {
        self.form().child(self.title("资料库与设置"))
        .child(label("业务数据保存在所选本地资料库。演示模式使用独立临时合成库。",12.,MUTED))
        .child(self.field("library-path","资料库文件"))
        .child(row().gap_2()
            .child(self.action("choose-library","选择已有文件",true,cx,|s,w,cx|s.choose_path("library-path",false,false,w,cx)))
            .child(self.action("choose-new-library","选择新文件位置",true,cx,|s,w,cx|s.choose_path("library-path",false,true,w,cx)))
            .child(self.action("open-library","打开",true,cx,|s,w,cx|s.open_library(Some(PathBuf::from(s.field_value("library-path",cx))),false,false,w,cx)))
            .child(self.action("create-library","创建空资料库",true,cx,|s,w,cx|s.open_library(Some(PathBuf::from(s.field_value("library-path",cx))),false,true,w,cx)))
            .child(self.action("demo-library","进入演示",true,cx,|s,w,cx|s.open_library(None,true,false,w,cx))))
        .child(row().gap_3().child(self.field("currency","报表币种")).child(self.field("timezone","显示时区")))
        .child(self.title("模型连接"))
        .child(row().gap_2().child(self.action("protocol-responses","Responses",true,cx,|s,_,cx|{s.business.protocol=Protocol::OpenaiResponses;cx.notify();})).child(self.action("protocol-chat","Chat Completions",true,cx,|s,_,cx|{s.business.protocol=Protocol::OpenaiChatCompletions;cx.notify();})).child(label(format!("已选 {:?}",self.business.protocol),12.,ORANGE)))
        .child(row().gap_3().child(self.field("model-url","服务 API 根地址")).child(self.field("model-id","模型")))
        .child(row().gap_3().child(self.field("credential-ref","系统凭据引用")).child(self.field("context-window","上下文窗口")))
        .child(self.field("secret","凭据（不保存到资料库或备份）"))
        .child(row().gap_2()
            .child(self.action("save-credential","保存凭据到系统",self.ready(),cx,|s,w,cx|{let op=Operation::Credential(s.field_value("credential-ref",cx),s.field_value("secret",cx));s.set_field("secret","",w,cx);s.start_operation(op,w,cx);}))
            .child(self.action("toggle-connection",if self.business.enabled{"连接：启用（点此停用）"}else{"连接：停用（点此启用）"},self.ready(),cx,|s,w,cx|{s.business.enabled= !s.business.enabled;s.save_settings(w,cx);}))
            .child(self.action("save-settings","保存设置",self.ready(),cx,|s,w,cx|s.save_settings(w,cx))))
        .child(label("保存设置不会调用模型。授权账户与日期在 AI 助手中选择；发送按钮才开始有界只读分析。",12.,MUTED))
    }

    pub(super) fn scope_panel(&self, cx: &mut Context<Self>) -> Div {
        let accounts = self
            .business
            .state
            .as_ref()
            .map(|s| s.accounts.as_slice())
            .unwrap_or(&[]);
        card()
            .p_3()
            .gap_2()
            .child(label("当前分析范围（应用后生效）", 14., INK))
            .child(row().gap_2().flex_wrap().children(accounts.iter().map(|a| {
                let id = a.id.clone();
                let selected = self.business.selected.contains(&id);
                self.action(
                    format!("scope-{}", a.id),
                    format!("{} {}", if selected { "✓" } else { "○" }, a.name),
                    self.ready(),
                    cx,
                    move |s, _, cx| {
                        if !s.business.selected.remove(&id) {
                            s.business.selected.insert(id.clone());
                        }
                        cx.notify();
                    },
                )
            })))
            .child(
                row()
                    .gap_3()
                    .child(self.field("scope-start", "开始（含）"))
                    .child(self.field("scope-end", "结束（不含）")),
            )
            .child(self.action(
                "apply-scope",
                "应用范围与日期",
                self.ready(),
                cx,
                |s, w, cx| s.save_settings(w, cx),
            ))
    }

    pub(super) fn accounts_business_page(&self, cx: &mut Context<Self>) -> Div {
        let events = self
            .business
            .state
            .as_ref()
            .map(|s| s.events.as_slice())
            .unwrap_or(&[]);
        self.form()
            .child(self.title("账户、资产与核对"))
            .child(
                row()
                    .gap_3()
                    .child(self.field("account-name", "新账户名称"))
                    .child(self.field("account-kind", "账户类型"))
                    .child(self.action(
                        "create-account",
                        "添加账户",
                        self.ready(),
                        cx,
                        |s, w, cx| {
                            s.start_operation(
                                Operation::Account(
                                    s.field_value("account-name", cx),
                                    s.field_value("account-kind", cx),
                                ),
                                w,
                                cx,
                            )
                        },
                    )),
            )
            .child(self.scope_panel(cx))
            .child(
                row()
                    .gap_2()
                    .child(self.field("portfolio-name", "组合名称"))
                    .child(self.action(
                        "save-portfolio",
                        "保存所选账户组合",
                        self.ready() && !self.business.selected.is_empty(),
                        cx,
                        |s, w, cx| {
                            s.start_operation(
                                Operation::Portfolio(
                                    s.field_value("portfolio-name", cx),
                                    s.business.selected.iter().cloned().collect(),
                                ),
                                w,
                                cx,
                            )
                        },
                    )),
            )
            .child(
                row().gap_2().flex_wrap().children(
                    self.business
                        .state
                        .iter()
                        .flat_map(|s| &s.portfolios)
                        .map(|p| {
                            let ids = p.accounts.clone();
                            self.action(
                                format!("portfolio-{}", p.id),
                                format!("应用 {}（{} 账户）", p.name, p.accounts.len()),
                                self.ready(),
                                cx,
                                move |s, w, cx| {
                                    s.business.selected = ids.iter().cloned().collect();
                                    s.save_settings(w, cx);
                                },
                            )
                        }),
                ),
            )
            .child(self.overview_cards())
            .child(label("持仓与现金 · 数量 / FIFO 成本", 15., INK))
            .children(self.lines.iter().map(|(a, asset, q, cost)| {
                label(format!("{a} · {asset}     {q}     成本 {cost}"), 13., INK)
            }))
            .child(label("登记来源余额（差异不会自动修改账本）", 15., INK))
            .child(
                row()
                    .gap_3()
                    .child(self.field("balance-asset", "资产"))
                    .child(self.field("balance-qty", "余额"))
                    .child(self.field("balance-date", "快照时点")),
            )
            .child(self.action(
                "observe-balance",
                "登记并核对所选单账户",
                self.ready() && self.business.selected.len() == 1,
                cx,
                |s, w, cx| {
                    let a = s
                        .business
                        .selected
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_default();
                    s.start_operation(
                        Operation::Observe(
                            a,
                            s.field_value("balance-asset", cx),
                            s.field_value("balance-qty", cx),
                            s.field_value("balance-date", cx),
                        ),
                        w,
                        cx,
                    )
                },
            ))
            .children(self.business.state.iter().flat_map(|s| &s.diffs).map(|d| {
                label(
                    format!(
                        "{} · {}：账本 {} / 来源 {} / 差额 {}",
                        d.account_id.0,
                        d.asset.0,
                        d.ledger_quantity,
                        d.observed_quantity,
                        d.difference
                    ),
                    12.,
                    RED,
                )
            }))
            .child(label(
                format!("成交流水 · {} 条（每页 20 条）", events.len()),
                15.,
                INK,
            ))
            .child(
                row()
                    .gap_2()
                    .child(self.action(
                        "events-prev",
                        "上一页",
                        self.business.event_page > 0,
                        cx,
                        |s, _, cx| {
                            s.business.event_page = s.business.event_page.saturating_sub(1);
                            cx.notify();
                        },
                    ))
                    .child(self.action(
                        "events-next",
                        "下一页",
                        (self.business.event_page + 1) * 20 < events.len(),
                        cx,
                        |s, _, cx| {
                            s.business.event_page += 1;
                            cx.notify();
                        },
                    )),
            )
            .children(
                events
                    .iter()
                    .skip(self.business.event_page * 20)
                    .take(20)
                    .map(|e| {
                        let id = e.id.clone();
                        let ins = e.instrument.clone();
                        let account = e.account.clone();
                        let qty = e.quantity.clone();
                        let day = e.at.clone();
                        let id_note = id.clone();
                        let id_correct = id.clone();
                        card()
                            .p_2()
                            .gap_1()
                            .child(label(
                                format!("{} · {} · {}", e.display_at, e.account, e.id),
                                11.,
                                MUTED,
                            ))
                            .child(label(&e.detail, 12., INK))
                            .child(
                                row()
                                    .gap_2()
                                    .child(self.action(
                                        format!("event-{}", e.id),
                                        "打开证据",
                                        self.ready(),
                                        cx,
                                        move |s, w, cx| {
                                            s.start_operation(
                                                Operation::Evidence(format!("event:{id}")),
                                                w,
                                                cx,
                                            )
                                        },
                                    ))
                                    .child(self.action(
                                        format!("event-note-{}", e.id),
                                        "关联笔记",
                                        self.ready() && ins.is_some() && !self.business.dirty,
                                        cx,
                                        move |s, w, cx| {
                                            s.page = Page::Journal;
                                            s.business.note = Some(JournalDraft {
                                                id: None,
                                                title: String::new(),
                                                body: String::new(),
                                                account_id: account.clone(),
                                                revision: 0,
                                                chart: ins.as_ref().map(|i| ChartContext {
                                                    instrument: i.clone(),
                                                    start_at: day.clone(),
                                                    end_at: (chrono::DateTime::parse_from_rfc3339(
                                                        &day,
                                                    )
                                                    .unwrap()
                                                        + chrono::Duration::days(1))
                                                    .to_rfc3339(),
                                                    adjustment: "raw".into(),
                                                    dataset_version: None,
                                                }),
                                            });
                                            s.set_field("link-event", &id_note, w, cx);
                                            s.set_field(
                                                "link-qty",
                                                qty.clone().unwrap_or_default(),
                                                w,
                                                cx,
                                            );
                                            s.set_field(
                                                "link-capacity",
                                                qty.clone().unwrap_or_default(),
                                                w,
                                                cx,
                                            );
                                            s.set_field("note-title", "成交复盘", w, cx);
                                            s.business
                                                .body
                                                .update(cx, |v, cx| v.set_value("", w, cx));
                                            cx.notify();
                                        },
                                    ))
                                    .child(self.action(
                                        format!("event-correct-{}", e.id),
                                        "现金更正",
                                        self.ready() && e.instrument.is_none(),
                                        cx,
                                        move |s, w, cx| {
                                            s.set_field("correction-id", &id_correct, w, cx);
                                            cx.notify();
                                        },
                                    )),
                            )
                    }),
            )
            .child(
                row()
                    .gap_3()
                    .child(self.field("correction-id", "现金事件 ID"))
                    .child(self.field("correction-amount", "替代金额"))
                    .child(self.action(
                        "correct-cash",
                        "确认现金更正",
                        self.ready(),
                        cx,
                        |s, w, cx| {
                            s.start_operation(
                                Operation::Correct(
                                    s.field_value("correction-id", cx),
                                    s.field_value("correction-amount", cx),
                                ),
                                w,
                                cx,
                            )
                        },
                    )),
            )
    }

    fn overview_cards(&self) -> Div {
        let Some(s) = &self.business.state else {
            return label("请打开资料库", 14., MUTED);
        };
        let mut body = row().gap_3().items_stretch();
        if let Some(wealth) = &s.wealth {
            body = body.child(
                card()
                    .p_3()
                    .flex_1()
                    .child(label(
                        if wealth.net_worth.is_some() {
                            "净资产"
                        } else {
                            "已知部分估值"
                        },
                        13.,
                        MUTED,
                    ))
                    .child(label(
                        format!("{} {}", wealth.known_net_worth.normalize(), s.currency),
                        27.,
                        INK,
                    ))
                    .child(label(
                        format!("数据质量：{:?}", wealth.quality),
                        12.,
                        ORANGE,
                    ))
                    .children(wealth.missing_inputs.iter().map(|m| label(m, 11., RED))),
            );
        } else {
            body = body.child(card().p_3().flex_1().child(label(
                "选择账户范围后显示资产",
                16.,
                MUTED,
            )));
        }
        if let Some(pnl) = &s.pnl {
            let amount = |v: Option<rust_decimal::Decimal>| {
                v.map(|d| d.normalize().to_string())
                    .unwrap_or_else(|| "缺少输入".into())
            };
            body = body.child(
                card()
                    .p_3()
                    .flex_1()
                    .child(label(
                        format!("期间损益 {} {}", amount(pnl.total_pnl), s.currency),
                        19.,
                        INK,
                    ))
                    .child(label(
                        format!(
                            "期初 {} · 期末 {} · 净投入 {}",
                            amount(pnl.start_net_worth),
                            amount(pnl.end_net_worth),
                            amount(pnl.net_external_flow)
                        ),
                        12.,
                        MUTED,
                    ))
                    .child(label(
                        format!(
                            "已实现 {} · 未实现 {} · 费用 {} · 收入 {}",
                            amount(pnl.realized_pnl),
                            amount(pnl.unrealized_pnl),
                            amount(pnl.fees_valued),
                            amount(pnl.income)
                        ),
                        12.,
                        INK,
                    ))
                    .children(pnl.missing_inputs.iter().map(|m| label(m, 11., RED))),
            );
        }
        body
    }

    pub(super) fn live_dashboard(&self, cx: &mut Context<Self>) -> Div {
        self.form()
            .child(
                row()
                    .justify_between()
                    .child(self.title("我的工作台"))
                    .child(label("本地资料库 · 日线文件 · 只读复盘", 12., MUTED)),
            )
            .child(self.scope_panel(cx))
            .child(self.overview_cards())
            .child(
                row()
                    .gap_3()
                    .items_stretch()
                    .child(
                        card()
                            .flex_1()
                            .p_3()
                            .gap_2()
                            .child(label("市场与日线", 15., INK))
                            .child(label(
                                self.business
                                    .chart_data
                                    .as_ref()
                                    .map(|d| {
                                        format!(
                                            "{} · {} · {} 根",
                                            d.context.instrument,
                                            d.source,
                                            d.bars.len()
                                        )
                                    })
                                    .unwrap_or_else(|| "尚无所选标的日线，请导入行情文件".into()),
                                12.,
                                MUTED,
                            ))
                            .child(
                                div()
                                    .h(px(320.))
                                    .child(interactive_chart(self.chart.clone())),
                            )
                            .child(self.action(
                                "home-chart",
                                "打开图表 / 导入行情",
                                true,
                                cx,
                                |s, _, cx| {
                                    s.page = Page::Chart;
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        card()
                            .w(px(300.))
                            .p_3()
                            .gap_3()
                            .child(label("AI 复盘", 15., INK))
                            .child(label(
                                if self.business.enabled {
                                    "连接已启用；发送前查看账户范围"
                                } else {
                                    "模型未启用，本地账本可正常使用"
                                },
                                13.,
                                MUTED,
                            ))
                            .child(self.action(
                                "home-ai",
                                "打开只读助手",
                                true,
                                cx,
                                |s, _, cx| {
                                    s.page = Page::Ai;
                                    cx.notify();
                                },
                            ))
                            .child(label("训练 / 策略", 15., INK))
                            .child(label("S2 / S3 待实现", 12., MUTED)),
                    ),
            )
            .child(
                row()
                    .gap_3()
                    .items_stretch()
                    .child(
                        card()
                            .p_3()
                            .flex_1()
                            .child(label("最近笔记", 15., INK))
                            .children(
                                self.business
                                    .state
                                    .iter()
                                    .flat_map(|s| s.notes.iter().take(4))
                                    .map(|(_, title, _)| label(title, 13., INK)),
                            ),
                    )
                    .child(
                        card()
                            .p_3()
                            .flex_1()
                            .gap_2()
                            .child(label("数据与恢复", 15., INK))
                            .child(self.action(
                                "home-import",
                                "CSV 预览与入账",
                                true,
                                cx,
                                |s, _, cx| {
                                    s.page = Page::Import;
                                    cx.notify();
                                },
                            ))
                            .child(self.action(
                                "home-recovery",
                                "备份 / 导出 / 恢复",
                                true,
                                cx,
                                |s, _, cx| {
                                    s.page = Page::Recovery;
                                    cx.notify();
                                },
                            )),
                    ),
            )
    }

    pub(super) fn import_page(&self, cx: &mut Context<Self>) -> Div {
        let rows = self
            .business
            .preview
            .as_ref()
            .map(|p| p.rows.as_slice())
            .unwrap_or(&[]);
        self.form().child(self.title("CSV 导入"))
        .child(label("先在资产页选择一个账户。输入为 UTF-8；时间含时区，金额用小数点。字段顺序：时间、类型、资产、数量、价格、费用、来源 ID、标的、报价币种。",12.,MUTED))
        .child(self.field("csv-path","CSV 文件"))
        .child(row().gap_2().child(self.action("choose-csv","选择文件",true,cx,|s,w,cx|s.choose_path("csv-path",false,false,w,cx))).child(self.action("preview-csv","读取并预览",self.ready() && self.business.selected.len()==1,cx,|s,w,cx|{let delimiter=match s.field_value("delimiter",cx).as_str(){"comma"=>b',',"semicolon"=>b';',"tab"=>b'\t',_=>{s.status="分隔符需为 comma / semicolon / tab".into();return;}};s.start_operation(Operation::Preview{path:PathBuf::from(s.field_value("csv-path",cx)),account:s.business.selected.iter().next().cloned().unwrap_or_default(),columns:s.field_value("mapping",cx).split(',').map(|s|s.trim().to_string()).collect(),delimiter},w,cx);})))
        .child(self.field("mapping","字段映射：九个原文件表头，按上述顺序（空值也需保留列）"))
        .child(self.field("delimiter","分隔符"))
        .child(label(format!("预览 {} 行 · 已选 {} 行有效数据；错误行不入账",rows.len(),self.business.accepted.len()),14.,INK))
        .child(row().gap_2().child(self.action("preview-prev","上一页",self.business.preview_page>0,cx,|s,_,cx|{s.business.preview_page=s.business.preview_page.saturating_sub(1);cx.notify();})).child(self.action("preview-next","下一页",(self.business.preview_page+1)*15<rows.len(),cx,|s,_,cx|{s.business.preview_page+=1;cx.notify();})))
        .children(rows.iter().skip(self.business.preview_page*15).take(15).map(|r|{let row_no=r.row_no;card().p_2().child(row().gap_2().child(self.action(format!("accept-row-{row_no}"),if self.business.accepted.contains(&row_no){"✓ 入账"}else{"○ 排除"},r.state=="valid" && self.ready(),cx,move|s,_,cx|{if !s.business.accepted.remove(&row_no){s.business.accepted.insert(row_no);}cx.notify();})).child(label(format!("第 {} 行 · {} · {} · {}",r.row_no,r.state,r.source_ref,r.reason),12.,INK))).children(self.business.details.iter().filter(|d|d.0==row_no).map(|(_,raw,normal)|label(format!("原始：{raw}\n规范化：{normal}"),11.,MUTED)))}))
        .child(self.action("duplicate-policy",if self.business.independent{"疑似重复：按独立来源接受"}else{"疑似重复：全部跳过"},self.ready(),cx,|s,_,cx|{s.business.independent= !s.business.independent;cx.notify();}))
        .child(self.action("commit-csv","确认：提交选中行并应用疑似重复处理",self.ready() && self.business.preview.is_some(),cx,|s,w,cx|{if let Some(preview)=s.business.preview.clone(){let suspects=preview.rows.iter().filter(|r|r.state=="duplicate_suspect").map(|r|r.row_no).collect();s.start_operation(Operation::Commit{path:PathBuf::from(s.field_value("csv-path",cx)),account:s.business.selected.iter().next().cloned().unwrap_or_default(),columns:s.field_value("mapping",cx).split(',').map(|v|v.trim().to_string()).collect(),delimiter:match s.field_value("delimiter",cx).as_str(){"semicolon"=>b';',"tab"=>b'\t',_=>b','},preview,rows:s.business.accepted.iter().copied().collect(),suspects,independent:s.business.independent},w,cx);}}))
    }

    pub(super) fn chart_business_page(&self, cx: &mut Context<Self>) -> Div {
        let data = self.business.chart_data.as_ref();
        self.form()
            .child(self.title("日线与证据"))
            .child(
                row()
                    .gap_2()
                    .child(self.field("market-path", "行情 JSON 文件"))
                    .child(self.action("choose-market", "选择", true, cx, |s, w, cx| {
                        s.choose_path("market-path", false, false, w, cx)
                    }))
                    .child(self.action(
                        "import-market",
                        "导入行情文件",
                        self.ready(),
                        cx,
                        |s, w, cx| {
                            s.start_operation(
                                Operation::Market(PathBuf::from(s.field_value("market-path", cx))),
                                w,
                                cx,
                            )
                        },
                    )),
            )
            .child(
                row()
                    .gap_3()
                    .child(self.field("chart-id", "完整标的 / 场所"))
                    .child(self.field("chart-start", "窗口开始"))
                    .child(self.field("chart-end", "窗口结束")),
            )
            .child(
                row()
                    .gap_2()
                    .child(self.action(
                        "load-chart",
                        "加载日线窗口",
                        self.ready(),
                        cx,
                        |s, w, cx| s.load_chart(w, cx),
                    ))
                    .child(self.action(
                        "save-chart-context",
                        "保存当前可见窗口",
                        self.ready() && data.is_some(),
                        cx,
                        |s, w, cx| {
                            let context = s.current_chart_context(cx);
                            if let Some(state) = &s.business.state {
                                let mut settings = state.settings.clone();
                                settings.chart = context;
                                s.start_operation(
                                    Operation::Settings(
                                        Box::new(settings),
                                        state.currency.clone(),
                                        state.timezone.clone(),
                                    ),
                                    w,
                                    cx,
                                );
                            }
                        },
                    ))
                    .child(self.action(
                        "chart-zoom-in",
                        "放大",
                        data.is_some(),
                        cx,
                        |s, _, cx| {
                            s.chart.update(cx, |chart, cx| {
                                chart.zoom(1.);
                                cx.notify();
                            })
                        },
                    ))
                    .child(self.action(
                        "chart-zoom-out",
                        "缩小",
                        data.is_some(),
                        cx,
                        |s, _, cx| {
                            s.chart.update(cx, |chart, cx| {
                                chart.zoom(-1.);
                                cx.notify();
                            })
                        },
                    ))
                    .child(
                        self.action("chart-left", "向前", data.is_some(), cx, |s, _, cx| {
                            s.chart.update(cx, |chart, cx| {
                                chart.offset = chart.offset.saturating_sub(chart.visible / 2 + 1);
                                chart.constrain_view();
                                cx.notify();
                            })
                        }),
                    )
                    .child(
                        self.action("chart-right", "向后", data.is_some(), cx, |s, _, cx| {
                            s.chart.update(cx, |chart, cx| {
                                chart.offset = (chart.offset + chart.visible / 2 + 1)
                                    .min(chart.candles.len().saturating_sub(chart.visible));
                                cx.notify();
                            })
                        }),
                    ),
            )
            .child(label(
                data.map(|d| {
                    format!(
                        "{} · 来源 {} · 版本 {} · raw · 1d · MA20 · 已载入 {}/{} 根",
                        d.context.instrument,
                        d.source,
                        d.context.dataset_version.as_deref().unwrap_or("无"),
                        d.bars.len() - d.warmup,
                        d.total
                    )
                })
                .unwrap_or_else(|| "无行情。导入文件并加载所选标的；不会使用首页合成价格。".into()),
                12.,
                MUTED,
            ))
            .child(
                div()
                    .h(px(420.))
                    .child(interactive_chart(self.chart.clone())),
            )
            .child(label(self.chart.read(cx).hover_text.clone(), 12., INK))
            .child(label("图中标记 / 固定证据", 14., INK))
            .children(
                data.into_iter()
                    .flat_map(|d| d.markers.iter())
                    .take(100)
                    .map(|(date, id)| {
                        let target = id.clone();
                        self.action(
                            format!("marker-{id}"),
                            format!("{date} · {id}"),
                            self.ready(),
                            cx,
                            move |s, w, cx| {
                                s.start_operation(Operation::Evidence(target.clone()), w, cx)
                            },
                        )
                    }),
            )
    }

    pub(super) fn journal_page(&self, cx: &mut Context<Self>) -> Div {
        let query = self.field_value("note-query", cx).to_lowercase();
        self.form()
            .child(self.title("交易笔记"))
            .child(
                row()
                    .gap_2()
                    .child(self.action(
                        "new-note",
                        "新建",
                        !self.business.dirty && self.ready(),
                        cx,
                        |s, w, cx| {
                            s.business.note = None;
                            s.set_field("note-title", "", w, cx);
                            s.business.body.update(cx, |v, cx| v.set_value("", w, cx));
                            s.business.dirty = false;
                            cx.notify();
                        },
                    ))
                    .child(
                        self.action("save-note", "保存", self.ready(), cx, |s, w, cx| {
                            s.save_note(w, cx)
                        }),
                    )
                    .child(self.action(
                        "discard-note",
                        "放弃未保存编辑",
                        !self.business.busy,
                        cx,
                        |s, w, cx| {
                            let body = s
                                .business
                                .note
                                .as_ref()
                                .map(|n| n.body.clone())
                                .unwrap_or_default();
                            s.business.body.update(cx, |v, cx| v.set_value(body, w, cx));
                            let title = s
                                .business
                                .note
                                .as_ref()
                                .map(|n| n.title.clone())
                                .unwrap_or_default();
                            s.set_field("note-title", title, w, cx);
                            s.business.dirty = false;
                            cx.notify();
                        },
                    ))
                    .child(label(
                        if self.business.dirty {
                            "未保存"
                        } else {
                            "编辑缓冲已同步"
                        },
                        12.,
                        if self.business.dirty { ORANGE } else { MUTED },
                    )),
            )
            .child(self.field("note-title", "标题"))
            .child(
                div()
                    .id("note-body")
                    .test_support()
                    .aria_label("笔记正文")
                    .on_click(cx.listener(|s, _, w, cx| {
                        s.business.body.update(cx, |input, cx| input.focus(w, cx))
                    }))
                    .child(
                        Textarea::new(&self.business.body)
                            .accessibility_id("note-body-input")
                            .h(px(240.))
                            .disabled(self.business.busy),
                    ),
            )
            .child(label(
                self.business
                    .note
                    .as_ref()
                    .map(|n| format!("账户 {} · 修订 {}", n.account_id, n.revision))
                    .unwrap_or_else(|| "新笔记归属当前所选第一个账户；保存当前图表窗口".into()),
                12.,
                MUTED,
            ))
            .child(
                row()
                    .gap_2()
                    .child(self.field("link-event", "关联成交 ID"))
                    .child(self.field("link-qty", "本笔记分配数量")),
            )
            .child(
                row()
                    .gap_2()
                    .child(
                        self.action(
                            "allocate-link",
                            "保存成交关联",
                            self.ready()
                                && self
                                    .business
                                    .note
                                    .as_ref()
                                    .and_then(|n| n.id.as_ref())
                                    .is_some(),
                            cx,
                            |s, w, cx| {
                                let note = s
                                    .business
                                    .note
                                    .as_ref()
                                    .and_then(|n| n.id.clone())
                                    .unwrap_or_default();
                                s.start_operation(
                                    Operation::Link(
                                        note,
                                        s.field_value("link-event", cx),
                                        s.field_value("link-qty", cx),
                                    ),
                                    w,
                                    cx,
                                );
                            },
                        ),
                    )
                    .child(
                        self.action(
                            "restore-note-chart",
                            "恢复笔记图表",
                            self.ready()
                                && self
                                    .business
                                    .note
                                    .as_ref()
                                    .and_then(|n| n.chart.as_ref())
                                    .is_some(),
                            cx,
                            |s, w, cx| {
                                if let Some(context) =
                                    s.business.note.as_ref().and_then(|n| n.chart.clone())
                                {
                                    s.set_chart_fields(&context, w, cx);
                                    s.page = Page::Chart;
                                    s.start_operation(Operation::Chart(context), w, cx);
                                }
                            },
                        ),
                    ),
            )
            .child(self.field("note-query", "搜索已载入笔记（最近 1000 条）"))
            .children(
                self.business
                    .state
                    .iter()
                    .flat_map(|s| s.notes.iter())
                    .filter(|(_, title, body)| {
                        format!("{title}{body}").to_lowercase().contains(&query)
                    })
                    .map(|(id, title, _)| {
                        let id = id.clone();
                        self.action(
                            format!("note-{id}"),
                            title.clone(),
                            self.ready() && !self.business.dirty,
                            cx,
                            move |s, w, cx| {
                                s.start_operation(Operation::OpenNote(id.clone(), None), w, cx)
                            },
                        )
                    }),
            )
    }

    pub(super) fn ai_page(&self, cx: &mut Context<Self>) -> Div {
        let accounts = self
            .business
            .state
            .as_ref()
            .map(|s| s.accounts.as_slice())
            .unwrap_or(&[]);
        let sessions = self
            .business
            .state
            .as_ref()
            .map(|s| s.sessions.as_slice())
            .unwrap_or(&[]);
        self.form()
            .child(self.title("只读 AI 复盘"))
            .child(self.scope_panel(cx))
            .child(label(
                "授权模型读取以下账户及其笔记（附件不发送）",
                13.,
                INK,
            ))
            .child(row().gap_2().flex_wrap().children(accounts.iter().map(|a| {
                let id = a.id.clone();
                let allowed = self.business.allowed.contains(&id);
                self.action(
                    format!("grant-{}", a.id),
                    format!(
                        "{} {}",
                        if allowed {
                            "✓ 已授权"
                        } else {
                            "○ 未授权"
                        },
                        a.name
                    ),
                    self.ready(),
                    cx,
                    move |s, w, cx| {
                        if !s.business.allowed.remove(&id) {
                            s.business.allowed.insert(id.clone());
                        }
                        s.save_settings(w, cx);
                    },
                )
            })))
            .child(label(
                self.business
                    .state
                    .as_ref()
                    .map(|s| {
                        format!(
                            "发送范围（已保存）：{} · {} ～ {} · {} · 模型 {}",
                            s.settings.selected_accounts.join(", "),
                            s.settings.start_at,
                            s.settings.end_at,
                            s.currency,
                            s.settings
                                .connection
                                .as_ref()
                                .map(|c| c.model_id.as_str())
                                .unwrap_or("未配置")
                        )
                    })
                    .unwrap_or_else(|| "未打开资料库".into()),
                12.,
                ORANGE,
            ))
            .child(self.field("prompt", "问题"))
            .child(
                row()
                    .gap_2()
                    .child(self.action(
                        "send-ai",
                        "按上述范围发送",
                        self.ready() && !self.business.ai_busy,
                        cx,
                        |s, w, cx| s.send_ai(w, cx),
                    ))
                    .child(self.action(
                        "cancel-ai",
                        "取消运行",
                        self.business.ai_busy,
                        cx,
                        |s, _, cx| {
                            s.business.ai_cancel.cancel();
                            s.status = "正在取消模型运行…".into();
                            cx.notify();
                        },
                    ))
                    .child(self.action(
                        "revoke-ai",
                        "撤销连接",
                        self.ready(),
                        cx,
                        |s, w, cx| {
                            s.business.grants.revoke_connection("desktop-primary");
                            s.business.enabled = false;
                            s.business.ai_generation += 1;
                            s.business.session = None;
                            s.business.ai_cancel.cancel();
                            s.start_operation(Operation::Revoke, w, cx);
                            cx.notify();
                        },
                    )),
            )
            .child(label(&self.business.transcript, 13., INK))
            .children(self.business.refs.iter().map(|id| {
                let id = id.clone();
                self.action(
                    format!("ai-evidence-{id}"),
                    format!("打开 {id}"),
                    self.ready(),
                    cx,
                    move |s, w, cx| s.start_operation(Operation::Evidence(id.clone()), w, cx),
                )
            }))
            .child(label(
                "历史会话（只读打开，发送时仍校验当前范围）",
                14.,
                INK,
            ))
            .children(sessions.iter().map(|(id, status)| {
                let id = id.clone();
                self.action(
                    format!("session-{id}"),
                    format!("{id} · {status}"),
                    self.ready(),
                    cx,
                    move |s, w, cx| s.start_operation(Operation::Transcript(id.clone()), w, cx),
                )
            }))
    }

    pub(super) fn recovery_page(&self, cx: &mut Context<Self>) -> Div {
        self.form().child(self.title("备份、导出与恢复"))
        .child(label("备份包含数据库、会话、固定证据和附件；系统凭据需在目标机器单独配置。恢复始终写入新目录。",12.,MUTED))
        .child(self.field("backup-path","新备份目录"))
        .child(self.action("backup","创建一致性备份",self.ready() && !self.business.ai_busy,cx,|s,w,cx|s.start_operation(Operation::Backup(PathBuf::from(s.field_value("backup-path",cx))),w,cx)))
        .child(self.field("export-path","空导出目录"))
        .child(self.action("export","导出可读数据",self.ready(),cx,|s,w,cx|s.start_operation(Operation::Export(PathBuf::from(s.field_value("export-path",cx))),w,cx)))
        .child(self.field("restore-source","来源备份目录"))
        .child(self.action("choose-backup","选择备份目录",true,cx,|s,w,cx|s.choose_path("restore-source",true,false,w,cx)))
        .child(self.field("restore-path","恢复目标文件（新目录）"))
        .child(self.action("restore","校验、恢复并打开",!self.business.busy && !self.business.ai_busy && !self.business.dirty,cx,|s,w,cx|{
            let source=PathBuf::from(s.field_value("restore-source",cx));let dest=PathBuf::from(s.field_value("restore-path",cx));
            let (epoch,serial,_)=s.business.gate.begin();s.business.busy=true;
            let background=cx.background_executor().spawn(async move{restore_backup(&source,&dest).map(|_|dest)});
            cx.spawn_in(w,async move|view,cx|{let result=background.await;let _=view.update_in(cx,|s,w,cx|{if !s.business.gate.accepts(epoch,serial){return;}s.business.busy=false;match result{Ok(path)=>s.open_library(Some(path),false,false,w,cx),Err(e)=>s.status=format!("恢复失败：{e}；当前资料库未切换")};cx.notify();});}).detach();
        }))
    }

    pub(super) fn evidence_panel(&self, cx: &mut Context<Self>) -> Div {
        let Some(e) = &self.business.evidence else {
            return div();
        };
        let id = e.target_id.clone();
        let revision = e.revision.as_ref().and_then(|r| r.parse().ok());
        card()
            .p_3()
            .gap_2()
            .child(
                row()
                    .gap_2()
                    .child(label(
                        format!(
                            "证据 {} · 修订 {}",
                            e.evidence_id,
                            e.revision.as_deref().unwrap_or("固定")
                        ),
                        13.,
                        ORANGE,
                    ))
                    .child(
                        self.action("close-evidence", "关闭证据", true, cx, |s, _, cx| {
                            s.business.evidence = None;
                            cx.notify();
                        }),
                    ),
            )
            .child(label(&e.body, 12., INK))
            .child(self.action(
                "open-evidence-note",
                "打开此笔记修订与图表",
                self.ready() && e.target_type == "journal" && !self.business.dirty,
                cx,
                move |s, w, cx| {
                    s.page = Page::Journal;
                    s.start_operation(Operation::OpenNote(id.clone(), revision), w, cx);
                },
            ))
    }
}
