// SPDX-License-Identifier: Apache-2.0

use winwincode_delivery::{
    application::session_binding::{
        SessionBindingAuthority, SessionBindingIdentity, accept_worker_session_with_authority,
        report_codex_thread_with_authority,
    },
    domain::{
        Delivery, DeliveryStatus, SessionAgentIdentity, SessionBinding, SessionBindingId,
        SessionBindingSourceProvenance, SessionRuntimeContext, SessionWorkspace,
    },
    projection::runtime::{
        RuntimeProjection, RuntimeProjectionErrorCode,
        test_support::{
            RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
        },
    },
};
use winwincode_domain::{
    AgentIdentityId, CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    ProductSessionId, RepositoryId, Revision, SchemaVersion, WorkItemId, WorkItemState, WorkRun,
    WorkRunId, WorkRunState, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceRevision,
};

// This tests concurrent independent writers. Candidate verification is covered by
// the same-item writer/reviewer/verifier transaction test, not a second WorkItem.
fn scheduled_delivery_with_independent_writers() -> Delivery {
    let delivery = Delivery::decode_json(include_bytes!("fixtures/delivery-main.json"))
        .expect("canonical Delivery fixture");
    let mut snapshot = delivery.into_snapshot();
    snapshot.status = DeliveryStatus::Ready;
    snapshot.evidence.clear();
    snapshot.verdict = None;

    let aggregate = &mut snapshot.work_run_aggregate;
    aggregate.runs.clear();
    aggregate.items[0].state = WorkItemState::Ready;
    let mut second_item = aggregate.items[0].clone();
    second_item.id = WorkItemId("wit_01J00000000000000000000001".into());
    second_item.title = "Implement the second independent item".into();
    second_item.goal = "Implement a separate, independent change.".into();
    aggregate.items.push(second_item);

    let producer_start = aggregate
        .start_next()
        .expect("producer WorkItem is runnable");
    let producer = active_run(&aggregate.contract.id, producer_start, 1);
    aggregate
        .append_run(producer.clone())
        .expect("producer WorkRun append");

    let second_start = aggregate.start_next().expect("second WorkItem is runnable");
    let second = active_run(&aggregate.contract.id, second_start, 2);
    aggregate
        .append_run(second.clone())
        .expect("second WorkRun append");

    snapshot.session_bindings = vec![
        binding_for(&snapshot, &producer, "executor"),
        binding_for(&snapshot, &second, "executor"),
    ];
    let delivery =
        Delivery::try_from_snapshot(snapshot).expect("scheduled Delivery with two leased WorkRuns");
    let delivery = bind_run(&delivery, &producer, 1);
    bind_run(&delivery, &second, 2)
}

fn active_run(
    contract_id: &winwincode_domain::WorkContractId,
    start: winwincode_delivery::application::workrun::WorkRunStart,
    seed: u64,
) -> WorkRun {
    WorkRun {
        attempt: start.attempt,
        candidate_digest: None,
        codex_thread_id: None,
        contract_revision: Revision(1),
        execution_job_id: ExecutionJobId(format!("job_01J0000000000000000000000{seed}")),
        fencing_token: seed.to_string(),
        id: WorkRunId(format!("wrn_01J0000000000000000000000{seed}")),
        lease_id: LeaseId(format!("lse_01J0000000000000000000000{seed}")),
        product_session_id: Some(ProductSessionId(format!(
            "psn_01J0000000000000000000000{seed}"
        ))),
        revision: Revision(1),
        schema_version: SchemaVersion::WinwincodeV1,
        state: WorkRunState::Leased,
        work_contract_id: contract_id.clone(),
        worker_id: WorkerId(format!("wrk_01J0000000000000000000000{seed}")),
        worker_instance_id: WorkerInstanceId(format!("wki_01J0000000000000000000000{seed}")),
        worker_session_id: WorkerSessionId(format!("wsn_01J0000000000000000000000{seed}")),
        work_item_id: start.work_item.id,
        work_item_revision: start.work_item.revision,
    }
}

fn binding_for(
    snapshot: &winwincode_delivery::domain::DeliverySnapshot,
    run: &WorkRun,
    role: &str,
) -> SessionBinding {
    let mut binding = snapshot
        .session_bindings
        .first()
        .expect("fixture binding")
        .clone();
    binding.id = SessionBindingId(format!("binding:{}", run.id.0));
    binding.work_contract_id = run.work_contract_id.clone();
    binding.work_contract_revision = run.contract_revision.clone();
    binding.work_item_id = run.work_item_id.clone();
    binding.work_item_revision = run.work_item_revision.clone();
    binding.work_run_id = run.id.clone();
    binding.product_session_id = run.product_session_id.clone().expect("run session");
    binding.execution_job_id = run.execution_job_id.clone();
    binding.execution_profile = Some(role.to_owned());
    binding.runtime_context = None;
    binding.worker_session_id = None;
    binding.codex_thread_id = None;
    binding.worker_id = None;
    binding.worker_instance_id = None;
    binding.lease_id = None;
    binding.attempt = u64::try_from(run.attempt).expect("run attempt");
    binding.fencing_token = None;
    binding.source_provenance = SessionBindingSourceProvenance::workrun_start("workrun.appended");
    binding.bound_at_millis = snapshot.updated_at_millis;
    binding
}

fn bind_run(delivery: &Delivery, run: &WorkRun, seed: u64) -> Delivery {
    let identity = SessionBindingIdentity {
        delivery_id: delivery.id().clone(),
        work_contract_id: run.work_contract_id.clone(),
        work_contract_revision: run.contract_revision.clone(),
        work_item_id: run.work_item_id.clone(),
        work_item_revision: run.work_item_revision.clone(),
        work_run_id: run.id.clone(),
        product_session_id: run.product_session_id.clone().expect("run session"),
        execution_job_id: run.execution_job_id.clone(),
    };
    let authority = SessionBindingAuthority::from_execution_port(
        run.worker_id.clone(),
        run.worker_instance_id.clone(),
        run.lease_id.clone(),
        u64::try_from(run.attempt).expect("run attempt"),
        FencingToken(run.fencing_token.clone()),
        run.worker_session_id.clone(),
        ExecutionMessageId(format!("msg_01J0000000000000000000000{seed}")),
    );
    let worker_bound = accept_worker_session_with_authority(
        delivery,
        delivery.revision(),
        &identity,
        &authority,
        delivery.snapshot().updated_at_millis + 1,
    )
    .expect("real WorkerSession binding");
    let role = worker_bound
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == identity.work_run_id)
        .and_then(|binding| binding.execution_profile.clone())
        .expect("execution profile");
    report_codex_thread_with_authority(
        &worker_bound,
        worker_bound.revision(),
        &identity,
        &authority,
        CodexThreadId(format!("cdx_01J0000000000000000000000{seed}")),
        SessionRuntimeContext {
            agent_identity: SessionAgentIdentity {
                id: AgentIdentityId(format!("agt_01J0000000000000000000000{seed}")),
                worker_id: run.worker_id.clone(),
                name: role.clone(),
                role,
            },
            provider: "fixture-provider".to_owned(),
            model: "fixture-model".to_owned(),
            workspace: SessionWorkspace {
                repository_id: RepositoryId("rep_01J00000000000000000000000".to_owned()),
                revision: WorkspaceRevision(format!("git-tree:{}", "a".repeat(64))),
                write_mode: "candidate".to_owned(),
            },
        },
        worker_bound.snapshot().updated_at_millis + 1,
    )
    .expect("real CodexThread binding")
}

#[test]
fn runtime_projection_folds_independent_writers() {
    let delivery = scheduled_delivery_with_independent_writers();
    let runs = &delivery.snapshot().work_run_aggregate.runs;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.state == WorkRunState::Running));
    assert_ne!(runs[0].id, runs[1].id);
    assert_ne!(runs[0].work_item_id, runs[1].work_item_id);

    let bindings = delivery.snapshot().session_bindings.clone();
    let accepted = bindings
        .iter()
        .enumerate()
        .map(|(index, binding)| {
            accepted_binding(
                &delivery,
                &binding.id,
                RuntimeAuthorityFixture {
                    lease_id: binding.lease_id.clone().expect("lease"),
                    fencing_token: binding.fencing_token.clone().expect("fence"),
                    worker_id: binding.worker_id.clone().expect("worker"),
                    worker_instance_id: binding.worker_instance_id.clone().expect("instance"),
                },
                None,
            )
            .map(|accepted| (index, accepted))
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("both writer bindings pass authority checks");
    let mut projection = RuntimeProjection::new(
        &delivery,
        accepted
            .iter()
            .map(|(_, binding)| binding.clone())
            .collect(),
    )
    .expect("both active sessions are projected");

    for (index, binding) in accepted {
        let event = accepted_event(
            &binding,
            1,
            &format!("xevt-active-{index}"),
            RuntimeFactFixture::Checkpoint,
        )
        .expect("accepted runtime checkpoint");
        projection.apply(&event).expect("active session checkpoint");
    }
    assert_eq!(
        projection
            .snapshot()
            .sessions
            .iter()
            .map(|session| session.as_of_sequence)
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
}

#[test]
fn runtime_projection_rejects_a_checkpoint_from_the_other_active_run() {
    let delivery = scheduled_delivery_with_independent_writers();
    let binding = &delivery.snapshot().session_bindings[0];
    let accepted = accepted_binding(
        &delivery,
        &binding.id,
        RuntimeAuthorityFixture {
            lease_id: binding.lease_id.clone().expect("lease"),
            fencing_token: binding.fencing_token.clone().expect("fence"),
            worker_id: binding.worker_id.clone().expect("worker"),
            worker_instance_id: binding.worker_instance_id.clone().expect("instance"),
        },
        None,
    )
    .expect("producer binding");
    let mut projection =
        RuntimeProjection::new(&delivery, vec![accepted.clone()]).expect("producer projection");
    let second_binding = delivery.snapshot().session_bindings[1].clone();
    let second_event = accepted_event(
        &accepted_binding(
            &delivery,
            &second_binding.id,
            RuntimeAuthorityFixture {
                lease_id: second_binding.lease_id.clone().expect("lease"),
                fencing_token: second_binding.fencing_token.clone().expect("fence"),
                worker_id: second_binding.worker_id.clone().expect("worker"),
                worker_instance_id: second_binding.worker_instance_id.clone().expect("instance"),
            },
            None,
        )
        .expect("second writer binding"),
        1,
        "xevt-second-writer-foreign",
        RuntimeFactFixture::Checkpoint,
    )
    .expect("second writer event");
    let error = projection
        .apply(&second_event)
        .expect_err("a different active WorkRun must not join this projection");
    assert_eq!(error.code(), RuntimeProjectionErrorCode::UnboundEvent);
}
