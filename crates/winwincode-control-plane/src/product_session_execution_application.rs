// SPDX-License-Identifier: Apache-2.0

//! Production bridge from canonical Worker/model facts to `ProductSession` Chat.

use std::{collections::BTreeMap, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{RepositoryScope, RepositoryScopeKind};
use winwincode_delivery::application::stage::seal_session_binding_authority;
use winwincode_domain::{
    ControlPlaneEventId, ExecutionJobId, ExecutionMessageId, Instant, ModelExchangeId,
    ProductSessionId, RequestId, Sha256Digest, SystemActorId,
};
use winwincode_execution_port::action_enforcement::ActionEnforcementIssuer;
use winwincode_execution_port::generated::{
    ApprovalRequestMessage, ExecutionJob, ExecutionOutcomeStatus, ExecutionPortMessage,
    ExecutionScope, InputRequestMessage, JobOutcomeMessage, ModelChunkMessage, ModelOpenMessage,
    SessionBindingMessage,
};
use winwincode_session::SessionBindingIdentity;
use winwincode_storage::{
    ExecutionDispatchAuthority, ExecutionJobState, ExecutionLeaseTerminalOutcome,
    ExecutionLeaseTerminalRequest, ExecutionQueueScope, ExecutionReservationRelease,
    ExecutionReservationReleaseReason, ExecutionReservationSettlement, ExecutionReservationState,
    ExecutionScopeReplacementAuthority, NewOutboxEvent, ProductStateStorage, PublicEventActor,
    ReceiptIdentity, RepositorySchedulerScope, RepositorySchedulerTerminalRequest, SqliteStorage,
    StateCommit, StateMutation, StorageError, StorageErrorKind, WorkerPoolId, WorkerSlotAuthority,
    WorkerSlotCancellation, WorkerSlotCloseRequest, WorkerSlotState, public_receipt_identity,
};

use crate::delivery_transaction::load_durable_execution_job;
use crate::durable_execution_port::product_session_outcome_output;
use crate::execution_port_service::lease_stamp;
use crate::{
    AppendAssistantMessageCommand, ChatInteractionService, ChatInteractionServiceError,
    ContinueProductSessionCommand, DurableExecutionPortContext, DurableExecutionPortDelegate,
    DurableExecutionPortError, DurableExecutionPortSupplement, DurableWorkerExecutionLifecycle,
    ExecutionPortServiceError, ModelExecutionBatchReceipt, ProductSessionCommandContext,
    ProductSessionService, ProductSessionServiceError, ProductSessionServiceErrorCode,
    ProductSessionTurnTerminalOutcome, RecordApprovalInteractionCommand,
    RecordAssistantTerminalCommand, RecordInputInteractionCommand,
    ReplaceProductSessionExecutionBindingCommand, WorkerExecutionLifecycleError,
    WorkerInteractionAuthority, issue_action_enforcement_receipt, public_repository_scope,
    repository_scope_key,
};

const SYSTEM_ACTOR_ID: &str = "sys_00000000000000000000000001";
const BINDING_STREAM_NAMESPACE: &[u8] = b"winwincode.product-session-worker-binding.v1";
const BINDING_RECEIPT_NAMESPACE: &[u8] = b"binding-receipt";
const MODEL_BINDING_RECEIPT_NAMESPACE: &[u8] = b"model-binding-receipt";
const MODEL_FRAME_RECEIPT_NAMESPACE: &[u8] = b"model-frame-receipt";
const MODEL_FRAME_SOURCE_RECEIPT_NAMESPACE: &[u8] = b"model-frame-source-receipt";
const MODEL_FRAME_PROJECTED_RECEIPT_NAMESPACE: &[u8] = b"model-frame-projected-receipt";
const MODEL_FRAME_PENDING_STREAM: &str = "product-session-provider-frame-pending:v1";
const MAX_PENDING_PROVIDER_FRAMES: usize = 4_096;
const MAX_PENDING_PROVIDER_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROVIDER_FRAME_HISTORY: usize = 100_000;
const MAX_PROVIDER_FRAME_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const PRODUCT_SESSION_EXECUTION_INTERNAL_TOPIC: &str = "product-session.execution.internal.v1";
const TERMINAL_RECEIPT_NAMESPACE: &[u8] = b"terminal-receipt";
const SLOT_CANCEL_RECEIPT_NAMESPACE: &[u8] = b"slot-cancel-receipt";
const SLOT_CLOSE_RECEIPT_NAMESPACE: &[u8] = b"slot-close-receipt";
const EXECUTION_TERMINAL_RECEIPT_NAMESPACE: &[u8] = b"execution-terminal-receipt";

/// Secret-free `ProductSession` execution bridge failure.
#[derive(Debug)]
pub enum ProductSessionExecutionApplicationError {
    ProductSession(ProductSessionServiceError),
    Storage(StorageError),
    WorkerLifecycle(WorkerExecutionLifecycleError),
    InvalidCanonicalFrame,
}

impl fmt::Display for ProductSessionExecutionApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProductSession execution application failed")
    }
}

impl std::error::Error for ProductSessionExecutionApplicationError {}

/// Decorates the one model/interaction delegate with `ProductSession` binding,
/// public-text, and terminal ownership.
pub struct ProductSessionExecutionApplication<Downstream> {
    downstream: Downstream,
    action_issuer: Option<ActionEnforcementIssuer>,
}

impl<Downstream> ProductSessionExecutionApplication<Downstream> {
    #[must_use]
    pub const fn new(downstream: Downstream) -> Self {
        Self {
            downstream,
            action_issuer: None,
        }
    }

    /// Installs the Control Plane-owned signer used for Worker action
    /// enforcement receipts. The signer is kept beside the existing
    /// downstream model delegate, so action requests and model exchanges use
    /// the same durable ingress and application state.
    #[must_use]
    pub const fn new_with_action_issuer(
        downstream: Downstream,
        action_issuer: ActionEnforcementIssuer,
    ) -> Self {
        Self {
            downstream,
            action_issuer: Some(action_issuer),
        }
    }

    #[must_use]
    pub const fn downstream(&self) -> &Downstream {
        &self.downstream
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableProviderPublicFrame {
    schema: String,
    repository_scope: RepositoryScope,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    model_exchange_id: ModelExchangeId,
    chunk: ModelChunkMessage,
    public_text_delta: String,
    public_stream_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingProviderPublicFrames {
    schema: String,
    frames: BTreeMap<String, DurableProviderPublicFrame>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderPublicFrameHistoryEntry {
    body_sha256: Sha256Digest,
    public_stream_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderPublicFrameHistory {
    schema: String,
    execution_job_id: ExecutionJobId,
    model_exchange_id: ModelExchangeId,
    entries: BTreeMap<String, ProviderPublicFrameHistoryEntry>,
}

pub(crate) fn project_product_session_model_batch(
    storage: &mut SqliteStorage,
    batch: &ModelExecutionBatchReceipt,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let chunks = batch
        .verified_chunks()
        .map_err(|()| ProductSessionExecutionApplicationError::InvalidCanonicalFrame)?;
    project_verified_product_session_chunks(storage, chunks)
}

fn project_verified_product_session_chunks(
    storage: &mut SqliteStorage,
    chunks: &[ModelChunkMessage],
) -> Result<(), ProductSessionExecutionApplicationError> {
    let mut proposed = Vec::new();
    for chunk in chunks {
        let Some(public_text_delta) = public_text_delta(chunk)? else {
            continue;
        };
        if chunk.session_identity.stage_run_id.is_some() {
            continue;
        }
        let Some(staged) = find_staged_binding_for_projection(storage, &chunk.lease.job_id)? else {
            continue;
        };
        let source = DurableProviderPublicFrame {
            schema: "winwincode.product-session-provider-frame.v1".to_owned(),
            repository_scope: repository_scope_from_execution_scope(&staged.execution_scope),
            product_session_id: chunk.session_identity.product_session_id.clone(),
            execution_job_id: chunk.lease.job_id.clone(),
            model_exchange_id: chunk.model_exchange_id.clone(),
            chunk: chunk.clone(),
            public_text_delta,
            public_stream_sequence: 0,
        };
        validate_provider_source(&staged, &source)?;
        validate_bound_provider_source(storage, &source)?;
        proposed.push(source);
    }
    let mut pending = persist_provider_batch_sources(storage, &proposed)?;
    pending.sort_by(provider_projection_order);
    for source in pending {
        project_provider_source(storage, &source)?;
    }
    Ok(())
}

pub(crate) fn reconcile_product_session_model_frames(
    storage: &mut SqliteStorage,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let (pending, _) = load_pending_provider_frames(storage)?;
    let mut ordered = pending.frames.into_values().collect::<Vec<_>>();
    ordered.sort_by(provider_projection_order);
    for source in ordered {
        project_provider_source(storage, &source)?;
    }
    Ok(())
}

fn reconcile_product_session_model_exchange(
    storage: &mut SqliteStorage,
    execution_job_id: &ExecutionJobId,
    model_exchange_id: &ModelExchangeId,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let (pending, _) = load_pending_provider_frames(storage)?;
    let mut ordered = pending
        .frames
        .into_values()
        .filter(|source| {
            source.execution_job_id == *execution_job_id
                && source.model_exchange_id == *model_exchange_id
        })
        .collect::<Vec<_>>();
    ordered.sort_by(provider_projection_order);
    for source in ordered {
        project_provider_source(storage, &source)?;
    }
    Ok(())
}

fn persist_provider_batch_sources(
    storage: &mut SqliteStorage,
    proposed: &[DurableProviderPublicFrame],
) -> Result<Vec<DurableProviderPublicFrame>, ProductSessionExecutionApplicationError> {
    if proposed.is_empty() {
        return Ok(Vec::new());
    }
    validate_provider_batch_sources(proposed)?;
    let request_id = provider_batch_request_id(proposed)?;
    let identity = application_receipt_identity(&proposed[0].repository_scope, &request_id)?;
    let digest = provider_batch_digest(proposed)?;
    if storage
        .load_receipt(&identity, &digest)
        .map_err(storage_failure)?
        .is_some()
    {
        return pending_batch_sources(storage, proposed);
    }
    let history_stream = provider_history_stream(&proposed[0]);
    for _ in 0..8 {
        let (mut pending, pending_revision) = load_pending_provider_frames(storage)?;
        let (mut history, history_revision) =
            load_provider_history(storage, &history_stream, &proposed[0])?;
        let mut ordered = proposed.to_vec();
        ordered.sort_by(provider_raw_order);
        let mut added = false;
        for proposed_source in ordered {
            let source_key = provider_source_key(&proposed_source);
            let body_sha256 = provider_source_digest(&proposed_source)?;
            if let Some(entry) = history.entries.get(&source_key) {
                if entry.body_sha256 != body_sha256 {
                    return Err(storage_failure(StorageError::invalid_input(
                        "Provider frame source differs from exact replay",
                    )));
                }
                if let Some(existing) = pending.frames.get(&source_key)
                    && !provider_source_body_matches(existing, &proposed_source)
                {
                    return Err(storage_failure(StorageError::invalid_input(
                        "pending Provider frame differs from exact history",
                    )));
                }
                continue;
            }
            if pending.frames.len() >= MAX_PENDING_PROVIDER_FRAMES
                || history.entries.len() >= MAX_PROVIDER_FRAME_HISTORY
            {
                return Err(storage_failure(StorageError::adapter(
                    "Provider frame recovery capacity is exhausted",
                )));
            }
            let mut source = proposed_source;
            source.public_stream_sequence =
                next_public_stream_sequence(storage, &pending, &history, &source)?;
            history.entries.insert(
                source_key.clone(),
                ProviderPublicFrameHistoryEntry {
                    body_sha256,
                    public_stream_sequence: source.public_stream_sequence,
                },
            );
            pending.frames.insert(source_key, source);
            added = true;
        }
        if !added {
            return pending_batch_sources(storage, proposed);
        }
        let history_payload = encode_provider_history(&history)?;
        let mutation =
            StateMutation::new(history_stream.clone(), history_revision, history_payload)
                .map_err(storage_failure)?;
        let commit = StateCommit::new(
            identity.clone(),
            digest.clone(),
            MODEL_FRAME_PENDING_STREAM,
            pending_revision,
            encode_pending_provider_frames(&pending)?,
            vec![internal_execution_event(&request_id)],
        )
        .with_state_mutation(mutation);
        match storage.commit(&commit) {
            Ok(_) => return pending_batch_sources(storage, proposed),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                if storage
                    .load_receipt(&identity, &digest)
                    .map_err(storage_failure)?
                    .is_some()
                {
                    return pending_batch_sources(storage, proposed);
                }
            }
            Err(error) => return Err(storage_failure(error)),
        }
    }
    Err(storage_failure(StorageError::adapter(
        "pending Provider batch recovery remained concurrent",
    )))
}

fn pending_batch_sources(
    storage: &SqliteStorage,
    proposed: &[DurableProviderPublicFrame],
) -> Result<Vec<DurableProviderPublicFrame>, ProductSessionExecutionApplicationError> {
    let (pending, _) = load_pending_provider_frames(storage)?;
    proposed
        .iter()
        .filter_map(|source| {
            pending
                .frames
                .get(&provider_source_key(source))
                .map(|existing| (existing, source))
        })
        .map(|(existing, proposed)| {
            if provider_source_body_matches(existing, proposed) {
                Ok(existing.clone())
            } else {
                Err(storage_failure(StorageError::invalid_input(
                    "pending Provider frame differs from exact replay",
                )))
            }
        })
        .collect()
}

fn validate_provider_batch_sources(
    proposed: &[DurableProviderPublicFrame],
) -> Result<(), ProductSessionExecutionApplicationError> {
    let first = &proposed[0];
    if proposed.iter().any(|source| {
        source.repository_scope != first.repository_scope
            || source.product_session_id != first.product_session_id
            || source.execution_job_id != first.execution_job_id
            || source.model_exchange_id != first.model_exchange_id
    }) {
        return Err(storage_failure(StorageError::invalid_input(
            "canonical Provider batch crosses one Chat authority",
        )));
    }
    let mut raw_sequences = proposed
        .iter()
        .map(|source| source.chunk.sequence.0)
        .collect::<Vec<_>>();
    raw_sequences.sort_unstable();
    if raw_sequences.first().is_some_and(|sequence| *sequence <= 0)
        || raw_sequences.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(storage_failure(StorageError::invalid_input(
            "canonical Provider batch has duplicate or invalid sequence",
        )));
    }
    Ok(())
}

fn provider_batch_request_id(
    proposed: &[DurableProviderPublicFrame],
) -> Result<RequestId, ProductSessionExecutionApplicationError> {
    serde_json::to_vec(
        &proposed
            .iter()
            .map(|source| &source.chunk.message_id)
            .collect::<Vec<_>>(),
    )
    .map(|bytes| derived_request_id(MODEL_FRAME_SOURCE_RECEIPT_NAMESPACE, &bytes))
    .map_err(|_| {
        storage_failure(StorageError::adapter(
            "Provider batch identity cannot encode",
        ))
    })
}

fn provider_batch_digest(
    proposed: &[DurableProviderPublicFrame],
) -> Result<Sha256Digest, ProductSessionExecutionApplicationError> {
    let digests = proposed
        .iter()
        .map(provider_source_digest)
        .collect::<Result<Vec<_>, _>>()?;
    serde_json::to_vec(&digests)
        .map(|bytes| Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
        .map_err(|_| storage_failure(StorageError::adapter("Provider batch digest cannot encode")))
}

fn provider_raw_order(
    left: &DurableProviderPublicFrame,
    right: &DurableProviderPublicFrame,
) -> std::cmp::Ordering {
    left.chunk.sequence.0.cmp(&right.chunk.sequence.0)
}

fn provider_projection_order(
    left: &DurableProviderPublicFrame,
    right: &DurableProviderPublicFrame,
) -> std::cmp::Ordering {
    left.product_session_id
        .0
        .cmp(&right.product_session_id.0)
        .then_with(|| left.model_exchange_id.0.cmp(&right.model_exchange_id.0))
        .then_with(|| {
            left.public_stream_sequence
                .cmp(&right.public_stream_sequence)
        })
        .then_with(|| provider_raw_order(left, right))
}

fn next_public_stream_sequence(
    storage: &mut SqliteStorage,
    pending: &PendingProviderPublicFrames,
    history: &ProviderPublicFrameHistory,
    source: &DurableProviderPublicFrame,
) -> Result<u64, ProductSessionExecutionApplicationError> {
    let scope_key = repository_scope_key(&source.repository_scope).map_err(storage_failure)?;
    let durable_last = ProductSessionService::new(storage)
        .last_assistant_stream_sequence(
            &scope_key,
            &source.product_session_id,
            &source.model_exchange_id,
        )
        .map_err(product_session_failure)?;
    let pending_last = pending
        .frames
        .values()
        .filter(|candidate| {
            candidate.product_session_id == source.product_session_id
                && candidate.model_exchange_id == source.model_exchange_id
        })
        .map(|candidate| candidate.public_stream_sequence)
        .max()
        .unwrap_or(0);
    let history_last = history
        .entries
        .values()
        .map(|entry| entry.public_stream_sequence)
        .max()
        .unwrap_or(0);
    durable_last
        .max(pending_last)
        .max(history_last)
        .checked_add(1)
        .filter(|sequence| *sequence <= 9_007_199_254_740_991)
        .ok_or_else(|| {
            storage_failure(StorageError::invalid_input(
                "public assistant stream sequence overflowed",
            ))
        })
}

fn project_provider_source(
    storage: &mut SqliteStorage,
    source: &DurableProviderPublicFrame,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let staged = load_staged_binding_for_projection(storage, &source.execution_job_id)?;
    validate_provider_source(&staged, source)?;
    let scope_key = repository_scope_key(&source.repository_scope).map_err(storage_failure)?;
    let record = ProductSessionService::new(storage)
        .get(&scope_key, &source.product_session_id)
        .map_err(product_session_failure)?
        .ok_or_else(|| storage_failure(StorageError::invalid_input("ProductSession missing")))?;
    let turn = record
        .turn_intents()
        .iter()
        .find(|turn| turn.execution_job_id == source.execution_job_id)
        .ok_or_else(|| storage_failure(StorageError::invalid_input("Chat turn missing")))?;
    let binding = record
        .bindings()
        .iter()
        .find(|binding| {
            binding.binding().identity().execution_job_id() == &source.execution_job_id
                && binding.model_exchange_id() == &source.model_exchange_id
        })
        .ok_or_else(|| storage_failure(StorageError::invalid_input("model binding missing")))?;
    validate_chunk_binding(&source.chunk, binding)?;
    let context = internal_context(
        &source.repository_scope,
        &derived_request_id(
            MODEL_FRAME_RECEIPT_NAMESPACE,
            source.chunk.message_id.0.as_bytes(),
        ),
        turn.session_revision,
        &source.chunk.sent_at,
    )?;
    let command = AppendAssistantMessageCommand {
        context,
        product_session_id: source.product_session_id.clone(),
        binding_identity: binding.binding().identity().clone(),
        runtime_authority: binding.slot().authority.clone(),
        execution_scope: binding.execution_scope().clone(),
        worker_pool_id: binding.worker_pool_id().clone(),
        model_exchange_id: source.model_exchange_id.clone(),
        stream_sequence: source.public_stream_sequence,
        public_text_delta: source.public_text_delta.clone(),
        state: crate::AssistantMessageState::Streaming,
        terminal_outcome: None,
    };
    ProductSessionService::new(storage)
        .append_assistant_message(&command)
        .map_err(product_session_failure)?;
    remove_projected_provider_source(storage, source)
}

fn remove_projected_provider_source(
    storage: &mut SqliteStorage,
    source: &DurableProviderPublicFrame,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let request_id = derived_request_id(
        MODEL_FRAME_PROJECTED_RECEIPT_NAMESPACE,
        source.chunk.message_id.0.as_bytes(),
    );
    let identity = application_receipt_identity(&source.repository_scope, &request_id)?;
    let digest = provider_source_digest(source)?;
    if storage
        .load_receipt(&identity, &digest)
        .map_err(storage_failure)?
        .is_some()
    {
        return Ok(());
    }
    let source_key = provider_source_key(source);
    for _ in 0..8 {
        let (mut pending, revision) = load_pending_provider_frames(storage)?;
        match pending.frames.get(&source_key) {
            Some(existing) if existing == source => {}
            Some(_) => {
                return Err(storage_failure(StorageError::invalid_input(
                    "pending Provider frame differs before completion",
                )));
            }
            None => {
                return Err(storage_failure(StorageError::adapter(
                    "pending Provider frame disappeared before completion",
                )));
            }
        }
        pending.frames.remove(&source_key);
        let commit = StateCommit::new(
            identity.clone(),
            digest.clone(),
            MODEL_FRAME_PENDING_STREAM,
            revision,
            encode_pending_provider_frames(&pending)?,
            vec![internal_execution_event(&request_id)],
        );
        match storage.commit(&commit) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                if storage
                    .load_receipt(&identity, &digest)
                    .map_err(storage_failure)?
                    .is_some()
                {
                    return Ok(());
                }
            }
            Err(error) => return Err(storage_failure(error)),
        }
    }
    Err(storage_failure(StorageError::adapter(
        "Provider frame completion remained concurrent",
    )))
}

fn load_pending_provider_frames(
    storage: &SqliteStorage,
) -> Result<(PendingProviderPublicFrames, u64), ProductSessionExecutionApplicationError> {
    let Some(state) = storage
        .load_state(MODEL_FRAME_PENDING_STREAM)
        .map_err(storage_failure)?
    else {
        return Ok((
            PendingProviderPublicFrames {
                schema: "winwincode.product-session-provider-frame-pending.v1".to_owned(),
                frames: BTreeMap::new(),
            },
            0,
        ));
    };
    let pending: PendingProviderPublicFrames =
        serde_json::from_slice(&state.payload).map_err(|_| {
            storage_failure(StorageError::adapter("pending Provider frames are corrupt"))
        })?;
    if pending.schema != "winwincode.product-session-provider-frame-pending.v1"
        || pending.frames.len() > MAX_PENDING_PROVIDER_FRAMES
        || state.payload.len() > MAX_PENDING_PROVIDER_FRAME_BYTES
        || pending.frames.iter().any(|(key, source)| {
            key != &provider_source_key(source) || source.public_stream_sequence == 0
        })
    {
        return Err(storage_failure(StorageError::adapter(
            "pending Provider frame catalog is invalid",
        )));
    }
    Ok((pending, state.revision))
}

fn encode_pending_provider_frames(
    pending: &PendingProviderPublicFrames,
) -> Result<Vec<u8>, ProductSessionExecutionApplicationError> {
    if pending.frames.len() > MAX_PENDING_PROVIDER_FRAMES {
        return Err(storage_failure(StorageError::adapter(
            "pending Provider frame recovery capacity is exhausted",
        )));
    }
    let payload = serde_json::to_vec(pending).map_err(|_| {
        storage_failure(StorageError::adapter(
            "pending Provider frames cannot encode",
        ))
    })?;
    if payload.len() > MAX_PENDING_PROVIDER_FRAME_BYTES {
        return Err(storage_failure(StorageError::adapter(
            "pending Provider frame recovery bytes are exhausted",
        )));
    }
    Ok(payload)
}

impl<Downstream> DurableExecutionPortDelegate for ProductSessionExecutionApplication<Downstream>
where
    Downstream: DurableExecutionPortDelegate,
{
    fn accept(
        &mut self,
        mut context: DurableExecutionPortContext<'_>,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        match supplement {
            DurableExecutionPortSupplement::ProductSessionBinding {
                job,
                dispatch,
                message,
            } => {
                accept_worker_binding(&mut context, job, dispatch, message)?;
                Ok(Vec::new())
            }
            DurableExecutionPortSupplement::ProductSessionOutcome {
                job,
                dispatch,
                message,
            } => accept_terminal(&mut context, job, dispatch, message),
            DurableExecutionPortSupplement::JobScopedWorkerMessage { dispatch, message } => {
                if let ExecutionPortMessage::ActionEnforcementRequestMessage(request) = message
                    && let Some(issuer) = self.action_issuer.as_ref()
                {
                    let server_time = context.server_time().clone();
                    let receipt = issue_action_enforcement_receipt(
                        context.storage(),
                        issuer,
                        &server_time,
                        request,
                    )
                    .map_err(|error| {
                        DurableExecutionPortError::Service(ExecutionPortServiceError::ActionPolicy(
                            error,
                        ))
                    })?;
                    return Ok(vec![ExecutionPortMessage::ActionEnforcementReceiptMessage(
                        receipt,
                    )]);
                }
                if let ExecutionPortMessage::ArtifactOpenMessage(artifact) = message {
                    let repository_scope = context.repository_scope().clone();
                    let authority = seal_session_binding_authority(dispatch);
                    let acknowledgement = context
                        .control_plane()
                        .accept_artifact_open(&repository_scope, artifact, &authority)
                        .map_err(|_| {
                            DurableExecutionPortError::Storage(StorageError::adapter(
                                "Artifact open was rejected by the canonical store",
                            ))
                        })?;
                    return Ok(vec![ExecutionPortMessage::ArtifactAckMessage(
                        acknowledgement,
                    )]);
                }
                if let ExecutionPortMessage::ArtifactChunkMessage(artifact) = message {
                    let repository_scope = context.repository_scope().clone();
                    let authority = seal_session_binding_authority(dispatch);
                    let acknowledgement = context
                        .control_plane()
                        .accept_artifact_chunk(&repository_scope, artifact, &authority)
                        .map_err(|_| {
                            DurableExecutionPortError::Storage(StorageError::adapter(
                                "Artifact chunk was rejected by the canonical store",
                            ))
                        })?;
                    return Ok(vec![ExecutionPortMessage::ArtifactAckMessage(
                        acknowledgement,
                    )]);
                }
                if let ExecutionPortMessage::InputRequestMessage(request) = message {
                    accept_input_request(&mut context, dispatch, request)?;
                    return Ok(Vec::new());
                }
                if let ExecutionPortMessage::ApprovalRequestMessage(request) = message {
                    accept_approval_request(&mut context, dispatch, request)?;
                    return Ok(Vec::new());
                }
                if let ExecutionPortMessage::ModelOpenMessage(open) = message
                    && open.session_identity.stage_run_id.is_none()
                {
                    attach_model_exchange(&mut context, dispatch, open)?;
                }
                self.downstream.accept(context, supplement)
            }
            DurableExecutionPortSupplement::WorkerMessage(_) => {
                self.downstream.accept(context, supplement)
            }
        }
    }
}

fn accept_input_request(
    context: &mut DurableExecutionPortContext<'_>,
    dispatch: &ExecutionDispatchAuthority,
    request: &InputRequestMessage,
) -> Result<(), DurableExecutionPortError> {
    let authority = product_interaction_authority(
        context,
        dispatch,
        &ExecutionPortMessage::InputRequestMessage(request.clone()),
    )?;
    let trusted_now = context.server_time().clone();
    ChatInteractionService::new(context.storage())
        .record_input_at(
            &RecordInputInteractionCommand {
                authority,
                request: request.clone(),
            },
            &trusted_now,
        )
        .map(|_| ())
        .map_err(interaction_ingress)
}

fn accept_approval_request(
    context: &mut DurableExecutionPortContext<'_>,
    dispatch: &ExecutionDispatchAuthority,
    request: &ApprovalRequestMessage,
) -> Result<(), DurableExecutionPortError> {
    let _authority = product_interaction_authority(
        context,
        dispatch,
        &ExecutionPortMessage::ApprovalRequestMessage(request.clone()),
    )?;
    let trusted_now = context.server_time().clone();
    let public_scope = public_repository_scope(context.repository_scope());
    ChatInteractionService::new(context.storage())
        .record_approval_at(
            &RecordApprovalInteractionCommand {
                public_scope,
                request: request.clone(),
            },
            &trusted_now,
        )
        .map(|_| ())
        .map_err(interaction_ingress)
}

fn product_interaction_authority(
    context: &mut DurableExecutionPortContext<'_>,
    dispatch: &ExecutionDispatchAuthority,
    message: &ExecutionPortMessage,
) -> Result<WorkerInteractionAuthority, DurableExecutionPortError> {
    let (lease, session_identity, worker_session_id) = match message {
        ExecutionPortMessage::InputRequestMessage(request) => (
            &request.lease,
            &request.session_identity,
            &request.worker_session_id,
        ),
        ExecutionPortMessage::ApprovalRequestMessage(request) => (
            &request.lease,
            &request.session_identity,
            &request.worker_session_id,
        ),
        _ => return Err(DurableExecutionPortError::UnsupportedMessage),
    };
    let (_, job) = load_durable_execution_job(context.storage(), &lease.job_id)
        .map_err(DurableExecutionPortError::Storage)?;
    let ExecutionScope::ProductSessionExecutionScope(job_scope) = &job.scope else {
        return Err(storage_ingress(
            "Worker interaction belongs to another Job scope",
        ));
    };
    if session_identity.stage_run_id.is_some()
        || session_identity.product_session_id != job_scope.product_session_id
        || worker_session_id != dispatch.worker_session_id()
        || session_identity.worker_session_id != *worker_session_id
        || lease != &lease_stamp(dispatch.lease())
    {
        return Err(storage_ingress(
            "Worker interaction identity differs from its accepted dispatch",
        ));
    }
    let staged = load_staged_binding(context.storage(), &binding_stream_id(&job.job_id))?;
    if staged.product_session_id != job_scope.product_session_id
        || staged.execution_job_id != job.job_id
        || staged.runtime_authority.worker_session_id != *worker_session_id
        || staged.runtime_authority.codex_thread_id != session_identity.codex_thread_id
        || staged.runtime_authority
            != runtime_authority_from_dispatch(
                dispatch,
                worker_session_id,
                &session_identity.codex_thread_id,
            )?
    {
        return Err(storage_ingress(
            "Worker interaction differs from staged Chat binding",
        ));
    }
    let repository_scope = context.repository_scope().clone();
    let scope_key =
        repository_scope_key(&repository_scope).map_err(DurableExecutionPortError::Storage)?;
    let record = ProductSessionService::new(context.storage())
        .get(&scope_key, &staged.product_session_id)
        .map_err(|error| product_session_ingress(&error))?
        .ok_or_else(|| storage_ingress("Worker interaction ProductSession is missing"))?;
    let job_record = context
        .storage()
        .load_execution_job_record(&job.job_id)
        .map_err(DurableExecutionPortError::Storage)?
        .ok_or_else(|| storage_ingress("Worker interaction ExecutionJob is missing"))?;
    if job_record.scope != staged.execution_scope {
        return Err(storage_ingress(
            "Worker interaction ExecutionJob scope differs",
        ));
    }
    let slot = context
        .storage()
        .worker_session_slots()
        .map_err(worker_slot_storage)?
        .load(worker_session_id)
        .map_err(worker_slot_storage)?
        .ok_or_else(|| storage_ingress("Worker interaction Worker slot is missing"))?;
    if slot.state != WorkerSlotState::Running || slot.authority != staged.runtime_authority {
        return Err(storage_ingress("Worker interaction Worker slot differs"));
    }
    Ok(WorkerInteractionAuthority {
        execution_scope: staged.execution_scope,
        worker_pool_id: staged.worker_pool_id,
        product_session_revision: record.session().revision(),
        job_revision: job_record.revision,
        worker_slot_revision: slot.revision,
    })
}

fn runtime_authority_from_dispatch(
    dispatch: &ExecutionDispatchAuthority,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    codex_thread_id: &winwincode_domain::CodexThreadId,
) -> Result<WorkerSlotAuthority, DurableExecutionPortError> {
    let attempt = dispatch.lease().attempt;
    if attempt == 0 {
        return Err(storage_ingress(
            "Worker interaction lease attempt is invalid",
        ));
    }
    Ok(WorkerSlotAuthority {
        worker_id: dispatch.lease().worker_id.clone(),
        worker_instance_id: dispatch.lease().worker_instance_id.clone(),
        worker_session_id: worker_session_id.clone(),
        codex_thread_id: codex_thread_id.clone(),
        job_id: dispatch.lease().job_id.clone(),
        lease_id: dispatch.lease().lease_id.clone(),
        attempt,
        fencing_token: dispatch.lease().fencing_token.clone(),
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StagedWorkerBinding {
    product_session_id: ProductSessionId,
    execution_scope: ExecutionQueueScope,
    worker_pool_id: WorkerPoolId,
    execution_job_id: ExecutionJobId,
    runtime_authority: WorkerSlotAuthority,
    bound_at: Instant,
    source_message_id: ExecutionMessageId,
}

fn accept_worker_binding(
    context: &mut DurableExecutionPortContext<'_>,
    job: &ExecutionJob,
    dispatch: &ExecutionDispatchAuthority,
    message: &SessionBindingMessage,
) -> Result<(), DurableExecutionPortError> {
    let request_id = derived_request_id(BINDING_RECEIPT_NAMESPACE, message.message_id.0.as_bytes());
    let identity = internal_receipt_identity(context.repository_scope(), &request_id)?;
    let digest = json_digest(&("product-session-worker-binding.v1", job, message))?;
    let stream_id = binding_stream_id(&job.job_id);
    if let Some(receipt) = context
        .storage()
        .load_receipt(&identity, &digest)
        .map_err(DurableExecutionPortError::Storage)?
    {
        if receipt.stream_id != stream_id {
            return Err(storage_ingress("binding receipt stream differs"));
        }
        return Ok(());
    }
    context.validate_first_seen_dispatch(dispatch)?;
    let staged = staged_binding(context, job, dispatch, message)?;
    let current = context
        .storage()
        .load_state(&stream_id)
        .map_err(DurableExecutionPortError::Storage)?;
    let expected_revision = if let Some(current) = current {
        let previous = decode_staged_binding(&current.payload)?;
        let replacement = context
            .storage()
            .load_execution_scope_replacement_authority(&job.job_id)
            .map_err(DurableExecutionPortError::Storage)?
            .ok_or_else(|| storage_ingress("Chat binding replacement authority is missing"))?;
        require_staged_replacement(&previous, &staged, dispatch, &replacement)?;
        current.revision
    } else {
        0
    };
    let bytes =
        serde_json::to_vec(&staged).map_err(|_| storage_ingress("binding encode failed"))?;
    context
        .storage()
        .commit(&StateCommit::new(
            identity,
            digest,
            stream_id,
            expected_revision,
            bytes,
            vec![internal_execution_event(&request_id)],
        ))
        .map_err(DurableExecutionPortError::Storage)?;
    Ok(())
}

fn attach_model_exchange(
    context: &mut DurableExecutionPortContext<'_>,
    dispatch: &ExecutionDispatchAuthority,
    message: &ModelOpenMessage,
) -> Result<(), DurableExecutionPortError> {
    let repository_scope = context.repository_scope().clone();
    let scope_key =
        repository_scope_key(&repository_scope).map_err(DurableExecutionPortError::Storage)?;
    let stream_id = binding_stream_id(&message.lease.job_id);
    let staged = load_staged_binding(context.storage(), &stream_id)?;
    if staged.runtime_authority.job_id != message.lease.job_id
        || staged.runtime_authority.worker_session_id != message.worker_session_id
        || staged.product_session_id != message.session_identity.product_session_id
        || message.session_identity.stage_run_id.is_some()
    {
        return Err(storage_ingress(
            "model.open differs from staged Chat binding",
        ));
    }
    let record = ProductSessionService::new(context.storage())
        .get(&scope_key, &staged.product_session_id)
        .map_err(|error| product_session_ingress(&error))?
        .ok_or_else(|| storage_ingress("ProductSession binding target is missing"))?;
    let turn = record
        .turn_intents()
        .iter()
        .find(|turn| turn.execution_job_id == message.lease.job_id)
        .ok_or_else(|| storage_ingress("ProductSession Chat turn is missing"))?;
    let command_context = internal_context(
        &repository_scope,
        &derived_request_id(
            MODEL_BINDING_RECEIPT_NAMESPACE,
            message.message_id.0.as_bytes(),
        ),
        turn.session_revision,
        context.server_time(),
    )
    .map_err(application_ingress)?;
    let binding_identity = SessionBindingIdentity::product_session(
        staged.product_session_id.clone(),
        staged.execution_job_id.clone(),
    )
    .map_err(|_| storage_ingress("staged Chat binding identity is invalid"))?;
    let replacement = context
        .storage()
        .load_execution_scope_replacement_authority(&message.lease.job_id)
        .map_err(DurableExecutionPortError::Storage)?
        .filter(|authority| {
            i64::try_from(authority.replacement_attempt()).ok() == Some(message.lease.attempt)
        })
        .filter(|authority| authority.predecessor_slot().is_some());
    if let Some(replacement) = replacement {
        let command = ReplaceProductSessionExecutionBindingCommand {
            context: command_context,
            product_session_id: staged.product_session_id,
            binding_identity,
            runtime_authority: staged.runtime_authority,
            execution_scope: staged.execution_scope,
            worker_pool_id: staged.worker_pool_id,
            model_exchange_id: message.model_exchange_id.clone(),
            replacement,
        };
        let replay = ProductSessionService::new(context.storage())
            .replay_replace_execution_binding(&command)
            .map_err(|error| product_session_ingress(&error))?;
        if replay.is_none() {
            context.validate_first_seen_dispatch(dispatch)?;
            ProductSessionService::new(context.storage())
                .replace_execution_binding(&command)
                .map_err(|error| product_session_ingress(&error))?;
        }
    } else {
        let command = ContinueProductSessionCommand {
            context: command_context,
            product_session_id: staged.product_session_id,
            binding_identity,
            runtime_authority: staged.runtime_authority,
            execution_scope: staged.execution_scope,
            worker_pool_id: staged.worker_pool_id,
            model_exchange_id: message.model_exchange_id.clone(),
        };
        let replay = ProductSessionService::new(context.storage())
            .replay_continue_session(&command)
            .map_err(|error| product_session_ingress(&error))?;
        if replay.is_none() {
            context.validate_first_seen_dispatch(dispatch)?;
            ProductSessionService::new(context.storage())
                .continue_session(&command)
                .map_err(|error| product_session_ingress(&error))?;
        }
    }
    Ok(())
}

fn accept_terminal(
    context: &mut DurableExecutionPortContext<'_>,
    job: &ExecutionJob,
    dispatch: &ExecutionDispatchAuthority,
    message: &JobOutcomeMessage,
) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
    let repository_scope = context.repository_scope().clone();
    let scope_key =
        repository_scope_key(&repository_scope).map_err(DurableExecutionPortError::Storage)?;
    let ExecutionScope::ProductSessionExecutionScope(job_scope) = &job.scope else {
        return Err(storage_ingress(
            "ProductSession outcome has another Job scope",
        ));
    };
    let record = ProductSessionService::new(context.storage())
        .get(&scope_key, &job_scope.product_session_id)
        .map_err(|error| product_session_ingress(&error))?
        .ok_or_else(|| storage_ingress("ProductSession outcome target is missing"))?;
    let turn = record
        .turn_intents()
        .iter()
        .find(|turn| turn.execution_job_id == job.job_id)
        .ok_or_else(|| storage_ingress("ProductSession outcome turn is missing"))?;
    let binding = record
        .bindings()
        .iter()
        .find(|binding| binding.binding().identity().execution_job_id() == &job.job_id)
        .ok_or_else(|| storage_ingress("ProductSession outcome binding is missing"))?;
    let terminal_outcome = ProductSessionTurnTerminalOutcome {
        status: message.outcome.status.clone(),
        usage: message.outcome.usage.clone(),
        last_event_sequence: message.outcome.last_event_sequence.clone(),
        finished_at: message.outcome.finished_at.clone(),
    };
    let command = RecordAssistantTerminalCommand {
        context: internal_context(
            &repository_scope,
            &derived_request_id(TERMINAL_RECEIPT_NAMESPACE, message.message_id.0.as_bytes()),
            turn.session_revision,
            context.server_time(),
        )
        .map_err(application_ingress)?,
        product_session_id: job_scope.product_session_id.clone(),
        binding_identity: binding.binding().identity().clone(),
        runtime_authority: binding.slot().authority.clone(),
        execution_scope: binding.execution_scope().clone(),
        worker_pool_id: binding.worker_pool_id().clone(),
        model_exchange_id: binding.model_exchange_id().clone(),
        terminal_outcome,
    };
    let replay = ProductSessionService::new(context.storage())
        .replay_assistant_terminal(&command)
        .map_err(|error| product_session_ingress(&error))?;
    let replayed = replay.is_some();
    if !replayed {
        context.validate_first_seen_dispatch(dispatch)?;
        reconcile_product_session_model_exchange(
            context.storage(),
            &job.job_id,
            binding.model_exchange_id(),
        )
        .map_err(application_ingress)?;
        ProductSessionService::new(context.storage())
            .record_assistant_terminal(&command)
            .map_err(|error| product_session_ingress(&error))?;
    }
    finish_execution_resources(
        context.storage(),
        message,
        binding.execution_scope(),
        binding.worker_pool_id(),
        &binding.slot().authority,
    )?;
    Ok(product_session_outcome_output(message, replayed))
}

pub(crate) fn finish_execution_resources(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    execution_scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    authority: &WorkerSlotAuthority,
) -> Result<(), DurableExecutionPortError> {
    let data_directory = storage
        .database_path()
        .parent()
        .ok_or_else(|| storage_ingress("Control Plane data directory is invalid"))?
        .to_path_buf();
    let lifecycle = DurableWorkerExecutionLifecycle::open(data_directory)
        .map_err(|error| worker_lifecycle_ingress(&error))?;
    let enterprise_terminal = match message.outcome.status {
        ExecutionOutcomeStatus::Succeeded => lifecycle
            .settle_terminal_outcome(message)
            .map_err(|error| worker_lifecycle_ingress(&error))?,
        ExecutionOutcomeStatus::Cancelled
        | ExecutionOutcomeStatus::Failed
        | ExecutionOutcomeStatus::InfrastructureError => lifecycle
            .release_terminal_outcome(message)
            .map_err(|error| worker_lifecycle_ingress(&error))?,
    };
    if enterprise_terminal.is_none() {
        finish_local_admission(storage, message, execution_scope, worker_pool_id)?;
    }
    finish_worker_slot(storage, message, authority)?;
    finish_queue_and_registry_lease(storage, message, execution_scope, authority)?;
    Ok(())
}

fn finish_local_admission(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    execution_scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) -> Result<(), DurableExecutionPortError> {
    let current = storage
        .execution_admission()
        .map_err(|_| storage_ingress("local execution admission cannot be opened"))?
        .load_reservation_by_job(&message.lease.job_id)
        .map_err(|_| storage_ingress("local execution admission cannot be read"))?
        .ok_or_else(|| storage_ingress("local execution admission is missing"))?;
    match message.outcome.status {
        ExecutionOutcomeStatus::Succeeded => {
            if current.state == ExecutionReservationState::Settled {
                return Ok(());
            }
            let usage = message
                .outcome
                .usage
                .as_ref()
                .ok_or_else(|| storage_ingress("successful local outcome has no Usage"))?;
            storage
                .execution_admission()
                .map_err(|_| storage_ingress("local execution admission cannot be opened"))?
                .settle(&ExecutionReservationSettlement {
                    scope: execution_scope.clone(),
                    worker_pool_id: worker_pool_id.clone(),
                    job_id: message.lease.job_id.clone(),
                    request_id: derived_request_id(
                        b"local-admission-settle",
                        message.message_id.0.as_bytes(),
                    ),
                    expected_revision: current.revision,
                    actual_tokens: u64::try_from(usage.tokens)
                        .map_err(|_| storage_ingress("local Usage tokens are invalid"))?,
                    actual_cost_microunits: u64::try_from(usage.cost_microunits)
                        .map_err(|_| storage_ingress("local Usage cost is invalid"))?,
                    actual_runtime_millis: u64::try_from(usage.runtime_millis)
                        .map_err(|_| storage_ingress("local Usage runtime is invalid"))?,
                    completed_at: message.outcome.finished_at.clone(),
                })
                .map_err(|_| storage_ingress("local execution admission cannot settle"))?;
        }
        ExecutionOutcomeStatus::Cancelled
        | ExecutionOutcomeStatus::Failed
        | ExecutionOutcomeStatus::InfrastructureError => {
            if current.state == ExecutionReservationState::Released {
                return Ok(());
            }
            storage
                .execution_admission()
                .map_err(|_| storage_ingress("local execution admission cannot be opened"))?
                .release(&ExecutionReservationRelease {
                    scope: execution_scope.clone(),
                    worker_pool_id: worker_pool_id.clone(),
                    job_id: message.lease.job_id.clone(),
                    request_id: derived_request_id(
                        b"local-admission-release",
                        message.message_id.0.as_bytes(),
                    ),
                    expected_revision: current.revision,
                    reason: match message.outcome.status {
                        ExecutionOutcomeStatus::Cancelled => {
                            ExecutionReservationReleaseReason::Cancelled
                        }
                        ExecutionOutcomeStatus::Failed
                        | ExecutionOutcomeStatus::InfrastructureError => {
                            ExecutionReservationReleaseReason::Failed
                        }
                        ExecutionOutcomeStatus::Succeeded => unreachable!(),
                    },
                    released_at: message.outcome.finished_at.clone(),
                })
                .map_err(|_| storage_ingress("local execution admission cannot release"))?;
        }
    }
    Ok(())
}

fn finish_worker_slot(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    authority: &WorkerSlotAuthority,
) -> Result<(), DurableExecutionPortError> {
    let current = storage
        .worker_session_slots()
        .map_err(worker_slot_storage)?
        .load(&authority.worker_session_id)
        .map_err(worker_slot_storage)?
        .ok_or_else(|| storage_ingress("Worker slot is missing at terminal"))?;
    let outcome = match message.outcome.status {
        ExecutionOutcomeStatus::Succeeded => WorkerSlotState::Completed,
        ExecutionOutcomeStatus::Cancelled => WorkerSlotState::Cancelled,
        ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
            WorkerSlotState::Failed
        }
    };
    if current.state == outcome {
        return Ok(());
    }
    let mut expected_revision = current.revision;
    if outcome == WorkerSlotState::Cancelled && current.state == WorkerSlotState::Running {
        let receipt = storage
            .worker_session_slots()
            .map_err(worker_slot_storage)?
            .request_cancellation(&WorkerSlotCancellation {
                authority: authority.clone(),
                request_id: derived_request_id(
                    SLOT_CANCEL_RECEIPT_NAMESPACE,
                    message.message_id.0.as_bytes(),
                ),
                expected_revision,
                requested_at: message.outcome.finished_at.clone(),
            })
            .map_err(worker_slot_storage)?;
        expected_revision = receipt.slot.revision;
    }
    storage
        .worker_session_slots()
        .map_err(worker_slot_storage)?
        .close(&WorkerSlotCloseRequest {
            authority: authority.clone(),
            request_id: derived_request_id(
                SLOT_CLOSE_RECEIPT_NAMESPACE,
                message.message_id.0.as_bytes(),
            ),
            expected_revision,
            outcome,
            closed_at: message.outcome.finished_at.clone(),
        })
        .map_err(worker_slot_storage)?;
    Ok(())
}

fn finish_queue_and_registry_lease(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    execution_scope: &ExecutionQueueScope,
    authority: &WorkerSlotAuthority,
) -> Result<(), DurableExecutionPortError> {
    let job_id = &authority.job_id;
    let current = storage
        .execution_queue()
        .and_then(|queue| queue.load_job(execution_scope, job_id))
        .map_err(|_| storage_ingress("Chat binding admission cannot be read"))?
        .ok_or_else(|| storage_ingress("ExecutionJob is missing at terminal"))?;
    let terminal_state = match message.outcome.status {
        ExecutionOutcomeStatus::Succeeded => ExecutionJobState::Completed,
        ExecutionOutcomeStatus::Cancelled
        | ExecutionOutcomeStatus::Failed
        | ExecutionOutcomeStatus::InfrastructureError => ExecutionJobState::Failed,
    };
    if current.state == terminal_state {
        return Ok(());
    }
    if !matches!(
        current.state,
        ExecutionJobState::Running | ExecutionJobState::Cancelling
    ) {
        return Err(storage_ingress(
            "ExecutionJob terminal source state is invalid",
        ));
    }
    storage
        .repository_scheduler()
        .and_then(|mut scheduler| {
            scheduler.settle_terminal(&RepositorySchedulerTerminalRequest {
                scope: RepositorySchedulerScope {
                    organization_id: execution_scope.organization_id.clone(),
                    workspace_id: execution_scope.workspace_id.clone(),
                    project_id: execution_scope.project_id.clone(),
                    repository_id: execution_scope.repository_id.clone(),
                },
                terminal: ExecutionLeaseTerminalRequest {
                    job_id: authority.job_id.clone(),
                    lease_id: authority.lease_id.clone(),
                    worker_id: authority.worker_id.clone(),
                    worker_instance_id: authority.worker_instance_id.clone(),
                    attempt: authority.attempt,
                    fencing_token: authority.fencing_token.clone(),
                    outcome: match message.outcome.status {
                        ExecutionOutcomeStatus::Succeeded => {
                            ExecutionLeaseTerminalOutcome::Completed
                        }
                        ExecutionOutcomeStatus::Cancelled => {
                            ExecutionLeaseTerminalOutcome::Cancelled
                        }
                        ExecutionOutcomeStatus::Failed
                        | ExecutionOutcomeStatus::InfrastructureError => {
                            ExecutionLeaseTerminalOutcome::Failed
                        }
                    },
                    terminal_at: message.outcome.finished_at.clone(),
                    request_id: derived_request_id(
                        EXECUTION_TERMINAL_RECEIPT_NAMESPACE,
                        message.message_id.0.as_bytes(),
                    ),
                },
            })
        })
        .map_err(DurableExecutionPortError::Storage)?;
    Ok(())
}

fn staged_binding(
    context: &mut DurableExecutionPortContext<'_>,
    job: &ExecutionJob,
    dispatch: &ExecutionDispatchAuthority,
    message: &SessionBindingMessage,
) -> Result<StagedWorkerBinding, DurableExecutionPortError> {
    let ExecutionScope::ProductSessionExecutionScope(job_scope) = &job.scope else {
        return Err(storage_ingress("Chat binding received another Job scope"));
    };
    if message.product_session_id != job_scope.product_session_id
        || message.session_identity.product_session_id != job_scope.product_session_id
        || message.stage_run_id.is_some()
        || message.session_identity.stage_run_id.is_some()
        || message.codex_thread_id != message.session_identity.codex_thread_id
        || message.worker_session_id != *dispatch.worker_session_id()
    {
        return Err(storage_ingress(
            "session.binding ProductSession identity differs",
        ));
    }
    let execution_scope =
        execution_scope(context.repository_scope(), &job_scope.product_session_id);
    let reservation = {
        let admission = context
            .storage()
            .execution_admission()
            .map_err(|_| storage_ingress("Chat binding admission cannot be opened"))?;
        admission
            .load_reservation_by_job(&job.job_id)
            .map_err(|_| storage_ingress("Chat binding admission cannot be read"))?
            .ok_or_else(|| storage_ingress("Chat binding admission is missing"))?
    };
    if reservation.scope != execution_scope {
        return Err(storage_ingress("Chat binding admission scope differs"));
    }
    let runtime_authority = WorkerSlotAuthority {
        worker_id: dispatch.lease().worker_id.clone(),
        worker_instance_id: dispatch.lease().worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        codex_thread_id: message.codex_thread_id.clone(),
        job_id: job.job_id.clone(),
        lease_id: dispatch.lease().lease_id.clone(),
        attempt: dispatch.lease().attempt,
        fencing_token: dispatch.lease().fencing_token.clone(),
    };
    let slot = context
        .storage()
        .worker_session_slots()
        .map_err(worker_slot_storage)?
        .load(&runtime_authority.worker_session_id)
        .map_err(worker_slot_storage)?
        .ok_or_else(|| storage_ingress("Chat binding Worker slot is missing"))?;
    if slot.authority != runtime_authority || slot.state != WorkerSlotState::Running {
        return Err(storage_ingress("Chat binding Worker slot differs"));
    }
    SessionBindingIdentity::product_session(
        job_scope.product_session_id.clone(),
        job.job_id.clone(),
    )
    .map_err(|_| storage_ingress("Chat binding identity is invalid"))?;
    Ok(StagedWorkerBinding {
        product_session_id: job_scope.product_session_id.clone(),
        execution_scope,
        worker_pool_id: reservation.worker_pool_id,
        execution_job_id: job.job_id.clone(),
        runtime_authority,
        bound_at: message.bound_at.clone(),
        source_message_id: message.message_id.clone(),
    })
}

fn execution_scope(
    repository: &RepositoryScope,
    product_session_id: &ProductSessionId,
) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: repository.organization_id.clone(),
        workspace_id: repository.workspace_id.clone(),
        project_id: repository.project_id.clone(),
        repository_id: repository.repository_id.clone(),
        product_session_id: product_session_id.clone(),
        delivery_id: None,
    }
}

fn require_staged_replacement(
    previous: &StagedWorkerBinding,
    successor: &StagedWorkerBinding,
    dispatch: &ExecutionDispatchAuthority,
    replacement: &ExecutionScopeReplacementAuthority,
) -> Result<(), DurableExecutionPortError> {
    let predecessor = replacement.predecessor_slot().ok_or_else(|| {
        storage_ingress("running Chat binding replacement has no predecessor slot")
    })?;
    if !replacement.authorizes_successor(dispatch)
        || replacement.scope() != &successor.execution_scope
        || replacement.stage_run_id().is_some()
        || previous.product_session_id != successor.product_session_id
        || previous.execution_scope != successor.execution_scope
        || previous.execution_job_id != successor.execution_job_id
        || previous.runtime_authority != *predecessor
        || replacement.previous_worker_session_id()
            != Some(&previous.runtime_authority.worker_session_id)
        || successor.runtime_authority.attempt != replacement.replacement_attempt()
    {
        return Err(storage_ingress(
            "sealed Chat binding replacement differs from its predecessor or successor",
        ));
    }
    Ok(())
}

fn decode_staged_binding(bytes: &[u8]) -> Result<StagedWorkerBinding, DurableExecutionPortError> {
    serde_json::from_slice(bytes).map_err(|_| storage_ingress("staged Chat binding is corrupt"))
}

fn load_staged_binding(
    storage: &mut SqliteStorage,
    stream_id: &str,
) -> Result<StagedWorkerBinding, DurableExecutionPortError> {
    let state = storage
        .load_state(stream_id)
        .map_err(DurableExecutionPortError::Storage)?
        .ok_or_else(|| storage_ingress("staged Chat binding is missing"))?;
    if state.revision == 0 {
        return Err(storage_ingress("staged Chat binding revision is invalid"));
    }
    decode_staged_binding(&state.payload)
}

fn load_staged_binding_for_projection(
    storage: &mut SqliteStorage,
    job: &ExecutionJobId,
) -> Result<StagedWorkerBinding, ProductSessionExecutionApplicationError> {
    find_staged_binding_for_projection(storage, job)?.ok_or_else(|| {
        storage_failure(StorageError::invalid_input(
            "ProductSession Provider batch has no staged Worker binding",
        ))
    })
}

fn find_staged_binding_for_projection(
    storage: &mut SqliteStorage,
    job: &ExecutionJobId,
) -> Result<Option<StagedWorkerBinding>, ProductSessionExecutionApplicationError> {
    let Some(state) = storage
        .load_state(&binding_stream_id(job))
        .map_err(storage_failure)?
    else {
        return Ok(None);
    };
    if state.revision == 0 {
        return Err(storage_failure(StorageError::adapter(
            "staged Chat binding revision is invalid",
        )));
    }
    serde_json::from_slice(&state.payload)
        .map(Some)
        .map_err(|_| storage_failure(StorageError::adapter("staged Chat binding is corrupt")))
}

fn binding_stream_id(job: &ExecutionJobId) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BINDING_STREAM_NAMESPACE);
    hasher.update([0]);
    hasher.update(job.0.as_bytes());
    format!("product-session-worker-binding:{:x}", hasher.finalize())
}

fn repository_scope_from_execution_scope(scope: &ExecutionQueueScope) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn validate_provider_source(
    staged: &StagedWorkerBinding,
    source: &DurableProviderPublicFrame,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let expected_scope = repository_scope_from_execution_scope(&staged.execution_scope);
    if source.schema != "winwincode.product-session-provider-frame.v1"
        || source.repository_scope != expected_scope
        || source.product_session_id != staged.product_session_id
        || source.execution_job_id != staged.execution_job_id
        || source.chunk.lease.job_id != staged.execution_job_id
        || source.chunk.model_exchange_id != source.model_exchange_id
        || source.chunk.session_identity.product_session_id != staged.product_session_id
        || source.chunk.session_identity.stage_run_id.is_some()
        || source.chunk.worker_session_id != staged.runtime_authority.worker_session_id
        || source.chunk.session_identity.worker_session_id
            != staged.runtime_authority.worker_session_id
        || source.chunk.session_identity.codex_thread_id != staged.runtime_authority.codex_thread_id
    {
        return Err(storage_failure(StorageError::invalid_input(
            "Provider frame source differs from the staged Chat binding",
        )));
    }
    let decoded_delta = public_text_delta(&source.chunk)?
        .ok_or(ProductSessionExecutionApplicationError::InvalidCanonicalFrame)?;
    if decoded_delta != source.public_text_delta {
        return Err(ProductSessionExecutionApplicationError::InvalidCanonicalFrame);
    }
    Ok(())
}

fn validate_chunk_binding(
    chunk: &ModelChunkMessage,
    binding: &crate::DurableSessionBinding,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let authority = &binding.slot().authority;
    let attempt = i64::try_from(authority.attempt)
        .map_err(|_| storage_failure(StorageError::adapter("stored Worker attempt is invalid")))?;
    if chunk.lease.job_id != authority.job_id
        || chunk.lease.lease_id != authority.lease_id
        || chunk.lease.worker_id != authority.worker_id
        || chunk.lease.worker_instance_id != authority.worker_instance_id
        || chunk.lease.attempt != attempt
        || chunk.lease.fencing_token != authority.fencing_token
        || chunk.worker_session_id != authority.worker_session_id
        || chunk.session_identity.worker_session_id != authority.worker_session_id
        || chunk.session_identity.codex_thread_id != authority.codex_thread_id
        || chunk.session_identity.product_session_id
            != *binding.binding().identity().product_session_id()
        || chunk.session_identity.stage_run_id.is_some()
        || chunk.model_exchange_id != *binding.model_exchange_id()
    {
        return Err(storage_failure(StorageError::invalid_input(
            "canonical Provider chunk differs from the ProductSession binding",
        )));
    }
    Ok(())
}

fn validate_bound_provider_source(
    storage: &mut SqliteStorage,
    source: &DurableProviderPublicFrame,
) -> Result<(), ProductSessionExecutionApplicationError> {
    let scope_key = repository_scope_key(&source.repository_scope).map_err(storage_failure)?;
    let record = ProductSessionService::new(storage)
        .get(&scope_key, &source.product_session_id)
        .map_err(product_session_failure)?
        .ok_or_else(|| storage_failure(StorageError::invalid_input("ProductSession missing")))?;
    let binding = record
        .bindings()
        .iter()
        .find(|binding| {
            binding.binding().identity().execution_job_id() == &source.execution_job_id
                && binding.model_exchange_id() == &source.model_exchange_id
        })
        .ok_or_else(|| storage_failure(StorageError::invalid_input("model binding missing")))?;
    validate_chunk_binding(&source.chunk, binding)
}

fn provider_source_key(source: &DurableProviderPublicFrame) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.product-session-provider-frame-source.v1");
    hasher.update([0]);
    hasher.update(source.execution_job_id.0.as_bytes());
    hasher.update([0]);
    hasher.update(source.model_exchange_id.0.as_bytes());
    hasher.update([0]);
    hasher.update(source.chunk.sequence.0.to_be_bytes());
    format!("provider-frame:{:x}", hasher.finalize())
}

fn provider_history_stream(source: &DurableProviderPublicFrame) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.product-session-provider-frame-history.v1");
    hasher.update([0]);
    hasher.update(source.execution_job_id.0.as_bytes());
    hasher.update([0]);
    hasher.update(source.model_exchange_id.0.as_bytes());
    format!(
        "product-session-provider-frame-history:{:x}",
        hasher.finalize()
    )
}

fn load_provider_history(
    storage: &SqliteStorage,
    stream_id: &str,
    source: &DurableProviderPublicFrame,
) -> Result<(ProviderPublicFrameHistory, u64), ProductSessionExecutionApplicationError> {
    let Some(state) = storage.load_state(stream_id).map_err(storage_failure)? else {
        return Ok((
            ProviderPublicFrameHistory {
                schema: "winwincode.product-session-provider-frame-history.v1".to_owned(),
                execution_job_id: source.execution_job_id.clone(),
                model_exchange_id: source.model_exchange_id.clone(),
                entries: BTreeMap::new(),
            },
            0,
        ));
    };
    let history: ProviderPublicFrameHistory = serde_json::from_slice(&state.payload)
        .map_err(|_| storage_failure(StorageError::adapter("Provider frame history is corrupt")))?;
    if history.schema != "winwincode.product-session-provider-frame-history.v1"
        || history.execution_job_id != source.execution_job_id
        || history.model_exchange_id != source.model_exchange_id
        || history.entries.len() > MAX_PROVIDER_FRAME_HISTORY
        || state.payload.len() > MAX_PROVIDER_FRAME_HISTORY_BYTES
        || history.entries.iter().any(|(key, entry)| {
            !key.starts_with("provider-frame:")
                || entry.public_stream_sequence == 0
                || !entry.body_sha256.0.starts_with("sha256:")
                || entry.body_sha256.0.len() != 71
        })
    {
        return Err(storage_failure(StorageError::adapter(
            "Provider frame history is invalid",
        )));
    }
    Ok((history, state.revision))
}

fn encode_provider_history(
    history: &ProviderPublicFrameHistory,
) -> Result<Vec<u8>, ProductSessionExecutionApplicationError> {
    if history.entries.len() > MAX_PROVIDER_FRAME_HISTORY {
        return Err(storage_failure(StorageError::adapter(
            "Provider frame history capacity is exhausted",
        )));
    }
    let payload = serde_json::to_vec(history).map_err(|_| {
        storage_failure(StorageError::adapter(
            "Provider frame history cannot encode",
        ))
    })?;
    if payload.len() > MAX_PROVIDER_FRAME_HISTORY_BYTES {
        return Err(storage_failure(StorageError::adapter(
            "Provider frame history bytes are exhausted",
        )));
    }
    Ok(payload)
}

fn provider_source_body_matches(
    existing: &DurableProviderPublicFrame,
    proposed: &DurableProviderPublicFrame,
) -> bool {
    existing.schema == proposed.schema
        && existing.repository_scope == proposed.repository_scope
        && existing.product_session_id == proposed.product_session_id
        && existing.execution_job_id == proposed.execution_job_id
        && existing.model_exchange_id == proposed.model_exchange_id
        && existing.chunk == proposed.chunk
        && existing.public_text_delta == proposed.public_text_delta
}

fn provider_source_digest(
    source: &DurableProviderPublicFrame,
) -> Result<Sha256Digest, ProductSessionExecutionApplicationError> {
    serde_json::to_vec(&(
        &source.schema,
        &source.repository_scope,
        &source.product_session_id,
        &source.execution_job_id,
        &source.model_exchange_id,
        &source.chunk,
        &source.public_text_delta,
    ))
    .map(|bytes| Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
    .map_err(|_| storage_failure(StorageError::adapter("Provider frame digest encode failed")))
}

fn internal_context(
    repository_scope: &RepositoryScope,
    request_id: &RequestId,
    expected_revision: u64,
    occurred_at: &Instant,
) -> Result<ProductSessionCommandContext, ProductSessionExecutionApplicationError> {
    let public_actor = PublicEventActor::System {
        id: SystemActorId(SYSTEM_ACTOR_ID.to_owned()),
    };
    let public_scope = public_repository_scope(repository_scope);
    let receipt_identity =
        public_receipt_identity(&public_actor, &public_scope, request_id.clone())
            .map_err(storage_failure)?;
    Ok(ProductSessionCommandContext {
        receipt_identity,
        expected_revision,
        event_id: ControlPlaneEventId(derived_identifier(
            b"product-session-event",
            request_id.0.as_bytes(),
            "evt",
        )),
        occurred_at: occurred_at.clone(),
        public_actor,
        public_scope,
    })
}

fn internal_receipt_identity(
    repository_scope: &RepositoryScope,
    request_id: &RequestId,
) -> Result<ReceiptIdentity, DurableExecutionPortError> {
    let actor = PublicEventActor::System {
        id: SystemActorId(SYSTEM_ACTOR_ID.to_owned()),
    };
    public_receipt_identity(
        &actor,
        &public_repository_scope(repository_scope),
        request_id.clone(),
    )
    .map_err(DurableExecutionPortError::Storage)
}

fn application_receipt_identity(
    repository_scope: &RepositoryScope,
    request_id: &RequestId,
) -> Result<ReceiptIdentity, ProductSessionExecutionApplicationError> {
    let actor = PublicEventActor::System {
        id: SystemActorId(SYSTEM_ACTOR_ID.to_owned()),
    };
    public_receipt_identity(
        &actor,
        &public_repository_scope(repository_scope),
        request_id.clone(),
    )
    .map_err(storage_failure)
}

fn public_text_delta(
    chunk: &ModelChunkMessage,
) -> Result<Option<String>, ProductSessionExecutionApplicationError> {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum PublicFrame {
        OutputTextDelta {
            delta: String,
        },
        #[serde(other)]
        Other,
    }
    if chunk.error.is_some() {
        return Err(ProductSessionExecutionApplicationError::InvalidCanonicalFrame);
    }
    let Some(payload) = chunk.payload.as_ref() else {
        return Ok(None);
    };
    if payload.content_type != "application/json" {
        return Err(ProductSessionExecutionApplicationError::InvalidCanonicalFrame);
    }
    let bytes = STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| ProductSessionExecutionApplicationError::InvalidCanonicalFrame)?;
    let observed = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    if observed != payload.payload_digest {
        return Err(ProductSessionExecutionApplicationError::InvalidCanonicalFrame);
    }
    let json = std::str::from_utf8(&bytes)
        .map_err(|_| ProductSessionExecutionApplicationError::InvalidCanonicalFrame)?;
    match serde_json::from_str(json)
        .map_err(|_| ProductSessionExecutionApplicationError::InvalidCanonicalFrame)?
    {
        PublicFrame::OutputTextDelta { delta } => Ok(Some(delta)),
        PublicFrame::Other => Ok(None),
    }
}

fn json_digest<T: Serialize>(value: &T) -> Result<Sha256Digest, DurableExecutionPortError> {
    serde_json::to_vec(value)
        .map(|bytes| Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
        .map_err(|_| storage_ingress("binding digest encode failed"))
}

fn derived_request_id(namespace: &[u8], input: &[u8]) -> RequestId {
    RequestId(derived_identifier(namespace, input, "req"))
}

fn internal_execution_event(request_id: &RequestId) -> NewOutboxEvent {
    NewOutboxEvent::internal(
        format!("internal:{}", request_id.0),
        PRODUCT_SESSION_EXECUTION_INTERNAL_TOPIC,
        Vec::new(),
    )
}

fn derived_identifier(namespace: &[u8], input: &[u8], prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(input);
    let encoded = format!("{:X}", hasher.finalize());
    format!("{prefix}_{}", &encoded[..26])
}

fn product_session_ingress(error: &ProductSessionServiceError) -> DurableExecutionPortError {
    let storage = match error.code() {
        ProductSessionServiceErrorCode::RequestConflict
        | ProductSessionServiceErrorCode::RevisionConflict => {
            StorageError::invalid_input("ProductSession receipt conflicts with durable state")
        }
        _ => StorageError::invalid_input(error.to_string()),
    };
    DurableExecutionPortError::Storage(storage)
}

fn interaction_ingress(_: ChatInteractionServiceError) -> DurableExecutionPortError {
    storage_ingress("Worker interaction request was rejected by canonical state")
}

fn product_session_failure(
    error: ProductSessionServiceError,
) -> ProductSessionExecutionApplicationError {
    ProductSessionExecutionApplicationError::ProductSession(error)
}

fn storage_failure(error: StorageError) -> ProductSessionExecutionApplicationError {
    ProductSessionExecutionApplicationError::Storage(error)
}

fn application_ingress(
    error: ProductSessionExecutionApplicationError,
) -> DurableExecutionPortError {
    match error {
        ProductSessionExecutionApplicationError::ProductSession(error) => {
            product_session_ingress(&error)
        }
        ProductSessionExecutionApplicationError::Storage(error) => {
            DurableExecutionPortError::Storage(error)
        }
        ProductSessionExecutionApplicationError::WorkerLifecycle(_) => {
            storage_ingress("Worker lifecycle failed")
        }
        ProductSessionExecutionApplicationError::InvalidCanonicalFrame => {
            storage_ingress("canonical Provider frame is invalid")
        }
    }
}

fn worker_lifecycle_ingress(error: &WorkerExecutionLifecycleError) -> DurableExecutionPortError {
    let _ = error;
    storage_ingress("Worker lifecycle settlement failed")
}

fn worker_slot_storage(error: impl fmt::Display) -> DurableExecutionPortError {
    let _ = error;
    storage_ingress("Worker slot terminalization failed")
}

fn storage_ingress(message: &'static str) -> DurableExecutionPortError {
    DurableExecutionPortError::Storage(StorageError::invalid_input(message))
}

#[cfg(test)]
#[path = "product_session_execution_application_tests.rs"]
mod tests;
