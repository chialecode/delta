# 集中事项台账

更新：2026-11-08；维护：A 汇总/审核，B 补实现与证据，用户处理真正产品/配置决定。此表为工程侧未决、开源和后续事项的唯一当前入口；用户决定、人工与授权只在 [USER-ACTIONS.md](../../USER-ACTIONS.md)，报告仅引用 ID。

状态：open、investigating、ready-for-decision、resolved、deferred。每项包含影响、推荐、负责人、截止/受阻动作和下一步；已确认项移入确认记录或补关闭依据，不反复询问。

## 用户事项入口

用户事项统一在根 [USER-ACTIONS.md](../../USER-ACTIONS.md) 维护（`Q-01` 已由 D-23 确认关闭、`Q-03` 已由 D-21 确认关闭、`ACT-07` 本机 Rust/MSVC 环境已复核关闭、`Q-02` 已由 D-24 确认 Apache-2.0 关闭；可选样本/Token `ACT-03` 仍在用户待办，远端推送需另行授权）。本表只维护 Agent 工程任务。

## 开源风险与适配缺口

| ID / 状态 | 上游与风险 | 当前处置 / 退出条件 | 负责人 / 截止与受阻范围 |
| --- | --- | --- | --- |
| OSS-01 / resolved | GPUI Kit 旧 `0.6.6` 缺少最新面板能力，且内置 `CandlestickChart` 默认仅面向 40 根日线静态卡片展示 | 已由 D-20 / D-21 解决方向问题：① M1 完全解耦 GUI，先完成金融数据底座与账本核心；② M2 升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接使用官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系，主 K 线面板外包视口滑动窗口（`visible_range` 缩放/平移切片）（见 [ADR-0003](0003-first-round-stack.md)）。集成实测风险转 `OSS-09` | A 已完成源码核查并经用户 D-21 确认关闭 |
| OSS-02 / resolved | 旧模型运行时的资源发现/隔离验证 | D-08 撤回旧候选，关闭该候选专属验证；工具白名单/状态验证转入 OSS-08 与有效合同 | A；2026-09-26；不表示旧候选实测通过 |
| OSS-03 / resolved | 原 Python 数据连接器/指标工作进程的离线锁定与跨进程隔离负担 | 已由用户 2026-11-07 **D-22** 解决：全项目彻底采用 100% 纯 Rust 实现，移除 `workers/python` 与运行期 Python 依赖；免 Key 美股行情由 Rust 直连 Yahoo Chart v8 与东财 `push2his` JSON 接口，日历与指标均由纯 Rust 实现 | A 已更新架构与 M1 计划 v2.2 关闭 |
| OSS-04 / resolved | `chrono-tz 0.10.4` / IANA 时区与美股（`XNYS`/`XNAS`）半日市、节假日、夏令时覆盖 | M1 纯 Rust 基于 `chrono` + `chrono-tz`（`America/New_York`）实现 NYSE/NASDAQ 法定节假日与半日市规则（1993 年前 14:00 ET、1993 年起 13:00 ET）+ 仓库内置版本化特殊闭市表 `calendar_special_xnys.json`，写入 SQLite `calendar_session` 并校验加密 `24×7` UTC 切分 | B 已在 M1-W02 完成实现并通过 `POC-D02` 实测：`1990-01-02`～`2027-12-31` 全量 13,878 个自然日（9,569 个交易日、80 个提前收盘日）与 `exchange_calendars` `xnys.csv` 及 NYSE 2023～2027 官方表逐日核对 `0` 差异，越界年份返回 `calendar_unknown`（见 [M1 阶段报告](../delivery/stage-report.md)） |
| OSS-05 / resolved | 必做公共数据源的字段、复权、限流、版本与 Decimal 边界 | M1 原生 REST/ZIP 完成真实编排、核验和缺失诊断；实际结果只采用本轮阶段报告 | B；官方 ccxt =4.5.91 实测构建成功，默认 Windows 栈溢出，64MiB worker 后 OKX/Kraken 成功；typed f64 边界使通用 adapter 转 LOOP-03，见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md) |
| OSS-06 / resolved | 锁文件传递依赖许可与数据来源条款 | 基于 Cargo.lock 的 metadata 清单重新核验；数量、表达式与选择的宽松许可分支见阶段报告 | B；不把代码许可当作行情再分发权；封存版本/导出使用 public_open / official_free_personal / unofficial_personal |
| OSS-07 / deferred | 兼容端点工具/SSE/用量能力差异 | M1 聚焦金融数据底座；模型双协议适配移至 M2 应用整合阶段验证 | A+B；M2 阶段执行，不阻塞 M1 数据底座 |
| OSS-08 / deferred | 自研 Rust 模型客户端需持续维护双协议/SSE/工具配对、取消和上下文恢复 | 保持 D-08 与 [模型客户端设计](../design/rust-model-client.md) 契约不变，在 M2 阶段接入 M1 已验证的数据与账本服务 | A+B；M2 阶段执行 |
| OSS-09 / deferred | M2 GPUI 集成风险（D-21 已确认方向，风险仍需实测）：`gpui-kit 0.9.0` 精确锁定 Zed 快照 `gpui-pre =0.3.11`（0.8.1 为 `=0.3.10`），升级伴随快照破坏；GPUI 执行器与 `tokio` 并存；前序构建在 Windows MSVC 曾需加大主线程栈；桌面层传递依赖规模大（许可盘点成本） | M1 已通过 `delta-app` 同步命令/作业句柄边界规避运行时耦合（[M1 计划 S-02](../delivery/stage-plan.md#3-a-已定案的阶段技术决定b-不再重新选型)）；M2 计划首个 spike 在 Windows 11 实际构建 `gpui-kit` 最小 Dock 工作台，记录构建时长、栈需求、依赖与许可清单 | A；M2 计划开始时触发，不阻塞 M1 |
| OSS-10 / deferred | Parquet/DuckDB 行情仓 | M1 只用 SQLite 单库，`dataset_partition.location_kind` 预留扩展；引入时保持上层查询契约并扩展一致性备份清单 | A+B；触发条件：M4 批量扫描需求，或 POC-D01/实测查询性能不达标 |

## 后续循环

| ID / 状态 | 内容 | 负责人 / 触发条件 | 当前边界 |
| --- | --- | --- | --- |
| LOOP-01 / deferred | 在 M1 数据底座与 M2 资产复盘应用贯通后，推进 M3 K 线训练与 M4 策略研究 | A；M2 完成后按用户指示启动 | M1 数据版本、`dataset_partition` 存储抽象与回放截止时间（`visible_until`）已为 M3/M4 预留原生接口 |
| LOOP-02 / deferred | MCP 对外适配、嵌入检索、多模型路由/长期记忆 | A；有真实第二消费者或检索不足证据时 | 统一能力接口可扩展，不作为 M1/M2 依赖 |
| LOOP-03 / deferred | 扩展通用 ccxt 无损 adapter、Alpaca bars、Longport SDK、平台原生 CSV 差异与真实只读账户 API | A+B；通用 adapter 在第二真实消费者/新交易所需求触发；平台样本或私有 SDK/API 在 ACT-03 实际可用且有数据使用授权后触发 | M1 必做模板/资金语义/原生公共源已补齐；扩展平台当前只验证共享规范化模板，真实原生覆盖未完成；私有服务、真实个人账单 not-run。不得以本项延期当前金额/隔离/恢复缺陷 |
| LEDGER-01 / open | 买入稳定币/法币（如 USD→USDT 入金兑换）当前形成持仓批次，之后不能作为报价腿结算（A-03 修正后显式拒绝，不再双重扣减）；应建模为现金换汇，保留兑换成本与汇兑收益 | A+B；触发：`ACT-03` 样本或用户流程出现入金兑换，或 M2 账本扩展前 | 当前不产生错误金额（拒绝并提示）；不承接 A-01～A-05 已修复的缺陷 |
| DOC-01 / resolved | 部分 M3+/AI 正文（`ai-design.md`、`interaction-and-workflows.md`、`financial-engine.md` 模拟章节、需求 P1/P2 行等）曾使用重建前的 `S0`–`S3`/`R1` 阶段名 | A；2026-11-07 已统一为 `M1`–`M5`，并在 `scripts/check_docs.py` 增加旧阶段标签门禁防止回流 | 已完成，全仓 `active`/`draft` 文档无旧 `S0`–`S4` 或 `R1`–`R2` 阶段名 |

### LOOP-03 分项接续

M1 计划第 3.1 / 6 节允许未完成扩展项登记后收敛；本项不承接当前必做金额、数据隔离或恢复缺陷。负责人均为 A+B。

| 分项 | 实际未完成部分 / 原因 | 接续触发 |
| --- | --- | --- |
| 通用 ccxt adapter | 官方 crate 的 typed OHLCV 为 f64，Windows 默认栈溢出；无损 Decimal adapter 和有界资源接入尚未实现，当前使用原生 REST | 第二真实消费者或新交易所需求；保留已实测 4.5.91 POC，重新验证具体兼容版本 |
| Alpaca bars | 当前仅公司行动离线 parser / 回放，`/v2/stocks/bars` 连接器未实现；属于扩展范围 | ACT-03 对应配置与数据使用授权实际可用，或 A 在后续阶段指定离线扩展任务 |
| Longport SDK | 计划中的 SDK 尚未接入产品，版本兼容与私有会话未测；当前必做公共源与 CSV 不依赖它 | ACT-03 对应配置/授权实际可用后，在后续阶段锁定兼容版本并验证 |
| 平台原生 CSV 差异 | 目前验证已声明的规范化模板与资金语义；没有真实个人原生账单样本，不宣称覆盖所有导出格式 | ACT-03 对应去标识样本实际可用且允许使用后，补平台差异回归 |
| 真实只读账户 API | 当前为 OKX/Binance 原生结构的合成回放实现，未接真实私有会话；M1 必做范围明确不要求真实 API 在线同步 | 实际只读配置与相应数据使用授权齐备后，先验证权限与范围，再接在线同步 |

新增项使用稳定 ID，不新建另一张散落的审批列表。需要用户选择时提供推荐/备选、具体证据、批准后动作和阻塞范围；有可逆替代继续独立工作。开发缺口不伪装成用户任务。
