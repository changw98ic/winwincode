// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, LeaseId, ProductSessionId,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_session::{
    RuntimeSourceIdentity, SessionBinding, SessionBindingError, SessionBindingIdentity,
};

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn delivery_identity() -> SessionBindingIdentity {
    SessionBindingIdentity::delivery_stage(
        DeliveryId(id("dlv", '1')),
        Some(DeliveryTaskId(id("dtk", '2'))),
        StageRunId(id("run", '3')),
        ProductSessionId(id("psn", '4')),
        ExecutionJobId(id("job", '5')),
    )
    .expect("canonical Delivery binding identity")
}

#[test]
fn binding_keeps_delivery_stage_and_all_execution_identities_separate() {
    let identity = delivery_identity();
    let binding = SessionBinding::pending(identity.clone()).expect("pending binding");
    let binding = binding
        .accept_worker_session(WorkerSessionId(id("wsn", 'A')))
        .expect("worker session accepted");
    let binding = binding
        .accept_codex_thread(CodexThreadId(id("cdx", 'B')))
        .expect("Codex thread accepted");
    let binding = binding
        .with_source_identity(
            RuntimeSourceIdentity::execution_worker(
                LeaseId(id("lse", '8')),
                WorkerId(id("wrk", '6')),
                WorkerInstanceId(id("wki", '7')),
                WorkerSessionId(id("wsn", 'A')),
            )
            .expect("execution worker source accepted"),
        )
        .expect("runtime source accepted");

    assert_eq!(binding.identity(), &identity);
    assert_eq!(binding.delivery_id(), Some(&DeliveryId(id("dlv", '1'))));
    assert_eq!(
        binding.delivery_task_id(),
        Some(&DeliveryTaskId(id("dtk", '2')))
    );
    assert_eq!(binding.stage_run_id(), Some(&StageRunId(id("run", '3'))));
    assert_eq!(
        binding.product_session_id(),
        &ProductSessionId(id("psn", '4'))
    );
    assert_eq!(binding.execution_job_id(), &ExecutionJobId(id("job", '5')));
    assert_eq!(
        binding.worker_session_id(),
        Some(&WorkerSessionId(id("wsn", 'A')))
    );
    assert_eq!(
        binding.codex_thread_id(),
        Some(&CodexThreadId(id("cdx", 'B')))
    );
    assert!(binding.source_identity().is_some());
    assert!(binding.is_complete());
}

#[test]
fn product_session_scope_does_not_invent_delivery_or_stage_run() {
    let identity = SessionBindingIdentity::product_session(
        ProductSessionId(id("psn", '1')),
        ExecutionJobId(id("job", '2')),
    )
    .expect("canonical ProductSession binding identity");
    let binding = SessionBinding::pending(identity).expect("pending Chat binding");

    assert_eq!(binding.delivery_id(), None);
    assert_eq!(binding.delivery_task_id(), None);
    assert_eq!(binding.stage_run_id(), None);
}

#[test]
fn codex_thread_requires_a_worker_session_and_rejects_conflicting_rebind() {
    let binding = SessionBinding::pending(delivery_identity()).expect("pending binding");
    let codex = CodexThreadId(id("cdx", '1'));
    assert_eq!(
        binding
            .clone()
            .accept_codex_thread(codex.clone())
            .expect_err("thread before WorkerSession"),
        SessionBindingError::WorkerSessionRequired
    );

    let worker = WorkerSessionId(id("wsn", '2'));
    let binding = binding
        .accept_worker_session(worker)
        .expect("WorkerSession");
    let binding = binding
        .accept_codex_thread(codex.clone())
        .expect("CodexThread");
    assert_eq!(
        binding
            .clone()
            .accept_codex_thread(codex)
            .expect("same thread replay"),
        binding
    );
    assert_eq!(
        binding
            .accept_codex_thread(CodexThreadId(id("cdx", '3')))
            .expect_err("changed CodexThread conflicts"),
        SessionBindingError::ConflictingIdentity("codexThreadId")
    );
}

#[test]
fn binding_rejects_invalid_scope_relationships_and_source_values() {
    let invalid_delivery = SessionBindingIdentity::delivery_stage(
        DeliveryId(id("dlv", '1')),
        None,
        StageRunId(id("run", '2')),
        ProductSessionId(id("psn", '3')),
        ExecutionJobId(id("job", '4')),
    )
    .expect("Delivery-level binding without task is valid");
    assert_eq!(invalid_delivery.delivery_task_id(), None);

    let binding = SessionBinding::pending(delivery_identity()).expect("binding");
    assert_eq!(
        binding
            .with_source_identity(
                RuntimeSourceIdentity::execution_worker(
                    LeaseId(id("lse", '1')),
                    WorkerId(id("wrk", '2')),
                    WorkerInstanceId(id("wki", '3')),
                    WorkerSessionId(id("wsn", '4')),
                )
                .expect("canonical source identity"),
            )
            .expect("source identity is accepted")
            .source_identity()
            .expect("source identity"),
        &RuntimeSourceIdentity::execution_worker(
            LeaseId(id("lse", '1')),
            WorkerId(id("wrk", '2')),
            WorkerInstanceId(id("wki", '3')),
            WorkerSessionId(id("wsn", '4')),
        )
        .expect("canonical source identity")
    );
}

#[test]
fn binding_rejects_a_source_identity_for_another_worker_session() {
    let binding = SessionBinding::pending(delivery_identity()).expect("binding");
    let binding = binding
        .accept_worker_session(WorkerSessionId(id("wsn", '1')))
        .expect("worker session");
    let source = RuntimeSourceIdentity::execution_worker(
        LeaseId(id("lse", '2')),
        WorkerId(id("wrk", '3')),
        WorkerInstanceId(id("wki", '4')),
        WorkerSessionId(id("wsn", '5')),
    )
    .expect("source identity");

    assert_eq!(
        binding
            .with_source_identity(source)
            .expect_err("foreign source identity"),
        SessionBindingError::ConflictingIdentity("sourceIdentity.workerSessionId")
    );
}

#[test]
fn binding_identity_matching_is_exact_and_does_not_accept_foreign_job_or_stage() {
    let binding = SessionBinding::pending(delivery_identity()).expect("binding");
    assert!(binding.matches_identity(binding.identity()));

    let foreign = SessionBindingIdentity::delivery_stage(
        DeliveryId(id("dlv", '1')),
        Some(DeliveryTaskId(id("dtk", '2'))),
        StageRunId(id("run", '9')),
        ProductSessionId(id("psn", '4')),
        ExecutionJobId(id("job", '5')),
    )
    .expect("foreign StageRun identity");
    assert!(!binding.matches_identity(&foreign));
}
