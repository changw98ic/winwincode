# Rust 方案审核权威合同

本合同冻结 Rust Control Plane 中唯一的方案审核事实、编码、状态和安全投影边界。它从现行 TypeScript plan-review 行为提取仍然有效的产品规则，再按当前 Rust Delivery、StageRun、SessionBinding 和 Attention 规则纠正执行身份。

机器可读规则位于 `delivery-solution-review-authority.rules.json`。本合同只定义必须实现的行为，不把文件存在或测试名称当作实现完成证明。

## 1. 唯一事实

唯一事实类型是 `ValidatedSolutionReviewSet`。它同时表示待审核和已结算的同一份 review set：

| `reviewStatus` | `decision` | `reviewerId` / `reviewedAt` | 可以提升任务并执行 |
| --- | --- | --- | --- |
| `pending` | `null` | `null` / `null` | 否 |
| `approved` | `approve` | 必须存在 | 是 |
| `changes_requested` | `request_changes` | 必须存在 | 否 |
| `rejected` | `reject` | 必须存在 | 否 |

`pending` 不是一个空占位。它已经包含可安全展示的方案、图、风险、未决项和非空有序任务提案，因此人工可以在决定前看到自己正在审核的内容。已结算状态继续引用相同的 `reviewSetSha256`。

`authority.pending_and_settled_are_one_type`：不建立“待审核方案”和“已批准方案”两套互不相干的数据。状态变化只结算同一个经过验证的 review set。

`authority.current_exact_review_set`：事实必须精确绑定当前：

```text
Delivery ID
+ DeliverySpec ID / revision
+ planning StageRun
+ planning SessionBinding
+ human review StageRun
+ AttentionItem
+ reviewSetSha256
+ reviewStatus / decision
+ reviewerId / reviewedAt（已结算时）
```

当前 Spec 中出现零个、多个、旧的或外来的 review set 都整体失败，不选择“最像当前”的一项。

## 2. 唯一 v1 编码

新的 canonical 编码只有以下两个严格分支，属于同一个 v1 协议族：

```text
winwincode.solution-review-context.v1
winwincode.solution-review-decision.v1
```

`encoding.one_strict_v1`：两个对象都拒绝未知字段、缺失字段、重复身份、越界值和错误协议。旧 TypeScript `winwincode.plan-review-*.v2` 只用于追溯来源，旧 v2 输入直接拒绝，不提供 alias、双读、fallback 或第二套摘要算法。

### Context exact keys

```text
schemaVersion
protocol
deliveryId
deliverySpecId
deliverySpecRevision
planningStageRunId
planningSessionBindingId
reviewStageRunId
attentionItemId
solution
architectureDiagram
processDiagram
risks
unresolvedItems
taskProposals
preparedAt
reviewSetSha256
```

### Decision exact keys

```text
schemaVersion
protocol
deliveryId
deliverySpecId
deliverySpecRevision
reviewStageRunId
attentionItemId
reviewSetSha256
action
comments
requestedChanges
```

`action=approve` 时 `requestedChanges` 必须为空；`request_changes` 必须列出至少一项修改；`reject` 必须有说明。公开投影只显示 typed `decision`，不返回完整 resolution JSON。

## 3. 摘要与任务提案

`digest.covers_ordered_task_proposals`：`reviewSetSha256` 是以下严格 v1 context（不含摘要字段本身）的 SHA-256 小写 64 位十六进制摘要：

```text
schemaVersion / protocol
Delivery / Spec / planning / review / Attention identities
solution / architectureDiagram / processDiagram
risks / unresolvedItems
taskProposals（保留数组顺序）
preparedAt
```

编码采用 Rust typed struct 的固定字段顺序、UTF-8、无额外空白。重启后解析并重新编码必须得到相同字节和摘要。任何任务字段变化或任务顺序变化都必须改变摘要。

`DeliveryTaskProposal` exact keys：

```text
id
title
goal
acceptanceCriterionIds
blockedByTaskIds
```

提案列表必须至少一项，最多 200 项。列表顺序是人工批准的产品顺序。任务 ID 唯一；每个任务至少引用一个当前验收条件；依赖必须存在、不能自引用、不能成环。Planner 不提交 `owner` 或初始 `status`；后续批准提升时 Control Plane 写入 `owner=null`、`status=pending`。

## 4. Stage 与人工身份

`stage.planning_binding_is_exact`：planning StageRun 必须是当前 Delivery-level、`codex/planner/succeeded` 运行，并且只存在一个完整的 planning SessionBinding。缺失、重复、旧的或外来的 binding 都拒绝整个 review set。

`legacy.human_review_binding_is_rejected`：Human plan-review 不是 `ExecutionJob`。Human review StageRun 没有 SessionBinding，也不伪造 WorkerSession 或 CodexThread。旧 TypeScript 的 `reviewSessionBinding` 不进入 v1 context、validated fact 或 projection。

待审核时，human review StageRun 是 `waiting` 或 `running`，Attention 是当前 `decision_required/open/blocking`。结算时：

* `approve` 对应 `resolved` Attention 和 `succeeded` review StageRun。
* `request_changes`、`reject` 对应 `dismissed` Attention 和 `failed` review StageRun。
* 所有结算状态的 authenticated reviewer 必须等于 `AttentionItem.resolvedBy`，存在 `assignedTo` 时也必须相等。
* `reviewedAt` 必须同时等于 `AttentionItem.resolvedAt` 和 review StageRun 的完成时间。

`decision.exact_settlement`：不能用调用方提交的 reviewer、时间或 action 替代这些当前事实。

## 5. 生产构造边界

`authority.production_resolver_only`：`ValidatedSolutionReviewSet` 是 opaque production fact：

* 类型只在 delivery crate 内可见，字段私有。
* 不实现 `Deserialize`。
* 没有 public raw constructor。
* HTTP request、WebSocket、Worker payload 和投影调用方都不能提交该事实。
* 测试构造只存在于 solution module 的 `cfg(test)` 私有代码中。

生产 `resolve_current_solution_review` 位于 `winwincode-delivery` crate 内部。它只从当前 canonical Delivery 的 Attention context/resolution、StageRun 和 SessionBinding 重建 typed v1，校验 exact keys、bounds、摘要、状态和身份，再产生 sealed fact。Resolver 缺失或任何 trusted fact 不足时，不返回半成品投影。

## 6. 唯一公开 wire

唯一 Delivery detail 字段是 `solutionReview`，唯一类型是 `SolutionReviewProjection`：

```text
solutionReview: SolutionReviewProjection | null
```

不保留 `solution` alias。

`projection.pending_review_is_visible`：当前 pending review 返回 safe solution、diagrams、risks、unresolvedItems、非空 ordered `taskProposals`、`reviewStatus=pending`，同时 `decision/reviewerId/reviewedAt=null`。

`projection.safe_fields_only`：公开字段只包括 exact authority IDs、`reviewSetSha256`、bounded solution、diagrams、risks、unresolvedItems、task proposals、review status、typed decision、reviewerId 和 reviewedAt。

raw Attention `context` 和 `resolution` 永远不进入公开投影。以下数据也禁止出现：credential、authorization、provider request/response、tool payload/output、runtime log、stdout/stderr、human review SessionBinding 和 ExecutionJob identity。

`promotion.approved_only`：只有 `reviewStatus=approved` 才能把同一摘要中的 task proposals 原子提升为 canonical DeliveryTask 图并开始执行。`pending`、`changes_requested`、`rejected` 均不授权提升或执行。

## 7. 当前实现缺口与 Rust 门禁

当前实现有四个已确认缺口：

1. 只有 `ApprovedSolutionReviewSet`，pending review 没有 typed safe projection。
2. `ProjectionInput.with_approved_solution` 接收调用方事实，production resolver 尚未存在。
3. 事实、摘要和投影没有 ordered `DeliveryTaskProposal`。
4. wire 仍叫 `solution: SolutionProjection`，不是唯一的 `solutionReview: SolutionReviewProjection`。

这些 finding 在 machine rules 中标为 `present`。合同预检以 planned gate 通过，不让主线因后续实现尚未开始而常红。

阶段 2.5.1.2 的明确 trigger 是 `crates/winwincode-delivery/src/application/solution_review.rs`。该文件出现后，四个 finding 必须改为 `closed`，Node gate 会检查实际代码，而不是只检查测试名：

1. `ValidatedSolutionReviewSet` 和 `DeliveryTaskProposal` 字段私有且 exact。
2. 事实没有 `Deserialize` 或 public raw constructor。
3. `resolve_current_solution_review` 是 crate 内部 resolver；投影 consumer 不是 public API。
4. `DeliveryProjection` 只有 `solution_review: Option<SolutionReviewProjection>`，没有旧 `solution` 字段。
5. 源码不含旧 v2 wire marker、`ApprovedSolutionReviewSet` 或 caller `with_approved_solution` 注入路径。
6. 对抗测试真实改变 pending/settled 状态、任务顺序、Spec/run/binding/Attention/reviewer/time、Human SessionBinding 和 raw secret/tool/log 内容，并断言拒绝或排除。
7. 编码重启往返保持相同字节和摘要。

这些检查只证明结构和指定行为存在；最终完成仍以 Rust 黑盒测试、完整门禁和对应 Bead 状态为准。
