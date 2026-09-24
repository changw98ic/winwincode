# WinWinCode Integration Branch Defect Review

Date: 2026-09-24

Branch: `integration/fusion-jev-and-lint-cleanup`

Fixed point: `origin/main` (`da407e4f3df3719718b23bd8913b73fe3ff73857`)

Reviewed head: `e5cc34e51531a165cb05b47dc4efe6bc453de0a4`

Scope: 12 committed changes covering Fusion/JEV convergence, immutable Snapshot,
heartbeat, outbox, CLI fixture, lint cleanup and agent documentation. Uncommitted
product-code changes were absent at the final review snapshot.

Purpose: consolidate implementation defects, contract drift, validation evidence
and the decisions required before remediation. This document is a review artifact;
Beads remains the source of truth for task status and acceptance tracking.

## Executive Decision

Current recommendation: **do not merge this branch and do not close its P0 Beads
issues**.

The branch contains useful tests and some correct lower-level fixes, but its two
main feature paths are not release-safe:

1. The Snapshot path is only partially integrated and still coexists with the
   types it was meant to replace.
2. The Fusion/JEV path violates evidence provenance, fail-open and provisional
   adjudication requirements recorded in ADR-0035 through ADR-0037.

There are 7 P0, 7 P1 and 2 P2 findings at the reviewed head. The product-owner
resolutions are listed in [Resolved Decisions](#resolved-decisions).

## Severity Model

- P0: can bind verification to the wrong code, manufacture evidence, violate
  authoritative ownership, or produce a false terminal decision.
- P1: violates an accepted contract or leaves an acceptance path unresolved, but
  has a narrower blast radius or an available operational recovery.
- P2: repository hygiene, maintainability or scope-control defect.

## Defect Summary

| ID | Priority | Area | Summary |
| --- | --- | --- | --- |
| SNAP-001 | P0 | Snapshot lifecycle | Snapshot is not constructed or frozen in the production verification path; old snapshot types remain authoritative. |
| SNAP-002 | P0 | Snapshot isolation | `git worktree` is detached but writable, so a verifier can pollute source before results are staged. |
| SNAP-003 | P0 | Snapshot contract | `Snapshot` identity and immutability do not match the canonical schema. |
| ARCH-001 | P0 | Dependency ownership | Worker directly depends on Delivery, violating the frozen dependency boundary. |
| FUS-001 | P0 | Evidence provenance | Model-authored strings are converted into evidence and counted as new evidence. |
| FUS-002 | P0 | Verified contradiction | Unverified R3 material can establish an opposing lead without verified counter-evidence. |
| FUS-003 | P0 | Fail-open | JEV failure aborts the whole Fusion composition instead of retaining anchor and disputed claims. |
| FUS-004 | P1 | Provisional lifecycle | `JevProvisional` is effectively terminal and cannot be overturned by later verifier results. |
| FUS-005 | P1 | Escalation | One investigation round without new evidence terminates as `Unresolvable` instead of escalating. |
| FUS-006 | P1 | Blind judging | JEV input includes seat/model identity, allowing brand or vote-source priors. |
| FUS-007 | P1 | Benchmark evidence | ADR-0038 metrics are exercised with hand-populated fixtures, not the required real benchmark. |
| DEL-001 | P1 | Candidate pin | The candidate pin transaction regression still fails at the expected-pinning assertion. |
| RUN-001 | P1 | Heartbeat | A background heartbeat can overwrite a terminal record with `ALIVE idle`. |
| BD-001 | P1 | Beads contract | Task acceptance, ownership and status drift prevent reliable ready/closed decisions. |
| SRC-001 | P2 | Source hygiene | The branch contributes format-check failures in ADRs and JSON fixtures. |
| PROC-001 | P2 | Scope | Lint cleanup and broad agent documentation are mixed into the feature integration branch. |

## Detailed Defects

### SNAP-001: Snapshot lifecycle is not production-bound

Priority: P0

Contract:

- `docs/superpowers/plans/2026-09-24-frozen-snapshot.md:7` says `Snapshot`
  replaces `ValidatedGitSnapshotFact` and `CandidateSnapshot`.
- The same plan requires freezing the candidate, allocating `snapshotId`, and
  binding every verification frame before verification begins.

Actual behavior:

- `crates/winwincode-delivery/src/domain/candidate.rs:128` still owns
  `ValidatedGitSnapshotFact` and the Delivery domain continues to use it.
- `crates/winwincode-worker/src/workspace.rs:498` still owns `CandidateSnapshot`;
  `snapshot_verification` at line 1397 remains the verification fact source.
- `freeze_worktree` is called only by
  `crates/winwincode-worker/tests/snapshot_freezing.rs:41`.
- Production construction of `SnapshotBuilder::new` was not found outside test
  helpers. The production staging path still accepts a separately reconstructed
  `CandidateSnapshot`.

Impact:

- The branch can report a `snapshotId` while evidence and verdict reconstruction
  continue through the old fact graph.
- A verdict cannot be proven to resolve to exactly one immutable Snapshot without
  cross-table inference.
- The old and new contracts can disagree during replay or recovery.

Required fix:

1. Select the Snapshot ownership model from decision Q-01.
2. Create and persist one Snapshot after Candidate sealing and before dispatching
   any verification job.
3. Make all verification session bindings, artifact frames, evidence and verdict
   reconstruction consume that one Snapshot identity.
4. Delete the replaced fact type and its call sites; do not keep a compatibility
   reader or alias.

Acceptance:

- A production vertical proves Candidate -> Snapshot -> verification binding ->
  Evidence -> Verdict without reading the old snapshot facts.
- A missing or foreign `snapshotId` is rejected before verification execution.
- Retrying verification against changed code allocates a different Snapshot.
- The old type names and constructors no longer compile into the product path.

### SNAP-002: Frozen source is writable

Priority: P0

Contract: `docs/superpowers/plans/2026-09-24-frozen-snapshot.md:7` requires a
read-only `git worktree`, so verifier writes cannot change what is tested.

Actual behavior:

- `crates/winwincode-worker/src/snapshot_worktree.rs:87` runs `git worktree add`
  but does not apply an OS permission, mount, sandbox or copy-on-write policy.
- `crates/winwincode-worker/tests/snapshot_freezing.rs:94` checks only that the
  new worktree starts clean. It never attempts a write and therefore does not
  prove read-only behavior.

Impact:

- A verifier, build hook or test can modify source before producing its result.
- A clean status at staging time does not prove the test ran against the original
  tree; code may have been changed and restored.
- Test results can be bound to the right commit ID while actually measuring
  different bytes.

Required fix:

Implement the enforcement selected in Q-03. At minimum, add a positive test that
attempts to create, delete and modify source files and proves all attempts fail,
while build/test scratch directories remain usable outside the sealed source tree.

Acceptance:

- Source mutation by the verifier is impossible or fails closed.
- Build outputs and test temp files cannot enter the source tree or digest.
- Re-running verification from one Snapshot yields the same source identity.

### SNAP-003: Snapshot identity and immutability breach the canonical contract

Priority: P0

Contract:

- `schema/winwincode/v1/domain.schema.json:2893` defines a canonical `snap_` ULID.
- `domain.schema.json:2903` requires a canonical `candidateId`.
- `domain.schema.json:2914` requires `immutable: true`.
- `docs/superpowers/plans/2026-09-24-frozen-snapshot.md:17` requires reuse of the
  existing canonical ULID allocator and forbids a second ID encoding.

Actual behavior:

- `crates/winwincode-delivery/src/domain/snapshot.rs:178` accepts an arbitrary
  non-empty `candidate_ref` string.
- `snapshot.rs:302` implements a separate SHA-derived `snap_` encoder.
- `snapshot.rs:114` exposes an unconditional public mutator named as a test seam.

Impact:

- Runtime `Snapshot` values do not necessarily round-trip through the canonical
  generated `Snapshot` contract.
- A caller can bind a non-canonical candidate identity.
- The public mutator contradicts the type's immutability claim even if the seal
  detects the modification later.

Required fix:

Adopt decisions Q-01 and Q-02. Use the canonical ID allocator and the canonical
candidate identity newtype. Remove production mutation; perform tamper tests
through serialization fixtures or a feature-gated test seam outside the product
API.

Acceptance:

- Every runtime Snapshot validates against the generated schema exactly.
- The same allocator rules and prefix checks govern Snapshot and other canonical
  IDs.
- Product code cannot mutate a built Snapshot through any exported API.

### ARCH-001: Worker depends directly on Delivery

Priority: P0

Contract:

- `docs/decisions/0028-control-plane-worker-dependency-rules.md:50` fixes Worker's
  production dependency closure.
- `AGENTS.md` reserves Delivery product state for Control Plane and execution
  facts for Worker.

Actual behavior:

- `crates/winwincode-worker/Cargo.toml:39` adds `winwincode-delivery`.
- `crates/winwincode-worker/src/workspace.rs:1477` imports and verifies the
  Delivery-owned Snapshot type.

Impact:

- The integration layer inverts ownership and permits Worker code to reach more
  Delivery behavior over time.
- Remote Worker transport semantics can now couple to Delivery internals even
  though remote deployment is supposed to replace transport only.

Required fix:

Resolve Q-01. Keep Worker limited to generated execution/domain facts and move
product Snapshot creation and persistence to Control Plane, or amend ADR-0028 and
the dependency inventory with a reviewed ownership change.

Acceptance:

- `cargo metadata --locked` and the dependency gate prove Worker no longer imports
  Delivery, unless an accepted ADR explicitly changes the boundary.
- The source inventory and dependency test match the selected ownership model.

### FUS-001: Model-authored strings become evidence

Priority: P0

Contract:

- `docs/decisions/0037-agent-fusion-engine.md:172` requires providers to return
  evidence with source references and reproducibility facts.
- `docs/decisions/0037-agent-fusion-engine.md:186` says models can explain
  evidence but cannot manufacture it.
- `docs/decisions/0036-multi-round-fusion-convergence.md:69` requires new
  evidence, reproduction, code path or counterexample.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_compose.rs:415` trusts a seat-owned
  `kind`, `detail` and `side` as `InvestigationEvidenceItem`.
- `fusion_compose.rs:659` treats every non-empty item as `produced_new_evidence`.
- The code then packages those strings as evidence for R4.

Impact:

- A model can manufacture the trigger required to advance rounds and influence
  adjudication without a command, test, diff, file or independent review fact.
- Consensus and self-asserted evidence can become an authority above tool facts.

Required fix:

Enforce decision Q-04. Require a machine-verifiable source reference and evidence
receipt for every evidence item. Store model explanation separately as a claim or
hypothesis that cannot satisfy `produced_new_evidence` by itself.

Acceptance:

- A seat returning only prose is recorded as unsupported and cannot advance R3.
- Evidence references resolve to a command, test, diff, file, run event or
  independent review.
- Removing or invalidating the source invalidates the derived claim support.

### FUS-002: Verified counter-evidence is not required before opposing conclusions

Priority: P0

Contract:

- `docs/decisions/0037-agent-fusion-engine.md:36` allows subtraction of a
  supported claim only through a verified contradiction.
- `docs/decisions/0037-agent-fusion-engine.md:128` makes verified
  counter-evidence a hard condition for `REFUTED`.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_compose.rs:660` accepts unverified
  R3 content.
- The R4 outcome at `fusion_compose.rs:721` can establish `ConfirmedOpposes` from
  that evidence pack without proving that a verified counter exists.

Impact:

- The implementation can suppress a supported minority or unique truth using
  unverified model material.
- The false-unique and 1v4 regression fixtures can pass while the host
  multi-round path still violates the same rule.

Required fix:

Keep `REFUTED` and opposing leads unavailable until the claim graph contains a
validated counter record marked verified by a tool, test, reproduction or
independent verifier.

Acceptance:

- A 4:1 model vote against one supported claim cannot remove or refute it.
- A model-authored counter without provenance cannot refute it.
- A verified counter can refute it and remains auditable.

### FUS-003: JEV failure is fail-closed

Priority: P0

Contract: `docs/decisions/0035-jev-strong-judge-verification-scheduler.md:162`
requires JEV failure to retain anchor and disputed claims and continue through a
verifier when possible.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_compose.rs:718` converts a judge
  failure into `FusionComposeError` and propagates it with `?`.

Impact:

- A temporary JEV outage discards the entire composed investigation instead of
  preserving useful independent claims.
- This directly violates the long-running runtime fail-open requirement.

Required fix:

Record an explicit `JEV_UNAVAILABLE` transition, retain all positions and
evidence, and dispatch available deterministic or independent verification. Do
not invent a leader while JEV is unavailable.

Acceptance:

- Injected JEV timeout, transport failure and invalid response retain both sides.
- The run reaches an auditable unresolved or verifier outcome rather than losing
  the panel result.

### FUS-004: JEV provisional is effectively terminal

Priority: P1

Contract: `docs/decisions/0035-jev-strong-judge-verification-scheduler.md:163`
allows verifier `confirmed` or `rejected` results to overturn provisional leading.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_analysis.rs:417` assigns
  `JevProvisional`.
- `fusion_analysis.rs:427` excludes provisional conflicts from the undecided set.
- `fusion_next_action` at `fusion_analysis.rs:349` finishes when that set is
  empty.

Impact:

- A lower-authority JEV opinion can become the last word before verifier evidence
  arrives.

Required fix:

Model provisional as a pending-verification state. Keep it in an actionable
`awaiting_verification` set and provide an explicit verifier transition to
confirmed or rejected.

Acceptance:

- A provisional answer cannot close the conflict by itself.
- A verifier can replace it in either direction and the original JEV reason stays
  auditable.

### FUS-005: No new evidence terminates instead of escalating

Priority: P1

Contract: `docs/decisions/0037-agent-fusion-engine.md:35` requires escalation,
not termination, when no new evidence is found.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_compose.rs:678` returns
  `Unresolvable` after one no-increment investigation.
- `crates/winwincode-control-plane/src/fusion_analysis.rs:375` encodes the same
  terminal behavior in the state machine.

Impact:

- A weak first provider or query plan permanently ends an otherwise resolvable
  dispute.

Required fix:

Advance the investigation ladder to a higher information-gain provider, tool,
test or reproduction. Formal tasks and the full batch have no token, call,
wall-time or monetary limit. End only on a valid terminal result, an unrecoverable
execution failure, or the sixth identical tool request, then retain `UNRESOLVED`
or `ATTENTION`.

Acceptance:

- Repeated prose-only responses escalate rather than terminate.
- A sixth identical tool request is intercepted before execution, terminates the
  local runner as `STUCK_TOOL_REPEAT_LIMIT`, is scored as unsuccessful and
  preserves disputed claims in the fixed denominator.

### FUS-006: JEV sees model identity

Priority: P1

Contract: `docs/decisions/0037-agent-fusion-engine.md:244` explicitly forbids
model names and vote counts in JEV input.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_compose.rs:495` includes `seat` or
  candidate identity while building the judge evidence pack.

Impact:

- The judge can apply brand, source or majority priors and no longer performs a
  blind evidence-only decision.

Required fix:

Use stable evidence IDs and anonymous claim positions only. Keep provenance in an
audit side channel not visible to JEV.

Acceptance:

- A serialized judge request contains no provider, seat, candidate or model name.
- Reordering or renaming seats does not change judge input semantics.

### FUS-007: ADR-0038 gate lacks real benchmark evidence

Priority: P1

Contract: `winwincode-community.5.16` requires Phase 1 Regret <= 1%, Minority >=
95%, Capture >= 95%, Constraint and Confirmed Recall = 100%, BindingError = 0,
StateRegression = 0 and Hallucinated = 0.

Actual behavior:

- `crates/winwincode-control-plane/src/fusion_bench.rs:473` validates metric and
  rebuild logic from hand-populated snapshots.
- No Phase 2-5 real benchmark result on the required A/B/C/D arms is bound to the
  current head.

Impact:

- Green unit tests cannot support the product gate or close `community.5.16`.

Required fix:

Resolve Q-06, freeze the dataset and baseline definition, then run the required
arms with reproducible per-task evidence and aggregate metrics.

Acceptance:

- Every aggregate can be recomputed from task-level inputs and outputs.
- Failed, stuck-tool-stopped, unrecoverable and missing-source runs cannot be
  counted as passes.
- The report binds source digest, both public repository revisions, frozen
  strategy/arm configuration, `max` reasoning effort, timestamps and complete
  usage records.

### DEL-001: Candidate pin regression remains red

Priority: P1

Actual behavior:

- `cargo test -p winwincode-control-plane --test session_binding_transaction
  --locked` reports 20 passed and 1 failed.
- `crates/winwincode-control-plane/tests/session_binding_transaction.rs:150`
  fails with `candidate pin exists`.

Impact:

- The exact candidate artifact and successful Worker outcome do not currently
  rebuild and pin the expected Delivery candidate on this head.

Required fix:

Diagnose the actual missing pin at the Control Plane transaction boundary. Do not
weaken or remove the assertion.

Acceptance:

- The test passes unchanged or is replaced with a stronger exact-binding test.
- Replay and duplicate acknowledgement retain exactly one candidate pin.

### RUN-001: Heartbeat can overwrite terminal state

Priority: P1

Actual behavior:

- `crates/winwincode-control-plane/src/heartbeat.rs:81` starts a background
  beater.
- `heartbeat.rs:117` writes `FINISHED` or `FAILED` and only then clears the
  running flag.
- Writes use unique staging files but no monotonic write guard or shared write
  lock. A background beat already past its running check can rename last and
  replace terminal state with `ALIVE idle`.

Impact:

- A supervisor can see a completed job as alive and later stalled.

Required fix:

Resolve Q-09. Serialize writes and require terminal status to be monotonic. A
write with a lower state or beat sequence must not replace a terminal record.

Acceptance:

- A deterministic race test interleaves `finish` with the background writer and
  keeps terminal status.
- Concurrent beats cannot regress phase, sequence or terminal status.

### BD-001: Beads task contracts have semantic drift

Priority: P1

Actual behavior:

- The workspace has 12 `in_progress` and 10 `open` issues.
- Five in-progress and three open issues have no structured acceptance criteria.
- `winwincode-ypka` places real acceptance criteria in its description while its
  structured acceptance field is empty.
- `winwincode-community.5.17` and `.5.18` correspond to landed code but remain
  open with no design or acceptance contract.
- `winwincode-community.5.1`, `.5.2` and `.5.4` remain in progress without an
  acceptance field.
- Snapshot Phase 1 is tracked by an untracked Markdown plan rather than one
  canonical Beads issue.

Impact:

- `bd ready`, status and issue closure cannot establish whether work is complete.
- Multiple overlapping tasks can claim or duplicate ownership of the same Fusion
  work.

Required fix:

Resolve Q-08, then normalize structured description/design/acceptance fields,
status and dependencies. Keep strict existing acceptance criteria in
`community.5.13` through `.5.16`; do not weaken them to match current code.

Acceptance:

- Every active implementation issue has an owner, a bounded design reference and
  testable acceptance criteria in structured fields.
- Each current defect maps to exactly one owner issue with dependencies.
- Closure notes bind commands, results and remaining risk.

### SRC-001: Branch files fail the source format gate

Priority: P2

Actual behavior:

- `corepack pnpm format:check` fails on trailing whitespace in ADR-0037 and
  ADR-0038, non-canonical JSON formatting in the Fusion regression fixtures, and
  unrelated local/format history.

Impact:

- `verify:source` cannot pass from the current tree.

Required fix:

Format files owned by this branch. Handle unrelated local artifacts and existing
schema formatting through a separate scoped issue so the integration fix does not
hide repository-wide debt.

Acceptance:

- A clean checkout passes `corepack pnpm format:check` for all tracked branch
  files, and the full command passes after the separate baseline debt is resolved.

### PROC-001: Feature scope is mixed

Priority: P2

Actual behavior:

- Commit `0c4df69c` clears unrelated workspace lint debt.
- Commit `464033cf` expands general agent documentation.
- These sit beside Fusion and Snapshot changes on one integration branch.

Impact:

- Review, rollback and issue evidence are less precise. The work conflicts with
  the instruction that old lint debt belongs in separate issues and fixes.

Required fix:

Split the branch according to Q-07 or document explicit independent issue and
commit ownership for every unrelated change.

Acceptance:

- Each commit and issue has one bounded reason for change and a matching test or
  documentation acceptance path.

## Beads Contract Audit

The Beads/Dolt workspace is operational; the defect is semantic contract drift,
not database corruption.

| Contract property | Current state | Verdict |
| --- | --- | --- |
| Readability | `bd where`, `bd list`, `bd show` work | Healthy |
| Structured acceptance | 8 of 22 open/in-progress items have no acceptance field | Drifted |
| Structured design | 10 of 22 open/in-progress items have no design field | Drifted |
| Status versus code | Some landed fixes remain open; some implemented components remain unverifiable | Drifted |
| Dependency semantics | Children and blockers are active concurrently without closure gates | Drifted |
| Evidence trace | Useful notes exist, but some acceptance lives only in descriptions | Drifted |
| Snapshot task ownership | No dedicated durable issue; Markdown plan is acting as a second task source | Missing |

Required repair order:

1. Decide the Snapshot ownership and canonical contract.
2. Create one durable Snapshot implementation issue and child defect issues.
3. Move acceptance text out of descriptions into structured acceptance fields.
4. Attach this review and the exact validation commands to the relevant issues.
5. Update status only after the issue's acceptance tests pass.

## Resolved Decisions

The product owner resolved Q-01 through Q-09 on 2026-09-24. The authoritative
contract is `docs/decisions/0039-integration-fusion-snapshot-remediation.md`.

| ID | Resolution | Direct defect impact |
| --- | --- | --- |
| Q-01 | Control Plane creates and persists Snapshot; Worker reports materialization and execution facts | SNAP-001, ARCH-001 |
| Q-02 | Use canonical `CandidateId`, canonical Snapshot ULID and immutable production Snapshot | SNAP-003 |
| Q-03 | Protect read-only input and isolate writable build/test scratch using Runner capability contracts | SNAP-002 |
| Q-04 | Separate model claims, source receipts and claim verification; require verified counters | FUS-001, FUS-002 |
| Q-05 | Keep JEV reversible and fail-open with recoverable unresolved/attention states | FUS-003 through FUS-006 |
| Q-06 | Freeze 20 public tasks x four arms x five strategies at `max` effort, plus five-strategy JEV ablations | FUS-007 |
| Q-07 | Keep the integration branch and scope new commits by responsibility | PROC-001 |
| Q-08 | Normalize only the active integration Beads contracts and bind status to evidence | BD-001 |
| Q-09 | Fix heartbeat terminal monotonicity and ordered writes now | RUN-001 |

Q-01 through Q-09 are closed as product decisions. Implementation may raise a
new decision only where an accepted contract would have to change.

## Execution Inputs Still Required

The model aliases, reasoning effort, task count/source, submission destination,
termination rule, budget, Runner and legacy-history policy are resolved in
ADR-0039. No product-owner input remains for those experiment conditions.

Resolved benchmark inputs:

- Exactly 20 tasks from `https://github.com/changw9813/agent-benchmark-tasks`
  at `fa9da301e493fb88d48c86cb8954ed46d9cd2ffe`.
- Submissions go to `https://github.com/changw98ic/agent-benchmark-submissions`;
  the task repository remains read-only to evaluated agents.
- Providers `glm5.1flash`, `mimov2.6pro`, `ds4.1flash` and `qwen3.8flash` each
  run once at reasoning effort `max`. `fusion(4)` is an independent four-model
  aggregation run with one call per member and one aggregation, not a provider or
  recursive Fusion stage.
- Every A/B/C/D cell contains four standalone provider runs plus one independent
  Fusion run: 320 + 80 = 400 main-matrix runs. The confirmed JEV ablations add
  240 + 60 = 300 runs. Fusion member and aggregation calls are metered separately.
- Neither a task nor the full batch has a token, call, wall-time or monetary upper
  limit. The sixth occurrence of an identical tool request is intercepted before
  execution and terminates the local runner as `STUCK_TOOL_REPEAT_LIMIT`; it
  remains in the denominator.
- The benchmark Runner is the local host `macOS 26.5.1
  aarch64-apple-darwin`; no separate Runner selection is required.
- Old benchmark history and legacy Snapshot facts are not retained in the new
  result set and are not migrated.

## Current Remediation Status

The following results apply to the current uncommitted worktree and are separate
from the immutable reviewed-head evidence below.

| Defect | Status | Evidence |
| --- | --- | --- |
| DEL-001 | Fixed in worktree | Exact Candidate and verification artifact pins pass 21/21 session-binding transactions, including restart/replay and stale-ref rejection |
| RUN-001 | Fixed in worktree | Terminal absorption, attempt generation, monotonic sequence, ordered finish and late-write rejection pass 5/5 focused tests |
| BD-001 | Normalized for integration scope | Every defect maps to one responsibility issue under `winwincode-community.7`; structured design/acceptance fields are present |
| SRC-001 | Branch-owned files fixed | ADR/fixture whitespace and JSON canonical formatting pass scoped checks; full format gate still has separately owned baseline debt |
| PROC-001 | Recorded | `docs/integration/2026-09-24-fusion-jev-snapshot-scope-ledger.md` maps all 12 mixed commits and rollback boundaries |
| SNAP-001/002/003, ARCH-001, FUS-001 through FUS-007 | Open | The production acceptance paths remain incomplete and their P0/P1 issues stay open |

Current remediation source digest over product code, ADRs, fixtures and the
benchmark README and scope ledger (excluding this report to avoid self-reference):
`46ecafb530a7a13c16e58a74c6d8e29d4b780e4f5f8dabc9b1319e9dd1c70048`.
It is the SHA-256 of the path-sorted SHA-256 records for the two Control Plane
source files, ADR-0037 through ADR-0039, the scope ledger, benchmark README and
all Fusion regression JSON fixtures.

## Validation Evidence

Passed on reviewed head `e5cc34e5`:

- `corepack pnpm contracts:check`
- `corepack pnpm lint:source`
- `corepack pnpm check:no-absolute-paths`
- `corepack pnpm typecheck`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- Fusion regression: 8 passed
- Control-plane Fusion units: 41 passed
- Fusion blind panel: 6 passed
- Snapshot binding: 4 passed
- Snapshot freezing: 4 passed
- Heartbeat units: 2 passed
- Outbox units: 22 passed
- Domain and ExecutionPort contract tests: 31 passed

Failed on reviewed head `e5cc34e5`:

- `cargo test -p winwincode-control-plane --test session_binding_transaction
  --locked`: 20 passed, 1 failed (`candidate pin exists`).
- `corepack pnpm format:check`: branch-owned ADR whitespace and fixture JSON
  formatting failures, plus unrelated local/baseline format noise.

Current worktree remediation checks:

- `cargo test -p winwincode-control-plane --test session_binding_transaction --locked`: 21 passed
- `cargo test -p winwincode-control-plane heartbeat::tests --lib --locked`: 5 passed
- Heartbeat-filtered integration suites: 3 passed
- `cargo clippy -p winwincode-control-plane --all-targets --all-features --locked -- -D warnings`: passed
- `rustfmt --check crates/winwincode-control-plane/src/heartbeat.rs`: passed
- Scoped branch-owned Markdown/JSON format validation: passed
- `git diff --check`: passed

Still failing or not run:

- Full `corepack pnpm verify`.
- Full `corepack pnpm format:check`: remaining failures are local `.playwright-cli` artifacts, one untracked benchmark-submission README and two pre-existing schema JSON files. These are assigned separately and are not counted as branch-owned format success.
- Real ADR-0038 Phase 2-5 benchmark.
- Four-platform release verification.

## Residual Risks Not Fully Tested

- Crash/replay behavior around the proposed two-phase Snapshot creation flow.
- Cross-platform enforcement of read-only source trees on macOS and Linux.
- Concurrent verification attempts against one Candidate and their fencing.
- Long-running JEV outage recovery and verifier scheduling after fail-open.
- Whether the generated canonical Snapshot DTO can round-trip every runtime value
  without loss after Q-01 and Q-02 are implemented.

## Exit Gate

The branch is ready for merge review only when:

1. Every P0 defect is fixed and has a focused regression test.
2. Decisions Q-01 through Q-09 are recorded in accepted ADRs or Beads decisions.
3. The Snapshot old path is removed and one production vertical proves the full
   Snapshot binding chain.
4. Fusion cannot advance or subtract claims without verified evidence provenance.
5. JEV failure is fail-open and verifier results can replace provisional leading.
6. `session_binding_transaction` is green without weakening assertions.
7. ADR-0038 reports are bound to a frozen real benchmark contract and current
   source digest.
8. Beads issue ownership and structured acceptance criteria are current.
9. `corepack pnpm verify` passes from a clean checkout.
