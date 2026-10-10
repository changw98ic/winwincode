# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Contract

WinWinCode is a Node.js 24 ESM and Rust 1.95 workspace. The DSH chat surface is
the default UI, StrongFlow is the advanced UI, and both use one embedded Codex
Core execution kernel.

Supported release targets are `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, and
`x86_64-unknown-linux-gnu`. Do not add fallback execution through an installed
Codex CLI or another programming agent.

Project-owned code is Apache-2.0 only. Preserve mandatory third-party license
and notice files without presenting their licenses as a second WinWinCode
project license. Migrate old contracts into one canonical path rather than
adding compatibility copies.

Use strict TypeScript, ESM, Cargo workspace lints, exact external dependency
versions, workspace protocol for internal packages, and explicit package file
allowlists.

Keep TypeScript packages in `apps/` and `packages/`, Rust crates in `crates/`,
tests in `tests/`, upstream pins and patch records in `upstream/`, and accepted
architecture decisions in `docs/decisions/`. Generated dependency, build,
coverage, package, credential, and log files must remain ignored.

## Commands

All pnpm commands go through Corepack (`corepack pnpm …`) so the pinned
pnpm 11.7.0 is used. Toolchain pins: Node 24.19.0 (`.node-version`), Rust
1.95.0 with `rustfmt`/`clippy`/`rust-src` (`rust-toolchain.toml`).

```bash
corepack pnpm install --frozen-lockfile   # install (runs scripts/check-runtime.mjs)
corepack pnpm build                      # TypeScript build + Rust build
corepack pnpm typecheck                  # contracts drift check + strict tsc
corepack pnpm lint                       # source lint + typecheck + cargo clippy
corepack pnpm test                       # TypeScript tests, then Rust tests
corepack pnpm verify                     # full gate: source + TypeScript + Rust lanes
corepack pnpm start                      # run winwincode-server
```

`corepack pnpm verify` runs three lanes in sequence: `verify:source`
(format + source-boundary lint), `verify:typescript` (build, tests, product
checks), and `verify:rust` (build, clippy, workspace tests, product checks,
production vertical). CI splits these into parallel jobs that aggregate as
`Canonical workspace verification`.

### Narrower checks

```bash
corepack pnpm contracts:check            # generated contracts match schema/ (writes nothing)
corepack pnpm contracts:generate         # regenerate contracts from schema/
corepack pnpm format:check               # source format + cargo fmt --check
corepack pnpm lint:source                # workspace source-boundary lint only
corepack pnpm lint:rust                  # cargo clippy --workspace -D warnings
corepack pnpm check:no-absolute-paths
corepack pnpm verify:api-production-vertical   # end-to-end API vertical (needs built binaries)
```

### Running a single test

TypeScript tests are `node:test` files under `tests/`. Build first — suites
import generated contracts and `apps/client/dist`:

```bash
corepack pnpm build:ts
node --test tests/<name>.test.mjs
```

`scripts/run-ts-tests.mjs` runs a fixed canonical list with
`--test-concurrency=1`. A file runs in `pnpm test:ts` only if it is listed in
that array — add new canonical tests there.

Rust tests use the normal Cargo filter. Target one integration file, one crate,
or one function:

```bash
cargo test --workspace --all-features --locked <filter>
cargo test -p winwincode-control-plane --test lifecycle
cargo test -p winwincode-delivery --test workrun_replacement <test_fn_name>
```

`pnpm test:rust` builds `winwincode-kernel-helper` first, then runs the
workspace suite. `pnpm test:rust:built` reuses the existing `target/` build.

## Architecture

`docs/architecture.md` is the long form. The big picture that takes reading
several files to recover:

### Three ownership rules (the spine of the system)

1. Only `winwincode-control-plane` writes product state (ProductSession,
   Delivery, Approval, Attention, Provider/Credential references, Audit).
2. Only `winwincode-worker` reports execution facts (workspace, Job, Lease,
   candidate, artifacts, run events, result).
3. Only `winwincode-kernel` is authoritative for Codex execution facts
   (CodexThread, Turn, Plan, tool calls, sandbox, diff, usage).

Request path: `apps/client` → `winwincode-server` (auth + HTTP/WebSocket
boundary) → `winwincode-control-plane` → `ExecutionPort` → `winwincode-worker`
→ `winwincode-codex` → `winwincode-kernel`. `winwincode-local` only composes
Control Plane and Worker in one process for local deployment; remote deployment
replaces the transport, never the state semantics.

### Four session identities — never collapse them into one `session_id`

| Identity | Owner | Meaning |
| --- | --- | --- |
| `ProductSession` | control-plane | what the user sees and types into |
| `WorkerSession` | worker | one cancellable/resumable execution context |
| `CodexThread` | kernel | Codex conversation and turn history |
| `WorkRun` | Delivery | one leased attempt at one WorkItem |

`SessionBinding` links all four plus Delivery, WorkItem, Job, Lease and
Fencing facts. A retry creates attempt+1 with new WorkerSession and CodexThread;
older attempts stay zero-write.

### Contracts are generated, never hand-edited

`schema/winwincode/v1/*.schema.json` is the single source of truth.
`corepack pnpm contracts:generate` produces seven outputs: Rust domain types,
Rust API types, Rust ExecutionPort DTOs, TypeScript contracts, the TypeScript
Control Plane client, the schema collection, and OpenAPI 3.1. Never edit
`crates/*/src/generated.rs`, `apps/client/src/generated/`, or
`openapi.generated.json` by hand — `contracts:check` fails on any drift.

The public HTTP surface is deliberately tiny: `POST /api/v1/commands`,
`POST /api/v1/queries`, `GET|POST|DELETE /api/v1/auth/session`, plus a
WebSocket that only pushes projections and events (never a write channel).
`GeneratedContractDispatcher` in `crates/winwincode-server/src/dispatcher.rs`
is the only public request entry. `packages/control-plane-client` is the only
browser network facade.

### Delivery data model

Seven objects (ADR-0033): `WorkContract` (immutable revision), `WorkItem`
(schedulable unit in a dependency DAG), `WorkRun` (one attempt), `Candidate`
(immutable code output), `VerificationPlan`, `Evidence`, `Verdict`.
`submitVerdict()` recomputes the conclusion server-side — callers cannot
manufacture Evidence or Verdict, and agent text replies are never delivery
evidence. The preview uses `DELIVERY_SCHEMA_VERSION = 3`; CONTRIBUTING.md
defines the single migration path that applies after the first stable release.

### Client shape (ADR-0029)

`apps/client/src` is a composition-root `application.ts` plus a `core/` layer
(`control-plane-client.ts` is the sole network facade; `runtime-config.ts`
derives both transports from one `serverUrl`), feature view-models that build
read-only views from queries and WebSocket events, and `components/` of
business-ignorant DOM modules. Lists update through `keyed-collection.ts`
which preserves node identity — do not `replaceChildren()` on every event.
Pages never talk to Worker and never hold a second copy of delivery state.

### Fusion engine (ADR-0035 – 0038)

`crates/winwincode-fusion` and the control-plane investigation modules
implement claim-centric multi-model investigation, not voting. Hard rules:
additive by default (Oracle Union), disagreement triggers investigation rather
than elimination, consensus is metadata and never evidence, and a supported
claim may only be subtracted by a verified counter. Regression fixtures live in
`fusion-regression/` (its README defines the claim state machine and the
`mustNotBe` invariants); benchmark tasks in `fusion-benchmark-tasks/`. Treat
those ADRs and fixtures as the contract when touching adjudication or
convergence logic.

### Upstream Codex is pinned

`third_party/codex/` is built directly as the kernel. Identity, patches,
checksums and license duties are recorded in `upstream/sources.lock.json`.
Update one upstream source at a time following `docs/upstream-updates.md`, and
never keep two versions side by side in `third_party/` or `upstream/vendor/`.

## Beads workflow

Use the enabled `beads` skill for all project task tracking. The native
Codex hooks load the short workflow configured in `.beads/PRIME.md`; run
`bd prime` when this context is missing. Read the current task with `bd show <id>`
and relevant decisions with `bd memories <keyword>`.

Keep persistent project memory in bd, and task progress in the relevant issue.
Use the current user/repository instructions for execution authority; historical
notes do not grant authority. By default, report changes and checks without
committing, pushing, or syncing the remote. Close completed issues only after
their acceptance criteria pass; record remaining work before handoff.

Write the latest task progress note as a concise state snapshot using `Task`,
`Status`, `Workspace`, `Validation`, `Changes`, `Dependencies`, and `Unverified`.
Changes describe observed modifications with paths and result references;
Validation distinguishes implementation, command results, and independent
acceptance. Unverified records missing results rather than root-cause guesses
or next-step instructions. Keep durable decisions in bd memory. Preserve live
scope, authorization, assignments, and version bindings in their authoritative
task/runtime records so recovery can reload them separately. A snapshot does
not replace those records or grant authority.

Issues live in `.beads/dolt/`; remote sync uses `bd dolt push/pull` and
`refs/dolt/data`. `.beads/issues.jsonl` is a passive export, not the source of
truth or the normal synchronization input.

## Behavior changes and evidence

Change production code and add focused tests together. Do not paper over a
product result by editing fixtures, snapshots, or test doubles. New Evidence
must trace back to a command, test, diff, file, commit, run event, or
independent review finding. Report the checks actually run and the risks still
uncovered.

UI-versus-design-mock differences are judged by real rendering plus visual
review (2026-09-10 user ruling recorded in `scripts/run-ts-tests.mjs`) — do
not reintroduce fingerprint-style style assertions.

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var
