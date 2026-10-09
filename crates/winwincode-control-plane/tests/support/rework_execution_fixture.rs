// SPDX-License-Identifier: Apache-2.0

//! Real scheduler and Git seams for post-terminal rework rejection regressions.
//! The caller starts each role through public commands. This helper consumes
//! that exact queued job and keeps verification Snapshot and rework boundaries.

#[path = "dispatch_fixture.rs"]
#[allow(dead_code)]
mod dispatch_fixture;

use std::{fmt::Write as _, fs, path::Path, process::Command};

use dispatch_fixture::{attempt_time, register_attempt_worker};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableExecutionPortIngress, EventPublishError,
    EventPublisher, LocalDeliveryAdapterConfig, OutboxEvent, RepositoryExecutionScheduler,
};
use winwincode_delivery::{
    application::workrun_execution::{
        DeliveryTerminalOutcomeFacts, SessionBindingAuthority, TerminalArtifactReference,
        TerminalOutcomeStatus,
        test_support::{
            active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
            terminal_outcome_metadata, terminal_worker_outcome,
        },
    },
    domain::Delivery,
};
use winwincode_domain::{
    AgentIdentityId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionEventId,
    ExecutionMessageId, ExecutionSequence, RepositoryScope, RequestId, RuntimeSessionAgentIdentity,
    RuntimeSessionContext, RuntimeSessionWorkspace, SchemaVersion, SessionBindingSourceIdentity,
    SessionBindingSourceIdentityKind, SessionIdentity, Sha256Digest, UserId, WorkerSessionId,
    WorkspaceRevision,
};
use winwincode_execution_port::generated::{
    ArtifactReference, ExecutionEventCategory, ExecutionEventRecord, ExecutionJob,
    ExecutionOutcome, ExecutionOutcomeStatus, ExecutionOutcomeUsage,
    ExecutionOutcomeUsageAccountingStatus, ExecutionPortMessage, ExecutionScope,
    ExecutionWorkspaceWriteMode, JobDispatchMessage, JobDispatchResultMessage,
    JobDispatchResultMessageKind, JobDispatchResultMessageStatus, JobOutcomeMessage,
    JobOutcomeMessageKind, LeaseWriteStatus, RuntimeEventMessage, RuntimeEventMessageKind,
    SessionBindingMessage, SessionBindingMessageKind, SnapshotFreezeReceipt,
    SnapshotFreezeReceiptMessage, SnapshotFreezeReceiptMessageKind,
};
use winwincode_storage::{
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest,
    ExecutionReservationStart, ProductStateStorage, RepositorySchedulerClaimRequest,
    RepositorySchedulerScope, SqliteStorage, WorkerPoolId, WorkerSlotAuthority,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

/// `clock` is a minute counter in the existing January 2027 dispatch fixture.
/// Choose a fresh identity seed for each stage, even though its attempt is one.
#[derive(Clone, Copy)]
pub struct ReworkAttemptConfig<'fixture> {
    pub root: &'fixture Path,
    pub scope: &'fixture RepositoryScope,
    pub delivery_id: &'fixture DeliveryId,
    pub job: &'fixture ExecutionJob,
    pub scope_seed: u64,
    pub identity_seed: u64,
    pub clock: u64,
}

pub struct RunningReworkAttempt {
    pub delivery: Delivery,
    pub job: ExecutionJob,
    pub binding: SessionBindingMessage,
    pub authority: SessionBindingAuthority,
    pub clock: u64,
}

/// Claim, admit, dispatch and bind the public command's sealed remediator job.
pub fn claim_running_rework(config: ReworkAttemptConfig<'_>) -> RunningReworkAttempt {
    assert_eq!(config.job.execution_profile, "remediator");
    claim_profile(config, None)
}

/// Verification roles use the production freeze request and Snapshot commit.
/// Their freeze receipt is an offline Worker fact fixture over the real source;
/// it does not claim that a Worker process materialized the Snapshot.
pub fn claim_running_profile(
    config: ReworkAttemptConfig<'_>,
    repository: &Path,
) -> RunningReworkAttempt {
    claim_profile(config, Some(repository))
}

#[allow(clippy::too_many_lines)]
fn claim_profile(
    config: ReworkAttemptConfig<'_>,
    repository: Option<&Path>,
) -> RunningReworkAttempt {
    let ReworkAttemptConfig {
        root,
        scope,
        delivery_id,
        job,
        scope_seed,
        identity_seed,
        clock,
    } = config;
    assert!(
        (1..=60).contains(&clock),
        "fixture clock must name a valid minute"
    );
    let ExecutionScope::WorkRunExecutionScope(work_scope) = &job.scope else {
        panic!("role fixture requires a Delivery WorkRun");
    };
    let verification = match job.execution_profile.as_str() {
        "reviewer" | "verifier" => {
            assert_eq!(
                job.workspace.write_mode,
                ExecutionWorkspaceWriteMode::ReadOnly
            );
            assert!(work_scope.rework_authorization.is_none());
            assert!(
                repository.is_some(),
                "verification requires its real source root"
            );
            true
        }
        "remediator" => {
            assert_eq!(
                job.workspace.write_mode,
                ExecutionWorkspaceWriteMode::Candidate
            );
            assert!(
                work_scope.rework_authorization.is_some(),
                "public sealed authorization"
            );
            false
        }
        profile => panic!("unsupported role fixture: {profile}"),
    };
    assert_eq!(job.attempt, 1, "a fresh role job starts at attempt one");
    let mut storage = SqliteStorage::open(root).expect("role scheduler storage");
    let (worker_id, worker_instance_id) =
        register_attempt_worker(&mut storage, identity_seed, 1, clock);
    let mut dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: RepositorySchedulerScope {
                organization_id: scope.organization_id.clone(),
                workspace_id: scope.workspace_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
            },
            request_id: RequestId(canonical_id("req", identity_seed + 260)),
            scheduler_generation: "sealed-role-regression".into(),
            worker_id,
            worker_instance_id,
            issued_at: attempt_time(clock, 3),
            expires_at: attempt_time(clock, 50),
        })
        .expect("claim public role job")
        .expect("queued role dispatch");
    assert_eq!(
        dispatch.job, *job,
        "claim must consume the exact public job"
    );
    assert!(
        dispatch.snapshot_id.is_none(),
        "the scheduler cannot manufacture a verification Snapshot"
    );
    assert!(dispatch.replacement_authority.is_none());
    prepare_role_admission(
        &mut storage,
        scope_seed,
        ExecutionQueueScope {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            product_session_id: work_scope.product_session_id.clone(),
            delivery_id: Some(delivery_id.clone()),
        },
        job,
        identity_seed,
        clock,
    );
    let mut control_plane = if verification {
        ControlPlane::start_local_with_delivery_adapters(
            ControlPlaneConfig::local(root),
            Box::new(RecordingPublisher),
            LocalDeliveryAdapterConfig::new(repository.unwrap(), scope.clone()),
        )
    } else {
        ControlPlane::start_local(
            ControlPlaneConfig::local(root),
            Box::new(RecordingPublisher),
        )
    }
    .expect("role dispatch Control Plane");
    if verification {
        dispatch = freeze_verification_dispatch(&mut control_plane, &mut storage, &dispatch, clock);
        assert!(
            dispatch.snapshot_id.is_some(),
            "canonical verification Snapshot"
        );
    }
    let (authority, binding) = binding_for_dispatch(&dispatch, identity_seed, clock);
    let result = JobDispatchResultMessage {
        error: None,
        job_id: dispatch.job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: dispatch.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", identity_seed + 261)),
        payload_digest: dispatch.job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", identity_seed + 261)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(clock, 4),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(binding.worker_session_id.clone()),
    };
    DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        scope,
        attempt_time(clock, 4),
    )
    .expect("same-database role ingress")
    .handle(&ExecutionPortMessage::JobDispatchResultMessage(result))
    .expect("accepted role dispatch");
    let slot_authority = WorkerSlotAuthority {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        worker_session_id: binding.worker_session_id.clone(),
        codex_thread_id: binding.codex_thread_id.clone(),
        job_id: dispatch.job.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        attempt: 1,
        fencing_token: dispatch.lease.fencing_token.clone(),
    };
    {
        let mut slots = storage.worker_session_slots().expect("role Worker slots");
        slots
            .configure_resources(
                &slot_authority.worker_id,
                &slot_authority.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 100,
                    max_disk_bytes: 100,
                    max_processes: 1,
                },
            )
            .expect("role slot resource limits");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: slot_authority,
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: RequestId(canonical_id("req", identity_seed + 262)),
                opened_at: attempt_time(clock, 5),
            })
            .expect("role Worker slot");
    }
    control_plane
        .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
        .expect("complete exact role SessionBinding");
    if verification {
        let runtime = RuntimeEventMessage {
            codex_thread_id: binding.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(canonical_id("xevt", identity_seed + 265)),
                occurred_at: attempt_time(clock, 8),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "Offline verification role fixture lifecycle fact".into(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease: binding.lease.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", identity_seed + 265)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: attempt_time(clock, 8),
            session_identity: binding.session_identity.clone(),
            worker_session_id: binding.worker_session_id.clone(),
        };
        let ack = control_plane
            .accept_runtime_event(scope, &runtime, &authority, &runtime.sent_at)
            .expect("public Snapshot-bound verification runtime ingress");
        assert_eq!(ack.status, LeaseWriteStatus::Accepted);
        assert_eq!(ack.ack_sequence, ExecutionAckSequence(1));
    }
    let stored = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("load bound role Delivery")
        .expect("bound role Delivery exists");
    let delivery = Delivery::decode_json(&stored.payload).expect("bound role Delivery JSON");
    control_plane.shutdown().expect("role dispatch shutdown");
    Box::new(storage)
        .close()
        .expect("role scheduler storage close");
    RunningReworkAttempt {
        delivery,
        job: dispatch.job,
        binding,
        authority,
        clock,
    }
}

/// Reuses the initial writer's immutable policies. Each new role has a new
/// `ProductSession` boundary, whose fixture limit follows that same organization
/// policy. The production reserve/start calls still enforce every boundary.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn prepare_role_admission(
    storage: &mut SqliteStorage,
    seed: u64,
    scope: ExecutionQueueScope,
    job: &ExecutionJob,
    identity_seed: u64,
    clock: u64,
) {
    let policies = configured_admission_policies(storage);
    let organization = ExecutionAdmissionBoundary::Organization {
        organization_id: scope.organization_id.clone(),
    };
    let default_limits = policies
        .iter()
        .find(|policy| policy.boundary == organization)
        .expect("the initial writer configured its organization policy")
        .limits;
    let worker_pool_id = WorkerPoolId(canonical_id("wpl", seed));
    let boundaries = [
        organization,
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("Delivery queue scope"),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("role admission");
    for boundary in boundaries {
        let existing = policies.iter().find(|policy| policy.boundary == boundary);
        assert!(
            existing.is_some()
                || matches!(&boundary, ExecutionAdmissionBoundary::ProductSession { .. }),
            "only the new role's ProductSession may need its first policy"
        );
        let limits = existing.map_or(default_limits, |policy| policy.limits);
        let created = admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("reuse exact immutable role admission policy");
        assert_eq!(
            created,
            existing.is_none(),
            "existing policy is an exact replay"
        );
    }
    let repository_access = match job.workspace.write_mode {
        ExecutionWorkspaceWriteMode::ReadOnly => ExecutionRepositoryAccess::ReadOnly,
        ExecutionWorkspaceWriteMode::Candidate => ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: job.job_id.0.clone(),
        },
    };
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", identity_seed + 230)),
            repository_access,
            reserved_tokens: Some(100),
            reserved_cost_microunits: Some(1_000),
            runtime_limit_millis: Some(30_000),
            submitted_at: attempt_time(clock, 1),
        })
        .expect("reserve role within configured admission limits");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", identity_seed + 231)),
            expected_revision: 1,
            started_at: attempt_time(clock, 2),
        })
        .expect("start role through the admission gate");
}

fn configured_admission_policies(storage: &SqliteStorage) -> Vec<ExecutionAdmissionPolicy> {
    let connection = rusqlite::Connection::open_with_flags(
        storage.database_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read the fixture's existing admission configuration");
    let rows = {
        let mut statement = connection
            .prepare(
                "SELECT boundary_json, max_concurrent, max_queued, token_budget, \
                 cost_budget_microunits, max_runtime_millis FROM execution_admission_policies",
            )
            .expect("read configured role policies");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ExecutionAdmissionLimits {
                        max_concurrent: admission_policy_integer(row.get(1)?, 1)?,
                        max_queued: admission_policy_integer(row.get(2)?, 2)?,
                        token_budget: row
                            .get::<_, Option<i64>>(3)?
                            .map(|value| admission_policy_integer(value, 3))
                            .transpose()?,
                        cost_budget_microunits: row
                            .get::<_, Option<i64>>(4)?
                            .map(|value| admission_policy_integer(value, 4))
                            .transpose()?,
                        max_runtime_millis: row
                            .get::<_, Option<i64>>(5)?
                            .map(|value| admission_policy_integer(value, 5))
                            .transpose()?,
                    },
                ))
            })
            .expect("configured policy rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("valid configured policy limits")
    };
    connection
        .close()
        .expect("admission configuration reader close");
    rows.into_iter()
        .map(|(boundary, limits)| ExecutionAdmissionPolicy {
            boundary: serde_json::from_str(&boundary).expect("configured policy boundary"),
            limits,
        })
        .collect()
}

fn admission_policy_integer(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn freeze_verification_dispatch(
    control_plane: &mut ControlPlane,
    storage: &mut SqliteStorage,
    dispatch: &JobDispatchMessage,
    clock: u64,
) -> JobDispatchMessage {
    let now = attempt_time(clock, 4);
    let ExecutionPortMessage::SnapshotFreezeRequestMessage(request) = control_plane
        .prepare_snapshot_freeze(storage, dispatch, &now)
        .expect("persist production Snapshot freeze request from the pinned real Candidate")
    else {
        panic!("verification requires Snapshot freeze");
    };
    let mut receipt = SnapshotFreezeReceipt {
        schema_version: SchemaVersion::WinwincodeV1,
        receipt_id: request.request_id.clone(),
        request_id: request.request_id.clone(),
        candidate_id: request.candidate.id.clone(),
        work_run_id: request.candidate.work_run_id.clone(),
        repository_id: request.repository_id.clone(),
        worker_id: request.lease.worker_id.clone(),
        worker_instance_id: request.lease.worker_instance_id.clone(),
        lease_id: request.lease.lease_id.clone(),
        attempt: request.lease.attempt,
        fencing_token: request.lease.fencing_token.clone(),
        base_commit_id: winwincode_domain::GitObjectId(request.candidate.base_commit.clone()),
        base_tree_id: request.base_tree_id.clone(),
        candidate_commit_id: winwincode_domain::GitObjectId(
            request.candidate.candidate_commit.clone(),
        ),
        candidate_tree_id: winwincode_domain::GitObjectId(request.candidate.candidate_tree.clone()),
        diff_sha256: request.candidate.diff_digest.clone(),
        content_digest: request.content_digest.clone(),
        frozen_at: now.clone(),
        validation_seal: Sha256Digest(String::new()),
    };
    receipt.validation_seal =
        winwincode_execution_port::snapshot_freeze::seal_freeze_receipt(&receipt)
            .expect("seal the offline Worker freeze receipt fixture");
    let message = SnapshotFreezeReceiptMessage {
        schema_version: SchemaVersion::WinwincodeV1,
        kind: SnapshotFreezeReceiptMessageKind::SnapshotFreezeReceipt,
        message_id: request.message_id,
        sent_at: now.clone(),
        lease: request.lease,
        receipt,
    };
    control_plane
        .accept_snapshot_freeze(storage, &message, &now)
        .expect("commit canonical Snapshot and role binding before dispatch")
        .dispatch
}

fn binding_for_dispatch(
    dispatch: &JobDispatchMessage,
    seed: u64,
    clock: u64,
) -> (SessionBindingAuthority, SessionBindingMessage) {
    let ExecutionScope::WorkRunExecutionScope(scope) = &dispatch.job.scope else {
        panic!("Delivery WorkRun scope");
    };
    let (worker_session_id, codex_thread_id) = if dispatch.snapshot_id.is_some() {
        winwincode_execution_port::execution_identity::canonical_dispatch_session_identity(
            &dispatch.lease.worker_id,
            &dispatch.lease.worker_instance_id,
            dispatch,
        )
        .expect("Snapshot-bound canonical role identity")
    } else {
        (
            WorkerSessionId(canonical_id("wsn", seed + 10)),
            CodexThreadId(canonical_id("cdx", seed + 10)),
        )
    };
    let message = SessionBindingMessage {
        attempt: dispatch.lease.attempt,
        bound_at: attempt_time(clock, 5),
        codex_thread_id: codex_thread_id.clone(),
        fencing_token: dispatch.lease.fencing_token.clone(),
        kind: SessionBindingMessageKind::SessionBinding,
        lease: dispatch.lease.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 263)),
        product_session_id: scope.product_session_id.clone(),
        runtime_context: RuntimeSessionContext {
            agent_identity: RuntimeSessionAgentIdentity {
                id: AgentIdentityId(canonical_id("agt", seed + 10)),
                worker_id: dispatch.lease.worker_id.clone(),
                name: dispatch.job.execution_profile.clone(),
                role: dispatch.job.execution_profile.clone(),
            },
            provider: "fixture-provider".into(),
            model: "fixture-model".into(),
            workspace: RuntimeSessionWorkspace {
                repository_id: dispatch.job.workspace.repository_id.clone(),
                revision: WorkspaceRevision(format!("git-tree:{}", "a".repeat(64))),
                write_mode: match dispatch.job.workspace.write_mode {
                    ExecutionWorkspaceWriteMode::ReadOnly => "read-only",
                    ExecutionWorkspaceWriteMode::Candidate => "candidate",
                }
                .into(),
            },
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(clock, 6),
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id: scope.product_session_id.clone(),
            work_run_id: Some(scope.work_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        },
        source_identity: SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: dispatch.lease.lease_id.clone(),
            worker_id: dispatch.lease.worker_id.clone(),
            worker_instance_id: dispatch.lease.worker_instance_id.clone(),
            worker_session_id: worker_session_id.clone(),
        },
        work_run_id: Some(scope.work_run_id.clone()),
        worker_id: dispatch.lease.worker_id.clone(),
        snapshot_id: dispatch.snapshot_id.clone(),
        worker_session_id: worker_session_id.clone(),
    };
    let lease = active_lease_identity(
        dispatch.job.job_id.clone(),
        1,
        dispatch.lease.lease_id.clone(),
        dispatch.lease.fencing_token.clone(),
        dispatch.lease.worker_id.clone(),
        dispatch.lease.worker_instance_id.clone(),
        worker_session_id,
    );
    let authority = session_binding_authority(
        lease,
        dispatch.lease.issued_at.clone(),
        dispatch.lease.expires_at.clone(),
    );
    (authority, message)
}

/// This is an offline Worker fact fixture, not a claimed model receipt.
pub fn successful_rework_terminal(
    running: &RunningReworkAttempt,
    artifact: ArtifactReference,
    seed: u64,
) -> (JobOutcomeMessage, DeliveryTerminalOutcomeFacts) {
    assert_eq!(running.job.execution_profile, "remediator");
    successful_profile_terminal(running, vec![artifact], seed)
}

/// Produces an offline successful terminal over the already bound exact role.
pub fn successful_profile_terminal(
    running: &RunningReworkAttempt,
    artifacts: Vec<ArtifactReference>,
    seed: u64,
) -> (JobOutcomeMessage, DeliveryTerminalOutcomeFacts) {
    let finish_millis = 1_800_000_000_000 + (running.clock - 1) * 60_000 + 30_000;
    let last_event_sequence = if matches!(
        running.job.execution_profile.as_str(),
        "reviewer" | "verifier"
    ) {
        ExecutionAckSequence(1)
    } else {
        ExecutionAckSequence(12)
    };
    let message = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: running.binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 264)),
        outcome: ExecutionOutcome {
            artifacts: artifacts.clone(),
            codex_thread_id: Some(running.binding.codex_thread_id.clone()),
            error: None,
            finished_at: attempt_time(running.clock, 30),
            last_event_sequence,
            status: ExecutionOutcomeStatus::Succeeded,
            summary: format!(
                "{} offline role fixture completed",
                running.job.execution_profile
            ),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: Some(400),
                runtime_millis: 25_000,
                tokens: Some(40),
                known_tokens: 40,
                accounting_status: ExecutionOutcomeUsageAccountingStatus::Known,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(running.clock, 30),
        session_identity: running.binding.session_identity.clone(),
        worker_session_id: running.binding.worker_session_id.clone(),
    };
    let metadata = terminal_outcome_metadata(
        message.outcome.codex_thread_id.clone(),
        finish_millis,
        message.outcome.last_event_sequence.clone(),
        artifacts
            .into_iter()
            .map(|artifact| TerminalArtifactReference {
                artifact_id: artifact.artifact_id,
                digest: artifact.digest,
            })
            .collect(),
    );
    let outcome = terminal_worker_outcome(
        running.binding.work_run_id.clone().expect("role WorkRun"),
        running.job.job_id.clone(),
        1,
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
        TerminalOutcomeStatus::Succeeded,
        metadata,
    );
    (
        message,
        delivery_terminal_outcome_facts(running.authority.clone(), outcome),
    )
}

#[derive(Clone, Copy)]
pub enum ReworkEscape {
    UnauthorizedPath,
    SamePathDifferentHunk,
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", "2027-01-15T07:00:00Z")
        .env("GIT_COMMITTER_DATE", "2027-01-15T07:00:00Z")
        .output()
        .expect("rework fixture Git command");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git UTF-8")
        .trim()
        .to_owned()
}

fn source_lines(value: u32, unrelated: u32) -> String {
    (1..=24).fold(String::new(), |mut source, line| {
        let value = if line == 2 {
            value
        } else if line == 18 {
            unrelated
        } else {
            0
        };
        writeln!(source, "pub const LINE_{line:02}: u32 = {value};")
            .expect("write fixture source into a String");
        source
    })
}

/// The first candidate changes line two. Line eighteen is outside its Git hunk.
pub fn initialize_rework_repository(repository: &Path) -> (String, String) {
    fs::create_dir_all(repository.join("src")).expect("rework repository root");
    git(repository, &["init", "-q"]);
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"rework-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("Cargo project metadata for public verification planning");
    fs::write(repository.join("src/lib.rs"), source_lines(0, 0)).expect("base source");
    git(repository, &["add", "Cargo.toml", "src/lib.rs"]);
    git(repository, &["commit", "-q", "-m", "base"]);
    let base = git(repository, &["rev-parse", "HEAD"]);
    fs::write(repository.join("src/lib.rs"), source_lines(1, 0)).expect("first candidate source");
    git(repository, &["add", "src/lib.rs"]);
    git(repository, &["commit", "-q", "-m", "first candidate"]);
    (base, git(repository, &["rev-parse", "HEAD"]))
}

/// Extend the exact sealed source commit with the authorized edit and one escape.
pub fn commit_rework_escape(
    repository: &Path,
    source_commit: &str,
    escape: ReworkEscape,
) -> String {
    git(repository, &["checkout", "-q", "--detach", source_commit]);
    let unrelated = match escape {
        ReworkEscape::UnauthorizedPath => {
            fs::create_dir_all(repository.join("__pycache__")).expect("unapproved directory");
            fs::write(
                repository.join("__pycache__/main.pyc"),
                b"offline Git fixture",
            )
            .expect("unapproved candidate file");
            0
        }
        ReworkEscape::SamePathDifferentHunk => 1,
    };
    fs::write(repository.join("src/lib.rs"), source_lines(2, unrelated)).expect("rework source");
    git(repository, &["add", "-A"]);
    git(
        repository,
        &["commit", "-q", "-m", "rework with authorization escape"],
    );
    git(repository, &["rev-parse", "HEAD"])
}
