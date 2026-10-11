// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_REASON_BYTES: usize = 256;

const WORKER_SESSION_CREDENTIAL_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS worker_session_credentials (
    worker_session_credential_id TEXT PRIMARY KEY NOT NULL,
    worker_session_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    worker_launch_grant_id TEXT NOT NULL,
    credential_digest TEXT NOT NULL
        CHECK (length(credential_digest) = 71 AND credential_digest LIKE 'sha256:%'),
    state TEXT NOT NULL CHECK (state IN ('active', 'rotated', 'revoked', 'expired')),
    expires_at TEXT NOT NULL,
    issued_at TEXT NOT NULL,
    ended_at TEXT,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991)
);
CREATE UNIQUE INDEX IF NOT EXISTS worker_session_credentials_one_active_per_session
    ON worker_session_credentials (worker_session_id)
    WHERE state = 'active';
CREATE INDEX IF NOT EXISTS worker_session_credentials_by_session
    ON worker_session_credentials (worker_session_id, state);
CREATE INDEX IF NOT EXISTS worker_session_credentials_by_digest
    ON worker_session_credentials (credential_digest);
CREATE TABLE IF NOT EXISTS worker_session_credentials_audit (
    audit_id TEXT PRIMARY KEY NOT NULL,
    action TEXT NOT NULL CHECK (action IN (
        'issued', 'rotated', 'revoked', 'expired')),
    worker_session_credential_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL,
    reason TEXT,
    occurred_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS worker_session_credentials_audit_by_credential
    ON worker_session_credentials_audit (worker_session_credential_id, occurred_at);
CREATE TABLE IF NOT EXISTS worker_session_credential_renewals (
    audit_id TEXT PRIMARY KEY NOT NULL,
    worker_session_credential_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 1),
    heartbeat_sequence INTEGER NOT NULL CHECK (heartbeat_sequence > 0),
    job_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    previous_expires_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    UNIQUE (worker_session_credential_id, revision)
);
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialState {
    Active,
    Rotated,
    Revoked,
    Expired,
}

impl WorkerSessionCredentialState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkerSessionCredentialStoreError> {
        match value {
            "active" => Ok(Self::Active),
            "rotated" => Ok(Self::Rotated),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            _ => Err(error(
                WorkerSessionCredentialStoreErrorKind::CorruptState,
                "stored worker session credential state is invalid",
            )),
        }
    }
}

impl fmt::Display for WorkerSessionCredentialState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialAuditAction {
    Issued,
    Rotated,
    Revoked,
    Expired,
    Renewed,
}

impl CredentialAuditAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Renewed => "renewed",
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::Issued => "01",
            Self::Rotated => "02",
            Self::Revoked => "03",
            Self::Expired => "04",
            Self::Renewed => "05",
        }
    }
}

#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialIssuance {
    pub(crate) worker_session_credential_id: String,
    pub(crate) worker_session_id: String,
    pub(crate) worker_id: String,
    pub(crate) worker_instance_id: String,
    pub(crate) worker_launch_grant_id: String,
    pub(crate) credential_digest: String,
    pub(crate) expires_at: Instant,
}

impl CredentialIssuance {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        worker_session_credential_id: impl Into<String>,
        worker_session_id: impl Into<String>,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        worker_launch_grant_id: impl Into<String>,
        credential_digest: impl Into<String>,
        expires_at: Instant,
    ) -> Result<Self, WorkerSessionCredentialStoreError> {
        let issuance = Self {
            worker_session_credential_id: worker_session_credential_id.into(),
            worker_session_id: worker_session_id.into(),
            worker_id: worker_id.into(),
            worker_instance_id: worker_instance_id.into(),
            worker_launch_grant_id: worker_launch_grant_id.into(),
            credential_digest: credential_digest.into(),
            expires_at,
        };
        validate_worker_session_credential_id(&issuance.worker_session_credential_id)?;
        validate_worker_session_id(&issuance.worker_session_id)?;
        validate_worker_id(&issuance.worker_id)?;
        validate_worker_instance_id(&issuance.worker_instance_id)?;
        validate_worker_launch_grant_id(&issuance.worker_launch_grant_id)?;
        validate_credential_digest(&issuance.credential_digest)?;
        validate_instant(&issuance.expires_at, "credential expiry")?;
        Ok(issuance)
    }

    #[must_use]
    pub fn worker_session_id(&self) -> &str {
        &self.worker_session_id
    }

    #[must_use]
    pub fn worker_launch_grant_id(&self) -> &str {
        &self.worker_launch_grant_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotation {
    worker_session_id: String,
    replacement_credential_id: String,
    replacement_credential_digest: String,
    replacement_expires_at: Instant,
    reason: Option<String>,
}

impl CredentialRotation {
    pub fn try_new(
        worker_session_id: impl Into<String>,
        replacement_credential_id: impl Into<String>,
        replacement_credential_digest: impl Into<String>,
        replacement_expires_at: Instant,
        reason: Option<&str>,
    ) -> Result<Self, WorkerSessionCredentialStoreError> {
        let rotation = Self {
            worker_session_id: worker_session_id.into(),
            replacement_credential_id: replacement_credential_id.into(),
            replacement_credential_digest: replacement_credential_digest.into(),
            replacement_expires_at,
            reason: reason.map(str::to_owned),
        };
        validate_worker_session_id(&rotation.worker_session_id)?;
        validate_worker_session_credential_id(&rotation.replacement_credential_id)?;
        validate_credential_digest(&rotation.replacement_credential_digest)?;
        validate_instant(&rotation.replacement_expires_at, "replacement expiry")?;
        if let Some(reason) = &rotation.reason
            && (reason.is_empty() || reason.len() > MAX_REASON_BYTES)
        {
            return Err(error(
                WorkerSessionCredentialStoreErrorKind::InvalidInput,
                "rotation reason must contain 1 to 256 bytes",
            ));
        }
        Ok(rotation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotationOutcome {
    pub retired: WorkerSessionCredentialRecord,
    pub issued: WorkerSessionCredentialRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialRecord {
    pub worker_session_credential_id: String,
    pub worker_session_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub worker_launch_grant_id: String,
    pub credential_digest: String,
    pub state: WorkerSessionCredentialState,
    pub expires_at: Instant,
    pub issued_at: Instant,
    pub ended_at: Option<Instant>,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialStoreErrorKind {
    InvalidInput,
    UnknownCredential,
    CredentialConflict,
    IllegalStateTransition,
    RevisionConflict,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialStoreError {
    kind: WorkerSessionCredentialStoreErrorKind,
    message: String,
}

impl WorkerSessionCredentialStoreError {
    #[must_use]
    pub const fn kind(&self) -> WorkerSessionCredentialStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerSessionCredentialStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerSessionCredentialStoreError {}

pub struct WorkerSessionCredentialLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    pub fn worker_session_credential_ledger(
        &mut self,
    ) -> Result<WorkerSessionCredentialLedger<'_>, WorkerSessionCredentialStoreError> {
        WorkerSessionCredentialLedger::new(self)
    }
}

impl<'storage> WorkerSessionCredentialLedger<'storage> {
    fn new(
        storage: &'storage mut SqliteStorage,
    ) -> Result<Self, WorkerSessionCredentialStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(WORKER_SESSION_CREDENTIAL_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    pub fn renew_after_accepted_heartbeat(
        &mut self,
        expected: &WorkerSessionCredentialRecord,
        heartbeat: &winwincode_execution_port::generated::WorkerHeartbeatMessage,
        lease: &winwincode_execution_port::generated::ActiveLeaseSummary,
        expires_at: &Instant,
        now: &Instant,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
        validate_instant(now, "renewal time")?;
        validate_instant(expires_at, "renewal expiry")?;
        validate_crockford_id(&heartbeat.message_id.0, "xmsg_", "heartbeat message id")?;
        if heartbeat.worker_id.0 != expected.worker_id
            || heartbeat.worker_instance_id.0 != expected.worker_instance_id
            || heartbeat.heartbeat_sequence.0 <= 0
            || lease.attempt <= 0
            || expires_at.0 <= expected.expires_at.0
            || now.0 >= expected.expires_at.0
        {
            return Ok(None);
        }
        let transaction = self.transaction()?;
        let updated = transaction.execute(
            "UPDATE worker_session_credentials AS c
             SET expires_at=?2, revision=revision+1
             WHERE c.worker_session_credential_id=?1 AND c.state='active'
               AND c.credential_digest=?3 AND c.revision=?4 AND c.expires_at>?5
               AND c.worker_id=?6 AND c.worker_instance_id=?7
               AND EXISTS (
                 SELECT 1 FROM worker_launch_grants g
                 JOIN device_execution_bindings b
                   ON b.worker_launch_grant_id=g.worker_launch_grant_id
                 JOIN client_occupancy_leases o ON o.occupancy_lease_id=g.occupancy_lease_id
                 JOIN client_nodes n ON n.client_node_id=g.client_node_id
                 WHERE g.worker_launch_grant_id=c.worker_launch_grant_id
                   AND g.worker_session_id=c.worker_session_id
                   AND g.worker_id=c.worker_id AND g.worker_instance_id=c.worker_instance_id
                   AND NOT EXISTS (SELECT 1 FROM worker_launch_grant_exits e WHERE e.worker_launch_grant_id=g.worker_launch_grant_id)
                   AND g.state='consumed'
                   AND b.state='bound' AND b.worker_session_id=c.worker_session_id
                   AND b.client_node_id=g.client_node_id
                   AND b.client_instance_id=g.client_instance_id
                   AND n.current_instance_id=g.client_instance_id
                   AND b.holder_user_id=g.holder_user_id AND o.holder_user_id=g.holder_user_id
                   AND b.repository_binding_id=g.repository_binding_id
                   AND b.occupancy_lease_id=o.occupancy_lease_id
                   AND b.occupancy_fencing_token=o.fencing_token
                   AND g.occupancy_fencing_token=o.fencing_token
                   AND o.client_node_id=g.client_node_id AND o.state IN ('occupied','draining','recovery_pending')
               )
               AND EXISTS (
                 SELECT 1 FROM execution_leases l
                 JOIN device_execution_current_facts f ON f.job_id=l.job_id
                 JOIN execution_dispatch_authorities d ON d.job_id=l.job_id AND d.lease_id=l.lease_id
                 JOIN execution_workers w ON w.worker_id=l.worker_id
                 JOIN execution_heartbeats h ON h.worker_id=w.worker_id
                   AND h.worker_instance_id=w.worker_instance_id
                 WHERE l.job_id=?8 AND l.lease_id=?9 AND l.attempt=?10
                   AND l.fencing_token=?11 AND l.worker_id=c.worker_id
                   AND l.worker_instance_id=c.worker_instance_id AND l.expires_at>?5
                   AND f.worker_launch_grant_id=c.worker_launch_grant_id
                   AND f.worker_session_id=c.worker_session_id
                   AND f.worker_id=c.worker_id AND f.worker_instance_id=c.worker_instance_id
                   AND d.worker_id=l.worker_id AND d.worker_instance_id=l.worker_instance_id
                   AND d.attempt=l.attempt AND d.fencing_token=l.fencing_token
                   AND w.worker_instance_id=c.worker_instance_id
                   AND w.heartbeat_sequence=?12 AND h.heartbeat_sequence=?12
                   AND NOT EXISTS (SELECT 1 FROM execution_lease_terminals t
                     WHERE t.lease_id=l.lease_id)
               )",
            params![expected.worker_session_credential_id, expires_at.0,
                expected.credential_digest, i64::try_from(expected.revision)
                    .map_err(|_| invalid_id("credential revision"))?, now.0, expected.worker_id,
                expected.worker_instance_id, lease.job_id.0, lease.lease_id.0,
                lease.attempt, lease.fencing_token.0, heartbeat.heartbeat_sequence.0],
        ).map_err(|sql| sql_error(&sql))?;
        if updated == 0 {
            return Ok(None);
        }
        let record = load_credential(&transaction, &expected.worker_session_credential_id)?
            .ok_or_else(credential_missing_after_write)?;
        let audit_digest = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}",
                record.worker_session_credential_id, record.revision
            ))
        );
        let audit_id = format!("wca_{}", audit_digest[..26].to_ascii_uppercase());
        transaction.execute(
            "INSERT INTO worker_session_credential_renewals VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![audit_id, record.worker_session_credential_id, record.worker_session_id,
                i64::try_from(record.revision).map_err(|_| invalid_id("credential revision"))?,
                heartbeat.heartbeat_sequence.0, lease.job_id.0, lease.lease_id.0,
                expected.expires_at.0, expires_at.0, now.0],
        ).map_err(|sql| sql_error(&sql))?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(Some(record))
    }

    pub fn issue(
        &mut self,
        issuance: &CredentialIssuance,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
        let transaction = self.transaction()?;
        let stored = issue_credential_in_transaction(&transaction, issuance, now)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(stored)
    }

    pub fn rotate(
        &mut self,
        rotation: &CredentialRotation,
        now: &Instant,
    ) -> Result<CredentialRotationOutcome, WorkerSessionCredentialStoreError> {
        validate_instant(now, "rotation time")?;
        let transaction = self.transaction()?;
        let retired = require_active_credential(&transaction, &rotation.worker_session_id)?;
        let updated = transaction
            .execute(
                "UPDATE worker_session_credentials
                 SET state = 'rotated', ended_at = ?2, revision = revision + 1
                 WHERE worker_session_credential_id = ?1 AND state = 'active'",
                params![retired.worker_session_credential_id, now.0],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("rotation"));
        }
        insert_audit(
            &transaction,
            CredentialAuditAction::Rotated,
            &retired.worker_session_credential_id,
            &retired.worker_session_id,
            &retired.worker_session_id,
            rotation.reason.as_deref(),
            now,
        )?;
        let replacement = CredentialIssuance::try_new(
            &rotation.replacement_credential_id,
            &retired.worker_session_id,
            &retired.worker_id,
            &retired.worker_instance_id,
            &retired.worker_launch_grant_id,
            &rotation.replacement_credential_digest,
            rotation.replacement_expires_at.clone(),
        )?;
        let inserted = transaction
            .execute(
                "INSERT INTO worker_session_credentials
                 (worker_session_credential_id, worker_session_id, worker_id,
                  worker_instance_id, worker_launch_grant_id, credential_digest,
                  state, expires_at, issued_at, ended_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, NULL, 1)",
                params![
                    replacement.worker_session_credential_id,
                    replacement.worker_session_id,
                    replacement.worker_id,
                    replacement.worker_instance_id,
                    replacement.worker_launch_grant_id,
                    replacement.credential_digest,
                    replacement.expires_at.0,
                    now.0,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if inserted != 1 {
            return Err(error(
                WorkerSessionCredentialStoreErrorKind::Storage,
                "credential rotation insert did not store exactly one row",
            ));
        }
        insert_audit(
            &transaction,
            CredentialAuditAction::Issued,
            &replacement.worker_session_credential_id,
            &replacement.worker_session_id,
            &replacement.worker_session_id,
            None,
            now,
        )?;
        let retired_record = load_credential(&transaction, &retired.worker_session_credential_id)?
            .ok_or_else(credential_missing_after_write)?;
        let issued_record =
            load_credential(&transaction, &replacement.worker_session_credential_id)?
                .ok_or_else(credential_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(CredentialRotationOutcome {
            retired: retired_record,
            issued: issued_record,
        })
    }

    pub fn revoke_for_session(
        &mut self,
        worker_session_id: &str,
        actor_user_id: &str,
        reason: Option<&str>,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
        validate_worker_session_id(worker_session_id)?;
        validate_user_id(actor_user_id)?;
        if let Some(reason) = reason
            && (reason.is_empty() || reason.len() > MAX_REASON_BYTES)
        {
            return Err(error(
                WorkerSessionCredentialStoreErrorKind::InvalidInput,
                "revocation reason must contain 1 to 256 bytes",
            ));
        }
        validate_instant(now, "revocation time")?;
        let transaction = self.transaction()?;
        let record = require_active_credential(&transaction, worker_session_id)?;
        let updated = transaction
            .execute(
                "UPDATE worker_session_credentials
                 SET state = 'revoked', ended_at = ?2, revision = revision + 1
                 WHERE worker_session_credential_id = ?1 AND state = 'active'",
                params![record.worker_session_credential_id, now.0],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("revocation"));
        }
        insert_audit(
            &transaction,
            CredentialAuditAction::Revoked,
            &record.worker_session_credential_id,
            &record.worker_session_id,
            actor_user_id,
            reason,
            now,
        )?;
        let revoked = load_credential(&transaction, &record.worker_session_credential_id)?
            .ok_or_else(credential_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(revoked)
    }

    pub fn expire(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, WorkerSessionCredentialStoreError> {
        validate_instant(cutoff, "expiry cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT worker_session_credential_id FROM worker_session_credentials
                 WHERE state = 'active' AND expires_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let candidates = statement
            .query_map([cutoff.0.as_str()], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut expired = Vec::with_capacity(candidates.len());
        for worker_session_credential_id in candidates {
            let record = load_credential(&transaction, &worker_session_credential_id)?
                .ok_or_else(credential_missing_after_write)?;
            let updated = transaction
                .execute(
                    "UPDATE worker_session_credentials
                     SET state = 'expired', ended_at = ?2, revision = revision + 1
                     WHERE worker_session_credential_id = ?1 AND state = 'active'",
                    params![worker_session_credential_id, cutoff.0],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated != 1 {
                continue;
            }
            insert_audit(
                &transaction,
                CredentialAuditAction::Expired,
                &record.worker_session_credential_id,
                &record.worker_session_id,
                &record.worker_session_id,
                Some("expiry deadline passed"),
                cutoff,
            )?;
            expired.push(worker_session_credential_id);
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(expired)
    }

    pub fn snapshot(
        &self,
        worker_session_credential_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
        validate_worker_session_credential_id(worker_session_credential_id)?;
        load_credential(self.connection()?, worker_session_credential_id)
    }

    pub fn active_for_session(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
        validate_worker_session_id(worker_session_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT worker_session_credential_id FROM worker_session_credentials
                 WHERE worker_session_id = ?1 AND state = 'active'",
            )
            .map_err(|sql| sql_error(&sql))?;
        let ids = statement
            .query_map([worker_session_id], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        match ids.len() {
            0 => Ok(None),
            1 => load_credential(connection, &ids[0]),
            _ => Err(error(
                WorkerSessionCredentialStoreErrorKind::CorruptState,
                "worker session holds more than one active credential",
            )),
        }
    }

    pub fn find_by_digest(
        &self,
        credential_digest: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
        validate_credential_digest(credential_digest)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT worker_session_credential_id FROM worker_session_credentials
                 WHERE credential_digest = ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let ids = statement
            .query_map([credential_digest], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        match ids.len() {
            0 => Ok(None),
            1 => load_credential(connection, &ids[0]),
            _ => Err(error(
                WorkerSessionCredentialStoreErrorKind::CorruptState,
                "one credential digest matches more than one stored credential",
            )),
        }
    }

    pub fn audit_trail(
        &self,
        worker_session_credential_id: &str,
    ) -> Result<Vec<CredentialAuditEntry>, WorkerSessionCredentialStoreError> {
        validate_worker_session_credential_id(worker_session_credential_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT audit_id, action, worker_session_credential_id,
                        worker_session_id, actor_user_id, reason, occurred_at
                 FROM worker_session_credentials_audit
                 WHERE worker_session_credential_id = ?1
                 UNION ALL
                 SELECT audit_id,'renewed',worker_session_credential_id,worker_session_id,
                        worker_session_id,NULL,occurred_at
                 FROM worker_session_credential_renewals WHERE worker_session_credential_id=?1
                 ORDER BY occurred_at, audit_id",
            )
            .map_err(|sql| sql_error(&sql))?;
        let entries = statement
            .query_map([worker_session_credential_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        entries
            .into_iter()
            .map(CredentialAuditEntry::from_row)
            .collect()
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, WorkerSessionCredentialStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, WorkerSessionCredentialStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialAuditEntry {
    pub audit_id: String,
    pub action: CredentialAuditAction,
    pub worker_session_credential_id: String,
    pub worker_session_id: String,
    pub actor_user_id: String,
    pub reason: Option<String>,
    pub occurred_at: Instant,
}

impl CredentialAuditEntry {
    #[allow(clippy::type_complexity)]
    fn from_row(
        row: (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        ),
    ) -> Result<Self, WorkerSessionCredentialStoreError> {
        let (
            audit_id,
            action,
            worker_session_credential_id,
            worker_session_id,
            actor_user_id,
            reason,
            occurred_at,
        ) = row;
        let action = match action.as_str() {
            "issued" => CredentialAuditAction::Issued,
            "rotated" => CredentialAuditAction::Rotated,
            "revoked" => CredentialAuditAction::Revoked,
            "expired" => CredentialAuditAction::Expired,
            "renewed" => CredentialAuditAction::Renewed,
            _ => {
                return Err(error(
                    WorkerSessionCredentialStoreErrorKind::CorruptState,
                    "stored credential audit action is invalid",
                ));
            }
        };
        Ok(Self {
            audit_id,
            action,
            worker_session_credential_id,
            worker_session_id,
            actor_user_id,
            reason,
            occurred_at: parse_stored_instant(&occurred_at, "credential audit instant")?,
        })
    }
}

fn require_active_credential(
    connection: &rusqlite::Connection,
    worker_session_id: &str,
) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
    let mut statement = connection
        .prepare(
            "SELECT worker_session_credential_id FROM worker_session_credentials
             WHERE worker_session_id = ?1 AND state = 'active'",
        )
        .map_err(|sql| sql_error(&sql))?;
    let ids = statement
        .query_map([worker_session_id], |row| row.get::<_, String>(0))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    drop(statement);
    match ids.len() {
        0 => Err(error(
            WorkerSessionCredentialStoreErrorKind::UnknownCredential,
            "the worker session carries no active credential",
        )),
        1 => load_credential(connection, &ids[0])?.ok_or_else(credential_missing_after_write),
        _ => Err(error(
            WorkerSessionCredentialStoreErrorKind::CorruptState,
            "worker session holds more than one active credential",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_audit(
    transaction: &Transaction<'_>,
    action: CredentialAuditAction,
    worker_session_credential_id: &str,
    worker_session_id: &str,
    actor_user_id: &str,
    reason: Option<&str>,
    now: &Instant,
) -> Result<(), WorkerSessionCredentialStoreError> {
    let audit_id = audit_identity(action, worker_session_credential_id)?;
    let inserted = transaction
        .execute(
            "INSERT INTO worker_session_credentials_audit
             (audit_id, action, worker_session_credential_id, worker_session_id,
              actor_user_id, reason, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                audit_id,
                action.as_str(),
                worker_session_credential_id,
                worker_session_id,
                actor_user_id,
                reason,
                now.0,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if inserted != 1 {
        return Err(error(
            WorkerSessionCredentialStoreErrorKind::Storage,
            "credential audit insert did not store exactly one row",
        ));
    }
    Ok(())
}

fn audit_identity(
    action: CredentialAuditAction,
    worker_session_credential_id: &str,
) -> Result<String, WorkerSessionCredentialStoreError> {
    let suffix = worker_session_credential_id
        .strip_prefix("wcred_")
        .ok_or_else(|| {
            error(
                WorkerSessionCredentialStoreErrorKind::InvalidInput,
                "worker session credential id is not canonical",
            )
        })?;
    let mut identity = String::with_capacity(4 + 26);
    identity.push_str("wca_");
    identity.push_str(action.code());
    identity.push_str(&suffix[..24]);
    Ok(identity)
}

fn load_credential(
    connection: &rusqlite::Connection,
    worker_session_credential_id: &str,
) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
    connection
        .query_row(
            "SELECT worker_session_credential_id, worker_session_id, worker_id,
                    worker_instance_id, worker_launch_grant_id, credential_digest,
                    state, expires_at, issued_at, ended_at, revision
             FROM worker_session_credentials WHERE worker_session_credential_id = ?1",
            [worker_session_credential_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(credential_record_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn credential_record_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
    ),
) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
    let (
        worker_session_credential_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        worker_launch_grant_id,
        credential_digest,
        state,
        expires_at,
        issued_at,
        ended_at,
        revision,
    ) = row;
    Ok(WorkerSessionCredentialRecord {
        worker_session_credential_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        worker_launch_grant_id,
        credential_digest,
        state: WorkerSessionCredentialState::parse(&state)?,
        expires_at: parse_stored_instant(&expires_at, "credential expiry")?,
        issued_at: parse_stored_instant(&issued_at, "credential issuance")?,
        ended_at: ended_at
            .map(|value| parse_stored_instant(&value, "credential end"))
            .transpose()?,
        revision: from_sql_integer(revision, "credential revision")?,
    })
}

fn validate_schema(
    connection: &rusqlite::Connection,
) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_columns(
        connection,
        "worker_session_credentials",
        &[
            "worker_session_credential_id",
            "worker_session_id",
            "worker_id",
            "worker_instance_id",
            "worker_launch_grant_id",
            "credential_digest",
            "state",
            "expires_at",
            "issued_at",
            "ended_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "worker_session_credentials_audit",
        &[
            "audit_id",
            "action",
            "worker_session_credential_id",
            "worker_session_id",
            "actor_user_id",
            "reason",
            "occurred_at",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), WorkerSessionCredentialStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            WorkerSessionCredentialStoreErrorKind::CorruptState,
            "worker session credential ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), WorkerSessionCredentialStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid_id(label));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(invalid_id(label));
    }
    Ok(())
}

fn invalid_id(label: &str) -> WorkerSessionCredentialStoreError {
    error(
        WorkerSessionCredentialStoreErrorKind::InvalidInput,
        format!("{label} is not canonical"),
    )
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn validate_worker_session_credential_id(
    value: &str,
) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wcred_", "worker session credential id")
}

fn validate_worker_session_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wsn_", "worker session id")
}

fn validate_worker_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wrk_", "worker id")
}

fn validate_worker_instance_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wki_", "worker instance id")
}

fn validate_worker_launch_grant_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wlg_", "worker launch grant id")
}

fn validate_user_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_credential_digest(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(error(
            WorkerSessionCredentialStoreErrorKind::InvalidInput,
            "credential digest is not a sha256 digest",
        ));
    };
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(error(
            WorkerSessionCredentialStoreErrorKind::InvalidInput,
            "credential digest is not a lowercase sha256 digest",
        ))
    }
}

fn validate_instant(value: &Instant, label: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    let bytes = value.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    let valid = bytes.len() == 24
        && bytes[23] == b'Z'
        && punctuation
            .iter()
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || index == 23 || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(error(
            WorkerSessionCredentialStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(
    value: &str,
    label: &str,
) -> Result<Instant, WorkerSessionCredentialStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn map_credential_insert_sql(sql: &rusqlite::Error) -> WorkerSessionCredentialStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                WorkerSessionCredentialStoreErrorKind::CredentialConflict,
                "the worker session already carries an active credential",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                WorkerSessionCredentialStoreErrorKind::CredentialConflict,
                "worker session credential id is already used",
            ),
            _ => error(
                WorkerSessionCredentialStoreErrorKind::InvalidInput,
                "worker session credential violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn credential_missing_after_write() -> WorkerSessionCredentialStoreError {
    error(
        WorkerSessionCredentialStoreErrorKind::CorruptState,
        "worker session credential row is missing after the write",
    )
}

fn cas_lost(action: &str) -> WorkerSessionCredentialStoreError {
    error(
        WorkerSessionCredentialStoreErrorKind::RevisionConflict,
        format!("worker session credential compare-and-swap lost during {action}"),
    )
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, WorkerSessionCredentialStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            WorkerSessionCredentialStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            WorkerSessionCredentialStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> WorkerSessionCredentialStoreError {
    error(
        WorkerSessionCredentialStoreErrorKind::Storage,
        format!("worker session credential ledger storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> WorkerSessionCredentialStoreError {
    error(
        WorkerSessionCredentialStoreErrorKind::Storage,
        "worker session credential ledger storage operation failed",
    )
}

fn error(
    kind: WorkerSessionCredentialStoreErrorKind,
    message: impl Into<String>,
) -> WorkerSessionCredentialStoreError {
    WorkerSessionCredentialStoreError {
        kind,
        message: message.into(),
    }
}

pub(crate) fn issue_credential_in_transaction(
    connection: &Transaction<'_>,
    issuance: &CredentialIssuance,
    now: &Instant,
) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
    validate_instant(now, "issue time")?;
    let inserted = connection
        .execute(
            "INSERT INTO worker_session_credentials
                 (worker_session_credential_id, worker_session_id, worker_id,
                  worker_instance_id, worker_launch_grant_id, credential_digest,
                  state, expires_at, issued_at, ended_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, NULL, 1)",
            params![
                issuance.worker_session_credential_id,
                issuance.worker_session_id,
                issuance.worker_id,
                issuance.worker_instance_id,
                issuance.worker_launch_grant_id,
                issuance.credential_digest,
                issuance.expires_at.0,
                now.0,
            ],
        )
        .map_err(|sql| map_credential_insert_sql(&sql))?;
    if inserted != 1 {
        return Err(error(
            WorkerSessionCredentialStoreErrorKind::Storage,
            "worker session credential insert did not store exactly one row",
        ));
    }
    insert_audit(
        connection,
        CredentialAuditAction::Issued,
        &issuance.worker_session_credential_id,
        &issuance.worker_session_id,
        &issuance.worker_session_id,
        None,
        now,
    )?;
    let record = load_credential(connection, &issuance.worker_session_credential_id)?
        .ok_or_else(credential_missing_after_write)?;
    Ok(record)
}
