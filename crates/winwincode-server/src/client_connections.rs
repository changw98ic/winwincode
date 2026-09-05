// SPDX-License-Identifier: Apache-2.0

//! User-facing Client connection flow over the real `ClientControlPort`
//! (plan 11.3-11.5, §16.3, contract `client-control-state-machines.md` 2-3).
//!
//! `POST /api/v1/clients/connections` validates the signed-in user, the three
//! rate-limit dimensions, the target Client's presence, lock, and
//! accepting-connections facts, and the presented dynamic connect code
//! digest; then it creates one durable access challenge, enqueues the
//! `client.access.challenge` downlink frame into the durable outbox, and
//! waits a bounded, configurable interval for the Device Client's
//! `client.access.challenge_ack` to be settled by the client exchange. A
//! confirmed challenge is consumed atomically together with the derived
//! `ClientAccessGrant` (`ConnectCodeService::consume_and_grant`), so a
//! retried request can never produce a second grant — the partial unique
//! index on `(client_node_id, user_id)` over active grants backs the same
//! guarantee at the storage layer.
//!
//! `GET /api/v1/clients` projects the signed-in user's directory of granted
//! Clients as `DeviceSummary` cards; occupancy is uniformly `available` until
//! the occupancy epic lands, and presence and lock facts map from the
//! `ClientNode` registry.
//!
//! `POST /api/v1/clients/grants/revoke` revokes one grant immediately
//! (contract 3): the holder themself or an Owner may revoke; revocation takes
//! effect without waiting for the Device Client.
//!
//! Authorization decisions (grant creation and revocation) are recorded in
//! the durable `client_connect_audit` table. The connect flow carries no
//! organization/workspace scope, so the enterprise-shaped `winwincode-audit`
//! event schema does not apply; the dedicated storage-level audit trail is
//! the canonical record for this domain.

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::ServerAccessChallengePayload;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_control_plane::AccessGrantService;
use winwincode_control_plane::ClientConnectServiceErrorKind;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_domain::Instant;
use winwincode_storage::AccessChallengeCreation;
use winwincode_storage::AccessGrantIssuance;
use winwincode_storage::AttemptDimension;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientLockState;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::ConnectAuditAction;
use winwincode_storage::ConnectAuditEntry;
use winwincode_storage::ConnectChallengeState;
use winwincode_storage::ConnectCodeConsume;
use winwincode_storage::ConnectCodeRecord;
use winwincode_storage::ConnectCodeState;
use winwincode_storage::GrantTrustMode;
use winwincode_storage::SqliteStorage;
use winwincode_storage::connect_attempt_window_anchor;

/// Schema version of the public browser-facing connect surface.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Bounded-wait and throttling policy of the connect flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConnectionsConfig {
    /// How long one connect request waits for the Device Client challenge
    /// acknowledgement before failing (plan 11.4 step 7).
    pub challenge_wait: std::time::Duration,
    /// How often the durable challenge state is polled while waiting.
    pub poll_interval: std::time::Duration,
    /// Length of the fixed connect-attempt window in seconds (plan 11.3).
    pub rate_window_seconds: u64,
    /// Failed connect attempts per window and dimension that block further
    /// attempts (plan 11.3).
    pub rate_max_attempts: u64,
}

impl Default for ClientConnectionsConfig {
    fn default() -> Self {
        Self {
            challenge_wait: std::time::Duration::from_secs(30),
            poll_interval: std::time::Duration::from_millis(200),
            rate_window_seconds: 300,
            rate_max_attempts: 5,
        }
    }
}

/// Stable failure categories of the connect flow boundary. Each domain
/// category maps to exactly one wire error code of the §16.3 taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientConnectionsErrorKind {
    /// The request body violated the connect contract.
    InvalidRequest,
    /// The public Client ID does not name a connectable Client.
    ClientNotFound,
    /// The Client is not reachable (offline, degraded, or did not answer the
    /// challenge within the bounded wait).
    ClientOffline,
    /// The presented code digest matches no code, or the device rejected it.
    ConnectCodeInvalid,
    /// The code is expired, exhausted, revoked, or already consumed.
    ConnectCodeExpired,
    /// The Client no longer accepts new connections.
    ClientConnectionsForbidden,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// One of the three attempt dimensions is throttled.
    RateLimited,
    /// The acting user may not revoke the requested grant.
    PermissionDenied,
    /// No active grant matches the revoke request.
    ResourceNotFound,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free connect flow failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConnectionsError {
    kind: ClientConnectionsErrorKind,
    message: String,
}

impl ClientConnectionsError {
    #[must_use]
    pub const fn kind(&self) -> ClientConnectionsErrorKind {
        self.kind
    }

    fn new(kind: ClientConnectionsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientConnectionsErrorKind::InvalidRequest,
            "connect request must carry a 9-12 digit clientId and an 8-digit connectionCode",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientConnectionsErrorKind::Unavailable,
            "client connect service is unavailable",
        )
    }
}

impl fmt::Display for ClientConnectionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientConnectionsError {}

/// The signed-in user's connect surface over the Server's one product-state
/// database directory. Like the client exchange, every operation opens and
/// closes its own storage connection so concurrent flows never share state in
/// memory and the bounded wait holds no database lock.
#[derive(Debug, Clone)]
pub struct ClientConnectionsApplication {
    data_directory: PathBuf,
    config: ClientConnectionsConfig,
}

/// What one validated connect attempt prepared before the bounded wait.
#[allow(clippy::large_enum_variant)]
enum Prepared {
    /// The user already holds an active grant on the Client: an idempotent
    /// retry that returns the same device list without a second grant.
    Completed(Value),
    /// The challenge was created (or reused) and the flow must wait for its
    /// acknowledgement.
    AwaitChallenge {
        node: ClientNodeRecord,
        code: ConnectCodeRecord,
        challenge_id: String,
    },
}

/// The outcome of one poll of a pending challenge.
enum PollOutcome {
    /// The challenge is still pending; keep waiting.
    Pending,
    /// The flow reached its terminal response body (the fresh device list).
    Completed(Value),
    /// The flow failed with the mapped domain error.
    Failed(ClientConnectionsError),
}

impl ClientConnectionsApplication {
    /// Composes the connect application over one product-state directory.
    ///
    /// # Errors
    ///
    /// Fails when the configuration violates its bounds.
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientConnectionsConfig,
    ) -> Result<Self, ClientConnectionsError> {
        if config.challenge_wait.is_zero()
            || config.poll_interval.is_zero()
            || config.rate_window_seconds == 0
            || config.rate_max_attempts == 0
        {
            return Err(ClientConnectionsError::new(
                ClientConnectionsErrorKind::InvalidRequest,
                "client connect configuration bounds must be positive",
            ));
        }
        Ok(Self {
            data_directory: data_directory.into(),
            config: config.clone(),
        })
    }

    /// Runs the full connect flow (plan 11.4, steps 3-10) and resolves to the
    /// fresh device list body of the `201` response.
    ///
    /// # Errors
    ///
    /// Returns the stable §16.3 failure categories; `ClientOffline` also
    /// covers a device that never answered the challenge within the bounded
    /// wait.
    pub async fn connect(
        &self,
        user_id: &str,
        client_ip: &str,
        request: &Value,
    ) -> Result<Value, ClientConnectionsError> {
        match self.prepare(user_id, client_ip, request)? {
            Prepared::Completed(body) => Ok(body),
            Prepared::AwaitChallenge {
                node,
                code,
                challenge_id,
            } => {
                let deadline = tokio::time::Instant::now() + self.config.challenge_wait;
                loop {
                    match self.poll(user_id, client_ip, &node, &code, &challenge_id)? {
                        PollOutcome::Pending => {}
                        PollOutcome::Completed(body) => return Ok(body),
                        PollOutcome::Failed(error) => return Err(error),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        // The device was online but never answered the
                        // challenge: within the fixed §16.3 taxonomy this is
                        // the offline category.
                        self.record_failures(user_id, client_ip, &node.client_node_id)?;
                        return Err(ClientConnectionsError::new(
                            ClientConnectionsErrorKind::ClientOffline,
                            "the device did not confirm the connect challenge in time",
                        ));
                    }
                    tokio::time::sleep(self.config.poll_interval).await;
                }
            }
        }
    }

    /// Projects the signed-in user's granted Clients as a device list body.
    ///
    /// # Errors
    ///
    /// Fails when durable state or storage is unavailable.
    pub fn list_clients(&self, user_id: &str) -> Result<Value, ClientConnectionsError> {
        let mut storage = self.open_storage()?;
        directory_json(&mut storage, user_id)
    }

    /// Revokes one active grant immediately (contract 3). The holder or an
    /// Owner may revoke; the request names the Client and optionally the
    /// holder (`clientId`, optional `userId`).
    ///
    /// # Errors
    ///
    /// Rejects an invalid body, an unknown grant, a non-holder non-Owner
    /// actor, or storage failure.
    pub fn revoke(
        &self,
        acting_user_id: &str,
        acting_is_owner: bool,
        request: &Value,
    ) -> Result<Value, ClientConnectionsError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientConnectionsError::invalid_request());
        };
        // `schemaVersion` plus `clientId`, optionally plus `userId`.
        if fields.len() != 2 && fields.len() != 3 {
            return Err(ClientConnectionsError::invalid_request());
        }
        let public_client_id = required_digits(fields.get("clientId"), 9, 12)?;
        let target_user_id = match fields.get("userId") {
            Some(value) => value
                .as_str()
                .ok_or_else(ClientConnectionsError::invalid_request)?
                .to_owned(),
            None => acting_user_id.to_owned(),
        };
        let mut storage = self.open_storage()?;
        let node = {
            let mut registry = ClientRegistryService::new(&mut storage);
            registry
                .snapshot_by_public_client_id(&public_client_id)
                .map_err(|_| ClientConnectionsError::unavailable())?
                .ok_or_else(|| {
                    ClientConnectionsError::new(
                        ClientConnectionsErrorKind::ResourceNotFound,
                        "no client matches the requested id",
                    )
                })?
        };
        let mut grants = AccessGrantService::new(&mut storage);
        let grant = grants
            .active_grant(&node.client_node_id, &target_user_id)
            .map_err(|_| ClientConnectionsError::unavailable())?
            .ok_or_else(|| {
                ClientConnectionsError::new(
                    ClientConnectionsErrorKind::ResourceNotFound,
                    "no active grant matches the requested client and user",
                )
            })?;
        if acting_user_id != target_user_id && !acting_is_owner {
            return Err(ClientConnectionsError::new(
                ClientConnectionsErrorKind::PermissionDenied,
                "only the grant holder or an Owner may revoke a grant",
            ));
        }
        let revoked = grants
            .revoke_grant(&grant.client_access_grant_id, grant.revision)
            .map_err(|error| match error.kind() {
                ClientConnectServiceErrorKind::UnknownAccessGrant => ClientConnectionsError::new(
                    ClientConnectionsErrorKind::ResourceNotFound,
                    "no active grant matches the requested client and user",
                ),
                _ => ClientConnectionsError::unavailable(),
            })?;
        record_audit(
            &mut storage,
            ConnectAuditAction::AccessRevoked,
            &node.client_node_id,
            &revoked.client_access_grant_id,
            &revoked.user_id,
            acting_user_id,
            Some("revoked by grant holder or owner"),
        )?;
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "revoked": true,
            "clientId": node.public_client_id,
            "userId": revoked.user_id,
        }))
    }

    /// Validates the request and durable preconditions, creates or reuses the
    /// durable access challenge, and enqueues the challenge downlink frame
    /// (plan 11.4, steps 4-6).
    #[allow(clippy::too_many_lines)]
    fn prepare(
        &self,
        user_id: &str,
        client_ip: &str,
        request: &Value,
    ) -> Result<Prepared, ClientConnectionsError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientConnectionsError::invalid_request());
        };
        if fields.len() != 3 {
            return Err(ClientConnectionsError::invalid_request());
        }
        let public_client_id = required_digits(fields.get("clientId"), 9, 12)?;
        let connection_code = required_digits(fields.get("connectionCode"), 8, 8)?;
        let now = now_instant();
        let code_digest = connect_code_digest(&connection_code);

        let mut storage = self.open_storage()?;
        let node = {
            let mut registry = ClientRegistryService::new(&mut storage);
            registry
                .snapshot_by_public_client_id(&public_client_id)
                .map_err(|_| ClientConnectionsError::unavailable())?
        };
        let node = match node {
            // Pending-enrollment and revoked identities are not Clients yet
            // (or any more); the boundary cannot connect them.
            None
            | Some(ClientNodeRecord {
                presence_state:
                    ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked,
                ..
            }) => {
                return Err(client_not_found());
            }
            Some(node)
                if matches!(
                    node.presence_state,
                    ClientPresenceState::Offline | ClientPresenceState::Degraded
                ) =>
            {
                return Err(ClientConnectionsError::new(
                    ClientConnectionsErrorKind::ClientOffline,
                    "the client is not online",
                ));
            }
            // A locally locked device is not connectable either.
            Some(node) if node.presence_state == ClientPresenceState::Locked => {
                return Err(client_locked());
            }
            Some(node) => node,
        };
        if !node.accepting_connections {
            return Err(ClientConnectionsError::new(
                ClientConnectionsErrorKind::ClientConnectionsForbidden,
                "the client no longer accepts new connections",
            ));
        }
        if node.lock_state == ClientLockState::Locked {
            return Err(client_locked());
        }

        // Three-dimensional throttling (plan 11.3): user, source address, and
        // target Client, each against the same fixed window anchor.
        let anchor = connect_attempt_window_anchor(&now, self.config.rate_window_seconds)
            .map_err(|_| ClientConnectionsError::unavailable())?;
        {
            let mut connect = ConnectCodeService::new(&mut storage);
            for (dimension, subject) in [
                (AttemptDimension::User, user_id),
                (AttemptDimension::Ip, client_ip),
                (AttemptDimension::Client, node.client_node_id.as_str()),
            ] {
                let blocked = connect
                    .connect_attempts_blocked(
                        dimension,
                        subject,
                        &anchor,
                        self.config.rate_max_attempts,
                    )
                    .map_err(|_| ClientConnectionsError::unavailable())?;
                if blocked {
                    return Err(ClientConnectionsError::new(
                        ClientConnectionsErrorKind::RateLimited,
                        "connect attempts are rate limited",
                    ));
                }
            }
        }

        // Idempotent retry of an already-successful connect: the retry sees
        // the existing active grant and returns the same 201 device list.
        {
            let mut grants = AccessGrantService::new(&mut storage);
            if grants
                .active_grant(&node.client_node_id, user_id)
                .map_err(|_| ClientConnectionsError::unavailable())?
                .is_some()
            {
                return directory_json(&mut storage, user_id).map(Prepared::Completed);
            }
        }

        // Code verification against the stored digest only (plan 11.3): a
        // wrong code is indistinguishable from an unknown one.
        let mut connect = ConnectCodeService::new(&mut storage);
        let code = connect
            .code_snapshot_by_digest(&code_digest)
            .map_err(|_| ClientConnectionsError::unavailable())?;
        let code = match code {
            None => {
                self.record_failures(user_id, client_ip, &public_client_id)?;
                return Err(connect_code_invalid());
            }
            Some(code) if code.client_node_id != node.client_node_id => {
                // The code belongs to another Client: still one invalid code.
                self.record_failures(user_id, client_ip, &public_client_id)?;
                return Err(connect_code_invalid());
            }
            Some(code) => code,
        };
        let state_error = match code.state {
            ConnectCodeState::Active => {
                if code.expires_at.0.as_str() <= now.0.as_str() {
                    Some("the connect code has expired")
                } else if code.remaining_attempts == 0 {
                    Some("the connect code has no attempts left")
                } else {
                    None
                }
            }
            // Consumed (used up) and revoked (refreshed or voided) read as
            // the expiry category of the §16.3 taxonomy.
            ConnectCodeState::Expired | ConnectCodeState::Consumed | ConnectCodeState::Revoked => {
                Some("the connect code is no longer usable")
            }
        };
        if let Some(reason) = state_error {
            self.record_failures(user_id, client_ip, &node.client_node_id)?;
            return Err(ClientConnectionsError::new(
                ClientConnectionsErrorKind::ConnectCodeExpired,
                reason,
            ));
        }

        // Create or reuse the durable challenge (plan 11.4, step 5) so a
        // retried request waits on the same challenge instead of queueing a
        // second frame.
        let (reused, challenge_id) = {
            let existing = connect
                .pending_challenge_for_subject(&node.client_node_id, user_id, &code.connect_code_id)
                .map_err(|_| ClientConnectionsError::unavailable())?;
            if let Some(pending) = existing {
                (true, pending.challenge_id)
            } else {
                let creation = AccessChallengeCreation::try_new(
                    generate_prefixed_id("cch_")?,
                    node.client_node_id.clone(),
                    code.connect_code_id.clone(),
                    user_id,
                )
                .map_err(|_| ClientConnectionsError::unavailable())?;
                let created = connect
                    .create_challenge(&creation, &now)
                    .map_err(|_| ClientConnectionsError::unavailable())?;
                (false, created.challenge_id)
            }
        };
        if !reused {
            enqueue_challenge_frame(
                &mut storage,
                &node,
                &challenge_id,
                &code.connect_code_id,
                &code_digest,
                &code.expires_at,
                user_id,
                &now,
            )?;
        }

        Ok(Prepared::AwaitChallenge {
            node,
            code,
            challenge_id,
        })
    }

    /// Reads the durable challenge state once and drives the flow to its next
    /// transition (plan 11.4, steps 7-9).
    fn poll(
        &self,
        user_id: &str,
        client_ip: &str,
        node: &ClientNodeRecord,
        code: &ConnectCodeRecord,
        challenge_id: &str,
    ) -> Result<PollOutcome, ClientConnectionsError> {
        let mut storage = self.open_storage()?;
        let challenge = {
            let mut connect = ConnectCodeService::new(&mut storage);
            connect
                .challenge_snapshot(challenge_id)
                .map_err(|_| ClientConnectionsError::unavailable())?
        };
        let Some(challenge) = challenge else {
            return Ok(PollOutcome::Failed(connect_code_invalid()));
        };
        match challenge.state {
            ConnectChallengeState::Pending => Ok(PollOutcome::Pending),
            ConnectChallengeState::Rejected => {
                self.record_failures(user_id, client_ip, &node.client_node_id)?;
                Ok(PollOutcome::Failed(connect_code_invalid()))
            }
            // The device confirmed this code generation; consume the code and
            // create the grant in one atomic transaction (plan 11.4 step 8,
            // contract 2 `active -> consumed`).
            ConnectChallengeState::Confirmed => {
                drop(storage);
                self.consume(user_id, client_ip, node, code)
                    .map(PollOutcome::Completed)
            }
        }
    }

    /// Consumes the confirmed code atomically and builds the `201` body.
    fn consume(
        &self,
        user_id: &str,
        client_ip: &str,
        node: &ClientNodeRecord,
        code: &ConnectCodeRecord,
    ) -> Result<Value, ClientConnectionsError> {
        let now = now_instant();
        let mut storage = self.open_storage()?;
        let mut connect = ConnectCodeService::new(&mut storage);
        let consume = ConnectCodeConsume::try_new(
            code.connect_code_id.clone(),
            code.code_digest.clone(),
            code.generation,
        )
        .map_err(|_| ClientConnectionsError::unavailable())?;
        let issuance = AccessGrantIssuance::try_new(
            generate_prefixed_id("cag_")?,
            node.client_node_id.clone(),
            user_id,
            user_id,
            GrantTrustMode::Trusted,
            None,
        )
        .map_err(|_| ClientConnectionsError::unavailable())?;
        match connect.consume_and_grant(&consume, &issuance, &now) {
            Ok(receipt) => {
                record_audit(
                    &mut storage,
                    ConnectAuditAction::AccessGranted,
                    &receipt.grant.client_node_id,
                    &receipt.grant.client_access_grant_id,
                    &receipt.grant.user_id,
                    user_id,
                    Some(if receipt.first_user {
                        "first user; use+manage+share"
                    } else {
                        "subsequent user; use"
                    }),
                )?;
                directory_json(&mut storage, user_id)
            }
            Err(error) => match error.kind() {
                // A concurrent retry of the same request won the consume: its
                // grant is ours to return (idempotent, one active grant per
                // user and client by the partial unique index).
                ClientConnectServiceErrorKind::AccessGrantConflict
                | ClientConnectServiceErrorKind::CodeNotActive => {
                    let mut grants = AccessGrantService::new(&mut storage);
                    if grants
                        .active_grant(&node.client_node_id, user_id)
                        .map_err(|_| ClientConnectionsError::unavailable())?
                        .is_some()
                    {
                        directory_json(&mut storage, user_id)
                    } else {
                        Err(ClientConnectionsError::new(
                            ClientConnectionsErrorKind::ConnectCodeExpired,
                            "the connect code was already used",
                        ))
                    }
                }
                ClientConnectServiceErrorKind::ConnectCodeExpired
                | ClientConnectServiceErrorKind::AttemptsExhausted => {
                    self.record_failures(user_id, client_ip, &node.client_node_id)?;
                    Err(ClientConnectionsError::new(
                        ClientConnectionsErrorKind::ConnectCodeExpired,
                        "the connect code is no longer usable",
                    ))
                }
                ClientConnectServiceErrorKind::GenerationMismatch
                | ClientConnectServiceErrorKind::UnknownConnectCode
                | ClientConnectServiceErrorKind::UnknownClientNode => {
                    self.record_failures(user_id, client_ip, &node.client_node_id)?;
                    Err(connect_code_invalid())
                }
                _ => Err(ClientConnectionsError::unavailable()),
            },
        }
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientConnectionsError> {
        SqliteStorage::open(&self.data_directory).map_err(|_| ClientConnectionsError::unavailable())
    }

    /// Records one failed attempt in all three throttle dimensions (plan
    /// 11.3). The client dimension uses the durable node id when the Client
    /// is known and the presented public id otherwise.
    fn record_failures(
        &self,
        user_id: &str,
        client_ip: &str,
        client_subject: &str,
    ) -> Result<(), ClientConnectionsError> {
        let now = now_instant();
        let anchor = connect_attempt_window_anchor(&now, self.config.rate_window_seconds)
            .map_err(|_| ClientConnectionsError::unavailable())?;
        let mut storage = self.open_storage()?;
        let mut connect = ConnectCodeService::new(&mut storage);
        for (dimension, subject) in [
            (AttemptDimension::User, user_id),
            (AttemptDimension::Ip, client_ip),
            (AttemptDimension::Client, client_subject),
        ] {
            connect
                .record_connect_failure(dimension, subject, &anchor)
                .map_err(|_| ClientConnectionsError::unavailable())?;
        }
        Ok(())
    }
}

fn client_not_found() -> ClientConnectionsError {
    ClientConnectionsError::new(
        ClientConnectionsErrorKind::ClientNotFound,
        "no client matches the requested id",
    )
}

fn client_locked() -> ClientConnectionsError {
    ClientConnectionsError::new(
        ClientConnectionsErrorKind::ClientLocked,
        "the client is locked",
    )
}

fn connect_code_invalid() -> ClientConnectionsError {
    ClientConnectionsError::new(
        ClientConnectionsErrorKind::ConnectCodeInvalid,
        "the connection code is not valid for this client",
    )
}

/// Reads one required all-digit field of an exact length range.
fn required_digits(
    value: Option<&Value>,
    min_digits: usize,
    max_digits: usize,
) -> Result<String, ClientConnectionsError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(ClientConnectionsError::invalid_request)?;
    if text.len() < min_digits || text.len() > max_digits {
        return Err(ClientConnectionsError::invalid_request());
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ClientConnectionsError::invalid_request());
    }
    Ok(text.to_owned())
}

/// SHA-256 digest of one presented 8-digit code (plan 11.3: only the digest
/// is ever persisted or compared).
fn connect_code_digest(code: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(code.as_bytes()))
}

/// Enqueues the `client.access.challenge` downlink frame into the durable
/// outbox at the next free stream position.
#[allow(clippy::too_many_arguments)]
fn enqueue_challenge_frame(
    storage: &mut SqliteStorage,
    node: &ClientNodeRecord,
    challenge_id: &str,
    connect_code_id: &str,
    code_digest: &str,
    expires_at: &Instant,
    requester_user_id: &str,
    now: &Instant,
) -> Result<(), ClientConnectionsError> {
    let instance = node
        .current_instance_id
        .clone()
        .ok_or_else(ClientConnectionsError::unavailable)?;
    let cursors = {
        let mut registry = ClientRegistryService::new(storage);
        registry
            .exchange_cursors(&node.client_node_id)
            .map_err(|_| ClientConnectionsError::unavailable())?
            .ok_or_else(ClientConnectionsError::unavailable)?
    };
    let mut downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientConnectionsError::unavailable())?;
    let outbox_high_water = downlink
        .high_water(&node.client_node_id)
        .map_err(|_| ClientConnectionsError::unavailable())?;
    let sequence = cursors
        .server_to_client_ack_sequence
        .max(outbox_high_water)
        .checked_add(1)
        .ok_or_else(ClientConnectionsError::unavailable)?;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("msg_")?,
        client_node_id: node.client_node_id.clone(),
        client_instance_id: instance,
        sequence,
        occurred_at: now.0.clone(),
        message: ServerToClientMessage::AccessChallenge(Box::new(ServerAccessChallengePayload {
            challenge_id: challenge_id.to_owned(),
            connect_code_id: connect_code_id.to_owned(),
            code_digest: code_digest.to_owned(),
            expires_at: expires_at.0.clone(),
            requester_user_id: requester_user_id.to_owned(),
        })),
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let stored = codec
        .encode_envelope(&envelope)
        .map_err(|_| ClientConnectionsError::unavailable())?;
    let frame = std::str::from_utf8(&stored.frame)
        .map_err(|_| ClientConnectionsError::unavailable())?
        .to_owned();
    downlink
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id.clone(),
                envelope.message_id.clone(),
                sequence,
                frame,
            )
            .map_err(|_| ClientConnectionsError::unavailable())?,
            now,
        )
        .map_err(|_| ClientConnectionsError::unavailable())?;
    Ok(())
}

/// Builds the device list body for one user: every active grant joined with
/// its registry projection, occupancy uniformly `available` until the
/// occupancy epic lands (§12.1, §16.4).
fn directory_json(
    storage: &mut SqliteStorage,
    user_id: &str,
) -> Result<Value, ClientConnectionsError> {
    let grants = {
        let mut grants = AccessGrantService::new(storage);
        grants
            .active_grants_for_user(user_id)
            .map_err(|_| ClientConnectionsError::unavailable())?
    };
    let mut registry = ClientRegistryService::new(storage);
    let mut clients = Vec::with_capacity(grants.len());
    for grant in grants {
        let record = registry
            .snapshot(&grant.client_node_id)
            .map_err(|_| ClientConnectionsError::unavailable())?;
        if let Some(record) = record
            && !matches!(
                record.presence_state,
                ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked
            )
        {
            clients.push(device_summary(&record));
        }
    }
    Ok(json!({
        "schemaVersion": SUPPORTED_SCHEMA_VERSION,
        "clients": clients,
    }))
}

/// One `DeviceSummary` card (the contract the browser facade validates).
fn device_summary(record: &ClientNodeRecord) -> Value {
    json!({
        "clientId": record.public_client_id,
        "displayName": record.display_name,
        "presence": presence_text(record.presence_state),
        "occupancy": "available",
        "capacityUsed": record.reported_running_worker_sessions,
        "capacityTotal": record.max_concurrent_worker_sessions,
        "lastHeartbeatAt": record
            .last_heartbeat_at
            .clone()
            .unwrap_or_else(|| record.created_at.clone())
            .0,
        "version": record.client_version,
    })
}

/// Maps the registry presence onto the three-value display presence (§12.1).
const fn presence_text(state: ClientPresenceState) -> &'static str {
    match state {
        ClientPresenceState::Online | ClientPresenceState::Degraded => "online",
        ClientPresenceState::Locked => "locked",
        // `pending_enrollment` and `revoked` never reach the directory
        // projection; offline is the safest display for them anyway.
        ClientPresenceState::Offline
        | ClientPresenceState::PendingEnrollment
        | ClientPresenceState::Revoked => "offline",
    }
}

/// Appends one connect-domain authorization audit entry. An audit failure
/// never undoes the durable grant decision, but it fails the request so the
/// gap is visible instead of silently swallowing the authorization record.
fn record_audit(
    storage: &mut SqliteStorage,
    action: ConnectAuditAction,
    client_node_id: &str,
    grant_id: &str,
    user_id: &str,
    actor_user_id: &str,
    detail: Option<&str>,
) -> Result<(), ClientConnectionsError> {
    let entry = ConnectAuditEntry::try_new(
        generate_prefixed_id("cad_")?,
        action,
        client_node_id,
        grant_id,
        user_id,
        actor_user_id,
        detail.map(str::to_owned),
        now_instant(),
    )
    .map_err(|_| ClientConnectionsError::unavailable())?;
    let mut connect = ConnectCodeService::new(storage);
    connect
        .record_connect_audit(&entry)
        .map_err(|_| ClientConnectionsError::unavailable())
}

/// The canonical application instant the boundary shares across one flow.
fn now_instant() -> Instant {
    use crate::application::StandaloneApplicationClock as _;
    crate::application::SystemStandaloneApplicationClock.now_instant()
}

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generates one canonical `prefix` + 26 character Crockford identifier.
fn generate_prefixed_id(prefix: &str) -> Result<String, ClientConnectionsError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientConnectionsError::unavailable())?;
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
        let mut config = ClientConnectionsConfig::default();
        assert!(ClientConnectionsApplication::open("unused", &config).is_ok());
        config.challenge_wait = std::time::Duration::ZERO;
        assert!(ClientConnectionsApplication::open("unused", &config).is_err());
    }

    #[test]
    fn device_summary_matches_the_facade_contract_field_by_field() {
        let record = ClientNodeRecord {
            client_node_id: "cnd_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
            public_client_id: "927351842".to_owned(),
            display_name: "Cheng's MacBook".to_owned(),
            platform: "aarch64-apple-darwin".to_owned(),
            architecture: "aarch64".to_owned(),
            client_version: "0.1.0-alpha.1".to_owned(),
            device_credential_digest: Some("sha256:aa".to_owned()),
            current_instance_id: Some("cix_A1A1A1A1A1A1A1A1A1A1A1A1A1".to_owned()),
            presence_state: ClientPresenceState::Online,
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            max_concurrent_worker_sessions: 4,
            reported_running_worker_sessions: 2,
            last_heartbeat_at: Some(Instant("2026-09-04T00:00:01.000Z".to_owned())),
            created_at: Instant("2026-09-04T00:00:00.000Z".to_owned()),
            revision: 7,
        };
        let value = device_summary(&record);
        let object = value.as_object().expect("summary object");
        assert_eq!(object.len(), 8, "exactly the facade fields");
        assert_eq!(value["clientId"], "927351842");
        assert_eq!(value["displayName"], "Cheng's MacBook");
        assert_eq!(value["presence"], "online");
        assert_eq!(value["occupancy"], "available");
        assert_eq!(value["capacityUsed"], 2);
        assert_eq!(value["capacityTotal"], 4);
        assert_eq!(value["lastHeartbeatAt"], "2026-09-04T00:00:01.000Z");
        assert_eq!(value["version"], "0.1.0-alpha.1");
    }

    #[test]
    fn presence_maps_degraded_to_online_and_terminal_states_to_offline() {
        assert_eq!(presence_text(ClientPresenceState::Online), "online");
        assert_eq!(presence_text(ClientPresenceState::Degraded), "online");
        assert_eq!(presence_text(ClientPresenceState::Offline), "offline");
        assert_eq!(presence_text(ClientPresenceState::Locked), "locked");
        assert_eq!(
            presence_text(ClientPresenceState::PendingEnrollment),
            "offline"
        );
        assert_eq!(presence_text(ClientPresenceState::Revoked), "offline");
    }

    #[test]
    fn required_digits_enforces_exact_digit_shapes() {
        let value = |text: &str| Some(Value::String(text.to_owned()));
        assert_eq!(
            required_digits(value("927351842").as_ref(), 9, 12).expect("valid id"),
            "927351842"
        );
        assert!(required_digits(value("12345678").as_ref(), 9, 12).is_err());
        assert!(required_digits(value("1234567890123").as_ref(), 9, 12).is_err());
        assert!(required_digits(value("1234567a").as_ref(), 8, 8).is_err());
        assert!(required_digits(None, 8, 8).is_err());
        assert_eq!(
            required_digits(value("12345678").as_ref(), 8, 8).expect("valid code"),
            "12345678"
        );
    }

    #[test]
    fn connect_code_digest_is_the_canonical_sha256_form() {
        let digest = connect_code_digest("68421975");
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), 7 + 64);
        let again = connect_code_digest("68421975");
        assert_eq!(digest, again);
        assert_ne!(digest, connect_code_digest("68421976"));
    }

    #[test]
    fn generated_ids_carry_the_connect_prefixes() {
        for prefix in ["cch_", "cag_", "cad_", "msg_"] {
            let id = generate_prefixed_id(prefix).expect("entropy");
            assert_eq!(id.len(), prefix.len() + 26);
            assert!(id.starts_with(prefix));
        }
    }
}
