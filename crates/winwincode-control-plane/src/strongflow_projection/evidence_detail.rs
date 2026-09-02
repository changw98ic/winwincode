// SPDX-License-Identifier: Apache-2.0

//! Exact Evidence detail reads with closed Artifact availability.

use winwincode_api::generated::{
    Actor, DeliveryEvidenceProjection, EvidenceArtifactAccessProjection,
    EvidenceArtifactContentGetQuery, EvidenceArtifactContentGetResultResponse,
    EvidenceArtifactContentGetResultResponseQuery, EvidenceArtifactContentResult,
    EvidenceArtifactContentUnavailableProjection, EvidenceArtifactContentUnavailableProjectionKind,
    EvidenceArtifactContentUnavailableProjectionState, EvidenceArtifactUnavailableProjection,
    EvidenceArtifactUnavailableProjectionState, EvidenceDetailProjection,
    EvidenceDetailProjectionKind, EvidenceGetQuery, EvidenceGetResultResponse,
    EvidenceGetResultResponseQuery, EvidenceOutcome, EvidenceReadBinding, PageInfo,
    QueryResultResponse, RepositoryScope, StrongFlowReadCursor,
};
use winwincode_delivery::domain::{EvidenceRef, VerifiedEvidenceOutcome};
use winwincode_domain::{DeliveryId, EvidenceId, SchemaVersion};

use super::{StrongFlowProjectionError, application, mapping};
use crate::ControlPlane;

const MAX_ARTIFACT_CHUNK_BYTES: i64 = 256 * 1024;
const MAX_ARTIFACT_BYTES: i64 = 1_099_511_627_776;
const NO_AUTHORITATIVE_LINK: &str = "no_authoritative_link";

struct BoundEvidence {
    cursor: StrongFlowReadCursor,
    evidence: DeliveryEvidenceProjection,
    outcome: EvidenceOutcome,
}

#[derive(Clone, Copy)]
struct EvidenceSelector<'query> {
    delivery_id: &'query DeliveryId,
    at_cursor: &'query StrongFlowReadCursor,
    read_page_limit: i64,
    evidence_id: &'query EvidenceId,
    candidate_ref: &'query str,
    stage_run_id: &'query winwincode_domain::StageRunId,
    session_binding_id: &'query str,
    evidence_type: &'query str,
    source_ref: &'query str,
}

pub(super) fn get(
    control_plane: &ControlPlane,
    query: &EvidenceGetQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    application::validate_limit(query.page.limit)?;
    if query.page.cursor.is_some() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Evidence detail does not accept a page cursor".to_owned(),
        ));
    }
    let bound = resolve_bound_evidence(
        control_plane,
        &query.actor,
        &query.scope,
        EvidenceSelector {
            delivery_id: &query.parameters.delivery_id,
            at_cursor: &query.parameters.at_cursor,
            read_page_limit: query.parameters.read_page_limit,
            evidence_id: &query.parameters.evidence_id,
            candidate_ref: &query.parameters.candidate_ref,
            stage_run_id: &query.parameters.stage_run_id,
            session_binding_id: &query.parameters.session_binding_id,
            evidence_type: &query.parameters.type_value,
            source_ref: &query.parameters.source_ref,
        },
    )?;
    Ok(QueryResultResponse::EvidenceGetResultResponse(
        EvidenceGetResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: EvidenceGetResultResponseQuery::EvidenceGet,
            result: EvidenceDetailProjection {
                artifact_access:
                    EvidenceArtifactAccessProjection::EvidenceArtifactUnavailableProjection(
                        EvidenceArtifactUnavailableProjection {
                            reason: NO_AUTHORITATIVE_LINK.to_owned(),
                            state: EvidenceArtifactUnavailableProjectionState::Unavailable,
                        },
                    ),
                evidence: bound.evidence,
                kind: EvidenceDetailProjectionKind::EvidenceDetail,
                outcome: bound.outcome,
                read_cursor: bound.cursor,
            },
            page: one_page(),
        },
    ))
}

pub(super) fn artifact_content_get(
    control_plane: &ControlPlane,
    query: &EvidenceArtifactContentGetQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    application::validate_limit(query.page.limit)?;
    if query.page.cursor.is_some() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Evidence Artifact range reads do not accept a page cursor".to_owned(),
        ));
    }
    validate_artifact_range(query)?;
    let binding = &query.parameters.evidence;
    let bound =
        resolve_bound_evidence(control_plane, &query.actor, &query.scope, selector(binding))?;

    // Artifact ids and digests supplied by a caller are stale selectors only.
    // The producer has not retained an exact Evidence-to-Artifact link, so this
    // path deliberately does not consult Artifact storage or confirm existence.
    Ok(
        QueryResultResponse::EvidenceArtifactContentGetResultResponse(
            EvidenceArtifactContentGetResultResponse {
                schema_version: SchemaVersion::WinwincodeV1,
                request_id: query.request_id.clone(),
                query:
                    EvidenceArtifactContentGetResultResponseQuery::EvidenceArtifactContentGet,
                result:
                    EvidenceArtifactContentResult::EvidenceArtifactContentUnavailableProjection(
                        EvidenceArtifactContentUnavailableProjection {
                            artifact_id: query.parameters.artifact_id.clone(),
                            evidence_id: bound.evidence.id,
                            kind: EvidenceArtifactContentUnavailableProjectionKind::EvidenceArtifactContentUnavailable,
                            read_cursor: bound.cursor,
                            reason: NO_AUTHORITATIVE_LINK.to_owned(),
                            state: EvidenceArtifactContentUnavailableProjectionState::Unavailable,
                        },
                    ),
                page: one_page(),
            },
        ),
    )
}

fn selector(binding: &EvidenceReadBinding) -> EvidenceSelector<'_> {
    EvidenceSelector {
        delivery_id: &binding.delivery_id,
        at_cursor: &binding.at_cursor,
        read_page_limit: binding.read_page_limit,
        evidence_id: &binding.evidence_id,
        candidate_ref: &binding.candidate_ref,
        stage_run_id: &binding.stage_run_id,
        session_binding_id: &binding.session_binding_id,
        evidence_type: &binding.type_value,
        source_ref: &binding.source_ref,
    }
}

fn resolve_bound_evidence(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    selector: EvidenceSelector<'_>,
) -> Result<BoundEvidence, StrongFlowProjectionError> {
    let read = application::replay_delivery_read(
        control_plane,
        actor,
        scope,
        selector.delivery_id,
        selector.at_cursor,
        selector.read_page_limit,
    )?;
    let cursor = mapping::cursor(&read)?;
    if &cursor != selector.at_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Evidence detail and Delivery reads do not share the same cursor".to_owned(),
        ));
    }
    let revision = u64::try_from(cursor.delivery_revision.0).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Delivery revision is outside the durable range".to_owned(),
        )
    })?;
    let delivery = application::load_revision(control_plane, selector.delivery_id, revision)?;
    let matches = delivery
        .snapshot()
        .evidence
        .iter()
        .filter(|evidence| mapping::public_evidence_id(&evidence.id) == *selector.evidence_id)
        .collect::<Vec<_>>();
    let [evidence] = matches.as_slice() else {
        return if matches.is_empty() {
            Err(StrongFlowProjectionError::ResourceNotFound(
                "the requested Evidence was not found at this read cursor".to_owned(),
            ))
        } else {
            Err(StrongFlowProjectionError::TrustedFactsUnavailable(
                "the requested Evidence identity is ambiguous".to_owned(),
            ))
        };
    };
    validate_selector(evidence, selector)?;

    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Evidence storage is unavailable".to_owned(),
        )
    })?;
    let artifacts = control_plane.artifact_store.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted Evidence Artifact authority is unavailable".to_owned(),
        )
    })?;
    let source_resolver = control_plane
        .git_source_resolver
        .as_deref()
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted Evidence source authority is unavailable".to_owned(),
            )
        })?;
    let authority = crate::delivery_verdict_authority::resolve(
        storage,
        artifacts,
        source_resolver,
        scope,
        &delivery,
    )
    .map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "accepted Evidence source facts cannot be reconstructed exactly".to_owned(),
        )
    })?;
    if authority.candidate.candidate_ref() != evidence.candidate_ref {
        return Err(StrongFlowProjectionError::CandidateStale(
            "Evidence Candidate binding is stale".to_owned(),
        ));
    }
    let resolved = authority
        .evidence
        .iter()
        .filter(|resolved| resolved.evidence() == *evidence)
        .collect::<Vec<_>>();
    let [resolved] = resolved.as_slice() else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "accepted Evidence source identity is missing or ambiguous".to_owned(),
        ));
    };

    Ok(BoundEvidence {
        cursor,
        evidence: mapping::historical_evidence(evidence)?,
        outcome: outcome(resolved.outcome()),
    })
}

fn validate_selector(
    evidence: &EvidenceRef,
    selector: EvidenceSelector<'_>,
) -> Result<(), StrongFlowProjectionError> {
    let evidence_type = serde_json::to_value(evidence.evidence_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence type cannot be represented by the public contract".to_owned(),
            )
        })?;
    if &evidence.delivery_id != selector.delivery_id
        || evidence.candidate_ref != selector.candidate_ref
        || &evidence.stage_run_id != selector.stage_run_id
        || evidence.session_binding_id.0 != selector.session_binding_id
        || evidence_type != selector.evidence_type
        || evidence.source_ref != selector.source_ref
    {
        return Err(StrongFlowProjectionError::CandidateStale(
            "Evidence detail binding is stale or foreign".to_owned(),
        ));
    }
    Ok(())
}

fn validate_artifact_range(
    query: &EvidenceArtifactContentGetQuery,
) -> Result<(), StrongFlowProjectionError> {
    let parameters = &query.parameters;
    if !is_artifact_id(&parameters.artifact_id)
        || !is_sha256_digest(&parameters.artifact_digest.0)
        || parameters.length <= 0
        || parameters.length > MAX_ARTIFACT_CHUNK_BYTES
        || parameters.offset < 0
        || parameters.artifact_size_bytes <= 0
        || parameters.artifact_size_bytes > MAX_ARTIFACT_BYTES
        || parameters.offset >= parameters.artifact_size_bytes
        || parameters.offset.checked_add(parameters.length).is_none()
        || parameters.artifact_media_type.len() > 200
        || !parameters.artifact_media_type.contains('/')
        || parameters
            .artifact_media_type
            .chars()
            .any(char::is_whitespace)
    {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Evidence Artifact range is outside the supported bounds".to_owned(),
        ));
    }
    Ok(())
}

fn is_artifact_id(value: &str) -> bool {
    value.strip_prefix("art_").is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

const fn outcome(source: VerifiedEvidenceOutcome) -> EvidenceOutcome {
    match source {
        VerifiedEvidenceOutcome::Observed => EvidenceOutcome::Observed,
        VerifiedEvidenceOutcome::Succeeded => EvidenceOutcome::Succeeded,
        VerifiedEvidenceOutcome::Failed => EvidenceOutcome::Failed,
        VerifiedEvidenceOutcome::TimedOut => EvidenceOutcome::TimedOut,
        VerifiedEvidenceOutcome::PolicyDenied => EvidenceOutcome::PolicyDenied,
        VerifiedEvidenceOutcome::InfrastructureFailed => EvidenceOutcome::InfrastructureFailed,
        VerifiedEvidenceOutcome::Cancelled => EvidenceOutcome::Cancelled,
    }
}

const fn one_page() -> PageInfo {
    PageInfo {
        has_more: false,
        next_cursor: None,
    }
}
