# M2：资产复盘桌面工作台、证据化 AI 复盘与 Windows 打包实施计划

版本 **1.0** · 2026-11-08 · 编制：Agent A · 执行：Agent B · 当前状态见 [执行状态](status.md)。本计划在 M1 v2.6（PR #1 集成为 `main` `0370b7e`，证据见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)）之上，按 [D-20 ～ D-29](../decisions/confirmed-decisions.md) 交付 M2。计划开始前 A 已完成 `OSS-09` 首个 spike：`gpui-kit 0.9.0` 本机构建通过、官方无头 Dock UI 测试 0 可见窗口且不抢焦点，官方没有 `Card` 组件（见 [spike 证据](../evidence/2026-11-08-m2-gpui-kit-spike.md)）。

## 1. 目标

1. **同一服务，两种消费者**：在 M1 `delta-app` 之上补齐带范围的查询契约（`AnalysisScope` + `ResultEnvelope` + 能力目录），桌面 UI 与 AI 只读工具调用同一函数，在同一范围与版本下得到一致的数值、质量和证据（AC-23）。
2. **桌面工作台（D-21）**：新建 `delta-desktop`，以 `gpui-kit 0.9.0` 官方 `DockArea` + `DockSkin` + `DockLayout` + `Panel` + `DataTable` 构建总览、资产、市场（K 线视口切片）、日志、数据中心、设置与右侧 AI 面板、底部任务面板；未交付的训练/策略不显示入口。
3. **完整资产复盘**：账户与统计组合配置、带范围与期间的损益分解、手工估值、余额核对、更正后报告过期提示，以及 `LEDGER-01` 现金换汇建模。
4. **交易日志**：Markdown 笔记、结构化模板、与成交/计划/图表上下文的双向关联、附件、搜索与诚实的自动保存。
5. **证据化 AI 复盘**：自研 Rust `ModelClient`（Responses / Chat Completions），宿主冻结范围、只读白名单工具、证据校验、会话与压缩检查点；未配置模型时全部本地功能可用。
6. **可交付**：备份/恢复/导出/删除覆盖笔记与附件，Windows 按用户安装包（正式版/开发版共存），依赖许可与通知齐全，性能按固定规模实测。

## 2. 开始基线、分支与 Git 规则

- 阶段父提交：`main` `0370b7e9103d0172a7ab47613ea253949d6a9b8b`（M1 squash 合并，父 `8329f63`）。当前分支 `codex/m2-desktop-workbench`；本计划所在提交是父提交之后的唯一阶段提交，B 实现、A 审核修正与返工在 push 前直接 `git commit --amend -s` 进该提交（D-09、D-27），完整 SHA 以交接消息为准。
- 工作区保护：未跟踪 `workers/agent/` 保持原样，不读取、不构建、不暂存。`.local/` 忽略目录下的 M1 备份、报告与本次 spike 保留。
- 工具链：沿用根 `rust-toolchain.toml` 精确 `1.99.1`；升级工具链按普通依赖变更同步本节。
- 远端：本阶段 push / PR / 合并另需用户在 [USER-ACTIONS](../../USER-ACTIONS.md) 授权具体动作；CI 只能使用仓库 Actions 白名单（`actions/checkout`、`actions/setup-node`，见 [Git 规则](../dev-rules/git-workflow.md#github-hygiene)）。
- 本机环境：Windows 11 + Visual Studio Build Tools 2026 + MSVC（`ACT-07` 已复核）；美国网络出口经 TUN 代理，公共端点基址保持可配置（沿用 M1 S-03）。

## 3. A 已定案的阶段技术决定（B 不再重新选型）

M1 的 S-01～S-11 继续有效（SQLite 单库、同步命令 + 作业句柄边界、纯 Rust 连接器、keyring、数据条款类别、日历、FX 时点、原始价口径、版本区间、前台无干扰、安装与数据位置）。M2 新增：

| ID | 决定 | 理由 / 退出条件 |
| --- | --- | --- |
| S-12 crate 布局 | 新增 `delta-desktop`（二进制 `delta`，只含视图、交互状态与 GPUI 适配）与 `delta-model`（ModelClient、两协议适配、SSE、HTTP 传输，不访问业务库）；AI 运行时、工具网关与会话仓储放 `delta-app::ai`，会话表放 SQLite 迁移。`delta-desktop` 只依赖 `delta-app`（及其公开 DTO），**不得直接依赖 `delta-data`、`rusqlite`、`tokio`、`reqwest`**；`AppService::store()` / `store_mut()` 收为 crate 内部，CLI 改用服务查询 | UI 不拥有 SQL、金融公式或指标（[DESIGN](../design-rules/DESIGN.md)）；依赖边界由 `cargo metadata` 检查固化，不靠约定 |
| S-13 查询契约 | `delta-app` 提供有类型的查询/命令：`AnalysisScope`（组合或账户集、期间、报表币种、`dataset_version`、`ledger_revision`、`visible_until`）→ `ResultEnvelope<T>`（数据、质量状态 `complete/partial/unavailable` 与缺口、证据引用 `EvidenceRef{kind,id,revision}`、版本、计算版本）。能力目录列出每个能力的 ID、schema 版本、读写类别与范围要求；AI 工具白名单是目录的只读子集，参数 schema 由同一类型生成 | 满足 [统一能力接口](../design/integration-contracts.md) 与 AC-23；避免 UI 与 AI 各写一套查询 |
| S-14 宿主线程模型 | `AppHost`：一个所有者线程持有 `AppService`（单写者、持库锁），UI 通过标准库通道发送请求、在 GPUI 后台执行器中等待结果，**UI 线程永不阻塞在 SQLite 或网络上**；长任务沿用 `JobController`，进度以 ≤10 Hz 推送；每个视图请求带 generation，迟到结果按 generation 丢弃。只读查询如实测 P95 不达标，可增加绑定 `dataset_version` 的 WAL 只读连接（B 按实测决定并记录） | GPUI 自带执行器、`AppService` 内含非 `Sync` 的 SQLite 连接；保持 M1 S-02 不暴露 Tokio 的边界 |
| S-15 工作台与卡片 | 按官方 `dock` 示例：`DockSkin::dock_area` + `DockLayout`，左侧导航（总览、市场、资产、日志、数据中心；设置在底部），中部工作区，右侧 AI 面板，底部任务面板；布局以带版本号的 JSON 存于应用数据根 `settings/layout.json`，版本不符或损坏时回到默认布局并提示。**`gpui-kit 0.9.0` 没有官方 `Card`**：按官方 story `ChartCard` 模式自建 `MetricCard`（标题、主值、币种、质量徽标、期间、说明、跳转）；表格统一用 `DataTable` + `TableDelegate` 虚拟滚动 | D-21 方向不变，`Card` 落地方式按上游实际 API 修正；布局恢复不能把损坏状态带进下次启动 |
| S-16 K 线视口 | `KlineViewport` 为纯 Rust、无 GPUI 依赖的状态机（`visible_range`、缩放、平移、跳到日期、十字光标、命中测试、标记布局），全部单元测试；渲染首选 **自绘 GPUI 元素**（`canvas` / 路径批量绘制主图、成交量与指标副图、成交/笔记标记）；`CandlestickChart` 仅当 spike M2-K02 证明切片后满足性能与标记需求时可用于主图，否则只用于小卡片。数据按窗口分页从 `AppHost` 读取 Decimal DTO，仅在绘制时转 `f64`；指标由 `delta-core` 从固定数据起点计算，不随窗口重算预热 | `CandlestickChart` 无视口、光标与标记层（spike 证据）；AC-11 要求不同窗口同日指标一致；NFR-04 |
| S-17 笔记与附件存储 | schema 4 迁移（迁移前自动一致性备份）：`note`（当前版本指针、模型访问开关）、`note_revision`（只追加）、`note_link`（成交事件/计划/标的/图表上下文，记录被链接对象修订号）、`chart_context`（标的/场所/周期/时间窗口/复权/指标参数/可选固定 `dataset_version`）、`attachment`（`attachments/` 下 SHA-256 内容寻址）、`tag` / `note_tag`、`trade_plan`。搜索用 SQLite FTS5 **trigram** 分词（中文 3 字以上子串），1～2 字查询回退为有界 `LIKE`；spike M2-K05 确认 `rusqlite` bundled 的 FTS5 可用 | 笔记与账本同库，使 AC-09 备份恢复与 AC-20 删除范围原子一致；FTS5 默认分词不切中文 |
| S-18 自动保存 | 编辑缓冲 → 停止输入 1 s 或失焦保存为新 `note_revision`；状态严格为 `编辑中 / 正在保存 / 已保存（时间） / 保存失败可重试`；写失败保留缓冲、不显示已保存；崩溃后恢复到最近持久化修订 | FR-JRN-01、DESIGN 状态要求 |
| S-19 ModelClient | 按 [Rust 模型客户端](../design/rust-model-client.md)：`reqwest 0.13`（沿用 M1 锁定）+ `eventsource-stream 0.2.3`（Codex 当前同版本；2022 年后无新版但被广泛使用，B 若发现与 reqwest 0.13 流不兼容可改用活跃的 `sse-stream 0.3.x` 并记录）；显式 `openai-responses` / `openai-chat-completions`，未知协议拒绝，不自动切换；默认上限：单条 SSE 记录 1 MiB、单 run 12 次工具调用、120 s、工具明细 100 行；401/403 不重试，429/5xx 在无可见输出前最多重试 2 次 | 已有设计正本；两协议都必须通过受控回环服务的独立全套用例（AC-25） |
| S-20 AI 运行时与工具 | `AgentRuntime` 冻结 scope 与数据/账本版本；只读工具首批：`scope.describe`、`report.period_breakdown`、`ledger.list_transactions`、`ledger.holdings_lots`、`reconcile.list_issues`、`data.quality_summary`、`journal.search_notes`（只返回允许模型访问的笔记）；最终报告为结构化“摘要 / 计算结果 / 证据 / 局限”，每个引用由宿主解析为范围内真实对象，失效或越权引用拦截；**正文中的数值必须能在本次工具回执中找到（规范化格式后匹配）**，否则该报告标为 `needs_review` 并高亮，不显示为已验证 | AC-10 / AC-19 / FR-AI-02/04：模型不得生成或改写金融数值 |
| S-21 会话与压缩 | 按 [上下文状态](../design/context-state-management.md)：SQLite 保存 run（含 generation）、转录项、工具回执、检查点与压缩来源；压缩在单事务内提交，失败保留原检查点并提示；换账户/换模型/换数据版本即新 run；系统约束、范围与引用由宿主重建，不信任摘要 | AC-24 |
| S-22 测试与前台无干扰 | 视图逻辑测试统一用 `gpui-kit` `test-support` 无头窗口（spike 实测 0 可见窗口、不抢焦点）；`KlineViewport`、查询、AI 运行时用普通单元/集成测试；真实窗口冒烟与性能基准只在显式 `verify-stage --ui-smoke` / `--ui-bench` 下运行：窗口不激活、不置前，哨兵记录前台窗口是否被切走与可见窗口数，结束由 Job 收口；回环模型服务只绑定 `127.0.0.1:0` | D-25；[质量门禁](../dev-rules/quality-gates.md#foreground-quiet) |
| S-23 打包 | 首选 `cargo-packager 0.11.x`（Apache-2.0 OR MIT，活跃）生成 NSIS **当前用户** 安装包：正式版装到 `%LOCALAPPDATA%\Programs\delta`、开发版 `delta-dev`，无需管理员，两通道可并存，卸载默认保留应用数据；构建期 `DELTA_CHANNEL=release` / `dev`。NSIS 只是构建期工具，运行期仍为单一 Rust 二进制（D-22 约束运行期）。若安装路径或并存不满足，备选 `cargo-wix` 按用户 MSI，或便携 zip + Rust 自写安装器；M2 不做代码签名（SmartScreen 提示在说明中如实写出） | D-26；AC-27 |
| S-24 本地化与可访问性 | 中文优先，所有界面文案经单一文案目录（键值）输出，为后续国际化留接口；涨跌配色“红涨绿跌 / 绿涨红跌”可切换且始终配符号/文字；键盘可达导航、表格、图表控制与对话框，Esc 行为一致；中文输入法组合态不提交 | NFR-09、DESIGN |

### 3.1 M2 范围内的需求与不做项

- 范围：[追踪表](traceability.json) 中 `stage = M2` 的 42 项 FR/NFR（含 M1 已完成子集的 UI/AI 补齐），验收 AC-08、AC-09（含笔记/附件）、AC-10、AC-19～AC-27，并在 UI 路径上回归 AC-01～AC-07、AC-22。
- 不做（属后续阶段，不显示入口）：训练（M3）、策略/扫描（M4）、多周期与绘图工具（FR-MKT-05，P1）、标签复盘统计（FR-JRN-05，P1）、TWR/XIRR 基准与币种影响完整分析（FR-LED-07，P1，现有日频近似 TWR/XIRR 可在条件满足时显示并标明方法）、只读账户 API 在线同步（FR-DATA-04，P1，`LOOP-03`）、AI 草稿接受（FR-AI-05，P1）。

## 4. 技术门槛（Spikes）与继续条件

| ID | 最小实际验证 | 通过后直接继续 | 失败或异常分支 |
| --- | --- | --- | --- |
| M2-K01 宿主接入 | `delta-desktop` 无头测试：启动 `AppHost`（临时 dev 根 + M1 AC-02 夹具库），总览 `MetricCard` 显示现金 1438、NAV 2068、总损益 68；`cargo metadata` 检查桌面 crate 无 `delta-data`/`rusqlite`/`tokio`/`reqwest` 直接依赖 | W03～W07 全部基于 `AppHost` | 若 GPUI 执行器与通道等待冲突，改为 `AppHost` 回调 + `cx.spawn` 轮询；不得把 Tokio 运行时暴露到视图层 |
| M2-K02 K 线性能 | release 构建、合成 100,000 根日线、1,000 根可见、脚本化缩放/平移 300 帧，记录帧时间分布（`--ui-bench`） | 自绘元素作为主图 | p95 帧时间 > 20 ms 时先做 LOD（按像素聚合 OHLC）与路径批量化；仍不达标记 `OSS-12` 并在报告写实测值，不宣称 NFR-04 达标 |
| M2-K03 SSE 与两协议 | 回环服务按官方线格式回放：跨字节 UTF-8、多行 data、分片工具参数、`[DONE]` 与 `response.completed`、截断/断流 | W07 继续 | `eventsource-stream` 不兼容时换 `sse-stream 0.3.x`；两者都不行再自写有界解析器并补同组用例 |
| M2-K04 打包路径 | `cargo-packager` NSIS currentUser 实际安装到 `%LOCALAPPDATA%\Programs\delta-dev`，卸载保留数据（安装在开发机执行需窗口时只作显式手动步骤） | W08 继续 | 按 S-23 备选顺序切换并记录 |
| M2-K05 笔记搜索 | 检查 bundled SQLite 的 FTS5 与 `trigram` 分词；中文 1/2/3+ 字查询与英文查询结果 | S-17 实现 | 无 trigram 时 FTS5 `unicode61` + 有界 `LIKE` 回退，记录限制 |
| M2-K06 主线程栈 | 完整工作台 dev 构建真实窗口启动（`--ui-smoke`）不溢出 | — | 仅对 `delta` 二进制设置链接器栈大小（如 8 MiB）并记录原因 |

## 5. 工作包、依赖与验收

依赖顺序：**W01 → W02 / W03 并行 → W04、W05、W06 → W07 → W08**。先打通最薄端到端切片：W01 查询契约 + W03 骨架 + W04 总览卡片显示 AC-02 结果（即 M2-K01），再扩展其余视图。

| 工作包 | 产出 | 正常路径验收 | 失败路径验收 | AC |
| --- | --- | --- | --- | --- |
| M2-W01 应用宿主、查询契约与 schema 4 | `AnalysisScope`、`ResultEnvelope`、`EvidenceRef`、能力目录；`AppHost`；收拢 `store()`；schema 4 迁移（S-17 笔记表、S-21 会话表、自选组、手工估值、`trade_plan`）与迁移前自动备份；计算版本按实际公式变化升级（如 `m2-ledger-1`） | 同 scope/版本两次查询结果逐字段一致；迁移幂等；迁移前备份可恢复 | 未知 scope/版本、陈旧 generation 拒绝；迁移中断无半写库并可从备份恢复 | AC-09、AC-23 |
| M2-W02 账本与报告扩展 | 账户（银行/券商/交易所/钱包/手工）与统计组合 CRUD，组合内去重；按 scope 与期间的损益分解（期初净资产、净外部资金流、投资损益、期末净资产；已实现、未实现、分红利息、费用、汇率影响、未归因项及原因）；基础回撤；手工资产/负债估值（估值日期与方式）；人工余额快照对账；更正后旧报告标过期；`LEDGER-01` 现金换汇（USD→USDT 等保留兑换成本与汇兑损益，之后可作报价腿） | AC-02/04/07 在账户、组合、期间范围下与手工期望一致；组合内同账户不重复计入；USD→USDT→BTC 链路金额守恒 | 缺价/缺汇率/成本未知显示“已知部分估值”与未纳入项，指标 unavailable 不显示为 0；更正后报告提示过期 | AC-02～AC-07、AC-22 |
| M2-W03 桌面骨架、首次使用与数据中心 | `delta-desktop`：Dock 工作台、导航、布局持久化、设置（报表币种、界面时区、数据目录只读展示、涨跌配色）；首次使用向导（建库、添加账户、导入或载入离线示例库）；导入预览界面（原始行、规范化值、错误、疑似重复、held 行确认/排除、映射修订）；数据中心（数据源能力与条款类别、覆盖区间与缺口、同步记录、质量问题、一键同步作业的进度/取消）；底部任务面板 | 首次使用到导入提交全程键盘可操作；同一文件重导提示已导入；同步作业进度与终态真实 | 空库、同步中、部分缺失、离线、限流/鉴权失败、取消、恢复失败均有原因与下一步；取消后 UI ≤1 s 响应 | AC-01、AC-16、AC-17、AC-21 |
| M2-W04 总览与资产视图 | 总览 `MetricCard`（净资产、净投入、区间损益、数据覆盖、待对账）与下钻；资产页：账户、持仓、FIFO 批次、流水（`DataTable` 虚拟滚动）、对账差异、来源批次与原始行；从统计进入流水保留范围与筛选 | 10 万条流水虚拟滚动；下钻数值与 W02 查询一致 | 缺失值显示为缺失而非 0；范围切换在页面可见 | AC-02～AC-07、AC-22、AC-26 |
| M2-W05 市场与 K 线 | 标的搜索（别名、场所与交易对区分）、自选组；`KlineViewport` + 自绘主图/成交量/MA/EMA/MACD/RSI 副图（参数可配、预热为空）；跳到日期、缩放、平移、十字光标；成交与笔记标记点击打开详情；复权/时区/过期/缺失/未完成 K 线标识；保存与恢复 `chart_context` | 10 万根按窗口加载；不同窗口同日指标一致；由笔记返回图表恢复上下文且锚点居中 | 向未来注入哨兵值不影响当前可见指标与标记（`visible_until`）；缺数据区间显示缺口而非连线 | AC-08、AC-11、AC-21、AC-26 |
| M2-W06 日志与笔记 | Markdown 编辑与预览、交易前/中/后模板（判断、失效条件、计划风险、实际执行、情绪、结论）、标签、搜索；一篇笔记关联多个成交/计划/图表，关联对象更正后提示；附件内容寻址与存在性校验；模型访问开关 | 自动保存状态诚实；分批建仓/减仓关联数量不超额；双向跳转 | 写失败保留缓冲并可重试；附件缺失被检出；崩溃恢复到最近修订 | AC-08、AC-09 |
| M2-W07 模型客户端与 AI 复盘 | `delta-model` 两协议适配与回环测试服务；`delta-app::ai` 运行时、工具网关、证据与数值校验、会话/压缩；模型连接配置（协议、base URL、model ID、keyring 凭据引用、超时与预算）、连通性与能力检测；右侧 AI 面板（发送前显示模型、账户、期间、是否含笔记；可固定范围；生成中标识；取消；证据可点击打开） | AC-02 场景问答数值 68 及分项正确、引用可打开；两协议文本流、工具配对、取消、错误均通过 | 越权指令笔记不扩权；伪造引用被拦截；模型离线/未配置时本地功能不受影响；半流不重放、迟到事件丢弃 | AC-10、AC-19、AC-23～AC-25 |
| M2-W08 运维、打包、性能与阶段门禁 | 备份/恢复/导出（账本 CSV/JSON、笔记 Markdown、附件、配置与数据清单）/删除预览 UI；日志不含密钥、完整账户号与笔记正文；按用户安装包（两通道）；产品锁文件逐包许可清单与通知（含 MPL-2.0 处理，`OSS-11`）；NFR-03/04/05 固定规模基准（硬件、次数、分位数）；`verify-stage --stage M2 --plan-version 1.0`（含 M1 回归） | 恢复后账本/笔记/附件引用一致；安装后无需开发环境可运行；基准报告真实 | 删除范围可确认且无残留引用；未测性能项写 not-run 不宣称达标 | AC-09、AC-20、AC-26、AC-27 |

用例清单见 [m2-stage-cases.json](m2-stage-cases.json)（`automated` / `manual` / `live`）；B 可以拆分或补充用例，但不得删除已有用例覆盖的需求/AC。

## 6. 不得缩减项

1. 金额与指标只由 `delta-core` / `delta-app` 计算；UI 与 AI 不得自行计算或改写金融数值；缺失不得显示为 0。
2. UI 与 AI 同 scope/版本结果一致（AC-23）；AI 工具只读、范围由宿主执行、引用与数值经宿主校验。
3. 保存、同步、导入、AI 运行的状态必须诚实：失败不显示成功，取消不显示完成，部分结果标明不完整。
4. 正式版/开发版数据隔离（D-26）、静音测试（D-25）、凭据只在 keyring、日志不含秘密与笔记正文。
5. 备份/恢复/导出/删除覆盖笔记、附件与会话数据。
6. M1 全部 AC 回归保持通过。

## 7. 真正依赖用户的事项（不阻塞工程）

- `ACT-01`：真实 AI 模型连接配置（协议、base URL、model ID、凭据、预算）。未提供时两协议以受控回环服务完成自动验收，真实服务保持 not-run。
- `ACT-03`：真实去标识账单样本 / 私有 Token（沿用 M1）。
- `H-01`～`H-03`：中文输入法、键盘/缩放与干净机器安装的人工检查，B 交付安装包与检查步骤后执行；结果不影响 A 技术审核与远端授权请求。

## 8. 检查命令

| 命令 | 状态 |
| --- | --- |
| `python scripts/check_docs.py`、`git diff --check`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --locked` | 已存在 |
| `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6 [--live]` | 已存在（M1 回归） |
| `cargo run -p delta-cli -- verify-stage --stage M2 --plan-version 1.0 [--live] [--ui-smoke] [--ui-bench]` | **已建立**：默认静音离线/无头检查；缺显式 UI 开关时 K02/K06/A20 与 overall 保持 not-run；`--live` 为公共探针，真实模型仍按 ACT-01 单独验证；显式窗口保持隐藏、不激活 |
| 依赖边界检查（`cargo metadata` 断言 `delta-desktop` 直接依赖集合） | **已建立**，纳入 M2-G-06，逐包许可执行完整收集与回归 |
| 安装包构建（`cargo packager` 或备选） | **已建立**：S-23 备选 `delta-cli package-build`，真实两通道 release 构建、静态 CRT 与 PE 导入检查、ZIP＋Rust 按用户安装器；执行事实见阶段报告 |

报告与日志写入忽略目录 `.local/reports/m2/`，脱敏摘要回写 `docs/delivery/stage-report.md`；结果绑定源码指纹（沿用 M1 规则，指纹范围扩展到新 crate 与 M2 夹具）。

## 9. 结束标准与收尾

- **B 实现与自检完成**：W01～W08 全部可执行内容实现，`verify-stage --stage M2 --ui-smoke --ui-bench` 的必要离线/原生 UI 验证 passed，spike 结论与失败分支如实记录，`docs/delivery/stage-report.md`（登记 `retireAfter: M2`）、[执行状态](status.md)、受影响正本与[集中台账](../decisions/open-questions.md)已同步。
- **A 技术审核通过**：A 对照本计划、实际 diff 与证据复验高风险项（金额一致性、范围隔离、AI 证据与数值校验、保存诚实、数据隔离与恢复），局部问题直接修复；工程阻断为零后在 [USER-ACTIONS](../../USER-ACTIONS.md) 登记本阶段 push / PR 的具体授权请求。
- **产品验收**：人工检查 `H-01`～`H-03` 与真实服务 `ACT-01` / `ACT-03` 独立推进，未测保持 not-run。

## 10. A → B 交接 prompt

```text
你是 DELTA 的 Agent B。读 AGENTS.md、docs/delivery/status.md 与唯一计划 docs/delivery/stage-plan.md（M2 v1.0），用例见 docs/delivery/m2-stage-cases.json，A 的 gpui-kit spike 证据见 docs/evidence/2026-11-08-m2-gpui-kit-spike.md。
分支 codex/m2-desktop-workbench，阶段父提交 0370b7e9103d0172a7ab47613ea253949d6a9b8b；完整 SHA 见交接消息。先核对 git status、父提交与现场，保留未跟踪 workers/agent/。
按第 5 节依赖顺序连续完成 M2-W01～W08，先打通 M2-K01 最薄切片；第 3 节 S-12～S-24 已定案，不重新选型，spike 失败按第 4 节分支处理并记录。遵守不得缩减项、D-25 静音、D-26 通道隔离；离线/回环结果与真实服务分开声明。
交付：建立 verify-stage --stage M2 并实跑，新建并登记 docs/delivery/stage-report.md，更新 status.md、受影响正本与台账，全部 git commit --amend -s 进当前 M2 阶段提交（父提交不变），给出新完整 SHA 与 B→A 审核 prompt。不 push、不操作 PR、不自动创建其他 Agent 会话。
```
