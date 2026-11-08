# M3：Windows 安装包、版本与自动更新实施计划（delta / delta-dev）

版本 **1.0** · 2026-11-09 · 编制：Agent A · 执行：Agent B · 当前状态见 [执行状态](status.md)。本计划在 M2 v1.0（[PR #2](https://github.com/chialecode/delta/pull/2) squash 合并为 `main` `5f30f6e`，证据见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)）之上执行。按用户 2026-11-09 指示（[D-31](../decisions/confirmed-decisions.md)），先解决安装包问题，再进入 K 线训练（M4）。

## 1. 现状问题与目标

M2 交付的是“便携 ZIP＋`delta-cli package-install`”。它可以安装和卸载，但还不是用户可用的安装包：

- 没有 `setup.exe`，没有开始菜单快捷方式，“应用和功能”里没有卸载入口。
- 可执行文件没有图标和版本资源；两个通道的版本都固定为 `0.1.0`。
- 没有单实例，也没有升级路径：覆盖安装和正在运行的应用会冲突。
- 没有更新机制；H-03 干净机器检查仍未执行。

目标：

1. **真正的按用户安装包**：正式版 `delta`、开发版 `delta-dev` 各一个 `setup.exe`。无需管理员，两个通道可并存。安装后有开始菜单入口和“应用和功能”卸载入口；升级就地完成；卸载默认保留应用数据（[D-26](../decisions/confirmed-decisions.md)）。
2. **版本与身份**：正式版、开发版和测试身份的版本号可推算，并写进 build-info、Windows 版本资源和“关于”页；有 Δ 应用图标。
3. **开发版本机自动更新**：打包通过后，用一条显式命令把本机开发版升到新构建。运行中的开发版先保存再退出，装完按原状态重开；绝不触碰正式版。
4. **正式版联网更新**：从 GitHub Releases 检查和下载更新。安装包另有 Ed25519 分离签名，内置公钥校验通过后才安装；拒绝降级、篡改和非 HTTPS。安装前先做一致性备份。
5. **可验证**：`verify-stage --stage M3` 以独立的测试安装身份真实执行静默安装、升级、更新、卸载。开发者的正式版和开发版安装与数据不被触碰。

## 2. 开始基线、分支与 Git 规则

- 阶段父提交：`main` `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`（M2 squash）。当前分支 `codex/m3-installer-update`。本计划所在提交是父提交之后唯一的阶段提交；B 实现、A 审核修正与返工在 push 前 `git commit --amend -s` 进该提交（D-09、D-27），完整 SHA 以交接消息为准。
- 工作区保护：未跟踪 `workers/agent/` 不读取、不构建、不暂存。`.local/` 下 M1/M2 报告与安装包保留。
- 工具链：沿用 `rust-toolchain.toml` `1.99.1`；Windows 目标 `x86_64-pc-windows-msvc`＋静态 CRT（M2 已建立）。
- 远端：push、PR、合并、发布 GitHub Release、修改 Actions 白名单均需用户在 [USER-ACTIONS](../../USER-ACTIONS.md) 对具体动作授权。

## 3. A 已定案的阶段技术决定（B 不再重新选型）

M1 的 S-01～S-11、M2 的 S-12～S-24 继续有效。M2 的 S-23（cargo-packager NSIS，备选便携 ZIP）由 S-25 取代。

| ID | 决定 | 理由 / 退出条件 |
| --- | --- | --- |
| S-25 安装器 | **Inno Setup 7.1.x**（2026-10-28 发布 7.1.1，官方下载页推荐 7.x；许可为 Inno Setup License，允许含商用的使用与再分发，需保留版权声明）。只作构建期工具，运行期仍为单一 Rust 二进制（D-22）。B 锁定精确版本，记录安装包 SHA-256；构建机通过 `winget install JRSoftware.InnoSetup`（指定版本、按用户范围）或官方安装包安装。`.iss` 模板入库，由 `delta-cli package-build` 注入通道参数后调用 `ISCC.exe`，找不到或版本不符即失败，不静默退回 ZIP | GPUI 的来源项目 Zed 在 Windows 上用 Inno Setup，按通道分 AppId/AppMutex，`PrivilegesRequired=lowest` 按用户安装（`script/bundle-windows.ps1`、`crates/zed/resources/windows/zed.iss`，2026-11-09 查证）。未选：cargo-packager（最近发行 0.11.8 为 2025-11-27，构建时还需另取 NSIS）；cargo-wix/MSI（按用户 MSI 与并存更繁琐）；Velopack（固定安装到 `%LOCALAPPDATA%\<id>`，与 D-26 的安装与数据位置冲突） |
| S-26 通道身份 | 正式版：AppId GUID-R、显示名 `DELTA`、安装到 `{localappdata}\Programs\delta`、`delta.exe` / `delta-cli.exe`、互斥量 `delta-instance`、AppUserModelID `delta.app`。开发版：GUID-D、`DELTA Dev`、`Programs\delta-dev`、`delta-dev-instance`、`delta.app.dev`。**测试身份**（仅门禁）：`delta-test` / `delta-dev-test`，GUID 与目录、互斥量、数据根全部独立，数据根由门禁指定到随机临时目录。`PrivilegesRequired=lowest`，`ArchitecturesAllowed=x64compatible`，简体中文（默认）＋英文；开始菜单快捷方式必有，桌面快捷方式默认不勾选；卸载只删安装清单内文件和 HKCU 卸载键，永不删除 `%LOCALAPPDATA%\delta(-dev)` 应用数据与“文档”导出 | 两通道并存、数据隔离（D-26）；测试身份使门禁真实安装而不碰开发者已装版本 |
| S-27 版本 | 正式版 `0.1.N` 由 Git 标签 `v0.1.N` 推算（首个正式版 `0.1.1`）；开发版 `<下一个正式版号>-dev.<UTC yyyymmddHHMMSS>`；测试身份沿用开发版规则。版本由构建环境注入（`DELTA_VERSION`），写进 build-info JSON、Windows `VERSIONINFO` 和“关于”页；发布时不修改 `Cargo.toml`。比较使用 `semver` crate，同通道拒绝降级 | 发布可复现，开发版严格递增 |
| S-28 图标与资源 | Δ 标识 SVG 源入库（`crates/delta-desktop/assets/icon.svg`），多尺寸 `.ico`（16～256）一次生成后入库并记录生成方式；`build.rs` 用 `winresource`（MIT）写入图标与版本资源，同一图标用于安装器与快捷方式 | 开始菜单和任务栏可识别；不复制任何第三方品牌资产 |
| S-29 单实例与控制通道 | 每个通道一个命名互斥量（即安装器 `AppMutex`）＋本通道命名管道 `\\.\pipe\<通道>-control`，只接受同一用户会话的连接。命令只有 `focus` 与 `quit-for-update`：后者先等笔记自动保存与进行中的写入完成、保存布局，再退出并回执；忙于同步/导入/AI 时回执 `busy`，不强退 | 安装器 `CloseApplications` 不强杀；更新前可安全退出 |
| S-30 开发版本机更新 | `delta-cli install-dev [--package <setup.exe>]`：在已打包且门禁通过的产物上执行。若 `delta-dev` 在运行，经控制管道请求 `quit-for-update`，30 s 内未退出即放弃并报告，绝不强杀；然后以 `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-` 静默安装，校验已安装版本等于产物版本；之前在运行则重新启动。命令只接受开发版通道产物，遇到正式版或测试身份直接拒绝 | 安装后立即可用新构建；正式版不受影响 |
| S-31 正式版联网更新 | 更新服务位于 `delta-app::update`，只在正式版启用：源为 `https://api.github.com/repos/chialecode/delta/releases/latest`（只取非预发布），资产为 `delta-setup-<版本>-x64.exe`、`latest.json`（版本、文件名、大小、SHA-256、发布时间）与 `.sig`。**校验**：Ed25519（`ed25519-dalek` 2.x，BSD-3-Clause）对规范串 `delta-update-v1\n<channel>\n<version>\n<file>\n<sha256>` 的分离签名，公钥内置；只允许 HTTPS，重定向只跟随 `github.com`、`api.github.com`、`objects.githubusercontent.com`、`release-assets.githubusercontent.com`；缺签名、签名不符、跨版本/跨通道签名、版本不高于当前、大小或哈希不符一律拒绝。**下载**：存放 `%LOCALAPPDATA%\delta\updates`，HTTP Range 续传，大小上限 512 MiB，后台低优先级。**应用**：顶部工具栏显示“更新就绪”；用户确认后检查忙碌状态，执行现有一致性备份，重新验签，以静默参数启动安装器并退出；安装器按 `AppMutex` 等待退出，完成后重启。启动时最多每 24 h 检查一次，设置页可关闭自动检查、手动检查并查看当前版本与来源（默认开启，见 USER-ACTIONS `Q-05`） | 无代码签名证书时也能保证更新完整性与来源（S-33）；不做块级差分，整包下载 |
| S-32 发布产物 | `delta-cli release-build --version 0.1.N`：独立 release 构建→PE 导入检查→Inno 打包→SHA-256→Ed25519 签名→`latest.json`，输出到 `.local/reports/m3/release/`。私钥只从仓库外密钥文件 `%APPDATA%\delta-release\update-signing.key`（A 审核按用户 [D-32](../decisions/confirmed-decisions.md) 由凭据管理器改为文件）或环境变量读取，不入库、不写日志；首次生成密钥由 B 在 W05 实现命令，密钥备份交给用户（`ACT-10`）。创建标签和 GitHub Release 由 `gh release create` 执行，需用户对具体版本授权（`ACT-11`）。每周 CI 自动发布依赖 Actions 白名单变更，本阶段不做（`LOOP-04`） | 本地可完整演练；远端发布只在授权后 |
| S-33 代码签名 | 本阶段不做 Authenticode 签名，SmartScreen 提示如实写入安装说明与 README；Ed25519 只保证更新完整性，不消除 SmartScreen 提示。是否购买证书见 `Q-04` | 没有证书时不阻塞交付 |
| S-34 M2 便携安装迁移 | 安装器在 `[Code]` 中检测目标目录的 M2 `install-manifest.json`：只删除清单内且哈希一致的文件和两个清单本身，其余文件保留并在日志中列出；之后由安装器接管。`package-install` / `package-uninstall` 改为仅用于测试和迁移诊断，不再出现在用户说明中 | 已按 M2 方式安装的用户可无损升级 |
| S-35 测试与前台无干扰 | `verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]`：默认只做离线单元与契约检查（版本推算、签名、降级、URL 白名单、`.iss` 生成）；`--installer` 构建测试身份安装包，真实静默安装到 `Programs\delta-test*`、启动已安装二进制的 `--build-info` 与 `--ui-smoke`（隐藏窗口）、v1→v2 升级保留数据、经控制管道的 `quit-for-update`、用 `127.0.0.1` 回环 HTTP 源演练联网更新（只有测试身份允许回环 HTTP，正式身份由单元测试证明会拒绝）、卸载后核对文件/HKCU 键已移除而数据保留，最后清理测试数据根。全程 below_normal、不弹窗、不激活窗口；安装器不显示向导（`/VERYSILENT`）。M2、M1 回归继续执行 | D-25；不触碰开发者的正式版/开发版安装与数据 |

### 3.1 范围与不做项

- 范围：NFR-07、新增 NFR-11（安装、升级与更新），验收 AC-27（更新为安装器口径）与新增 AC-30。
- 不做：Authenticode 签名（`Q-04`）、每周 CI 构建与自动发布（`LOOP-04`）、块级差分更新、macOS/Linux 安装包、开发版联网更新。

## 4. 技术门槛（Spikes）与继续条件

| ID | 最小实际验证 | 通过后直接继续 | 失败或异常分支 |
| --- | --- | --- | --- |
| M3-K01 Inno 按用户安装 | 测试身份 `.iss` 以 `/VERYSILENT` 安装到 `Programs\delta-test`，不弹 UAC、不出现窗口；HKCU 卸载键存在，静默卸载后移除 | W02 继续 | 7.1.x 有阻断问题时改用同期 6.7.x 并记录；仍不行退回 S-23 便携 ZIP，并在报告中写明未达成项 |
| M3-K02 AppMutex 与控制管道 | 运行中的测试身份应用收到 `quit-for-update` 后保存并退出；安装器 `AppMutex` 检测到运行时等待而不强杀 | W03/W04 继续 | 管道在隐藏窗口测试中不可用时改用窗口消息 `WM_CLOSE`＋保存钩子，仍须先保存再退出 |
| M3-K03 Ed25519 与回环更新 | `ed25519-dalek` 2.x 签名/验签；回环 HTTP 源完成检查→Range 续传→验签→静默升级→重启 | W04 继续 | 续传异常时整包重下，上限不变 |
| M3-K04 静默安装不抢焦点 | 门禁期间可见窗口数 0、前台窗口不变（安装器静默、`--ui-smoke` 隐藏） | W06 继续 | 静默安装仍出现窗口时，改由 `CreateDesktop` 隔离桌面执行并记录 |

## 5. 工作包、依赖与验收

依赖顺序：**W01 → W02 → W03 / W04 并行 → W05 → W06**。先打通最薄切片：W01 版本资源＋W02 测试身份静默安装/卸载（M3-K01）。

| 工作包 | 产出 | 正常路径验收 | 失败路径验收 | AC |
| --- | --- | --- | --- | --- |
| M3-W01 版本、身份与资源 | 版本推算（标签/开发/测试）、`DELTA_VERSION` 注入、build-info、`VERSIONINFO`＋图标（S-27/S-28）、通道身份常量（S-26）、单实例与控制管道（S-29）、设置页“关于”（版本、通道、源码指纹、数据目录） | 同一源码构建出的正式/开发/测试身份版本号符合规则；第二个实例只唤起已有窗口 | 非法标签、降级版本构建失败；跨通道管道连接被拒 | AC-27、AC-30 |
| M3-W02 Inno 安装包 | `.iss` 模板（中英文、AppId/AppMutex/CloseApplications、快捷方式、HKCU 卸载、保留数据）、`package-build` 输出 `setup.exe`、包内许可/通知、PE 导入检查沿用、M2 便携安装迁移（S-34） | 两通道 `setup.exe` 并存安装；升级保留资料库、笔记与设置；卸载保留数据 | 正在运行时安装等待保存退出而不强杀；迁移遇哈希不符文件保留并报告 | AC-27、AC-30 |
| M3-W03 开发版本机更新 | `delta-cli install-dev`（S-30） | 运行中的开发版保存退出→静默安装→按原状态重启，版本与产物一致 | 30 s 未退出则放弃且不改动安装；拒绝正式版/测试身份产物 | AC-30 |
| M3-W04 正式版联网更新 | `delta-app::update`（检查、续传、验签、拒绝降级、备份、应用）、工具栏“更新就绪”提示、设置页更新区（S-31） | 回环源演练：检查到新版本→下载→验签→备份→静默升级→重启后版本正确且数据完整 | 篡改、缺签名、跨版本签名、降级、非 HTTPS、非白名单重定向、超大文件均拒绝并显示原因；离线时本地功能不受影响 | AC-30 |
| M3-W05 发布产物与演练 | `delta-cli release-build`、签名密钥生成/读取命令（S-32）、本地发布演练（产物＋回环源升级测试身份） | 产物齐全、签名可验证、`latest.json` 与文件一致 | 缺私钥时明确失败，不生成未签名的“发布”产物 | AC-30 |
| M3-W06 门禁、文档与回归 | `verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]`（S-35）、用户安装说明与 SmartScreen 说明、H-03 改为安装器步骤、M2/M1 回归 | 门禁通过并绑定源码指纹；M2 v1.0 与 M1 v2.6 回归通过 | 未运行的安装/在线项保持 not-run | AC-27、AC-30 |

用例清单见 [m3-stage-cases.json](m3-stage-cases.json)（`automated` / `manual` / `live`）。B 可以拆分或补充用例，但不得删除已有用例覆盖的需求/AC。

## 6. 不得缩减项

1. 正式版与开发版的安装目录、AppId、互斥量与数据根完全隔离；门禁只用测试身份，绝不安装、升级或卸载开发者的正式版/开发版。
2. 卸载与升级永不删除应用数据和用户导出；升级前的一致性备份失败时不安装。
3. 安装器与更新器都不强杀进程，先保存再退出；忙碌时由用户决定。
4. 更新只接受 HTTPS、白名单主机、内置公钥验签通过且版本更高的安装包；私钥不入库、不进日志。
5. 静默与前台无干扰（D-25）；离线/回环演练与真实 GitHub 发布分开声明。
6. M1 v2.6 与 M2 v1.0 全部验收回归保持通过。

## 7. 真正依赖用户的事项（不阻塞工程）

- `Q-04`：是否购买 Authenticode 代码签名证书（默认不买，安装时有 SmartScreen 提示）。
- `Q-05`：正式版默认是否自动检查更新（默认开启，可在设置关闭）。
- `ACT-10`：正式版更新签名私钥的备份与保管（B 实现生成命令后执行，首次公开发布前完成）。
- `ACT-11`：首个正式版 `v0.1.1` 创建标签与 GitHub Release 的授权（A 审核通过后登记具体请求）。
- `H-03`（改为安装器步骤）与新增 `H-04`（正式版更新体验）：人工检查，可顺延。

## 8. 检查命令

| 命令 | 状态 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --locked` | 已存在 |
| `cargo run -p delta-cli -- verify-stage --stage M2 --plan-version 1.0 [--ui-smoke] [--ui-bench]`、`--stage M1 --plan-version 2.6` | 已存在（回归） |
| `cargo run -p delta-cli -- verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]` | 已建立（W06），结果见 [阶段报告](stage-report.md) |
| `delta-cli package-build [--identity …]`（Inno `setup.exe`）、`install-dev`、`release-build`、`release-keygen`（D-32 起为密钥文件，`release-key-export` 已删除）、`control`、`update-test-source` | 已建立（W02/W03/W05）；安装包固定输出到 `dist/`，每个身份只保留最新；M2 便携 ZIP 改名 `package-portable-build`，与 `package-install` / `package-uninstall` 仅用于测试和迁移诊断 |

报告与日志写入 `.local/reports/m3/`，脱敏摘要回写 `docs/delivery/stage-report.md`（B 建立，登记 `retireAfter: M3`）；结果绑定源码指纹，指纹范围扩展到 `.iss` 模板、图标资源与 M3 用例。

## 9. 结束标准与收尾

- **B 实现与自检完成**：W01～W06 全部可执行内容实现；`verify-stage --stage M3 --installer --ui-smoke` 必要检查 passed；spike 结论与失败分支如实记录；阶段报告、[执行状态](status.md)、受影响正本与[集中台账](../decisions/open-questions.md)已同步。
- **A 技术审核通过**：A 对照本计划、实际 diff 与证据，重点复验通道隔离、数据保留、不强杀、签名与降级拒绝、私钥边界；工程阻断为零后在 USER-ACTIONS 登记 push / PR 的具体授权请求。
- **产品验收**：H-03、H-04 与真实 GitHub 发布（ACT-11）独立推进，未测保持 not-run。

## 10. A → B 交接 prompt

```text
你是 DELTA 的 Agent B。读 AGENTS.md、docs/delivery/status.md 与唯一计划 docs/delivery/stage-plan.md（M3 v1.0），用例见 docs/delivery/m3-stage-cases.json。
分支 codex/m3-installer-update，阶段父提交 5f30f6e1ddf6383f25317d5c87acd21934e75ed9；完整 SHA 见交接消息。先核对 git status、父提交与现场，保留未跟踪 workers/agent/。
按第 5 节依赖顺序连续完成 M3-W01～W06，先打通 M3-K01 最薄切片（测试身份静默安装/卸载）；第 3 节 S-25～S-35 已定案，不重新选型，spike 失败按第 4 节分支处理并记录。门禁只用测试身份，绝不安装、升级或卸载开发者的 delta / delta-dev；遵守不得缩减项、D-25 静音、D-26 通道与数据隔离；回环演练与真实 GitHub 发布分开声明。
交付：建立 verify-stage --stage M3 并实跑（含 --installer --ui-smoke 与 M2/M1 回归），新建并登记 docs/delivery/stage-report.md，更新 status.md、受影响正本与台账，全部 git commit --amend -s 进当前 M3 阶段提交（父提交不变），给出新完整 SHA 与 B→A 审核 prompt。不 push、不操作 PR、不创建 GitHub Release，不自动创建其他 Agent 会话。
```
