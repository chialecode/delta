# 数据、凭据与恢复规则

触发：导入、数据库、文件、迁移、AI 发送、子进程或日志修改。数据语义以 [数据模型](../design/data-model.md) 与 [金融规则](../design/financial-engine.md)为准。

## 数据库与迁移

每连接启用外键；金额 TEXT 由 Decimal 解析运算，不通过 SQLite REAL 聚合。写入统一执行器与短事务，UI 不阻塞等待数据库。schema migration 追加版本，已经交付的迁移不原地重写；迁移前一致性备份，失败保持旧库可打开或明确恢复流程。

导入预览绑定文件、映射与修订；提交再次校验并幂等。同秒同价并不构成跨文件重复证据。冲销/替代与修订推进原子提交。取消发生在 commit 后应返回已提交回执，不能报取消并诱导重导。

<a id="install-and-data-locations"></a>

## 安装与数据位置（D-26）

所有路径、目录名、文件名、可执行文件名、凭据命名空间与应用标识一律使用小写 `delta`；产品显示名仍为 “DELTA”。正式版与开发版两个通道完全隔离，可以同时安装、同时存在数据。

| 通道 | 安装目录（M2 安装包，默认，可改） | 应用数据根（资料库） | 用户文件根（默认，可改） | 可执行文件 / 凭据命名空间 / 应用标识 |
| --- | --- | --- | --- | --- |
| `release` | `%LOCALAPPDATA%\Programs\delta` | `%LOCALAPPDATA%\delta` | `<文档>\delta` | `delta.exe` / `delta` / `io.github.chialecode.delta` |
| `dev` | `%LOCALAPPDATA%\Programs\delta-dev` | `%LOCALAPPDATA%\delta-dev` | `<文档>\delta-dev` | `delta-dev.exe` / `delta-dev` / `io.github.chialecode.delta-dev` |
| `source`（`cargo run`、`cargo test` 等未写入通道的构建） | 不安装 | 与 `dev` 相同，由数据目录锁避免与已安装的开发版同时打开；手工运行建议设 `DELTA_DATA_DIR=.local/dev-data` | 与 `dev` 相同 | `delta.exe` / `delta-dev` / 同 `dev` |

- **通道写入方式**：打包时以构建期环境变量 `DELTA_CHANNEL=release|dev` 写入（`option_env!`），未写入即 `source`；`source` 永远不使用正式版位置。
- **应用数据根**（`%LOCALAPPDATA%\delta[-dev]`）存放应用自己管理、需要一致性备份的内容：`manifest.json`、`delta.sqlite`（含 WAL 文件）、`raw/`、`imports/`（导入文件副本）、`attachments/`、`backups/`、`logs/`、`cache/`（可随时删除）、`settings.json` 与数据目录锁 `delta.lock`。附件与导入副本留在这里，是为了与数据库在同一次备份恢复中保持引用一致（AC-09）。
- **用户文件根**（`<文档>\delta[-dev]`）只放用户主动产出、可离开应用单独使用的文件：`exports/`（`delta export` 默认输出）与用户手动保存的报告。`<文档>` 是系统登记的“文档”文件夹（`SHGetKnownFolderPath(FOLDERID_Documents)`，可能被重定向到别的盘或 OneDrive），不能用 `%USERPROFILE%\Documents` 拼接。
- **不使用的位置**：不写漫游的 `%APPDATA%`（数据库大、含本机路径）；不在用户主目录建 `.delta`；安装目录只放程序文件，任何数据都不写进去；卸载只删安装目录，保留应用数据与用户文件。
- **覆盖与测试**：`DELTA_DATA_DIR` 对所有通道优先覆盖应用数据根，`DELTA_DOCUMENTS_DIR` 覆盖用户文件根（CLI 另有等价的 `--data-dir`）。测试与门禁必须同时把 `DELTA_DATA_DIR`、`DELTA_DOCUMENTS_DIR` 和 `LOCALAPPDATA` 指向本次运行的临时根，并在结束时断言临时根之外没有新建任何 `delta`、`delta-dev` 目录或文件。
- **失败行为**：位置不能创建或不可写时返回明确错误（含解析出的路径与来源：默认、环境变量或参数），不悄悄换到别处。数据目录锁 `delta.lock` 用 `LockFileEx`（`LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY`）独占，拿不到锁立即报错，不打开或创建数据库。
- **凭据**：`keyring` 条目的服务名以通道命名空间开头（如 `delta:tiingo`、`delta-dev:tiingo`），测试只用 `delta-test-<run-id>`；两个通道互不读取对方的凭据。
- **模型凭据隔离**：模型服务名为通道前缀加 `model`，account 由 `(library.id, model_connection.id)` 哈希派生；同通道不同资料库不得仅凭相同连接 ID 共用 secret。设置页不回填密钥，空字段保留原引用；写入配置失败恢复原条目。备份保留资料库 ID 与公开配置，不备份 secret；恢复后仍需当前机器对应的 keyring 条目。
- **改变位置**：用户改变应用数据根属于可恢复迁移（一致性备份 → 复制到新位置 → 校验清单哈希与外键 → 切换），失败保留旧位置；M1 只支持默认位置与覆盖，迁移命令随 M2 设置界面交付。

## 文件与恢复

应用资料库与测试目录分离，测试只使用合成数据与显式临时库。路径经规范化校验，附件/解压禁止绝对路径、`..` 和链接逃逸；扩展名不是内容验证。覆盖和删除前明确范围。

SQLite WAL 数据使用备份 API/一致性快照，不只复制主文件。备份清单含 schema、文件哈希和可选缺省行情；恢复到新目录校验后切换，失败不覆盖现库。Rust 会话转录与检查点在同一 SQLite 一致性备份范围内，记录关联的账本修订；凭据真值不进备份。

M2 删除预览显示资料库 ID、绝对目标、账户/笔记/附件/会话数量及文件/字节范围，执行要求输入完整 ID。关闭作业并释放锁后再次核对 manifest 与数据库身份、路径与链接；身份变化即拒绝。失败后原库仍完整时重新打开，否则明确从外部备份恢复。资料库之外的备份、导出和模型服务留存不在本地删除范围内。

Windows 包由独立 `release`/`dev` 构建产生，显式目标为 `x86_64-pc-windows-msvc` 并静态链接 CRT，覆盖 Cargo 编码 flags 的继承优先级；清单记录全部文件哈希与源码指纹。构建和安装均有界解析普通及延迟 PE 导入，拒绝依赖外部 MSVC 运行库，包内 `binary-imports.json` 保存 DLL 名称。安装校验 product/channel/version，随机 staging 验证完成后切换。卸载先验证全部安装身份、路径和哈希及 Windows 文件写入 lease，再删除受管文件；未知文件和资料库保留。正在运行的安装目录内 CLI 不能自卸载，使用解压包中的外部 CLI。

## 模型与工作进程

密钥通过 keyring 引用，只注入必要连接；不写命令行、公开配置或日志。配置 UI 只存字面值/受控环境引用，不开放执行命令来解析凭据的配置表达式。环境继承采用必要字段白名单，策略进程不继承模型或账户凭据。

首版业务工具只读、白名单、参数校验、范围检查；Rust 模型客户端不得加载编码工具、全局配置/项目指令或其他应用凭据；用隔离探针实测。全项目按 D-22 采用纯 Rust 实现，不引入外部脚本子进程，也不开放任意第三方未受控扩展。详见 [上下文状态](../design/context-state-management.md)。

模型发送前显示账户/期间/笔记范围；附件默认不发送。重新发送、自动压缩也受同一范围约束，撤权与关闭使运行失效。测试真实端点只使用已授权配置和数据范围；缺配置保留模拟测试结果并列 ACT，不索取聊天中的秘密。

## 诊断与验收

日志只记录 ID、错误码、耗时、版本和大小；不包含原始账户、笔记、完整 prompt、token 或 HTTP Authorization。脱敏不能依赖仅替换已知一条密钥。用户触发的诊断导出有可检查清单。

M2 审计写入只接收白名单 action 与时间、版本、字节/数量元数据，不持久化调用方消息正文；即使 secret 未在替换列表中或只有笔记片段，也不能进入日志。泄露扫描返回命中序号，不将秘密再次写入诊断文本。

必要回归：中断导入/迁移、重复提交、WAL 备份恢复、路径逃逸、密钥泄露探针、跨账户访问、取消后的晚到响应。失败先修代码；不能关闭断言制造通过。
