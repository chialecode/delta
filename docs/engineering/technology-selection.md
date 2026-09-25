# DELTA 技术栈与通用能力比较

版本 0.4 · 2026-09-26 · Agent A 已完成首轮资料比较，尚无本轮 Rust 模型客户端兼容验证证据。用户决定见 [确认记录](../decisions/confirmed-decisions.md)，实际来源见 [调研报告](../evidence/2026-09-26-technology-research.md)和[快照](../evidence/2026-09-26-upstream-snapshot.json)。

## 1. 结论与状态

首轮建议：**Rust + GPUI Kit；SQLite/rusqlite；rust_decimal；Tokio/reqwest；参考 Codex 官方实现的自研 Rust 模型客户端；按需 TA-Lib/Python；统一 Rust 应用能力与上下文范围。** 开源负责通用机制，DELTA 负责领域语义、业务装配、快照/权限和证据。

“已确认方向”是 D-05/06/08；“首选待验”可按当前计划实施并锁兼容版本；“备选”只在明确条件触发；“后续”不加入首轮安装清单。推荐不是已经交付或依赖组合已验证。

比較按需求适配、当前维护、Windows/离线分发、许可、集成和退出成本进行，不用虚构分数掩盖未测结果。版本表是查询快照；B 采用经验证版本、完整锁文件和实际许可证清单，不盲目安装所有 latest。

## 2. 桌面与图表

| 方案 | 复用价值 | 成本/风险 | 本轮结论与验证门槛 |
| --- | --- | --- | --- |
| GPUI Kit 0.6.6 + 匹配 GPUI | Rust 原生控件、表格/Dock/编辑和主题；符合用户方向 | GPU、中文输入/缩放和图表完整度待验；组件与 GPUI 版本耦合 | 首选；使用 kit 匹配版本，不自行拼 Zed 主分支；POC-01/02 |
| Tauri 2 + Web UI + Lightweight Charts | Rust 核心保留、Web 图表生态成熟 | 新增 Web 前端与 IPC/WebView 分发，偏离当前 GPUI | 备选，只有 GPUI 失败且用户确认额外前端栈后考虑；不先建设双 UI |
| iced 0.14 | Rust、明确消息/状态模型 | 当前金融图表/富编辑/布局仍需适配；切换价值未证实 | 备选比较对象，不因纯 Rust 自动更适合 |
| egui/eframe 0.36 | Rust、研究工具快速原型 | 文档编辑、复杂焦点与产品式 UI 需验证 | 不作为首轮主框架，可用于小实验但不新增第二产品 UI |
| 原生 GPUI CandlestickChart | 无 WebView 依赖，主题一致 | 现有源码不证明缩放平移、子窗格、标记与 10 万数据窗口化满足需求 | 先做最小 ChartAdapter 验证，薄适配可满足就继续 |
| Wry 0.57 + Lightweight Charts 5.2.1 | 成熟金融图表基础，避免自研绘图引擎 | GPUI 嵌入窗口/焦点/DPI/打包未证实；需 NOTICE/归属 | Rust + Python 基线下不启用；只有另行确认额外前端栈后才验证，不能默认引入 |

图表不拥有指标和账本真相。必须验证 1000 可见 K 线、缩放/平移/十字线、成交量和指标、交易/笔记标记与上下文恢复。缺口涉及大量自研基础交互时停止扩建，记录 OSS-01 并给可运行比较；不为图表卡住账本/AI 核心。

## 3. 存储、金融与数据处理

| 能力 | 首选 | 备选 / 取舍 | DELTA 自研边界 |
| --- | --- | --- | --- |
| 事务账本/设置/笔记/首批行情 | SQLite + rusqlite 0.40.2 | SQLx 0.9 有异步/多后端，但单机写执行器无需两套驱动；避免 ORM 隐藏金额/迁移语义 | schema、幂等、事件修订、对账和范围；复用事务/备份接口 |
| 金额/数量 | rust_decimal 1.43 | bigdecimal 0.4 支持更大范围，但需约束运算/格式；首版明确拒绝 Decimal 超范围 | FIFO/费用/现金流/公司行动政策，不能自造十进制数值库 |
| 全文与中文检索 | SQLite FTS5 + 标签/时间；中文实测后选 trigram/受控子串回退 | 独立搜索服务、向量库增加同步和分发；暂无召回证据支持引入 | 原文修订/权限过滤/删除失效；短中文词不能以 FTS 默认分词自然成立 |
| 日线保存 | 首期 SQLite | Parquet 60/Arrow 在规模实测瓶颈后；DuckDB/Polars 按研究用例二选一 | 覆盖清单、来源、复权/时点版本；不提前引入多库一致性 |
| CSV | csv 1.4 + serde，显式编码/小数/时区映射 | calamine 面向表格文件，首轮 CSV 不引入；不手写 RFC CSV parser | 映射、预览、重复判定、错误行与事务入账 |
| 账本引擎复用 | 对照 Beancount 等模型/样本，主账本用本项目事件与统一服务 | 外部纯文本账本的修订/来源/跨语言/许可与运行时成本需专门验证 | 不是另建会计平台；仅当前业务规则，手算+独立对照，未来可输出开放格式 |

SQLite 外键/备份/全文编译特性在锁定版本确认。Decimal 序列化字符串，SQL REAL 聚合禁用。Beancount 没有完成完整许可与适配审计，暂不嵌入；不将“有会计库”或“要自研金融规则”视作免比较理由。

## 4. 通用工程栈

| 能力 | 推荐及复用 | 比较与不额外建设 | 验证/维护重点 |
| --- | --- | --- | --- |
| 异步与取消 | Tokio + tokio-util | 不自研调度器；GPUI 主循环与 Tokio 清晰桥接 | 有界队列、取消、关闭、晚到结果；不在 UI block_on |
| HTTP | reqwest | 复用 HTTP/TLS；Rust 模型协议用窄适配封装，参考 Codex 官方实现 | 超时/重试/代理/TLS/响应大小与错误脱敏 |
| 数据协议 | serde/serde_json + schemars；运行时参数用成熟 schema validator | 从 Rust 合同导出或比对跨语言 schema；不手写互不一致字段表 | Decimal 字符串、schema 主版本、未知字段/工具、兼容样本 |
| 错误/日志 | thiserror + tracing；应用最外层可用 anyhow | 不以字符串拼接充当跨进程错误协议；不自建日志框架 | 错误码、来源链与用户安全提示，秘密和正文不记录 |
| ID / 目录 / 临时文件 | uuid、directories、tempfile | 复用平台目录与原子/临时文件工具，不硬编码用户目录 | 稳定身份、路径逃逸、测试与真实资料分离 |
| 时间 | chrono + chrono-tz | 时间戳/时区复用库；交易日历不是 weekday 运算 | DST、半日市、日线切分、实际 tzdb 版本；OSS-04 |
| 凭据 | keyring | 明文 JSON/env 文件不作为产品持久存储；开发本地配置另行忽略 | Windows 凭据读写/失效/删除与异常，不把 key 放 CLI |
| Markdown 编辑/显示 | 先复用 GPUI Kit 输入/Markdown 能力；必要时 pulldown-cmark | 不先嵌入完整 Web IDE，不额外造富文本文档模型 | 中文 IME、自动保存、链接/附件路径与崩溃恢复 |
| 测试 | Rust 内建测试 + proptest；模型客户端用受控 HTTP/SSE 端点，Python 用成熟测试工具 | 不把模拟 provider 当真实可用；不写镜像实现测试 | 独立期望、属性守恒、生命周期/协议故障 |
| 依赖/构建 | rust-toolchain.toml + Cargo.lock；Python runtime/uv.lock 按需 | 保持 Rust + Python；不开设额外前端/Agent 语言工程 | 可重建安装、Windows 打包、native 传递依赖许可 |
| 文档/CI | Python 标准库检查 + GitHub Actions | 当前不必部署文档网站或复杂知识库 | 同一入口、本地负向夹具；CI 配置不等于远端通过 |

Rust 现场 1.98.1、Python 3.12.14 可调用，B 依据上游 MSRV 和实际兼容锁工具链。安装系统级编译依赖若实际缺失需具体诊断，不能凭 cargo 命令存在宣称 Windows 构建环境完备。

## 5. Rust 模型客户端与编排比较

| 方案 | 已核实能力/来源 | 集成与限制 | 本轮结论 |
| --- | --- | --- | --- |
| 参考 Codex 官方 Rust 源码，自研窄 ModelClient | 固定提交的 transport/Responses/SSE/retry/history/compact 模块；[研究与许可](../evidence/2026-09-26-codex-rust-research.md) | 自行维护两协议、流拼接、工具关联、取消和兼容；DELTA 拥有状态 | 用户 D-08 已选；POC-04 与协议故障测试定稿 |
| 直接复用 Codex 内部 crates | 有完整 Rust 实现，源码可审阅 | Cargo 依赖内部 workspace；不承诺独立稳定 SDK，升级面较大 | 不作为产品依赖，按需参考小范围设计 |
| Codex app-server / CLI | 官方提供完整 Agent harness 接口 | 编码工具/认证/配置/生命周期远超当前只读金融客户端范围 | 不接入，不继承其账号或工具权限 |
| Rig 0.42 | Rust 原生 LLM/工具生态，首轮已有注册表维护证据 | 会话/压缩/恢复仍需 DELTA 整合；与指定自研客户端方向不同 | 窄层维护成本失控时比较替换，不建第二套运行时 |
| Python 工作流平台 | 工作流/checkpoint 生态 | 引入第二个模型状态所有者，跨语言同步成本增大 | Python 保留指标/日历/研究职责 |
| MCP / rmcp 3.4 | 官方 Rust SDK，工具协议可复用 | 不替代应用服务、scope 或会话真相 | 第二真实客户端需要时适配；LOOP-02 |

主组合仍优先成熟开源：Tokio、reqwest、serde、SSE parser。Codex 使用 eventsource-stream，可作为 parser 起始候选，B 核对版本、维护、许可与 reqwest 组合后锁定；A 已读取其 Codex 使用位置，未完成该 parser 全量维护审计。不能为窄自研客户端另造 HTTP/TLS/调度器。

官方 OpenAI 连接推荐 Responses，兼容端点按显式配置选择 Chat Completions。R1 两适配均须经真实 Rust 客户端连接受控服务验证；Codex 的 Responses 源码不证明 Chat Completions 兼容。具体类型、范围、非目标与用例见 [模型客户端](../design/rust-model-client.md)、[上下文](../design/context-state-management.md)及 [ADR-0002](../decisions/0002-ai-integration.md)。

## 6. 指标、日历、数据源与研究

| 能力 | 首选 / 比较 | 首轮动作与后续条件 |
| --- | --- | --- |
| MA/EMA/MACD/RSI | TA-Lib Python 0.8.1 + 对应 native wheel；Rust ta/yata/ta-lib/talib 备选 | TA-Lib 维护/对照较清晰；Rust 候选较老或绑定上下文不同，不能只为少一个 worker 默认选它。POC-05 验证实际预热、数值、原生打包；缺口记录后才考虑有限自研 |
| 交易日历 | exchange_calendars 4.13.2；供应商官方日历作授权来源 | 验证 NYSE/NASDAQ、半日市/DST 与 24×7；可导入版本化 session 文件减少运行依赖，不能假设所有市场都覆盖 |
| 美股数据 | Longport 官方 Rust SDK 4.3.7；CSV/OHLCV 文件兜底 | 用户账户/地区/权限/费用未确定；先做能力和格式验证，不以 SDK 等于数据采购完成 |
| 加密数据 | CCXT Python 4.5.84 / Rust 候选 | Python 生态与现有分析环境可复用；Rust 可减少 worker，但目标场所完备性待测。按 Q-01 选择 Rust/Python 的已验证接口，不额外引入语言栈 |
| Python 管理 | uv 0.12.19 + 锁文件；Pydantic 按协议需要 | 开发/构建工具，不要求终端用户安装 Python；下载或捆绑 runtime 的许可、校验和离线启动需验证 |
| 扫描与分析 | Polars 或 DuckDB 后续按表达式/SQL 工作流取舍 | S1 SQLite 足够时不引入；先测数据量瓶颈再扩展 Parquet |
| 训练/回测 | S2 比较 NautilusTrader 等活跃核心和最小模拟适配 | 当前只读取官方架构/平台说明，未完成许可/时钟/订单语义验证；不能在 S1 顺带嵌入完整交易平台或先默认自研撮合 |

TA-Lib 原生及 wheel 的许可分别核对；Python 包 license 元数据为空时以实际 LICENSE 为证据。CCXT 各语言/交易所语义可能不同，统一 API 不消除分页、费用币种、历史覆盖和限流差异。所有账户连接只读，不向模型注册下单能力。

## 7. 开源问题与自研判据

OSS-01 图表、OSS-02 旧运行时方案已关闭、OSS-03 分发、OSS-04 日历、OSS-05 数据源、OSS-06 维护/许可证据、OSS-07 兼容协议、OSS-08 Rust 客户端维护全部集中于 [事项台账](../decisions/open-questions.md)。技术报告补证据，不另设当前状态表。

允许自研：统一 CallContext/ResultEnvelope、金融政策、业务 mapping、scope/版本校验、上下文选材、Rust 模型客户端/受控工具循环/有限摘要检查点（D-08）、图表/来源适配。需要先证明缺口才自研：指标算法、图表基础引擎、撮合核心。当前不建：微服务、通用插件市场、向量库、节点 IDE、任意脚本沙箱、全平台发布系统。

依赖选择流程：候选资料 → 当前需求 spike → 锁版 → 主/失败/打包验证 → 记录可替换边界与许可 → 进入本轮整合。小版本兼容调整可由 B 自主；方向、数据语义或大运行时替换由 A 提供集中方案。开源源码补丁必须有版本、复现、最小 diff、回归与移除条件。

## 8. 官方注册表查询快照

以下由本次保存的真实元数据整理。日期是所列版本发布日期；仅表明发布迹象，不表示已通过安装、安全、许可兼容或产品验收。进一步仓库推送/归档状态与失败请求见 JSON。

| 项目 | 版本 | 发布日期 | 许可元数据/已核文件 | 来源 |
| --- | --- | --- | --- | --- |
| crate: gpui-kit | 0.6.6 | 2026-09-21 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/gpui-kit) |
| crate: gpui-component | 0.6.6 | 2026-09-21 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/gpui-component) |
| crate: gpui | 0.2.2 | 2025-10-22 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/gpui) |
| crate: iced | 0.14.0 | 2025-12-07 | MIT | [官方](https://crates.io/api/v1/crates/iced) |
| crate: eframe | 0.36.2 | 2026-09-08 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/eframe) |
| crate: tauri | 2.11.6 | 2026-09-19 | Apache-2.0 OR MIT | [官方](https://crates.io/api/v1/crates/tauri) |
| crate: wry | 0.57.0 | 2026-09-08 | Apache-2.0 OR MIT | [官方](https://crates.io/api/v1/crates/wry) |
| crate: rusqlite | 0.40.2 | 2026-08-08 | MIT | [官方](https://crates.io/api/v1/crates/rusqlite) |
| crate: sqlx | 0.9.0 | 2026-05-21 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/sqlx) |
| crate: rust_decimal | 1.43.0 | 2026-09-02 | MIT | [官方](https://crates.io/api/v1/crates/rust_decimal) |
| crate: bigdecimal | 0.4.10 | 2025-12-27 | MIT/Apache-2.0 | [官方](https://crates.io/api/v1/crates/bigdecimal) |
| crate: tokio | 1.53.1 | 2026-07-20 | MIT | [官方](https://crates.io/api/v1/crates/tokio) |
| crate: reqwest | 0.13.5 | 2026-09-08 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/reqwest) |
| crate: chrono | 0.4.45 | 2026-06-04 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/chrono) |
| crate: chrono-tz | 0.10.4 | 2025-07-11 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/chrono-tz) |
| crate: keyring | 4.2.0 | 2026-08-29 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/keyring) |
| crate: serde | 1.0.229 | 2026-07-18 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/serde) |
| crate: thiserror | 2.0.21 | 2026-09-23 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/thiserror) |
| crate: tracing | 0.1.44 | 2025-12-18 | MIT | [官方](https://crates.io/api/v1/crates/tracing) |
| crate: csv | 1.4.0 | 2025-10-17 | Unlicense/MIT | [官方](https://crates.io/api/v1/crates/csv) |
| crate: schemars | 1.2.2 | 2026-07-27 | MIT | [官方](https://crates.io/api/v1/crates/schemars) |
| crate: rig-core | 0.42.0 | 2026-08-17 | MIT | [官方](https://crates.io/api/v1/crates/rig-core) |
| crate: rmcp | 3.4.1 | 2026-09-23 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/rmcp) |
| crate: ta | 0.5.0 | 2021-06-26 | MIT | [官方](https://crates.io/api/v1/crates/ta) |
| crate: yata | 0.7.0 | 2024-03-07 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/yata) |
| crate: longport | 4.3.7 | 2026-08-12 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/longport) |
| crate: proptest | 1.11.0 | 2026-03-24 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/proptest) |
| pypi: TA-Lib | 0.8.1 | 2026-09-21 | 字段缺失；核实际文件 | [官方](https://pypi.org/pypi/TA-Lib/json) |
| pypi: uv | 0.12.19 | 2026-09-25 | MIT OR Apache-2.0 | [官方](https://pypi.org/pypi/uv/json) |
| pypi: exchange_calendars | 4.13.2 | 2026-03-10 | Apache-2.0 | [官方](https://pypi.org/pypi/exchange_calendars/json) |
| pypi: pydantic | 2.13.5 | 2026-08-28 | MIT | [官方](https://pypi.org/pypi/pydantic/json) |
| pypi: ccxt | 4.5.84 | 2026-09-24 | MIT | [官方](https://pypi.org/pypi/ccxt/json) |
| pypi: polars | 1.44.2 | 2026-09-09 | MIT 文本；见元数据快照 | [官方](https://pypi.org/pypi/polars/json) |
| npm: lightweight-charts | 5.2.1 | npm latest | Apache-2.0 | [官方](https://registry.npmjs.org/lightweight-charts/latest) |
| npm: ccxt | 4.5.84 | 2026-09-24 | MIT | [官方](https://registry.npmjs.org/ccxt) |
| crate: uuid | 1.26.1 | 2026-09-10 | Apache-2.0 OR MIT | [官方](https://crates.io/api/v1/crates/uuid) |
| crate: directories | 6.0.0 | 2025-01-12 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/directories) |
| crate: tempfile | 3.27.0 | 2026-03-11 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/tempfile) |
| crate: serde_json | 1.0.151 | 2026-07-20 | MIT OR Apache-2.0 | [官方](https://crates.io/api/v1/crates/serde_json) |
| crate: tokio-util | 0.7.19 | 2026-07-21 | MIT | [官方](https://crates.io/api/v1/crates/tokio-util) |
| crate: pulldown-cmark | 0.13.4 | 2026-05-20 | MIT | [官方](https://crates.io/api/v1/crates/pulldown-cmark) |
| crate: calamine | 0.36.1 | 2026-07-27 | MIT | [官方](https://crates.io/api/v1/crates/calamine) |
| crate: duckdb | 1.10505.0 | 2026-07-22 | MIT | [官方](https://crates.io/api/v1/crates/duckdb) |
| crate: parquet | 60.0.0 | 2026-09-15 | Apache-2.0 | [官方](https://crates.io/api/v1/crates/parquet) |
| crate: ccxt | 4.5.84 | 2026-09-24 | MIT | [官方](https://crates.io/api/v1/crates/ccxt) |
| crate: ta-lib | 0.1.2 | 2023-01-16 | Apache-2.0 OR BSD-3-Clause OR MIT OR Zlib | [官方](https://crates.io/api/v1/crates/ta-lib) |
| crate: talib | 0.1.2 | 2024-12-19 | MIT | [官方](https://crates.io/api/v1/crates/talib) |

Codex 固定提交源码已核查；GPUI/Zed/Rig 等主分支与发布包可能不同。遇到版本差异先依据锁包重新核实，不能把 main 的 API 直接写成可编译代码。依赖最终定稿由 B 的实际工程和 A 审核结果支持。
