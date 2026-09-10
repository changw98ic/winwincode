// SPDX-License-Identifier: Apache-2.0

//! Durable peer-session collaboration for the personal Community runtime.
//!
//! The Control Plane is the only writer. Agent sessions submit typed requests
//! and results; this service resolves stable Agent identities, validates every
//! transition, and commits the catalog plus its receipt atomically.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_delivery::domain::EvidenceRef;
use winwincode_domain::{
    EvidenceId, ProductSessionId, RequestId, Sha256Digest, WorkItemId, WorkerSessionId,
};
use winwincode_execution_port::{agent_config::AgentIdentity, generated::WorkerCapabilityFeature};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StorageError, StorageErrorKind, StoredState,
};

const STATE_SCHEMA: &str = "winwincode.peer-collaboration.v1";
const STATE_STREAM: &str = "peer-collaboration:community";
const RECEIPT_TOPIC: &str = "peer-collaboration.receipt.internal.v1";
const MAX_AGENTS: usize = 256;
const MAX_REQUESTS: usize = 20_000;
const MAX_CONTEXT_REFS: usize = 16;
const MAX_EVIDENCE_REFS: usize = 64;
const MAX_HOPS: u8 = 4;
const RATE_WINDOW_MILLIS: u64 = 60_000;
const MAX_REQUESTS_PER_WINDOW: usize = 20;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Stable peer-collaboration failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerCollaborationErrorKind {
    InvalidRequest,
    Unauthorized,
    TargetNotFound,
    InvalidState,
    Duplicate,
    LoopDetected,
    RateLimited,
    RevisionConflict,
    RequestConflict,
    Storage,
    Corrupt,
}

/// Secret-safe peer-collaboration error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerCollaborationError {
    kind: PeerCollaborationErrorKind,
    message: &'static str,
}

impl PeerCollaborationError {
    const fn new(kind: PeerCollaborationErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    /// Returns the stable machine-readable error category.
    #[must_use]
    pub const fn kind(&self) -> PeerCollaborationErrorKind {
        self.kind
    }
}

impl fmt::Display for PeerCollaborationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PeerCollaborationError {}

impl From<StorageError> for PeerCollaborationError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => revision_conflict(),
            StorageErrorKind::RequestConflict => request_conflict(),
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => invalid(),
            StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => storage_error(),
        }
    }
}

/// Trusted clock failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCollaborationClockError;

/// Clock used for durable ordering and rate limiting.
pub trait PeerCollaborationClock: Send {
    /// Returns Unix epoch milliseconds.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted clock is unavailable.
    fn now_millis(&mut self) -> Result<u64, PeerCollaborationClockError>;
}

/// System implementation of the trusted clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemPeerCollaborationClock;

impl PeerCollaborationClock for SystemPeerCollaborationClock {
    fn now_millis(&mut self) -> Result<u64, PeerCollaborationClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PeerCollaborationClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| PeerCollaborationClockError)
    }
}

/// Current availability of one stable Agent identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAvailability {
    Offline,
    Recovering,
    Working,
}

/// Durable Agent Directory row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDirectoryEntry {
    pub identity: AgentIdentity,
    pub current_session_id: Option<WorkerSessionId>,
    pub availability: AgentAvailability,
    pub updated_at_millis: u64,
}

/// Human-usable directory selector. Callers never need an Agent UUID.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AgentSelector {
    Identity { name_or_role: String },
    Capability { capability: WorkerCapabilityFeature },
}

/// Controller-owned directory update.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDirectoryCommand {
    pub request_id: RequestId,
    pub expected_catalog_revision: u64,
    pub identity: AgentIdentity,
    pub current_session_id: Option<WorkerSessionId>,
    pub availability: AgentAvailability,
}

/// One directory query result cut.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDirectorySnapshot {
    pub catalog_revision: u64,
    pub agents: Vec<AgentDirectoryEntry>,
}

/// Authenticated runtime identity supplied by the Controller, not by the model.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSessionPrincipal {
    pub identity: AgentIdentity,
    pub session_id: WorkerSessionId,
}

/// Bounded references are passed instead of copying chat or workspace content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationContextRef {
    WorkItem {
        work_item_id: WorkItemId,
    },
    ProductSession {
        product_session_id: ProductSessionId,
    },
    Candidate {
        work_item_id: WorkItemId,
        candidate_digest: Sha256Digest,
    },
    Evidence {
        evidence_id: EvidenceId,
    },
}

/// Typed collaboration request payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationPayload {
    Ask {
        question: String,
        context_refs: Vec<CollaborationContextRef>,
    },
    Consult {
        question: String,
        context_refs: Vec<CollaborationContextRef>,
    },
    Delegate {
        parent_work_item_id: WorkItemId,
        delegated_work_item_id: WorkItemId,
        objective: String,
        context_refs: Vec<CollaborationContextRef>,
    },
    ReviewRequest {
        work_item_id: WorkItemId,
        candidate_digest: Sha256Digest,
        context_refs: Vec<CollaborationContextRef>,
    },
}

/// Closed request kind used by the read projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationRequestKind {
    Ask,
    Consult,
    Delegate,
    ReviewRequest,
}

/// Whether an answer is an opinion or is supported as an authoritative fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerClassification {
    Opinion,
    AuthoritativeFact,
}

/// Typed result returned to the original requester.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationResult {
    AskAnswer {
        answer: String,
        classification: AnswerClassification,
        evidence_refs: Vec<EvidenceRef>,
    },
    ConsultAnswer {
        answer: String,
        classification: AnswerClassification,
        evidence_refs: Vec<EvidenceRef>,
    },
    DelegatedWork {
        summary: String,
        evidence_refs: Vec<EvidenceRef>,
    },
    Review {
        candidate_digest: Sha256Digest,
        summary: String,
        evidence_refs: Vec<EvidenceRef>,
    },
}

/// Durable delivery lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationRequestState {
    Queued,
    Delivered,
    Acknowledged,
    Completed,
}

/// Stable collaboration request identity.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CollaborationRequestId(pub String);

/// Canonical durable request. Requester identity is filled from the principal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollaborationRequest {
    pub id: CollaborationRequestId,
    pub requester: AgentIdentity,
    pub requester_session_id: WorkerSessionId,
    pub target: AgentIdentity,
    pub target_session_id: Option<WorkerSessionId>,
    pub parent_request_id: Option<CollaborationRequestId>,
    pub hop_count: u8,
    pub payload: CollaborationPayload,
    pub state: CollaborationRequestState,
    pub result: Option<CollaborationResult>,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

/// Request submission contains no requester identity field.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitCollaborationCommand {
    pub request_id: RequestId,
    pub expected_catalog_revision: u64,
    pub collaboration_request_id: CollaborationRequestId,
    pub target: AgentSelector,
    pub parent_request_id: Option<CollaborationRequestId>,
    pub payload: CollaborationPayload,
}

/// Target-side state transition reported to the Controller.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CollaborationAdvanceAction {
    Deliver,
    Acknowledge,
    Complete { result: CollaborationResult },
}

/// Idempotent collaboration transition command.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvanceCollaborationCommand {
    pub request_id: RequestId,
    pub expected_catalog_revision: u64,
    pub collaboration_request_id: CollaborationRequestId,
    pub action: CollaborationAdvanceAction,
}

/// Secret-safe durable mutation receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerCollaborationReceipt {
    pub catalog_revision: u64,
    pub collaboration_request_id: Option<CollaborationRequestId>,
    pub state: Option<CollaborationRequestState>,
    pub idempotent_replay: bool,
}

/// Read lane used by the non-UI collaboration projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerCollaborationLane {
    Inbox,
    Delegated,
    Waiting,
}

/// Candidate freshness derived from the current candidate digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFreshness {
    NotApplicable,
    Current,
    Stale,
}

/// Bounded list row. It intentionally contains no question, answer, context, or evidence body.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCollaborationProjection {
    pub request_id: CollaborationRequestId,
    pub kind: CollaborationRequestKind,
    pub state: CollaborationRequestState,
    pub lane: PeerCollaborationLane,
    pub source_session_id: WorkerSessionId,
    pub target_session_id: Option<WorkerSessionId>,
    pub review_freshness: ReviewFreshness,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerCollaborationState {
    schema: String,
    revision: u64,
    agents: BTreeMap<String, AgentDirectoryEntry>,
    requests: BTreeMap<CollaborationRequestId, CollaborationRequest>,
}

/// Durable personal peer-collaboration service.
pub struct PeerCollaborationService<'a> {
    storage: &'a mut dyn ProductStateStorage,
    clock: Box<dyn PeerCollaborationClock>,
}

impl<'a> PeerCollaborationService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self::with_clock(storage, Box::new(SystemPeerCollaborationClock))
    }

    #[must_use]
    pub fn with_clock(
        storage: &'a mut dyn ProductStateStorage,
        clock: Box<dyn PeerCollaborationClock>,
    ) -> Self {
        Self { storage, clock }
    }

    /// Returns the current catalog revision.
    ///
    /// # Errors
    ///
    /// Rejects corrupt durable state or a storage failure.
    pub fn catalog_revision(&self) -> Result<u64, PeerCollaborationError> {
        Ok(self.load_state()?.revision)
    }

    /// Applies one Controller-owned Agent Directory update.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, invalid availability/session pairings, stale
    /// revisions, conflicting request ids, or storage failures.
    pub fn sync_agent(
        &mut self,
        command: &AgentDirectoryCommand,
    ) -> Result<PeerCollaborationReceipt, PeerCollaborationError> {
        validate_directory_command(command)?;
        let (identity, digest) =
            command_identity("directory", "controller", &command.request_id, command)?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return decode_receipt(&receipt, true);
        }
        let now = self.now()?;
        let mut state = self.load_state()?;
        require_revision(&state, command.expected_catalog_revision)?;
        if state.agents.len() >= MAX_AGENTS && !state.agents.contains_key(&command.identity.id) {
            return Err(invalid());
        }
        if let Some(session_id) = &command.current_session_id
            && state.agents.values().any(|entry| {
                entry.identity.id != command.identity.id
                    && entry.current_session_id.as_ref() == Some(session_id)
            })
        {
            return Err(invalid());
        }
        state.agents.insert(
            command.identity.id.clone(),
            AgentDirectoryEntry {
                identity: command.identity.clone(),
                current_session_id: command.current_session_id.clone(),
                availability: command.availability,
                updated_at_millis: now,
            },
        );
        self.commit(
            state,
            command.expected_catalog_revision,
            identity,
            digest,
            None,
            None,
        )
    }

    /// Finds Agents by a human identity label or capability.
    ///
    /// # Errors
    ///
    /// Rejects invalid selectors or corrupt durable state.
    pub fn find_agents(
        &self,
        selector: &AgentSelector,
    ) -> Result<AgentDirectorySnapshot, PeerCollaborationError> {
        validate_selector(selector)?;
        let state = self.load_state()?;
        let mut agents = state
            .agents
            .values()
            .filter(|entry| selector_matches(selector, entry))
            .cloned()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| {
            availability_rank(left.availability)
                .cmp(&availability_rank(right.availability))
                .then_with(|| left.identity.id.cmp(&right.identity.id))
        });
        Ok(AgentDirectorySnapshot {
            catalog_revision: state.revision,
            agents,
        })
    }

    /// Queues one typed request after resolving its target from the directory.
    ///
    /// # Errors
    ///
    /// Rejects unknown sessions, stale revisions, duplicates, loops, excess hops,
    /// rate-limit excess, invalid payloads, or storage failures.
    pub fn submit(
        &mut self,
        principal: &PeerSessionPrincipal,
        command: &SubmitCollaborationCommand,
    ) -> Result<PeerCollaborationReceipt, PeerCollaborationError> {
        validate_principal(principal)?;
        validate_submit(command)?;
        let (identity, digest) = command_identity(
            "submit",
            &principal.identity.id,
            &command.request_id,
            &(principal, command),
        )?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return decode_receipt(&receipt, true);
        }
        let now = self.now()?;
        let mut state = self.load_state()?;
        require_revision(&state, command.expected_catalog_revision)?;
        require_current_principal(&state, principal, true)?;
        if state
            .requests
            .contains_key(&command.collaboration_request_id)
        {
            return Err(invalid_state());
        }
        if state.requests.len() >= MAX_REQUESTS {
            return Err(invalid());
        }
        let target = resolve_target(&state, &command.target, &principal.identity.id)?;
        let hop_count = validate_parent_and_hop(
            &state,
            principal,
            &target.identity,
            command.parent_request_id.as_ref(),
        )?;
        enforce_rate_limit(&state, &principal.identity.id, now)?;
        enforce_duplicate(
            &state,
            &principal.identity.id,
            &target.identity.id,
            &command.payload,
        )?;
        let request = CollaborationRequest {
            id: command.collaboration_request_id.clone(),
            requester: principal.identity.clone(),
            requester_session_id: principal.session_id.clone(),
            target: target.identity,
            target_session_id: target.current_session_id,
            parent_request_id: command.parent_request_id.clone(),
            hop_count,
            payload: command.payload.clone(),
            state: CollaborationRequestState::Queued,
            result: None,
            created_at_millis: now,
            updated_at_millis: now,
        };
        state
            .requests
            .insert(command.collaboration_request_id.clone(), request);
        self.commit(
            state,
            command.expected_catalog_revision,
            identity,
            digest,
            Some(command.collaboration_request_id.clone()),
            Some(CollaborationRequestState::Queued),
        )
    }

    /// Applies one target-side delivery, acknowledgement, or completion report.
    ///
    /// # Errors
    ///
    /// Rejects foreign Agents, stale revisions, illegal transitions, mismatched
    /// result types, candidate changes, or storage failures.
    pub fn advance(
        &mut self,
        principal: &PeerSessionPrincipal,
        command: &AdvanceCollaborationCommand,
    ) -> Result<PeerCollaborationReceipt, PeerCollaborationError> {
        validate_principal(principal)?;
        validate_advance(command)?;
        let (identity, digest) = command_identity(
            "advance",
            &principal.identity.id,
            &command.request_id,
            &(principal, command),
        )?;
        if let Some(receipt) = self.storage.load_receipt(&identity, &digest)? {
            return decode_receipt(&receipt, true);
        }
        let now = self.now()?;
        let mut state = self.load_state()?;
        require_revision(&state, command.expected_catalog_revision)?;
        require_current_principal(&state, principal, true)?;
        let request = state
            .requests
            .get_mut(&command.collaboration_request_id)
            .ok_or_else(invalid_state)?;
        if request.target.id != principal.identity.id {
            return Err(unauthorized());
        }
        match &command.action {
            CollaborationAdvanceAction::Deliver
                if request.state == CollaborationRequestState::Queued =>
            {
                request.state = CollaborationRequestState::Delivered;
            }
            CollaborationAdvanceAction::Acknowledge
                if request.state == CollaborationRequestState::Delivered =>
            {
                request.state = CollaborationRequestState::Acknowledged;
            }
            CollaborationAdvanceAction::Complete { result }
                if request.state == CollaborationRequestState::Acknowledged =>
            {
                validate_result_for_payload(result, &request.payload)?;
                request.state = CollaborationRequestState::Completed;
                request.result = Some(result.clone());
            }
            _ => return Err(invalid_state()),
        }
        request.target_session_id = Some(principal.session_id.clone());
        request.updated_at_millis = now;
        let state_value = request.state;
        self.commit(
            state,
            command.expected_catalog_revision,
            identity,
            digest,
            Some(command.collaboration_request_id.clone()),
            Some(state_value),
        )
    }

    /// Reads one complete request for its source or target Agent only.
    ///
    /// # Errors
    ///
    /// Rejects foreign or stale sessions and unknown requests.
    pub fn get(
        &self,
        principal: &PeerSessionPrincipal,
        request_id: &CollaborationRequestId,
    ) -> Result<CollaborationRequest, PeerCollaborationError> {
        validate_principal(principal)?;
        validate_request_id(request_id)?;
        let state = self.load_state()?;
        require_current_principal(&state, principal, false)?;
        let request = state.requests.get(request_id).ok_or_else(invalid_state)?;
        if request.requester.id != principal.identity.id
            && request.target.id != principal.identity.id
        {
            return Err(unauthorized());
        }
        Ok(request.clone())
    }

    /// Builds the bounded Inbox/Delegated/Waiting projection for one current Agent.
    /// Candidate freshness is derived from the supplied current `WorkItem` digests.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale sessions, malformed candidate digests, or corrupt state.
    pub fn project(
        &self,
        principal: &PeerSessionPrincipal,
        current_candidates: &[(WorkItemId, Sha256Digest)],
    ) -> Result<Vec<PeerCollaborationProjection>, PeerCollaborationError> {
        validate_principal(principal)?;
        for (work_item_id, digest) in current_candidates {
            validate_id(&work_item_id.0, "wit_")?;
            validate_digest(digest)?;
        }
        let state = self.load_state()?;
        require_current_principal(&state, principal, false)?;
        let mut rows = state
            .requests
            .values()
            .filter_map(|request| {
                project_request(request, &principal.identity.id, current_candidates)
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            (left.created_at_millis, &left.request_id)
                .cmp(&(right.created_at_millis, &right.request_id))
        });
        Ok(rows)
    }

    fn now(&mut self) -> Result<u64, PeerCollaborationError> {
        let now = self.clock.now_millis().map_err(|_| {
            PeerCollaborationError::new(
                PeerCollaborationErrorKind::Storage,
                "trusted collaboration clock is unavailable",
            )
        })?;
        validate_millis(now)?;
        Ok(now)
    }

    fn load_state(&self) -> Result<PeerCollaborationState, PeerCollaborationError> {
        match self.storage.load_state(STATE_STREAM)? {
            Some(stored) => decode_state(&stored),
            None => Ok(PeerCollaborationState {
                schema: STATE_SCHEMA.to_owned(),
                revision: 0,
                agents: BTreeMap::new(),
                requests: BTreeMap::new(),
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        mut state: PeerCollaborationState,
        expected_revision: u64,
        identity: ReceiptIdentity,
        digest: Sha256Digest,
        request_id: Option<CollaborationRequestId>,
        request_state: Option<CollaborationRequestState>,
    ) -> Result<PeerCollaborationReceipt, PeerCollaborationError> {
        state.revision = next_revision(expected_revision)?;
        let receipt = PeerCollaborationReceipt {
            catalog_revision: state.revision,
            collaboration_request_id: request_id,
            state: request_state,
            idempotent_replay: false,
        };
        let event_id = format!("peer-collaboration:{}", &digest.0[7..]);
        let committed = self.storage.commit(&StateCommit::new(
            identity,
            digest,
            STATE_STREAM,
            expected_revision,
            serde_json::to_vec(&state).map_err(|_| invalid())?,
            vec![NewOutboxEvent::internal(
                event_id,
                RECEIPT_TOPIC,
                serde_json::to_vec(&receipt).map_err(|_| invalid())?,
            )],
        ))?;
        decode_receipt(&committed, false)
    }
}

fn decode_state(stored: &StoredState) -> Result<PeerCollaborationState, PeerCollaborationError> {
    let state: PeerCollaborationState =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
    if stored.stream_id != STATE_STREAM
        || state.schema != STATE_SCHEMA
        || stored.revision != state.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || state.agents.len() > MAX_AGENTS
        || state.requests.len() > MAX_REQUESTS
    {
        return Err(corrupt());
    }
    let mut sessions = BTreeSet::new();
    for (agent_id, entry) in &state.agents {
        validate_directory_entry(entry).map_err(|_| corrupt())?;
        if agent_id != &entry.identity.id
            || entry
                .current_session_id
                .as_ref()
                .is_some_and(|session| !sessions.insert(session.0.clone()))
        {
            return Err(corrupt());
        }
    }
    let mut active_digests = BTreeSet::new();
    for (request_id, request) in &state.requests {
        validate_request(request, &state).map_err(|_| corrupt())?;
        if request_id != &request.id
            || request.state != CollaborationRequestState::Completed
                && !active_digests.insert(dedupe_digest(request)?.0)
        {
            return Err(corrupt());
        }
    }
    Ok(state)
}

fn validate_directory_command(
    command: &AgentDirectoryCommand,
) -> Result<(), PeerCollaborationError> {
    validate_command_request(&command.request_id, command.expected_catalog_revision)?;
    validate_directory_entry(&AgentDirectoryEntry {
        identity: command.identity.clone(),
        current_session_id: command.current_session_id.clone(),
        availability: command.availability,
        updated_at_millis: 1,
    })
}

fn validate_directory_entry(entry: &AgentDirectoryEntry) -> Result<(), PeerCollaborationError> {
    validate_agent(&entry.identity)?;
    validate_millis(entry.updated_at_millis)?;
    match (&entry.current_session_id, entry.availability) {
        (None, AgentAvailability::Offline)
        | (Some(_), AgentAvailability::Recovering | AgentAvailability::Working) => {}
        _ => return Err(invalid()),
    }
    if let Some(session_id) = &entry.current_session_id {
        validate_id(&session_id.0, "wsn_")?;
    }
    Ok(())
}

fn validate_agent(identity: &AgentIdentity) -> Result<(), PeerCollaborationError> {
    validate_id(&identity.id, "agt_")?;
    validate_id(&identity.worker_id.0, "wrk_")?;
    validate_text(&identity.name, 200)?;
    validate_token(&identity.role, 100)?;
    validate_digest(&identity.capabilities.capability_digest)?;
    if identity.capabilities.max_concurrent_jobs <= 0
        || identity.capabilities.max_concurrent_jobs > 10_000
        || identity.capabilities.features.len() > 32
        || identity
            .capabilities
            .features
            .iter()
            .enumerate()
            .any(|(index, feature)| identity.capabilities.features[..index].contains(feature))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_principal(principal: &PeerSessionPrincipal) -> Result<(), PeerCollaborationError> {
    validate_agent(&principal.identity)?;
    validate_id(&principal.session_id.0, "wsn_")
}

fn validate_selector(selector: &AgentSelector) -> Result<(), PeerCollaborationError> {
    match selector {
        AgentSelector::Identity { name_or_role } => validate_text(name_or_role, 200),
        AgentSelector::Capability { .. } => Ok(()),
    }
}

fn validate_submit(command: &SubmitCollaborationCommand) -> Result<(), PeerCollaborationError> {
    validate_command_request(&command.request_id, command.expected_catalog_revision)?;
    validate_request_id(&command.collaboration_request_id)?;
    if let Some(parent) = &command.parent_request_id {
        validate_request_id(parent)?;
        if parent == &command.collaboration_request_id {
            return Err(loop_detected());
        }
    }
    validate_selector(&command.target)?;
    validate_payload(&command.payload)
}

fn validate_advance(command: &AdvanceCollaborationCommand) -> Result<(), PeerCollaborationError> {
    validate_command_request(&command.request_id, command.expected_catalog_revision)?;
    validate_request_id(&command.collaboration_request_id)?;
    if let CollaborationAdvanceAction::Complete { result } = &command.action {
        validate_result(result)?;
    }
    Ok(())
}

fn validate_command_request(
    request_id: &RequestId,
    revision: u64,
) -> Result<(), PeerCollaborationError> {
    validate_id(&request_id.0, "req_")?;
    if revision > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    Ok(())
}

fn validate_request_id(request_id: &CollaborationRequestId) -> Result<(), PeerCollaborationError> {
    validate_id(&request_id.0, "col_")
}

fn validate_payload(payload: &CollaborationPayload) -> Result<(), PeerCollaborationError> {
    let (text, context_refs): (Option<&str>, &Vec<CollaborationContextRef>) = match payload {
        CollaborationPayload::Ask {
            question,
            context_refs,
        }
        | CollaborationPayload::Consult {
            question,
            context_refs,
        } => (Some(question), context_refs),
        CollaborationPayload::Delegate {
            parent_work_item_id,
            delegated_work_item_id,
            objective,
            context_refs,
        } => {
            validate_id(&parent_work_item_id.0, "wit_")?;
            validate_id(&delegated_work_item_id.0, "wit_")?;
            if parent_work_item_id == delegated_work_item_id {
                return Err(invalid());
            }
            (Some(objective), context_refs)
        }
        CollaborationPayload::ReviewRequest {
            work_item_id,
            candidate_digest,
            context_refs,
        } => {
            validate_id(&work_item_id.0, "wit_")?;
            validate_digest(candidate_digest)?;
            (None, context_refs)
        }
    };
    if let Some(text) = text {
        validate_body(text)?;
    }
    if context_refs.len() > MAX_CONTEXT_REFS {
        return Err(invalid());
    }
    let mut unique = BTreeSet::new();
    for context_ref in context_refs {
        validate_context_ref(context_ref)?;
        if !unique.insert(digest_json(context_ref)?.0) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_context_ref(
    context_ref: &CollaborationContextRef,
) -> Result<(), PeerCollaborationError> {
    match context_ref {
        CollaborationContextRef::WorkItem { work_item_id }
        | CollaborationContextRef::Candidate { work_item_id, .. } => {
            validate_id(&work_item_id.0, "wit_")?;
        }
        CollaborationContextRef::ProductSession { product_session_id } => {
            validate_id(&product_session_id.0, "psn_")?;
        }
        CollaborationContextRef::Evidence { evidence_id } => {
            validate_id(&evidence_id.0, "evd_")?;
        }
    }
    if let CollaborationContextRef::Candidate {
        candidate_digest, ..
    } = context_ref
    {
        validate_digest(candidate_digest)?;
    }
    Ok(())
}

fn validate_result(result: &CollaborationResult) -> Result<(), PeerCollaborationError> {
    let (body, evidence) = match result {
        CollaborationResult::AskAnswer {
            answer,
            classification,
            evidence_refs,
        }
        | CollaborationResult::ConsultAnswer {
            answer,
            classification,
            evidence_refs,
        } => {
            if *classification == AnswerClassification::AuthoritativeFact
                && evidence_refs.is_empty()
            {
                return Err(invalid());
            }
            (answer, evidence_refs)
        }
        CollaborationResult::DelegatedWork {
            summary,
            evidence_refs,
        }
        | CollaborationResult::Review {
            summary,
            evidence_refs,
            ..
        } => (summary, evidence_refs),
    };
    validate_body(body)?;
    if evidence.len() > MAX_EVIDENCE_REFS {
        return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    for reference in evidence {
        validate_evidence(reference)?;
        if !ids.insert(reference.id.0.clone()) {
            return Err(invalid());
        }
    }
    if let CollaborationResult::Review {
        candidate_digest, ..
    } = result
    {
        validate_digest(candidate_digest)?;
    }
    Ok(())
}

fn validate_evidence(reference: &EvidenceRef) -> Result<(), PeerCollaborationError> {
    if reference.schema_version != 1
        || reference.delivery_spec_revision == 0
        || reference.delivery_spec_revision > MAX_SAFE_INTEGER
    {
        return Err(invalid());
    }
    validate_id(&reference.id.0, "evd_")?;
    validate_id(&reference.delivery_id.0, "dlv_")?;
    validate_text(&reference.delivery_spec_id.0, 128)?;
    validate_id(&reference.work_run_id.0, "wrn_")?;
    validate_token(&reference.session_binding_id.0, 128)?;
    validate_text(&reference.candidate_ref, 2_000)?;
    validate_text(&reference.source_ref, 2_000)?;
    validate_millis(reference.created_at_millis)
}

fn validate_result_for_payload(
    result: &CollaborationResult,
    payload: &CollaborationPayload,
) -> Result<(), PeerCollaborationError> {
    validate_result(result)?;
    match (payload, result) {
        (CollaborationPayload::Ask { .. }, CollaborationResult::AskAnswer { .. })
        | (CollaborationPayload::Consult { .. }, CollaborationResult::ConsultAnswer { .. })
        | (CollaborationPayload::Delegate { .. }, CollaborationResult::DelegatedWork { .. }) => {
            Ok(())
        }
        (
            CollaborationPayload::ReviewRequest {
                candidate_digest: requested,
                ..
            },
            CollaborationResult::Review {
                candidate_digest: reviewed,
                ..
            },
        ) if requested == reviewed => Ok(()),
        _ => Err(invalid_state()),
    }
}

fn validate_request(
    request: &CollaborationRequest,
    state: &PeerCollaborationState,
) -> Result<(), PeerCollaborationError> {
    validate_request_id(&request.id)?;
    validate_agent(&request.requester)?;
    validate_agent(&request.target)?;
    validate_id(&request.requester_session_id.0, "wsn_")?;
    if let Some(session_id) = &request.target_session_id {
        validate_id(&session_id.0, "wsn_")?;
    }
    validate_payload(&request.payload)?;
    validate_millis(request.created_at_millis)?;
    validate_millis(request.updated_at_millis)?;
    if request.requester.id == request.target.id
        || request.updated_at_millis < request.created_at_millis
        || request.hop_count > MAX_HOPS
        || !state.agents.contains_key(&request.requester.id)
        || !state.agents.contains_key(&request.target.id)
        || (request.state == CollaborationRequestState::Completed) != request.result.is_some()
    {
        return Err(invalid());
    }
    match (&request.parent_request_id, request.hop_count) {
        (None, 0) => {}
        (Some(parent_id), hop) if hop > 0 => {
            let parent = state.requests.get(parent_id).ok_or_else(invalid)?;
            if parent.hop_count.checked_add(1) != Some(hop)
                || parent.target.id != request.requester.id
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    if let Some(result) = &request.result {
        validate_result_for_payload(result, &request.payload)?;
    }
    Ok(())
}

fn require_current_principal(
    state: &PeerCollaborationState,
    principal: &PeerSessionPrincipal,
    require_working: bool,
) -> Result<(), PeerCollaborationError> {
    let entry = state
        .agents
        .get(&principal.identity.id)
        .ok_or_else(unauthorized)?;
    if entry.identity != principal.identity
        || entry.current_session_id.as_ref() != Some(&principal.session_id)
        || entry.availability == AgentAvailability::Offline
        || require_working && entry.availability != AgentAvailability::Working
    {
        return Err(unauthorized());
    }
    Ok(())
}

fn resolve_target(
    state: &PeerCollaborationState,
    selector: &AgentSelector,
    requester_id: &str,
) -> Result<AgentDirectoryEntry, PeerCollaborationError> {
    let mut matches = state
        .agents
        .values()
        .filter(|entry| entry.identity.id != requester_id && selector_matches(selector, entry))
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        availability_rank(left.availability)
            .cmp(&availability_rank(right.availability))
            .then_with(|| left.identity.id.cmp(&right.identity.id))
    });
    matches.into_iter().next().ok_or_else(target_not_found)
}

fn selector_matches(selector: &AgentSelector, entry: &AgentDirectoryEntry) -> bool {
    match selector {
        AgentSelector::Identity { name_or_role } => {
            entry.identity.name.eq_ignore_ascii_case(name_or_role)
                || entry.identity.role.eq_ignore_ascii_case(name_or_role)
        }
        AgentSelector::Capability { capability } => {
            entry.identity.capabilities.features.contains(capability)
        }
    }
}

const fn availability_rank(availability: AgentAvailability) -> u8 {
    match availability {
        AgentAvailability::Working => 0,
        AgentAvailability::Recovering => 1,
        AgentAvailability::Offline => 2,
    }
}

fn validate_parent_and_hop(
    state: &PeerCollaborationState,
    principal: &PeerSessionPrincipal,
    target: &AgentIdentity,
    parent_id: Option<&CollaborationRequestId>,
) -> Result<u8, PeerCollaborationError> {
    let Some(parent_id) = parent_id else {
        return Ok(0);
    };
    let parent = state.requests.get(parent_id).ok_or_else(invalid_state)?;
    if parent.target.id != principal.identity.id {
        return Err(unauthorized());
    }
    if parent.requester.id == target.id {
        return Err(loop_detected());
    }
    parent
        .hop_count
        .checked_add(1)
        .filter(|hop| *hop <= MAX_HOPS)
        .ok_or_else(loop_detected)
}

fn enforce_duplicate(
    state: &PeerCollaborationState,
    requester_id: &str,
    target_id: &str,
    payload: &CollaborationPayload,
) -> Result<(), PeerCollaborationError> {
    let candidate = digest_json(&(requester_id, target_id, payload))?;
    if state.requests.values().any(|request| {
        request.state != CollaborationRequestState::Completed
            && dedupe_digest(request).is_ok_and(|digest| digest == candidate)
    }) {
        Err(PeerCollaborationError::new(
            PeerCollaborationErrorKind::Duplicate,
            "an equivalent collaboration request is already active",
        ))
    } else {
        Ok(())
    }
}

fn enforce_rate_limit(
    state: &PeerCollaborationState,
    requester_id: &str,
    now: u64,
) -> Result<(), PeerCollaborationError> {
    let mut count = 0;
    for request in state
        .requests
        .values()
        .filter(|request| request.requester.id == requester_id)
    {
        if request.created_at_millis > now {
            return Err(corrupt());
        }
        if now - request.created_at_millis < RATE_WINDOW_MILLIS {
            count += 1;
        }
    }
    if count >= MAX_REQUESTS_PER_WINDOW {
        Err(PeerCollaborationError::new(
            PeerCollaborationErrorKind::RateLimited,
            "collaboration request rate limit was reached",
        ))
    } else {
        Ok(())
    }
}

fn dedupe_digest(request: &CollaborationRequest) -> Result<Sha256Digest, PeerCollaborationError> {
    digest_json(&(
        request.requester.id.as_str(),
        request.target.id.as_str(),
        &request.payload,
    ))
}

fn project_request(
    request: &CollaborationRequest,
    agent_id: &str,
    current_candidates: &[(WorkItemId, Sha256Digest)],
) -> Option<PeerCollaborationProjection> {
    let lane = if request.target.id == agent_id {
        PeerCollaborationLane::Inbox
    } else if request.requester.id == agent_id {
        if matches!(request.payload, CollaborationPayload::Delegate { .. }) {
            PeerCollaborationLane::Delegated
        } else {
            PeerCollaborationLane::Waiting
        }
    } else {
        return None;
    };
    let review_freshness = match &request.payload {
        CollaborationPayload::ReviewRequest {
            work_item_id,
            candidate_digest,
            ..
        } => {
            if current_candidates
                .iter()
                .any(|(current_item, current_digest)| {
                    current_item == work_item_id && current_digest == candidate_digest
                })
            {
                ReviewFreshness::Current
            } else {
                ReviewFreshness::Stale
            }
        }
        _ => ReviewFreshness::NotApplicable,
    };
    Some(PeerCollaborationProjection {
        request_id: request.id.clone(),
        kind: request_kind(&request.payload),
        state: request.state,
        lane,
        source_session_id: request.requester_session_id.clone(),
        target_session_id: request.target_session_id.clone(),
        review_freshness,
        created_at_millis: request.created_at_millis,
        updated_at_millis: request.updated_at_millis,
    })
}

const fn request_kind(payload: &CollaborationPayload) -> CollaborationRequestKind {
    match payload {
        CollaborationPayload::Ask { .. } => CollaborationRequestKind::Ask,
        CollaborationPayload::Consult { .. } => CollaborationRequestKind::Consult,
        CollaborationPayload::Delegate { .. } => CollaborationRequestKind::Delegate,
        CollaborationPayload::ReviewRequest { .. } => CollaborationRequestKind::ReviewRequest,
    }
}

fn require_revision(
    state: &PeerCollaborationState,
    expected: u64,
) -> Result<(), PeerCollaborationError> {
    if state.revision == expected {
        Ok(())
    } else {
        Err(revision_conflict())
    }
}

fn command_identity<T: Serialize>(
    operation: &str,
    agent_id: &str,
    request_id: &RequestId,
    value: &T,
) -> Result<(ReceiptIdentity, Sha256Digest), PeerCollaborationError> {
    let serialized = serde_json::to_vec(value).map_err(|_| invalid())?;
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(
            format!("winwincode.peer-collaboration.agent.v1\0{agent_id}").into_bytes(),
        )?,
        ReceiptScopeKey::from_encoded(b"winwincode.peer-collaboration.community.v1".to_vec())?,
        request_id.clone(),
    )?;
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.peer-collaboration-command.v1\0");
    hasher.update(operation.as_bytes());
    hasher.update([0]);
    hasher.update(serialized);
    Ok((
        identity,
        Sha256Digest(format!("sha256:{:x}", hasher.finalize())),
    ))
}

fn decode_receipt(
    receipt: &CommitReceipt,
    idempotent_replay: bool,
) -> Result<PeerCollaborationReceipt, PeerCollaborationError> {
    let [event] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if event.topic != RECEIPT_TOPIC {
        return Err(corrupt());
    }
    let mut decoded: PeerCollaborationReceipt =
        serde_json::from_slice(&event.payload).map_err(|_| corrupt())?;
    if decoded.catalog_revision != receipt.revision {
        return Err(corrupt());
    }
    decoded.idempotent_replay = idempotent_replay;
    Ok(decoded)
}

fn digest_json(value: &impl Serialize) -> Result<Sha256Digest, PeerCollaborationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn validate_id(value: &str, prefix: &str) -> Result<(), PeerCollaborationError> {
    if !value.starts_with(prefix)
        || value.len() <= prefix.len()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn validate_token(value: &str, max_bytes: usize) -> Result<(), PeerCollaborationError> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, max_chars: usize) -> Result<(), PeerCollaborationError> {
    if value.trim() != value
        || value.is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn validate_body(value: &str) -> Result<(), PeerCollaborationError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > 16 * 1024
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn validate_digest(value: &Sha256Digest) -> Result<(), PeerCollaborationError> {
    if value.0.len() != 71
        || !value.0.starts_with("sha256:")
        || !value.0[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn validate_millis(value: u64) -> Result<(), PeerCollaborationError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(invalid())
    } else {
        Ok(())
    }
}

fn next_revision(current: u64) -> Result<u64, PeerCollaborationError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(invalid)
}

const fn invalid() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::InvalidRequest,
        "peer collaboration request is invalid",
    )
}

const fn unauthorized() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::Unauthorized,
        "peer collaboration authority denied the request",
    )
}

const fn target_not_found() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::TargetNotFound,
        "no peer Agent matched the selector",
    )
}

const fn invalid_state() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::InvalidState,
        "peer collaboration request is in the wrong state",
    )
}

const fn loop_detected() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::LoopDetected,
        "peer collaboration loop or hop limit was detected",
    )
}

const fn revision_conflict() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::RevisionConflict,
        "peer collaboration catalog revision changed",
    )
}

const fn request_conflict() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::RequestConflict,
        "peer collaboration requestId was reused with different input",
    )
}

const fn storage_error() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::Storage,
        "peer collaboration storage operation failed",
    )
}

const fn corrupt() -> PeerCollaborationError {
    PeerCollaborationError::new(
        PeerCollaborationErrorKind::Corrupt,
        "durable peer collaboration state is corrupt",
    )
}
