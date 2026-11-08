# M2 v1.0 阶段验证与交付报告

> 2026-11-09 归档说明（Agent A，M3 开始时阶段清理）：本报告原位于 `docs/delivery/stage-report.md`。M2 已按 `ACT-09` 授权经 [PR #2](https://github.com/chialecode/delta/pull/2) squash 合并为 `main` 提交 `5f30f6e1ddf6383f25317d5c87acd21934e75ed9`（树与审核后的阶段提交 `84519a5` 一致，必需检查 `docs`、DCO 通过）。正文保持合并时内容，只修正归档后的相对链接；有效决定与未关闭事项已回写正本。

更新：2026-11-09；角色：Agent B（实现）/ Agent A（集中审核）；唯一计划：M2 v1.0（PR #2 版本 `docs/delivery/stage-plan.md`，Git 历史归档），用例见 [m2-stage-cases.json](../delivery/m2-stage-cases.json)。分支 `codex/m2-desktop-workbench`；固定父提交 `0370b7e9103d0172a7ab47613ea253949d6a9b8b`；复核基点 `6dfa28c69c11d2764bc2233e407eb4827dd4812d`。最终被测源码指纹为 `e2ba2bdc3bc070f3620cd2fb1beb92f2620495a708d04b9ad91c7b5cf4857a71`；amend 后完整 SHA 以实际 B→A 交接与 `git rev-parse HEAD` 为准。未跟踪 `workers/agent/` 未读取、构建、修改或暂存；未执行远端操作。

## 当前判断

**B 实现与自检：完成；A 技术审核：通过（见 [A 集中技术审核](#a-集中技术审核2026-11-09)）；产品验收：未完成。** M2-W01～W08 的可执行工作与 F-01～F-07 修复已完成，最终静态 CRT 包、M2 全门禁和 M1 v2.6 回归在相同源码指纹下通过；当前无工程阻断。原测试不足以支持实际界面、帧性能、安装通道和权限隔离的结论，已由本轮真实证据替换。真实模型、个人账单和人工检查仍按下文保持 not-run。

## 复核发现与修复

| 编号 / 严重度 | 触发、实际与预期 | 修复与复验入口 |
| --- | --- | --- |
| F-01 / P1 / resolved | 导航无事件，各页同内容；导入/笔记/AI 只存在直接调用的状态方法；同步是计时任务。预期实际控件使用后台宿主并展示真实状态 | `controller.rs`、`ui_forms.rs` 建立实际工作流；保存期间保留新输入、失败保留缓冲与旧范围；真实同步 phase/记录数与在途取消。M2-A-07～12/16、实际点击/输入/导入提交与阻塞回环 HTTP 取消回归通过 |
| F-02 / P1 / resolved | 冒烟没有 GPUI 窗口；格式化/切片计时当帧时间，p99 复制 p95；默认 AC-26 宣称 passed | `ui_verification.rs` 创建真实隐藏 HWND，显式初始化尺寸并执行 WM_PAINT；100k 图表/表格 20 预热＋300 实测帧各有原始样本和绘制计数；5 个新进程首屏及 300 次工作台交互。门禁重算 p50/p95/p99，缺显式验证保持 not-run；最终 release 包 M2-K02/K06/A20 通过 |
| F-03 / P0 / resolved | 冒烟/基准递归删除调用方 data-dir；找不到桌面二进制时复制 CLI，两个通道复制同一 source 构建 | 验证只创建并清理随机子目录，核对父目录/身份/链接；真实 `DELTA_CHANNEL=release/dev` 独立构建、build-info 与完整文件哈希，安装随机 staging 校验后切换；卸载先核对全部所有权、哈希和 Windows lease，自卸载在删文件前拒绝。M2-K04、M2-A-17、两包 ZIP 全文件哈希与未知文件/数据保留通过 |
| F-04 / P1 / resolved | 中文数字/正负匹配失真，keyring 只用进程缓存；同连接 ID 跨库密钥覆盖；历史与预算守卫不完整 | 宿主结构化报告＋显式引用校验、规范化保留正负；工具投影重算 result_id/证据；一致内存快照、完整连接/journal 指纹、累计预算和原子压缩；keyring account 绑定资料库身份，设置恢复/空密钥/失败回滚/丢失条目重配。M2-A-01/13～16 与跨库凭据回归通过 |
| F-05 / P1 / resolved | 图表缺账户 scope，笔记无期间/保存截止；复权和缓存可能使用未来数据 | 冻结零版本/空账户；逐项校验 note owner、review_date、事件/图表锚点及真实保存 cutoff；复权过滤未来公司行动，缓存绑定内容/指标/版本/截止，恢复日期窗口与锚点。M2-A-01/10～12 与范围/截止/缓存回归通过 |
| F-06 / P1 / resolved | 日志依赖调用方提供已知 secret 和完整正文，未列密钥/正文片段可落盘，泄露诊断会回显命中秘密 | 审计改为白名单 action 和时间/版本/大小元数据，舍弃调用方正文；扫描只返回命中位置序号。M2-A-17 中未知 secret、正文片段和诊断回显回归通过 |
| F-07 / P1 / resolved | 真实两通道包的 desktop/CLI 均导入 `VCRUNTIME140.dll`，包未带运行库，开发机已有安装掩盖干净机器失败 | 显式 MSVC target＋静态 CRT，覆盖 Cargo 编码 flags 优先级；构建/安装有界检查普通及延迟 PE 导入并纳入包内哈希清单。实际 PE、截断/无效 RVA/延迟依赖回归和 M2-K04 通过；两个包的 desktop/CLI 均无外部 MSVC 运行库导入，包内 CLI 安装与外部卸载实跑通过 |

普通实现修复无需新增 ADR；金融、范围、状态、恢复、接口与打包行为已回写职责正本。S-23 使用计划内便携 ZIP＋Rust 按用户安装备选，M2 v1.0 的范围和版本不变；Git 规则中遗留的 M1 固定父提交说明已改为按当前唯一计划核对。

## A 集中技术审核（2026-11-09）

角色：Agent A；待审提交 `6a388c92f1bc684b243b53f2c4f7bceadf912db2`（父 `0370b7e`）。A 独立重算待审源码指纹，与 B 声明的 `e2ba2bdc…5a71` 一致，跟踪树干净；`workers/agent/` 未读取、构建、修改或暂存。按风险抽查 AI 工具范围/冻结版本、证据与数值校验、`CashExchange` 记账、范围解析、卸载所有权和删除计划，并以隐藏原生窗口实际截图复核界面。审核修正与用户要求的界面重设计已 amend 进同一阶段提交，最终被测源码指纹 `3c6a16dceea9515b5deb4942d26ae7bfa1181af1f4c4621b2064dc3cb34293d9`。

| 编号 / 严重度 | 触发、实际与预期 | 修复与复验 |
| --- | --- | --- |
| F-08 / P1 / resolved | 摘要数字提取把前缀含字母的数字一律当标识符跳过：模型写 `总损益 PnL:99999 USD`、`SGD99999` 等虚构数值仍判 `verified`，违反 S-20“正文数值须在工具回执中找到” | `ai.rs`：`键:值`/`键=值` 与 3～5 位大写货币/代码前缀按金融数值校验；只有本次有效或已拦截的引用 ID 先作为标识符剔除，内联有效引用仍可 `verified`。新增 3 个绕过用例与内联引用用例，修复前绕过用例会判为已验证；AI 设计正本同步 |
| F-09 / P2 / resolved | 实际截图显示：应用未注册图标资源，所有 `Icon`（状态横幅、下钻箭头）为空白；Dock 面板以裸实体加入，标题栏显示内部类型名 `CenterWorkspacePanel` 等 | 主窗口与冒烟/基准/截图均 `with_assets(gpui_kit::assets::Assets)`；面板经 `panel_handle` 加入并自绘标题。截图复核图标与标题正确 |

**用户要求的界面重设计（行为不变）**：参照长桥桌面客户端的布局与信息密度（不复制其品牌资产）：新增 `delta-desktop::theme` 浅色主题；顶部工具栏（标识、当前页、账户、紧凑范围输入、AI/任务开关）、左侧图标导航栏、底部状态栏；市场页改为自选列表＋报价头＋图表工具条＋K 线，涨跌额/涨跌幅由新的 `KlineWindowSliceDto::last_close_change`（`delta-app`，Decimal）计算，UI 只格式化；总览为净资产主值＋卡片网格；资产页批次/对账改为表格。原有元素 ID、测试文本和交互契约全部保留；规则回写 [DESIGN](../design-rules/DESIGN.md)。显式截图测试 `capture_workbench_pages`（`#[ignore]`，隐藏窗口、不激活）输出六页 PNG 到 `.local/reports/m2/a-review/ui-capture/`（`before/` 为原界面，`v2/` 为重设计后）。

**A 复验（最终源码指纹）**：`verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench` overall passed（日志 `.local/reports/m2/logs-12160-1794157036210/`，控制台 `.local/reports/m2/a-review/verify-stage-m2.log`），0 可见窗口、未抢焦点、正式数据根未触碰；工作区 95 passed、0 failed、2 ignored（live 探针与显式截图）；fmt/clippy 零警告。M1 v2.6 回归 overall passed：22 passed、5 项 extension/live not-run（日志 `.local/reports/m1/logs-28164-1794157361857/`，控制台 `.local/reports/m2/a-review/verify-stage-m1.log`）。release 原生基准（同机 RTX 4070 SUPER、缩放 1.5、逻辑窗口约 918.7×596）：

| 项目 | 次数 | p50 | p95 | p99 | p95 门槛 |
| --- | ---: | ---: | ---: | ---: | ---: |
| AC-02 首屏 | 5 个新进程 | 318 ms | 339 ms | 339 ms | ≤3000 ms |
| 工作台导航/持久化/绘制 | 300 | 31.008 ms | 33.530 ms | 35.134 ms | ≤100 ms |
| 100k 日线、1000 可见图表帧 | 300（另 20 预热） | 11.920 ms | 17.789 ms | 18.963 ms | ≤20 ms |
| 100k 行 DataTable 滚动帧 | 300（另 20 预热） | 5.672 ms | 7.238 ms | 7.591 ms | ≤20 ms |

图表帧 p95 余量约 2.2 ms，较 B 轮无实质退化；结论只适用于记录的机器与口径。本轮重新构建的两通道静态 CRT 包：release `delta-0.1.0-windows-x86_64-5b192dd5-0290-4d8c-a6af-ec2bd8eb6e23.zip`（36448773 字节，SHA-256 `30c8d6020eaa5c284dff8ec20cf936db6baefb11b3c236c21f940fc96b022500`），dev `delta-dev-0.1.0-windows-x86_64-11308079-6aa4-45ef-8519-efd9c76c399c.zip`（36468341 字节，SHA-256 `1db0013e4f2b3176a28b4252232dfa2113636e7895a721e58d5e12f971b961b3`），M2-K04 安装/卸载与导入检查随门禁通过；H-01～H-03 以这两个包为准，下文 B 轮包表仅为历史。

**结论**：B 实现与自检：完成。**A 技术审核：通过**，F-08/F-09 已在同一阶段提交修复并复验，无剩余工程阻断，无需交回 B。产品阶段验收：未完成——真实模型 ACT-01、个人账单 ACT-03、人工 H-01～H-03 保持 not-run，界面重设计尚待你在实际窗口中确认。下一步为 [USER-ACTIONS](../../USER-ACTIONS.md) `ACT-09` 推送与创建 PR 的授权。

## 工作包与验证入口

| 工作包 | 实现与重点入口 |
| --- | --- |
| M2-W01 | `delta-app/host.rs`、`delta-data/m2_schema.rs` / `storage.rs`：一致快照、AnalysisScope、ResultEnvelope、CapabilityCatalog、schema 3→4 备份/故障回滚；M2-A-01/02，冻结空 scope/零行情回归 |
| M2-W02 | `delta-core/ledger.rs` / `valuation.rs`、导入/报告服务：账户/组合去重、期间损益、缺价/FX/成本、手工估值、对账、更正 stale、现金换汇；M2-A-04～06 与 M1 金额回归 |
| M2-W03/04 | DockArea/DockSkin/Panel/DataTable、六页导航、账户/组合、预设导入、总览下钻、真实同步状态；M2-A-03/07～09 与实际点击/输入回归 |
| M2-W05 | KlineViewport、批量 GPUI quads/path、滚轮/拖拽/光标/键盘、日期/指标/复权、交易/笔记标记；M2-A-10/11，同日指标窗口不变、未来哨兵、缓存内容指纹 |
| M2-W06 | 笔记模板/正文/标签、owner/复盘日期、乐观修订、诚实自动保存、搜索/附件、图表双向恢复；M2-A-11/12，保存失败/范围切换和期间/截止回归 |
| M2-W07 | `delta-model` 两协议真实 HTTP/SSE 回环、`delta-app/ai.rs` 工具/报告/历史/压缩，设置与 AI 控件；M2-A-13～16，中文/符号/伪造引用/权限/跨库凭据回归 |
| M2-W08 | 一致备份/恢复/导出/删除、白名单日志、真实两通道静态 CRT 包/PE 校验/安装/外部卸载、逐包许可与全门禁；M2-A-17～20 与 M1 v2.6 回归 |

AC-02 手算固定：初始 2000，买 10×100 费 1，卖 4×110 费 1，期末价 105 → 现金 1438、持仓 6、成本 600.6、已实现 38.6、未实现 29.4、NAV 2068、总损益 68 USD。金融数值由 core/app 产生，UI 与 AI 不另算。

## 本轮实际验证

最终命令均退出 0，源码指纹相同：

```text
cargo run -p delta-cli --locked -- verify-stage --stage M2 --plan-version 1.0 --ui-smoke --ui-bench
cargo run -p delta-cli --locked -- verify-stage --stage M1 --plan-version 2.6
```

| 验证 | 实际结果与原始证据 |
| --- | --- |
| M2 全门禁 | overall passed；M2-G-01～06 与 M2-A-01～20 共 26 项 passed；M2-K01～06 全 passed；3 项人工与 2 项 live not-run。报告 `.local/reports/m2/verify-stage-report.json`，日志 `.local/reports/m2/logs-16112-1794143708105/`，控制台日志 `.local/reports/m2/b-review/verify-stage-static-crt-final.log` |
| 静态与测试 | docs/diff/fmt/clippy/test 均通过，Clippy 零警告；工作区 94 passed、0 failed、1 ignored（`test_m1_live_network_probes`）。其中 app 56、desktop 14、CLI 5，含实际表单/导航/保存失败/导入/AI 无模型/图表输入和包 PE 回归 |
| 模型协议 | `openai-responses` 与 `openai-chat-completions` 的受控 HTTP/SSE 回环、工具配对、reasoning、预算、认证/限流/取消/截断恢复通过；M2-K03/A13～16。真实模型保持 not-run |
| M1 v2.6 回归 | overall passed；22 项 passed、5 项 extension/live not-run；百万行 SQLite POC passed，独立 ccxt POC-D06 本轮 not-run。报告 `.local/reports/m1/verify-stage-report.json`，日志 `.local/reports/m1/logs-4832-1794143844223/` 和 `.local/reports/m2/b-review/m1-regression-static-crt-final.log` |
| 许可 | M2-G-06 与 3 项 Python 回归通过，885/885、0 errors；48 包明确标准正文＋完整锁定源码回退，4 种 AND 义务全部保留。MPL 包源码零修改，公开逐包清单见 [依赖许可](../evidence/2026-11-08-dependency-licenses.md) |
| 安装与卸载 | 两通道分别编译，包内 CLI 实际安装；并存、dev 和 release 外部卸载、安装目录内自卸载拒绝、两个数据根和未知文件保留均通过。命令日志见 M2 日志目录的 `package-*.json` 与 `installed-self-uninstall.json` |
| ZIP 分发内容 | 两包各 2443 个清单文件＋1 个 manifest；2444 个 ZIP 条目全部 CRC/哈希校验，无重名/越界/链接条目。原始结果 `.local/reports/m2/b-review/archive-verification.json` |
| 静音与隔离 | M2/M1 均为 below_normal、并发 4、0 可见窗口、0 临时残留、0 观测范围外新建目录、正式数据根未触碰；原生冒烟/基准未抢焦点。观测范围为目录元数据，非全系统写入追踪 |

## 原生 release 性能与包

Windows 11；20 逻辑核（Intel Family 6 Model 183）；RTX 4070 SUPER，driver 621.14；缩放 1.5，逻辑窗口约 918.7×596 px。以下均为最终静态 CRT release 包实测，原始样本在 `.local/reports/m2/ui-bench-report.json`，门禁独立重算分位数。

| 项目 | 实测次数 | p50 | p95 | p99 | p95 门槛 |
| --- | ---: | ---: | ---: | ---: | ---: |
| AC-02 首屏 | 5 个新进程 | 233 ms | 244 ms | 244 ms | ≤3000 ms |
| 工作台导航/设置持久化/绘制 | 300 | 30.820 ms | 32.068 ms | 32.765 ms | ≤100 ms |
| 100k 日线、1000 根可见图表帧 | 300（另 20 预热） | 7.743 ms | 18.171 ms | 21.725 ms | ≤20 ms |
| 100k 行 DataTable 滚动帧 | 300（另 20 预热） | 5.501 ms | 7.765 ms | 13.360 ms | ≤20 ms |

实际图表绘制 320 次、表格 cell 绘制 49,600 次；取消接受 <1 ms（整毫秒记录 0）、worker 释放 5 ms，在途 provider 取消另有阻塞 HTTP 回归。图表 p99 超过 20 ms，当前 p95 满足门槛；性能结论只适用于记录的机器和测量口径。

| 通道 | 最终 ZIP（位于 `.local/reports/m2/packages/`） | 字节数 | SHA-256 |
| --- | --- | ---: | --- |
| release | `delta-0.1.0-windows-x86_64-61ec907d-5d81-478d-a783-6548d1e7600b.zip` | 36392374 | `a58afa432977dbe279bb4db5069315e4097a7c9c9de08eb5bfcd76c294adb566` |
| dev | `delta-dev-0.1.0-windows-x86_64-3b91a692-ff4a-4ccd-aa28-712937e20008.zip` | 36411929 | `74227965db3dc9ffd6769c9a0014c40ebff8a463434451f6ca53b2f1bbf2b072` |

产物索引为 `.local/reports/m2/packages/package-artifacts.json`；每包带 `README.txt`、`package-manifest.json`、`binary-imports.json`、项目通知及完整第三方许可/归档。两个 desktop 与两个 CLI 的普通/延迟导入均无 `vcruntime` / `msvcp` / `msvcr` / `concrt`；仍依赖 Windows 系统 DLL。两通道及四个二进制 SHA 不同，具体可执行文件 SHA 见 archive-verification 原始结果。包未签名，H-03 干净机器仍 not-run。

## 验证边界与接续

默认 `verify-stage --stage M2 --plan-version 1.0` 执行静音离线/无头检查；没有 `--ui-smoke --ui-bench` 时，K02/K06/A20 与 overall 保持 not-run，成功退出仅表示已执行检查无失败。阶段完整 B 自检需显式运行两项 UI 验证。隐藏窗口帧包含 GPUI 更新/布局/绘制与 DirectX render/present 调用，排除等待定时器；FPS 为帧预算倒数，不能当作可见桌面的持续显示采样。首屏为 AC-02 本地库、5 个新进程，包含夹具初始化，排除 OS loader 且不清空系统文件缓存；100k 图表/表格规模分别测试。

本轮只使用合成数据、回环服务、测试 keyring 命名空间和隔离根。真实模型 ACT-01、真实个人账单 ACT-03、中文输入法/缩放/干净机器安装 H-01～H-03 保持 not-run；公共行情本轮未运行时同样标 not-run，历史 live 记录不冒充本次通过。机器为 Windows 11、20 逻辑核、RTX 4070 SUPER，性能外推限于实际记录的硬件/缩放。

集中待处理项为 OSS-07（用户配置的真实兼容端点）、OSS-10（M4 存储扩展）、LOOP-01～03（后续训练/策略及扩展连接器），见 [工程台账](../decisions/open-questions.md)。OSS-08/09/11 已按本轮证据关闭；F-01～07 全部修复并复验，没有本轮必做工程缺陷延期给用户。

首选下一步：Agent A 对同一 M2 v1.0 计划与本轮提交集中技术审核，核对源码指纹并复用充分的未变更证据，重点复验金融、权限、恢复、实际 UI 和包的边界。A 收敛后才在 USER-ACTIONS 登记具体远端授权事项；独立人工检查不等于技术审核或产品验收通过。
