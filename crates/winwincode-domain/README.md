# winwincode-domain

This internal crate owns the Rust definitions of shared identifiers and scalar
value objects generated from the canonical schemas under
`schema/winwincode/v1`. Other Rust crates reuse these types instead of defining
transport-specific copies.

Run `pnpm contracts:generate` after a schema change and commit the generated
files together.

Project-owned code is licensed under Apache-2.0.
