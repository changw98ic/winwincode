// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CandidateAvailability, CandidateDiffGetParameters, CandidateDiffGetQuery,
    CandidateDiffGetQueryQuery, CandidateFileEncoding, CandidateFileStatus,
    CandidateFilesListParameters, CandidateFilesListQuery, CandidateFilesListQueryQuery,
    CandidateHistoricalReviewGetParameters, CandidateHistoricalReviewGetQuery,
    CandidateHistoricalReviewGetQueryQuery, CandidateHistoryListParameters,
    CandidateHistoryListQuery, CandidateHistoryListQueryQuery, DeliveryGetParameters,
    DeliveryGetQuery, DeliveryGetQueryQuery, DeliverySubmitVerdictCommand,
    DeliverySubmitVerdictCommandCommand, DeliverySubmitVerdictPayload,
    EvidenceArtifactAccessProjection, EvidenceArtifactContentGetParameters,
    EvidenceArtifactContentGetQuery, EvidenceArtifactContentGetQueryQuery,
    EvidenceArtifactContentResult, EvidenceArtifactKind, EvidenceGetQuery, EvidenceGetQueryQuery,
    EvidenceOutcome, EvidenceReadBinding, PageRequest, QueryResultResponse, RepositoryScope,
    RepositoryScopeKind, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent,
    strongflow_projection::{StrongFlowProjectionError, StrongFlowProjectionQueryPort},
};
use winwincode_delivery::{
    application::{
        stage::{
            DeliveryTerminalOutcomeFacts, TerminalArtifactReference, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome,
            },
        },
        verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, RepositoryKind, RepositoryRef, SessionBinding, StageRun,
        candidate::freeze_delivery_candidate_from_source,
    },
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, ControlPlaneEventId, DeliveryId, ExecutionAckSequence, ExecutionEventId,
    ExecutionMessageId, ExecutionSequence, Instant, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, SystemActorId, UserId,
    WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactReference, EncodedPayload, ExecutionEventCategory, ExecutionEventRecord,
    ExecutionOutcomeStatus,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactAccess,
    ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateGitPinReceipt, CandidateGitReleaseAuthority,
    CandidateGitTerminalOutcome, CandidateSourceManifest, LocalArtifactObjectStore,
    LocalGitSourceResolver, NewOutboxEvent, ProductStateStorage, ProjectionEventStream,
    PublicEventActor, PublicEventScope, PublicEventSource, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, SqliteStorage, StateCommit, StateMutation, receipt_actor_key,
    receipt_scope_key,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";

#[derive(Clone, Copy)]
enum RuntimeFixture {
    Valid,
    StaleCandidate,
    NonJsonEvidence,
    LaterWriterFailed,
    AmbiguousWriter,
    AmbiguousVerification,
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_file_and_diff_queries_are_exact_bounded_and_secret_safe() {
    let seeded = seed_verdict("candidate-review-read", RuntimeFixture::Valid);
    let host = start(&seeded);
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", 77)),
        kind: UserActorKind::User,
    });
    let delivery_query = DeliveryGetQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: DeliveryGetParameters {
            at_cursor: None,
            delivery_id: seeded.delivery.id().clone(),
        },
        query: DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId(canonical_id("req", 920)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::DeliveryGetResultResponse(delivery_response) =
        host.delivery_get(&delivery_query).expect("Delivery detail")
    else {
        panic!("delivery.get returned another response kind");
    };
    let candidate = delivery_response
        .result
        .current_candidate
        .expect("current Candidate");
    let cursor = delivery_response.result.read_cursor;
    let files_query = CandidateFilesListQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CandidateFilesListParameters {
            at_cursor: cursor.clone(),
            candidate_ref: candidate.candidate_ref.clone(),
            candidate_tree_id: candidate.candidate_tree_id.clone(),
            delivery_id: seeded.delivery.id().clone(),
            diff_sha256: candidate.diff_sha256.clone(),
            path_prefix: Some("src/".to_owned()),
            read_page_limit: 20,
            statuses: Vec::new(),
        },
        query: CandidateFilesListQueryQuery::CandidateFilesList,
        request_id: RequestId(canonical_id("req", 921)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::CandidateFilesListResultResponse(first_page) = host
        .candidate_files_list(&files_query)
        .expect("first Candidate file page")
    else {
        panic!("candidate.files.list returned another response kind");
    };
    assert_eq!(first_page.result.items.len(), 1);
    assert_eq!(
        first_page.result.items[0].encoding,
        CandidateFileEncoding::Utf8
    );
    assert!(first_page.page.has_more);
    assert_eq!(first_page.result.read_cursor, cursor);
    let mut second_query = files_query.clone();
    second_query.page.cursor = first_page.page.next_cursor;
    second_query.request_id = RequestId(canonical_id("req", 922));
    let QueryResultResponse::CandidateFilesListResultResponse(second_page) = host
        .candidate_files_list(&second_query)
        .expect("second Candidate file page")
    else {
        panic!("candidate.files.list returned another response kind");
    };
    assert_eq!(second_page.result.items.len(), 1);
    assert!(!second_page.page.has_more);
    let paths = [
        first_page.result.items[0].path.as_str(),
        second_page.result.items[0].path.as_str(),
    ];
    assert!(paths.contains(&"src/extra.rs"));
    assert!(paths.contains(&"src/lib.rs"));
    let mut changed_filter = second_query;
    changed_filter.parameters.path_prefix = Some("tests/".to_owned());
    assert!(matches!(
        host.candidate_files_list(&changed_filter),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));

    let diff_query = CandidateDiffGetQuery {
        actor,
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CandidateDiffGetParameters {
            at_cursor: cursor,
            candidate_ref: candidate.candidate_ref.clone(),
            candidate_tree_id: candidate.candidate_tree_id.clone(),
            delivery_id: seeded.delivery.id().clone(),
            diff_sha256: candidate.diff_sha256.clone(),
            length: 32,
            offset: 0,
            path: "src/lib.rs".to_owned(),
            read_page_limit: 20,
        },
        query: CandidateDiffGetQueryQuery::CandidateDiffGet,
        request_id: RequestId(canonical_id("req", 923)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::CandidateDiffGetResultResponse(diff) = host
        .candidate_diff_get(&diff_query)
        .expect("bounded Candidate diff")
    else {
        panic!("candidate.diff.get returned another response kind");
    };
    assert_eq!(diff.result.path, "src/lib.rs");
    assert_eq!(diff.result.status, CandidateFileStatus::Modified);
    assert_eq!(diff.result.content_encoding, CandidateFileEncoding::Utf8);
    assert_eq!(diff.result.returned_bytes, 32);
    assert!(diff.result.next_offset.is_some());
    assert!(!diff.result.data_base64.contains("candidate()"));

    let mut stale = diff_query.clone();
    stale.parameters.diff_sha256 = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    assert!(matches!(
        host.candidate_diff_get(&stale),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    let mut traversal = diff_query.clone();
    traversal.parameters.path = "../secret".to_owned();
    assert!(matches!(
        host.candidate_diff_get(&traversal),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));
    let mut foreign_scope = diff_query.clone();
    foreign_scope.scope.repository_id = RepositoryId(canonical_id("rep", 88));
    assert!(matches!(
        host.candidate_diff_get(&foreign_scope),
        Err(StrongFlowProjectionError::PermissionDenied(_))
    ));
    let mut over_limit = diff_query;
    over_limit.parameters.length = 262_145;
    assert!(matches!(
        host.candidate_diff_get(&over_limit),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));

    host.shutdown().expect("Candidate review shutdown");
    cleanup(seeded);
}

#[test]
#[allow(clippy::too_many_lines)]
fn evidence_detail_rebuilds_outcome_and_artifact_access_stays_closed() {
    let seeded = seed_verdict("evidence-detail-read", RuntimeFixture::Valid);
    let mut host = start(&seeded);
    host.delivery_submit_verdict(&verdict_command(&seeded, 924))
        .expect("persist accepted Evidence");
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", 924)),
        kind: UserActorKind::User,
    });
    let QueryResultResponse::DeliveryGetResultResponse(delivery) = host
        .delivery_get(&DeliveryGetQuery {
            actor: actor.clone(),
            page: PageRequest {
                cursor: None,
                limit: 20,
            },
            parameters: DeliveryGetParameters {
                at_cursor: None,
                delivery_id: seeded.delivery.id().clone(),
            },
            query: DeliveryGetQueryQuery::DeliveryGet,
            request_id: RequestId(canonical_id("req", 924)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: seeded.scope.clone(),
        })
        .expect("Delivery Evidence cut")
    else {
        panic!("delivery.get returned another response kind");
    };
    let evidence = delivery
        .result
        .evidence
        .first()
        .expect("accepted Evidence")
        .clone();
    let cursor = delivery.result.read_cursor;
    let query = EvidenceGetQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: EvidenceReadBinding {
            at_cursor: cursor.clone(),
            candidate_ref: evidence.candidate_ref.clone(),
            delivery_id: seeded.delivery.id().clone(),
            evidence_id: evidence.id.clone(),
            read_page_limit: 20,
            session_binding_id: evidence.session_binding_id.clone(),
            source_ref: evidence.source_ref.clone(),
            stage_run_id: evidence.stage_run_id.clone(),
            type_value: evidence.type_value.clone(),
        },
        query: EvidenceGetQueryQuery::EvidenceGet,
        request_id: RequestId(canonical_id("req", 925)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::EvidenceGetResultResponse(detail) =
        host.evidence_get(&query).expect("exact Evidence detail")
    else {
        panic!("evidence.get returned another response kind");
    };
    assert_eq!(detail.result.evidence, evidence);
    assert_eq!(detail.result.read_cursor, cursor);
    assert!(detail.result.evidence.id.0.starts_with("evd_"));
    assert_eq!(detail.result.evidence.id.0.len(), 30);
    assert!(
        !serde_json::to_string(&detail)
            .expect("serialize Evidence detail")
            .contains("evidence:sha256:")
    );
    assert!(matches!(
        detail.result.outcome,
        EvidenceOutcome::Observed | EvidenceOutcome::Succeeded
    ));
    let EvidenceArtifactAccessProjection::EvidenceArtifactUnavailableProjection(access) =
        detail.result.artifact_access
    else {
        panic!("unlinked Evidence exposed Artifact access");
    };
    assert_eq!(access.reason, "no_authoritative_link");

    let content_query = EvidenceArtifactContentGetQuery {
        actor,
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: EvidenceArtifactContentGetParameters {
            artifact_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            artifact_id: canonical_id("art", 925),
            artifact_kind: EvidenceArtifactKind::Log,
            artifact_media_type: "text/plain".to_owned(),
            artifact_size_bytes: 1_000_000,
            evidence: EvidenceReadBinding {
                at_cursor: cursor,
                candidate_ref: evidence.candidate_ref.clone(),
                delivery_id: seeded.delivery.id().clone(),
                evidence_id: evidence.id.clone(),
                read_page_limit: 20,
                session_binding_id: evidence.session_binding_id.clone(),
                source_ref: evidence.source_ref.clone(),
                stage_run_id: evidence.stage_run_id.clone(),
                type_value: evidence.type_value.clone(),
            },
            length: 262_144,
            offset: 0,
        },
        query: EvidenceArtifactContentGetQueryQuery::EvidenceArtifactContentGet,
        request_id: RequestId(canonical_id("req", 926)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::EvidenceArtifactContentGetResultResponse(content) = host
        .evidence_artifact_content_get(&content_query)
        .expect("closed Evidence Artifact content")
    else {
        panic!("evidence.artifact.content.get returned another response kind");
    };
    let EvidenceArtifactContentResult::EvidenceArtifactContentUnavailableProjection(content) =
        content.result
    else {
        panic!("unlinked Evidence returned Artifact bytes");
    };
    assert_eq!(content.reason, "no_authoritative_link");
    let public_json = serde_json::to_string(&content).expect("serialize safe response");
    for forbidden in [
        "leaseId",
        "fencingToken",
        "workerId",
        "storeKey",
        "objectKey",
        "path",
        "locator",
        "credential",
    ] {
        assert!(!public_json.contains(forbidden), "leaked {forbidden}");
    }

    let mut stale = query.clone();
    stale.parameters.stage_run_id = winwincode_domain::StageRunId(canonical_id("run", 999));
    assert!(matches!(
        host.evidence_get(&stale),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    let mut stale_candidate = query.clone();
    stale_candidate.parameters.candidate_ref = format!("git-candidate:sha256:{}", "0".repeat(64));
    assert!(matches!(
        host.evidence_get(&stale_candidate),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    let mut stale_session = query.clone();
    stale_session.parameters.session_binding_id = canonical_id("sbn", 999);
    assert!(matches!(
        host.evidence_get(&stale_session),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    let mut stale_source = query.clone();
    stale_source.parameters.source_ref = "foreign:runtime:source".to_owned();
    assert!(matches!(
        host.evidence_get(&stale_source),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    let mut unknown = query.clone();
    unknown.parameters.evidence_id = winwincode_domain::EvidenceId(canonical_id("evd", 999));
    assert!(matches!(
        host.evidence_get(&unknown),
        Err(StrongFlowProjectionError::ResourceNotFound(_))
    ));
    let mut foreign = query.clone();
    foreign.scope.repository_id = RepositoryId(canonical_id("rep", 999));
    assert!(matches!(
        host.evidence_get(&foreign),
        Err(StrongFlowProjectionError::PermissionDenied(_))
    ));
    let mut oversized = content_query.clone();
    oversized.parameters.length = 262_145;
    assert!(matches!(
        host.evidence_artifact_content_get(&oversized),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));
    let mut noncanonical_artifact = content_query.clone();
    noncanonical_artifact.parameters.artifact_id = "../../private/object".to_owned();
    assert!(matches!(
        host.evidence_artifact_content_get(&noncanonical_artifact),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));
    let mut invalid_digest = content_query.clone();
    invalid_digest.parameters.artifact_digest = Sha256Digest("sha256:UPPERCASE".to_owned());
    assert!(matches!(
        host.evidence_artifact_content_get(&invalid_digest),
        Err(StrongFlowProjectionError::InvalidRequest(_))
    ));

    host.shutdown().expect("Evidence detail first shutdown");
    let restarted = start(&seeded);
    let QueryResultResponse::EvidenceGetResultResponse(replayed) = restarted
        .evidence_get(&query)
        .expect("restart exact Evidence detail")
    else {
        panic!("restart evidence.get returned another response kind");
    };
    assert_eq!(replayed.result.evidence, evidence);
    restarted
        .shutdown()
        .expect("Evidence detail replay shutdown");
    cleanup(seeded);
}

#[test]
#[allow(clippy::too_many_lines)]
fn candidate_history_replays_original_review_and_retention_without_authorizing_it() {
    let seeded = seed_verdict("candidate-history-read", RuntimeFixture::Valid);
    let mut host = start(&seeded);
    host.delivery_submit_verdict(&verdict_command(&seeded, 930))
        .expect("persist Candidate review facts");
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", 931)),
        kind: UserActorKind::User,
    });
    let QueryResultResponse::DeliveryGetResultResponse(delivery) = host
        .delivery_get(&DeliveryGetQuery {
            actor: actor.clone(),
            page: PageRequest {
                cursor: None,
                limit: 20,
            },
            parameters: DeliveryGetParameters {
                at_cursor: None,
                delivery_id: seeded.delivery.id().clone(),
            },
            query: DeliveryGetQueryQuery::DeliveryGet,
            request_id: RequestId(canonical_id("req", 931)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: seeded.scope.clone(),
        })
        .expect("Delivery review cut")
    else {
        panic!("delivery.get returned another response kind");
    };
    let cursor = delivery.result.read_cursor;
    let candidate = delivery
        .result
        .current_candidate
        .expect("reviewed Candidate");
    let history_query = CandidateHistoryListQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: CandidateHistoryListParameters {
            at_cursor: cursor.clone(),
            delivery_id: seeded.delivery.id().clone(),
            read_page_limit: 20,
        },
        query: CandidateHistoryListQueryQuery::CandidateList,
        request_id: RequestId(canonical_id("req", 932)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::CandidateHistoryListResultResponse(history) = host
        .candidate_history_list(&history_query)
        .expect("Candidate history")
    else {
        panic!("candidate.list returned another response kind");
    };
    assert_eq!(history.result.items.len(), 1);
    let item = &history.result.items[0];
    assert_eq!(item.candidate, candidate);
    assert_eq!(item.availability, CandidateAvailability::Available);
    assert!(item.is_current_at_read_cursor);
    assert_eq!(item.first_seen_delivery_revision, Revision(1));
    assert_eq!(item.review_delivery_revision, Some(Revision(2)));

    let review_query = CandidateHistoricalReviewGetQuery {
        actor: actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CandidateHistoricalReviewGetParameters {
            at_cursor: cursor,
            candidate_ref: candidate.candidate_ref.clone(),
            candidate_tree_id: candidate.candidate_tree_id.clone(),
            delivery_id: seeded.delivery.id().clone(),
            diff_sha256: candidate.diff_sha256.clone(),
            read_page_limit: 20,
        },
        query: CandidateHistoricalReviewGetQueryQuery::CandidateReviewGet,
        request_id: RequestId(canonical_id("req", 933)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    };
    let QueryResultResponse::CandidateHistoricalReviewGetResultResponse(review) = host
        .candidate_historical_review_get(&review_query)
        .expect("historical Candidate review")
    else {
        panic!("candidate.review.get returned another response kind");
    };
    assert!(review.result.display_only);
    assert!(!review.result.current_authorization);
    assert!(!review.result.evidence.is_empty());
    assert!(review.result.verdict.is_some());
    assert!(
        review
            .result
            .evidence
            .iter()
            .all(|evidence| evidence.candidate_ref == candidate.candidate_ref)
    );

    let mut foreign = history_query.clone();
    foreign.scope.repository_id = RepositoryId(canonical_id("rep", 999));
    assert!(matches!(
        host.candidate_history_list(&foreign),
        Err(StrongFlowProjectionError::PermissionDenied(_))
    ));
    let mut stale = review_query.clone();
    stale.parameters.diff_sha256 = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    assert!(matches!(
        host.candidate_historical_review_get(&stale),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));

    let pin = load_candidate_pin(&seeded);
    let base = git_text(&seeded.repository, &["rev-parse", "HEAD~1"]);
    git(
        &seeded.repository,
        &["update-ref", pin.reference_name(), &base],
    );
    assert!(matches!(
        host.candidate_history_list(&history_query),
        Err(StrongFlowProjectionError::CandidateStale(_))
    ));
    git(
        &seeded.repository,
        &[
            "update-ref",
            pin.reference_name(),
            pin.candidate_commit_id(),
        ],
    );

    let moved_repository = seeded.root.join("repository-moved");
    fs::rename(&seeded.repository, &moved_repository).expect("move repository");
    assert!(host.candidate_history_list(&history_query).is_err());
    fs::rename(&moved_repository, &seeded.repository).expect("restore repository");

    host.shutdown().expect("history first shutdown");
    let restarted = start(&seeded);
    let QueryResultResponse::CandidateHistoryListResultResponse(replayed) = restarted
        .candidate_history_list(&history_query)
        .expect("restart Candidate history replay")
    else {
        panic!("candidate.list replay returned another response kind");
    };
    assert_eq!(
        replayed.result.items[0].availability,
        CandidateAvailability::Available
    );

    release_candidate_pin(&seeded);
    let QueryResultResponse::CandidateHistoryListResultResponse(released) = restarted
        .candidate_history_list(&history_query)
        .expect("released Candidate history")
    else {
        panic!("released candidate.list returned another response kind");
    };
    assert_eq!(
        released.result.items[0].availability,
        CandidateAvailability::Released
    );
    let QueryResultResponse::CandidateHistoricalReviewGetResultResponse(released_review) =
        restarted
            .candidate_historical_review_get(&review_query)
            .expect("released Candidate review")
    else {
        panic!("released candidate.review.get returned another response kind");
    };
    assert_eq!(
        released_review.result.availability,
        CandidateAvailability::Released
    );
    assert!(released_review.result.display_only);
    assert!(!released_review.result.current_authorization);
    restarted.shutdown().expect("history replay shutdown");
    cleanup(seeded);
}

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(Default)]
struct CapturingJournal {
    publication: std::sync::Mutex<Option<AtomicPublication>>,
}

impl DeliveryJournalPort for CapturingJournal {
    fn load(
        &self,
        _delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        Ok(None)
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        *self.publication.lock().expect("publication lock") = Some(publication);
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedCatalogEntry<'entry> {
    schema_version: u8,
    repository_scope: &'entry RepositoryScope,
    delivery_id: &'entry DeliveryId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedTerminalAuthority<'authority> {
    schema_version: u8,
    delivery_id: &'authority DeliveryId,
    stage_run_id: &'authority winwincode_domain::StageRunId,
    job_id: &'authority winwincode_domain::ExecutionJobId,
    attempt: u64,
    lease_id: &'authority winwincode_domain::LeaseId,
    fencing_token: &'authority winwincode_domain::FencingToken,
    worker_id: &'authority winwincode_domain::WorkerId,
    worker_instance_id: &'authority winwincode_domain::WorkerInstanceId,
    worker_session_id: &'authority winwincode_domain::WorkerSessionId,
    issued_at: Instant,
    expires_at: Instant,
    artifacts: Vec<ArtifactReference>,
    codex_thread_id: &'authority Option<winwincode_domain::CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: ExecutionOutcomeStatus,
    disposition: SeedTerminalDisposition,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SeedTerminalDisposition {
    Settled { delivery_revision: u64 },
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedRuntimeLedger<'ledger> {
    schema_version: u8,
    delivery_id: Option<&'ledger DeliveryId>,
    delivery_task_id: Option<&'ledger winwincode_domain::DeliveryTaskId>,
    stage_run_id: Option<&'ledger winwincode_domain::StageRunId>,
    product_session_id: &'ledger ProductSessionId,
    execution_job_id: &'ledger winwincode_domain::ExecutionJobId,
    worker_session_id: &'ledger winwincode_domain::WorkerSessionId,
    codex_thread_id: &'ledger winwincode_domain::CodexThreadId,
    lease_id: &'ledger winwincode_domain::LeaseId,
    attempt: u64,
    fencing_token: &'ledger winwincode_domain::FencingToken,
    worker_id: &'ledger winwincode_domain::WorkerId,
    worker_instance_id: &'ledger winwincode_domain::WorkerInstanceId,
    highest_sequence: u64,
    events: Vec<SeedRuntimeLedgerEvent>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedRuntimeLedgerEvent {
    event: ExecutionEventRecord,
    event_digest: Sha256Digest,
}

struct SeededVerdict {
    root: PathBuf,
    data: PathBuf,
    repository: PathBuf,
    scope: RepositoryScope,
    delivery: Delivery,
    candidate_digest: Sha256Digest,
}

#[test]
#[allow(clippy::too_many_lines)]
fn production_adapter_joins_durable_facts_replays_and_rejects_stale_sources() {
    let valid = seed_verdict("valid", RuntimeFixture::Valid);
    let command = verdict_command(&valid, 900);
    let mut first = start(&valid);
    let completed = first
        .delivery_submit_verdict(&command)
        .expect("production verdict");
    assert_eq!(completed.previous_revision, command.expected_revision);
    assert_eq!(
        completed.current_revision.0,
        command.expected_revision.0 + 1
    );
    first.shutdown().expect("first shutdown");

    let mut restarted = start(&valid);
    let replay = restarted
        .delivery_submit_verdict(&command)
        .expect("restart replay");
    assert_eq!(replay, completed);
    restarted.shutdown().expect("replay shutdown");

    let mut stale_command = verdict_command(&valid, 901);
    stale_command.expected_revision = completed.current_revision;
    stale_command.payload.candidate_digest = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    let mut stale_host = start(&valid);
    assert_eq!(
        stale_host
            .delivery_submit_verdict(&stale_command)
            .expect_err("caller stale-check digest must not replace durable candidate")
            .code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
    stale_host.shutdown().expect("stale shutdown");
    cleanup(valid);

    for (label, fixture) in [
        ("stale-runtime", RuntimeFixture::StaleCandidate),
        ("non-json-evidence", RuntimeFixture::NonJsonEvidence),
        ("later-writer-failed", RuntimeFixture::LaterWriterFailed),
        ("ambiguous-writer", RuntimeFixture::AmbiguousWriter),
        (
            "ambiguous-verification",
            RuntimeFixture::AmbiguousVerification,
        ),
    ] {
        let seeded = seed_verdict(label, fixture);
        let command = verdict_command(&seeded, 910);
        let before_revision = delivery_state_revision(&seeded.data);
        let mut host = start(&seeded);
        assert_eq!(
            host.delivery_submit_verdict(&command)
                .expect_err("stale runtime or Evidence must fail closed")
                .code(),
            winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
        );
        host.shutdown().expect("negative shutdown");
        assert_eq!(delivery_state_revision(&seeded.data), before_revision);
        cleanup(seeded);
    }
}

fn seed_verdict(label: &str, runtime_fixture: RuntimeFixture) -> SeededVerdict {
    let root = unique_root(label);
    let data = root.join("data");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical repository");
    let scope = repository_scope(77);
    let delivery = fixture_delivery(&repository, base_commit, runtime_fixture);
    fs::create_dir_all(&data).expect("data directory");
    seed_delivery(&data, &scope, &delivery);
    let candidate_digest = seed_verdict_sources(
        &data,
        &repository,
        &scope,
        &delivery,
        &candidate_commit,
        runtime_fixture,
    );
    SeededVerdict {
        root,
        data,
        repository,
        scope,
        delivery,
        candidate_digest,
    }
}

fn fixture_delivery(
    repository: &Path,
    base_commit: String,
    runtime_fixture: RuntimeFixture,
) -> Delivery {
    let fixture = verdict_fixture(
        &DeliveryId(canonical_id("dlv", 77)),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: repository
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("portable repository locator")
            .to_owned(),
    };
    snapshot.spec.base_revision = base_commit;
    match runtime_fixture {
        RuntimeFixture::LaterWriterFailed => append_failed_writer(&mut snapshot),
        RuntimeFixture::AmbiguousWriter => append_ambiguous_writer(&mut snapshot),
        RuntimeFixture::AmbiguousVerification => append_ambiguous_verifier(&mut snapshot),
        RuntimeFixture::Valid
        | RuntimeFixture::StaleCandidate
        | RuntimeFixture::NonJsonEvidence => {}
    }
    for (index, binding) in snapshot.session_bindings.iter_mut().enumerate() {
        let seed = format!("production-{index}");
        binding.execution_job_id =
            winwincode_domain::ExecutionJobId(canonical_id("job", 1_000 + index as u64));
        binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
            "wsn",
            1_000 + index as u64,
        )));
        binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id(
            "cdx",
            1_000 + index as u64,
        )));
        *binding = binding.clone().with_test_authority(&seed, binding.attempt);
        binding.worker_id = Some(winwincode_domain::WorkerId(canonical_id(
            "wrk",
            1_000 + index as u64,
        )));
        binding.worker_instance_id = Some(winwincode_domain::WorkerInstanceId(canonical_id(
            "wki",
            1_000 + index as u64,
        )));
        binding.lease_id = Some(winwincode_domain::LeaseId(canonical_id(
            "lse",
            1_000 + index as u64,
        )));
        binding.fencing_token = Some(winwincode_domain::FencingToken(
            (1_000 + index as u64).to_string(),
        ));
    }
    Delivery::try_from_snapshot(snapshot).expect("production verdict Delivery")
}

fn seed_verdict_sources(
    data: &Path,
    repository: &Path,
    scope: &RepositoryScope,
    delivery: &Delivery,
    candidate_commit: &str,
    runtime_fixture: RuntimeFixture,
) -> Sha256Digest {
    let object_store =
        LocalArtifactObjectStore::open(data.join("artifacts")).expect("local Artifact objects");
    let mut artifacts = ArtifactStore::open(data.join("artifact-catalog"), Box::new(object_store))
        .expect("Artifact catalog");
    let source_resolver =
        LocalGitSourceResolver::open(repository.parent().expect("repository parent"))
            .expect("Git resolver");
    let scope_key = repository_scope_key(scope);
    let mut storage = SqliteStorage::open(data).expect("durable storage");
    let mut writer_candidate = None;
    let current_candidate_ref;

    let writer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.role == "executor")
        .expect("writer");
    let (writer_terminal, writer_source) = seed_terminal_and_artifact(
        &mut storage,
        &mut artifacts,
        &source_resolver,
        &scope_key,
        delivery,
        writer,
        candidate_commit,
        1_100,
    );
    pin_writer_candidate(
        &mut storage,
        &mut artifacts,
        repository,
        &scope_key,
        &writer_terminal,
        &writer_source,
    );
    if matches!(
        runtime_fixture,
        RuntimeFixture::LaterWriterFailed | RuntimeFixture::AmbiguousWriter
    ) {
        current_candidate_ref = "git-candidate:latest-writer-failed".to_owned();
    } else {
        let frozen =
            freeze_delivery_candidate_from_source(delivery, &writer_source, &writer_terminal)
                .expect("frozen production candidate");
        current_candidate_ref = frozen.candidate_ref().to_owned();
        writer_candidate.replace(frozen);
    }
    seed_runtime(
        &mut storage,
        &scope_key,
        delivery,
        writer,
        &current_candidate_ref,
        "executor",
        runtime_fixture,
        1_290,
    );

    for (index, role) in ["reviewer", "verifier"].into_iter().enumerate() {
        let run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.role == role)
            .expect("verification run");
        seed_terminal_and_artifact(
            &mut storage,
            &mut artifacts,
            &source_resolver,
            &scope_key,
            delivery,
            run,
            candidate_commit,
            1_200 + index as u64,
        );
        seed_runtime(
            &mut storage,
            &scope_key,
            delivery,
            run,
            &current_candidate_ref,
            role,
            runtime_fixture,
            1_300 + index as u64 * 10,
        );
    }
    Box::new(storage).close().expect("storage close");
    artifacts.close().expect("Artifact close");
    writer_candidate.map_or_else(
        || Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        |candidate| {
            Sha256Digest(
                candidate
                    .candidate_ref()
                    .strip_prefix("git-candidate:")
                    .expect("candidate prefix")
                    .to_owned(),
            )
        },
    )
}

fn pin_writer_candidate(
    storage: &mut SqliteStorage,
    artifacts: &mut ArtifactStore,
    repository: &Path,
    scope: &ReceiptScopeKey,
    terminal: &DeliveryTerminalOutcomeFacts,
    source: &winwincode_storage::ValidatedGitSourceArtifact,
) {
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
    .expect("writer Artifact provenance");
    let receipt = artifacts
        .complete_write_receipt(scope, source.artifact().artifact_id(), &provenance)
        .expect("writer Artifact receipt");
    storage
        .git_candidate_retention(repository.parent().expect("repository root"))
        .expect("candidate retention")
        .pin_after_final_artifact_ack(&receipt, source, &digest(61_100))
        .expect("candidate pin");
}

fn append_failed_writer(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let mut run = snapshot
        .stage_runs
        .iter()
        .find(|run| run.role == "executor")
        .expect("executor")
        .clone();
    let source_run_id = run.id.clone();
    run.id = winwincode_domain::StageRunId(canonical_id("run", 8_001));
    run.stage = winwincode_delivery::domain::DeliveryStage::Reworking;
    run.role = "remediator".into();
    run.status = winwincode_delivery::domain::StageRunStatus::Failed;
    run.attempt = run.attempt.saturating_add(1);
    run.started_at_millis = snapshot
        .stage_runs
        .iter()
        .map(|current| current.started_at_millis)
        .max()
        .expect("latest run")
        .saturating_add(10);
    run.finished_at_millis = Some(run.started_at_millis.saturating_add(1));
    let mut binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == source_run_id)
        .expect("executor binding")
        .clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_001));
    binding.stage_run_id = run.id.clone();
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_001));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_001));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_001,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_001)));
    binding.attempt = run.attempt;
    binding.bound_at_millis = run.started_at_millis.saturating_add(1);
    snapshot.updated_at_millis = binding.bound_at_millis;
    snapshot.stage_runs.push(run);
    snapshot.session_bindings.push(binding);
}

fn append_ambiguous_verifier(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let mut run = snapshot
        .stage_runs
        .iter()
        .find(|run| run.role == "verifier")
        .expect("verifier")
        .clone();
    let source_run_id = run.id.clone();
    run.id = winwincode_domain::StageRunId(canonical_id("run", 8_002));
    let mut binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == source_run_id)
        .expect("verifier binding")
        .clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_002));
    binding.stage_run_id = run.id.clone();
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_002));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_002));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_002,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_002)));
    snapshot.stage_runs.push(run);
    snapshot.session_bindings.push(binding);
}

fn append_ambiguous_writer(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let mut run = snapshot
        .stage_runs
        .iter()
        .find(|run| run.role == "executor")
        .expect("executor")
        .clone();
    let source_run_id = run.id.clone();
    run.id = winwincode_domain::StageRunId(canonical_id("run", 8_003));
    let mut binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == source_run_id)
        .expect("executor binding")
        .clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_003));
    binding.stage_run_id = run.id.clone();
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_003));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_003));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_003,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_003)));
    snapshot.stage_runs.push(run);
    snapshot.session_bindings.push(binding);
}

#[allow(clippy::too_many_arguments)]
fn seed_terminal_and_artifact(
    storage: &mut SqliteStorage,
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    run: &StageRun,
    candidate_commit: &str,
    seed: u64,
) -> (
    DeliveryTerminalOutcomeFacts,
    winwincode_storage::ValidatedGitSourceArtifact,
) {
    let binding = exact_binding(delivery, run);
    let worker_session = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread = binding.codex_thread_id.clone().expect("CodexThread");
    let lease_id = binding.lease_id.clone().expect("lease");
    let fencing_token = binding.fencing_token.clone().expect("fence");
    let worker_id = binding.worker_id.clone().expect("Worker");
    let worker_instance = binding.worker_instance_id.clone().expect("WorkerInstance");
    let finished_at = run.finished_at_millis.expect("finished run");
    let SeededCandidateArtifact {
        artifact_id,
        digest,
        source,
    } = seed_candidate_artifact(
        artifacts,
        resolver,
        scope,
        delivery,
        binding,
        candidate_commit,
        seed,
        finished_at,
    );
    let terminal = delivery_terminal_outcome_facts(
        session_binding_authority(
            active_lease_identity(
                binding.execution_job_id.clone(),
                binding.attempt,
                lease_id.clone(),
                fencing_token.clone(),
                worker_id.clone(),
                worker_instance.clone(),
                worker_session.clone(),
            ),
            Instant("2026-08-25T00:00:00.000Z".into()),
            Instant("2026-08-25T01:00:00.000Z".into()),
        ),
        terminal_worker_outcome(
            run.id.clone(),
            binding.execution_job_id.clone(),
            binding.attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance,
            worker_session,
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                Some(codex_thread),
                finished_at,
                ExecutionAckSequence(4),
                vec![TerminalArtifactReference {
                    artifact_id: artifact_id.clone(),
                    digest: digest.clone(),
                }],
            ),
        ),
    );
    let persisted = SeedTerminalAuthority {
        schema_version: 1,
        delivery_id: delivery.id(),
        stage_run_id: &run.id,
        job_id: &binding.execution_job_id,
        attempt: binding.attempt,
        lease_id: binding.lease_id.as_ref().expect("lease"),
        fencing_token: binding.fencing_token.as_ref().expect("fence"),
        worker_id: binding.worker_id.as_ref().expect("Worker"),
        worker_instance_id: binding.worker_instance_id.as_ref().expect("WorkerInstance"),
        worker_session_id: binding.worker_session_id.as_ref().expect("WorkerSession"),
        issued_at: Instant("2026-08-25T00:00:00.000Z".into()),
        expires_at: Instant("2026-08-25T01:00:00.000Z".into()),
        artifacts: vec![ArtifactReference {
            artifact_id,
            digest,
        }],
        codex_thread_id: &binding.codex_thread_id,
        finished_at_millis: finished_at,
        last_event_sequence: ExecutionAckSequence(4),
        status: ExecutionOutcomeStatus::Succeeded,
        disposition: SeedTerminalDisposition::Settled {
            delivery_revision: delivery.revision(),
        },
    };
    write_state_once(
        storage,
        format!("delivery-terminal-authority:{}", binding.execution_job_id.0),
        serde_json::to_vec(&persisted).expect("terminal JSON"),
        seed + 20_000,
    );
    (terminal, source)
}

struct SeededCandidateArtifact {
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    source: winwincode_storage::ValidatedGitSourceArtifact,
}

#[allow(clippy::too_many_arguments)]
fn seed_candidate_artifact(
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    binding: &SessionBinding,
    candidate_commit: &str,
    seed: u64,
    finished_at: u64,
) -> SeededCandidateArtifact {
    let provenance = ArtifactProvenance::execution_job(
        binding.execution_job_id.clone(),
        binding.attempt,
        binding.lease_id.clone().expect("lease"),
        binding.fencing_token.clone().expect("fence"),
        binding.worker_id.clone().expect("Worker"),
        binding.worker_instance_id.clone().expect("WorkerInstance"),
        binding.worker_session_id.clone().expect("WorkerSession"),
    )
    .expect("Artifact provenance");
    let artifact_id = ArtifactId(canonical_id("art", seed));
    let bytes = CandidateSourceManifest::new(candidate_commit.to_owned())
        .expect("candidate manifest")
        .encode()
        .expect("manifest encode");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2)),
            RequestId(canonical_id("req", seed * 2)),
            artifact_id.clone(),
            "candidate",
            CANDIDATE_MEDIA_TYPE,
            digest.clone(),
            bytes.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: OrganizationId("org_00000000000000000000000091".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000091".into()),
                project_id: ProjectId("prj_00000000000000000000000091".into()),
                repository_id: RepositoryId("rep_00000000000000000000000091".into()),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(ProductSessionId(canonical_id("psn", seed))),
                user_id: UserId("usr_00000000000000000000000091".into()),
            },
            ArtifactRetention::Indefinite,
            finished_at.saturating_sub(1),
        ))
        .expect("Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2 + 1)),
            artifact_id.clone(),
            provenance.clone(),
            finished_at,
            1,
            "application/octet-stream",
            digest.clone(),
            bytes,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope.clone(),
            artifact_id.clone(),
            digest.clone(),
            provenance,
        ))
        .expect("Artifact read");
    let source = resolver
        .resolve_candidate(
            &object,
            &delivery.snapshot().spec.repository.locator,
            &delivery.snapshot().spec.base_revision,
        )
        .expect("source resolution");
    SeededCandidateArtifact {
        artifact_id,
        digest,
        source,
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_runtime(
    storage: &mut SqliteStorage,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    run: &StageRun,
    candidate_ref: &str,
    role: &str,
    fixture: RuntimeFixture,
    seed: u64,
) {
    let binding = exact_binding(delivery, run);
    let runtime_candidate = match fixture {
        RuntimeFixture::StaleCandidate => "git-candidate:stale-runtime",
        RuntimeFixture::Valid
        | RuntimeFixture::NonJsonEvidence
        | RuntimeFixture::LaterWriterFailed
        | RuntimeFixture::AmbiguousWriter
        | RuntimeFixture::AmbiguousVerification => candidate_ref,
    };
    let cited_event = match fixture {
        RuntimeFixture::NonJsonEvidence => format!("event-{role}-binary"),
        RuntimeFixture::Valid
        | RuntimeFixture::StaleCandidate
        | RuntimeFixture::LaterWriterFailed
        | RuntimeFixture::AmbiguousWriter
        | RuntimeFixture::AmbiguousVerification => format!("event-{role}-source"),
    };
    let events = runtime_events(
        run,
        runtime_candidate,
        &delivery.snapshot().spec.id.0,
        delivery.snapshot().spec.revision,
        &delivery.snapshot().spec.acceptance_criteria[0].id.0,
        role,
        &cited_event,
    );
    let stream = runtime_stream_id(scope, &binding.execution_job_id);
    for highest in 1..=events.len() {
        let ledger = SeedRuntimeLedger {
            schema_version: 1,
            delivery_id: Some(delivery.id()),
            delivery_task_id: run.delivery_task_id.as_ref(),
            stage_run_id: Some(&run.id),
            product_session_id: &binding.product_session_id,
            execution_job_id: &binding.execution_job_id,
            worker_session_id: binding.worker_session_id.as_ref().expect("WorkerSession"),
            codex_thread_id: binding.codex_thread_id.as_ref().expect("CodexThread"),
            lease_id: binding.lease_id.as_ref().expect("lease"),
            attempt: binding.attempt,
            fencing_token: binding.fencing_token.as_ref().expect("fence"),
            worker_id: binding.worker_id.as_ref().expect("Worker"),
            worker_instance_id: binding.worker_instance_id.as_ref().expect("WorkerInstance"),
            highest_sequence: highest as u64,
            events: events[..highest].to_vec(),
        };
        write_state_revision(
            storage,
            stream.clone(),
            highest as u64 - 1,
            serde_json::to_vec(&ledger).expect("runtime JSON"),
            seed + highest as u64,
        );
    }
}

fn runtime_events(
    run: &StageRun,
    candidate_ref: &str,
    delivery_spec_id: &str,
    delivery_spec_revision: u64,
    criterion_id: &str,
    role: &str,
    cited_event: &str,
) -> Vec<SeedRuntimeLedgerEvent> {
    let source_id = ExecutionEventId(format!("event-{role}-source"));
    let binary = b"\0verification-binary\xff";
    let payloads = [
        (
            ExecutionEventCategory::Lifecycle,
            ExecutionEventId(format!("event-{role}-policy")),
            encoded_json(&json!({
                "protocol": "winwincode.verification-session-policy.v1",
                "workspace_mode": "candidate-read-only",
                "permission_profile": "candidate-read-only-restricted",
                "candidate_ref": candidate_ref,
            })),
        ),
        (
            ExecutionEventCategory::Activity,
            ExecutionEventId(format!("event-{role}-binary")),
            EncodedPayload {
                content_type: "application/octet-stream".into(),
                data_base64: STANDARD.encode(binary),
                payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(binary))),
            },
        ),
        (
            if role == "reviewer" {
                ExecutionEventCategory::Command
            } else {
                ExecutionEventCategory::Test
            },
            source_id,
            encoded_json(&json!({"status": "completed", "exit_code": 0})),
        ),
        (
            ExecutionEventCategory::Activity,
            ExecutionEventId(format!("event-{role}-result")),
            encoded_json(&json!({
                "protocol": "winwincode.independent-verification-result.v1",
                "delivery_spec_id": delivery_spec_id,
                "delivery_spec_revision": delivery_spec_revision,
                "candidate_ref": candidate_ref,
                "findings": [{
                    "finding_id": format!("finding-{role}"),
                    "criterion_id": criterion_id,
                    "verdict": "pass",
                    "explanation": format!("{role} accepted the current candidate"),
                    "evidence_sources": [{
                        "type": if role == "reviewer" { "command" } else { "test" },
                        "event_id": cited_event,
                    }],
                }],
            })),
        ),
    ];
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, (category, event_id, payload))| {
            let sequence = index + 1;
            let occurred_at_millis = run
                .finished_at_millis
                .expect("verification finish")
                .saturating_sub(4 - sequence as u64);
            let event = ExecutionEventRecord {
                category,
                event_id,
                occurred_at: fixture_instant(occurred_at_millis),
                payload: Some(payload),
                sequence: ExecutionSequence(i64::try_from(sequence).expect("bounded sequence")),
                summary: format!("{role} verification fact"),
            };
            let digest = Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&event).expect("event JSON"))
            ));
            SeedRuntimeLedgerEvent {
                event,
                event_digest: digest,
            }
        })
        .collect()
}

fn fixture_instant(millis: u64) -> Instant {
    const BASE: u64 = 1_800_000_000_000;
    let offset = millis.checked_sub(BASE).expect("fixture time after base");
    assert!(offset < 1_000, "fixture time stays inside one second");
    Instant(format!("2027-01-15T08:00:00.{offset:03}Z"))
}

fn encoded_json(value: &serde_json::Value) -> EncodedPayload {
    let bytes = serde_json::to_vec(value).expect("JSON");
    EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn seed_delivery(root: &Path, scope: &RepositoryScope, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 50_000)),
            request_digest: "a".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("Delivery publication")
    else {
        panic!("Delivery seed must create a journal");
    };
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
        manifest,
        first_record: AggregateJournalRecord::new(
            first_record.sequence,
            first_record.digest,
            first_record.bytes,
        ),
    };
    let catalog_stream = delivery_catalog_stream(scope, delivery.id());
    let catalog = serde_json::to_vec(&SeedCatalogEntry {
        schema_version: 1,
        repository_scope: scope,
        delivery_id: delivery.id(),
    })
    .expect("catalog JSON");
    let public_actor = PublicEventActor::System {
        id: SystemActorId(canonical_id("sys", 50_001)),
    };
    let event = NewOutboxEvent::public_projection(
        ControlPlaneEventId(canonical_id("evt", 50_001)),
        "delivery.changed.v1",
        b"{}".to_vec(),
        ProjectionEventStream::Delivery(delivery.id().clone()),
        PublicEventScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        Instant("2027-01-15T08:00:00.000Z".into()),
        PublicEventSource::ControlPlane {
            actor: public_actor.clone(),
            component: "delivery-verdict-authority-test".into(),
        },
    )
    .expect("public Delivery projection event");
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    receipt_actor_key(&public_actor).expect("public actor key"),
                    repository_scope_key(scope),
                    RequestId(canonical_id("req", 50_001)),
                )
                .expect("public receipt identity"),
                digest(50_001),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("Delivery JSON"),
                vec![event],
            )
            .with_journal_publication(publication)
            .with_state_mutation(
                StateMutation::new(catalog_stream, 0, catalog).expect("catalog mutation"),
            ),
        )
        .expect("seed Delivery");
    Box::new(storage).close().expect("seed storage close");
}

fn write_state_once(storage: &mut SqliteStorage, stream: String, payload: Vec<u8>, seed: u64) {
    write_state_revision(storage, stream, 0, payload, seed);
}

fn write_state_revision(
    storage: &mut SqliteStorage,
    stream: String,
    expected_revision: u64,
    payload: Vec<u8>,
    seed: u64,
) {
    storage
        .commit(&StateCommit::new(
            receipt_identity(
                ReceiptScopeKey::from_encoded(b"production-verdict-fixture".to_vec())
                    .expect("fixture scope"),
                seed,
            ),
            digest(seed),
            stream,
            expected_revision,
            payload,
            vec![NewOutboxEvent::internal(
                format!("fixture-state-{seed}"),
                "fixture.seed.internal",
                b"{}".to_vec(),
            )],
        ))
        .expect("seed product state");
}

fn receipt_identity(scope: ReceiptScopeKey, seed: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(format!("fixture-actor-{seed}").into_bytes())
            .expect("actor key"),
        scope,
        RequestId(canonical_id("req", seed)),
    )
    .expect("receipt identity")
}

fn digest(seed: u64) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(format!("fixture-{seed}"))
    ))
}

fn exact_binding<'delivery>(
    delivery: &'delivery Delivery,
    run: &StageRun,
) -> &'delivery SessionBinding {
    delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("exact binding")
}

fn verdict_command(seeded: &SeededVerdict, seed: u64) -> DeliverySubmitVerdictCommand {
    DeliverySubmitVerdictCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command: DeliverySubmitVerdictCommandCommand::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(seeded.delivery.revision()).expect("revision")),
        payload: DeliverySubmitVerdictPayload {
            candidate_digest: seeded.candidate_digest.clone(),
            delivery_id: seeded.delivery.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    }
}

fn start(seeded: &SeededVerdict) -> ControlPlane {
    ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&seeded.data),
        Box::new(NoopPublisher),
        LocalDeliveryAdapterConfig::new(&seeded.repository, seeded.scope.clone()),
    )
    .expect("production Control Plane")
}

fn load_candidate_pin(seeded: &SeededVerdict) -> CandidateGitPinReceipt {
    let mut storage = SqliteStorage::open(&seeded.data).expect("open retention storage");
    let pins = {
        let mut retention = storage
            .git_candidate_retention(seeded.repository.parent().expect("repository root"))
            .expect("candidate retention");
        retention
            .load_by_delivery(seeded.delivery.id())
            .expect("load candidate retention")
    };
    Box::new(storage).close().expect("close retention storage");
    let [pin] = pins.as_slice() else {
        panic!("fixture must have one candidate pin");
    };
    pin.clone()
}

fn release_candidate_pin(seeded: &SeededVerdict) {
    let mut storage = SqliteStorage::open(&seeded.data).expect("open release storage");
    let authority = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        seeded.delivery.id().clone(),
        CandidateGitTerminalOutcome::Delivered,
        digest(62_001),
        digest(62_002),
    )
    .expect("release authority");
    {
        let mut retention = storage
            .git_candidate_retention(seeded.repository.parent().expect("repository root"))
            .expect("candidate retention");
        let pins = retention
            .load_by_delivery(seeded.delivery.id())
            .expect("load candidate retention");
        let [pin] = pins.as_slice() else {
            panic!("fixture must have one candidate pin");
        };
        retention
            .release_after_delivery_final(pin, &authority)
            .expect("release candidate pin");
    }
    Box::new(storage).close().expect("close release storage");
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn repository_scope_key(scope: &RepositoryScope) -> ReceiptScopeKey {
    receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("repository scope key")
}

fn delivery_catalog_stream(scope: &RepositoryScope, delivery_id: &DeliveryId) -> String {
    format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(serde_json::to_vec(scope).expect("scope JSON")),
        delivery_id.0
    )
}

fn runtime_stream_id(
    scope: &ReceiptScopeKey,
    job_id: &winwincode_domain::ExecutionJobId,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-ledger-stream.v1\0");
    digest.update((scope.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope.as_bytes());
    digest.update((job_id.0.len() as u64).to_be_bytes());
    digest.update(job_id.0.as_bytes());
    format!("runtime:{:x}", digest.finalize())
}

fn initialize_repository(repository: &Path) -> (String, String) {
    fs::create_dir_all(repository.join("src")).expect("repository directory");
    git(repository, &["init", "-q", "-b", "main"]);
    git(
        repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(repository, &["config", "user.name", "Fixture"]);
    fs::write(repository.join("src/lib.rs"), "pub fn base() {}\n").expect("base source");
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "base"]);
    let base = git_text(repository, &["rev-parse", "HEAD"]);
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn base() {}\npub fn candidate() {}\n",
    )
    .expect("candidate source");
    fs::write(
        repository.join("src/extra.rs"),
        "pub fn added_by_candidate() {}\n",
    )
    .expect("additional Candidate source");
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "candidate"]);
    let candidate = git_text(repository, &["rev-parse", "HEAD"]);
    (base, candidate)
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run Git query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}

fn delivery_state_revision(data: &Path) -> i64 {
    rusqlite::Connection::open(data.join("control-plane.sqlite3"))
        .expect("open database")
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id LIKE 'delivery:%'",
            [],
            |row| row.get(0),
        )
        .expect("Delivery state revision")
}

fn cleanup(seeded: SeededVerdict) {
    fs::remove_dir_all(seeded.root).expect("fixture cleanup");
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-production-verdict-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}
