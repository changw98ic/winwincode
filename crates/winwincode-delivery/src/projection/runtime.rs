// SPDX-License-Identifier: Apache-2.0

//! Exact-bound runtime projection over accepted, sealed Worker/Codex facts.
//!
//! Accepted bindings and events are capabilities with private fields and no
//! deserializer. Production callers therefore cannot promote raw Worker input
//! into trusted projection facts.
//!
//! ```compile_fail
//! use winwincode_delivery::projection::runtime::AcceptedRuntimeEvent;
//!
//! let _forged = AcceptedRuntimeEvent {
//!     sequence: 1,
//!     ..todo!()
//! };
//! ```

use std::{collections::BTreeMap, error::Error, fmt};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionEventId, ExecutionJobId, FencingToken,
    LeaseId, ProductSessionId, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};

use crate::domain::{Delivery, SessionBindingId};
use crate::projection::redaction::{
    RuntimeDiffSummaryProjection, contains_credential_material, is_safe_source_ref,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_RUNTIME_SESSIONS: usize = 256;
const MAX_PLAN_ITEMS: usize = 100;
const MAX_AGENTS: usize = 256;
const MAX_ACTIVITIES: usize = 100;
const MAX_USAGE_METRICS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProjectionErrorCode {
    InvalidBinding,
    AmbiguousBinding,
    UnboundEvent,
    StaleAuthority,
    InvalidFact,
    ConflictingEvent,
    OldEvent,
    MissingSequence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProjectionError {
    code: RuntimeProjectionErrorCode,
    message: String,
}

impl RuntimeProjectionError {
    pub const fn code(&self) -> RuntimeProjectionErrorCode {
        self.code
    }
}

impl fmt::Display for RuntimeProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeIdentity {
    delivery_id: DeliveryId,
    stage_run_id: StageRunId,
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    execution_job_id: ExecutionJobId,
    lease_id: LeaseId,
    attempt: u64,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRuntimeBinding {
    session_binding_id: SessionBindingId,
    delivery_task_id: Option<DeliveryTaskId>,
    identity: RuntimeIdentity,
    settled_last_sequence: Option<u64>,
    seal: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRuntimeEvent {
    identity: RuntimeIdentity,
    sequence: u64,
    event_id: ExecutionEventId,
    fact: AcceptedRuntimeFact,
    seal: Sha256Digest,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum AcceptedRuntimeFact {
    Checkpoint,
    Plan(RuntimePlanProjection),
    Agent(RuntimeAgentProjection),
    Activity(RuntimeActivityProjection),
    Usage(RuntimeUsageProjection),
    Recovery(RuntimeRecoveryProjection),
    LiveDiff(RuntimeDiffSummaryProjection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePlanItemProjection {
    pub step: String,
    pub status: RuntimePlanItemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePlanProjection {
    pub item_id: Option<String>,
    pub explanation: Option<String>,
    pub items: Vec<RuntimePlanItemProjection>,
    pub text: Option<String>,
    pub complete: bool,
    pub source_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAgentStatus {
    Unknown,
    Waiting,
    Running,
    Completed,
    Interrupted,
    Failed,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAgentProjection {
    pub thread_id: CodexThreadId,
    pub parent_thread_id: Option<CodexThreadId>,
    pub path: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub status: RuntimeAgentStatus,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAgentEdgeProjection {
    pub parent_thread_id: CodexThreadId,
    pub child_thread_id: CodexThreadId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActivityType {
    Command,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActivityStatus {
    Running,
    Completed,
    Failed,
    Declined,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeActivityOutcome {
    Observed,
    Succeeded,
    TaskFailed,
    TimedOut,
    PolicyDenied,
    InfrastructureFailed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeActivityProjection {
    pub call_id: String,
    pub activity_type: RuntimeActivityType,
    pub command: Option<String>,
    pub status: RuntimeActivityStatus,
    pub outcome: RuntimeActivityOutcome,
    pub exit_code: Option<i32>,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUsageProjection {
    pub totals: Vec<RuntimeUsageMetricProjection>,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUsageMetricProjection {
    pub name: String,
    pub value: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeRecoveryState {
    None,
    Required,
    InProgress,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRecoveryProjection {
    pub state: RuntimeRecoveryState,
    pub failure_count: u64,
    pub recovery_count: u64,
    pub last_failure_source_ref: Option<String>,
    pub latest_recovery_source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSessionProjection {
    pub session_binding_id: SessionBindingId,
    pub stage_run_id: StageRunId,
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub product_session_id: ProductSessionId,
    pub worker_session_id: WorkerSessionId,
    pub codex_thread_id: CodexThreadId,
    pub execution_job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
    pub as_of_sequence: u64,
    pub plan: Option<RuntimePlanProjection>,
    pub agents: Vec<RuntimeAgentProjection>,
    pub agent_edges: Vec<RuntimeAgentEdgeProjection>,
    pub activities: Vec<RuntimeActivityProjection>,
    pub usage: Option<RuntimeUsageProjection>,
    pub recovery: RuntimeRecoveryProjection,
    pub diff_summary: Option<RuntimeDiffSummaryProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeFoldSnapshot {
    pub delivery_id: DeliveryId,
    pub sessions: Vec<RuntimeSessionProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeApplyOutcome {
    Applied {
        session_binding_id: SessionBindingId,
        sequence: u64,
    },
    DuplicateAcknowledged {
        session_binding_id: SessionBindingId,
        sequence: u64,
    },
    ReplayRequired {
        session_binding_id: SessionBindingId,
        from_sequence: u64,
        observed_sequence: u64,
    },
}

#[derive(Debug, Clone, Default)]
struct SessionReceipts {
    by_sequence: BTreeMap<u64, Sha256Digest>,
    by_event_id: BTreeMap<String, (u64, Sha256Digest)>,
}

#[derive(Debug, Clone)]
pub struct RuntimeProjection {
    snapshot: RuntimeFoldSnapshot,
    bindings: Vec<AcceptedRuntimeBinding>,
    receipts: BTreeMap<String, SessionReceipts>,
}

impl RuntimeProjection {
    /// Opens a read-only projection over exact accepted runtime bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when a sealed binding is not current and exact for the
    /// supplied canonical Delivery.
    pub fn new(
        delivery: &Delivery,
        bindings: Vec<AcceptedRuntimeBinding>,
    ) -> Result<Self, RuntimeProjectionError> {
        if bindings.len() > MAX_RUNTIME_SESSIONS {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidBinding,
                "runtime projection exceeds the bounded 256-session wire contract",
            ));
        }
        for (index, binding) in bindings.iter().enumerate() {
            validate_binding(delivery, binding)?;
            if bindings[index + 1..].iter().any(|other| {
                other.session_binding_id == binding.session_binding_id
                    || same_session_scope(&other.identity, &binding.identity)
            }) {
                return Err(projection_error(
                    RuntimeProjectionErrorCode::AmbiguousBinding,
                    "accepted runtime bindings repeat one SessionBinding or execution identity",
                ));
            }
        }
        let mut sessions = bindings
            .iter()
            .map(|binding| RuntimeSessionProjection {
                session_binding_id: binding.session_binding_id.clone(),
                stage_run_id: binding.identity.stage_run_id.clone(),
                delivery_task_id: binding.delivery_task_id.clone(),
                product_session_id: binding.identity.product_session_id.clone(),
                worker_session_id: binding.identity.worker_session_id.clone(),
                codex_thread_id: binding.identity.codex_thread_id.clone(),
                execution_job_id: binding.identity.execution_job_id.clone(),
                lease_id: binding.identity.lease_id.clone(),
                attempt: binding.identity.attempt,
                fencing_token: binding.identity.fencing_token.clone(),
                as_of_sequence: 0,
                plan: None,
                agents: Vec::new(),
                agent_edges: Vec::new(),
                activities: Vec::new(),
                usage: None,
                recovery: RuntimeRecoveryProjection {
                    state: RuntimeRecoveryState::None,
                    failure_count: 0,
                    recovery_count: 0,
                    last_failure_source_ref: None,
                    latest_recovery_source_ref: None,
                },
                diff_summary: None,
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session_binding_id.0.cmp(&right.session_binding_id.0));
        Ok(Self {
            snapshot: RuntimeFoldSnapshot {
                delivery_id: delivery.id().clone(),
                sessions,
            },
            bindings,
            receipts: BTreeMap::new(),
        })
    }

    pub fn snapshot(&self) -> &RuntimeFoldSnapshot {
        &self.snapshot
    }

    /// Rebuilds a Delivery-stage projection from the accepted runtime-ledger
    /// positions retained by the Control Plane.
    ///
    /// The ledger deliberately stores the generated `ExecutionEventRecord`
    /// separately from this Delivery-domain crate. The bridge therefore folds
    /// each already-accepted position as a bounded checkpoint; it never
    /// interprets Worker payload bytes or promotes an unaccepted event into a
    /// projection fact. A future semantic fact adapter can replace the
    /// checkpoint mapper while retaining this exact binding and sequence
    /// boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the current Delivery binding, `StageRun`, scheduler
    /// authority, or accepted sequence positions are not exact.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted_checkpoints(
        delivery: &Delivery,
        session_binding_id: &SessionBindingId,
        lease_id: LeaseId,
        fencing_token: FencingToken,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        settled_last_sequence: Option<u64>,
        accepted_events: &[(u64, ExecutionEventId)],
    ) -> Result<Self, RuntimeProjectionError> {
        let binding = persisted_binding(
            delivery,
            session_binding_id,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance_id,
            settled_last_sequence,
        )?;
        let events = accepted_events
            .iter()
            .map(|(sequence, event_id)| {
                let mut event = AcceptedRuntimeEvent {
                    identity: binding.identity.clone(),
                    sequence: *sequence,
                    event_id: event_id.clone(),
                    fact: AcceptedRuntimeFact::Checkpoint,
                    seal: Sha256Digest(String::new()),
                };
                event.seal = seal_event(&event)?;
                Ok(event)
            })
            .collect::<Result<Vec<_>, RuntimeProjectionError>>()?;
        Self::replay(delivery, vec![binding], &events)
    }

    /// Rebuilds the projection from a complete ordered accepted-fact stream.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid binding or fact, or when the supplied
    /// stream contains a sequence gap.
    pub fn replay(
        delivery: &Delivery,
        bindings: Vec<AcceptedRuntimeBinding>,
        events: &[AcceptedRuntimeEvent],
    ) -> Result<Self, RuntimeProjectionError> {
        let mut projection = Self::new(delivery, bindings)?;
        for event in events {
            if matches!(
                projection.apply(event)?,
                RuntimeApplyOutcome::ReplayRequired { .. }
            ) {
                return Err(projection_error(
                    RuntimeProjectionErrorCode::MissingSequence,
                    "persisted runtime replay is missing a contiguous sequence",
                ));
            }
        }
        let complete = projection.bindings.iter().all(|binding| {
            binding.settled_last_sequence.is_none_or(|last| {
                projection.snapshot.sessions.iter().any(|session| {
                    session.session_binding_id == binding.session_binding_id
                        && session.as_of_sequence == last
                })
            })
        });
        if !complete {
            return Err(projection_error(
                RuntimeProjectionErrorCode::MissingSequence,
                "persisted runtime replay does not reach a settled binding terminal sequence",
            ));
        }
        Ok(projection)
    }

    /// Folds one already accepted and sealed runtime fact.
    ///
    /// # Errors
    ///
    /// Returns an error before mutation when the event is not exact for one
    /// accepted binding.
    pub fn apply(
        &mut self,
        event: &AcceptedRuntimeEvent,
    ) -> Result<RuntimeApplyOutcome, RuntimeProjectionError> {
        if event.seal != seal_event(event)? {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidFact,
                "runtime event seal is invalid",
            ));
        }
        if !portable_value(&event.event_id.0, 200) || event.sequence > MAX_SAFE_INTEGER {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidFact,
                "runtime event identity or sequence is outside the accepted contract",
            ));
        }
        let mut matching = self
            .bindings
            .iter()
            .filter(|binding| same_session_scope(&binding.identity, &event.identity));
        let binding = matching.next().ok_or_else(|| {
            projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event does not match an accepted binding",
            )
        })?;
        if matching.next().is_some()
            || binding
                .settled_last_sequence
                .is_some_and(|last| event.sequence > last)
        {
            return Err(projection_error(
                RuntimeProjectionErrorCode::UnboundEvent,
                "runtime event is ambiguous or follows its settled binding",
            ));
        }
        if binding.identity != event.identity {
            return Err(projection_error(
                RuntimeProjectionErrorCode::StaleAuthority,
                "runtime event carries an expired lease, attempt, fence, or Worker identity",
            ));
        }
        validate_fact(&event.fact)?;
        if event.sequence == 0 {
            return Err(projection_error(
                RuntimeProjectionErrorCode::OldEvent,
                "runtime event sequence must start at one",
            ));
        }
        let receipt_key = binding.session_binding_id.0.clone();
        let receipts = self.receipts.entry(receipt_key).or_default();
        if receipt_already_accepted(receipts, event)? {
            return Ok(RuntimeApplyOutcome::DuplicateAcknowledged {
                session_binding_id: binding.session_binding_id.clone(),
                sequence: event.sequence,
            });
        }
        let session = self
            .snapshot
            .sessions
            .iter_mut()
            .find(|session| session.session_binding_id == binding.session_binding_id)
            .ok_or_else(|| {
                projection_error(
                    RuntimeProjectionErrorCode::InvalidBinding,
                    "runtime projection lost its accepted SessionBinding",
                )
            })?;
        let next_sequence = session.as_of_sequence.saturating_add(1);
        if event.sequence < next_sequence {
            return Err(projection_error(
                RuntimeProjectionErrorCode::OldEvent,
                "runtime event is older than the current contiguous sequence",
            ));
        }
        if event.sequence > next_sequence {
            return Ok(RuntimeApplyOutcome::ReplayRequired {
                session_binding_id: binding.session_binding_id.clone(),
                from_sequence: next_sequence,
                observed_sequence: event.sequence,
            });
        }
        let mut next_session = session.clone();
        fold_fact(&mut next_session, &event.fact)?;
        next_session.as_of_sequence = event.sequence;
        *session = next_session;
        receipts
            .by_sequence
            .insert(event.sequence, event.seal.clone());
        receipts.by_event_id.insert(
            event.event_id.0.clone(),
            (event.sequence, event.seal.clone()),
        );
        Ok(RuntimeApplyOutcome::Applied {
            session_binding_id: binding.session_binding_id.clone(),
            sequence: event.sequence,
        })
    }
}

fn receipt_already_accepted(
    receipts: &SessionReceipts,
    event: &AcceptedRuntimeEvent,
) -> Result<bool, RuntimeProjectionError> {
    match receipts.by_sequence.get(&event.sequence) {
        Some(previous) if previous == &event.seal => return Ok(true),
        Some(_) => {
            return Err(projection_error(
                RuntimeProjectionErrorCode::ConflictingEvent,
                "runtime sequence was already accepted with different content",
            ));
        }
        None => {}
    }
    match receipts.by_event_id.get(&event.event_id.0) {
        Some((previous_sequence, previous_seal))
            if *previous_sequence != event.sequence || previous_seal != &event.seal =>
        {
            Err(projection_error(
                RuntimeProjectionErrorCode::ConflictingEvent,
                "runtime event identity was reused with different content",
            ))
        }
        _ => Ok(false),
    }
}

fn fold_fact(
    session: &mut RuntimeSessionProjection,
    fact: &AcceptedRuntimeFact,
) -> Result<(), RuntimeProjectionError> {
    validate_fact(fact)?;
    match fact {
        AcceptedRuntimeFact::Checkpoint => {}
        AcceptedRuntimeFact::Plan(plan) => {
            session.plan = Some(plan.clone());
        }
        AcceptedRuntimeFact::Agent(agent) => {
            fold_agent(session, agent)?;
        }
        AcceptedRuntimeFact::Activity(activity) => {
            fold_activity(session, activity)?;
        }
        AcceptedRuntimeFact::Usage(usage) => {
            let mut usage = usage.clone();
            usage
                .totals
                .sort_by(|left, right| left.name.cmp(&right.name));
            session.usage = Some(usage);
        }
        AcceptedRuntimeFact::Recovery(recovery) => {
            fold_recovery(session, recovery)?;
        }
        AcceptedRuntimeFact::LiveDiff(summary) => {
            session.diff_summary = Some(summary.clone());
        }
    }
    Ok(())
}

fn fold_activity(
    session: &mut RuntimeSessionProjection,
    activity: &RuntimeActivityProjection,
) -> Result<(), RuntimeProjectionError> {
    if let Some(existing) = session
        .activities
        .iter_mut()
        .find(|existing| existing.call_id == activity.call_id)
    {
        if existing.activity_type != activity.activity_type
            || existing.command != activity.command
            || (activity_status_is_terminal(existing.status)
                && !activity_status_is_terminal(activity.status))
        {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidFact,
                "runtime activity identity is immutable and terminal status cannot regress",
            ));
        }
        *existing = activity.clone();
    } else {
        if session.activities.len() >= MAX_ACTIVITIES {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidFact,
                "runtime activities exceed the bounded 100-item projection",
            ));
        }
        session.activities.push(activity.clone());
    }
    session
        .activities
        .sort_by(|left, right| left.call_id.cmp(&right.call_id));
    Ok(())
}

const fn activity_status_is_terminal(status: RuntimeActivityStatus) -> bool {
    matches!(
        status,
        RuntimeActivityStatus::Completed
            | RuntimeActivityStatus::Failed
            | RuntimeActivityStatus::Declined
            | RuntimeActivityStatus::Cancelled
    )
}

fn fold_recovery(
    session: &mut RuntimeSessionProjection,
    recovery: &RuntimeRecoveryProjection,
) -> Result<(), RuntimeProjectionError> {
    if recovery.failure_count < session.recovery.failure_count
        || recovery.recovery_count < session.recovery.recovery_count
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime recovery aggregate cannot discard accepted failure or recovery history",
        ));
    }
    session.recovery = recovery.clone();
    Ok(())
}

fn fold_agent(
    session: &mut RuntimeSessionProjection,
    agent: &RuntimeAgentProjection,
) -> Result<(), RuntimeProjectionError> {
    let is_root = agent.thread_id == session.codex_thread_id;
    let parent_is_known = agent.parent_thread_id.as_ref().is_some_and(|parent| {
        session
            .agents
            .iter()
            .any(|existing| &existing.thread_id == parent)
    });
    if (is_root && agent.parent_thread_id.is_some())
        || (!is_root && !parent_is_known)
        || session
            .agents
            .iter()
            .find(|existing| existing.thread_id == agent.thread_id)
            .is_some_and(|existing| {
                existing.parent_thread_id != agent.parent_thread_id
                    || existing.path != agent.path
                    || (agent_status_is_terminal(existing.status)
                        && !agent_status_is_terminal(agent.status))
            })
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime Agent graph must stay rooted at the bound CodexThread with immutable parents",
        ));
    }

    if let Some(existing) = session
        .agents
        .iter_mut()
        .find(|existing| existing.thread_id == agent.thread_id)
    {
        *existing = agent.clone();
    } else {
        if session.agents.len() >= MAX_AGENTS {
            return Err(projection_error(
                RuntimeProjectionErrorCode::InvalidFact,
                "runtime agent graph exceeds its bounded public projection",
            ));
        }
        session.agents.push(agent.clone());
    }
    session
        .agents
        .sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
    if !agent_graph_is_acyclic(&session.agents) {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime Agent graph contains a parent cycle",
        ));
    }
    session.agent_edges = session
        .agents
        .iter()
        .filter_map(|agent| {
            agent
                .parent_thread_id
                .as_ref()
                .map(|parent| RuntimeAgentEdgeProjection {
                    parent_thread_id: parent.clone(),
                    child_thread_id: agent.thread_id.clone(),
                })
        })
        .collect();
    session.agent_edges.sort_by(|left, right| {
        (&left.parent_thread_id.0, &left.child_thread_id.0)
            .cmp(&(&right.parent_thread_id.0, &right.child_thread_id.0))
    });
    Ok(())
}

const fn agent_status_is_terminal(status: RuntimeAgentStatus) -> bool {
    matches!(
        status,
        RuntimeAgentStatus::Completed
            | RuntimeAgentStatus::Interrupted
            | RuntimeAgentStatus::Failed
            | RuntimeAgentStatus::Closed
    )
}

fn agent_graph_is_acyclic(agents: &[RuntimeAgentProjection]) -> bool {
    agents.iter().all(|agent| {
        let mut current = Some(&agent.thread_id);
        let mut seen = Vec::new();
        while let Some(thread_id) = current {
            if seen.contains(&thread_id) {
                return false;
            }
            seen.push(thread_id);
            current = agents
                .iter()
                .find(|candidate| candidate.thread_id == *thread_id)
                .and_then(|candidate| candidate.parent_thread_id.as_ref());
        }
        true
    })
}

fn validate_fact(fact: &AcceptedRuntimeFact) -> Result<(), RuntimeProjectionError> {
    match fact {
        AcceptedRuntimeFact::Checkpoint => Ok(()),
        AcceptedRuntimeFact::Plan(plan) => validate_plan(plan),
        AcceptedRuntimeFact::Agent(agent) => validate_agent(agent),
        AcceptedRuntimeFact::Activity(activity) => validate_activity(activity),
        AcceptedRuntimeFact::Usage(usage) => validate_usage(usage),
        AcceptedRuntimeFact::Recovery(recovery) => validate_recovery(recovery),
        AcceptedRuntimeFact::LiveDiff(summary) => {
            if summary.details_visible() || !is_safe_source_ref(summary.source_ref()) {
                Err(projection_error(
                    RuntimeProjectionErrorCode::InvalidFact,
                    "live Diff fact is not a secret-safe count summary",
                ))
            } else {
                Ok(())
            }
        }
    }
}

fn validate_plan(plan: &RuntimePlanProjection) -> Result<(), RuntimeProjectionError> {
    if plan.items.len() > MAX_PLAN_ITEMS
        || plan
            .item_id
            .as_deref()
            .is_some_and(|item_id| !portable_value(item_id, 200))
        || plan
            .explanation
            .as_deref()
            .is_some_and(|explanation| !safe_public_text(explanation, 20_000))
        || plan
            .items
            .iter()
            .any(|item| !safe_public_text(&item.step, 20_000))
        || plan
            .text
            .as_deref()
            .is_some_and(|text| !safe_public_text(text, 20_000))
        || !safe_source_ref(&plan.source_ref)
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime plan is not a bounded accepted projection fact",
        ));
    }
    Ok(())
}

fn validate_agent(agent: &RuntimeAgentProjection) -> Result<(), RuntimeProjectionError> {
    if !portable_value(&agent.thread_id.0, 200)
        || agent
            .parent_thread_id
            .as_ref()
            .is_some_and(|parent| !portable_value(&parent.0, 200))
        || agent.parent_thread_id.as_ref() == Some(&agent.thread_id)
        || agent
            .path
            .as_deref()
            .is_some_and(|path| !safe_public_text(path, 4_096))
        || agent
            .role
            .as_deref()
            .is_some_and(|role| !portable_value(role, 200))
        || agent
            .nickname
            .as_ref()
            .is_some_and(|nickname| !safe_public_text(nickname, 200))
        || !safe_source_ref(&agent.source_ref)
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime agent is not one bounded acyclic graph fact",
        ));
    }
    Ok(())
}

fn validate_activity(activity: &RuntimeActivityProjection) -> Result<(), RuntimeProjectionError> {
    if !portable_value(&activity.call_id, 200)
        || activity
            .command
            .as_deref()
            .is_some_and(|command| !safe_public_text(command, 20_000))
        || !safe_source_ref(&activity.source_ref)
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime activity contains unbounded or sensitive public text",
        ));
    }
    Ok(())
}

fn validate_usage(usage: &RuntimeUsageProjection) -> Result<(), RuntimeProjectionError> {
    if usage.totals.len() > MAX_USAGE_METRICS
        || usage
            .totals
            .iter()
            .any(|metric| !portable_value(&metric.name, 100) || metric.value > MAX_SAFE_INTEGER)
        || usage.totals.iter().enumerate().any(|(index, metric)| {
            usage.totals[index + 1..]
                .iter()
                .any(|next| next.name == metric.name)
        })
        || !safe_source_ref(&usage.source_ref)
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime usage is not one bounded non-negative metric set",
        ));
    }
    Ok(())
}

fn validate_recovery(recovery: &RuntimeRecoveryProjection) -> Result<(), RuntimeProjectionError> {
    let recovery_source_matches =
        (recovery.recovery_count > 0) == recovery.latest_recovery_source_ref.is_some();
    let state_matches = match recovery.state {
        RuntimeRecoveryState::None => {
            recovery.failure_count == 0
                && recovery.recovery_count == 0
                && recovery.last_failure_source_ref.is_none()
                && recovery.latest_recovery_source_ref.is_none()
        }
        RuntimeRecoveryState::Required | RuntimeRecoveryState::InProgress => {
            recovery.failure_count > recovery.recovery_count
                && recovery.last_failure_source_ref.is_some()
                && recovery_source_matches
        }
        RuntimeRecoveryState::Recovered => {
            recovery.failure_count > 0
                && recovery.recovery_count == recovery.failure_count
                && recovery.last_failure_source_ref.is_some()
                && recovery.latest_recovery_source_ref.is_some()
        }
    };
    if !state_matches
        || recovery.failure_count > MAX_SAFE_INTEGER
        || recovery.recovery_count > MAX_SAFE_INTEGER
        || recovery
            .last_failure_source_ref
            .as_deref()
            .is_some_and(|source_ref| !is_safe_source_ref(source_ref))
        || recovery
            .latest_recovery_source_ref
            .as_deref()
            .is_some_and(|source_ref| !is_safe_source_ref(source_ref))
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidFact,
            "runtime recovery contains unbounded or sensitive public text",
        ));
    }
    Ok(())
}

fn safe_public_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= maximum
        && !value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        && !contains_credential_material(value)
}

fn portable_value(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        })
}

fn safe_source_ref(value: &str) -> bool {
    is_safe_source_ref(value)
}

fn persisted_binding(
    delivery: &Delivery,
    session_binding_id: &SessionBindingId,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    settled_last_sequence: Option<u64>,
) -> Result<AcceptedRuntimeBinding, RuntimeProjectionError> {
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| &binding.id == session_binding_id)
        .ok_or_else(|| {
            projection_error(
                RuntimeProjectionErrorCode::InvalidBinding,
                "persisted runtime SessionBinding is missing",
            )
        })?;
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == binding.stage_run_id)
        .ok_or_else(|| {
            projection_error(
                RuntimeProjectionErrorCode::InvalidBinding,
                "persisted runtime StageRun is missing",
            )
        })?;
    let accepted = binding.worker_session_id.clone().ok_or_else(|| {
        projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "persisted runtime WorkerSession is missing",
        )
    })?;
    let codex_thread_id = binding.codex_thread_id.clone().ok_or_else(|| {
        projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "persisted runtime CodexThread is missing",
        )
    })?;
    let mut accepted = AcceptedRuntimeBinding {
        session_binding_id: binding.id.clone(),
        delivery_task_id: binding.delivery_task_id.clone(),
        identity: RuntimeIdentity {
            delivery_id: delivery.id().clone(),
            stage_run_id: run.id.clone(),
            product_session_id: binding.product_session_id.clone(),
            worker_session_id: accepted,
            codex_thread_id,
            execution_job_id: binding.execution_job_id.clone(),
            lease_id,
            attempt: run.attempt,
            fencing_token,
            worker_id,
            worker_instance_id,
        },
        settled_last_sequence,
        seal: Sha256Digest(String::new()),
    };
    accepted.seal = seal_binding(&accepted)?;
    validate_binding(delivery, &accepted)?;
    Ok(accepted)
}

fn same_session_scope(left: &RuntimeIdentity, right: &RuntimeIdentity) -> bool {
    left.delivery_id == right.delivery_id
        && left.stage_run_id == right.stage_run_id
        && left.product_session_id == right.product_session_id
        && left.worker_session_id == right.worker_session_id
        && left.codex_thread_id == right.codex_thread_id
        && left.execution_job_id == right.execution_job_id
}

fn validate_binding(
    delivery: &Delivery,
    accepted: &AcceptedRuntimeBinding,
) -> Result<(), RuntimeProjectionError> {
    if accepted.seal != seal_binding(accepted)? {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "accepted runtime binding seal is invalid",
        ));
    }
    let canonical_fence = accepted
        .identity
        .fencing_token
        .0
        .parse::<u64>()
        .is_ok_and(|value| {
            value > 0
                && value <= MAX_SAFE_INTEGER
                && value.to_string() == accepted.identity.fencing_token.0
        });
    if !portable_value(&accepted.identity.lease_id.0, 200)
        || !portable_value(&accepted.identity.worker_id.0, 200)
        || !portable_value(&accepted.identity.worker_instance_id.0, 200)
        || !canonical_fence
        || accepted.identity.attempt == 0
        || accepted.identity.attempt > MAX_SAFE_INTEGER
        || accepted
            .settled_last_sequence
            .is_some_and(|sequence| sequence == 0 || sequence > MAX_SAFE_INTEGER)
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "accepted runtime scheduler authority is malformed",
        ));
    }
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.id == accepted.session_binding_id
                && binding.delivery_id == accepted.identity.delivery_id
                && binding.delivery_task_id == accepted.delivery_task_id
                && binding.stage_run_id == accepted.identity.stage_run_id
                && binding.product_session_id == accepted.identity.product_session_id
                && binding.execution_job_id == accepted.identity.execution_job_id
                && binding.worker_session_id.as_ref() == Some(&accepted.identity.worker_session_id)
                && binding.codex_thread_id.as_ref() == Some(&accepted.identity.codex_thread_id)
        });
    let binding = bindings.next();
    let mut runs = delivery.snapshot().stage_runs.iter().filter(|run| {
        run.id == accepted.identity.stage_run_id
            && run.delivery_id == accepted.identity.delivery_id
            && run.attempt == accepted.identity.attempt
    });
    let run = runs.next();
    let settled_matches = run.is_some_and(|run| {
        let settled = run.finished_at_millis.is_some();
        settled == accepted.settled_last_sequence.is_some()
    });
    if accepted.identity.delivery_id != *delivery.id()
        || binding.is_none()
        || bindings.next().is_some()
        || run.is_none()
        || runs.next().is_some()
        || !settled_matches
    {
        return Err(projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            "accepted runtime binding does not match one current SessionBinding and StageRun",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BindingSeal<'binding> {
    session_binding_id: &'binding SessionBindingId,
    delivery_task_id: Option<&'binding DeliveryTaskId>,
    identity: &'binding RuntimeIdentity,
    settled_last_sequence: Option<u64>,
}

fn seal_binding(binding: &AcceptedRuntimeBinding) -> Result<Sha256Digest, RuntimeProjectionError> {
    seal(&BindingSeal {
        session_binding_id: &binding.session_binding_id,
        delivery_task_id: binding.delivery_task_id.as_ref(),
        identity: &binding.identity,
        settled_last_sequence: binding.settled_last_sequence,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventSeal<'event> {
    identity: &'event RuntimeIdentity,
    sequence: u64,
    event_id: &'event ExecutionEventId,
    fact: &'event AcceptedRuntimeFact,
}

fn seal_event(event: &AcceptedRuntimeEvent) -> Result<Sha256Digest, RuntimeProjectionError> {
    seal(&EventSeal {
        identity: &event.identity,
        sequence: event.sequence,
        event_id: &event.event_id,
        fact: &event.fact,
    })
}

fn seal(value: &impl Serialize) -> Result<Sha256Digest, RuntimeProjectionError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        projection_error(
            RuntimeProjectionErrorCode::InvalidBinding,
            format!("runtime fact seal cannot be encoded: {error}"),
        )
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn projection_error(
    code: RuntimeProjectionErrorCode,
    message: impl Into<String>,
) -> RuntimeProjectionError {
    RuntimeProjectionError {
        code,
        message: message.into(),
    }
}

/// Narrow fixture-only constructor for accepted runtime capabilities. The
/// real persisted-fact adapter is a Phase 4 gate and production builds do not
/// enable this module.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{
        AcceptedRuntimeBinding, AcceptedRuntimeEvent, AcceptedRuntimeFact, Delivery,
        ExecutionEventId, FencingToken, LeaseId, RuntimeActivityProjection, RuntimeAgentProjection,
        RuntimeIdentity, RuntimePlanProjection, RuntimeProjectionError, RuntimeProjectionErrorCode,
        RuntimeRecoveryProjection, RuntimeUsageMetricProjection, RuntimeUsageProjection,
        SessionBindingId, Sha256Digest, WorkerId, WorkerInstanceId, projection_error, seal_binding,
        seal_event, validate_binding, validate_fact,
    };
    use crate::projection::redaction::live_diff_summary;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RuntimeAuthorityFixture {
        pub lease_id: LeaseId,
        pub fencing_token: FencingToken,
        pub worker_id: WorkerId,
        pub worker_instance_id: WorkerInstanceId,
    }

    impl Default for RuntimeAuthorityFixture {
        fn default() -> Self {
            Self {
                lease_id: LeaseId("lease-runtime-fixture".into()),
                fencing_token: FencingToken("1".into()),
                worker_id: WorkerId("worker-runtime-fixture".into()),
                worker_instance_id: WorkerInstanceId("worker-instance-runtime-fixture".into()),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum RuntimeFactFixture {
        Checkpoint,
        Plan(RuntimePlanProjection),
        Agent(RuntimeAgentProjection),
        Activity(RuntimeActivityProjection),
        Usage {
            totals: Vec<(String, i64)>,
            source_ref: String,
        },
        Recovery(RuntimeRecoveryProjection),
        LiveDiff {
            changed_file_count: u64,
            additions: u64,
            deletions: u64,
            source_ref: String,
        },
    }

    /// Seals one fixture binding after checking it against the canonical
    /// Delivery. A settled run requires a positive final event sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical binding is incomplete or the
    /// supplied scheduler authority is outside the accepted contract.
    pub fn accepted_binding(
        delivery: &Delivery,
        session_binding_id: &SessionBindingId,
        authority: RuntimeAuthorityFixture,
        settled_last_sequence: Option<u64>,
    ) -> Result<AcceptedRuntimeBinding, RuntimeProjectionError> {
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| &binding.id == session_binding_id)
            .ok_or_else(|| {
                projection_error(
                    RuntimeProjectionErrorCode::InvalidBinding,
                    "fixture SessionBinding is missing",
                )
            })?;
        let run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == binding.stage_run_id)
            .ok_or_else(|| {
                projection_error(
                    RuntimeProjectionErrorCode::InvalidBinding,
                    "fixture StageRun is missing",
                )
            })?;
        let mut accepted = AcceptedRuntimeBinding {
            session_binding_id: binding.id.clone(),
            delivery_task_id: binding.delivery_task_id.clone(),
            identity: RuntimeIdentity {
                delivery_id: delivery.id().clone(),
                stage_run_id: run.id.clone(),
                product_session_id: binding.product_session_id.clone(),
                worker_session_id: binding.worker_session_id.clone().ok_or_else(|| {
                    projection_error(
                        RuntimeProjectionErrorCode::InvalidBinding,
                        "fixture WorkerSession is missing",
                    )
                })?,
                codex_thread_id: binding.codex_thread_id.clone().ok_or_else(|| {
                    projection_error(
                        RuntimeProjectionErrorCode::InvalidBinding,
                        "fixture CodexThread is missing",
                    )
                })?,
                execution_job_id: binding.execution_job_id.clone(),
                lease_id: authority.lease_id,
                attempt: run.attempt,
                fencing_token: authority.fencing_token,
                worker_id: authority.worker_id,
                worker_instance_id: authority.worker_instance_id,
            },
            settled_last_sequence,
            seal: Sha256Digest(String::new()),
        };
        accepted.seal = seal_binding(&accepted)?;
        validate_binding(delivery, &accepted)?;
        Ok(accepted)
    }

    /// Seals one safe semantic event under an already accepted fixture
    /// binding. Raw logs, process output, tool payloads, and provider traffic
    /// have no representable variant.
    ///
    /// # Errors
    ///
    /// Returns an error when the fixture fact is invalid or cannot be sealed.
    pub fn accepted_event(
        binding: &AcceptedRuntimeBinding,
        sequence: u64,
        event_id: &str,
        fact: RuntimeFactFixture,
    ) -> Result<AcceptedRuntimeEvent, RuntimeProjectionError> {
        let fact = match fact {
            RuntimeFactFixture::Checkpoint => AcceptedRuntimeFact::Checkpoint,
            RuntimeFactFixture::Plan(plan) => AcceptedRuntimeFact::Plan(plan),
            RuntimeFactFixture::Agent(agent) => AcceptedRuntimeFact::Agent(agent),
            RuntimeFactFixture::Activity(activity) => AcceptedRuntimeFact::Activity(activity),
            RuntimeFactFixture::Usage { totals, source_ref } => {
                let totals = totals
                    .into_iter()
                    .map(|(name, value)| {
                        u64::try_from(value)
                            .map(|value| RuntimeUsageMetricProjection { name, value })
                            .map_err(|_| {
                                projection_error(
                                    RuntimeProjectionErrorCode::InvalidFact,
                                    "runtime usage fixture contains a negative metric",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                AcceptedRuntimeFact::Usage(RuntimeUsageProjection { totals, source_ref })
            }
            RuntimeFactFixture::Recovery(recovery) => AcceptedRuntimeFact::Recovery(recovery),
            RuntimeFactFixture::LiveDiff {
                changed_file_count,
                additions,
                deletions,
                source_ref,
            } => AcceptedRuntimeFact::LiveDiff(
                live_diff_summary(changed_file_count, additions, deletions, &source_ref).map_err(
                    |error| {
                        projection_error(RuntimeProjectionErrorCode::InvalidFact, error.to_string())
                    },
                )?,
            ),
        };
        validate_fact(&fact)?;
        let mut event = AcceptedRuntimeEvent {
            identity: binding.identity.clone(),
            sequence,
            event_id: ExecutionEventId(event_id.to_owned()),
            fact,
            seal: Sha256Digest(String::new()),
        };
        event.seal = seal_event(&event)?;
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Delivery, test_fixture};

    fn fixture() -> (Delivery, AcceptedRuntimeBinding, AcceptedRuntimeEvent) {
        let delivery = Delivery::try_from_snapshot(test_fixture()).expect("canonical Delivery");
        let session = &delivery.snapshot().session_bindings[0];
        let run = &delivery.snapshot().stage_runs[0];
        let identity = RuntimeIdentity {
            delivery_id: delivery.id().clone(),
            stage_run_id: run.id.clone(),
            product_session_id: session.product_session_id.clone(),
            worker_session_id: session
                .worker_session_id
                .clone()
                .expect("accepted WorkerSession"),
            codex_thread_id: session
                .codex_thread_id
                .clone()
                .expect("accepted CodexThread"),
            execution_job_id: session.execution_job_id.clone(),
            lease_id: LeaseId("lease-runtime-projection".into()),
            attempt: run.attempt,
            fencing_token: FencingToken("7".into()),
            worker_id: WorkerId("worker-runtime-projection".into()),
            worker_instance_id: WorkerInstanceId("worker-instance-runtime-projection".into()),
        };
        let mut binding = AcceptedRuntimeBinding {
            session_binding_id: session.id.clone(),
            delivery_task_id: session.delivery_task_id.clone(),
            identity: identity.clone(),
            settled_last_sequence: Some(1),
            seal: Sha256Digest(String::new()),
        };
        binding.seal = seal_binding(&binding).expect("binding seal");
        let mut event = AcceptedRuntimeEvent {
            identity,
            sequence: 1,
            event_id: ExecutionEventId("runtime-event-1".into()),
            fact: AcceptedRuntimeFact::Checkpoint,
            seal: Sha256Digest(String::new()),
        };
        event.seal = seal_event(&event).expect("event seal");
        (delivery, binding, event)
    }

    fn event_with_fact(
        template: &AcceptedRuntimeEvent,
        sequence: u64,
        fact: AcceptedRuntimeFact,
    ) -> AcceptedRuntimeEvent {
        let mut event = template.clone();
        event.sequence = sequence;
        event.event_id = ExecutionEventId(format!("runtime-event-{sequence}"));
        event.fact = fact;
        event.seal = seal_event(&event).expect("semantic event seal");
        event
    }

    #[test]
    fn runtime_event_requires_one_exact_session_binding() {
        let (delivery, binding, event) = fixture();
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding]).expect("exact runtime binding");
        assert_eq!(
            projection.apply(&event).expect("exact bound event"),
            RuntimeApplyOutcome::Applied {
                session_binding_id: delivery.snapshot().session_bindings[0].id.clone(),
                sequence: 1,
            }
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);
    }

    #[test]
    fn runtime_projection_rejects_unbound_or_ambiguous_event() {
        let (delivery, binding, event) = fixture();
        let duplicate = RuntimeProjection::new(&delivery, vec![binding.clone(), binding.clone()])
            .expect_err("duplicate accepted authority must be ambiguous");
        assert_eq!(
            duplicate.code(),
            RuntimeProjectionErrorCode::AmbiguousBinding
        );

        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding]).expect("exact binding");
        let mut foreign = event;
        foreign.identity.delivery_id = DeliveryId("dlv_01J00000000000000000000009".into());
        foreign.seal = seal_event(&foreign).expect("foreign sealed event");
        let error = projection
            .apply(&foreign)
            .expect_err("unbound event must fail before projection");
        assert_eq!(error.code(), RuntimeProjectionErrorCode::UnboundEvent);
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 0);
    }

    #[test]
    fn runtime_projection_rejects_stale_lease_attempt() {
        let (delivery, binding, event) = fixture();
        for mutate in [
            |identity: &mut RuntimeIdentity| identity.lease_id = LeaseId("lease-stale".into()),
            |identity: &mut RuntimeIdentity| identity.attempt += 1,
            |identity: &mut RuntimeIdentity| {
                identity.fencing_token = FencingToken("6".into());
            },
            |identity: &mut RuntimeIdentity| {
                identity.worker_instance_id = WorkerInstanceId("worker-instance-old".into());
            },
            |identity: &mut RuntimeIdentity| {
                identity.worker_id = WorkerId("worker-old".into());
            },
        ] {
            let mut stale = event.clone();
            mutate(&mut stale.identity);
            stale.seal = seal_event(&stale).expect("stale fact is still sealed");
            let mut projection =
                RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("exact binding");
            let error = projection
                .apply(&stale)
                .expect_err("stale scheduler authority must fail before projection");
            assert_eq!(error.code(), RuntimeProjectionErrorCode::StaleAuthority);
            assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 0);
        }
    }

    #[test]
    fn runtime_binding_requires_positive_terminal_sequence_and_exact_task() {
        let (delivery, binding, _) = fixture();

        let mut zero_terminal = binding.clone();
        zero_terminal.settled_last_sequence = Some(0);
        zero_terminal.seal = seal_binding(&zero_terminal).expect("zero terminal seal");
        let zero_error = RuntimeProjection::new(&delivery, vec![zero_terminal])
            .expect_err("a settled binding must name a positive terminal sequence");
        assert_eq!(
            zero_error.code(),
            RuntimeProjectionErrorCode::InvalidBinding
        );

        let mut foreign_task = binding;
        foreign_task.delivery_task_id = Some(DeliveryTaskId("delivery-task-foreign".into()));
        foreign_task.seal = seal_binding(&foreign_task).expect("foreign task seal");
        let task_error = RuntimeProjection::new(&delivery, vec![foreign_task])
            .expect_err("runtime authority cannot move a SessionBinding to another task");
        assert_eq!(
            task_error.code(),
            RuntimeProjectionErrorCode::InvalidBinding
        );
    }

    #[test]
    fn runtime_projection_requires_contiguous_session_sequence() {
        let (delivery, mut binding, first) = fixture();
        binding.settled_last_sequence = Some(3);
        binding.seal = seal_binding(&binding).expect("expanded binding seal");
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding]).expect("exact binding");

        projection.apply(&first).expect("first event");
        assert_eq!(
            projection.apply(&first).expect("exact duplicate ack"),
            RuntimeApplyOutcome::DuplicateAcknowledged {
                session_binding_id: delivery.snapshot().session_bindings[0].id.clone(),
                sequence: 1,
            }
        );

        let mut third = first.clone();
        third.sequence = 3;
        third.event_id = ExecutionEventId("runtime-event-3".into());
        third.seal = seal_event(&third).expect("third event seal");
        assert_eq!(
            projection.apply(&third).expect("gap asks for replay"),
            RuntimeApplyOutcome::ReplayRequired {
                session_binding_id: delivery.snapshot().session_bindings[0].id.clone(),
                from_sequence: 2,
                observed_sequence: 3,
            }
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);

        let mut second = first.clone();
        second.sequence = 2;
        second.event_id = ExecutionEventId("runtime-event-2".into());
        second.seal = seal_event(&second).expect("second event seal");
        projection.apply(&second).expect("replayed missing event");
        projection
            .apply(&third)
            .expect("gap closes at sequence three");
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 3);
        assert_eq!(
            projection
                .apply(&first)
                .expect("older byte-identical duplicate ack"),
            RuntimeApplyOutcome::DuplicateAcknowledged {
                session_binding_id: delivery.snapshot().session_bindings[0].id.clone(),
                sequence: 1,
            }
        );

        let mut changed_first = first.clone();
        changed_first.event_id = ExecutionEventId("changed-runtime-event-1".into());
        changed_first.seal = seal_event(&changed_first).expect("changed event seal");
        let error = projection
            .apply(&changed_first)
            .expect_err("changed content at an accepted sequence must fail");
        assert_eq!(error.code(), RuntimeProjectionErrorCode::ConflictingEvent);
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 3);
    }

    #[test]
    fn agent_graph_projection_uses_bound_runtime_events() {
        let (delivery, mut binding, mut event) = fixture();
        binding.settled_last_sequence = Some(3);
        binding.seal = seal_binding(&binding).expect("binding seal");
        event.fact = AcceptedRuntimeFact::Plan(RuntimePlanProjection {
            item_id: Some("plan-update-1".into()),
            explanation: Some("Execute the approved slices.".into()),
            items: vec![RuntimePlanItemProjection {
                step: "Build the exact-bound projection".into(),
                status: RuntimePlanItemStatus::InProgress,
            }],
            text: None,
            complete: false,
            source_ref: "runtime:plan-1".into(),
        });
        event.seal = seal_event(&event).expect("plan event seal");

        let mut root_agent = event.clone();
        root_agent.sequence = 2;
        root_agent.event_id = ExecutionEventId("runtime-event-2".into());
        root_agent.fact = AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
            thread_id: event.identity.codex_thread_id.clone(),
            parent_thread_id: None,
            path: Some("executor".into()),
            nickname: Some("executor".into()),
            role: Some("executor".into()),
            status: RuntimeAgentStatus::Running,
            source_ref: "runtime:agent-root".into(),
        });
        root_agent.seal = seal_event(&root_agent).expect("root agent seal");

        let child_thread = CodexThreadId("codex-thread-reviewer-child".into());
        let mut child_agent = event.clone();
        child_agent.sequence = 3;
        child_agent.event_id = ExecutionEventId("runtime-event-3".into());
        child_agent.fact = AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
            thread_id: child_thread.clone(),
            parent_thread_id: Some(event.identity.codex_thread_id.clone()),
            path: Some("executor/reviewer".into()),
            nickname: Some("reviewer".into()),
            role: Some("reviewer".into()),
            status: RuntimeAgentStatus::Completed,
            source_ref: "runtime:agent-child".into(),
        });
        child_agent.seal = seal_event(&child_agent).expect("child agent seal");

        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        for accepted in [&event, &root_agent, &child_agent] {
            projection.apply(accepted).expect("bound runtime fact");
        }
        let session = &projection.snapshot().sessions[0];
        assert_eq!(session.plan.as_ref().expect("plan").items.len(), 1);
        assert_eq!(session.agents.len(), 2);
        assert_eq!(
            session.agent_edges,
            vec![RuntimeAgentEdgeProjection {
                parent_thread_id: event.identity.codex_thread_id,
                child_thread_id: child_thread,
            }]
        );
    }

    #[test]
    fn agent_graph_rejects_a_foreign_root_or_reparented_thread() {
        let (delivery, binding, template) = fixture();
        let mut foreign_root = event_with_fact(
            &template,
            1,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                thread_id: CodexThreadId("codex-thread-foreign".into()),
                parent_thread_id: None,
                path: Some("foreign".into()),
                nickname: None,
                role: None,
                status: RuntimeAgentStatus::Running,
                source_ref: "runtime:agent-foreign".into(),
            }),
        );
        foreign_root.event_id = ExecutionEventId("runtime-event-foreign-root".into());
        foreign_root.seal = seal_event(&foreign_root).expect("foreign root seal");
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("binding");
        let root_error = projection
            .apply(&foreign_root)
            .expect_err("the graph root must be the bound CodexThread");
        assert_eq!(root_error.code(), RuntimeProjectionErrorCode::InvalidFact);
        assert!(projection.snapshot().sessions[0].agents.is_empty());

        let root = event_with_fact(
            &template,
            1,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                thread_id: template.identity.codex_thread_id.clone(),
                parent_thread_id: None,
                path: Some("executor".into()),
                nickname: None,
                role: Some("executor".into()),
                status: RuntimeAgentStatus::Running,
                source_ref: "runtime:agent-root".into(),
            }),
        );
        let mut child = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                thread_id: CodexThreadId("codex-thread-child".into()),
                parent_thread_id: Some(template.identity.codex_thread_id.clone()),
                path: Some("executor/child".into()),
                nickname: None,
                role: Some("reviewer".into()),
                status: RuntimeAgentStatus::Running,
                source_ref: "runtime:agent-child".into(),
            }),
        );
        let mut expanded_binding = binding;
        expanded_binding.settled_last_sequence = Some(3);
        expanded_binding.seal = seal_binding(&expanded_binding).expect("expanded binding seal");
        projection = RuntimeProjection::new(&delivery, vec![expanded_binding]).expect("binding");
        projection.apply(&root).expect("bound root");
        projection.apply(&child).expect("bound child");

        child.sequence = 3;
        child.event_id = ExecutionEventId("runtime-event-reparent".into());
        if let AcceptedRuntimeFact::Agent(agent) = &mut child.fact {
            agent.parent_thread_id = Some(CodexThreadId("codex-thread-other-parent".into()));
        }
        child.seal = seal_event(&child).expect("reparent seal");
        let reparent_error = projection
            .apply(&child)
            .expect_err("an accepted Agent identity cannot be reparented");
        assert_eq!(
            reparent_error.code(),
            RuntimeProjectionErrorCode::InvalidFact
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 2);
    }

    #[test]
    fn agent_graph_preserves_path_and_terminal_state() {
        let (delivery, mut binding, template) = fixture();
        binding.settled_last_sequence = Some(3);
        binding.seal = seal_binding(&binding).expect("binding seal");
        let root = event_with_fact(
            &template,
            1,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                thread_id: template.identity.codex_thread_id.clone(),
                parent_thread_id: None,
                path: Some("executor".into()),
                nickname: None,
                role: Some("executor".into()),
                status: RuntimeAgentStatus::Running,
                source_ref: "runtime:agent-running".into(),
            }),
        );
        let completed = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                status: RuntimeAgentStatus::Completed,
                source_ref: "runtime:agent-completed".into(),
                ..match &root.fact {
                    AcceptedRuntimeFact::Agent(agent) => agent.clone(),
                    _ => unreachable!(),
                }
            }),
        );
        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        projection.apply(&root).expect("root Agent");

        let changed_path = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                path: Some("other/path".into()),
                ..match &completed.fact {
                    AcceptedRuntimeFact::Agent(agent) => agent.clone(),
                    _ => unreachable!(),
                }
            }),
        );
        let path_error = projection
            .apply(&changed_path)
            .expect_err("one Agent thread cannot change graph path");
        assert_eq!(path_error.code(), RuntimeProjectionErrorCode::InvalidFact);
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);

        projection.apply(&completed).expect("completed Agent");
        let regression = event_with_fact(
            &template,
            3,
            AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                status: RuntimeAgentStatus::Waiting,
                source_ref: "runtime:agent-regressed".into(),
                ..match &completed.fact {
                    AcceptedRuntimeFact::Agent(agent) => agent.clone(),
                    _ => unreachable!(),
                }
            }),
        );
        let regression_error = projection
            .apply(&regression)
            .expect_err("terminal Agent cannot become waiting again");
        assert_eq!(
            regression_error.code(),
            RuntimeProjectionErrorCode::InvalidFact
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 2);
    }

    #[test]
    fn activity_projection_distinguishes_commands_and_tests() {
        let (delivery, mut binding, mut command) = fixture();
        binding.settled_last_sequence = Some(2);
        binding.seal = seal_binding(&binding).expect("binding seal");
        command.fact = AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
            call_id: "call-command".into(),
            activity_type: RuntimeActivityType::Command,
            command: Some("cargo fmt --check".into()),
            status: RuntimeActivityStatus::Completed,
            outcome: RuntimeActivityOutcome::Succeeded,
            exit_code: Some(0),
            source_ref: "runtime:call-command".into(),
        });
        command.seal = seal_event(&command).expect("command seal");
        let mut test = command.clone();
        test.sequence = 2;
        test.event_id = ExecutionEventId("runtime-event-2".into());
        test.fact = AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
            call_id: "call-test".into(),
            activity_type: RuntimeActivityType::Test,
            command: Some("cargo test -p winwincode-delivery".into()),
            status: RuntimeActivityStatus::Failed,
            outcome: RuntimeActivityOutcome::TaskFailed,
            exit_code: Some(101),
            source_ref: "runtime:call-test".into(),
        });
        test.seal = seal_event(&test).expect("test seal");

        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        projection.apply(&command).expect("command activity");
        projection.apply(&test).expect("test activity");
        let activities = &projection.snapshot().sessions[0].activities;
        assert_eq!(activities.len(), 2);
        assert_eq!(activities[0].activity_type, RuntimeActivityType::Command);
        assert_eq!(activities[1].activity_type, RuntimeActivityType::Test);
        assert_eq!(activities[1].exit_code, Some(101));
    }

    #[test]
    fn semantic_runtime_text_rejects_common_bare_credentials() {
        let (_, binding, _) = fixture();
        for (index, command) in [
            "deploy ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "request --header value sk-proj-0123456789abcdefghijklmnopqrstuvwxyz",
            "git fetch https://alice:hunter2@example.test/repository.git",
            "printf '%s' '-----BEGIN PRIVATE KEY-----'",
        ]
        .into_iter()
        .enumerate()
        {
            let error = test_support::accepted_event(
                &binding,
                1,
                &format!("runtime-secret-{index}"),
                test_support::RuntimeFactFixture::Activity(RuntimeActivityProjection {
                    call_id: format!("call-secret-{index}"),
                    activity_type: RuntimeActivityType::Command,
                    command: Some(command.into()),
                    status: RuntimeActivityStatus::Completed,
                    outcome: RuntimeActivityOutcome::Succeeded,
                    exit_code: Some(0),
                    source_ref: format!("runtime:call-secret-{index}"),
                }),
            )
            .expect_err("credential-shaped semantic text cannot enter a public runtime fold");
            assert_eq!(error.code(), RuntimeProjectionErrorCode::InvalidFact);
        }

        let source_error = test_support::accepted_event(
            &binding,
            1,
            "runtime-secret-source",
            test_support::RuntimeFactFixture::Activity(RuntimeActivityProjection {
                call_id: "call-secret-source".into(),
                activity_type: RuntimeActivityType::Command,
                command: Some("cargo check".into()),
                status: RuntimeActivityStatus::Completed,
                outcome: RuntimeActivityOutcome::Succeeded,
                exit_code: Some(0),
                source_ref: "runtime:ghp_0123456789abcdefghijklmnopqrstuvwxyz".into(),
            }),
        )
        .expect_err("a credential-shaped source reference cannot enter a public runtime fold");
        assert_eq!(source_error.code(), RuntimeProjectionErrorCode::InvalidFact);
    }

    #[test]
    fn activity_projection_preserves_call_identity_and_terminal_state() {
        let (delivery, mut binding, template) = fixture();
        binding.settled_last_sequence = Some(3);
        binding.seal = seal_binding(&binding).expect("binding seal");
        let running = event_with_fact(
            &template,
            1,
            AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                call_id: "call-stable".into(),
                activity_type: RuntimeActivityType::Command,
                command: Some("cargo test -p winwincode-delivery".into()),
                status: RuntimeActivityStatus::Running,
                outcome: RuntimeActivityOutcome::Observed,
                exit_code: None,
                source_ref: "runtime:call-stable-running".into(),
            }),
        );
        let completed = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                call_id: "call-stable".into(),
                activity_type: RuntimeActivityType::Command,
                command: Some("cargo test -p winwincode-delivery".into()),
                status: RuntimeActivityStatus::Completed,
                outcome: RuntimeActivityOutcome::Succeeded,
                exit_code: Some(0),
                source_ref: "runtime:call-stable-completed".into(),
            }),
        );
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("binding");
        projection.apply(&running).expect("running activity");

        let changed_identity = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                call_id: "call-stable".into(),
                activity_type: RuntimeActivityType::Test,
                command: Some("cargo test --workspace".into()),
                status: RuntimeActivityStatus::Completed,
                outcome: RuntimeActivityOutcome::Succeeded,
                exit_code: Some(0),
                source_ref: "runtime:call-mutated".into(),
            }),
        );
        let identity_error = projection
            .apply(&changed_identity)
            .expect_err("one call cannot change its type or command");
        assert_eq!(
            identity_error.code(),
            RuntimeProjectionErrorCode::InvalidFact
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);

        projection.apply(&completed).expect("terminal activity");
        let regression = event_with_fact(
            &template,
            3,
            AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                status: RuntimeActivityStatus::Running,
                outcome: RuntimeActivityOutcome::Observed,
                exit_code: None,
                source_ref: "runtime:call-regressed".into(),
                ..match &completed.fact {
                    AcceptedRuntimeFact::Activity(activity) => activity.clone(),
                    _ => unreachable!(),
                }
            }),
        );
        let regression_error = projection
            .apply(&regression)
            .expect_err("terminal activity cannot become running again");
        assert_eq!(
            regression_error.code(),
            RuntimeProjectionErrorCode::InvalidFact
        );
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 2);
    }

    #[test]
    fn usage_projection_totals_bound_session_metrics() {
        let (delivery, mut binding, mut first) = fixture();
        binding.settled_last_sequence = Some(2);
        binding.seal = seal_binding(&binding).expect("binding seal");
        first.fact = AcceptedRuntimeFact::Usage(RuntimeUsageProjection {
            totals: vec![
                RuntimeUsageMetricProjection {
                    name: "z_metric".into(),
                    value: 1,
                },
                RuntimeUsageMetricProjection {
                    name: "a_metric".into(),
                    value: 2,
                },
            ],
            source_ref: "runtime:usage-1".into(),
        });
        first.seal = seal_event(&first).expect("first usage seal");
        let mut latest = first.clone();
        latest.sequence = 2;
        latest.event_id = ExecutionEventId("runtime-event-2".into());
        latest.fact = AcceptedRuntimeFact::Usage(RuntimeUsageProjection {
            totals: vec![
                RuntimeUsageMetricProjection {
                    name: "output_tokens".into(),
                    value: 4,
                },
                RuntimeUsageMetricProjection {
                    name: "input_tokens".into(),
                    value: 10,
                },
            ],
            source_ref: "runtime:usage-2".into(),
        });
        latest.seal = seal_event(&latest).expect("latest usage seal");

        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        projection.apply(&first).expect("first usage");
        projection.apply(&latest).expect("latest usage");
        let usage = projection.snapshot().sessions[0]
            .usage
            .as_ref()
            .expect("latest bound usage");
        assert_eq!(
            usage.totals,
            vec![
                RuntimeUsageMetricProjection {
                    name: "input_tokens".into(),
                    value: 10,
                },
                RuntimeUsageMetricProjection {
                    name: "output_tokens".into(),
                    value: 4,
                },
            ]
        );
        assert_eq!(usage.source_ref, "runtime:usage-2");
    }

    #[test]
    fn live_fold_and_persisted_replay_are_equal() {
        let (delivery, mut binding, mut plan) = fixture();
        binding.settled_last_sequence = Some(3);
        binding.seal = seal_binding(&binding).expect("binding seal");
        plan.fact = AcceptedRuntimeFact::Plan(RuntimePlanProjection {
            item_id: None,
            explanation: Some("Replay persisted facts.".into()),
            items: vec![RuntimePlanItemProjection {
                step: "Replay the persisted facts".into(),
                status: RuntimePlanItemStatus::Completed,
            }],
            text: Some("Projection replay".into()),
            complete: true,
            source_ref: "runtime:plan-replay".into(),
        });
        plan.seal = seal_event(&plan).expect("plan seal");
        let mut activity = plan.clone();
        activity.sequence = 2;
        activity.event_id = ExecutionEventId("runtime-event-2".into());
        activity.fact = AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
            call_id: "call-replay".into(),
            activity_type: RuntimeActivityType::Test,
            command: Some("cargo test -p winwincode-delivery".into()),
            status: RuntimeActivityStatus::Completed,
            outcome: RuntimeActivityOutcome::Succeeded,
            exit_code: Some(0),
            source_ref: "runtime:call-replay".into(),
        });
        activity.seal = seal_event(&activity).expect("activity seal");
        let mut usage = plan.clone();
        usage.sequence = 3;
        usage.event_id = ExecutionEventId("runtime-event-3".into());
        usage.fact = AcceptedRuntimeFact::Usage(RuntimeUsageProjection {
            totals: vec![RuntimeUsageMetricProjection {
                name: "input_tokens".into(),
                value: 12,
            }],
            source_ref: "runtime:usage-replay".into(),
        });
        usage.seal = seal_event(&usage).expect("usage seal");
        let persisted = vec![plan, activity, usage];

        let mut live =
            RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("live projection");
        live.apply(&persisted[0]).expect("live batch one");
        for event in &persisted[1..] {
            live.apply(event).expect("live batch two");
        }
        let replayed = RuntimeProjection::replay(&delivery, vec![binding], &persisted)
            .expect("complete persisted replay");
        assert_eq!(live.snapshot(), replayed.snapshot());
        assert_eq!(
            serde_json::to_vec(live.snapshot()).expect("live json"),
            serde_json::to_vec(replayed.snapshot()).expect("replay json")
        );
        let incomplete =
            RuntimeProjection::replay(&delivery, vec![live.bindings[0].clone()], &persisted[..2])
                .expect_err("settled replay must reach its sealed terminal sequence");
        assert_eq!(
            incomplete.code(),
            RuntimeProjectionErrorCode::MissingSequence
        );
    }

    #[test]
    fn recovery_and_live_diff_are_exact_bound_and_summary_only() {
        let (delivery, mut binding, mut recovery) = fixture();
        binding.settled_last_sequence = Some(2);
        binding.seal = seal_binding(&binding).expect("binding seal");
        recovery.fact = AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
            state: RuntimeRecoveryState::Recovered,
            failure_count: 1,
            recovery_count: 1,
            last_failure_source_ref: Some("runtime:failure-1".into()),
            latest_recovery_source_ref: Some("runtime:recovery-1".into()),
        });
        recovery.seal = seal_event(&recovery).expect("recovery seal");
        let mut diff = recovery.clone();
        diff.sequence = 2;
        diff.event_id = ExecutionEventId("runtime-event-2".into());
        diff.fact = AcceptedRuntimeFact::LiveDiff(
            crate::projection::redaction::live_diff_summary(2, 9, 3, "runtime:diff-2")
                .expect("safe Diff counts"),
        );
        diff.seal = seal_event(&diff).expect("Diff seal");

        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        projection.apply(&recovery).expect("recovery fact");
        projection.apply(&diff).expect("Diff fact");
        let session = &projection.snapshot().sessions[0];
        assert_eq!(session.recovery.recovery_count, 1);
        assert_eq!(
            session
                .diff_summary
                .as_ref()
                .expect("live Diff")
                .additions(),
            9
        );
        assert!(
            !session
                .diff_summary
                .as_ref()
                .expect("live Diff")
                .details_visible()
        );
    }

    #[test]
    fn recovery_projection_enforces_consistent_aggregate() {
        let (delivery, mut binding, template) = fixture();
        binding.settled_last_sequence = Some(4);
        binding.seal = seal_binding(&binding).expect("binding seal");
        let required = event_with_fact(
            &template,
            1,
            AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::Required,
                failure_count: 1,
                recovery_count: 0,
                last_failure_source_ref: Some("runtime:failure-1".into()),
                latest_recovery_source_ref: None,
            }),
        );
        let in_progress = event_with_fact(
            &template,
            2,
            AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::InProgress,
                failure_count: 1,
                recovery_count: 0,
                last_failure_source_ref: Some("runtime:failure-1".into()),
                latest_recovery_source_ref: None,
            }),
        );
        let recovered = event_with_fact(
            &template,
            3,
            AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::Recovered,
                failure_count: 1,
                recovery_count: 1,
                last_failure_source_ref: Some("runtime:failure-1".into()),
                latest_recovery_source_ref: Some("runtime:recovery-1".into()),
            }),
        );
        let mut projection =
            RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("binding");
        projection.apply(&required).expect("recovery required");
        projection
            .apply(&in_progress)
            .expect("recovery in progress");
        projection.apply(&recovered).expect("recovered");
        let reset = event_with_fact(
            &template,
            4,
            AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::None,
                failure_count: 0,
                recovery_count: 0,
                last_failure_source_ref: None,
                latest_recovery_source_ref: None,
            }),
        );
        let reset_error = projection
            .apply(&reset)
            .expect_err("recovery aggregates cannot reset accepted history");
        assert_eq!(reset_error.code(), RuntimeProjectionErrorCode::InvalidFact);
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 3);

        for invalid in [
            RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::Required,
                failure_count: 1,
                recovery_count: 1,
                last_failure_source_ref: Some("runtime:failure-invalid".into()),
                latest_recovery_source_ref: None,
            },
            RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::Recovered,
                failure_count: 2,
                recovery_count: 1,
                last_failure_source_ref: Some("runtime:failure-invalid".into()),
                latest_recovery_source_ref: Some("runtime:recovery-invalid".into()),
            },
            RuntimeRecoveryProjection {
                state: RuntimeRecoveryState::InProgress,
                failure_count: 1,
                recovery_count: 2,
                last_failure_source_ref: Some("runtime:failure-invalid".into()),
                latest_recovery_source_ref: Some("runtime:recovery-invalid".into()),
            },
        ] {
            let invalid_event =
                event_with_fact(&template, 1, AcceptedRuntimeFact::Recovery(invalid));
            let mut fresh =
                RuntimeProjection::new(&delivery, vec![binding.clone()]).expect("binding");
            let error = fresh
                .apply(&invalid_event)
                .expect_err("inconsistent recovery aggregate must fail");
            assert_eq!(error.code(), RuntimeProjectionErrorCode::InvalidFact);
            assert_eq!(fresh.snapshot().sessions[0].as_of_sequence, 0);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn runtime_session_serialization_matches_canonical_wire() {
        let (delivery, mut binding, template) = fixture();
        binding.settled_last_sequence = Some(6);
        binding.seal = seal_binding(&binding).expect("binding seal");
        let root_thread = template.identity.codex_thread_id.clone();
        let events = vec![
            event_with_fact(
                &template,
                1,
                AcceptedRuntimeFact::Plan(RuntimePlanProjection {
                    item_id: None,
                    explanation: Some("Run the exact contract checks.".into()),
                    items: vec![RuntimePlanItemProjection {
                        step: "Project runtime facts".into(),
                        status: RuntimePlanItemStatus::InProgress,
                    }],
                    text: None,
                    complete: false,
                    source_ref: "runtime:plan-wire".into(),
                }),
            ),
            event_with_fact(
                &template,
                2,
                AcceptedRuntimeFact::Agent(RuntimeAgentProjection {
                    thread_id: root_thread.clone(),
                    parent_thread_id: None,
                    path: Some("executor".into()),
                    nickname: None,
                    role: Some("executor".into()),
                    status: RuntimeAgentStatus::Running,
                    source_ref: "runtime:agent-wire".into(),
                }),
            ),
            event_with_fact(
                &template,
                3,
                AcceptedRuntimeFact::Activity(RuntimeActivityProjection {
                    call_id: "call-wire".into(),
                    activity_type: RuntimeActivityType::Test,
                    command: None,
                    status: RuntimeActivityStatus::Running,
                    outcome: RuntimeActivityOutcome::Observed,
                    exit_code: None,
                    source_ref: "runtime:activity-wire".into(),
                }),
            ),
            event_with_fact(
                &template,
                4,
                AcceptedRuntimeFact::Usage(RuntimeUsageProjection {
                    totals: vec![
                        RuntimeUsageMetricProjection {
                            name: "output_tokens".into(),
                            value: 4,
                        },
                        RuntimeUsageMetricProjection {
                            name: "input_tokens".into(),
                            value: 10,
                        },
                    ],
                    source_ref: "runtime:usage-wire".into(),
                }),
            ),
            event_with_fact(
                &template,
                5,
                AcceptedRuntimeFact::Recovery(RuntimeRecoveryProjection {
                    state: RuntimeRecoveryState::InProgress,
                    failure_count: 1,
                    recovery_count: 0,
                    last_failure_source_ref: Some("runtime:failure-wire".into()),
                    latest_recovery_source_ref: None,
                }),
            ),
            event_with_fact(
                &template,
                6,
                AcceptedRuntimeFact::LiveDiff(
                    crate::projection::redaction::live_diff_summary(2, 9, 3, "runtime:diff-wire")
                        .expect("safe Diff summary"),
                ),
            ),
        ];
        let mut projection = RuntimeProjection::new(&delivery, vec![binding]).expect("binding");
        for event in &events {
            projection.apply(event).expect("wire event");
        }

        let actual =
            serde_json::to_value(&projection.snapshot().sessions[0]).expect("runtime session JSON");
        assert_eq!(
            actual,
            serde_json::json!({
                "sessionBindingId": "binding-verifier-1",
                "stageRunId": "stage-verification-1",
                "deliveryTaskId": "delivery-task-api",
                "productSessionId": "product-session-verifier",
                "workerSessionId": "worker-session-verifier",
                "codexThreadId": "codex-thread-verifier",
                "executionJobId": "execution-job-verifier",
                "leaseId": "lease-runtime-projection",
                "attempt": 1,
                "fencingToken": "7",
                "asOfSequence": 6,
                "plan": {
                    "itemId": null,
                    "explanation": "Run the exact contract checks.",
                    "items": [{"step": "Project runtime facts", "status": "in_progress"}],
                    "text": null,
                    "complete": false,
                    "sourceRef": "runtime:plan-wire"
                },
                "agents": [{
                    "threadId": "codex-thread-verifier",
                    "parentThreadId": null,
                    "path": "executor",
                    "nickname": null,
                    "role": "executor",
                    "status": "running",
                    "sourceRef": "runtime:agent-wire"
                }],
                "agentEdges": [],
                "activities": [{
                    "callId": "call-wire",
                    "activityType": "test",
                    "command": null,
                    "status": "running",
                    "outcome": "observed",
                    "exitCode": null,
                    "sourceRef": "runtime:activity-wire"
                }],
                "usage": {
                    "totals": [
                        {"name": "input_tokens", "value": 10},
                        {"name": "output_tokens", "value": 4}
                    ],
                    "sourceRef": "runtime:usage-wire"
                },
                "recovery": {
                    "state": "in-progress",
                    "failureCount": 1,
                    "recoveryCount": 0,
                    "lastFailureSourceRef": "runtime:failure-wire",
                    "latestRecoverySourceRef": null
                },
                "diffSummary": {
                    "changedFileCount": 2,
                    "additions": 9,
                    "deletions": 3,
                    "detailsVisible": false,
                    "sourceRef": "runtime:diff-wire"
                }
            })
        );
        assert!(actual.get("workerId").is_none());
        assert!(actual.get("workerInstanceId").is_none());
    }
}
