# Rust Delivery 候选、证据、结论与返工合同

这份合同固定阶段 2.4 的行为。它不增加新的业务对象，也不把 Worker 的执行结果直接当成交付结论。Rust Control Plane 必须从当前 Delivery、已经保存的运行事实和不可变候选中重新计算 Evidence、CriterionResult 与 DeliveryVerdict。

机器可读版本位于
[`delivery-evidence-verdict-rework.rules.json`](./delivery-evidence-verdict-rework.rules.json)。
Node 门禁会先核对现行 TypeScript 行为与目标规则；当
`crates/winwincode-delivery/src/domain/candidate.rs` 出现后，同一门禁会开始检查每个指定的 Rust 模块和测试名。

## 实现边界：阶段 2.4 只处理已经验证的事实

阶段 2.4 的 Domain 是纯判断层。它不打开 Git 仓库，不读取 Worker 工作区、Artifact 或运行
日志，也不把测试夹具当成外部事实已经验证的证明。它只接收构造入口受限的已验证值，检查
身份、时间、候选、角色、outcome 和返工范围之间是否一致，再派生 CandidateRef、EvidenceRef、
CriterionResult、DeliveryVerdict 和 Attention。

外部事实由以下边界产生：

| 已验证值 | 负责产生的边界 | 当前阶段 |
| --- | --- | --- |
| `ValidatedGitSnapshotFact` | GitSnapshotResolver 与 Artifact adapter | 后续 adapter 集成 |
| `VerifiedTerminalOutcome` | 阶段 2.3 coordinator 与 Worker outcome adapter | 扩充现有边界 |
| `AcceptedRuntimeSourceFact` | ExecutionPort ingestion 与追加式运行台账 adapter | 后续 adapter 集成 |
| `ValidatedCheckoutAttestationFact` | Worker checkout 与 Artifact adapter | 后续 adapter 集成 |

这些值不能由 HTTP DTO、公开命令或原始 Worker 消息直接构造。阶段 2.4 单元测试可以通过
crate-private 测试入口建立夹具，证明“缺少、过期或不匹配的已验证值会被拒绝”；它不代表
Git、Artifact、checkout 或运行台账 adapter 已经完成。对应 adapter 及集成门禁到位前，生产
路径保持关闭，不能靠一个 Domain 测试宣布候选或 Evidence 已经可信。

## HTTP 提交边界

`delivery.submit_verdict` 只表示“请 Control Plane 针对当前候选重新计算结论”。外部请求的
payload 只有两个字段：

```json
{
  "deliveryId": "dlv_01J00000000000000000000000",
  "candidateDigest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}
```

`candidateDigest` 只用于发现候选已经变化，不是 Evidence，也不是调用方提供的结论。HTTP
请求还必须带经过认证的 actor、完整 repository scope、requestId 和 expectedRevision。
Evidence、criterionResults、Verdict、Attention、Delivery status、credential、verification
结论和原始 runtime facts 都由服务端可信事实产生，出现在请求中会被当作未知字段拒绝。

相同 actor、repository scope、requestId 和完全相同的命令重试时，返回第一次保存的 HTTP
状态与响应正文，不重新计算。相同 requestId 改了命令内容时返回
`IDEMPOTENCY_CONFLICT`；revision 已变化时返回 `REVISION_CONFLICT`；候选摘要已经过期时
返回 `CANDIDATE_STALE`；可信事实接入尚未启用或暂时不可用时返回
`TRUSTED_FACTS_UNAVAILABLE`。完整机器合同以
[`control-plane-http.schema.json`](../../schema/winwincode/v1/control-plane-http.schema.json)
为唯一来源。

## 1. 什么才是“当前候选”

候选是一次已经冻结的 Git 结果，不是第十一个 Delivery 业务对象。它由这些事实共同确定：

- 当前 Delivery、DeliverySpec ID 和 revision；
- 仓库与基线 revision；
- 产出修改的 executor 或 remediator StageRun，包括 DeliveryTask、阶段、角色和 attempt；
- 该 StageRun 的精确 SessionBinding，包括 ProductSession、ExecutionJob、WorkerSession 和 CodexThread；
- base/candidate commit、tree、Diff SHA-256 和变化路径。

相同事实必须得到相同 candidateRef。任何字段被改写、DeliverySpec revision 改变，或者后续 executing/reworking 写入阶段已经开始，这个 candidateRef 都不再代表当前候选。后续写入阶段即使仍在运行，也已经足以让前一个候选失效。

仅仅把一组格式正确的 commit、tree 和摘要字符串传给 Control Plane，不会产生候选。
GitSnapshotResolver 与 Artifact adapter 必须重新读取 base/candidate commit 对应的 tree，
重建两者之间的 Diff，并逐项核对 Diff 摘要、变化路径和每个路径的对象 ID，再封装成
`ValidatedGitSnapshotFact`。阶段 2.4 Domain 只接受这个受限值；调用方字符串与 Git 实际
结果不一致，或者调用方绕过 adapter 直接提交字符串时，候选冻结直接失败。

这次 Git 读取还必须属于 producer SessionBinding 的精确成功 Worker outcome 和 Job 工作区。
adapter 负责产生对应的 `VerifiedTerminalOutcome` 和 `ValidatedCheckoutAttestationFact`，
Domain 负责核对它们与候选身份完全一致。仅有一个写着 `succeeded` 的 StageRun，或者拿到
别的 Job、attempt、Lease、fence、Worker 实例、WorkerSession 的产物，都不能冻结当前候选。

对应规则：

- `candidate.freeze_exact_writer_facts`
- `candidate.git_snapshot_is_rebuilt`
- `candidate.producer_outcome_artifact_exact`
- `candidate.invalidated_by_spec_or_writer_change`

## 2. Reviewer 与 Verifier 怎样保持独立

Reviewer 和 Verifier 都是必需角色。对当前候选，每个角色必须恰好解析到一个当前 assignment，
也就是一个角色明确的 verifying StageRun 和一个完整 SessionBinding；零个或多个 assignment
都会在读取 finding 前直接失败。两者不能复用候选写入者的 ProductSession、ExecutionJob、
WorkerSession 或 CodexThread，也不能彼此复用这些身份。Adversarial Verifier 可以额外启用，
但不能代替前两个角色。

每个验证工作区都是 `candidate-read-only`。如果运行事实显示 Reviewer、Verifier 或
Adversarial Verifier 成功写入文件、应用补丁或通过命令改变候选，本次验证结果作废。除了
检查已知写入事件，GitSnapshotResolver 与 Artifact adapter 还必须封装验证前后的 commit、
tree 和 Diff；Domain 比较这两个已验证快照。Git 快照发生变化时，即使事件种类没有被识别
为写入，也不能使用该 Session 的结果。

模型的最终回复不是验证阶段的终止事实。一个角色只有在 canonical StageRun 已经
`succeeded`，并且阶段 2.3 outcome adapter 已产生与当前 SessionBinding、ExecutionJob、
attempt、Lease、fencing token、Worker、Worker 实例、WorkerSession 和 CodexThread 全部匹配
的 `VerifiedTerminalOutcome` 后，才算完成。`turn.completed` 不能让仍为 `running` 的
StageRun 进入通过路径。
结构化结果和全部支持来源的 sequence 还必须不大于这个 Worker outcome 的
lastEventSequence。

目标系统中的 ProductSession、ExecutionJob、WorkerSession 和 CodexThread 是不同身份，
Evidence 的来源必须通过 SessionBinding 同时匹配它们，不能继续保留 `dshSessionId`、
`codexSessionId` 或一个通用 session_id 作为 Rust 运行合同。

对应规则：

- `verification.reviewer_and_verifier_required`
- `verification.role_sessions_are_independent`
- `verification.read_only_candidate_policy`
- `verification.successful_candidate_write_rejected`
- `verification.stage_and_runtime_terminal_agree`

## 3. Evidence 怎样绑定原始事实

EvidenceRef 只保存有界引用，不复制命令输出、完整日志或聊天内容。每项 Evidence 至少绑定：

```text
当前 Delivery
+ 当前 DeliverySpec ID/revision
+ 当前 candidateRef
+ StageRun
+ SessionBinding
+ evidence type
+ sourceRef
```

对运行来源，解析器还必须找到唯一且更早、已经被 Control Plane 接受的原始事实，并核对
ProductSession、ExecutionJob、WorkerSession、CodexThread、StageRun、角色、attempt、Lease、
fencing token、Worker、Worker 实例与来源序号。来源缺失、重复、晚于 finding、类型不符、
属于旧 Job/Lease、别的 Session 或别的候选，都会使这项 Evidence 失效。Git commit、Diff 和
文件来源也必须能从冻结候选精确重建。

Verdict 命令只提交有界来源位置，不提交 RuntimeEvent 正文、时间、outcome 或 sequence。
运行台账 adapter 从已经接受并持久保存的追加记录读取这些值，封装成
`AcceptedRuntimeSourceFact`；调用方不能靠抬高时间或拼装事件制造 Evidence。test、command、
file、diff、commit 和 finding 还必须绑定 adapter 产生的
`ValidatedCheckoutAttestationFact`：其 commit 与 tree 必须等于当前冻结候选。仅仅时间上
发生在验证阶段内，或者事件没有写 candidateRef，都不能证明它检查的是当前候选。

原始来源自己的时间和 sequence 必须不早于 StageRun 开始与 SessionBinding 建立时间，早于
引用它的 finding 和 Verdict，并且不超过匹配 Worker outcome 的 lastEventSequence。解析器
不能用 `max(stageStart, bindingTime, eventTime)` 抬高旧来源时间。

对应规则：

- `evidence.current_spec_revision`
- `evidence.current_candidate`
- `evidence.current_stage_run`
- `evidence.current_session_binding`
- `evidence.source_identity_exact`
- `evidence.source_time_within_stage_and_terminal`
- `evidence.accepted_runtime_ledger_only`
- `evidence.runtime_checkout_matches_candidate`

## 4. Verdict 怎样关闭误报通过的路径

Control Plane 按每个当前验收条件计算一项 CriterionResult。`pass` 和 `fail` 必须引用至少一项当前 Evidence；`inconclusive` 和 `infra_error` 可以用缺失或失败的验证状态说明原因。每个当前验收条件必须恰好出现一次，外部或重复条件都不接受。

每项条件先按下表分类：

| 观察到的事实 | CriterionResult |
| --- | --- |
| 缺少必需 Session、Session 仍在运行或没有形成完整结果 | `inconclusive` |
| 直接证据不足 | `inconclusive` |
| Reviewer 与 Verifier 对同一条件结论冲突 | `inconclusive` |
| 运行环境失败、超时、策略拒绝或取消 | `infra_error` |
| Agent 声称通过，但直接依据显示检查失败 | `inconclusive`，并保留 evidence-mismatch |
| 验收条件没有批准的 verificationMethod | `inconclusive`，并保持 verification-blocked |
| 只有 Agent 消息、最终回复、review finding 或泛型 runtime event | `inconclusive` |
| 依据完整且各角色一致指出产品失败 | `fail` |
| 依据完整、必需角色都结束且一致通过 | `pass` |

失败的测试或命令不能支持 `pass`。Agent 的完成消息也不能支持 `pass`；结论必须由程序检查来源事实后产生。

Agent 也不能重新解释直接结果：产品检查失败不能由消息改成 `infra_error`，成功的已声明检查
不能只靠消息改成产品 `fail`，运行环境故障也不能伪装成产品结论。程序先读取直接 outcome，
再结合每个必需角色的一致结论进行分类。

`pass` 和产品 `fail` 都要求每个必需角色提供当前直接依据并得出一致的产品结论。一方指出
产品失败，另一方缺失、仍在运行、证据不足或结论不明时，结果仍为 `inconclusive`；另一方
发生运行环境故障时保持 `infra_error`，不能被一条产品失败消息覆盖。只有在逐项结果已经
完成这一步判断后，才按 `fail`、`infra_error`、`inconclusive`、`pass` 的顺序汇总不同的必需
验收条件。

所有必需条件再按 `fail`、`infra_error`、`inconclusive`、`pass` 的顺序折叠成 DeliveryVerdict。Reviewer/Verifier 冲突已经先在单个条件上变成 `inconclusive`，不会被其中一个 `fail` 消息覆盖。

公开命令不接受调用方或 Worker 制作的 EvidenceRef、CriterionResult、DeliveryVerdict、Delivery
status 或业务 Attention。Control Plane 必须重新读取当前 revision、候选、SessionBinding 与
来源事实后再构造这些领域对象，并在一次原子变更中保存；调用方提交的 `pass` 不能直接写入
Delivery，Worker 的 job outcome 也不是 DeliveryVerdict。

对应规则：

- `verdict.pass_or_fail_requires_evidence`
- `verdict.agent_message_cannot_pass`
- `verdict.direct_evidence_controls_classification`
- `verdict.missing_verification_method_is_inconclusive`
- `verdict.required_roles_agree_on_product_outcome`
- `verdict.derived_facts_are_recomputed`
- `verdict.all_criteria_exactly_once`
- `verdict.missing_session_is_inconclusive`
- `verdict.insufficient_evidence_is_inconclusive`
- `verdict.conflict_is_inconclusive`
- `verdict.environment_failure_is_infra_error`
- `verdict.failed_check_cannot_pass`

## 5. 返工为什么必须有边界

代码返工只能由 Codex remediator 执行。Control Plane 根据当前 DeliverySpec 下全部
DeliveryTask 的 reworking StageRun 总数计算下一次 attempt，不能接受调用方自报次数，也
不能在切换任务后从 1 重新开始；总次数不能超过当前 DeliverySpec.maxReworkAttempts。

图上精确返工必须同时命中当前候选、Diff SHA-256、原 DeliveryTask 范围、当前图、节点、变化文件、hunk SHA-256 和当前 EvidenceRef。任一引用过期、属于别的路径或扩大任务范围，都不会启动返工。

启动前的标注和 prompt 不是最终边界证明。remediator 结束后，GitSnapshotResolver 与
Artifact adapter 封装新 Git 快照，Domain 把这个已验证值与批准内容比较；新候选增加了未
批准路径，或者改动了批准 hunk 以外的内容时，本次返工结果作废，不能冻结成下一轮候选。

remediator StageRun 一开始，前一个候选就失去“当前”资格。历史 Evidence 可以留在追加记录里，但不能再授权新的通过、审核或发布。remediator 必须产生新的候选，再经过独立 Reviewer、Verifier、逐项 Evidence 和新的 DeliveryVerdict。

当已用次数达到上限，或同一条件返工后再次失败，下一步进入定义澄清，不继续自动启动代码返工。

一个 Verdict 同时产生多个阻塞 Attention 时，下一状态由完整 action 集合决定，不能由最后
解决的那一项决定。定义澄清优先于代码返工，代码返工优先于验证重试；改变解决顺序不能把
本应进入 `Clarifying` 的 Delivery 带回 `Verifying`。

对应规则：

- `rework.precise_current_candidate_scope`
- `rework.result_stays_within_approved_scope`
- `rework.bounded_remediator_only`
- `rework.attempt_uses_total_delivery_history`
- `rework.invalidates_previous_candidate`
- `rework.repeated_or_exhausted_failure_clarifies`
- `attention.combined_actions_use_safest_transition`

## 6. Rust 实现位置与启用方式

阶段 2.4 只在 `crates/winwincode-delivery` 内实现这些领域判断：

```text
src/domain/candidate.rs
src/domain/verification.rs
src/domain/evidence.rs
src/domain/verdict.rs
src/domain/rework.rs
```

`candidate.rs` 是这组实现的启用标记。它尚未出现时，测试核对规则清单、现行 TypeScript 公开入口、已有行为测试和目标测试名；它出现后，五个模块及机器规则中列出的所有 Rust 测试都必须同时存在，并拒绝重新加入旧 `dshSessionId` / `codexSessionId` 字段。这样阶段 2.4 开始编码后，不能只迁成功路径而漏掉失效、冲突和返工边界。

这些模块只计算 Delivery 事实，并以受限构造的已验证值作为外部输入。候选文件、worktree 和
运行产物仍由 Execution Worker 管理；GitSnapshotResolver、Artifact、outcome 和运行台账
adapter 负责把外部事实验证并封装。它们的生产集成不由阶段 2.4 Domain 单元测试冒充完成。

## 7. 完整规则索引

| 规则 | 要保证的结果 |
| --- | --- |
| `candidate.freeze_exact_writer_facts` | 候选身份覆盖精确 writer、Job、Session 与 Git 事实 |
| `candidate.git_snapshot_is_rebuilt` | Git resolver 重建并核对 commit、tree、Diff、路径和对象 ID |
| `candidate.producer_outcome_artifact_exact` | 候选只从精确成功 Worker outcome 的 Job 工作区冻结 |
| `candidate.invalidated_by_spec_or_writer_change` | Spec 或后续 writer 变化立即使候选过期 |
| `verification.reviewer_and_verifier_required` | Reviewer 与 Verifier 都不能省略 |
| `verification.role_sessions_are_independent` | 每个验证角色只有一个当前绑定，且完整身份不复用 |
| `verification.read_only_candidate_policy` | 所有验证角色只能读取候选 |
| `verification.successful_candidate_write_rejected` | 事件或前后 Git 快照显示写入时结果作废 |
| `verification.stage_and_runtime_terminal_agree` | StageRun 与精确 Worker 终止事实同时完成才可结算 |
| `evidence.current_spec_revision` | Evidence 属于当前 Spec revision |
| `evidence.current_candidate` | Evidence 属于当前候选 |
| `evidence.current_stage_run` | Evidence 属于正确 StageRun |
| `evidence.current_session_binding` | Evidence 属于正确 SessionBinding |
| `evidence.source_identity_exact` | 来源类型与 Job、Lease、fence、Worker、Session 完整身份精确一致 |
| `evidence.source_time_within_stage_and_terminal` | 来源时间和序号位于 StageRun、绑定与终止边界内 |
| `evidence.accepted_runtime_ledger_only` | Evidence 只解析持久运行台账中的已接受事实 |
| `evidence.runtime_checkout_matches_candidate` | 运行来源的 checkout commit/tree 必须等于当前候选 |
| `verdict.pass_or_fail_requires_evidence` | pass/fail 必须有直接 Evidence |
| `verdict.agent_message_cannot_pass` | Agent 消息和泛型事件不能单独支持 pass |
| `verdict.direct_evidence_controls_classification` | 产品、环境与成功分类以直接 outcome 为准 |
| `verdict.missing_verification_method_is_inconclusive` | 缺少已批准验证方法时保持 inconclusive |
| `verdict.required_roles_agree_on_product_outcome` | 必需角色全部给出直接依据并一致后才 pass/fail |
| `verdict.derived_facts_are_recomputed` | 调用方和 Worker 不能提交派生结论对象 |
| `verdict.all_criteria_exactly_once` | 当前条件全部且只评估一次 |
| `verdict.missing_session_is_inconclusive` | 缺 Session 时关闭通过路径 |
| `verdict.insufficient_evidence_is_inconclusive` | 证据不足时关闭通过路径 |
| `verdict.conflict_is_inconclusive` | 角色冲突保留并变成 inconclusive |
| `verdict.environment_failure_is_infra_error` | 运行环境问题不冒充产品失败 |
| `verdict.failed_check_cannot_pass` | 失败检查不能支撑 pass |
| `rework.precise_current_candidate_scope` | 返工只命中当前候选的精确位置 |
| `rework.result_stays_within_approved_scope` | 返工后的新 Diff 仍受批准 task、文件和 hunk 限制 |
| `rework.bounded_remediator_only` | 返工角色和次数由服务控制 |
| `rework.attempt_uses_total_delivery_history` | attempt 使用当前 DeliverySpec 的全部返工历史 |
| `rework.invalidates_previous_candidate` | 返工后必须形成新候选并完整重验 |
| `rework.repeated_or_exhausted_failure_clarifies` | 重复失败或次数用尽进入定义澄清 |
| `attention.combined_actions_use_safest_transition` | 多个 Attention 按最安全动作推进且不受解决顺序影响 |
