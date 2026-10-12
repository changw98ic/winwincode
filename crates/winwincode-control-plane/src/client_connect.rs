// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    AccessGrantIssuance, AccessGrantRecord, AttemptDimension, ClientConnectStoreError,
    ClientConnectStoreErrorKind, ConnectAttemptState, ConnectAuditEntry, ConnectCodeConsume,
    ConnectCodePublication, ConnectCodeRecord, ConnectCodeRevocation, ConnectGrantReceipt,
    GrantPermissions, GrantSource, SqliteStorage,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientConnectServiceErrorKind {
    InvalidInput,
    UnknownClientNode,
    ClientConnectionsForbidden,
    ClientLocked,
    UnknownConnectCode,
    UnknownAccessGrant,
    CodeNotActive,
    ConnectCodeExpired,
    AttemptsExhausted,
    GenerationMismatch,
    ConnectCodeDigestConflict,
    ConnectCodeIdConflict,
    AccessGrantConflict,
    IllegalStateTransition,
    RevisionConflict,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConnectServiceError {
    kind: ClientConnectServiceErrorKind,
    message: String,
}

impl ClientConnectServiceError {
    #[must_use]
    pub const fn kind(&self) -> ClientConnectServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientConnectServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientConnectServiceError {}

impl From<ClientConnectStoreError> for ClientConnectServiceError {
    fn from(source: ClientConnectStoreError) -> Self {
        Self {
            kind: match source.kind() {
                ClientConnectStoreErrorKind::InvalidInput => {
                    ClientConnectServiceErrorKind::InvalidInput
                }
                ClientConnectStoreErrorKind::UnknownClientNode => {
                    ClientConnectServiceErrorKind::UnknownClientNode
                }
                ClientConnectStoreErrorKind::ClientConnectionsForbidden => {
                    ClientConnectServiceErrorKind::ClientConnectionsForbidden
                }
                ClientConnectStoreErrorKind::ClientLocked => {
                    ClientConnectServiceErrorKind::ClientLocked
                }
                ClientConnectStoreErrorKind::UnknownConnectCode => {
                    ClientConnectServiceErrorKind::UnknownConnectCode
                }
                ClientConnectStoreErrorKind::UnknownAccessGrant => {
                    ClientConnectServiceErrorKind::UnknownAccessGrant
                }
                ClientConnectStoreErrorKind::CodeNotActive => {
                    ClientConnectServiceErrorKind::CodeNotActive
                }
                ClientConnectStoreErrorKind::ConnectCodeExpired => {
                    ClientConnectServiceErrorKind::ConnectCodeExpired
                }
                ClientConnectStoreErrorKind::AttemptsExhausted => {
                    ClientConnectServiceErrorKind::AttemptsExhausted
                }
                ClientConnectStoreErrorKind::GenerationMismatch => {
                    ClientConnectServiceErrorKind::GenerationMismatch
                }
                ClientConnectStoreErrorKind::ConnectCodeDigestConflict => {
                    ClientConnectServiceErrorKind::ConnectCodeDigestConflict
                }
                ClientConnectStoreErrorKind::ConnectCodeIdConflict => {
                    ClientConnectServiceErrorKind::ConnectCodeIdConflict
                }
                ClientConnectStoreErrorKind::AccessGrantConflict => {
                    ClientConnectServiceErrorKind::AccessGrantConflict
                }
                ClientConnectStoreErrorKind::IllegalStateTransition => {
                    ClientConnectServiceErrorKind::IllegalStateTransition
                }
                ClientConnectStoreErrorKind::RevisionConflict => {
                    ClientConnectServiceErrorKind::RevisionConflict
                }
                ClientConnectStoreErrorKind::CorruptState => {
                    ClientConnectServiceErrorKind::CorruptState
                }
                ClientConnectStoreErrorKind::Storage => ClientConnectServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

pub struct ConnectCodeService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ConnectCodeService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    pub fn publish(
        &mut self,
        publication: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .publish(publication, now)?)
    }

    pub fn active_code_for_client(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_code_for_client(client_node_id)?)
    }

    pub fn refresh_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
        replacement: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .refresh_code(revocation, replacement, now)?)
    }

    pub fn revoke_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .revoke_code(revocation)?)
    }

    pub fn record_failed_attempt(
        &mut self,
        connect_code_id: &str,
        expected_revision: u64,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_failed_attempt(connect_code_id, expected_revision)?)
    }

    pub fn expire_codes_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .expire_codes_due(cutoff)?)
    }

    pub fn code_snapshot(
        &mut self,
        connect_code_id: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .code_snapshot(connect_code_id)?)
    }

    pub fn code_snapshot_by_digest(
        &mut self,
        code_digest: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .code_snapshot_by_digest(code_digest)?)
    }

    pub fn consume_and_grant(
        &mut self,
        consume: &ConnectCodeConsume,
        issuance: &AccessGrantIssuance,
        now: &Instant,
    ) -> Result<ConnectGrantReceipt, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .consume_and_create_grant(consume, issuance, now)?)
    }

    pub fn record_connect_failure(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
    ) -> Result<ConnectAttemptState, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_connect_failure(dimension, subject_key, window_anchor)?)
    }

    pub fn connect_failure_count(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
    ) -> Result<u64, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .connect_failure_count(dimension, subject_key, window_anchor)?)
    }

    pub fn connect_attempts_blocked(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
        max_attempts: u64,
    ) -> Result<bool, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .connect_attempts_blocked(dimension, subject_key, window_anchor, max_attempts)?)
    }

    pub fn record_connect_audit(
        &mut self,
        entry: &ConnectAuditEntry,
    ) -> Result<(), ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_connect_audit(entry)?)
    }
}

pub struct AccessGrantService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> AccessGrantService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    pub fn create_grant(
        &mut self,
        issuance: &AccessGrantIssuance,
        grant_source: GrantSource,
        permissions: GrantPermissions,
        now: &Instant,
    ) -> Result<AccessGrantRecord, ClientConnectServiceError> {
        Ok(self.storage.client_connect_ledger()?.create_grant(
            issuance,
            grant_source,
            permissions,
            now,
        )?)
    }

    pub fn active_grant(
        &mut self,
        client_node_id: &str,
        user_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_grant(client_node_id, user_id)?)
    }

    pub fn active_grants_for_user(
        &mut self,
        user_id: &str,
    ) -> Result<Vec<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_grants_for_user(user_id)?)
    }

    pub fn active_grants_for_client(
        &mut self,
        client_node_id: &str,
    ) -> Result<Vec<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_grants_for_client(client_node_id)?)
    }

    pub fn grant_snapshot(
        &mut self,
        client_access_grant_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .grant_snapshot(client_access_grant_id)?)
    }

    pub fn revoke_grant(
        &mut self,
        client_access_grant_id: &str,
        expected_revision: u64,
    ) -> Result<AccessGrantRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .revoke_grant(client_access_grant_id, expected_revision)?)
    }

    pub fn expire_grants_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .expire_grants_due(cutoff)?)
    }
}
