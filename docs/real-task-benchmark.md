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

## JEV session replay is a different track

Baseline vs Deterministic GC vs GC+Jev comparison lives in
`scripts/evaluate-jev-session-replay.mjs` and
[`docs/jev-session-replay.md`](./jev-session-replay.md). That harness allows
clearly labeled fixture/replay datasets. Passing a session-replay kind into this
E13 evaluator fails closed and points at the JEV script. JEV replay results are
not E13 production evidence.
