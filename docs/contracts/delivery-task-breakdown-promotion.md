# DeliveryTask 图原子提升合同

本合同冻结阶段 2.5.7 的唯一任务提升路径。当前状态是 implemented/enforced：旧的
`Vec<DeliveryTask>` 调用入口和 HTTP `payload.tasks` 已被替换并删除，没有别名、双读或旧格式
回退。

本次盘点先检查了本地代码索引状态。仓库没有可执行的 `dist/scripts/code-index.js`，所以
后续结论来自 Git 文件清单、`rg` 和逐文件读取，只声称文件级覆盖，不声称完整符号图或
调用图覆盖。

## 结果

Planner 提出的任务必须先进入同一个方案审核集合，和方案、风险、图、当前 DeliverySpec、
Planning StageRun、Planning SessionBinding、Human plan-review StageRun 与 Attention 一起
生成 `reviewSetSha256`。人批准的是这一个完整集合，不是一个随后还能被 HTTP 调用方替换
的任务列表。

最终权威链只有一条：

```text
DeliveryTaskProposal（在 typed solution review context 内）
  ↓ resolve_current_solution_review
ValidatedSolutionReviewSet
  ↓ approved_task_promotion，仅 approved
ApprovedTaskPromotion
  ↓ prepare_task_breakdown_promotion
TaskBreakdownPromotionTransition
  ↓ DeliveryCommand::ApproveTaskBreakdown
canonical DeliveryTask graph + Delivery journal
  ↓ ControlPlane::commit_delivery_task_breakdown
state + scoped receipt + DeliveryTaskBreakdownApprovedEvent outbox
```

这条链上的对象各有一个写入者。Web、HTTP、Control Plane、Worker 和 Codex Plan 都不能
直接构造 `DeliveryTask` 图。

## 1. Planner 提案属于方案审核集合

规则 `authority.planner_proposals_are_review_digest_input`：每个 `DeliveryTaskProposal` 只有
以下五个字段：

```text
id
title
goal
acceptanceCriterionIds
blockedByTaskIds
```

提案数量是 1 到 200 个，`title` 最多 256 个代码单元。提案的数组顺序和每个字段都进入
`reviewSetSha256`。调换两个任务、改一个标题、改一个验收条件或改一条依赖，都会得到
另一个审核集合。

Planner 不提供 `owner`、`status` 或 `deliveryId`。Codex Plan item 也不直接成为
`DeliveryTaskId`。这避免把 Codex 的运行计划复制成第二套产品任务事实。

## 2. 只有 approved seal 可以提升

规则 `authority.approved_seal_is_the_only_promotion_input`：
`resolve_current_solution_review(&Delivery)` 从当前 canonical Delivery 的 typed Attention
context/resolution 重建 `ValidatedSolutionReviewSet`。这个对象不是 HTTP DTO，也没有公开 raw
constructor 或 `Deserialize` 入口。

只有 `approved` 能产生 `ApprovedTaskPromotion`。以下状态都返回 `None`：

```text
pending
changes_requested
rejected
```

`ApprovedTaskPromotion` 是借用当前 validated review 的 crate-private seal。它不 `Clone`，
不序列化，不反序列化，只提供当前 `reviewSetSha256` 和有序 `task_proposals()` 的只读 getter。
它还私下保存 Delivery、Spec revision、review StageRun 和 Attention 的当前 attempt 身份，并
提供 `validate_for_delivery(&Delivery)`。这样从旧 Delivery 解析出的 approved seal 不能在同一
Delivery 后来产生的新 review attempt 上继续使用。

## 3. 领域内只有一个任务构造入口

规则 `authority.transition_is_the_only_task_constructor`：新模块固定为
`application::task_breakdown`，唯一构造入口是：

```rust
prepare_task_breakdown_promotion(
    &Delivery,
    &ApprovedTaskPromotion,
) -> Result<TaskBreakdownPromotionTransition, TaskBreakdownPromotionError>
```

这个函数隐藏全部映射、再次校验、revision 增量、源 Delivery seal 和事件构造。
`TaskBreakdownPromotionTransition` 的字段保持 private，没有 raw constructor，也不能从 JSON
恢复。调用方只能使用这一个深模块接口，不能把编辑后的 Delivery snapshot 冒充合法提升。
函数必须先调用 `ApprovedTaskPromotion::validate_for_delivery`，确认 seal 仍属于传入的当前
Delivery attempt，然后才能读取 digest 和有序提案。

旧的 `application::task::approve_task_breakdown(..., Vec<DeliveryTask>, ...)` 必须删除。
这不是保留旧入口再包一层新函数。

## 4. 提案到任务逐项等价

规则 `mapping.proposal_fields_and_order_are_exact`：合法提升后，顺序和五个提案字段逐项不变：

| Proposal | DeliveryTask |
| --- | --- |
| `id` | `id` |
| `title` | `title` |
| `goal` | `goal` |
| `acceptanceCriterionIds` | `acceptanceCriterionIds` |
| `blockedByTaskIds` | `blockedByTaskIds` |

不能重新排序成拓扑序，不能改写标题，不能合并任务，不能补出调用方提供的字段。

规则 `mapping.control_plane_derives_owner_and_status`：另外三个字段由产品事实产生：

```text
deliveryId = 当前 Delivery.id
owner = None
status = pending
```

其中 `owner = None`，`status = pending`。之后的 owner 分配和状态变化走各自的 canonical
命令，不能夹在本次审核批准里。

## 5. 图校验先于任何修改

规则 `graph.invalid_graphs_change_no_fact`：以下任一情况都拒绝，且 Delivery、journal、命令
回执和 outbox 都不变化：

1. 零个任务；
2. 重复 task id；
3. 某个任务没有验收条件；
4. 同一个任务重复引用验收条件；
5. 引用旧 Spec 或不存在的验收条件；
6. 全图没有覆盖当前 Spec 的全部验收条件；
7. 自依赖；
8. 同一个依赖重复出现；
9. 依赖的 task id 不存在；
10. 任意长度的环。

这些规则在 typed solution review 建立 seal 时检查，提升时还要验证 seal 与当前 Delivery
完全一致。Production 不提供“构造一个错误 seal 再试”的入口；负例通过 typed context
解析和 module-local test support 验证。

## 6. 当前审核和 stale-check 必须完全匹配

规则 `freshness.current_delivery_spec_review_and_digest_are_required`：执行提升时必须重新确认：

```text
DeliveryId
当前 DeliverySpec id / revision
Planning StageRun
Planning SessionBinding
Human plan-review StageRun
DecisionRequired Attention
authenticated reviewer / reviewedAt
approved decision
reviewSetSha256
```

旧 Spec、外来 Delivery、旧 planning run、外来 binding、另一个 Attention、被修改的 digest
或非 approved 决定都在 transition 前失败。Human plan-review 本身不是 `ExecutionJob`，也不
增加伪造的 Human SessionBinding。

即使旧 review 当时确实是 approved，只要当前 Delivery 已经有新的 Spec 或 review attempt，
旧 `ApprovedTaskPromotion` 的 `validate_for_delivery` 也必须失败；不能只比较一个碰巧相同的
digest。

规则 `freshness.revision_race_and_second_approval_fail_closed`：任务图只能填入当前 Spec
revision 的空图一次。相同源 revision 的并发请求最多一个成功；另一个收到 revision
conflict，不留下半张图。已有任务图时再次批准返回 conflict。要换图，必须创建新的 Spec
revision，重新规划并重新审核。

## 7. HTTP 只传身份和 stale-check

规则 `http.payload_contains_stale_identity_only`：命令仍是
`delivery.approve_task_breakdown`，外层 `CommandEnvelope` 仍包含 authenticated actor、完整
scope、`requestId` 和 `expectedRevision`。调用方只能提交 `deliveryId` 和 `reviewSetSha256`：

```json
{
  "deliveryId": "dlv_...",
  "reviewSetSha256": "sha256:..."
}
```

`tasks` 不再是这个命令的输入。`taskProposals`、`owner`、`status`、raw review 和
`solutionReview` 也都是未知字段。schema 的 `additionalProperties` 必须为 `false`。

HTTP 使用生成的 `Sha256Digest`，格式是 `sha256:` 加 64 位小写十六进制。Delivery 内部
seal 当前保存 64 位小写十六进制；adapter 只剥离一个固定前缀并做逐字节比较，不接受
第二种 digest 表示。

canonical schema、Rust 生成类型和 TypeScript 生成类型现在都只保留这两个字段。门禁会拒绝
`tasks` 再次进入 payload；旧 payload 不作为兼容格式保留。

## 8. Store 只有专用命令

规则 `store.specialized_command_is_the_only_append`：Store 的唯一入口固定为：

```text
DeliveryCommand::ApproveTaskBreakdown(Box<ApproveDeliveryTaskBreakdown>)
```

`ApproveDeliveryTaskBreakdown` 只带 `delivery_id`、`request_id`、`request_digest`、
`expected_revision` 和 `review_set_sha256`，不带 `tasks`、snapshot 或外部 transition。
Store 从 verified journal tail 读取当前 Delivery，解析当前 approved review，取得 seal，再
调用 `prepare_task_breakdown_promotion`。

新 journal operation 是 `DeliveryMutationOperation::TaskBreakdownApproved`。普通
`AppendDelivery` 必须明确拒绝这个 operation，不能靠调用者声称 operation 名称获得权限。

## 9. 一个 Control Plane 事务写入四个事实

规则 `transaction.four_members_commit_or_rollback`：Control Plane 模块固定为
`task_breakdown_transaction.rs`，唯一 public 应用入口是
`ControlPlane::commit_delivery_task_breakdown(...)`。

每次首次提交把以下四个成员放进同一个 `ProductStateStorage::commit`：

```text
canonical Delivery state
Delivery journal record
actor + full scope + requestId command receipt
outbox rows: internal DeliveryTaskBreakdownApprovedEvent + public delivery.changed.v1
```

也就是状态、Delivery journal、命令回执和 outbox 事件一起提交。任何一个 insert、尾部比较
或 revision 比较失败，四个成员全部回滚，也不发布事件。门禁要求 SQLite failure injection
分别打断 `product_state`、`aggregate_journal_records`、`command_receipts` 和 `outbox`，然后
逐表证明没有部分数据。

`DeliveryTaskBreakdownApprovedEvent` 是同一个 transition 的 immutable internal event，
严格字段是：

```text
schemaVersion
deliveryId
deliveryRevision
deliverySpecId
deliverySpecRevision
reviewSetSha256
tasks（保持批准顺序）
```

内部事件 topic 是 `delivery.task_breakdown.approved`。事件包含第一次提交的有序任务图，便于
回执重放验证。同一事务还追加 `delivery.changed.v1`，其 canonical `changeKind` 是
`advanced`，并从该 Delivery 的持久本地 stream sequence 签发 projection
cursor。canonical 权威仍是 Delivery journal，不是 WebSocket 或内存广播。

## 10. 回执必须先于当前状态

规则 `replay.receipt_first_returns_original_graph`：Control Plane 先从 actor、完整 scope 和
`requestId` 生成 receipt identity，再从整个 canonical command 生成 digest。它必须先查原命令回执，再读取当前 Delivery 或重新解析审核集合。

相同 identity 和相同 command digest 命中回执时：

1. 不检查当前 `expectedRevision` 是否仍是最新 revision；
2. 不重新解析当前 solution review；
3. 从原 journal revision 验证当时的 Delivery snapshot；
4. 严格解码回执里的原事件；
5. 返回第一次提交的任务图、revision 和事件字节；
6. 不写第二条 journal record，也不重复写两条原始 outbox row。

这保证第一次成功后即使 Delivery 又进入执行阶段，网络重试也不会被误判为 stale，更不会
根据新状态重算另一张图。

相同 actor、scope、`requestId` 但 command digest 不同，返回 request conflict。不同 actor
或不同 scope 的同名 request 不共享回执。

## 11. 只发布已经提交的事件

规则 `outbox.only_committed_event_is_published`：transaction 先返回 durable receipt，
`ControlPlane` 再 flush outbox。publisher 不接收内存中的 transition 或待提交 event。

如果数据库提交成功但发布失败，四个成员保持 committed，尚未发布的 outbox rows 保持
pending，结果明确表示 publication pending。启动恢复或下一次 flush 发布同一个 event id、
同一份 event bytes 和同一个 public projection cursor，不重新执行任务提升。

## 12. 两条通用旁路都必须关闭

规则 `bypass.generic_append_and_commit_are_rejected`：普通 `Append` 和通用 `ControlPlane::commit` 都不能写任务提升。前者不能使用
`DeliveryMutationOperation::TaskBreakdownApproved`，后者继续拒绝全部 Delivery command，
包括 `DeliveryApproveTaskBreakdown`。

HTTP adapter 只能调用 `ControlPlane::commit_delivery_task_breakdown`。它不能直接拿
`ProductStateStorage`、`StateChange` 或 `AppendDelivery` 拼一个看似相同的提交。

## 13. 当前启用的真实门禁

以下四条实现与测试路径都已存在：

```text
crates/winwincode-delivery/src/application/task_breakdown.rs
crates/winwincode-control-plane/src/task_breakdown_transaction.rs
crates/winwincode-delivery/tests/task_breakdown_promotion.rs
crates/winwincode-control-plane/tests/task_breakdown_transaction.rs
```

当前门禁要求四条路径和 typed solution review 全部存在，并做三层验证：

1. 读取真实 Rust source，检查 crate-private seal、唯一 constructor、private transition、
   current-attempt validation、specialized Store command、receipt-first 调用顺序、四成员 commit
   和 committed-outbox 顺序；
2. 提取每个指定 Rust test 的函数体，确认它实际调用 production seam、覆盖相应输入并断言
   接受或拒绝，而不是只出现一个测试名；
3. 真正执行 `winwincode-delivery` unit/integration target 和
   `winwincode-control-plane` integration target，并确认每个测试运行成功。

门禁还会确认旧 `application::task::approve_task_breakdown`、HTTP `tasks`、generic Append
和 generic Control Plane commit 路径已经删除或明确拒绝。只有文档、字符串或空测试函数
不会让门禁通过。
