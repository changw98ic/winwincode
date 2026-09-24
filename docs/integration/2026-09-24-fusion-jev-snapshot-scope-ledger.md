# Fusion/JEV and Snapshot Integration Scope Ledger

Date: 2026-09-24

Branch: integration/fusion-jev-and-lint-cleanup

Base: da407e4f3df3719718b23bd8913b73fe3ff73857

Reviewed head: e5cc34e51531a165cb05b47dc4efe6bc453de0a4

This ledger records the reason, ownership, validation and rollback boundary for
historical mixed commits. It preserves the integration branch without rewriting
history. Future changes follow ADR-0039 Q-07 and map to one bounded
responsibility task per commit.

## Historical Commit Ledger

| Commit | Primary responsibility | Independent rationale | Acceptance path | Rollback boundary |
| --- | --- | --- | --- | --- |
| 4726a562 | Fusion/JEV investigation engine | Introduces the ADR-0035 through ADR-0038 feature implementation and fixtures | Fusion regression, unit, blind-panel and future frozen real benchmark | Roll back the Fusion feature set; Snapshot and runtime fixes remain independently testable |
| 0c4df69c | Unrelated lint cleanup | Removes pre-existing workspace Clippy debt outside the Fusion modules | Workspace Clippy, with source inventory unchanged | Revert only formatting/lint edits; no product-state contract depends on this commit |
| 464033cf | Agent documentation | Records commands, narrow test invocation and architecture navigation | Documentation review and source format check | Revert documentation only |
| d78e4140 | Outbox replay idempotence | Prevents replayed action/approval responses from creating duplicate product effects | Outbox unit suite and replay tests | Revert outbox idempotence independently of Fusion and Snapshot |
| 32d97d7d | CLI schema fixture | Keeps the device fixture relative to the live schema version | CLI fixture test and contracts check | Revert the CLI fixture only |
| 62d2789a | Heartbeat liveness write | Prevents torn heartbeat reads and defers the first background tick | Heartbeat unit suite; superseded by RUN-001 terminal-order repair | Revert heartbeat write behavior without changing runtime journal plans |
| d72a6315 | Candidate artifact protocol | Uses the decoded candidate media type in the exact artifact payload | Candidate pin and artifact-message tests | Revert the protocol fixture without weakening candidate pin assertions |
| dd57ece1 | Canonical Snapshot schema | Adds immutable Snapshot to generated source schema | Contracts check and domain contract tests | Revert schema and generated outputs together |
| 0b99b278 | Snapshot binding compatibility transition | Makes snapshotId optional while constructors migrate | Contract generation and constructor tests | Revert transition only after SNAP-001 cutover; no permanent fallback is allowed |
| 9fa6318f | Worker Snapshot materialization | Adds candidate freeze through a detached worktree | Snapshot freezing tests | Revert materialization; production verification remains blocked until Q-03 protection is enforced |
| 14df7ac3 | Delivery Snapshot domain | Introduces sealed Snapshot domain data | Snapshot binding and domain tests | Revert domain type only with SNAP-001 owner migration |
| e5cc34e5 | Worker Snapshot verification naming | Passes Snapshot identity on verification frames | Snapshot binding and session transaction tests | Revert naming only with SNAP-001 binding migration |

## Current Remediation Boundaries

| Change set | Responsibility issue | Rollback boundary |
| --- | --- | --- |
| ADR-0039 and defect decision record | winwincode-community.7 | Documentation/contract only |
| Candidate pin exact verification retention | winwincode-community.7.6 | Control Plane artifact-pin path; no Snapshot architecture change |
| Heartbeat terminal monotonicity | winwincode-community.7.7 | Control Plane heartbeat module; no Runtime Journal change |
| Branch-owned ADR and fixture formatting | winwincode-community.7.9 | Mechanical formatting only |

## Remaining Scope

The canonical Snapshot production chain, protected source capability, Fusion
source receipts and verified counter gate, JEV recovery/blind judging and frozen
real benchmark remain separate open remediation tasks. An independent fix merge
must not be treated as acceptance of those feature gates.
