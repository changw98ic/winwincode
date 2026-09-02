// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_audit::AuditScope;
use winwincode_domain::{CredentialReferenceId, Sha256Digest};

use crate::model::{
    corrupt, domain_digest, invalid, scope_bytes, validate_count, validate_digest,
    validate_integration_id, validate_scope, validate_time,
};
use crate::{
    ConnectorAuthority, ConnectorCallError, ConnectorCallErrorKind, ConnectorProtocol,
    ConnectorRegistration, ConnectorRegistrationReceipt, ConnectorState, EnterpriseIntegrationId,
    InboundDispatch, InboundReceipt, InboundStatus, IntegrationAuditFact, IntegrationAuditKind,
    IntegrationError, IntegrationErrorKind, IntegrationLeaseId, IntegrationOperationKey,
    NormalizedInboundEvent, OutboundAttemptResult, OutboundCallReceipt, OutboundClaim,
    OutboundDeliveryReceipt, OutboundEnqueueReceipt, OutboundOperation, OutboundOperationState,
    OutboundRequest, RetryPolicy,
};

const SCHEMA: &str = r"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS integration_connectors (
    integration_id TEXT PRIMARY KEY NOT NULL,
    scope_json BLOB NOT NULL,
    scope_digest TEXT NOT NULL,
    protocol TEXT NOT NULL,
    credential_reference_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'credential_revoked')),
    registered_at INTEGER NOT NULL CHECK (registered_at > 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= registered_at)
);
CREATE TABLE IF NOT EXISTS integration_inbound_receipts (
    integration_id TEXT NOT NULL,
    event_key TEXT NOT NULL,
    ordering_key_digest TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    provider_sequence INTEGER NOT NULL CHECK (provider_sequence > 0),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'ignored_out_of_order')),
    command_digest TEXT NOT NULL,
    received_at INTEGER NOT NULL CHECK (received_at > 0),
    PRIMARY KEY (integration_id, event_key),
    FOREIGN KEY (integration_id) REFERENCES integration_connectors(integration_id) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS integration_inbound_ordering (
    integration_id TEXT NOT NULL,
    ordering_key_digest TEXT NOT NULL,
    last_provider_sequence INTEGER NOT NULL CHECK (last_provider_sequence > 0),
    PRIMARY KEY (integration_id, ordering_key_digest),
    FOREIGN KEY (integration_id) REFERENCES integration_connectors(integration_id)
      ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS integration_inbound_dispatches (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    integration_id TEXT NOT NULL,
    event_key TEXT NOT NULL,
    command_name TEXT NOT NULL,
    command_payload BLOB NOT NULL,
    command_digest TEXT NOT NULL,
    UNIQUE (integration_id, event_key),
    FOREIGN KEY (integration_id, event_key)
      REFERENCES integration_inbound_receipts(integration_id, event_key) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS integration_outbound_operations (
    integration_id TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    operation_name TEXT NOT NULL,
    payload BLOB NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'leased', 'delivered', 'dead_letter')),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    eligible_at INTEGER NOT NULL CHECK (eligible_at > 0),
    lease_id TEXT,
    lease_expires_at INTEGER,
    max_attempts INTEGER NOT NULL CHECK (max_attempts > 0),
    initial_backoff INTEGER NOT NULL CHECK (initial_backoff > 0),
    max_backoff INTEGER NOT NULL CHECK (max_backoff >= initial_backoff),
    enqueued_at INTEGER NOT NULL CHECK (enqueued_at > 0),
    completed_at INTEGER,
    remote_receipt_digest TEXT,
    PRIMARY KEY (integration_id, operation_key),
    FOREIGN KEY (integration_id) REFERENCES integration_connectors(integration_id) ON DELETE RESTRICT,
    CHECK ((state = 'leased') = (lease_id IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK ((state IN ('delivered', 'dead_letter')) = (completed_at IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS integration_outbound_due
  ON integration_outbound_operations(integration_id, state, eligible_at, operation_key);
CREATE TABLE IF NOT EXISTS integration_outbound_attempt_receipts (
    integration_id TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    lease_id TEXT NOT NULL,
    outcome_kind TEXT NOT NULL CHECK (outcome_kind IN
      ('delivered', 'retry_scheduled', 'dead_lettered')),
    failure_kind TEXT CHECK (failure_kind IS NULL OR failure_kind IN
      ('retryable', 'permanent', 'credential_revoked')),
    outcome_code TEXT NOT NULL,
    remote_receipt_digest TEXT,
    remote_write_performed INTEGER,
    result_eligible_at INTEGER NOT NULL CHECK (result_eligible_at > 0),
    completed_at INTEGER NOT NULL CHECK (completed_at > 0),
    PRIMARY KEY (integration_id, operation_key, attempt),
    FOREIGN KEY (integration_id, operation_key)
      REFERENCES integration_outbound_operations(integration_id, operation_key) ON DELETE RESTRICT,
    CHECK ((outcome_kind = 'delivered') = (failure_kind IS NULL))
);
CREATE TABLE IF NOT EXISTS integration_audit_outbox (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_json BLOB NOT NULL,
    integration_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    occurred_at INTEGER NOT NULL CHECK (occurred_at > 0)
);
";

/// SQLite-backed connector authority, receipts, command outbox, retry queue,
/// and secret-safe audit outbox.
pub struct IntegrationStorage {
    database_path: PathBuf,
    connection: Connection,
}

impl IntegrationStorage {
    /// Opens or creates the integration database under one data directory.
    ///
    /// # Errors
    ///
    /// Returns a stable storage error for directory, database, or schema failure.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, IntegrationError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|_| storage_error())?;
        let database_path = data_directory.join("integration.sqlite3");
        let connection = Connection::open(&database_path).map_err(|_| storage_error())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| storage_error())?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|_| storage_error())?;
        Ok(Self {
            database_path,
            connection,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Registers one connector or exactly replays the same authority facts.
    ///
    /// # Errors
    ///
    /// Rejects changed connector reuse, tenant conflict, or storage failure.
    pub fn register(
        &mut self,
        registration: &ConnectorRegistration,
    ) -> Result<ConnectorRegistrationReceipt, IntegrationError> {
        let transaction = self.transaction()?;
        if let Some((authority, registered_at)) =
            load_authority_row(&transaction, registration.integration_id())?
        {
            if authority.scope() != registration.scope() {
                return Err(tenant_error());
            }
            if authority.protocol() != registration.protocol()
                || authority.credential_reference_id() != registration.credential_reference_id()
                || registered_at != registration.registered_at_millis()
            {
                return Err(conflict_error());
            }
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(ConnectorRegistrationReceipt::new(authority, true));
        }
        let scope_json = scope_bytes(registration.scope())?;
        let scope_digest = domain_digest(b"winwincode.integration.scope.v1", &scope_json);
        transaction
            .execute(
                "INSERT INTO integration_connectors
                 (integration_id, scope_json, scope_digest, protocol, credential_reference_id,
                  revision, state, registered_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 'active', ?6, ?6)",
                params![
                    registration.integration_id().0.as_str(),
                    scope_json,
                    scope_digest.0,
                    registration.protocol().as_str(),
                    registration.credential_reference_id().0,
                    to_sql(registration.registered_at_millis())?,
                ],
            )
            .map_err(|_| storage_error())?;
        let request_digest = registration_digest(registration)?;
        insert_audit(
            &transaction,
            registration.scope(),
            registration.integration_id(),
            IntegrationAuditKind::ConnectorRegistered,
            &request_digest,
            registration.registered_at_millis(),
        )?;
        let authority = load_authority_row(&transaction, registration.integration_id())?
            .ok_or_else(corrupt)?
            .0;
        transaction.commit().map_err(|_| storage_error())?;
        Ok(ConnectorRegistrationReceipt::new(authority, false))
    }

    /// Loads the exact tenant-scoped connector authority.
    ///
    /// # Errors
    ///
    /// Rejects missing, foreign-tenant, or corrupt authority.
    pub fn authority(
        &self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
    ) -> Result<ConnectorAuthority, IntegrationError> {
        validate_scope(scope)?;
        let authority = load_authority_row(&self.connection, integration_id)?
            .ok_or_else(not_found_error)?
            .0;
        require_scope(scope, &authority)?;
        Ok(authority)
    }

    /// Atomically revokes one connector credential authority.
    ///
    /// # Errors
    ///
    /// Rejects foreign tenant, stale revision, invalid time, or storage failure.
    pub fn revoke_credential(
        &mut self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
        expected_revision: u64,
        occurred_at_millis: u64,
    ) -> Result<ConnectorAuthority, IntegrationError> {
        validate_time(occurred_at_millis)?;
        let transaction = self.transaction()?;
        let authority = load_authority_row(&transaction, integration_id)?
            .ok_or_else(not_found_error)?
            .0;
        require_scope(scope, &authority)?;
        if authority.state() == ConnectorState::CredentialRevoked {
            let original_revision = authority.revision().checked_sub(1).ok_or_else(corrupt)?;
            if expected_revision == original_revision
                && occurred_at_millis == authority.updated_at_millis()
            {
                transaction.commit().map_err(|_| storage_error())?;
                return Ok(authority);
            }
            return Err(conflict_error());
        }
        if authority.revision() != expected_revision {
            return Err(conflict_error());
        }
        let revision = checked_increment(authority.revision())?;
        transaction
            .execute(
                "UPDATE integration_connectors
                 SET state = 'credential_revoked', revision = ?1, updated_at = ?2
                 WHERE integration_id = ?3 AND revision = ?4 AND state = 'active'",
                params![
                    to_sql(revision)?,
                    to_sql(occurred_at_millis)?,
                    integration_id.0.as_str(),
                    to_sql(expected_revision)?,
                ],
            )
            .map_err(|_| storage_error())?;
        let request_digest = domain_digest(
            b"winwincode.integration.credential-revoked.v1",
            integration_id.0.as_bytes(),
        );
        insert_audit(
            &transaction,
            scope,
            integration_id,
            IntegrationAuditKind::CredentialRevoked,
            &request_digest,
            occurred_at_millis,
        )?;
        let authority = load_authority_row(&transaction, integration_id)?
            .ok_or_else(corrupt)?
            .0;
        transaction.commit().map_err(|_| storage_error())?;
        Ok(authority)
    }

    /// Persists one authenticated normalized event and, only when current,
    /// one formal-command dispatch in the same transaction.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, changed event reuse, tenant mismatch, revoked
    /// credentials, or storage failure.
    pub fn accept_inbound(
        &mut self,
        expected_authority: &ConnectorAuthority,
        request: &crate::InboundWebhookRequest,
        normalized: &NormalizedInboundEvent,
    ) -> Result<InboundReceipt, IntegrationError> {
        let event_key = request.event_key();
        let ordering_key_digest = request.ordering_key_digest();
        let payload_digest = request.payload_digest();
        let transaction = self.transaction()?;
        let (current, _) = load_authority_row(&transaction, request.integration_id())?
            .ok_or_else(not_found_error)?;
        require_scope(request.scope(), &current)?;
        require_exact_active(expected_authority, &current)?;
        if let Some(receipt) =
            load_inbound_receipt(&transaction, request.integration_id(), &event_key, true)?
        {
            if receipt.payload_digest() != &payload_digest
                || receipt.ordering_key_digest() != &ordering_key_digest
                || receipt.provider_sequence() != request.provider_sequence()
                || receipt.command_digest() != normalized.command_digest()
            {
                return Err(conflict_error());
            }
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(receipt);
        }
        let last_sequence = load_last_provider_sequence(
            &transaction,
            request.integration_id(),
            &ordering_key_digest,
        )?;
        let status = if request.provider_sequence() <= last_sequence {
            InboundStatus::IgnoredOutOfOrder
        } else {
            InboundStatus::Accepted
        };
        transaction
            .execute(
                "INSERT INTO integration_inbound_receipts
                 (integration_id, event_key, ordering_key_digest, payload_digest,
                  provider_sequence, status, command_digest, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    request.integration_id().0.as_str(),
                    event_key.0,
                    ordering_key_digest.0,
                    payload_digest.0,
                    to_sql(request.provider_sequence())?,
                    inbound_status_value(status),
                    normalized.command_digest().0,
                    to_sql(request.received_at_millis())?,
                ],
            )
            .map_err(|_| storage_error())?;
        if status == InboundStatus::Accepted {
            transaction
                .execute(
                    "INSERT INTO integration_inbound_ordering
                     (integration_id, ordering_key_digest, last_provider_sequence)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(integration_id, ordering_key_digest) DO UPDATE SET
                       last_provider_sequence = excluded.last_provider_sequence",
                    params![
                        request.integration_id().0.as_str(),
                        ordering_key_digest.0,
                        to_sql(request.provider_sequence())?,
                    ],
                )
                .map_err(|_| storage_error())?;
            transaction
                .execute(
                    "INSERT INTO integration_inbound_dispatches
                     (integration_id, event_key, command_name, command_payload, command_digest)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        request.integration_id().0.as_str(),
                        event_key.0,
                        normalized.command_name(),
                        normalized.command_payload(),
                        normalized.command_digest().0,
                    ],
                )
                .map_err(|_| storage_error())?;
        }
        insert_audit(
            &transaction,
            request.scope(),
            request.integration_id(),
            if status == InboundStatus::Accepted {
                IntegrationAuditKind::InboundAccepted
            } else {
                IntegrationAuditKind::InboundIgnored
            },
            &payload_digest,
            request.received_at_millis(),
        )?;
        let receipt =
            load_inbound_receipt(&transaction, request.integration_id(), &event_key, false)?
                .ok_or_else(corrupt)?;
        transaction.commit().map_err(|_| storage_error())?;
        Ok(receipt)
    }

    /// Scans a bounded page of accepted command dispatches.
    ///
    /// # Errors
    ///
    /// Rejects foreign tenant, invalid cursor/limit, corrupt data, or storage failure.
    pub fn inbound_dispatches(
        &self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<InboundDispatch>, IntegrationError> {
        require_page(after_sequence, limit)?;
        let authority = self.authority(scope, integration_id)?;
        require_scope(scope, &authority)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, event_key, command_name, command_payload, command_digest
                 FROM integration_inbound_dispatches
                 WHERE integration_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
            )
            .map_err(|_| storage_error())?;
        let rows = statement
            .query_map(
                params![
                    integration_id.0.as_str(),
                    to_sql(after_sequence)?,
                    i64::try_from(limit).map_err(|_| invalid())?,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(|_| storage_error())?;
        rows.map(|row| {
            let (sequence, event_key, command_name, command_payload, command_digest) =
                row.map_err(|_| storage_error())?;
            let event_key = stored_digest(event_key)?;
            let command_digest = stored_digest(command_digest)?;
            let normalized =
                NormalizedInboundEvent::try_new(command_name.clone(), command_payload.clone())
                    .map_err(|_| corrupt())?;
            if normalized.command_digest() != &command_digest {
                return Err(corrupt());
            }
            Ok(InboundDispatch::from_stored(
                from_sql(sequence)?,
                integration_id.clone(),
                event_key,
                command_name,
                command_payload,
                command_digest,
            ))
        })
        .collect()
    }

    /// Enqueues or exactly replays one outbound operation.
    ///
    /// # Errors
    ///
    /// Rejects revoked credentials, changed operation-key reuse, stale tenant,
    /// or storage failure.
    pub fn enqueue_outbound(
        &mut self,
        request: &OutboundRequest,
    ) -> Result<OutboundEnqueueReceipt, IntegrationError> {
        let transaction = self.transaction()?;
        let authority = load_authority_row(&transaction, request.integration_id())?
            .ok_or_else(not_found_error)?
            .0;
        require_scope(request.scope(), &authority)?;
        require_active(&authority)?;
        if let Some(operation) = load_outbound_operation(
            &transaction,
            request.integration_id(),
            request.operation_key(),
        )? {
            if operation.request_digest() != request.request_digest() {
                return Err(conflict_error());
            }
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(OutboundEnqueueReceipt::new(operation, true));
        }
        let policy = request.retry_policy();
        transaction
            .execute(
                "INSERT INTO integration_outbound_operations
                 (integration_id, operation_key, request_digest, operation_name, payload, state,
                  attempt, eligible_at, lease_id, lease_expires_at, max_attempts,
                  initial_backoff, max_backoff, enqueued_at, completed_at,
                  remote_receipt_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, NULL, NULL,
                         ?7, ?8, ?9, ?6, NULL, NULL)",
                params![
                    request.integration_id().0.as_str(),
                    request.operation_key().digest().0,
                    request.request_digest().0,
                    request.operation_name(),
                    request.payload(),
                    to_sql(request.enqueued_at_millis())?,
                    i64::from(policy.max_attempts()),
                    to_sql(policy.initial_backoff_millis())?,
                    to_sql(policy.max_backoff_millis())?,
                ],
            )
            .map_err(|_| storage_error())?;
        insert_audit(
            &transaction,
            request.scope(),
            request.integration_id(),
            IntegrationAuditKind::OutboundEnqueued,
            request.request_digest(),
            request.enqueued_at_millis(),
        )?;
        let operation = load_outbound_operation(
            &transaction,
            request.integration_id(),
            request.operation_key(),
        )?
        .ok_or_else(corrupt)?;
        transaction.commit().map_err(|_| storage_error())?;
        Ok(OutboundEnqueueReceipt::new(operation, false))
    }

    /// Claims the next due request for one active connector. Expired leases
    /// retain the same operation key and payload on the next attempt.
    ///
    /// # Errors
    ///
    /// Rejects revoked/foreign connector, invalid lease, corrupt state, or storage failure.
    pub fn claim_due(
        &mut self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
        now_millis: u64,
        lease_id: IntegrationLeaseId,
        lease_expires_at_millis: u64,
    ) -> Result<Option<OutboundClaim>, IntegrationError> {
        validate_time(now_millis)?;
        validate_time(lease_expires_at_millis)?;
        if lease_expires_at_millis <= now_millis {
            return Err(invalid());
        }
        let transaction = self.transaction()?;
        let authority = load_authority_row(&transaction, integration_id)?
            .ok_or_else(not_found_error)?
            .0;
        require_scope(scope, &authority)?;
        require_active(&authority)?;
        let stored = select_due_operation(&transaction, integration_id, now_millis)?;
        let Some(stored) = stored else {
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(None);
        };
        if stored.attempt >= stored.retry_policy.max_attempts() {
            dead_letter_expired_claim(&transaction, &authority, &stored, now_millis)?;
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(None);
        }
        let attempt = stored.attempt.checked_add(1).ok_or_else(corrupt)?;
        transaction
            .execute(
                "UPDATE integration_outbound_operations
                 SET state = 'leased', attempt = ?1, lease_id = ?2, lease_expires_at = ?3
                 WHERE integration_id = ?4 AND operation_key = ?5",
                params![
                    i64::from(attempt),
                    lease_id.as_str(),
                    to_sql(lease_expires_at_millis)?,
                    integration_id.0.as_str(),
                    stored.operation_key.digest().0,
                ],
            )
            .map_err(|_| storage_error())?;
        let claim = OutboundClaim::from_stored(
            authority,
            stored.operation_key,
            stored.request_digest,
            stored.operation_name,
            stored.payload,
            attempt,
            lease_id,
        );
        transaction.commit().map_err(|_| storage_error())?;
        Ok(Some(claim))
    }

    /// Records exact successful delivery or replays the same attempt receipt.
    ///
    /// # Errors
    ///
    /// Rejects changed outcome reuse, stale claims, tenant mismatch, or storage failure.
    pub fn record_success(
        &mut self,
        scope: &AuditScope,
        claim: &OutboundClaim,
        remote: &OutboundCallReceipt,
        completed_at_millis: u64,
    ) -> Result<OutboundDeliveryReceipt, IntegrationError> {
        validate_time(completed_at_millis)?;
        let transaction = self.transaction()?;
        require_claim_scope(scope, claim)?;
        if let Some(result) = load_attempt_result(&transaction, claim, false)? {
            let OutboundAttemptResult::Delivered(receipt) = result else {
                return Err(conflict_error());
            };
            if receipt.remote_receipt_digest() != Some(remote.remote_receipt_digest()) {
                return Err(conflict_error());
            }
            if receipt.remote_write_performed() != Some(remote.remote_write_performed()) {
                return Err(conflict_error());
            }
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(replay_delivery(&receipt));
        }
        let operation = require_leased_operation(&transaction, claim)?;
        transaction
            .execute(
                "UPDATE integration_outbound_operations
                 SET state = 'delivered', lease_id = NULL, lease_expires_at = NULL,
                     completed_at = ?1, remote_receipt_digest = ?2
                 WHERE integration_id = ?3 AND operation_key = ?4",
                params![
                    to_sql(completed_at_millis)?,
                    remote.remote_receipt_digest().0,
                    claim.authority().integration_id().0.as_str(),
                    claim.operation_key().digest().0,
                ],
            )
            .map_err(|_| storage_error())?;
        insert_attempt_receipt(
            &transaction,
            claim,
            &NewAttemptReceipt::delivered(remote, completed_at_millis),
        )?;
        insert_audit(
            &transaction,
            scope,
            claim.authority().integration_id(),
            IntegrationAuditKind::OutboundDelivered,
            operation.request_digest(),
            completed_at_millis,
        )?;
        let operation = load_outbound_operation(
            &transaction,
            claim.authority().integration_id(),
            claim.operation_key(),
        )?
        .ok_or_else(corrupt)?;
        let receipt = OutboundDeliveryReceipt::from_stored(
            operation,
            Some(remote.remote_receipt_digest().clone()),
            Some(remote.remote_write_performed()),
            completed_at_millis,
            false,
        );
        transaction.commit().map_err(|_| storage_error())?;
        Ok(receipt)
    }

    /// Records an exact retry/permanent/revoked failure or replays that attempt.
    ///
    /// # Errors
    ///
    /// Rejects changed failure reuse, stale claims, tenant mismatch, or storage failure.
    pub fn record_failure(
        &mut self,
        scope: &AuditScope,
        claim: &OutboundClaim,
        failure: &ConnectorCallError,
        failed_at_millis: u64,
    ) -> Result<OutboundAttemptResult, IntegrationError> {
        validate_time(failed_at_millis)?;
        let transaction = self.transaction()?;
        require_claim_scope(scope, claim)?;
        if let Some(result) = load_attempt_result(&transaction, claim, true)? {
            let (stored_code, stored_kind, stored_failed_at) =
                load_attempt_failure(&transaction, claim)?;
            if stored_code != failure.code() || stored_kind != failure.kind() {
                return Err(conflict_error());
            }
            if let OutboundAttemptResult::RetryScheduled(operation) = &result {
                let policy = load_retry_policy(&transaction, claim)?;
                let expected = retry_at_with_provider_floor(
                    policy,
                    claim.attempt(),
                    stored_failed_at,
                    failure.retry_after_millis(),
                )?;
                if operation.eligible_at_millis() != expected {
                    return Err(conflict_error());
                }
            }
            transaction.commit().map_err(|_| storage_error())?;
            return Ok(mark_attempt_replay(result));
        }
        let operation = require_leased_operation(&transaction, claim)?;
        let policy = load_retry_policy(&transaction, claim)?;
        let dead_letter = failure.kind() != ConnectorCallErrorKind::Retryable
            || claim.attempt() >= policy.max_attempts();
        if dead_letter {
            transaction
                .execute(
                    "UPDATE integration_outbound_operations
                     SET state = 'dead_letter', lease_id = NULL, lease_expires_at = NULL,
                         completed_at = ?1
                     WHERE integration_id = ?2 AND operation_key = ?3",
                    params![
                        to_sql(failed_at_millis)?,
                        claim.authority().integration_id().0.as_str(),
                        claim.operation_key().digest().0,
                    ],
                )
                .map_err(|_| storage_error())?;
            if failure.kind() == ConnectorCallErrorKind::CredentialRevoked {
                revoke_in_transaction(&transaction, claim.authority(), failed_at_millis)?;
            }
            insert_attempt_receipt(
                &transaction,
                claim,
                &NewAttemptReceipt::failure(
                    "dead_lettered",
                    failure,
                    failed_at_millis,
                    failed_at_millis,
                ),
            )?;
            insert_audit(
                &transaction,
                scope,
                claim.authority().integration_id(),
                IntegrationAuditKind::OutboundDeadLettered,
                operation.request_digest(),
                failed_at_millis,
            )?;
        } else {
            let retry_at = retry_at_with_provider_floor(
                policy,
                claim.attempt(),
                failed_at_millis,
                failure.retry_after_millis(),
            )?;
            transaction
                .execute(
                    "UPDATE integration_outbound_operations
                     SET state = 'pending', eligible_at = ?1, lease_id = NULL,
                         lease_expires_at = NULL
                     WHERE integration_id = ?2 AND operation_key = ?3",
                    params![
                        to_sql(retry_at)?,
                        claim.authority().integration_id().0.as_str(),
                        claim.operation_key().digest().0,
                    ],
                )
                .map_err(|_| storage_error())?;
            insert_attempt_receipt(
                &transaction,
                claim,
                &NewAttemptReceipt::failure("retry_scheduled", failure, retry_at, failed_at_millis),
            )?;
            insert_audit(
                &transaction,
                scope,
                claim.authority().integration_id(),
                IntegrationAuditKind::OutboundRetryScheduled,
                operation.request_digest(),
                failed_at_millis,
            )?;
        }
        let result = load_attempt_result(&transaction, claim, true)?.ok_or_else(corrupt)?;
        transaction.commit().map_err(|_| storage_error())?;
        Ok(result)
    }

    /// Reads current outbound state within one tenant.
    ///
    /// # Errors
    ///
    /// Rejects foreign tenant, missing operation, corrupt state, or storage failure.
    pub fn outbound_operation(
        &self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
        operation_key: &IntegrationOperationKey,
    ) -> Result<OutboundOperation, IntegrationError> {
        self.authority(scope, integration_id)?;
        load_outbound_operation(&self.connection, integration_id, operation_key)?
            .ok_or_else(not_found_error)
    }

    /// Scans a bounded secret-safe audit-outbox page for one tenant connector.
    ///
    /// # Errors
    ///
    /// Rejects foreign tenant, invalid cursor/limit, corrupt facts, or storage failure.
    pub fn audit_facts(
        &self,
        scope: &AuditScope,
        integration_id: &EnterpriseIntegrationId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<IntegrationAuditFact>, IntegrationError> {
        require_page(after_sequence, limit)?;
        self.authority(scope, integration_id)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, scope_json, kind, request_digest, occurred_at
                 FROM integration_audit_outbox
                 WHERE integration_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
            )
            .map_err(|_| storage_error())?;
        let rows = statement
            .query_map(
                params![
                    integration_id.0.as_str(),
                    to_sql(after_sequence)?,
                    i64::try_from(limit).map_err(|_| invalid())?,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(|_| storage_error())?;
        rows.map(|row| {
            let (sequence, stored_scope, kind, request_digest, occurred_at) =
                row.map_err(|_| storage_error())?;
            let stored_scope = stored_scope_value(&stored_scope)?;
            if &stored_scope != scope {
                return Err(corrupt());
            }
            Ok(IntegrationAuditFact::from_stored(
                from_sql(sequence)?,
                stored_scope,
                integration_id.clone(),
                parse_audit_kind(&kind)?,
                stored_digest(request_digest)?,
                from_sql(occurred_at)?,
            ))
        })
        .collect()
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, IntegrationError> {
        self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| storage_error())
    }
}

struct StoredOutbound {
    operation_key: IntegrationOperationKey,
    request_digest: Sha256Digest,
    operation_name: String,
    payload: Vec<u8>,
    attempt: u32,
    retry_policy: RetryPolicy,
}

fn load_authority_row(
    connection: &Connection,
    integration_id: &EnterpriseIntegrationId,
) -> Result<Option<(ConnectorAuthority, u64)>, IntegrationError> {
    validate_integration_id(integration_id)?;
    let row = connection
        .query_row(
            "SELECT scope_json, protocol, credential_reference_id, revision, state,
                    registered_at, updated_at
             FROM integration_connectors WHERE integration_id = ?1",
            [integration_id.0.as_str()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?;
    row.map(
        |(scope, protocol, credential, revision, state, registered_at, updated_at)| {
            Ok((
                ConnectorAuthority::from_stored(
                    integration_id.clone(),
                    stored_scope_value(&scope)?,
                    ConnectorProtocol::try_new(protocol).map_err(|_| corrupt())?,
                    CredentialReferenceId(credential),
                    from_sql(revision)?,
                    parse_connector_state(&state)?,
                    from_sql(updated_at)?,
                )?,
                from_sql(registered_at)?,
            ))
        },
    )
    .transpose()
}

fn load_inbound_receipt(
    connection: &Connection,
    integration_id: &EnterpriseIntegrationId,
    event_key: &Sha256Digest,
    replay: bool,
) -> Result<Option<InboundReceipt>, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT ordering_key_digest, payload_digest, provider_sequence, status,
                    command_digest, received_at
             FROM integration_inbound_receipts WHERE integration_id = ?1 AND event_key = ?2",
            params![integration_id.0.as_str(), event_key.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?;
    row.map(
        |(ordering_key, payload, sequence, status, command, received_at)| {
            Ok(
                InboundReceipt::from_stored(crate::model::StoredInboundReceipt {
                    integration_id: integration_id.clone(),
                    event_key: event_key.clone(),
                    ordering_key_digest: stored_digest(ordering_key)?,
                    payload_digest: stored_digest(payload)?,
                    provider_sequence: from_sql(sequence)?,
                    status: parse_inbound_status(&status)?,
                    command_digest: stored_digest(command)?,
                    received_at_millis: from_sql(received_at)?,
                })
                .with_replay(replay),
            )
        },
    )
    .transpose()
}

fn load_last_provider_sequence(
    connection: &Connection,
    integration_id: &EnterpriseIntegrationId,
    ordering_key_digest: &Sha256Digest,
) -> Result<u64, IntegrationError> {
    let value = connection
        .query_row(
            "SELECT last_provider_sequence FROM integration_inbound_ordering
             WHERE integration_id = ?1 AND ordering_key_digest = ?2",
            params![integration_id.0.as_str(), ordering_key_digest.0],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| storage_error())?;
    value.map_or(Ok(0), from_sql)
}

fn load_outbound_operation(
    connection: &Connection,
    integration_id: &EnterpriseIntegrationId,
    operation_key: &IntegrationOperationKey,
) -> Result<Option<OutboundOperation>, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT request_digest, state, attempt, eligible_at
             FROM integration_outbound_operations
             WHERE integration_id = ?1 AND operation_key = ?2",
            params![integration_id.0.as_str(), operation_key.digest().0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?;
    row.map(|(request_digest, state, attempt, eligible_at)| {
        Ok(OutboundOperation::from_stored(
            integration_id.clone(),
            operation_key.clone(),
            stored_digest(request_digest)?,
            parse_outbound_state(&state)?,
            u32::try_from(attempt).map_err(|_| corrupt())?,
            from_sql(eligible_at)?,
        ))
    })
    .transpose()
}

fn select_due_operation(
    connection: &Connection,
    integration_id: &EnterpriseIntegrationId,
    now_millis: u64,
) -> Result<Option<StoredOutbound>, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT operation_key, request_digest, operation_name, payload, attempt,
                    max_attempts, initial_backoff, max_backoff
             FROM integration_outbound_operations
             WHERE integration_id = ?1 AND
               ((state = 'pending' AND eligible_at <= ?2) OR
                (state = 'leased' AND lease_expires_at <= ?2))
             ORDER BY eligible_at, operation_key LIMIT 1",
            params![integration_id.0.as_str(), to_sql(now_millis)?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?;
    row.map(
        |(operation_key, request_digest, operation_name, payload, attempt, max, initial, cap)| {
            Ok(StoredOutbound {
                operation_key: IntegrationOperationKey::from_stored(stored_digest(operation_key)?)?,
                request_digest: stored_digest(request_digest)?,
                operation_name,
                payload,
                attempt: u32::try_from(attempt).map_err(|_| corrupt())?,
                retry_policy: RetryPolicy::try_new(
                    u32::try_from(max).map_err(|_| corrupt())?,
                    from_sql(initial)?,
                    from_sql(cap)?,
                )
                .map_err(|_| corrupt())?,
            })
        },
    )
    .transpose()
}

fn require_leased_operation(
    connection: &Connection,
    claim: &OutboundClaim,
) -> Result<OutboundOperation, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT request_digest, state, attempt, eligible_at, lease_id
             FROM integration_outbound_operations
             WHERE integration_id = ?1 AND operation_key = ?2",
            params![
                claim.authority().integration_id().0.as_str(),
                claim.operation_key().digest().0,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?
        .ok_or_else(not_found_error)?;
    let (request_digest, state, attempt, eligible_at, lease_id) = row;
    if state != "leased"
        || u32::try_from(attempt).map_err(|_| corrupt())? != claim.attempt()
        || lease_id.as_deref() != Some(claim.lease_id().as_str())
        || stored_digest(request_digest.clone())? != *claim.request_digest()
    {
        return Err(conflict_error());
    }
    Ok(OutboundOperation::from_stored(
        claim.authority().integration_id().clone(),
        claim.operation_key().clone(),
        stored_digest(request_digest)?,
        OutboundOperationState::Leased,
        claim.attempt(),
        from_sql(eligible_at)?,
    ))
}

fn load_retry_policy(
    connection: &Connection,
    claim: &OutboundClaim,
) -> Result<RetryPolicy, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT max_attempts, initial_backoff, max_backoff
             FROM integration_outbound_operations
             WHERE integration_id = ?1 AND operation_key = ?2",
            params![
                claim.authority().integration_id().0.as_str(),
                claim.operation_key().digest().0,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|_| storage_error())?;
    RetryPolicy::try_new(
        u32::try_from(row.0).map_err(|_| corrupt())?,
        from_sql(row.1)?,
        from_sql(row.2)?,
    )
    .map_err(|_| corrupt())
}

fn load_attempt_result(
    connection: &Connection,
    claim: &OutboundClaim,
    allow_failure: bool,
) -> Result<Option<OutboundAttemptResult>, IntegrationError> {
    let row = connection
        .query_row(
            "SELECT outcome_kind, remote_receipt_digest, remote_write_performed,
                    result_eligible_at, completed_at
             FROM integration_outbound_attempt_receipts
             WHERE integration_id = ?1 AND operation_key = ?2 AND attempt = ?3 AND lease_id = ?4",
            params![
                claim.authority().integration_id().0.as_str(),
                claim.operation_key().digest().0,
                i64::from(claim.attempt()),
                claim.lease_id().as_str(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<bool>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| storage_error())?;
    row.map(
        |(kind, remote, remote_write_performed, eligible, completed)| {
            let state = match kind.as_str() {
                "delivered" => OutboundOperationState::Delivered,
                "retry_scheduled" if allow_failure => OutboundOperationState::Pending,
                "dead_lettered" if allow_failure => OutboundOperationState::DeadLetter,
                _ => return Err(conflict_error()),
            };
            let operation = OutboundOperation::from_stored(
                claim.authority().integration_id().clone(),
                claim.operation_key().clone(),
                claim.request_digest().clone(),
                state,
                claim.attempt(),
                from_sql(eligible)?,
            );
            let receipt = OutboundDeliveryReceipt::from_stored(
                operation.clone(),
                remote.map(stored_digest).transpose()?,
                remote_write_performed,
                from_sql(completed)?,
                false,
            );
            Ok(match state {
                OutboundOperationState::Delivered => OutboundAttemptResult::Delivered(receipt),
                OutboundOperationState::Pending => OutboundAttemptResult::RetryScheduled(operation),
                OutboundOperationState::DeadLetter => OutboundAttemptResult::DeadLettered(receipt),
                OutboundOperationState::Leased => return Err(corrupt()),
            })
        },
    )
    .transpose()
}

fn load_attempt_failure(
    connection: &Connection,
    claim: &OutboundClaim,
) -> Result<(String, ConnectorCallErrorKind, u64), IntegrationError> {
    let (code, kind, failed_at) = connection
        .query_row(
            "SELECT outcome_code, failure_kind, completed_at
             FROM integration_outbound_attempt_receipts
             WHERE integration_id = ?1 AND operation_key = ?2 AND attempt = ?3",
            params![
                claim.authority().integration_id().0.as_str(),
                claim.operation_key().digest().0,
                i64::from(claim.attempt()),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|_| storage_error())?;
    let kind = kind.ok_or_else(corrupt)?;
    Ok((
        code,
        parse_connector_failure_kind(&kind)?,
        from_sql(failed_at)?,
    ))
}

fn retry_at_with_provider_floor(
    policy: RetryPolicy,
    attempt: u32,
    failed_at: u64,
    retry_after_millis: Option<u64>,
) -> Result<u64, IntegrationError> {
    let policy_retry_at = policy.retry_at(attempt, failed_at)?;
    let Some(retry_after_millis) = retry_after_millis else {
        return Ok(policy_retry_at);
    };
    let provider_retry_at = failed_at
        .checked_add(retry_after_millis)
        .ok_or_else(invalid)?;
    validate_time(provider_retry_at)?;
    Ok(policy_retry_at.max(provider_retry_at))
}

struct NewAttemptReceipt<'a> {
    outcome: &'static str,
    code: &'a str,
    remote: Option<&'a OutboundCallReceipt>,
    failure: Option<ConnectorCallErrorKind>,
    eligible_at: u64,
    completed_at: u64,
}

impl<'a> NewAttemptReceipt<'a> {
    const fn delivered(remote: &'a OutboundCallReceipt, completed_at: u64) -> Self {
        Self {
            outcome: "delivered",
            code: "APPLIED",
            remote: Some(remote),
            failure: None,
            eligible_at: completed_at,
            completed_at,
        }
    }

    fn failure(
        outcome: &'static str,
        failure: &'a ConnectorCallError,
        eligible_at: u64,
        completed_at: u64,
    ) -> Self {
        Self {
            outcome,
            code: failure.code(),
            remote: None,
            failure: Some(failure.kind()),
            eligible_at,
            completed_at,
        }
    }
}

fn insert_attempt_receipt(
    transaction: &Transaction<'_>,
    claim: &OutboundClaim,
    receipt: &NewAttemptReceipt<'_>,
) -> Result<(), IntegrationError> {
    transaction
        .execute(
            "INSERT INTO integration_outbound_attempt_receipts
             (integration_id, operation_key, attempt, lease_id, outcome_kind, failure_kind,
              outcome_code, remote_receipt_digest, remote_write_performed, result_eligible_at,
              completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                claim.authority().integration_id().0.as_str(),
                claim.operation_key().digest().0,
                i64::from(claim.attempt()),
                claim.lease_id().as_str(),
                receipt.outcome,
                receipt.failure.map(connector_failure_kind_value),
                receipt.code,
                receipt
                    .remote
                    .map(OutboundCallReceipt::remote_receipt_digest)
                    .map(|value| &value.0),
                receipt
                    .remote
                    .map(OutboundCallReceipt::remote_write_performed),
                to_sql(receipt.eligible_at)?,
                to_sql(receipt.completed_at)?,
            ],
        )
        .map_err(|_| storage_error())?;
    Ok(())
}

fn dead_letter_expired_claim(
    transaction: &Transaction<'_>,
    authority: &ConnectorAuthority,
    stored: &StoredOutbound,
    now_millis: u64,
) -> Result<(), IntegrationError> {
    transaction
        .execute(
            "UPDATE integration_outbound_operations
             SET state = 'dead_letter', lease_id = NULL, lease_expires_at = NULL, completed_at = ?1
             WHERE integration_id = ?2 AND operation_key = ?3",
            params![
                to_sql(now_millis)?,
                authority.integration_id().0.as_str(),
                stored.operation_key.digest().0,
            ],
        )
        .map_err(|_| storage_error())?;
    insert_audit(
        transaction,
        authority.scope(),
        authority.integration_id(),
        IntegrationAuditKind::OutboundDeadLettered,
        &stored.request_digest,
        now_millis,
    )
}

fn revoke_in_transaction(
    transaction: &Transaction<'_>,
    authority: &ConnectorAuthority,
    now_millis: u64,
) -> Result<(), IntegrationError> {
    let revision = checked_increment(authority.revision())?;
    transaction
        .execute(
            "UPDATE integration_connectors SET state = 'credential_revoked', revision = ?1,
                    updated_at = ?2 WHERE integration_id = ?3 AND revision = ?4",
            params![
                to_sql(revision)?,
                to_sql(now_millis)?,
                authority.integration_id().0.as_str(),
                to_sql(authority.revision())?,
            ],
        )
        .map_err(|_| storage_error())?;
    let request_digest = domain_digest(
        b"winwincode.integration.credential-revoked.v1",
        authority.integration_id().0.as_bytes(),
    );
    insert_audit(
        transaction,
        authority.scope(),
        authority.integration_id(),
        IntegrationAuditKind::CredentialRevoked,
        &request_digest,
        now_millis,
    )
}

fn insert_audit(
    transaction: &Transaction<'_>,
    scope: &AuditScope,
    integration_id: &EnterpriseIntegrationId,
    kind: IntegrationAuditKind,
    request_digest: &Sha256Digest,
    occurred_at: u64,
) -> Result<(), IntegrationError> {
    validate_digest(request_digest)?;
    validate_time(occurred_at)?;
    transaction
        .execute(
            "INSERT INTO integration_audit_outbox
             (scope_json, integration_id, kind, request_digest, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                scope_bytes(scope)?,
                integration_id.0.as_str(),
                audit_kind_value(kind),
                request_digest.0,
                to_sql(occurred_at)?,
            ],
        )
        .map_err(|_| storage_error())?;
    Ok(())
}

fn registration_digest(
    registration: &ConnectorRegistration,
) -> Result<Sha256Digest, IntegrationError> {
    let mut bytes = scope_bytes(registration.scope())?;
    bytes.extend_from_slice(registration.integration_id().0.as_bytes());
    bytes.extend_from_slice(registration.protocol().as_str().as_bytes());
    bytes.extend_from_slice(registration.credential_reference_id().0.as_bytes());
    Ok(domain_digest(
        b"winwincode.integration.connector-registration.v1",
        &bytes,
    ))
}

fn require_exact_active(
    expected: &ConnectorAuthority,
    current: &ConnectorAuthority,
) -> Result<(), IntegrationError> {
    if expected != current {
        return Err(conflict_error());
    }
    require_active(current)
}

fn require_active(authority: &ConnectorAuthority) -> Result<(), IntegrationError> {
    if authority.state() == ConnectorState::Active {
        Ok(())
    } else {
        Err(IntegrationError::new(
            IntegrationErrorKind::CredentialRevoked,
            "connector credential is revoked",
        ))
    }
}

fn require_scope(
    scope: &AuditScope,
    authority: &ConnectorAuthority,
) -> Result<(), IntegrationError> {
    validate_scope(scope)?;
    if authority.scope() == scope {
        Ok(())
    } else {
        Err(tenant_error())
    }
}

fn require_claim_scope(scope: &AuditScope, claim: &OutboundClaim) -> Result<(), IntegrationError> {
    require_scope(scope, claim.authority())
}

fn require_page(after_sequence: u64, limit: usize) -> Result<(), IntegrationError> {
    if after_sequence > crate::model::MAX_SAFE_INTEGER || !(1..=500).contains(&limit) {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn replay_delivery(receipt: &OutboundDeliveryReceipt) -> OutboundDeliveryReceipt {
    OutboundDeliveryReceipt::from_stored(
        receipt.operation().clone(),
        receipt.remote_receipt_digest().cloned(),
        receipt.remote_write_performed(),
        receipt.completed_at_millis(),
        true,
    )
}

fn mark_attempt_replay(result: OutboundAttemptResult) -> OutboundAttemptResult {
    match result {
        OutboundAttemptResult::Delivered(receipt) => {
            OutboundAttemptResult::Delivered(replay_delivery(&receipt))
        }
        OutboundAttemptResult::DeadLettered(receipt) => {
            OutboundAttemptResult::DeadLettered(replay_delivery(&receipt))
        }
        OutboundAttemptResult::RetryScheduled(operation) => {
            OutboundAttemptResult::RetryScheduled(operation)
        }
    }
}

fn stored_scope_value(bytes: &[u8]) -> Result<AuditScope, IntegrationError> {
    let scope: AuditScope = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
    if scope_bytes(&scope)? != bytes {
        return Err(corrupt());
    }
    Ok(scope)
}

fn stored_digest(value: String) -> Result<Sha256Digest, IntegrationError> {
    let digest = Sha256Digest(value);
    validate_digest(&digest).map_err(|_| corrupt())?;
    Ok(digest)
}

fn parse_connector_state(value: &str) -> Result<ConnectorState, IntegrationError> {
    match value {
        "active" => Ok(ConnectorState::Active),
        "credential_revoked" => Ok(ConnectorState::CredentialRevoked),
        _ => Err(corrupt()),
    }
}

fn parse_connector_failure_kind(value: &str) -> Result<ConnectorCallErrorKind, IntegrationError> {
    match value {
        "retryable" => Ok(ConnectorCallErrorKind::Retryable),
        "permanent" => Ok(ConnectorCallErrorKind::Permanent),
        "credential_revoked" => Ok(ConnectorCallErrorKind::CredentialRevoked),
        _ => Err(corrupt()),
    }
}

const fn connector_failure_kind_value(value: ConnectorCallErrorKind) -> &'static str {
    match value {
        ConnectorCallErrorKind::Retryable => "retryable",
        ConnectorCallErrorKind::Permanent => "permanent",
        ConnectorCallErrorKind::CredentialRevoked => "credential_revoked",
    }
}

fn parse_inbound_status(value: &str) -> Result<InboundStatus, IntegrationError> {
    match value {
        "accepted" => Ok(InboundStatus::Accepted),
        "ignored_out_of_order" => Ok(InboundStatus::IgnoredOutOfOrder),
        _ => Err(corrupt()),
    }
}

const fn inbound_status_value(value: InboundStatus) -> &'static str {
    match value {
        InboundStatus::Accepted => "accepted",
        InboundStatus::IgnoredOutOfOrder => "ignored_out_of_order",
    }
}

fn parse_outbound_state(value: &str) -> Result<OutboundOperationState, IntegrationError> {
    match value {
        "pending" => Ok(OutboundOperationState::Pending),
        "leased" => Ok(OutboundOperationState::Leased),
        "delivered" => Ok(OutboundOperationState::Delivered),
        "dead_letter" => Ok(OutboundOperationState::DeadLetter),
        _ => Err(corrupt()),
    }
}

fn parse_audit_kind(value: &str) -> Result<IntegrationAuditKind, IntegrationError> {
    match value {
        "connector_registered" => Ok(IntegrationAuditKind::ConnectorRegistered),
        "credential_revoked" => Ok(IntegrationAuditKind::CredentialRevoked),
        "inbound_accepted" => Ok(IntegrationAuditKind::InboundAccepted),
        "inbound_ignored" => Ok(IntegrationAuditKind::InboundIgnored),
        "outbound_enqueued" => Ok(IntegrationAuditKind::OutboundEnqueued),
        "outbound_delivered" => Ok(IntegrationAuditKind::OutboundDelivered),
        "outbound_retry_scheduled" => Ok(IntegrationAuditKind::OutboundRetryScheduled),
        "outbound_dead_lettered" => Ok(IntegrationAuditKind::OutboundDeadLettered),
        _ => Err(corrupt()),
    }
}

const fn audit_kind_value(value: IntegrationAuditKind) -> &'static str {
    match value {
        IntegrationAuditKind::ConnectorRegistered => "connector_registered",
        IntegrationAuditKind::CredentialRevoked => "credential_revoked",
        IntegrationAuditKind::InboundAccepted => "inbound_accepted",
        IntegrationAuditKind::InboundIgnored => "inbound_ignored",
        IntegrationAuditKind::OutboundEnqueued => "outbound_enqueued",
        IntegrationAuditKind::OutboundDelivered => "outbound_delivered",
        IntegrationAuditKind::OutboundRetryScheduled => "outbound_retry_scheduled",
        IntegrationAuditKind::OutboundDeadLettered => "outbound_dead_lettered",
    }
}

fn checked_increment(value: u64) -> Result<u64, IntegrationError> {
    let next = value.checked_add(1).ok_or_else(corrupt)?;
    validate_count(next)?;
    Ok(next)
}

fn to_sql(value: u64) -> Result<i64, IntegrationError> {
    i64::try_from(value).map_err(|_| invalid())
}

fn from_sql(value: i64) -> Result<u64, IntegrationError> {
    let value = u64::try_from(value).map_err(|_| corrupt())?;
    validate_count(value)?;
    Ok(value)
}

const fn storage_error() -> IntegrationError {
    IntegrationError::new(IntegrationErrorKind::Storage, "integration storage failed")
}

const fn not_found_error() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::NotFound,
        "integration fact was not found",
    )
}

const fn tenant_error() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::TenantMismatch,
        "integration tenant scope does not match",
    )
}

const fn conflict_error() -> IntegrationError {
    IntegrationError::new(
        IntegrationErrorKind::Conflict,
        "integration request conflicts with durable state",
    )
}
