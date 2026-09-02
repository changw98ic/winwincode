# winwincode-storage

This crate is the Control Plane persistence seam. `ProductStateStorage::commit`
atomically writes one canonical state revision, an optional opaque aggregate
journal publication, the scoped command receipt, and its outbox events. The
local adapter uses SQLite with a bundled SQLite library; a later PostgreSQL
adapter implements this same port and transaction result.

Command receipts are keyed by the canonical actor identity, every ID present
in the organization/workspace/project/repository scope, and `requestId`.
Storage receives opaque typed keys and a SHA-256 command digest rather than the
command payload, credentials, or authentication proof. SQLite v1 receipt data
and v2 databases are migrated in one startup transaction to schema v3. Runtime
uses only the v3 composite receipt and aggregate-journal tables. An idempotent
replay returns the original event IDs and payload bytes from the durable outbox.

Aggregate journal values stay opaque: storage owns the static tables,
transaction, and tail compare-and-append; the Control Plane domain adapter owns
Delivery record decoding and digest-chain verification. A replay-only commit
fails closed when an older split write left a journal record without its scoped
receipt, so retry data cannot manufacture a replacement job event.

The outbox is delivered at least once in its persisted sequence order. Event
publishers therefore deduplicate by the stable `event_id`.

## SQLite dependency decision

`rusqlite` is pinned exactly to `0.39.0`. The repository's embedded Codex
dependency already selects `libsqlite3-sys 0.37.x` through `sqlx`; `rusqlite
0.40.x` requires `libsqlite3-sys 0.38.x`, and Cargo rejects the two native
libraries because both declare `links = "sqlite3"`.

`rusqlite 0.40.1` fixed SQL injection when an untrusted value is used as a
named SAVEPOINT. This adapter does not create named SAVEPOINTs or accept SQL
identifiers from callers. Table, column, index, and PRAGMA names are static
source literals, while every request id, stream id, revision, payload, event
id, and topic is passed as a bound SQL value. Tests reject dynamic SAVEPOINT
and formatted-SQL paths and exercise SQL-looking values through the public
storage interface. The pin should be upgraded after the Codex/sqlx dependency
closure moves to the same `libsqlite3-sys` generation. The upstream fix is
recorded in the [rusqlite 0.40.1 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.40.1).
