# 2026-09-26：Agent A 技术调研证据

类型：reference；执行者：当前 Agent A；仅调研，没有安装候选或执行应用验证。可机器阅读的实际结果见 [快照](2026-09-26-upstream-snapshot.json)。

当前 Rust 模型方案的固定源码与官方接口依据另见 [Codex 参考研究](2026-09-26-codex-rust-research.md)。

## 方法与限制

2026-09-26 查询 crates.io、PyPI 及图表组件官方注册表；读取部分 GitHub 仓库元数据与官方 README/SDK/源码，以及 OpenAI 官方文档。注册表日期区分 release 的 created/upload/time 与包 updated_at。GitHub pushed_at 只证明仓库有推送，不证明代码质量、issue 响应或 CI 通过。

部分 GitHub API 查询返回 403 rate limit，记录失败并停止此渠道；改从官方源码/注册表读取必要事实。没有完成传递许可证、漏洞扫描、目标平台和全部 issue/CI 审核，相关缺口为 OSS-06。main 分支说明仅提供调查线索，实施按锁定版本复核。

## 关键发现

| 能力 | 本次观察 | 对 DELTA 的含义 |
| --- | --- | --- |
| GPUI Kit / component | 0.6.6，2026-09-21，Apache-2.0；仓库未归档、09-25 有推送 | 可作为优先候选；使用整套匹配依赖，不单独拼 GPUI 最新主分支 |
| GPUI CandlestickChart | 官方源码包含蜡烛与 tooltip/crossline 等实现 | 不证明已满足缩放、指标子图、业务标记和大窗口性能，需 POC |
| rusqlite | 0.40.2，08-08，MIT；仓库未归档、09-25 有推送 | 单机事务与备份首选；阻塞操作移出 UI |
| Rig / rmcp | 0.42.0 / 3.4.1，08-17 / 09-23，MIT / Apache-2.0；对应仓库未归档 | Rust AI 与 MCP 备选；当前按 D-08 自研窄客户端，不同时建多套主运行时 |
| TA-Lib | Python 0.8.1，09-21；官方 LICENSE 为 BSD-2-Clause，PyPI license 字段空 | 原生库/NumPy/wheel 组合需验证，不能将注册表空字段当作无许可或许可清算完成 |
| CCXT | Python 4.5.84，09-24，MIT；README 列多语言接口 | 按 D-08 比较 Python 与 Rust 的目标场所能力和分发成本 |
| Lightweight Charts | npm 5.2.1，Apache-2.0；官方 NOTICE/README 要求可见归属和链接 | 仅作比较，不纳入当前 Rust + Python 基线；另行确认额外前端栈后再考虑 |
| chrono-tz | 0.10.4，2025-07-11 | 发布较旧，必须实查打包 tzdb 与目标时区，不凭版本印象淘汰/通过 |

## 官方材料与适配判断

- [GPUI Kit](https://github.com/longbridge/gpui-kit)与[蜡烛图源码](https://github.com/longbridge/gpui-kit/blob/main/crates/component/src/chart/candlestick_chart.rs)：可复用控件，完整图表适配需实测。
- [OpenAI 流式响应](https://developers.openai.com/api/docs/guides/streaming-responses)、[迁移协议区别](https://developers.openai.com/api/docs/guides/migrate-to-responses)：Responses 使用 typed events，Chat Completions 是 delta chunks；工具关联与上下文字段不应混用。官方建议 Responses 不意味着所有第三方兼容端点支持它。
- [Lightweight Charts](https://github.com/tradingview/lightweight-charts)、[NOTICE](https://github.com/tradingview/lightweight-charts/blob/master/NOTICE)：复用与归属边界。
- [Longport SDK](https://github.com/longportapp/openapi)、[CCXT](https://github.com/ccxt/ccxt)：只选择获授权市场/只读能力；SDK 提供的交易接口不进入 DELTA 首版工具。
- [TA-Lib Python](https://github.com/TA-Lib/ta-lib-python)、[Beancount](https://github.com/beancount/beancount)、[NautilusTrader](https://github.com/nautechsystems/nautilus_trader)：指标、账本对照与后续模拟候选，不据名称直接替代 DELTA 事件/修订与训练时钟。

选择理由和待测门槛见 [技术选型](../engineering/technology-selection.md)，当前风险统一见 [集中台账](../decisions/open-questions.md)。没有在本次文档任务调用真实模型、账户接口或发送私人金融内容。
