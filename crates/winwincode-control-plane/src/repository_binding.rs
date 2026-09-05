// SPDX-License-Identifier: Apache-2.0

//! Repository-binding and repository-access-grant application services over
//! the durable Server-side repository registry.
//!
//! The Control Plane owns the persisted projection of Device Client-reported
//! repository facts (ADR-0030, plan 7.6, 13): binding upserts that are
//! idempotent by binding id under `expectedRevision` compare-and-swap,
//! seven-state availability scan reports (contract 7), binding removal, and
//! the explicit `RepositoryAccessGrant` relationships (plan 7.7). The
//! absolute local path never crosses this boundary. Visibility follows plan
//! 13.4: an `active` `ClientAccessGrant` carrying `use` on the client node
//! AND an `active` `RepositoryAccessGrant` on the binding must both exist.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    RepositoryAccessGrantIssuance, RepositoryAccessGrantRecord, RepositoryBindingProjection,
    RepositoryBindingReceipt, RepositoryBindingRecord, RepositoryBindingStoreError,
    RepositoryBindingStoreErrorKind, RepositoryGrantPermissions, RepositoryScanOutcome,
    SqliteStorage,
};

/// Re-exported so service consumers can name the scan-outcome vocabulary
/// without importing the storage crate directly.
pub use winwincode_storage::{RepositoryAvailability, RepositoryDirtyState, RepositoryGrantState};

/// Stable repository-binding service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryBindingServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No repository binding matches the requested identity.
    UnknownRepositoryBinding,
    /// No repository access grant matches the requested identity.
    UnknownAccessGrant,
    /// The repository fingerprint is already bound to another binding on the
    /// same client node, or the binding id is already used elsewhere.
    FingerprintConflict,
    /// An active grant for the user and binding already exists, or the grant
    /// id is already used.
    AccessGrantConflict,
    /// The supplied `expectedRevision` no longer matches the durable revision.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free repository-binding service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingServiceError {
    kind: RepositoryBindingServiceErrorKind,
    message: String,
}

impl RepositoryBindingServiceError {
    #[must_use]
    pub const fn kind(&self) -> RepositoryBindingServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for RepositoryBindingServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepositoryBindingServiceError {}

impl From<RepositoryBindingStoreError> for RepositoryBindingServiceError {
    fn from(source: RepositoryBindingStoreError) -> Self {
        Self {
            kind: match source.kind() {
                RepositoryBindingStoreErrorKind::InvalidInput => {
                    RepositoryBindingServiceErrorKind::InvalidInput
                }
                RepositoryBindingStoreErrorKind::UnknownClientNode => {
                    RepositoryBindingServiceErrorKind::UnknownClientNode
                }
                RepositoryBindingStoreErrorKind::UnknownRepositoryBinding => {
                    RepositoryBindingServiceErrorKind::UnknownRepositoryBinding
                }
                RepositoryBindingStoreErrorKind::UnknownAccessGrant => {
                    RepositoryBindingServiceErrorKind::UnknownAccessGrant
                }
                RepositoryBindingStoreErrorKind::FingerprintConflict => {
                    RepositoryBindingServiceErrorKind::FingerprintConflict
                }
                RepositoryBindingStoreErrorKind::AccessGrantConflict => {
                    RepositoryBindingServiceErrorKind::AccessGrantConflict
                }
                RepositoryBindingStoreErrorKind::RevisionConflict => {
                    RepositoryBindingServiceErrorKind::RevisionConflict
                }
                RepositoryBindingStoreErrorKind::CorruptState => {
                    RepositoryBindingServiceErrorKind::CorruptState
                }
                RepositoryBindingStoreErrorKind::Storage => {
                    RepositoryBindingServiceErrorKind::Storage
                }
            },
            message: source.to_string(),
        }
    }
}

/// Repository-binding application service over one storage connection.
///
/// Owns the device-reported binding projection: registration and refresh
/// (idempotent by binding id, CAS-guarded), seven-state availability scan
/// reports, removal, and the plan 13.4 visibility projection.
pub struct RepositoryBindingService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> RepositoryBindingService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Upserts the Device Client-reported binding projection (plan 7.6):
    /// idempotent by binding id, guarded by `expectedRevision` CAS on the
    /// update path. A byte-identical re-report is an accepted replay that
    /// leaves the revision untouched; a fingerprint already bound to a
    /// different binding id on the same client node fails closed.
    ///
    /// # Errors
    ///
    /// Rejects invalid projection facts, an unknown client node, a
    /// fingerprint conflict, a stale `expectedRevision`, or storage failure.
    pub fn upsert(
        &mut self,
        projection: &RepositoryBindingProjection,
        last_scanned_at: Option<&Instant>,
        expected_revision: u64,
        now: &Instant,
    ) -> Result<RepositoryBindingReceipt, RepositoryBindingServiceError> {
        Ok(self.storage.repository_binding_ledger()?.upsert(
            projection,
            last_scanned_at,
            expected_revision,
            now,
        )?)
    }

    /// Applies one rescan outcome to an existing binding (contract 7): the
    /// seven availability states may move to any state after
    /// re-verification; there are no terminal states.
    ///
    /// # Errors
    ///
    /// Rejects an unknown binding, a stale `expectedRevision`, or storage
    /// failure.
    pub fn update_availability(
        &mut self,
        repository_binding_id: &str,
        outcome: &RepositoryScanOutcome,
        last_scanned_at: &Instant,
        expected_revision: u64,
    ) -> Result<RepositoryBindingRecord, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .update_availability(
                repository_binding_id,
                outcome,
                last_scanned_at,
                expected_revision,
            )?)
    }

    /// Removes one binding (`client.repository.removed`); its access grants
    /// cascade away and the fingerprint becomes re-registrable. Removing an
    /// absent binding is an accepted idempotent replay reporting `false`.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical binding identity or storage failure.
    pub fn remove(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<bool, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .remove(repository_binding_id)?)
    }

    /// Returns one durable repository binding projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical binding identity or storage failure.
    pub fn snapshot(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<Option<RepositoryBindingRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .snapshot(repository_binding_id)?)
    }

    /// Returns every durable binding projection of one client node.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn bindings_for_client(
        &mut self,
        client_node_id: &str,
    ) -> Result<Vec<RepositoryBindingRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .bindings_for_client(client_node_id)?)
    }

    /// Returns the repository bindings of one client node that the user may
    /// see (plan 13.4): an `active` `ClientAccessGrant` carrying `use` on
    /// the client node AND an `active` `RepositoryAccessGrant` on the
    /// binding must both exist.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or storage failure.
    pub fn visible_bindings(
        &mut self,
        user_id: &str,
        client_node_id: &str,
    ) -> Result<Vec<RepositoryBindingRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .visible_bindings(user_id, client_node_id)?)
    }
}

/// Repository-access-grant application service over one storage connection.
///
/// Owns explicit per-user repository authorization (plan 7.7, 13.4): grant
/// creation, immediate revocation, and active-grant lookups.
pub struct RepositoryAccessGrantService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> RepositoryAccessGrantService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Creates one `active` grant (plan 7.7). At most one active grant per
    /// user and binding exists; a revoked grant does not block a fresh
    /// re-grant.
    ///
    /// # Errors
    ///
    /// Rejects an unknown binding, an already-active grant for the user and
    /// binding, a reused grant id, or storage failure.
    pub fn create_grant(
        &mut self,
        issuance: &RepositoryAccessGrantIssuance,
        permissions: RepositoryGrantPermissions,
        now: &Instant,
    ) -> Result<RepositoryAccessGrantRecord, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .create_grant(issuance, permissions, now)?)
    }

    /// Revokes one grant: visibility and use end immediately without waiting
    /// for the Device Client (plan 13.4). Revoking an already-`revoked`
    /// grant is an accepted idempotent replay.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a stale `expectedRevision`, or storage
    /// failure.
    pub fn revoke_grant(
        &mut self,
        repository_access_grant_id: &str,
        expected_revision: u64,
    ) -> Result<RepositoryAccessGrantRecord, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .revoke_grant(repository_access_grant_id, expected_revision)?)
    }

    /// Returns one durable access grant projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn grant_snapshot(
        &mut self,
        repository_access_grant_id: &str,
    ) -> Result<Option<RepositoryAccessGrantRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .grant_snapshot(repository_access_grant_id)?)
    }

    /// Returns every active grant on one repository binding.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical binding identity or storage failure.
    pub fn active_grants_for_binding(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .active_grants_for_binding(repository_binding_id)?)
    }

    /// Returns every active grant of one user across all bindings.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical user identity or storage failure.
    pub fn active_grants_for_user(
        &mut self,
        user_id: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .active_grants_for_user(user_id)?)
    }
}
