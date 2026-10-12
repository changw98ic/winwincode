// SPDX-License-Identifier: Apache-2.0

//! Receipt-first acceptance and recoverable acknowledgement of one Worker outcome.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::Instant;
use winwincode_execution_port::generated::{ExecutionOutcomeStatus, JobOutcomeMessage};
use winwincode_storage::{
    CommitReceipt, ExecutionAuthorityCommitGuard, ExecutionDispatchAuthority,
    ExecutionJobCancellationIntent, ExecutionJobRecord, ExecutionJobState,
    ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest, ExecutionQueueScope,
    NewOutboxEvent, ProductStateStorage, PublicEventActor, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, RepositorySchedulerScope, RepositorySchedulerTerminalRequest, SqliteStorage,
    StateCommit, StateMutation, StorageError, WorkerPoolId, WorkerSlotAuthority,
};

pub(crate) struct ExecutionOutcomeCommit<'a> {
    pub message: &'a JobOutcomeMessage,
    pub decision: &'a ExecutionTerminalDecision,
    pub dispatch: &'a ExecutionDispatchAuthority,
    pub scope: &'a ExecutionQueueScope,
    pub worker_pool_id: &'a WorkerPoolId,
    pub authority: &'a WorkerSlotAuthority,
    pub follow_up: Option<&'a AcceptedOutcomeFollowUp>,
}

/// Control Plane cancellation decides the product terminal. The original
/// Worker frame remains immutable evidence and the exact acknowledgement key.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecutionTerminalDecision {
    status: ExecutionOutcomeStatus,
    terminal_at: Instant,
    cancellation: Option<ExecutionJobCancellationIntent>,
}

impl ExecutionTerminalDecision {
    pub(crate) fn apply(&self, message: &JobOutcomeMessage) -> JobOutcomeMessage {
        let mut effective = message.clone();
        effective.outcome.status = self.status.clone();
        effective.outcome.finished_at = self.terminal_at.clone();
        effective
    }

    fn validate_retained(&self, message: &JobOutcomeMessage) -> Result<(), StorageError> {
        let valid =
            winwincode_execution_port::execution_identity::canonical_instant(&self.terminal_at)
                && match &self.cancellation {
                    Some(cancellation) => {
                        self.status == ExecutionOutcomeStatus::Cancelled
                            && winwincode_execution_port::execution_identity::canonical_instant(
                                &cancellation.requested_at,
                            )
                            && cancellation
                                .request_id
                                .0
                                .strip_prefix("req_")
                                .is_some_and(|id| {
                                    id.len() == 26
                                        && id.bytes().all(|byte| {
                                            b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte)
                                        })
                                })
                            && self.terminal_at.0 >= cancellation.requested_at.0
                            && self.terminal_at.0 >= message.outcome.finished_at.0
                    }
                    None => {
                        self.status == message.outcome.status
                            && self.terminal_at == message.outcome.finished_at
                    }
                };
        if !valid {
            return Err(StorageError::adapter(
                "retained execution terminal decision is inconsistent",
            ));
        }
        Ok(())
    }
}

pub(crate) fn terminal_decision(
    storage: &dyn ProductStateStorage,
    message: &JobOutcomeMessage,
    accepted_at: &Instant,
) -> Result<ExecutionTerminalDecision, StorageError> {
    let job = storage
        .load_execution_job_record(&message.lease.job_id)?
        .ok_or_else(|| StorageError::invalid_input("terminal queue authority is missing"))?;
    if job.state == ExecutionJobState::Cancelling {
        let cancellation = job
            .cancellation
            .clone()
            .ok_or_else(|| StorageError::adapter("cancelling Job has no cancellation intent"))?;
        return Ok(ExecutionTerminalDecision {
            status: ExecutionOutcomeStatus::Cancelled,
            terminal_at: Instant(
                accepted_at
                    .0
                    .clone()
                    .max(job.updated_at.0)
                    .max(message.outcome.finished_at.0.clone()),
            ),
            cancellation: Some(cancellation),
        });
    }
    Ok(ExecutionTerminalDecision {
        status: message.outcome.status.clone(),
        terminal_at: message.outcome.finished_at.clone(),
        cancellation: None,
    })
}

fn validate_decision(
    decision: &ExecutionTerminalDecision,
    job: &ExecutionJobRecord,
    message: &JobOutcomeMessage,
) -> Result<(), StorageError> {
    let valid = if job.state == ExecutionJobState::Cancelling {
        decision.status == ExecutionOutcomeStatus::Cancelled
            && decision.cancellation.is_some()
            && decision.cancellation == job.cancellation
            && decision.terminal_at.0 >= job.updated_at.0
            && decision.terminal_at.0 >= message.outcome.finished_at.0
    } else {
        decision.cancellation.is_none()
            && decision.status == message.outcome.status
            && decision.terminal_at == message.outcome.finished_at
    };
    if !valid {
        return Err(StorageError::invalid_input(
            "terminal decision differs from current cancellation authority",
        ));
    }
    Ok(())
}

/// Delivery continuation retained with the accepted terminal, including whether
/// the accepting host owned automatic progression. Actor comes from the original
/// durable dispatch command; it never comes from Worker-controlled prose.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AcceptedOutcomeFollowUp {
    pub completed_profile: String,
    pub initiating_actor: PublicEventActor,
    pub controller_required: bool,
}

/// The immutable accepted frame also retains the exact resources needed to
/// resume cleanup. The acknowledgement watermark grants no invocation rights.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AcceptedExecutionOutcome {
    pub message: JobOutcomeMessage,
    pub decision: ExecutionTerminalDecision,
    pub scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub authority: WorkerSlotAuthority,
    receipt_actor: Vec<u8>,
    receipt_scope: Vec<u8>,
    pub product_revision: u64,
    pub acknowledgement_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<AcceptedOutcomeFollowUp>,
}

fn stream_id(message: &JobOutcomeMessage) -> String {
    format!(
        "execution-outcome:{}:{}",
        message.lease.job_id.0, message.lease.attempt
    )
}

/// Resource cleanup may use the frozen product decision while its raw Worker
/// frame remains the receipt key. This proof grants accounting rights only.
pub(crate) fn is_cancellation_cleanup(
    storage: &dyn ProductStateStorage,
    effective: &JobOutcomeMessage,
) -> Result<bool, StorageError> {
    let Some(stored) = storage.load_state(&stream_id(effective))? else {
        return Ok(false);
    };
    let accepted: AcceptedExecutionOutcome = serde_json::from_slice(&stored.payload)
        .map_err(|_| StorageError::adapter("accepted execution outcome cannot be decoded"))?;
    accepted.decision.validate_retained(&accepted.message)?;
    Ok(accepted.decision.cancellation.is_some()
        && accepted.decision.apply(&accepted.message) == *effective
        && stored.revision == if accepted.acknowledgement_ready { 2 } else { 1 })
}

pub(crate) enum OutcomeReplay {
    Missing,
    AuthorityConflict,
    FrameConflict,
    Exact(Box<AcceptedExecutionOutcome>),
}

pub(crate) fn outcome_replay(
    storage: &dyn ProductStateStorage,
    message: &JobOutcomeMessage,
) -> Result<OutcomeReplay, StorageError> {
    let Some(stored) = storage.load_state(&stream_id(message))? else {
        return Ok(OutcomeReplay::Missing);
    };
    let retained: AcceptedExecutionOutcome = serde_json::from_slice(&stored.payload)
        .map_err(|_| StorageError::adapter("accepted execution outcome cannot be decoded"))?;
    retained.decision.validate_retained(&retained.message)?;
    if retained.follow_up.is_some() != retained.scope.delivery_id.is_some() {
        return Err(StorageError::adapter(
            "accepted execution continuation differs from its product scope",
        ));
    }
    if stored.revision != if retained.acknowledgement_ready { 2 } else { 1 } {
        return Err(StorageError::adapter(
            "execution acknowledgement watermark differs",
        ));
    }
    if retained.message.lease != message.lease
        || retained.message.worker_session_id != message.worker_session_id
        || retained.message.session_identity != message.session_identity
    {
        return Ok(OutcomeReplay::AuthorityConflict);
    }
    if retained.message != *message {
        return Ok(OutcomeReplay::FrameConflict);
    }
    Ok(OutcomeReplay::Exact(Box::new(retained)))
}

fn retained_outcome(
    storage: &dyn ProductStateStorage,
    message: &JobOutcomeMessage,
) -> Result<Option<AcceptedExecutionOutcome>, StorageError> {
    match outcome_replay(storage, message)? {
        OutcomeReplay::Missing => Ok(None),
        OutcomeReplay::Exact(retained) => Ok(Some(*retained)),
        OutcomeReplay::AuthorityConflict => Err(StorageError::invalid_input(
            "accepted outcome authority differs",
        )),
        OutcomeReplay::FrameConflict => Err(StorageError::invalid_input(
            "job.outcome differs from its accepted original frame",
        )),
    }
}

pub(crate) fn commit_accepted_outcome(
    storage: &mut dyn ProductStateStorage,
    commit: StateCommit,
    outcome: &ExecutionOutcomeCommit<'_>,
) -> Result<CommitReceipt, StorageError> {
    let message = outcome.message;
    let retained = retained_outcome(storage, message)?;
    if let Some(receipt) = storage.load_receipt(&commit.receipt_identity, &commit.command_digest)? {
        if retained.is_none() {
            return Err(StorageError::adapter(
                "terminal receipt has no accepted execution outcome",
            ));
        }
        return Ok(receipt);
    }
    if retained.is_some() {
        return Err(StorageError::invalid_input(
            "execution outcome already belongs to another receipt",
        ));
    }
    let job = storage
        .load_execution_job_record(&message.lease.job_id)?
        .ok_or_else(|| StorageError::invalid_input("terminal queue authority is missing"))?;
    let lease = outcome.dispatch.lease();
    let authority = outcome.authority;
    if job.scope != *outcome.scope
        || authority.job_id != message.lease.job_id
        || authority.lease_id != message.lease.lease_id
        || authority.worker_id != message.lease.worker_id
        || authority.worker_instance_id != message.lease.worker_instance_id
        || Some(authority.attempt) != u64::try_from(message.lease.attempt).ok()
        || authority.fencing_token != message.lease.fencing_token
        || authority.worker_session_id != message.worker_session_id
        || authority.codex_thread_id != message.session_identity.codex_thread_id
        || authority.job_id != lease.job_id
        || authority.lease_id != lease.lease_id
        || authority.worker_id != lease.worker_id
        || authority.worker_instance_id != lease.worker_instance_id
        || authority.attempt != lease.attempt
        || authority.fencing_token != lease.fencing_token
        || authority.worker_session_id != *outcome.dispatch.worker_session_id()
    {
        return Err(StorageError::invalid_input(
            "terminal facts differ from dispatch and cleanup authority",
        ));
    }
    validate_decision(outcome.decision, &job, message)?;
    if outcome.follow_up.is_some() != outcome.scope.delivery_id.is_some() {
        return Err(StorageError::invalid_input(
            "execution continuation differs from its product scope",
        ));
    }
    let request = terminal_request(&outcome.decision.apply(message), &job.scope)?;
    let guard = ExecutionAuthorityCommitGuard::terminal_round(job, outcome.dispatch.clone())?;
    let retained = AcceptedExecutionOutcome {
        message: message.clone(),
        decision: outcome.decision.clone(),
        scope: outcome.scope.clone(),
        worker_pool_id: outcome.worker_pool_id.clone(),
        authority: authority.clone(),
        receipt_actor: commit.receipt_identity.actor_key().as_bytes().to_vec(),
        receipt_scope: commit.receipt_identity.scope_key().as_bytes().to_vec(),
        product_revision: commit
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::invalid_input("terminal product revision overflowed"))?,
        acknowledgement_ready: false,
        follow_up: outcome.follow_up.cloned(),
    };
    let commit = commit.with_state_mutation(StateMutation::new(
        stream_id(message),
        0,
        encode(&retained)?,
    )?);
    storage.commit_with_execution_terminal(&commit, &guard, &request)
}

/// Call only after exact cross-store cleanup and required publication succeeds.
pub(crate) fn mark_acknowledgement_ready(
    storage: &mut dyn ProductStateStorage,
    message: &JobOutcomeMessage,
) -> Result<(), StorageError> {
    let mut retained = retained_outcome(storage, message)?
        .ok_or_else(|| StorageError::invalid_input("execution outcome is not accepted"))?;
    if retained.acknowledgement_ready {
        return Ok(());
    }
    retained.acknowledgement_ready = true;
    let bytes = encode(&retained)?;
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(retained.receipt_actor.clone())?,
        ReceiptScopeKey::from_encoded(retained.receipt_scope.clone())?,
        crate::session_binding_transaction::execution_message_request_id(
            &message.message_id,
            &message.lease,
            "outcome-acknowledgement-ready",
        )?,
    )?;
    let digest = winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let event = NewOutboxEvent::internal(
        format!("execution-outcome-ready:{}", identity.request_id().0),
        "execution-outcome.acknowledgement-ready.v1",
        serde_json::to_vec(&serde_json::json!({"jobId": message.lease.job_id, "attempt": message.lease.attempt, "messageId": message.message_id}))
            .map_err(|_| StorageError::adapter("execution acknowledgement event cannot be encoded"))?,
    );
    storage.commit(&StateCommit::new(
        identity,
        digest,
        stream_id(message),
        1,
        bytes,
        vec![event],
    ))?;
    Ok(())
}

fn encode(retained: &AcceptedExecutionOutcome) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(retained)
        .map_err(|_| StorageError::adapter("execution outcome cannot be encoded"))
}

fn terminal_request(
    message: &JobOutcomeMessage,
    scope: &ExecutionQueueScope,
) -> Result<RepositorySchedulerTerminalRequest, StorageError> {
    Ok(RepositorySchedulerTerminalRequest {
        scope: RepositorySchedulerScope {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        terminal: ExecutionLeaseTerminalRequest {
            job_id: message.lease.job_id.clone(),
            lease_id: message.lease.lease_id.clone(),
            worker_id: message.lease.worker_id.clone(),
            worker_instance_id: message.lease.worker_instance_id.clone(),
            attempt: u64::try_from(message.lease.attempt)
                .map_err(|_| StorageError::invalid_input("terminal attempt is invalid"))?,
            fencing_token: message.lease.fencing_token.clone(),
            outcome: match message.outcome.status {
                ExecutionOutcomeStatus::Succeeded => ExecutionLeaseTerminalOutcome::Completed,
                ExecutionOutcomeStatus::Cancelled => ExecutionLeaseTerminalOutcome::Cancelled,
                ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
                    ExecutionLeaseTerminalOutcome::Failed
                }
            },
            terminal_at: message.outcome.finished_at.clone(),
            request_id: crate::product_session_execution_application::execution_message_request_id(
                b"execution-terminal-receipt",
                &message.lease,
                &message.message_id,
            ),
        },
    })
}

impl crate::ControlPlane {
    /// Completes only already accepted facts. Cleanup and optional Delivery
    /// continuation grant no new rights to the retained Worker frame.
    pub(crate) fn finish_accepted_execution_outcome(
        &mut self,
        repository_scope: &winwincode_domain::RepositoryScope,
        message: &JobOutcomeMessage,
        resume_manual_controller: bool,
    ) -> Result<(), crate::DurableExecutionPortError> {
        use crate::DurableExecutionPortError as Error;
        let accepted = retained_outcome(self.storage_ref().map_err(Error::Storage)?, message)
            .map_err(Error::Storage)?
            .ok_or_else(|| {
                Error::Storage(StorageError::invalid_input(
                    "execution outcome is not accepted",
                ))
            })?;
        if accepted.scope.organization_id != repository_scope.organization_id
            || accepted.scope.workspace_id != repository_scope.workspace_id
            || accepted.scope.project_id != repository_scope.project_id
            || accepted.scope.repository_id != repository_scope.repository_id
        {
            return Err(Error::Storage(StorageError::invalid_input(
                "accepted outcome scope differs",
            )));
        }
        if accepted.acknowledgement_ready
            && (!resume_manual_controller
                || accepted
                    .follow_up
                    .as_ref()
                    .is_none_or(|follow_up| follow_up.controller_required))
        {
            return Ok(());
        }
        if !accepted.acknowledgement_ready {
            let database_path = self.local_database_path.as_ref().ok_or_else(|| {
                Error::Storage(StorageError::adapter(
                    "accepted outcome resource completion requires the configured local store",
                ))
            })?;
            let directory = database_path.parent().ok_or_else(|| {
                Error::Storage(StorageError::adapter(
                    "accepted outcome local store has no data directory",
                ))
            })?;
            let mut storage = SqliteStorage::open(directory).map_err(Error::Storage)?;
            if storage.database_path() != database_path {
                return Err(Error::Configuration);
            }
            crate::product_session_execution_application::finish_execution_resources(
                &mut storage,
                &accepted.decision.apply(message),
                &accepted.scope,
                &accepted.worker_pool_id,
                &accepted.authority,
            )?;
        }
        if let Some(follow_up) = &accepted.follow_up {
            if !accepted.acknowledgement_ready {
                self.flush_outbox().map_err(Error::Publication)?;
            }
            let effective = accepted.decision.apply(message);
            let should_continue = effective.outcome.status == ExecutionOutcomeStatus::Succeeded
                || (effective.outcome.status == ExecutionOutcomeStatus::Failed
                    && effective.outcome.error.as_ref().is_some_and(|error| {
                        error.code == winwincode_execution_port::generated::ExecutionPortErrorCode::CandidateUnchanged
                    }));
            if should_continue && (follow_up.controller_required || resume_manual_controller) {
                if follow_up.controller_required
                    && (self.delivery_authority.is_none() || self.delivery_dispatcher.is_none())
                {
                    return Err(Error::Controller(
                        crate::DeliveryApplicationError::TrustedFactsUnavailable(
                            "accepted Delivery continuation requires its configured Controller"
                                .into(),
                        ),
                    ));
                }
                let delivery_id = accepted.scope.delivery_id.as_ref().ok_or_else(|| {
                    Error::Storage(StorageError::adapter(
                        "accepted Delivery continuation has no Delivery identity",
                    ))
                })?;
                self.continue_delivery_after_terminal(
                    repository_scope,
                    delivery_id,
                    &follow_up.completed_profile,
                    &message.lease.job_id,
                    accepted.product_revision,
                    &follow_up.initiating_actor,
                )
                .map_err(Error::Controller)?;
            }
        }
        if accepted.acknowledgement_ready {
            // A manually accepted terminal can later enter the configured
            // Controller. Its own receipt and current Delivery revision guard
            // that continuation; the Worker acknowledgement stays unchanged.
            return Ok(());
        }
        mark_acknowledgement_ready(self.storage_mut().map_err(Error::Storage)?, message)
            .map_err(Error::Storage)
    }
}
