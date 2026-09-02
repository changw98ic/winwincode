// SPDX-License-Identifier: Apache-2.0

//! Durable Observer decisions and their product-visible lifecycle.
//!
//! The service consumes only structured Control Plane facts. It stores no
//! model transcript, hidden reasoning, tool payload, or Agent lifecycle. A
//! blocking decision becomes effective only at an explicit safe checkpoint;
//! recording or replaying that decision never closes a `ProductSession`, a
//! Worker slot, an admission reservation, or its worktree.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_delivery::application::failure_router::{FailureRoute, FailureRoutingDecision};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, EvidenceId, ExecutionEventId, ExecutionJobId, FencingToken,
    Instant, LeaseId, ModelExchangeId, ProductSessionId, Sha256Digest, StageRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_execution_port::runtime_trace_outbox::TraceGateOutcome;
use winwincode_session::{ExecutionRoute, ProductSessionState};
use winwincode_storage::{
    CommitReceipt, ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRecord,
    ExecutionReservationState, NewOutboxEvent, ProductStateStorage, ReceiptIdentity,
    ReceiptScopeKey, SqliteStorage, StateCommit, StorageError, StorageErrorKind, WorkerPoolId,
    WorkerSlotRecord, WorkerSlotState,
};

use crate::{
    ProductSessionPersistence, ProductSessionRecord, ProductSessionService,
    ProductSessionServiceError,
};

/// Current durable schema for the Observer decision catalog and receipts.
pub const OBSERVER_DECISION_SERVICE_SCHEMA_VERSION: u8 = 1;

const OBSERVER_DECISION_RECEIPT_TOPIC: &str = "observer-decision.receipt.internal.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_REASON_CODE_LENGTH: usize = 100;
const MAX_SUMMARY_LENGTH: usize = 2_000;
const MAX_EVIDENCE_REFERENCES: usize = 256;

/// Storage seam joining Observer state to existing session, Worker-slot, and
/// admission authorities.
pub trait ObserverDecisionPersistence: ProductStateStorage {
    /// Loads the canonical `ProductSession` projection in one exact scope.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the canonical session catalog is corrupt
    /// or unavailable.
    fn load_observer_product_session(
        &mut self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<Option<ProductSessionRecord>, StorageError>;

    /// Loads the canonical Worker slot and its exact-scope admission record.
    ///
    /// # Errors
    ///
    /// Returns a storage error when either existing authority cannot be read.
    fn load_observer_worker_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<Option<(WorkerSlotRecord, ExecutionReservationRecord)>, StorageError>;
}

impl ObserverDecisionPersistence for SqliteStorage {
    fn load_observer_product_session(
        &mut self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<Option<ProductSessionRecord>, StorageError> {
        ProductSessionService::new(self)
            .get(scope, product_session_id)
            .map_err(|error| product_session_storage_error(&error))
    }

    fn load_observer_worker_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<Option<(WorkerSlotRecord, ExecutionReservationRecord)>, StorageError> {
        ProductSessionPersistence::load_worker_binding_source(
            self,
            scope,
            worker_pool_id,
            worker_session_id,
        )
    }
}

/// Common idempotency, optimistic-concurrency, and event facts for one command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDecisionCommandContext {
    pub receipt_identity: ReceiptIdentity,
    /// Observer-session revision, independent from the `ProductSession` revision.
    pub expected_revision: u64,
    pub event_id: ControlPlaneEventId,
    pub occurred_at: Instant,
}

/// The four structured decisions understood by the Control Plane.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverDecisionKind {
    Watch,
    PlanDelta,
    Pause,
    Replan,
}

impl ObserverDecisionKind {
    /// Maps the Action Gateway outcome into the Observer vocabulary.
    #[must_use]
    pub const fn from_gate_decision(decision: &GateDecision) -> Option<Self> {
        match decision {
            GateDecision::AllowWithWatch { .. } => Some(Self::Watch),
            GateDecision::RequestPlanDelta { .. } => Some(Self::PlanDelta),
            GateDecision::PauseForHuman { .. } => Some(Self::Pause),
            GateDecision::ReplanRequired { .. } => Some(Self::Replan),
            GateDecision::Allow | GateDecision::DenyAction { .. } => None,
        }
    }

    /// Maps the secret-safe runtime trace outcome into the Observer vocabulary.
    #[must_use]
    pub const fn from_trace_gate_outcome(outcome: TraceGateOutcome) -> Option<Self> {
        match outcome {
            TraceGateOutcome::AllowWithWatch => Some(Self::Watch),
            TraceGateOutcome::RequestPlanDelta => Some(Self::PlanDelta),
            TraceGateOutcome::PauseForHuman => Some(Self::Pause),
            TraceGateOutcome::ReplanRequired => Some(Self::Replan),
            TraceGateOutcome::Allow | TraceGateOutcome::DenyAction => None,
        }
    }

    /// Maps failure-router actions which require an Observer lifecycle change.
    #[must_use]
    pub const fn from_failure_route(route: FailureRoute) -> Option<Self> {
        match route {
            FailureRoute::Repair => Some(Self::PlanDelta),
            FailureRoute::Replan => Some(Self::Replan),
            FailureRoute::Clarification
            | FailureRoute::AcceptanceReview
            | FailureRoute::ModelEscalation
            | FailureRoute::HumanReview
            | FailureRoute::Abort => Some(Self::Pause),
            FailureRoute::InfraRetry => None,
        }
    }
}

/// Trusted subsystem which produced one structured decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverDecisionOrigin {
    CodexObserver,
    ActionGateway,
    RuntimeTrace,
    FailureRouter,
}

/// Structured decision input. It deliberately has no transcript or reasoning field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDecisionInput {
    kind: ObserverDecisionKind,
    origin: ObserverDecisionOrigin,
    source_decision_code: String,
    reason_code: String,
    summary: String,
    evidence_ref_ids: Vec<EvidenceId>,
}

impl ObserverDecisionInput {
    /// Builds a bounded result emitted by the embedded Codex Observer seam.
    #[must_use]
    pub fn codex(
        kind: ObserverDecisionKind,
        reason_code: impl Into<String>,
        summary: impl Into<String>,
        evidence_ref_ids: Vec<EvidenceId>,
    ) -> Self {
        Self {
            kind,
            origin: ObserverDecisionOrigin::CodexObserver,
            source_decision_code: decision_code(kind).into(),
            reason_code: reason_code.into(),
            summary: summary.into(),
            evidence_ref_ids,
        }
    }

    /// Builds an input directly from the existing Action Gateway result.
    #[must_use]
    pub fn from_gate_decision(
        decision: &GateDecision,
        evidence_ref_ids: Vec<EvidenceId>,
    ) -> Option<Self> {
        let kind = ObserverDecisionKind::from_gate_decision(decision)?;
        let (source_decision_code, summary) = match decision {
            GateDecision::AllowWithWatch { reason } => ("allow_with_watch", reason),
            GateDecision::RequestPlanDelta { reason } => ("request_plan_delta", reason),
            GateDecision::PauseForHuman { reason } => ("pause_for_human", reason),
            GateDecision::ReplanRequired { reason } => ("replan_required", reason),
            GateDecision::Allow | GateDecision::DenyAction { .. } => return None,
        };
        Some(Self {
            kind,
            origin: ObserverDecisionOrigin::ActionGateway,
            source_decision_code: source_decision_code.into(),
            reason_code: format!("action_gateway.{source_decision_code}"),
            summary: summary.clone(),
            evidence_ref_ids,
        })
    }

    /// Builds an input from the secret-safe gate fact in the runtime trace.
    #[must_use]
    pub fn from_trace_gate_outcome(
        outcome: TraceGateOutcome,
        summary: impl Into<String>,
        evidence_ref_ids: Vec<EvidenceId>,
    ) -> Option<Self> {
        let kind = ObserverDecisionKind::from_trace_gate_outcome(outcome)?;
        let source_decision_code = trace_gate_code(outcome);
        Some(Self {
            kind,
            origin: ObserverDecisionOrigin::RuntimeTrace,
            source_decision_code: source_decision_code.into(),
            reason_code: format!("runtime_trace.{source_decision_code}"),
            summary: summary.into(),
            evidence_ref_ids,
        })
    }

    /// Builds an input from the existing Delivery failure router's immutable packet.
    #[must_use]
    pub fn from_failure_decision(decision: &FailureRoutingDecision) -> Option<Self> {
        let route = decision.next_action();
        let kind = ObserverDecisionKind::from_failure_route(route)?;
        let source_decision_code = failure_route_code(route);
        Some(Self {
            kind,
            origin: ObserverDecisionOrigin::FailureRouter,
            source_decision_code: source_decision_code.into(),
            reason_code: format!("failure_router.{source_decision_code}"),
            summary: decision.packet().counterexample().observed.clone(),
            evidence_ref_ids: decision.packet().evidence_ref_ids().to_vec(),
        })
    }

    #[must_use]
    pub const fn kind(&self) -> ObserverDecisionKind {
        self.kind
    }

    #[must_use]
    pub const fn origin(&self) -> ObserverDecisionOrigin {
        self.origin
    }

    #[must_use]
    pub fn source_decision_code(&self) -> &str {
        &self.source_decision_code
    }

    #[must_use]
    pub fn reason_code(&self) -> &str {
        &self.reason_code
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub fn evidence_ref_ids(&self) -> &[EvidenceId] {
        &self.evidence_ref_ids
    }
}

/// Content-addressed reference to one accepted runtime-trace fact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserverRuntimeTraceRef {
    pub event_id: ExecutionEventId,
    pub sequence: u64,
    pub digest: Sha256Digest,
}

/// Exact structured source joined from the interaction route and durable
/// ProductSession/Worker authorities.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserverExecutionSource {
    pub product_session_id: ProductSessionId,
    pub product_session_revision: u64,
    pub stage_run_id: Option<StageRunId>,
    pub execution_job_id: ExecutionJobId,
    pub job_revision: u64,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub codex_thread_id: CodexThreadId,
    pub lease_id: LeaseId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
    pub worker_slot_revision: u64,
    pub model_exchange_id: Option<ModelExchangeId>,
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub runtime_trace: ObserverRuntimeTraceRef,
}

impl ObserverExecutionSource {
    /// Copies an already sealed interaction route into the Observer source.
    ///
    /// # Errors
    ///
    /// Rejects queued routes without an exact runtime authority or slot revision.
    pub fn from_interaction_route(
        route: &ExecutionRoute,
        product_session_revision: u64,
        codex_thread_id: CodexThreadId,
        execution_scope: ExecutionQueueScope,
        worker_pool_id: WorkerPoolId,
        runtime_trace: ObserverRuntimeTraceRef,
    ) -> Result<Self, ObserverDecisionServiceError> {
        let runtime = route.runtime.as_ref().ok_or_else(|| {
            observer_error(
                ObserverDecisionServiceErrorCode::SourceMismatch,
                "Observer source requires an active interaction runtime route",
            )
        })?;
        let worker_slot_revision = route.worker_slot_revision.ok_or_else(|| {
            observer_error(
                ObserverDecisionServiceErrorCode::SourceMismatch,
                "Observer source requires an exact Worker-slot revision",
            )
        })?;
        Ok(Self {
            product_session_id: route.product_session_id.clone(),
            product_session_revision,
            stage_run_id: route.stage_run_id.clone(),
            execution_job_id: route.execution_job_id.clone(),
            job_revision: route.job_revision,
            worker_id: runtime.worker_id.clone(),
            worker_instance_id: runtime.worker_instance_id.clone(),
            worker_session_id: runtime.worker_session_id.clone(),
            codex_thread_id,
            lease_id: runtime.lease_id.clone(),
            attempt: runtime.attempt,
            fencing_token: runtime.fencing_token.clone(),
            worker_slot_revision,
            model_exchange_id: route.model_exchange_id.clone(),
            execution_scope,
            worker_pool_id,
            runtime_trace,
        })
    }
}

/// Safe point after which the Control Plane may expose a blocking state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverCheckpointKind {
    BeforeAction,
    RuntimeCheckpoint,
    TurnBoundary,
    WorkerParked,
}

/// Exact runtime fact proving one safe checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserverSafeCheckpoint {
    pub kind: ObserverCheckpointKind,
    pub runtime_trace: ObserverRuntimeTraceRef,
    pub observed_at: Instant,
}

/// Creates one decision, optionally already at a proven safe checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordObserverDecisionCommand {
    pub context: ObserverDecisionCommandContext,
    pub source: ObserverExecutionSource,
    pub decision: ObserverDecisionInput,
    pub safe_checkpoint: Option<ObserverSafeCheckpoint>,
}

/// Advances the current pending decision at one later safe checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyObserverCheckpointCommand {
    pub context: ObserverDecisionCommandContext,
    pub product_session_id: ProductSessionId,
    pub decision_event_id: ExecutionEventId,
    pub safe_checkpoint: ObserverSafeCheckpoint,
}

/// Product-visible state of the current structured Observer decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverDecisionState {
    Watching,
    PlanDeltaPending,
    PlanDeltaReady,
    PausePending,
    Paused,
    ReplanPending,
    ReplanReady,
}

impl ObserverDecisionState {
    /// Whether another action must wait for this decision to be resolved.
    #[must_use]
    pub const fn blocks_new_actions(self) -> bool {
        !matches!(self, Self::Watching)
    }

    const fn is_pending(self) -> bool {
        matches!(
            self,
            Self::PlanDeltaPending | Self::PausePending | Self::ReplanPending
        )
    }
}

/// Exact resources retained while a decision is pending or paused.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObserverRetainedResources {
    pub product_session_id: ProductSessionId,
    pub product_session_revision: u64,
    pub execution_job_id: ExecutionJobId,
    pub worker_session_id: WorkerSessionId,
    pub worker_slot_revision: u64,
    pub codex_thread_id: CodexThreadId,
    pub worktree_key: Option<String>,
}

/// Read-only, explainable projection for one decision lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDecisionProjection {
    decision_event_id: ExecutionEventId,
    session_revision: u64,
    kind: ObserverDecisionKind,
    state: ObserverDecisionState,
    origin: ObserverDecisionOrigin,
    source_decision_code: String,
    reason_code: String,
    summary: String,
    evidence_ref_ids: Vec<EvidenceId>,
    source: ObserverExecutionSource,
    safe_checkpoint: Option<ObserverSafeCheckpoint>,
    retained_resources: ObserverRetainedResources,
    decided_at: Instant,
    updated_at: Instant,
}

impl ObserverDecisionProjection {
    #[must_use]
    pub const fn decision_event_id(&self) -> &ExecutionEventId {
        &self.decision_event_id
    }

    #[must_use]
    pub const fn session_revision(&self) -> u64 {
        self.session_revision
    }

    #[must_use]
    pub const fn kind(&self) -> ObserverDecisionKind {
        self.kind
    }

    #[must_use]
    pub const fn state(&self) -> ObserverDecisionState {
        self.state
    }

    #[must_use]
    pub const fn origin(&self) -> ObserverDecisionOrigin {
        self.origin
    }

    #[must_use]
    pub fn source_decision_code(&self) -> &str {
        &self.source_decision_code
    }

    #[must_use]
    pub fn reason_code(&self) -> &str {
        &self.reason_code
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub fn evidence_ref_ids(&self) -> &[EvidenceId] {
        &self.evidence_ref_ids
    }

    #[must_use]
    pub const fn source(&self) -> &ObserverExecutionSource {
        &self.source
    }

    #[must_use]
    pub const fn safe_checkpoint(&self) -> Option<&ObserverSafeCheckpoint> {
        self.safe_checkpoint.as_ref()
    }

    #[must_use]
    pub const fn retained_resources(&self) -> &ObserverRetainedResources {
        &self.retained_resources
    }

    #[must_use]
    pub const fn decided_at(&self) -> &Instant {
        &self.decided_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> &Instant {
        &self.updated_at
    }

    /// True for every projection because this service emits no resource mutation.
    #[must_use]
    pub const fn preserves_session_worker_and_worktree(&self) -> bool {
        true
    }
}

/// Replay-safe result of one decision or checkpoint command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDecisionMutationReceipt {
    pub projection: ObserverDecisionProjection,
    pub catalog_revision: u64,
    pub replayed: bool,
}

/// Stable machine-readable service error category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverDecisionServiceErrorCode {
    InvalidInput,
    NotFound,
    AlreadyExists,
    RevisionConflict,
    RequestConflict,
    InvalidState,
    SourceMismatch,
    UnsafeCheckpoint,
    CorruptState,
    Storage,
}

/// Bounded Observer application-service failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDecisionServiceError {
    code: ObserverDecisionServiceErrorCode,
    message: String,
}

impl ObserverDecisionServiceError {
    #[must_use]
    pub const fn code(&self) -> ObserverDecisionServiceErrorCode {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ObserverDecisionServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ObserverDecisionServiceError {}

/// Durable command and read service for structured Observer decisions.
pub struct ObserverDecisionService<'storage> {
    storage: &'storage mut dyn ObserverDecisionPersistence,
}

#[allow(clippy::missing_errors_doc)]
impl<'storage> ObserverDecisionService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ObserverDecisionPersistence) -> Self {
        Self { storage }
    }

    /// Stores one structured decision after joining its existing authorities.
    pub fn record_decision(
        &mut self,
        command: &RecordObserverDecisionCommand,
    ) -> Result<ObserverDecisionMutationReceipt, ObserverDecisionServiceError> {
        let digest = command_digest("record", record_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Recorded)? {
            return Ok(replay);
        }
        validate_context(&command.context)?;
        validate_source_shape(&command.source)?;
        validate_decision(&command.decision)?;
        if let Some(checkpoint) = &command.safe_checkpoint {
            validate_checkpoint(checkpoint, &command.source.runtime_trace)?;
        }
        let worktree_key = self.validate_durable_source(
            command.context.receipt_identity.scope_key(),
            &command.source,
        )?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let session = catalog
            .sessions
            .entry(command.source.product_session_id.0.clone())
            .or_default();
        require_revision(session.revision, command.context.expected_revision)?;
        if session
            .decisions
            .contains_key(&command.source.runtime_trace.event_id.0)
        {
            return Err(observer_error(
                ObserverDecisionServiceErrorCode::AlreadyExists,
                "Observer runtime event already has a decision",
            ));
        }
        if session
            .current_decision()
            .is_some_and(|current| current.state.is_pending())
        {
            return Err(observer_error(
                ObserverDecisionServiceErrorCode::InvalidState,
                "Observer decision remains pending until a safe checkpoint",
            ));
        }
        session.revision = next_revision(session.revision)?;
        let state = decision_state(command.decision.kind, command.safe_checkpoint.is_some());
        let retained_resources = ObserverRetainedResources {
            product_session_id: command.source.product_session_id.clone(),
            product_session_revision: command.source.product_session_revision,
            execution_job_id: command.source.execution_job_id.clone(),
            worker_session_id: command.source.worker_session_id.clone(),
            worker_slot_revision: command.source.worker_slot_revision,
            codex_thread_id: command.source.codex_thread_id.clone(),
            worktree_key,
        };
        let persisted = PersistedObserverDecision {
            decision_event_id: command.source.runtime_trace.event_id.clone(),
            session_revision: session.revision,
            kind: command.decision.kind,
            state,
            origin: command.decision.origin,
            source_decision_code: command.decision.source_decision_code.clone(),
            reason_code: command.decision.reason_code.clone(),
            summary: command.decision.summary.clone(),
            evidence_ref_ids: normalized_evidence(&command.decision.evidence_ref_ids)?,
            source: command.source.clone(),
            safe_checkpoint: command.safe_checkpoint.clone(),
            retained_resources,
            decided_at: command.context.occurred_at.clone(),
            updated_at: command.context.occurred_at.clone(),
        };
        session.current_decision_event_id = Some(persisted.decision_event_id.clone());
        session
            .decisions
            .insert(persisted.decision_event_id.0.clone(), persisted.clone());
        self.commit(
            &command.context,
            digest,
            MutationKind::Recorded,
            catalog,
            &persisted,
        )
    }

    /// Advances one pending decision only after an explicit safe checkpoint.
    pub fn apply_checkpoint(
        &mut self,
        command: &ApplyObserverCheckpointCommand,
    ) -> Result<ObserverDecisionMutationReceipt, ObserverDecisionServiceError> {
        let digest = command_digest("checkpoint", checkpoint_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Checkpointed)? {
            return Ok(replay);
        }
        validate_context(&command.context)?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let (source, prior_trace) = {
            let session = catalog
                .sessions
                .get(&command.product_session_id.0)
                .ok_or_else(observer_not_found)?;
            require_revision(session.revision, command.context.expected_revision)?;
            let current = session.current_decision().ok_or_else(observer_not_found)?;
            if current.decision_event_id != command.decision_event_id {
                return Err(observer_error(
                    ObserverDecisionServiceErrorCode::SourceMismatch,
                    "safe checkpoint does not target the current Observer decision",
                ));
            }
            if !current.state.is_pending() {
                return Err(observer_error(
                    ObserverDecisionServiceErrorCode::InvalidState,
                    "current Observer decision is not waiting for a safe checkpoint",
                ));
            }
            (current.source.clone(), current.source.runtime_trace.clone())
        };
        validate_checkpoint(&command.safe_checkpoint, &prior_trace)?;
        self.validate_durable_source(command.context.receipt_identity.scope_key(), &source)?;
        let session = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(observer_not_found)?;
        session.revision = next_revision(session.revision)?;
        let session_revision = session.revision;
        let current = session
            .current_decision_mut()
            .ok_or_else(observer_not_found)?;
        current.session_revision = session_revision;
        current.state = checkpointed_state(current.kind);
        current.safe_checkpoint = Some(command.safe_checkpoint.clone());
        current.updated_at = command.context.occurred_at.clone();
        let persisted = current.clone();
        self.commit(
            &command.context,
            digest,
            MutationKind::Checkpointed,
            catalog,
            &persisted,
        )
    }

    /// Reads the current decision for one exact `ProductSession` and scope.
    pub fn get_current(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<Option<ObserverDecisionProjection>, ObserverDecisionServiceError> {
        self.load_catalog(scope)?
            .sessions
            .get(&product_session_id.0)
            .and_then(PersistedObserverSession::current_decision)
            .map(PersistedObserverDecision::to_projection)
            .transpose()
    }

    /// Reads deterministic decision history ordered by observer-session revision.
    pub fn history(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<Vec<ObserverDecisionProjection>, ObserverDecisionServiceError> {
        let mut history = self
            .load_catalog(scope)?
            .sessions
            .get(&product_session_id.0)
            .map(|session| {
                session
                    .decisions
                    .values()
                    .map(PersistedObserverDecision::to_projection)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        history.sort_unstable_by_key(ObserverDecisionProjection::session_revision);
        Ok(history)
    }

    fn validate_durable_source(
        &mut self,
        scope: &ReceiptScopeKey,
        source: &ObserverExecutionSource,
    ) -> Result<Option<String>, ObserverDecisionServiceError> {
        let record = self
            .storage
            .load_observer_product_session(scope, &source.product_session_id)
            .map_err(|error| observer_storage_error(&error))?
            .ok_or_else(observer_not_found)?;
        validate_product_session_binding(&record, source)?;
        let (slot, reservation) = self
            .storage
            .load_observer_worker_source(
                &source.execution_scope,
                &source.worker_pool_id,
                &source.worker_session_id,
            )
            .map_err(|error| observer_storage_error(&error))?
            .ok_or_else(|| {
                observer_error(
                    ObserverDecisionServiceErrorCode::SourceMismatch,
                    "Observer source has no exact durable Worker slot and admission",
                )
            })?;
        validate_worker_source(&slot, &reservation, source)?;
        Ok(match reservation.repository_access {
            ExecutionRepositoryAccess::IsolatedWrite { worktree_key } => Some(worktree_key),
            ExecutionRepositoryAccess::ReadOnly | ExecutionRepositoryAccess::SharedWrite => None,
        })
    }

    fn replay(
        &self,
        context: &ObserverDecisionCommandContext,
        digest: &Sha256Digest,
        kind: MutationKind,
    ) -> Result<Option<ObserverDecisionMutationReceipt>, ObserverDecisionServiceError> {
        self.storage
            .load_receipt(&context.receipt_identity, digest)
            .map_err(|error| observer_storage_error(&error))?
            .map(|receipt| decode_receipt(&receipt, kind, true))
            .transpose()
    }

    fn load_catalog(
        &self,
        scope: &ReceiptScopeKey,
    ) -> Result<PersistedObserverCatalog, ObserverDecisionServiceError> {
        let stream_id = observer_catalog_stream_id(scope);
        let Some(state) = self
            .storage
            .load_state(&stream_id)
            .map_err(|error| observer_storage_error(&error))?
        else {
            return Ok(PersistedObserverCatalog::default());
        };
        let catalog: PersistedObserverCatalog =
            serde_json::from_slice(&state.payload).map_err(|error| {
                observer_corrupt(format!("Observer catalog cannot be decoded: {error}"))
            })?;
        catalog.validate(state.revision)?;
        Ok(catalog)
    }

    fn commit(
        &mut self,
        context: &ObserverDecisionCommandContext,
        digest: Sha256Digest,
        kind: MutationKind,
        mut catalog: PersistedObserverCatalog,
        persisted: &PersistedObserverDecision,
    ) -> Result<ObserverDecisionMutationReceipt, ObserverDecisionServiceError> {
        let expected_catalog_revision = catalog.revision;
        catalog.revision = next_revision(catalog.revision)?;
        let event = PersistedObserverMutationEvent {
            schema_version: OBSERVER_DECISION_SERVICE_SCHEMA_VERSION,
            kind,
            catalog_revision: catalog.revision,
            decision: persisted.clone(),
        };
        let event_bytes = serde_json::to_vec(&event).map_err(|error| {
            observer_corrupt(format!("Observer event cannot be encoded: {error}"))
        })?;
        let state = serde_json::to_vec(&catalog).map_err(|error| {
            observer_corrupt(format!("Observer catalog cannot be encoded: {error}"))
        })?;
        let receipt_event = NewOutboxEvent::internal(
            format!("internal:observer-decision:{}", context.event_id.0),
            OBSERVER_DECISION_RECEIPT_TOPIC,
            event_bytes,
        );
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            observer_catalog_stream_id(context.receipt_identity.scope_key()),
            expected_catalog_revision,
            state,
            vec![receipt_event],
        );
        let receipt = self
            .storage
            .commit(&commit)
            .map_err(|error| observer_storage_error(&error))?;
        let decoded = decode_receipt(&receipt, kind, receipt.idempotent_replay)?;
        if !receipt.idempotent_replay && decoded.projection != persisted.to_projection()? {
            return Err(observer_corrupt(
                "committed Observer event differs from the accepted projection",
            ));
        }
        Ok(decoded)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationKind {
    Recorded,
    Checkpointed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedObserverMutationEvent {
    schema_version: u8,
    kind: MutationKind,
    catalog_revision: u64,
    decision: PersistedObserverDecision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedObserverCatalog {
    schema_version: u8,
    revision: u64,
    sessions: BTreeMap<String, PersistedObserverSession>,
}

impl Default for PersistedObserverCatalog {
    fn default() -> Self {
        Self {
            schema_version: OBSERVER_DECISION_SERVICE_SCHEMA_VERSION,
            revision: 0,
            sessions: BTreeMap::new(),
        }
    }
}

impl PersistedObserverCatalog {
    fn validate(&self, stored_revision: u64) -> Result<(), ObserverDecisionServiceError> {
        if self.schema_version != OBSERVER_DECISION_SERVICE_SCHEMA_VERSION
            || self.revision == 0
            || self.revision != stored_revision
        {
            return Err(observer_corrupt(
                "Observer catalog contract or revision is inconsistent",
            ));
        }
        for (product_session_id, session) in &self.sessions {
            session.validate(product_session_id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedObserverSession {
    revision: u64,
    current_decision_event_id: Option<ExecutionEventId>,
    decisions: BTreeMap<String, PersistedObserverDecision>,
}

impl PersistedObserverSession {
    fn current_decision(&self) -> Option<&PersistedObserverDecision> {
        self.current_decision_event_id
            .as_ref()
            .and_then(|event_id| self.decisions.get(&event_id.0))
    }

    fn current_decision_mut(&mut self) -> Option<&mut PersistedObserverDecision> {
        self.current_decision_event_id
            .as_ref()
            .and_then(|event_id| self.decisions.get_mut(&event_id.0))
    }

    fn validate(&self, product_session_id: &str) -> Result<(), ObserverDecisionServiceError> {
        if self.revision == 0 || self.revision > MAX_SAFE_INTEGER || self.decisions.is_empty() {
            return Err(observer_corrupt("Observer session revision is invalid"));
        }
        let current = self.current_decision().ok_or_else(|| {
            observer_corrupt("Observer session current decision reference is missing")
        })?;
        if current.session_revision != self.revision {
            return Err(observer_corrupt(
                "Observer current decision revision differs from its session revision",
            ));
        }
        let mut revisions = HashSet::with_capacity(self.decisions.len());
        for (event_id, decision) in &self.decisions {
            if event_id != &decision.decision_event_id.0
                || product_session_id != decision.source.product_session_id.0
                || !revisions.insert(decision.session_revision)
            {
                return Err(observer_corrupt(
                    "Observer decision identity or revision is inconsistent",
                ));
            }
            decision.to_projection()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedObserverDecision {
    decision_event_id: ExecutionEventId,
    session_revision: u64,
    kind: ObserverDecisionKind,
    state: ObserverDecisionState,
    origin: ObserverDecisionOrigin,
    source_decision_code: String,
    reason_code: String,
    summary: String,
    evidence_ref_ids: Vec<EvidenceId>,
    source: ObserverExecutionSource,
    safe_checkpoint: Option<ObserverSafeCheckpoint>,
    retained_resources: ObserverRetainedResources,
    decided_at: Instant,
    updated_at: Instant,
}

impl PersistedObserverDecision {
    fn to_projection(&self) -> Result<ObserverDecisionProjection, ObserverDecisionServiceError> {
        validate_source_shape(&self.source)?;
        validate_bounded_code(&self.source_decision_code, "sourceDecisionCode")?;
        validate_bounded_code(&self.reason_code, "reasonCode")?;
        validate_safe_summary(&self.summary)?;
        normalized_evidence(&self.evidence_ref_ids)?;
        if self.decision_event_id != self.source.runtime_trace.event_id
            || self.session_revision == 0
            || self.session_revision > MAX_SAFE_INTEGER
            || self.retained_resources.product_session_id != self.source.product_session_id
            || self.retained_resources.product_session_revision
                != self.source.product_session_revision
            || self.retained_resources.execution_job_id != self.source.execution_job_id
            || self.retained_resources.worker_session_id != self.source.worker_session_id
            || self.retained_resources.worker_slot_revision != self.source.worker_slot_revision
            || self.retained_resources.codex_thread_id != self.source.codex_thread_id
        {
            return Err(observer_corrupt(
                "Observer projection identities or retained resources are inconsistent",
            ));
        }
        match (&self.safe_checkpoint, self.state) {
            (None, state) if state.is_pending() || state == ObserverDecisionState::Watching => {}
            (Some(checkpoint), state) if !state.is_pending() => {
                validate_checkpoint(checkpoint, &self.source.runtime_trace)?;
            }
            _ => {
                return Err(observer_corrupt(
                    "Observer checkpoint and lifecycle state are inconsistent",
                ));
            }
        }
        Ok(ObserverDecisionProjection {
            decision_event_id: self.decision_event_id.clone(),
            session_revision: self.session_revision,
            kind: self.kind,
            state: self.state,
            origin: self.origin,
            source_decision_code: self.source_decision_code.clone(),
            reason_code: self.reason_code.clone(),
            summary: self.summary.clone(),
            evidence_ref_ids: self.evidence_ref_ids.clone(),
            source: self.source.clone(),
            safe_checkpoint: self.safe_checkpoint.clone(),
            retained_resources: self.retained_resources.clone(),
            decided_at: self.decided_at.clone(),
            updated_at: self.updated_at.clone(),
        })
    }
}

fn validate_context(
    context: &ObserverDecisionCommandContext,
) -> Result<(), ObserverDecisionServiceError> {
    if context.expected_revision > MAX_SAFE_INTEGER {
        return Err(observer_invalid(
            "expectedRevision is outside the safe range",
        ));
    }
    validate_id(&context.event_id.0, "eventId")?;
    validate_instant(&context.occurred_at, "occurredAt")
}

fn validate_decision(decision: &ObserverDecisionInput) -> Result<(), ObserverDecisionServiceError> {
    validate_bounded_code(&decision.source_decision_code, "sourceDecisionCode")?;
    validate_bounded_code(&decision.reason_code, "reasonCode")?;
    validate_safe_summary(&decision.summary)?;
    normalized_evidence(&decision.evidence_ref_ids).map(|_| ())
}

fn validate_source_shape(
    source: &ObserverExecutionSource,
) -> Result<(), ObserverDecisionServiceError> {
    validate_id(&source.product_session_id.0, "productSessionId")?;
    validate_id(&source.execution_job_id.0, "executionJobId")?;
    validate_id(&source.worker_id.0, "workerId")?;
    validate_id(&source.worker_instance_id.0, "workerInstanceId")?;
    validate_id(&source.worker_session_id.0, "workerSessionId")?;
    validate_id(&source.codex_thread_id.0, "codexThreadId")?;
    validate_id(&source.lease_id.0, "leaseId")?;
    if let Some(stage_run_id) = &source.stage_run_id {
        validate_id(&stage_run_id.0, "stageRunId")?;
    }
    if source.product_session_revision == 0
        || source.product_session_revision > MAX_SAFE_INTEGER
        || source.job_revision == 0
        || source.job_revision > MAX_SAFE_INTEGER
        || source.worker_slot_revision == 0
        || source.worker_slot_revision > MAX_SAFE_INTEGER
        || source.attempt == 0
        || source.attempt > 1_000
        || source.execution_scope.product_session_id != source.product_session_id
        || source.stage_run_id.is_some() != source.execution_scope.delivery_id.is_some()
    {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::SourceMismatch,
            "Observer execution source scope or revisions are inconsistent",
        ));
    }
    validate_fencing_token(&source.fencing_token)?;
    validate_runtime_trace(&source.runtime_trace)
}

fn validate_product_session_binding(
    record: &ProductSessionRecord,
    source: &ObserverExecutionSource,
) -> Result<(), ObserverDecisionServiceError> {
    if record.session().id() != &source.product_session_id
        || record.session().project_id() != &source.execution_scope.project_id
        || record.session().repository_id() != &source.execution_scope.repository_id
        || record.session().revision() != source.product_session_revision
        || record.session().state() != ProductSessionState::Running
    {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::SourceMismatch,
            "Observer source does not match the current running ProductSession",
        ));
    }
    let matches = record
        .bindings()
        .iter()
        .filter(|durable| {
            let binding = durable.binding();
            let slot = durable.slot();
            binding.execution_job_id() == &source.execution_job_id
                && binding.product_session_id() == &source.product_session_id
                && binding.stage_run_id() == source.stage_run_id.as_ref()
                && binding.worker_session_id() == Some(&source.worker_session_id)
                && binding.codex_thread_id() == Some(&source.codex_thread_id)
                && slot.authority.worker_id == source.worker_id
                && slot.authority.worker_instance_id == source.worker_instance_id
                && slot.authority.lease_id == source.lease_id
                && slot.authority.attempt == source.attempt
                && slot.authority.fencing_token == source.fencing_token
                && slot.revision == source.worker_slot_revision
        })
        .count();
    if matches != 1 {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::SourceMismatch,
            "Observer source does not match exactly one ProductSession binding",
        ));
    }
    Ok(())
}

fn validate_worker_source(
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
    source: &ObserverExecutionSource,
) -> Result<(), ObserverDecisionServiceError> {
    if slot.state != WorkerSlotState::Running
        || reservation.state != ExecutionReservationState::Running
        || slot.authority.worker_id != source.worker_id
        || slot.authority.worker_instance_id != source.worker_instance_id
        || slot.authority.worker_session_id != source.worker_session_id
        || slot.authority.codex_thread_id != source.codex_thread_id
        || slot.authority.job_id != source.execution_job_id
        || slot.authority.lease_id != source.lease_id
        || slot.authority.attempt != source.attempt
        || slot.authority.fencing_token != source.fencing_token
        || slot.revision != source.worker_slot_revision
        || reservation.scope != source.execution_scope
        || reservation.worker_pool_id != source.worker_pool_id
        || reservation.job_id != source.execution_job_id
    {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::SourceMismatch,
            "Observer source differs from the durable Worker slot or admission",
        ));
    }
    Ok(())
}

fn validate_checkpoint(
    checkpoint: &ObserverSafeCheckpoint,
    decision_trace: &ObserverRuntimeTraceRef,
) -> Result<(), ObserverDecisionServiceError> {
    validate_runtime_trace(&checkpoint.runtime_trace)?;
    validate_instant(&checkpoint.observed_at, "checkpoint.observedAt")?;
    if checkpoint.runtime_trace.sequence < decision_trace.sequence {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::UnsafeCheckpoint,
            "safe checkpoint precedes the Observer decision trace",
        ));
    }
    if checkpoint.runtime_trace.event_id == decision_trace.event_id
        && checkpoint.runtime_trace.digest != decision_trace.digest
    {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::UnsafeCheckpoint,
            "safe checkpoint reuses the decision event with another digest",
        ));
    }
    Ok(())
}

fn validate_runtime_trace(
    runtime_trace: &ObserverRuntimeTraceRef,
) -> Result<(), ObserverDecisionServiceError> {
    validate_id(&runtime_trace.event_id.0, "runtimeTrace.eventId")?;
    if runtime_trace.sequence == 0 || runtime_trace.sequence > MAX_SAFE_INTEGER {
        return Err(observer_invalid(
            "runtimeTrace.sequence is outside the safe range",
        ));
    }
    validate_digest(&runtime_trace.digest, "runtimeTrace.digest")
}

fn normalized_evidence(
    evidence_ref_ids: &[EvidenceId],
) -> Result<Vec<EvidenceId>, ObserverDecisionServiceError> {
    if evidence_ref_ids.len() > MAX_EVIDENCE_REFERENCES {
        return Err(observer_invalid("too many Observer Evidence references"));
    }
    let mut normalized = evidence_ref_ids.to_vec();
    normalized.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if normalized.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(observer_invalid(
            "Observer Evidence references contain duplicates",
        ));
    }
    for evidence_id in &normalized {
        validate_id(&evidence_id.0, "evidenceRefId")?;
    }
    Ok(normalized)
}

fn validate_bounded_code(value: &str, field: &str) -> Result<(), ObserverDecisionServiceError> {
    if value.is_empty()
        || value.len() > MAX_REASON_CODE_LENGTH
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(observer_invalid(format!("{field} is not a stable code")));
    }
    Ok(())
}

fn validate_safe_summary(value: &str) -> Result<(), ObserverDecisionServiceError> {
    let normalized = value.to_ascii_lowercase();
    if value.trim().is_empty()
        || value.chars().count() > MAX_SUMMARY_LENGTH
        || value.chars().any(char::is_control)
        || [
            "authorization:",
            "bearer ",
            "password=",
            "password:",
            "secret=",
            "secret:",
            "token=",
            "token:",
            "api_key",
            "apikey",
            "private key",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return Err(observer_invalid(
            "Observer summary is empty, oversized, or not secret-safe",
        ));
    }
    Ok(())
}

fn validate_id(value: &str, field: &str) -> Result<(), ObserverDecisionServiceError> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.'))
    {
        return Err(observer_invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), ObserverDecisionServiceError> {
    if value.0.is_empty()
        || value.0.len() > 64
        || value.0.chars().any(char::is_control)
        || !value.0.ends_with('Z')
    {
        return Err(observer_invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_digest(digest: &Sha256Digest, field: &str) -> Result<(), ObserverDecisionServiceError> {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return Err(observer_invalid(format!("{field} is invalid")));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(observer_invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_fencing_token(token: &FencingToken) -> Result<(), ObserverDecisionServiceError> {
    let value = token
        .0
        .parse::<u64>()
        .map_err(|_| observer_invalid("fencingToken is invalid"))?;
    if value == 0 || value > MAX_SAFE_INTEGER || value.to_string() != token.0 {
        return Err(observer_invalid("fencingToken is invalid"));
    }
    Ok(())
}

const fn decision_state(kind: ObserverDecisionKind, checkpointed: bool) -> ObserverDecisionState {
    match (kind, checkpointed) {
        (ObserverDecisionKind::Watch, _) => ObserverDecisionState::Watching,
        (ObserverDecisionKind::PlanDelta, false) => ObserverDecisionState::PlanDeltaPending,
        (ObserverDecisionKind::PlanDelta, true) => ObserverDecisionState::PlanDeltaReady,
        (ObserverDecisionKind::Pause, false) => ObserverDecisionState::PausePending,
        (ObserverDecisionKind::Pause, true) => ObserverDecisionState::Paused,
        (ObserverDecisionKind::Replan, false) => ObserverDecisionState::ReplanPending,
        (ObserverDecisionKind::Replan, true) => ObserverDecisionState::ReplanReady,
    }
}

fn checkpointed_state(kind: ObserverDecisionKind) -> ObserverDecisionState {
    match kind {
        ObserverDecisionKind::Watch => ObserverDecisionState::Watching,
        ObserverDecisionKind::PlanDelta => ObserverDecisionState::PlanDeltaReady,
        ObserverDecisionKind::Pause => ObserverDecisionState::Paused,
        ObserverDecisionKind::Replan => ObserverDecisionState::ReplanReady,
    }
}

const fn decision_code(kind: ObserverDecisionKind) -> &'static str {
    match kind {
        ObserverDecisionKind::Watch => "watch",
        ObserverDecisionKind::PlanDelta => "plan_delta",
        ObserverDecisionKind::Pause => "pause",
        ObserverDecisionKind::Replan => "replan",
    }
}

const fn trace_gate_code(outcome: TraceGateOutcome) -> &'static str {
    match outcome {
        TraceGateOutcome::Allow => "allow",
        TraceGateOutcome::AllowWithWatch => "allow_with_watch",
        TraceGateOutcome::RequestPlanDelta => "request_plan_delta",
        TraceGateOutcome::PauseForHuman => "pause_for_human",
        TraceGateOutcome::DenyAction => "deny_action",
        TraceGateOutcome::ReplanRequired => "replan_required",
    }
}

const fn failure_route_code(route: FailureRoute) -> &'static str {
    match route {
        FailureRoute::Repair => "repair",
        FailureRoute::Replan => "replan",
        FailureRoute::Clarification => "clarification",
        FailureRoute::AcceptanceReview => "acceptance_review",
        FailureRoute::InfraRetry => "infra_retry",
        FailureRoute::ModelEscalation => "model_escalation",
        FailureRoute::HumanReview => "human_review",
        FailureRoute::Abort => "abort",
    }
}

fn require_revision(actual: u64, expected: u64) -> Result<(), ObserverDecisionServiceError> {
    if actual != expected {
        return Err(observer_error(
            ObserverDecisionServiceErrorCode::RevisionConflict,
            format!("Observer expected revision {expected}, current revision {actual}"),
        ));
    }
    Ok(())
}

fn next_revision(current: u64) -> Result<u64, ObserverDecisionServiceError> {
    current
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_INTEGER)
        .ok_or_else(|| observer_corrupt("Observer revision overflowed"))
}

fn decode_receipt(
    receipt: &CommitReceipt,
    expected_kind: MutationKind,
    replayed: bool,
) -> Result<ObserverDecisionMutationReceipt, ObserverDecisionServiceError> {
    let [event] = receipt.events.as_slice() else {
        return Err(observer_corrupt(
            "Observer receipt does not contain exactly one event",
        ));
    };
    if event.topic != OBSERVER_DECISION_RECEIPT_TOPIC {
        return Err(observer_corrupt("Observer receipt has another event topic"));
    }
    let persisted: PersistedObserverMutationEvent = serde_json::from_slice(&event.payload)
        .map_err(|error| {
            observer_corrupt(format!("Observer receipt cannot be decoded: {error}"))
        })?;
    if persisted.schema_version != OBSERVER_DECISION_SERVICE_SCHEMA_VERSION
        || persisted.kind != expected_kind
        || persisted.catalog_revision != receipt.revision
    {
        return Err(observer_corrupt(
            "Observer receipt contract, command, or revision is inconsistent",
        ));
    }
    Ok(ObserverDecisionMutationReceipt {
        projection: persisted.decision.to_projection()?,
        catalog_revision: persisted.catalog_revision,
        replayed,
    })
}

fn observer_catalog_stream_id(scope: &ReceiptScopeKey) -> String {
    format!("observer-decisions:{:x}", Sha256::digest(scope.as_bytes()))
}

fn command_digest(
    kind: &str,
    fields: serde_json::Value,
) -> Result<Sha256Digest, ObserverDecisionServiceError> {
    let bytes = serde_json::to_vec(&(kind, fields)).map_err(|error| {
        observer_corrupt(format!("Observer command cannot be encoded: {error}"))
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn context_digest_fields(context: &ObserverDecisionCommandContext) -> serde_json::Value {
    serde_json::json!({
        "expectedRevision": context.expected_revision,
        "eventId": context.event_id.0,
        "occurredAt": context.occurred_at.0,
    })
}

fn record_digest_fields(command: &RecordObserverDecisionCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "source": command.source,
        "decision": {
            "kind": command.decision.kind,
            "origin": command.decision.origin,
            "sourceDecisionCode": command.decision.source_decision_code,
            "reasonCode": command.decision.reason_code,
            "summary": command.decision.summary,
            "evidenceRefIds": command.decision.evidence_ref_ids,
        },
        "safeCheckpoint": command.safe_checkpoint,
    })
}

fn checkpoint_digest_fields(command: &ApplyObserverCheckpointCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id,
        "decisionEventId": command.decision_event_id,
        "safeCheckpoint": command.safe_checkpoint,
    })
}

fn product_session_storage_error(error: &ProductSessionServiceError) -> StorageError {
    StorageError::adapter(error.to_string())
}

fn observer_storage_error(error: &StorageError) -> ObserverDecisionServiceError {
    let code = match error.kind() {
        StorageErrorKind::RevisionConflict => ObserverDecisionServiceErrorCode::RevisionConflict,
        StorageErrorKind::RequestConflict | StorageErrorKind::RequestReplayMissing => {
            ObserverDecisionServiceErrorCode::RequestConflict
        }
        StorageErrorKind::InvalidInput => ObserverDecisionServiceErrorCode::InvalidInput,
        StorageErrorKind::Adapter
        | StorageErrorKind::Closed
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired => ObserverDecisionServiceErrorCode::Storage,
    };
    observer_error(code, format!("Observer storage failed: {error}"))
}

fn observer_not_found() -> ObserverDecisionServiceError {
    observer_error(
        ObserverDecisionServiceErrorCode::NotFound,
        "Observer ProductSession or decision does not exist in this scope",
    )
}

fn observer_invalid(message: impl Into<String>) -> ObserverDecisionServiceError {
    observer_error(ObserverDecisionServiceErrorCode::InvalidInput, message)
}

fn observer_corrupt(message: impl Into<String>) -> ObserverDecisionServiceError {
    observer_error(ObserverDecisionServiceErrorCode::CorruptState, message)
}

fn observer_error(
    code: ObserverDecisionServiceErrorCode,
    message: impl Into<String>,
) -> ObserverDecisionServiceError {
    ObserverDecisionServiceError {
        code,
        message: message.into(),
    }
}
