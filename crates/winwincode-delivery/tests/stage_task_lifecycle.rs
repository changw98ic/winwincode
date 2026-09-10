#![cfg(feature = "test-support")]

use winwincode_delivery::{
    application::stage::{
        DurableTerminalOutcomeInput, TerminalArtifactReference, TerminalOutcomeStatus,
        reconcile_durable_terminal_outcome,
    },
    domain::Delivery,
};
use winwincode_domain::{
    ArtifactId, ExecutionAckSequence, ExecutionJobId, FencingToken, Instant, Sha256Digest,
    WorkRunId,
};

fn fixture() -> Delivery {
    Delivery::decode_json(include_bytes!("fixtures/delivery-main.json"))
        .expect("canonical WorkRun fixture")
}

fn running_fixture() -> Delivery {
    let mut snapshot = fixture().into_snapshot();
    snapshot.status = winwincode_delivery::domain::DeliveryStatus::Executing;
    let run = snapshot
        .work_run_aggregate
        .runs
        .first_mut()
        .expect("fixture run");
    run.state = winwincode_domain::WorkRunState::Running;
    snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::InProgress;
    Delivery::try_from_snapshot(snapshot).expect("running WorkRun fixture")
}

#[test]
fn canonical_workrun_terminal_report_requires_exact_binding() {
    let delivery = running_fixture();
    let run = &delivery.snapshot().work_run_aggregate.runs[0];
    let binding = &delivery.snapshot().session_bindings[0];
    let input = DurableTerminalOutcomeInput {
        work_run_id: run.id.clone(),
        execution_job_id: run.execution_job_id.clone(),
        attempt: run.attempt.cast_unsigned(),
        lease_id: run.lease_id.clone(),
        fencing_token: FencingToken(run.fencing_token.clone()),
        worker_id: run.worker_id.clone(),
        worker_instance_id: run.worker_instance_id.clone(),
        worker_session_id: run.worker_session_id.clone(),
        status: TerminalOutcomeStatus::Succeeded,
        codex_thread_id: Some(binding.codex_thread_id.clone().expect("bound thread")),
        finished_at_millis: 1_800_000_000_020,
        last_event_sequence: ExecutionAckSequence(2),
        artifacts: vec![TerminalArtifactReference {
            artifact_id: ArtifactId("art_01J00000000000000000000000".into()),
            digest: Sha256Digest(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
        }],
        issued_at: Instant("2027-01-15T08:00:00.000Z".into()),
        expires_at: Instant("2027-01-15T08:01:40.000Z".into()),
    };
    assert!(
        reconcile_durable_terminal_outcome(&delivery, input.clone()).is_ok(),
        "valid running report must pass"
    );
    let mut forged = input.clone();
    forged.fencing_token = FencingToken("forged".into());
    assert!(
        reconcile_durable_terminal_outcome(&delivery, forged).is_err(),
        "fencing mismatch must fail closed"
    );
    let mut wrong_run = input.clone();
    wrong_run.work_run_id = WorkRunId("wrn_00000000000000000000000099".into());
    assert!(reconcile_durable_terminal_outcome(&delivery, wrong_run).is_err());
    let mut wrong_job = input;
    wrong_job.execution_job_id = ExecutionJobId("job_00000000000000000000000099".into());
    assert!(reconcile_durable_terminal_outcome(&delivery, wrong_job).is_err());
}
