# DELTA 技术栈与通用能力比较

版本 1.3 · 2026-11-07 · Agent A 已按 [D-20 ～ D-23](../decisions/confirmed-decisions.md) 完成技术栈重评与金融数据底座开源调研；v1.2/v1.3 按用户 D-22 确立**全项目 100% 纯 Rust 实现**（彻底移除 Python 运行期依赖），并按 D-23（核心主力：**富途牛牛 + 欧易 OKX**；常用：老虎、IBKR、长桥、嘉信、东方财富、币安）及美国网络出口在线实测定案 M1 存储与纯 Rust 连接器（见 [M1 计划第 3 节](../delivery/stage-plan.md#3-a-已定案的阶段技术决定b-不再重新选型)）。详细实证见 [2026-11-07 调研报告](../evidence/2026-11-07-stack-and-data-research.md)、[2026-11-07 上游快照](../evidence/2026-11-07-upstream-snapshot.json) 及历史对照 [2026-09-26 调研报告](../evidence/2026-09-26-technology-research.md)。

## 1. 核心结论：100% 纯 Rust，数据底座优先，M2 使用新版 GPUI

1. **全项目 100% 纯 Rust 实现（D-22）**：
   - 采用 **纯 Rust 核心（`delta-core` / `delta-data` / `delta-app` / `delta-cli`）+ SQLite 单库（控制面、事务账本与日线行情）**；不引入 `workers/python` 或任何运行期 Python 子进程。
   - 调研中核查的 Python 库（如 `yfinance`、`akshare`、`exchange_calendars`）仅借用其**公开 HTTP 端点参数语义与日历规则对照**，在 DELTA 中全部由 Rust `reqwest` + `serde_json` 与 `chrono` + `chrono-tz` 原生实现。
   - Parquet/DuckDB 推迟到 M5 批量扫描或实测性能不足时引入（`OSS-10`）。
2. **优先解决金融数据来源与本地自动化数据平面（M1，D-20）**：
   - **M1 零桌面 GUI 依赖**：先把美股/ETF、加密现货、法币/稳定币汇率、公司行动（拆股/分红）、交易日历及券商/交易所账单导入与增量同步跑通并验证。
   - **与 M2 GPUI 兼容的异步边界**：`delta-app` 对外只提供同步命令/查询与后台作业句柄，不暴露 `tokio`/`reqwest` 类型；GPUI 自带执行器，可直接调用而无需桥接两个运行时。
3. **GPUI 合理性复盘与新版 `gpui-kit` 官方面板采用结论（见 [ADR-0003](../decisions/0003-first-round-stack.md)、D-21 与 `OSS-01` / `OSS-09`）**：
   - **`0.6.6` 的来源与历史教训**：`0.6.6` 来自 `2026-09-26` 初始调研快照（`docs/evidence/2026-09-26-upstream-snapshot.json`），前序废弃代码锁死在旧版 `0.6.6` 并手写绝对定位布局，且当时尚未完成金融数据底座，导致界面空转；同时 `gpui-kit` 内置的 `CandlestickChart`（497 行）默认面向 40 根日线静态卡片展示，若不加视口切片直接塞入数千根历史 K 线会导致挤压。
   - **已确认方案（D-21，关闭 `Q-03`）**：**M1 先完成纯 Rust 金融数据底座；M2 继续使用 GPUI，摒弃旧 `0.6.6` 锁定，升级至长桥最新 `gpui-kit`（本地参考 `0.8.1` / crates.io `0.9.0`，后者精确锁定 `gpui-pre =0.3.11`）**，直接按照长桥官方 `DockArea` + `DockLayout` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系构建桌面工作台，并在主 K 线与回放 `Panel` 外层封装 GPUI 视口滑动窗口（`visible_range: Range<usize>` 滚轮缩放与拖拽平移切片）。快照锁定、Windows 构建与依赖规模等 M2 集成风险在 `OSS-09` 跟踪。

## 2. 金融数据来源与纯 Rust 连接器选型（M1 核心）

“必做/扩展”分层以 M1 v2.6 计划 3.1 节（PR #1 版本，Git 历史归档；实际结果见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)）为准；下表记录比较与角色。

| 领域 | M1 纯 Rust 首选（已在线实测） | 备选 / 扩展方案（纯 Rust） | DELTA 整合与自研边界 |
| --- | --- | --- | --- |
| 美股/ETF 历史日线与公司行动（拆股/分红） | 零配置（Rust `reqwest` 直连公开 JSON）：**Yahoo Finance Chart v8**（`query1.finance.yahoo.com/v8/finance/chart/{symbol}?interval=1d&events=div,split`，返回拆股调整后的 OHLCV、`adjclose`、`splits`、`dividends`，原始价须按 M1 计划 S-08 还原）主 + **东方财富美股日线接口**（`push2his.eastmoney.com/api/qt/stock/kline/get`，`secid=105.AAPL` 等，`fqt=0` 原始价、`fqt=1/2` 复权）核验；配置 Token：**Tiingo REST**（原始/复权日线、`divCash`、`splitFactor`）主 + Yahoo Chart v8 核验 | **Alpaca Market Data REST**（`/v2/stocks/bars` + `/v1/corporate-actions`，扩展层）；长桥用户可选 **Longport Rust SDK 4.3.7**；专业用户可选 **Databento Rust SDK 0.64.0** / Polygon / Finnhub REST | 统一 `MarketDataProvider` 接口；自动维护 `NASDAQ:AAPL ↔ US.AAPL ↔ 105.AAPL ↔ AAPL.US` 别名映射；交叉校验拆股/分红缺口，异常时显式标记 `partial/warning` |
| 加密现货行情 | **欧易（OKX）公开现货 REST**（用户第一主力：`www.okx.com/api/v5/market/candles` 与 `history-candles`，`bar=1Dutc`，带 `confirm` 完结标识）+ **币安（Binance）公开行情**（增量 `data-api.binance.vision/api/v3/klines`，回填 `data.binance.vision` 归档 + `.CHECKSUM` SHA-256 校验）。美国出口下 `api.binance.com` 返回 451，不使用 | **官方 Rust `ccxt 4.5.92` crate**（MIT，按交易所 feature 裁剪）作为扩展层多场所通用连接器，经 POC-D06 验证 | 统一 `VENUE:BASE/QUOTE`（`OKX:BTC/USDT`、`BINANCE:BTC/USDT`）身份；仅提交已完结 K 线，分页游标在本地事务落盘后推进；只读硬约束，绝不注册下单接口 |
| 加密只读账户 | `AccountProvider` trait + 录制夹具（覆盖 **OKX V5** 与 **Binance** 形态，M1 验证 AC-17） | 用户提供 Key 后的真实只读 REST 同步（OKX V5 等，扩展/后续） | 余额与流水差异生成 `reconciliation_issue`，不改账本 |
| 法币与稳定币汇率（FX） | 法币：**Frankfurter REST**（`api.frankfurter.dev/v1`，ECB 参考汇率，`USD/HKD`、`USD/CNY` 自 2005 年起）；稳定币：`USDT/USD` = **Coinbase Exchange** 主 + **Bitstamp** 核验，`USDC/USD` = **Bitstamp** 主 + **Kraken** 核验（Kraken 公开 OHLC 仅返回最近 720 根；Coinbase 无 `USDC-USD` 产品） | 东方财富外汇 JSON 接口 / **FRED** 公开接口 | 严格区分 `USD`、`USDT`、`USDC`（FR-LED-02 / AC-06）；非 ECB 发布日按 `carried_forward` 规则（A-07）；同版本确定性逆汇率与缺失标记 |
| 交易日历与时段切分 | **Rust `chrono 0.4.45` + `chrono-tz 0.10.4`**（`America/New_York`）编码 NYSE/NASDAQ 法定节假日与 13:00 ET 半日市规则 + 仓库内置版本化特殊闭市表 JSON | 数据供应商官方日历接口核验 | 精确识别 `XNYS`/`XNAS` 节假日、半日市、夏令时及加密 `24×7` UTC 切分；区分“休市”“缺数据”“日历未覆盖” |
| 券商/交易所流水与账单导入 | **富途牛牛（Futu）成交与资金流水 CSV**（用户第一证券主力）+ **欧易（OKX）现货成交与资金账单 CSV**（用户第一加密主力）+ **通用 CSV 映射模板**（`csv 1.4.0`）+ **IBKR Flex Query XML 解析器** + **币安（Binance）现货成交与充提 CSV** | **老虎证券（Tiger）CSV**、**长桥证券（Longbridge）CSV**、**嘉信证券（Schwab）CSV**、**东方财富交割单 CSV**、**IBKR Activity CSV**（扩展预设）；Longport 只读账户 API（后续） | 文件哈希 + 行号/来源业务主键幂等去重；跨文件疑似重复进入待核对，不自动吞单 |

所有 Provider 在 `capabilities()` 中声明 `requires_key`、`rate_limit` 与数据条款类别（`public_open` / `official_free_personal` / `unofficial_personal`）。代码开源许可不授予行情数据使用权；Yahoo/东财接口为非官方接口，仅供个人本地使用，M1 不做行情再分发。

## 3. 本地数据存储与分析引擎（全部纯 Rust）

| 能力 | M1 首选 | 备选 / 取舍 | DELTA 自研边界 |
| --- | --- | --- | --- |
| 控制面、事务账本、日线行情 | **SQLite + rusqlite 0.41.1**（bundled，WAL，Backup API） | SQLx 0.9 适合多数据库后端，单机无需双驱动；不用 ORM 隐藏金额语义 | 迁移事务、幂等批次、冲销更正、水位线 `sync_checkpoint`、`dataset_version` / `dataset_partition`（M1 仅 `sqlite` 位置）与一致性备份恢复 |
| 大规模批量扫描（M5+） | 推迟：**Parquet 60.1.0 + DuckDB 1.20000.1**（`OSS-10`） | Polars 0.55.2 | 通过 `dataset_partition.location_kind` 抽象接入，不改变上层查询契约；触发条件为 M5 扫描需求或实测性能不足 |
| 金额与十进制精度 | **rust_decimal 1.43.0** | bigdecimal 0.4.10 | FIFO 成本批次、费用分摊、第三资产扣费、跨账户搬移成本与多币种折算；禁止 SQL `REAL` 求和 |
| 技术指标与策略评估 | **Rust 原生确定性实现**（MA/EMA/MACD/RSI，手算夹具；M5 受控类型化条件/策略引擎） | `ta 0.5.0` / `yata 0.7.0` | 固定数据版本起点计算，保证不同观察窗口同日期指标值完全一致；受控策略带步数/超时配额与取消隔离（AC-16） |
| 私有 Token 与模型密钥 | **数据目录内 DPAPI 加密文件**（`windows-sys` `CryptProtectData`，[D-32](../decisions/confirmed-decisions.md)，2026-11-09 起取代 `keyring` 与 Windows 凭据管理器） | 开发/CI 显式受控环境变量引用 | 不进命令行、配置明文与日志；加密不可用时拒绝保存 |

## 4. 桌面、CLI 与 AI 栈比较

| 层级 | 方案 | 复用价值 | 成本与风险 | 结论 |
| --- | --- | --- | --- | --- |
| M1 交互与验证入口 | **Rust CLI (`delta-cli`) + 自动化测试套件** | 零 GUI 阻塞，直接验证数据拉取、落盘、对账、质量报告与恢复 | 无图形界面 | **M1 必选**：所有数据与账本能力先通过 CLI 与集成测试跑通 |
| M2+ 桌面与图表（已确认 D-21） | **长桥最新 `gpui-kit` (`0.8.x` / `0.9.x`) 官方 `DockArea` + `Panel` + `DataTable` + `Card` 面板体系 + K 线视口滑动窗口切片** | 纯 Rust 同进程调用 M1 数据与账本服务；直接复用长桥官方可停靠/分屏/持久化面板与高密度表格 | 需在主 K 线 `Panel` 维护 `visible_range` 缩放/平移窗口切片；`gpui-pre` 精确快照锁定与 Windows 构建风险见 `OSS-09` | **已确认（D-21，关闭 `Q-03`）** |
| AI 模型客户端（M2） | **参考 Codex 官方实现的自研窄 Rust `ModelClient`**（D-08 / D-22） | 基于 `tokio 1.54.1` + `reqwest 0.13.5` 支持 Responses 与 Chat Completions 双协议，SQLite 保存会话与原子压缩检查点 | 需自行维护窄协议层；运行时私有于 `delta-app`，不外泄给 UI | **已确认（D-08 / D-22）**：在 M2 阶段对接 M1 验证完毕的只读数据与账本服务 |

## 5. 官方注册表查询快照（2026-11-07）

完整字段（含调研对照过的生态包）见 [2026-11-07 上游快照](../evidence/2026-11-07-upstream-snapshot.json)。以下为纯 Rust 栈核心 crate：

| 包名 | 生态 | 最新版本 | 发布时间（UTC） | 许可证 |
| --- | --- | --- | --- | --- |
| `rusqlite` | crates.io | `0.41.1` | 2026-10-28 | MIT |
| `duckdb` | crates.io | `1.20000.1` | 2026-10-28 | MIT |
| `parquet` | crates.io | `60.1.0` | 2026-10-28 | Apache-2.0 |
| `rust_decimal` | crates.io | `1.43.0` | 2026-09-02 | MIT |
| `tokio` | crates.io | `1.54.1` | 2026-10-21 | MIT |
| `reqwest` | crates.io | `0.13.5` | 2026-09-08 | MIT OR Apache-2.0 |
| `chrono` / `chrono-tz` | crates.io | `0.4.45` / `0.10.4` | 2026-06-04 / 2025-07-11 | MIT OR Apache-2.0 |
| `csv` | crates.io | `1.4.0` | 2025-10-17 | Unlicense OR MIT |
| `ccxt` | crates.io | `4.5.92` | 2026-11-04 | MIT |
| `longport` | crates.io | `4.3.7` | 2026-08-12 | MIT OR Apache-2.0 |
| `databento` | crates.io | `0.64.0` | 2026-10-27 | Apache-2.0 |
| `windows-sys`（门禁前台无干扰，D-25） | crates.io | `0.61.2` | 2025-10-06 | MIT OR Apache-2.0 |
| `gpui-kit` / `gpui-pre` | crates.io | `0.9.0`（依赖 `gpui-pre =0.3.11`）/ `0.3.11` | 2026-11-02 | Apache-2.0 |

## 8. M1 兼容版本与扩展 POC 落地

产品兼容版本以根 Cargo.toml/Cargo.lock 与 Rust 1.99.1 为准，不把调研时“最新版本”直接视为本地已构建版本。M1 保留 reqwest/serde_json 原生 REST 与 Binance ZIP CHECKSUM 路径，精度入口为 Decimal。

官方 ccxt 的独立 POC 精确锁定 `=4.5.91`（okx/kraken features），实际构建与网络调用见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md)：不是停更问题，退出原因为 typed OHLCV f64 精度边界及默认 Windows 栈溢出。通用无损 adapter、Longport SDK 和额外原生账单模板登记 LOOP-03；不改变 M1 必做原生连接器和 M2 GPUI 方向。

## 9. M2 桌面、模型客户端与打包选型（2026-11-08 A 核实）

阶段定案见 [M2 计划第 3 节](../delivery/stage-plan.md#3-a-已定案的阶段技术决定b-不再重新选型)，本机实测见 [gpui-kit spike](../evidence/2026-11-08-m2-gpui-kit-spike.md)。

| 能力 | 选定 | 官方来源与日期 | 适配与许可边界 |
| --- | --- | --- | --- |
| 桌面框架 | `gpui-kit =0.9.0`（→ `gpui-pre =0.3.11`） | crates.io 2026-11-02；longbridge/gpui-kit | Apache-2.0；A 初始 spike 在 Rust 1.99.1 构建通过、锁文件 848 包；B 最终工作区为 6 个自有＋885 个第三方包，3 个 MPL-2.0 依赖的通知/源码来源处理见 `OSS-11` 和阶段报告 |
| 工作台 | 官方 `DockArea` / `DockLayout` / `DockSkin` / `Panel` / `DataTable` | 同上 | **0.9.0 无官方 `Card` 组件**：D-21 的 `Card` 按官方 story `ChartCard` 模式自建 `MetricCard`，方向不变 |
| K 线 | 自研 `KlineViewport` + 自绘 GPUI 元素；`CandlestickChart` 仅作候选 | 同上 | 官方 `CandlestickChart` 无视口、光标与标记层，数值为 `f64`；Decimal 只在绘制时转换 |
| UI 测试 | `gpui-kit` `test-support` 无头窗口 | 上游 `crates/kit/TESTING.md` | 本机实测 0 可见窗口、焦点不变，满足 D-25 |
| SSE | `eventsource-stream 0.2.3`；备选 `sse-stream 0.3.x` | crates.io（前者 2022-02，Codex 当前同版本；后者 2026-09） | 均为 MIT OR Apache-2.0；B 验证与 `reqwest 0.13` 流的兼容 |
| 安装包 | **Inno Setup 7.1.x**（门禁固定小版本并记录哈希）按用户安装（`PrivilegesRequired=lowest`）；图标与版本资源 `winresource`；版本比较 `semver` 1.x；更新签名 `ed25519-dalek` 2.x。未采用：`cargo-packager`（NSIS，单通道 AppId/互斥量与静默升级控制需另写钩子）、Velopack（安装根固定在自身目录结构，与 D-26 路径冲突）、MSIX（需签名证书） | jrsoftware.org：7.1.1 发布于 2026-10-28，7.x 为推荐线（2026-11-09 核对）；Zed 官方 Windows 打包同样用 Inno Setup 按用户安装、按通道区分 AppId/AppMutex；crates.io `cargo-packager` 0.11.8（2025-11-27） | Inno Setup 自有许可允许免费商用，为构建期工具，不随包分发其源码义务；`winresource` MIT、`semver` MIT OR Apache-2.0、`ed25519-dalek` BSD-3-Clause；运行期仍为单一 Rust 二进制（D-22） |

B 的兼容版本锁定为 `gpui-kit =0.9.0` / `gpui-pre =0.3.11`、`reqwest 0.13.5` / `eventsource-stream 0.2.3`，具体运行证据见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)。M2 时本机缺少 NSIS/WiX 工具，S-23 落地为 `package-build` 便携 ZIP＋Rust 按用户安装器；M3 起改为 Inno Setup 安装程序（见 [M3 计划](../delivery/stage-plan.md) S-25～S-35），便携 ZIP 仅保留为测试与迁移诊断产物；两通道分别编译，build-info、源码指纹和文件哈希可复验。Windows 目标显式设置 `x86_64-pc-windows-msvc` 与 `+crt-static`，同时覆盖 `CARGO_ENCODED_RUSTFLAGS`，避免继承 flags 取消静态运行库；显式 target 让该设置只用于目标代码，不施加给 host proc-macro。构建和安装检查普通/延迟 PE 导入并保存 `binary-imports.json`，拒绝外部 MSVC 运行库依赖。当前机器的包内 CLI 安装/外部卸载与 H-03 干净机器验证分开记录，不能以开发机已有运行库或复制 source 二进制宣称包可交付。

**M3 安装与更新落地（B，2026-11-09）**：Inno Setup 锁定 **7.1.1**，构建机用 `winget install --id JRSoftware.InnoSetup.7 --version 7.1.1 --scope user --silent --disable-interactivity` 安装到 `%LOCALAPPDATA%\Programs\Inno Setup 7`（官方安装包 SHA-256 `8a5a6711c3766a6e5fb74f280ba0e3551e719ce94538d901fdbd1f8f9920b3f0`，`ISCC.exe` SHA-256 `e0f9f1dc32032650c6629f6dee41eb6d20ba140e4e3c07c25d8a24f9ba781644`）。`ISCC.exe` 不带可读的文件版本资源，因此版本由模板内 ISPP `Ver` 检查强制为 7.1.x，安装后的 HKCU 键记录 `Inno Setup: Setup Version`，门禁据此核对；`DELTA_ISCC` 可指定编译器路径，找不到时 `package-build` / `release-build` / `--installer` 明确失败，不回退 ZIP。Inno Setup License 允许含商用的使用与再分发，要求保留其版权与网址（安装器“关于”框原样保留），致谢写入 `THIRD-PARTY-NOTICES.txt`。Rust 侧锁定 `winresource 0.1.34`（MIT，图标与 `VERSIONINFO`）、`semver 1.0.28`（MIT OR Apache-2.0）、`ed25519-dalek 2.2.0`（BSD-3-Clause，依赖 `curve25519-dalek 4.1.3`）与 `getrandom 0.3.4`（MIT OR Apache-2.0，密钥生成），更新下载沿用已锁定的 `reqwest 0.13.5`。
