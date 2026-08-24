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

Project-owned code is licensed under Apache-2.0.
