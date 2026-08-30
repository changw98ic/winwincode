// SPDX-License-Identifier: Apache-2.0

use winwincode_audit::AuditScope;
use winwincode_backup::BackupComponentKind;
use winwincode_domain::Sha256Digest;
use winwincode_storage::{
    CommitReceipt, LoadedAggregateJournal, OutboxEvent, PendingAuditEvent, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StoredState,
};

use crate::{PostgresError, PostgresMigrationPlan, PostgresMigrationReceipt};

/// Fixed transaction stages executed inside one `PostgreSQL` transaction.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PostgresTransactionStage {
    ReceiptLookup,
    RevisionGuards,
    CanonicalState,
    AggregateJournal,
    CommandReceipt,
    AuditOutbox,
    PublicOutbox,
    Commit,
}

/// Validated transaction input passed to a `PostgreSQL` protocol backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresCommitPlan {
    tenant_scope: ReceiptScopeKey,
    commit: StateCommit,
}

impl PostgresCommitPlan {
    /// Builds the exact canonical transaction input.
    ///
    /// # Errors
    ///
    /// Rejects a commit whose durable receipt scope differs from the adapter's
    /// fixed tenant authority or whose canonical validation fails.
    pub fn try_new(
        tenant_scope: &ReceiptScopeKey,
        commit: &StateCommit,
    ) -> Result<Self, PostgresError> {
        commit
            .validate_for_storage_adapter()
            .map_err(|_| PostgresError::new(crate::PostgresErrorKind::InvalidInput))?;
        if commit.receipt_identity.scope_key() != tenant_scope {
            return Err(PostgresError::new(crate::PostgresErrorKind::InvalidInput));
        }
        Ok(Self {
            tenant_scope: tenant_scope.clone(),
            commit: commit.clone(),
        })
    }

    #[must_use]
    pub const fn tenant_scope(&self) -> &ReceiptScopeKey {
        &self.tenant_scope
    }

    #[must_use]
    pub const fn commit(&self) -> &StateCommit {
        &self.commit
    }

    #[must_use]
    pub const fn stages() -> [PostgresTransactionStage; 8] {
        [
            PostgresTransactionStage::ReceiptLookup,
            PostgresTransactionStage::RevisionGuards,
            PostgresTransactionStage::CanonicalState,
            PostgresTransactionStage::AggregateJournal,
            PostgresTransactionStage::CommandReceipt,
            PostgresTransactionStage::AuditOutbox,
            PostgresTransactionStage::PublicOutbox,
            PostgresTransactionStage::Commit,
        ]
    }
}

/// Secret-free facts returned by one exported `PostgreSQL` snapshot component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresSnapshotExport {
    checkpoint_digest: Sha256Digest,
    content_digest: Sha256Digest,
    record_count: u64,
    byte_count: u64,
}

impl PostgresSnapshotExport {
    /// Builds one exact export receipt.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests or counts outside the JSON safe integer range.
    pub fn try_new(
        checkpoint_digest: Sha256Digest,
        content_digest: Sha256Digest,
        record_count: u64,
        byte_count: u64,
    ) -> Result<Self, PostgresError> {
        if !canonical_backup_digest(&checkpoint_digest)
            || !canonical_backup_digest(&content_digest)
            || record_count > 9_007_199_254_740_991
            || byte_count > 9_007_199_254_740_991
        {
            return Err(PostgresError::new(crate::PostgresErrorKind::CorruptData));
        }
        Ok(Self {
            checkpoint_digest,
            content_digest,
            record_count,
            byte_count,
        })
    }

    #[must_use]
    pub const fn checkpoint_digest(&self) -> &Sha256Digest {
        &self.checkpoint_digest
    }

    #[must_use]
    pub const fn content_digest(&self) -> &Sha256Digest {
        &self.content_digest
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

fn canonical_backup_digest(digest: &Sha256Digest) -> bool {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Synchronous protocol seam implemented by the real network backend and the
/// deterministic offline `PostgreSQL` contract fixture.
pub trait PostgresProtocolPort: Send {
    /// Atomically applies or exactly replays the complete ordered migration plan.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe migration conflict or backend failure.
    fn migrate(
        &mut self,
        plan: &PostgresMigrationPlan,
    ) -> Result<PostgresMigrationReceipt, PostgresError>;

    /// Executes every [`PostgresCommitPlan::stages`] member in one serializable
    /// transaction and returns only after `PostgreSQL` commit succeeds.
    ///
    /// # Errors
    ///
    /// Returns a typed concurrency, idempotency, integrity, or backend failure.
    fn commit(&mut self, plan: &PostgresCommitPlan) -> Result<CommitReceipt, PostgresError>;

    /// Loads one tenant-scoped durable command receipt.
    ///
    /// # Errors
    ///
    /// Returns a scope, integrity, or backend failure.
    fn load_receipt(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        identity: &ReceiptIdentity,
    ) -> Result<Option<CommitReceipt>, PostgresError>;

    /// Loads one tenant-scoped canonical state stream.
    ///
    /// # Errors
    ///
    /// Returns an integrity or backend failure.
    fn load_state(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        stream_id: &str,
    ) -> Result<Option<StoredState>, PostgresError>;

    /// Loads one tenant-scoped opaque aggregate journal.
    ///
    /// # Errors
    ///
    /// Returns an integrity or backend failure.
    fn load_journal(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        aggregate_type: &str,
        aggregate_id: &str,
    ) -> Result<Option<LoadedAggregateJournal>, PostgresError>;

    /// Loads unpublished outbox events in durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns an integrity or backend failure.
    fn pending_events(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
    ) -> Result<Vec<OutboxEvent>, PostgresError>;

    /// Idempotently marks one exact outbox event as published.
    ///
    /// # Errors
    ///
    /// Returns an identity, integrity, or backend failure.
    fn mark_published(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        event_id: &str,
    ) -> Result<(), PostgresError>;

    /// Loads the audit event attached to one tenant-scoped receipt.
    ///
    /// # Errors
    ///
    /// Returns a scope, integrity, or backend failure.
    fn load_pending_audit_event(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, PostgresError>;

    /// Loads audit events not yet appended to the immutable audit ledger.
    ///
    /// # Errors
    ///
    /// Returns an integrity or backend failure.
    fn pending_audit_events(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
    ) -> Result<Vec<PendingAuditEvent>, PostgresError>;

    /// Idempotently marks one audit event as persisted.
    ///
    /// # Errors
    ///
    /// Returns an identity, integrity, or backend failure.
    fn mark_audit_event_persisted(
        &mut self,
        tenant_scope: &ReceiptScopeKey,
        event_id: &str,
    ) -> Result<(), PostgresError>;

    /// Exports one narrow component from one transaction-consistent cut.
    ///
    /// # Errors
    ///
    /// Returns a scope, integrity, or backend failure.
    fn export_snapshot(
        &mut self,
        kind: BackupComponentKind,
        scope: &AuditScope,
        consistency_cut_digest: &Sha256Digest,
    ) -> Result<PostgresSnapshotExport, PostgresError>;

    /// Deterministically releases the protocol connection and owned resources.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe backend failure.
    fn close(&mut self) -> Result<(), PostgresError>;
}
