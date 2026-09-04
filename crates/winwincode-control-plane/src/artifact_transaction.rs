// SPDX-License-Identifier: Apache-2.0

//! Generated `ExecutionPort` Artifact messages mapped to the durable Artifact store.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use winwincode_delivery::{
    application::stage::SessionBindingAuthority,
    domain::{Delivery, StageRunStatus},
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{ExecutionAckSequence, ExecutionSequence, SchemaVersion, SessionIdentity};
use winwincode_execution_port::generated::{
    ArtifactAckMessage, ArtifactAckMessageKind, ArtifactChunkMessage, ArtifactKind,
    ArtifactOpenMessage, ExecutionJob, ExecutionLeaseStamp, ExecutionPortError,
    ExecutionPortErrorCode, ExecutionScope, LeaseWriteStatus,
};
use winwincode_storage::{
    ArtifactChunk, ArtifactError, ArtifactErrorKind, ArtifactMeteringAttribution, ArtifactOpen,
    ArtifactProvenance, ArtifactRetention, ArtifactStore, ArtifactWriteReceipt, DurableOutboxEvent,
    EnterpriseQuotaReservationState, ProductStateStorage, PublicEventActor, StorageError,
    public_actor_from_receipt_key,
};

use crate::artifact_enterprise_quota::{
    ArtifactEnterpriseQuotaAdmission, ArtifactEnterpriseQuotaReservation,
    ArtifactEnterpriseQuotaSaga, ArtifactEnterpriseQuotaSagaError,
};
use crate::delivery_transaction::{delivery_stream_id, load_durable_execution_job};
use crate::repository_scope_key;
use crate::session_binding_transaction::{instant_millis, require_id};

const MAX_ENCODED_PAYLOAD_BYTES: usize = 16_777_216;

/// Failure before an Artifact message can be acknowledged.
#[derive(Debug)]
pub enum ArtifactMessageError {
    Storage(StorageError),
    Artifact(ArtifactError),
    EnterpriseQuota(ArtifactEnterpriseQuotaSagaError),
    EnterpriseQuotaDenied,
}

impl fmt::Display for ArtifactMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "Artifact authority failed: {error}"),
            Self::Artifact(error) => write!(formatter, "Artifact storage failed: {error}"),
            Self::EnterpriseQuota(error) => write!(formatter, "Artifact quota failed: {error}"),
            Self::EnterpriseQuotaDenied => formatter.write_str("Artifact quota denied the write"),
        }
    }
}

impl std::error::Error for ArtifactMessageError {}

impl From<StorageError> for ArtifactMessageError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ArtifactError> for ArtifactMessageError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<ArtifactEnterpriseQuotaSagaError> for ArtifactMessageError {
    fn from(error: ArtifactEnterpriseQuotaSagaError) -> Self {
        Self::EnterpriseQuota(error)
    }
}

pub(crate) fn accept_open(
    storage: &dyn ProductStateStorage,
    artifacts: &mut ArtifactStore,
    scope: &RepositoryScope,
    message: &ArtifactOpenMessage,
    authority: &SessionBindingAuthority,
    enterprise_quota: &mut ArtifactEnterpriseQuotaSaga<'_, '_>,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    validate_open_shape(message)?;
    let context = ArtifactMessageContext::from_authority(
        scope,
        &message.lease,
        &message.worker_session_id,
        &message.sent_at,
        authority,
    )?;
    let (durable, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    let session_claim = ArtifactSessionClaim {
        worker_session_id: &message.worker_session_id,
        message_identity: &message.session_identity,
    };
    let replay_identity = context.validate_replay_identity(
        storage,
        scope,
        &durable,
        &job,
        &message.lease,
        session_claim,
    )?;
    let open = frozen_artifact_open(scope, message, &context, &durable, &job, &replay_identity)?;
    match artifacts.replay_open(&open) {
        Ok(Some(receipt)) => {
            require_replay_quota(enterprise_quota, &open, &message.sent_at)?;
            return ack_open(message, &receipt, &replay_identity);
        }
        Ok(None) => {}
        Err(error) if error.kind() == ArtifactErrorKind::Conflict => {
            return ack_open_conflict(
                artifacts,
                &context.scope_key,
                message,
                &error,
                &replay_identity,
            );
        }
        Err(error) => return Err(error.into()),
    }
    let session_identity = context.validate_durable(
        storage,
        scope,
        &durable,
        &job,
        &message.lease,
        session_claim,
    )?;
    if let Some(rejection) = context.rejection {
        let acknowledged = conflict_acknowledged_sequence(
            artifacts,
            &context.scope_key,
            &message.artifact.artifact_id,
        )?;
        return ack_lease_rejection(
            message.message_id.clone(),
            message.sent_at.clone(),
            message.lease.clone(),
            message.worker_session_id.clone(),
            message.artifact.artifact_id.clone(),
            acknowledged,
            rejection,
            &session_identity,
        );
    }
    let reservation = reserve_artifact_open(enterprise_quota, &open, &message.sent_at)?;
    match artifacts.open_artifact(open.clone()) {
        Ok(receipt) => ack_open(message, &receipt, &session_identity),
        Err(error) if error.kind() == ArtifactErrorKind::Conflict => {
            if let Some(receipt) = artifacts.replay_open(&open)? {
                return ack_open(message, &receipt, &session_identity);
            }
            enterprise_quota.release(
                &reservation,
                winwincode_storage::EnterpriseQuotaReleaseReason::Failed,
                &message.sent_at,
            )?;
            ack_open_conflict(
                artifacts,
                &context.scope_key,
                message,
                &error,
                &session_identity,
            )
        }
        Err(error) => {
            enterprise_quota.release(
                &reservation,
                winwincode_storage::EnterpriseQuotaReleaseReason::Failed,
                &message.sent_at,
            )?;
            Err(error.into())
        }
    }
}

fn frozen_artifact_open(
    scope: &RepositoryScope,
    message: &ArtifactOpenMessage,
    context: &ArtifactMessageContext,
    durable: &DurableOutboxEvent,
    job: &ExecutionJob,
    session_identity: &SessionIdentity,
) -> Result<ArtifactOpen, ArtifactMessageError> {
    let size_bytes = u64::try_from(message.artifact.size_bytes)
        .map_err(|_| StorageError::invalid_input("Artifact sizeBytes is out of range"))?;
    Ok(ArtifactOpen::new(
        context.scope_key.clone(),
        message.message_id.clone(),
        message.request_id.clone(),
        message.artifact.artifact_id.clone(),
        artifact_kind(&message.artifact.kind),
        message.artifact.media_type.clone(),
        message.artifact.digest.clone(),
        size_bytes,
        message.artifact.file_name.clone(),
        context.provenance.clone(),
        metering_attribution(scope, durable, job, session_identity)?,
        ArtifactRetention::Indefinite,
        context.sent_at_millis,
    ))
}

fn reserve_artifact_open(
    enterprise_quota: &mut ArtifactEnterpriseQuotaSaga<'_, '_>,
    open: &ArtifactOpen,
    requested_at: &winwincode_domain::Instant,
) -> Result<ArtifactEnterpriseQuotaReservation, ArtifactMessageError> {
    match enterprise_quota.reserve_open(open, requested_at)? {
        ArtifactEnterpriseQuotaAdmission::Admitted(reservation) => Ok(reservation),
        ArtifactEnterpriseQuotaAdmission::TerminalReplay(_) => {
            Err(ArtifactMessageError::EnterpriseQuota(
                ArtifactEnterpriseQuotaSagaError::UnexpectedTerminalReservation,
            ))
        }
        ArtifactEnterpriseQuotaAdmission::Denied(_) => {
            Err(ArtifactMessageError::EnterpriseQuotaDenied)
        }
    }
}

fn require_replay_quota(
    enterprise_quota: &mut ArtifactEnterpriseQuotaSaga<'_, '_>,
    open: &ArtifactOpen,
    requested_at: &winwincode_domain::Instant,
) -> Result<(), ArtifactMessageError> {
    match enterprise_quota.reserve_open(open, requested_at)? {
        ArtifactEnterpriseQuotaAdmission::Admitted(_) => Ok(()),
        ArtifactEnterpriseQuotaAdmission::TerminalReplay(receipt)
            if receipt.record.state == EnterpriseQuotaReservationState::Settled =>
        {
            Ok(())
        }
        ArtifactEnterpriseQuotaAdmission::TerminalReplay(_) => {
            Err(ArtifactMessageError::EnterpriseQuota(
                ArtifactEnterpriseQuotaSagaError::UnexpectedTerminalReservation,
            ))
        }
        ArtifactEnterpriseQuotaAdmission::Denied(_) => {
            Err(ArtifactMessageError::EnterpriseQuotaDenied)
        }
    }
}

fn metering_attribution(
    scope: &RepositoryScope,
    durable: &DurableOutboxEvent,
    job: &ExecutionJob,
    session_identity: &SessionIdentity,
) -> Result<ArtifactMeteringAttribution, StorageError> {
    let actor = public_actor_from_receipt_key(durable.receipt_identity().actor_key())?;
    let PublicEventActor::User { id: user_id } = actor else {
        return Err(StorageError::invalid_input(
            "Artifact storage attribution requires the authenticated User actor",
        ));
    };
    let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
        return Err(StorageError::invalid_input(
            "Artifact storage attribution requires a Delivery stage",
        ));
    };
    if session_identity.product_session_id != job_scope.product_session_id {
        return Err(StorageError::invalid_input(
            "Artifact storage attribution differs from the verified ProductSession",
        ));
    }
    Ok(ArtifactMeteringAttribution {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        delivery_id: Some(job_scope.delivery_id.clone()),
        product_session_id: Some(session_identity.product_session_id.clone()),
        user_id,
    })
}

pub(crate) fn accept_chunk(
    storage: &dyn ProductStateStorage,
    artifacts: &mut ArtifactStore,
    scope: &RepositoryScope,
    message: &ArtifactChunkMessage,
    authority: &SessionBindingAuthority,
    enterprise_quota: &mut ArtifactEnterpriseQuotaSaga<'_, '_>,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    validate_chunk_shape(message)?;
    let context = ArtifactMessageContext::from_authority(
        scope,
        &message.lease,
        &message.worker_session_id,
        &message.sent_at,
        authority,
    )?;
    let bytes = STANDARD
        .decode(&message.payload.data_base64)
        .map_err(|_| StorageError::invalid_input("Artifact chunk dataBase64 is invalid"))?;
    if STANDARD.encode(&bytes) != message.payload.data_base64 {
        return Err(
            StorageError::invalid_input("Artifact chunk dataBase64 is not canonical").into(),
        );
    }
    let sequence = u64::try_from(message.sequence.0)
        .map_err(|_| StorageError::invalid_input("Artifact chunk sequence is out of range"))?;
    let chunk = ArtifactChunk::new(
        context.scope_key.clone(),
        message.message_id.clone(),
        message.artifact_id.clone(),
        context.provenance.clone(),
        context.sent_at_millis,
        sequence,
        message.payload.content_type.clone(),
        message.payload.payload_digest.clone(),
        bytes,
        message.is_final,
    );
    let (durable, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    let session_claim = ArtifactSessionClaim {
        worker_session_id: &message.worker_session_id,
        message_identity: &message.session_identity,
    };
    let replay_identity = context.validate_replay_identity(
        storage,
        scope,
        &durable,
        &job,
        &message.lease,
        session_claim,
    )?;
    match artifacts.replay_chunk(&chunk) {
        Ok(Some(receipt)) => {
            if message.is_final {
                enterprise_quota.recover_final(&message.artifact_id, artifacts)?;
            }
            return ack_chunk(message, &receipt, &replay_identity);
        }
        Ok(None) => {}
        Err(error) => {
            return ack_chunk_error(
                artifacts,
                &context.scope_key,
                message,
                error,
                &replay_identity,
            );
        }
    }
    let session_identity = context.validate_durable(
        storage,
        scope,
        &durable,
        &job,
        &message.lease,
        session_claim,
    )?;
    if let Some(rejection) = context.rejection {
        let acknowledged =
            conflict_acknowledged_sequence(artifacts, &context.scope_key, &message.artifact_id)?;
        return ack_lease_rejection(
            message.message_id.clone(),
            message.sent_at.clone(),
            message.lease.clone(),
            message.worker_session_id.clone(),
            message.artifact_id.clone(),
            acknowledged,
            rejection,
            &session_identity,
        );
    }
    match artifacts.append_chunk(&chunk) {
        Ok(receipt) => {
            if message.is_final {
                enterprise_quota.recover_final(&message.artifact_id, artifacts)?;
            }
            ack_chunk(message, &receipt, &session_identity)
        }
        Err(error) => ack_chunk_error(
            artifacts,
            &context.scope_key,
            message,
            error,
            &session_identity,
        ),
    }
}

fn conflict_acknowledged_sequence(
    artifacts: &ArtifactStore,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    artifact_id: &winwincode_domain::ArtifactId,
) -> Result<u64, ArtifactMessageError> {
    match artifacts.acknowledged_sequence(scope_key, artifact_id) {
        Ok(sequence) => Ok(sequence),
        Err(error) if error.kind() == ArtifactErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn ack_open_conflict(
    artifacts: &ArtifactStore,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    message: &ArtifactOpenMessage,
    error: &ArtifactError,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    let acknowledged =
        conflict_acknowledged_sequence(artifacts, scope_key, &message.artifact.artifact_id)?;
    ack_failure(
        message.message_id.clone(),
        message.sent_at.clone(),
        message.lease.clone(),
        message.worker_session_id.clone(),
        message.artifact.artifact_id.clone(),
        acknowledged,
        LeaseWriteStatus::RejectedConflict,
        None,
        ExecutionPortErrorCode::MessageConflict,
        error.to_string(),
        false,
        session_identity,
    )
}

fn ack_chunk_error(
    artifacts: &ArtifactStore,
    scope_key: &winwincode_storage::ReceiptScopeKey,
    message: &ArtifactChunkMessage,
    error: ArtifactError,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    match error.kind() {
        ArtifactErrorKind::SequenceGap => {
            let acknowledged =
                conflict_acknowledged_sequence(artifacts, scope_key, &message.artifact_id)?;
            let replay_from = acknowledged.checked_add(1).ok_or_else(|| {
                StorageError::invalid_input("Artifact replay sequence exceeds public range")
            })?;
            let replay_from = i64::try_from(replay_from).map_err(|_| {
                StorageError::invalid_input("Artifact replay sequence exceeds public range")
            })?;
            ack_failure(
                message.message_id.clone(),
                message.sent_at.clone(),
                message.lease.clone(),
                message.worker_session_id.clone(),
                message.artifact_id.clone(),
                acknowledged,
                LeaseWriteStatus::Gap,
                Some(ExecutionSequence(replay_from)),
                ExecutionPortErrorCode::SequenceGap,
                error.to_string(),
                true,
                session_identity,
            )
        }
        ArtifactErrorKind::DigestMismatch => {
            let acknowledged =
                conflict_acknowledged_sequence(artifacts, scope_key, &message.artifact_id)?;
            ack_failure(
                message.message_id.clone(),
                message.sent_at.clone(),
                message.lease.clone(),
                message.worker_session_id.clone(),
                message.artifact_id.clone(),
                acknowledged,
                LeaseWriteStatus::RejectedConflict,
                None,
                ExecutionPortErrorCode::ArtifactDigestMismatch,
                error.to_string(),
                false,
                session_identity,
            )
        }
        ArtifactErrorKind::Conflict => {
            let acknowledged =
                conflict_acknowledged_sequence(artifacts, scope_key, &message.artifact_id)?;
            ack_failure(
                message.message_id.clone(),
                message.sent_at.clone(),
                message.lease.clone(),
                message.worker_session_id.clone(),
                message.artifact_id.clone(),
                acknowledged,
                LeaseWriteStatus::RejectedConflict,
                None,
                ExecutionPortErrorCode::MessageConflict,
                error.to_string(),
                false,
                session_identity,
            )
        }
        _ => Err(error.into()),
    }
}

struct ArtifactMessageContext {
    scope_key: winwincode_storage::ReceiptScopeKey,
    provenance: ArtifactProvenance,
    sent_at_millis: u64,
    rejection: Option<ArtifactLeaseRejection>,
}

#[derive(Clone, Copy)]
struct ArtifactSessionClaim<'a> {
    worker_session_id: &'a winwincode_domain::WorkerSessionId,
    message_identity: &'a SessionIdentity,
}

#[derive(Clone, Copy)]
enum ArtifactLeaseRejection {
    Expired,
    StaleFencingToken,
    WorkerInstance,
}

impl ArtifactMessageContext {
    fn from_authority(
        scope: &RepositoryScope,
        lease: &ExecutionLeaseStamp,
        worker_session_id: &winwincode_domain::WorkerSessionId,
        sent_at: &winwincode_domain::Instant,
        authority: &SessionBindingAuthority,
    ) -> Result<Self, StorageError> {
        let scope_key = repository_scope_key(scope)?;
        let mut rejection = validate_authority(lease, worker_session_id, authority)?;
        let sent_at_millis = instant_millis(sent_at)?;
        let issued_at_millis = instant_millis(&lease.issued_at)?;
        let expires_at_millis = instant_millis(&lease.expires_at)?;
        if sent_at_millis < issued_at_millis {
            return Err(StorageError::invalid_input(
                "Artifact message time precedes its active lease",
            ));
        }
        if sent_at_millis >= expires_at_millis {
            rejection = Some(ArtifactLeaseRejection::Expired);
        }
        let attempt = u64::try_from(lease.attempt)
            .map_err(|_| StorageError::invalid_input("Artifact lease attempt is out of range"))?;
        let provenance = ArtifactProvenance::execution_job(
            lease.job_id.clone(),
            attempt,
            lease.lease_id.clone(),
            lease.fencing_token.clone(),
            lease.worker_id.clone(),
            lease.worker_instance_id.clone(),
            worker_session_id.clone(),
        )
        .map_err(ArtifactMessageError::Artifact)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
        Ok(Self {
            scope_key,
            provenance,
            sent_at_millis,
            rejection,
        })
    }

    fn validate_durable(
        &self,
        storage: &dyn ProductStateStorage,
        scope: &RepositoryScope,
        durable: &DurableOutboxEvent,
        job: &ExecutionJob,
        lease: &ExecutionLeaseStamp,
        claim: ArtifactSessionClaim<'_>,
    ) -> Result<SessionIdentity, StorageError> {
        if durable.receipt_identity().scope_key() != &self.scope_key
            || job.workspace.repository_id != scope.repository_id
            || job.job_id != lease.job_id
            || job.attempt != lease.attempt
        {
            return Err(StorageError::invalid_input(
                "Artifact message does not match the durable ExecutionJob scope",
            ));
        }
        let session_identity = validate_current_binding(
            storage,
            durable,
            job,
            lease,
            claim.worker_session_id,
            true,
            self.rejection.is_none(),
        )?;
        if claim.message_identity != &session_identity {
            return Err(StorageError::invalid_input(
                "Artifact message SessionIdentity does not match the durable SessionBinding",
            ));
        }
        Ok(session_identity)
    }

    fn validate_replay_identity(
        &self,
        storage: &dyn ProductStateStorage,
        scope: &RepositoryScope,
        durable: &DurableOutboxEvent,
        job: &ExecutionJob,
        lease: &ExecutionLeaseStamp,
        claim: ArtifactSessionClaim<'_>,
    ) -> Result<SessionIdentity, StorageError> {
        if durable.receipt_identity().scope_key() != &self.scope_key
            || job.workspace.repository_id != scope.repository_id
            || job.job_id != lease.job_id
            || job.attempt != lease.attempt
        {
            return Err(StorageError::invalid_input(
                "Artifact message does not match the durable ExecutionJob scope",
            ));
        }
        let session_identity = validate_current_binding(
            storage,
            durable,
            job,
            lease,
            claim.worker_session_id,
            false,
            false,
        )?;
        if claim.message_identity != &session_identity {
            return Err(StorageError::invalid_input(
                "Artifact message SessionIdentity does not match the durable SessionBinding",
            ));
        }
        Ok(session_identity)
    }
}

fn validate_current_binding(
    storage: &dyn ProductStateStorage,
    durable: &DurableOutboxEvent,
    job: &ExecutionJob,
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    require_active_run: bool,
    require_authority: bool,
) -> Result<SessionIdentity, StorageError> {
    let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
        return Err(StorageError::invalid_input(
            "Artifact message requires a Delivery stage ExecutionJob",
        ));
    };
    if durable.stream_id() != delivery_stream_id(&job_scope.delivery_id) {
        return Err(StorageError::invalid_input(
            "Artifact ExecutionJob receipt identifies another Delivery",
        ));
    }
    let state = storage
        .load_state(durable.stream_id())?
        .ok_or_else(|| StorageError::invalid_input("Artifact Delivery state does not exist"))?;
    let delivery = Delivery::decode_json(&state.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let mut runs = delivery.snapshot().stage_runs.iter().filter(|run| {
        run.id == job_scope.stage_run_id
            && (!require_active_run
                || matches!(
                    run.status,
                    StageRunStatus::Running | StageRunStatus::Waiting
                ))
    });
    let run = runs.next().ok_or_else(|| {
        StorageError::invalid_input("Artifact ExecutionJob StageRun is not active")
    })?;
    let attempt = u64::try_from(job.attempt).map_err(|_| {
        StorageError::invalid_input("Artifact ExecutionJob attempt is out of range")
    })?;
    if runs.next().is_some() || run.attempt != attempt {
        return Err(StorageError::invalid_input(
            "Artifact ExecutionJob StageRun is ambiguous or stale",
        ));
    }
    let bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id)
        .collect::<Vec<_>>();
    let [binding] = bindings.as_slice() else {
        return Err(StorageError::invalid_input(
            "Artifact StageRun must have one exact SessionBinding",
        ));
    };
    if binding.execution_job_id != job.job_id
        || binding.product_session_id != job_scope.product_session_id
        || binding.delivery_task_id != job_scope.delivery_task_id
        || binding.worker_session_id.as_ref() != Some(worker_session_id)
        || binding.codex_thread_id.is_none()
    {
        return Err(StorageError::invalid_input(
            "Artifact message does not match the complete durable SessionBinding",
        ));
    }
    let codex_thread_id = binding.codex_thread_id.clone().ok_or_else(|| {
        StorageError::invalid_input("Artifact StageRun SessionBinding has no CodexThread")
    })?;
    if require_authority
        && (binding.worker_id.as_ref() != Some(&lease.worker_id)
            || binding.worker_instance_id.as_ref() != Some(&lease.worker_instance_id)
            || binding.lease_id.as_ref() != Some(&lease.lease_id)
            || binding.attempt != attempt
            || binding.fencing_token.as_ref() != Some(&lease.fencing_token))
    {
        return Err(StorageError::invalid_input(
            "Artifact message authority does not match the durable SessionBinding",
        ));
    }
    Ok(SessionIdentity {
        codex_thread_id,
        product_session_id: binding.product_session_id.clone(),
        stage_run_id: Some(binding.stage_run_id.clone()),
        worker_session_id: worker_session_id.clone(),
    })
}

fn validate_authority(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    authority: &SessionBindingAuthority,
) -> Result<Option<ArtifactLeaseRejection>, StorageError> {
    let attempt = u64::try_from(lease.attempt)
        .map_err(|_| StorageError::invalid_input("Artifact lease attempt is out of range"))?;
    let active = authority.active_lease();
    if active.execution_job_id() != &lease.job_id
        || active.attempt() != attempt
        || active.lease_id() != &lease.lease_id
        || active.worker_id() != &lease.worker_id
        || active.worker_session_id() != worker_session_id
        || authority.issued_at() != &lease.issued_at
        || authority.expires_at() != &lease.expires_at
    {
        return Err(StorageError::invalid_input(
            "Artifact message does not match the scheduler-owned active lease",
        ));
    }
    if active.worker_instance_id() != &lease.worker_instance_id {
        return Ok(Some(ArtifactLeaseRejection::WorkerInstance));
    }
    if active.fencing_token() != &lease.fencing_token {
        if decimal_token_less(&lease.fencing_token.0, &active.fencing_token().0) {
            return Ok(Some(ArtifactLeaseRejection::StaleFencingToken));
        }
        return Err(StorageError::invalid_input(
            "Artifact message uses an unissued future fencing token",
        ));
    }
    Ok(None)
}

fn validate_open_shape(message: &ArtifactOpenMessage) -> Result<(), StorageError> {
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(StorageError::invalid_input(
            "Artifact message schemaVersion is unsupported",
        ));
    }
    require_id(&message.message_id.0, "xmsg_", "messageId")?;
    require_id(&message.request_id.0, "req_", "requestId")?;
    require_id(&message.lease.job_id.0, "job_", "lease.jobId")?;
    require_id(&message.lease.lease_id.0, "lse_", "lease.leaseId")?;
    require_id(&message.lease.worker_id.0, "wrk_", "lease.workerId")?;
    require_id(
        &message.lease.worker_instance_id.0,
        "wki_",
        "lease.workerInstanceId",
    )?;
    require_id(&message.worker_session_id.0, "wsn_", "workerSessionId")?;
    validate_session_identity_shape(&message.session_identity)?;
    validate_lease_numbers(message.lease.attempt, &message.lease.fencing_token.0)?;
    if message.artifact.size_bytes < 0 {
        return Err(StorageError::invalid_input(
            "Artifact sizeBytes must not be negative",
        ));
    }
    Ok(())
}

fn validate_chunk_shape(message: &ArtifactChunkMessage) -> Result<(), StorageError> {
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(StorageError::invalid_input(
            "Artifact message schemaVersion is unsupported",
        ));
    }
    require_id(&message.message_id.0, "xmsg_", "messageId")?;
    require_id(&message.lease.job_id.0, "job_", "lease.jobId")?;
    require_id(&message.lease.lease_id.0, "lse_", "lease.leaseId")?;
    require_id(&message.lease.worker_id.0, "wrk_", "lease.workerId")?;
    require_id(
        &message.lease.worker_instance_id.0,
        "wki_",
        "lease.workerInstanceId",
    )?;
    require_id(&message.worker_session_id.0, "wsn_", "workerSessionId")?;
    validate_session_identity_shape(&message.session_identity)?;
    validate_lease_numbers(message.lease.attempt, &message.lease.fencing_token.0)?;
    if message.sequence.0 <= 0 {
        return Err(StorageError::invalid_input(
            "Artifact chunk sequence must be positive",
        ));
    }
    let content_type = message.payload.content_type.as_bytes();
    if content_type.is_empty()
        || content_type.len() > 200
        || !content_type[0].is_ascii_alphanumeric()
        || !content_type.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-' | b'/'
                )
        })
    {
        return Err(StorageError::invalid_input(
            "Artifact chunk contentType is invalid",
        ));
    }
    if message.payload.data_base64.len() > MAX_ENCODED_PAYLOAD_BYTES {
        return Err(StorageError::invalid_input(
            "Artifact chunk dataBase64 exceeds the transport limit",
        ));
    }
    Ok(())
}

fn validate_session_identity_shape(identity: &SessionIdentity) -> Result<(), StorageError> {
    require_id(
        &identity.product_session_id.0,
        "psn_",
        "sessionIdentity.productSessionId",
    )?;
    let stage_run_id = identity.stage_run_id.as_ref().ok_or_else(|| {
        StorageError::invalid_input("sessionIdentity.stageRunId is required for artifacts")
    })?;
    require_id(&stage_run_id.0, "run_", "sessionIdentity.stageRunId")?;
    require_id(
        &identity.worker_session_id.0,
        "wsn_",
        "sessionIdentity.workerSessionId",
    )?;
    require_id(
        &identity.codex_thread_id.0,
        "cdx_",
        "sessionIdentity.codexThreadId",
    )?;
    Ok(())
}

fn validate_lease_numbers(attempt: i64, fencing_token: &str) -> Result<(), StorageError> {
    if attempt <= 0
        || fencing_token.is_empty()
        || fencing_token.len() > 20
        || fencing_token.starts_with('0')
        || !fencing_token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StorageError::invalid_input(
            "Artifact lease attempt or fencingToken is invalid",
        ));
    }
    Ok(())
}

fn decimal_token_less(left: &str, right: &str) -> bool {
    left.len() < right.len() || (left.len() == right.len() && left < right)
}

fn artifact_kind(kind: &ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Candidate => "candidate",
        ArtifactKind::CommandOutput => "command_output",
        ArtifactKind::Diff => "diff",
        ArtifactKind::Log => "log",
        ArtifactKind::Report => "report",
        ArtifactKind::TestOutput => "test_output",
        ArtifactKind::Usage => "usage",
    }
}

fn ack_open(
    message: &ArtifactOpenMessage,
    receipt: &ArtifactWriteReceipt,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    ack(
        message.message_id.clone(),
        message.sent_at.clone(),
        message.lease.clone(),
        message.worker_session_id.clone(),
        message.artifact.artifact_id.clone(),
        receipt,
        session_identity,
    )
}

fn ack_chunk(
    message: &ArtifactChunkMessage,
    receipt: &ArtifactWriteReceipt,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    ack(
        message.message_id.clone(),
        message.sent_at.clone(),
        message.lease.clone(),
        message.worker_session_id.clone(),
        message.artifact_id.clone(),
        receipt,
        session_identity,
    )
}

#[allow(clippy::too_many_arguments)]
fn ack_lease_rejection(
    message_id: winwincode_domain::ExecutionMessageId,
    sent_at: winwincode_domain::Instant,
    lease: ExecutionLeaseStamp,
    worker_session_id: winwincode_domain::WorkerSessionId,
    artifact_id: winwincode_domain::ArtifactId,
    acknowledged_sequence: u64,
    rejection: ArtifactLeaseRejection,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    let (status, code, message) = match rejection {
        ArtifactLeaseRejection::Expired => (
            LeaseWriteStatus::RejectedExpiredLease,
            ExecutionPortErrorCode::LeaseExpired,
            "Artifact message lease has expired",
        ),
        ArtifactLeaseRejection::StaleFencingToken => (
            LeaseWriteStatus::RejectedStaleFencingToken,
            ExecutionPortErrorCode::StaleFencingToken,
            "Artifact message uses a stale fencing token",
        ),
        ArtifactLeaseRejection::WorkerInstance => (
            LeaseWriteStatus::RejectedWorkerInstance,
            ExecutionPortErrorCode::WorkerInstanceChanged,
            "Artifact message comes from a replaced Worker instance",
        ),
    };
    ack_failure(
        message_id,
        sent_at,
        lease,
        worker_session_id,
        artifact_id,
        acknowledged_sequence,
        status,
        None,
        code,
        message.into(),
        false,
        session_identity,
    )
}

fn ack(
    message_id: winwincode_domain::ExecutionMessageId,
    sent_at: winwincode_domain::Instant,
    lease: ExecutionLeaseStamp,
    worker_session_id: winwincode_domain::WorkerSessionId,
    artifact_id: winwincode_domain::ArtifactId,
    receipt: &ArtifactWriteReceipt,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    let acknowledged = i64::try_from(receipt.acknowledged_sequence())
        .map_err(|_| StorageError::invalid_input("Artifact ack sequence exceeds public range"))?;
    Ok(ack_response(
        message_id,
        sent_at,
        lease,
        worker_session_id,
        artifact_id,
        ExecutionAckSequence(acknowledged),
        if receipt.is_duplicate() {
            LeaseWriteStatus::Duplicate
        } else {
            LeaseWriteStatus::Accepted
        },
        None,
        None,
        session_identity,
    ))
}

#[allow(clippy::too_many_arguments)]
fn ack_failure(
    message_id: winwincode_domain::ExecutionMessageId,
    sent_at: winwincode_domain::Instant,
    lease: ExecutionLeaseStamp,
    worker_session_id: winwincode_domain::WorkerSessionId,
    artifact_id: winwincode_domain::ArtifactId,
    acknowledged_sequence: u64,
    status: LeaseWriteStatus,
    replay_from_sequence: Option<ExecutionSequence>,
    code: ExecutionPortErrorCode,
    message: String,
    retryable: bool,
    session_identity: &SessionIdentity,
) -> Result<ArtifactAckMessage, ArtifactMessageError> {
    let acknowledged = i64::try_from(acknowledged_sequence)
        .map_err(|_| StorageError::invalid_input("Artifact ack sequence exceeds public range"))?;
    Ok(ack_response(
        message_id,
        sent_at,
        lease,
        worker_session_id,
        artifact_id,
        ExecutionAckSequence(acknowledged),
        status,
        replay_from_sequence,
        Some(ExecutionPortError {
            code,
            message,
            retryable,
        }),
        session_identity,
    ))
}

#[allow(clippy::too_many_arguments)]
fn ack_response(
    message_id: winwincode_domain::ExecutionMessageId,
    sent_at: winwincode_domain::Instant,
    lease: ExecutionLeaseStamp,
    worker_session_id: winwincode_domain::WorkerSessionId,
    artifact_id: winwincode_domain::ArtifactId,
    ack_sequence: ExecutionAckSequence,
    status: LeaseWriteStatus,
    replay_from_sequence: Option<ExecutionSequence>,
    error: Option<ExecutionPortError>,
    session_identity: &SessionIdentity,
) -> ArtifactAckMessage {
    ArtifactAckMessage {
        ack_sequence,
        artifact_id,
        error,
        kind: ArtifactAckMessageKind::ArtifactAck,
        lease,
        message_id,
        replay_from_sequence,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at,
        session_identity: session_identity.clone(),
        status,
        worker_session_id,
    }
}
