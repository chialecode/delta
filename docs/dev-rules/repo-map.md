# 仓库地图

当前实际目录由 [执行状态](../delivery/status.md) 和现场文件共同确定。项目已按 [D-20](../decisions/confirmed-decisions.md) 从文档治理基线重建，已完成并集成 M1 v2.6 数据底座（[M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)）、M2 v1.0 桌面复盘工作台（[M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)）与 M3 v1.0 安装更新及 v0.1.1 发布（[M3 阶段报告](../evidence/2026-11-12-m3-stage-report.md)），当前为 M4 K 线训练（[计划](../delivery/stage-plan.md)、[M4 阶段报告](../delivery/stage-report.md)）。

| 已有路径 | 责任 |
| --- | --- |
| 根 Markdown（`README.md`、`AGENTS.md`、`USER-ACTIONS.md` 等） | 产品入口、开发/审核/安全/设计入口与用户待办唯一入口 |
| `docs/requirements` | FR/NFR 需求正本 |
| `docs/design` | 系统架构、自动化数据平面、数据模型、金融规则、交互、AI 与整合契约 |
| `docs/engineering` | 技术栈重评、MVP 路线和 AC 定义 |
| `docs/governance` | 文档权威与登记（`document-registry.json`） |
| `docs/dev-rules`、`docs/design-rules`、`docs/product-rules` | 工程、UI、产品与本地参考约束 |
| `docs/decisions` | 已确认决定（D-01～D-32）、ADR-0001～0003、工程事项台账 |
| `docs/delivery`、`docs/evidence`、`docs/templates` | 当前计划（`stage-plan.md`）、阶段报告（`stage-report.md`）、状态（`status.md`）、需求追踪、调研证据与模板 |
| `scripts`、`.github` | 文档门禁（`check_docs.py`）、构建期逐包许可收集与回归（`collect_third_party_licenses.py`、`test_collect_third_party_licenses.py`）、CI/PR/分支保护配置 |

## M1～M4 实现结构与依赖边界（已建立）

| 路径 | 所有权和依赖约束 |
| --- | --- |
| `crates/delta-core` | **M1–M4 已建立**：Decimal 领域模型、平衡分录、FIFO 成本、`LEDGER-01` 现金换汇（`cash_exchange`）、转账配对、公司行动与期间损益分解（`m2-ledger-1`）、纯 Rust 技术指标；M4 起新增 `sim.rs`（`m4-sim-1` 纯函数现货撮合、预留、可组合费用、`SHA-256` 确定性抽样与时点拆股复权）与 `sim_report.rs`（`m4-report-1` 训练报告、路径段 MAE/MFE、计划执行率与买入持有基准）；零 UI/DB/网络依赖 |
| `crates/delta-data` | **M1–M4 已建立**：具名市场 Provider/capabilities、离线录制 `AccountProvider`、标的别名表、纯 Rust 日历、FX、多源核验、声明的账单模板、schema 5 SQLite（含账户/组合去重、手工估值、自选组、交易计划、图表上下文、Markdown 笔记修订/附件/FTS5 `trigram` 索引、AI 会话与原子压缩检查点，以及 M4 `m4_schema.rs` 的 8 张模拟/训练表、不可变/单调触发器与 SQL 截断训练端口）与封存版本查询；泛化 MarketDataProvider trait 与真实账户 API 仍待后续实现 |
| `crates/delta-model` | **M2 已建立**：自研窄 `ModelClient`、`openai-responses` 与 `openai-chat-completions` 双协议适配、`eventsource-stream 0.2.3` SSE 帧解析与受控回环测试服务（`ModelLoopbackServer`）；不访问业务库 |
| `crates/delta-app` | **M1–M4 已建立**：同步应用命令/查询（`AppService`）、单写者宿主线程（`AppHost`）、带范围查询契约（`AnalysisScope` / `ResultEnvelope` / `EvidenceRef` / `CapabilityCatalog`）、`KlineViewport` 纯 Rust 视口状态机、AI 复盘运行时与只读工具网关（`AgentRuntime` / `AnalysisToolGateway`）、S-10 静默进程助手与窗口哨兵、S-11 通道路径解析与 `delta.lock` 独占锁、S-04 凭据存储（D-32 起为数据目录内 DPAPI 加密文件）、后台作业控制器（`JobController`）；M3 起有安装身份（`identity.rs`）、版本推算（`version.rs`）、单实例与本身份控制管道（`instance.rs`）、Ed25519 签名更新客户端（`update.rs`，内置公钥 `release-signing-key.json`）；M4 起有 `training.rs`（`TrainingDataPort`、训练生命周期/委托/笔记/重放/重试/删除预览编排）、`export.rs` 模拟导出与 `ScopeTarget::TrainingRun` 结束后只读工具集；`store()` / `store_mut()` 收为 crate 内部，对外不暴露 `tokio`/`reqwest` 类型 |
| `crates/delta-desktop` | **M2–M4 已建立**（M2 计划 S-12（计划已归档于 Git 历史，结果见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)））：二进制 `delta`，`gpui-kit =0.9.0` 官方 `DockArea` / `DockLayout` / `DockSkin` / `Panel` / `DataTable` 工作台与自建 `MetricCard`、`StatusBanner`、`KlineChartView`、`i18n`；M4 起 `training_ui.rs` 提供第七页“训练”工作区（`KlineViewport` 复用、快捷键防误触分离、结束→报告→显式“揭示”→AI 复盘）；只依赖 `delta-app` 及其公开 DTO，不直接依赖 `delta-data`、`rusqlite`、`tokio`、`reqwest`；M3 起 `lifecycle.rs` 负责控制命令桥接、保存后退出序列与更新状态，`assets/icon.svg` / `icon.ico`（正式版身份：深灰底 `#3C4043` 白色 “D”）与 `icon-dev.svg` / `icon-dev.ico`（开发版身份 `delta-dev`、`delta-dev-test`：同一图标加右下角琥珀色 “DEV” 角标，2026-11-14 用户要求）由 `scripts/generate_icon.py` 确定性生成，`build.rs` + `winresource` 按身份写入图标与版本资源，安装程序与“应用和功能”入口用同一图标 |
| `crates/delta-cli` | **M1–M4 已建立**：二进制 `delta-cli`，纯 Rust 命令行与验证入口（`init`、`import`、`sync`、`reconcile`、`explain`、`quality`、`backup`、`restore`、`export`、`library delete`、`credential`、`package-build`（Inno `setup.exe`）、`install-dev`、`release-build`、`release-keygen`、`control`、`training`（`training_cmd.rs`）、`verify-stage --stage M1\|M2\|M3\|M4`；测试与迁移诊断用 `package-portable-build`、`package-install`、`package-uninstall`）；`installer/delta.iss` 为 Inno Setup 7.1.x 模板 |
| `THIRD-PARTY-NOTICES.txt` | **M2 已建立**：包含 `MPL-2.0` 源码链接说明的第三方开源通知清单（M2 遗留的 cargo-packager 配置已在 M4-W07 退役删除） |
| `tests/fixtures` | **M1 已建立**：美股/ETF 与加密现货录制报文、10 类标准与券商 CSV/Flex 样本、`1990`～`2027` 交易日历对照表与来源说明（`PROVENANCE.md`） |
| `scripts/` | 标准库 Python 工具：`check_docs.py`（文档门禁）、`collect_third_party_licenses.py`（许可收集）、`generate_icon.py`（图标生成）、`clean_artifacts.py`（构建产物清理，见 [构建产物](local-files-and-references.md#build-artifacts)）、`quick_gate.py`（日常的快速门禁，见[两级门禁](quality-gates.md#gate-tiers)） |
| `.local`、`target`、`dist` | 忽略的隔离测试数据、历史备份 bundle 与构建产物；`dist/` 只放每个身份最新的安装包 |

B 在实施阶段只创建当前阶段有实际消费者的 crate 与模块。`workers/agent/` 为工作区既有未跟踪目录，保持原状不纳入构建与提交。

当前核心分工：`delta-core/src/sim.rs` 与 `sim_report.rs` 是纯 Decimal 模拟撮合与训练报告；`delta-data/src/m4_schema.rs` 是 Schema 5 模拟存储与 SQL 截断端口；`delta-app/src/host.rs` 是 `AppHost` 与带范围查询契约，`training.rs` 是训练服务与数据端口，`ai.rs` 是只读工具网关（含 `ScopeTarget::TrainingRun`）与证据/数值核验，`kline.rs` 是 K 线视口状态机，`sync.rs` 是真实同步事务和 worker lease，`export.rs` 是开放格式一致性导出，`service.rs` 是导入/报告/对账/恢复/删除，`regression.rs` 是应用回归。桌面 `workbench.rs` 与 `training_ui.rs` 负责 Dock 视图、训练工作区与无头 UI 测试，`controller.rs` 负责串行后台操作与迟到结果处理，`ui_forms.rs` 负责输入/实际控件，`chart_canvas.rs` 负责视口批量绘制，`ui_verification.rs` 负责显式隐藏原生窗口与帧/首屏/交互/M4“下一根”测量及显式页面截图，`theme.rs` 负责主题与外壳尺寸。CLI 的 `training_cmd.rs` 提供 `training` 命令集，`packaging.rs` 保留 M2 便携包（测试与迁移诊断），`installer.rs` 负责 Inno 构建、安装状态读取、`install-dev`、发布签名与 M2 便携安装迁移，`verify.rs` / `verify_m2.rs` / `verify_m3.rs` / `verify_m4.rs` 保存门禁原始日志、命名测试映射和源码指纹。各阶段完成判断见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)、[M3 阶段报告](../evidence/2026-11-12-m3-stage-report.md) 与 [M4 阶段报告](../delivery/stage-report.md)。
