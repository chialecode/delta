# M5 v1.0 阶段验证与交付报告

更新：2026-11-15；角色：Agent B（实现与自检）、Agent A（集中技术审核）。唯一计划：[M5 计划](stage-plan.md) v1.0，用例见 [m5-stage-cases.json](m5-stage-cases.json)；用户口径见 [D-33、D-34、D-35](../decisions/confirmed-decisions.md)。分支 `codex/m5-training-workbench`；固定阶段父提交 `a44f86f4a168e7dd9c51d186cd9668a51d1f0ccd`；B 交付时整套门禁的被测源码指纹 `c97131fc40a746b5d0a475b7e0b79a9fb938d73b7c0e2831fe0a5c4793dcafe6`，**A 审核修正后的最终源码指纹 `9d51a55f4950522f5bfdbf25cba7629e94d3e899e210a14257036f976ec7f3e2`**（差异与验证见 [A 集中技术审核](#a-review)）；完整阶段提交 SHA 以 `git rev-parse HEAD` 为准（B 交付时的 SHA 写在交接说明中）。未跟踪的 `workers/` 未读取、构建、修改或暂存；未 push、未操作 PR、未发布版本。

## 当前判断

**B 实现与自检：完成；A 集中技术审核：通过（2026-11-15，修正 1 项阻断，工程阻断为零）；产品验收：未开始（`H-06` not-run）。** 在上述指纹下，`verify-stage --stage M5 --plan-version 1.0 --ui-smoke --ui-bench --live-data` overall **passed**：`M5-G-01～07`、`M5-A-01～17`、`M5-L-01` 与 `M5-K01～K03` 全部 passed，`M5-M-01`（人工 H-06）not-run。同一指纹下 M1 v2.6、M2 v1.0（`--ui-smoke --ui-bench`）、M3 v1.0（`--installer --ui-smoke`）、M4 v1.0（`--ui-smoke --ui-bench`）回归全部 passed。门禁全程 `below_normal`、0 个可见窗口、前台焦点不变（D-25），开发者本机 `delta` / `delta-dev` 数据与文档目录未被触碰（D-26）。

**真实公开源与录制夹具分开声明**：`M5-K01` / `M5-L-01` 是对币安、欧易真实公开服务的实跑（临时资料库，release 构建）；其余数据类用例使用 `crates/delta-app/src/market_fixture.rs` 的**完全合成**回环行情夹具（币安/欧易报文结构），不证明真实价格或服务可用性（见 [夹具来源](../../tests/fixtures/PROVENANCE.md#5-m5-追加夹具2026-11-15agent-b)）。

## Spike 结论（计划第 5 节）

| Spike | 实际验证 | 结论 |
| --- | --- | --- |
| M5-K01 比特币全历史准备（真实公开源） | 空临时资料库，release 构建，默认请求（`BINANCE:BTC/USDT` 主、`OKX:BTC/USDT` 核验，`1d`/`4h`/`1h`）。首次准备 **857.1 s**（≤ 900 s）；随即增量 **0.5 ms / +0 根**（数据未落后一个周期，作业不发网络请求；真实的增量网络路径由夹具用例 `m5_prepare_failures_retry_and_incremental_update` 覆盖）；`1h` 候选池构建 **0.674 s**（≤ 2 s，70,199 个候选）；库增长 196.4 MB；失败批次 0；三个级别都能开局 | **通过**，但首次准备余量很小，登记 [PERF-01](../decisions/open-questions.md)。各批明细见下表。**本阶段内第一次实跑没有通过**：`1h` 候选池构建 152.5 s，同时发现币安 `1h` 归档里有偏离整点的 K 线。两项都已修复后重跑，见[实跑中发现并修复的问题](#live-findings) |
| M5-K02 连续播放延迟 | release `delta.exe --m5-ui-bench`，隐藏非激活窗口，`1h` 局 1,000 根可见＋MA5/10/20/60、MACD、RSI＋20 条画线＋5 条挂单线＋成本线与成交标记，20 步预热＋300 步、每 150 ms 一根 | **通过**：每步 p50/p95/p99 = 39.5/**55.7**/59.8 ms（≤ 100），实际间隔 p50/p95/p99 = 156.2/**161.5**/164.8 ms（≤ 200）；分阶段 p95：推进 23.0、快照 19.8、练习账户 8.9、绘制 5.5、画线 0.2 ms。门禁按原始样本独立重算。开发中第一次测得 p95 148 ms，原因是训练端口每步查询按 `strftime` 全表扫描；增加索引 `idx_bar_training_port` 与等价的 `open_at` 区间预过滤后（结果行集不变）达标，未启用失败分支 |
| M5-K03 撤销与旧版本重放 | 门禁进程跨进程驱动 `delta-cli training`：夹具准备数据→练习账户开局→快捷成交、限价、改单、快捷卖出后撤销、推进 25 根、推进后撤销被拒、再快捷成交、推进 3 根；CLI 从头/检查点重放（两个独立进程）与宿主从头/检查点重放 | **通过**：四路 `state_hash` 一致（`7835f308…`，34 步，检查点 seq 25），撤销后现金/持仓/成交逐字段复原。用真实 M4 代码生成的 schema 5 资料库（`tests/fixtures/libraries/m4-schema5`）验证：旧 `m4-sim-1` 运行在新代码下重放哈希与 M4 记录一致。开发中发现并修复一个确定性缺陷：撤销后订单号被重新分配，导致重放不一致；`SimReplayer` 改为保存命令前状态，编号计数只向前推进 |

`M5-K01` 各批明细（真实公开源，2026-11-15 08:30Z 前后）：

| 批次 | 根数 | 最早开盘 | 归档月 / REST 页 | 日内缺口 | 质量问题 | 跨源比较 / 偏离 > 50 bps / 最大偏离 | 耗时 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 币安 `1d` | 3,376 | 2017-08-18 | 111 / 1 | 0 | 0 | — | 125.1 s |
| 欧易 `1d` | 3,230 | 2018-01-11 | 0 / 34 | 0 | 22 | 3,230 / 22 / 588.78 bps | 13.5 s |
| 币安 `4h` | 20,246 | 2017-08-17 04:00 | 111 / 1 | 9 | 0 | — | 144.6 s |
| 欧易 `4h` | 19,380 | 2018-01-11 08:00 | 0 / 195 | 0 | 61 | 19,364 / 149 / 472.57 bps | 84.2 s |
| 币安 `1h` | 80,882 | 2017-08-17 04:00 | 111 / 1 | 28 | 0 | — | 160.3 s |
| 欧易 `1h` | 77,517 | 2018-01-11 11:00 | 0 / 777 | 0 | 74 | 77,355 / 548 / 472.57 bps | 329.4 s |

跨源偏离集中在 2018 年前后，只作为欧易（核验源）的价格质量问题记录，含这些日期的窗口不进入欧易候选池；币安（默认训练标的）价格不被改写。币安的日内缺口是交易所停机造成的真实缺口；任何训练窗口都不跨越缺口（连续性检查）。

<a id="live-findings"></a>

### 实跑中发现并修复的问题

1. **币安 `1h` 归档含偏离整点的 K 线（真实数据缺陷）**：开发中第一次真实准备时，币安 `1h` 批整体失败。原因是 `BTCUSDT-1h-2018-02` 中，交易所 2018-02-08 停机恢复后发布的 43 根小时线偏移了 28 分 14.789 秒（2018-02-09 09:28 至 02-11 03:28）。修复：三个解析器（币安归档、币安 REST、欧易）跳过无法放到 UTC 网格上的 K 线，不平移、不补造数据；留下的空洞计为日内缺口，任何窗口都不会跨越它。回归测试 `providers::archive_member_tests::off_grid_archive_klines_are_skipped_not_fatal` 使用该月真实行的形状。修复后币安 `1h` 发布 80,882 根。
2. **`1h` 候选池构建 152.5 s（性能缺陷）**：第一次完整门禁（指纹 `44b2a954…`，报告保留为 `.local/reports/m5/verify-stage-report-run1-44b2a954.json`）中，`M5-L-01` 因此判为 failed。原因是候选池对每个起点重扫整个窗口，并对每对相邻 K 线做 24 次 Decimal 除法的“未解释拆股跳变”检查。修复：每根 K 线与每对相邻 K 线的检查只算一次，窗口用前缀计数判断，接受的窗口与逐窗扫描完全相同（M4 候选池哈希与旧运行重放不变）。新增 `m4_schema::tests::test_m5_k01_large_1h_pool_excludes_gap_and_jump_windows_quickly`（30,000 根，含缺口与跳变，精确核对候选数）。修复后真实数据上 0.674 s。在此之前，用户每开一局 `1h` 或查看数据状态都要等约 2.5 分钟。
3. **桌面要等全部批次才能开局**：准备作业运行时，所有“新一局”按钮都被禁用（约 15 分钟）。修复：作业阶段变化时（即某一批已原子发布）重新读取就绪状态，已就绪级别的按钮立即可用（日线约 2 分钟），状态行显示“已可开局：1d…”；测试 `prepared_timeframes_start_before_the_whole_job_finishes`。

<a id="a-review"></a>

## A 集中技术审核（2026-11-15）

A 对照计划 v1.0、阶段 diff（父提交 `a44f86f` → B 交付提交 `708ee5c`）与 `.local/reports/m5/` 证据复核。B 交付证据先核对：门禁报告 overall passed、`release_data_dir_touched = false`、0 个可见窗口、`below_normal`；A 用独立脚本复算源码指纹，与报告中的 `c97131fc…` 一致。

| 复核项 | 结论 |
| --- | --- |
| 截断在宿主层 | 通过：高一级别视图只由 `query_run_visible_final_bars` 的截断结果聚合，隐藏模式不返回 `open_at`；日内步进只查 `(visible_until, train_end_at]` 内的下一根 |
| Decimal 可手算 | 通过：快捷成交夹具独立手算（100＋0.20 半点差＋0.05 滑点＝100.25，费用 4.01，现金 5,985.99）；不利取整与“不缩量拒绝”有反例 |
| 撤销不跨推进 | 通过：`SimReplayer` 存命令前状态，推进/结束清空撤销栈，编号计数只前进；`m5-sim-2` 恢复从边界检查点（seq 0、推进或结束）开始，撤销在三路重放中一致 |
| `m4-sim-1` 旧局哈希不变 | 通过：A 直接打开 `tests/fixtures/libraries/m4-schema5`：`schema_version = 5`、`training_session` 无 M5 列、三局 `m4-sim-1`，持久化哈希与 `expected.json` 一致；测试在新代码下两种重放均匹配，M4 候选池哈希头保持原字节 |
| 练习账户守恒、放弃也结算 | 通过：结束（含到末根自动结束）与放弃都在同一 savepoint 内提交状态并追加结算分录；删除追加冲销；可用＝分录之和 |
| 单击/拖动/单键不成交 | 通过：`command_for_key` 没有成交命令，只有在草稿已选方向时 `Ctrl+Enter` 才确认；输入框焦点吞掉 `N`/`→`/`Space`/`Q`/`W`/`E`/`Delete`/`Ctrl+Z`；草稿未选方向时 `Ctrl+Enter` 不落到委托面板 |
| 迁移先备份 | 通过：schema < 6 时通用 `before-schema-<n>-*` 一致性备份；故障注入回滚后旧库与备份完好 |
| off-grid K 线与跨缺口窗口 | 通过：三个解析器都跳过不在 UTC 网格上的 K 线，不平移、不补造；日内相邻对要求 `下一根开盘 == 本根收盘` 且 `收盘 == 开盘＋周期`。前缀计数池与原逐窗扫描等价（坏 K 线 `[start, end]`、坏相邻对与拆股跳变 `[start/warmup, end-1]`，跳变检查只看相邻对） |
| verify_m5 live 与 G-06 | 通过：`deps.py` 退出码 2 记 not-run 且使总体为 not-run，永不记为通过；`M5-L-01`/`K01` 单独记录，只有全部批次断网才记 not-run，失败不冒充夹具结果 |
| 真实源与夹具分开声明 | 通过：`market_fixture.rs` 标明完全合成；`PROVENANCE.md` §5 登记合成行情、真实 M4 代码生成的 schema 5 库与已发布 v0.1.1 清单/签名 |
| M5-W08（计划 §10 专项） | 通过：`Cargo.lock` 中 `zip 9.4.2`、`quick-xml 0.42.0`（另有传递依赖 0.41.0，已在修复线以上）、`sha2 0.11.1`、`ed25519-dalek 3.0.0`、`getrandom 0.4.3`；生产代码唯一的解压点是 `read_single_csv_member`，路径穿越、绝对路径、符号链接、目录、多成员反例均被拒；v0.1.1 签名回归存在；A 复跑 `deps.py --audit` 结果为 passed（892 个包，0 个未处理漏洞，8 条“停止维护”提示） |

**A-F01（阻断，已修正）：准备训练数据期间资料库不可写。** `prepare_one_batch` 在一个 `SAVEPOINT` 里完成整批下载；`create_dataset_version` 是该批第一次写入，之后写锁一直持有到下载结束（币安 1h 约 160 s，欧易 1h 约 330 s），而连接的 `busy_timeout` 是 5 s。A 的探针：日线已就绪、4h 批正在下载时开局，5.56 s 后返回 `database is locked`（还被误标为 `PRACTICE_ACCOUNT_FAILED`、不可重试）。所以 B 的“实跑修复 3”（准备中已就绪级别可开局）在真实应用里不生效，首次准备约 15 分钟内开局、推进、划转全部失败；后台增量更新时网络一慢也会这样（违背 S-48“不阻塞开局”）。修正：拆成两段——**下载阶段**（`fetch_training_batch`）不写资料库，原始报文留在内存；**提交阶段**（`commit_training_batch`）在 savepoint 内建 `dataset_version`、存原始报文、写 K 线、记质量、发布。原子性不变：失败或取消仍整体回滚，不建版本、不移水位线。行 ID 与 `valid_from_version` 本来就由存储层按提交版本生成。持锁时间实测（调试构建、全历史 1h 夹具）：币安 1.8 s、欧易 4.2 s（含跨源比较），release 更短，都在 5 s 以内。新增回归 `training_m5::tests::m5_runs_start_and_advance_while_a_prepare_batch_downloads`：4h 批下载期间开局并推进一根，要求 4 s 内完成且作业仍在下载；取消后 `dataset_version` 数量不变、4h 水位线为空。

**同时修正（非阻断）**：币安 REST 分页原来用解析后最后一根已对齐 K 线推进游标；如果一整页都是偏离整点或未收盘的行，会提前结束分页，之后增量更新会永远停在“up_to_date”。现在改为用原始行中最新的开盘时间推进（`+1 ms`）。

**未改、记录在案**：币安 2018-02-08 00:00 那根 1h 只覆盖 28 分钟（交易所停机），按整小时保留；它后面紧跟缺口，所以只可能出现在窗口末端。对训练影响很小，并入 [LOOP-07](../decisions/open-questions.md) 的停机缺口处理。

**修正后验证**：`python scripts/quick_gate.py` passed（74 s，`-p delta-app` 84 项测试）；`python scripts/quick_gate.py --all` passed（108 s，全工作区 clippy 与测试）；`deps.py --audit` passed。按[两级门禁](../dev-rules/quality-gates.md#gate-tiers)，整套 `verify-stage`（M5 含 `--ui-smoke --ui-bench --live-data`，以及 M1～M4 回归）的证据仍对应 B 交付指纹 `c97131fc…`，没有重跑；A 修正只改动 `delta-app` 的 `training_data.rs` 与 `training_m5_tests.rs`，最终指纹 `9d51a55f…` 由快速门禁覆盖。审核时按指示没有碰 `delta-dev`；随后按用户要求重装开发版（[开发流程](../dev-rules/development-workflow.md#dev-install-at-handoff)）：`package-build` + `install-dev` 把 `0.1.2-dev.20261115093401`（`source_fingerprint = 9d51a55f…`，安装包 `dist/delta-dev-setup-0.1.2-dev.20261115093401-x64.exe`，`sha256 = aedb8bca…6696`）静默装到 `delta-dev`，替换 `0.1.2-dev.20261115084615`；正式版 `delta` 未触碰。

## 工作包与实现

| 工作包 | 实现 | 用例 |
| --- | --- | --- |
| M5-W01 数据修复与比特币训练数据 | `delta-app::training_data`：`prepare_training_data`（按（标的，级别）分批，各批独立 `dataset_version` 原子发布；币安月度归档校验 `CHECKSUM`、近期走 REST；欧易 `history-candles` 分页；水位线续传并回读重叠 3 根；欧易对币安同级别跨源比较）、`start_prepare_training_data` 后台作业、`training_data_status`。S-49 三项修复：① 稳定币汇率缺口不再阻塞加密 K 线发布（只记估值类质量问题）；② `missing_fee_asset_fx` 误报消除（报价腿稳定币从现金结算一次）；③ 候选池只按影响价格的质量问题排除（`QualityImpact`，`CandidateQualityFilter::PriceAffectingOnly`）。归档成员安全读取 `read_single_csv_member` | A-01、A-02、A-03、L-01 |
| M5-W02 加密多级别 | `delta-core::timeframe`（`1d`/`4h`/`1h` UTC 对齐、`aggregate_bars`），bar 行 ID 与质量键对日内级别使用开盘时刻，训练端口按级别截断，`idx_bar_training_port` 与区间预过滤，`get_training_aggregated_view` 只由已截断基础 K 线聚合，美股选日内级别明确拒绝 | A-04、A-05 |
| M5-W03 练习账户与历史分析 | schema 6（`m5_schema.rs`）：`practice_account`、只追加 `practice_account_entry`、`training_drawing`、`training_session` 新列与 `abandoned` 状态，迁移前先备份（`before-schema-5-*`），故障注入回滚；`delta-core::account_report`（`m5-account-report-1`）；划转、开局划出、结束/放弃结算、删除冲销、训练历史 | A-10、A-11、A-12 |
| M5-W04 引擎 `m5-sim-2` | 快捷单收盘成交（不利滑点/半点差与不利取整，零量 K 线拒绝，不缩量）、`ModifyOrder`、`UndoLast`（`SimReplayer` 保存命令前状态）、日内 `DAY` 在 UTC 日末过期；新字段为默认值时不进入规范化 JSON，`m4-sim-1` 哈希不变 | A-06、A-07、A-08、A-09、K03 |
| M5-W05 训练工作台 | `training_workbench.rs` 纯交互模型（键位、工具、播放、画线下单草稿、默认比例、拖动改单与锁定，按构造保证单击/拖动/单键/播放步不成交）；`training_workbench_ui.rs`（工具栏、信息行、覆盖物、练习账户侧栏、默认比特币开局卡片、执行器时钟播放）；`chart_canvas::ChartOverlay`；`ui_verification_m5.rs`（`--m5-ui-smoke`、`--m5-ui-bench`）。开发中修复：草稿打开且未选选项时 `Ctrl+Enter` 落到普通委托面板提交（误触缺陷） | A-13、A-15、K02 |
| M5-W06 CLI 与 AI | `delta-cli training prepare-data\|account\|history\|modify\|undo\|abandon\|restart\|view\|drawing`，`create --timeframe/--amount/--independent`，`order --fill-rule visible-close`；AI `training.run_summary` 增加级别、资金来源与有效操作次数；“已结束”含 `abandoned` | A-14 |
| M5-W07 门禁、文档与回归 | `delta-cli::verify_m5`；源码指纹加入 `m5-stage-cases.json`、`.zip`（按二进制）与 `.sig`；训练数据状态查询移入存储端口（`training_bar_coverage`，保持 `delta-app` 无裸 `FROM bar`）；正本同步见下文 | G-01～07、A-16 |
| M5-W08 依赖盘点与漏洞修复 | 升级 `zip` 2.6 → `=9.4.2`（修复含 Critical GHSA-m665-v7pm-7grj 在内的 6 条）、`quick-xml` 0.37 → 0.42（RUSTSEC-2026-0194/0195；适配新 API）、`sha2` 0.11、`ed25519-dalek` 3.0、`getrandom` 0.4；单成员归档读取拒绝路径穿越、绝对路径、符号链接、目录、多成员、非 `.csv` 与超过 16 MB 的成员；已发布 v0.1.1 的更新清单与签名用内置公钥验证仍通过（`update::tests::published_v0_1_1_manifest_signature_still_verifies`） | A-17 |

计划第 3 节的注释修改（策略/回测由 M5 改称 M6）已在 `i18n.rs` 与 `sim.rs` 完成。

## 本轮实际验证

全部在 Windows 11 本机以低优先级静默执行，被测指纹 `c97131fc40a746b5d0a475b7e0b79a9fb938d73b7c0e2831fe0a5c4793dcafe6`：

| 检查 | 结果 |
| --- | --- |
| `python scripts/quick_gate.py`（每次修改后） | passed（最后一次 30 s） |
| `verify-stage --stage M5 --plan-version 1.0 --ui-smoke --ui-bench --live-data`（`cargo run --release -p delta-cli`） | **passed**，977 s；报告 `.local/reports/m5/verify-stage-report.json`，日志 `.local/reports/m5/logs-28856-1794731398193/` |
| └ `M5-G-01～05`（文档、diff、fmt、clippy、`cargo test --workspace --locked`） | passed；测试 **171 passed / 0 failed / 2 ignored**（ignored 为需显式开启的 M1 公网探针与截图采集） |
| └ `M5-G-06` `deps.py --audit` / 盘点 | passed：892 个包，0 个未处理漏洞，8 条“停止维护”类告警只列出（`instant`、`paste`、`rustls-pemfile`、`rustybuzz`、`ttf-parser`、`zbus_names`、`zvariant`、`zvariant_derive`，均为传递依赖，无修复版本）；盘点到期 0 项，未满 7 天 5 项（见下） |
| └ `M5-G-07` 架构 | passed：19 个用例 ID 与清单一致；桌面 16 个源文件不访问核心引擎与存储；`account_report.rs` 只用 Decimal |
| └ `--ui-smoke` | passed：release 隐藏原生窗口，默认比特币开局（练习账户）、播放 5 根、快捷成交后撤销、画线、结束、揭示，12 次图表绘制，0 个可见窗口（夹具数据） |
| `verify-stage --stage M1 --plan-version 2.6` | passed（同指纹）：22 passed / 5 not-run |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` | passed（同指纹）：26 passed / 5 not-run，`M2-K01～06` passed |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | passed（同指纹）：16 passed / 3 not-run，`M3-K01～04` passed |
| `verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` | passed（同指纹）：24 passed / 2 not-run，`M4-K01～K03` passed |
| 本机开发版安装（`package-build` + `install-dev`） | 已装到 `delta-dev`：`0.1.2-dev.20261115084615`（上一版 `0.1.2-dev.20261113184806`），`source_fingerprint = c97131fc…`，安装包 `dist/delta-dev-setup-0.1.2-dev.20261115084615-x64.exe`；正式版 `delta` 未触碰 |

依赖盘点（D-35）：到期升级 0 项；未满 7 天、本阶段不升：`gpui-kit 0.9.1`（5 天）、`rusqlite 0.42.0`（4 天）、`uuid 1.29.2`（6 天）、`zip 9.6.0`（0 天）、工具链 `rust 1.100.0`（2 天，约 2026-11-20 满 7 天），登记 [DEPS-01](../decisions/open-questions.md)。`scripts/deps-exceptions.json` 无例外条目。没有回退的升级。

## 计划第 7 节不得缩减项

| 项 | 证据 |
| --- | --- |
| 截断在宿主层 | 训练端口 SQL 截断与哨兵测试；高一级别视图由宿主聚合，`m5_aggregated_view_matches_hand_calc_and_never_leaks_future_sentinel`；`M5-G-07` 桌面不访问存储/引擎 |
| Decimal 可手算 | `m5_quick_fill_at_visible_close_is_hand_checkable`、`m5_quick_fill_modify_undo_and_settlement_are_hand_checkable`、`account_report_is_hand_checkable` |
| 撤销不越过推进 | `m5_modify_and_undo_restore_exact_state_and_never_cross_an_advance`；门禁探针“推进后撤销被拒” |
| `m4-sim-1` 旧运行哈希不变 | 真实 M4 资料库夹具 `m5_genuine_m4_library_upgrades_and_old_run_hashes_are_unchanged`；M4 回归同指纹通过 |
| 练习账户守恒且放弃也结算 | `m5_abandon_restart_and_delete_keep_practice_account_conserved`；门禁探针放弃后 `equity == available`、无未结束分配 |
| 任何单击/拖动/单键不直接成交 | `no_key_or_click_ever_trades`、`focused_input_swallows_workbench_keys`、无头流程（含已修复的 `Ctrl+Enter` 误触） |
| 迁移先备份 | schema 5 → 6 先生成 `before-schema-5-*`，故障注入回滚后旧库与备份完好 |

## 限制与集中待处理项

- **H-06**（人工，`M5-M-01`）：训练工作台实机体验，步骤见 [USER-ACTIONS](../../USER-ACTIONS.md)；可顺延，不阻塞审核与远端动作。本机开发版已重装为包含 A-F01 修正的 `0.1.2-dev.20261115093401`（`9d51a55f…`）。
- [LOOP-07](../decisions/open-questions.md)：画线撤销用 `Delete`、不进入 `Ctrl+Z` 栈；画线不能拖动编辑；聚合视图不画画线；停机缺口不跨源补齐。
- [PERF-01](../decisions/open-questions.md)：首次准备 857～893 s，贴近 15 分钟上限（触发条件与做法已登记）。
- [DEPS-01](../decisions/open-questions.md)：未满 7 天的新版本与工具链 1.100.0。
- 门禁跨进程探针与 UI 冒烟/基准使用合成夹具；真实服务结果只来自 `--live-data`。

## 正本同步

[数据模型](../design/data-model.md)（schema 6 三张表与新列、多级别 bar 行 ID、`idx_bar_training_port`）、[金融规则](../design/financial-engine.md)（§7.1 `m5-sim-2`、练习账户、`m5-account-report-1`）、[交互工作流](../design/interaction-and-workflows.md)（§6.1 训练工作台）、[设计规则](../design-rules/DESIGN.md)（训练图表覆盖物）、[系统架构](../design/system-architecture.md)（§9）、[统一接口](../design/integration-contracts.md)、[AI 设计](../design/ai-design.md)、[仓库地图](../dev-rules/repo-map.md)、[质量门禁](../dev-rules/quality-gates.md)、[追踪](traceability.json)（FR-TRN-07～09 → implemented）、[集中台账](../decisions/open-questions.md)、[USER-ACTIONS](../../USER-ACTIONS.md)（H-06 步骤）、[夹具来源](../../tests/fixtures/PROVENANCE.md)。
