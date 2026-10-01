// SPDX-License-Identifier: Apache-2.0

//! An unchanged Remediator authorizes another independent review of the same candidate.
use super::{
    Delivery, DeliverySnapshot, DeliveryStatus, DeliveryValidationError,
    DeliveryValidationErrorCode, SessionBinding, validation_error,
};
use crate::application::workrun_execution::{DeliveryTerminalOutcomeFacts, TerminalOutcomeStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Sha256Digest, WorkItemState, WorkRunId, WorkRunState};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SameCandidateReverificationFact {
    pub producer_work_run_id: WorkRunId,
    pub remediator_work_run_id: WorkRunId,
    pub remediator_job_id: ExecutionJobId,
    pub candidate_ref: String,
    pub candidate_digest: Sha256Digest,
    pub authorization_digest: Sha256Digest,
    pub source_delivery_revision: u64,
    pub binding_floor: usize,
    pub binding_prefix_digest: Sha256Digest,
}

/// Private, non-deserializable authority produced after exact terminal and rework joins.
#[derive(Clone, Debug)]
pub struct VerifiedUnchangedReworkRecovery {
    fact: SameCandidateReverificationFact,
    source: Delivery,
}
fn invalid() -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::RelationshipMismatch,
        "delivery.sameCandidateReverification",
        "unchanged remediation authority is inconsistent",
    )
}
fn prefix(
    snapshot: &DeliverySnapshot,
    floor: usize,
) -> Result<Sha256Digest, DeliveryValidationError> {
    let bindings = snapshot.session_bindings.get(..floor).ok_or_else(invalid)?;
    // Runtime attachment may legitimately follow a lease. Seal immutable
    // execution identities and business scope instead of those attachments.
    let identities: Vec<_> = bindings
        .iter()
        .map(|b| {
            (
                &b.id,
                &b.work_run_id,
                &b.execution_job_id,
                b.attempt,
                &b.work_contract_id,
                &b.work_contract_revision,
                &b.work_item_id,
                &b.work_item_revision,
                &b.execution_profile,
                b.bound_at_millis,
            )
        })
        .collect();
    let bytes = serde_json::to_vec(&identities).map_err(|_| invalid())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

/// Seals an exact accepted rework source and a failed Remediator terminal fact.
/// # Errors
/// Rejects altered source, active competitors, foreign jobs, or incomplete authorization.
pub fn verify_unchanged_rework_recovery(
    current: &Delivery,
    source: &Delivery,
    terminal: &DeliveryTerminalOutcomeFacts,
    authorization_digest: &Sha256Digest,
    candidate_ref: &str,
) -> Result<VerifiedUnchangedReworkRecovery, DeliveryValidationError> {
    let verified = terminal.verify(current).map_err(|_| invalid())?;
    let verdict = source.snapshot().verdict.as_ref().ok_or_else(invalid)?;
    let producer = selected_writer(source.snapshot()).ok_or_else(invalid)?;
    let remediator = current
        .snapshot()
        .session_bindings
        .iter()
        .find(|b| {
            b.execution_job_id == *verified.execution_job_id()
                && b.work_run_id == *verified.work_run_id()
                && b.attempt == verified.attempt()
        })
        .ok_or_else(invalid)?;
    let old_run = source
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|r| r.id == producer.work_run_id)
        .ok_or_else(invalid)?;
    let retired = current
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|r| r.id == producer.work_run_id)
        .ok_or_else(invalid)?;
    let new_run = current
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|r| r.id == remediator.work_run_id)
        .ok_or_else(invalid)?;
    if verified.status() != TerminalOutcomeStatus::Failed
        || remediator.execution_profile.as_deref() != Some("remediator")
        || current.id() != source.id()
        || current.snapshot().spec != source.snapshot().spec
        || current.revision() <= source.revision()
        || verdict.status != super::DeliveryVerdictStatus::Fail
        || verdict.candidate_ref != candidate_ref
        || old_run.state != WorkRunState::CandidateReady
        || retired.state != WorkRunState::Settled
        || new_run.work_item_id != old_run.work_item_id
        || new_run.work_item_revision != old_run.work_item_revision
        || selected_writer(current.snapshot()).is_none_or(|b| b.id != remediator.id)
        || authorization_digest.0.len() != 71
        || !authorization_digest.0.starts_with("sha256:")
    {
        return Err(invalid());
    }
    let floor = current.snapshot().session_bindings.len();
    Ok(VerifiedUnchangedReworkRecovery {
        source: current.clone(),
        fact: SameCandidateReverificationFact {
            producer_work_run_id: producer.work_run_id.clone(),
            remediator_work_run_id: remediator.work_run_id.clone(),
            remediator_job_id: verified.execution_job_id().clone(),
            candidate_ref: candidate_ref.into(),
            candidate_digest: verdict.candidate_digest.clone(),
            authorization_digest: authorization_digest.clone(),
            source_delivery_revision: source.revision(),
            binding_floor: floor,
            binding_prefix_digest: prefix(current.snapshot(), floor)?,
        },
    })
}

impl VerifiedUnchangedReworkRecovery {
    /// Applies recovery inside the same terminal transaction. The failed run stays failed.
    /// # Errors
    /// Rejects any terminal transition other than the exact next revision of the sealed source.
    pub fn apply(
        &self,
        source: &Delivery,
        settled: &Delivery,
    ) -> Result<Delivery, DeliveryValidationError> {
        if source != &self.source
            || settled.id() != self.source.id()
            || settled.revision() != self.source.revision() + 1
        {
            return Err(invalid());
        }
        let mut snapshot = settled.clone().into_snapshot();
        let remediator = snapshot
            .work_run_aggregate
            .runs
            .iter()
            .find(|r| {
                r.id == self.fact.remediator_work_run_id
                    && r.execution_job_id == self.fact.remediator_job_id
            })
            .ok_or_else(invalid)?;
        if remediator.state != WorkRunState::Failed {
            return Err(invalid());
        }
        let item_id = remediator.work_item_id.clone();
        let producer = snapshot
            .work_run_aggregate
            .runs
            .iter_mut()
            .find(|r| r.id == self.fact.producer_work_run_id)
            .ok_or_else(invalid)?;
        if producer.state != WorkRunState::Settled {
            return Err(invalid());
        }
        producer.state = WorkRunState::CandidateReady;
        producer.revision.0 = producer.revision.0.checked_add(1).ok_or_else(invalid)?;
        let item = snapshot
            .work_run_aggregate
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .ok_or_else(invalid)?;
        if item.state != WorkItemState::Failed {
            return Err(invalid());
        }
        item.state = WorkItemState::CandidateReady;
        snapshot.status = DeliveryStatus::Ready;
        snapshot.verdict = None;
        snapshot.evidence.clear();
        snapshot.same_candidate_reverification = Some(self.fact.clone());
        Delivery::try_from_snapshot(snapshot)
    }
}

pub(crate) fn validate_fact(snapshot: &DeliverySnapshot) -> Result<(), DeliveryValidationError> {
    let Some(fact) = &snapshot.same_candidate_reverification else {
        return Ok(());
    };
    let producer = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|r| r.id == fact.producer_work_run_id)
        .ok_or_else(invalid)?;
    let remediator = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|r| {
            r.id == fact.remediator_work_run_id && r.execution_job_id == fact.remediator_job_id
        })
        .ok_or_else(invalid)?;
    let last_writer = snapshot
        .session_bindings
        .iter()
        .rev()
        .find(|b| {
            matches!(
                b.execution_profile.as_deref(),
                Some("executor" | "remediator")
            )
        })
        .ok_or_else(invalid)?;
    if remediator.state != WorkRunState::Failed
        || producer.work_item_id != remediator.work_item_id
        || producer.work_item_revision != remediator.work_item_revision
        || !matches!(
            producer.state,
            WorkRunState::CandidateReady | WorkRunState::Settled
        )
        || last_writer.work_run_id != fact.remediator_work_run_id
        || fact.source_delivery_revision >= snapshot.revision
        || fact.binding_floor == 0
        || prefix(snapshot, fact.binding_floor)? != fact.binding_prefix_digest
        || fact.candidate_ref.is_empty()
    {
        return Err(invalid());
    }
    Ok(())
}

/// Selects the latest writer, or the exact producer named by the active recovery fact.
#[must_use]
pub fn selected_writer(snapshot: &DeliverySnapshot) -> Option<&SessionBinding> {
    if let Some(fact) = &snapshot.same_candidate_reverification {
        return snapshot
            .session_bindings
            .iter()
            .find(|b| b.work_run_id == fact.producer_work_run_id);
    }
    snapshot
        .session_bindings
        .iter()
        .filter(|b| {
            matches!(
                b.execution_profile.as_deref(),
                Some("executor" | "remediator")
            )
        })
        .max_by_key(|b| (b.bound_at_millis, b.attempt))
}

/// The latest Reviewer after the recovery floor starts the current full review round.
#[must_use]
pub fn verification_round_floor(snapshot: &DeliverySnapshot) -> usize {
    let minimum = snapshot
        .same_candidate_reverification
        .as_ref()
        .map_or(0, |fact| fact.binding_floor);
    let Some(writer) = selected_writer(snapshot) else {
        return snapshot.session_bindings.len();
    };
    snapshot
        .session_bindings
        .iter()
        .enumerate()
        .skip(minimum)
        .filter(|(_, b)| {
            b.execution_profile.as_deref() == Some("reviewer")
                && b.work_contract_id == writer.work_contract_id
                && b.work_contract_revision == writer.work_contract_revision
                && b.work_item_id == writer.work_item_id
                && b.work_item_revision == writer.work_item_revision
        })
        .map(|(index, _)| index)
        .next_back()
        .unwrap_or({
            if minimum > 0 {
                snapshot.session_bindings.len()
            } else {
                0
            }
        })
}
