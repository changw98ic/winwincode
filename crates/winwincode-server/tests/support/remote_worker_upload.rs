// SPDX-License-Identifier: Apache-2.0

//! Real Worker/SQLite/HTTPS/exchange regression. The ingress uses the production
//! `ArtifactStore`; only the HTTP listener and business routing are fixture code.
#![allow(clippy::too_many_lines)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use winwincode_codex::candidate_artifact_outbox::CandidateArtifactUpload;
use winwincode_codex::{
    CodexCoreAdapter as _, ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexOptions,
};
use winwincode_domain::{
    ExecutionAckSequence, ExecutionMessageId, Instant, SchemaVersion, Sha256Digest,
};
use winwincode_execution_port::{
    generated::*,
    transport::{
        ExecutionPortCore, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
    },
};
use winwincode_server::{
    FileRemoteWorkerAuthenticator, ProductionRemoteWorkerExchange, RemoteWorkerExchangePort,
    RepositoryRuntimeScheduler,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, LocalArtifactObjectStore, ReceiptScopeKey,
};
use winwincode_worker::{
    WorkerConfig, WorkerErrorCode, WorkerMain, remote_transport::RemoteWorkerPort,
    workspace_runtime::JobWorkspaceRuntime,
};

#[derive(Default)]
struct Metrics {
    outstanding: HashSet<ExecutionMessageId>,
    accepted: HashSet<ExecutionMessageId>,
    refused: Vec<ExecutionMessageId>,
    backlog: Vec<usize>,
    heartbeats: usize,
}

struct ArtifactIngress {
    store: Arc<Mutex<ArtifactStore>>,
    metrics: Arc<Mutex<Metrics>>,
    receipts: HashMap<ExecutionMessageId, Vec<ExecutionPortMessage>>,
}

fn scope() -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(b"remote-upload-regression".to_vec()).unwrap()
}

fn provenance(
    lease: &ExecutionLeaseStamp,
    session: &winwincode_domain::WorkerSessionId,
) -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        lease.job_id.clone(),
        u64::try_from(lease.attempt).unwrap(),
        lease.lease_id.clone(),
        lease.fencing_token.clone(),
        lease.worker_id.clone(),
        lease.worker_instance_id.clone(),
        session.clone(),
    )
    .unwrap()
}

impl ExecutionPortCore for ArtifactIngress {
    type Output = Vec<ExecutionPortMessage>;
    type Error = String;
    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        let message_id =
            winwincode_execution_port::transport::execution_message_id(message).unwrap();
        if let Some(receipt) = self.receipts.get(&message_id) {
            return Ok(receipt.clone());
        }
        let response = match message {
            ExecutionPortMessage::ArtifactOpenMessage(open) => {
                let attribution = ArtifactMeteringAttribution {
                    organization_id: winwincode_domain::OrganizationId(super::id("org", 1)),
                    workspace_id: winwincode_domain::WorkspaceId(super::id("wsp", 1)),
                    project_id: winwincode_domain::ProjectId(super::id("prj", 1)),
                    repository_id: winwincode_domain::RepositoryId(super::id("rep", 1)),
                    delivery_id: None,
                    product_session_id: Some(open.session_identity.product_session_id.clone()),
                    user_id: winwincode_domain::UserId(super::id("usr", 1)),
                };
                self.store
                    .lock()
                    .unwrap()
                    .open_artifact(ArtifactOpen::new(
                        scope(),
                        open.message_id.clone(),
                        open.request_id.clone(),
                        open.artifact.artifact_id.clone(),
                        "candidate",
                        open.artifact.media_type.clone(),
                        open.artifact.digest.clone(),
                        u64::try_from(open.artifact.size_bytes).unwrap(),
                        open.artifact.file_name.clone(),
                        provenance(&open.lease, &open.worker_session_id),
                        attribution,
                        ArtifactRetention::Indefinite,
                        1000,
                    ))
                    .map_err(|e| e.to_string())?;
                ExecutionPortMessage::ArtifactAckMessage(ArtifactAckMessage {
                    retained_artifact: None,
                    ack_sequence: ExecutionAckSequence(0),
                    artifact_id: open.artifact.artifact_id.clone(),
                    error: None,
                    kind: ArtifactAckMessageKind::ArtifactAck,
                    lease: open.lease.clone(),
                    message_id: ExecutionMessageId(super::id("xmsg", 10000)),
                    replay_from_sequence: None,
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: open.sent_at.clone(),
                    session_identity: open.session_identity.clone(),
                    status: LeaseWriteStatus::Accepted,
                    worker_session_id: open.worker_session_id.clone(),
                })
            }
            ExecutionPortMessage::ArtifactChunkMessage(chunk) => {
                self.store
                    .lock()
                    .unwrap()
                    .append_chunk(&ArtifactChunk::new(
                        scope(),
                        chunk.message_id.clone(),
                        chunk.artifact_id.clone(),
                        provenance(&chunk.lease, &chunk.worker_session_id),
                        1000,
                        u64::try_from(chunk.sequence.0).unwrap(),
                        chunk.payload.content_type.clone(),
                        chunk.payload.payload_digest.clone(),
                        STANDARD
                            .decode(&chunk.payload.data_base64)
                            .map_err(|e| e.to_string())?,
                        chunk.is_final,
                    ))
                    .map_err(|e| e.to_string())?;
                ExecutionPortMessage::ArtifactAckMessage(ArtifactAckMessage {
                    retained_artifact: None,
                    ack_sequence: ExecutionAckSequence(chunk.sequence.0),
                    artifact_id: chunk.artifact_id.clone(),
                    error: None,
                    kind: ArtifactAckMessageKind::ArtifactAck,
                    lease: chunk.lease.clone(),
                    message_id: ExecutionMessageId(super::id(
                        "xmsg",
                        10000 + u64::try_from(chunk.sequence.0).unwrap(),
                    )),
                    replay_from_sequence: None,
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: chunk.sent_at.clone(),
                    session_identity: chunk.session_identity.clone(),
                    status: LeaseWriteStatus::Accepted,
                    worker_session_id: chunk.worker_session_id.clone(),
                })
            }
            ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) => {
                self.metrics.lock().unwrap().heartbeats += 1;
                serde_json::from_value(serde_json::json!({
                    "kind":"worker.heartbeat_ack", "schemaVersion":SchemaVersion::WinwincodeV1,
                    "messageId":super::id("xmsg",20000 + u64::try_from(heartbeat.heartbeat_sequence.0).unwrap()),
                    "sentAt":heartbeat.sent_at, "serverTime":heartbeat.observed_at, "workerId":heartbeat.worker_id,
                    "workerInstanceId":heartbeat.worker_instance_id, "heartbeatSequence":heartbeat.heartbeat_sequence,
                    "status":"accepted", "nextHeartbeatWithinMs":1000
                })).unwrap()
            }
            _ => return Err("unexpected upstream frame".into()),
        };
        let mut metrics = self.metrics.lock().unwrap();
        metrics.accepted.insert(message_id.clone());
        metrics
            .outstanding
            .insert(winwincode_execution_port::transport::execution_message_id(&response).unwrap());
        self.receipts.insert(message_id, vec![response.clone()]);
        Ok(vec![response])
    }
}

fn adapter(root: &Path, config: &WorkerConfig) -> ProductionCodexAdapter {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace.join("target"), std::path::PathBuf::from);
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    let helper = target.join("debug/winwincode-kernel-helper");
    ProductionCodexAdapter::open(ProductionCodexConfig::try_new(ProductionCodexOptions {
        data_directory: root.join("worker"), helper_release_manifest: winwincode_codex::HelperReleaseManifest::from_test_helper(&helper).unwrap(),
        helper_executable: helper, provider: "deterministic-fixture".into(), model: "unused-model".into(),
        gateway_route: ModelGatewayRoute { capability: "reasoning".into(), route: "unused-fixture".into() },
        registered_capabilities: config.capabilities.clone(), discovered_capabilities: vec![],
        action_signing_key: winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey::from_bytes([31;32]).unwrap(),
        execution_envelope: winwincode_execution_port::action_gateway::ExecutionEnvelopeToken { version:1, digest: Sha256Digest(format!("sha256:{}","a".repeat(64))) },
        execution_mode: ExecutionMode::React, observer_mode: winwincode_codex::ObserverMode::Off,
    }).unwrap()).unwrap()
}

async fn scenario(chunks: usize) {
    let root = super::temporary_root(&format!("remote-upload-{chunks}"));
    let application = super::open_application(&root, "fixture").unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let messages = fixture["messages"].as_array().unwrap();
    let mut open: ArtifactOpenMessage = serde_json::from_value(
        messages
            .iter()
            .find(|m| m["kind"] == "artifact.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let now = Instant("2027-01-15T08:00:02.000Z".into());
    open.lease.issued_at = Instant("2027-01-15T08:00:00.000Z".into());
    open.lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    let config = WorkerConfig {
        worker_id: open.lease.worker_id.clone(),
        worker_instance_id: open.lease.worker_instance_id.clone(),
        started_at: now.clone(),
        capabilities: WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            features: vec![WorkerCapabilityFeature::Shell],
            max_concurrent_jobs: 1,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    };
    // A recovered Chat candidate uses the same production chunk/ACK ledger.
    // The bytes are deterministic and no Kernel turn or Provider is started.
    let bytes = vec![0x7f; (chunks - 1) * 64 * 1024 + 1];
    let upload = CandidateArtifactUpload {
        snapshot_id: None,
        job_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        logical_job_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        execution_profile: "codex-chat".into(),
        scope: serde_json::from_value(
            messages
                .iter()
                .find(|m| m["kind"] == "job.dispatch")
                .unwrap()["job"]["scope"]
                .clone(),
        )
        .unwrap(),
        replacement_authority: None,
        lease: open.lease.clone(),
        worker_session_id: open.worker_session_id.clone(),
        session_identity: open.session_identity.clone(),
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
        bytes: bytes.clone(),
        created_at: now.clone(),
    };
    let mut codex = adapter(&root, &config);
    let retained = codex.retain_candidate_artifact(&upload).unwrap();
    assert_eq!(retained.deliveries.len(), chunks + 1);
    let token = root.join("worker.token");
    fs::write(&token, b"fixture-proof").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let auth = FileRemoteWorkerAuthenticator::open(
        &token,
        config.worker_id.clone(),
        winwincode_storage::WorkerPoolId(super::id("wpl", 1)),
        super::worker_scope(1),
        "fixture-issuer".into(),
        "fixture-worker".into(),
        "local-build".into(),
        Instant("2027-01-15T09:00:00.000Z".into()),
        &now,
    )
    .unwrap();
    let scheduler = RepositoryRuntimeScheduler::from_application(
        &application,
        serde_json::from_value(super::repository_scope_json(1)).unwrap(),
        config.worker_id.clone(),
        config.worker_instance_id.clone(),
        "upload-regression",
        Duration::from_secs(10),
    )
    .unwrap();
    let store = Arc::new(Mutex::new(
        ArtifactStore::open(
            root.join("artifacts"),
            Box::new(LocalArtifactObjectStore::open(root.join("objects")).unwrap()),
        )
        .unwrap(),
    ));
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let exchange = Arc::new(ProductionRemoteWorkerExchange::new(
        &root,
        Arc::new(auth),
        scheduler,
        ArtifactIngress {
            store: store.clone(),
            metrics: metrics.clone(),
            receipts: HashMap::new(),
        },
    ));
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let listener_task = {
        let exchange = exchange.clone();
        let metrics = metrics.clone();
        let now = now.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let exchange = exchange.clone();
                let metrics = metrics.clone();
                let now = now.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut raw = vec![];
                    let mut scratch = [0u8; 8192];
                    let (header_end, body_len, credential) = loop {
                        let n = tls.read(&mut scratch).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&scratch[..n]);
                        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = std::str::from_utf8(&raw[..i]).unwrap();
                            let value = |name: &str| {
                                header
                                    .lines()
                                    .filter_map(|line| line.split_once(':'))
                                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                                    .map(|(_, value)| value.trim())
                            };
                            break (
                                i + 4,
                                value("content-length").unwrap().parse::<usize>().unwrap(),
                                value("authorization")
                                    .unwrap()
                                    .strip_prefix("Bearer ")
                                    .unwrap()
                                    .as_bytes()
                                    .to_vec(),
                            );
                        }
                        assert!(raw.len() < 16384);
                    };
                    while raw.len() < header_end + body_len {
                        let n = tls.read(&mut scratch).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&scratch[..n]);
                    }
                    let body = &raw[header_end..header_end + body_len];
                    let request = RemoteExchangeRequest::decode(body).unwrap();
                    let frame =
                        RemoteTransportAdapter::<ArtifactIngress>::decode(request.frame()).unwrap();
                    let response = exchange.exchange(credential, body, now).unwrap();
                    let decoded = RemoteExchangeResponse::decode(&response).unwrap();
                    {
                        let mut metrics = metrics.lock().unwrap();
                        for ack in request.acknowledgements() {
                            metrics.outstanding.remove(ack);
                        }
                        if !decoded.frame_accepted() {
                            metrics.refused.push(
                                winwincode_execution_port::transport::execution_message_id(
                                    frame.message(),
                                )
                                .unwrap(),
                            );
                        }
                        let backlog = metrics.outstanding.len();
                        metrics.backlog.push(backlog);
                    }
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    );
                    if tls.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = tls.write_all(&response).await;
                });
            }
        })
    };
    let (port, handle) = RemoteWorkerPort::open(
        &format!("https://{address}"),
        cert.der(),
        &token,
        config.worker_id.clone(),
        config.worker_instance_id.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    fs::create_dir_all(root.join("sources")).unwrap();
    let mut worker = WorkerMain::new(
        config.clone(),
        port,
        codex,
        JobWorkspaceRuntime::open(root.join("workspaces"), root.join("sources")).unwrap(),
    );
    Box::pin(worker.start(now.clone())).await.unwrap();
    let (id, registration) = handle.next_control().unwrap().unwrap();
    Box::pin(worker.accept_control(&registration, now.clone()))
        .await
        .unwrap();
    handle.confirm(id).unwrap();
    // Intentionally delay ACK intake until the actual server admission threshold.
    for _ in 0..512 {
        let requests_before = metrics.lock().unwrap().backlog.len();
        match Box::pin(worker.flush_durable_outbox()).await {
            Err(error) if error.code == WorkerErrorCode::ExecutionBackpressure => break,
            Err(error) => panic!("unexpected fill error: {error:?}"),
            Ok(()) => {}
        }
        assert!(
            metrics.lock().unwrap().backlog.len() - requests_before <= 64,
            "each drive turn sends a bounded batch"
        );
        if metrics.lock().unwrap().outstanding.len() == 129 {
            break;
        }
    }
    assert_eq!(metrics.lock().unwrap().outstanding.len(), 129);
    let refusal = Box::pin(worker.heartbeat(now.clone())).await.unwrap_err();
    assert_eq!(refusal.code, WorkerErrorCode::ExecutionBackpressure);
    let refused = metrics.lock().unwrap().refused.last().unwrap().clone();
    let db = rusqlite::Connection::open(root.join("worker/worker-codex.sqlite3")).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM execution_outbox WHERE delivery_id=?1",
            [&refused.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "pending",
        "frameAccepted=false never marks upstream successful"
    );
    let mut final_confirmed = false;
    let mut artifact_confirmed = 0;
    for _ in 0..1024 {
        while let Some((id, message)) = handle.next_control().unwrap() {
            let before = metrics.lock().unwrap().backlog.len();
            Box::pin(worker.accept_control(&message, now.clone()))
                .await
                .expect("durable ACK intake is independent of transport");
            assert_eq!(
                metrics.lock().unwrap().backlog.len(),
                before,
                "ACK intake performs no network send"
            );
            handle.confirm(id).unwrap();
            if matches!(&message, ExecutionPortMessage::ArtifactAckMessage(_)) {
                artifact_confirmed += 1;
            }
            if matches!(&message, ExecutionPortMessage::ArtifactAckMessage(ack) if ack.ack_sequence.0 == i64::try_from(chunks).unwrap())
            {
                final_confirmed = true;
            }
        }
        Box::pin(worker.flush_durable_outbox()).await.unwrap();
        Box::pin(worker.heartbeat(now.clone())).await.unwrap();
        if final_confirmed {
            break;
        }
    }
    // The final drive may have emitted both a retained heartbeat and a new
    // one. Consume their receipts, then send confirmations before inspecting
    // the remaining queue (the fresh heartbeat leaves exactly one receipt).
    while let Some((id, message)) = handle.next_control().unwrap() {
        Box::pin(worker.accept_control(&message, now.clone()))
            .await
            .unwrap();
        handle.confirm(id).unwrap();
    }
    Box::pin(worker.heartbeat(now.clone())).await.unwrap();
    assert_eq!(artifact_confirmed, chunks + 1);
    let (_, mut codex) = worker.into_parts();
    assert_eq!(
        codex
            .accepted_candidate_artifact(&upload.authority())
            .unwrap(),
        Some(retained.artifact.clone())
    );
    assert!(
        codex
            .pending_execution_deliveries()
            .unwrap()
            .iter()
            .all(|row| !matches!(
                row.message,
                ExecutionPortMessage::ArtifactOpenMessage(_)
                    | ExecutionPortMessage::ArtifactChunkMessage(_)
            ))
    );
    let object = store
        .lock()
        .unwrap()
        .read_exact(&ArtifactAccess::new(
            scope(),
            retained.artifact.artifact_id.clone(),
            retained.artifact.digest.clone(),
            provenance(&upload.lease, &upload.worker_session_id),
        ))
        .unwrap();
    assert_eq!(
        object.bytes(),
        bytes,
        "actual stored bytes and final digest are unchanged"
    );
    let metrics = metrics.lock().unwrap();
    assert!(metrics.heartbeats > 0);
    assert!(
        metrics.outstanding.len() <= 1,
        "only the latest heartbeat receipt may remain"
    );
    assert!(
        metrics.backlog.windows(2).any(|pair| pair[1] < pair[0]),
        "confirmed controls reduce actual backlog"
    );
    assert!(
        metrics.refused.len() <= 2,
        "no scan amplification after backpressure"
    );
    eprintln!(
        "chunks={chunks} bytes={} confirmed={artifact_confirmed} backpressure_rejections={} heartbeats={} final_pending={}",
        upload.bytes.len(),
        metrics.refused.len(),
        metrics.heartbeats,
        metrics.outstanding.len()
    );
    drop(metrics);
    drop(db);
    drop(codex);
    let mut reopened = adapter(&root, &config);
    assert_eq!(
        reopened
            .accepted_candidate_artifact(&upload.authority())
            .unwrap(),
        Some(retained.artifact)
    );
    drop(reopened);
    listener_task.abort();
    drop(exchange);
    drop(application);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn recovered_uploads_drain_through_real_worker_sqlite_https_and_exchange() {
    for chunks in [128, 129, 256] {
        Box::pin(scenario(chunks)).await;
    }
}
