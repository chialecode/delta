# 上游事项台账

更新：2026-11-24；维护：维护者。本表只记开源上游与适配缺口（OSS）。需求、缺陷、返工与后续迭代都在 [Issues](https://github.com/chialecode/delta/issues)，需要用户处理的事项是 [`needs-user` issue](https://github.com/chialecode/delta/issues?q=is%3Aissue+label%3Aneeds-user)（[D-38](confirmed-decisions.md)）。

状态：open、investigating、resolved、deferred。新增项用稳定 `OSS-编号`，写清影响、当前处置或退出条件与触发条件；需要排期的工作另开 issue 并互相链接。有可逆替代时继续独立工作，开发缺口不伪装成用户任务。

## 开源风险与适配缺口

| ID / 状态 | 上游与风险 | 当前处置 / 退出条件 | 负责人 / 截止与受阻范围 |
| --- | --- | --- | --- |
| OSS-01 / resolved | GPUI Kit 旧 `0.6.6` 缺少最新面板能力，且内置 `CandlestickChart` 默认仅面向 40 根日线静态卡片展示 | 已由 D-20 / D-21 解决方向问题：① M1 完全解耦 GUI，先完成金融数据底座与账本核心；② M2 升级至长桥最新 `gpui-kit`（`0.8.x` / `0.9.x`），直接使用官方 `DockArea` + `DockSkin` + `Panel` + `DataTable` + `Card` 面板体系，主 K 线面板外包视口滑动窗口（`visible_range` 缩放/平移切片）（见 [ADR-0003](0003-first-round-stack.md)）。集成实测风险转 `OSS-09` | A 已完成源码核查并经用户 D-21 确认关闭 |
| OSS-02 / resolved | 旧模型运行时的资源发现/隔离验证 | D-08 撤回旧候选，关闭该候选专属验证；工具白名单/状态验证转入 OSS-08 与有效合同 | A；2026-09-26；不表示旧候选实测通过 |
| OSS-03 / resolved | 原 Python 数据连接器/指标工作进程的离线锁定与跨进程隔离负担 | 已由用户 2026-11-07 **D-22** 解决：全项目彻底采用 100% 纯 Rust 实现，移除 `workers/python` 与运行期 Python 依赖；免 Key 美股行情由 Rust 直连 Yahoo Chart v8 与东财 `push2his` JSON 接口，日历与指标均由纯 Rust 实现 | A 已更新架构与 M1 计划 v2.2 关闭 |
| OSS-04 / resolved | `chrono-tz 0.10.4` / IANA 时区与美股（`XNYS`/`XNAS`）半日市、节假日、夏令时覆盖 | M1 纯 Rust 基于 `chrono` + `chrono-tz`（`America/New_York`）实现 NYSE/NASDAQ 法定节假日与半日市规则（1993 年前 14:00 ET、1993 年起 13:00 ET）+ 仓库内置版本化特殊闭市表 `calendar_special_xnys.json`，写入 SQLite `calendar_session` 并校验加密 `24×7` UTC 切分 | B 已在 M1-W02 完成实现并通过 `POC-D02` 实测：`1990-01-02`～`2027-12-31` 全量 13,878 个自然日（9,569 个交易日、80 个提前收盘日）与 `exchange_calendars` `xnys.csv` 及 NYSE 2023～2027 官方表逐日核对 `0` 差异，越界年份返回 `calendar_unknown`（见 [M1 阶段报告](../evidence/2026-11-08-m1-stage-report.md)） |
| OSS-05 / resolved | 必做公共数据源的字段、复权、限流、版本与 Decimal 边界 | M1 原生 REST/ZIP 完成真实编排、核验和缺失诊断；实际结果只采用本轮阶段报告 | B；官方 ccxt =4.5.91 实测构建成功，默认 Windows 栈溢出，64MiB worker 后 OKX/Kraken 成功；typed f64 边界使通用 adapter 转 LOOP-03，见 [POC-D06](../evidence/2026-11-08-ccxt-rust-poc.md) |
| OSS-06 / resolved | 锁文件传递依赖许可与数据来源条款 | 基于 Cargo.lock 的 metadata 清单重新核验；数量、表达式与选择的宽松许可分支见阶段报告 | B；不把代码许可当作行情再分发权；封存版本/导出使用 public_open / official_free_personal / unofficial_personal |
| OSS-07 / open | 兼容端点工具/SSE/用量能力差异 | M2 已执行受控回环全套验证：`delta-model` 对 `openai-responses` 与 `openai-chat-completions` 独立通过 9 类回环用例（`M2-K03` / `M2-A-13`）；真实在线模型服务只对用户按 [#16](https://github.com/chialecode/delta/issues/16) 配置的协议/模型声明（见 [M2 阶段报告](../evidence/2026-11-09-m2-stage-report.md)） | 缺真实配置时真实服务保持 not-run，不阻塞工程收敛 |
| OSS-08 / resolved | 自研 Rust 模型客户端需持续维护双协议/SSE/工具配对、取消和上下文恢复 | 本轮修复 wire 名称、并行请求成组历史、reasoning、SSE/累计预算、显式证据/数字、连接与 journal 指纹和跨库 keyring；锁定 `reqwest 0.13.5` + `eventsource-stream 0.2.3`，M2-K03/A13～16 回环和权限/恢复回归通过 | B；2026-11-08 按 [阶段报告](../evidence/2026-11-09-m2-stage-report.md) 关闭 M2 工程适配，真实兼容端点仍归 OSS-07 |
| OSS-09 / resolved | M2 GPUI 集成风险：`gpui-kit 0.9.0` 精确锁定 `gpui-pre =0.3.11`，升级伴随快照破坏；宿主后台边界、Windows 栈和真实性能 | 实际六页/表单/图表交互、隐藏原生 HWND 与 8 MiB 栈实跑；最终静态 CRT release 包完成 100k 图表/表格各 300 实测帧、5 次首屏和 300 次交互，p95 均满足门槛，原始分位数与限制见阶段报告 | B；2026-11-08 M2-K01/K02/K06 与 M2-A-20 通过关闭；后续升级重新验证锁定快照 |
| OSS-10 / deferred | Parquet/DuckDB 行情仓 | M1–M2 只用 SQLite 单库，`dataset_partition.location_kind` 预留扩展；引入时保持上层查询契约并扩展一致性备份清单 | [#31](https://github.com/chialecode/delta/issues/31)；触发条件：M6 批量扫描需求，或 POC-D01/实测查询性能不达标 |
| OSS-11 / resolved | M2 MPL-2.0 与多许可分发；原收集漏掉 AND 义务且只核声明不足以支持分发 | 本轮收集 885/885、0 errors；完整保留 4 种 AND 义务、原始 notices/哈希；48 包明确标准正文＋完整锁定源码回退，SPDX 数据固定 commit，见 [逐包清单](../evidence/2026-11-08-dependency-licenses.md) | B；2026-11-08 M2-G-06、3 项收集回归与真实两通道 ZIP 全文件哈希/随附正文归档验证通过关闭；MPL 包零源码修改 |
| OSS-12 / resolved | Inno Setup 为外部构建工具，不在 Cargo 锁定范围内；版本漂移会改变安装器行为 | B；2026-11-09 M3-W02 关闭 | 锁定 7.1.1（winget `JRSoftware.InnoSetup.7`，按用户范围）。`ISCC.exe` 无可读版本资源，改由 `.iss` 内 ISPP `Ver` 检查拒绝 7.1.x 以外的编译器，门禁核对已安装键 `Inno Setup: Setup Version` 以 `7.1.` 开头；安装包与 `ISCC.exe` 哈希见 [技术选型](../engineering/technology-selection.md)；缺失时 `package-build` / `--installer` 明确失败（`INNO_SETUP_MISSING`），不回退 ZIP；安装方式写入 README。**2026-11-25 起改为 NSIS 3.13（[D-39](confirmed-decisions.md#d-39)）**：`scripts/fetch_nsis.py` 固定官方 `nsis-3.13.zip` 的下载地址与 SHA-256，解压到 Git 忽略的 `.local/tools/nsis-3.13/`；`delta.nsi` 以 `NSIS_VERSION` 拒绝其他版本；缺失时明确失败（`NSIS_MISSING`），提示取得方法，不回退 ZIP |

## 原后续循环与用户事项的去向

2026-11-24 起改为 issue 驱动。原“后续循环”与“用户事项入口”两节的最后版本见[固定提交的集中事项台账](https://github.com/chialecode/delta/blob/805d5aa011a25c92b399237543fd50e1b5fdde68/docs/decisions/open-questions.md)，原用户待办见[固定提交的 USER-ACTIONS](https://github.com/chialecode/delta/blob/805d5aa011a25c92b399237543fd50e1b5fdde68/USER-ACTIONS.md)。

| 原 ID | 去向 |
| --- | --- |
| LOOP-01（M6 策略研究） | [#24](https://github.com/chialecode/delta/issues/24) |
| LOOP-02 | [#25](https://github.com/chialecode/delta/issues/25) |
| LOOP-03 及其分项 | [#26](https://github.com/chialecode/delta/issues/26) |
| LOOP-04（每周自动发布） | [#23](https://github.com/chialecode/delta/issues/23) |
| LOOP-05 | [#27](https://github.com/chialecode/delta/issues/27) |
| LOOP-06 | [#28](https://github.com/chialecode/delta/issues/28) |
| LOOP-07 | [#29](https://github.com/chialecode/delta/issues/29) |
| PERF-01 | [#30](https://github.com/chialecode/delta/issues/30) |
| DEPS-01 | [每周维护的依赖盘点](../dev-rules/quality-gates.md#dependencies) |
| LEDGER-01、TEST-01～03、DOC-01 | 已解决，见固定提交 |
| 用户事项 ACT-01、ACT-03、ACT-10、Q-04/Q-05、H-01～H-06 | [`needs-user` issue #14～#21](https://github.com/chialecode/delta/issues?q=is%3Aissue+label%3Aneeds-user) |
