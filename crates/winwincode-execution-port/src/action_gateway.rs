// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_domain::{Instant, RequestId, WorkerSessionId};

use crate::action_enforcement::{
    ActionEnforcementError, ActionReceiptClaim, ActionReceiptUseError, ActionReceiptUseStore,
};
use crate::action_normalizer::{
    ActionIntent, ActionNormalization, ActionNormalizationError, IntentMismatch, ObservedAction,
    ToolRequest, normalize_action,
};
use crate::generated::{ActionEnforcementReceiptMessage, ExecutionLeaseStamp};
pub use crate::repository_rule_pack::{PostActionHook, PostActionOutcome};
use crate::repository_rule_pack::{
    RepositoryRuleEvent, RepositoryRuleFact, RepositoryRulePack, language_for_path,
};
use winwincode_domain::SessionIdentity;

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveWorkerAuthority {
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerActionAuthority {
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerActionRequest {
    pub invocation_request_id: RequestId,
    pub authority: WorkerActionAuthority,
    pub intent: ActionIntent,
    pub request: ToolRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    AllowWithWatch { reason: String },
    RequestPlanDelta { reason: String },
    PauseForHuman { reason: String },
    DenyAction { reason: String },
    ReplanRequired { reason: String },
}

impl GateDecision {
    const fn authorizes_execution(&self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithWatch { .. })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GateInput<'action, Policy> {
    pub policy: &'action Policy,
    pub intent: &'action ActionIntent,
    pub observed: &'action ObservedAction,
}

pub trait DeterministicActionGate<Policy> {
    fn decide(&mut self, input: GateInput<'_, Policy>) -> GateDecision;
}

pub trait PreActionDecisionRecorder<Policy> {
    type Error;

    fn record(
        &mut self,
        input: GateInput<'_, Policy>,
        decision: &GateDecision,
    ) -> Result<(), Self::Error>;

    fn record_post_action(
        &mut self,
        input: GateInput<'_, Policy>,
        outcome: PostActionOutcome,
        hooks: &[PostActionHook],
    ) -> Result<(), Self::Error>;
}

#[must_use]
pub fn post_action_hooks(
    observed: &ObservedAction,
    outcome: PostActionOutcome,
) -> Vec<PostActionHook> {
    post_action_hooks_with_rules(&RepositoryRulePack::project_defaults(), observed, outcome)
}

fn post_action_hooks_with_rules(
    rules: &RepositoryRulePack,
    observed: &ObservedAction,
    outcome: PostActionOutcome,
) -> Vec<PostActionHook> {
    let event = match observed.source {
        crate::action_normalizer::ActionSource::File
            if observed.operation != crate::action_normalizer::ActionOperation::Execute =>
        {
            RepositoryRuleEvent::FileChanged
        }
        crate::action_normalizer::ActionSource::Shell
        | crate::action_normalizer::ActionSource::Git => RepositoryRuleEvent::CommandFinished,
        crate::action_normalizer::ActionSource::Network
        | crate::action_normalizer::ActionSource::Mcp
        | crate::action_normalizer::ActionSource::File => return Vec::new(),
    };
    let targets = if event == RepositoryRuleEvent::FileChanged {
        observed
            .targets
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    } else {
        vec![""]
    };
    targets
        .into_iter()
        .flat_map(|path| {
            rules
                .dry_run(&RepositoryRuleFact {
                    event,
                    language: language_for_path(path),
                    path: (!path.is_empty()).then_some(path),
                    outcome: Some(outcome),
                })
                .map_or_else(|_| Vec::new(), |evaluation| evaluation.actions)
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub trait CodexToolExecutor {
    type Output;
    type Error;

    fn execute(&mut self, request: &ToolRequest) -> Result<Self::Output, Self::Error>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedAction<Output> {
    pub normalization: ActionNormalization,
    pub decision: GateDecision,
    pub output: Output,
}

pub type ActionGatewayResult<Output, RecorderError, ExecutorError> =
    Result<ExecutedAction<Output>, ActionGatewayError<RecorderError, ExecutorError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionGatewayRejectionCode {
    InvalidCurrentAuthority,
    StaleLease,
    ExpiredLease,
    StaleWorkerSession,
    StaleSessionIdentity,
    IntentMismatch,
    ActionNotApproved,
    InvalidActionReceipt,
    ActionReceiptConsumed,
    ActionReceiptConflict,
    ActionReceiptUnavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionGatewayError<RecorderError, ExecutorError> {
    Rejected {
        code: ActionGatewayRejectionCode,
        reason: String,
    },
    Normalization(ActionNormalizationError),
    IntentMismatch(Vec<IntentMismatch>),
    NotApproved(GateDecision),
    ActionEnforcement(ActionEnforcementError),
    ReceiptUse(ActionReceiptUseError),
    DecisionRecord(RecorderError),
    PostActionRecord(RecorderError),
    Executor(ExecutorError),
}

impl<RecorderError, ExecutorError> ActionGatewayError<RecorderError, ExecutorError> {
    #[must_use]
    pub const fn rejection_code(&self) -> Option<ActionGatewayRejectionCode> {
        match self {
            Self::Rejected { code, .. } => Some(*code),
            Self::Normalization(_)
            | Self::DecisionRecord(_)
            | Self::PostActionRecord(_)
            | Self::Executor(_) => None,
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
            Self::PostActionRecord(error) => {
                write!(
                    formatter,
                    "post-action hook was not durably recorded: {error}"
                )
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

pub struct WorkerActionGateway<Policy, Gate, Recorder, Executor> {
    authority: ActiveWorkerAuthority,
    policy: Policy,
    gate: Gate,
    recorder: Recorder,
    executor: Executor,
    rules: RepositoryRulePack,
}

impl<Policy, Gate, Recorder, Executor> WorkerActionGateway<Policy, Gate, Recorder, Executor>
where
    Gate: DeterministicActionGate<Policy>,
    Recorder: PreActionDecisionRecorder<Policy>,
    Executor: CodexToolExecutor,
{
    #[must_use]
    pub fn new(
        authority: ActiveWorkerAuthority,
        policy: Policy,
        gate: Gate,
        recorder: Recorder,
        executor: Executor,
    ) -> Self {
        Self {
            authority,
            policy,
            gate,
            recorder,
            executor,
            rules: RepositoryRulePack::project_defaults(),
        }
    }

    pub fn with_repository_rules(
        mut self,
        rules: RepositoryRulePack,
    ) -> Result<Self, crate::repository_rule_pack::RepositoryRulePackError> {
        self.rules = rules.with_project_defaults()?;
        Ok(self)
    }

    pub fn replace_authority(&mut self, authority: ActiveWorkerAuthority, policy: Policy) {
        self.authority = authority;
        self.policy = policy;
    }

    pub fn execute(
        &mut self,
        now: &Instant,
        action: &WorkerActionRequest,
        receipt: &ActionEnforcementReceiptMessage,
        receipt_store: &mut dyn ActionReceiptUseStore,
    ) -> ActionGatewayResult<Executor::Output, Recorder::Error, Executor::Error> {
        validate_authority(now, &self.authority, &action.authority)?;

        let local_normalization = normalize_action(&action.intent, &action.request)
            .map_err(ActionGatewayError::Normalization)?;
        if !local_normalization.comparison.matches {
            return Err(ActionGatewayError::IntentMismatch(
                local_normalization.comparison.mismatches,
            ));
        }
        let normalization = crate::action_enforcement::validate_action_receipt(action, receipt)
            .map_err(ActionGatewayError::ActionEnforcement)?;

        let decision = self.gate.decide(GateInput {
            policy: &self.policy,
            intent: &normalization.intent,
            observed: &normalization.observed,
        });
        self.recorder
            .record(
                GateInput {
                    policy: &self.policy,
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
        let input = GateInput {
            policy: &self.policy,
            intent: &normalization.intent,
            observed: &normalization.observed,
        };
        let output = match self.executor.execute(&action.request) {
            Ok(output) => {
                let hooks = post_action_hooks_with_rules(
                    &self.rules,
                    input.observed,
                    PostActionOutcome::Succeeded,
                );
                self.recorder
                    .record_post_action(input, PostActionOutcome::Succeeded, &hooks)
                    .map_err(ActionGatewayError::PostActionRecord)?;
                output
            }
            Err(error) => {
                let hooks = post_action_hooks_with_rules(
                    &self.rules,
                    input.observed,
                    PostActionOutcome::Failed,
                );
                self.recorder
                    .record_post_action(input, PostActionOutcome::Failed, &hooks)
                    .map_err(ActionGatewayError::PostActionRecord)?;
                return Err(ActionGatewayError::Executor(error));
            }
        };
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
    presented: &WorkerActionAuthority,
) -> Result<(), ActionGatewayError<RecorderError, ExecutorError>> {
    if active.worker_session_id != active.session_identity.worker_session_id
        || active.lease.attempt < 1
        || active.lease.attempt > 1_000
        || active.lease.issued_at.0 >= active.lease.expires_at.0
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
