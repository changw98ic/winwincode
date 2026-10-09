// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "test-support")]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use winwincode_api::generated::{Actor, ModelRoute};
use winwincode_codex::{ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexOptions};
use winwincode_control_plane::{
    CreateProductSessionCommand, ProductSessionExecutionConfig, ProductSessionService,
    SubmitChatMessageCommand, product_session_command_context,
};
use winwincode_domain as domain;
use winwincode_execution_port::generated as wire;
use winwincode_storage::{ExecutionQueueScope, SqliteStorage};

type NativeWorker = winwincode_worker::WorkerMain<RecordedPort, ProductionCodexAdapter>;

#[derive(Clone, Default)]
struct RecordedPort(Arc<Mutex<Vec<wire::ExecutionPortMessage>>>);
impl RecordedPort {
    fn messages(&self) -> Vec<wire::ExecutionPortMessage> {
        self.0.lock().unwrap().clone()
    }
}
impl winwincode_codex::WorkerExecutionPort for RecordedPort {
    type Error = ();
    fn send(
        &mut self,
        message: wire::ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), ()>> {
        self.0.lock().unwrap().push(message);
        std::future::ready(Ok(()))
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::var_os("WWC_APPROVAL_RESTART_DIRECTORY").map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "winwincode-runtime-unified-{}-{unique}",
                    std::process::id()
                ))
            },
            PathBuf::from,
        );
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn repository(&self) -> PathBuf {
        self.0.join("sources/rep_0000000000000000000000000A")
    }
    fn source_revision(&self) -> String {
        let repository = self.repository();
        fs::create_dir_all(repository.join("src")).unwrap();
        git(&repository, &["init", "-q"]);
        git(&repository, &["config", "user.name", "Runtime Fixture"]);
        git(
            &repository,
            &["config", "user.email", "runtime@example.invalid"],
        );
        for (path, bytes) in source_files() {
            fs::write(repository.join(path), bytes).unwrap();
        }
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-qm", "source"]);
        git(&repository, &["rev-parse", "HEAD"])
    }
    fn workspace_runtime(&self) -> winwincode_worker::workspace_runtime::JobWorkspaceRuntime {
        winwincode_worker::workspace_runtime::JobWorkspaceRuntime::open(
            self.0.join("workspaces"),
            self.0.join("sources"),
        )
        .unwrap()
    }
    fn adapter_with_owner(
        &self,
        config: &winwincode_worker::WorkerConfig,
        hosted: bool,
    ) -> ProductionCodexAdapter {
        self.adapter_with_mode(config, hosted, winwincode_codex::ExecutionMode::React)
    }
    fn adapter_with_mode(
        &self,
        config: &winwincode_worker::WorkerConfig,
        hosted: bool,
        mode: winwincode_codex::ExecutionMode,
    ) -> ProductionCodexAdapter {
        let helper = std::env::var_os("WWC_TEST_HELPER").map_or_else(
            || {
                PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap())
                    .join("debug/winwincode-kernel-helper")
            },
            PathBuf::from,
        );
        let configuration = ProductionCodexConfig::try_new(ProductionCodexOptions {
            data_directory: self.0.join("worker"),
            helper_release_manifest: winwincode_codex::HelperReleaseManifest::from_test_helper(&helper).unwrap(),
            helper_executable: helper,
            provider: "winwincode-loopback".into(), model: "loopback-model".into(),
            gateway_route: wire::ModelGatewayRoute { capability: "reasoning".into(), route: "embedded-canonical-loopback".into() },
            registered_capabilities: config.capabilities.clone(), discovered_capabilities: Vec::new(),
            action_signing_key: winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey::from_bytes([31; 32]).unwrap(),
            execution_envelope: winwincode_execution_port::action_gateway::ExecutionEnvelopeToken { version: 1, digest: digest(b"test-envelope") },
            execution_mode: mode, observer_mode: winwincode_codex::ObserverMode::Off,
        }).unwrap();
        let configuration = if hosted {
            configuration.with_host_action_approvals()
        } else {
            configuration
        };
        let configuration = if std::env::var_os("WWC_APPROVAL_TIMEOUT_EXIT").is_some() {
            configuration.with_test_interaction_timeout_exit()
        } else {
            configuration
        };
        ProductionCodexAdapter::open(configuration).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("NATIVE_FIXTURE_RETAINED {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn source_files() -> [(&'static str, &'static [u8]); 4] {
    [
        (".gitignore", b"/target/\n/Cargo.lock\n"),
        (
            "Cargo.toml",
            b"[package]\nname=\"runtime-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        ),
        ("fixture.txt", b"source\n"),
        ("src/lib.rs", b"pub fn fixture() {}\n"),
    ]
}
fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn digest(bytes: &[u8]) -> domain::Sha256Digest {
    domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
fn now() -> domain::Instant {
    domain::Instant("2030-01-01T00:00:02.000Z".into())
}
fn message_id(seed: usize) -> domain::ExecutionMessageId {
    domain::ExecutionMessageId(format!("xmsg_{seed:026X}"))
}
fn templates() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap()
}
fn template(kind: &str) -> serde_json::Value {
    templates()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["kind"] == kind)
        .unwrap()
        .clone()
}
fn dispatch(root: &Fixture, role: &str) -> wire::JobDispatchMessage {
    let mut dispatch: wire::JobDispatchMessage =
        serde_json::from_value(template("job.dispatch")).unwrap();
    dispatch.sent_at = now();
    dispatch.lease.issued_at = domain::Instant("2030-01-01T00:00:00.000Z".into());
    dispatch.lease.expires_at = domain::Instant("2030-01-01T00:15:00.000Z".into());
    dispatch.job.limits.deadline_at = Some(domain::Instant("2030-01-01T00:45:00.000Z".into()));
    dispatch.job.execution_profile = role.into();
    dispatch.job.workspace.write_mode = wire::ExecutionWorkspaceWriteMode::ReadOnly;
    dispatch.job.workspace.checkout_revision = root.source_revision();
    let input = dispatch.job.work_input.as_mut().unwrap();
    input.candidate_ref = Some(format!(
        "refs/winwincode/candidates/{}",
        dispatch.job.workspace.checkout_revision
    ));
    input.work_contract.criteria[0].verification_method = Some("cat fixture.txt".into());
    dispatch.job.goal = input.work_item.goal.clone();
    dispatch
}
fn worker_config(dispatch: &wire::JobDispatchMessage) -> winwincode_worker::WorkerConfig {
    winwincode_worker::WorkerConfig {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        started_at: domain::Instant("2029-12-31T23:59:55.000Z".into()),
        capabilities: wire::WorkerCapabilitySet {
            capability_digest: digest(b"capabilities"),
            features: vec![wire::WorkerCapabilityFeature::Sandbox],
            max_concurrent_jobs: 1,
            platform: wire::WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}
async fn register(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    config: &winwincode_worker::WorkerConfig,
) {
    register_at(worker, port, config, now()).await;
}

async fn register_at(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    config: &winwincode_worker::WorkerConfig,
    clock: domain::Instant,
) {
    Box::pin(worker.start(clock.clone())).await.unwrap();
    let request = port
        .messages()
        .into_iter()
        .find_map(|message| {
            if let wire::ExecutionPortMessage::WorkerRegisterMessage(value) = message {
                Some(value.request_id)
            } else {
                None
            }
        })
        .unwrap();
    let mut response: wire::WorkerRegistrationResultMessage =
        serde_json::from_value(template("worker.registration_result")).unwrap();
    response.request_id = request;
    response.worker_id = config.worker_id.clone();
    response.worker_instance_id = config.worker_instance_id.clone();
    response.sent_at = clock.clone();
    response.server_time = clock.clone();
    response.lease_recovery = wire::WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases;
    worker
        .accept_control(
            &wire::ExecutionPortMessage::WorkerRegistrationResultMessage(response),
            clock,
        )
        .await
        .unwrap();
}
async fn wait_for_open(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    index: usize,
) -> wire::ModelOpenMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
        if let Some(open) = port
            .messages()
            .into_iter()
            .filter_map(|message| {
                if let wire::ExecutionPortMessage::ModelOpenMessage(open) = message {
                    Some(open)
                } else {
                    None
                }
            })
            .nth(index)
        {
            return open;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let outcomes = port
        .messages()
        .iter()
        .filter(|message| matches!(message, wire::ExecutionPortMessage::JobOutcomeMessage(_)))
        .count();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for message in port.messages() {
        let value = serde_json::to_value(message).unwrap();
        *counts
            .entry(value["kind"].as_str().unwrap().into())
            .or_default() += 1;
        if value["kind"] == "job.dispatch_result" {
            eprintln!("NATIVE_DISPATCH_RESULT {value}");
        }
    }
    eprintln!(
        "NATIVE_PUBLIC_FACTS active={} message_counts={counts:?}",
        worker.active_jobs().len()
    );
    panic!(
        "Core repair exchange {} was never submitted; outcomes={outcomes}; expected live public coordination without adapter restart",
        index + 1
    );
}
async fn deliver_frames_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    index: usize,
    frames: &[serde_json::Value],
    replay: bool,
    clock: domain::Instant,
) {
    for (sequence, frame) in frames.iter().enumerate() {
        let bytes = serde_json::to_vec(frame).unwrap();
        let message = wire::ModelChunkMessage {
            error: None,
            is_final: sequence == frames.len() - 1,
            kind: wire::ModelChunkMessageKind::ModelChunk,
            lease: open.lease.clone(),
            message_id: message_id(10_000 + index * 10 + sequence),
            model_exchange_id: open.model_exchange_id.clone(),
            payload: Some(wire::EncodedPayload {
                content_type: "application/json".into(),
                data_base64: STANDARD.encode(&bytes),
                payload_digest: digest(&bytes),
            }),
            schema_version: domain::SchemaVersion::WinwincodeV1,
            sent_at: clock.clone(),
            sequence: domain::ExecutionSequence(i64::try_from(sequence + 1).unwrap()),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        };
        let message = wire::ExecutionPortMessage::ModelChunkMessage(message);
        worker
            .accept_control(&message, clock.clone())
            .await
            .unwrap();
        if replay {
            worker
                .accept_control(&message, clock.clone())
                .await
                .unwrap();
        }
    }
}

fn run_native(future: impl Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

fn at(time: &str) -> domain::Instant {
    domain::Instant(format!("2030-01-01T{time}.000Z"))
}

async fn request_escalated_shell(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    marker: &Path,
) {
    request_escalated_shell_at(worker, open, marker, 0, now()).await;
}

async fn request_escalated_shell_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    marker: &Path,
    index: usize,
    clock: domain::Instant,
) {
    let arguments = serde_json::json!({
        "cmd": format!("printf approval-executed > '{}'", marker.display()),
        "sandbox_permissions": "require_escalated",
        "justification": "Exercise the local approval expiry fixture."
    });
    let frames = [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_item_done","item":{"type":"function_call","name":"exec_command","namespace":"functions","call_id":"approval-expiry-call","arguments":arguments.to_string()}}),
        serde_json::json!({"type":"completed","responseId":"approval-expiry-response","tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
    ];
    deliver_frames_at(worker, open, index, &frames, false, clock).await;
}

async fn wait_for_shell_approval(
    worker: &mut NativeWorker,
    port: &RecordedPort,
) -> wire::ApprovalRequestMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
        if let Some(approval) = port.messages().into_iter().find_map(|message| {
            if let wire::ExecutionPortMessage::ApprovalRequestMessage(value) = message {
                Some(value)
            } else {
                None
            }
        }) {
            assert_eq!(
                approval.action.category,
                wire::ApprovalActionCategory::Shell
            );
            assert_eq!(
                approval
                    .action
                    .sanitized_detail
                    .as_ref()
                    .unwrap()
                    .reason_code,
                wire::ApprovalActionReasonCode::SandboxEscalation
            );
            return approval;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let counts = port.messages().iter().fold(
        std::collections::BTreeMap::<String, usize>::new(),
        |mut counts, message| {
            let kind = serde_json::to_value(message).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned();
            *counts.entry(kind).or_default() += 1;
            counts
        },
    );
    panic!("real shell sandbox escalation did not request approval: {counts:?}");
}

async fn renew_before_approval_expiry(
    worker: &mut NativeWorker,
    approval: &wire::ApprovalRequestMessage,
) -> wire::ExecutionLeaseStamp {
    renew_interaction_lease(worker, &approval.lease).await
}

async fn renew_interaction_lease(
    worker: &mut NativeWorker,
    original: &wire::ExecutionLeaseStamp,
) -> wire::ExecutionLeaseStamp {
    let mut value = template("lease.renew");
    let mut lease = original.clone();
    lease.expires_at = at("00:28:00");
    value["messageId"] = serde_json::json!(message_id(60_000));
    value["sentAt"] = serde_json::json!(at("00:13:00"));
    value["priorExpiresAt"] = serde_json::json!(original.expires_at);
    value["lease"] = serde_json::to_value(&lease).unwrap();
    worker
        .accept_control(&serde_json::from_value(value).unwrap(), at("00:13:00"))
        .await
        .unwrap();
    assert_eq!(worker.active_jobs()[0].lease.expires_at, at("00:28:00"));
    lease
}

fn is_rejected_tool_result(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(fields) => {
            let is_output = value["type"] == "function_call_output"
                && value["call_id"] == "approval-expiry-call";
            let is_tool_message =
                value["role"] == "tool" && value["tool_call_id"] == "approval-expiry-call";
            if is_output || is_tool_message {
                let content = value["output"]
                    .as_str()
                    .or_else(|| value["content"].as_str())
                    .unwrap_or("")
                    .to_lowercase();
                return ["denied", "reject", "expired", "approval", "timed out"]
                    .iter()
                    .any(|word| content.contains(word));
            }
            fields.values().any(is_rejected_tool_result)
        }
        serde_json::Value::Array(values) => values.iter().any(is_rejected_tool_result),
        _ => false,
    }
}

async fn assert_approval_wait_converges(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    marker: &Path,
) {
    assert_approval_wait_converges_at(worker, port, marker, at("00:15:01")).await;
}

async fn assert_approval_wait_converges_at(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    marker: &Path,
    expired: domain::Instant,
) {
    for _ in 0..250 {
        Box::pin(worker.poll_codex(expired.clone())).await.unwrap();
        for message in port.messages() {
            match message {
                wire::ExecutionPortMessage::ModelOpenMessage(open) => {
                    let request: serde_json::Value = serde_json::from_slice(
                        &STANDARD.decode(&open.request.data_base64).unwrap(),
                    )
                    .unwrap();
                    if is_rejected_tool_result(&request) {
                        assert!(
                            !marker.exists(),
                            "expired approval must never execute its shell"
                        );
                        return;
                    }
                }
                wire::ExecutionPortMessage::JobOutcomeMessage(outcome) => {
                    assert!(matches!(
                        outcome.outcome.status,
                        wire::ExecutionOutcomeStatus::Failed
                            | wire::ExecutionOutcomeStatus::Cancelled
                    ));
                    assert!(
                        !marker.exists(),
                        "expired approval must never execute its shell"
                    );
                    return;
                }
                _ => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!marker.exists(), "fixture shell did not execute");
    panic!(
        "expired sandbox approval still blocks real Core: no rejected tool result or original Job terminal after the renewed lease kept execution alive"
    );
}

#[test]
fn expired_shell_approval_converges_after_execution_lease_renewal() {
    run_native(async {
        let root = Fixture::new();
        let mut pending = start_pending_shell(&root).await;
        let saved = retained_request(&root, "approvalId", &pending.approval.approval_id.0);
        assert_eq!(pending.approval.expires_at, at("00:15:00"));
        renew_before_approval_expiry(&mut pending.worker, &pending.approval).await;
        assert_unexpired_wait(&mut pending.worker, &pending.port, &root).await;
        assert_approval_wait_converges(&mut pending.worker, &pending.port, &pending.marker).await;
        assert_timeout_settled(&root, &saved);
        assert_no_requeue(&mut pending.worker, &pending.port).await;
        Box::pin(pending.worker.shutdown(at("00:15:01")))
            .await
            .unwrap();
    });
}

struct PendingShell {
    worker: NativeWorker,
    port: RecordedPort,
    dispatch: wire::JobDispatchMessage,
    approval: wire::ApprovalRequestMessage,
    marker: PathBuf,
}

async fn start_pending_shell(root: &Fixture) -> PendingShell {
    let mut dispatch = dispatch(root, "executor");
    dispatch.job.workspace.write_mode = wire::ExecutionWorkspaceWriteMode::Candidate;
    dispatch.job.work_input.as_mut().unwrap().candidate_ref = None;
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
    let marker = root.0.join("approval-must-not-execute");
    request_escalated_shell(&mut worker, &open, &marker).await;
    let approval = wait_for_shell_approval(&mut worker, &port).await;
    PendingShell {
        worker,
        port,
        dispatch,
        approval,
        marker,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RetainedRequest {
    delivery_id: String,
    family: String,
    correlation_key: String,
    digest: String,
    frame: serde_json::Value,
}

fn core_database(root: &Fixture) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        root.0.join("worker/worker-codex.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn retained_request(root: &Fixture, id_field: &str, id: &str) -> RetainedRequest {
    let db = core_database(root);
    let mut statement = db.prepare("SELECT delivery_id,family,correlation_key,frame_digest,frame_json FROM execution_outbox").unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .find_map(|(delivery_id, family, correlation_key, digest, bytes)| {
            let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            (frame[id_field] == id).then_some(RetainedRequest {
                delivery_id,
                family,
                correlation_key,
                digest: {
                    assert_eq!(digest, format!("sha256:{:x}", Sha256::digest(&bytes)));
                    digest
                },
                frame,
            })
        })
        .expect("original durable interaction request")
}

fn stored_run(root: &Fixture) -> serde_json::Value {
    let bytes: Vec<u8> = core_database(root)
        .query_row("SELECT record_json FROM codex_run", [], |row| row.get(0))
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn assert_timeout_settled(root: &Fixture, saved: &RetainedRequest) {
    let record = stored_run(root);
    let timeouts = record["interactionTimeouts"]
        .as_array()
        .expect("durable timeout intent");
    assert_eq!(
        timeouts.len(),
        1,
        "one original request creates one timeout intent"
    );
    let timeout = &timeouts[0];
    assert_eq!(timeout["request"], saved.frame);
    let request: wire::ExecutionPortMessage = serde_json::from_value(saved.frame.clone()).unwrap();
    let mut hash = Sha256::new();
    hash.update(b"winwincode.interaction-timeout-request.v1");
    hash.update([0]);
    hash.update(serde_json::to_vec(&request).unwrap());
    assert_eq!(
        timeout["requestDigest"],
        format!("sha256:{:x}", hash.finalize())
    );
    assert_eq!(
        retained_request(
            root,
            "messageId",
            saved.frame["messageId"].as_str().unwrap()
        )
        .digest,
        saved.digest,
        "the original transport digest also remains unchanged"
    );
    assert_eq!(timeout["request"]["expiresAt"], "2030-01-01T00:15:00.000Z");
    assert!(
        timeout["appliedKernelSessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    let db = core_database(root);
    let (state, acknowledgement_required): (String, i64) = db
        .query_row(
            "SELECT state, acknowledgement_required FROM execution_outbox WHERE delivery_id=?",
            [&saved.delivery_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        state, "sent_attempt",
        "original request remains as audit evidence"
    );
    assert_eq!(
        acknowledgement_required, 0,
        "local settlement stops pending/requeue without inventing a Control Plane response"
    );
    let cp_receipts: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM execution_response_receipt WHERE family=? AND correlation_key=?",
            [&saved.family, &saved.correlation_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        cp_receipts, 0,
        "local timeout is not a Control Plane decision receipt"
    );
    let (table, column, id) = if saved.frame["kind"] == "approval.request" {
        (
            "approval_operation",
            "approval_id",
            saved.frame["approvalId"].as_str().unwrap(),
        )
    } else {
        (
            "input_operation",
            "input_request_id",
            saved.frame["inputRequestId"].as_str().unwrap(),
        )
    };
    let state: String = db
        .query_row(
            &format!("SELECT state FROM {table} WHERE {column}=?"),
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "resolved");
}

async fn assert_unexpired_wait(worker: &mut NativeWorker, port: &RecordedPort, root: &Fixture) {
    for _ in 0..3 {
        Box::pin(worker.poll_codex(at("00:14:59"))).await.unwrap();
    }
    assert!(
        stored_run(root)["interactionTimeouts"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        1
    );
}

async fn assert_no_requeue(worker: &mut NativeWorker, port: &RecordedPort) {
    let count = port
        .messages()
        .iter()
        .filter(|message| {
            matches!(
                message,
                wire::ExecutionPortMessage::ApprovalRequestMessage(_)
            )
        })
        .count();
    for _ in 0..3 {
        Box::pin(worker.poll_codex(at("00:15:02"))).await.unwrap();
        worker.flush_durable_outbox().await.unwrap();
    }
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(
                message,
                wire::ExecutionPortMessage::ApprovalRequestMessage(_)
            ))
            .count(),
        count
    );
}

fn late_approval(approval: &wire::ApprovalRequestMessage) -> wire::ExecutionPortMessage {
    wire::ExecutionPortMessage::ApprovalDecisionMessage(wire::ApprovalDecisionMessage {
        approval_id: approval.approval_id.clone(),
        decided_at: at("00:15:01"),
        decision: wire::ApprovalDecisionMessageDecision::Approved,
        kind: wire::ApprovalDecisionMessageKind::ApprovalDecision,
        lease: approval.lease.clone(),
        message_id: message_id(61_000),
        reason: None,
        schema_version: domain::SchemaVersion::WinwincodeV1,
        scope: wire::ApprovalDecisionMessageScope::Once,
        sent_at: at("00:15:01"),
        session_identity: approval.session_identity.clone(),
        worker_session_id: approval.worker_session_id.clone(),
    })
}

#[test]
fn late_approval_is_rejected_before_the_next_poll_and_never_executes() {
    run_native(async {
        let root = Fixture::new();
        let mut pending = start_pending_shell(&root).await;
        let saved = retained_request(&root, "approvalId", &pending.approval.approval_id.0);
        renew_before_approval_expiry(&mut pending.worker, &pending.approval).await;
        pending
            .worker
            .accept_control(&late_approval(&pending.approval), at("00:15:01"))
            .await
            .expect_err("an alive renewed lease cannot authorize a late approval");
        assert_approval_wait_converges(&mut pending.worker, &pending.port, &pending.marker).await;
        assert_timeout_settled(&root, &saved);
        assert_no_requeue(&mut pending.worker, &pending.port).await;
        assert!(!pending.marker.exists());
        Box::pin(pending.worker.shutdown(at("00:15:02")))
            .await
            .unwrap();
    });
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedShell {
    dispatch: wire::JobDispatchMessage,
    approval: wire::ApprovalRequestMessage,
    request: RetainedRequest,
    marker: PathBuf,
}

fn run_crash_child(root: &Fixture, name: &str, after_resolution: bool) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
        .env("WWC_APPROVAL_PREPARE_CHILD", "1");
    if after_resolution {
        command.env("WWC_APPROVAL_TIMEOUT_EXIT", "1");
    }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(73),
                "fixture must exit at the exact crash seam"
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("crash fixture did not reach the intended process exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

async fn prepare_shell_crash(root: &Fixture, after_resolution: bool) {
    let mut pending = start_pending_shell(root).await;
    let request = retained_request(root, "approvalId", &pending.approval.approval_id.0);
    pending.dispatch.lease =
        renew_before_approval_expiry(&mut pending.worker, &pending.approval).await;
    pending.dispatch.sent_at = at("00:13:00");
    fs::write(
        root.0.join("saved-shell.json"),
        serde_json::to_vec(&SavedShell {
            dispatch: pending.dispatch,
            approval: pending.approval,
            request,
            marker: pending.marker,
        })
        .unwrap(),
    )
    .unwrap();
    if after_resolution {
        Box::pin(pending.worker.poll_codex(at("00:15:01")))
            .await
            .unwrap();
        panic!("configured timeout crash seam was not reached");
    }
    std::process::exit(73);
}

async fn reopen_shell_and_check(root: &Fixture, after_resolution: bool) {
    let mut saved: SavedShell =
        serde_json::from_slice(&fs::read(root.0.join("saved-shell.json")).unwrap()).unwrap();
    let db = core_database(root);
    let (state, ack): (String, i64) = db
        .query_row(
            "SELECT state, acknowledgement_required FROM execution_outbox WHERE delivery_id=?",
            [&saved.request.delivery_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(ack, 1, "crash happened before original transport cleanup");
    assert_eq!(state, "sent_attempt");
    let operation: String = db
        .query_row(
            "SELECT state FROM approval_operation WHERE approval_id=?",
            [&saved.approval.approval_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        operation,
        if after_resolution {
            "resolved"
        } else {
            "pending"
        }
    );
    drop(db);
    saved.dispatch.message_id = message_id(63_000);
    saved.dispatch.request_id = domain::RequestId(format!("req_{:026X}", 63_000));
    saved.dispatch.sent_at = at("00:15:02");
    let config = worker_config(&saved.dispatch);
    let port = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        config.clone(),
        port.clone(),
        root.adapter_with_owner(&config, true),
        root.workspace_runtime(),
    );
    register_at(&mut worker, &port, &config, at("00:15:02")).await;
    worker
        .accept_control(
            &wire::ExecutionPortMessage::JobDispatchMessage(saved.dispatch),
            at("00:15:02"),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    assert_dispatch_accepted(&port);
    for _ in 0..400 {
        Box::pin(worker.poll_codex(at("00:15:02"))).await.unwrap();
        if port
            .messages()
            .iter()
            .any(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let opens = port
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            wire::ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
            _ => None,
        })
        .collect::<Vec<_>>();
    let already_rejected = opens.iter().any(|open| {
        let value: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&open.request.data_base64).unwrap()).unwrap();
        is_rejected_tool_result(&value)
    });
    if !already_rejected {
        let open = opens
            .last()
            .expect("reopened Core must request recovery model input");
        let arguments = serde_json::json!({
            "cmd": format!("printf approval-executed > '{}'", saved.marker.display()),
            "sandbox_permissions": "require_escalated",
            "justification": "Exercise the local approval expiry fixture."
        });
        let frames = [
            serde_json::json!({"type":"created"}),
            serde_json::json!({"type":"output_item_done","item":{"type":"function_call","name":"exec_command","namespace":"functions","call_id":"approval-expiry-call","arguments":arguments.to_string()}}),
            serde_json::json!({"type":"completed","responseId":"approval-expiry-recovery","tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
        ];
        deliver_frames_at(&mut worker, open, 2, &frames, false, at("00:15:02")).await;
    }
    assert_approval_wait_converges_at(&mut worker, &port, &saved.marker, at("00:15:03")).await;
    assert_timeout_settled(root, &saved.request);
    assert_no_requeue(&mut worker, &port).await;
    worker
        .accept_control(&late_approval(&saved.approval), at("00:15:03"))
        .await
        .expect_err("late approval remains rejected after restart");
    assert!(!saved.marker.exists());
    Box::pin(worker.shutdown(at("00:15:03"))).await.unwrap();
}

#[test]
fn pending_approval_process_exit_keeps_original_deadline_and_converges() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_shell_crash(&root, false).await;
            return;
        }
        run_crash_child(
            &root,
            "pending_approval_process_exit_keeps_original_deadline_and_converges",
            false,
        );
        reopen_shell_and_check(&root, false).await;
    });
}

#[test]
fn resolved_timeout_process_exit_finishes_outbox_cleanup_without_cp_decision() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_shell_crash(&root, true).await;
            return;
        }
        run_crash_child(
            &root,
            "resolved_timeout_process_exit_finishes_outbox_cleanup_without_cp_decision",
            true,
        );
        reopen_shell_and_check(&root, true).await;
    });
}

fn fixture_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn input_dispatch(root: &Fixture) -> wire::JobDispatchMessage {
    let scope = domain::RepositoryScope {
        kind: domain::RepositoryScopeKind::Repository,
        organization_id: domain::OrganizationId(fixture_id("org", 1)),
        workspace_id: domain::WorkspaceId(fixture_id("wsp", 1)),
        project_id: domain::ProjectId(fixture_id("prj", 1)),
        repository_id: domain::RepositoryId("rep_0000000000000000000000000A".into()),
    };
    let actor = Actor::UserActor(domain::UserActor {
        id: domain::UserId(fixture_id("usr", 1)),
        kind: domain::UserActorKind::User,
    });
    let product_session_id = domain::ProductSessionId(fixture_id("psn", 901));
    let mut storage = SqliteStorage::open(root.0.join("control-plane")).unwrap();
    let receipt = {
        let mut service = ProductSessionService::new(&mut storage);
        service
            .create(&CreateProductSessionCommand {
                context: product_session_command_context(
                    &actor,
                    &scope,
                    domain::RequestId(fixture_id("req", 901)),
                    &domain::Revision(0),
                    domain::ControlPlaneEventId(fixture_id("evt", 901)),
                    domain::Instant("2029-12-31T23:59:55.000Z".into()),
                )
                .unwrap(),
                product_session_id: product_session_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
                title: "Input expiry fixture".into(),
                model_route: ModelRoute {
                    credential_reference_id: domain::CredentialReferenceId(fixture_id("crd", 1)),
                    provider_id: "winwincode-loopback".into(),
                    model_id: "loopback-model".into(),
                },
            })
            .unwrap();
        service
            .submit_chat(&SubmitChatMessageCommand {
                attachments: Vec::new(),
                context: product_session_command_context(
                    &actor,
                    &scope,
                    domain::RequestId(fixture_id("req", 902)),
                    &domain::Revision(1),
                    domain::ControlPlaneEventId(fixture_id("evt", 902)),
                    at("00:00:00"),
                )
                .unwrap(),
                product_session_id: product_session_id.clone(),
                message: "Reply with the selected input.".into(),
                execution_config: ProductSessionExecutionConfig::try_new(
                    scope.clone(),
                    root.source_revision(),
                    "chat",
                    Some(240),
                    1_000_000,
                )
                .unwrap(),
            })
            .unwrap()
    };
    let queue_scope = ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id,
        delivery_id: None,
    };
    let record = storage
        .execution_queue()
        .unwrap()
        .load_job(&queue_scope, &receipt.turn_intent.execution_job_id)
        .unwrap()
        .unwrap();
    let mut dispatch: wire::JobDispatchMessage =
        serde_json::from_value(template("job.dispatch")).unwrap();
    dispatch.job = serde_json::from_slice(&record.dispatch_payload).unwrap();
    dispatch.job.limits.deadline_at = Some(at("00:45:00"));
    dispatch.lease.job_id = dispatch.job.job_id.clone();
    dispatch.lease.issued_at = at("00:00:00");
    dispatch.lease.expires_at = at("00:15:00");
    dispatch.snapshot_id = None;
    dispatch.sent_at = now();
    dispatch
}

async fn request_input_at(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    call: &str,
    index: usize,
    clock: domain::Instant,
) {
    let arguments = serde_json::json!({"questions":[{
        "id":"continue", "header":"Continue", "question":"Continue this turn?",
        "options":[{"label":"continue","description":"PRIVATE_INPUT_CHOICE"},
                   {"label":"revise","description":"Revise before continuing."}]
    }]});
    let frames = [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_item_done","item":{"type":"function_call",
            "name":"request_user_input","namespace":"functions","call_id":call,
            "arguments":arguments.to_string()}}),
        serde_json::json!({"type":"completed","responseId":format!("input-response-{index}"),
            "tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,
                "reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
    ];
    deliver_frames_at(worker, open, index, &frames, false, clock).await;
}

async fn wait_input_at(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    index: usize,
    clock: domain::Instant,
) -> wire::InputRequestMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(clock.clone())).await.unwrap();
        let mut seen = std::collections::BTreeSet::new();
        if let Some(request) = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::InputRequestMessage(request) => Some(request),
                _ => None,
            })
            .filter(|request| seen.insert(request.input_request_id.0.clone()))
            .nth(index)
        {
            return request;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("real Core request_user_input did not reach its durable input waiter");
}

async fn wait_open_at(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    index: usize,
    clock: domain::Instant,
) -> wire::ModelOpenMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(clock.clone())).await.unwrap();
        if let Some(open) = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .nth(index)
        {
            return open;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("real input result did not resume its Core turn");
}

async fn wait_different_input_at(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    prior: &wire::InputRequestMessage,
    clock: domain::Instant,
) -> wire::InputRequestMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(clock.clone())).await.unwrap();
        if let Some(request) = port
            .messages()
            .into_iter()
            .find_map(|message| match message {
                wire::ExecutionPortMessage::InputRequestMessage(request)
                    if request.input_request_id != prior.input_request_id =>
                {
                    Some(request)
                }
                _ => None,
            })
        {
            return request;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("a different recovery input did not reach its real Core waiter");
}

async fn start_pending_input(
    root: &Fixture,
) -> (
    NativeWorker,
    RecordedPort,
    wire::JobDispatchMessage,
    wire::InputRequestMessage,
) {
    let dispatch = input_dispatch(root);
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
    request_input_at(&mut worker, &open, "input-expiry-call", 0, now()).await;
    let request = wait_input_at(&mut worker, &port, 0, now()).await;
    (worker, port, dispatch, request)
}

fn input_response_at(
    request: &wire::InputRequestMessage,
    clock: domain::Instant,
) -> wire::ExecutionPortMessage {
    wire::ExecutionPortMessage::InputResponseMessage(wire::InputResponseMessage {
        input_request_id: request.input_request_id.clone(),
        kind: wire::InputResponseMessageKind::InputResponse,
        lease: request.lease.clone(),
        message_id: message_id(62_000),
        responded_at: clock.clone(),
        schema_version: domain::SchemaVersion::WinwincodeV1,
        sent_at: clock,
        session_identity: request.session_identity.clone(),
        status: wire::InputResponseMessageStatus::Provided,
        value: Some(domain::InteractiveInputValue {
            mode: request.mode.clone(),
            value: request.choices.as_ref().unwrap()[0].value.clone(),
        }),
        worker_session_id: request.worker_session_id.clone(),
    })
}

fn input_operation_state(root: &Fixture, id: &str) -> (String, String) {
    core_database(root)
        .query_row(
            "SELECT state, turn_id FROM input_operation WHERE input_request_id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn tool_output(value: &serde_json::Value, call: &str) -> Option<serde_json::Value> {
    if value["type"] == "function_call_output" && value["call_id"] == call {
        return Some(value["output"].clone());
    }
    match value {
        serde_json::Value::Object(fields) => {
            fields.values().find_map(|value| tool_output(value, call))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| tool_output(value, call))
        }
        _ => None,
    }
}

#[test]
fn input_timeout_after_lease_renewal_cannot_answer_a_new_input_in_the_same_turn() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port, _, request) = start_pending_input(&root).await;
        let saved = retained_request(&root, "inputRequestId", &request.input_request_id.0);
        assert_eq!(request.expires_at, at("00:15:00"));
        renew_interaction_lease(&mut worker, &request.lease).await;
        assert_unexpired_wait(&mut worker, &port, &root).await;
        worker
            .accept_control(&input_response_at(&request, at("00:15:01")), at("00:15:01"))
            .await
            .expect_err("expired input cannot be answered after lease renewal");
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:15:01")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(&body, "input-expiry-call")
            .expect("empty timeout answer reached real Core");
        assert!(
            output.to_string().contains("[]"),
            "timeout supplies an empty input result"
        );
        assert_timeout_settled(&root, &saved);
        request_input_at(
            &mut worker,
            &continuation,
            "input-new-call",
            1,
            at("00:15:02"),
        )
        .await;
        let newer = wait_input_at(&mut worker, &port, 1, at("00:15:02")).await;
        assert_ne!(newer.input_request_id, request.input_request_id);
        assert_eq!(newer.expires_at, at("00:28:00"));
        let original_operation = input_operation_state(&root, &request.input_request_id.0);
        let new_operation = input_operation_state(&root, &newer.input_request_id.0);
        assert_eq!(original_operation.0, "resolved");
        assert_eq!(new_operation.0, "pending");
        assert_eq!(
            original_operation.1, new_operation.1,
            "the new waiter belongs to the same Core turn"
        );
        worker
            .accept_control(&input_response_at(&request, at("00:15:03")), at("00:15:03"))
            .await
            .expect_err("old input response cannot settle the new waiter");
        for _ in 0..3 {
            Box::pin(worker.poll_codex(at("00:15:03"))).await.unwrap();
        }
        assert_eq!(
            input_operation_state(&root, &newer.input_request_id.0).0,
            "pending"
        );
        assert_eq!(
            stored_run(&root)["interactionTimeouts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
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
        Box::pin(worker.shutdown(at("00:15:04"))).await.unwrap();
    });
}

fn assert_dispatch_accepted(port: &RecordedPort) {
    let results = port
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            wire::ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    if results.is_empty() {
        let kinds = port.messages().into_iter().map(|message| {
            let value = serde_json::to_value(message).unwrap();
            serde_json::json!({"kind":value["kind"],"code":value["code"],"error":value.get("error").and_then(|e| e.get("code"))})
        }).collect::<Vec<_>>();
        panic!("restart dispatch produced no public dispatch receipt: {kinds:?}");
    }
    for result in results {
        eprintln!(
            "RESTART_DISPATCH_RESULT {}",
            serde_json::to_string(&result).unwrap()
        );
        assert_eq!(
            serde_json::to_value(result).unwrap()["status"],
            "accepted",
            "the same Job must actually resume before testing timeout recovery"
        );
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedInput {
    dispatch: wire::JobDispatchMessage,
    request: wire::InputRequestMessage,
    retained: RetainedRequest,
}

async fn prepare_input_crash(root: &Fixture, after_resolution: bool) {
    let (mut worker, _, mut dispatch, request) = start_pending_input(root).await;
    let retained = retained_request(root, "inputRequestId", &request.input_request_id.0);
    dispatch.lease = renew_interaction_lease(&mut worker, &request.lease).await;
    dispatch.sent_at = at("00:13:00");
    fs::write(
        root.0.join("saved-input.json"),
        serde_json::to_vec(&SavedInput {
            dispatch,
            request,
            retained,
        })
        .unwrap(),
    )
    .unwrap();
    if after_resolution {
        Box::pin(worker.poll_codex(at("00:15:01"))).await.unwrap();
        panic!("configured input timeout crash seam was not reached");
    }
    std::process::exit(73);
}

async fn reopen_input(root: &Fixture) -> (NativeWorker, RecordedPort, SavedInput) {
    let mut saved: SavedInput =
        serde_json::from_slice(&fs::read(root.0.join("saved-input.json")).unwrap()).unwrap();
    saved.dispatch.message_id = message_id(63_000);
    saved.dispatch.request_id = domain::RequestId(format!("req_{:026X}", 63_000));
    saved.dispatch.sent_at = at("00:15:02");
    let config = worker_config(&saved.dispatch);
    let port = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        config.clone(),
        port.clone(),
        root.adapter_with_owner(&config, true),
        root.workspace_runtime(),
    );
    register_at(&mut worker, &port, &config, at("00:15:02")).await;
    worker
        .accept_control(
            &wire::ExecutionPortMessage::JobDispatchMessage(saved.dispatch.clone()),
            at("00:15:02"),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    assert_dispatch_accepted(&port);
    (worker, port, saved)
}

async fn assert_new_recovery_waiter_remains_pending(
    worker: &mut NativeWorker,
    port: &RecordedPort,
    root: &Fixture,
    newer: &wire::InputRequestMessage,
) {
    for _ in 0..3 {
        Box::pin(worker.poll_codex(at("00:15:03"))).await.unwrap();
    }
    assert_eq!(
        input_operation_state(root, &newer.input_request_id.0).0,
        "pending"
    );
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        1,
        "old timeout cannot answer a different same-turn recovery waiter"
    );
    let timeout = &stored_run(root)["interactionTimeouts"][0];
    assert_eq!(timeout["waitingForExactEvent"], true);
    assert!(
        timeout["appliedKernelSessionId"].is_null(),
        "a restored wait does not invent a submitted response"
    );
}

#[test]
fn restarted_pending_input_converges_only_after_its_exact_core_event() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_input_crash(&root, false).await;
            return;
        }
        run_crash_child(
            &root,
            "restarted_pending_input_converges_only_after_its_exact_core_event",
            false,
        );
        let (mut worker, port, saved) = reopen_input(&root).await;
        let recovery = wait_open_at(&mut worker, &port, 0, at("00:15:02")).await;
        request_input_at(
            &mut worker,
            &recovery,
            "input-different-pending-recovery-call",
            2,
            at("00:15:02"),
        )
        .await;
        let newer =
            wait_different_input_at(&mut worker, &port, &saved.request, at("00:15:02")).await;
        assert_ne!(newer.input_request_id, saved.request.input_request_id);
        let original = input_operation_state(&root, &saved.request.input_request_id.0);
        let new = input_operation_state(&root, &newer.input_request_id.0);
        assert_eq!(original.0, "pending");
        assert_eq!(new.0, "pending");
        assert_eq!(original.1, new.1, "both requests use the resumed Core turn");
        assert_new_recovery_waiter_remains_pending(&mut worker, &port, &root, &newer).await;
        worker
            .accept_control(
                &input_response_at(&saved.request, at("00:15:03")),
                at("00:15:03"),
            )
            .await
            .expect_err("late old input cannot authorize the new waiter");
        let mut response = input_response_at(&newer, at("00:15:04"));
        if let wire::ExecutionPortMessage::InputResponseMessage(value) = &mut response {
            value.message_id = message_id(62_001);
        }
        worker
            .accept_control(&response, at("00:15:04"))
            .await
            .unwrap();
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:15:04")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        assert!(
            tool_output(&body, "input-different-pending-recovery-call")
                .unwrap()
                .to_string()
                .contains("continue"),
            "only the new exact response resumes the new waiter"
        );
        request_input_at(
            &mut worker,
            &continuation,
            "input-expiry-call",
            3,
            at("00:15:05"),
        )
        .await;
        let continuation = wait_open_at(&mut worker, &port, 2, at("00:15:06")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        assert!(
            tool_output(&body, "input-expiry-call")
                .unwrap()
                .to_string()
                .contains("[]")
        );
        assert_timeout_settled(&root, &saved.retained);
        assert_eq!(port.messages().iter().filter(|message| matches!(message,
            wire::ExecutionPortMessage::InputRequestMessage(request) if request.input_request_id == saved.request.input_request_id)).count(), 0,
            "expired original prompt must not re-enter the host waiting queue");
        worker
            .accept_control(
                &input_response_at(&saved.request, at("00:15:07")),
                at("00:15:07"),
            )
            .await
            .expect_err("late original input stays rejected after restart");
        Box::pin(worker.shutdown(at("00:15:07"))).await.unwrap();
    });
}

#[test]
fn resolved_input_timeout_crash_never_answers_a_different_recovery_waiter() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_input_crash(&root, true).await;
            return;
        }
        run_crash_child(
            &root,
            "resolved_input_timeout_crash_never_answers_a_different_recovery_waiter",
            true,
        );
        let saved: SavedInput =
            serde_json::from_slice(&fs::read(root.0.join("saved-input.json")).unwrap()).unwrap();
        assert_eq!(
            input_operation_state(&root, &saved.request.input_request_id.0).0,
            "resolved"
        );
        let ack: i64 = core_database(&root)
            .query_row(
                "SELECT acknowledgement_required FROM execution_outbox WHERE delivery_id=?",
                [&saved.retained.delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ack, 1, "actual exit73 must precede outbox cleanup");
        let (mut worker, port, saved) = reopen_input(&root).await;
        let recovery = wait_open_at(&mut worker, &port, 0, at("00:15:02")).await;
        request_input_at(
            &mut worker,
            &recovery,
            "input-new-recovery-call",
            2,
            at("00:15:02"),
        )
        .await;
        let newer =
            wait_different_input_at(&mut worker, &port, &saved.request, at("00:15:02")).await;
        assert_ne!(newer.input_request_id, saved.request.input_request_id);
        let original = input_operation_state(&root, &saved.request.input_request_id.0);
        let new = input_operation_state(&root, &newer.input_request_id.0);
        assert_eq!(original.0, "resolved");
        assert_eq!(new.0, "pending");
        assert_eq!(
            original.1, new.1,
            "different input shares the resumed Core turn"
        );
        assert_new_recovery_waiter_remains_pending(&mut worker, &port, &root, &newer).await;
        let ack: i64 = core_database(&root)
            .query_row(
                "SELECT acknowledgement_required FROM execution_outbox WHERE delivery_id=?",
                [&saved.retained.delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ack, 0, "resolved crash gap is cleaned once during recovery");
        worker
            .accept_control(
                &input_response_at(&saved.request, at("00:15:03")),
                at("00:15:03"),
            )
            .await
            .expect_err("old input response cannot authorize new waiter");
        let mut response = input_response_at(&newer, at("00:15:04"));
        if let wire::ExecutionPortMessage::InputResponseMessage(value) = &mut response {
            value.message_id = message_id(62_001);
        }
        worker
            .accept_control(&response, at("00:15:04"))
            .await
            .unwrap();
        let continued = wait_open_at(&mut worker, &port, 1, at("00:15:04")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continued.request.data_base64).unwrap())
                .unwrap();
        assert!(
            tool_output(&body, "input-new-recovery-call")
                .unwrap()
                .to_string()
                .contains("continue"),
            "only the exact new response resumes Core"
        );
        Box::pin(worker.shutdown(at("00:15:05"))).await.unwrap();
    });
}

#[test]
fn renewed_restart_rejects_a_foreign_lease_without_reusing_the_old_run() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_shell_crash(&root, false).await;
            return;
        }
        run_crash_child(
            &root,
            "renewed_restart_rejects_a_foreign_lease_without_reusing_the_old_run",
            false,
        );
        let saved: SavedShell =
            serde_json::from_slice(&fs::read(root.0.join("saved-shell.json")).unwrap()).unwrap();
        let mut dispatch = saved.dispatch;
        dispatch.lease.lease_id = domain::LeaseId(format!("lse_{:026X}", 63_001));
        dispatch.message_id = message_id(63_001);
        dispatch.request_id = domain::RequestId(format!("req_{:026X}", 63_001));
        dispatch.sent_at = at("00:15:02");
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register_at(&mut worker, &port, &config, at("00:15:02")).await;
        // Worker startup legitimately records original interaction deadlines
        // before any dispatch. Measure only changes caused by the foreign one.
        let before = stored_run(&root);
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("00:15:02"),
            )
            .await
            .unwrap();
        worker.flush_durable_outbox().await.unwrap();
        let result = port
            .messages()
            .into_iter()
            .find_map(|message| match message {
                wire::ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result),
                _ => None,
            })
            .expect("foreign recovery receives an explicit rejection");
        assert_ne!(serde_json::to_value(result).unwrap()["status"], "accepted");
        assert!(worker.active_jobs().is_empty());
        assert!(
            !port
                .messages()
                .iter()
                .any(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
        );
        assert!(
            stored_run(&root) == before,
            "rejected foreign recovery cannot edit the original run"
        );
        assert_eq!(
            retained_request(&root, "approvalId", &saved.approval.approval_id.0).frame,
            saved.request.frame
        );
        assert!(!saved.marker.exists());
        Box::pin(worker.shutdown(at("00:15:03"))).await.unwrap();
    });
}

#[test]
fn recovered_approval_rejects_changed_command_for_the_same_operation() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_APPROVAL_PREPARE_CHILD").is_some() {
            prepare_shell_crash(&root, false).await;
            return;
        }
        run_crash_child(
            &root,
            "recovered_approval_rejects_changed_command_for_the_same_operation",
            false,
        );
        let mut saved: SavedShell =
            serde_json::from_slice(&fs::read(root.0.join("saved-shell.json")).unwrap()).unwrap();
        let approval_facts = || -> (String, Vec<u8>) {
            core_database(&root)
                .query_row(
                    "SELECT request_digest,detail_json FROM approval_operation WHERE approval_id=?",
                    [&saved.approval.approval_id.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
        };
        let before = approval_facts();
        saved.dispatch.message_id = message_id(63_000);
        saved.dispatch.request_id = domain::RequestId(format!("req_{:026X}", 63_000));
        saved.dispatch.sent_at = at("00:15:02");
        let config = worker_config(&saved.dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register_at(&mut worker, &port, &config, at("00:15:02")).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(saved.dispatch.clone()),
                at("00:15:02"),
            )
            .await
            .unwrap();
        worker.flush_durable_outbox().await.unwrap();
        assert_dispatch_accepted(&port);
        let open = wait_open_at(&mut worker, &port, 0, at("00:15:02")).await;
        let changed_marker = root.0.join("changed-command-executed");
        request_escalated_shell_at(&mut worker, &open, &changed_marker, 2, at("00:15:02")).await;
        let mut rejected = None;
        for _ in 0..400 {
            match Box::pin(worker.poll_codex(at("00:15:03"))).await {
                Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                Err(error) => {
                    rejected = Some(error);
                    break;
                }
            }
        }
        assert_eq!(
            rejected
                .expect("changed approval payload must conflict")
                .code,
            winwincode_worker::WorkerErrorCode::UnexpectedMessage
        );
        assert_eq!(
            approval_facts(),
            before,
            "old approval authority stays immutable"
        );
        assert!(!changed_marker.exists());
        assert!(!saved.marker.exists());
        Box::pin(worker.shutdown(at("00:15:04"))).await.unwrap();
    });
}
