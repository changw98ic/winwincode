# Rust Control Plane StrongFlow 组合投影合同

机器规则位于
[`control-plane-strongflow-projection.rules.json`](./control-plane-strongflow-projection.rules.json)。
这份合同冻结阶段 2.5.6 的 Control Plane 组合边界，不修改 Delivery 领域模型、公开 schema、
生成代码或 HTTP/WebSocket 实现。

## 一句话边界

Delivery 模块先生成内部投影，Control Plane 再把它与可信运行台账、可信发布事实组合，并且
只输出 `winwincode-api` 生成的类型。

```text
winwincode-delivery internal projection
             +
accepted runtime-ledger projection
             +
current publication authorization/result
             ↓
Rust Control Plane StrongFlowProjectionQueryPort
             ↓
winwincode-api generated QueryResultResponse
             ↓
future HTTP / WebSocket adapter
```

Delivery crate 不依赖 winwincode-api，也不读取 HTTP 类型。Control Plane 可以依赖
Delivery、API 和 Storage，并负责最后一次权限、范围、读取截面和输出检查。Web 与 Worker 都
不能越过 Control Plane 直接读取 Delivery storage。

## 当前真实情况

已经成立的部分：

- Delivery snapshot、追加式 journal、command receipt 和 outbox intent 可以在同一个 Rust
  storage transaction 中提交；
- `winwincode-control-plane` 已依赖 `winwincode-delivery`、`winwincode-api` 和
  `winwincode-storage`；
- `winwincode-delivery` 没有反向依赖 API 或 Control Plane；
- Candidate、terminal outcome 和 Evidence 的测试构造入口没有在生产依赖中开放。

尚未成立的部分：

- Rust Control Plane 还没有可信的追加式 runtime ledger read port；
- `delivery.get` 和 `runtime.projection.get` 还没有共享的组合读取 cursor；
- 生成的 `DeliveryProjection` 仍是列表摘要；
- 生成的 `PublicationProjection` 没有 candidate、Verdict、人工批准和 target 身份；
- Rust Publication owner、HTTP server 和 WebSocket server 尚未实现；
- durable outbox 还没有公开 WebSocket 所需的 scope、stream 和 stream-local sequence。

因此 `crates/winwincode-control-plane/src/strongflow_projection.rs` 当前应当不存在。文件出现
后，Node 门禁会立即运行真实 Rust black-box integration test，而不是只搜索一个测试名称。

## 组合只允许一个方向

Control Plane 只做三件事：

1. 从 Delivery journal 的当前 tail 恢复并验证当前 Delivery；
2. 调用 Delivery 自己的只读投影入口；
3. 把 Delivery 投影与可信 runtime/publication read port 的结果映射为生成的 API DTO。

它不能重新解释 AcceptanceCriterion、StageRun、Evidence、Verdict 或 Attention，也不能把
完整 `Delivery` / `DeliverySnapshot` 作为 HTTP 结果。`ControlPlane::load_state(stream_id)`、
`ProductStateStorage`、`StateChange` 和通用 `commit` 是低层生命周期/迁移接口，不是未来
HTTP 或 WebSocket adapter 的读取入口。

目标公开入口固定为：

```rust
pub trait StrongFlowProjectionQueryPort {
    fn delivery_get(/* generated DeliveryGetQuery */);
    fn runtime_projection_get(/* generated RuntimeProjectionGetQuery */);
}
```

真实签名必须接收生成的 `DeliveryGetQuery` / `RuntimeProjectionGetQuery`，返回生成的
`QueryResultResponse`，并以 `StrongFlowProjectionError` 报告关闭、过期、越界或缺少可信
事实。不得增加 `from_raw`、`from_snapshot`、`from_worker` 或 `new_unchecked` 公共入口。

## 两个 HTTP 查询必须读取同一个截面

StrongFlow 初次加载会调用 `delivery.get` 和 `runtime.projection.get`。这两个调用必须读取
同一个有上限的读取截面，而不能把两次独立的最新读取拼在一起。

`delivery.get` 返回的生成 DTO 必须包含一个 Control Plane 签发的不透明 projection
cursor。cursor 至少绑定：

```text
DeliveryId
+ Delivery revision
+ runtime projection through-cursor
+ publication revision（包括“当前没有 publication”的明确版本）
+ authorized repository scope
+ server page limit
```

runtime cursor 可以内部保存每个 SessionBinding 的连续 tail，也可以指向 Control Plane
拥有的全局 accepted-fact sequence；浏览器不能构造或展开它。`runtime.projection.get` 必须
使用 `delivery.get` 给出的同一 cursor。以下任一变化都拒绝整份结果并要求重新读取：

- Delivery、Spec 或 Delivery revision 改变；
- SessionBinding、StageRun、ExecutionJob、Lease、attempt 或 fence 不再当前；
- runtime ledger 缺口、重复 identity 不同内容、越过 cursor 或超过上限；
- publication revision、candidate、Verdict、批准或 target 改变；
- cursor 属于其他 actor、Organization、Workspace、Project、Repository 或 Delivery；
- 读取期间发生并发更新，导致任何来源不再属于同一个截面。

同一授权范围、journal tail、runtime cursor、publication revision 和 limit 的重放必须得到
相同字段、顺序和分页结果。Control Plane 不读取完整无上限 ledger，也不让 Web 根据摘要文字
自行合并缺失事件。

## Publication 不是按 DeliveryId 随手关联

发布摘要必须同时匹配当前 Delivery、候选、通过结论、人工批准和目标。完整集合是：

```text
DeliveryId
+ current DeliverySpec ID/revision
+ current frozen candidate identity/digest
+ current passing DeliveryVerdict ID
+ resolved human publication approval and review-set digest
+ immutable publication target
+ publication intent/result revision
```

生成的 `PublicationProjection` 目前只有 publication ID、Delivery ID、revision、state 和
updatedAt，单凭这些字段不能证明上述关系。`publication.publish` 请求中的
`candidateDigest` 和 `target` 也只能作为 stale-check assertion，不能成为权威 candidate 或
target。

Publication owner 必须从当前持久事实派生一个不可伪造的授权截面，并在 provider 调用前把
intent 与该截面原子保存。Control Plane 只能读取这个可信结果。旧 candidate、旧 Verdict、
旧批准、旧 target、外部 Delivery 或校验后发生的并发变更都会拒绝整份组合结果；不能把过期
发布显示成成功，也不能把缺少可信 publication adapter 解释为“没有发布”。

## 缺少 adapter 时生产入口保持关闭

缺少可信运行台账或发布 adapter 时，生产查询返回可信事实不可用，即生成错误码
`TRUSTED_FACTS_UNAVAILABLE`，不使用以下替代来源：

- HTTP payload 或浏览器内存中的 Delivery / candidate / Verdict；
- 最新收到但还未写入 accepted ledger 的 Worker 消息；
- `RuntimeEventMessage`、`JobOutcomeMessage` 或任意 JSON 对象；
- crate-private `test_support` fixture；
- TypeScript 旧 StrongFlow 服务的内存投影；
- 只有 publication state 字符串的弱关联记录。

Git/Artifact、runtime ledger、terminal/Lease 和 Publication adapter 完成各自集成门禁后，
Control Plane 才能组装生产结果。Domain 单元测试通过不代表这些 adapter 已经提供可信事实。

## HTTP 与 WebSocket 边界

HTTP 和 WebSocket 输入不能构造领域投影、运行事实或发布事实。

HTTP adapter 只解析生成的 `QueryRequest`，完成认证和完整 repository scope 检查，然后调用
`StrongFlowProjectionQueryPort`。它不能调用 `load_state`、`ProductStateStorage::load_*` 或
通用 `commit`，也不能把 `serde_json::Value` 交给投影模块。

WebSocket 只发布已经提交的只读通知或安全 delta。公开 event cursor 的 scope、stream、
stream-local sequence 和 projection cursor 必须与 accepted fact 在同一事务中保存。不能直接
广播 Worker 消息，也不能把 storage outbox 的全局 sequence 当成可恢复的 Delivery stream
cursor。出现 gap、过期 cursor、权限 epoch 变化或 reset 时，浏览器重新执行有上限的 HTTP
读取。

投影公开字段不包含：

- API key、Authorization、Credential；
- provider request/response；
- 原始 runtime log、stdout、stderr、完整 tool payload；
- 执行中 changed file path、hunk、hunk content 或 unified Diff。

## stale、foreign 和并发错位统一 fail-closed

投影组合是全有或全无。任何来源 foreign、stale、ambiguous、缺失或越过 bounded cursor 时，
Control Plane 返回稳定的生成错误，不输出其余“看起来还能用”的部分。至少区分：

- `REVISION_CONFLICT`：Delivery 在读取截面建立后改变；
- `TRUSTED_FACTS_UNAVAILABLE`：可信 runtime/publication owner 尚未接入或事实缺失；
- `PERMISSION_DENIED`：actor 与完整 scope 不匹配；
- `INVALID_REQUEST`：cursor、limit 或生成 query 结构无效；
- `SERVICE_UNAVAILABLE`：可信 adapter 暂时不可读，并且没有满足 cursor 的 durable replay。

错误详情同样遵守公开字段红线，不复制内部 identity map、日志或 provider body。

## P0 风险

### 1. Delivery/runtime torn read

当前两个 query 没有共享 cursor。若分别读取 latest，页面可能组合 revision A 的 Delivery、
revision B 的 runtime 和 revision C 的 publication。

### 2. 通用 storage 入口被 transport 误用

Control Plane 当前公开 `load_state`、`StateChange` 和 storage 类型。未来 transport 若直接使用
它们，会绕过 Delivery journal、typed scope、投影校验与脱敏。

### 3. runtime ledger authority 缺失

生成 Runtime DTO 已存在，但 Rust 还没有 accepted ledger。直接投影 Worker 消息会允许
未持久、乱序、旧 Lease 或跨 Session 的内容进入页面。

### 4. Publication 关联字段不足

当前 Publication DTO 无法证明 candidate、Verdict、approval 和 target 都是当前值。

### 5. WebSocket cursor 尚未 durable

当前 outbox 的全局 sequence 不能证明 `(scope, stream)` 内连续，也不能证明与 HTTP snapshot
属于同一截面。

## 实现顺序

1. `winwincode-delivery` 完成内部 requirements/solution/stage/task/Attention/Evidence/Verdict
   投影，保持不依赖 API。
2. runtime ledger adapter 持久化并重放精确 SessionBinding 事件，提供连续且有上限的 cursor。
3. canonical schema/codegen 一次性生成 StrongFlow detail DTO 和不透明 projection cursor，
   同步更新 Rust 与 TypeScript。
4. `winwincode-publication` 产生绑定 Delivery revision、candidate、Verdict、人工批准和 target
   的可信 intent/result。
5. Control Plane 实现 `StrongFlowProjectionQueryPort`，在同一 read cut 下组合上述三类事实。
6. 最后接 HTTP/WebSocket adapter；Web 只消费生成 client，Worker 仍只走 ExecutionPort。

这个顺序避免 Control Plane 在依赖缺失时先造一份临时领域模型，也避免 transport 层成为第二
个 Delivery 或 runtime authority。

## 公开 API 变更性质

后续 detail DTO 和 projection cursor 是一次 canonical schema revision，属于 breaking
contract change。schema、Rust 生成类型、TypeScript 生成 client、StrongFlow Web 和 CLI 必须
同批切换。实现不保留旧 `delivery.get` 详情形状的 alias、双读或手写 DTO；`delivery.list`
如需继续使用紧凑列表项，应当拥有明确独立的生成类型，而不是把旧详情 DTO 留作兼容入口。
