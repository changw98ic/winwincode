# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

## Repository Contract

WinWinCode is a Node.js 24 ESM and Rust 1.95 workspace. The DSH chat surface is
the default UI, StrongFlow is the advanced UI, and both use one embedded Codex
Core execution kernel.

Supported release targets are `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, and
`x86_64-unknown-linux-gnu`. Do not add fallback execution through an installed
Codex CLI or another programming agent.

Keep TypeScript packages in `apps/` and `packages/`, Rust crates in `crates/`,
tests in `tests/`, upstream pins and patch records in `upstream/`, and accepted
architecture decisions in `docs/decisions/`. Generated dependency, build,
coverage, package, credential, and log files must remain ignored.

Project-owned code is Apache-2.0 only. Preserve mandatory third-party license
and notice files without presenting their licenses as a second WinWinCode
project license. Migrate old contracts into one canonical path rather than
adding compatibility copies.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->


## Build & Test

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm typecheck
corepack pnpm test
corepack pnpm lint
corepack pnpm build
corepack pnpm verify
```

## Architecture Overview

The TypeScript host composes DSH UI and model services. A native Node boundary
embeds the Rust Codex Core kernel. The default chat UI and StrongFlow advanced
mode submit to that same kernel.

## Conventions & Patterns

Use strict TypeScript, ESM, Cargo workspace lints, exact external dependency
versions, workspace protocol for internal packages, and explicit package file
allowlists.
