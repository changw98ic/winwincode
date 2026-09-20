// SPDX-License-Identifier: Apache-2.0

//! Exact Evidence detail reads with closed Artifact availability.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest;
use winwincode_api::generated::{
    Actor, DeliveryEvidenceProjection, EvidenceArtifactAccessProjection,
    EvidenceArtifactAvailableProjection, EvidenceArtifactAvailableProjectionState,
    EvidenceArtifactContentChunkProjection, EvidenceArtifactContentChunkProjectionEncoding,
    EvidenceArtifactContentEncoding, EvidenceArtifactContentGetQuery,
    EvidenceArtifactContentGetResultResponse, EvidenceArtifactContentGetResultResponseQuery,
    EvidenceArtifactContentResult, EvidenceArtifactContentUnavailableProjection,
    EvidenceArtifactContentUnavailableProjectionKind,
    EvidenceArtifactContentUnavailableProjectionState, EvidenceArtifactDescriptorProjection,
    EvidenceArtifactKind, EvidenceArtifactPreviewMode, EvidenceArtifactProvenanceProjection,
    EvidenceArtifactUnavailableProjection, EvidenceArtifactUnavailableProjectionState,
    EvidenceDetailProjection, EvidenceDetailProjectionKind, EvidenceGetQuery,
    EvidenceGetResultResponse, EvidenceGetResultResponseQuery, EvidenceOutcome,
    EvidencePageAnnotationProjection, EvidenceReadBinding, PageInfo, QueryResultResponse,
    StrongFlowReadCursor,
};
use winwincode_delivery::domain::{EvidenceRef, VerifiedEvidenceOutcome};
use winwincode_domain::{DeliveryId, EvidenceId, RepositoryScope, SchemaVersion, WorkRunId};
use winwincode_execution_port::generated::ExecutionEventCategory;
use winwincode_storage::{ArtifactAccess, ArtifactProvenance};

use super::{StrongFlowProjectionError, application, mapping};
use crate::{
    ControlPlane, PageAnnotation, PageAnnotationState, repository_scope_key,
    runtime_event_transaction::{decode_runtime_ledger_state, runtime_stream_id_for_projection},
    session_binding_transaction::instant_millis,
    terminal_outcome_transaction::load_successful_terminal_authority,
};

const MAX_ARTIFACT_CHUNK_BYTES: i64 = 256 * 1024;
const MAX_ARTIFACT_BYTES: i64 = 1_099_511_627_776;
const NO_AUTHORITATIVE_LINK: &str = "no_authoritative_link";

struct BoundEvidence {
    cursor: StrongFlowReadCursor,
    evidence: DeliveryEvidenceProjection,
    outcome: EvidenceOutcome,
    artifacts: Vec<BoundArtifact>,
    page_annotation: Option<EvidencePageAnnotationProjection>,
}

struct BoundArtifact {
    descriptor: EvidenceArtifactDescriptorProjection,
    access: ArtifactAccess,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageAnnotationCatalog {
    schema: String,
    scope: RepositoryScope,
    revision: u64,
    claims: BTreeMap<String, serde_json::Value>,
    annotations: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    page_annotations: BTreeMap<String, PageAnnotation>,
}

#[derive(Clone)]
struct EvidenceSelector<'query> {
    delivery_id: &'query DeliveryId,
    at_cursor: &'query StrongFlowReadCursor,
    read_page_limit: i64,
    evidence_id: &'query EvidenceId,
    candidate_ref: &'query str,
    work_run_id: WorkRunId,
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
        &EvidenceSelector {
            delivery_id: &query.parameters.delivery_id,
            at_cursor: &query.parameters.at_cursor,
            read_page_limit: query.parameters.read_page_limit,
            evidence_id: &query.parameters.evidence_id,
            candidate_ref: &query.parameters.candidate_ref,
            work_run_id: query.parameters.work_run_id.clone(),
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
                artifact_access: artifact_access(bound.artifacts),
                evidence: bound.evidence,
                kind: EvidenceDetailProjectionKind::EvidenceDetail,
                outcome: bound.outcome,
                page_annotation: bound.page_annotation,
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
    let bound = resolve_bound_evidence(
        control_plane,
        &query.actor,
        &query.scope,
        &selector(binding),
    )?;

    let Some(artifact) = select_artifact(
        &bound.artifacts,
        &query.parameters.artifact_id,
        &query.parameters.artifact_digest,
        &query.parameters.artifact_kind,
        &query.parameters.artifact_media_type,
        query.parameters.artifact_size_bytes,
    ) else {
        return Ok(unavailable_content_response(
            query,
            bound.cursor,
            bound.evidence.id,
            NO_AUTHORITATIVE_LINK,
        ));
    };
    let object = control_plane
        .artifact_store
        .as_ref()
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted Evidence Artifact authority is unavailable".to_owned(),
            )
        })?
        .read_exact_range(
            &artifact.access,
            u64::try_from(query.parameters.offset).map_err(|_| {
                StrongFlowProjectionError::InvalidRequest(
                    "Evidence Artifact offset is invalid".to_owned(),
                )
            })?,
            u64::try_from(query.parameters.length).map_err(|_| {
                StrongFlowProjectionError::InvalidRequest(
                    "Evidence Artifact length is invalid".to_owned(),
                )
            })?,
        )
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted Evidence Artifact content is unavailable".to_owned(),
            )
        })?;
    let bytes = object.bytes();
    let content_encoding = if std::str::from_utf8(bytes).is_ok() {
        EvidenceArtifactContentEncoding::Utf8
    } else {
        EvidenceArtifactContentEncoding::Binary
    };
    let offset = object.range().offset();
    let next_offset = (offset + u64::try_from(bytes.len()).unwrap_or(0))
        .lt(&object.range().total_size())
        .then(|| i64::try_from(offset + u64::try_from(bytes.len()).unwrap_or(0)).ok())
        .flatten();
    Ok(QueryResultResponse::EvidenceArtifactContentGetResultResponse(
        EvidenceArtifactContentGetResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: EvidenceArtifactContentGetResultResponseQuery::EvidenceArtifactContentGet,
            result: EvidenceArtifactContentResult::EvidenceArtifactContentChunkProjection(
                EvidenceArtifactContentChunkProjection {
                    artifact: artifact.descriptor.clone(),
                    content_encoding,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    encoding: EvidenceArtifactContentChunkProjectionEncoding::Base64,
                    evidence: bound.evidence,
                    kind: winwincode_api::generated::EvidenceArtifactContentChunkProjectionKind::EvidenceArtifactContentChunk,
                    next_offset,
                    offset: query.parameters.offset,
                    preview_mode: artifact.descriptor.preview_mode.clone(),
                    read_cursor: bound.cursor,
                    returned_bytes: i64::try_from(bytes.len()).map_err(|_| StrongFlowProjectionError::Internal("Evidence Artifact range is too large".to_owned()))?,
                    state: winwincode_api::generated::EvidenceArtifactContentChunkProjectionState::Available,
                    total_bytes: artifact.descriptor.size_bytes,
                    truncated: next_offset.is_some(),
                },
            ),
            page: one_page(),
        },
    ))
}

fn select_artifact<'artifacts>(
    artifacts: &'artifacts [BoundArtifact],
    artifact_id: &str,
    digest: &winwincode_domain::Sha256Digest,
    kind: &EvidenceArtifactKind,
    media_type: &str,
    size_bytes: i64,
) -> Option<&'artifacts BoundArtifact> {
    artifacts.iter().find(|artifact| {
        artifact.descriptor.artifact_id == artifact_id
            && artifact.descriptor.digest == *digest
            && artifact.descriptor.kind == *kind
            && artifact.descriptor.media_type == media_type
            && artifact.descriptor.size_bytes == size_bytes
    })
}

fn selector(binding: &EvidenceReadBinding) -> EvidenceSelector<'_> {
    EvidenceSelector {
        delivery_id: &binding.delivery_id,
        at_cursor: &binding.at_cursor,
        read_page_limit: binding.read_page_limit,
        evidence_id: &binding.evidence_id,
        candidate_ref: &binding.candidate_ref,
        work_run_id: WorkRunId(binding.work_run_id.0.clone()),
        session_binding_id: &binding.session_binding_id,
        evidence_type: &binding.type_value,
        source_ref: &binding.source_ref,
    }
}

#[allow(clippy::too_many_lines)]
fn resolve_bound_evidence(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    selector: &EvidenceSelector<'_>,
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

    if evidence.evidence_type == winwincode_delivery::domain::EvidenceRefType::ReviewFinding {
        let (page_annotation, artifacts) =
            resolve_page_annotation_source(control_plane, scope, &delivery, evidence)?;
        return Ok(BoundEvidence {
            cursor,
            evidence: mapping::historical_evidence(evidence)?,
            outcome: EvidenceOutcome::Observed,
            artifacts,
            page_annotation: Some(page_annotation),
        });
    }

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

    let artifacts = resolve_artifacts(control_plane, scope, &delivery, evidence)?;

    Ok(BoundEvidence {
        cursor,
        evidence: mapping::historical_evidence(evidence)?,
        outcome: outcome(resolved.outcome()),
        artifacts,
        page_annotation: None,
    })
}

fn validate_selector(
    evidence: &EvidenceRef,
    selector: &EvidenceSelector<'_>,
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
        || selector.work_run_id.0 != evidence.work_run_id.0
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

#[allow(clippy::too_many_lines)]
fn resolve_artifacts(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery: &winwincode_delivery::domain::Delivery,
    evidence: &EvidenceRef,
) -> Result<Vec<BoundArtifact>, StrongFlowProjectionError> {
    if !matches!(
        evidence.evidence_type,
        winwincode_delivery::domain::EvidenceRefType::Test
            | winwincode_delivery::domain::EvidenceRefType::Command
    ) {
        return Ok(Vec::new());
    }
    let Some(event_id) = evidence.source_ref.strip_prefix("runtime_event:") else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Test or Command Evidence source_ref is not a runtime event".to_owned(),
        ));
    };
    if !is_event_id(event_id) {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Test or Command Evidence runtime event id is invalid".to_owned(),
        ));
    }
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| {
            binding.id == evidence.session_binding_id && binding.work_run_id == evidence.work_run_id
        })
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence runtime binding is missing".to_owned(),
            )
        })?;
    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Evidence storage is unavailable".to_owned(),
        )
    })?;
    let scope_key = repository_scope_key(scope).map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Artifact scope is unavailable".to_owned(),
        )
    })?;
    let stream_id = runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id);
    let Some(stored) = storage.load_state(&stream_id).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime ledger is unavailable".to_owned(),
        )
    })?
    else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime ledger is missing".to_owned(),
        ));
    };
    let ledger = decode_runtime_ledger_state(&stored, &stream_id).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime ledger is not canonical".to_owned(),
        )
    })?;
    let terminal = load_successful_terminal_authority(storage, delivery, &binding.execution_job_id)
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence terminal Artifact authority is unavailable".to_owned(),
            )
        })?;
    validate_runtime_authority(&ledger, delivery, binding, &terminal)?;
    let terminal_sequence =
        u64::try_from(terminal.metadata().last_event_sequence().0).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence terminal runtime sequence is invalid".to_owned(),
            )
        })?;
    let events = ledger
        .events
        .iter()
        .filter(|entry| entry.event.event_id.0 == event_id)
        .collect::<Vec<_>>();
    let [event] = events.as_slice() else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event is missing or ambiguous".to_owned(),
        ));
    };
    let event_sequence = u64::try_from(event.event.sequence.0).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event sequence is invalid".to_owned(),
        )
    })?;
    if event_sequence > terminal_sequence {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event follows the terminal authority".to_owned(),
        ));
    }
    let event_bytes = serde_json::to_vec(&event.event).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event is not canonical".to_owned(),
        )
    })?;
    let event_digest = format!("sha256:{:x}", sha2::Sha256::digest(event_bytes));
    if event.event_digest.0 != event_digest {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event digest changed".to_owned(),
        ));
    }
    let event_occurred_at = instant_millis(&event.event.occurred_at).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event time is invalid".to_owned(),
        )
    })?;
    if event_occurred_at > terminal.metadata().finished_at_millis() {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event follows the terminal outcome".to_owned(),
        ));
    }
    let Some(payload) = event.event.payload.as_ref() else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event has no payload".to_owned(),
        ));
    };
    let expected_category = match evidence.evidence_type {
        winwincode_delivery::domain::EvidenceRefType::Test => ExecutionEventCategory::Test,
        winwincode_delivery::domain::EvidenceRefType::Command => ExecutionEventCategory::Command,
        _ => {
            return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence runtime event category is unsupported".to_owned(),
            ));
        }
    };
    if event.event.category != expected_category {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event category does not match Evidence type".to_owned(),
        ));
    }
    if payload.content_type != "application/json" {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime event payload type is unsupported".to_owned(),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence runtime payload is invalid".to_owned(),
            )
        })?;
    if base64::engine::general_purpose::STANDARD.encode(&bytes) != payload.data_base64 {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime payload is not canonical base64".to_owned(),
        ));
    }
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
    if payload.payload_digest.0 != digest {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime payload digest changed".to_owned(),
        ));
    }
    let stage: StageEvidencePayload = serde_json::from_slice(&bytes).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime payload is not canonical".to_owned(),
        )
    })?;
    if !is_stage_source_id(&stage.source_id)
        || stage.status.trim().is_empty()
        || !is_sha256_digest(&stage.command_digest)
    {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime stage payload identity is invalid".to_owned(),
        ));
    }
    let _ = stage.exit_code;
    let active = terminal.authority().active_lease();
    let provenance = ArtifactProvenance::execution_job(
        active.execution_job_id().clone(),
        active.attempt(),
        active.lease_id().clone(),
        active.fencing_token().clone(),
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.worker_session_id().clone(),
    )
    .map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact provenance is invalid".to_owned(),
        )
    })?;
    let store = control_plane.artifact_store.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted Evidence Artifact authority is unavailable".to_owned(),
        )
    })?;
    let Some(reference) = stage.artifact else {
        return Ok(Vec::new());
    };
    if !is_artifact_id(&reference.artifact_id) || !is_sha256_digest(&reference.digest.0) {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime Artifact reference is invalid".to_owned(),
        ));
    }
    let terminal_matches = terminal
        .metadata()
        .artifacts()
        .iter()
        .filter(|artifact| {
            artifact.artifact_id.0 == reference.artifact_id && artifact.digest == reference.digest
        })
        .count();
    if terminal_matches != 1 {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime Artifact is absent or ambiguous in the terminal outcome".to_owned(),
        ));
    }
    let expected_provenance = provenance.clone();
    let access = ArtifactAccess::new(
        scope_key.clone(),
        winwincode_domain::ArtifactId(reference.artifact_id),
        reference.digest,
        provenance,
    );
    let record = store.describe_exact(&access).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact metadata is unavailable".to_owned(),
        )
    })?;
    if record.provenance() != &expected_provenance {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact provenance changed".to_owned(),
        ));
    }
    let expected_kind = match evidence.evidence_type {
        winwincode_delivery::domain::EvidenceRefType::Test => EvidenceArtifactKind::TestOutput,
        winwincode_delivery::domain::EvidenceRefType::Command => {
            EvidenceArtifactKind::CommandOutput
        }
        _ => unreachable!("Evidence type was checked above"),
    };
    let Some(kind) = evidence_artifact_kind(record.kind()) else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact kind is unsupported".to_owned(),
        ));
    };
    if kind != expected_kind {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact kind does not match Evidence type".to_owned(),
        ));
    }
    let size_bytes = i64::try_from(record.size_bytes()).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence Artifact size is invalid".to_owned(),
        )
    })?;
    Ok(vec![BoundArtifact {
        descriptor: EvidenceArtifactDescriptorProjection {
            artifact_id: record.artifact_id().0.clone(),
            digest: record.digest().clone(),
            file_name: record.file_name().map(str::to_owned),
            kind,
            media_type: record.media_type().to_owned(),
            preview_mode: preview_mode(record.media_type()),
            provenance: EvidenceArtifactProvenanceProjection {
                candidate_ref: evidence.candidate_ref.clone(),
                delivery_id: evidence.delivery_id.clone(),
                delivery_revision: winwincode_domain::Revision(
                    i64::try_from(delivery.revision()).map_err(|_| {
                        StrongFlowProjectionError::TrustedFactsUnavailable(
                            "Evidence Delivery revision is invalid".to_owned(),
                        )
                    })?,
                ),
                evidence_id: mapping::public_evidence_id(&evidence.id),
                session_binding_id: evidence.session_binding_id.0.clone(),
                work_run_id: evidence.work_run_id.clone(),
            },
            size_bytes,
        },
        access,
    }])
}

#[allow(clippy::too_many_lines)]
fn resolve_page_annotation_source(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery: &winwincode_delivery::domain::Delivery,
    evidence: &EvidenceRef,
) -> Result<(EvidencePageAnnotationProjection, Vec<BoundArtifact>), StrongFlowProjectionError> {
    let annotation_id = evidence
        .source_ref
        .strip_prefix("page-annotation:")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding Evidence source is not a page annotation".to_owned(),
            )
        })?;
    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Evidence storage is unavailable".to_owned(),
        )
    })?;
    let stream_id = page_annotation_catalog_stream(scope)?;
    let Some(stored) = storage.load_state(&stream_id).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation source is unavailable".to_owned(),
        )
    })?
    else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation source is unavailable".to_owned(),
        ));
    };
    let value: serde_json::Value = serde_json::from_slice(&stored.payload).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation source is not canonical".to_owned(),
        )
    })?;
    let catalog: PageAnnotationCatalog = serde_json::from_value(value.clone()).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation source is not canonical".to_owned(),
        )
    })?;
    if serde_json::to_value(&catalog).ok().as_ref() != Some(&value)
        || stored.stream_id != stream_id
        || stored.revision != catalog.revision
        || catalog.schema != "winwincode.collaboration-inbox.v1"
        || catalog.scope != *scope
    {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation source is not canonical".to_owned(),
        ));
    }
    let Some(annotation) = catalog.page_annotations.get(annotation_id) else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation is missing".to_owned(),
        ));
    };
    if annotation.id.0 != annotation_id
        || annotation.state != PageAnnotationState::Active
        || annotation.evidence_id != evidence.id
        || annotation.candidate.delivery_id != *delivery.id()
        || annotation.candidate.delivery_spec_id != evidence.delivery_spec_id.0
        || annotation.candidate.delivery_spec_revision != evidence.delivery_spec_revision
        || annotation.candidate.candidate_ref != evidence.candidate_ref
        || annotation.candidate.work_run_id != evidence.work_run_id
        || annotation.candidate.session_binding_id != evidence.session_binding_id.0
        || annotation.body.trim().is_empty()
    {
        return Err(StrongFlowProjectionError::CandidateStale(
            "ReviewFinding page annotation binding is stale or foreign".to_owned(),
        ));
    }
    let target =
        serde_json::from_value(serde_json::to_value(&annotation.target).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding page annotation target is not canonical".to_owned(),
            )
        })?)
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding page annotation target is not canonical".to_owned(),
            )
        })?;
    let screenshot_artifact = serde_json::from_value(
        serde_json::to_value(&annotation.screenshot_artifact).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding page annotation Artifact is not canonical".to_owned(),
            )
        })?,
    )
    .map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact is not canonical".to_owned(),
        )
    })?;
    let page_annotation = EvidencePageAnnotationProjection {
        body: annotation.body.clone(),
        target,
        screenshot_artifact,
    };
    let artifacts = annotation
        .screenshot_artifact
        .as_ref()
        .map(|reference| {
            resolve_page_annotation_artifact_ref(
                control_plane,
                scope,
                delivery,
                evidence,
                reference,
            )
        })
        .transpose()?
        .unwrap_or_default();
    Ok((page_annotation, artifacts))
}

#[allow(clippy::too_many_lines)]
fn resolve_page_annotation_artifact_ref(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery: &winwincode_delivery::domain::Delivery,
    evidence: &EvidenceRef,
    reference: &crate::PageAnnotationArtifactRef,
) -> Result<Vec<BoundArtifact>, StrongFlowProjectionError> {
    if !is_artifact_id(&reference.artifact_id) || !is_sha256_digest(&reference.digest.0) {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact reference is invalid".to_owned(),
        ));
    }
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| {
            binding.id == evidence.session_binding_id && binding.work_run_id == evidence.work_run_id
        })
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding page annotation runtime binding is missing".to_owned(),
            )
        })?;
    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Evidence storage is unavailable".to_owned(),
        )
    })?;
    let terminal = load_successful_terminal_authority(storage, delivery, &binding.execution_job_id)
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "ReviewFinding page annotation Artifact authority is unavailable".to_owned(),
            )
        })?;
    let terminal_matches = terminal
        .metadata()
        .artifacts()
        .iter()
        .filter(|artifact| {
            artifact.artifact_id.0 == reference.artifact_id && artifact.digest == reference.digest
        })
        .count();
    if terminal_matches != 1 {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact is absent or ambiguous".to_owned(),
        ));
    }
    let active = terminal.authority().active_lease();
    let provenance = ArtifactProvenance::execution_job(
        active.execution_job_id().clone(),
        active.attempt(),
        active.lease_id().clone(),
        active.fencing_token().clone(),
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.worker_session_id().clone(),
    )
    .map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact provenance is invalid".to_owned(),
        )
    })?;
    let scope_key = repository_scope_key(scope).map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "canonical Artifact scope is unavailable".to_owned(),
        )
    })?;
    let access = ArtifactAccess::new(
        scope_key,
        winwincode_domain::ArtifactId(reference.artifact_id.clone()),
        reference.digest.clone(),
        provenance.clone(),
    );
    let store = control_plane.artifact_store.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted Evidence Artifact authority is unavailable".to_owned(),
        )
    })?;
    let record = store.describe_exact(&access).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact metadata is unavailable".to_owned(),
        )
    })?;
    if record.provenance() != &provenance {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact provenance changed".to_owned(),
        ));
    }
    if !matches!(
        record.media_type(),
        "image/png" | "image/jpeg" | "image/webp"
    ) {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact media type is unsupported".to_owned(),
        ));
    }
    let kind = evidence_artifact_kind(record.kind()).ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "ReviewFinding page annotation Artifact kind is unsupported".to_owned(),
        )
    })?;
    Ok(vec![BoundArtifact {
        descriptor: EvidenceArtifactDescriptorProjection {
            artifact_id: record.artifact_id().0.clone(),
            digest: record.digest().clone(),
            file_name: record.file_name().map(str::to_owned),
            kind,
            media_type: record.media_type().to_owned(),
            preview_mode: preview_mode(record.media_type()),
            provenance: EvidenceArtifactProvenanceProjection {
                candidate_ref: evidence.candidate_ref.clone(),
                delivery_id: evidence.delivery_id.clone(),
                delivery_revision: winwincode_domain::Revision(
                    i64::try_from(delivery.revision()).map_err(|_| {
                        StrongFlowProjectionError::TrustedFactsUnavailable(
                            "Evidence Delivery revision is invalid".to_owned(),
                        )
                    })?,
                ),
                evidence_id: mapping::public_evidence_id(&evidence.id),
                session_binding_id: evidence.session_binding_id.0.clone(),
                work_run_id: evidence.work_run_id.clone(),
            },
            size_bytes: i64::try_from(record.size_bytes()).map_err(|_| {
                StrongFlowProjectionError::TrustedFactsUnavailable(
                    "ReviewFinding page annotation Artifact size is invalid".to_owned(),
                )
            })?,
        },
        access,
    }])
}

fn page_annotation_catalog_stream(
    scope: &RepositoryScope,
) -> Result<String, StrongFlowProjectionError> {
    let bytes = serde_json::to_vec(scope).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "page annotation scope is not canonical".to_owned(),
        )
    })?;
    Ok(format!(
        "collaboration-inbox:sha256:{:x}",
        sha2::Sha256::digest(bytes)
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StageEvidencePayload {
    source_id: String,
    status: String,
    exit_code: i64,
    command_digest: String,
    artifact: Option<StageArtifactReference>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StageArtifactReference {
    artifact_id: String,
    digest: winwincode_domain::Sha256Digest,
}

fn validate_runtime_authority(
    ledger: &crate::runtime_event_transaction::RuntimeLedgerState,
    delivery: &winwincode_delivery::domain::Delivery,
    binding: &winwincode_delivery::domain::SessionBinding,
    terminal: &winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts,
) -> Result<(), StrongFlowProjectionError> {
    let active = terminal.authority().active_lease();
    let terminal_sequence =
        u64::try_from(terminal.metadata().last_event_sequence().0).map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Evidence terminal runtime sequence is invalid".to_owned(),
            )
        })?;
    let exact = binding.delivery_id == *delivery.id()
        && ledger.delivery_id.as_ref() == Some(delivery.id())
        && ledger.work_item_id.is_none()
        && ledger.work_run_id.as_ref() == Some(&binding.work_run_id)
        && ledger.product_session_id == binding.product_session_id
        && ledger.execution_job_id == binding.execution_job_id
        && binding.worker_session_id.as_ref() == Some(&ledger.worker_session_id)
        && binding.worker_id.as_ref() == Some(&ledger.worker_id)
        && binding.worker_instance_id.as_ref() == Some(&ledger.worker_instance_id)
        && binding.lease_id.as_ref() == Some(&ledger.lease_id)
        && binding.fencing_token.as_ref() == Some(&ledger.fencing_token)
        && binding.attempt == ledger.attempt
        && binding.codex_thread_id.as_ref() == Some(&ledger.codex_thread_id)
        && terminal.work_run_id() == &binding.work_run_id
        && terminal.metadata().codex_thread_id() == Some(&ledger.codex_thread_id)
        && ledger.lease_id == *active.lease_id()
        && ledger.attempt == active.attempt()
        && ledger.fencing_token == *active.fencing_token()
        && ledger.worker_id == *active.worker_id()
        && ledger.worker_instance_id == *active.worker_instance_id()
        && ledger.worker_session_id == *active.worker_session_id()
        && ledger.highest_sequence == terminal_sequence;
    if exact {
        Ok(())
    } else {
        Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Evidence runtime identity is foreign or stale".to_owned(),
        ))
    }
}

fn evidence_artifact_kind(value: &str) -> Option<EvidenceArtifactKind> {
    Some(match value {
        "command_output" => EvidenceArtifactKind::CommandOutput,
        "diff" => EvidenceArtifactKind::Diff,
        "log" => EvidenceArtifactKind::Log,
        "report" | "screenshot" => EvidenceArtifactKind::Report,
        "test_output" => EvidenceArtifactKind::TestOutput,
        _ => return None,
    })
}

fn is_stage_source_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 4096
        && !value
            .bytes()
            .any(|byte| matches!(byte, 0..=8 | 11..=12 | 14..=31 | 127))
}

fn preview_mode(media_type: &str) -> EvidenceArtifactPreviewMode {
    if media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/javascript" | "application/xml"
        )
    {
        EvidenceArtifactPreviewMode::InlineText
    } else {
        EvidenceArtifactPreviewMode::DownloadOnly
    }
}

fn artifact_access(artifacts: Vec<BoundArtifact>) -> EvidenceArtifactAccessProjection {
    if artifacts.is_empty() {
        EvidenceArtifactAccessProjection::EvidenceArtifactUnavailableProjection(
            EvidenceArtifactUnavailableProjection {
                reason: NO_AUTHORITATIVE_LINK.to_owned(),
                state: EvidenceArtifactUnavailableProjectionState::Unavailable,
            },
        )
    } else {
        EvidenceArtifactAccessProjection::EvidenceArtifactAvailableProjection(
            EvidenceArtifactAvailableProjection {
                items: artifacts
                    .into_iter()
                    .map(|artifact| artifact.descriptor)
                    .collect(),
                state: EvidenceArtifactAvailableProjectionState::Available,
            },
        )
    }
}

fn unavailable_content_response(
    query: &EvidenceArtifactContentGetQuery,
    cursor: StrongFlowReadCursor,
    evidence_id: EvidenceId,
    reason: &str,
) -> QueryResultResponse {
    QueryResultResponse::EvidenceArtifactContentGetResultResponse(
        EvidenceArtifactContentGetResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: EvidenceArtifactContentGetResultResponseQuery::EvidenceArtifactContentGet,
            result: EvidenceArtifactContentResult::EvidenceArtifactContentUnavailableProjection(
                EvidenceArtifactContentUnavailableProjection {
                    artifact_id: query.parameters.artifact_id.clone(),
                    evidence_id,
                    kind: EvidenceArtifactContentUnavailableProjectionKind::EvidenceArtifactContentUnavailable,
                    read_cursor: cursor,
                    reason: reason.to_owned(),
                    state: EvidenceArtifactContentUnavailableProjectionState::Unavailable,
                },
            ),
            page: one_page(),
        },
    )
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
        || parameters
            .offset
            .checked_add(parameters.length)
            .is_some_and(|end| end > parameters.artifact_size_bytes)
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
    is_canonical_id(value, "art_")
}

fn is_event_id(value: &str) -> bool {
    is_canonical_id(value, "xevt_")
}

fn is_canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use winwincode_delivery::domain::{Delivery, EvidenceRef, EvidenceRefType};
    use winwincode_domain::{
        ArtifactId, CodexThreadId, DeliveryId, EvidenceId, ExecutionAckSequence, ExecutionEventId,
        ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
        OrganizationId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId,
        Sha256Digest, SystemActorId, UserId, WorkRunId, WorkerId, WorkerInstanceId,
        WorkerSessionId, WorkspaceId,
    };
    use winwincode_execution_port::generated::{
        ArtifactReference, EncodedPayload, ExecutionEventCategory, ExecutionEventRecord,
        ExecutionOutcomeStatus,
    };
    use winwincode_storage::{
        ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
        ArtifactRetention, ArtifactStore, LocalArtifactObjectStore, NewOutboxEvent,
        ProductStateStorage, PublicEventActor, ReceiptIdentity, SqliteStorage, StateCommit,
        StateMutation, receipt_actor_key,
    };

    use super::*;
    use crate::{ControlPlane, EventPublishError, EventPublisher, OutboxEvent};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
    fn test_scope() -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_01J00000000000000000000000".into()),
            workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
            project_id: ProjectId("prj_01J00000000000000000000000".into()),
            repository_id: RepositoryId("rep_01J00000000000000000000000".into()),
        }
    }

    struct NoopPublisher;

    impl EventPublisher for NoopPublisher {
        fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Corruption {
        None,
        PayloadDigest,
        TerminalArtifactList,
        LedgerIdentity,
        MissingPayload,
        WrongPayloadType,
        NoArtifact,
        CategoryMismatch,
        ArtifactStoreDescriptor,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct TerminalAuthorityFixture {
        schema_version: u8,
        delivery_id: DeliveryId,
        work_run_id: WorkRunId,
        job_id: ExecutionJobId,
        attempt: u64,
        lease_id: LeaseId,
        fencing_token: FencingToken,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_session_id: WorkerSessionId,
        issued_at: Instant,
        expires_at: Instant,
        artifacts: Vec<ArtifactReference>,
        codex_thread_id: Option<CodexThreadId>,
        finished_at_millis: u64,
        last_event_sequence: ExecutionAckSequence,
        status: ExecutionOutcomeStatus,
        disposition: TerminalDispositionFixture,
    }

    #[derive(Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum TerminalDispositionFixture {
        Settled { delivery_revision: u64 },
    }

    struct Fixture {
        control_plane: ControlPlane,
        delivery: Delivery,
        scope: RepositoryScope,
        evidence: EvidenceRef,
        artifact_id: ArtifactId,
        digest: Sha256Digest,
        bytes: Vec<u8>,
    }

    // Write the persisted Command/Test stage source directly to exercise the
    // binding seam.
    #[allow(clippy::too_many_lines)]
    fn fixture(corruption: Corruption) -> Fixture {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-evidence-detail-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        let delivery = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical Delivery fixture");
        let binding = delivery
            .snapshot()
            .session_bindings
            .first()
            .expect("fixture binding");
        let scope = test_scope();
        let scope_key = repository_scope_key(&scope).expect("scope key");
        let artifact_id = ArtifactId("art_01J00000000000000000000000".into());
        let bytes = b"authoritative output\n".to_vec();
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
        let provenance = ArtifactProvenance::execution_job(
            binding.execution_job_id.clone(),
            binding.attempt,
            binding.lease_id.clone().expect("lease"),
            binding.fencing_token.clone().expect("fence"),
            binding.worker_id.clone().expect("worker"),
            binding.worker_instance_id.clone().expect("worker instance"),
            binding.worker_session_id.clone().expect("worker session"),
        )
        .expect("Artifact provenance");
        let object_store =
            LocalArtifactObjectStore::open(root.join("objects")).expect("Artifact objects");
        let mut artifacts = ArtifactStore::open(root.join("catalog"), Box::new(object_store))
            .expect("Artifact catalog");
        artifacts
            .open_artifact(ArtifactOpen::new(
                scope_key.clone(),
                ExecutionMessageId("xmsg_01J00000000000000000000000".into()),
                RequestId("req_01J00000000000000000000000".into()),
                artifact_id.clone(),
                "test_output",
                "text/plain",
                digest.clone(),
                bytes.len() as u64,
                Some("verification.txt".into()),
                provenance.clone(),
                ArtifactMeteringAttribution {
                    organization_id: OrganizationId("org_01J00000000000000000000000".into()),
                    workspace_id: WorkspaceId("wsp_01J00000000000000000000000".into()),
                    project_id: ProjectId("prj_01J00000000000000000000000".into()),
                    repository_id: RepositoryId("rep_01J00000000000000000000000".into()),
                    delivery_id: Some(delivery.id().clone()),
                    product_session_id: Some(binding.product_session_id.clone()),
                    user_id: UserId("usr_01J00000000000000000000000".into()),
                },
                ArtifactRetention::Indefinite,
                1_800_000_000_000,
            ))
            .expect("Artifact open");
        artifacts
            .append_chunk(&ArtifactChunk::new(
                scope_key.clone(),
                ExecutionMessageId("xmsg_01J00000000000000000000001".into()),
                artifact_id.clone(),
                provenance,
                1_800_000_000_001,
                1,
                "text/plain",
                digest.clone(),
                bytes.clone(),
                true,
            ))
            .expect("Artifact complete");

        let event_id = ExecutionEventId("xevt_01J00000000000000000000000".into());
        let stage_artifact_id = match corruption {
            Corruption::ArtifactStoreDescriptor => {
                ArtifactId("art_01J00000000000000000000001".into())
            }
            _ => artifact_id.clone(),
        };
        let stage_digest = match corruption {
            Corruption::ArtifactStoreDescriptor => {
                Sha256Digest(format!("sha256:{}", "d".repeat(64)))
            }
            _ => digest.clone(),
        };
        let stage = serde_json::json!({
            "sourceId": "call-fixture-stage",
            "status": "succeeded",
            "exitCode": 0,
            "commandDigest": format!("sha256:{}", "c".repeat(64)),
            "artifact": match corruption {
                Corruption::NoArtifact => None::<serde_json::Value>,
                Corruption::None
                | Corruption::PayloadDigest
                | Corruption::TerminalArtifactList
                | Corruption::LedgerIdentity
                | Corruption::MissingPayload
                | Corruption::WrongPayloadType
                | Corruption::CategoryMismatch
                | Corruption::ArtifactStoreDescriptor => Some(serde_json::json!({
                    "artifactId": stage_artifact_id.0.clone(),
                    "digest": stage_digest.0.clone(),
                })),
            },
        });
        let stage_bytes = serde_json::to_vec(&stage).expect("stage JSON");
        let payload_digest = match corruption {
            Corruption::PayloadDigest => Sha256Digest(format!("sha256:{}", "0".repeat(64))),
            Corruption::None
            | Corruption::TerminalArtifactList
            | Corruption::LedgerIdentity
            | Corruption::MissingPayload
            | Corruption::WrongPayloadType
            | Corruption::CategoryMismatch
            | Corruption::ArtifactStoreDescriptor
            | Corruption::NoArtifact => {
                Sha256Digest(format!("sha256:{:x}", Sha256::digest(&stage_bytes)))
            }
        };
        let payload = match corruption {
            Corruption::MissingPayload => None,
            Corruption::WrongPayloadType => Some(EncodedPayload {
                content_type: "text/plain".into(),
                data_base64: STANDARD.encode(&stage_bytes),
                payload_digest: payload_digest.clone(),
            }),
            Corruption::None
            | Corruption::PayloadDigest
            | Corruption::TerminalArtifactList
            | Corruption::LedgerIdentity
            | Corruption::CategoryMismatch
            | Corruption::ArtifactStoreDescriptor
            | Corruption::NoArtifact => Some(EncodedPayload {
                content_type: "application/json".into(),
                data_base64: STANDARD.encode(&stage_bytes),
                payload_digest,
            }),
        };
        let event = ExecutionEventRecord {
            category: match corruption {
                Corruption::CategoryMismatch => ExecutionEventCategory::Command,
                _ => ExecutionEventCategory::Test,
            },
            event_id: event_id.clone(),
            occurred_at: Instant("2026-08-25T00:00:00.000Z".into()),
            payload,
            sequence: ExecutionSequence(1),
            summary: "verification stage".into(),
        };
        let event_digest = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&event).expect("event JSON"))
        ));
        let ledger = crate::runtime_event_transaction::RuntimeLedgerState {
            schema_version: 1,
            delivery_id: Some(delivery.id().clone()),
            work_item_id: None,
            work_run_id: Some(binding.work_run_id.clone()),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            worker_session_id: match corruption {
                Corruption::LedgerIdentity => {
                    WorkerSessionId("wsn_01J00000000000000000000001".into())
                }
                Corruption::None
                | Corruption::PayloadDigest
                | Corruption::TerminalArtifactList
                | Corruption::MissingPayload
                | Corruption::WrongPayloadType
                | Corruption::CategoryMismatch
                | Corruption::ArtifactStoreDescriptor
                | Corruption::NoArtifact => {
                    binding.worker_session_id.clone().expect("worker session")
                }
            },
            codex_thread_id: binding.codex_thread_id.clone().expect("thread"),
            lease_id: binding.lease_id.clone().expect("lease"),
            attempt: binding.attempt,
            fencing_token: binding.fencing_token.clone().expect("fence"),
            worker_id: binding.worker_id.clone().expect("worker"),
            worker_instance_id: binding.worker_instance_id.clone().expect("worker instance"),
            sequence_offset: 0,
            highest_sequence: 1,
            events: vec![crate::runtime_event_transaction::RuntimeLedgerEvent {
                event,
                event_digest,
            }],
        };
        let terminal_artifacts = match corruption {
            Corruption::TerminalArtifactList => vec![ArtifactReference {
                artifact_id: ArtifactId("art_01J00000000000000000000001".into()),
                digest: Sha256Digest(format!("sha256:{}", "1".repeat(64))),
            }],
            Corruption::None
            | Corruption::PayloadDigest
            | Corruption::LedgerIdentity
            | Corruption::MissingPayload
            | Corruption::WrongPayloadType => {
                vec![ArtifactReference {
                    artifact_id: stage_artifact_id.clone(),
                    digest: stage_digest.clone(),
                }]
            }
            Corruption::NoArtifact => Vec::new(),
            Corruption::CategoryMismatch => vec![ArtifactReference {
                artifact_id: stage_artifact_id.clone(),
                digest: stage_digest.clone(),
            }],
            Corruption::ArtifactStoreDescriptor => vec![ArtifactReference {
                artifact_id: stage_artifact_id,
                digest: stage_digest,
            }],
        };
        let terminal = TerminalAuthorityFixture {
            schema_version: 1,
            delivery_id: delivery.id().clone(),
            work_run_id: binding.work_run_id.clone(),
            job_id: binding.execution_job_id.clone(),
            attempt: binding.attempt,
            lease_id: binding.lease_id.clone().expect("lease"),
            fencing_token: binding.fencing_token.clone().expect("fence"),
            worker_id: binding.worker_id.clone().expect("worker"),
            worker_instance_id: binding.worker_instance_id.clone().expect("worker instance"),
            worker_session_id: binding.worker_session_id.clone().expect("worker session"),
            issued_at: Instant("2026-08-25T00:00:00.000Z".into()),
            expires_at: Instant("2026-08-25T01:00:00.000Z".into()),
            artifacts: terminal_artifacts,
            codex_thread_id: binding.codex_thread_id.clone(),
            finished_at_millis: 1_800_000_000_020,
            last_event_sequence: ExecutionAckSequence(1),
            status: ExecutionOutcomeStatus::Succeeded,
            disposition: TerminalDispositionFixture::Settled {
                delivery_revision: delivery.revision(),
            },
        };
        let runtime_stream =
            runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id);
        let terminal_stream = crate::terminal_outcome_transaction::terminal_authority_stream_id(
            &binding.execution_job_id,
        );
        let actor = PublicEventActor::System {
            id: SystemActorId("sys_01J00000000000000000000000".into()),
        };
        let receipt = ReceiptIdentity::new(
            receipt_actor_key(&actor).expect("receipt actor"),
            scope_key,
            RequestId("req_01J00000000000000000000002".into()),
        )
        .expect("receipt identity");
        let mut storage = SqliteStorage::open(root.join("state")).expect("SQLite storage");
        storage
            .commit(
                &StateCommit::new(
                    receipt,
                    Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                    "evidence-detail-fixture",
                    0,
                    b"{}".to_vec(),
                    vec![NewOutboxEvent::internal(
                        "evt_01J00000000000000000000000",
                        "evidence-detail.fixture",
                        b"{}".to_vec(),
                    )],
                )
                .with_state_mutation(
                    StateMutation::new(
                        runtime_stream,
                        0,
                        serde_json::to_vec(&ledger).expect("runtime JSON"),
                    )
                    .expect("runtime mutation"),
                )
                .with_state_mutation(
                    StateMutation::new(
                        terminal_stream,
                        0,
                        serde_json::to_vec(&terminal).expect("terminal JSON"),
                    )
                    .expect("terminal mutation"),
                ),
            )
            .expect("durable fixture commit");
        let control_plane = ControlPlane::start_with_artifacts(
            Box::new(storage),
            artifacts,
            Box::new(NoopPublisher),
        )
        .expect("Control Plane");
        let evidence = EvidenceRef {
            schema_version: 1,
            id: EvidenceId("evd_01J00000000000000000000000".into()),
            delivery_id: delivery.id().clone(),
            delivery_spec_id: delivery.snapshot().spec.id.clone(),
            delivery_spec_revision: delivery.snapshot().spec.revision,
            work_run_id: binding.work_run_id.clone(),
            session_binding_id: binding.id.clone(),
            candidate_ref: "git-candidate:fixture".into(),
            evidence_type: EvidenceRefType::Test,
            source_ref: format!("runtime_event:{}", event_id.0),
            created_at_millis: 1_800_000_000_020,
        };
        Fixture {
            control_plane,
            delivery,
            scope,
            evidence,
            artifact_id,
            digest,
            bytes,
        }
    }

    #[test]
    fn direct_stage_payload_without_artifact_is_unavailable() {
        let fixture = fixture(Corruption::NoArtifact);
        let bound = resolve_artifacts(
            &fixture.control_plane,
            &fixture.scope,
            &fixture.delivery,
            &fixture.evidence,
        )
        .expect("stage payload without artifact");
        assert!(bound.is_empty());
        assert!(matches!(
            artifact_access(bound),
            EvidenceArtifactAccessProjection::EvidenceArtifactUnavailableProjection(_)
        ));
        fixture.control_plane.shutdown().expect("fixture shutdown");
    }

    #[test]
    fn direct_stage_payload_binds_terminal_artifact_and_reads_exact_range() {
        let fixture = fixture(Corruption::None);
        let binding = fixture
            .delivery
            .snapshot()
            .session_bindings
            .first()
            .expect("fixture binding");
        load_successful_terminal_authority(
            fixture.control_plane.storage_ref().expect("storage"),
            &fixture.delivery,
            &binding.execution_job_id,
        )
        .unwrap_or_else(|error| panic!("terminal fixture: {error:?}"));
        let bound = resolve_artifacts(
            &fixture.control_plane,
            &fixture.scope,
            &fixture.delivery,
            &fixture.evidence,
        )
        .expect("authoritative stage binding");
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].descriptor.artifact_id, fixture.artifact_id.0);
        assert_eq!(bound[0].descriptor.digest, fixture.digest);
        assert_eq!(
            bound[0].descriptor.size_bytes,
            i64::try_from(fixture.bytes.len()).expect("fixture size")
        );
        assert_eq!(bound[0].descriptor.kind, EvidenceArtifactKind::TestOutput);
        let object = fixture
            .control_plane
            .artifact_store
            .as_ref()
            .expect("Artifact store")
            .read_exact_range(&bound[0].access, 0, 13)
            .expect("exact Artifact range");
        assert_eq!(object.bytes(), &fixture.bytes[..13]);

        let descriptor = &bound[0].descriptor;
        assert!(
            select_artifact(
                &bound,
                "art_01J00000000000000000000001",
                &descriptor.digest,
                &descriptor.kind,
                &descriptor.media_type,
                descriptor.size_bytes,
            )
            .is_none()
        );
        assert!(
            select_artifact(
                &bound,
                &descriptor.artifact_id,
                &Sha256Digest(format!("sha256:{}", "0".repeat(64))),
                &descriptor.kind,
                &descriptor.media_type,
                descriptor.size_bytes,
            )
            .is_none()
        );
        assert!(
            select_artifact(
                &bound,
                &descriptor.artifact_id,
                &descriptor.digest,
                &EvidenceArtifactKind::Diff,
                &descriptor.media_type,
                descriptor.size_bytes,
            )
            .is_none()
        );
        assert!(
            select_artifact(
                &bound,
                &descriptor.artifact_id,
                &descriptor.digest,
                &descriptor.kind,
                "application/json",
                descriptor.size_bytes,
            )
            .is_none()
        );
        assert!(
            select_artifact(
                &bound,
                &descriptor.artifact_id,
                &descriptor.digest,
                &descriptor.kind,
                &descriptor.media_type,
                descriptor.size_bytes + 1,
            )
            .is_none()
        );
        fixture.control_plane.shutdown().expect("fixture shutdown");
    }

    #[test]
    fn runtime_payload_digest_and_terminal_artifact_mismatch_fail_closed() {
        for corruption in [
            Corruption::PayloadDigest,
            Corruption::TerminalArtifactList,
            Corruption::LedgerIdentity,
            Corruption::CategoryMismatch,
            Corruption::ArtifactStoreDescriptor,
        ] {
            let fixture = fixture(corruption);
            assert!(
                resolve_artifacts(
                    &fixture.control_plane,
                    &fixture.scope,
                    &fixture.delivery,
                    &fixture.evidence,
                )
                .is_err()
            );
            fixture.control_plane.shutdown().expect("fixture shutdown");
        }
    }

    #[test]
    fn malformed_runtime_source_and_artifact_references_fail_closed() {
        let valid_fixture = fixture(Corruption::None);
        let mut malformed_source = valid_fixture.evidence.clone();
        malformed_source.source_ref = "command-output:local".into();
        assert!(matches!(
            resolve_artifacts(
                &valid_fixture.control_plane,
                &valid_fixture.scope,
                &valid_fixture.delivery,
                &malformed_source,
            ),
            Err(StrongFlowProjectionError::TrustedFactsUnavailable(_))
        ));
        let mut missing_event = valid_fixture.evidence.clone();
        missing_event.source_ref = "runtime_event:xevt_01J00000000000000000000001".into();
        assert!(matches!(
            resolve_artifacts(
                &valid_fixture.control_plane,
                &valid_fixture.scope,
                &valid_fixture.delivery,
                &missing_event,
            ),
            Err(StrongFlowProjectionError::TrustedFactsUnavailable(_))
        ));

        valid_fixture
            .control_plane
            .shutdown()
            .expect("fixture shutdown");

        for corruption in [Corruption::MissingPayload, Corruption::WrongPayloadType] {
            let corrupt_fixture = fixture(corruption);
            assert!(matches!(
                resolve_artifacts(
                    &corrupt_fixture.control_plane,
                    &corrupt_fixture.scope,
                    &corrupt_fixture.delivery,
                    &corrupt_fixture.evidence,
                ),
                Err(StrongFlowProjectionError::TrustedFactsUnavailable(_))
            ));
            corrupt_fixture
                .control_plane
                .shutdown()
                .expect("fixture shutdown");
        }
    }
}
