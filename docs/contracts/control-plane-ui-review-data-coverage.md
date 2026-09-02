# Control Plane UI review data coverage audit

Status: UI-401 audit baseline plus the implemented UI-402 current-Candidate
files/diff, UI-402A Candidate-history, and UI-402B Evidence-detail/Artifact-
range slices for the StrongFlow review surfaces. The remaining baseline gaps
stay assigned to the UI-402 child tasks listed below.

## UI-402 implementation status

The current-Candidate and Candidate-history vertical slices are implemented
end to end:

- `candidate.files.list` returns one stable page of changed-file metadata from
  the exact retained base-to-Candidate Git range. Each item contains normalized
  `path`, optional `oldPath`, a closed status, text additions/deletions or
  explicit binary classification, and `utf-8`/`binary`/`unknown-8bit` content
  classification.
- `candidate.diff.get` returns at most 256 KiB of base64-encoded canonical Git
  diff bytes for one path selected from that trusted changed-file inventory.
  `offset`, `returnedBytes`, `totalBytes`, `nextOffset`, media type, encoding,
  file-diff digest, and Candidate binding are explicit.
- Both queries require the server-issued `StrongFlowReadCursor`, Delivery ID,
  Candidate ref/tree/diff digest, actor, and repository scope. The Control
  Plane replays the exact Delivery cut, reloads the current aggregate, rereads
  the candidate Artifact, and revalidates commit/tree/full-diff facts from the
  controlled Git source before returning data.
- `readPageLimit` names the `delivery.get` `page.limit` sealed into the read
  cursor. It is separate from the changed-file page size, so file pagination
  does not weaken or accidentally change the exact StrongFlow cut.
- File-page cursors are bound to actor, scope, exact read cursor, Candidate
  identity/digest, filters, and page limit. Paths are normalized
  repository-relative values and must exist in the exact trusted changed-file
  inventory. Repository locators, caller-supplied Git revisions, raw Artifact
  manifests, and diff bodies remain outside base/live/WebSocket projections.
- `candidate.list` replays the complete verified append-only Delivery journal
  through the requested read revision, rebuilds immutable Candidate identities
  from the original Delivery/Artifact/Git authority, and joins the exact durable
  Git-retention receipt. It returns first/last/review revisions, whether the
  Candidate is current at the read cursor, and closed `available`/`released`
  availability. Its stable cursor is bound to actor, scope, Delivery, exact read
  cursor, rebuilt history digest, read-page limit, and page limit.
- `candidate.review.get` selects only a Candidate returned by that history and
  requires its ref, tree, and diff digest. It returns the original Candidate-
  bound Evidence and Verdict plus the history revisions and availability, with
  constant `displayOnly: true` and `currentAuthorization: false`.
- Both history reads fail closed for a foreign scope/read cursor, stale or
  changed Candidate binding, broken journal, unsettled or missing retention
  receipt, moved repository, or tampered retained ref. Historical Evidence
  never participates in current authorization.
- `evidence.get` replays the exact Delivery cut, requires the Evidence's
  Candidate, StageRun, SessionBinding, type, and source reference as stale
  selectors, then rebuilds the accepted terminal/runtime authority before
  returning the closed outcome. Durable internal source seals are projected to
  one stable public `EvidenceId`; the internal seal is not exposed.
- `evidence.artifact.content.get` defines the bounded descriptor/range/chunk
  contract, with a 256 KiB request maximum, explicit continuation/truncation,
  base64 bytes, and `download_only` binary degradation. The current producer
  has no general Evidence-to-Artifact link, so the live query returns the
  closed `no_authoritative_link` result without looking up the caller's
  Artifact selector or confirming that it exists.
- Artifact storage now has metadata-only exact description and an exact range
  read. Local reads stream and hash the whole object while retaining only the
  requested range; object-store reads verify returned range, catalog size, and
  digest. Scope, full provenance, completion/deletion state, digest, size,
  range, and the 256 KiB limit are rechecked before bytes can be returned.

Candidate-to-Candidate file/diff comparison, general Evidence and Artifact
link production, Approval enrichment, and Publication detail remain in the
remaining UI-402 children. Preview/screenshot/browser logs,
structured test cases, general Evidence-to-Artifact linkage, and rich Approval
payloads remain producer gaps rather than synthesized read data.

## Audit method and limits

The repository-local index was fresh, but its effective coverage was
`file-inventory-only`. The conclusions below therefore come from direct reads
of the generated TypeScript and Rust contracts, their JSON Schemas, the Rust
projection/query implementations, the Git and Artifact authorities, and the
tests that assert the public-data boundary. No complete symbol or call-graph
coverage is claimed.

The relevant canonical sources are:

- `schema/winwincode/v1/control-plane-http.schema.json`,
  `schema/winwincode/v1/domain.schema.json`, and
  `schema/winwincode/v1/execution-port.schema.json`;
- `apps/client/src/generated/contracts.ts` and
  `apps/client/src/generated/control-plane-client.ts`;
- `crates/winwincode-api/src/generated.rs` and
  `crates/winwincode-execution-port/src/generated.rs`;
- `crates/winwincode-server/src/application.rs` for dispatched query names;
- `crates/winwincode-control-plane/src/strongflow_projection/`,
  `delivery_verdict_authority.rs`, `chat_interaction_projection.rs`, and the
  publication applications for the actual projection sources;
- `crates/winwincode-storage/src/git_source.rs`,
  `git_candidate_retention.rs`, and `artifact.rs` for reconstructable Git facts
  and authorized Artifact reads;
- `crates/winwincode-control-plane/tests/strongflow_projection.rs`,
  `chat_interaction_projection.rs`, and `publication_application.rs` for the
  public-data, restart, scope, and replay boundaries.

Status terms used in the matrices:

- **Present**: available through the generated browser client now.
- **Derivable**: a deterministic trusted source exists, but the value is not in
  a public projection or query. This is still a backend read gap for the UI.
- **Read gap**: canonical data is retained, but there is no authorized browser
  read model or suitable bounded-content seam.
- **Producer gap**: the required fact is not produced or durably linked. A new
  read query cannot reconstruct it.

## Existing query inventory

The generated `QueryName` union and Rust server dispatcher currently expose
only these relevant reads:

| Query | Result relevant to review UI | Exact useful coverage | Boundary |
| --- | --- | --- | --- |
| `delivery.get` | `DeliveryDetailProjection` | Current candidate identity, bounded Evidence references, Verdict/criterion results, current Publication summary, Delivery revision, and `StrongFlowReadCursor` | No candidate history, paths, hunks, content, Evidence outcome/content, or preview data |
| `runtime.projection.get` | `RuntimeProjectionSnapshot` | Exact paired read cursor, live sessions/agents, bounded runtime activities, and count-only diff summary | Live runtime support only; it is not the frozen-candidate review authority |
| `candidate.files.list` | `CandidateFilePage` | Exact current-Candidate changed paths, rename source, closed status, binary/encoding classification, and text additions/deletions with bound cursor pagination | Current Candidate and Spec base only; no Candidate history or two-Candidate comparison |
| `candidate.diff.get` | `CandidateDiffChunkProjection` | Exact current-Candidate per-file Git diff bytes, file digest, byte range, continuation offset, and binary classification | No canonical hunk/line-coordinate DTO and no historical or two-Candidate comparison |
| `candidate.list` | `CandidateHistoryPage` | Stable same-Delivery Candidate summaries from the verified journal, exact Candidate Artifact/Git facts, current-at-cursor marker, first/last/review revisions, and durable `available`/`released` retention state | Selects compare inputs; it does not expose file bodies or make released candidates readable |
| `candidate.review.get` | `CandidateHistoricalReviewProjection` | Original Candidate-bound Evidence/Verdict and exact history revisions for one Candidate selected from `candidate.list` | Explicitly display-only and never current authorization; no general Evidence/Artifact content |
| `evidence.get` | `EvidenceDetailProjection` | Exact Evidence reference plus closed outcome rebuilt from the accepted Delivery, terminal, Git Artifact, and runtime authorities | General Artifact descriptor is explicitly `unavailable/no_authoritative_link` until the producer persists an exact link |
| `evidence.artifact.content.get` | `EvidenceArtifactContentResult` | Exact Evidence binding and a bounded, typed Artifact range/chunk/download contract | Currently returns `unavailable/no_authoritative_link` without consulting Artifact storage; it does not confirm a caller-supplied Artifact ID |
| `approval.list`, `approval.get` | `ApprovalProjection` | Approval identity/state/time/subject plus exact chat/session binding | No category, safe action detail, risk, reason, or decision scope |
| `publication.list`, `publication.get` | `PublicationProjection` | Candidate/verdict/approval binding, target, current state, and closed secret-safe GitHub resource identity | No operation steps, state-transition history, retry receipt, or cancellation detail |

The generated StrongFlow paired-reload metadata still pairs only `delivery.get`
and `runtime.projection.get`; Candidate detail reads are explicit follow-up
queries bound to that exact cursor. Generated Candidate file, diff, history,
historical-review, Evidence detail, and Evidence Artifact-content queries now
exist. There is still no Preview query or produced general Evidence-to-Artifact
link.

## Review-panel coverage matrix

| Surface fact | Existing public field/query | Deterministic source or derivation | Exact gap | Sensitive-data boundary | Recommended owner |
| --- | --- | --- | --- | --- | --- |
| Current Candidate identity | **Present** in `delivery.get.currentCandidate`: `candidateRef`, Spec ID/revision, producer stage/session IDs, commit, tree, diff SHA-256, frozen time | Rebuilt from the exact successful writer terminal, candidate Artifact, controlled Git repository, and Delivery Spec | None for identity | IDs and digest are selectors, not authorization; every detail read must still bind repository scope and current Delivery cursor/revision | UI-403/404/409 frontend |
| Candidate history and availability | **Present** in `candidate.list`: exact Candidate identity, first/last/review Delivery revisions, current-at-read-cursor marker, and `available`/`released` | Rebuilt through the exact read revision from the verified append-only Delivery journal, terminal/Artifact/Git authority, and one exact durable Git-retention receipt | None for compare selection and explicit availability; released Candidates remain selectable only for display metadata/review | Reject foreign scope/cursor, stale bindings, unsettled/missing receipts, moved repository, and tampered refs; never accept caller repository locators or Git revisions | UI-405 frontend |
| Changed path names and present/deleted state | **Present for the current Candidate** in `candidate.files.list`; count-only live `RuntimeDiffSummaryProjection` remains non-authoritative | Rebuilt from the exact controlled base/candidate commit range after Artifact and Delivery-cut validation | Historical/two-Candidate file comparison remains; `candidate.list` now supplies the selector | Paths can reveal source layout; they stay out of base/live projections and are returned only on scoped detail reads | UI-403 frontend now; later UI-402/UI-405 for source comparison |
| Added/modified/deleted/renamed status, binary/encoding, additions/deletions | **Present for the current Candidate** in `candidate.files.list` | Controlled Git uses explicit stable rename/copy thresholds and exact `numstat` classification | Historical/two-Candidate source comparison remains | Facts are bound to commit/tree/diff digest and never accepted from the browser | UI-403/404 frontend now; later UI-402/UI-405 for source comparison |
| Unified or side-by-side diff | **Present as bounded canonical per-file Git diff bytes** in `candidate.diff.get` | Regenerated from the exact retained base/candidate commits after full-diff digest revalidation | A structured canonical hunk/line-coordinate DTO and historical/two-Candidate source comparison remain | Diff bytes are base64 chunks only; they never enter list/live/WebSocket projections, logs, telemetry, or error details | UI-404 may render the current diff; later UI-402/UI-405 for source comparison; hunk coordinates still gate exact UI-408 anchors |
| Base-to-Candidate compare | **Present for the current Candidate** through `candidate.files.list` and `candidate.diff.get` | Rebuilt from the Delivery Spec base revision and exact retained Candidate | Historical Candidate as comparison target remains | Scope, Delivery, Spec revision, candidate ref/tree, and diff digest must match | UI-405 frontend for the current Candidate; later UI-402 for historical source reads |
| Candidate-to-Candidate compare | `candidate.list` now supplies same-Delivery Candidate selectors and availability; no two-sided file/diff query | **Derivable only for two available retained candidates in the same Delivery** by reconstructing their trusted artifacts/Git pins | Add the two-sided file/diff compare contract; released Candidates remain review-metadata only | Reject foreign Delivery/Spec candidates, stale current cursor, moved/tampered refs, and caller-supplied commits/locators | Later UI-402 read model; UI-405 frontend |
| Candidate-to-Candidate Verdict/Evidence changes | **Present** per Candidate in `candidate.review.get`; current facts also remain in `delivery.get` | Rebuilt from the verified append-only Delivery journal and filtered to the original Candidate ref; rework can clear these facts from the current snapshot without erasing history | None for display comparison; general Evidence/Artifact content remains separate | Response constants enforce `displayOnly: true` and `currentAuthorization: false`; facts remain bound to original Candidate/Spec/stage/session | UI-405 frontend |
| Change reason and findings | Current `solutionReview.requestedChanges/comments`, Attention `resolutionSummary`, and Verdict `unresolvedFindings` are public | Findings and general review/Attention reasons are **Present**. There is no exact public Candidate-to-remediation-reason binding; `RemediationInput.instructions` is a command input, not a Candidate review projection | UI-409 may display the existing facts with their true labels. An exact “why this Candidate changed” row needs a trusted Candidate/rework-reason projection rather than a client-side guess | Keep private command context and protocol payloads out; expose only bounded human-authored public review summaries | UI-409 frontend for existing facts; Delivery/rework projection owner for an exact Candidate reason |
| Evidence identity/provenance | **Present** in `delivery.get.evidence`: ID, type, source ref, Candidate, Spec, stage, session, created time | Frozen from accepted candidate/runtime facts | None for the bounded reference | `sourceRef` is a reference, not an Artifact access grant | UI-406/409 frontend |
| Criterion result and Evidence join | **Present** in Verdict criteria: result ID, criterion ID, pass/fail/inconclusive/infra error, Evidence IDs, explanation, evaluation time | Deterministic Verdict projection | None for criterion-level display and client-side Evidence join | Explanations remain bounded public review text; do not substitute log bodies | UI-406/409 frontend |
| Per-Evidence test/command outcome | **Present** in `evidence.get` as observed/succeeded/failed/timed-out/policy-denied/infrastructure-failed/cancelled | Rebuilt from the exact accepted Delivery, terminal, Candidate source, and runtime ledger authorities at the supplied read cursor | `skip` and structured test-case results have no current canonical fact | The server rejects stale Candidate/StageRun/SessionBinding/type/source selectors and never infers outcome from free-form logs | UI-406 frontend; producer/verification contract for skip and test cases |
| Evidence/Artifact descriptor | **Present as a closed availability union** in `evidence.get`; current result is `unavailable/no_authoritative_link` | Some exact Artifacts are retained with fenced provenance, but the current producer does not persist a general Evidence-to-Artifact link | A trusted Evidence-to-Artifact descriptor link is still a producer gap for general logs/reports/test outputs | Artifact ID/digest never becomes bearer authorization; the unavailable path performs no Artifact lookup | Producer first, then the existing UI-402B available branch; UI-406 frontend |
| Evidence/Artifact content | **Present as a bounded contract** in `evidence.artifact.content.get`; live result is closed unavailable until a link exists. `ArtifactStore` now has exact describe/range authority | Exact range reads recheck scope, full provenance, completion/deletion, digest, size, and range; Local verifies the full object while buffering only the requested bytes | Producer must durably attach an exact Artifact reference to accepted Evidence before the available branch is reachable | 256 KiB maximum; base64 chunks, continuation/truncation, binary download degradation; no store keys, lease/fence, worker, credential, or paths | Producer link, then UI-406 frontend |
| Approval identity/state | **Present** in `approval.get/list` | Projection is restart-stable and exact-bound | None for a compact pending-card row | Subject is the deliberately public bounded summary | UI-503 frontend |
| Approval action category | Not public | **Derivable at ingress** from `ApprovalRequestMessage.action.category`; current Codex producer emits only `shell` or `filesystem_write` | Persist/project the closed category. Existing persisted public rows do not contain it | Closed enum only | ExecutionPort/Control Plane projection, then UI-402/503 |
| Approval shell/cwd/files/network/MCP/risk/reason | No public fields. ExecutionPort permits an arbitrary optional base64 `EncodedPayload`, but projection intentionally discards it | **Producer gap**: current Codex adapter sends `details: None` and generic summaries. Actual cwd, files, network/MCP detail, and risk are not safely produced or retained | Define a closed typed, pre-redacted display contract before projecting it. A read endpoint must not decode or forward arbitrary `EncodedPayload` | See the approval allowlist below; raw payload, environment, stdin, headers, query/userinfo, tool arguments, output, token, and credential material stay server-side | Codex/ExecutionPort/Control Plane ingress first; UI-402 only exposes the safe projection; UI-503 frontend |
| Approval scope (`once`/`worker_session`) | Not in Approval projection or `approval.decide` command input | ExecutionPort decision supports both values, but Control Plane currently emits `once` unconditionally | Read projection plus, if the user must choose it, an Approval command-contract change outside a read-only UI-402 patch | Closed enum; never derive authorization duration from display text | Approval command/domain follow-up; UI-402 can project current effective scope; UI-503 frontend |
| Preview URL and health | No schema, projection, producer, or query | None | **Producer gap**, not a read-only gap | A signed/capability URL is transient secret material: return short-lived scoped access with expiry, never persist/log its token | Preview lifecycle/Worker/Control Plane follow-up; then UI-402 and UI-407 |
| Screenshot/browser test | No preview/screenshot Artifact kind, Evidence type, or product projection | Test-run screenshots in repository test output are not product evidence | **Producer gap** for immutable screenshot descriptor/digest and structured browser-test Evidence | Image bytes and metadata use scoped Artifact access; strip unsafe metadata; cap dimensions/size | Browser-evidence producer + Artifact/Control Plane follow-up; then UI-402 and UI-407 |
| Console/network logs | No product schema or projection | None | **Producer gap** for typed, bounded, redacted browser evidence | Remove cookies, authorization headers, bodies, query/userinfo, local paths, and credentials before persistence | Browser-evidence producer + Artifact/Control Plane follow-up; then UI-402 and UI-407 |
| Publication summary/external ref | **Present** in Delivery detail and `publication.get/list`: binding, target, state, update time, and closed repo/number resource ref | Durable Publication projection | None for compact status and external reference | Resource ref is a closed identity, never arbitrary URL/provider response | UI-409 frontend |
| Publication operation steps/history/receipt/retry/cancel detail | Not public | Internal durable Publication state has four closed steps and a revision journal; current projection drops step/cancellation/transition detail | Secret-safe detail projection for `publication.get`; list remains compact. Retry action semantics need the existing command/domain authority rather than a UI-invented transition | Exclude provider request/response, credentials, idempotency keys, raw receipt digests, actor/request digests, and arbitrary remote URLs | Publication/Control Plane + UI-402; UI-409 frontend |

## Downstream UI work classification

| Bead | Can be implemented from current reads | Needs UI-402 read work | Needs work outside read-only UI-402 |
| --- | --- | --- | --- |
| UI-403 Changed Files | Current Candidate header plus `candidate.files.list` path/oldPath, status, additions/deletions, binary/encoding, exact identity, filters and stable cursor | Text-search is client-side within loaded pages; historical Candidate source reads remain | None for current base-to-Candidate view |
| UI-404 Diff Viewer | Current Candidate identity plus `candidate.diff.get` normalized path, binary state, exact digest, bounded bytes and continuation | Structured hunks/old-new line coordinates remain if the viewer or UI-408 anchors require a server-owned hunk DTO; historical/two-Candidate source reads remain | Candidate selection and historical review are now present |
| UI-405 Candidate Comparison | Candidate selector and review-result comparison from `candidate.list` plus `candidate.review.get`, including retained availability and original Evidence/Verdict | File/diff reads still need an optional same-Delivery base Candidate for source comparison | Released candidates remain display-only and are not a Git-read fallback |
| UI-406 Evidence Viewer | Evidence table, provenance, criterion join, Verdict status, exact outcome from `evidence.get`, and explicit Artifact unavailable state | The bounded Artifact contract and storage seam are ready; text/log/download becomes reachable after an exact producer link exists | Structured test cases/skip and general Evidence-to-Artifact link require producer contracts |
| UI-407 Preview | No real preview/screenshot data | The eventual detail needs scoped preview health/expiry/policy plus screenshot/browser-test descriptors and bounded redacted console/network log reads | Preview lifecycle, screenshot/browser Evidence producer, health/policy, and log redaction contracts |
| UI-408 Review Comments | Browser-local drafts can bind Delivery revision, current Candidate ref/diff digest, criterion IDs, and Evidence IDs. Submission can use existing `delivery.resolve_attention` with `RemediationInput` and `expectedRevision` | Diff response needs stable path/hunk/old/new line coordinates and a final Candidate/digest stale check | None for local drafts; do not create a comment domain entity |
| UI-409 Technical Details | Candidate identity, current Verdict/criteria/findings, Evidence references, general review/Attention summaries, Publication summary/external ref | Candidate/file detail, safe Evidence/Artifact detail, historical compare detail, and bounded Publication step/history detail | Exact Candidate change reason needs a rework projection; provider retry semantics remain Publication-domain owned |
| UI-503 Approval Detail | Approval identity/state/time/subject/binding | `approval.get` needs closed category/effective scope, followed by a typed safe-detail union when produced | Typed safe action detail producer; selectable worker-session scope requires an Approval command-contract change |

## Required public-data boundaries

Candidate review reads must take server-resolved repository scope plus Delivery
identity, current revision/read cursor, Candidate ref, tree and diff digest. A
Candidate-to-Candidate request adds a second Candidate ref/digest, but both
must resolve under the same Delivery. The server rejects stale, foreign,
released, moved, or caller-invented Git facts. File paths are normalized
repository-relative paths selected only from the trusted diff; path traversal,
absolute paths, NUL bytes, and unlisted paths are rejected.

Large lists and bodies are never returned as one unbounded projection:

- changed files use stable ordering and continuation paging;
- diff and file text use bounded hunks/chunks with explicit `truncated` and
  continuation/range metadata;
- Artifact/log reads recheck scope, exact digest, content size and media type
  on every request;
- binary or undecodable content returns classification and download metadata,
  not a lossy text conversion;
- base/live Delivery and WebSocket projections continue to exclude path, hunk,
  diff, log, and raw Artifact bodies.

Approval detail uses a closed discriminated union produced already redacted:

- shell: bounded display command plus normalized repository-relative working
  directory; no environment, stdin, shell history, or unredacted arguments;
- filesystem write: bounded normalized repository-relative path list and
  operation kind; no patch body;
- network: protocol and normalized host/port only; no URL userinfo, query,
  headers, cookies, request body, DNS response, or credentials;
- MCP: server/tool identity and an allowlisted summary only; no raw arguments
  or tool output;
- all: closed risk code, bounded public reason, effective decision scope, and
  expiry. Arbitrary `EncodedPayload`, base64 bytes, credentials, tokens, raw
  provider payloads, command output, and private decision reasons remain
  excluded.

## Recommended UI-402 contract scope

UI-402 should remain a read-model change and should not introduce new
Delivery, Task, Run, Review, Agent, Comment, Preview, or Approval entities.
The minimal canonical additions are:

1. `candidate.list`: **implemented** as retained Candidate summaries for one
   Delivery, including explicit `available`/`released` availability, history
   revisions, current-at-cursor marker, and the exact cursor at which the list
   was resolved.
2. `candidate.files.list`: **implemented for current base-to-Candidate** with
   stable paged file facts. Same-Delivery Candidate-to-Candidate comparison is
   assigned to `winwincode-zdd.1`.
3. `candidate.diff.get`: **implemented for current base-to-Candidate** as one
   bounded exact per-file Git-diff byte stream. A structured hunk/line model,
   if required for stable comment anchors, remains with the historical compare
   slice rather than becoming a second rendering-specific query.
4. `candidate.review.get`: **implemented** for one retained Candidate's
   historical Verdict and Evidence references reconstructed from the verified
   append-only Delivery journal. The response is always display-only and never
   current authorization.
5. `evidence.get`: **implemented** as safe typed detail for facts already
   authoritative. Outcome is rebuilt from accepted sources; Artifact access is
   explicitly unavailable when no exact link exists.
6. `evidence.artifact.content.get` and the exact Storage range seam:
   **implemented** with closed unavailable behavior until a producer link
   exists. The available contract binds scope, descriptor, digest, size,
   media-type, provenance, truncation/continuation, and binary degradation.
   Candidate diff content continues to come from the controlled Git resolver,
   not an Artifact manifest.
7. Expand `approval.get` only with closed category/effective-scope and typed
   sanitized detail that was produced safely at ingress. Do not expose
   `EncodedPayload`. Category/effective-scope projection can land in UI-402;
   missing action detail and selectable scope need their owning upstream work.
8. Make `publication.get` a true detail result with bounded closed step/state
   history and secret-safe cancellation/retry status, while keeping
   `publication.list` compact.

Every addition must update the canonical JSON Schema, generated Rust and
TypeScript types, generated browser client metadata, Rust query dispatcher and
projection sources, plus negative tests for foreign scope, stale Candidate,
changed digest, path traversal, over-limit page/range, binary/encoding
degradation, secret redaction, restart rebuild, and exact generated output.

Preview access, screenshot/browser Evidence, structured test-case/skip facts,
general Evidence-to-Artifact linkage, rich Approval action detail, and
selectable Approval duration are producer or command-contract work. UI-402 can
expose those facts after they exist, but a read-only implementation must not
invent them.
