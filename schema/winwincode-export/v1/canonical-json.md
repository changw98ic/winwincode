# `winwincode-export/v1` canonical JSON

This file is the normative byte-encoding adjunct to
`winwincode-export.schema.json`. Implementations must apply these rules directly; behavior observed
from a particular JSON library is not part of the contract.

## Document encoding

- The document is the shortest valid UTF-8 encoding of Unicode scalar values, with no byte-order
  mark, leading or trailing bytes, insignificant whitespace, or trailing newline.
- The only JSON values in v1 are objects, arrays, and strings. Numbers, `true`, `false`, and `null`
  are not members of this format and fail the JSON Schema.
- Object names and values have no spaces around `:` or `,`. Object members appear in exactly these
  orders:
  - document: `format`, `exportId`, `contentSha256`, `content`;
  - digest material: `format`, `exportId`, `content`;
  - content: `profileDisplayName`, `organizations`, `projects`;
  - organization: `sourceOrganizationId`, `slug`, `displayName`;
  - project: `sourceProjectId`, `sourceOrganizationId`, `slug`, `displayName`.
- Organizations sort by `sourceOrganizationId`. Projects sort first by `sourceOrganizationId`,
  then by `sourceProjectId`. Each comparison is lexicographic by Unicode scalar value. Arrays use
  that sorted order and otherwise preserve every element; no other array reordering is permitted.

## String encoding

Each string starts and ends with `"`. Between them:

- U+0022 (`"`) is `\"`, and U+005C (`\`) is `\\`.
- U+0008, U+0009, U+000A, U+000C, and U+000D use `\b`, `\t`, `\n`, `\f`, and `\r`.
- Every other U+0000 through U+001F value uses lowercase `\u00xx` with exactly four hexadecimal
  digits.
- U+002F (`/`) is literal `/`; `\/` is not canonical.
- Every other Unicode scalar value is literal UTF-8. This includes U+00E9, U+2028, U+2029, and
  astral values. `\u00e9` and UTF-16 surrogate-pair escapes are not canonical spellings.
- Lone UTF-16 surrogates are not Unicode scalar values and are rejected.

`canonical-json-string.example.json.bytes` is the exact single-string conformance fixture. It
covers `/`, backslash, quote, U+00E9, an astral value, the five short control escapes, and lowercase
`\u00xx`. It exercises the encoder primitive; the schema still rejects control values in v1 data
fields. The fixture intentionally has no trailing newline.

## Digest and acceptance

`contentSha256` is lowercase SHA-256 of the canonical digest-material bytes. The complete document
uses the same rules. A decoder parses only after the 16 MiB original-byte gate, validates the JSON
value against the schema, independently regenerates the canonical document and digest-material
bytes, and accepts only an exact byte-for-byte match. `validate.js` implements these steps for
non-Rust consumers, and `conformance-vectors.json` holds the shared vectors both implementations
must accept and reject identically.
