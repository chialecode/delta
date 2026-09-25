# DELTA

**以个人资产账本为基础、以投资决策复盘为核心的 AI 原生桌面工作台。**

DELTA = **Delta + Evaluation + Ledger + Training + Analyzer**。

面向美股、ETF 和加密现货，将市场观察、资产记录、交易笔记、历史训练与策略评估连接到同一套可追溯的数据上。桌面应用及领域核心使用 Rust；AI 首轮参考 Codex 官方 Rust 实现自研模型客户端，指标与策略研究按需使用 Python。

## 当前阶段

当前已提交成果处于需求与设计阶段，尚无经过验证的应用实现；现场工作区状态见 [当前状态](docs/delivery/status.md)。本文档中的功能、性能和兼容性均为实现目标，不代表已经交付。

首版目标：导入一个真实账户的交易记录，核对资产和收益变化，在 K 线上复盘交易，并生成可追溯到原始记录的 AI 复盘报告。

文档框架已经建立，A 将据此制定首轮连续实施计划与交接 prompt；产品实现尚待验证。

## 文档

从 [文档导航](docs/README.md) 开始阅读。开发者和 AI 先读 [AGENTS](AGENTS.md)、[项目关键事实](docs/project-baseline.md)与[当前状态](docs/delivery/status.md)。

- [产品需求](docs/requirements/product-requirements.md)：用户、范围、功能编号与验收标准。
- [交互与工作流](docs/design/interaction-and-workflows.md)：页面、操作流程、异常状态和图表关联。
- [系统架构](docs/design/system-architecture.md)：Rust 桌面架构、模块边界、任务与扩展接口。
- [数据模型](docs/design/data-model.md)：账本、行情、笔记、实验和数据版本。
- [金融计算与模拟规则](docs/design/financial-engine.md)：损益、估值、收益率、撮合与历史可见性。
- [AI 设计](docs/design/ai-design.md)：工具调用、证据、权限与评估。
- [Rust 模型客户端](docs/design/rust-model-client.md)：官方源码参考、双协议、流式与自研边界。
- [开源优先的技术选型](docs/engineering/technology-selection.md)：候选依赖、维护证据、复用边界与验证门槛。
- [实施路线与验收](docs/engineering/delivery-plan.md)：阶段交付、需求映射、测试与待决事项。

## 实现原则

1. 优先复用活跃、许可证适用、可维护的开源实现，把开发投入放在方案整合、金融语义、AI 上下文状态和统一接口上。
2. 本地优先；用户拥有自己的账本、笔记和导出数据。
3. 金融计算可复现，AI 解释有证据，数据缺失显式展示。
4. 先完成可使用的纵向功能，再增加市场、账户和资产种类。
5. 技术选型先通过小型验证，再锁定版本；不为尚未出现的需求建设复杂基础设施。

## 协作与关键记录

后续采用 Agent A 计划与审核、Agent B 连续实现与自检；每轮输出对方可直接使用的 prompt。开源问题、人工决定与后续优化统一在 [集中事项台账](docs/decisions/open-questions.md)处理。需求、决定、契约、运行证据和交接全部保存在仓库，见 [文档治理](docs/governance/documentation-policy.md)。

当前可运行文档检查：`python scripts/check_docs.py`（Python 3.12+，标准库）。产品工程与运行入口由首轮实施建立。项目许可证尚未选择，依赖开源许可不等于本项目已按相同许可发布。
