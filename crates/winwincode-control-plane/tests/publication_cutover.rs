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
    PublicationTarget as ApiPublicationTarget, PublicationTargetProvider, RepositoryScope,
    RepositoryScopeKind, UserActor, UserActorKind,
};
use winwincode_audit::{AuditOutcome, AuditScope};
use winwincode_control_plane::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ControlPlane,
    ControlPlaneConfig, EventPublishError, EventPublisher, NewOutboxEvent, OutboxEvent,
    PreparedPublication,
};
use winwincode_delivery::application::{
    attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
    solution_review::test_support::{
        SolutionComponentFixture, SolutionComponentKindFixture, SolutionConnectionFixture,
        SolutionDiagramEdgeFixture, SolutionDiagramFixture, SolutionDiagramKindFixture,
        SolutionDiagramNodeFixture, SolutionDiagramNodeKindFixture, SolutionFixture,
        SolutionReviewDecisionFixture, SolutionReviewFixture, SolutionReviewTaskProposalFixture,
        prepare_solution_review_fixture, settle_solution_review_fixture,
    },
    stage::{
        AdvanceStageInput, NewStageIdentities, ReviewAttentionSeed, TerminalOutcomeStatus, advance,
        test_support::{
            active_lease_identity, terminal_outcome_metadata, terminal_worker_outcome,
            verify_terminal_outcome,
        },
    },
};
use winwincode_delivery::domain::{
    AcceptanceCriterionId, CandidatePathFact, CandidatePathState, Delivery, DeliveryStage,
    DeliveryStatus, DeliveryTask, DeliveryTaskStatus, FrozenDeliveryCandidate,
    GitHubIssueSourceRef, GitHubPullRequestTargetRef, RepositoryKind, RepositoryRef,
    SessionBinding, SessionBindingId, SessionBindingSourceProvenance, StageRun, StageRunActorType,
    StageRunStatus,
    candidate::{
        CandidateHunkFact,
        test_support::{CandidateFixtureInput, freeze_storage_candidate_fixture},
    },
    delivery_id_for_github_issue_source,
};
use winwincode_delivery::store::{
    AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
    DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, CredentialReferenceId, DeliveryId, DeliveryTaskId,
    ExecutionAckSequence, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId, RequestId, Revision,
    SchemaVersion, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId,
    WorkspaceId,
};
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

fn semantic_review(task_id: DeliveryTaskId, assigned_to: &str) -> SolutionReviewFixture {
    let diagram = |id: &str, kind| SolutionDiagramFixture {
        id: id.to_owned(),
        kind,
        title: format!("{id} publication review"),
        nodes: vec![
            SolutionDiagramNodeFixture {
                id: format!("{id}:delivery"),
                label: "Delivery".into(),
                description: "Owns the reviewed publication facts.".into(),
                kind: SolutionDiagramNodeKindFixture::DeliveryControl,
                trust_boundary: Some("control-plane".into()),
                unresolved: false,
            },
            SolutionDiagramNodeFixture {
                id: format!("{id}:github"),
                label: "GitHub".into(),
                description: "Receives the approved pull request.".into(),
                kind: SolutionDiagramNodeKindFixture::External,
                trust_boundary: Some("provider".into()),
                unresolved: false,
            },
        ],
        edges: vec![SolutionDiagramEdgeFixture {
            id: format!("{id}:publish"),
            from: format!("{id}:delivery"),
            to: format!("{id}:github"),
            label: "publishes".into(),
        }],
    };
    SolutionReviewFixture {
        attention_title: "Review the GitHub publication plan".into(),
        assigned_to: assigned_to.into(),
        solution: SolutionFixture {
            id: "solution:publication-cutover".into(),
            summary: "Publish one exact reviewed Delivery candidate.".into(),
            approach: vec![
                "Freeze the candidate and independent verdict.".into(),
                "Publish only the sealed review package.".into(),
            ],
            components: vec![SolutionComponentFixture {
                id: "component:publication".into(),
                label: "Publication".into(),
                responsibility: "Binds the reviewed Delivery to one pull request.".into(),
                kind: SolutionComponentKindFixture::Component,
                trust_boundary: Some("control-plane".into()),
                unresolved: false,
                repository_path_prefixes: vec!["crates".into()],
            }],
            connections: vec![SolutionConnectionFixture {
                id: "connection:publication-github".into(),
                from: "platform:codex-core".into(),
                to: "component:publication".into(),
                label: "prepares reviewed operations".into(),
            }],
        },
        architecture_diagram: diagram(
            "diagram:publication-architecture",
            SolutionDiagramKindFixture::SystemArchitecture,
        ),
        process_diagram: diagram(
            "diagram:publication-process",
            SolutionDiagramKindFixture::ProcessFlow,
        ),
        risks: vec!["A stale candidate must never reach GitHub.".into()],
        unresolved_items: Vec::new(),
        task_proposals: vec![SolutionReviewTaskProposalFixture {
            id: task_id,
            title: "Prepare the reviewed candidate".into(),
            goal: "Satisfy every current acceptance criterion.".into(),
            acceptance_criterion_ids: vec![
                AcceptanceCriterionId("criterion-required".into()),
                AcceptanceCriterionId("criterion-optional".into()),
            ],
            blocked_by_task_ids: Vec::new(),
        }],
    }
}

fn planning_delivery() -> Delivery {
    let source = GitHubIssueSourceRef {
        schema_version: 3,
        provider: "github".into(),
        kind: "issue".into(),
        repository: "example/widget".into(),
        number: 7,
    };
    let delivery_id = delivery_id_for_github_issue_source(&source).expect("GitHub Delivery id");
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-approved-solution-review.json"
    ))
    .expect("solution-review source fixture")
    .into_snapshot();
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.spec.source_ref = Some(source);
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
    snapshot.status = DeliveryStatus::Planning;
    snapshot.tasks.clear();
    snapshot.stage_runs.truncate(1);
    snapshot.stage_runs[0].delivery_id = delivery_id.clone();
    snapshot.stage_runs[0].status = StageRunStatus::Running;
    snapshot.stage_runs[0].finished_at_millis = None;
    snapshot.session_bindings.truncate(1);
    snapshot.session_bindings[0].delivery_id = delivery_id;
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.stage_runs[0].started_at_millis;
    Delivery::try_from_snapshot(snapshot).expect("active GitHub planning Delivery")
}

fn approved_solution_review() -> (Delivery, DeliveryTaskId) {
    let delivery = planning_delivery();
    let run = &delivery.snapshot().stage_runs[0];
    let binding = &delivery.snapshot().session_bindings[0];
    let worker_session_id = binding
        .worker_session_id
        .clone()
        .expect("planning WorkerSession");
    let lease = active_lease_identity(
        binding.execution_job_id.clone(),
        run.attempt,
        LeaseId(canonical_id("lse", 301)),
        FencingToken("301".into()),
        WorkerId(canonical_id("wrk", 301)),
        WorkerInstanceId(canonical_id("wki", 301)),
        worker_session_id.clone(),
    );
    let finished_at = 1_800_000_000_020;
    let terminal = terminal_worker_outcome(
        run.id.clone(),
        binding.execution_job_id.clone(),
        run.attempt,
        lease.lease_id().clone(),
        lease.fencing_token().clone(),
        lease.worker_id().clone(),
        lease.worker_instance_id().clone(),
        worker_session_id,
        TerminalOutcomeStatus::Succeeded,
        terminal_outcome_metadata(
            binding.codex_thread_id.clone(),
            finished_at,
            ExecutionAckSequence(9),
            Vec::new(),
        ),
    );
    let verified = verify_terminal_outcome(&delivery, &lease, terminal)
        .expect("verified planning terminal outcome");
    let reviewer = canonical_id("usr", 302);
    let task_id = DeliveryTaskId(canonical_id("dtk", 301));
    let prepared = prepare_solution_review_fixture(
        &delivery,
        AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId(canonical_id("psn", 302)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", 302)),
                execution_job_id: ExecutionJobId(canonical_id("job", 302)),
                session_binding_id: SessionBindingId::new("binding-publication-review")
                    .expect("review binding id"),
                attention_item_id: AttentionItemId(canonical_id("att", 302)),
            },
            review: None,
            previous_outcome: Some(verified),
            current_lease: Some(lease),
            rework_authorization: None,
            now_millis: finished_at,
        },
        semantic_review(task_id.clone(), &reviewer),
    )
    .expect("prepared solution review");
    let settled = settle_solution_review_fixture(
        &prepared.transition().delivery,
        &reviewer,
        finished_at + 1,
        SolutionReviewDecisionFixture::Approve {
            comments: Some("Approved for the exact GitHub target.".into()),
        },
    )
    .expect("approved solution review");
    (settled.into_transition().into_delivery(), task_id)
}

#[allow(clippy::too_many_lines)]
fn ready_delivery() -> (Delivery, FrozenDeliveryCandidate) {
    let (approved, task_id) = approved_solution_review();
    let approved_snapshot = approved.snapshot();
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("passing Delivery fixture")
    .into_snapshot();
    snapshot.id = approved.id().clone();
    snapshot.spec = approved_snapshot.spec.clone();
    snapshot.tasks = vec![DeliveryTask {
        schema_version: 3,
        id: task_id.clone(),
        delivery_id: approved.id().clone(),
        title: "Prepare the reviewed candidate".into(),
        goal: "Satisfy every current acceptance criterion.".into(),
        acceptance_criterion_ids: vec![
            AcceptanceCriterionId("criterion-required".into()),
            AcceptanceCriterionId("criterion-optional".into()),
        ],
        blocked_by_task_ids: Vec::new(),
        owner: None,
        status: DeliveryTaskStatus::Completed,
    }];
    snapshot
        .stage_runs
        .clone_from(&approved_snapshot.stage_runs);
    snapshot
        .session_bindings
        .clone_from(&approved_snapshot.session_bindings);
    snapshot
        .attention_items
        .clone_from(&approved_snapshot.attention_items);

    let executor_stage_run_id = StageRunId(canonical_id("run", 303));
    let executor_binding_id =
        SessionBindingId::new("binding-publication-executor").expect("executor binding id");
    snapshot.stage_runs.push(StageRun {
        schema_version: 3,
        id: executor_stage_run_id.clone(),
        delivery_id: approved.id().clone(),
        delivery_task_id: Some(task_id.clone()),
        stage: DeliveryStage::Executing,
        actor_type: StageRunActorType::Codex,
        role: "executor".into(),
        status: StageRunStatus::Succeeded,
        attempt: 1,
        started_at_millis: 1_800_000_000_040,
        finished_at_millis: Some(1_800_000_000_050),
    });
    snapshot.session_bindings.push(SessionBinding {
        schema_version: 3,
        id: executor_binding_id.clone(),
        delivery_id: approved.id().clone(),
        delivery_task_id: Some(task_id.clone()),
        stage_run_id: executor_stage_run_id.clone(),
        product_session_id: ProductSessionId(canonical_id("psn", 303)),
        execution_job_id: ExecutionJobId(canonical_id("job", 303)),
        worker_session_id: Some(WorkerSessionId(canonical_id("wsn", 303))),
        codex_thread_id: Some(CodexThreadId(canonical_id("cdx", 303))),
        worker_id: Some(WorkerId(canonical_id("wrk", 303))),
        worker_instance_id: Some(WorkerInstanceId(canonical_id("wki", 303))),
        lease_id: Some(LeaseId(canonical_id("lse", 303))),
        attempt: 1,
        fencing_token: Some(FencingToken("303".into())),
        source_provenance: SessionBindingSourceProvenance::execution_port(ExecutionMessageId(
            canonical_id("msg", 303),
        )),
        bound_at_millis: 1_800_000_000_041,
    });

    let mut verifier = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("verification fixture")
    .into_snapshot();
    let mut verifier_run = verifier.stage_runs.remove(0);
    verifier_run.delivery_id = approved.id().clone();
    verifier_run.delivery_task_id = Some(task_id.clone());
    verifier_run.id = StageRunId(canonical_id("run", 304));
    verifier_run.started_at_millis = 1_800_000_000_060;
    verifier_run.finished_at_millis = Some(1_800_000_000_070);
    let mut verifier_binding = verifier.session_bindings.remove(0);
    verifier_binding.delivery_id = approved.id().clone();
    verifier_binding.delivery_task_id = Some(task_id);
    verifier_binding.id =
        SessionBindingId::new("binding-publication-verifier").expect("verifier binding id");
    verifier_binding.stage_run_id = verifier_run.id.clone();
    verifier_binding.product_session_id = ProductSessionId(canonical_id("psn", 304));
    verifier_binding.execution_job_id = ExecutionJobId(canonical_id("job", 304));
    verifier_binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", 304)));
    verifier_binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", 304)));
    verifier_binding.bound_at_millis = 1_800_000_000_061;
    snapshot.stage_runs.push(verifier_run.clone());
    snapshot.session_bindings.push(verifier_binding.clone());
    snapshot.evidence = verifier.evidence;
    for evidence in &mut snapshot.evidence {
        evidence.delivery_id = approved.id().clone();
        evidence.stage_run_id = verifier_run.id.clone();
        evidence.session_binding_id = verifier_binding.id.clone();
        evidence.created_at_millis = 1_800_000_000_069;
    }
    snapshot.verdict = verifier.verdict;
    let verdict = snapshot.verdict.as_mut().expect("passing verdict");
    verdict.delivery_id = approved.id().clone();
    for result in &mut verdict.criteria {
        result.delivery_id = approved.id().clone();
        result.evaluated_at_millis = 1_800_000_000_071;
    }
    verdict.produced_at_millis = 1_800_000_000_072;
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::ReadyToDeliver;
    snapshot.updated_at_millis = 1_800_000_000_073;

    let pre_candidate = Delivery::try_from_snapshot(snapshot).expect("candidate lifecycle facts");
    let candidate = freeze_storage_candidate_fixture(
        &pre_candidate,
        &executor_stage_run_id,
        &executor_binding_id,
        CandidateFixtureInput {
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
    let mut snapshot = pre_candidate.into_snapshot();
    for evidence in &mut snapshot.evidence {
        evidence.candidate_ref = candidate.candidate_ref().into();
    }
    let verdict = snapshot.verdict.as_mut().expect("passing verdict");
    verdict.candidate_ref = candidate.candidate_ref().into();
    for result in &mut verdict.criteria {
        result.candidate_ref = candidate.candidate_ref().into();
    }
    (
        Delivery::try_from_snapshot(snapshot).expect("ready publication Delivery"),
        candidate,
    )
}

fn delivered_fixture() -> (Delivery, FrozenDeliveryCandidate) {
    let (ready, candidate) = ready_delivery();
    let approver = canonical_id("usr", 305);
    let advanced = advance(
        &ready,
        AdvanceStageInput {
            expected_revision: ready.revision(),
            product_session_id: ProductSessionId(canonical_id("psn", 305)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", 305)),
                execution_job_id: ExecutionJobId(canonical_id("job", 305)),
                session_binding_id: SessionBindingId::new("binding-delivery-review-unused")
                    .expect("review binding id"),
                attention_item_id: AttentionItemId(canonical_id("att", 305)),
            },
            review: Some(ReviewAttentionSeed {
                title: "Approve the exact publication package".into(),
                context: "candidate-verdict-and-target".into(),
                assigned_to: approver.clone(),
            }),
            previous_outcome: None,
            current_lease: None,
            rework_authorization: None,
            now_millis: 1_800_000_000_080,
        },
    )
    .expect("DeliveryReview advance");
    let review = advanced.delivery;
    let attention = review
        .snapshot()
        .attention_items
        .iter()
        .find(|item| {
            item.item_type == winwincode_delivery::domain::AttentionItemType::DeliveryApproval
        })
        .expect("Delivery approval Attention");
    let delivered = resolve_attention(
        &review,
        ResolveAttentionInput {
            expected_revision: review.revision(),
            attention_item_id: attention.id.clone(),
            stage_run_id: attention.stage_run_id.clone().expect("DeliveryReview run"),
            expected_context: attention.context.clone(),
            actor: approver,
            decision: AttentionDecision::Resolved,
            resolution: "approved exact candidate, verdict, package, and target".into(),
            now_millis: 1_800_000_000_081,
        },
    )
    .expect("Delivery approval settlement")
    .into_delivery();
    assert_eq!(delivered.snapshot().status, DeliveryStatus::Delivered);
    let mut snapshot = delivered.into_snapshot();
    snapshot.revision = 1;
    (
        Delivery::try_from_snapshot(snapshot).expect("seedable delivered fixture"),
        candidate,
    )
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
