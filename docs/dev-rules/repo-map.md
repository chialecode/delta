# 仓库地图

当前实际目录由 [执行状态](../delivery/status.md)和现场文件共同确定。

| 已有路径 | 责任 |
| --- | --- |
| 根 Markdown | 产品入口、开发/审核/安全与设计入口 |
| `docs/requirements` | FR/NFR 需求正本 |
| `docs/design` | 架构、数据、金融、交互、AI 与整合契约 |
| `docs/engineering` | 技术选择、MVP 路线和 AC 定义 |
| `docs/governance` | 文档权威与登记 |
| `docs/dev-rules`、`docs/design-rules`、`docs/product-rules` | 工程、UI、产品约束 |
| `docs/decisions` | 确认、ADR、集中待处理 |
| `docs/delivery`、`docs/evidence`、`docs/templates` | 当前计划/状态、验证证据、模板 |
| `scripts`、`.github` | 文档自动检查与 CI/PR 模板 |

## 已有实现与约束

| 路径 | 所有权和依赖 |
| --- | --- |
| `crates/delta-core` | Decimal 领域模型、金融规则；无 UI/DB/网络依赖 |
| `crates/delta-app` | 命令/查询、权限、快照、任务与证据；依赖 core |
| `crates/delta-infra` | 存储、网络、凭据、worker 适配；实现 app 端口 |
| `apps/desktop` | GPUI 宿主、UI 组合根；不写金融公式 |
| `crates/delta-infra/src/model` | 自研窄 Rust 模型客户端；参考 Codex，不依赖整个上游工作区 |
| `crates/delta-app/src/ai` | 工具编排、上下文、预算、检查点与会话端口 |
| `workers/python` | 已有指标 JSONL worker，后续策略按权限另行复用 |
| `tests/fixtures` | 合成金融与协议固定样本、独立期望值 |
| `.local`、`target`、`dist` | 忽略的隔离测试数据与构建输出 |

B 可在同一职责下先使用模块再拆 crate，最终目录随实际工程回写；不为了目录表提前生成无消费者代码。公共协议先置于 app 的 contracts 模块，跨语言 schema 从同一正本导出或校验。

桌面现有 `pages.rs` 管理服务页与导航，`dashboard.rs` 为明确隔离的首页视觉样例，`chart.rs` 负责图表渲染/交互，`theme.rs` 集中视觉 token。`business.rs` 管理表单/视图与任务代次，`tasks.rs` 调用后台应用服务；`sqlite/workbench.rs` 提供桌面投影和持久设置。`verify_stage.py` / `package_stage.py` 读取当前用例映射，保存源码/产物指纹。`workers/agent/` 是已有未跟踪旧脚手架，未纳入构建。
