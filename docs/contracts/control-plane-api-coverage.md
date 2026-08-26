# Control Plane API 覆盖审计

机器可读结果位于
[`control-plane-api-coverage.matrix.json`](./control-plane-api-coverage.matrix.json)。该矩阵把
ADR-0028 清单中的 Chat、StrongFlow、CLI、行为测试和发布门禁调用方，逐项映射到公开的
HTTP、WebSocket 和 ExecutionPort 合同。`tests/control-plane-api-coverage.test.mjs` 会拒绝
漏掉的调用方、可观察行为或公开 union 分支。

## 当前冻结范围

| 边界 | 分支数 | 用途 |
| --- | ---: | --- |
| HTTP Command | 19 | 所有会改变产品状态的用户和管理操作 |
| HTTP Query | 15 | 列表、详情、Chat 历史和可重建运行投影 |
| WebSocket Event | 10 | 已保存的产品、消息、运行、审批、协作和 Worker 投影 |
| ExecutionPort Message | 26 | Worker 注册、Job/Lease、运行、产物、模型、输入、审批、取消和结果 |

## 审计中纠正的四个缺口

1. 默认 Chat 原先不能构造合法 `ExecutionScope`，因为它被强制要求提供 Delivery 和
   StageRun。现在 Chat 使用 `product-session` 分支，StrongFlow 使用 `delivery-stage`
   分支。
2. `session.get` 和 `delivery.get` 只返回元数据，WebSocket reset 后不能重建 Chat 与运行
   视图。现在使用 `session.messages.list` 和 `runtime.projection.get`。
3. WebSocket 原先没有用户可见的 Chat 消息事件。现在
   `product-session.message.appended.v1` 只推送安全的消息投影。
4. Worker 能请求交互输入，但浏览器没有合法响应入口。现在 `input.respond` 绑定 Actor、
   当前 revision、ProductSession、WorkerSession、ExecutionJob 和 InputRequest，Control
   Plane 校验后才发送 `input.response`。

矩阵同时固定边界：WebSocket 不接业务写入；Worker 不写 DeliveryVerdict 或数据库；Web
和 Worker 都拿不到长期 Provider Credential；公开合同只允许 `codexThreadId` 作为 Codex
会话引用，不暴露 Turn、Plan 或 Agent 内部对象。

公共错误的 `details` 仍可携带递归的机器可读事实，但每层对象都应用同一份敏感字段
拒绝规则。合同样本包含直接和嵌套泄露两类反例，生成的 Rust 与 TypeScript 类型也必须
能够编译这一递归结构。
