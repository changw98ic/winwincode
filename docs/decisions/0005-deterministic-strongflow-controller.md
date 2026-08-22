# ADR-0005：StrongFlow 只按程序规则推进和暂停

- 状态：已接受
- 日期：2026-08-21
- 对应任务：`winwincode-9c4.2.3`
- 实现：[`packages/strongflow/src/controller.ts`](../../packages/strongflow/src/controller.ts)

## 结论

`StrongFlowController` 是作业自动推进的唯一入口。它每次都重新读取已经保存的作业状态，由固定映射选择下一项工作，再调用该阶段唯一注册的角色。模型只返回当前阶段规定的数据，不能返回下一状态、批准结果、完成结论或交付结论。

```text
已保存的作业状态
        │
        ▼
程序固定的 state → stage 映射
        │
        ▼
唯一角色提供者 ── 只返回该阶段数据
        │
        ▼
校验结果并先保存事件
        │
        └── 再从新状态选择下一步
```

控制器不使用“主管模型”编写或选择流程。提示词、角色输出和界面参数都不能改变状态转换表。

## 固定执行顺序

正常流程只有以下顺序：

| 已保存状态 | 控制器动作 | 成功后的状态 |
| --- | --- | --- |
| `DEFINING_REQUIREMENTS` | 运行 `REQUIREMENTS` | `DEFINING_SOLUTION` |
| `DEFINING_SOLUTION` | 运行 `SOLUTION` | `DEFINING_DIAGRAMS` |
| `DEFINING_DIAGRAMS` | 运行 `DIAGRAMS` | `AWAITING_HUMAN_REVIEW` |
| `AWAITING_HUMAN_REVIEW` | 不运行角色，只等待人 | 由人工决定 |
| `PLANNING` | 运行 `PLANNING` | `EXECUTING` |
| `EXECUTING` | 运行 `EXECUTION` | `VERIFYING` |
| `VERIFYING` | 运行 `VERIFICATION` | 完成门禁或修复 |
| `REMEDIATING` | 运行 `REMEDIATION` | `VERIFYING` |
| `AWAITING_COMPLETION_GATE` | 运行程序化完成检查 | 交付或修复 |
| `DELIVERING` | 运行 `DELIVERY` | `READY_TO_DELIVER` |
| `READY_TO_DELIVER` | 由系统写入最终交付事件 | `DELIVERED` |

需求、方案和两张默认图完成后，自动运行立即停下。重复调用控制器只返回“等待人工审核”，不追加事件、不调用模型、不启动角色。进程重启后也按保存的 `AWAITING_HUMAN_REVIEW` 得到同样结果。只有已经保存且精确匹配当前四个定义标识的人工批准，才能使状态进入 `PLANNING`。

人工请求修改时，状态机按 `requirements`、`solution` 或 `diagrams` 返回对应阶段。控制器不猜修改范围，只执行已经保存的决定。例如退回方案时保留当前需求，只重新运行方案和两张图，然后再次暂停等待人。

## 单阶段占用

角色运行前必须先保存 `stage.started`，其中包含新的 `StageRunId` 和 `AttemptId`。只有这条事件成功保存后才调用角色。阶段结束时，成功或失败事件必须引用同一组标识和同一角色。

同一控制器内的操作排队执行。多个控制器或进程同时处理同一作业时，不可覆盖的下一事件文件决定胜者：

1. 一个控制器成功发布 `stage.started`；
2. 其他控制器看到活动阶段后保持空闲，或因下一事件已被抢先发布而得到 `CONTROLLER_CONFLICT`；
3. 只有成功发布开始事件的控制器调用角色。

因此，同一阶段不会因两个控制器竞争而运行两次。已经保存但尚未结算的活动阶段也不会被自动重跑；启动时如何判定旧活动阶段失联并写入中断，属于单独的恢复流程。

## 角色结果边界

每个角色只能返回本阶段的精确数据形状。例如：

- 需求返回 `RequirementId`；
- 方案返回与当前需求匹配的 `RequirementId` 和新 `SolutionId`；
- 图阶段返回需求、方案和两项 `DiagramId` 的完整定义；
- 执行返回 `CandidateId`；
- 验证返回同一候选结果和 `passed` 或 `remediation-required`；
- 修复与交付必须继续引用同一候选结果。

额外字段、错误标识、陈旧定义、错误候选结果或模型自报的下一状态都按 `INVALID_STAGE_RESULT` 记录为阶段失败。未知异常只保存统一的基础设施错误，不把内部异常文本直接写入长期事件；角色若要提供具体错误，必须显式抛出已经整理过的 `StrongFlowStageProviderFailure`。

## 完成检查归程序所有

完成检查必须实现 `authority: 'program'`，模型角色不能注册为完成检查。检查只返回：

- `passed`：进入交付；
- `failed` 和非空原因：进入修复，再次验证后重新检查。

完成检查应是可重复执行、只读取当前候选结果和证据的程序检查。检查抛错、被取消或返回其他形状时，作业进入可恢复的 `INTERRUPTED`，不会产生通过事件。

交付角色成功后先进入 `READY_TO_DELIVER`，再由控制器写入系统来源的 `job.delivered`。这样进程即使在两步之间退出，重启后也只补写最终记录，不会再次运行交付角色。

## 失败、中断和取消

控制器在以下时机检查取消或中断信号：阶段开始前、开始事件保存后、角色返回后以及完成检查返回后。活动角色和完成检查都会收到 `AbortSignal`。

- 角色明确失败：保存 `stage.failed`，进入终态 `FAILED`，不保存成功事件；
- 运行信号中断：保存 `job.interrupted`，清除活动阶段，之后必须显式 `resume` 才会重新运行；
- 用户取消：先中止活动调用，再保存中断和取消，最终进入 `CANCELLED`；
- 已经终止或交付的作业：控制器不再执行角色。

同一进程内，取消请求会立即设置在控制器上，而不是等排队轮到取消操作后才生效。控制器在发布成功事件前观察到取消时，会先保存中断而不保存阶段成功；如果成功事件已经原子发布，后到的取消只能停止后续工作，不能改写已经发布的历史。

## 自动推进上限

`runUntilPause` 默认最多执行 64 个会改变状态的动作，调用者可在 `1` 到 `10000` 之间设置更小上限。达到上限后不再启动下一项工作，并返回 `STEP_LIMIT_REACHED`。如果最后一个允许动作恰好进入人工审核，则直接返回审核暂停，不把正常暂停误报为超限。

这个上限防止验证与修复长期循环占住主机。每一次已经完成的动作都已先保存；调用者再次运行时从最后状态继续，不依赖控制器内存。
