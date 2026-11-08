# M2 GPUI Kit 0.9.0 构建与无头 UI 测试 spike

记录与实测：2026-11-08；执行者：Agent A；作用域：M2 计划开始时的 `OSS-09` 首个技术验证（只构建、只跑无头测试，不打开真实窗口）。结论供 [M2 计划](../delivery/stage-plan.md) 定案；具体兼容版本由 B 在产品工作区再次锁定并提供运行证据。

## 1. 官方来源与上游事实

- [crates.io gpui-kit](https://crates.io/crates/gpui-kit)：最新稳定版 `0.9.0`（2026-11-02，Apache-2.0），此前 `0.8.1`（2026-10-26）、`0.8.0`（2026-10-19）；`0.9.0` 精确依赖 `gpui-pre =0.3.11`、`gpui-base` / `gpui-component` / `gpui-kit-assets ^0.9.0`。
- [longbridge/gpui-kit](https://github.com/longbridge/gpui-kit) 源码（本机只读参考，`0.9.0` 版本源码）核查：
  - 存在并公开导出：`gpui_kit::component::dock::{DockArea, DockAreaState, DockLayout, DockPlacement, DockSkin, Panel, BasePanel, PanelControl, PanelEvent, PanelStyle, panel_handle}`（`DockArea` / `DockLayout` 定义在 `gpui-base`，`DockSkin` / `Panel` 在 `gpui-component`）、`table::{DataTable, TableDelegate, Table}`、`chart::{CandlestickChart, AreaChart, LineChart, BarChart, PieChart, ...}`、`GroupBox`、`DescriptionList`、`StatusBar`、`Sidebar`、`VirtualList`、Markdown `TextView`、`Input` / 编辑器等。
  - **不存在官方 `Card` 组件**：`0.9.0` 没有导出名为 `Card` 的类型；官方 story 中的图表“卡片”是 story 私有的 `ChartCard` 组合写法（标题、期间、图表、图例、趋势）。D-21 列出的 `Card` 因此按“官方 story 模式自建组合件”落地，不改变 D-21 方向。
  - `CandlestickChart<T, X, Y>` 为 `Plot` 实现（497 行），按传入的全部数据绘制，无内建视口状态、十字光标或标记层；图表数值为 `f64`。`gpui-component` 另有 `decimal` feature（转发 `gpui-base/decimal`）。
  - 官方 `test-support` feature 提供 `#[gpui_kit::test]` 与 `gpui_kit::test::{TestWindowExt, TestAppContextExt, TestSupportExt}`，用真实组件、真实命中测试做无头 UI 集成测试（见上游 `crates/kit/TESTING.md`）。

## 2. 本机实测（Windows 11 / MSVC / Rust 1.99.1）

隔离工程位于忽略目录 `.local/spikes/m2-gpui-kit-0.9.0/`（独立 `[workspace]`，复制仓库 `rust-toolchain.toml` 精确 `1.99.1`），不是产品依赖。

| 项目 | 实际结果 |
| --- | --- |
| 依赖 | 直接依赖 `gpui-kit = "=0.9.0"`（dev 另开 `test-support`）；解析 `gpui-pre 0.3.11`；锁文件共 **848** 个包（847 个第三方），对比 M1 产品工作区 225 个 |
| 构建 | `cargo build`（dev，`CARGO_BUILD_JOBS=8`、`CARGO_INCREMENTAL=0`）exit 0，耗时 92 s；本机 Cargo 注册表与下载缓存已存在，**不是冷构建时长**。dev 可执行 53 MB，`target/` 3.3 GB |
| 符号 | 主程序引用 `dock::DockArea`、`dock::DockSkin`、`table::Table` 编译通过；默认不启动应用循环（未打开窗口） |
| 无头 UI 测试 | 复制上游 `crates/kit/tests/integration/dock.rs` 的 `dock_switches_and_reorders_real_tabs`（Apache-2.0，仅 spike 内使用）：`DockSkin::dock_area` + `DockLayout::tabs()` 两个面板，真实点击切换标签与拖拽重排。`cargo test --no-run` 36 s；运行 **1 passed，0.06 s** |
| 前台无干扰（D-25） | 测试进程以 `CreateNoWindow`、`BelowNormal` 优先级启动，20 ms 间隔 `EnumWindows` 统计该 PID 的可见顶层窗口：**最大 0 个**；`GetForegroundWindow` 前后**未变化** |
| 栈 | 本次构建与无头测试未出现栈溢出；真实窗口与完整工作台的栈需求未测 |

## 3. 依赖许可盘点（`cargo metadata --locked`）

847 个第三方包均有许可表达式，绝大多数为 MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode-3.0 等宽松许可或含宽松分支的多选表达式。需要在 M2 分发通知中单独处理：

| 包 | 许可 | 处理要点 |
| --- | --- | --- |
| `dwrote 0.11.5` | MPL-2.0 | Windows 运行期字体依赖；文件级弱 copyleft，未修改时与 Apache-2.0 分发兼容，但须在通知中保留 MPL 文本与源码获取方式 |
| `option-ext 0.2.0` | MPL-2.0 | 同上（经 `dirs` 系依赖引入） |
| `cbindgen 0.28.0` | MPL-2.0 | 构建期工具依赖，B 核对是否进入 Windows 目标产物 |
| `self_cell 1.3.0`、`r-efi` | `Apache-2.0 OR GPL-2.0-only`、`MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 选择宽松分支 |

该盘点是 spike 锁文件，不是产品锁文件；B 在产品工作区重新生成逐包清单（沿用 M1 [依赖许可](2026-11-08-dependency-licenses.md) 格式）并确认分发通知（`OSS-11`）。

## 4. 结论与边界

1. `gpui-kit 0.9.0` 可在本机工具链下构建，官方无头 UI 测试不创建可见窗口、不抢焦点，可以作为 M2 界面逻辑测试与门禁基础（`OSS-09` 的构建/静音风险部分已验证）。
2. 未验证：真实窗口渲染、DirectX/GPU 驱动差异、中文输入法、125%/150% 缩放、K 线 10 万根窗口性能、发布构建体积与冷构建时长、完整工作台栈需求。这些由 M2 计划的 spike 与验收用例覆盖。
3. `Card` 与 K 线视口状态需自建；`CandlestickChart` 只在 spike 通过后作为渲染候选（见 M2 计划 S-04 / S-05）。
