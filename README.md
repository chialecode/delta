# DELTA

**以本地自动化金融数据底座与个人资产账本为基础、以投资决策复盘为核心的桌面工作台。**

DELTA = **Delta + Evaluation + Ledger + Training + Analyzer**。

面向美股、ETF 和加密现货，将多源行情与汇率同步、资产账本、交易笔记、历史训练与策略评估连接到同一套可追溯的数据快照上。按 [D-22](docs/decisions/confirmed-decisions.md)，产品代码采用 Rust，运行期不需要 Python；桌面工作台与证据化 AI 属于 M2，训练与策略评估属于后续阶段。

## 当前阶段（2026-11-08 M2 Agent B 自检完成、待 A 审核）

项目已按 [D-20](docs/decisions/confirmed-decisions.md) 从文档治理基线重建，放弃此前“先构建 GPUI 桌面应用”的代码与产物，确立 **“优先解决金融数据来源与本地自动化数据底座（M1），再构建上层应用（M2+）”** 的演进路线。详见 [当前状态](docs/delivery/status.md)、[M2 阶段计划](docs/delivery/stage-plan.md) 与 [M2 阶段报告](docs/delivery/stage-report.md)。

- **M1（已完成，PR #1 合并）**：已建立 `delta-core`、`delta-data`、`delta-app`、`delta-cli`。提供欧易 OKX / 币安公共现货日线与归档、Yahoo / 东方财富美股日线核验、Frankfurter 法币及 Coinbase / Bitstamp / Kraken 稳定币汇率、`XNYS` / `XNAS` / `CRYPTO_24_7` 日历；账本支持富途、OKX、币安、IBKR Flex 与通用模板、资金流水、FIFO、事件 FX 固定、不可变数据版本、报告、导出和备份恢复。实际验证与未完成边界见 [M1 阶段报告](docs/evidence/2026-11-08-m1-stage-report.md)。
- **M2（B 实现与自检完成 / ready-for-review，A 审核待执行）**：按 [D-21](docs/decisions/confirmed-decisions.md) 与 [ADR-0003](docs/decisions/0003-first-round-stack.md)，在 M1 数据底座之上新增 `delta-model` 与 `delta-desktop`（二进制 `delta`），采用最新 **GPUI Kit（`=0.9.0`）官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` 面板体系**及自建 `MetricCard`、`KlineViewport` / `KlineChartView`、`StatusBanner` 构建六页工作台，包含实际输入/后台操作、范围与期间隔离、`schema 4` 笔记/附件/FTS5 搜索、期间损益分解与现金换汇（`m2-ledger-1`）、双协议只读 AI 复盘和真实两通道静态 CRT 包。M2 全门禁与 M1 回归通过；实际证据和未测范围见阶段报告，产品验收未完成。

## 本地运行

Windows 首发，工具链锁定 Rust `1.99.1`，需要 MSVC Build Tools。源码运行使用 `delta-dev` 通道；下面把演示数据与文档输出全部放在忽略目录 `.local/`，使用的是公开合成样本。

```powershell
$env:CARGO_BUILD_JOBS='4'
$env:CARGO_INCREMENTAL='0'
$env:DELTA_DATA_DIR=Join-Path (Get-Location) '.local/demo-data'
$env:DELTA_DOCUMENTS_DIR=Join-Path (Get-Location) '.local/demo-documents'
cargo run -p delta-cli -- init
cargo run -p delta-cli -- import --account DEMO --preset futu --file tests/fixtures/statements/futu_en_sample.csv --commit
cargo run -p delta-cli -- sync --fixtures tests/fixtures
cargo run -p delta-cli -- explain --as-of 2020-09-02
```

`import` 默认只预览，`--commit` 才入账；多币种交易提交前必须已有可靠事件汇率和足够现金。`sync --fixtures` 只做离线回放。真实公共同步可用 `register --venue NASDAQ --symbol AAPL` 注册标的，再用 `sync --symbol NASDAQ:AAPL --from 2020-08-01 --until 2020-09-03`；`--until` 为排他日期，网络失败会给结构化错误与质量记录。完整命令通过 `cargo run -p delta-cli -- --help` 查看。

`explain` 当前提供整库 inception→as-of 视图，TWR 使用日频近似；未知历史成本、缺行情或 FX 使相关损益/收益率 unavailable。平台预设是已声明的规范化模板，尚未验证全部真实原生导出格式；私有账户 API、Tiingo/Alpaca/Longport 的真实服务结果见 `ACT-03` 与 `LOOP-03`。样本范围见 [夹具来源](tests/fixtures/PROVENANCE.md)。

使用上述隔离数据目录时，可执行 `cargo run -p delta-desktop --bin delta` 打开桌面；空资料库也可从界面载入离线示例。模型配置在设置页完成；未配置时总览、资产、图表和笔记照常可用。模型凭据进入 keyring，设置重启可恢复公开配置，留空密钥保留已有认证。

Windows 包通过 `cargo run -p delta-cli -- package-build` 生成，索引为 `.local/reports/m2/packages/package-artifacts.json`。包静态链接 CRT 并检查 DLL 导入，安装与卸载不需要开发环境或另装 VC++ 可再发行组件；解压后分别用包内 CLI 执行 `package-install --channel release|dev --package-dir .`。关闭应用后，用解压目录中的外部 CLI 执行 `package-uninstall --channel release|dev`；卸载保留资料库和未知文件，安装目录内的运行中 CLI 会在删除前拒绝自卸载。真实干净机器步骤在 [USER-ACTIONS](USER-ACTIONS.md) H-03。

## 文档导航

从 [文档导航](docs/README.md) 开始阅读。开发者和 AI 先读 [AGENTS](AGENTS.md)、[项目关键事实](docs/project-baseline.md)、[当前状态](docs/delivery/status.md) 与 [用户待办](USER-ACTIONS.md)。

- [2026-11-07 GPUI 复盘与金融数据底座调研](docs/evidence/2026-11-07-stack-and-data-research.md)：为什么先做 GPUI 不合理、美股/加密接口与开源项目对比、数据自动化集成方案。
- [开源优先的技术选型](docs/engineering/technology-selection.md)：数据源、存储（M1 SQLite 单库）、桌面栈与 AI 比较。
- [系统架构](docs/design/system-architecture.md)：内置自动化数据平面（自动订阅、水位线增量同步、多源交叉核验）与模块边界。
- [数据模型](docs/design/data-model.md) 与 [金融计算规则](docs/design/financial-engine.md)：账本、FIFO 成本、拆股分红、汇率与历史可见性。
- [产品需求](docs/requirements/product-requirements.md) 与 [实施路线与验收](docs/engineering/delivery-plan.md)：61 项 FR/NFR 与 29 项 AC 验收标准。

## 协作与门禁

采用 Agent A 计划与集中审核、Agent B 连续实现与自检；需要用户决定的事项统一在 [USER-ACTIONS.md](USER-ACTIONS.md) 维护，工程任务在 [集中事项台账](docs/decisions/open-questions.md) 维护。

```powershell
$env:DELTA_GATE_JOBS='4'
cargo run -p delta-cli --locked -- verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench
cargo run -p delta-cli --locked -- verify-stage --stage M1 --plan-version 2.6
```

门禁实际执行文档、diff、格式、Clippy、工作区测试、依赖许可、打包与隔离检查，M1 回归还执行百万行 SQLite POC；日志和源码指纹分别保存在 `.local/reports/m2/`、`m1/`。M2 的两个 UI 开关运行隐藏原生窗口、首屏/导航与 100k 图表/表格基准；缺开关时相应结果与 overall 保持 not-run。可显式追加 `--live` 单列公共端点在线探针；本轮未运行，不使用私人样本或私有凭据。静音与观测范围见 [质量门禁](docs/dev-rules/quality-gates.md)。

## 许可

项目自有源码和文档采用 [Apache-2.0](LICENSE)；第三方依赖保留各自的许可和通知，锁文件盘点见 [依赖许可证据](docs/evidence/2026-11-08-dependency-licenses.md)。代码许可不授予任何行情数据的使用权，数据来源条款见 [技术选型](docs/engineering/technology-selection.md)。
