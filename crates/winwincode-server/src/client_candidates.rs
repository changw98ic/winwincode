use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use rusqlite::OpenFlags;
use serde_json::Value;
use serde_json::json;
use winwincode_client_port::domain::ApplyStrategy;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerCandidateApplyPayload;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::LocalCandidateService;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_control_plane::RepositoryBindingService;
use winwincode_domain::Instant;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::LocalApplyReceiptRecord;
use winwincode_storage::LocalApplyResult;
use winwincode_storage::LocalApplySettlement;
use winwincode_storage::LocalApplyStrategy;
use winwincode_storage::LocalCandidateReceiptRecord;
use winwincode_storage::LocalCandidateReceiptState;
use winwincode_storage::OccupancyLeaseRecord;
use winwincode_storage::SqliteStorage;

use crate::client_occupancy::client_mirror_revision_view;

const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

const CANDIDATE_REF_PREFIX: &str = "refs/winwincode/candidates/";

const BRANCH_NAMESPACE: &str = "winwincode/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCandidatesConfig {
    pub apply_wait: std::time::Duration,
    pub poll_interval: std::time::Duration,
}

impl Default for ClientCandidatesConfig {
    fn default() -> Self {
        Self {
            apply_wait: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_millis(200),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientCandidatesErrorKind {
    InvalidRequest,
    ClientNotFound,
    ClientOffline,
    ClientLocked,
    OccupancyRequired,
    NotHolder,
    AccessDenied,
    CandidateNotFound,
    WrongState,
    ApplyResultTimeout,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientCandidatesError {
    kind: ClientCandidatesErrorKind,
    message: String,
}

impl ClientCandidatesError {
    #[must_use]
    pub const fn kind(&self) -> ClientCandidatesErrorKind {
        self.kind
    }

    fn new(kind: ClientCandidatesErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientCandidatesErrorKind::InvalidRequest,
            "candidate request is invalid",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientCandidatesErrorKind::Unavailable,
            "client candidate service is unavailable",
        )
    }
}

impl fmt::Display for ClientCandidatesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientCandidatesError {}

struct PreparedCommand {
    node: ClientNodeRecord,
    lease: OccupancyLeaseRecord,
    candidate: LocalCandidateReceiptRecord,
    target_branch: Option<String>,
    expected_head: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    Branch,
    Apply,
}

#[derive(Debug, Clone)]
pub struct ClientCandidatesApplication {
    data_directory: PathBuf,
    config: ClientCandidatesConfig,
}

impl ClientCandidatesApplication {
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientCandidatesConfig,
    ) -> Result<Self, ClientCandidatesError> {
        if config.apply_wait.is_zero() || config.poll_interval.is_zero() {
            return Err(ClientCandidatesError::new(
                ClientCandidatesErrorKind::InvalidRequest,
                "client candidate configuration bounds must be positive",
            ));
        }
        Ok(Self {
            data_directory: data_directory.into(),
            config: config.clone(),
        })
    }

    pub fn list(
        &self,
        user_id: &str,
        public_client_id: &str,
    ) -> Result<Value, ClientCandidatesError> {
        if !is_public_client_id(public_client_id) {
            return Err(ClientCandidatesError::invalid_request());
        }
        let mut storage = self.open_storage()?;
        let node = lookup_node(&mut storage, public_client_id)?;
        let visible = visible_binding_ids(&mut storage, user_id, &node)?;
        let candidate_ids = node_candidate_rows(&self.data_directory, &node.client_node_id)?;
        let mut candidates = Vec::with_capacity(candidate_ids.len());
        {
            let mut ledger = LocalCandidateService::new(&mut storage);
            for (receipt_id, binding_id) in candidate_ids {
                if !visible.contains(&binding_id) {
                    continue;
                }
                let record = ledger
                    .candidate_snapshot(&receipt_id)
                    .map_err(|_| ClientCandidatesError::unavailable())?
                    .ok_or_else(ClientCandidatesError::unavailable)?;
                let history = ledger
                    .apply_history_for_candidate(&receipt_id)
                    .map_err(|_| ClientCandidatesError::unavailable())?;
                candidates.push(candidate_summary(&record, &history));
            }
        }
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": node.public_client_id,
            "candidates": candidates,
        }))
    }

    pub async fn create_branch(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientCandidatesError> {
        self.branch_command(user_id, request, CommandKind::Branch)
            .await
    }

    pub async fn apply_candidate(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientCandidatesError> {
        self.branch_command(user_id, request, CommandKind::Apply)
            .await
    }

    pub fn discard_candidate(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientCandidatesError> {
        let prepared = self.prepare(user_id, request, &DiscardFields)?;
        let mut storage = self.open_storage()?;
        if prepared.candidate.state != LocalCandidateReceiptState::Discarded {
            if prepared.candidate.state.is_terminal() {
                return Err(ClientCandidatesError::new(
                    ClientCandidatesErrorKind::WrongState,
                    "an applied candidate cannot be discarded",
                ));
            }
            let history = load_history(&mut storage, &prepared.candidate)?;
            let settlement = LocalApplySettlement::try_new(
                generate_prefixed_id("lar_").map_err(|_| ClientCandidatesError::unavailable())?,
                prepared.candidate.local_candidate_receipt_id.clone(),
                prepared.node.client_node_id.clone(),
                prepared.candidate.repository_binding_id.clone(),
                prepared.candidate.candidate_ref.clone(),
                branch_of(&history).unwrap_or_else(|| prepared.candidate.candidate_ref.clone()),
                prepared.candidate.candidate_commit.clone(),
                LocalApplyStrategy::CreateBranch,
                LocalApplyResult::Discarded,
                None,
                None,
            )
            .map_err(|_| ClientCandidatesError::unavailable())?;
            let mut ledger = LocalCandidateService::new(&mut storage);
            ledger
                .record_apply_result(&settlement, &now_instant())
                .map_err(|_| ClientCandidatesError::unavailable())?;
        }
        let candidate = load_candidate(&mut storage, &prepared.candidate)?;
        let history = load_history(&mut storage, &candidate)?;
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": prepared.node.public_client_id,
            "candidate": candidate_summary(&candidate, &history),
        }))
    }

    #[allow(clippy::too_many_lines)]
    async fn branch_command(
        &self,
        user_id: &str,
        request: &Value,
        kind: CommandKind,
    ) -> Result<Value, ClientCandidatesError> {
        let prepared = match kind {
            CommandKind::Branch => self.prepare(user_id, request, &BranchFields)?,
            CommandKind::Apply => self.prepare(user_id, request, &ApplyFields)?,
        };
        let strategy = match kind {
            CommandKind::Branch => LocalApplyStrategy::CreateBranch,
            CommandKind::Apply => LocalApplyStrategy::Merge,
        };
        let (target_branch, expected_head) = match kind {
            CommandKind::Branch => (
                requested_branch_name(&prepared.candidate),
                prepared.candidate.candidate_commit.clone(),
            ),
            CommandKind::Apply => (
                prepared
                    .target_branch
                    .clone()
                    .ok_or_else(ClientCandidatesError::invalid_request)?,
                prepared
                    .expected_head
                    .clone()
                    .ok_or_else(ClientCandidatesError::invalid_request)?,
            ),
        };

        let mut storage = self.open_storage()?;
        let history = load_history(&mut storage, &prepared.candidate)?;
        if let Some(replay) =
            idempotent_replay(&prepared, &history, kind, &target_branch, &expected_head)
        {
            return Ok(replay);
        }
        if prepared.candidate.state.is_terminal() {
            return Err(ClientCandidatesError::new(
                ClientCandidatesErrorKind::WrongState,
                "the candidate lifecycle already ended",
            ));
        }

        let known: HashSet<String> = history
            .iter()
            .map(|receipt| receipt.local_apply_receipt_id.clone())
            .collect();
        let mirror_revision =
            client_mirror_revision_view(&self.data_directory, &prepared.node.client_node_id)
                .map_err(|_| ClientCandidatesError::unavailable())?;
        enqueue_apply_command(
            &mut storage,
            &prepared.node,
            &prepared.lease,
            &prepared.candidate,
            &target_branch,
            &expected_head,
            strategy,
            user_id,
            mirror_revision,
        )?;

        let deadline = tokio::time::Instant::now() + self.config.apply_wait;
        loop {
            if let Some(outcome) = self.poll_settlement(&prepared, &known, kind)? {
                return outcome;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientCandidatesError::new(
                    ClientCandidatesErrorKind::ApplyResultTimeout,
                    "the device did not settle the candidate command in time",
                ));
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    fn poll_settlement(
        &self,
        prepared: &PreparedCommand,
        known: &HashSet<String>,
        kind: CommandKind,
    ) -> Result<Option<Result<Value, ClientCandidatesError>>, ClientCandidatesError> {
        let mut storage = self.open_storage()?;
        let candidate = load_candidate(&mut storage, &prepared.candidate)?;
        let history = load_history(&mut storage, &candidate)?;
        let Some(newest) = history
            .iter()
            .rev()
            .find(|receipt| !known.contains(&receipt.local_apply_receipt_id))
            .cloned()
        else {
            return Ok(None);
        };
        let outcome = match kind {
            CommandKind::Branch => {
                if newest.result == LocalApplyResult::BranchCreated {
                    Ok(json!({
                        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                        "clientId": prepared.node.public_client_id,
                        "candidate": candidate_summary(&candidate, &history),
                        "branchName": newest.target_branch,
                    }))
                } else {
                    Err(ClientCandidatesError::new(
                        ClientCandidatesErrorKind::WrongState,
                        "the device did not create the candidate branch",
                    ))
                }
            }
            CommandKind::Apply => Ok(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "clientId": prepared.node.public_client_id,
                "receipt": apply_receipt_json(&newest),
            })),
        };
        Ok(Some(outcome))
    }

    fn prepare(
        &self,
        user_id: &str,
        request: &Value,
        shape: &dyn RequestShape,
    ) -> Result<PreparedCommand, ClientCandidatesError> {
        let parsed = shape.parse(request)?;
        let mut storage = self.open_storage()?;
        let node = lookup_node(&mut storage, &parsed.client_id)?;
        require_online(&node)?;
        let lease = require_holder_lease(&mut storage, &node, user_id)?;
        require_binding_visible(&mut storage, user_id, &node, &parsed.repository_binding_id)?;
        let candidate = {
            let mut ledger = LocalCandidateService::new(&mut storage);
            ledger
                .candidate_for_ref(&node.client_node_id, &parsed.candidate_ref)
                .map_err(|_| ClientCandidatesError::unavailable())?
                .ok_or_else(|| {
                    ClientCandidatesError::new(
                        ClientCandidatesErrorKind::CandidateNotFound,
                        "no retained candidate matches the requested reference",
                    )
                })?
        };
        Ok(PreparedCommand {
            node,
            lease,
            candidate,
            target_branch: parsed.target_branch,
            expected_head: parsed.expected_head,
        })
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientCandidatesError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| ClientCandidatesError::unavailable())
    }
}

fn idempotent_replay(
    prepared: &PreparedCommand,
    history: &[LocalApplyReceiptRecord],
    kind: CommandKind,
    target_branch: &str,
    expected_head: &str,
) -> Option<Value> {
    match kind {
        CommandKind::Branch => {
            let branch_name = branch_of(history)?;
            Some(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "clientId": prepared.node.public_client_id,
                "candidate": candidate_summary(&prepared.candidate, history),
                "branchName": branch_name,
            }))
        }
        CommandKind::Apply => {
            let receipt = history.iter().find(|receipt| {
                receipt.result == LocalApplyResult::Applied
                    && receipt.strategy == LocalApplyStrategy::Merge
                    && receipt.target_branch == target_branch
                    && receipt.expected_head == expected_head
            })?;
            Some(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "clientId": prepared.node.public_client_id,
                "receipt": apply_receipt_json(receipt),
            }))
        }
    }
}

#[derive(Debug)]
struct ParsedRequest {
    client_id: String,
    candidate_ref: String,
    repository_binding_id: String,
    target_branch: Option<String>,
    expected_head: Option<String>,
}

trait RequestShape {
    fn parse(&self, request: &Value) -> Result<ParsedRequest, ClientCandidatesError>;
}

struct BranchFields;

struct DiscardFields;

struct ApplyFields;

impl RequestShape for BranchFields {
    fn parse(&self, request: &Value) -> Result<ParsedRequest, ClientCandidatesError> {
        let fields = object_fields(request, 4)?;
        Ok(ParsedRequest {
            client_id: required_client_id(fields.get("clientId"))?,
            candidate_ref: required_candidate_ref(fields.get("candidateRef"))?,
            repository_binding_id: required_binding_id(fields.get("repositoryBindingId"))?,
            target_branch: None,
            expected_head: None,
        })
    }
}

impl RequestShape for DiscardFields {
    fn parse(&self, request: &Value) -> Result<ParsedRequest, ClientCandidatesError> {
        BranchFields.parse(request)
    }
}

impl RequestShape for ApplyFields {
    fn parse(&self, request: &Value) -> Result<ParsedRequest, ClientCandidatesError> {
        let fields = object_fields(request, 6)?;
        Ok(ParsedRequest {
            client_id: required_client_id(fields.get("clientId"))?,
            candidate_ref: required_candidate_ref(fields.get("candidateRef"))?,
            repository_binding_id: required_binding_id(fields.get("repositoryBindingId"))?,
            target_branch: Some(required_target_branch(fields.get("targetBranch"))?),
            expected_head: Some(required_expected_head(fields.get("expectedHead"))?),
        })
    }
}

fn object_fields(
    request: &Value,
    expected_fields: usize,
) -> Result<&serde_json::Map<String, Value>, ClientCandidatesError> {
    let fields = request
        .as_object()
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    if fields.len() != expected_fields {
        return Err(ClientCandidatesError::invalid_request());
    }
    Ok(fields)
}

fn required_client_id(value: Option<&Value>) -> Result<String, ClientCandidatesError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    if is_public_client_id(text) {
        Ok(text.to_owned())
    } else {
        Err(ClientCandidatesError::invalid_request())
    }
}

fn required_candidate_ref(value: Option<&Value>) -> Result<String, ClientCandidatesError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    let Some(suffix) = text.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(ClientCandidatesError::invalid_request());
    };
    let shaped = !suffix.is_empty()
        && suffix.len() <= 200
        && suffix.as_bytes()[0].is_ascii_alphanumeric()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if shaped {
        Ok(text.to_owned())
    } else {
        Err(ClientCandidatesError::invalid_request())
    }
}

fn required_binding_id(value: Option<&Value>) -> Result<String, ClientCandidatesError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    let shaped = !text.is_empty() && text.len() <= 96;
    if shaped {
        Ok(text.to_owned())
    } else {
        Err(ClientCandidatesError::invalid_request())
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn required_target_branch(value: Option<&Value>) -> Result<String, ClientCandidatesError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    let shaped = !text.is_empty()
        && text.len() <= 200
        && text.as_bytes()[0].is_ascii_alphanumeric()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        && !text.contains("..")
        && !text.ends_with('/')
        && !text.ends_with(".lock");
    if shaped {
        Ok(text.to_owned())
    } else {
        Err(ClientCandidatesError::invalid_request())
    }
}

fn required_expected_head(value: Option<&Value>) -> Result<String, ClientCandidatesError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientCandidatesError::invalid_request)?;
    let shaped = (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if shaped {
        Ok(text.to_owned())
    } else {
        Err(ClientCandidatesError::invalid_request())
    }
}

fn is_public_client_id(value: &str) -> bool {
    (9..=12).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn lookup_node(
    storage: &mut SqliteStorage,
    public_client_id: &str,
) -> Result<ClientNodeRecord, ClientCandidatesError> {
    let mut registry = ClientRegistryService::new(storage);
    let record = registry
        .snapshot_by_public_client_id(public_client_id)
        .map_err(|_| ClientCandidatesError::unavailable())?;
    match record {
        None
        | Some(ClientNodeRecord {
            presence_state: ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked,
            ..
        }) => Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::ClientNotFound,
            "no client matches the requested id",
        )),
        Some(node) => Ok(node),
    }
}

fn require_online(node: &ClientNodeRecord) -> Result<(), ClientCandidatesError> {
    if matches!(
        node.presence_state,
        ClientPresenceState::Offline | ClientPresenceState::Degraded
    ) {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::ClientOffline,
            "the client is not online",
        ));
    }
    if node.presence_state == ClientPresenceState::Locked {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::ClientLocked,
            "the client is locked",
        ));
    }
    Ok(())
}

fn require_holder_lease(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    user_id: &str,
) -> Result<OccupancyLeaseRecord, ClientCandidatesError> {
    let lease = {
        let mut occupancy = ClientOccupancyService::new(storage);
        occupancy
            .active_lease_for_node(&node.client_node_id)
            .map_err(|_| ClientCandidatesError::unavailable())?
    };
    let Some(lease) = lease else {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::OccupancyRequired,
            "the client is not occupied; claim occupancy before candidate operations",
        ));
    };
    if lease.holder_user_id != user_id {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::NotHolder,
            "only the occupancy holder may operate on candidates",
        ));
    }
    if !matches!(
        lease.state,
        OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
    ) {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::OccupancyRequired,
            "the occupancy is not confirmed by the device",
        ));
    }
    Ok(lease)
}

fn require_binding_visible(
    storage: &mut SqliteStorage,
    user_id: &str,
    node: &ClientNodeRecord,
    repository_binding_id: &str,
) -> Result<(), ClientCandidatesError> {
    let visible = visible_binding_ids(storage, user_id, node)?;
    if !visible.contains(repository_binding_id) {
        return Err(ClientCandidatesError::new(
            ClientCandidatesErrorKind::AccessDenied,
            "the repository binding is not visible to the caller",
        ));
    }
    Ok(())
}

fn visible_binding_ids(
    storage: &mut SqliteStorage,
    user_id: &str,
    node: &ClientNodeRecord,
) -> Result<HashSet<String>, ClientCandidatesError> {
    let mut bindings = RepositoryBindingService::new(storage);
    Ok(bindings
        .visible_bindings(user_id, &node.client_node_id)
        .map_err(|_| ClientCandidatesError::unavailable())?
        .into_iter()
        .map(|record| record.repository_binding_id)
        .collect())
}

fn load_candidate(
    storage: &mut SqliteStorage,
    candidate: &LocalCandidateReceiptRecord,
) -> Result<LocalCandidateReceiptRecord, ClientCandidatesError> {
    let mut ledger = LocalCandidateService::new(storage);
    ledger
        .candidate_snapshot(&candidate.local_candidate_receipt_id)
        .map_err(|_| ClientCandidatesError::unavailable())?
        .ok_or_else(ClientCandidatesError::unavailable)
}

fn load_history(
    storage: &mut SqliteStorage,
    candidate: &LocalCandidateReceiptRecord,
) -> Result<Vec<LocalApplyReceiptRecord>, ClientCandidatesError> {
    let mut ledger = LocalCandidateService::new(storage);
    ledger
        .apply_history_for_candidate(&candidate.local_candidate_receipt_id)
        .map_err(|_| ClientCandidatesError::unavailable())
}

fn branch_of(history: &[LocalApplyReceiptRecord]) -> Option<String> {
    history
        .iter()
        .find(|receipt| receipt.result == LocalApplyResult::BranchCreated)
        .map(|receipt| receipt.target_branch.clone())
}

fn requested_branch_name(candidate: &LocalCandidateReceiptRecord) -> String {
    let short = candidate.candidate_commit.get(..7).unwrap_or_default();
    format!("{BRANCH_NAMESPACE}candidate-{short}")
}

#[allow(clippy::too_many_arguments)]
fn enqueue_apply_command(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    lease: &OccupancyLeaseRecord,
    candidate: &LocalCandidateReceiptRecord,
    target_branch: &str,
    expected_head: &str,
    strategy: LocalApplyStrategy,
    user_id: &str,
    mirror_revision: u64,
) -> Result<(), ClientCandidatesError> {
    let instance = node
        .current_instance_id
        .clone()
        .ok_or_else(ClientCandidatesError::unavailable)?;
    let now = now_instant();
    let wire_strategy = match strategy {
        LocalApplyStrategy::CreateBranch => ApplyStrategy::CreateBranch,
        LocalApplyStrategy::FastForward => ApplyStrategy::FastForward,
        LocalApplyStrategy::CherryPick => ApplyStrategy::CherryPick,
        LocalApplyStrategy::Merge => ApplyStrategy::Merge,
    };
    let message = ServerToClientMessage::CandidateApply(ServerCandidateApplyPayload {
        occupancy: occupancy_stamp(lease, mirror_revision),
        candidate_ref: candidate.candidate_ref.clone(),
        repository_binding_id: candidate.repository_binding_id.clone(),
        target_branch: target_branch.to_owned(),
        expected_head: expected_head.to_owned(),
        requester_user_id: user_id.to_owned(),
        strategy: wire_strategy,
    });
    let cursors = {
        let mut registry = ClientRegistryService::new(storage);
        registry
            .exchange_cursors(&node.client_node_id)
            .map_err(|_| ClientCandidatesError::unavailable())?
            .ok_or_else(ClientCandidatesError::unavailable)?
    };
    let mut downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientCandidatesError::unavailable())?;
    let outbox_high_water = downlink
        .high_water(&node.client_node_id)
        .map_err(|_| ClientCandidatesError::unavailable())?;
    let sequence = cursors
        .server_to_client_ack_sequence
        .max(outbox_high_water)
        .checked_add(1)
        .ok_or_else(ClientCandidatesError::unavailable)?;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("msg_")
            .map_err(|_| ClientCandidatesError::unavailable())?,
        client_node_id: node.client_node_id.clone(),
        client_instance_id: instance,
        sequence,
        occurred_at: now.0.clone(),
        message,
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let stored = codec
        .encode_envelope(&envelope)
        .map_err(|_| ClientCandidatesError::unavailable())?;
    let frame = std::str::from_utf8(&stored.frame)
        .map_err(|_| ClientCandidatesError::unavailable())?
        .to_owned();
    downlink
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id.clone(),
                envelope.message_id.clone(),
                sequence,
                frame,
            )
            .map_err(|_| ClientCandidatesError::unavailable())?,
            &now,
        )
        .map_err(|_| ClientCandidatesError::unavailable())?;
    Ok(())
}

fn occupancy_stamp(lease: &OccupancyLeaseRecord, mirror_revision: u64) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision: mirror_revision,
            idempotency_key: format!(
                "idem_candidate_apply_{}",
                generate_prefixed_id("att_").unwrap_or_else(|_| "att_fallback".to_owned())
            ),
        },
        occupancy_lease_id: lease.occupancy_lease_id.clone(),
        occupancy_fencing_token: lease.fencing_token,
    }
}

fn candidate_summary(
    record: &LocalCandidateReceiptRecord,
    history: &[LocalApplyReceiptRecord],
) -> Value {
    json!({
        "localCandidateReceiptId": record.local_candidate_receipt_id,
        "candidateRef": record.candidate_ref,
        "repositoryBindingId": record.repository_binding_id,
        "candidateCommit": record.candidate_commit,
        "localRefName": record.local_ref_name,
        "state": record.state.as_str(),
        "createdAt": record.created_at.0,
        "revision": record.revision,
        "branchName": branch_of(history),
        "history": history.iter().map(apply_receipt_json).collect::<Vec<_>>(),
    })
}

fn apply_receipt_json(receipt: &LocalApplyReceiptRecord) -> Value {
    json!({
        "localApplyReceiptId": receipt.local_apply_receipt_id,
        "candidateRef": receipt.candidate_ref,
        "repositoryBindingId": receipt.repository_binding_id,
        "targetBranch": receipt.target_branch,
        "expectedHead": receipt.expected_head,
        "strategy": receipt.strategy.as_str(),
        "result": receipt.result.as_str(),
        "resultingCommit": receipt.resulting_commit,
        "conflictArtifactRef": receipt.conflict_artifact_ref,
        "createdAt": receipt.created_at.0,
        "revision": receipt.revision,
    })
}

fn node_candidate_rows(
    data_directory: &Path,
    client_node_id: &str,
) -> Result<Vec<(String, String)>, ClientCandidatesError> {
    let database = data_directory.join("control-plane.sqlite3");
    if !database.exists() {
        return Ok(Vec::new());
    }
    let connection =
        rusqlite::Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| ClientCandidatesError::unavailable())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| ClientCandidatesError::unavailable())?;
    let read = || -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = connection.prepare(
            "SELECT local_candidate_receipt_id, repository_binding_id
             FROM local_candidate_receipts
             WHERE client_node_id = ?1
             ORDER BY created_at, local_candidate_receipt_id",
        )?;
        let rows = statement
            .query_map([client_node_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    match read() {
        Ok(rows) => Ok(rows),
        Err(sql) if is_no_such_table(&sql) => Ok(Vec::new()),
        Err(_) => Err(ClientCandidatesError::unavailable()),
    }
}

fn is_no_such_table(sql: &rusqlite::Error) -> bool {
    matches!(sql, rusqlite::Error::SqliteFailure(_, Some(message))
        if message.contains("no such table"))
}

fn now_instant() -> Instant {
    use crate::application::StandaloneApplicationClock as _;
    crate::application::SystemStandaloneApplicationClock.now_instant()
}

const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn generate_prefixed_id(prefix: &str) -> Result<String, ClientCandidatesError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientCandidatesError::unavailable())?;
    let mut identity = String::with_capacity(prefix.len() + 26);
    identity.push_str(prefix);
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_storage::LocalCandidateRetained;

    fn canonical_node(suffix_digit: char) -> String {
        format!("cnd_{suffix_digit}{}", "A".repeat(25))
    }

    fn canonical_id(prefix: &str) -> String {
        format!("{prefix}{}1", "A".repeat(25))
    }

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn record() -> LocalCandidateReceiptRecord {
        LocalCandidateReceiptRecord {
            local_candidate_receipt_id: canonical_id("lcr_"),
            client_node_id: canonical_node('A'),
            repository_binding_id: canonical_id("rbd_"),
            candidate_ref: "refs/winwincode/candidates/0f9e8d7".to_owned(),
            candidate_commit: "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776".to_owned(),
            local_ref_name: "refs/winwincode/candidates/0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776"
                .to_owned(),
            state: LocalCandidateReceiptState::Retained,
            created_at: instant("2026-09-04T12:00:00.000Z"),
            revision: 1,
        }
    }

    #[test]
    fn config_rejects_zero_bounds() {
        let mut config = ClientCandidatesConfig::default();
        assert!(ClientCandidatesApplication::open("unused", &config).is_ok());
        config.apply_wait = std::time::Duration::ZERO;
        assert!(ClientCandidatesApplication::open("unused", &config).is_err());
    }

    #[test]
    fn generated_ids_carry_the_candidate_prefixes() {
        for prefix in ["lar_", "att_", "msg_"] {
            let id = generate_prefixed_id(prefix).expect("entropy");
            assert_eq!(id.len(), prefix.len() + 26);
            assert!(id.starts_with(prefix));
        }
    }

    #[test]
    fn public_client_id_shape_is_nine_to_twelve_digits() {
        assert!(is_public_client_id("927351842"));
        assert!(!is_public_client_id("12345678"));
        assert!(!is_public_client_id("1234567890123"));
        assert!(!is_public_client_id("12345678a"));
    }

    #[test]
    fn request_shapes_enforce_the_exact_facade_fields() {
        let branch = json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": "927351842",
            "candidateRef": "refs/winwincode/candidates/0f9e8d7",
            "repositoryBindingId": "rbd_1",
        });
        let parsed = BranchFields.parse(&branch).expect("branch shape");
        assert_eq!(parsed.client_id, "927351842");
        assert_eq!(parsed.candidate_ref, "refs/winwincode/candidates/0f9e8d7");
        assert!(parsed.target_branch.is_none());

        let mut extra = branch.clone();
        extra["extra"] = json!(1);
        assert_eq!(
            BranchFields.parse(&extra).expect_err("extra field").kind(),
            ClientCandidatesErrorKind::InvalidRequest
        );
        assert_eq!(
            ApplyFields
                .parse(&branch)
                .expect_err("missing apply fields")
                .kind(),
            ClientCandidatesErrorKind::InvalidRequest
        );

        let foreign = json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": "927351842",
            "candidateRef": "refs/heads/main",
            "repositoryBindingId": "rbd_1",
        });
        assert_eq!(
            BranchFields
                .parse(&foreign)
                .expect_err("foreign ref")
                .kind(),
            ClientCandidatesErrorKind::InvalidRequest
        );
    }

    #[test]
    fn apply_fields_require_a_branch_shaped_target_and_full_head() {
        let base = json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "clientId": "927351842",
            "candidateRef": "refs/winwincode/candidates/0f9e8d7",
            "repositoryBindingId": "rbd_1",
        });
        let good = |branch: &str, head: &str| {
            let mut value = base.clone();
            value["targetBranch"] = json!(branch);
            value["expectedHead"] = json!(head);
            ApplyFields.parse(&value)
        };
        let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
        assert_eq!(
            good("feature/x-1.0", commit)
                .expect("valid apply")
                .target_branch
                .expect("branch"),
            "feature/x-1.0"
        );
        for branch in ["", "-flag", "a..b", "ends/", "x.lock", "has space"] {
            assert_eq!(
                good(branch, commit).expect_err(branch).kind(),
                ClientCandidatesErrorKind::InvalidRequest,
                "{branch}"
            );
        }
        for head in ["0f9e8d7", "0F9E8D7C6B5A4938271605F4E3D2C1B0A9988776", "zz"] {
            assert_eq!(
                good("main", head).expect_err(head).kind(),
                ClientCandidatesErrorKind::InvalidRequest,
                "{head}"
            );
        }
    }

    #[test]
    fn the_requested_branch_name_is_deterministic_inside_the_namespace() {
        let mut candidate = record();
        assert_eq!(
            requested_branch_name(&candidate),
            "winwincode/candidate-0f9e8d7"
        );
        candidate.candidate_commit = "a".repeat(64);
        assert_eq!(
            requested_branch_name(&candidate),
            "winwincode/candidate-aaaaaaa"
        );
    }

    #[test]
    fn branch_names_come_from_the_branch_created_receipt() {
        let history = Vec::new();
        assert_eq!(branch_of(&history), None);
        assert!(
            json!({"branchName": serde_json::to_value(branch_of(&history)).unwrap()})["branchName"]
                .is_null()
        );
    }

    #[test]
    fn node_candidate_rows_tolerate_a_missing_ledger() {
        let directory = std::env::temp_dir().join(format!(
            "candidates-missing-{}-{}",
            std::process::id(),
            getrandom::u64().expect("entropy")
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("directory");
        let node = canonical_node('B');
        assert!(
            node_candidate_rows(&directory, &node)
                .expect("empty projection")
                .is_empty()
        );

        let connection =
            rusqlite::Connection::open(directory.join("control-plane.sqlite3")).expect("database");
        connection
            .execute("CREATE TABLE other (id INTEGER)", [])
            .expect("table");
        drop(connection);
        assert!(
            node_candidate_rows(&directory, &node)
                .expect("empty projection without the table")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn only_a_missing_table_reads_as_an_empty_projection() {
        assert!(is_no_such_table(&rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("no such table: local_candidate_receipts".to_owned()),
        )));
        assert!(!is_no_such_table(&rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_owned()),
        )));
    }

    #[test]
    fn retained_commands_validate_before_any_ledger_write() {
        let command = LocalCandidateRetained::try_new(
            canonical_id("lcr_"),
            canonical_node('A'),
            canonical_id("rbd_"),
            "refs/winwincode/candidates/0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776",
            "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776",
            "refs/winwincode/candidates/0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776",
        );
        assert!(command.is_ok());
        let wrong = LocalCandidateRetained::try_new(
            "lcr_not-canonical",
            canonical_node('A'),
            canonical_id("rbd_"),
            "refs/winwincode/candidates/x",
            "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776",
            "refs/winwincode/candidates/x",
        );
        assert!(wrong.is_err());
    }
}
