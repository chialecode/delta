# DELTA 开发与贡献

先读 [AGENTS](AGENTS.md) 与 [当前状态](docs/delivery/status.md)。项目已按 D-20 从文档治理基线重建，优先构建 M1 金融数据来源与本地数据底座，尚未包含产品实现代码。

## 最小开发闭环

1. 找到用户目标、需求 ID、验收场景、当前计划和已确认决定。
2. 阅读相关设计；新增通用能力先比较开源方案，候选缺口记入 [集中事项台账](docs/decisions/open-questions.md)。
3. 在明确范围内实现、验证和修复；公有契约、数据兼容与文档同次更新。
4. 运行 [质量门禁](docs/dev-rules/quality-gates.md)，按 [REVIEW](REVIEW.md)自查；记录真实版本、样本、命令与结果。
5. 按 [协作流程](docs/dev-rules/development-workflow.md)提交完整交付报告；B 交给 A 集中审核，A 收敛后准备明确版本与 PR 草稿，并通过 [USER-ACTIONS](USER-ACTIONS.md) 请求用户授权具体远端动作。

文档修改运行 `python scripts/check_docs.py` 与 `git diff --check`，文档脚本使用 Python 3.12+ 标准库。产品工程建立后，Rust 工具链按 `rust-toolchain.toml` 与锁文件管理；阶段统一门禁（待 B 建立）为 `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.3`。原始日志留忽略目录，受控验证、真实数据源和设备未测分别报告。

按一个阶段一个分支/提交/PR 工作，未 push 的阶段修正直接 amend，使用现有 Git 身份保留 DCO 签名；已 push 后默认追加集中修复。A 技术通过后，人工检查与非阻塞决定可以顺延，不必为了等待它们再交 A 审核。push、创建/更新 PR、合并和发布分别需要明确授权，不能从技术通过或参考流程中推导。

## 变更材料

局部修复直接描述问题和验证；跨模块功能使用 [变更模板](docs/templates/change-proposal.md)；长期取舍使用 [ADR 模板](docs/templates/decision-record.md)；模块契约使用 [模块模板](docs/templates/module-spec.md)。使用所需字段，不为每个小变更生成全部材料。

Git 约定见 [Git 工作流](docs/dev-rules/git-workflow.md)。远端 PR 可使用 [PR 模板](.github/PULL_REQUEST_TEMPLATE.md)。本仓自有源码和文档采用 [Apache-2.0](LICENSE)（[D-24](docs/decisions/confirmed-decisions.md)）；第三方依赖保留各自的许可和通知，提交即表示同意按该许可贡献。依赖许可和数据授权分别盘点，见 [安全说明](SECURITY.md)。

外部反馈、缺陷和建议请使用 [Discussions](https://github.com/chialecode/delta/discussions)；为防止垃圾内容，仓库关闭 Issues，PR 仅限协作者创建。
