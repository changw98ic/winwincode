// SPDX-License-Identifier: Apache-2.0

//! Product-owned refusal of an already successful writer's candidate.
//! The execution result and source remain immutable. A typed Attention records
//! why that output cannot enter verification; it is not another Candidate.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, AttentionItemId, ExecutionAckSequence, ExecutionJobId, Sha256Digest, WorkRunId,
    WorkRunState,
};

use super::{CoordinationError, CoordinationErrorCode};
use crate::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, DELIVERY_SCHEMA_VERSION, Delivery,
    DeliveryId, DeliverySpecId, DeliveryStatus, SessionBindingId, same_candidate::selected_writer,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRejectionReason {
    MissingReworkAuthorization,
    ReworkAuthorizationMismatch,
    ReworkSourceOutsideAuthorization,
    InvalidCandidateSource,
}

/// Exact immutable source and execution coordinates observed by the CP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateRejectionFact {
    pub version: u8,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub delivery_spec_revision: u64,
    pub source_delivery_revision: u64,
    pub session_binding_id: SessionBindingId,
    pub work_run_id: WorkRunId,
    pub job_id: ExecutionJobId,
    pub attempt: u64,
    pub artifact_id: ArtifactId,
    pub artifact_digest: Sha256Digest,
    pub last_event_sequence: ExecutionAckSequence,
    pub candidate_commit_id: String,
    pub candidate_tree_id: String,
    pub diff_sha256: String,
    pub authorization_digest: Option<Sha256Digest>,
    pub reason: CandidateRejectionReason,
}

impl CandidateRejectionFact {
    /// Derives the canonical Attention identity from the complete refusal fact.
    ///
    /// # Panics
    ///
    /// Panics only if the serialization contract of the fact's plain value
    /// fields is broken and the fact can no longer be encoded as JSON.
    #[must_use]
    pub fn attention_id(&self) -> AttentionItemId {
        // All fields are plain serializable values. The complete fact binds
        // the stable refusal identity, including its source and authorization.
        let bytes = serde_json::to_vec(self).expect("candidate rejection fact serializes");
        let digest = Sha256::digest(bytes);
        let mut prefix = [0_u8; 16];
        prefix.copy_from_slice(&digest[..16]);
        let mut value = u128::from_be_bytes(prefix);
        let mut encoded = [b'0'; 26];
        for byte in encoded.iter_mut().rev() {
            *byte = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"[(value & 31) as usize];
            value >>= 5;
        }
        AttentionItemId(format!(
            "att_{}",
            encoded.into_iter().map(char::from).collect::<String>()
        ))
    }

    fn matches_writer(&self, delivery: &Delivery) -> bool {
        let Some(writer) = selected_writer(delivery.snapshot()) else {
            return false;
        };
        self.version == 1
            && self.delivery_id == *delivery.id()
            && self.delivery_spec_id == delivery.snapshot().spec.id
            && self.delivery_spec_revision == delivery.snapshot().spec.revision
            && self.source_delivery_revision <= delivery.revision()
            && self.session_binding_id == writer.id
            && self.work_run_id == writer.work_run_id
            && self.job_id == writer.execution_job_id
            && self.attempt == writer.attempt
            && delivery
                .snapshot()
                .work_run_aggregate
                .runs
                .iter()
                .any(|run| {
                    run.id == self.work_run_id
                        && run.execution_job_id == self.job_id
                        && u64::try_from(run.attempt).ok() == Some(self.attempt)
                        && run.state == WorkRunState::CandidateReady
                })
    }
}

/// Returns only the canonical, exact currently rejected writer's Attention.
#[must_use]
pub fn current_candidate_rejection(delivery: &Delivery) -> Option<CandidateRejectionFact> {
    if delivery.snapshot().status != DeliveryStatus::NeedsAttention {
        return None;
    }
    delivery.snapshot().attention_items.iter().find_map(|item| {
        let fact = rejection_attention_fact(item)?;
        (item.status == AttentionItemStatus::Open && fact.matches_writer(delivery)).then_some(fact)
    })
}

#[must_use]
pub fn rejection_attention_fact(item: &AttentionItem) -> Option<CandidateRejectionFact> {
    if item.item_type != AttentionItemType::VerificationBlocked
        || !item.blocking
        || !winwincode_domain::is_canonical_prefixed_id(&item.id.0, "att_")
    {
        return None;
    }
    let fact: CandidateRejectionFact = serde_json::from_str(&item.context).ok()?;
    (fact.version == 1
        && item.id == fact.attention_id()
        && item.delivery_id == fact.delivery_id
        && item.delivery_spec_id == fact.delivery_spec_id
        && item.work_run_id.as_ref() == Some(&fact.work_run_id)
        && serde_json::to_string(&fact).ok().as_deref() == Some(item.context.as_str()))
    .then_some(fact)
}

/// Non-deserializable seal for the specialized journal mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateRejectedTransition {
    source_delivery: Delivery,
    delivery: Delivery,
}

impl CandidateRejectedTransition {
    /// Seals a CP-observed refusal after the exact writer success was committed.
    /// # Errors
    /// Rejects stale versions, a foreign/non-successful writer, or a duplicate refusal.
    pub fn new(
        delivery: &Delivery,
        fact: CandidateRejectionFact,
    ) -> Result<Self, CoordinationError> {
        if !fact.matches_writer(delivery)
            || fact.source_delivery_revision != delivery.revision()
            || delivery
                .snapshot()
                .attention_items
                .iter()
                .any(|item| item.id == fact.attention_id())
            || delivery
                .snapshot()
                .work_run_aggregate
                .runs
                .iter()
                .any(|run| {
                    matches!(
                        run.state,
                        WorkRunState::Queued | WorkRunState::Leased | WorkRunState::Running
                    )
                })
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "candidate refusal does not match the current successful idle writer",
            ));
        }
        let context = serde_json::to_string(&fact).map_err(|error| {
            CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
        })?;
        let mut snapshot = delivery.clone().into_snapshot();
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= crate::domain::MAX_SAFE_INTEGER)
            .ok_or_else(|| {
                CoordinationError::new(
                    CoordinationErrorCode::RevisionConflict,
                    "Delivery revision exhausted",
                )
            })?;
        snapshot.status = DeliveryStatus::NeedsAttention;
        snapshot.attention_items.push(AttentionItem {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: fact.attention_id(),
            delivery_id: fact.delivery_id,
            delivery_spec_id: fact.delivery_spec_id,
            work_run_id: Some(fact.work_run_id),
            item_type: AttentionItemType::VerificationBlocked,
            title: "Candidate rejected. Update the task specification to start again.".into(),
            context,
            options: vec![],
            assigned_to: None,
            blocking: true,
            status: AttentionItemStatus::Open,
            resolution: None,
            resolved_by: None,
            created_at_millis: snapshot.updated_at_millis,
            resolved_at_millis: None,
        });
        let next = Delivery::try_from_snapshot(snapshot).map_err(|error| {
            CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
        })?;
        Ok(Self {
            source_delivery: delivery.clone(),
            delivery: next,
        })
    }

    #[must_use]
    pub const fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub(crate) fn validate_source(&self, current: &Delivery) -> Result<(), CoordinationError> {
        if current != &self.source_delivery {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "Delivery changed before candidate refusal",
            ));
        }
        Ok(())
    }
}
