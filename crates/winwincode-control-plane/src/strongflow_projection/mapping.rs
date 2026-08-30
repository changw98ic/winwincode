// SPDX-License-Identifier: Apache-2.0

//! Lossless mapping from Delivery-owned read models to generated transport DTOs.

use winwincode_api::generated as api;
use winwincode_delivery::{
    domain::{
        AttentionItemStatus, AttentionItemType, CriterionVerdict, DeliveryStage,
        DeliveryStatus as DomainDeliveryStatus, DeliveryTaskStatus as DomainTaskStatus,
        EvidenceRefType, RepositoryKind, SessionBindingSourceKind, StageRunActorType,
        StageRunStatus,
    },
    projection::{self as delivery_projection, runtime as runtime_projection},
};
use winwincode_domain::{
    Count, GitHubRepositorySlug, Instant, Revision, SchemaVersion, SessionBindingSourceIdentity,
    SessionBindingSourceIdentityKind, SessionIdentity, Sha256Digest,
};

use super::{
    StrongFlowProjectionError, application::EstablishedDeliveryRead,
    sources::TrustedProductSessionRuntimeSession,
};

pub(super) fn cursor(
    read: &EstablishedDeliveryRead,
) -> Result<api::StrongFlowReadCursor, StrongFlowProjectionError> {
    super::application::generated_cursor(&read.cursor)
}

pub(super) fn delivery_detail(
    read: &EstablishedDeliveryRead,
) -> Result<api::DeliveryDetailProjection, StrongFlowProjectionError> {
    let source = &read.detail;
    let read_cursor = cursor(read)?;
    let publication = publication(read, &read_cursor)?;
    Ok(api::DeliveryDetailProjection {
        kind: api::DeliveryDetailProjectionKind::DeliveryDetail,
        schema_version: SchemaVersion::WinwincodeV1,
        read_cursor,
        delivery_id: source.delivery_id().clone(),
        delivery_revision: revision(source.delivery_revision(), "delivery revision")?,
        ownership: ownership(&read.cursor.scope),
        status: delivery_status(source.status()),
        requirements: requirements(source.requirements())?,
        solution_review: source.solution_review().map(solution_review).transpose()?,
        stages: source
            .stages()
            .iter()
            .map(stage)
            .collect::<Result<_, _>>()?,
        tasks: source.tasks().iter().map(task).collect(),
        attention: source
            .attention()
            .iter()
            .map(attention)
            .collect::<Result<_, _>>()?,
        evidence: source
            .evidence()
            .iter()
            .map(evidence)
            .collect::<Result<_, _>>()?,
        current_candidate: source.current_candidate().map(candidate).transpose()?,
        verdict: source.verdict().map(verdict).transpose()?,
        publication,
    })
}

fn ownership(scope: &api::RepositoryScope) -> api::DeliveryOwnershipProjection {
    api::DeliveryOwnershipProjection {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn requirements(
    source: &delivery_projection::RequirementsProjection,
) -> Result<api::DeliveryRequirementsProjection, StrongFlowProjectionError> {
    let spec = source.spec();
    let publication_target = spec
        .publication_target()
        .map(publication_target)
        .transpose()?;
    let source_ref = spec
        .source_ref()
        .map(|source| {
            if source.provider != "github" || source.kind != "issue" {
                return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
                    "the current issue source is not canonical".to_owned(),
                ));
            }
            Ok(api::DeliverySourceIssueProjection {
                kind: api::DeliverySourceIssueProjectionKind::Issue,
                provider: api::DeliverySourceIssueProjectionProvider::Github,
                repository: source.repository.clone(),
                number: integer(source.number, "issue number")?,
            })
        })
        .transpose()?;
    Ok(api::DeliveryRequirementsProjection {
        delivery_spec_id: spec.id().0.clone(),
        delivery_spec_revision: revision(spec.revision(), "spec revision")?,
        title: spec.title().to_owned(),
        goal: spec.goal().to_owned(),
        scope: spec.scope().to_vec(),
        out_of_scope: spec.out_of_scope().to_vec(),
        constraints: spec.constraints().to_vec(),
        acceptance_criteria: spec
            .acceptance_criteria()
            .iter()
            .map(|criterion| api::DeliveryAcceptanceCriterionProjection {
                id: criterion.id().0.clone(),
                description: criterion.description().to_owned(),
                verification_method: criterion.verification_method().map(str::to_owned),
                required: criterion.required(),
            })
            .collect(),
        source_ref,
        publication_target,
        repository: api::DeliveryRepositoryProjection {
            kind: match spec.repository().kind {
                RepositoryKind::LocalGit => "local-git",
                RepositoryKind::GitHub => "github",
            }
            .to_owned(),
            locator: spec.repository().locator.clone(),
        },
        base_revision: spec.base_revision().to_owned(),
        max_rework_attempts: integer(spec.max_rework_attempts(), "max rework attempts")?,
    })
}

fn stage(
    source: &delivery_projection::StageProjection,
) -> Result<api::DeliveryStageProjection, StrongFlowProjectionError> {
    Ok(api::DeliveryStageProjection {
        id: source.id().clone(),
        delivery_task_id: source.delivery_task_id().cloned(),
        stage: stage_name(source.stage()).to_owned(),
        actor_type: match source.actor_type() {
            StageRunActorType::Codex => "codex",
            StageRunActorType::Human => "human",
        }
        .to_owned(),
        role: source.role().to_owned(),
        status: stage_status(source.status()).to_owned(),
        attempt: integer(source.attempt(), "stage attempt")?,
        started_at: millis_to_instant(source.started_at())?,
        finished_at: source.finished_at().map(millis_to_instant).transpose()?,
        session_binding: source
            .session_binding()
            .map(|binding| session_binding(binding, source.id(), source.attempt()))
            .transpose()?,
    })
}

fn session_binding(
    source: &delivery_projection::SessionBindingProjection,
    stage_run_id: &winwincode_domain::StageRunId,
    stage_attempt: u64,
) -> Result<api::DeliveryStageSessionBindingProjection, StrongFlowProjectionError> {
    if source.attempt() != stage_attempt {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "SessionBinding attempt does not match its StageRun".to_owned(),
        ));
    }

    let authority_presence = [
        source.worker_id().is_some(),
        source.worker_instance_id().is_some(),
        source.lease_id().is_some(),
        source.fencing_token().is_some(),
    ];
    if authority_presence.iter().any(|present| *present)
        && !authority_presence.iter().all(|present| *present)
    {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "SessionBinding has partial persisted Worker authority".to_owned(),
        ));
    }

    let complete = authority_presence.iter().all(|present| *present)
        && source.worker_session_id().is_some()
        && source.codex_thread_id().is_some();
    if !complete {
        return Ok(api::DeliveryStageSessionBindingProjection {
            attempt: None,
            binding_id: source.binding_id().0.clone(),
            bound_at: millis_to_instant(source.bound_at())?,
            codex_thread_id: None,
            execution_job_id: source.execution_job_id().clone(),
            fencing_token: None,
            lease_id: None,
            product_session_id: source.product_session_id().clone(),
            session_identity: None,
            source_identity: None,
            stage_run_id: None,
            worker_id: None,
            worker_session_id: None,
        });
    }

    let worker_id = source.worker_id().cloned().expect("complete authority");
    let worker_instance_id = source
        .worker_instance_id()
        .cloned()
        .expect("complete authority");
    let lease_id = source.lease_id().cloned().expect("complete authority");
    let fencing_token = source.fencing_token().cloned().expect("complete authority");
    let worker_session_id = source
        .worker_session_id()
        .cloned()
        .expect("complete identity");
    let codex_thread_id = source
        .codex_thread_id()
        .cloned()
        .expect("complete identity");
    let attempt = integer(source.attempt(), "session binding attempt")?;
    let source_identity = match source.source_provenance().kind() {
        SessionBindingSourceKind::ExecutionPort => SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: lease_id.clone(),
            worker_id: worker_id.clone(),
            worker_instance_id,
            worker_session_id: worker_session_id.clone(),
        },
        SessionBindingSourceKind::DeliveryAdvance | SessionBindingSourceKind::LegacyMigration => {
            return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
                "SessionBinding source is not an accepted execution-worker authority".to_owned(),
            ));
        }
    };
    let session_identity = SessionIdentity {
        codex_thread_id: codex_thread_id.clone(),
        product_session_id: source.product_session_id().clone(),
        stage_run_id: Some(stage_run_id.clone()),
        worker_session_id: worker_session_id.clone(),
    };
    Ok(api::DeliveryStageSessionBindingProjection {
        attempt: Some(attempt),
        binding_id: source.binding_id().0.clone(),
        bound_at: millis_to_instant(source.bound_at())?,
        codex_thread_id: Some(codex_thread_id),
        execution_job_id: source.execution_job_id().clone(),
        fencing_token: Some(fencing_token.0),
        lease_id: Some(lease_id),
        product_session_id: source.product_session_id().clone(),
        session_identity: Some(session_identity),
        source_identity: Some(source_identity),
        stage_run_id: Some(stage_run_id.clone()),
        worker_id: Some(worker_id),
        worker_session_id: Some(worker_session_id),
    })
}

fn task(source: &delivery_projection::DeliveryTaskProjection) -> api::DeliveryTaskDetailProjection {
    api::DeliveryTaskDetailProjection {
        id: source.id().clone(),
        title: source.title().to_owned(),
        goal: source.goal().to_owned(),
        acceptance_criterion_ids: source
            .acceptance_criterion_ids()
            .iter()
            .map(|id| id.0.clone())
            .collect(),
        blocked_by_task_ids: source.blocked_by_task_ids().to_vec(),
        owner: source.owner().map(str::to_owned),
        status: match source.status() {
            DomainTaskStatus::Pending => api::DeliveryTaskStatus::Pending,
            DomainTaskStatus::Active => api::DeliveryTaskStatus::Active,
            DomainTaskStatus::Blocked => api::DeliveryTaskStatus::Blocked,
            DomainTaskStatus::Verifying => api::DeliveryTaskStatus::Verifying,
            DomainTaskStatus::Completed => api::DeliveryTaskStatus::Completed,
            DomainTaskStatus::Failed => api::DeliveryTaskStatus::Failed,
        },
        stage_run_ids: source.stage_run_ids().to_vec(),
        evidence_refs: source.evidence_refs().to_vec(),
    }
}

fn attention(
    source: &delivery_projection::AttentionItemProjection,
) -> Result<api::DeliveryAttentionProjection, StrongFlowProjectionError> {
    Ok(api::DeliveryAttentionProjection {
        id: source.id().clone(),
        delivery_spec_id: source.delivery_spec_id().0.clone(),
        stage_run_id: source.stage_run_id().cloned(),
        type_value: match source.item_type() {
            AttentionItemType::RequirementQuestion => "requirement_question",
            AttentionItemType::DecisionRequired => "decision_required",
            AttentionItemType::VerificationBlocked => "verification_blocked",
            AttentionItemType::ScopeChange => "scope_change",
            AttentionItemType::DeliveryApproval => "delivery_approval",
        }
        .to_owned(),
        title: source.title().to_owned(),
        options: source
            .options()
            .iter()
            .map(|option| api::DeliveryAttentionOptionProjection {
                id: option.id().to_owned(),
                label: option.label().to_owned(),
                description: option.description().to_owned(),
            })
            .collect(),
        assigned_to: source.assigned_to().map(actor_id).transpose()?,
        blocking: source.blocking(),
        status: match source.status() {
            AttentionItemStatus::Open => "open",
            AttentionItemStatus::Resolved => "resolved",
            AttentionItemStatus::Dismissed => "dismissed",
        }
        .to_owned(),
        resolution_summary: source.resolution_summary().map(str::to_owned),
        resolved_by: source.resolved_by().map(actor_id).transpose()?,
        created_at: millis_to_instant(source.created_at())?,
        resolved_at: source.resolved_at().map(millis_to_instant).transpose()?,
    })
}

fn evidence(
    source: &delivery_projection::EvidenceProjection,
) -> Result<api::DeliveryEvidenceProjection, StrongFlowProjectionError> {
    Ok(api::DeliveryEvidenceProjection {
        id: source.id().clone(),
        delivery_spec_id: source.delivery_spec_id().0.clone(),
        delivery_spec_revision: revision(
            source.delivery_spec_revision(),
            "evidence spec revision",
        )?,
        stage_run_id: source.stage_run_id().clone(),
        session_binding_id: source.session_binding_id().0.clone(),
        candidate_ref: source.candidate_ref().to_owned(),
        type_value: match source.evidence_type() {
            EvidenceRefType::Test => "test",
            EvidenceRefType::Command => "command",
            EvidenceRefType::Diff => "diff",
            EvidenceRefType::File => "file",
            EvidenceRefType::Commit => "commit",
            EvidenceRefType::PullRequest => "pull_request",
            EvidenceRefType::RuntimeEvent => "runtime_event",
            EvidenceRefType::ReviewFinding => "review_finding",
        }
        .to_owned(),
        source_ref: source.source_ref().to_owned(),
        created_at: millis_to_instant(source.created_at())?,
    })
}

fn candidate(
    source: &delivery_projection::CurrentCandidateProjection,
) -> Result<api::FrozenCandidateSummaryProjection, StrongFlowProjectionError> {
    Ok(api::FrozenCandidateSummaryProjection {
        candidate_ref: source.candidate_ref().to_owned(),
        delivery_spec_id: source.delivery_spec_id().0.clone(),
        delivery_spec_revision: revision(
            source.delivery_spec_revision(),
            "candidate spec revision",
        )?,
        producer_stage_run_id: source.producer_stage_run_id().clone(),
        producer_session_binding_id: source.producer_session_binding_id().0.clone(),
        candidate_commit_id: source.candidate_commit_id().to_owned(),
        candidate_tree_id: source.candidate_tree_id().to_owned(),
        diff_sha256: digest(source.diff_sha256())?,
        frozen_at: millis_to_instant(source.frozen_at())?,
    })
}

fn verdict(
    source: &delivery_projection::VerdictProjection,
) -> Result<api::DeliveryVerdictProjection, StrongFlowProjectionError> {
    Ok(api::DeliveryVerdictProjection {
        id: source.id().0.clone(),
        delivery_spec_id: source.delivery_spec_id().0.clone(),
        delivery_spec_revision: revision(source.delivery_spec_revision(), "verdict spec revision")?,
        candidate_ref: source.candidate_ref().to_owned(),
        status: criterion_verdict(source.status()).to_owned(),
        criteria: source
            .criteria()
            .iter()
            .map(|criterion| {
                Ok(api::DeliveryCriterionResultProjection {
                    result_id: criterion.result_id().0.clone(),
                    criterion_id: criterion.criterion_id().0.clone(),
                    verdict: criterion_verdict(criterion.verdict()).to_owned(),
                    evidence_refs: criterion.evidence_refs().to_vec(),
                    explanation: criterion.explanation().to_owned(),
                    evaluated_at: millis_to_instant(criterion.evaluated_at())?,
                })
            })
            .collect::<Result<_, StrongFlowProjectionError>>()?,
        unresolved_findings: source.unresolved_findings().to_vec(),
        produced_at: millis_to_instant(source.produced_at())?,
    })
}

fn solution_review(
    source: &delivery_projection::SolutionReviewProjection,
) -> Result<api::SolutionReviewProjection, StrongFlowProjectionError> {
    Ok(api::SolutionReviewProjection {
        delivery_id: source.delivery_id().clone(),
        delivery_spec_id: source.delivery_spec_id().0.clone(),
        delivery_spec_revision: revision(
            source.delivery_spec_revision(),
            "solution spec revision",
        )?,
        planning_stage_run_id: source.planning_stage_run_id().clone(),
        planning_session_binding_id: source.planning_session_binding_id().0.clone(),
        review_stage_run_id: source.review_stage_run_id().clone(),
        attention_item_id: source.attention_item_id().clone(),
        review_set_sha256: digest(source.review_set_sha256())?,
        solution_id: source.solution_id().to_owned(),
        summary: source.summary().to_owned(),
        approach: source.approach().to_vec(),
        components: source
            .components()
            .iter()
            .map(|component| api::SolutionReviewComponentProjection {
                id: component.id().to_owned(),
                label: component.label().to_owned(),
                responsibility: component.responsibility().to_owned(),
                kind: match component.kind() {
                    delivery_projection::SolutionComponentKind::Component => "component",
                    delivery_projection::SolutionComponentKind::External => "external",
                    delivery_projection::SolutionComponentKind::DataStore => "data-store",
                }
                .to_owned(),
                trust_boundary: component.trust_boundary().map(str::to_owned),
                unresolved: component.unresolved(),
                repository_path_prefixes: component.repository_path_prefixes().to_vec(),
            })
            .collect(),
        connections: source
            .connections()
            .iter()
            .map(|connection| api::SolutionReviewConnectionProjection {
                id: connection.id().to_owned(),
                from: connection.from().to_owned(),
                to: connection.to().to_owned(),
                label: connection.label().to_owned(),
            })
            .collect(),
        architecture_diagram: solution_diagram(source.architecture_diagram()),
        process_diagram: solution_diagram(source.process_diagram()),
        risks: source.risks().to_vec(),
        unresolved_items: source.unresolved_items().to_vec(),
        task_proposals: source
            .task_proposals()
            .iter()
            .map(|task| api::DeliveryTaskProposalProjection {
                id: task.id().clone(),
                title: task.title().to_owned(),
                goal: task.goal().to_owned(),
                acceptance_criterion_ids: task
                    .acceptance_criterion_ids()
                    .iter()
                    .map(|id| id.0.clone())
                    .collect(),
                blocked_by_task_ids: task.blocked_by_task_ids().to_vec(),
            })
            .collect(),
        review_status: match source.review_status() {
            delivery_projection::SolutionReviewStatusProjection::Pending => {
                api::SolutionReviewStatus::Pending
            }
            delivery_projection::SolutionReviewStatusProjection::Approved => {
                api::SolutionReviewStatus::Approved
            }
            delivery_projection::SolutionReviewStatusProjection::ChangesRequested => {
                api::SolutionReviewStatus::ChangesRequested
            }
            delivery_projection::SolutionReviewStatusProjection::Rejected => {
                api::SolutionReviewStatus::Rejected
            }
        },
        decision: source.decision().map(|decision| match decision {
            delivery_projection::SolutionReviewDecisionProjection::Approve => {
                api::SolutionReviewDecision::Approve
            }
            delivery_projection::SolutionReviewDecisionProjection::RequestChanges => {
                api::SolutionReviewDecision::RequestChanges
            }
            delivery_projection::SolutionReviewDecisionProjection::Reject => {
                api::SolutionReviewDecision::Reject
            }
        }),
        comments: source.comments().map(str::to_owned),
        requested_changes: source.requested_changes().map(<[String]>::to_vec),
        reviewer_id: source.reviewer_id().map(actor_id).transpose()?,
        reviewed_at: source.reviewed_at().map(millis_to_instant).transpose()?,
    })
}

fn solution_diagram(
    source: &delivery_projection::DiagramProjection,
) -> api::SolutionReviewDiagramProjection {
    api::SolutionReviewDiagramProjection {
        id: source.id().to_owned(),
        kind: match source.kind() {
            delivery_projection::DiagramKind::SystemArchitecture => "system-architecture",
            delivery_projection::DiagramKind::ProcessFlow => "process-flow",
        }
        .to_owned(),
        title: source.title().to_owned(),
        nodes: source
            .nodes()
            .iter()
            .map(|node| api::SolutionReviewDiagramNodeProjection {
                id: node.id().to_owned(),
                label: node.label().to_owned(),
                description: node.description().to_owned(),
                kind: match node.kind() {
                    delivery_projection::DiagramNodeKind::Interaction => "interaction",
                    delivery_projection::DiagramNodeKind::DeliveryControl => "delivery-control",
                    delivery_projection::DiagramNodeKind::Execution => "execution",
                    delivery_projection::DiagramNodeKind::Repository => "repository",
                    delivery_projection::DiagramNodeKind::Component => "component",
                    delivery_projection::DiagramNodeKind::External => "external",
                    delivery_projection::DiagramNodeKind::DataStore => "data-store",
                    delivery_projection::DiagramNodeKind::Stage => "stage",
                    delivery_projection::DiagramNodeKind::Decision => "decision",
                }
                .to_owned(),
                trust_boundary: node.trust_boundary().map(str::to_owned),
                unresolved: node.unresolved(),
            })
            .collect(),
        edges: source
            .edges()
            .iter()
            .map(|edge| api::SolutionReviewDiagramEdgeProjection {
                id: edge.id().to_owned(),
                from: edge.from().to_owned(),
                to: edge.to().to_owned(),
                label: edge.label().to_owned(),
            })
            .collect(),
    }
}

fn actor_id(value: &str) -> Result<api::ActorId, StrongFlowProjectionError> {
    if value.starts_with("usr_") {
        Ok(api::ActorId::UserId(winwincode_domain::UserId(
            value.to_owned(),
        )))
    } else if value.starts_with("svc_") {
        Ok(api::ActorId::ServiceAccountId(
            winwincode_domain::ServiceAccountId(value.to_owned()),
        ))
    } else if value.starts_with("sys_") {
        Ok(api::ActorId::SystemActorId(
            winwincode_domain::SystemActorId(value.to_owned()),
        ))
    } else {
        Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "solution review actor identity is not canonical".to_owned(),
        ))
    }
}

#[allow(clippy::too_many_lines)]
fn publication(
    read: &EstablishedDeliveryRead,
    cursor: &api::StrongFlowReadCursor,
) -> Result<Option<api::PublicationProjection>, StrongFlowProjectionError> {
    let Some(result) = read.publication.result() else {
        return Ok(None);
    };
    let authorization = read.publication_authorization.as_ref().ok_or_else(|| {
        StrongFlowProjectionError::RevisionConflict(
            "publication result has no current publication authorization".to_owned(),
        )
    })?;
    if authorization.read_cursor() != cursor || result.binding() != authorization.binding() {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "publication result is outside the bounded authorized cut".to_owned(),
        ));
    }
    let binding = authorization.binding();
    let approval = read
        .detail
        .attention()
        .iter()
        .find(|item| item.id() == binding.approval_id())
        .ok_or_else(|| {
            StrongFlowProjectionError::RevisionConflict(
                "publication approval is no longer current".to_owned(),
            )
        })?;
    let approved_by = approval.resolved_by().ok_or_else(|| {
        StrongFlowProjectionError::RevisionConflict("publication approval is incomplete".to_owned())
    })?;
    let approved_at = approval.resolved_at().ok_or_else(|| {
        StrongFlowProjectionError::RevisionConflict("publication approval is incomplete".to_owned())
    })?;
    let target_source = read
        .detail
        .requirements()
        .spec()
        .publication_target()
        .ok_or_else(|| {
            StrongFlowProjectionError::RevisionConflict(
                "publication target is no longer current".to_owned(),
            )
        })?;
    let target = publication_target(target_source)?;
    let resource_ref = result
        .resource()
        .map(|resource| publication_resource_ref(resource, &target))
        .transpose()?;
    if cursor.delivery_id != *binding.delivery_id()
        || cursor.delivery_revision.0
            != integer(binding.delivery_revision(), "publication delivery revision")?
        || cursor.publication_revision != *result.revision()
    {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "publication facts do not share the response cursor".to_owned(),
        ));
    }
    Ok(Some(api::PublicationProjection {
        id: result.publication_id().clone(),
        revision: result.revision().clone(),
        delivery_id: binding.delivery_id().clone(),
        delivery_spec_id: binding.delivery_spec_id().to_owned(),
        delivery_spec_revision: revision(
            binding.delivery_spec_revision(),
            "publication spec revision",
        )?,
        candidate_ref: binding.candidate_ref().to_owned(),
        delivery_verdict_id: binding.verdict_id().to_owned(),
        verdict_status: api::PublicationProjectionVerdictStatus::Pass,
        approval_attention_item_id: binding.approval_id().clone(),
        approved_by: actor_id(approved_by)?,
        approved_at: millis_to_instant(approved_at)?,
        publication_set_sha256: result.publication_set_sha256().clone(),
        target,
        state: result.state().to_owned(),
        resource_ref,
        updated_at: result.updated_at().clone(),
    }))
}

fn publication_resource_ref(
    resource: &super::PublicationResourceFact,
    target: &api::PublicationTarget,
) -> Result<api::PublicationResourceRef, StrongFlowProjectionError> {
    if resource.repository() != target.repository.0 {
        return Err(StrongFlowProjectionError::RevisionConflict(
            "published resource identity differs from the authorized GitHub target".to_owned(),
        ));
    }
    Ok(api::PublicationResourceRef {
        kind: match resource.kind() {
            super::PublicationResourceKind::GitHubIssue => {
                api::PublicationResourceKind::GithubIssue
            }
            super::PublicationResourceKind::GitHubPullRequest => {
                api::PublicationResourceKind::GithubPullRequest
            }
        },
        repository: GitHubRepositorySlug(resource.repository().to_owned()),
        number: integer(resource.number(), "publication resource number")?,
    })
}

fn publication_target(
    source: &winwincode_delivery::domain::DeliveryPublicationTarget,
) -> Result<api::PublicationTarget, StrongFlowProjectionError> {
    if source.provider != "github" || source.kind != "pull-request" {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "publication target cannot be represented by the canonical contract".to_owned(),
        ));
    }
    Ok(api::PublicationTarget {
        provider: api::PublicationTargetProvider::Github,
        repository: GitHubRepositorySlug(source.repository.clone()),
        base_branch: source.base_branch.clone(),
        head_repository: GitHubRepositorySlug(source.head_repository.clone()),
        head_branch: source.head_branch.clone(),
    })
}

pub(super) fn runtime_snapshot_for_delivery(
    read: &EstablishedDeliveryRead,
    stage_run_id: &winwincode_domain::StageRunId,
    product_session_id: &winwincode_domain::ProductSessionId,
) -> Result<api::RuntimeProjectionSnapshot, StrongFlowProjectionError> {
    let stage = read
        .detail
        .stages()
        .iter()
        .find(|stage| stage.id() == stage_run_id)
        .ok_or_else(|| {
            StrongFlowProjectionError::ResourceNotFound(
                "the exact StageRun was not found at this read cut".to_owned(),
            )
        })?;
    if stage.actor_type() != StageRunActorType::Codex
        || stage
            .session_binding()
            .is_none_or(|binding| binding.product_session_id() != product_session_id)
    {
        return Err(StrongFlowProjectionError::ResourceNotFound(
            "the exact ProductSession binding was not found at this read cut".to_owned(),
        ));
    }
    let sessions = read
        .runtime
        .snapshot()
        .sessions
        .iter()
        .filter(|session| {
            &session.stage_run_id == stage_run_id
                && &session.product_session_id == product_session_id
        })
        .map(runtime_session)
        .collect::<Result<Vec<_>, _>>()?;
    if sessions.len() > 1 {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "multiple runtime sessions matched one Delivery StageRun at this read cut".to_owned(),
        ));
    }
    Ok(api::RuntimeProjectionSnapshot {
        kind: api::RuntimeProjectionSnapshotKind::RuntimeProjection,
        event_cursor: api::RuntimeProjectionEventCursor::DeliveryEventReadCursor(
            read.cursor.event_cursor.clone(),
        ),
        revision: read.runtime.ledger_revision().clone(),
        read_cursor: Some(cursor(read)?),
        product_session_id: product_session_id.clone(),
        delivery_id: Some(read.detail.delivery_id().clone()),
        stage_run_id: Some(stage_run_id.clone()),
        last_projection_sequence: integer(
            read.runtime.accepted_sequence(),
            "runtime accepted sequence",
        )?,
        sessions,
        rebuilt_at: read.runtime.rebuilt_at().clone(),
    })
}

pub(super) fn runtime_snapshot_for_product_session(
    read: &super::application::EstablishedProductSessionRead,
    product_session_id: &winwincode_domain::ProductSessionId,
) -> Result<api::RuntimeProjectionSnapshot, StrongFlowProjectionError> {
    let sessions = read
        .runtime
        .product_session_runtime()
        .map(runtime_product_session)
        .transpose()?
        .into_iter()
        .filter(|session| &session.product_session_id == product_session_id)
        .collect();
    Ok(api::RuntimeProjectionSnapshot {
        kind: api::RuntimeProjectionSnapshotKind::RuntimeProjection,
        event_cursor: api::RuntimeProjectionEventCursor::ProductSessionEventReadCursor(
            read.event_cursor.clone(),
        ),
        revision: read.runtime.ledger_revision().clone(),
        read_cursor: None,
        product_session_id: product_session_id.clone(),
        delivery_id: None,
        stage_run_id: None,
        last_projection_sequence: integer(
            read.runtime.accepted_sequence(),
            "runtime accepted sequence",
        )?,
        sessions,
        rebuilt_at: read.runtime.rebuilt_at().clone(),
    })
}

/// Maps the standalone `ProductSession` identity to the generated runtime
/// session contract. `ProductSession` execution has no Delivery `StageRun` or
/// domain `SessionBinding` id; the stable projection key is derived from the
/// exact ProductSession/ExecutionJob identity retained by the runtime ledger.
fn runtime_product_session(
    source: &TrustedProductSessionRuntimeSession,
) -> Result<api::RuntimeSessionProjection, StrongFlowProjectionError> {
    Ok(api::RuntimeSessionProjection {
        activities: Vec::new(),
        agent_edges: Vec::new(),
        agents: Vec::new(),
        as_of_sequence: integer(source.as_of_sequence, "session sequence")?,
        attempt: integer(source.attempt, "runtime attempt")?,
        codex_thread_id: source.codex_thread_id.clone(),
        delivery_task_id: None,
        diff_summary: None,
        execution_job_id: source.execution_job_id.clone(),
        fencing_token: source.fencing_token.0.clone(),
        lease_id: source.lease_id.clone(),
        plan: None,
        product_session_id: source.product_session_id.clone(),
        recovery: api::RuntimeRecoveryProjection {
            state: api::RuntimeRecoveryState::None,
            failure_count: Count(0),
            recovery_count: Count(0),
            last_failure_source_ref: None,
            latest_recovery_source_ref: None,
        },
        session_binding_id: format!("product-session-runtime:{}", source.execution_job_id.0),
        stage_run_id: None,
        usage: None,
        worker_session_id: source.worker_session_id.clone(),
    })
}

#[allow(clippy::too_many_lines)]
fn runtime_session(
    source: &runtime_projection::RuntimeSessionProjection,
) -> Result<api::RuntimeSessionProjection, StrongFlowProjectionError> {
    Ok(api::RuntimeSessionProjection {
        session_binding_id: source.session_binding_id.0.clone(),
        stage_run_id: Some(source.stage_run_id.clone()),
        delivery_task_id: source.delivery_task_id.clone(),
        product_session_id: source.product_session_id.clone(),
        worker_session_id: source.worker_session_id.clone(),
        codex_thread_id: source.codex_thread_id.clone(),
        execution_job_id: source.execution_job_id.clone(),
        lease_id: source.lease_id.clone(),
        attempt: integer(source.attempt, "runtime attempt")?,
        fencing_token: source.fencing_token.0.clone(),
        as_of_sequence: integer(source.as_of_sequence, "session sequence")?,
        plan: source.plan.as_ref().map(|plan| api::RuntimePlanProjection {
            item_id: plan.item_id.clone(),
            explanation: plan.explanation.clone(),
            text: plan.text.clone(),
            complete: plan.complete,
            source_ref: plan.source_ref.clone(),
            items: plan
                .items
                .iter()
                .map(|item| api::RuntimePlanItemProjection {
                    step: item.step.clone(),
                    status: match item.status {
                        runtime_projection::RuntimePlanItemStatus::Pending => {
                            api::RuntimePlanItemStatus::Pending
                        }
                        runtime_projection::RuntimePlanItemStatus::InProgress => {
                            api::RuntimePlanItemStatus::InProgress
                        }
                        runtime_projection::RuntimePlanItemStatus::Completed => {
                            api::RuntimePlanItemStatus::Completed
                        }
                    },
                })
                .collect(),
        }),
        agents: source
            .agents
            .iter()
            .map(|agent| api::RuntimeAgentProjection {
                thread_id: agent.thread_id.clone(),
                parent_thread_id: agent.parent_thread_id.clone(),
                path: agent.path.clone(),
                nickname: agent.nickname.clone(),
                role: agent.role.clone(),
                status: match agent.status {
                    runtime_projection::RuntimeAgentStatus::Unknown => {
                        api::RuntimeAgentStatus::Unknown
                    }
                    runtime_projection::RuntimeAgentStatus::Waiting => {
                        api::RuntimeAgentStatus::Waiting
                    }
                    runtime_projection::RuntimeAgentStatus::Running => {
                        api::RuntimeAgentStatus::Running
                    }
                    runtime_projection::RuntimeAgentStatus::Completed => {
                        api::RuntimeAgentStatus::Completed
                    }
                    runtime_projection::RuntimeAgentStatus::Interrupted => {
                        api::RuntimeAgentStatus::Interrupted
                    }
                    runtime_projection::RuntimeAgentStatus::Failed => {
                        api::RuntimeAgentStatus::Failed
                    }
                    runtime_projection::RuntimeAgentStatus::Closed => {
                        api::RuntimeAgentStatus::Closed
                    }
                },
                source_ref: agent.source_ref.clone(),
            })
            .collect(),
        agent_edges: source
            .agent_edges
            .iter()
            .map(|edge| api::RuntimeAgentEdgeProjection {
                parent_thread_id: edge.parent_thread_id.clone(),
                child_thread_id: edge.child_thread_id.clone(),
            })
            .collect(),
        activities: source
            .activities
            .iter()
            .map(|activity| api::RuntimeActivityProjection {
                call_id: activity.call_id.clone(),
                activity_type: match activity.activity_type {
                    runtime_projection::RuntimeActivityType::Command => {
                        api::RuntimeActivityType::Command
                    }
                    runtime_projection::RuntimeActivityType::Test => api::RuntimeActivityType::Test,
                },
                command: activity.command.clone(),
                status: match activity.status {
                    runtime_projection::RuntimeActivityStatus::Running => {
                        api::RuntimeActivityStatus::Running
                    }
                    runtime_projection::RuntimeActivityStatus::Completed => {
                        api::RuntimeActivityStatus::Completed
                    }
                    runtime_projection::RuntimeActivityStatus::Failed => {
                        api::RuntimeActivityStatus::Failed
                    }
                    runtime_projection::RuntimeActivityStatus::Declined => {
                        api::RuntimeActivityStatus::Declined
                    }
                    runtime_projection::RuntimeActivityStatus::Cancelled => {
                        api::RuntimeActivityStatus::Cancelled
                    }
                    runtime_projection::RuntimeActivityStatus::Unknown => {
                        api::RuntimeActivityStatus::Unknown
                    }
                },
                outcome: match activity.outcome {
                    runtime_projection::RuntimeActivityOutcome::Observed => {
                        api::RuntimeActivityOutcome::Observed
                    }
                    runtime_projection::RuntimeActivityOutcome::Succeeded => {
                        api::RuntimeActivityOutcome::Succeeded
                    }
                    runtime_projection::RuntimeActivityOutcome::TaskFailed => {
                        api::RuntimeActivityOutcome::TaskFailed
                    }
                    runtime_projection::RuntimeActivityOutcome::TimedOut => {
                        api::RuntimeActivityOutcome::TimedOut
                    }
                    runtime_projection::RuntimeActivityOutcome::PolicyDenied => {
                        api::RuntimeActivityOutcome::PolicyDenied
                    }
                    runtime_projection::RuntimeActivityOutcome::InfrastructureFailed => {
                        api::RuntimeActivityOutcome::InfrastructureFailed
                    }
                    runtime_projection::RuntimeActivityOutcome::Cancelled => {
                        api::RuntimeActivityOutcome::Cancelled
                    }
                },
                exit_code: activity.exit_code.map(i64::from),
                source_ref: activity.source_ref.clone(),
            })
            .collect(),
        usage: source
            .usage
            .as_ref()
            .map(|usage| {
                Ok::<_, StrongFlowProjectionError>(api::RuntimeUsageProjection {
                    source_ref: usage.source_ref.clone(),
                    totals: usage
                        .totals
                        .iter()
                        .map(|metric| {
                            Ok(api::RuntimeUsageMetricProjection {
                                name: metric.name.clone(),
                                value: integer(metric.value, "usage value")?,
                            })
                        })
                        .collect::<Result<_, StrongFlowProjectionError>>()?,
                })
            })
            .transpose()?,
        recovery: api::RuntimeRecoveryProjection {
            state: match source.recovery.state {
                runtime_projection::RuntimeRecoveryState::None => api::RuntimeRecoveryState::None,
                runtime_projection::RuntimeRecoveryState::Required => {
                    api::RuntimeRecoveryState::Required
                }
                runtime_projection::RuntimeRecoveryState::InProgress => {
                    api::RuntimeRecoveryState::InProgress
                }
                runtime_projection::RuntimeRecoveryState::Recovered => {
                    api::RuntimeRecoveryState::Recovered
                }
            },
            failure_count: count(source.recovery.failure_count, "failure count")?,
            recovery_count: count(source.recovery.recovery_count, "recovery count")?,
            last_failure_source_ref: source.recovery.last_failure_source_ref.clone(),
            latest_recovery_source_ref: source.recovery.latest_recovery_source_ref.clone(),
        },
        diff_summary: source
            .diff_summary
            .as_ref()
            .map(|diff| {
                Ok::<_, StrongFlowProjectionError>(api::RuntimeDiffSummaryProjection {
                    changed_file_count: count(diff.changed_file_count(), "changed file count")?,
                    additions: count(diff.additions(), "diff additions")?,
                    deletions: count(diff.deletions(), "diff deletions")?,
                    details_visible: diff.details_visible(),
                    source_ref: diff.source_ref().to_owned(),
                })
            })
            .transpose()?,
    })
}

fn delivery_status(value: DomainDeliveryStatus) -> api::DeliveryStatus {
    match value {
        DomainDeliveryStatus::Draft => api::DeliveryStatus::Draft,
        DomainDeliveryStatus::Clarifying => api::DeliveryStatus::Clarifying,
        DomainDeliveryStatus::Ready => api::DeliveryStatus::Ready,
        DomainDeliveryStatus::Planning => api::DeliveryStatus::Planning,
        DomainDeliveryStatus::PlanReview => api::DeliveryStatus::PlanReview,
        DomainDeliveryStatus::Executing => api::DeliveryStatus::Executing,
        DomainDeliveryStatus::Verifying => api::DeliveryStatus::Verifying,
        DomainDeliveryStatus::Reworking => api::DeliveryStatus::Reworking,
        DomainDeliveryStatus::NeedsAttention => api::DeliveryStatus::NeedsAttention,
        DomainDeliveryStatus::ReadyToDeliver => api::DeliveryStatus::ReadyToDeliver,
        DomainDeliveryStatus::Delivered => api::DeliveryStatus::Delivered,
    }
}

fn stage_name(value: DeliveryStage) -> &'static str {
    match value {
        DeliveryStage::Clarifying => "clarifying",
        DeliveryStage::Planning => "planning",
        DeliveryStage::PlanReview => "plan-review",
        DeliveryStage::Executing => "executing",
        DeliveryStage::Verifying => "verifying",
        DeliveryStage::Reworking => "reworking",
        DeliveryStage::DeliveryReview => "delivery-review",
    }
}
fn stage_status(value: StageRunStatus) -> &'static str {
    match value {
        StageRunStatus::Running => "running",
        StageRunStatus::Waiting => "waiting",
        StageRunStatus::Succeeded => "succeeded",
        StageRunStatus::Failed => "failed",
        StageRunStatus::Cancelled => "cancelled",
    }
}
fn criterion_verdict(value: CriterionVerdict) -> &'static str {
    match value {
        CriterionVerdict::Pass => "pass",
        CriterionVerdict::Fail => "fail",
        CriterionVerdict::Inconclusive => "inconclusive",
        CriterionVerdict::InfraError => "infra_error",
    }
}

fn revision(value: u64, label: &str) -> Result<Revision, StrongFlowProjectionError> {
    integer(value, label).map(Revision)
}
fn count(value: u64, label: &str) -> Result<Count, StrongFlowProjectionError> {
    integer(value, label).map(Count)
}
fn integer(value: u64, label: &str) -> Result<i64, StrongFlowProjectionError> {
    i64::try_from(value).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable(format!(
            "{label} exceeds the public integer range"
        ))
    })
}
fn digest(value: &str) -> Result<Sha256Digest, StrongFlowProjectionError> {
    let raw = value.strip_prefix("sha256:").unwrap_or(value);
    if raw.len() == 64
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(Sha256Digest(format!("sha256:{raw}")))
    } else {
        Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "a projection digest is not canonical".to_owned(),
        ))
    }
}

fn millis_to_instant(value: u64) -> Result<Instant, StrongFlowProjectionError> {
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400).map_err(|_| {
        StrongFlowProjectionError::TrustedFactsUnavailable("timestamp exceeds RFC 3339".to_owned())
    })?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(StrongFlowProjectionError::TrustedFactsUnavailable(
            "timestamp exceeds RFC 3339".to_owned(),
        ));
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let text =
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z");
    Ok(Instant(text))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strongflow_projection::{PublicationResourceFact, PublicationResourceKind};

    fn pull_request_target() -> api::PublicationTarget {
        api::PublicationTarget {
            provider: api::PublicationTargetProvider::Github,
            repository: GitHubRepositorySlug("owner/repository".to_owned()),
            base_branch: "main".to_owned(),
            head_repository: GitHubRepositorySlug("contributor/repository".to_owned()),
            head_branch: "delivery".to_owned(),
        }
    }

    #[test]
    fn publication_resource_preserves_each_closed_github_kind() {
        for (source, expected) in [
            (
                PublicationResourceKind::GitHubIssue,
                api::PublicationResourceKind::GithubIssue,
            ),
            (
                PublicationResourceKind::GitHubPullRequest,
                api::PublicationResourceKind::GithubPullRequest,
            ),
        ] {
            let resource = PublicationResourceFact::try_new(source, "owner/repository", 7)
                .expect("valid closed GitHub identity");
            let projected = publication_resource_ref(&resource, &pull_request_target())
                .expect("matching closed GitHub resource");
            assert_eq!(projected.kind, expected);
            assert_eq!(projected.repository.0, "owner/repository");
            assert_eq!(projected.number, 7);
        }
    }

    #[test]
    fn publication_resource_rejects_a_foreign_repository() {
        let resource = PublicationResourceFact::try_new(
            PublicationResourceKind::GitHubPullRequest,
            "other/repository",
            7,
        )
        .expect("valid closed GitHub identity");
        assert!(matches!(
            publication_resource_ref(&resource, &pull_request_target()),
            Err(StrongFlowProjectionError::RevisionConflict(_))
        ));
    }

    #[test]
    fn pending_session_binding_maps_to_the_closed_nullable_branch() {
        let parsed = winwincode_delivery::domain::Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical Delivery fixture");
        let mut snapshot = parsed.into_snapshot();
        snapshot.status = DomainDeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        let binding = snapshot
            .session_bindings
            .first_mut()
            .expect("fixture SessionBinding");
        binding.worker_session_id = None;
        binding.codex_thread_id = None;
        binding.worker_id = None;
        binding.worker_instance_id = None;
        binding.lease_id = None;
        binding.fencing_token = None;
        binding.source_provenance =
            winwincode_delivery::domain::SessionBindingSourceProvenance::delivery_advance(
                "delivery.advance",
            );
        let delivery = winwincode_delivery::domain::Delivery::try_from_snapshot(snapshot)
            .expect("pending SessionBinding");
        let projected = winwincode_delivery::projection::project_delivery_detail(
            winwincode_delivery::projection::ProjectionInput::new(&delivery),
        )
        .expect("Delivery projection");

        let source = projected.stages().first().expect("fixture StageRun");
        let mapped = stage(source).expect("generated stage projection");
        let binding = mapped.session_binding.expect("Codex stage SessionBinding");
        assert_eq!(binding.worker_session_id, None);
        assert_eq!(binding.codex_thread_id, None);
        assert_eq!(binding.stage_run_id, None);
        assert_eq!(binding.worker_id, None);
        assert_eq!(binding.lease_id, None);
        assert_eq!(binding.attempt, None);
        assert_eq!(binding.fencing_token, None);
        assert_eq!(binding.session_identity, None);
        assert_eq!(binding.source_identity, None);
    }

    #[test]
    fn whole_second_projection_timestamp_keeps_the_required_milliseconds() {
        assert_eq!(
            millis_to_instant(0).expect("Unix epoch").0,
            "1970-01-01T00:00:00.000Z"
        );
    }
}
