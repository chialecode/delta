# M4 v1.0 阶段验证与交付报告

更新：2026-11-13；角色：Agent B（实现与自检）、Agent A（集中技术审核）；唯一计划：[M4 阶段计划](stage-plan.md) v1.0，用例见 [m4-stage-cases.json](m4-stage-cases.json)。分支 `codex/m4-kline-training`；固定父提交 `45ac8c527ab5b51217d5f712deebd704e9d539ef`；**最终被测源码指纹 `02d0f1bdb3084b9e5d43d5f5e7ae1e05eab94a88049fc23ee9b28c13c69c727e`**（A 审核时为 `96254d7d…`，差异为[阶段内追加](#stage-additions)；B 自检指纹 `b104c94a…`，与 A 审核时的差异仅为 A 修正 F-01）；完整阶段提交 SHA 以 `git rev-parse HEAD` 为准。未跟踪 `workers/` 未读取、构建、修改或暂存；push、PR 与合并按 2026-11-14 的 ACT-13 授权执行；未创建 GitHub Release，未改远端设置。

## 当前判断

**B 实现与自检：完成；A 集中技术审核：通过（2026-11-13，修正 1 项 P2，工程阻断 0，见 [A 集中技术审核](#a-集中技术审核2026-11-13)）；产品验收：未开始（`H-05`、`ACT-01` not-run）。** 以下 B 自检记录保留原指纹；A 在审核指纹 `96254d7d…` 下复跑的四阶段门禁见 A 审核一节，阶段内追加后在最终指纹 `02d0f1bd…` 下的复跑见[阶段内追加](#stage-additions)。B 自检指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa` 下，`verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` overall **passed**（`M4-G-01～06`、`M4-A-01～18`、`M4-K01～K03` 全部 passed），同一源码指纹下 `M1 v2.6`、`M2 v1.0`（`--ui-smoke --ui-bench`）与 `M3 v1.0`（`--installer --ui-smoke`）三阶段回归全部 **passed**。

模拟与训练持久化位于独立的 `schema 5` 八张模拟表，不写入真实账本与 `fill` 表，训练创建、推进、笔记归档与删除前后 `ledger_revision` 均保持不变。结束后 AI 复盘在门禁中针对本机 `127.0.0.1` 双协议（`openai-responses` 与 `openai-chat-completions`）SSE 回环服务验证（`M4-A-14`），**不是真实在线模型服务**；真实在线模型复盘（`M4-L-01`，依赖可选 `ACT-01`）与训练实机体验（`M4-M-01`，[USER-ACTIONS](../../USER-ACTIONS.md) `H-05`）保持 `not-run`。门禁全程按 [D-25](../decisions/confirmed-decisions.md) 低优先级静默运行，可感知窗口数 `0`、前台焦点未变化，开发者本机 `%LOCALAPPDATA%\delta` 与 `%LOCALAPPDATA%\delta-dev` 未被触碰（[D-26](../decisions/confirmed-decisions.md)）。

## Spike 结论（第 5 节）

| Spike | 实际验证 | 结论与发现 |
| --- | --- | --- |
| M4-K01 截断数据端口 | 在 M1 夹具资料库上建立训练运行，把当前回放时钟 `visible_until` 之后的全部 `bar` 与 `corporate_action` 覆写为极端数值哨兵（`999999999`）与非十进制毒化字符串哨兵（`'POISON_FUTURE_BAR'`），同时构造多数据源同交易日 `close_at` 偏斜样本；依次读取桌面图表切片、SMA/EMA/MACD/RSI 指标、委托校验/预览、`delta-cli training show` 与 `ScopeTarget::TrainingRun` 下全部 5 个 `training.*` AI 工具输出，并逐步推进重复检查 | 通过，W02 继续。截断在 `crates/delta-data/src/m4_schema.rs` SQL 查询层（`is_final = 1` 且 `close_at <= visible_until`，公司行动 `ex_date <= effective_visible_ex_date(visible_until)`，覆盖加密现货 `00:00Z` UTC 日切边界）与单日按源优先级去重（`dedupe_bars_by_session_date`）完成；`crates/delta-app/src/` 生产代码对 `FROM bar` 裸 SQL 查询数为 `0`。所有消费者输出逐字节不含哨兵、指标预热终点永不晚于时钟、SQLite 触发器与宿主双重禁止时钟倒退 |
| M4-K02 确定性重放 | 同一训练命令序列（市价/限价/止损市价/OCO/入场附带止盈止损、撤单、推进、公司行动、结束）在 `Scratch`（从头重放）、`FromCheckpoint`（从 `periodic_20` / `pause` 检查点恢复重放）与独立第二子进程三种方式下逐步校验 `state_hash` 与期末报告 | 通过，W03 继续。`delta-core::sim`（引擎版本 `m4-sim-1`）全程使用 `rust_decimal::Decimal`（零 `f64`/`f32`）、有序容器 `BTreeMap`/`BTreeSet`（零 `HashMap`/`HashSet`）、零系统时钟读取与 `SHA-256(seed ‖ purpose ‖ counter)` 计数器模式确定性随机数；`SimState::canonical_json()` 生成每步 `state_hash`，三种方式每步哈希与最终 `m4-report-1` 报告逐字节一致 |
| M4-K03 下一根延迟 | 静态 CRT release 包 `delta.exe --m4-ui-bench`，低优先级并调用 `opt_out_of_execution_speed_throttling`（`power_throttling = "execution_speed_opt_out"`，[质量门禁](../dev-rules/quality-gates.md#foreground-quiet)），隐藏非激活原生窗口（`1400×950`），1,000 根可见历史 K 线＋默认 SMA/EMA/MACD/RSI＋持仓与挂单，20 步预热＋300 步实测“下一根”（含 SQLite 原子事务保存＋撮合＋时点拆股复权＋指标更新＋1,000 根视口切片＋GPUI 布局绘制＋DirectX `WM_PAINT`） | 通过，W06/W07 继续。实测 300 步分位数 `p50 = 31.151 ms`、`p95 = 39.966 ms`、`p99 = 57.453 ms`（`max = 131.345 ms`），满足 `p95 <= 100.0 ms` 门槛，原生图表重绘计数 `>= 300`、`visible_windows = 0`、`focus_stolen = false`。`TrainingDataPort` 对可见 K 线与指标采用增量缓存，且 `replay_events_on_state` 将重放行情扫描下界收敛至 `train_start_at`，无需启用检查点异步化备选分支 |

## 工作包与实现

| 工作包 | 实现 | 验证入口 |
| --- | --- | --- |
| M4-W01 回放时钟、抽样与数据端口 | `delta-data::m4_schema`：`query_training_visible_final_bars`、`query_run_visible_final_bars`、`query_next_training_step_bar`、`query_training_visible_corporate_actions`、`query_training_candidate_pool`（排除日历/覆盖缺口、`warning`/`partial` 质量问题、不完整公司行动与未解释跳变，计算 `candidate_pool_hash`）、`redraw_training_session_with_checkpoint`（开始前单 savepoint 更新抽样与初始检查点，开始后拒绝）。`delta-core::sim`：`DeterministicPrng`、`sample_candidate_sequence`（拒绝采样消除模偏差）、`derive_point_in_time_split_adjusted_bars`（仅回溯 `ex_date <= 当前时钟` 的拆股，分红不复权）。`delta-app::training`：`TrainingDataPort`、抽样前入库 `seed` 与 `candidate_pool_hash`、隐藏模式全链路脱敏（`标的 A（美股/ETF｜加密现货）`、`第 n 根`，直至 `finished` 且显式 `reveal`） | `M4-K01`、`M4-A-01`、`M4-A-02`、`M4-A-03` |
| M4-W02 撮合与费用引擎 | `delta-core::sim`（`SIM_ENGINE_VERSION = "m4-sim-1"`）：纯函数 `step` / `step_multi_bars`（支持多标的固定排序：事件时间、优先级、标的 ID、订单序号）与 `preview_order_submission`。支持 `Market`、`Limit`、`StopMarket`、`Oco`（止盈限价＋止损市价）及入场单附带 `AttachedBracket`，有效期 `Day` / `Gtc`（`StopLimit` 明确拒绝）；第 `t` 根收盘后提交、`t+1` 开盘生效；固定 `O → L → H → C` 路径，先处理开盘跳空（限价改善不劣于限价，止损跳空按开盘价加不利滑点），附带单仅在入场点之后路径段激活，同根双触达先执行止损并累加 `ambiguous_fill_count`；`round_price_adverse_tick` 不利方向舍入，违反 `tick_size`/`lot_size` 拒绝并返回上下最近有效值；买单 `1.10 × last_close` 市价缓冲＋最大预估费用预留、OCO 两腿共享一份持仓预留，`INSUFFICIENT_FUNDS` / `INSUFFICIENT_POSITION` / 整单 `INSUFFICIENT_FUNDS_AT_FILL`（不缩量）；零成交量/停牌/不可交易 K 线不成交（`Day` 过期、`GTC` 保留）；除权日开盘前拆股调整持仓与单位成本并以 `corporate_action_split` 取消挂单，现金分红按除权日假设单次入账。`FeeProfileVersion` 与 `settle_order_fees`：可组合费率/按单位/固定费/按单最低最高（多笔成交在订单结束时结算一次）/报价或实收资产扣费/四种舍入/滑点与半点差独立配置 | `M4-A-03`、`M4-A-04`、`M4-A-05`、`M4-A-06`、`M4-A-07`、`M4-A-08` |
| M4-W03 持久化、恢复与重放 | `delta-data::m4_schema` 与 `storage`：`CURRENT_SCHEMA_VERSION = 5`，`schema 4 → 5` 迁移前先经 SQLite Backup API 生成 `before-schema-4-*` 一致性快照（旧版读取器与 `schema > 5` 明确拒绝）；新增 8 张模拟/训练表（`fee_profile_version`、`simulation_run`、`training_session`、`sim_order`、`sim_fill`、`simulation_event`、`sim_checkpoint`、`training_note`）及不可变/时钟单调触发器；每步 `commit_simulation_step_atomic` 单事务提交，在 `initial`、每 20 步 `periodic_20`、`pause` 与 `finish` 写检查点；恢复时 `verify_or_mark_run_damaged` 重放命令并核对 `state_hash`，不一致标为 `damaged`；`create_retry_training_run` 建立带 `parent_run_id` 的独立新运行；`LibraryManifest::table_row_counts` 备份恢复核验、`export_library`（`simulation/simulation_bundle.json` + 8 张 CSV）与 `preview_delete_training_run` / `delete_training_run` | `M4-K02`、`M4-A-08`、`M4-A-09`、`M4-A-10`、`M4-A-11` |
| M4-W04 训练报告 | `delta-core::sim_report`（`SIM_REPORT_VERSION = "m4-report-1"`）：`compute_training_report`、`compute_buy_and_hold_benchmark`、`bar_path_slice_extrema`。全 Decimal 计算期末权益与收益率、已实现/未实现损益、费用合计、收盘权益曲线与最大回撤、持仓时间占比、已关闭交易组（`flat → open → flat`）胜率（零盈亏单列）、盈亏比与 Profit Factor（`NoClosedTrades` / `NoLosses` / `NoWins` 显式状态）、带样本数的期望值、入场成交点后至出场点前路径段 MAE/MFE（单位价格与百分比）、`PlanExecutionSummary`（有计划交易按计划退出或记录偏离理由占比，无计划交易单列）、同期同资金同费用买入持有基准及结构化 `SampleLimitations` | `M4-A-12` |
| M4-W05 训练笔记与 AI 复盘 | `delta-data::m4_schema` 与 `delta-app::{training, ai, host}`：训练中有无委托均可写 `training_note`（理由、情绪标签、关联委托），结束后 `archive_training_notes_to_journal` 经标准日志命令归档并标注“来自模拟训练”，不污染真实账本；用户自主评分（`save_training_self_evaluation`）与 AI 文本分离存储展示。新增 `ScopeTarget::TrainingRun(run_id)`：未结束运行一律以 `SIM_AI_REVIEW_REQUIRES_FINISHED` / `SCOPE_DENIED` 拒绝；结束后只开放 5 个只读工具（`training.run_summary`、`training.orders_and_fills`、`training.report`、`training.notes`、`training.revealed_bars`）并阻断真实账户工具；校验 `SIM-ORD-*` / `SIM-FILL-*` / `TRN-NOTE-*` 可打开证据引用与金融数值（剔除 `seed` 等元数据伪装），范围修订绑定 `latest_state_hash + revealed`；模型离线时训练与报告照常可用 | `M4-A-13`、`M4-A-14` |
| M4-W06 桌面训练工作区与 CLI | `delta-desktop::{training_ui, workbench, components, controller, ui_forms, ui_verification}`：主导航新增“训练”页（运行列表、新建对话框、复用 `KlineViewport` 与自绘 K 线画布的十字光标/拖拽平移/滚轮缩放、常驻“模拟训练”标识、预计成本与费用预览下单面板、`Ctrl+Enter` 提交委托、`N`/`→` 推进下一根且焦点在输入框时绝不误提交、`P` 暂停/`O` 数量/`J` 笔记/`Esc` 退焦点清光标、结束→报告→显式“揭示”→AI 复盘）。`delta-cli::training_cmd`：`delta-cli training create\|list\|show\|order\|cancel\|advance\|note\|finish\|reveal\|report\|replay\|retry\|delete` 及辅助子命令，与桌面共用同步 `AppHost` 契约，非 TTY 删除强制 `--yes` 并转义终端控制字符 | `M4-A-15`、`M4-A-16`、`M4-A-17` |
| M4-W07 门禁、清理、文档与回归 | `delta-cli::verify_m4`：`verify-stage --stage M4 --plan-version 1.0 [--ui-smoke] [--ui-bench]`；删除 M2 遗留 `Packager.toml` 并更新 `verify.rs` 指纹清单（加入 `docs/delivery/m4-stage-cases.json`）与 [仓库地图](../dev-rules/repo-map.md)；在 `verify.rs` 加固 `TEST-02`（M1 `lock_probe` 显式持锁就绪握手）；同步 [金融规则](../design/financial-engine.md)、[数据模型](../design/data-model.md)、[交互工作流](../design/interaction-and-workflows.md)、[AI 设计](../design/ai-design.md)、[统一接口](../design/integration-contracts.md)、[系统架构](../design/system-architecture.md)、[质量门禁](../dev-rules/quality-gates.md)、追踪表与台账，并在同指纹下跑通 M1/M2/M3/M4 门禁 | `M4-G-01～06`、`M4-A-17`、`M4-A-18` |

## S-41 平台费用预设核查与阶段清理记录

### 1. S-41 平台费用预设官方核查结论（2026-11-12）

按计划 `S-41`，B 在 2026-11-12 对富途美股与欧易现货官方费率规则进行了核查：

- **富途美股（Futu US Equity）**：官方佣金与平台使用费随开户主体地区（如香港、美国、新加坡等不同持牌实体）及用户所选套餐（阶梯式按月累计股数计费 vs 固定按股计费）变化，且卖出方向叠加随监管公告调整的 SEC 规费与 FINRA 交易活动费（TAF），无法在不引入隐含账户/日期假设的前提下折叠为单一静态预设。
- **欧易现货（OKX Spot）**：官方现货费率按普通用户与 VIP 1～9（30 日交易量、资产量或 OKB 持仓）分级，且严格区分挂单（Maker）与吃单（Taker）费率；仅凭日线 OHLC 无法无歧义判定每笔限价单属于 Maker 还是 Taker。
- **处置结果**：严格遵守 `S-41` 退出条件，本阶段**只内置 3 个明示为场景假设的模板**——`FPV:BUILTIN:ZERO_COST:v1`（`zero_cost`，零成本）、`FPV:BUILTIN:US_EQUITY_PER_SHARE_EXAMPLE:v1`（`us_equity_per_share_example`，美股按股收费示例）、`FPV:BUILTIN:CRYPTO_RATE_EXAMPLE:v1`（`crypto_rate_example`，加密按成交额示例），全部标记 `is_scenario_assumption = true`（`is_platform_preset = false`）；Rust 校验（`FeeProfileVersion::validate`）与 SQLite `CHECK` 约束同时强制任何非场景假设的内置预设必须具备非空官方来源 URL 与核查日期（`M4-A-08`）。富途美股与欧易现货平台预设保持在 [集中台账](../decisions/open-questions.md) `LOOP-06`。

### 2. 阶段清理与 `TEST-02` 修复

- **退役 `Packager.toml`（计划第 3 节）**：已从版本库删除 M2 遗留的 `Packager.toml`，同步移除 `crates/delta-cli/src/verify.rs` `source_fingerprint` 清单中的 `"Packager.toml"` 引用（并将 `"docs/delivery/m4-stage-cases.json"` 纳入源码指纹），同时更新 [仓库地图](../dev-rules/repo-map.md)。
- **关闭 `TEST-02`（M1 `lock_probe` 冷启动超时）**：在 `crates/delta-cli/src/verify.rs` 中将 `M1-A-15` 独占锁探针改为先等待持锁子进程输出显式持锁就绪标记（放宽新编译 debug 可执行文件的冷启动等待上限），确认持锁后再启动第二进程严格断言锁冲突拒绝，不放宽任何独占锁语义。台账 `TEST-02` 已标为 `resolved`。

## 本轮实际验证

最终被测源码指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa`，均在 Windows 11 本机以低优先级静默执行（[D-25](../decisions/confirmed-decisions.md)）：

| 检查 | 结果 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | passed（门禁 `M4-G-01～04` 实跑） |
| `cargo test --workspace --locked` | passed：`138 passed / 0 failed / 2 ignored`（`delta-app` 73、`delta-cli` 13、`delta-core` 16、`delta-data` 14、`delta-desktop` 17、`delta-model` 5，耗时 `15,198 ms`，门禁 `M4-G-05`） |
| `verify-stage --stage M1 --plan-version 2.6` | passed（同指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa`）：`22 passed / 5 not-run`（`M1-G-01～05`、`M1-A-01～17` 与 `POC-D01` 全部 passed：101 万行写入 `11,586 ms`、5,000 根版本区间查询 `14 ms`、备份恢复 `13,938 ms`；`M1-A-15` 显式持锁就绪握手 `23 ms` 通过；报告 `.local/reports/m1/verify-stage-report.json`） |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` | passed（同指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa`）：`26 passed / 5 not-run`（`M2-G-01～06`、`M2-A-01～20` 与 `M2-K01～06` 全部 passed：首屏 `p95 = 244 ms`，10 万根 K 线原生帧 `p50/p95/p99 = 6.611 / 7.910 / 12.164 ms`，10 万行表格滚动 `p50/p95/p99 = 2.571 / 5.564 / 7.609 ms`；报告 `.local/reports/m2/verify-stage-report.json`） |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | passed（同指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa`）：`16 passed / 3 not-run`（`M3-G-01～06`、`M3-A-01～10` 与 `M3-K01～04` 全部 passed：Inno Setup 7.1.1 双测试身份静默安装/共存/卸载、M2 便携版迁移、忙碌退避与 Ed25519 回环自动更新全部通过，开发者本机 2,474 个已安装文件零改动；报告 `.local/reports/m3/verify-stage-report.json`） |
| `verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` | passed（同指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa`）：`24 passed / 2 not-run`，3 个 Spike（`M4-K01～K03`）全部 passed（`M4-G-01～06` 与 `M4-A-01～18` 全部 passed，`M4-M-01` 与 `M4-L-01` `not-run`；报告 `.local/reports/m4/verify-stage-report.json`，日志目录 `.local/reports/m4/logs-14076-1794516666413`） |

M4 用例明细（报告 `.local/reports/m4/verify-stage-report.json`，日志在同目录 `logs-*`）：

| 用例 | 结果 | 实际证据（摘要） |
| --- | --- | --- |
| M4-G-01～06 | passed | `check_docs.py`、`git diff --check`、`fmt`、`clippy`、工作区测试实跑；架构与静音静态边界检查通过（`delta-app` 0 `pub async fn`、0 裸 `FROM bar` 查询、0 越界 `Command::new`、0 非回环绑定、`delta-desktop` 零越层依赖）；release `--m4-ui-smoke` 通过（`workspace_chart_paints = 2`、`total_chart_paints = 5`、`revealed_post_finish_bars = 60`、`power_throttling = "execution_speed_opt_out"`、`visible_windows = 0`、`focus_stolen = false`，耗时 `433 ms`） |
| M4-A-01（K01） | passed | 未来 K 线与公司行动写入 `999999999` 与 `'POISON_FUTURE_BAR'` 哨兵及同交易日多源偏斜样本：图表切片、指标预热、委托校验、CLI `show` 与全部 AI 工具零泄漏；加密 `00:00Z` 边界、时钟单调与未完成前拒绝揭示全部通过 |
| M4-A-02 | passed | `SHA-256` 计数器模式抽样与拒绝采样：同种子＋同 `candidate_pool_hash` 结果一致，缺口/`warning`/`partial`/缺公司行动区间全部排除，开始前重抽入库、开始后重抽拒绝，未揭示隐藏运行全链路无真实代码与日期 |
| M4-A-03 | passed | 录制 Yahoo 拆股与现金分红夹具：除权日前图表不暴露未来拆股；除权日开盘前持仓数量×比例、单位成本÷比例、总成本不变，挂单以 `corporate_action_split` 取消，分红单次入账并在报告标注除权日入账假设 |
| M4-A-04 | passed | 手算夹具：`t+1` 开盘生效，市价/限价跳空改善且不劣于限价，不利方向 `tick_size` 舍入，`StopLimit` 拒绝，违反 `tick_size`/`lot_size` 拒绝并提示最近合法上下界 |
| M4-A-05 | passed | `O → L → H → C` 路径：OCO 同根双触达先执行止损、取消另一腿并计入 `ambiguous_fill_count`；跳空跌破止损按开盘价加不利滑点成交；入场附带止盈/止损仅在入场点之后路径段激活成交 |
| M4-A-06 | passed | 买单 `1.10 × last_close` 缓冲与预估费用预留、OCO 共享单份持仓预留；`INSUFFICIENT_FUNDS`、`INSUFFICIENT_POSITION` 与跳空超缓冲整单 `INSUFFICIENT_FUNDS_AT_FILL`（不缩量，现金/持仓永不为负）；零成交量 K 线不成交，`DAY` 过期、`GTC` 保留 |
| M4-A-07 | passed | 合成单笔与多笔部分成交在 `settle_order_fees` 结算：按单最低费在订单结束时只结算一次，按笔费用精确累加，四种舍入模式与精度一致，`received_asset` 扣费准确扣减实收资产数量 |
| M4-A-08 | passed | `fee_profile_version` 被引用或内置后由 Rust 与 SQLite 触发器双重禁止原地修改/删除；派生 `v2` 只作用于新建运行，既有运行恢复与重放保持 `v1`；内置预设来源约束校验通过 |
| M4-A-09 | passed | 暂停关闭重开状态一致；子进程 `crash_mid_advance`（`BEGIN IMMEDIATE`＋小缓存溢出落盘后 `abort()`）重开无半步、无重复成交；篡改 `state_hash` 重开标为 `damaged` 并拒绝继续推进 |
| M4-A-10（K02） | passed | `Scratch`、`FromCheckpoint` 与第二进程三种重放每步 `state_hash` 及期末报告逐项一致；`retry` 生成引用 `parent_run_id` 的独立新运行，原运行委托、成交与报告不变 |
| M4-A-11 | passed | v0.1.1（`schema 4`）资料库打开前自动创建 `before-schema-4-*` 备份并升级至 `schema 5`，旧版读取器与 `schema > 5` 明确拒绝；训练全流程 `ledger_revision` 与真实 `fill` 表不变；备份恢复（核对全部 8 张新表行数）、开放导出（`simulation/simulation_bundle.json`＋8 CSV）与删除预览/删除通过 |
| M4-A-12 | passed | 手算报告夹具（盈亏交易组、分批加减仓、期末未平仓、零亏损、零交易、跨拆股回撤）：收益率、已实现/未实现、总费用、最大回撤、持仓占比、胜率（零盈亏单列）、Profit Factor、期望值、路径段 MAE/MFE、计划执行率、买入持有基准与样本限制全部一致 |
| M4-A-13 | passed | 训练中有无委托均可记笔记；结束后归档经正常日志命令保存并标注“来自模拟训练”，真实账本无模拟成交，原训练笔记不变 |
| M4-A-14 | passed | 双协议回环模型服务：未结束运行被宿主以 `SIM_AI_REVIEW_REQUIRES_FINISHED` 拒绝；结束后复盘引用可打开的 `SIM-ORD-*`/`SIM-FILL-*`/`TRN-NOTE-*` 证据，拦截模型捏造引用与未授权数值，笔记越权指令不扩大权限；模型离线时训练与报告正常可用 |
| M4-A-15 | passed | `delta-cli training` 子进程驱动的同一运行在 CLI `show`/`report` JSON 与桌面 `AppHost` DTO 中持仓、现金、委托、成交、报告数值及数据/引擎/费用版本逐项一致 |
| M4-A-16 | passed | GPUI 无头工作台测试：隐藏训练新建→下单（`Ctrl+Enter`）→`N`/`→` 推进（焦点在委托输入框时绝不误提交委托）→`P`/`O`/`J`/`Esc` 与图表十字光标/缩放→记笔记→结束→报告→显式“揭示”→AI 复盘；常驻“模拟训练”标识，揭示前无真实代码/日期，后台错误显式提示且不标成功 |
| M4-A-17（K03） | passed | Release 隐藏原生窗口（`execution_speed_opt_out`），1,000 根可见历史＋默认指标，20 预热＋300 实测步“下一根”延迟 `p50 = 31.151 ms`、`p95 = 39.966 ms`、`p99 = 57.453 ms`（`max = 131.345 ms`）（门槛 `p95 <= 100.0 ms`），原始样本与硬件口径保存在 `.local/reports/m4/` |
| M4-A-18 | passed | 门禁全程 `BELOW_NORMAL_PRIORITY_CLASS`、可感知窗口 `0`、焦点未变化、真实 `delta` / `delta-dev` 未触碰、无临时残留；同一指纹 `b104c94a39e258d8afa33502f4a11bc5a18f13baa9268197f745ddcdfd83f1aa` 下 M1 v2.6、M2 v1.0（`--ui-smoke --ui-bench`）、M3 v1.0（`--installer --ui-smoke`）全部 passed |
| M4-M-01 / M4-L-01 | not-run | `H-05` 训练实机体验待用户在开发版安装包上执行；真实在线模型复盘需用户按可选 `ACT-01` 配置真实模型连接 |

## 与计划的补充说明（未缩减范围）

- **结束与揭示两步显式分离（S-38 / S-46）**：点击“结束训练”（或执行不带 `--reveal` 的 `delta-cli training finish`）后，运行进入 `finished` 状态，但在用户显式点击“揭示”（或执行 `training reveal`）前，隐藏模式下的工作区、委托/成交、笔记、报告与 `training.*` AI 工具仍保持脱敏（`标的 A`、`第 n 根`），确保用户先阅读执行报告并完成自主评分，再揭示后续 60 根行情与真实标的/日期；揭示时同步推进 `ScopeTarget::TrainingRun` 的修订指纹。
- **CLI 完整生命周期子命令**：除计划列出的 12 个核心子命令外，`delta-cli training` 补充暴露 `reveal`、`redraw`、`start`、`pause`、`resume`、`fee-profiles`、`self-eval`、`evidence`，全部调用同一 `AppService` / `AppHost` 同步方法，并对终端 JSON 输出中的控制字符（含 BEL `\x07`、ANSI 转义与 Unicode `Cc`）统一安全转义。
- **零新增外部依赖**：`Cargo.toml` 与 `Cargo.lock` 在 M4 零改动，保持 M3 的 895 个第三方依赖包与既有许可盘点结论不变。

## 验证边界与接续

- **`not-run` 边界**：
  - `M4-L-01`（真实在线模型 AI 复盘）：需用户按 [USER-ACTIONS](../../USER-ACTIONS.md) 可选 `ACT-01` 配置真实协议、端点、模型与密钥后独立验证；门禁 `M4-A-14` 为本机 `127.0.0.1` 双协议 SSE 回环演练，不宣称真实在线模型服务已测。
  - `M4-M-01`（`H-05` 训练实机体验）：已在 [USER-ACTIONS](../../USER-ACTIONS.md) 补充 `H-05` 具体实机体验步骤，连同既有 `H-01～H-04`、`ACT-01`、`ACT-03`、`ACT-10` 私钥离线备份独立推进，不阻塞 A 技术审核。
- **模型与范围假设（`LOOP-06`）**：M4 模拟引擎为日线、单标的训练界面、现货做多、整笔成交（不模拟盘口流动性与部分成交参与率）、固定 `O → L → H → C` 盘中路径、隐藏模式不变换价格水平、现金分红按除权日开盘前入账假设；部分成交、止损限价、做空/融资/衍生品、多标的同时训练、小时/分钟回放、训练中 AI 教练模式及富途美股/欧易现货真实平台费率预设保留在 [集中台账](../decisions/open-questions.md) `LOOP-06`，并在每份训练报告 `SampleLimitations` 中明示。
- **首选下一步**：见下方 [接续](#接续)。

## A 集中技术审核（2026-11-13）

角色：Agent A；待审提交 `f5a127a92946509eec12afbf81784acd4ca25b9f`（父 `45ac8c5`）。A 用独立脚本从 `git archive HEAD` 重算源码指纹，与 B 声明的 `b104c94a…` 一致；B 的四份门禁报告 `overall_status = passed` 且均绑定该指纹；工作区仅有未跟踪 `workers/`（未读取、未暂存）。A 复跑 `cargo test --workspace --locked`（138 passed、0 failed、2 ignored），与 B 记录一致。对照计划第 4～7 节逐项复核实际 diff：

| 审核点 | 结论与依据 |
| --- | --- |
| 未来数据截断与哨兵（M4-K01） | `m4_schema` 所有训练读取在 SQL `WHERE` 中绑定冻结 `dataset_version`、`is_final = 1` 与 `close_at <= visible_until`；“下一根”先只查元数据列 `MIN(session_date)`，再只物化该交易日的行，后续哨兵行从不经过 `map_bar_row`；公司行动按 `ex_date <= effective_visible_ex_date(...)` 在 SQL 截断，推进时以下一根 `close_at` 查询并只取 `ex_date` 等于该交易日的行；多源同日偏斜按同一 `source_priority` 去重，重放取到的 K 线与当初推进时一致。时点复权只回溯 `ex_date` 不晚于该标的最后可见交易日与时钟日期的拆股，分红不复权 |
| 撮合与预留（m4-sim-1，S-36～S-40） | 第 t 根收盘提交、t+1 开盘生效；`O→L→H→C` 分段内按距当前路径点远近排序，成交后路径点前移，附带单只在激活点之后的路径段触发；限价跳空改善按开盘价加滑点且不劣于限价，止损跳空按开盘价加不利滑点；点差只用于市价单（与 S-39 一致）；买入止损的预留取 `max(止损价, 收盘价×1.1)`；`INSUFFICIENT_FUNDS_AT_FILL` 以“可用＋本单预留”判断并整单拒绝、级联撤销附带腿；零成交量不成交、`DAY` 过期；附带腿在桌面与 CLI 默认 `GTC`，不会随 `DAY` 入场单在当根后静默失效 |
| 费用组件（S-41） | 按单最低/最高费只在最后一笔成交时结算一次，按单层级以累计未舍入金额舍入后再差额分摊；`received_asset` 扣费减少实收数量、现金只扣成交额；平台预设必须有 `https` 官方来源与核查日期（Rust 与 SQLite 双重约束），本阶段只内置 3 个场景假设模板 |
| 公司行动（S-42） | 拆股：数量×比例、总成本不变、该标的全部挂单以 `corporate_action_split` 撤销；现金分红按除权日盘前入账并打标记；同一行动以 `(标的, ex_date, 类型)` 记账防重复 |
| 迁移、隔离与持久化（S-43） | `schema < 5` 时先 `create_consistent_backup` 生成 `before-schema-4-*` 再在 savepoint 内迁移，`schema > 5` 拒绝；每步一个 savepoint，含 `expected_prev_seq` 乐观校验；删除只涉及 8 张模拟表，不动日志与账本；备份清单新增全部表行数并在恢复时核对 |
| 重放（M4-K02） | 恢复＝最近检查点＋其后命令重放，逐步核对 `state_hash`；引擎零 `HashMap`/浮点/系统时钟。见下方 F-01 |
| AI 权限（S-45） | `ScopeTarget::TrainingRun` 每次工具调用都重新解析范围并要求运行 `finished`；训练范围只放行 5 个 `training.*` 工具，账户范围拒绝 `training.*`；`run_id` 参数不能跨运行；修订指纹绑定 `latest_state_hash`＋`revealed`＋训练笔记；笔记文字中的数字不进入授权数值集合 |
| 隐藏模式（S-38） | 摘要、委托、成交、笔记、事件、报告、图表切片与 AI 工具在 `hidden_mode && !(finished && revealed)` 时统一替换标的、日期与 K 线 ID；揭示前 `training.revealed_bars` 不返回任何后续行情 |
| 快捷键与 CLI（S-46、S-47） | 单键 `N`/`→`/`P`/`O`/`J` 仅在无文本输入焦点且无修饰键时生效，提交只能 `Ctrl+Enter`；CLI 与桌面调用同一 `AppHost` 方法 |
| 清理与 TEST-02 | `Packager.toml` 已删除，指纹清单同步；锁探针的持锁进程在拿到锁后才输出 `LOCKED`，父进程在 1.2 s 持锁期内严格断言冲突，锁语义未放宽 |

**A 修正（已 amend 进同一阶段提交）**：

- **F-01（P2，已修复）恢复重放把存储读取失败当成损坏**。触发：任何改变运行的操作（推进、下单、暂停等）先调用 `verify_and_recover_sim_state`，其中重放检查点之后的 `advance_bar` 需要经截断端口读 K 线；读取失败（磁盘 I/O 错误、表不可读）被转成与哈希不一致相同的字符串原因，`verify_or_mark_run_damaged` 随即把运行**永久**标记为 `damaged`，之后不能继续。预期（S-43）：只有哈希、命令、K 线或检查点不一致才标记损坏，读取失败应原样报错并保持运行不变。修复：`crates/delta-app/src/training.rs` 新增 `ReplayFailure { Integrity, Storage }`，`replay_events_on_state` 与 `compute_recovery_hashes_for_bundle` 区分两类；`verify_and_recover_sim_state` 遇 `Storage` 直接返回原错误、不调用标记，`Integrity` 行为不变。新增回归测试 `test_m4_a09_recovery_storage_read_failure_does_not_mark_run_damaged`：推进两根后让 `bar` 表不可读，修复前返回 `RUN_DAMAGED`（已实测失败），修复后返回 `TRAINING_PORT_FAILED`，运行仍为 `active`、`latest_seq = 2`，恢复表后可继续推进。原有篡改哈希即标记损坏的用例（M4-A-09）保持通过。

**P3 观察（不要求返工，不登记台账）**：

- F-02：`delta-core::sim` 与 `delta-data::m4_schema` 对多源公司行动的来源优先级顺序不同（前者 tiingo 优先，后者 yahoo 优先）。宿主端口已先去重、候选池排除多源比例冲突的区间，引擎实际只收到一条，结果不受影响；以后若直接向引擎传多源行动需先统一。
- F-03：计划执行率中，未关联委托但写于交易组持有期内的任意非空笔记都算作“偏离时记录了理由”，口径偏宽；报告已分列有计划/无计划，口径可在 H-05 反馈后收紧。
- F-04：关闭隐藏模式的运行在结束后、点击“揭示”前，AI 工具即可读到后续 60 根行情。隐藏模式下严格要求先揭示；非隐藏模式下标的本已公开，属复盘顺序体验问题。

**A 复跑（审核指纹 `96254d7db4d8ae6a1dfda724d156ba815142d00e4816a9392ce02f943f90714a`，Windows 11 本机，低优先级静默）**：

| 检查 | 结果 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo test --workspace --locked` | passed：139 passed、0 failed、2 ignored（新增 F-01 回归测试） |
| `verify-stage --stage M1 --plan-version 2.6` | passed：22 passed、5 not-run；日志 `.local/reports/m1/logs-22992-1794533088192/` |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` | passed：26 passed、5 not-run；图表 p50/p95/p99 = 7.2/8.6/9.4 ms，表格 = 2.7/5.1/7.3 ms，首屏 p95 236 ms；日志 `.local/reports/m2/logs-24308-1794533201162/` |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | passed：16 passed、3 not-run；日志 `.local/reports/m3/logs-18116-1794533472927/` |
| `verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` | passed：24 passed、2 not-run（`M4-M-01`、`M4-L-01`），3 个 Spike passed；“下一根” p50/p95/p99 = 31.3/41.0/51.7 ms（max 77.5 ms），`execution_speed_opt_out`，0 可见窗口、焦点未变；日志 `.local/reports/m4/logs-8236-1794534319914/` |

**K03 测量备注**：紧接 M3 安装包门禁之后的两次 M4 运行，“下一根” p95 为 85.2 ms 与 84.9 ms（p99 103～107 ms），仍在 100 ms 门槛内但余量小。为区分代码与环境，A 暂存修正、在 B 原树（`b104c94a…`）上复测得 p95 41.9 ms，随后恢复修正再测得 41.0 ms。两种代码在同一时段结果一致，说明偏高的两次来自安装门禁刚结束时的本机状态，F-01 只改错误分支，不影响性能。B 原树那次 M4 运行因 `M4-A-18` 拒绝与 M1～M3 报告指纹不一致而整体 failed，这是证据绑定的正确行为。各次报告留存于 `.local/a-review-m4/`。

**边界**：AI 复盘只在本机回环服务上验证，不是真实在线模型；`M4-M-01`（H-05）与 `M4-L-01`（ACT-01）保持 not-run。帧与延迟结论只适用于退出节流的测量口径与本机环境（RTX 4070 SUPER、缩放 1.5）。

<a id="stage-additions"></a>
## 阶段内追加（用户 2026-11-13 要求，已 amend 进同一阶段提交）

A 审核收敛后，用户在同一阶段内追加了与训练功能无关的要求（快捷方式、开发版图标、开发版安装时机），连同由此发现的指纹缺口一并修改，都在同一阶段提交里：

| 要求 | 实现位置 | 验证 |
| --- | --- | --- |
| **安装快捷方式**：静默安装（无人参与）首次安装时 `delta` 与 `delta-dev` 都创建桌面与开始菜单快捷方式；覆盖安装或更新时正式版不新建，开发版每次重新创建桌面快捷方式（提示有新版本）、不新建开始菜单快捷方式；有人参与的安装在向导的“附加任务”页两项都可选，覆盖安装时默认勾选当前已有的 | `crates/delta-cli/installer/delta.iss`（`[Tasks]` / `[Icons]` 的 `WantStartMenuIcon`、`WantDesktopIcon`，首装由 HKCU 卸载键判断）；向导文字在 `delta-zh-CN.isl` / `delta-en.isl`；`installer.rs` 按身份传 `DesktopOnUpdate` | `M3-A-03` 首装后桌面快捷方式存在、`M3-A-04` 正式版覆盖安装不重建已删除的两个快捷方式、`M3-A-06` 开发版覆盖安装重建桌面且不建开始菜单、卸载后桌面快捷方式消失；模板指令由 `verify_m3` 核对 |
| **源码指纹覆盖 `.isl`**：向导文字文件此前不在指纹里，改了它不会让门禁结果过期 | `crates/delta-cli/src/verify.rs` 指纹扩展名加 `isl` | 新指纹下四阶段全部重跑（下表） |
| **应用图标**：正式版与开发版都改为深灰底 `#3C4043` 白色 “D”（字母高度 250–780、笔画 110，按 1024 单位，替换原来的青绿三角标志），开发版另加右下角琥珀色 “DEV” 角标、一眼区分（2026-11-14 再次调整） | `crates/delta-desktop/assets/icon-dev.svg` / `icon-dev.ico`（`scripts/generate_icon.py` 确定性生成，`icon.svg` / `icon.ico` 同步重画；先渲染 1024 像素再按面积平均缩成 16/24/32/48/64/128/256 各尺寸，小尺寸的笔画与角标粗细与大图一致，2026-11-14 用户指出角标粗细不一后改）；两个 `build.rs` 对 `delta-dev`、`delta-dev-test` 嵌入它，`installer.rs` 的 `identity_icon` 让安装程序与卸载项用同一图标 | 新增测试 `development_identities_carry_the_dev_icon_everywhere`；`M3` 的开发版测试身份安装包用新图标构建、安装与升级通过 |
| **开发版安装时机**：每次需要用户人工检查时由 Agent 自动执行 `package-build` + `install-dev`（B 交付整个阶段、A 审查修复或返工后、按用户反馈修复后请用户复查），用户不用自己装；只为自动验证的打包与门禁不装（2026-11-14 用户纠正） | [协作流程](../dev-rules/development-workflow.md#dev-install-at-handoff)；`H-05` 第 ① 步改为先确认本机已是交付版本 | 请用户人工检查前执行（2026-11-14，图标统一缩放方法后）：`package-build` 生成 `dist/delta-dev-setup-0.1.2-dev.20261113184806-x64.exe`（`source_fingerprint` 与下表相同），`install-dev` 把本机 `delta-dev` 从 `0.1.2-dev.20261113180802` 静默更新到 `0.1.2-dev.20261113184806`（安装程序退出码 0，桌面快捷方式按开发版规则重建） |

**最终指纹 `02d0f1bdb3084b9e5d43d5f5e7ae1e05eab94a88049fc23ee9b28c13c69c727e` 下的复跑（2026-11-14，图标改为先渲染 1024 像素再按面积平均缩小之后；Windows 11 本机，低优先级静默）**：

| 检查 | 结果 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check` | passed |
| `cargo test --workspace --locked` | passed：140 passed、0 failed、2 ignored |
| `verify-stage --stage M1 --plan-version 2.6` | passed：22 passed、5 not-run；日志 `.local/reports/m1/logs-14288-1794593869943/` |
| `verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` | passed：26 passed、5 not-run；图表 p50/p95/p99 = 7.1/8.2/9.9 ms，表格 = 3.2/4.8/6.6 ms；日志 `.local/reports/m2/logs-18560-1794593943561/` |
| `verify-stage --stage M3 --plan-version 1.0 --installer --ui-smoke` | passed：16 passed、3 not-run；日志 `.local/reports/m3/logs-16484-1794594940697/`。同一指纹的前一次运行 failed：运行期间开发者本机真实的开始菜单 `DELTA Dev.lnk` 被外部删除（门禁只动 `DELTA Dev Test` 测试身份），`M3-A-10` 按规则判失败；未改代码，原样重跑通过 |
| `verify-stage --stage M4 --plan-version 1.0 --ui-smoke --ui-bench` | passed：24 passed、2 not-run（`M4-M-01`、`M4-L-01`），3 个 Spike passed；“下一根” p50/p95/p99 = 31.8/41.7/43.8 ms（max 48.0 ms），0 可见窗口、焦点未变；日志 `.local/reports/m4/logs-17916-1794595639733/` |

指纹只覆盖源码与夹具，本节文档修改不改变指纹，上述报告仍然有效。

## 接续

A 技术审核已收敛，工程阻断为零。用户已于 2026-11-14 授权 `ACT-13`（[USER-ACTIONS](../../USER-ACTIONS.md)）：推送 `codex/m4-kline-training`、创建到 `main` 的 PR，检查通过后 squash 合并并清理分支。H-05 实机体验、可选 ACT-01 真实模型与是否发布 v0.1.2 独立推进，不是该授权的前提。
