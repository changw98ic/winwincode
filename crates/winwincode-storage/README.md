# winwincode-storage

This crate is the Control Plane persistence seam. `ProductStateStorage::commit`
atomically writes one canonical state revision and its outbox events. The local
adapter uses SQLite with a bundled SQLite library; a later PostgreSQL adapter
must implement the same small interface instead of changing application code.

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
