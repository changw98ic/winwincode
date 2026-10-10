use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    RepositoryAccessGrantIssuance, RepositoryAccessGrantRecord, RepositoryBindingProjection,
    RepositoryBindingReceipt, RepositoryBindingRecord, RepositoryBindingStoreError,
    RepositoryBindingStoreErrorKind, RepositoryGrantPermissions, RepositoryScanOutcome,
    SqliteStorage,
};

pub use winwincode_storage::{RepositoryAvailability, RepositoryDirtyState, RepositoryGrantState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryBindingServiceErrorKind {
    InvalidInput,
    UnknownClientNode,
    UnknownRepositoryBinding,
    UnknownAccessGrant,
    FingerprintConflict,
    AccessGrantConflict,
    RevisionConflict,
    CorruptState,
    Storage,
}

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

pub struct RepositoryBindingService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> RepositoryBindingService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

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

    pub fn remove(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<bool, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .remove(repository_binding_id)?)
    }

    pub fn snapshot(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<Option<RepositoryBindingRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .snapshot(repository_binding_id)?)
    }

    pub fn bindings_for_client(
        &mut self,
        client_node_id: &str,
    ) -> Result<Vec<RepositoryBindingRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .bindings_for_client(client_node_id)?)
    }

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

pub struct RepositoryAccessGrantService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> RepositoryAccessGrantService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

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

    pub fn grant_snapshot(
        &mut self,
        repository_access_grant_id: &str,
    ) -> Result<Option<RepositoryAccessGrantRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .grant_snapshot(repository_access_grant_id)?)
    }

    pub fn active_grants_for_binding(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingServiceError> {
        Ok(self
            .storage
            .repository_binding_ledger()?
            .active_grants_for_binding(repository_binding_id)?)
    }

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
