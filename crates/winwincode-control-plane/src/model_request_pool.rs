// SPDX-License-Identifier: Apache-2.0

//! Route-isolated model request admission and bounded stream backpressure.
//!
//! Each Provider/model/Credential/organization/project route owns independent
//! concurrency slots and a FIFO waiting queue. The pool keeps only bounded,
//! opaque stream frames in memory and releases an active slot only when its
//! stream reaches a terminal outcome or is explicitly cancelled.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{ModelRoute, RepositoryScopeKind};
use winwincode_domain::{
    CredentialReferenceId, ModelExchangeId, OrganizationId, ProjectId, RequestId,
};

use crate::{
    FrozenModelRouteAuthority, ModelSettingsTarget, ProviderGatewayIdentity,
    ProviderGatewayOpenReceipt,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DURABLE_POOL_SCHEMA: &str = "winwincode.model-request-pool-exchange.v1";
const DURABLE_POOL_AUTHORITY_SCHEMA: &str = "winwincode.model-request-pool-authority.v1";

/// Immutable partition key derived from the trusted Provider Gateway identity
/// and its resolved route.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelRequestRouteKey {
    provider: String,
    model: String,
    credential_reference: CredentialReferenceId,
    organization: OrganizationId,
    project: ProjectId,
}

impl ModelRequestRouteKey {
    /// Builds the canonical pool partition from a trusted Gateway identity and route.
    ///
    /// # Errors
    ///
    /// Rejects non-ProductSession identities and malformed route fields.
    pub fn from_gateway(
        identity: &ProviderGatewayIdentity,
        route: &ModelRoute,
    ) -> Result<Self, ModelRequestPoolError> {
        Self::from_target(identity.target(), route)
    }

    pub(crate) fn from_target(
        target: &ModelSettingsTarget,
        route: &ModelRoute,
    ) -> Result<Self, ModelRequestPoolError> {
        let ModelSettingsTarget::ProductSession {
            repository_scope, ..
        } = target
        else {
            return Err(pool_error(
                ModelRequestPoolErrorCode::IdentityMismatch,
                "model request pool requires a ProductSession Gateway identity",
            ));
        };
        if repository_scope.kind != RepositoryScopeKind::Repository {
            return Err(pool_error(
                ModelRequestPoolErrorCode::IdentityMismatch,
                "model request pool requires a canonical repository scope",
            ));
        }
        validate_token(&route.provider_id, 128, "providerId")?;
        validate_token(&route.model_id, 200, "modelId")?;
        validate_prefixed_id(
            &route.credential_reference_id.0,
            "crd_",
            "credentialReferenceId",
        )?;
        validate_prefixed_id(
            &repository_scope.organization_id.0,
            "org_",
            "organizationId",
        )?;
        validate_prefixed_id(&repository_scope.project_id.0, "prj_", "projectId")?;
        Ok(Self {
            provider: route.provider_id.clone(),
            model: route.model_id.clone(),
            credential_reference: route.credential_reference_id.clone(),
            organization: repository_scope.organization_id.clone(),
            project: repository_scope.project_id.clone(),
        })
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference
    }

    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        &self.organization
    }

    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project
    }

    fn sort_tuple(&self) -> (&str, &str, &str, &str, &str) {
        (
            &self.organization.0,
            &self.project.0,
            &self.provider,
            &self.model,
            &self.credential_reference.0,
        )
    }
}

impl Ord for ModelRequestRouteKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_tuple().cmp(&other.sort_tuple())
    }
}

impl PartialOrd for ModelRequestRouteKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One pool admission constructed from a successful Provider Gateway open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestAdmission {
    pub route: ModelRequestRouteKey,
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
}

impl ModelRequestAdmission {
    /// Joins trusted Gateway identity and route facts to one exchange request.
    ///
    /// # Errors
    ///
    /// Rejects an identity/route mismatch or malformed exchange identifiers.
    pub fn from_gateway_route(
        identity: &ProviderGatewayIdentity,
        route: &ModelRoute,
        model_exchange_id: ModelExchangeId,
        request_id: RequestId,
    ) -> Result<Self, ModelRequestPoolError> {
        validate_prefixed_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
        validate_prefixed_id(&request_id.0, "req_", "requestId")?;
        Ok(Self {
            route: ModelRequestRouteKey::from_gateway(identity, route)?,
            model_exchange_id,
            request_id,
        })
    }

    /// Copies only secret-free route and exchange identities from the Gateway result.
    ///
    /// # Errors
    ///
    /// Rejects an identity/route mismatch or malformed exchange identifiers.
    pub fn from_gateway_open(
        identity: &ProviderGatewayIdentity,
        receipt: &ProviderGatewayOpenReceipt,
    ) -> Result<Self, ModelRequestPoolError> {
        Self::from_gateway_route(
            identity,
            &receipt.route,
            receipt.model_exchange_id.clone(),
            receipt.request_id.clone(),
        )
    }

    /// Rebuilds one pool admission from an already validated durable route
    /// authority without consulting current Settings or Catalog revisions.
    ///
    /// # Errors
    ///
    /// Rejects a corrupt frozen authority or malformed request identities.
    pub fn from_frozen_authority(
        authority: &FrozenModelRouteAuthority,
        model_exchange_id: ModelExchangeId,
        request_id: RequestId,
    ) -> Result<Self, ModelRequestPoolError> {
        authority.validate_fingerprint().map_err(|_| {
            pool_error(
                ModelRequestPoolErrorCode::IdentityMismatch,
                "frozen model route authority is invalid",
            )
        })?;
        validate_prefixed_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
        validate_prefixed_id(&request_id.0, "req_", "requestId")?;
        Ok(Self {
            route: authority.route_key().clone(),
            model_exchange_id,
            request_id,
        })
    }
}

/// Fixed memory and concurrency bounds applied independently to every route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelRequestPoolConfig {
    pub max_routes: usize,
    pub max_active_per_route: usize,
    pub max_waiting_per_route: usize,
    pub max_exchange_records_per_route: usize,
    pub max_buffered_frames_per_stream: usize,
    pub max_buffered_bytes_per_stream: usize,
    pub resume_buffered_frames_per_stream: usize,
    pub resume_buffered_bytes_per_stream: usize,
}

impl ModelRequestPoolConfig {
    fn validate(self) -> Result<Self, ModelRequestPoolError> {
        let minimum_records = self
            .max_active_per_route
            .checked_add(self.max_waiting_per_route)
            .ok_or_else(|| invalid_config("model request record limit overflowed"))?;
        if self.max_routes == 0
            || self.max_active_per_route == 0
            || self.max_exchange_records_per_route < minimum_records
            || self.max_buffered_frames_per_stream == 0
            || self.max_buffered_bytes_per_stream == 0
            || self.resume_buffered_frames_per_stream >= self.max_buffered_frames_per_stream
            || self.resume_buffered_bytes_per_stream >= self.max_buffered_bytes_per_stream
        {
            return Err(invalid_config("model request pool limits are invalid"));
        }
        Ok(self)
    }
}

/// Current lifecycle state for one exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestState {
    Queued,
    Active,
    Succeeded,
    Failed,
    Cancelled,
}

impl ModelRequestState {
    const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Provider-neutral terminal outcome used to release one active slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestTerminalOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl ModelRequestTerminalOutcome {
    const fn state(self) -> ModelRequestState {
        match self {
            Self::Succeeded => ModelRequestState::Succeeded,
            Self::Failed => ModelRequestState::Failed,
            Self::Cancelled => ModelRequestState::Cancelled,
        }
    }
}

/// Result category from a pool admission attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestAdmissionStatus {
    Started,
    Queued,
    Duplicate,
}

/// Admission result including the stable FIFO position when queued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestAdmissionReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub state: ModelRequestState,
    pub status: ModelRequestAdmissionStatus,
    pub queue_position: Option<usize>,
}

/// Opaque, bounded stream frame retained until acknowledged.
#[derive(Clone, Eq, PartialEq)]
pub struct ModelStreamFrame {
    sequence: u64,
    payload: Vec<u8>,
    terminal_outcome: Option<ModelRequestTerminalOutcome>,
}

impl ModelStreamFrame {
    #[must_use]
    pub fn data(sequence: u64, payload: Vec<u8>) -> Self {
        Self {
            sequence,
            payload,
            terminal_outcome: None,
        }
    }

    #[must_use]
    pub fn terminal(sequence: u64, payload: Vec<u8>, outcome: ModelRequestTerminalOutcome) -> Self {
        Self {
            sequence,
            payload,
            terminal_outcome: Some(outcome),
        }
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    #[must_use]
    pub const fn terminal_outcome(&self) -> Option<ModelRequestTerminalOutcome> {
        self.terminal_outcome
    }
}

impl fmt::Debug for ModelStreamFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelStreamFrame")
            .field("sequence", &self.sequence)
            .field("payload_bytes", &self.payload.len())
            .field("terminal_outcome", &self.terminal_outcome)
            .finish()
    }
}

/// Result of attempting to append one model stream frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFrameWriteStatus {
    Accepted,
    Duplicate,
    Backpressured,
}

/// Provider read decision derived only from this stream's bounded buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelStreamReadControl {
    Read,
    Paused,
    Closed,
}

/// Stream-frame result with bounded-buffer and newly granted exchange facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFrameWriteReceipt {
    pub status: ModelFrameWriteStatus,
    pub state: ModelRequestState,
    pub highest_sequence: u64,
    pub buffered_frames: usize,
    pub buffered_bytes: usize,
    pub read_control: ModelStreamReadControl,
    pub granted_exchange_id: Option<ModelExchangeId>,
}

/// Result of acknowledging buffered stream frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFrameAckReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub acknowledged_sequence: u64,
    pub buffered_frames: usize,
    pub buffered_bytes: usize,
    pub read_control: ModelStreamReadControl,
    pub replayed: bool,
}

/// Idempotent terminal result and the FIFO request which acquired its slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestTerminalReceipt {
    pub model_exchange_id: ModelExchangeId,
    pub outcome: ModelRequestTerminalOutcome,
    pub replayed: bool,
    pub granted_exchange_id: Option<ModelExchangeId>,
}

/// Read-only reconnect projection. Reading it performs no lifecycle transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestSnapshot {
    pub route: ModelRequestRouteKey,
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub state: ModelRequestState,
    pub queue_position: Option<usize>,
    pub next_sequence: u64,
    pub acknowledged_sequence: u64,
    pub buffered_frames: usize,
    pub buffered_bytes: usize,
    pub read_control: ModelStreamReadControl,
    pub terminal_outcome: Option<ModelRequestTerminalOutcome>,
}

/// Read-only capacity projection for one isolated route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoutePoolSnapshot {
    pub route: ModelRequestRouteKey,
    pub active: usize,
    pub waiting: usize,
    pub retained_terminal: usize,
    pub records: usize,
}

/// Stable pool failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestPoolErrorCode {
    InvalidConfig,
    InvalidInput,
    IdentityMismatch,
    RouteLimit,
    QueueFull,
    RecordLimit,
    ExchangeConflict,
    NotFound,
    InvalidState,
    SequenceGap,
    FrameConflict,
    TerminalConflict,
}

/// Bounded model request pool failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequestPoolError {
    code: ModelRequestPoolErrorCode,
    message: &'static str,
}

impl ModelRequestPoolError {
    #[must_use]
    pub const fn code(&self) -> ModelRequestPoolErrorCode {
        self.code
    }
}

impl fmt::Display for ModelRequestPoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelRequestPoolError {}

#[derive(Clone, Debug)]
struct ExchangeRecord {
    model_exchange_id: ModelExchangeId,
    request_id: RequestId,
    state: ModelRequestState,
    next_sequence: u64,
    acknowledged_sequence: u64,
    buffered_bytes: usize,
    provider_read_paused: bool,
    frames: BTreeMap<u64, ModelStreamFrame>,
    terminal_outcome: Option<ModelRequestTerminalOutcome>,
    terminal_frame: Option<FrameFingerprint>,
}

impl ExchangeRecord {
    fn new(admission: &ModelRequestAdmission, state: ModelRequestState) -> Self {
        Self {
            model_exchange_id: admission.model_exchange_id.clone(),
            request_id: admission.request_id.clone(),
            state,
            next_sequence: 1,
            acknowledged_sequence: 0,
            buffered_bytes: 0,
            provider_read_paused: false,
            frames: BTreeMap::new(),
            terminal_outcome: None,
            terminal_frame: None,
        }
    }

    fn highest_sequence(&self) -> u64 {
        self.next_sequence.saturating_sub(1)
    }

    const fn read_control(&self) -> ModelStreamReadControl {
        if self.state.terminal() {
            ModelStreamReadControl::Closed
        } else if self.provider_read_paused {
            ModelStreamReadControl::Paused
        } else {
            ModelStreamReadControl::Read
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameFingerprint {
    sequence: u64,
    digest: [u8; 32],
    outcome: ModelRequestTerminalOutcome,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePoolExchange {
    schema: String,
    route_fingerprint: String,
    model_exchange_id: ModelExchangeId,
    request_id: RequestId,
    state: String,
    next_sequence: u64,
    acknowledged_sequence: u64,
    provider_read_paused: bool,
    frames: Vec<DurablePoolFrame>,
    terminal_outcome: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePoolFrame {
    sequence: u64,
    payload_base64: String,
    terminal_outcome: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePoolAuthority {
    schema: String,
    config: DurablePoolConfig,
    routes: Vec<DurablePoolRoute>,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePoolConfig {
    max_active_per_route: usize,
    max_waiting_per_route: usize,
    max_routes: usize,
    max_exchange_records_per_route: usize,
    max_buffered_frames_per_stream: usize,
    max_buffered_bytes_per_stream: usize,
    resume_buffered_frames_per_stream: usize,
    resume_buffered_bytes_per_stream: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePoolRoute {
    provider: String,
    model: String,
    credential_reference_id: CredentialReferenceId,
    organization_id: OrganizationId,
    project_id: ProjectId,
    waiting: Vec<ModelExchangeId>,
    exchanges: Vec<DurablePoolExchange>,
}

#[derive(Clone, Default)]
struct RoutePartition {
    records: BTreeMap<String, ExchangeRecord>,
    waiting: VecDeque<String>,
}

type RestoredPoolAuthority = (
    BTreeMap<ModelRequestRouteKey, RoutePartition>,
    HashMap<String, ModelRequestRouteKey>,
);

impl RoutePartition {
    fn active_count(&self) -> usize {
        self.records
            .values()
            .filter(|record| record.state == ModelRequestState::Active)
            .count()
    }

    fn terminal_count(&self) -> usize {
        self.records
            .values()
            .filter(|record| record.state.terminal())
            .count()
    }

    fn queue_position(&self, model_exchange_id: &str) -> Option<usize> {
        self.waiting
            .iter()
            .position(|queued| queued == model_exchange_id)
            .map(|index| index + 1)
    }

    fn grant_next(&mut self) -> Option<ModelExchangeId> {
        while let Some(exchange_id) = self.waiting.pop_front() {
            let Some(record) = self.records.get_mut(&exchange_id) else {
                continue;
            };
            if record.state == ModelRequestState::Queued {
                record.state = ModelRequestState::Active;
                return Some(record.model_exchange_id.clone());
            }
        }
        None
    }
}

/// In-memory, deterministic request pool. Durable Gateway settlement and
/// Provider streaming remain owned by their existing modules.
#[derive(Clone)]
pub struct ModelRequestPool {
    config: ModelRequestPoolConfig,
    routes: BTreeMap<ModelRequestRouteKey, RoutePartition>,
    exchange_routes: HashMap<String, ModelRequestRouteKey>,
}

#[allow(clippy::missing_errors_doc)]
impl ModelRequestPool {
    pub fn new(config: ModelRequestPoolConfig) -> Result<Self, ModelRequestPoolError> {
        Ok(Self {
            config: config.validate()?,
            routes: BTreeMap::new(),
            exchange_routes: HashMap::new(),
        })
    }

    /// Starts immediately within the route limit or joins that route's FIFO queue.
    pub fn submit(
        &mut self,
        admission: &ModelRequestAdmission,
    ) -> Result<ModelRequestAdmissionReceipt, ModelRequestPoolError> {
        validate_admission(admission)?;
        if let Some(existing_route) = self.exchange_routes.get(&admission.model_exchange_id.0) {
            if existing_route != &admission.route {
                return Err(exchange_conflict());
            }
            let partition = self.routes.get(existing_route).ok_or_else(pool_corrupt)?;
            let record = partition
                .records
                .get(&admission.model_exchange_id.0)
                .ok_or_else(pool_corrupt)?;
            if record.request_id != admission.request_id {
                return Err(exchange_conflict());
            }
            return Ok(ModelRequestAdmissionReceipt {
                model_exchange_id: record.model_exchange_id.clone(),
                state: record.state,
                status: ModelRequestAdmissionStatus::Duplicate,
                queue_position: partition.queue_position(&admission.model_exchange_id.0),
            });
        }
        if !self.routes.contains_key(&admission.route)
            && self.routes.len() >= self.config.max_routes
        {
            return Err(pool_error(
                ModelRequestPoolErrorCode::RouteLimit,
                "model request route capacity is exhausted",
            ));
        }
        let partition = self.routes.entry(admission.route.clone()).or_default();
        if partition.records.len() >= self.config.max_exchange_records_per_route {
            return Err(pool_error(
                ModelRequestPoolErrorCode::RecordLimit,
                "model request route record capacity is exhausted",
            ));
        }
        let (state, status, queue_position) =
            if partition.active_count() < self.config.max_active_per_route {
                (
                    ModelRequestState::Active,
                    ModelRequestAdmissionStatus::Started,
                    None,
                )
            } else {
                if partition.waiting.len() >= self.config.max_waiting_per_route {
                    return Err(pool_error(
                        ModelRequestPoolErrorCode::QueueFull,
                        "model request route waiting queue is full",
                    ));
                }
                partition
                    .waiting
                    .push_back(admission.model_exchange_id.0.clone());
                (
                    ModelRequestState::Queued,
                    ModelRequestAdmissionStatus::Queued,
                    Some(partition.waiting.len()),
                )
            };
        partition.records.insert(
            admission.model_exchange_id.0.clone(),
            ExchangeRecord::new(admission, state),
        );
        self.exchange_routes.insert(
            admission.model_exchange_id.0.clone(),
            admission.route.clone(),
        );
        Ok(ModelRequestAdmissionReceipt {
            model_exchange_id: admission.model_exchange_id.clone(),
            state,
            status,
            queue_position,
        })
    }

    /// Retains one contiguous frame or pauses Provider reading at the hard watermark.
    pub fn push_frame(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frame: &ModelStreamFrame,
    ) -> Result<ModelFrameWriteReceipt, ModelRequestPoolError> {
        self.push_frames(model_exchange_id, std::slice::from_ref(frame))
    }

    /// Atomically retains one Provider event's contiguous canonical frame batch.
    /// A rejected batch leaves every frame uncommitted and flips only the read
    /// control to paused, so the caller can retry the exact batch after an ack.
    pub fn push_frames(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        frames: &[ModelStreamFrame],
    ) -> Result<ModelFrameWriteReceipt, ModelRequestPoolError> {
        self.validate_frame_batch(frames)?;
        let config = self.config;
        let route = self.route_for(model_exchange_id)?.clone();
        let partition = self.routes.get_mut(&route).ok_or_else(pool_corrupt)?;
        let record = partition
            .records
            .get_mut(&model_exchange_id.0)
            .ok_or_else(pool_corrupt)?;
        let mut candidate = record.clone();
        let mut accepted = false;
        let mut accepted_terminal = false;
        for frame in frames {
            let digest = frame_digest(frame);
            if classify_frame_write(&candidate, frame, digest)?.is_some() {
                continue;
            }
            let next_frames = candidate.frames.len().saturating_add(1);
            let next_bytes = candidate.buffered_bytes.saturating_add(frame.payload.len());
            if next_frames > config.max_buffered_frames_per_stream
                || next_bytes > config.max_buffered_bytes_per_stream
            {
                record.provider_read_paused = true;
                return Ok(frame_receipt(
                    record,
                    ModelFrameWriteStatus::Backpressured,
                    None,
                ));
            }
            candidate.frames.insert(frame.sequence, frame.clone());
            candidate.buffered_bytes = next_bytes;
            candidate.next_sequence = candidate.next_sequence.checked_add(1).ok_or_else(|| {
                pool_error(
                    ModelRequestPoolErrorCode::InvalidState,
                    "model stream sequence overflowed",
                )
            })?;
            if let Some(outcome) = frame.terminal_outcome {
                candidate.state = outcome.state();
                candidate.terminal_outcome = Some(outcome);
                candidate.terminal_frame = Some(FrameFingerprint {
                    sequence: frame.sequence,
                    digest,
                    outcome,
                });
                accepted_terminal = true;
            }
            accepted = true;
        }
        if !accepted {
            return Ok(frame_receipt(
                record,
                ModelFrameWriteStatus::Duplicate,
                None,
            ));
        }
        candidate.provider_read_paused = !accepted_terminal
            && (candidate.frames.len() >= config.max_buffered_frames_per_stream
                || candidate.buffered_bytes >= config.max_buffered_bytes_per_stream);
        *record = candidate;
        let granted_exchange_id = accepted_terminal.then(|| partition.grant_next()).flatten();
        let record = partition
            .records
            .get(&model_exchange_id.0)
            .ok_or_else(pool_corrupt)?;
        Ok(frame_receipt(
            record,
            ModelFrameWriteStatus::Accepted,
            granted_exchange_id,
        ))
    }

    /// Validates that one canonical Provider event batch can fit into an empty
    /// stream buffer. Current occupancy is intentionally ignored.
    pub fn validate_frame_batch(
        &self,
        frames: &[ModelStreamFrame],
    ) -> Result<(), ModelRequestPoolError> {
        validate_frame_batch_shape(frames)?;
        let batch_bytes = frames
            .iter()
            .try_fold(0_usize, |total, frame| {
                total.checked_add(frame.payload.len())
            })
            .ok_or_else(|| invalid_input("model stream frame batch size overflowed"))?;
        if frames.len() > self.config.max_buffered_frames_per_stream
            || batch_bytes > self.config.max_buffered_bytes_per_stream
        {
            return Err(invalid_input(
                "model stream frame batch exceeds the configured hard limit",
            ));
        }
        Ok(())
    }

    /// Drops acknowledged frames and frees their bounded buffer space.
    pub fn acknowledge(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        sequence: u64,
    ) -> Result<ModelFrameAckReceipt, ModelRequestPoolError> {
        if sequence > MAX_SAFE_INTEGER {
            return Err(invalid_input("model acknowledgement sequence is invalid"));
        }
        let route = self.route_for(model_exchange_id)?.clone();
        let record = self
            .routes
            .get_mut(&route)
            .and_then(|partition| partition.records.get_mut(&model_exchange_id.0))
            .ok_or_else(pool_corrupt)?;
        if sequence > record.highest_sequence() {
            return Err(pool_error(
                ModelRequestPoolErrorCode::SequenceGap,
                "model acknowledgement exceeds the accepted stream",
            ));
        }
        if sequence <= record.acknowledged_sequence {
            return Ok(ack_receipt(record, true));
        }
        record
            .frames
            .retain(|frame_sequence, _frame| *frame_sequence > sequence);
        record.buffered_bytes = record
            .frames
            .values()
            .map(|frame| frame.payload.len())
            .sum();
        record.acknowledged_sequence = sequence;
        if record.provider_read_paused
            && record.frames.len() <= self.config.resume_buffered_frames_per_stream
            && record.buffered_bytes <= self.config.resume_buffered_bytes_per_stream
        {
            record.provider_read_paused = false;
        }
        Ok(ack_receipt(record, false))
    }

    /// Cancels an active or queued request. Active cancellation grants exactly
    /// one FIFO waiter on the same route.
    pub fn cancel(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<ModelRequestTerminalReceipt, ModelRequestPoolError> {
        self.terminate(model_exchange_id, ModelRequestTerminalOutcome::Cancelled)
    }

    /// Applies one terminal outcome exactly once and releases an active slot.
    pub fn terminate(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        outcome: ModelRequestTerminalOutcome,
    ) -> Result<ModelRequestTerminalReceipt, ModelRequestPoolError> {
        validate_prefixed_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
        let route = self.route_for(model_exchange_id)?.clone();
        let partition = self.routes.get_mut(&route).ok_or_else(pool_corrupt)?;
        let state = partition
            .records
            .get(&model_exchange_id.0)
            .map(|record| (record.state, record.terminal_outcome))
            .ok_or_else(pool_corrupt)?;
        if state.0.terminal() {
            if state.1 == Some(outcome) {
                return Ok(ModelRequestTerminalReceipt {
                    model_exchange_id: model_exchange_id.clone(),
                    outcome,
                    replayed: true,
                    granted_exchange_id: None,
                });
            }
            return Err(pool_error(
                ModelRequestPoolErrorCode::TerminalConflict,
                "model request already has another terminal outcome",
            ));
        }
        if state.0 == ModelRequestState::Queued && outcome != ModelRequestTerminalOutcome::Cancelled
        {
            return Err(pool_error(
                ModelRequestPoolErrorCode::InvalidState,
                "queued model request may only be cancelled",
            ));
        }
        if state.0 == ModelRequestState::Queued {
            partition
                .waiting
                .retain(|queued| queued != &model_exchange_id.0);
        }
        let record = partition
            .records
            .get_mut(&model_exchange_id.0)
            .ok_or_else(pool_corrupt)?;
        record.state = outcome.state();
        record.terminal_outcome = Some(outcome);
        record.provider_read_paused = false;
        if outcome == ModelRequestTerminalOutcome::Cancelled {
            record.frames.clear();
            record.buffered_bytes = 0;
        }
        let granted_exchange_id = if state.0 == ModelRequestState::Active {
            partition.grant_next()
        } else {
            None
        };
        Ok(ModelRequestTerminalReceipt {
            model_exchange_id: model_exchange_id.clone(),
            outcome,
            replayed: false,
            granted_exchange_id,
        })
    }

    /// Returns buffered frames after a cursor without exceeding caller limits.
    pub fn read_buffered(
        &self,
        model_exchange_id: &ModelExchangeId,
        after_sequence: u64,
        max_frames: usize,
        max_bytes: usize,
    ) -> Result<Vec<ModelStreamFrame>, ModelRequestPoolError> {
        if max_frames == 0 || max_bytes == 0 || after_sequence > MAX_SAFE_INTEGER {
            return Err(invalid_input("model stream read limits are invalid"));
        }
        let record = self.record(model_exchange_id)?;
        let mut bytes = 0_usize;
        let mut frames = Vec::new();
        for frame in record
            .frames
            .range((after_sequence.saturating_add(1))..)
            .map(|entry| entry.1)
        {
            if frames.len() >= max_frames || bytes.saturating_add(frame.payload.len()) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(frame.payload.len());
            frames.push(frame.clone());
        }
        Ok(frames)
    }

    /// Rebuilds the exact current exchange projection without reapplying terminal state.
    pub fn reconnect(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<ModelRequestSnapshot, ModelRequestPoolError> {
        let route = self.route_for(model_exchange_id)?;
        let partition = self.routes.get(route).ok_or_else(pool_corrupt)?;
        let record = partition
            .records
            .get(&model_exchange_id.0)
            .ok_or_else(pool_corrupt)?;
        Ok(ModelRequestSnapshot {
            route: route.clone(),
            model_exchange_id: record.model_exchange_id.clone(),
            request_id: record.request_id.clone(),
            state: record.state,
            queue_position: partition.queue_position(&model_exchange_id.0),
            next_sequence: record.next_sequence,
            acknowledged_sequence: record.acknowledged_sequence,
            buffered_frames: record.frames.len(),
            buffered_bytes: record.buffered_bytes,
            read_control: record.read_control(),
            terminal_outcome: record.terminal_outcome,
        })
    }

    /// Encodes one bounded exchange buffer for the restricted durable authority.
    ///
    /// # Errors
    ///
    /// Rejects unknown/corrupt exchange state or serialization failure.
    pub fn export_exchange(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Vec<u8>, ModelRequestPoolError> {
        let route = self.route_for(model_exchange_id)?;
        let record = self.record(model_exchange_id)?;
        let durable = durable_pool_exchange(route, record);
        serde_json::to_vec(&durable).map_err(|_| pool_corrupt())
    }

    /// Restores one exact bounded exchange buffer after Control Plane restart.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical bytes, changed route/request identity, invalid
    /// sequence/fingerprint state, or exhausted route capacity.
    pub fn restore_exchange(
        &mut self,
        admission: &ModelRequestAdmission,
        bytes: &[u8],
    ) -> Result<ModelRequestAdmissionReceipt, ModelRequestPoolError> {
        let durable: DurablePoolExchange =
            serde_json::from_slice(bytes).map_err(|_| pool_corrupt())?;
        if serde_json::to_vec(&durable).map_err(|_| pool_corrupt())? != bytes
            || durable.schema != DURABLE_POOL_SCHEMA
            || durable.route_fingerprint != route_fingerprint(&admission.route)
            || durable.model_exchange_id != admission.model_exchange_id
            || durable.request_id != admission.request_id
        {
            return Err(exchange_conflict());
        }
        let restored = restored_record(&durable, self.config)?;
        let receipt = self.submit(admission)?;
        if receipt.state != ModelRequestState::Active && restored.state == ModelRequestState::Active
        {
            return Err(pool_error(
                ModelRequestPoolErrorCode::InvalidState,
                "durable active model exchange cannot restore into a queued slot",
            ));
        }
        let route = self.route_for(&admission.model_exchange_id)?.clone();
        let record = self
            .routes
            .get_mut(&route)
            .and_then(|partition| partition.records.get_mut(&admission.model_exchange_id.0))
            .ok_or_else(pool_corrupt)?;
        *record = restored;
        Ok(ModelRequestAdmissionReceipt {
            model_exchange_id: admission.model_exchange_id.clone(),
            state: record.state,
            status: ModelRequestAdmissionStatus::Duplicate,
            queue_position: None,
        })
    }

    /// Encodes the complete bounded route/queue authority for process restart.
    ///
    /// # Errors
    ///
    /// Returns a stable error for corrupt in-memory state or serialization failure.
    pub fn export_authority(&self) -> Result<Vec<u8>, ModelRequestPoolError> {
        let routes = self
            .routes
            .iter()
            .map(|(route, partition)| DurablePoolRoute {
                provider: route.provider.clone(),
                model: route.model.clone(),
                credential_reference_id: route.credential_reference.clone(),
                organization_id: route.organization.clone(),
                project_id: route.project.clone(),
                waiting: partition
                    .waiting
                    .iter()
                    .cloned()
                    .map(ModelExchangeId)
                    .collect(),
                exchanges: partition
                    .records
                    .values()
                    .map(|record| durable_pool_exchange(route, record))
                    .collect(),
            })
            .collect();
        serde_json::to_vec(&DurablePoolAuthority {
            schema: DURABLE_POOL_AUTHORITY_SCHEMA.to_owned(),
            config: DurablePoolConfig::from(self.config),
            routes,
        })
        .map_err(|_| pool_corrupt())
    }

    /// Replaces an empty in-memory pool with the exact durable authority.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical/corrupt bytes, changed configuration, duplicate
    /// routes/exchanges, or inconsistent FIFO state.
    pub fn restore_authority(&mut self, bytes: &[u8]) -> Result<(), ModelRequestPoolError> {
        if !self.routes.is_empty() || !self.exchange_routes.is_empty() {
            return Err(pool_error(
                ModelRequestPoolErrorCode::InvalidState,
                "model request pool restore requires an empty instance",
            ));
        }
        let durable: DurablePoolAuthority =
            serde_json::from_slice(bytes).map_err(|_| pool_corrupt())?;
        if serde_json::to_vec(&durable).map_err(|_| pool_corrupt())? != bytes
            || durable.schema != DURABLE_POOL_AUTHORITY_SCHEMA
            || durable.config != DurablePoolConfig::from(self.config)
            || durable.routes.len() > self.config.max_routes
        {
            return Err(pool_corrupt());
        }
        let (routes, exchange_routes) = restored_authority(&durable, self.config)?;
        self.routes = routes;
        self.exchange_routes = exchange_routes;
        Ok(())
    }

    /// Reports whether this process has loaded any route authority.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty() && self.exchange_routes.is_empty()
    }

    /// Returns the stable identities of exchanges that still own unacknowledged
    /// frames. Payloads remain inside the bounded pool authority.
    #[must_use]
    pub(crate) fn buffered_exchange_ids(&self) -> Vec<ModelExchangeId> {
        let mut exchange_ids = self
            .routes
            .values()
            .flat_map(|partition| partition.records.values())
            .filter(|record| !record.frames.is_empty())
            .map(|record| record.model_exchange_id.clone())
            .collect::<Vec<_>>();
        exchange_ids.sort_by(|left, right| left.0.cmp(&right.0));
        exchange_ids
    }

    /// Returns capacity use for one route without exposing buffered payloads.
    #[must_use]
    pub fn route_snapshot(&self, route: &ModelRequestRouteKey) -> Option<ModelRoutePoolSnapshot> {
        let partition = self.routes.get(route)?;
        Some(ModelRoutePoolSnapshot {
            route: route.clone(),
            active: partition.active_count(),
            waiting: partition.waiting.len(),
            retained_terminal: partition.terminal_count(),
            records: partition.records.len(),
        })
    }

    /// Forgets an acknowledged terminal record after its durable owner has
    /// retained the terminal receipt.
    pub fn forget_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<(), ModelRequestPoolError> {
        let route = self.route_for(model_exchange_id)?.clone();
        let partition = self.routes.get_mut(&route).ok_or_else(pool_corrupt)?;
        let record = partition
            .records
            .get(&model_exchange_id.0)
            .ok_or_else(pool_corrupt)?;
        if !record.state.terminal() || !record.frames.is_empty() {
            return Err(pool_error(
                ModelRequestPoolErrorCode::InvalidState,
                "model request can be forgotten only after terminal frames are acknowledged",
            ));
        }
        partition.records.remove(&model_exchange_id.0);
        self.exchange_routes.remove(&model_exchange_id.0);
        let route_is_empty = partition.records.is_empty();
        if route_is_empty {
            self.routes.remove(&route);
        }
        Ok(())
    }

    fn route_for(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<&ModelRequestRouteKey, ModelRequestPoolError> {
        validate_prefixed_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
        self.exchange_routes
            .get(&model_exchange_id.0)
            .ok_or_else(|| {
                pool_error(
                    ModelRequestPoolErrorCode::NotFound,
                    "model request exchange is not registered",
                )
            })
    }

    fn record(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<&ExchangeRecord, ModelRequestPoolError> {
        let route = self.route_for(model_exchange_id)?;
        self.routes
            .get(route)
            .and_then(|partition| partition.records.get(&model_exchange_id.0))
            .ok_or_else(pool_corrupt)
    }
}

fn validate_admission(admission: &ModelRequestAdmission) -> Result<(), ModelRequestPoolError> {
    validate_route_key(&admission.route)?;
    validate_prefixed_id(&admission.model_exchange_id.0, "mdl_", "modelExchangeId")?;
    validate_prefixed_id(&admission.request_id.0, "req_", "requestId")
}

fn validate_route_key(route: &ModelRequestRouteKey) -> Result<(), ModelRequestPoolError> {
    validate_token(&route.provider, 128, "providerId")?;
    validate_token(&route.model, 200, "modelId")?;
    validate_prefixed_id(
        &route.credential_reference.0,
        "crd_",
        "credentialReferenceId",
    )?;
    validate_prefixed_id(&route.organization.0, "org_", "organizationId")?;
    validate_prefixed_id(&route.project.0, "prj_", "projectId")
}

fn validate_frame(frame: &ModelStreamFrame) -> Result<(), ModelRequestPoolError> {
    if frame.sequence == 0 || frame.sequence > MAX_SAFE_INTEGER {
        return Err(invalid_input("model stream sequence is invalid"));
    }
    Ok(())
}

fn validate_frame_batch_shape(frames: &[ModelStreamFrame]) -> Result<(), ModelRequestPoolError> {
    if frames.is_empty() {
        return Err(invalid_input("model stream frame batch is empty"));
    }
    for (index, frame) in frames.iter().enumerate() {
        validate_frame(frame)?;
        if frame.terminal_outcome == Some(ModelRequestTerminalOutcome::Cancelled) {
            return Err(pool_error(
                ModelRequestPoolErrorCode::InvalidInput,
                "model stream cancellation must use the pool cancellation path",
            ));
        }
        if frame.terminal_outcome.is_some() && index + 1 != frames.len() {
            return Err(invalid_input(
                "model stream terminal frame must end its canonical batch",
            ));
        }
    }
    Ok(())
}

fn restored_record(
    durable: &DurablePoolExchange,
    config: ModelRequestPoolConfig,
) -> Result<ExchangeRecord, ModelRequestPoolError> {
    let state = parse_request_state(&durable.state)?;
    if durable.next_sequence == 0
        || durable.acknowledged_sequence >= durable.next_sequence
        || durable.next_sequence > MAX_SAFE_INTEGER
    {
        return Err(pool_corrupt());
    }
    let terminal_outcome = durable
        .terminal_outcome
        .as_deref()
        .map(parse_terminal_outcome)
        .transpose()?;
    if terminal_outcome.map(ModelRequestTerminalOutcome::state) != state.terminal().then_some(state)
    {
        return Err(pool_corrupt());
    }
    let mut frames = BTreeMap::new();
    let mut buffered_bytes = 0_usize;
    for durable_frame in &durable.frames {
        let payload = STANDARD
            .decode(&durable_frame.payload_base64)
            .map_err(|_| pool_corrupt())?;
        let terminal_outcome = durable_frame
            .terminal_outcome
            .as_deref()
            .map(parse_terminal_outcome)
            .transpose()?;
        let frame = ModelStreamFrame {
            sequence: durable_frame.sequence,
            payload,
            terminal_outcome,
        };
        validate_frame(&frame)?;
        buffered_bytes = buffered_bytes
            .checked_add(frame.payload.len())
            .ok_or_else(pool_corrupt)?;
        if frames.insert(frame.sequence, frame).is_some() {
            return Err(pool_corrupt());
        }
    }
    if frames.len() > config.max_buffered_frames_per_stream
        || buffered_bytes > config.max_buffered_bytes_per_stream
        || !frames
            .keys()
            .copied()
            .eq((durable.acknowledged_sequence + 1)..durable.next_sequence)
    {
        return Err(pool_corrupt());
    }
    if state == ModelRequestState::Queued
        && (durable.next_sequence != 1
            || durable.acknowledged_sequence != 0
            || durable.provider_read_paused
            || !frames.is_empty()
            || terminal_outcome.is_some())
    {
        return Err(pool_corrupt());
    }
    let terminal_frame = terminal_outcome
        .filter(|outcome| *outcome != ModelRequestTerminalOutcome::Cancelled)
        .map(|outcome| {
            let frame = frames
                .get(&durable.next_sequence.saturating_sub(1))
                .ok_or_else(pool_corrupt)?;
            if frame.terminal_outcome != Some(outcome) {
                return Err(pool_corrupt());
            }
            Ok(FrameFingerprint {
                sequence: frame.sequence,
                digest: frame_digest(frame),
                outcome,
            })
        })
        .transpose()?;
    if terminal_outcome == Some(ModelRequestTerminalOutcome::Cancelled) && !frames.is_empty() {
        return Err(pool_corrupt());
    }
    Ok(ExchangeRecord {
        model_exchange_id: durable.model_exchange_id.clone(),
        request_id: durable.request_id.clone(),
        state,
        next_sequence: durable.next_sequence,
        acknowledged_sequence: durable.acknowledged_sequence,
        buffered_bytes,
        provider_read_paused: durable.provider_read_paused,
        frames,
        terminal_outcome,
        terminal_frame,
    })
}

impl From<ModelRequestPoolConfig> for DurablePoolConfig {
    fn from(config: ModelRequestPoolConfig) -> Self {
        Self {
            max_active_per_route: config.max_active_per_route,
            max_waiting_per_route: config.max_waiting_per_route,
            max_routes: config.max_routes,
            max_exchange_records_per_route: config.max_exchange_records_per_route,
            max_buffered_frames_per_stream: config.max_buffered_frames_per_stream,
            max_buffered_bytes_per_stream: config.max_buffered_bytes_per_stream,
            resume_buffered_frames_per_stream: config.resume_buffered_frames_per_stream,
            resume_buffered_bytes_per_stream: config.resume_buffered_bytes_per_stream,
        }
    }
}

fn durable_pool_exchange(
    route: &ModelRequestRouteKey,
    record: &ExchangeRecord,
) -> DurablePoolExchange {
    DurablePoolExchange {
        schema: DURABLE_POOL_SCHEMA.to_owned(),
        route_fingerprint: route_fingerprint(route),
        model_exchange_id: record.model_exchange_id.clone(),
        request_id: record.request_id.clone(),
        state: request_state_name(record.state).to_owned(),
        next_sequence: record.next_sequence,
        acknowledged_sequence: record.acknowledged_sequence,
        provider_read_paused: record.provider_read_paused,
        frames: record
            .frames
            .values()
            .map(|frame| DurablePoolFrame {
                sequence: frame.sequence,
                payload_base64: STANDARD.encode(&frame.payload),
                terminal_outcome: frame
                    .terminal_outcome
                    .map(terminal_outcome_name)
                    .map(str::to_owned),
            })
            .collect(),
        terminal_outcome: record
            .terminal_outcome
            .map(terminal_outcome_name)
            .map(str::to_owned),
    }
}

fn restored_authority(
    durable: &DurablePoolAuthority,
    config: ModelRequestPoolConfig,
) -> Result<RestoredPoolAuthority, ModelRequestPoolError> {
    let mut routes = BTreeMap::new();
    let mut exchange_routes = HashMap::new();
    for durable_route in &durable.routes {
        let route = ModelRequestRouteKey {
            provider: durable_route.provider.clone(),
            model: durable_route.model.clone(),
            credential_reference: durable_route.credential_reference_id.clone(),
            organization: durable_route.organization_id.clone(),
            project: durable_route.project_id.clone(),
        };
        validate_route_key(&route)?;
        if durable_route.exchanges.len() > config.max_exchange_records_per_route {
            return Err(pool_corrupt());
        }
        let mut records = BTreeMap::new();
        for durable_exchange in &durable_route.exchanges {
            if durable_exchange.route_fingerprint != route_fingerprint(&route) {
                return Err(pool_corrupt());
            }
            let record = restored_record(durable_exchange, config)?;
            let exchange_id = record.model_exchange_id.0.clone();
            if records.insert(exchange_id.clone(), record).is_some()
                || exchange_routes.insert(exchange_id, route.clone()).is_some()
            {
                return Err(pool_corrupt());
            }
        }
        let waiting = durable_route
            .waiting
            .iter()
            .map(|exchange| exchange.0.clone())
            .collect::<VecDeque<_>>();
        if waiting.len() > config.max_waiting_per_route
            || waiting.iter().any(|exchange| {
                records
                    .get(exchange)
                    .is_none_or(|record| record.state != ModelRequestState::Queued)
            })
            || records
                .values()
                .filter(|record| record.state == ModelRequestState::Queued)
                .any(|record| !waiting.contains(&record.model_exchange_id.0))
        {
            return Err(pool_corrupt());
        }
        let partition = RoutePartition { records, waiting };
        if partition.active_count() > config.max_active_per_route
            || routes.insert(route, partition).is_some()
        {
            return Err(pool_corrupt());
        }
    }
    Ok((routes, exchange_routes))
}

fn route_fingerprint(route: &ModelRequestRouteKey) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.model-request-route.v1\0");
    for value in [
        route.organization.0.as_str(),
        route.project.0.as_str(),
        route.provider.as_str(),
        route.model.as_str(),
        route.credential_reference.0.as_str(),
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
}

const fn request_state_name(state: ModelRequestState) -> &'static str {
    match state {
        ModelRequestState::Queued => "queued",
        ModelRequestState::Active => "active",
        ModelRequestState::Succeeded => "succeeded",
        ModelRequestState::Failed => "failed",
        ModelRequestState::Cancelled => "cancelled",
    }
}

fn parse_request_state(value: &str) -> Result<ModelRequestState, ModelRequestPoolError> {
    match value {
        "queued" => Ok(ModelRequestState::Queued),
        "active" => Ok(ModelRequestState::Active),
        "succeeded" => Ok(ModelRequestState::Succeeded),
        "failed" => Ok(ModelRequestState::Failed),
        "cancelled" => Ok(ModelRequestState::Cancelled),
        _ => Err(pool_corrupt()),
    }
}

const fn terminal_outcome_name(outcome: ModelRequestTerminalOutcome) -> &'static str {
    match outcome {
        ModelRequestTerminalOutcome::Succeeded => "succeeded",
        ModelRequestTerminalOutcome::Failed => "failed",
        ModelRequestTerminalOutcome::Cancelled => "cancelled",
    }
}

fn parse_terminal_outcome(
    value: &str,
) -> Result<ModelRequestTerminalOutcome, ModelRequestPoolError> {
    match value {
        "succeeded" => Ok(ModelRequestTerminalOutcome::Succeeded),
        "failed" => Ok(ModelRequestTerminalOutcome::Failed),
        "cancelled" => Ok(ModelRequestTerminalOutcome::Cancelled),
        _ => Err(pool_corrupt()),
    }
}

fn classify_frame_write(
    record: &ExchangeRecord,
    frame: &ModelStreamFrame,
    digest: [u8; 32],
) -> Result<Option<ModelFrameWriteStatus>, ModelRequestPoolError> {
    if record.state.terminal() {
        if frame.sequence <= record.acknowledged_sequence {
            return Ok(Some(ModelFrameWriteStatus::Duplicate));
        }
        if record
            .frames
            .get(&frame.sequence)
            .is_some_and(|existing| frame_digest(existing) == digest)
        {
            return Ok(Some(ModelFrameWriteStatus::Duplicate));
        }
        if let (Some(outcome), Some(terminal)) = (frame.terminal_outcome, record.terminal_frame)
            && terminal.sequence == frame.sequence
            && terminal.digest == digest
            && terminal.outcome == outcome
        {
            return Ok(Some(ModelFrameWriteStatus::Duplicate));
        }
        return Err(pool_error(
            ModelRequestPoolErrorCode::FrameConflict,
            "terminal model stream rejected another frame",
        ));
    }
    if record.state != ModelRequestState::Active {
        return Err(pool_error(
            ModelRequestPoolErrorCode::InvalidState,
            "queued model request cannot accept stream frames",
        ));
    }
    match frame.sequence.cmp(&record.next_sequence) {
        Ordering::Less if frame.sequence <= record.acknowledged_sequence => {
            Ok(Some(ModelFrameWriteStatus::Duplicate))
        }
        Ordering::Less => {
            let existing = record
                .frames
                .get(&frame.sequence)
                .ok_or_else(pool_corrupt)?;
            if frame_digest(existing) == digest {
                Ok(Some(ModelFrameWriteStatus::Duplicate))
            } else {
                Err(pool_error(
                    ModelRequestPoolErrorCode::FrameConflict,
                    "model stream sequence was reused with different content",
                ))
            }
        }
        Ordering::Equal => Ok(None),
        Ordering::Greater => Err(pool_error(
            ModelRequestPoolErrorCode::SequenceGap,
            "model stream frame is not contiguous",
        )),
    }
}

fn frame_digest(frame: &ModelStreamFrame) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(frame.sequence.to_be_bytes());
    digest.update([match frame.terminal_outcome {
        None => 0,
        Some(ModelRequestTerminalOutcome::Succeeded) => 1,
        Some(ModelRequestTerminalOutcome::Failed) => 2,
        Some(ModelRequestTerminalOutcome::Cancelled) => 3,
    }]);
    digest.update(
        u64::try_from(frame.payload.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(&frame.payload);
    digest.finalize().into()
}

fn frame_receipt(
    record: &ExchangeRecord,
    status: ModelFrameWriteStatus,
    granted_exchange_id: Option<ModelExchangeId>,
) -> ModelFrameWriteReceipt {
    ModelFrameWriteReceipt {
        status,
        state: record.state,
        highest_sequence: record.highest_sequence(),
        buffered_frames: record.frames.len(),
        buffered_bytes: record.buffered_bytes,
        read_control: record.read_control(),
        granted_exchange_id,
    }
}

fn ack_receipt(record: &ExchangeRecord, replayed: bool) -> ModelFrameAckReceipt {
    ModelFrameAckReceipt {
        model_exchange_id: record.model_exchange_id.clone(),
        acknowledged_sequence: record.acknowledged_sequence,
        buffered_frames: record.frames.len(),
        buffered_bytes: record.buffered_bytes,
        read_control: record.read_control(),
        replayed,
    }
}

fn validate_token(
    value: &str,
    max_len: usize,
    _field: &'static str,
) -> Result<(), ModelRequestPoolError> {
    if value.is_empty()
        || value.len() > max_len
        || value.chars().any(char::is_control)
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
    {
        return Err(invalid_input("model request route token is invalid"));
    }
    Ok(())
}

fn validate_prefixed_id(
    value: &str,
    prefix: &str,
    field: &'static str,
) -> Result<(), ModelRequestPoolError> {
    validate_token(value, 200, field)?;
    if !value.starts_with(prefix) || value.len() == prefix.len() {
        return Err(invalid_input("model request identity is invalid"));
    }
    Ok(())
}

fn invalid_config(message: &'static str) -> ModelRequestPoolError {
    pool_error(ModelRequestPoolErrorCode::InvalidConfig, message)
}

fn invalid_input(message: &'static str) -> ModelRequestPoolError {
    pool_error(ModelRequestPoolErrorCode::InvalidInput, message)
}

fn exchange_conflict() -> ModelRequestPoolError {
    pool_error(
        ModelRequestPoolErrorCode::ExchangeConflict,
        "model exchange identity was reused with another route or request",
    )
}

fn pool_corrupt() -> ModelRequestPoolError {
    pool_error(
        ModelRequestPoolErrorCode::InvalidState,
        "model request pool indexes are inconsistent",
    )
}

const fn pool_error(
    code: ModelRequestPoolErrorCode,
    message: &'static str,
) -> ModelRequestPoolError {
    ModelRequestPoolError { code, message }
}
