// SPDX-License-Identifier: Apache-2.0

//! Durable routing from sealed Gate decisions to Approval and Attention facts.
//!
//! This service persists business facts only. The Action Gateway remains the
//! only action execution point, [`InteractionRouter`] remains the authority for
//! human-response semantics, and UI/Inbox state is produced by downstream
//! projections.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ApprovalId, AttentionItemId, ControlPlaneEventId, Instant, RequestId, ServiceAccountId,
    Sha256Digest, StageRunId, SystemActorId, UserId,
};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_session::{
    AuthenticatedActor, DecisionRouteBinding, ExecutionRoute, InteractionDecision,
    InteractionExpiry, InteractionOutcome, InteractionRegistration, InteractionResponse,
    InteractionRouter, InteractionRoutingError, InteractionSubject, RuntimeRouteAuthority,
};
use winwincode_storage::{
    ExecutionQueueScope, ExecutionReservationState, NewOutboxEvent, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StateMutation, StorageError, StorageErrorKind, WorkerPoolId,
    WorkerSlotAuthority, WorkerSlotState,
};

use crate::{
    ObserverDecisionKind, ObserverDecisionProjection, ProductSessionPersistence,
    ProductSessionService, ProductSessionServiceError,
};

/// Version of the durable Gate interaction state and event payload.
pub const GATE_INTERACTION_SCHEMA_VERSION: u8 = 1;
const GATE_INTERACTION_RECEIPT_TOPIC: &str = "gate-interaction.receipt.internal.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_BOUNDED_TEXT: usize = 2_000;

/// Authenticated owner sealed into a pending Approval or Attention route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum GateInteractionActor {
    User(UserId),
    ServiceAccount(ServiceAccountId),
    System(SystemActorId),
}

/// Exact identity of the user-facing business fact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum GateInteractionSubject {
    Approval(ApprovalId),
    Attention(AttentionItemId),
}

/// Non-executing Gate/Observer decisions that require a human route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutableGateDecisionKind {
    PlanDelta,
    Pause,
    Deny,
    Replan,
}

/// Secret-safe Gate decision fact. Free-form policy text is retained only by digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutableGateDecision {
    pub kind: RoutableGateDecisionKind,
    pub reason_sha256: Sha256Digest,
}

impl RoutableGateDecision {
    /// Converts one Action Gateway stop decision into its safe routing fact.
    ///
    /// # Errors
    ///
    /// Allowing decisions do not create Approval or Attention business state.
    pub fn from_gate(decision: &GateDecision) -> Result<Self, GateInteractionServiceError> {
        let (kind, reason) = match decision {
            GateDecision::RequestPlanDelta { reason } => {
                (RoutableGateDecisionKind::PlanDelta, reason)
            }
            GateDecision::PauseForHuman { reason } => (RoutableGateDecisionKind::Pause, reason),
            GateDecision::DenyAction { reason } => (RoutableGateDecisionKind::Deny, reason),
            GateDecision::ReplanRequired { reason } => (RoutableGateDecisionKind::Replan, reason),
            GateDecision::Allow | GateDecision::AllowWithWatch { .. } => {
                return Err(error(
                    GateInteractionServiceErrorCode::DecisionNotRoutable,
                    "allowing Gate decisions do not create human interaction state",
                ));
            }
        };
        if reason.is_empty() || reason.len() > MAX_BOUNDED_TEXT {
            return Err(invalid("Gate decision reason is invalid"));
        }
        Ok(Self {
            kind,
            reason_sha256: sha256(reason.as_bytes()),
        })
    }

    /// Converts one durable Observer projection into its safe routing fact.
    ///
    /// # Errors
    ///
    /// WATCH is observational and does not create a human interaction.
    pub fn from_observer(
        projection: &ObserverDecisionProjection,
    ) -> Result<Self, GateInteractionServiceError> {
        let kind = match projection.kind() {
            ObserverDecisionKind::PlanDelta => RoutableGateDecisionKind::PlanDelta,
            ObserverDecisionKind::Pause => RoutableGateDecisionKind::Pause,
            ObserverDecisionKind::Replan => RoutableGateDecisionKind::Replan,
            ObserverDecisionKind::Watch => {
                return Err(error(
                    GateInteractionServiceErrorCode::DecisionNotRoutable,
                    "WATCH Observer decisions do not create human interaction state",
                ));
            }
        };
        let reason = serde_json::to_vec(&(
            projection.reason_code(),
            projection.summary(),
            projection.evidence_ref_ids(),
        ))
        .map_err(|encode_error| {
            corrupt(format!(
                "Observer decision reason cannot be encoded: {encode_error}"
            ))
        })?;
        Ok(Self {
            kind,
            reason_sha256: sha256(&reason),
        })
    }
}

/// Action and envelope facts which the Action Gateway seals around an Observer decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateActionSeal {
    pub action_id: String,
    pub action_digest: Sha256Digest,
    pub envelope_version: u64,
    pub envelope_digest: Sha256Digest,
    pub candidate: Option<GateCandidateIdentity>,
}

/// Optional candidate identity sealed with an action decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateCandidateIdentity {
    pub candidate_ref: String,
    pub candidate_digest: Sha256Digest,
    pub candidate_revision: u64,
}

/// Exact action and policy-envelope fact produced by the Gate/Observer path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateDecisionFact {
    pub decision: RoutableGateDecision,
    pub action_id: String,
    pub action_digest: Sha256Digest,
    pub envelope_version: u64,
    pub envelope_digest: Sha256Digest,
    pub decision_revision: u64,
    pub candidate: Option<GateCandidateIdentity>,
}

/// All execution facts that must remain exact while a human route is pending.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateInteractionAuthority {
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub product_session_revision: u64,
    pub stage_run_id: Option<StageRunId>,
    pub job_revision: u64,
    pub worker_slot_revision: u64,
    pub runtime: WorkerSlotAuthority,
    pub lease_expires_at: Instant,
    pub gate: GateDecisionFact,
}

impl GateInteractionAuthority {
    /// Joins one durable Observer projection to the Action Gateway seal which
    /// identified the stopped action.
    ///
    /// # Errors
    ///
    /// Rejects WATCH projections and internally inconsistent Observer sources.
    pub fn from_observer(
        projection: &ObserverDecisionProjection,
        action: GateActionSeal,
        lease_expires_at: Instant,
    ) -> Result<Self, GateInteractionServiceError> {
        let source = projection.source();
        if source.product_session_id != source.execution_scope.product_session_id
            || source.execution_job_id.0.is_empty()
            || source.worker_session_id.0.is_empty()
        {
            return Err(authority_mismatch(
                "Observer projection does not carry one exact execution source",
            ));
        }
        Ok(Self {
            execution_scope: source.execution_scope.clone(),
            worker_pool_id: source.worker_pool_id.clone(),
            product_session_revision: source.product_session_revision,
            stage_run_id: source.stage_run_id.clone(),
            job_revision: source.job_revision,
            worker_slot_revision: source.worker_slot_revision,
            runtime: WorkerSlotAuthority {
                worker_id: source.worker_id.clone(),
                worker_instance_id: source.worker_instance_id.clone(),
                worker_session_id: source.worker_session_id.clone(),
                codex_thread_id: source.codex_thread_id.clone(),
                job_id: source.execution_job_id.clone(),
                lease_id: source.lease_id.clone(),
                attempt: source.attempt,
                fencing_token: source.fencing_token.clone(),
            },
            lease_expires_at,
            gate: GateDecisionFact {
                decision: RoutableGateDecision::from_observer(projection)?,
                action_id: action.action_id,
                action_digest: action.action_digest,
                envelope_version: action.envelope_version,
                envelope_digest: action.envelope_digest,
                decision_revision: projection.session_revision(),
                candidate: action.candidate,
            },
        })
    }
}

/// Common durable command facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateInteractionCommandContext {
    pub receipt_identity: ReceiptIdentity,
    pub event_id: ControlPlaneEventId,
    pub occurred_at: Instant,
}

/// Registers one sealed Gate decision as Approval or Attention.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterGateInteractionCommand {
    pub context: GateInteractionCommandContext,
    pub subject: GateInteractionSubject,
    pub authority: GateInteractionAuthority,
    pub authorized_actor: GateInteractionActor,
    pub expires_at: Instant,
    /// Empty for Approval; the only accepted resolutions for Attention.
    pub attention_decisions: Vec<String>,
}

/// Typed human decision accepted by the Gate interaction route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GateHumanDecision {
    Approve {
        reason_sha256: Sha256Digest,
    },
    Reject {
        reason_sha256: Sha256Digest,
    },
    ResolveAttention {
        decision: String,
        resolution_sha256: Sha256Digest,
    },
}

/// Responds to one exact pending route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RespondGateInteractionCommand {
    pub context: GateInteractionCommandContext,
    pub subject: GateInteractionSubject,
    pub authority: GateInteractionAuthority,
    pub actor: GateInteractionActor,
    pub decision: GateHumanDecision,
    pub responded_at: Instant,
}

/// Expires one exact unresolved route without authorizing an action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpireGateInteractionCommand {
    pub context: GateInteractionCommandContext,
    pub subject: GateInteractionSubject,
    pub authority: GateInteractionAuthority,
    pub expired_at: Instant,
}

/// Durable lifecycle state exposed to business projections.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateInteractionState {
    Pending,
    Approved,
    Rejected,
    AttentionResolved,
    Expired,
}

/// Secret-safe durable business fact. It carries no UI layout or Inbox state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateInteractionRecord {
    pub subject: GateInteractionSubject,
    pub authority: GateInteractionAuthority,
    pub authorized_actor: GateInteractionActor,
    pub expires_at: Instant,
    pub attention_decisions: Vec<String>,
    pub state: GateInteractionState,
    pub current_revision: u64,
}

/// Replay-safe mutation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateInteractionMutationReceipt {
    pub record: GateInteractionRecord,
    pub replayed: bool,
}

/// One prepared Gate-state write that another Control Plane aggregate can
/// attach to the same durable receipt transaction.
pub struct PreparedGateInteractionMutation {
    stream_id: String,
    expected_revision: u64,
    persisted: PersistedGateInteraction,
}

impl PreparedGateInteractionMutation {
    pub const fn record(&self) -> &GateInteractionRecord {
        &self.persisted.record
    }

    pub fn state_mutation(&self) -> Result<StateMutation, GateInteractionServiceError> {
        let state = serde_json::to_vec(&self.persisted).map_err(|error| {
            corrupt(format!("Gate interaction state cannot be encoded: {error}"))
        })?;
        StateMutation::new(self.stream_id.clone(), self.expected_revision, state)
            .map_err(|error| storage_error(&error))
    }
}

/// Stable fail-closed service code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateInteractionServiceErrorCode {
    InvalidInput,
    DecisionNotRoutable,
    SubjectMismatch,
    NotFound,
    AlreadyResolved,
    RequestConflict,
    RevisionConflict,
    AuthorityMismatch,
    ActorMismatch,
    Expired,
    CorruptState,
    Storage,
}

/// Gate interaction service failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateInteractionServiceError {
    code: GateInteractionServiceErrorCode,
    message: String,
}

impl GateInteractionServiceError {
    #[must_use]
    pub const fn code(&self) -> GateInteractionServiceErrorCode {
        self.code
    }
}

impl fmt::Display for GateInteractionServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GateInteractionServiceError {}

/// Durable application service joining Gate facts to the human route state machine.
pub struct GateInteractionService<'storage> {
    storage: &'storage mut dyn ProductSessionPersistence,
}

impl<'storage> GateInteractionService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductSessionPersistence) -> Self {
        Self { storage }
    }

    /// Persists one Approval or Attention registration.
    ///
    /// # Errors
    ///
    /// Rejects stale, expired, cross-scope, or non-routable decisions before writing.
    pub fn register(
        &mut self,
        command: &RegisterGateInteractionCommand,
    ) -> Result<GateInteractionMutationReceipt, GateInteractionServiceError> {
        let digest = command_digest("register", register_fields(command))?;
        if let Some(replay) = self.replay(&command.context.receipt_identity, &digest)? {
            return Ok(replay);
        }
        validate_subject_decision(&command.subject, command.authority.gate.decision.kind)?;
        validate_attention_choices(&command.subject, &command.attention_decisions)?;
        validate_authority_shape(&command.authority)?;
        self.require_current_authority(
            command.context.receipt_identity.scope_key(),
            &command.authority,
            &command.context.occurred_at,
        )?;
        if command.context.occurred_at.0 >= effective_expiry(command).0 {
            return Err(error(
                GateInteractionServiceErrorCode::Expired,
                "Gate interaction authority is already expired",
            ));
        }
        let registration = to_registration(command);
        let mut router = InteractionRouter::default();
        router
            .register_interaction(registration)
            .map_err(|error| route_error(&error))?;
        let record = GateInteractionRecord {
            subject: command.subject.clone(),
            authority: command.authority.clone(),
            authorized_actor: command.authorized_actor.clone(),
            expires_at: effective_expiry(command),
            attention_decisions: sorted_choices(&command.attention_decisions),
            state: GateInteractionState::Pending,
            current_revision: command.authority.gate.decision_revision,
        };
        self.commit(
            &command.context,
            digest,
            0,
            &PersistedGateInteraction {
                schema_version: GATE_INTERACTION_SCHEMA_VERSION,
                record,
                resolution: None,
            },
        )
    }

    /// Applies one authenticated response to an exact pending route.
    ///
    /// # Errors
    ///
    /// Rejects mismatched or stale facts. A response after the sealed deadline
    /// deterministically records Expired and never authorizes the action.
    pub fn respond(
        &mut self,
        command: &RespondGateInteractionCommand,
    ) -> Result<GateInteractionMutationReceipt, GateInteractionServiceError> {
        let digest = command_digest("respond", respond_fields(command))?;
        if let Some(replay) = self.replay(&command.context.receipt_identity, &digest)? {
            return Ok(replay);
        }
        let prepared = self.prepare_response(command)?;
        self.commit(
            &command.context,
            digest,
            prepared.expected_revision,
            &prepared.persisted,
        )
    }

    /// Prepares the canonical Gate write for an application transaction that
    /// also updates another Control Plane state stream.
    ///
    /// # Errors
    ///
    /// Rejects a missing, terminal, stale, cross-scope, or unauthorized fact.
    pub fn prepare_response(
        &mut self,
        command: &RespondGateInteractionCommand,
    ) -> Result<PreparedGateInteractionMutation, GateInteractionServiceError> {
        let (stored_revision, mut persisted) = self.load(
            command.context.receipt_identity.scope_key(),
            &command.subject,
        )?;
        require_exact_record(&persisted.record, &command.subject, &command.authority)?;
        if persisted.record.authorized_actor != command.actor {
            return Err(error(
                GateInteractionServiceErrorCode::ActorMismatch,
                "authenticated actor does not own this Gate interaction",
            ));
        }
        if command.responded_at.0 < persisted.record.expires_at.0 {
            self.require_current_authority(
                command.context.receipt_identity.scope_key(),
                &command.authority,
                &command.responded_at,
            )?;
        }
        let mut router = rebuild_router(&persisted)?;
        let request_id = command.context.receipt_identity.request_id().clone();
        let (route_receipt, resolution) = if command.responded_at.0 >= persisted.record.expires_at.0
        {
            let route_receipt = router
                .expire(&InteractionExpiry {
                    request_id: request_id.clone(),
                    subject: to_subject(&command.subject),
                    binding: to_binding(&command.authority),
                    expired_at: command.responded_at.clone(),
                })
                .map_err(|error| route_error(&error))?;
            (
                route_receipt,
                PersistedResolution::Expiry {
                    request_id,
                    expired_at: command.responded_at.clone(),
                },
            )
        } else {
            let response = InteractionResponse {
                request_id,
                actor: to_actor(&command.actor),
                subject: to_subject(&command.subject),
                binding: to_binding(&command.authority),
                decision: to_decision(&command.decision),
                responded_at: command.responded_at.clone(),
            };
            let route_receipt = router
                .respond(&response)
                .map_err(|error| route_error(&error))?;
            (
                route_receipt,
                PersistedResolution::Response {
                    request_id: response.request_id,
                    actor: command.actor.clone(),
                    decision: command.decision.clone(),
                    responded_at: command.responded_at.clone(),
                },
            )
        };
        persisted.record.state = state_from_outcome(route_receipt.outcome);
        persisted.record.current_revision = route_receipt.current_revision;
        persisted.resolution = Some(resolution);
        Ok(PreparedGateInteractionMutation {
            stream_id: interaction_stream_id(
                command.context.receipt_identity.scope_key(),
                &command.subject,
            )?,
            expected_revision: stored_revision,
            persisted,
        })
    }

    /// Expires one exact pending route.
    ///
    /// # Errors
    ///
    /// Rejects early expiry, binding mismatch, final state, and storage errors.
    pub fn expire(
        &mut self,
        command: &ExpireGateInteractionCommand,
    ) -> Result<GateInteractionMutationReceipt, GateInteractionServiceError> {
        let digest = command_digest("expire", expire_fields(command))?;
        if let Some(replay) = self.replay(&command.context.receipt_identity, &digest)? {
            return Ok(replay);
        }
        let (stored_revision, mut persisted) = self.load(
            command.context.receipt_identity.scope_key(),
            &command.subject,
        )?;
        require_exact_record(&persisted.record, &command.subject, &command.authority)?;
        let mut router = rebuild_router(&persisted)?;
        let expiry = InteractionExpiry {
            request_id: command.context.receipt_identity.request_id().clone(),
            subject: to_subject(&command.subject),
            binding: to_binding(&command.authority),
            expired_at: command.expired_at.clone(),
        };
        let route_receipt = router
            .expire(&expiry)
            .map_err(|error| route_error(&error))?;
        persisted.record.state = GateInteractionState::Expired;
        persisted.record.current_revision = route_receipt.current_revision;
        persisted.resolution = Some(PersistedResolution::Expiry {
            request_id: expiry.request_id,
            expired_at: command.expired_at.clone(),
        });
        self.commit(&command.context, digest, stored_revision, &persisted)
    }

    /// Reads one exact business fact after restart.
    ///
    /// # Errors
    ///
    /// Returns corruption or storage errors for an invalid durable record.
    pub fn get(
        &self,
        scope: &ReceiptScopeKey,
        subject: &GateInteractionSubject,
    ) -> Result<Option<GateInteractionRecord>, GateInteractionServiceError> {
        let stream_id = interaction_stream_id(scope, subject)?;
        let Some(state) = self
            .storage
            .load_state(&stream_id)
            .map_err(|error| storage_error(&error))?
        else {
            return Ok(None);
        };
        let persisted = decode_state(&state.payload, state.revision)?;
        Ok(Some(persisted.record))
    }

    /// Revalidates one pending durable Gate fact against the current
    /// `ProductSession`, Worker slot, admission, lease, and fence.
    ///
    /// # Errors
    ///
    /// Rejects a terminal, expired, stale, or corrupt durable fact.
    pub fn require_current_pending(
        &mut self,
        scope: &ReceiptScopeKey,
        record: &GateInteractionRecord,
        now: &Instant,
    ) -> Result<(), GateInteractionServiceError> {
        if record.state != GateInteractionState::Pending {
            return Err(error(
                GateInteractionServiceErrorCode::AlreadyResolved,
                "Gate interaction is no longer pending",
            ));
        }
        if now.0 >= record.expires_at.0 {
            return Err(error(
                GateInteractionServiceErrorCode::Expired,
                "Gate interaction has expired",
            ));
        }
        self.require_current_authority(scope, &record.authority, now)
    }

    fn replay(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<GateInteractionMutationReceipt>, GateInteractionServiceError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(|error| storage_error(&error))?
            .map(|receipt| decode_receipt(&receipt.events, true))
            .transpose()
    }

    fn load(
        &self,
        scope: &ReceiptScopeKey,
        subject: &GateInteractionSubject,
    ) -> Result<(u64, PersistedGateInteraction), GateInteractionServiceError> {
        let stream_id = interaction_stream_id(scope, subject)?;
        let state = self
            .storage
            .load_state(&stream_id)
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                error(
                    GateInteractionServiceErrorCode::NotFound,
                    "Gate interaction was not found",
                )
            })?;
        let persisted = decode_state(&state.payload, state.revision)?;
        Ok((state.revision, persisted))
    }

    fn require_current_authority(
        &mut self,
        scope: &ReceiptScopeKey,
        authority: &GateInteractionAuthority,
        now: &Instant,
    ) -> Result<(), GateInteractionServiceError> {
        if now.0 >= authority.lease_expires_at.0 {
            return Err(error(
                GateInteractionServiceErrorCode::Expired,
                "Worker lease has expired",
            ));
        }
        let record = ProductSessionService::new(self.storage)
            .get(scope, &authority.execution_scope.product_session_id)
            .map_err(|error| product_session_error(&error))?
            .ok_or_else(|| {
                error(
                    GateInteractionServiceErrorCode::AuthorityMismatch,
                    "ProductSession is not current in this receipt scope",
                )
            })?;
        require_product_session(&record, authority)?;
        let current = self
            .storage
            .load_worker_interaction_source(
                &authority.execution_scope,
                &authority.worker_pool_id,
                &authority.runtime.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                authority_mismatch("current Worker slot or reservation was not found")
            })?;
        let (slot, reservation, lease) = current;
        if slot.state != WorkerSlotState::Running
            || slot.authority != authority.runtime
            || slot.revision != authority.worker_slot_revision
            || reservation.state != ExecutionReservationState::Running
            || reservation.scope != authority.execution_scope
            || reservation.worker_pool_id != authority.worker_pool_id
            || reservation.job_id != authority.runtime.job_id
            || reservation.revision != authority.job_revision
            || lease.job_id != authority.runtime.job_id
            || lease.lease_id != authority.runtime.lease_id
            || lease.worker_id != authority.runtime.worker_id
            || lease.worker_instance_id != authority.runtime.worker_instance_id
            || lease.attempt != authority.runtime.attempt
            || lease.fencing_token != authority.runtime.fencing_token
            || lease.expires_at != authority.lease_expires_at
        {
            return Err(authority_mismatch(
                "current Worker slot, lease, fence, or admission does not match",
            ));
        }
        Ok(())
    }

    fn commit(
        &mut self,
        context: &GateInteractionCommandContext,
        digest: Sha256Digest,
        expected_revision: u64,
        persisted: &PersistedGateInteraction,
    ) -> Result<GateInteractionMutationReceipt, GateInteractionServiceError> {
        let state = serde_json::to_vec(&persisted).map_err(|error| {
            corrupt(format!("Gate interaction state cannot be encoded: {error}"))
        })?;
        let event = PersistedGateInteractionEvent {
            schema_version: GATE_INTERACTION_SCHEMA_VERSION,
            record: persisted.record.clone(),
        };
        let payload = serde_json::to_vec(&event).map_err(|error| {
            corrupt(format!("Gate interaction event cannot be encoded: {error}"))
        })?;
        let stream_id = interaction_stream_id(
            context.receipt_identity.scope_key(),
            &persisted.record.subject,
        )?;
        let receipt_event = NewOutboxEvent::internal(
            format!("internal:gate-interaction:{}", context.event_id.0),
            GATE_INTERACTION_RECEIPT_TOPIC,
            payload,
        );
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            stream_id,
            expected_revision,
            state,
            vec![receipt_event],
        );
        let receipt = self
            .storage
            .commit(&commit)
            .map_err(|error| storage_error(&error))?;
        decode_receipt(&receipt.events, receipt.idempotent_replay)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedGateInteraction {
    schema_version: u8,
    record: GateInteractionRecord,
    resolution: Option<PersistedResolution>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum PersistedResolution {
    Response {
        request_id: RequestId,
        actor: GateInteractionActor,
        decision: GateHumanDecision,
        responded_at: Instant,
    },
    Expiry {
        request_id: RequestId,
        expired_at: Instant,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedGateInteractionEvent {
    schema_version: u8,
    record: GateInteractionRecord,
}

fn decode_state(
    payload: &[u8],
    stored_revision: u64,
) -> Result<PersistedGateInteraction, GateInteractionServiceError> {
    let persisted: PersistedGateInteraction = serde_json::from_slice(payload)
        .map_err(|error| corrupt(format!("Gate interaction state cannot be decoded: {error}")))?;
    if persisted.schema_version != GATE_INTERACTION_SCHEMA_VERSION
        || persisted.record.current_revision == 0
        || stored_revision != 1 + u64::from(persisted.resolution.is_some())
        || persisted.record.current_revision
            != persisted
                .record
                .authority
                .gate
                .decision_revision
                .checked_add(u64::from(persisted.resolution.is_some()))
                .ok_or_else(|| corrupt("Gate interaction revision overflowed"))?
    {
        return Err(corrupt(
            "Gate interaction state contract or decision revision is inconsistent",
        ));
    }
    validate_durable_record(&persisted.record)?;
    let rebuilt = rebuild_router(&persisted)?;
    drop(rebuilt);
    Ok(persisted)
}

fn validate_durable_record(
    record: &GateInteractionRecord,
) -> Result<(), GateInteractionServiceError> {
    let valid = validate_authority_shape(&record.authority).is_ok()
        && validate_subject_decision(&record.subject, record.authority.gate.decision.kind).is_ok()
        && validate_attention_choices(&record.subject, &record.attention_decisions).is_ok()
        && canonical_instant(&record.expires_at)
        && record.expires_at.0 <= record.authority.lease_expires_at.0;
    if !valid {
        return Err(corrupt("stored Gate interaction authority is invalid"));
    }
    Ok(())
}

fn rebuild_router(
    persisted: &PersistedGateInteraction,
) -> Result<InteractionRouter, GateInteractionServiceError> {
    let mut router = InteractionRouter::default();
    router
        .register_interaction(InteractionRegistration {
            subject: to_subject(&persisted.record.subject),
            binding: to_binding(&persisted.record.authority),
            authorized_actor: to_actor(&persisted.record.authorized_actor),
            expires_at: persisted.record.expires_at.clone(),
            attention_decisions: persisted.record.attention_decisions.clone(),
        })
        .map_err(|error| corrupt(format!("stored Gate interaction is invalid: {error}")))?;
    if let Some(resolution) = &persisted.resolution {
        let receipt = match resolution {
            PersistedResolution::Response {
                request_id,
                actor,
                decision,
                responded_at,
            } => router.respond(&InteractionResponse {
                request_id: request_id.clone(),
                actor: to_actor(actor),
                subject: to_subject(&persisted.record.subject),
                binding: to_binding(&persisted.record.authority),
                decision: to_decision(decision),
                responded_at: responded_at.clone(),
            }),
            PersistedResolution::Expiry {
                request_id,
                expired_at,
            } => router.expire(&InteractionExpiry {
                request_id: request_id.clone(),
                subject: to_subject(&persisted.record.subject),
                binding: to_binding(&persisted.record.authority),
                expired_at: expired_at.clone(),
            }),
        }
        .map_err(|error| {
            corrupt(format!(
                "stored Gate interaction resolution is invalid: {error}"
            ))
        })?;
        if state_from_outcome(receipt.outcome) != persisted.record.state
            || receipt.current_revision != persisted.record.current_revision
        {
            return Err(corrupt(
                "stored Gate interaction outcome or revision is inconsistent",
            ));
        }
    } else if persisted.record.state != GateInteractionState::Pending {
        return Err(corrupt(
            "pending Gate interaction has a terminal durable state",
        ));
    }
    Ok(router)
}

fn require_product_session(
    record: &crate::ProductSessionRecord,
    authority: &GateInteractionAuthority,
) -> Result<(), GateInteractionServiceError> {
    let session = record.session();
    if session.revision() != authority.product_session_revision
        || session.id() != &authority.execution_scope.product_session_id
        || session.project_id() != &authority.execution_scope.project_id
        || session.repository_id() != &authority.execution_scope.repository_id
    {
        return Err(authority_mismatch(
            "ProductSession identity or revision is stale",
        ));
    }
    let exact_binding = record.bindings().iter().any(|durable| {
        let binding = durable.binding();
        binding.product_session_id() == &authority.execution_scope.product_session_id
            && binding.execution_job_id() == &authority.runtime.job_id
            && binding.stage_run_id() == authority.stage_run_id.as_ref()
            && binding.delivery_id() == authority.execution_scope.delivery_id.as_ref()
            && binding.worker_session_id() == Some(&authority.runtime.worker_session_id)
            && binding.codex_thread_id() == Some(&authority.runtime.codex_thread_id)
            && durable.slot().authority == authority.runtime
    });
    if !exact_binding {
        return Err(authority_mismatch(
            "ProductSession has no exact StageRun, Job, WorkerSession, or CodexThread binding",
        ));
    }
    Ok(())
}

fn require_exact_record(
    record: &GateInteractionRecord,
    subject: &GateInteractionSubject,
    authority: &GateInteractionAuthority,
) -> Result<(), GateInteractionServiceError> {
    if &record.subject != subject {
        return Err(error(
            GateInteractionServiceErrorCode::SubjectMismatch,
            "Gate interaction subject does not match",
        ));
    }
    if &record.authority != authority {
        return Err(authority_mismatch(
            "Gate interaction authority does not match its sealed registration",
        ));
    }
    Ok(())
}

fn validate_authority_shape(
    authority: &GateInteractionAuthority,
) -> Result<(), GateInteractionServiceError> {
    if authority.execution_scope.product_session_id.0.is_empty()
        || authority.runtime.job_id.0.is_empty()
        || authority.runtime.attempt == 0
        || authority.runtime.attempt > 1_000
        || authority.product_session_revision == 0
        || authority.product_session_revision > MAX_SAFE_INTEGER
        || authority.job_revision == 0
        || authority.job_revision > MAX_SAFE_INTEGER
        || authority.worker_slot_revision == 0
        || authority.worker_slot_revision > MAX_SAFE_INTEGER
        || authority.gate.decision_revision == 0
        || authority.gate.decision_revision > MAX_SAFE_INTEGER
        || authority.gate.envelope_version == 0
        || authority.gate.envelope_version > MAX_SAFE_INTEGER
        || authority.gate.action_id.is_empty()
        || authority.gate.action_id.len() > MAX_BOUNDED_TEXT
        || !canonical_digest(&authority.gate.action_digest)
        || !canonical_digest(&authority.gate.envelope_digest)
        || !canonical_digest(&authority.gate.decision.reason_sha256)
        || !canonical_instant(&authority.lease_expires_at)
    {
        return Err(invalid("Gate interaction authority is invalid"));
    }
    if let Some(candidate) = &authority.gate.candidate
        && (candidate.candidate_ref.is_empty()
            || candidate.candidate_ref.len() > MAX_BOUNDED_TEXT
            || !canonical_digest(&candidate.candidate_digest)
            || candidate.candidate_revision == 0
            || candidate.candidate_revision > MAX_SAFE_INTEGER)
    {
        return Err(invalid("Gate interaction candidate identity is invalid"));
    }
    Ok(())
}

fn validate_subject_decision(
    subject: &GateInteractionSubject,
    kind: RoutableGateDecisionKind,
) -> Result<(), GateInteractionServiceError> {
    let matches = matches!(
        (subject, kind),
        (
            GateInteractionSubject::Approval(_),
            RoutableGateDecisionKind::PlanDelta
        ) | (
            GateInteractionSubject::Attention(_),
            RoutableGateDecisionKind::Pause
                | RoutableGateDecisionKind::Deny
                | RoutableGateDecisionKind::Replan
        )
    );
    if !matches {
        return Err(error(
            GateInteractionServiceErrorCode::SubjectMismatch,
            "Gate decision does not map to this interaction subject",
        ));
    }
    Ok(())
}

fn validate_attention_choices(
    subject: &GateInteractionSubject,
    choices: &[String],
) -> Result<(), GateInteractionServiceError> {
    match subject {
        GateInteractionSubject::Approval(_) if choices.is_empty() => Ok(()),
        GateInteractionSubject::Attention(_) if !choices.is_empty() => {
            if choices
                .iter()
                .any(|choice| choice.is_empty() || choice.len() > MAX_BOUNDED_TEXT)
            {
                return Err(invalid("Attention choices are invalid"));
            }
            let sorted = sorted_choices(choices);
            if sorted.len() != choices.len() {
                return Err(invalid("Attention choices must be unique"));
            }
            Ok(())
        }
        _ => Err(invalid(
            "Approval must have no choices and Attention must have sealed choices",
        )),
    }
}

fn sorted_choices(choices: &[String]) -> Vec<String> {
    let mut sorted = choices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

fn effective_expiry(command: &RegisterGateInteractionCommand) -> Instant {
    if command.expires_at.0 <= command.authority.lease_expires_at.0 {
        command.expires_at.clone()
    } else {
        command.authority.lease_expires_at.clone()
    }
}

fn to_registration(command: &RegisterGateInteractionCommand) -> InteractionRegistration {
    InteractionRegistration {
        subject: to_subject(&command.subject),
        binding: to_binding(&command.authority),
        authorized_actor: to_actor(&command.authorized_actor),
        expires_at: effective_expiry(command),
        attention_decisions: command.attention_decisions.clone(),
    }
}

fn to_subject(subject: &GateInteractionSubject) -> InteractionSubject {
    match subject {
        GateInteractionSubject::Approval(id) => InteractionSubject::Approval(id.clone()),
        GateInteractionSubject::Attention(id) => InteractionSubject::Attention(id.clone()),
    }
}

fn to_actor(actor: &GateInteractionActor) -> AuthenticatedActor {
    match actor {
        GateInteractionActor::User(id) => AuthenticatedActor::User(id.clone()),
        GateInteractionActor::ServiceAccount(id) => AuthenticatedActor::ServiceAccount(id.clone()),
        GateInteractionActor::System(id) => AuthenticatedActor::System(id.clone()),
    }
}

fn to_binding(authority: &GateInteractionAuthority) -> DecisionRouteBinding {
    DecisionRouteBinding {
        execution: ExecutionRoute {
            product_session_id: authority.execution_scope.product_session_id.clone(),
            stage_run_id: authority.stage_run_id.clone(),
            execution_job_id: authority.runtime.job_id.clone(),
            job_revision: authority.job_revision,
            runtime: Some(RuntimeRouteAuthority {
                lease_id: authority.runtime.lease_id.clone(),
                worker_id: authority.runtime.worker_id.clone(),
                worker_instance_id: authority.runtime.worker_instance_id.clone(),
                worker_session_id: authority.runtime.worker_session_id.clone(),
                attempt: authority.runtime.attempt,
                fencing_token: authority.runtime.fencing_token.clone(),
            }),
            worker_slot_revision: Some(authority.worker_slot_revision),
            model_exchange_id: None,
        },
        action_id: authority.gate.action_id.clone(),
        decision_revision: authority.gate.decision_revision,
    }
}

fn to_decision(decision: &GateHumanDecision) -> InteractionDecision {
    match decision {
        GateHumanDecision::Approve { reason_sha256 } => InteractionDecision::Approve {
            reason_sha256: reason_sha256.clone(),
        },
        GateHumanDecision::Reject { reason_sha256 } => InteractionDecision::Reject {
            reason_sha256: reason_sha256.clone(),
        },
        GateHumanDecision::ResolveAttention {
            decision,
            resolution_sha256,
        } => InteractionDecision::ResolveAttention {
            decision: decision.clone(),
            resolution_sha256: resolution_sha256.clone(),
        },
    }
}

const fn state_from_outcome(outcome: InteractionOutcome) -> GateInteractionState {
    match outcome {
        InteractionOutcome::Approved => GateInteractionState::Approved,
        InteractionOutcome::Rejected => GateInteractionState::Rejected,
        InteractionOutcome::AttentionResolved => GateInteractionState::AttentionResolved,
        InteractionOutcome::Expired => GateInteractionState::Expired,
        InteractionOutcome::InputReceived => GateInteractionState::Pending,
    }
}

fn decode_receipt(
    events: &[winwincode_storage::OutboxEvent],
    replayed: bool,
) -> Result<GateInteractionMutationReceipt, GateInteractionServiceError> {
    let [event] = events else {
        return Err(corrupt(
            "Gate interaction receipt does not contain exactly one event",
        ));
    };
    if event.topic != GATE_INTERACTION_RECEIPT_TOPIC {
        return Err(corrupt(
            "Gate interaction receipt contains another event topic",
        ));
    }
    let event: PersistedGateInteractionEvent =
        serde_json::from_slice(&event.payload).map_err(|error| {
            corrupt(format!(
                "Gate interaction receipt cannot be decoded: {error}"
            ))
        })?;
    if event.schema_version != GATE_INTERACTION_SCHEMA_VERSION {
        return Err(corrupt("Gate interaction receipt schema is invalid"));
    }
    Ok(GateInteractionMutationReceipt {
        record: event.record,
        replayed,
    })
}

fn register_fields(command: &RegisterGateInteractionCommand) -> serde_json::Value {
    serde_json::json!({
        "eventId": command.context.event_id,
        "occurredAt": command.context.occurred_at,
        "subject": command.subject,
        "authority": command.authority,
        "authorizedActor": command.authorized_actor,
        "expiresAt": command.expires_at,
        "attentionDecisions": command.attention_decisions,
    })
}

fn respond_fields(command: &RespondGateInteractionCommand) -> serde_json::Value {
    serde_json::json!({
        "eventId": command.context.event_id,
        "occurredAt": command.context.occurred_at,
        "subject": command.subject,
        "authority": command.authority,
        "actor": command.actor,
        "decision": command.decision,
        "respondedAt": command.responded_at,
    })
}

fn expire_fields(command: &ExpireGateInteractionCommand) -> serde_json::Value {
    serde_json::json!({
        "eventId": command.context.event_id,
        "occurredAt": command.context.occurred_at,
        "subject": command.subject,
        "authority": command.authority,
        "expiredAt": command.expired_at,
    })
}

fn command_digest(
    operation: &str,
    fields: serde_json::Value,
) -> Result<Sha256Digest, GateInteractionServiceError> {
    let bytes = serde_json::to_vec(&(operation, fields)).map_err(|error| {
        corrupt(format!(
            "Gate interaction command cannot be encoded: {error}"
        ))
    })?;
    Ok(sha256(&bytes))
}

fn interaction_stream_id(
    scope: &ReceiptScopeKey,
    subject: &GateInteractionSubject,
) -> Result<String, GateInteractionServiceError> {
    let subject = serde_json::to_vec(subject).map_err(|error| {
        corrupt(format!(
            "Gate interaction subject cannot be encoded: {error}"
        ))
    })?;
    Ok(format!(
        "gate-interaction:{:x}:{:x}",
        Sha256::digest(scope.as_bytes()),
        Sha256::digest(subject)
    ))
}

fn canonical_instant(instant: &Instant) -> bool {
    let value = instant.0.as_bytes();
    value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn route_error(error_value: &InteractionRoutingError) -> GateInteractionServiceError {
    let code = match error_value {
        InteractionRoutingError::UnknownInteraction => GateInteractionServiceErrorCode::NotFound,
        InteractionRoutingError::ActorMismatch => GateInteractionServiceErrorCode::ActorMismatch,
        InteractionRoutingError::SubjectMismatch
        | InteractionRoutingError::DecisionKindMismatch
        | InteractionRoutingError::AttentionDecisionNotAllowed => {
            GateInteractionServiceErrorCode::SubjectMismatch
        }
        InteractionRoutingError::BindingMismatch => {
            GateInteractionServiceErrorCode::AuthorityMismatch
        }
        InteractionRoutingError::RevisionConflict { .. } => {
            GateInteractionServiceErrorCode::RevisionConflict
        }
        InteractionRoutingError::AlreadyResolved => {
            GateInteractionServiceErrorCode::AlreadyResolved
        }
        InteractionRoutingError::IdempotencyConflict => {
            GateInteractionServiceErrorCode::RequestConflict
        }
        InteractionRoutingError::DuplicateRegistration
        | InteractionRoutingError::InvalidField(_)
        | InteractionRoutingError::UnknownProductSession
        | InteractionRoutingError::SessionAlreadyCancelled => {
            GateInteractionServiceErrorCode::InvalidInput
        }
    };
    error(code, error_value.to_string())
}

fn storage_error(error_value: &StorageError) -> GateInteractionServiceError {
    let code = match error_value.kind() {
        StorageErrorKind::InvalidInput => GateInteractionServiceErrorCode::InvalidInput,
        StorageErrorKind::RevisionConflict => GateInteractionServiceErrorCode::RevisionConflict,
        StorageErrorKind::RequestConflict => GateInteractionServiceErrorCode::RequestConflict,
        _ => GateInteractionServiceErrorCode::Storage,
    };
    error(code, error_value.to_string())
}

fn product_session_error(error_value: &ProductSessionServiceError) -> GateInteractionServiceError {
    error(
        GateInteractionServiceErrorCode::Storage,
        error_value.to_string(),
    )
}

fn invalid(message: impl Into<String>) -> GateInteractionServiceError {
    error(GateInteractionServiceErrorCode::InvalidInput, message)
}

fn authority_mismatch(message: impl Into<String>) -> GateInteractionServiceError {
    error(GateInteractionServiceErrorCode::AuthorityMismatch, message)
}

fn corrupt(message: impl Into<String>) -> GateInteractionServiceError {
    error(GateInteractionServiceErrorCode::CorruptState, message)
}

fn error(
    code: GateInteractionServiceErrorCode,
    message: impl Into<String>,
) -> GateInteractionServiceError {
    GateInteractionServiceError {
        code,
        message: message.into(),
    }
}
