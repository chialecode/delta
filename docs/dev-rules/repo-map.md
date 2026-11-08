# 仓库地图

当前实际目录由 [执行状态](../delivery/status.md) 和现场文件共同确定。项目已按 [D-20](../decisions/confirmed-decisions.md) 从文档治理基线重建，当前已完成 M1 v2.6 纯 Rust 数据底座、账本核心与 CLI 实现（见 [M1 阶段报告](../delivery/stage-report.md)）。

| 已有路径 | 责任 |
| --- | --- |
| 根 Markdown（`README.md`、`AGENTS.md`、`USER-ACTIONS.md` 等） | 产品入口、开发/审核/安全/设计入口与用户待办唯一入口 |
| `docs/requirements` | FR/NFR 需求正本 |
| `docs/design` | 系统架构、自动化数据平面、数据模型、金融规则、交互、AI 与整合契约 |
| `docs/engineering` | 技术栈重评、MVP 路线和 AC 定义 |
| `docs/governance` | 文档权威与登记（`document-registry.json`） |
| `docs/dev-rules`、`docs/design-rules`、`docs/product-rules` | 工程、UI、产品与本地参考约束 |
| `docs/decisions` | 已确认决定（D-01～D-23）、ADR-0001～0003、工程事项台账 |
| `docs/delivery`、`docs/evidence`、`docs/templates` | 当前计划（`stage-plan.md`）、状态（`status.md`）、需求追踪、调研证据与模板 |
| `scripts`、`.github` | 文档门禁（`check_docs.py`）与 CI/PR/分支保护配置 |

## M1～M2 实现结构与依赖边界（M1 已建立，M2 桌面端待建）

| 路径 | 所有权和依赖约束 |
| --- | --- |
| `crates/delta-core` | **M1 已建立**：Decimal 领域模型、平衡分录、FIFO 成本、转账配对、公司行动与估值损益、纯 Rust 技术指标；零 UI/DB/网络依赖 |
| `crates/delta-data` | **M1 已建立**：具名市场 Provider/capabilities、离线录制 `AccountProvider`、标的别名表、纯 Rust 日历、FX、多源核验、声明的账单模板、schema 3 SQLite 与封存版本查询；泛化 MarketDataProvider trait 与真实账户 API 仍待后续实现 |
| `crates/delta-app` | **M1 已建立（M2 扩展 AI/UI 网关）**：同步应用命令/查询（`AppService`）、S-10 静默进程助手与窗口哨兵、S-11 通道路径解析与 `delta.lock` 独占锁、S-04 Windows 凭据管理器封装、后台作业控制器（`JobController`）；对外不暴露 `tokio`/`reqwest` 类型 |
| `crates/delta-cli` | **M1 已建立**：纯 Rust 命令行与验证入口（`init`、`import`、`sync`、`reconcile`、`explain`、`quality`、`backup`、`restore`、`export`、`library delete`、`credential`、`verify-stage`），用于无 GUI 环境下完整验证数据底座与账本（按 D-22 全项目纯 Rust，不建 `workers/python`） |
| `tests/fixtures` | **M1 已建立**：美股/ETF 与加密现货录制报文、10 类标准与券商 CSV/Flex 样本、`1990`～`2027` 交易日历对照表与来源说明（`PROVENANCE.md`） |
| `apps/desktop` | **M2 待建立**：GPUI 桌面工作台宿主（按 D-21 使用新版 `gpui-kit` 官方 `DockArea` / `Panel` / `DataTable` / `Card` 面板体系）；只调用 `delta-app` 同步命令/查询与作业句柄，不写金融公式或直接拼 SQL |
| `.local`、`target`、`dist` | 忽略的隔离测试数据、历史备份 bundle 与构建产物 |

B 在实施 M1 时只创建当前阶段有实际消费者的 crate 与模块，不提前生成空的 M2 桌面框架目录。`workers/agent/` 为工作区既有未跟踪目录，保持原状不纳入构建与提交。

当前核心分工：`delta-app/src/sync.rs` 是真实同步事务和 worker lease，`export.rs` 是一致性导出，`service.rs` 是导入/报告/对账/恢复/删除，`regression.rs` 是应用回归；`delta-cli/src/verify.rs` 保存真实门禁日志和源码指纹。具体完成判断见阶段报告，目录存在不表示完整 M2 需求实现。
