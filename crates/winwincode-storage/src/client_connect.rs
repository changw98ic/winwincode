// SPDX-License-Identifier: Apache-2.0

//! Durable connect-code, access-grant, and connect-attempt storage.
//!
//! This module stores the persisted Control Plane authority for the UU-style
//! Client connection flow (plan 7.3, 7.4, 11.3, 11.5): one-time
//! `ClientConnectCode` digests, the `ClientAccessGrant` relationships they
//! establish, and the fixed-window connect attempt counters that throttle the
//! user, IP, and Client dimensions. Code and grant states and their legal
//! transitions follow the frozen state machine in
//! `docs/contracts/client-control-state-machines.md` (contracts 2 and 3); the
//! Server only ever persists the SHA-256 digest of a connect code, never the
//! code itself.
//!
//! The atomic consume boundary lives here: consuming an `active` code and
//! inserting the derived access grant commit in one `SQLite` `IMMEDIATE`
//! transaction, so a concurrent consume can never produce two grants or a
//! twice-consumed code.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_SUBJECT_KEY_BYTES: usize = 128;
const MAX_WINDOW_SECONDS: u64 = 366 * 24 * 60 * 60;

const CLIENT_CONNECT_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS client_connect_codes (
    connect_code_id TEXT PRIMARY KEY NOT NULL,
    code_digest TEXT NOT NULL UNIQUE,
    client_node_id TEXT NOT NULL,
    issued_by_instance_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0 AND generation <= 9007199254740991),
    expires_at TEXT NOT NULL,
    remaining_attempts INTEGER NOT NULL CHECK (remaining_attempts >= 0),
    state TEXT NOT NULL CHECK (state IN ('active', 'consumed', 'expired', 'revoked')),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS client_connect_codes_by_client
    ON client_connect_codes (client_node_id, state);
CREATE TABLE IF NOT EXISTS client_access_grants (
    client_access_grant_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    permissions TEXT NOT NULL CHECK (length(permissions) > 0),
    trust_mode TEXT NOT NULL CHECK (trust_mode IN ('temporary', 'trusted')),
    state TEXT NOT NULL CHECK (state IN ('active', 'revoked', 'expired')),
    grant_source TEXT NOT NULL CHECK (grant_source IN ('connect_code', 'administrator', 'local_confirmation')),
    granted_by_user_id TEXT NOT NULL,
    expires_at TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS client_access_grants_one_active_per_user_client
    ON client_access_grants (client_node_id, user_id) WHERE state = 'active';
CREATE INDEX IF NOT EXISTS client_access_grants_by_client_user
    ON client_access_grants (client_node_id, user_id, state);
CREATE TABLE IF NOT EXISTS connect_attempts (
    dimension TEXT NOT NULL CHECK (dimension IN ('user', 'ip', 'client')),
    subject_key TEXT NOT NULL,
    window_started_at TEXT NOT NULL,
    failed_attempts INTEGER NOT NULL CHECK (failed_attempts >= 0),
    PRIMARY KEY (dimension, subject_key)
);
";

/// Lifecycle state of one `ClientConnectCode` (contract 2).
///
/// `consumed`, `expired`, and `revoked` are terminal; `active` is the only
/// consumable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectCodeState {
    /// Published and waiting for one successful consume.
    Active,
    /// Terminal: the code was consumed by a successful grant creation.
    Consumed,
    /// Terminal: `expiresAt` passed without a consume.
    Expired,
    /// Terminal: the Client refreshed or voided the code.
    Revoked,
}

impl ConnectCodeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Consumed => "consumed",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientConnectStoreError> {
        match value {
            "active" => Ok(Self::Active),
            "consumed" => Ok(Self::Consumed),
            "expired" => Ok(Self::Expired),
            "revoked" => Ok(Self::Revoked),
            _ => Err(error(
                ClientConnectStoreErrorKind::CorruptState,
                "stored connect code state is invalid",
            )),
        }
    }
}

impl fmt::Display for ConnectCodeState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle state of one `ClientAccessGrant` (contract 3).
///
/// `active` is the only usable state; `revoked` and `expired` are terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessGrantState {
    /// The user may use the Client subject to occupancy and ACL checks.
    Active,
    /// Terminal: a holder of manage or share revoked the access.
    Revoked,
    /// Terminal: a temporary grant's `expiresAt` passed.
    Expired,
}

impl AccessGrantState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientConnectStoreError> {
        match value {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            _ => Err(error(
                ClientConnectStoreErrorKind::CorruptState,
                "stored access grant state is invalid",
            )),
        }
    }
}

impl fmt::Display for AccessGrantState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Trust mode of one `ClientAccessGrant` (plan 11.6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantTrustMode {
    /// Valid only for this connection or a short bounded term.
    Temporary,
    /// Survives re-login; skips the connect code, never occupancy or ACL.
    Trusted,
}

impl GrantTrustMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::Trusted => "trusted",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientConnectStoreError> {
        match value {
            "temporary" => Ok(Self::Temporary),
            "trusted" => Ok(Self::Trusted),
            _ => Err(error(
                ClientConnectStoreErrorKind::CorruptState,
                "stored access grant trust mode is invalid",
            )),
        }
    }
}

impl fmt::Display for GrantTrustMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Origin recorded on one `ClientAccessGrant` (contract 3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantSource {
    /// The atomic connect-code consume path; requires the Device Client ACK.
    ConnectCode,
    /// Created directly by an administrator.
    Administrator,
    /// Created after a local confirmation on the Device Client.
    LocalConfirmation,
}

impl GrantSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectCode => "connect_code",
            Self::Administrator => "administrator",
            Self::LocalConfirmation => "local_confirmation",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientConnectStoreError> {
        match value {
            "connect_code" => Ok(Self::ConnectCode),
            "administrator" => Ok(Self::Administrator),
            "local_confirmation" => Ok(Self::LocalConfirmation),
            _ => Err(error(
                ClientConnectStoreErrorKind::CorruptState,
                "stored access grant source is invalid",
            )),
        }
    }
}

impl fmt::Display for GrantSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Permission set of one `ClientAccessGrant` (plan 7.4, 11.5).
///
/// `use` is mandatory because a grant only ever expresses permission to use
/// the Client; the canonical stored form is the fixed-order token string, for
/// example `use+manage+share`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrantPermissions {
    can_use: bool,
    can_manage: bool,
    can_share: bool,
}

impl GrantPermissions {
    /// The default grant of a subsequent user (plan 11.5).
    pub const USE: Self = Self {
        can_use: true,
        can_manage: false,
        can_share: false,
    };
    /// The default grant of the first connecting user (plan 11.5).
    pub const USE_MANAGE_SHARE: Self = Self {
        can_use: true,
        can_manage: true,
        can_share: true,
    };

    /// Builds one permission set.
    ///
    /// # Errors
    ///
    /// Rejects a set without `use`: a grant that may not be used grants
    /// nothing and would silently bypass the occupancy gate.
    pub fn try_new(
        can_use: bool,
        can_manage: bool,
        can_share: bool,
    ) -> Result<Self, ClientConnectStoreError> {
        if !can_use {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "access grant permissions must include use",
            ));
        }
        Ok(Self {
            can_use,
            can_manage,
            can_share,
        })
    }

    #[must_use]
    pub const fn can_use(self) -> bool {
        self.can_use
    }

    #[must_use]
    pub const fn can_manage(self) -> bool {
        self.can_manage
    }

    #[must_use]
    pub const fn can_share(self) -> bool {
        self.can_share
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        // `use` is mandatory, so the two optional tokens fully cover the
        // canonical string space.
        match (self.can_manage, self.can_share) {
            (false, false) => "use",
            (true, false) => "use+manage",
            (false, true) => "use+share",
            (true, true) => "use+manage+share",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientConnectStoreError> {
        match value {
            "use" => Ok(Self::USE),
            "use+manage" => Self::try_new(true, true, false),
            "use+share" => Self::try_new(true, false, true),
            "use+manage+share" => Ok(Self::USE_MANAGE_SHARE),
            _ => Err(error(
                ClientConnectStoreErrorKind::CorruptState,
                "stored access grant permissions are invalid",
            )),
        }
    }
}

impl fmt::Display for GrantPermissions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Rate-limit dimension of one connect attempt counter (plan 11.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptDimension {
    /// The authenticated user that submitted the code.
    User,
    /// The network address the attempt originated from.
    Ip,
    /// The Client the attempt targeted.
    Client,
}

impl AttemptDimension {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ip => "ip",
            Self::Client => "client",
        }
    }
}

impl fmt::Display for AttemptDimension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Command that publishes one one-time connect code digest (plan 11.3,
/// contract 2 initial transition).
///
/// The Device Client generates the 8-digit code and only its SHA-256 digest
/// reaches durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodePublication {
    connect_code_id: String,
    code_digest: String,
    client_node_id: String,
    issued_by_instance_id: String,
    generation: u64,
    expires_at: Instant,
    remaining_attempts: u32,
}

impl ConnectCodePublication {
    /// Builds one validated publication command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a non-canonical SHA-256 digest, a
    /// zero generation, a zero attempt count, or a non-canonical expiry.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        connect_code_id: impl Into<String>,
        code_digest: impl Into<String>,
        client_node_id: impl Into<String>,
        issued_by_instance_id: impl Into<String>,
        generation: u64,
        expires_at: Instant,
        remaining_attempts: u32,
    ) -> Result<Self, ClientConnectStoreError> {
        let publication = Self {
            connect_code_id: connect_code_id.into(),
            code_digest: code_digest.into(),
            client_node_id: client_node_id.into(),
            issued_by_instance_id: issued_by_instance_id.into(),
            generation,
            expires_at,
            remaining_attempts,
        };
        publication.validate()?;
        Ok(publication)
    }

    fn validate(&self) -> Result<(), ClientConnectStoreError> {
        validate_connect_code_id(&self.connect_code_id)?;
        validate_sha256_digest(&self.code_digest)?;
        validate_client_node_id(&self.client_node_id)?;
        validate_client_instance_id(&self.issued_by_instance_id)?;
        if self.generation == 0 || self.generation > MAX_SAFE_INTEGER {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code generation must be between 1 and the safe integer range",
            ));
        }
        validate_instant(&self.expires_at, "connect code expiry")?;
        if self.remaining_attempts == 0 {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code must allow at least one remaining attempt",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn connect_code_id(&self) -> &str {
        &self.connect_code_id
    }

    #[must_use]
    pub fn code_digest(&self) -> &str {
        &self.code_digest
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn issued_by_instance_id(&self) -> &str {
        &self.issued_by_instance_id
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn expires_at(&self) -> &Instant {
        &self.expires_at
    }

    #[must_use]
    pub const fn remaining_attempts(&self) -> u32 {
        self.remaining_attempts
    }
}

/// Device Client challenge acknowledgement bound into the atomic consume
/// (contract 2: `active -> consumed` requires `client.access.challenge_ack`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodeConsume {
    connect_code_id: String,
    presented_code_digest: String,
    ack_generation: u64,
}

impl ConnectCodeConsume {
    /// Builds one validated consume command.
    ///
    /// `connect_code_id` names the code acknowledged by the Device Client,
    /// `presented_code_digest` is the server-side digest of the code the user
    /// submitted, and `ack_generation` is the code generation the Client
    /// confirmed still valid locally.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a non-canonical digest, or a zero
    /// generation.
    pub fn try_new(
        connect_code_id: impl Into<String>,
        presented_code_digest: impl Into<String>,
        ack_generation: u64,
    ) -> Result<Self, ClientConnectStoreError> {
        let consume = Self {
            connect_code_id: connect_code_id.into(),
            presented_code_digest: presented_code_digest.into(),
            ack_generation,
        };
        validate_connect_code_id(&consume.connect_code_id)?;
        validate_sha256_digest(&consume.presented_code_digest)?;
        if consume.ack_generation == 0 {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code consume generation must be positive",
            ));
        }
        Ok(consume)
    }

    #[must_use]
    pub fn connect_code_id(&self) -> &str {
        &self.connect_code_id
    }

    #[must_use]
    pub fn presented_code_digest(&self) -> &str {
        &self.presented_code_digest
    }

    #[must_use]
    pub const fn ack_generation(&self) -> u64 {
        self.ack_generation
    }
}

/// Revocation or refresh reference to one published connect code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodeRevocation {
    connect_code_id: String,
    expected_revision: u64,
}

impl ConnectCodeRevocation {
    /// Builds one validated revocation reference.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical code id or an out-of-range revision.
    pub fn try_new(
        connect_code_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<Self, ClientConnectStoreError> {
        let revocation = Self {
            connect_code_id: connect_code_id.into(),
            expected_revision,
        };
        validate_connect_code_id(&revocation.connect_code_id)?;
        validate_revision(revocation.expected_revision)?;
        Ok(revocation)
    }

    #[must_use]
    pub fn connect_code_id(&self) -> &str {
        &self.connect_code_id
    }

    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }
}

/// Command that creates one `ClientAccessGrant` (plan 7.4).
///
/// `grantSource` is supplied by the caller for the standalone creation path;
/// the atomic connect-code consume path forces `connect_code`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessGrantIssuance {
    client_access_grant_id: String,
    client_node_id: String,
    user_id: String,
    granted_by_user_id: String,
    trust_mode: GrantTrustMode,
    expires_at: Option<Instant>,
}

impl AccessGrantIssuance {
    /// Builds one validated grant issuance command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or a `temporary` trust mode without
    /// an `expiresAt`: a temporary grant is only ever short-lived (plan 11.6).
    pub fn try_new(
        client_access_grant_id: impl Into<String>,
        client_node_id: impl Into<String>,
        user_id: impl Into<String>,
        granted_by_user_id: impl Into<String>,
        trust_mode: GrantTrustMode,
        expires_at: Option<Instant>,
    ) -> Result<Self, ClientConnectStoreError> {
        let issuance = Self {
            client_access_grant_id: client_access_grant_id.into(),
            client_node_id: client_node_id.into(),
            user_id: user_id.into(),
            granted_by_user_id: granted_by_user_id.into(),
            trust_mode,
            expires_at,
        };
        issuance.validate()?;
        Ok(issuance)
    }

    fn validate(&self) -> Result<(), ClientConnectStoreError> {
        validate_access_grant_id(&self.client_access_grant_id)?;
        validate_client_node_id(&self.client_node_id)?;
        validate_user_id(&self.user_id)?;
        validate_user_id(&self.granted_by_user_id)?;
        if let Some(expires_at) = &self.expires_at {
            validate_instant(expires_at, "access grant expiry")?;
        }
        if self.trust_mode == GrantTrustMode::Temporary && self.expires_at.is_none() {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "temporary access grants must carry an expiry",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn client_access_grant_id(&self) -> &str {
        &self.client_access_grant_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    #[must_use]
    pub fn granted_by_user_id(&self) -> &str {
        &self.granted_by_user_id
    }

    #[must_use]
    pub const fn trust_mode(&self) -> GrantTrustMode {
        self.trust_mode
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<&Instant> {
        self.expires_at.as_ref()
    }
}

/// Durable `ClientConnectCode` projection row (plan 7.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodeRecord {
    /// Stable Server-side connect code identifier.
    pub connect_code_id: String,
    /// Canonical SHA-256 digest of the presented code; never the code itself.
    pub code_digest: String,
    /// Client the code grants access to.
    pub client_node_id: String,
    /// Device Client process instance that issued the code.
    pub issued_by_instance_id: String,
    /// Code generation confirmed by the Device Client challenge ACK.
    pub generation: u64,
    /// Instant after which the code can no longer be consumed.
    pub expires_at: Instant,
    /// Verification attempts the code still accepts.
    pub remaining_attempts: u32,
    /// Machine-level code state.
    pub state: ConnectCodeState,
    /// Instant the code was published.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Durable `ClientAccessGrant` projection row (plan 7.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessGrantRecord {
    /// Stable Server-side grant identifier.
    pub client_access_grant_id: String,
    /// Client the grant applies to.
    pub client_node_id: String,
    /// User holding the grant.
    pub user_id: String,
    /// Permission set; always includes `use`.
    pub permissions: GrantPermissions,
    /// Trust mode of the grant.
    pub trust_mode: GrantTrustMode,
    /// Machine-level grant state.
    pub state: AccessGrantState,
    /// Origin of the grant.
    pub grant_source: GrantSource,
    /// User that created the grant.
    pub granted_by_user_id: String,
    /// Expiry instant of a temporary grant, if any.
    pub expires_at: Option<Instant>,
    /// Instant the grant was created.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Result of the atomic consume-and-grant transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectGrantReceipt {
    /// Exact durable connect code projection after the consume.
    pub code: ConnectCodeRecord,
    /// Exact durable grant projection after the creation.
    pub grant: AccessGrantRecord,
    /// True when this user is the first user ever granted on the Client and
    /// therefore received `use+manage+share` (plan 11.5).
    pub first_user: bool,
}

/// Durable fixed-window connect attempt counter (plan 11.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectAttemptState {
    /// Rate-limit dimension of the counter.
    pub dimension: AttemptDimension,
    /// Subject key inside the dimension.
    pub subject_key: String,
    /// Anchor instant of the current fixed window.
    pub window_started_at: Instant,
    /// Failed attempts recorded inside the current window.
    pub failed_attempts: u64,
}

/// Stable connect-domain failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientConnectStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No connect code matches the requested identity or presented digest.
    UnknownConnectCode,
    /// No access grant matches the requested identity.
    UnknownAccessGrant,
    /// The connect code is not `active`, so the transition is rejected.
    CodeNotActive,
    /// The connect code's `expiresAt` has passed.
    ConnectCodeExpired,
    /// The connect code has no remaining verification attempts.
    AttemptsExhausted,
    /// The challenge ACK names a different code generation.
    GenerationMismatch,
    /// A connect code digest is already registered.
    ConnectCodeDigestConflict,
    /// A connect code id is already used.
    ConnectCodeIdConflict,
    /// An active grant for the user and client already exists, or the grant
    /// id is already used.
    AccessGrantConflict,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// The supplied `expectedRevision` no longer matches the durable revision.
    RevisionConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free connect-domain storage error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConnectStoreError {
    kind: ClientConnectStoreErrorKind,
    message: String,
}

impl ClientConnectStoreError {
    #[must_use]
    pub const fn kind(&self) -> ClientConnectStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientConnectStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientConnectStoreError {}

/// Connect-code, access-grant, and attempt ledger borrowing the sole
/// product-state `SQLite` authority.
pub struct ClientConnectLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable connect-code ledger on this same product-state
    /// database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn client_connect_ledger(
        &mut self,
    ) -> Result<ClientConnectLedger<'_>, ClientConnectStoreError> {
        ClientConnectLedger::new(self)
    }
}

impl<'storage> ClientConnectLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ClientConnectStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(CLIENT_CONNECT_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Registers one one-time connect code digest as `active` (plan 11.3,
    /// contract 2 initial transition).
    ///
    /// The Server only stores the digest; the code itself never persists. The
    /// referenced client node must exist and the expiry must lie in the
    /// future: publishing an already-expired code is a caller bug.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, an already-registered digest or code
    /// id, an expiry at or before `now`, or storage failure.
    pub fn publish(
        &mut self,
        publication: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
        publication.validate()?;
        validate_instant(now, "publication time")?;
        if publication.expires_at.0.as_str() <= now.0.as_str() {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code expiry must be in the future",
            ));
        }
        let transaction = self.transaction()?;
        require_client_node(&transaction, publication.client_node_id())?;
        let inserted = transaction
            .execute(
                "INSERT INTO client_connect_codes
                 (connect_code_id, code_digest, client_node_id, issued_by_instance_id,
                  generation, expires_at, remaining_attempts, state, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, 1)",
                params![
                    publication.connect_code_id(),
                    publication.code_digest(),
                    publication.client_node_id(),
                    publication.issued_by_instance_id(),
                    sql_integer(publication.generation)?,
                    publication.expires_at.0,
                    publication.remaining_attempts,
                    now.0,
                ],
            )
            .map_err(|sql| map_code_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::Storage,
                "connect code insert did not store exactly one row",
            ));
        }
        let record =
            load_connect_code(&transaction, publication.connect_code_id())?.ok_or_else(|| {
                error(
                    ClientConnectStoreErrorKind::CorruptState,
                    "published connect code row is missing after insert",
                )
            })?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Refreshes one connect code in a single transaction (contract 2:
    /// `active -> revoked` triggered by a Client refresh) by revoking the old
    /// code and publishing the replacement atomically.
    ///
    /// # Errors
    ///
    /// Rejects a stale `expectedRevision`, an old code that is not `active`,
    /// an invalid replacement publication, or storage failure.
    pub fn refresh_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
        replacement: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
        replacement.validate()?;
        validate_instant(now, "refresh time")?;
        if replacement.expires_at.0.as_str() <= now.0.as_str() {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code expiry must be in the future",
            ));
        }
        let transaction = self.transaction()?;
        let old = require_connect_code(&transaction, revocation.connect_code_id())?;
        ensure_code_revision(&old, revocation.expected_revision())?;
        if old.state != ConnectCodeState::Active {
            return Err(illegal_code_transition(&old, "refresh"));
        }
        let revoked = transaction
            .execute(
                "UPDATE client_connect_codes
                 SET state = 'revoked', revision = revision + 1
                 WHERE connect_code_id = ?1 AND state = 'active' AND revision = ?2",
                params![revocation.connect_code_id(), sql_integer(old.revision)?,],
            )
            .map_err(|sql| sql_error(&sql))?;
        if revoked != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::RevisionConflict,
                "connect code revision changed during refresh",
            ));
        }
        require_client_node(&transaction, replacement.client_node_id())?;
        let inserted = transaction
            .execute(
                "INSERT INTO client_connect_codes
                 (connect_code_id, code_digest, client_node_id, issued_by_instance_id,
                  generation, expires_at, remaining_attempts, state, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, 1)",
                params![
                    replacement.connect_code_id(),
                    replacement.code_digest(),
                    replacement.client_node_id(),
                    replacement.issued_by_instance_id(),
                    sql_integer(replacement.generation)?,
                    replacement.expires_at.0,
                    replacement.remaining_attempts,
                    now.0,
                ],
            )
            .map_err(|sql| map_code_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::Storage,
                "replacement connect code insert did not store exactly one row",
            ));
        }
        let record =
            load_connect_code(&transaction, replacement.connect_code_id())?.ok_or_else(|| {
                error(
                    ClientConnectStoreErrorKind::CorruptState,
                    "replacement connect code row is missing after insert",
                )
            })?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Revokes one connect code (contract 2: `active -> revoked`).
    ///
    /// Revoking an already-`revoked` code is an accepted idempotent replay
    /// that leaves the revision untouched; `consumed` and `expired` are
    /// terminal and reject the transition.
    ///
    /// # Errors
    ///
    /// Rejects an unknown code, a stale `expectedRevision`, an illegal
    /// terminal-state transition, or storage failure.
    pub fn revoke_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
    ) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
        let transaction = self.transaction()?;
        let record = require_connect_code(&transaction, revocation.connect_code_id())?;
        ensure_code_revision(&record, revocation.expected_revision())?;
        if record.state == ConnectCodeState::Revoked {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != ConnectCodeState::Active {
            return Err(illegal_code_transition(&record, "revoke"));
        }
        let updated = transaction
            .execute(
                "UPDATE client_connect_codes
                 SET state = 'revoked', revision = revision + 1
                 WHERE connect_code_id = ?1 AND state = 'active' AND revision = ?2",
                params![revocation.connect_code_id(), sql_integer(record.revision)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::RevisionConflict,
                "connect code revision changed during revoke",
            ));
        }
        let updated = require_connect_code(&transaction, revocation.connect_code_id())?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Records one failed verification attempt against an `active` code by
    /// decrementing `remainingAttempts` (plan 7.3, 11.3).
    ///
    /// Rate limiting itself is policy enforcement and never changes the code
    /// state; this only burns one of the code's finite attempts.
    ///
    /// # Errors
    ///
    /// Rejects an unknown code, a stale `expectedRevision`, a code that is
    /// not `active`, an exhausted attempt budget, or storage failure.
    pub fn record_failed_attempt(
        &mut self,
        connect_code_id: &str,
        expected_revision: u64,
    ) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
        validate_connect_code_id(connect_code_id)?;
        validate_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let record = require_connect_code(&transaction, connect_code_id)?;
        ensure_code_revision(&record, expected_revision)?;
        if record.state != ConnectCodeState::Active {
            return Err(illegal_code_transition(&record, "failed attempt"));
        }
        if record.remaining_attempts == 0 {
            return Err(error(
                ClientConnectStoreErrorKind::AttemptsExhausted,
                "connect code has no remaining attempts",
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE client_connect_codes
                 SET remaining_attempts = remaining_attempts - 1, revision = revision + 1
                 WHERE connect_code_id = ?1 AND state = 'active' AND revision = ?2",
                params![connect_code_id, sql_integer(record.revision)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::RevisionConflict,
                "connect code revision changed during failed attempt",
            ));
        }
        let updated = require_connect_code(&transaction, connect_code_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Projects due `active` codes to `expired` (contract 2 time judgement).
    ///
    /// The caller owns the timeout policy: every `active` code whose
    /// `expiresAt` is at or before `cutoff` is swept.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_codes_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectStoreError> {
        validate_instant(cutoff, "connect code expiry cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT connect_code_id, revision FROM client_connect_codes
                 WHERE state = 'active' AND expires_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let due = statement
            .query_map([cutoff.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut expired = Vec::with_capacity(due.len());
        for (connect_code_id, revision) in due {
            let revision = from_sql_integer(revision, "connect code revision")?;
            let updated = transaction
                .execute(
                    "UPDATE client_connect_codes
                     SET state = 'expired', revision = revision + 1
                     WHERE connect_code_id = ?1 AND state = 'active' AND revision = ?2",
                    params![connect_code_id, sql_integer(revision)?],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated == 1 {
                expired.push(connect_code_id);
            }
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(expired)
    }

    /// Consumes one `active` connect code and creates the derived access
    /// grant inside one transaction (contract 2 `active -> consumed`,
    /// contract 3 initial transition, plan 11.4 step 8-9).
    ///
    /// Exactly-once semantics: the consume is a single compare-and-swap on
    /// the code row (`state = 'active'` and the loaded revision), and the
    /// grant insert obeys the partial unique index of one active grant per
    /// user and client. A losing concurrent consume observes a terminal code
    /// and never produces a grant, because its whole transaction rolls back.
    ///
    /// The permissions follow plan 11.5 and are derived inside the same
    /// transaction: the first user ever granted on the Client receives
    /// `use+manage+share`, every later user receives `use`. The grant source
    /// is always `connect_code`; administrator and local-confirmation grants
    /// use [`ClientConnectLedger::create_grant`].
    ///
    /// # Errors
    ///
    /// Rejects an unknown or digest-mismatched code (indistinguishable by
    /// design so failures never reveal that a Client belongs to a user), a
    /// code that is not `active`, an expired code, an exhausted attempt
    /// budget, a generation mismatch with the challenge ACK, an unknown
    /// client node, an already-active grant for the user and client, or
    /// storage failure.
    pub fn consume_and_create_grant(
        &mut self,
        consume: &ConnectCodeConsume,
        issuance: &AccessGrantIssuance,
        now: &Instant,
    ) -> Result<ConnectGrantReceipt, ClientConnectStoreError> {
        issuance.validate()?;
        validate_instant(now, "consume time")?;
        let transaction = self.transaction()?;
        let code = load_connect_code(&transaction, consume.connect_code_id())?;
        let Some(code) = code else {
            return Err(unknown_connect_code());
        };
        ensure_code_consumable(&code, consume, now)?;
        if issuance.client_node_id() != code.client_node_id {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "access grant client node must match the consumed code",
            ));
        }
        let first_user = !grant_exists_for_client(&transaction, code.client_node_id.as_str())?;
        let permissions = if first_user {
            GrantPermissions::USE_MANAGE_SHARE
        } else {
            GrantPermissions::USE
        };
        let inserted = transaction
            .execute(
                "INSERT INTO client_access_grants
                 (client_access_grant_id, client_node_id, user_id, permissions, trust_mode,
                  state, grant_source, granted_by_user_id, expires_at, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', 'connect_code', ?6, ?7, ?8, 1)",
                params![
                    issuance.client_access_grant_id(),
                    code.client_node_id,
                    issuance.user_id(),
                    permissions.as_str(),
                    issuance.trust_mode().as_str(),
                    issuance.granted_by_user_id(),
                    issuance.expires_at().map(|instant| instant.0.clone()),
                    now.0,
                ],
            )
            .map_err(|sql| map_grant_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::Storage,
                "access grant insert did not store exactly one row",
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE client_connect_codes
                 SET state = 'consumed', revision = revision + 1
                 WHERE connect_code_id = ?1 AND state = 'active' AND revision = ?2",
                params![code.connect_code_id, sql_integer(code.revision)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            // The whole transaction aborts, so the grant insert above rolls
            // back as well: exactly one consume can ever win.
            return Err(error(
                ClientConnectStoreErrorKind::RevisionConflict,
                "connect code revision changed during consume",
            ));
        }
        let consumed = require_connect_code(&transaction, &code.connect_code_id)?;
        let grant = load_access_grant(&transaction, issuance.client_access_grant_id())?
            .ok_or_else(grant_missing_after_consume)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(ConnectGrantReceipt {
            code: consumed,
            grant,
            first_user,
        })
    }

    /// Creates one grant outside the connect-code flow (contract 3 initial
    /// transition via `administrator` or `local_confirmation`).
    ///
    /// The connect-code source is rejected here: that origin may only be
    /// produced by the atomic consume path, which is the sole place the
    /// required Device Client challenge ACK exists.
    ///
    /// # Errors
    ///
    /// Rejects the `connect_code` source, an unknown client node, an
    /// already-active grant for the user and client, or storage failure.
    pub fn create_grant(
        &mut self,
        issuance: &AccessGrantIssuance,
        grant_source: GrantSource,
        permissions: GrantPermissions,
        now: &Instant,
    ) -> Result<AccessGrantRecord, ClientConnectStoreError> {
        issuance.validate()?;
        if grant_source == GrantSource::ConnectCode {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code grants must be created through the atomic consume path",
            ));
        }
        validate_instant(now, "grant creation time")?;
        if let Some(expires_at) = issuance.expires_at()
            && expires_at.0.as_str() <= now.0.as_str()
        {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "access grant expiry must be in the future",
            ));
        }
        let transaction = self.transaction()?;
        require_client_node(&transaction, issuance.client_node_id())?;
        let inserted = transaction
            .execute(
                "INSERT INTO client_access_grants
                 (client_access_grant_id, client_node_id, user_id, permissions, trust_mode,
                  state, grant_source, granted_by_user_id, expires_at, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8, ?9, 1)",
                params![
                    issuance.client_access_grant_id(),
                    issuance.client_node_id(),
                    issuance.user_id(),
                    permissions.as_str(),
                    issuance.trust_mode().as_str(),
                    grant_source.as_str(),
                    issuance.granted_by_user_id(),
                    issuance.expires_at().map(|instant| instant.0.clone()),
                    now.0,
                ],
            )
            .map_err(|sql| map_grant_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::Storage,
                "access grant insert did not store exactly one row",
            ));
        }
        let record = load_access_grant(&transaction, issuance.client_access_grant_id())?
            .ok_or_else(|| {
                error(
                    ClientConnectStoreErrorKind::CorruptState,
                    "access grant row is missing after insert",
                )
            })?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Returns the active grant of one user on one client, if any.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or storage failure.
    pub fn active_grant(
        &self,
        client_node_id: &str,
        user_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectStoreError> {
        validate_client_node_id(client_node_id)?;
        validate_user_id(user_id)?;
        load_active_grant(self.connection()?, client_node_id, user_id)
    }

    /// Returns every active grant of one user across all clients.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical user identity or storage failure.
    pub fn active_grants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<AccessGrantRecord>, ClientConnectStoreError> {
        validate_user_id(user_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT client_access_grant_id, client_node_id, user_id, permissions,
                        trust_mode, state, grant_source, granted_by_user_id, expires_at,
                        created_at, revision
                 FROM client_access_grants
                 WHERE user_id = ?1 AND state = 'active'
                 ORDER BY created_at, client_access_grant_id",
            )
            .map_err(|sql| sql_error(&sql))?;
        let grants = statement
            .query_map([user_id], read_grant_row)
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        grants.into_iter().map(access_grant_from_row).collect()
    }

    /// Returns one durable access grant projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn grant_snapshot(
        &self,
        client_access_grant_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectStoreError> {
        validate_access_grant_id(client_access_grant_id)?;
        load_access_grant(self.connection()?, client_access_grant_id)
    }

    /// Revokes one access grant (contract 3: `active -> revoked`).
    ///
    /// Revocation takes effect immediately without waiting for the Device
    /// Client. Revoking an already-`revoked` grant is an accepted idempotent
    /// replay; `expired` is terminal and rejects the transition.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a stale `expectedRevision`, an illegal
    /// terminal-state transition, or storage failure.
    pub fn revoke_grant(
        &mut self,
        client_access_grant_id: &str,
        expected_revision: u64,
    ) -> Result<AccessGrantRecord, ClientConnectStoreError> {
        validate_access_grant_id(client_access_grant_id)?;
        validate_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let record = require_access_grant(&transaction, client_access_grant_id)?;
        ensure_grant_revision(&record, expected_revision)?;
        if record.state == AccessGrantState::Revoked {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != AccessGrantState::Active {
            return Err(error(
                ClientConnectStoreErrorKind::IllegalStateTransition,
                format!(
                    "access grant transition {} -> revoked is not legal",
                    record.state
                ),
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE client_access_grants
                 SET state = 'revoked', revision = revision + 1
                 WHERE client_access_grant_id = ?1 AND state = 'active' AND revision = ?2",
                params![client_access_grant_id, sql_integer(record.revision)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                ClientConnectStoreErrorKind::RevisionConflict,
                "access grant revision changed during revoke",
            ));
        }
        let updated = require_access_grant(&transaction, client_access_grant_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Projects due temporary grants to `expired` (contract 3 time
    /// judgement).
    ///
    /// Only `active` grants with a non-null `expiresAt` at or before `cutoff`
    /// are swept; `trusted` grants without an expiry never expire here.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_grants_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectStoreError> {
        validate_instant(cutoff, "access grant expiry cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT client_access_grant_id, revision FROM client_access_grants
                 WHERE state = 'active' AND expires_at IS NOT NULL AND expires_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let due = statement
            .query_map([cutoff.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut expired = Vec::with_capacity(due.len());
        for (client_access_grant_id, revision) in due {
            let revision = from_sql_integer(revision, "access grant revision")?;
            let updated = transaction
                .execute(
                    "UPDATE client_access_grants
                     SET state = 'expired', revision = revision + 1
                     WHERE client_access_grant_id = ?1 AND state = 'active' AND revision = ?2",
                    params![client_access_grant_id, sql_integer(revision)?],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated == 1 {
                expired.push(client_access_grant_id);
            }
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(expired)
    }

    /// Returns one durable connect code projection by identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical code identity or storage failure.
    pub fn code_snapshot(
        &self,
        connect_code_id: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectStoreError> {
        validate_connect_code_id(connect_code_id)?;
        load_connect_code(self.connection()?, connect_code_id)
    }

    /// Returns one durable connect code projection by presented digest.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical digest or storage failure.
    pub fn code_snapshot_by_digest(
        &self,
        code_digest: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectStoreError> {
        validate_sha256_digest(code_digest)?;
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT connect_code_id, code_digest, client_node_id, issued_by_instance_id,
                        generation, expires_at, remaining_attempts, state, created_at, revision
                 FROM client_connect_codes WHERE code_digest = ?1",
                [code_digest],
                read_code_row,
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?
            .map(connect_code_from_row)
            .transpose()
    }

    /// Records one failed connect attempt in the fixed window anchored at
    /// `window_started_at` (plan 11.3).
    ///
    /// The caller owns the window policy and derives the anchor from `now`
    /// with [`connect_attempt_window_anchor`]. A stored counter anchored at
    /// any other window is reset, so a new window always starts from one
    /// failed attempt.
    ///
    /// # Errors
    ///
    /// Rejects an invalid dimension subject key, a non-canonical anchor, or
    /// storage failure.
    pub fn record_connect_failure(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_started_at: &Instant,
    ) -> Result<ConnectAttemptState, ClientConnectStoreError> {
        validate_subject_key(dimension, subject_key)?;
        validate_instant(window_started_at, "attempt window anchor")?;
        let transaction = self.transaction()?;
        let stored = load_connect_attempt(&transaction, dimension, subject_key)?;
        let state = match stored {
            None => {
                transaction
                    .execute(
                        "INSERT INTO connect_attempts
                         (dimension, subject_key, window_started_at, failed_attempts)
                         VALUES (?1, ?2, ?3, 1)",
                        params![dimension.as_str(), subject_key, window_started_at.0],
                    )
                    .map_err(|sql| sql_error(&sql))?;
                ConnectAttemptState {
                    dimension,
                    subject_key: subject_key.to_owned(),
                    window_started_at: window_started_at.clone(),
                    failed_attempts: 1,
                }
            }
            Some(mut stored) => {
                if stored.window_started_at.0.as_str() == window_started_at.0.as_str() {
                    stored.failed_attempts += 1;
                } else {
                    stored.window_started_at = window_started_at.clone();
                    stored.failed_attempts = 1;
                }
                let failed_attempts = sql_integer(stored.failed_attempts)?;
                transaction
                    .execute(
                        "UPDATE connect_attempts
                         SET window_started_at = ?3, failed_attempts = ?4
                         WHERE dimension = ?1 AND subject_key = ?2",
                        params![
                            dimension.as_str(),
                            subject_key,
                            stored.window_started_at.0,
                            failed_attempts
                        ],
                    )
                    .map_err(|sql| sql_error(&sql))?;
                ConnectAttemptState {
                    dimension,
                    subject_key: subject_key.to_owned(),
                    window_started_at: stored.window_started_at,
                    failed_attempts: stored.failed_attempts,
                }
            }
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(state)
    }

    /// Returns the failed attempt count recorded in the fixed window
    /// anchored at `window_started_at`.
    ///
    /// A counter anchored at an older window reads as zero.
    ///
    /// # Errors
    ///
    /// Rejects an invalid dimension subject key, a non-canonical anchor, or
    /// storage failure.
    pub fn connect_failure_count(
        &self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_started_at: &Instant,
    ) -> Result<u64, ClientConnectStoreError> {
        validate_subject_key(dimension, subject_key)?;
        validate_instant(window_started_at, "attempt window anchor")?;
        let stored = load_connect_attempt(self.connection()?, dimension, subject_key)?;
        Ok(match stored {
            Some(stored) if stored.window_started_at.0.as_str() == window_started_at.0.as_str() => {
                stored.failed_attempts
            }
            _ => 0,
        })
    }

    /// Applies the caller's attempt threshold as policy: true when the
    /// current window already recorded at least `max_attempts` failures.
    ///
    /// # Errors
    ///
    /// Rejects a zero threshold, an invalid dimension subject key, a
    /// non-canonical anchor, or storage failure.
    pub fn connect_attempts_blocked(
        &self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_started_at: &Instant,
        max_attempts: u64,
    ) -> Result<bool, ClientConnectStoreError> {
        if max_attempts == 0 {
            return Err(error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect attempt threshold must be positive",
            ));
        }
        let count = self.connect_failure_count(dimension, subject_key, window_started_at)?;
        Ok(count >= max_attempts)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, ClientConnectStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, ClientConnectStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Anchor instant of the fixed attempt window that contains `now`.
///
/// The canonical `YYYY-MM-DDTHH:MM:SS.sssZ` instant is floored to the nearest
/// multiple of `window_seconds` since the Unix epoch, giving every attempt in
/// one wall-clock window the same durable anchor.
///
/// # Errors
///
/// Rejects a non-canonical instant or a window length outside
/// `1..=MAX_WINDOW_SECONDS`.
pub fn connect_attempt_window_anchor(
    now: &Instant,
    window_seconds: u64,
) -> Result<Instant, ClientConnectStoreError> {
    validate_instant(now, "attempt window instant")?;
    if window_seconds == 0 || window_seconds > MAX_WINDOW_SECONDS {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            "connect attempt window length is outside the supported range",
        ));
    }
    let bytes = now.0.as_bytes();
    let year = parse_two_or_four_digit_year(&bytes[0..4]);
    let month = parse_two_digits(&bytes[5..7]);
    let day = parse_two_digits(&bytes[8..10]);
    let hour = parse_two_digits(&bytes[11..13]);
    let minute = parse_two_digits(&bytes[14..16]);
    let second = parse_two_digits(&bytes[17..19]);
    let epoch_seconds = days_from_civil(year, i64::from(month), i64::from(day)) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    let window = i64::try_from(window_seconds).map_err(|_| {
        error(
            ClientConnectStoreErrorKind::InvalidInput,
            "connect attempt window length is outside the supported range",
        )
    })?;
    let anchored = div_floor(epoch_seconds, window) * window;
    let days = div_floor(anchored, 86_400);
    let remainder = anchored - days * 86_400;
    let (year, month, day) = civil_from_days(days);
    let anchored = format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.000Z",
        hour = remainder / 3_600,
        minute = (remainder % 3_600) / 60,
        second = remainder % 60,
    );
    let anchored = Instant(anchored);
    validate_instant(&anchored, "attempt window anchor")?;
    Ok(anchored)
}

fn parse_two_digits(digits: &[u8]) -> u8 {
    (digits[0] - b'0') * 10 + (digits[1] - b'0')
}

fn parse_two_or_four_digit_year(digits: &[u8]) -> i64 {
    let mut year = 0_i64;
    for digit in digits {
        year = year * 10 + i64::from(digit - b'0');
    }
    year
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_of_period = (month + 9) % 12;
    let day_of_year = (153 * month_of_period + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Civil date for days since the Unix epoch (Howard Hinnant's algorithm).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_of_period = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_of_period + 2) / 5 + 1;
    let month = if month_of_period < 10 {
        month_of_period + 3
    } else {
        month_of_period - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

const fn div_floor(numerator: i64, denominator: i64) -> i64 {
    let quotient = numerator / denominator;
    if numerator % denominator != 0 && ((numerator < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

#[derive(Clone, Debug)]
struct StoredCodeRow {
    connect_code_id: String,
    code_digest: String,
    client_node_id: String,
    issued_by_instance_id: String,
    generation: i64,
    expires_at: String,
    remaining_attempts: i64,
    state: String,
    created_at: String,
    revision: i64,
}

fn read_code_row(row: &rusqlite::Row<'_>) -> Result<StoredCodeRow, rusqlite::Error> {
    Ok(StoredCodeRow {
        connect_code_id: row.get(0)?,
        code_digest: row.get(1)?,
        client_node_id: row.get(2)?,
        issued_by_instance_id: row.get(3)?,
        generation: row.get(4)?,
        expires_at: row.get(5)?,
        remaining_attempts: row.get(6)?,
        state: row.get(7)?,
        created_at: row.get(8)?,
        revision: row.get(9)?,
    })
}

fn connect_code_from_row(row: StoredCodeRow) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
    let state = ConnectCodeState::parse(&row.state)?;
    let created_at = parse_stored_instant(&row.created_at, "connect code creation")?;
    let expires_at = parse_stored_instant(&row.expires_at, "connect code expiry")?;
    let generation = from_sql_integer(row.generation, "connect code generation")?;
    if generation == 0 {
        return Err(error(
            ClientConnectStoreErrorKind::CorruptState,
            "stored connect code generation is zero",
        ));
    }
    let remaining = u32::try_from(row.remaining_attempts).map_err(|_| {
        error(
            ClientConnectStoreErrorKind::CorruptState,
            "stored connect code remaining attempts is out of range",
        )
    })?;
    Ok(ConnectCodeRecord {
        connect_code_id: row.connect_code_id,
        code_digest: row.code_digest,
        client_node_id: row.client_node_id,
        issued_by_instance_id: row.issued_by_instance_id,
        generation,
        expires_at,
        remaining_attempts: remaining,
        state,
        created_at,
        revision: from_sql_integer(row.revision, "connect code revision")?,
    })
}

#[derive(Clone, Debug)]
struct StoredGrantRow {
    client_access_grant_id: String,
    client_node_id: String,
    user_id: String,
    permissions: String,
    trust_mode: String,
    state: String,
    grant_source: String,
    granted_by_user_id: String,
    expires_at: Option<String>,
    created_at: String,
    revision: i64,
}

fn read_grant_row(row: &rusqlite::Row<'_>) -> Result<StoredGrantRow, rusqlite::Error> {
    Ok(StoredGrantRow {
        client_access_grant_id: row.get(0)?,
        client_node_id: row.get(1)?,
        user_id: row.get(2)?,
        permissions: row.get(3)?,
        trust_mode: row.get(4)?,
        state: row.get(5)?,
        grant_source: row.get(6)?,
        granted_by_user_id: row.get(7)?,
        expires_at: row.get(8)?,
        created_at: row.get(9)?,
        revision: row.get(10)?,
    })
}

fn access_grant_from_row(
    row: StoredGrantRow,
) -> Result<AccessGrantRecord, ClientConnectStoreError> {
    let state = AccessGrantState::parse(&row.state)?;
    let trust_mode = GrantTrustMode::parse(&row.trust_mode)?;
    let grant_source = GrantSource::parse(&row.grant_source)?;
    let permissions = GrantPermissions::parse(&row.permissions)?;
    let created_at = parse_stored_instant(&row.created_at, "access grant creation")?;
    let expires_at = row
        .expires_at
        .map(|value| parse_stored_instant(&value, "access grant expiry"))
        .transpose()?;
    Ok(AccessGrantRecord {
        client_access_grant_id: row.client_access_grant_id,
        client_node_id: row.client_node_id,
        user_id: row.user_id,
        permissions,
        trust_mode,
        state,
        grant_source,
        granted_by_user_id: row.granted_by_user_id,
        expires_at,
        created_at,
        revision: from_sql_integer(row.revision, "access grant revision")?,
    })
}

#[derive(Clone, Debug)]
struct StoredAttemptRow {
    window_started_at: Instant,
    failed_attempts: u64,
}

fn load_connect_attempt(
    connection: &rusqlite::Connection,
    dimension: AttemptDimension,
    subject_key: &str,
) -> Result<Option<StoredAttemptRow>, ClientConnectStoreError> {
    let raw = connection
        .query_row(
            "SELECT window_started_at, failed_attempts
             FROM connect_attempts WHERE dimension = ?1 AND subject_key = ?2",
            params![dimension.as_str(), subject_key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((window_started_at, failed_attempts)) = raw else {
        return Ok(None);
    };
    let window_started_at = parse_stored_instant(&window_started_at, "attempt window anchor")?;
    let failed_attempts = u64::try_from(failed_attempts).map_err(|_| {
        error(
            ClientConnectStoreErrorKind::CorruptState,
            "stored connect attempt count is negative",
        )
    })?;
    Ok(Some(StoredAttemptRow {
        window_started_at,
        failed_attempts,
    }))
}

fn load_connect_code(
    connection: &rusqlite::Connection,
    connect_code_id: &str,
) -> Result<Option<ConnectCodeRecord>, ClientConnectStoreError> {
    connection
        .query_row(
            "SELECT connect_code_id, code_digest, client_node_id, issued_by_instance_id,
                    generation, expires_at, remaining_attempts, state, created_at, revision
             FROM client_connect_codes WHERE connect_code_id = ?1",
            [connect_code_id],
            read_code_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(connect_code_from_row)
        .transpose()
}

fn require_connect_code(
    connection: &rusqlite::Connection,
    connect_code_id: &str,
) -> Result<ConnectCodeRecord, ClientConnectStoreError> {
    load_connect_code(connection, connect_code_id)?.ok_or_else(unknown_connect_code)
}

/// Applies the frozen consume preconditions to a loaded code row.
///
/// A wrong presented digest must be indistinguishable from an unknown code so
/// a failed attempt never reveals that a Client belongs to any user
/// (plan 17.5).
fn ensure_code_consumable(
    code: &ConnectCodeRecord,
    consume: &ConnectCodeConsume,
    now: &Instant,
) -> Result<(), ClientConnectStoreError> {
    if code.code_digest != consume.presented_code_digest() {
        return Err(unknown_connect_code());
    }
    if code.state != ConnectCodeState::Active {
        return Err(error(
            ClientConnectStoreErrorKind::CodeNotActive,
            format!(
                "connect code is {} and can no longer be consumed",
                code.state
            ),
        ));
    }
    if code.generation != consume.ack_generation() {
        return Err(error(
            ClientConnectStoreErrorKind::GenerationMismatch,
            "challenge acknowledgement names a different code generation",
        ));
    }
    if code.expires_at.0.as_str() <= now.0.as_str() {
        return Err(error(
            ClientConnectStoreErrorKind::ConnectCodeExpired,
            "connect code expiry has passed",
        ));
    }
    if code.remaining_attempts == 0 {
        return Err(error(
            ClientConnectStoreErrorKind::AttemptsExhausted,
            "connect code has no remaining attempts",
        ));
    }
    Ok(())
}

fn grant_missing_after_consume() -> ClientConnectStoreError {
    error(
        ClientConnectStoreErrorKind::CorruptState,
        "access grant row is missing after consume",
    )
}

fn load_access_grant(
    connection: &rusqlite::Connection,
    client_access_grant_id: &str,
) -> Result<Option<AccessGrantRecord>, ClientConnectStoreError> {
    connection
        .query_row(
            "SELECT client_access_grant_id, client_node_id, user_id, permissions, trust_mode,
                    state, grant_source, granted_by_user_id, expires_at, created_at, revision
             FROM client_access_grants WHERE client_access_grant_id = ?1",
            [client_access_grant_id],
            read_grant_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(access_grant_from_row)
        .transpose()
}

fn require_access_grant(
    connection: &rusqlite::Connection,
    client_access_grant_id: &str,
) -> Result<AccessGrantRecord, ClientConnectStoreError> {
    load_access_grant(connection, client_access_grant_id)?.ok_or_else(|| {
        error(
            ClientConnectStoreErrorKind::UnknownAccessGrant,
            "access grant does not exist",
        )
    })
}

fn load_active_grant(
    connection: &rusqlite::Connection,
    client_node_id: &str,
    user_id: &str,
) -> Result<Option<AccessGrantRecord>, ClientConnectStoreError> {
    connection
        .query_row(
            "SELECT client_access_grant_id, client_node_id, user_id, permissions, trust_mode,
                    state, grant_source, granted_by_user_id, expires_at, created_at, revision
             FROM client_access_grants
             WHERE client_node_id = ?1 AND user_id = ?2 AND state = 'active'",
            params![client_node_id, user_id],
            read_grant_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(access_grant_from_row)
        .transpose()
}

fn grant_exists_for_client(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<bool, ClientConnectStoreError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM client_access_grants WHERE client_node_id = ?1)",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    Ok(exists == 1)
}

fn require_client_node(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<(), ClientConnectStoreError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM client_nodes WHERE client_node_id = ?1)",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    if exists != 1 {
        return Err(error(
            ClientConnectStoreErrorKind::UnknownClientNode,
            "client node does not exist",
        ));
    }
    Ok(())
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), ClientConnectStoreError> {
    validate_columns(
        connection,
        "client_connect_codes",
        &[
            "connect_code_id",
            "code_digest",
            "client_node_id",
            "issued_by_instance_id",
            "generation",
            "expires_at",
            "remaining_attempts",
            "state",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "client_access_grants",
        &[
            "client_access_grant_id",
            "client_node_id",
            "user_id",
            "permissions",
            "trust_mode",
            "state",
            "grant_source",
            "granted_by_user_id",
            "expires_at",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "connect_attempts",
        &[
            "dimension",
            "subject_key",
            "window_started_at",
            "failed_attempts",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), ClientConnectStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            ClientConnectStoreErrorKind::CorruptState,
            "client connect schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_connect_code_id(value: &str) -> Result<(), ClientConnectStoreError> {
    validate_crockford_id(value, "cct_", "connect code id")
}

fn validate_access_grant_id(value: &str) -> Result<(), ClientConnectStoreError> {
    validate_crockford_id(value, "cag_", "access grant id")
}

fn validate_client_node_id(value: &str) -> Result<(), ClientConnectStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_client_instance_id(value: &str) -> Result<(), ClientConnectStoreError> {
    validate_crockford_id(value, "cix_", "client instance id")
}

fn validate_user_id(value: &str) -> Result<(), ClientConnectStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), ClientConnectStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_byte) {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
}

const fn is_crockford_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn validate_subject_key(
    dimension: AttemptDimension,
    subject_key: &str,
) -> Result<(), ClientConnectStoreError> {
    if subject_key.is_empty() || subject_key.len() > MAX_SUBJECT_KEY_BYTES {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            "connect attempt subject key must contain 1 to 128 bytes",
        ));
    }
    if subject_key
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == 0)
    {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            "connect attempt subject key must not contain control bytes",
        ));
    }
    if dimension == AttemptDimension::User {
        validate_user_id(subject_key)?;
    }
    Ok(())
}

fn validate_sha256_digest(value: &str) -> Result<(), ClientConnectStoreError> {
    let canonical = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if canonical {
        Ok(())
    } else {
        Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            "connect code digest is not canonical SHA-256",
        ))
    }
}

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant, label: &str) -> Result<(), ClientConnectStoreError> {
    let bytes = value.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    let valid = bytes.len() == 24
        && bytes[23] == b'Z'
        && punctuation
            .iter()
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || index == 23 || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, ClientConnectStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn validate_revision(value: u64) -> Result<(), ClientConnectStoreError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientConnectStoreErrorKind::InvalidInput,
            "expected revision exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn ensure_code_revision(
    record: &ConnectCodeRecord,
    expected_revision: u64,
) -> Result<(), ClientConnectStoreError> {
    if record.revision != expected_revision {
        return Err(error(
            ClientConnectStoreErrorKind::RevisionConflict,
            "connect code revision does not match expectedRevision",
        ));
    }
    Ok(())
}

fn ensure_grant_revision(
    record: &AccessGrantRecord,
    expected_revision: u64,
) -> Result<(), ClientConnectStoreError> {
    if record.revision != expected_revision {
        return Err(error(
            ClientConnectStoreErrorKind::RevisionConflict,
            "access grant revision does not match expectedRevision",
        ));
    }
    Ok(())
}

fn illegal_code_transition(record: &ConnectCodeRecord, action: &str) -> ClientConnectStoreError {
    error(
        ClientConnectStoreErrorKind::IllegalStateTransition,
        format!(
            "connect code transition {} during {action} is not legal",
            record.state
        ),
    )
}

fn map_code_insert_sql(sql: &rusqlite::Error) -> ClientConnectStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                ClientConnectStoreErrorKind::ConnectCodeDigestConflict,
                "connect code digest is already registered",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                ClientConnectStoreErrorKind::ConnectCodeIdConflict,
                "connect code id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => error(
                ClientConnectStoreErrorKind::UnknownClientNode,
                "client node does not exist",
            ),
            _ => error(
                ClientConnectStoreErrorKind::InvalidInput,
                "connect code violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn map_grant_insert_sql(sql: &rusqlite::Error) -> ClientConnectStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                ClientConnectStoreErrorKind::AccessGrantConflict,
                "an active access grant for this user and client already exists",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                ClientConnectStoreErrorKind::AccessGrantConflict,
                "access grant id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => error(
                ClientConnectStoreErrorKind::UnknownClientNode,
                "client node does not exist",
            ),
            _ => error(
                ClientConnectStoreErrorKind::InvalidInput,
                "access grant violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn unknown_connect_code() -> ClientConnectStoreError {
    error(
        ClientConnectStoreErrorKind::UnknownConnectCode,
        "connect code does not exist",
    )
}

fn sql_integer(value: u64) -> Result<i64, ClientConnectStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            ClientConnectStoreErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, ClientConnectStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            ClientConnectStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientConnectStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> ClientConnectStoreError {
    error(
        ClientConnectStoreErrorKind::Storage,
        format!("client connect storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> ClientConnectStoreError {
    error(
        ClientConnectStoreErrorKind::Storage,
        "client connect storage operation failed",
    )
}

fn error(kind: ClientConnectStoreErrorKind, message: impl Into<String>) -> ClientConnectStoreError {
    ClientConnectStoreError {
        kind,
        message: message.into(),
    }
}
