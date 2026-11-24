# DELTA 开发与贡献

AI Agent 与程序员遵循同一套规则：开工前先读 [AGENTS](AGENTS.md)，再看[文档导航](docs/README.md)与[已确认决定](docs/decisions/confirmed-decisions.md)。

## 完成一次改动

1. 从 [Issues](https://github.com/chialecode/delta/issues) 找到对应需求或缺陷；跨模块或改公共契约的改动，先在 issue 里写清方案。
2. 阅读相关设计；新增通用能力先比较开源方案（[开源与架构规则](docs/dev-rules/architecture-and-open-source.md)），上游缺口登记进[上游事项台账](docs/decisions/open-questions.md)。
3. 按[每次改动的流程](docs/dev-rules/development-workflow.md#change-loop)推进：一个 issue 一个小 PR，分支 `<type>/<issue号>-<简短描述>`，Conventional Commits，每个提交 `git commit -s`（DCO）。
4. 运行[快速门禁](docs/dev-rules/quality-gates.md#gate-tiers) `python scripts/quick_gate.py`，PR 里写清验证证据；由非作者按 [REVIEW](REVIEW.md) 审查后合并。受控验证、真实数据源和设备未测分别报告。

局部修复直接描述问题和验证；跨模块功能可用[变更模板](docs/templates/change-proposal.md)，长期取舍用 [ADR 模板](docs/templates/decision-record.md)，模块契约用[模块模板](docs/templates/module-spec.md)；按需选用，不为每个小变更生成全部材料。

## 许可、讨论与安全

本仓自有源码和文档采用 [Apache-2.0](LICENSE)（[D-24](docs/decisions/confirmed-decisions.md)），不另签贡献协议，每次提交通过 `git commit -s` 附带 DCO（`Signed-off-by`）声明；第三方依赖保留各自的许可和通知。依赖许可和数据授权分别盘点，安全问题按 [SECURITY](SECURITY.md) 私密报告。

Issues 与 Pull Request 只允许协作者创建；外部反馈、问题交流与功能建议请用 [Discussions](https://github.com/chialecode/delta/discussions)。
