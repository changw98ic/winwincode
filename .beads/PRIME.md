# Beads workflow

Beads is the source of truth for project tasks and durable decisions. This file
configures the startup reminder; task progress and project memory belong in bd.

- Read the requested task with `bd show <id>`. Use `bd ready --json` when the user
  asks for available work; keep the current request's scope.
- Create an issue before implementation and claim it with `bd update <id> --claim`.
  Record progress, evidence, and follow-up work in the relevant issue.
- After context recovery, reload the active issue. Retrieve relevant decisions
  with `bd memories <keyword>` rather than loading the entire memory collection.
  Historical notes describe past work; check the current issue and active
  instructions for ownership, status, and execution permissions.
- Store lasting decisions with `bd remember --key <key> "decision"`; update the
  same key when that decision changes. Keep task progress in issue notes.
- Run the checks relevant to the change. Close an issue only after its acceptance
  criteria pass; report changed files, validation, and remaining work at handoff.
- Follow current user and repository authority for commits, pushes, and remote
  sync. The default is local work and a handoff, with no automatic remote sync.
- Issues live in `.beads/dolt/`. `.beads/issues.jsonl` is a passive export;
  cross-machine sync uses `bd dolt push/pull` and `refs/dolt/data`.
