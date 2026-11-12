# 当前执行状态

更新：2026-11-12；维护：Agent A / Agent B。当前阶段：**M3 v1.0 已合并（PR #3，`main` `6f0aff3`）；首个正式版 v0.1.1 已发布（ACT-11）**。按用户指示与 [D-31](../decisions/confirmed-decisions.md)，M2 之后先补齐 Windows 安装包、版本与自动更新，原 M3～M5 顺延为 M4～M6。唯一计划见 [M3 阶段计划](stage-plan.md)，用例见 [m3-stage-cases.json](m3-stage-cases.json)，实际证据见 [M3 阶段报告](stage-report.md)。

## 基线与判断

- M1 v2.6 经 [PR #1](https://github.com/chialecode/delta/pull/1) 合并为 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`；证据见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)。
- M2 v1.0 按 `ACT-09` 授权经 [PR #2](https://github.com/chialecode/delta/pull/2) squash 合并为 `main` `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`；证据与 A 审核结论见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)。M2 产品验收仍未完成：真实模型（ACT-01）、真实个人账单（ACT-03）与 H-01、H-02 保持 not-run（H-03 改为 M3 安装器步骤）。
- 当前分支 `codex/m3-installer-update`，固定阶段父提交 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`；A 计划与 B 实现在同一阶段提交中，完整 SHA 见 B→A 交接消息与 `git rev-parse HEAD`。
- **B 实现：完成**（M3-W01～W06）。**A 集中技术审核：通过**：修正 TEST-03 测量口径（隐藏窗口 EcoQoS 节流）、M2 便携迁移预检、安装包固定输出到 `dist/` 与产物清理，并按用户 [D-32](../decisions/confirmed-decisions.md) 把凭据与签名私钥改为文件；最终源码指纹 `77dd43c8…` 下 M3 `--installer --ui-smoke`、M2 `--ui-smoke --ui-bench`、M1 回归全部 passed。产品验收：未开始（H-03、H-04、真实发布 not-run）。
- 既有未跟踪 `workers/agent/` 保持原样，未读取、构建、修改或暂存。未执行 push、PR、GitHub Release 或远端设置变更。

## 本轮（A 审核）

- 对照计划第 5、6 节复核身份隔离、数据保留、不强杀、更新传输与签名、私钥边界、分离启动与 M2 迁移，结论见 [阶段报告](stage-report.md#a-集中技术审核2026-11-09)。
- 用户要求：构建产物只留最新（`dist/`、`scripts/clean_artifacts.py`、dev profile；`target/` 32 GB → 约 3.5 GB）；凭据不用凭据管理器（D-32）；mbx 评估登记 `LOOP-05`。
- TEST-01、TEST-03 关闭。未执行 push、PR、GitHub Release 或远端设置变更；`workers/agent/` 未触碰。

## 接续

- 2026-11-12：M3 按 `ACT-12` 经 [PR #3](https://github.com/chialecode/delta/pull/3) squash 合并为 `main` `6f0aff3`。正式版签名密钥已生成（ACT-10，私钥在仓库外，待用户备份），公钥内置于 `crates/delta-app/src/release-signing-key.json`。

- 2026-11-12：首个正式版 [v0.1.1](https://github.com/chialecode/delta/releases/tag/v0.1.1) 按 `ACT-11` 从 `main` `3bb97e6` 构建发布；从 `releases/latest` 取回的清单、安装包与签名用内置公钥验证通过（`M3-L-01` 的发布部分）。已装正式版收到下一个版本的实机体验仍是 H-04。

首选下一步：用户备份签名私钥（ACT-10），按 H-03 / H-04 实机安装与体验更新；Q-04、Q-05 独立推进。

## B 实现记录

- 新增安装身份、版本推算、单实例与本身份控制管道、Ed25519 签名更新（`delta-app`），保存后退出序列、工具栏“更新就绪”、设置页“关于/更新”（`delta-desktop`），Inno Setup 7.1.1 按用户安装器模板、`package-build` / `install-dev` / `release-build` / `release-keygen` / `release-key-export` / `control` / `update-test-source` 与 M2 便携安装迁移（`delta-cli`），Δ 图标与 `VERSIONINFO`。
- `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` 只真实安装测试身份 `delta-test` / `delta-dev-test`，在线更新以 `127.0.0.1` 回环源演练；回环演练不是 GitHub 发布。M1 v2.6、M2 v1.0 回归在同一源码指纹下实跑。
- `M3-A-10` 的静音、隔离、清理部分在同一次运行中已通过；未通过的只有 M2 回归前置条件。spike 结论、门禁中发现并修复的问题（Inno 零尺寸辅助窗口、卸载第二阶段被作业终止、控制管道关闭竞态、分离启动继承调用方标准句柄）与 not-run 边界见阶段报告。
