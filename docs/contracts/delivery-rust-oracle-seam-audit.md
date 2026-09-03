# Rust Delivery / StrongFlow oracle seam audit

## Audit boundary

This audit maps the ten frozen TypeScript transcripts in
`tests/fixtures/oracles/delivery-strongflow-typescript.v1.json` to the single
canonical Rust Delivery contract. The TypeScript commands, DTOs, snapshots,
error names, and store representation are migration input only. They are not a
second runtime contract.

The final audit was repeated after the integrated Rust differential runner
fixed in `4b1099d1db577bf523f3de65ddaeede10fa139ff`, and the canonical expected
oracle was regenerated in full from one fresh real ten-scenario execution of
that runner. The repository-local index entry
was absent, so this document claims Git inventory, `rg`, and direct file
coverage only. It does not claim a complete symbol or call graph.

## Final result

All ten scenarios execute through the typed Rust Control Plane, real SQLite
storage, production Delivery transitions, and sealed test-support facts. The
Node migration gate authors one closed v2 plan. Rust consumes that plan and
does not read the TypeScript oracle or derive migration facts independently.

The complete canonical result is committed at
`tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json`:

```text
bytes: 5,200,904
lines: 82,348
sha256: 496511c2c5910a7877a8ae4392e8cae4e119798481b7b7b4fcab3e18f3d5d5da
```

The final comparison is 10/10. There are no remaining P0 or P1 seam gaps and
no unresolved canonical result differences.

| Audited boundary | Final disposition |
| --- | --- |
| Base Delivery commands | `ControlPlane::commit_delivery_command` handles create, update, human advance, and Attention resolution with trusted repository, Spec, clock, and sealed transition facts. |
| Codex dispatch | `commit_delivery_execution` persists the Delivery advance and ExecutionJob intent atomically. |
| Session binding | `commit_delivery_session_binding` applies WorkerSession and CodexThread facts as two ordered, receipt-backed mutations. |
| Worker terminal outcome | `commit_delivery_terminal_outcome` verifies the generated `job.outcome`, durable job, lease, session, thread, time, sequence, and artifacts before one atomic terminal mutation. Every migrated terminal outcome carries immutable zero Usage because the legacy terminal fact has no provider Usage measurement; the typed seam requires Usage on a successful outcome. |
| Task promotion | `commit_delivery_task_breakdown` promotes only a sealed approved review graph and returns a validated durable receipt before current facts on replay. |
| Verdict | `commit_delivery_verdict` consumes sealed current-candidate verification facts; replay returns its validated durable receipt before current state or journal. |
| Rework | The precise rework authorization joins the sealed failed candidate, exact changed hunk, current verdict Evidence, and replacement candidate. |
| StrongFlow reads | `delivery.get` returns the bounded read cursor; `runtime.projection.get` uses that exact cursor and the last complete Codex binding from the typed Delivery projection. |
| Storage observation | The runner reads SQLite product state, aggregate journal, scoped receipts, outbox rows, and projection cursors. Corruption changes only saved journal bytes while the Control Plane is stopped. |
| Migration authority | Node creates the closed plan v2, including terminal outcome status per source command. Rust has no embedded-oracle or local status-derivation fallback. |

## Canonical contract authority

The runtime authorities are:

- `schema/winwincode/v1/control-plane-http.schema.json` for commands, queries,
  errors, and public Delivery projections;
- `schema/winwincode/v1/domain.schema.json` for shared identities, scope, and
  runtime projection;
- `schema/winwincode/v1/execution-port.schema.json` for execution messages;
- `crates/winwincode-api/src/generated.rs` for generated Rust DTOs;
- `crates/winwincode-delivery/src/domain/` for sealed Delivery facts;
- `crates/winwincode-delivery/src/store.rs` for journal commands;
- `crates/winwincode-control-plane/src/` for typed atomic transactions;
- `crates/winwincode-storage/src/lib.rs` for state, receipts, journal, outbox,
  and projection-cursor atomicity.

Unknown transport fields are rejected. Fixture authority is available only to
tests or the `test-support` feature. Normal builds expose no constructor for
raw trusted facts.

## Legacy operation migration

| Frozen TypeScript operation | Canonical operation | Migration rule |
| --- | --- | --- |
| `createDelivery` | `delivery.create` | Initial caller-authored tasks are removed. The payload contains the canonical Delivery ID and Spec; `expectedRevision` is 0. |
| `updateDeliverySpec` | `delivery.update_spec` | Public Spec input is joined with trusted repository, source, and ordered verification-method facts. |
| Codex `startStage` | `delivery.advance` plus the typed execution transaction | The server chooses the runnable task, stage, session, job, IDs, and clock. |
| Human `startStage` and successful human `bindSession` | One `delivery.advance` provenance group | Human stages have no Codex SessionBinding and add no binding revision. |
| Accepted Codex `bindSession` | One generated SessionBinding message | The message produces two ordered `session.bound` mutations: WorkerSession, then CodexThread. |
| Unknown Codex `bindSession` | Rejected generated message | The rejection is retained in the migrated trace and writes nothing. |
| Verifier terminal fact before `submitVerdict` | One generated `job.outcome` message | Each distinct terminal fact is committed once. Repeated verdict submissions reuse the accepted outcome. |
| `resolveAttention` | `delivery.resolve_attention` | The host supplies sealed review or remediation authority; the caller cannot author internal JSON. |
| `submitVerdict` | `delivery.submit_verdict` | Public input is only Delivery and candidate identity. Verification, Evidence, task state, Attention, and verdict come from sealed facts. |
| `getDeliveryProjection` | `delivery.get`, conditionally followed by `runtime.projection.get` | Runtime is queried only when the Delivery result contains a complete Codex binding. |

Task promotion is inserted after a plan review is approved and before the
first executing stage. `delivery.approve_task_breakdown` accepts only
`deliveryId` and `reviewSetSha256`; task content remains sealed server
authority.

## Public error migration

| Legacy branch | Canonical error |
| --- | --- |
| Duplicate create under a different request ID | `WRONG_STATE` |
| Same actor, full scope, request ID, but changed command bytes | `IDEMPOTENCY_CONFLICT` |
| Stale expected revision | `REVISION_CONFLICT` |
| Stale verdict candidate | `CANDIDATE_STALE` |
| Illegal transition | `WRONG_STATE` |
| Missing Delivery | `RESOURCE_NOT_FOUND` |
| Malformed payload or invalid sealed task graph | `INVALID_REQUEST` |
| Corrupt journal read | `SERVICE_UNAVAILABLE` |

Every failure uses the generated `ErrorEnvelope`. The runner compares the
complete code, message, details, retryability, and revision fields instead of
retaining old error aliases.

## Authority and fixture boundary

| Fact | Product authority | Differential fixture |
| --- | --- | --- |
| Actor, repository scope, request ID, expected revision, payload | Generated envelope plus authenticated transport | Fixed canonical User actor `usr_00000000000000000000000000`, scope, and request IDs; every human attention assignee is that same User |
| Clock | Trusted Control Plane clock/facts | Frozen scenario time |
| Repository and source | Trusted repository/source adapter | Local Git fixture and recorded source identity |
| Spec semantics | Trusted Spec authority bound to exact criterion IDs and order | Frozen scope, constraints, rework limit, and verification methods |
| Solution review | Private production resolver | High-level prepare/settle fixtures with semantic proposals |
| Task graph | Sealed approved solution review | Deterministic default proposal or explicit invalid graph fixture |
| Candidate and rework | Git/artifact resolver and precise rework policy | Frozen candidate, exact paths/hunks, and replacement fixture |
| Verification and verdict | Production verification and verdict resolvers | `verdict_facts_fixture` for Pass, Fail, Inconclusive, or InfraError |
| Session binding | ExecutionJob, lease, Worker, session, and thread authority | Generated binding message plus sealed lease facts |
| Terminal outcome | Generated outcome message joined to durable execution authority | Opaque terminal facts; raw lease/fence constructors remain private |
| Runtime projection | Trusted runtime ledger | Accepted semantic binding/event fixtures |
| Corruption | No production mutation API | Exact SQLite journal-byte backup, mutation, and restore while stopped |

## Deterministic task migration

An approved legacy review without proposals becomes one canonical task:

1. Hash the NUL-separated UTF-8 fields
   `winwincode.solution-review-default-task.v1`, `deliveryId`, `specId`, and
   decimal `specRevision` with SHA-256.
2. Encode the first 16 bytes as 26 Crockford Base32 characters and prefix
   `dtk_`.
3. Copy the current Spec title and goal.
4. Copy every criterion ID in Spec order and use an empty dependency list.
5. Recompute the complete review-set digest and bind the approval decision to
   that digest.
6. Promote through `delivery.approve_task_breakdown`; the task starts pending
   with no owner.

Legacy task IDs in the task-DAG seed are migrated separately with namespace
`winwincode.oracle-task-id-migration.v1`, preserving order and remapping every
dependency. The frozen vectors are:

```text
dtk_59X1F156B8YGG0P7G1K9KR5KB1
dtk_7HT0EYAWGG4MD098E2F2Z5XNTW
```

The legacy cyclic create branch is a sealed invalid solution-review proposal.
It fails before state, journal, receipt, or outbox mutation.

## Atomic storage and replay

Every successful typed mutation commits the canonical Delivery state,
aggregate journal record, scoped receipt, internal transaction event, public
`delivery.changed.v1` event, and any runtime invalidation in one SQLite
transaction. Publication happens only after that commit.

Receipt replay validates the durable identity, digest, revision, stream, and
event membership before returning. It does not depend on current state,
journal, replacement facts, or a second business transition. A publication
failure therefore returns the committed receipt and is recoverable after
restart without repeating product work.

The runner uses direct `SqliteStorage::commit` only for the explicitly named
strict seed fixture. Product commands do not use `FileDeliveryJournal`, direct
`DeliveryStore` mutations, hand-built public projections, or empty store
observations.

## Ten-scenario result

| Scenario | Final revision | Terminal outcome messages | Runtime projection | Canonical result |
| --- | ---: | ---: | --- | --- |
| `success-closed-loop` | 21 | 1 | present | Delivered; six stages, four complete Codex bindings, one promoted task, Pass verdict. |
| `request-id-replay` | 1 | 0 | absent | Exact create replay; one durable mutation and no duplicate publication. |
| `revision-conflict` | 2 | 0 | absent | Stale update returns `REVISION_CONFLICT`; durable facts stay unchanged. |
| `corruption-recovery` | 1 | 0 | absent | Corrupt read returns `SERVICE_UNAVAILABLE`; exact bytes restore the original projection. |
| `task-dag` | 2 | 0 | absent | The prerequisite starts first with a pending binding; isolated cyclic proposal writes nothing. |
| `candidate-invalidation` | 31 | 2 | present | Old digest returns `CANDIDATE_STALE`; corrected candidate reaches Pass. |
| `attention` | 8 | 0 | present | Approved plan is promoted; resolved Attention remains visible. |
| `inconclusive` | 19 | 1 | present | Canonical Inconclusive verdict opens verification-blocked Attention. |
| `infra-error` | 19 | 1 | present | Verifier `infrastructure_error` terminal outcome yields canonical InfraError without rewriting the succeeded Reviewer. |
| `rework` | 31 | 2 | present | Fail, precise rework, replacement candidate, and final Pass all use sealed facts. |

Approved solution-review projection remains visible through its explicit legal
successor states: Executing, Verifying, Reworking, NeedsAttention,
ReadyToDeliver, and Delivered. Earlier or incompatible states still fail
closed.

## Comparison and normalization

The closed result compares, in order:

1. every migrated command, request, response, error, and source provenance;
2. the complete ordered Delivery snapshot, tasks, stages, bindings, Attention,
   Evidence, candidates, and verdict;
3. aggregate journal manifest and records;
4. scoped receipts and every receipt event;
5. outbox rows, publication state, and projection cursors;
6. `delivery.get` and conditional `runtime.projection.get` responses.

Comparison reports the first exact RFC 6901 leaf path. Missing fields, extra
fields, array reordering, or changed values all fail. The plan contains no
expected response, assertion, store state, projection, or final observation.

Only exact runtime bindings may be normalized. In the committed result,
`<NODE_EXECUTABLE>` replaces 14 machine-specific executable path values.
`<ORACLE_ROOT>` and `<AUTH_PROOF>` appear zero times, and the random-identity
map is empty. No credential or host path remains in the expected fixture.

## Verification commands

```bash
corepack pnpm oracle:delivery:rust:check
node --test tests/delivery-strongflow-rust-differential-gate.test.mjs
cargo test -p winwincode-control-plane --features test-support \
  --test delivery_strongflow_differential_runner --locked
cargo test -p winwincode-control-plane --all-features --locked
cargo clippy -p winwincode-control-plane --all-targets --all-features \
  --locked -- -D warnings
```

The Node command is the sole full ten-scenario entry: it validates and writes
the v2 plan, supplies both runner paths, runs Cargo, normalizes the actual
result, and compares it with the committed expected value. Direct Cargo runs
without both plan-path environment variables run focused tests only; providing
just one path fails explicitly.

The final runner and both independent Standards and Spec reviews passed these
checks. Deliberately changing the infrastructure outcome produced the stable
first difference:

```text
/scenarios/8/commands/17/request/outcome/status
expected: infrastructure_error
actual: succeeded
```
