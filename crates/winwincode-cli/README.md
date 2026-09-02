# WinWinCode CLI

`wwc` is the canonical command for local repository setup and diagnostics:

```text
wwc init [PATH] [--confirm-git-init] [--baseline head|snapshot|cancel] [--confirm-snapshot]
wwc repo attach [PATH] [--baseline head|snapshot|cancel] [--confirm-snapshot]
wwc doctor [PATH]
```

The command parser performs no Git or filesystem work directly. It calls the
`LocalLauncherPort`; the system adapter is the only local side-effect gateway.
Git initialization and snapshot creation require explicit flags. Snapshot
commits use a dedicated `refs/winwincode/snapshots/*` reference and a temporary
index, so the current branch, worktree index, and stash remain unchanged.

Repository bindings are stored outside the repository under the configured
WinWinCode state root. They contain an exact baseline SHA, canonical local path,
baseline source, and only whether a remote exists. Remote URLs, environment
values, provider keys, and other secrets are never persisted.
