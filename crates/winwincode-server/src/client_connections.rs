// SPDX-License-Identifier: Apache-2.0

//! Server-owned browser authorization for access to registered Clients.
//!
//! `POST /api/v1/clients/connections` validates the signed-in user, rate
//! limits, stored access policy and connect-code digest. It atomically consumes
//! the code and creates the grant without contacting Device or requiring it
//! to be online. Retried requests return the existing grant.
//!
//! `GET /api/v1/clients` projects the signed-in user's directory of granted
//! Clients as `DeviceSummary` cards; occupancy is uniformly `available` until
//! the occupancy epic lands, and presence and lock facts map from the
//! `ClientNode` registry.
//!
//! `POST /api/v1/clients/grants/revoke` revokes the Owner's grant immediately;
//! revocation takes effect without waiting for the Device Client.
//!
//! Authorization decisions (grant creation and revocation) are recorded in
//! the durable `client_connect_audit` table. The connect flow carries no
//! organization/workspace scope, so the scoped `winwincode-audit`
//! event schema does not apply; the dedicated storage-level audit trail is
//! the canonical record for this domain.

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use winwincode_control_plane::AccessGrantService;
use winwincode_control_plane::ClientConnectServiceErrorKind;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::RepositoryAccessGrantService;
use winwincode_control_plane::RepositoryBindingService;
use winwincode_domain::Instant;
use winwincode_storage::AccessGrantIssuance;
use winwincode_storage::AttemptDimension;
use winwincode_storage::ClientLockState;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::ConnectAuditAction;
use winwincode_storage::ConnectAuditEntry;
use winwincode_storage::ConnectCodeConsume;
use winwincode_storage::ConnectCodeRecord;
use winwincode_storage::ConnectCodeState;
use winwincode_storage::GrantTrustMode;
use winwincode_storage::RepositoryAccessGrantIssuance;
use winwincode_storage::RepositoryGrantPermissions;
use winwincode_storage::SqliteStorage;
use winwincode_storage::connect_attempt_window_anchor;

/// Schema version of the public browser-facing connect surface.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Server-side throttling policy of the connect flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConnectionsConfig {
    /// Length of the fixed connect-attempt window in seconds (plan 11.3).
    pub rate_window_seconds: u64,
    /// Failed connect attempts per window and dimension that block further
    /// attempts (plan 11.3).
    pub rate_max_attempts: u64,
}

impl Default for ClientConnectionsConfig {
    fn default() -> Self {
        Self {
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
    /// The requested operation needs a reachable Client.
    ClientOffline,
    /// The presented code digest matches no code.
    ConnectCodeInvalid,
    /// The code is expired, exhausted, revoked, or already consumed.
    ConnectCodeExpired,
    /// The Client no longer accepts new connections.
    ClientConnectionsForbidden,
    /// The Client is locked by a local operator.
    ClientLocked,
    /// One of the three attempt dimensions is throttled.
    RateLimited,
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

/// The signed-in user's authorization surface over the Server's product-state
/// database. Device transport is independent of browser authorization.
#[derive(Debug, Clone)]
pub struct ClientConnectionsApplication {
    data_directory: PathBuf,
    config: ClientConnectionsConfig,
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
        if config.rate_window_seconds == 0 || config.rate_max_attempts == 0 {
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

    /// Authorizes a browser request from Server-owned state, without a Device round trip.
    ///
    /// # Errors
    /// Rejects invalid, expired or already-consumed codes and unavailable storage.
    pub fn connect(
        &self,
        user_id: &str,
        client_ip: &str,
        request: &Value,
    ) -> Result<Value, ClientConnectionsError> {
        self.prepare(user_id, client_ip, request)
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

    /// Revokes the signed-in Owner's active grant immediately (contract 3).
    ///
    /// # Errors
    ///
    /// Rejects an invalid body, an unknown grant, or storage failure.
    pub fn revoke(
        &self,
        owner_user_id: &str,
        request: &Value,
    ) -> Result<Value, ClientConnectionsError> {
        let Some(fields) = request.as_object() else {
            return Err(ClientConnectionsError::invalid_request());
        };
        if fields.len() != 2
            || fields.get("schemaVersion").and_then(Value::as_str) != Some(SUPPORTED_SCHEMA_VERSION)
        {
            return Err(ClientConnectionsError::invalid_request());
        }
        let public_client_id = required_digits(fields.get("clientId"), 9, 12)?;
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
            .active_grant(&node.client_node_id, owner_user_id)
            .map_err(|_| ClientConnectionsError::unavailable())?
            .ok_or_else(|| {
                ClientConnectionsError::new(
                    ClientConnectionsErrorKind::ResourceNotFound,
                    "no active grant matches the requested client and user",
                )
            })?;
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
            owner_user_id,
            Some("revoked by local owner"),
        )?;
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "revoked": true,
            "clientId": node.public_client_id,
        }))
    }

    /// Validates the request and consumes the Server-owned code atomically.
    #[allow(clippy::too_many_lines)]
    fn prepare(
        &self,
        user_id: &str,
        client_ip: &str,
        request: &Value,
    ) -> Result<Value, ClientConnectionsError> {
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
                ensure_repository_grants(&mut storage, &node.client_node_id, user_id)?;
                return directory_json(&mut storage, user_id);
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

        drop(storage);
        self.consume(user_id, client_ip, &node, &code)
    }

    /// Consumes the validated code atomically and builds the `201` body.
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
                ensure_repository_grants(&mut storage, &node.client_node_id, user_id)?;
                directory_json(&mut storage, user_id)
            }
            Err(error) => match error.kind() {
                ClientConnectServiceErrorKind::ClientConnectionsForbidden => {
                    Err(ClientConnectionsError::new(
                        ClientConnectionsErrorKind::ClientConnectionsForbidden,
                        "the client no longer accepts new connections",
                    ))
                }
                ClientConnectServiceErrorKind::ClientLocked => Err(client_locked()),
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
                        ensure_repository_grants(&mut storage, &node.client_node_id, user_id)?;
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

fn ensure_repository_grants(
    storage: &mut SqliteStorage,
    client_node_id: &str,
    user_id: &str,
) -> Result<(), ClientConnectionsError> {
    let can_manage = AccessGrantService::new(storage)
        .active_grant(client_node_id, user_id)
        .map_err(|_| ClientConnectionsError::unavailable())?
        .is_some_and(|grant| grant.permissions.can_manage());
    if !can_manage {
        return Ok(());
    }
    let bindings = RepositoryBindingService::new(storage)
        .bindings_for_client(client_node_id)
        .map_err(|_| ClientConnectionsError::unavailable())?;
    for binding in bindings {
        let already_granted = RepositoryAccessGrantService::new(storage)
            .active_grants_for_binding(&binding.repository_binding_id)
            .map_err(|_| ClientConnectionsError::unavailable())?
            .into_iter()
            .any(|grant| grant.user_id == user_id);
        if already_granted {
            continue;
        }
        let issuance = RepositoryAccessGrantIssuance::try_new(
            generate_prefixed_id("rag_")?,
            &binding.repository_binding_id,
            user_id,
            user_id,
        )
        .map_err(|_| ClientConnectionsError::unavailable())?;
        RepositoryAccessGrantService::new(storage)
            .create_grant(
                &issuance,
                RepositoryGrantPermissions::UseManage,
                &now_instant(),
            )
            .map_err(|_| ClientConnectionsError::unavailable())?;
    }
    Ok(())
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
        config.rate_window_seconds = 0;
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
