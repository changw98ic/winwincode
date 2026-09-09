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

### Offline WorkRun migration

Stop source writers and export a consistent canonical Delivery snapshot first.
Run the offline tool; it neither starts workers nor changes the original input:

```bash
cargo run -p winwincode-delivery --bin migrate_workrun --locked -- \
  --input=/path/to/delivery.json --db=/path/to/migration.sqlite \
  --output=/path/to/workrun.json --backup-dir=/path/to/input-backups
```

The command creates the input backup with a SHA-256 sidecar using create-new
semantics, records the conversion atomically in SQLite, and refuses to replace
an existing backup or output whose bytes differ. Re-running the same input
returns the stored receipt snapshot. To restore into a new path, verify the sidecar first, then copy without replacing
anything:

```bash
shasum -a 256 -c /path/to/input-backups/delivery.json.sha256
cp -n /path/to/input-backups/delivery.json /path/to/restored/delivery.json
```

Keep the original input and migration receipt for audit. The restored file is a
new offline snapshot, not a second running system. Production scheduling cutover
is a separate operation described in ADR-0033; old leases are never reactivated.
