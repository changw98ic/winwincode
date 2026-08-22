# ADR-0022：CPB 只提供设计知识，不成为 WinWinCode 的运行时或数据源

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.12.4`
- CPB 公开源码基线：`changw98ic/codepatchbay@68bb0d591b0333b57a8be863458367123885b52a`
- 边界检查：[`scripts/verify-cpb-boundary.mjs`](../../scripts/verify-cpb-boundary.mjs)、[`scripts/verify-packages.mjs`](../../scripts/verify-packages.mjs)
- 测试：[`tests/cpb-design-migration.test.mjs`](../../tests/cpb-design-migration.test.mjs)

## 结论

WinWinCode 只读取 CPB 已提交、可公开核对的设计文档，从中保留仍然适用的问题定义、流程原则和失败教训，再用 WinWinCode 当前的 DSH 界面、单一内嵌 Codex Core、八个固定角色和正式制品格式重新表达。

CPB 不进入 WinWinCode 的运行时。WinWinCode 不导入 CPB 源码模块，不读取 CPB 任务或状态，不提供 CPB 数据迁移器，也不保留连接 CPB 旧路径的适配器。相同问题已经由 WinWinCode 的正式决定覆盖时，该决定是唯一有效入口；本文件只记录设计来源和取舍，不建立第二套产品合同。

## 审查输入边界

本次审查只使用 CPB Git 提交 `68bb0d591b0333b57a8be863458367123885b52a` 中已经跟踪的文档。审查时 CPB 本地工作树有未提交内容；这些文件以及它们表达的新增方案都没有进入本次迁移。以后若要吸收新的 CPB 设计，必须先固定新的公开提交，再单独审查和更新本决定，不能从某台机器的工作树直接复制。

以下内容即使位于该公开提交中，也只作为历史设计资料阅读，不作为可导入数据：运行证据、评测结果、项目记录、Wiki 记录、故障日志和生成文件。源路径只用于说明观点来自哪里，不表示 WinWinCode 在构建或运行时需要 CPB 仓库。

## 设计主题清单

| CPB 设计主题 | 代表性的已提交来源 | 处理 | WinWinCode 唯一入口 | 在 WinWinCode 中的准确含义 |
| --- | --- | --- | --- | --- |
| 人工控制的本地闭环 | `docs/product/cpb-full-product-vision-plan.md`、`docs/product/cpb-closed-loop-mvp-plan.md` | 采用并重写 | [ADR-0002](0002-strongflow-job-language.md)、[ADR-0004](0004-human-review-gate.md)、[ADR-0005](0005-deterministic-strongflow-controller.md) | 需求、方案、系统架构图和流程图必须先成为四项准确制品，再由已认证的人审核；只有批准记录与四项当前身份完全一致时才能进入计划和执行。退回标注形成新的修改回合。CPB Hub 或远程 PR 都不是流程前提。 |
| 执行内核与产品外壳分开 | `docs/architecture/runtime-boundaries.md`、`docs/product/cpb-runtime-independent-evolution-plan.md`、`docs/product/cpb-product-entry-execution-kernel-plan-2026-07-27.md` | 采用原则并重写 | [ADR-0001](0001-upstream-integration.md)、[ADR-0005](0005-deterministic-strongflow-controller.md)、[ADR-0017](0017-shared-operator-interface.md)、[ADR-0018](0018-local-operator-service.md)、[ADR-0020](0020-governed-role-kernel-authority.md) | 一个内嵌 Codex Core 是聊天和 StrongFlow 的唯一执行权威；DSH 提供界面和模型适配，WinWinCode 提供流程与持久状态。不会启动 CPB worker、外部编程 Agent 或第二套执行循环。 |
| 固定角色、多 Agent 协作和受控交接 | `docs/superpowers/plans/2026-05-13-24h-unattended-fixed-role-agents.md`、`docs/multi-agent-orchestration-roadmap.md` | 采用并收窄 | [ADR-0006](0006-canonical-strongflow-roles.md)、[ADR-0011](0011-governed-role-session-lifecycle.md)、[ADR-0012](0012-typed-role-turn-results.md)、[ADR-0016](0016-controlled-durable-handoffs.md) | 固定为需求分析、方案设计、计划、执行、审查、验证、对抗验证和修复八个角色。每个角色使用同一内核中的独立受管会话；输入由程序按当前状态选择，交接内容持久化并可重建，不复用 CPB 的提供商进程或角色定义。 |
| 可重放状态、崩溃恢复和准确所有权 | `docs/architecture/runtime-boundaries.md`、`docs/architecture/cpb-hub-registry-consistency.md` | 采用并重写 | [ADR-0002](0002-strongflow-job-language.md)、[ADR-0003](0003-strongflow-job-store.md)、[ADR-0008](0008-isolated-git-worktree-lifecycle.md)、[ADR-0010](0010-workspace-crash-reconciliation.md)、[ADR-0016](0016-controlled-durable-handoffs.md)、[ADR-0018](0018-local-operator-service.md) | 状态由原子追加的正式事件重建；工作区、锁和进程必须绑定可核验身份。损坏或无法证明持有者已死亡时停止恢复，不读取 CPB Hub 注册表，也不按单独 PID 猜测所有权。 |
| 候选隔离、冻结、重放和独立检查 | `docs/architecture/cpb-trace-replay.md`、`docs/architecture/cpb-coding-comparison.md`、`docs/product/cpb-flagship-validation-gate.md`、`docs/architecture/cpb-v0.5-runtime-release-stabilization-spec.md` | 采用并重写 | [ADR-0007](0007-workspace-and-candidate-identity.md)、[ADR-0008](0008-isolated-git-worktree-lifecycle.md)、[ADR-0009](0009-candidate-freeze-and-verification-snapshots.md)、[ADR-0015](0015-content-addressed-artifact-store.md) | 每项工作绑定准确的 Git 基准、文件树、候选、差异和摘要；验证只检查冻结后的独立副本。外部评测若以后加入，只能在执行结束后读取准确候选，不能把评测答案提供给执行角色。 |
| 验收清单、证据和完成判断 | `docs/superpowers/specs/2026-06-12-checklist-first-task-verification-design.md`、`docs/architecture/cpb-trace-replay.md`、`docs/architecture/cpb-v0.5-runtime-release-stabilization-spec.md` | 采用并重写 | [ADR-0002](0002-strongflow-job-language.md)、[ADR-0005](0005-deterministic-strongflow-controller.md)、[ADR-0012](0012-typed-role-turn-results.md)、[ADR-0015](0015-content-addressed-artifact-store.md)、[ADR-0016](0016-controlled-durable-handoffs.md) | 人工批准后的需求验收条件和方案成为本次执行的固定定义。完成状态由程序检查当前候选、直接证据、审查和验证制品得出；角色口头声称成功、旧证据或缺失证据都不能解锁交付。CPB 的旧清单回退格式不保留。 |
| 进程、文件、网络和凭据边界 | `docs/security/cpb-agent-secret-boundary.md`、`docs/architecture/runtime-boundaries.md` | 采用并强化 | [ADR-0019](0019-role-permission-and-approval-matrix.md)、[ADR-0020](0020-governed-role-kernel-authority.md)、[ADR-0021](0021-governed-process-credential-and-audit-boundaries.md) | 每个角色使用固定工具和环境名单；命令必须经过当前平台的操作系统沙箱；凭据只在 DSH 实际模型调用时解析；持久化前再次拒绝凭据内容并写脱敏审计。没有可用沙箱时直接停止，不继承 CPB 的 Agent 环境。 |
| 面向人的稳定视图和本地操作入口 | `docs/product/cpb-product-entry-execution-kernel-plan-2026-07-27.md`、`docs/product/cpb-full-product-vision-plan.md`、`docs/product/cpb-closed-loop-mvp-plan.md` | 采用并重写 | [ADR-0017](0017-shared-operator-interface.md)、[ADR-0018](0018-local-operator-service.md) | DSH 高级工作台与 CLI 读取同一个本地操作接口。公开结果显示产品状态、制品和可审核证据，不暴露内部存储路径、原生句柄、供应商对象或凭据。远程发布只是需要人工允许的最后动作。 |
| 图上展示执行差异 | `docs/product/cpb-full-product-vision-plan.md` 中的 Review Bundle 与 Trace Explorer 原则 | 只保留“让人审核准确差异”的原则；三状态是 WinWinCode 新设计 | [ADR-0013](0013-canonical-strongflow-artifacts.md)、[ADR-0014](0014-deterministic-definition-diagrams.md)、[ADR-0017](0017-shared-operator-interface.md) | 同一张系统架构图和流程图复用稳定节点：执行前全绿；执行中有变化的节点实时变浅蓝且不能读取细节；执行结束后变黄并可打开准确文件、变更块和说明。人工标注必须绑定当前候选、差异、图和节点。 |
| 发布与评测证据 | `docs/product/cpb-flagship-validation-gate.md`、`docs/architecture/cpb-coding-comparison.md`、`docs/architecture/cpb-trace-replay.md` | 只采用证明原则 | 本决定、[ADR-0009](0009-candidate-freeze-and-verification-snapshots.md)、[ADR-0015](0015-content-addressed-artifact-store.md) 和仓库发布检查 | 发布证据必须绑定固定源码、准确候选和真实目标平台；执行后的独立检查不能影响执行输入。CPB 的评测命令、载荷、历史得分和运行记录都不迁移，完整外部评测产品也不在本任务中冒充已实现。 |
| 单一来源、深模块和硬切换 | `docs/architecture/runtime-boundaries.md`、`docs/architecture/cpb-v0.5-runtime-release-stabilization-spec.md`、`docs/product/cpb-product-entry-execution-kernel-plan-2026-07-27.md` | 采用原则，删除相反做法 | [ADR-0001](0001-upstream-integration.md) 及全部当前正式决定 | 每项能力只有一个正式合同和一个实现入口。CPB 文档中用于保留旧调用方的适配器、环境开关、旧格式回退和新旧双轨方案全部舍弃。 |
| Experience Ledger、Channel Gateway、Browser Agent、通用 Provider Registry 和常驻自治路线 | `docs/product/cpb-full-product-vision-plan.md`、`docs/multi-agent-orchestration-roadmap.md`、`docs/superpowers/plans/2026-07-28-cpb-agent-platform-maturity-rfc.md` | 删除或推迟 | 当前没有运行时入口 | 这些内容不是当前 WinWinCode 必需能力，不复制到现有流程。以后若有独立需求，必须按 DSH 与内嵌 Codex 架构重新设计。 |
| CPB 专属运行表面 | `docs/product/cpb-runtime-independent-evolution-plan.md`、`docs/product/cpb-full-product-vision-plan.md`、`docs/product/cpb-closed-loop-mvp-plan.md` | 删除，不迁移 | 无 | CPB 队列、Hub、worker、ACP、Redis、OPC、tokenAgent、历史 Web 路由、命令、环境变量和提供商结构都不是 WinWinCode 依赖；相应职责已经由本地作业服务、DSH 或内嵌 Codex Core 重新定义，未被重新定义的部分不进入产品。 |

## 明确排除的数据与旧路径

以下内容不复制、不转换、不挂载，也不提供读取入口：

- CPB 的任务、Beads/Dolt 数据、项目清单、队列、作业数据库、事件数据库和注册表；
- 日志、trace、运行证据、评测记录、生成报告、Wiki 项目记录和 `docs/product/evidence/**`；
- 会话、worktree、补丁缓存、进程锁、PID、临时目录、恢复记录和 worker 状态；
- 凭据、环境文件、用户配置、模型配置、认证缓存和任何密钥材料；
- CPB 源码模块、可执行命令、包依赖、内部导入路径、运行目录和环境变量；
- Hub、Redis、ACP、OPC、tokenAgent、旧 Web 页面和其他 CPB 专属接口；
- 审查基线提交之外的本地文件，包括未提交的规格草稿；
- 仅为旧版本继续工作的读取器、转换器、别名、回退开关和双写路径。

## 自动边界

`node scripts/verify-cpb-boundary.mjs` 检查当前 Git 工作树中会参与产品构建或运行的源码、脚本、工作流和依赖清单。它拒绝 CPB 包名、导入、命令、状态目录和运行配置标记，也拒绝 JavaScript、pnpm 和 Cargo 清单中的 CPB 依赖。文档可以说明本决定，但不因此成为运行时依赖。

`node scripts/verify-packages.mjs` 继续使用每个 npm 包的真实打包清单，并额外检查将要发布的文本内容和路径。包内若出现 CPB 运行标记、依赖声明或内部状态文件，发布检查直接失败。原生二进制不按文本猜测内容，但它的固定源码身份、文件名单、摘要和许可仍由原有原生包检查覆盖。

这两项检查证明当前仓库和当前发布包没有已知 CPB 运行依赖或内部状态；它们不把“没有命中字符串”冒充设计迁移完成。因此测试还会核对本文件固定了公开来源提交、列出采用/重写/删除三种处理，并保持 README 指向这一唯一清单。

## 当前实现范围

稳定制品、两张定义图、安全渲染、图节点身份、执行中禁止差异详情和执行结束后允许详情的操作事件已经有正式合同与测试。DSH 工作台中的实际页面由 `winwincode-9c4.9.5` 接入，三种图状态和点击审核由 `winwincode-9c4.9.6` 实现；在这两个任务完成前，本决定不会把它们描述成已经可用的界面。

本任务也没有实现 CPB 的完整外部评测、Experience Ledger、Channel Gateway 或通用 Agent 注册平台。它只完成设计知识的公开取舍、归位和禁止运行数据进入项目的可重复检查。
