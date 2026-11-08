//! Localization catalog and market color convention (`S-24`, `M2-M-02`, `DESIGN.md`).
//!
//! Enforces:
//! - Chinese-first (`Locale::ZhCn` default) single key-value copy catalog with `Locale::EnUs` fallback.
//! - Scope honesty (`S-15` / `3.1`): navigation exposes only delivered M2 capabilities (never
//!   undelivered M3 training or M4 strategy entries).
//! - Market color convention (`RedUpGreenDown` vs `GreenUpRedDown`) always paired with explicit
//!   sign symbols (`+` / `-` / `0` / `—`) and direction words (`▲ 涨` / `▼ 跌` / `— 平`), and
//!   never fabricating `0` when a value is `None` (`AC-03`, `DESIGN.md`).

use delta_app::{to_canonical_text, QualityStatus};
use gpui_kit::{component::ActiveTheme as _, App, Hsla};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Supported UI locales (`S-24`: Chinese-first default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    #[default]
    ZhCn,
    EnUs,
}

impl Locale {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en_us" => Self::EnUs,
            _ => Self::ZhCn,
        }
    }
}

/// Primary left-sidebar navigation keys allowed in M2 (`S-15`, `3.1`).
/// Undelivered M3 training and M4 strategy entries are intentionally excluded.
pub const PRIMARY_NAV_KEYS: &[&str] = &[
    "nav.overview",
    "nav.market",
    "nav.assets",
    "nav.journal",
    "nav.data_center",
    "nav.settings",
];

/// Secondary dock panel keys (right AI review assistant and bottom background tasks).
pub const DOCK_PANEL_NAV_KEYS: &[&str] = &["nav.ai_panel", "nav.tasks"];

/// Returns `true` only if `key` belongs to the delivered M2 navigation scope (`S-15`).
pub fn is_in_scope_nav_key(key: &str) -> bool {
    PRIMARY_NAV_KEYS.contains(&key) || DOCK_PANEL_NAV_KEYS.contains(&key)
}

/// Single key-value copy catalog for the DELTA desktop workbench (`S-24`).
pub struct I18nCatalog;

const FORM_COPY: &[(&str, &str, &str)] = &[
    ("market.watchlist", "自选", "Watchlist"),
    ("market.symbols", "个标的", "symbols"),
    ("market.current", "当前标的", "Current"),
    ("market.search_results", "搜索结果", "Search results"),
    ("market.name_code", "名称/代码", "Name/Code"),
    ("market.close_of", "收盘", "Close of"),
    (
        "market.vs_previous",
        "较前一可见交易日",
        "vs previous visible session",
    ),
    ("market.no_bars", "暂无可见 K 线", "No visible bars"),
    ("market.open", "开盘", "Open"),
    ("market.high", "最高", "High"),
    ("market.low", "最低", "Low"),
    ("market.volume", "成交量", "Volume"),
    ("market.prev_close", "前收", "Prev. close"),
    ("market.bars", "K 线数", "Bars"),
    ("market.daily", "日 K", "Daily"),
    ("market.markers", "交易与笔记标记", "Trade & note markers"),
    ("overview.hero", "总净资产", "Net worth"),
    ("overview.assets_section", "资产概况", "Assets"),
    (
        "overview.breakdown_section",
        "期间损益分解",
        "Period breakdown",
    ),
    (
        "assets.lots_section",
        "持仓与 FIFO 批次",
        "Holdings & FIFO lots",
    ),
    ("assets.recon_section", "对账差异", "Reconciliation"),
    ("assets.flows_section", "流水明细", "Transactions"),
    ("assets.none", "暂无记录", "No records"),
    ("assets.account", "账户", "Account"),
    ("assets.asset", "标的", "Asset"),
    ("assets.quantity", "剩余数量", "Quantity"),
    ("assets.cost", "剩余成本", "Cost"),
    ("assets.expected", "账本", "Ledger"),
    ("assets.observed", "快照", "Snapshot"),
    ("assets.difference", "差异", "Difference"),
    ("ai.header", "AI 复盘", "AI review"),
    (
        "ai.offline_hint",
        "未配置模型时本地功能不受影响",
        "Local features work without a model",
    ),
    ("form.accounts", "账户与统计组合", "Accounts and portfolios"),
    ("form.account_id", "账户 ID", "Account ID"),
    ("form.account_name", "账户名称", "Account name"),
    (
        "form.account_kind",
        "类型（bank / broker / exchange / wallet / manual）",
        "Kind (bank / broker / exchange / wallet / manual)",
    ),
    ("form.platform", "平台", "Platform"),
    ("form.portfolio_id", "组合 ID", "Portfolio ID"),
    ("form.portfolio_name", "组合名称", "Portfolio name"),
    (
        "form.member_accounts",
        "成员账户（逗号分隔）",
        "Member accounts (comma separated)",
    ),
    (
        "form.member_portfolios",
        "子组合（逗号分隔）",
        "Member portfolios (comma separated)",
    ),
    ("form.import", "账单导入", "Statement import"),
    ("form.import_account", "导入到账户", "Import account"),
    (
        "form.import_preset",
        "账单格式（generic / futu / okx / flex 等）",
        "Preset (generic / futu / okx / flex etc.)",
    ),
    ("form.import_path", "账单文件路径", "Statement file path"),
    ("form.sync", "公开行情与汇率同步", "Market data and FX sync"),
    (
        "form.sync_symbols",
        "标的 ID（逗号分隔；留空使用订阅）",
        "Instrument IDs (empty uses subscriptions)",
    ),
    ("form.sync_start", "开始日期", "Start date"),
    ("form.sync_end", "结束日期（不含）", "End date (exclusive)"),
    (
        "form.manual_valuation",
        "手工资产与负债估值",
        "Manual assets and liabilities",
    ),
    ("form.asset_id", "资产或负债名称", "Asset or liability"),
    ("form.value", "估值金额", "Value"),
    ("form.currency", "币种", "Currency"),
    ("form.valuation_date", "估值日期", "Valuation date"),
    ("form.valuation_method", "估值方式", "Valuation method"),
    (
        "form.balance_reconciliation",
        "余额快照核对",
        "Balance reconciliation",
    ),
    ("form.balance_account", "核对账户", "Account to reconcile"),
    (
        "form.balance_asset",
        "标的或币种 ID",
        "Instrument or currency ID",
    ),
    (
        "form.balance_qty",
        "实测余额或数量",
        "Observed balance or quantity",
    ),
    (
        "form.balance_time",
        "快照时间（含时区）",
        "Snapshot timestamp (with timezone)",
    ),
    ("form.amend_qty", "更正数量", "Corrected quantity"),
    (
        "form.amend_price",
        "更正价格（可留空）",
        "Corrected price (optional)",
    ),
    ("form.amend_fee", "更正费用", "Corrected fee"),
    (
        "form.transaction_search",
        "流水标的筛选（留空清除）",
        "Filter instrument (empty clears)",
    ),
    (
        "form.instrument_search",
        "搜索标的、场所或别名",
        "Search instruments, venues or aliases",
    ),
    ("form.jump_date", "跳到日期", "Jump to date"),
    ("form.watchlist_name", "自选组名称", "Watchlist name"),
    ("form.indicators", "指标参数", "Indicator parameters"),
    ("form.note_title", "笔记标题", "Note title"),
    ("form.review_date", "复盘日期", "Review date"),
    (
        "form.note_tags",
        "标签（逗号分隔）",
        "Tags (comma separated)",
    ),
    ("form.note_body", "Markdown 笔记正文", "Markdown note body"),
    ("form.notes_search", "搜索日志", "Search notes"),
    ("form.thesis", "判断与依据", "Thesis"),
    ("form.invalidation", "失效条件", "Invalidation"),
    ("form.planned_risk", "计划风险", "Planned risk"),
    ("form.execution", "实际执行", "Actual execution"),
    ("form.emotion", "情绪", "Emotion"),
    ("form.conclusion", "复盘结论", "Conclusion"),
    ("form.markdown_preview", "Markdown 预览", "Markdown preview"),
    ("form.fill_event", "关联成交 ID", "Linked fill ID"),
    ("form.fill_qty", "分配成交数量", "Allocated quantity"),
    (
        "form.attachment_path",
        "附件文件路径（上限 16 MiB）",
        "Attachment path (up to 16 MiB)",
    ),
    ("form.report_currency", "报表币种", "Report currency"),
    (
        "form.timezone",
        "显示时区（IANA）",
        "Display timezone (IANA)",
    ),
    ("form.data_directory", "资料库目录", "Library directory"),
    ("form.model_connection", "模型连接", "Model connection"),
    ("form.model_protocol", "服务协议", "Service protocol"),
    ("form.base_url", "服务地址", "Base URL"),
    ("form.model_id", "模型 ID", "Model ID"),
    (
        "form.api_key",
        "API key（留空保留已存凭据）",
        "API key (empty keeps stored credential)",
    ),
    (
        "form.context_window",
        "上下文窗口（token）",
        "Context window (tokens)",
    ),
    (
        "form.output_tokens",
        "输出预算（token）",
        "Output budget (tokens)",
    ),
    ("form.run_timeout", "运行上限（秒）", "Run limit (seconds)"),
    (
        "form.library_operations",
        "备份、导出与恢复",
        "Backup, export and restore",
    ),
    ("form.backup_path", "新备份文件路径", "New backup path"),
    ("form.export_path", "新导出目录", "New export directory"),
    ("form.restore_backup", "待恢复备份文件", "Backup to restore"),
    (
        "form.restore_target",
        "恢复到空目录",
        "Restore to an empty directory",
    ),
    ("form.ai_question", "输入复盘问题", "Ask a review question"),
    (
        "form.ai_stream_unverified",
        "生成中 · 文本尚未核验",
        "Generating · text unverified",
    ),
    (
        "form.scope_target",
        "范围（all / 账户 ID / portfolio:组合 ID）",
        "Scope (all / account IDs / portfolio:ID)",
    ),
    (
        "form.scope_start",
        "期间开始（可留空）",
        "Period start (optional)",
    ),
    ("form.scope_end", "截止日期", "As-of date"),
    ("action.save_account", "保存账户", "Save account"),
    (
        "action.load_example",
        "载入离线示例",
        "Load offline example",
    ),
    ("action.save_portfolio", "保存统计组合", "Save portfolio"),
    ("action.preview_import", "预览账单", "Preview statement"),
    ("action.commit_import", "确认导入", "Commit import"),
    (
        "action.include_held",
        "确认独立成交",
        "Include distinct fill",
    ),
    ("action.exclude_held", "排除重复", "Exclude duplicate"),
    ("action.sync", "同步订阅", "Sync subscriptions"),
    ("action.note_pre", "交易前笔记", "Pre-trade note"),
    ("action.note_during", "交易中笔记", "During-trade note"),
    ("action.note_post", "交易后笔记", "Post-trade note"),
    (
        "action.allow_model",
        "允许模型读取此笔记",
        "Allow model access",
    ),
    (
        "action.disallow_model",
        "撤回模型访问",
        "Revoke model access",
    ),
    ("action.link_fill", "关联成交", "Link fill"),
    ("action.link_chart", "关联当前图表", "Link current chart"),
    ("action.attach", "添加附件", "Add attachment"),
    ("action.search", "搜索", "Search"),
    ("action.jump_date", "跳转", "Jump"),
    ("action.zoom_in", "放大", "Zoom in"),
    ("action.zoom_out", "缩小", "Zoom out"),
    ("action.pan_left", "向前平移", "Pan left"),
    ("action.pan_right", "向后平移", "Pan right"),
    ("action.adjustment", "切换复权", "Change adjustment"),
    ("action.add_watchlist", "加入自选组", "Add to watchlist"),
    ("action.apply_indicators", "应用指标", "Apply indicators"),
    ("action.previous_page", "上一页流水", "Previous page"),
    ("action.next_page", "下一页流水", "Next page"),
    ("action.save_asset", "保存资产估值", "Save asset valuation"),
    (
        "action.save_liability",
        "保存负债估值",
        "Save liability valuation",
    ),
    (
        "action.reconcile",
        "记录并核对快照",
        "Record and reconcile snapshot",
    ),
    (
        "action.amend",
        "保存更正（原始行保留）",
        "Save correction (raw row retained)",
    ),
    ("action.save_settings", "保存设置", "Save settings"),
    (
        "action.change_colors",
        "切换涨跌配色",
        "Change market colors",
    ),
    (
        "action.save_connection",
        "保存模型连接",
        "Save model connection",
    ),
    ("action.probe_model", "检查连通性", "Probe connection"),
    ("action.backup", "创建一致性备份", "Create backup"),
    ("action.export", "导出开放格式", "Export library"),
    ("action.restore", "恢复并校验", "Restore and verify"),
    ("action.ai_send", "发送复盘问题", "Send review question"),
    ("action.ai_cancel", "取消生成", "Cancel generation"),
    ("action.compact", "压缩较早回合", "Compact earlier turns"),
    ("action.apply_scope", "应用范围", "Apply scope"),
    (
        "form.delete_confirm",
        "请输入预览中的资料库编号确认删除",
        "Enter the previewed library ID to confirm deletion",
    ),
    ("action.delete_preview", "预览删除范围", "Preview deletion"),
    (
        "action.delete_confirm",
        "删除资料库并退出",
        "Delete library and quit",
    ),
    ("action.pin_scope", "固定本次范围", "Pin this scope"),
    ("action.unpin_scope", "跟随页面范围", "Follow page scope"),
    (
        "action.include_notes",
        "纳入已授权笔记",
        "Include permitted notes",
    ),
    ("action.exclude_notes", "不纳入笔记", "Exclude notes"),
    ("ai.evidence", "引用证据", "Cited evidence"),
    ("ai.saved_reports", "已保存的复盘", "Saved reviews"),
    ("ai.verified", "已核验", "Verified"),
    ("ai.needs_review", "需复核", "Needs review"),
];

impl I18nCatalog {
    /// Lookup localized UI copy by `locale` and `key`.
    pub fn t(locale: Locale, key: &str) -> &'static str {
        if let Some((_, zh, en)) = FORM_COPY.iter().find(|(id, _, _)| *id == key) {
            return match locale {
                Locale::ZhCn => zh,
                Locale::EnUs => en,
            };
        }
        match locale {
            Locale::ZhCn => Self::zh_cn(key),
            Locale::EnUs => Self::en_us(key),
        }
    }

    /// Convenience lookup using the default Chinese-first locale (`Locale::ZhCn`).
    pub fn tr(key: &str) -> &'static str {
        Self::t(Locale::ZhCn, key)
    }

    /// Localized label for a [`QualityStatus`] badge.
    pub fn quality_label(locale: Locale, status: QualityStatus) -> &'static str {
        match status {
            QualityStatus::Exact => Self::t(locale, "quality.exact"),
            QualityStatus::CarriedForward => Self::t(locale, "quality.carried_forward"),
            QualityStatus::Partial => Self::t(locale, "quality.partial"),
            QualityStatus::Warning => Self::t(locale, "quality.warning"),
            QualityStatus::Unavailable => Self::t(locale, "quality.unavailable"),
        }
    }

    fn zh_cn(key: &str) -> &'static str {
        match key {
            // Navigation (S-15: only delivered M2 capabilities; no training or strategy)
            "nav.overview" => "总览",
            "nav.market" => "市场",
            "nav.assets" => "资产",
            "nav.journal" => "日志",
            "nav.data_center" => "数据中心",
            "nav.settings" => "设置",
            "nav.ai_panel" => "AI 复盘助手",
            "nav.tasks" => "后台任务",

            // Overview & Breakdown MetricCards (S-15, M2-W04)
            "card.cash" => "现金余额",
            "card.nav" => "总净资产 (NAV)",
            "card.total_pnl" => "区间总损益",
            "card.net_invested" => "累计净投入",
            "card.data_coverage" => "数据覆盖与质量",
            "card.reconcile_pending" => "待核对事项",
            "card.realized_pnl" => "已实现损益",
            "card.unrealized_pnl" => "未实现损益",
            "card.dividend_interest" => "分红与利息",
            "card.fee_total" => "交易与资金费用",
            "card.fx_impact" => "汇率影响",
            "card.unattributed" => "未归因项",
            "card.max_drawdown" => "区间最大回撤",
            "card.unavailable" => "— (缺失/不可用)",
            "card.drilldown_flows" => "查看明细流水",

            // Quality statuses (DESIGN.md)
            "quality.exact" => "完整 (Exact)",
            "quality.carried_forward" => "汇率顺延 (Carried Forward)",
            "quality.partial" => "已知部分估值 (Partial)",
            "quality.warning" => "需关注 (Warning)",
            "quality.unavailable" => "不可用 (Unavailable)",

            // Journal autosave statuses (S-18)
            "autosave.editing" => "编辑中",
            "autosave.saving" => "正在保存...",
            "autosave.saved" => "已保存",
            "autosave.failed" => "保存失败（可重试）",

            // Honest system states (DESIGN.md required states)
            "state.empty_library.title" => "空数据目录（尚未建库）",
            "state.empty_library" => {
                "当前数据目录尚未建立账本或导入交易记录。请通过首次使用向导创建账户、导入账单或载入离线示例库。"
            }
            "state.syncing.title" => "数据同步进行中",
            "state.syncing" => {
                "正在后台同步行情与汇率数据，已完成批次将在校验通过后推进水位线。"
            }
            "state.partial_valuation.title" => "已知部分估值（Partial）",
            "state.partial_valuation" => {
                "当前展示为已知部分估值：存在缺失行情、汇率或未知成本批次，受影响指标标为不可用，绝不按 0 计入。"
            }
            "state.offline.title" => "离线模式",
            "state.offline" => {
                "当前处于离线或网络不可达状态，本地已缓存账本、历史行情与笔记可正常复盘。"
            }
            "state.auth_or_rate_limit.title" => "鉴权失败或触发限流",
            "state.auth_or_rate_limit" => {
                "数据源或模型服务返回鉴权失败或触发限流（401/403/429），请核对凭据或稍后重试。"
            }
            "state.save_failed.title" => "自动保存失败",
            "state.save_failed" => {
                "保存失败：本地编辑缓冲已完整保留，未覆盖已持久化修订，请检查磁盘权限后点击重试。"
            }
            "state.cancelled.title" => "操作已取消",
            "state.cancelled" => {
                "操作已取消：未提交的中间状态已安全回滚，水位线与既有版本保持不变。"
            }
            "state.stale_report.title" => "报告已过期（底层数据已更正）",
            "state.stale_report" => {
                "该复盘报告基于旧版账本或行情修订生成，底层记录已发生更正，请重新计算以获取最新核验结果。"
            }
            "state.restore_failed.title" => "备份恢复失败",
            "state.restore_failed" => {
                "备份恢复校验未通过：已中止切换并保留原库或自动迁移前备份，请检查备份包完整性。"
            }
            "state.layout_fallback.title" => "已恢复默认工作台布局",
            "state.layout_fallback" => {
                "布局配置文件版本不兼容或已损坏，已安全回退至默认工作台布局。"
            }
            "state.model_unavailable.title" => "AI 模型未连接（本地功能全部可用）",
            "state.model_unavailable" => {
                "未配置 AI 模型连接或当前模型端点不可达；全部本地总览、资产、K 线与交易日志功能均可正常使用。"
            }

            // Next-action button labels for StatusBanner
            "action.open_wizard" => "打开首次使用向导",
            "action.cancel_sync" => "取消同步",
            "action.inspect_gaps" => "查看缺失项明细",
            "action.retry_sync" => "重试连接",
            "action.configure_credentials" => "检查凭据与限流设置",
            "action.retry_save" => "立即重试保存",
            "action.restart_task" => "重新发起任务",
            "action.recompute_report" => "重新计算报告",
            "action.restore_backup" => "重新选择备份包",
            "action.reset_layout" => "保存默认布局",
            "action.configure_model" => "配置模型连接",

            // Market & K-Line labels (S-16, M2-W05)
            "kline.adj.raw" => "不复权 (raw)",
            "kline.adj.split" => "前复权/拆股复权 (split_adjusted)",
            "kline.adj.total_return" => "全收益复权 (total_return_adjusted)",
            "kline.incomplete_tail" => "未收盘 K 线已过滤 (Incomplete Tail)",
            "kline.stale" => "数据已过期 (Stale)",
            "kline.warmup" => "预热期 (Warm-up)",
            "kline.gap" => "GAP (休市/缺口)",
            "kline.version_drift" => "固定数据版本与最新库存在差异，可切换对比",
            "kline.marker.buy" => "B 买入",
            "kline.marker.sell" => "S 卖出",
            "kline.marker.note" => "N 笔记",
            "kline.marker.plan" => "P 计划",

            // Market color conventions
            "color.red_up_green_down" => "红涨绿跌（A 股/港股习惯）",
            "color.green_up_red_down" => "绿涨红跌（美股/国际习惯）",

            _ => "未定义文案",
        }
    }

    fn en_us(key: &str) -> &'static str {
        match key {
            // Navigation
            "nav.overview" => "Overview",
            "nav.market" => "Market",
            "nav.assets" => "Assets",
            "nav.journal" => "Journal",
            "nav.data_center" => "Data Center",
            "nav.settings" => "Settings",
            "nav.ai_panel" => "AI Review Assistant",
            "nav.tasks" => "Background Tasks",

            // Overview & Breakdown MetricCards
            "card.cash" => "Cash Balance",
            "card.nav" => "Net Asset Value (NAV)",
            "card.total_pnl" => "Period Total P&L",
            "card.net_invested" => "Cumulative Net Invested",
            "card.data_coverage" => "Data Coverage & Quality",
            "card.reconcile_pending" => "Pending Reconciliation",
            "card.realized_pnl" => "Realized P&L",
            "card.unrealized_pnl" => "Unrealized P&L",
            "card.dividend_interest" => "Dividends & Interest",
            "card.fee_total" => "Trading & Funding Fees",
            "card.fx_impact" => "FX Impact",
            "card.unattributed" => "Unattributed Items",
            "card.max_drawdown" => "Max Drawdown",
            "card.unavailable" => "— (Missing / Unavailable)",
            "card.drilldown_flows" => "Inspect Flows",

            // Quality statuses
            "quality.exact" => "Exact",
            "quality.carried_forward" => "Carried Forward",
            "quality.partial" => "Partial Valuation",
            "quality.warning" => "Warning",
            "quality.unavailable" => "Unavailable",

            // Journal autosave statuses
            "autosave.editing" => "Editing",
            "autosave.saving" => "Saving...",
            "autosave.saved" => "Saved",
            "autosave.failed" => "Save failed (Retry available)",

            // Honest system states
            "state.empty_library.title" => "Empty Library",
            "state.empty_library" => {
                "No ledger or transactions found in this data directory. Use the first-run wizard to create an account, import statements, or load the offline sample library."
            }
            "state.syncing.title" => "Data Sync in Progress",
            "state.syncing" => {
                "Synchronizing market bars and FX rates in the background. Watermarks advance only after batch verification."
            }
            "state.partial_valuation.title" => "Partial Valuation",
            "state.partial_valuation" => {
                "Showing known-part valuation: missing prices, FX rates, or unknown cost lots exist. Affected metrics are marked Unavailable and never counted as 0."
            }
            "state.offline.title" => "Offline Mode",
            "state.offline" => {
                "Network is offline or unreachable. Cached ledger, historical bars, and journal notes remain fully available."
            }
            "state.auth_or_rate_limit.title" => "Auth Failed or Rate Limited",
            "state.auth_or_rate_limit" => {
                "Provider or model endpoint returned 401/403/429. Check credentials in keyring or retry later."
            }
            "state.save_failed.title" => "Autosave Failed",
            "state.save_failed" => {
                "Save failed: local edit buffer is preserved without overwriting the last persisted revision. Retry when storage is writable."
            }
            "state.cancelled.title" => "Operation Cancelled",
            "state.cancelled" => {
                "Operation cancelled: uncommitted state was rolled back and watermarks remain unchanged."
            }
            "state.stale_report.title" => "Stale Report (Underlying Data Amended)",
            "state.stale_report" => {
                "This report was generated from an older ledger or dataset revision. Recompute to view updated results."
            }
            "state.restore_failed.title" => "Backup Restore Failed",
            "state.restore_failed" => {
                "Backup verification failed: restore aborted and existing library preserved. Inspect backup archive integrity."
            }
            "state.layout_fallback.title" => "Default Layout Restored",
            "state.layout_fallback" => {
                "Workbench layout file was incompatible or corrupted; reverted safely to the default dock layout."
            }
            "state.model_unavailable.title" => "AI Model Unavailable (Local Features Ready)",
            "state.model_unavailable" => {
                "No AI model connection is configured or reachable. All local overview, assets, K-line, and journal features work normally."
            }

            // Next-action button labels
            "action.open_wizard" => "Open First-Run Wizard",
            "action.cancel_sync" => "Cancel Sync",
            "action.inspect_gaps" => "Inspect Missing Inputs",
            "action.retry_sync" => "Retry Connection",
            "action.configure_credentials" => "Check Credentials & Limits",
            "action.retry_save" => "Retry Save Now",
            "action.restart_task" => "Restart Task",
            "action.recompute_report" => "Recompute Report",
            "action.restore_backup" => "Select Backup Archive",
            "action.reset_layout" => "Save Default Layout",
            "action.configure_model" => "Configure Model",

            // Market & K-Line labels
            "kline.adj.raw" => "Raw (raw)",
            "kline.adj.split" => "Split Adjusted (split_adjusted)",
            "kline.adj.total_return" => "Total Return Adjusted (total_return_adjusted)",
            "kline.incomplete_tail" => "Unclosed Bar Filtered (Incomplete Tail)",
            "kline.stale" => "Stale Data (Stale)",
            "kline.warmup" => "Warm-up",
            "kline.gap" => "GAP (Session Gap)",
            "kline.version_drift" => "Pinned dataset version differs from latest library",
            "kline.marker.buy" => "B Buy",
            "kline.marker.sell" => "S Sell",
            "kline.marker.note" => "N Note",
            "kline.marker.plan" => "P Plan",

            // Market color conventions
            "color.red_up_green_down" => "Red Up / Green Down (CN/Asia)",
            "color.green_up_red_down" => "Green Up / Red Down (US/Intl)",

            _ => "Undefined Copy",
        }
    }
}

/// Top-level convenience lookup by `locale` and `key`.
pub fn t(locale: Locale, key: &str) -> &'static str {
    I18nCatalog::t(locale, key)
}

/// Top-level convenience lookup defaulting to `Locale::ZhCn`.
pub fn tr(key: &str) -> &'static str {
    I18nCatalog::tr(key)
}

/// Direction of a signed financial change (`S-24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDirection {
    Up,
    Down,
    Flat,
    Unavailable,
}

/// Formatted representation of a signed change with explicit sign symbol and direction word (`DESIGN.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDisplay {
    pub text: String,
    pub direction: ChangeDirection,
    pub sign_symbol: &'static str,
    pub direction_word: &'static str,
}

/// User-switchable market color convention (`S-24`, `M2-M-02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketColorConvention {
    /// Chinese/Asian convention: Red for positive/up (`+` / `▲ 涨`), Green for negative/down (`-` / `▼ 跌`).
    #[default]
    RedUpGreenDown,
    /// US/International convention: Green for positive/up (`+` / `▲ 涨`), Red for negative/down (`-` / `▼ 跌`).
    GreenUpRedDown,
}

impl MarketColorConvention {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RedUpGreenDown => "red_up_green_down",
            Self::GreenUpRedDown => "green_up_red_down",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "green_up_red_down" | "us" | "green_up" => Self::GreenUpRedDown,
            _ => Self::RedUpGreenDown,
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::RedUpGreenDown => Self::GreenUpRedDown,
            Self::GreenUpRedDown => Self::RedUpGreenDown,
        }
    }

    pub fn label(&self, locale: Locale) -> &'static str {
        match self {
            Self::RedUpGreenDown => I18nCatalog::t(locale, "color.red_up_green_down"),
            Self::GreenUpRedDown => I18nCatalog::t(locale, "color.green_up_red_down"),
        }
    }

    /// Formats a signed change with canonical Decimal text, currency, sign symbol, and direction word.
    ///
    /// **Critical boundary (`AC-03`, `DESIGN.md`)**: NEVER displays `0` when `value` is `None`.
    pub fn format_signed_change(&self, value: Option<Decimal>, currency: &str) -> SignedDisplay {
        let ccy = currency.trim();
        match value {
            None => SignedDisplay {
                text: "— (缺失/不可用)".to_string(),
                direction: ChangeDirection::Unavailable,
                sign_symbol: "—",
                direction_word: "缺失/不可用",
            },
            Some(v) if v > Decimal::ZERO => {
                let canonical = to_canonical_text(v);
                let text = if ccy.is_empty() {
                    format!("+{canonical} (▲ 涨)")
                } else {
                    format!("+{canonical} {ccy} (▲ 涨)")
                };
                SignedDisplay {
                    text,
                    direction: ChangeDirection::Up,
                    sign_symbol: "+",
                    direction_word: "▲ 涨",
                }
            }
            Some(v) if v < Decimal::ZERO => {
                let canonical = to_canonical_text(v);
                let text = if ccy.is_empty() {
                    format!("{canonical} (▼ 跌)")
                } else {
                    format!("{canonical} {ccy} (▼ 跌)")
                };
                SignedDisplay {
                    text,
                    direction: ChangeDirection::Down,
                    sign_symbol: "-",
                    direction_word: "▼ 跌",
                }
            }
            Some(_) => {
                let text = if ccy.is_empty() {
                    "0 (— 平)".to_string()
                } else {
                    format!("0 {ccy} (— 平)")
                };
                SignedDisplay {
                    text,
                    direction: ChangeDirection::Flat,
                    sign_symbol: "0",
                    direction_word: "— 平",
                }
            }
        }
    }

    /// Pure-Rust semantic token names `(foreground_token, subtle_background_token)` for deterministic testing.
    pub fn semantic_color_tokens(
        &self,
        direction: ChangeDirection,
    ) -> (&'static str, &'static str) {
        match (self, direction) {
            (Self::RedUpGreenDown, ChangeDirection::Up) => ("red", "red_light"),
            (Self::RedUpGreenDown, ChangeDirection::Down) => ("green", "green_light"),
            (Self::GreenUpRedDown, ChangeDirection::Up) => ("green", "green_light"),
            (Self::GreenUpRedDown, ChangeDirection::Down) => ("red", "red_light"),
            (_, ChangeDirection::Flat | ChangeDirection::Unavailable) => {
                ("muted_foreground", "muted")
            }
        }
    }

    /// Resolves `(foreground, subtle_background)` [`Hsla`] colors from `cx.theme()` according to the convention.
    pub fn colors(&self, direction: ChangeDirection, cx: &App) -> (Hsla, Hsla) {
        let theme = cx.theme();
        match (self, direction) {
            (Self::RedUpGreenDown, ChangeDirection::Up) => (theme.red, theme.red_light),
            (Self::RedUpGreenDown, ChangeDirection::Down) => (theme.green, theme.green_light),
            (Self::GreenUpRedDown, ChangeDirection::Up) => (theme.green, theme.green_light),
            (Self::GreenUpRedDown, ChangeDirection::Down) => (theme.red, theme.red_light),
            (_, ChangeDirection::Flat | ChangeDirection::Unavailable) => {
                (theme.muted_foreground, theme.muted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_s24_i18n_catalog_and_scope_honesty() {
        assert_eq!(Locale::default(), Locale::ZhCn);
        assert_eq!(tr("nav.overview"), "总览");
        assert_eq!(t(Locale::EnUs, "nav.overview"), "Overview");
        assert_eq!(tr("card.nav"), "总净资产 (NAV)");
        assert_eq!(tr("quality.partial"), "已知部分估值 (Partial)");
        assert_eq!(tr("autosave.failed"), "保存失败（可重试）");

        // Scope honesty (S-15 / 3.1): no training or strategy entry in M2 navigation
        for &key in PRIMARY_NAV_KEYS.iter().chain(DOCK_PANEL_NAV_KEYS.iter()) {
            assert!(is_in_scope_nav_key(key));
            assert_ne!(I18nCatalog::t(Locale::ZhCn, key), "未定义文案");
            assert_ne!(I18nCatalog::t(Locale::EnUs, key), "Undefined Copy");
        }
        assert!(!is_in_scope_nav_key("nav.training"));
        assert!(!is_in_scope_nav_key("nav.strategy"));
    }

    #[test]
    fn test_s24_market_color_convention_never_shows_zero_for_none() {
        let cn = MarketColorConvention::RedUpGreenDown;
        let us = MarketColorConvention::GreenUpRedDown;

        let missing = cn.format_signed_change(None, "USD");
        assert_eq!(missing.direction, ChangeDirection::Unavailable);
        assert_eq!(missing.text, "— (缺失/不可用)");
        assert!(!missing.text.contains('0'));

        let up = cn.format_signed_change(Some(Decimal::from_str("68.50").unwrap()), "USD");
        assert_eq!(up.direction, ChangeDirection::Up);
        assert_eq!(up.text, "+68.5 USD (▲ 涨)");
        assert_eq!(up.sign_symbol, "+");
        assert_eq!(up.direction_word, "▲ 涨");

        let down = us.format_signed_change(Some(Decimal::from_str("-12.25").unwrap()), "USD");
        assert_eq!(down.direction, ChangeDirection::Down);
        assert_eq!(down.text, "-12.25 USD (▼ 跌)");
        assert_eq!(down.sign_symbol, "-");
        assert_eq!(down.direction_word, "▼ 跌");

        let flat = cn.format_signed_change(Some(Decimal::ZERO), "USD");
        assert_eq!(flat.direction, ChangeDirection::Flat);
        assert_eq!(flat.text, "0 USD (— 平)");

        // Verify color convention inversion between RedUpGreenDown and GreenUpRedDown
        assert_eq!(
            cn.semantic_color_tokens(ChangeDirection::Up),
            ("red", "red_light")
        );
        assert_eq!(
            cn.semantic_color_tokens(ChangeDirection::Down),
            ("green", "green_light")
        );
        assert_eq!(
            us.semantic_color_tokens(ChangeDirection::Up),
            ("green", "green_light")
        );
        assert_eq!(
            us.semantic_color_tokens(ChangeDirection::Down),
            ("red", "red_light")
        );
        assert_eq!(
            cn.semantic_color_tokens(ChangeDirection::Unavailable),
            ("muted_foreground", "muted")
        );
    }
}
