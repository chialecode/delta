# 用户待办

核对日期：2026-11-09。这里是需要你处理的事项的唯一入口，可以分次回复，也可以以后再做。开发继续使用已说明的可逆默认；未回复不等于授权，未测保持 not-run。Agent 的代码缺口和开源适配任务在 [工程事项](docs/decisions/open-questions.md)，不交给你补实现。

2026-11-08 M1 已按 `ACT-08` 授权推送、经 PR #1 squash 合并为 `main` `0370b7e`，阶段分支已清理；当前进入 **M2（资产复盘桌面工作台、证据化 AI 复盘与 Windows 打包）**，唯一计划见 [M2 计划](docs/delivery/stage-plan.md) v1.0。阶段命名约定见 [D-30](docs/decisions/confirmed-decisions.md)（人读标签大写、路径与分支小写），阶段编号与 PR 编号不绑定（D-28 修订）。下面 `ACT-09` 是 M2 推送/创建 PR 的具体授权请求；`ACT-01`、`ACT-03` 与 `H-01`～`H-03` 在方便时处理，都不阻塞 Agent 开发。

## 决定与真实条件

| 编号 / 状态 | 需要你做什么 | 推荐与当前默认 | 真正受阻动作 |
| --- | --- | --- | --- |
| Q-01 / resolved | 确认你最常用的美股券商和加密交易所 | **已确认（D-23，2026-11-07）**：常用平台为 **富途牛牛、老虎证券、IBKR、长桥、嘉信证券、东方财富、币安、欧易**，其中**使用最多（核心主力）为富途牛牛与欧易（OKX）**。M1 必做已调整为：① 加密现货同时内置 **欧易（OKX）公开现货 REST（`bar=1Dutc`）+ 币安公开行情镜像与归档**；② 账单导入必做覆盖 **富途牛牛 CSV、欧易（OKX）现货账单 CSV、通用 CSV、IBKR Flex XML、币安现货 CSV**（老虎、长桥、嘉信、东财交割单、IBKR Activity CSV 为 M1 扩展预设）；③ 美股/ETF 零配置由纯 Rust 直连 **Yahoo Chart v8 主 + 东方财富美股日线（`push2his`）核验**（配置 Token 时走 Tiingo 主 + Yahoo 核验），自动映射 `US.AAPL ↔ AAPL.US ↔ 105.AAPL ↔ NASDAQ:AAPL` | 已关闭；无阻塞 |
| Q-03 / resolved | 确认后续 M2 桌面界面的技术栈方向（基于本次 GPUI 复盘结论） | **已确认（D-21，2026-11-07）**：① M1 纯做 100% Rust 数据底座与 CLI（D-22 已彻底移除 Python）；② M2 继续使用 GPUI，摒弃旧 `0.6.6` 锁定，升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接按照长桥官方提供的 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系构建桌面工作台，并通过视口滑动窗口（Viewport Slicing）驱动交互式 K 线与回放面板 | 已关闭；无阻塞 |
| ACT-07 / resolved | 自行安装 Rust 开发环境（Visual Studio Build Tools + `rustup`） | **已复核通过（2026-11-07）**：本机已就绪 Visual Studio Build Tools 2026（`18.11.12328.437`，`MSVC 14.51.36231` `Hostx64/x64/cl.exe` 与 `link.exe`、Windows SDK `10.0.26100.0`）及 `rustup`（`stable-x86_64-pc-windows-msvc`，`rustc 1.99.1`、`cargo 1.99.1`、`clippy 0.1.99`、`rustfmt 1.10.0-stable`），最小编译与 MSVC 链接实测通过 | 已关闭；M1 构建阻塞已解除 |
| ACT-03 / open | （可选）提供富途牛牛、欧易（OKX）、IBKR、币安、老虎、长桥、嘉信的去标识导出 CSV/Flex 样本，或免费行情 API Token（Tiingo；扩展层 Alpaca / Longport） | 若只提供一部分，**优先富途牛牛与欧易（OKX）导出样本**：这两个是 M1 必做导入器，但没有公开格式规范，目前只能按帮助文档构造合成样本。真实样本放忽略目录 `.local/private-samples/`；Token 存入 Windows 凭据管理器（B 实现 `delta-cli` 凭据命令后按其说明录入），开发期也可设环境变量 `DELTA_TIINGO_API_KEY`；不要粘贴到聊天或提交 Git | 真实个人账户账单与 Tiingo 在线端到端验收；缺样本/Token 时 M1 先用免 Key 公共源与公开标准结构夹具验证，真实样本/Tiingo live 保持 not-run |
| ACT-01 / open（M2，可选） | 真实 AI 模型连接测试配置：协议（`openai-responses` 或 `openai-chat-completions`）、base URL、model ID、凭据与测试预算 | 不提供也不阻塞：M2 用受控回环服务对两种协议做全套自动验收，真实服务保持 not-run。提供时凭据只录入 Windows 凭据管理器（B 实现模型连接设置后按其说明录入），不要粘贴到聊天或提交 Git | 用例 `M2-L-02`：你配置的协议/模型的真实可用性声明 |
| Q-02 / resolved | 首次对外分发前选择项目许可证 | **已确认（D-24，2026-11-07）**：与你的其他项目统一采用 **Apache-2.0**，已添加根 [LICENSE](LICENSE)（标准原文）。B 在 M1 锁定依赖后生成传递许可清单，并检查与 Apache-2.0 分发的兼容性 | 已关闭；无阻塞 |

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

### ACT-09 / authorized（M2 推送、创建 PR，CI 通过后合并）

- **2026-11-09 用户授权**：“授权 ACT-09，push 到远端然后 CI 没问题就 merge”。范围：推送 `codex/m2-desktop-workbench`、创建到 `main` 的 PR，必需检查（`docs`、DCO）全部通过后 squash 合并；不含发布与远端设置变更。执行结果见 [执行状态](docs/delivery/status.md)。

- **A 技术审核已收敛（2026-11-09）**：结论与证据见 [M2 阶段报告](docs/delivery/stage-report.md#a-集中技术审核2026-11-09)。本地分支 `codex/m2-desktop-workbench` 仅含一个阶段提交（父提交 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`，带 DCO 签名），最终完整 SHA 以交接消息和 `git rev-parse HEAD` 为准。
- **请求授权的具体动作**：① `git push -u origin codex/m2-desktop-workbench`；② 创建 PR：来源 `codex/m2-desktop-workbench` → 目标 `main`，标题 `feat(m2): desktop review workbench, evidence-checked AI review and Windows packaging (M2 v1.0)`，正文列明 M2-W01～W08 实际行为、A 审核修复（F-08 AI 数值校验绕过、F-09 图标资源/面板标题）、长桥风格界面重设计、实际检查与 not-run 边界（真实模型 ACT-01、个人账单 ACT-03、H-01～H-03）。
- **不包含**：合并、发布、改远端设置；这些需你另行授权。授权后 A 核对 CI（`docs`、DCO）与审查反馈。回复“ACT-09 授权推送并创建 PR”即可。
- 人工检查 H-01～H-03 不是本动作前提，可顺延；合并也不代表产品已验收。

## 人工检查（M2，B 交付安装包与检查步骤后执行，可顺延）

| 编号 / 状态 | 需要你做什么 | 说明 | 不影响 |
| --- | --- | --- | --- |
| H-01 / pending | 中文输入法检查：在笔记、搜索、导入字段映射中用拼音等输入法选词、组合态按回车 | 用例 `M2-M-01`；记录输入法名称与版本 | A 技术审核与远端授权请求 |
| H-02 / pending | 键盘、焦点、Esc、125%/150% 缩放、小窗口与涨跌配色检查 | 用例 `M2-M-02` | 同上 |
| H-03 / pending | 干净机器（虚拟机或 Windows 沙盒）按用户安装正式版与开发版、离线运行、卸载一个通道 | 用例 `M2-M-03`；验证无需管理员、两通道并存、卸载保留数据 | 同上 |

检查所用安装包和源码指纹以 [M2 阶段报告](docs/delivery/stage-report.md) 的本轮产物为准；索引位于 `.local/reports/m2/packages/package-artifacts.json`，包含两通道 ZIP、解压目录与 SHA-256。每个包带 `README.txt`、`package-manifest.json`、`binary-imports.json`、全部第三方许可与归档，静态链接 CRT 并检查普通/延迟 DLL 导入。

- **H-01**：在开发版日志页编辑正文/结构字段，输入中文并在候选组合态按 Enter；在市场搜索、笔记搜索和导入预设输入框重复。等待 1 秒或失焦后观察保存状态，重启核对已保存文字。记录输入法名称、版本与失败位置；任意拖拽字段映射编辑器尚未提供。
- **H-02**：Tab/方向键遍历六页、表格、图表控制和操作按钮；检查可见焦点、Esc 清光标/退出临时交互；在 100%/125%/150% 和小窗口下检查长金额、主要按钮、红涨绿跌/绿涨红跌的符号文字。记录缩放与窗口尺寸。
- **H-03**：在无 Rust/MSVC/Python、未另装 VC++ 可再发行组件的干净 Windows 环境解压两包，分别从解压目录执行 `delta-cli.exe package-install --channel release --package-dir .` 和 `delta-dev-cli.exe package-install --channel dev --package-dir .`。运行已安装两版，确认离线工作及数据根分别为 `delta` / `delta-dev`；关闭应用后，用**解压目录中的外部 CLI**执行 `package-uninstall --channel dev`，核对正式版和开发版资料库仍在。安装目录内正在运行的 CLI 会在删除前拒绝自卸载；包未签名，记录实际安装/启动结果。

资料库恢复/删除的人工观察可随 H-03 进行：先在资料库之外保存一致性备份；设置页“恢复”填写备份与新目标目录，成功后以该目录启动。删除预览要核对路径、账户/笔记/附件/会话数量并输入完整资料库 ID；外部备份、导出与模型服务留存需独立处理。不要在唯一真实资料库上尝试破坏性验证。

## 回写规则

你可以随时直接回复编号（例如：“Q-01 我主要用 IBKR 和 Binance，美股可以先接 Tiingo + 免 Key 兜底”）。答复后 Agent 同轮更新 [确认记录](docs/decisions/confirmed-decisions.md)、相关设计和计划。完整协议见 [开发沟通](docs/dev-rules/communication-protocol.md)。
