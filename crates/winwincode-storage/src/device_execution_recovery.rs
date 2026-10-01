// SPDX-License-Identifier: Apache-2.0

//! Trusted Device exit and launch lineage authorize replacement of an exact
//! execution attempt. Original launch and reservation facts remain immutable.
use crate::{DeviceExecutionReservationFacts, ExecutionLeaseRecord, SqliteStorage, StorageError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, WorkRunId};

fn invalid() -> StorageError {
    StorageError::invalid_input("Device execution recovery authority differs")
}
fn sql(error: impl std::fmt::Display) -> StorageError {
    StorageError::adapter(error.to_string())
}
fn digest(facts: &DeviceExecutionReservationFacts) -> Result<String, StorageError> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(facts).map_err(sql)?)
    ))
}

fn successor(
    connection: &Connection,
    facts: &DeviceExecutionReservationFacts,
    grant_id: &str,
) -> Result<crate::WorkerLaunchGrantRecord, StorageError> {
    let grant = crate::client_launch_grant::load_launch_grant(connection, grant_id)
        .map_err(sql)?
        .ok_or_else(invalid)?;
    let live:bool=connection.query_row(
        "WITH RECURSIVE lineage(predecessor_grant_id,successor_grant_id,depth) AS (
          SELECT predecessor_grant_id,successor_grant_id,1 FROM worker_replacement_launches WHERE predecessor_grant_id=?1
          UNION ALL SELECT r.predecessor_grant_id,r.successor_grant_id,l.depth+1 FROM worker_replacement_launches r JOIN lineage l ON r.predecessor_grant_id=l.successor_grant_id JOIN worker_launch_grant_exits x ON x.worker_launch_grant_id=r.predecessor_grant_id WHERE l.depth<100)
         SELECT EXISTS(SELECT 1 FROM lineage r
         JOIN worker_launch_grant_exits e ON e.worker_launch_grant_id=?1
         JOIN client_nodes n ON n.client_node_id=e.client_node_id
         LEFT JOIN worker_launch_process_closures pc ON pc.grant_id=e.worker_launch_grant_id
         JOIN client_occupancy_leases o ON o.occupancy_lease_id=e.occupancy_lease_id
         JOIN device_execution_bindings b ON b.worker_launch_grant_id=r.successor_grant_id
         WHERE r.successor_grant_id=?2
          AND n.current_instance_id=COALESCE(pc.reporting_client_instance_id,e.client_instance_id)
          AND b.client_instance_id=n.current_instance_id AND b.state='bound'
          AND o.client_node_id=e.client_node_id AND o.holder_user_id=?3
          AND o.fencing_token=e.occupancy_fencing_token AND o.state IN ('occupied','draining')
          AND NOT EXISTS(SELECT 1 FROM worker_launch_grant_exits x WHERE x.worker_launch_grant_id=r.successor_grant_id))",
        params![facts.worker_launch_grant_id,grant_id,facts.holder_user_id],|row|row.get(0)).map_err(sql)?;
    if !live
        || grant.state != crate::WorkerLaunchGrantState::Consumed
        || grant.client_node_id != facts.client_node_id
        || grant.holder_user_id != facts.holder_user_id
        || grant.repository_binding_id != facts.repository_binding_id
        || grant.occupancy_lease_id != facts.occupancy_lease_id
        || grant.occupancy_fencing_token != facts.occupancy_fencing_token
        || grant.worker_id != facts.worker_id
        || grant.worker_instance_id == facts.worker_instance_id
        || grant.product_session_id != facts.product_session_id
        || grant.work_run_id.as_ref().map(|id| &id.0) != facts.work_run_id.as_ref()
    {
        return Err(invalid());
    }
    Ok(grant)
}

fn new_facts(
    previous: &DeviceExecutionReservationFacts,
    grant: &crate::WorkerLaunchGrantRecord,
    run: Option<&WorkRunId>,
    now: &Instant,
) -> DeviceExecutionReservationFacts {
    let mut facts = previous.clone();
    facts
        .client_instance_id
        .clone_from(&grant.client_instance_id);
    facts
        .worker_launch_grant_id
        .clone_from(&grant.worker_launch_grant_id);
    facts.worker_session_id.clone_from(&grant.worker_session_id);
    facts.worker_id.clone_from(&grant.worker_id);
    facts
        .worker_instance_id
        .clone_from(&grant.worker_instance_id);
    facts.work_run_id = run.map(|id| id.0.clone());
    facts.attached_at = now.clone();
    facts
}
fn append(
    connection: &Connection,
    attempt: u64,
    facts: &DeviceExecutionReservationFacts,
    receipt: Option<&str>,
    now: &Instant,
) -> Result<(), StorageError> {
    connection.execute("INSERT INTO device_execution_fact_replacements(job_id,attempt,facts_json,replacement_receipt_id,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![facts.job_id,i64::try_from(attempt).map_err(|_|invalid())?,serde_json::to_string(facts).map_err(sql)?,receipt,now.0]).map_err(sql)?;
    Ok(())
}

/// Stages a consumed launch successor. A never-claimed queued route is replaced
/// atomically without manufacturing a lease; active work waits for lease expiry.
/// # Errors
/// Rejects changed launch, Device, occupancy, role, attempt or cancellation facts.
#[allow(clippy::too_many_lines)]
pub fn stage_device_execution_recovery(
    storage: &mut SqliteStorage,
    job_id: &ExecutionJobId,
    grant_id: &str,
    role: Option<&str>,
    now: &Instant,
) -> Result<DeviceExecutionReservationFacts, StorageError> {
    storage.device_execution_binding_ledger().map_err(sql)?;
    storage.repository_scheduler()?;
    let transaction = storage
        .connection_mut()?
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let job = crate::repository_scheduler::load_execution_job_by_id(&transaction, job_id)?
        .ok_or_else(invalid)?;
    if job.cancellation.is_some()
        || !matches!(
            job.state,
            crate::ExecutionJobState::Queued
                | crate::ExecutionJobState::Leased
                | crate::ExecutionJobState::Running
        )
    {
        return Err(invalid());
    }
    let payload: winwincode_execution_port::generated::ExecutionJob =
        serde_json::from_slice(&job.dispatch_payload).map_err(|_| invalid())?;
    if payload.job_id != job.job_id
        || payload.payload_digest != job.payload_digest
        || u64::try_from(payload.attempt).ok() != Some(job.attempt)
        || payload.workspace.repository_id != job.scope.repository_id
    {
        return Err(invalid());
    }
    match &payload.scope {
        winwincode_execution_port::generated::ExecutionScope::ProductSessionExecutionScope(
            scope,
        ) => {
            if role.is_some()
                || job.work_run_id.is_some()
                || job.scope.delivery_id.is_some()
                || payload.work_input.is_some()
                || scope.product_session_id != job.scope.product_session_id
            {
                return Err(invalid());
            }
        }
        winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) => {
            if role != Some(payload.execution_profile.as_str())
                || !matches!(
                    payload.execution_profile.as_str(),
                    "executor"
                        | "reviewer"
                        | "verifier"
                        | "adversarial-verifier"
                        | "remediator"
                        | "planner"
                )
                || scope.attempt != payload.attempt
                || job.work_run_id.as_ref() != Some(&scope.work_run_id)
                || scope.product_session_id != job.scope.product_session_id
            {
                return Err(invalid());
            }
        }
    }
    let facts = crate::device_execution_binding::load_facts(&transaction, &job_id.0)
        .map_err(sql)?
        .ok_or_else(invalid)?;
    if job.work_run_id.is_some() != role.is_some()
        || job.scope.product_session_id.0 != facts.product_session_id.as_deref().unwrap_or("")
        || job.work_run_id.as_ref().map(|id| &id.0) != facts.work_run_id.as_ref()
    {
        return Err(invalid());
    }
    if facts.worker_launch_grant_id == grant_id && facts.role.as_deref() == role {
        transaction.commit().map_err(sql)?;
        return Ok(facts);
    }
    if facts.role.as_deref() != role
        || job.work_run_id.as_ref().map(|id| &id.0) != facts.work_run_id.as_ref()
    {
        return Err(invalid());
    }
    let grant = successor(&transaction, &facts, grant_id)?;
    if job.state == crate::ExecutionJobState::Queued {
        if crate::execution_registry::load_lease_in_transaction(&transaction, job_id)?.is_some() {
            return Err(invalid());
        }
        let replacement = new_facts(&facts, &grant, job.work_run_id.as_ref(), now);
        append(&transaction, job.attempt, &replacement, None, now)?;
        transaction.commit().map_err(sql)?;
        return Ok(replacement);
    }
    let lease = crate::execution_registry::load_lease_in_transaction(&transaction, job_id)?
        .ok_or_else(invalid)?;
    if lease.attempt != job.attempt
        || lease.worker_id.0 != facts.worker_id
        || lease.worker_instance_id.0 != facts.worker_instance_id
    {
        return Err(invalid());
    }
    let previous:Option<(String,String)>=transaction.query_row("SELECT predecessor_facts_digest,successor_grant_id FROM device_execution_recovery_intents WHERE job_id=?1 AND predecessor_attempt=?2",params![job_id.0,i64::try_from(job.attempt).map_err(|_|invalid())?],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql)?;
    let fact_digest = digest(&facts)?;
    if let Some((original, successor)) = previous {
        if original != fact_digest {
            return Err(invalid());
        }
        if successor != grant_id {
            let exited:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?1)",[&successor],|r|r.get(0)).map_err(sql)?;
            if !exited {
                return Err(invalid());
            }
            transaction.execute("UPDATE device_execution_recovery_intents SET successor_grant_id=?3,created_at=?4 WHERE job_id=?1 AND predecessor_attempt=?2 AND applied_at IS NULL",params![job_id.0,i64::try_from(job.attempt).map_err(|_|invalid())?,grant_id,now.0]).map_err(sql)?;
        }
    } else {
        transaction
            .execute(
                "INSERT INTO device_execution_recovery_intents VALUES(?1,?2,?3,?4,?5,NULL)",
                params![
                    job_id.0,
                    i64::try_from(job.attempt).map_err(|_| invalid())?,
                    fact_digest,
                    grant_id,
                    now.0
                ],
            )
            .map_err(sql)?;
    }
    transaction.commit().map_err(sql)?;
    Ok(facts)
}

pub(crate) fn recovery_ready(
    connection: &Connection,
    lease: &ExecutionLeaseRecord,
    worker: &winwincode_domain::WorkerId,
    instance: &winwincode_domain::WorkerInstanceId,
) -> Result<bool, StorageError> {
    let facts =
        crate::device_execution_binding::load_facts(connection, &lease.job_id.0).map_err(sql)?;
    let Some(facts) = facts else { return Ok(true) }; // Existing non-Device replacement rules.
    let intent:Option<(String,String)>=connection.query_row("SELECT predecessor_facts_digest,successor_grant_id FROM device_execution_recovery_intents WHERE job_id=?1 AND predecessor_attempt=?2 AND applied_at IS NULL",params![lease.job_id.0,i64::try_from(lease.attempt).map_err(|_|invalid())?],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql)?;
    let Some((original, grant_id)) = intent else {
        return Ok(false);
    };
    if original != digest(&facts)? {
        return Err(invalid());
    }
    let grant = successor(connection, &facts, &grant_id)?;
    Ok(grant.worker_id == worker.0
        && grant.worker_instance_id == instance.0
        && lease.worker_id.0 == facts.worker_id
        && lease.worker_instance_id.0 == facts.worker_instance_id)
}

pub(crate) fn commit_recovery(
    connection: &Connection,
    previous: &ExecutionLeaseRecord,
    next: &ExecutionLeaseRecord,
    run: Option<&WorkRunId>,
    now: &Instant,
) -> Result<(), StorageError> {
    let Some(facts) =
        crate::device_execution_binding::load_facts(connection, &previous.job_id.0).map_err(sql)?
    else {
        return Ok(());
    };
    if !recovery_ready(
        connection,
        previous,
        &next.worker_id,
        &next.worker_instance_id,
    )? {
        return Err(invalid());
    }
    let grant_id:String=connection.query_row("SELECT successor_grant_id FROM device_execution_recovery_intents WHERE job_id=?1 AND predecessor_attempt=?2",params![previous.job_id.0,i64::try_from(previous.attempt).map_err(|_|invalid())?],|r|r.get(0)).map_err(sql)?;
    let grant = successor(connection, &facts, &grant_id)?;
    let authority = crate::execution_scope_replacement::load_execution_scope_replacement(
        connection,
        &previous.job_id,
    )?
    .ok_or_else(invalid)?;
    if authority.predecessor_lease() != previous
        || authority.replacement_lease() != next
        || authority.work_run_id() != run
    {
        return Err(invalid());
    }
    append(
        connection,
        next.attempt,
        &new_facts(&facts, &grant, run, now),
        Some(&authority.receipt_id().0),
        now,
    )?;
    connection.execute("UPDATE device_execution_recovery_intents SET applied_at=?3 WHERE job_id=?1 AND predecessor_attempt=?2 AND applied_at IS NULL",params![previous.job_id.0,i64::try_from(previous.attempt).map_err(|_|invalid())?,now.0]).map_err(sql)?;
    connection.execute("INSERT INTO execution_attempt_accounting_pending(job_id,attempt,lease_json,observed_at) VALUES(?1,?2,?3,?4)",params![previous.job_id.0,i64::try_from(previous.attempt).map_err(|_|invalid())?,serde_json::to_string(previous).map_err(sql)?,now.0]).map_err(sql)?;
    Ok(())
}
