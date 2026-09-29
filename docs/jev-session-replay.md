# JEV-04 Phase1 session replay

`scripts/evaluate-jev-session-replay.mjs` is the Phase1 comparison harness for
Baseline vs Deterministic GC vs GC+Jev. It is **not** the E13 production gate.
E13 stays in `scripts/evaluate-real-task-benchmark.mjs` and still accepts only
production evidence.

## Why this harness is separate

The E13 real-task gate proves product delivery quality on production WorkRuns.
JEV-04 asks a different question: does OpenJev KEEP/DROP on real coding-agent
context hold Critical Recall >= 99% and Task Success >= baseline before the
project expands Jev Runtime? Fixture and replay datasets are allowed here when
they are clearly labeled. They must never be rewritten as E13 production claims.

## Dataset contract

```json
{
  "schemaVersion": 1,
  "kind": "winwincode.jev-session-replay.v1",
  "track": "jev-session-replay",
  "phase": "phase1",
  "evidenceClass": "fixture",
  "datasetLabel": "jev-phase1-fixture-skeleton-not-production",
  "missingRealData": ["..."],
  "tasks": []
}
```

- `evidenceClass` is required: `production`, `fixture`, or `replay`.
- `datasetLabel` is required. Non-production labels must not contain production
  or E13 wording.
- Each available run carries `provenance` equal to the dataset `evidenceClass`.
- Fixture/replay runs still bind configuration digests and evidence SHA-256 files.
- Production runs additionally require exact source commits and canonical
  WorkItem/WorkRun ids.
- A run that cannot execute is `status: "unavailable"` with a code and detail.
  It never attaches measurements.

## Phase1 modes

Default selected modes:

1. `baseline` — no deterministic GC, no Jev
2. `deterministic-gc` — BD-03 deterministic tool-context GC only
3. `gc+jev` — deterministic GC followed by Jev decisions (BD-01/BD-02)

Later arms (`jev+memory`, `jev+rotation`) are accepted only when explicitly
selected and stay frozen until Phase1 passes.

## Comparative report skeleton

The report kind is `winwincode.jev-session-replay-report.v1`. It always includes:

- `evidenceClass`, `productionClaims`, `separateFromE13ProductionGate`
- per-mode metric totals with `total` / `measuredTotal` / `knownRuns` / `unknownRuns`
- `comparison.arms` deltas: billed input, cache hit rate, repeated tools,
  task success, verification, critical recall, latency, provider cost
- `phase1Gate.hardGate`: Critical Recall >= 99% and Task Success >= baseline
- `phase1Gate.expandedGate`: billed input -15%, repeated tool -15%, Jev cost
  share <= 10% of savings, verification not below baseline
- `phase1Gate.missingRealSessionData`: inventory of evidence still absent
- `eligibleAsProductionEvidence`: true only when `evidenceClass` is production
  and the hard gate passes

Unavailable or incomplete inputs produce `unknown`, never a silent pass.
A labeled fixture can validate harness logic; it cannot authorize expanding
Jev Runtime.

## Labeled fixture skeleton

Committed skeleton:

- dataset: `tests/fixtures/jev-session-replay/phase1.skeleton.json`
- evidence: `tests/fixtures/jev-session-replay/fixture-evidence*.txt`

```bash
node scripts/evaluate-jev-session-replay.mjs \
  --input tests/fixtures/jev-session-replay/phase1.skeleton.json \
  --repository-root . \
  --output /tmp/jev-phase1-fixture-report.json
```

## Real session data still missing

Recorded on every report and in `MISSING_REAL_SESSION_DATA`:

1. Production Codex session rollouts with per-turn raw/cached/uncached/output/reasoning token usage
2. Tool-call traces under each Phase1 arm so tool duplication can be measured, not assumed
3. Critical-constraint inventories per task for Critical Recall scoring
4. Provider cost traces (input/cache/output/jev/retry) for baseline, deterministic-gc, and gc+jev
5. Live OpenJev (BD-01) KEEP/DROP scores wired into the replay loop
6. Decision-engine (BD-02) auditable ContextRetention decisions from real sessions
7. Deterministic GC (BD-03) applied histories replayed side-by-side with baseline
8. TTFT and end-to-end latency instrumentation for every selected mode
9. A real multi-task corpus with WorkItem/WorkRun identities large enough for Phase1
10. Independent verification outcomes so Task Success is comparable to baseline

Until those inputs exist, Phase1 remains `phase1DecisionReady: false` for
non-production evidence even when fixture numbers look favorable.

## Relationship to sibling JEV branches

- BD-01 OpenJev provider adapter: supplies KEEP/DROP scores for `gc+jev`
- BD-02 decision engine: auditable PIN/KEEP/TRUNCATE/DROP/ARCHIVE actions
- BD-03 deterministic GC: the `deterministic-gc` arm and the GC stage before Jev
- BD-04 this harness: comparative measurement only; it does not implement GC or Jev

## Remote scoring transports

The Rust `HttpsJevRemoteTransport::try_new` constructor targets self-hosted NLI
services. For TypeSafe, use `try_new_system_one` with the configured HTTPS
`/v1/systemone` endpoint, a model returned by `/v1/models`, `devices = [Remote]`
and `dtypes = [Auto]`. The service controls accelerator placement and precision.

The adapter maps each premise/hypothesis pair to a named `choice` question with
entailment, contradiction and neutral criteria. It matches returned answers by
name, validates normalized probabilities and requires reported token usage.
Requests and responses are bounded to 2 MiB. Credentials stay in the Bearer
header; configuration and transport debug output redact them.

The wire contract is defined by the [TypeSafe OpenAPI document](https://api.typesafe.ai/openapi.json).
A direct service check can run the ignored Rust test `system_one_live_transport`
with `WWC_TYPESAFE_ENDPOINT`, `WWC_TYPESAFE_MODEL` and `WWC_TYPESAFE_API_KEY` supplied
through the process environment. Its result is transport evidence. Production
Fusion/JEV runs still require the corresponding Worker composition and evidence.
SystemOne results retain input tokens, output tokens and the response model through
single/batch evaluation and runtime observations. The configured model alias stays
in `model_id`; `resolved_model_id` identifies the actual response model. Providers
that do not report output usage or a resolved model leave those fields absent.
Formal reporting must treat absence as unknown rather than zero or the requested alias.

`RuntimeFusionJudge` returns the complete JEV run record, including failed
attempts before recovery. `MultiRoundReport.jev_calls` groups successful usage
and failed attempts by claim for host accounting. These records can be
serialized separately; they never enter the judge premise or authorize a claim.
The production benchmark adapter must persist them with its task ledger.
