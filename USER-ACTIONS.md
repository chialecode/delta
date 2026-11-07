# 用户待办

核对日期：2026-11-08。这里是需要你处理的事项的唯一入口，可以分次回复，也可以以后再做。开发继续使用已说明的可逆默认；未回复不等于授权，未测保持 not-run。Agent 的代码缺口和开源适配任务在 [工程事项](docs/decisions/open-questions.md)，不交给你补实现。

2026-11-07 已按你的指示（[D-20 ～ D-23](docs/decisions/confirmed-decisions.md)）从文档治理基线重建项目，确立 **100% 纯 Rust 实现 + M1 金融数据底座优先**，并将你最常用的 **富途牛牛 + 欧易（OKX）**（以及常用的老虎证券、IBKR、长桥、嘉信、东方财富、币安）写入当前唯一计划 [M1 数据底座计划](docs/delivery/stage-plan.md)（v2.6）。本机 Rust + MSVC 开发环境（`ACT-07`）也已复核通过。

## 决定与真实条件

| 编号 / 状态 | 需要你做什么 | 推荐与当前默认 | 真正受阻动作 |
| --- | --- | --- | --- |
| Q-01 / resolved | 确认你最常用的美股券商和加密交易所 | **已确认（D-23，2026-11-07）**：常用平台为 **富途牛牛、老虎证券、IBKR、长桥、嘉信证券、东方财富、币安、欧易**，其中**使用最多（核心主力）为富途牛牛与欧易（OKX）**。M1 必做已调整为：① 加密现货同时内置 **欧易（OKX）公开现货 REST（`bar=1Dutc`）+ 币安公开行情镜像与归档**；② 账单导入必做覆盖 **富途牛牛 CSV、欧易（OKX）现货账单 CSV、通用 CSV、IBKR Flex XML、币安现货 CSV**（老虎、长桥、嘉信、东财交割单、IBKR Activity CSV 为 M1 扩展预设）；③ 美股/ETF 零配置由纯 Rust 直连 **Yahoo Chart v8 主 + 东方财富美股日线（`push2his`）核验**（配置 Token 时走 Tiingo 主 + Yahoo 核验），自动映射 `US.AAPL ↔ AAPL.US ↔ 105.AAPL ↔ NASDAQ:AAPL` | 已关闭；无阻塞 |
| Q-03 / resolved | 确认后续 M2 桌面界面的技术栈方向（基于本次 GPUI 复盘结论） | **已确认（D-21，2026-11-07）**：① M1 纯做 100% Rust 数据底座与 CLI（D-22 已彻底移除 Python）；② M2 继续使用 GPUI，摒弃旧 `0.6.6` 锁定，升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接按照长桥官方提供的 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系构建桌面工作台，并通过视口滑动窗口（Viewport Slicing）驱动交互式 K 线与回放面板 | 已关闭；无阻塞 |
| ACT-07 / resolved | 自行安装 Rust 开发环境（Visual Studio Build Tools + `rustup`） | **已复核通过（2026-11-07）**：本机已就绪 Visual Studio Build Tools 2026（`18.11.12328.437`，`MSVC 14.51.36231` `Hostx64/x64/cl.exe` 与 `link.exe`、Windows SDK `10.0.26100.0`）及 `rustup`（`stable-x86_64-pc-windows-msvc`，`rustc 1.99.1`、`cargo 1.99.1`、`clippy 0.1.99`、`rustfmt 1.10.0-stable`），最小编译与 MSVC 链接实测通过 | 已关闭；M1 构建阻塞已解除 |
| ACT-03 / open | （可选）提供富途牛牛、欧易（OKX）、IBKR、币安、老虎、长桥、嘉信的去标识导出 CSV/Flex 样本，或免费行情 API Token（Tiingo；扩展层 Alpaca / Longport） | 若只提供一部分，**优先富途牛牛与欧易（OKX）导出样本**：这两个是 M1 必做导入器，但没有公开格式规范，目前只能按帮助文档构造合成样本。真实样本放忽略目录 `.local/private-samples/`；Token 存入 Windows 凭据管理器（B 实现 `delta-cli` 凭据命令后按其说明录入），开发期也可设环境变量 `DELTA_TIINGO_API_KEY`；不要粘贴到聊天或提交 Git | 真实个人账户账单与 Tiingo 在线端到端验收；缺样本/Token 时 M1 先用免 Key 公共源与公开标准结构夹具验证，真实样本/Tiingo live 保持 not-run |
| ACT-01 / deferred | 真实 AI 模型连接测试配置（协议类型、base URL、model ID、凭据引用与测试预算） | 延后至 M2 应用整合阶段；当前 M1 聚焦金融数据源与账本核心 | M2 真实 AI 协议兼容与可用性声明 |
| Q-02 / resolved | 首次对外分发前选择项目许可证 | **已确认（D-24，2026-11-07）**：与你的其他项目统一采用 **Apache-2.0**，已添加根 [LICENSE](LICENSE)（标准原文）。B 在 M1 锁定依赖后生成传递许可清单，并检查与 Apache-2.0 分发的兼容性 | 已关闭；无阻塞 |

## 外部与远端操作

### ACT-06 / resolved（远端基线重建已完成；后续阶段推送另行按需授权）

- 已按你 2026-11-07 的明确授权，将旧远端仓库重命名为 `chialecode/delta-old`，并重新创建全新的公开远端仓库 `chialecode/delta`。
- 新远端 `origin/main` 与本地 `main` 均仅保留第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`（空 `README.md`），无任何旧 PR 或废弃历史；同时已恢复仓库合并设置（仅允许 squash merge、合并后自动删除分支）与 `main-protection` 规则集（active）。
- 文档治理基线与当前阶段分支 `codex/m1-data-foundation` 按你的指示仅保留在本地，暂不推送；后续何时推送文档基线或 M1 分支均需你另行明确授权。

### ACT-08 / authorized（M1 v2.6 推送、PR、合并与分支清理，2026-11-08 授权）

- **你的授权（2026-11-08）**：推送 `codex/m1-data-foundation`（已并入文档治理基线）到 `origin`；以该分支向 `main` 创建 PR，修复 PR 检查直到全绿后合并；合并后清理该阶段分支；随后开启 M2 阶段。
- **执行约束**：合并方式按 `main-protection` 规则集只能 squash（文档基线与 M1 阶段提交合为 `main` 上一个提交，原始提交保留在 PR 记录中）；推送后的修复按 D-09 追加提交，不改写已推送历史。M2 计划在本地起草，M2 的推送/PR 仍需你另行授权。
- **DCO（D-27）**：推送范围内每个提交都带 `Signed-off-by`。
- **不受影响**：`ACT-03` 真实样本 / 私有服务与产品验收继续独立推进；合并不代表产品已验收。

## 回写规则

你可以随时直接回复编号（例如：“Q-01 我主要用 IBKR 和 Binance，美股可以先接 Tiingo + 免 Key 兜底”）。答复后 Agent 同轮更新 [确认记录](docs/decisions/confirmed-decisions.md)、相关设计和计划。完整协议见 [开发沟通](docs/dev-rules/communication-protocol.md)。
