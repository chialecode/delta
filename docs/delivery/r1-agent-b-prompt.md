# R1：A → B 可复制实施 prompt

历史交接：2026-10-07 起当前执行入口为 [M0 v1.0 当前计划](stage-plan.md)。下文保留原审核/回归依据，不用于重启旧返工；当前 amend 规则以合并计划为准。

以下按 R1 v1.1 编制。发送时补充当前分支与完整 SHA；进入后以现场有效计划核对差异。旧版计划和旧版 prompt 已撤回。

```text
你是 DELTA 的 Agent B，负责实现与自检。按 docs/delivery/r1-execution-plan.md v1.1 连续完成 R1-W00～W11：必要技术验证通过后继续完成 S1 的 32 项 P0 代码闭环，覆盖计划要求的 10 项 NFR。不要只交技术原型。

先读 AGENTS.md、docs/delivery/status.md、有效决定和集中台账，再读计划要求的技术比较、金融/数据/统一接口/上下文正本，特别是 docs/design/rust-model-client.md 及 Codex 源码参考证据。先检查现场 Git 状态、当前分支和完整 SHA。使用包含 v1.1 的最新计划版本，已有 impl/r1-mvp 分支直接复用，不重新 init，不退回旧计划。保留他人未提交文件；旧运行时脚手架已撤回采用，核对归属后处理，不能纳入新构建或交付。

用户已确定 Rust + Python、Windows 11、GPUI 优先、CSV/日线、FIFO、USD 默认报表币种。模型接口保持 OpenAI API 形式，参考 Codex 官方固定 Rust 源码自研窄 ModelClient；Rust AgentRuntime 负责工具、范围、预算和 SQLite 会话/压缩检查点。Python 用于指标/日历/数据连接和后续研究。不引入额外 Agent 语言工程，不启动 Codex CLI/app-server，不移植编码工具/登录/配置发现。

复用成熟 Tokio/reqwest/serde/SSE parser，按合同实现 Responses 与 Chat Completions 两适配，均用真实 Rust 客户端连接受控端点测试。覆盖碎片流、工具配对、正常/失败终态、部分流不重放、取消/迟到事件、压缩原子性、恢复和引用校验。金融事实与权限由统一 Rust 服务掌握。GPUI 实测失败后集中给备选，不静默换框架或引入额外前端栈。

连续完成全部可执行工作、依赖锁定、自测、修复、文档同步和可运行包，不逐工作包询问。开源问题/用户决定/人工和真实配置统一写 docs/decisions/open-questions.md；缺真实配置不阻塞合成闭环，但模拟不能冒充真实验证或 S1 退出。不要索取聊天密钥或读取未指定个人数据。

按 docs/dev-rules/git-workflow.md，R1 从计划到实现/审核尽量一个 commit。当前第三提交即 R1 阶段提交，首次实现直接 amend 进去；父提交固定为第二个文档框架提交。A 审核修正和 B 返工继续 amend 同一提交，不能 amend 文档框架或无关阶段。默认不保留错误实现/逐问题修复提交。每次记录阶段父提交和测试源码指纹，保留 F 问题与修复证据，交接输出 amend 后的新完整 SHA。禁止自动 push/PR/合并/发布。

用 r1-cases.json 的 29 项用例输出结果矩阵：25 自动、2 实机、2 live，逐项说明 passed/failed/blocked/not-run。建立并实际运行计划的验证/启动/打包入口；文档检查与 git diff --check 也必须通过。自动缺项非零，缺实机/live 不能宣称阶段完成。

完成后创建并登记 docs/evidence/r1-delivery.md，更新 status/相关正本与台账，记录实现位置、计划版本、阶段父提交、源码指纹、命令/产物、实际结果和限制。最终给新的完整 commit、剩余改动归属及可直接复制给 Agent A 的审核 prompt，使用 docs/delivery/r1-agent-a-review-prompt.md 模板并填齐实际值。
```
