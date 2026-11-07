# 2026-11-07：GPUI 合理性复盘与金融数据底座调研证据

类型：reference；执行者：Agent A；基线：文档治理基线（按 D-20 放弃此前已构建的桌面先行代码与产物）。机器可核对元数据见 [2026-11-07 上游快照](2026-11-07-upstream-snapshot.json)。

## 1. 为什么原路线“先构建 GPUI 桌面应用”不合理

结合 2026-11-07 最新上游事实与前序废弃构建的实测教训，直接以 GPUI 作为首轮主轴存在四项结构性不合理：

| 维度 | 实证事实（2026-11-07） | 对 DELTA 的真实影响 |
| --- | --- | --- |
| 上游底座与快照耦合 | crates.io 官方 `gpui` 停在 `0.2.2`（2025-10-22）；`gpui-kit 0.9.0`（2026-11-02）与本地参考 `0.8.1`（SHA `56e9153c`）被迫精确锁死 Zed 快照包 `gpui-pre = "=0.3.10"` / `"0.3.11"`，并在注释中明示周度快照会破坏编译（`#3156`）；同时引入 `smol 2` 与 `gpui-pre-reqwest 0.12.15`，与本项目的 `tokio 1.54.1` + `reqwest 0.13.5` 双栈并存 | GPUI 并非面向第三方桌面应用承诺语义化版本稳定的独立 GUI 库；每次升级组件库都伴随底层快照与异步/HTTP 传递依赖震荡 |
| 金融图表能力严重错位 | `gpui-kit` 的 `candlestick_chart.rs`（497 行）只是基于 `ScaleBand` 的静态分类柱图：每次全量遍历 `self.data` 绘制，无滚轮缩放、拖拽平移、10 万根 K 线按窗裁剪、成交量/MACD/RSI 联动副图、买卖/笔记标记和训练逐根回放游标 | 在 GPUI 中实现专业复盘与 K 线训练工作台，等价于从零自研并维护一套 GPU 金融图表引擎，违背 D-02“优先复用成熟开源、聚焦金融语义整合”的原则 |
| Windows 平台工程摩擦与依赖膨胀 | 复杂 GPUI 视图树在 Windows MSVC 未优化/测试构建下触发主线程 1 MiB 栈溢出（被迫链接 `/STACK:8388608`）；隐藏 Direct3D 表面受限于系统初始尺寸，无法自动覆盖真实中文 IME、125%/150% DPI 与屏幕呈现帧率；仅桌面层就引入约 580+ 传递 crate（含 `syntect`、`lsp-types`、`ropey` 等编辑器依赖） | 大量工程精力被消耗在 GUI 布局栈、离屏渲染 hack 与数百个传递依赖的许可证清查上，而真正决定金融工作台价值的“真实美股/加密数据获取与对账”却停留在合成 JSON/CSV |
| 研发优先级倒置 | 金融账本、估值、复权、汇率、回撤与 K 线复盘的全部计算前提是**可信、完整、可增量更新的金融数据** | 先做 UI 会产生没有真实数据源支撑的空壳页面；必须反转为“先解决金融数据来源与本地自动化数据底座（M1），再接入账本与界面（M2+）” |

### UI 技术栈重评与 D-21 确认结论

1. **M1（当前阶段）完全剥离桌面 GUI 依赖**：只构建 `delta-core`（Decimal 领域模型）、`delta-data`（统一数据源、同步水位线、SQLite 控制面 + Parquet/DuckDB 行情仓；存储已由第 4 节修订为 M1 SQLite 单库）、`delta-cli`（数据拉取、导入、对账、质量诊断 CLI）及按需 Python 数据连接器进程。
2. **为什么之前 `0.6.6` 效果不佳，以及 M2 如何正确使用长桥官方 `gpui-kit` 面板体系（D-21 已确认）**：
   - **`0.6.6` 的来源**：来自 `2026-09-26` 初始调研快照（`docs/evidence/2026-09-26-upstream-snapshot.json`）。前序废弃构建锁死在旧版 `0.6.6` 并手写绝对定位 `div`，既没有金融数据底座支撑，也未采用长桥在最新 `gpui-kit`（本地参考 `0.8.1` / crates.io `0.9.0`）中成熟的官方 `DockArea` 面板系统。
   - **长桥官方 `gpui-kit` 已开源与未开源的边界**：`gpui-kit` 完整开源了商业级 **`DockArea` + `DockLayout` + `DockSkin` + `BasePanel` / `Panel` 可停靠多面板工作区**（见 `dock_story.rs`）、**`DataTable` / `VirtualList` 数十万行虚拟滚动高密度表格**及 **`Card` + 概览图表**；公开仓库中未见带缩放/拖拽视口状态机的交易 K 线面板容器（`chart_story.rs` 中的 `CandlestickChart` 默认面向 40 根日线静态卡片展示）；“Longbridge Pro 专有容器未开源”是 A 的推断，非上游声明。
   - **M2 已确认方案（D-21，关闭 `Q-03`）**：M2 继续使用 GPUI，升级至长桥最新 **`gpui-kit`（`0.8.x` / `0.9.x`）**，100% 按照官方 `DockArea` + `Panel` + `DataTable` + `Card` 范式构建各业务面板，并在主 K 线与回放 `Panel` 外层封装 GPUI 视口滑动窗口（`visible_range: Range<usize>`，响应滚轮缩放可视根数与鼠标拖拽平移时间窗口），每帧只从本地 DuckDB/内存切片当前可见的 40～200 根 K 线进行渲染。

## 2. 美股/ETF 与加密现货数据接口及开源生态调研

围绕用户提出的“美股、加密有哪些可用接口与开源项目，以及如何与应用整合而非手动维护”，2026-11-07 查证结果如下：

### 2.1 美股与 ETF 数据源（日线 OHLCV、拆股、分红、交易日历）

| 方案 / 项目 | 最新版本与许可（2026-11-07） | 覆盖内容与优势 | 局限与边界 | DELTA 角色定位 |
| --- | --- | --- | --- | --- |
| **Tiingo REST API**（Rust 直连 / Python `tiingo 0.16.1`，MIT） | REST v1 / PyPI `0.16.1` | 免费层即可获取深度美股/ETF 日线，单次响应同时返回原始 `open/high/low/close`、复权 `adjOpen..adjClose`、现金分红 `divCash` 与拆股比例 `splitFactor` | 需免费注册 API Token；受免费层小时/日请求配额约束 | **美股日线与公司行动首选官方免费 API**（Rust 原生 `reqwest` 适配，无需额外重依赖） |
| **Alpaca Market Data API**（Rust 直连 / `alpaca-py 0.45.0`，Apache-2.0） | PyPI `0.45.0`（2026-11-05） | 免费开发者账户支持美股/ETF 历史日线/分钟线及 `/v1/corporate-actions`（拆股、现金/股票分红、并购） | 需免费 API Key；免费实时流仅 IEX，历史日线与公司行动对复盘足够 | **美股日线与独立公司行动端点并列首选 API** |
| **AKShare**（`akshare 1.20.35`，MIT；本地参考 `1.20.22`） | PyPI `1.20.35`（2026-11-06） | 免 Key；`stock_us_hist`（东方财富 `push2his`）支持美股/ETF 原始、前复权 `qfq`、后复权 `hfq` 日/周/月线；`stock_us_daily`（新浪）含 `qfq-factor`；`forex_hist_em` 支持外汇历史 | 东财接口需内部 `secid` 前缀（如 `105.AAPL`）且无结构化拆股/分红明细表；新浪接口依赖 `py_mini_racer`（V8）且源码注释明示 `CIEN`/`AI` 复权因子错误；无美股交易日历 | **免 Key 零配置兜底与交叉核验源**（通过受控 Python 数据进程调用，自动维护 `NASDAQ:AAPL ↔ 105.AAPL` 映射，不单独充当公司行动唯一真相） |
| **yfinance**（`yfinance 1.8.0`，Apache-2.0） | PyPI `1.8.0`（2026-11-03） | 免 Key；同时提供美股/ETF 历史 OHLCV 与离散的 `splits`、`dividends`（`actions`）序列及基础汇率 | 非官方 Yahoo 接口，存在 crumb/限流波动，不可用于生产实盘交易 | **免 Key 历史 OHLCV 与拆股/分红交叉校验源** |
| **Longport OpenAPI**（Rust `longport 4.3.7` / Python，MIT OR Apache-2.0） | crates.io `4.3.7`（2026-08-12） | 官方 Rust SDK，支持美股/港股/ETF K 线、复权与只读账户资产/成交/流水同步 | 需长桥账户与对应行情权限 | **长桥用户的一体化行情 + 只读账户同步源** |
| **Polygon (Massive) / Finnhub / Twelve Data / Databento** | `polygon-api-client 1.16.3`、`finnhub-python 2.4.30`、`twelvedata 1.4.0`、Rust `databento 0.64.0` | 专业/商业级历史与实时行情、拆股分红接口 | 免费层历史深度或日请求数受限（Databento 按量计费） | **可选增强 Provider**（共用统一 `MarketDataProvider` 契约） |
| **exchange_calendars / pandas_market_calendars** | PyPI `4.13.2`（Apache-2.0）/ `5.6.0`（2026-10-31，MIT） | 权威 `XNYS`（NYSE）、`XNAS`（NASDAQ）节假日、半日市（如感恩节次日 13:00 ET 提前休市）与 `America/New_York` 夏令时切分 | Python 库；需导出或由 worker 同步至本地 SQLite 日历表 | **美股交易日历与休市/缺数判定的权威来源** |

### 2.2 加密现货与稳定币/法币汇率数据源

| 方案 / 项目 | 最新版本与许可（2026-11-07） | 覆盖内容与优势 | 局限与边界 | DELTA 角色定位 |
| --- | --- | --- | --- | --- |
| **CCXT**（Rust `ccxt 4.5.92` / Python `ccxt 4.5.92`，MIT） | `4.5.92`（2026-11-04） | 覆盖 100+ 加密交易所（Binance、OKX、Coinbase、Kraken、Bybit 等）；**公共行情 `fetch_ohlcv`（如 `BTC/USDT`、`ETH/USDT`、`USDT/USD`、`USDC/USD`）完全免 API Key**；配置只读 Key 后支持 `fetch_my_trades`、`fetch_balance`、`fetch_deposits`、`fetch_withdrawals` | 不同交易所历史分页窗口、限流和手续费币种字段需规范化 | **加密现货行情与只读账户同步的核心标准底座** |
| **Binance Public Data (`data.binance.vision`) & 交易所公开 REST** | 官方公开归档 / REST | 免 Key、高吞吐；提供现货日线/分钟线 ZIP 归档与 `.CHECKSUM`（SHA-256），适合快速拉取数年完整历史 | 仅覆盖对应交易所自身交易对 | **Rust 原生大批量加密历史 K 线高速同步源** |
| **ECB / Frankfurter / FRED + Coinbase/Bitstamp/Kraken 稳定币对** | 官方公开接口 | 法币汇率（`USD/CNY`、`USD/HKD`、`EUR/USD` 等）免 Key 获取；稳定币汇率从真实美元现货对获取（第 4 节修正：Coinbase 仅有 `USDT-USD`，`USDC/USD` 取 Bitstamp/Kraken） | 需区分法币工作日与加密 24×7 时点 | **多币种账本与稳定币独立估值（FR-LED-02 / AC-06）的标准汇率源** |

### 2.3 开源数据平台与本地量化底座参考

- **OpenBB Platform（`openbb 5.0.0`，Apache-2.0）**：采用 `Provider -> Standard Model (EquityHistorical / CryptoHistorical /Calendar)` 的插件化架构。DELTA 借鉴其“标准输入/输出模型 + 多源可切换路由”的设计，但不直接捆绑整个 OpenBB 庞大依赖树，而是保持 Rust 宿主契约轻量可控。
- **NautilusTrader（Rust 核心 + Arrow/Parquet 数据目录）与 DuckDB（`1.20000.1`）+ Parquet（`60.1.0`）**：业界本地优先金融数据底座的标准模式——**SQLite 负责控制面（标的映射、水位线、数据版本清单、公司行动、账本事务），Parquet/DuckDB 负责分析面（Bronze 原始快照 → Silver 清洗去重后的 K 线分片）**。
- **券商/交易所账单解析（`ibflex 1.1` / 标准 CSV 适配器）**：针对盈透证券（IBKR Flex Query / Activity CSV）、嘉信/长桥/富途 CSV、Binance/OKX/Coinbase 现货成交与充提流水，提供预设 schema 适配器，免去用户每次手调列映射。

## 3. 数据获取如何与应用结合（告别手动维护）

数据获取绝不能做成“用户自己在外部跑脚本、手动下载 CSV 再反复导入”的割裂工具，而应作为应用内置的 **自动化本地数据平面（`delta-data`）**：

1. **账本与自选驱动的自动订阅（Ledger-Driven Auto-Subscription）**：当用户导入券商/交易所流水（如出现 `NASDAQ:AAPL`、`NYSE:SPY`、`BINANCE:BTC/USDT`）或添加自选标的时，系统自动将该标的、计价资产及所需汇率对（如 `USDT/USD`、`USD/CNY`）加入本地同步清单。
2. **基于交易日历的水位线增量同步（Watermark + Calendar-Aware Incremental Sync）**：SQLite `sync_checkpoint` 记录每个 `(instrument_id, timeframe, source, adjustment)` 的最后已提交时间。应用启动、用户点击“刷新数据”或后台定时任务触发时，只拉取水位线之后的增量 K 线、新增分红/拆股和汇率，结合 `calendar_session` 自动区分“周末/节假日休市”与“真的数据缺失（`coverage_gap`）”。
3. **多源降级与交叉核验（Fallback & Cross-Check Pipeline）**：
   - 美股日线默认走 **Tiingo / Alpaca**（配置免费 Token 时）或 **yfinance + AKShare 交叉验证**（零配置免 Key 模式）：当免 Key 源出现复权差异（如拆股日跳空但缺失拆股记录）时，自动标记 `quality_status = warning/partial` 并提示补全，绝不静默产出错误成本或收益率。
   - 加密现货默认走 **Rust 原生公开 REST / CCXT 免 Key 拉取**，自动同步 `USDT/USD` 与 `USDC/USD` 真实日线汇率。
4. **不可变数据版本快照（`dataset_version`）**：每次同步落盘后生成带内容哈希的 `dataset_version` 清单。账本估值、图表展示、后续训练回放与 AI 证据问答全部引用该快照 ID，实现“一次自动拉取，全模块确定性复用”。

## 4. A 自审复核与在线实测（2026-11-07 v2.1/v2.2）

执行者：Agent A（自审修订 M1 计划至 v2.1，并按用户 D-22“全项目彻底纯 Rust”升级至 v2.2）。网络：用户确认本机经 TUN 代理以美国出口访问外网（探针显示 `US / California`）。以下为一次性 HTTP 探针结果，属于 A 的调研实测，不是 B 的连接器验收；机器可读记录见 [上游快照](2026-11-07-upstream-snapshot.json) 的 `liveProbes2026_11_07`。

| 端点 | 结果 | 对计划的影响 |
| --- | --- | --- |
| `api.binance.com/api/v3/klines` | HTTP 451（受限地区） | 不作为数据源；Binance.com 账户 API 在美国出口同样不可用，M1 账户走 CSV |
| `data-api.binance.vision/api/v3/klines` | 200，返回 BTCUSDT 日线 | 加密增量同步主路径 |
| `data.binance.vision/.../BTCUSDT-1d-2026-11-01.zip.CHECKSUM` | 200，SHA-256 文本 | 归档回填并校验 |
| Coinbase Exchange `products/USDT-USD` / `USDT-USD/candles` | `online`；2022-01 区间返回日线 | `USDT/USD` 主源（300 根/页分页） |
| Coinbase Exchange `products/USDC-USD` | 404 | Coinbase 将 USDC 与 USD 1:1 兑换，无独立市场；不得作为 `USDC/USD` 来源 |
| Bitstamp `ohlc/usdcusd`、`ohlc/usdtusd`（`start=2022-01-01`） | 200，自 2022-01-01 起日线 | `USDC/USD` 主源、`USDT/USD` 核验 |
| Kraken `OHLC?pair=USDCUSD&interval=1440&since=2017` | 200，但仅返回 721 根（首日 2024-11-17） | 公开 OHLC 只给最近约 720 根，只能做近期核验 |
| Frankfurter `v1/latest`、`v1/2000-01-03` | 200；2000 年有 `HKD` 无 `CNY` | ECB 的 `CNY` 自 2005 年起，更早区间必须显式缺失 |
| Yahoo `v8/finance/chart/AAPL?interval=1d&period1=1596240000&period2=1599200000&events=div,split` | 200，单次 JSON 返回 24 根日线 + `dividends(0.205)` + `splits(4:1)` | 纯 Rust `reqwest` 直连即可获取 OHLCV、复权价、拆股、分红，无需 Python `yfinance` |
| 东财 `push2his.eastmoney.com`（`secid=105.AAPL`） | 200，返回原始与复权日线 JSON | 纯 Rust `reqwest` 直连即可作为美股核验源，无需 Python `akshare` |
| OKX `market/candles`、`market/history-candles`（`bar=1Dutc`）及 `public/instruments` | 200，返回 UTC 日线与 `confirm` 完结标识、`tickSz`/`lotSz` | 用户第一加密主力（D-23），纳入 M1 必做公开现货连接器 |
| Tiingo `tiingo/daily/aapl`（无 Token） | 403 | 需 Token；M1 以录制夹具验证，live 待 `ACT-03` |

注册表与文档补充核查：

- crates.io `ccxt 4.5.92`（2026-11-04，MIT，仓库 `ccxt/ccxt`）为官方转译 Rust 包，按交易所 feature 裁剪（`all`、`binance`、`okx`、`kraken` 等），依赖 `ccxt-base`；作为扩展层 POC-D06 候选。
- crates.io `gpui-kit 0.9.0` 发布于 2026-11-02 13:43:39 UTC，依赖 `gpui-pre =0.3.11`、`gpui-pre-platform =0.3.11` 等精确快照；`0.8.1` 发布于 2026-10-26（本地参考副本为其源码，锁定 `=0.3.10`）。
- Alpaca 历史公司行动的稳定端点为 Market Data `GET /v1/corporate-actions`；`v1beta1` 仅用于部分流式/预览路径，旧 Trading API `/v2/corporate_actions/announcements` 已弃用。

自审结论与修订：按用户 **D-22** 全项目彻底采用 **100% 纯 Rust 实现**（移除 `workers/python` 与运行期 Python 依赖，关闭 `OSS-03`）；按用户 **D-23**（关闭 `Q-01`）将最高频主力 **富途牛牛 CSV** 与 **欧易 OKX 公开现货 REST（`bar=1Dutc`）+ 现货账单 CSV** 提升为 M1 必做（兼顾老虎、IBKR、长桥、嘉信、东方财富、币安）；本机 MSVC + `rustup` 环境复核通过（关闭 `ACT-07`）；M1 存储收敛为 SQLite 单库（Parquet/DuckDB 转 `OSS-10`）；`delta-app` 对外采用同步命令/作业句柄以兼容 M2 GPUI 执行器（`OSS-09`）；连接器按“必做/扩展”分层；补齐凭据、数据条款、汇率非发布日规则（A-07）与 AC-20 子集交付；清理 D-21 后残留的 Tauri 推荐表述。详见 [M1 计划 v2.3](../delivery/stage-plan.md)。

## 5. A 计划复审实测（2026-11-07 v2.4）

本节为 A 复审 M1 计划 v2.3 时的补充实测（同一美国网络出口，裸 HTTP，未经产品代码）：

- **Yahoo Chart v8 价格口径**：`/v8/finance/chart/AAPL?period1=1598486400&period2=1598918400&interval=1d&events=div,split` 返回 2020-08-28 `close=124.80750274658203`、`volume=187630000`，同时返回 2020-08-31 `splitRatio=4:1` 事件。东方财富 `push2his`（`secid=105.AAPL`、`fqt=0`）同日收盘 `499.23`。结论：Yahoo 的 OHLCV 为拆股调整值（成交量同样按拆股放大），不是原始价；JSON 价格为浮点噪声值。→ 计划 S-08 / 默认 A-08。
- **Binance 归档时间戳单位**：`data.binance.vision/data/spot/daily/klines/BTCUSDT/1d/BTCUSDT-1d-2025-03-01.zip` 首列开盘时间为 `1740787200000000`（微秒），`.CHECKSUM` 为 `1019e8a1…6b81`。2025 年前归档为毫秒。→ 计划 3.1 节与 POC-D03。
- **日历对照数据**：`exchange_calendars` 仓库数据文件 `tests/resources/xnys.csv`（HTTP 200，Apache-2.0）含每个交易时段开收盘 UTC 时刻，覆盖 `1990-01-02`～`2023-01-13`。可作为纯数据测试夹具，不引入 Python。→ 计划 S-06 / POC-D02。
- **crates.io**：`keyring` 最新稳定版 `4.2.0`。各版本对平台后端的启用方式不同，计划要求以跨进程持久化测试证明实际使用 Windows 凭据管理器。

复审其他结论（非实测）：`dataset_version` 每次同步若复制 `bar` 行会使单库膨胀，改为有效版本区间（S-09）；`M1-W05` 账本核心不依赖连接器，依赖图改为与 `M1-W02` 并行；`rust-toolchain.toml` 精确锁定 `1.99.1`；A→B prompt 按 [协作流程](../dev-rules/development-workflow.md#agent-handoff) 改为只给角色、入口、基线、范围、检查与交付，不复制验收。
