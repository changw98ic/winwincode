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
| 已批准方案与两张图 | 当前 plan-review Attention | Spec、规划/审核 StageRun、SessionBinding、Attention 和 review digest 全部一致 |
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

每个运行 Session 的序号必须连续。完全相同的重复事件只确认一次；出现缺口时请求从缺失
序号重放；同一身份或序号后来变成另一份内容时直接报告冲突。

## 刷新、断线和重启

页面首次打开时读取 `delivery.get` 和 `runtime.projection.get`。WebSocket 负责告诉页面
哪些内容已经变化，并可以追加合同允许的安全运行条目。

WebSocket cursor 已过期、权限 epoch 改变或服务要求 reset 时，页面先重新读取完整 HTTP
快照，再从新快照给出的 cursor 继续订阅。页面不拿旧内存状态猜测缺失内容。

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
WebSocket 只通知读取方刷新或追加只读内容，不接受 Delivery、Attention、Verdict 或
Publication 写入。

Web 只使用生成的 HTTP 和 WebSocket 客户端。页面代码可以负责表单、组件、路由、图表和
本地选择状态，但不能手写第二份 DTO，也不能直接改完整领域对象。Web 不连接 Execution Worker，
也不导入 ExecutionPort；所有网络请求都发给 Rust Control Plane。

## 当前 phase-one 合同中已经确认的缺口

本轮不修改 phase-one canonical schema。已经确认的缺口保存在机器规则的
`contractFindings` 中：

1. `delivery.get` 目前返回列表大小的 Delivery 摘要，缺少 StrongFlow 详情；
2. 目前没有已批准方案的公开结构化投影；
3. RuntimeProjectionItem 只有种类和文字摘要，缺少 Plan、Agent、Activity、Usage、
   Evidence 等结构化字段；
4. WebSocket 的 runtime delta 同样只有摘要；
5. Publication 需要单独查询，尚未定义它与 Delivery 详情的一致读取规则；
6. ExecutionPort 的 `runtime.event` 没有保证携带 CodexThreadId，Control Plane 必须先有
   权威 SessionBinding，不能从文字或不透明 payload 猜测；
7. 当前 codegen 只生成 Web 类型，还没有生成 HTTP/WebSocket 运行客户端。

这些缺口要在 phase 2.5 实现时一次性更新 canonical schema、Rust 类型和 TypeScript
生成物，不能在页面里用手写字段或旧 TS 服务补一条长期并行路径。
