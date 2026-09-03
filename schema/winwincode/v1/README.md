# WinWinCode 公开合同 v1

这一目录冻结 TypeScript Web、Rust Control Plane 与 Rust Execution Worker 共用的第一版
公开合同。

## 唯一入口

| 路由 | 用途 |
| --- | --- |
| `GET /api/v1/auth/session` | 从 HttpOnly cookie 恢复当前 Actor 和授权 Scope |
| `POST /api/v1/auth/session` | 用短时 bootstrap proof 创建独立浏览器 session |
| `DELETE /api/v1/auth/session` | 撤销当前浏览器 session 并清除 cookie |
| `POST /api/v1/commands` | 提交会改变业务状态的命令 |
| `POST /api/v1/queries` | 读取不会改变业务状态的投影 |

WebSocket 只推送投影、运行事件、审批请求和通知。业务变更统一提交到 command
入口，避免 HTTP 与 WebSocket 各自形成一套写入规则。

command/query 两个业务路由都必须显式认证，且只接受两种方式之一：同源 Web/本地 Host 使用
`wwc_session` cookie，服务账号和企业客户端使用 `Authorization: Bearer <JWT>`。认证材料
不进入 query、command body 或 URL query；包络中的 `actor` 必须等于服务端认证出的
principal。浏览器 session 创建只在 `Authorization` header 接受 `bootstrapProof`；创建成功
后和 cookie reload 都返回 secret-free `AuthSessionResponse`，其中 Actor 与有界 Scope 集合
来自当前服务端 session。bootstrap proof、cookie 原值和长期 token 都不会进入 URL、响应、
DOM 或 Client 可读存储。OpenAPI 的 `bootstrapProof`、`sessionCookie` 与 `bearerAuth`
security scheme 是唯一认证合同，不存在匿名分支。

## 文件

- [`domain.schema.json`](./domain.schema.json) 定义跨传输共用的 ID、Actor、Scope、
  revision、Command 包络和错误包络，由阶段 1.2 提供。
- [`control-plane-http.schema.json`](./control-plane-http.schema.json) 定义 HTTP
  command、query、分页和响应。
- [`control-plane-events.schema.json`](./control-plane-events.schema.json) 定义 WebSocket
  订阅、续传、投影事件和传输控制帧；状态语义见
  [`Control Plane WebSocket v1`](../../../docs/contracts/control-plane-websocket.md)。
- [`execution-port.schema.json`](./execution-port.schema.json) 定义 Control Plane 与 Worker
  之间唯一的消息边界；状态语义见
  [`ExecutionPort v1`](../../../docs/contracts/execution-port-v1.md)。
- [`control-plane-http.schema.json`](./control-plane-http.schema.json) 根部的
  `x-winwincode-openapi.paths` 是可合成的 OpenAPI 3.1 路由片段；阶段 1.6
  从它生成唯一的 `openapi.generated.json`，供服务端路由、文档和客户端使用。
- [`examples/control-plane-http.examples.json`](./examples/control-plane-http.examples.json)
  固定成功请求、重复请求、revision 冲突和错误 cursor 样本。

HTTP schema 只通过 `$ref` 使用通用领域定义，不再声明另一份 ID、Actor、Scope
或 revision。

## 生成与漂移检查

`pnpm contracts:generate` 离线读取本目录全部 `*.schema.json`，一次更新以下七个文件：

- `crates/winwincode-domain/src/generated.rs`：Rust 共用 ID 和基础值类型；
- `crates/winwincode-api/src/generated.rs`：引用上述共用类型的 Rust Control Plane HTTP/WebSocket 传输类型；
- `crates/winwincode-execution-port/src/generated.rs`：Rust ExecutionPort Worker 消息 DTO；
- `apps/client/src/generated/contracts.ts`：Web 客户端类型；
- `apps/client/src/generated/control-plane-client.ts`：Web Control Plane 校验客户端与内嵌运行时合同；
- `schema-collection.generated.json`：可独立分发的 JSON Schema 集合；
- `openapi.generated.json`：OpenAPI 3.1 文档。

`pnpm contracts:check` 只做比较，不写文件；缺文件或人工修改生成物都会失败。每个公开
`$defs` 名称在 v1 合同集合中必须唯一，跨文件 `$ref` 只能指向本目录内的 canonical
schema。生成文件带统一来源摘要，重复生成相同输入不会改写文件。Rust 中每个独立的基础值只在
`winwincode-domain` 定义一次，API 数据结构从 `winwincode-domain` crate 根直接引用，不保留第二份 ID、旧别名或生成文件布局。

## 已冻结的写入范围

| 产品范围 | Command |
| --- | --- |
| Chat | `session.create`、`chat.submit`、`input.respond`、`session.cancel`、`session.close` |
| Delivery / StrongFlow | `delivery.create`、`delivery.update_spec`、`delivery.approve_task_breakdown`、`delivery.advance`、`delivery.resolve_attention`、`delivery.submit_verdict` |
| 设置 | `settings.update` |
| 凭据引用 | `credential.reference.create`、`credential.reference.rotate`、`credential.reference.revoke`、`credential.reference.delete` |
| 审批 | `approval.decide` |
| Worker 管理 | `worker.drain`、`worker.enable` |
| 发布 | `publication.publish`、`publication.cancel` |

每个 command 都必须带：

- `requestId`：同一 Actor 和 Scope 内的重试身份；
- `expectedRevision`：执行写入前必须匹配的资源版本；
- `actor`：必须与认证后的调用者一致；
- `scope`：调用者必须有权访问的组织、工作区、项目或仓库范围。

相同 `requestId` 和完全相同的输入再次提交时，服务返回第一次保存的 HTTP
状态和响应正文，不重复执行。相同 `requestId` 对应不同输入时返回
`IDEMPOTENCY_CONFLICT`。资源版本已变化时返回 `REVISION_CONFLICT`，错误详情同时
给出 `expectedRevision` 和 `currentRevision`。

成功响应按原始 `command` 或 `query` 组成判别联合。每个操作只允许自己的结果投影；例如
`delivery.get` 只能返回 `DeliveryDetailProjection`，`runtime.projection.get` 只能返回
`RuntimeProjectionSnapshot`。调用方或服务端不能把同一结果改写 discriminator 后当作另一项
操作的响应。`DeliveryDetailProjection.diagramExecution` 是同一读取截面的有界图执行事实：
它只携带当前 Candidate 摘要、节点、路径状态和 Task/Evidence 引用，不携带 diff 字节或 hunk
内容。

## 查询与 cursor

查询覆盖 ProductSession、Chat 消息、可重建运行投影、Delivery、设置、凭据引用、审批、
Worker 和 Publication 的列表与详情。`session.messages.list` 返回只含用户/助手正文的
安全消息投影；`runtime.projection.get` 按 ProductSession 或 Delivery + StageRun 返回重启
和 WebSocket reset 所需的完整有界投影。列表按稳定快照、`updatedAt`、ID 排序。cursor 是服务端生成的
不透明字符串，并绑定当前 Scope、查询名、筛选条件和快照；换 Scope、换筛选条件、
被篡改或已失效的 cursor 统一返回 `INVALID_REQUEST`，错误字段为 `page.cursor`。

完整 Delivery 和 runtime 投影还返回传输中立的 `EventReadCursor`。Delivery 的这个位置包含在
`StrongFlowReadCursor` 的签名字段中；product-session runtime 直接返回自己的位置。Web 完成
HTTP 读取后把它原样放入 `transport.subscribe.v1.startAt`。订阅接受帧建立 sequence 与权限版本
基线，后续第一条事件必须是 `baseline + 1`；保留窗口已经丢失该位置时返回 4409 并重新完整读取。
`domain.schema.json` 只定义中立 cursor，WebSocket schema 单向引用它，不把传输对象反向塞进领域层。

`input.respond` 不是 ExecutionPort 透传。Control Plane 必须先核对 Actor、Scope、当前
revision、ProductSession、WorkerSession、ExecutionJob 和 InputRequest 的完整绑定，之后
才生成一条 `input.response`。浏览器不能提交 Lease、messageId 或任意 Worker 消息。

## 稳定错误

| HTTP 状态 | 错误码 |
| ---: | --- |
| 400 | `INVALID_REQUEST` |
| 401 | `AUTHENTICATION_REQUIRED` |
| 403 | `PERMISSION_DENIED` |
| 404 | `RESOURCE_NOT_FOUND` |
| 409 | `IDEMPOTENCY_CONFLICT`、`REVISION_CONFLICT`、`WRONG_STATE` |
| 429 | `RATE_LIMITED` |
| 500 | `INTERNAL_ERROR` |
| 503 | `SERVICE_UNAVAILABLE` |

只有 `RATE_LIMITED` 和 `SERVICE_UNAVAILABLE` 标记为可重试。凭据查询只返回引用和
`secretState`，不会返回密钥内容或 Vault 内部定位信息。

`ErrorEnvelope.details` 可以保存递归 JSON 事实，但每一层对象都会拒绝凭据、原始模型
请求、工具载荷、数据库记录和 Codex 内部对象等越权字段名；嵌套对象和数组不能绕过
这项检查。
