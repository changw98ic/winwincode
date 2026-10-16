// SPDX-License-Identifier: Apache-2.0

use crate::{
    ClientNodeRegistration, ExecutionJobCancellationRequest, ExecutionJobState,
    ExecutionJobTransitionRequest, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionReservationRelease, ExecutionReservationReleaseReason, ExecutionReservationState,
    RepositorySchedulerScope, RepositorySchedulerTerminalRequest, SqliteStorage, StorageError,
    execution_admission, execution_queue, execution_registry, repository_scheduler, sql_error,
    worker_session_slots,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, RequestId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionOutcomeUsage;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS device_pairing_tokens (
 token_digest TEXT PRIMARY KEY NOT NULL, owner_user_id TEXT NOT NULL,
 expires_at TEXT NOT NULL, consumed_request_id TEXT
);
CREATE TABLE IF NOT EXISTS device_identity_bindings (
 client_node_id TEXT PRIMARY KEY NOT NULL REFERENCES client_nodes(client_node_id),
 public_key TEXT UNIQUE NOT NULL, owner_user_id TEXT NOT NULL,
 request_id TEXT UNIQUE NOT NULL, intent_digest TEXT NOT NULL,
 token_digest TEXT NOT NULL, response_json TEXT NOT NULL,
 bound_at TEXT NOT NULL, revoked_at TEXT
);
CREATE TABLE IF NOT EXISTS device_auth_challenges (
 challenge_id TEXT PRIMARY KEY NOT NULL, public_key TEXT NOT NULL,
 challenge TEXT NOT NULL, expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS device_auth_events (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT, client_node_id TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('device.bound','device.revoked')), occurred_at TEXT NOT NULL
);
";

pub(crate) fn create_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction.execute_batch(SCHEMA).map_err(sql_error)
}

pub(crate) fn validate_schema(connection: &Connection) -> Result<(), StorageError> {
    for query in [
        "SELECT token_digest,owner_user_id,expires_at,consumed_request_id FROM device_pairing_tokens LIMIT 0",
        "SELECT client_node_id,public_key,owner_user_id,request_id,intent_digest,token_digest,response_json,bound_at,revoked_at FROM device_identity_bindings LIMIT 0",
        "SELECT challenge_id,public_key,challenge,expires_at FROM device_auth_challenges LIMIT 0",
        "SELECT sequence,client_node_id,kind,occurred_at FROM device_auth_events LIMIT 0",
    ] {
        connection.prepare(query).map_err(sql_error)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthStoreError {
    Unavailable,
    Authentication,
    PairingRejected,
    InvalidRequest,
    Conflict,
    Revoked,
    NotFound,
}

impl From<rusqlite::Error> for DeviceAuthStoreError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Unavailable
    }
}
impl From<StorageError> for DeviceAuthStoreError {
    fn from(_: StorageError) -> Self {
        Self::Unavailable
    }
}

pub struct DeviceBindingWrite<'a> {
    pub request_id: &'a str,
    pub public_key: &'a str,
    pub token_digest: &'a str,
    pub intent_digest: &'a str,
    pub registration: &'a ClientNodeRegistration,
    pub access_grant_id: &'a str,
    pub now: &'a str,
}

impl SqliteStorage {
    pub fn device_identity_owned_by(
        &self,
        node: &str,
        owner: &str,
    ) -> Result<bool, DeviceAuthStoreError> {
        self.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE client_node_id=?1 AND owner_user_id=?2 AND revoked_at IS NULL)",params![node,owner],|row|row.get(0)).map_err(Into::into)
    }

    pub fn active_remote_worker_instances(
        &mut self,
        now: &str,
    ) -> Result<Vec<(String, String)>, DeviceAuthStoreError> {
        self.client_node_registry()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.worker_launch_grant_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.client_occupancy_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        let mut statement=self.connection()?.prepare("SELECT DISTINCT g.worker_id,g.worker_instance_id FROM worker_launch_grants g JOIN client_nodes n ON n.client_node_id=g.client_node_id JOIN client_occupancy_leases o ON o.occupancy_lease_id=g.occupancy_lease_id WHERE g.state IN ('issued','consumed') AND g.ended_at IS NULL AND (g.state='consumed' OR g.expires_at>?1) AND n.presence_state!='revoked' AND n.current_instance_id=g.client_instance_id AND o.state IN ('occupied','draining','recovery_pending') AND o.client_node_id=g.client_node_id AND o.fencing_token=g.occupancy_fencing_token AND NOT EXISTS(SELECT 1 FROM worker_launch_grant_exits e WHERE e.worker_launch_grant_id=g.worker_launch_grant_id) AND NOT EXISTS(SELECT 1 FROM worker_launch_process_closures c WHERE c.grant_id=g.worker_launch_grant_id AND c.reporting_client_instance_id=g.client_instance_id)")?;
        statement
            .query_map([now], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn initialize_device_auth(&mut self) -> Result<(), DeviceAuthStoreError> {
        self.client_node_registry()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.client_connect_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.connection()?.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn publish_device_pairing(
        &mut self,
        digest: &str,
        owner: &str,
        expiry: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        self.connection()?.execute("INSERT INTO device_pairing_tokens(token_digest,owner_user_id,expires_at) VALUES(?1,?2,?3)",params![digest,owner,expiry])?;
        Ok(())
    }

    pub fn publish_device_challenge(
        &mut self,
        id: &str,
        key: &str,
        challenge: &str,
        expiry: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM device_auth_challenges WHERE expires_at<=?1",
            [now],
        )?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM device_auth_challenges WHERE substr(challenge_id,1,6)!='bound_'",
            [],
            |row| row.get(0),
        )?;
        if count >= 4096 {
            return Err(DeviceAuthStoreError::Unavailable);
        }
        tx.execute(
            "INSERT INTO device_auth_challenges VALUES(?1,?2,?3,?4)",
            params![id, key, challenge, expiry],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn device_challenge_secret(
        &mut self,
        key: &str,
    ) -> Result<Option<String>, DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        self.connection()?
            .query_row(
                "SELECT token_digest FROM device_identity_bindings WHERE public_key=?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn consume_bound_device_challenge(
        &mut self,
        id: &str,
        key: &str,
        challenge: &str,
        expiry: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        if expiry <= now {
            return Err(DeviceAuthStoreError::Authentication);
        }
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM device_auth_challenges WHERE expires_at<=?1",
            [now],
        )?;
        if tx.execute(
            "INSERT OR IGNORE INTO device_auth_challenges VALUES(?1,?2,?3,?4)",
            params![id, key, challenge, expiry],
        )? != 1
        {
            return Err(DeviceAuthStoreError::Authentication);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn device_challenge(
        &mut self,
        id: &str,
        key: &str,
        now: &str,
    ) -> Result<String, DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        self.connection()?.query_row("SELECT challenge FROM device_auth_challenges WHERE challenge_id=?1 AND public_key=?2 AND expires_at>?3 AND substr(challenge_id,1,6)!='bound_'",params![id,key,now],|r|r.get(0)).optional()?.ok_or(DeviceAuthStoreError::Authentication)
    }

    pub fn consume_device_challenge(
        &mut self,
        id: &str,
        key: &str,
        challenge: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        if self.connection()?.execute("DELETE FROM device_auth_challenges WHERE challenge_id=?1 AND public_key=?2 AND challenge=?3 AND expires_at>?4 AND substr(challenge_id,1,6)!='bound_'",params![id,key,challenge,now])? !=1 { return Err(DeviceAuthStoreError::Authentication); }
        Ok(())
    }

    pub fn authenticate_device_binding(
        &mut self,
        node: &str,
        key: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let record:Option<(Option<String>,String)>=self.connection()?.query_row("SELECT b.revoked_at,n.presence_state FROM device_identity_bindings b JOIN client_nodes n USING(client_node_id) WHERE b.client_node_id=?1 AND b.public_key=?2",params![node,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        match record {
            Some((None, state)) if state != "revoked" => Ok(()),
            Some(_) => Err(DeviceAuthStoreError::Revoked),
            None => Err(DeviceAuthStoreError::Authentication),
        }
    }

    pub fn bind_device_identity(
        &mut self,
        write: &DeviceBindingWrite<'_>,
    ) -> Result<String, DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior:Option<(String,String,String,String,Option<String>,String)>=tx.query_row("SELECT b.public_key,b.intent_digest,b.token_digest,b.response_json,b.revoked_at,n.presence_state FROM device_identity_bindings b JOIN client_nodes n USING(client_node_id) WHERE request_id=?1",[write.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        if let Some((key, intent, token, response, revoked, presence)) = prior {
            if key != write.public_key
                || intent != write.intent_digest
                || token != write.token_digest
            {
                return Err(DeviceAuthStoreError::Conflict);
            }
            if revoked.is_some() || presence == "revoked" {
                return Err(DeviceAuthStoreError::Revoked);
            }
            return Ok(response);
        }
        let owner:String=tx.query_row("SELECT owner_user_id FROM device_pairing_tokens WHERE token_digest=?1 AND consumed_request_id IS NULL AND expires_at>?2",params![write.token_digest,write.now],|r|r.get(0)).optional()?.ok_or(DeviceAuthStoreError::PairingRejected)?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE public_key=?1)",
            [write.public_key],
            |r| r.get(0),
        )?;
        if existing {
            return Err(DeviceAuthStoreError::Conflict);
        }
        let r = write.registration;
        tx.execute("INSERT INTO client_nodes(client_node_id,public_client_id,display_name,platform,architecture,client_version,current_instance_id,created_at,revision) VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,1)",params![r.client_node_id(),r.public_client_id(),r.display_name(),r.platform(),r.architecture(),r.client_version(),write.now])?;
        tx.execute(
            "INSERT INTO client_exchange_cursors(client_node_id) VALUES(?1)",
            [r.client_node_id()],
        )?;
        let response=serde_json::json!({"schemaVersion":"winwincode/v1","requestId":write.request_id,"clientNodeId":r.client_node_id(),"publicClientId":r.public_client_id(),"ownerUserId":owner,"boundAt":write.now}).to_string();
        tx.execute(
            "INSERT INTO device_identity_bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
            params![
                r.client_node_id(),
                write.public_key,
                owner,
                write.request_id,
                write.intent_digest,
                write.token_digest,
                response,
                write.now
            ],
        )?;
        tx.execute("INSERT INTO client_access_grants VALUES(?1,?2,?3,'use+manage+share','trusted','active','administrator',?3,NULL,?4,1)",params![write.access_grant_id,r.client_node_id(),owner,write.now])?;
        tx.execute(
            "UPDATE device_pairing_tokens SET consumed_request_id=?2 WHERE token_digest=?1",
            params![write.token_digest, write.request_id],
        )?;
        tx.execute("INSERT INTO device_auth_events(client_node_id,kind,occurred_at) VALUES(?1,'device.bound',?2)",params![r.client_node_id(),write.now])?;
        tx.commit()?;
        Ok(response)
    }

    pub fn revoke_device_identity(
        &mut self,
        node: &str,
        owner: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE client_node_id=?1 AND owner_user_id=?2)",params![node,owner],|r|r.get(0))?;
        if !authorized {
            return Err(DeviceAuthStoreError::NotFound);
        }
        let newly_revoked=tx.execute("UPDATE device_identity_bindings SET revoked_at=?2 WHERE client_node_id=?1 AND revoked_at IS NULL",params![node,now])?;
        if newly_revoked == 0 {
            tx.commit()?;
            return Ok(());
        }
        tx.execute("UPDATE client_nodes SET presence_state='revoked',accepting_connections=0,revision=revision+1 WHERE client_node_id=?1 AND presence_state!='revoked'",[node])?;
        tx.execute("UPDATE client_access_grants SET state='revoked',revision=revision+1 WHERE client_node_id=?1 AND state='active'",[node])?;
        settle_revoked_executions(&tx, node, &Instant(now.to_owned()))?;
        for (table, sql) in [
            (
                "client_occupancy_leases",
                "UPDATE client_occupancy_leases SET state='released',release_reason='force_released',idle_expires_at=NULL,recovery_deadline_at=NULL,revision=revision+1 WHERE client_node_id=?1 AND state IN ('reserving','occupied','draining','recovery_pending')",
            ),
            (
                "worker_launch_grants",
                "UPDATE worker_launch_grants SET state='revoked',ended_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state IN ('issued','consumed')",
            ),
            (
                "device_execution_bindings",
                "UPDATE device_execution_bindings SET state='released',released_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state='bound'",
            ),
            (
                "device_scheduler_reservations",
                "UPDATE device_scheduler_reservations SET state='released',release_reason='expired',updated_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state!='released'",
            ),
        ] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )?;
            if exists {
                let mut statement = tx.prepare(sql)?;
                if statement.parameter_count() == 2 {
                    statement.execute(params![node, now])?;
                } else {
                    statement.execute([node])?;
                }
            }
        }
        tx.execute("INSERT INTO device_auth_events(client_node_id,kind,occurred_at) VALUES(?1,'device.revoked',?2)",params![node,now])?;
        tx.commit()?;
        Ok(())
    }
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, DeviceAuthStoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn settle_revoked_executions(
    transaction: &rusqlite::Transaction<'_>,
    node: &str,
    now: &Instant,
) -> Result<(), DeviceAuthStoreError> {
    if !table_exists(transaction, "worker_launch_grants")? {
        return Ok(());
    }
    if table_exists(transaction, "execution_workers")? {
        let workers = transaction.prepare(
            "SELECT DISTINCT worker_id,worker_instance_id FROM worker_launch_grants WHERE client_node_id=?1",
        )?.query_map([node], |row| Ok((WorkerId(row.get(0)?),WorkerInstanceId(row.get(1)?))))?
            .collect::<Result<Vec<_>,_>>()?;
        for (worker, instance) in workers {
            execution_registry::mark_worker_disconnected_in_transaction(
                transaction,
                &worker,
                &instance,
            )?;
        }
    }
    if table_exists(transaction, "worker_session_slots")? {
        worker_session_slots::fence_device_worker_sessions_in_transaction(transaction, node, now)
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
    }
    if !table_exists(transaction, "execution_leases")? {
        return Ok(());
    }
    let mut jobs = transaction.prepare(
        "SELECT DISTINCT l.job_id FROM execution_leases l JOIN worker_launch_grants g ON g.worker_id=l.worker_id AND g.worker_instance_id=l.worker_instance_id WHERE g.client_node_id=?1 AND NOT EXISTS(SELECT 1 FROM execution_lease_terminals t WHERE t.lease_id=l.lease_id) ORDER BY l.job_id",
    )?.query_map([node], |row| Ok(ExecutionJobId(row.get(0)?)))?
        .collect::<Result<Vec<_>,_>>()?;
    if table_exists(transaction, "device_execution_reservation_facts")?
        && table_exists(transaction, "device_execution_bindings")?
        && table_exists(transaction, "scheduler_execution_jobs")?
    {
        jobs.extend(
            transaction
                .prepare(
                    "SELECT r.job_id FROM execution_admission_reservations r
             JOIN scheduler_execution_jobs j USING(job_id)
             LEFT JOIN device_execution_current_facts f USING(job_id)
             WHERE r.state IN ('queued','running') AND j.state IN ('queued','cancelling')
             AND NOT EXISTS(SELECT 1 FROM execution_leases l WHERE l.job_id=r.job_id)
             AND (f.client_node_id=?1 OR (f.job_id IS NULL AND EXISTS(
                 SELECT 1 FROM device_execution_bindings b JOIN worker_launch_grants g
                 ON g.worker_launch_grant_id=b.worker_launch_grant_id
                 WHERE b.client_node_id=?1 AND g.client_node_id=b.client_node_id
                 AND g.worker_launch_grant_id=(SELECT newest.worker_launch_grant_id
                     FROM worker_launch_grants newest
                     WHERE (j.work_run_id IS NOT NULL AND newest.work_run_id=j.work_run_id)
                        OR (j.work_run_id IS NULL AND newest.work_run_id IS NULL
                            AND newest.product_session_id=j.product_session_id)
                     ORDER BY newest.created_at DESC,newest.worker_launch_grant_id DESC LIMIT 1)
                 AND ((j.work_run_id IS NOT NULL AND j.work_run_id=b.work_run_id)
                      OR (j.work_run_id IS NULL AND b.work_run_id IS NULL
                          AND j.product_session_id=b.product_session_id))))) ORDER BY r.job_id",
                )?
                .query_map([node], |row| Ok(ExecutionJobId(row.get(0)?)))?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    for job in jobs {
        settle_revoked_execution(transaction, node, &job, now)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn settle_revoked_execution(
    transaction: &rusqlite::Transaction<'_>,
    node: &str,
    job_id: &ExecutionJobId,
    now: &Instant,
) -> Result<(), DeviceAuthStoreError> {
    let lease = execution_registry::load_lease_in_transaction(transaction, job_id)?;
    let job = if table_exists(transaction, "scheduler_execution_jobs")? {
        repository_scheduler::load_execution_job_by_id(transaction, job_id)?
    } else {
        None
    };
    if job.as_ref().is_some_and(|job| {
        matches!(
            job.state,
            ExecutionJobState::Completed | ExecutionJobState::Failed
        )
    }) {
        return Ok(());
    }
    let digest = format!(
        "{:X}",
        Sha256::digest(format!(
            "winwincode.device-revocation.v1\0{node}\0{}",
            lease.as_ref().map_or(&job_id.0, |lease| &lease.lease_id.0)
        ))
    );
    let request_id = RequestId(format!("req_{}", &digest[..26]));
    if let Some(lease) = lease.as_ref() {
        let terminal = ExecutionLeaseTerminalRequest {
            job_id: lease.job_id.clone(),
            lease_id: lease.lease_id.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            attempt: lease.attempt,
            fencing_token: lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Cancelled,
            terminal_at: now.clone(),
            request_id: request_id.clone(),
        };
        if let Some(job) = job {
            if job.attempt != lease.attempt || job.payload_digest != lease.payload_digest {
                return Err(DeviceAuthStoreError::Conflict);
            }
            if job.state == ExecutionJobState::Leased {
                execution_queue::transition_execution_job_in_transaction(
                    transaction,
                    &ExecutionJobTransitionRequest {
                        scope: job.scope,
                        job_id: job.job_id,
                        request_id: request_id.clone(),
                        expected_revision: job.revision,
                        from: ExecutionJobState::Leased,
                        to: ExecutionJobState::Failed,
                        occurred_at: now.clone(),
                    },
                )?;
                execution_registry::finish_execution_lease_in_transaction(transaction, &terminal)?;
            } else {
                repository_scheduler::settle_terminal_in_transaction(
                    transaction,
                    &RepositorySchedulerTerminalRequest {
                        scope: RepositorySchedulerScope {
                            organization_id: job.scope.organization_id,
                            workspace_id: job.scope.workspace_id,
                            project_id: job.scope.project_id,
                            repository_id: job.scope.repository_id,
                        },
                        terminal,
                    },
                )?;
            }
        } else {
            execution_registry::finish_execution_lease_in_transaction(transaction, &terminal)?;
        }
    } else {
        let mut job = job.ok_or(DeviceAuthStoreError::Unavailable)?;
        if job.state == ExecutionJobState::Queued {
            job = execution_queue::cancel_execution_job_in_transaction(
                transaction,
                &ExecutionJobCancellationRequest {
                    scope: job.scope,
                    job_id: job.job_id,
                    request_id: RequestId(format!("req_{}", &digest[26..52])),
                    expected_revision: job.revision,
                    requested_at: now.clone(),
                },
            )?
            .job;
        }
        if job.state != ExecutionJobState::Cancelling {
            return Err(DeviceAuthStoreError::Conflict);
        }
        execution_queue::transition_execution_job_in_transaction(
            transaction,
            &ExecutionJobTransitionRequest {
                scope: job.scope,
                job_id: job.job_id,
                request_id: request_id.clone(),
                expected_revision: job.revision,
                from: job.state,
                to: ExecutionJobState::Failed,
                occurred_at: now.clone(),
            },
        )?;
    }
    if table_exists(transaction, "execution_admission_reservations")?
        && let Some(reservation) =
            execution_admission::load_reservation_by_job_in_transaction(transaction, job_id)
                .map_err(|_| DeviceAuthStoreError::Unavailable)?
        && matches!(
            reservation.state,
            ExecutionReservationState::Queued | ExecutionReservationState::Running
        )
    {
        let usage = if let Some(lease) = lease.as_ref() {
            let runtime_millis = transaction.query_row(
                "SELECT CAST(ROUND((julianday(?1)-julianday(?2))*86400000) AS INTEGER)",
                params![
                    now.0,
                    reservation
                        .started_at
                        .as_ref()
                        .unwrap_or(&lease.issued_at)
                        .0
                ],
                |row| row.get(0),
            )?;
            let known_tokens = if table_exists(transaction, "execution_attempt_accounting_totals")?
            {
                transaction.query_row("SELECT COALESCE(SUM(known_tokens),0) FROM execution_attempt_accounting_totals WHERE job_id=?1",[&job_id.0],|row| row.get(0))?
            } else {
                0
            };
            let usage = ExecutionOutcomeUsage::unknown(runtime_millis, known_tokens);
            if !winwincode_execution_port::usage::valid_usage(&usage) {
                return Err(DeviceAuthStoreError::InvalidRequest);
            }
            Some(usage)
        } else {
            None
        };
        execution_admission::release_reservation_in_transaction(
            transaction,
            &ExecutionReservationRelease {
                scope: reservation.scope,
                worker_pool_id: reservation.worker_pool_id,
                job_id: job_id.clone(),
                request_id,
                expected_revision: reservation.revision,
                reason: ExecutionReservationReleaseReason::Cancelled,
                released_at: now.clone(),
            },
            usage.as_ref(),
        )
        .map_err(|_| DeviceAuthStoreError::Unavailable)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION,
        ExecutionAdmissionBoundary, ExecutionJobSubmission, ExecutionQueueScope,
        ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationSettlement,
        ExecutionReservationStart, RepositorySchedulerClaimRequest,
        RepositorySchedulerDispatchResultRequest, WorkerAuthenticationIdentity,
        WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
        WorkerSlotAuthority, WorkerSlotCloseRequest, WorkerSlotOpenRequest,
        WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
    };
    use winwincode_domain::{
        CodexThreadId, ExecutionMessageId, ExecutionSequence, OrganizationId, ProductSessionId,
        ProjectId, RepositoryId, Sha256Digest, UserId, WorkerSessionId, WorkspaceId,
    };

    fn id(prefix: &str, seed: u64) -> String {
        format!("{prefix}_{seed:026}")
    }

    fn at(second: u64) -> Instant {
        Instant(format!("2026-01-01T00:00:{second:02}.000Z"))
    }

    fn repository() -> RepositorySchedulerScope {
        RepositorySchedulerScope {
            organization_id: OrganizationId(id("org", 1)),
            workspace_id: WorkspaceId(id("wsp", 1)),
            project_id: ProjectId(id("prj", 1)),
            repository_id: RepositoryId(id("rep", 1)),
        }
    }

    fn bind_device(storage: &mut SqliteStorage, seed: u64) {
        let token = format!("token-{seed}");
        storage
            .publish_device_pairing(&token, &id("usr", 1), &at(59).0)
            .unwrap();
        let registration = ClientNodeRegistration::try_new(
            id("cnd", seed),
            format!("{seed:012}"),
            "Device",
            "aarch64-apple-darwin",
            "aarch64",
            "0.2.0",
            None,
            4,
        )
        .unwrap();
        storage
            .bind_device_identity(&DeviceBindingWrite {
                request_id: &id("req", seed),
                public_key: &format!("key-{seed}"),
                token_digest: &token,
                intent_digest: "intent",
                registration: &registration,
                access_grant_id: &id("cag", seed),
                now: &at(0).0,
            })
            .unwrap();
        storage.repository_binding_ledger().unwrap();
        storage.connection().unwrap().execute(
            "INSERT INTO repository_bindings(repository_binding_id,client_node_id,display_name,repository_kind,dirty_state,availability,repository_fingerprint,created_at,revision) VALUES(?1,?2,'fixture','git','clean','available',?1,?3,1)",
            params![id("rbd",seed),id("cnd",seed),at(0).0],
        ).unwrap();
    }

    #[test]
    fn bound_challenge_consumption_preserves_capacity_and_reclaims_expired_entries() {
        let directory =
            std::env::temp_dir().join(format!("wwc-bound-challenge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let mut storage = SqliteStorage::open(&directory).unwrap();
        bind_device(&mut storage, 1);
        bind_device(&mut storage, 2);
        let tx = storage.connection_mut().unwrap().transaction().unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO device_auth_challenges VALUES(?1,'key-1','signed',?2)")
                .unwrap();
            for index in 0..4096 {
                insert
                    .execute(params![format!("bound_consumed-{index}"), at(59).0])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        for index in 0..16 {
            let id = format!("bound_next-{index}");
            assert!(
                storage
                    .consume_bound_device_challenge(&id, "key-1", "signed", &at(59).0, &at(58).0)
                    .is_ok()
            );
            assert_eq!(
                storage.consume_bound_device_challenge(
                    &id,
                    "key-1",
                    "signed",
                    &at(59).0,
                    &at(58).0
                ),
                Err(DeviceAuthStoreError::Authentication)
            );
        }
        assert!(
            storage
                .consume_bound_device_challenge(
                    "bound_other",
                    "key-2",
                    "signed",
                    &at(59).0,
                    &at(58).0
                )
                .is_ok()
        );
        assert!(
            storage
                .publish_device_challenge("anonymous", "key-3", "random", &at(59).0, &at(58).0)
                .is_ok()
        );
        assert_eq!(
            storage.device_challenge("bound_next-0", "key-1", &at(58).0),
            Err(DeviceAuthStoreError::Authentication)
        );
        assert_eq!(
            storage.consume_device_challenge("bound_next-0", "key-1", "signed", &at(58).0),
            Err(DeviceAuthStoreError::Authentication)
        );
        assert_eq!(
            storage.consume_bound_device_challenge(
                "bound_expired",
                "key-1",
                "signed",
                &at(59).0,
                &at(59).0
            ),
            Err(DeviceAuthStoreError::Authentication)
        );
        assert!(
            storage
                .consume_bound_device_challenge(
                    "bound_new",
                    "key-1",
                    "signed",
                    "2026-01-01T00:01:00.000Z",
                    &at(59).0
                )
                .is_ok()
        );
        let retained: i64 = storage
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM device_auth_challenges", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, 1);
        drop(storage);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[allow(clippy::too_many_lines)]
    fn prepare_execution(
        storage: &mut SqliteStorage,
        seed: u64,
        node: u64,
        accepted: bool,
        reserve_only: bool,
    ) -> WorkerSlotAuthority {
        let repo = repository();
        let scope = ExecutionQueueScope {
            organization_id: repo.organization_id.clone(),
            workspace_id: repo.workspace_id.clone(),
            project_id: repo.project_id.clone(),
            repository_id: repo.repository_id.clone(),
            product_session_id: ProductSessionId(id("psn", seed)),
            delivery_id: None,
        };
        let worker = WorkerId(id("wrk", seed));
        let instance = WorkerInstanceId(id("wki", seed));
        let digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
        storage
            .execution_registry()
            .unwrap()
            .register_worker(&WorkerRegistrationRequest {
                authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                    control_plane_principal: format!("device:{}", id("cnd", node)),
                },
                protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".into()],
                capability_digest: digest.clone(),
                security_zone: "local".into(),
                max_slots: 4,
                message_id: ExecutionMessageId(id("xmsg", seed * 100)),
                request_id: RequestId(id("req", seed * 100)),
                sent_at: at(1),
                started_at: at(1),
                worker_id: worker.clone(),
                worker_instance_id: instance.clone(),
            })
            .unwrap();
        storage
            .execution_registry()
            .unwrap()
            .record_heartbeat(&WorkerHeartbeatRequest {
                active_leases: vec![],
                available_slots: 4,
                heartbeat_sequence: ExecutionSequence(1),
                max_slots: 4,
                running_slots: 0,
                message_id: ExecutionMessageId(id("xmsg", seed * 100 + 1)),
                observed_at: at(2),
                sent_at: at(2),
                worker_id: worker.clone(),
                worker_instance_id: instance.clone(),
            })
            .unwrap();
        storage
            .execution_queue()
            .unwrap()
            .submit(&ExecutionJobSubmission {
                scope: scope.clone(),
                job_id: ExecutionJobId(id("job", seed)),
                request_id: RequestId(id("req", seed * 100 + 2)),
                payload_digest: digest.clone(),
                dispatch_payload: format!(r#"{{"jobId":"{}"}}"#, id("job", seed)).into_bytes(),
                attempt: if seed == 1 { 2 } else { 1 },
                dependencies: vec![],
                work_run_id: None,
                submitted_at: at(2),
            })
            .unwrap();
        let pool = WorkerPoolId(id("wpl", 1));
        {
            let mut admission = storage.execution_admission().unwrap();
            for boundary in [
                ExecutionAdmissionBoundary::Organization {
                    organization_id: scope.organization_id.clone(),
                },
                ExecutionAdmissionBoundary::Project {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                },
                ExecutionAdmissionBoundary::Repository {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                    repository_id: scope.repository_id.clone(),
                },
                ExecutionAdmissionBoundary::ProductSession {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                    product_session_id: scope.product_session_id.clone(),
                },
                ExecutionAdmissionBoundary::WorkerPool {
                    organization_id: scope.organization_id.clone(),
                    worker_pool_id: pool.clone(),
                },
            ] {
                admission.configure_runtime_policy(boundary, false).unwrap();
            }
            admission
                .reserve(&ExecutionReservationRequest {
                    scope: scope.clone(),
                    user_id: UserId(id("usr", 1)),
                    worker_pool_id: pool.clone(),
                    job_id: ExecutionJobId(id("job", seed)),
                    request_id: RequestId(id("req", seed * 100 + 3)),
                    repository_access: ExecutionRepositoryAccess::ReadOnly,
                    reserved_tokens: None,
                    reserved_cost_microunits: None,
                    runtime_limit_millis: None,
                    submitted_at: at(2),
                })
                .unwrap();
            if !reserve_only {
                admission
                    .start(&ExecutionReservationStart {
                        scope: scope.clone(),
                        worker_pool_id: pool,
                        job_id: ExecutionJobId(id("job", seed)),
                        request_id: RequestId(id("req", seed * 100 + 4)),
                        expected_revision: 1,
                        started_at: at(3),
                    })
                    .unwrap();
            }
        }
        let claim = storage
            .repository_scheduler()
            .unwrap()
            .claim_next(&RepositorySchedulerClaimRequest {
                scope: repo.clone(),
                request_id: RequestId(id("req", seed * 100 + 5)),
                scheduler_generation: "boot-A".into(),
                worker_id: worker.clone(),
                worker_instance_id: instance.clone(),
                issued_at: at(3),
                expires_at: at(50),
            })
            .unwrap()
            .unwrap();
        assert_eq!(claim.job.job_id, ExecutionJobId(id("job", seed)));
        let authority = WorkerSlotAuthority {
            worker_id: worker.clone(),
            worker_instance_id: instance.clone(),
            worker_session_id: WorkerSessionId(id("wsn", seed)),
            codex_thread_id: CodexThreadId(id("cdx", seed)),
            job_id: claim.job.job_id.clone(),
            lease_id: claim.lease.lease_id.clone(),
            attempt: claim.lease.attempt,
            fencing_token: claim.lease.fencing_token.clone(),
        };
        if !reserve_only {
            let mut slots = storage.worker_session_slots().unwrap();
            slots
                .configure_resources(
                    &worker,
                    &instance,
                    WorkerSlotResourceLimits {
                        max_memory_bytes: 4,
                        max_disk_bytes: 4,
                        max_processes: 4,
                    },
                )
                .unwrap();
            slots
                .open(&WorkerSlotOpenRequest {
                    authority: authority.clone(),
                    resources: WorkerSlotResources {
                        memory_bytes: 1,
                        disk_bytes: 1,
                        process_slots: 1,
                    },
                    request_id: RequestId(id("req", seed * 100 + 6)),
                    opened_at: at(4),
                })
                .unwrap();
        }
        if accepted {
            storage
                .repository_scheduler()
                .unwrap()
                .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
                    scope: repo,
                    dispatch: DispatchResultRequest {
                        checked_at: at(5),
                        expires_at: claim.lease.expires_at,
                        issued_at: claim.lease.issued_at,
                        fencing_token: claim.lease.fencing_token,
                        job_id: claim.job.job_id,
                        lease_id: claim.lease.lease_id,
                        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 7)),
                        payload_digest: digest,
                        request_id: RequestId(id("req", seed * 100 + 7)),
                        sent_at: at(5),
                        status: DispatchResultStatus::Accepted,
                        attempt: claim.lease.attempt,
                        error: None,
                        worker_id: worker.clone(),
                        worker_instance_id: instance.clone(),
                        worker_session_id: Some(authority.worker_session_id.clone()),
                    },
                })
                .unwrap();
        }
        storage.worker_launch_grant_ledger().unwrap();
        storage.connection().unwrap().execute(
            "INSERT INTO worker_launch_grants(worker_launch_grant_id,client_node_id,client_instance_id,holder_user_id,occupancy_lease_id,occupancy_fencing_token,repository_binding_id,worker_session_id,worker_id,worker_instance_id,expires_at,state,consumed_at,created_at,revision) VALUES(?1,?2,?3,?4,?5,1,?6,?7,?8,?9,?10,'consumed',?11,?11,1)",
            params![id("wlg",seed),id("cnd",node),id("cix",node),id("usr",1),id("ocl",node),id("rbd",node),id("wsn",seed),worker.0,instance.0,at(50).0,at(1).0],
        ).unwrap();
        authority
    }

    fn snapshot(storage: &SqliteStorage, job: &ExecutionJobId) -> Vec<String> {
        let connection = storage.connection().unwrap();
        [
            "SELECT json_array(state,attempt,revision,updated_at) FROM scheduler_execution_jobs WHERE job_id=?1",
            "SELECT json_array(lease_id,worker_id,worker_instance_id,attempt,fencing_token,issued_at,expires_at) FROM execution_leases WHERE job_id=?1",
            "SELECT json_array(state,revision,updated_at,terminal_at) FROM worker_session_slots WHERE job_id=?1",
            "SELECT json_array(state,revision,updated_at,terminal_at,actual_tokens,actual_cost_microunits,actual_runtime_millis,release_reason) FROM execution_admission_reservations WHERE job_id=?1",
        ].into_iter().map(|sql|connection.query_row(sql,[&job.0],|row|row.get(0)).unwrap()).collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn revoke_closes_running_and_unaccepted_leases_without_changing_success_or_other_devices() {
        let directory =
            std::env::temp_dir().join(format!("wwc-revoke-resources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let mut storage = SqliteStorage::open(&directory).unwrap();
        bind_device(&mut storage, 1);
        bind_device(&mut storage, 2);
        let running = prepare_execution(&mut storage, 1, 1, true, false);
        let leased = prepare_execution(&mut storage, 2, 1, false, false);
        let unstarted = prepare_execution(&mut storage, 7, 1, false, true);
        assert!(
            storage
                .worker_session_slots()
                .unwrap()
                .load(&unstarted.worker_session_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            storage
                .execution_admission()
                .unwrap()
                .load_reservation_by_job(&unstarted.job_id)
                .unwrap()
                .unwrap()
                .state,
            ExecutionReservationState::Queued
        );
        let completed = prepare_execution(&mut storage, 3, 1, true, false);
        let other = prepare_execution(&mut storage, 4, 2, true, false);
        let successor = prepare_execution(&mut storage, 6, 2, false, false);
        storage
            .connection()
            .unwrap()
            .execute(
                "DELETE FROM worker_session_slots WHERE worker_session_id=?1",
                [&successor.worker_session_id.0],
            )
            .unwrap();
        storage.connection().unwrap().execute(
            "INSERT INTO worker_session_slots SELECT ?1,?2,?3,worker_id,worker_instance_id,?4,1,'1',state,event_cursor,memory_bytes,disk_bytes,process_slots,revision,opened_at,updated_at,cancellation_requested_at,terminal_at FROM worker_session_slots WHERE worker_session_id=?5",
            params![id("wsn",996),id("cdx",996),successor.job_id.0,id("lse",996),running.worker_session_id.0],
        ).unwrap();
        let successor_lease = storage
            .execution_registry()
            .unwrap()
            .load_lease(&successor.job_id)
            .unwrap()
            .unwrap();
        let successor_admission = storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&successor.job_id)
            .unwrap()
            .unwrap();
        storage
            .worker_session_slots()
            .unwrap()
            .close(&WorkerSlotCloseRequest {
                authority: completed.clone(),
                request_id: RequestId(id("req", 900)),
                expected_revision: 1,
                outcome: WorkerSlotState::Completed,
                closed_at: at(6),
            })
            .unwrap();
        let completed_scope = execution_admission::load_reservation_by_job_in_transaction(
            storage.connection().unwrap(),
            &completed.job_id,
        )
        .unwrap()
        .unwrap()
        .scope;
        storage
            .execution_admission()
            .unwrap()
            .settle(&ExecutionReservationSettlement {
                scope: completed_scope,
                worker_pool_id: WorkerPoolId(id("wpl", 1)),
                job_id: completed.job_id.clone(),
                request_id: RequestId(id("req", 901)),
                expected_revision: 2,
                actual_tokens: 7,
                actual_cost_microunits: Some(11),
                actual_runtime_millis: 3000,
                completed_at: at(6),
            })
            .unwrap();
        storage
            .repository_scheduler()
            .unwrap()
            .settle_terminal(&RepositorySchedulerTerminalRequest {
                scope: repository(),
                terminal: ExecutionLeaseTerminalRequest {
                    job_id: completed.job_id.clone(),
                    lease_id: completed.lease_id.clone(),
                    worker_id: completed.worker_id.clone(),
                    worker_instance_id: completed.worker_instance_id.clone(),
                    attempt: completed.attempt,
                    fencing_token: completed.fencing_token.clone(),
                    outcome: ExecutionLeaseTerminalOutcome::Completed,
                    terminal_at: at(6),
                    request_id: RequestId(id("req", 902)),
                },
            })
            .unwrap();
        storage.connection().unwrap().execute(
            "INSERT INTO worker_launch_grants SELECT ?1,client_node_id,client_instance_id,holder_user_id,occupancy_lease_id,occupancy_fencing_token,repository_binding_id,?2,?3,?4,product_session_id,work_run_id,expires_at,'expired',consumed_at,ended_at,created_at,revision FROM worker_launch_grants WHERE worker_launch_grant_id=?5",
            params![id("wlg",999),id("wsn",999),other.worker_id.0,id("wki",999),id("wlg",1)],
        ).unwrap();
        let success_before = snapshot(&storage, &completed.job_id);
        let other_before = snapshot(&storage, &other.job_id);
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO execution_attempt_accounting_totals VALUES(?1,1,9,15,9)",
                [&running.job_id.0],
            )
            .unwrap();
        storage.connection().unwrap().execute_batch("CREATE TRIGGER revoke_fault BEFORE INSERT ON execution_admission_reconciliation BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert_eq!(
            storage.revoke_device_identity(&id("cnd", 1), &id("usr", 1), &at(10).0),
            Err(DeviceAuthStoreError::Unavailable)
        );
        assert!(
            storage
                .device_identity_owned_by(&id("cnd", 1), &id("usr", 1))
                .unwrap()
        );
        assert_eq!(
            storage
                .execution_registry()
                .unwrap()
                .load_worker(&running.worker_id)
                .unwrap()
                .unwrap()
                .health,
            crate::WorkerHealth::Healthy
        );
        assert_eq!(
            storage
                .worker_session_slots()
                .unwrap()
                .load(&running.worker_session_id)
                .unwrap()
                .unwrap()
                .state,
            WorkerSlotState::Running
        );
        storage
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER revoke_fault")
            .unwrap();
        storage
            .revoke_device_identity(&id("cnd", 1), &id("usr", 1), &at(10).0)
            .unwrap();
        for authority in [&running, &leased, &unstarted] {
            assert_eq!(
                repository_scheduler::load_execution_job_by_id(
                    storage.connection().unwrap(),
                    &authority.job_id
                )
                .unwrap()
                .unwrap()
                .state,
                ExecutionJobState::Failed
            );
            let slot = storage
                .worker_session_slots()
                .unwrap()
                .load(&authority.worker_session_id)
                .unwrap();
            assert_eq!(
                slot.map(|slot| slot.state),
                if authority == &unstarted {
                    None
                } else {
                    Some(WorkerSlotState::RecoveryFailed)
                }
            );
            let reservation = storage
                .execution_admission()
                .unwrap()
                .load_reservation_by_job(&authority.job_id)
                .unwrap()
                .unwrap();
            assert_eq!(reservation.state, ExecutionReservationState::Released);
            assert_eq!(
                reservation.release_reason,
                Some(ExecutionReservationReleaseReason::Cancelled)
            );
            assert_eq!(reservation.actual_tokens, None);
            assert_eq!(reservation.actual_cost_microunits, None);
            assert_eq!(reservation.actual_runtime_millis, None);
            assert_eq!(
                storage
                    .execution_registry()
                    .unwrap()
                    .load_worker(&authority.worker_id)
                    .unwrap()
                    .unwrap()
                    .health,
                crate::WorkerHealth::TimedOut
            );
            let (outcome,usage,status):(String,String,String)=storage.connection().unwrap().query_row(
                "SELECT t.outcome,a.terminal_usage_json,a.accounting_status FROM execution_lease_terminals t JOIN execution_admission_reconciliation a USING(job_id) WHERE t.lease_id=?1",[&authority.lease_id.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            assert_eq!(outcome, "cancelled");
            assert_eq!(status, "pending");
            let usage: ExecutionOutcomeUsage = serde_json::from_str(&usage).unwrap();
            assert_eq!(usage.tokens, None);
            assert_eq!(usage.cost_microunits, None);
            assert_eq!(usage.runtime_millis, 7000);
            assert_eq!(
                usage.known_tokens,
                if authority == &running { 9 } else { 0 }
            );
            assert_eq!(usage.accounting_status,winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Unknown);
        }
        assert_eq!(
            storage
                .worker_session_slots()
                .unwrap()
                .load(&WorkerSessionId(id("wsn", 996)))
                .unwrap()
                .unwrap()
                .state,
            WorkerSlotState::RecoveryFailed
        );
        assert_eq!(
            storage
                .execution_registry()
                .unwrap()
                .load_lease(&successor.job_id)
                .unwrap()
                .unwrap(),
            successor_lease
        );
        assert_eq!(
            storage
                .execution_admission()
                .unwrap()
                .load_reservation_by_job(&successor.job_id)
                .unwrap()
                .unwrap(),
            successor_admission
        );
        assert_eq!(
            repository_scheduler::load_execution_job_by_id(
                storage.connection().unwrap(),
                &successor.job_id
            )
            .unwrap()
            .unwrap()
            .state,
            ExecutionJobState::Leased
        );
        let after = snapshot(&storage, &running.job_id);
        storage
            .revoke_device_identity(&id("cnd", 1), &id("usr", 1), &at(11).0)
            .unwrap();
        assert_eq!(snapshot(&storage, &running.job_id), after);
        assert_eq!(snapshot(&storage, &completed.job_id), success_before);
        assert_eq!(snapshot(&storage, &other.job_id), other_before);
        assert_eq!(
            storage
                .execution_registry()
                .unwrap()
                .load_worker(&other.worker_id)
                .unwrap()
                .unwrap()
                .health,
            crate::WorkerHealth::Healthy
        );
        let next = prepare_execution(&mut storage, 5, 2, true, false);
        assert_eq!(
            repository_scheduler::load_execution_job_by_id(
                storage.connection().unwrap(),
                &next.job_id
            )
            .unwrap()
            .unwrap()
            .state,
            ExecutionJobState::Running
        );
        let outcomes: i64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM product_state WHERE stream_id LIKE 'execution-outcome:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcomes, 0);
        drop(storage);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn token_binding_access_and_receipt_roll_back_together() {
        let directory =
            std::env::temp_dir().join(format!("wwc-binding-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let mut storage = SqliteStorage::open(&directory).unwrap();
        storage
            .publish_device_pairing(
                "token-digest",
                "usr_00000000000000000000000001",
                "2027-01-01T00:00:00.000Z",
            )
            .unwrap();
        storage.connection().unwrap().execute_batch("CREATE TRIGGER binding_fault BEFORE INSERT ON device_auth_events BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        let registration = ClientNodeRegistration::try_new(
            "cnd_00000000000000000000000001",
            "123456789012",
            "Device",
            "aarch64-apple-darwin",
            "aarch64",
            "0.2.0",
            None,
            0,
        )
        .unwrap();
        let mut write = DeviceBindingWrite {
            request_id: "req_00000000000000000000000001",
            public_key: "key",
            token_digest: "token-digest",
            intent_digest: "intent",
            registration: &registration,
            access_grant_id: "cag_00000000000000000000000001",
            now: "2026-01-01T00:00:00.000Z",
        };
        assert_eq!(
            storage.bind_device_identity(&write),
            Err(DeviceAuthStoreError::Unavailable)
        );
        let count: i64 = storage
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM client_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        for table in [
            "device_identity_bindings",
            "client_access_grants",
            "device_auth_events",
        ] {
            let count: i64 = storage
                .connection()
                .unwrap()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
        let consumed: Option<String> = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT consumed_request_id FROM device_pairing_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(consumed, None);
        storage
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER binding_fault")
            .unwrap();
        let receipt = storage.bind_device_identity(&write).unwrap();
        write.now = "2028-01-01T00:00:00.000Z";
        assert_eq!(storage.bind_device_identity(&write).unwrap(), receipt);
        let permissions: String = storage
            .connection()
            .unwrap()
            .query_row("SELECT permissions FROM client_access_grants", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(permissions, "use+manage+share");
        storage
            .revoke_device_identity(
                registration.client_node_id(),
                "usr_00000000000000000000000001",
                write.now,
            )
            .unwrap();
        assert_eq!(
            storage.bind_device_identity(&write),
            Err(DeviceAuthStoreError::Revoked)
        );
        drop(storage);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
