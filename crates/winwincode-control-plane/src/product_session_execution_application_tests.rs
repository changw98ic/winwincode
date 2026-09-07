// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{Connection, params};
use serde_json::Value;
use winwincode_api::generated::ModelRoute;
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, CredentialReferenceId, ExecutionJobId, ExecutionMessageId,
    ExecutionSequence, FencingToken, Instant, LeaseId, ModelExchangeId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, SessionIdentity, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionJob, ExecutionLeaseStamp, ModelChunkMessage, ModelChunkMessageKind,
};
use winwincode_session::SessionBindingIdentity;
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, ProductStateStorage, PublicEventActor,
    ReceiptScopeKey, SqliteStorage, StateCommit, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
    public_receipt_identity, receipt_scope_key,
};

use super::*;
use crate::{
    ContinueProductSessionCommand, CreateProductSessionCommand, ProductSessionExecutionConfig,
    SubmitChatMessageCommand,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Directory(PathBuf);

impl Directory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-product-session-execution-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ProjectionFixture {
    directory: Directory,
    storage: SqliteStorage,
    repository_scope: RepositoryScope,
    receipt_scope: ReceiptScopeKey,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    model_exchange_id: ModelExchangeId,
    authority: WorkerSlotAuthority,
    lease: ExecutionLeaseStamp,
}

impl ProjectionFixture {
    fn new(label: &str, seed: u64) -> Self {
        let directory = Directory::new(label);
        let repository_scope = repository_scope(seed);
        let public_scope = public_repository_scope(&repository_scope);
        let receipt_scope = receipt_scope_key(&public_scope).expect("receipt scope");
        let product_session_id = ProductSessionId(id("psn", seed));
        let model_exchange_id = ModelExchangeId(id("mdl", seed));
        let mut storage = SqliteStorage::open(&directory.0).expect("storage");
        let execution_job_id = seed_product_session(
            &mut storage,
            &repository_scope,
            &receipt_scope,
            &product_session_id,
            seed,
        );
        let execution_scope = execution_scope(&repository_scope, &product_session_id);
        seed_worker(&mut storage, seed);
        let (authority, lease) =
            seed_runtime(&mut storage, &execution_scope, &execution_job_id, seed);
        bind_product_session(
            &mut storage,
            &repository_scope,
            &product_session_id,
            &model_exchange_id,
            &execution_scope,
            &authority,
            seed,
        );
        seed_staged_binding(
            &mut storage,
            &repository_scope,
            &product_session_id,
            &execution_scope,
            &authority,
            seed,
        );
        Self {
            directory,
            storage,
            repository_scope,
            receipt_scope,
            product_session_id,
            execution_job_id,
            model_exchange_id,
            authority,
            lease,
        }
    }

    fn chunk(&self, raw_sequence: u64, delta: &str) -> ModelChunkMessage {
        let payload_json = serde_json::json!({
            "type": "output_text_delta",
            "delta": delta,
        })
        .to_string();
        ModelChunkMessage {
            error: None,
            is_final: false,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: self.lease.clone(),
            message_id: ExecutionMessageId(id("xmsg", 10_000 + raw_sequence)),
            model_exchange_id: self.model_exchange_id.clone(),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: STANDARD.encode(payload_json.as_bytes()),
                payload_digest: Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload_json.as_bytes())
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: at(20),
            sequence: ExecutionSequence(i64::try_from(raw_sequence).expect("raw sequence")),
            session_identity: SessionIdentity {
                codex_thread_id: self.authority.codex_thread_id.clone(),
                product_session_id: self.product_session_id.clone(),
                stage_run_id: None,
                worker_session_id: self.authority.worker_session_id.clone(),
            },
            worker_session_id: self.authority.worker_session_id.clone(),
        }
    }

    fn assistant_content(&mut self) -> String {
        ProductSessionService::new(&mut self.storage)
            .get(&self.receipt_scope, &self.product_session_id)
            .expect("ProductSession read")
            .expect("ProductSession")
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .map(|message| message.content.clone())
            .unwrap_or_default()
    }

    fn public_sequence(&mut self) -> u64 {
        ProductSessionService::new(&mut self.storage)
            .last_assistant_stream_sequence(
                &self.receipt_scope,
                &self.product_session_id,
                &self.model_exchange_id,
            )
            .expect("public stream sequence")
    }

    fn source(&mut self, chunk: ModelChunkMessage) -> DurableProviderPublicFrame {
        let staged = load_staged_binding_for_projection(&mut self.storage, &chunk.lease.job_id)
            .expect("staged binding");
        let source = DurableProviderPublicFrame {
            schema: "winwincode.product-session-provider-frame.v1".to_owned(),
            repository_scope: self.repository_scope.clone(),
            product_session_id: self.product_session_id.clone(),
            execution_job_id: self.execution_job_id.clone(),
            model_exchange_id: self.model_exchange_id.clone(),
            public_text_delta: public_text_delta(&chunk)
                .expect("canonical public frame")
                .expect("public delta"),
            chunk,
            public_stream_sequence: 0,
        };
        validate_provider_source(&staged, &source).expect("staged source");
        validate_bound_provider_source(&mut self.storage, &source).expect("bound source");
        source
    }

    fn pending_source(&mut self, chunk: ModelChunkMessage) -> DurableProviderPublicFrame {
        let source = self.source(chunk);
        persist_provider_batch_sources(&mut self.storage, &[source])
            .expect("persist pending source")
            .into_iter()
            .next()
            .expect("new pending source")
    }

    fn catalog_stream_id(&self) -> String {
        format!(
            "product-sessions:{:x}",
            Sha256::digest(self.receipt_scope.as_bytes())
        )
    }
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2032-01-01T00:00:{:02}.000Z", second % 60))
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn command_context(
    repository_scope: &RepositoryScope,
    request_seed: u64,
    expected_revision: u64,
) -> ProductSessionCommandContext {
    let actor = PublicEventActor::User {
        id: UserId(id("usr", 1)),
    };
    let public_scope = public_repository_scope(repository_scope);
    ProductSessionCommandContext {
        receipt_identity: public_receipt_identity(
            &actor,
            &public_scope,
            RequestId(id("req", request_seed)),
        )
        .expect("receipt identity"),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request_seed)),
        occurred_at: at(request_seed),
        public_actor: actor,
        public_scope,
    }
}

fn seed_product_session(
    storage: &mut SqliteStorage,
    repository_scope: &RepositoryScope,
    receipt_scope: &ReceiptScopeKey,
    product_session_id: &ProductSessionId,
    seed: u64,
) -> ExecutionJobId {
    let mut service = ProductSessionService::new(storage);
    service
        .create(&CreateProductSessionCommand {
            context: command_context(repository_scope, seed * 100, 0),
            product_session_id: product_session_id.clone(),
            project_id: repository_scope.project_id.clone(),
            repository_id: repository_scope.repository_id.clone(),
            title: "Provider projection".to_owned(),
            model_route: ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", seed)),
                model_id: "fixture-model".to_owned(),
                provider_id: "fixture-provider".to_owned(),
            },
        })
        .expect("create ProductSession");
    let execution_config = ProductSessionExecutionConfig::try_new(
        repository_scope.clone(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config");
    let receipt = service
        .submit_chat(&SubmitChatMessageCommand {
            context: command_context(repository_scope, seed * 100 + 1, 1),
            product_session_id: product_session_id.clone(),
            message: "project canonical Provider output".to_owned(),
            execution_config,
        })
        .expect("submit Chat");
    assert_eq!(
        receipt.mutation.record.turn_intents()[0].product_session_id,
        *product_session_id
    );
    assert_eq!(
        receipt_scope_key(&public_repository_scope(repository_scope)).expect("scope"),
        *receipt_scope
    );
    receipt.turn_intent.execution_job_id
}

fn execution_scope(
    repository_scope: &RepositoryScope,
    product_session_id: &ProductSessionId,
) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: repository_scope.organization_id.clone(),
        workspace_id: repository_scope.workspace_id.clone(),
        project_id: repository_scope.project_id.clone(),
        repository_id: repository_scope.repository_id.clone(),
        product_session_id: product_session_id.clone(),
        delivery_id: None,
    }
}

fn worker_pool(seed: u64) -> WorkerPoolId {
    WorkerPoolId(id("wpl", seed))
}

fn seed_worker(storage: &mut SqliteStorage, seed: u64) {
    let registration = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "projection-fixture".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".to_owned(),
        max_slots: 1,
        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 2)),
        request_id: RequestId(id("req", seed * 100 + 2)),
        sent_at: at(2),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
    };
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 1,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 3)),
        observed_at: at(3),
        sent_at: at(3),
        worker_id: registration.worker_id.clone(),
        worker_instance_id: registration.worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker(&registration)
        .expect("register Worker");
    registry
        .record_heartbeat(&heartbeat)
        .expect("heartbeat Worker");
}

fn admission_boundaries(
    scope: &ExecutionQueueScope,
    pool: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
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
    ]
}

fn seed_runtime(
    storage: &mut SqliteStorage,
    scope: &ExecutionQueueScope,
    job_id: &ExecutionJobId,
    seed: u64,
) -> (WorkerSlotAuthority, ExecutionLeaseStamp) {
    seed_execution_admission(storage, scope, job_id, seed);
    let record = storage
        .execution_queue()
        .expect("queue")
        .load_job(scope, job_id)
        .expect("load Job")
        .expect("Job");
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload).expect("ExecutionJob");
    let claim = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken(seed.to_string()),
        issued_at: at(5),
        job_id: job_id.clone(),
        lease_id: LeaseId(id("lse", seed)),
        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 6)),
        payload_digest: job.payload_digest,
        request_id: RequestId(id("req", seed * 100 + 6)),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
        attempt: 1,
    };
    storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("claim execution");
    let authority = WorkerSlotAuthority {
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", seed)),
        codex_thread_id: CodexThreadId(id("cdx", seed)),
        job_id: job_id.clone(),
        lease_id: claim.lease_id.clone(),
        attempt: 1,
        fencing_token: claim.fencing_token.clone(),
    };
    let mut slots = storage.worker_session_slots().expect("slots");
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
        .expect("slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: authority.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", seed * 100 + 7)),
            opened_at: at(6),
        })
        .expect("open slot");
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at,
        fencing_token: claim.fencing_token,
        issued_at: claim.issued_at,
        job_id: claim.job_id,
        lease_id: claim.lease_id,
        worker_id: claim.worker_id,
        worker_instance_id: claim.worker_instance_id,
    };
    (authority, lease)
}

fn seed_execution_admission(
    storage: &mut SqliteStorage,
    scope: &ExecutionQueueScope,
    job_id: &ExecutionJobId,
    seed: u64,
) {
    let pool = worker_pool(seed);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 100_000,
        cost_budget_microunits: 1_000_000,
        max_runtime_millis: 60_000,
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in admission_boundaries(scope, &pool) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("admission policy");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: pool.clone(),
                job_id: job_id.clone(),
                request_id: RequestId(id("req", seed * 100 + 4)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 100,
                reserved_cost_microunits: 1_000,
                runtime_limit_millis: 30_000,
                submitted_at: at(4),
            })
            .expect("reserve execution");
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool,
                job_id: job_id.clone(),
                request_id: RequestId(id("req", seed * 100 + 5)),
                expected_revision: 1,
                started_at: at(5),
            })
            .expect("start execution");
    }
}

fn bind_product_session(
    storage: &mut SqliteStorage,
    repository_scope: &RepositoryScope,
    product_session_id: &ProductSessionId,
    model_exchange_id: &ModelExchangeId,
    execution_scope: &ExecutionQueueScope,
    authority: &WorkerSlotAuthority,
    seed: u64,
) {
    let command = ContinueProductSessionCommand {
        context: command_context(repository_scope, seed * 100 + 8, 2),
        product_session_id: product_session_id.clone(),
        binding_identity: SessionBindingIdentity::product_session(
            product_session_id.clone(),
            authority.job_id.clone(),
        )
        .expect("binding identity"),
        runtime_authority: authority.clone(),
        execution_scope: execution_scope.clone(),
        worker_pool_id: worker_pool(seed),
        model_exchange_id: model_exchange_id.clone(),
    };
    ProductSessionService::new(storage)
        .continue_session(&command)
        .expect("bind ProductSession");
}

fn seed_staged_binding(
    storage: &mut SqliteStorage,
    repository_scope: &RepositoryScope,
    product_session_id: &ProductSessionId,
    execution_scope: &ExecutionQueueScope,
    authority: &WorkerSlotAuthority,
    seed: u64,
) {
    let staged = StagedWorkerBinding {
        product_session_id: product_session_id.clone(),
        execution_scope: execution_scope.clone(),
        worker_pool_id: worker_pool(seed),
        execution_job_id: authority.job_id.clone(),
        runtime_authority: authority.clone(),
        bound_at: at(7),
        source_message_id: ExecutionMessageId(id("xmsg", seed * 100 + 8)),
    };
    let actor = PublicEventActor::System {
        id: SystemActorId(SYSTEM_ACTOR_ID.to_owned()),
    };
    let identity = public_receipt_identity(
        &actor,
        &public_repository_scope(repository_scope),
        RequestId(id("req", seed * 100 + 9)),
    )
    .expect("staged receipt");
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            binding_stream_id(&authority.job_id),
            0,
            serde_json::to_vec(&staged).expect("staged binding JSON"),
            vec![internal_execution_event(&RequestId(id(
                "req",
                seed * 100 + 9,
            )))],
        ))
        .expect("staged binding");
}

fn seed_projected_chat_sequence(fixture: &ProjectionFixture, sequence: u64) {
    let connection = Connection::open(fixture.storage.database_path()).expect("fixture database");
    let stream_id = fixture.catalog_stream_id();
    let (revision, payload) = connection
        .query_row(
            "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .expect("ProductSession catalog state");
    let mut catalog: Value = serde_json::from_slice(&payload).expect("ProductSession catalog JSON");
    let session = catalog
        .get_mut("sessions")
        .and_then(Value::as_object_mut)
        .and_then(|sessions| sessions.get_mut(&fixture.product_session_id.0))
        .expect("fixture ProductSession");
    let assistant = session
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| {
            messages.iter_mut().find(|message| {
                message.get("modelExchangeId").and_then(Value::as_str)
                    == Some(fixture.model_exchange_id.0.as_str())
            })
        })
        .expect("fixture assistant message");
    assistant["projection"]["content"] =
        Value::String("x".repeat(usize::try_from(sequence).expect("fixture sequence fits memory")));
    assistant["lastStreamSequence"] = Value::from(sequence);
    let next_revision = revision.checked_add(1).expect("catalog revision");
    catalog["revision"] = Value::from(next_revision);
    let next_payload = serde_json::to_vec(&catalog).expect("seeded catalog JSON");
    assert_eq!(
        connection
            .execute(
                "UPDATE product_state SET revision = ?2, payload = ?3 \
                 WHERE stream_id = ?1 AND revision = ?4",
                params![stream_id, next_revision, next_payload, revision],
            )
            .expect("seed projected Chat state"),
        1
    );
}

fn seed_provider_history(
    fixture: &mut ProjectionFixture,
    source: &DurableProviderPublicFrame,
    sequence: u64,
) {
    let stream_id = provider_history_stream(source);
    let (mut history, revision) =
        load_provider_history(&fixture.storage, &stream_id, source).expect("Provider history");
    for public_stream_sequence in 2..=sequence {
        history.entries.insert(
            format!("provider-frame:fixture-{public_stream_sequence:016x}"),
            ProviderPublicFrameHistoryEntry {
                body_sha256: Sha256Digest(format!("sha256:{public_stream_sequence:064x}")),
                public_stream_sequence,
            },
        );
    }
    let payload = encode_provider_history(&history).expect("seeded Provider history JSON");
    let request_id = derived_request_id(b"provider-history-test-seed", &sequence.to_be_bytes());
    let identity = application_receipt_identity(&fixture.repository_scope, &request_id)
        .expect("history receipt identity");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload)));
    fixture
        .storage
        .commit(&StateCommit::new(
            identity,
            digest,
            stream_id,
            revision,
            payload,
            vec![internal_execution_event(&request_id)],
        ))
        .expect("seed compact Provider history");
}

fn install_chat_append_failure(fixture: &ProjectionFixture) -> Connection {
    let connection = Connection::open(fixture.storage.database_path()).expect("fixture database");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER fail_product_session_chat_append \
             BEFORE UPDATE ON product_state \
             WHEN OLD.stream_id = '{}' \
             BEGIN SELECT RAISE(ABORT, 'injected Chat append failure'); END;",
            fixture.catalog_stream_id()
        ))
        .expect("install Chat append failure");
    connection
}

#[test]
fn raw_provider_sequences_map_to_contiguous_public_ordinals_and_reject_changed_or_cross_exchange() {
    let mut fixture = ProjectionFixture::new("ordinal", 11);
    let first = fixture.chunk(3, "A");
    let second = fixture.chunk(5, "B");
    project_verified_product_session_chunks(&mut fixture.storage, &[first.clone(), second])
        .expect("project sparse canonical frames");
    assert_eq!(fixture.assistant_content(), "AB");
    assert_eq!(fixture.public_sequence(), 2);

    project_verified_product_session_chunks(&mut fixture.storage, std::slice::from_ref(&first))
        .expect("exact raw replay");
    assert_eq!(fixture.assistant_content(), "AB");
    assert_eq!(fixture.public_sequence(), 2);

    let mut changed = first;
    let payload_json =
        serde_json::json!({"type":"output_text_delta","delta":"changed"}).to_string();
    changed.payload = Some(EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(payload_json.as_bytes()),
        payload_digest: Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(payload_json.as_bytes())
        )),
    });
    assert!(
        project_verified_product_session_chunks(&mut fixture.storage, &[changed]).is_err(),
        "changed source body must conflict with the original receipt"
    );

    let mut cross_exchange = fixture.chunk(7, "cross");
    cross_exchange.model_exchange_id = ModelExchangeId(id("mdl", 999));
    assert!(
        project_verified_product_session_chunks(&mut fixture.storage, &[cross_exchange]).is_err(),
        "another exchange cannot write this Chat binding"
    );
    assert_eq!(fixture.assistant_content(), "AB");
    assert!(
        load_pending_provider_frames(&fixture.storage)
            .expect("pending catalog")
            .0
            .frames
            .is_empty()
    );
}

#[test]
fn whole_public_batch_is_durable_before_restart_projection_and_replays_in_public_order() {
    let mut fixture = ProjectionFixture::new("batch-restart", 13);
    let first_chunk = fixture.chunk(3, "A");
    let second_chunk = fixture.chunk(5, "B");
    let first = fixture.source(first_chunk);
    let second = fixture.source(second_chunk);
    let pending = persist_provider_batch_sources(&mut fixture.storage, &[first, second])
        .expect("durable public batch");
    assert_eq!(
        pending
            .iter()
            .map(|source| source.public_stream_sequence)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(fixture.assistant_content(), "");
    assert_eq!(
        load_pending_provider_frames(&fixture.storage)
            .expect("whole pending batch")
            .0
            .frames
            .len(),
        2
    );
    let directory = fixture.directory.0.clone();
    drop(fixture.storage);

    let mut reopened = SqliteStorage::open(&directory).expect("restart storage");
    reconcile_product_session_model_frames(&mut reopened).expect("ordered batch recovery");
    let receipt_scope =
        receipt_scope_key(&public_repository_scope(&repository_scope(13))).expect("scope");
    let record = ProductSessionService::new(&mut reopened)
        .get(&receipt_scope, &ProductSessionId(id("psn", 13)))
        .expect("ProductSession after batch restart")
        .expect("ProductSession");
    assert_eq!(
        record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant message")
            .content,
        "AB"
    );
}

#[test]
fn projected_history_does_not_block_one_pending_frame_restart_reconciliation() {
    let mut fixture = ProjectionFixture::new("history", 12);
    let first_chunk = fixture.chunk(3, "x");
    project_verified_product_session_chunks(
        &mut fixture.storage,
        std::slice::from_ref(&first_chunk),
    )
    .expect("project first historical public frame");
    let first_source = fixture.source(first_chunk);
    seed_projected_chat_sequence(&fixture, 4_097);
    seed_provider_history(&mut fixture, &first_source, 4_097);
    assert_eq!(fixture.public_sequence(), 4_097);
    assert_eq!(fixture.assistant_content().len(), 4_097);
    assert!(
        load_pending_provider_frames(&fixture.storage)
            .expect("empty pending catalog")
            .0
            .frames
            .is_empty()
    );

    let next_chunk = fixture.chunk(8_199, "z");
    let pending = fixture.pending_source(next_chunk);
    assert_eq!(pending.public_stream_sequence, 4_098);
    assert_eq!(
        load_pending_provider_frames(&fixture.storage)
            .expect("one pending frame")
            .0
            .frames
            .len(),
        1
    );
    let directory = fixture.directory.0.clone();
    drop(fixture.storage);

    let mut reopened = SqliteStorage::open(&directory).expect("restart storage");
    reconcile_product_session_model_frames(&mut reopened).expect("restart reconciliation");
    let scope = repository_scope(12);
    let receipt_scope = receipt_scope_key(&public_repository_scope(&scope)).expect("scope");
    let record = ProductSessionService::new(&mut reopened)
        .get(&receipt_scope, &ProductSessionId(id("psn", 12)))
        .expect("ProductSession after restart")
        .expect("ProductSession");
    let assistant = record
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .expect("assistant message");
    assert_eq!(assistant.content.len(), 4_098);
    assert!(assistant.content.ends_with('z'));
    assert!(
        load_pending_provider_frames(&reopened)
            .expect("reconciled pending catalog")
            .0
            .frames
            .is_empty()
    );
    reconcile_product_session_model_frames(&mut reopened).expect("exact restart replay");
}

#[test]
fn restart_reconciles_only_the_second_frame_after_first_frame_commits() {
    let mut fixture = ProjectionFixture::new("partial-append", 14);
    let first_chunk = fixture.chunk(3, "A");
    let second_chunk = fixture.chunk(5, "B");
    let first = fixture.source(first_chunk);
    let second = fixture.source(second_chunk);
    let mut pending = persist_provider_batch_sources(&mut fixture.storage, &[first, second])
        .expect("durable public batch");
    pending.sort_by(provider_projection_order);

    project_provider_source(&mut fixture.storage, &pending[0]).expect("first Chat append");
    assert_eq!(fixture.assistant_content(), "A");
    assert_eq!(
        load_pending_provider_frames(&fixture.storage)
            .expect("second frame pending")
            .0
            .frames
            .len(),
        1
    );

    let failure = install_chat_append_failure(&fixture);
    assert!(
        project_provider_source(&mut fixture.storage, &pending[1]).is_err(),
        "the injected Chat failure must leave the second frame pending"
    );
    failure
        .execute_batch("DROP TRIGGER fail_product_session_chat_append;")
        .expect("remove Chat append failure");
    assert_eq!(fixture.assistant_content(), "A");
    assert_eq!(
        load_pending_provider_frames(&fixture.storage)
            .expect("failed frame remains pending")
            .0
            .frames
            .len(),
        1
    );
    let directory = fixture.directory.0.clone();
    drop(fixture.storage);

    let mut reopened = SqliteStorage::open(&directory).expect("restart storage");
    reconcile_product_session_model_frames(&mut reopened).expect("recover second Chat append");
    let scope = repository_scope(14);
    let receipt_scope = receipt_scope_key(&public_repository_scope(&scope)).expect("scope");
    let record = ProductSessionService::new(&mut reopened)
        .get(&receipt_scope, &ProductSessionId(id("psn", 14)))
        .expect("ProductSession after partial recovery")
        .expect("ProductSession");
    assert_eq!(
        record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant message")
            .content,
        "AB"
    );
    assert!(
        load_pending_provider_frames(&reopened)
            .expect("partial recovery emptied pending catalog")
            .0
            .frames
            .is_empty()
    );
    reconcile_product_session_model_frames(&mut reopened).expect("exact partial replay");
}
