// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fmt::Write as _;

use winwincode_control_plane::{
    ChatInteractionApiService, ChatInteractionService, ContinueProductSessionCommand,
    ProductSessionApiClock, RecordInputInteractionCommand, WorkerInteractionAuthority,
    WorkerInteractionDeliveryError, WorkerInteractionOutboundPort,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};

fn product_input_scope(request: &wire::InputRequestMessage) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: domain::OrganizationId(fixture_id("org", 1)),
        workspace_id: domain::WorkspaceId(fixture_id("wsp", 1)),
        project_id: domain::ProjectId(fixture_id("prj", 1)),
        repository_id: domain::RepositoryId("rep_0000000000000000000000000A".into()),
        product_session_id: request.session_identity.product_session_id.clone(),
        delivery_id: None,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "fixture constructs real durable admission, lease, slot and ProductSession binding before the public input API"
)]
fn prepare_product_input_authority(
    root: &Fixture,
    open: &wire::ModelOpenMessage,
    request: &wire::InputRequestMessage,
) -> WorkerInteractionAuthority {
    let mut storage = SqliteStorage::open(root.0.join("control-plane")).unwrap();
    let scope = product_input_scope(request);
    let pool = WorkerPoolId(fixture_id("wpl", 1));
    let runtime = WorkerSlotAuthority {
        worker_id: request.lease.worker_id.clone(),
        worker_instance_id: request.lease.worker_instance_id.clone(),
        worker_session_id: request.worker_session_id.clone(),
        codex_thread_id: request.session_identity.codex_thread_id.clone(),
        job_id: request.lease.job_id.clone(),
        lease_id: request.lease.lease_id.clone(),
        attempt: u64::try_from(request.lease.attempt).unwrap(),
        fencing_token: request.lease.fencing_token.clone(),
    };
    let existing = storage
        .worker_session_slots()
        .unwrap()
        .load(&runtime.worker_session_id)
        .unwrap();
    if existing.is_none() {
        let before = domain::Instant("2029-12-31T23:59:59.000Z".into());
        let config = worker_config(&wire::JobDispatchMessage {
            lease: request.lease.clone(),
            ..serde_json::from_value(template("job.dispatch")).unwrap()
        });
        let registration = WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "input-fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: config.capabilities.capability_digest,
            security_zone: "local".into(),
            max_slots: 1,
            message_id: message_id(94_001),
            request_id: domain::RequestId(fixture_id("req", 94_001)),
            sent_at: before.clone(),
            started_at: config.started_at,
            worker_id: runtime.worker_id.clone(),
            worker_instance_id: runtime.worker_instance_id.clone(),
        };
        let mut registry = storage.execution_registry().unwrap();
        registry.register_worker(&registration).unwrap();
        assert_eq!(
            registry
                .record_heartbeat(&WorkerHeartbeatRequest {
                    active_leases: Vec::new(),
                    available_slots: 1,
                    max_slots: 1,
                    running_slots: 0,
                    heartbeat_sequence: domain::ExecutionSequence(1),
                    message_id: message_id(94_002),
                    observed_at: before.clone(),
                    sent_at: before.clone(),
                    worker_id: runtime.worker_id.clone(),
                    worker_instance_id: runtime.worker_instance_id.clone(),
                })
                .unwrap()
                .status,
            LeaseWriteStatus::Accepted
        );
        let limits = ExecutionAdmissionLimits {
            max_concurrent: 1,
            max_queued: 1,
            token_budget: None,
            cost_budget_microunits: None,
            max_runtime_millis: None,
        };
        let boundaries = [
            ExecutionAdmissionBoundary::Organization {
                organization_id: scope.organization_id.clone(),
            },
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
            ExecutionAdmissionBoundary::WorkerPool {
                organization_id: scope.organization_id.clone(),
                worker_pool_id: pool.clone(),
            },
        ];
        let mut admission = storage.execution_admission().unwrap();
        for boundary in boundaries {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .unwrap();
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: domain::UserId(fixture_id("usr", 1)),
                worker_pool_id: pool.clone(),
                job_id: runtime.job_id.clone(),
                request_id: domain::RequestId(fixture_id("req", 94_010)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: None,
                reserved_cost_microunits: None,
                runtime_limit_millis: None,
                submitted_at: at("00:00:00"),
            })
            .unwrap();
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool.clone(),
                job_id: runtime.job_id.clone(),
                request_id: domain::RequestId(fixture_id("req", 94_011)),
                expected_revision: 1,
                started_at: at("00:00:00"),
            })
            .unwrap();
        let job = storage
            .execution_queue()
            .unwrap()
            .load_job(&scope, &runtime.job_id)
            .unwrap()
            .unwrap();
        let claim = ExecutionLeaseClaim {
            expires_at: request.lease.expires_at.clone(),
            fencing_token: runtime.fencing_token.clone(),
            issued_at: request.lease.issued_at.clone(),
            job_id: runtime.job_id.clone(),
            lease_id: runtime.lease_id.clone(),
            message_id: message_id(94_012),
            payload_digest: job.payload_digest,
            request_id: domain::RequestId(fixture_id("req", 94_012)),
            worker_id: runtime.worker_id.clone(),
            worker_instance_id: runtime.worker_instance_id.clone(),
            attempt: runtime.attempt,
        };
        assert_eq!(
            storage
                .execution_registry()
                .unwrap()
                .claim_execution_job(&claim)
                .unwrap()
                .status,
            LeaseWriteStatus::Accepted
        );
        let mut slots = storage.worker_session_slots().unwrap();
        slots
            .configure_resources(
                &runtime.worker_id,
                &runtime.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 1000,
                    max_disk_bytes: 1000,
                    max_processes: 4,
                },
            )
            .unwrap();
        slots
            .open(&WorkerSlotOpenRequest {
                authority: runtime.clone(),
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: domain::RequestId(fixture_id("req", 94_013)),
                opened_at: now(),
            })
            .unwrap();
        let public_scope = winwincode_storage::PublicEventScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        };
        let scope_key = winwincode_storage::receipt_scope_key(&public_scope).unwrap();
        let actor = winwincode_storage::PublicEventActor::User {
            id: domain::UserId(fixture_id("usr", 1)),
        };
        let identity = winwincode_storage::ReceiptIdentity::new(
            winwincode_storage::receipt_actor_key(&actor).unwrap(),
            scope_key.clone(),
            domain::RequestId(fixture_id("req", 94_020)),
        )
        .unwrap();
        let mut service = ProductSessionService::new(&mut storage);
        let revision = service
            .get(&scope_key, &scope.product_session_id)
            .unwrap()
            .unwrap()
            .session()
            .revision();
        service
            .continue_session(&ContinueProductSessionCommand {
                context: winwincode_control_plane::ProductSessionCommandContext {
                    receipt_identity: identity,
                    expected_revision: revision,
                    event_id: domain::ControlPlaneEventId(fixture_id("evt", 94_020)),
                    occurred_at: now(),
                    public_actor: actor,
                    public_scope,
                },
                product_session_id: scope.product_session_id.clone(),
                binding_identity: winwincode_session::SessionBindingIdentity::product_session(
                    scope.product_session_id.clone(),
                    runtime.job_id.clone(),
                )
                .unwrap(),
                runtime_authority: runtime.clone(),
                execution_scope: scope.clone(),
                worker_pool_id: pool.clone(),
                model_exchange_id: open.model_exchange_id.clone(),
            })
            .unwrap();
    }
    let public_scope = winwincode_storage::PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    };
    let scope_key = winwincode_storage::receipt_scope_key(&public_scope).unwrap();
    let product_session_revision = ProductSessionService::new(&mut storage)
        .get(&scope_key, &scope.product_session_id)
        .unwrap()
        .unwrap()
        .session()
        .revision();
    let job_revision = storage
        .execution_admission()
        .unwrap()
        .load_reservation(&scope, &pool, &runtime.job_id)
        .unwrap()
        .unwrap()
        .revision;
    let worker_slot_revision = storage
        .worker_session_slots()
        .unwrap()
        .load(&runtime.worker_session_id)
        .unwrap()
        .unwrap()
        .revision;
    WorkerInteractionAuthority {
        execution_scope: scope,
        worker_pool_id: pool,
        product_session_revision,
        job_revision,
        worker_slot_revision,
    }
}

struct InputApiClock(domain::Instant);
impl ProductSessionApiClock for InputApiClock {
    fn now(&mut self) -> domain::Instant {
        self.0.clone()
    }
}

#[derive(Default)]
struct InputApiOutbound(Vec<wire::ExecutionPortMessage>);
impl WorkerInteractionOutboundPort for InputApiOutbound {
    fn deliver(
        &mut self,
        message: &wire::ExecutionPortMessage,
    ) -> Result<(), WorkerInteractionDeliveryError> {
        self.0.push(message.clone());
        Ok(())
    }
}

fn product_input_response(
    root: &Fixture,
    authority: &WorkerInteractionAuthority,
    request: &wire::InputRequestMessage,
    value: &str,
    clock: domain::Instant,
    index: usize,
) -> Result<wire::ExecutionPortMessage, winwincode_control_plane::ChatInteractionServiceError> {
    let mut storage = SqliteStorage::open(root.0.join("control-plane")).unwrap();
    let receipt =
        ChatInteractionService::new(&mut storage).record_input(&RecordInputInteractionCommand {
            authority: authority.clone(),
            request: request.clone(),
        })?;
    let scope = &authority.execution_scope;
    let command=serde_json::from_value(serde_json::json!({
        "actor":{"kind":"user","id":fixture_id("usr",1)},"command":"input.respond",
        "expectedRevision":receipt.revision,"payload":{
            "executionJobId":request.lease.job_id,"inputRequestId":request.input_request_id,
            "productSessionId":request.session_identity.product_session_id,"sessionIdentity":request.session_identity,
            "status":"provided","value":{"mode":request.mode,"value":value},"workerSessionId":request.worker_session_id,
        },"requestId":fixture_id("req",95_000+u64::try_from(index).unwrap()),"schemaVersion":"winwincode/v1",
        "scope":{"kind":"repository","organizationId":scope.organization_id,"workspaceId":scope.workspace_id,
            "projectId":scope.project_id,"repositoryId":scope.repository_id},
    })).unwrap();
    let mut outbound = InputApiOutbound::default();
    ChatInteractionApiService::new(&mut storage, &mut InputApiClock(clock), &mut outbound)
        .respond_input(command)?;
    assert_eq!(
        outbound.0.len(),
        1,
        "one real CP outbound frame for this accepted answer"
    );
    Ok(outbound.0.pop().unwrap())
}

async fn request_multiquestion_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    count: usize,
    index: usize,
    clock: domain::Instant,
) {
    request_multiquestion_with_marker_at(worker, open, count, index, clock, None).await;
}

async fn request_multiquestion_with_marker_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    count: usize,
    index: usize,
    clock: domain::Instant,
    marker: Option<&Path>,
) {
    request_multiquestion_with_call_id_at(
        worker,
        open,
        count,
        index,
        clock,
        marker,
        "multiquestion-call",
    )
    .await;
}

async fn request_multiquestion_with_call_id_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    count: usize,
    index: usize,
    clock: domain::Instant,
    marker: Option<&Path>,
    call_id: &str,
) {
    let questions = (0..count)
        .map(|index| {
            serde_json::json!({
                "id":format!("choice_{index}"), "header":format!("Choice{index}"),
                "question":format!("Select choice {index}?"),
                "options":[{"label":"continue","description":"Continue with this choice."},
                           {"label":"revise","description":"Revise this choice."}]
            })
        })
        .collect::<Vec<_>>();
    let arguments = serde_json::json!({"questions":questions});
    let mut script = format!(
        "// @exec: {{\"yield_time_ms\":10000}}\nconst inputTool=ALL_TOOLS.find(t=>t.name.endsWith('request_user_input'));\nif (!inputTool) throw new Error('Required input tool unavailable');\nconst result=await tools[inputTool.name]({arguments});\n"
    );
    if let Some(marker) = marker {
        let quoted_path = format!("'{}'", marker.to_str().unwrap().replace('\'', "'\\''"));
        let command =
            serde_json::to_string(&format!("printf '%s' consumed > {quoted_path}")).unwrap();
        write!(script, "const execTool=ALL_TOOLS.find(t=>t.name.endsWith('exec_command'));\nif (!execTool) throw new Error('Required marker tool unavailable');\nawait tools[execTool.name]({{cmd:{command}}});\n").unwrap();
    }
    script.push_str("text(result?.output ?? result);");
    let frames = [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_item_done","item":{"type":"custom_tool_call","name":"exec","call_id":call_id,"input":script}}),
        serde_json::json!({"type":"completed","responseId":"multiquestion-response","tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
    ];
    deliver_frames_at(worker, open, index, &frames, false, clock).await;
}

fn decoded_tool_json(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Object(fields)
            if fields.contains_key("answers")
                || fields.contains_key("resultRef")
                || fields.contains_key("state") =>
        {
            Some(value.clone())
        }
        serde_json::Value::Object(fields) => fields.values().find_map(decoded_tool_json),
        serde_json::Value::Array(values) => values.iter().find_map(decoded_tool_json),
        serde_json::Value::String(text) => serde_json::from_str(text)
            .ok()
            .and_then(|value| decoded_tool_json(&value))
            .or_else(|| {
                text.split_once("Output:\n")
                    .and_then(|(_, json)| serde_json::from_str(json).ok())
                    .and_then(|value| decoded_tool_json(&value))
            }),
        _ => None,
    }
}

fn old_script_marker_requests(root: &Fixture) -> i64 {
    let path = root.0.join("worker/kernel-home/state_5.sqlite");
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    connection.query_row("SELECT COUNT(*) FROM tool_requests WHERE json_extract(request_json,'$.tool_name')='exec_command'",[],|row|row.get(0)).unwrap()
}

fn projected_input_requests(port: &RecordedPort) -> Vec<wire::InputRequestMessage> {
    let mut seen = std::collections::BTreeSet::new();
    port.messages()
        .into_iter()
        .filter_map(|message| match message {
            wire::ExecutionPortMessage::InputRequestMessage(request) => Some(request),
            _ => None,
        })
        .filter(|request| seen.insert(request.input_request_id.0.clone()))
        .collect()
}

async fn wait_two_questions(
    worker: &mut NativeWorker,
    port: &RecordedPort,
) -> Vec<wire::InputRequestMessage> {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
        worker.flush_durable_outbox().await.unwrap();
        let requests = projected_input_requests(port);
        if requests.len() == 2 {
            return requests;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("both real Core questions must reach the product");
}

#[test]
fn partial_multiquestion_input_deadline_preserves_answer_and_expires_only_unanswered_question() {
    run_native(async {
        let root = Fixture::new();
        let dispatch = input_dispatch(&root);
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                now(),
            )
            .await
            .unwrap();
        let open = wait_for_open(&mut worker, &port, 0).await;
        request_multiquestion_at(&mut worker, &open, 2, 0, now()).await;
        let requests = wait_two_questions(&mut worker, &port).await;
        worker
            .accept_control(
                &input_response_at(&requests[0], at("00:01:00")),
                at("00:01:00"),
            )
            .await
            .unwrap();
        mechanism_interactions::renew_at_ten_minutes(&mut worker, &requests[0].lease).await;
        worker
            .accept_control(
                &input_response_at(&requests[1], at("00:15:01")),
                at("00:15:01"),
            )
            .await
            .expect_err("renewal does not extend the original input deadline");
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:15:01")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(&body, "multiquestion-call")
            .expect("the full Core call resumes with one provided and one expired answer");
        let response = decoded_tool_json(&output)
            .expect("actual public exec carries the complete typed input response");
        assert_eq!(
            response["answers"]["choice_0"]["answers"],
            serde_json::json!(["continue"])
        );
        assert_eq!(
            response["answers"]["choice_1"]["answers"],
            serde_json::json!([])
        );
        assert_eq!(
            mechanism_interactions::tool_output_count(&body, "multiquestion-call"),
            1
        );
        Box::pin(worker.shutdown(at("00:15:02"))).await.unwrap();
    });
}

#[test]
fn malformed_multiquestion_input_returns_model_feedback_and_allows_corrected_call() {
    run_native(async {
        let root = Fixture::new();
        let dispatch = input_dispatch(&root);
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                now(),
            )
            .await
            .unwrap();
        let open = wait_for_open(&mut worker, &port, 0).await;
        request_multiquestion_at(&mut worker, &open, 4, 0, now()).await;
        let continuation = wait_open_at(&mut worker, &port, 1, now()).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(&body, "multiquestion-call")
            .expect("malformed arguments return to the model through the Core tool result");
        assert!(
            output.to_string().contains("questions"),
            "the result explains the argument boundary: {output}"
        );
        assert!(projected_input_requests(&port).is_empty());
        assert!(
            !port
                .messages()
                .iter()
                .any(|message| matches!(message, wire::ExecutionPortMessage::JobOutcomeMessage(_)))
        );
        request_multiquestion_with_call_id_at(
            &mut worker,
            &continuation,
            2,
            1,
            now(),
            None,
            "multiquestion-corrected-call",
        )
        .await;
        let requests = wait_two_questions(&mut worker, &port).await;
        for request in &requests {
            worker
                .accept_control(&input_response_at(request, at("00:01:00")), at("00:01:00"))
                .await
                .unwrap();
        }
        let corrected = wait_open_at(&mut worker, &port, 2, at("00:01:00")).await;
        let corrected_body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&corrected.request.data_base64).unwrap())
                .unwrap();
        assert!(tool_output(&corrected_body, "multiquestion-corrected-call").is_some());
        Box::pin(worker.shutdown(at("00:01:01"))).await.unwrap();
    });
}

fn input_call_record(root: &Fixture) -> serde_json::Value {
    let encoded: Vec<u8> = core_database(root)
        .query_row("SELECT record_json FROM input_call", [], |row| row.get(0))
        .unwrap();
    serde_json::from_slice(&encoded).unwrap()
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PartialInputFixture {
    dispatch: wire::JobDispatchMessage,
    original_model_open: wire::ModelOpenMessage,
    requests: Vec<wire::InputRequestMessage>,
    first_response: wire::ExecutionPortMessage,
    final_response: Option<wire::ExecutionPortMessage>,
    original_kernel_session_id: String,
    original_input_call: serde_json::Value,
    original_tool_requests: Vec<(i64, String, String, String)>,
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one subprocess crash fixture covers real partial input persistence and exact Core waiter recovery"
)]
fn partial_multiquestion_answer_is_retained_after_crash_without_restarting_old_call() {
    const EXACT: &str = "mechanism_multiquestion::partial_multiquestion_answer_is_retained_after_crash_without_restarting_old_call";
    partial_input_crash_case(EXACT, false, false);
}

#[test]
fn large_multiquestion_answer_remains_private_after_crash_and_old_call_fails() {
    const EXACT: &str = "mechanism_multiquestion::large_multiquestion_answer_remains_private_after_crash_and_old_call_fails";
    partial_input_crash_case(EXACT, true, false);
}

#[test]
fn completed_multiquestion_answers_remain_retained_after_crash_without_reposting() {
    const EXACT: &str = "mechanism_multiquestion::completed_multiquestion_answers_remain_retained_after_crash_without_reposting";
    partial_input_crash_case(EXACT, false, true);
}

#[allow(
    clippy::too_many_lines,
    reason = "one real subprocess crash preserves the original call and consumes its exact durable input"
)]
fn partial_input_crash_case(exact: &str, large: bool, all_answered: bool) {
    if std::env::var_os("WWC_PARTIAL_INPUT_PHASE").is_none() {
        let root = Fixture::new();
        let barrier = root.0.join("mechanism-barrier");
        fs::create_dir_all(&barrier).unwrap();
        for (phase, expected) in [("prepare", 73), ("recover", 0)] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", exact, "--nocapture", "--test-threads=1"])
                .env("WWC_PARTIAL_INPUT_PHASE", phase)
                .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
                .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
                .output()
                .unwrap();
            eprintln!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.status.code(),
                Some(expected),
                "partial input {phase} reached its exact process boundary"
            );
        }
        return;
    }
    run_native(async move {
        let root = Fixture::new();
        let marker = root.0.join("old-input-post-wait.marker");
        let first_answer = if large {
            format!("{}汉🦀", "汉🦀\n\u{0000}\\\"".repeat(3_333))
        } else {
            "continue".into()
        };
        if std::env::var("WWC_PARTIAL_INPUT_PHASE").unwrap() == "prepare" {
            let dispatch = input_dispatch(&root);
            let config = worker_config(&dispatch);
            let port = RecordedPort::default();
            let mut worker = winwincode_worker::WorkerMain::new(
                config.clone(),
                port.clone(),
                root.adapter_with_owner(&config, true),
                root.workspace_runtime(),
            );
            register(&mut worker, &port, &config).await;
            worker
                .accept_control(
                    &wire::ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                    now(),
                )
                .await
                .unwrap();
            let open = wait_for_open(&mut worker, &port, 0).await;
            request_multiquestion_with_marker_at(&mut worker, &open, 2, 0, now(), Some(&marker))
                .await;
            let requests = wait_two_questions(&mut worker, &port).await;
            assert_eq!(first_answer.chars().count(), if large { 20_000 } else { 8 });
            let authority = prepare_product_input_authority(&root, &open, &requests[0]);
            let first_response = product_input_response(
                &root,
                &authority,
                &requests[0],
                &first_answer,
                at("00:01:00"),
                0,
            )
            .expect("the public CP input API accepts the original raw first answer");
            worker
                .accept_control(&first_response, at("00:01:00"))
                .await
                .unwrap();
            worker
                .accept_control(&first_response, at("00:01:00"))
                .await
                .unwrap();
            assert_eq!(
                input_operation_state(&root, &requests[0].input_request_id.0).0,
                "resolved"
            );
            let final_response = if all_answered {
                fs::write(
                    root.0.join("mechanism-barrier/input.dispatch.arm"),
                    b"pause original Core before delivery",
                )
                .unwrap();
                let response = product_input_response(
                    &root,
                    &authority,
                    &requests[1],
                    "continue",
                    at("00:01:00"),
                    1,
                )
                .expect("the public CP input API accepts the original second answer");
                worker
                    .accept_control(&response, at("00:01:00"))
                    .await
                    .unwrap();
                for _ in 0..1_000 {
                    if root
                        .0
                        .join("mechanism-barrier/input.dispatch.paused")
                        .exists()
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert!(
                    root.0
                        .join("mechanism-barrier/input.dispatch.paused")
                        .exists(),
                    "actual submitted response paused before Core waiter consumes it"
                );
                let applied: String = core_database(&root)
                    .query_row(
                        "SELECT json_extract(record_json,'$.appliedIncarnationId') FROM input_call",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(
                    !applied.is_empty(),
                    "old incarnation records submission before crashing"
                );
                Some(response)
            } else {
                None
            };
            assert_eq!(
                input_operation_state(&root, &requests[1].input_request_id.0).0,
                if all_answered { "resolved" } else { "pending" }
            );
            assert!(
                !root
                    .0
                    .join("mechanism-barrier/input.waiter.consumed")
                    .exists()
            );
            assert!(!marker.exists());
            assert_eq!(old_script_marker_requests(&root), 0);
            let original_input_call = input_call_record(&root);
            assert_eq!(
                original_input_call["answers"][&requests[0].input_request_id.0]["values"],
                serde_json::json!([first_answer])
            );
            if all_answered {
                assert_eq!(
                    original_input_call["answers"][&requests[1].input_request_id.0]["values"],
                    serde_json::json!(["continue"])
                );
            }
            fs::write(
                root.0.join("partial-input.json"),
                serde_json::to_vec(&PartialInputFixture {
                    original_kernel_session_id: stored_run(&root)["kernelSessionId"]
                        .as_str()
                        .unwrap()
                        .into(),
                    original_input_call,
                    original_tool_requests: pending_tool_requests(&root),
                    dispatch,
                    original_model_open: open,
                    requests,
                    first_response,
                    final_response,
                })
                .unwrap(),
            )
            .unwrap();
            std::process::exit(73);
        }
        let saved: PartialInputFixture =
            serde_json::from_slice(&fs::read(root.0.join("partial-input.json")).unwrap()).unwrap();
        let kernel_home = root.0.join("worker/kernel-home");
        assert_eq!(
            winwincode_codex::recover_interrupted_tool_calls(&kernel_home)
                .await
                .unwrap(),
            u64::try_from(saved.original_tool_requests.len()).unwrap(),
            "the confirmed exited process releases the locks for original pending calls"
        );
        assert_eq!(
            winwincode_codex::recover_interrupted_tool_calls(&kernel_home)
                .await
                .unwrap(),
            0,
            "recovery closes each original call once"
        );
        if all_answered {
            fs::remove_file(root.0.join("mechanism-barrier/input.dispatch.arm")).unwrap();
        }
        let mut dispatch = saved.dispatch;
        dispatch.message_id = message_id(93_000);
        dispatch.request_id = domain::RequestId(fixture_id("req", 93_000));
        dispatch.sent_at = at("00:02:00");
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register_at(&mut worker, &port, &config, at("00:02:00")).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("00:02:00"),
            )
            .await
            .unwrap();
        assert!(
            !root
                .0
                .join("mechanism-barrier/input.waiter.consumed")
                .exists(),
            "old ready receipt cannot submit into an uninstalled new waiter"
        );
        if all_answered {
            assert_eq!(
                stored_run(&root)["kernelSessionId"],
                saved.original_kernel_session_id,
                "resume preserves thread UUID while callback incarnation changes"
            );
        }
        worker
            .accept_control(&saved.first_response, at("00:02:00"))
            .await
            .unwrap();
        assert!(
            !root
                .0
                .join("mechanism-barrier/input.waiter.consumed")
                .exists(),
            "a replayed product answer cannot consume before exact current event"
        );
        let open = wait_open_at(&mut worker, &port, 0, at("00:02:00")).await;
        request_multiquestion_with_marker_at(
            &mut worker,
            &open,
            2,
            1,
            at("00:02:01"),
            Some(&marker),
        )
        .await;
        for request in projected_input_requests(&port) {
            assert!(
                saved.requests.iter().any(|original| {
                    serde_json::to_value(original).unwrap()
                        == serde_json::to_value(&request).unwrap()
                }),
                "the recorded transport can retain only the exact original input frames"
            );
        }
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:02:02")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(&body, "multiquestion-call")
            .expect("the restored exact Core waiter consumed both durable answers");
        let recovery = decoded_tool_json(&output)
            .expect("restored input returns typed interrupted-cell evidence");
        assert_eq!(recovery["state"], "reconciled_execution_evidence");
        assert_failed_tool_call(&root, &body, "multiquestion-call");
        assert!(!saved.original_tool_requests.is_empty());
        let mut closed = Vec::new();
        for (sequence, request, input, owner) in &saved.original_tool_requests {
            let record = failed_tool_record(&root, *sequence);
            assert_eq!((&record.0, &record.1, &record.2), (request, input, owner));
            closed.push((*sequence, record));
        }
        assert_eq!(
            input_call_record(&root),
            saved.original_input_call,
            "accepted private answers remain intact without reposting to a new waiter"
        );
        assert_eq!(
            mechanism_interactions::tool_output_count(&body, "multiquestion-call"),
            1
        );
        assert!(
            !root
                .0
                .join("mechanism-barrier/input.waiter.consumed")
                .exists()
        );
        assert!(!marker.exists());
        assert_eq!(old_script_marker_requests(&root), 0);
        worker
            .accept_control(&saved.first_response, at("00:02:02"))
            .await
            .unwrap();
        if let Some(response) = &saved.final_response {
            worker
                .accept_control(response, at("00:02:02"))
                .await
                .unwrap();
        }
        for (sequence, before) in closed {
            assert_eq!(
                failed_tool_record(&root, sequence),
                before,
                "late original answers cannot overwrite the closed request/result pair"
            );
        }
        assert_eq!(input_call_record(&root), saved.original_input_call);
        assert!(
            !root
                .0
                .join("mechanism-barrier/input.waiter.consumed")
                .exists()
        );
        assert!(!marker.exists());
        assert_eq!(old_script_marker_requests(&root), 0);
        Box::pin(worker.shutdown(at("00:02:03"))).await.unwrap();
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "保持合法多问题从真实Core事件到Adapter投影的单fixture路径及原断言"
)]
fn multiquestion_case(mode: &str, count: usize, exact: &str, other: bool) {
    if std::env::var_os("WWC_MECHANISM_MULTIQ_CHILD").is_none() {
        let root = Fixture::new();
        let barrier = root.0.join("mechanism-barrier");
        fs::create_dir_all(&barrier).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                exact,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WWC_MECHANISM_MULTIQ_CHILD", "1")
            .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
            .env("WWC_MECHANISM_CORE_MODE", mode)
            .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "legal {count}-question {mode} request must not end the Job as infrastructure error"
        );
        return;
    }
    let mode = mode.to_owned();
    run_native(async move {
        let root = Fixture::new();
        let dispatch = input_dispatch(&root);
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                now(),
            )
            .await
            .unwrap();
        let open = wait_for_open(&mut worker, &port, 0).await;
        request_multiquestion_at(&mut worker, &open, count, 0, now()).await;
        let mut poll_error_count = 0;
        let mut first_poll_error = None;
        let post_frame_started = std::time::Instant::now();
        for _ in 0..400 {
            if let Err(error) = Box::pin(worker.poll_codex(now())).await {
                poll_error_count += 1;
                first_poll_error.get_or_insert_with(|| format!("{error:?}"));
            }
            worker.flush_durable_outbox().await.unwrap();
            if port.messages().iter().any(|message| {
                matches!(
                    message,
                    wire::ExecutionPortMessage::InputRequestMessage(_)
                        | wire::ExecutionPortMessage::JobOutcomeMessage(_)
                )
            }) || post_frame_started.elapsed() >= std::time::Duration::from_secs(20)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let barrier = PathBuf::from(std::env::var_os("WWC_MECHANISM_INTERACTION_BARRIER").unwrap());
        let actual_event: serde_json::Value = serde_json::from_slice(
            &fs::read(barrier.join("input.request.event.json"))
                .expect("actual Core handler emitted the legal question event"),
        )
        .unwrap();
        let outcomes = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .collect::<Vec<_>>();
        let event_questions =
            find_event_field(&actual_event, "questions").expect("questions on the real Core event");
        let blocking = find_event_field(&actual_event, "isBlocking")
            .expect("blocking flag on real Core event");
        if other {
            assert!(
                event_questions
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|question| question["isOther"] == true),
                "Other permission comes from the actual Core normalization event"
            );
        }
        eprintln!(
            "MECHANISM_M14 {}",
            serde_json::json!({"mode":mode,"modeScope":if mode=="Plan"{"test injected existing Core mode; no production Plan API claim"}else{"production default mode"},"questionCount":count,"actualCoreEvent":actual_event,"outcomes":outcomes,"pollErrorCount":poll_error_count,"firstPollError":first_poll_error,"postFrameElapsedMillis":post_frame_started.elapsed().as_millis(),"inputRequests":port.messages().iter().filter(|message|matches!(message,wire::ExecutionPortMessage::InputRequestMessage(_))).count()})
        );
        assert_eq!(event_questions.as_array().unwrap().len(), count);
        assert_eq!(blocking.as_bool(), Some(mode == "Plan"));
        assert!(
            !outcomes.iter().any(|outcome| outcome.outcome.status
                == wire::ExecutionOutcomeStatus::InfrastructureError),
            "a legal Core event must not terminate its Job with InfrastructureError"
        );
        assert!(
            port.messages().iter().any(|message| matches!(
                message,
                wire::ExecutionPortMessage::InputRequestMessage(_)
            )),
            "legal questions must reach product input projection"
        );
        assert_eq!(
            poll_error_count, 0,
            "valid Core input events must project without Adapter errors"
        );
        let mut seen = std::collections::BTreeSet::new();
        let requests = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::InputRequestMessage(request) => Some(request),
                _ => None,
            })
            .filter(|request| seen.insert(request.input_request_id.0.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            requests.len(),
            count,
            "every question reaches the canonical product input contract"
        );
        let authority = other.then(|| prepare_product_input_authority(&root, &open, &requests[0]));
        let other_value = "  用户的其他选择 🦀  ";
        let mut accepted_responses = Vec::new();
        let mut projection = winwincode_control_plane::chat_interaction_projection::ChatInteractionProjectionLedger::default();
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.lease, requests[0].lease);
            assert_eq!(request.session_identity, requests[0].session_identity);
            assert_eq!(request.expires_at, requests[0].expires_at);
            projection
                .record_input_request(request)
                .expect("each actual Core question is valid product input");
            assert_eq!(request.prompt, format!("Select choice {index}?"));
            let response = if let Some(authority) = &authority {
                let accepted = product_input_response(
                    &root,
                    authority,
                    request,
                    if index == 0 { other_value } else { "continue" },
                    at("00:01:00"),
                    index,
                );
                eprintln!(
                    "INPUT_OTHER_PUBLIC_BOUNDARY {}",
                    serde_json::json!({
                        "actualCoreAllowsOther":true,"originalPromptAllowOther":serde_json::to_value(request).unwrap().get("allowOther"),
                        "publicApiAccepted":accepted.is_ok(),"publicApiError":accepted.as_ref().err().map(|error|format!("{:?}",error.code())),
                        "questionIndex":index,
                    })
                );
                accepted.expect("the actual Core Other permission must permit this value through the public CP API")
            } else {
                input_response_at(request, at("00:01:00"))
            };
            accepted_responses.push(response.clone());
            worker
                .accept_control(&response, at("00:01:00"))
                .await
                .unwrap();
            worker
                .accept_control(&response, at("00:01:00"))
                .await
                .unwrap();
            if index + 1 < count {
                for _ in 0..3 {
                    Box::pin(worker.poll_codex(at("00:01:00"))).await.unwrap();
                }
                assert_eq!(
                    port.messages()
                        .iter()
                        .filter(|message| matches!(
                            message,
                            wire::ExecutionPortMessage::ModelOpenMessage(_)
                        ))
                        .count(),
                    1,
                    "a partial answer set must not settle the original Core waiter"
                );
                let mut changed = response.clone();
                if let wire::ExecutionPortMessage::InputResponseMessage(response) = &mut changed {
                    response.value.as_mut().unwrap().value = "revise".into();
                }
                worker
                    .accept_control(&changed, at("00:01:00"))
                    .await
                    .expect_err("an accepted question answer is immutable");
            }
        }
        assert_eq!(projection.snapshot().events.len(), count);
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:01:00")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(&body, "multiquestion-call")
            .expect("the original Core multiquestion waiter produced a tool result");
        let response = decoded_tool_json(&output)
            .expect("actual public exec carries the complete typed input response");
        assert_eq!(response["answers"].as_object().unwrap().len(), count);
        for index in 0..count {
            assert_eq!(
                response["answers"][format!("choice_{index}")]["answers"],
                serde_json::json!([if other && index == 0 {
                    other_value
                } else {
                    "continue"
                }])
            );
        }
        assert_eq!(
            mechanism_interactions::tool_output_count(&body, "multiquestion-call"),
            1
        );
        for response in &accepted_responses {
            worker
                .accept_control(response, at("00:01:00"))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            Box::pin(worker.poll_codex(at("00:01:00"))).await.unwrap();
        }
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            2
        );
        eprintln!(
            "INPUT_CALL_CONSUMED {}",
            serde_json::json!({"mode":mode,"questionCount":count,"actualWaiterConsumed":true,"toolResultCount":1,"answerCount":response["answers"].as_object().unwrap().len()})
        );
        Box::pin(worker.shutdown(at("00:01:00"))).await.unwrap();
    });
}

fn find_event_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(fields) => fields.get(key).or_else(|| {
            fields
                .values()
                .find_map(|value| find_event_field(value, key))
        }),
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_event_field(value, key))
        }
        _ => None,
    }
}

#[test]
fn m14_default_two_questions_reach_product_without_infrastructure_terminal() {
    multiquestion_case(
        "Default",
        2,
        "mechanism_multiquestion::m14_default_two_questions_reach_product_without_infrastructure_terminal",
        false,
    );
}

#[test]
fn m14_plan_three_questions_reach_product_without_infrastructure_terminal() {
    multiquestion_case(
        "Plan",
        3,
        "mechanism_multiquestion::m14_plan_three_questions_reach_product_without_infrastructure_terminal",
        false,
    );
}

#[test]
fn actual_core_other_permission_reaches_public_input_api_and_original_waiter() {
    multiquestion_case(
        "Default",
        2,
        "mechanism_multiquestion::actual_core_other_permission_reaches_public_input_api_and_original_waiter",
        true,
    );
}
