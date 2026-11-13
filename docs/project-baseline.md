# 项目关键事实与接续入口

更新：2026-11-07（按 [D-20](decisions/confirmed-decisions.md) 从文档治理基线重建）。本文保留项目的关键入口和上下文；具体事实以所链接正本为准。

## 产品与投入重点

DELTA 是本地优先的个人资产账本和投资复盘工作台，覆盖美股、ETF、加密现货，后续逐步贯通训练与策略研究。

按 D-20，全项目**优先解决金融数据来源与本地自动化数据底座（M1），而不是先构建桌面 UI 应用**：

1. **M1（金融数据底座与账本核对核心）**：零桌面 GUI 依赖且按 [D-22](decisions/confirmed-decisions.md) **全项目 100% 纯 Rust 实现**，按 [D-23](decisions/confirmed-decisions.md) 聚焦用户主力平台（核心主力：**富途牛牛、欧易 OKX**；常用：**老虎证券、IBKR、长桥、嘉信、东方财富、币安**），构建 `delta-core`、`delta-data`、`delta-app`、`delta-cli`，打通美股/ETF（纯 Rust 直连 Yahoo Chart v8 / 东方财富美股日线免 Key 交叉核验，可选 Tiingo 官方 API）、加密现货（欧易 OKX 公开现货 REST + 币安公开行情镜像与归档）、法币/稳定币汇率（ECB/Frankfurter + Coinbase/Bitstamp/Kraken 真实 `USDT/USD`、`USDC/USD`）、交易日历（`XNYS`/`XNAS`/`CRYPTO_24_7`）及富途牛牛/欧易 OKX/IBKR/币安等账单导入与水位线自动增量同步。
2. **M2（完整资产复盘桌面应用与 AI 证据问答）**：在 M1 数据底座跑通后，按 [D-21](decisions/confirmed-decisions.md) 与 [ADR-0003](decisions/0003-first-round-stack.md) 接入最新 **GPUI Kit（`0.8.x` / `0.9.x`）官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系**（K 线面板配视口滑动窗口切片）及参考 Codex 官方源码的自研窄 Rust 模型客户端（D-08）。

## 新接手时恢复哪些信息

| 问题 | 唯一维护入口 |
| --- | --- |
| 谁负责、如何推进与交接 | [AGENTS](../AGENTS.md)、[A/B 协作](dev-rules/development-workflow.md) |
| 现在到底交付到哪里、下一步是什么 | [当前状态](delivery/status.md)、[当前阶段计划](delivery/stage-plan.md) |
| 什么已经确认、什么只是默认值 | [有效决定](decisions/confirmed-decisions.md) |
| 用户决定、人工检查、外部配置与具体远端授权 | [用户待办](../USER-ACTIONS.md) |
| 开源缺陷、返工、后续优化与承接 | [工程台账](decisions/open-questions.md) |
| 做什么、何时验收 | [需求](requirements/product-requirements.md)、[路线与验收](engineering/delivery-plan.md) |
| 金融含义、数据来源与恢复 | [金融规则](design/financial-engine.md)、[数据模型](design/data-model.md)、[系统架构](design/system-architecture.md) |
| 模块如何共同服务 CLI、UI 和 AI | [统一接口](design/integration-contracts.md) |
| AI 如何保存、压缩、恢复上下文 | [上下文状态](design/context-state-management.md)、[Rust 模型客户端](design/rust-model-client.md)、[AI 设计](design/ai-design.md) |
| 为什么重评 GPUI 与选择现有数据源栈 | [选型](engineering/technology-selection.md)、[ADR-0003](decisions/0003-first-round-stack.md)、[2026-11-07 调研证据](evidence/2026-11-07-stack-and-data-research.md) |
| 哪些结果有实证 | [证据索引](evidence/README.md)、[需求追踪](delivery/traceability.json) |

## 关键知识保留规则

聊天中改变需求、选择接口、发现隐含数据假设或确认验收后，当轮回写正本及相关决定/证据。区分**开发上下文**（通过仓库文档、Git 和交付报告接续）与**产品 AI 上下文**（按运行协议和数据权限持久化）。
