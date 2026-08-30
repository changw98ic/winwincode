// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    ApprovalId, AttentionItemId, ExecutionJobId, FencingToken, InputRequestId, Instant, LeaseId,
    ModelExchangeId, ProductSessionId, RequestId, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_session::{
    AuthenticatedActor, DecisionRouteBinding, ExecutionRoute, InteractionDecision,
    InteractionExpiry, InteractionOutcome, InteractionRegistration, InteractionResponse,
    InteractionRouter, InteractionRoutingError, InteractionSubject, RouteWriteStatus,
    RuntimeRouteAuthority, SessionCancellationRequest, SessionCancellationSnapshot,
};

fn id(prefix: &str, number: u64) -> String {
    format!("{prefix}_{number:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2026-08-27T00:00:{second:02}.000Z"))
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn actor(number: u64) -> AuthenticatedActor {
    AuthenticatedActor::User(UserId(id("usr", number)))
}

fn runtime(number: u64) -> RuntimeRouteAuthority {
    RuntimeRouteAuthority {
        lease_id: LeaseId(id("lse", number)),
        worker_id: WorkerId(id("wrk", number)),
        worker_instance_id: WorkerInstanceId(id("wki", number)),
        worker_session_id: WorkerSessionId(id("wsn", number)),
        attempt: 1,
        fencing_token: FencingToken(number.to_string()),
    }
}

fn active_execution(session: u64, job: u64) -> ExecutionRoute {
    ExecutionRoute {
        product_session_id: ProductSessionId(id("psn", session)),
        stage_run_id: Some(StageRunId(id("run", job))),
        execution_job_id: ExecutionJobId(id("job", job)),
        job_revision: 4,
        runtime: Some(runtime(job)),
        worker_slot_revision: Some(7),
        model_exchange_id: Some(ModelExchangeId(id("mdl", job))),
    }
}

fn queued_execution(session: u64, job: u64) -> ExecutionRoute {
    ExecutionRoute {
        product_session_id: ProductSessionId(id("psn", session)),
        stage_run_id: None,
        execution_job_id: ExecutionJobId(id("job", job)),
        job_revision: 2,
        runtime: None,
        worker_slot_revision: None,
        model_exchange_id: None,
    }
}

fn binding(session: u64, job: u64, revision: u64) -> DecisionRouteBinding {
    DecisionRouteBinding {
        execution: active_execution(session, job),
        action_id: format!("tool.shell.write-{job}"),
        decision_revision: revision,
    }
}

fn register(
    router: &mut InteractionRouter,
    subject: InteractionSubject,
    binding: DecisionRouteBinding,
    authorized_actor: AuthenticatedActor,
    attention_decisions: Vec<&str>,
) {
    router
        .register_interaction(InteractionRegistration {
            subject,
            binding,
            authorized_actor,
            expires_at: at(20),
            attention_decisions: attention_decisions.into_iter().map(str::to_owned).collect(),
        })
        .expect("registration must be valid");
}

#[test]
fn approval_is_bound_to_actor_session_stage_job_lease_action_and_revision() {
    let mut router = InteractionRouter::default();
    let subject = InteractionSubject::Approval(ApprovalId(id("apr", 1)));
    let sealed = binding(1, 1, 8);
    register(
        &mut router,
        subject.clone(),
        sealed.clone(),
        actor(1),
        vec![],
    );

    let response = InteractionResponse {
        request_id: RequestId(id("req", 1)),
        actor: actor(1),
        subject: subject.clone(),
        binding: sealed.clone(),
        decision: InteractionDecision::Approve {
            reason_sha256: digest('a'),
        },
        responded_at: at(10),
    };

    let mut foreign_actor = response.clone();
    foreign_actor.actor = actor(2);
    assert_eq!(
        router.respond(&foreign_actor),
        Err(InteractionRoutingError::ActorMismatch)
    );

    let mut foreign_lease = response.clone();
    foreign_lease.binding.execution.runtime = Some(runtime(2));
    assert_eq!(
        router.respond(&foreign_lease),
        Err(InteractionRoutingError::BindingMismatch)
    );

    let mut stale_revision = response.clone();
    stale_revision.binding.decision_revision = 7;
    assert_eq!(
        router.respond(&stale_revision),
        Err(InteractionRoutingError::RevisionConflict {
            expected: 7,
            actual: 8,
        })
    );

    let applied = router.respond(&response).expect("approval must route");
    assert_eq!(applied.status, RouteWriteStatus::Applied);
    assert_eq!(applied.outcome, InteractionOutcome::Approved);
    assert_eq!(applied.previous_revision, 8);
    assert_eq!(applied.current_revision, 9);
    assert_eq!(applied.binding, sealed);

    let duplicate = router
        .respond(&response)
        .expect("exact replay must resolve");
    assert_eq!(duplicate.status, RouteWriteStatus::Duplicate);
    assert_eq!(duplicate.current_revision, 9);

    let mut conflicting_replay = response.clone();
    conflicting_replay.decision = InteractionDecision::Reject {
        reason_sha256: digest('b'),
    };
    assert_eq!(
        router.respond(&conflicting_replay),
        Err(InteractionRoutingError::IdempotencyConflict)
    );

    let mut second_command = response;
    second_command.request_id = RequestId(id("req", 2));
    assert_eq!(
        router.respond(&second_command),
        Err(InteractionRoutingError::AlreadyResolved)
    );
}

#[test]
fn reject_and_expiry_are_terminal_and_replay_safe() {
    let mut router = InteractionRouter::default();
    let rejection_subject = InteractionSubject::Approval(ApprovalId(id("apr", 2)));
    let rejection_binding = binding(1, 2, 3);
    register(
        &mut router,
        rejection_subject.clone(),
        rejection_binding.clone(),
        actor(1),
        vec![],
    );
    let rejection = InteractionResponse {
        request_id: RequestId(id("req", 3)),
        actor: actor(1),
        subject: rejection_subject,
        binding: rejection_binding,
        decision: InteractionDecision::Reject {
            reason_sha256: digest('c'),
        },
        responded_at: at(10),
    };
    assert_eq!(
        router
            .respond(&rejection)
            .expect("rejection must route")
            .outcome,
        InteractionOutcome::Rejected
    );

    let expiry_subject = InteractionSubject::Approval(ApprovalId(id("apr", 3)));
    let expiry_binding = binding(1, 3, 6);
    register(
        &mut router,
        expiry_subject.clone(),
        expiry_binding.clone(),
        actor(1),
        vec![],
    );
    let expiry = InteractionExpiry {
        request_id: RequestId(id("req", 4)),
        subject: expiry_subject,
        binding: expiry_binding,
        expired_at: at(20),
    };
    let expired = router.expire(&expiry).expect("expiry must route");
    assert_eq!(expired.status, RouteWriteStatus::Applied);
    assert_eq!(expired.outcome, InteractionOutcome::Expired);
    assert_eq!(expired.current_revision, 7);
    assert_eq!(
        router
            .expire(&expiry)
            .expect("expiry replay must resolve")
            .status,
        RouteWriteStatus::Duplicate
    );

    let late_subject = InteractionSubject::Approval(ApprovalId(id("apr", 4)));
    let late_binding = binding(1, 4, 10);
    register(
        &mut router,
        late_subject.clone(),
        late_binding.clone(),
        actor(1),
        vec![],
    );
    let late_approval = InteractionResponse {
        request_id: RequestId(id("req", 5)),
        actor: actor(1),
        subject: late_subject,
        binding: late_binding,
        decision: InteractionDecision::Approve {
            reason_sha256: digest('d'),
        },
        responded_at: at(21),
    };
    assert_eq!(
        router
            .respond(&late_approval)
            .expect("late response must deterministically expire")
            .outcome,
        InteractionOutcome::Expired
    );
}

#[test]
fn input_and_attention_route_only_matching_typed_decisions() {
    let mut router = InteractionRouter::default();
    let input_subject = InteractionSubject::UserInput(InputRequestId(id("inp", 1)));
    let input_binding = binding(2, 5, 2);
    register(
        &mut router,
        input_subject.clone(),
        input_binding.clone(),
        actor(2),
        vec![],
    );
    let wrong_kind = InteractionResponse {
        request_id: RequestId(id("req", 6)),
        actor: actor(2),
        subject: input_subject.clone(),
        binding: input_binding.clone(),
        decision: InteractionDecision::Approve {
            reason_sha256: digest('e'),
        },
        responded_at: at(10),
    };
    assert_eq!(
        router.respond(&wrong_kind),
        Err(InteractionRoutingError::DecisionKindMismatch)
    );
    let input = InteractionResponse {
        decision: InteractionDecision::UserInput {
            value_sha256: digest('f'),
        },
        ..wrong_kind
    };
    assert_eq!(
        router.respond(&input).expect("input must route").outcome,
        InteractionOutcome::InputReceived
    );

    let attention_subject = InteractionSubject::Attention(AttentionItemId(id("att", 1)));
    let attention_binding = binding(2, 6, 12);
    register(
        &mut router,
        attention_subject.clone(),
        attention_binding.clone(),
        actor(2),
        vec!["retry", "abort"],
    );
    let unknown_choice = InteractionResponse {
        request_id: RequestId(id("req", 7)),
        actor: actor(2),
        subject: attention_subject.clone(),
        binding: attention_binding.clone(),
        decision: InteractionDecision::ResolveAttention {
            decision: "ignore".to_owned(),
            resolution_sha256: digest('1'),
        },
        responded_at: at(10),
    };
    assert_eq!(
        router.respond(&unknown_choice),
        Err(InteractionRoutingError::AttentionDecisionNotAllowed)
    );
    let allowed_choice = InteractionResponse {
        decision: InteractionDecision::ResolveAttention {
            decision: "retry".to_owned(),
            resolution_sha256: digest('2'),
        },
        ..unknown_choice
    };
    let attention_receipt = router
        .respond(&allowed_choice)
        .expect("sealed Attention choice must route");
    assert_eq!(
        attention_receipt.outcome,
        InteractionOutcome::AttentionResolved
    );
    assert_eq!(attention_receipt.binding, attention_binding);
}

#[test]
fn product_session_cancel_propagates_to_exact_jobs_worker_and_model_stream() {
    let mut router = InteractionRouter::default();
    router
        .register_cancellation_scope(SessionCancellationSnapshot {
            product_session_id: ProductSessionId(id("psn", 1)),
            revision: 15,
            authorized_actor: actor(1),
            // Reverse order proves output does not depend on registration order.
            active_executions: vec![active_execution(1, 20), queued_execution(1, 10)],
        })
        .expect("first session must register");
    router
        .register_cancellation_scope(SessionCancellationSnapshot {
            product_session_id: ProductSessionId(id("psn", 2)),
            revision: 9,
            authorized_actor: actor(2),
            active_executions: vec![active_execution(2, 30)],
        })
        .expect("sibling session must register");

    let request = SessionCancellationRequest {
        request_id: RequestId(id("req", 8)),
        actor: actor(1),
        product_session_id: ProductSessionId(id("psn", 1)),
        expected_revision: 15,
        reason: "user requested cancellation".to_owned(),
        requested_at: at(11),
    };
    let mut wrong_actor = request.clone();
    wrong_actor.actor = actor(2);
    assert_eq!(
        router.cancel_session(&wrong_actor),
        Err(InteractionRoutingError::ActorMismatch)
    );
    let mut stale = request.clone();
    stale.expected_revision = 14;
    assert_eq!(
        router.cancel_session(&stale),
        Err(InteractionRoutingError::RevisionConflict {
            expected: 14,
            actual: 15,
        })
    );

    let receipt = router
        .cancel_session(&request)
        .expect("session cancellation must route");
    assert_eq!(receipt.status, RouteWriteStatus::Applied);
    assert_eq!(receipt.previous_revision, 15);
    assert_eq!(receipt.current_revision, 16);
    assert_eq!(receipt.routes.len(), 2);
    assert_eq!(receipt.routes[0].job.execution_job_id.0, id("job", 10));
    assert!(receipt.routes[0].worker.is_none());
    assert!(receipt.routes[0].model_stream.is_none());

    let active = &receipt.routes[1];
    assert_eq!(active.job.execution_job_id.0, id("job", 20));
    assert_eq!(active.job.product_session_id.0, id("psn", 1));
    assert_eq!(active.job.expected_revision, 4);
    let worker = active
        .worker
        .as_ref()
        .expect("active Worker must be cancelled");
    assert_eq!(worker.expected_revision, 7);
    assert_eq!(worker.runtime.lease_id.0, id("lse", 20));
    assert_eq!(worker.runtime.worker_session_id.0, id("wsn", 20));
    assert_eq!(worker.runtime.fencing_token.0, "20");
    let model = active
        .model_stream
        .as_ref()
        .expect("active model stream must be cancelled");
    assert_eq!(model.model_exchange_id.0, id("mdl", 20));
    assert_eq!(model.runtime, worker.runtime);

    let duplicate = router
        .cancel_session(&request)
        .expect("exact cancellation replay must resolve");
    assert_eq!(duplicate.status, RouteWriteStatus::Duplicate);
    assert_eq!(duplicate.routes, receipt.routes);

    let mut conflicting = request.clone();
    conflicting.reason = "different input".to_owned();
    assert_eq!(
        router.cancel_session(&conflicting),
        Err(InteractionRoutingError::IdempotencyConflict)
    );

    let sibling = SessionCancellationRequest {
        request_id: RequestId(id("req", 9)),
        actor: actor(2),
        product_session_id: ProductSessionId(id("psn", 2)),
        expected_revision: 9,
        reason: "cancel sibling explicitly".to_owned(),
        requested_at: at(12),
    };
    let sibling_receipt = router
        .cancel_session(&sibling)
        .expect("sibling must remain independently cancellable");
    assert_eq!(sibling_receipt.routes.len(), 1);
    assert_eq!(
        sibling_receipt.routes[0].job.execution_job_id.0,
        id("job", 30)
    );
}

#[test]
fn cancellation_snapshot_rejects_cross_session_and_incomplete_runtime_authority() {
    let mut router = InteractionRouter::default();
    let cross_session = SessionCancellationSnapshot {
        product_session_id: ProductSessionId(id("psn", 1)),
        revision: 1,
        authorized_actor: actor(1),
        active_executions: vec![active_execution(2, 1)],
    };
    assert_eq!(
        router.register_cancellation_scope(cross_session),
        Err(InteractionRoutingError::BindingMismatch)
    );

    let mut incomplete = active_execution(1, 1);
    incomplete.worker_slot_revision = None;
    assert_eq!(
        router.register_cancellation_scope(SessionCancellationSnapshot {
            product_session_id: ProductSessionId(id("psn", 1)),
            revision: 1,
            authorized_actor: actor(1),
            active_executions: vec![incomplete],
        }),
        Err(InteractionRoutingError::InvalidField("runtimeAuthority"))
    );
}
