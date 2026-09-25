# 2026-09-26：Codex 官方 Rust 参考核查

角色：Agent A；用途：依据用户 D-08 调整模型客户端方案。仅阅读官方文档与源码，没有编译 Codex、运行 DELTA 模型客户端或调用真实模型。

## 1. 可复核基点

通过 `git ls-remote https://github.com/openai/codex.git refs/heads/main` 查询提交 `c7e80f873f67dbef58206b9d4f3c60e9d556eb16`，再按提交获取 GitHub tree 和 15 个参考文件。每个文件的固定 URL、获取状态、SHA-256 和行数保存在 [机器快照](2026-09-26-codex-rust-snapshot.json)。main 是查询时快照，不是稳定 SDK 版本，也不是对维护质量和全部 issue 的审计。

## 2. 参考映射

以下路径均相对于该官方固定提交，链接可直接阅读原文件。

| 源码 | 实際观察 | DELTA 采用边界 |
| --- | --- | --- |
| [Responses endpoint](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/codex-api/src/endpoint/responses.rs) | ResponsesClient 组合 transport/provider/auth，单独封装请求和流 | 参考客户端分层；不把内部 crate 当独立稳定 SDK |
| [底层 SSE](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/codex-client/src/sse.rs) | eventsource-stream、空闲超时、有界 channel，提前关闭报错 | 复用成熟 parser；DELTA 另定义累计大小、取消和协议成功终态 |
| [Responses 事件解析与测试](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/codex-api/src/sse/responses.rs) | 类型化事件与错误/终态解析，包含事件样例测试 | 参考碎片与事件测试；不据此宣称 Chat Completions 已支持 |
| [retry](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/codex-client/src/retry.rs) | 有限尝试、429/5xx/transport 分类、Retry-After、退避和抖动 | DELTA 增加部分流不可重放、工具去重和费用未知约束 |
| [core client](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/core/src/client.rs) | 上层模型会话与请求协调 | 仅参考边界；不移植完整编码运行时、认证或传输功能全集 |
| [history](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/core/src/context_manager/history.rs)、[normalize](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/core/src/context_manager/normalize.rs) | 历史管理与 tool call/output 配对；有补缺失输出和移除孤立输出逻辑 | DELTA 保留原始审计记录；中断输出只能标 interrupted/error，不能补造执行成功 |
| [compact](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/codex-rs/core/src/compact.rs) | 压缩与历史替换/上下文重建协调 | 参考检查点概念；本地范围由宿主重建，不依赖 Codex 内部 compact API |

已读取 codex-client/codex-api Cargo manifest，两者使用多项 workspace 内部依赖。因此本轮推荐参考设计并实现 DELTA 窄接口，不能把仓库有 Rust crate 写成存在可直接安装的官方通用 Rust SDK。

## 3. 官方接口资料

- [Codex app-server](https://developers.openai.com/codex/app-server)：官方描述完整 harness 的客户端接口并链接开源仓库。本轮采用源码参考，不接入该服务器。
- [Streaming Responses](https://developers.openai.com/api/docs/guides/streaming-responses)：Responses 使用有类型的 SSE 事件。
- [迁移到 Responses](https://developers.openai.com/api/docs/guides/migrate-to-responses)：官方推荐新项目使用 Responses；Chat Completions 与 Responses 的消息、工具关联和流式格式不同。Responses 的 reasoning/output items 不能一律转成普通文本。

本轮实际检索并获取上述官方页面，源码另按用户要求读取官方 GitHub。未指定真实模型，不能据文档推定某端点支持全部 tools、usage、reasoning 或相同上下文窗口。`store: false` 的 API 语义不能替代第三方数据留存政策。

## 4. 许可和未测项

官方根 [LICENSE](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/LICENSE) 为 Apache-2.0；[NOTICE](https://github.com/openai/codex/blob/c7e80f873f67dbef58206b9d4f3c60e9d556eb16/NOTICE) 保留 OpenAI 版权及 Ratatui 派生说明。源码复制需按具体片段核对许可、版权、NOTICE 和修改说明，不能只用“开源”概括。本轮没有复制运行时代码，也未改变 DELTA 项目许可证。

已确认的是参考来源和设计依据；DELTA 的 Windows 构建、依赖组合、流式正确性、压缩/恢复和真实服务全都尚待 B 实施。维护/来源跟进集中于 OSS-06/08，端点差异于 OSS-07；具体执行见 [模型客户端合同](../design/rust-model-client.md)。
