// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use winwincode_audit::AuditScope;
use winwincode_domain::Sha256Digest;

use crate::{
    BackupComponentKind, BackupComponentSnapshot, BackupError, BackupManifest, MAX_SAFE_INTEGER,
};

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Stable identity for one idempotent restore attempt.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RestoreId(String);

impl RestoreId {
    /// Builds a canonical `rst_` identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities.
    pub fn try_new(value: impl Into<String>) -> Result<Self, BackupError> {
        let value = value.into();
        let valid = value.strip_prefix("rst_").is_some_and(|suffix| {
            suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
        });
        if !valid {
            return Err(BackupError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Facts read back from one staged backend generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreEvidence {
    snapshot: BackupComponentSnapshot,
}

impl RestoreEvidence {
    #[must_use]
    pub const fn new(snapshot: BackupComponentSnapshot) -> Self {
        Self { snapshot }
    }

    #[must_use]
    pub const fn snapshot(&self) -> &BackupComponentSnapshot {
        &self.snapshot
    }
}

/// Sealed complete restore authorization. Only [`RestoreCoordinator`] can
/// construct this value after verification.
#[derive(Clone, Debug)]
pub struct VerifiedRestore {
    restore_id: RestoreId,
    manifest: BackupManifest,
}

impl VerifiedRestore {
    #[must_use]
    pub const fn restore_id(&self) -> &RestoreId {
        &self.restore_id
    }

    #[must_use]
    pub const fn manifest(&self) -> &BackupManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        self.manifest.scope()
    }

    #[must_use]
    pub const fn manifest_digest(&self) -> &Sha256Digest {
        self.manifest.manifest_digest()
    }
}

/// Durable prepare result for a staged generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestorePreparation {
    Prepared,
    AlreadyPrepared,
}

/// Atomic activation result. The previous active generation remains visible
/// until this step succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreActivation {
    Activated,
    AlreadyActivated,
}

/// Stable target failure without database/object-store diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestoreTargetError;

impl RestoreTargetError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RestoreTargetError {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomic restore boundary implemented by PostgreSQL/object/Vault adapters.
/// Implementations are idempotent by `restore_id` plus `manifest_digest` and
/// must reject changed reuse.
pub trait RestoreTarget {
    /// Persists a non-active staged generation.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe error when staging is unavailable or conflicts.
    fn prepare(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestorePreparation, RestoreTargetError>;

    /// Atomically switches all subsystem roots to the verified generation.
    ///
    /// # Errors
    ///
    /// Returns without changing the previous active generation on failure.
    fn activate(
        &mut self,
        restore: &VerifiedRestore,
    ) -> Result<RestoreActivation, RestoreTargetError>;
}

/// Verifies a fixed backup cut before crossing the restore target boundary.
pub struct RestoreCoordinator;

impl RestoreCoordinator {
    /// Verifies one complete staged generation without activating it.
    ///
    /// Rolling upgrades use this to prove rollback viability before any
    /// deployment mutation occurs.
    ///
    /// # Errors
    ///
    /// Rejects incomplete, tampered, mixed-cut, or cross-tenant evidence.
    pub fn verify(
        restore_id: RestoreId,
        manifest: &BackupManifest,
        evidence: impl IntoIterator<Item = RestoreEvidence>,
    ) -> Result<VerifiedRestore, BackupError> {
        verify(restore_id, manifest, evidence)
    }

    /// Prepares and atomically activates an already verified generation.
    ///
    /// # Errors
    ///
    /// Preserves the previous active generation and the staged generation when
    /// the target is unavailable.
    pub fn activate(
        verified: &VerifiedRestore,
        target: &mut dyn RestoreTarget,
    ) -> Result<RestoreActivation, BackupError> {
        target
            .prepare(verified)
            .map_err(|_| BackupError::unavailable())?;
        target
            .activate(verified)
            .map_err(|_| BackupError::unavailable())
    }

    /// Verifies, prepares, then atomically activates one generation.
    ///
    /// # Errors
    ///
    /// Fails before the target for incomplete, tampered, mixed-cut, or
    /// cross-tenant evidence. Target failures preserve the staged generation
    /// for exact restart replay.
    pub fn restore(
        restore_id: RestoreId,
        manifest: &BackupManifest,
        evidence: impl IntoIterator<Item = RestoreEvidence>,
        target: &mut dyn RestoreTarget,
    ) -> Result<RestoreActivation, BackupError> {
        let verified = Self::verify(restore_id, manifest, evidence)?;
        Self::activate(&verified, target)
    }
}

fn verify(
    restore_id: RestoreId,
    manifest: &BackupManifest,
    evidence: impl IntoIterator<Item = RestoreEvidence>,
) -> Result<VerifiedRestore, BackupError> {
    if manifest.captured_at_millis() == 0 || manifest.captured_at_millis() > MAX_SAFE_INTEGER {
        return Err(BackupError::integrity());
    }
    let expected = manifest
        .components()
        .iter()
        .map(|component| (component.kind(), component))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for evidence in evidence {
        let snapshot = evidence.snapshot();
        if snapshot.scope() != manifest.scope() {
            return Err(BackupError::tenant());
        }
        if !seen.insert(snapshot.kind()) {
            return Err(BackupError::conflict());
        }
        let Some(component) = expected.get(&snapshot.kind()) else {
            return Err(BackupError::incomplete());
        };
        if snapshot != *component
            || snapshot.consistency_cut_digest() != manifest.consistency_cut_digest()
        {
            return Err(BackupError::integrity());
        }
    }
    if seen.len() != BackupComponentKind::REQUIRED.len() {
        return Err(BackupError::incomplete());
    }
    Ok(VerifiedRestore {
        restore_id,
        manifest: manifest.clone(),
    })
}
