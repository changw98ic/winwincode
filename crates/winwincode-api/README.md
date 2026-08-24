# winwincode-api

This internal crate exposes Rust transport types generated from the canonical
schemas under `schema/winwincode/v1`. Shared identifiers and scalar value
objects come directly from `winwincode-domain`; this crate does not define
transport-specific copies or aliases. Run `pnpm contracts:generate` after a
schema change and commit the resulting files together.

Project-owned code is licensed under Apache-2.0.
