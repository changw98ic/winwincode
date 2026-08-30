# WinWinCode PostgreSQL

This crate owns the PostgreSQL migration plan and the adapter-neutral
transaction protocol behind the canonical `ProductStateStorage` port. The
offline protocol fixture proves migration, transaction, tenant, replay,
outbox, and backup semantics without presenting SQLite as PostgreSQL.

The external network driver and live PostgreSQL gate are tracked separately;
the protocol never stores or reports a DSN, password, TLS key, or access token.
