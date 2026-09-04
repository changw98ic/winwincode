// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::too_many_lines,
    reason = "black-box application tests keep each durable replay and paging assertion together"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, ErrorCode, PageRequest, PublicationCancelCommand, PublicationCancelCommandCommand,
    PublicationCancelPayload, PublicationDetailProjectionKind, PublicationGetParameters,
    PublicationGetQuery, PublicationGetQueryQuery, PublicationListParameters, PublicationListQuery,
    PublicationListQueryQuery, PublicationPublishCommand, PublicationPublishCommandCommand,
    PublicationPublishPayload, PublicationTarget as ApiPublicationTarget,
    PublicationTargetProvider, RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind,
};
use winwincode_audit::{AuditScope, AuditStore};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
};
use winwincode_domain::{
    OpaqueCursor, OrganizationId, ProjectId, PublicationId, RepositoryId, RequestId, Revision,
    SchemaVersion, UserId, WorkspaceId,
};
use winwincode_publication::{
    PolicyPermission, PublicationOperation, PublicationOperationKind, PublicationPolicyContext,
    PublicationPolicyEvidence, PublicationPolicyOrigin, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation, PublicationRequester,
    PublicationResourceFact, PublicationResourceKind, RepositoryPolicyScope,
    RepositoryPublicationPolicy,
    test_support::{CurrentPublicationFixture, current_publication_fixture},
};

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(Default)]
struct NoProviderCalls {
    lookups: usize,
    applies: usize,
}

impl PublicationPort for NoProviderCalls {
    fn lookup(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.lookups += 1;
        panic!("Publication application publish/query/cancel path called the provider")
    }

    fn apply(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.applies += 1;
        panic!("Publication application publish/query/cancel path called the provider")
    }
}

struct SuccessfulProvider;

impl PublicationPort for SuccessfulProvider {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        Ok(PublicationPortObservation::absent(operation))
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        let resource = (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("canonical pull request resource")
        });
        Ok(PublicationPortMutation::applied(operation, resource, true))
    }
}

struct RemoteUrlCodeProvider;

impl PublicationPort for RemoteUrlCodeProvider {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        Ok(PublicationPortObservation::unknown(
            operation,
            "https://example.com/provider-result",
        ))
    }

    fn apply(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        panic!("an unknown lookup must stop before a provider write")
    }
}

#[test]
fn publish_maps_generated_projection_and_preserves_receipt_conflicts() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let command = publish_command(&fixture, 1, 11);
    let evidence = evidence(&fixture, 1_100);
    let mut provider = NoProviderCalls::default();
    let mut control_plane = start(&root);

    let first = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            fixture.attribution(),
            &policy,
            &evidence,
            &origin,
            &mut provider,
        )
        .expect("publish one canonical intent");
    assert_eq!(first.revision(), 1);
    let projected = control_plane
        .publication_get(&get_query(10, 1, repository_scope()))
        .expect("map the verified intent through the generated get projection");
    assert_eq!(
        projected.result.kind,
        PublicationDetailProjectionKind::PublicationDetail
    );
    assert_eq!(projected.result.summary.id, publication_id(1));
    assert_eq!(
        projected.result.summary.approved_by,
        winwincode_api::generated::ActorId::UserId(UserId(
            fixture.authorization().approved_by().to_owned()
        ))
    );
    assert_eq!(
        projected.result.summary.target.repository.0,
        "example/widget"
    );
    assert_eq!(projected.result.summary.resource_ref, None);
    assert_eq!(projected.result.steps.len(), 4);
    assert_eq!(
        projected
            .result
            .steps
            .iter()
            .map(|step| (step.kind.as_str(), step.state.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("branch", "pending"),
            ("pull_request", "pending"),
            ("issue_comment", "pending"),
            ("commit_status", "pending"),
        ]
    );
    assert!(projected.result.retryable);
    assert!(projected.result.cancellable);
    assert_eq!(projected.result.cancellation, None);
    assert!(!projected.result.history_truncated);
    assert_eq!(projected.result.history.len(), 1);
    assert_eq!(projected.result.history[0].revision, Revision(1));
    assert_eq!(projected.result.history[0].state, "pending");
    let public_detail = serde_json::to_value(&projected.result).expect("serialize public detail");
    let encoded = serde_json::to_string(&public_detail).expect("encode public detail");
    for forbidden in [
        "providerIdempotencyKey",
        "operationKey",
        "requestSha256",
        "candidateCommitId",
        "artifactId",
        "artifactDigest",
        "intentSha256",
        "repositoryScopeSha256",
    ] {
        assert!(!encoded.contains(forbidden), "detail leaked {forbidden}");
    }
    for private_value in [
        fixture.authorization().artifact_id(),
        fixture.authorization().artifact_digest().0.as_str(),
        fixture.authorization().repository_scope_sha256().0.as_str(),
        fixture.authorization().provider_idempotency_key(),
    ] {
        assert!(
            !encoded.contains(private_value),
            "detail leaked a private durable value"
        );
    }

    let replay = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            fixture.attribution(),
            &policy,
            &evidence,
            &origin,
            &mut provider,
        )
        .expect("replay the exact publish receipt");
    assert_eq!(replay, first);

    let generated_replay = control_plane
        .publication_publish(&command)
        .expect("generated-only application replay precedes every production authority");
    assert_eq!(generated_replay.result.id, first.id().clone());
    assert_eq!(generated_replay.current_revision, Revision(1));

    let mut changed = command.clone();
    changed.payload.publication_id = publication_id(2);
    let conflict = control_plane
        .commit_publication_publish(
            &changed,
            fixture.authorization(),
            fixture.attribution(),
            &policy,
            &evidence,
            &origin,
            &mut provider,
        )
        .expect_err("the same receipt identity cannot accept a changed body");
    assert_eq!(conflict.public_code(), ErrorCode::IdempotencyConflict);
    let generated_conflict = control_plane
        .publication_publish(&changed)
        .expect_err("generated-only application preserves the durable receipt conflict");
    assert_eq!(
        generated_conflict.public_code(),
        ErrorCode::IdempotencyConflict
    );
    assert_eq!((provider.lookups, provider.applies), (0, 0));

    control_plane.shutdown().expect("clean shutdown");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn get_projects_completed_steps_and_exact_status_history_without_provider_requests() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let mut no_provider = NoProviderCalls::default();
    let mut control_plane = start(&root);
    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut no_provider,
        1,
        11,
        1_100,
    );
    let context = PublicationPolicyContext::try_new(
        PublicationRequester::User(requester_id()),
        request_id(12),
        policy_scope(),
        origin,
        evidence(&fixture, 2_000),
    )
    .expect("current publication policy context");
    control_plane
        .resume_publication(
            fixture.publication_id(),
            &context,
            &policy,
            &mut SuccessfulProvider,
        )
        .expect("complete publication through existing domain authority");

    let detail = control_plane
        .publication_get(&get_query(13, 1, repository_scope()))
        .expect("read completed publication detail");
    assert_eq!(detail.result.summary.revision, Revision(11));
    assert_eq!(detail.result.summary.state, "published");
    assert_eq!(detail.result.history.len(), 11);
    assert_eq!(detail.result.history[0].state, "pending");
    assert_eq!(detail.result.history[1].state, "publishing");
    assert_eq!(detail.result.history[10].state, "published");
    assert!(
        detail
            .result
            .steps
            .iter()
            .all(|step| step.state == "succeeded" && !step.retryable)
    );
    assert_eq!(
        detail
            .result
            .steps
            .iter()
            .map(|step| step.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["branch", "pull_request", "issue_comment", "commit_status"]
    );
    assert_eq!(
        detail.result.steps[1].resource_ref,
        detail.result.summary.resource_ref
    );
    assert!(!detail.result.retryable);
    assert!(!detail.result.cancellable);
    assert_eq!(detail.result.cancellation, None);
    assert!(!detail.result.history_truncated);
    assert_eq!((no_provider.lookups, no_provider.applies), (0, 0));

    control_plane.shutdown().expect("clean shutdown");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn get_redacts_a_provider_url_disguised_as_an_outcome_code() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let mut no_provider = NoProviderCalls::default();
    let mut control_plane = start(&root);
    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut no_provider,
        1,
        11,
        1_100,
    );
    let context = PublicationPolicyContext::try_new(
        PublicationRequester::User(requester_id()),
        request_id(12),
        policy_scope(),
        origin,
        evidence(&fixture, 2_000),
    )
    .expect("current publication policy context");
    control_plane
        .resume_publication(
            fixture.publication_id(),
            &context,
            &policy,
            &mut RemoteUrlCodeProvider,
        )
        .expect("persist the provider's unknown result");

    let detail = control_plane
        .publication_get(&get_query(13, 1, repository_scope()))
        .expect("redact the unsafe provider code without hiding durable status");
    assert_eq!(detail.result.summary.revision, Revision(3));
    assert_eq!(detail.result.steps[0].state, "unknown");
    assert_eq!(detail.result.steps[0].outcome_code, None);
    assert!(
        !serde_json::to_string(&detail)
            .expect("encode detail")
            .contains("https://")
    );

    control_plane.shutdown().expect("clean shutdown");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn cancel_is_scope_revision_and_restart_safe_without_provider_effects() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let mut provider = NoProviderCalls::default();
    let mut control_plane = start(&root);
    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut provider,
        1,
        11,
        1_100,
    );

    let command = cancel_command(1, 21, Revision(1), repository_scope(), "operator cancelled");
    let cancelled = control_plane
        .publication_cancel(&command, 1_200)
        .expect("cancel pending Publication");
    assert_eq!(cancelled.previous_revision, Revision(1));
    assert_eq!(cancelled.current_revision, Revision(2));
    assert_eq!(cancelled.result.state, "cancelled");
    control_plane.shutdown().expect("close before restart");

    let mut control_plane = start(&root);
    let replay = control_plane
        .publication_cancel(&command, 9_000)
        .expect("restart replays the original cancellation time and result");
    assert_eq!(replay, cancelled);
    let detail = control_plane
        .publication_get(&get_query(44, 1, repository_scope()))
        .expect("restart rebuilds cancellation detail from the verified journal");
    assert_eq!(detail.result.summary.revision, Revision(2));
    assert_eq!(
        detail
            .result
            .history
            .iter()
            .map(|entry| (entry.revision.clone(), entry.state.as_str()))
            .collect::<Vec<_>>(),
        vec![(Revision(1), "pending"), (Revision(2), "cancelled")]
    );
    let cancellation = detail
        .result
        .cancellation
        .expect("closed cancellation detail");
    assert_eq!(cancellation.revision, Revision(2));
    assert_eq!(cancellation.cancelled_at.0, "1970-01-01T00:00:01.200Z");
    assert_eq!(cancellation.reason, "operator cancelled");
    assert!(!detail.result.retryable);
    assert!(!detail.result.cancellable);

    let mut changed = command.clone();
    changed.payload.reason = "changed receipt body".to_owned();
    let changed_error = control_plane
        .publication_cancel(&changed, 9_100)
        .expect_err("changed cancellation body conflicts");
    assert_eq!(changed_error.public_code(), ErrorCode::IdempotencyConflict);

    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut provider,
        2,
        12,
        1_300,
    );
    let stale = cancel_command(2, 22, Revision(0), repository_scope(), "stale revision");
    let stale_error = control_plane
        .publication_cancel(&stale, 1_400)
        .expect_err("stale cancellation revision is rejected");
    assert_eq!(stale_error.public_code(), ErrorCode::RevisionConflict);

    let foreign = cancel_command(2, 23, Revision(1), foreign_scope(), "foreign scope");
    let foreign_error = control_plane
        .publication_cancel(&foreign, 1_500)
        .expect_err("foreign repository scope is rejected");
    assert_eq!(foreign_error.public_code(), ErrorCode::InvalidRequest);
    assert_eq!((provider.lookups, provider.applies), (0, 0));

    control_plane.shutdown().expect("clean shutdown");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn cancel_reuses_requested_audit_time_after_state_failure_and_restart() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let mut provider = NoProviderCalls::default();
    let command = cancel_command(1, 25, Revision(1), repository_scope(), "retry cancellation");
    let mut control_plane = start(&root);
    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut provider,
        1,
        15,
        1_100,
    );
    install_cancel_state_failure(&root, &command.payload.publication_id);

    let failure = control_plane
        .publication_cancel(&command, 1_200)
        .expect_err("state failure follows the durable requested audit");
    assert_eq!(failure.public_code(), ErrorCode::ServiceUnavailable);
    let unchanged = control_plane
        .publication_get(&get_query(43, 1, repository_scope()))
        .expect("failed cancellation leaves the original Publication");
    assert_eq!(unchanged.result.summary.state, "pending");
    assert_eq!(unchanged.result.summary.revision, Revision(1));
    control_plane.shutdown().expect("close failed attempt");
    assert_eq!(
        cancel_audit_facts(&root),
        vec![("publication.cancel-requested".to_owned(), 1_200)]
    );

    remove_cancel_state_failure(&root);
    let mut control_plane = start(&root);
    let cancelled = control_plane
        .publication_cancel(&command, 9_000)
        .expect("retry reuses the immutable requested audit timestamp");
    assert_eq!(cancelled.current_revision, Revision(2));
    assert_eq!(cancelled.result.state, "cancelled");
    let replay = control_plane
        .publication_cancel(&command, 12_000)
        .expect("exact replay retains both original audit facts");
    assert_eq!(replay, cancelled);
    control_plane.shutdown().expect("close successful retry");
    assert_eq!(
        cancel_audit_facts(&root),
        vec![
            ("publication.cancel-requested".to_owned(), 1_200),
            ("publication.cancelled".to_owned(), 9_000),
        ]
    );
    assert_eq!((provider.lookups, provider.applies), (0, 0));
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn list_and_get_use_stable_scope_bound_snapshots_across_restart() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let policy = policy(&fixture);
    let origin = origin();
    let mut provider = NoProviderCalls::default();
    let mut control_plane = start(&root);
    for (index, request, occurred_at) in [(1, 11, 1_100), (2, 12, 1_200), (3, 13, 1_300)] {
        publish(
            &mut control_plane,
            &fixture,
            &policy,
            &origin,
            &mut provider,
            index,
            request,
            occurred_at,
        );
    }

    let first = control_plane
        .publication_list(&list_query(31, 2, None, Vec::new()))
        .expect("read first stable Publication page");
    assert_eq!(
        first
            .result
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        vec![publication_id(1), publication_id(2)]
    );
    assert!(first.page.has_more);
    let cursor = first.page.next_cursor.expect("first page cursor");

    publish(
        &mut control_plane,
        &fixture,
        &policy,
        &origin,
        &mut provider,
        4,
        14,
        1_400,
    );
    let second = control_plane
        .publication_list(&list_query(32, 2, Some(cursor), Vec::new()))
        .expect("continue below the frozen upper bound");
    assert_eq!(
        second
            .result
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        vec![publication_id(3)]
    );
    assert!(!second.page.has_more);

    let get = control_plane
        .publication_get(&get_query(41, 3, repository_scope()))
        .expect("read exact fully verified Publication");
    assert_eq!(get.result.summary.id, publication_id(3));
    assert_eq!(get.result.summary.state, "pending");
    let foreign_get = control_plane
        .publication_get(&get_query(42, 3, foreign_scope()))
        .expect_err("get cannot cross a repository scope");
    assert_eq!(foreign_get.public_code(), ErrorCode::InvalidRequest);

    let mutable_page = control_plane
        .publication_list(&list_query(33, 1, None, Vec::new()))
        .expect("open another frozen snapshot");
    let mutable_cursor = mutable_page.page.next_cursor.expect("snapshot cursor");
    control_plane
        .publication_cancel(
            &cancel_command(
                2,
                24,
                Revision(1),
                repository_scope(),
                "invalidate stable snapshot",
            ),
            1_500,
        )
        .expect("mutate a Publication inside the frozen upper bound");
    let expired = control_plane
        .publication_list(&list_query(34, 1, Some(mutable_cursor), Vec::new()))
        .expect_err("a changed matching snapshot expires the cursor");
    assert_eq!(expired.public_code(), ErrorCode::ReadCursorExpired);
    control_plane
        .shutdown()
        .expect("close before query restart");

    let mut control_plane = start(&root);
    let cancelled_only = control_plane
        .publication_list(&list_query(35, 20, None, vec!["cancelled".to_owned()]))
        .expect("rebuild a filtered list from durable state after restart");
    assert_eq!(cancelled_only.result.items.len(), 1);
    assert_eq!(cancelled_only.result.items[0].id, publication_id(2));
    assert_eq!((provider.lookups, provider.applies), (0, 0));

    control_plane.shutdown().expect("clean shutdown");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[allow(clippy::too_many_arguments)]
fn publish(
    control_plane: &mut ControlPlane,
    fixture: &CurrentPublicationFixture,
    policy: &RepositoryPublicationPolicy,
    origin: &PublicationPolicyOrigin,
    provider: &mut NoProviderCalls,
    publication: u64,
    request: u64,
    occurred_at: u64,
) {
    control_plane
        .commit_publication_publish(
            &publish_command(fixture, publication, request),
            fixture.authorization(),
            fixture.attribution(),
            policy,
            &evidence(fixture, occurred_at),
            origin,
            provider,
        )
        .expect("persist fixture Publication");
}

fn publish_command(
    fixture: &CurrentPublicationFixture,
    publication: u64,
    request: u64,
) -> PublicationPublishCommand {
    let authorization = fixture.authorization();
    PublicationPublishCommand {
        actor: actor(),
        command: PublicationPublishCommandCommand::PublicationPublish,
        expected_revision: Revision(0),
        payload: PublicationPublishPayload {
            candidate_digest: authorization.candidate_digest().clone(),
            delivery_id: authorization.binding().delivery_id().clone(),
            publication_id: publication_id(publication),
            target: ApiPublicationTarget {
                base_branch: authorization.target().base_branch().to_owned(),
                head_branch: authorization.target().head_branch().to_owned(),
                head_repository: winwincode_domain::GitHubRepositorySlug(
                    authorization.target().head_repository().to_owned(),
                ),
                provider: PublicationTargetProvider::Github,
                repository: winwincode_domain::GitHubRepositorySlug(
                    authorization.target().repository().to_owned(),
                ),
            },
        },
        request_id: request_id(request),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: repository_scope(),
    }
}

fn cancel_command(
    publication: u64,
    request: u64,
    expected_revision: Revision,
    scope: RepositoryScope,
    reason: &str,
) -> PublicationCancelCommand {
    PublicationCancelCommand {
        actor: actor(),
        command: PublicationCancelCommandCommand::PublicationCancel,
        expected_revision,
        payload: PublicationCancelPayload {
            publication_id: publication_id(publication),
            reason: reason.to_owned(),
        },
        request_id: request_id(request),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn get_query(request: u64, publication: u64, scope: RepositoryScope) -> PublicationGetQuery {
    PublicationGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: PublicationGetParameters {
            publication_id: publication_id(publication),
        },
        query: PublicationGetQueryQuery::PublicationGet,
        request_id: request_id(request),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn list_query(
    request: u64,
    limit: i64,
    cursor: Option<OpaqueCursor>,
    states: Vec<String>,
) -> PublicationListQuery {
    PublicationListQuery {
        actor: actor(),
        page: PageRequest { cursor, limit },
        parameters: PublicationListParameters {
            delivery_id: None,
            states,
        },
        query: PublicationListQueryQuery::PublicationList,
        request_id: request_id(request),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: repository_scope(),
    }
}

fn evidence(fixture: &CurrentPublicationFixture, occurred_at: u64) -> PublicationPolicyEvidence {
    PublicationPolicyEvidence::try_from_current_facts(
        fixture.authorization(),
        true,
        true,
        occurred_at,
    )
    .expect("sealed fixture evidence")
}

fn policy(fixture: &CurrentPublicationFixture) -> RepositoryPublicationPolicy {
    RepositoryPublicationPolicy::try_new(
        policy_scope(),
        fixture.authorization().target().repository(),
        vec![PublicationRequester::User(requester_id())],
        Vec::new(),
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        Vec::new(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        10_000,
    )
    .expect("closed fixture policy")
}

fn policy_scope() -> RepositoryPolicyScope {
    let scope = repository_scope();
    RepositoryPolicyScope::try_new(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("canonical repository policy scope")
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
        project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
    }
}

fn foreign_scope() -> RepositoryScope {
    RepositoryScope {
        repository_id: RepositoryId("rep_00000000000000000000000002".to_owned()),
        ..repository_scope()
    }
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: requester_id(),
        kind: UserActorKind::User,
    })
}

fn requester_id() -> UserId {
    UserId("usr_00000000000000000000000002".to_owned())
}

fn publication_id(index: u64) -> PublicationId {
    PublicationId(format!("pub_{index:026}"))
}

fn request_id(index: u64) -> RequestId {
    RequestId(format!("req_{index:026}"))
}

fn origin() -> PublicationPolicyOrigin {
    PublicationPolicyOrigin::local("publication-application-test")
        .expect("canonical local audit origin")
}

fn audit_access() -> winwincode_audit::AuditAccess {
    let scope = repository_scope();
    AuditScope::repository(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("canonical audit scope")
    .into_access()
}

fn cancel_audit_facts(root: &Path) -> Vec<(String, u64)> {
    let store = AuditStore::open(root.join("audit")).expect("open audit fixture");
    let facts = store
        .read(&audit_access(), 0, 200, i64::MAX as u64)
        .expect("read audit fixture")
        .records()
        .iter()
        .filter_map(|record| record.event())
        .filter(|event| event.result_code().starts_with("publication.cancel"))
        .map(|event| (event.result_code().to_owned(), event.occurred_at_millis()))
        .collect();
    store.close().expect("close audit fixture");
    facts
}

fn install_cancel_state_failure(root: &Path, publication_id: &PublicationId) {
    let connection = Connection::open(root.join("control-plane.sqlite3"))
        .expect("open publication state failure injector");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER fail_publication_cancel_state BEFORE UPDATE ON product_state \
             WHEN OLD.stream_id = 'publication:{}' BEGIN \
             SELECT RAISE(ABORT, 'injected Publication cancellation failure'); END;",
            publication_id.0
        ))
        .expect("install publication state failure");
    connection
        .close()
        .expect("close publication state failure injector");
}

fn remove_cancel_state_failure(root: &Path) {
    let connection = Connection::open(root.join("control-plane.sqlite3"))
        .expect("open publication state failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_publication_cancel_state;")
        .expect("remove publication state failure");
    connection
        .close()
        .expect("close publication state failure remover");
}

fn start(root: &PathBuf) -> ControlPlane {
    ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .expect("start local Control Plane")
}

fn temporary_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "winwincode-publication-application-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("create fixture root");
    path
}
