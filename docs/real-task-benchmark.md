# Real-task benchmark gate

`scripts/evaluate-real-task-benchmark.mjs` accepts only a dataset with at least 20 production tasks. Every row must bind an exact source commit, configuration digest, WorkItem, WorkRun, result, and one or more repository-relative evidence files with matching SHA-256 digests.

The gate requires simple, medium, and complex work; recovery, regression, and collaboration cases; and both successful and unsuccessful outcomes. Missing evidence, changed evidence, synthetic provenance, or incomplete coverage stops the run.

Run it without modifying the evidence dataset:

```bash
node scripts/evaluate-real-task-benchmark.mjs \
  --input /path/to/real-task-benchmark.json \
  --repository-root /path/to/evidence-root \
  --output /new/path/benchmark-report.json
```

The output reports accepted task rate, human active minutes per task, recovery success, false and missed attention, verification-failure escape, rework rate, model/verification cost with unknown counts, and paired peer-collaboration benefit. A zero denominator stays `null`; it is never reported as a zero rate.

## Session replay comparison

The same evaluator also accepts `kind: "winwincode.session-replay-benchmark.v1"`. Each task has exactly one selected run for each of `baseline`, `jev`, `jev+memory`, and `jev+rotation`. Select a smaller batch with `--modes baseline,jev`.

An available run must use `provenance: "production"`, bind its source commit, configuration digest, WorkItem, WorkRun, and evidence digest, and provide this result shape:

```json
{
  "tokens": { "rawInput": 0, "cachedInput": 0, "uncachedInput": 0, "output": 0, "reasoning": 0, "jevInput": 0 },
  "context": { "active": 0, "removed": 0, "archived": 0, "bootstrap": 0 },
  "agent": { "turns": 0, "toolCalls": 0, "fileReads": 0, "greps": 0, "tests": 0, "repeatToolCalls": 0 },
  "perf": { "ttftMs": 0, "latencyMs": 0, "jevLatencyMs": null, "rotations": 0 },
  "quality": { "success": true, "verificationPassed": true, "regression": false, "contextFailure": false, "forgottenConstraint": false, "criticalRecall": { "recalled": 99, "total": 100 } },
  "cost": [{ "provider": "PROVIDER", "inputUsd": 0, "cacheUsd": 0, "outputUsd": 0, "jevUsd": 0, "retryUsd": 0 }]
}
```

Every measurement may be `null` when production evidence does not expose it. A mode that cannot run is recorded without measurements or evidence:

```json
{
  "mode": "jev",
  "status": "unavailable",
  "unavailable": { "code": "DEPENDENCY_UNAVAILABLE", "detail": "BD-01 is not ready" }
}
```

The report kind is `winwincode.session-replay-benchmark-report.v1`. Every numeric result contains `total`, `measuredTotal`, `knownRuns`, and `unknownRuns`, so totals and gates can be recalculated from the report. Unavailable or incomplete inputs make the relevant gate `unknown`; they never count as a pass. The gate checks billed-input and repeated-tool reductions of at least 15%, Jev cost at most 10% of provider-cost savings, no success or verification drop, memory regression below 2%, and critical recall of at least 99%.

```bash
node scripts/evaluate-real-task-benchmark.mjs \
  --input /path/to/session-replay.json \
  --repository-root /path/to/evidence-root \
  --modes baseline,jev,jev+memory,jev+rotation \
  --output /new/path/session-replay-report.json
```
