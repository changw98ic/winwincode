# Rust Delivery / StrongFlow differential gate

## Result

This contract freezes the test-only boundary that compares the ten legacy
TypeScript scenarios with the single canonical Rust Delivery and Control Plane
implementation. It does not preserve a legacy product DTO.

The machine-readable contract is:

```text
docs/contracts/delivery-strongflow-rust-differential.rules.json
```

The source oracle remains:

```text
tests/fixtures/oracles/delivery-strongflow-typescript.v1.json
```

The Node gate is:

```bash
corepack pnpm oracle:delivery:rust:check
```

When neither Rust runner trigger exists, this command verifies the source
oracle hash and closed shape, all ten command dispositions, the stripped
execution plan, the versioned migration rules, and the exact comparator. When
either trigger exists, the same command runs the real Cargo integration target,
requires the typed canonical expected file, and compares the entire result.

## One-time canonical migration

The migration version is
`winwincode.delivery-strongflow-legacy-to-canonical.v1`. It is a separate,
test-only migration step. It is not part of normalization.

Canonical schema authority comes from:

- `schema/winwincode/v1/control-plane-http.schema.json` for the closed command,
  query, response, Delivery detail, and runtime projection unions;
- `schema/winwincode/v1/domain.schema.json` for actors, scopes, errors, shared
  envelopes, IDs, and public summaries;
- `crates/winwincode-api/src/generated.rs` for the generated Rust closed types;
- `crates/winwincode-delivery/src/domain/mod.rs` and
  `domain/session_binding.rs` for the strict internal Delivery snapshot;
- `crates/winwincode-delivery/src/store.rs` and
  `crates/winwincode-storage/src/lib.rs` for journal, state, receipt, outbox, and
  cursor facts.

Unknown legacy commands, errors, scenario fields, observations, projection
branches, runtime branches, or store branches reject migration. The migration
does not guess a canonical DTO from the legacy JSON. A typed Rust fixture
builder must construct the new values and decode or deserialize them through
the current closed types.

### Command dispositions

| Legacy input | Canonical disposition |
| --- | --- |
| `createDelivery` | `delivery.create`; `tasks` is always empty. |
| `task-dag` source 2 `createDelivery` | `fixture.solution-review.validate`; the second Spec drives the real high-level invalid-proposal resolver and the main store is unchanged. |
| `updateDeliverySpec` | `delivery.update_spec`. |
| Codex `startStage` | one `delivery.advance` that creates the pending ProductSession/ExecutionJob binding authority. |
| accepted Codex `bindSession` | one strict generated `SessionBindingMessage`, recorded as `execution-port.message`; its worker and Codex facts commit in two ordered revisions. |
| successful Human-review `bindSession` | provenance on its one `delivery.advance`; Human review has no execution binding and adds no revision. |
| rejected foreign `bindSession` | one strict `SessionBindingMessage` sent to the real typed seam; its canonical rejection writes nothing. |
| `resolveAttention` | `delivery.resolve_attention`. |
| terminal Worker fact before `submitVerdict` | one strict generated `JobOutcomeMessage`, recorded as `execution-port.message`; `apply_terminal_outcome` commits it as one independent revision before Verdict submission. |
| `submitVerdict` | `delivery.submit_verdict` with only the current candidate digest; sealed facts build Evidence and Verdict server-side. A repeated request carrying the same already-accepted terminal fact does not emit a second outcome. |
| `getDeliveryProjection` | Always `delivery.get`. Append `runtime.projection.get` only when the returned Delivery projection contains an eligible complete Codex binding. |
| `fixture.*` | a closed test-only fixture command; never a `CommandRequest`. |

The rules JSON lists every source command index and canonical group for every
scenario. A source command without a disposition is a contract error.

### Conditional runtime projection

After a completed `delivery.get`, the migration scans
`response.result.stages` in its canonical array order. A stage is eligible only
when `actorType` is `codex`, `sessionBinding` is present, and both
`workerSessionId` and `codexThreadId` are non-null. It selects the last eligible
stage. Exactly one `runtime.projection.get` then uses:

- `deliveryId` from `delivery.get.response.result.deliveryId`;
- `atCursor` from `delivery.get.response.result.readCursor`;
- `stageRunId` from the selected stage `id`; and
- `productSessionId` from the selected stage binding.

The runtime response must be the completed closed generated response, must
carry those same Delivery, StageRun, ProductSession, and read-cursor values,
and must contain exactly one runtime session. The observation's `runtime`
value is that complete response result.

A failed `delivery.get`, a Delivery with no binding, or a Delivery whose only
binding is still pending produces no runtime query. Its observation's
`runtime` value is exactly `null`. This applies to `request-id-replay`,
`revision-conflict`, `corruption-recovery`, and `task-dag`; the pending
task-DAG binding is not complete. The migration never invents fallback StageRun
or ProductSession identities.

### Mandatory task promotion

The legacy common flow enters execution with an empty task list. Canonical Rust
requires a non-empty approved task graph before execution, verification, or
rework. Therefore the migration inserts `delivery.approve_task_breakdown`
immediately after the plan-review Attention decision in these scenarios:

```text
success-closed-loop
candidate-invalidation
attention
inconclusive
infra-error
rework
```

The old v2 review context has no task proposals. The typed sealed fixture uses
the sole current default-task derivation. It joins the namespace
`winwincode.solution-review-default-task.v1`, Delivery ID, Spec ID, and decimal
Spec revision with NUL bytes; hashes those UTF-8 bytes with SHA-256; interprets
the first 16 digest bytes as a big-endian `u128`; encodes that value as exactly
26 Crockford Base32 characters with alphabet
`0123456789ABCDEFGHJKMNPQRSTVWXYZ`; and prefixes `dtk_`.

The proposal title and goal come from the current DeliverySpec; its criterion
IDs include every criterion in Spec order; dependencies are empty; its promoted
owner is `null` and status is `pending`. The task enters the complete current v1
review with deterministic `preparedAt`, then the private `review_set_digest`
computes the new digest. Task identity does not use the old review digest.
Missing review authority, deterministic time, a current `reviewSetSha256`, any
Spec criterion, or the deterministic proposal rejects migration.

The seeded `task-dag` snapshot uses a separate one-time task-ID migration. A
task ID already in canonical `dtk_` form is kept exactly. Every other ID hashes
the UTF-8 bytes of
`winwincode.oracle-task-id-migration.v1\0<deliveryId>\0<legacyTaskId>` with
SHA-256, converts the first 16 digest bytes as a big-endian `u128` to exactly
26 Crockford Base32 characters, and prefixes `dtk_`. The migration uses two
passes: first map every task ID, then map every dependency through that closed
table. Task order, title, goal, and criterion order stay exact; owner becomes
`null`. Thus `oracle-task-prerequisite` becomes
`dtk_59X1F156B8YGG0P7G1K9KR5KB1`, and `oracle-task-dependent` becomes
`dtk_7HT0EYAWGG4MD098E2F2Z5XNTW` with its dependency updated to the first ID.

The earlier canonical `delivery.advance` automatically selects that migrated
prerequisite task rather than reproducing the legacy blocked-task error. Source
command 2 does not create or promote another Delivery. It becomes the explicit
test-only command `fixture.solution-review.validate`, whose input is exactly the
second legacy DeliverySpec plus `invalidProposalKind:"dependency-cycle"`. The
fixture calls `.6.8` `invalid_task_proposals_fixture` and the production
`prepare_solution_review_fixture` resolver against an isolated canonical
planning handoff built from that source Spec. The resolver returns
`INVALID_REQUEST` before any Approved review seal exists, and the main scenario
store remains byte-for-byte unchanged: no revision, event, receipt, outbox
item, or journal record is written.

The inserted promotion changes revisions, events, task states, snapshots,
projections, receipts, outbox entries, cursors, and every later journal digest.
All of those values are rebuilt. Legacy digest values are never copied after a
snapshot changes.

The store observation is a closed test wrapper over current typed values:
`state` is exactly `{streamId,revision,snapshot}`; journal manifest and records
use the exact `DeliveryStoreManifest` and `DeliveryStoreRecord` JSON fields;
each receipt is exactly
`{actorKey,scopeKey,requestId,streamId,revision,idempotentReplay,events}`; and
each outbox item is exactly
`{sequence,eventId,topic,payload,projectionCursor,published}`. A durable receipt
records the original write with `idempotentReplay:false`; a replay flag belongs
to the command response and is not rewritten into the persisted receipt fact.

### Session and error migration

Legacy `dshSessionId` and `codexSessionId` are not renamed. A versioned identity
map constructs the strict generated `SessionBindingMessage`, including its
ProductSession, job lease, worker, WorkerSession, CodexThread, fence, clock, and
message identities. The real typed Control Plane seam performs
`accept_worker_session` and `report_codex_thread` as two consecutive
`session.bound` commits. Each commit adds one revision, journal record, receipt,
event, and outbox fact. Human review creates no message. The task-DAG transcript
has no bind source, so its advance leaves one pending binding and no message is
invented.

Each distinct final verifier terminal Worker fact is also migrated through the
generated `JobOutcomeMessage` branch. The message uses the exact current
ExecutionJob lease, WorkerSession, and CodexThread. Its finish time, last event
sequence, and summary come from the matching successful verifier
`turn.completed` fact; its artifact list is empty because the legacy fact has
no ExecutionPort `ArtifactReference`. The real terminal-outcome seam applies
`apply_terminal_outcome` as a separate durable commit and adds one revision.
The candidate-invalidation stale/current submit pair carries the same second
terminal fact, so that fact is committed once before the stale submit attempt
and reused by the following current-candidate submit. All later revisions,
events, receipts, outbox values, cursors, and journal digests are rebuilt from
this sequence.

The canonical error envelope is always the closed generated shape. A duplicate
create with a different request ID and the same Delivery ID maps to
`WRONG_STATE` (HTTP 409, non-retryable); `IDEMPOTENCY_CONFLICT` is reserved for
the same scoped request ID with a changed body. A corrupt journal read maps to
`SERVICE_UNAVAILABLE`. The message, retryability, details, and
`currentRevision` in the canonical expected file come from the typed public
mapper and compare exactly; they are not normalized.

## Runner process contract

Either of these files activates the gate:

```text
crates/winwincode-control-plane/tests/delivery_strongflow_differential_runner.rs
crates/winwincode-control-plane/tests/support/differential_runner.rs
```

Once activated, both are required and Node runs exactly:

```bash
cargo test -p winwincode-control-plane --features test-support --test delivery_strongflow_differential_runner
```

Node supplies two environment variables:

| Variable | Meaning |
| --- | --- |
| `WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT` | Path to a mode-0600 execution-plan JSON file. |
| `WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT` | Path where the Rust test must write its raw, unnormalized result JSON. |

The plan contains only `schemaVersion`, `oracleSchemaVersion`, runtime
bindings, and `scenarios[].{id,commands}`. A public request carries only its
legacy request input. A fixture command carries only its fixture input. The
plan contains no command response, assertion, final observation, final
journal, receipt, outbox, or final snapshot. The only state-seeding exception
is the declared `fixture.store.seed-snapshot.input.snapshot` command.

The canonical expected file becomes mandatory only after the Rust trigger:

```text
tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json
```

Its migration metadata is separate from `result`. The runner writes exactly
the `result` value:

```json
{
  "schemaVersion": "winwincode.delivery-strongflow-rust-differential-result.v1",
  "oracleSchemaVersion": "winwincode.delivery-strongflow-differential-oracle.v1",
  "scenarios": [
    {
      "id": "SCENARIO_ID",
      "commands": [
        {
          "sourceCommandIndexes": [0],
          "kind": "control-plane.command",
          "request": {},
          "response": {}
        }
      ],
      "observation": {
        "events": [],
        "projection": { "delivery": {}, "runtime": {} },
        "snapshot": {},
        "store": {
          "state": {},
          "journal": { "manifest": {}, "records": [], "snapshot": {} },
          "receipts": [],
          "outbox": []
        },
        "verdict": null
      }
    }
  ]
}
```

The typed builder fills the closed generated and internal values represented by
the empty objects above. They are not open extension points.

Public command requests, query requests, completed responses, query responses,
execution-port messages, and error envelopes use the exact generated key sets
listed in the machine rules. `delivery.create.tasks` must be exactly empty. The
runtime projection query uses the closed `delivery-stage` parameter branch and
the read cursor returned by `delivery.get`.

The test-only response around an execution-port message is closed. A successful
`SessionBindingMessage` contains exactly two ordered commit results, first
`accept_worker_session`, then `report_codex_thread`. A successful
`JobOutcomeMessage` contains exactly one `apply_terminal_outcome` commit. Every
commit has a consecutive revision and a closed receipt. Session-binding failure
contains the exact canonical error and unchanged current revision. This wrapper
records the typed seam result for comparison; it is not a product DTO.

The test-only fixture union is closed as well. A fixture request is exactly
`{kind,input}`. Successful responses are exactly `{outcome:"completed",result}`;
fixture results cover only runtime-source installation, service restart,
snapshot seeding, record corruption, and record restoration. The one
`fixture.solution-review.validate` branch has exact input keys
`{spec,invalidProposalKind}` and an exact rejected response carrying the
canonical `INVALID_REQUEST` error. Each fixture branch has the exact key set in
the machine rules, so fixture output cannot carry legacy response data.

## Ten exact scenarios

The order is part of the contract:

1. `success-closed-loop`
2. `request-id-replay`
3. `revision-conflict`
4. `corruption-recovery`
5. `task-dag`
6. `candidate-invalidation`
7. `attention`
8. `inconclusive`
9. `infra-error`
10. `rework`

For every scenario, the comparison includes all canonical requests and
responses, the final strict Delivery snapshot, accepted runtime event order,
the Delivery projection, the exact runtime projection or `null`, Verdict,
state, the complete journal digest chain, receipts, outbox events, and cursors.

The migrated checkpoints are also frozen as ordinary facts, not normalization:

| Scenario | Revision | Stage runs | Session bindings | Tasks |
| --- | ---: | ---: | ---: | ---: |
| `success-closed-loop` | 21 | 6 | 4 | 1 |
| `attention` | 8 | 2 | 1 | 1 |
| `inconclusive` | 19 | 5 | 4 | 1 |
| `infra-error` | 19 | 5 | 4 | 1 |
| `candidate-invalidation` | 31 | 8 | 7 | 1 |
| `rework` | 31 | 8 | 7 | 1 |
| `task-dag` | 2 | 1 | 1 | 2 |

The remaining exact final revisions are `request-id-replay` 1,
`revision-conflict` 2, and `corruption-recovery` 1. In `task-dag`, the seeded
revision is 1 and the one canonical advance produces revision 2; its pending
binding keeps both worker-session and Codex-thread identities null.

Legacy `assertions` are oracle self-check metadata. They never enter runner
input or output. The gate first verifies the exact legacy assertion object,
then evaluates the corresponding canonical assertion against the migrated
expected facts. This prevents the runner from satisfying an assertion by
copying the expected assertion value.

## Normalization boundary

Only these supplied runtime values may change:

| Binding | Placeholder | Match |
| --- | --- | --- |
| `ORACLE_ROOT` | `<ORACLE_ROOT>` | literal substring in strings |
| `NODE_EXECUTABLE` | `<NODE_EXECUTABLE>` | literal substring in strings |
| `AUTH_PROOF` | `<AUTH_PROOF>` | exact whole string |

The fixture-random-identity list is currently empty. Adding one requires a new
named entry, placeholder, and exact path policy in the machine rules. An
undeclared runtime identity is rejected.

The raw Rust output may not contain placeholders. Node substitutes only the
known raw binding values. It does not normalize time, request ID, revision,
error, message, details, event order or sequence, candidate, Evidence,
Verdict, snapshot, projection, task identity, cursor, receipt, outbox entry, or
journal digest.

## Complete comparison and difference path

Objects compare by their complete key/value set. Arrays compare in exact order.
The comparator recursively visits object keys in deterministic lexical order
and array indexes in ascending order. It stops at the first actionable leaf and
reports an RFC 6901 JSON Pointer, the exact expected value, and the exact actual
value.

For example, a changed revision is reported as:

```text
/scenarios/2/observation/snapshot/revision
```

The gate never reduces comparison to scenario status, test name, count, digest
summary, or selected fields. Its tests deliberately change one product fact and
prove that the triggered process path reports that exact first leaf.
