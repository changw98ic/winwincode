// SPDX-License-Identifier: Apache-2.0

//! Exact, bounded Candidate file and diff reads over trusted Artifact/Git facts.

use base64::{
    Engine as _, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CandidateDiffChunkProjection, CandidateDiffChunkProjectionEncoding,
    CandidateDiffChunkProjectionKind, CandidateDiffChunkProjectionMediaType, CandidateDiffGetQuery,
    CandidateDiffGetResultResponse, CandidateDiffGetResultResponseQuery, CandidateFileEncoding,
    CandidateFilePage, CandidateFilePageKind, CandidateFileProjection, CandidateFileStatus,
    CandidateFilesListQuery, CandidateFilesListResultResponse,
    CandidateFilesListResultResponseQuery, PageInfo, QueryResultResponse, RepositoryScope,
};
use winwincode_domain::{Count, OpaqueCursor, SchemaVersion, Sha256Digest};
use winwincode_storage::{
    ArtifactError, ArtifactErrorKind, GitCandidateReviewFile, GitCandidateReviewFileEncoding,
    GitCandidateReviewFileStatus, ValidatedGitCandidateReview, ValidatedGitSourceArtifact,
};

use super::{StrongFlowProjectionError, application, mapping};
use crate::ControlPlane;

const MAX_DIFF_CHUNK_BYTES: usize = 256 * 1024;
const MAX_DIFF_BYTES: usize = 256 * 1024 * 1024;
const CURSOR_VERSION: u8 = 1;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateFilePageBinding<'query> {
    actor: &'query Actor,
    scope: &'query RepositoryScope,
    delivery_id: &'query winwincode_domain::DeliveryId,
    read_cursor: &'query winwincode_api::generated::StrongFlowReadCursor,
    candidate_ref: &'query str,
    candidate_tree_id: &'query str,
    diff_sha256: &'query Sha256Digest,
    statuses: &'query [CandidateFileStatus],
    path_prefix: Option<&'query str>,
    read_page_limit: i64,
    page_limit: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CandidateFilePageCursor {
    version: u8,
    binding_sha256: String,
    next_index: u64,
}

struct CurrentCandidateSource {
    summary: winwincode_api::generated::FrozenCandidateSummaryProjection,
    cursor: winwincode_api::generated::StrongFlowReadCursor,
    source: ValidatedGitSourceArtifact,
}

pub(super) fn files_list(
    control_plane: &ControlPlane,
    query: &CandidateFilesListQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    let limit = application::validate_limit(query.page.limit)?;
    validate_prefix(query.parameters.path_prefix.as_deref())?;
    reject_duplicate_statuses(&query.parameters.statuses)?;
    let current = current_candidate_source(
        control_plane,
        &query.actor,
        &query.scope,
        &query.parameters.delivery_id,
        &query.parameters.at_cursor,
        &query.parameters.candidate_ref,
        &query.parameters.candidate_tree_id,
        &query.parameters.diff_sha256,
        query.parameters.read_page_limit,
    )?;
    let resolver = control_plane
        .git_source_resolver
        .as_deref()
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted Git Candidate review reads are unavailable".to_owned(),
            )
        })?;
    let review = resolver
        .candidate_review(&current.source)
        .map_err(|error| candidate_source_error(&error))?;
    validate_review(&review, &current.summary)?;
    let binding_sha256 = binding_sha256(&CandidateFilePageBinding {
        actor: &query.actor,
        scope: &query.scope,
        delivery_id: &query.parameters.delivery_id,
        read_cursor: &query.parameters.at_cursor,
        candidate_ref: &query.parameters.candidate_ref,
        candidate_tree_id: &query.parameters.candidate_tree_id,
        diff_sha256: &query.parameters.diff_sha256,
        statuses: &query.parameters.statuses,
        path_prefix: query.parameters.path_prefix.as_deref(),
        read_page_limit: query.parameters.read_page_limit,
        page_limit: query.page.limit,
    })?;
    let start = decode_cursor(query.page.cursor.as_ref(), &binding_sha256)?;
    let filtered = review
        .files()
        .iter()
        .filter(|file| matches_filters(file, query))
        .collect::<Vec<_>>();
    if start > filtered.len() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate file page cursor is outside the retained result".to_owned(),
        ));
    }
    let end = start.saturating_add(limit).min(filtered.len());
    let items = filtered[start..end]
        .iter()
        .map(|file| file_projection(file))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = (end < filtered.len())
        .then(|| encode_cursor(&binding_sha256, end))
        .transpose()?;
    Ok(QueryResultResponse::CandidateFilesListResultResponse(
        CandidateFilesListResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: CandidateFilesListResultResponseQuery::CandidateFilesList,
            result: CandidateFilePage {
                candidate: current.summary,
                items,
                kind: CandidateFilePageKind::CandidateFilePage,
                read_cursor: current.cursor,
            },
            page: PageInfo {
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        },
    ))
}

pub(super) fn diff_get(
    control_plane: &ControlPlane,
    query: &CandidateDiffGetQuery,
) -> Result<QueryResultResponse, StrongFlowProjectionError> {
    application::validate_scope(&query.scope)?;
    application::validate_limit(query.page.limit)?;
    if query.page.cursor.is_some() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate diff range reads do not accept a page cursor".to_owned(),
        ));
    }
    validate_path(&query.parameters.path)?;
    let offset = usize::try_from(query.parameters.offset).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "Candidate diff offset is outside the supported range".to_owned(),
        )
    })?;
    let length = usize::try_from(query.parameters.length).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "Candidate diff length is outside the supported range".to_owned(),
        )
    })?;
    if length == 0 || length > MAX_DIFF_CHUNK_BYTES || offset > MAX_DIFF_BYTES {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate diff range is outside the supported bounds".to_owned(),
        ));
    }
    let current = current_candidate_source(
        control_plane,
        &query.actor,
        &query.scope,
        &query.parameters.delivery_id,
        &query.parameters.at_cursor,
        &query.parameters.candidate_ref,
        &query.parameters.candidate_tree_id,
        &query.parameters.diff_sha256,
        query.parameters.read_page_limit,
    )?;
    let resolver = control_plane
        .git_source_resolver
        .as_deref()
        .ok_or_else(|| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "trusted Git Candidate diff reads are unavailable".to_owned(),
            )
        })?;
    let diff = resolver
        .candidate_diff(&current.source, &query.parameters.path)
        .map_err(|error| candidate_source_error(&error))?;
    if diff.bytes().is_empty() || diff.bytes().len() > MAX_DIFF_BYTES {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted Candidate diff bytes are outside the supported bounds".to_owned(),
        ));
    }
    if offset >= diff.bytes().len() {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate diff offset is outside the selected file".to_owned(),
        ));
    }
    let end = offset.saturating_add(length).min(diff.bytes().len());
    let returned = &diff.bytes()[offset..end];
    let next_offset = (end < diff.bytes().len())
        .then(|| public_integer(end, "Candidate diff next offset"))
        .transpose()?;
    Ok(QueryResultResponse::CandidateDiffGetResultResponse(
        CandidateDiffGetResultResponse {
            schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(),
            query: CandidateDiffGetResultResponseQuery::CandidateDiffGet,
            result: CandidateDiffChunkProjection {
                binary: diff.is_binary(),
                candidate: current.summary,
                content_encoding: encoding(diff.encoding()),
                data_base64: STANDARD.encode(returned),
                encoding: CandidateDiffChunkProjectionEncoding::Base64,
                file_diff_sha256: canonical_digest(diff.file_diff_sha256())?,
                kind: CandidateDiffChunkProjectionKind::CandidateDiffChunk,
                media_type: CandidateDiffChunkProjectionMediaType::ApplicationVndWinwincodeGitDiff,
                next_offset,
                offset: public_integer(offset, "Candidate diff offset")?,
                old_path: diff.old_path().map(str::to_owned),
                path: diff.path().to_owned(),
                read_cursor: current.cursor,
                returned_bytes: public_integer(returned.len(), "Candidate diff returned bytes")?,
                status: status(diff.status()),
                total_bytes: public_integer(diff.bytes().len(), "Candidate diff total bytes")?,
            },
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn current_candidate_source(
    control_plane: &ControlPlane,
    actor: &Actor,
    scope: &RepositoryScope,
    delivery_id: &winwincode_domain::DeliveryId,
    at_cursor: &winwincode_api::generated::StrongFlowReadCursor,
    candidate_ref: &str,
    candidate_tree_id: &str,
    diff_sha256: &Sha256Digest,
    page_limit: i64,
) -> Result<CurrentCandidateSource, StrongFlowProjectionError> {
    let read = application::replay_delivery_read(
        control_plane,
        actor,
        scope,
        delivery_id,
        at_cursor,
        page_limit,
    )?;
    let cursor = mapping::cursor(&read)?;
    if &cursor != at_cursor {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Candidate detail and Delivery reads do not share the same cursor".to_owned(),
        ));
    }
    let summary = read
        .detail
        .current_candidate()
        .map(mapping::candidate)
        .transpose()?
        .ok_or_else(|| {
            StrongFlowProjectionError::ResourceNotFound(
                "the current Delivery has no readable Candidate".to_owned(),
            )
        })?;
    if summary.candidate_ref != candidate_ref
        || summary.candidate_tree_id != candidate_tree_id
        || &summary.diff_sha256 != diff_sha256
    {
        return Err(StrongFlowProjectionError::CandidateStale(
            "Candidate detail binding is stale".to_owned(),
        ));
    }
    let delivery = application::load_current(control_plane, delivery_id)?;
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
    let (candidate, source) =
        crate::delivery_verdict_authority::resolve_current_candidate_with_source(
            storage, artifacts, resolver, scope, &delivery,
        )
        .map_err(|_| {
            StrongFlowProjectionError::TrustedFactsUnavailable(
                "current Candidate facts cannot be rebuilt".to_owned(),
            )
        })?
        .ok_or_else(|| {
            StrongFlowProjectionError::ResourceNotFound(
                "the current Delivery has no readable Candidate".to_owned(),
            )
        })?;
    if read.publication.candidate() != Some(&candidate)
        || candidate.candidate_ref() != summary.candidate_ref
        || candidate.candidate_tree_id() != summary.candidate_tree_id
        || canonical_digest(candidate.diff_sha256())? != summary.diff_sha256
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Candidate facts changed while the detail read was rebuilt".to_owned(),
        ));
    }
    Ok(CurrentCandidateSource {
        summary,
        cursor,
        source,
    })
}

fn validate_review(
    review: &ValidatedGitCandidateReview,
    summary: &winwincode_api::generated::FrozenCandidateSummaryProjection,
) -> Result<(), StrongFlowProjectionError> {
    if review.candidate_commit_id() != summary.candidate_commit_id
        || review.candidate_tree_id() != summary.candidate_tree_id
        || canonical_digest(review.diff_sha256())? != summary.diff_sha256
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "Candidate review facts changed while the read was rebuilt".to_owned(),
        ));
    }
    Ok(())
}

fn matches_filters(file: &GitCandidateReviewFile, query: &CandidateFilesListQuery) -> bool {
    let status_matches = query.parameters.statuses.is_empty()
        || query.parameters.statuses.contains(&status(file.status()));
    let prefix_matches = query
        .parameters
        .path_prefix
        .as_deref()
        .is_none_or(|prefix| file.path().starts_with(prefix));
    status_matches && prefix_matches
}

fn file_projection(
    file: &GitCandidateReviewFile,
) -> Result<CandidateFileProjection, StrongFlowProjectionError> {
    Ok(CandidateFileProjection {
        additions: file
            .additions()
            .map(|value| public_count(value, "Candidate additions"))
            .transpose()?,
        binary: file.is_binary(),
        deletions: file
            .deletions()
            .map(|value| public_count(value, "Candidate deletions"))
            .transpose()?,
        encoding: encoding(file.encoding()),
        old_path: file.old_path().map(str::to_owned),
        path: file.path().to_owned(),
        status: status(file.status()),
    })
}

const fn status(value: GitCandidateReviewFileStatus) -> CandidateFileStatus {
    match value {
        GitCandidateReviewFileStatus::Added => CandidateFileStatus::Added,
        GitCandidateReviewFileStatus::Modified => CandidateFileStatus::Modified,
        GitCandidateReviewFileStatus::Deleted => CandidateFileStatus::Deleted,
        GitCandidateReviewFileStatus::Renamed => CandidateFileStatus::Renamed,
        GitCandidateReviewFileStatus::Copied => CandidateFileStatus::Copied,
        GitCandidateReviewFileStatus::TypeChanged => CandidateFileStatus::TypeChanged,
    }
}

const fn encoding(value: GitCandidateReviewFileEncoding) -> CandidateFileEncoding {
    match value {
        GitCandidateReviewFileEncoding::Utf8 => CandidateFileEncoding::Utf8,
        GitCandidateReviewFileEncoding::Binary => CandidateFileEncoding::Binary,
        GitCandidateReviewFileEncoding::Unknown8Bit => CandidateFileEncoding::Unknown8bit,
    }
}

fn reject_duplicate_statuses(
    statuses: &[CandidateFileStatus],
) -> Result<(), StrongFlowProjectionError> {
    if statuses.len() > 6
        || statuses
            .iter()
            .enumerate()
            .any(|(index, status)| statuses[index + 1..].contains(status))
    {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate file status filters are duplicated or over limit".to_owned(),
        ));
    }
    Ok(())
}

fn validate_prefix(prefix: Option<&str>) -> Result<(), StrongFlowProjectionError> {
    if let Some(prefix) = prefix {
        validate_path_shape(prefix, true)?;
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), StrongFlowProjectionError> {
    validate_path_shape(path, false)
}

fn validate_path_shape(
    path: &str,
    allow_trailing_slash: bool,
) -> Result<(), StrongFlowProjectionError> {
    let core = if allow_trailing_slash {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    if core.is_empty()
        || path.len() > 4_096
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || core
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate path is not a normalized repository-relative path".to_owned(),
        ));
    }
    Ok(())
}

fn binding_sha256(
    value: &CandidateFilePageBinding<'_>,
) -> Result<String, StrongFlowProjectionError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        StrongFlowProjectionError::Internal("Candidate page binding cannot be encoded".to_owned())
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn encode_cursor(
    binding_sha256: &str,
    next_index: usize,
) -> Result<OpaqueCursor, StrongFlowProjectionError> {
    let next_index = u64::try_from(next_index).map_err(|_| {
        StrongFlowProjectionError::Internal(
            "Candidate page cursor index exceeds its portable bound".to_owned(),
        )
    })?;
    let bytes = serde_json::to_vec(&CandidateFilePageCursor {
        version: CURSOR_VERSION,
        binding_sha256: binding_sha256.to_owned(),
        next_index,
    })
    .map_err(|_| {
        StrongFlowProjectionError::Internal("Candidate page cursor cannot be encoded".to_owned())
    })?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > 2_048 {
        return Err(StrongFlowProjectionError::Internal(
            "Candidate page cursor exceeds its public bound".to_owned(),
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
        StrongFlowProjectionError::InvalidRequest("Candidate page cursor is invalid".to_owned())
    })?;
    let decoded: CandidateFilePageCursor = serde_json::from_slice(&bytes).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest("Candidate page cursor is invalid".to_owned())
    })?;
    if decoded.version != CURSOR_VERSION || decoded.binding_sha256 != binding_sha256 {
        return Err(StrongFlowProjectionError::InvalidRequest(
            "Candidate page cursor is foreign or stale".to_owned(),
        ));
    }
    usize::try_from(decoded.next_index).map_err(|_| {
        StrongFlowProjectionError::InvalidRequest(
            "Candidate page cursor index is outside the supported range".to_owned(),
        )
    })
}

fn candidate_source_error(error: &ArtifactError) -> StrongFlowProjectionError {
    match error.kind() {
        ArtifactErrorKind::InvalidInput => StrongFlowProjectionError::InvalidRequest(
            "Candidate review request is invalid".to_owned(),
        ),
        ArtifactErrorKind::NotFound => StrongFlowProjectionError::ResourceNotFound(
            "Candidate review source was not found".to_owned(),
        ),
        ArtifactErrorKind::Conflict | ArtifactErrorKind::DigestMismatch => {
            StrongFlowProjectionError::CandidateStale("Candidate review source is stale".to_owned())
        }
        ArtifactErrorKind::PermissionDenied => StrongFlowProjectionError::PermissionDenied(
            "Candidate review source is not authorized".to_owned(),
        ),
        ArtifactErrorKind::SequenceGap
        | ArtifactErrorKind::Incomplete
        | ArtifactErrorKind::Retained
        | ArtifactErrorKind::Corrupt
        | ArtifactErrorKind::Adapter
        | ArtifactErrorKind::Closed => StrongFlowProjectionError::TrustedFactsUnavailable(
            "trusted Candidate review source is unavailable".to_owned(),
        ),
    }
}

fn canonical_digest(value: &str) -> Result<Sha256Digest, StrongFlowProjectionError> {
    let raw = value.strip_prefix("sha256:").unwrap_or(value);
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "Candidate digest is not canonical".to_owned(),
        ));
    }
    Ok(Sha256Digest(format!("sha256:{raw}")))
}

fn public_integer(value: usize, label: &str) -> Result<i64, StrongFlowProjectionError> {
    i64::try_from(value).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(format!(
            "{label} exceeds the public integer range"
        ))
    })
}

fn public_count(value: u64, label: &str) -> Result<Count, StrongFlowProjectionError> {
    i64::try_from(value).map(Count).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(format!(
            "{label} exceeds the public integer range"
        ))
    })
}
