# Control Plane HTTP v1

这一目录冻结浏览器和外部客户端访问 Rust Control Plane 的第一版 HTTP 合同。

## 唯一入口

| 路由 | 用途 |
| --- | --- |
| `POST /api/v1/commands` | 提交会改变业务状态的命令 |
| `POST /api/v1/queries` | 读取不会改变业务状态的投影 |

WebSocket 只推送投影、运行事件、审批请求和通知。业务变更统一提交到 command
入口，避免 HTTP 与 WebSocket 各自形成一套写入规则。

## 文件

- [`domain.schema.json`](./domain.schema.json) 定义跨传输共用的 ID、Actor、Scope、
  revision、Command 包络和错误包络，由阶段 1.2 提供。
- [`control-plane-http.schema.json`](./control-plane-http.schema.json) 定义 HTTP
  command、query、分页和响应。
- [`control-plane-http.schema.json`](./control-plane-http.schema.json) 根部的
  `x-winwincode-openapi.paths` 是可合成的 OpenAPI 3.1 路由片段；阶段 1.6
  从它生成唯一的 `openapi.generated.json`，供服务端路由、文档和客户端使用。
- [`examples/control-plane-http.examples.json`](./examples/control-plane-http.examples.json)
  固定成功请求、重复请求、revision 冲突和错误 cursor 样本。

HTTP schema 只通过 `$ref` 使用通用领域定义，不再声明另一份 ID、Actor、Scope
或 revision。

## 已冻结的写入范围

| 产品范围 | Command |
| --- | --- |
| Chat | `session.create`、`chat.submit`、`session.cancel`、`session.close` |
| Delivery / StrongFlow | `delivery.create`、`delivery.update_spec`、`delivery.approve_task_breakdown`、`delivery.advance`、`delivery.resolve_attention`、`delivery.submit_verdict` |
| 设置 | `settings.update` |
| 凭据引用 | `credential.reference.create`、`credential.reference.delete` |
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

## 查询与 cursor

查询覆盖 ProductSession、Delivery、设置、凭据引用、审批、Worker 和 Publication
的列表与详情。列表按稳定快照、`updatedAt`、ID 排序。cursor 是服务端生成的
不透明字符串，并绑定当前 Scope、查询名、筛选条件和快照；换 Scope、换筛选条件、
被篡改或已失效的 cursor 统一返回 `INVALID_REQUEST`，错误字段为 `page.cursor`。

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
