# ADR-0039: Integration Fusion/JEV and Snapshot Remediation

Status: Accepted

Date: 2026-09-24

## Context

The review of `integration/fusion-jev-and-lint-cleanup` at `e5cc34e5`, against
`origin/main` at `da407e4f`, found that the Snapshot and Fusion/JEV integration
paths do not satisfy the ownership, evidence, lifecycle and evaluation contracts
in ADR-0028 and ADR-0033 through ADR-0038. The complete findings and evidence are
recorded in `docs/defects/2026-09-24-integration-fusion-snapshot-defect-review.md`.

This ADR records the product-owner decisions Q-01 through Q-09. It is the
implementation contract for the remediation work tracked under
`winwincode-community.7`. It does not weaken `winwincode-community.5.13` through
`winwincode-community.5.16`.

## Decisions

### Q-01. Snapshot ownership and production chain

Control Plane creates, assigns and persists the product Snapshot. Worker
materializes the requested input, enforces execution isolation and reports
execution facts. This does not change the Worker/Delivery dependency boundary.

- Control Plane owns Candidate product identity, Snapshot identity allocation
  and persistence, verification binding, product state transitions and final
  acceptance records.
- Worker owns exact input materialization, execution isolation, command execution
  and output storage. It does not allocate a product Snapshot, write Delivery
  product state or decide final acceptance.
- A Verifier evaluates a specified Snapshot and verification scope. It cannot
  replace the code under test, alter acceptance rules or overgeneralize evidence.
- Shared contracts define cross-boundary data and validation. They do not become
  a second product-state authority.
- The Worker production dependency closure must not introduce Delivery.

The only production sequence is:

```text
Candidate is sealed and its artifact and pin are valid
  -> Control Plane sends an idempotent freeze request
  -> Worker materializes the specified code and establishes protection
  -> Worker reports freeze execution facts and artifact references
  -> Control Plane validates Candidate, code identity and request ownership
  -> Control Plane allocates and creates the canonical Snapshot
  -> Control Plane transactionally stores Snapshot, binding and outbox event
  -> Verification is dispatched
  -> receipt -> Evidence -> VerifierResult -> Verdict
```

No verification execution may begin before Snapshot persistence and session
binding. A successful Worker freeze is not a product Snapshot. Requests use the
existing idempotency and workrun-generation constraints.

VerificationSession, command/test/artifact receipts, Evidence, VerifierResult and
Verdict must all resolve to the same Snapshot. Runtime code injects and validates
these bindings. Models do not supply authoritative binding fields. Missing,
foreign, stale-attempt or mismatched bindings are rejected both at execution and
result ingestion. A late result from an older attempt remains auditable but
cannot overwrite the current attempt.

The model context carries the necessary `snapshotId`, `evidenceId`, `claimId`,
current constraints and the evidence content or digest needed for the current
judgment. Complete receipts and metadata remain in SQLite and artifact storage
and are referenced on demand. Context economy cannot remove semantic material
needed to make a judgment.

`ValidatedGitSnapshotFact`, `CandidateSnapshot` and their production construction
and read paths leave the new verification chain. No runtime alias, compatibility
reader or fallback to the old fact graph is retained. Old Snapshot facts and old
benchmark history are excluded from the new system and are neither retained as
new-system records nor migrated. New canonical records start without importing
legacy identity or evidence graphs.

Recovery is idempotent at each boundary:

| Interruption | Recovery |
| --- | --- |
| Worker freeze incomplete | Retry the original request; do not dispatch verification |
| Worker frozen, binding uncommitted | Recover the binding idempotently; clean unbound material only after confirming no references |
| Snapshot committed, dispatch pending | Replay the existing outbox event |
| Receipt delivered repeatedly | Store idempotently; do not create a second terminal state |
| Late result from an older workrun | Keep audit only; do not change the current binding or conclusion |

### Q-02. Canonical identity and immutability

Candidate uses canonical `CandidateId`. Snapshot uses the existing canonical ULID
allocator and `snap_` prefix. A code digest is stored separately for consistency
checks and never allocates identity.

- A retry of the same freeze request returns the same product result.
- A new verification attempt reuses the same Snapshot.
- Changed protected input creates a new sealed Candidate and Snapshot.
- Changed runtime environment or execution parameters create a new execution
  condition or attempt and cannot masquerade as the original execution.
- Arbitrary candidate strings, a second Snapshot ID encoding and production
  Snapshot mutators are prohibited.

Snapshot code identity and sealed content are immutable. Retry counts, scheduling,
materialization availability and cleanup state belong to associated execution
records. Changing tested input requires a new Snapshot; there is no temporary
immutability bypass. Legacy Snapshot IDs, facts and evidence links are not
translated into canonical IDs; there is no historical migration path.

### Q-03. Protected source and writable execution space

Freeze the protected input that verification actually reads: source, controlled
test inputs, build scripts, lockfiles and controlled configuration. The versioned
contract defines the scope; an executing model cannot narrow it. Runtime
environment, external dependencies and execution parameters are recorded
separately.

Use two logical areas:

```text
protected input: source, controlled tests, build scripts, lockfiles and config
  - read-only for the complete verifier and child-process execution chain
writable scratch: build output, cache, temp files, coverage, logs and exports
  - isolated per verification attempt and never part of source identity
```

Build layout follows the project under test. Steps that must generate or change
protected source run before Candidate sealing. Verification cannot adapt source
to make a test pass and then attribute the result to the original Snapshot.

Protection applies to the Verifier, tested commands and child processes,
including build and test hooks. Permission changes, replacement, rename and path
redirection cannot bypass it. Materialization cannot also be used as another job
or development workspace. Trusted hosts and administrators remain outside the
threat model.

`git worktree` may materialize input but does not provide read-only enforcement.
Pre/post digests remain supplemental checks and do not prove in-execution
immutability. Tests must cover modify-then-restore attempts.

The unified contract is Runner capability and acceptance, not one filesystem
implementation. Linux, macOS and every declared supported platform must prove the
same protected-input capability. A Runner that cannot provide it must transfer the
work to a capable Runner or produce a reasoned pending state. It cannot issue a
strong verification pass from a writable copy plus before/after digests.

Production-entry acceptance blocks create, modify, delete, rename, replacement
and bypass attempts against protected input, including modify-then-restore. It
allows authorized scratch writes, isolates scratch across attempts and withholds
a strong pass when protection is unavailable or fails.

### Q-04. Claims, receipts and claim verification

Three levels are distinct:

| Level | Meaning | Allowed effect |
| --- | --- | --- |
| Claim / Hypothesis | Model reasoning, questions and proposed actions | Enter investigation and request verification |
| Source Receipt | Locatable file, command, test, diff, run event or independent-review fact | Prove a source exists and bind its version and execution ownership |
| Claim Verification | Verification of a proposition for a version, premise and scope | Confirm or refute a claim and change corresponding acceptance state |

A real source does not by itself prove the model's proposition. A passing test
receipt proves that execution only; its coverage and scope must be evaluated
against the claim. Models may propose new code paths and counterexamples. Those
proposals enter a verification queue and become evidence only after independent
checks succeed.

`produced_new_evidence` requires a valid source and receipt, a real increment over
existing material, and relevance to the current dispute. Prose restatement,
invented receipt IDs, duplicate output, repackaging and unsupported reproduction
claims are not increments. Re-execution counts only when it adds a verifiable
observation, reproduction result or planned stability evidence relevant to the
investigation.

Fusion is an Oracle Union by default. Valid findings and unresolved disagreement
remain in the claim set. Claim identity uses proposition, scope and version, not
surface wording. Majority opinion cannot remove a valid minority finding.

Confirmation, refutation of a supported claim, `ConfirmedOpposes`, withdrawal of
valid support and final acceptance from a claim all require verification. A
counter must address the same proposition, scope and applicable version and must
be independently checked. Unverified R3 material and a 4:1 opinion distribution
cannot satisfy this gate.

Verifier output includes basis, scope and binding. Component names confer no
authority. Machine receipts prove execution facts; Verifier interpretation
decides whether those facts satisfy a proposition or acceptance rule.

Blind JEV input excludes provider, model and seat identity, support counts,
majority/minority labels and brand-revealing metadata. It uses stable anonymous
claim and evidence IDs within the run; identity mapping remains in audit. Evidence
content is not anonymized: paths, commands, outputs, preconditions and version
relationships remain available. Seat renaming, reordering and repeated identical
support do not change judgment semantics.

### Q-05. JEV authority, lifecycle and recovery

JEV retains three bounded roles:

- Context and memory management: choose retention, compression, archival,
  discard and reconstruction without changing Canonical State.
- Semantic judgment: explain disputes and issue reversible provisional leading
  and evidence-sufficiency judgments without manufacturing final facts.
- Verification planning: select the next evidence, tool, reproduction or
  independent-verification action within permissions and the frozen experiment
  termination rules.

JEV is neither a second execution kernel nor a queue-only component.

Across compression, archival and reconstruction, preserve active instructions
and hard constraints, valid confirmed/refuted conclusions, unresolved disputed
claims, valid evidence and references, current task/Snapshot/verification binding,
and necessary pending actions or blockers. Structured summaries and references
are sufficient. Moving stale material out of context does not revoke its facts.
Publish a rebuilt context only after validating the fact version. A failed version
check preserves the old context and retries generation or application.

High-frequency judgment does not imply high-frequency context rewriting. Policy
considers projected context growth, model attention behavior, interference risk,
cache-reusable prefixes, rebuild and future-call cost, foreground latency and
expected remaining work. Thresholds are model/strategy configuration. Formal runs
freeze their configuration.

`JevProvisional` remains awaiting verification:

```text
dispute -> reversible JEV provisional -> Awaiting Verification
  support -> confirm the scoped conclusion
  verified counter -> replace the provisional conclusion
  insufficient evidence -> unresolved and continue or block
```

A provisional cannot close a required dispute. Verifier results can replace it in
either direction. The original reason and replacement basis remain auditable.

On timeout, transport failure or invalid response, record `JEV_UNAVAILABLE` and
retain anchor, claims, disputes, evidence and completed independent
investigations. Continue deterministic or independent verification where
possible; otherwise produce `unresolved` or `attention`. Do not fabricate leading
or lower acceptance. Fail-open means investigation and execution may continue
under a degraded strategy, not that acceptance automatically passes.

A round without new evidence cannot immediately become `Unresolvable`. Continue
with a meaningful action such as targeted source reading, tool execution,
reproduction or independent verification. Formal task runs and the full batch
have no token, call, wall-time or monetary upper limit. Stop only on a valid
terminal result, an unrecoverable execution/permission failure, or stuck-tool
detection. A tool-request identity is the hash of tool name, target resource,
canonical arguments and requested-content digest, excluding request IDs,
timestamps and progress metadata. When the same identity has already occurred
more than five times in one run, intercept the sixth request before execution,
record `STUCK_TOOL_REPEAT_LIMIT`, and terminate the local runner. It is never a
pass and remains in the denominator. Missing permission, dependency or material
creates an attention or blocker. `unresolved` and `inconclusive` are reasoned,
recorded and recoverable states; new evidence, restored dependencies or manual
authorization can resume them.

The final gate covers only the declared acceptance scope. Unresolved discussion
outside that scope does not block a satisfied delivery. Any unresolved necessary
acceptance item blocks overall acceptance.

### Q-06. Frozen evaluation contract

The formal capability evaluation uses exactly 20 tasks from the public repository
<https://github.com/changw9813/agent-benchmark-tasks>, frozen at commit
`fa9da301e493fb88d48c86cb8954ed46d9cd2ffe`. Every task records source/version,
input Snapshot, requirement and acceptance rules, allowed tools/permissions,
scoring, observed cost/time and required preserved constraints/facts. Tuning and
formal task sets are separate. Hand-populated metric fixtures test arithmetic only
and are not capability evidence. Fault injection is a separate suite and is
reported separately.

The frozen comparison axis contains four provider runs and one independent
aggregation result:

| Comparison | Composition |
| --- | --- |
| `glm5.1flash` | single model |
| `mimov2.6pro` | single model |
| `ds4.1flash` | single model |
| `qwen3.8flash` | single model |
| `fusion(4)` | one independent run invoking the four members above once each, followed by one aggregation |

Every provider/member model call uses reasoning effort `max`. The controlled main
experiment is:

| Arm | Post-candidate Fusion processing | JEV |
| --- | --- | --- |
| A | off | off |
| B | off | on |
| C | on | off |
| D | on | on |

`fusion(4)` is not a provider and is not a candidate-generation stage before a
second Fusion pass. It is one independent four-model aggregation run: its four
members each produce one result and one aggregation combines them. It does not
reuse the standalone provider-run outputs and is never recursively re-fused. All
five comparison runs appear in every arm. The fixed main matrix contains
`20 x 4 x 4 = 320` standalone provider runs and `20 x 4 x 1 = 80` Fusion runs,
for 400 evaluation runs with no repeated sampling. Every Fusion run's four member
calls and one aggregation call are metered separately. Tasks, tool permissions,
acceptance rules, verification capability and termination rules are identical
across arms. Verifier is not a D-only variable. JEV-off arms retain each provider's
native context management or stable compression. Model membership, permissions
and acceptance rules do not change silently within a comparison.

Separate JEV ablations compare Context-only, Judge-only and Full JEV. Each
ablation crosses the same four standalone providers plus one independent
`fusion(4)` run and runs every task once, producing 300 additional evaluation
runs: 240 standalone provider runs and 60 Fusion runs. Aggregation scope, rebuild
thresholds, degradation and verification scheduling are versioned and frozen for
a formal batch.

Each model call sets reasoning effort to `max`. Neither a run nor the full batch
has a token, call, wall-time or monetary upper limit. The runner stops on the
stuck-tool condition defined in Q-05. A stuck-tool stop is recorded as an
unsuccessful run and remains in the fixed denominator. The formal benchmark
Runner is the local host `macOS 26.5.1 aarch64-apple-darwin`; no separate Runner
selection is required.

The task repository is read-only to evaluated agents. Each frozen result is
published by the scheduler to the separate public submission repository
<https://github.com/changw98ic/agent-benchmark-submissions> under
`submit/<task-id>/<run-id>`. Old benchmark results, old run histories and legacy
Snapshot facts are not retained in the new result set, imported or migrated.

The dynamic policy registry retains Provider Native, Current Stable, Previous
Best and Candidate as policy versions and competition states, not replacements
for the experimental axes. Promotion requires held-out validation, not the best
tuning run.

Report three single-model references: every one of the four evaluated single
models, the batch-best single model selected by mean score across all tasks, and
the post-hoc per-task best single-model result. `fusion(4)` is not a single-model
baseline. The post-hoc result is explicitly retrospective and is not an
online-dispatch claim.

Scores are normalized to `[0,1]` and tasks are equally weighted. Each matrix cell
has one score because formal cells are not repeated. For batch-best single model
`b` and tested strategy `F`:

```text
Regret(F,b) = (1/N) * sum_t max(0, S_b,t - S_F,t)
```

Only losses accumulate; improvements cannot offset them. For tied batch-best
baselines, compute each and use the larger Regret for the gate. Report regret
against the per-task best separately and do not conflate its name.

The gate is:

| Metric | Counting rule | Gate |
| --- | --- | ---: |
| Regret | Per-task nonnegative loss against batch-best single model | <= 1% |
| Minority | Verified-correct minority claims retained in the final usable result | >= 95% |
| Capture | Verifiably correct union from candidate answers captured by final valid conclusions | >= 95% |
| Constraint Recall | Active hard constraints retained across cleanup/rebuild | 100% |
| Confirmed Recall | Active confirmed facts and necessary references retained across cleanup/rebuild | 100% |
| BindingError | Missing, foreign or wrong bindings incorrectly accepted | 0 |
| StateRegression | Illegal rollback, stale overwrite or provisional overwrite of verified facts | 0 |
| Hallucinated | Unsupported new entries presented as fact/evidence by rebuild or final conclusion | 0 |

Correct claims left only in raw logs do not count as retained or captured.
Unverified hypotheses do not count as confirmed Capture. Verified replacement of
a JEV provisional and correct rejection of an injected bad binding are not
regressions. Model hypothesis error and unproductive investigation cost are
reported separately. Constraint and binding checks are per-run, not averages.
An empty sample is "insufficient evidence", not 100%.

Task list and denominator freeze before execution. Stuck-tool stops, unrecoverable
provider/tool failures, missing sources, malformed output and exceptions remain
in the denominator. A degraded completion is scored by its final result and
includes all retries. Fault injection is scored against its target state
transition. Neither a formal task nor the full batch has a scheduled
budget/deadline cutoff.

Quality, token/cache, cost, time and context effects are reported separately.
Measure model, Fusion, JEV and Verifier input/output, cache hits and rebuild
reinjection; all calls, failures, retries, judging and rebuild cost; wall time,
model wait, tool time, rebuild time and tail latency; and rebuild count/interval,
compression, bad eviction, stale residue and forgetting events. Cold and reusable
cache cases are separate. The 1,000-task/day projection is estimated from complete
task samples, while concurrency is evaluated separately from summed task time.

Each report binds source digest and commit, task-repository URL and revision,
submission-repository URL and frozen commit, policy version, model/configuration,
timestamp, per-task input/output and complete usage records. Every aggregate is
recomputable. Any post-run rule or configuration change creates a new experiment
version. Reports include numerator, denominator and uncertainty. Zero errors in a
small sample does not imply zero production error.

### Q-07. Integration branch and responsibility

Keep `integration/fusion-jev-and-lint-cleanup`; a full history rewrite is not a
precondition. New changes are independently commit-sized and grouped by Snapshot
identity/binding, source protection, Fusion evidence/state, JEV lifecycle/recovery,
Candidate pin/heartbeat, formatting/docs and benchmark evidence. Each commit maps
to one responsibility task and its acceptance evidence.

An independent Candidate pin, heartbeat or format fix can merge without the full
architecture, but its safety must be proved on that exact change. It cannot reuse
an unrelated integration-head result. Historical mixed commits retain a scope and
ownership ledger. Independent bug-fix merges do not imply that the Snapshot,
Fusion or JEV feature passed acceptance.

The integration branch retains the full real-benchmark merge gate. Fusion
acceptance required before integration merge cannot silently move to a later
release.

### Q-08. Beads responsibility and acceptance

Normalize the active issues involved in this integration, not the complete
historical backlog. Every active implementation issue has one owner, a bounded
design/ADR reference, structured acceptance criteria, dependency/blocker links and
code/evidence locations. Each defect maps to exactly one responsibility task. A
parent can own integration exit while children own separate defects without
duplicate implementation claims.

Status follows acceptance evidence rather than code presence. A close record
contains actual commit or source digest, command or experiment configuration,
result and evidence location, item-by-item acceptance mapping and remaining risk
with scope. Unit counts, model summaries, implementation presence or a clean
console alone are insufficient.

### Q-09. Heartbeat terminal monotonicity

Fix heartbeat now without expanding into Runtime Journal. All writes for an
attempt use one publication entry point that combines state checks and write
ordering. A background beater cannot bypass it.

`FINISHED` and `FAILED` are absorbing for a run/attempt. Later `ALIVE` or stale
phase records cannot overwrite them. A retry uses a new attempt or run generation
and cannot revive an old attempt.

Completion first closes the synchronization boundary to ordinary beats, then
stops and drains or awaits the beater, then publishes the terminal state through
the same entry point and confirms the write. A staging-file rename alone does not
provide this ordering.

Every record binds run and attempt generation and uses a monotonic sequence to
reject stale updates. The sequence prevents overwrite without encoding business
phases as a globally nonrepeatable order. A new attempt may repeat a phase but
cannot revive an older attempt. Heartbeat remains an execution observation and is
not a second delivery-terminal authority.

Deterministic tests cover background-check/finish interleaving, finish/write
commit interleaving, an old beat after terminal publication, a late old-attempt
record and multiple out-of-order beats. None may overwrite terminal or newer
current state.

## Consequences

- Snapshot, evidence and runtime-state ownership remain aligned with the three
  authoritative-owner rules.
- Strong verification is capability-gated and cannot be inferred from a writable
  checkout or model prose.
- The current branch remains blocked from integration merge until the identity,
  source-protection, evidence, JEV, runtime consistency, task, benchmark and clean
  checkout gates pass with current evidence.
- Local tests may land before complete features, but feature issues remain open
  until their production acceptance paths pass.
