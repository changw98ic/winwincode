# Rust Delivery 候选、证据、结论与返工合同

这份合同固定阶段 2.4 的行为。它不增加新的业务对象，也不把 Worker 的执行结果直接当成交付结论。Rust Control Plane 必须从当前 Delivery、已经保存的运行事实和不可变候选中重新计算 Evidence、CriterionResult 与 DeliveryVerdict。

机器可读版本位于
[`delivery-evidence-verdict-rework.rules.json`](./delivery-evidence-verdict-rework.rules.json)。
Node 门禁会先核对现行 TypeScript 行为与目标规则；当
`crates/winwincode-delivery/src/domain/candidate.rs` 出现后，同一门禁会开始检查每个指定的 Rust 模块和测试名。

## 1. 什么才是“当前候选”

候选是一次已经冻结的 Git 结果，不是第十一个 Delivery 业务对象。它由这些事实共同确定：

- 当前 Delivery、DeliverySpec ID 和 revision；
- 仓库与基线 revision；
- 产出修改的 executor 或 remediator StageRun；
- 该 StageRun 的精确 SessionBinding；
- base/candidate commit、tree、Diff SHA-256 和变化路径。

相同事实必须得到相同 candidateRef。任何字段被改写、DeliverySpec revision 改变，或者后续 executing/reworking 写入阶段已经开始，这个 candidateRef 都不再代表当前候选。后续写入阶段即使仍在运行，也已经足以让前一个候选失效。

对应规则：

- `candidate.freeze_exact_writer_facts`
- `candidate.invalidated_by_spec_or_writer_change`

## 2. Reviewer 与 Verifier 怎样保持独立

Reviewer 和 Verifier 都是必需角色。两者各自拥有角色明确的 verifying StageRun 和完整 SessionBinding；两者不能复用候选写入者的 Session，也不能彼此复用同一组 Session 身份。Adversarial Verifier 可以额外启用，但不能代替前两个角色。

每个验证工作区都是 `candidate-read-only`。如果运行事实显示 Reviewer、Verifier 或 Adversarial Verifier 成功写入文件或应用补丁，本次验证结果作废，候选引用仍只属于原 executor 或 remediator 的冻结结果。

目标系统中的 ProductSession、WorkerSession、CodexThread 是三个不同身份，Evidence 的来源必须通过 SessionBinding 同时匹配它们，不能继续把一个通用 session_id 当成三者。

对应规则：

- `verification.reviewer_and_verifier_required`
- `verification.role_sessions_are_independent`
- `verification.read_only_candidate_policy`
- `verification.successful_candidate_write_rejected`

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

对运行来源，解析器还必须找到唯一且更早的原始事实，并核对 ProductSession、WorkerSession、CodexThread、StageRun、角色与来源序号。来源缺失、重复、类型不符、属于别的 Session、属于别的候选，都会使这项 Evidence 失效。Git commit、Diff 和文件来源也必须能从冻结候选精确重建。

对应规则：

- `evidence.current_spec_revision`
- `evidence.current_candidate`
- `evidence.current_stage_run`
- `evidence.current_session_binding`
- `evidence.source_identity_exact`

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
| 依据完整且各角色一致指出产品失败 | `fail` |
| 依据完整、必需角色都结束且一致通过 | `pass` |

失败的测试或命令不能支持 `pass`。Agent 的完成消息也不能支持 `pass`；结论必须由程序检查来源事实后产生。

所有必需条件再按 `fail`、`infra_error`、`inconclusive`、`pass` 的顺序折叠成 DeliveryVerdict。Reviewer/Verifier 冲突已经先在单个条件上变成 `inconclusive`，不会被其中一个 `fail` 消息覆盖。

公开 `delivery.submit_verdict` 命令里的 candidateDigest、criterionResults 和 evidenceIds 都是不可信输入。Control Plane 必须重新读取当前 revision、候选、SessionBinding 与来源事实后再构造领域对象；调用方提交的 `pass` 不能直接写入 Delivery，Worker 的 job outcome 也不是 DeliveryVerdict。

对应规则：

- `verdict.pass_or_fail_requires_evidence`
- `verdict.all_criteria_exactly_once`
- `verdict.missing_session_is_inconclusive`
- `verdict.insufficient_evidence_is_inconclusive`
- `verdict.conflict_is_inconclusive`
- `verdict.environment_failure_is_infra_error`
- `verdict.failed_check_cannot_pass`

## 5. 返工为什么必须有边界

代码返工只能由 Codex remediator 执行。Control Plane 根据已存在的 reworking StageRun 数量计算下一次 attempt，不能接受调用方自报次数；总次数不能超过当前 DeliverySpec.maxReworkAttempts。

图上精确返工必须同时命中当前候选、Diff SHA-256、原 DeliveryTask 范围、当前图、节点、变化文件、hunk SHA-256 和当前 EvidenceRef。任一引用过期、属于别的路径或扩大任务范围，都不会启动返工。

remediator StageRun 一开始，前一个候选就失去“当前”资格。历史 Evidence 可以留在追加记录里，但不能再授权新的通过、审核或发布。remediator 必须产生新的候选，再经过独立 Reviewer、Verifier、逐项 Evidence 和新的 DeliveryVerdict。

当已用次数达到上限，或同一条件返工后再次失败，下一步进入定义澄清，不继续自动启动代码返工。

对应规则：

- `rework.precise_current_candidate_scope`
- `rework.bounded_remediator_only`
- `rework.invalidates_previous_candidate`
- `rework.repeated_or_exhausted_failure_clarifies`

## 6. Rust 实现位置与启用方式

阶段 2.4 只在 `crates/winwincode-delivery` 内实现这些领域判断：

```text
src/domain/candidate.rs
src/domain/verification.rs
src/domain/evidence.rs
src/domain/verdict.rs
src/domain/rework.rs
```

`candidate.rs` 是这组实现的启用标记。它尚未出现时，测试核对规则清单、现行 TypeScript 公开入口、已有行为测试和目标测试名；它出现后，五个模块及机器规则中列出的所有 Rust 测试都必须同时存在。这样阶段 2.4 开始编码后，不能只迁成功路径而漏掉失效、冲突和返工边界。

这些模块只计算 Delivery 事实。候选文件、worktree 和运行产物仍由 Execution Worker 管理；Control Plane 只读取精确引用并写入 canonical Delivery 状态。

## 7. 完整规则索引

| 规则 | 要保证的结果 |
| --- | --- |
| `candidate.freeze_exact_writer_facts` | 候选身份覆盖精确 writer 与 Git 事实 |
| `candidate.invalidated_by_spec_or_writer_change` | Spec 或后续 writer 变化立即使候选过期 |
| `verification.reviewer_and_verifier_required` | Reviewer 与 Verifier 都不能省略 |
| `verification.role_sessions_are_independent` | 两个验证角色及候选写入者使用不同 Session |
| `verification.read_only_candidate_policy` | 所有验证角色只能读取候选 |
| `verification.successful_candidate_write_rejected` | 验证 Session 的成功写入使结果作废 |
| `evidence.current_spec_revision` | Evidence 属于当前 Spec revision |
| `evidence.current_candidate` | Evidence 属于当前候选 |
| `evidence.current_stage_run` | Evidence 属于正确 StageRun |
| `evidence.current_session_binding` | Evidence 属于正确 SessionBinding |
| `evidence.source_identity_exact` | 来源类型与完整运行身份精确一致 |
| `verdict.pass_or_fail_requires_evidence` | pass/fail 必须有直接 Evidence |
| `verdict.all_criteria_exactly_once` | 当前条件全部且只评估一次 |
| `verdict.missing_session_is_inconclusive` | 缺 Session 时关闭通过路径 |
| `verdict.insufficient_evidence_is_inconclusive` | 证据不足时关闭通过路径 |
| `verdict.conflict_is_inconclusive` | 角色冲突保留并变成 inconclusive |
| `verdict.environment_failure_is_infra_error` | 运行环境问题不冒充产品失败 |
| `verdict.failed_check_cannot_pass` | 失败检查不能支撑 pass |
| `rework.precise_current_candidate_scope` | 返工只命中当前候选的精确位置 |
| `rework.bounded_remediator_only` | 返工角色和次数由服务控制 |
| `rework.invalidates_previous_candidate` | 返工后必须形成新候选并完整重验 |
| `rework.repeated_or_exhausted_failure_clarifies` | 重复失败或次数用尽进入定义澄清 |
