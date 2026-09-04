// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceService, DurableProviderGatewayIdentitySource, HttpsSseProviderConfig,
    HttpsSseProviderLimits, HttpsSseProviderTimeouts, LocalModelPolicyAuthority,
    LocalModelPolicyAuthorityConfig, LocalSecretStoreAdapter, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionLimits, ModelAdmissionPolicyLayer, ModelCapability,
    ModelExecutionOpenReceipt, ModelExecutionPortReceipt, ModelRequestPoolConfig,
    ModelRoutePolicyDecision, ModelSettingsRequest, ModelSettingsService, ModelSettingsTarget,
    ModelSettingsValues, ModelToolSupport, ProviderAdmissionReservationConfig,
    ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor,
    ProviderGatewayIdentityPort, ProviderGatewayOpenReceipt, ProviderTokenPricing, ResolvedSecret,
    StandaloneModelExecutionApplication, StandaloneModelExecutionConfig, StandaloneProviderConfig,
    StructuredOutputSupport, local_loopback_retry_policy,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, DeliveryId, EnterprisePolicyId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Revision, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::{
    generated::{
        DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, EncodedPayload, ExecutionJob,
        ExecutionLeaseStamp, ExecutionLimits, ExecutionPortMessage, ExecutionScope,
        ExecutionWorkspace, ExecutionWorkspaceWriteMode, LeaseWriteStatus, ModelAckMessage,
        ModelAckMessageKind, ModelGatewayRoute, ModelOpenMessage, ModelOpenMessageKind,
    },
    transport::{ExecutionPortCore, FrameDirection, RemoteTransportAdapter, TypedFrame},
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, EnterprisePolicyActor, EnterprisePolicyChildOverrideMode,
    EnterprisePolicyDefinition, EnterprisePolicyEffect, EnterprisePolicyInheritanceMode,
    EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyScope, EnterprisePolicyState,
    EnterprisePolicyVersionSource, EnterprisePolicyWrite, ExecutionAdmissionBoundary,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    NewOutboxEvent, ProductStateStorage, StateCommit, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};
use winwincode_storage::{LeaseWriteStatus as StorageLeaseWriteStatus, SqliteStorage};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const PROVIDER_ID: &str = "winwincode-loopback";
const MODEL_ID: &str = "loopback-model";
const SECRET_FIXTURE: &[u8] = b"provider-production-secret-fixture";
const INPUT_FIXTURE: &[u8] = br#"{"prompt":"provider-production-private-input"}"#;
const ANTHROPIC_INPUT_FIXTURE: &[u8] = br#"{"requestId":"codex-request-live-1","provider":"winwincode","sessionId":"session-live-1","threadId":"thread-live-1","turnId":"turn-live-1","request":{"model":"local-display-model[1m]","instructions":"Reply with the single word OK.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"provider-production-private-input"}]}],"tools":[{"type":"function","name":"read_file","description":"Read one file","strict":false,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},{"type":"custom","name":"apply_patch","description":"Apply one patch","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}],"tool_choice":"auto","parallel_tool_calls":true,"reasoning":{"effort":"high","summary":"auto"},"store":false,"stream":true,"stream_options":null,"include":[],"service_tier":null,"prompt_cache_key":null,"text":null,"client_metadata":null}}"#;

struct HttpsFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    request: mpsc::Receiver<Vec<u8>>,
    server: thread::JoinHandle<()>,
}

impl HttpsFixture {
    fn start(body: String) -> Self {
        Self::start_with_declared_length(body, None)
    }

    fn start_truncated(body: String) -> Self {
        let declared_length = body.len().saturating_add(64);
        Self::start_with_declared_length(body, Some(declared_length))
    }

    fn start_with_declared_length(body: String, declared_length: Option<usize>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Provider TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("Provider TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Provider TLS fixture");
        let address = listener.local_addr().expect("Provider TLS address");
        let (request_tx, request) = mpsc::channel();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().expect("accept Provider TLS request");
            let connection =
                ServerConnection::new(Arc::new(config)).expect("Provider TLS connection");
            let mut stream = StreamOwned::new(connection, socket);
            request_tx
                .send(read_http_request(&mut stream))
                .expect("record Provider request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                declared_length.unwrap_or(body.len()),
                body
            )
            .expect("write Provider SSE response");
            stream.flush().expect("flush Provider SSE response");
        });
        Self {
            endpoint: format!("https://localhost:{}/v1/model", address.port()),
            certificate_der: cert.der().to_vec(),
            request,
            server,
        }
    }

    fn provider_config(&self, provider_id: &str) -> StandaloneProviderConfig {
        StandaloneProviderConfig::HttpsSse(self.https_config(provider_id))
    }

    fn anthropic_provider_config(
        &self,
        provider_id: &str,
        max_output_tokens: u32,
        pricing: ProviderTokenPricing,
    ) -> StandaloneProviderConfig {
        StandaloneProviderConfig::HttpsSse(
            self.https_config(provider_id)
                .with_anthropic_messages(max_output_tokens, pricing)
                .expect("Anthropic Messages config"),
        )
    }

    fn https_config(&self, provider_id: &str) -> HttpsSseProviderConfig {
        HttpsSseProviderConfig::try_new(
            provider_id.to_owned(),
            self.endpoint.clone(),
            HttpsSseProviderTimeouts {
                connect: Duration::from_secs(2),
                first_byte: Duration::from_secs(2),
                idle: Duration::from_secs(2),
                total: Duration::from_secs(5),
            },
            HttpsSseProviderLimits {
                response_bytes: 64 * 1024,
                event_bytes: 8 * 1024,
                events: 64,
            },
        )
        .expect("Provider HTTPS/SSE config")
        .with_specific_tls_roots(vec![self.certificate_der.clone()])
        .expect("Provider TLS root")
    }

    fn finish(self) -> Vec<u8> {
        self.server.join().expect("join Provider TLS fixture");
        self.request.recv().expect("captured Provider request")
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read Provider request");
        assert_ne!(count, 0, "Provider request closed before body");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = http_content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return request;
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn http_content_length(headers: &[u8]) -> usize {
    std::str::from_utf8(headers)
        .expect("UTF-8 Provider request headers")
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length: ")
                .or_else(|| line.strip_prefix("content-length: "))
        })
        .expect("Provider Content-Length")
        .parse()
        .expect("numeric Provider Content-Length")
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-provider-production-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn data(&self) -> PathBuf {
        self.0.join("control-plane")
    }

    fn secrets(&self) -> PathBuf {
        self.0.join("secrets")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn policy_digest(value: &impl serde::Serialize) -> Sha256Digest {
    let canonical = serde_json::to_value(value).expect("Policy value fixture");
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("serialize Policy fixture"))
    ))
}

fn deny_provider_policy(storage: &mut SqliteStorage) {
    let definition = EnterprisePolicyDefinition {
        default_effect: EnterprisePolicyEffect::Deny,
        child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
        rules: Vec::new(),
    };
    storage
        .enterprise_policy_ledger()
        .expect("open enterprise Policy ledger")
        .write(&EnterprisePolicyWrite {
            policy_id: EnterprisePolicyId(id("pol", 90)),
            policy_kind: EnterprisePolicyKind::Provider,
            scope: EnterprisePolicyScope::Organization {
                organization_id: OrganizationId(id("org", 1)),
            },
            mode: EnterprisePolicyMode::Enforce,
            state: EnterprisePolicyState::Active,
            definition_sha256: policy_digest(&definition),
            definition,
            effective_at: at("2029-12-31T00:00:00.000Z"),
            inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
            base_version: None,
            expected_revision: 0,
            source: EnterprisePolicyVersionSource {
                actor: EnterprisePolicyActor::User {
                    id: UserId(id("usr", 1)),
                },
                request_id: RequestId(id("req", 90)),
            },
            updated_at: at("2029-12-31T00:00:00.000Z"),
        })
        .expect("write Provider deny Policy");
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn encoded_payload(bytes: &[u8]) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn model_open_with_payload(payload: &[u8]) -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", 1));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: at("2030-01-01T00:05:00.000Z"),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: at("2030-01-01T00:00:00.000Z"),
            job_id: ExecutionJobId(id("job", 1)),
            lease_id: LeaseId(id("lse", 1)),
            worker_id: WorkerId(id("wrk", 1)),
            worker_instance_id: WorkerInstanceId(id("wki", 1)),
        },
        message_id: ExecutionMessageId(id("xmsg", 101)),
        model_exchange_id: ModelExchangeId(id("mdl", 1)),
        request: encoded_payload(payload),
        request_id: RequestId(id("req", 101)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "configured-session-route".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at("2030-01-01T00:00:01.000Z"),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 1)),
            product_session_id: ProductSessionId(id("psn", 1)),
            stage_run_id: Some(StageRunId(id("run", 1))),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

fn model_capability(model_id: &str) -> ModelCapability {
    ModelCapability {
        model_id: model_id.to_owned(),
        display_name: format!("{model_id} model"),
        context_window_tokens: 128_000,
        max_output_tokens: 16_000,
        tool_support: ModelToolSupport::Parallel,
        structured_output_support: StructuredOutputSupport::Unsupported,
        reasoning_efforts: vec!["high".to_owned()],
    }
}

fn configure_provider_authority(
    storage: &mut SqliteStorage,
    message: &ModelOpenMessage,
    provider_id: &str,
    model_id: &str,
    adapter_kind: &str,
) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: provider_id.to_owned(),
                display_name: format!("{provider_id} Provider"),
                adapter_kind: adapter_kind.to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![model_capability(model_id)],
            },
        )
        .expect("register loopback Provider");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Loopback Credential".to_owned(),
                    provider_id: provider_id.to_owned(),
                    vault_locator: "local-production://loopback".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_893_456_000_000,
        )
        .expect("create Credential reference");
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: repository_scope(),
                    product_session_id: message.session_identity.product_session_id.clone(),
                },
                request_id: RequestId(id("req", 3)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(ModelRoute {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                }),
                worker_concurrency_limit: 1,
            },
        )
        .expect("configure ProductSession model route");
}

fn commit_execution_job(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let job = ExecutionJob {
        attempt: message.lease.attempt,
        execution_profile: "executor".to_owned(),
        goal: "execute authenticated model request".to_owned(),
        job_id: message.lease.job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: at("2030-01-01T00:05:00.000Z"),
            max_artifact_bytes: 1_000_000,
            max_runtime_seconds: 300,
        },
        payload_digest: message.request.payload_digest.clone(),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: DeliveryId(id("dlv", 1)),
            delivery_task_id: None,
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: message.session_identity.product_session_id.clone(),
            rework_authorization: None,
            stage_run_id: message
                .session_identity
                .stage_run_id
                .clone()
                .expect("delivery stage identity"),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            repository_id: repository_scope().repository_id,
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    let identity = winwincode_control_plane::command_receipt_identity(
        &actor(),
        &Scope::RepositoryScope(repository_scope()),
        RequestId(id("req", 4)),
    )
    .expect("ExecutionJob receipt identity");
    storage
        .commit(&StateCommit::new(
            identity,
            digest('d'),
            "provider-production-execution-job",
            0,
            br#"{"schema":"provider-production-job.v1"}"#.to_vec(),
            vec![NewOutboxEvent::internal(
                format!("execution-job:{}", job.job_id.0),
                "execution.job.dispatch",
                serde_json::to_vec(&job).expect("ExecutionJob JSON"),
            )],
        ))
        .expect("commit durable ExecutionJob");
}

fn execution_scope() -> ExecutionQueueScope {
    let scope = repository_scope();
    ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id: ProductSessionId(id("psn", 1)),
        delivery_id: Some(DeliveryId(id("dlv", 1))),
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let mut boundaries = vec![
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
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
        },
    ];
    boundaries.push(ExecutionAdmissionBoundary::Delivery {
        organization_id: scope.organization_id.clone(),
        delivery_id: scope.delivery_id.clone().expect("delivery scope"),
    });
    boundaries
}

fn register_worker(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let registration = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "standalone-control-plane".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["model".to_owned()],
        capability_digest: digest('a'),
        security_zone: "local".to_owned(),
        max_slots: 1,
        message_id: ExecutionMessageId(id("xmsg", 5)),
        request_id: RequestId(id("req", 5)),
        sent_at: at("2029-12-31T23:59:56.000Z"),
        started_at: at("2029-12-31T23:59:55.000Z"),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
    };
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 1,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 6)),
        observed_at: at("2029-12-31T23:59:58.000Z"),
        sent_at: at("2029-12-31T23:59:58.000Z"),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
    };
    {
        let mut registry = storage.execution_registry().expect("execution registry");
        registry
            .register_worker(&registration)
            .expect("register standalone Worker");
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat)
                .expect("record Worker heartbeat")
                .status,
            StorageLeaseWriteStatus::Accepted
        );
    }
}

fn start_execution_admission(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let scope = execution_scope();
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 1,
        max_queued: 1,
        token_budget: 10_000,
        cost_budget_microunits: 10_000,
        max_runtime_millis: 300_000,
    };
    {
        let mut admission = storage.execution_admission().expect("execution admission");
        for boundary in admission_boundaries(&scope) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("configure execution policy");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: WorkerPoolId(id("wpl", 1)),
                job_id: message.lease.job_id.clone(),
                request_id: RequestId(id("req", 7)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 100,
                reserved_cost_microunits: 100,
                runtime_limit_millis: 300_000,
                submitted_at: at("2029-12-31T23:59:56.000Z"),
            })
            .expect("reserve execution");
        admission
            .start(&ExecutionReservationStart {
                scope,
                worker_pool_id: WorkerPoolId(id("wpl", 1)),
                job_id: message.lease.job_id.clone(),
                request_id: RequestId(id("req", 8)),
                expected_revision: 1,
                started_at: at("2029-12-31T23:59:57.000Z"),
            })
            .expect("start execution");
    }
}

fn claim_lease_and_open_slot(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let claim = ExecutionLeaseClaim {
        expires_at: message.lease.expires_at.clone(),
        fencing_token: message.lease.fencing_token.clone(),
        issued_at: message.lease.issued_at.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", 9)),
        payload_digest: message.request.payload_digest.clone(),
        request_id: RequestId(id("req", 9)),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        attempt: u64::try_from(message.lease.attempt).expect("positive attempt"),
    };
    assert_eq!(
        storage
            .execution_registry()
            .expect("execution registry")
            .claim_execution_job(&claim)
            .expect("claim execution lease")
            .status,
        StorageLeaseWriteStatus::Accepted
    );
    let authority = WorkerSlotAuthority {
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        codex_thread_id: message.session_identity.codex_thread_id.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        attempt: claim.attempt,
        fencing_token: message.lease.fencing_token.clone(),
    };
    let mut slots = storage.worker_session_slots().expect("Worker slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("configure slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority,
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 10)),
            opened_at: message.lease.issued_at.clone(),
        })
        .expect("open WorkerSession slot");
}

fn configure_worker_authority(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    register_worker(storage, message);
    start_execution_admission(storage, message);
    claim_lease_and_open_slot(storage, message);
}

struct FixedClock;

impl ModelAdmissionClock for FixedClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(31_556_300)
    }
}

fn policy() -> LocalModelPolicyAuthority {
    let base = ModelAdmissionPolicyLayer::try_new(
        "standalone-loopback-policy".to_owned(),
        1,
        "budget-2030-01".to_owned(),
        ModelRoutePolicyDecision::Allow,
        ModelAdmissionLimits {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
            concurrent_requests: 100,
            token_budget: 1_000_000,
            cost_budget_micros: 1_000_000,
        },
    )
    .expect("model admission policy");
    LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base,
        enterprise_ceilings: Vec::new(),
    })
    .expect("local model policy authority")
}

fn pool_config() -> ModelRequestPoolConfig {
    ModelRequestPoolConfig {
        max_routes: 4,
        max_active_per_route: 1,
        max_waiting_per_route: 4,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 32,
        max_buffered_bytes_per_stream: 64 * 1024,
        resume_buffered_frames_per_stream: 8,
        resume_buffered_bytes_per_stream: 16 * 1024,
    }
}

fn application_with_providers(
    root: &TestDirectory,
    providers: Vec<StandaloneProviderConfig>,
) -> StandaloneModelExecutionApplication {
    application_with_provider_reservation(
        root,
        providers,
        ProviderAdmissionReservationConfig::try_new(100, 10).expect("Provider reservation config"),
    )
}

fn application_with_provider_reservation(
    root: &TestDirectory,
    providers: Vec<StandaloneProviderConfig>,
    admission: ProviderAdmissionReservationConfig,
) -> StandaloneModelExecutionApplication {
    StandaloneModelExecutionApplication::open_with_clock(
        StandaloneModelExecutionConfig {
            data_directory: root.data(),
            secret_directory: root.secrets(),
            providers,
            admission,
            pool: pool_config(),
            policy: Box::new(policy()),
            retry_policy: Box::new(local_loopback_retry_policy().expect("loopback retry policy")),
        },
        Box::new(FixedClock),
    )
    .expect("open standalone model application")
}

fn application(root: &TestDirectory) -> StandaloneModelExecutionApplication {
    application_with_providers(
        root,
        vec![StandaloneProviderConfig::Loopback {
            provider_id: PROVIDER_ID.to_owned(),
        }],
    )
}

fn setup_for(
    root: &TestDirectory,
    provider_id: &str,
    model_id: &str,
    adapter_kind: &str,
) -> ModelOpenMessage {
    setup_for_secret(
        root,
        provider_id,
        model_id,
        adapter_kind,
        SECRET_FIXTURE.to_vec(),
    )
}

fn setup_for_secret(
    root: &TestDirectory,
    provider_id: &str,
    model_id: &str,
    adapter_kind: &str,
    secret: Vec<u8>,
) -> ModelOpenMessage {
    setup_for_secret_with_payload(
        root,
        provider_id,
        model_id,
        adapter_kind,
        secret,
        INPUT_FIXTURE,
    )
}

fn setup_for_secret_with_payload(
    root: &TestDirectory,
    provider_id: &str,
    model_id: &str,
    adapter_kind: &str,
    secret: Vec<u8>,
    payload: &[u8],
) -> ModelOpenMessage {
    let message = model_open_with_payload(payload);
    let mut storage = SqliteStorage::open(root.data()).expect("open setup storage");
    configure_provider_authority(&mut storage, &message, provider_id, model_id, adapter_kind);
    commit_execution_job(&mut storage, &message);
    configure_worker_authority(&mut storage, &message);
    let resolution = CredentialReferenceService::new(&mut storage)
        .resolve(
            &Scope::OrganizationScope(organization_scope()),
            &CredentialReferenceId(id("crd", 1)),
        )
        .expect("resolve Credential reference");
    drop(storage);
    LocalSecretStoreAdapter::open(root.secrets())
        .expect("open local SecretStore")
        .store(
            &resolution,
            ResolvedSecret::from_bytes(secret).expect("resolved secret"),
        )
        .expect("store loopback credential");
    message
}

fn setup(root: &TestDirectory) -> ModelOpenMessage {
    setup_for(root, PROVIDER_ID, MODEL_ID, "deterministic-loopback")
}

fn open_frame(message: &ModelOpenMessage) -> TypedFrame {
    TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::ModelOpenMessage(message.clone()),
    )
    .expect("typed ModelOpen frame")
}

fn opened(receipt: ModelExecutionPortReceipt) -> ProviderGatewayOpenReceipt {
    let ModelExecutionPortReceipt::Opened(ModelExecutionOpenReceipt::Opened { gateway, .. }) =
        receipt
    else {
        panic!("standalone request must open the Provider");
    };
    gateway
}

fn final_ack(message: &ModelOpenMessage, sequence: &ExecutionSequence) -> ModelAckMessage {
    ModelAckMessage {
        ack_sequence: ExecutionAckSequence(sequence.0),
        error: None,
        kind: ModelAckMessageKind::ModelAck,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", 102)),
        model_exchange_id: message.model_exchange_id.clone(),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at("2030-01-01T00:00:02.000Z"),
        session_identity: message.session_identity.clone(),
        status: LeaseWriteStatus::Accepted,
        worker_session_id: message.worker_session_id.clone(),
    }
}

struct EncodeOnlyCore;

impl ExecutionPortCore for EncodeOnlyCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

fn remote_bytes(frame: &TypedFrame) -> Vec<u8> {
    RemoteTransportAdapter::<EncodeOnlyCore>::encode(frame).expect("canonical remote frame")
}

fn assert_files_omit(root: &Path, needle: &[u8]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(path).expect("read fixture directory") {
                pending.push(entry.expect("fixture entry").path());
            }
        } else if path.is_file() {
            let bytes = fs::read(path).expect("read fixture file");
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "restricted fixture bytes reached a durable file"
            );
        }
    }
}

fn external_sse(delta: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"response.started\",\"responseId\":\"response-https-1\"}}\n\n",
            "data: {{\"type\":\"text.started\",\"index\":0}}\n\n",
            "data: {{\"type\":\"text.delta\",\"index\":0,\"delta\":{delta}}}\n\n",
            "data: {{\"type\":\"text.ended\",\"index\":0}}\n\n",
            "data: {{\"type\":\"usage\",\"inputTokens\":10,\"cachedInputTokens\":0,",
            "\"cacheWriteInputTokens\":0,\"outputTokens\":5,\"reasoningOutputTokens\":0,",
            "\"actualCostMicros\":10}}\n\n",
            "data: {{\"type\":\"response.finished\",\"reason\":\"stop\"}}\n\n"
        ),
        delta = serde_json::to_string(delta).expect("SSE delta JSON")
    )
}

fn anthropic_sse(delta: &str) -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"message-anthropic-1\",",
        "\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",",
        "\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,",
        "\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,",
        "\"cache_creation_input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"check inputs\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":__DELTA__}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",",
        "\"name\":\"read_file\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,",
        "\"delta\":{\"type\":\"input_json_delta\",",
        "\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":3,",
        "\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-2\",",
        "\"name\":\"apply_patch\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":3,",
        "\"delta\":{\"type\":\"input_json_delta\",",
        "\"partial_json\":\"{\\\"input\\\":\\\"*** Begin Patch\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",",
        "\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},",
        "\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,",
        "\"cache_creation_input_tokens\":3,\"output_tokens\":7}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    )
    .replace(
        "__DELTA__",
        &serde_json::to_string(delta).expect("Anthropic SSE delta JSON"),
    )
}

fn assert_provider_request(request: &[u8]) {
    assert!(
        request
            .windows(SECRET_FIXTURE.len())
            .any(|window| window == SECRET_FIXTURE)
    );
    assert!(
        request
            .windows(INPUT_FIXTURE.len())
            .any(|window| window == INPUT_FIXTURE)
    );
    assert!(
        request
            .windows(b"idempotency-key".len())
            .any(|window| { window.eq_ignore_ascii_case(b"idempotency-key") })
    );
}

fn assert_anthropic_provider_request(request: &[u8], upstream_model: &str) {
    assert!(
        request
            .windows(SECRET_FIXTURE.len())
            .any(|window| window == SECRET_FIXTURE)
    );
    assert!(
        request
            .windows(b"idempotency-key".len())
            .any(|window| window.eq_ignore_ascii_case(b"idempotency-key"))
    );
    assert!(
        request
            .windows(b"anthropic-version: 2023-06-01".len())
            .any(|window| window.eq_ignore_ascii_case(b"anthropic-version: 2023-06-01"))
    );
    assert!(!String::from_utf8_lossy(request).contains("[1m]"));
    assert!(
        !request
            .windows(ANTHROPIC_INPUT_FIXTURE.len())
            .any(|window| window == ANTHROPIC_INPUT_FIXTURE)
    );

    let header_end = find_bytes(request, b"\r\n\r\n").expect("Provider request header end");
    let body: serde_json::Value =
        serde_json::from_slice(&request[header_end + 4..]).expect("Anthropic request JSON");
    assert_eq!(body["model"], upstream_model);
    assert_eq!(body["stream"], true);
    assert_eq!(body["output_config"], serde_json::json!({"effort": "high"}));
    assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
    assert_eq!(body["tool_choice"], serde_json::json!({"type": "auto"}));
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(body["tools"][1]["name"], "apply_patch");
    assert_eq!(body["tools"][1]["input_schema"]["required"][0], "input");
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "provider-production-private-input"
    );
}

#[test]
fn standalone_local_remote_restart_and_terminal_ack_share_one_durable_runtime() {
    let root = TestDirectory::new("vertical");
    let message = setup(&root);
    let frame = open_frame(&message);
    let bytes = remote_bytes(&frame);
    assert_eq!(
        RemoteTransportAdapter::<EncodeOnlyCore>::decode(&bytes).expect("decode remote frame"),
        frame
    );

    let first = {
        let mut app = application(&root);
        opened(app.accept_local(&frame).expect("local ModelOpen"))
    };
    let resolution = {
        let mut storage = SqliteStorage::open(root.data()).expect("open cleanup storage");
        CredentialReferenceService::new(&mut storage)
            .resolve(
                &Scope::OrganizationScope(organization_scope()),
                &CredentialReferenceId(id("crd", 1)),
            )
            .expect("resolve cleanup Credential")
    };
    LocalSecretStoreAdapter::open(root.secrets())
        .expect("open cleanup SecretStore")
        .delete(&resolution)
        .expect("remove secret before exact restart replay");

    let mut restarted = application(&root);
    let replay = opened(
        restarted
            .accept_remote(&bytes)
            .expect("remote exact restart replay"),
    );
    assert_eq!(replay.model_exchange_id, first.model_exchange_id);
    assert_eq!(replay.adapter_request_id, first.adapter_request_id);
    assert_eq!(replay.route, first.route);
    assert!(replay.idempotent_replay);

    let batch = restarted
        .complete_loopback(&replay, &at("2030-01-01T00:00:01.000Z"))
        .expect("complete loopback Provider stream");
    assert!(!batch.chunks.is_empty());
    assert!(batch.chunks.last().expect("terminal chunk").is_final);
    assert!(batch.chunks.iter().all(|chunk| {
        chunk.worker_session_id == message.worker_session_id
            && chunk.session_identity == message.session_identity
    }));
    let acknowledgement = final_ack(
        &message,
        &batch.chunks.last().expect("terminal chunk").sequence,
    );
    let ack_frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::ModelAckMessage(acknowledgement),
    )
    .expect("typed final ModelAck");
    restarted
        .accept_remote(&remote_bytes(&ack_frame))
        .expect("remote terminal acknowledgement");
    drop(restarted);

    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), b"provider-production-private-input");
}

#[test]
fn enterprise_provider_policy_denies_before_secret_resolution_and_replays_one_audit() {
    let root = TestDirectory::new("enterprise-policy-denied");
    let message = setup(&root);
    let resolution = {
        let mut storage = SqliteStorage::open(root.data()).expect("open Policy setup storage");
        deny_provider_policy(&mut storage);
        CredentialReferenceService::new(&mut storage)
            .resolve(
                &Scope::OrganizationScope(organization_scope()),
                &CredentialReferenceId(id("crd", 1)),
            )
            .expect("resolve Credential for deletion")
    };
    LocalSecretStoreAdapter::open(root.secrets())
        .expect("open SecretStore")
        .delete(&resolution)
        .expect("delete secret before guarded open");
    let frame = open_frame(&message);
    let mut app = application(&root);
    app.accept_local(&frame)
        .expect_err("enterprise Provider Policy must deny");
    drop(app);

    let mut storage = SqliteStorage::open(root.data()).expect("reopen denied runtime");
    let audit = storage
        .enterprise_policy_evaluation_ledger()
        .expect("open Policy audit")
        .scan_audit(None, 10)
        .expect("scan Policy audit");
    assert_eq!(
        audit.entries.len(),
        2,
        "Model allow and Provider deny are audited"
    );
    assert_eq!(
        audit.entries[1].decision.outcome,
        winwincode_storage::EnterprisePolicyEvaluationOutcome::Deny
    );
    drop(storage);

    let mut restarted = application(&root);
    restarted
        .accept_local(&frame)
        .expect_err("exact denied replay stays denied without a secret");
    drop(restarted);
    let mut storage = SqliteStorage::open(root.data()).expect("reopen replayed runtime");
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("open replay audit")
            .scan_audit(None, 10)
            .expect("scan replay audit")
            .entries
            .len(),
        2,
        "exact replay does not duplicate Policy audit"
    );
}

#[test]
fn durable_identity_rejects_stale_fence_and_foreign_session() {
    let root = TestDirectory::new("identity");
    let message = setup(&root);
    let identity = DurableProviderGatewayIdentitySource::open(root.data())
        .expect("open durable identity source");
    let authorized = identity
        .authorize(&message)
        .expect("authorize exact envelope");
    assert_eq!(
        authorized.target(),
        &ModelSettingsTarget::ProductSession {
            repository_scope: repository_scope(),
            product_session_id: ProductSessionId(id("psn", 1)),
        }
    );

    let mut stale = message.clone();
    stale.lease.fencing_token = FencingToken("0".to_owned());
    assert!(identity.authorize(&stale).is_err());
    let mut foreign = message;
    foreign.worker_session_id = WorkerSessionId(id("wsn", 2));
    foreign.session_identity.worker_session_id = foreign.worker_session_id.clone();
    assert!(identity.authorize(&foreign).is_err());
    assert_files_omit(&root.data(), SECRET_FIXTURE);
}

#[test]
fn external_https_sse_completion_and_credential_leak_share_durable_terminal_path() {
    const EXTERNAL_PROVIDER: &str = "winwincode-https-fixture";
    const EXTERNAL_MODEL: &str = "https-fixture-model";
    for (label, delta, expect_failure) in [
        ("external-success", "verified response", false),
        (
            "external-credential-leak",
            std::str::from_utf8(SECRET_FIXTURE).expect("UTF-8 secret fixture"),
            true,
        ),
    ] {
        let root = TestDirectory::new(label);
        let message = setup_for(
            &root,
            EXTERNAL_PROVIDER,
            EXTERNAL_MODEL,
            "verified-https-sse",
        );
        let tls = HttpsFixture::start(external_sse(delta));
        let provider = tls.provider_config(EXTERNAL_PROVIDER);
        let mut application = application_with_providers(&root, vec![provider]);
        let open = opened(
            application
                .accept_local(&open_frame(&message))
                .expect("external Provider ModelOpen"),
        );
        let batch = application
            .complete_https_sse(&open, &at("2030-01-01T00:00:02.000Z"))
            .unwrap_or_else(|error| panic!("{label}: {error:?}"));
        assert!(batch.chunks.last().expect("terminal chunk").is_final);
        assert_eq!(
            batch
                .flow
                .gateway_terminal
                .as_ref()
                .expect("Gateway terminal")
                .outcome
                == winwincode_control_plane::ProviderGatewayTerminalOutcome::Failed,
            expect_failure
        );
        assert_provider_request(&tls.finish());
        drop(application);
        assert_files_omit(&root.data(), SECRET_FIXTURE);
        assert_files_omit(&root.data(), INPUT_FIXTURE);
    }
}

#[test]
fn anthropic_messages_translation_stream_usage_and_secret_gate_share_the_durable_runtime() {
    const ANTHROPIC_PROVIDER: &str = "winwincode-anthropic-fixture";
    const UPSTREAM_MODEL: &str = "glm-5.2";
    let root = TestDirectory::new("anthropic-messages");
    let message = setup_for_secret_with_payload(
        &root,
        ANTHROPIC_PROVIDER,
        UPSTREAM_MODEL,
        "anthropic-messages",
        SECRET_FIXTURE.to_vec(),
        ANTHROPIC_INPUT_FIXTURE,
    );
    let tls = HttpsFixture::start(anthropic_sse("OK"));
    let provider = tls.anthropic_provider_config(
        ANTHROPIC_PROVIDER,
        64,
        ProviderTokenPricing {
            input_micros_per_million_tokens: 300_000,
            cached_input_micros_per_million_tokens: 0,
            cache_write_micros_per_million_tokens: 0,
            output_micros_per_million_tokens: 1_000_000,
            reasoning_output_micros_per_million_tokens: 0,
        },
    );
    let mut application = application_with_providers(&root, vec![provider]);
    let open = opened(
        application
            .accept_local(&open_frame(&message))
            .expect("Anthropic Messages ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &at("2030-01-01T00:00:02.000Z"))
        .expect("Anthropic Messages completion");
    assert!(batch.chunks.last().expect("terminal chunk").is_final);
    let terminal = batch.flow.gateway_terminal.expect("Gateway terminal");
    assert_eq!(
        terminal.outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Succeeded
    );
    assert_eq!(
        terminal.admission.outcome,
        winwincode_control_plane::ModelReservationTerminalOutcome::Completed
    );
    assert_eq!(terminal.admission.actual_tokens, 22);
    assert_eq!(terminal.admission.actual_cost_micros, 10);
    let payloads = batch
        .chunks
        .iter()
        .filter_map(|chunk| chunk.payload.as_ref())
        .map(|payload| {
            let bytes = STANDARD
                .decode(&payload.data_base64)
                .expect("decode canonical Provider frame");
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .expect("canonical Provider frame JSON")
        })
        .collect::<Vec<_>>();
    assert!(payloads.iter().any(|payload| {
        payload["type"] == "reasoning_content_delta" && payload["delta"] == "check inputs"
    }));
    assert!(payloads.iter().any(|payload| {
        payload["type"] == "tool_call_input_delta" && payload["delta"] == "{\"path\":\"README.md\"}"
    }));
    assert!(payloads.iter().any(|payload| {
        payload["type"] == "tool_call_input_delta" && payload["delta"] == "*** Begin Patch"
    }));
    assert_anthropic_provider_request(&tls.finish(), UPSTREAM_MODEL);
    drop(application);
    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), b"provider-production-private-input");
    assert_files_omit(&root.data(), b"[1m]");
}

#[test]
fn disconnected_anthropic_messages_stream_settles_failure_without_durable_private_data() {
    const ANTHROPIC_PROVIDER: &str = "winwincode-anthropic-disconnect-fixture";
    const UPSTREAM_MODEL: &str = "mimo-v2.5-pro";
    let root = TestDirectory::new("anthropic-messages-disconnect");
    let message = setup_for_secret_with_payload(
        &root,
        ANTHROPIC_PROVIDER,
        UPSTREAM_MODEL,
        "anthropic-messages",
        SECRET_FIXTURE.to_vec(),
        ANTHROPIC_INPUT_FIXTURE,
    );
    let tls = HttpsFixture::start_truncated(anthropic_sse("incomplete"));
    let provider =
        tls.anthropic_provider_config(ANTHROPIC_PROVIDER, 64, ProviderTokenPricing::default());
    let mut application = application_with_providers(&root, vec![provider]);
    let open = opened(
        application
            .accept_local(&open_frame(&message))
            .expect("Anthropic Messages ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &at("2030-01-01T00:00:02.000Z"))
        .expect("disconnected Anthropic Messages settlement");
    assert_eq!(
        batch
            .flow
            .gateway_terminal
            .expect("failure terminal")
            .outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Failed
    );
    assert_anthropic_provider_request(&tls.finish(), UPSTREAM_MODEL);
    drop(application);
    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), b"provider-production-private-input");
    assert_files_omit(&root.data(), b"[1m]");
}

#[test]
fn anthropic_credential_echo_is_rejected_before_canonical_or_durable_output() {
    const ANTHROPIC_PROVIDER: &str = "winwincode-anthropic-leak-fixture";
    const UPSTREAM_MODEL: &str = "glm-5.2";
    let root = TestDirectory::new("anthropic-messages-credential-echo");
    let message = setup_for_secret_with_payload(
        &root,
        ANTHROPIC_PROVIDER,
        UPSTREAM_MODEL,
        "anthropic-messages",
        SECRET_FIXTURE.to_vec(),
        ANTHROPIC_INPUT_FIXTURE,
    );
    let reflected = std::str::from_utf8(SECRET_FIXTURE).expect("UTF-8 credential fixture");
    let tls = HttpsFixture::start(anthropic_sse(reflected));
    let provider =
        tls.anthropic_provider_config(ANTHROPIC_PROVIDER, 64, ProviderTokenPricing::default());
    let mut application = application_with_providers(&root, vec![provider]);
    let open = opened(
        application
            .accept_local(&open_frame(&message))
            .expect("Anthropic Messages ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &at("2030-01-01T00:00:02.000Z"))
        .expect("credential echo failure settlement");
    assert_eq!(
        batch
            .flow
            .gateway_terminal
            .expect("failure terminal")
            .outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Failed
    );
    for payload in batch
        .chunks
        .iter()
        .filter_map(|chunk| chunk.payload.as_ref())
    {
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .expect("decode failure frame");
        assert!(
            !bytes
                .windows(SECRET_FIXTURE.len())
                .any(|value| value == SECRET_FIXTURE)
        );
    }
    assert_anthropic_provider_request(&tls.finish(), UPSTREAM_MODEL);
    drop(application);
    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), b"provider-production-private-input");
}

#[test]
fn external_stream_lost_across_restart_fails_closed_without_second_provider_call() {
    const EXTERNAL_PROVIDER: &str = "winwincode-https-restart-fixture";
    const EXTERNAL_MODEL: &str = "https-restart-model";
    let root = TestDirectory::new("external-restart");
    let message = setup_for(
        &root,
        EXTERNAL_PROVIDER,
        EXTERNAL_MODEL,
        "verified-https-sse",
    );
    let tls = HttpsFixture::start(external_sse("response lost with process"));
    let provider = tls.provider_config(EXTERNAL_PROVIDER);
    let open = {
        let mut application = application_with_providers(&root, vec![provider.clone()]);
        opened(
            application
                .accept_local(&open_frame(&message))
                .expect("external Provider ModelOpen"),
        )
    };
    assert_provider_request(&tls.finish());
    let mut restarted = application_with_providers(&root, vec![provider]);
    let batch = restarted
        .complete_https_sse(&open, &at("2030-01-01T00:00:03.000Z"))
        .expect("settle interrupted external stream");
    assert_eq!(
        batch
            .flow
            .gateway_terminal
            .expect("failure terminal")
            .outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Failed
    );
    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), INPUT_FIXTURE);
}

#[test]
fn truncated_external_stream_settles_failure_and_cleans_durable_resources() {
    const EXTERNAL_PROVIDER: &str = "winwincode-https-disconnect-fixture";
    const EXTERNAL_MODEL: &str = "https-disconnect-model";
    let root = TestDirectory::new("external-disconnect");
    let message = setup_for(
        &root,
        EXTERNAL_PROVIDER,
        EXTERNAL_MODEL,
        "verified-https-sse",
    );
    let tls = HttpsFixture::start_truncated(external_sse("truncated response"));
    let provider = tls.provider_config(EXTERNAL_PROVIDER);
    let mut application = application_with_providers(&root, vec![provider]);
    let open = opened(
        application
            .accept_local(&open_frame(&message))
            .expect("external Provider ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &at("2030-01-01T00:00:04.000Z"))
        .expect("settle truncated external stream");
    assert_eq!(
        batch
            .flow
            .gateway_terminal
            .expect("failure terminal")
            .outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Failed
    );
    assert_provider_request(&tls.finish());
    drop(application);
    assert_files_omit(&root.data(), SECRET_FIXTURE);
    assert_files_omit(&root.data(), INPUT_FIXTURE);
}

struct LiveAnthropicGate {
    label: &'static str,
    gate_environment: &'static str,
    endpoint_environment: &'static str,
    provider_environment: &'static str,
    secret_file_environment: &'static str,
    tls_root_environment: &'static str,
    upstream_model: &'static str,
}

fn run_live_anthropic_gate(config: &LiveAnthropicGate) {
    assert_eq!(
        std::env::var(config.gate_environment).as_deref(),
        Ok("1"),
        "set the explicit live Provider gate to 1"
    );
    let endpoint =
        std::env::var(config.endpoint_environment).expect("configured live Provider endpoint");
    let provider_id =
        std::env::var(config.provider_environment).expect("configured live Provider identifier");
    let secret_path = PathBuf::from(
        std::env::var_os(config.secret_file_environment)
            .expect("configured live Provider secret file"),
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&secret_path)
            .expect("live Provider secret metadata")
            .permissions()
            .mode()
            & 0o077,
        0,
        "live Provider secret file must not grant group or other permissions"
    );
    let live_secret = fs::read(&secret_path).expect("read live Provider secret file");
    let root = TestDirectory::new(config.label);
    let message = setup_for_secret_with_payload(
        &root,
        &provider_id,
        config.upstream_model,
        "anthropic-messages",
        live_secret.clone(),
        ANTHROPIC_INPUT_FIXTURE,
    );
    let mut provider = HttpsSseProviderConfig::try_new(
        provider_id,
        endpoint,
        HttpsSseProviderTimeouts {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            total: Duration::from_mins(2),
        },
        HttpsSseProviderLimits {
            response_bytes: 16 * 1024 * 1024,
            event_bytes: 1024 * 1024,
            events: 10_000,
        },
    )
    .expect("live Provider config");
    if let Some(root_path) = std::env::var_os(config.tls_root_environment) {
        provider = provider
            .with_specific_tls_roots(vec![fs::read(root_path).expect("read live TLS root")])
            .expect("live Provider TLS root");
    }
    provider = provider
        .with_anthropic_messages(64, ProviderTokenPricing::default())
        .expect("live Anthropic Messages config");
    let mut application = application_with_provider_reservation(
        &root,
        vec![StandaloneProviderConfig::HttpsSse(provider)],
        ProviderAdmissionReservationConfig::try_new(10_000, 10)
            .expect("live Provider reservation config"),
    );
    let open = opened(
        application
            .accept_local(&open_frame(&message))
            .expect("live Provider ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &at("2030-01-01T00:00:02.000Z"))
        .expect("live Provider SSE completion");
    assert!(batch.chunks.last().expect("live terminal chunk").is_final);
    let terminal = batch.flow.gateway_terminal.expect("live Gateway terminal");
    assert_eq!(
        terminal.outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Succeeded
    );
    assert_eq!(
        terminal.admission.outcome,
        winwincode_control_plane::ModelReservationTerminalOutcome::Completed
    );
    assert!(terminal.admission.actual_tokens > 0);
    drop(application);
    assert_files_omit(&root.data(), &live_secret);
    assert_files_omit(&root.data(), b"provider-production-private-input");
    assert_files_omit(&root.data(), b"[1m]");
}

#[test]
#[ignore = "requires the explicit GLM endpoint and secret-file live gate"]
fn live_glm_5_2_anthropic_gateway_settlement_and_leak_gate() {
    run_live_anthropic_gate(&LiveAnthropicGate {
        label: "live-glm-5-2",
        gate_environment: "WINWINCODE_GLM_LIVE_PROVIDER_GATE",
        endpoint_environment: "WINWINCODE_GLM_LIVE_PROVIDER_ENDPOINT",
        provider_environment: "WINWINCODE_GLM_LIVE_PROVIDER_ID",
        secret_file_environment: "WINWINCODE_GLM_LIVE_PROVIDER_SECRET_FILE",
        tls_root_environment: "WINWINCODE_GLM_LIVE_PROVIDER_TLS_ROOT_DER",
        upstream_model: "glm-5.2",
    });
}

#[test]
#[ignore = "requires the explicit MIMO endpoint and secret-file live gate"]
fn live_mimo_v2_5_pro_anthropic_gateway_settlement_and_leak_gate() {
    run_live_anthropic_gate(&LiveAnthropicGate {
        label: "live-mimo-v2-5-pro",
        gate_environment: "WINWINCODE_MIMO_LIVE_PROVIDER_GATE",
        endpoint_environment: "WINWINCODE_MIMO_LIVE_PROVIDER_ENDPOINT",
        provider_environment: "WINWINCODE_MIMO_LIVE_PROVIDER_ID",
        secret_file_environment: "WINWINCODE_MIMO_LIVE_PROVIDER_SECRET_FILE",
        tls_root_environment: "WINWINCODE_MIMO_LIVE_PROVIDER_TLS_ROOT_DER",
        upstream_model: "mimo-v2.5-pro",
    });
}

#[path = "provider_production/live_delivery.rs"]
mod live_delivery;
