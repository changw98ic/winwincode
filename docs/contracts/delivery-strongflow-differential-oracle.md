# Delivery / StrongFlow differential oracle

## Purpose

This oracle freezes the observable behavior of the TypeScript
`StrongFlowDeliveryInvoker` before the Delivery owner moves to Rust. It is a
migration input, not a second application contract.

The committed oracle is:

```text
tests/fixtures/oracles/delivery-strongflow-typescript.v1.json
```

It contains ten deterministic scenarios:

```text
success-closed-loop
request-id-replay
revision-conflict
corruption-recovery
task-dag
candidate-invalidation
attention
inconclusive
infra-error
rework
```

Each scenario preserves the ordered command transcript and the final canonical
snapshot, runtime events, StrongFlow projections, DeliveryVerdict, and durable
store facts. Failed commands preserve the public error code, message, and
`currentRevision`.

## Command source interface

The Rust differential plan consumes `scenarios[].commands` in order after
removing every response, assertion, and final observation. The versioned
test-only migration maps each legacy command index to the current canonical
Rust command, query, or sealed fixture action. The Rust result contains only
current canonical DTOs and internal facts; it does not return the old
StrongFlow response shape.

`strongflow.request` sends `command.request` through the canonical Delivery
request interface. Fixture commands establish deterministic external state:

| Command | Meaning |
| --- | --- |
| `fixture.execution-source.replace` | Install the recorded runtime events and frozen candidate before projection. |
| `fixture.service.restart` | Recreate the service while keeping its durable home. |
| `fixture.store.seed-snapshot` | Seed the exact canonical snapshot required by a state-boundary scenario. |
| `fixture.store.corrupt-record` | Apply the declared controlled corruption to one durable record. |
| `fixture.store.restore-record` | Restore that record before reopening the service. |

The runner supplies three bindings when hydrating a transcript:

| Placeholder | Runner value |
| --- | --- |
| `<ORACLE_ROOT>` | An isolated disposable fixture root. |
| `<NODE_EXECUTABLE>` | The Node executable used by the deterministic verification fixture. |
| `<AUTH_PROOF>` | A fixture-only authentication proof that is never written to the oracle. |

The Rust runner writes raw values. The trigger-aware Node gate applies only the
declared host binding substitutions and compares the entire canonical result.
It must not reduce comparison to a status summary. In particular, error codes,
revisions, snapshots, events, projections, evidence, Attention, verdict,
receipts, outbox entries, cursors, and the rebuilt store chain remain exact.

The canonical mapping, runner protocol, and first-difference rule are frozen in
[`delivery-strongflow-rust-differential.md`](delivery-strongflow-rust-differential.md).

## Normalization boundary

The exporter removes only host-local facts:

- the disposable fixture root;
- the local Node executable path;
- explicitly supplied random fixture identities, if a scenario needs them;
- authentication proof values.

Scenario clocks, request IDs, revisions, error codes, Git object identities,
candidate identities, event order, snapshots, projections, evidence, and
verdicts are deterministic and remain unchanged.

## Commands

Regenerate the TypeScript baseline after an intentional behavior change:

```bash
corepack pnpm oracle:delivery:export
```

Verify that the committed baseline matches the current TypeScript behavior:

```bash
corepack pnpm oracle:delivery:check
```

The regular parallel Node test lane reads the committed ten-scenario baseline
and runs only the small `request-id-replay` live tracer. `test:ts` then invokes
the exporter with `--check` once, after the Node suite, so the full current
behavior comparison is isolated from the parallel test processes and reuses
the TypeScript build produced at the start of `test:ts`.

The trigger-aware Rust migration gate must pass before the TypeScript owner is
deleted:

```bash
corepack pnpm oracle:delivery:rust:check
```
