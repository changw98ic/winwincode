// SPDX-License-Identifier: Apache-2.0

//! Single pre-action gateway for Codex Core tool side effects.
//!
//! The gateway does not implement another tool runtime. It validates the
//! current lease, `WorkerSession`, `SessionIdentity`, and Execution Envelope token,
//! normalizes the real Codex tool request, runs one injected deterministic
//! gate, and only then forwards an approved request to the injected Codex tool
//! executor.

use std::fmt;

use winwincode_domain::{Instant, RequestId, Sha256Digest, WorkerSessionId};

use crate::action_enforcement::{
    ActionEnforcementError, ActionEnforcementVerifier, ActionReceiptClaim, ActionReceiptUseError,
    ActionReceiptUseStore,
};
use crate::action_normalizer::{
    ActionIntent, ActionNormalization, ActionNormalizationError, IntentMismatch, ObservedAction,
    ToolRequest, normalize_action,
};
use crate::generated::{ActionEnforcementReceiptMessage, ExecutionLeaseStamp};
use winwincode_domain::SessionIdentity;

/// Version and digest bound into a Worker's execution token.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionEnvelopeToken {
    /// Monotonically increasing compiled-envelope version.
    pub version: u64,
    /// Digest of the exact compiled envelope contents.
    pub digest: Sha256Digest,
}

/// The current compiled policy and the token which identifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionEnvelope<Policy> {
    /// Token presented by every action authorized under this envelope.
    pub token: ExecutionEnvelopeToken,
    /// Compiled deterministic policy consumed by the injected gate.
    pub policy: Policy,
}

/// Exact active `WorkerSession` authority accepted by the gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveWorkerAuthority {
    /// Current scheduler lease including attempt and fencing token.
    pub lease: ExecutionLeaseStamp,
    /// Current `WorkerSession` identity.
    pub worker_session_id: WorkerSessionId,
    /// Current `ProductSession`, `StageRun`, `WorkerSession`, and `CodexThread` binding.
    pub session_identity: SessionIdentity,
}

/// Authority presented by one pending Codex Core tool request.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerActionAuthority {
    /// Lease under which the action was prepared.
    pub lease: ExecutionLeaseStamp,
    /// `WorkerSession` under which the action was prepared.
    pub worker_session_id: WorkerSessionId,
    /// Session binding under which the action was prepared.
    pub session_identity: SessionIdentity,
    /// Execution Envelope token under which the action was prepared.
    pub envelope: ExecutionEnvelopeToken,
}

/// One pending action submitted to the only Codex Core side-effect gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerActionRequest {
    /// Stable action invocation identity allocated before Policy evaluation.
    pub invocation_request_id: RequestId,
    /// Lease, session, and envelope authority presented by the Worker.
    pub authority: WorkerActionAuthority,
    /// Executor-declared intent.
    pub intent: ActionIntent,
    /// Actual typed Codex tool request to normalize and, if approved, execute.
    pub request: ToolRequest,
}

/// Deterministic gate outcome. Only the two allow outcomes authorize execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Execute the action without an observer notification.
    Allow,
    /// Execute the action and emit the supplied reason to the observer path.
    AllowWithWatch { reason: String },
    /// Stop before execution and request a bounded plan change.
    RequestPlanDelta { reason: String },
    /// Stop before execution and route the action to human attention.
    PauseForHuman { reason: String },
    /// Stop before execution because the action is forbidden.
    DenyAction { reason: String },
    /// Stop before execution because the current plan is no longer valid.
    ReplanRequired { reason: String },
}

impl GateDecision {
    const fn authorizes_execution(&self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithWatch { .. })
    }
}

/// Complete deterministic input presented to the gate.
#[derive(Debug, Clone, Copy)]
pub struct GateInput<'action, Policy> {
    /// Exact current compiled Execution Envelope.
    pub envelope: &'action ExecutionEnvelope<Policy>,
    /// Validated executor intent.
    pub intent: &'action ActionIntent,
    /// Normalized action derived from the real tool request.
    pub observed: &'action ObservedAction,
}

/// Injected deterministic policy evaluator.
///
/// This trait owns policy evaluation only. It cannot run the tool request.
pub trait DeterministicActionGate<Policy> {
    /// Decides one already-authority-checked and already-normalized action.
    fn decide(&mut self, input: GateInput<'_, Policy>) -> GateDecision;
}

/// Durable pre-action journal for one deterministic gate decision.
///
/// Implementations append to the Worker runtime `TraceOutbox`. The gateway calls
/// this after the gate decides and before either executing or returning a
/// structured stop decision.
pub trait PreActionDecisionRecorder<Policy> {
    /// Durable journal failure.
    type Error;

    /// Persists one gate decision before any tool side effect.
    ///
    /// # Errors
    ///
    /// Returns an outbox or identity-allocation failure. The gateway then
    /// stops without invoking the Codex tool executor.
    fn record(
        &mut self,
        input: GateInput<'_, Policy>,
        decision: &GateDecision,
    ) -> Result<(), Self::Error>;
}

/// The existing Codex Core tool runtime seam.
///
/// Implementations delegate to the embedded Codex Core runtime. The gateway
/// intentionally defines no file, Git, shell, network, or MCP implementation.
pub trait CodexToolExecutor {
    /// Successful tool result.
    type Output;
    /// Error returned by the existing Codex Core tool runtime.
    type Error;

    /// Executes one request after the gateway has authorized it.
    ///
    /// # Errors
    ///
    /// Returns the existing Codex Core tool error.
    fn execute(&mut self, request: &ToolRequest) -> Result<Self::Output, Self::Error>;
}

/// Successful gateway result, including the exact decision and normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedAction<Output> {
    /// Normalized action which the gate approved.
    pub normalization: ActionNormalization,
    /// Exact allowing gate decision.
    pub decision: GateDecision,
    /// Result returned by the embedded Codex Core tool runtime.
    pub output: Output,
}

/// Result returned by the canonical Action Gateway execution point.
pub type ActionGatewayResult<Output, RecorderError, ExecutorError> =
    Result<ExecutedAction<Output>, ActionGatewayError<RecorderError, ExecutorError>>;

/// Stable pre-execution rejection category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionGatewayRejectionCode {
    /// The current authority itself is internally invalid.
    InvalidCurrentAuthority,
    /// The presented lease differs from the active lease or fencing token.
    StaleLease,
    /// The exact current lease has expired.
    ExpiredLease,
    /// The presented `WorkerSession` differs from the active `WorkerSession`.
    StaleWorkerSession,
    /// The presented `SessionIdentity` differs from the active binding.
    StaleSessionIdentity,
    /// The presented envelope token differs from the current envelope.
    StaleExecutionEnvelope,
    /// The real request does not match the executor-declared intent.
    IntentMismatch,
    /// The deterministic gate did not authorize the action.
    ActionNotApproved,
    /// The Control Plane receipt is invalid or does not match this action.
    InvalidActionReceipt,
    /// This exact action receipt was already consumed.
    ActionReceiptConsumed,
    /// Durable receipt-use state conflicts with this invocation.
    ActionReceiptConflict,
    /// Durable receipt-use state is unavailable.
    ActionReceiptUnavailable,
}

/// Gateway failure. All variants except [`Self::Executor`] happen before a side effect.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionGatewayError<RecorderError, ExecutorError> {
    /// A stable fail-closed authority or decision rejection.
    Rejected {
        /// Stable rejection category.
        code: ActionGatewayRejectionCode,
        /// Secret-free explanation.
        reason: String,
    },
    /// The actual tool request or intent could not be normalized.
    Normalization(ActionNormalizationError),
    /// Intent differs from the actual normalized request.
    IntentMismatch(Vec<IntentMismatch>),
    /// Gate outcome stopped execution.
    NotApproved(GateDecision),
    /// The Control Plane receipt failed verification.
    ActionEnforcement(ActionEnforcementError),
    /// The durable receipt-use claim failed.
    ReceiptUse(ActionReceiptUseError),
    /// The gate decided, but its control event was not durably retained.
    DecisionRecord(RecorderError),
    /// The authorized request reached Codex Core and its existing runtime failed.
    Executor(ExecutorError),
}

impl<RecorderError, ExecutorError> ActionGatewayError<RecorderError, ExecutorError> {
    /// Returns a stable rejection code for errors raised before execution.
    #[must_use]
    pub const fn rejection_code(&self) -> Option<ActionGatewayRejectionCode> {
        match self {
            Self::Rejected { code, .. } => Some(*code),
            Self::Normalization(_) | Self::DecisionRecord(_) | Self::Executor(_) => None,
            Self::ActionEnforcement(_) => Some(ActionGatewayRejectionCode::InvalidActionReceipt),
            Self::ReceiptUse(ActionReceiptUseError::Conflict) => {
                Some(ActionGatewayRejectionCode::ActionReceiptConflict)
            }
            Self::ReceiptUse(ActionReceiptUseError::Storage) => {
                Some(ActionGatewayRejectionCode::ActionReceiptUnavailable)
            }
            Self::IntentMismatch(_) => Some(ActionGatewayRejectionCode::IntentMismatch),
            Self::NotApproved(_) => Some(ActionGatewayRejectionCode::ActionNotApproved),
        }
    }
}

impl<RecorderError: fmt::Display, ExecutorError: fmt::Display> fmt::Display
    for ActionGatewayError<RecorderError, ExecutorError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { reason, .. } => formatter.write_str(reason),
            Self::Normalization(error) => write!(formatter, "action normalization failed: {error}"),
            Self::IntentMismatch(mismatches) => write!(
                formatter,
                "action intent does not match {} observed field(s)",
                mismatches.len()
            ),
            Self::NotApproved(decision) => {
                write!(formatter, "action was not approved: {decision:?}")
            }
            Self::ActionEnforcement(error) => {
                write!(formatter, "Control Plane action receipt rejected: {error}")
            }
            Self::ReceiptUse(error) => write!(formatter, "action receipt claim failed: {error}"),
            Self::DecisionRecord(error) => {
                write!(formatter, "gate decision was not durably recorded: {error}")
            }
            Self::Executor(error) => write!(formatter, "Codex tool execution failed: {error}"),
        }
    }
}

impl<RecorderError, ExecutorError> std::error::Error
    for ActionGatewayError<RecorderError, ExecutorError>
where
    RecorderError: std::error::Error + 'static,
    ExecutorError: std::error::Error + 'static,
{
}

/// Single Worker action gateway for all Codex Core tool request families.
pub struct WorkerActionGateway<Policy, Gate, Recorder, Executor> {
    authority: ActiveWorkerAuthority,
    envelope: ExecutionEnvelope<Policy>,
    gate: Gate,
    recorder: Recorder,
    executor: Executor,
}

impl<Policy, Gate, Recorder, Executor> WorkerActionGateway<Policy, Gate, Recorder, Executor>
where
    Gate: DeterministicActionGate<Policy>,
    Recorder: PreActionDecisionRecorder<Policy>,
    Executor: CodexToolExecutor,
{
    /// Creates a gateway bound to the current lease, `WorkerSession`, and envelope.
    #[must_use]
    pub fn new(
        authority: ActiveWorkerAuthority,
        envelope: ExecutionEnvelope<Policy>,
        gate: Gate,
        recorder: Recorder,
        executor: Executor,
    ) -> Self {
        Self {
            authority,
            envelope,
            gate,
            recorder,
            executor,
        }
    }

    /// Atomically replaces the authority used for all subsequent actions.
    ///
    /// Replacing the envelope immediately makes requests carrying its prior
    /// token stale, even when their old lease has not expired yet.
    pub fn replace_authority(
        &mut self,
        authority: ActiveWorkerAuthority,
        envelope: ExecutionEnvelope<Policy>,
    ) {
        self.authority = authority;
        self.envelope = envelope;
    }

    /// Validates, gates, and executes one pending Codex Core tool request.
    ///
    /// The executor is unreachable until every authority check, action
    /// normalization, intent comparison, and gate approval succeeds.
    ///
    /// # Errors
    ///
    /// Returns a fail-closed rejection before execution for invalid, expired,
    /// stale, mismatched, or unapproved actions. Returns [`ActionGatewayError::Executor`]
    /// only after the gate authorized the action and Codex Core was invoked.
    pub fn execute(
        &mut self,
        now: &Instant,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
        verifier: &ActionEnforcementVerifier,
        receipt_store: &mut dyn ActionReceiptUseStore,
    ) -> ActionGatewayResult<Executor::Output, Recorder::Error, Executor::Error> {
        validate_authority(
            now,
            &self.authority,
            &self.envelope.token,
            &action.authority,
        )?;

        let local_normalization = normalize_action(&action.intent, &action.request)
            .map_err(ActionGatewayError::Normalization)?;
        if !local_normalization.comparison.matches {
            return Err(ActionGatewayError::IntentMismatch(
                local_normalization.comparison.mismatches,
            ));
        }
        let normalization = verifier
            .verify(action, receipt)
            .map_err(ActionGatewayError::ActionEnforcement)?;

        let decision = self.gate.decide(GateInput {
            envelope: &self.envelope,
            intent: &normalization.intent,
            observed: &normalization.observed,
        });
        self.recorder
            .record(
                GateInput {
                    envelope: &self.envelope,
                    intent: &normalization.intent,
                    observed: &normalization.observed,
                },
                &decision,
            )
            .map_err(ActionGatewayError::DecisionRecord)?;
        if !decision.authorizes_execution() {
            return Err(ActionGatewayError::NotApproved(decision));
        }

        match receipt_store
            .claim(receipt)
            .map_err(ActionGatewayError::ReceiptUse)?
        {
            ActionReceiptClaim::Fresh => {}
            ActionReceiptClaim::AlreadyConsumed => {
                return Err(rejected(
                    ActionGatewayRejectionCode::ActionReceiptConsumed,
                    "action receipt was already consumed",
                ));
            }
        }
        let output = self
            .executor
            .execute(&action.request)
            .map_err(ActionGatewayError::Executor)?;
        Ok(ExecutedAction {
            normalization,
            decision,
            output,
        })
    }
}

fn validate_authority<RecorderError, ExecutorError>(
    now: &Instant,
    active: &ActiveWorkerAuthority,
    current_envelope: &ExecutionEnvelopeToken,
    presented: &WorkerActionAuthority,
) -> Result<(), ActionGatewayError<RecorderError, ExecutorError>> {
    if active.worker_session_id != active.session_identity.worker_session_id
        || active.lease.attempt < 1
        || active.lease.attempt > 1_000
        || active.lease.issued_at.0 >= active.lease.expires_at.0
        || current_envelope.version == 0
        || !is_sha256_digest(&current_envelope.digest.0)
        || !is_canonical_instant(&active.lease.issued_at.0)
        || !is_canonical_instant(&active.lease.expires_at.0)
        || !is_canonical_instant(&now.0)
    {
        return Err(rejected(
            ActionGatewayRejectionCode::InvalidCurrentAuthority,
            "current Worker authority is internally invalid",
        ));
    }
    if presented.lease != active.lease {
        return Err(rejected(
            ActionGatewayRejectionCode::StaleLease,
            "presented lease, attempt, Worker instance, or fencing token is stale",
        ));
    }
    if now.0 >= active.lease.expires_at.0 {
        return Err(rejected(
            ActionGatewayRejectionCode::ExpiredLease,
            "active lease has expired",
        ));
    }
    if presented.worker_session_id != active.worker_session_id
        || presented.worker_session_id != presented.session_identity.worker_session_id
    {
        return Err(rejected(
            ActionGatewayRejectionCode::StaleWorkerSession,
            "presented WorkerSession is stale or inconsistent",
        ));
    }
    if presented.session_identity != active.session_identity {
        return Err(rejected(
            ActionGatewayRejectionCode::StaleSessionIdentity,
            "presented session binding is stale",
        ));
    }
    if presented.envelope != *current_envelope {
        return Err(rejected(
            ActionGatewayRejectionCode::StaleExecutionEnvelope,
            "presented Execution Envelope token is stale",
        ));
    }
    Ok(())
}

fn rejected<RecorderError, ExecutorError>(
    code: ActionGatewayRejectionCode,
    reason: &str,
) -> ActionGatewayError<RecorderError, ExecutorError> {
    ActionGatewayError::Rejected {
        code,
        reason: reason.to_owned(),
    }
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_canonical_instant(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes
            .iter()
            .enumerate()
            .filter(|(index, _)| !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23))
            .all(|(_, byte)| byte.is_ascii_digit())
        && number(bytes, 5, 7).is_some_and(|month| (1..=12).contains(&month))
        && number(bytes, 8, 10).is_some_and(|day| (1..=31).contains(&day))
        && number(bytes, 11, 13).is_some_and(|hour| hour <= 23)
        && number(bytes, 14, 16).is_some_and(|minute| minute <= 59)
        && number(bytes, 17, 19).is_some_and(|second| second <= 59)
}

fn number(bytes: &[u8], start: usize, end: usize) -> Option<u8> {
    bytes.get(start..end)?.iter().try_fold(0_u8, |value, byte| {
        value.checked_mul(10)?.checked_add(byte.checked_sub(b'0')?)
    })
}
