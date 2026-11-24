# Git 与远端规则

## 1. 基础历史

1. 第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`（空 `README.md`）为不可变基础提交，**严禁 amend 或改写其 SHA**。
2. 按用户 2026-11-07 指示（[D-20](../decisions/confirmed-decisions.md)），项目已从文档治理基线重建；远端 `chialecode/delta` 同日重建，旧仓库改名保留。远端重建或历史改写后，先核对本地 `main` 与 `origin/main` 是否分叉再切换；分叉的旧提交先用 `git bundle` 备份到 `.local/` 再对齐。
3. 本仓设置 `core.autocrlf=false`、`pull.ff=only`、`fetch.prune=true`。

<a id="dco"></a>

**DCO（D-27）**：仓库启用 DCO 检查，**每个提交**都必须带与作者一致的 `Signed-off-by` 尾注，一律使用 `git commit -s`。push 前核对：`git log --format='%h %(trailers:key=Signed-off-by,valueonly)' origin/main..HEAD`，任何一行缺签名先补签再推送。

<a id="single-stage-commit"></a>
## 2. 分支、提交与 PR

- 一个 issue 一个小 PR，分支 `<type>/<issue号>-<简短描述>`，Conventional Commits（`type(scope): 具体结果`），正文写最终行为、验证和限制；PR 合并时 squash 成一个提交，分支里提交几次都可以（[每次改动的流程](development-workflow.md#change-loop)）。
- 阶段编号（`M1`～`M7`）只用于路线与验收：人读标签大写，机器标识中的阶段部分小写（[D-30](../decisions/confirmed-decisions.md)）；阶段编号与 PR、issue 编号独立（D-28）。
- 暂存与提交前执行 `python scripts/check_docs.py` 与 `git diff --check`，用 `git add -u` 或明确文件路径暂存（严禁 `git add .` 或 `git clean -fdx`）；工作区里与任务无关的未跟踪文件（如 `workers/`）不混入暂存区。

<a id="remote-auth"></a>
<a id="github-hygiene"></a>
## 3. 远端

远端为 [chialecode/delta](https://github.com/chialecode/delta)。

| 动作 | 条件 |
| --- | --- |
| push、建 PR、更新 PR | 长期授权；push 只推指定分支，不用 `--all`/`--mirror` |
| 合并低风险 PR | CI `repository-quality` 与独立审查通过后由 Agent 用 `gh pr merge <号> --squash --match-head-commit <头提交>` 合并 |
| 合并[高风险](development-workflow.md#high-risk) PR | 用户合并 |
| 发布正式版、改远端设置与规则集、签名密钥、改写已推送历史 | 用户对具体动作授权（改写时用指定旧 SHA 的 `--force-with-lease`） |

- **规则集 `main-protection`**：`main` 只经 PR 写入，禁止强推和删除，线性历史，只允许 squash 合并，所有讨论必须解决，必需检查为 `repository-quality`（[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)：Windows 运行器上的文档与空白检查、`cargo fmt --check`、`cargo clippy -D warnings` 与 `cargo test --workspace --locked`，等同 `quick_gate.py --all`；Cargo 依赖与 `target/` 用 `actions/cache` 按 `Cargo.lock` 与工具链缓存），无绕过名单。
- **仓库入口**：开启 [Issues](https://github.com/chialecode/delta/issues)（只允许协作者创建）与 [Discussions](https://github.com/chialecode/delta/discussions)，关闭 Wiki 与 Projects；PR 仅协作者可创建；允许自动合并与更新分支，合并后删除分支；网页提交要求 DCO 签名；来自 fork 的 PR 运行 workflow 前须维护者批准（所有外部贡献者）；安全问题走私密漏洞报告。远端设置的镜像是 [repository-settings.json](../../.github/repository-settings.json)、[rulesets/main.json](../../.github/rulesets/main.json) 与 [actions-policy.json](../../.github/actions-policy.json)，以 GitHub 实际回读为准，改远端设置时同步更新镜像；其中 `issue_creation_policy` 与 `pull_request_creation_policy` 的 REST 接口不返回，用 GraphQL 仓库字段 `issueCreationPolicy` / `pullRequestCreationPolicy` 核对。
- **Actions**：只允许镜像里列出的官方 action（`actions/checkout`、`actions/cache`），远端强制固定完整 SHA 并注明版本，Dependabot 同步更新；token 默认只读。
- **Dependabot**（[.github/dependabot.yml](../../.github/dependabot.yml)，D-35）：Cargo 与 `rust-toolchain.toml` 每周、Actions 每月，冷却期 7 天，兼容更新合为一组，关闭自动 rebase；版本更新 PR 在[每周维护](development-workflow.md#weekly)时处理，风险分级见[依赖盘点](quality-gates.md#dependencies)：Cargo 兼容更新与 `rust-toolchain.toml` 小版本按低风险，主版本与 Actions 更新按高风险。Dependabot 告警与自动安全修复 PR 已打开；安全修复 PR 不受冷却期限制，有就尽快处理。
- **GitHub API / CLI 低频原则**：优先单次 `gh api` / `gh pr view` 获取所需字段，禁止高频轮询 CI 状态或重复调用远端写操作。
