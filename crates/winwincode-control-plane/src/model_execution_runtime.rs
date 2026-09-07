// SPDX-License-Identifier: Apache-2.0

//! Unique Worker `ModelPort` runtime over the durable Provider Gateway.
//!
//! Local and remote execution-port adapters invoke the same typed core. The
//! runtime writes an opening tombstone before any Credential or Provider side
//! effect, persists only secret-free authority and retry context, restores an
//! accepted exchange without reopening the Provider, and joins all chunks and
//! acknowledgements through the one flow-control coordinator.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource, ProjectScope, Scope,
    SystemActor, SystemActorKind,
};
use winwincode_domain::{
    ExecutionMessageId, ExecutionSequence, Instant, ModelExchangeId, RequestId, Sha256Digest,
    SystemActorId,
};
use winwincode_execution_port::{
    generated::{
        EncodedPayload, ExecutionPortMessage, ModelAckMessage, ModelChunkMessage,
        ModelChunkMessageKind, ModelOpenMessage,
    },
    transport::ExecutionPortCore,
};
use winwincode_storage::{
    EnterpriseQuotaReleaseReason, ProductStateStorage, ProviderExchangeBegin,
    ProviderExchangeFailure, ProviderExchangeFinalAck, ProviderExchangeOpened,
    ProviderExchangeSnapshot, ProviderExchangeState, ProviderExchangeStoreError,
    ProviderExchangeStoreErrorCode, ProviderExchangeTerminal, ProviderExchangeTerminalProgress,
    ProviderExchangeTerminalStage, SqliteStorage, StateCommit,
};

use crate::{
    CanonicalModelStreamFrame, DurableProviderPolicyEnforcement, EnterpriseQuotaAdmissionPort,
    FrozenModelRouteAuthority, ModelAttemptFailureFact, ModelExecutionCertainty,
    ModelFrameAckReceipt, ModelFrameWriteReceipt, ModelFrameWriteStatus, ModelRequestAdmission,
    ModelRequestAdmissionReceipt, ModelRequestAdmissionStatus, ModelRequestPool,
    ModelRequestPoolConfig, ModelRequestState, ModelRequestTerminalOutcome, ModelRetryPlannerError,
    ModelRetryPlannerErrorKind, ModelRetryPreOpenPlannerPort, ModelRetrySettlementContext,
    ModelRetrySettlementContextPort, ModelStreamFlowAckReceipt, ModelStreamFlowCancellationReceipt,
    ModelStreamFlowCoordinator, ModelStreamFlowError, ModelStreamFlowWriteReceipt,
    ModelStreamReadControl, ProviderAdmissionOpenReceipt, ProviderEnterpriseQuotaSaga,
    ProviderGateway, ProviderGatewayDurableExchange, ProviderGatewayError,
    ProviderGatewayErrorKind, ProviderGatewayOpenReceipt, ProviderGatewayTerminal,
    ProviderGatewayTerminalOutcome, ProviderGatewayTerminalProgress,
    ProviderGatewayTerminalProgressPort, ProviderGatewayTerminalProgressStage,
    ProviderGatewayTerminalReceipt, ProviderPolicyErrorKind, command_receipt_identity,
    model_route_availability::{
        model_request_pool_readiness_stream_id, model_route_availability_invalidated_event,
    },
};

const FAILURE_INTERRUPTED: &str = "provider_acceptance_unknown";
const FAILURE_GATEWAY_PREFIX: &str = "gateway_";
const POOL_READINESS_STATE_SCHEMA: &str = "winwincode.model-request-pool-readiness.v1";
const POOL_READINESS_SYSTEM_ACTOR: &str = "sys_00000000000000000000000002";

fn pool_submit_operation(
    model_exchange_id: &ModelExchangeId,
    open_digest: &Sha256Digest,
) -> String {
    format!("submit\0{}\0{}", model_exchange_id.0, open_digest.0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolReadinessState<'scope> {
    schema: &'static str,
    scope: &'scope ProjectScope,
}

/// Stable production runtime failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelExecutionRuntimeErrorKind {
    InvalidInput,
    UnsupportedMessage,
    ExchangeConflict,
    OpenInterrupted,
    OpenFailed,
    Planning,
    ContextUnavailable,
    ContextCorrupt,
    Gateway,
    Flow,
    Storage,
}

/// Bounded error that never retains model payload or Provider diagnostics.
#[derive(Debug)]
pub struct ModelExecutionRuntimeError {
    kind: ModelExecutionRuntimeErrorKind,
}

impl ModelExecutionRuntimeError {
    const fn new(kind: ModelExecutionRuntimeErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> ModelExecutionRuntimeErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelExecutionRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("durable model execution runtime operation failed")
    }
}

impl std::error::Error for ModelExecutionRuntimeError {}

impl From<ProviderExchangeStoreError> for ModelExecutionRuntimeError {
    fn from(error: ProviderExchangeStoreError) -> Self {
        let kind = match error.code() {
            ProviderExchangeStoreErrorCode::InvalidInput => {
                ModelExecutionRuntimeErrorKind::InvalidInput
            }
            ProviderExchangeStoreErrorCode::Conflict => {
                ModelExecutionRuntimeErrorKind::ExchangeConflict
            }
            ProviderExchangeStoreErrorCode::InvalidState
            | ProviderExchangeStoreErrorCode::NotFound
            | ProviderExchangeStoreErrorCode::Storage => ModelExecutionRuntimeErrorKind::Storage,
        };
        Self::new(kind)
    }
}

impl From<ProviderGatewayError> for ModelExecutionRuntimeError {
    fn from(_error: ProviderGatewayError) -> Self {
        Self::new(ModelExecutionRuntimeErrorKind::Gateway)
    }
}

impl From<ModelStreamFlowError> for ModelExecutionRuntimeError {
    fn from(_error: ModelStreamFlowError) -> Self {
        Self::new(ModelExecutionRuntimeErrorKind::Flow)
    }
}

/// Durable exchange store and canonical retry-settlement context source.
pub struct DurableModelExchangeAuthority {
    storage: Mutex<SqliteStorage>,
    database_path: PathBuf,
}

impl DurableModelExchangeAuthority {
    /// Opens the internal exchange authority over the product database.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the database or restricted table cannot be opened.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, ModelExecutionRuntimeError> {
        let mut storage = SqliteStorage::open(data_directory).map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
        })?;
        storage.provider_exchange_store()?;
        let database_path = storage.database_path().to_path_buf();
        Ok(Self {
            storage: Mutex::new(storage),
            database_path,
        })
    }

    /// Returns the exact canonical database path for composition checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Deterministically checkpoints and closes the owned database connection.
    ///
    /// # Errors
    ///
    /// Returns a stable storage failure if the mutex is poisoned or close fails.
    pub fn close(self) -> Result<(), ModelExecutionRuntimeError> {
        let storage = self.storage.into_inner().map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
        })?;
        Box::new(storage)
            .close()
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))
    }

    fn begin(
        &self,
        request: &ProviderExchangeBegin,
    ) -> Result<ProviderExchangeSnapshot, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .begin_open(request)
            .map_err(Into::into)
    }

    fn begin_with_pool_authority(
        &self,
        request: &ProviderExchangeBegin,
        pool_authority_json: &[u8],
        scope: &ProjectScope,
    ) -> Result<ProviderExchangeSnapshot, ModelExecutionRuntimeError> {
        let operation = pool_submit_operation(&request.model_exchange_id, &request.open_digest);
        let readiness = self.pool_readiness_commit(
            scope,
            &operation,
            pool_authority_json,
            &request.started_at,
        )?;
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .begin_open_with_pool_authority(request, pool_authority_json, &readiness)
            .map_err(Into::into)
    }

    fn opened(
        &self,
        model_exchange_id: &ModelExchangeId,
        open_digest: &Sha256Digest,
        opened: &ProviderExchangeOpened,
    ) -> Result<ProviderExchangeSnapshot, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .commit_opened(model_exchange_id, open_digest, opened)
            .map_err(Into::into)
    }

    fn failed_with_pool_authority(
        &self,
        model_exchange_id: &ModelExchangeId,
        open_digest: &Sha256Digest,
        failure: &ProviderExchangeFailure,
        pool_authority_json: &[u8],
        scope: &ProjectScope,
    ) -> Result<ProviderExchangeSnapshot, ModelExecutionRuntimeError> {
        let operation = format!(
            "failed\0{}\0{}\0{}",
            model_exchange_id.0, open_digest.0, failure.failure_kind
        );
        let readiness =
            self.pool_readiness_commit(scope, &operation, pool_authority_json, &failure.failed_at)?;
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .commit_failed_with_pool_authority(
                model_exchange_id,
                open_digest,
                failure,
                pool_authority_json,
                &readiness,
            )
            .map_err(Into::into)
    }

    fn terminal_with_pool_authority(
        &self,
        model_exchange_id: &ModelExchangeId,
        terminal: &ProviderExchangeTerminal,
        pool_authority_json: &[u8],
        scope: &ProjectScope,
    ) -> Result<ProviderExchangeSnapshot, ModelExecutionRuntimeError> {
        let operation = format!(
            "terminal\0{}\0{}",
            model_exchange_id.0, terminal.terminal_digest.0
        );
        let readiness = self.pool_readiness_commit(
            scope,
            &operation,
            pool_authority_json,
            &terminal.settled_at,
        )?;
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .commit_terminal_with_pool_authority(
                model_exchange_id,
                terminal,
                pool_authority_json,
                &readiness,
            )
            .map_err(Into::into)
    }

    fn terminal_progress(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeTerminalProgress>, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .load_terminal_progress(model_exchange_id)
            .map_err(Into::into)
    }

    fn load_pool_authority(&self) -> Result<Option<Vec<u8>>, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .load_pool_authority()
            .map(|authority| authority.map(|authority| authority.state_json().to_vec()))
            .map_err(Into::into)
    }

    fn save_pool_authority(
        &self,
        bytes: &[u8],
        updated_at: &Instant,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .save_pool_authority(bytes, updated_at)
            .map(|_| ())
            .map_err(Into::into)
    }

    fn save_pool_authority_with_readiness(
        &self,
        bytes: &[u8],
        updated_at: &Instant,
        scope: &ProjectScope,
        operation: &str,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let readiness = self.pool_readiness_commit(scope, operation, bytes, updated_at)?;
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .save_pool_authority_with_readiness(bytes, updated_at, &readiness)
            .map(|_| ())
            .map_err(Into::into)
    }

    fn final_ack(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeFinalAck>, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .load_final_ack(model_exchange_id)
            .map_err(Into::into)
    }

    fn commit_final_ack(
        &self,
        acknowledgement: &ModelAckMessage,
        ack_digest: &Sha256Digest,
        receipt_json: &[u8],
        pool_authority_json: &[u8],
        scope: &ProjectScope,
    ) -> Result<ProviderExchangeFinalAck, ModelExecutionRuntimeError> {
        let operation = format!(
            "final-ack\0{}\0{}",
            acknowledgement.model_exchange_id.0, ack_digest.0
        );
        let readiness = self.pool_readiness_commit(
            scope,
            &operation,
            pool_authority_json,
            &acknowledgement.sent_at,
        )?;
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .commit_final_ack_with_pool_authority(
                &acknowledgement.model_exchange_id,
                ack_digest,
                acknowledgement.ack_sequence.0,
                receipt_json,
                pool_authority_json,
                &acknowledgement.sent_at,
                &readiness,
            )
            .map_err(Into::into)
    }

    fn snapshot(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderExchangeSnapshot>, ModelExecutionRuntimeError> {
        let mut storage = self.lock()?;
        storage
            .provider_exchange_store()?
            .load(model_exchange_id)
            .map_err(Into::into)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SqliteStorage>, ModelExecutionRuntimeError> {
        self.storage
            .lock()
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))
    }

    fn pool_readiness_commit(
        &self,
        project_scope: &ProjectScope,
        operation: &str,
        authority_json: &[u8],
        occurred_at: &Instant,
    ) -> Result<StateCommit, ModelExecutionRuntimeError> {
        let scope = Scope::ProjectScope(project_scope.clone());
        let stream_id = model_request_pool_readiness_stream_id(project_scope).map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
        })?;
        let expected_revision = self
            .lock()?
            .load_state(&stream_id)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?
            .map_or(0, |stored| stored.revision);
        let revision = expected_revision.checked_add(1).ok_or_else(|| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
        })?;
        let actor = Actor::SystemActor(SystemActor {
            id: SystemActorId(POOL_READINESS_SYSTEM_ACTOR.to_owned()),
            kind: SystemActorKind::System,
        });
        let mut request_digest = Sha256::new();
        request_digest.update(b"winwincode.model-request-pool-readiness-request.v1\0");
        request_digest.update(operation.as_bytes());
        let request_hex = format!("{:X}", request_digest.finalize());
        let request_id = RequestId(format!("req_{}", &request_hex[..26]));
        let identity = command_receipt_identity(&actor, &scope, request_id).map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
        })?;
        let mut command_digest = Sha256::new();
        command_digest.update(b"winwincode.model-request-pool-readiness-command.v1\0");
        command_digest.update(operation.as_bytes());
        command_digest.update([0]);
        command_digest.update(authority_json);
        let command_digest = Sha256Digest(format!("sha256:{:x}", command_digest.finalize()));
        let event = model_route_availability_invalidated_event(
            &actor,
            &scope,
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::RequestPool,
            revision,
            occurred_at.clone(),
            operation.as_bytes(),
        )
        .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?;
        let state = serde_json::to_vec(&PoolReadinessState {
            schema: POOL_READINESS_STATE_SCHEMA,
            scope: project_scope,
        })
        .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?;
        Ok(StateCommit::new(
            identity,
            command_digest,
            stream_id,
            expected_revision,
            state,
            vec![event],
        ))
    }
}

impl ProviderGatewayTerminalProgressPort for DurableModelExchangeAuthority {
    fn load(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ProviderGatewayTerminalProgress>, ProviderGatewayError> {
        let progress = self
            .terminal_progress(model_exchange_id)
            .map_err(|_| ProviderGatewayError::storage())?;
        progress
            .map(|progress| {
                let admission = progress
                    .admission_receipt_json()
                    .map(serde_json::from_slice)
                    .transpose()
                    .map_err(|_| ProviderGatewayError::storage())?;
                let terminal = progress
                    .terminal_receipt_json()
                    .map(decode_terminal_receipt)
                    .transpose()
                    .map_err(|_| ProviderGatewayError::storage())?;
                Ok(ProviderGatewayTerminalProgress {
                    stage: gateway_terminal_stage(progress.stage),
                    admission,
                    terminal,
                })
            })
            .transpose()
    }

    fn record(
        &self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        stage: ProviderGatewayTerminalProgressStage,
        admission: Option<&crate::ModelReservationTerminalReceipt>,
        terminal: Option<&ProviderGatewayTerminalReceipt>,
        observed_at: &Instant,
    ) -> Result<(), ProviderGatewayError> {
        let digest = terminal_digest(command).map_err(|_| ProviderGatewayError::storage())?;
        let stage = storage_terminal_stage(stage);
        let admission_json = admission
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| ProviderGatewayError::storage())?;
        let terminal_json = terminal
            .map(terminal_receipt_json)
            .transpose()
            .map_err(|_| ProviderGatewayError::storage())?;
        let mut storage = self
            .storage
            .lock()
            .map_err(|_| ProviderGatewayError::storage())?;
        storage
            .provider_exchange_store()
            .and_then(|mut store| {
                store.record_terminal_progress(
                    model_exchange_id,
                    &digest,
                    stage,
                    admission_json.as_deref(),
                    terminal_json.as_deref(),
                    observed_at,
                )
            })
            .map(|_| ())
            .map_err(|error| terminal_progress_error(&error))
    }
}

fn terminal_progress_error(error: &ProviderExchangeStoreError) -> ProviderGatewayError {
    match error.code() {
        ProviderExchangeStoreErrorCode::InvalidInput => ProviderGatewayError::invalid(),
        ProviderExchangeStoreErrorCode::Conflict | ProviderExchangeStoreErrorCode::InvalidState => {
            ProviderGatewayError::terminal_conflict()
        }
        ProviderExchangeStoreErrorCode::NotFound => ProviderGatewayError::exchange_not_found(),
        ProviderExchangeStoreErrorCode::Storage => ProviderGatewayError::storage(),
    }
}

impl fmt::Debug for DurableModelExchangeAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableModelExchangeAuthority")
            .field("database_path", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Result of one `ModelOpen` through the durable production path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelExecutionOpenReceipt {
    Queued {
        pool: ModelRequestAdmissionReceipt,
        context_fingerprint: String,
    },
    Opened {
        gateway: ProviderGatewayOpenReceipt,
        pool: ModelRequestAdmissionReceipt,
        context_fingerprint: String,
        idempotent_replay: bool,
    },
}

/// Result of one Provider batch and the generated Worker chunks.
#[derive(Debug)]
pub struct ModelExecutionBatchReceipt {
    pub chunks: Vec<ModelChunkMessage>,
    pub flow: ModelStreamFlowWriteReceipt,
    authority_sha256: Sha256Digest,
}

impl ModelExecutionBatchReceipt {
    pub(crate) fn verified_chunks(&self) -> Result<&[ModelChunkMessage], ()> {
        let encoded = serde_json::to_vec(&self.chunks).map_err(|_| ())?;
        let observed = Sha256Digest(format!("sha256:{:x}", Sha256::digest(encoded)));
        (observed == self.authority_sha256)
            .then_some(self.chunks.as_slice())
            .ok_or(())
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn corrupt_chunks_for_test(&mut self) {
        if let Some(chunk) = self.chunks.first_mut() {
            chunk.sent_at.0.push_str("-tampered");
        }
    }
}

/// Rebuilds terminal Worker batches which are durable in the Provider pool but
/// have not necessarily crossed the `ProductSession` projection boundary yet.
///
/// # Errors
///
/// Fails closed when the pool authority, exchange authority, or frozen Worker
/// binding disagree.
pub(crate) fn recover_terminal_model_execution_batches(
    storage: &mut SqliteStorage,
    config: ModelRequestPoolConfig,
) -> Result<Vec<ModelExecutionBatchReceipt>, ModelExecutionRuntimeError> {
    let pool_authority = storage.provider_exchange_store()?.load_pool_authority()?;
    let Some(pool_authority) = pool_authority else {
        return Ok(Vec::new());
    };
    let mut pool = ModelRequestPool::new(config).map_err(|_| runtime_flow_error())?;
    pool.restore_authority(pool_authority.state_json())
        .map_err(|_| runtime_flow_error())?;
    let mut batches = Vec::new();
    for model_exchange_id in pool.buffered_exchange_ids() {
        let pool_snapshot = pool
            .reconnect(&model_exchange_id)
            .map_err(|_| runtime_flow_error())?;
        if !matches!(
            pool_snapshot.state,
            ModelRequestState::Succeeded | ModelRequestState::Failed
        ) {
            continue;
        }
        let exchange = storage
            .provider_exchange_store()?
            .load(&model_exchange_id)?
            .ok_or_else(runtime_context_corrupt)?;
        if exchange.state != ProviderExchangeState::Terminal {
            return Err(runtime_context_corrupt());
        }
        let durable = durable_gateway_exchange(&exchange)?;
        let gateway_terminal = exchange
            .terminal_receipt_json()
            .map(decode_terminal_receipt)
            .transpose()?
            .ok_or_else(runtime_context_corrupt)?;
        if pool_snapshot.request_id != durable.open_receipt().request_id
            || &pool_snapshot.route != durable.route_authority().route_key()
            || !terminal_outcomes_match(pool_snapshot.terminal_outcome, gateway_terminal.outcome)
        {
            return Err(runtime_context_corrupt());
        }
        let frames = pool
            .read_buffered(
                &model_exchange_id,
                0,
                config.max_buffered_frames_per_stream,
                config.max_buffered_bytes_per_stream,
            )
            .map_err(|_| runtime_flow_error())?;
        if frames.len() != pool_snapshot.buffered_frames
            || frames
                .iter()
                .map(|frame| frame.payload().len())
                .sum::<usize>()
                != pool_snapshot.buffered_bytes
        {
            return Err(runtime_context_corrupt());
        }
        let chunks = model_chunks_from_pool_frames(&durable, &frames, &exchange.updated_at)?;
        let flow = ModelStreamFlowWriteReceipt {
            pool: ModelFrameWriteReceipt {
                status: ModelFrameWriteStatus::Duplicate,
                state: pool_snapshot.state,
                highest_sequence: pool_snapshot.next_sequence.saturating_sub(1),
                buffered_frames: pool_snapshot.buffered_frames,
                buffered_bytes: pool_snapshot.buffered_bytes,
                read_control: pool_snapshot.read_control,
                granted_exchange_id: None,
            },
            provider_control: None,
            gateway_terminal: Some(gateway_terminal),
        };
        batches.push(seal_batch_receipt(chunks, flow)?);
    }
    Ok(batches)
}

const fn runtime_flow_error() -> ModelExecutionRuntimeError {
    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
}

const fn runtime_context_corrupt() -> ModelExecutionRuntimeError {
    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
}

const fn terminal_outcomes_match(
    pool: Option<ModelRequestTerminalOutcome>,
    gateway: ProviderGatewayTerminalOutcome,
) -> bool {
    matches!(
        (pool, gateway),
        (
            Some(ModelRequestTerminalOutcome::Succeeded),
            ProviderGatewayTerminalOutcome::Succeeded
        ) | (
            Some(ModelRequestTerminalOutcome::Failed),
            ProviderGatewayTerminalOutcome::Failed
        ) | (
            Some(ModelRequestTerminalOutcome::Cancelled),
            ProviderGatewayTerminalOutcome::Cancelled
        )
    )
}

/// Result of one Worker acknowledgement.
#[derive(Debug)]
pub enum ModelExecutionAckReceipt {
    Acknowledged(ModelStreamFlowAckReceipt),
    Cancelled(ModelStreamFlowCancellationReceipt),
}

/// Shared typed execution-port result used by local and remote adapters.
#[derive(Debug)]
pub enum ModelExecutionPortReceipt {
    Opened(ModelExecutionOpenReceipt),
    Acknowledged(ModelExecutionAckReceipt),
}

/// Unique production runtime joining durable exchange authority, Gateway, and
/// stream flow control. It owns no second Provider or retry ledger.
pub struct ModelExecutionRuntime<'a, 'storage> {
    exchanges: &'a DurableModelExchangeAuthority,
    planner: &'a mut dyn ModelRetryPreOpenPlannerPort,
    planned_contexts: &'a dyn ModelRetrySettlementContextPort,
    gateway: &'a mut ProviderGateway<'storage>,
    pool: &'a mut ModelRequestPool,
    enterprise_quota: Option<&'a mut dyn EnterpriseQuotaAdmissionPort>,
    enterprise_policy: Option<&'a mut DurableProviderPolicyEnforcement>,
}

struct RuntimeOpenContext {
    context: ModelRetrySettlementContext,
    reservation: Option<ProviderAdmissionOpenReceipt>,
    existing_exchange: bool,
}

impl<'a, 'storage> ModelExecutionRuntime<'a, 'storage> {
    #[must_use]
    pub const fn new(
        exchanges: &'a DurableModelExchangeAuthority,
        planner: &'a mut dyn ModelRetryPreOpenPlannerPort,
        planned_contexts: &'a dyn ModelRetrySettlementContextPort,
        gateway: &'a mut ProviderGateway<'storage>,
        pool: &'a mut ModelRequestPool,
    ) -> Self {
        Self {
            exchanges,
            planner,
            planned_contexts,
            gateway,
            pool,
            enterprise_quota: None,
            enterprise_policy: None,
        }
    }

    /// Creates a production runtime with the unique enterprise quota port.
    #[must_use]
    pub const fn new_with_enterprise_quota(
        exchanges: &'a DurableModelExchangeAuthority,
        planner: &'a mut dyn ModelRetryPreOpenPlannerPort,
        planned_contexts: &'a dyn ModelRetrySettlementContextPort,
        gateway: &'a mut ProviderGateway<'storage>,
        pool: &'a mut ModelRequestPool,
        enterprise_quota: &'a mut dyn EnterpriseQuotaAdmissionPort,
    ) -> Self {
        Self {
            exchanges,
            planner,
            planned_contexts,
            gateway,
            pool,
            enterprise_quota: Some(enterprise_quota),
            enterprise_policy: None,
        }
    }

    /// Creates the production runtime with the unique enterprise Policy and
    /// quota authorities installed before Provider access.
    #[must_use]
    pub const fn new_with_enterprise_controls(
        exchanges: &'a DurableModelExchangeAuthority,
        planner: &'a mut dyn ModelRetryPreOpenPlannerPort,
        planned_contexts: &'a dyn ModelRetrySettlementContextPort,
        gateway: &'a mut ProviderGateway<'storage>,
        pool: &'a mut ModelRequestPool,
        enterprise_quota: &'a mut dyn EnterpriseQuotaAdmissionPort,
        enterprise_policy: &'a mut DurableProviderPolicyEnforcement,
    ) -> Self {
        Self {
            exchanges,
            planner,
            planned_contexts,
            gateway,
            pool,
            enterprise_quota: Some(enterprise_quota),
            enterprise_policy: Some(enterprise_policy),
        }
    }

    /// Accepts one Worker model-open message using its durable retry plan.
    ///
    /// # Errors
    ///
    /// Fails closed for missing/corrupt context, changed input, interrupted
    /// opening, Gateway failure, or pool failure.
    #[allow(clippy::too_many_lines)]
    pub fn open(
        &mut self,
        message: &ModelOpenMessage,
    ) -> Result<ModelExecutionOpenReceipt, ModelExecutionRuntimeError> {
        self.restore_pool_authority()?;
        let RuntimeOpenContext {
            context,
            reservation,
            existing_exchange,
        } = self.open_context(message)?;
        let route_authority = active_authority(&context)?;
        let open_digest = runtime_open_digest(message, &context)?;
        let adapter_request_id = adapter_request_id(message, &open_digest);
        let begin = ProviderExchangeBegin {
            model_exchange_id: message.model_exchange_id.clone(),
            request_id: message.request_id.clone(),
            message_id: message.message_id.clone(),
            open_digest: open_digest.clone(),
            provider_id: route_authority.route().provider_id.clone(),
            adapter_request_id: adapter_request_id.clone(),
            started_at: message.sent_at.clone(),
        };

        if existing_exchange {
            let snapshot = self.exchanges.begin(&begin)?;
            return self.replay_open(&snapshot, &context, message);
        }

        let pool_before_submit = self.pool.clone();
        let pool = self.submit_pool(route_authority, message)?;
        let pool_authority_json = match self.pool_authority_json() {
            Ok(bytes) => bytes,
            Err(error) => {
                *self.pool = pool_before_submit;
                return Err(error);
            }
        };
        if pool.state == ModelRequestState::Queued {
            let operation = pool_submit_operation(&message.model_exchange_id, &open_digest);
            if let Err(error) = self.exchanges.save_pool_authority_with_readiness(
                &pool_authority_json,
                &message.sent_at,
                &route_authority.route_key().project_scope(),
                &operation,
            ) {
                *self.pool = pool_before_submit;
                return Err(error);
            }
            return Ok(ModelExecutionOpenReceipt::Queued {
                pool,
                context_fingerprint: context.context_fingerprint().to_owned(),
            });
        }
        if pool.state != ModelRequestState::Active {
            return Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::ExchangeConflict,
            ));
        }

        let snapshot = match self.exchanges.begin_with_pool_authority(
            &begin,
            &pool_authority_json,
            &route_authority.route_key().project_scope(),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                *self.pool = pool_before_submit;
                return Err(error);
            }
        };
        if snapshot.idempotent_replay {
            return self.replay_open(&snapshot, &context, message);
        }
        let reservation = reservation.as_ref().ok_or_else(|| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
        })?;
        let gateway =
            match self.open_gateway_after_reservation(message, reservation, &adapter_request_id) {
                Ok(receipt) => receipt,
                Err(error) => {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("model execution provider open error: {error:?}");
                    }
                    let kind = error.kind();
                    self.gateway.release_interrupted_open(
                        &reservation.route_authority,
                        &reservation.reservation.request_id,
                        &reservation.reservation.model_exchange_id,
                    )?;
                    self.terminate_pool_if_present(&message.model_exchange_id)?;
                    self.persist_gateway_failure(message, &open_digest, kind)?;
                    return Err(ModelExecutionRuntimeError::new(
                        ModelExecutionRuntimeErrorKind::OpenFailed,
                    ));
                }
            };
        let finalize = self.persist_opened(message, &open_digest, &context);
        if let Err(error) = finalize {
            self.cleanup_opened_provider(&message.model_exchange_id, &message.sent_at)?;
            self.terminate_pool_if_present(&message.model_exchange_id)?;
            self.persist_gateway_failure(message, &open_digest, ProviderGatewayErrorKind::Storage)?;
            return Err(error);
        }
        Ok(ModelExecutionOpenReceipt::Opened {
            gateway,
            pool,
            context_fingerprint: context.context_fingerprint().to_owned(),
            idempotent_replay: false,
        })
    }

    fn open_gateway_after_reservation(
        &mut self,
        message: &ModelOpenMessage,
        reservation: &ProviderAdmissionOpenReceipt,
        adapter_request_id: &str,
    ) -> Result<ProviderGatewayOpenReceipt, ProviderGatewayError> {
        if let Some(policy) = self.enterprise_policy.as_deref_mut() {
            let context = self
                .planned_contexts
                .load_context(&message.model_exchange_id)
                .map_err(|_| ProviderGatewayError::policy_unavailable())?
                .ok_or_else(ProviderGatewayError::policy_unavailable)?;
            policy
                .enforce(&context)
                .map_err(|error| match error.kind() {
                    ProviderPolicyErrorKind::Rejected => ProviderGatewayError::policy_denied(),
                    ProviderPolicyErrorKind::Unavailable => {
                        ProviderGatewayError::policy_unavailable()
                    }
                })?;
        }
        if let Some(enterprise_quota) = self.enterprise_quota.as_deref_mut() {
            self.gateway.open_after_reservation_with_enterprise_quota(
                message,
                reservation,
                adapter_request_id,
                self.planned_contexts,
                enterprise_quota,
            )
        } else {
            self.gateway
                .open_after_reservation(message, reservation, adapter_request_id)
        }
    }

    fn open_context(
        &mut self,
        message: &ModelOpenMessage,
    ) -> Result<RuntimeOpenContext, ModelExecutionRuntimeError> {
        let existing_exchange = self
            .exchanges
            .snapshot(&message.model_exchange_id)?
            .is_some();
        let reservation = if existing_exchange {
            None
        } else {
            Some(self.gateway.reserve_before_open(message).map_err(|error| {
                if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                    eprintln!("model execution provider reserve error: {error:?}");
                }
                error
            })?)
        };
        let context = if let Some(reservation) = &reservation {
            self.prepare_planned_context(message, reservation)?
        } else {
            self.load_planned_context(&message.model_exchange_id)?
        };
        validate_context_message(&context, message)?;
        let route_authority = active_authority(&context)?;
        if reservation
            .as_ref()
            .is_some_and(|reservation| &reservation.route_authority != route_authority)
        {
            return Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::ContextCorrupt,
            ));
        }
        Ok(RuntimeOpenContext {
            context,
            reservation,
            existing_exchange,
        })
    }

    /// Converts one Provider batch, applies flow control, and produces the
    /// canonical Control Plane-to-Worker model-chunk messages.
    ///
    /// # Errors
    ///
    /// Rejects unknown/terminal exchanges, inconsistent terminal facts, or
    /// flow/storage failures.
    pub fn offer_provider_batch(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frames: &[CanonicalModelStreamFrame],
        terminal: Option<ProviderGatewayTerminal>,
        sent_at: &Instant,
    ) -> Result<ModelExecutionBatchReceipt, ModelExecutionRuntimeError> {
        self.restore_pool_authority()?;
        let snapshot = self.exchanges.snapshot(model_exchange_id)?.ok_or_else(|| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
        })?;
        if snapshot.state != ProviderExchangeState::Opened {
            return Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::ExchangeConflict,
            ));
        }
        self.restore_pool_if_needed(&snapshot)?;
        let durable = durable_gateway_exchange(&snapshot)?;
        self.gateway.restore_durable_exchange(&durable)?;
        self.synchronize_restored_provider_read(model_exchange_id)?;
        let flow = ModelStreamFlowCoordinator::new(self.pool, self.gateway)
            .offer_provider_batch_with_progress(
                model_exchange_id,
                frames,
                terminal,
                self.exchanges,
                sent_at,
            )?;
        let chunks = model_chunks(&durable, frames, sent_at)?;
        if let (Some(command), Some(receipt)) = (terminal, flow.gateway_terminal.as_ref()) {
            self.persist_terminal(model_exchange_id, command, receipt, sent_at)?;
        } else {
            self.persist_pool_authority(sent_at)?;
        }
        seal_batch_receipt(chunks, flow)
    }

    /// Applies a Worker acknowledgement to the same Gateway/flow instance.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, unknown exchanges, invalid sequence, or
    /// flow/storage failure.
    pub fn acknowledge(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<ModelExecutionAckReceipt, ModelExecutionRuntimeError> {
        self.restore_pool_authority()?;
        let snapshot = self
            .exchanges
            .snapshot(&acknowledgement.model_exchange_id)?
            .ok_or_else(|| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
            })?;
        if !matches!(
            snapshot.state,
            ProviderExchangeState::Opened | ProviderExchangeState::Terminal
        ) {
            return Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::ExchangeConflict,
            ));
        }
        let durable = durable_gateway_exchange(&snapshot)?;
        self.gateway.restore_durable_exchange(&durable)?;
        if snapshot.state == ProviderExchangeState::Terminal {
            self.gateway
                .validate_worker_acknowledgement(acknowledgement)?;
            return self.acknowledge_terminal(acknowledgement);
        }
        self.restore_pool_if_needed(&snapshot)?;
        self.synchronize_restored_provider_read(&acknowledgement.model_exchange_id)?;
        if acknowledgement.error.is_some() {
            let mut flow = ModelStreamFlowCoordinator::new(self.pool, self.gateway);
            let receipt = flow.cancel_from_worker_with_progress(acknowledgement, self.exchanges)?;
            self.persist_terminal(
                &acknowledgement.model_exchange_id,
                ProviderGatewayTerminal::Cancelled,
                &receipt.gateway,
                &acknowledgement.sent_at,
            )?;
            Ok(ModelExecutionAckReceipt::Cancelled(receipt))
        } else {
            self.gateway
                .validate_worker_acknowledgement(acknowledgement)?;
            let sequence = u64::try_from(acknowledgement.ack_sequence.0).map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
            })?;
            let mut flow = ModelStreamFlowCoordinator::new(self.pool, self.gateway);
            let receipt = flow.acknowledge(&acknowledgement.model_exchange_id, sequence)?;
            self.persist_pool_authority(&acknowledgement.sent_at)?;
            Ok(ModelExecutionAckReceipt::Acknowledged(receipt))
        }
    }

    fn acknowledge_terminal(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<ModelExecutionAckReceipt, ModelExecutionRuntimeError> {
        let ack_digest = model_ack_digest(acknowledgement)?;
        if let Some(stored) = self
            .exchanges
            .final_ack(&acknowledgement.model_exchange_id)?
        {
            if stored.ack_digest != ack_digest
                || stored.ack_sequence != acknowledgement.ack_sequence.0
            {
                return Err(ModelExecutionRuntimeError::new(
                    ModelExecutionRuntimeErrorKind::ExchangeConflict,
                ));
            }
            let mut receipt = decode_final_ack_receipt(stored.receipt_json())?;
            receipt.pool.replayed = true;
            return Ok(ModelExecutionAckReceipt::Acknowledged(receipt));
        }
        self.restore_pool_if_needed(
            &self
                .exchanges
                .snapshot(&acknowledgement.model_exchange_id)?
                .ok_or_else(|| {
                    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage)
                })?,
        )?;
        let sequence = u64::try_from(acknowledgement.ack_sequence.0).map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
        })?;
        let mut staged_pool = self.pool.clone();
        let pool = staged_pool
            .acknowledge(&acknowledgement.model_exchange_id, sequence)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))?;
        let receipt = ModelStreamFlowAckReceipt {
            pool,
            provider_control: None,
        };
        if receipt.pool.buffered_frames == 0 {
            staged_pool
                .forget_terminal(&acknowledgement.model_exchange_id)
                .map_err(|_| {
                    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
                })?;
            let authority = staged_pool.export_authority().map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
            })?;
            let receipt_json = final_ack_receipt_json(&receipt)?;
            self.exchanges.commit_final_ack(
                acknowledgement,
                &ack_digest,
                &receipt_json,
                &authority,
                &self
                    .pool
                    .project_scope_for_exchange(&acknowledgement.model_exchange_id)
                    .map_err(|_| {
                        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
                    })?,
            )?;
        } else {
            let authority = staged_pool.export_authority().map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
            })?;
            self.exchanges
                .save_pool_authority(&authority, &acknowledgement.sent_at)?;
        }
        *self.pool = staged_pool;
        Ok(ModelExecutionAckReceipt::Acknowledged(receipt))
    }

    fn prepare_planned_context(
        &mut self,
        message: &ModelOpenMessage,
        reservation: &ProviderAdmissionOpenReceipt,
    ) -> Result<ModelRetrySettlementContext, ModelExecutionRuntimeError> {
        let prepared = match self.planner.prepare(message, reservation) {
            Ok(context) => context,
            Err(error) => return self.planning_failure(reservation, &error),
        };
        let durable = self.load_planned_context(&message.model_exchange_id)?;
        let prepared_bytes = prepared.encode_json().map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
        })?;
        let durable_bytes = durable.encode_json().map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
        })?;
        if prepared_bytes != durable_bytes {
            return Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::ContextCorrupt,
            ));
        }
        Ok(durable)
    }

    fn planning_failure(
        &mut self,
        reservation: &ProviderAdmissionOpenReceipt,
        error: &ModelRetryPlannerError,
    ) -> Result<ModelRetrySettlementContext, ModelExecutionRuntimeError> {
        if let Some(release) = error.release_authority() {
            if !release.authorizes(reservation) {
                return Err(ModelExecutionRuntimeError::new(
                    ModelExecutionRuntimeErrorKind::ContextCorrupt,
                ));
            }
            let released = self.gateway.release_interrupted_open(
                &reservation.route_authority,
                release.request_id(),
                release.model_exchange_id(),
            )?;
            if released.is_none() {
                return Err(ModelExecutionRuntimeError::new(
                    ModelExecutionRuntimeErrorKind::ContextCorrupt,
                ));
            }
        }
        let kind = match error.kind() {
            ModelRetryPlannerErrorKind::InvalidRequest
            | ModelRetryPlannerErrorKind::IdentityMismatch => {
                ModelExecutionRuntimeErrorKind::InvalidInput
            }
            ModelRetryPlannerErrorKind::Policy | ModelRetryPlannerErrorKind::Ledger => {
                ModelExecutionRuntimeErrorKind::Planning
            }
            ModelRetryPlannerErrorKind::Storage => ModelExecutionRuntimeErrorKind::Storage,
        };
        Err(ModelExecutionRuntimeError::new(kind))
    }

    fn load_planned_context(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<ModelRetrySettlementContext, ModelExecutionRuntimeError> {
        self.planned_contexts
            .load_context(model_exchange_id)
            .map_err(|error| match error.kind() {
                crate::ModelRetrySettlementContextErrorKind::Corrupt => {
                    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
                }
                crate::ModelRetrySettlementContextErrorKind::Unavailable => {
                    ModelExecutionRuntimeError::new(
                        ModelExecutionRuntimeErrorKind::ContextUnavailable,
                    )
                }
            })?
            .ok_or_else(|| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextUnavailable)
            })
    }

    fn replay_open(
        &mut self,
        snapshot: &ProviderExchangeSnapshot,
        context: &ModelRetrySettlementContext,
        message: &ModelOpenMessage,
    ) -> Result<ModelExecutionOpenReceipt, ModelExecutionRuntimeError> {
        match snapshot.state {
            ProviderExchangeState::Opening => {
                self.gateway.cleanup_interrupted_open(
                    active_authority(context)?,
                    &message.request_id,
                    &message.model_exchange_id,
                    &snapshot.provider_id,
                    &snapshot.adapter_request_id,
                )?;
                self.terminate_pool_if_present(&message.model_exchange_id)?;
                let pool_authority_json = self.pool_authority_json()?;
                self.exchanges.failed_with_pool_authority(
                    &message.model_exchange_id,
                    &snapshot.open_digest,
                    &ProviderExchangeFailure {
                        failure_kind: FAILURE_INTERRUPTED.to_owned(),
                        failed_at: message.sent_at.clone(),
                    },
                    &pool_authority_json,
                    &active_authority(context)?.route_key().project_scope(),
                )?;
                Err(ModelExecutionRuntimeError::new(
                    ModelExecutionRuntimeErrorKind::OpenInterrupted,
                ))
            }
            ProviderExchangeState::Failed => {
                self.terminate_pool_if_present(&message.model_exchange_id)?;
                Err(ModelExecutionRuntimeError::new(
                    if snapshot.failure_kind.as_deref() == Some(FAILURE_INTERRUPTED) {
                        ModelExecutionRuntimeErrorKind::OpenInterrupted
                    } else {
                        ModelExecutionRuntimeErrorKind::OpenFailed
                    },
                ))
            }
            ProviderExchangeState::Terminal => {
                validate_persisted_context(snapshot, context)?;
                self.terminate_pool_if_present(&message.model_exchange_id)?;
                Err(ModelExecutionRuntimeError::new(
                    ModelExecutionRuntimeErrorKind::ExchangeConflict,
                ))
            }
            ProviderExchangeState::Opened => {
                validate_persisted_context(snapshot, context)?;
                let restored = self
                    .pool
                    .reconnect(&snapshot.model_exchange_id)
                    .map_err(|_| {
                        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
                    })?;
                if restored.state != ModelRequestState::Active {
                    return Err(ModelExecutionRuntimeError::new(
                        ModelExecutionRuntimeErrorKind::ExchangeConflict,
                    ));
                }
                let pool = ModelRequestAdmissionReceipt {
                    model_exchange_id: restored.model_exchange_id,
                    state: restored.state,
                    status: ModelRequestAdmissionStatus::Duplicate,
                    queue_position: restored.queue_position,
                };
                let durable = durable_gateway_exchange(snapshot)?;
                let gateway = self.gateway.restore_durable_exchange(&durable)?;
                self.synchronize_restored_provider_read(&snapshot.model_exchange_id)?;
                Ok(ModelExecutionOpenReceipt::Opened {
                    gateway,
                    pool,
                    context_fingerprint: context.context_fingerprint().to_owned(),
                    idempotent_replay: true,
                })
            }
        }
    }

    fn submit_pool(
        &mut self,
        route_authority: &FrozenModelRouteAuthority,
        message: &ModelOpenMessage,
    ) -> Result<ModelRequestAdmissionReceipt, ModelExecutionRuntimeError> {
        let admission = ModelRequestAdmission::from_frozen_authority(
            route_authority,
            message.model_exchange_id.clone(),
            message.request_id.clone(),
        )
        .map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
        })?;
        self.pool
            .submit(&admission)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))
    }

    fn restore_pool_if_needed(
        &mut self,
        snapshot: &ProviderExchangeSnapshot,
    ) -> Result<(), ModelExecutionRuntimeError> {
        if self.pool.reconnect(&snapshot.model_exchange_id).is_ok() {
            return Ok(());
        }
        Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::ContextCorrupt,
        ))
    }

    fn restore_pool_authority(&mut self) -> Result<(), ModelExecutionRuntimeError> {
        if !self.pool.is_empty() {
            return Ok(());
        }
        if let Some(bytes) = self.exchanges.load_pool_authority()? {
            self.pool.restore_authority(&bytes).map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
            })?;
        }
        Ok(())
    }

    fn persist_pool_authority(
        &self,
        updated_at: &Instant,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let bytes = self.pool_authority_json()?;
        self.exchanges.save_pool_authority(&bytes, updated_at)
    }

    fn pool_authority_json(&self) -> Result<Vec<u8>, ModelExecutionRuntimeError> {
        self.pool
            .export_authority()
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))
    }

    fn persist_opened(
        &mut self,
        message: &ModelOpenMessage,
        open_digest: &Sha256Digest,
        context: &ModelRetrySettlementContext,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let durable = self.gateway.durable_exchange(&message.model_exchange_id)?;
        validate_open_authority(&durable, context, message)?;
        let opened = ProviderExchangeOpened::new(
            Sha256Digest(durable.route_authority().fingerprint().to_owned()),
            durable.route_authority().to_durable_json().map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
            })?,
            durable.to_durable_receipt_json()?,
            context.encode_json().map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
            })?,
            message.sent_at.clone(),
        )?;
        self.exchanges
            .opened(&message.model_exchange_id, open_digest, &opened)?;
        Ok(())
    }

    fn synchronize_restored_provider_read(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let paused = self
            .pool
            .reconnect(model_exchange_id)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))?
            .read_control
            == ModelStreamReadControl::Paused;
        self.gateway
            .set_provider_read_paused(model_exchange_id, paused)?;
        Ok(())
    }

    fn cleanup_opened_provider(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        observed_at: &Instant,
    ) -> Result<(), ModelExecutionRuntimeError> {
        self.gateway.apply_terminal(
            model_exchange_id,
            ProviderGatewayTerminal::Failed {
                failure: ModelAttemptFailureFact::from_gateway(
                    ProviderGatewayErrorKind::Storage,
                    ModelExecutionCertainty::AcceptanceUnknown,
                ),
                charge: None,
            },
            observed_at,
        )?;
        Ok(())
    }

    fn terminate_pool_failed(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<(), ModelExecutionRuntimeError> {
        self.pool
            .terminate(model_exchange_id, ModelRequestTerminalOutcome::Failed)
            .map(|_| ())
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))
    }

    fn terminate_pool_if_present(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<(), ModelExecutionRuntimeError> {
        if self
            .pool
            .reconnect(model_exchange_id)
            .is_ok_and(|snapshot| {
                matches!(
                    snapshot.state,
                    ModelRequestState::Queued | ModelRequestState::Active
                )
            })
        {
            self.terminate_pool_failed(model_exchange_id)?;
        }
        if self
            .pool
            .reconnect(model_exchange_id)
            .is_ok_and(|snapshot| {
                snapshot.state != ModelRequestState::Active
                    && snapshot.state != ModelRequestState::Queued
                    && snapshot.buffered_frames == 0
            })
        {
            self.pool.forget_terminal(model_exchange_id).map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow)
            })?;
        }
        Ok(())
    }

    fn persist_gateway_failure(
        &self,
        message: &ModelOpenMessage,
        open_digest: &Sha256Digest,
        kind: ProviderGatewayErrorKind,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let pool_authority_json = self.pool_authority_json()?;
        self.exchanges.failed_with_pool_authority(
            &message.model_exchange_id,
            open_digest,
            &ProviderExchangeFailure {
                failure_kind: format!("{FAILURE_GATEWAY_PREFIX}{}", gateway_kind(kind)),
                failed_at: message.sent_at.clone(),
            },
            &pool_authority_json,
            &active_authority(&self.load_planned_context(&message.model_exchange_id)?)?
                .route_key()
                .project_scope(),
        )?;
        Ok(())
    }

    fn release_failed_enterprise_quota(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        settled_at: &Instant,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let reason = match command {
            ProviderGatewayTerminal::Completed { .. } => return Ok(()),
            ProviderGatewayTerminal::Failed { .. } => EnterpriseQuotaReleaseReason::Failed,
            ProviderGatewayTerminal::Cancelled => EnterpriseQuotaReleaseReason::Cancelled,
        };
        if let Some(quota) = self.enterprise_quota.as_deref_mut() {
            ProviderEnterpriseQuotaSaga::new(quota)
                .release_durable_terminal(
                    self.planned_contexts,
                    model_exchange_id,
                    reason,
                    settled_at.clone(),
                )
                .map_err(|_| {
                    ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Gateway)
                })?;
        }
        Ok(())
    }

    fn persist_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        command: ProviderGatewayTerminal,
        receipt: &ProviderGatewayTerminalReceipt,
        settled_at: &Instant,
    ) -> Result<(), ModelExecutionRuntimeError> {
        let receipt_json = terminal_receipt_json(receipt)?;
        let pool_authority_json = self.pool_authority_json()?;
        let project_scope = self
            .pool
            .project_scope_for_exchange(model_exchange_id)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Flow))?;
        self.release_failed_enterprise_quota(model_exchange_id, command, settled_at)?;
        self.exchanges.terminal_with_pool_authority(
            model_exchange_id,
            &ProviderExchangeTerminal::new(
                terminal_digest(command)?,
                receipt_json,
                settled_at.clone(),
            )?,
            &pool_authority_json,
            &project_scope,
        )?;
        Ok(())
    }
}

impl ExecutionPortCore for ModelExecutionRuntime<'_, '_> {
    type Output = ModelExecutionPortReceipt;
    type Error = ModelExecutionRuntimeError;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        match message {
            ExecutionPortMessage::ModelOpenMessage(message) => {
                self.open(message).map(ModelExecutionPortReceipt::Opened)
            }
            ExecutionPortMessage::ModelAckMessage(message) => self
                .acknowledge(message)
                .map(ModelExecutionPortReceipt::Acknowledged),
            _ => Err(ModelExecutionRuntimeError::new(
                ModelExecutionRuntimeErrorKind::UnsupportedMessage,
            )),
        }
    }
}

fn runtime_open_digest(
    message: &ModelOpenMessage,
    context: &ModelRetrySettlementContext,
) -> Result<Sha256Digest, ModelExecutionRuntimeError> {
    let bytes = serde_json::to_vec(&(message, context.request_fingerprint())).map_err(|_| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn adapter_request_id(message: &ModelOpenMessage, open_digest: &Sha256Digest) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-adapter-request.v1\0");
    for value in [
        message.model_exchange_id.0.as_bytes(),
        message.request_id.0.as_bytes(),
        open_digest.0.as_bytes(),
    ] {
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value);
    }
    format!("adapter_{:x}", digest.finalize())
}

fn validate_context_message(
    context: &ModelRetrySettlementContext,
    message: &ModelOpenMessage,
) -> Result<(), ModelExecutionRuntimeError> {
    let start = context.start_receipt();
    if start.model_exchange_id != message.model_exchange_id
        || start.reservation_request_id != message.request_id
        || message.worker_session_id != message.session_identity.worker_session_id
    {
        return Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::ExchangeConflict,
        ));
    }
    Ok(())
}

fn validate_persisted_context(
    snapshot: &ProviderExchangeSnapshot,
    context: &ModelRetrySettlementContext,
) -> Result<(), ModelExecutionRuntimeError> {
    let expected = context.encode_json().map_err(|_| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
    })?;
    let persisted = snapshot.settlement_context_json().ok_or_else(|| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
    })?;
    let decoded = ModelRetrySettlementContext::decode_json(persisted).map_err(|_| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
    })?;
    if persisted != expected || decoded.context_fingerprint() != context.context_fingerprint() {
        return Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::ExchangeConflict,
        ));
    }
    Ok(())
}

fn active_authority(
    context: &ModelRetrySettlementContext,
) -> Result<&FrozenModelRouteAuthority, ModelExecutionRuntimeError> {
    let attempt = context.start_receipt().attempt;
    let mut first = 1_u64;
    for step in context.request().plan.steps() {
        let last = first.saturating_add(step.max_attempts()).saturating_sub(1);
        if (first..=last).contains(&attempt) {
            return Ok(step.authority());
        }
        first = last.saturating_add(1);
    }
    Err(ModelExecutionRuntimeError::new(
        ModelExecutionRuntimeErrorKind::ContextCorrupt,
    ))
}

fn validate_open_authority(
    durable: &ProviderGatewayDurableExchange,
    context: &ModelRetrySettlementContext,
    message: &ModelOpenMessage,
) -> Result<(), ModelExecutionRuntimeError> {
    if durable.route_authority().fingerprint() != context.start_receipt().route_fingerprint
        || durable.open_receipt().request_id != message.request_id
        || durable.open_receipt().model_exchange_id != message.model_exchange_id
    {
        return Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::ExchangeConflict,
        ));
    }
    Ok(())
}

fn durable_gateway_exchange(
    snapshot: &ProviderExchangeSnapshot,
) -> Result<ProviderGatewayDurableExchange, ModelExecutionRuntimeError> {
    let authority = FrozenModelRouteAuthority::from_durable_json(
        snapshot.route_authority_json().ok_or_else(|| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
        })?,
    )
    .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt))?;
    ProviderGatewayDurableExchange::from_durable_parts(
        authority,
        snapshot.open_receipt_json().ok_or_else(|| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::ContextCorrupt)
        })?,
    )
    .map_err(Into::into)
}

fn model_chunks(
    durable: &ProviderGatewayDurableExchange,
    frames: &[CanonicalModelStreamFrame],
    sent_at: &Instant,
) -> Result<Vec<ModelChunkMessage>, ModelExecutionRuntimeError> {
    frames
        .iter()
        .map(|frame| {
            let sequence = i64::try_from(frame.sequence()).map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
            })?;
            Ok(ModelChunkMessage {
                error: None,
                is_final: frame.is_terminal(),
                kind: ModelChunkMessageKind::ModelChunk,
                lease: durable.lease().clone(),
                message_id: model_chunk_message_id(
                    &durable.open_receipt().model_exchange_id,
                    frame.sequence(),
                ),
                model_exchange_id: durable.open_receipt().model_exchange_id.clone(),
                payload: Some(frame.encoded_payload()),
                schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
                sent_at: sent_at.clone(),
                sequence: ExecutionSequence(sequence),
                session_identity: durable.session_identity().clone(),
                worker_session_id: durable.worker_session_id().clone(),
            })
        })
        .collect()
}

fn model_chunks_from_pool_frames(
    durable: &ProviderGatewayDurableExchange,
    frames: &[crate::ModelStreamFrame],
    sent_at: &Instant,
) -> Result<Vec<ModelChunkMessage>, ModelExecutionRuntimeError> {
    frames
        .iter()
        .map(|frame| {
            let sequence = i64::try_from(frame.sequence()).map_err(|_| {
                ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
            })?;
            Ok(ModelChunkMessage {
                error: None,
                is_final: frame.terminal_outcome().is_some(),
                kind: ModelChunkMessageKind::ModelChunk,
                lease: durable.lease().clone(),
                message_id: model_chunk_message_id(
                    &durable.open_receipt().model_exchange_id,
                    frame.sequence(),
                ),
                model_exchange_id: durable.open_receipt().model_exchange_id.clone(),
                payload: Some(EncodedPayload {
                    content_type: "application/json".to_owned(),
                    data_base64: STANDARD.encode(frame.payload()),
                    payload_digest: Sha256Digest(format!(
                        "sha256:{:x}",
                        Sha256::digest(frame.payload())
                    )),
                }),
                schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
                sent_at: sent_at.clone(),
                sequence: ExecutionSequence(sequence),
                session_identity: durable.session_identity().clone(),
                worker_session_id: durable.worker_session_id().clone(),
            })
        })
        .collect()
}

fn seal_batch_receipt(
    chunks: Vec<ModelChunkMessage>,
    flow: ModelStreamFlowWriteReceipt,
) -> Result<ModelExecutionBatchReceipt, ModelExecutionRuntimeError> {
    let authority_sha256 = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&chunks).map_err(|_| {
            ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
        })?)
    ));
    Ok(ModelExecutionBatchReceipt {
        chunks,
        flow,
        authority_sha256,
    })
}

fn model_chunk_message_id(
    model_exchange_id: &ModelExchangeId,
    sequence: u64,
) -> ExecutionMessageId {
    deterministic_id(
        "xmsg",
        b"winwincode.model-chunk.v1\0",
        &[model_exchange_id.0.as_bytes(), &sequence.to_be_bytes()],
    )
}

fn deterministic_id(prefix: &str, domain: &[u8], parts: &[&[u8]]) -> ExecutionMessageId {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut first = [0_u8; 16];
    first.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(first);
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = alphabet[usize::try_from(value & 31).expect("base32 digit fits")];
        value >>= 5;
    }
    ExecutionMessageId(format!(
        "{prefix}_{}",
        std::str::from_utf8(&suffix).expect("Crockford alphabet is UTF-8")
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredTerminalReceipt<'a> {
    model_exchange_id: &'a ModelExchangeId,
    outcome: ProviderGatewayTerminalOutcome,
    admission: &'a crate::ModelReservationTerminalReceipt,
    settled_at: &'a Instant,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableTerminalReceipt {
    model_exchange_id: ModelExchangeId,
    outcome: ProviderGatewayTerminalOutcome,
    admission: crate::ModelReservationTerminalReceipt,
    settled_at: Instant,
}

fn terminal_receipt_json(
    receipt: &ProviderGatewayTerminalReceipt,
) -> Result<Vec<u8>, ModelExecutionRuntimeError> {
    serde_json::to_vec(&StoredTerminalReceipt {
        model_exchange_id: &receipt.model_exchange_id,
        outcome: receipt.outcome,
        admission: &receipt.admission,
        settled_at: &receipt.settled_at,
    })
    .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))
}

fn decode_terminal_receipt(
    bytes: &[u8],
) -> Result<ProviderGatewayTerminalReceipt, ModelExecutionRuntimeError> {
    let receipt: DurableTerminalReceipt = serde_json::from_slice(bytes)
        .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?;
    Ok(ProviderGatewayTerminalReceipt {
        model_exchange_id: receipt.model_exchange_id,
        outcome: receipt.outcome,
        admission: receipt.admission,
        settled_at: receipt.settled_at,
        idempotent_replay: true,
    })
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableFinalAckReceipt {
    schema: String,
    model_exchange_id: ModelExchangeId,
    acknowledged_sequence: u64,
}

fn final_ack_receipt_json(
    receipt: &ModelStreamFlowAckReceipt,
) -> Result<Vec<u8>, ModelExecutionRuntimeError> {
    if receipt.pool.buffered_frames != 0
        || receipt.pool.buffered_bytes != 0
        || receipt.pool.read_control != ModelStreamReadControl::Closed
        || receipt.provider_control.is_some()
    {
        return Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::Flow,
        ));
    }
    serde_json::to_vec(&DurableFinalAckReceipt {
        schema: "winwincode.provider-exchange-final-ack.v1".to_owned(),
        model_exchange_id: receipt.pool.model_exchange_id.clone(),
        acknowledged_sequence: receipt.pool.acknowledged_sequence,
    })
    .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))
}

fn decode_final_ack_receipt(
    bytes: &[u8],
) -> Result<ModelStreamFlowAckReceipt, ModelExecutionRuntimeError> {
    let receipt: DurableFinalAckReceipt = serde_json::from_slice(bytes)
        .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?;
    if receipt.schema != "winwincode.provider-exchange-final-ack.v1"
        || serde_json::to_vec(&receipt)
            .map_err(|_| ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::Storage))?
            != bytes
    {
        return Err(ModelExecutionRuntimeError::new(
            ModelExecutionRuntimeErrorKind::Storage,
        ));
    }
    Ok(ModelStreamFlowAckReceipt {
        pool: ModelFrameAckReceipt {
            model_exchange_id: receipt.model_exchange_id,
            acknowledged_sequence: receipt.acknowledged_sequence,
            buffered_frames: 0,
            buffered_bytes: 0,
            read_control: ModelStreamReadControl::Closed,
            replayed: false,
        },
        provider_control: None,
    })
}

fn model_ack_digest(
    acknowledgement: &ModelAckMessage,
) -> Result<Sha256Digest, ModelExecutionRuntimeError> {
    let bytes = serde_json::to_vec(acknowledgement).map_err(|_| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn terminal_digest(
    command: ProviderGatewayTerminal,
) -> Result<Sha256Digest, ModelExecutionRuntimeError> {
    let value = match command {
        ProviderGatewayTerminal::Cancelled => serde_json::json!({"kind": "cancelled"}),
        ProviderGatewayTerminal::Completed {
            usage,
            actual_cost_micros,
        } => serde_json::json!({
            "kind": "completed",
            "inputTokens": usage.input_tokens,
            "cachedInputTokens": usage.cached_input_tokens,
            "cacheWriteInputTokens": usage.cache_write_input_tokens,
            "outputTokens": usage.output_tokens,
            "reasoningOutputTokens": usage.reasoning_output_tokens,
            "actualCostMicros": actual_cost_micros,
        }),
        ProviderGatewayTerminal::Failed { failure, charge } => serde_json::json!({
            "kind": "failed",
            "failureKind": format!("{:?}", failure.kind),
            "certainty": format!("{:?}", failure.certainty),
            "charge": charge.map(|charge| serde_json::json!({
                "inputTokens": charge.usage.input_tokens,
                "cachedInputTokens": charge.usage.cached_input_tokens,
                "cacheWriteInputTokens": charge.usage.cache_write_input_tokens,
                "outputTokens": charge.usage.output_tokens,
                "reasoningOutputTokens": charge.usage.reasoning_output_tokens,
                "actualCostMicros": charge.actual_cost_micros,
            })),
        }),
    };
    let bytes = serde_json::to_vec(&value).map_err(|_| {
        ModelExecutionRuntimeError::new(ModelExecutionRuntimeErrorKind::InvalidInput)
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn gateway_kind(kind: ProviderGatewayErrorKind) -> &'static str {
    match kind {
        ProviderGatewayErrorKind::InvalidRequest => "invalid_request",
        ProviderGatewayErrorKind::IdentityDenied => "identity_denied",
        ProviderGatewayErrorKind::IdentityUnavailable => "identity_unavailable",
        ProviderGatewayErrorKind::RouteUnavailable => "route_unavailable",
        ProviderGatewayErrorKind::RouteMismatch => "route_mismatch",
        ProviderGatewayErrorKind::ProviderNotFound => "provider_not_found",
        ProviderGatewayErrorKind::ProviderDisabled => "provider_disabled",
        ProviderGatewayErrorKind::ModelNotFound => "model_not_found",
        ProviderGatewayErrorKind::ModelDisabled => "model_disabled",
        ProviderGatewayErrorKind::CredentialUnavailable => "credential_unavailable",
        ProviderGatewayErrorKind::CredentialScopeMismatch => "credential_scope_mismatch",
        ProviderGatewayErrorKind::AdapterNotRegistered => "adapter_not_registered",
        ProviderGatewayErrorKind::AdapterRejected => "adapter_rejected",
        ProviderGatewayErrorKind::AdapterRateLimited => "adapter_rate_limited",
        ProviderGatewayErrorKind::AdapterUnavailable => "adapter_unavailable",
        ProviderGatewayErrorKind::AdapterProtocol => "adapter_protocol",
        ProviderGatewayErrorKind::ExchangeConflict => "exchange_conflict",
        ProviderGatewayErrorKind::ExchangeNotFound => "exchange_not_found",
        ProviderGatewayErrorKind::TerminalConflict => "terminal_conflict",
        ProviderGatewayErrorKind::PolicyDenied => "policy_denied",
        ProviderGatewayErrorKind::PolicyUnavailable => "policy_unavailable",
        ProviderGatewayErrorKind::AdmissionDenied => "admission_denied",
        ProviderGatewayErrorKind::AdmissionUnavailable => "admission_unavailable",
        ProviderGatewayErrorKind::SettlementUnavailable => "settlement_unavailable",
        ProviderGatewayErrorKind::CredentialLeak => "credential_leak",
        ProviderGatewayErrorKind::Storage => "storage",
    }
}

const fn storage_terminal_stage(
    stage: ProviderGatewayTerminalProgressStage,
) -> ProviderExchangeTerminalStage {
    match stage {
        ProviderGatewayTerminalProgressStage::Prepared => ProviderExchangeTerminalStage::Prepared,
        ProviderGatewayTerminalProgressStage::CancelStarted => {
            ProviderExchangeTerminalStage::CancelStarted
        }
        ProviderGatewayTerminalProgressStage::Cancelled => ProviderExchangeTerminalStage::Cancelled,
        ProviderGatewayTerminalProgressStage::ReleaseStarted => {
            ProviderExchangeTerminalStage::ReleaseStarted
        }
        ProviderGatewayTerminalProgressStage::Released => ProviderExchangeTerminalStage::Released,
        ProviderGatewayTerminalProgressStage::AdmissionStarted => {
            ProviderExchangeTerminalStage::AdmissionStarted
        }
        ProviderGatewayTerminalProgressStage::AdmissionSettled => {
            ProviderExchangeTerminalStage::AdmissionSettled
        }
        ProviderGatewayTerminalProgressStage::SettlementStarted => {
            ProviderExchangeTerminalStage::SettlementStarted
        }
        ProviderGatewayTerminalProgressStage::SettlementSettled => {
            ProviderExchangeTerminalStage::SettlementSettled
        }
    }
}

const fn gateway_terminal_stage(
    stage: ProviderExchangeTerminalStage,
) -> ProviderGatewayTerminalProgressStage {
    match stage {
        ProviderExchangeTerminalStage::Prepared => ProviderGatewayTerminalProgressStage::Prepared,
        ProviderExchangeTerminalStage::CancelStarted => {
            ProviderGatewayTerminalProgressStage::CancelStarted
        }
        ProviderExchangeTerminalStage::Cancelled => ProviderGatewayTerminalProgressStage::Cancelled,
        ProviderExchangeTerminalStage::ReleaseStarted => {
            ProviderGatewayTerminalProgressStage::ReleaseStarted
        }
        ProviderExchangeTerminalStage::Released => ProviderGatewayTerminalProgressStage::Released,
        ProviderExchangeTerminalStage::AdmissionStarted => {
            ProviderGatewayTerminalProgressStage::AdmissionStarted
        }
        ProviderExchangeTerminalStage::AdmissionSettled => {
            ProviderGatewayTerminalProgressStage::AdmissionSettled
        }
        ProviderExchangeTerminalStage::SettlementStarted => {
            ProviderGatewayTerminalProgressStage::SettlementStarted
        }
        ProviderExchangeTerminalStage::SettlementSettled => {
            ProviderGatewayTerminalProgressStage::SettlementSettled
        }
    }
}
