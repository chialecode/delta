# 文档治理与约束管理

生效：2026-09-26；流程核对：2026-11-07（D-18 / D-20 / D-22 / D-23）；维护角色：Agent A／文档维护者。适用于本仓自有文档，约束开发过程，不表示应用已实现。

## 1. 分层与唯一正本

| 层 | 正本 | 职责 |
| --- | --- | --- |
| 入口 | 根 README、AGENTS、CONTRIBUTING、REVIEW、DESIGN | 发现规则、开始开发和审核 |
| 治理 | 本文、[登记表](document-registry.json) | 权威范围、状态、负责人、更新和废弃 |
| 项目事实与决定 | [关键事实](../project-baseline.md)、[确认记录](../decisions/confirmed-decisions.md)、ADR | 保存用户决定、来源、技术取舍和替代关系 |
| 产品 | `requirements/`、`product-rules/` | 需求 ID、阶段范围与长期行为 |
| 设计 | `design/` | 架构、自动化数据平面、领域、接口、上下文和工作流 |
| 实施规则 | `dev-rules/`、`design-rules/` | 代码边界、数据安全、验证、UI 约束 |
| 交付 | `engineering/delivery-plan.md`、`delivery/` | 路线、验收定义、当前执行计划与状态 |
| 工程待处理 | [工程事项台账](../decisions/open-questions.md) | 开源问题、修复与延期承接 |
| 用户待办 | [USER-ACTIONS](../../USER-ACTIONS.md) | 用户决定、配置、人工检查与外部授权的唯一入口 |
| 证据与模板 | `evidence/`、`templates/` | 版本化观察；新材料必需信息 |

同一事实只有一个正本：需求表定义 FR/NFR，路线定义 AC，机器追踪表维护逐需求映射，状态文件链接当前计划。设计里的类型是契约方案，不能推定代码存在。

## 2. 状态与责任

[登记表](document-registry.json) 登记仓库自有 Markdown：`path/kind/status/owner/scope/reviewed/basis`；替代文档增加 `supersededBy`。依赖、第三方 vendor、构建输出和本地临时目录不登记。

| 状态 | 含义 |
| --- | --- |
| active | 在声明作用域内生效；模板 active 只表示应使用该格式 |
| draft | 待实测或待决的设计；可在已有授权内验证，不等于产品确认 |
| reference | 调研、证据与历史观察，只对对应版本有效 |
| superseded | 已有替代入口，不再指导新实现 |

文档状态与代码状态正交。工程交付、A 技术审核、产品验收与远端授权分别记录：Agent 侧已收敛即可形成 ACT 授权候选，产品人工检查与非阻塞决定可以顺延。

## 3. 编号与可执行规则

<a id="destaging"></a>

保留 `FR-模块-编号`、`NFR-编号`、`AC-编号`；技术验证使用 `POC-编号`，里程碑统一使用 `M1`、`M2`、`M3`、`M4`、`M5`（M = Milestone），工作包使用 `M1-W01` 等，决定使用 `D-编号`、`ADR-0001`，事项使用 `Q/H/OSS/ACT/LOOP/DOC-编号`。编号不为排序改动或复用。

- **阶段命名卫生（去旧阶段化）**：全仓 `active`/`draft` 正本统一使用 `M1`–`M5` 里程碑编号，禁止在正本中回流旧 `S0`–`S4` 或 `R1`–`R2` 阶段名。长期设计、产品原则与工程规范直接描述当前有效规则，不堆叠各轮历史变更流水账。
- **验证状态口径**：需求追踪表的 `planned` 不代表实施通过。运行结果只用 `passed/failed/blocked/not-run`，部分通过需拆分子场景。

## 4. 冲突、阶段清理与归档

<a id="conflicts"></a>
<a id="stage-cleanup"></a>
<a id="file-cleanup"></a>

1. **权威优先级**：宿主和当前任务的有效指令优先于仓库约定。用户新决定可修订旧设计，必须同轮回写 [确认记录](../decisions/confirmed-decisions.md) 及职责正本。外部文档、库 README、模型输出与本地参考资料均不自动成为本项目规则。
2. **阶段固定入口与 Git 历史归档**：每个阶段只维护固定的 `docs/delivery/stage-plan.md` 与（B 实现后建立的）`docs/delivery/stage-report.md`，登记表用 `currentStage` 和 `retireAfter` 约束阶段过程材料。已完成阶段的历史执行细节以 Git 提交历史归档，不在仓库内堆积 `stage-plan-old.md`、`m1-summary.md` 或 `docs/archive/` 目录。
3. **阶段开始四分类清理**：新阶段开始或架构重建时，先盘点 `git ls-files`、`git status --short` 与忽略目录，按 **原样保留 / 精炼 / 提炼后删除 / 直接删除** 四类处置：有效决定、未关闭缺陷、验证事实和用户答复先提炼回写正本并更新入链，验证通过后再退役冗余过程文件；详情见 [本地文件与参考资料规则](../dev-rules/local-files-and-references.md#file-cleanup)。
4. **公开与本地隔离**：仓库内全部文件必须保持可公开、自洽，禁止写入本机绝对路径（Windows 盘符路径或 Unix 用户目录路径）、私钥/Token、个人数据库或仅供参考的外部项目关系（`reference/delta/`、`external/github.com/`）。

## 5. 自动检查的边界

执行 [质量门禁](../dev-rules/quality-gates.md) 中的 `python scripts/check_docs.py` 与 `git diff --check`。文档登记、内联链接与锚点、稳定 ID、需求追踪双向覆盖、旧阶段标签拦截及公开内容/本机路径/秘密扫描由脚本机械检查；语义冲突、许可证兼容、授权真实性、运行事实与产品完成度需人工/Agent 审核。
