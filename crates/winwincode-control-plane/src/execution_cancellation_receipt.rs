// SPDX-License-Identifier: Apache-2.0

//! Receipt-first confirmation of the original scheduler cancellation control.
//! A cancellation acknowledgement never supplies execution terminal evidence.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ExecutionMessageId, RepositoryScope, Sha256Digest};
use winwincode_execution_port::generated::{
    JobCancelAckMessage, JobCancelAckMessageKind, JobCancelAckMessageStatus,
};
use winwincode_storage::{
    CommitReceipt, ExecutionAuthorityCommitGuard, ExecutionJobCancellationIntent,
    ExecutionQueueScope, NewOutboxEvent, ProductStateStorage, ReceiptIdentity,
    RepositorySchedulerCancellationReceipt, RepositorySchedulerScope, SqliteStorage, StateCommit,
    StateMutation, StateRevisionGuard, StorageError,
};

use crate::session_binding_transaction::{
    execution_message_actor_key, execution_message_request_id, require_id,
};

const ACK_PHASE: &str = "job-cancellation-acknowledgement";
const ACK_TOPIC: &str = "execution.job-cancellation-acknowledged.v1";

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptedCancellationAcknowledgement {
    schema_version: u8,
    message: JobCancelAckMessage,
    original_cancel_message_id: ExecutionMessageId,
    cancellation: ExecutionJobCancellationIntent,
    scope: ExecutionQueueScope,
}

/// References the original positive ACK receipt; supplies no terminal facts.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConsumedCancellationControl {
    schema_version: u8,
    acknowledgement_stream_id: String,
    acknowledgement_sha256: Sha256Digest,
}

pub(crate) fn accept(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    message: &JobCancelAckMessage,
) -> Result<(), StorageError> {
    let identity = ReceiptIdentity::new(
        execution_message_actor_key(&message.message_id)?,
        crate::repository_scope_key(scope)?,
        execution_message_request_id(&message.message_id, &message.lease, ACK_PHASE)?,
    )?;
    let digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(message).map_err(encode_error)?)
    ));
    let stream_id = format!("execution-cancellation-ack:{}", identity.request_id().0);
    if let Some(receipt) = storage.load_receipt(&identity, &digest)? {
        return validate_retained(storage, &receipt, &stream_id, message);
    }
    validate_shape(message)?;
    let source = storage
        .repository_scheduler()?
        .load_cancellation_receipt(
            &RepositorySchedulerScope {
                organization_id: scope.organization_id.clone(),
                workspace_id: scope.workspace_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
            },
            &message.request_id,
        )?
        .ok_or_else(|| invalid("cancellation acknowledgement has no original cancel receipt"))?;
    validate_original_control(&source, message)?;
    let job = storage
        .load_execution_job_record(&message.lease.job_id)?
        .ok_or_else(|| invalid("cancellation acknowledgement Job is missing"))?;
    let dispatch = storage
        .load_execution_dispatch_authority(&message.lease.job_id)?
        .ok_or_else(|| invalid("cancellation acknowledgement dispatch is missing"))?;
    if job.scope != source.job.scope
        || job.payload_digest != source.job.payload_digest
        || job.attempt != source.job.attempt
        || job.cancellation != source.job.cancellation
    {
        return Err(invalid(
            "cancellation acknowledgement original Job is no longer current",
        ));
    }
    crate::durable_execution_port::require_exact_dispatch_message(
        &message.lease,
        &message.worker_session_id,
        &message.session_identity.worker_session_id,
        &dispatch,
        storage,
    )
    .map_err(|error| invalid(&error.to_string()))?;
    let guards = binding_guards(storage, scope, &job, message)?;
    let accepted = AcceptedCancellationAcknowledgement {
        schema_version: 1,
        message: message.clone(),
        original_cancel_message_id: source
            .message_id
            .clone()
            .ok_or_else(|| invalid("original cancel message is missing"))?,
        cancellation: source
            .job
            .cancellation
            .clone()
            .ok_or_else(|| invalid("original cancellation intent is missing"))?,
        scope: source.job.scope.clone(),
    };
    let bytes = serde_json::to_vec(&accepted).map_err(encode_error)?;
    let mut commit = StateCommit::new(
        identity,
        digest,
        stream_id.clone(),
        0,
        bytes,
        vec![accepted_event(&accepted)?],
    );
    for guard in guards {
        commit = commit.with_state_guard(guard);
    }
    if confirms_control_consumption(message) {
        commit = retain_control_consumption(storage, scope, &source, &stream_id, commit)?;
    }
    let guard = ExecutionAuthorityCommitGuard::terminal_round(job, dispatch)?;
    let receipt = storage.commit_with_execution_authority_guard(&commit, &guard)?;
    validate_retained(storage, &receipt, &stream_id, message)
}

fn retain_control_consumption(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    source: &RepositorySchedulerCancellationReceipt,
    acknowledgement_stream_id: &str,
    commit: StateCommit,
) -> Result<StateCommit, StorageError> {
    let original_id = source
        .message_id
        .as_ref()
        .ok_or_else(|| invalid("original cancel message is missing"))?;
    let control_stream = consumed_control_stream(original_id);
    if let Some(state) = storage.load_state(&control_stream)? {
        if !control_consumed(storage, scope, source)? {
            return Err(invalid("cancel control consumption receipt is missing"));
        }
        return Ok(
            commit.with_state_guard(StateRevisionGuard::new(control_stream, state.revision)?)
        );
    }
    let reference = ConsumedCancellationControl {
        schema_version: 1,
        acknowledgement_stream_id: acknowledgement_stream_id.to_owned(),
        acknowledgement_sha256: commit.command_digest.clone(),
    };
    Ok(commit.with_state_mutation(StateMutation::new(
        control_stream,
        0,
        serde_json::to_vec(&reference).map_err(encode_error)?,
    )?))
}

/// Stops scheduler retransmission only after the exact positive Worker ACK
/// and its receipt were committed. Negative ACKs remain eligible for replay.
pub(crate) fn control_consumed(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    source: &RepositorySchedulerCancellationReceipt,
) -> Result<bool, StorageError> {
    let Some(original_id) = source.message_id.as_ref() else {
        return Ok(false);
    };
    let Some(state) = storage.load_state(&consumed_control_stream(original_id))? else {
        return Ok(false);
    };
    let reference: ConsumedCancellationControl = serde_json::from_slice(&state.payload)
        .map_err(|_| StorageError::adapter("cancel control consumption reference is corrupt"))?;
    let acknowledgement = storage
        .load_state(&reference.acknowledgement_stream_id)?
        .ok_or_else(|| StorageError::adapter("consumed cancel control has no ACK state"))?;
    let accepted: AcceptedCancellationAcknowledgement =
        serde_json::from_slice(&acknowledgement.payload)
            .map_err(|_| StorageError::adapter("consumed cancel control ACK state is corrupt"))?;
    let message = &accepted.message;
    let identity = ReceiptIdentity::new(
        execution_message_actor_key(&message.message_id)?,
        crate::repository_scope_key(scope)?,
        execution_message_request_id(&message.message_id, &message.lease, ACK_PHASE)?,
    )?;
    let expected_stream = format!("execution-cancellation-ack:{}", identity.request_id().0);
    let digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(message).map_err(encode_error)?)
    ));
    if state.revision != 1
        || reference.schema_version != 1
        || reference.acknowledgement_stream_id != expected_stream
        || reference.acknowledgement_sha256 != digest
        || accepted.original_cancel_message_id != *original_id
        || accepted.scope != source.job.scope
        || Some(&accepted.cancellation) != source.job.cancellation.as_ref()
        || !confirms_control_consumption(message)
    {
        return Err(StorageError::adapter(
            "cancel control consumption differs from its original positive ACK",
        ));
    }
    validate_original_control(source, message)?;
    let receipt = storage.load_receipt(&identity, &digest)?.ok_or_else(|| {
        StorageError::adapter("consumed cancel control has no original ACK receipt")
    })?;
    validate_retained(storage, &receipt, &expected_stream, message)?;
    Ok(true)
}

fn confirms_control_consumption(message: &JobCancelAckMessage) -> bool {
    message.error.is_none()
        && matches!(
            message.status,
            JobCancelAckMessageStatus::Accepted
                | JobCancelAckMessageStatus::AlreadyCancelling
                | JobCancelAckMessageStatus::AlreadyTerminal
        )
}

fn consumed_control_stream(original_id: &ExecutionMessageId) -> String {
    format!("execution-cancellation-consumed:{}", original_id.0)
}

fn validate_shape(message: &JobCancelAckMessage) -> Result<(), StorageError> {
    require_id(
        &message.message_id.0,
        "xmsg_",
        "cancel acknowledgement messageId",
    )?;
    require_id(
        &message.request_id.0,
        "req_",
        "cancel acknowledgement requestId",
    )?;
    if message.kind != JobCancelAckMessageKind::JobCancelAck
        || message
            .error
            .as_ref()
            .is_some_and(|error| !(1..=500).contains(&error.message.chars().count()))
        || !winwincode_execution_port::execution_identity::canonical_instant(&message.sent_at)
    {
        return Err(invalid(
            "cancellation acknowledgement differs from its canonical message shape",
        ));
    }
    Ok(())
}

fn validate_original_control(
    source: &RepositorySchedulerCancellationReceipt,
    message: &JobCancelAckMessage,
) -> Result<(), StorageError> {
    let lease = source
        .lease
        .as_ref()
        .ok_or_else(|| invalid("original cancellation has no lease"))?;
    let cancellation = source
        .job
        .cancellation
        .as_ref()
        .ok_or_else(|| invalid("original cancellation has no intent"))?;
    let exact = source.request_id == message.request_id
        && cancellation.request_id == message.request_id
        && source.message_id.is_some()
        && source.worker_session_id.as_ref() == Some(&message.worker_session_id)
        && source.codex_thread_id.as_ref() == Some(&message.session_identity.codex_thread_id)
        && source.job.scope.product_session_id == message.session_identity.product_session_id
        && source.job.work_run_id == message.session_identity.work_run_id
        && message.worker_session_id == message.session_identity.worker_session_id
        && lease.job_id == message.lease.job_id
        && lease.lease_id == message.lease.lease_id
        && lease.worker_id == message.lease.worker_id
        && lease.worker_instance_id == message.lease.worker_instance_id
        && i64::try_from(lease.attempt).ok() == Some(message.lease.attempt)
        && lease.fencing_token == message.lease.fencing_token
        && lease.issued_at == message.lease.issued_at
        && lease.expires_at == message.lease.expires_at
        && message.sent_at.0 >= cancellation.requested_at.0;
    if !exact {
        return Err(invalid(
            "cancellation acknowledgement differs from the original cancel control",
        ));
    }
    Ok(())
}

fn binding_guards(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    job: &winwincode_storage::ExecutionJobRecord,
    message: &JobCancelAckMessage,
) -> Result<Vec<StateRevisionGuard>, StorageError> {
    let slot = storage
        .worker_session_slots()
        .map_err(|error| invalid(&error.to_string()))?
        .load(&message.worker_session_id)
        .map_err(|error| invalid(&error.to_string()))?
        .ok_or_else(|| invalid("cancellation acknowledgement Worker slot is missing"))?;
    let authority = &slot.authority;
    if authority.job_id != message.lease.job_id
        || authority.worker_id != message.lease.worker_id
        || authority.worker_instance_id != message.lease.worker_instance_id
        || authority.worker_session_id != message.worker_session_id
        || authority.codex_thread_id != message.session_identity.codex_thread_id
        || authority.lease_id != message.lease.lease_id
        || i64::try_from(authority.attempt).ok() != Some(message.lease.attempt)
        || authority.fencing_token != message.lease.fencing_token
    {
        return Err(invalid(
            "cancellation acknowledgement differs from its exact Worker binding",
        ));
    }
    if let Some(delivery_id) = &job.scope.delivery_id {
        let stream = crate::delivery_transaction::delivery_stream_id(delivery_id);
        let state = storage
            .load_state(&stream)?
            .ok_or_else(|| invalid("cancelled Delivery is missing"))?;
        let delivery = winwincode_delivery::domain::Delivery::decode_json(&state.payload)
            .map_err(|error| invalid(&error.to_string()))?;
        if !delivery.snapshot().session_bindings.iter().any(|binding| {
            binding.execution_job_id == job.job_id
                && Some(&binding.work_run_id) == message.session_identity.work_run_id.as_ref()
                && binding.product_session_id == message.session_identity.product_session_id
                && binding.worker_session_id.as_ref() == Some(&message.worker_session_id)
                && binding.codex_thread_id.as_ref()
                    == Some(&message.session_identity.codex_thread_id)
                && i64::try_from(binding.attempt).ok() == Some(message.lease.attempt)
        }) {
            return Err(invalid(
                "cancellation acknowledgement Delivery binding is no longer current",
            ));
        }
        return Ok(vec![StateRevisionGuard::new(stream, state.revision)?]);
    }
    crate::product_session_execution_application::chat_cancellation_ack_guards(
        storage, scope, job, message,
    )
}

fn validate_retained(
    storage: &dyn ProductStateStorage,
    receipt: &CommitReceipt,
    stream_id: &str,
    message: &JobCancelAckMessage,
) -> Result<(), StorageError> {
    let state = storage.load_state(stream_id)?.ok_or_else(|| {
        StorageError::adapter("cancellation acknowledgement receipt has no original state")
    })?;
    let accepted: AcceptedCancellationAcknowledgement = serde_json::from_slice(&state.payload)
        .map_err(|_| StorageError::adapter("accepted cancellation acknowledgement is corrupt"))?;
    if accepted.schema_version != 1
        || accepted.message != *message
        || state.revision != 1
        || receipt.revision != 1
        || receipt.stream_id != stream_id
        || accepted.cancellation.request_id != message.request_id
        || receipt.events.len() != 1
        || receipt.events[0].event_id != accepted_event(&accepted)?.event_id
        || receipt.events[0].topic != ACK_TOPIC
        || receipt.events[0].payload != accepted_event(&accepted)?.payload
    {
        return Err(StorageError::adapter(
            "cancellation acknowledgement receipt differs from original acceptance",
        ));
    }
    Ok(())
}

fn accepted_event(
    accepted: &AcceptedCancellationAcknowledgement,
) -> Result<NewOutboxEvent, StorageError> {
    Ok(NewOutboxEvent::internal(
        format!(
            "execution-cancellation-ack:{}",
            execution_message_request_id(
                &accepted.message.message_id,
                &accepted.message.lease,
                ACK_PHASE,
            )?
            .0
        ),
        ACK_TOPIC,
        serde_json::to_vec(&serde_json::json!({
            "jobId": accepted.message.lease.job_id,
            "requestId": accepted.message.request_id,
            "originalCancelMessageId": accepted.original_cancel_message_id,
            "acknowledgementMessageId": accepted.message.message_id,
            "status": accepted.message.status,
        }))
        .map_err(encode_error)?,
    ))
}

fn invalid(message: &str) -> StorageError {
    StorageError::invalid_input(message)
}
fn encode_error(_: serde_json::Error) -> StorageError {
    StorageError::adapter("cancellation acknowledgement cannot encode")
}
