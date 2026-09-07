# `winwincode-export/v1`

This directory is the only JSON contract for moving public Community product data into another
WinWinCode product. The source-neutral envelope is:

1. `format`: exactly `winwincode-export/v1`;
2. `exportId`: the retry identity selected for this export;
3. `contentSha256`: lowercase SHA-256 of compact UTF-8 JSON containing `format`, `exportId`, then
   `content`;
4. `content`: `profileDisplayName`, organizations, and projects.

All `sourceId` and `displayName` limits count Unicode code points, not UTF-8 bytes or UTF-16 code
units. They reject Unicode control and surrogate code points and reject leading or trailing
ECMA-262 whitespace, including U+FEFF. This is the same boundary implemented by Rust and the JSON
Schema.

The entire compact UTF-8 document is limited to 16 MiB (16,777,216 bytes). JSON Schema cannot
express a serialized-byte limit, so non-Rust consumers must call
`parseBoundedWinWinCodeExportJson` from `parse-bounded.js` on the original bytes before applying the
Schema or importing records. The byte guard accepts exactly 16 MiB and rejects the next byte.

Organizations sort by `sourceOrganizationId`. Projects sort by
`(sourceOrganizationId, sourceProjectId)`, comparing Unicode scalar values lexicographically. The
digest includes that order. `canonical-json.md` is the normative byte-encoding adjunct: it fixes
UTF-8, every object-member order, array order, and every JSON string escape without relying on a
JSON library's output. V1 contains no numbers, booleans, or nulls. `canonical-json.js` is the
independent Node implementation. Decoding rejects a different order or escape spelling even when
it represents the same JSON value.

`winwincode-export.example.json.bytes` is the cross-product document fixture and
`canonical-json-string.example.json.bytes` covers every string escape class. Each entire file is a
canonical JSON value and intentionally has no trailing newline. Consumers must accept those exact
bytes and independently reproduce the document and digest-material bytes before importing records.

The format contains only public domain identity and display data. It has no field for credentials,
tokens, local absolute paths, logs, raw execution payloads, repository filesystem state, or the
private SQLite stores owned by Codex and Device Client. A destination product maps the source
profile into its own account or tenant configuration after decoding this document.
