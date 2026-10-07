# DELTA 文档导航

基线：2026-09-26。文档约束服务于持续 MVP 开发；active 文档不代表产品已实现。实际交付只看 [当前状态](delivery/status.md)与对应证据。

## 最短阅读路径

首次接手：根 [AGENTS](../AGENTS.md) → [项目关键事实](project-baseline.md) → [当前状态](delivery/status.md) → 当前计划 → [集中事项](decisions/open-questions.md)。按变更触发读专题，不要求每次全量阅读。

当前轮次：[R1 唯一计划与 B prompt](delivery/stage-plan.md)、[当前报告](delivery/stage-report.md)、[用户待办](../USER-ACTIONS.md)。原 R1 v1.1 的计划/用例/审核作为本阶段基础范围、回归与未关闭验收依据保留；当前唯一执行入口为合并计划 R1 v1.2。

| 专题 | 正本 | 管理内容 |
| --- | --- | --- |
| 沟通/本地资料 | [沟通协议](dev-rules/communication-protocol.md)、[本地文件与参考](dev-rules/local-files-and-references.md)、[用户待办](../USER-ACTIONS.md) | 用户与 Agent 分开交接、忽略/参考边界 |
| 治理 | [政策](governance/documentation-policy.md)、[登记表](governance/document-registry.json) | 权威、元数据、更新/冲突/归档 |
| 产品 | [原则](product-rules/core-product-principles.md)、[需求](requirements/product-requirements.md) | 长期行为、FR/NFR、阶段范围 |
| 架构 | [系统](design/system-architecture.md)、[数据](design/data-model.md)、[金融](design/financial-engine.md) | 所有权、数值与恢复 |
| AI 整合 | [AI](design/ai-design.md)、[统一接口](design/integration-contracts.md)、[上下文状态](design/context-state-management.md)、[Rust 客户端](design/rust-model-client.md) | 模型协议、工具、scope、会话/压缩/恢复 |
| UI | [规则](design-rules/DESIGN.md)、[工作流](design/interaction-and-workflows.md) | 控件、状态、图表、用户操作 |
| 工程 | [仓库地图](dev-rules/repo-map.md)、[开源与架构](dev-rules/architecture-and-open-source.md)、[数据安全](dev-rules/data-and-security.md) | 代码位置、复用、数据和权限 |
| 协作 | [A/B](dev-rules/development-workflow.md)、[Git](dev-rules/git-workflow.md)、[门禁](dev-rules/quality-gates.md)、[审核](../REVIEW.md) | 长计划、连续执行、自检/审核、交接 |
| 技术 | [选型](engineering/technology-selection.md)、[调研证据](evidence/2026-09-26-technology-research.md) | 比较、维护依据、待测与替换 |
| 决定 | [索引](decisions/README.md)、[确认/假设](decisions/confirmed-decisions.md)、[集中台账](decisions/open-questions.md) | 不依赖聊天的决定与待办 |
| 交付 | [路线/AC](engineering/delivery-plan.md)、[逐需求追踪](delivery/traceability.json)、[当前状态](delivery/status.md) | MVP 循环、验收定义、实际执行 |
| 证据 | [证据索引](evidence/README.md) | 版本化观察和验证范围 |
| 模板 | [变更](templates/change-proposal.md)、[ADR](templates/decision-record.md)、[模块](templates/module-spec.md)、[验证](templates/validation-report.md)、[交接](templates/agent-handoff.md) | 按规模选用，不全套强制 |

全部自有 Markdown 及其 owner/scope/status/basis 以登记表为完整清单。新增规则补此索引和 AGENTS 触发入口，普通报告在证据索引/状态关联即可。

## 维护约定

保留稳定 FR/NFR/AC。需求、设计、验收和证据随行为变化同步；关键决定同轮回写。候选与正式依赖锁分开，模拟与真实验证分开。用户事项仅在 USER-ACTIONS.md 维护当前状态；工程未决/上游/后续事项在集中台账。

机械检查：`python scripts/check_docs.py`；不代表产品实现或人工验收通过。
