# M1 v2.6 实现复查与交付报告

更新：2026-11-08；角色：Agent B（第 1～5 节）/ Agent A 集中复核（第 6 节）；唯一计划：[M1 v2.6](stage-plan.md)。分支 `codex/m1-data-foundation`；固定父提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`。交付完整 SHA 在 amend 后的交接消息给出，报告用父提交与源码指纹避免自引用。既有未跟踪 `workers/agent/` 保持原样，未读取、修改或暂存；没有远端动作。

本报告替代此前缺少实际断言支撑的“全部通过”表述。B 自检、A 技术审核和产品真实样本验收分别判断。测试夹具的真实性见 [PROVENANCE](../../tests/fixtures/PROVENANCE.md)，官方 ccxt 的实际观察见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md)。

## 1. 复查问题与修复

| ID | 原问题 | 实际修复与主要位置 |
| --- | --- | --- |
| F-01 | CLI 的自动同步未形成完整真实网络闭环，标的身份可能与库中身份不同 | `delta-app/src/sync.rs` 实现注册表驱动的真实分页同步、持久订阅、实际来源水位线、稳定币主/核验/降级、币安月归档 CHECKSUM、行情与 FX 同事务封存；`registry.rs` 按供应商 wire 格式选择别名，重载后仍发送 `BTCUSDT` |
| F-02 | 取消/超时可能与提交竞态，已提交作业仍报取消，或 worker 丢失锁 | 发布事务与作业状态共用锁；取消先于发布则回滚，发布完成则成功；worker 继承同一 `InstanceLock` lease，公开会话必须持锁 |
| F-03 | 缺 FX 或缺期初现金时可能按 1 入账或生成负现金 | 缺可靠事件 FX 拒绝入账；应用导入可先同步缺失 FX 再做 authoritative commit；现金 FIFO 验证余额及历史成本，支持 USD 中间币换算 |
| F-04 | 行情 FX 修订会改变已提交交易的记账成本 | schema 3 增加 `event_book_fx`，导入、更正与疑似重复确认固定实际采用的事件汇率；新增迁移前一致性备份、未来 schema 拒绝、旧报告失效和旧 source ID 分类迁移 |
| F-05 | 同秒交易、重导和预览篡改可能重复入账；更正丢失历史分录 | 文件哈希和宿主重建预览验证；可靠业务 ID 与内容分类去重，无 ID 使用 batch+行并标疑似重复；保留 journal revision、原分录、冲销与替换，重建派生批次 |
| F-06 | 富途/欧易/币安独立资金表不足，空 ID 和重复列被误处理 | v2 严格资金模板转换到同一 CSV 引擎，覆盖入金/出金/收入/独立费用；币安交易导出允许无 account/tradeid；重复列、未知时区/交易方向/成本标志和负费用 rebate 显式拒绝；Flex 检查 XML 属性、多账户和转账方向 |
| F-07 | BTC 充提可能记成 USDT 现金；未知成本卖出后恢复了虚假的损益数值 | 加密外部入库建未知成本 opening lot，外部转出搬移 lot；资产费用只扣一次 FIFO slice；未知历史成本贯穿后续处置，数值损益保持 unavailable，显式更正 opening 单位成本后重算 |
| F-08 | 外部银行/钱包被无条件视为组合成员；不同到账日的在途金额可能丢失或重复 | 整库 scope 使用实际事件账户与登记的 active 账户；外部 counterparty 不自动入组；内部现金继承 FIFO 成本，持仓继承原日期和成本，只在两端属于范围时计在途资产，小范围转账按边界市值计资金流 |
| F-09 | 估值收益率使用固定结果，外币资金流采用期末 FX，报告身份缺少 cutoff | 实际逐日历史重放生成日频近似 TWR / XIRR / 可比回撤；资金流按发生日折算；报告绑定范围、币种、cutoff、来源与 `m1-ledger-3`，增加范围/币种转换差额；历史输入只加载一次 |
| F-10 | 多来源/多版本公司行动可能重复复权，总回报序列缺少乘法股息因子 | 同逻辑行动选一个版本/来源；从原始 OHLC 派生 split/total-return，非法因子或缺前收盘拒绝；估值使用 raw，指标使用明示复权序列 |
| F-11 | 导出吞错、恢复漏文件、删除只认文件名、开发写入正式根 | 导出 coherent read snapshot 的 8 CSV，staging 与哈希成功才发布；递归备份 raw/imports/attachments/settings；恢复验证库身份/哈希/外键；删除先验证链接与数据库/manifest 身份，持锁执行，递归枚举/计数任何错误拒绝执行；打开、备份、恢复、导出、删除均验证通道边界 |
| F-12 | native 账户录制边界仅有余额，指标缺少独立手算 | `FixtureAccountProvider` 解析 OKX/Binance 提取的原生余额/成交/已完成充提数组，保持 Decimal、账户与时点边界；六类指标手算与未来哨兵测试，XNYS/XNAS 全范围对照 |
| F-13 | 门禁固定写 pass、旧 ccxt 结论错误、无失败日志和源码绑定、命令超时仅杀直接子进程 | `delta-cli/src/verify.rs` 保存命令 stdout/stderr/退出码/是否超时，按实际测试名称映射验收，失败也生成报告；每条命令独立 Job 收口整棵 Windows 进程树后再回收输出管道；保存源码 SHA-256、实际进程优先级、窗口 PID/标题及有限范围目录观测；初始化真实临时库再测非 TTY 删除，核对新增嵌套文件的 1 文件/22 字节；结果统一 passed/failed/blocked/not-run，POC-D06 当前 gate 标 not-run 并引用独立实测 |

这些是 M1 当前必须解决的金融金额、身份、质量、恢复和证据缺陷，未转为后续优化。旧错误结果不是本次交付的验收依据。

## 2. 工作包与可复验入口

| 工作包 | 实现入口 | 实际验证入口 / 边界 |
| --- | --- | --- |
| M1-W01 | `storage.rs`、`paths.rs`、`process.rs`、`service.rs` | 迁移/版本/恢复测试；POC-D01 101 万行；跨进程 keyring、锁竞争；静音崩溃/删除；开发通道保护 |
| M1-W02 | `registry.rs`、`calendar.rs`、FX providers、`valuation.rs` | XNYS/XNAS 1990～2027 对照；法币 carry ≤4 日、逆向/跨币种、稳定币实际价；缺 FX 拒绝 |
| M1-W03 | `providers.rs`、`sync.rs` | 回环多页/429/取消/归档校验；公开 OKX、币安 REST/归档、稳定币在线探针；真实账户 API 未接入 |
| M1-W04 | equity providers、公司行动与调整函数 | AAPL 原始价/分红还原、缺拆股告警、重复行动；NVDA 拆股/SPY 分红公共在线探针；Tiingo/Alpaca 仅离线结构验证 |
| M1-W05 | `importers.rs`、`ledger.rs`、`valuation.rs`、应用报告 | authoritative import、Futu/OKX/Binance trade/funding、Flex、FIFO/更正、现金汇兑、转账范围、未知成本、事件 FX 固定；真实个人样本 not-run |
| M1-W06 | `sync.rs`、`export.rs`、`verify.rs`、指标 | 实际 import→回环 HTTP→封存版本→报告→导出；六类指标手算与 cutoff；一致性备份/恢复、诊断脱敏 |
| M1-EXT | 扩展模板、Alpaca parser、ATR/Bollinger、ccxt POC | 通用规范化扩展模板/Alpaca 结构回放通过；实际平台全部原生导出差异、Alpaca bars、Longport SDK、通用 ccxt adapter 见 LOOP-03 |

`M1-A-02`～`M1-A-12`、`M1-A-16` 和 `M1-A-17` 由 `verify.rs` 捕获的命名测试映射生成；`M1-A-01` 实跑百万行 POC，`M1-A-13`～`M1-A-15` 实跑跨进程验证。`M1-A-18` / POC-D06 有单独实测文档，本次 gate 不重跑则标 not-run。每个实际测试结果和对应 gate 日志都保存在 JSON。

## 3. 实际验证

工具链：Rust / Cargo 1.99.1，Windows MSVC；产品运行期只有 Rust。Python 只运行开发期文档门禁。

> 本节是 B 交付输入（指纹 `2820f9ba…`）的证据，A 已独立复算该指纹一致；A 修正后源码指纹改变，当前有效 gate 证据见 [第 6 节](#6-agent-a-集中复核)。

2026-11-08 B 交付版本的静音完整 `verify-stage` 与公共 `--live` 均 **passed**，两次执行前后源码指纹一致：

```text
2820f9bae2a78649df453e892f80c80df83b06212243892ee1a360d1828b3b17
```

| 验证 | 实际结果 | 证据 |
| --- | --- | --- |
| docs / diff / fmt / Clippy | 两轮均 passed；Clippy 为 workspace/all-targets `-D warnings` | 各轮 `M1-G-01`～`M1-G-04` 原始命令日志 |
| workspace tests | 每轮 **47 passed / 1 ignored**：app 38、core 4、data 5；忽略项是独立 online 测试 | `M1-G-05` stdout/stderr/退出码与 48 个命名结果 |
| M1 必做用例 | `M1-A-01`～`M1-A-16` 全部 passed；M1-A-17 的规范化扩展回放 passed | 实际百万行 POC、命名测试映射、跨进程断言 |
| Windows 隔离 | 实际 `below_normal` / jobs=4；可见窗口 **0**，临时目录残留 **0**，观测范围外新目录 **0**，正式数据根目录元数据未变化 | 进程树超时后父/子锁均可重获；keyring、锁竞争、静音 crash 与非 TTY 删除证据 |
| 公共网络 | **12 passed / HTTP 200**；私有服务 **3 not-run** | 独立 `live_probes`，见下表 |
| POC-D06 / M1-A-18 | 当前 gate **not-run**；单独实测的成功/栈失败分别记录 | [官方 ccxt POC](../evidence/2026-11-08-ccxt-rust-poc.md) |
| README 示例 | 临时隔离根中 init→Futu 导入→fixture sync→explain，4 条命令 exit 0，提交 3 个合成事件，临时根清理；报告如实 partial | `.local/reports/m1/readme-example-report.json` |

最终离线报告开始时间 `2026-11-08 00:54:10 +08:00`，日志目录 `.local/reports/m1/logs-26028-1794070450596/`；最终在线报告开始时间 `2026-11-08 00:54:44 +08:00`，日志目录 `.local/reports/m1/logs-19868-1794070484904/`。此前不同源码指纹的结果不作为本次交付证据。运行结果状态均校验为 passed/failed/blocked/not-run。

| POC-D01（实际 1,010,000 行） | 最终离线 | 最终在线 |
| --- | ---: | ---: |
| 批量写入 | 9,245 ms | 7,537 ms |
| 5,000 bar 版本区间查询（目标 <50 ms） | **14 ms** | **12 ms** |
| 一致性备份 + 恢复及校验 | 12,303 ms | 12,381 ms |

POC 还断言旧/新版本可见区间和 cutoff 查询；这些是本机两次测量，未扩展为所有硬件的性能保证。

| 公共 probe | 实际 HTTP / 状态 | 返回与关键断言 |
| --- | --- | --- |
| OKX | 200 / passed | 99 根已完成 `1Dutc` 日线 |
| Binance Vision | 200 / passed | 99 根已完成 UTC 日线 |
| Binance 月归档 | 200 / passed | 2025-03 ZIP 与 SHA-256 CHECKSUM 校验；31 根，微秒时间戳 |
| Coinbase USDT/USD | 200 / passed | 349 个日度汇率值 |
| Bitstamp USDC/USD | 200 / passed | 4 个日度汇率值 |
| Bitstamp USDT/USD | 200 / passed | 4 个独立核验汇率值 |
| Kraken USDC/USD | 200 / passed | 720 个日度汇率值，保留公开接口回溯上限边界 |
| Frankfurter | 200 / passed | 9 个法币汇率值，USD→HKD/CNY/EUR |
| Yahoo AAPL | 200 / passed | 4 根 / 1 个行动，2020-08-28 raw close 还原为 **499.23** |
| 东方财富 AAPL | 200 / passed | 4 根 raw 日线，同日 close **499.23** |
| Yahoo NVDA | 200 / passed | 4 根 / 2 个行动，2024-06-10 **10:1** 拆股断言通过 |
| Yahoo SPY | 200 / passed | 6 根 / 1 个行动，2025-03-21 现金分红断言通过 |

这些是实际公共服务请求，不是回环/fixture 结果；Tiingo、Alpaca、Longport 在线请求及真实个人账单均未执行，未读取私人样本或私有凭据。

可运行命令：

```powershell
$env:DELTA_GATE_JOBS='4'
$env:CARGO_BUILD_JOBS='4'
$env:CARGO_INCREMENTAL='0'
cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6
cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6 --live
```

入口设置静音子进程、低优先级、Job 收口和受限并发，内部实际运行 docs/diff/fmt/clippy/tests。报告位于 `.local/reports/m1/verify-stage[-live]-report.json`，日志位于同目录 `logs-<run-id>/`。源码指纹包含工作区 Rust/Cargo、锁文件、工具链、金融夹具和文档检查器，不包含 `workers/` 或本地 POC/个人数据；执行结束验证源码未变。

重点手算证据：AC-02 应用闭环现金 **1438**、剩余数量 **6**、成本 **600.6**、已实现 **38.6**、未实现 **29.4**、NAV **2068**、总损益 **68**，日频近似 TWR **0.034**。外币现金在途例保留成本 **99.8**、费用汇兑收益 **0.04**、NAV **119.76** / 损益 **19.76**；独立资金表例现金 **95**、净投入 **90**、损益 **5**；EUR 记账的 100 HKD 在显式 USD/HKD=10 与 USD/EUR=0.8 的合成报价下固定为 **8 EUR**。

目录观测仅检查真实 `LOCALAPPDATA/delta` / `delta-dev` 的目录元数据及本次临时根，窗口哨兵覆盖本次 Job 内的可见顶层窗口；这不等于全系统写入追踪或桌面上所有进程的窗口证明。

## 4. 依赖与数据条款

项目 Apache-2.0；实际 `cargo metadata --locked --format-version 1` 盘点 **221 个第三方包 + 4 个工作区包**，每个第三方包均有许可表达式，选择宽松许可分支并保留通知义务，详见 [逐包清单与边界](../evidence/2026-11-08-dependency-licenses.md)。其他目标平台的 3 个发布包未附顶层许可原文，不能把表达式盘点称为全平台分发通知已完备；本阶段未生成安装包。数据类别只采用 `public_open` / `official_free_personal` / `unofficial_personal`；封存版本记录实际来源、条款、能力与端点，导出携带该类别。代码许可不授予行情再分发权。

官方 `ccxt =4.5.91` 独立构建成功，默认 Windows 栈溢出；64 MiB worker + boxed future 后 OKX/Kraken 真实请求成功。其 typed OHLCV 为 f64，因此没有作为 Decimal 入口；扩展 adapter 延期归属 LOOP-03，不能声称官方 ccxt 停更。

## 5. 限制、当前判断与交接

M1 提供已声明规范化模板和合成/公开结构回放；Futu 默认纽约时区、OKX/Binance UTC，费用模板采用非负支出，rebate 需显式收入。未知列/格式拒绝并给诊断。扩展平台当前共享规范化模板，不能宣称兼容所有原生账单。`ACT-03` 真实去标识样本及 Tiingo/Alpaca/Longport 私有服务未测，不访问私人样本或凭据来替代授权；后续 native 模板/通用 adapter/SDK 的范围、归属和触发只维护在 [LOOP-03](../decisions/open-questions.md)。

报告目前为整库 inception→as-of 视图，收益率明确为日频近似，缺行情/汇率/未知成本使相关损益/收益率 unavailable；没有 M2 桌面 UI、AI 网关、训练或策略执行。未知 opening 成本只能由明确更正填入，不能用当前市场价猜历史成本。

- B 实现与自检：**passed，可交 Agent A 集中复核**；M1 必做没有已知工程阻断，扩展未完成部分按计划第 3.1 / 6 节登记 LOOP-03。
- A 技术审核：见第 6 节（2026-11-08 已执行，含 A 局部修正）。
- 产品验收：M1 自动证据与公共服务结果单列；ACT-03 真实样本/私有服务 not-run；M2 完整 FR/AC 未验收。
- 首选下一步：Agent A 集中复核唯一 M1 v2.6 计划、本报告和本次本地提交，复用源码指纹一致的实际证据；未自动启动会话，也未执行远端动作。

可复制 B→A prompt（交付时附完整 SHA）：

```text
请作为 Agent A 集中复核 M1 v2.6。唯一计划 docs/delivery/stage-plan.md，结果与 F-01～F-13 见 docs/delivery/stage-report.md。分支 codex/m1-data-foundation，固定父提交 8329f638536e4f303764045a2f10a2c1ab31fcd3；交付完整 SHA 见本次交接消息，源码指纹与实际 gate 日志见报告。
先核对 git status、父提交与指纹，保留未跟踪 workers/agent/。重点复核事件 FX 固定、authoritative 导入/去重/冲销、第三资产费用、未知成本、转账在途与 scope、收益率的日频近似、版本封存、恢复/删除/通道隔离以及门禁是否由实际命名测试支撑。
复用指纹一致的证据，局部确定问题直接修复并验证；大缺口整批返工。分列 B 自检、A 技术审核与产品验收。必做收敛后转用户具体远端授权；ACT-03/LOOP-03 非阻塞部分独立推进。不 push、不操作 PR、不自动创建其他 Agent 会话。
```

## 6. Agent A 集中复核

2026-11-08，Agent A。输入：分支 `codex/m1-data-foundation`，B 交付提交（指纹 `2820f9ba…`），父提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3`。工作区仅有未跟踪 `workers/agent/`，保持未读取、未修改、未暂存。A 用独立脚本按 `verify.rs` 规则复算源码指纹为 `2820f9ba…b3b17`，与第 3 节一致；B 的两份 gate JSON 绑定同一指纹、`overall_status=passed`，命名测试映射要求全部列名测试 passed，失败测试会使 G-05 与映射失败，未发现固定写 pass。

### 6.1 复核范围与结论

逐项阅读 F-01～F-13 对应实现：账本重放与现金 FIFO 二次处理、第三资产费用与 FeeOnly、未知成本传递、现金/持仓转账在途与 scope、估值与日频近似 TWR / XIRR、`event_book_fx` 的导入/更正/复制、authoritative 预览重建与文件哈希、更正冲销分录、版本发布哈希与 cutoff、备份恢复校验、删除前身份/链接/锁、通道保护和 gate 映射。F-01～F-13 的修复方向成立；另发现下列确定问题，均为局部修正，A 直接修复并补测试，无需整批返工。

| ID | 问题与失败场景 | A 修正 | 回归测试 |
| --- | --- | --- | --- |
| A-01 | 期初现金只按报价币事件 FX 入账：通用 CSV `opening_balance`，`symbol=HKD`、`quote_currency=USD` 时 1000 HKD 记为 1000 USD 投入与成本（实测复现）；现金判定只认 USD/HKD/CNY/EUR 四个币种，JPY 等会走持仓分支 | 现金判定改用 `is_fiat_currency`；期初现金币种与报价币不一致时拒绝 | `regression::opening_cash_requires_its_own_currency_and_known_openings_need_unit_cost` |
| A-02 | 标记成本已知但无单位成本的期初持仓，仅在已登记标的时拒绝；未登记标的会以 0 成本入账 | 非现金期初持仓 `cost_known && price=None` 一律拒绝 | 同上 |
| A-03 | 买入报价腿双重扣减：同账户既有 USDT 现金又有 USDT 买入批次时，花 100 USDT 买 BTC 会同时扣现金与批次，剩余 USDT 由 100 变 0（实测复现） | 报价腿只经现金 FIFO 结算并校验余额；现金不足时拒绝，不再消耗批次 | `regression::stablecoin_quote_leg_is_settled_once_from_cash` |
| A-04 | 通道保护只覆盖 `%LOCALAPPDATA%\delta`，源码/开发构建仍可把导出、备份、恢复目标指向正式版用户文件根 `<文档>\delta`（D-26） | 守卫同时覆盖两个正式根；文档根取系统登记位置，不受测试覆盖变量影响 | `regression::source_channel_cannot_target_release_user_file_root` |
| A-06 | gate 源码指纹按工作区原始字节计算；Git 在 `eol=lf` 下把 CRLF 工作副本视为未修改，导致指纹与实际提交内容不一致（B 交付 `2820f9ba…` 的提交树实为 `bc97dd90…`），证据绑定不精确 | 指纹对文本输入按 LF 规范化，与提交内容一致；A 独立脚本验证 CRLF/LF 结果相同 | 两轮 gate 实跑（下表） |
| A-05 | 报表估值读取 `fx_rate` 只按版本号过滤，未像记账 FX 一样要求已发布（现有同步均在同一 savepoint 内发布，属纵深防护） | 报表 FX 查询联结 `dataset_publication` | 由既有版本/FX 修订测试覆盖 |

新测试已加入 `M1-A-06` / `M1-A-16` 的命名映射；行为变化同步到 [金融规则](../design/financial-engine.md)。买入稳定币/法币作为批次后无法再作报价腿结算，现为显式拒绝，建模为现金换汇的后续项登记为 `LEDGER-01`（[集中台账](../decisions/open-questions.md)）。

### 6.2 A 修正后的实际验证

A 修正后源码指纹（LF 规范化；A 独立脚本、gate 与提交树三者一致）：

```text
ae2f8960e406fd44c43e9ef81bc83e13084ac503dbbe65bebcdf24a8fe7a447b
```

| 验证 | 结果 | 证据 |
| --- | --- | --- |
| 离线 `verify-stage`（开始 `2026-11-08 05:17:18 +08:00`） | **passed**；G-01～G-05 passed；workspace **50 passed / 1 ignored**（app 41、core 4、data 5；忽略项为独立 online 测试）；`M1-A-01`～`M1-A-17` passed；`M1-A-18` not-run；`below_normal` / jobs=18（默认 CPU−2）；可见窗口 0、临时残留 0、范围外新目录 0、正式数据根未变化 | `.local/reports/m1/verify-stage-report.json`，日志 `logs-21320-1794086238325/` |
| 公共 `--live`（开始 `2026-11-08 05:17:51 +08:00`） | **passed**；12 个公共 probe 均 HTTP 200 且关键断言通过（与第 3 节同一组断言）；Tiingo / Alpaca / Longport **not-run** | `.local/reports/m1/verify-stage-live-report.json`，日志 `logs-20584-1794086271772/` |
| POC-D01（1,010,000 行） | 离线：写入 4,105 ms，5,000 bar 版本查询 **6 ms**，备份+恢复 9,660 ms；在线轮：4,140 / **7** / 9,707 ms | 同上报告 `poc_results` |
| 文档门禁 | 报告/状态/台账回写后另行运行 `python scripts/check_docs.py` 与 `git diff --check` 均 exit 0 | 本节回写后的命令结果 |

gate 内 G-01 运行于文档回写前；文档回写不改变源码指纹，回写后已单独重跑文档门禁。

### 6.3 判断与接续

- **B 自检**：passed（第 1～5 节），A 未以其替代审核。
- **A 技术审核**：**passed with A fixes**。A-01～A-06 已修复并通过上述实跑；无剩余 M1 必做工程阻断。非阻塞项：`LEDGER-01`、`LOOP-03`。
- **产品验收**：未完成。自动证据与公共服务证据如上；`ACT-03` 真实去标识样本与 Tiingo / Alpaca / Longport 私有服务 not-run；M2 FR/AC 未验收。人工/私有项可独立推进，不阻塞远端授权请求。
- 修正按 D-09 amend 进同一阶段提交（带 `Signed-off-by`，D-27），实跑在提交内容上执行。完整 SHA 见交接消息与 `git rev-parse HEAD`。
- **首选下一步**：用户按 [USER-ACTIONS](../../USER-ACTIONS.md) `ACT-08` 授权具体远端动作。未执行 push / PR，未创建其他 Agent 会话。
