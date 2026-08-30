// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use winwincode_audit::AuditScope;
use winwincode_domain::Sha256Digest;

use crate::{
    BackupComponentKind, BackupComponentSnapshot, BackupDependency, BackupError, BackupId,
    BackupManifest, MAX_SAFE_INTEGER, manifest::validate_digest,
};

/// Sealed fixed-cut request passed to every backend snapshot adapter.
#[derive(Clone, Debug)]
pub struct BackupSnapshotRequest {
    scope: AuditScope,
    consistency_cut_digest: Sha256Digest,
    captured_at_millis: u64,
}

impl BackupSnapshotRequest {
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn consistency_cut_digest(&self) -> &Sha256Digest {
        &self.consistency_cut_digest
    }

    #[must_use]
    pub const fn captured_at_millis(&self) -> u64 {
        self.captured_at_millis
    }
}

/// Stable source failure without database, object-store, Vault, key, or secret
/// diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupSnapshotSourceError;

impl BackupSnapshotSourceError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for BackupSnapshotSourceError {
    fn default() -> Self {
        Self::new()
    }
}

/// One authoritative subsystem snapshot adapter. `SecretStore` implementations
/// emit only `SecretReferences` encrypted-envelope/reference digests and counts;
/// this interface has no field for plaintext or customer key bytes.
pub trait BackupSnapshotSource {
    #[must_use]
    fn kind(&self) -> BackupComponentKind;

    /// Creates or exactly replays an immutable snapshot for the supplied cut.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe backend failure.
    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError>;
}

/// Canonical capture coordinator for `PostgreSQL`, object storage, and
/// `SecretStore` reference/encrypted-envelope adapters.
pub struct BackupCaptureCoordinator;

impl BackupCaptureCoordinator {
    /// Captures every required backend in deterministic component order and
    /// constructs the only canonical manifest/dependency graph.
    ///
    /// # Errors
    ///
    /// Rejects missing/duplicate adapters before any snapshot call. A backend
    /// mismatch or failure produces no manifest.
    #[allow(clippy::too_many_arguments)]
    pub fn capture(
        backup_id: BackupId,
        scope: AuditScope,
        resident_region: &str,
        captured_at_millis: u64,
        consistency_cut_digest: Sha256Digest,
        sources: &mut [&mut dyn BackupSnapshotSource],
    ) -> Result<BackupManifest, BackupError> {
        validate_digest(&consistency_cut_digest)?;
        if captured_at_millis == 0 || captured_at_millis > MAX_SAFE_INTEGER {
            return Err(BackupError::invalid());
        }
        let kinds = sources
            .iter()
            .map(|source| source.kind())
            .collect::<BTreeSet<_>>();
        if sources.len() != BackupComponentKind::REQUIRED.len()
            || kinds.len() != BackupComponentKind::REQUIRED.len()
            || BackupComponentKind::REQUIRED
                .iter()
                .any(|kind| !kinds.contains(kind))
        {
            return Err(BackupError::incomplete());
        }
        sources.sort_by_key(|source| source.kind());
        let request = BackupSnapshotRequest {
            scope: scope.clone(),
            consistency_cut_digest,
            captured_at_millis,
        };
        let mut components = Vec::with_capacity(sources.len());
        for source in sources {
            let expected_kind = source.kind();
            let snapshot = source
                .snapshot(&request)
                .map_err(|_| BackupError::unavailable())?;
            if snapshot.kind() != expected_kind
                || snapshot.scope() != &scope
                || snapshot.consistency_cut_digest() != request.consistency_cut_digest()
            {
                return Err(BackupError::integrity());
            }
            components.push(snapshot);
        }
        let dependencies = canonical_dependencies(&components)?;
        BackupManifest::try_new(
            backup_id,
            scope,
            resident_region,
            captured_at_millis,
            components,
            dependencies,
        )
    }
}

fn canonical_dependencies(
    components: &[BackupComponentSnapshot],
) -> Result<Vec<BackupDependency>, BackupError> {
    let target = |kind| {
        components
            .iter()
            .find(|component| component.kind() == kind)
            .map(BackupComponentSnapshot::content_digest)
            .cloned()
            .ok_or_else(BackupError::incomplete)
    };
    [
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::AuditLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::LeaseRegistry,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::UsageLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::ArtifactObjects,
        ),
        (
            BackupComponentKind::ReferenceCatalog,
            BackupComponentKind::SecretReferences,
        ),
    ]
    .into_iter()
    .map(|(source, target_kind)| {
        BackupDependency::try_new(source, target_kind, target(target_kind)?)
    })
    .collect()
}
