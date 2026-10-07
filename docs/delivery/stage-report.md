# R1 合并阶段报告

2026-10-07 · 当前计划 [R1 v1.2](stage-plan.md)。下列 A/B 原始记录是在误标 R2 v1.0 时执行，保留当时提交、命令、指纹和产物事实；其旧阶段指令已被末尾 A 合并审核取代。前半保留 Agent A 的 W00 记录；最新交付见下方“Agent B：W01～W06 实现与自检”。B 自检不等于 A 独立复核或 S1 产品验收。

## 基线与已做内容

阶段父提交 `8370bc3ff16ab2bdb45a916c60d4c69f55ed6a82`，分支 `codex/r2-workbench`；最终提交 SHA 在交付消息给出。没有修改 Cargo 依赖锁或金融/AI 服务实现。

| 内容 | 实现位置 | 结果与边界 |
| --- | --- | --- |
| 文档治理升级 | AGENTS、沟通协议、本地资料、登记表、导航、状态、决定、用户待办 | 单写入者、完整阶段交接、用户事项唯一入口；保留金融验收和旧回归 |
| 本机参考 | 仓库外约定下的 README/PROPOSALS/design | 记录已确认参考、GitHub commit/release、许可与范围；原图保留且复制哈希一致；不进仓库 |
| 首页复刻 | `dashboard.rs`、`theme.rs`、`pages.rs`、`main.rs` | 白灰橙双层工作台、TitleBar、搜索入口、现有服务页；未实现能力明确标记 |
| 图表修复 | `apps/desktop/src/chart.rs` | canvas 尺寸；按实际几何定位；边界夹紧，窗口外松鼠标结束拖动；网格/成交量/MA20 |
| 启动一致性 | `scripts/run_r1.py` | 不因 exe 存在就跳过源码新鲜度检查，运行前由 Cargo 检查构建 |
| A 后续计划 | `stage-plan.md` | W01～W06 桌面复盘闭环；继承 S1 及安全/金融门禁；B 尚未执行 |

## 验证范围

测试只使用合成库与本机受控条件；未连接真实行情、模型、券商或用户账户。未运行的 IME/DPI、干净机器和 GPU FPS 保持 not-run。

实际窗口暴露原图表 canvas 零高度，已修复；固定 1200 px 坐标不适合多图面板，已改为每幅画布真实尺寸并增加边界回归。重建时运行中的演示 exe 被 Windows 锁定，关闭后重建成功。

Windows 截图检查已看到双层卡片、合成 K 线、成交量、MA20 正常显示。之后增加价格轴、调整下排高度和字体，用户按物理 Esc 停止 Computer Use，随即停止全部窗口操作；最后微调只验证编译/自动测试，不宣称已截图复核或用户视觉批准。H-01 保留 not-run。

## 文件整理与接续

没有删除仍服务未关闭验收的旧计划/用例/报告；旧 prompt 加历史标记，当前导航指向 R2。未跟踪 `workers/agent/` 原有两文件不删除、不构建、不提交；缓存/临时库/历史包和日志未做无关清理。reference 不属于 Git 仓库。

用户事项仅在 [USER-ACTIONS](../../USER-ACTIONS.md)：H-01、ACT-01/02/03/04、Q-01/02。工程后续在 [集中事项](../decisions/open-questions.md)：AI-01/UI-01、原 OSS 与 LOOP。没有新增需要用户重复确认的参考提案。

结论：文档和视觉实现可审查；业务服务接入待 B 执行，S1 未验收。下一步使用唯一计划中的完整 B prompt。

## 最终自动检查与产物

| 命令 | 实际结果 |
| --- | --- |
| `python scripts/check_docs.py` | passed：58 份 Markdown、61 项需求，登记/相对链接/追踪有效 |
| `git diff --check` | passed |
| `cargo fmt --all -- --check` | passed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | passed |
| `cargo test --workspace --locked` | passed，全部执行套件无失败；原有代理子进程探针仍由父测试显式启动，保留其 ignore 标记 |
| `cargo build --locked -p delta-desktop` | passed，生成当前 debug exe |
| `python -m unittest discover -s tests/python` | passed：15 项，含 3 项文档反例 |
| `python -m unittest discover -s workers/python/tests` | passed：11 项 |

新增桌面交互用例验证：点击首页 ETH 样例后两图同步、1M 范围联动、Library 自选没有被样例污染、可返回服务搜索。新增图表用例验证不同面板宽度定位一致、左右边界及拖动。

构建有已有增量缓存重命名“拒绝访问”提示，以及 MSVC 创建导入库的 linker stdout 提示；各命令退出 0，不视为产品失败，也不声称已消除环境提示。

本次按实际桌面缺口将相关 FR 的完整实现状态改为 in-progress，并增加 implementationScope，保留原服务验证证据；这是纠正交付范围，不是回退已完成的服务实现。未改变 FR/AC 和金融计算合同。

源码指纹（排序的 `路径:SHA256`，覆盖本次桌面/脚本/新测试与 Cargo 清单/锁）：`3c170809b19d7a13b51e8dca544fe1e22eda2c628d6eea005f6243d59d2f951e`。

产物 `target/debug/delta-desktop.exe` SHA256：`b6e289bc4a68821382ef51d255c7346388866a98f0afc9c785f1c236ed1c11e2`。逐文件指纹及脱敏检查摘要在忽略的 `.local/reports/r2/verification.json`，不随产品分发。没有重新生成 release 包，旧 dist/r1 不是本轮 UI 包。

## Agent B：W01～W06 实现与自检

2026-10-07。从交付基点 `48a6a18c54639c1048b0b92b95e1c67121df966d` 连续实施。阶段父提交仍为 `8370bc3ff16ab2bdb45a916c60d4c69f55ed6a82`，分支 `codex/r2-workbench`。保留 A 的首页构图、样例隔离、图表几何修复和全部金融/安全测试，没有升级 Cargo 依赖或引入第二个模型运行时。

### 工作包与实际行为

| 包 | 实现 | 实际验证与范围 |
| --- | --- | --- |
| W01 | `business.rs`/`tasks.rs` 分离输入、呈现与后台操作；持久库与显式 demo；v6 配置；最近库原子有序写入；切库代次与关闭保护 | 多库/重开、损坏/外来库字节不变、旧回调拒绝、切库清空连接字段；真实原生窗口首次启动/创建空库可用；设置 IANA 时区用于流水显示 |
| W02 | 账户五类型、命名组合/成员去重，映射 CSV 预览、原始/规范行、行选择与疑似重复策略、确认前重读核验、更正/核对/总览 | 桌面真实输入/点击驱动服务；独立样本现金 1438、持仓 AAPL 6、净资产 2068、期间损益 68；变更文件/映射拒绝；旧幂等/重叠/陈旧判定/取消回执回归保留；命名组合重启后仍有效且不重复计入 |
| W03 | OHLCV 文件先验证后原子导入；不可变版本；场所/标的/日期日线、MA20 隐藏预热、成交/笔记标记、固定 ChartContext | 独立合成行情文件；相同版本幂等/变更拒绝、坏 OHLC 原子失败、不同交易场所/空态/日期边界；1月20日窗口 MA20 含此前19根，缩放/平移不能进入预热前缀；原面板宽度交互回归保留 |
| W04 | 持久 Markdown 文本编辑、显式保存/检索、成交数量关联、图表上下文与修订证据 | 故障注入后缓冲保留；乐观修订冲突拒绝；旧证据正文不变；旧修订可打开图表；未保存时禁止切库/关闭；不再使用静态 Vec 充当笔记 |
| W05 | 协议/连接/OS 凭据引用、明确账户授权、已保存范围发送、聊天/历史/取消/撤销与证据入口 | 真实 Rust HTTP/SSE 客户端+AgentRuntime+ProductionHost+SQLite 的桌面受控工具闭环；只有凭据源为合成；默认拒绝全部账户、保存不含秘密、缩小范围/撤权含压缩；历史证据从持久工具消息恢复。真实供应商测试 not-run |
| W06 | 备份/可读导出、新目录恢复；清单/路径/链接/哈希/数据库/附件验证后原子发布；当前验证器和稳定包 | 失败恢复不覆盖源库、附件/会话/证据一致；桌面备份→恢复→切换；零测试/缺模式/错阶段父提交/陈旧源码均阻断验证或打包；本地 release 包启动验证与许可盘点 |

`tests/fixtures/r2-ledger.csv` 和 `r2-ohlcv.json` 为独立合成输入，不含真实数据。新自动用例在 `apps/desktop/src/workbench_tests.rs`、`crates/delta-infra/tests/r2_workbench.rs` 和 `tests/python/test_verify_stage.py`；机器映射为 [合并映射（保留原编号）](r1-stage-cases.json)。输入、预期、实际匹配测试和源码版本由验证报告保存。

### 自检发现与修复记录

- 受控端点最初漏发工具 scope，被宿主拒绝；修正测试端点的合法工具参数，未放宽宿主合同。工具结果经实际数字/证据校验后，桌面点击打开返回证据。
- Textarea 测试入口未落到真实编辑控件，增加可访问焦点包装；写失败的实际缓冲测试随后通过，未改成绕过输入的状态赋值测试。
- 确认 CSV 时补核验原文件/映射/账户，字段变化清空预览；提交后的取消仍报告实际提交结果，避免引导重复操作。
- 修复清空 URL 未移除旧连接、切库残留模型字段、历史结果误把 result ID 当 JSON、授权扩张早于成功落盘；缩小授权先失效，新增授权只在持久化成功后生效。
- 补最近库有序写入、未保存关闭保护和图表预热边界；保留服务失败回执及输入，不把刷新失败改写为提交失败。
- 交付检查补银行/钱包与命名组合，组合内成员去重，不改变资金事实或模型授权。
- 第一次打包期间修改了源码指纹实现，打包器按设计拒绝产物并退出非零，未发布旧产物。最终指纹按 Git LF 文本规范计算，支持 Windows 工作区与干净检出的换行一致性。

### 最终验证、源码与产物

以下由最终机器报告回填；旧 W00 指纹/产物只代表 A 当时成果。

| 命令 / 范围 | 最终结果 |
| --- | --- |
| `python scripts/check_docs.py` | passed；58 份 Markdown、61 项需求 |
| `git diff --check` | passed |
| `cargo fmt --all -- --check` | passed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | passed |
| `cargo test --workspace --locked` | passed；实际执行 177 项（Rust 含父测试启动的子进程探针） |
| `python -m unittest discover -s tests/python` | passed；21 项 |
| `python -m unittest discover -s workers/python/tests` | passed；11 项 |
| `cargo build --workspace --locked` | passed |
| `python scripts/verify_stage.py --stage R2 --plan-version 1.0` | passed；W01～W06 自动范围全部通过；R2-M01/L01/L02 为 not-run |
| `python scripts/package_stage.py --stage R2 --plan-version 1.0` | passed；Cargo release 构建；所有包文件哈希复核通过 |

最终源码指纹：`fdc162ff722c86ca577b8c505d5d5f2174e8b43357039b97c4c2399c549dfe9f`（75 个文件；算法 `r2-source-v2: sorted path:sha256, Git LF text, code/scripts/fixtures/current case mapping`）。

debug exe SHA256：`af0aadcbc6f5c09d8a56c5bd270d0119c1287abc7c09398f24c1f19b323ac007`。

本地稳定包：`dist/r2-fdc162ff722c-8c5f40bb13c0`。release exe SHA256：`8c5f40bb13c04c311ac054fea1d8e106fb0bb976726aa2c8a88a646f01f34ac0`。包内 `Start-DELTA.cmd` / `Demo.cmd`、`README.txt`、`samples/`、`VERSION.json` 和许可文件可供本地复核。

包内 1471 个文件由 VERSION.json 记录 SHA256（VERSION.json 本身不自哈希）。报告在 `.local/reports/r2/verification.json` / `package.json`，最终完整 Git SHA 从本次交付消息与 `git rev-parse HEAD` 获取；报告记录 pre-amend 基点，避免自引用提交。

Windows 本机图形窗口与本地包可启动；已有增量缓存拒绝访问/链接器 stdout 提示不影响退出码，未声称消除环境提示。最终叙述性文档同步后另行重跑文档与 diff 门禁。

R1 的 39 个 Cargo 测试命令映射对应断言由本次 workspace 运行继续核验（按过滤器及集成套件识别），原零匹配、金融守恒、取消/压缩/撤权、证据寻址、字面搜索、代理回环等断言不删除。R1-A-25 的旧父提交/报告存在性自检换为当前阶段配置校验；R1 脚本保留历史作用域，不能再套用其文档框架父提交结论。R1-A-12/19/24 仍含未测设备部分，自动通过不覆盖那些声明。

原生窗口证据使用 computer-use 插件：观察初始设置/空库、显式样例首页、文件日线及固定成交证据；本地截图索引为 `.local/reports/r2/screenshots/`。`final-demo-home.png`、`final-file-chart.png`、`final-event-evidence.png` 是命名组合补充前的同一图表/首页实现，最终组合由自动桌面输入回归覆盖；旧 `release-startup.png` 属于中间包；`final-release-startup.png` 已记录最终 `r2-fdc162ff722c-8c5f40bb13c0` 包成功启动设置页，随后正常关闭。截图不等于 H-01 人工批准，也不构成 IME/DPI 或 GPU 性能证据。

### 限制与集中事项

- S0/S1 不宣称完成。H-01、ACT-01/02/03、Q-01/02 的状态与操作仅在 [USER-ACTIONS](../../USER-ACTIONS.md)；未获得新的真实数据使用或外部操作授权。
- 当前 CSV 入口为 UTF-8、九表头、带时区日期；现金更正为已提供的桌面更正入口，其他事件继续按服务合同导入。日线 raw 文件路径可用，真实来源和数据许可仍未验收。
- 每次图表载入最多 20000 根所选日线和 19 根隐藏预热；笔记检索展示最近 1000 条。正文为 Markdown 文本编辑，不提供富文本预览；训练/策略继续按 LOOP-01 在 S2/S3 实施。
- Windows 本地 release 启动已测；无开发工具干净机、签名安装器、IME、125%/150%、交互 P95 和 GPU 50 FPS 未测。可选 Python/TA-Lib 运行时未捆绑，账本/图表/笔记/恢复路径无需该 worker。
- 许可盘点是 Cargo 完整解析图，包含非目标/dev/传递依赖，不能等同二进制实际链接集合；913 个第三方条目中 91 个没有找到包内许可证文件。OSS-06 承接逐项核验，Q-02 仍阻止外部分发。AI-01/UI-01 已完成本轮实现和自检，待 A 独立复核；其他 OSS 与 LOOP 状态仍在集中台账。S1 的完整自选分组、指标参数、笔记标签/自动保存/模板/附件管理及现金外事件编辑在 UI-02 承接，逐需求范围已如实保留；这些工程工作不转为用户事项。

金融计算与工具公共协议未改变，因此需求/AC 正文无需重新定义；已同步实际流程、数据封装、架构、质量门禁、状态和逐需求实现范围。没有复制另一套金融公式或新增人工审批。`workers/agent/` 保持原有未跟踪状态；没有清理无关缓存/本机资料；仅 amend R2，不改 R1、框架和阶段父提交，不 push/PR/合并/发布。

### 历史 B → A prompt（已失效，勿执行）

```text
请作为 DELTA Agent A 对 R2 v1.0 集中复核。先读 AGENTS.md、docs/delivery/status.md、stage-plan.md 和 stage-report.md（Agent B 章节），并按规则寻找上级 reference/delta/README.md。工作分支 codex/r2-workbench；阶段父提交必须仍是 8370bc3ff16ab2bdb45a916c60d4c69f55ed6a82。使用交接消息的完整 SHA 与 git rev-parse HEAD 核对，不退回初始 A 树。
本轮 B 已完成 W01～W06 的桌面/服务闭环和自检。按报告的最终源码指纹与包 VERSION.json 校验版本，重点审查：库/范围/任务代次隔离、CSV 确认前文件身份与陈旧判定、重复与金额守恒、命名组合去重、日线版本与 MA20 预热边界、笔记写失败缓冲/旧修订证据、设置落盘与撤权含压缩、清单/附件/会话恢复、最终包新鲜度。保留 A 首页构图、全部原金融/安全回归。
运行 python scripts/verify_stage.py --stage R2 --plan-version 1.0；针对疑点补独立反例，必要时重建本地包。不要用旧 verify_r1.py 的父提交自检判断 R2。自动/受控通过与人工/live not-run 必须分开，不把 S1 改成已验收。用户事项仅更新 USER-ACTIONS.md，工程问题在 open-questions.md。workers/agent/ 原样保留，不构建、不提交。
在同一 stage-report 追加 A 独立复核结论、问题编号/证据、处理与遗留范围，必要修复直接 amend R2，不 amend R1/框架、不改阶段父提交、不做未授权外部操作。交付完整 SHA、更新后的源码/产物指纹和一个首选下一步；有返工时给引用 R2 v1.0 的完整 B prompt。
```


## Agent A：R1 v1.2 合并与集中审核

2026-10-07。依据用户明确纠正，原 R1 与误标 R2 合并；当前仍是 R1。原基础交付 `8370bc3ff16ab2bdb45a916c60d4c69f55ed6a82`、桌面交付 `38ae4e8da766efd83baa270262403d1aee7ab626` 的全部内容保留；只有本节列明的局部修复与阶段/远端文档调整。历史报告的 R2 命令、指纹和截图描述仅代表当时观察，旧 prompt 失效。

### Git、范围与文件归属

合并父提交为 `a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe`；分支 `codex/r1-workbench`。原始空提交 `8329f638536e4f303764045a2f10a2c1ab31fcd3` 和文档框架均保持 SHA 不变，已原样推送 [origin/main](https://github.com/chialecode/delta/tree/main)。基础提交的 GitHub Documentation 检查成功。阶段 push 前压为一个 commit；后续已推送修复不擅自强推。

本地合并前全部 refs 已保存 `.local/review-r1/pre-consolidation.bundle`；其他本地分支不改写。`workers/agent/` 保持未跟踪，不读取业务内容、不构建、不提交。旧用例/报告/脚本保留原断言；原桌面用例配置改为 `r1-stage-cases.json`，其中 R2-* 历史 ID 和样本/测试名保留，避免与基础 R1-Wxx 重号。当前唯一计划、登记表、状态、需求追踪与验证/打包默认值统一到 R1 v1.2。

### 审核发现与处理

本节 F 编号只在本报告内唯一，原 r1-review.md 的 F-01～F-25 仍保留。

| 编号 / 严重度 | 触发、实际与预期 | 位置 / 契约 | 处置与复验 |
| --- | --- | --- | --- |
| F-01 / P1 / 已修复 | 向只有 manifest.json 或附件、尚无 library.sqlite 的非空目录备份；旧代码会写入新数据库并覆盖同名清单/附件。预期在任何备份写入前拒绝，既有文件字节不变 | `crates/delta-infra/src/sqlite/app.rs` 的 backup_to；FR-OPS-01、NFR-06，数据安全的覆盖范围约束 | 增加非空目录前置检查；独立回归 r2_w06_backup_rejects_nonempty_destination_without_overwriting 验证拒绝、清单/附件字节不变、无新数据库，同时空目录仍成功；定向与全套通过 |
| F-02 / P1 / 待 B 实施 | 原 R1 v1.1 承诺完整 S1/P0；实际 journal_page 仅显式保存 Markdown，缺标签/自动保存及完整模板/附件管理，其他 UI-02 能力也未完成。旧交付把这些留到另一轮，不能据六个桌面包通过判定完整 R1 完成 | `apps/desktop/src/business.rs` journal_page / chart_page，traceability implementationScope；FR-JRN-01/02/04、FR-MKT-01/02 等原 P0 | 在同一 R1 v1.2 §8 集中收口，唯一能力清单维护 UI-02；完整 R1 为 changes-requested，draft PR 不声明可合并；完成后补真实控件/重启/失败/修订测试并回交 A |

A 已读代码核对 `tasks.rs` 的 CSV 原文件/映射/账户重验、提交后回执、后台 snapshot，`business.rs` 的库 epoch/serial、脏笔记切库保护、配置保存前缩权/成功后授权、AI 代次，`sqlite/workbench.rs` 的持久配置/日线版本与预热，`app.rs` 的备份/新目录恢复清单、哈希和附件引用。结合旧金融/模型受控矩阵与本轮实际 workspace 复验，已实现范围未发现本节之外需立即修复的问题；这不是对未实现能力或真实环境的通过声明。

### 本轮实际验证

统一命令 `python scripts/verify_stage.py --stage R1 --plan-version 1.2` 退出 0：文档 58 份/需求 61 项、diff、fmt、clippy、Rust workspace 178 项、Python 21 项、worker 11 项及 workspace build 全部通过。原 39 个 Cargo 命令映射继续有通过的断言；原人工缺口、桌面 manual/live 三类仍 not-run。首次文档同步过程中发现旧 r2-cases 链接，已修正后重新通过文档及 Python 反例检查。

测试基点 `b36f1a5671fc3efd6c6d0cb23321a6224b152d06`；源码指纹 `91e8e8465a7e97c2769220a8581649fc957304911db028b2570068cd2d4fbdea`（75 文件，stage-source-v2，规范化 Git LF）。debug exe SHA256 `15891ea92a42a60085a4dd4fb0cbc5ab47b10de9763e9a134442354dee7d5009`。机器报告在 `.local/reports/r1/verification.json`；叙述文档不入源码指纹，最终同步后单独复跑文档门禁。

本轮没有重新操作原生窗口，旧截图保留对应版本限制；本机窗口、IME/DPI、干净机、真实模型和真实账户不能由上述自动测试代替。未引入依赖、未改变金融公式或工具公共协议，无需重定义需求/AC 正文；已同步交互中备份失败行为。许可与真实数据条件继续按 OSS/ACT/H 分开维护。

本轮 `python scripts/package_stage.py --stage R1 --plan-version 1.2` 成功；本地包 `dist/r1-91e8e8465a7e-ef3c048b9daa`，release exe SHA256 `ef3c048b9daaa0c244002eb1abe516e9bd31c3575d54275b5d543e0b997f7403`。包中 1471 个文件逐个复算哈希全部一致。许可盘点仍为 913 条，其中 91 条未找到包内许可文件（OSS-06）；本轮未重新启动该 release 窗口，不能套用旧包启动截图。

### 结论与 B 接续

B 已交付基础服务和六包桌面闭环；A 已完成本次合并审核并修复 F-01，完整 R1 因 F-02 为 changes-requested；S0/S1 未验收。首选下一步是 B 在同一阶段完成 UI-02 后回交 A，不开启 R2。阶段分支推送和 draft PR 属于当前授权；PR 合并、发布、强推没有包含在授权内。

```text
请作为 DELTA Agent B 继续 R1 v1.2。仓库为当前 DELTA 工作区，分支 codex/r1-workbench，稳定阶段父提交 a1180cfb5f0a0051c1fc3f14ac4db7d8f06691fe；以本次交付完整 SHA 和 git rev-parse HEAD 核对现场，不回退旧树。先读 AGENTS.md、docs/delivery/status.md、stage-plan.md 和 stage-report.md 的最新 Agent A 合并审核。
原 R1 与误标 R2 已合并，当前仍 R1。按计划 §8 连续完成 F-02 / UI-02 全部工程范围；原 R1 v1.1 的 S1/P0 合同仍有效。保留 F-01 非空备份目录保护和全部既有金融、隔离、撤权、恢复断言。workers/agent/ 原样保留、不提交。真实服务/设备条件未满足保持 not-run，不转交用户补代码。
执行统一 R1 验证和打包，补控件、重启、失败、自动保存并发、附件路径与修订证据。已推送后追加集中修复，不 amend/强推已共享提交；保持同一 PR。若门禁的单提交父校验阻止合法追加修复，按 Git 正本扩展为验证稳定父提交以来的同分支线性历史并补负例，禁止删去基线校验。更新唯一计划/报告/追踪/台账和源产物指纹，回传新完整 SHA 与给 A 的复核 prompt。push/PR 沿用 ACT-04 授权，合并/发布另需明确授权。
```
