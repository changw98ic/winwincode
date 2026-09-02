# Rust Delivery 发布合同

## 结论

当前发布路径由 Rust Control Plane 统一持有 Delivery 业务事实，Rust Worker 统一持有
Job、Lease、fencing、工作区和执行运行时。`apps/client` 通过生成的 HTTP/WebSocket
facade 访问 `winwincode-server`；Server 只做认证、合同校验和组合，不另建产品状态。

机器规则位于 [`delivery-rust-cutover.rules.json`](./delivery-rust-cutover.rules.json)。本门
使用 Git 文件清单、`rg` 和直接读取，声明文件级覆盖；依赖方向和目录闭包由
[ADR-0028](../decisions/0028-control-plane-worker-migration.md)、
[目标图](../decisions/0028-control-plane-worker-target-graph.json) 与
[源码清单](../decisions/0028-control-plane-worker-migration.inventory.json) 共同固定。

## 唯一 Delivery 路径

```text
apps/client
  └─ generated Control Plane API
       └─ winwincode-server
            └─ winwincode-control-plane
                 ├─ winwincode-delivery
                 ├─ winwincode-storage
                 └─ winwincode-publication
                      └─ winwincode-worker → winwincode-execution-port → Codex Core
```

所有正式变更进入 Control Plane 的 typed command：

- `commit_delivery_command`：创建和更新 Delivery；
- `commit_delivery_execution`：记录一次新的执行 attempt；
- `commit_delivery_session_binding`：绑定 ProductSession、StageRun、Job、WorkerSession
  和 CodexThread；
- `commit_delivery_terminal_outcome`：写入当前 attempt 的 terminal outcome；
- `commit_delivery_task_breakdown`：提交获批任务图；
- `commit_delivery_verdict`：记录 Reviewer/Verifier 的独立结论。

Delivery 与 runtime 读取通过 `StrongFlowProjectionQueryPort`。当前状态、追加式 journal、
同请求 receipt 和待发送 outbox 在一个 SQLite 事务中提交。重启恢复同一 receipt 和 cursor；
重复请求不增加 revision，也不重复发送外部副作用。

## Attempt、SessionBinding 与恢复

每个执行 attempt 都有唯一 `ExecutionJob`、`WorkerSession`、`CodexThread`、Lease 和
Fencing 身份。Worker 重启只恢复当前合法 Lease；旧 Lease 的 runtime、outcome 和 cancel
记录保持零写。Failed 进入 retry 时严格创建 attempt+1，并旋转 Delivery 与 ProductSession
的真实 owner binding，receipt 先于任何 outbox 或响应发布。

取消是独立终态。Control Plane 先提交 cancel receipt，Worker 只处理当前 attempt；相同
request ID 重放返回原 receipt。连接在提交后丢失时，重启从 durable receipt/outbox 继续；外部
响应丢失、重复 dispatch 和重复 terminal 都按相同 revision、attempt、fencing 事实去重。

## 精确场景结果

唯一结果文件为
`tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json`，SHA-256 为
`246451128fbc0526b5f9c23377f63a2dca54921f58b6140cad7b0f3cf22a0aa7`。当前十个场景固定如下：

| 场景 | 最终修订号 | 关键结果 |
| --- | ---: | --- |
| `success-closed-loop` | 21 | Delivered，Pass |
| `request-id-replay` | 1 | 同一请求返回原结果，不重复写入 |
| `revision-conflict` | 2 | 旧修订号被拒绝，状态不变 |
| `corruption-recovery` | 1 | 损坏时拒绝读取，恢复后逐值一致 |
| `task-dag` | 2 | 前置任务先执行，循环任务图零写入 |
| `candidate-invalidation` | 31 | 旧候选拒绝，新候选 Pass |
| `attention` | 8 | Attention 保留并正确结算 |
| `inconclusive` | 19 | Inconclusive，进入待处理状态 |
| `infra-error` | 19 | InfraError，进入待处理状态 |
| `rework` | 31 | Fail 后返工，再以新候选 Pass |

## 依赖与发布边界

- `winwincode-control-plane` 只持有产品写入权，不依赖 Server 或 Worker；
- `winwincode-server` 组合生成 API、Control Plane、Worker 和 Local；
- `winwincode-worker` 只依赖 Codex、Domain 和 ExecutionPort；
- `winwincode-local` 只组装 Control Plane、Worker 和 Observability；
- `crates/helper` 是无产品依赖的独立辅助可执行文件；
- Client 不连接 Worker，不持有长期凭据，不手写传输 DTO；
- 新业务调用方必须先进入生成 schema 和 Control Plane typed command，不能新增旁路。

## 检查

```bash
corepack pnpm contracts:check
corepack pnpm verify:source
corepack pnpm format:check
corepack pnpm verify:phase-6.6
node --test tests/delivery-rust-cutover-gate.test.mjs
```

门禁要求目录、manifest、生成产物和 source boundary 都符合当前单一路径；任何第二个
Client 网络实现、Worker 直接产品写入、失配 attempt 绑定或未清理的旧产物都会失败。
