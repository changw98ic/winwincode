# WinWinCode Upgrade and Recovery Drill

This crate runs deterministic rolling-upgrade and disaster-recovery drills. It
does not own health, drain, deployment, backup, or business state. Those facts
cross narrow ports and the only backup/restore implementation remains
`winwincode-backup`.

Canonical `ControlPlaneInstanceHealth` cuts map directly into drill
observations, so upgrades reuse the sole durable lease/drain authority.

Every successful run produces canonical digest-bound evidence for the exact
tenant, approved source, backup generation, confirmed-state boundary, RPO, and
RTO. An incomplete drain, altered manifest, cross-tenant fact, failed rollback,
or failed integrity check stops before activation.
