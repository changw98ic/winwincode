# WinWinCode Object Store

Verified-TLS S3-compatible byte storage for the canonical `ArtifactStore`.
The crate owns no Artifact metadata or tenant authorization; those remain in
the Control Plane catalog. It also exposes the secret-free `ArtifactObjects`
snapshot source consumed by the canonical backup manifest.

Every write requires an `aws:kms` server-side-encryption receipt. The KMS key
reference, workload credential, bucket, prefix, and object bytes never enter
the backup manifest or adapter diagnostics.
