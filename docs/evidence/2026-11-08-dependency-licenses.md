# M1–M2 锁文件依赖许可盘点

日期：2026-11-08；执行：Agent B；唯一计划：[M2 v1.0](../delivery/stage-plan.md)。原始命令为 `cargo metadata --locked --format-version 1`，metadata 与 stderr 保存在忽略目录 `.local/reports/m2/`。可公开的逐包清单、表达式、许可分支、许可文件哈希和来源仓库见 [JSON 清单](2026-11-08-dependency-licenses.json)，随安装包与便携包分发的第三方通知正文见仓库根 [THIRD-PARTY-NOTICES.txt](../../THIRD-PARTY-NOTICES.txt)。

## 1. 实际范围与结果

本次 M2 产品工作区 metadata 包含 **6 个工作区包（`delta-core`、`delta-data`、`delta-model`、`delta-app`、`delta-desktop`、`delta-cli`）+ 885 个第三方包**。885 是名称/版本不同的已解析依赖包数，包含 `gpui-kit =0.9.0`、`eventsource-stream 0.2.3`、构建依赖和其他目标平台依赖，不是 Windows 二进制实际装入包数。锁文件 SHA-256：`f01595156f542efd12b91bcdb507702bf080f58a4351e1fe13264ac09f48d730`。

所有 885 个包都声明了 license 表达式。本轮收集 **885 / 885，0 errors**，保留锁定源码中的许可/NOTICE/版权文件，并逐项校验所选义务；其中 **48 个包**未取得对应版本的原许可正文，明确采用固定 SPDX 标准正文加完整锁定源码归档，不能表述为取得了 885 份原包许可正文。除 3 个未修改的文件级弱 copyleft `MPL-2.0` 包外，其余包所选义务均为宽松许可；原样保留表达式，不把依赖改标为本项目 Apache-2.0。

| 采用分支 | 包数 | 保留义务 |
| --- | ---: | --- |
| MIT | 775 | 保留许可原文与版权声明 |
| Apache-2.0 | 56 | 保留许可与适用 NOTICE，按许可记录修改 |
| Unicode-3.0 | 22 | 保留 Unicode 许可及版权/通知 |
| BSD-3-Clause | 9 | 保留许可/版权，遵守不背书条款 |
| ISC | 5 | 保留许可与版权声明 |
| BSD-2-Clause | 4 | 保留许可与版权声明 |
| MPL-2.0 | 3 | 保留 MPL-2.0 通知与对应源码获取链接（见第 2 节 `OSS-11`） |
| Zlib | 3 | 保留通知，不歪曲来源和修改 |
| 0BSD | 2 | 宽松无署名要求，保留包元信息记录 |
| CC0-1.0 | 2 | 公有领域奉献声明 |
| MIT AND Unicode-3.0 | 1 | 两项义务同时保留（`unicode-ident`） |
| MIT AND BSD-3-Clause | 1 | 两项义务同时保留（`encoding_rs 0.8.44`） |
| MIT AND NCSA | 1 | 两项义务同时保留（`libfuzzer-sys 0.4.13`） |
| Apache-2.0 AND ISC | 1 | 两项义务同时保留（`ring 0.17.14`） |

## 2. 需要解释的声明与 `MPL-2.0` 源码通知（`OSS-11`）

- **`MPL-2.0` 的 3 个包（`OSS-11`）**：
  - `dwrote 0.11.5`（`gpui-pre` 在 Windows 下的 DirectWrite 字体绑定，上游：`https://github.com/servo/dwrote-rs`）
  - `option-ext 0.2.0`（`dirs-sys` 间接依赖，上游：`https://github.com/soc/option-ext.git`）
  - `cbindgen 0.28.0`（构建期依赖，不进入运行时二进制，上游：`https://github.com/mozilla/cbindgen`）
  - 本项目对上述 3 个 crate 源码作 **零修改**；按 MPL-2.0 第 3.2(a) 条，在 [THIRD-PARTY-NOTICES.txt](../../THIRD-PARTY-NOTICES.txt) 与安装包/便携包内随附 MPL-2.0 声明及上游仓库与 `static.crates.io` 源码归档链接。
- **`MIT/Apache-2.0` 与多许可包**：包中同时存在 MIT/Apache-2.0 或 `MIT OR Apache-2.0 OR LGPL-2.1-or-later`（如 `r-efi`）时，统一选择 `MIT` 分支；JSON 清单同时保留 Cargo 原始 `license` 字段与规范化表达式。
- **`unicode-ident`**：表达式为 `(MIT OR Apache-2.0) AND Unicode-3.0`，采用 `MIT AND Unicode-3.0`，同时保留 MIT 与 Unicode-3.0 通知。
- **`Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`**：按其 `MIT` 备选分支计入。

SPDX 表达式按括号、AND、OR 与 WITH 解析；旧 Cargo `/` 语法作为 OR。缓存绑定 package ID、source、manifest 哈希、上游提交/path 和包校验信息，不能只按名称版本复用。缺正文的标准文本来自 [SPDX license-list-data v3.28.0](https://github.com/spdx/license-list-data/tree/c4a7237ec8f4654e867546f9f409749300f1bf4c)，固定 commit `c4a7237ec8f4654e867546f9f409749300f1bf4c`；每包清单记录正文/源码来源、SHA-256 和 fallback 标记。

收集器为构建期 Python 工具，产品运行期仍为纯 Rust。`python -m unittest discover -s scripts -p test_collect_third_party_licenses.py` 的 3 项回归覆盖 AND 义务、缓存身份和标准正文/源码回退；原始 manifest 与日志位于 `.local/reports/m2/b-review/licenses-v3/`。最终 M2 门禁与两通道打包重新收集通过，全部正文、归档及 `ATTRIBUTION.json` 已放入各包 `third-party-licenses/`；两个 ZIP 各 2443 个清单文件的哈希校验通过，源码指纹与产物 SHA 见 [阶段报告](../delivery/stage-report.md)。

## 3. 非 crate 与独立 POC 边界

`libsqlite3-sys` 的绑定源码为 MIT；bundled SQLite 的许可来源为 [SQLite Public Domain](https://www.sqlite.org/copyright.html)，分发清单分别保留绑定版权与 SQLite 的来源声明。

日历夹具引用 `exchange_calendars` 的 Apache-2.0 数据文件；出处与使用范围见 [PROVENANCE](../../tests/fixtures/PROVENANCE.md#1-交易日历夹具testsfixturescalendar)。行情数据的 `public_open` / `official_free_personal` / `unofficial_personal` 只是来源条款类别，不由代码许可授予再分发权。

官方 ccxt `=4.5.91` 是 `.local/poc/` 下的独立兼容性实验，不计入产品锁文件。其构建、依赖版本和 f64/栈边界见 [POC-D06](2026-11-08-ccxt-rust-poc.md)。
