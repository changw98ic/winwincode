// SPDX-License-Identifier: Apache-2.0

//! Isolated, short-lived preview access over an outbound Device Client tunnel.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt as _, StreamExt as _};
use getrandom::fill;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{mpsc, oneshot};
use winwincode_client_port::managed_app::{ManagedAppMode, ManagedAppRunConfig};
use winwincode_client_port::preview::{
    DevicePreviewFrame, MAX_PREVIEW_BODY_BYTES, MAX_PREVIEW_HEADERS, PREVIEW_TUNNEL_SCHEMA_VERSION,
    PreviewHeader, PreviewSourceDescriptor, PreviewSourceMode, ServerPreviewFrame,
    is_canonical_git_commit,
};
use winwincode_control_plane::{
    ClientOccupancyService, ClientRegistryService, OccupancyLeaseState, ProductStateStorage,
    RepositoryBindingService,
};
use winwincode_delivery::domain::{Delivery, EvidenceRefType};
use winwincode_domain::{ExecutionJobId, WorkRunId, WorkRunState, is_canonical_prefixed_id};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionScope, ExecutionWorkspaceWriteMode,
};
use winwincode_storage::{ExecutionJobState, SqliteStorage};

const ACCESS_TTL: Duration = Duration::from_mins(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SOURCES_PER_CLIENT: usize = 16;
const MAX_PENDING_REQUESTS: usize = 64;

#[derive(Clone)]
pub(crate) struct PreviewApplication {
    data_directory: PathBuf,
    public_origin: String,
    authority: String,
    broker: Arc<Mutex<BrokerState>>,
}

impl fmt::Debug for PreviewApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewApplication")
            .field("public_origin", &self.public_origin)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct BrokerState {
    generation: u64,
    connections: HashMap<String, PreviewConnection>,
    accesses: HashMap<String, PreviewAccess>,
    pending: HashMap<String, PendingRequest>,
}

struct PreviewConnection {
    generation: u64,
    sources: BTreeMap<String, PreviewSourceDescriptor>,
    outbound: mpsc::Sender<ServerPreviewFrame>,
}

struct PreviewAccess {
    owner_user_id: String,
    client_node_id: String,
    source: PreviewSourceDescriptor,
    token_digest: [u8; 32],
    expires_at: std::time::Instant,
}

struct PendingRequest {
    client_node_id: String,
    generation: u64,
    completion: oneshot::Sender<Result<PreviewHttpResponse, PreviewError>>,
}

#[derive(Debug)]
pub(crate) struct PreviewHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewErrorKind {
    InvalidRequest,
    NotFound,
    Forbidden,
    Conflict,
    Timeout,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewError {
    kind: PreviewErrorKind,
    message: &'static str,
}

impl PreviewError {
    const fn new(kind: PreviewErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub(crate) const fn kind(&self) -> PreviewErrorKind {
        self.kind
    }

    pub(crate) const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for PreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PreviewError {}

impl PreviewApplication {
    pub(crate) fn open(data_directory: PathBuf, public_origin: String) -> Option<Self> {
        let authority = public_origin.split_once("://")?.1.to_ascii_lowercase();
        Some(Self {
            data_directory,
            public_origin,
            authority,
            broker: Arc::new(Mutex::new(BrokerState::default())),
        })
    }

    pub(crate) fn matches_origin(&self, headers: &HeaderMap) -> bool {
        headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.authority))
    }

    pub(crate) async fn serve_tunnel(&self, client_node_id: String, mut socket: WebSocket) {
        let registration = tokio::time::timeout(REGISTER_TIMEOUT, socket.next()).await;
        let Ok(Some(Ok(Message::Text(text)))) = registration else {
            let _ = socket.close().await;
            return;
        };
        let Ok(DevicePreviewFrame::Register {
            schema_version,
            sources,
        }) = serde_json::from_str::<DevicePreviewFrame>(&text)
        else {
            let _ = socket.close().await;
            return;
        };
        if schema_version != PREVIEW_TUNNEL_SCHEMA_VERSION {
            let _ = socket.close().await;
            return;
        }
        let Ok(sources) = validate_sources(sources) else {
            let _ = socket.close().await;
            return;
        };
        let (outbound, mut requests) = mpsc::channel(MAX_PENDING_REQUESTS);
        let generation = self.register_connection(&client_node_id, sources, outbound);
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    let Ok(text) = serde_json::to_string(&request) else { break };
                    if socket.send(Message::Text(text.into())).await.is_err() { break }
                }
                message = socket.next() => {
                    let Some(Ok(message)) = message else { break };
                    match message {
                        Message::Text(text) => {
                            let Ok(frame) = serde_json::from_str::<DevicePreviewFrame>(&text) else { break };
                            if !self.apply_device_frame(&client_node_id, generation, frame) { break }
                        }
                        Message::Ping(bytes) => {
                            if socket.send(Message::Pong(bytes)).await.is_err() { break }
                        }
                        Message::Close(_) => break,
                        Message::Binary(_) | Message::Pong(_) => {}
                    }
                }
            }
        }
        self.unregister_connection(&client_node_id, generation);
    }

    pub(crate) fn grant(&self, user_id: &str, request: &Value) -> Result<Value, PreviewError> {
        let fields = request.as_object().ok_or_else(invalid_request)?;
        if fields.len() != 3
            || fields.get("schemaVersion").and_then(Value::as_str) != Some("winwincode/v1")
        {
            return Err(invalid_request());
        }
        let client_id = fields
            .get("clientId")
            .and_then(Value::as_str)
            .filter(|value| portable_identifier(value))
            .ok_or_else(invalid_request)?;
        let source_id = fields
            .get("sourceId")
            .and_then(Value::as_str)
            .filter(|value| portable_identifier(value))
            .ok_or_else(invalid_request)?;
        let client_node_id = self.authorize_holder(user_id, client_id)?;
        let source = {
            let state = self.lock_broker()?;
            state
                .connections
                .get(&client_node_id)
                .and_then(|connection| connection.sources.get(source_id))
                .cloned()
                .ok_or_else(|| {
                    PreviewError::new(
                        PreviewErrorKind::Conflict,
                        "preview source is not connected",
                    )
                })?
        };
        self.authorize_source(user_id, client_id, &client_node_id, &source)?;
        let access_id = random_hex("pva_", 16)?;
        let token = random_hex("", 32)?;
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let expires_at = std::time::Instant::now() + ACCESS_TTL;
        self.lock_broker()?.accesses.insert(
            access_id.clone(),
            PreviewAccess {
                owner_user_id: user_id.to_owned(),
                client_node_id,
                source: source.clone(),
                token_digest: digest,
                expires_at,
            },
        );
        let expires_at_text = (OffsetDateTime::now_utc()
            + time::Duration::seconds(i64::try_from(ACCESS_TTL.as_secs()).unwrap_or(300)))
        .format(&Rfc3339)
        .map_err(|_| unavailable())?;
        Ok(json!({
            "schemaVersion": "winwincode/v1",
            "previewAccessId": access_id,
            "previewUrl": format!("{}/p/{}/{}/", self.public_origin, access_id, token),
            "expiresAt": expires_at_text,
            "source": source,
        }))
    }

    pub(crate) fn revoke(&self, user_id: &str, access_id: &str) -> Result<(), PreviewError> {
        if !portable_identifier(access_id) {
            return Err(invalid_request());
        }
        let mut state = self.lock_broker()?;
        let access = state.accesses.get(access_id).ok_or_else(not_found)?;
        if access.owner_user_id != user_id {
            return Err(PreviewError::new(
                PreviewErrorKind::Forbidden,
                "preview access belongs to another user",
            ));
        }
        state.accesses.remove(access_id);
        Ok(())
    }

    pub(crate) async fn forward(
        &self,
        access_id: &str,
        token: &str,
        method: &Method,
        target: &str,
        request_headers: &HeaderMap,
        body: &[u8],
    ) -> Result<PreviewHttpResponse, PreviewError> {
        if body.len() > MAX_PREVIEW_BODY_BYTES || !allowed_method(method) || !safe_target(target) {
            return Err(invalid_request());
        }
        let token_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let (client_node_id, generation, source_id, outbound) = {
            let mut state = self.lock_broker()?;
            state
                .accesses
                .retain(|_, access| access.expires_at > std::time::Instant::now());
            let access = state.accesses.get(access_id).ok_or_else(not_found)?;
            if !constant_time_eq(&access.token_digest, &token_digest) {
                return Err(not_found());
            }
            let connection = state
                .connections
                .get(&access.client_node_id)
                .ok_or_else(source_unavailable)?;
            if connection.sources.get(&access.source.source_id) != Some(&access.source) {
                return Err(PreviewError::new(
                    PreviewErrorKind::Conflict,
                    "preview source identity changed",
                ));
            }
            (
                access.client_node_id.clone(),
                connection.generation,
                access.source.source_id.clone(),
                connection.outbound.clone(),
            )
        };
        let request_id = random_hex("pvr_", 16)?;
        let headers = filtered_headers(request_headers)?;
        let (completion, receiver) = oneshot::channel();
        {
            let mut state = self.lock_broker()?;
            if state
                .connections
                .get(&client_node_id)
                .is_none_or(|connection| connection.generation != generation)
                || state.pending.len() >= MAX_PENDING_REQUESTS
            {
                return Err(source_unavailable());
            }
            state.pending.insert(
                request_id.clone(),
                PendingRequest {
                    client_node_id,
                    generation,
                    completion,
                },
            );
        }
        let frame = ServerPreviewFrame::HttpRequest {
            request_id: request_id.clone(),
            source_id,
            method: method.as_str().to_owned(),
            target: target.to_owned(),
            headers,
            body_base64: BASE64.encode(body),
        };
        if outbound.send(frame).await.is_err() {
            self.remove_pending(&request_id);
            return Err(source_unavailable());
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(source_unavailable()),
            Err(_) => {
                self.remove_pending(&request_id);
                Err(PreviewError::new(
                    PreviewErrorKind::Timeout,
                    "preview source response timed out",
                ))
            }
        }
    }

    fn register_connection(
        &self,
        client_node_id: &str,
        sources: BTreeMap<String, PreviewSourceDescriptor>,
        outbound: mpsc::Sender<ServerPreviewFrame>,
    ) -> u64 {
        let mut state = self
            .broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        let stale_completions = state
            .pending
            .extract_if(|_, request| request.client_node_id == client_node_id)
            .map(|(_, request)| request.completion)
            .collect::<Vec<_>>();
        state.connections.insert(
            client_node_id.to_owned(),
            PreviewConnection {
                generation,
                sources,
                outbound,
            },
        );
        drop(state);
        for completion in stale_completions {
            let _ = completion.send(Err(source_unavailable()));
        }
        generation
    }

    fn unregister_connection(&self, client_node_id: &str, generation: u64) {
        let mut state = self
            .broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .connections
            .get(client_node_id)
            .is_some_and(|connection| connection.generation == generation)
        {
            state.connections.remove(client_node_id);
            let pending = state
                .pending
                .extract_if(|_, request| request.client_node_id == client_node_id)
                .map(|(_, request)| request.completion)
                .collect::<Vec<_>>();
            drop(state);
            for completion in pending {
                let _ = completion.send(Err(source_unavailable()));
            }
        }
    }

    fn apply_device_frame(
        &self,
        client_node_id: &str,
        generation: u64,
        frame: DevicePreviewFrame,
    ) -> bool {
        let DevicePreviewFrame::HttpResponse {
            request_id,
            status,
            headers,
            body_base64,
        } = frame
        else {
            return false;
        };
        let completion = {
            let Ok(mut state) = self.lock_broker() else {
                return false;
            };
            let belongs = state.pending.get(&request_id).is_some_and(|request| {
                request.client_node_id == client_node_id && request.generation == generation
            });
            if !belongs {
                return true;
            }
            state
                .pending
                .remove(&request_id)
                .map(|request| request.completion)
        };
        let Some(completion) = completion else {
            return true;
        };
        let response = decode_response(status, headers, &body_base64);
        let _ = completion.send(response);
        true
    }

    fn remove_pending(&self, request_id: &str) {
        if let Ok(mut state) = self.lock_broker() {
            state.pending.remove(request_id);
        }
    }

    fn authorize_holder(
        &self,
        user_id: &str,
        public_client_id: &str,
    ) -> Result<String, PreviewError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| unavailable())?;
        let node = ClientRegistryService::new(&mut storage)
            .snapshot_by_public_client_id(public_client_id)
            .map_err(|_| unavailable())?
            .ok_or_else(not_found)?;
        let lease = ClientOccupancyService::new(&mut storage)
            .active_lease_for_node(&node.client_node_id)
            .map_err(|_| unavailable())?
            .ok_or_else(|| {
                PreviewError::new(
                    PreviewErrorKind::Conflict,
                    "client occupancy is required for preview access",
                )
            })?;
        let result = if lease.holder_user_id != user_id {
            Err(PreviewError::new(
                PreviewErrorKind::Forbidden,
                "only the occupancy holder may open preview access",
            ))
        } else if !matches!(
            lease.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            Err(PreviewError::new(
                PreviewErrorKind::Conflict,
                "client occupancy is not confirmed",
            ))
        } else {
            Ok(node.client_node_id)
        };
        Box::new(storage).close().map_err(|_| unavailable())?;
        result
    }

    #[allow(clippy::too_many_lines)]
    fn authorize_source(
        &self,
        user_id: &str,
        public_client_id: &str,
        client_node_id: &str,
        source: &PreviewSourceDescriptor,
    ) -> Result<(), PreviewError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| unavailable())?;
        let visible = RepositoryBindingService::new(&mut storage)
            .visible_bindings(user_id, client_node_id)
            .map_err(|_| unavailable())?;
        if !visible
            .iter()
            .any(|binding| binding.repository_binding_id == source.repository_binding_id)
        {
            return Err(PreviewError::new(
                PreviewErrorKind::Forbidden,
                "preview repository is not visible to this user",
            ));
        }

        if !is_canonical_prefixed_id(&source.source_id, "job_")
            || !is_canonical_prefixed_id(&source.work_run_id, "wrn_")
            || !is_canonical_prefixed_id(&source.repository_binding_id, "rbd_")
        {
            return Err(source_authority_conflict());
        }
        let work_run_id = WorkRunId(source.work_run_id.clone());
        let source_job_id = ExecutionJobId(source.source_id.clone());
        let Some(job) = (match source.mode {
            PreviewSourceMode::Live => storage
                .load_active_execution_job_record_for_work_run(&work_run_id)
                .map_err(|_| unavailable())?,
            PreviewSourceMode::FrozenCandidate => storage
                .load_execution_job_record(&source_job_id)
                .map_err(|_| unavailable())?,
        }) else {
            return Err(source_authority_conflict());
        };
        if job.job_id != source_job_id
            || job.work_run_id.as_ref() != Some(&work_run_id)
            || (matches!(source.mode, PreviewSourceMode::FrozenCandidate)
                && job.state != ExecutionJobState::Completed)
        {
            return Err(source_authority_conflict());
        }
        let Some(facts) = storage
            .load_work_run_device_binding_facts(&job.job_id)
            .map_err(|_| unavailable())?
        else {
            return Err(source_authority_conflict());
        };
        let Some(config_record) = storage
            .load_managed_app_run_config_for_work_run(&source.work_run_id)
            .map_err(|_| unavailable())?
        else {
            return Err(source_authority_conflict());
        };
        let config: ManagedAppRunConfig =
            serde_json::from_slice(&config_record.config_json).map_err(|_| unavailable())?;
        config.validate().map_err(|_| unavailable())?;
        if config_record.repository_binding_id != source.repository_binding_id
            || config_record.work_run_id != source.work_run_id
            || config_record.attempt != i64::from(config.attempt)
        {
            return Err(source_authority_conflict());
        }
        let dispatch: ExecutionJob =
            serde_json::from_slice(&job.dispatch_payload).map_err(|_| unavailable())?;
        if dispatch.job_id != job.job_id
            || dispatch.attempt != i64::try_from(job.attempt).unwrap_or(-1)
        {
            return Err(source_authority_conflict());
        }
        let ExecutionScope::WorkRunExecutionScope(scope) = &dispatch.scope else {
            return Err(source_authority_conflict());
        };
        let Some(work_input) = dispatch.work_input.as_ref() else {
            return Err(source_authority_conflict());
        };
        let Some(device_target) = work_input.device_target.as_ref() else {
            return Err(source_authority_conflict());
        };
        if scope.work_run_id != work_run_id
            || device_target.client_id != public_client_id
            || device_target.client_node_id != client_node_id
            || device_target.repository_binding_id != source.repository_binding_id
        {
            return Err(source_authority_conflict());
        }
        if matches!(source.mode, PreviewSourceMode::FrozenCandidate) {
            authorize_current_frozen_candidate(
                &storage,
                &job,
                &work_run_id,
                source,
                &config,
                &dispatch,
            )?;
        }
        validate_source_authority(
            source,
            public_client_id,
            &facts.public_client_id,
            &facts.repository_binding_id,
            facts.work_run_id.as_deref(),
            job.work_run_id.as_ref().map(|id| id.0.as_str()),
            &job.job_id.0,
            &config,
            job.attempt,
        )
    }

    fn lock_broker(&self) -> Result<std::sync::MutexGuard<'_, BrokerState>, PreviewError> {
        self.broker.lock().map_err(|_| unavailable())
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn authorize_current_frozen_candidate(
    storage: &SqliteStorage,
    job: &winwincode_storage::ExecutionJobRecord,
    work_run_id: &WorkRunId,
    source: &PreviewSourceDescriptor,
    config: &ManagedAppRunConfig,
    dispatch: &ExecutionJob,
) -> Result<(), PreviewError> {
    let Some(delivery_id) = job.scope.delivery_id.as_ref() else {
        return Err(source_authority_conflict());
    };
    if !is_canonical_prefixed_id(&delivery_id.0, "dlv_") {
        return Err(source_authority_conflict());
    }
    let Some(state) = storage
        .load_state(&format!("delivery:{}", delivery_id.0))
        .map_err(|_| unavailable())?
    else {
        return Err(source_authority_conflict());
    };
    let delivery = Delivery::decode_json(&state.payload).map_err(|_| unavailable())?;
    if delivery.id() != delivery_id || delivery.revision() != state.revision {
        return Err(source_authority_conflict());
    }
    let Some(run) = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| &run.id == work_run_id)
    else {
        return Err(source_authority_conflict());
    };
    let writers = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            matches!(
                binding.execution_profile.as_deref(),
                Some("executor" | "remediator")
            )
        });
    let Some(current_key) = writers
        .clone()
        .map(|binding| (binding.bound_at_millis, binding.attempt))
        .max()
    else {
        return Err(source_authority_conflict());
    };
    let current = writers
        .filter(|binding| (binding.bound_at_millis, binding.attempt) == current_key)
        .collect::<Vec<_>>();
    if current.len() != 1 {
        return Err(source_authority_conflict());
    }
    let source_bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.work_run_id == *work_run_id
                && binding.execution_job_id == job.job_id
                && matches!(
                    binding.execution_profile.as_deref(),
                    Some("reviewer" | "verifier" | "adversarial-verifier")
                )
        })
        .collect::<Vec<_>>();
    if source_bindings.len() != 1 {
        return Err(source_authority_conflict());
    }
    if validate_frozen_candidate_source(
        job.state,
        &run.state,
        &run.execution_job_id,
        run.attempt,
        &job.job_id,
        job.attempt,
        &dispatch.workspace.write_mode,
    )
    .is_err()
    {
        return Err(source_authority_conflict());
    }
    let writer = current[0];
    let commits = delivery
        .snapshot()
        .evidence
        .iter()
        .filter(|evidence| {
            evidence.delivery_id == *delivery.id()
                && evidence.delivery_spec_id == delivery.snapshot().spec.id
                && evidence.delivery_spec_revision == delivery.snapshot().spec.revision
                && evidence.work_run_id == writer.work_run_id
                && evidence.session_binding_id == writer.id
                && evidence.evidence_type == EvidenceRefType::Commit
        })
        .collect::<Vec<_>>();
    let commit_ids = commits
        .iter()
        .filter_map(|evidence| evidence.source_ref.strip_prefix("git_commit:"))
        .collect::<Vec<_>>();
    validate_frozen_candidate_commit(
        &delivery.snapshot().spec.base_revision,
        source.candidate_commit.as_deref(),
        config.candidate_commit.as_deref(),
        dispatch.workspace.checkout_revision.as_str(),
        dispatch
            .work_input
            .as_ref()
            .and_then(|input| input.candidate_ref.as_deref()),
        commits
            .first()
            .map(|evidence| evidence.candidate_ref.as_str()),
        commits.len(),
        &commit_ids,
    )
}

fn validate_frozen_candidate_source(
    job_state: ExecutionJobState,
    run_state: &WorkRunState,
    run_job_id: &ExecutionJobId,
    run_attempt: i64,
    job_id: &ExecutionJobId,
    job_attempt: u64,
    write_mode: &ExecutionWorkspaceWriteMode,
) -> Result<(), PreviewError> {
    if job_state != ExecutionJobState::Completed
        || !matches!(
            run_state,
            WorkRunState::CandidateReady | WorkRunState::Settled
        )
        || run_job_id != job_id
        || run_attempt != i64::try_from(job_attempt).unwrap_or(-1)
        || *write_mode != ExecutionWorkspaceWriteMode::ReadOnly
    {
        return Err(source_authority_conflict());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_frozen_candidate_commit(
    base_revision: &str,
    source_candidate: Option<&str>,
    config_candidate: Option<&str>,
    checkout_revision: &str,
    dispatch_candidate_ref: Option<&str>,
    evidence_candidate_ref: Option<&str>,
    commit_evidence_count: usize,
    commit_evidence: &[&str],
) -> Result<(), PreviewError> {
    if commit_evidence_count != 1
        || commit_evidence.len() != 1
        || !is_canonical_git_commit(commit_evidence[0])
        || commit_evidence[0] == base_revision
        || source_candidate != Some(commit_evidence[0])
        || config_candidate != Some(commit_evidence[0])
        || checkout_revision != commit_evidence[0]
        || dispatch_candidate_ref.is_none()
        || dispatch_candidate_ref != evidence_candidate_ref
    {
        return Err(source_authority_conflict());
    }
    Ok(())
}

fn validate_sources(
    sources: Vec<PreviewSourceDescriptor>,
) -> Result<BTreeMap<String, PreviewSourceDescriptor>, PreviewError> {
    if sources.is_empty() || sources.len() > MAX_SOURCES_PER_CLIENT {
        return Err(invalid_request());
    }
    let mut result = BTreeMap::new();
    for source in sources {
        if !is_canonical_prefixed_id(&source.source_id, "job_")
            || !is_canonical_prefixed_id(&source.work_run_id, "wrn_")
            || !is_canonical_prefixed_id(&source.repository_binding_id, "rbd_")
        {
            return Err(invalid_request());
        }
        match (source.mode, source.candidate_commit.as_deref()) {
            (PreviewSourceMode::Live, None) => {}
            (PreviewSourceMode::FrozenCandidate, Some(commit))
                if is_canonical_git_commit(commit) => {}
            _ => return Err(invalid_request()),
        }
        if result.insert(source.source_id.clone(), source).is_some() {
            return Err(invalid_request());
        }
    }
    Ok(result)
}

fn filtered_headers(headers: &HeaderMap) -> Result<Vec<PreviewHeader>, PreviewError> {
    let mut result = Vec::new();
    for (name, value) in headers {
        if safe_header_name(name) {
            let value = value.to_str().map_err(|_| invalid_request())?;
            result.push(PreviewHeader {
                name: name.as_str().to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if result.len() > MAX_PREVIEW_HEADERS {
        return Err(invalid_request());
    }
    Ok(result)
}

fn decode_response(
    status: u16,
    headers: Vec<PreviewHeader>,
    body_base64: &str,
) -> Result<PreviewHttpResponse, PreviewError> {
    let status = StatusCode::from_u16(status).map_err(|_| source_unavailable())?;
    if headers.len() > MAX_PREVIEW_HEADERS {
        return Err(source_unavailable());
    }
    let mut safe_headers = HeaderMap::new();
    for header in headers {
        let name =
            HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| source_unavailable())?;
        let value = HeaderValue::from_str(&header.value).map_err(|_| source_unavailable())?;
        if name == http::header::LOCATION && !safe_redirect(&header.value) {
            return Err(source_unavailable());
        }
        if safe_header_name(&name) {
            safe_headers.append(name, value);
        }
    }
    let body = BASE64
        .decode(body_base64)
        .map_err(|_| source_unavailable())?;
    if body.len() > MAX_PREVIEW_BODY_BYTES {
        return Err(source_unavailable());
    }
    Ok(PreviewHttpResponse {
        status,
        headers: safe_headers,
        body,
    })
}

fn safe_header_name(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization"
            | "cookie"
            | "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn allowed_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
    )
}

fn safe_target(target: &str) -> bool {
    target.starts_with('/')
        && !target.starts_with("//")
        && target.len() <= 8_192
        && !target.bytes().any(|byte| byte.is_ascii_control())
}

fn safe_redirect(location: &str) -> bool {
    let first_segment = location.split(['/', '?', '#']).next().unwrap_or_default();
    !location.is_empty()
        && !location.starts_with("//")
        && !location.contains(['\\', '\r', '\n'])
        && !first_segment.contains(':')
}

fn portable_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

#[allow(clippy::too_many_arguments)]
fn validate_source_authority(
    source: &PreviewSourceDescriptor,
    public_client_id: &str,
    facts_public_client_id: &str,
    facts_repository_binding_id: &str,
    facts_work_run_id: Option<&str>,
    job_work_run_id: Option<&str>,
    job_id: &str,
    config: &ManagedAppRunConfig,
    job_attempt: u64,
) -> Result<(), PreviewError> {
    if !is_canonical_prefixed_id(&source.source_id, "job_")
        || !is_canonical_prefixed_id(&source.work_run_id, "wrn_")
        || !is_canonical_prefixed_id(&source.repository_binding_id, "rbd_")
    {
        return Err(source_authority_conflict());
    }
    let mode_matches = matches!(
        (config.mode, source.mode),
        (ManagedAppMode::Live, PreviewSourceMode::Live)
            | (
                ManagedAppMode::FrozenCandidate,
                PreviewSourceMode::FrozenCandidate
            )
    );
    if facts_public_client_id != public_client_id
        || facts_repository_binding_id != source.repository_binding_id
        || facts_work_run_id != Some(source.work_run_id.as_str())
        || job_work_run_id != Some(source.work_run_id.as_str())
        || config.repository_binding_id != source.repository_binding_id
        || config.run_id != source.work_run_id
        || config.source_id != source.source_id
        || config.source_id != job_id
        || u32::try_from(job_attempt).ok() != Some(config.attempt)
        || !mode_matches
        || config.candidate_commit != source.candidate_commit
    {
        return Err(source_authority_conflict());
    }
    Ok(())
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn random_hex(prefix: &str, bytes: usize) -> Result<String, PreviewError> {
    let mut random = vec![0_u8; bytes];
    fill(&mut random).map_err(|_| unavailable())?;
    let mut value = String::with_capacity(prefix.len() + bytes * 2);
    value.push_str(prefix);
    for byte in random {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").map_err(|_| unavailable())?;
    }
    Ok(value)
}

const fn invalid_request() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::InvalidRequest,
        "preview request is invalid",
    )
}

const fn not_found() -> PreviewError {
    PreviewError::new(PreviewErrorKind::NotFound, "preview access was not found")
}

const fn source_unavailable() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::Unavailable,
        "preview source is unavailable",
    )
}

const fn unavailable() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::Unavailable,
        "preview service is unavailable",
    )
}

const fn source_authority_conflict() -> PreviewError {
    PreviewError::new(
        PreviewErrorKind::Conflict,
        "preview source authority is not active",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, work_run: &str) -> PreviewSourceDescriptor {
        PreviewSourceDescriptor {
            source_id: id.to_owned(),
            work_run_id: work_run.to_owned(),
            repository_binding_id: "rbd_00000000000000000000000001".to_owned(),
            mode: PreviewSourceMode::Live,
            candidate_commit: None,
        }
    }

    fn managed_config(source: &PreviewSourceDescriptor) -> ManagedAppRunConfig {
        ManagedAppRunConfig {
            schema_version:
                winwincode_client_port::managed_app::MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION
                    .to_owned(),
            run_id: source.work_run_id.clone(),
            repository_binding_id: source.repository_binding_id.clone(),
            template_revision: 1,
            attempt: 1,
            mode: ManagedAppMode::Live,
            candidate_commit: None,
            cwd: "app".to_owned(),
            argv: vec!["bin".to_owned()],
            env: BTreeMap::new(),
            health_check: winwincode_client_port::managed_app::ManagedAppHealthCheck {
                path: "/health".to_owned(),
                timeout_ms: 5_000,
            },
            listen_port: 3_000,
            source_id: source.source_id.clone(),
        }
    }

    #[test]
    fn preview_source_must_match_device_and_managed_app_authority() {
        let source = source(
            "job_00000000000000000000000001",
            "wrn_00000000000000000000000001",
        );
        let config = managed_config(&source);
        assert!(
            validate_source_authority(
                &source,
                "927351842",
                "927351842",
                "rbd_00000000000000000000000001",
                Some("wrn_00000000000000000000000001"),
                Some("wrn_00000000000000000000000001"),
                "job_00000000000000000000000001",
                &config,
                1,
            )
            .is_ok()
        );

        let mut mismatched = source.clone();
        mismatched.repository_binding_id = "rbd_00000000000000000000000002".to_owned();
        assert_eq!(
            validate_source_authority(
                &mismatched,
                "927351842",
                "927351842",
                "rbd_00000000000000000000000001",
                Some("wrn_00000000000000000000000001"),
                Some("wrn_00000000000000000000000001"),
                "job_00000000000000000000000001",
                &config,
                1,
            )
            .unwrap_err()
            .kind(),
            PreviewErrorKind::Conflict
        );

        let mut mismatched = source;
        mismatched.source_id = "job_00000000000000000000000002".to_owned();
        assert!(
            validate_source_authority(
                &mismatched,
                "927351842",
                "927351842",
                "rbd_00000000000000000000000001",
                Some("wrn_00000000000000000000000001"),
                Some("wrn_00000000000000000000000001"),
                "job_00000000000000000000000001",
                &config,
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_frozen_candidate_requires_current_commit_evidence() {
        let job_id = ExecutionJobId("job_00000000000000000000000001".to_owned());
        let commit = "a".repeat(40);
        assert!(
            validate_frozen_candidate_source(
                ExecutionJobState::Completed,
                &WorkRunState::Settled,
                &job_id,
                2,
                &job_id,
                2,
                &ExecutionWorkspaceWriteMode::ReadOnly,
            )
            .is_ok()
        );
        assert!(
            validate_frozen_candidate_commit(
                &"b".repeat(40),
                Some(&commit),
                Some(&commit),
                &commit,
                Some("git-candidate:current"),
                Some("git-candidate:current"),
                1,
                &[commit.as_str()],
            )
            .is_ok()
        );

        let old_commit = "c".repeat(40);
        assert!(
            validate_frozen_candidate_commit(
                &"b".repeat(40),
                Some(&old_commit),
                Some(&old_commit),
                &old_commit,
                Some("git-candidate:old"),
                Some("git-candidate:current"),
                1,
                &[commit.as_str()],
            )
            .is_err()
        );
        assert!(
            validate_frozen_candidate_commit(
                &"b".repeat(40),
                Some(&commit),
                Some(&commit),
                &commit,
                Some("git-candidate:current"),
                Some("git-candidate:current"),
                2,
                &[commit.as_str()],
            )
            .is_err()
        );
    }

    /// Frozen preview authorize succeeds only when the Delivery carries exactly
    /// one executor Commit Evidence sealed from the real frozen candidate.
    #[test]
    fn authorize_current_frozen_candidate_succeeds_with_sealed_commit_evidence() {
        use winwincode_delivery::domain::{DELIVERY_SCHEMA_VERSION, Delivery};
        use winwincode_domain::Sha256Digest;
        use winwincode_execution_port::generated::ExecutionJob;
        use winwincode_storage::{
            NewOutboxEvent, ProductStateStorage as _, ReceiptActorKey, ReceiptIdentity,
            ReceiptScopeKey, StateCommit,
        };

        fn canonical(prefix: &str, seed: &str) -> String {
            let digest = Sha256::digest(seed.as_bytes());
            let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
            let mut suffix = [b'0'; 26];
            let mut value = u128::from_be_bytes(digest[..16].try_into().expect("sha256"));
            for slot in suffix.iter_mut().rev() {
                *slot = alphabet[(value & 31) as usize];
                value >>= 5;
            }
            format!("{prefix}{}", String::from_utf8_lossy(&suffix))
        }

        let root = std::env::temp_dir().join(format!(
            "winwincode-preview-authorize-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp storage");

        let delivery_id = canonical("dlv_", "preview-authorize-delivery");
        let executor_job = canonical("job_", "preview-authorize-executor-job");
        let verifier_job = canonical("job_", "preview-authorize-verifier-job");
        let executor_run = canonical("wrn_", "preview-authorize-executor-run");
        let verifier_run = canonical("wrn_", "preview-authorize-verifier-run");
        let executor_binding = canonical("bnd_", "preview-authorize-executor-binding");
        let verifier_binding = canonical("bnd_", "preview-authorize-verifier-binding");
        let base_revision = "b".repeat(40);
        let candidate_commit = "a".repeat(40);
        let candidate_ref =
            "git-candidate:sha256:a71f11375d06a904ac3ed9faeffe28cddcc754e663b2f6b77029a84de6743e26";
        let spec_id = canonical("dsp_", "preview-authorize-spec");
        let evidence_id = canonical("evd_", "preview-authorize-commit-evidence");

        let delivery_json = serde_json::json!({
            "schemaVersion": DELIVERY_SCHEMA_VERSION,
            "id": delivery_id,
            "revision": 1,
            "status": "ready",
            "spec": {
                "schemaVersion": DELIVERY_SCHEMA_VERSION,
                "id": spec_id,
                "deliveryId": delivery_id,
                "revision": 1,
                "title": "frozen preview authorize",
                "goal": "authorize frozen preview with Commit evidence",
                "scope": ["TASK.md"],
                "outOfScope": [],
                "constraints": [],
                "acceptanceCriteria": [{
                    "schemaVersion": DELIVERY_SCHEMA_VERSION,
                    "id": canonical("crt_", "preview-authorize-criterion"),
                    "description": "npm run verify passes",
                    "required": true,
                    "verificationMethod": "npm run verify"
                }],
                "sourceProductSessionId": canonical("psn_", "preview-authorize-source"),
                "sourceRef": null,
                "publicationTarget": null,
                "repository": {
                    "schemaVersion": DELIVERY_SCHEMA_VERSION,
                    "kind": "local-git",
                    "locator": "devices/preview-authorize"
                },
                "baseRevision": base_revision,
                "maxReworkAttempts": 3,
                "createdAtMillis": 1_800_000_000_000_u64
            },
            "sessionBindings": [
                {
                    "schemaVersion": DELIVERY_SCHEMA_VERSION,
                    "id": executor_binding,
                    "deliveryId": delivery_id,
                    "workContractId": canonical("wct_", "preview-authorize-contract"),
                    "workContractRevision": 1,
                    "workItemId": canonical("wit_", "preview-authorize-item"),
                    "workItemRevision": 1,
                    "workRunId": executor_run,
                    "productSessionId": canonical("psn_", "preview-authorize-executor-psn"),
                    "executionJobId": executor_job,
                    "executionProfile": "executor",
                    "runtimeContext": {
                        "agentIdentity": {
                            "id": canonical("agt_", "preview-authorize-executor-agt"),
                            "workerId": canonical("wrk_", "preview-authorize-executor-wrk"),
                            "name": "Executor",
                            "role": "executor"
                        },
                        "provider": "fixture-provider",
                        "model": "fixture-model",
                        "workspace": {
                            "repositoryId": canonical("rep_", "preview-authorize"),
                            "revision": format!("git-tree:{}", "c".repeat(64)),
                            "writeMode": "candidate"
                        }
                    },
                    "workerSessionId": canonical("wsn_", "preview-authorize-executor-wsn"),
                    "codexThreadId": canonical("cdx_", "preview-authorize-executor-cdx"),
                    "boundAtMillis": 1_800_000_000_011_u64,
                    "attempt": 1,
                    "workerId": canonical("wrk_", "preview-authorize-executor-wrk"),
                    "workerInstanceId": canonical("wki_", "preview-authorize-executor-wki"),
                    "leaseId": canonical("lse_", "preview-authorize-executor-lse"),
                    "fencingToken": "1",
                    "sourceProvenance": {
                        "kind": "execution-port",
                        "reference": canonical("msg_", "preview-authorize-executor-msg")
                    }
                },
                {
                    "schemaVersion": DELIVERY_SCHEMA_VERSION,
                    "id": verifier_binding,
                    "deliveryId": delivery_id,
                    "workContractId": canonical("wct_", "preview-authorize-contract"),
                    "workContractRevision": 1,
                    "workItemId": canonical("wit_", "preview-authorize-item"),
                    "workItemRevision": 1,
                    "workRunId": verifier_run,
                    "productSessionId": canonical("psn_", "preview-authorize-verifier-psn"),
                    "executionJobId": verifier_job,
                    "executionProfile": "verifier",
                    "runtimeContext": {
                        "agentIdentity": {
                            "id": canonical("agt_", "preview-authorize-verifier-agt"),
                            "workerId": canonical("wrk_", "preview-authorize-verifier-wrk"),
                            "name": "Verifier",
                            "role": "verifier"
                        },
                        "provider": "fixture-provider",
                        "model": "fixture-model",
                        "workspace": {
                            "repositoryId": canonical("rep_", "preview-authorize"),
                            "revision": format!("git-tree:{}", "d".repeat(64)),
                            "writeMode": "read-only"
                        }
                    },
                    "workerSessionId": canonical("wsn_", "preview-authorize-verifier-wsn"),
                    "codexThreadId": canonical("cdx_", "preview-authorize-verifier-cdx"),
                    "boundAtMillis": 1_800_000_000_031_u64,
                    "attempt": 1,
                    "workerId": canonical("wrk_", "preview-authorize-verifier-wrk"),
                    "workerInstanceId": canonical("wki_", "preview-authorize-verifier-wki"),
                    "leaseId": canonical("lse_", "preview-authorize-verifier-lse"),
                    "fencingToken": "2",
                    "sourceProvenance": {
                        "kind": "execution-port",
                        "reference": canonical("msg_", "preview-authorize-verifier-msg")
                    }
                }
            ],
            "attentionItems": [],
            "evidence": [{
                "schemaVersion": DELIVERY_SCHEMA_VERSION,
                "id": evidence_id,
                "deliveryId": delivery_id,
                "deliverySpecId": spec_id,
                "deliverySpecRevision": 1,
                "workRunId": executor_run,
                "sessionBindingId": executor_binding,
                "candidateRef": candidate_ref,
                "type": "commit",
                "sourceRef": format!("git_commit:{candidate_commit}"),
                "createdAtMillis": 1_800_000_000_060_u64
            }],
            "verdict": null,
            "createdAtMillis": 1_800_000_000_000_u64,
            "updatedAtMillis": 1_800_000_000_070_u64,
            "workRunAggregate": {
                "schemaVersion": "winwincode/v1",
                "contract": {
                    "schemaVersion": "winwincode/v1",
                    "id": canonical("wct_", "preview-authorize-contract"),
                    "revision": 1,
                    "objective": "frozen preview",
                    "scope": ["TASK.md"],
                    "protectedScope": [],
                    "constraints": [],
                    "criteria": [{
                        "id": canonical("crt_", "preview-authorize-criterion"),
                        "description": "npm run verify passes",
                        "required": true,
                        "requiredEvidenceClass": "machine",
                        "verificationMethod": "npm run verify"
                    }],
                    "requiredHumanAuthority": "none",
                    "createdAt": "2027-01-15T08:00:00.000Z"
                },
                "items": [{
                    "schemaVersion": "winwincode/v1",
                    "id": canonical("wit_", "preview-authorize-item"),
                    "workContractId": canonical("wct_", "preview-authorize-contract"),
                    "workContractRevision": 1,
                    "title": "frozen preview",
                    "goal": "authorize frozen preview",
                    "state": "done",
                    "revision": 1,
                    "criterionIds": [canonical("crt_", "preview-authorize-criterion")],
                    "dependsOn": []
                }],
                "runs": [
                    {
                        "schemaVersion": "winwincode/v1",
                        "id": executor_run,
                        "workContractId": canonical("wct_", "preview-authorize-contract"),
                        "contractRevision": 1,
                        "workItemId": canonical("wit_", "preview-authorize-item"),
                        "workItemRevision": 1,
                        "revision": 1,
                        "state": "settled",
                        "executionJobId": executor_job,
                        "attempt": 1,
                        "workerId": canonical("wrk_", "preview-authorize-executor-wrk"),
                        "workerInstanceId": canonical("wki_", "preview-authorize-executor-wki"),
                        "workerSessionId": canonical("wsn_", "preview-authorize-executor-wsn"),
                        "leaseId": canonical("lse_", "preview-authorize-executor-lse"),
                        "fencingToken": "1",
                        "productSessionId": canonical("psn_", "preview-authorize-executor-psn"),
                        "codexThreadId": canonical("cdx_", "preview-authorize-executor-cdx")
                    },
                    {
                        "schemaVersion": "winwincode/v1",
                        "id": verifier_run,
                        "workContractId": canonical("wct_", "preview-authorize-contract"),
                        "contractRevision": 1,
                        "workItemId": canonical("wit_", "preview-authorize-item"),
                        "workItemRevision": 1,
                        "revision": 1,
                        "state": "settled",
                        "executionJobId": verifier_job,
                        "attempt": 1,
                        "workerId": canonical("wrk_", "preview-authorize-verifier-wrk"),
                        "workerInstanceId": canonical("wki_", "preview-authorize-verifier-wki"),
                        "workerSessionId": canonical("wsn_", "preview-authorize-verifier-wsn"),
                        "leaseId": canonical("lse_", "preview-authorize-verifier-lse"),
                        "fencingToken": "2",
                        "productSessionId": canonical("psn_", "preview-authorize-verifier-psn"),
                        "codexThreadId": canonical("cdx_", "preview-authorize-verifier-cdx")
                    }
                ]
            }
        });
        let delivery_bytes = serde_json::to_vec(&delivery_json).expect("delivery JSON");
        let delivery = Delivery::decode_json(&delivery_bytes)
            .expect("canonical frozen Delivery with Commit evidence");
        assert_eq!(delivery.snapshot().evidence.len(), 1);
        assert_eq!(
            delivery.snapshot().evidence[0].evidence_type,
            EvidenceRefType::Commit
        );

        let mut storage = SqliteStorage::open(&root).expect("preview storage");
        let scope = ReceiptScopeKey::from_encoded(b"preview-authorize-fixture".to_vec())
            .expect("receipt scope");
        storage
            .commit(&StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"preview-authorize-actor".to_vec())
                        .expect("actor"),
                    scope,
                    winwincode_domain::RequestId(canonical("req_", "preview-authorize")),
                )
                .expect("receipt identity"),
                Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"preview-authorize"))),
                format!("delivery:{delivery_id}"),
                0,
                delivery.encode_json().expect("Delivery payload"),
                vec![NewOutboxEvent::internal(
                    "preview-authorize-seed",
                    "fixture.seed.internal",
                    b"{}".to_vec(),
                )],
            ))
            .expect("seed Delivery state");

        let job = winwincode_storage::ExecutionJobRecord {
            scope: winwincode_storage::ExecutionQueueScope {
                organization_id: winwincode_domain::OrganizationId(canonical(
                    "org_",
                    "preview-authorize",
                )),
                workspace_id: winwincode_domain::WorkspaceId(canonical(
                    "wsp_",
                    "preview-authorize",
                )),
                project_id: winwincode_domain::ProjectId(canonical("prj_", "preview-authorize")),
                repository_id: winwincode_domain::RepositoryId(canonical(
                    "rep_",
                    "preview-authorize",
                )),
                product_session_id: winwincode_domain::ProductSessionId(canonical(
                    "psn_",
                    "preview-authorize-verifier-psn",
                )),
                delivery_id: Some(winwincode_domain::DeliveryId(delivery_id.clone())),
            },
            job_id: ExecutionJobId(verifier_job.clone()),
            submission_request_id: winwincode_domain::RequestId(canonical(
                "req_",
                "preview-authorize-job",
            )),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"job"))),
            dispatch_payload: Vec::new(),
            state: ExecutionJobState::Completed,
            attempt: 1,
            revision: 1,
            dependencies: Vec::new(),
            work_run_id: Some(WorkRunId(verifier_run.clone())),
            submitted_at: winwincode_domain::Instant("2027-01-15T08:00:00.000Z".into()),
            updated_at: winwincode_domain::Instant("2027-01-15T08:01:00.000Z".into()),
            cancellation: None,
        };

        let mut source = source(&verifier_job, &verifier_run);
        source.mode = PreviewSourceMode::FrozenCandidate;
        source.candidate_commit = Some(candidate_commit.clone());
        source.repository_binding_id = canonical("rbd_", "preview-authorize");

        let mut config = managed_config(&source);
        config.mode = ManagedAppMode::FrozenCandidate;
        config.candidate_commit = Some(candidate_commit.clone());

        let dispatch: ExecutionJob = serde_json::from_value(serde_json::json!({
            "attempt": 1,
            "executionProfile": "verifier",
            "goal": "verify frozen candidate",
            "jobId": verifier_job,
            "limits": {
                "deadlineAt": "2027-01-15T08:05:00.000Z",
                "maxArtifactBytes": 1_000_000,
                "maxRuntimeSeconds": 600
            },
            "payloadDigest": format!("sha256:{:x}", Sha256::digest(b"dispatch")),
            "scope": {
                "attempt": 1,
                "kind": "work-run",
                "productSessionId": canonical("psn_", "preview-authorize-verifier-psn"),
                "reworkAuthorization": null,
                "workContractId": canonical("wct_", "preview-authorize-contract"),
                "workContractRevision": 1,
                "workItemId": canonical("wit_", "preview-authorize-item"),
                "workItemRevision": 1,
                "workRunId": verifier_run
            },
            "workInput": {
                "candidateRef": candidate_ref,
                "deliverySpecId": spec_id,
                "deliverySpecRevision": 1,
                "schemaVersion": "winwincode/v1",
                "workContract": {
                    "schemaVersion": "winwincode/v1",
                    "id": canonical("wct_", "preview-authorize-contract"),
                    "revision": 1,
                    "objective": "frozen preview",
                    "scope": ["TASK.md"],
                    "protectedScope": [],
                    "constraints": [],
                    "criteria": [{
                        "id": canonical("crt_", "preview-authorize-criterion"),
                        "description": "npm run verify passes",
                        "required": true,
                        "requiredEvidenceClass": "command",
                        "verificationMethod": "npm run verify"
                    }],
                    "requiredHumanAuthority": "none",
                    "createdAt": "2027-01-15T08:00:00.000Z"
                },
                "workItem": {
                    "schemaVersion": "winwincode/v1",
                    "id": canonical("wit_", "preview-authorize-item"),
                    "workContractId": canonical("wct_", "preview-authorize-contract"),
                    "workContractRevision": 1,
                    "title": "frozen preview",
                    "goal": "authorize frozen preview",
                    "state": "done",
                    "revision": 1,
                    "criterionIds": [canonical("crt_", "preview-authorize-criterion")],
                    "dependsOn": []
                }
            },
            "workspace": {
                "checkoutRevision": candidate_commit,
                "repositoryId": canonical("rep_", "preview-authorize"),
                "writeMode": "read-only"
            }
        }))
        .expect("frozen verifier ExecutionJob");

        authorize_current_frozen_candidate(
            &storage,
            &job,
            &WorkRunId(verifier_run.clone()),
            &source,
            &config,
            &dispatch,
        )
        .expect("frozen preview authorize accepts sealed executor Commit evidence");

        // Fail closed when the sealed Commit evidence is absent.
        let mut missing = delivery.clone().into_snapshot();
        missing.evidence.clear();
        missing.revision = 2;
        missing.updated_at_millis = 1_800_000_000_080;
        let missing = Delivery::try_from_snapshot(missing).expect("Delivery without Commit");
        storage
            .commit(&StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"preview-authorize-actor-2".to_vec())
                        .expect("actor"),
                    ReceiptScopeKey::from_encoded(b"preview-authorize-fixture-2".to_vec())
                        .expect("scope"),
                    winwincode_domain::RequestId(canonical("req_", "preview-authorize-missing")),
                )
                .expect("receipt identity"),
                Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(b"preview-authorize-missing")
                )),
                format!("delivery:{delivery_id}"),
                1,
                missing.encode_json().expect("missing Commit payload"),
                vec![NewOutboxEvent::internal(
                    "preview-authorize-missing",
                    "fixture.seed.internal",
                    b"{}".to_vec(),
                )],
            ))
            .expect("overwrite Delivery without Commit evidence");
        let error = authorize_current_frozen_candidate(
            &storage,
            &job,
            &WorkRunId(verifier_run),
            &source,
            &config,
            &dispatch,
        )
        .expect_err("frozen preview authorize fails closed without Commit evidence");
        assert_eq!(error.kind(), PreviewErrorKind::Conflict);

        drop(storage);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn live_preview_does_not_accept_frozen_candidate_facts() {
        let source = source(
            "job_00000000000000000000000001",
            "wrn_00000000000000000000000001",
        );
        let mut config = managed_config(&source);
        config.mode = ManagedAppMode::FrozenCandidate;
        config.candidate_commit = Some("a".repeat(40));
        assert!(
            validate_source_authority(
                &source,
                "927351842",
                "927351842",
                &source.repository_binding_id,
                Some(&source.work_run_id),
                Some(&source.work_run_id),
                &source.source_id,
                &config,
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn preview_registration_uses_work_run_identity() {
        let value = serde_json::json!({
            "sourceId": "pvs_demo",
            "workRunId": "wrn_demo",
            "repositoryBindingId": "rbd_demo",
            "mode": "live"
        });
        let descriptor: PreviewSourceDescriptor = serde_json::from_value(value).expect("parses");
        assert_eq!(descriptor.work_run_id, "wrn_demo");
        let legacy = serde_json::json!({
            "sourceId": "pvs_demo",
            "workerSessionId": "wsn_demo",
            "repositoryBindingId": "rbd_demo",
            "mode": "live"
        });
        assert!(serde_json::from_value::<PreviewSourceDescriptor>(legacy).is_err());
    }

    #[test]
    fn reconnect_changes_source_identity_and_invalidates_old_access() {
        let application = PreviewApplication::open(
            PathBuf::from("unused"),
            "https://preview.example".to_owned(),
        )
        .unwrap();
        let (sender, _receiver) = mpsc::channel(1);
        let first = source("pvs_demo", "wrn_old");
        application.register_connection(
            "cnd_demo",
            BTreeMap::from([(first.source_id.clone(), first.clone())]),
            sender,
        );
        application.broker.lock().unwrap().accesses.insert(
            "pva_demo".to_owned(),
            PreviewAccess {
                owner_user_id: "usr_demo".to_owned(),
                client_node_id: "cnd_demo".to_owned(),
                source: first,
                token_digest: Sha256::digest(b"token").into(),
                expires_at: std::time::Instant::now() + ACCESS_TTL,
            },
        );
        let (sender, _receiver) = mpsc::channel(1);
        let replacement = source("pvs_demo", "wrn_new");
        application.register_connection(
            "cnd_demo",
            BTreeMap::from([(replacement.source_id.clone(), replacement)]),
            sender,
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(application.forward(
                "pva_demo",
                "token",
                &Method::GET,
                "/",
                &HeaderMap::new(),
                &[],
            ))
            .unwrap_err();
        assert_eq!(error.kind(), PreviewErrorKind::Conflict);
    }

    #[test]
    fn cloud_metadata_and_hop_by_hop_inputs_are_rejected_or_stripped() {
        assert!(!safe_target("//169.254.169.254/latest/meta-data"));
        assert!(!allowed_method(&Method::CONNECT));
        assert!(!safe_redirect("http://169.254.169.254/latest/meta-data"));
        assert!(safe_redirect("/sign-in"));
        let mut headers = HeaderMap::new();
        headers.insert(http::header::COOKIE, HeaderValue::from_static("secret=x"));
        headers.insert("x-preview-test", HeaderValue::from_static("ok"));
        assert_eq!(filtered_headers(&headers).unwrap().len(), 1);
    }

    #[test]
    fn frozen_candidate_requires_a_full_lowercase_sha1_or_sha256_object_id() {
        for length in [40, 64] {
            let mut candidate = source(
                "job_00000000000000000000000002",
                "wrn_00000000000000000000000001",
            );
            candidate.mode = PreviewSourceMode::FrozenCandidate;
            candidate.candidate_commit = Some("a".repeat(length));
            assert!(validate_sources(vec![candidate]).is_ok());
        }
        for commit in [
            "a".repeat(41),
            "a".repeat(63),
            "A".repeat(40),
            format!("{}g", "a".repeat(39)),
        ] {
            let mut candidate = source(
                "job_00000000000000000000000002",
                "wrn_00000000000000000000000001",
            );
            candidate.mode = PreviewSourceMode::FrozenCandidate;
            candidate.candidate_commit = Some(commit);
            assert!(validate_sources(vec![candidate]).is_err());
        }
    }

    #[test]
    fn backend_relays_only_the_access_bound_source_and_revocation_is_immediate() {
        let application = PreviewApplication::open(
            PathBuf::from("unused"),
            "https://preview.example".to_owned(),
        )
        .unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let source = source("pvs_demo", "wrn_demo");
        let generation = application.register_connection(
            "cnd_demo",
            BTreeMap::from([(source.source_id.clone(), source.clone())]),
            sender,
        );
        application.broker.lock().unwrap().accesses.insert(
            "pva_demo".to_owned(),
            PreviewAccess {
                owner_user_id: "usr_demo".to_owned(),
                client_node_id: "cnd_demo".to_owned(),
                source,
                token_digest: Sha256::digest(b"token").into(),
                expires_at: std::time::Instant::now() + ACCESS_TTL,
            },
        );
        let relay = application.clone();
        let responder = application.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let request = tokio::spawn(async move {
                relay
                    .forward(
                        "pva_demo",
                        "token",
                        &Method::GET,
                        "/index.html",
                        &HeaderMap::new(),
                        &[],
                    )
                    .await
            });
            let frame = receiver.recv().await.unwrap();
            let ServerPreviewFrame::HttpRequest {
                request_id,
                source_id,
                target,
                ..
            } = frame;
            assert_eq!(source_id, "pvs_demo");
            assert_eq!(target, "/index.html");
            assert!(responder.apply_device_frame(
                "cnd_demo",
                generation,
                DevicePreviewFrame::HttpResponse {
                    request_id,
                    status: 200,
                    headers: vec![PreviewHeader {
                        name: "content-type".to_owned(),
                        value: "text/html".to_owned(),
                    }],
                    body_base64: BASE64.encode("preview ok"),
                },
            ));
            let response = request.await.unwrap().unwrap();
            assert_eq!(response.status, StatusCode::OK);
            assert_eq!(response.body, b"preview ok");
        });
        application.revoke("usr_demo", "pva_demo").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(application.forward(
                "pva_demo",
                "token",
                &Method::GET,
                "/",
                &HeaderMap::new(),
                &[],
            ))
            .unwrap_err();
        assert_eq!(error.kind(), PreviewErrorKind::NotFound);
    }
}
