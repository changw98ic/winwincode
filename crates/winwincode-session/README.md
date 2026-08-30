# winwincode-session

ProductSession lifecycle state and exact execution-session identity bindings.

The crate is Control Plane code. It intentionally has no dependency on
Delivery, API DTOs, Execution Worker, Codex Core, or the Control Plane
composition crate.

Binding values are validated domain facts, not wire DTOs. The Control Plane
acceptance adapter joins a generated `session.binding` message to its one
scheduler-owned sealed lease and then creates the canonical binding values.
Lease authority is never created or restored in this crate.

Legacy Delivery snapshots enter through the single
`migration::migrate_legacy_delivery_json` conversion. The
`SqliteSessionIdentityMigration` adapter durably records its source marker,
canonical snapshot, and consumed marker in one transaction, so a restart reads
the first result instead of applying the old source again.

Project-owned code is licensed under Apache-2.0.
