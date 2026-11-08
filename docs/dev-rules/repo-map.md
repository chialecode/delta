# 仓库地图

当前实际目录由 [执行状态](../delivery/status.md) 和现场文件共同确定。项目已按 [D-20](../decisions/confirmed-decisions.md) 从文档治理基线重建，已完成并集成 M1 v2.6 纯 Rust 数据底座、账本核心与 CLI 实现（见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)），当前为 M2（[计划](../delivery/stage-plan.md)）。

| 已有路径 | 责任 |
| --- | --- |
| 根 Markdown（`README.md`、`AGENTS.md`、`USER-ACTIONS.md` 等） | 产品入口、开发/审核/安全/设计入口与用户待办唯一入口 |
| `docs/requirements` | FR/NFR 需求正本 |
| `docs/design` | 系统架构、自动化数据平面、数据模型、金融规则、交互、AI 与整合契约 |
| `docs/engineering` | 技术栈重评、MVP 路线和 AC 定义 |
| `docs/governance` | 文档权威与登记（`document-registry.json`） |
| `docs/dev-rules`、`docs/design-rules`、`docs/product-rules` | 工程、UI、产品与本地参考约束 |
| `docs/decisions` | 已确认决定（D-01～D-30）、ADR-0001～0003、工程事项台账 |
| `docs/delivery`、`docs/evidence`、`docs/templates` | 当前计划（`stage-plan.md`）、状态（`status.md`）、需求追踪、调研证据与模板 |
| `scripts`、`.github` | 文档门禁（`check_docs.py`）、构建期逐包许可收集与回归（`collect_third_party_licenses.py`、`test_collect_third_party_licenses.py`）、CI/PR/分支保护配置 |

## M1～M2 实现结构与依赖边界（已建立）

| 路径 | 所有权和依赖约束 |
| --- | --- |
| `crates/delta-core` | **M1–M2 已建立**：Decimal 领域模型、平衡分录、FIFO 成本、`LEDGER-01` 现金换汇（`cash_exchange`）、转账配对、公司行动与期间损益分解（`m2-ledger-1`）、纯 Rust 技术指标；零 UI/DB/网络依赖 |
| `crates/delta-data` | **M1–M2 已建立**：具名市场 Provider/capabilities、离线录制 `AccountProvider`、标的别名表、纯 Rust 日历、FX、多源核验、声明的账单模板、schema 4 SQLite（含账户/组合去重、手工估值、自选组、交易计划、图表上下文、Markdown 笔记修订/附件/FTS5 `trigram` 索引、AI 会话与原子压缩检查点）与封存版本查询；泛化 MarketDataProvider trait 与真实账户 API 仍待后续实现 |
| `crates/delta-model` | **M2 已建立**：自研窄 `ModelClient`、`openai-responses` 与 `openai-chat-completions` 双协议适配、`eventsource-stream 0.2.3` SSE 帧解析与受控回环测试服务（`ModelLoopbackServer`）；不访问业务库 |
| `crates/delta-app` | **M1–M2 已建立**：同步应用命令/查询（`AppService`）、单写者宿主线程（`AppHost`）、带范围查询契约（`AnalysisScope` / `ResultEnvelope` / `EvidenceRef` / `CapabilityCatalog`）、`KlineViewport` 纯 Rust 视口状态机、AI 复盘运行时与只读工具网关（`AgentRuntime` / `AnalysisToolGateway`）、S-10 静默进程助手与窗口哨兵、S-11 通道路径解析与 `delta.lock` 独占锁、S-04 Windows 凭据管理器封装、后台作业控制器（`JobController`）；`store()` / `store_mut()` 收为 crate 内部，对外不暴露 `tokio`/`reqwest` 类型 |
| `crates/delta-desktop` | **M2 已建立**（[M2 计划 S-12](../delivery/stage-plan.md#3-a-已定案的阶段技术决定b-不再重新选型)）：二进制 `delta`，`gpui-kit =0.9.0` 官方 `DockArea` / `DockLayout` / `DockSkin` / `Panel` / `DataTable` 工作台与自建 `MetricCard`、`StatusBanner`、`KlineChartView`、`i18n`；只依赖 `delta-app` 及其公开 DTO，不直接依赖 `delta-data`、`rusqlite`、`tokio`、`reqwest` |
| `crates/delta-cli` | **M1–M2 已建立**：二进制 `delta-cli`，纯 Rust 命令行与验证入口（`init`、`import`、`sync`、`reconcile`、`explain`、`quality`、`backup`、`restore`、`export`、`library delete`、`credential`、`package-install`、`package-uninstall`、`verify-stage --stage M1|M2`） |
| `Packager.toml`、`THIRD-PARTY-NOTICES.txt` | **M2 已建立**：Windows 按用户打包配置与包含 `MPL-2.0` 源码链接说明的第三方开源通知清单 |
| `tests/fixtures` | **M1 已建立**：美股/ETF 与加密现货录制报文、10 类标准与券商 CSV/Flex 样本、`1990`～`2027` 交易日历对照表与来源说明（`PROVENANCE.md`） |
| `.local`、`target`、`dist` | 忽略的隔离测试数据、历史备份 bundle 与构建产物 |

B 在实施阶段只创建当前阶段有实际消费者的 crate 与模块。`workers/agent/` 为工作区既有未跟踪目录，保持原状不纳入构建与提交。

当前核心分工：`delta-app/src/host.rs` 是 `AppHost` 与带范围查询契约，`ai.rs` 是只读工具网关与证据/数值核验，`kline.rs` 是 K 线视口状态机，`sync.rs` 是真实同步事务和 worker lease，`export.rs` 是开放格式一致性导出，`service.rs` 是导入/报告/对账/恢复/删除，`regression.rs` 是应用回归。桌面 `workbench.rs` 负责 Dock 视图与无头 UI 测试，`controller.rs` 负责串行后台操作与迟到结果处理，`ui_forms.rs` 负责输入/实际控件，`chart_canvas.rs` 负责视口批量绘制，`ui_verification.rs` 负责显式隐藏原生窗口与帧/首屏/交互测量及显式页面截图，`theme.rs` 负责主题与外壳尺寸。CLI 的 `packaging.rs` 构建、校验、安装和卸载真实两通道包，`verify.rs` / `verify_m2.rs` 保存门禁原始日志、命名测试映射和源码指纹。具体完成判断见 [阶段报告](../delivery/stage-report.md)。
