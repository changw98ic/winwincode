# WinWinCode CLI

`wwc` is the canonical command for local repository setup, diagnostics, and
Owner user administration:

```text
wwc init [PATH] [--confirm-git-init] [--baseline head|snapshot|cancel] [--confirm-snapshot]
wwc repo attach [PATH] [--baseline head|snapshot|cancel] [--confirm-snapshot]
wwc doctor [PATH]
wwc user create <USERNAME> [--role owner|member] [--data-dir PATH]
wwc user disable <USERNAME> [--data-dir PATH]
wwc user enable <USERNAME> [--data-dir PATH]
wwc user reset-password <USERNAME> [--data-dir PATH]
```

## User administration

The `wwc user` commands reuse the canonical `UserAccountService` on the
Server product-state database. `--data-dir` (or `WWC_SERVER_DATA_DIRECTORY`)
must point at the same directory the Server uses.

- Temporary passwords are random, shown exactly once, and never stored or
  repeated by any later command.
- `--role owner` is accepted only while the directory has no Owner account;
  the Server's browser one-time bootstrap stays the other initialization
  path. `--role member` (the default) requires an existing Owner.
- Disabling an account never touches browser sessions: session revocation
  is a Server responsibility. When a Server shares this data directory, a
  CLI disable reaches already logged-in browsers only after a Server
  restart or through an HTTP management endpoint.
- While no Owner exists, every other `wwc user` command exits with the
  initialization guidance instead of writing accounts.

The command parser performs no Git or filesystem work directly. It calls the
`LocalLauncherPort`; the system adapter is the only local side-effect gateway.
Git initialization and snapshot creation require explicit flags. Snapshot
commits use a dedicated `refs/winwincode/snapshots/*` reference and a temporary
index, so the current branch, worktree index, and stash remain unchanged.

Repository bindings are stored outside the repository under the configured
WinWinCode state root. They contain an exact baseline SHA, canonical local path,
baseline source, and only whether a remote exists. Remote URLs, environment
values, provider keys, and other secrets are never persisted.
