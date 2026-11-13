# 当前执行状态

更新：2026-11-15；维护：Agent A / Agent B。当前阶段：**M5 训练工作台，计划 v1.0：B 实现与自检完成，A 集中技术审核通过，待用户授权 ACT-15（推送与 PR）**。唯一计划见 [M5 阶段计划](stage-plan.md)，用例见 [m5-stage-cases.json](m5-stage-cases.json)；阶段与口径来自用户 2026-11-14 的指示与选择（[D-33、D-34](../decisions/confirmed-decisions.md)）。

## 基线与判断

- M1 v2.6 经 [PR #1](https://github.com/chialecode/delta/pull/1) 合并为 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`；证据见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)。
- M2 v1.0 经 [PR #2](https://github.com/chialecode/delta/pull/2) 合并为 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`；证据见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)。产品验收未完成：真实模型（ACT-01）、真实个人账单（ACT-03）与 H-01、H-02 保持 not-run。
- M3 v1.0 经 [PR #3](https://github.com/chialecode/delta/pull/3) 合并为 `6f0aff39046504e218da9546063d3b93fabd49e7`；证据见 [M3 阶段报告](../evidence/2026-11-12-m3-stage-report.md)。首个正式版 [v0.1.1](https://github.com/chialecode/delta/releases/tag/v0.1.1) 已发布。H-03、H-04 与签名私钥备份（ACT-10）保持待办。
- M4 v1.0 经 [PR #6](https://github.com/chialecode/delta/pull/6) 合并为 `0ba60104413135fe1da3dc034aea7b0dc8879a50`；证据见 [M4 阶段报告](../evidence/2026-11-14-m4-stage-report.md)。两级门禁（`scripts/quick_gate.py`）经 PR #7 合并为 `a44f86f4a168e7dd9c51d186cd9668a51d1f0ccd`。H-05 与可选 ACT-01 保持 not-run。
- M5 分支 `codex/m5-training-workbench`，固定阶段父提交 `a44f86f4a168e7dd9c51d186cd9668a51d1f0ccd`；A 计划与阶段清理是父提交之后唯一的阶段提交，完整 SHA 以 `git rev-parse HEAD` 为准。
- 既有未跟踪 `workers/` 保持原样，未读取、构建、修改或暂存。未执行 push、PR 或发布。

## 审核后用户要求的小改动（2026-11-16）

- 桌面二进制改为 Windows GUI 子系统，启动不再弹出终端窗口；`--version`、`--help`、`--build-info` 与 `*-ui-smoke` / `*-ui-bench` 在终端里会附着父控制台输出，管道调用（门禁、安装器 `--build-info`）不受影响（新增 `windows-sys` 特性 `Win32_System_Console`）。
- 默认字号 13 → 15 px（等宽 12 → 14 px，导航标签 11 → 12 px），同步 [设计规则](../design-rules/DESIGN.md)。
- 验证：`quick_gate.py` passed（工作区测试）；构建产物 PE 子系统为 GUI，管道输出 `--version` / `--build-info` 正常。整套门禁未重跑，已装 `delta-dev` 仍为 `9d51a55f…`，需重新 `package-build` + `install-dev` 才生效。

## 本轮（A 集中技术审核，2026-11-15）

- 按计划第 10 节复核第 7 节不得缩减项、off-grid K 线与跨缺口窗口、前缀计数池、准备中按级别开局、`verify_m5` 的 live 层与 G-06 not-run 判定、真实源与夹具分开声明，以及 M5-W08；逐项结论见 [M5 阶段报告](stage-report.md#a-review)。
- **A-F01（阻断，已修正）**：准备作业在一个 savepoint 里整批下载，下载期间一直持有写锁（`busy_timeout` 5 s），所以准备期间开局、推进与划转都报 `database is locked`，B 的“已就绪级别可开局”在真实应用里不生效。现在下载不写库，只有几秒的提交阶段持锁；原子性不变，加了回归测试。同时修正：币安 REST 分页改用原始行推进游标。
- 验证：`quick_gate.py` 与 `quick_gate.py --all` passed，`deps.py --audit` passed。整套门禁证据仍对应 B 交付指纹 `c97131fc…`（按两级门禁不重跑）；最终指纹 `9d51a55f4950522f5bfdbf25cba7629e94d3e899e210a14257036f976ec7f3e2`。修正已 amend 进同一阶段提交，父提交不变。
- 审核时按指示未触碰 `delta` / `delta-dev`；之后按用户要求重装开发版：`delta-dev` 现为 `0.1.2-dev.20261115093401`（`source_fingerprint = 9d51a55f…`），正式版未触碰。

## 上一轮（B 实现与自检，2026-11-15）

- `M5-W01～W08` 全部完成，结果见 [M5 阶段报告](stage-report.md)。被测源码指纹 `c97131fc40a746b5d0a475b7e0b79a9fb938d73b7c0e2831fe0a5c4793dcafe6`：`verify-stage --stage M5 --plan-version 1.0 --ui-smoke --ui-bench --live-data` passed（`M5-K01～K03`、`M5-A-01～17`、`M5-L-01` passed，`M5-M-01` 为人工 H-06），同指纹 M1～M4 回归 passed。
- 真实公开源实跑（`M5-L-01`，临时资料库、release）：比特币 `1d`/`4h`/`1h` 首次准备 857.1 s（≤ 900 s），`1h` 候选池 0.674 s。实跑中发现并修复两项：币安 2018-02 的偏移小时线导致 `1h` 批失败；`1h` 候选池构建 152.5 s。另外桌面改为某级别一就绪即可开局。
- 依赖：`zip`、`quick-xml` 的漏洞已修复，签名相关依赖已升级，`deps.py --audit` 结果为 0 个未处理漏洞；未满 7 天的新版本登记在 DEPS-01。
- 开发版 `0.1.2-dev.20261115084615` 已装到本机 `delta-dev`，供 H-06 使用。

## 更早（A 完整阶段计划，2026-11-14）

- 用户开启新阶段：参考提供的训练软件截图做训练工作台，不做期货，美股与加密共用一套，默认以比特币作为训练数据（D-33）；并选择跨局练习账户、加密 4H/1H、快捷单按当前收盘价（D-34）。阶段插入为 M5，策略研究顺延 M6、扩展顺延 M7。
- 现场核查（只读打开开发版资料库，公开源同步只写系统临时目录）：开发版资料库 0 根 K 线；2021-06 之前的比特币同步因稳定币汇率缺失整体失败；币安 BTC 日线全部被误标 `missing_fee_asset_fx`，导致无法开局，而欧易 BTC 可以；币安 `BTCUSDT` 1h/4h/1d 自 2017-08-17 起、欧易自 2018 年起可得。细节与处置见 [M5 计划](stage-plan.md#1-现状与目标)第 1 节与 S-48、S-49。
- 阶段清理：M4 阶段报告迁入 [证据](../evidence/2026-11-14-m4-stage-report.md)；路线、需求（FR-TRN-07～09）、验收（AC-31～33）、追踪、决定、台账、登记表（`currentStage = M5`）、门禁脚本阶段表（到 `M7`）同步。`python scripts/check_docs.py` passed。

## 接续

下一步：用户授权 [ACT-15](../../USER-ACTIONS.md)（推送 `codex/m5-training-workbench` 并创建到 `main` 的 PR）；授权后 A 执行并核对 CI（`docs`、DCO）与审查反馈。合并、标签与发布另行授权。

用户事项独立推进、不阻塞：H-06 训练工作台实机体验（开发版已重装为 `9d51a55f…`），ACT-10 私钥离线备份，H-01～H-05，可选 ACT-01 与 ACT-03，以及 Q-04、Q-05 默认值。
