# M1 锁文件依赖许可盘点

日期：2026-11-08；执行：Agent B；唯一计划：[M1 v2.6](../delivery/stage-plan.md)。原始命令为 `cargo metadata --locked --format-version 1`，metadata 与 stderr 保存在忽略目录 `.local/reports/m1/`。可公开的逐包清单、表达式、许可分支、许可文件哈希和来源仓库见 [JSON 清单](2026-11-08-dependency-licenses.json)。

## 1. 实际范围与结果

本次 metadata 包含 **4 个工作区包 + 221 个第三方包**。221 是名称/版本不同的已解析依赖包数，包含构建依赖和其他目标平台依赖，不是 Windows 二进制实际装入包数。锁文件 SHA-256：`09204b09cd1c28442a4639e3395aa9ccabc9489d705c0e6a1928781552013683`。

所有 221 个包都声明了 license 表达式。按以下分支可使用宽松许可；没有必须选择 LGPL 的包，也没有未声明的 crate 级表达式。原样保留表达式，不把依赖改标为本项目 Apache-2.0。

| 采用分支 | 包数 | 保留义务 |
| --- | ---: | --- |
| MIT | 194 | 保留许可原文与版权声明 |
| Apache-2.0 | 5 | 保留许可与适用 NOTICE，按许可记录修改 |
| Unicode-3.0 | 18 | 保留 Unicode 许可及版权/通知 |
| MIT AND Unicode-3.0 | 1 | 两项义务同时保留 |
| BSD-3-Clause | 1 | 保留许可/版权，遵守不背书条款 |
| Zlib | 2 | 保留通知，不歪曲来源和修改 |

该结果是锁定版本的许可表达式核验，足以说明 M1 当前依赖没有强制 copyleft 分支；不把它扩展成对所有平台、嵌入第三方资源或未来二进制分发的“100% 合规”声明。

## 2. 需要解释的声明

- `MIT/Apache-2.0` 的 8 个包：`fallible-iterator`、`fallible-streaming-iterator`、`foreign-types`、`foreign-types-shared`、`hyper-tls`、`openssl-macros`、`openssl-probe`、`openssl-sys`。包中同时存在 MIT/Apache 原文，本次选择 MIT 分支；JSON 另存规范化表达式，保留 Cargo 原值。
- `Unlicense/MIT` 的 `csv` / `csv-core`：包内 COPYING 明确允许任一许可，本次选择 MIT。
- `r-efi` 两个版本声明 `MIT OR Apache-2.0 OR LGPL-2.1-or-later`，选择 MIT；它们和 `rsqlite-vfs` 的发布包未附顶层许可原文，清单如实标为空。这些属于其他目标平台依赖；本次不宣称已打包这些目标的分发通知。实际发布应从清单中的上游仓库补齐对应原文。
- `unicode-ident` 的 `(MIT OR Apache-2.0) AND Unicode-3.0` 采用 `MIT AND Unicode-3.0`，不能仅选 MIT 后丢掉 Unicode 通知。
- `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` 按其 MIT 备选分支计入，不把 exception 当作独立项目许可。

## 3. 非 crate 与独立 POC 边界

`libsqlite3-sys` 的绑定源码为 MIT；bundled SQLite 的许可来源为 [SQLite Public Domain](https://www.sqlite.org/copyright.html)，分发清单应分别保留绑定版权与 SQLite 的来源声明。系统 TLS 等目标平台原生库不是 metadata 表达式盘点的全量对象，本阶段没有生成安装包。

日历夹具引用 `exchange_calendars` 的 Apache-2.0 数据文件；出处与使用范围见 [PROVENANCE](../../tests/fixtures/PROVENANCE.md#1-交易日历夹具testsfixturescalendar)。行情数据的 `public_open` / `official_free_personal` / `unofficial_personal` 只是来源条款类别，不由代码许可授予再分发权。

官方 ccxt `=4.5.91` 是 `.local/poc/` 下的独立兼容性实验，不计入这 221 个包，也没有加入产品锁文件。其构建、依赖版本和 f64/栈边界见 [POC-D06](2026-11-08-ccxt-rust-poc.md)。
