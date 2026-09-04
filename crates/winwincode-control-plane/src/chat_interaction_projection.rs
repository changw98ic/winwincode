// SPDX-License-Identifier: Apache-2.0

//! Secret-safe, rebuildable Chat input and approval projections.
//!
//! Execution Workers submit typed request messages to this module. Only the
//! public prompt, choices, summary, expiry, and complete execution binding are
//! retained. Browser clients read the resulting HTTP projection and use the
//! matching WebSocket event only as an invalidation signal.

use std::collections::{HashMap, HashSet};
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ApprovalDecidePayload, ApprovalEffectiveDecisionScope, ApprovalGetQuery,
    ApprovalGetResultResponse, ApprovalGetResultResponseQuery, ApprovalListQuery,
    ApprovalListResultResponse, ApprovalListResultResponseQuery, ApprovalPage, ApprovalPageKind,
    ApprovalProjection, ApprovalProjectionCategory, ApprovalSanitizedDetailProjection,
    ApprovalSanitizedDetailProjectionKind, ApprovalSanitizedDetailUnavailableReason,
    ChatApprovalInteractionProjection, ChatApprovalInteractionProjectionKind,
    ChatInputInteractionProjection, ChatInputInteractionProjectionKind,
    ChatInteractionBindingProjection, ChatInteractionListQuery, ChatInteractionListResultResponse,
    ChatInteractionListResultResponseQuery, ChatInteractionOptionProjection, ChatInteractionPage,
    ChatInteractionPageKind, ChatInteractionProjection,
    ControlPlaneWebSocketApprovalListReloadQuery,
    ControlPlaneWebSocketChatInteractionListReloadQuery,
    ControlPlaneWebSocketChatInteractionsInvalidatedEvent,
    ControlPlaneWebSocketChatInteractionsInvalidatedEventTypeValue, InputRespondPayload, PageInfo,
};
use winwincode_domain::{
    ApprovalId, ExecutionMessageId, InputRequestId, Instant, InteractiveInputMode, OpaqueCursor,
    ProductSessionId, Revision, SchemaVersion, Sha256Digest,
};
use winwincode_execution_port::generated::{
    ApprovalActionCategory, ApprovalRequestMessage, InputRequestMessage,
};

/// Durable, secret-safe event used to rebuild the browser projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatInteractionProjectionEvent {
    InputRecorded {
        source_message_id: ExecutionMessageId,
        source_sha256: Sha256Digest,
        projection: ChatInputInteractionProjection,
    },
    ApprovalRecorded {
        source_message_id: ExecutionMessageId,
        source_sha256: Sha256Digest,
        projection: ApprovalProjection,
    },
    InputResolved {
        input_request_id: InputRequestId,
        revision: Revision,
        state: String,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
        revision: Revision,
        state: String,
    },
}

/// Complete durable state. Serializing it never exposes an `ExecutionPort`
/// envelope, approval details, provider data, credential data, or a response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatInteractionProjectionSnapshot {
    pub events: Vec<ChatInteractionProjectionEvent>,
}

/// Whether a Worker request created a projection or exactly replayed one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionWriteStatus {
    Applied,
    Duplicate,
}

/// Result of recording one Worker interaction request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionWriteReceipt {
    pub status: ProjectionWriteStatus,
    pub revision: Revision,
    pub product_session_id: ProductSessionId,
}

/// Fail-closed projection or command-validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatInteractionProjectionError {
    InvalidField(&'static str),
    BindingMismatch(&'static str),
    SourceMessageConflict,
    InteractionIdentityConflict,
    UnknownInteraction,
    StateConflict,
    RevisionConflict {
        expected: Revision,
        actual: Revision,
    },
    Expired,
    UnsupportedState,
    InvalidPage,
    InvalidCursor,
    StaleCursor,
    SnapshotConflict,
}

impl fmt::Display for ChatInteractionProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(formatter, "invalid interaction field: {field}"),
            Self::BindingMismatch(field) => {
                write!(formatter, "interaction binding mismatch: {field}")
            }
            Self::SourceMessageConflict => {
                formatter.write_str("source message was replayed with different input")
            }
            Self::InteractionIdentityConflict => {
                formatter.write_str("interaction identity is already bound to another request")
            }
            Self::UnknownInteraction => formatter.write_str("interaction is not registered"),
            Self::StateConflict => formatter.write_str("interaction is no longer pending"),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "interaction revision conflict: expected {}, actual {}",
                expected.0, actual.0
            ),
            Self::Expired => formatter.write_str("interaction binding has expired"),
            Self::UnsupportedState => formatter.write_str("interaction state is unsupported"),
            Self::InvalidPage => formatter.write_str("interaction page request is invalid"),
            Self::InvalidCursor => formatter.write_str("interaction cursor is invalid"),
            Self::StaleCursor => formatter.write_str("interaction cursor snapshot is stale"),
            Self::SnapshotConflict => formatter.write_str("interaction snapshot is inconsistent"),
        }
    }
}

impl std::error::Error for ChatInteractionProjectionError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CursorPayload {
    revision: i64,
    product_session_id: ProductSessionId,
    states: Vec<String>,
    offset: usize,
}

/// Control Plane aggregate for pending Chat input and approval state.
#[derive(Default)]
pub struct ChatInteractionProjectionLedger {
    snapshot: ChatInteractionProjectionSnapshot,
}

#[allow(clippy::missing_errors_doc)]
impl ChatInteractionProjectionLedger {
    /// Rebuilds one projection aggregate from its secret-safe event stream.
    pub fn restore(
        snapshot: ChatInteractionProjectionSnapshot,
    ) -> Result<Self, ChatInteractionProjectionError> {
        let ledger = Self { snapshot };
        ledger.rebuild()?;
        Ok(ledger)
    }

    /// Returns the exact state that must be persisted atomically.
    #[must_use]
    pub fn snapshot(&self) -> ChatInteractionProjectionSnapshot {
        self.snapshot.clone()
    }

    /// Records one input request and discards all private Worker envelope data.
    pub fn record_input_request(
        &mut self,
        request: &InputRequestMessage,
    ) -> Result<ProjectionWriteReceipt, ChatInteractionProjectionError> {
        validate_message_binding(
            &request.worker_session_id,
            &request.session_identity,
            &request.sent_at,
            &request.expires_at,
        )?;
        if request.prompt.trim().is_empty() {
            return Err(ChatInteractionProjectionError::InvalidField("prompt"));
        }
        validate_request_choices(request)?;
        let revision = Revision(self.next_revision()?);
        let projection = ChatInputInteractionProjection {
            allow_empty: request.allow_empty,
            binding: binding_from_request(
                &request.lease.job_id,
                &request.worker_session_id,
                &request.session_identity,
            ),
            expires_at: request.expires_at.clone(),
            input_request_id: request.input_request_id.clone(),
            kind: ChatInputInteractionProjectionKind::Input,
            mode: request.mode.clone(),
            options: request
                .choices
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|choice| ChatInteractionOptionProjection {
                    id: choice.id.clone(),
                    label: choice.label.clone(),
                    value: choice.value.clone(),
                })
                .collect(),
            prompt: request.prompt.clone(),
            revision: revision.clone(),
            state: "pending".to_owned(),
        };
        let source_sha256 = source_digest(request)?;
        if let Some(receipt) = self.input_duplicate_receipt(request, &source_sha256, &projection)? {
            return Ok(receipt);
        }
        if self
            .rebuild()?
            .inputs
            .contains_key(&request.input_request_id)
        {
            return Err(ChatInteractionProjectionError::InteractionIdentityConflict);
        }
        self.snapshot
            .events
            .push(ChatInteractionProjectionEvent::InputRecorded {
                source_message_id: request.message_id.clone(),
                source_sha256,
                projection: projection.clone(),
            });
        Ok(ProjectionWriteReceipt {
            status: ProjectionWriteStatus::Applied,
            revision,
            product_session_id: projection.binding.product_session_id,
        })
    }

    /// Records one approval request. `action.details` is intentionally never
    /// copied; the browser receives only the public action summary.
    pub fn record_approval_request(
        &mut self,
        request: &ApprovalRequestMessage,
    ) -> Result<ProjectionWriteReceipt, ChatInteractionProjectionError> {
        validate_message_binding(
            &request.worker_session_id,
            &request.session_identity,
            &request.sent_at,
            &request.expires_at,
        )?;
        if request.action.summary.trim().is_empty() {
            return Err(ChatInteractionProjectionError::InvalidField(
                "action.summary",
            ));
        }
        let revision = Revision(self.next_revision()?);
        let projection = ApprovalProjection {
            binding: binding_from_request(
                &request.lease.job_id,
                &request.worker_session_id,
                &request.session_identity,
            ),
            category: projection_category(&request.action.category),
            effective_decision_scope: ApprovalEffectiveDecisionScope::Once,
            expires_at: request.expires_at.clone(),
            id: request.approval_id.clone(),
            requested_at: request.sent_at.clone(),
            revision: revision.clone(),
            sanitized_detail: ApprovalSanitizedDetailProjection {
                kind: ApprovalSanitizedDetailProjectionKind::Unavailable,
                reason: if request.action.details.is_some() {
                    ApprovalSanitizedDetailUnavailableReason::EncodedPayloadRedacted
                } else {
                    ApprovalSanitizedDetailUnavailableReason::ProducerUnavailable
                },
            },
            state: "pending".to_owned(),
            subject: request.action.summary.clone(),
        };
        let source_sha256 = source_digest(request)?;
        if let Some(receipt) =
            self.approval_duplicate_receipt(request, &source_sha256, &projection)?
        {
            return Ok(receipt);
        }
        if self.rebuild()?.approvals.contains_key(&request.approval_id) {
            return Err(ChatInteractionProjectionError::InteractionIdentityConflict);
        }
        self.snapshot
            .events
            .push(ChatInteractionProjectionEvent::ApprovalRecorded {
                source_message_id: request.message_id.clone(),
                source_sha256,
                projection: projection.clone(),
            });
        Ok(ProjectionWriteReceipt {
            status: ProjectionWriteStatus::Applied,
            revision,
            product_session_id: projection.binding.product_session_id,
        })
    }

    /// Validates the exact projection-bound `input.respond` payload and records
    /// only its terminal state, never the submitted user value.
    pub fn apply_input_response(
        &mut self,
        expected_revision: &Revision,
        payload: &InputRespondPayload,
        now: &Instant,
    ) -> Result<Revision, ChatInteractionProjectionError> {
        let state = self.rebuild()?;
        let projection = state
            .inputs
            .get(&payload.input_request_id)
            .ok_or(ChatInteractionProjectionError::UnknownInteraction)?;
        validate_pending(
            projection.revision.clone(),
            &projection.state,
            &projection.expires_at,
            expected_revision,
            now,
        )?;
        validate_input_payload(payload, projection)?;
        let terminal = match payload.status.as_str() {
            "provided" => "provided",
            "cancelled" => "cancelled",
            _ => return Err(ChatInteractionProjectionError::UnsupportedState),
        };
        let revision = Revision(self.next_revision()?);
        self.snapshot
            .events
            .push(ChatInteractionProjectionEvent::InputResolved {
                input_request_id: payload.input_request_id.clone(),
                revision: revision.clone(),
                state: terminal.to_owned(),
            });
        Ok(revision)
    }

    /// Validates the complete projection-bound `approval.decide` payload and
    /// records only its terminal state, never the operator reason.
    pub fn apply_approval_decision(
        &mut self,
        expected_revision: &Revision,
        payload: &ApprovalDecidePayload,
        now: &Instant,
    ) -> Result<Revision, ChatInteractionProjectionError> {
        let state = self.rebuild()?;
        let projection = state
            .approvals
            .get(&payload.approval_id)
            .ok_or(ChatInteractionProjectionError::UnknownInteraction)?;
        validate_pending(
            projection.revision.clone(),
            &projection.state,
            &projection.expires_at,
            expected_revision,
            now,
        )?;
        validate_binding(&payload.binding, &projection.binding)?;
        let terminal = match payload.decision.as_str() {
            "approve" => "approved",
            "reject" => "rejected",
            _ => return Err(ChatInteractionProjectionError::UnsupportedState),
        };
        let revision = Revision(self.next_revision()?);
        self.snapshot
            .events
            .push(ChatInteractionProjectionEvent::ApprovalResolved {
                approval_id: payload.approval_id.clone(),
                revision: revision.clone(),
                state: terminal.to_owned(),
            });
        Ok(revision)
    }

    /// Executes the canonical `session.interactions.list` HTTP query.
    pub fn query(
        &self,
        request: &ChatInteractionListQuery,
        now: &Instant,
    ) -> Result<ChatInteractionListResultResponse, ChatInteractionProjectionError> {
        if !(1..=200).contains(&request.page.limit) {
            return Err(ChatInteractionProjectionError::InvalidPage);
        }
        let normalized_states = normalize_interaction_states(&request.parameters.states)?;
        let state = self.rebuild()?;
        let offset = decode_cursor(
            request.page.cursor.as_ref(),
            state.revision,
            &request.parameters.product_session_id,
            &normalized_states,
        )?;
        let mut items = Vec::new();
        for projection in state.inputs.values() {
            let mut projection = projection.clone();
            expire_if_needed(&mut projection.state, &projection.expires_at, now);
            if projection.binding.product_session_id == request.parameters.product_session_id
                && interaction_state_matches(&normalized_states, &projection.state)
            {
                items.push(ChatInteractionProjection::ChatInputInteractionProjection(
                    projection,
                ));
            }
        }
        for approval in state.approvals.values() {
            let mut approval = approval.clone();
            expire_if_needed(&mut approval.state, &approval.expires_at, now);
            if approval.binding.product_session_id == request.parameters.product_session_id
                && interaction_state_matches(&normalized_states, &approval.state)
            {
                items.push(
                    ChatInteractionProjection::ChatApprovalInteractionProjection(
                        ChatApprovalInteractionProjection {
                            approval,
                            kind: ChatApprovalInteractionProjectionKind::Approval,
                        },
                    ),
                );
            }
        }
        items.sort_by_key(interaction_sort_key);
        if offset > items.len() {
            return Err(ChatInteractionProjectionError::InvalidCursor);
        }
        let limit = usize::try_from(request.page.limit)
            .map_err(|_| ChatInteractionProjectionError::InvalidPage)?;
        let end = offset.saturating_add(limit).min(items.len());
        let has_more = end < items.len();
        let next_cursor = if has_more {
            Some(encode_cursor(&CursorPayload {
                revision: state.revision,
                product_session_id: request.parameters.product_session_id.clone(),
                states: normalized_states,
                offset: end,
            })?)
        } else {
            None
        };
        Ok(ChatInteractionListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor,
            },
            query: ChatInteractionListResultResponseQuery::SessionInteractionsList,
            request_id: request.request_id.clone(),
            result: ChatInteractionPage {
                items: items[offset..end].to_vec(),
                kind: ChatInteractionPageKind::ChatInteractionPage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Executes the canonical `approval.get` HTTP query.
    pub fn approval_get(
        &self,
        request: &ApprovalGetQuery,
        now: &Instant,
    ) -> Result<ApprovalGetResultResponse, ChatInteractionProjectionError> {
        validate_single_item_page(request.page.limit, request.page.cursor.as_ref())?;
        let state = self.rebuild()?;
        let mut approval = state
            .approvals
            .get(&request.parameters.approval_id)
            .cloned()
            .ok_or(ChatInteractionProjectionError::UnknownInteraction)?;
        expire_if_needed(&mut approval.state, &approval.expires_at, now);
        Ok(ApprovalGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: ApprovalGetResultResponseQuery::ApprovalGet,
            request_id: request.request_id.clone(),
            result: approval,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Executes the canonical scope-wide `approval.list` HTTP query.
    pub fn approval_list(
        &self,
        request: &ApprovalListQuery,
        now: &Instant,
    ) -> Result<ApprovalListResultResponse, ChatInteractionProjectionError> {
        if !(1..=200).contains(&request.page.limit) {
            return Err(ChatInteractionProjectionError::InvalidPage);
        }
        let states = normalize_approval_states(&request.parameters.states)?;
        let state = self.rebuild()?;
        let offset = decode_approval_cursor(request.page.cursor.as_ref(), state.revision, &states)?;
        let mut items = state
            .approvals
            .values()
            .cloned()
            .map(|mut approval| {
                expire_if_needed(&mut approval.state, &approval.expires_at, now);
                approval
            })
            .filter(|approval| states.iter().any(|value| value == &approval.state))
            .collect::<Vec<_>>();
        items.sort_by_key(|approval| (approval.revision.0, approval.id.0.clone()));
        if offset > items.len() {
            return Err(ChatInteractionProjectionError::InvalidCursor);
        }
        let limit = usize::try_from(request.page.limit)
            .map_err(|_| ChatInteractionProjectionError::InvalidPage)?;
        let end = offset.saturating_add(limit).min(items.len());
        let has_more = end < items.len();
        let next_cursor = has_more
            .then(|| {
                encode_cursor(&ApprovalCursorPayload {
                    revision: state.revision,
                    states: states.clone(),
                    offset: end,
                })
            })
            .transpose()?;
        Ok(ApprovalListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor,
            },
            query: ApprovalListResultResponseQuery::ApprovalList,
            request_id: request.request_id.clone(),
            result: ApprovalPage {
                items: items[offset..end].to_vec(),
                kind: ApprovalPageKind::ApprovalPage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Returns one current approval projection for a durable command result.
    pub fn approval(
        &self,
        approval_id: &ApprovalId,
        now: &Instant,
    ) -> Result<Option<ApprovalProjection>, ChatInteractionProjectionError> {
        let mut approval = self.rebuild()?.approvals.get(approval_id).cloned();
        if let Some(value) = &mut approval {
            expire_if_needed(&mut value.state, &value.expires_at, now);
        }
        Ok(approval)
    }

    /// Builds the secret-free WebSocket invalidation for the two authoritative
    /// HTTP snapshots used by Chat.
    pub fn invalidation(
        &self,
        product_session_id: ProductSessionId,
    ) -> Result<ControlPlaneWebSocketChatInteractionsInvalidatedEvent, ChatInteractionProjectionError>
    {
        let revision = Revision(self.rebuild()?.revision);
        Ok(ControlPlaneWebSocketChatInteractionsInvalidatedEvent {
            product_session_id,
            reload_queries: (
                ControlPlaneWebSocketChatInteractionListReloadQuery::SessionInteractionsList,
                ControlPlaneWebSocketApprovalListReloadQuery::ApprovalList,
            ),
            revision,
            type_value: ControlPlaneWebSocketChatInteractionsInvalidatedEventTypeValue::ChatInteractionsInvalidatedV1,
        })
    }

    fn next_revision(&self) -> Result<i64, ChatInteractionProjectionError> {
        self.rebuild()?
            .revision
            .checked_add(1)
            .ok_or(ChatInteractionProjectionError::SnapshotConflict)
    }

    fn input_duplicate_receipt(
        &self,
        request: &InputRequestMessage,
        source_sha256: &Sha256Digest,
        candidate: &ChatInputInteractionProjection,
    ) -> Result<Option<ProjectionWriteReceipt>, ChatInteractionProjectionError> {
        for event in &self.snapshot.events {
            match event {
                ChatInteractionProjectionEvent::InputRecorded {
                    source_message_id,
                    source_sha256: recorded_sha256,
                    projection,
                } if source_message_id == &request.message_id => {
                    let mut candidate = candidate.clone();
                    candidate.revision = projection.revision.clone();
                    if recorded_sha256 != source_sha256 || &candidate != projection {
                        return Err(ChatInteractionProjectionError::SourceMessageConflict);
                    }
                    return Ok(Some(ProjectionWriteReceipt {
                        status: ProjectionWriteStatus::Duplicate,
                        revision: projection.revision.clone(),
                        product_session_id: projection.binding.product_session_id.clone(),
                    }));
                }
                ChatInteractionProjectionEvent::ApprovalRecorded {
                    source_message_id, ..
                } if source_message_id == &request.message_id => {
                    return Err(ChatInteractionProjectionError::SourceMessageConflict);
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn approval_duplicate_receipt(
        &self,
        request: &ApprovalRequestMessage,
        source_sha256: &Sha256Digest,
        candidate: &ApprovalProjection,
    ) -> Result<Option<ProjectionWriteReceipt>, ChatInteractionProjectionError> {
        for event in &self.snapshot.events {
            match event {
                ChatInteractionProjectionEvent::ApprovalRecorded {
                    source_message_id,
                    source_sha256: recorded_sha256,
                    projection,
                } if source_message_id == &request.message_id => {
                    let mut candidate = candidate.clone();
                    candidate.revision = projection.revision.clone();
                    if recorded_sha256 != source_sha256 || &candidate != projection {
                        return Err(ChatInteractionProjectionError::SourceMessageConflict);
                    }
                    return Ok(Some(ProjectionWriteReceipt {
                        status: ProjectionWriteStatus::Duplicate,
                        revision: projection.revision.clone(),
                        product_session_id: projection.binding.product_session_id.clone(),
                    }));
                }
                ChatInteractionProjectionEvent::InputRecorded {
                    source_message_id, ..
                } if source_message_id == &request.message_id => {
                    return Err(ChatInteractionProjectionError::SourceMessageConflict);
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn rebuild(&self) -> Result<RebuiltProjection, ChatInteractionProjectionError> {
        let mut result = RebuiltProjection::default();
        let mut source_messages = HashMap::new();
        for event in &self.snapshot.events {
            match event {
                ChatInteractionProjectionEvent::InputRecorded {
                    source_message_id,
                    source_sha256: _,
                    projection,
                } => {
                    register_source(&mut source_messages, source_message_id, "input")?;
                    validate_projection_choices(&projection.mode, &projection.options)
                        .map_err(|_| ChatInteractionProjectionError::SnapshotConflict)?;
                    if projection.revision.0 <= result.revision
                        || result
                            .inputs
                            .insert(projection.input_request_id.clone(), projection.clone())
                            .is_some()
                    {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    result.revision = projection.revision.0;
                }
                ChatInteractionProjectionEvent::ApprovalRecorded {
                    source_message_id,
                    source_sha256: _,
                    projection,
                } => {
                    register_source(&mut source_messages, source_message_id, "approval")?;
                    if projection.revision.0 <= result.revision
                        || result
                            .approvals
                            .insert(projection.id.clone(), projection.clone())
                            .is_some()
                    {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    result.revision = projection.revision.0;
                }
                ChatInteractionProjectionEvent::InputResolved {
                    input_request_id,
                    revision,
                    state,
                } => {
                    if revision.0 <= result.revision {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    let projection = result
                        .inputs
                        .get_mut(input_request_id)
                        .ok_or(ChatInteractionProjectionError::SnapshotConflict)?;
                    if projection.state != "pending"
                        || !matches!(state.as_str(), "provided" | "cancelled")
                    {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    projection.state.clone_from(state);
                    projection.revision = revision.clone();
                    result.revision = revision.0;
                }
                ChatInteractionProjectionEvent::ApprovalResolved {
                    approval_id,
                    revision,
                    state,
                } => {
                    if revision.0 <= result.revision {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    let projection = result
                        .approvals
                        .get_mut(approval_id)
                        .ok_or(ChatInteractionProjectionError::SnapshotConflict)?;
                    if projection.state != "pending"
                        || !matches!(state.as_str(), "approved" | "rejected" | "cancelled")
                    {
                        return Err(ChatInteractionProjectionError::SnapshotConflict);
                    }
                    projection.state.clone_from(state);
                    projection.revision = revision.clone();
                    result.revision = revision.0;
                }
            }
        }
        Ok(result)
    }
}

#[derive(Default)]
struct RebuiltProjection {
    revision: i64,
    inputs: HashMap<InputRequestId, ChatInputInteractionProjection>,
    approvals: HashMap<ApprovalId, ApprovalProjection>,
}

fn register_source(
    sources: &mut HashMap<ExecutionMessageId, &'static str>,
    message_id: &ExecutionMessageId,
    kind: &'static str,
) -> Result<(), ChatInteractionProjectionError> {
    if sources.insert(message_id.clone(), kind).is_some() {
        return Err(ChatInteractionProjectionError::SnapshotConflict);
    }
    Ok(())
}

fn source_digest<T: Serialize>(value: &T) -> Result<Sha256Digest, ChatInteractionProjectionError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ChatInteractionProjectionError::InvalidField("sourceMessage"))?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn validate_message_binding(
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &winwincode_domain::SessionIdentity,
    sent_at: &Instant,
    expires_at: &Instant,
) -> Result<(), ChatInteractionProjectionError> {
    if worker_session_id != &session_identity.worker_session_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "workerSessionId",
        ));
    }
    if expires_at.0 <= sent_at.0 {
        return Err(ChatInteractionProjectionError::InvalidField("expiresAt"));
    }
    Ok(())
}

fn binding_from_request(
    execution_job_id: &winwincode_domain::ExecutionJobId,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &winwincode_domain::SessionIdentity,
) -> ChatInteractionBindingProjection {
    ChatInteractionBindingProjection {
        execution_job_id: execution_job_id.clone(),
        product_session_id: session_identity.product_session_id.clone(),
        session_identity: session_identity.clone(),
        worker_session_id: worker_session_id.clone(),
    }
}

fn projection_category(category: &ApprovalActionCategory) -> ApprovalProjectionCategory {
    match category {
        ApprovalActionCategory::FilesystemWrite => ApprovalProjectionCategory::FilesystemWrite,
        ApprovalActionCategory::Mcp => ApprovalProjectionCategory::Mcp,
        ApprovalActionCategory::Network => ApprovalProjectionCategory::Network,
        ApprovalActionCategory::Shell => ApprovalProjectionCategory::Shell,
    }
}

fn validate_pending(
    actual_revision: Revision,
    state: &str,
    expires_at: &Instant,
    expected_revision: &Revision,
    now: &Instant,
) -> Result<(), ChatInteractionProjectionError> {
    if state != "pending" {
        return Err(ChatInteractionProjectionError::StateConflict);
    }
    if &actual_revision != expected_revision {
        return Err(ChatInteractionProjectionError::RevisionConflict {
            expected: expected_revision.clone(),
            actual: actual_revision,
        });
    }
    if now.0 >= expires_at.0 {
        return Err(ChatInteractionProjectionError::Expired);
    }
    Ok(())
}

fn validate_input_payload(
    payload: &InputRespondPayload,
    projection: &ChatInputInteractionProjection,
) -> Result<(), ChatInteractionProjectionError> {
    let binding = &projection.binding;
    if payload.product_session_id != binding.product_session_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "productSessionId",
        ));
    }
    if payload.execution_job_id != binding.execution_job_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "executionJobId",
        ));
    }
    if payload.worker_session_id != binding.worker_session_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "workerSessionId",
        ));
    }
    if payload.session_identity != binding.session_identity {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "sessionIdentity",
        ));
    }
    match payload.status.as_str() {
        "provided" => {
            let value = payload
                .value
                .as_ref()
                .ok_or(ChatInteractionProjectionError::InvalidField("value"))?;
            if value.mode != projection.mode {
                return Err(ChatInteractionProjectionError::InvalidField("value.mode"));
            }
            match &projection.mode {
                InteractiveInputMode::Text => {
                    if !projection.allow_empty && value.value.is_empty() {
                        return Err(ChatInteractionProjectionError::InvalidField("value.value"));
                    }
                }
                InteractiveInputMode::Confirmation | InteractiveInputMode::SingleChoice => {
                    if !projection
                        .options
                        .iter()
                        .any(|option| option.value == value.value)
                    {
                        return Err(ChatInteractionProjectionError::InvalidField("value.value"));
                    }
                }
            }
        }
        "cancelled" if payload.value.is_some() => {
            return Err(ChatInteractionProjectionError::InvalidField("value"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_request_choices(
    request: &InputRequestMessage,
) -> Result<(), ChatInteractionProjectionError> {
    let choices = request.choices.as_deref().unwrap_or_default();
    let projected = choices
        .iter()
        .map(|choice| ChatInteractionOptionProjection {
            id: choice.id.clone(),
            label: choice.label.clone(),
            value: choice.value.clone(),
        })
        .collect::<Vec<_>>();
    validate_projection_choices(&request.mode, &projected)
}

fn validate_projection_choices(
    mode: &InteractiveInputMode,
    choices: &[ChatInteractionOptionProjection],
) -> Result<(), ChatInteractionProjectionError> {
    if matches!(mode, InteractiveInputMode::Text) && !choices.is_empty()
        || !matches!(mode, InteractiveInputMode::Text) && choices.is_empty()
    {
        return Err(ChatInteractionProjectionError::InvalidField("choices"));
    }
    let mut ids = HashSet::with_capacity(choices.len());
    for choice in choices {
        if choice.label.trim().is_empty()
            || choice.value.trim().is_empty()
            || !canonical_choice_id(&choice.id.0)
            || !ids.insert(choice.id.clone())
        {
            return Err(ChatInteractionProjectionError::InvalidField("choices"));
        }
    }
    Ok(())
}

fn canonical_choice_id(value: &str) -> bool {
    value.strip_prefix("ich_").is_some_and(|identifier| {
        identifier.len() == 26
            && identifier.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    })
}

fn validate_binding(
    candidate: &ChatInteractionBindingProjection,
    actual: &ChatInteractionBindingProjection,
) -> Result<(), ChatInteractionProjectionError> {
    if candidate != actual {
        return Err(ChatInteractionProjectionError::BindingMismatch("binding"));
    }
    if candidate.product_session_id != candidate.session_identity.product_session_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "sessionIdentity.productSessionId",
        ));
    }
    if candidate.worker_session_id != candidate.session_identity.worker_session_id {
        return Err(ChatInteractionProjectionError::BindingMismatch(
            "sessionIdentity.workerSessionId",
        ));
    }
    Ok(())
}

fn expire_if_needed(state: &mut String, expires_at: &Instant, now: &Instant) {
    if state == "pending" && now.0 >= expires_at.0 {
        "expired".clone_into(state);
    }
}

fn normalize_interaction_states(
    states: &[String],
) -> Result<Vec<String>, ChatInteractionProjectionError> {
    let mut result = if states.is_empty() {
        vec!["pending".to_owned()]
    } else {
        states.to_vec()
    };
    if result.iter().any(|state| {
        !matches!(
            state.as_str(),
            "pending" | "resolved" | "expired" | "cancelled"
        )
    }) {
        return Err(ChatInteractionProjectionError::UnsupportedState);
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn interaction_state_matches(filters: &[String], actual: &str) -> bool {
    filters.iter().any(|filter| match filter.as_str() {
        "resolved" => matches!(actual, "provided" | "approved" | "rejected"),
        value => value == actual,
    })
}

fn normalize_approval_states(
    states: &[String],
) -> Result<Vec<String>, ChatInteractionProjectionError> {
    let mut result = if states.is_empty() {
        vec!["pending".to_owned()]
    } else {
        states.to_vec()
    };
    if result.iter().any(|state| {
        !matches!(
            state.as_str(),
            "pending" | "approved" | "rejected" | "expired"
        )
    }) {
        return Err(ChatInteractionProjectionError::UnsupportedState);
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn validate_single_item_page(
    limit: i64,
    cursor: Option<&OpaqueCursor>,
) -> Result<(), ChatInteractionProjectionError> {
    if !(1..=200).contains(&limit) || cursor.is_some() {
        return Err(ChatInteractionProjectionError::InvalidPage);
    }
    Ok(())
}

fn interaction_sort_key(interaction: &ChatInteractionProjection) -> (i64, String) {
    match interaction {
        ChatInteractionProjection::ChatInputInteractionProjection(value) => {
            (value.revision.0, value.input_request_id.0.clone())
        }
        ChatInteractionProjection::ChatApprovalInteractionProjection(value) => {
            (value.approval.revision.0, value.approval.id.0.clone())
        }
    }
}

fn encode_cursor<T: Serialize>(
    payload: &T,
) -> Result<OpaqueCursor, ChatInteractionProjectionError> {
    serde_json::to_vec(payload)
        .map(|bytes| OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
        .map_err(|_| ChatInteractionProjectionError::InvalidCursor)
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    revision: i64,
    product_session_id: &ProductSessionId,
    states: &[String],
) -> Result<usize, ChatInteractionProjectionError> {
    let Some(cursor) = cursor else { return Ok(0) };
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| ChatInteractionProjectionError::InvalidCursor)?;
    let payload: CursorPayload = serde_json::from_slice(&bytes)
        .map_err(|_| ChatInteractionProjectionError::InvalidCursor)?;
    if payload.revision != revision {
        return Err(ChatInteractionProjectionError::StaleCursor);
    }
    if &payload.product_session_id != product_session_id || payload.states != states {
        return Err(ChatInteractionProjectionError::InvalidCursor);
    }
    Ok(payload.offset)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ApprovalCursorPayload {
    revision: i64,
    states: Vec<String>,
    offset: usize,
}

fn decode_approval_cursor(
    cursor: Option<&OpaqueCursor>,
    revision: i64,
    states: &[String],
) -> Result<usize, ChatInteractionProjectionError> {
    let Some(cursor) = cursor else { return Ok(0) };
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| ChatInteractionProjectionError::InvalidCursor)?;
    let payload: ApprovalCursorPayload = serde_json::from_slice(&bytes)
        .map_err(|_| ChatInteractionProjectionError::InvalidCursor)?;
    if payload.revision != revision {
        return Err(ChatInteractionProjectionError::StaleCursor);
    }
    if payload.states != states {
        return Err(ChatInteractionProjectionError::InvalidCursor);
    }
    Ok(payload.offset)
}
