# 当前执行状态

更新：2026-11-08；维护：Agent A / Agent B。当前阶段：**M1 v2.6 / A 技术审核通过（含 A 局部修正），用户已授权 `ACT-08`（推送、PR、修复检查后 squash 合并、清理分支、开启 M2），执行中**。

## 基线与重建事实

- 已按用户 2026-11-07 指示（[D-20 ～ D-23](../decisions/confirmed-decisions.md)）从文档治理基线重建项目，放弃此前已构建的 GPUI 桌面先行代码、产物与历史阶段提交（旧分支本地备份已保存在忽略目录 `.local/rebuild-backup-20261107/`）。
- 第一提交 `8329f63`（空 `README.md`）保持不变；文档治理基线并入 M1 阶段提交（[D-29](../decisions/confirmed-decisions.md)）。
- 已按用户 2026-11-07 授权重建远端仓库（`ACT-06`）：新建 `chialecode/delta` 并恢复合并策略与 `main-protection` ruleset；远端 `origin/main` 与本地 `main` 均仅保留第一提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`（无任何旧 PR）。
- 当前工作分支 `codex/m1-data-foundation`（唯一阶段提交的父提交为 `8329f638536e4f303764045a2f10a2c1ab31fcd3`）按用户指示暂不推送，仅保留在本地（新完整 SHA 见交接消息与 `git rev-parse HEAD`；本文件随提交内容无法自含自身 SHA）。工作区既有未跟踪目录 `workers/agent/` 保持原样未触碰。

## 本轮完成内容（Agent A）

1. **GPUI 与技术栈合理性复盘及 D-21 / D-22 / D-23 确认**：
   - **D-20 / D-21**：M1 先剥离 GUI 完成金融数据底座；M2 继续使用 GPUI 并升级至最新 `gpui-kit`（`0.8.x` / `0.9.x`），采用官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系 + K 线视口滑动窗口切片（见 [ADR-0003](../decisions/0003-first-round-stack.md)、[技术选型](../engineering/technology-selection.md)）。
   - **D-22（全项目彻底 100% 纯 Rust 实现）**：彻底移除 `workers/python` 与运行期 Python 依赖（关闭 `OSS-03`）。免 Key 美股行情由 Rust 直连 Yahoo Finance Chart v8（单次 JSON 含 OHLCV、复权价、`splits`、`dividends`）与东方财富 `push2his` JSON 接口交叉核验；美股 `XNYS`/`XNAS` 日历、技术指标与 M4 受控策略引擎全部由纯 Rust 实现。
   - **D-23（确认个人常用平台，关闭 `Q-01`）**：常用平台为 **富途牛牛、老虎证券、IBKR、长桥证券、嘉信证券、东方财富、币安、欧易**，其中**使用最多（核心主力）为富途牛牛与欧易（OKX）**。已将 **富途牛牛 CSV** 与 **欧易 OKX 公开现货 REST（`candles`/`history-candles`，`bar=1Dutc`）+ 现货账单 CSV** 提升为 M1 必做（连同币安、IBKR Flex、通用 CSV），老虎、长桥、嘉信、东财交割单与 IBKR Activity CSV 纳入 M1 扩展预设。
2. **M1 计划 v2.3 自审定案、在线实测与本机环境复核**：
   - 清理 D-21 后残留的 Tauri 推荐表述；
   - 定案 S-01～S-07：M1 SQLite 单库（Parquet/DuckDB 转 `OSS-10`）、`delta-app` 同步命令/作业句柄异步边界（M2 GPUI 风险转 `OSS-09`）、纯 Rust 连接器“必做/扩展”分层、keyring 凭据、Provider 数据条款类别、纯 Rust 日历引擎、法币非发布日 `carried_forward` 规则（A-07）；
   - 在美国网络出口实测公共端点并修正事实：欧易 OKX `candles` 与 `history-candles`（`bar=1Dutc`）及 `public/instruments` HTTP 200；`api.binance.com` 451 → 改用 `data-api.binance.vision`/`data.binance.vision`；Coinbase 无 `USDC-USD` → `USDC/USD` 取 Bitstamp 主 + Kraken 核验；Kraken OHLC 仅最近 720 根；Yahoo Chart v8 实测返回 `AAPL` 拆股/分红 JSON；Alpaca 公司行动用 `/v1/corporate-actions`（见 [调研报告第 4 节](../evidence/2026-11-07-stack-and-data-research.md#4-a-自审复核与在线实测2026-11-07-v21v22)）；
   - 复核本机环境通过并关闭 `ACT-07`（Visual Studio Build Tools 2026 `18.11.12328.437` + `rustup` `stable-x86_64-pc-windows-msvc` `rustc 1.99.1` 编译与 MSVC 链接实测通过）。

3. **M1 计划 v2.4 复审修订（不改变用户决定）**：实测发现 Yahoo Chart v8 的 OHLCV 为拆股调整值（`AAPL` 2020-08-28 `124.8075` vs 东财原始价 `499.23`），新增 S-08 原始价规范口径（默认 A-08）与 S-09 行情修订/版本区间存储；补 Binance 归档 2025 年起微秒时间戳、日历已验证范围 `1990`～`2027` 与对照夹具、JSON 数值直接解析为 Decimal、交叉核验默认阈值、端点基址可配置、工具链精确锁定 `1.99.1`、keyring 原生后端验证；依赖图改为 `M1-W05a` 账本核心与 `M1-W02` 并行并要求先打通最薄端到端切片；按协作流程精简 A→B prompt。同步数据模型 v0.4、系统架构、技术选型、质量门禁、台账 `OSS-04`/`OSS-05` 与 [调研证据第 5 节](../evidence/2026-11-07-stack-and-data-research.md#5-a-计划复审实测2026-11-07-v24)。

4. **许可证（D-24，关闭 `Q-02`）**：按用户回复，项目自有源码与文档采用 Apache-2.0，已添加根 `LICENSE`（标准原文），同步 README、CONTRIBUTING 与 M1-W01 包元数据要求；`OSS-06` 改为检查依赖与 Apache-2.0 分发的兼容性。

5. **测试与门禁前台无干扰（D-25，M1 计划 v2.5 S-10）**：按用户要求在 [质量门禁](../dev-rules/quality-gates.md#foreground-quiet) 建立规则正本——静默进程助手（`CREATE_NO_WINDOW`）、回环测试服务只绑 `127.0.0.1:0`（避免防火墙对话框）、`SetErrorMode` 屏蔽崩溃对话框、非交互、不打开浏览器/资源管理器、控制字符转义、`BELOW_NORMAL` 优先级与受限并发、Job Object 收口与 `EnumWindows` 窗口哨兵；M1-W01 建立、M1-W06 验收。

6. **安装与数据位置（D-26，M1 计划 v2.6 S-11）**：路径一律小写 `delta`；正式版/开发版通道隔离——安装 `%LOCALAPPDATA%\Programs\delta[-dev]`、应用数据 `%LOCALAPPDATA%\delta[-dev]`、用户导出 `<文档>\delta[-dev]`；源码运行与测试等同开发版或临时根，不碰正式数据；不用 `%APPDATA%`、不建 `~\.delta`；`delta.lock` 独占锁。规则正本见 [安装与数据位置](../dev-rules/data-and-security.md#install-and-data-locations)。

## Agent B 复查与修复

本轮发现并修复真实同步、注册表 wire 别名、事件 FX 固定、导入去重与更正历史、独立资金流水、第三资产费用、未知历史成本、转账在途与账户范围、实际收益率、封存版本、备份恢复/导出/删除与开发通道隔离等缺口。完整问题清单和实现映射见 [F-01～F-13](stage-report.md#1-复查问题与修复)。计算版本为 `m1-ledger-3`，数据库 schema 为 3。

门禁改为保存实际命令日志与测试名称，运行结果绑定源码指纹；不再把一个部分流程的结果批量写成验收通过。官方 Rust ccxt 的真实隔离 POC 已纠正为活跃 `4.5.91`、构建成功、默认 Windows 栈溢出、加大 worker 栈后公共请求成功、typed OHLCV 为 f64，详见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md)。扩展模板/SDK/通用 adapter 的确实未完成部分集中维护在 [LOOP-03](../decisions/open-questions.md)。

## Agent A 集中复核

2026-11-08 A 核对了分支、父提交与 B 交付指纹（独立复算一致），逐项复核 F-01～F-13，并直接修复 6 个局部确定问题：期初现金币种错配入账（A-01）、已知成本期初持仓缺单位成本时以 0 入账（A-02）、稳定币报价腿双重扣减（A-03）、开发通道可写正式版 `<文档>\delta`（A-04）、报表 FX 未要求已发布版本（A-05），并修正 gate 指纹未按 LF 规范化导致证据与提交内容不一致（A-06）。修正、回归测试与实跑证据见 [阶段报告第 6 节](stage-report.md#6-agent-a-集中复核)；后续现金换汇建模登记为 `LEDGER-01`。

## 当前验证与接续

A 修正后源码指纹 `ae2f8960e406fd44c43e9ef81bc83e13084ac503dbbe65bebcdf24a8fe7a447b`：静音离线 `verify-stage` 与公共 `--live` 均 passed（workspace 50 passed / 1 ignored，必做用例全部 passed，12 个公共 probe HTTP 200，私有服务 3 项 not-run），文档回写后单独重跑文档门禁通过。B 交付指纹 `2820f9ba…` 的结果仅作历史输入。每个提交带 `Signed-off-by`（D-27，见 [Git 规则](../dev-rules/git-workflow.md#dco)）。

- B 自检：passed。A 技术审核：passed with A fixes，无 M1 必做工程阻断。产品验收：未完成（`ACT-03` 真实样本与私有服务 not-run，M2 未验收），人工/私有项独立推进。
- 修正已按 D-09 amend 进同一本地阶段提交（父提交仍为 `8329f638536e4f303764045a2f10a2c1ab31fcd3`）；完整 SHA 见交接消息与 `git rev-parse HEAD`。

推送前 A 回读远端发现仓库 Actions 白名单只允许 `actions/checkout@*` 与 `actions/setup-node@*`，原 `docs.yml` 使用的 `actions/setup-python@v5` 会使 ruleset 必需检查 `docs` 被拒；已改为 runner 自带 `python3`（脚本无第三方依赖），规则写入 [Git 规则](../dev-rules/git-workflow.md#github-hygiene)。该修正不在源码指纹范围内，上述 gate 证据仍有效。

下一步：按 `ACT-08` 推送 `codex/m1-data-foundation`、创建 PR、`docs` 检查通过后 squash 合并并清理分支，然后在本地起草 M2 计划（M2 推送/PR 另需授权）。未自动创建其他 Agent 会话。
