# WinWinCode Backup

This crate owns the single enterprise backup-manifest format and the sealed
restore/deletion boundaries. A manifest contains only tenant identities,
checkpoints, counts, and SHA-256 digests. Secret values have no representation.

Restore is fail-closed: every required subsystem is verified against one
consistency cut before a target can receive the sealed restore authorization.
The target keeps its previous active generation until atomic activation.
Deletion is possible only through the canonical Audit data-governance authority;
retention and legal holds are evaluated before the storage port is called.
