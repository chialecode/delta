# 验证与交付门禁

只声明实际执行结果。文档、Cargo 与 `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6` 检查已建立；当前运行结论和源码指纹只维护在 [阶段报告](../delivery/stage-report.md)，本规范不预先宣称某版本通过。

## 1. 当前已存在并可运行的检查

在仓库根执行：

- `python scripts/check_docs.py`
- `git diff --check`
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6`（及显式开启在线探针的 `--live`）

要求 Python 3.12+ 标准库，无第三方包依赖。`check_docs.py` 校验：

1. 全部自有 Markdown 均已在 [document-registry.json](../governance/document-registry.json) 登记，且 `kind/status/owner/scope/reviewed/basis` 合法；
2. `currentStage` 与 `retireAfter` 按里程碑序（`M1 < M2 < M3 < M4 < M5`）一致，防止跨阶段遗留过程文档；
3. 全仓 `active`/`draft` 正本无旧 `S0`–`S4` 或 `R1`–`R2` 阶段名回流；
4. 内联相对链接、文件存在性与标题/显式 HTML 锚点有效；
5. `CLAUDE.md` 仅包含 `@AGENTS.md`；
6. [需求文档](../requirements/product-requirements.md) 的 61 项 FR/NFR、[实施路线](../engineering/delivery-plan.md) 的 29 项 AC 与 [traceability.json](../delivery/traceability.json) 双向完整映射，且正本中引用的 `FR-*`/`NFR-*`/`AC-*`/`POC-D*` 编号均真实存在；
7. 若存在阶段用例清单（`docs/delivery/m1-stage-cases.json`），自动校验用例字段、工作包引用及阶段需求覆盖完整性；
8. **公开内容与秘密/本机路径门禁**：扫描工作树（跟踪文件与未忽略的未跟踪文件）及 Git 暂存区，拦截本机绝对路径（Windows 盘符路径或 Unix 用户目录路径）、本地外部参考路径泄露（`reference/delta`、`external/github.com`）、私钥/API Token、内嵌凭据 URL 以及禁止入库的 `.env`/`.sqlite`/`.db` 等敏感文件。

## 2. M1 产品工程必需门禁（已建立）

<a id="fast-gate-order"></a>
<a id="bounded-validation"></a>
<a id="temp-cleanup"></a>

| 变更范围 | 要求与证据 |
| --- | --- |
| Rust 核心与数据平面 | `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --locked` |
| 美股/ETF 与加密数据源适配（纯 Rust） | 免 Key 公共源（欧易 OKX 公共行情、Binance 公开行情镜像与归档、Coinbase/Bitstamp/Kraken 稳定币、Frankfurter、Yahoo Chart v8、东方财富 `push2his`）与官方 API（Tiingo）录制报文回放、分页游标事务推进、限流退避、拆股/分红交叉核验与缺失诊断测试；可开关的 live 探针结果单独记录 |
| 交易日历与时区 | `XNYS`/`XNAS` 节假日、13:00 ET 半日市、`America/New_York` 夏令时及加密 `24×7` UTC 切分对照测试（AC-21） |
| 账本、FIFO 与多币种估值 | AC-01～07、AC-22 独立手算期望对照（含主力富途牛牛 CSV 与欧易 OKX 现货 CSV）、Decimal 精度/守恒性质测试、`USD`/`USDT`/`USDC` 独立估值验证 |
| 存储与备份恢复 | SQLite 迁移幂等、故障注入无半笔事件、Backup API 一致性快照与哈希核验、新目录一致性恢复校验（AC-09）、导出/删除预览与日志脱敏（AC-20 子集） |
| 后台作业隔离与技术指标（纯 Rust） | `delta-app` 后台同步/连接器作业超时、取消与异常响应隔离恢复（AC-16）；Rust 指标手算对照与未来哨兵截断（AC-11） |

B 在 M1-W06 建立纯 Rust 统一验证入口 `cargo run -p delta-cli -- verify-stage --stage M1 --plan-version 2.6`，按**由快到慢快速失败顺序**串联执行：

1. 文档与公开卫生门禁（`python scripts/check_docs.py`、`git diff --check`）；
2. 静态检查（`cargo fmt`、`cargo clippy`）；
3. `cargo test --workspace` 执行 core/data/app 测试，实际包顺序由 Cargo 决定；
4. SQLite 百万行 POC、跨进程 keyring/独占锁、超时进程树与管道收口、初始化库的非 TTY 删除/递归范围与静音崩溃；
5. 从捕获的命名测试结果生成验收映射，缺测试或未执行不能写 pass；
6. 显式开启的真实网络探针（`--live`，与离线确定性门禁分开统计）。

**有界验证与零临时残留约束**：

- 所有测试、HTTP 请求和后台作业必须设置显式超时与资源上限，严禁无界等待或无限重试。
- 测试使用系统 TEMP 下的隔离目录；stage gate 把 `DELTA_DATA_DIR`、`DELTA_DOCUMENTS_DIR` 与 `LOCALAPPDATA` 指向本次临时根，结束检查门禁根与本次新建的已知测试目录是否残留。真实 `LOCALAPPDATA/delta[-dev]` 只观察目录元数据，不能读写个人金融数据；该观测不等于全系统文件追踪（D-26，见 [安装与数据位置](data-and-security.md#install-and-data-locations)）。
- 机器可读验证报告与原始日志输出至忽略目录 `.local/reports/m1/`，仅将脱敏摘要回写至 `docs/delivery/stage-report.md`。
- 每条 docs/diff/fmt/clippy/test 命令保存 stdout/stderr/退出码/是否超时；每条命令持有独立 Windows Job，直接子进程退出或超时后先收口后代进程，再回收输出管道。错误或 POC panic 也写 failed 报告，不能早退丢失证据。报告绑定 Rust/Cargo/工具链/夹具/检查器源码 SHA-256，执行后核对未变。命名测试映射的 elapsed_ms=0 表示未单独计时，实际套件耗时在 command 记录中；结果状态只用 passed/failed/blocked/not-run。POC-D06 独立历史实测在证据文档保存，本次 gate 未重跑则标 not-run；私有服务同样 not-run。

<a id="foreground-quiet"></a>

**前台无干扰约束（[D-25](../decisions/confirmed-decisions.md)，适用于产品测试、`verify-stage` 与 Agent 开发期命令）**：测试和门禁在后台运行时，不得弹出窗口、抢走焦点、发出声音或明显拖慢前台工作。

| 干扰来源 | 规则 | 自动证据 |
| --- | --- | --- |
| 子进程控制台窗口闪现（从无控制台的 Agent/IDE 启动时最常见） | 产品与门禁代码只通过统一的静默进程助手启动子进程：Windows 上设置 `CREATE_NO_WINDOW`，`stdin` 为空，`stdout`/`stderr` 捕获到报告目录；`clippy.toml` 以 `disallowed-methods` 禁止在助手模块外直接调用 `std::process::Command::new` | clippy 零警告；窗口哨兵 |
| Windows 防火墙“允许访问”对话框 | 回环测试服务只绑定 `127.0.0.1:0`（随机端口），禁止绑定 `0.0.0.0`、`::` 或固定端口；统一由测试支持模块提供 | 门禁扫描测试源码中的非回环绑定，命中即失败 |
| 崩溃/错误报告对话框（WER“已停止工作”、找不到文件/磁盘错误框） | `verify-stage` 启动即设置 `SetErrorMode(SEM_FAILCRITICALERRORS \| SEM_NOGPFAULTERRORBOX \| SEM_NOOPENFILEERRORBOX)`，由全部子进程继承；崩溃只记录退出码与日志 | 注入一个崩溃子进程的自测：无窗口出现且报告为 failed |
| 交互式提示阻塞或弹出 | 测试与门禁全部非交互：CLI 确认类命令在非 TTY/空 `stdin` 下直接以错误退出（测试显式传 `--yes`）；git 子进程设置 `GIT_TERMINAL_PROMPT=0`、`GCM_INTERACTIVE=never`；凭据测试只操作 `delta-test-<run-id>` 前缀条目并清理，不触碰真实条目 | 空 `stdin` 下确认命令在超时内退出的测试 |
| 打开浏览器、资源管理器或其他应用 | 产品命令默认不自动打开任何文件或网址（打开动作需显式 `--open`），测试与门禁禁止使用；Agent 开发期同样不运行 `start`、`explorer`、`gh … --web` 等会切到前台的命令 | 窗口哨兵 |
| 声音 | 不调用系统提示音或通知；终端输出对外部数据（行情字段、账单文本、错误信息）中的控制字符（含 BEL `\x07` 与 ANSI 转义）转义后再打印，既避免响铃也防止终端注入 | 含 `\x07` 与 ANSI 序列的夹具输出测试 |
| 抢占 CPU 拖慢前台 | `verify-stage` 将自身设为 `BELOW_NORMAL_PRIORITY_CLASS`（子进程继承），并发构建/测试默认 `max(1, 逻辑核数 - 2)`，可用 `DELTA_GATE_JOBS` 调整；live 探针保持默认关闭 | 报告记录实际优先级与并发数 |

**窗口哨兵与进程树收口**：`verify-stage` 将当前进程及继承的子进程放入 Windows Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），执行静音命令期间用 `EnumWindows` 轮询该 Job 的可见顶层窗口；出现窗口即判 failed，报告保存 PID 与标题。实际优先级通过系统查询记录。该哨兵不观测其他进程的全部桌面窗口。依赖为 `windows-sys`（MIT OR Apache-2.0，仅在 Windows 目标启用）。

M2 起的桌面测试沿用同一约束：界面逻辑测试使用 GPUI 测试平台，不创建真实窗口；真实窗口冒烟测试只在显式开关下运行，窗口创建时不激活、不置前，结束后由 Job 收口；该类测试的哨兵改为检查“焦点是否被切走”，在 M2 计划中细化。

## 3. 阶段完成判断

| 判断 | 必须满足的条件 |
| --- | --- |
| B 实现与自检完成 | M1-W01～W06 全部可执行工作包与自动检查通过，无工程缺口转交用户 |
| A 技术审核通过（Agent 侧阶段完成） | A 核对实际 diff、代码、测试与数据质量边界，工程阻断为零；按 D-18 转入 [USER-ACTIONS](../../USER-ACTIONS.md) 请求具体远端授权 |
| 真实私有账户/Key 验收 | 用户按 `ACT-03` 提供真实配置后独立验证；未测前保持 not-run，不冒充已通过 |
