# 项目关键事实与接续入口

更新：2026-09-26。本文保留项目的关键入口和上下文；具体事实以所链接正本为准，不复制字段合同或执行台账。

## 产品与投入重点

DELTA 是本地优先的个人资产账本和投资复盘桌面工作台。覆盖美股、ETF、加密现货，后续逐步贯通训练与策略研究。首个可用目标是导入、核对、图表与笔记、带证据的 AI 复盘及备份恢复。

优先活跃开源方案；自研重点是方案整合、金融语义、稳定的能力接口、上下文状态和证据关联。数据库、通用控件、网络、序列化和流解析继续复用开源；按 D-08 自研窄 Rust 模型客户端及上下文整合，参考 Codex 官方源码并自行维护兼容/恢复。用户确认及假设分别见 [决定记录](decisions/confirmed-decisions.md)。

## 新接手时恢复哪些信息

| 问题 | 唯一维护入口 |
| --- | --- |
| 谁负责、如何推进与交接 | [AGENTS](../AGENTS.md)、[A/B 协作](dev-rules/development-workflow.md) |
| 现在到底交付到哪里 | [当前状态](delivery/status.md) |
| 什么已经确认、什么只是默认值 | [有效决定](decisions/confirmed-decisions.md) |
| 开源缺陷、人工决定、外部配置与后续优化 | [用户待办](../USER-ACTIONS.md)、[工程台账](decisions/open-questions.md) |
| 做什么、何时验收 | [需求](requirements/product-requirements.md)、[路线与验收](engineering/delivery-plan.md) |
| 金融含义、数据来源与恢复 | [金融规则](design/financial-engine.md)、[数据模型](design/data-model.md) |
| 模块如何共同服务 UI 和 AI | [统一接口](design/integration-contracts.md)、[架构](design/system-architecture.md) |
| AI 如何保存、压缩、恢复上下文 | [上下文状态](design/context-state-management.md)、[Rust 模型客户端](design/rust-model-client.md)、[AI 设计](design/ai-design.md) |
| 为什么选择现有技术 | [选型](engineering/technology-selection.md)、[ADR 目录](decisions/README.md) |
| 哪些结果有实证 | [证据索引](evidence/README.md)、[需求追踪](delivery/traceability.json) |

## 关键知识保留规则

聊天中改变需求、选择接口、发现隐含数据假设或确认验收后，当轮回写正本及相关决定/证据。尚未完成的下一步写当前状态和有效计划；开发笔记不能覆盖历史验证事实。新计划引用前轮最终版本和未关闭问题，避免从零重新讨论。

区分两类上下文：**开发上下文**通过仓库文档、Git 和交付报告接续；**产品 AI 上下文**按运行协议和数据权限持久化。不得把开发聊天自动导入用户分析会话，或将产品会话摘要当作开发授权。
