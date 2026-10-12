// SPDX-License-Identifier: Apache-2.0

use std::{fmt, future::Future, path::Path, sync::Arc};

use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    CodexThreadId, Instant, Sha256Digest, WorkerId, WorkerInstanceId, WorkerSessionId,
    WorkspaceRevision,
};
use winwincode_execution_port::{
    agent_config::AgentSessionConfigSnapshot,
    execution_identity::{canonical_codex_thread_id, canonical_execution_run_digest},
    generated::{
        ActionEnforcementReceiptMessage, ApprovalDecisionMessage, ArtifactAckMessage,
        ArtifactReference, ChangeBatchProgressEvent, ChangeBatchProposalEvent, ExecutionJob,
        ExecutionLeaseStamp, ExecutionOutcomeUsage, ExecutionPortMessage, FinalCandidateFreezeFact,
        InputResponseMessage, JobDispatchMessage, JobOutcomeMessage, ModelChunkMessage,
        RepairEnvelope, RepairLoopBudget, RepairLoopContextPack, RepairLoopCounters,
        RepairLoopStopReason, RuntimeEventMessage, RuntimeReplayRequestMessage,
    },
    runtime_trace_outbox::{RuntimeTraceInputError, SecretSafeTraceSummary},
};

use crate::candidate_artifact_outbox::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    RetainedCandidateArtifact,
};
use crate::diagnostic_artifact_outbox::{
    DiagnosticArtifactAuthority, DiagnosticArtifactUpload, RetainedDiagnosticArtifact,
};
use crate::failure_diagnostic::CodexFailureDiagnostic;

pub type LocalModelStartGuard = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodexRunKey {
    pub job_id: winwincode_domain::ExecutionJobId,
    pub attempt: i64,
    pub fencing_token: winwincode_domain::FencingToken,
    pub payload_digest: Sha256Digest,
}

impl CodexRunKey {
    #[must_use]
    pub fn from_dispatch(dispatch: &JobDispatchMessage) -> Self {
        Self {
            job_id: dispatch.job.job_id.clone(),
            attempt: dispatch.job.attempt,
            fencing_token: dispatch.lease.fencing_token.clone(),
            payload_digest: dispatch.job.payload_digest.clone(),
        }
    }

    pub fn canonical_thread_id(&self) -> Result<CodexThreadId, CodexRunKeyError> {
        canonical_codex_thread_id(
            &self.job_id,
            self.attempt,
            &self.fencing_token,
            &self.payload_digest,
        )
        .map_err(|_| CodexRunKeyError)
    }

    pub fn canonical_digest(&self) -> Result<Sha256Digest, CodexRunKeyError> {
        canonical_execution_run_digest(
            &self.job_id,
            self.attempt,
            &self.fencing_token,
            &self.payload_digest,
        )
        .map_err(|_| CodexRunKeyError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexRunKeyError;

impl fmt::Display for CodexRunKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("canonical Codex run identity is invalid")
    }
}

impl std::error::Error for CodexRunKeyError {}

#[cfg(test)]
mod tests {
    use winwincode_domain::{ExecutionJobId, FencingToken, Sha256Digest};

    use super::CodexRunKey;

    #[test]
    fn canonical_digest_has_one_stable_full_length_vector() {
        let key = CodexRunKey {
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            attempt: 1,
            fencing_token: FencingToken("7".to_owned()),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        };

        assert_eq!(
            key.canonical_digest().expect("canonical run digest"),
            Sha256Digest(
                "sha256:809025db6972e3dfc1f4e61b3e908e79f70871e62e29bebf615c3656a6adc6ac"
                    .to_owned()
            )
        );
        assert_eq!(
            key.canonical_thread_id()
                .expect("canonical thread identity")
                .0,
            "cdx_809025DB6972E3DFC1F4E61B3E"
        );
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodexThreadStart<'job> {
    pub snapshot_id: Option<&'job winwincode_domain::SnapshotId>,
    pub run_key: &'job CodexRunKey,
    pub worker_id: &'job WorkerId,
    pub job: &'job ExecutionJob,
    pub lease: &'job ExecutionLeaseStamp,
    pub worker_session_id: &'job WorkerSessionId,
    pub workspace: &'job Path,
    pub workspace_revision: &'job WorkspaceRevision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexThreadSession {
    pub thread_id: CodexThreadId,
    pub agent_config: AgentSessionConfigSnapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexTurnCompletion {
    pub summary: SecretSafeTraceSummary,
    pub artifacts: Vec<ArtifactReference>,
    pub usage: Option<ExecutionOutcomeUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedLoopPhase {
    Continue,
    Repair,
}

impl DelegatedLoopPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Repair => "repair",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegatedLoopTransition {
    pub phase: DelegatedLoopPhase,
    pub repair_round: i64,
    pub context: RepairLoopContextPack,
    pub budget: RepairLoopBudget,
    pub worker_counters: RepairLoopCounters,
    pub observed_at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DelegatedLoopTransitionOutcome {
    Submitted {
        turn_id: String,
        counters: RepairLoopCounters,
    },
    Completed {
        turn_id: String,
        counters: RepairLoopCounters,
    },
    Stopped {
        reason: RepairLoopStopReason,
        counters: RepairLoopCounters,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DelegatedObserverPreflight {
    pub batch_id: winwincode_domain::ChangeBatchId,
    pub budget: RepairLoopBudget,
    pub worker_counters: RepairLoopCounters,
    pub observed_at: Instant,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegatedObserverSettlement {
    pub batch_id: winwincode_domain::ChangeBatchId,
    pub completed_at: Instant,
    pub usage: Option<ExecutionOutcomeUsage>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DelegatedObserverPreflightOutcome {
    Allowed {
        counters: RepairLoopCounters,
    },
    Stopped {
        reason: RepairLoopStopReason,
        counters: RepairLoopCounters,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegatedLoopStopFact {
    pub batch_id: winwincode_domain::ChangeBatchId,
    pub reason: RepairLoopStopReason,
    pub counters: RepairLoopCounters,
    pub stopped_at: Instant,
}

#[must_use]
pub fn delegated_loop_turn_id(transition: &DelegatedLoopTransition) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.delegated-loop-turn.v1\0");
    for part in [
        transition.context.identity.run_key.as_bytes(),
        transition.phase.as_str().as_bytes(),
        transition.context.identity.batch_id.0.as_bytes(),
        transition.repair_round.to_string().as_bytes(),
    ] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part);
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("trn_{}", &digest[..26].to_ascii_uppercase())
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodexPoll {
    Pending,
    RuntimeTrace(Box<RuntimeEventMessage>),
    ChangeBatchProposed(Box<ChangeBatchProposalEvent>),
    ChangeBatchProgress(Box<ChangeBatchProgressEvent>),
    RepairRequired(Box<RepairEnvelope>),
    Completed(CodexTurnCompletion),
    CompletedWithDiagnostics(CodexTurnCompletion, Vec<ArtifactReference>),
    Inconclusive(SecretSafeTraceSummary),
    Failed(SecretSafeTraceSummary),
    FailedWithDiagnostics(SecretSafeTraceSummary, Vec<ArtifactReference>),
    Cancelled(SecretSafeTraceSummary),
    CancelledWithDiagnostics(SecretSafeTraceSummary, Vec<ArtifactReference>),
    InfrastructureFailed(SecretSafeTraceSummary),
    InfrastructureFailedWithDiagnostics(SecretSafeTraceSummary, Vec<ArtifactReference>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DurableExecutionDelivery {
    pub delivery_id: String,
    pub message: ExecutionPortMessage,
}

pub type ActionRequestTransport =
    Arc<dyn Fn(ExecutionPortMessage) -> Result<Vec<ExecutionPortMessage>, ()> + Send + Sync>;

pub trait CodexCoreAdapter {
    type Error: Send + 'static;

    fn model_start_session_allowed(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        parent: &winwincode_domain::SessionIdentity,
    ) -> Result<bool, Self::Error> {
        Ok(open.session_identity == *parent)
    }

    fn local_model_start_guard(
        &mut self,
        _open: &winwincode_execution_port::generated::ModelOpenMessage,
        _now: &Instant,
        _observed_at: std::time::Instant,
    ) -> Result<Option<LocalModelStartGuard>, Self::Error> {
        Ok(None)
    }

    fn retained_outcome_usage(
        &mut self,
        _thread_id: &CodexThreadId,
    ) -> Result<Option<ExecutionOutcomeUsage>, Self::Error> {
        Ok(None)
    }

    fn recovered_worker_session_id(
        &mut self,
        _dispatch: &JobDispatchMessage,
    ) -> Result<Option<WorkerSessionId>, Self::Error> {
        Ok(None)
    }

    fn retained_failure_diagnostic(
        &self,
        _thread_id: &CodexThreadId,
    ) -> Option<CodexFailureDiagnostic> {
        None
    }

    fn error_diagnostic(&self, _error: &Self::Error) -> Option<CodexFailureDiagnostic> {
        None
    }

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadSession, Self::Error>> + Send;

    fn observe_now(&mut self, _now: &Instant) -> Result<(), Self::Error> {
        Ok(())
    }

    fn renew_lease(
        &mut self,
        _thread_id: &CodexThreadId,
        _renewal: &winwincode_execution_port::generated::LeaseRenewMessage,
        _now: &Instant,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn install_action_request_transport(&mut self, _transport: ActionRequestTransport) {}

    fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        goal: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn reconcile_delegated_transition(
        &mut self,
        _thread_id: &CodexThreadId,
        transition: DelegatedLoopTransition,
    ) -> impl Future<Output = Result<DelegatedLoopTransitionOutcome, Self::Error>> + Send {
        async move {
            Ok(DelegatedLoopTransitionOutcome::Stopped {
                reason: RepairLoopStopReason::InfrastructureError,
                counters: transition.worker_counters,
            })
        }
    }

    fn preflight_delegated_observer(
        &mut self,
        _thread_id: &CodexThreadId,
        preflight: DelegatedObserverPreflight,
    ) -> Result<DelegatedObserverPreflightOutcome, Self::Error> {
        Ok(DelegatedObserverPreflightOutcome::Stopped {
            reason: RepairLoopStopReason::InfrastructureError,
            counters: preflight.worker_counters,
        })
    }

    fn retain_delegated_observer_settlement(
        &mut self,
        _thread_id: &CodexThreadId,
        _settlement: DelegatedObserverSettlement,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn retain_delegated_loop_stop(
        &mut self,
        _thread_id: &CodexThreadId,
        fact: &DelegatedLoopStopFact,
    ) -> Result<DelegatedLoopStopFact, Self::Error> {
        Ok(fact.clone())
    }

    fn delegated_loop_stop(
        &mut self,
        _thread_id: &CodexThreadId,
    ) -> Result<Option<DelegatedLoopStopFact>, Self::Error> {
        Ok(None)
    }

    fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        now: &Instant,
    ) -> impl Future<Output = Result<CodexPoll, Self::Error>> + Send;

    fn release_poll_delivery(
        &mut self,
        _thread_id: &CodexThreadId,
        _delivery: &CodexPoll,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn accept_model_chunk(
        &mut self,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn accept_action_receipt(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn accept_approval_decision(
        &mut self,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn accept_input_response(
        &mut self,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error>;

    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error>;

    fn pending_execution_delivery_batch(
        &mut self,
        after_delivery: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let deliveries = self.pending_execution_deliveries()?;
        let start = after_delivery
            .and_then(|id| deliveries.iter().position(|row| row.delivery_id == id))
            .map_or(0, |index| index + 1);
        Ok(deliveries.into_iter().skip(start).take(limit).collect())
    }

    fn recovered_message_sequence(&mut self) -> Result<u64, Self::Error> {
        Ok(0)
    }

    fn recovered_heartbeat_sequence(
        &mut self,
        _worker_id: &WorkerId,
        _worker_instance_id: &WorkerInstanceId,
    ) -> Result<i64, Self::Error> {
        Ok(0)
    }

    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error>;

    fn requeue_execution_delivery(&mut self, _delivery_id: &str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn degrade_auxiliary_execution_delivery(
        &mut self,
        _delivery_id: &str,
    ) -> Result<Option<DurableExecutionDelivery>, Self::Error> {
        Ok(None)
    }

    fn record_execution_delivery_rejected(
        &mut self,
        _delivery_id: &str,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn execution_delivery_is_rejected(&mut self, _delivery_id: &str) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn has_rejected_terminal_delivery(
        &mut self,
        _worker_id: &WorkerId,
        _instance_id: &WorkerInstanceId,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn has_rejected_job_delivery(
        &mut self,
        _lease: &ExecutionLeaseStamp,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn fail_required_execution_delivery(
        &mut self,
        thread_id: &CodexThreadId,
        at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.interrupt(thread_id, at)
    }

    fn accept_execution_delivery_ack(
        &mut self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error>;

    fn retain_candidate_artifact(
        &mut self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error>;

    fn accept_candidate_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error>;

    fn accept_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<ArtifactAckOutcome, Self::Error>;

    fn retain_diagnostic_artifact(
        &mut self,
        upload: &DiagnosticArtifactUpload,
    ) -> Result<RetainedDiagnosticArtifact, Self::Error>;

    fn accepted_diagnostic_artifacts(
        &mut self,
        authority: &DiagnosticArtifactAuthority,
    ) -> Result<Vec<ArtifactReference>, Self::Error>;

    fn accepted_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error>;

    fn retain_final_candidate_freeze(
        &mut self,
        _thread_id: &CodexThreadId,
        fact: &FinalCandidateFreezeFact,
    ) -> Result<FinalCandidateFreezeFact, Self::Error> {
        Ok(fact.clone())
    }

    fn final_candidate_freeze(
        &mut self,
        _thread_id: &CodexThreadId,
    ) -> Result<Option<FinalCandidateFreezeFact>, Self::Error> {
        Ok(None)
    }

    fn begin_candidate_artifact_cancel(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn candidate_artifact_delivery_allowed(
        &mut self,
        _message: &ExecutionPortMessage,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn cancel_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error>;

    fn replay_execution_deliveries(
        &mut self,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error>;

    fn retain_job_outcome(
        &mut self,
        thread_id: &CodexThreadId,
        outcome: &JobOutcomeMessage,
    ) -> impl Future<Output = Result<DurableExecutionDelivery, Self::Error>> + Send;

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error>;

    fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        interrupted_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn close_thread(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArtifactAckOutcome {
    Pending,
    Replay(Vec<DurableExecutionDelivery>),
    Accepted(ArtifactReference),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionPortFailureKind {
    Backpressure,
    Unavailable,
    MessageRejected,
    Terminal,
}

pub trait WorkerExecutionPort {
    type Error;

    fn has_pending_controls(&self) -> bool {
        false
    }

    fn failure_kind(_error: &Self::Error) -> ExecutionPortFailureKind {
        ExecutionPortFailureKind::Unavailable
    }

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

pub fn secret_safe_runtime_summary(
    value: impl Into<String>,
) -> Result<SecretSafeTraceSummary, RuntimeTraceInputError> {
    SecretSafeTraceSummary::new(value)
}
