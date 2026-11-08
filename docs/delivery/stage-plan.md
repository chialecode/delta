# M1：金融数据来源底座、自动化增量同步与账本核对实施计划

版本 **2.6** · 2026-11-07 · 编制：Agent A · 执行：Agent B · 当前状态见 [执行状态](status.md)。v2.6 按用户 [D-26](../decisions/confirmed-decisions.md) 增加 S-11 安装与数据位置（小写 `delta`，正式版/开发版 `delta-dev` 隔离），规则正本见 [安装与数据位置](../dev-rules/data-and-security.md#install-and-data-locations)。v2.5 按用户 [D-25](../decisions/confirmed-decisions.md) 增加 S-10 测试与门禁前台无干扰（静音、不弹窗、不抢焦点、低优先级），规则正本见 [质量门禁](../dev-rules/quality-gates.md#foreground-quiet)。v2.4 为 A 计划复审修订（不改变用户决定）：补定行情价格口径（S-08，Yahoo `close` 实测为拆股调整价）、行情修订与数据版本存储（S-09）、日历已验证范围与对照数据、Binance 归档时间戳单位、JSON 数值无损解析、真实依赖顺序与最薄端到端切片，并按协作流程精简 A→B prompt（实证见 [调研证据第 5 节](../evidence/2026-11-07-stack-and-data-research.md#5-a-计划复审实测2026-11-07-v24)）。v2.3 按用户 [D-22 ～ D-23](../decisions/confirmed-decisions.md) 确立**全项目 100% 纯 Rust 实现**与**用户主力平台（富途牛牛 + 欧易 OKX 核心主力，兼顾老虎、IBKR、长桥、嘉信、东方财富、币安）优先覆盖**，并在 v2.1/v2.2 基础上定案 SQLite 单库、实测数据源路由、`delta-app` 异步边界与必做/扩展分层（实证见 [调研证据第 4 节](../evidence/2026-11-07-stack-and-data-research.md#4-a-自审复核与在线实测2026-11-07-v21v22)）。

## 1. 目标与核心转变（D-20 / D-21 / D-22 / D-23）

按用户指示（[D-20 ～ D-23](../decisions/confirmed-decisions.md)），项目从文档治理基线重建，反转研发顺序并统一为纯 Rust 技术栈：

1. **全项目 100% 纯 Rust 实现（D-22）**：不引入 `workers/python` 或任何 Python 运行期子进程。所有行情/汇率/公司行动连接器、美股与加密交易日历、技术指标、十进制账本、CLI 与后续 M2 GPUI 桌面端、AI 模型客户端、M3 训练、M4 策略引擎全部用 Rust 实现，编译产物为单一原生二进制。
2. **优先解决美股/ETF 与加密现货等金融数据来源问题，聚焦用户主力平台（D-20 / D-23）**：针对用户确认的常用平台（核心主力：**富途牛牛、欧易 OKX**；常用：**老虎证券、IBKR、长桥证券、嘉信证券、东方财富、币安 Binance**），Rust 原生打通免 Key 公共数据源（**欧易 OKX 公开现货 REST `bar=1Dutc`**、**币安公开行情镜像与历史归档**、Frankfurter/ECB 法币汇率、Coinbase/Bitstamp/Kraken 真实 `USDT/USD` 与 `USDC/USD` 日线、Yahoo Finance Chart v8 + **东方财富美股日线接口**交叉核验）与官方免费 API（Tiingo REST），内置权威交易日历（`XNYS`/`XNAS` 与 `CRYPTO_24_7`）及主力券商/交易所账单导入模板。
3. **构建内置自动化数据平面（`delta-data`），告别外部手动维护**：“账本/自选驱动自动订阅 → 交易日历感知的水位线增量同步（`sync_checkpoint`）→ 多源降级与拆股/分红交叉核验 → SQLite 单库落盘 → 不可变 `dataset_version` 快照”。
4. **M1 彻底解耦桌面 GUI（D-21）**：本阶段不引入 GPUI 或任何桌面窗口依赖，通过 `delta-core`、`delta-data`、`delta-app`、`delta-cli` 完成端到端数据获取、账单导入、FIFO 成本与多币种损益对账闭环。M2 桌面端采用新版 `gpui-kit` 官方面板体系；M1 按第 3 节 S-02 保持与之兼容的异步边界。

## 2. 开始基线、分支与 Git 规则

- 不可变基础提交：第一提交 `8329f63`（空 `README.md`）；文档治理基线并入本阶段提交（[D-29](../decisions/confirmed-decisions.md)）。
- 当前分支：`codex/m1-data-foundation`，阶段父提交固定为 `8329f638536e4f303764045a2f10a2c1ab31fcd3`；阶段提交的当前完整 SHA 以 [执行状态](status.md) 为准。
- 阶段提交规则（[D-09](../decisions/confirmed-decisions.md)、[Git 规则](../dev-rules/git-workflow.md)）：本计划所在提交是 `8329f63` 之后的唯一阶段提交；B 的实现、A 的审核修正与 B 的返工在 push 前均**直接 `git commit --amend` 进该阶段提交**，不得 amend 第一提交。
- 工作区保护：保留既有未跟踪目录 `workers/agent/`，不删除、不构建、不暂存；旧废弃分支的本地备份 bundle 保存在忽略目录 `.local/rebuild-backup-20261107/`。无具体远端授权（[ACT-06](../../USER-ACTIONS.md)）前不执行 `git push` 或 PR 操作。
- 网络与本机环境：用户确认本机经 TUN 代理以美国网络出口访问外网；A 已实测全部必做公共端点（含欧易 OKX `candles`/`history-candles`、Binance 镜像、Yahoo Chart v8 拆股/分红 JSON 与东财美股 JSON）。本机 [ACT-07](../../USER-ACTIONS.md) 已完成复核（Visual Studio Build Tools 2026 `18.11.12328.437` + `rustup` `stable-x86_64-pc-windows-msvc` `rustc 1.99.1` 编译链接实测通过）；Rust 版本由 B 以仓库根 `rust-toolchain.toml` **精确锁定 `channel = "1.99.1"`**（组件 `rustfmt`、`clippy`；不用浮动 `stable`，满足 NFR-08 可复现），升级工具链按普通依赖变更同步本节。
- 网络出口可变：所有公共端点基址（含 OKX 区域域名、Binance 镜像）必须可配置，并在 `capabilities()` 与同步记录中保存实际基址；默认值按上述实测，`api.binance.com` 不作默认。HTTP 请求统一设置显式 User-Agent、超时与重试上限。

## 3. A 已定案的阶段技术决定（B 不再重新选型）

| ID | 决定 | 理由 / 退出条件 |
| --- | --- | --- |
| S-01 存储 | **M1 只用 SQLite（`rusqlite` bundled，WAL）**：控制面、账本、日线 `bar`、汇率、公司行动、日历、水位线、`dataset_version` 与原始响应摘要全部在单一 `delta.sqlite`（位于 S-11 应用数据根）；`dataset_partition.location_kind` 保留存储类型抽象（M1 仅 `sqlite`）。原始响应正文按内容哈希寻址保存（库内表或资料库 `raw/` 目录二选一由 B 定，必须纳入备份清单） | 个人组合日线规模（数百标的 × 数十年 ≈ 百万行级）SQLite 足够；单库使 AC-09 一致性备份恢复可由 SQLite Backup API 原子完成；避免 Windows 下 DuckDB bundled C++ 编译成本。Parquet/DuckDB 推迟为 `OSS-10`，触发条件：M4 批量扫描或实测查询性能不达标 |
| S-02 异步边界 | `delta-core` 纯同步、无 IO；`delta-data` 内部使用 `tokio` + `reqwest`；**`delta-app` 对外 API 不暴露 `tokio`/`reqwest` 类型与 `async fn`**，采用同步命令/查询 + 后台作业句柄（`JobId`、超时/取消/进度通过标准库通道或回调 trait，作业 panic 与超时由宿主隔离转为结构化错误），运行时由 `delta-app` 私有持有 | M2 GPUI（`gpui-kit 0.9.0` 精确锁定 `gpui-pre =0.3.11`，自带执行器）可在 UI 线程直接调用而无需桥接两个运行时；同时原生满足 AC-16 后台任务超时/取消/故障隔离；M2 风险见 `OSS-09` |
| S-03 连接器 | **100% 纯 Rust（`reqwest` + `serde_json`）实现**，见 3.1 节“必做/扩展”分层；不启动任何 Python 进程 | 消除跨语言 IPC 与 Python 打包负担；全部必做端点已于 2026-11-07 在线实测返回标准 JSON/ZIP（Tiingo 需 Token） |
| S-04 凭据 | 私有 Token（Tiingo，扩展层 Alpaca）以 `keyring`（Windows 凭据管理器）为正式存储；开发/CI 仅允许显式受控环境变量引用（如 `DELTA_TIINGO_API_KEY`）；禁止出现在命令行参数、配置明文与日志 | 遵守 [数据安全规则](../dev-rules/data-and-security.md) 与 FR-OPS-03 |
| S-05 数据条款 | 每个 Provider 的 `capabilities()` 声明 `requires_key`、`rate_limit`、`terms_class`（`public_open` / `official_free_personal` / `unofficial_personal`）；`dataset_version` 记录来源条款类别；导出携带来源与条款类别，M1 不提供行情再分发功能 | 代码开源许可不授予行情数据使用权；Yahoo/东财接口为 `unofficial_personal`，仅个人本地使用 |
| S-06 日历 | **100% 纯 Rust 实现**：基于 `chrono` + `chrono-tz`（`America/New_York`）编码 `XNYS`/`XNAS` 法定节假日（含 Good Friday 复活节算法、Juneteenth 生效年份、独立日/感恩节次日/平安夜 13:00 ET 半日市、周末顺延观察日）+ 仓库内置的版本化特殊表 JSON（历史哀悼日/飓风等特殊闭市，以及**无法由规则推出的历史半日市**，带 SHA-256），建库/迁移时生成写入 SQLite `calendar_session`；`CRYPTO_24_7` 为 UTC 日切。**已验证范围定为 `1990-01-02`～`2027-12-31`**，范围外返回 `calendar_unknown`。日线 `session_date` 一律按交易所本地日期（`America/New_York`）归属，不由 UTC 日期推断 | 零外部运行时依赖、离线确定性可复现。对照数据：1990～2023-01 使用 `exchange_calendars`（Apache-2.0）仓库数据文件 `tests/resources/xnys.csv`（实测覆盖 1990-01-02～2023-01-13）作测试夹具，仅取数据文件、不引入 Python，保留许可声明并计入 `OSS-06`；2023～2027 以 NYSE 官网公布的假日与提前收盘表录制为夹具。`XNAS` 与 `XNYS` 若有差异记入特殊表 |
| S-07 汇率时点（可逆默认 A-07） | 法币参考汇率仅在 ECB 发布日存在；非发布日估值使用最近一个不晚于估值时点的已发布值并标记 `carried_forward`（记录来源日期）；连续超过 4 个日历日无发布即视为缺汇率（`partial`）。稳定币用自身日线收盘，不做 1.0 假设 | 规则同步至 [金融规则](../design/financial-engine.md)；Frankfurter 的 `USD/CNY` 仅自 2005 年起有数据，更早区间必须显式缺失 |
| S-08 行情价格口径（可逆默认 A-08） | **规范存储为原始未复权 OHLCV + `corporate_action`**，`adjustment` 取值 `raw` / `split_adjusted` / `total_return_adjusted`；复权序列由原始价与带版本的公司行动因子派生，供应商自带的复权序列只入核验，不作估值输入。**Yahoo Chart v8 的 `open/high/low/close/volume` 是拆股调整值而非原始价**，`adjclose` 为拆股+分红调整，`dividends` 金额也按之后的拆股调整：Yahoo 原始价须以“调整值 × 之后累计拆股因子”还原（成交量除以该因子），并与东财 `fqt=0`、Tiingo 原始价核验。JSON 数值必须从源文本直接解析为 `Decimal`（禁止 `f64` 中间值），Yahoo 浮点噪声按版本化规整规则处理（美股价格保留 4 位小数）；账本分红以券商流水为准，行情侧分红只作核验 | 实测 Yahoo `AAPL` 2020-08-28 `close=124.80750274658203`，东财 `fqt=0` 同日 `499.23`（4:1 拆股前原始价）。若把 Yahoo `close` 当原始价，估值与 AC-05 拆股会被重复调整；与 [金融规则](../design/financial-engine.md#3-转账与公司行动) “估值优先使用原始价格并显式处理公司行动”一致 |
| S-09 行情修订与数据版本 | `bar` 按 `(instrument, timeframe, session, adjustment, source, open_at)` 保存内容，修订时保留旧行并以 `valid_from_version` / `valid_to_version` 标识有效版本区间；`dataset_version` 是清单与版本号，**不复制行情行**，按版本查询为 `valid_from_version ≤ v < valid_to_version`（`valid_to_version` 为空表示仍有效）。每次增量同步回看重叠窗口（美股最近 5 个交易时段、加密最近 3 个 UTC 日），内容变化生成新修订与 `data_revision` 诊断，不静默覆盖；新增拆股只追加 `corporate_action` 与新因子版本，原始价不重写 | 避免每次同步复制百万行；固定版本查询可复现（M3/M4 依赖）；修订可追溯 |
| S-10 前台无干扰（D-25） | 测试、`verify-stage` 与 Agent 开发期命令按 [质量门禁](../dev-rules/quality-gates.md#foreground-quiet) 执行：子进程统一经静默进程助手（`CREATE_NO_WINDOW`、空 `stdin`、输出捕获，`clippy.toml` 禁止直接 `Command::new`）；回环测试服务只绑定 `127.0.0.1:0`；门禁设置 `SetErrorMode` 屏蔽崩溃与错误对话框、`BELOW_NORMAL` 优先级与 `max(1, 逻辑核数 - 2)` 并发；子进程放入 Job Object 统一收口，并以窗口哨兵（`EnumWindows`）检测任何可见窗口；全程非交互、不打开浏览器或资源管理器、不发声，外部文本中的控制字符转义后输出。依赖 `windows-sys 0.61.x`（仅 Windows 目标） | 用户要求后台测试尽量不影响前台工作。最常见干扰是从无控制台的 Agent/IDE 启动时每个子进程闪出控制台窗口，以及测试服务绑定非回环地址触发防火墙对话框；两者都可由代码规则消除，并由哨兵自动证明 |
| S-11 安装与数据位置（D-26） | 按 [安装与数据位置](../dev-rules/data-and-security.md#install-and-data-locations) 实现单一位置解析模块（`delta-app` 内，CLI 与 M2 共用）：通道 `release` / `dev` / `source`（构建期 `DELTA_CHANNEL`）；应用数据根 `%LOCALAPPDATA%\delta[-dev]`，用户文件根 `<文档>\delta[-dev]`（`SHGetKnownFolderPath`），`source` 等同 `dev`；`DELTA_DATA_DIR` / `DELTA_DOCUMENTS_DIR` / `--data-dir` 覆盖；`delta.lock` 独占锁；`keyring` 服务名按通道命名空间；全部小写，不用 `%APPDATA%` 与 `~\.delta`。M1 不做安装包，安装目录约定供 M2 打包使用 | 与用户其他项目一致的通道约定；正式数据不会被开发构建或测试误写；单写锁与 S-01 单写执行器一致 |

### 3.1 连接器与导入格式分层（全部纯 Rust）

按用户 [D-23](../decisions/confirmed-decisions.md)（核心主力：**富途牛牛 + 欧易 OKX**；常用：老虎、IBKR、长桥、嘉信、东方财富、币安）定案如下：

| 领域 | M1 必做（阶段完成前提） | M1 扩展（必做通过后执行；未完成登记 `LOOP-03`，不阻塞收敛） |
| --- | --- | --- |
| 加密现货日线 | ① **欧易（OKX）公开现货 REST**（用户第一主力：`www.okx.com/api/v5/market/candles` 与 `/api/v5/market/history-candles`，`bar=1Dutc` 原生对齐 UTC 日切，仅提交 `confirm="1"` 已完结 K 线）；② **币安（Binance）公开行情**：增量用 `data-api.binance.vision/api/v3/klines`，回填用 `data.binance.vision` 日/月归档并校验 `.CHECKSUM`（SHA-256）；**现货归档自 2025-01-01 起时间戳为微秒**（此前为毫秒，实测 `BTCUSDT-1d-2025-03-01` 开盘时间 `1740787200000000`），解析器显式判定单位并用跨 2024-12-31/2025-01-01 的夹具验证。支持 `OKX:BTC/USDT`、`BINANCE:BTC/USDT` 独立同步与跨所核验。**不得使用 `api.binance.com`**（美国出口实测 HTTP 451） | 通用多场所连接器：官方 Rust `ccxt 4.5.x` crate（按交易所 feature 裁剪）（POC-D06） |
| 稳定币汇率 | `USDT/USD`：Coinbase Exchange `USDT-USD` 日线（主，300 根/页分页）+ Bitstamp `usdtusd`（核验）；`USDC/USD`：Bitstamp `usdcusd`（主，1000 根/页）+ Kraken `USDCUSD`（核验，公开 OHLC 仅返回最近 720 根）。Coinbase 无 `USDC-USD` 产品 | — |
| 法币汇率 | Rust 原生 Frankfurter（`api.frankfurter.dev/v1`，ECB 参考汇率，`USD/HKD`、`USD/CNY` 等） | 东方财富外汇历史接口或 FRED 公开 CSV/JSON 兜底 |
| 美股/ETF 日线与公司行动 | 零配置路由（纯 Rust HTTP）：**Yahoo Finance Chart v8**（`query1.finance.yahoo.com/v8/finance/chart/{symbol}?interval=1d&events=div,split`，主：公司行动事件 + 拆股调整 OHLCV，按 S-08 还原原始价）+ **东方财富美股日线接口**（`push2his.eastmoney.com/api/qt/stock/kline/get`，`secid=105.AAPL` 等，`fqt=0` 原始价核验，`fqt=1/2` 仅作复权核验）；配置 Token 路由：**Tiingo REST**（主，含 `divCash`/`splitFactor`）+ **Yahoo Chart v8**（核验）。Tiingo 无 Token 时以录制夹具验证，live 保持 not-run | Alpaca Market Data REST：`/v2/stocks/bars` + `/v1/corporate-actions`（`v1beta1` 不再使用）；长桥用户可选 `longport 4.3.7` Rust SDK |
| 账户只读 API | `AccountProvider` trait + 录制夹具实现（覆盖 **欧易 OKX V5** 与 **Binance** 形态成交/余额/充提），覆盖 AC-17；M1 不做真实账户 API 在线同步 | 真实只读 API 同步（OKX V5 只读 REST / Longport 只读 API，需用户 Key；Binance.com 账户 API 在美国出口不可用） |
| 账单导入（各预设均建立在同一通用 CSV 映射引擎之上，按表头签名识别；未知列或缺必需列拒绝并给出诊断，不静默忽略） | ① **富途牛牛（Futu）历史成交与资金流水 CSV**（用户第一美股/证券主力，支持中/英文表头、佣金/平台费/规费汇总与 `US.AAPL ↔ NASDAQ:AAPL` 映射）；② **欧易（OKX）现货成交与资金账单 CSV**（用户第一加密主力）；③ **通用 CSV 映射模板**；④ **IBKR Flex Query XML**（Trades、CashTransactions、CorporateActions、Transfers）；⑤ **币安（Binance）现货成交历史 + 充提 CSV** | 其余常用券商预设模板：**老虎证券（Tiger）活动账单 CSV**、**长桥证券（Longbridge）账单 CSV**、**嘉信证券（Charles Schwab Transactions）CSV**、**东方财富交割单 CSV**、**IBKR Activity CSV** |
| 技术指标 | Rust 原生 MA/EMA/MACD/RSI，手算夹具对照 + 未来哨兵截断测试（AC-11） | Rust 原生扩展指标（ATR、Bollinger Bands 等） |

交叉核验默认阈值（可逆，B 可按实测调整并在阶段报告记录理由；结果写入 `data_quality_issue`，账户余额差异仍用 `reconciliation_issue`）：同一交易日两源原始收盘价相对差 > 0.5% 记 `price_mismatch`；相邻交易日原始收盘比值落在常见拆股/合股比例 ±5% 内且无对应 `corporate_action` 记 `missing_corporate_action`；稳定币相对 1 的偏离只记录不告警（AC-06 要求按实际价估值）。

富途与欧易账单没有公开格式规范且尚无真实样本（`ACT-03`）：B 以官方帮助文档或导出界面可得的列名构造合成夹具，夹具目录附 `PROVENANCE.md` 记录列名来源与日期；真实样本验证保持 not-run，收到样本后按差异修正映射并补回归夹具。

## 4. 技术门槛（Spikes）与继续条件

| ID | 最小实际验证 | 通过后直接继续 | 失败或异常分支 |
| --- | --- | --- | --- |
| POC-D01 | SQLite 迁移 + WAL + 外键；`bar` 表 100 万行写入/去重、S-09 按版本查询（含约 1% 修订行）与按 `visible_until` 截断查询耗时记录；Backup API 一致性快照并恢复到新目录、哈希一致 | M1-W01 完整存储实现 | 若单库查询不达标，先加复合索引与分区表；仍不达标记录实测并升级 `OSS-10`，不在 M1 引入 DuckDB |
| POC-D02 | 纯 Rust 生成 `XNYS`/`XNAS` 日历并验证：2026-03-09（03-08 夏令时切换后首个交易日，开盘 13:30 UTC）、2026-04-03 耶稣受难日、2026-07-03 独立日观察日休市、2026-11-27 感恩节次日 13:00 ET 半日市、2012-10-29/30 飓风闭市、2025-01-09 国家哀悼日闭市、加密 UTC 日切；`1990-01-02`～`2027-12-31` 全量与 S-06 对照夹具逐日一致（开收盘时刻同样比对） | M1-W02 水位线判定与休市/缺数诊断 | 差异直接修正规则或特殊表；无法确认的年份缩小已验证范围、使其返回 `calendar_unknown`，不猜测 |
| POC-D03 | 欧易 OKX `candles`/`history-candles`（`bar=1Dutc`）与 Binance 镜像 klines / 归档 CHECKSUM；Coinbase/Bitstamp/Kraken 稳定币分页；Frankfurter 历史区间；Binance 归档毫秒/微秒边界；全部以本地回环服务回放录制报文验证分页、429/5xx 退避与去重 | M1-W03 增量同步与限流恢复 | 某端点 live 不可达时保留回放测试并在报告中标 live not-run 及错误码，不改写为通过 |
| POC-D04 | 纯 Rust Yahoo Chart v8 + 东财 `push2his` 获取 `AAPL`（2020-08-31 4:1 拆股）、`NVDA`（2024-06-10 10:1 拆股）、`SPY`（季度分红）；断言 `AAPL` 2020-08-28 还原后原始收盘 `499.23`（拆股调整值 `124.8075`）、原始成交量 = Yahoo 成交量 ÷ 4，并与东财 `fqt=0` 一致；Tiingo 以录制夹具解析 | M1-W04 拆股/分红核验与复权跳空告警 | 注入“缺失拆股记录”的夹具，交叉核验器必须输出 `partial/warning` 与 `data_quality_issue`（`missing_corporate_action`） |
| POC-D05 | Decimal 跨层无损（含 JSON 数值源文本直接解析、SQLite TEXT 往返）+ AC-02 最小账本（通用 CSV → FIFO → 现金 1438 / 成本 600.6 / 已实现 38.6 / 未实现 29.4 / 总损益 68） | M1-W05 完整导入与账本 | 金额、数量或成本守恒不通过即阻断交付，优先修复领域核心 |
| POC-D06（扩展） | 官方 Rust `ccxt` crate 仅启用 `okx`/`kraken` feature 的编译时间、依赖树与 `fetch_ohlcv` 免 Key 调用 | 采用 Rust `ccxt` 实现扩展层通用连接器 | 若 `ccxt` crate 编译过重或不稳定，保留原生 REST 实现并记录于 `OSS-05` |

## 5. 工作包与依赖顺序

真实依赖顺序（`M1-W05` 的导入与账本核心不依赖连接器，提前做以尽早暴露金额问题）：

```text
M1-W01 ─┬─ M1-W02（标的/日历/汇率）─┬─ M1-W03（加密）─┐
        │                            └─ M1-W04（美股）─┼─ M1-W05b（估值与多币种对账）─ M1-W06 ─ 扩展层
        └─ M1-W05a（导入引擎、FIFO、分录、账本断言）─────┘
```

B 一次接手连续执行全部必做工作包并自检，再执行扩展层，不逐包停顿询问。先打通一条最薄端到端切片（建库 → 通用 CSV 导入 AC-02 → 一个加密源与一个美股源同步 → 报告 → 备份恢复），再横向补齐各来源与格式；状态必须真实报告，未完成项保持 failed/blocked，不得缩减第 6 节必做项。

| 工作包 | 交付内容与模块位置 | 正常路径验收 | 失败/边界路径验收 | 对应需求与 AC |
| --- | --- | --- | --- | --- |
| M1-W01 | 建立纯 Rust Cargo workspace（`crates/delta-core`、`crates/delta-data`、`crates/delta-app`、`crates/delta-cli`，包元数据统一 `license = "Apache-2.0"`，见 D-24）与 `rust-toolchain.toml`；SQLite 迁移、资料库目录与 `manifest.json`、Backup API 一致性备份/恢复；S-02 异步作业边界与 S-04 凭据存取骨架 | 建库、迁移、备份、恢复到新目录后哈希与外键校验一致；`Decimal` 字符串无损往返 | 迁移中断或损坏库拒绝覆盖原库；恢复目标非空拒绝执行；凭据不出现在日志与配置明文；以跨进程写入/读取/删除测试证明 `keyring` 实际使用 Windows 凭据管理器原生后端，而非内存或模拟后端（只操作 `delta-test-<run-id>` 前缀条目并清理）；S-10 静默进程助手、回环测试服务助手与 `clippy.toml` 禁用规则在本包建立，后续包直接复用；S-11 位置解析：三通道默认路径、覆盖优先级、`<文档>` 经系统登记位置解析、不可写报错不回退、第二个进程拿不到 `delta.lock` 立即报错的测试全部通过，且在临时 `LOCALAPPDATA` 与 `DELTA_DOCUMENTS_DIR` 下运行后临时根之外零新建目录 | FR-BASE-01/02、FR-OPS-01/02/03/04、NFR-01/06/08；AC-09 |
| M1-W02 | 标的与跨源别名注册表（`NASDAQ:AAPL ↔ US.AAPL ↔ 105.AAPL ↔ AAPL.US`、`OKX:BTC/USDT`、`BINANCE:BTC/USDT`）、S-06 纯 Rust 日历引擎与查询、Frankfurter 法币汇率与 3.1 节稳定币汇率同步、S-07 汇率时点规则 | 冬夏令时、半日市、节假日与加密 UTC 日切准确；同版本逆汇率可推导；`carried_forward` 标记正确 | 区分法定休市、`coverage_gap` 与 `calendar_unknown`；缺汇率或稳定币脱锚显式标记，不假定 `1.0` | FR-DATA-03、FR-MKT-04、FR-LED-02；AC-06、AC-21 |
| M1-W03 | 加密现货连接器（3.1 节必做：**欧易 OKX 公开现货 REST** + **币安公开行情镜像与归档**）+ `AccountProvider` 夹具实现（OKX / Binance）；`sync_checkpoint` 统一水位线 | 免 Key 增量拉取 `OKX:BTC/USDT`、`BINANCE:BTC/USDT`、`ETH/USDT` 日线；仅提交已完结 K 线，水位线在同一事务落盘后推进；归档 CHECKSUM 不符拒收 | 429/5xx 退避、断流重传幂等去重；余额快照与流水不一致生成 `reconciliation_issue` 而不篡改账本 | FR-DATA-03/04/05、FR-MKT-01；AC-17、AC-22 |
| M1-W04 | 美股/ETF 纯 Rust 连接器与公司行动（Yahoo Chart v8 + 东财 `push2his` + Tiingo）；交叉核验器 | 按 S-08 入库原始价（Yahoo 还原 + 东财 `fqt=0` 核验）、拆股与现金分红，复权序列派生；按 `dataset_version` 固定查询可复现（S-09） | 单源复权异常或缺失拆股/分红时检出跳空差异并标记 `partial/warning`；连接器超时/取消/异常响应被 `delta-app` 作业控制器安全隔离（AC-16） | FR-DATA-03、FR-MKT-04、FR-LED-08；AC-05、AC-06、AC-16 |
| M1-W05（a 导入与账本核心 / b 估值与对账） | 账单导入（3.1 节必做格式：**富途牛牛 CSV**、**欧易 OKX CSV**、通用 CSV、IBKR Flex XML、币安 CSV）：映射、预览、幂等提交、冲销更正；FIFO 成本、第三资产手续费、跨账户搬移、多币种净资产/损益分解 | AC-01～07、AC-22 精确对齐手算值；富途与欧易账单夹具完整走通入账与费用分摊 | 同文件重导幂等跳过；跨文件同秒同价生成疑似重复待核对；超卖、未知成本或预览后文件被篡改明确拒绝 | FR-DATA-01/02/05、FR-LED-01～06/08；AC-01～07、AC-22 |
| M1-W06 | 自动化编排（导入富途/欧易等账单后自动订阅所需标的与汇率对 → 日历水位线增量同步 → 新 `dataset_version` → 对账/损益报告）；Rust 指标与截断测试；**AC-20 子集**：`delta export`（账本/行情/汇率 CSV + JSON 清单与哈希）、`delta library delete` 预览与确认、诊断日志脱敏；纯 Rust 阶段验证入口 `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6`（包 `delta-cli`、二进制名 `delta`；live 探针为 `#[ignore]` 测试，只由 `verify-stage --live` 运行并单独统计）；锁定 `Cargo.lock` 后生成依赖与许可清单（`OSS-06`） | 一条 CLI 流程完成“建库 → 导入 → 自动同步 → 报告 → 备份恢复校验”；向未来注入极端哨兵值不影响当前指标；导出可被独立复核 | 后台同步作业超时/取消/故障隔离后主程序保持可用且结果不标成功（AC-16）；删除预览与实际删除范围一致、无残留引用；日志无 Token、完整账户号；构建、测试、文档门禁零警告；S-10 全部自动证据通过：窗口哨兵零窗口、注入崩溃子进程无对话框且报告 failed、空 `stdin` 下确认命令在超时内退出、控制字符转义、非回环绑定扫描为零，报告记录实际优先级与并发数 | FR-MKT-02、FR-OPS-03/04、NFR-02/05/07/10；AC-11、AC-16、AC-20 |
| M1-EXT | 3.1 节扩展层（老虎 Tiger / 长桥 Longbridge / 嘉信 Schwab / 东财 / IBKR Activity CSV 预设、Alpaca REST、POC-D06、扩展指标） | 各扩展项有回放/夹具测试与 live 探针记录 | 未完成项逐项登记 `LOOP-03`（原因、归属、触发条件） | 同上述对应 FR |

## 6. 结束标准、不得缩减项与明确边界

- **B 完成判定**：M1-W01～W06 全部完成且 `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6` 通过；扩展层已完成或逐项登记 `LOOP-03`；所有可达免 Key 端点的 live 探针已实跑并记录（含失败码）；需 Token 的 live 验证标 not-run 并引用 `ACT-03`。
- **不得缩减**：100% 纯 Rust 实现（无 Python 运行期依赖）、用户两大最高频主力（富途牛牛 CSV + 欧易 OKX 行情/CSV）必做支持、Decimal 金额精度、S-08 原始价口径、S-09 按版本可复现、S-10 前台无干扰（窗口哨兵零窗口）、S-11 通道与数据位置隔离、AC-01～07 及 AC-09/11/16/17/20（子集）/21/22 的核心断言、`USD`/`USDT`/`USDC` 独立估值、拆股/分红与复权口径一致性、水位线原子推进、一致性备份与恢复、S-02 异步边界、S-04 凭据规则。
- **本阶段不建**：GPUI 或任何桌面窗口、任何 Python 子进程或脚本包、Parquet/DuckDB 行情仓（`OSS-10`）、自由画线工具、K 线训练交互（M3）、策略回测（M4）、AI 聊天界面与模型客户端（M2）。
- **在线与录制分离声明**：所有连接器的解析、分页、限流、去重与交叉核验必须由受控回环服务与真实结构录制夹具 100% 自动验证；live 探针默认关闭、以显式开关运行，结果单独记录，绝不把回放写成真实在线验收。录制夹具不得含任何账户标识或 Token。`verify-stage` 调用 `python scripts/check_docs.py` 属开发期文档门禁，不构成产品运行期 Python 依赖（D-22 约束产品代码与运行期）。
- **真正的人工依赖**：本机 Rust/MSVC 工具链（`ACT-07`）已就绪无阻塞；Tiingo（及扩展层 Alpaca/Longport）Token 的 live 验证与真实个人去标识账单样本（`ACT-03`）不阻塞 M1 工程收敛。

## 7. 交付产物与可复制 A → B 执行 Prompt

B 完成后更新 `docs/delivery/status.md`、新建 `docs/delivery/stage-report.md`（在 `document-registry.json` 登记），同步受影响正本（数据模型、架构、台账），将全部修改 `git commit --amend` 进当前阶段提交，随后输出 B → A 审核 prompt。

```text
你是 DELTA 的 Agent B。执行唯一计划 docs/delivery/stage-plan.md（M1 v2.6），以第 3 节 S-01～S-11、3.1 节必做/扩展分层与第 4～6 节为准，不重新选型、不缩减第 6 节必做项。
入口：AGENTS.md、docs/delivery/status.md、本计划，以及计划引用的数据模型、金融规则、系统架构、质量门禁和 USER-ACTIONS.md。
基线：分支 codex/m1-data-foundation，阶段父提交 8329f638536e4f303764045a2f10a2c1ab31fcd3，阶段提交完整 SHA 以交接消息为准并用 git rev-parse HEAD 核对；保留未跟踪的 workers/agent/ 不触碰；ACT-07 工具链已就绪。
范围：按第 5 节依赖图连续完成 M1-W01～W06（先打通最薄端到端切片），再执行 M1-EXT，未完成扩展项登记 LOOP-03；不逐包询问。重点自证 S-08（AAPL 2020-08-28 还原原始收盘 499.23）、S-09 按版本查询、Binance 归档微秒时间戳、1990～2027 日历逐日对照、S-10 前台无干扰（窗口哨兵零窗口；你自己的构建与测试命令同样不弹窗、不打开浏览器或资源管理器）、S-11 数据位置（路径一律小写 delta；源码运行与测试不碰 %LOCALAPPDATA%\delta 正式数据，手工运行用 DELTA_DATA_DIR=.local/dev-data）。
检查：建立并运行 cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6；live 探针以 --live 显式运行并如实记录（含失败码），Tiingo 与真实账单样本保持 not-run（ACT-03）；离线回放不得写成在线通过。
交付：新建并登记 docs/delivery/stage-report.md，更新 status.md、受影响正本与台账，全部 git commit --amend 进当前 M1 阶段提交（父提交不变），给出新完整 SHA 与 B→A 审核 prompt。不执行 push / PR。
```
