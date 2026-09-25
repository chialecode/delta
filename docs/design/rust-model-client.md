# Rust 模型客户端设计

版本 0.1 · 2026-09-26 · 依据 D-08 与 [ADR-0002](../decisions/0002-ai-integration.md)。这是 R1 实施合同，尚未证明产品代码或真实端点可用。

## 1. 范围与参考方式

DELTA 使用 Rust + Python。模型客户端、只读工具编排和上下文状态用 Rust；Python 承担按需指标、交易日历、数据连接与后续策略研究。首轮自研一个窄模型客户端，复用 Tokio、reqwest、serde 和成熟 SSE 解析组件，不自研 HTTP、TLS、异步调度或通用编码 Agent 平台。

参考 OpenAI 官方 Codex 的 Rust 实现，固定提交 `c7e80f873f67dbef58206b9d4f3c60e9d556eb16`。参考位置、源码哈希、许可和限制见 [源码研究](../evidence/2026-09-26-codex-rust-research.md)。该提交是阅读基点，不是 DELTA 的 Cargo 依赖或已验证兼容版本。首轮不启动 Codex CLI/app-server，不引入整个工作区，也不移植其 shell、文件编辑、插件、登录或编码权限体系。

## 2. 分层与所有权

| 层 | 负责 | 边界 |
| --- | --- | --- |
| `ModelClient` | 类型化请求、流式事件、取消、能力声明和错误 | 不访问业务数据库、不执行工具、不拥有会话真相 |
| 协议适配 | Responses / Chat Completions 编解码、工具参数拼接、协议终态 | 不把一种协议的字段直接传给另一种，不暗换模型或端点 |
| HTTP transport | TLS、认证注入、连接/空闲/总超时、有界响应、有限重试 | 复用库；不记录请求正文或凭据，不自动重定向携带认证的请求到别的主机 |
| `AgentRuntime`（Rust 应用层） | 冻结 scope、工具循环、预算、报告验证、上下文编排 | 工具只经 AnalysisToolGateway；模型返回的授权字段无效 |
| SQLite 会话仓储 | 转录、工具回执、运行状态、检查点和压缩来源 | Rust 写入，数据库迁移；UI 仅是投影，详见 [上下文状态](context-state-management.md) |

模块可先置于 `delta-infra::model`、`delta-app::ai`，需要独立测试边界时再拆 `delta-model` crate；不要为了图中的每一层建立一个空 crate。

## 3. 最小公共契约

```text
ModelConnection
  id, protocol, base_url, model_id, credential_ref,
  context_window, capabilities, timeouts, retry_policy

ModelRequest
  request_id, run_id, generation, connection_id,
  instructions, input_items[], tool_definitions[], output_budget

ModelEvent
  request_id, sequence,
  kind: Started | TextDelta | ToolCallDelta | ToolCallReady |
        Usage | Completed | Failed

ModelError
  code, retryable, safe_message, provider_request_id?, retry_after?
```

类型是设计合同，具体 Rust trait/stream/cancellation token 由 B 落地。输入项保留消息、工具调用/结果及协议需要的 reasoning/opaque 项；不把结构化输出拍平成字符串。opaque 项限定连接、协议、模型和 scope，拒绝跨供应商复用；不把不可读内容当作授权或金融事实。

运行状态与 UI 事件使用 [统一接口](integration-contracts.md)的 run/generation。一个模型请求的 Completed 仅表示协议结束；工具回合、后续模型请求和报告校验完成后，整个分析 run 才能 succeeded。Usage 可能多次更新，按协议语义归并；缺失记 unknown，不写零费用。

## 4. 两种协议的 R1 范围

| 项目 | Responses | Chat Completions |
| --- | --- | --- |
| 路由 | 显式 `openai-responses`，配置的 API 根下 `/responses` | 显式 `openai-chat-completions`，API 根下 `/chat/completions` |
| 上下文 | 类型化 input/output items；首轮本地保存并发送所需历史 | messages / assistant tool_calls / tool messages |
| 工具关联 | function_call 与 function_call_output 按 call_id 关联 | 分片先按 choice/tool index 拼接，完整后按 tool_call_id 关联 |
| 流式 | typed SSE events；按 response.completed/failed/incomplete 等语义判断 | delta chunks、finish_reason 与 `[DONE]`；严格区分正常结束和截断 |
| 首轮最低能力 | 文本流、完整工具调用、取消、错误；保留所需 reasoning 项 | 文本流、完整工具调用、取消、错误；模型能力逐项检测 |
| 验证 | 受控端点独立全套用例 | 受控端点独立全套用例 |

OpenAI 官方连接优先推荐 Responses；兼容端点按用户选定协议连接。两种适配均属于 R1 自动验收，实际服务仅对用户配置和授权的协议/模型声明通过；不要求为两种协议各购买服务。未知协议明确拒绝，不能失败后自动切协议或供应商。

首轮只做文本与业务 function tools；图像、音频、Realtime、WebSocket、远端会话托管、provider 原生检索和 computer-use 延期。结构化报告优先由宿主类型/引用校验保证，不把供应商独有 structured output 设为所有端点的前置。

自定义端点配置校验 URL、路径拼接和认证方式；禁止在 URL 中嵌密钥。远端用 HTTPS，本地开发受控服务可显式使用回环 HTTP。默认凭据是 API key 的 keyring 引用，不移植 ChatGPT 登录流程。Responses 在支持时使用 `store: false`，保留必要 reasoning/encrypted 项；它不等于第三方端点的零留存承诺。

## 5. 流式、重试与工具安全

- 使用成熟 SSE parser（Codex 使用 eventsource-stream；B 核对实际维护、许可和 reqwest 兼容后锁版），测试跨字节 UTF-8、多行 data、分片 JSON、注释/空帧、重复/未知事件和有界缓冲。未知非关键事件可忽略并计数，关键完成信息缺失必须失败。
- 工具参数完整、JSON/schema 合法、call ID 唯一且工具回合正常结束后才交网关；不能执行半条参数。重复 call ID 同参数去重，异参数拒绝；首轮顺序执行业务工具，避免并发放大范围或预算。
- 正常 EOF 不能替代协议成功终态；length/content_filter、incomplete、断流保留部分结果并标明不完整。不能把部分文本包装为成功金融报告。
- 明确 401/403 不重试；429 和可重试 5xx/连接故障在尚无应用可见输出时最多重试 2 次，尊重 Retry-After、退避和总预算。远端是否处理未知时说明可能重复计费。出现可见文本/工具片段后不自动重放整个请求，不重复已完成工具。
- 取消覆盖连接、流读取、退避等待、工具调度和压缩；关闭流、取消任务并回收句柄，期限未退出标 interrupted。陈旧 generation 不能写会话或 UI。进程内任务退出不以杀死桌面进程实现。
- 默认每条 SSE/协议记录上限 1 MiB、工具明细 100 行、单 run 12 次工具调用与 120 秒；全请求/累计流/事件队列也必须有上限。B 按真实样本锁定具体值并记录；超限报错，不能无限缓存或截断成合法成功。

## 6. 上下文与压缩协作

客户端不压缩历史。Rust 应用层按 [上下文状态](context-state-management.md)选择可压缩完整回合，用同一 ModelClient 进行预算内摘要，并以 SQLite 事务提交检查点。原始记录保留；系统约束、授权范围、数值及引用由宿主重建，不信任摘要恢复权限。

Codex 的历史配对与压缩检查点可作为设计参考，但编码提示、内部 compact 接口和供应商特殊续传不能当作通用 API。R1 不依赖远端 compact endpoint；本地摘要失败时保留原检查点，提示上下文不足或结束本次运行，不能悄悄丢历史。

## 7. 实施与维护责任

POC-04 先打通真实 Rust 客户端到受控 HTTP/SSE 服务，再做工具循环、状态仓储和故障注入；整个 ModelClient 被 mock 掉不能计作协议通过。测试基于官方线格式样本和独立期待，不用生产序列化器同时生成假服务的所有响应。

首轮 R1-A-15～20、22 与 R1-L-02 覆盖本合同。必须留存两协议结果矩阵、取消/半流重试/工具配对证据、压缩原子失败与重启恢复，以及无模型配置仍可用的本地功能。

每次上游参考升级记录旧/新 SHA、采用模式、相关 API 文档和回归范围；不自动跟随 main。实际复制源码须记录文件/提交/改动、保留适用 Apache-2.0 许可、版权和 NOTICE，并核对片段的其他来源；本轮只引用源码。若维护或兼容成本失控，记录 OSS-08，A 比较 Rust 库替换窄适配，维持 ModelClient 与 SQLite 数据格式的迁移/导出边界。
