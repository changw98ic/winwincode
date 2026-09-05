// SPDX-License-Identifier: Apache-2.0

//! Durable Server-side `WorkerSessionCredential` ledger and its audit trail.
//!
//! Short-lived Worker session credentials replace the shared static Worker
//! bearer (plan 17.2): exactly one credential backs one `WorkerSession` and
//! one `WorkerInstance`, and it authorizes exactly the one
//! `WorkerLaunchGrant` it was issued beside. A credential is created
//! `active`, and the frozen `active | rotated | revoked | expired` state
//! machine is the whole lifetime:
//!
//! - `rotate` retires the session's `active` credential and inserts its
//!   replacement inside one immediate transaction, so a worker session never
//!   holds two live credentials and there is no acceptance window where the
//!   old material still authenticates;
//! - `revoke_for_session` terminates the `active` credential immediately —
//!   a running worker loses its proof on its very next exchange;
//! - `expire` is the sweep that retires every `active` credential whose
//!   deadline passed; verification itself already refuses an expired
//!   credential, so the sweep is durable hygiene over the same rule.
//!
//! Uniqueness: at most one `active` credential may exist per
//! `worker_session_id` — the partial unique index is the durable backstop.
//!
//! Credentials: only the `sha256:` digest of the 32-byte material is stored.
//! The raw material never enters this ledger; it crosses the launch response
//! and the device chain once and is dropped (plan 17.2).
//!
//! Audit: every issuance, rotation, revocation, and expiry is appended to
//! `worker_session_credentials_audit` inside the same transaction as the
//! state change. Audit identities derive deterministically from the
//! credential suffix and the action, so each of the at-most-once transitions
//! records exactly one row without the storage layer drawing entropy.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
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
";

/// Lifecycle state of one `WorkerSessionCredential` (plan 17.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialState {
    /// The material authenticates the bound worker session and instance.
    Active,
    /// Terminal: a rotation replaced this credential; its material is dead.
    Rotated,
    /// Terminal: revoked; the material is dead immediately.
    Revoked,
    /// Terminal: the expiry deadline passed.
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

/// Credential decision recorded in the credential audit trail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialAuditAction {
    /// The credential was issued (initial issuance or rotation replacement).
    Issued,
    /// A rotation retired this credential in favor of its replacement.
    Rotated,
    /// The credential was revoked.
    Revoked,
    /// The expiry sweep terminated the credential.
    Expired,
}

impl CredentialAuditAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    /// Two-character audit identity discriminator. Crockford Base32 digits
    /// only, so the derived audit id stays canonical.
    const fn code(self) -> &'static str {
        match self {
            Self::Issued => "01",
            Self::Rotated => "02",
            Self::Revoked => "03",
            Self::Expired => "04",
        }
    }
}

/// Validated atomic issue command (plan 17.2).
///
/// The `_id` postfix on every field is the plan's own domain vocabulary, so
/// the lint against repeated field suffixes is intentionally allowed here.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialIssuance {
    worker_session_credential_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
    worker_launch_grant_id: String,
    credential_digest: String,
    expires_at: Instant,
}

impl CredentialIssuance {
    /// Builds one validated issue command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a non-`sha256:` credential digest,
    /// or a non-canonical expiry before any durable write.
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

/// Validated rotation command (plan 17.2): the session's `active` credential
/// is retired and replaced by the named replacement inside one transaction.
/// The worker identities and the bound launch grant are inherited from the
/// retired credential, so a rotation can never re-bind a credential to
/// another worker session, worker instance, or launch grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotation {
    worker_session_id: String,
    replacement_credential_id: String,
    replacement_credential_digest: String,
    replacement_expires_at: Instant,
    reason: Option<String>,
}

impl CredentialRotation {
    /// Builds one validated rotation command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a non-`sha256:` replacement digest,
    /// a non-canonical expiry, or an out-of-bounds reason.
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

/// Outcome of one rotation: the retired credential and its replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRotationOutcome {
    /// The previously `active` credential, now terminal `rotated`.
    pub retired: WorkerSessionCredentialRecord,
    /// The replacement, `active` from the rotation instant on.
    pub issued: WorkerSessionCredentialRecord,
}

/// Durable `WorkerSessionCredential` row (plan 17.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSessionCredentialRecord {
    /// Stable credential identifier.
    pub worker_session_credential_id: String,
    /// Worker session the credential authorizes.
    pub worker_session_id: String,
    /// Worker identity the credential authorizes.
    pub worker_id: String,
    /// Worker instance the credential authorizes.
    pub worker_instance_id: String,
    /// The one launch grant this credential authorizes.
    pub worker_launch_grant_id: String,
    /// `sha256:` digest of the credential material; never the material.
    pub credential_digest: String,
    /// Lifecycle state.
    pub state: WorkerSessionCredentialState,
    /// Deadline after which the credential no longer authenticates.
    pub expires_at: Instant,
    /// Instant the credential was issued.
    pub issued_at: Instant,
    /// Terminal instant; set on `rotated`, `revoked`, and `expired`.
    pub ended_at: Option<Instant>,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Stable credential ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSessionCredentialStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// No credential matches the requested identity.
    UnknownCredential,
    /// The worker session already carries an `active` credential.
    CredentialConflict,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race that should be impossible inside
    /// one immediate transaction.
    RevisionConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free credential ledger error.
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

/// Worker session credential ledger borrowing the sole product-state `SQLite`
/// authority.
pub struct WorkerSessionCredentialLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable worker session credential ledger on this same
    /// product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
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

    /// Atomically issues one worker session credential (plan 17.2). The
    /// `active` row and its `issued` audit entry commit together. The worker
    /// session must not already carry an `active` credential.
    ///
    /// # Errors
    ///
    /// Rejects an `active` credential already present for the session, a
    /// reused credential id, or storage failure.
    pub fn issue(
        &mut self,
        issuance: &CredentialIssuance,
        now: &Instant,
    ) -> Result<WorkerSessionCredentialRecord, WorkerSessionCredentialStoreError> {
        validate_instant(now, "issue time")?;
        let transaction = self.transaction()?;
        let inserted = transaction
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
            &transaction,
            CredentialAuditAction::Issued,
            &issuance.worker_session_credential_id,
            &issuance.worker_session_id,
            &issuance.worker_session_id,
            None,
            now,
        )?;
        let record = load_credential(&transaction, &issuance.worker_session_credential_id)?
            .ok_or_else(credential_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Atomically rotates one worker session's credential (plan 17.2): the
    /// `active` credential becomes terminal `rotated` and its replacement
    /// becomes `active` inside one immediate transaction, so the old
    /// material stops authenticating at the same instant the new material
    /// starts. The replacement inherits the retired credential's worker
    /// identities and launch grant.
    ///
    /// # Errors
    ///
    /// Rejects a session without an `active` credential or storage failure.
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

    /// Revokes the session's `active` credential immediately (plan 17.2): the
    /// material stops authenticating at once, whether the worker session is
    /// still launching or already running.
    ///
    /// # Errors
    ///
    /// Rejects a session without an `active` credential, an over-long
    /// reason, or storage failure.
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

    /// Expires every `active` credential whose expiry deadline is at or
    /// before `cutoff` (plan 17.2). Returns the expired credential ids.
    ///
    /// Verification already refuses an expired credential, so this sweep is
    /// the durable state transition over the same rule.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
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

    /// Returns one durable credential projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical credential identity, corrupt stored rows, or
    /// storage failure.
    pub fn snapshot(
        &self,
        worker_session_credential_id: &str,
    ) -> Result<Option<WorkerSessionCredentialRecord>, WorkerSessionCredentialStoreError> {
        validate_worker_session_credential_id(worker_session_credential_id)?;
        load_credential(self.connection()?, worker_session_credential_id)
    }

    /// Returns the one `active` credential of a worker session, if any.
    ///
    /// More than one `active` row is a corrupt database and fails closed.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical worker session identity, a corrupt active
    /// set, or storage failure.
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

    /// Looks one credential up by its stored `sha256:` digest — the durable
    /// half of digest verification. State and expiry are judged by the
    /// caller so every authentication failure stays one uniform category.
    ///
    /// More than one row for one digest is a corrupt database and fails
    /// closed.
    ///
    /// # Errors
    ///
    /// Rejects a non-`sha256:` digest shape, a corrupt digest match, or
    /// storage failure.
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

    /// Returns every durable credential audit entry of one credential,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical credential identity or storage failure.
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

/// One credential audit row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialAuditEntry {
    /// Stable audit identity.
    pub audit_id: String,
    /// Recorded action.
    pub action: CredentialAuditAction,
    /// Credential the entry refers to.
    pub worker_session_credential_id: String,
    /// Worker session the credential authorizes.
    pub worker_session_id: String,
    /// User that drove the transition; self-audits carry the session id.
    pub actor_user_id: String,
    /// Machine-readable reason, when one exists.
    pub reason: Option<String>,
    /// Instant the entry was recorded.
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

/// Loads the session's one `active` credential or fails with the unknown
/// category; the partial unique index keeps the active set a singleton.
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

/// Appends one credential audit row inside the caller's transaction. Audit
/// identities derive from the credential suffix and the action code, so each
/// of the at-most-once transitions records exactly one row.
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

/// Deterministic audit identity: the action discriminator followed by the
/// first 24 characters of the credential suffix. Every action happens at
/// most once per credential, so identities never collide.
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
    validate_crockford_id(value, "ws_", "worker session id")
}

fn validate_worker_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wkr_", "worker id")
}

fn validate_worker_instance_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "winst_", "worker instance id")
}

fn validate_worker_launch_grant_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "wlg_", "worker launch grant id")
}

fn validate_user_id(value: &str) -> Result<(), WorkerSessionCredentialStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

/// Validates the canonical `sha256:` + 64 lowercase hex digest shape.
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

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
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
            // The realistic unique violation is the one-active-per-session
            // partial index; a credential id reuse shares the extended code
            // family and fails closed as a conflict too.
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
