// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "test-support")]

#[path = "support/test_helper_path.rs"]
mod test_helper_path;

#[path = "support/canonical_code_mode.rs"]
mod canonical_code_mode;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use winwincode_codex::{ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexOptions};
use winwincode_domain as domain;
use winwincode_execution_port::generated as wire;

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
        static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if let Some(root) = std::env::var_os("WWC_RUNTIME_RESTART_DIRECTORY") {
            let root = PathBuf::from(root);
            fs::create_dir_all(&root).unwrap();
            return Self(root);
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "winwincode-runtime-unified-{}-{unique}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("fixture directory creation failed: {error}"),
            }
        }
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
    fn adapter(&self, config: &winwincode_worker::WorkerConfig) -> ProductionCodexAdapter {
        self.adapter_with_owner(config, false)
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
        let helper = test_helper_path::kernel_helper_path();
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
        ProductionCodexAdapter::open(if hosted {
            configuration.with_host_action_approvals()
        } else {
            configuration
        })
        .unwrap()
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
    dispatch.lease.expires_at = domain::Instant("2030-01-01T00:05:00.000Z".into());
    dispatch.job.limits.deadline_at = Some(dispatch.lease.expires_at.clone());
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
    Box::pin(worker.start(now())).await.unwrap();
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
    response.sent_at = now();
    response.server_time = now();
    response.lease_recovery = wire::WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases;
    worker
        .accept_control(
            &wire::ExecutionPortMessage::WorkerRegistrationResultMessage(response),
            now(),
        )
        .await
        .unwrap();
}
async fn freeze_and_dispatch(
    root: &Fixture,
    worker: &mut NativeWorker,
    port: &RecordedPort,
    dispatch: &mut wire::JobDispatchMessage,
) -> wire::ExecutionPortMessage {
    let mut freeze: wire::SnapshotFreezeRequestMessage =
        serde_json::from_value(template("snapshot.freeze_request")).unwrap();
    let tree = git(&root.repository(), &["rev-parse", "HEAD^{tree}"]);
    let mut content = Sha256::new();
    for (path, bytes) in source_files() {
        for field in [path.as_bytes(), b"100644", bytes] {
            content.update(u64::try_from(field.len()).unwrap().to_be_bytes());
            content.update(field);
        }
    }
    let input = dispatch.job.work_input.as_ref().unwrap();
    freeze.candidate.work_contract_id = input.work_contract.id.clone();
    freeze.candidate.contract_revision = input.work_contract.revision.clone();
    freeze.candidate.work_item_id = input.work_item.id.clone();
    freeze.candidate.base_commit = dispatch.job.workspace.checkout_revision.clone();
    freeze.candidate.candidate_commit = dispatch.job.workspace.checkout_revision.clone();
    freeze.candidate.candidate_tree = tree.clone();
    freeze.candidate.candidate_ref = input.candidate_ref.clone().unwrap();
    freeze.candidate.diff_digest = digest(&[]);
    freeze.base_tree_id.0 = tree;
    freeze.repository_id = dispatch.job.workspace.repository_id.clone();
    freeze.content_digest = domain::Sha256Digest(format!("sha256:{:x}", content.finalize()));
    freeze.dispatch = dispatch.clone();
    freeze.lease = dispatch.lease.clone();
    worker
        .accept_control(
            &wire::ExecutionPortMessage::SnapshotFreezeRequestMessage(freeze.clone()),
            now(),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    let response = port
        .messages()
        .into_iter()
        .find_map(|message| {
            if let wire::ExecutionPortMessage::SnapshotFreezeReceiptMessage(value) = message {
                Some(value)
            } else {
                None
            }
        })
        .expect("real frozen source receipt");
    winwincode_execution_port::snapshot_freeze::validate_freeze_receipt(&freeze, &response)
        .unwrap();
    let receipt = response.receipt;
    let mut snapshot = domain::Snapshot {
        schema_version: domain::SchemaVersion::WinwincodeV1,
        snapshot_id: domain::SnapshotId("snap_00000000000000000000000001".into()),
        candidate_id: receipt.candidate_id,
        work_run_id: receipt.work_run_id,
        repository_id: receipt.repository_id,
        base_commit_id: receipt.base_commit_id,
        base_tree_id: receipt.base_tree_id,
        candidate_commit_id: receipt.candidate_commit_id,
        candidate_tree_id: receipt.candidate_tree_id,
        diff_sha256: receipt.diff_sha256,
        content_digest: receipt.content_digest,
        created_at_millis: 1_893_456_000_000,
        immutable: true,
        validation_seal: digest(&[]),
    };
    snapshot.validation_seal = domain::seal_snapshot(&snapshot);
    dispatch.snapshot_id = Some(snapshot.snapshot_id.clone());
    let verification = wire::ExecutionPortMessage::SnapshotVerificationDispatchMessage(
        wire::SnapshotVerificationDispatchMessage {
            kind: wire::SnapshotVerificationDispatchMessageKind::SnapshotVerify,
            schema_version: domain::SchemaVersion::WinwincodeV1,
            message_id: dispatch.message_id.clone(),
            sent_at: dispatch.sent_at.clone(),
            dispatch: dispatch.clone(),
            snapshot,
        },
    );
    worker.accept_control(&verification, now()).await.unwrap();
    verification
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
async fn complete_model(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    index: usize,
    text: &str,
) {
    let frames = [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_item_done","item":{"type":"message","id":format!("fixture-message-{index}"),"role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]}}),
        serde_json::json!({"type":"completed","responseId":format!("fixture-response-{index}"),"tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":true}),
    ];
    deliver_frames(worker, open, index, &frames, false).await;
}
async fn deliver_frames(
    worker: &mut NativeWorker,
    open: &wire::ModelOpenMessage,
    index: usize,
    frames: &[serde_json::Value],
    replay: bool,
) {
    let frames = canonical_code_mode::canonical_frames(frames);
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
            sent_at: now(),
            sequence: domain::ExecutionSequence(i64::try_from(sequence + 1).unwrap()),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        };
        let message = wire::ExecutionPortMessage::ModelChunkMessage(message);
        worker.accept_control(&message, now()).await.unwrap();
        if replay {
            worker.accept_control(&message, now()).await.unwrap();
        }
    }
}

#[test]
fn reviewer_repair_coordinates_three_real_core_turns_without_restart() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let root = Fixture::new();
                    let mut dispatch = dispatch(&root, "reviewer");
                    let config = worker_config(&dispatch);
                    let port = RecordedPort::default();
                    let mut worker = winwincode_worker::WorkerMain::new(
                        config.clone(),
                        port.clone(),
                        root.adapter(&config),
                        root.workspace_runtime(),
                    );
                    register(&mut worker, &port, &config).await;
                    freeze_and_dispatch(&root, &mut worker, &port, &mut dispatch).await;
                    let mut exchanges = std::collections::BTreeSet::new();
                    for index in 0..4 {
                        let open = wait_for_open(&mut worker, &port, index).await;
                        assert!(
                            exchanges.insert(open.model_exchange_id.0.clone()),
                            "each repair is an exact distinct exchange"
                        );
                        complete_model(&mut worker, &open, index, "{}").await;
                    }
                    let mut outcome = None;
                    for _ in 0..400 {
                        Box::pin(worker.poll_codex(now())).await.unwrap();
                        outcome = port.messages().into_iter().find_map(|message| {
                            if let wire::ExecutionPortMessage::JobOutcomeMessage(value) = message {
                                Some(value)
                            } else {
                                None
                            }
                        });
                        if outcome.is_some() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    assert_eq!(
                        outcome
                            .expect("repair exhaustion must terminate")
                            .outcome
                            .status,
                        wire::ExecutionOutcomeStatus::Failed
                    );
                    assert_eq!(
                        exchanges.len(),
                        4,
                        "initial Core turn and all three repair turns must finish"
                    );
                    Box::pin(worker.shutdown(now())).await.unwrap();
                });
        })
        .unwrap()
        .join()
        .unwrap();
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

async fn wait_for_outcome(
    worker: &mut NativeWorker,
    port: &RecordedPort,
) -> wire::JobOutcomeMessage {
    for _ in 0..400 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
        if let Some(outcome) = port.messages().into_iter().find_map(|message| {
            if let wire::ExecutionPortMessage::JobOutcomeMessage(value) = message {
                Some(value)
            } else {
                None
            }
        }) {
            return outcome;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("public coordination did not retain its terminal outcome");
}

async fn acknowledge_outcome(worker: &mut NativeWorker, outcome: &wire::JobOutcomeMessage) {
    let mut ack: wire::JobOutcomeAckMessage =
        serde_json::from_value(template("job.outcome_ack")).unwrap();
    ack.lease = outcome.lease.clone();
    ack.worker_session_id = outcome.worker_session_id.clone();
    ack.session_identity = outcome.session_identity.clone();
    ack.sent_at = now();
    let message = wire::ExecutionPortMessage::JobOutcomeAckMessage(ack);
    worker.accept_control(&message, now()).await.unwrap();
    worker.accept_control(&message, now()).await.unwrap();
    assert!(
        worker.active_jobs().is_empty(),
        "terminal ACK releases the execution lease exactly once"
    );
}

#[test]
fn second_repair_cancellation_is_terminal_and_releases_after_duplicate_ack() {
    run_native(async {
        let root = Fixture::new();
        let mut dispatch = dispatch(&root, "reviewer");
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter(&config),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        freeze_and_dispatch(&root, &mut worker, &port, &mut dispatch).await;
        for index in 0..2 {
            let open = wait_for_open(&mut worker, &port, index).await;
            complete_model(&mut worker, &open, index, "{}").await;
        }
        let open = wait_for_open(&mut worker, &port, 2).await;
        let mut cancel: wire::JobCancelMessage =
            serde_json::from_value(template("job.cancel")).unwrap();
        cancel.lease = open.lease.clone();
        cancel.worker_session_id = open.worker_session_id.clone();
        cancel.session_identity = open.session_identity.clone();
        cancel.sent_at = now();
        cancel.requested_at = now();
        let message = wire::ExecutionPortMessage::JobCancelMessage(cancel);
        worker.accept_control(&message, now()).await.unwrap();
        worker.accept_control(&message, now()).await.unwrap();
        let outcome = wait_for_outcome(&mut worker, &port).await;
        assert_eq!(
            outcome.outcome.status,
            wire::ExecutionOutcomeStatus::Cancelled
        );
        acknowledge_outcome(&mut worker, &outcome).await;
        for _ in 0..5 {
            Box::pin(worker.poll_codex(now())).await.unwrap();
        }
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            3
        );
        Box::pin(worker.shutdown(now())).await.unwrap();
    });
}

#[test]
fn second_repair_provider_failure_stops_coordination_and_releases_terminal_lease() {
    run_native(async {
        let root = Fixture::new();
        let mut dispatch = dispatch(&root, "reviewer");
        let config = worker_config(&dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter(&config),
            root.workspace_runtime(),
        );
        register(&mut worker, &port, &config).await;
        freeze_and_dispatch(&root, &mut worker, &port, &mut dispatch).await;
        for index in 0..2 {
            let open = wait_for_open(&mut worker, &port, index).await;
            complete_model(&mut worker, &open, index, "{}").await;
        }
        let open = wait_for_open(&mut worker, &port, 2).await;
        let chunk = wire::ModelChunkMessage {
            error: Some(wire::ExecutionPortError {
                code: wire::ExecutionPortErrorCode::DeviceProviderResponseIncomplete,
                message: "fixture provider stopped before its final response".into(),
                retryable: false,
            }),
            is_final: true,
            kind: wire::ModelChunkMessageKind::ModelChunk,
            lease: open.lease.clone(),
            message_id: message_id(30_000),
            model_exchange_id: open.model_exchange_id.clone(),
            payload: None,
            schema_version: domain::SchemaVersion::WinwincodeV1,
            sent_at: now(),
            sequence: domain::ExecutionSequence(1),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        };
        worker
            .accept_control(&wire::ExecutionPortMessage::ModelChunkMessage(chunk), now())
            .await
            .unwrap();
        let outcome = wait_for_outcome(&mut worker, &port).await;
        assert_eq!(outcome.outcome.status, wire::ExecutionOutcomeStatus::Failed);
        acknowledge_outcome(&mut worker, &outcome).await;
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            3
        );
        Box::pin(worker.shutdown(now())).await.unwrap();
    });
}

async fn prepare_restart_input(root: &Fixture) {
    let mut dispatch = dispatch(root, "reviewer");
    let config = worker_config(&dispatch);
    let port = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        config.clone(),
        port.clone(),
        root.adapter(&config),
        root.workspace_runtime(),
    );
    register(&mut worker, &port, &config).await;
    let verification = freeze_and_dispatch(root, &mut worker, &port, &mut dispatch).await;
    for index in 0..2 {
        let open = wait_for_open(&mut worker, &port, index).await;
        complete_model(&mut worker, &open, index, "{}").await;
    }
    let pending = wait_for_open(&mut worker, &port, 2).await;
    fs::write(
        root.0.join("restart-input.json"),
        serde_json::to_vec(
            &serde_json::json!({"dispatch":dispatch,"verification":verification,"pending":pending}),
        )
        .unwrap(),
    )
    .unwrap();
    // Deliberately bypass Drop: emulate loss of the Worker process, not graceful Core cancellation.
    std::process::exit(73);
}

fn run_restart_preparation(root: &Fixture) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart_replays_exact_repair_exchange_and_rejects_changed_approval_owner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("WWC_RUNTIME_RESTART_DIRECTORY", &root.0)
        .env("WWC_RUNTIME_RESTART_PREPARE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("restart preparation exceeded its bounded local fixture deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(73),
        "preparation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn restart_replays_exact_repair_exchange_and_rejects_changed_approval_owner() {
    run_native(async {
        let root = Fixture::new();
        if std::env::var_os("WWC_RUNTIME_RESTART_PREPARE").is_some() {
            prepare_restart_input(&root).await;
        }
        run_restart_preparation(&root);
        let retained: serde_json::Value =
            serde_json::from_slice(&fs::read(root.0.join("restart-input.json")).unwrap()).unwrap();
        let dispatch: wire::JobDispatchMessage =
            serde_json::from_value(retained["dispatch"].clone()).unwrap();
        let verified_dispatch: wire::ExecutionPortMessage =
            serde_json::from_value(retained["verification"].clone()).unwrap();
        let pending: wire::ModelOpenMessage =
            serde_json::from_value(retained["pending"].clone()).unwrap();
        let original_request: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&pending.request.data_base64).unwrap())
                .unwrap();
        let original_format = original_request["request"]["text"]["format"].clone();
        assert!(
            original_format["schema"].is_object(),
            "the real repair request must carry its structured output schema"
        );
        let config = worker_config(&dispatch);
        let replay_port = RecordedPort::default();
        let mut replay = winwincode_worker::WorkerMain::new(
            config.clone(),
            replay_port.clone(),
            root.adapter(&config),
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port, &config).await;
        replay
            .accept_control(&verified_dispatch, now())
            .await
            .unwrap();
        let reopened = wait_for_open(&mut replay, &replay_port, 0).await;
        assert_eq!(
            reopened.model_exchange_id, pending.model_exchange_id,
            "restart must reattach the same durable operation"
        );
        assert_eq!(
            reopened.request, pending.request,
            "restart must not create a replacement paid request"
        );
        let reopened_request: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&reopened.request.data_base64).unwrap())
                .unwrap();
        assert_eq!(
            reopened_request["request"]["text"]["format"], original_format,
            "recovery must preserve the exact typed turn output schema"
        );
        let store = rusqlite::Connection::open_with_flags(
            root.0.join("worker/worker-codex.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let exchanges: i64 = store
            .query_row("SELECT COUNT(*) FROM model_call_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            exchanges, 3,
            "recovery must not admit a replacement exchange"
        );
        drop(store);
        drop(replay);
        let changed_port = RecordedPort::default();
        let mut changed = winwincode_worker::WorkerMain::new(
            config.clone(),
            changed_port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register(&mut changed, &changed_port, &config).await;
        changed
            .accept_control(&verified_dispatch, now())
            .await
            .unwrap();
        changed.flush_durable_outbox().await.unwrap();
        assert!(changed_port.messages().iter().any(
            |message| matches!(message, wire::ExecutionPortMessage::JobDispatchResultMessage(result)
            if result.status == wire::JobDispatchResultMessageStatus::RejectedCapability)
        ));
        assert!(changed.active_jobs().is_empty());
        assert!(
            !changed_port
                .messages()
                .iter()
                .any(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
        );
        Box::pin(changed.shutdown(now())).await.unwrap();
    });
}

async fn reject_locked_batch_delivery(
    worker: &mut NativeWorker,
    journal_path: &Path,
) -> rusqlite::Connection {
    let blocked = rusqlite::Connection::open(journal_path).unwrap();
    blocked.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut rejected = false;
    for _ in 0..200 {
        if let Err(error) = Box::pin(worker.poll_codex(now())).await {
            assert_eq!(
                error.code,
                winwincode_worker::WorkerErrorCode::DelegatedPollMismatch
            );
            rejected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        rejected,
        "real ChangeBatch journal lock must reject the first delivery"
    );
    blocked
}

async fn assert_exact_batch_retried_once(worker: &mut NativeWorker, journal_path: &Path) {
    let blocked = reject_locked_batch_delivery(worker, journal_path).await;
    blocked.execute_batch("ROLLBACK").unwrap();
    drop(blocked);
    let mut retained = false;
    for _ in 0..200 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
        let database = rusqlite::Connection::open(journal_path).unwrap();
        let count = database
            .query_row(
                "SELECT COUNT(*) FROM change_batch_intent WHERE receipt_json IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        if count == 1 {
            retained = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        retained,
        "clearing the transient fault must retry the same durable batch"
    );
    for _ in 0..5 {
        Box::pin(worker.poll_codex(now())).await.unwrap();
    }
    let database = rusqlite::Connection::open(journal_path).unwrap();
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM change_batch_intent", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

async fn start_delegated_proposal(root: &Fixture) -> (NativeWorker, RecordedPort) {
    let mut dispatch = dispatch(root, "executor");
    dispatch.job.work_input.as_mut().unwrap().candidate_ref = None;
    dispatch.job.workspace.write_mode = wire::ExecutionWorkspaceWriteMode::ReadOnly;
    let config = worker_config(&dispatch);
    let port = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        config.clone(),
        port.clone(),
        root.adapter_with_mode(
            &config,
            false,
            winwincode_codex::ExecutionMode::DelegatedPatch,
        ),
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
    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn fixture() {}\n+pub fn fixture() { let _ = 1; }\n*** End Patch\n";
    let proposal = serde_json::json!({
        "acceptanceCriteriaIds": dispatch.job.work_input.as_ref().unwrap().work_item.criterion_ids,
        "disposition": "continue", "schemaVersion": 1, "validationProfile": "changed", "patch": patch,
    });
    let frames = [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_item_done","item":{"type":"custom_tool_call","name":"submit_change_batch","call_id":"fixture-exact-batch","input":proposal.to_string()}}),
        serde_json::json!({"type":"completed","responseId":"fixture-batch-response","tokenUsage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"endTurn":false}),
    ];
    deliver_frames(&mut worker, &open, 0, &frames, false).await;
    (worker, port)
}

async fn cancel_active_job(worker: &mut NativeWorker) {
    let active = worker.active_jobs()[0].clone();
    let mut cancel: wire::JobCancelMessage =
        serde_json::from_value(template("job.cancel")).unwrap();
    cancel.lease = active.lease;
    cancel.worker_session_id = active.worker_session_id;
    cancel.session_identity = active.session_identity;
    cancel.sent_at = now();
    cancel.requested_at = now();
    worker
        .accept_control(&wire::ExecutionPortMessage::JobCancelMessage(cancel), now())
        .await
        .unwrap();
}

fn batch_receipt(root: &Fixture) -> Vec<u8> {
    let database = rusqlite::Connection::open(
        root.0
            .join(".workspaces-change-batches/change-batch.sqlite3"),
    )
    .unwrap();
    database
        .query_row("SELECT receipt_json FROM change_batch_intent", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn assert_quarantined_batch(
    root: &Fixture,
    active: &winwincode_worker::ActiveJob,
    original: &[u8],
) {
    let database = rusqlite::Connection::open(
        root.0
            .join(".workspaces-change-batches/change-batch.sqlite3"),
    )
    .unwrap();
    let (workspace, batch, state, accepted): (String, String, String, String) = database.query_row(
        "SELECT workspace_id, active_batch_id, state, accepted_revision FROM change_batch_workspace", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    let receipt: wire::ChangeBatchReceipt = serde_json::from_slice(original).unwrap();
    assert_eq!(
        batch_receipt(root),
        original,
        "quarantine must preserve the actual applied receipt"
    );
    assert_eq!(state, "quarantined");
    assert_eq!(batch, receipt.identity.batch_id.0);
    assert_eq!(accepted, receipt.base_revision.0);
    let progress: Vec<u8> = database
        .query_row(
            "SELECT event_json FROM change_batch_progress ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let progress: wire::ChangeBatchProgressEvent = serde_json::from_slice(&progress).unwrap();
    assert_eq!(
        progress.state,
        wire::ChangeBatchProgressState::InfrastructureFailed
    );
    assert_eq!(progress.identity, receipt.identity);
    let checkout = root.0.join("workspaces").join(workspace).join("checkout");
    assert_eq!(
        fs::read_to_string(checkout.join("src/lib.rs")).unwrap(),
        "pub fn fixture() { let _ = 1; }\n"
    );
    assert!(
        checkout
            .parent()
            .unwrap()
            .join(".winwincode-workspace.json")
            .is_file()
    );
    let mut recovered = root.workspace_runtime();
    let error = recovered
        .open_for_job_recovering(active, None, &now())
        .unwrap_err();
    assert_eq!(
        error.code(),
        winwincode_worker::workspace_runtime::JobWorkspaceErrorCode::ChangeBatch
    );
}

#[test]
fn change_batch_storage_failure_releases_exact_delivery_for_idempotent_retry() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port) = start_delegated_proposal(&root).await;
        let journal_path = root
            .0
            .join(".workspaces-change-batches/change-batch.sqlite3");
        assert_exact_batch_retried_once(&mut worker, &journal_path).await;
        let saved_active = worker.active_jobs()[0].clone();
        let receipt = batch_receipt(&root);
        cancel_active_job(&mut worker).await;
        let outcome = wait_for_outcome(&mut worker, &port).await;
        assert_eq!(
            outcome.outcome.status,
            wire::ExecutionOutcomeStatus::Cancelled
        );
        acknowledge_outcome(&mut worker, &outcome).await;
        for _ in 0..5 {
            Box::pin(worker.poll_codex(now())).await.unwrap();
        }
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            1
        );
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::JobOutcomeMessage(_)
                ))
                .count(),
            1
        );
        assert_quarantined_batch(&root, &saved_active, &receipt);
        Box::pin(worker.shutdown(now())).await.unwrap();
    });
}

#[test]
fn cancellation_preempts_unconsumed_batch_and_never_replays_its_patch() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port) = start_delegated_proposal(&root).await;
        let journal_path = root
            .0
            .join(".workspaces-change-batches/change-batch.sqlite3");
        let blocked = reject_locked_batch_delivery(&mut worker, &journal_path).await;
        cancel_active_job(&mut worker).await;
        worker.flush_durable_outbox().await.unwrap();
        assert!(port.messages().iter().any(|message| matches!(
            message, wire::ExecutionPortMessage::JobCancelAckMessage(ack)
            if ack.status == wire::JobCancelAckMessageStatus::Accepted
        )));
        blocked.execute_batch("ROLLBACK").unwrap();
        drop(blocked);
        let outcome = wait_for_outcome(&mut worker, &port).await;
        assert_eq!(
            outcome.outcome.status,
            wire::ExecutionOutcomeStatus::Cancelled
        );
        acknowledge_outcome(&mut worker, &outcome).await;
        for _ in 0..5 {
            Box::pin(worker.poll_codex(now())).await.unwrap();
        }
        let database = rusqlite::Connection::open(&journal_path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM change_batch_intent", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0,
            "cancelled pending batch must never apply after storage recovers"
        );
        let codex = rusqlite::Connection::open(root.0.join("worker/worker-codex.sqlite3")).unwrap();
        let record: Vec<u8> = codex
            .query_row("SELECT record_json FROM codex_run", [], |row| row.get(0))
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&record).unwrap();
        assert!(
            record["batchIntent"].is_object(),
            "cancellation must preserve durable batch history"
        );
        assert_eq!(record["terminal"]["kind"], "cancelled");
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            1
        );
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::JobOutcomeMessage(_)
                ))
                .count(),
            1
        );
        Box::pin(worker.shutdown(now())).await.unwrap();
    });
}
