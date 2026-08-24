# StrongFlow 只读投影合同

机器可读规则位于
[`strongflow-projection.rules.json`](./strongflow-projection.rules.json)。本合同固定的是
StrongFlow 页面可以读取什么、每项内容来自哪里，以及刷新、重启和实时推送必须怎样保持
一致。它不增加另一套 Delivery、Agent、Session 或任务状态。

## 页面只读一份组合结果

StrongFlow 的完整页面由四类已经存在的事实组合而成：

1. Rust Delivery 模块保存的当前 Delivery、DeliverySpec、DeliveryTask、StageRun、
   SessionBinding、Attention、Evidence 和 Verdict；
2. 当前方案审核 Attention 中已经校验过的方案、两张图、审核摘要和人工决定；
3. Control Plane 已经接收并保存的 Worker/Codex 运行事件；
4. Rust Publication 模块保存的当前发布意图和结果。

页面不保存一份可以独立修改的 Delivery 副本。投影代码也不推进 Delivery、不启动 Codex、
不创建 Agent、不执行命令、不发布 GitHub 内容。它只把已有事实整理成适合页面读取的字段。

## 页面必须覆盖的内容

| 页面内容 | 唯一来源 | 必须绑定的范围 |
| --- | --- | --- |
| 需求与验收条件 | 当前 canonical DeliverySpec | 当前 Delivery 和 Spec revision |
| 当前方案审核与两张图 | 当前 plan-review Attention | pending 或 settled review 都绑定 Spec、规划执行 SessionBinding、人工审核 StageRun、Attention 和 review digest；settled 还绑定认证审核人和审核时间；人工审核不伪造执行 SessionBinding |
| 阶段 | canonical StageRun 与 SessionBinding | 当前 Delivery；运行内容必须再匹配实际 SessionBinding |
| DeliveryTask | canonical DeliveryTask | 当前 Delivery；只按 DeliveryTaskId 汇总阶段和证据 |
| Plan 与 Agent Graph | 已接受的 Worker/Codex 事件 | 精确 SessionBinding 和当前 Job/Lease/attempt/fence |
| Command / Test | 已接受的 Worker/Codex 事件 | 精确 SessionBinding；不包含完整输出 |
| Usage | 已接受的 Worker/Codex 事件 | 精确 SessionBinding；只接受非负数字指标 |
| Attention | canonical AttentionItem | 当前 Delivery 和当前 Spec |
| Evidence | canonical EvidenceRef | 当前 Spec revision 和当前冻结候选 |
| Verdict | canonical DeliveryVerdict | 当前 Spec、候选和全部当前验收条件 |
| Publication | canonical Publication | 当前 Delivery、目标、候选、Verdict 和人工批准 |

Codex Plan 只显示为运行内容，不自动变成 DeliveryTask。Agent Graph 也只是 Codex 实际
Agent 关系的只读显示，不是 Control Plane 创建的第二张 Agent 图。

## 运行事件怎样进入页面

一条事件至少要经过下面的检查：

```text
DeliveryId + StageRunId + ProductSessionId
          + WorkerSessionId + CodexThreadId
          + ExecutionJobId + LeaseId + attempt + fencingToken
                              ↓
                   精确命中一个 SessionBinding
                              ↓
                   序号是下一条连续事件
                              ↓
                    写入可重放的运行事实
                              ↓
                       生成只读投影
```

未绑定的事件不进入投影。匹配多个 SessionBinding 的事件也不进入投影。过期 Lease、旧
attempt、旧 Worker 实例或旧 fencing token 在保存运行事实之前就结束处理，因此不会先
显示到页面再撤回。

每个 Codex StageRun 在阶段列表中都必须保留 ProductSession 与 ExecutionJob 绑定，即使
WorkerSession 或 CodexThread 还没接入；这两个字段用显式 `null` 表示尚未绑定。CodexThread 一旦存在，
WorkerSession 也必须存在。人工审核阶段的 `sessionBinding` 必须是 `null`。完整 runtime
Session 投影则只接受已经同时绑定 WorkerSession 和 CodexThread 的运行事实。

每个运行 Session 的序号必须连续。完全相同的重复事件只确认一次；出现缺口时请求从缺失
序号重放；同一身份或序号后来变成另一份内容时直接报告冲突。

## 刷新、断线和重启

页面首次打开时先读取 `delivery.get`。Control Plane 在结果中签发一个
`StrongFlowReadCursor`，它精确绑定 repository scope、Delivery revision、runtime ledger
revision、已接受运行事实序号和 publication revision。页面把同一个 cursor 作为 `atCursor`
提交给 `runtime.projection.get`；第二个结果必须返回完全相同的 cursor。cursor 不使用生成时间，
也不能由浏览器自行拼装。每次查询的 envelope scope 和 deliveryId 都必须与 cursor 完全一致；
认证和授权检查仍按当前 actor 执行，cursor 不是跨 scope 的访问凭据。

如果服务端已经清理该 cursor 所需的运行记录，查询返回 HTTP 409 和
`READ_CURSOR_EXPIRED`。页面必须丢弃整份未完成的 StrongFlow 局部快照，从不带 `atCursor`
的 `delivery.get` 重新开始，再用新 cursor 读取 runtime。相同旧 cursor 不能原样重试。
cursor 格式损坏返回 `INVALID_REQUEST`，跨 scope 使用返回 `PERMISSION_DENIED`，可信事实读取器
暂不可用返回 `TRUSTED_FACTS_UNAVAILABLE`；这些错误不会泄露其他 scope 是否存在。

WebSocket 只发送 `runtime-projection.invalidated.v1` 等失效通知；它不附带另一份运行详情。
Delivery-stage 失效分支带 `scopeKind=delivery-stage` 和非空 Delivery/StageRun 身份。收到该分支
后，页面重新执行上述两步：先用
`delivery.get` 建立新读取截面，再让 `runtime.projection.get` 使用该截面。任何一步失败，
或者两个结果的 cursor 不一致，页面丢弃整对结果并重新读取，不能显示一半新、一半旧的内容。
两项都成功后才从新快照继续订阅。页面不拿旧内存状态猜测缺失内容。

普通 Chat 的 product-session 失效分支带 `scopeKind=product-session`，不带 DeliveryId 或
StageRunId，只重新读取 `runtime.projection.get`，其快照 `readCursor=null`。它不会为了复用
StrongFlow 刷新路径而制造隐藏 Delivery。

通用 `transport.reset-required.v1` 不携带固定 reload query。客户端收到 reset、WebSocket cursor
过期或权限 epoch 改变时，先丢弃原订阅的旧局部状态，再按保存的
`subscription.stream.kind` 选择完整重载：Delivery stream 走上述 paired read，product-session
stream 只读 runtime。完整重载成功前不发布一份不完整的新快照。

同一组 canonical 记录和同一组按序运行事件，无论是实时逐条处理，还是进程重启后完整
重放，字段顺序和内容都相同。也就是刷新和重启后得到同一份结果。

## Diff 和敏感内容

执行中只显示 Diff 数量摘要：变化文件数量、增加行数、删除行数、`detailsVisible=false`
和安全的来源引用。执行中只显示 Diff 数量摘要，不返回文件路径、变化文件列表、hunk、
hunk 内容或 unified Diff。

路径和 hunk 只能通过单独的、需要权限的冻结候选详情读取，并且同时满足：

- 生产 StageRun 已结束；
- 候选已经冻结；
- 候选仍属于当前 Spec revision；
- Diff digest 已重新核对；
- 生产 SessionBinding 精确匹配；
- 读取者有交付审核权限。

任何公开投影都不包含原始 Session 日志、stdout、stderr、完整 tool payload、Provider
请求/响应、Authorization 内容或 Credential。Evidence 只保留来源引用，不复制上述内容。

## HTTP、WebSocket 和 Web 的边界

主要业务写入仍然只走 HTTP command，并携带 `requestId` 与 `expectedRevision`。
WebSocket 只通知读取方刷新，不接受 Delivery、Attention、Verdict 或
Publication 写入。

Web 只使用生成的 HTTP 和 WebSocket 客户端。页面代码可以负责表单、组件、路由、图表和
本地选择状态，但不能手写第二份 DTO，也不能直接改完整领域对象。Web 不连接 Execution Worker，
也不导入 ExecutionPort；所有网络请求都发给 Rust Control Plane。

## Canonical 传输已经收口

- `DeliveryPage` 继续返回紧凑的 `DeliveryProjection`；`delivery.get` 返回有
  `kind=delivery_detail` 判别字段的 `DeliveryDetailProjection`。
- `SolutionReviewProjection` 来自当前已验证 review set。PlanReview 中的 pending 内容也可安全
  显示；pending 的决定、意见、修改要求、审核人和时间均为 `null`。settled 状态公开审核人、
  时间、决定和有界意见，其中只有 `changes_requested` 必须提供非空 `requestedChanges`。
  这些字段来自已校验的 typed decision，不公开原始 Attention context 或 resolution。
- `taskProposals` 是非空有序列表，并纳入 `reviewSetSha256`。只有 `reviewStatus=approved` 时，
  `delivery.approve_task_breakdown` 才能按原顺序逐字段提升这些任务；HTTP 调用方只提交
  `deliveryId + reviewSetSha256`，不能另交一份 tasks。Planner 不能在 proposal 中指定 owner；
  提升后的 owner 为 `null`、初始状态为 `pending`，后续只能通过已认证的 assignment command 修改。
- `runtime.projection.get` 返回按 SessionBinding 组织的 Plan、Agent Graph、Activity、Usage、
  Recovery 和 Diff 数量摘要。
- `runtime-projection.invalidated.v1` 只通知页面重新读取 HTTP 快照。delivery-stage 分支执行
  paired read，product-session 分支只重载 runtime；旧的文字摘要追加事件已删除。
- `DeliveryDetailProjection` 和 delivery-stage `RuntimeProjectionSnapshot` 都携带同一个
  `StrongFlowReadCursor`。`delivery.get` 建立截面，`runtime.projection.get` 只重放该截面，
  Web 不拼接两个独立的 latest 结果。
- Publication summary 与当前 Delivery、Spec、候选、通过 Verdict、人工批准和目标在同一个
  `delivery.get` 读取截面中核对；已有 publication 事实只要任一字段过期，就拒绝整份截面。
  `resourceRef` 只公开 closed `kind + owner/repository + number`，repository 必须等于 target；
  不返回任意 URL、query、fragment、userinfo 或原始 Provider payload。
- ExecutionPort 在 `runtime.event` 前接受一条完整的 `session.binding`，并要求事件携带相同
  `codexThreadId`。未绑定或不一致的事件在保存和投影之前被拒绝。
- canonical schema 中的 HTTP 路由、查询结果、WebSocket 失效事件和 reset reload metadata
  会一起生成到 Rust、TypeScript、Schema Collection 和 OpenAPI。Web 客户端实现直接读取这份
  metadata，不再维护手写路由表。
