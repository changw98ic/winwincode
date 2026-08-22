# ADR-0006：StrongFlow 固定为八个受约束角色

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.3.1`
- 实现：[`packages/contracts/src/strongflow-role.ts`](../../packages/contracts/src/strongflow-role.ts)

## 结论

StrongFlow 使用八个固定角色，不增加可以改流程、批准定义或宣布交付成功的“主管角色”。每个角色都通过同一套版本化配置取得模型路线、推理强度、时间与费用上限、系统指令、权限预设、工作区模式和可接收/必须产出的制品种类。

```text
DSH 模型目录 ── 校验 provider / model / reasoning
                         │
八个运行分配 ────────────┤
                         ▼
              canonical RoleSpec × 8
                         │
                         ▼
            后续 Codex Core 会话创建
```

配置只能选择 DSH 已提供的模型路线并设置正数上限，不能改写角色责任、替换权限预设、提升工作区权限或替换系统指令。这样，模型兼容性仍来自 DSH，而流程权限由 WinWinCode 固定。

## 八个角色

| 角色标识 | 显示名称 | 主要输入 | 必须输出 | 工作区 |
| --- | --- | --- | --- | --- |
| `requirements` | Requirements Analyst | 用户请求 | `RequirementSpec` | 源码只读 |
| `solution` | Solution Architect | 一个 `RequirementSpec` | `SolutionDesign`、系统架构图、流程流转图 | 源码只读 |
| `planner` | Planner | 完整定义和匹配的人工批准 | `ExecutionPlan` | 源码只读 |
| `executor` | Executor | 已批准定义和执行计划 | `PatchManifest` | 候选工作区可写 |
| `reviewer` | Reviewer | 冻结候选和执行依据 | `ReviewReport` | 候选工作区只读 |
| `verifier` | Verifier | 候选、评审和验收依据 | `VerificationReport` | 候选工作区只读 |
| `adversarial-verifier` | Adversarial Verifier | 候选及已有评审、验证证据 | 独立 `VerificationReport` | 候选工作区只读 |
| `remediator` | Remediator | 有界修复请求和候选证据 | 新 `PatchManifest`、`RemediationReport` | 候选工作区可写 |

`DeliveryReceipt` 由后续程序化交付路径产生，不为此增加第九个模型角色。

## 需求与方案必须分开

Requirements Analyst 只能产生 `REQUIREMENT_SPEC`。它的固定指令明确禁止选择架构、实现、文件、命令、补丁、模型路线或批准结果。

Solution Architect 只接受一个 `REQUIREMENT_SPEC`，并产出：

1. 引用该需求身份的 `SOLUTION_DESIGN`；
2. `SYSTEM_ARCHITECTURE_DIAGRAM`；
3. `PROCESS_FLOW_DIAGRAM`。

因此，需求输出中不能夹带方案，方案也不能脱离需求单独出现。Planner 必须同时收到四项定义制品和匹配的 `HUMAN_REVIEW_RECORD`，否则后续角色创建流程没有完整输入。

## 模型路线来自 DSH

每个角色的运行分配包含：

- DSH `provider`；
- DSH `model`；
- `reasoningEffort`，可以是模型目录声明的字符串或 `null`；
- `maxTurns`；
- `maxWallTimeMillis`；
- `maxTotalTokens`；
- `maxCostUsdMicros`。

WinWinCode 启动时把配置与注入的 DSH 模型目录逐项核对。未知 provider/model 组合、模型不支持的推理强度、空目录、重复模型或非正数上限都会直接阻止启动。配置不硬编码某一家模型，DeepSeek、Anthropic 或其他 DSH 已支持路线使用同一验证入口。

## 权限预设与工作区

角色配置不再重复携带可被局部修改的工具和沙箱字段，而是只引用一个内置 `permissionPreset`。完整的文件、工具、进程、网络、审批、预算、发布、凭据和审计规则统一由 [`strongflow-permission.ts`](../../packages/contracts/src/strongflow-permission.ts) 解析。可见模型工具仍只有八种固定能力：读取/写入制品、读取工作区、搜索代码、查看候选差异、运行命令、运行测试和修改候选。

只有 Executor 和 Remediator 同时满足以下三项：

- `workspaceMode: candidate-write`；
- 权限预设的 `filesystem.mode: candidate-write`；
- 权限预设的工具列表包含 `candidate.patch`。

其他六个角色全部只读。Reviewer、Verifier 和 Adversarial Verifier 只能执行已冻结的验证探针，不能修改被冻结的候选。所有模型角色默认禁用网络、排除原始凭据、不能作出定义或运行权限决定，也不能直接远程发布。

需求定义的人工审核仍由 `StrongFlowHumanReviewGate` 单独处理；运行中的网络、凭据、权限、预算和远程发布请求使用另一类带来源身份的操作决定。模型角色没有人工批准工具，只能发出受支持的请求，不能批准自己的请求。

## 只接受一条当前合同

当前配置版本为 `2`，解析后角色按固定顺序输出。输入顺序可以不同，但不能缺少、重复或增加角色。以下安全相关字段必须与内置策略完全相同：

- 显示名称；
- 系统指令；
- 权限预设；
- 工作区模式；
- 输入制品列表；
- 输出制品列表。

额外字段、未知版本、未知制品、旧角色名和修改过的策略都会失败，不保留旧合同或宽松回退。经验证的配置会完整冻结，JSON 序列化后重新解析得到同一份规范配置。
