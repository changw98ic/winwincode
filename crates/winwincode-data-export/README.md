# WinWinCode data export contract

This database-neutral crate owns the canonical `winwincode-export/v1` document. It validates,
sorts, hashes, encodes, and decodes the complete one-time export while exposing no database
driver, connection, filesystem, credential, log, Codex, or Device Client type.

The JSON Schema is `schema/winwincode-export/v1/winwincode-export.schema.json`. Community's local
SQLite adapter creates this document; other products consume these bytes rather than defining a
second source format.

String limits count Unicode code points and reject leading or trailing ECMA-262 whitespace,
including U+FEFF. The complete compact UTF-8 document is limited to 16 MiB. Non-Rust consumers use
`schema/winwincode-export/v1/parse-bounded.js` to enforce that byte limit before schema validation.

`schema/winwincode-export/v1/canonical-json.md` defines the only byte encoding. It fixes UTF-8,
object-member and array order, and string escapes independently of `serde_json` or another JSON
library. The Rust writer and `schema/winwincode-export/v1/canonical-json.js` implement those rules
separately and must reproduce the published document, string conformance fixture, and digest bytes.
