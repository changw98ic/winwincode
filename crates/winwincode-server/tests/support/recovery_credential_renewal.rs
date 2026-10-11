// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::RwLock;
use winwincode_control_plane::{
    DurableExecutionPortContext, DurableExecutionPortDelegate, DurableExecutionPortError,
    DurableExecutionPortSupplement,
};
use winwincode_execution_port::{
    generated::ExecutionPortMessage,
    transport::{
        FrameDirection, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
        TypedFrame,
    },
};
use winwincode_server::{
    ProductionRemoteWorkerExchange, RemoteWorkerExchangePort, RepositoryRuntimeScheduler,
    ServerExecutionPortCore, WorkerSessionCredentialService, WorkerSessionRemoteAuthenticator,
};
use winwincode_storage::{CredentialAuditAction, WorkerRegistryScope};

struct Clock(RwLock<Instant>);
impl StandaloneApplicationClock for Clock {
    fn now_instant(&self) -> Instant {
        self.0.read().unwrap().clone()
    }
    fn now_millis(&self) -> u64 {
        u64::try_from(
            time::OffsetDateTime::parse(
                &self.now_instant().0,
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap()
            .unix_timestamp_nanos()
                / 1_000_000,
        )
        .unwrap()
    }
}
struct BaseMessagesOnly;
impl DurableExecutionPortDelegate for BaseMessagesOnly {
    fn accept(
        &mut self,
        _: DurableExecutionPortContext<'_>,
        _: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        panic!("unexpected delegated message")
    }
}
type Core = ServerExecutionPortCore<BaseMessagesOnly>;
fn worker_scope() -> WorkerRegistryScope {
    let scope = api_scope();
    WorkerRegistryScope::Repository {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
    }
}
fn exchange(
    root: &Path,
    app: &StandaloneControlPlaneApplication,
    anchor: &AnchorLaunch,
) -> ProductionRemoteWorkerExchange<Core> {
    ProductionRemoteWorkerExchange::new(
        root,
        Arc::new(WorkerSessionRemoteAuthenticator::new(root, worker_scope())),
        RepositoryRuntimeScheduler::from_application(
            app,
            api_scope(),
            WorkerId(anchor.worker_id.clone()),
            WorkerInstanceId(anchor.worker_instance_id.clone()),
            format!(
                "recovery-renewal-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ),
            std::time::Duration::from_mins(15),
        )
        .unwrap(),
        ServerExecutionPortCore::from_application(
            app,
            api_scope(),
            BaseMessagesOnly,
            std::time::Duration::from_mins(15),
        )
        .unwrap(),
    )
}
fn fixture(kind: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == kind)
        .unwrap()
        .clone()
}
fn send(
    exchange: &ProductionRemoteWorkerExchange<Core>,
    anchor: &AnchorLaunch,
    secret: &str,
    message: serde_json::Value,
    acks: &mut Vec<ExecutionMessageId>,
    now: &Instant,
) -> Vec<ExecutionPortMessage> {
    let description = format!("{} at {}", message["kind"], now.0);
    let frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        serde_json::from_value(message).unwrap(),
    )
    .unwrap();
    let request = RemoteExchangeRequest::new(
        WorkerId(anchor.worker_id.clone()),
        WorkerInstanceId(anchor.worker_instance_id.clone()),
        std::mem::take(acks),
        RemoteTransportAdapter::<Core>::encode(&frame).unwrap(),
    )
    .unwrap()
    .with_acceptance_receipt();
    let bytes = exchange
        .exchange(
            secret.as_bytes().to_vec(),
            &request.encode().unwrap(),
            now.clone(),
        )
        .unwrap_or_else(|error| panic!("{description}: {error}"));
    let response = RemoteExchangeResponse::decode(&bytes).unwrap();
    assert!(response.frame_accepted());
    response
        .deliveries()
        .iter()
        .map(|delivery| {
            acks.push(delivery.delivery_id.clone());
            RemoteTransportAdapter::<Core>::decode(&delivery.frame)
                .unwrap()
                .message()
                .clone()
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn recovery_heartbeats_renew_credentials_and_execution_leases_across_restart() {
    let root = temporary_root("recovery-renewal-chain");
    let baseline = initialize_repository(&root.join("repository"));
    let clock = Arc::new(Clock(RwLock::new(instant("2027-01-15T08:00:00.000Z"))));
    let mut app = compose_application_with_clock(&root, None, clock.clone());
    let holder = canonical_id("usr", 1);
    app.command(
        &principal(&holder),
        CommandFamily::Delivery,
        delivery_create_request(10, 1, &holder, &baseline),
    )
    .unwrap();
    app.command(
        &principal(&holder),
        CommandFamily::Delivery,
        delivery_task_breakdown_request(12, 1, &holder),
    )
    .unwrap();
    app.command(
        &principal(&holder),
        CommandFamily::Delivery,
        workrun_start_request(11, 1, &holder, 2),
    )
    .unwrap();
    let mut storage = open_storage(&root);
    storage
        .user_account_ledger()
        .unwrap()
        .create(&winwincode_domain::UserAccount {
            user_id: UserId(holder.clone()),
            username: "renewal-owner".into(),
            normalized_username: "renewal-owner".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$fixture$fixture".into(),
            created_at: clock.now_instant(),
            updated_at: clock.now_instant(),
            revision: winwincode_domain::Revision(1),
        })
        .unwrap();
    let job_id = queued_job_id(&mut storage);
    let job = storage.load_execution_job_record(&job_id).unwrap().unwrap();
    let (node, instance, occupancy, token, binding) = work_run_device_fixture(&root, 810, &holder);
    let anchor = work_run_anchor(
        &root,
        820,
        &node,
        &instance,
        &holder,
        &occupancy,
        token,
        &binding,
        &job.scope.product_session_id.0,
        &job.work_run_id.unwrap().0,
    );
    settle_launch(&root, &anchor, &occupancy, token);
    app.command(
        &principal(&holder),
        CommandFamily::Delivery,
        workrun_start_request(11, 1, &holder, 2),
    )
    .unwrap();
    let material = winwincode_server::issue_credential_material().unwrap();
    let credential = WorkerSessionCredentialService::new(&mut storage)
        .issue_for_launch(
            &anchor.worker_session_id,
            &anchor.worker_id,
            &anchor.worker_instance_id,
            &anchor.worker_launch_grant_id,
            material.credential_digest(),
            &clock.now_instant(),
        )
        .unwrap();
    let mut wire = exchange(&root, &app, &anchor);
    let mut acks = Vec::new();
    let mut register = fixture("worker.register");
    register["workerId"] = anchor.worker_id.clone().into();
    register["workerInstanceId"] = anchor.worker_instance_id.clone().into();
    register["sentAt"] = clock.now_instant().0.clone().into();
    register["startedAt"] = clock.now_instant().0.clone().into();
    let responses = send(
        &wire,
        &anchor,
        material.material(),
        register,
        &mut acks,
        &clock.now_instant(),
    );
    let dispatch = responses
        .iter()
        .find_map(|message| {
            if let ExecutionPortMessage::JobDispatchMessage(dispatch) = message {
                Some(dispatch.clone())
            } else {
                None
            }
        })
        .expect("registration must claim the real anchored Job");
    let (session, thread) =
        winwincode_execution_port::execution_identity::canonical_dispatch_session_identity(
            &WorkerId(anchor.worker_id.clone()),
            &WorkerInstanceId(anchor.worker_instance_id.clone()),
            &dispatch,
        )
        .unwrap();
    let mut result = fixture("job.dispatch_result");
    result["lease"] = serde_json::to_value(&dispatch.lease).unwrap();
    result["jobId"] = dispatch.job.job_id.0.clone().into();
    result["payloadDigest"] = dispatch.job.payload_digest.0.clone().into();
    result["requestId"] = dispatch.request_id.0.clone().into();
    result["workerSessionId"] = session.0.clone().into();
    result["sentAt"] = clock.now_instant().0.clone().into();
    result["status"] = "accepted".into();
    result["error"] = serde_json::Value::Null;
    send(
        &wire,
        &anchor,
        material.material(),
        result,
        &mut acks,
        &clock.now_instant(),
    );
    let mut bound = fixture("session.binding");
    let scope = serde_json::to_value(&dispatch.job.scope).unwrap();
    bound["lease"] = serde_json::to_value(&dispatch.lease).unwrap();
    for (key, value) in [
        ("workerId", &anchor.worker_id),
        ("workerSessionId", &session.0),
    ] {
        bound[key] = value.clone().into();
    }
    bound["productSessionId"] = scope["productSessionId"].clone();
    bound["workRunId"] = scope["workRunId"].clone();
    bound["attempt"] = dispatch.lease.attempt.into();
    bound["leaseId"] = dispatch.lease.lease_id.0.clone().into();
    bound["fencingToken"] = dispatch.lease.fencing_token.0.clone().into();
    bound["sentAt"] = clock.now_instant().0.clone().into();
    bound["boundAt"] = clock.now_instant().0.clone().into();
    bound["snapshotId"] = serde_json::Value::Null;
    bound["sessionIdentity"]["productSessionId"] = scope["productSessionId"].clone();
    bound["sessionIdentity"]["workRunId"] = scope["workRunId"].clone();
    bound["sessionIdentity"]["workerSessionId"] = session.0.clone().into();
    bound["codexThreadId"] = thread.0.clone().into();
    bound["sessionIdentity"]["codexThreadId"] = thread.0.clone().into();
    bound["sourceIdentity"]["workerId"] = anchor.worker_id.clone().into();
    bound["sourceIdentity"]["workerInstanceId"] = anchor.worker_instance_id.clone().into();
    bound["sourceIdentity"]["workerSessionId"] = session.0.clone().into();
    bound["sourceIdentity"]["leaseId"] = dispatch.lease.lease_id.0.clone().into();
    bound["runtimeContext"]["agentIdentity"]["workerId"] = anchor.worker_id.clone().into();
    bound["runtimeContext"]["agentIdentity"]["name"] =
        dispatch.job.execution_profile.clone().into();
    bound["runtimeContext"]["agentIdentity"]["role"] =
        dispatch.job.execution_profile.clone().into();
    bound["runtimeContext"]["workspace"]["repositoryId"] =
        dispatch.job.workspace.repository_id.0.clone().into();
    send(
        &wire,
        &anchor,
        material.material(),
        bound.clone(),
        &mut acks,
        &clock.now_instant(),
    );
    storage
        .client_node_registry()
        .unwrap()
        .update_presence(&node, ClientPresenceState::Offline, 2)
        .unwrap();
    storage
        .client_occupancy_ledger()
        .unwrap()
        .mark_recovery_pending(
            &occupancy,
            &instant("2027-01-15T10:00:00.000Z"),
            &clock.now_instant(),
        )
        .unwrap();
    let mut renewal_frames = 0;
    let mut worker_lease = dispatch.lease.clone();
    for minute in 1..=65 {
        *clock.0.write().unwrap() = instant(&format!(
            "2027-01-15T{:02}:{:02}:00.000Z",
            8 + minute / 60,
            minute % 60
        ));
        if minute == 33 {
            drop(wire);
            app.shutdown().unwrap();
            app = compose_application_with_clock(&root, None, clock.clone());
            wire = exchange(&root, &app, &anchor);
        }
        let lease = &worker_lease;
        assert!(
            lease.expires_at.0 > clock.now_instant().0,
            "Worker must receive its renewal before expiry at minute {minute}"
        );
        let heartbeat = serde_json::json!({"schemaVersion":"winwincode/v1","kind":"worker.heartbeat",
            "messageId":ulid_id("xmsg", 10000 + minute), "sentAt":clock.now_instant(),"observedAt":clock.now_instant(),
            "workerId":anchor.worker_id,"workerInstanceId":anchor.worker_instance_id,"heartbeatSequence":minute,
            "capacity":{"runningJobs":1,"availableSlots":3},"activeLeases":[{"jobId":job_id,
                "leaseId":lease.lease_id,"attempt":lease.attempt,"fencingToken":lease.fencing_token,
                "expiresAt":lease.expires_at,"lastEventSequence":0}]});
        let messages = send(
            &wire,
            &anchor,
            material.material(),
            heartbeat,
            &mut acks,
            &clock.now_instant(),
        );
        assert!(messages.iter().any(|message|matches!(message,
            ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) if ack.status == winwincode_execution_port::generated::WorkerHeartbeatAckMessageStatus::Accepted && ack.error.is_none())), "heartbeat minute {minute}: {:?}", messages.iter().filter_map(|message| if let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = message {Some((&ack.status, &ack.error))} else {None}).collect::<Vec<_>>());
        for message in &messages {
            if let ExecutionPortMessage::LeaseRenewMessage(renewal) = message {
                assert_eq!(renewal.prior_expires_at, worker_lease.expires_at);
                assert!(
                    winwincode_execution_port::execution_identity::valid_lease_renewal(
                        &worker_lease,
                        renewal,
                        &clock.now_instant()
                    )
                );
                worker_lease = renewal.lease.clone();
                renewal_frames += 1;
            }
        }
        let current = storage
            .worker_session_credential_ledger()
            .unwrap()
            .find_by_digest(material.credential_digest())
            .unwrap()
            .unwrap();
        if minute >= 15 {
            assert_eq!(
                current.revision,
                1 + minute / 15,
                "accepted recovery heartbeat must renew before the credential expires"
            );
        }
    }
    assert!(
        renewal_frames >= 4,
        "real Core must deliver execution lease renewals"
    );
    assert_eq!(
        storage
            .worker_session_credential_ledger()
            .unwrap()
            .audit_trail(&credential.worker_session_credential_id)
            .unwrap()
            .iter()
            .filter(|a| a.action == CredentialAuditAction::Renewed)
            .count(),
        4
    );
    let mut outcome = fixture("job.outcome");
    outcome["lease"] = serde_json::to_value(&worker_lease).unwrap();
    outcome["workerSessionId"] = session.0.clone().into();
    outcome["sessionIdentity"] = bound["sessionIdentity"].clone();
    outcome["sentAt"] = clock.now_instant().0.clone().into();
    outcome["outcome"] = serde_json::json!({"status":"failed", "summary":"fixture terminal failure after recovery",
        "finishedAt":clock.now_instant(), "lastEventSequence":1, "usage":null, "codexThreadId":thread,
        "artifacts":[]});
    let responses = send(
        &wire,
        &anchor,
        material.material(),
        outcome,
        &mut acks,
        &clock.now_instant(),
    );
    assert!(responses.iter().any(|message| matches!(message, ExecutionPortMessage::JobOutcomeAckMessage(ack)
        if ack.status == winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Accepted && ack.error.is_none())),
        "terminal ACK: {:?}", responses.iter().filter_map(|message| if let ExecutionPortMessage::JobOutcomeAckMessage(ack) = message {Some((&ack.status, &ack.error))} else {None}).collect::<Vec<_>>());
    assert_eq!(
        storage
            .load_execution_job_record(&job_id)
            .unwrap()
            .unwrap()
            .state,
        ExecutionJobState::Failed
    );
    assert!(
        storage
            .execution_registry()
            .unwrap()
            .load_live_lease(&job_id, &clock.now_instant())
            .unwrap()
            .is_none(),
        "terminal outcome must release the execution lease"
    );
    drop(wire);
    app.shutdown().unwrap();
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}
