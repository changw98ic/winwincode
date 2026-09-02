# Rust Control Plane StrongFlow 组合投影合同

机器规则位于
[`control-plane-strongflow-projection.rules.json`](./control-plane-strongflow-projection.rules.json)。
当前状态是 implemented/enforced：阶段 2.5.6 已实现 Control Plane 组合查询、共享读取截面、
生成 DTO 映射和持久事件 cursor；生产 HTTP/WebSocket adapter 仍由后续阶段接线。

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

## 当前实现状态

已经成立：

- `crates/winwincode-control-plane/src/strongflow_projection.rs` 提供
  `StrongFlowProjectionQueryPort`，输入和输出只使用 `winwincode-api` 生成类型；
- Delivery 模块拥有 requirements、solution、stage、task、Attention、Evidence 和 Verdict 的
  内部只读投影，且不反向依赖 API 或 Control Plane；
- `delivery.get` 签发 `StrongFlowReadCursor`，`runtime.projection.get` 必须重放同一 Delivery、
  runtime、publication、event stream、actor、scope 和 page limit 截面；
- runtime 与 publication 读取端口只接受封闭的可信事实。生产 adapter 未安装时返回
  `TRUSTED_FACTS_UNAVAILABLE`，不会用 Worker 消息或空 publication 冒充成功；
- 生成的详情 DTO 已包含当前方案审核、候选、Verdict、发布授权与结果的精确关联；
- storage 在同一事务中持久化 projection scope、resource stream、stream-local sequence、
  event id 和 outbox payload；
- Node 门禁会真实运行 Rust black-box integration test，不依靠测试名称判定完成。

当前还没有生产 HTTP server、WebSocket server，以及 Phase 3/4 所属的真实 Publication 与
runtime-ledger adapter。这个缺口由“缺少 adapter 时查询关闭”处理，不改变本阶段已经实现的
组合边界。

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

生成的详情投影会带上经过上述完整集合校验的 publication 结果。`publication.publish`
请求中的 `candidateDigest` 和 `target` 仍只作为 stale-check assertion，不能成为权威
candidate 或 target。

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

## 已关闭的预检风险

阶段 2.5.6.1 记录的六项预检风险都已有可执行关闭证据：

1. Delivery/runtime torn read：两个 query 共享完整可比较的 `StrongFlowReadCursor`；
2. 通用 loader 与 transport/storage 旁路：公开查询只走 typed query port，未来 transport
   source gate 明确拒绝 storage、`commit` 和 `load_state`；
3. runtime authority 缺失：查询只接受 sealed trusted adapter read，adapter 缺失时关闭；
4. Publication 关联不足：可信 binding 同时覆盖 Delivery、Spec、candidate、Verdict、人工批准、
   target 和 publication revision；
5. WebSocket cursor 不可恢复：scope-local stream sequence 与 outbox 事件原子持久化；
6. raw HTTP、WebSocket 或 Worker fact 构造：生产投影事实没有 raw/public constructor。

机器规则把这些项目保存在 `closedPreflightRisks`，每项都带 `status=closed` 和具体关闭证据，
不会继续把已经修复的预检状态报告为当前 P0。

## 已验证的实现链与后续接线

1. `winwincode-delivery` 已完成内部 requirements/solution/stage/task/Attention/Evidence/Verdict
   投影，保持不依赖 API。
2. trusted runtime read port 已冻结精确 SessionBinding、连续 sequence 与有上限 cursor；真实
   runtime-ledger adapter 在 Phase 4 接入。
3. canonical schema/codegen 已一次性生成 StrongFlow detail DTO 和不透明 projection cursor，
   同步更新 Rust 与 TypeScript。
4. `winwincode-publication` 已拥有完整 fact binding、持久 intent ledger、provider port 和
   Phase 3.3 的生产 GitHub HTTP/credential-reference adapter；它们共用同一恢复路径。
5. Control Plane 已实现 `StrongFlowProjectionQueryPort`，在同一 read cut 下组合三类事实。
6. 后续只接 HTTP/WebSocket adapter；Web 只消费生成 client，Worker 仍只走 ExecutionPort。

这个顺序避免 Control Plane 在依赖缺失时先造一份临时领域模型，也避免 transport 层成为第二
个 Delivery 或 runtime authority。

## 公开 API 变更性质

detail DTO 和 projection cursor 已作为一次 canonical schema revision 同步生成 Rust 与
TypeScript。StrongFlow Web 和 CLI 的产品入口切换仍需同批完成。实现不保留旧 `delivery.get`
详情形状的 alias、双读或手写 DTO；`delivery.list` 如需继续使用紧凑列表项，应当拥有明确
独立的生成类型，而不是把旧详情 DTO 留作兼容入口。
