// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use winwincode_backup::BackupComponentKind;
use winwincode_domain::Sha256Digest;
use winwincode_storage::{
    AggregateJournalKey, CommitReceipt, LoadedAggregateJournal, OutboxEvent, PendingAuditEvent,
    ProductStateStorage, ProjectionEventCursor, ProjectionEventStreamKey, ProjectionReadCut,
    ReceiptIdentity, ReceiptScopeKey, StateCommit, StorageError, StoredState,
};

use crate::{
    PostgresBackupSnapshotSource, PostgresCommitPlan, PostgresError, PostgresErrorKind,
    PostgresMigrationPlan, PostgresProtocolPort,
};

/// `PostgreSQL` implementation of the canonical product-state storage port.
pub struct PostgresStorage<P: PostgresProtocolPort> {
    protocol: Arc<Mutex<P>>,
    tenant_scope: ReceiptScopeKey,
}

impl<P: PostgresProtocolPort> fmt::Debug for PostgresStorage<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresStorage")
            .field("protocol", &"[BOUND]")
            .field("tenant_scope", &"[BOUND]")
            .finish()
    }
}

impl<P: PostgresProtocolPort> PostgresStorage<P> {
    /// Applies the canonical migration plan before returning a usable adapter.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe migration or protocol failure. No command can be
    /// accepted when the migration receipt differs from the embedded plan.
    pub fn try_open(mut protocol: P, tenant_scope: ReceiptScopeKey) -> Result<Self, PostgresError> {
        let plan = PostgresMigrationPlan::current()?;
        let receipt = protocol.migrate(&plan)?;
        if receipt.version() != crate::POSTGRES_SCHEMA_VERSION
            || receipt.plan_digest() != plan.digest()
        {
            let _ = protocol.close();
            return Err(PostgresError::new(PostgresErrorKind::MigrationConflict));
        }
        Ok(Self {
            protocol: Arc::new(Mutex::new(protocol)),
            tenant_scope,
        })
    }

    /// Creates one narrow canonical backup component source sharing the same
    /// `PostgreSQL` snapshot protocol.
    ///
    /// # Errors
    ///
    /// Rejects `ArtifactObjects`, which belongs only to the object-store
    /// adapter.
    pub fn backup_source(
        &self,
        kind: BackupComponentKind,
    ) -> Result<PostgresBackupSnapshotSource<P>, PostgresError> {
        if kind == BackupComponentKind::ArtifactObjects {
            return Err(PostgresError::new(PostgresErrorKind::InvalidInput));
        }
        Ok(PostgresBackupSnapshotSource::new(
            Arc::clone(&self.protocol),
            kind,
        ))
    }

    fn protocol(&self) -> Result<MutexGuard<'_, P>, StorageError> {
        self.protocol
            .lock()
            .map_err(|_| StorageError::adapter("PostgreSQL storage is unavailable"))
    }

    fn require_scope(&self, identity: &ReceiptIdentity) -> Result<(), StorageError> {
        if identity.scope_key() != &self.tenant_scope {
            return Err(StorageError::invalid_input(
                "PostgreSQL tenant scope differs from adapter authority",
            ));
        }
        Ok(())
    }

    fn map_commit_error(error: PostgresError, commit: &StateCommit) -> StorageError {
        match error.kind() {
            PostgresErrorKind::RevisionConflict => StorageError::revision_conflict(
                commit.expected_revision,
                error.actual_revision().unwrap_or(commit.expected_revision),
            ),
            PostgresErrorKind::RequestConflict => {
                StorageError::request_conflict(commit.receipt_identity.request_id())
            }
            PostgresErrorKind::InvalidInput => {
                StorageError::invalid_input("PostgreSQL transaction input is invalid")
            }
            _ => StorageError::adapter(error.to_string()),
        }
    }
}

impl<P: PostgresProtocolPort + 'static> ProductStateStorage for PostgresStorage<P> {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        self.require_scope(&commit.receipt_identity)?;
        let plan = PostgresCommitPlan::try_new(&self.tenant_scope, commit)
            .map_err(|error| Self::map_commit_error(error, commit))?;
        self.protocol()?
            .commit(&plan)
            .map_err(|error| Self::map_commit_error(error, commit))
    }

    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        self.require_scope(identity)?;
        if !canonical_digest(command_digest) {
            return Err(StorageError::invalid_input(
                "command digest is not canonical",
            ));
        }
        let Some(mut receipt) = self
            .protocol()?
            .load_receipt(&self.tenant_scope, identity)
            .map_err(storage_error)?
        else {
            return Ok(None);
        };
        if &receipt.command_digest != command_digest {
            return Err(StorageError::request_conflict(identity.request_id()));
        }
        receipt.idempotent_replay = true;
        Ok(Some(receipt))
    }

    fn load_receipt_for_identity(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        self.require_scope(identity)?;
        self.protocol()?
            .load_receipt(&self.tenant_scope, identity)
            .map_err(storage_error)
    }

    fn load_pending_audit_event(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, StorageError> {
        self.require_scope(identity)?;
        self.protocol()?
            .load_pending_audit_event(&self.tenant_scope, identity)
            .map_err(storage_error)
    }

    fn pending_audit_events(&self) -> Result<Vec<PendingAuditEvent>, StorageError> {
        self.protocol()?
            .pending_audit_events(&self.tenant_scope)
            .map_err(storage_error)
    }

    fn mark_audit_event_persisted(&mut self, event_id: &str) -> Result<(), StorageError> {
        if event_id.is_empty() {
            return Err(StorageError::invalid_input("audit event id is invalid"));
        }
        self.protocol()?
            .mark_audit_event_persisted(&self.tenant_scope, event_id)
            .map_err(storage_error)
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        if stream_id.is_empty() {
            return Err(StorageError::invalid_input("stream id is invalid"));
        }
        self.protocol()?
            .load_state(&self.tenant_scope, stream_id)
            .map_err(storage_error)
    }

    fn load_projection_read_cut(
        &self,
        _state_stream_ids: &[String],
        _key: &ProjectionEventStreamKey,
        _expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError> {
        Err(StorageError::adapter(
            "PostgreSQL projection read cuts require the live network backend",
        ))
    }

    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        self.protocol()?
            .load_journal(&self.tenant_scope, key.aggregate_type(), key.aggregate_id())
            .map_err(storage_error)
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        self.protocol()?
            .pending_events(&self.tenant_scope)
            .map_err(storage_error)
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        if event_id.is_empty() {
            return Err(StorageError::invalid_input("outbox event id is invalid"));
        }
        self.protocol()?
            .mark_published(&self.tenant_scope, event_id)
            .map_err(storage_error)
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        self.protocol()?.close().map_err(storage_error)
    }
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn storage_error(error: PostgresError) -> StorageError {
    match error.kind() {
        PostgresErrorKind::InvalidInput => {
            StorageError::invalid_input("PostgreSQL adapter input is invalid")
        }
        _ => StorageError::adapter(error.to_string()),
    }
}
