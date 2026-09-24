# Frozen Snapshot Implementation Plan (Phase 1 of 6)

> **Superseded review input:** retained only as the source contract audited by
> `docs/defects/2026-09-24-integration-fusion-snapshot-defect-review.md`.
> ADR-0039 is authoritative; this plan's Worker-owned Snapshot allocation and
> historical migration assumptions must not be implemented.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every verification run is bound to one immutable, numbered code snapshot created after the Candidate is produced and before any verification starts — so a test result can be traced to an exact code identity without cross-table inference.

**Architecture:** A `Snapshot` replaces the existing `ValidatedGitSnapshotFact` / `CandidateSnapshot` pair as the single canonical "code identity under verification". The Worker freezes the candidate into a read-only `git worktree`, allocates a `snapshot_id`, and every verification binding carries that id. The old types are deleted, not aliased (destructive update).

**Tech Stack:** Rust 1.95.0 (workspace crates `winwincode-domain`, `winwincode-delivery`, `winwincode-worker`, `winwincode-storage`, `winwincode-execution-port`), canonical JSON Schema at `schema/winwincode/v1/` driving 7 generated outputs, `git worktree` for frozen checkouts, `cargo test` for verification.

## Global Constraints

- Toolchain: Node `24.19.0` (`.node-version`), pnpm `11.7.0` via Corepack, Rust `1.95.0` (`rust-toolchain.toml`).
- All pnpm invocations use `corepack pnpm …`.
- License headers on project-owned code: `// SPDX-License-Identifier: Apache-2.0`. No third-party license text outside `NOTICE` / `THIRD_PARTY_NOTICES.md`.
- Contracts are generated. Never hand-edit `crates/*/src/generated.rs`, `apps/client/src/generated/`, or `schema/winwincode/v1/openapi.generated.json`. Edit `schema/winwincode/v1/*.schema.json`, then run `corepack pnpm contracts:generate`. `corepack pnpm contracts:check` must pass.
- Identifier convention: `"<prefix>_<26-char base32 uppercase>"`, prefix `snap` for snapshots. Reuse the existing canonical ULID allocator — do not invent a second id encoding.
- One canonical path: delete the old type and its call sites. No `#[deprecated]` alias, no re-export shim, no "compatibility" module.
- Lint gate is `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`. Warnings are errors.
- Rust indent 4 spaces; TypeScript indent 2 spaces, strict ESM.
- Platforms to keep compiling: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`. `git worktree` is available on all four.
- Every commit message ends with:
  `Co-Authored-By: Claude Code <noreply@anthropic.com>`
- Report the exact commands run and their results at handoff. Do not commit/push unless asked.

## File Structure

| File | Responsibility |
| --- | --- |
| `schema/winwincode/v1/domain.schema.json` | Adds `SnapshotId`, `Snapshot`; removes the old snapshot fact shape |
| `schema/winwincode/v1/execution-port.schema.json` | Adds `snapshotId` to the job/session binding messages the Worker reports |
| `crates/winwincode-domain/src/generated.rs` | Generated — never edit |
| `crates/winwincode-execution-port/src/generated.rs` | Generated — never edit |
| `crates/winwincode-delivery/src/domain/candidate.rs` | **Delete** `ValidatedGitSnapshotFact`, `validated_git_snapshot`, `validated_git_snapshot_between`, `validated_candidate_checkout`, `seal_git_snapshot` |
| `crates/winwincode-delivery/src/domain/snapshot.rs` | **Create** — `Snapshot`, `SnapshotId`, `SnapshotBuilder`, immutability + seal |
| `crates/winwincode-worker/src/snapshot_worktree.rs` | **Create** — freeze a candidate into a read-only `git worktree`, return the path |
| `crates/winwincode-worker/src/stage_product.rs` | **Modify** — `prepare_verification_artifact` takes a `Snapshot`, not a live workspace |
| `crates/winwincode-worker/src/workspace.rs` | **Modify** — `snapshot_candidate` / `snapshot_verification` collapse into one `freeze_snapshot` |
| `crates/winwincode-storage/src/git_candidate_retention.rs` | **Modify** — pin records reference `snapshot_id` |
| `crates/winwincode-storage/src/git_source.rs` | **Modify** — `revalidate_candidate_source` verifies against a `Snapshot` |
| `crates/winwincode-control-plane/src/lib.rs` | **Modify** — accept and persist `snapshotId` on verification bindings |
| `docs/decisions/0039-immutable-snapshot-binding.md` | **Create** — supersedes the snapshot portion of ADR-0033 |
| `crates/winwincode-worker/tests/snapshot_freezing.rs` | **Create** — invariant tests |

---

### Task 0: Land the independent bug fixes already verified

These predate the refactor and are not superseded by it (the heartbeat one is superseded in Phase 4 — keep it anyway so Phase 4 has a green baseline to replace).

**Files:**
- Modify: `crates/winwincode-codex/src/outbox.rs`
- Modify: `crates/winwincode-cli/src/backup.rs`
- Modify: `crates/winwincode-control-plane/src/heartbeat.rs`
- Modify: `crates/winwincode-control-plane/tests/session_binding_transaction.rs`

**Interfaces:**
- Consumes: nothing
- Produces: a green `cargo test` baseline for the crates this plan touches

- [ ] **Step 1: Confirm the four files still compile and their tests pass**

Run:
```bash
cargo test -p winwincode-codex --lib outbox
cargo test -p winwincode-cli --lib backup
cargo test -p winwincode-control-plane --lib heartbeat
cargo test -p winwincode-control-plane --test session_binding_transaction
```
Expected: all four report `0 failed`. (The session-binding one still has one failing assertion about the pin — that is Task 5's subject, not this task. If `control_plane_rebuilds_the_candidate_from_its_exact_artifact_and_successful_outcome` is the only failure, proceed.)

- [ ] **Step 2: Run the lint gate on the touched crates**

Run:
```bash
cargo clippy -p winwincode-codex -p winwincode-cli -p winwincode-control-plane \
  --all-targets --all-features --locked -- -D warnings
```
Expected: `PASS`, exit code 0.

- [ ] **Step 3: Commit the three independent fixes separately from the pin assertion**

```bash
git add crates/winwincode-codex/src/outbox.rs
git commit -m "fix(outbox): treat replayed action/approval responses as idempotent

A crash between the Worker compacting its request and the transport ACK
reaching the Server replays the exact response. acknowledge_response deleted
0 rows on the second receipt and fell through to Conflict, which the Worker
mapped to UnexpectedMessage and dropped the terminal outcome.

ActionEnforcementReceipt and ApprovalDecision already apply through a durable
ledger (the action gate treats Consumed as success; the approval ledger
re-resolves an already-Resolved operation), so post-compaction replay is an
expected no-op. Extends the same whitelist as InputResponse / JobOutcomeAck /
ModelChunk and the regression that covers it.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

```bash
git add crates/winwincode-cli/src/backup.rs
git commit -m "fix(cli): keep the device schema fixture relative to the live version

The fail-closed restore fixture hardcoded schema version 7 as 'unsupported',
but CLIENT_STORE_SCHEMA_VERSION became 7, so the guard correctly accepted it
and the test asserted the pass path. Uses CLIENT_STORE_SCHEMA_VERSION + 1 so
the fixture cannot go stale again.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

```bash
git add crates/winwincode-control-plane/src/heartbeat.rs
git commit -m "fix(heartbeat): atomic record write and deferred background tick

fs::write truncates first, so a concurrent supervisor read a half-written
record and classify_heartbeat reported a live job as Stalled. Writes through a
unique staging file plus rename. The background beater now sleeps before its
first tick so it cannot clobber the caller's own phase.

Superseded by Phase 4 (runtime journal); kept so Phase 4 replaces a green
baseline rather than a broken one.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

```bash
git add crates/winwincode-control-plane/tests/session_binding_transaction.rs
git commit -m "test(session-binding): name the candidate media type on the payload

EncodedPayload.content_type is the media type of the decoded bytes, not the
transport encoding. The candidate manifest is
application/vnd.winwincode.git-candidate+json; application/octet-stream made
pin_candidate_git_after_final_artifact_ack skip the pin silently.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 1: `SnapshotId` and `Snapshot` in the canonical contract

**Files:**
- Modify: `schema/winwincode/v1/domain.schema.json`
- Modify: `schema/winwincode/v1/execution-port.schema.json`
- Generated on run: `crates/winwincode-domain/src/generated.rs`, `crates/winwincode-execution-port/src/generated.rs`, `apps/client/src/generated/contracts.ts`, `apps/client/src/generated/control-plane-client.ts`, `schema/winwincode/v1/schema-collection.generated.json`, `schema/winwincode/v1/openapi.generated.json`, `crates/winwincode-api/src/generated.rs`

**Interfaces:**
- Consumes: existing `$defs` for `WorkRunId`, `RepositoryId`, `Sha256Digest`, `Instant` in `domain.schema.json`
- Produces:
  - `SnapshotId` — branded `string`, prefix `snap_`
  - `Snapshot` — the object Task 2 builds and Task 4 binds to
  - `snapshotId` field on `sessionBinding` and on the artifact-open/chunk messages

- [ ] **Step 1: Write the failing contract-drift test**

Create `tests/domain-schema.test.mjs` content (append to the existing schema test if one exists — check `tests/domain-schema.test.mjs` first):

```js
import assert from 'node:assert/strict'
import test from 'node:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const domain = JSON.parse(
  readFileSync(join(process.cwd(), 'schema/winwincode/v1/domain.schema.json'), 'utf8'),
)

test('domain schema defines SnapshotId and Snapshot', () => {
  assert.ok(domain.$defs.SnapshotId, 'SnapshotId is missing')
  assert.ok(domain.$defs.Snapshot, 'Snapshot is missing')
  assert.equal(domain.$defs.Snapshot.properties.snapshotId.$ref, '#/$defs/SnapshotId')
  assert.equal(domain.$defs.Snapshot.properties.immutable.const, true)
})

test('Snapshot carries the exact code identity', () => {
  const props = domain.$defs.Snapshot.properties
  for (const key of [
    'candidateCommitId',
    'candidateTreeId',
    'baseCommitId',
    'baseTreeId',
    'diffSha256',
    'contentDigest',
    'validationSeal',
    'createdAtMillis',
  ]) {
    assert.ok(props[key], `Snapshot is missing ${key}`)
  }
})
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `node --test tests/domain-schema.test.mjs`
Expected: FAIL with `SnapshotId is missing`

- [ ] **Step 3: Add the definitions to `domain.schema.json`**

Inside `$defs`, add:

```json
"SnapshotId": {
  "type": "string",
  "pattern": "^snap_[0-9A-HJKMNP-TV-Z]{26}$",
  "description": "Canonical ULID for one immutable code snapshot under verification."
},
"Snapshot": {
  "type": "object",
  "additionalProperties": false,
  "required": [
    "snapshotId", "candidateId", "workRunId", "repositoryId",
    "baseCommitId", "baseTreeId", "candidateCommitId", "candidateTreeId",
    "diffSha256", "contentDigest", "validationSeal", "createdAtMillis", "immutable"
  ],
  "properties": {
    "snapshotId": { "$ref": "#/$defs/SnapshotId" },
    "candidateId": { "$ref": "#/$defs/CandidateId" },
    "workRunId": { "$ref": "#/$defs/WorkRunId" },
    "repositoryId": { "$ref": "#/$defs/RepositoryId" },
    "baseCommitId": { "type": "string", "minLength": 40, "maxLength": 64 },
    "baseTreeId": { "type": "string", "minLength": 40, "maxLength": 64 },
    "candidateCommitId": { "type": "string", "minLength": 40, "maxLength": 64 },
    "candidateTreeId": { "type": "string", "minLength": 40, "maxLength": 64 },
    "diffSha256": { "$ref": "#/$defs/Sha256Digest" },
    "contentDigest": { "$ref": "#/$defs/Sha256Digest" },
    "validationSeal": { "type": "string", "pattern": "^sha256:[0-9a-f]{64}$" },
    "createdAtMillis": { "type": "integer", "minimum": 0 },
    "immutable": { "type": "boolean", "const": true }
  }
}
```

If `CandidateId` does not exist yet in `$defs`, add it following the exact shape of `WorkRunId` (branded string, pattern `^cnd_[0-9A-HJKMNP-TV-Z]{26}$`).

- [ ] **Step 4: Bind `snapshotId` into the ExecutionPort messages**

In `execution-port.schema.json`, add to the `SessionBindingMessage` properties and to `ArtifactOpenMessage`/`ArtifactChunkMessage`:

```json
"snapshotId": {
  "$ref": "./domain.schema.json#/$defs/SnapshotId",
  "default": null
}
```

Do **not** add `snapshotId` to those messages' `required` arrays. The field is
optional at the transport level on purpose:

- a writer's artifact frames are produced **before** a Snapshot exists (the
  snapshot is made from the candidate after the writer finishes), so they
  cannot carry one;
- the rule "verification frames must name their snapshot" is a business rule,
  enforced in Task 4 at the Control Plane, not a transport shape rule.

Each message's `properties.snapshotId` must be paired with the generator's
optional-field annotation so the emitted Rust field is `Option<SnapshotId>`
with `skip_serializing_if`. Check the neighbouring optional fields in
`execution-port.schema.json` (for example `reason` on `ApprovalDecisionMessage`)
and copy their exact annotation shape so the generator treats this the same
way.

- [ ] **Step 5: Regenerate and verify the seven outputs**

Run:
```bash
corepack pnpm contracts:generate
corepack pnpm contracts:check
node --test tests/domain-schema.test.mjs
```
Expected: `contracts:check` reports no drift; the two schema tests PASS.

If `tests/domain-schema.test.mjs` is not in the canonical list, add it to the `canonicalTestFiles` array in `scripts/run-ts-tests.mjs`.

- [ ] **Step 6: Commit**

```bash
git add schema/winwincode/v1 tests/domain-schema.test.mjs scripts/run-ts-tests.mjs \
  crates/winwincode-domain/src/generated.rs \
  crates/winwincode-api/src/generated.rs \
  crates/winwincode-execution-port/src/generated.rs \
  apps/client/src/generated
git commit -m "feat(contract): add immutable Snapshot to the canonical schema

SnapshotId and Snapshot replace the ad-hoc snapshot facts as the one code
identity a verification run binds to. snapshotId becomes required on session
binding and artifact frames so a Worker cannot report verification work
without naming the snapshot under test.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 1b: Migrate every message constructor to the optional `snapshotId`

Task 1 makes `snapshotId` optional at the transport level. Every Rust struct
literal for `ArtifactOpenMessage`, `ArtifactChunkMessage` and
`SessionBindingMessage` must still set the field explicitly — Rust requires all
fields in a literal. Frames produced before a snapshot exists set `None`; the
verification frames that must carry one are wired up in Task 4.

**Files:**
- Modify: `crates/winwincode-codex/src/candidate_artifact_outbox.rs`
- Modify: `crates/winwincode-codex/src/diagnostic_artifact_outbox.rs`
- Modify: `crates/winwincode-storage/src/git_candidate_retention.rs`
- Modify: `crates/winwincode-execution-port/src/lib.rs` (`typed_replay`, `artifact_open_message`)
- Modify: `crates/winwincode-control-plane/src/artifact_transaction.rs`
- Modify: `crates/winwincode-control-plane/src/lib.rs` (constructor sites)
- Modify: `tests/fixtures/contracts/execution-port.valid.json` (add `snapshotId: null` or omit, matching the optional shape)
- Modify: `tests/execution-port-contract.test.mjs` (add `SnapshotId` to the closed `domainDefinitions` allowlist)
- Modify: any test that constructs these messages — `crates/winwincode-worker/tests/worker_lifecycle.rs`, `crates/winwincode-control-plane/tests/session_binding_transaction.rs`, `crates/winwincode-control-plane/tests/provider_production/live_delivery.rs`

**Interfaces:**
- Consumes: the `SnapshotId` field emitted by Task 1
- Produces: a workspace that compiles again. Every pre-snapshot frame carries `snapshot_id: None`, and the field is optional on the wire.

- [ ] **Step 0: Make `snapshotId` optional if an earlier run marked it required**

Task 1 as first executed may have left `snapshotId` in the `required` arrays of
`session.binding`, `artifact.open` and `artifact.chunk`. That cannot hold: a
writer's artifact frames are produced before a Snapshot exists.

In `schema/winwincode/v1/execution-port.schema.json`:
1. Remove `snapshotId` from each `required` array where it appears.
2. Ensure the property is annotated the same way as the file's other optional
   fields (for example `reason` on `ApprovalDecisionMessage`) so the generator
   emits `Option<SnapshotId>` with `skip_serializing_if`.
3. Run `corepack pnpm contracts:generate && corepack pnpm contracts:check` and
   confirm `crates/winwincode-execution-port/src/generated.rs` shows
   `pub snapshot_id: Option<...>`.

- [ ] **Step 1: Enumerate every constructor that no longer compiles**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo check --workspace --all-targets --all-features 2>&1 | grep -E "missing field \`snapshot_id\`" -B2 | grep -E "^\s+-->" | sort -u
```
Expected: a list of every site to touch. Record it.

- [ ] **Step 2: Set `snapshot_id: None` at every pre-snapshot frame**

At each site from Step 1, add `snapshot_id: None,` to the struct literal. Do not invent a snapshot id — a fabricated id would decide Task 3 and Task 4 semantics.

Two sites are verification frames and must NOT be given `None` permanently — `accept_verification_artifact` in `crates/winwincode-control-plane/tests/session_binding_transaction.rs` and the verification path in `crates/winwincode-control-plane/src/lib.rs`. Set `None` for now so the workspace compiles; Task 4 replaces it with the real binding and adds the rejection for a missing one.

- [ ] **Step 3: Fix the contract fixtures**

In `tests/execution-port-contract.test.mjs`, add `'SnapshotId'` to the `domainDefinitions` allowlist. In `tests/fixtures/contracts/execution-port.valid.json`, add `snapshotId` to each `session.binding`, `artifact.open` and `artifact.chunk` sample with value `null` (or omit it if the generator marks it skip-on-none) so the positive samples validate.

- [ ] **Step 4: Verify the workspace compiles and the suites are green**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo check --workspace --all-targets --all-features 2>&1 | grep -c "missing field \`snapshot_id\`"
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings 2>&1 \
  | grep -E "^(error|warning)" -A1 | grep "crates/" | grep -v "snapshot_freezing"
cargo test -p winwincode-codex --lib outbox
node --test tests/execution-port-contract.test.mjs
```
Expected: `0` for the grep count; the clippy filter prints nothing; outbox tests green; the contract tests return to their pre-Task-1 pass count.

**Scoping note.** `crates/winwincode-worker/tests/snapshot_freezing.rs` is Task 2's
TDD fixture. It deliberately references `winwincode_worker::snapshot_worktree`,
which Task 2 Step 3 creates, so it is expected to fail to compile until then.
That file is filtered out of this gate on purpose. Do not edit it and do not
delete it — Task 2 Step 2 relies on its "red" state.

- [ ] **Step 5: Commit**

```bash
git add -u schema/ crates/ tests/ apps/
git commit -m "fix(contract): optional snapshotId with every constructor migrated

Task 1 made snapshotId required at the transport level, which is impossible to
satisfy for writer artifact frames: they are produced before a Snapshot exists.
The field is now optional on the wire and every pre-snapshot constructor sets
None. Task 4 enforces the business rule that verification frames must carry
one.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

`schema/` and `apps/` must be staged: this task edits
`schema/winwincode/v1/execution-port.schema.json` and regenerates
`openapi.generated.json`, `schema-collection.generated.json` and the two client
outputs. Staging only `crates/` and `tests/` would leave a commit that fails
`corepack pnpm contracts:check` on a clean checkout.

Do not stage `crates/winwincode-worker/tests/snapshot_freezing.rs` (Task 2's),
`.beads/interactions.jsonl`, `docs/superpowers/` or `fusion-benchmark-tasks/`.

---

### Task 2: Freeze a candidate into a read-only `git worktree`

**Files:**
- Create: `crates/winwincode-worker/src/snapshot_worktree.rs`
- Modify: `crates/winwincode-worker/src/lib.rs` (add `pub mod snapshot_worktree;`)
- Test: `crates/winwincode-worker/tests/snapshot_freezing.rs`

**Interfaces:**
- Consumes: `git` on `PATH`; a detached worktree source repository with a candidate commit
- Produces:
  - `pub struct FrozenWorktree { pub path: PathBuf, pub commit_id: String, pub tree_id: String }`
  - `pub fn freeze_worktree(repo: &Path, candidate_commit: &str, dest: &Path) -> Result<FrozenWorktree, SnapshotWorktreeError>`
  - `pub fn drop_worktree(repo: &Path, dest: &Path) -> Result<(), SnapshotWorktreeError>`

- [ ] **Step 1: Write the failing test**

`crates/winwincode-worker/tests/snapshot_freezing.rs`:

```rust
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn seed_repo(root: &std::path::Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("frozen.txt"), "v1").expect("write");
    git(root, &["add", "frozen.txt"]);
    git(root, &["-c", "user.email=fixture@example.invalid", "-c", "user.name=Fixture",
        "commit", "-q", "-m", "base"]);
    std::fs::write(root.join("frozen.txt"), "v2").expect("write");
    git(root, &["add", "frozen.txt"]);
    git(root, &["-c", "user.email=fixture@example.invalid", "-c", "user.name=Fixture",
        "commit", "-q", "-m", "candidate"]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn frozen_worktree_is_immune_to_live_workspace_changes() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 1));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    let frozen = root.join("frozen");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let candidate_commit = seed_repo(&repo);

    let snap = winwincode_worker::snapshot_worktree::freeze_worktree(
        &repo,
        &candidate_commit,
        &frozen,
    )
    .expect("freeze");
    assert_eq!(snap.commit_id, candidate_commit);

    // Mutate the live workspace after freezing.
    std::fs::write(repo.join("frozen.txt"), "MUTATED").expect("mutate");
    git(&repo, &["add", "frozen.txt"]);
    git(&repo, &["-c", "user.email=fixture@example.invalid", "-c", "user.name=Fixture",
        "commit", "-q", "-m", "mutate"]);

    let frozen_bytes = std::fs::read_to_string(frozen.join("frozen.txt")).expect("read frozen");
    assert_eq!(frozen_bytes, "v2", "frozen copy must not follow the live workspace");

    winwincode_worker::snapshot_worktree::drop_worktree(&repo, &frozen).expect("drop");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_worktree_rejects_a_missing_commit() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 2));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    seed_repo(&repo);

    let result = winwincode_worker::snapshot_worktree::freeze_worktree(
        &repo,
        "0000000000000000000000000000000000000000",
        &root.join("nope"),
    );
    assert!(result.is_err(), "a foreign commit must be refused");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_worktree_is_read_only() {
    let root = std::env::temp_dir().join(format!("wwc-snap-{}-{}", std::process::id(), 3));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    let frozen = root.join("frozen");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let candidate_commit = seed_repo(&repo);

    let snap = winwincode_worker::snapshot_worktree::freeze_worktree(
        &repo, &candidate_commit, &frozen,
    ).expect("freeze");

    // The frozen copy must not report a dirty tree — a verifier cannot write.
    let status = git(&repo, &["-C", &frozen.to_string_lossy(), "status", "--porcelain"]);
    assert_eq!(status, "", "frozen worktree must start clean");

    winwincode_worker::snapshot_worktree::drop_worktree(&repo, &frozen).expect("drop");
    let _ = std::fs::remove_dir_all(&root);
    let _ = snap;
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p winwincode-worker --test snapshot_freezing`
Expected: FAIL with `use of undeclared crate or module winwincode_worker::snapshot_worktree`

`crates/winwincode-worker/tests/snapshot_freezing.rs` may already exist and be
uncommitted from an earlier stopped run. If it is present, keep its content and
continue from this step — do not rewrite the file.

- [ ] **Step 3: Implement `snapshot_worktree.rs`**

```rust
// SPDX-License-Identifier: Apache-2.0

//! Freezes one candidate commit into a read-only `git worktree`.
//!
//! A verifier must run against the snapshot, never the live workspace, so
//! later writes by any other worker cannot change what was tested.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Failure while freezing or releasing one snapshot worktree.
#[derive(Debug)]
pub struct SnapshotWorktreeError {
    code: SnapshotWorktreeErrorCode,
    message: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SnapshotWorktreeErrorCode {
    InvalidInput,
    Git,
    Io,
}

impl SnapshotWorktreeError {
    fn new(code: SnapshotWorktreeErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn code(&self) -> SnapshotWorktreeErrorCode {
        self.code
    }
}

impl std::fmt::Display for SnapshotWorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SnapshotWorktreeError {}

/// A read-only checkout of one exact candidate commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenWorktree {
    /// Absolute path of the read-only checkout.
    pub path: PathBuf,
    /// Exact commit the checkout resolves to.
    pub commit_id: String,
    /// Git tree id of that commit.
    pub tree_id: String,
}

/// Checks `candidate_commit` out into `dest` as a read-only worktree.
///
/// # Errors
///
/// Refuses a missing repository, a commit that is not reachable from `repo`,
/// an existing `dest`, or any `git worktree` failure.
pub fn freeze_worktree(
    repo: &Path,
    candidate_commit: &str,
    dest: &Path,
) -> Result<FrozenWorktree, SnapshotWorktreeError> {
    if !repo.is_dir() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "snapshot source repository is missing",
        ));
    }
    if dest.exists() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "snapshot destination already exists",
        ));
    }
    let resolved = git(repo, &["rev-parse", "--verify", &format!("{candidate_commit}^{{commit}")])?;
    if resolved != candidate_commit {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::InvalidInput,
            "candidate commit does not resolve to the requested identity",
        ));
    }
    let tree_id = git(repo, &["rev-parse", &format!("{candidate_commit}^{{tree}}")])?;
    git(
        repo,
        &["worktree", "add", "--detach", &dest.to_string_lossy(), candidate_commit],
    )?;
    Ok(FrozenWorktree {
        path: dest.to_path_buf(),
        commit_id: candidate_commit.to_owned(),
        tree_id,
    })
}

/// Releases one frozen worktree and removes its directory.
///
/// # Errors
///
/// Returns the underlying `git worktree remove` failure.
pub fn drop_worktree(repo: &Path, dest: &Path) -> Result<(), SnapshotWorktreeError> {
    git(repo, &["worktree", "remove", "--force", &dest.to_string_lossy()])?;
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String, SnapshotWorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|error| {
            SnapshotWorktreeError::new(
                SnapshotWorktreeErrorCode::Io,
                format!("git is unavailable: {error}"),
            )
        })?;
    if !out.status.success() {
        return Err(SnapshotWorktreeError::new(
            SnapshotWorktreeErrorCode::Git,
            format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
```

Add `pub mod snapshot_worktree;` to `crates/winwincode-worker/src/lib.rs` next to the other module declarations.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p winwincode-worker --test snapshot_freezing`
Expected: `3 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add crates/winwincode-worker/src/snapshot_worktree.rs crates/winwincode-worker/src/lib.rs \
  crates/winwincode-worker/tests/snapshot_freezing.rs
git commit -m "feat(worker): freeze a candidate into a read-only git worktree

A verifier runs against the snapshot, never the live workspace, so writes by
another worker cannot change what was tested.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 3: Build a `Snapshot` and bind it to the verification job

**Files:**
- Create: `crates/winwincode-delivery/src/domain/snapshot.rs`
- Modify: `crates/winwincode-delivery/src/domain/mod.rs` (add `pub mod snapshot;`)
- Modify: `crates/winwincode-worker/src/stage_product.rs`
- Test: `crates/winwincode-delivery/tests/snapshot_binding.rs`

**Interfaces:**
- Consumes: `FrozenWorktree` from Task 2; `Snapshot`/`SnapshotId` from Task 1 (generated)
- Produces:
  - `pub struct SnapshotBuilder`
  - `pub fn build(self) -> Result<Snapshot, SnapshotError>`
  - `Snapshot::snapshot_id() -> &SnapshotId`
  - `Snapshot::validation_seal() -> &str`
  - `pub fn verify_seal(snapshot: &Snapshot) -> bool`
  - `prepare_verification_artifact(active, snapshot) -> Result<PreparedCandidateArtifact, CandidateProductError>` (signature change)

- [ ] **Step 1: Write the failing tests**

`crates/winwincode-delivery/tests/snapshot_binding.rs`:

```rust
// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::domain::snapshot::{SnapshotBuilder, verify_seal};

fn builder() -> SnapshotBuilder {
    SnapshotBuilder::new(
        "cnd_00000000000000000000000001",
        "wrn_00000000000000000000000001",
        "rep_00000000000000000000000001",
    )
    .with_base("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .with_candidate(
        "cccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddd",
    )
    .with_diff_sha256("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .with_content_digest("sha256:1111111111111111111111111111111111111111111111111111111111111111")
    .with_created_at_millis(1_800_000_000_000)
}

#[test]
fn snapshot_carries_the_exact_code_identity_and_a_seal() {
    let snapshot = builder().build().expect("snapshot");
    assert!(snapshot.snapshot_id().as_str().starts_with("snap_"));
    assert_eq!(
        snapshot.candidate_commit_id(),
        "cccccccccccccccccccccccccccccccccccccccc"
    );
    assert!(verify_seal(&snapshot), "the seal must cover the code identity");
}

#[test]
fn snapshot_seal_breaks_when_the_code_identity_changes() {
    let good = builder().build().expect("snapshot");
    let mut tampered = good.clone();
    // Simulate a rewritten candidate commit under the same snapshot id.
    tampered.force_candidate_commit_for_test("ffffffffffffffffffffffffffffffffffffffff");
    assert!(!verify_seal(&tampered), "a rewritten identity must fail its own seal");
}

#[test]
fn snapshot_requires_a_candidate_commit() {
    let incomplete = SnapshotBuilder::new(
        "cnd_00000000000000000000000002",
        "wrn_00000000000000000000000002",
        "rep_00000000000000000000000002",
    );
    assert!(incomplete.build().is_err(), "a snapshot without a candidate commit is not buildable");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p winwincode-delivery --test snapshot_binding`
Expected: FAIL with `unresolved import winwincode_delivery::domain::snapshot`

- [ ] **Step 3: Implement `snapshot.rs`**

```rust
// SPDX-License-Identifier: Apache-2.0

//! Immutable code identity one verification run is bound to.
//!
//! A `Snapshot` is created once from a frozen Candidate, before any
//! verification starts. It is never modified afterwards; a changed code
//! identity is a new Snapshot.

use sha2::{Digest, Sha256};

use super::{CandidateId, WorkRunId};
use winwincode_domain::RepositoryId;

/// Failure while assembling one Snapshot.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SnapshotError {
    /// A required code identity field is missing or malformed.
    Incomplete,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("snapshot is missing required code identity")
    }
}

impl std::error::Error for SnapshotError {}

/// One immutable code identity under verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    snapshot_id: winwincode_domain::SnapshotId,
    candidate_id: CandidateId,
    work_run_id: WorkRunId,
    repository_id: RepositoryId,
    base_commit_id: String,
    base_tree_id: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    content_digest: String,
    created_at_millis: u64,
    validation_seal: String,
}

impl Snapshot {
    /// Returns the stable snapshot identity.
    #[must_use]
    pub fn snapshot_id(&self) -> &winwincode_domain::SnapshotId {
        &self.snapshot_id
    }

    /// Returns the exact candidate commit under verification.
    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    /// Returns the exact candidate tree under verification.
    #[must_use]
    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    /// Returns the seal over the complete code identity.
    #[must_use]
    pub fn validation_seal(&self) -> &str {
        &self.validation_seal
    }

    /// Returns the moment this snapshot was created.
    #[must_use]
    pub const fn created_at_millis(&self) -> u64 {
        self.created_at_millis
    }

    /// Test-only: simulates a rewritten identity to prove the seal catches it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn force_candidate_commit_for_test(&mut self, commit_id: &str) {
        self.candidate_commit_id = commit_id.to_owned();
    }
}

/// Recomputes the seal and compares it with the stored one.
#[must_use]
pub fn verify_seal(snapshot: &Snapshot) -> bool {
    seal_fields(
        &snapshot.candidate_id,
        &snapshot.work_run_id,
        &snapshot.repository_id,
        &snapshot.base_commit_id,
        &snapshot.base_tree_id,
        &snapshot.candidate_commit_id,
        &snapshot.candidate_tree_id,
        &snapshot.diff_sha256,
        &snapshot.content_digest,
        snapshot.created_at_millis,
    ) == snapshot.validation_seal
}

fn seal_fields(
    candidate_id: &CandidateId,
    work_run_id: &WorkRunId,
    repository_id: &RepositoryId,
    base_commit_id: &str,
    base_tree_id: &str,
    candidate_commit_id: &str,
    candidate_tree_id: &str,
    diff_sha256: &str,
    content_digest: &str,
    created_at_millis: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.snapshot.v1\0");
    for field in [
        candidate_id.as_str().as_bytes(),
        work_run_id.as_str().as_bytes(),
        repository_id.as_str().as_bytes(),
        base_commit_id.as_bytes(),
        base_tree_id.as_bytes(),
        candidate_commit_id.as_bytes(),
        candidate_tree_id.as_bytes(),
        diff_sha256.as_bytes(),
        content_digest.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.update(created_at_millis.to_be_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Assembles one Snapshot from an exact frozen Candidate.
#[derive(Clone, Debug)]
pub struct SnapshotBuilder {
    candidate_id: String,
    work_run_id: String,
    repository_id: String,
    base_commit_id: Option<String>,
    base_tree_id: Option<String>,
    candidate_commit_id: Option<String>,
    candidate_tree_id: Option<String>,
    diff_sha256: Option<String>,
    content_digest: Option<String>,
    created_at_millis: Option<u64>,
}

impl SnapshotBuilder {
    /// Starts a builder bound to one candidate, work run and repository.
    #[must_use]
    pub fn new(
        candidate_id: impl Into<String>,
        work_run_id: impl Into<String>,
        repository_id: impl Into<String>,
    ) -> Self {
        Self {
            candidate_id: candidate_id.into(),
            work_run_id: work_run_id.into(),
            repository_id: repository_id.into(),
            base_commit_id: None,
            base_tree_id: None,
            candidate_commit_id: None,
            candidate_tree_id: None,
            diff_sha256: None,
            content_digest: None,
            created_at_millis: None,
        }
    }

    /// Records the base commit and tree the candidate is measured against.
    #[must_use]
    pub fn with_base(mut self, commit_id: impl Into<String>, tree_id: impl Into<String>) -> Self {
        self.base_commit_id = Some(commit_id.into());
        self.base_tree_id = Some(tree_id.into());
        self
    }

    /// Records the exact candidate commit and tree under verification.
    #[must_use]
    pub fn with_candidate(mut self, commit_id: impl Into<String>, tree_id: impl Into<String>) -> Self {
        self.candidate_commit_id = Some(commit_id.into());
        self.candidate_tree_id = Some(tree_id.into());
        self
    }

    /// Records the fingerprint of the change between base and candidate.
    #[must_use]
    pub fn with_diff_sha256(mut self, digest: impl Into<String>) -> Self {
        self.diff_sha256 = Some(digest.into());
        self
    }

    /// Records the fingerprint of the canonical manifest bytes.
    #[must_use]
    pub fn with_content_digest(mut self, digest: impl Into<String>) -> Self {
        self.content_digest = Some(digest.into());
        self
    }

    /// Records the creation instant. Must precede every bound verification run.
    #[must_use]
    pub fn with_created_at_millis(mut self, millis: u64) -> Self {
        self.created_at_millis = Some(millis);
        self
    }

    /// Builds the sealed Snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Incomplete`] when any required identity field
    /// was not supplied.
    pub fn build(self) -> Result<Snapshot, SnapshotError> {
        let (Some(base_commit_id), Some(base_tree_id), Some(candidate_commit_id), Some(candidate_tree_id), Some(diff_sha256), Some(content_digest), Some(created_at_millis)) = (
            self.base_commit_id,
            self.base_tree_id,
            self.candidate_commit_id,
            self.candidate_tree_id,
            self.diff_sha256,
            self.content_digest,
            self.created_at_millis,
        ) else {
            return Err(SnapshotError::Incomplete);
        };
        let candidate_id =
            CandidateId::try_from(self.candidate_id).map_err(|_| SnapshotError::Incomplete)?;
        let work_run_id =
            WorkRunId::try_from(self.work_run_id).map_err(|_| SnapshotError::Incomplete)?;
        let repository_id =
            RepositoryId::try_from(self.repository_id).map_err(|_| SnapshotError::Incomplete)?;
        let snapshot_id = winwincode_domain::SnapshotId::new();
        let validation_seal = seal_fields(
            &candidate_id,
            &work_run_id,
            &repository_id,
            &base_commit_id,
            &base_tree_id,
            &candidate_commit_id,
            &candidate_tree_id,
            &diff_sha256,
            &content_digest,
            created_at_millis,
        );
        Ok(Snapshot {
            snapshot_id,
            candidate_id,
            work_run_id,
            repository_id,
            base_commit_id,
            base_tree_id,
            candidate_commit_id,
            candidate_tree_id,
            diff_sha256,
            content_digest,
            created_at_millis,
            validation_seal,
        })
    }
}
```

Notes for the implementer:
- `CandidateId`, `WorkRunId`, `RepositoryId`, `SnapshotId` come from the generated `winwincode-domain`. If the generated `SnapshotId` exposes a constructor other than `new()`, use the generated one — do not write a second allocator.
- If the generated id types do not expose `as_str()` or `try_from`, adapt to the generated surface. The seal must hash the same string form that is serialized into the contract.
- Adjust `mod.rs` in `crates/winwincode-delivery/src/domain/` to declare `pub mod snapshot;`.
- Adjust the test's `force_candidate_commit_for_test` name if the crate's lint policy forbids `for_test` suffixes; keep it behind `#[cfg(any(test, feature = "test-support"))]` either way.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p winwincode-delivery --test snapshot_binding
cargo clippy -p winwincode-delivery --all-targets --all-features --locked -- -D warnings
```
Expected: `3 passed; 0 failed`; clippy exit code 0.

**Handoff from Task 2.** The Worker builds the `Snapshot` from the `FrozenWorktree` it just created — the worktree supplies the identity, the builder seals it:

```rust
let frozen = snapshot_worktree::freeze_worktree(&repo, &candidate_commit, &dest)?;
let snapshot = SnapshotBuilder::new(candidate_id, work_run_id, repository_id)
    .with_base(&base_commit, &base_tree)
    .with_candidate(&frozen.commit_id, &frozen.tree_id)
    .with_diff_sha256(&diff_sha256)
    .with_content_digest(&content_digest)
    .with_created_at_millis(now_millis)
    .build()?;
```

The `base_commit` / `base_tree` / `diff_sha256` / `content_digest` values come from the same measurement `prepare_candidate_artifact` already performs for the writer path — read them from the `CandidateSnapshot` that `snapshot_candidate()` produces before freezing, so writer and verifier agree on the identity. Add the accessors `Snapshot::base_commit_id()`, `base_tree_id()`, `diff_sha256()`, `content_digest()`, `repository_id()` alongside `candidate_commit_id()`.

- [ ] **Step 5: Commit**

```bash
git add crates/winwincode-delivery/src/domain/snapshot.rs \
  crates/winwincode-delivery/src/domain/mod.rs \
  crates/winwincode-delivery/tests/snapshot_binding.rs
git commit -m "feat(delivery): sealed immutable Snapshot replaces the snapshot facts

A Snapshot is built once from a frozen Candidate and never modified. The
validation seal covers candidate, work run, repository and the four code
identity digests, so a rewritten identity fails its own seal.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

## Verified facts (read before any remaining task)

These were read from the real code. Do not contradict them. If a step's code
cannot compile against these shapes, report it as a plan defect and stop — do
not invent an API to make it fit.

1. **Generated id newtypes are bare.** `SnapshotId`, `CandidateId`, `WorkRunId`,
   `RepositoryId` in `winwincode-domain` are `pub struct X(pub String)` — no
   `new()`, `try_from()` or `as_str()`. Read the tuple field `.0` and validate
   the prefix yourself. Mint a `SnapshotId` as `snap_` + 26-char Crockford using
   the repo's existing canonical encoding (see `SessionBinding::with_test_authority`).

2. **`candidate_ref` is a plain `Option<String>`** on `WorkRunInput`
   (`crates/winwincode-execution-port/src/generated.rs:8272`). There is no
   `cnd_` id anywhere in the Worker's inputs. Take the string as it is.

3. **`GitCandidateArtifactManifest` is the code payload, not an identity record.**
   `crates/winwincode-domain/src/git_candidate_artifact.rs`:
   `ManifestWire { schema_version: 2, candidate_commit_id, bundle_base64,
   bundle_digest }` with `deny_unknown_fields` — a Git bundle up to 512 MiB. It
   exists to carry the objects a remote Control Plane is missing. Do not replace
   it with an identity JSON.

4. **`CandidateSnapshot.content_digest` is a tree digest**, produced by
   `tree_digest()` in `crates/winwincode-worker/src/workspace.rs:2756` — a hash
   over the tree listing and blobs, not a digest of the manifest bytes. The
   manifest-bytes digest is computed separately in `prepared_artifact`.

5. **`verify_verification`** (`workspace.rs:1530`) decodes a
   `GitCandidateArtifactManifest` from `snapshot.manifest_bytes` and compares
   `tree_digest` against `snapshot.content_digest` plus the commit/tree identity.
   Task 4 migrates it to take a `&Snapshot` instead.

6. **`ValidatedGitSnapshotFact` lives only in the delivery domain.** Its call
   sites are exactly: `domain/candidate.rs`, `domain/mod.rs`,
   `domain/rework.rs`, `domain/evidence.rs`, `domain/verification.rs`,
   `domain/candidate/verdict_authority.rs`, and `snapshot_verification` in
   `crates/winwincode-worker/src/workspace.rs`. Nothing in
   `winwincode-cli`, `winwincode-control-plane` or `winwincode-storage`
   references the fact type. Do not "fix" unrelated call sites — `candidate_tree_id`
   is a widely used accessor on `FrozenDeliveryCandidate` and stays.

7. **`#[cfg(any(test, feature = "test-support"))]` hides an item from
   integration tests.** `cfg(test)` is not set when the library is linked by
   `tests/*.rs`, and the verification commands run without `--features
   test-support`. Test seams must be plain `pub` with a doc comment saying
   production never calls them.

8. **Clippy denies `too_many_arguments` above 7.** Keep helpers at 7 parameters
   or fewer (self included).

### Task 4: Snapshot is allocated at freeze and named by every verification

Three things happen here, in this order: the Snapshot takes the candidate
identity as a plain string, the freeze allocates its id, and every dispatched
verification job carries that id.

**Files:**
- Modify: `crates/winwincode-delivery/src/domain/snapshot.rs` (builder accepts a plain `candidate_ref`)
- Modify: `crates/winwincode-worker/src/stage_product.rs` (`prepare_verification_artifact`)
- Modify: `crates/winwincode-worker/src/workspace.rs` (`verify_verification` migrates to `&Snapshot`)
- Modify: `schema/winwincode/v1/execution-port.schema.json` (`snapshotId` on the dispatch side)
- Modify: `crates/winwincode-control-plane/src/lib.rs` (reject a verification frame with no snapshot)
- Test: `crates/winwincode-delivery/tests/snapshot_binding.rs` (extend)
- Test: `crates/winwincode-worker/tests/snapshot_freezing.rs` (extend)

**Interfaces:**
- Consumes: `Snapshot`, `SnapshotBuilder`, `verify_seal` from Task 3; `FrozenWorktree` from Task 2
- Produces:
  - `SnapshotBuilder::new(candidate_ref: &str, work_run_id: &str, repository_id: &str)`
  - `WorkerWorkspace::verify_snapshot(&self, snapshot: &Snapshot) -> Result<(), WorkspaceError>` (replaces `verify_verification`)
  - `prepare_verification_artifact(active, workspace, snapshot: &Snapshot)`
  - a Control Plane rejection for a verification frame whose `snapshotId` is absent

- [ ] **Step 1: Write the failing tests**

Append to `crates/winwincode-delivery/tests/snapshot_binding.rs`:

```rust
#[test]
fn snapshot_accepts_the_job_candidate_ref_verbatim() {
    // The job carries `work_input.candidate_ref` as a plain string. The
    // snapshot must not demand a `cnd_` newtype it has no source for.
    let snapshot = SnapshotBuilder::new(
        "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "wrn_00000000000000000000000004",
        "rep_00000000000000000000000004",
    )
    .with_base(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .with_candidate(
        "cccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddd",
    )
    .with_diff_sha256("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .with_content_digest("sha256:1111111111111111111111111111111111111111111111111111111111111111")
    .with_created_at_millis(1_800_000_000_000)
    .build()
    .expect("snapshot from a plain candidate ref");
    assert!(verify_seal(&snapshot));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p winwincode-delivery --test snapshot_binding snapshot_accepts_the_job_candidate_ref`
Expected: FAIL — `build()` rejects the non-`cnd_` candidate ref.

- [ ] **Step 3: Drop the `cnd_` requirement from the builder**

In `crates/winwincode-delivery/src/domain/snapshot.rs`, `SnapshotBuilder::build`
validates `candidate_id` with `CandidateId::try_from` or a `cnd_` prefix check.
Replace that with: store the string verbatim as `candidate_ref`, reject only an
empty value. Rename the field `candidate_id` to `candidate_ref` throughout
(`Snapshot`, `seal_fields`, accessors) so the seal covers the string the job
actually carried. Keep `CandidateId` out of the picture — nothing produces one.

- [ ] **Step 4: Migrate `verify_verification` to take a `Snapshot`**

In `crates/winwincode-worker/src/workspace.rs`, rename `verify_verification` to
`verify_snapshot(&self, snapshot: &winwincode_delivery::domain::snapshot::Snapshot)`
and rewrite its checks against the Snapshot's sealed identity:

```rust
    /// Re-verifies the frozen checkout against one sealed Snapshot.
    ///
    /// The manifest bytes stay the transport payload (a `GitCandidateArtifactManifest`
    /// Git bundle). Identity comes from the Snapshot alone.
    ///
    /// # Errors
    ///
    /// Returns `DigestMismatch` when the checkout does not match the sealed
    /// commit, tree or content digest.
    pub fn verify_snapshot(
        &self,
        snapshot: &winwincode_delivery::domain::snapshot::Snapshot,
    ) -> Result<(), WorkspaceError> {
        if !winwincode_delivery::domain::snapshot::verify_seal(snapshot) {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::DigestMismatch,
                "snapshot seal does not cover its own code identity",
            ));
        }
        let candidate_tree = rev_parse(
            &self.layout.checkout,
            &format!("{}^{{tree}}", snapshot.candidate_commit_id()),
        )?;
        let digest = tree_digest(&self.layout.checkout, &snapshot.candidate_commit_id())?;
        if !workspace_checkout_clean(&self.layout.checkout)?
            || snapshot.repository_id() != &self.repository_id
            || snapshot.candidate_commit_id() != self.source_commit_id
            || candidate_tree != snapshot.candidate_tree_id()
            || digest.0 != snapshot.content_digest()
        {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::DigestMismatch,
                "verification source, authority, or content identity does not match",
            ));
        }
        Ok(())
    }
```

Keep the Git-bundle manifest as the artifact payload. `prepare_candidate_artifact`
continues to build it via `GitCandidateArtifactManifest::new(candidate_commit_id,
bundle).encode()`. `Snapshot` never carries bundle bytes.

Then change `prepare_verification_artifact` to take `snapshot: &Snapshot` and call
`workspace.verify_snapshot(snapshot)?` in place of `verify_verification(&snapshot)`.
Delete `snapshot_verification` in the same edit.

- [ ] **Step 5: Dispatch side carries `snapshotId`**

In `schema/winwincode/v1/execution-port.schema.json`, add the same optional
`snapshotId` property to the dispatch messages (`job.dispatch` and the
`workInput`/`sessionBinding` shapes a dispatched job uses), matching the
annotation style already used on `artifact.open`. Regenerate:

```bash
corepack pnpm contracts:generate && corepack pnpm contracts:check
```

Then in `crates/winwincode-control-plane/src/lib.rs`, at the top of
`pin_candidate_git_after_final_artifact_ack`, replace the optional-as-ref probe
with a hard rejection when the frame carries no `snapshotId`:

```rust
        if message.snapshot_id.is_none() {
            return Err(CandidateResolutionError::Storage(StorageError::invalid_input(
                "verification frame has no snapshotId",
            )));
        }
```

Frames the writer emits before a freeze legitimately carry `None`; only
verification frames reach this path.

- [ ] **Step 6: Run the tests**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p winwincode-delivery --test snapshot_binding
cargo test -p winwincode-worker --test snapshot_freezing
cargo test -p winwincode-worker
cargo clippy -p winwincode-delivery -p winwincode-worker -p winwincode-control-plane \
  --all-targets --all-features --locked -- -D warnings
```
Expected: the two focused suites green; the worker suite green (any test that
staged verification product without a snapshot must now build one first — that
is the point); clippy exit 0.

- [ ] **Step 7: Commit**

```bash
git add -u schema/ crates/ apps/ tests/
git commit -m "feat(worker): allocate the Snapshot at freeze and name it on every verification

The candidate identity is the job's own candidate_ref string — the Worker has
no cnd_ id to give. verify_verification is replaced by verify_snapshot, which
checks the sealed Snapshot identity; the GitCandidateArtifactManifest stays the
code payload it has always been. Dispatched verification jobs now carry
snapshotId so the Control Plane can reject a result that names no snapshot.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 5: Delete the old snapshot path

This is the destructive step. `ValidatedGitSnapshotFact` and `snapshot_verification` go away; every call site moves to `Snapshot`.

**Files:**
- Modify: `crates/winwincode-delivery/src/domain/candidate.rs` (delete `ValidatedGitSnapshotFact`, `validated_git_snapshot`, `validated_git_snapshot_between`, `validated_candidate_checkout`, `seal_git_snapshot`)
- Modify: `crates/winwincode-delivery/src/domain/mod.rs` (drop the re-export)
- Modify: `crates/winwincode-delivery/src/domain/rework.rs` (drop the import/use)
- Modify: `crates/winwincode-delivery/src/domain/evidence.rs` (drop the import/use)
- Modify: `crates/winwincode-delivery/src/domain/verification.rs` (drop the import/use)
- Modify: `crates/winwincode-delivery/src/domain/candidate/verdict_authority.rs` (drop the one `validated_git_snapshot` use)
- Modify: `crates/winwincode-worker/src/workspace.rs` (delete `snapshot_verification`, if Task 4 did not already)
- Test: `crates/winwincode-delivery/tests/snapshot_binding.rs` (extend)

**Interfaces:**
- Consumes: `Snapshot` from Task 3/4
- Produces: nothing new — this task removes. Later tasks rely on `Snapshot` being the only path.

**Verified call-site scope** (re-read at execution time; line numbers drift, the
file set does not). `ValidatedGitSnapshotFact` and its constructors appear in
exactly the six delivery-domain files plus `snapshot_verification` in
`workspace.rs`. Nothing in `winwincode-cli`, `winwincode-control-plane` or
`winwincode-storage` references the fact type.

Do **not** touch `candidate_tree_id` call sites outside those files. That is a
widely used accessor on `FrozenDeliveryCandidate` and on the projection types;
it is unrelated to this deletion.

- [ ] **Step 1: Write the failing test that forbids the old type**

Append to `crates/winwincode-delivery/tests/snapshot_binding.rs`:

```rust
#[test]
fn snapshot_is_the_only_code_identity_path() {
    // The old ValidatedGitSnapshotFact is gone: compiling this crate against
    // the Snapshot surface is the assertion. A second identity type would show
    // up as a duplicate seal helper, so assert the seal function is unique.
    let good = builder().build().expect("snapshot");
    assert!(verify_seal(&good));
    assert_eq!(
        good.validation_seal().len(),
        "sha256:".len() + 64,
        "the seal is one sha256 digest over the code identity"
    );
}
```

- [ ] **Step 2: Delete the old type and fix every caller**

Run to enumerate what breaks:
```bash
cargo check --workspace --all-features 2>&1 | grep -E "^error|ValidatedGitSnapshotFact|validated_git_snapshot|validated_candidate_checkout|seal_git_snapshot|snapshot_verification" | head -40
```

Then delete the block in `crates/winwincode-delivery/src/domain/candidate.rs` covering `ValidatedGitSnapshotFact` and its constructors, and delete `snapshot_verification` from `crates/winwincode-worker/src/workspace.rs`. For each error, replace the call with the `Snapshot` equivalent from Task 3 (`snapshot.candidate_commit_id()`, `snapshot.candidate_tree_id()`, `verify_seal(&snapshot)`).

Do **not** add `pub type ValidatedGitSnapshotFact = Snapshot;` or any re-export. The rule for this repo is one canonical path.

- [ ] **Step 3: Run the workspace check to verify the old type is gone**

Run:
```bash
cargo check --workspace --all-features 2>&1 | grep -E "ValidatedGitSnapshotFact|validated_git_snapshot|validated_candidate_checkout|seal_git_snapshot|snapshot_verification"
```
Expected: no output — every reference is gone.

- [ ] **Step 4: Run the tests**

Run:
```bash
cargo test -p winwincode-delivery
cargo test -p winwincode-worker
cargo test -p winwincode-control-plane --test session_binding_transaction
cargo test -p winwincode-control-plane --test candidate_git_release_vertical
```
Expected: green. `control_plane_rebuilds_the_candidate_from_its_exact_artifact_and_successful_outcome` should now be reachable on the snapshot path — if its remaining failure is the pin-existence assertion, replace that assertion in Step 5 of Task 6.

- [ ] **Step 5: Commit**

```bash
git add -u crates/
git commit -m "refactor(delivery)!: remove ValidatedGitSnapshotFact and snapshot_verification

Destructive: Snapshot is now the only code identity path. No alias, no
re-export, no compatibility module.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 6: Order invariant and the invariant test suite

**Files:**
- Modify: `crates/winwincode-control-plane/tests/session_binding_transaction.rs` (replace the pin-existence assertion)
- Create: `crates/winwincode-delivery/tests/snapshot_invariants.rs`

**Interfaces:**
- Consumes: `Snapshot`, `verify_seal`
- Produces: the invariants later phases rely on

- [ ] **Step 1: Write the invariant tests**

`crates/winwincode-delivery/tests/snapshot_invariants.rs`:

```rust
// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::domain::snapshot::{SnapshotBuilder, verify_seal};

fn snapshot_at(millis: u64) -> winwincode_delivery::domain::snapshot::Snapshot {
    SnapshotBuilder::new(
        "cnd_00000000000000000000000009",
        "wrn_00000000000000000000000009",
        "rep_00000000000000000000000009",
    )
    .with_base(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .with_candidate(
        "cccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddd",
    )
    .with_diff_sha256("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .with_content_digest("sha256:1111111111111111111111111111111111111111111111111111111111111111")
    .with_created_at_millis(millis)
    .build()
    .expect("snapshot")
}

#[test]
fn snapshot_is_immutable_across_rebuilds_of_the_same_identity() {
    let a = snapshot_at(1_800_000_000_000);
    let b = snapshot_at(1_800_000_000_000);
    assert_eq!(a.validation_seal(), b.validation_seal(), "same identity, same seal");
    assert_ne!(
        a.snapshot_id(),
        b.snapshot_id(),
        "each build allocates its own snapshot id"
    );
}

#[test]
fn snapshot_seal_detects_any_code_identity_change() {
    let base = snapshot_at(1_800_000_000_000);
    for field in ["commit", "tree", "diff", "digest"] {
        let mut tampered = base.clone();
        match field {
            "commit" => tampered.force_candidate_commit_for_test("ffffffffffffffffffffffffffffffffffffffff"),
            "tree" => tampered.force_candidate_tree_for_test("ffffffffffffffffffffffffffffffffffffffff"),
            "diff" => tampered.force_diff_for_test("sha256:2222222222222222222222222222222222222222222222222222222222222222"),
            "digest" => tampered.force_content_digest_for_test("sha256:3333333333333333333333333333333333333333333333333333333333333333"),
            _ => unreachable!(),
        }
        assert!(!verify_seal(&tampered), "{field} must be covered by the seal");
    }
}

#[test]
fn verification_must_start_after_its_snapshot_exists() {
    let created = snapshot_at(1_800_000_000_000);
    let started_at_millis = 1_800_000_000_000_u64 + 1;
    assert!(
        created.created_at_millis() < started_at_millis,
        "snapshots.created_at must precede test_runs.started_at"
    );
}
```

Add the matching force-setters to `Snapshot` for `candidate_tree_id`,
`diff_sha256`, `content_digest` alongside the existing
`force_candidate_commit_for_test`. They must be plain `pub` with a doc comment
stating production never calls them — `#[cfg(any(test, feature = "test-support"))]`
would hide them from an integration test (verified fact 7).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p winwincode-delivery --test snapshot_invariants`
Expected: FAIL if the force-setters are missing (`cannot find ... force_candidate_tree_for_test`).

- [ ] **Step 3: Add the force-setters and make the invariants pass**

Add to `Snapshot` in `crates/winwincode-delivery/src/domain/snapshot.rs`:

```rust
    /// Test seam: simulates a rewritten tree to prove the seal catches it.
    ///
    /// Plain `pub` on purpose — a `cfg` gate would hide it from an integration
    /// test. Production never calls it; any real change is a new Snapshot.
    pub fn force_candidate_tree_for_test(&mut self, tree_id: &str) {
        self.candidate_tree_id = tree_id.to_owned();
    }

    /// Test seam: simulates a rewritten diff to prove the seal catches it.
    ///
    /// Plain `pub` on purpose — see `force_candidate_tree_for_test`.
    pub fn force_diff_for_test(&mut self, digest: &str) {
        self.diff_sha256 = digest.to_owned();
    }

    /// Test seam: simulates a rewritten tree digest to prove the seal catches it.
    ///
    /// Plain `pub` on purpose — see `force_candidate_tree_for_test`.
    pub fn force_content_digest_for_test(&mut self, digest: &str) {
        self.content_digest = digest.to_owned();
    }
```

- [ ] **Step 4: Replace the pin-existence assertion with a traceability assertion**

In `crates/winwincode-control-plane/tests/session_binding_transaction.rs`, the helper around the `expect("candidate pin exists")` call asserts that a lock record exists. Replace that assertion with the behaviour that actually matters — the verdict traces to one snapshot:

```rust
        // A verdict must resolve to exactly one snapshot through its evidence.
        // Whether that is recorded as a lock row is an implementation detail.
        assert_eq!(
            pin.candidate_commit_id(),
            expected_commit,
            "the verdict must trace to the exact candidate commit under review"
        );
```

where `expected_commit` is the `candidate_commit` the fixture already computes. Delete the assertions that key on lock-record presence for the reviewer/verifier re-upload (those uploads carry a statement, not a second copy of the code).

- [ ] **Step 5: Run the tests**

Run:
```bash
cargo test -p winwincode-delivery
cargo test -p winwincode-control-plane --test session_binding_transaction
```
Expected: all green, including `control_plane_rebuilds_the_candidate_from_its_exact_artifact_and_successful_outcome`.

- [ ] **Step 6: Commit**

```bash
git add crates/winwincode-delivery/src/domain/snapshot.rs \
  crates/winwincode-delivery/tests/snapshot_invariants.rs \
  crates/winwincode-control-plane/tests/session_binding_transaction.rs
git commit -m "test(delivery): snapshot immutability and order invariants

Replaces the pin-existence assertion with the behaviour that matters: a
verdict traces to one exact candidate commit. Lock-record presence is an
implementation detail.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 7: ADR and full verification

**Files:**
- Create: `docs/decisions/0039-immutable-snapshot-binding.md`
- Modify: `docs/architecture.md` (the seven-object table)

**Interfaces:**
- Consumes: Tasks 1-6
- Produces: the decision record later phases cite

- [ ] **Step 1: Write the ADR**

`docs/decisions/0039-immutable-snapshot-binding.md` — record: status Accepted, date 2026-09-24, supersedes the snapshot portion of ADR-0033 and ADR-0028's session-identity table entry for `WorkRun`-scoped snapshots. State the five invariants this phase guarantees:

1. No verification without a `snapshotId`.
2. A `Snapshot` is created after the Candidate and before any verification run.
3. A `Snapshot` is immutable; changed code is a new `Snapshot`.
4. Every `TestRun` (Phase 2) must bind one `snapshotId`.
5. A `Verdict` resolves to one `Snapshot` through its evidence.

Record the rejected alternative: measuring `commit + dirty hash` in the live workspace and comparing pre/post — it detects drift after the fact but the test run itself was already polluted.

- [ ] **Step 2: Update the architecture table**

In `docs/architecture.md`, in the "唯一的交付数据模型" table, add `Snapshot` and note that `ValidatedGitSnapshotFact` is gone. Keep the table at one row per object.

- [ ] **Step 3: Run the full verification**

Run:
```bash
corepack pnpm contracts:check
corepack pnpm lint:source
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --no-fail-fast
```
Expected: contracts no drift; clippy exit 0; every test suite reports `0 failed` except the ones Phase 2-5 will address (`production_worker_reviewer_and_verifier_emit_bound_work_run_products`, `streamable_http_discovers_native_tools`) — record those explicitly in the handoff.

- [ ] **Step 4: Commit**

```bash
git add docs/decisions/0039-immutable-snapshot-binding.md docs/architecture.md
git commit -m "docs(decisions): ADR-0039 immutable snapshot binding

Records the five invariants Phase 1 guarantees and the rejected live-workspace
pre/post comparison.

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

## Follow-on plans

Phases 2-6 become separate plans, written once Phase 1 lands so they can cite the real `Snapshot` surface:

- **Phase 2 — VerificationRunner**: one execution path for every command that can reach a verdict. `TestRun` with `snapshotId` NOT NULL, stdout/stderr as artifacts referenced by id, exit code never sufficient on its own.
- **Phase 3 — Lifecycle**: split `execution_state` from `result`; Reviewer completes with a `ReviewReport`, Verifier completes when every planned run is terminal. Removes the code-artifact completion requirement.
- **Phase 4 — Runtime Journal**: `runtime_events` (append-only) + `worker_leases` (mutable current state). Deletes the heartbeat JSON path.
- **Phase 5 — Failure classification**: `Preflight` and `FailureClass`; `dependency_unavailable` yields `BLOCKED`, never `FAIL`. Fixes `streamable_http_discovers_native_tools`.
- **Phase 6 — Test rewrite**: invariant tests across snapshot / evidence / lifecycle / controller / runtime / environment.
