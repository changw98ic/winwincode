// SPDX-License-Identifier: Apache-2.0

//! Offline input drives the production Worker/Core consumer. The fake Control
//! Plane only accepts real HTTPS frames and supplies exact signed controls.

use super::*;
use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant as WallInstant};

use serde_json::{Value, json};
use winwincode_codex::WorkerExecutionPort;
use winwincode_execution_port::capability_adapter::{
    CapabilityDescriptor, CapabilityHealth, CapabilityOrigin,
};
use winwincode_execution_port::generated::ApprovalRequestMessage;
use winwincode_execution_port::transport::{
    ExecutionPortCore, RemoteExchangeDelivery, RemoteExchangeRequest, RemoteExchangeResponse,
    RemoteTransportAdapter, execution_message_id,
};
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal, ControlPlaneApiPort,
    EventSubscription, RemoteWorkerExchangePort, RemoteWorkerTransportError, RequestAuthenticator,
    ServerConfig, ServerTls, SqliteAuthSessionManager, UserAccountService,
    start_server_with_remote_worker,
};
use winwincode_worker::remote_transport::{
    RemoteWorkerPort, RemoteWorkerPortError, RemoteWorkerTransportHandle,
};

type NativeWorker =
    winwincode_worker::WorkerMain<ScriptedLocalModelPort, winwincode_codex::ProductionCodexAdapter>;

/// Models remain a scripted local boundary. Production `DeviceModels` consumes
/// these same two message families before consulting the remote Server port.
struct ScriptedLocalModelPort {
    remote: RemoteWorkerPort,
    models: RecordedPort,
}

impl WorkerExecutionPort for ScriptedLocalModelPort {
    type Error = RemoteWorkerPortError;

    fn failure_kind(error: &Self::Error) -> winwincode_codex::ExecutionPortFailureKind {
        RemoteWorkerPort::failure_kind(error)
    }

    async fn send(&mut self, message: ExecutionPortMessage) -> Result<(), Self::Error> {
        match message {
            local @ (ExecutionPortMessage::ModelOpenMessage(_)
            | ExecutionPortMessage::ModelAckMessage(_)) => self
                .models
                .send(local)
                .await
                .map_err(|()| RemoteWorkerPortError::Protocol),
            remote => self.remote.send(remote).await,
        }
    }
}

const MCP_SERVER: &str = "native_progress_fixture";
const MCP_TOOL: &str = "progress_probe";
const ORIGIN: &str = "https://client.example";
const TOKEN: &[u8] = b"offline-native-progress-transport";

#[derive(Default)]
struct SlowQuery {
    entered: tokio::sync::Notify,
    started: Mutex<Option<WallInstant>>,
    completed: std::sync::atomic::AtomicBool,
}

impl SlowQuery {
    fn elapsed(&self) -> Duration {
        self.started.lock().unwrap().unwrap().elapsed()
    }
}

impl ControlPlaneApiPort for SlowQuery {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Ok(json!({"ok": true}))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        *self.started.lock().unwrap() = Some(WallInstant::now());
        self.entered.notify_one();
        // The independent timer bounds cleanup even on the original blocked
        // current-thread Server. It does not release the gate on test success.
        std::thread::sleep(Duration::from_secs(2));
        self.completed.store(true, Ordering::SeqCst);
        Ok(json!({"ok": true}))
    }

    fn subscribe(
        &self,
        _: &AuthenticatedPrincipal,
        _: Value,
    ) -> Result<EventSubscription, ApiError> {
        let (_, events) = tokio::sync::mpsc::channel(1);
        Ok(EventSubscription {
            initial_frames: Vec::new(),
            events,
        })
    }

    fn event_control(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Vec<Value>, ApiError> {
        Ok(Vec::new())
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

struct ServerThread {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerThread {
    fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("independent Server runtime shutdown");
    }
}

impl Drop for ServerThread {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct WireCore;

impl ExecutionPortCore for WireCore {
    type Output = ();
    type Error = Infallible;

    fn accept(&mut self, _: &ExecutionPortMessage) -> Result<(), Infallible> {
        Ok(())
    }
}

#[derive(Default)]
struct Boundary {
    messages: Mutex<Vec<ExecutionPortMessage>>,
    controls: Mutex<VecDeque<ExecutionPortMessage>>,
    next_receipt: AtomicU64,
}

impl Boundary {
    fn queue(&self, message: ExecutionPortMessage) {
        assert_eq!(
            FrameDirection::for_message(&message).unwrap(),
            FrameDirection::ControlPlaneToWorker,
            "fixture remote delivery must respect generated direction: {}",
            serde_json::to_value(&message).unwrap()["kind"],
        );
        self.controls.lock().unwrap().push_back(message);
    }

    fn messages(&self) -> Vec<ExecutionPortMessage> {
        self.messages.lock().unwrap().clone()
    }

    fn permit(
        &self,
        action: &winwincode_execution_port::generated::ActionEnforcementRequestMessage,
    ) -> ExecutionPortMessage {
        let now = at("2030-01-01T00:00:02.000Z");
        let mut receipt = ActionEnforcementReceiptMessage {
            actor: UserActor {
                id: UserId(id("usr", 9)),
                kind: UserActorKind::User,
            },
            decision: ActionEnforcementDecision::Permit,
            evaluated_at: now.clone(),
            evaluation_sha256: digest('e'),
            job_id: action.job_id.clone(),
            kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
            lease: action.lease.clone(),
            matched_condition_sha256: action.matched_condition_sha256.clone(),
            message_id: ExecutionMessageId(id(
                "xmsg",
                4_000 + self.next_receipt.fetch_add(1, Ordering::SeqCst),
            )),
            policy_kind: action.policy_kind.clone(),
            policy_mode: None,
            policy_version: None,
            receipt_signature: digest('0'),
            request_id: action.request_id.clone(),
            resource: action.resource.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: repository_scope(),
            sent_at: now,
            session_identity: action.session_identity.clone(),
            subject_sha256: action.subject_sha256.clone(),
            worker_session_id: action.worker_session_id.clone(),
        };
        ActionEnforcementIssuer::new(action_signing_key())
            .sign(&mut receipt)
            .unwrap();
        ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt)
    }
}

impl RemoteWorkerExchangePort for Boundary {
    fn exchange(
        &self,
        credential: Vec<u8>,
        body: &[u8],
        _: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        assert_eq!(credential, TOKEN);
        let request = RemoteExchangeRequest::decode(body).expect("bounded real HTTPS exchange");
        assert!(request.supports_acceptance_receipt());
        let frame = RemoteTransportAdapter::<WireCore>::decode(request.frame()).unwrap();
        let message = frame.message().clone();
        match &message {
            ExecutionPortMessage::WorkerRegisterMessage(register) => {
                self.queue(ExecutionPortMessage::WorkerRegistrationResultMessage(
                    WorkerRegistrationResultMessage {
                        error: None,
                        heartbeat_interval_ms: 2_000,
                        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
                        lease_recovery:
                            WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
                        message_id: ExecutionMessageId(id("xmsg", 3_001)),
                        request_id: register.request_id.clone(),
                        schema_version: SchemaVersion::WinwincodeV1,
                        sent_at: at("2030-01-01T00:00:00.000Z"),
                        server_time: at("2030-01-01T00:00:00.000Z"),
                        status: WorkerRegistrationResultMessageStatus::Accepted,
                        worker_id: register.worker_id.clone(),
                        worker_instance_id: register.worker_instance_id.clone(),
                    },
                ));
            }
            ExecutionPortMessage::ActionEnforcementRequestMessage(action) => {
                self.queue(self.permit(action));
            }
            _ => {}
        }
        self.messages.lock().unwrap().push(message);
        let deliveries = self
            .controls
            .lock()
            .unwrap()
            .drain(..)
            .map(|message| {
                let delivery_id = execution_message_id(&message).unwrap();
                let kind = serde_json::to_value(&message).unwrap()["kind"].clone();
                let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message)
                    .unwrap_or_else(|error| panic!("fixture remote delivery {kind}: {error:?}"));
                RemoteExchangeDelivery {
                    delivery_id,
                    frame: RemoteTransportAdapter::<WireCore>::encode(&frame).unwrap(),
                }
            })
            .collect();
        Ok(RemoteExchangeResponse::with_acceptance(deliveries, true)
            .unwrap()
            .encode()
            .unwrap())
    }
}

async fn drain_controls(worker: &mut NativeWorker, handle: &RemoteWorkerTransportHandle) {
    while let Some((delivery, message)) = handle.next_control().unwrap() {
        worker
            .accept_control(&message, at("2030-01-01T00:00:02.000Z"))
            .await
            .expect("real HTTPS control must pass Worker authority checks");
        handle.confirm(delivery).unwrap();
    }
}

async fn poll_until<T>(
    worker: &mut NativeWorker,
    handle: &RemoteWorkerTransportHandle,
    boundary: &Boundary,
    models: &RecordedPort,
    inspect: impl Fn(&[ExecutionPortMessage]) -> Option<T>,
    label: &str,
) -> T {
    for _ in 0..400 {
        drain_controls(worker, handle).await;
        worker
            .poll_codex(at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        drain_controls(worker, handle).await;
        let mut messages = boundary.messages();
        messages.extend(models.messages());
        if let Some(value) = inspect(&messages) {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}; observed kinds: {:?}",
        boundary
            .messages()
            .iter()
            .map(|message| serde_json::to_value(message).unwrap()["kind"].clone())
            .collect::<Vec<_>>()
    );
}

fn native_worker_config() -> winwincode_worker::WorkerConfig {
    let mut config = worker_config();
    config
        .capabilities
        .features
        .push(WorkerCapabilityFeature::Mcp);
    config
}

fn native_config(root: &TestDirectory) -> winwincode_codex::ProductionCodexConfig {
    winwincode_codex::ProductionCodexConfig::try_new(winwincode_codex::ProductionCodexOptions {
        data_directory: root.worker(),
        helper_executable: helper_executable(),
        helper_release_manifest: helper_release_manifest(),
        provider: PROVIDER_ID.to_owned(),
        model: MODEL_ID.to_owned(),
        gateway_route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "embedded-canonical-loopback".to_owned(),
        },
        registered_capabilities: native_worker_config().capabilities,
        discovered_capabilities: vec![
            CapabilityDescriptor::mcp(
                MCP_SERVER,
                MCP_TOOL,
                "1",
                CapabilityHealth::Healthy,
                CapabilityOrigin::CodexCoreMcp,
            )
            .unwrap(),
        ],
        action_signing_key: action_signing_key(),
        execution_envelope: winwincode_execution_port::action_gateway::ExecutionEnvelopeToken {
            version: 1,
            digest: digest('a'),
        },
        execution_mode: winwincode_codex::ExecutionMode::React,
        observer_mode: winwincode_codex::ObserverMode::Off,
    })
    .unwrap()
}

fn install_mcp(root: &TestDirectory) -> PathBuf {
    let home = root.worker().join("kernel-home");
    fs::create_dir_all(&home).unwrap();
    let server = root.0.join("native-progress-mcp.py");
    fs::write(&server, r"import json,sys
from pathlib import Path
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request: continue
    method=request['method']
    if method=='initialize': result={'protocolVersion':request['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method=='tools/list': result={'tools':[{'name':'progress_probe','description':'Offline native approval progress fixture','inputSchema':{'type':'object','properties':{},'additionalProperties':False}}]}
    elif method=='tools/call':
        params=request.get('params',{})
        Path(__file__).with_suffix('.called').write_text(json.dumps({'method':method,'name':params.get('name'),'arguments':params.get('arguments',{})}))
        result={'content':[{'type':'text','text':'fixture'}]}
    elif method=='ping': result={}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'Method not found'}}),flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
").unwrap();
    fs::write(home.join("config.toml"), format!(
        "[mcp_servers.{MCP_SERVER}]\ncommand = \"/usr/bin/python3\"\nargs = [\"-I\", {:?}]\nstartup_timeout_sec = 5\ntool_timeout_sec = 5\n",
        server.to_string_lossy(),
    )).unwrap();
    server.with_extension("called")
}

fn approval_state(
    root: &TestDirectory,
    approval: &ApprovalRequestMessage,
) -> (String, String, String, Option<String>) {
    rusqlite::Connection::open_with_flags(
        root.worker().join("worker-codex.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap().query_row(
        "SELECT operation_kind, operation_id, state, resolution_digest FROM approval_operation WHERE approval_id = ?1",
        [&approval.approval_id.0],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).expect("native MCP approval must be durable before outbound transport")
}

pub(super) async fn run() {
    let root = TestDirectory::new("native-mcp-query-progress");
    let called = install_mcp(&root);
    let dispatch = dispatch(&root);
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert_path = root.0.join("certificate.pem");
    let key_path = root.0.join("private-key.pem");
    fs::write(&cert_path, cert.pem()).unwrap();
    fs::write(&key_path, signing_key.serialize_pem()).unwrap();
    let accounts = Arc::new(UserAccountService::open(root.0.join("auth")).unwrap());
    let sessions = Arc::new(
        SqliteAuthSessionManager::open(
            root.0.join("auth-sessions"),
            vec![AuthSessionBootstrap::new("offline-native-bootstrap").unwrap()],
            vec![Scope::OrganizationScope(OrganizationScope {
                kind: OrganizationScopeKind::Organization,
                organization_id: OrganizationId(id("org", 1)),
            })],
            AuthSessionConfig::default(),
            accounts,
            None,
        )
        .unwrap(),
    );
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    let slow = Arc::new(SlowQuery::default());
    let boundary = Arc::new(Boundary::default());
    let server_config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        "https://control.example",
        ServerTls::Pem {
            certificate_path: cert_path,
            private_key_path: key_path,
        },
        BTreeSet::from([ORIGIN.to_owned()]),
        root.0.join("server"),
        Duration::from_secs(5),
    )
    .unwrap();
    let (ready_sender, ready) = tokio::sync::oneshot::channel();
    let (stop_sender, stop) = tokio::sync::oneshot::channel();
    let server_slow = Arc::clone(&slow);
    let server_boundary = Arc::clone(&boundary);
    // The separated Worker/Core runtime cannot be blocked directly by Server
    // CPU work. A regression must cross the real HTTPS/outbox wait boundary.
    let server_thread = std::thread::Builder::new()
        .name("native-mcp-independent-server".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let running = start_server_with_remote_worker(
                        server_config,
                        sessions,
                        authenticator,
                        server_slow,
                        Some(server_boundary),
                        None,
                    )
                    .await
                    .unwrap();
                    ready_sender.send(running.local_address()).unwrap();
                    let _ = stop.await;
                    running.shutdown().await.unwrap();
                });
        })
        .unwrap();
    let server = ServerThread {
        stop: Some(stop_sender),
        thread: Some(server_thread),
    };
    let address = ready.await.unwrap();
    let origin = format!("https://localhost:{}", address.port());
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_der(cert.der().as_ref()).unwrap())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let login = client.post(format!("{origin}/api/v1/auth/session"))
        .header("Origin", ORIGIN).bearer_auth("offline-native-bootstrap")
        .header("Content-Type", "application/json")
        .body(json!({"schemaVersion":"winwincode/v1","username":"owner","password":"native-progress-password"}).to_string())
        .send().await.unwrap();
    assert_eq!(login.status().as_u16(), 201);
    let cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let credential = root.0.join("execution-credential");
    fs::write(&credential, TOKEN).unwrap();
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    let (port, handle) = RemoteWorkerPort::open(
        &origin,
        cert.der().as_ref(),
        credential,
        WorkerId(id("wrk", 1)),
        WorkerInstanceId(id("wki", 1)),
        Duration::from_secs(5),
    )
    .unwrap();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(native_config(&root)).unwrap();
    let models = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        native_worker_config(),
        ScriptedLocalModelPort {
            remote: port,
            models: models.clone(),
        },
        adapter,
        root.workspace_runtime(),
    );
    worker.start(at("2030-01-01T00:00:00.000Z")).await.unwrap();
    drain_controls(&mut worker, &handle).await;
    boundary.queue(ExecutionPortMessage::JobDispatchMessage(dispatch.clone()));
    worker
        .heartbeat(at("2030-01-01T00:00:02.000Z"))
        .await
        .unwrap();
    drain_controls(&mut worker, &handle).await;
    let first_open = poll_until(
        &mut worker,
        &handle,
        &boundary,
        &models,
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                _ => None,
            })
        },
        "initial native ModelOpen missing",
    )
    .await;
    setup(&root, &first_open, &dispatch.job);
    let mut model_app = application(&root);
    let gateway = opened(
        model_app
            .accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                first_open.clone(),
            )))
            .unwrap(),
    );
    let call_id = "offline-native-mcp-call".to_owned();
    let chunks = provider_chunks(
        &first_open,
        &gateway,
        [
            ProviderStreamEvent::ResponseStarted {
                observed_model_id: None,
                provider_response_id: "offline-native-mcp-response".to_owned(),
            },
            ProviderStreamEvent::ToolCallStarted {
                index: 0,
                provider_call_id: call_id.clone(),
                identity: ProviderToolIdentity::try_new(
                    ProviderToolKind::Function,
                    MCP_TOOL.to_owned(),
                    Some(format!("mcp__{MCP_SERVER}")),
                )
                .unwrap(),
            },
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                provider_call_id: call_id.clone(),
                delta: "{}".to_owned(),
            },
            ProviderStreamEvent::ToolCallEnded {
                index: 0,
                provider_call_id: call_id,
            },
            ProviderStreamEvent::Usage(ProviderTokenUsage {
                input_tokens: 10,
                cached_input_tokens: Some(0),
                cache_write_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 0,
            }),
            ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
        ],
        3_100,
    );
    let final_model_sequence = chunks.last().unwrap().sequence.0;
    for chunk in chunks {
        // Device model input is local. Generated model.chunk is an outbound
        // accounting frame, so it cannot be replayed as a Server delivery.
        // This is the same exact local ModelPort intake used by the existing
        // production verticals; Core approval events remain native output.
        worker
            .accept_control(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("scripted local Device ModelPort input");
    }

    let slow_request = tokio::spawn(async move {
        client.post(format!("{origin}/api/v1/queries"))
            .header("Origin", ORIGIN).header("Cookie", cookie)
            .header("Content-Type", "application/json")
            .body(json!({"schemaVersion":"winwincode/v1","requestId":id("req", 3_100),"query":"fixture.slow"}).to_string())
            .send().await.unwrap().status()
    });
    slow.entered.notified().await;
    let approval = poll_until(
        &mut worker,
        &handle,
        &boundary,
        &models,
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ApprovalRequestMessage(approval) => Some(approval.clone()),
                _ => None,
            })
        },
        "native Core MCP Elicitation was not consumed into ApprovalRequest",
    )
    .await;
    let approval_elapsed = slow.elapsed();
    let pending = approval_state(&root, &approval);
    assert_eq!(pending.0, "mcp");
    assert_eq!(pending.2, "pending");
    assert!(pending.3.is_none());
    let operation: Value = serde_json::from_str(&pending.1).unwrap();
    assert_eq!(operation[0], MCP_SERVER);
    assert!(
        !called.exists(),
        "MCP must wait for its exact approval callback"
    );
    assert_eq!(
        approval
            .action
            .sanitized_detail
            .as_ref()
            .unwrap()
            .reason_code,
        ApprovalActionReasonCode::McpPermission
    );
    boundary.queue(ExecutionPortMessage::ApprovalDecisionMessage(
        ApprovalDecisionMessage {
            approval_id: approval.approval_id.clone(),
            decided_at: at("2030-01-01T00:00:02.000Z"),
            decision: ApprovalDecisionMessageDecision::Approved,
            kind: ApprovalDecisionMessageKind::ApprovalDecision,
            lease: approval.lease.clone(),
            message_id: ExecutionMessageId(id("xmsg", 3_900)),
            reason: None,
            schema_version: SchemaVersion::WinwincodeV1,
            scope: ApprovalDecisionMessageScope::Once,
            sent_at: at("2030-01-01T00:00:02.000Z"),
            session_identity: approval.session_identity.clone(),
            worker_session_id: approval.worker_session_id.clone(),
        },
    ));
    worker
        .heartbeat(at("2030-01-01T00:00:02.000Z"))
        .await
        .unwrap();
    drain_controls(&mut worker, &handle).await;
    poll_until(
        &mut worker,
        &handle,
        &boundary,
        &models,
        |_| called.exists().then_some(()),
        "resolved native MCP callback did not reach tools/call",
    )
    .await;
    let call_elapsed = slow.elapsed();
    let completed_before_query = !slow.completed.load(Ordering::SeqCst);
    let call: Value = serde_json::from_str(&fs::read_to_string(&called).unwrap()).unwrap();
    assert_eq!(
        call,
        json!({"method":"tools/call","name":MCP_TOOL,"arguments":{}})
    );
    let resolved = approval_state(&root, &approval);
    assert_eq!(resolved.2, "resolved");
    assert!(resolved.3.is_some());
    assert_eq!(
        resolved.1, pending.1,
        "exact native operation identity must survive resolution"
    );
    assert!(handle.terminal_error().is_none());
    let final_ack = models
        .messages()
        .into_iter()
        .find_map(|message| match message {
            ExecutionPortMessage::ModelAckMessage(ack)
                if ack.model_exchange_id == first_open.model_exchange_id
                    && ack.ack_sequence.0 == final_model_sequence =>
            {
                Some(ack)
            }
            _ => None,
        })
        .expect("real adapter must acknowledge the final scripted local ModelPort chunk");
    assert_eq!(final_ack.lease, first_open.lease);
    assert_eq!(final_ack.session_identity, first_open.session_identity);
    assert_eq!(final_ack.worker_session_id, first_open.worker_session_id);
    assert_eq!(final_ack.status, LeaseWriteStatus::Accepted);
    assert!(final_ack.error.is_none() && final_ack.replay_from_sequence.is_none());
    let slow_status = slow_request.await.unwrap();
    worker
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .unwrap();
    server.finish();
    println!(
        "native_mcp_progress={}",
        json!({
            "approvalMillis":approval_elapsed.as_millis(), "toolsCallMillis":call_elapsed.as_millis(),
            "completedBeforeSlowQuery":completed_before_query,
            "durablePending":true, "durableResolved":true, "mcpMethod":"tools/call",
            "input":"scripted-local-Device-ModelPort", "transport":"RemoteWorkerPort HTTPS",
            "localModelFinalAckSequence":final_ack.ack_sequence.0,
            "serverRuntime":"independent-current-thread", "workerRuntime":"independent-current-thread",
        })
    );
    assert_eq!(slow_status.as_u16(), 200);
    // This regression verifies ordering while the slow query is blocked.
    // Durations are observations; one second is not a product approval SLA.
    assert!(
        completed_before_query,
        "slow Server query stalled the real Worker/Core MCP consumer: approval={approval_elapsed:?}, tools/call={call_elapsed:?}, before_query={completed_before_query}"
    );
}
