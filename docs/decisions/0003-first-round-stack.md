# ADR-0003：数据优先架构重排与 GPUI 官方 Dock 面板体系升级

日期：2026-11-07；状态：accepted（D-20、D-21，替代原首轮 `gpui-kit 0.6.6` 桌面先行草案）；责任：Agent A。关联证据：[2026-11-07 栈与数据调研](../evidence/2026-11-07-stack-and-data-research.md)、[上游快照](../evidence/2026-11-07-upstream-snapshot.json)。

## 1. 背景与问题

项目在文档治理基线（2026-09-26）之后的前序尝试暴露出两个核心问题：

1. **研发顺序倒置（没数据先写壳）**：个人金融工作台的核心壁垒是可信的美股/ETF、加密现货、法币/稳定币汇率、拆股分红公司行动、交易日历及券商/交易所流水数据底座。在未打通真实数据获取与自动增量同步前先堆砌桌面页面，导致界面只能围绕几条静态假数据空转。
2. **旧版 `gpui-kit 0.6.6` 用法不当与图表视口边界**：
   - `0.6.6` 来自 `2026-09-26` 初始调研快照（`docs/evidence/2026-09-26-upstream-snapshot.json`），前序废弃代码锁死在 `0.6.6` 并手写绝对定位布局，未使用长桥在最新 `gpui-kit`（本地参考 `0.8.1` / crates.io `0.9.0`）中成熟的官方 `DockArea` + `DockLayout` + `DockSkin` + `Panel` + `DataTable` 面板系统；
   - `gpui-kit` 内置的 `CandlestickChart`（497 行，见 `chart_story.rs`）默认面向 40 根日线静态卡片展示，若不加视口窗口切片（Viewport Slicing）直接传入数千根历史 K 线会导致柱体挤压，且缺少滚轮缩放与拖拽平移状态机。

## 2. 决定（D-20 & D-21）

1. **全项目从文档治理基线重建，切换为“M1 数据底座优先 → M2 官方 GPUI 面板接入”路线**：
   - **M1（金融数据来源与本地自动化数据底座）**：不引入 GUI 依赖，且按 [D-22](confirmed-decisions.md) 100% 纯 Rust 实现（不引入 Python 进程）。优先交付 `delta-core`、`delta-data`、`delta-app`、`delta-cli`，打通美股/ETF、加密现货、汇率、公司行动、交易日历、券商/交易所流水导入及水位线自动增量同步（M1 为 SQLite 单库，见下方 2026-11-07 A 复核修订）。
   - **M2（基于长桥官方 `gpui-kit` 面板体系的资产复盘桌面端）**：在 M1 数据底座验证通过后，同一 Rust 进程零拷贝接入新版 `gpui-kit`（`0.8.x` / `0.9.x`）。
2. **构建内置自动化数据平面，告别外部手动维护**：
   - 采用 **“100% 纯 Rust 原生连接器 + 纯 Rust 交易日历与指标 + SQLite 单库（`dataset_partition` 存储抽象）”** 架构，Parquet/DuckDB 推迟到 M4 批量扫描或实测性能不足时接入（`OSS-10`）；
   - 通过 **账本/自选驱动自动订阅 + 交易日历感知的水位线增量同步 + 多源降级与交叉核验 + 不可变 `dataset_version` 快照**，使行情、分红拆股和汇率随应用或 CLI 自动增量维护。
3. **M2 桌面端直接采用长桥官方 `gpui-kit`（`0.8.x` / `0.9.x`）面板体系（D-21）**：
   - 彻底弃用旧 `0.6.6` 锁定，升级至最新 `gpui-kit`（`0.8.1` / `0.9.0`）；
   - **工作区骨架**：直接按照长桥官方 `dock_story.rs` 范式，使用 `DockArea` + `DockLayout` + `DockSkin` + `BasePanel` / `Panel` 构建可停靠、可拖拽分屏、多标签页切换与 JSON 持久化恢复的工作台面板（自选股面板、K 线与回放面板、资产概览卡片面板、持仓与 FIFO 税批面板、AI 助手面板）；
   - **表格与卡片**：直接使用官方 `DataTable`（虚拟滚动、固定列、排序）与 `Card` + `AreaChart` / `PieChart` / `BarChart`；
   - **交互式 K 线与回放面板**：外层封装 GPUI 视口滑动窗口状态机（`visible_range: Range<usize>`，响应滚轮缩放可视根数、鼠标拖拽平移时间窗口与十字光标），每帧仅从本地行情存储/内存切片当前可见的 40～200 根 K 线进行渲染；必要时也可按面板粒度挂载官方 `gpui-webview`。

## 3. 影响与退出策略

- M1 阶段完全聚焦于美股/ETF 与加密现货数据准确性、幂等入账和成本损益对账，不受 UI 调试干扰；
- M2 阶段直接复用长桥官方 `gpui-kit` 现成面板与高密度表格原语，并与 M1 的纯 Rust 数据服务在同进程内零 IPC 开销结合。

## 4. 2026-11-07 A 复核修订（不改变 D-20 / D-21）

- **M1 存储收敛为 SQLite 单库**：个人组合日线规模下单库即可满足查询，且使 AC-09 一致性备份由 SQLite Backup API 原子完成；Parquet/DuckDB 保留为 `dataset_partition` 背后的后续存储选项（`OSS-10`）。
- **`delta-app` 异步边界**：`gpui-kit 0.9.0` 精确锁定 `gpui-pre =0.3.11` 并使用 GPUI 自带执行器，M1 的 `delta-app` 对外只提供同步命令/查询与作业句柄，不暴露 `tokio` 类型，避免 M2 运行时桥接返工。快照锁定、Windows 构建与依赖规模风险登记 `OSS-09`，在 M2 计划开始时以实际构建验证。
- 调研中“Longbridge Pro 交易 K 线容器未开源”系 A 依据公开仓库内容的推断，非上游声明；M2 K 线面板按自研视口切片规划，不依赖该推断。

## 5. 2026-11-08 M2 开始时 A 核实（不改变 D-21）

- `gpui-kit 0.9.0` 在本机 Rust 1.99.1 / MSVC 下构建通过，官方无头 Dock UI 测试不创建可见窗口（[spike 证据](../evidence/2026-11-08-m2-gpui-kit-spike.md)），`OSS-09` 的构建与静音风险部分已验证。
- 0.9.0 **没有导出官方 `Card` 组件**（官方 story 的图表卡片是私有组合写法）。决定中的 `Card` 按该模式自建 `MetricCard`，其余 `DockArea` / `DockLayout` / `DockSkin` / `Panel` / `DataTable` 均直接使用官方实现（[M2 计划 S-15](../delivery/stage-plan.md#3-a-已定案的阶段技术决定b-不再重新选型)）。
