# 用户待办

核对日期：2026-11-09。这里是需要你处理的事项的唯一入口，可以分次回复，也可以以后再做。开发继续使用已说明的可逆默认；未回复不等于授权，未测保持 not-run。Agent 的代码缺口和开源适配任务在 [工程事项](docs/decisions/open-questions.md)，不交给你补实现。

2026-11-09 M2 已按 `ACT-09` 授权推送、经 PR #2 squash 合并为 `main` `5f30f6e`，阶段分支已清理；按你的指示当前进入 **M3（Windows 安装包、版本与自动更新）**，唯一计划见 [M3 计划](docs/delivery/stage-plan.md) v1.0，阶段编号顺延见 [D-31](docs/decisions/confirmed-decisions.md)。M3 已完成 B 实现与 A 集中技术审核（工程阻断 0），等你对 **ACT-12（推送并创建 PR）** 授权；Q-04、Q-05 有默认值，不回复也不阻塞；ACT-10、ACT-11 与 H-03、H-04 可独立推进。

## 决定与真实条件

| 编号 / 状态 | 需要你做什么 | 推荐与当前默认 | 真正受阻动作 |
| --- | --- | --- | --- |
| Q-01 / resolved | 确认你最常用的美股券商和加密交易所 | **已确认（D-23，2026-11-07）**：常用平台为 **富途牛牛、老虎证券、IBKR、长桥、嘉信证券、东方财富、币安、欧易**，其中**使用最多（核心主力）为富途牛牛与欧易（OKX）**。M1 必做已调整为：① 加密现货同时内置 **欧易（OKX）公开现货 REST（`bar=1Dutc`）+ 币安公开行情镜像与归档**；② 账单导入必做覆盖 **富途牛牛 CSV、欧易（OKX）现货账单 CSV、通用 CSV、IBKR Flex XML、币安现货 CSV**（老虎、长桥、嘉信、东财交割单、IBKR Activity CSV 为 M1 扩展预设）；③ 美股/ETF 零配置由纯 Rust 直连 **Yahoo Chart v8 主 + 东方财富美股日线（`push2his`）核验**（配置 Token 时走 Tiingo 主 + Yahoo 核验），自动映射 `US.AAPL ↔ AAPL.US ↔ 105.AAPL ↔ NASDAQ:AAPL` | 已关闭；无阻塞 |
| Q-03 / resolved | 确认后续 M2 桌面界面的技术栈方向（基于本次 GPUI 复盘结论） | **已确认（D-21，2026-11-07）**：① M1 纯做 100% Rust 数据底座与 CLI（D-22 已彻底移除 Python）；② M2 继续使用 GPUI，摒弃旧 `0.6.6` 锁定，升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接按照长桥官方提供的 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系构建桌面工作台，并通过视口滑动窗口（Viewport Slicing）驱动交互式 K 线与回放面板 | 已关闭；无阻塞 |
| ACT-07 / resolved | 自行安装 Rust 开发环境（Visual Studio Build Tools + `rustup`） | **已复核通过（2026-11-07）**：本机已就绪 Visual Studio Build Tools 2026（`18.11.12328.437`，`MSVC 14.51.36231` `Hostx64/x64/cl.exe` 与 `link.exe`、Windows SDK `10.0.26100.0`）及 `rustup`（`stable-x86_64-pc-windows-msvc`，`rustc 1.99.1`、`cargo 1.99.1`、`clippy 0.1.99`、`rustfmt 1.10.0-stable`），最小编译与 MSVC 链接实测通过 | 已关闭；M1 构建阻塞已解除 |
| ACT-03 / open | （可选）提供富途牛牛、欧易（OKX）、IBKR、币安、老虎、长桥、嘉信的去标识导出 CSV/Flex 样本，或免费行情 API Token（Tiingo；扩展层 Alpaca / Longport） | 若只提供一部分，**优先富途牛牛与欧易（OKX）导出样本**：这两个是 M1 必做导入器，但没有公开格式规范，目前只能按帮助文档构造合成样本。真实样本放忽略目录 `.local/private-samples/`；Token 用 `delta-cli credential set --stdin` 录入，保存为本机数据目录里的加密文件（D-32），开发期也可设环境变量 `DELTA_TIINGO_API_KEY`；不要粘贴到聊天或提交 Git | 真实个人账户账单与 Tiingo 在线端到端验收；缺样本/Token 时 M1 先用免 Key 公共源与公开标准结构夹具验证，真实样本/Tiingo live 保持 not-run |
| ACT-01 / open（M2，可选） | 真实 AI 模型连接测试配置：协议（`openai-responses` 或 `openai-chat-completions`）、base URL、model ID、凭据与测试预算 | 不提供也不阻塞：M2 用受控回环服务对两种协议做全套自动验收，真实服务保持 not-run。提供时密钥只在设置页录入，保存为本机数据目录里的加密文件（D-32），不要粘贴到聊天或提交 Git | 用例 `M2-L-02`：你配置的协议/模型的真实可用性声明 |
| Q-02 / resolved | 首次对外分发前选择项目许可证 | **已确认（D-24，2026-11-07）**：采用 **Apache-2.0**，已添加根 [LICENSE](LICENSE)（标准原文）。B 在 M1 锁定依赖后生成传递许可清单，并检查与 Apache-2.0 分发的兼容性 | 已关闭；无阻塞 |
| Q-04 / open（M3，可选） | 是否购买 Windows 代码签名证书（Authenticode） | **默认不买**：安装时 Windows SmartScreen 会提示“未知发布者”，需点“更多信息→仍要运行”；更新包另用 Ed25519 签名保证完整性与来源。个人开发者证书每年约数百美元且需身份验证 | 不阻塞；只影响首次安装的提示 |
| Q-05 / open（M3，可选） | 正式版是否默认自动检查更新 | **默认开启**：启动时最多每 24 小时访问一次 GitHub Releases，只下载签名正确的更高版本，安装前需你确认；设置页可随时关闭或手动检查。开发版不联网检查，用 `install-dev` 更新 | 不阻塞 |

## 外部与远端操作

### ACT-06 / resolved（远端基线重建已完成；后续阶段推送另行按需授权）

- 已按你 2026-11-07 的明确授权，将旧远端仓库重命名为 `chialecode/delta-old`，并重新创建全新的公开远端仓库 `chialecode/delta`。
- 新远端 `origin/main` 与本地 `main` 均仅保留第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`（空 `README.md`），无任何旧 PR 或废弃历史；同时已恢复仓库合并设置（仅允许 squash merge、合并后自动删除分支）与 `main-protection` 规则集（active）。
- 文档治理基线与当前阶段分支 `codex/m1-data-foundation` 按你的指示仅保留在本地，暂不推送；后续何时推送文档基线或 M1 分支均需你另行明确授权。

### ACT-08 / resolved（M1 推送、PR、合并与分支清理，2026-11-08 已执行）

- 按你 2026-11-08 的授权执行：推送 `codex/m1-data-foundation` → 创建 [PR #1](https://github.com/chialecode/delta/pull/1) → 必需检查 `docs` 与 DCO 通过 → squash 合并为 `main` `0370b7e9103d0172a7ab47613ea253949d6a9b8b`（带 `Signed-off-by`，树与阶段提交 `475cbc8` 一致）→ 远端分支自动删除、本地分支删除。
- 推送前 A 发现仓库 Actions 白名单不含 `actions/setup-python`，已把 `docs.yml` 改为 runner 自带 `python3`，未改动任何远端设置。
- 本地旧 `main`（`27b26d1`，重建前旧远端上的 M0 命名合并）与新 `origin/main` 分叉，已备份为忽略目录 `.local/rebuild-backup-20261108/` 的 bundle 后对齐。
- 合并不代表产品已验收；M2 的推送/PR 需你另行授权（A 审核通过后在此登记新 ACT）。

### ACT-09 / resolved（M2 推送、PR、合并与分支清理，2026-11-09 已执行）

- 按你 2026-11-09 的授权执行：推送 `codex/m2-desktop-workbench` → 创建 [PR #2](https://github.com/chialecode/delta/pull/2) → 必需检查 `docs` 与 DCO 通过 → squash 合并为 `main` `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`（树与阶段提交一致）→ 远端分支自动删除、本地分支删除。未发布、未改远端设置；合并不代表产品已验收。

以下为授权前的登记原文：

- **2026-11-09 用户授权**：“授权 ACT-09，push 到远端然后 CI 没问题就 merge”。范围：推送 `codex/m2-desktop-workbench`、创建到 `main` 的 PR，必需检查（`docs`、DCO）全部通过后 squash 合并；不含发布与远端设置变更。执行结果见 [执行状态](docs/delivery/status.md)。

- **A 技术审核已收敛（2026-11-09）**：结论与证据见 [M2 阶段报告](docs/evidence/2026-11-09-m2-stage-report.md#a-集中技术审核2026-11-09)。本地分支 `codex/m2-desktop-workbench` 仅含一个阶段提交（父提交 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`，带 DCO 签名），最终完整 SHA 以交接消息和 `git rev-parse HEAD` 为准。
- **请求授权的具体动作**：① `git push -u origin codex/m2-desktop-workbench`；② 创建 PR：来源 `codex/m2-desktop-workbench` → 目标 `main`，标题 `feat(m2): desktop review workbench, evidence-checked AI review and Windows packaging (M2 v1.0)`，正文列明 M2-W01～W08 实际行为、A 审核修复（F-08 AI 数值校验绕过、F-09 图标资源/面板标题）、长桥风格界面重设计、实际检查与 not-run 边界（真实模型 ACT-01、个人账单 ACT-03、H-01～H-03）。
- **不包含**：合并、发布、改远端设置；这些需你另行授权。授权后 A 核对 CI（`docs`、DCO）与审查反馈。回复“ACT-09 授权推送并创建 PR”即可。
- 人工检查 H-01～H-03 不是本动作前提，可顺延；合并也不代表产品已验收。

### ACT-12 / pending（M3 推送与 PR，等待你授权）

- **A 技术审核已收敛（2026-11-09）**：结论与证据见 [M3 阶段报告](docs/delivery/stage-report.md#a-集中技术审核2026-11-09)。本地分支 `codex/m3-installer-update` 仅含一个阶段提交（父提交 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`，带 DCO 签名），最终被测源码指纹 `77dd43c8798e0b97b93f4a970a1d89b79bc3416f49e75c3142458afa91c09622`；完整 SHA 以交接消息和 `git rev-parse HEAD` 为准。
- **请求授权的具体动作**：① `git push -u origin codex/m3-installer-update`；② 创建 PR：来源 `codex/m3-installer-update` → 目标 `main`，标题 `feat(m3): Windows installers, versioning, single instance and signed updates (M3 v1.0)`，正文列明 M3-W01～W06 实际行为、A 审核修正（TEST-03 节流根因与测量口径、迁移预检、`dist/` 安装包与产物清理、D-32 凭据与签名私钥改为文件）、实际检查与 not-run 边界（H-03、H-04、真实发布 ACT-10/ACT-11）。
- **不包含**：合并、创建标签或 GitHub Release、改远端设置；这些需你另行授权。授权后 A 核对 CI（`docs`、DCO）与审查反馈。回复“ACT-12 授权推送并创建 PR”即可（若希望 CI 通过后直接 squash 合并，请一并写明）。
- 人工检查 H-03、H-04 不是本动作前提，可顺延；合并也不代表产品已验收。

### ACT-10 / pending（M3，首个正式版前；A 审核已通过）：生成并备份正式版更新签名私钥

- 命令已由 B 实现（M3-W05），按 D-32 用文件保存。在本机仓库根目录执行一次：`cargo run -p delta-cli -- release-keygen`。私钥写进仓库外的 `%APPDATA%\delta-release\update-signing.key`，终端只显示文件路径与公钥（`public_key_hex`）；文件已存在时命令拒绝覆盖，也拒绝写进仓库。
- 备份：把这个文件复制到离线介质（例如加密 U 盘）或密码管理器的附件里。私钥丢失后，已安装的正式版无法再验证新更新，只能手动下载安装。
- 把终端显示的公钥交给 Agent（公钥可公开，不要发私钥），由 Agent 写入 `crates/delta-app/src/release-signing-key.json` 并随下一阶段提交审核；`release-build` 会拒绝与内置公钥不一致的私钥。首次公开发布（ACT-11）前完成；不影响开发版与本地演练。

### ACT-11 / pending（M3，A 审核通过后登记具体请求）：首个正式版 v0.1.1 发布

- 动作：在 ACT-10 完成、内置公钥已提交后，用 `cargo run -p delta-cli -- release-build --version 0.1.1` 在 `.local/reports/m3/release/delta-0.1.1/` 生成 `delta-setup-0.1.1-x64.exe`、`latest.json` 与 `.sig`，再创建标签 `v0.1.1` 并用 `gh release create` 上传这三个文件。A 审核通过后在这里写明提交 SHA、文件哈希与命令，等你回复授权后执行；未授权前用例 `M3-L-01` 保持 not-run。门禁里的回环更新演练（`M3-A-08`）不是 GitHub 发布。

## 人工检查（M2/M3，B 交付安装包与检查步骤后执行，可顺延）

| 编号 / 状态 | 需要你做什么 | 说明 | 不影响 |
| --- | --- | --- | --- |
| H-01 / pending | 中文输入法检查：在笔记、搜索、导入字段映射中用拼音等输入法选词、组合态按回车 | 用例 `M2-M-01`；记录输入法名称与版本 | A 技术审核与远端授权请求 |
| H-02 / pending | 键盘、焦点、Esc、125%/150% 缩放、小窗口与涨跌配色检查 | 用例 `M2-M-02` | 同上 |
| H-03 / pending | 干净机器（虚拟机或 Windows 沙盒）用 `setup.exe` 安装正式版与开发版、离线运行、在“应用和功能”卸载一个通道 | M3 起为用例 `M3-M-01`（取代 `M2-M-03` 的便携包步骤）；验证无需管理员、两通道并存、卸载保留数据，记录 SmartScreen 提示。步骤见下文 H-03 | 同上 |
| H-04 / pending（M3） | 正式版更新体验：观察“更新就绪”提示、设置页“关于/更新”、忙碌时的确认和重启 | 用例 `M3-M-02`；最自然的时机是 v0.1.1 发布后再发布 v0.1.2 时；之前可按下文 H-04 用测试身份和本机回环源体验 | 同上 |

H-01、H-02 所用安装包和源码指纹以 [M2 阶段报告](docs/evidence/2026-11-09-m2-stage-report.md) 的产物为准（也可改用 M3 开发版安装包）；H-03、H-04 以 [M3 阶段报告](docs/delivery/stage-report.md) 为准。M3 安装包静态链接 CRT、检查普通/延迟 DLL 导入，并附带 `README.txt`、`build-info.json` 与全部第三方许可。

- **H-01**：在开发版日志页编辑正文/结构字段，输入中文并在候选组合态按 Enter；在市场搜索、笔记搜索和导入预设输入框重复。等待 1 秒或失焦后观察保存状态，重启核对已保存文字。记录输入法名称、版本与失败位置；任意拖拽字段映射编辑器尚未提供。
- **H-02**：Tab/方向键遍历六页、表格、图表控制和操作按钮；检查可见焦点、Esc 清光标/退出临时交互；在 100%/125%/150% 和小窗口下检查长金额、主要按钮、红涨绿跌/绿涨红跌的符号文字。记录缩放与窗口尺寸。
- **H-03（M3 起）**：在开发机生成两个安装包：`cargo run -p delta-cli -- package-build`（开发版，输出仓库根 `dist/delta-dev-setup-<版本>-x64.exe`，`dist/` 里每个身份只保留最新一个）；正式版在 ACT-10 后用 `release-build --version 0.1.1` 生成（只在本地，不发布）。把两个 `setup.exe` 复制到干净环境（Windows 沙盒或虚拟机，无 Rust/MSVC/Python、未另装 VC++ 运行库），分别双击安装：记录 SmartScreen 提示文字与是否需要管理员（应不需要）；从开始菜单启动“DELTA”和“DELTA Dev”，确认两个窗口可同时运行、离线可用、设置页“关于”显示各自版本与数据目录（`%LOCALAPPDATA%\delta`、`%LOCALAPPDATA%\delta-dev`）；关闭 DELTA Dev 后在“设置 → 应用 → 已安装的应用”卸载它，确认 DELTA 仍可运行、两个数据目录都还在。再运行一次正式版安装包（覆盖安装）时让 DELTA 保持打开，确认它被请求保存退出、安装完成后数据仍在。

- **H-04**：在开发机仓库根目录执行（全部使用测试身份 `delta-test`，不影响你的 `delta` / `delta-dev`）：① `cargo run -p delta-cli -- package-build --identity delta-test`，双击 `dist/` 中生成的 `delta-test-setup-*.exe` 安装；② `cargo run -p delta-cli -- release-keygen --key-file %TEMP%\delta-h04\test-signing.key`（一次性测试密钥，不要用正式密钥文件）；③ 稍等一分钟后执行 `cargo run -p delta-cli -- release-build --test-identity --key-file %TEMP%\delta-h04\test-signing.key`（测试身份自动取比①更新的开发版号）；④ `cargo run -p delta-cli -- update-test-source --release-dir <③输出的目录>`，命令显示 `DELTA_UPDATE_SOURCE` 与 `DELTA_UPDATE_PUBLIC_KEY` 并保持本机回环源运行；⑤ 在另一个 PowerShell 设置这两个环境变量后启动 `%LOCALAPPDATA%\Programs\delta-test\delta-test.exe`，在设置页手动检查更新，观察下载、“更新就绪”、忙碌确认（可先开始一次同步）、安装与重启后的版本。结束后在“已安装的应用”卸载 DELTA Test，并删除 `%TEMP%\delta-h04` 目录里的测试密钥。记录措辞是否清楚、耗时与是否有数据丢失。

资料库恢复/删除的人工观察可随 H-03 进行：先在资料库之外保存一致性备份；设置页“恢复”填写备份与新目标目录，成功后以该目录启动。删除预览要核对路径、账户/笔记/附件/会话数量并输入完整资料库 ID；外部备份、导出与模型服务留存需独立处理。不要在唯一真实资料库上尝试破坏性验证。

## 回写规则

你可以随时直接回复编号（例如：“Q-01 我主要用 IBKR 和 Binance，美股可以先接 Tiingo + 免 Key 兜底”）。答复后 Agent 同轮更新 [确认记录](docs/decisions/confirmed-decisions.md)、相关设计和计划。完整协议见 [开发沟通](docs/dev-rules/communication-protocol.md)。
