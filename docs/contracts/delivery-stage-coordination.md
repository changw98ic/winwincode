# Delivery 阶段、任务、会话绑定与 Attention 规则

这是一道**目标门禁，不是实现完成声明**。机器可读规则在
[`delivery-stage-coordination.rules.json`](delivery-stage-coordination.rules.json)。阶段
`winwincode-9c4.16.2.3` 的 Rust 代码出现后，测试会要求它提供完整模块和黑盒测试；在
Rust 模块出现前，本文件只固定要实现的结果。

## 谁负责什么

Rust Control Plane 是 Delivery 状态的唯一写入方。`winwincode-delivery` 判断当前
Delivery 的合法下一步，Control Plane 提交状态和 outbox 后，才通过 `ExecutionPort`
把工作交给 Worker。Worker 不能直接改 Delivery。

Codex Core 继续负责 Plan、Agent、工具和实际执行。Control Plane **不保存 Codex
Plan、Agent Graph 或 Tool Call**，也不建立另一套 Codex 调度器。一次需要 Codex 的
交付阶段只创建一个 `ExecutionJob`；Job 使用现有的 `DeliveryStageExecutionScope`，明确
携带当前 ProductSession、Delivery、DeliveryTask（如果有）和 StageRun 身份。

## 合法下一阶段

一个 Delivery 同时最多只有一个活动 StageRun。`running` 和 `waiting` 都算活动；
`succeeded`、`failed` 和 `cancelled` 都已经结束。调用方只提交 `delivery.advance`，不能
自己指定一个方便的下一阶段或 attempt。

| 当前 Delivery 状态 | 当前活动阶段 | 唯一可开始阶段 | 开始后的 Delivery 状态 |
| --- | --- | --- | --- |
| `draft` | 无 | `clarifying` | `clarifying` |
| `clarifying` | 无 | `clarifying` | `clarifying` |
| `ready` | 无 | `planning` | `planning` |
| `planning` | 无 | `planning` | `planning` |
| `planning` | `planning` | `plan-review` | `needs-attention` |
| `executing` | 无 | `executing` | `executing` |
| `executing` | `executing` | `verifying` | `verifying` |
| `verifying` | 无 | `verifying` | `verifying` |
| `verifying` | `verifying` | `verifying` | `verifying` |
| `reworking` | 无 | `reworking` | `reworking` |
| `reworking` | `reworking` | `verifying` | `verifying` |
| `ready-to-deliver` | 无 | `delivery-review` | `needs-attention` |

开始下一阶段是一项原子变更，顺序是：

1. 核对当前 revision 和 request；
2. 确认只有一个合法下一阶段；
3. 阶段交接时结束已经绑定 Session 的前一个 StageRun；
4. 写入新的 StageRun；
5. 更新 DeliveryTask 和 Delivery 状态；
6. 人工审核阶段同时写入关联的开放阻塞 AttentionItem；
7. Codex 阶段同时写入 ExecutionJob intent；
8. 状态和 outbox 在同一个事务提交后再发送 Job。

固定角色如下：

| 阶段 | 执行者 | 角色 |
| --- | --- | --- |
| `clarifying` | Codex | `requirements` |
| `planning` | Codex | `planner` |
| `plan-review` | 人 | `reviewer` |
| `executing` | Codex | `executor` |
| `verifying` | Codex | `reviewer`、`verifier`、`adversarial-verifier` |
| `reworking` | Codex | `remediator` |
| `delivery-review` | 人 | `approver` |

## 恢复与取消

程序重启后恢复活动阶段时，继续使用原来的 StageRun ID、attempt、ExecutionJob ID 和已
接受的 SessionBinding，**不会创建第二个 StageRun**。恢复过程必须重新核对完整身份；
仅在日志里看到模型的最终文字，不等于 Worker 已经提交合法的终态结果。

取消是两步过程：

1. Control Plane 发送 `job.cancel`；
2. 当前租约内的 Worker 返回 `job.outcome`，且结果是 `cancelled`。

`job.cancel_ack` 只表示 Worker 收到了取消请求，并不表示执行已经结束。收到带有正确
Job、attempt、Lease 和 fencing 身份的终态结果后，Control Plane 才把同一个 StageRun
标为 `cancelled`、写入结束时间并释放活动阶段位置。它不会自动创建替代运行。关联任务
回到本阶段可重试的状态：执行取消回到 `pending`，验证取消保持 `verifying`，返工取消
回到 `failed`。

## DeliveryTask 图和状态

`delivery.create` 的 tasks 必须为空。规划结果在当前 `SolutionReviewProjection` 中提出非空、
有序的 `taskProposals`，并把完整任务内容和顺序纳入 `reviewSetSha256`。人工审核状态变成
`approved` 后，调用方只向 `delivery.approve_task_breakdown` 提交 `deliveryId` 和同一个
`reviewSetSha256`；Control Plane 从可信 review set 逐字段提升任务，一次性写入当前 Spec
revision 的任务图。Planner proposal 不含 owner，提升后 owner 固定为 `null`、状态固定为
`pending`；后续责任人只能通过已认证的 assignment command 设置。调用方不能在该 command 中
另交或替换 tasks。Codex Plan 不会被复制成 DeliveryTask。已经批准的图不能用普通看板编辑
改写；要换图，先提交新的 Spec revision 并重新规划、审核。

批准的图至少有一个任务，并同时满足：

- 每个任务至少关联当前 Spec 的一项 AcceptanceCriterion；
- 每项依赖都属于同一个 Delivery；
- 任务不能依赖自己；
- 依赖不能形成环。

任务只有在**所有依赖任务都已经 completed** 时才可开始。Control Plane 只选择一个
当前可运行的产品任务并形成阶段 Job，不在这里调度 Codex Plan 或子 Agent。当前产品
规则仍保持一个活动 StageRun，因此多个可运行任务不会同时创建多个交付阶段。

任务状态由交付事实改变，不接受自由填写：

| 原状态 | 事实 | 新状态 |
| --- | --- | --- |
| `pending` | 开始执行 | `active` |
| `active` | 开始验证 | `verifying` |
| `verifying` | 验证通过 | `completed` |
| `verifying` | 验证失败 | `failed` |
| `failed` | 开始返工 | `active` |
| `active` | 执行取消 | `pending` |
| `verifying` | 验证取消 | `verifying` |
| `active` | 返工取消 | `failed` |

## SessionBinding 必须精确

ProductSession、WorkerSession、CodexThread 和 StageRun 是不同身份。SessionBinding 还要
记录 Delivery、可选 DeliveryTask 和 ExecutionJob，不能压成一个含义不清的
`sessionId`。

创建 Job 时已经知道 Delivery、Task、StageRun、ProductSession 和 ExecutionJob。Worker
接受派发后补上 WorkerSession；Worker 报告线程后再补上 CodexThread。Delivery 级阶段
可以没有 DeliveryTask，其他身份只能按上述生命周期暂时为空。任何外来、冲突或重复
绑定都会停止恢复，不会猜一个 Session 后继续。

## Attention 阻塞

只要存在当前、开放且阻塞的 AttentionItem，`delivery.advance` 就不会创建或派发 Job。
`plan-review` 和 `delivery-review` 必须在开始时同时创建一个与审核 StageRun 关联的开放
阻塞项。

解决 Attention 时必须同时匹配当前 Delivery revision、操作者、AttentionItem、关联
StageRun 和冻结上下文。还有其他阻塞项时，Delivery 继续保持 `needs-attention`。解决
命令只提交人的决定，不在同一命令里偷偷启动执行；下一次明确的 `delivery.advance`
才开始阶段。

Codex 的命令、文件或网络审批仍是 ExecutionPort 的 Approval，不会伪装成业务
AttentionItem。

## requestId 重放

同一个 requestId 与同一 command、actor、scope、expectedRevision 和 payload digest
再次到达时，Control Plane 返回第一次保存的结果。它不会再写 StageRun、
SessionBinding、ExecutionJob 或 outbox，也不会再次派发 Job。同一个 requestId 如果
对应不同内容，返回 `IDEMPOTENCY_CONFLICT`。

这条规则与 Control Plane 的事务/outbox 顺序共同保证：提交成功但发送中断时只恢复同
一个 Job，而不是重复执行阶段。

## Rust 实现出现后的检查

实现需要提供：

- `winwincode-delivery` 下的 stage、task、session_binding 和 attention application 模块；
- `winwincode-control-plane` 下只负责把当前阶段意图变成生成类型 `ExecutionJob` 的
  delivery_execution 模块；
- Delivery 生命周期黑盒测试和 Control Plane 派发黑盒测试；
- 符合 ADR-0028 目标依赖图的 Cargo 依赖；
- 不直接依赖 Codex Core、旧 kernel/native 或 N-API；
- 不在 Control Plane 重新声明 `ExecutionJob` 或 `DeliveryStageExecutionScope`。

这些检查通过只说明阶段、任务、绑定、Attention 和 Job 派发满足本合同。任务是否关闭
仍由实际 Rust 测试结果和 Beads 记录决定。
