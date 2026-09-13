# Agent Instructions

## Repository Contract

WinWinCode is a Node.js 24 ESM and Rust 1.95 workspace. The DSH chat surface is
the default UI, StrongFlow is the advanced UI, and both use one embedded Codex
Core execution kernel.

Supported release targets are `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, and
`x86_64-unknown-linux-gnu`. Do not add fallback execution through an installed
Codex CLI or another programming agent.

Use these repository entry points:

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm typecheck
corepack pnpm test
corepack pnpm lint
corepack pnpm build
corepack pnpm verify
```

Keep TypeScript packages in `apps/` and `packages/`, Rust crates in `crates/`,
tests in `tests/`, upstream pins and patch records in `upstream/`, and accepted
architecture decisions in `docs/decisions/`. Generated dependency, build,
coverage, package, credential, and log files must remain ignored.

Project-owned code is Apache-2.0 only. Preserve mandatory third-party license
and notice files without presenting their licenses as a second WinWinCode
project license. Migrate old contracts into one canonical path rather than
adding compatibility copies.

Use strict TypeScript, ESM, Cargo workspace lints, exact external dependency
versions, workspace protocol for internal packages, and explicit package file
allowlists.

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

Issues live in `.beads/dolt/`; remote sync uses `bd dolt push/pull` and
`refs/dolt/data`. `.beads/issues.jsonl` is a passive export, not the source of
truth or the normal synchronization input.

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
