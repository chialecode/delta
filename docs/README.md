# DELTA 文档导航

基线：2026-09-26；重建与数据优先核对：2026-11-07（[D-20](decisions/confirmed-decisions.md)）。文档约束服务于持续 MVP 开发；active 文档不代表产品已实现。实现状态以 `main` 上的代码、[逐需求追踪](delivery/traceability.json)与[证据](evidence/README.md)为准；进行中的工作在 [Issues](https://github.com/chialecode/delta/issues)，需要用户本人处理的事项是 [`needs-user` issue](https://github.com/chialecode/delta/issues?q=is%3Aissue+label%3Aneeds-user)。

## 最短阅读路径

首次接手：根 [AGENTS](../AGENTS.md) → [项目关键事实](project-baseline.md) → [开发流程](dev-rules/development-workflow.md) → 对应的 issue。按变更触发读专题，不要求每次全量阅读。

| 专题 | 正本 | 管理内容 |
| --- | --- | --- |
| 协作 | [开发流程](dev-rules/development-workflow.md)、[Git](dev-rules/git-workflow.md)、[门禁](dev-rules/quality-gates.md)、[审核](../REVIEW.md)、[教训](../LESSONS.md) | issue → 小 PR → CI 与独立审查 → 合并、找用户的时机、每周维护 |
| 本地资料 | [本地文件与参考](dev-rules/local-files-and-references.md) | 忽略目录、本机参考的边界 |
| 治理 | [政策](governance/documentation-policy.md)、[登记表](governance/document-registry.json) | 权威、元数据、更新/冲突/归档 |
| 产品 | [原则](product-rules/core-product-principles.md)、[需求](requirements/product-requirements.md) | 长期行为、FR/NFR、阶段范围 |
| 架构与数据底座 | [系统架构](design/system-architecture.md)、[数据模型](design/data-model.md)、[金融规则](design/financial-engine.md) | 自动化数据平面、SQLite 单库存储、数值与恢复 |
| AI 整合 | [AI](design/ai-design.md)、[统一接口](design/integration-contracts.md)、[上下文状态](design/context-state-management.md)、[Rust 客户端](design/rust-model-client.md) | 模型协议、工具、scope、会话/压缩/恢复 |
| UI | [规则](design-rules/DESIGN.md)、[工作流](design/interaction-and-workflows.md) | 控件、状态、图表、用户操作 |
| 工程 | [仓库地图](dev-rules/repo-map.md)、[开源与架构](dev-rules/architecture-and-open-source.md)、[数据安全](dev-rules/data-and-security.md) | 代码位置、复用、数据和权限、安装与更新 |
| 技术与数据源 | [选型](engineering/technology-selection.md)、[2026-11-07 调研证据](evidence/2026-11-07-stack-and-data-research.md) | GPUI 合理性复盘、美股/加密数据源比较、待测与替换 |
| 决定与上游 | [索引](decisions/README.md)、[确认/假设](decisions/confirmed-decisions.md)、[上游事项台账](decisions/open-questions.md) | 不依赖聊天的决定、开源风险与适配缺口 |
| 交付 | [路线/AC](engineering/delivery-plan.md)、[逐需求追踪](delivery/traceability.json) | M1～M7 演进、验收定义、阶段用例 |
| 证据 | [证据索引](evidence/README.md) | 版本化观察、各阶段验证与交付报告 |
| 模板 | [变更](templates/change-proposal.md)、[ADR](templates/decision-record.md)、[模块](templates/module-spec.md)、[验证](templates/validation-report.md) | 按规模选用，不全套强制 |

## 维护约定

保留稳定 FR/NFR/AC。需求、设计、验收和证据随行为变化同步；关键决定同轮回写。候选与正式依赖锁分开，录制模拟与真实在线服务验证分开。需求、缺陷、后续迭代与用户事项在 Issues；上游与开源适配缺口在上游事项台账。

机械检查：`python scripts/check_docs.py`；不代表产品实现或人工验收通过。
