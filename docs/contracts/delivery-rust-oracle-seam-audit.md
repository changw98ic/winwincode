# Rust Delivery / StrongFlow oracle seam audit

## Audit boundary

This audit maps the ten frozen TypeScript transcripts in
`tests/fixtures/oracles/delivery-strongflow-typescript.v1.json` to the one
canonical Rust Delivery contract. It does not retain the TypeScript commands,
DTOs, snapshots, error names, or store representation as a second runtime
contract.

The file inventory was read at base commit
`dd17130afc377f2b7350d07288aafc6205be80e6`. The repository-local index command
referenced by the indexing skill was not present in this checkout, so the
coverage claim is limited to the Git file inventory, direct file reads, `jq`,
and `rg`. It is file-level coverage, not a complete symbol or call graph.

The audit also checked these isolated follow-up commits:

- `84c8c09622bea78394a0163eea27585c384ff3fb` makes verdict replay receipt-first.
- `4cbd5a182033cf1f82522f7f7eaa688de1749812` makes task-promotion replay
  receipt-first.
- `bc53b13b1e82c9aee62369a8e51f7e06f45d5886` exposes sealed, feature-only
  solution-review and verdict fixtures. This hash was read with `git rev-parse`
  in the isolated sealed-fixtures worktree.

## Result

The oracle can be migrated to Rust, but an exact comparison cannot mean
byte-for-byte equality with the old JSON. Several old operations are no longer
product operations, task promotion is now mandatory, and each accepted legacy
Codex `bindSession` becomes one `SessionBindingMessage` that is committed as
two ordered `session.bound` transactions. The migration gate must
first transform every old transcript into the current schema and then compare
the entire transformed result.

| Severity | Finding | Concrete consequence | Current disposition |
| --- | --- | --- | --- |
| P0 | At the audited base, `ControlPlane::commit` rejects every Delivery command and the typed host exposes only execution, rework, task-promotion, and verdict transactions. | `delivery.create`, `delivery.update_spec`, human `delivery.advance`, and `delivery.resolve_attention` cannot run through the product transaction boundary. | A separate `delivery_command_transaction` implementation is in progress and requires final review. |
| P0 | A transport `DeliverySpecInput` has no verification method, while the in-progress mapper currently writes `verification_method: None`. | Required criteria can only compute `Inconclusive`; the Pass and Fail scenarios are unreachable. | The command transaction must consume command-bound trusted Spec facts or another canonical product authority for verification methods. It must not invent a method. |
| P0 | The in-progress human-review command writes plain review text and stores the public `resolution` string directly. | It cannot produce the sealed solution-review context, digest, and decision required by `delivery.approve_task_breakdown`. | The production command path must construct the same private typed review facts used by the domain resolver; public callers must not author authority JSON. |
| P0 at base | Approved solution reviews, invalid task graphs, current-candidate verdict facts, `Inconclusive`, and `InfraError` had no cross-crate semantic builder. | The runner had to rewrite private JSON or could not reach four verdict branches. | Closed by `bc53b13b1e82c9aee62369a8e51f7e06f45d5886`; raw context and validated authority types remain private. |
| P1 at base | Verdict replay loaded the current journal before returning a stored receipt. | A valid retry failed if current state or journal facts were damaged or had moved on. | Closed by `84c8c09622bea78394a0163eea27585c384ff3fb`. |
| P1 at base | Task-promotion replay parsed payload, expected revision, and review digest before returning a stored receipt. | A valid retry depended on the current decoder even though the exact result was already durable. | Closed by `4cbd5a182033cf1f82522f7f7eaa688de1749812`. |
| P1 | The in-progress base-command replay still reinterprets `expectedRevision`. | Its replay ordering differs from verdict and task promotion. | Return from a self-validating receipt before parsing payload, revision, facts, state, or journal. |
| P1 | The old snapshots create a SessionBinding in one mutation. The canonical runtime turns each accepted Codex `bindSession` into one `SessionBindingMessage` and commits its worker-session and Codex-thread facts as two typed `session.bound` transactions. | Hydrating bindings at the stage revision would skip two product transactions and undercount revisions, receipts, journal records, and public events. | Preserve separate Codex `startStage` and `bindSession` source positions and commit both binding transactions. Fold a successful human bind into its human advance provenance. Emit and reject an unknown/invalid binding message with zero writes. |

## Canonical contract authority

The current transport authority is:

- `schema/winwincode/v1/control-plane-http.schema.json` for requests,
  responses, errors, queries, and public Delivery projections;
- `schema/winwincode/v1/domain.schema.json` for the shared command envelope,
  identities, scope, and runtime projection;
- `crates/winwincode-api/src/generated.rs` for generated Rust DTOs;
- `crates/winwincode-delivery/src/domain/` for the canonical Delivery facts;
- `crates/winwincode-delivery/src/store.rs` for journal commands and replay;
- `crates/winwincode-storage/src/lib.rs` for state, receipt, outbox, projection
  cursor, and aggregate-journal atomicity.

Unknown fields are rejected by the generated payloads. A canonical command has
this exact outer shape:

```json
{
  "schemaVersion": "winwincode/v1",
  "command": "delivery.create",
  "actor": { "kind": "user", "id": "usr_..." },
  "scope": {
    "kind": "repository",
    "organizationId": "org_...",
    "workspaceId": "wsp_...",
    "projectId": "prj_...",
    "repositoryId": "rep_..."
  },
  "requestId": "req_...",
  "expectedRevision": 0,
  "payload": {}
}
```

Every completed Delivery command returns only the generated completed envelope:

```json
{
  "schemaVersion": "winwincode/v1",
  "requestId": "req_...",
  "command": "delivery.create",
  "outcome": "completed",
  "previousRevision": 0,
  "currentRevision": 1,
  "result": {}
}
```

`result` is the generated `DeliveryProjection` for that command. Query results
use only this generated envelope:

```json
{
  "schemaVersion": "winwincode/v1",
  "requestId": "req_...",
  "query": "delivery.get",
  "result": {},
  "page": { "nextCursor": null, "hasMore": false }
}
```

The exact projection fields stay sourced from the schema definitions
`DeliveryProjection`, `DeliveryDetailProjection`, and
`RuntimeProjectionSnapshot`; the migration must not recreate the old combined
`diagramExecution` / `runtimeExecution` response.

## Legacy operation migration

| Frozen TypeScript operation | One canonical Rust operation | Rename or real behavior change |
| --- | --- | --- |
| `createDelivery` | `delivery.create`; payload is `{deliveryId,spec,tasks:[]}` and `expectedRevision` is `0`. | Contract migration. Caller-authored initial tasks are removed. |
| `updateDeliverySpec` | `delivery.update_spec`; payload is `{deliveryId,spec}`. | Contract migration. Repository and source facts remain trusted host authority. |
| Codex `startStage` | `delivery.advance`, which creates the ProductSession/ExecutionJob and pending binding authority. | Real behavior change. The server chooses the stage, role, task, IDs, clock, job, and pending binding authority. The source position remains distinct from the later bind. |
| Human `startStage` and its successful human `bindSession` | One `delivery.advance` entry carrying both source provenances. | Real behavior change. A human stage has no Codex SessionBinding and adds no binding revision. Plan review must carry a sealed solution-review context. |
| accepted Codex `bindSession` | One canonical `SessionBindingMessage`; its WorkerSession and CodexThread facts are applied by two ordered typed `session.bound` Control Plane transactions. | Real behavior change. It is not folded into `delivery.advance`, and it must not be replayed at the stage revision. The migration retains its own legacy source provenance. |
| unknown or invalid `bindSession` | One rejected canonical binding-message entry with zero writes. | The versioned migration preserves the failed source assertion but does not support the old DTO at runtime. |
| `resolveAttention` | `delivery.resolve_attention`; payload is `{deliveryId,attentionItemId,decision,resolution,remediation}`. | Contract migration. The host derives sealed review decisions and remediation authority; the caller does not supply raw internal facts. |
| `submitVerdict` | `delivery.submit_verdict`; payload is only `{deliveryId,candidateDigest}`. | Real authority change. Evidence, verification results, Attention, task state, and verdict are computed from sealed server facts. |
| `getDeliveryProjection` | `delivery.get`, then `runtime.projection.get` with the bounded cursor returned by the Delivery read. | Real read-model change. The old monolithic projection is removed. |

The six canonical Delivery mutation names are therefore:

```text
delivery.create
delivery.update_spec
delivery.approve_task_breakdown
delivery.advance
delivery.resolve_attention
delivery.submit_verdict
```

`delivery.approve_task_breakdown` is inserted after approval of the current plan
review and before the first executing stage. Its payload is only
`{deliveryId,reviewSetSha256}`. The caller cannot send task content.

## Public error migration

| Old oracle code or branch | Canonical code | Reason |
| --- | --- | --- |
| duplicate create with a different request ID and the same Delivery ID: `DELIVERY_CONFLICT` | `WRONG_STATE`, HTTP 409, `retryable:false` | The request shape is valid, but the create precondition “Delivery does not exist” is false. It is not an idempotency or revision conflict. |
| same actor, full scope, and request ID with changed command bytes | `IDEMPOTENCY_CONFLICT`, HTTP 409 | This is the only request-ID digest conflict. |
| stale `expectedRevision` | `REVISION_CONFLICT`, HTTP 409, with current revision | The command compared against a real current revision. |
| stale verdict candidate reported as `INVALID_REQUEST` | `CANDIDATE_STALE`, HTTP 409 | Candidate identity is a dedicated stale-check branch. |
| illegal stage or status: `WRONG_DELIVERY_STATE` | `WRONG_STATE`, HTTP 409 | The current Delivery cannot make that transition. |
| malformed payload, unknown field, invalid task graph | `INVALID_REQUEST`, HTTP 400 | The request or sealed proposed graph is invalid before mutation. |
| corrupted journal read: `STORE_FAILURE` | `SERVICE_UNAVAILABLE`, HTTP 503 | The storage integrity read failed; the old store code is not public. |

`IDEMPOTENCY_CONFLICT` must never represent duplicate create. The storage layer
already has `StorageErrorKind::JournalAlreadyExists`, and Delivery Store already
has `DeliveryStoreErrorCode::DeliveryAlreadyExists`. The command transaction
needs a typed `AlreadyExists` branch that maps to `WRONG_STATE`; it does not need
another storage error kind.

Every error uses the strict generated `ErrorEnvelope`; old response fields are
not copied beside it:

```json
{
  "schemaVersion": "winwincode/v1",
  "requestId": "req_...",
  "error": {
    "code": "WRONG_STATE",
    "message": "Delivery dlv_... already exists",
    "retryable": false,
    "details": { "field": "deliveryId", "deliveryId": "dlv_..." }
  }
}
```

The exact message is a versioned assertion once the transport mapper is merged;
the code, retryability, and duplicate-create details above are already fixed by
the typed command error.

## Authority and fixture boundary

| Fact | Product authority | Runner fixture authority | Forbidden shortcut |
| --- | --- | --- | --- |
| actor, full repository scope, request ID, expected revision, public payload | Generated command envelope plus authenticated transport | Deterministic canonical IDs and fixed actor/scope | Reusing the old schema-7 request |
| clock | Injected trusted Control Plane fact bound to the exact command digest | Scenario clock from the frozen transcript | `SystemTime` or deriving time from a request body |
| repository kind and locator | Trusted repository adapter, checked against repository scope | Local fixture `RepositoryRef` | Mapping `repositoryId` to a made-up local path |
| source issue/ref and Git base | Trusted source/repository adapter | Recorded local fixture source and Git object IDs | Treating the public repository ID as source authority |
| Spec completion, including verification methods | Trusted Spec/product authority bound to the command and criterion IDs | Deterministic criterion methods copied from the oracle’s reviewed Spec | Writing `None` or inventing one generic method |
| DeliverySpec, StageRun, job, Attention, binding, and event IDs | Control Plane/domain constructor | Deterministic high-level test-support builder | Caller-supplied raw IDs in a public payload |
| solution-review context, decision, and digest | Private solution-review resolver | `application::solution_review::test_support` semantic fixtures | Static private JSON or string replacement |
| candidate, verification, Evidence, and verdict facts | Private domain resolvers | `verdict_facts_fixture(current_delivery,current_candidate,outcome)` | Caller-authored raw Evidence or verdict |
| runtime events | Trusted runtime ledger adapter | `projection::runtime::test_support::{accepted_binding,accepted_event}` with semantic `RuntimeFactFixture` values | Copying old raw runtime events into canonical authority types |
| publication facts | Trusted publication adapter | Deterministic local publication fixture | Inferring approval from Delivery status alone |
| corruption | Storage adapter is normally opaque | After shutdown, save exact row bytes, mutate the chosen journal row, then restore exact bytes | A production “corrupt” API |

The public runtime seam already exists in
`crates/winwincode-delivery/src/projection/runtime.rs` under feature
`test-support`: `RuntimeAuthorityFixture`, `RuntimeFactFixture`,
`accepted_binding`, and `accepted_event`. It exposes semantic runtime facts,
not the private accepted-authority representation.

The sealed fixture commit exposes:

- `prepare_solution_review_fixture` and `settle_solution_review_fixture`;
- semantic valid and invalid task-proposal fixtures;
- `verdict_facts_fixture` for an existing Delivery and candidate;
- `VerdictFixtureOutcome::{Pass,Fail,Inconclusive,InfraError}`.

## Deterministic task migration

An old approved plan review has no task proposals. Passing an empty proposal
list to the sealed fixture derives exactly one task from the current Spec:

1. Join these UTF-8 strings with one NUL byte between fields:
   `winwincode.solution-review-default-task.v1`, `deliveryId`, `specId`, and the
   decimal `specRevision`.
2. Compute SHA-256, take the first 16 bytes as one big-endian `u128`, and encode
   it as exactly 26 Crockford Base32 characters with alphabet
   `0123456789ABCDEFGHJKMNPQRSTVWXYZ`.
3. Prefix the result with `dtk_`.
4. Set `title` to the current `DeliverySpec.title` and `goal` to the current
   `DeliverySpec.goal`.
5. Set `acceptanceCriterionIds` to every current Spec criterion ID in Spec
   order, including optional criteria. Set `blockedByTaskIds` to `[]`.
6. Put that task into the complete new solution-review v1 context and call the
   private `review_set_digest` over the full digest input. The review decision
   must bind that new digest. A digest over the task alone is invalid.
7. After the review is approved, call `delivery.approve_task_breakdown`; the
   promoted task has `owner:null` and status `pending`.

The old cyclic `createDelivery(tasks)` case is not retained. It becomes an
invalid sealed review proposal and must fail before journal, snapshot, receipt,
or outbox mutation.

## Storage, restart, and event chain

`SqliteStorage::open(root)` owns `root/control-plane.sqlite3`.
`ProductStateStorage` exposes state, scoped receipts, aggregate journals,
projection cursors, pending outbox events, and atomic commit. Delivery uses:

- `DeliverySnapshot::encode_json` / `Delivery::decode_json` for canonical state;
- `DeliveryStore` and `StagedDeliveryJournal` for the aggregate journal;
- one scoped receipt keyed by canonical actor, full scope, and request ID;
- transaction-owned internal events where the typed transaction defines one;
- one public `delivery.changed.v1` event with an authenticated projection cursor;
- outbox publication only after state, journal, receipt, and events commit.

Each migrated mutation and each of the two transactions produced from an
accepted `SessionBindingMessage` advances the Delivery revision and appends its
own journal/receipt/event chain. The old
journal digest and request digest cannot be copied because the command bytes and
mutation sequence changed. Rebuild the chain by executing the canonical
commands.

For corruption recovery, the fixture must close the Control Plane, copy the
exact selected `aggregate_journal_records.payload` bytes, change only that row,
assert `SERVICE_UNAVAILABLE`, restore the saved bytes, and reopen the same data
root. The recovered snapshot must equal the canonical pre-corruption snapshot.

For receipt replay, a hit is authoritative before current facts:

- same receipt identity and command digest;
- exact stored stream and revision;
- exact internal event set required by that transaction;
- exact `delivery.changed.v1` payload, event ID, and cursor;
- no new journal, state, receipt, outbox, publication, dispatch, or facts read.

## Ten-scenario migration table

The `canonical revision` column includes two additional `session.bound`
transactions for every accepted Codex `bindSession`. Every one follows its own
Codex `startStage` source entry; those two source entries are not collapsed.
A successful human bind is folded into the corresponding human advance
provenance and adds no revision. An unknown bind becomes a rejected canonical
message entry and adds no revision.
The runtime event observation must be rebuilt from accepted semantic facts; the
old raw event count is recorded only as migration input.

| Scenario | Canonical command transcript | Fixture / restart authority | Canonical snapshot and projection | Events, verdict, and store chain | Existing entry and remaining gap |
| --- | --- | --- | --- | --- | --- |
| `success-closed-loop` | `create`; duplicate create → `WRONG_STATE`; `update_spec`; stale update → `REVISION_CONFLICT`; illegal skip → `WRONG_STATE`; planning `advance`; emit and reject the unknown bind; accepted planning bind → two `session.bound`; human plan review with its successful human bind provenance folded in; `resolve_attention`; `approve_task_breakdown`; executor/reviewer/verifier advances, each followed at its original Codex bind source by two `session.bound`; `submit_verdict`; human delivery review with its successful bind provenance folded in; `resolve_attention`; bounded reads. | Deterministic command facts, sealed approved review, four accepted Codex bind messages, one rejected unknown bind, Pass facts, runtime/publication sources. No restart. | Canonical revision **20**: base mutation sequence 12 plus 8 binding-message revisions. Status `delivered`; six StageRuns; four SessionBindings; one derived task. `delivery.get` and runtime read share one cursor. | Old input: rev17, 21 raw runtime events, six bindings, four evidence, Pass, 17 records. New chain has 20 journal revisions/receipts for successful mutations; runtime facts are regenerated; verdict is Pass. The two successful human bind sources are carried by their human advances and add no record. | Execution, task promotion, verdict, query, and storage seams exist. Base commands need final transaction; verification-method and sealed human-review P0s remain in its current WIP. |
| `request-id-replay` | Execute the same canonical `delivery.create` twice with identical actor, full scope, request ID, revision, and body; then bounded reads. | Matching receipt fixture; runtime sources can be empty. No restart required. | Revision **1**, status `draft`, no tasks, stages, bindings, Attention, Evidence, or verdict. Second response equals the original result. | Exactly one successful create chain. Replay reads and validates the receipt first and emits nothing. | Storage `load_receipt` exists. The create transaction must use the same strict receipt-first ordering as task/verdict. |
| `revision-conflict` | `create` r1; `update_spec` r2; repeat the stale update with `expectedRevision:1`; bounded reads. | Deterministic clocks and Spec facts. No restart. | Revision **2**, status `ready`; rejected command changes no snapshot or projection fact. | Error is `REVISION_CONFLICT` with current revision 2. Two successful mutation chains only. | Delivery Store and Storage have typed revision conflict. Base update transaction is in progress. |
| `corruption-recovery` | `create`; close; corrupt one journal record; `delivery.get`; restore exact bytes; reopen same root; `delivery.get`; install sources; runtime read. | Storage fixture owns row-byte backup/mutation/restore and durable root. Restart recreates Control Plane, not the database. | Revision **1**, status `draft`; recovered Delivery projection exactly equals the pre-corruption canonical projection. | Corrupt read is `SERVICE_UNAVAILABLE`; restoration creates no command, receipt, event, or revision. Old `STORE_FAILURE` is removed. | SQLite path and lifecycle exist. Corruption remains test-only direct DB work. |
| `task-dag` | Seed a canonical revision-1 executing Delivery with ordered pending prerequisite/dependent tasks; `delivery.advance` selects the prerequisite and creates a pending binding. The old transcript has no later bind source, so no `SessionBindingMessage` is invented. Build a separate sealed cyclic proposal and prove promotion rejection; bounded reads. | `fixture.store.seed-snapshot` may seed only a strict current Rust snapshot. Invalid graph comes from the high-level sealed fixture. | Canonical revision **2** for the seeded Delivery. Task order is preserved; prerequisite is in progress and dependent stays pending. The one SessionBinding still has no WorkerSession or CodexThread. This intentionally differs from the old blocked direct call. | One seeded journal record plus the successful advance record. Cycle rejection adds no durable fact. | `runnable_task` already chooses the first runnable task; `delivery.advance` has no task ID. Sealed invalid fixtures are available at the follow-up tip. |
| `candidate-invalidation` | Follow the approved path through first Fail verdict; resolve verification-blocked Attention; remediator/reviewer/verifier advances; at every original accepted bind source emit one message and two `session.bound` transactions; submit the prior candidate digest → `CANDIDATE_STALE`; submit current digest → Pass; bounded reads. | Sealed Fail and Pass facts for the current Delivery/candidate, frozen-candidate replacement, seven accepted bind messages, runtime sources. | Canonical revision **29**: base mutation sequence 15 plus 14 binding-message revisions. Status `ready-to-deliver`; eight StageRuns; seven SessionBindings; one task; changed candidate digest. | Old input: rev22, 39 raw runtime events, eight evidence, 22 records. Stale command writes nothing; final verdict is Pass. Canonical successful store chain has 29 revisions. | Verdict transaction and current-candidate facts builder exist. Base/rework commands and binding transaction must be composed by the runner. |
| `attention` | `create`; `update_spec`; planning advance; at its accepted Codex bind source commit one message as two binding transactions; human plan review with its successful human bind provenance folded in; resolve its Attention; promote tasks; bounded reads. | Sealed pending/approved review, one accepted Codex bind message, empty runtime sources if no later activity. No restart. | Canonical revision **8**: base 6 plus two binding-message revisions. Status `executing`; two StageRuns; one SessionBinding; one resolved Attention; one pending task. | Old input: rev7, two bindings, one Attention, seven records. Canonical chain has eight revisions; the resolved Attention remains in the snapshot. | Solution-review fixture now covers the sealed facts. Production human advance/resolve must use the same private resolver. |
| `inconclusive` | Approved path through executor/reviewer/verifier; at each original accepted bind source commit one message as two binding transactions; submit current candidate with incomplete or missing verification settlements; bounded reads. | `verdict_facts_fixture(..., Inconclusive)`, current candidate, four accepted bind messages, runtime sources. | Canonical revision **18**: base 10 plus eight binding messages. Status `needs-attention`; five StageRuns; four SessionBindings; one task; resolved plan-review Attention plus open `verification_blocked`. | Old input: rev14, 21 raw events, three Evidence, 14 records. Canonical criterion/verdict status is `inconclusive`; no fake Fail evidence is added. | Outcome builder exists at the sealed-fixtures tip; verdict transaction exists. |
| `infra-error` | Same stage sequence as `inconclusive`; submit current candidate with a terminal infrastructure settlement; bounded reads. | `verdict_facts_fixture(..., InfraError)`, current candidate, four binding pairs, runtime sources. | Canonical revision **18**; status `needs-attention`; five StageRuns; four SessionBindings; one task; open `verification_blocked`. | Old input: rev14, 21 raw events, four Evidence, 14 records. Canonical verdict is `infra_error`; it is not represented as Fail. | Outcome builder maps to `InfrastructureFailed`; verdict computation already has the typed branch. |
| `rework` | Approved path; Fail verdict; resolve verification-blocked Attention; remediator/reviewer/verifier advances; at every original accepted bind source emit one message and two binding transactions; current candidate Pass verdict; bounded reads. | Sealed Fail then Pass facts, bounded rework authorization, seven accepted bind messages, runtime sources. | Canonical revision **29**: base 15 plus 14 binding-message revisions. Status `ready-to-deliver`; eight StageRuns; seven SessionBindings; one task; candidate changed. | Old input: rev22, 39 raw events, verdicts Fail then Pass, 22 records. Canonical successful store chain has 29 revisions. | Rework transaction, execution transaction, verdict transaction, and sealed facts exist; runner must compose them without raw authority. |

## Projection and observation assertions

For every scenario, comparison after migration must cover all of these facts:

1. strict canonical Delivery snapshot, including ordered tasks, StageRuns,
   SessionBindings, Attention, Evidence, verdict, status, and revision;
2. every committed receipt identity, revision, event payload, projection cursor,
   and replay flag;
3. aggregate journal manifest and ordered record chain generated by the new
   command sequence;
4. pending/published outbox state and no duplicate publication on receipt replay;
5. `delivery.get` result and its bounded StrongFlow cursor;
6. `runtime.projection.get` at that exact cursor;
7. frozen candidate and verdict identity where present;
8. exact canonical public error code, message, details, retryability, and current
   revision where the generated error schema carries it.

Do not assert the old revision, record count, SessionBinding count, error code,
or combined projection when this audit identifies a real behavior change.

## Verification evidence

The two receipt-ordering fixes were validated in the isolated audit worktree
with:

```text
cargo fmt --all -- --check
cargo test -p winwincode-control-plane
cargo clippy -p winwincode-control-plane --tests -- -D warnings
git diff --check
```

The task-promotion red test first failed because the transaction parsed an
unusable receipt-bound payload. After the fix it returned the original receipt
while current state and journal bytes were damaged, and it emitted no new
publication. The verdict replay test likewise returns the original receipt
before reading replacement facts or the damaged current state/journal.
