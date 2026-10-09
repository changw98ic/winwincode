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

## JEV session replay is a different track

Baseline vs Deterministic GC vs GC+Jev comparison lives in
`scripts/evaluate-jev-session-replay.mjs` and
[`docs/jev-session-replay.md`](./jev-session-replay.md). That harness allows
clearly labeled fixture/replay datasets. Passing a session-replay kind into this
E13 evaluator fails closed and points at the JEV script. JEV replay results are
not E13 production evidence.
