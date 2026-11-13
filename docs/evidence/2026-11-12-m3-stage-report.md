# M3 v1.0 阶段验证与交付报告

> 2026-11-12 归档说明（Agent A，M4 开始时阶段清理）：本报告原位于 `docs/delivery/stage-report.md`。M3 已按 `ACT-12` 授权经 [PR #3](https://github.com/chialecode/delta/pull/3) squash 合并为 `main` 提交 `6f0aff39046504e218da9546063d3b93fabd49e7`；随后按 `ACT-10` 内置正式版更新公钥（`main` `3bb97e6`），按 `ACT-11` 从该提交构建并发布首个正式版 [v0.1.1](https://github.com/chialecode/delta/releases/tag/v0.1.1)，从 `releases/latest` 取回的清单、安装包与签名用内置公钥验证通过（`M3-L-01` 的发布部分）。正文保持合并时内容，只修正归档后的相对链接；有效决定与未关闭事项（H-03、H-04、TEST-02、ACT-10 私钥备份）已回写正本。

更新：2026-11-09；角色：Agent B（实现与自检）/ Agent A（集中审核）；唯一计划：M3 v1.0（PR #3 版本 `docs/delivery/stage-plan.md`，Git 历史归档），用例见 [m3-stage-cases.json](../delivery/m3-stage-cases.json)。分支 `codex/m3-installer-update`；固定父提交 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`；B 交付的阶段提交 `9199fe0b1abde44ee14e60bcfb728f35f1dd53c5`（B 最终被测指纹 `95003b7d…`）。A 审核修正已 amend 进同一阶段提交，**最终被测源码指纹 `77dd43c8798e0b97b93f4a970a1d89b79bc3416f49e75c3142458afa91c09622`**；完整 SHA 以交接消息与 `git rev-parse HEAD` 为准。未跟踪 `workers/agent/` 未读取、构建、修改或暂存；未执行 push、PR、GitHub Release 或远端设置变更。

## 当前判断

**B 实现：完成；A 集中技术审核：通过，工程阻断 0；产品验收：未开始。** 最终指纹下 `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` overall **passed**（`M3-G-01～06`、`M3-A-01～10`、`M3-K01～K04` 全部 passed），同指纹 M1 v2.6 与 M2 v1.0（`--ui-smoke --ui-bench`）回归 passed。B 自检时唯一的失败 `M3-A-10`（M2 帧性能，TEST-03）经 A 定位为 Windows 对隐藏窗口进程的执行速度节流（EcoQoS），测量进程退出该节流后同机达标（图表 p95 9.6 ms、表格 p95 6.9 ms，门槛 ≤20 ms 不变），见下文 [A 集中技术审核](#a-集中技术审核2026-11-09)。按用户指示（[D-32](../decisions/confirmed-decisions.md)），凭据与发布签名私钥改为文件，不再使用 Windows 凭据管理器。

门禁只真实安装、升级、卸载测试身份 `delta-test` / `delta-dev-test`。开发者的 `delta` / `delta-dev` 安装目录文件哈希、HKCU 卸载键、开始菜单快捷方式和数据目录树在门禁前后逐项一致。在线更新只做了 `127.0.0.1` 回环演练（`M3-A-08`），**不是真实 GitHub 发布**。真实发布（`M3-L-01`，需 ACT-10/ACT-11）与人工检查 H-03/H-04（`M3-M-01/02`）保持 not-run。下文“本轮实际验证”及之前各节为 B 自检记录（指纹 `95003b7d…`），A 复跑结果以 A 审核一节为准。

## Spike 结论（第 4 节）

| Spike | 实际验证 | 结论与发现 |
| --- | --- | --- |
| M3-K01 Inno 按用户安装 | Inno Setup 7.1.1（winget `JRSoftware.InnoSetup.7`，按用户安装）编译的测试身份 `setup.exe` 以 `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART` 安装到 `%LOCALAPPDATA%\Programs\delta-test`，不需要提权；HKCU 卸载键、开始菜单快捷方式和 `VERSIONINFO` 齐全；静默卸载移除程序文件、键与快捷方式，数据保留 | 通过，W02 继续。① Inno 卸载分两阶段：第一阶段复制 `_unins*.tmp` 后退出，由第二阶段删除 `unins000.exe`。放在 kill-on-close 作业里会被截断，导致文件残留。改为在作业之外启动卸载器，以 HKCU 键消失、`unins000.exe` 删除且日志出现 `_unins-done.tmp` 作为完成条件（上限 180 s）。② `InitializeSetup` 早于内置 `AppMutex` 检查，因此模板在 `InitializeSetup` 中先请求保存退出，再交给 `AppMutex` 兜底 |
| M3-K02 AppMutex 与控制管道 | 运行中的测试身份收到 `quit-for-update` 后保存待写笔记与布局，以 0 退出；忙碌时回答 `busy`，安装器中止且不改动安装；不响应控制的应用不被强杀，安装器超时放弃 | 通过，W03/W04 继续；未使用 `WM_CLOSE` 备选。控制管道在 `Drop` 时用单次 poke 唤醒监听线程，存在竞态（约 1/16 卡住）。改为有界重试，连接失败时调用 `DisconnectNamedPipe`，之后连续 60 次通过 |
| M3-K03 Ed25519 与回环更新 | `ed25519-dalek` 2.2.0 签名/验签；回环源返回 GitHub latest release 形态的 JSON，第一次下载在 1/3 处中断，之后 Range 续传、验签、备份，在作业之外启动安装器，静默升级后重启 | 通过，W04 继续；续传正常，未触发整包重下 |
| M3-K04 静默安装不抢焦点 | 门禁全程每 200 ms 采样前台窗口和门禁自有进程（测试身份、安装/卸载器、ISCC 等）的可感知窗口 | 通过，未使用 `CreateDesktop`。Inno 的 `Exec` 会创建一个 0×0 的 `TWindowDisabler-Window`（带 `WS_VISIBLE`，用户看不到）。哨兵只把 `WS_VISIBLE`、矩形非空且未被 DWM cloak 的窗口计为可感知窗口。全程可感知窗口数 `0`，前台未被门禁进程占用 |

**门禁中发现的分离启动缺陷（已修复）**：`M3-A-06` 前两轮失败，表现为“应用未重启”。事件日志显示重启后的进程已就绪，但门禁检查时它已按 200 s 上限退出。原因是 `CreateProcessW` 开启了句柄继承，`spawn_detached_app` 启动的应用继承了 `install-dev` 自身可继承的 stdout 管道。读取方于是要等应用退出才能收到 EOF，这同样会让开发者执行 `delta-cli install-dev | tee …` 时挂起。修复：分离启动前清除本进程标准句柄的继承标志（`Stdio::inherit()` 不受影响，因为 std 会为子进程显式复制句柄）。新增回归测试 `process::tests::m3_a06_detached_launch_does_not_hold_the_callers_stdout_open`：不修复时调用方等待 15.2 s 后失败，修复后 0.03 s 通过。A-06 的存活判断也从互斥量检查改为通过重启后应用自己的控制管道往返（`focus` 返回新版本）。

**Spike 事故（D-25，已处置）**：早期在 Git Bash 中手动运行 K01 spike 安装器时，MSYS 路径转换把 `/VERYSILENT` 改写成了路径。安装器于是按非静默方式运行，并停在可见的语言选择窗口。B 结束了这个自建 spike 安装器进程（未安装任何内容，不涉及应用或用户数据），随后做了三项修正：① 模板加 `ShowLanguageDialog=no`（门禁 M3-G-06 核对）；② 之后的手动命令一律带 `MSYS_NO_PATHCONV=1`；③ 门禁只经 Rust `run_quiet_command` / `spawn_detached_app` 直接传参启动安装器，不经过 shell。此后门禁内无可感知窗口。

## 工作包与实现

| 工作包 | 实现 | 验证入口 |
| --- | --- | --- |
| M3-W01 版本、身份与资源 | `delta-app::identity`：四个身份 delta / delta-dev / delta-test / delta-dev-test，各有独立 AppId GUID、互斥量 `<id>-instance`、管道 `\\.\pipe\<id>-control`、AUMID 与数据根；测试身份不能编译为正式版。`version`：标签 `0.1.N`，开发/测试为 `<下一版本>-dev.<UTC 14 位>`，非法和降级版本拒绝。`DELTA_VERSION` / `DELTA_IDENTITY` / `DELTA_SOURCE_FINGERPRINT` 注入 build-info。`VERSIONINFO` 与 Δ 图标（`scripts/generate_icon.py`）。`instance`：单实例，以及只接受本身份的控制管道（`focus` / `quit-for-update`）。设置页“关于/更新” | M3-A-01、M3-A-02、M3-A-03（安装后的 `VERSIONINFO`） |
| M3-W02 Inno 安装包 | `crates/delta-cli/installer/delta.iss`：中英文；AppId、AppMutex、`SetupMutex`、`CloseApplications=no`；快捷方式；HKCU 卸载；不删数据。ISPP `Ver` 检查只接受 7.1.x。`package-build` 的流程是：静态 CRT 构建，核对 build-info 与 PE 导入，随附许可/通知/README/build-info，用 ISCC 编译，写入带 SHA-256 的 sidecar；构建期间源码指纹变化时拒绝。M2 便携安装迁移见 S-34 | M3-A-03、M3-A-04、M3-A-05、M3-G-06 |
| M3-W03 开发版本机更新 | `install-dev [--package] [--test-identity]`：只接受 delta-dev 包（门禁用 `--test-identity` 换成 delta-dev-test），拒绝其他身份和不更高的版本。运行中的应用先保存退出，30 s 内未退出或回答忙碌则放弃，不改动安装。静默安装后，在作业之外按原参数重启 | M3-A-06 |
| M3-W04 正式版联网更新 | `delta-app::update`：只用 HTTPS 与白名单主机（github.com / objects.githubusercontent.com 等），重定向逐跳校验，有大小上限，Range 续传；用内置公钥验签，签名绑定通道和版本，拒绝降级。启动安装前做一致性备份，备份失败则不安装；随后在作业之外启动安装器。工具栏显示“更新就绪”，设置页提供检查/开关（Q-05 默认开启）。离线时本地功能不受影响。只有 `delta-test` 读取 `DELTA_UPDATE_SOURCE` / `DELTA_UPDATE_PUBLIC_KEY`（回环演练用） | M3-A-07、M3-A-08 |
| M3-W05 发布产物与演练 | `release-keygen`（A 审核按 D-32 改为生成仓库外密钥文件 `%APPDATA%\delta-release\update-signing.key`，拒绝覆盖与写进仓库，只打印路径与公钥；原 `release-key-export` 删除，备份即离线复制该文件）；`release-build`：缺私钥时在构建前以 `RELEASE_KEY_MISSING` 失败；正式版私钥必须与内置公钥一致；产出 `setup.exe`、`latest.json`、`.sig`、`public-key.txt`。`update-test-source` 只服务 channel 为 `test` 的清单 | M3-A-09、M3-A-08 |
| M3-W06 门禁、文档与回归 | `verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]`；README 安装与 SmartScreen 说明；USER-ACTIONS 中 H-03 改为安装器步骤，新增 H-04 测试身份体验步骤，补充 ACT-10/ACT-11 命令；M1/M2 回归 | M3-G-01～06、M3-A-10 |

## 本轮实际验证

最终源码指纹 `95003b7df43ad0500181a8f0afb8288c73255327471b0596b74f7763e5899eb2`，均在 Windows 11 本机以低优先级静默运行：

| 检查 | 结果 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | passed（门禁 `M3-G-01～04` 实跑，另在序列开头单独执行 fmt/clippy） |
| `cargo test --workspace --locked` | passed：103 passed、0 failed、2 ignored（live 探针与显式截图）；门禁 `M3-G-05` |
| `verify-stage --stage M1 --plan-version 2.6` | passed：22 passed、5 项 extension/live not-run。本序列第一次运行中锁探针 `M1-A-15` 冷启动超时一次（TEST-02），同指纹立即重跑通过 |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke` | failed：25 passed、5 not-run（人工 H-01～H-03、live、真实模型），仅 `M2-A-20` / `M2-K02` 帧性能未达标（见下）；首屏 p95 229 ms、导航 p95 43.3 ms 达标；0 可见窗口。仅 `--ui-smoke`（不带 `--ui-bench`）时 overall 为 not-run |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | failed，仅 `M3-A-10`（M2 回归前置条件）与随之绑定的 `M3-K04`；其余必需项 passed（日志 `.local/reports/m3/logs-3136-1794199760499/`） |

M3 用例（报告 `.local/reports/m3/verify-stage-report.json`，日志在同目录 `logs-*`）：

| 用例 | 结果 | 实际证据（摘要） |
| --- | --- | --- |
| M3-G-01～05 | passed | check_docs、`git diff --check`、fmt、clippy、工作区测试实跑 |
| M3-G-06 | passed | `.iss` 指令齐全、无数据删除；四个身份的 AppId、互斥量、管道、可执行文件与 AUMID 互不相同；正式版公钥尚未内置（ACT-10），正式版更新校验失败关闭 |
| M3-A-01 / A-02 / A-07 | passed | 版本推算与身份隔离、单实例与跨身份管道拒绝、签名/降级/传输策略与回环续传/重定向拒绝的单元测试 |
| M3-A-03（K01） | passed | 两个测试身份静默按用户安装（setup 2.9 s，无提权），HKCU 键、Inno 7.1.1、快捷方式、build-info 与 `VERSIONINFO [0,1,1,0]` 正确，安装后隐藏窗口 `--ui-smoke` 通过；静默卸载约 1.0 s 移除文件、键与快捷方式，数据根与未知用户文件保留 |
| M3-A-04（K02） | passed | 同步忙碌时应用回答 busy，安装器 36.6 s 后中止，版本不变、应用继续运行；不响应控制的应用不被强杀，安装器 66.4 s 后放弃；空闲时 quit-for-update 保存待写笔记与布局，以 0 退出，5.6 s 内完成升级；账户、笔记、附件与设置保留 |
| M3-A-05 | passed | M2 布局（7 个清单文件，1 个被修改，另有 1 个未知文件）在 3.2 s 内迁移：已核对的文件与两份清单移除，`README.txt` 改名为 `README.txt.m2-modified` 保留，`user-notes.txt` 保留，两者都写入安装日志 |
| M3-A-06 | passed | 拒绝 delta-test 包（`INSTALL_DEV_IDENTITY`）和同版本包（`INSTALL_DEV_DOWNGRADE`），安装与运行均不变；运行中的应用保存退出、静默升级，按原参数重启后经自身控制管道以新版本回答 focus（持有互斥量），随后按请求退出；资料库保留 |
| M3-A-08（K03） | passed | 回环源提供 `latest.json`、`.sig` 和在 1/3 处中断的安装包；应用 Range 续传，忙碌时只提示不安装，空闲后验签、备份、在作业之外启动安装器并退出；静默升级后重启，资料库保留，备份可恢复。**仅回环演练，不是 GitHub 发布** |
| M3-A-09 | passed | 无私钥时在构建前以 `RELEASE_KEY_MISSING` 失败且无产物；测试命名空间生成密钥并拒绝覆盖；`release-build` 产出的安装包、`latest.json` 与 `.sig` 可用生成的公钥验证，换通道验证失败；无 MSVC 运行库导入；私钥不出现在 4982 个日志/报告文件中 |
| M3-A-10 | failed | 低优先级；0 可见/前台窗口；开发者安装、卸载键、快捷方式与 8 个数据目录不变；无临时残留。只有“M2 回归 overall passed”这一前置条件未满足（M2-A-20，见下） |
| M3-K04 | failed（绑定 A-10） | 静音本身达标：全程 0 个可感知窗口，前台未被门禁进程占用 |
| M3-M-01 / M-02 / L-01 | not-run | H-03、H-04 人工检查；真实 GitHub 发布需 ACT-10、ACT-11 |

**M2 帧性能对照（TEST-03）**：同机（RTX 4070 SUPER，驱动 621.14，缩放 1.5，逻辑窗口 918.7×596）、同测量口径，均为 release 隐藏窗口、低优先级，各 300 帧：

| 二进制 | 运行 | 图表 p95 | 表格 p95 | 导航 p95 |
| --- | ---: | ---: | ---: | ---: |
| M2 审核时（`3c6a16dc`，M2 报告记录） | 1 | 18.2 ms | 7.8 ms | 32.1 ms |
| 归档 M2 最终包 `3c6a16dc`（本次现场复测） | 3 | 36.3～43.2 ms | 24.0～25.8 ms | 42.0～43.4 ms |
| M3 构建 `95003b7d`（门禁 + 现场复测） | 1 + 5 | 34.4～46.7 ms | 22.2～25.4 ms | 41.5～43.8 ms |

基线包与 M3 构建在误差范围内一致，GPU 复测时空闲于 P8；首屏（约 230 ms）与取消延迟不变。M3 未修改图表与表格组件（`chart_canvas.rs`、`components.rs`）；`workbench.rs` 的改动是工具栏“更新就绪”、设置页“关于/更新”和生命周期/验证常驻模式。

## 与计划的偏差与补充（未缩减范围）

- **S-34 迁移的修改文件**：清单中哈希不符的文件不原地保留，而是改名为 `<名称>.m2-modified` 保留，避免被新版本文件覆盖；未知文件原样保留，两类都写入安装日志。卸载新版本时这些文件也保留。`M3-A-05` 期望已按此改写。
- **`package-build` 拒绝正式版身份**：正式版只能经 `release-build`（需签名私钥）产出，避免出现未签名的“正式版”安装包；M2 便携 ZIP 构建改名为 `package-portable-build`。
- **`install-dev --test-identity`**：门禁和 H-04 用它把目标换成 delta-dev-test。不带该参数时只认 delta-dev，从不触碰正式版。
- **验证常驻模式 `--verification-resident`**：仅测试身份可用，以隐藏窗口运行，事件写入 stdout 与 `<data_dir>/verification-events.jsonl`（重启后的进程没有可读的 stdout），并设硬性最长运行时间，到期后自行保存退出。
- **窗口哨兵细化**：在 D-25 的“可见窗口”中排除零尺寸与被 cloak 的窗口（见 K04），其余规则不变。
- **`install-dev` 重启方式**：经 `spawn_detached_app` 在门禁作业之外启动，不随调用方作业结束，参数恢复为原运行参数。
- **`CloseApplications=no`**：计划写的是 CloseApplications，实际关闭了 Restart Manager 关闭应用的功能，因为它可能在应用未保存时结束进程。改为在 `InitializeSetup` 中经本身份 CLI 发送 `control quit-for-update`（保存后退出，忙碌时中止安装），再由 `AppMutex` 兜底等待，满足不强杀要求。
- **Inno 版本锁定**：`ISCC.exe` 没有版本资源，因此由 `.iss` 内 ISPP `Ver` 检查锁定 7.1.x（OSS-12）。
- **命令补充**：`update-test-source`（回环源）、`control focus|quit-for-update`（只能指向本身份或测试身份）、隐藏命令 `__migrate-portable`（安装器内迁移助手）。
- **测试私钥**（A 审核按 D-32 修改）：`release-build --test-identity --key-file <一次性密钥>` 只能用于测试身份，拒绝正式密钥文件；环境变量私钥 `DELTA_RELEASE_SIGNING_KEY` 只对正式版生效。

## 验证边界与接续

- **not-run**：`M3-M-01`（H-03 干净机器安装两通道、SmartScreen）、`M3-M-02`（H-04 正式版更新体验）、`M3-L-01`（真实 GitHub 发布 v0.1.1，需 ACT-10 生成并备份私钥、内置公钥入库，以及 ACT-11 授权）。正式版内置公钥尚未写入（ACT-10），因此正式版更新校验在拿到公钥前一律失败关闭，属预期行为。
- **回环演练 ≠ 发布**：`M3-A-08` 的检查、下载、续传、验签、安装和重启都针对本机 `127.0.0.1` 测试源与测试私钥，不证明 GitHub 网络、真实 Release 资产或正式私钥可用。
- **SmartScreen**：Q-04 默认不购买代码签名证书，安装时会出现“未知发布者”提示；README 已说明。
- **已知不稳定测试（未修改，登记 TEST-01）**：M2 既有测试 `test_m2_a14_ai_review_ac02_qa_evidence_number_verification_and_injection_defense` 在约 40 次全量并行运行中出现 1 次 `CREDENTIAL_UNAVAILABLE`（Windows 凭据管理器瞬时失败），单独重跑通过。A 审核按 D-32 去掉凭据管理器后，该失败来源已不存在（TEST-01 关闭）。
- **依赖**：第三方包 897 个（M2 为 885），许可收集 0 errors；`Cargo.lock`（LF 规范化）SHA-256 `cd374a417f4e910ca487d04d3d186e41afbde9746af91d14d41f32343ca22b58`；版本锁定见[技术选型](../engineering/technology-selection.md)。

## A 集中技术审核（2026-11-09）

角色：Agent A；待审提交 `9199fe0b1abde44ee14e60bcfb728f35f1dd53c5`（父 `5f30f6e`）。A 独立重算 B 声明的源码指纹 `95003b7d…` 一致，工作区仅有未跟踪 `workers/`。对照计划第 5、6 节逐项复核实际 diff：

| 审核点 | 结论与依据 |
| --- | --- |
| 四个身份隔离 | `identity.rs` 中 AppId、安装目录、可执行文件、互斥量 `<id>-instance`、管道 `\\.\pipe\<id>-control`、AUMID 两两不同（单元测试与 M3-G-06）；测试身份编译为 dev 通道但数据、文档与凭据根用自身名称，且 `ensure_channel_data_isolation` 额外禁止访问 `delta-dev` 根；`.iss` 固定 `DefaultDirName={localappdata}\Programs\<id>`、`UsePreviousAppDir=no` |
| 数据保留、备份失败不安装 | 模板无 `[UninstallDelete]`、不引用 `{userappdata}`/`{userdocs}`；应用内更新在 `backup_and_launch_installer` 中先一致性备份，失败返回 `BackupFailed`，不启动安装器；A-03/A-04/A-08 实测数据保留 |
| 不强杀 | `CloseApplications=no`、`RestartApplications=no`；`InitializeSetup` 经已安装 CLI 发送 `quit-for-update`，忙碌或超时即中止且不改动；`AppMutex` 兜底；全仓新增代码只有门禁测试身份清理用到 `taskkill`，模板禁止出现 |
| 更新传输与签名 | 只接受 HTTPS＋白名单主机（无端口、无内嵌凭据），重定向逐跳校验，测试身份才允许 `127.0.0.1` HTTP；签名规范串绑定通道、版本、文件名与 SHA-256；缺签名、跨通道、非更高版本、超限均拒绝；安装前再次验签；正式版内置公钥为空时失败关闭 |
| 私钥边界 | 私钥只在密钥文件或正式版环境变量中；`release-build` 先读密钥再构建，缺失时无产物；正式版私钥必须与内置公钥一致；A-09 扫描日志与报告无私钥 |
| 分离启动 | `spawn_detached_app` 以 `CREATE_BREAKAWAY_FROM_JOB` 脱离调用方作业（作业允许 breakaway），安装器不允许留在作业内；启动前清除本进程标准句柄继承标志，`Stdio::inherit()` 仍由 std 显式复制，回归测试覆盖 |
| M2 便携迁移 | 哈希一致的清单文件删除，修改过的改名 `*.m2-modified` 保留，未知文件保留并写日志 |

**A 修正（已 amend 进同一阶段提交）**：

1. **TEST-03 根因与测量口径（M2-A-20/K02）**：同一 release 二进制、同为低优先级，退出执行速度节流前后图表 p95 39.3 ms → 8.6 ms、表格 22.3 ms → 5.9 ms；改普通优先级则无改善（36～38 ms）。Windows 11 对隐藏/后台窗口进程施加 EcoQoS 节流，测到的是系统后台调度而不是用户在前台窗口看到的帧耗时；节流下还偶发“隐藏窗口尚无有效的绘制尺寸”启动失败。修正：`--ui-bench` 测量进程调用 `opt_out_of_execution_speed_throttling`（仍为低优先级），报告记录 `power_throttling`，M2 门禁只接受 `execution_speed_opt_out` 样本；门槛不变（[质量门禁](../dev-rules/quality-gates.md#foreground-quiet)）。
2. **M2 便携迁移的部分执行**：原实现逐个删除，遇到正在运行的 M2 应用占用的文件时已删除一部分，却报告“nothing was changed”。改为先分类并对每个待删文件做写打开预检，任一被占用即 `MIGRATE_IN_USE` 中止且不改动任何文件；新增占用场景单元测试。
3. **密钥导出先查后写**：改为 `create_new` 原子拒绝已有文件（随后该命令按 D-32 删除，`release-keygen` 同样使用 `create_new`）。
4. **安装包位置与产物清理（用户要求）**：`package-build` 固定把 `setup.exe` 写到仓库根 `dist/`，成功后删除同一身份的旧安装包，每个身份只留最新；构建日志在 `.local/reports/m3/packages/`；每次构建的许可暂存目录结束即删除（原先每次留下约 25 MB）。`install-dev` 默认取 `dist/` 最新开发版包，日志写 `.local/reports/m3/install-dev/`。新增 `scripts/clean_artifacts.py`（默认只列出，`--apply` 删除）。dev profile 改为工作区只保留行号表、依赖不带调试信息：`target/` 从 32 GB 降到完整 clippy＋测试后约 3.5 GB，干净构建测试约 3 分钟。见[构建产物](../dev-rules/local-files-and-references.md#build-artifacts)。
5. **凭据改为文件（用户 D-32）**：应用凭据改为数据目录 `credentials/<命名空间>/` 下的 DPAPI 加密文件（命名空间、提供方、账户绑定进附加熵；原子写入；不可加密时拒绝），一致性备份与导出不含该目录，删除资料库随之删除；发布签名私钥改为仓库外 `%APPDATA%\delta-release\update-signing.key` 密钥文件，`release-keygen [--key-file]` 拒绝覆盖与写进仓库，`release-build --test-identity --key-file` 用一次性密钥，`release-key-export` 删除。移除 `keyring` 依赖（第三方包 897 → 895，`Cargo.lock` LF SHA-256 `b9031453fe66de57a9e49f90de4efeec5e31680c067888346c9a2746a77e1a01`）。凭据管理器中原本没有 `delta*` 条目，无需迁移。新增单元测试覆盖无明文落盘、换槽位/命名空间不能解密、损坏即报错；M1-A-13 跨进程读取改为凭据文件，M3-A-09 改为临时目录密钥文件并验证拒绝写进仓库。TEST-01（凭据管理器并发瞬时失败）随之关闭。
6. **mbx（jdx/mr-boxington）评估**：单检出与 Windows 复制恢复下收益有限，登记 `LOOP-05`，多工作树并行开发时再评估。

**A 复跑（最终指纹 `77dd43c8798e0b97b93f4a970a1d89b79bc3416f49e75c3142458afa91c09622`，Windows 11 本机，低优先级静默）**：

| 检查 | 结果 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo test --workspace --locked` | passed：104 passed、0 failed、2 ignored |
| `verify-stage --stage M1 --plan-version 2.6` | passed（5 not-run, 22 passed） |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` | passed；图表 p50/p95/p99 = 7.1/9.6/12.3 ms，表格 = 4.2/6.9/7.6 ms，首屏 p95 255 ms，导航 p95 40.9 ms；`power_throttling` = `execution_speed_opt_out`；0 可见窗口 |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | passed：M3-A-01～10、M3-G-01～06 全部 passed，M3-M-01/M-02/L-01 not-run；日志 `.local/reports/m3/logs-19004-1794224901779/` |

中间指纹 `8008ebc1…`（含修正 1～4、未含 D-32）也完整跑过一次三阶段门禁并全部 passed，报告留存于 `.local/reports/m3/a-review/`。

**收尾修订（2026-11-12，用户要求）**：只改文字——`crates/delta-cli/src/installer.rs` 一行注释与 D-24、D-26、D-32、`Q-02` 的措辞，去掉对仓库之外项目的提及。源码指纹因这行注释不再是上面的值；行为未变，复跑 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` 与 `python scripts/check_docs.py` 均 passed，三阶段门禁结果沿用上面的指纹。

**边界**：回环更新演练不是 GitHub 发布；`M3-M-01`（H-03）、`M3-M-02`（H-04）、`M3-L-01`（ACT-10、ACT-11）保持 not-run。帧性能结论只适用于退出节流的测量口径与本机环境（RTX 4070 SUPER、缩放 1.5）；用户在前台窗口使用时不受该节流影响。

## 接续

A 审核收敛，工程阻断为零。首选下一步：用户按 [USER-ACTIONS](../../USER-ACTIONS.md) `ACT-12` 授权推送本阶段分支并创建 PR。ACT-10（生成并备份签名密钥文件）、ACT-11（v0.1.1 发布）与 H-03、H-04 人工检查独立推进，不是推送/PR 的前提。
