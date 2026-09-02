# ADR-0022：CPB 只提供设计知识，不成为运行时或数据源

- 状态：已接受
- 日期：2026-08-22
- 修订日期：2026-08-23
- 对应任务：`winwincode-9c4.12.4`
- CPB 公开源码基线：`changw98ic/codepatchbay@68bb0d591b0333b57a8be863458367123885b52a`
- 当前所有权决定：[ADR-0001](0001-upstream-integration.md)、[ADR-0023](0023-canonical-delivery-ownership.md)
- 自动检查：[`scripts/verify-cpb-boundary.mjs`](../../scripts/verify-cpb-boundary.mjs)、[`scripts/verify-packages.mjs`](../../scripts/verify-packages.mjs)

## 结论

WinWinCode 只采用 CPB 已提交文档中的问题定义、失败经验和证明原则。它不导入 CPB 源码、任务、运行记录或内部数据，也不保留连接旧产品合同的适配器。

所有采用的内容都必须进入当前三层边界：

```text
DSH：交互、模型、凭据、Session 和界面
Codex Core：Plan、Agent Graph、工具、Shell、沙箱、权限和执行恢复
WinWinCode：DeliverySpec、交付阶段、Attention、Evidence 和 Verdict
```

当 CPB 文档与 [ADR-0023](0023-canonical-delivery-ownership.md) 冲突时，后者是唯一有效设计。旧的 Job、固定 Agent 编排、独立工具运行时、独立权限中心和制品总线没有迁入当前产品。

## 审查输入

审查只使用提交 `68bb0d591b0333b57a8be863458367123885b52a` 中已跟踪的文档。审查时 CPB 工作树存在未提交内容；这些文件以及它们表达的新增方案都没有进入本次迁移。

代表性来源如下：

- `docs/architecture/runtime-boundaries.md`
- `docs/architecture/cpb-trace-replay.md`
- `docs/architecture/cpb-hub-registry-consistency.md`
- `docs/architecture/cpb-coding-comparison.md`
- `docs/architecture/cpb-v0.5-runtime-release-stabilization-spec.md`
- `docs/security/cpb-agent-secret-boundary.md`
- `docs/product/cpb-full-product-vision-plan.md`
- `docs/product/cpb-closed-loop-mvp-plan.md`
- `docs/product/cpb-product-entry-execution-kernel-plan-2026-07-27.md`
- `docs/product/cpb-runtime-independent-evolution-plan.md`
- `docs/product/cpb-flagship-validation-gate.md`
- `docs/multi-agent-orchestration-roadmap.md`
- `docs/superpowers/specs/2026-06-12-checklist-first-task-verification-design.md`
- `docs/superpowers/plans/2026-05-13-24h-unattended-fixed-role-agents.md`
- `docs/superpowers/plans/2026-07-28-cpb-agent-platform-maturity-rfc.md`

运行证据、评测结果、项目记录、Wiki、故障日志、生成文件和 `docs/product/evidence/**` 只属于原项目历史，不是可导入数据。

## 取舍

| 主题 | 处理 | 当前归属 |
| --- | --- | --- |
| 执行内核与产品外壳分开 | 采用并重写 | DSH 负责产品外壳；Codex Core 是唯一执行内核 |
| 人工审核后再进入执行 | 采用原则并重写 | WinWinCode 保存业务 Attention 和阶段决定；执行审批仍由 Codex/DSH 负责 |
| 可重放状态和准确身份 | 采用并重写 | Delivery 使用追加记录；Codex 与 DSH 运行状态继续由各自所有者保存 |
| 验收条件、直接证据和完成判断 | 只采用证明原则 | `AcceptanceCriterion`、`EvidenceRef`、`CriterionResult`、`DeliveryVerdict` |
| 图上展示准确差异 | 采用原则并重写 | 从 Codex Diff 生成只读视图，不建立第二份差异或执行记录 |
| 固定 Agent 编排、角色交接和自建工具运行时 | 删除，不迁移 | Codex Multi-Agent、工具和权限是唯一权威 |
| Hub、队列、worker、Redis、ACP、OPC、tokenAgent 和旧 Web 路由 | 删除，不迁移 | 当前产品没有对应运行入口 |
| Experience Ledger、Channel Gateway、Browser Agent 和通用 Provider Registry | 删除，不迁移 | DSH 已拥有通用交互、Provider 和插件边界 |

## 明确排除

WinWinCode 不复制、不转换、不挂载以下内容：

- CPB 任务、Beads/Dolt 数据、队列、作业数据库、事件数据库和注册表；
- 日志、trace、运行证据、评测记录、生成报告和 Wiki 项目记录；
- 会话、worktree、补丁缓存、进程锁、PID、临时目录、恢复记录和 worker 状态；
- 凭据、环境文件、用户配置、模型配置、认证缓存和密钥材料；
- CPB 源码模块、包依赖、命令、内部导入路径、运行目录和环境变量；
- 审查基线之外的本地草稿；
- 为旧版本保留的读取器、转换器、别名、回退开关和双写路径。

WinWinCode 不提供 CPB 数据迁移器。

## 自动边界

`node scripts/verify-cpb-boundary.mjs` 检查会参与构建或运行的源码、脚本、工作流和依赖清单，拒绝 CPB 包名、导入、命令、状态目录和运行配置。

`node scripts/verify-packages.mjs` 检查真实 npm 打包清单，拒绝 CPB 运行标记和已经删除的旧 StrongFlow Job 文件。两项检查只能证明当前源码与包中没有已知运行依赖；产品能力是否完成仍由 Delivery、投影、界面和端到端测试分别证明。
