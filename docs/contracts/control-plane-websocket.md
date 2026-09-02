# Control Plane WebSocket v1

## 用途

[`control-plane-events.schema.json`](../../schema/winwincode/v1/control-plane-events.schema.json)
是 TypeScript Web 与 Rust Control Plane
之间唯一的实时消息合同。它推送已经由 Control Plane 接受并持久化的事实，以及订阅、
确认、续传和心跳等传输控制消息。

主要业务写入只走 HTTP。WebSocket v1 不接受主要业务 command，也不接受 Delivery、
Approval、Attention、Task、Publication、Credential 或 Worker 管理写入。浏览器通过 HTTP
提交带 `requestId` 和 `expectedRevision` 的操作，再通过 WebSocket 收到结果投影。

## 连接认证

认证只发生在 WebSocket 的 HTTP upgrade。同源 Web 和本地 Host 使用名为
`wwc_session` 的 session cookie；服务账号和企业客户端使用
`Authorization: Bearer <JWT>` 请求头。匿名 upgrade 一律拒绝，服务端解析出的 principal
必须重新授权订阅中的完整 Scope。

认证材料不放入 URL query，也不放入 subscribe、resume、ack、pong 或任何其他 frame。
这样代理日志、浏览器历史和可恢复事件台账都不会保存 session 或 bearer token。

## 一条订阅只对应一个事件流

一条订阅由以下二元组唯一确定：

```text
canonical Scope + EventReadStream
```

`Scope` 和传输中立的 `EventReadStream` 都来自 `domain.schema.json`。Scope 保存完整的
Organization、Workspace、Project、Repository 连续层级，EventReadStream 只增加一个资源选择器：

| `EventReadStream.kind` | 资源选择器 | 用途 |
| --- | --- | --- |
| `scope` | 无 | 组织、工作区、项目或仓库级活动 |
| `delivery` | `deliveryId` | Delivery、DeliveryTask、Attention 与交付活动 |
| `product-session` | `productSessionId` | ProductSession、Chat 消息、Approval、Presence 与运行投影 |
| `lease` | `workerId`、`leaseId` | Worker health 与一次租约绑定的运行状态 |

服务端在接受订阅、续传、重放和实时发送时，都必须从 `scope.organizationId` 开始检查当前
用户权限，并检查资源确实属于该 Scope。不同 Organization、Delivery、ProductSession 或
Lease 的事件不能进入同一条订阅。

## 事件包络

每个 `event.v1` 都包含：

- `eventId`：全局去重标识；
- `scope` 与 `stream`：权限和隔离边界；
- `sequence`：同一 `scope + stream` 内严格递增的序号；
- `occurredAt`：Control Plane 接受该事实的时间；
- `source`：事实来自 Control Plane 还是带明确 Lease 的 Execution Worker；
- `authorizationEpoch`：这次发送使用的权限版本；
- `event`：带固定 `type` 的公开投影。

浏览器只按同一条流的 `sequence` 应用事件，并用 `eventId` 去重。序号不是跨租户或跨流
的全局顺序。Control Plane 必须在持久化、权限检查和资源归属检查通过后才发送事件。

## 订阅、确认与续传

### 新订阅

浏览器发送 `transport.subscribe.v1`，明确 Scope、EventReadStream、需要的事件类型和起点。
Control Plane 检查权限后返回 `transport.subscription-accepted.v1`，其中的 cursor 是该流
本次发送的起点。

`startAt` 只有三种合法值：`latest`、`earliest-available`，或 HTTP 完整快照返回的
`EventReadCursor`。页面已经读取 HTTP 快照时必须使用第三种，不能改成 `latest`；否则 HTTP
响应返回后、WebSocket 订阅建立前发生的事件会永久漏掉。Delivery 快照从
`readCursor.eventCursor` 取这个值，product-session runtime 快照从自己的 `eventCursor` 取值。

cursor 的 `scope + stream` 必须与 subscription 完全相同。服务端接受后返回的
`subscription-accepted.cursor` 必须保留提交 cursor 的 sequence 和 eventId，作为连续读取
基线。第一条业务事件必须是 `accepted.cursor.sequence + 1`，后续每条也只能加一；跳号、
倒退或跨流都按协议错误处理。`subscription-accepted.authorizationEpoch` 同时建立权限基线，
后续事件不能带更早的权限版本。

### 确认

浏览器在成功写入本地投影后发送累计 `transport.ack.v1`。确认 cursor 必须与订阅的
Scope 和 EventReadStream 完全相同。服务端只保留每条订阅最后一个已确认 cursor；比它更旧
的确认不改变状态，跨流确认直接返回协议错误。

### 断线续传

浏览器持久保存最后一个已确认 cursor。重新连接后发送 `transport.resume.v1`，并再次
提交原 Scope、EventReadStream 和事件类型。Control Plane 在每次续传前重新检查权限，返回
`transport.resume-accepted.v1` 后，从 `after.sequence + 1` 开始按序重放。

已被服务端确认的事件不会重放。如果最后一次确认在网络中丢失，客户端可能再次收到
已经应用但尚未被服务端确认的事件；客户端必须用 `eventId` 和 cursor 去重，不能重复
应用。服务端在每一批重放前再次检查权限，并在每次实时发送前再次检查当前
`authorizationEpoch`。

如果 cursor 已被保留策略清理、事件流已重建或权限边界已经改变，Control Plane 返回
`transport.reset-required.v1` 并以 `4409` 关闭连接。它是通用传输帧，不携带固定
`reloadQueries`。客户端先丢弃该订阅对应的旧局部状态，再根据自己保存的原始
`subscription.stream.kind` 执行完整重载；只有重载全部成功后才发布新快照和建立新订阅。
Delivery 页面先让 `delivery.get` 签发 `StrongFlowReadCursor`，再把它作为
`runtime.projection.get` 的 `atCursor`，两个结果必须返回完全相同的 cursor。该 cursor 的签名
也覆盖 `eventCursor`；paired read 成功后，页面立即用这个 eventCursor 建立新订阅。
product-session 页面只读取自己的 runtime snapshot，并用快照自己的 eventCursor 建立订阅。
任一必要请求失败或 cursor 错位时，客户端继续保持旧局部状态已丢弃，不猜测、跳过缺失序号
或拼接两个独立的 latest 快照。

`product-session.message.appended.v1` 只携带已经保存的 `ChatMessageProjection`：消息角色
只允许用户或助手，正文有大小上限，不包含原始 Provider 请求/响应、工具负载、Credential
或 Codex 内部对象。`runtime-projection.invalidated.v1` 不携带运行摘要或详情，并用
`scopeKind` 严格区分两条路径：`delivery-stage` 必须带非空 `deliveryId + stageRunId`，按上述
同一读取截面依次读取 `delivery.get` 和 `runtime.projection.get`；`product-session` 不带这两个
Delivery 字段，只重新读取 `runtime.projection.get`，也不要求 `StrongFlowReadCursor`，但该
完整 runtime 快照仍必须带 product-session 流的 `eventCursor`。浏览器
因此不会从文字消息拼出第二份运行模型，不会把不同 revision 的 Delivery 与 runtime 拼在
一起，也不会为了普通 Chat 暗中创建一个 Delivery。

## 权限变化

成员、角色、项目权限或资源归属变化时，Control Plane 递增权限版本并重新检查现有
订阅。失去权限的订阅收到 `transport.authorization-revoked.v1`，随后以 `4403` 关闭。
发送该消息后，不得再发送旧权限版本的业务事件。重新获得权限必须建立新订阅，旧
cursor 不能自动扩大可见范围。

## 慢客户端与背压

每条订阅最多允许 `256` 个未确认事件。达到此软上限后，Control Plane 发送
`transport.backpressure.v1` 并暂停发送新的实时事件，但继续在有界服务端队列中记录
待发送序号。

出现任一条件时，Control Plane 以 `4408` 关闭连接：

- 待发送和未确认事件达到 `1024`；
- 发出背压通知后 `30000` 毫秒内仍未收到要求的确认。

断开不会推进最后确认 cursor。客户端重连后从最后一个已确认 cursor 续传。这样慢
客户端不会无限占用内存，也不会把未处理事件误报为已经接收。

## 事件来源与资源一致性

Control Plane 来源包含明确的业务 Actor。Execution Worker 来源必须同时包含
`workerId`、`workerSessionId`、`leaseId` 和 `codexThreadId`。带 Worker 来源的 Lease
事件只能进入相同 `workerId + leaseId` 的流；过期 Lease 和过期 fencing token 在进入
本合同之前已被 ExecutionPort 拒绝。

事件 payload 中的 `deliveryId`、`productSessionId` 或 `leaseId` 必须与顶层 EventReadStream
一致。Control Plane 在广播前执行这项检查，浏览器也把不一致视为协议错误并关闭连接。

## 关闭码

| 代码 | 含义 | 客户端动作 |
| ---: | --- | --- |
| `4403` | 当前订阅权限被撤销 | 清除该订阅，不自动重试 |
| `4408` | 客户端未及时确认，服务端执行背压断开 | 从最后确认 cursor 续传 |
| `4409` | cursor 或权限边界不能安全续传 | 用 HTTP Query 重载，再新建订阅 |

其他输入错误通过 `transport.error.v1` 返回 canonical `ErrorEnvelope`。错误响应不会把非法
帧变成业务 command，也不会隐式执行任何写操作。

## 版本和生成

所有公开分支都使用必需的 `type` 单值作为判别字段。通用 ID、Scope、Actor、Instant、
Revision、ErrorEnvelope 和传输中立的 `EventReadCursor` 只从相邻的 `domain.schema.json`
引用。WebSocket schema 只依赖 domain schema，domain schema 不反向依赖任何传输 schema，
因此生成器不会靠循环引用或复制第二份 cursor 来完成 HTTP 到 WebSocket 的衔接。
