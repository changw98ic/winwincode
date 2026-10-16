// SPDX-License-Identifier: Apache-2.0

use rusqlite::{OptionalExtension, params};
use winwincode_client_port::domain::LocalApplyReceipt;

use crate::apply_engine::{CandidateApplyError, CandidateApplyRequest};
use crate::store::DeviceStore;

#[derive(Clone, Debug)]
pub(crate) struct ApplicationRecord {
    pub request: CandidateApplyRequest,
    pub started_at: String,
    pub finished_receipt: Option<LocalApplyReceipt>,
    pub recovery_outcome: Option<String>,
    pub reported_epoch: Option<u64>,
}

pub(crate) fn application(
    store: &DeviceStore,
    application_id: &str,
) -> Result<Option<ApplicationRecord>, CandidateApplyError> {
    let raw = store
        .connection()?
        .query_row(
            "SELECT approval_id, repository_binding_id, candidate_ref, candidate_digest, \
         target_branch, base_commit, strategy, original_lease_id, original_epoch, \
         started_at, finished_receipt_json, recovery_outcome, reported_epoch FROM candidate_applications WHERE application_id = ?1",
            [application_id],
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
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                ))
            },
        )
        .optional()
        .map_err(crate::store::sql_error)?;
    let Some((
        approval_id,
        repository_binding_id,
        candidate_ref,
        candidate_digest,
        target_branch,
        expected_head,
        strategy,
        occupancy_lease_id,
        occupancy_fencing_token,
        started_at,
        receipt,
        recovery_outcome,
        reported_epoch,
    )) = raw
    else {
        return Ok(None);
    };
    Ok(Some(ApplicationRecord {
        request: CandidateApplyRequest {
            application_id: application_id.into(),
            approval_id,
            repository_binding_id,
            candidate_ref,
            candidate_digest,
            target_branch,
            expected_head,
            strategy: serde_json::from_str(&strategy)
                .map_err(|error| CandidateApplyError::store(error.to_string()))?,
            occupancy_lease_id,
            occupancy_fencing_token: u64::try_from(occupancy_fencing_token)
                .map_err(|error| CandidateApplyError::store(error.to_string()))?,
            reconciled_retry: false,
        },
        started_at,
        recovery_outcome,
        reported_epoch: reported_epoch
            .map(u64::try_from)
            .transpose()
            .map_err(|error| CandidateApplyError::store(error.to_string()))?,
        finished_receipt: receipt
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|error| CandidateApplyError::store(error.to_string()))?,
    }))
}

pub(crate) fn blocked_application(
    store: &DeviceStore,
    binding: &str,
    candidate_ref: &str,
) -> Result<Option<ApplicationRecord>, CandidateApplyError> {
    let id = store.connection()?.query_row("SELECT application_id FROM candidate_applications WHERE repository_binding_id = ?1 AND candidate_ref = ?2 AND recovery_outcome = 'outcome_unknown' AND finished_receipt_json IS NULL LIMIT 1",
        params![binding, candidate_ref], |row| row.get::<_, String>(0)).optional().map_err(crate::store::sql_error)?;
    match id {
        Some(id) => application(store, &id),
        None => Ok(None),
    }
}

pub(crate) fn start_application(
    store: &mut DeviceStore,
    request: &CandidateApplyRequest,
    started_at: &str,
) -> Result<(), CandidateApplyError> {
    let strategy = serde_json::to_string(&request.strategy)
        .map_err(|error| CandidateApplyError::store(error.to_string()))?;
    store
        .connection_mut()?
        .execute(
            "INSERT INTO candidate_applications \
         (application_id, approval_id, repository_binding_id, candidate_ref, candidate_digest, \
          target_branch, base_commit, strategy, original_lease_id, original_epoch, started_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                request.application_id,
                request.approval_id,
                request.repository_binding_id,
                request.candidate_ref,
                request.candidate_digest,
                request.target_branch,
                request.expected_head,
                strategy,
                request.occupancy_lease_id,
                i64::try_from(request.occupancy_fencing_token)
                    .map_err(|error| CandidateApplyError::store(error.to_string()))?,
                started_at
            ],
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

pub(crate) fn finish_application(
    store: &mut DeviceStore,
    receipt: &LocalApplyReceipt,
) -> Result<(), CandidateApplyError> {
    let json = serde_json::to_string(receipt)
        .map_err(|error| CandidateApplyError::store(error.to_string()))?;
    let updated = store.connection_mut()?.execute(
        "UPDATE candidate_applications SET \
         reported_epoch = CASE WHEN finished_receipt_json IS NULL THEN NULL ELSE reported_epoch END, \
         finished_receipt_json = ?2 \
         WHERE application_id = ?1 AND (finished_receipt_json IS NULL OR finished_receipt_json = ?2)",
        params![receipt.local_apply_receipt_id, json],
    ).map_err(crate::store::sql_error)?;
    if updated != 1 {
        return Err(CandidateApplyError::store(
            "application result conflicts with its durable journal",
        ));
    }
    Ok(())
}

pub(crate) fn begin_application_attempt(
    store: &mut DeviceStore,
    application_id: &str,
) -> Result<(), CandidateApplyError> {
    store
        .connection_mut()?
        .execute(
            "UPDATE candidate_applications SET recovery_outcome = NULL \
         WHERE application_id = ?1 AND finished_receipt_json IS NULL",
            [application_id],
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

pub(crate) fn unfinished_applications(
    store: &DeviceStore,
) -> Result<Vec<ApplicationRecord>, CandidateApplyError> {
    Ok(all_applications(store)?
        .into_iter()
        .filter(|record| record.finished_receipt.is_none())
        .collect())
}

pub(crate) fn all_applications(
    store: &DeviceStore,
) -> Result<Vec<ApplicationRecord>, CandidateApplyError> {
    let ids = {
        let mut statement = store
            .connection()?
            .prepare(
                "SELECT application_id FROM candidate_applications \
             ORDER BY started_at, application_id",
            )
            .map_err(crate::store::sql_error)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(crate::store::sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(crate::store::sql_error)?
    };
    ids.iter()
        .map(|id| {
            application(store, id)?
                .ok_or_else(|| CandidateApplyError::store("application journal entry disappeared"))
        })
        .collect()
}

pub(crate) fn mark_reported(
    store: &mut DeviceStore,
    application_id: &str,
    epoch: u64,
) -> Result<(), CandidateApplyError> {
    store
        .connection_mut()?
        .execute(
            "UPDATE candidate_applications SET reported_epoch = ?2 WHERE application_id = ?1",
            params![
                application_id,
                i64::try_from(epoch)
                    .map_err(|error| CandidateApplyError::store(error.to_string()))?
            ],
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

pub(crate) fn note_recovery_outcome(
    store: &mut DeviceStore,
    application_id: &str,
    outcome: &str,
) -> Result<(), CandidateApplyError> {
    store
        .connection_mut()?
        .execute(
            "UPDATE candidate_applications SET \
         reported_epoch = CASE WHEN recovery_outcome = ?2 THEN reported_epoch ELSE NULL END, \
         recovery_outcome = ?2 WHERE application_id = ?1 AND finished_receipt_json IS NULL",
            params![application_id, outcome],
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}
