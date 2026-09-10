// SPDX-License-Identifier: Apache-2.0

//! Durable Approval and Delivery Attention Inbox source adapter.

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_delivery::domain::{AttentionItemStatus, AttentionItemType};
use winwincode_domain::{Instant, RepositoryScope, Sha256Digest};

use crate::{
    CollaborationCandidateIdentity, CollaborationInboxItemId, CollaborationInboxItemKind,
    CollaborationInboxItemState, CollaborationInboxSourceError, CollaborationInboxSourceItem,
    CollaborationInboxSourcePort, CollaborationInboxSourceSnapshot,
    FormalCollaborationCommandRoute, ProductSessionPersistence, ResponsibilityReviewKind,
    ResponsibilityRole, ResponsibilityTarget,
    chat_interaction_application::collaboration_approval_snapshot,
    delivery_application::collaboration_delivery_snapshot, repository_scope_key,
    session_binding_transaction::instant_millis,
};

/// Durable scope-wide Approval and Attention source backed by canonical state.
pub struct DurableCollaborationInboxSource {
    storage: Box<dyn ProductSessionPersistence>,
}

impl DurableCollaborationInboxSource {
    #[must_use]
    pub fn new(storage: Box<dyn ProductSessionPersistence>) -> Self {
        Self { storage }
    }
}

impl CollaborationInboxSourcePort for DurableCollaborationInboxSource {
    fn snapshot(
        &mut self,
        scope: &RepositoryScope,
    ) -> Result<CollaborationInboxSourceSnapshot, CollaborationInboxSourceError> {
        source_snapshot(self.storage.as_ref(), scope).map_err(|()| CollaborationInboxSourceError)
    }
}

fn source_snapshot(
    storage: &dyn ProductSessionPersistence,
    scope: &RepositoryScope,
) -> Result<CollaborationInboxSourceSnapshot, ()> {
    let scope_key = repository_scope_key(scope).map_err(|_| ())?;
    let approvals = collaboration_approval_snapshot(
        storage,
        &scope_key,
        &Instant("1970-01-01T00:00:00.000Z".to_owned()),
    )
    .map_err(|_| ())?;
    let deliveries = collaboration_delivery_snapshot(storage, scope).map_err(|_| ())?;
    let mut items = Vec::new();
    let mut item_state_guards = BTreeMap::new();
    for approval in approvals.approvals {
        let source_sha256 = digest(&(&approvals.snapshot_sha256, &approval))?;
        let candidate = approval
            .candidate
            .map(|candidate| CollaborationCandidateIdentity {
                candidate_ref: candidate.candidate_ref,
                candidate_digest: candidate.candidate_digest,
                candidate_revision: candidate.candidate_revision,
            });
        let target = match (&approval.delivery_id, &candidate) {
            (Some(delivery_id), Some(_)) => ResponsibilityTarget::Review {
                delivery_id: delivery_id.clone(),
                review: ResponsibilityReviewKind::Solution,
            },
            _ => ResponsibilityTarget::ProductSession {
                product_session_id: approval.projection.binding.product_session_id.clone(),
            },
        };
        let id = CollaborationInboxItemId::Approval(approval.projection.id.clone());
        items.push(CollaborationInboxSourceItem {
            id: id.clone(),
            kind: CollaborationInboxItemKind::Approval,
            target,
            responsibility_role: ResponsibilityRole::Approver,
            source_revision: u64::try_from(approval.projection.revision.0).map_err(|_| ())?,
            source_sha256,
            title_sha256: digest(&approval.projection.subject)?,
            opened_at_millis: instant_millis(&approval.projection.requested_at).map_err(|_| ())?,
            expires_at_millis: Some(
                instant_millis(&approval.projection.expires_at).map_err(|_| ())?,
            ),
            state: approval_state(&approval.projection.state)?,
            candidate,
            command_route: FormalCollaborationCommandRoute::ApprovalDecide {
                approval_id: approval.projection.id,
                product_session_id: approval.projection.binding.product_session_id,
            },
        });
        item_state_guards.insert(id, vec![approvals.state_guard.clone()]);
    }
    let mut revision = approvals.revision;
    for record in deliveries.records {
        revision = revision.checked_add(record.delivery.revision()).ok_or(())?;
        for attention in &record.delivery.snapshot().attention_items {
            let (target, responsibility_role) =
                attention_responsibility(attention.item_type, &attention.delivery_id);
            let id = CollaborationInboxItemId::DeliveryAttention(attention.id.clone());
            items.push(CollaborationInboxSourceItem {
                id: id.clone(),
                kind: CollaborationInboxItemKind::DeliveryAttention,
                target,
                responsibility_role,
                source_revision: record.delivery.revision(),
                source_sha256: digest(attention)?,
                title_sha256: digest(&attention.title)?,
                opened_at_millis: attention.created_at_millis,
                expires_at_millis: None,
                state: attention_state(attention.status),
                candidate: None,
                command_route: FormalCollaborationCommandRoute::DeliveryResolveAttention {
                    attention_item_id: attention.id.clone(),
                    delivery_id: attention.delivery_id.clone(),
                },
            });
            item_state_guards.insert(id, record.state_guards.clone());
        }
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(CollaborationInboxSourceSnapshot {
        scope: scope.clone(),
        revision,
        snapshot_sha256: digest(&items)?,
        item_state_guards,
        items,
    })
}

fn attention_responsibility(
    item_type: AttentionItemType,
    delivery_id: &winwincode_domain::DeliveryId,
) -> (ResponsibilityTarget, ResponsibilityRole) {
    match item_type {
        AttentionItemType::DecisionRequired => (
            ResponsibilityTarget::Review {
                delivery_id: delivery_id.clone(),
                review: ResponsibilityReviewKind::Solution,
            },
            ResponsibilityRole::Reviewer,
        ),
        AttentionItemType::DeliveryApproval => (
            ResponsibilityTarget::Review {
                delivery_id: delivery_id.clone(),
                review: ResponsibilityReviewKind::Delivery,
            },
            ResponsibilityRole::Approver,
        ),
        AttentionItemType::RequirementQuestion
        | AttentionItemType::VerificationBlocked
        | AttentionItemType::ScopeChange => (
            ResponsibilityTarget::Delivery {
                delivery_id: delivery_id.clone(),
            },
            ResponsibilityRole::Assignee,
        ),
    }
}

fn approval_state(value: &str) -> Result<CollaborationInboxItemState, ()> {
    match value {
        "pending" => Ok(CollaborationInboxItemState::Pending),
        "approved" => Ok(CollaborationInboxItemState::Approved),
        "rejected" => Ok(CollaborationInboxItemState::Rejected),
        "expired" => Ok(CollaborationInboxItemState::Expired),
        _ => Err(()),
    }
}

const fn attention_state(value: AttentionItemStatus) -> CollaborationInboxItemState {
    match value {
        AttentionItemStatus::Open => CollaborationInboxItemState::Pending,
        AttentionItemStatus::Resolved | AttentionItemStatus::Dismissed => {
            CollaborationInboxItemState::Resolved
        }
    }
}

fn digest<T: Serialize>(value: &T) -> Result<Sha256Digest, ()> {
    let bytes = serde_json::to_vec(value).map_err(|_| ())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}
