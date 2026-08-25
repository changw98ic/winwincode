// SPDX-License-Identifier: Apache-2.0

//! Same-cut composition over the verified aggregate journal and trusted source ports.

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, DeliveryEventReadCursor, DeliveryEventReadStream, DeliveryEventReadStreamKind,
    ProductSessionEventReadCursor, ProductSessionEventReadStream,
    ProductSessionEventReadStreamKind, RepositoryScope, RepositoryScopeKind, StrongFlowReadCursor,
};
use winwincode_delivery::{
    domain::{
        AttentionItemStatus, AttentionItemType, Delivery, DeliveryStage, DeliveryStatus,
        DeliveryVerdictStatus, StageRunActorType, StageRunStatus,
    },
    projection::{DeliveryProjection, ProjectionInput, project_delivery_detail},
    store::{
        AtomicPublication, DeliveryJournalPort, DeliveryQuery, DeliveryQueryPort, DeliveryStore,
        JournalBackendError, JournalBackendErrorCode, JournalEntryState, JournalRecordBytes,
        LoadedDeliveryJournal,
    },
};
use winwincode_domain::{DeliveryId, EventReadPosition, OpaqueCursor, Revision, Sha256Digest};

use super::{
    DeliveryRuntimeReadRequest, ProductSessionRuntimeReadRequest, PublicationFactBinding,
    StrongFlowProjectionError, TrustedProjectionReadError, TrustedPublicationProjectionRead,
    TrustedRuntimeProjectionRead,
};
use crate::{
    AggregateJournalKey, ControlPlane, ProjectionEventCursor, ProjectionEventStream,
    ProjectionEventStreamKey, StorageErrorKind, repository_scope_key,
};

const MAX_QUERY_LIMIT: usize = 200;

/// Exact current publishable fact set joined to one bounded read cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicationAuthorizationSnapshot {
    binding: PublicationFactBinding,
    read_cursor: StrongFlowReadCursor,
}

impl PublicationAuthorizationSnapshot {
    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn read_cursor(&self) -> &StrongFlowReadCursor {
        &self.read_cursor
    }
}

#[derive(Debug, Clone)]
pub(super) struct EstablishedDeliveryRead {
    pub detail: DeliveryProjection,
    pub runtime: TrustedRuntimeProjectionRead,
    pub publication: TrustedPublicationProjectionRead,
    pub cursor: BoundedReadCursor,
    pub publication_authorization: Option<PublicationAuthorizationSnapshot>,
}

#[derive(Debug, Clone)]
pub(super) struct EstablishedProductSessionRead {
    pub runtime: TrustedRuntimeProjectionRead,
    pub event_cursor: ProductSessionEventReadCursor,
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
    pub event_cursor: DeliveryEventReadCursor,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorSeal<'cut> {
    actor_sha256: &'cut str,
    scope: &'cut RepositoryScope,
    delivery_id: &'cut DeliveryId,
    delivery_revision: u64,
    delivery_content_sha256: &'cut str,
    runtime_ledger_revision: &'cut Revision,
    runtime_accepted_sequence: u64,
    runtime_source_seal: &'cut Sha256Digest,
    runtime_content_sha256: &'cut str,
    publication_revision: &'cut Revision,
    publication_source_seal: &'cut Sha256Digest,
    publication_content_sha256: &'cut str,
    event_cursor: &'cut DeliveryEventReadCursor,
    page_limit: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeContentSeal<'cut> {
    scope: &'cut RepositoryScope,
    delivery_revision: u64,
    ledger_revision: &'cut Revision,
    accepted_sequence: u64,
    rebuilt_at: &'cut winwincode_domain::Instant,
    snapshot: &'cut winwincode_delivery::projection::runtime::RuntimeFoldSnapshot,
    source_seal: &'cut Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationContentSeal<'cut> {
    scope: &'cut RepositoryScope,
    delivery_id: &'cut DeliveryId,
    delivery_revision: u64,
    publication_revision: &'cut Revision,
    candidate: Option<&'cut winwincode_delivery::domain::FrozenDeliveryCandidate>,
    result: Option<&'cut super::PublicationResultFact>,
    source_seal: &'cut Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalSeal<'value> {
    delivery_id: &'value DeliveryId,
    delivery_revision: u64,
    delivery_spec_id: &'value winwincode_delivery::domain::DeliverySpecId,
    delivery_spec_revision: u64,
    stage_run: &'value winwincode_delivery::domain::StageRun,
    attention_item_id: &'value winwincode_domain::AttentionItemId,
    attention_context: &'value str,
    attention_resolution: &'value str,
    assigned_to: &'value str,
    resolved_by: &'value str,
    resolved_at: u64,
    candidate_ref: &'value str,
    diff_sha256: &'value str,
    verdict_id: &'value winwincode_delivery::domain::DeliveryVerdictId,
    target_sha256: &'value str,
}

struct CurrentPublicationApproval<'delivery> {
    run: &'delivery winwincode_delivery::domain::StageRun,
    attention: &'delivery winwincode_delivery::domain::AttentionItem,
    resolution: &'delivery str,
    assigned_to: &'delivery str,
    resolved_by: &'delivery str,
    resolved_at: u64,
}

#[allow(clippy::too_many_lines)]
pub(super) fn establish_delivery_read(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    limit: i64,
) -> Result<EstablishedDeliveryRead, StrongFlowProjectionError> {
    let source_limit = validate_limit(limit)?;
    validate_scope(scope)?;
    let actor_sha256 = actor_digest(actor)?;
    let sources = control_plane.strongflow_sources.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted runtime and publication facts are unavailable".to_owned(),
        )
    })?;
    let event_key = delivery_event_stream_key(scope, delivery_id)?;
    let event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, None)
        .map_err(current_event_storage_error)?;

    let first = load_current(control_plane, delivery_id)?;
    let publication = sources
        .publication
        .read_current(scope, delivery_id, first.revision(), None)
        .map_err(current_source_error)?;
    validate_publication_read(scope, delivery_id, &publication, first.revision())?;
    let detail = project_delivery_detail(publication.candidate().map_or_else(
        || ProjectionInput::new(&first),
        |candidate| ProjectionInput::new(&first).with_candidate(candidate),
    ))?;
    let binding = derive_publication_binding(&first, &detail)?;
    validate_publication_result(&publication, binding.as_ref())?;

    let runtime_request = DeliveryRuntimeReadRequest::new(
        scope.clone(),
        delivery_id.clone(),
        first.revision(),
        None,
        source_limit,
    );
    let runtime = sources
        .runtime
        .read_delivery(&runtime_request)
        .map_err(current_source_error)?;
    validate_runtime_read(scope, &detail, &runtime)?;

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
        .map_err(current_source_error)?;
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
            source_limit,
        ))
        .map_err(current_source_error)?;
    if !same_runtime_cut(&runtime, &exact_runtime) {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "runtime facts changed while the read cut was established".to_owned(),
        ));
    }
    let exact_event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, Some(&event_cursor))
        .map_err(current_event_storage_error)?;
    if exact_event_cursor != event_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the Delivery event stream changed while the read cut was established".to_owned(),
        ));
    }

    let cursor = bounded_cursor(
        &actor_sha256,
        scope,
        &first,
        &runtime,
        &publication,
        &event_cursor,
        limit,
    )?;
    let publication_cursor = generated_cursor(&cursor)?;
    let publication_authorization = binding.map(|binding| PublicationAuthorizationSnapshot {
        binding,
        read_cursor: publication_cursor,
    });
    Ok(EstablishedDeliveryRead {
        detail,
        runtime,
        publication,
        cursor,
        publication_authorization,
    })
}

#[allow(clippy::too_many_lines)]
pub(super) fn replay_delivery_read(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    cursor: &StrongFlowReadCursor,
    limit: i64,
) -> Result<EstablishedDeliveryRead, StrongFlowProjectionError> {
    let source_limit = validate_limit(limit)?;
    validate_scope(scope)?;
    if cursor.scope != *scope || cursor.delivery_id != *delivery_id {
        return Err(StrongFlowProjectionError::PermissionDenied(
            "the read cursor does not authorize this repository and aggregate".to_owned(),
        ));
    }
    if cursor.delivery_revision.0 < 1
        || cursor.runtime_ledger_revision.0 < 0
        || cursor.runtime_accepted_sequence < 0
        || cursor.publication_revision.0 < 0
        || !canonical_cursor_token(&cursor.token)
    {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "the read cursor shape is invalid".to_owned(),
        ));
    }
    let expected_event_cursor =
        parse_delivery_event_cursor(scope, delivery_id, &cursor.event_cursor)?;
    let event_key = expected_event_cursor.key().clone();
    let event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, Some(&expected_event_cursor))
        .map_err(cursor_event_storage_error)?;
    let delivery_revision = u64::try_from(cursor.delivery_revision.0).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("read cursor revision is invalid".to_owned())
    })?;
    let runtime_sequence = u64::try_from(cursor.runtime_accepted_sequence).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "read cursor runtime sequence is invalid".to_owned(),
        )
    })?;
    let actor_sha256 = actor_digest(actor)?;
    let sources = control_plane.strongflow_sources.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted runtime and publication facts are unavailable".to_owned(),
        )
    })?;
    let first = load_revision(control_plane, delivery_id, delivery_revision)?;
    let publication = sources
        .publication
        .read_current(
            scope,
            delivery_id,
            delivery_revision,
            Some(&cursor.publication_revision),
        )
        .map_err(cursor_source_error)?;
    validate_publication_read(scope, delivery_id, &publication, delivery_revision)?;
    let detail = project_delivery_detail(publication.candidate().map_or_else(
        || ProjectionInput::new(&first),
        |candidate| ProjectionInput::new(&first).with_candidate(candidate),
    ))?;
    let binding = derive_publication_binding(&first, &detail)?;
    validate_publication_result(&publication, binding.as_ref())?;
    let runtime = sources
        .runtime
        .read_delivery(&DeliveryRuntimeReadRequest::new(
            scope.clone(),
            delivery_id.clone(),
            delivery_revision,
            Some(super::RuntimeCutExpectation::new(
                cursor.runtime_ledger_revision.clone(),
                runtime_sequence,
            )),
            source_limit,
        ))
        .map_err(cursor_source_error)?;
    validate_runtime_read(scope, &detail, &runtime)?;

    let second = load_revision(control_plane, delivery_id, delivery_revision)?;
    let exact_publication = sources
        .publication
        .read_current(
            scope,
            delivery_id,
            delivery_revision,
            Some(&cursor.publication_revision),
        )
        .map_err(cursor_source_error)?;
    let exact_runtime = sources
        .runtime
        .read_delivery(&DeliveryRuntimeReadRequest::new(
            scope.clone(),
            delivery_id.clone(),
            delivery_revision,
            Some(super::RuntimeCutExpectation::new(
                cursor.runtime_ledger_revision.clone(),
                runtime_sequence,
            )),
            source_limit,
        ))
        .map_err(cursor_source_error)?;
    let exact_event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, Some(&event_cursor))
        .map_err(cursor_event_storage_error)?;
    if second != first
        || exact_publication != publication
        || !same_runtime_cut(&runtime, &exact_runtime)
        || exact_event_cursor != event_cursor
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "trusted facts changed while the exact read cut was replayed".to_owned(),
        ));
    }
    let bounded = bounded_cursor(
        &actor_sha256,
        scope,
        &first,
        &runtime,
        &publication,
        &event_cursor,
        limit,
    )?;
    if bounded.token.0 != cursor.token
        || bounded.runtime_ledger_revision != cursor.runtime_ledger_revision
        || bounded.runtime_accepted_sequence != runtime_sequence
        || bounded.publication_revision != cursor.publication_revision
        || bounded.event_cursor != cursor.event_cursor
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the read cursor signature or trusted cut is stale".to_owned(),
        ));
    }
    let publication_cursor = generated_cursor(&bounded)?;
    let publication_authorization = binding.map(|binding| PublicationAuthorizationSnapshot {
        binding,
        read_cursor: publication_cursor,
    });
    Ok(EstablishedDeliveryRead {
        detail,
        runtime,
        publication,
        cursor: bounded,
        publication_authorization,
    })
}

pub(super) fn establish_product_session_read(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    product_session_id: &winwincode_domain::ProductSessionId,
    limit: usize,
) -> Result<EstablishedProductSessionRead, StrongFlowProjectionError> {
    let sources = control_plane.strongflow_sources.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted runtime facts are unavailable".to_owned(),
        )
    })?;
    let event_key = product_session_event_stream_key(scope, product_session_id)?;
    let event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, None)
        .map_err(current_event_storage_error)?;
    let request = ProductSessionRuntimeReadRequest::new(
        scope.clone(),
        product_session_id.clone(),
        None,
        limit,
    );
    let runtime = sources
        .runtime
        .read_product_session(&request)
        .map_err(current_source_error)?;
    validate_product_session_runtime(scope, product_session_id, &runtime)?;
    let exact_runtime = sources
        .runtime
        .read_product_session(&ProductSessionRuntimeReadRequest::new(
            scope.clone(),
            product_session_id.clone(),
            Some(super::RuntimeCutExpectation::new(
                runtime.ledger_revision().clone(),
                runtime.accepted_sequence(),
            )),
            limit,
        ))
        .map_err(current_source_error)?;
    let exact_event_cursor = control_plane
        .storage_ref()
        .map_err(current_event_storage_error)?
        .load_projection_event_cursor(&event_key, Some(&event_cursor))
        .map_err(current_event_storage_error)?;
    if !same_runtime_cut(&runtime, &exact_runtime) || exact_event_cursor != event_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "ProductSession runtime facts changed while the read cut was established".to_owned(),
        ));
    }
    Ok(EstablishedProductSessionRead {
        runtime,
        event_cursor: generated_product_session_event_cursor(
            scope,
            product_session_id,
            &event_cursor,
        )?,
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
    let journal = ReadOnlyJournal {
        delivery_id: delivery_id.clone(),
        loaded,
    };
    DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::Get(delivery_id.clone()))
        .map_err(|_| {
            StrongFlowProjectionError::ServiceUnavailable(
                "canonical journal verification failed".to_owned(),
            )
        })
}

fn load_revision(
    control_plane: &ControlPlane,
    delivery_id: &DeliveryId,
    revision: u64,
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
    let journal = ReadOnlyJournal {
        delivery_id: delivery_id.clone(),
        loaded: LoadedDeliveryJournal {
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
        },
    };
    let store = DeliveryStore::borrowed(&journal);
    let current = store
        .query(DeliveryQuery::Get(delivery_id.clone()))
        .map_err(|_| {
            StrongFlowProjectionError::ServiceUnavailable(
                "canonical journal verification failed".to_owned(),
            )
        })?;
    if revision > current.revision() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "read cursor names a Delivery revision that has never been issued".to_owned(),
        ));
    }
    if revision == current.revision() {
        return Ok(current);
    }
    store
        .query(DeliveryQuery::GetRevision {
            delivery_id: delivery_id.clone(),
            revision,
        })
        .map_err(|error| match error.code() {
            winwincode_delivery::store::DeliveryStoreErrorCode::DeliveryNotFound => {
                StrongFlowProjectionError::ReadCursorExpired(
                    "the requested Delivery revision is outside the retained read window"
                        .to_owned(),
                )
            }
            _ => StrongFlowProjectionError::ServiceUnavailable(
                "canonical journal verification failed".to_owned(),
            ),
        })
}

struct ReadOnlyJournal {
    delivery_id: DeliveryId,
    loaded: LoadedDeliveryJournal,
}

impl DeliveryJournalPort for ReadOnlyJournal {
    fn load(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        Ok((delivery_id == &self.delivery_id).then(|| self.loaded.clone()))
    }

    fn publish(&self, _publication: AtomicPublication) -> Result<(), JournalBackendError> {
        Err(JournalBackendError::new(
            JournalBackendErrorCode::Io,
            "projection journal is read-only",
        ))
    }
}

fn validate_runtime_read(
    scope: &RepositoryScope,
    detail: &DeliveryProjection,
    runtime: &TrustedRuntimeProjectionRead,
) -> Result<(), StrongFlowProjectionError> {
    validate_runtime_scope(scope, runtime)?;
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

pub(super) fn validate_runtime_scope(
    scope: &RepositoryScope,
    runtime: &TrustedRuntimeProjectionRead,
) -> Result<(), StrongFlowProjectionError> {
    if runtime.scope() == scope {
        Ok(())
    } else {
        Err(StrongFlowProjectionError::PermissionDenied(
            "runtime facts belong to another repository scope".to_owned(),
        ))
    }
}

fn validate_product_session_runtime(
    scope: &RepositoryScope,
    product_session_id: &winwincode_domain::ProductSessionId,
    runtime: &TrustedRuntimeProjectionRead,
) -> Result<(), StrongFlowProjectionError> {
    validate_runtime_scope(scope, runtime)?;
    if runtime.snapshot().sessions.is_empty()
        || runtime
            .snapshot()
            .sessions
            .iter()
            .any(|session| &session.product_session_id != product_session_id)
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "runtime facts do not exactly belong to the requested ProductSession".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn derive_publication_binding(
    delivery: &Delivery,
    detail: &DeliveryProjection,
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
    if delivery.snapshot().status != DeliveryStatus::Delivered {
        return Ok(None);
    }
    let Some(approval) = current_publication_approval(delivery)? else {
        return Ok(None);
    };
    let run = approval.run;
    let target_sha256 = sha256_json(target)?;
    let approval_review_set_sha256 = sha256_json(&ApprovalSeal {
        delivery_id: detail.delivery_id(),
        delivery_revision: detail.delivery_revision(),
        delivery_spec_id: candidate.delivery_spec_id(),
        delivery_spec_revision: candidate.delivery_spec_revision(),
        stage_run: run,
        attention_item_id: &approval.attention.id,
        attention_context: &approval.attention.context,
        attention_resolution: approval.resolution,
        assigned_to: approval.assigned_to,
        resolved_by: approval.resolved_by,
        resolved_at: approval.resolved_at,
        candidate_ref: candidate.candidate_ref(),
        diff_sha256: candidate.diff_sha256(),
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
        approval.attention.id.clone(),
        approval_review_set_sha256,
        target_sha256,
    )
    .map(Some)
    .map_err(current_source_error)
}

fn current_publication_approval(
    delivery: &Delivery,
) -> Result<Option<CurrentPublicationApproval<'_>>, StrongFlowProjectionError> {
    let snapshot = delivery.snapshot();
    let max_attempt = snapshot
        .stage_runs
        .iter()
        .filter(|run| run.stage == DeliveryStage::DeliveryReview)
        .map(|run| run.attempt)
        .max();
    let Some(max_attempt) = max_attempt else {
        return Ok(None);
    };
    let current_runs = snapshot
        .stage_runs
        .iter()
        .filter(|run| run.stage == DeliveryStage::DeliveryReview && run.attempt == max_attempt)
        .collect::<Vec<_>>();
    let [run] = current_runs.as_slice() else {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the current delivery review attempt is ambiguous".to_owned(),
        ));
    };
    if run.delivery_id != snapshot.id
        || run.delivery_task_id.is_some()
        || run.actor_type != StageRunActorType::Human
        || run.role != "approver"
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the current delivery review is not one human approver authority".to_owned(),
        ));
    }
    if run.status != StageRunStatus::Succeeded || run.finished_at_millis.is_none() {
        return Ok(None);
    }
    let approvals = snapshot
        .attention_items
        .iter()
        .filter(|item| {
            item.delivery_id == snapshot.id
                && item.delivery_spec_id == snapshot.spec.id
                && item.stage_run_id.as_ref() == Some(&run.id)
                && item.item_type == AttentionItemType::DeliveryApproval
        })
        .collect::<Vec<_>>();
    let [approval] = approvals.as_slice() else {
        return if approvals.is_empty() {
            Ok(None)
        } else {
            Err(StrongFlowProjectionError::RevisionConflict(
                "the current delivery approval is ambiguous".to_owned(),
            ))
        };
    };
    let (Some(resolution), Some(resolved_by), Some(resolved_at), Some(assigned_to)) = (
        approval.resolution.as_deref(),
        approval.resolved_by.as_deref(),
        approval.resolved_at_millis,
        approval.assigned_to.as_deref(),
    ) else {
        return Ok(None);
    };
    if approval.status != AttentionItemStatus::Resolved
        || !approval.blocking
        || assigned_to != resolved_by
        || approval.created_at_millis != run.started_at_millis
        || resolved_at < approval.created_at_millis
        || run.finished_at_millis != Some(resolved_at)
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the current delivery approval actor or time is not exact".to_owned(),
        ));
    }
    Ok(Some(CurrentPublicationApproval {
        run,
        attention: approval,
        resolution,
        assigned_to,
        resolved_by,
        resolved_at,
    }))
}

fn validate_publication_result(
    publication: &TrustedPublicationProjectionRead,
    expected: Option<&PublicationFactBinding>,
) -> Result<(), StrongFlowProjectionError> {
    match (publication.result(), expected) {
        (Some(result), Some(expected)) if result.binding() == expected => Ok(()),
        (None, _) => Ok(()),
        _ => Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "a sealed current Delivery approval is unavailable".to_owned(),
        )),
    }
}

fn validate_publication_read(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    publication: &TrustedPublicationProjectionRead,
    delivery_revision: u64,
) -> Result<(), StrongFlowProjectionError> {
    if publication.scope() != scope {
        return Err(StrongFlowProjectionError::PermissionDenied(
            "publication facts belong to another repository scope".to_owned(),
        ));
    }
    if publication.delivery_id() == delivery_id
        && publication.delivery_revision() == delivery_revision
    {
        Ok(())
    } else {
        Err(StrongFlowProjectionError::RevisionConflict(
            "publication facts belong to another aggregate revision".to_owned(),
        ))
    }
}

pub(super) fn delivery_event_stream_key(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<ProjectionEventStreamKey, StrongFlowProjectionError> {
    let scope_key = repository_scope_key(scope).map_err(|_| {
        StrongFlowProjectionError::PermissionDenied(
            "a canonical repository scope is required".to_owned(),
        )
    })?;
    ProjectionEventStreamKey::new(
        scope_key,
        ProjectionEventStream::Delivery(delivery_id.clone()),
    )
    .map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "the Delivery event stream identity is invalid".to_owned(),
        )
    })
}

pub(super) fn product_session_event_stream_key(
    scope: &RepositoryScope,
    product_session_id: &winwincode_domain::ProductSessionId,
) -> Result<ProjectionEventStreamKey, StrongFlowProjectionError> {
    let scope_key = repository_scope_key(scope).map_err(|_| {
        StrongFlowProjectionError::PermissionDenied(
            "a canonical repository scope is required".to_owned(),
        )
    })?;
    ProjectionEventStreamKey::new(
        scope_key,
        ProjectionEventStream::ProductSession(product_session_id.clone()),
    )
    .map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "the ProductSession event stream identity is invalid".to_owned(),
        )
    })
}

fn parse_delivery_event_cursor(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    cursor: &DeliveryEventReadCursor,
) -> Result<ProjectionEventCursor, StrongFlowProjectionError> {
    if cursor.scope != *scope
        || cursor.stream.kind != DeliveryEventReadStreamKind::Delivery
        || cursor.stream.delivery_id != *delivery_id
        || cursor.sequence.0 < 0
        || (cursor.sequence.0 == 0) != cursor.event_id.is_none()
    {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "the Delivery event cursor shape or stream identity is invalid".to_owned(),
        ));
    }
    let sequence = u64::try_from(cursor.sequence.0).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "the Delivery event cursor sequence is invalid".to_owned(),
        )
    })?;
    ProjectionEventCursor::try_new(
        delivery_event_stream_key(scope, delivery_id)?,
        sequence,
        cursor.event_id.clone(),
    )
    .map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("the Delivery event cursor is invalid".to_owned())
    })
}

fn generated_delivery_event_cursor(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    cursor: &ProjectionEventCursor,
) -> Result<DeliveryEventReadCursor, StrongFlowProjectionError> {
    if cursor.key() != &delivery_event_stream_key(scope, delivery_id)? {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the durable event cursor belongs to another repository stream".to_owned(),
        ));
    }
    Ok(DeliveryEventReadCursor {
        event_id: cursor.event_id().cloned(),
        scope: scope.clone(),
        sequence: EventReadPosition(i64::try_from(cursor.sequence()).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "the durable event sequence exceeds the public integer range".to_owned(),
            )
        })?),
        stream: DeliveryEventReadStream {
            delivery_id: delivery_id.clone(),
            kind: DeliveryEventReadStreamKind::Delivery,
        },
    })
}

fn generated_product_session_event_cursor(
    scope: &RepositoryScope,
    product_session_id: &winwincode_domain::ProductSessionId,
    cursor: &ProjectionEventCursor,
) -> Result<ProductSessionEventReadCursor, StrongFlowProjectionError> {
    if cursor.key() != &product_session_event_stream_key(scope, product_session_id)? {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "the durable event cursor belongs to another ProductSession stream".to_owned(),
        ));
    }
    Ok(ProductSessionEventReadCursor {
        event_id: cursor.event_id().cloned(),
        scope: scope.clone(),
        sequence: EventReadPosition(i64::try_from(cursor.sequence()).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "the durable event sequence exceeds the public integer range".to_owned(),
            )
        })?),
        stream: ProductSessionEventReadStream {
            kind: ProductSessionEventReadStreamKind::ProductSession,
            product_session_id: product_session_id.clone(),
        },
    })
}

fn current_event_storage_error(_source: crate::StorageError) -> StrongFlowProjectionError {
    StrongFlowProjectionError::ServiceUnavailable(
        "the durable projection event cut is unavailable".to_owned(),
    )
}

fn cursor_event_storage_error(source: crate::StorageError) -> StrongFlowProjectionError {
    match source.kind() {
        StorageErrorKind::EventCursorExpired => StrongFlowProjectionError::ReadCursorExpired(
            "the Delivery event cursor is outside the retained stream window".to_owned(),
        ),
        StorageErrorKind::InvalidInput => StrongFlowProjectionError::RevisionConflict(
            "the Delivery event cursor does not match durable stream history".to_owned(),
        ),
        _ => current_event_storage_error(source),
    }
}

#[allow(clippy::too_many_arguments)]
fn bounded_cursor(
    actor_sha256: &str,
    scope: &RepositoryScope,
    delivery: &Delivery,
    runtime: &TrustedRuntimeProjectionRead,
    publication: &TrustedPublicationProjectionRead,
    event_cursor: &ProjectionEventCursor,
    page_limit: i64,
) -> Result<BoundedReadCursor, StrongFlowProjectionError> {
    let delivery_content_sha256 = sha256_json(delivery)?;
    let runtime_content_sha256 = sha256_json(&RuntimeContentSeal {
        scope: runtime.scope(),
        delivery_revision: runtime.delivery_revision(),
        ledger_revision: runtime.ledger_revision(),
        accepted_sequence: runtime.accepted_sequence(),
        rebuilt_at: runtime.rebuilt_at(),
        snapshot: runtime.snapshot(),
        source_seal: runtime.source_seal(),
    })?;
    let publication_content_sha256 = sha256_json(&PublicationContentSeal {
        scope: publication.scope(),
        delivery_id: publication.delivery_id(),
        delivery_revision: publication.delivery_revision(),
        publication_revision: publication.publication_revision(),
        candidate: publication.candidate(),
        result: publication.result(),
        source_seal: publication.source_seal(),
    })?;
    let event_cursor = generated_delivery_event_cursor(scope, delivery.id(), event_cursor)?;
    let seal = CursorSeal {
        actor_sha256,
        scope,
        delivery_id: delivery.id(),
        delivery_revision: delivery.revision(),
        delivery_content_sha256: &delivery_content_sha256,
        runtime_ledger_revision: runtime.ledger_revision(),
        runtime_accepted_sequence: runtime.accepted_sequence(),
        runtime_source_seal: runtime.source_seal(),
        runtime_content_sha256: &runtime_content_sha256,
        publication_revision: publication.publication_revision(),
        publication_source_seal: publication.source_seal(),
        publication_content_sha256: &publication_content_sha256,
        event_cursor: &event_cursor,
        page_limit,
    };
    let token = OpaqueCursor(format!("sfc1_{}", sha256_json(&seal)?));
    Ok(BoundedReadCursor {
        token,
        scope: scope.clone(),
        delivery_id: delivery.id().clone(),
        delivery_revision: delivery.revision(),
        runtime_ledger_revision: runtime.ledger_revision().clone(),
        runtime_accepted_sequence: runtime.accepted_sequence(),
        publication_revision: publication.publication_revision().clone(),
        event_cursor,
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

pub(super) fn generated_cursor(
    cut: &BoundedReadCursor,
) -> Result<StrongFlowReadCursor, StrongFlowProjectionError> {
    Ok(StrongFlowReadCursor {
        token: cut.token.0.clone(),
        scope: cut.scope.clone(),
        delivery_id: cut.delivery_id.clone(),
        delivery_revision: Revision(i64::try_from(cut.delivery_revision).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "delivery revision exceeds the public integer range".to_owned(),
            )
        })?),
        runtime_ledger_revision: cut.runtime_ledger_revision.clone(),
        runtime_accepted_sequence: i64::try_from(cut.runtime_accepted_sequence).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "runtime sequence exceeds the public integer range".to_owned(),
            )
        })?,
        publication_revision: cut.publication_revision.clone(),
        event_cursor: cut.event_cursor.clone(),
    })
}

pub(super) fn validate_limit(limit: i64) -> Result<usize, StrongFlowProjectionError> {
    usize::try_from(limit)
        .ok()
        .filter(|limit| (1..=MAX_QUERY_LIMIT).contains(limit))
        .ok_or_else(|| {
            StrongFlowProjectionError::InvalidRequest(
                "projection page limit must be between 1 and 200".to_owned(),
            )
        })
}

pub(super) fn validate_scope(scope: &RepositoryScope) -> Result<(), StrongFlowProjectionError> {
    if scope.kind == RepositoryScopeKind::Repository && repository_scope_key(scope).is_ok() {
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

pub(super) fn current_source_error(
    source: TrustedProjectionReadError,
) -> StrongFlowProjectionError {
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
        TrustedProjectionReadError::ExactCutNotRetained => {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted projection facts do not retain a required current source".to_owned(),
            )
        }
        TrustedProjectionReadError::Stale => StrongFlowProjectionError::RevisionConflict(
            "trusted projection facts changed while a current cut was established".to_owned(),
        ),
        TrustedProjectionReadError::Invalid => StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted projection facts are not canonical".to_owned(),
        ),
    }
}

fn cursor_source_error(source: TrustedProjectionReadError) -> StrongFlowProjectionError {
    match source {
        TrustedProjectionReadError::ExactCutNotRetained => {
            StrongFlowProjectionError::ReadCursorExpired(
                "trusted projection facts no longer retain the requested cut".to_owned(),
            )
        }
        other => current_source_error(other),
    }
}

fn canonical_cursor_token(value: &str) -> bool {
    value.strip_prefix("sfc1_").is_some_and(|seal| {
        seal.len() == 64
            && seal
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_delivery::domain::{AttentionItem, StageRun, StageRunActorType, StageRunStatus};
    use winwincode_domain::{AttentionItemId, StageRunId};

    fn delivered_with_exact_approval() -> Delivery {
        let parsed = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture");
        let mut snapshot = parsed.into_snapshot();
        snapshot.status = DeliveryStatus::Delivered;
        snapshot.updated_at_millis = 1_800_000_000_040;
        let run_id = StageRunId("stage-delivery-review-1".into());
        snapshot.stage_runs.push(StageRun {
            schema_version: 3,
            id: run_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage: DeliveryStage::DeliveryReview,
            actor_type: StageRunActorType::Human,
            role: "approver".into(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: Some(1_800_000_000_040),
        });
        snapshot.attention_items.push(AttentionItem {
            schema_version: 3,
            id: AttentionItemId("attention-delivery-approval-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(run_id),
            item_type: AttentionItemType::DeliveryApproval,
            title: "Approve delivery".into(),
            context: "candidate-and-verdict-review-set".into(),
            options: Vec::new(),
            assigned_to: Some("usr_approver".into()),
            blocking: true,
            status: AttentionItemStatus::Resolved,
            resolution: Some("approved".into()),
            resolved_by: Some("usr_approver".into()),
            created_at_millis: 1_800_000_000_030,
            resolved_at_millis: Some(1_800_000_000_040),
        });
        Delivery::try_from_snapshot(snapshot).expect("delivered approval fixture")
    }

    #[test]
    fn publication_approval_requires_one_exact_human_actor_and_time() {
        let valid = delivered_with_exact_approval();
        let approval = current_publication_approval(&valid)
            .expect("valid authority")
            .expect("approval");
        assert_eq!(approval.assigned_to, "usr_approver");
        assert_eq!(approval.resolved_at, 1_800_000_000_040);

        let mut wrong_actor = valid.clone().into_snapshot();
        let review = wrong_actor.stage_runs.last_mut().expect("delivery review");
        review.actor_type = StageRunActorType::Codex;
        review.role = "executor".into();
        let wrong_actor = Delivery::try_from_snapshot(wrong_actor).expect("structurally valid");
        assert!(matches!(
            current_publication_approval(&wrong_actor),
            Err(StrongFlowProjectionError::RevisionConflict(_))
        ));

        let mut wrong_reviewer = valid.clone().into_snapshot();
        wrong_reviewer
            .attention_items
            .last_mut()
            .expect("delivery approval")
            .resolved_by = Some("usr_foreign".into());
        let wrong_reviewer =
            Delivery::try_from_snapshot(wrong_reviewer).expect("structurally valid");
        assert!(matches!(
            current_publication_approval(&wrong_reviewer),
            Err(StrongFlowProjectionError::RevisionConflict(_))
        ));

        let mut wrong_time = valid.into_snapshot();
        wrong_time
            .attention_items
            .last_mut()
            .expect("delivery approval")
            .resolved_at_millis = Some(1_800_000_000_041);
        wrong_time.updated_at_millis = 1_800_000_000_041;
        let wrong_time = Delivery::try_from_snapshot(wrong_time).expect("structurally valid");
        assert!(matches!(
            current_publication_approval(&wrong_time),
            Err(StrongFlowProjectionError::RevisionConflict(_))
        ));

        let valid = delivered_with_exact_approval();
        let mut duplicate = valid.into_snapshot();
        let mut second = duplicate
            .attention_items
            .last()
            .expect("delivery approval")
            .clone();
        second.id = AttentionItemId("attention-delivery-approval-duplicate".into());
        duplicate.attention_items.push(second);
        let duplicate = Delivery::try_from_snapshot(duplicate).expect("structurally valid");
        assert!(matches!(
            current_publication_approval(&duplicate),
            Err(StrongFlowProjectionError::RevisionConflict(_))
        ));
    }
}
