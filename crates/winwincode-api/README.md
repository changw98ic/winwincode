# winwincode-api

This internal crate exposes the Rust HTTP/WebSocket transport types generated
from the canonical Control Plane schemas under `schema/winwincode/v1`.
Shared identifiers and value objects come directly from the
`winwincode-domain` crate root. ExecutionPort wire DTOs have their own
`winwincode-execution-port` crate; this crate does not re-export, alias, or
duplicate them. Run `pnpm contracts:generate` after a schema change and
commit the resulting files together.

Project-owned code is licensed under Apache-2.0.
