# ADR-0003：首轮技术与通用栈

日期：2026-09-26；状态：proposed（工程组合待 B 实测），用户指定方向 D-05/06/08 不再待选；责任：A 推荐、B 验证。

## 决定范围

沿用 Rust、Windows、GPUI；首轮推荐 GPUI Kit 匹配依赖、SQLite/rusqlite、rust_decimal、Tokio/tokio-util、reqwest、serde/serde_json/schemars、thiserror/tracing、chrono/chrono-tz、keyring、csv 和常规 uuid/directories/tempfile。Rust 模型客户端按 ADR-0002，指标优先 TA-Lib，Python 仅按需打包。

这是可实施推荐，不是全部包的安装命令。具体版本、feature、工具链和传递许可由实际锁文件及验证结果记录；未用组件不预装。已确认的框架基线保持，兼容修订可在相同契约下完成。

## 理由与备选

[完整比较](../engineering/technology-selection.md)已对 GPUI/Tauri/iced/egui、原生/Web 图表、rusqlite/SQLx、Decimal/BigDecimal、Codex 源码参考/内部 crate/app-server/Rig、指标/日历/CCXT 语言选择给出适配和成本。公开资料与版本查询见 [证据](../evidence/2026-09-26-technology-research.md)。

选择依据是最少运行组件贯通 MVP，并把投入保留给业务/AI 整合；不因依赖库“功能多”扩大产品范围。GPUI 和 Rust 模型客户端的实测门槛分别保护交互可用性与只读金融上下文。通用组件选型不能替代对自身金额、恢复与权限的验证。

## 定稿与替换

执行 [R1](../delivery/r1-execution-plan.md) POC 门槛后，B 记录兼容矩阵、native 依赖、锁版、许可和关键用例结果，A 核对后将具体已验证部分定稿。用户已授权方向不重复审批，实质改变方向/语义再集中决定。

上游问题跟踪 OSS-01～08，不 fork 一套新平台。停止维护、关键缺陷无解、打包不可用或许可冲突触发替换比较；旧数据和会话需要迁移/导出，禁止静默丢弃。
