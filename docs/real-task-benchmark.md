# Real-task benchmark gate

`scripts/evaluate-real-task-benchmark.mjs` is the **E13 production gate only**.
It accepts a dataset with at least 20 production tasks. Every row must bind an
exact source commit, configuration digest, WorkItem, WorkRun, result, and one or
more repository-relative evidence files with matching SHA-256 digests.

The gate requires simple, medium, and complex work; recovery, regression, and
collaboration cases; and both successful and unsuccessful outcomes. Missing
evidence, changed evidence, synthetic provenance, or incomplete coverage stops
the run. Fixture and replay datasets are rejected here.

Run it without modifying the evidence dataset:

```bash
node scripts/evaluate-real-task-benchmark.mjs \
  --input /path/to/real-task-benchmark.json \
  --repository-root /path/to/evidence-root \
  --output /new/path/benchmark-report.json
```

The output reports accepted task rate, human active minutes per task, recovery
success, false and missed attention, verification-failure escape, rework rate,
model/verification cost with unknown counts, and paired peer-collaboration
benefit. A zero denominator stays `null`; it is never reported as a zero rate.

## MiMo Device route

`scripts/run-device-task-vertical.mjs` provisions `mimo-v2.6-pro` with
`openai_responses` and the explicit private header
`x-openai-internal-codex-responses-lite: true`. This preserves native CodeMode
custom tools and their Lark grammar. The API key remains Device-local.

The MiMo profile also sets `responsesStructuredOutput: 'text'` in the
encrypted Device Provider configuration. For a structured result, the Device
Provider includes the complete original JSON Schema in the instructions and
omits the request's `text` field. The final response must parse as JSON and
pass strict local validation against the original schema. This mode accepts a
limited schema subset and rejects unknown keywords before sending a model
request. Native custom tools and grammar are preserved.

Set `XIAOMI_API_KEY`, `XIAOMI_MODEL=mimo-v2.6-pro`, and the complete HTTPS
endpoint in `XIAOMI_RESPONSES_URL`. For example, the official endpoints are
`https://api.xiaomimimo.com/v1/responses` and
`https://token-plan-cn.xiaomimimo.com/v1/responses`.

An existing `XIAOMI_BASE_URL` on either of those official origins is also
accepted. The script resolves it to the same origin's `/v1/responses`, including
when the old base contains `/anthropic`. `XIAOMI_RESPONSES_URL` takes precedence
when both variables are set. Proxy routes require `XIAOMI_RESPONSES_URL` so their
paths are explicit. URLs must omit embedded credentials, query parameters, and
fragments. Fusion provisioning uses the same MiMo route.

These endpoint and Lite requirements follow the provider's
[Codex configuration](https://mimo.mi.com/docs/en-US/tokenplan/integration/codex-configuration)
and [Responses API](https://mimo.mi.com/docs/en-US/api/chat/responses).

## Device task supervision and configuration requests

Each Delivery poll checks its durable supervisor owner and process identity.
The check reads the owner record without advancing the supervision journal.
Active WorkRuns still pass through their exact launch anchors, execution lease
checks and approval projections. An obsolete owner stops before the next poll.

Device Provider, extension and repository configuration relays bind a request ID
to the complete encrypted envelope. The HTTP executor freezes that envelope
before sending and retries transient failures with the same bytes and request ID.
Incomplete envelopes and unrelated mutations require reconciliation. Permanent
HTTP failures stop through the shared network policy.

Rejected Device configuration applies and Control Plane commands or queries
retain the HTTP status, safe server error code, phase, request ID and network
attempt history through the shared response-failure wrapper. Task reports retain
those safe facts. Response messages and configuration plaintext stay private.

## Device Provider credential admission

OpenCode configuration may contain [environment and file references](https://opencode.ai/docs/config/#env-vars)
such as `{env:NAME}` and `{file:/path/to/key}`. Batch preparation must resolve
these references from the explicitly selected account's credential source before
provisioning a Device. Account bindings must remain separate; the shared shell
environment must not silently replace another account's saved credential.

Device provisioning and task configuration share a local admission check.
Missing or non-string API keys fail with `PROVIDER_CREDENTIAL_MISSING`.
Unresolved environment or file references in API keys or custom header values
fail with `PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED`, including embedded
references. Both errors omit credential values and stop before Device requests,
encrypted saves, or paid model preflight. Valid opaque credentials retain their
original bytes. A saved token's local expiry does not prove upstream acceptance;
the actual native preflight supplies that evidence. Previous failed requests and
their billing and diagnostic records remain unchanged.

## Native Provider request identity

All native HTTPS protocols derive conversation headers from the original
kernel `ModelStreamRequest` envelope before translating its body. `session-id`
uses its `sessionId`, and `thread-id` uses its `threadId`. These are kernel
identities; a Worker session or a benchmark batch identifier is not substituted.
Concurrent conversations keep separate headers, and a physical retry keeps the
same headers without updating shared Device Provider configuration.

The default User-Agent is `WinWinCode/<version>`. On the official
`https://opencode.ai` origin (default port or port 443), `x-opencode-session`
also carries the stable `threadId`, as required by the provider's
[coding-agent integration](https://opencode.ai/docs/go/#where-can-i-use-it).
Explicit custom headers override each matching default case-insensitively;
the transport emits one value for each name. Identity values must be nonempty
printable ASCII tokens of at most 512 bytes. Invalid envelope identities are
rejected before the request is sent. Raw canonical payloads without an envelope
keep their body and do not acquire invented conversation identities.

## Native Provider failure diagnostics

External usage receipts allow additive metadata. OpenAI Chat, Responses, and
Anthropic adapters validate the counters they consume instead of requiring an
exact list of upstream usage fields. Optional audio, image, prediction, and
future metadata do not invalidate an otherwise complete response. Canonical
request and generated product contracts keep their strict validation.

OpenAI Chat maps `prompt_tokens_details.cache_write_tokens` into the existing
cache-write input counter. Cached reads and writes are subsets of input tokens,
so they are not added to the total again. Consumed counters must be unsigned
safe integers, agree with reported totals and DeepSeek cache counters, and fit
within their input or output totals. Missing cache-read usage remains unknown.
These details follow the [OpenAI SDK usage contract](https://github.com/openai/openai-python/blob/main/src/openai/types/completion_usage.py).

The same decoder supplies completed-stream and observed-attempt accounting.
Usage retained from a response that later fails does not establish successful
stream completion or a known monetary cost. Historical failed attempts are
preserved; an offline replay verifies a repaired decoder without rewriting
their paid ledger or issuing another upstream request.

Anthropic `ping` events are transport heartbeats. They do not create or advance
a message, and can appear before `message_start`, between message events, or
after a terminal event. Message ordering, JSON and event-type validation, and
frame limits still apply. A heartbeat cannot replace `message_start` or
`message_stop`. This follows the [Anthropic SDK stream handling](https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/_streaming.py).

Anthropic usage deltas replace the counters they provide and preserve omitted
counters. The adapter sums uncached input, cache reads, and cache writes into
the canonical input total. A later snapshot can revise that breakdown while
keeping or increasing the total. The total input and output counts must remain
nondecreasing; individual input categories need not. Completion, observed
accounting, and pricing use the same final breakdown. See the
[Anthropic cache accounting contract](https://platform.claude.com/docs/en/build-with-claude/prompt-caching#tracking-cache-performance)
and [SDK snapshot accumulation](https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/lib/streaming/_messages.py).

The native Provider transport retains non-2xx response bodies through the same
private log channel as failed SSE conversion. Public diagnostics contain the
original HTTP classification and the opaque `responseLog` / `responseLogStatus`
fields. Upstream text, credentials, and arbitrary URLs stay out of public errors.

The log directory has mode `0700`, and each generated log has mode `0600`.
Its first line is JSON metadata; the remaining bytes are the response body.
For non-2xx responses, `capture.complete`, `capture.truncated`,
`capture.limitBytes`, and `capture.readFailure` describe the retained bytes.
Capture uses the smaller of the configured response limit and 64 KiB, while the
original opening deadline and cancellation still apply. The opaque filename
uses the existing `sse-<digest>.log` namespace for both HTTP and SSE failures.

An interrupted read, truncation, or log-write failure does not change the HTTP
retry decision. Permanent failures remain permanent; transient responses retain
their finite request retry policy. A historical failure without a body reference
keeps its unknown cause, and operators must not replay it merely to obtain logs.

## JEV session replay is a different track

Baseline vs Deterministic GC vs GC+Jev comparison lives in
`scripts/evaluate-jev-session-replay.mjs` and
[`docs/jev-session-replay.md`](./jev-session-replay.md). That harness allows
clearly labeled fixture/replay datasets. Passing a session-replay kind into this
E13 evaluator fails closed and points at the JEV script. JEV replay results are
not E13 production evidence.
