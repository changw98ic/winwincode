// SPDX-License-Identifier: Apache-2.0

//! Sealed old-to-new authority for one scheduler-owned execution replacement.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, RequestId, Sha256Digest, StageRunId};

use crate::{
    ExecutionDispatchAuthority, ExecutionLeaseRecord, ExecutionQueueScope, SqliteStorage,
    StorageError, WorkerSlotAuthority, sql_error,
};

pub(crate) const EXECUTION_SCOPE_REPLACEMENT_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS execution_scope_replacements (
    job_id TEXT NOT NULL,
    successor_attempt INTEGER NOT NULL CHECK (successor_attempt > 1),
    receipt_id TEXT NOT NULL UNIQUE,
    receipt_digest TEXT NOT NULL,
    logical_job_digest TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    stage_run_id TEXT,
    predecessor_lease_json TEXT NOT NULL,
    predecessor_worker_session_id TEXT,
    predecessor_slot_json TEXT,
    successor_lease_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    applied_at TEXT,
    PRIMARY KEY (job_id, successor_attempt)
);
";

/// Durable scheduler seal proving one exact logical Job moved to a fresh lease.
///
/// Fields are private: only the repository scheduler transaction can create the
/// predecessor/successor lineage consumed by Delivery, `ProductSession`, and
/// Worker workspace recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionScopeReplacementAuthority {
    receipt_id: RequestId,
    receipt_digest: Sha256Digest,
    logical_job_digest: Sha256Digest,
    scope: ExecutionQueueScope,
    stage_run_id: Option<StageRunId>,
    predecessor_lease: ExecutionLeaseRecord,
    predecessor_worker_session_id: Option<winwincode_domain::WorkerSessionId>,
    predecessor_slot: Option<WorkerSlotAuthority>,
    successor_lease: ExecutionLeaseRecord,
    created_at: Instant,
    applied_at: Option<Instant>,
}

impl ExecutionScopeReplacementAuthority {
    #[must_use]
    pub const fn receipt_id(&self) -> &RequestId {
        &self.receipt_id
    }

    #[must_use]
    pub const fn receipt_digest(&self) -> &Sha256Digest {
        &self.receipt_digest
    }

    #[must_use]
    pub const fn logical_job_digest(&self) -> &Sha256Digest {
        &self.logical_job_digest
    }

    #[must_use]
    pub const fn job_id(&self) -> &ExecutionJobId {
        &self.successor_lease.job_id
    }

    #[must_use]
    pub const fn scope(&self) -> &ExecutionQueueScope {
        &self.scope
    }

    #[must_use]
    pub const fn stage_run_id(&self) -> Option<&StageRunId> {
        self.stage_run_id.as_ref()
    }

    #[must_use]
    pub const fn previous_attempt(&self) -> u64 {
        self.predecessor_lease.attempt
    }

    #[must_use]
    pub const fn replacement_attempt(&self) -> u64 {
        self.successor_lease.attempt
    }

    #[must_use]
    pub const fn previous_lease_id(&self) -> &winwincode_domain::LeaseId {
        &self.predecessor_lease.lease_id
    }

    #[must_use]
    pub const fn predecessor_lease(&self) -> &ExecutionLeaseRecord {
        &self.predecessor_lease
    }

    #[must_use]
    pub const fn predecessor_slot(&self) -> Option<&WorkerSlotAuthority> {
        self.predecessor_slot.as_ref()
    }

    #[must_use]
    pub fn previous_worker_session_id(&self) -> Option<&winwincode_domain::WorkerSessionId> {
        self.predecessor_worker_session_id.as_ref()
    }

    #[must_use]
    pub const fn replacement_lease(&self) -> &ExecutionLeaseRecord {
        &self.successor_lease
    }

    #[must_use]
    pub const fn created_at(&self) -> &Instant {
        &self.created_at
    }

    #[must_use]
    pub const fn applied_at(&self) -> Option<&Instant> {
        self.applied_at.as_ref()
    }

    #[must_use]
    pub const fn applied(&self) -> bool {
        self.applied_at.is_some()
    }

    /// Verifies that a newly accepted dispatch is the sealed successor.
    #[must_use]
    pub fn authorizes_successor(&self, dispatch: &ExecutionDispatchAuthority) -> bool {
        dispatch.lease() == &self.successor_lease
    }
}

pub(crate) struct NewExecutionScopeReplacement<'facts> {
    pub receipt_id: &'facts RequestId,
    pub logical_job_digest: &'facts Sha256Digest,
    pub scope: &'facts ExecutionQueueScope,
    pub stage_run_id: Option<&'facts StageRunId>,
    pub predecessor_lease: &'facts ExecutionLeaseRecord,
    pub predecessor_worker_session_id: Option<&'facts winwincode_domain::WorkerSessionId>,
    pub predecessor_slot: Option<&'facts WorkerSlotAuthority>,
    pub successor_lease: &'facts ExecutionLeaseRecord,
    pub created_at: &'facts Instant,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplacementSeal<'facts> {
    receipt_id: &'facts RequestId,
    logical_job_digest: &'facts Sha256Digest,
    scope: &'facts ExecutionQueueScope,
    stage_run_id: Option<&'facts StageRunId>,
    predecessor_lease: &'facts ExecutionLeaseRecord,
    predecessor_worker_session_id: Option<&'facts winwincode_domain::WorkerSessionId>,
    predecessor_slot: Option<&'facts WorkerSlotAuthority>,
    successor_lease: &'facts ExecutionLeaseRecord,
    created_at: &'facts Instant,
}

pub(crate) fn insert_execution_scope_replacement(
    connection: &Connection,
    facts: &NewExecutionScopeReplacement<'_>,
) -> Result<ExecutionScopeReplacementAuthority, StorageError> {
    connection
        .execute_batch(EXECUTION_SCOPE_REPLACEMENT_SCHEMA)
        .map_err(sql_error)?;
    let seal = ReplacementSeal {
        receipt_id: facts.receipt_id,
        logical_job_digest: facts.logical_job_digest,
        scope: facts.scope,
        stage_run_id: facts.stage_run_id,
        predecessor_lease: facts.predecessor_lease,
        predecessor_worker_session_id: facts.predecessor_worker_session_id,
        predecessor_slot: facts.predecessor_slot,
        successor_lease: facts.successor_lease,
        created_at: facts.created_at,
    };
    let receipt_digest = replacement_seal_digest(&seal)?;
    connection
        .execute(
            "INSERT INTO execution_scope_replacements
                (job_id, successor_attempt, receipt_id, receipt_digest, logical_job_digest, scope_json,
                 stage_run_id, predecessor_lease_json, predecessor_worker_session_id,
                 predecessor_slot_json,
                 successor_lease_json, created_at, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)",
            params![
                facts.successor_lease.job_id.0,
                i64::try_from(facts.successor_lease.attempt)
                    .map_err(|_| StorageError::invalid_input("replacement attempt is invalid"))?,
                facts.receipt_id.0,
                receipt_digest.0,
                facts.logical_job_digest.0,
                encode(facts.scope)?,
                facts.stage_run_id.map(|stage_run_id| stage_run_id.0.as_str()),
                encode(facts.predecessor_lease)?,
                facts
                    .predecessor_worker_session_id
                    .map(|worker_session_id| worker_session_id.0.as_str()),
                facts.predecessor_slot.map(encode).transpose()?,
                encode(facts.successor_lease)?,
                facts.created_at.0,
            ],
        )
        .map_err(sql_error)?;
    load_execution_scope_replacement(connection, &facts.successor_lease.job_id)?
        .ok_or_else(|| StorageError::adapter("execution replacement authority was not stored"))
}

pub(crate) fn load_execution_scope_replacement(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionScopeReplacementAuthority>, StorageError> {
    connection
        .execute_batch(EXECUTION_SCOPE_REPLACEMENT_SCHEMA)
        .map_err(sql_error)?;
    connection
        .query_row(
            "SELECT receipt_id, receipt_digest, logical_job_digest, scope_json,
                    stage_run_id, predecessor_lease_json, predecessor_worker_session_id,
                    predecessor_slot_json,
                    successor_lease_json, created_at, applied_at
             FROM execution_scope_replacements WHERE job_id = ?1
             ORDER BY successor_attempt DESC LIMIT 1",
            [&job_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?
        .map(|stored| decode_authority(job_id, stored))
        .transpose()
}

type StoredAuthority = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
);

fn decode_authority(
    job_id: &ExecutionJobId,
    stored: StoredAuthority,
) -> Result<ExecutionScopeReplacementAuthority, StorageError> {
    let authority = ExecutionScopeReplacementAuthority {
        receipt_id: RequestId(stored.0),
        receipt_digest: Sha256Digest(stored.1),
        logical_job_digest: Sha256Digest(stored.2),
        scope: decode(&stored.3)?,
        stage_run_id: stored.4.map(StageRunId),
        predecessor_lease: decode(&stored.5)?,
        predecessor_worker_session_id: stored.6.map(winwincode_domain::WorkerSessionId),
        predecessor_slot: stored.7.as_deref().map(decode).transpose()?,
        successor_lease: decode(&stored.8)?,
        created_at: Instant(stored.9),
        applied_at: stored.10.map(Instant),
    };
    if authority.predecessor_lease.job_id != *job_id
        || authority.successor_lease.job_id != *job_id
        || authority.successor_lease.attempt
            != authority.predecessor_lease.attempt.saturating_add(1)
        || authority.predecessor_lease.worker_id != authority.successor_lease.worker_id
        || authority.predecessor_lease.worker_instance_id
            == authority.successor_lease.worker_instance_id
        || authority.predecessor_lease.payload_digest != authority.successor_lease.payload_digest
        || authority.predecessor_slot.as_ref().is_some_and(|slot| {
            authority.predecessor_worker_session_id.as_ref() != Some(&slot.worker_session_id)
        })
        || authority
            .predecessor_slot
            .as_ref()
            .is_some_and(|slot| !slot_matches_lease(slot, &authority.predecessor_lease))
    {
        return Err(StorageError::adapter(
            "execution replacement authority is corrupt",
        ));
    }
    let seal = ReplacementSeal {
        receipt_id: &authority.receipt_id,
        logical_job_digest: &authority.logical_job_digest,
        scope: &authority.scope,
        stage_run_id: authority.stage_run_id.as_ref(),
        predecessor_lease: &authority.predecessor_lease,
        predecessor_worker_session_id: authority.predecessor_worker_session_id.as_ref(),
        predecessor_slot: authority.predecessor_slot.as_ref(),
        successor_lease: &authority.successor_lease,
        created_at: &authority.created_at,
    };
    if replacement_seal_digest(&seal)? != authority.receipt_digest {
        return Err(StorageError::adapter(
            "execution replacement authority digest is corrupt",
        ));
    }
    Ok(authority)
}

fn replacement_seal_digest(seal: &ReplacementSeal<'_>) -> Result<Sha256Digest, StorageError> {
    let encoded = serde_json::to_vec(seal)
        .map_err(|_| StorageError::adapter("execution replacement seal cannot encode"))?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn slot_matches_lease(slot: &WorkerSlotAuthority, lease: &ExecutionLeaseRecord) -> bool {
    slot.job_id == lease.job_id
        && slot.lease_id == lease.lease_id
        && slot.worker_id == lease.worker_id
        && slot.worker_instance_id == lease.worker_instance_id
        && slot.attempt == lease.attempt
        && slot.fencing_token == lease.fencing_token
}

fn encode(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|_| StorageError::adapter("execution replacement fact cannot encode"))
}

fn decode<'de, T: Deserialize<'de>>(value: &'de str) -> Result<T, StorageError> {
    serde_json::from_str(value)
        .map_err(|_| StorageError::adapter("execution replacement fact is corrupt"))
}

impl SqliteStorage {
    /// Loads the one scheduler-sealed old-to-new authority for a logical Job.
    ///
    /// # Errors
    ///
    /// Returns corruption or storage failures; a missing record returns `None`.
    pub fn load_execution_scope_replacement(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionScopeReplacementAuthority>, StorageError> {
        load_execution_scope_replacement(self.connection()?, job_id)
    }

    /// Marks an owner phase applied only under the exact accepted successor.
    ///
    /// # Errors
    ///
    /// Rejects a foreign successor dispatch, changed completion time, or
    /// missing scheduler replacement authority.
    pub fn mark_execution_scope_replacement_applied(
        &mut self,
        dispatch: &ExecutionDispatchAuthority,
        applied_at: &Instant,
    ) -> Result<bool, StorageError> {
        let authority =
            load_execution_scope_replacement(self.connection()?, &dispatch.lease().job_id)?
                .ok_or_else(|| {
                    StorageError::invalid_input("execution replacement authority is missing")
                })?;
        if !authority.authorizes_successor(dispatch) {
            return Err(StorageError::invalid_input(
                "execution replacement successor dispatch is foreign",
            ));
        }
        if let Some(stored) = authority.applied_at() {
            if stored == applied_at {
                return Ok(false);
            }
            return Err(StorageError::invalid_input(
                "execution replacement completion time changed",
            ));
        }
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE execution_scope_replacements SET applied_at = ?1
             WHERE job_id = ?2 AND successor_attempt = ?3
               AND successor_lease_json = ?4 AND applied_at IS NULL",
                params![
                    applied_at.0,
                    dispatch.lease().job_id.0,
                    i64::try_from(dispatch.lease().attempt).map_err(|_| {
                        StorageError::invalid_input("replacement attempt is invalid")
                    })?,
                    encode(dispatch.lease())?
                ],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(StorageError::adapter(
                "execution replacement completion lost its authority",
            ));
        }
        Ok(true)
    }
}
