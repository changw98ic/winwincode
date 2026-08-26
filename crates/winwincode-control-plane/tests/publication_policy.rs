// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::too_many_lines,
    reason = "black-box policy tracers keep each storage, audit, and provider assertion together"
)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use winwincode_api::generated::{
    Actor, ErrorCode, PublicationPublishCommand as ApiPublicationPublishCommand,
    PublicationPublishCommandCommand, PublicationPublishPayload,
    PublicationTarget as ApiPublicationTarget, PublicationTargetProvider, RepositoryScope,
    RepositoryScopeKind, UserActor, UserActorKind,
};
use winwincode_audit::{AuditActionKind, AuditActor, AuditOutcome, AuditScope, AuditState};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest,
    UserId, WorkspaceId,
};
use winwincode_publication::{
    PolicyPermission, PublicationAuthorization, PublicationOperation, PublicationOperationKind,
    PublicationPolicyContext, PublicationPolicyDecision, PublicationPolicyEvidence,
    PublicationPolicyOrigin, PublicationPolicyRule, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation, PublicationRequester,
    PublicationResourceFact, PublicationResourceKind, PublicationState, RepositoryPolicyScope,
    RepositoryPublicationPolicy,
    test_support::{CurrentPublicationFixture, current_publication_fixture},
};
use winwincode_storage::SqliteStorage;

#[derive(Default)]
struct NoProviderCalls {
    lookups: usize,
    applies: usize,
}

#[derive(Default)]
struct CountingUnknownPort {
    lookups: usize,
    applies: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderCallKind {
    Lookup,
    Apply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DifferentialProviderMode {
    Success,
    PermissionDenied,
    RateLimitedOnce,
    PullRequestRace,
    CommentRejected,
}

struct DifferentialProvider {
    mode: DifferentialProviderMode,
    calls: Vec<(ProviderCallKind, PublicationOperationKind)>,
    remote_writes: Vec<PublicationOperationKind>,
    rate_limit_returned: bool,
}

impl DifferentialProvider {
    fn new(mode: DifferentialProviderMode) -> Self {
        Self {
            mode,
            calls: Vec::new(),
            remote_writes: Vec::new(),
            rate_limit_returned: false,
        }
    }

    fn resource(operation: &PublicationOperation) -> Option<PublicationResourceFact> {
        (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("canonical fake GitHub pull request")
        })
    }
}

impl PublicationPort for DifferentialProvider {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.calls
            .push((ProviderCallKind::Lookup, operation.kind()));
        if self.mode == DifferentialProviderMode::RateLimitedOnce && !self.rate_limit_returned {
            self.rate_limit_returned = true;
            return Ok(PublicationPortObservation::unknown(
                operation,
                "github-rate-limited",
            ));
        }
        Ok(PublicationPortObservation::absent(operation))
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.calls.push((ProviderCallKind::Apply, operation.kind()));
        if self.mode == DifferentialProviderMode::PermissionDenied
            && operation.kind() == PublicationOperationKind::Branch
        {
            return Ok(PublicationPortMutation::rejected(
                operation,
                "github-permission-denied",
            ));
        }
        if self.mode == DifferentialProviderMode::CommentRejected
            && operation.kind() == PublicationOperationKind::IssueComment
        {
            return Ok(PublicationPortMutation::rejected(
                operation,
                "github-comment-rejected",
            ));
        }
        let remote_write_performed = !(self.mode == DifferentialProviderMode::PullRequestRace
            && operation.kind() == PublicationOperationKind::PullRequest);
        if remote_write_performed {
            self.remote_writes.push(operation.kind());
        }
        Ok(PublicationPortMutation::applied(
            operation,
            Self::resource(operation),
            remote_write_performed,
        ))
    }
}

impl PublicationPort for CountingUnknownPort {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.lookups += 1;
        Ok(PublicationPortObservation::unknown(
            operation,
            "provider-result-pending",
        ))
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.applies += 1;
        Ok(PublicationPortMutation::unknown(
            operation,
            "provider-result-pending",
        ))
    }
}

impl PublicationPort for NoProviderCalls {
    fn lookup(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        self.lookups += 1;
        panic!("persisting a Publication intent must not call the provider")
    }

    fn apply(
        &mut self,
        _operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        self.applies += 1;
        panic!("persisting a Publication intent must not call the provider")
    }
}

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
fn allowed_publication_records_the_exact_policy_rule_before_persisting_intent() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = PublicationRequester::User(UserId("usr_00000000000000000000000002".to_owned()));
    let approver = UserId(fixture.authorization().approved_by().to_owned());
    let policy_scope = RepositoryPolicyScope::try_new(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical repository policy scope");
    let policy = RepositoryPublicationPolicy::try_new(
        policy_scope,
        "example/widget",
        vec![requester.clone()],
        Vec::new(),
        vec![approver],
        Vec::new(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    )
    .expect("closed repository publication policy");
    let evidence = PublicationPolicyEvidence::try_from_current_facts(
        fixture.authorization(),
        true,
        true,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("sealed publication policy evidence");
    let command = ApiPublicationPublishCommand {
        actor: Actor::UserActor(UserActor {
            id: match requester {
                PublicationRequester::User(id) => id,
                PublicationRequester::ServiceAccount(_) | PublicationRequester::System(_) => {
                    unreachable!("fixture requester is a user")
                }
            },
            kind: UserActorKind::User,
        }),
        command: PublicationPublishCommandCommand::PublicationPublish,
        expected_revision: Revision(0),
        payload: PublicationPublishPayload {
            candidate_digest: fixture.authorization().candidate_digest().clone(),
            delivery_id: fixture.authorization().binding().delivery_id().clone(),
            publication_id: fixture.publication_id().clone(),
            target: ApiPublicationTarget {
                base_branch: fixture.authorization().target().base_branch().to_owned(),
                head_branch: fixture.authorization().target().head_branch().to_owned(),
                head_repository: winwincode_domain::GitHubRepositorySlug(
                    fixture
                        .authorization()
                        .target()
                        .head_repository()
                        .to_owned(),
                ),
                provider: PublicationTargetProvider::Github,
                repository: winwincode_domain::GitHubRepositorySlug(
                    fixture.authorization().target().repository().to_owned(),
                ),
            },
        },
        request_id: RequestId("req_00000000000000000000000011".to_owned()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    let origin =
        PublicationPolicyOrigin::local("control-plane-http").expect("closed local request origin");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane with the immutable audit store");
    let mut port = NoProviderCalls::default();
    let foreign_authorization = PublicationAuthorization::try_from_current_facts(
        fixture.authorization().binding().clone(),
        fixture.authorization().source().clone(),
        fixture.authorization().target().clone(),
        fixture.authorization().candidate_commit_id(),
        fixture.authorization().artifact_id(),
        fixture.authorization().artifact_digest().clone(),
        fixture.authorization().approved_by(),
        fixture.authorization().approved_at_millis(),
        Sha256Digest(format!("sha256:{}", "0".repeat(64))),
    )
    .expect("structurally valid authorization from another repository scope");
    let foreign_evidence = PublicationPolicyEvidence::try_from_current_facts(
        &foreign_authorization,
        true,
        true,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("sealed foreign policy evidence");
    let foreign = control_plane
        .commit_publication_publish(
            &command,
            &foreign_authorization,
            &policy,
            &foreign_evidence,
            &origin,
            &mut port,
        )
        .expect_err("repository scope digest must match the exact policy scope");
    assert_eq!(foreign.public_code(), ErrorCode::TrustedFactsUnavailable);

    let publication = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &policy,
            &evidence,
            &origin,
            &mut port,
        )
        .expect("allow and persist the exact Publication intent");

    assert_eq!(publication.state(), PublicationState::Pending);
    assert_eq!((port.lookups, port.applies), (0, 0));

    let access = AuditScope::repository(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("canonical audit scope")
    .into_access();
    let page = control_plane
        .read_audit(
            &access,
            0,
            10,
            fixture.publish_context().occurred_at_millis(),
        )
        .expect("read policy audit through the Control Plane seam");
    let [record, intent_record] = page.records() else {
        panic!("an allowed intent must record both its policy decision and durable result")
    };
    let event = record.event().expect("policy audit payload is retained");
    assert_eq!(event.action().kind(), AuditActionKind::Policy);
    assert_eq!(event.action().name(), "publication.allowed");
    assert_eq!(event.result_code(), "policy.allowed");
    assert_eq!(event.outcome(), AuditOutcome::Succeeded);
    assert_eq!(
        event.state(),
        &AuditState::Unchanged {
            current: Some(policy.digest().clone()),
        },
    );
    assert_eq!(event.request_id(), &command.request_id);
    assert_eq!(
        event.actor(),
        &AuditActor::User(match &command.actor {
            Actor::UserActor(actor) => actor.id.clone(),
            Actor::ServiceAccountActor(_) | Actor::SystemActor(_) => unreachable!(),
        }),
    );
    assert_eq!(
        event.subject().delivery_id(),
        Some(fixture.authorization().binding().delivery_id()),
    );
    assert_eq!(
        event.subject().publication_id(),
        Some(fixture.publication_id()),
    );
    let intent = intent_record
        .event()
        .expect("publication intent audit payload is retained");
    assert_eq!(intent.action().kind(), AuditActionKind::Publication);
    assert_eq!(intent.action().name(), "publication.state");
    assert_eq!(intent.result_code(), "publication.intent-recorded");
    assert_eq!(intent.outcome(), AuditOutcome::Succeeded);
    assert!(matches!(
        intent.state(),
        AuditState::Unchanged { current: Some(_) }
    ));
    assert_eq!(intent.request_id(), &command.request_id);
    assert_eq!(
        intent.subject().publication_id(),
        Some(fixture.publication_id()),
    );

    control_plane.shutdown().expect("shutdown Control Plane");
    fs::remove_dir_all(&root).expect("remove fixture root");
}

#[test]
fn explicit_requester_deny_wins_and_records_the_rule_without_intent_or_provider_call() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = UserId("usr_00000000000000000000000002".to_owned());
    let requester_fact = PublicationRequester::User(requester.clone());
    let policy_scope = repository_policy_scope(&scope);
    let denied_policy = RepositoryPublicationPolicy::try_new(
        policy_scope.clone(),
        "example/widget",
        vec![requester_fact.clone()],
        vec![requester_fact.clone()],
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        PolicyPermission::Deny,
        true,
        PolicyPermission::Deny,
        1,
    )
    .expect("overlapping allow and explicit-deny policy");
    let denied_evidence = PublicationPolicyEvidence::try_from_current_facts(
        fixture.authorization(),
        false,
        false,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("sealed denied evidence");
    let command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000012",
    );
    let origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let mut port = NoProviderCalls::default();
    let denied = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &denied_policy,
            &denied_evidence,
            &origin,
            &mut port,
        )
        .expect_err("explicit requester deny must win");
    assert_eq!(denied.public_code(), ErrorCode::PermissionDenied);
    assert!(!denied.retryable());
    assert_eq!(
        denied.decision().map(PublicationPolicyDecision::rule),
        Some(PublicationPolicyRule::RequesterExplicitDeny),
    );
    assert_eq!(
        denied.public_details().get("ruleId"),
        Some(&winwincode_api::generated::ErrorDetailValue::Variant4(
            "publication.requester.denied".to_owned(),
        )),
    );
    assert_eq!((port.lookups, port.applies), (0, 0));

    let access = AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("audit scope")
    .into_access();
    let denied_page = control_plane
        .read_audit(
            &access,
            0,
            10,
            fixture.publish_context().occurred_at_millis(),
        )
        .expect("read denial audit");
    let [denied_record] = denied_page.records() else {
        panic!("one denial must record one rule")
    };
    let denied_event = denied_record.event().expect("retained denial");
    assert_eq!(denied_event.outcome(), AuditOutcome::Rejected);
    assert_eq!(denied_event.action().name(), "publication.requester.denied");
    assert_eq!(denied_event.result_code(), "policy.denied");

    let allowed_policy = RepositoryPublicationPolicy::try_new(
        policy_scope,
        "example/widget",
        vec![requester_fact],
        Vec::new(),
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        Vec::new(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    )
    .expect("replacement current policy");
    let allowed_evidence = PublicationPolicyEvidence::try_from_current_facts(
        fixture.authorization(),
        true,
        true,
        fixture.publish_context().occurred_at_millis(),
    )
    .expect("allowed evidence");
    let publication = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &allowed_policy,
            &allowed_evidence,
            &origin,
            &mut port,
        )
        .expect("a recorded denial must not create an intent or command receipt");
    assert_eq!(publication.revision(), 1);
    assert_eq!(publication.state(), PublicationState::Pending);
    assert_eq!((port.lookups, port.applies), (0, 0));

    control_plane.shutdown().expect("shutdown Control Plane");
    fs::remove_dir_all(&root).expect("remove fixture root");
}

#[test]
fn repository_verification_artifact_and_approval_denials_are_exact_and_side_effect_free() {
    let fixture = current_publication_fixture();
    let requester = UserId("usr_00000000000000000000000002".to_owned());
    let requester_fact = PublicationRequester::User(requester.clone());
    let approver = UserId(fixture.authorization().approved_by().to_owned());
    let cases = vec![
        (
            "approver-explicit-deny",
            RepositoryPublicationPolicy::try_new(
                repository_policy_scope(&repository_scope()),
                "example/widget",
                vec![requester_fact.clone()],
                Vec::new(),
                vec![approver.clone()],
                vec![approver.clone()],
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                5_000,
            )
            .expect("explicit approver denial policy"),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::ApproverExplicitDeny,
        ),
        (
            "requester-not-allowed",
            RepositoryPublicationPolicy::try_new(
                repository_policy_scope(&repository_scope()),
                "example/widget",
                vec![PublicationRequester::User(UserId(
                    "usr_00000000000000000000000009".to_owned(),
                ))],
                Vec::new(),
                vec![approver.clone()],
                Vec::new(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                5_000,
            )
            .expect("requester allow-list policy"),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::RequesterNotAllowed,
        ),
        (
            "approver-not-allowed",
            RepositoryPublicationPolicy::try_new(
                repository_policy_scope(&repository_scope()),
                "example/widget",
                vec![requester_fact],
                Vec::new(),
                vec![UserId("usr_00000000000000000000000009".to_owned())],
                Vec::new(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                5_000,
            )
            .expect("approver allow-list policy"),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::ApproverNotAllowed,
        ),
        (
            "repository-write",
            repository_policy(
                &fixture,
                requester.clone(),
                PolicyPermission::Deny,
                true,
                PolicyPermission::Allow,
                5_000,
            ),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::RepositoryWriteDenied,
        ),
        (
            "independent-verification",
            repository_policy(
                &fixture,
                requester.clone(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                5_000,
            ),
            policy_evidence(&fixture, false, true, 1_100),
            PublicationPolicyRule::IndependentVerificationRequired,
        ),
        (
            "artifact-export-policy",
            repository_policy(
                &fixture,
                requester.clone(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Deny,
                5_000,
            ),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::ArtifactExportDenied,
        ),
        (
            "artifact-export-fact",
            repository_policy(
                &fixture,
                requester.clone(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                5_000,
            ),
            policy_evidence(&fixture, true, false, 1_100),
            PublicationPolicyRule::ArtifactNotExportable,
        ),
        (
            "approval-expired",
            repository_policy(
                &fixture,
                requester.clone(),
                PolicyPermission::Allow,
                true,
                PolicyPermission::Allow,
                50,
            ),
            policy_evidence(&fixture, true, true, 1_100),
            PublicationPolicyRule::ApprovalExpired,
        ),
    ];

    for (index, (name, policy, evidence, expected_rule)) in cases.into_iter().enumerate() {
        let root = temporary_root();
        let scope = repository_scope();
        let command = api_publish_command(
            &fixture,
            requester.clone(),
            &scope,
            &format!("req_{:026}", index + 30),
        );
        let origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("start Control Plane");
        let mut port = NoProviderCalls::default();

        let error = control_plane
            .commit_publication_publish(
                &command,
                fixture.authorization(),
                &policy,
                &evidence,
                &origin,
                &mut port,
            )
            .expect_err("policy denial must fail closed");
        assert_eq!(error.public_code(), ErrorCode::PermissionDenied, "{name}");
        assert_eq!(
            error.decision().map(PublicationPolicyDecision::rule),
            Some(expected_rule),
            "{name}",
        );
        assert_eq!((port.lookups, port.applies), (0, 0), "{name}");

        let access = AuditScope::repository(
            scope.organization_id,
            scope.workspace_id,
            scope.project_id,
            scope.repository_id,
        )
        .expect("audit scope")
        .into_access();
        let page = control_plane
            .read_audit(&access, 0, 10, evidence.observed_at_millis())
            .expect("read exact policy denial");
        let [record] = page.records() else {
            panic!("{name}: one denial audit expected")
        };
        assert_eq!(
            record.event().expect("retained denial").action().name(),
            expected_rule.as_str(),
            "{name}",
        );

        control_plane.shutdown().expect("shutdown Control Plane");
        fs::remove_dir_all(&root).expect("remove fixture root");
    }
}

#[test]
fn missing_audit_store_fails_closed_but_exact_receipt_replay_needs_no_current_policy_or_audit() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = UserId("usr_00000000000000000000000002".to_owned());
    let command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000041",
    );
    let allowed_policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let evidence = policy_evidence(&fixture, true, true, 1_100);
    let origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");

    let storage = SqliteStorage::open(&root).expect("open product storage without audit");
    let mut without_audit = ControlPlane::start(Box::new(storage), Box::new(RecordingPublisher))
        .expect("start Control Plane without an audit adapter");
    let mut port = NoProviderCalls::default();
    let unavailable = without_audit
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &allowed_policy,
            &evidence,
            &origin,
            &mut port,
        )
        .expect_err("an unaudited new intent must fail closed");
    assert_eq!(unavailable.public_code(), ErrorCode::ServiceUnavailable);
    assert!(unavailable.retryable());
    assert_eq!((port.lookups, port.applies), (0, 0));
    without_audit.shutdown().expect("close first Control Plane");

    let mut with_audit = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart with immutable audit storage");
    let original = with_audit
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &allowed_policy,
            &evidence,
            &origin,
            &mut port,
        )
        .expect("audit and persist the original intent");
    assert_eq!(original.revision(), 1);
    with_audit.shutdown().expect("close audited Control Plane");

    let storage = SqliteStorage::open(&root).expect("reopen product storage without audit");
    let mut replay_only = ControlPlane::start(Box::new(storage), Box::new(RecordingPublisher))
        .expect("restart without audit storage");
    let denied_policy = RepositoryPublicationPolicy::try_new(
        repository_policy_scope(&scope),
        "example/widget",
        vec![PublicationRequester::User(requester.clone())],
        vec![PublicationRequester::User(requester)],
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        Vec::new(),
        PolicyPermission::Deny,
        true,
        PolicyPermission::Deny,
        1,
    )
    .expect("replacement denied policy");
    let replacement_facts = policy_evidence(&fixture, false, false, 9_999);
    let replay = replay_only
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &denied_policy,
            &replacement_facts,
            &origin,
            &mut port,
        )
        .expect("exact receipt must replay before current policy and audit");
    assert_eq!(replay, original);
    assert_eq!((port.lookups, port.applies), (0, 0));
    replay_only.shutdown().expect("close replay Control Plane");

    let verifier = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("reopen both durable stores");
    let access = AuditScope::repository(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("audit scope")
    .into_access();
    assert_eq!(
        verifier
            .read_audit(&access, 0, 10, 9_999)
            .expect("read retained policy audit")
            .records()
            .len(),
        2,
        "receipt replay must not append a second policy decision or durable-result event",
    );
    verifier.shutdown().expect("shutdown verifier");
    fs::remove_dir_all(&root).expect("remove fixture root");
}

#[test]
fn resume_records_current_policy_before_any_provider_lookup_or_state_transition() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = UserId("usr_00000000000000000000000002".to_owned());
    let publish_policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000042",
    );
    let origin = PublicationPolicyOrigin::local("control-plane-worker").expect("local origin");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let mut publish_port = NoProviderCalls::default();
    control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &publish_policy,
            &policy_evidence(&fixture, true, true, 1_100),
            &origin,
            &mut publish_port,
        )
        .expect("persist policy-guarded intent");

    let denied_policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Deny,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let denied_context = policy_context(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000043",
        2_000,
    );
    let mut denied_port = NoProviderCalls::default();
    let denied = control_plane
        .resume_publication(
            fixture.publication_id(),
            &denied_context,
            &denied_policy,
            &mut denied_port,
        )
        .expect_err("repository write denial must precede provider lookup");
    assert_eq!(denied.public_code(), ErrorCode::PermissionDenied);
    assert_eq!(
        denied.decision().map(PublicationPolicyDecision::rule),
        Some(PublicationPolicyRule::RepositoryWriteDenied),
    );
    assert_eq!((denied_port.lookups, denied_port.applies), (0, 0));

    let allowed_context = policy_context(
        &fixture,
        requester,
        &scope,
        "req_00000000000000000000000044",
        2_001,
    );
    let mut allowed_port = CountingUnknownPort::default();
    let resumed = control_plane
        .resume_publication(
            fixture.publication_id(),
            &allowed_context,
            &publish_policy,
            &mut allowed_port,
        )
        .expect("allowed resume reaches the provider once");
    assert_eq!(resumed.state(), PublicationState::Publishing);
    assert_eq!(resumed.revision(), 3);
    assert_eq!((allowed_port.lookups, allowed_port.applies), (1, 0));

    let access = AuditScope::repository(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("audit scope")
    .into_access();
    let page = control_plane
        .read_audit(&access, 0, 10, 2_001)
        .expect("read publish and resume policy audits");
    assert_eq!(page.records().len(), 5);
    assert_eq!(
        page.records()[2]
            .event()
            .expect("retained denial")
            .action()
            .name(),
        PublicationPolicyRule::RepositoryWriteDenied.as_str(),
    );
    assert_eq!(
        page.records()[3]
            .event()
            .expect("retained allowance")
            .action()
            .name(),
        PublicationPolicyRule::Allowed.as_str(),
    );
    let incomplete = page.records()[4]
        .event()
        .expect("retained incomplete publication result");
    assert_eq!(incomplete.action().name(), "publication.state");
    assert_eq!(incomplete.result_code(), "publication.incomplete");
    assert_eq!(incomplete.outcome(), AuditOutcome::Failed);

    control_plane.shutdown().expect("shutdown Control Plane");
    fs::remove_dir_all(&root).expect("remove fixture root");
}

#[test]
fn policy_guarded_success_preserves_provider_order_and_command_replay() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = UserId("usr_00000000000000000000000002".to_owned());
    let policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000051",
    );
    let evidence = policy_evidence(&fixture, true, true, 1_100);
    let origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let mut provider = DifferentialProvider::new(DifferentialProviderMode::Success);

    let pending = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &policy,
            &evidence,
            &origin,
            &mut provider,
        )
        .expect("persist audited publication intent");
    assert_eq!(pending.state(), PublicationState::Pending);
    assert!(provider.calls.is_empty());

    let published = control_plane
        .resume_publication(
            fixture.publication_id(),
            &policy_context(
                &fixture,
                requester,
                &scope,
                "req_00000000000000000000000052",
                2_000,
            ),
            &policy,
            &mut provider,
        )
        .expect("complete every fake GitHub operation");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(published.revision(), 11);
    assert_eq!(
        published.resource(),
        Some(
            &PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("canonical pull request resource"),
        ),
    );
    assert_eq!(provider.calls, complete_provider_order());
    assert_eq!(
        provider.remote_writes,
        [
            PublicationOperationKind::Branch,
            PublicationOperationKind::PullRequest,
            PublicationOperationKind::IssueComment,
            PublicationOperationKind::CommitStatus,
        ],
    );

    let calls_after_success = provider.calls.clone();
    let writes_after_success = provider.remote_writes.clone();
    let replay = control_plane
        .commit_publication_publish(
            &command,
            fixture.authorization(),
            &policy,
            &evidence,
            &origin,
            &mut provider,
        )
        .expect("return the exact initial command receipt");
    assert_eq!(replay.state(), PublicationState::Pending);
    assert_eq!(replay.revision(), 1);
    assert_eq!(provider.calls, calls_after_success);
    assert_eq!(provider.remote_writes, writes_after_success);

    let audit = control_plane
        .read_audit(&audit_access(&scope), 0, 10, 2_000)
        .expect("read publish and resume decisions");
    assert_eq!(audit.records().len(), 4);
    assert_eq!(
        audit
            .records()
            .iter()
            .map(|record| record.event().expect("retained audit").result_code())
            .collect::<Vec<_>>(),
        [
            "policy.allowed",
            "publication.intent-recorded",
            "policy.allowed",
            "publication.published",
        ],
    );

    control_plane.shutdown().expect("shutdown Control Plane");
    fs::remove_dir_all(&root).expect("remove fixture root");
}

#[test]
fn permission_and_rate_limit_keep_terminal_and_retryable_outcomes_distinct() {
    let fixture = current_publication_fixture();
    let scope = repository_scope();
    let requester = UserId("usr_00000000000000000000000002".to_owned());

    let denied_root = temporary_root();
    let denied_policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let denied_command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000061",
    );
    let denied_origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");
    let mut denied_control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&denied_root),
        Box::new(RecordingPublisher),
    )
    .expect("start denied Control Plane");
    let mut denied_provider = DifferentialProvider::new(DifferentialProviderMode::PermissionDenied);
    denied_control_plane
        .commit_publication_publish(
            &denied_command,
            fixture.authorization(),
            &denied_policy,
            &policy_evidence(&fixture, true, true, 1_100),
            &denied_origin,
            &mut denied_provider,
        )
        .expect("persist denied-provider intent");
    let failed = denied_control_plane
        .resume_publication(
            fixture.publication_id(),
            &policy_context(
                &fixture,
                requester.clone(),
                &scope,
                "req_00000000000000000000000062",
                2_000,
            ),
            &denied_policy,
            &mut denied_provider,
        )
        .expect("persist the secret-safe permission rejection");
    assert_eq!(failed.state(), PublicationState::Failed);
    assert_eq!(failed.revision(), 4);
    assert_eq!(
        denied_provider.calls,
        [
            (ProviderCallKind::Lookup, PublicationOperationKind::Branch,),
            (ProviderCallKind::Apply, PublicationOperationKind::Branch,),
        ],
    );
    assert!(denied_provider.remote_writes.is_empty());
    let calls_after_failure = denied_provider.calls.clone();
    let repeated = denied_control_plane
        .resume_publication(
            fixture.publication_id(),
            &policy_context(
                &fixture,
                requester.clone(),
                &scope,
                "req_00000000000000000000000063",
                2_001,
            ),
            &denied_policy,
            &mut denied_provider,
        )
        .expect("a terminal permission failure performs no later provider call");
    assert_eq!(repeated, failed);
    assert_eq!(denied_provider.calls, calls_after_failure);
    denied_control_plane
        .shutdown()
        .expect("shutdown denied Control Plane");
    fs::remove_dir_all(&denied_root).expect("remove denied fixture root");

    let limited_root = temporary_root();
    let limited_policy = repository_policy(
        &fixture,
        requester.clone(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        5_000,
    );
    let limited_command = api_publish_command(
        &fixture,
        requester.clone(),
        &scope,
        "req_00000000000000000000000064",
    );
    let limited_origin =
        PublicationPolicyOrigin::local("control-plane-http").expect("local origin");
    let mut limited_control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&limited_root),
        Box::new(RecordingPublisher),
    )
    .expect("start rate-limited Control Plane");
    let mut limited_provider = DifferentialProvider::new(DifferentialProviderMode::RateLimitedOnce);
    limited_control_plane
        .commit_publication_publish(
            &limited_command,
            fixture.authorization(),
            &limited_policy,
            &policy_evidence(&fixture, true, true, 1_100),
            &limited_origin,
            &mut limited_provider,
        )
        .expect("persist rate-limit intent");
    let limited = limited_control_plane
        .resume_publication(
            fixture.publication_id(),
            &policy_context(
                &fixture,
                requester.clone(),
                &scope,
                "req_00000000000000000000000065",
                2_000,
            ),
            &limited_policy,
            &mut limited_provider,
        )
        .expect("keep a rate-limited operation pending");
    assert_eq!(limited.state(), PublicationState::Publishing);
    assert_eq!(limited.revision(), 3);
    assert!(limited_provider.remote_writes.is_empty());
    let published = limited_control_plane
        .resume_publication(
            fixture.publication_id(),
            &policy_context(
                &fixture,
                requester,
                &scope,
                "req_00000000000000000000000066",
                2_001,
            ),
            &limited_policy,
            &mut limited_provider,
        )
        .expect("resume after the rate limit clears");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(published.revision(), 12);
    assert_eq!(limited_provider.remote_writes.len(), 4);
    assert_eq!(
        limited_provider
            .calls
            .iter()
            .filter(|call| call.1 == PublicationOperationKind::Branch)
            .copied()
            .collect::<Vec<_>>(),
        [
            (ProviderCallKind::Lookup, PublicationOperationKind::Branch,),
            (ProviderCallKind::Lookup, PublicationOperationKind::Branch,),
            (ProviderCallKind::Apply, PublicationOperationKind::Branch,),
        ],
    );
    limited_control_plane
        .shutdown()
        .expect("shutdown rate-limited Control Plane");
    fs::remove_dir_all(&limited_root).expect("remove rate-limited fixture root");
}

#[test]
fn pull_request_race_reconciles_and_comment_rejection_stops_status() {
    for (mode, request_suffix, expected_state) in [
        (
            DifferentialProviderMode::PullRequestRace,
            71_u64,
            PublicationState::Published,
        ),
        (
            DifferentialProviderMode::CommentRejected,
            73_u64,
            PublicationState::Failed,
        ),
    ] {
        let root = temporary_root();
        let fixture = current_publication_fixture();
        let scope = repository_scope();
        let requester = UserId("usr_00000000000000000000000002".to_owned());
        let policy = repository_policy(
            &fixture,
            requester.clone(),
            PolicyPermission::Allow,
            true,
            PolicyPermission::Allow,
            5_000,
        );
        let command_request = format!("req_000000000000000000000000{request_suffix}");
        let resume_request = format!("req_000000000000000000000000{}", request_suffix + 1);
        let command = api_publish_command(&fixture, requester.clone(), &scope, &command_request);
        let origin = PublicationPolicyOrigin::local("control-plane-http").expect("local origin");
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("start differential Control Plane");
        let mut provider = DifferentialProvider::new(mode);
        control_plane
            .commit_publication_publish(
                &command,
                fixture.authorization(),
                &policy,
                &policy_evidence(&fixture, true, true, 1_100),
                &origin,
                &mut provider,
            )
            .expect("persist differential Publication intent");
        let result = control_plane
            .resume_publication(
                fixture.publication_id(),
                &policy_context(&fixture, requester, &scope, &resume_request, 2_000),
                &policy,
                &mut provider,
            )
            .expect("persist differential provider result");
        assert_eq!(result.state(), expected_state);

        if mode == DifferentialProviderMode::PullRequestRace {
            assert_eq!(result.revision(), 11);
            assert_eq!(provider.calls, complete_provider_order());
            assert_eq!(
                provider.remote_writes,
                [
                    PublicationOperationKind::Branch,
                    PublicationOperationKind::IssueComment,
                    PublicationOperationKind::CommitStatus,
                ],
                "the reconciled PR belongs to the Publication without another local write",
            );
        } else {
            assert_eq!(result.revision(), 8);
            assert_eq!(
                provider.calls,
                complete_provider_order()[..6],
                "comment rejection must stop before commit-status lookup or apply",
            );
            assert_eq!(
                provider.remote_writes,
                [
                    PublicationOperationKind::Branch,
                    PublicationOperationKind::PullRequest,
                ],
            );
        }

        control_plane.shutdown().expect("shutdown Control Plane");
        fs::remove_dir_all(&root).expect("remove fixture root");
    }
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

fn complete_provider_order() -> Vec<(ProviderCallKind, PublicationOperationKind)> {
    [
        PublicationOperationKind::Branch,
        PublicationOperationKind::PullRequest,
        PublicationOperationKind::IssueComment,
        PublicationOperationKind::CommitStatus,
    ]
    .into_iter()
    .flat_map(|kind| {
        [
            (ProviderCallKind::Lookup, kind),
            (ProviderCallKind::Apply, kind),
        ]
    })
    .collect()
}

fn audit_access(scope: &RepositoryScope) -> winwincode_audit::AuditAccess {
    AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical audit scope")
    .into_access()
}

fn repository_policy_scope(scope: &RepositoryScope) -> RepositoryPolicyScope {
    RepositoryPolicyScope::try_new(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical repository policy scope")
}

fn repository_policy(
    fixture: &CurrentPublicationFixture,
    requester: UserId,
    repository_write: PolicyPermission,
    require_independent_verification: bool,
    artifact_export: PolicyPermission,
    max_approval_age_millis: u64,
) -> RepositoryPublicationPolicy {
    RepositoryPublicationPolicy::try_new(
        repository_policy_scope(&repository_scope()),
        fixture.authorization().target().repository(),
        vec![PublicationRequester::User(requester)],
        Vec::new(),
        vec![UserId(fixture.authorization().approved_by().to_owned())],
        Vec::new(),
        repository_write,
        require_independent_verification,
        artifact_export,
        max_approval_age_millis,
    )
    .expect("closed repository publication policy")
}

fn policy_evidence(
    fixture: &CurrentPublicationFixture,
    independent_verification: bool,
    artifact_exportable: bool,
    observed_at_millis: u64,
) -> PublicationPolicyEvidence {
    PublicationPolicyEvidence::try_from_current_facts(
        fixture.authorization(),
        independent_verification,
        artifact_exportable,
        observed_at_millis,
    )
    .expect("sealed policy evidence")
}

fn policy_context(
    fixture: &CurrentPublicationFixture,
    requester: UserId,
    scope: &RepositoryScope,
    request_id: &str,
    observed_at_millis: u64,
) -> PublicationPolicyContext {
    PublicationPolicyContext::try_new(
        PublicationRequester::User(requester),
        RequestId(request_id.to_owned()),
        repository_policy_scope(scope),
        PublicationPolicyOrigin::local("control-plane-worker").expect("local worker origin"),
        policy_evidence(fixture, true, true, observed_at_millis),
    )
    .expect("sealed policy context")
}

fn api_publish_command(
    fixture: &CurrentPublicationFixture,
    requester: UserId,
    scope: &RepositoryScope,
    request_id: &str,
) -> ApiPublicationPublishCommand {
    ApiPublicationPublishCommand {
        actor: Actor::UserActor(UserActor {
            id: requester,
            kind: UserActorKind::User,
        }),
        command: PublicationPublishCommandCommand::PublicationPublish,
        expected_revision: Revision(0),
        payload: PublicationPublishPayload {
            candidate_digest: fixture.authorization().candidate_digest().clone(),
            delivery_id: fixture.authorization().binding().delivery_id().clone(),
            publication_id: fixture.publication_id().clone(),
            target: ApiPublicationTarget {
                base_branch: fixture.authorization().target().base_branch().to_owned(),
                head_branch: fixture.authorization().target().head_branch().to_owned(),
                head_repository: winwincode_domain::GitHubRepositorySlug(
                    fixture
                        .authorization()
                        .target()
                        .head_repository()
                        .to_owned(),
                ),
                provider: PublicationTargetProvider::Github,
                repository: winwincode_domain::GitHubRepositorySlug(
                    fixture.authorization().target().repository().to_owned(),
                ),
            },
        },
        request_id: RequestId(request_id.to_owned()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn temporary_root() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-publication-policy-{}-{nonce}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}
