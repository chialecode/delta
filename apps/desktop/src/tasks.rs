//! Background application operations. The view submits owned inputs and only
//! accepts results for its current library epoch. No GPUI, SQL or money math.
use anyhow::{anyhow, Result};
use delta_app::{
    ai::{
        grants::Grants,
        runtime::{AgentRuntime, RunOutcome, RunRequest},
        session::{RunBudget, SessionStore},
    },
    contracts::CancelToken,
};
use delta_core::{
    pnl::PeriodPnl,
    valuation::{BalanceDiff, WealthView},
};
use delta_infra::{
    host::ProductionHost,
    model::{CredentialSource, DeltaModelClient},
    sqlite::{app::*, store::Library, workbench::*},
};
use std::{path::PathBuf, sync::Arc};

pub struct Snapshot {
    pub accounts: Vec<AccountView>,
    pub portfolios: Vec<PortfolioView>,
    pub settings: DesktopSettings,
    pub lines: Vec<AccountLine>,
    pub instruments: Vec<InstrumentHit>,
    pub watch: Vec<String>,
    pub notes: Vec<(String, String, String)>,
    pub events: Vec<LedgerRow>,
    pub wealth: Option<WealthView>,
    pub pnl: Option<PeriodPnl>,
    pub diffs: Vec<BalanceDiff>,
    pub sessions: Vec<(String, String)>,
    pub currency: String,
    pub timezone: String,
}

pub fn snapshot(library: &Library) -> Result<Snapshot> {
    let accounts = library.accounts()?;
    let mut settings = library.desktop_settings()?;
    if settings.start_at.is_empty() {
        settings.start_at = "2000-01-01T00:00:00Z".into();
    }
    if settings.end_at.is_empty() {
        settings.end_at = format!(
            "{}T00:00:00Z",
            chrono::Utc::now().date_naive() + chrono::Duration::days(1)
        );
    }
    let scope = if settings.selected_accounts.is_empty() {
        None
    } else {
        Some(library.desktop_scope(
            &settings.selected_accounts,
            &settings.start_at,
            &settings.end_at,
        )?)
    };
    let mut result = Snapshot {
        accounts,
        portfolios: library.portfolios()?,
        settings,
        lines: Vec::new(),
        instruments: library.search_instrument_hits("")?,
        watch: library.watchlist()?,
        notes: library.search_journal("", 1000, None)?,
        events: Vec::new(),
        wealth: None,
        pnl: None,
        diffs: Vec::new(),
        sessions: library.sessions()?,
        currency: library.reporting_currency()?,
        timezone: library.timezone()?,
    };
    if let Some(scope) = scope {
        result.lines = library
            .account_lines_at(Some(scope.end_at))?
            .into_iter()
            .filter(|l| scope.covers_account(&delta_core::AccountId::new(&l.account)))
            .collect();
        result.wealth = Some(library.wealth(&scope, scope.end_at)?);
        result.pnl = Some(library.explain(&scope)?);
        result.diffs = library.reconcile(&scope)?;
        result.events = library.ledger_rows(&scope)?;
    }
    Ok(result)
}

pub enum Operation {
    Refresh,
    Revoke,
    Watch(String, bool),
    Account(String, String),
    Portfolio(String, Vec<String>),
    Settings(Box<DesktopSettings>, String, String),
    Preview {
        path: PathBuf,
        account: String,
        columns: Vec<String>,
        delimiter: u8,
    },
    Commit {
        path: PathBuf,
        account: String,
        columns: Vec<String>,
        delimiter: u8,
        preview: CsvPreview,
        rows: Vec<i64>,
        suspects: Vec<i64>,
        independent: bool,
    },
    Correct(String, String),
    Observe(String, String, String, String),
    Market(PathBuf),
    Chart(ChartContext),
    SaveNote(JournalDraft),
    OpenNote(String, Option<i64>),
    Link(String, String, String),
    Session(String),
    Evidence(String),
    Backup(PathBuf),
    Export(PathBuf),
    Credential(String, String),
    Transcript(String),
}

pub enum Content {
    None,
    Permissions(Box<DesktopSettings>),
    Preview(CsvPreview, Vec<(i64, String, String)>),
    Chart(ChartWindow),
    Note(JournalDraft),
    Evidence(OpenedEvidence),
    Transcript(String, String, Vec<String>),
}
pub struct Update {
    pub message: String,
    pub snapshot: Option<Snapshot>,
    pub content: Content,
}

impl Operation {
    pub fn execute(self, library: Arc<Library>, cancel: CancelToken) -> Result<Update> {
        if cancel.is_cancelled() {
            return Err(anyhow!("已取消；未提交"));
        }
        let mut content = Content::None;
        let mut refresh = true;
        let message = match self {
            Self::Session(id) => {
                let mut s = library.desktop_settings()?;
                s.session_id = Some(id);
                library.save_desktop_settings(&s)?;
                "会话已保存".into()
            }
            Self::Revoke => {
                let mut settings = library.desktop_settings()?;
                settings.connection_enabled = false;
                settings.session_id = None;
                library.save_desktop_settings(&settings)?;
                content = Content::Permissions(Box::new(settings));
                "连接已撤销并保存；进行中的请求与压缩已终止".into()
            }
            Self::Refresh => "已刷新".into(),
            Self::Watch(id, on) => {
                if on {
                    library.add_watch(&id)?;
                } else {
                    library.remove_watch(&id)?;
                }
                "自选已保存".into()
            }
            Self::Portfolio(name, ids) => {
                library.save_portfolio(&name, &ids)?;
                "命名组合已保存；账户成员去重且不会增加模型授权".into()
            }
            Self::Account(name, kind) => {
                let id = library.create_account(&name, &kind)?;
                let mut s = library.desktop_settings()?;
                s.selected_accounts = vec![id];
                library.save_desktop_settings(&s)?;
                "账户已创建".into()
            }
            Self::Settings(s, currency, timezone) => {
                library.apply_desktop_settings(&s, &currency, &timezone)?;
                content = Content::Permissions(s);
                "设置已保存；尚未向模型发送数据".into()
            }
            Self::Preview {
                path,
                account,
                columns,
                delimiter,
            } => {
                let bytes = read_bounded(&path, 32 * 1024 * 1024)?;
                if cancel.is_cancelled() {
                    return Err(anyhow!("预览已取消"));
                }
                let preview = library.preview_mapped_csv(&account, &bytes, &columns, delimiter)?;
                let details = library.preview_details(&preview.batch_id)?;
                content = Content::Preview(preview, details);
                refresh = false;
                "预览就绪；请选择要入账的行，再确认提交".into()
            }
            Self::Commit {
                path,
                account,
                columns,
                delimiter,
                preview,
                rows,
                suspects,
                independent,
            } => {
                let bytes = read_bounded(&path, 32 * 1024 * 1024)?;
                library.verify_mapped_preview(&preview, &account, &bytes, &columns, delimiter)?;
                if cancel.is_cancelled() {
                    return Err(anyhow!("已取消；未提交"));
                }
                let receipt = if suspects.is_empty() {
                    library.commit_preview(
                        &preview.batch_id,
                        &preview.mapping_version,
                        &preview.file_hash,
                        &rows,
                    )?
                } else if independent {
                    library.accept_independent_sources(
                        &preview.batch_id,
                        &preview.mapping_version,
                        &preview.file_hash,
                        &rows,
                        &suspects,
                    )?
                } else {
                    library.skip_suspected_rows(
                        &preview.batch_id,
                        &preview.mapping_version,
                        &preview.file_hash,
                        &rows,
                        &suspects,
                    )?
                };
                // Never turn a committed receipt into cancellation after the fact.
                format!(
                    "已提交：新增 {}，跳过 {}，未入账 {}",
                    receipt.accepted, receipt.skipped, receipt.rejected
                )
            }
            Self::Correct(id, amount) => {
                library.correct_cash_amount(&id, &amount)?;
                "更正已保存；原始证据仍可打开".into()
            }
            Self::Observe(account, asset, quantity, at) => {
                library.observe_balance(&account, &asset, &quantity, &at)?;
                "来源余额已登记并核对".into()
            }
            Self::Market(path) => {
                let bytes = read_bounded(&path, 64 * 1024 * 1024)?;
                let file: MarketFile = serde_json::from_slice(&bytes)?;
                if cancel.is_cancelled() {
                    return Err(anyhow!("行情导入已取消"));
                }
                library.import_market_file(&file)?;
                format!(
                    "行情已导入：{} · {} · {} 根日线",
                    file.source,
                    file.dataset_version,
                    file.bars.len()
                )
            }
            Self::Chart(context) => {
                content = Content::Chart(library.chart_window(&context)?);
                refresh = false;
                "图表已加载；仅显示所选文件版本".into()
            }
            Self::SaveNote(draft) => {
                let saved = library.save_journal_draft(&draft)?;
                content = Content::Note(saved);
                "笔记已保存".into()
            }
            Self::OpenNote(id, rev) => {
                content = Content::Note(library.journal_draft(&id, rev)?);
                refresh = false;
                "已打开笔记修订".into()
            }
            Self::Link(note, event, quantity) => {
                let draft = library.journal_draft(&note, None)?;
                let events = library.recorded_events(Some(&[draft.account_id]))?;
                let trade = events
                    .iter()
                    .find(|e| e.id.0 == event)
                    .ok_or_else(|| anyhow!("成交不在笔记账户中"))?;
                let capacity = match &trade.payload {
                    delta_core::EventPayload::Buy { quantity, .. }
                    | delta_core::EventPayload::Sell { quantity, .. } => quantity.to_string(),
                    _ => return Err(anyhow!("只可关联买卖成交")),
                };
                library.allocate_link(&note, &event, &quantity, &capacity)?;
                "成交关联已保存".into()
            }
            Self::Evidence(id) => {
                content = Content::Evidence(library.open_evidence(&id)?);
                refresh = false;
                "已打开固定证据".into()
            }
            Self::Backup(path) => {
                let m = library.backup_to(&path)?;
                format!("备份完成，{} 个文件；凭据不在备份中", m.files.len())
            }
            Self::Export(path) => {
                if path.exists() && path.read_dir()?.next().is_some() {
                    return Err(anyhow!("请选择空导出目录"));
                }
                library.export_to(&path)?;
                "已导出账户 CSV、事件 JSON 和笔记 Markdown".into()
            }
            Self::Credential(reference, secret) => {
                store_os_credential(&reference, &secret).map_err(|e| anyhow!(e))?;
                refresh = false;
                "凭据已保存到系统凭据库".into()
            }
            Self::Transcript(id) => {
                let messages = library.messages(&id, None)?;
                let runs = library.runs(&id)?;
                let text = messages
                    .iter()
                    .map(|m| format!("{:?} / {:?}\n{}", m.kind, m.status, m.payload))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let mut refs = Vec::new();
                for message in &messages {
                    if let Some(ids) = message.payload["evidence_refs"].as_array() {
                        refs.extend(ids.iter().filter_map(|v| v.as_str().map(str::to_string)));
                    }
                }
                refs.sort();
                refs.dedup();
                content = Content::Transcript(id, text, refs);
                refresh = false;
                format!("只读历史 · {} 个运行；切换范围将开启新会话", runs.len())
            }
        };
        // A refresh failure must not hide a successful write and invite a retry.
        let (state, message) = if refresh {
            match snapshot(&library) {
                Ok(s) => (Some(s), message),
                Err(e) => (
                    None,
                    format!("{message}；刷新失败：{e}，请刷新查看已提交结果"),
                ),
            }
        } else {
            (None, message)
        };
        Ok(Update {
            message,
            snapshot: state,
            content,
        })
    }
}

pub fn read_bounded(path: &std::path::Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(anyhow!("文件超过大小限制"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(anyhow!("文件超过大小限制"));
    }
    Ok(bytes)
}

/// Invoked on the background executor, never from render/input handlers.
pub fn run_ai(
    library: Arc<Library>,
    settings: DesktopSettings,
    prompt: String,
    grants: Grants,
    cancel: CancelToken,
    credentials: Arc<dyn CredentialSource>,
) -> Result<RunOutcome> {
    if !settings.connection_enabled {
        return Err(anyhow!("模型连接未启用"));
    }
    let connection = settings
        .connection
        .clone()
        .ok_or_else(|| anyhow!("请先配置模型"))?;
    let scope = library.desktop_scope(
        &settings.selected_accounts,
        &settings.start_at,
        &settings.end_at,
    )?;
    let session = match settings.session_id {
        Some(id) => id,
        None => library.create_session(&library.id)?,
    };
    let client = DeltaModelClient::new(connection.clone(), credentials)
        .map_err(|e| anyhow!(e.safe_message))?;
    let store = Library::open(&library.path)?;
    let runtime = AgentRuntime::new(
        client,
        store,
        Arc::new(ProductionHost::new(library.clone())),
    )
    .with_grants(grants);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let out =
        rt.block_on(
            runtime.run_turn(RunRequest {
                session_id: session,
                user_prompt: prompt,
                scope_snapshot: scope,
                connection: connection.clone(),
                budget: RunBudget {
                    max_tool_calls: 12,
                    max_model_requests: 5,
                    context_window_tokens: connection.context_window_tokens,
                    max_duration_secs: 120,
                },
                system_instructions:
                    "只读投资复盘。所有数值必须来自工具；引用返回的证据 ID；明确缺失数据与局限。"
                        .into(),
                cancel,
            }),
        )?;
    // Do not resurrect old permissions/configuration when a run ends.
    Ok(out)
}

#[derive(Default)]
pub struct TaskGate {
    pub epoch: u64,
    pub serial: u64,
    pub cancel: CancelToken,
}
impl TaskGate {
    pub fn begin(&mut self) -> (u64, u64, CancelToken) {
        self.serial += 1;
        self.cancel = CancelToken::new();
        (self.epoch, self.serial, self.cancel.clone())
    }
    pub fn switch(&mut self) {
        self.cancel.cancel();
        self.epoch += 1;
        self.serial += 1;
    }
    pub fn accepts(&self, epoch: u64, serial: u64) -> bool {
        self.epoch == epoch && self.serial == serial
    }
}
