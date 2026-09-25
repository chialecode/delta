# DELTA 开发与贡献

先读 [AGENTS](AGENTS.md) 与 [当前状态](docs/delivery/status.md)。当前是文档和实施准备阶段；没有 Cargo 工程不等于构建失败，也不能声称应用测试通过。

## 最小开发闭环

1. 找到用户目标、需求 ID、验收场景、当前计划和已确认决定。
2. 阅读相关设计；新增通用能力先比较开源方案，候选缺口记入 [集中事项台账](docs/decisions/open-questions.md)。
3. 在明确范围内实现、验证和修复；公有契约、数据兼容与文档同次更新。
4. 运行 [质量门禁](docs/dev-rules/quality-gates.md)，按 [REVIEW](REVIEW.md)自查；记录真实版本、样本、命令与结果。
5. 按 [协作流程](docs/dev-rules/development-workflow.md)提交完整交付报告，集中交接，不按单个文件交接。

当前可运行：`python scripts/check_docs.py`，Python 3.12+，无第三方依赖。后续依赖安装、运行、测试命令由 B 随真实工程补全，不预先声称存在。

## 变更材料

局部修复直接描述问题和验证；跨模块功能使用 [变更模板](docs/templates/change-proposal.md)；长期取舍使用 [ADR 模板](docs/templates/decision-record.md)；模块契约使用 [模块模板](docs/templates/module-spec.md)。使用所需字段，不为每个小变更生成全部材料。

Git 约定见 [Git 工作流](docs/dev-rules/git-workflow.md)。远端 PR 可使用 [PR 模板](.github/PULL_REQUEST_TEMPLATE.md)。本仓项目许可证尚未选定；不得借用依赖许可证宣称整个项目已按同一许可证开放。依赖许可和数据授权分别盘点，见 [安全说明](SECURITY.md)。
