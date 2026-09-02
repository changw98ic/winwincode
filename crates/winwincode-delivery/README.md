# winwincode-delivery

This internal crate owns the canonical ten-object Delivery aggregate and its
append-only record journal.

The Control Plane calls two small interfaces:

- `DeliveryCommandPort::execute` validates and publishes create/append commands.
- `DeliveryQueryPort::query` reconstructs one fully verified Delivery.

Database crates implement `DeliveryJournalPort`. The adapter stores opaque
manifest and record bytes, atomically creates a journal, and atomically compares
the expected tail before appending. Domain validation, request replay,
`expectedRevision`, record digests, recovery, and corruption rejection remain in
this crate.

Phase 2.1 connects its transaction through `DeliveryStore::borrowed`. The
transaction stages `AtomicPublication` and the matching outbox event, then makes
both authoritative in one outer `ProductStateStorage` commit. A long-lived local
module can use `DeliveryStore::new` with a shared adapter.

`InMemoryDeliveryJournal` is deterministic test infrastructure. It is not the
local or enterprise persistence choice.

The `application` module owns the narrow stage-coordination commands. It picks
the only legal next stage, permits one active `StageRun`, requires an exact
lease-fenced terminal Worker outcome for handoff, approves the current reviewed
task graph once, blocks on current Attention items, and returns immutable
effects for the Control Plane to persist. It never dispatches an `ExecutionJob`
or copies Codex plan, agent, tool, or scheduler state.

`SessionBinding` has one canonical shape: Delivery/task/StageRun plus
`ProductSessionId`, `ExecutionJobId`, optional `WorkerSessionId`, and optional
`CodexThreadId`. The old DSH/Codex pair exists only in the frozen TypeScript
oracle and is normalized once inside test migration support.

Project-owned code is licensed under Apache-2.0.
