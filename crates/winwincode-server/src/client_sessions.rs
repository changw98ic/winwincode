// SPDX-License-Identifier: Apache-2.0

//! User-facing Worker launch flow over the durable `WorkerLaunchGrant`
//! ledger (plan 14.2-14.3, 17.2, contract `client-control-port-v1.md`
//! `client.worker.launch` / `client.worker.launch_ack` / `client.worker.stop`).
//!
//! `POST /api/v1/sessions` is the launch entry the signed-in occupancy holder
//! uses to start one `WorkerSession` on the Client they occupy. The flow
//! validates the durable preconditions (`WorkerLaunchGrantService::issue`:
//! the caller is the lease holder, the lease is `occupied` or `draining`,
//! the binding belongs to the leased Client and is visible to the holder,
//! and a worker-session slot is free), mints the worker identities and a
//! 32-byte one-time worker credential (only the `sha256:` digest is
//! persisted), enqueues the `client.worker.launch` downlink frame with every
//! `C + L` field into the durable outbox, and waits a bounded, configurable
//! interval for the Device Client's `client.worker.launch_ack` to be settled
//! by the client exchange (`settle_launch_ack`). An accepted acknowledgement
//! consumes the grant exactly once; a rejection keeps it `issued` with the
//! reason in the launch audit trail.
//!
//! `worker_stop_message` is the shared `client.worker.stop` construction
//! helper: the supervisor and release flows stamp the same occupancy
//! fencing context so the device rejects any stop carrying a stale token.

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use winwincode_client_port::domain::ClientWorkerStopReason;
use winwincode_client_port::domain::WorkerLaunchGrant;
use winwincode_client_port::domain::WorkerLaunchGrantState as WireGrantState;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_client_port::messages::ServerWorkerLaunchPayload;
use winwincode_client_port::messages::ServerWorkerStopPayload;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::LaunchGrantState;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_control_plane::WorkerLaunchGrantService;
use winwincode_control_plane::WorkerLaunchGrantServiceErrorKind;
use winwincode_domain::Instant;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::SqliteStorage;

use crate::client_occupancy::client_mirror_revision_view;
use crate::client_occupancy::offset_instant;

/// Schema version of the public browser-facing launch surface.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Bounded-wait and credential policy of the launch flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSessionsConfig {
    /// How long one launch waits for the Device Client launch acknowledgement
    /// before failing (plan 14.3, bounded step 10).
    pub launch_wait: std::time::Duration,
    /// How often the durable grant state is polled while waiting.
    pub poll_interval: std::time::Duration,
    /// Time-to-live of one issued launch grant; an unanswered grant expires
    /// at `issuedAt + ttl` and can no longer be consumed.
    pub grant_ttl: std::time::Duration,
}

impl Default for ClientSessionsConfig {
    fn default() -> Self {
        Self {
            launch_wait: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_millis(200),
            grant_ttl: std::time::Duration::from_mins(2),
        }
    }
}

/// Stable failure categories of the launch flow boundary. Each category maps
/// to exactly one wire error code of the central launch error-code table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientSessionsErrorKind {
    /// The request body violated the launch contract.
    InvalidRequest,
    /// The public Client ID does not name a launchable Client.
    ClientNotFound,
    /// The Client is not reachable (offline or degraded).
    ClientOffline,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// The occupancy lease belongs to another user.
    NotHolder,
    /// The Client has no usable occupancy (none, unconfirmed, or pending
    /// recovery).
    OccupancyRequired,
    /// The binding is unknown, foreign, or invisible to the holder.
    BindingNotVisible,
    /// The Client has no free worker-session slot.
    CapacityExhausted,
    /// The grant expired before the device answered.
    GrantExpired,
    /// The Device Client rejected the launch, or the grant was revoked
    /// while waiting.
    LaunchRejected,
    /// The Device Client did not answer within the bounded wait; the grant
    /// stays `issued` until its expiry.
    LaunchAckTimeout,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free launch flow failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientSessionsError {
    kind: ClientSessionsErrorKind,
    message: String,
}

impl ClientSessionsError {
    #[must_use]
    pub const fn kind(&self) -> ClientSessionsErrorKind {
        self.kind
    }

    fn new(kind: ClientSessionsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientSessionsErrorKind::InvalidRequest,
            "launch request must carry a 9-12 digit clientId and a repositoryBindingId",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientSessionsErrorKind::Unavailable,
            "client session launch service is unavailable",
        )
    }
}

impl fmt::Display for ClientSessionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientSessionsError {}

/// What one validated launch prepared before the bounded wait.
struct PreparedLaunch {
    node: ClientNodeRecord,
    occupancy_lease_id: String,
    occupancy_fencing_token: u64,
    grant: winwincode_storage::WorkerLaunchGrantRecord,
    /// Raw one-time worker credential material (lowercase hex); it crosses
    /// the launch response once and never enters durable state.
    worker_credential: String,
    product_session_id: String,
    stage_run_id: String,
}

/// The outcome of one poll of a pending launch grant.
enum PollOutcome {
    /// The grant is still `issued`; keep waiting.
    Pending,
    /// The grant was consumed; the flow returns the `201` body.
    Consumed,
    /// The flow failed with the mapped domain error.
    Failed(ClientSessionsError),
}

/// The signed-in user's Worker launch surface over the Server's one
/// product-state database directory. Like the connect and occupancy flows,
/// every operation opens and closes its own storage connection so concurrent
/// flows never share state in memory and the bounded wait holds no database
/// lock.
#[derive(Debug, Clone)]
pub struct ClientSessionsApplication {
    data_directory: PathBuf,
    config: ClientSessionsConfig,
}

impl ClientSessionsApplication {
    /// Composes the launch application over one product-state directory.
    ///
    /// # Errors
    ///
    /// Fails when the configuration violates its bounds.
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientSessionsConfig,
    ) -> Result<Self, ClientSessionsError> {
        if config.launch_wait.is_zero()
            || config.poll_interval.is_zero()
            || config.grant_ttl.is_zero()
        {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::InvalidRequest,
                "client session configuration bounds must be positive",
            ));
        }
        Ok(Self {
            data_directory: data_directory.into(),
            config: config.clone(),
        })
    }

    /// Runs the full launch flow (plan 14.3, steps 3-5 with the bounded
    /// device acknowledgement) and resolves to the `201` session body.
    ///
    /// # Errors
    ///
    /// Returns the stable launch failure categories; `LaunchAckTimeout`
    /// leaves the grant `issued` (it expires at its deadline), and
    /// `LaunchRejected` covers a Device Client that answered with a
    /// rejection.
    pub async fn launch(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientSessionsError> {
        let prepared = self.prepare(user_id, request)?;
        let grant_id = prepared.grant.worker_launch_grant_id.clone();
        let deadline = tokio::time::Instant::now() + self.config.launch_wait;
        loop {
            match self.poll(&grant_id)? {
                PollOutcome::Pending => {}
                PollOutcome::Consumed => return Ok(session_body(&prepared)),
                PollOutcome::Failed(error) => return Err(error),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::LaunchAckTimeout,
                    "the device did not acknowledge the worker launch in time",
                ));
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Validates the request and durable preconditions, issues the launch
    /// grant, and enqueues the `client.worker.launch` downlink frame.
    #[allow(clippy::too_many_lines)]
    fn prepare(
        &self,
        user_id: &str,
        request: &Value,
    ) -> Result<PreparedLaunch, ClientSessionsError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientSessionsError::invalid_request());
        };
        if fields.len() != 3 {
            return Err(ClientSessionsError::invalid_request());
        }
        let public_client_id = required_client_id(fields.get("clientId"))?;
        let repository_binding_id = fields
            .get("repositoryBindingId")
            .and_then(Value::as_str)
            .ok_or_else(ClientSessionsError::invalid_request)?
            .to_owned();

        let mut storage = self.open_storage()?;
        let node = Self::lookup_node(&mut storage, &public_client_id)?;
        let now = now_instant();

        // Occupancy: the caller must hold the node's one active lease and it
        // must be device-confirmed (`occupied` or `draining`).
        let lease = {
            let mut occupancy = ClientOccupancyService::new(&mut storage);
            occupancy
                .active_lease_for_node(&node.client_node_id)
                .map_err(|_| ClientSessionsError::unavailable())?
        };
        let Some(lease) = lease else {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::OccupancyRequired,
                "the client is not occupied; claim occupancy before launching",
            ));
        };
        if lease.holder_user_id != user_id {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::NotHolder,
                "only the occupancy holder may launch a worker session",
            ));
        }
        if !matches!(
            lease.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            return Err(ClientSessionsError::new(
                ClientSessionsErrorKind::OccupancyRequired,
                "the occupancy is not confirmed by the device",
            ));
        }

        // Worker identities and the one-time credential (32 random bytes;
        // only the digest is persisted).
        let worker_session_id = generate_prefixed_id("ws_")?;
        let worker_id = generate_prefixed_id("wkr_")?;
        let worker_instance_id = generate_prefixed_id("winst_")?;
        let product_session_id = generate_prefixed_id("ps_")?;
        let stage_run_id = generate_prefixed_id("run_")?;
        let (worker_credential, credential_digest) = issue_worker_credential()?;
        let expires_at = offset_instant(&now, duration_millis(self.config.grant_ttl))
            .ok_or_else(ClientSessionsError::unavailable)?;
        let issuance = winwincode_storage::LaunchGrantIssuance::try_new(
            generate_prefixed_id("wlg_")?,
            node.client_node_id.clone(),
            node.current_instance_id
                .clone()
                .ok_or_else(ClientSessionsError::unavailable)?,
            user_id,
            lease.occupancy_lease_id.clone(),
            lease.fencing_token,
            repository_binding_id,
            worker_session_id,
            worker_id,
            worker_instance_id,
            credential_digest,
            Some(product_session_id.clone()),
            Some(stage_run_id.clone()),
            expires_at,
        )
        .map_err(|_| ClientSessionsError::unavailable())?;
        let grant = {
            let mut grants = WorkerLaunchGrantService::new(&mut storage);
            match grants.issue(&issuance, &now) {
                Ok(grant) => grant,
                Err(error) => return Err(issue_gate_error(error.kind())),
            }
        };

        // The launch command is computed against the mirror revision the
        // device last confirmed: the device refuses any other stamp.
        let mirror_revision_view =
            client_mirror_revision_view(&self.data_directory, &node.client_node_id)
                .map_err(|_| ClientSessionsError::unavailable())?;
        enqueue_frame(
            &mut storage,
            &node,
            ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
                occupancy: occupancy_stamp(
                    mirror_revision_view,
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    &format!("idem_launch_{}", grant.worker_launch_grant_id),
                ),
                launch_grant: WorkerLaunchGrant {
                    worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
                    client_node_id: node.client_node_id.clone(),
                    client_instance_id: grant.client_instance_id.clone(),
                    occupancy_lease_id: lease.occupancy_lease_id.clone(),
                    occupancy_fencing_token: lease.fencing_token,
                    repository_binding_id: grant.repository_binding_id.clone(),
                    product_session_id: grant.product_session_id.clone().unwrap_or_default(),
                    stage_run_id: grant.stage_run_id.clone().unwrap_or_default(),
                    worker_session_id: grant.worker_session_id.clone(),
                    worker_id: grant.worker_id.clone(),
                    worker_instance_id: grant.worker_instance_id.clone(),
                    credential_digest: grant.credential_digest.clone(),
                    expires_at: grant.expires_at.0.clone(),
                    state: WireGrantState::Issued,
                    revision: grant.revision,
                },
            }),
            &now,
        )?;

        Ok(PreparedLaunch {
            node,
            occupancy_lease_id: lease.occupancy_lease_id,
            occupancy_fencing_token: lease.fencing_token,
            grant,
            worker_credential,
            product_session_id,
            stage_run_id,
        })
    }

    /// Reads the durable grant state once and drives the flow to its next
    /// transition (plan 14.3 step 10). A rejection never moves the state, so
    /// the launch audit trail carries the verdict: a recorded
    /// `launch_rejected` entry fails the flow immediately instead of
    /// burning the whole bounded wait.
    fn poll(&self, worker_launch_grant_id: &str) -> Result<PollOutcome, ClientSessionsError> {
        let mut storage = self.open_storage()?;
        let mut grants = WorkerLaunchGrantService::new(&mut storage);
        let grant = grants
            .snapshot(worker_launch_grant_id)
            .map_err(|_| ClientSessionsError::unavailable())?;
        let Some(grant) = grant else {
            return Ok(PollOutcome::Failed(ClientSessionsError::unavailable()));
        };
        match grant.state {
            LaunchGrantState::Issued => {
                let rejected = grants
                    .audit_trail(worker_launch_grant_id)
                    .map_err(|_| ClientSessionsError::unavailable())?
                    .into_iter()
                    .any(|entry| entry.action.as_str() == "launch_rejected");
                if rejected {
                    Ok(PollOutcome::Failed(ClientSessionsError::new(
                        ClientSessionsErrorKind::LaunchRejected,
                        "the device rejected the worker launch",
                    )))
                } else {
                    Ok(PollOutcome::Pending)
                }
            }
            LaunchGrantState::Consumed => Ok(PollOutcome::Consumed),
            LaunchGrantState::Revoked => Ok(PollOutcome::Failed(ClientSessionsError::new(
                ClientSessionsErrorKind::LaunchRejected,
                "the launch grant was revoked before the device accepted",
            ))),
            LaunchGrantState::Expired => Ok(PollOutcome::Failed(ClientSessionsError::new(
                ClientSessionsErrorKind::GrantExpired,
                "the launch grant expired before the device accepted",
            ))),
        }
    }

    fn lookup_node(
        storage: &mut SqliteStorage,
        public_client_id: &str,
    ) -> Result<ClientNodeRecord, ClientSessionsError> {
        let mut registry = ClientRegistryService::new(storage);
        let record = registry
            .snapshot_by_public_client_id(public_client_id)
            .map_err(|_| ClientSessionsError::unavailable())?;
        match record {
            None
            | Some(ClientNodeRecord {
                presence_state:
                    ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked,
                ..
            }) => Err(ClientSessionsError::new(
                ClientSessionsErrorKind::ClientNotFound,
                "no client matches the requested id",
            )),
            Some(node)
                if matches!(
                    node.presence_state,
                    ClientPresenceState::Offline | ClientPresenceState::Degraded
                ) =>
            {
                Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::ClientOffline,
                    "the client is not online",
                ))
            }
            Some(node) if node.presence_state == ClientPresenceState::Locked => {
                Err(ClientSessionsError::new(
                    ClientSessionsErrorKind::ClientLocked,
                    "the client is locked",
                ))
            }
            Some(node) => Ok(node),
        }
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientSessionsError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| ClientSessionsError::unavailable())
    }
}

/// Maps one issue-gate failure onto the central launch error-code taxonomy.
fn issue_gate_error(kind: WorkerLaunchGrantServiceErrorKind) -> ClientSessionsError {
    match kind {
        WorkerLaunchGrantServiceErrorKind::UnknownClientNode
        | WorkerLaunchGrantServiceErrorKind::UnknownOccupancyLease => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy behind the launch is gone",
        ),
        WorkerLaunchGrantServiceErrorKind::PresenceNotOnline => ClientSessionsError::new(
            ClientSessionsErrorKind::ClientOffline,
            "the client is not online",
        ),
        WorkerLaunchGrantServiceErrorKind::ClientLocked => ClientSessionsError::new(
            ClientSessionsErrorKind::ClientLocked,
            "the client is locked",
        ),
        WorkerLaunchGrantServiceErrorKind::NotLeaseHolder => ClientSessionsError::new(
            ClientSessionsErrorKind::NotHolder,
            "only the occupancy holder may launch a worker session",
        ),
        WorkerLaunchGrantServiceErrorKind::OccupancyNotConfirmed => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy is not confirmed by the device",
        ),
        WorkerLaunchGrantServiceErrorKind::FencingTokenMismatch => ClientSessionsError::new(
            ClientSessionsErrorKind::OccupancyRequired,
            "the occupancy stamp is stale",
        ),
        WorkerLaunchGrantServiceErrorKind::UnknownRepositoryBinding
        | WorkerLaunchGrantServiceErrorKind::BindingForeignClient
        | WorkerLaunchGrantServiceErrorKind::BindingNotVisible => ClientSessionsError::new(
            ClientSessionsErrorKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ),
        WorkerLaunchGrantServiceErrorKind::CapacityExhausted => ClientSessionsError::new(
            ClientSessionsErrorKind::CapacityExhausted,
            "the client has no free worker-session slot",
        ),
        WorkerLaunchGrantServiceErrorKind::LaunchGrantConflict => ClientSessionsError::new(
            ClientSessionsErrorKind::CapacityExhausted,
            "the worker session already carries a live launch",
        ),
        _ => ClientSessionsError::unavailable(),
    }
}

/// Builds the `201` session body. The raw worker credential material crosses
/// exactly here and on the device chain; durable state keeps only its
/// digest.
fn session_body(prepared: &PreparedLaunch) -> Value {
    json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "clientId": prepared.node.public_client_id,
        "workerLaunchGrantId": prepared.grant.worker_launch_grant_id,
        "repositoryBindingId": prepared.grant.repository_binding_id,
        "occupancyLeaseId": prepared.occupancy_lease_id,
        "occupancyFencingToken": prepared.occupancy_fencing_token,
        "workerSessionId": prepared.grant.worker_session_id,
        "workerId": prepared.grant.worker_id,
        "workerInstanceId": prepared.grant.worker_instance_id,
        "productSessionId": prepared.product_session_id,
        "stageRunId": prepared.stage_run_id,
        "credentialDigest": prepared.grant.credential_digest,
        "workerCredential": prepared.worker_credential,
        "expiresAt": prepared.grant.expires_at.0,
    })
}

/// Builds the occupancy fencing stamp every occupancy-backed downlink
/// command carries (contract `client-control-port-v1.md`, `C + L`).
#[must_use]
pub fn occupancy_stamp(
    expected_revision: u64,
    occupancy_lease_id: &str,
    fencing_token: u64,
    idempotency_key: &str,
) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision,
            idempotency_key: idempotency_key.to_owned(),
        },
        occupancy_lease_id: occupancy_lease_id.to_owned(),
        occupancy_fencing_token: fencing_token,
    }
}

/// Builds one `client.worker.stop` downlink message (contract
/// `client-control-port-v1.md`): the stamped occupancy context, the worker
/// session and worker to stop, and the reason. The supervisor and release
/// flows enqueue this through the durable outbox so a device that is offline
/// still receives the stop after it reconnects.
#[must_use]
pub fn worker_stop_message(
    occupancy: OccupancyCommandContext,
    worker_session_id: &str,
    worker_id: &str,
    reason: ClientWorkerStopReason,
) -> ServerToClientMessage {
    ServerToClientMessage::WorkerStop(ServerWorkerStopPayload {
        occupancy,
        worker_session_id: worker_session_id.to_owned(),
        worker_id: worker_id.to_owned(),
        reason,
    })
}

/// Enqueues one Server → Client frame into the durable outbox at the next
/// free stream position.
fn enqueue_frame(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    message: ServerToClientMessage,
    now: &Instant,
) -> Result<(), ClientSessionsError> {
    let instance = node
        .current_instance_id
        .clone()
        .ok_or_else(ClientSessionsError::unavailable)?;
    let cursors = {
        let mut registry = ClientRegistryService::new(storage);
        registry
            .exchange_cursors(&node.client_node_id)
            .map_err(|_| ClientSessionsError::unavailable())?
            .ok_or_else(ClientSessionsError::unavailable)?
    };
    let mut downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientSessionsError::unavailable())?;
    let outbox_high_water = downlink
        .high_water(&node.client_node_id)
        .map_err(|_| ClientSessionsError::unavailable())?;
    let sequence = cursors
        .server_to_client_ack_sequence
        .max(outbox_high_water)
        .checked_add(1)
        .ok_or_else(ClientSessionsError::unavailable)?;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("msg_")?,
        client_node_id: node.client_node_id.clone(),
        client_instance_id: instance,
        sequence,
        occurred_at: now.0.clone(),
        message,
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let stored = codec
        .encode_envelope(&envelope)
        .map_err(|_| ClientSessionsError::unavailable())?;
    let frame = std::str::from_utf8(&stored.frame)
        .map_err(|_| ClientSessionsError::unavailable())?
        .to_owned();
    downlink
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id.clone(),
                envelope.message_id.clone(),
                sequence,
                frame,
            )
            .map_err(|_| ClientSessionsError::unavailable())?,
            now,
        )
        .map_err(|_| ClientSessionsError::unavailable())?;
    Ok(())
}

/// Reads one required public Client ID: 9-12 ASCII digits.
fn required_client_id(value: Option<&Value>) -> Result<String, ClientSessionsError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientSessionsError::invalid_request)?;
    if (9..=12).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(text.to_owned())
    } else {
        Err(ClientSessionsError::invalid_request())
    }
}

/// Issues one random 32-byte worker credential and returns its lowercase hex
/// material plus the persisted `sha256:` digest. Only the digest ever enters
/// durable state (plan 17.2); the material crosses the launch response and
/// the device chain exactly once.
fn issue_worker_credential() -> Result<(String, String), ClientSessionsError> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|_| ClientSessionsError::unavailable())?;
    Ok((hex_encode(&secret), credential_digest(&secret)))
}

/// Computes the persisted `sha256:` digest of one credential secret.
fn credential_digest(secret: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(secret))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

/// The canonical application instant the boundary shares across one flow.
fn now_instant() -> Instant {
    use crate::application::StandaloneApplicationClock as _;
    crate::application::SystemStandaloneApplicationClock.now_instant()
}

/// Signed millisecond amount of one duration, clamped to the `i64` range.
fn duration_millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generates one canonical `prefix` + 26 character Crockford identifier.
fn generate_prefixed_id(prefix: &str) -> Result<String, ClientSessionsError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientSessionsError::unavailable())?;
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

    #[test]
    fn config_rejects_zero_bounds() {
        let mut config = ClientSessionsConfig::default();
        assert!(ClientSessionsApplication::open("unused", &config).is_ok());
        config.launch_wait = std::time::Duration::ZERO;
        assert!(ClientSessionsApplication::open("unused", &config).is_err());
    }

    #[test]
    fn generated_ids_carry_the_launch_prefixes() {
        for prefix in ["wlg_", "ws_", "wkr_", "winst_", "ps_", "run_", "msg_"] {
            let id = generate_prefixed_id(prefix).expect("entropy");
            assert_eq!(id.len(), prefix.len() + 26);
            assert!(id.starts_with(prefix));
        }
    }

    #[test]
    fn worker_credential_material_is_32_bytes_and_digest_bound() {
        let (material, digest) = issue_worker_credential().expect("entropy");
        assert_eq!(material.len(), 64, "lowercase hex of 32 bytes");
        assert!(
            material
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), 71);
        let (again, again_digest) = issue_worker_credential().expect("entropy");
        assert_ne!(material, again, "every credential is fresh randomness");
        assert_ne!(digest, again_digest);
    }

    #[test]
    fn public_client_id_shape_is_nine_to_twelve_digits() {
        let value = |text: &str| Some(Value::String(text.to_owned()));
        assert_eq!(
            required_client_id(value("927351842").as_ref()).expect("valid"),
            "927351842"
        );
        assert!(required_client_id(value("12345678").as_ref()).is_err());
        assert!(required_client_id(value("1234567890123").as_ref()).is_err());
        assert!(required_client_id(None).is_err());
    }

    #[test]
    fn stop_message_carries_the_stamped_occupancy_context() {
        let message = worker_stop_message(
            occupancy_stamp(7, "ocl_A", 9, "idem_stop_wlg"),
            "ws_A",
            "wkr_A",
            ClientWorkerStopReason::GrantRevoked,
        );
        let ServerToClientMessage::WorkerStop(payload) = &message else {
            panic!("worker stop message expected");
        };
        assert_eq!(payload.worker_session_id, "ws_A");
        assert_eq!(payload.worker_id, "wkr_A");
        assert_eq!(payload.reason, ClientWorkerStopReason::GrantRevoked);
        assert_eq!(payload.occupancy.occupancy_lease_id, "ocl_A");
        assert_eq!(payload.occupancy.occupancy_fencing_token, 9);
        assert_eq!(payload.occupancy.command.expected_revision, 7);
        assert_eq!(payload.occupancy.command.idempotency_key, "idem_stop_wlg");
    }
}
