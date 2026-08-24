// SPDX-License-Identifier: Apache-2.0

//! Same-cut composition over the verified aggregate journal and trusted source ports.

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{Actor, RepositoryScope};
use winwincode_delivery::{
    domain::{
        AttentionItemStatus, AttentionItemType, Delivery, DeliveryStage, DeliveryVerdictStatus,
        StageRunStatus,
    },
    projection::{DeliveryDetailProjection, ProjectionInput, project_delivery_detail},
    store::{DeliveryJournalCodec, JournalEntryState, JournalRecordBytes, LoadedDeliveryJournal},
};
use winwincode_domain::{DeliveryId, OpaqueCursor, Revision, Sha256Digest};

use super::{
    DeliveryRuntimeReadRequest, PublicationFactBinding, StrongFlowProjectionError,
    TrustedProjectionReadError, TrustedPublicationProjectionRead, TrustedRuntimeProjectionRead,
};
use crate::{AggregateJournalKey, ControlPlane};

const MAX_QUERY_LIMIT: usize = 200;

/// Exact current publishable fact set joined to one bounded read cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationAuthorizationSnapshot {
    binding: PublicationFactBinding,
    read_cursor: OpaqueCursor,
}

impl PublicationAuthorizationSnapshot {
    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn read_cursor(&self) -> &OpaqueCursor {
        &self.read_cursor
    }
}

#[derive(Debug, Clone)]
pub(super) struct EstablishedDeliveryRead {
    pub detail: DeliveryDetailProjection,
    pub runtime: TrustedRuntimeProjectionRead,
    pub publication: TrustedPublicationProjectionRead,
    pub cursor: BoundedReadCursor,
    pub publication_authorization: Option<PublicationAuthorizationSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct BoundedReadCursor {
    pub token: OpaqueCursor,
    pub scope: RepositoryScope,
    pub delivery_id: DeliveryId,
    pub delivery_revision: u64,
    pub runtime_ledger_revision: Revision,
    pub runtime_accepted_sequence: u64,
    pub publication_revision: Revision,
    pub limit: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorSeal<'cut> {
    actor_sha256: &'cut str,
    scope: &'cut RepositoryScope,
    delivery_id: &'cut DeliveryId,
    delivery_revision: u64,
    runtime_ledger_revision: &'cut Revision,
    runtime_accepted_sequence: u64,
    runtime_source_seal: &'cut Sha256Digest,
    publication_revision: &'cut Revision,
    publication_source_seal: &'cut Sha256Digest,
    limit: usize,
}

pub(super) fn establish_delivery_read(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    limit: i64,
) -> Result<EstablishedDeliveryRead, StrongFlowProjectionError> {
    let limit = validate_limit(limit)?;
    validate_scope(scope)?;
    let actor_sha256 = actor_digest(actor)?;
    let sources = control_plane.strongflow_sources.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted runtime and publication facts are unavailable".to_owned(),
        )
    })?;

    let first = load_current(control_plane, delivery_id)?;
    let publication = sources
        .publication
        .read_current(scope, delivery_id, first.revision(), None)
        .map_err(source_error)?;
    require_publication_revision(&publication, first.revision())?;
    let detail = project_delivery_detail(publication.candidate().map_or_else(
        || ProjectionInput::new(&first),
        |candidate| ProjectionInput::new(&first).with_candidate(candidate),
    ))?;
    let binding = derive_publication_binding(&detail)?;
    validate_publication_result(&publication, binding.as_ref())?;

    let runtime_request = DeliveryRuntimeReadRequest::new(
        scope.clone(),
        delivery_id.clone(),
        first.revision(),
        None,
        limit,
    );
    let runtime = sources
        .runtime
        .read_delivery(&runtime_request)
        .map_err(source_error)?;
    validate_runtime_read(&detail, &runtime, limit)?;

    let second = load_current(control_plane, delivery_id)?;
    if second != first {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the current aggregate changed while its read cut was established".to_owned(),
        ));
    }
    let exact_publication = sources
        .publication
        .read_current(
            scope,
            delivery_id,
            first.revision(),
            Some(publication.publication_revision()),
        )
        .map_err(source_error)?;
    if exact_publication != publication {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "publication facts changed while the read cut was established".to_owned(),
        ));
    }
    let exact_runtime = sources
        .runtime
        .read_delivery(&DeliveryRuntimeReadRequest::new(
            scope.clone(),
            delivery_id.clone(),
            first.revision(),
            Some(super::RuntimeCutExpectation::new(
                runtime.ledger_revision().clone(),
                runtime.accepted_sequence(),
            )),
            limit,
        ))
        .map_err(source_error)?;
    if !same_runtime_cut(&runtime, &exact_runtime) {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "runtime facts changed while the read cut was established".to_owned(),
        ));
    }

    let cursor = bounded_cursor(
        &actor_sha256,
        scope,
        delivery_id,
        first.revision(),
        &runtime,
        &publication,
        limit,
    )?;
    let publication_authorization = binding.map(|binding| PublicationAuthorizationSnapshot {
        binding,
        read_cursor: cursor.token.clone(),
    });
    Ok(EstablishedDeliveryRead {
        detail,
        runtime,
        publication,
        cursor,
        publication_authorization,
    })
}

fn load_current(
    control_plane: &ControlPlane,
    delivery_id: &DeliveryId,
) -> Result<Delivery, StrongFlowProjectionError> {
    let key = AggregateJournalKey::new("delivery", &delivery_id.0).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("aggregate identity is invalid".to_owned())
    })?;
    let journal = control_plane
        .storage_ref()
        .map_err(|_| {
            StrongFlowProjectionError::ServiceUnavailable(
                "canonical storage is unavailable".to_owned(),
            )
        })?
        .load_journal(&key)
        .map_err(|_| {
            StrongFlowProjectionError::ServiceUnavailable(
                "canonical journal cannot be read".to_owned(),
            )
        })?
        .ok_or_else(|| {
            StrongFlowProjectionError::ResourceNotFound(
                "the requested aggregate was not found".to_owned(),
            )
        })?;
    let loaded = LoadedDeliveryJournal {
        manifest: journal.manifest,
        records: journal
            .records
            .into_iter()
            .map(|record| JournalRecordBytes {
                sequence: record.sequence,
                state: JournalEntryState::Published,
                digest: record.digest,
                bytes: record.payload,
            })
            .collect(),
    };
    DeliveryJournalCodec::verify(delivery_id, loaded)
        .map(|stored| stored.snapshot)
        .map_err(|_| {
            StrongFlowProjectionError::ServiceUnavailable(
                "canonical journal verification failed".to_owned(),
            )
        })
}

fn validate_runtime_read(
    detail: &DeliveryDetailProjection,
    runtime: &TrustedRuntimeProjectionRead,
    _limit: usize,
) -> Result<(), StrongFlowProjectionError> {
    if runtime.delivery_revision() != detail.delivery_revision()
        || runtime.snapshot().delivery_id != *detail.delivery_id()
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "runtime facts belong to another aggregate revision".to_owned(),
        ));
    }
    for session in &runtime.snapshot().sessions {
        let stage = detail
            .stages()
            .iter()
            .find(|stage| stage.id() == &session.stage_run_id)
            .ok_or_else(|| {
                StrongFlowProjectionError::RevisionConflict(
                    "runtime facts name a stage outside the current aggregate".to_owned(),
                )
            })?;
        let binding = stage.session_binding().ok_or_else(|| {
            StrongFlowProjectionError::RevisionConflict(
                "runtime facts name a stage without a current session binding".to_owned(),
            )
        })?;
        if stage.delivery_task_id() != session.delivery_task_id.as_ref()
            || stage.attempt() != session.attempt
            || binding.binding_id() != &session.session_binding_id
            || binding.product_session_id() != &session.product_session_id
            || binding.execution_job_id() != &session.execution_job_id
            || binding.worker_session_id() != Some(&session.worker_session_id)
            || binding.codex_thread_id() != Some(&session.codex_thread_id)
        {
            return Err(StrongFlowProjectionError::RevisionConflict(
                "runtime facts no longer match the complete current binding".to_owned(),
            ));
        }
    }
    Ok(())
}

fn derive_publication_binding(
    detail: &DeliveryDetailProjection,
) -> Result<Option<PublicationFactBinding>, StrongFlowProjectionError> {
    let Some(candidate) = detail.current_candidate() else {
        return Ok(None);
    };
    let Some(verdict) = detail.verdict() else {
        return Ok(None);
    };
    let Some(target) = detail.requirements().spec().publication_target() else {
        return Ok(None);
    };
    if verdict.status() != DeliveryVerdictStatus::Pass
        || verdict.delivery_spec_id() != candidate.delivery_spec_id()
        || verdict.delivery_spec_revision() != candidate.delivery_spec_revision()
        || verdict.candidate_ref() != candidate.candidate_ref()
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "current candidate and passing verdict are not exact".to_owned(),
        ));
    }
    let approvals = detail
        .attention()
        .iter()
        .filter(|item| {
            item.item_type() == AttentionItemType::DeliveryApproval
                && item.status() == AttentionItemStatus::Resolved
                && item.delivery_spec_id() == candidate.delivery_spec_id()
        })
        .filter_map(|item| {
            let stage_id = item.stage_run_id()?;
            let stage = detail
                .stages()
                .iter()
                .find(|stage| stage.id() == stage_id)?;
            (stage.stage() == DeliveryStage::DeliveryReview
                && stage.status() == StageRunStatus::Succeeded
                && item.resolution_summary().is_some()
                && item.resolved_by().is_some()
                && item.resolved_at().is_some())
            .then_some(item)
        })
        .collect::<Vec<_>>();
    if approvals.len() != 1 {
        return Ok(None);
    }
    let approval = approvals[0];
    let target_sha256 = sha256_json(target)?;
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ApprovalSeal<'value> {
        approval_id: &'value winwincode_domain::AttentionItemId,
        stage_run_id: Option<&'value winwincode_domain::StageRunId>,
        resolved_by: Option<&'value str>,
        resolution: Option<&'value str>,
        resolved_at: Option<u64>,
        candidate_ref: &'value str,
        verdict_id: &'value winwincode_delivery::domain::DeliveryVerdictId,
        target_sha256: &'value str,
    }
    let approval_review_set_sha256 = sha256_json(&ApprovalSeal {
        approval_id: approval.id(),
        stage_run_id: approval.stage_run_id(),
        resolved_by: approval.resolved_by(),
        resolution: approval.resolution_summary(),
        resolved_at: approval.resolved_at(),
        candidate_ref: candidate.candidate_ref(),
        verdict_id: verdict.id(),
        target_sha256: &target_sha256,
    })?;
    PublicationFactBinding::try_new(
        detail.delivery_id().clone(),
        detail.delivery_revision(),
        candidate.delivery_spec_id().clone(),
        candidate.delivery_spec_revision(),
        candidate.candidate_ref(),
        candidate.diff_sha256(),
        verdict.id().clone(),
        approval.id().clone(),
        approval_review_set_sha256,
        target_sha256,
    )
    .map(Some)
    .map_err(source_error)
}

fn validate_publication_result(
    publication: &TrustedPublicationProjectionRead,
    expected: Option<&PublicationFactBinding>,
) -> Result<(), StrongFlowProjectionError> {
    match (publication.result(), expected) {
        (Some(result), Some(expected)) if result.binding() == expected => Ok(()),
        (None, _) => Ok(()),
        _ => Err(StrongFlowProjectionError::RevisionConflict(
            "publication result is not bound to the exact current approved fact set".to_owned(),
        )),
    }
}

fn require_publication_revision(
    publication: &TrustedPublicationProjectionRead,
    delivery_revision: u64,
) -> Result<(), StrongFlowProjectionError> {
    if publication.delivery_revision() == delivery_revision {
        Ok(())
    } else {
        Err(StrongFlowProjectionError::RevisionConflict(
            "publication facts belong to another aggregate revision".to_owned(),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn bounded_cursor(
    actor_sha256: &str,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    runtime: &TrustedRuntimeProjectionRead,
    publication: &TrustedPublicationProjectionRead,
    limit: usize,
) -> Result<BoundedReadCursor, StrongFlowProjectionError> {
    let seal = CursorSeal {
        actor_sha256,
        scope,
        delivery_id,
        delivery_revision,
        runtime_ledger_revision: runtime.ledger_revision(),
        runtime_accepted_sequence: runtime.accepted_sequence(),
        runtime_source_seal: runtime.source_seal(),
        publication_revision: publication.publication_revision(),
        publication_source_seal: publication.source_seal(),
        limit,
    };
    let token = OpaqueCursor(format!("sfc1:{}", sha256_json(&seal)?));
    Ok(BoundedReadCursor {
        token,
        scope: scope.clone(),
        delivery_id: delivery_id.clone(),
        delivery_revision,
        runtime_ledger_revision: runtime.ledger_revision().clone(),
        runtime_accepted_sequence: runtime.accepted_sequence(),
        publication_revision: publication.publication_revision().clone(),
        limit,
    })
}

fn same_runtime_cut(
    first: &TrustedRuntimeProjectionRead,
    second: &TrustedRuntimeProjectionRead,
) -> bool {
    first.delivery_revision() == second.delivery_revision()
        && first.ledger_revision() == second.ledger_revision()
        && first.accepted_sequence() == second.accepted_sequence()
        && first.rebuilt_at() == second.rebuilt_at()
        && first.source_seal() == second.source_seal()
        && first.snapshot() == second.snapshot()
}

fn validate_limit(limit: i64) -> Result<usize, StrongFlowProjectionError> {
    usize::try_from(limit)
        .ok()
        .filter(|limit| (1..=MAX_QUERY_LIMIT).contains(limit))
        .ok_or_else(|| {
            StrongFlowProjectionError::InvalidRequest(
                "projection page limit must be between 1 and 200".to_owned(),
            )
        })
}

fn validate_scope(scope: &RepositoryScope) -> Result<(), StrongFlowProjectionError> {
    if scope.kind == "repository"
        && portable(&scope.organization_id.0)
        && portable(&scope.workspace_id.0)
        && portable(&scope.project_id.0)
        && portable(&scope.repository_id.0)
    {
        Ok(())
    } else {
        Err(StrongFlowProjectionError::PermissionDenied(
            "a complete repository scope is required".to_owned(),
        ))
    }
}

fn actor_digest(actor: &Actor) -> Result<String, StrongFlowProjectionError> {
    let bytes = serde_json::to_vec(actor).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("actor identity is invalid".to_owned())
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn sha256_json(value: &impl Serialize) -> Result<String, StrongFlowProjectionError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        StrongFlowProjectionError::Internal("projection identity cannot be encoded".to_owned())
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn source_error(source: TrustedProjectionReadError) -> StrongFlowProjectionError {
    match source {
        TrustedProjectionReadError::Unavailable => {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted projection facts are unavailable".to_owned(),
            )
        }
        TrustedProjectionReadError::TemporarilyUnavailable => {
            StrongFlowProjectionError::ServiceUnavailable(
                "trusted projection facts are temporarily unavailable".to_owned(),
            )
        }
        TrustedProjectionReadError::Stale => StrongFlowProjectionError::RevisionConflict(
            "trusted projection facts no longer name the requested cut".to_owned(),
        ),
        TrustedProjectionReadError::Invalid => StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted projection facts are not canonical".to_owned(),
        ),
    }
}

fn portable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}
