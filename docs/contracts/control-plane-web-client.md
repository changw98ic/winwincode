# Control Plane Web Client 实现合同

机器规则位于
[`control-plane-web-client.rules.json`](./control-plane-web-client.rules.json)。当前状态是
implemented/enforced：生成文件和可执行行为证明已经存在，门禁会拒绝手改产物、类型漂移和
浏览器绕过。

## 当前接入面

| 范围 | 当前事实 | 阶段 2.5.4 的处理 |
| --- | --- | --- |
| Schema 生成器 | `scripts/generate-contracts.mjs` 从 canonical schema 同时生成 Rust、TypeScript、OpenAPI、schema collection 和 Web client | Web client 已进入同一个生成器，没有第二个手写生成脚本 |
| Web 生成目录 | `contracts.ts` 保存 DTO，`control-plane-client.ts` 保存唯一浏览器网络实现 | 两个文件都由同一份 canonical schema 生成并检查漂移 |
| StrongFlow | `packages/strongflow/src/client.ts` 通过 DSH Typert 的 `strongflow.invoke` / `strongflow.advance` 调用，并每两秒读取一次完整投影 | 新 Web 页面只调用生成的 HTTP/WebSocket client；旧调用不会成为第二条长期路径 |
| StrongFlow 浏览器状态 | 只把选中的 Delivery ID 放进 `localStorage`；Delivery 和运行投影仍由远端重新读取 | cursor 只能保存为恢复位置，不能成为 Delivery 事实 |
| StrongFlow 重启恢复 | `delivery-recovery.ts` 从 DeliveryStore 和 RuntimeSessionLedger 重放，再决定下一项动作 | 这是服务端恢复事实，不是浏览器 cursor 的替代品 |
| Chat | 当前页面来自 stock DSH Web App；`agent-factory.ts` 用 RuntimeSessionLedger 恢复 Codex Session，仓库没有 project-owned Chat 页面源码 | 本阶段生成通用 Query/WebSocket 能力，但不宣称 Chat 页面已经迁移 |

这份盘点使用 Git 文件清单、`rg` 和 `ast-grep`。仓库本地索引脚本在该 worktree 中缺失，
所以这里只声明文件级覆盖，不声明完整调用图覆盖。

## 唯一生成输出

`apps/web/src/generated/control-plane-client.ts` 是实现触发文件，也是浏览器网络访问的唯一
所有者。它和 `contracts.ts` 都由同一个 canonical generator 产生，并带生成标记。

生成文件必须真实导出以下接口，而不是只在测试文本中留下名字：

- `createControlPlaneHttpClient()` / `ControlPlaneHttpClient`；
- `createControlPlaneWebSocketClient()` / `ControlPlaneWebSocketClient`；
- `createStrongFlowProjectionSubscription()` / `StrongFlowProjectionSubscription`；
- 只保留安全错误字段的 `ControlPlaneClientError`。

生成 client 只从相邻的 `./contracts.js` 读取 canonical DTO。页面不能从旧
`@winwincode/contracts`、StrongFlow Host、ExecutionPort、Native 或 Worker 包拼装另一套
传输对象。

## HTTP 规则

写操作只向 `/api/v1/commands` 发送 generated `CommandRequest`；读操作只向
`/api/v1/queries` 发送 generated `QueryRequest`。

每个 command 保留调用方给出的 `requestId` 和 `expectedRevision`。网络重试必须重发完全
相同的 command envelope，不能生成新的 requestId，也不能把 revision 改成服务端刚返回的
值后暗中重试。分页 cursor 是不透明值；页面不能拆解、拼接或跨 Scope、Query、筛选条件、
快照复用。

非成功响应只按 canonical `ErrorEnvelope` 处理。`ControlPlaneClientError` 只暴露
`code`、`message`、`requestId`、`retryable` 和已经清理过的 `details`。也就是只暴露 canonical `ErrorEnvelope` 中已经清理过的错误字段，
不复制响应 body、header、URL、stack、
Authorization、Credential、Token、Provider 请求或响应。格式错误的服务端响应只产生有界
通用错误。

## WebSocket 规则

WebSocket client 只发送四种 canonical frame：subscribe、resume、累计 ack 和 pong。
WebSocket 不提交业务 command；Delivery、Attention、Verdict、Publication 和其他写入仍走
HTTP。

事件处理成功后才推进最后已确认 cursor 并发送累计 ack。普通断线和 `4408` 背压断线从
最后已确认 cursor 续传；不能从最后收到但尚未应用的事件续传。`4403` 表示权限已经撤销，
客户端清除订阅并停止自动重试。事件用 `eventId` 加同一 Scope/Stream cursor 去重。

通用 `transport.reset-required.v1` 不携带固定 query 清单。生成 client 保存最初的
`subscription.stream.kind`：Delivery stream 走完整 StrongFlow 双查询，ProductSession stream
只重读它自己的运行投影。服务端运行投影失效事件则保留严格的两分支 `reloadQueries`，客户端
会核对分支和本地订阅一致，不能凭空补一个 Delivery。

## StrongFlow reset

首次打开或收到 `4409` / `transport.reset-required.v1` 时，生成 client 使用同一 Scope 和
Delivery 身份发出两次 HTTP query。只有 `delivery.get` 和 `runtime.projection.get` 都成功，
并返回一个可用于同一事件流的 cursor 后，才能一次性替换页面快照并开始订阅。

任一 query 失败时丢弃这次成对读取的部分结果，不显示一半新、一半旧的页面，也不提前
订阅。重试仍然重新读取这一对 query。客户端从返回 cursor 续传或建立新订阅，不使用 reset
前的旧内存 cursor 猜测缺失事件。

如果第二次查询返回 `READ_CURSOR_EXPIRED`，client 丢弃第一次查询结果，从不带 `atCursor`
的 `delivery.get` 重新建立整组读取；不会只重试 runtime。格式错误或属于其他范围的 cursor
仍按原错误结束，不进入这个恢复分支。

ProductSession 的生成订阅能力在首次打开、运行失效和 `4409` 时都只调用
`runtime.projection.get`，请求中没有 Delivery、StageRun 或 StrongFlow cursor。Chat 消息历史
仍按 Chat 合同读取；stock DSH Chat 页面本身没有在阶段 2.5.4 中迁移。

## 页面边界与自动门禁

Web 不连接 Execution Worker，不持有 Worker 地址，不导入 ExecutionPort，也不手写 HTTP
或 WebSocket DTO。`apps/web/src/generated` 之外出现直接 `fetch()` 或 `new WebSocket()`
会直接失败。

Node 门禁解析 TypeScript 源码，检查真实 export declaration、接口成员、生成标记、generator
输出和 import 依赖，并执行 `tests/generated-control-plane-client.test.mjs` 中的 fake HTTP/WS
行为证明。门禁不通过搜索测试名称来冒充实现完成。
