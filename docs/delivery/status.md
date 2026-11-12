# 当前执行状态

更新：2026-11-14；维护：Agent A / Agent B。当前阶段：**M4 K 线训练，计划 v1.0：A 技术审核通过（工程阻断 0），用户已于 2026-11-14 授权 `ACT-13`（推送、PR、squash 合并与清理分支）**。唯一计划见 [M4 阶段计划](stage-plan.md)，验证与交付报告见 [M4 阶段报告](stage-report.md)，用例见 [m4-stage-cases.json](m4-stage-cases.json)。

## 基线与判断

- M1 v2.6 经 [PR #1](https://github.com/chialecode/delta/pull/1) 合并为 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`；证据见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)。
- M2 v1.0 经 [PR #2](https://github.com/chialecode/delta/pull/2) 合并为 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`；证据见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)。产品验收未完成：真实模型（ACT-01）、真实个人账单（ACT-03）与 H-01、H-02 保持 not-run。
- M3 v1.0 经 [PR #3](https://github.com/chialecode/delta/pull/3) 合并为 `6f0aff39046504e218da9546063d3b93fabd49e7`；证据见 [M3 阶段报告](../evidence/2026-11-12-m3-stage-report.md)。首个正式版 [v0.1.1](https://github.com/chialecode/delta/releases/tag/v0.1.1) 从 `main` `3bb97e6` 构建发布，发布资产用内置公钥验证通过。H-03、H-04 与签名私钥备份（ACT-10）保持待办。
- M4 分支 `codex/m4-kline-training`，固定阶段父提交 `45ac8c527ab5b51217d5f712deebd704e9d539ef`；B 实现、A 审核修正按 [D-09](../decisions/confirmed-decisions.md) 合并入同一阶段提交，**最终被测源码指纹 `96254d7db4d8ae6a1dfda724d156ba815142d00e4816a9392ce02f943f90714a`**（B 自检指纹 `b104c94a…`），完整 SHA 以 `git rev-parse HEAD` 为准。
- 既有未跟踪 `workers/` 保持原样，未读取、构建、修改或暂存。未执行 push、PR 或发布。

## 本轮（A 集中技术审核，2026-11-13）

- 独立重算 B 指纹一致、复跑工作区测试一致后，对照计划逐项复核未来截断、撮合/预留/费用、公司行动、迁移与账本隔离、原子提交与重放、AI 权限、隐藏模式、快捷键与 CLI、`Packager.toml` 退役与 `TEST-02`，结论见 [M4 阶段报告](stage-report.md#a-集中技术审核2026-11-13)。
- 修正 **F-01（P2）**：恢复重放时的存储读取失败曾被当作状态损坏，把运行永久标记为 `damaged`；改为原样报错、运行不变，只有哈希/命令/K 线/检查点不一致才标记损坏，新增回归测试。另记 3 项 P3 观察（不返工）。
- 最终指纹 `96254d7d…` 下 `cargo test` 139 passed / 0 failed / 2 ignored，`verify-stage` M1 v2.6、M2 v1.0（`--ui-smoke --ui-bench`）、M3 v1.0（`--installer --ui-smoke`）、M4 v1.0（`--ui-smoke --ui-bench`）全部 **passed**；“下一根” p95 = 41.0 ms。紧接安装门禁后曾测得 p95 约 85 ms，A/B 复测确认来自本机状态而非代码（详见报告）。`M4-M-01`（H-05）与 `M4-L-01`（ACT-01）保持 not-run。未执行 push、PR 或发布。

## 本轮（B 实现与自检，2026-11-12）

- 完成 `M4-W01～W07` 全部可执行范围：SQL/宿主层回放时钟截断端口与时点拆股复权（`M4-K01`，数值与字符串未来哨兵零泄漏）、`SHA-256` 确定性候选池抽样与隐藏模式全链路脱敏、纯 Decimal 现货模拟撮合与可组合费用引擎（`delta-core::sim` `m4-sim-1`，仅内置 3 个明示场景假设费用模板，富途美股与欧易现货核查后保留在 `LOOP-06`）、`schema 5` 迁移前备份与 8 张模拟/训练表原子持久化、崩溃恢复与三路确定性重放（`M4-K02`）、训练报告与同期买入持有基准（`delta-core::sim_report` `m4-report-1`）、训练笔记归档与结束后只读 AI 复盘（`ScopeTarget::TrainingRun`）、桌面“训练”工作区（`Ctrl+Enter` 提交与 `N`/`→` 推进严格分离）与 `delta-cli training` 命令集。
- 完成阶段清理与台账修复：删除 M2 遗留 `Packager.toml` 并更新源码指纹清单与仓库地图；加固 M1 `lock_probe` 显式持锁就绪握手并关闭 `TEST-02`。
- 最终源码指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa` 下，`verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` **passed**（`M4-G-01～06`、`M4-A-01～18`、`M4-K01～K03` 全部 passed，其中 1,000 根历史＋默认指标 300 步“下一根”延迟 `p50 = 31.151 ms`、`p95 = 39.966 ms`、`p99 = 57.453 ms`（`max = 131.345 ms`），满足 `p95 <= 100 ms`），同一指纹下 `M1 v2.6`、`M2 v1.0`（`--ui-smoke --ui-bench`）与 `M3 v1.0`（`--installer --ui-smoke`）回归全部 **passed**；全程 0 可见窗口、焦点未变、真实通道数据未触碰。`M4-M-01`（`H-05`）与 `M4-L-01`（`ACT-01`）保持 `not-run`，详见 [M4 阶段报告](stage-report.md)。

## 接续

下一步：按已授权的 [ACT-13](../../USER-ACTIONS.md) 推送 `codex/m4-kline-training`、创建到 `main` 的 PR，检查通过后 squash 合并。

用户事项独立推进、不阻塞：ACT-10 私钥离线备份，H-01～H-05 实机检查，可选 ACT-01 真实模型连接与 ACT-03 个人样本/Token，以及 Q-04、Q-05 默认值。
