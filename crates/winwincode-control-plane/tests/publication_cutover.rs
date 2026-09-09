// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::too_many_lines,
    reason = "the black-box cutover tracer keeps one Delivery, Artifact, GitHub, recovery, and audit proof together"
)]

#[path = "support/github_fixture.rs"]
mod github_fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, PublicationPublishCommand as ApiPublicationPublishCommand,
    PublicationPublishCommandCommand, PublicationPublishPayload,
    PublicationTarget as ApiPublicationTarget, PublicationTargetProvider,
};
use winwincode_audit::{AuditOutcome, AuditScope};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ControlPlane,
    ControlPlaneConfig, EventPublishError, EventPublisher, NewOutboxEvent, OutboxEvent,
    PreparedPublication,
};
use winwincode_delivery::domain::{
    CandidatePathFact, CandidatePathState, Delivery, DeliveryStage, DeliveryStatus,
    DeliveryTaskStatus, FrozenDeliveryCandidate, GitHubIssueSourceRef, GitHubPullRequestTargetRef,
    RepositoryKind, RepositoryRef,
    candidate::{
        CandidateHunkFact,
        test_support::{CandidateFixtureInput, freeze_candidate_fixture},
    },
    delivery_id_for_github_issue_source,
};
use winwincode_delivery::store::{
    AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
    DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
};
use winwincode_domain::{
    CredentialReferenceId, DeliveryId, OrganizationId, ProjectId, PublicationId, RepositoryId,
    RequestId, Revision, SchemaVersion, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_publication::{
    GitHubAdapterConfig, GitHubPublicationAdapter, PolicyPermission,
    PublicationEnterpriseAttribution, PublicationPolicyContext, PublicationPolicyEvidence,
    PublicationPolicyOrigin, PublicationRequester, PublicationResourceFact,
    PublicationResourceKind, PublicationState, RepositoryPolicyScope, RepositoryPublicationPolicy,
};
use winwincode_storage::{
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

use github_fixture::{FixtureCredentialResolver, FixtureGitHub, TOKEN};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-publication-cutover-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", 301)),
        workspace_id: WorkspaceId(canonical_id("wsp", 301)),
        project_id: ProjectId(canonical_id("prj", 301)),
        repository_id: RepositoryId(canonical_id("rep", 301)),
    }
}

fn publication_policy(
    scope: &RepositoryScope,
    prepared: &PreparedPublication,
    requester: &UserId,
) -> RepositoryPublicationPolicy {
    RepositoryPublicationPolicy::try_new(
        repository_policy_scope(scope),
        prepared.authorization().target().repository(),
        vec![PublicationRequester::User(requester.clone())],
        Vec::new(),
        vec![UserId(prepared.authorization().approved_by().to_owned())],
        Vec::new(),
        PolicyPermission::Allow,
        true,
        PolicyPermission::Allow,
        10_000,
    )
    .expect("closed Publication policy")
}

fn repository_policy_scope(scope: &RepositoryScope) -> RepositoryPolicyScope {
    RepositoryPolicyScope::try_new(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical Publication policy scope")
}

fn publish_command(
    scope: &RepositoryScope,
    prepared: &PreparedPublication,
    requester: UserId,
    publication_id: PublicationId,
    request_id: RequestId,
) -> ApiPublicationPublishCommand {
    ApiPublicationPublishCommand {
        actor: Actor::UserActor(UserActor {
            id: requester,
            kind: UserActorKind::User,
        }),
        command: PublicationPublishCommandCommand::PublicationPublish,
        expected_revision: Revision(0),
        payload: PublicationPublishPayload {
            candidate_digest: prepared.authorization().candidate_digest().clone(),
            delivery_id: prepared.authorization().binding().delivery_id().clone(),
            publication_id,
            target: ApiPublicationTarget {
                base_branch: prepared.authorization().target().base_branch().to_owned(),
                head_branch: prepared.authorization().target().head_branch().to_owned(),
                head_repository: winwincode_domain::GitHubRepositorySlug(
                    prepared
                        .authorization()
                        .target()
                        .head_repository()
                        .to_owned(),
                ),
                provider: PublicationTargetProvider::Github,
                repository: winwincode_domain::GitHubRepositorySlug(
                    prepared.authorization().target().repository().to_owned(),
                ),
            },
        },
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn policy_evidence(
    prepared: &PreparedPublication,
    observed_at_millis: u64,
) -> PublicationPolicyEvidence {
    PublicationPolicyEvidence::try_from_current_facts(
        prepared.authorization(),
        true,
        true,
        observed_at_millis,
    )
    .expect("current Publication policy evidence")
}

fn policy_context(
    scope: &RepositoryScope,
    prepared: &PreparedPublication,
    requester: UserId,
    request_id: RequestId,
    observed_at_millis: u64,
) -> PublicationPolicyContext {
    PublicationPolicyContext::try_new(
        PublicationRequester::User(requester),
        request_id,
        repository_policy_scope(scope),
        PublicationPolicyOrigin::local("control-plane-publication-worker")
            .expect("closed Publication origin"),
        policy_evidence(prepared, observed_at_millis),
    )
    .expect("sealed Publication policy context")
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

fn ready_delivery() -> (Delivery, FrozenDeliveryCandidate) {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical ready Delivery fixture")
    .into_snapshot();
    let delivery_id = delivery_id_for_github_issue_source(&GitHubIssueSourceRef {
        schema_version: 3,
        provider: "github".into(),
        kind: "issue".into(),
        repository: "example/widget".into(),
        number: 7,
    })
    .expect("GitHub Delivery id");
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.spec.source_ref = Some(GitHubIssueSourceRef {
        schema_version: 3,
        provider: "github".into(),
        kind: "issue".into(),
        repository: "example/widget".into(),
        number: 7,
    });
    snapshot.spec.publication_target = Some(GitHubPullRequestTargetRef {
        schema_version: 3,
        provider: "github".into(),
        kind: "pull-request".into(),
        repository: "example/widget".into(),
        base_branch: "main".into(),
        head_repository: "example/widget".into(),
        head_branch: "winwincode/delivery".into(),
    });
    snapshot.spec.repository = RepositoryRef {
        schema_version: 3,
        kind: RepositoryKind::GitHub,
        locator: "example/widget".into(),
    };
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::ReadyToDeliver;
    for task in &mut snapshot.tasks {
        task.delivery_id = delivery_id.clone();
        task.status = DeliveryTaskStatus::Completed;
    }
    for run in &mut snapshot.stage_runs {
        run.delivery_id = delivery_id.clone();
        run.stage = DeliveryStage::Executing;
        run.role = "executor".into();
    }
    for binding in &mut snapshot.session_bindings {
        binding.delivery_id = delivery_id.clone();
        binding.execution_profile = Some("executor".into());
    }
    let binding_work_run_id = winwincode_domain::WorkRunId(canonical_id("wrn", 303));
    snapshot.session_bindings[0].id =
        winwincode_delivery::domain::SessionBindingId::new("binding-executor-publication")
            .expect("executor binding id");
    snapshot.session_bindings[0].work_run_id = binding_work_run_id.clone();
    snapshot.work_run_aggregate.runs[0].id = binding_work_run_id.clone();
    snapshot.evidence[0].work_run_id = binding_work_run_id;
    snapshot.evidence[0].session_binding_id = snapshot.session_bindings[0].id.clone();
    for evidence in &mut snapshot.evidence {
        evidence.delivery_id = delivery_id.clone();
    }
    let verdict = snapshot.verdict.as_mut().expect("passing verdict");
    verdict.delivery_id = delivery_id.clone();
    for result in &mut verdict.criteria {
        result.delivery_id = delivery_id.clone();
    }
    let delivery = Delivery::try_from_snapshot(snapshot).expect("canonical ready Delivery");
    let run_id = delivery.snapshot().session_bindings[0].work_run_id.clone();
    let binding_id = delivery.snapshot().session_bindings[0].id.clone();
    let candidate = freeze_candidate_fixture(
        &delivery,
        &run_id,
        &binding_id,
        CandidateFixtureInput {
            finished_at_millis: 1_800_000_000_020,
            base_commit_id: "0123456789012345678901234567890123456789".into(),
            base_tree_id: "1".repeat(40),
            candidate_commit_id: "a".repeat(40),
            candidate_tree_id: "3".repeat(40),
            diff_sha256: "a".repeat(64),
            changed_paths: vec![CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4".repeat(40)),
            }],
            changed_hunks: vec![CandidateHunkFact {
                file_path: "src/invitation.rs".into(),
                hunk_sha256: "b".repeat(64),
                source_hunk_sha256: None,
            }],
            artifact_ref: canonical_id("art", 303),
            artifact_digest: Sha256Digest(format!("sha256:{}", "9".repeat(64))),
            terminal_event_sequence: 12,
        },
    );
    let mut snapshot = delivery.into_snapshot();
    let verdict = snapshot.verdict.as_mut().expect("current verdict");
    verdict.candidate_ref = candidate.candidate_ref().into();
    for criterion in &mut verdict.criteria {
        criterion.candidate_ref = candidate.candidate_ref().into();
    }
    for evidence in &mut snapshot.evidence {
        evidence.candidate_ref = candidate.candidate_ref().into();
    }
    snapshot.stage_runs.clear();
    let delivery = Delivery::try_from_snapshot(snapshot).expect("exact candidate verdict");
    winwincode_delivery::projection::project_delivery_detail(
        winwincode_delivery::projection::ProjectionInput::new(&delivery).with_candidate(&candidate),
    )
    .expect("ready candidate projection");
    (delivery, candidate)
}

fn delivered_fixture() -> (Delivery, FrozenDeliveryCandidate) {
    use winwincode_delivery::application::attention::{
        AttentionDecision, ResolveAttentionInput, resolve_attention,
    };
    let (ready, candidate) = ready_delivery();
    let approval =
        winwincode_delivery::application::verdict::test_support::delivery_approval_fixture(
            &ready,
            1_800_000_000_080,
        );
    assert!(approval.work_run_id.is_none());
    let mut snapshot = ready.into_snapshot();
    snapshot.attention_items.push(approval.clone());
    snapshot.status = DeliveryStatus::NeedsAttention;
    let pending = Delivery::try_from_snapshot(snapshot).expect("pending canonical approval");
    let settled = resolve_attention(
        &pending,
        ResolveAttentionInput {
            expected_revision: pending.revision(),
            attention_item_id: approval.id,
            work_run_id: None,
            expected_context: approval.context,
            actor: canonical_id("usr", 305),
            decision: AttentionDecision::Resolved,
            resolution: "approved exact candidate, verdict, package, and target".into(),
            now_millis: 1_800_000_000_081,
        },
    )
    .expect("resolve canonical publication approval")
    .into_delivery();
    let mut snapshot = settled.into_snapshot();
    snapshot.revision = 1;
    let delivery = Delivery::try_from_snapshot(snapshot).expect("seedable delivered fixture");
    (delivery, candidate)
}

#[derive(Default)]
struct CapturingJournal {
    publication: Mutex<Option<AtomicPublication>>,
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

fn seed_delivery(root: &Path, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 306)),
            request_digest: "b".repeat(64),
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
        .expect("seed publication")
    else {
        panic!("seed must create one Delivery journal");
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
    let mut storage = SqliteStorage::open(root).expect("seed SQLite");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"publication-seed-actor".to_vec())
                        .expect("seed actor"),
                    ReceiptScopeKey::from_encoded(b"publication-seed-scope".to_vec())
                        .expect("seed scope"),
                    RequestId(canonical_id("req", 307)),
                )
                .expect("seed receipt identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed Delivery bytes"),
                vec![NewOutboxEvent::internal(
                    "publication-seed-event",
                    "delivery.seeded",
                    b"seed".to_vec(),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed transaction");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    Box::new(storage).close().expect("close seed storage");
}

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[test]
fn delivered_github_candidate_prepares_one_exact_secret_safe_review_package_artifact() {
    let root = temporary_root();
    let scope = repository_scope();
    let (delivery, candidate) = delivered_fixture();
    seed_delivery(&root, &delivery);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let requester = UserId(canonical_id("usr", 306));

    let prepared = control_plane
        .prepare_publication(&scope, &candidate, &requester)
        .expect("prepare review package and Publication authority");
    let replay = control_plane
        .prepare_publication(&scope, &candidate, &requester)
        .expect("exact preparation replay");

    assert_eq!(prepared, replay);
    assert_eq!(
        prepared.authorization().binding().delivery_id(),
        delivery.id()
    );
    assert_eq!(
        prepared.authorization().artifact_id(),
        prepared.review_package_artifact_id().0
    );
    assert_eq!(
        prepared.authorization().artifact_digest(),
        prepared.review_package_digest()
    );
    let package: serde_json::Value =
        serde_json::from_slice(prepared.review_package_bytes()).expect("review package JSON");
    assert_eq!(package["protocol"], "winwincode.github-review-package.v1");
    assert_eq!(package["delivery"]["deliveryId"], delivery.id().0);
    assert_eq!(
        package["candidate"]["candidateRef"],
        candidate.candidate_ref()
    );
    assert_eq!(package["approval"]["approvedBy"], canonical_id("usr", 305));
    let encoded =
        String::from_utf8(prepared.review_package_bytes().to_vec()).expect("review package UTF-8");
    for forbidden in [
        "fencingToken",
        "leaseId",
        "workerId",
        "workerInstanceId",
        "workerSessionId",
        "codexThreadId",
        "rawContext",
        "rawResolution",
        "ghp_",
        "github-token",
    ] {
        assert!(!encoded.contains(forbidden), "leaked {forbidden}");
    }

    control_plane.shutdown().expect("shutdown Control Plane");
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn publication_preparation_rejects_an_unapproved_delivery_before_creating_an_artifact() {
    let root = temporary_root();
    let scope = repository_scope();
    let (ready, candidate) = ready_delivery();
    seed_delivery(&root, &ready);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let requester = UserId(canonical_id("usr", 306));

    let error = control_plane
        .prepare_publication(&scope, &candidate, &requester)
        .expect_err("ReadyToDeliver is not a settled human publication approval");
    assert!(
        error.to_string().contains("no exact publishable approval"),
        "unexpected preparation error: {error}",
    );
    control_plane.shutdown().expect("shutdown Control Plane");

    let catalog = Connection::open(
        root.join("artifact-catalog")
            .join("artifact-catalog.sqlite3"),
    )
    .expect("open Artifact catalog");
    let count: i64 = catalog
        .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
        .expect("count Artifact metadata");
    assert_eq!(count, 0, "rejected preparation must create no Artifact");
    drop(catalog);
    fs::remove_dir_all(root).expect("remove fixture root");
}

#[test]
fn publication_rejects_ambiguous_or_foreign_approval_without_an_artifact() {
    for invalid in ["foreign-assignee", "ambiguous", "execution-bound"] {
        let root = temporary_root();
        let scope = repository_scope();
        let (delivery, candidate) = delivered_fixture();
        assert!(
            delivery.snapshot().stage_runs.is_empty(),
            "publication has no stage authority"
        );
        let mut snapshot = delivery.into_snapshot();
        let approval = snapshot.attention_items.last_mut().expect("approval");
        if invalid == "ambiguous" {
            let mut duplicate = approval.clone();
            duplicate.id = winwincode_domain::AttentionItemId(canonical_id("att", 999));
            snapshot.attention_items.push(duplicate);
        } else if invalid == "execution-bound" {
            approval.work_run_id = Some(snapshot.work_run_aggregate.runs[0].id.clone());
        } else {
            approval.assigned_to = Some(canonical_id("usr", 999));
        }
        let delivery = Delivery::try_from_snapshot(snapshot)
            .expect("valid snapshot with conflicting approval");
        seed_delivery(&root, &delivery);
        let mut plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("start");
        plane
            .prepare_publication(&scope, &candidate, &UserId(canonical_id("usr", 306)))
            .expect_err("approval ambiguity or foreign assignee must be rejected");
        plane.shutdown().expect("shutdown");
        let db = Connection::open(root.join("artifact-catalog/artifact-catalog.sqlite3"))
            .expect("catalog");
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
            .expect("artifact count");
        assert_eq!(count, 0);
        drop(db);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

#[test]
fn approved_delivery_recovers_one_partial_github_publication_and_audits_each_result() {
    let root = temporary_root();
    let scope = repository_scope();
    let (delivery, candidate) = delivered_fixture();
    seed_delivery(&root, &delivery);
    let github = FixtureGitHub::start();
    github.drop_issue_comment_response_once();
    let requester = UserId(canonical_id("usr", 306));
    let publication_id = PublicationId(canonical_id("pub", 301));

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("start Control Plane");
    let prepared = control_plane
        .prepare_publication(&scope, &candidate, &requester)
        .expect("prepare approved review package");
    let policy = publication_policy(&scope, &prepared, &requester);
    let command = publish_command(
        &scope,
        &prepared,
        requester.clone(),
        publication_id.clone(),
        RequestId(canonical_id("req", 308)),
    );
    let origin = PublicationPolicyOrigin::local("control-plane-publication-http")
        .expect("closed Publication origin");
    let first_observed_at = 1_800_000_000_100;
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId(canonical_id("crd", 301)),
        github.base_url.clone(),
    )
    .expect("loopback GitHub adapter config");
    let mut adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver);
    let attribution = PublicationEnterpriseAttribution::try_new(
        &repository_policy_scope(&scope),
        prepared.authorization().binding().delivery_id().clone(),
        candidate.producer_product_session_id().clone(),
        requester.clone(),
    )
    .expect("sealed Publication enterprise attribution");

    let pending = control_plane
        .commit_publication_publish(
            &command,
            prepared.authorization(),
            &attribution,
            &policy,
            &policy_evidence(&prepared, first_observed_at),
            &origin,
            &mut adapter,
        )
        .expect("persist approved Publication intent");
    assert_eq!(pending.state(), PublicationState::Pending);

    let interrupted = control_plane
        .resume_publication(
            &publication_id,
            &policy_context(
                &scope,
                &prepared,
                requester.clone(),
                RequestId(canonical_id("req", 309)),
                first_observed_at + 1,
            ),
            &policy,
            &mut adapter,
        )
        .expect("persist unknown result after dropped GitHub response");
    assert_eq!(interrupted.state(), PublicationState::Publishing);
    assert_eq!(
        github.snapshot().writes,
        ["branch", "pull-request", "issue-comment"],
    );
    let first_audit = control_plane
        .read_audit(&audit_access(&scope), 0, 20, first_observed_at + 1)
        .expect("read incomplete Publication audit");
    let incomplete = first_audit
        .records()
        .last()
        .and_then(winwincode_audit::AuditRecord::event)
        .expect("retained incomplete result");
    assert_eq!(incomplete.result_code(), "publication.incomplete");
    assert_eq!(incomplete.outcome(), AuditOutcome::Failed);

    control_plane
        .shutdown()
        .expect("shutdown after interruption");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart Control Plane on the same durable root");
    let config = GitHubAdapterConfig::try_new(
        CredentialReferenceId(canonical_id("crd", 301)),
        github.base_url.clone(),
    )
    .expect("restart GitHub adapter config");
    let mut restarted_adapter = GitHubPublicationAdapter::new(config, FixtureCredentialResolver);
    let published = restarted
        .resume_publication(
            &publication_id,
            &policy_context(
                &scope,
                &prepared,
                requester,
                RequestId(canonical_id("req", 310)),
                first_observed_at + 2,
            ),
            &policy,
            &mut restarted_adapter,
        )
        .expect("reconcile remote comment and finish Publication");
    assert_eq!(published.state(), PublicationState::Published);
    assert_eq!(published.revision(), 12);
    assert_eq!(
        published.resource(),
        Some(
            &PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                17,
            )
            .expect("canonical GitHub pull request"),
        ),
    );
    let remote = github.snapshot();
    assert_eq!(
        remote.writes,
        ["branch", "pull-request", "issue-comment", "commit-status"],
        "restart must reconcile the written comment instead of duplicating it",
    );
    assert!(
        remote
            .authorizations
            .iter()
            .all(|authorization| { authorization.as_deref() == Some(&format!("Bearer {TOKEN}")) })
    );

    let audit = restarted
        .read_audit(&audit_access(&scope), 0, 20, first_observed_at + 2)
        .expect("read complete Publication audit");
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
            "publication.incomplete",
            "policy.allowed",
            "publication.published",
        ],
    );
    assert_eq!(
        audit
            .records()
            .last()
            .and_then(winwincode_audit::AuditRecord::event)
            .expect("retained published result")
            .outcome(),
        AuditOutcome::Succeeded,
    );

    restarted
        .shutdown()
        .expect("shutdown recovered Control Plane");
    fs::remove_dir_all(root).expect("remove fixture root");
}
