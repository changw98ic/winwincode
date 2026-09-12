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
