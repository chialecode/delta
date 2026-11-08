# 集中事项台账

更新：2026-11-09；维护：A 汇总/审核，B 补实现与证据，用户处理真正产品/配置决定。此表为工程侧未决、开源和后续事项的唯一当前入口；用户决定、人工与授权只在 [USER-ACTIONS.md](../../USER-ACTIONS.md)，报告仅引用 ID。

状态：open、investigating、ready-for-decision、resolved、deferred。每项包含影响、推荐、负责人、截止/受阻动作和下一步；已确认项移入确认记录或补关闭依据，不反复询问。

## 用户事项入口

用户事项统一在根 [USER-ACTIONS.md](../../USER-ACTIONS.md) 维护（`Q-01` 已由 D-23 确认关闭、`Q-03` 已由 D-21 确认关闭、`ACT-07` 本机 Rust/MSVC 环境已复核关闭、`Q-02` 已由 D-24 确认 Apache-2.0 关闭；可选样本/Token `ACT-03`、M2 模型连接 `ACT-01` 与人工检查 `H-01`～`H-03` 在用户待办，远端推送需另行授权）。本表只维护 Agent 工程任务。

## 开源风险与适配缺口

| ID / 状态 | 上游与风险 | 当前处置 / 退出条件 | 负责人 / 截止与受阻范围 |
| --- | --- | --- | --- |
| OSS-01 / resolved | GPUI Kit 旧 `0.6.6` 缺少最新面板能力，且内置 `CandlestickChart` 默认仅面向 40 根日线静态卡片展示 | 已由 D-20 / D-21 解决方向问题：① M1 完全解耦 GUI，先完成金融数据底座与账本核心；② M2 升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接使用官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系，主 K 线面板外包视口滑动窗口（`visible_range` 缩放/平移切片）（见 [ADR-0003](0003-first-round-stack.md)）。集成实测风险转 `OSS-09` | A 已完成源码核查并经用户 D-21 确认关闭 |
| OSS-02 / resolved | 旧模型运行时的资源发现/隔离验证 | D-08 撤回旧候选，关闭该候选专属验证；工具白名单/状态验证转入 OSS-08 与有效合同 | A；2026-09-26；不表示旧候选实测通过 |
| OSS-03 / resolved | 原 Python 数据连接器/指标工作进程的离线锁定与跨进程隔离负担 | 已由用户 2026-11-07 **D-22** 解决：全项目彻底采用 100% 纯 Rust 实现，移除 `workers/python` 与运行期 Python 依赖；免 Key 美股行情由 Rust 直连 Yahoo Chart v8 与东财 `push2his` JSON 接口，日历与指标均由纯 Rust 实现 | A 已更新架构与 M1 计划 v2.2 关闭 |
| OSS-04 / resolved | `chrono-tz 0.10.4` / IANA 时区与美股（`XNYS`/`XNAS`）半日市、节假日、夏令时覆盖 | M1 纯 Rust 基于 `chrono` + `chrono-tz`（`America/New_York`）实现 NYSE/NASDAQ 法定节假日与半日市规则（1993 年前 14:00 ET、1993 年起 13:00 ET）+ 仓库内置版本化特殊闭市表 `calendar_special_xnys.json`，写入 SQLite `calendar_session` 并校验加密 `24×7` UTC 切分 | B 已在 M1-W02 完成实现并通过 `POC-D02` 实测：`1990-01-02`～`2027-12-31` 全量 13,878 个自然日（9,569 个交易日、80 个提前收盘日）与 `exchange_calendars` `xnys.csv` 及 NYSE 2023～2027 官方表逐日核对 `0` 差异，越界年份返回 `calendar_unknown`（见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)） |
| OSS-05 / resolved | 必做公共数据源的字段、复权、限流、版本与 Decimal 边界 | M1 原生 REST/ZIP 完成真实编排、核验和缺失诊断；实际结果只采用本轮阶段报告 | B；官方 ccxt =4.5.91 实测构建成功，默认 Windows 栈溢出，64MiB worker 后 OKX/Kraken 成功；typed f64 边界使通用 adapter 转 LOOP-03，见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md) |
| OSS-06 / resolved | 锁文件传递依赖许可与数据来源条款 | 基于 Cargo.lock 的 metadata 清单重新核验；数量、表达式与选择的宽松许可分支见阶段报告 | B；不把代码许可当作行情再分发权；封存版本/导出使用 public_open / official_free_personal / unofficial_personal |
| OSS-07 / open | 兼容端点工具/SSE/用量能力差异 | M2 已执行受控回环全套验证：`delta-model` 对 `openai-responses` 与 `openai-chat-completions` 独立通过 9 类回环用例（`M2-K03` / `M2-A-13`）；真实在线模型服务只对用户 `ACT-01` 配置的协议/模型声明（见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)） | A+B；缺 `ACT-01` 时真实服务保持 not-run，不阻塞工程收敛 |
| OSS-08 / resolved | 自研 Rust 模型客户端需持续维护双协议/SSE/工具配对、取消和上下文恢复 | 本轮修复 wire 名称、并行请求成组历史、reasoning、SSE/累计预算、显式证据/数字、连接与 journal 指纹和跨库 keyring；锁定 `reqwest 0.13.5` + `eventsource-stream 0.2.3`，M2-K03/A13～16 回环和权限/恢复回归通过 | B；2026-11-08 按 [阶段报告](../evidence/2026-11-09-m2-stage-report.md) 关闭 M2 工程适配，真实兼容端点仍归 OSS-07 |
| OSS-09 / resolved | M2 GPUI 集成风险：`gpui-kit 0.9.0` 精确锁定 `gpui-pre =0.3.11`，升级伴随快照破坏；宿主后台边界、Windows 栈和真实性能 | 实际六页/表单/图表交互、隐藏原生 HWND 与 8 MiB 栈实跑；最终静态 CRT release 包完成 100k 图表/表格各 300 实测帧、5 次首屏和 300 次交互，p95 均满足门槛，原始分位数与限制见阶段报告 | B；2026-11-08 M2-K01/K02/K06 与 M2-A-20 通过关闭；后续升级重新验证锁定快照 |
| OSS-10 / deferred | Parquet/DuckDB 行情仓 | M1–M2 只用 SQLite 单库，`dataset_partition.location_kind` 预留扩展；引入时保持上层查询契约并扩展一致性备份清单 | A+B；触发条件：M5 批量扫描需求，或 POC-D01/实测查询性能不达标 |
| OSS-11 / resolved | M2 MPL-2.0 与多许可分发；原收集漏掉 AND 义务且只核声明不足以支持分发 | 本轮收集 885/885、0 errors；完整保留 4 种 AND 义务、原始 notices/哈希；48 包明确标准正文＋完整锁定源码回退，SPDX 数据固定 commit，见 [逐包清单](../evidence/2026-11-08-dependency-licenses.md) | B；2026-11-08 M2-G-06、3 项收集回归与真实两通道 ZIP 全文件哈希/随附正文归档验证通过关闭；MPL 包零源码修改 |
| OSS-12 / resolved | Inno Setup 为外部构建工具，不在 Cargo 锁定范围内；版本漂移会改变安装器行为 | B；2026-11-09 M3-W02 关闭 | 锁定 7.1.1（winget `JRSoftware.InnoSetup.7`，按用户范围）。`ISCC.exe` 无可读版本资源，改由 `.iss` 内 ISPP `Ver` 检查拒绝 7.1.x 以外的编译器，门禁核对已安装键 `Inno Setup: Setup Version` 以 `7.1.` 开头；安装包与 `ISCC.exe` 哈希见 [技术选型](../engineering/technology-selection.md)；缺失时 `package-build` / `--installer` 明确失败（`INNO_SETUP_MISSING`），不回退 ZIP；安装方式写入 README |

## 后续循环

| ID / 状态 | 内容 | 负责人 / 触发条件 | 当前边界 |
| --- | --- | --- | --- |
| LOOP-01 / deferred | 在 M1 数据底座、M2 资产复盘应用与 M3 安装更新贯通后，推进 M4 K 线训练与 M5 策略研究 | A；M3 完成后按用户指示启动 | M1 数据版本、`dataset_partition` 存储抽象与回放截止时间（`visible_until`）已为 M4/M5 预留原生接口 |
| LOOP-02 / deferred | MCP 对外适配、嵌入检索、多模型路由/长期记忆 | A；有真实第二消费者或检索不足证据时 | 统一能力接口可扩展，不作为 M1/M2 依赖 |
| LOOP-03 / deferred | 扩展通用 ccxt 无损 adapter、Alpaca bars、Longport SDK、平台原生 CSV 差异与真实只读账户 API | A+B；通用 adapter 在第二真实消费者/新交易所需求触发；平台样本或私有 SDK/API 在 ACT-03 实际可用且有数据使用授权后触发 | M1 必做模板/资金语义/原生公共源已补齐；扩展平台当前只验证共享规范化模板，真实原生覆盖未完成；私有服务、真实个人账单 not-run。不得以本项延期当前金额/隔离/恢复缺陷 |
| LOOP-04 / deferred | 每周 CI 自动构建并发布正式版（标签、Release 资产与签名） | A+B；仓库 Actions 白名单放开所需 action 且用户授权远端设置变更与签名私钥进入 CI 机密后 | M3 只做本地 `release-build` 与经 `ACT-11` 单次授权的手动发布；签名私钥不进入 CI 前不做自动发布 |
| LOOP-05 / deferred | 共享 Rust 构建缓存 `mbx`（[jdx/mr-boxington](https://github.com/jdx/mr-boxington)，MIT，2026-11-09 查证：最新 `v1.29.2` 发布于 2026-11-03，提供 Windows x64 发行包）：包装 cargo，按编译等价键从共享仓库恢复编译产物，跨 git worktree 共用，并按磁盘预算回收 `target/`；另有 GitHub Actions 缓存 action | A；触发条件：开始用多个 git worktree 并行开发（每个工作树各自约 3.5 GB 以上 `target/`），或 CI 加入 Rust 构建 | 当前单一检出不采用：主要膨胀来源（依赖调试信息、增量缓存、门禁重复包）已由 dev profile 与 `scripts/clean_artifacts.py` 处理（见[构建产物](../dev-rules/local-files-and-references.md#build-artifacts)）；`target/` 与打包用 `.local/target-packages/` 的 RUSTFLAGS/target 不同，缓存键本就不能共用；Windows 上恢复是复制字节而非链接，单检出时反而多一份存储；`mbx setup` 会改动全局 cargo 调用，引入前须先验证门禁静音（D-25）与源码指纹/`--locked` 行为，并经用户同意改动其全局开发环境。CI 侧还受 Actions 白名单限制（同 LOOP-04） |
| LEDGER-01 / resolved | 买入稳定币/法币（如 USD→USDT 入金兑换）在 M1 形成持仓批次，之后不能作为报价腿结算；应建模为现金换汇，保留兑换成本与汇兑收益 | M2-W02 已在 `delta-core` 与 `delta-data::importers` 实现 `EconomicEventType::CashExchange`（`cash_exchange`）：源币种按现金 FIFO 处置并记录已实现汇兑损益，目标币种（含 USDT/USDC）进入现金 FIFO 账本并保留兑换手续费成本，后续买入加密资产（如 `USD → USDT → BTC`）只从报价现金结算一次 | B 已在 M2-W02 完成实现并通过 `M2-A-06` 与 M1 `M1-A-06` 回归关闭 |
| TEST-01 / resolved | M2 测试 `test_m2_a14_…` 在全量并行中偶发 `CREDENTIAL_UNAVAILABLE`（Windows 凭据管理器瞬时失败，约 1/40） | A；2026-11-09 按 D-32 去掉凭据管理器、测试凭据改为临时数据目录里的加密文件后关闭 | 失败来源已不存在；若文件凭据再出现并发失败另立条目，见 [M3 阶段报告](../delivery/stage-report.md#a-集中技术审核2026-11-09) |
| TEST-02 / deferred | M1 门禁锁探针 `M1-A-15` 要求子进程在 2 s 内启动并持锁。M3 最终序列的第一次 M1 回归中出现一次 `lock_probe: holder did not acquire or exit cleanly`（刚重新构建的 debug 可执行文件冷启动），同指纹立即重跑通过（持锁 824 ms） | B；再次出现，或下一阶段修改 M1 门禁时处理：等待条件改为以持锁事件为准并放宽冷启动上限，不放宽锁语义本身 | 锁行为本身由同指纹重跑与单元测试覆盖；门禁失败仍如实记录，见 [M3 阶段报告](../delivery/stage-report.md) |
| TEST-03 / resolved | M2-A-20 / M2-K02 原生帧性能在 M3 自检中未达标（图表 p95 34～47 ms、表格 22～26 ms，归档 M2 包同样不达标） | A；2026-11-09 定位为 Windows 11 对隐藏窗口进程的执行速度节流（EcoQoS）：同一二进制退出节流后图表 p95 8.6 ms、表格 5.9 ms，提高优先级无效。测量进程改为退出该节流并在报告记录 `power_throttling`，门禁只接受退出节流的样本，门槛不变；最终指纹 M2 回归图表 p95 9.6 ms、表格 6.9 ms | 已关闭；见 [质量门禁](../dev-rules/quality-gates.md#foreground-quiet) 与 [M3 阶段报告](../delivery/stage-report.md#a-集中技术审核2026-11-09) |
| DOC-01 / resolved | 部分 M4+/AI 正文（`ai-design.md`、`interaction-and-workflows.md`、`financial-engine.md` 模拟章节、需求 P1/P2 行等）曾使用重建前的 `S0`–`S3`/`R1` 阶段名 | A；2026-11-07 已统一为 `M1`–`M6`，并在 `scripts/check_docs.py` 增加旧阶段标签门禁防止回流 | 已完成，全仓 `active`/`draft` 文档无旧 `S0`–`S4` 或 `R1`–`R2` 阶段名 |

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
