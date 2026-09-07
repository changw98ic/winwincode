# `winwincode-export/v1`

This directory is the only JSON contract for moving public Community product data into another
WinWinCode product. The source-neutral envelope is:

1. `format`: exactly `winwincode-export/v1`;
2. `exportId`: the retry identity selected for this export;
3. `contentSha256`: lowercase SHA-256 of compact UTF-8 JSON containing `format`, `exportId`, then
   `content`;
4. `content`: `profileDisplayName`, organizations, and projects.

## The JSON Schema is only the structural layer

`winwincode-export.schema.json` describes field shapes: member names, types, string boundaries,
and record counts. JSON Schema cannot see across records, so it also accepts documents the
contract rejects: repeated organization or project identifiers, a project referencing an
organization outside this document, arrays in another order, a well-formed but wrong
`contentSha256`, and another byte spelling of the same JSON value. A consumer that stops at the
schema reaches different conclusions than the Rust contract owner. The schema stays published for
documentation and editor tooling; it is not a validation path.

## One validation path

Non-Rust consumers validate one complete document with `validateWinWinCodeExportBytes(bytes)` from
`validate.js`. It returns `{status: "accepted", document, contentSha256}` or
`{status: "rejected", category, message}` without throwing, and its categories match the Rust
`WinWinCodeExportError` variants one for one. `assertWinWinCodeExportBytes(bytes)` throws a
`SyntaxError` carrying that category instead. `validate.js` needs only Node itself.

The gate applies, in order:

1. the 16 MiB limit on the original bytes before decoding (`too-large`);
2. UTF-8 and JSON decoding plus the structural schema rules, including the rejection of lone UTF-16
   surrogates (`invalid-json`);
3. the format identifier (`unsupported-format`), the export identifier (`invalid-export-id`), and
   then record text, slugs, identifiers, and counts (`invalid-content`,
   `local-path-not-allowed`), record uniqueness (`duplicate-source-identifier`), and organization
   references (`unknown-source-organization`);
4. canonical record order (`non-canonical`), the recomputed `contentSha256` (`digest-mismatch`),
   and finally the exact canonical byte spelling of the whole document (`non-canonical`).

That order is part of the contract: a document with several faults is rejected by the first rule
it breaks, in both implementations.

## Conformance vectors

`conformance-vectors.json` publishes the shared positive and negative set. Each vector holds one
complete document with its expected verdict and, for a rejection, the expected category. The Rust
runner `crates/winwincode-data-export/tests/conformance_vectors.rs` and the non-Rust runner
`tests/data-export-semantic-conformance.test.mjs` execute that one file and must agree on every
vector, so a contract change cannot land in one implementation alone.
`tests/fixtures/data-export-conformance-vectors.mjs` regenerates the file from the canonical
encoder.

All `sourceId` and `displayName` limits count Unicode code points, not UTF-8 bytes or UTF-16 code
units. They reject Unicode control and surrogate code points and reject leading or trailing
ECMA-262 whitespace, including U+FEFF. This is the same boundary implemented by Rust, the JSON
Schema, and `validate.js`.

The entire compact UTF-8 document is limited to 16 MiB (16,777,216 bytes). JSON Schema cannot
express a serialized-byte limit, so the strict gate measures the original bytes and rejects the
next byte above exactly 16 MiB before decoding.

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
bytes and independently reproduce the document and digest-material bytes before importing records;
`conformance-vectors.json` pins the document fixture as one of its vectors.

The format contains only public domain identity and display data. It has no field for credentials,
tokens, local absolute paths, logs, raw execution payloads, repository filesystem state, or the
private SQLite stores owned by Codex and Device Client. A destination product maps the source
profile into its own account or tenant configuration after decoding this document.
