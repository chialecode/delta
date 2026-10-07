# R1 集中审核：Agent A

更新：2026-10-01（Agent A 集中审核；同日完成 RW-01、RW-02、RW-03 复核，最新结论见文末“RW-03 复核”节，前文保留首轮审核与 RW-01、RW-02 复核记录）。本报告依据 [REVIEW](../../REVIEW.md) 与 [R1 计划](../delivery/r1-execution-plan.md) v1.1，对照真实代码与 B 的 [交付证据](r1-delivery.md) 审核 32 项 P0 和必要技术门槛。A 已在同一阶段提交内修复明确的局部问题；较大或跨模块缺口按稳定 F 编号整批交回 B。阶段提交的新完整 SHA 不写在本报告中（避免自引用），由交接消息与现场 `git log` 给出。

## 基点与身份

| 项 | 值 |
| --- | --- |
| 审核对象 | 分支 `impl/r1-mvp`；B 交付提交 `fa409912a63b1309d718e318bcec4f9e52245eaa`；阶段父提交 `a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe`（文档框架，未 amend） |
| B 源码指纹 | 50 个文件，SHA-256 `3cae0fc2…cab519`；在迁移后的新机器上复现 B 的全部门禁为绿（40 项 Rust、8 项 worker、6 项契约测试，以及 fmt/clippy/文档门禁/diff-check） |
| 审核后源码指纹 | 52 个文件，SHA-256 `e11991c28d03c6a9986e1d8ba89e4821ad8db268ea0e87ca3e86d6033321afce`（`verify_r1.py` 同一算法，不含文档） |
| 环境 | Windows 11 x64；rustc 1.98.1；Python 3.12.14；离线，未连接任何真实账户、行情或模型端点 |
| 本地日志 | `.local/reports/r1-review/`（忽略目录）：B 门禁复现、`prefix_*_probe.log` 修复前失败探针、`postfix/` 修复后门禁、`b_results_before_review.json` |
| 未跟踪现场 | `workers/agent/` 为旧脚手架，保留原样，不纳入构建与提交 |

## 三项结论

| 结论 | 结果 | 依据 |
| --- | --- | --- |
| B 实现状态 | **未完成** | 生产应用服务与宿主不存在（F-10）：设置/账户、CSV 导入、行情提供方、笔记/附件、备份/恢复/导出/删除和性能基线均未实现；32 项 P0 中仅账本核心与 AI 运行时有实质实现 |
| A 技术审核 | **不通过，需返工** | 7 个 P0（F-01～04 已由 A 修复；F-11 待返工）；B 的 15 项“通过”中 14 项只覆盖部分预期，另有 2 项测试依赖缺陷才能通过（F-01、F-03）。A 修复后为 1 项通过、15 项部分覆盖、9 项未运行 |
| S0/S1 产品验收 | **不通过** | 自动用例未全部通过；实机（R1-M-01/02）与 live（R1-L-01/02）仍为 not-run，缺 ACT-01/02/03、Q-01 条件 |

## 实际检查与验证

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| Rust 全量测试 | `cargo test --workspace --no-fail-fast` | 68 项通过（B 原有 40 项 + A 新增 28 项回归） |
| 格式 / 静态检查 | `cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets -- -D warnings` | 退出码 0 |
| Worker / 验证器契约 | `python -m unittest discover -s workers/python/tests`；`python -m unittest discover -s tests/python` | 9 项、10 项通过 |
| 文档 / 空白 | `python scripts/check_docs.py`；`git diff --check HEAD` | 退出码 0 |
| 验证器 | `python scripts/verify_r1.py` | 退出码 1（诚实输出）：4 个门禁全绿；R1-A-25 通过，15 项部分覆盖，9 项未运行 |
| 修复前探针 | 用 HEAD 版本源码运行 A 新增测试 | core 12 项失败（`ledger/valuation/pnl`），网关 1 项、运行时 2 项、存储 4 项、worker 1 项失败；`unknown_cost_sale` 与 `period_pnl_counts_only_scope_accounts` 两项后补，没有修复前探针 |

“部分覆盖”表示映射命令全部通过，但该用例预期中仍有未执行部分；验证器把它计为非通过，并保留命令证据。

## 发现清单

“A 已修”表示修复已在本阶段提交内完成并定向复验；“B 返工”进入下方返工批次。

| ID | 级别 | 触发 → 实际 / 预期 | 依据 | 位置 | 处置与验证 |
| --- | --- | --- | --- | --- | --- |
| F-01 | P0 | 转入待入账区的事件已经部分改动现金和持仓（如现金不足的买入、缺 FX 的入金）→ 应“记录但不应用”。B 的 FIFO 部分卖出黄金用例没有入金也通过，正是依赖此缺陷 | [金融规则 §2](../design/financial-engine.md#2-记账与成本) 待入账语义 | `crates/delta-core/src/ledger.rs` `apply_one` | A 已修：按事件快照，失败时回滚后再挂入待入账；修正 `ac02_golden.rs` 的黄金用例（补入金 1000，断言现金 899、无待入账）；`r1_a_07_*_pending_without_side_effects` |
| F-02 | P0 | 入金/出金/费用/利息/分红金额 ≤0 被接受，出金可透支 → 应拒绝或挂入待入账 | 金融规则 §1/§2 | `ledger.rs` `require_positive`、`apply_cash_flow` | A 已修：`r1_a_07_non_positive_cash_flow_amount_is_rejected`、`withdrawal_beyond_cash_is_pending` |
| F-03 | P0 | 用第三资产（BNB）支付的买入手续费又从计价货币扣了一次：USDT 余额 9340 → 应为 9400，只扣 BNB 0.009；BTC 成本 10060 | 金融规则 §2 第三资产费用资本化 | `ledger.rs` 买入分支 | A 已修：`cash_out = gross + quote_fees`，成本另计第三资产费用价值；`r1_a_07_third_asset_fee_buy_charges_fee_asset_only`；`transfers_corporate_crypto.rs` 改为精确断言 USDT 9400 且无待入账 |
| F-04 | P0 | 成本未知的卖出分录不平衡；未知与已知成本混合处置时，已实现损益仍记为完整；成本币种与成交计价币种不同时混算；直接用持有资产付费时分录不平衡且币种混用 → 应分录平衡，并且“未知≠0” | 金融规则 §2；[数据模型](../design/data-model.md) 复式零和 | `ledger.rs` `Disposal`、`dispose_fee_asset`、`apply_standalone_fee` | A 已修：权益科目“待补全成本”承接差额；`unknown_cost_disposals` 计数；币种不一致挂入待入账；`fee_disposal_pnl`/`valued_fees` 统一按本位币；5 项 `r1_a_07` 回归 + `unknown_cost_sale_posts_balanced_without_realized` |
| F-05 | P1 | 不匹配的转入会消耗转出池，更正后的转入无法再配对；非现金转账要求资产→USD 汇率（BTC） → 应保留转出，并按批次历史成本的本位币值过渡 | 金融规则 §3 | `ledger.rs` `TransferAllocation` | A 已修：`r1_a_06_transfer_mismatch_keeps_outbound_for_corrected_inbound` |
| F-06 | P1 | 区间已实现/费用/收入取累计值而非区间值；损益聚合含范围外账户；无价格的已清仓持仓使总值降为 partial；缺价不列出缺失项和已知部分；跨币种已实现直接相加 → 应按区间、按范围、按本位币/报表币种计算 | 金融规则 §4 | `crates/delta-core/src/{pnl,valuation}.rs` | A 已修：`value_accounts` 范围过滤、`known_market_value`、`missing_inputs`、期末减期初；`r1_a_09_*`、`r1_a_05_closed_position_*`、`r1_a_06_period_pnl_counts_only_scope_accounts` |
| F-07 | P1 | 网关参数校验对对象 schema 不生效（`spec.as_str()` 为空即放行）：`limit: 9999`、`scope` 传字符串、`scope` 多余字段都能通过 → 应返回 INVALID_ARGUMENT | [统一能力接口](../design/integration-contracts.md) | `crates/delta-app/src/ai/gateway.rs` | A 已修：递归校验 type/union/enum/范围/required/additionalProperties/items，另有一项测试防止 schema 使用不支持的关键字；`r1_a_15_*` |
| F-08 | P1 | 压缩摘要和历史中的 System/Summary 条目以 system 角色发给模型；无法识别的消息类型被当作 Summary；摘要请求不响应取消，取消被报成 provider 错误；存储遇到损坏行时取默认值或丢弃；调试环境变量会把工具输出打到 stderr → 应把它们作为不可信用户数据，取消即中止，损坏即报错 | [上下文状态 §4](../design/context-state-management.md)；摘要作为不可信历史内容插入，不能提升为系统指令 | `runtime.rs`、`crates/delta-infra/src/sqlite/ai_store.rs` | A 已修：`r1_a_18_compaction_*` 断言请求体中无 system 角色；`r1_a_18_cancel_during_compaction_summary_stops_without_checkpoint` |
| F-09 | P1 | 收到 `ToolCallReady` 就立即执行，不等模型回合正常结束；助手的 function_call / tool_calls 没有持久化和编码，下一轮只发 `function_call_output`，真实 Responses/Chat 端点会拒绝；中断的调用不标为 interrupted；压缩可能拆开调用与输出 | [Rust 模型客户端 §5](../design/rust-model-client.md)；模型上下文必须按完整的 tool call/output 组装 | `runtime.rs`、`crates/delta-infra/src/model/adapters.rs` | B 返工 |
| F-10 | P1 | 没有生产 `AnalysisHost` 和应用服务：测试宿主返回固定值，UI 与 AI 不共用服务。W02/W04/W05/W06/W09/W10 及 W08 生产宿主未实现：设置/账户/组合、CSV 预览与提交、文件行情/日历、笔记 UI 与自动保存、附件、备份/恢复/导出/删除、OS 凭据存储、负债与手工估值、基础回撤、性能基线 | 计划 v1.1 §3；FR-BASE/DATA/JRN/OPS、FR-LED-05/06、FR-AI-01/08 | `crates/delta-app/src` 仅有 `ai/` 和 `contracts.rs` | B 返工 |
| F-11 | P0 | 网关只比较运行时自己填写的 `scope_ref`，不核对模型传入的 `scope.account_ids` 和期间是否在冻结范围内；同一会话换范围后，新运行仍会把旧运行的工具输出带入模型上下文 → 应在宿主代码中强制范围，换范围时使用干净上下文 | [AI 设计](../design/ai-design.md)、FR-AI-03/07、R1-A-17 | `gateway.rs` `execute`、`runtime.rs` `build_input_items` | B 返工 |
| F-12 | P1 | 报告校验只检查证据 ID；与工具结果矛盾的数字（引用有效）仍被视为有效报告 → 应校验关键数字并给出不足说明 | FR-AI-02/04、R1-A-21 | `runtime.rs` `validate_report_references` | B 返工 |
| F-13 | P1 | 在途转账（已转出、尚未转入）在估值和净值中消失，跨区间时制造虚假的亏损和收益；没有人工余额对账 | 金融规则 §3/§4；FR-LED-04、FR-DATA-05 | `valuation.rs` 未读取 `transfer_pool` | B 返工 |
| F-14 | P1 | 每次保存修订都向 FTS 追加一行而不删除旧行，旧版本内容仍能被搜到；`MATCH` 直接使用原始输入（语法错误或运算符注入）；检索没有范围/可见性过滤 | FR-JRN-01、R1-A-13 | `store.rs` `save_journal`、`search_journal` | B 返工 |
| F-15 | P1 | 桌面账户页显示硬编码行（包括 F-03 的错误值 9340），说明文字却声称“R1-A-02/05 已由服务测试验证” → 应展示服务结果，或明确标为静态演示 | FR-AI-08 共用能力；R1-A-05 | `apps/desktop/src/pages.rs` | A 已修说明文字和数值（改为“静态 POC 演示行，非服务结果”，9400）；接入服务属于 B 返工 |
| F-16 | P1 | 指标只有手算样本，没有锁定版本的 TA-Lib 独立参考向量，也没有 visible_until 哨兵覆盖 → FR-MKT-02 的参考对齐无证据 | FR-MKT-02、R1-A-11 | `workers/python` | B 返工 |
| F-17 | P1 | SSE 单记录计数器从不清零，总量超过 1 MiB 的正常长输出全部失败；传输错误信息带 URL；连接失败不重试，与文档不一致 | Rust 模型客户端 §5 | `crates/delta-infra/src/model/mod.rs` | A 已修：按空行分界计数（可跨 chunk）、超限后结束流、`without_url()`、仅连接阶段错误有界重试；`r1_a_16_long_stream_*`、`transport_failure_message_has_no_url`、`model::tests` |
| F-18 | P1 | 时间戳按原始 RFC 3339 文本做字典序比较：`16:00-05:00` 的收盘 K 线在 20:00Z 就可见（未来数据越界）；估值读取复权序列；FX 返回查询时间而非生效时间 | 金融规则 §3/§6；FR-MKT-04 | `store.rs` | A 已修：规范为 UTC 纳秒 `Z` 格式写入并查询、只用 `adjustment='raw'`、返回实际 `effective_at`；`store_market_data.rs` 中 4 项 `r1_a_10_*`（修复前均失败） |
| F-19 | P1 | 验证器的门禁失败不影响退出码；匹配 0 个测试的过滤器算通过；部分覆盖的用例记为 passed（B 报告 15/25） | 质量门禁；R1-A-25 | `scripts/verify_r1.py` | A 已修：门禁计入退出码，0 个测试即失败，新增 `partial` 状态；4 项负向夹具 |
| F-20 | P2 | worker 收到非对象消息、params/spec 类型错误、NaN 时崩溃或输出非法 JSON | Worker 协议 v1 | `workers/python/delta_worker.py` | A 已修：`test_malformed_messages_do_not_kill_the_worker` |
| F-21 | P2 | MA20 对 21 个值求和后除以 20；线段画在前收盘价与 MA 之间；预热随视口重新开始 | FR-MKT-02 | `apps/desktop/src/chart.rs` | A 已修：对全序列计算 `sma_close`，画 MA 与 MA 之间的线段；2 项单元测试 |
| F-22 | P2 | 打包 README 中的协议示例错误；守恒性质测试计算了成本守恒却丢弃结果 | 金融规则 §2 | `scripts/package_r1.py`、`conservation_prop.rs` | A 已修：断言每笔分录平衡且成本守恒（只容许 Decimal 28 位求和残差，清仓时为精确 0），并新增 proptest 回归种子 |
| F-23 | P1 | R1-A-16 未覆盖重定向、请求超时、背压、两协议请求体断言；`model_protocol.rs` 复制了一份 fake 端点 | R1-A-16 预期 | `crates/delta-infra/tests` | B 返工（A 已为 `tests/fake` 加入请求体捕获） |
| F-24 | P2 | 审核前创建的开发库中，时间戳不是规范格式 | F-18 | `migrate.rs` | B 返工：增加规范化迁移，或在交付说明中声明 R1 开发库需重建（尚无用户数据） |
| F-25 | P1 | 其余用例的覆盖缺口：A-01 依赖边界与许可检查；A-07 超精度/溢出；A-08 重复来源导入与复权不重复计算；A-09 负债；A-17 运行中换账户/页面与旧快照重放；A-19 UI/窗口/切库取消与迟到事件；A-20 开发配置/凭据/AGENTS 探针、worker 不接收凭据、诊断导出 | r1-cases.json | 各测试目标 | B 返工 |

## 32 项 P0 现状

| 范围 | 现状 |
| --- | --- |
| FR-LED-01/02/03/08 | 核心已实现（A 修复 F-01～05 后）；没有服务/UI 读取路径；复权交互未测 |
| FR-LED-04/06 | 核心部分实现：在途估值缺失（F-13），基础回撤未实现 |
| FR-LED-05 | 部分：资产估值已有，负债与手工估值缺失 |
| FR-DATA-02 | 部分：`source_ref` 幂等与更正已有；导入批次、重建流程和测试缺失 |
| FR-DATA-03、FR-MKT-04 | 部分：存储原语已由 A 修复（F-18）；没有提供方、日历或 UI 标注 |
| FR-MKT-01/02 | 部分：合成数据 POC 图表（缩放/平移/十字线/MA20）与 worker 指标；没有搜索、自选或参考对齐（F-16） |
| FR-JRN-01 | 部分：存储保存与检索（F-14）；没有 UI 与自动保存 |
| FR-AI-01/02/03/04/06/07/08 | 部分：双协议客户端与运行时可用于受控端点；工具配对（F-09）、生产宿主（F-10）、范围强制（F-11）、数字校验（F-12）和 OS 凭据存储缺失；core 不依赖模型，离线能力在架构上成立 |
| FR-BASE-01/02、FR-DATA-01/05、FR-MKT-03、FR-JRN-02/03/04、FR-OPS-01/02/03/04 | 未实现（F-10、F-13） |

## 用例矩阵（审核后）

`python scripts/verify_r1.py` 实测：R1-A-25 **passed**。R1-A-01/05/06/07/08/09/10/11/15/16/17/18/19/20/21 为 **partial**，命令全部通过，未覆盖部分见 F 编号和验证器理由。R1-A-02/03/04/12/13/14/22/23/24 为 **not-run**（产品路径未实现）。R1-M-01/02、R1-L-01/02 为 **not-run**。

## 需保留的 A 修复

返工时保留并继续扩展，不回退：`crates/delta-core/src/{ledger,valuation,pnl}.rs`；`crates/delta-core/tests/{review_regressions,ac02_golden,transfers_corporate_crypto,conservation_prop}.rs` 及 proptest 回归种子；`crates/delta-app/src/ai/{gateway,runtime}.rs`；`crates/delta-infra/src/model/mod.rs`；`crates/delta-infra/src/sqlite/{store,ai_store}.rs`；`crates/delta-infra/tests/{fake/mod,model_protocol,runtime_ai,store_market_data}.rs`；`apps/desktop/src/{chart,pages}.rs`；`workers/python/` 下的 worker 及其测试；`scripts/{verify_r1,package_r1}.py`；`tests/python/test_verify_contract.py`。

## 返工批次与人工事项

B 一次性处理 F-09～F-14、F-15（服务接入部分）、F-16、F-23～F-25；按原编号逐条回应修复和验证，继续 amend 同一阶段提交。可复制 prompt 随交接消息给出，引用计划 v1.1 和最新完整 SHA。集中台账记为 RW-01。

需要人参与的事项不变：ACT-01（真实模型端点）、ACT-02（IME/DPI/干净安装）、ACT-03 与 Q-01（去标识账户与行情来源）。这些条件在返工完成、自动用例转为通过之前不阻塞开发，也不能代替实现缺口。

## 限制

所有结论都基于离线的受控端点和合成数据。没有调用真实模型、账户或行情，没有做 IME/DPI/干净安装实机检查，也没有法律许可结论。修复前探针只覆盖本次新增的测试，没有对 B 的全部历史结果逐条重放。

## RW-01 复核（Agent A，2026-10-01）

按原 F 编号复核 B 的 [RW-01 回应](r1-delivery.md)（F-09～F-16、F-23～F-25）。A 对照代码复核，并先用修复前源码跑新增测试，确认缺陷存在；局部确定的问题在同一阶段提交内修复，较大缺口记为 RW-02 交回 B。

| 项 | 值 |
| --- | --- |
| 复核对象 | 分支 `impl/r1-mvp`；B 返工提交 `46f0577006c301e0794cbf8e14143e0f3d167db0`；阶段父提交 `a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe`（文档框架，未 amend） |
| B 源码指纹 | 58 个文件，SHA-256 `55c358050af559b1c429bbc64ce8bd31b8ccd2afb461c3171ed9019a8f8b886c`。现场复现 B 的门禁：fmt、clippy、104 项 Rust、10 项 worker、12 项契约、文档门禁和 diff-check 均为 0，`verify_r1.py` 为 1 |
| 复核后源码指纹 | 58 个文件，SHA-256 `128cf56971f11f022725ae55625068b86c48ba27f1ebe1ec7a21720a4dd963b3`（`verify_r1.py` 同一算法，不含文档） |
| 环境 | Windows 11 x64；rustc 1.98.1；Python 3.12.14；离线；TA-Lib 未安装 |
| 本地日志 | `.local/reports/r1-rw01-review/`（忽略目录）：`prefix_runtime.log`、`prefix_app_service.log` 为修复前失败记录；`gates_after.log`、`cargo_test_after_full.log`、`results_after.json` 为修复后结果 |
| 未跟踪现场 | `workers/agent/` 保持原样，不提交 |

### 三项结论（复核后）

| 结论 | 结果 | 依据 |
| --- | --- | --- |
| B 实现状态 | **未完成** | 生产 `Library` / `ProductionHost`、范围快照、工具回合配对、FTS、在途和对账已落地。复核新发现 15 项缺陷（第 1～15 项），A 已全部修复。跨文件疑似重复（A-03）、可打开的证据（A-21）、TA-Lib 参考向量（A-11）仍未实现 |
| A 技术审核 | **不通过，进入 RW-02** | 新发现中 2 项为 P0：换范围后旧范围工具输出回流（F-11）、删除资料库误删同目录另一库的附件（F-10）；另有 11 项 P1、2 项 P2。均已由 A 修复并有测试。交回 B 的是 RW-02 中 3 项 P1 和 2 项 P2 |
| S0/S1 产品验收 | **不通过** | 自动用例 7 项 partial；R1-M-01/02、R1-L-01/02 仍为 not-run（缺 ACT-01/02/03、Q-01） |

### 按 F 编号复核

| ID | B 回应要点 | A 复核 | 结论 |
| --- | --- | --- | --- |
| F-09 | 缓冲到正常终态才执行；两协议成对编码；未完成调用标 `interrupted`；压缩按原子组 | 执行时机、配对与压缩分组成立。新缺陷：被中断的 `function_call` 在下一轮仍被发送，没有对应输出，真实端点会拒绝整段请求（P1）；第二次压缩只总结上次检查点之后的消息，第一份摘要从上下文中静默丢失（P1，上下文状态 §4） | A 已修，关闭 |
| F-10 / F-15 | `sqlite/app.rs` 生产服务、`host.rs` 宿主；桌面行来自 `load_demo_lines` | 服务与宿主成立，桌面行来自服务。新缺陷见下方 A 修复清单第 6、8～14 项；跨文件疑似重复与证据可打开未实现 | A 修复后部分关闭；剩余进入 RW-02 |
| F-11 | `ScopeSnapshot` + `check_tool_args`，越界为 `SCOPE_DENIED`，换范围用干净上下文 | 宿主与网关双重校验成立。新缺陷（P0）：只比较上一代运行。范围 A→B→B 时，第三轮又带回 A 的工具输出与证据。换范围后的压缩也会把旧范围历史发给模型 | A 已修，关闭；长历史换范围后需开新会话，属 RW-02（P2） |
| F-12 | 数字与工具回执对齐，partial 时须写明不足 | 机制成立。新缺陷：中文正文里数字紧贴汉字（如“总损益99，”）时不被检查；紧贴汉字的伪造引用（如“证据res:x”）也能通过（P1）。另外，报告引用的证据无法打开 | A 已修数字与引用扫描；证据可打开进入 RW-02 |
| F-13 | `in_transit()` 加回组合估值；对账报告差额不改账 | 新缺陷：在途非现金按历史成本估值而非市价（P1）；对账用当前账本比较历史时点观测，并把首屏缓存行当作观测（P1） | A 已修，关闭 |
| F-14 | 修订先删旧 FTS 行；短语转义；按账户过滤 | 成立。新缺陷：先 `LIMIT 200`（中文路径为最近 500 条）再在 Rust 侧过滤，其他账户笔记多时，范围内结果漏检（P2） | A 已修，关闭 |
| F-16 | `visible_until` 截断与哨兵测试；锁定公式 SMA 向量 | `visible_until` 成立。向量只覆盖 SMA，按公式锁定，没有实跑 TA-Lib；EMA/RSI/MACD 的种子差异没有参考 | 保持 partial，进入 RW-02 |
| F-23 | 不跟随重定向；超时不重试；有界队列背压；单一 fake | 三项测试有效，fake 只剩一份。设计要求连接、空闲、总三类超时，实现只有总超时（P2） | 基本关闭；超时分类进入 RW-02 |
| F-24 | v4 迁移在同一事务内规范时间戳 | 成立。复核发现 W09 必测路径缺失：迁移前无一致性备份；打开更新版本资料库时不拒绝（P1） | A 已修，关闭 |
| F-25 | 依赖边界/许可清单测试；A-07/08/09/17/19/20 补测 | 依赖边界与许可清单测试成立，许可证仍未选定（Q-02） | 关闭 |

### A 本轮修复（同一阶段提交）

除第 16 项外，新增测试都在修复前源码上失败过（记录见本地日志）：

1. F-13 在途非现金估值：`ledger.rs`、`valuation.rs` 改为按 `at` 的价格估值，并沿用历史成本计算未实现损益；成本未知时标 partial。测试：`r1_a_06_in_transit_position_is_valued_at_market_not_at_cost`（修复前 10000，应为 50000）。
2. F-11 换范围：`runtime.rs` 的 `visible_context` 判断会话中是否有其他经济范围的运行。有则不用检查点，只保留同范围运行的消息，超出窗口时拒绝压缩，不把旧范围发给模型。测试：`r1_a_17_a_later_run_in_the_new_scope_still_drops_the_old_scope`。
3. F-09 孤立调用：`paired_history` 只发送“完整调用 + 完整输出”配对，中断记录留在库内。测试：`r1_a_18_interrupted_call_is_not_resent_without_an_output`。
4. 压缩摘要接续：压缩输入带上前一份摘要。测试：`r1_a_18_second_compaction_carries_the_first_summary_forward`。
5. F-12 中文数字与引用：按 ASCII 连续段扫描。汉字视为分隔；`2026年1月31日`、`2026-01-31`、`第3` 这类日期和序号不算金额；千分位整体解析；`event:`、`journal:` 也纳入引用校验。测试：`r1_a_21_numbers_next_to_chinese_text_are_checked`（含伪造引用和中文日期的正例）。
6. A-09 手工估值：同账户、同资产、同方向只取查询时点前最新的一条，不再累加（修复前 220，应为 120），并按查询时点汇率折算（金融规则 §4）。测试：`r1_a_09_newer_manual_mark_replaces_the_older_one`。
7. F-13 对账：每条观测与其 `as_of` 时点的账本比较，排除 `cache` 行。测试：`r1_a_06_reconciliation_uses_the_ledger_at_the_observation_time`。
8. A-03 导入：同一文件内 `source_ref` 重复的行判为 error。事件、写入计数检查和批次状态在同一事务提交；有任何一行无法写入则整批回滚，回执不再把未写入的行算作 accepted。修复前回执 accepted 2，实际只写入 1。测试：`r1_a_03_receipt_counts_match_the_events_written`。B 的 `r1_a_03_csv_preview_commit_*` 原先断言“接受已存在来源 → 成功且不重复入账”，这正是静默丢弃；已改为断言失败且不写入，跳过后计数为 1/1。
9. A-04 更正：分组标记、替代事件和报告过期改在一个事务内完成。修订号取组内最大值 +1，并检查写入计数。修复前同一原事件第二次更正被唯一索引静默忽略，余额停在第一次更正值。测试：`r1_a_04_a_second_correction_of_the_same_event_is_applied`。
10. A-15 宿主持仓行：`get_portfolio_summary` 的持仓行与估值使用同一 `as_of` 截止，不再包含之后的成交（修复前 150，应为 100）。测试：`r1_a_15_portfolio_tool_lines_follow_as_of`。
11. A-23 删除隔离（P0）：B 允许两个资料库共用目录，因而共用 `attachments/`；“恢复到旧库旁边，再删除旧库”会删掉新库的附件。按系统架构 §9，改为一个目录只放一个资料库：在已有资料库的目录中新建或恢复时拒绝。附件路径只接受 `attachments/<文件名>`。测试：`r1_a_23_deleting_a_library_keeps_another_librarys_attachments`。该测试在修复前探针之后按目录规则重写，探针记录的是旧版失败（`attachment file missing`）。
12. A-22 恢复：清单必须列出 `library.sqlite`，只恢复清单中逐一校验过的文件；目标已存在时提前拒绝。测试：`r1_a_22_restore_rejects_a_manifest_that_omits_the_library`。
13. A-22 导出：说明中写真实记账币种，不再写死 USD；账户 CSV 改用 `csv` 写出并转义。测试：`r1_a_22_export_names_the_library_book_currency`。
14. W09 迁移：迁移前用 `VACUUM INTO` 把一致性副本写入 `backups/<库名>.pre-migration-v<N>.sqlite`；资料库版本高于当前构建时拒绝打开。测试：`r1_a_22_older_library_is_backed_up_before_migration_and_newer_is_refused`（修复前没有备份）。
15. F-14 检索召回：账户过滤改在 SQL 内、`LIMIT` 之前执行（`json_each`）；中文改用转义后的 `LIKE` 匹配当前修订，不再只扫描最近 500 条。测试：`r1_a_13_scoped_search_is_not_cut_short_by_other_accounts`。
16. `scripts/verify_r1.py`：R1-A-03、R1-A-21 加入 partial 并写明原因，R1-A-11 理由补充 EMA/RSI/MACD。此项无修复前探针。

### 实际检查

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| Rust 全量 | `cargo test --workspace` | 119 项通过（B 104 项 + A 新增 15 项），0 失败 |
| 格式 / 静态检查 | `cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets -- -D warnings` | 退出码 0 |
| Worker / 契约 | 两处 `python -m unittest discover` | 10 项、12 项通过 |
| 文档 / 空白 | `python scripts/check_docs.py`；`git diff --check HEAD` | 退出码 0 |
| 验证器 | `python scripts/verify_r1.py` | 退出码 1（诚实输出）：4 个门禁全绿；18 项 passed，7 项 partial（R1-A-03/11/12/18/19/21/24），实机 2 项与 live 2 项 not-run |
| 修复前探针 | 修复前源码 + 新增测试 | runtime 4 项、service 9 项、迁移 1 项、core 1 项失败，失败原因与上表一致 |
| 首轮 A 修复回退检查 | `git diff 8051671 46f0577`（首轮审核提交 → B 返工）逐行查删除内容 | 未回退：转账分配、笔记修订、两协议工具输出、验证器命令均迁移到新位置，仍被测试覆盖 |
| `results.json` 抽查 | `.local/reports/r1/results.json` | `plan_version` 1.1；`git_base` 为 `46f0577…` / `a1180cf…`；源码指纹与上表一致；partial 均带命令退出码和理由 |

全部测试使用临时库、合成账本和本机假端点。TA-Lib 固定向量是按公式锁定的，不是实跑结果。

### 用例矩阵（RW-01 复核后）

R1-A-01/02/04/05/06/07/08/09/10/13/14/15/16/17/20/22/23/25 为 **passed**。R1-A-03/11/12/18/19/21/24 为 **partial**，理由见验证器和 RW-02。R1-M-01/02、R1-L-01/02 为 **not-run**。

### RW-02 返工批次（交 B）

| ID | 级别 | 缺口 | 期望 |
| --- | --- | --- | --- |
| F-10 / A-03 | P1 | 只按全库唯一的 `source_ref` 判重，不同账户的同号成交会被误判；没有跨文件内容指纹；“接受重复”的语义含糊 | 来源唯一性改为按账户（v5 迁移）；同账户同来源视为“已导入”，自动跳过并给出理由；内容指纹（时间/标的/数量/价格/费用）只生成疑似重复，用户确认接受时以独立来源写入；把跳过与接受拆成清楚的 API；测试计数与理由 |
| F-12 / A-21 | P1 | 证据 ID 每次调用随机生成，没有持久化，无法打开；`evidence_ref` 表未使用；没有经由运行时 + `ProductionHost` 得到 68 的端到端测试；没有任何数值回执时，报告中的数字不受检查 | 持久化回执与证据（结果、事件、笔记、修订），提供打开接口；用合成库端到端验证 68 及其分项；区分报告与闲聊：报告中的数字必须有回执支持 |
| F-16 / A-11 | P1 | 只锁定了 SMA 公式向量 | 在隔离环境安装固定版本 TA-Lib，生成 SMA/EMA/RSI/MACD 参考向量（含预热）并锁定版本与来源；安装失败时如实记录，不得写成实跑 |
| F-23 | P2 | 只有总超时 | 分别实现连接、空闲（读）和总超时，并测试长流不被误杀 |
| F-11 | P2 | 换范围后历史超出窗口时，现在拒绝并要求开新会话 | 推荐范围切换时自动开新会话（或给检查点打范围标记），并说明 UI 行为 |

R1-A-12/18/19/24 的已知 partial 原因不变，由 B 继续补齐，或在 ACT-02 实机时测量。

### 需保留的 A 修复（RW-01 复核）

返工时保留，不得回退：`crates/delta-core/src/{ledger,valuation}.rs`（在途、手工估值）；`crates/delta-app/src/ai/runtime.rs`（`visible_context`、`paired_history`、摘要接续、`ascii_runs`）；`crates/delta-infra/src/sqlite/{store,app,migrate}.rs`（`insert_event`、`ensure_sole_library`、导入/更正事务、对账、恢复清单、导出、迁移前备份与版本拒绝、检索）；`crates/delta-infra/src/host.rs`（as-of 持仓行）；`crates/delta-infra/tests/{runtime_ai,app_service}.rs` 新增测试，以及对 `r1_a_03`、`r1_a_14`、`r1_a_22` 的修改；`crates/delta-core/tests/review_regressions.rs`；`scripts/verify_r1.py` 的 partial 理由。

### 限制

- 结论只基于离线合成数据和受控假端点。
- 修复前探针只覆盖本轮新增测试。
- A-23 测试在探针后重写（见上）。
- 在中文正文中扫描数字仍是启发式规则：百分比与比率按原样比对，可能误报。

## RW-02 复核（Agent A，2026-10-01）

按原 F 编号复核 B 的 [RW-02 回应](r1-delivery.md)：F-10/A-03、F-12/A-21、F-16/A-11、F-23、F-11，以及 A-12/18/19/24 的 partial 处理。A 对照代码复核，新增测试先在 B 的源码上运行，确认缺陷存在。局部确定的问题在同一阶段提交内修复，其余缺口记为 RW-03 交回 B。

| 项 | 值 |
| --- | --- |
| 复核对象 | 分支 `impl/r1-mvp`；B 返工提交 `4298b08c206c3e655cbd55b77bafe3cfb13564de`；阶段父提交 `a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe`（文档框架，未 amend） |
| B 源码指纹 | 58 个文件，SHA-256 `dd523c07bdf9edc9bf30a40bca242e4575120d6b60db00cc4b5cb7b200e7fc98`，现场复算一致 |
| 复核后源码指纹 | 58 个文件，SHA-256 `0dc9aa34d56e84788279baae825ccc896150246c27312c54562062c2bd2acfb6`（`verify_r1.py` 同一算法，不含文档） |
| 环境 | Windows 11 x64；rustc 1.98.1；Python 3.12.14；离线。TA-Lib 0.6.8 只在隔离的 `.local/talib-venv` 中，不入库。本机在回环地址上配置了系统代理（`HTTP(S)_PROXY` 和 Windows 系统代理） |
| 本地日志 | `.local/reports/r1-rw02-review/`（忽略目录）：`prefix_app_service.log`、`prefix_runtime.log` 为修复前失败记录；`cargo_test_after.log`、`verify_after.log`、`results_after.json` 为修复后结果 |
| 未跟踪现场 | `workers/agent/` 保持原样，不提交 |

### 三项结论（RW-02 复核后）

| 结论 | 结果 | 依据 |
| --- | --- | --- |
| B 实现状态 | **未完成** | F-10/A-03、F-16/A-11、F-11 成立。F-12/A-21 的机制成立，但有 4 项缺陷（A 已修）。F-23 的空闲和总超时成立，连接超时在本机没有得到验证。桌面搜索和自选界面未实现（A-12）。“压缩中撤权”没有实现路径（A-18） |
| A 技术审核 | **不通过，进入 RW-03** | 复核新发现 6 项缺陷：0 项 P0，3 项 P1，3 项 P2，A 均已修复并补测试。另把 B 误标为 passed 的 R1-A-16、R1-A-18 改回 partial，并更正 R1-A-12 的理由。交回 B 的 RW-03 共 3 项，均为 P2 |
| S0/S1 产品验收 | **不通过** | 自动用例 5 项 partial（R1-A-12/16/18/19/24）；R1-M-01/02、R1-L-01/02 仍为 not-run（缺 ACT-01/02/03、Q-01） |

### 按 F 编号复核

| ID | B 回应要点 | A 复核 | 结论 |
| --- | --- | --- | --- |
| F-10 / A-03 | v5 按账户唯一来源；同账户同来源自动跳过；跨文件内容指纹只作疑似；跳过与接受拆成两个接口 | 迁移、计数、跨账户同来源、同文件同指纹独立和“接受时写入自身来源”都有测试，成立。新缺陷（P1）：预览之后，另一个文件写入了同指纹成交，再提交旧预览，该行仍按“有效”写入，绕过疑似重复决定；用例要求修订后提交旧预览须拒绝。新缺陷（P2）：同一文件以同一映射再次预览，直接报 SQLite 唯一约束错误，旧预览作废后也无法重新预览 | A 已修，关闭 |
| F-12 / A-21 | 结果 id 持久化；`open_evidence`；区分报告与闲聊；`ProductionHost` 端到端 68 | 端到端 68 和证据可打开成立。新缺陷（P1）：证据 id 只由工具名、`scope_ref` 和判别量生成，写库用 `ON CONFLICT DO UPDATE`。同一运行内，不同账户子集的持仓摘要共用一个 id；账本变化后重算会覆盖旧值，旧报告的证据打开的是新值。修复前探针中两个不同结果的 id 都是 `res:get_portfolio_summary:e1b4e515192368b7`。新缺陷（P1，回归）：`looks_like_report` 取代了“有回执就核对全部数字”的规则，调用工具后回答“组合市值 99999”被当作成功报告接受。新缺陷（P2）：`explain_pnl` 把期间内全部事件（含已被更正替代的原事件）都列为证据，60 笔事件产生 61 个引用，工具输出随账本线性增长。新缺陷（P2）：不带修订的 `journal:<id>` 会打开最新修订 | A 已修，关闭 |
| F-16 / A-11 | 隔离 venv 实跑 TA-Lib 0.6.8，锁定 SMA/EMA/RSI/MACD 向量 | A 在 `.local/talib-venv` 中重新执行 TA-Lib 0.6.8（C 库 0.6.4）：已提交向量与现场输出最大差为 0。另用 5 组 120 根随机序列对照，worker 与 TA-Lib 的 SMA(5)/EMA(12)/RSI(14)/MACD(12,26,9) 差值在 1e-8 内，预热位置一致。实跑成立 | 关闭 |
| F-23 | 连接、空闲、总超时分开 | 空闲与总超时实现正确，本地假端点测试有效。连接超时测试在本机不成立。客户端默认走系统代理，发往 192.0.2.1 的请求由本机代理接收，约 5.2 秒后返回 502，客户端按 5xx 重试 3 次，共约 17 秒。临时探针改用不走代理的客户端后，网络层拦截同样在约 5.2 秒失败，错误也不是连接错误或超时。该测试只断言耗时小于 28 秒，通过与连接预算无关 | R1-A-16 改回 partial；进入 RW-03（P2） |
| F-11 | 换范围开子会话或复用；检查点带范围标记 | 子会话复用、旧转录不外发和检查点范围标记都有测试；A 的 A→B→B 测试仍通过。新会话路由后，`mixed` 分支不可达 | 关闭 |
| A-12/18/19/24 | 只保留需要实机的 partial | A-19、A-24 的理由成立。A-12：桌面搜索和自选界面尚未实现，不是实机测量能补的，理由已改为“未实现（RW-03），截图需实机”。A-18：B 去掉了 partial，但用例步骤“压缩中撤权”既没有实现也没有测试；B 的检查点写入失败测试事先没有检查点，证明不了“失败不切换”。A 已加强该测试，并把 A-18 改回 partial | 理由已更正；余项进入 RW-03 |

### A 本轮修复（同一阶段提交）

第 1～5 项的新增测试在修复前源码上失败（记录见本地日志）：

1. F-12/A-21 证据不可变：`tool_evidence_id` 把值和质量纳入哈希，按内容寻址；`store_analysis_result` 改为 `ON CONFLICT DO NOTHING`。测试：`r1_a_21_evidence_id_keeps_the_value_it_was_issued_for`。内容寻址后 id 由结果决定，黄金 68 测试改为先直接调用一次宿主取得 id。
2. F-12/A-21 数字核对：本轮有工具回执时核对全部数字；没有回执时，用扩充后的财务名词（损益、市值、成本、余额、持仓、资产、股息、费用等）或证据引用判断是否为报告。测试：`r1_a_21_numbers_are_checked_without_pnl_keywords`。
3. F-12/A-21 事件证据有上限：只引用期间内最新的 20 条有效事件，结果写明 `events_in_period` 与 `events_cited`。测试：`r1_a_21_pnl_event_evidence_is_bounded`。
4. F-12/A-21 笔记证据必须钉住修订，不带修订的 id 拒绝打开。测试：`r1_a_21_journal_evidence_requires_a_revision`。
5. F-10/A-03 陈旧预览：提交时按当前账本重新判定每一行，判定变化即拒绝并要求重新预览。测试：`r1_a_03_preview_is_stale_once_the_ledger_gains_a_matching_fill`。
6. F-10/A-03 重新预览：同一账户、文件和映射已有未提交预览时，由新预览替换；已提交时明确拒绝，提示“已导入该文件”。由第 5 项测试覆盖。此项是写测试时发现的：B 的表结构有 `UNIQUE(account_id, file_hash, mapping_version)`，预览用的是普通 `INSERT`；没有单独的修复前探针日志。
7. A-18 测试加强：`r1_a_18_checkpoint_write_failure_keeps_the_previous_entry` 先完成一次压缩，再让第二次检查点写入失败，断言原检查点仍生效，两批历史都在。
8. `scripts/verify_r1.py`：R1-A-16、R1-A-18 改回 partial 并写明原因，R1-A-12 的理由已更正。
9. 设计同步：[AI 设计](../design/ai-design.md) §4 补证据 id、笔记修订、数字核对和事件引用上限的规则。[交互工作流](../design/interaction-and-workflows.md) §3 补重复导入、预览替换和陈旧预览的规则。
10. 清理：`commit_import` 不再读取未使用的 `reason` 列。

### 实际检查

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 现场身份 | `git rev-parse`、`source_fingerprint()` | HEAD `4298b08…`、父提交 `a1180cf…`、B 指纹 `dd523c07…`，均与交接一致 |
| 修复前探针 | B 源码 + 新增 5 项测试 | 5 项全部失败，原因与上表一致；B 原有 29 项服务测试在同次运行中通过 |
| TA-Lib 对照 | `.local/talib-venv` 中直接调用 `talib` | 固定向量与现场输出最大差 0；5 组随机序列与 worker 一致（≤1e-8） |
| 代理探针 | 临时测试直接构建 reqwest 客户端（已删除） | 走系统代理：502，5.17 秒；不走代理：既非连接错误也非超时，5.18 秒 |
| Rust 全量 | `cargo test --workspace` | 134 项通过（B 129 项 + A 新增 5 项），0 失败 |
| 格式 / 静态检查 | `cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets -- -D warnings` | 退出码 0 |
| Worker / 契约 | 两处 `python -m unittest discover` | 11 项、12 项通过 |
| 文档 / 空白 | `python scripts/check_docs.py`；`git diff --check` | 退出码 0 |
| 验证器 | `python scripts/verify_r1.py` | 退出码 1（诚实输出）：4 个门禁全绿；20 项 passed，5 项 partial（R1-A-12/16/18/19/24），实机 2 项与 live 2 项 not-run |
| A 既有修复回退检查 | `git diff 2fa31a29 4298b08` 的删除行；关键函数与 A 的 14 项测试逐一检索 | 未回退：`delta-core` 无删除；`visible_context`、`paired_history`、摘要接续、`ascii_runs`、`insert_event`、`ensure_sole_library`、迁移前备份与版本拒绝、对账、恢复清单、as-of 持仓行都在，A 的测试全部保留并通过 |
| `results.json` 抽查 | `.local/reports/r1/results.json` | `plan_version` 1.1；`git_base` 为 `4298b08…` / `a1180cf…`；源码指纹与上表一致；partial 均带理由 |

全部测试使用临时库、合成账本和本机假端点。TA-Lib 对照在本机隔离 venv 中离线执行；门禁比较的是已提交的向量，不导入 `talib`。

### 用例矩阵（RW-02 复核后）

R1-A-01/02/03/04/05/06/07/08/09/10/11/13/14/15/17/20/21/22/23/25 为 **passed**（20 项）。R1-A-12/16/18/19/24 为 **partial**，理由见验证器和 RW-03。R1-M-01/02、R1-L-01/02 为 **not-run**。

### RW-03 返工批次（交 B）

| ID | 级别 | 缺口 | 期望 |
| --- | --- | --- | --- |
| F-23 / A-16 | P2 | 连接超时没有被验证；客户端默认走系统代理，在本机代理把不可达地址变成 502 | 连接配置增加代理策略。推荐默认跟随系统代理，可选不用代理或指定代理；明文 `http` 远端不得经代理发送凭据。测试一律不走代理，并以确定性方式触发连接超时（例如挂起的自定义解析器，或不完成握手的本地端点）。断言错误属于连接阶段、按连接失败重试，且总耗时远小于总预算 |
| F-09 / A-18 | P2 | “压缩中撤权”没有实现路径 | 定义 R1 的撤权，例如撤销模型连接，或缩小授权账户、笔记范围。压缩进行中撤权时：停止摘要，不写检查点，运行进入终态，旧范围内容不再外发。补测试 |
| F-15 / A-12 | P2 | 桌面没有标的搜索和自选界面，服务接口已测；`search_instruments` 的 `LIKE` 没有转义 `_` | 桌面接入服务层的搜索与自选，补自动化交互测试并转义通配符；截图留到 ACT-02 实机 |

R1-A-19（窗口关闭 ≤1 秒）和 R1-A-24（交互 P95、GPU 50 FPS）需在 ACT-02 实机测量，不需要先改代码；B 可预先加计时埋点。

### 需保留的 A 修复（RW-02 复核）

返工时保留，不得回退：`crates/delta-infra/src/host.rs`（内容寻址证据 id、事件证据上限）；`crates/delta-infra/src/sqlite/app.rs`（证据只写不改、笔记证据必须带修订、`commit_import` 提交前重新判定、预览替换与已导入拒绝）；`crates/delta-app/src/ai/runtime.rs`（有回执即核对全部数字、`REPORT_TERMS`）；`crates/delta-infra/tests/{app_service,runtime_ai}.rs` 中本轮新增和加强的测试；`scripts/verify_r1.py` 中 R1-A-12/16/18 的理由。RW-01 复核列出的保留项继续有效。

### 限制

- 结论只基于离线合成数据和受控假端点。
- 本机有系统代理和网络层拦截，连接超时的结论只适用于本机环境。
- 报告识别仍按关键词判断。有回执时会核对全部数字，工具调用后的闲聊数字可能误报；运行时会重试一次，仍不一致就失败。
- 事件证据上限 20 条、按时间取最新，是 A 的实现选择，不是产品决定。
- 重新预览缺陷没有单独的修复前探针日志（见第 6 项）。

## RW-03 复核（Agent A，2026-10-01）

按原 F 编号复核 B 的 [RW-03 回应](r1-delivery.md)：F-23/A-16、F-09/A-18、F-15/A-12，以及 A-19/A-24 的计时埋点。A 对照代码复核，不以 B 的叙述为准；新增测试先在 B 的源码上运行。A 还对关键分支做了 17 项变异，逐一检查测试能否杀死。

| 项 | 值 |
| --- | --- |
| 复核对象 | 分支 `impl/r1-mvp`；B 返工提交 `9284ccba62c726ef1b1e5023c40bc02ba8ec3dbe`；阶段父提交 `a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe`（文档框架，未 amend） |
| B 源码指纹 | 61 个文件，SHA-256 `80f30e79e25d9c4888556e15d8c4e51cab7bbd80c4b2cb6a638bf1d38f422e14`，现场复算一致 |
| 复核后源码指纹 | 61 个文件，SHA-256 `de8f0b81f59911efbb9ac4f266254d7db05b20d1e0534291687576fcd32f7857`（`verify_r1.py` 同一算法，不含文档） |
| 环境 | Windows 11 x64；rustc 1.98.1；Python 3.12.14；离线 |
| 本地日志 | `.local/reports/r1-rw03-review/`（忽略目录）：`cargo_test_b.log` 为 B 源码的全量测试；`mutations_on_b.json` 为 B 源码上的 17 项变异；`prefix_model_proxy.log` 为修复前失败与代理探针记录；`mutations_after_a.json`、`cargo_test_after.log`、`verify_after.log` 为修复后结果 |
| 未跟踪现场 | `workers/agent/` 保持原样，不提交 |

### 三项结论（RW-03 复核后）

| 结论 | 结果 | 依据 |
| --- | --- | --- |
| B 实现状态 | **RW-03 已完成** | 三项都已实现，并有测试。变异检查中，测试杀死了 17 项中的 15 项，另 2 项见下文说明。复核发现 1 项缺陷（P2），A 已修复。R1 余下的项目只能在实机或真实服务上完成，不属于代码返工 |
| A 技术审核 | **通过，RW-03 关闭，不开新返工批次** | 新发现 1 项 P2：`custom` 代理与回环明文端点组合时，令牌以明文经过代理。A 已修复并补测试，另加强了 3 处测试 |
| S0/S1 产品验收 | **不通过** | 3 项 partial（R1-A-12/19/24），都只差实机测量；R1-M-01/02、R1-L-01/02 为 not-run（缺 ACT-01/02/03、Q-01） |

### 按 F 编号复核

| ID | B 回应要点 | A 复核 | 结论 |
| --- | --- | --- | --- |
| F-23 / A-16 | 代理策略 `system`/`direct`/`custom`；路由在连接前决定；连接超时用挂起的解析器和停滞的 TLS 握手确定性触发；删除 192.0.2.1 测试；`system` 策略用子进程验证 | 两条连接超时测试都用 `Direct`，不走任何代理，也不依赖外网。测试中唯一的外部地址 203.0.113.9 只用于路由和构建断言，不建立连接。192.0.2.1 测试已删除。B 源码上的 5 项代理/超时变异（P1～P5）都被杀死。<br>**新缺陷（P2）**：`custom` 代理加回环明文端点（例如 `http://127.0.0.1:…`）时，路由是 `Custom`。请求以明文 `POST` 发给代理，`Authorization: Bearer` 和令牌对代理可见。修复前探针中，假代理收到 3 次带令牌的明文请求（一次加两次重试），本机端点 0 次。这违背了 B 自己写的“回环地址总是直连”和“避免令牌明文穿过代理”。<br>另有测试缺口：子进程探针没有确认子进程真的运行了测试。过滤名不匹配时子进程同样退出 0，“假代理什么都没收到”的两项断言会空过 | A 已修，关闭；R1-A-16 passed |
| F-09 / A-18 | 撤权定义为撤销模型连接和缩小授权账户，笔记随账户；`Grants`/`RunLease`；运行前接纳；检查点写入与撤权互斥；终态 `revoked`，重启不改写 | 定义与 [上下文状态](../design/context-state-management.md) §5 一致。笔记只经按账户过滤的 `search_journal` 进入模型，没有单独的笔记通路。接纳、租约令牌、`commit` 互斥、`revoked` 终态和 `mark_interrupted_runs` 排除都与代码相符。10 项撤权变异（M1～M10）中 8 项被杀死。<br>M7（缩小账户时不取消令牌）只有 `grants` 单元测试能杀死：运行时测试没有时间断言，摘要慢慢完成后才被提交拒绝，测试照样通过。A 已加时限断言。<br>M2、M3（模型请求前、摘要请求前的取消检查）存活。这两处检查守护的是同一任务内一段同步代码的窗口，确定性测试无法单独触发。`DeltaModelClient` 在已取消的令牌上不发请求（`biased` 选择），覆盖了同一窗口，但原来没有测试：原取消测试允许 ≤1 次请求。A 补了测试。<br>B 文档把缩小账户测试写成“同一时刻两个运行”，实际是先后两个运行，A 已在交付证据中更正 | 关闭；R1-A-18 passed |
| F-15 / A-12 | `search_instruments` 转义 `%`、`_`、`\`；`add_watch` 校验标的存在，并新增 `remove_watch`；桌面“标的/自选”页；4 项 GPUI 无头交互测试 | `LIKE … ESCAPE '\'` 与 `like_contains` 转义正确：去掉 `\` 或 `%` 转义的变异都被杀死，`%` 的变异同时被服务测试和桌面测试杀死。4 项无头测试真实点击、输入，读写同一应用服务。A 的 `r1_a_05_account_page_reads_service_lines` 函数体与 RW-02 时字节一致，并且通过。<br>观察（非缺陷）：演示库按进程号建临时目录，退出后不清理 | 关闭；R1-A-12 仍为 partial（只差截图，ACT-02） |
| A-19 / A-24 | `perf.rs` 计时埋点：交互处理与图表绘制的 CPU 时间、关闭请求到事件循环结束的时间，日志写 P95 摘要 | 只记录，没有阈值，也没有宣称达标。GPU 呈现不在测量范围内，验证器理由属实。窗口全部关闭后退出事件循环，这一改动只影响关闭流程 | 保持 partial（ACT-02） |

### A 本轮修复（同一阶段提交）

1. F-23/A-16 回环直连：`proxy_route` 先判断回环，回环端点在任何策略下都直连，`custom` 也不例外。代理自身的回环不是用户的本地服务，明文请求经代理会把令牌交给代理。测试 `r1_a_16_loopback_endpoint_never_goes_through_a_custom_proxy` 在修复前失败（`left: Custom, right: Direct`）。路由表中 `custom` + `http://127.0.0.1:9000` 改为直连，并增加 `custom` + `https://localhost:8443`。[Rust 模型客户端](../design/rust-model-client.md) 已同步。
2. 子进程探针：父测试断言子进程实际运行并通过了 1 项测试。A 临时把过滤名改错，父测试随即失败，确认断言有效（已还原）。
3. `r1_a_18_narrowing_accounts_during_compaction_stops_only_the_affected_run` 加 2.5 秒时限：缩小账户必须中断在途摘要，而不只是摘要完成后拒绝写检查点。加强后 M7 在运行时测试中也失败。
4. 新测试 `r1_a_16_a_cancelled_token_sends_no_request`：已取消的令牌不发出任何请求。去掉客户端连接阶段的取消分支（C1）后，这项测试和原取消测试都失败。
5. 更正 [交付证据](r1-delivery.md) 中缩小账户测试的描述。

### 变异检查

在 B 源码上做 17 项，每项改源码、跑对应测试、还原；还原后文件哈希与改前一致：

| 范围 | 变异 | 结果 |
| --- | --- | --- |
| 撤权 | M1 运行不用租约令牌；M4 `commit` 忽略撤权；M5 不写 `revoked`；M6 重启改写 `revoked`；M8 接纳忽略已撤销连接；M9 接纳忽略账户限制；M10 撤销连接不取消令牌 | 均杀死 |
| 撤权 | M7 缩小账户不取消令牌 | 只被 `grants` 单元测试杀死；A 加强后运行时测试也杀死 |
| 撤权 | M2 模型请求前不检查取消；M3 摘要请求前不检查取消 | 存活，原因见上文；由客户端测试（C1）覆盖同一窗口 |
| 代理/超时 | P1 `Direct` 保留系统代理；P2 `system` 代理明文远端；P3 连接预算放大 100 倍；P4 连接失败不重试；P5 连接超时报成一般失败 | 均杀死 |
| 搜索 | S1 不转义 `\`；S2 不转义 `%` | 均杀死 |

A 修复后补做：P2（按新代码重写）、P6（回环端点经 `custom` 代理）、C1 均被杀死；M7 被运行时测试杀死；M2、M3 仍存活。

### 实际检查

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 现场身份 | `git rev-parse`、`git status`、`source_fingerprint()` | 分支、HEAD `9284ccb…`、父提交 `a1180cf…`、B 指纹 `80f30e79…` 都与交接一致；工作区只有未跟踪的 `workers/agent/` |
| B 结果复现 | B 源码上 `cargo test --workspace` | 157 项通过，0 失败，1 项忽略（子进程探针），与 B 报告一致 |
| 修复前探针 | B 源码加新增回环测试；临时探针记录假代理收到的请求（已删除） | 新测试失败；假代理收到 3 次明文 `POST`，含 `Authorization` 和令牌，端点 0 次 |
| Rust 全量 | `cargo test --workspace` | 159 项通过（B 157 项 + A 新增 2 项），0 失败，1 项忽略 |
| 格式 / 静态检查 | `cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets -- -D warnings` | 退出码 0 |
| Worker / 契约 | 两处 `python -m unittest discover` | 11 项、12 项通过 |
| 文档 / 空白 | `python scripts/check_docs.py`；`git diff --check` | 退出码 0 |
| 验证器 | `python scripts/verify_r1.py` | 退出码 1（诚实输出）：门禁全绿；22 项 passed，3 项 partial（R1-A-12/19/24），实机 2 项与 live 2 项 not-run |
| 用例未被改动 | `git diff c4b0af03 9284ccba -- docs/delivery/r1-cases.json` | 无差异；partial 理由与代码现状一致 |
| A 既有修复回退检查 | `git diff c4b0af03 9284ccba` 的删除行，逐个文件检查 | 未回退。`host.rs` 无改动。`app.rs` 只删除旧的 `search_instruments`。`runtime.rs` 只把检查点写入移进租约提交，并改了工具循环头；“有回执即核对全部数字”和 `REPORT_TERMS` 都在。`app_service.rs`、`runtime_ai.rs` 无删除行。`model_protocol.rs` 只删除 192.0.2.1 测试。`verify_r1.py` 中 R1-A-12 的理由改为只差截图，与现状相符 |
| `results.json` 抽查 | `.local/reports/r1/results.json` | `plan_version` 1.1；`git_base` 为 `9284ccb…` / `a1180cf…`；源码指纹与上表一致；partial 都有理由 |

全部测试都在本机进行，使用临时库、合成账本、本机假端点和本机假代理，没有连接真实账户、行情、模型、代理或外网地址。

### 用例矩阵（RW-03 复核后）

R1-A-01/02/03/04/05/06/07/08/09/10/11/13/14/15/16/17/18/20/21/22/23/25 为 **passed**（22 项）。R1-A-12/19/24 为 **partial**，只差 ACT-02 实机。R1-M-01/02、R1-L-01/02 为 **not-run**。

### 余项（不开 RW-04）

代码返工已清。下面各项只能在实机或真实服务上完成，或者属于后续阶段，在集中台账中跟踪：

| 台账 | 内容 |
| --- | --- |
| ACT-02 | R1-A-12 截图；R1-A-19 窗口关闭 ≤1 秒；R1-A-24 交互 P95 与 GPU 50 FPS；IME/DPI；干净安装 |
| ACT-01 / ACT-03 / Q-01 | 真实模型端点；去标识真实账户与行情 |
| AI-01 | 撤权界面入口；模型连接配置的持久化与管理界面 |

### 需保留的 A 修复（RW-03 复核）

后续修改时保留，不得回退：
- `crates/delta-infra/src/model/mod.rs`：回环端点在任何代理策略下直连。
- `crates/delta-infra/tests/model_proxy.rs`：回环测试、路由表的回环行、子进程实际运行断言。
- `crates/delta-infra/tests/model_protocol.rs`：已取消令牌不发请求。
- `crates/delta-infra/tests/runtime_ai.rs`：缩小账户的时限断言。

RW-01、RW-02 复核列出的保留项继续有效。

### 限制

- 结论只基于离线合成数据、本机假端点和本机假代理，不代表真实模型、真实代理或真实服务通过。
- `system` 策略只验证了环境变量路径，没有伪造 Windows 注册表中的系统代理。
- M2、M3 存活：运行时的这两处检查依赖客户端在已取消令牌上不发请求。换用其他 `ModelClient` 实现时，也要满足同一约定。
- 撤权和超时测试带有时限（2.5 秒，3～12 秒），机器负载极高时可能误报。
- 计时埋点只记录 CPU 侧时间，不是达标证据。
