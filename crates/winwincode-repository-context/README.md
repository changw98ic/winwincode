# WinWinCode Repository Context

This crate exposes the read-only `RepositoryContextPort`. A query names an
exact Git commit SHA, and every reported fact comes from that commit rather
than from mutable worktree files.

The local code-index result always states its mode, freshness, baseline, and
capabilities. `ast-grep-outline` may claim symbol outlines. The
`git-file-inventory` fallback only claims file paths, languages, sizes, and Git
object fingerprints; it never claims symbol, caller, callee, dependency-graph,
or test-relation coverage.

`RepositoryContextScanner` can use a configured `LocalCodeIndexPort`. A stale
configured index is refreshed once outside the repository and checked again.
If no index command exists or freshness still cannot be proven, the scanner
returns a fresh baseline-bound file inventory with an explicit fallback
reason. It does not start a daemon or create `.codegraph`, PID, or socket
state.
