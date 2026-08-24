# ExecutionPort v1 合同

`ExecutionPort` 是 Rust Control Plane 与 Rust Execution Worker 之间唯一的执行边界。
机器可读的合同是
[`schema/winwincode/v1/execution-port.schema.json`](../../schema/winwincode/v1/execution-port.schema.json)。
本文件解释该合同的状态语义，不建立第二份类型定义。

## 所有权边界

- Control Plane 拥有 Scheduler、Worker Registry、Job、Lease、Fencing、ProductSession
  和 canonical Delivery 状态。
- Worker 只上报注册与心跳事实，以及当前有效 Lease 内的运行事件、产物、模型请求、
  输入请求、执行审批请求、取消确认和执行结果。
- Worker 返回的 `job.outcome` 是执行结果，不是 `DeliveryVerdict`。Control Plane 根据
  当前 Delivery、Evidence 和审核结果推进产品状态。
- 消息只引用公开 ID 和投影，不包含数据库表、存储记录、长期 Provider Credential，
  也不包含 Codex 的 Turn、Plan 或 Agent 内部对象。`codexThreadId` 只是会话绑定引用。

## Chat 与 Delivery 使用不同执行 Scope

`ExecutionScope` 是两个严格分支：

| `kind` | 必需身份 | 用途 |
| --- | --- | --- |
| `product-session` | `productSessionId` | 默认 Chat；不创建隐藏 Delivery 或 StageRun |
| `delivery-stage` | `productSessionId`、`deliveryId`、`stageRunId` | StrongFlow 阶段执行；可附加 `deliveryTaskId` |

两种分支都经过相同的 Job、Attempt、Lease 和 Fencing 校验。ProductSession Chat 不能伪造
Delivery 身份，Delivery 阶段也不能省略当前 StageRun。

## 一份合同，两种 Adapter

本地同进程 Adapter 与远程进程 Adapter 必须接受同一份 `ExecutionPortMessage` union，
并产生相同的确认、拒绝和重放结果。Socket、HTTP、gRPC、地址、Header 和对象存储配置
属于 Adapter 配置，不进入消息合同。本地模式不得通过共享可变状态绕过 Job、Lease、
Attempt 或 Fencing 校验。

## 消息方向

| 方向 | 消息 |
| --- | --- |
| Worker → Control Plane | `worker.register`、`worker.capabilities`、`worker.heartbeat` |
| Control Plane → Worker | `worker.registration_result`、`worker.heartbeat_ack` |
| Control Plane → Worker | `job.dispatch`、`lease.renew`、`runtime.replay_request`、`job.cancel` |
| Worker → Control Plane | `job.dispatch_result`、`session.binding`、`runtime.event`、`job.cancel_ack`、`job.outcome` |
| Worker → Control Plane | `artifact.open`、`artifact.chunk`、`model.open`、`model.ack` |
| Control Plane → Worker | `artifact.ack`、`model.chunk` |
| Worker → Control Plane | `input.request`、`approval.request` |
| Control Plane → Worker | `input.response`、`approval.decision`、`job.outcome_ack` |

除注册、能力和心跳外，每条 Worker 写消息都必须携带完整 `ExecutionLeaseStamp`：

```text
leaseId
jobId
workerId
workerInstanceId
attempt
fencingToken
issuedAt
expiresAt
```

`fencingToken` 使用十进制字符串，避免跨 Rust、JavaScript 和数据库时丢失 64 位整数
精度。重新派发到另一个 Worker 实例或新的尝试时，Control Plane 必须使用更大的 token。

Worker 接受 Job 并建立 CodexThread 后，先发送一条 `session.binding`。它把
`ProductSessionId + WorkerSessionId + CodexThreadId` 绑定到当前
`Job + attempt + Lease + fencingToken`。只有 Control Plane 已经接受并保存这条绑定后，
对应的 `runtime.event` 才能进入持久化和 StrongFlow 投影。相同身份重发是幂等的；同一
Job/Lease 改成另一条 Session 或 CodexThread 是冲突。`runtime.event.codexThreadId` 也必须
与已接受绑定相同，不能从摘要或编码 payload 中猜测。

## 重试与恢复的固定结果

| 情况 | 合同结果 | 状态变化 |
| --- | --- | --- |
| 相同 request、job、attempt、fence 和 payload digest 重复派发 | `duplicate` | 返回原 `workerSessionId`，不启动第二次执行 |
| 相同 eventId、sequence 和 payload digest 重放 | `duplicate` | 确认原事件，不重复持久化或投影 |
| 收到的 sequence 大于“最高连续 sequence + 1” | `gap` | 保持原 `ackSequence`，从 `replayFromSequence` 请求重放 |
| Worker 断线后带有效 Lease 重连 | `replay_required` | 按原 eventId 和 sequence 重放 ack 之后的事件 |
| Worker 写入时 Lease 已过期 | `rejected_expired_lease` | 不保存事件、产物或结果 |
| Worker 重启并产生新的 workerInstanceId | `reacquire_required` | 原实例写入被拒绝，取得新 Lease 后才继续 |
| Worker 使用小于当前值的 fencingToken | `rejected_stale_fencing_token` | 不保存事件、产物或结果 |
| 同一消息身份对应不同 payload | `rejected_conflict` | 不覆盖已接受的数据 |

`ackSequence` 表示 Control Plane 已连续接受的最大 sequence，而不是“见过的最大值”。
状态为 `gap` 时必须返回 `replayFromSequence`；其他状态不得附带该字段。运行事件、产物
块和模型流都使用这一规则。

## Worker 注册与重启

`workerId` 表示可调度 Worker 身份；`workerInstanceId` 表示一次进程启动。每次进程重启
都生成新的 `workerInstanceId`。注册结果中的 `leaseRecovery` 明确返回：

- `no_active_leases`：当前实例没有需要处理的旧 Lease；
- `reacquire_required`：Control Plane 仍记录旧实例 Lease，新的实例必须重新取得 Lease，
  不能直接续写旧实例的数据。

Heartbeat 只更新存活、容量和当前 Lease 进度，不隐式派发 Job。Job 派发仍使用独立的
`job.dispatch`，因此 Heartbeat 重试不会意外启动执行。

## Artifact、Model、Input 与 Approval

- Artifact 使用 `artifact.open` 建立不可变摘要和长度，再用有 sequence 的
  `artifact.chunk` 传输。合同不暴露本地路径、对象存储键或上传 URL。
- Model 使用 `model.open` 提交 Provider 中立的编码请求，由 Control Plane 根据
  `ModelGatewayRoute` 解析模型与长期 Credential。Worker 只收到有 sequence 的编码响应块。
- Input 和 Approval 都带 WorkerSession、Lease 和独立请求身份。产品会话、审批队列、
  决策人和审计记录仍由 Control Plane 管理。
- 浏览器通过 HTTP `input.respond` 提交有 Actor 和 revision 的业务命令。Control Plane 核对
  ProductSession、WorkerSession、ExecutionJob 和 InputRequest 的绑定后，才生成
  `input.response`；HTTP 负载不能直接构造 ExecutionPort 消息。
- Cancel 是 Control Plane 发出的协作式请求。最终状态以租约内 `job.outcome` 为准，
  `job.cancel_ack` 本身不等同于执行已经结束。

## 可执行样本

- `tests/fixtures/contracts/execution-port.valid.json` 为 26 种消息各提供一个合法样本，并为
  默认 Chat 与 Delivery Stage 各提供一个合法 `ExecutionScope`。
- `tests/fixtures/contracts/execution-port.invalid.json` 固定缺失 Lease、越权写 Delivery、
  泄露 Credential、非法 sequence/fence、缺失重放点和传输字段泄露等拒绝样本。
- `tests/execution-port-contract.test.mjs` 使用 Draft 2020-12 validator 验证 schema、样本、
  所有权边界和固定恢复结果。
