// SPDX-License-Identifier: Apache-2.0

//! Verified append-only Candidate history and display-only historical review.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CandidateAvailability, CandidateHistoricalReviewGetQuery,
    CandidateHistoricalReviewGetResultResponse, CandidateHistoricalReviewGetResultResponseQuery,
    CandidateHistoricalReviewProjection, CandidateHistoricalReviewProjectionKind,
    CandidateHistoryItemProjection, CandidateHistoryListQuery, CandidateHistoryListResultResponse,
    CandidateHistoryListResultResponseQuery, CandidateHistoryPage, CandidateHistoryPageKind,
    PageInfo, QueryResultResponse, StrongFlowReadCursor,
};
use winwincode_delivery::{
    application::candidate_rejection::{CandidateRejectionFact, current_candidate_rejection},
    domain::{Delivery, DeliveryVerdict, EvidenceRef, FrozenDeliveryCandidate},
};
use winwincode_domain::{DeliveryId, OpaqueCursor, RepositoryScope, Revision, SchemaVersion};
use winwincode_storage::{
    CandidateGitPinReceipt, CandidateGitRetentionError, CandidateGitRetentionErrorKind,
    CandidateGitRetentionState,
};

use super::{StrongFlowProjectionError, application, mapping};
use crate::ControlPlane;

const CURSOR_VERSION: u8 = 1;

#[derive(Clone)]
struct CandidateHistoryFact {
    candidate: FrozenDeliveryCandidate,
    availability: CandidateAvailability,
    first_seen_revision: u64,
    last_seen_revision: u64,
    review_revision: Option<u64>,
    evidence: Vec<EvidenceRef>,
    verdict: Option<DeliveryVerdict>,
    is_current_at_read_cursor: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateHistoryPageBinding<'query> {
    actor: &'query Actor,
    scope: &'query RepositoryScope,
    delivery_id: &'query DeliveryId,
    read_cursor: &'query StrongFlowReadCursor,
    history_sha256: &'query str,
    read_page_limit: i64,
    page_limit: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CandidateHistoryPageCursor {
    version: u8,
    binding_sha256: String,
    next_index: u64,
}

pub(super) fn list(
    control_plane: &ControlPlane,
    query: &CandidateHistoryListQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    let limit = application::validate_limit(query.page.limit)?;
    let read = application::replay_delivery_read(
        control_plane,
        &query.actor,
        &query.scope,
        &query.parameters.delivery_id,
        &query.parameters.at_cursor,
        query.parameters.read_page_limit,
    )?;
    let read_cursor = mapping::cursor(&read)?;
    if read_cursor != query.parameters.at_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Candidate history and Delivery reads do not share the same cursor".to_owned(),
        ));
    }
    let mut facts = rebuild_history(
        control_plane,
        &query.scope,
        &query.parameters.delivery_id,
        read_cursor_revision(&read_cursor)?,
        read.detail
            .current_candidate()
            .map(winwincode_delivery::projection::CurrentCandidateProjection::candidate_ref),
    )?;
    facts.sort_by(|left, right| {
        right
            .first_seen_revision
            .cmp(&left.first_seen_revision)
            .then_with(|| {
                left.candidate
                    .candidate_ref()
                    .cmp(right.candidate.candidate_ref())
            })
    });
    let items = facts
        .iter()
        .map(history_item)
        .collect::<Result<Vec<_>, _>>()?;
    let history_sha256 = value_sha256(&items, "Candidate history")?;
    let binding_sha256 = value_sha256(
        &CandidateHistoryPageBinding {
            actor: &query.actor,
            scope: &query.scope,
            delivery_id: &query.parameters.delivery_id,
            read_cursor: &query.parameters.at_cursor,
            history_sha256: &history_sha256,
            read_page_limit: query.parameters.read_page_limit,
            page_limit: query.page.limit,
        },
        "Candidate history page binding",
    )?;
    let start = decode_cursor(query.page.cursor.as_ref(), &binding_sha256)?;
    if start > items.len() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate history page cursor is outside the retained result".to_owned(),
        ));
    }
    let end = start.saturating_add(limit).min(items.len());
    let next_cursor = (end < items.len())
        .then(|| encode_cursor(&binding_sha256, end))
        .transpose()?;
    Ok(QueryResultResponse::CandidateHistoryListResultResponse(
        CandidateHistoryListResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: CandidateHistoryListResultResponseQuery::CandidateList,
            result: CandidateHistoryPage {
                items: items[start..end].to_vec(),
                kind: CandidateHistoryPageKind::CandidateHistoryPage,
                read_cursor,
            },
            page: PageInfo {
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        },
    ))
}

pub(super) fn review_get(
    control_plane: &ControlPlane,
    query: &CandidateHistoricalReviewGetQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    application::validate_limit(query.page.limit)?;
    if query.page.cursor.is_some() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate historical review does not accept a page cursor".to_owned(),
        ));
    }
    let read = application::replay_delivery_read(
        control_plane,
        &query.actor,
        &query.scope,
        &query.parameters.delivery_id,
        &query.parameters.at_cursor,
        query.parameters.read_page_limit,
    )?;
    let read_cursor = mapping::cursor(&read)?;
    if read_cursor != query.parameters.at_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Candidate historical review and Delivery reads do not share the same cursor"
                .to_owned(),
        ));
    }
    let facts = rebuild_history(
        control_plane,
        &query.scope,
        &query.parameters.delivery_id,
        read_cursor_revision(&read_cursor)?,
        read.detail
            .current_candidate()
            .map(winwincode_delivery::projection::CurrentCandidateProjection::candidate_ref),
    )?;
    let selected = facts
        .into_iter()
        .find(|fact| fact.candidate.candidate_ref() == query.parameters.candidate_ref)
        .ok_or_else(|| {
            StrongFlowProjectionError::CandidateStale(
                "historical Candidate binding is stale or foreign".to_owned(),
            )
        })?;
    let candidate = mapping::frozen_candidate(&selected.candidate)?;
    if candidate.candidate_tree_id != query.parameters.candidate_tree_id
        || candidate.diff_sha256 != query.parameters.diff_sha256
    {
        return Err(StrongFlowProjectionError::CandidateStale(
            "historical Candidate binding is stale or changed".to_owned(),
        ));
    }
    let evidence = selected
        .evidence
        .iter()
        .map(mapping::historical_evidence)
        .collect::<Result<Vec<_>, _>>()?;
    if evidence
        .iter()
        .any(|item| item.candidate_id != candidate.candidate_id)
    {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "historical Evidence is not bound to its original Candidate".to_owned(),
        ));
    }
    let verdict = selected
        .verdict
        .as_ref()
        .map(|verdict| mapping::historical_verdict(verdict, &selected.candidate))
        .transpose()?;
    Ok(
        QueryResultResponse::CandidateHistoricalReviewGetResultResponse(
            CandidateHistoricalReviewGetResultResponse {
                schema_version: SchemaVersion::WinwincodeV1,
                request_id: query.request_id.clone(),
                query: CandidateHistoricalReviewGetResultResponseQuery::CandidateReviewGet,
                result: CandidateHistoricalReviewProjection {
                    availability: selected.availability,
                    candidate,
                    current_authorization: false,
                    display_only: true,
                    evidence,
                    first_seen_delivery_revision: revision(
                        selected.first_seen_revision,
                        "historical Candidate first revision",
                    )?,
                    kind: CandidateHistoricalReviewProjectionKind::CandidateHistoricalReview,
                    last_seen_delivery_revision: revision(
                        selected.last_seen_revision,
                        "historical Candidate last revision",
                    )?,
                    read_cursor,
                    review_delivery_revision: selected
                        .review_revision
                        .map(|value| revision(value, "historical Candidate review revision"))
                        .transpose()?,
                    verdict,
                },
                page: PageInfo {
                    has_more: false,
                    next_cursor: None,
                },
            },
        ),
    )
}

// Read only the already verified journal cut. A later Spec may clear its
// current Attention, but the exact refusal remains in this bounded history.
// A read ending before that refusal cannot borrow a future classification.
fn refusals_in_verified_cut(snapshots: &[Delivery]) -> Vec<CandidateRejectionFact> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let fact = current_candidate_rejection(snapshot)?;
            (fact.source_delivery_revision.checked_add(1) == Some(snapshot.revision()))
                .then_some(fact)
        })
        .collect()
}

fn candidate_for_verified_history(
    candidate_join: Result<Option<FrozenDeliveryCandidate>, crate::DeliveryAuthorityError>,
    refusals: &[CandidateRejectionFact],
) -> Result<Option<FrozenDeliveryCandidate>, StrongFlowProjectionError> {
    match candidate_join {
        Ok(candidate) => Ok(candidate),
        Err(error)
            if error
                .candidate_rejection
                .as_deref()
                .is_some_and(|observed| {
                    // The old revision's terminal, Artifact, Git source and sealed
                    // authorization were resolved again. Every coordinate, including
                    // source revision, must equal a refusal retained in this cut.
                    // Generic IO/storage errors and foreign/tampered sources remain errors.
                    refusals.iter().any(|retained| observed == retained)
                }) =>
        {
            Ok(None)
        }
        Err(_) => Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "historical Candidate facts cannot be rebuilt".to_owned(),
        )),
    }
}

fn rebuild_history(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    through_revision: u64,
    current_candidate_ref: Option<&str>,
) -> Result<Vec<CandidateHistoryFact>, StrongFlowProjectionError> {
    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable("canonical storage is unavailable".to_owned())
    })?;
    let artifacts = control_plane.artifact_store.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Candidate Artifact facts are unavailable".to_owned(),
        )
    })?;
    let resolver = control_plane
        .git_source_resolver
        .as_deref()
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Candidate Git facts are unavailable".to_owned(),
            )
        })?;
    let snapshots =
        application::load_history_through(control_plane, delivery_id, through_revision)?;
    let pins = load_retention(control_plane, delivery_id)?;
    let refusals = refusals_in_verified_cut(&snapshots);
    let mut history: Vec<CandidateHistoryFact> = Vec::new();
    for snapshot in snapshots {
        let candidate_join =
            crate::delivery_verdict_authority::resolve_current_candidate_with_source(
                storage, artifacts, resolver, scope, &snapshot,
            )
            .map(|joined| joined.map(|(candidate, _source)| candidate));
        let Some(candidate) = candidate_for_verified_history(candidate_join, &refusals)? else {
            continue;
        };
        let availability = candidate_availability(&candidate, &pins)?;
        let candidate_evidence = snapshot
            .snapshot()
            .evidence
            .iter()
            .filter(|evidence| evidence.candidate_id == *candidate.candidate_id())
            .cloned()
            .collect::<Vec<_>>();
        let candidate_verdict = snapshot
            .snapshot()
            .verdict
            .as_ref()
            .filter(|verdict| verdict.candidate_ref == candidate.candidate_ref())
            .cloned();
        let has_review = !candidate_evidence.is_empty() || candidate_verdict.is_some();
        if let Some(existing) = history
            .iter_mut()
            .find(|item| item.candidate.candidate_ref() == candidate.candidate_ref())
        {
            if existing.candidate != candidate || existing.availability != availability {
                return Err(StrongFlowProjectionError::CandidateStale(
                    "historical Candidate identity or retention changed while rebuilding"
                        .to_owned(),
                ));
            }
            existing.last_seen_revision = snapshot.revision();
            if has_review {
                existing.review_revision = Some(snapshot.revision());
                existing.evidence = candidate_evidence;
                existing.verdict = candidate_verdict;
            }
            continue;
        }
        history.push(CandidateHistoryFact {
            availability,
            first_seen_revision: snapshot.revision(),
            last_seen_revision: snapshot.revision(),
            review_revision: has_review.then_some(snapshot.revision()),
            evidence: candidate_evidence,
            verdict: candidate_verdict,
            is_current_at_read_cursor: current_candidate_ref
                .is_some_and(|current| current == candidate.candidate_ref()),
            candidate,
        });
    }
    Ok(history)
}

fn load_retention(
    control_plane: &ControlPlane,
    delivery_id: &DeliveryId,
) -> Result<Vec<CandidateGitPinReceipt>, StrongFlowProjectionError> {
    control_plane.local_database_path().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Candidate retention requires canonical local storage".to_owned(),
        )
    })?;
    let repository_root = control_plane.git_repository_root.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::TrustedFactsUnavailable(
            "Candidate retention repository root is unavailable".to_owned(),
        )
    })?;
    let storage = control_plane.storage_ref().map_err(|_| {
        StrongFlowProjectionError::ServiceUnavailable(
            "Candidate retention storage is unavailable".to_owned(),
        )
    })?;
    storage
        .read_candidate_git_retention_for_delivery(repository_root, delivery_id)
        .map_err(|error| retention_error(&error))
}

fn candidate_availability(
    candidate: &FrozenDeliveryCandidate,
    pins: &[CandidateGitPinReceipt],
) -> Result<CandidateAvailability, StrongFlowProjectionError> {
    let matching = pins
        .iter()
        .filter(|pin| {
            pin.delivery_id() == candidate.delivery_id()
                && pin.artifact_id().0 == candidate.producer_artifact_ref()
                && pin.artifact_digest() == candidate.producer_artifact_digest()
                && pin.repository_locator() == candidate.repository().locator
                && pin.candidate_commit_id() == candidate.candidate_commit_id()
                && pin.candidate_tree_id() == candidate.candidate_tree_id()
        })
        .collect::<Vec<_>>();
    let [pin] = matching.as_slice() else {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "historical Candidate has no one exact retention receipt".to_owned(),
        ));
    };
    match pin.state() {
        CandidateGitRetentionState::Pinned => Ok(CandidateAvailability::Available),
        CandidateGitRetentionState::Released => Ok(CandidateAvailability::Released),
        CandidateGitRetentionState::PinIntent | CandidateGitRetentionState::ReleaseIntent => {
            Err(StrongFlowProjectionError::TrustedFactsUnavailable(
                "historical Candidate retention is not settled".to_owned(),
            ))
        }
    }
}

fn history_item(
    source: &CandidateHistoryFact,
) -> Result<CandidateHistoryItemProjection, StrongFlowProjectionError> {
    Ok(CandidateHistoryItemProjection {
        availability: source.availability.clone(),
        candidate: mapping::frozen_candidate(&source.candidate)?,
        first_seen_delivery_revision: revision(
            source.first_seen_revision,
            "historical Candidate first revision",
        )?,
        is_current_at_read_cursor: source.is_current_at_read_cursor,
        last_seen_delivery_revision: revision(
            source.last_seen_revision,
            "historical Candidate last revision",
        )?,
        review_delivery_revision: source
            .review_revision
            .map(|value| revision(value, "historical Candidate review revision"))
            .transpose()?,
    })
}

fn retention_error(error: &CandidateGitRetentionError) -> StrongFlowProjectionError {
    match error.kind() {
        CandidateGitRetentionErrorKind::InvalidInput => StrongFlowProjectionError::InvalidRequest(
            "Candidate retention request is invalid".to_owned(),
        ),
        CandidateGitRetentionErrorKind::PermissionDenied => {
            StrongFlowProjectionError::PermissionDenied(
                "Candidate retention repository identity is not authorized".to_owned(),
            )
        }
        CandidateGitRetentionErrorKind::Conflict => StrongFlowProjectionError::CandidateStale(
            "Candidate retention reference is stale or changed".to_owned(),
        ),
        CandidateGitRetentionErrorKind::NotFound | CandidateGitRetentionErrorKind::Corrupt => {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "Candidate retention facts are unavailable".to_owned(),
            )
        }
        CandidateGitRetentionErrorKind::Adapter | CandidateGitRetentionErrorKind::Closed => {
            StrongFlowProjectionError::ServiceUnavailable(
                "Candidate retention service is unavailable".to_owned(),
            )
        }
    }
}

fn read_cursor_revision(cursor: &StrongFlowReadCursor) -> Result<u64, StrongFlowProjectionError> {
    u64::try_from(cursor.delivery_revision.0).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "Candidate history read revision is invalid".to_owned(),
        )
    })
}

fn revision(value: u64, label: &str) -> Result<Revision, StrongFlowProjectionError> {
    i64::try_from(value).map(Revision).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(format!(
            "{label} exceeds the public integer range"
        ))
    })
}

fn value_sha256(value: &impl Serialize, label: &str) -> Result<String, StrongFlowProjectionError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| StrongFlowProjectionError::Internal(format!("{label} cannot be encoded")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn encode_cursor(
    binding_sha256: &str,
    next_index: usize,
) -> Result<OpaqueCursor, StrongFlowProjectionError> {
    let next_index = u64::try_from(next_index).map_err(|_| {
        StrongFlowProjectionError::Internal(
            "Candidate history cursor index exceeds its portable bound".to_owned(),
        )
    })?;
    let bytes = serde_json::to_vec(&CandidateHistoryPageCursor {
        version: CURSOR_VERSION,
        binding_sha256: binding_sha256.to_owned(),
        next_index,
    })
    .map_err(|_| {
        StrongFlowProjectionError::Internal("Candidate history cursor cannot be encoded".to_owned())
    })?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > 2_048 {
        return Err(StrongFlowProjectionError::Internal(
            "Candidate history cursor exceeds its public bound".to_owned(),
        ));
    }
    Ok(OpaqueCursor(encoded))
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    binding_sha256: &str,
) -> Result<usize, StrongFlowProjectionError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let bytes = URL_SAFE_NO_PAD.decode(&cursor.0).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("Candidate history cursor is invalid".to_owned())
    })?;
    let decoded: CandidateHistoryPageCursor = serde_json::from_slice(&bytes).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("Candidate history cursor is invalid".to_owned())
    })?;
    if decoded.version != CURSOR_VERSION || decoded.binding_sha256 != binding_sha256 {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate history cursor is foreign or stale".to_owned(),
        ));
    }
    usize::try_from(decoded.next_index).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "Candidate history cursor index is outside the supported range".to_owned(),
        )
    })
}
