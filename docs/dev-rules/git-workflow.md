# Git 初始化、阶段提交与远端授权

## 1. 基础历史与 D-20 重建基线

1. 第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`（空 `README.md`）为不可变基础提交，**严禁 amend 或改写其 SHA**。文档治理与开发契约基线按用户 2026-11-08 决定并入 M1 阶段提交（[D-29](../decisions/confirmed-decisions.md)）。
2. 按用户 2026-11-07 明确指示（[D-20](../decisions/confirmed-decisions.md)），项目已从文档治理基线重建，放弃此前桌面先行构建的提交与产物（旧本地分支已备份在忽略目录 `.local/rebuild-backup-20261107/`）。
3. 远端 `origin/main` 与本地 `main` 已按用户 2026-11-07 授权重建（新建 `chialecode/delta` 并启用 `main-protection` ruleset），仅保留第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`；含文档治理基线的阶段分支 `codex/m1-data-foundation`（以 `8329f63` 为唯一父提交）按用户 `ACT-08` 授权于 2026-11-08 推送并经 PR #1 squash 合并为 `main` `0370b7e9103d0172a7ab47613ea253949d6a9b8b`，阶段分支已删除；`main` 只允许 squash 合并，阶段合并后 `main` 上为一个 squash 提交，原始提交保留在 PR 记录中。

<a id="dco"></a>

**DCO（D-27）**：仓库启用 DCO 检查，**每个提交**（含基础提交、阶段提交与 PR 追加修复）都必须带与作者一致的 `Signed-off-by` 尾注，一律使用 `git commit -s` / `git commit --amend -s`。push 前对待推送范围逐个核对：`git log --format='%h %(trailers:key=Signed-off-by,valueonly)' origin/main..HEAD`，任何一行缺签名即先补签再推送。

## 2. 一个阶段一个可审核提交（D-09）

<a id="single-stage-commit"></a>

1. 用户 D-09 明确要求：同一阶段在 push 前尽可能只保留一个 commit。
2. A 在阶段父提交之后建立当前阶段（阶段与计划版本见 [阶段计划](../delivery/stage-plan.md)）的计划提交；push 前 B 的实现、A 的审核修正和 B 的返工**直接 `git commit --amend -s` 进该阶段提交**，保留 DCO `Signed-off-by`，不把中间过程、错误实现或沟通轮次留作独立提交。
3. 阶段父提交按唯一计划固定，amend 前后核对不变；M4 为 `main` `45ac8c527ab5b51217d5f712deebd704e9d539ef`（M3 squash 与 v0.1.1 发布记录之后）。不得误 amend 第一提交、已关闭阶段或无关提交。
4. 暂存与提交前必须执行 `python scripts/check_docs.py` 与 `git diff --check`，使用 `git add -u` 或明确文件路径暂存（严禁 `git add .` 或 `git clean -fdx`）。每次 amend 后核对父提交不变、工作区剩余未跟踪文件（如 `workers/agent/`）归属清楚且未被混入暂存区。最终交付消息给出**新的完整 SHA**。

## 3. 远端动作、低频 GitHub 调用与明确授权（D-14 / D-18）

<a id="remote-auth"></a>
<a id="github-hygiene"></a>

远端为 [chialecode/delta](https://github.com/chialecode/delta)。

| 动作 | 执行条件与结束点 |
| --- | --- |
| 本地 commit / amend | 已有本地开发授权，完成相应验证和暂存核对；提供新完整 SHA，无需逐轮再问 |
| push / 远端同步 | 技术收敛、候选验证与公开内容核对完成，且用户已对 [USER-ACTIONS](../../USER-ACTIONS.md) 中的具体 ACT 明确授权 |
| 创建 / 更新 PR | 已获相应 PR 动作授权，来源/目标与范围明确；正文列明实际行为、检查和未测边界 |
| 合并 / 分支清理 / 发布 | 分别需要用户明确授权，不能从本地通过、流程说明或前序特例推导 |

- **仓库入口**：为防止垃圾内容，外部只开放 Discussions；关闭 Issues、Wiki 和 Projects，PR 仅协作者可创建；来自 fork 的 PR 运行 workflow 前须维护者批准（所有外部贡献者）；安全问题走私密漏洞报告。期望配置见 [repository-settings.json](../../.github/repository-settings.json)，以 GitHub 实际回读为准。
- **Actions 白名单**：仓库只允许 `actions/checkout@*` 与 `actions/setup-node@*`（GitHub 实际回读，2026-11-08）。workflow 不得引入其他 action；文档门禁 `scripts/check_docs.py` 无第三方依赖，CI 直接使用 runner 自带 `python3`。需要新 action 时先在 USER-ACTIONS 登记远端设置变更。
- **GitHub API / CLI 低频原则**：执行已获授权的远端查询或配置时，优先单次 `gh api` / `gh pr view` 获取所需字段，禁止高频轮询 CI 状态或重复调用远端写操作。
- 远端 `origin/main` 当前为 `45ac8c527ab5b51217d5f712deebd704e9d539ef`（M3 经 PR #3 squash 为 `6f0aff3`，其后 PR #4 内置更新公钥、PR #5 记录 v0.1.1 发布，2026-11-12）。M4 阶段分支 `codex/m4-kline-training` 以其为唯一父提交；在用户针对 [USER-ACTIONS](../../USER-ACTIONS.md) 中具体 ACT 明确授权前不推送。阶段编号与 PR 编号不绑定（[D-28](../decisions/confirmed-decisions.md)），实际 PR 号记录在阶段报告与执行状态。
- 远端重建或历史改写后，先核对本地 `main` 与 `origin/main` 是否分叉再切换；分叉的旧提交先用 `git bundle` 备份到 `.local/` 再对齐（2026-11-08 本地旧 `main` `27b26d1` 已备份为 `.local/rebuild-backup-20261108/`）。
