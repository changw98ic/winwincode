// SPDX-License-Identifier: Apache-2.0

//! Connect-code and access-grant application services over the durable
//! Server-side connect ledger.
//!
//! The Control Plane owns the UU-style connection flow (plan 11): publishing
//! one-time connect code digests, consuming a code exactly once together with
//! the derived `ClientAccessGrant`, revocation and refresh, temporary-grant
//! expiry judgement, and the fixed-window connect attempt counters that
//! throttle the user, IP, and Client dimensions. Semantics follow the frozen
//! state machine in `docs/contracts/client-control-state-machines.md`
//! (contracts 2 and 3).

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    AccessChallengeCreation, AccessChallengeRecord, AccessGrantIssuance, AccessGrantRecord,
    AttemptDimension, ClientConnectStoreError, ClientConnectStoreErrorKind, ConnectAttemptState,
    ConnectAuditEntry, ConnectChallengeVerdict, ConnectCodeConsume, ConnectCodePublication,
    ConnectCodeRecord, ConnectCodeRevocation, ConnectGrantReceipt, GrantPermissions, GrantSource,
    SqliteStorage,
};

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientConnectServiceErrorKind {
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
    /// A challenge id is already used, or the challenge was already settled.
    ChallengeConflict,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// The supplied `expectedRevision` no longer matches the durable revision.
    RevisionConflict,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free connect-domain service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConnectServiceError {
    kind: ClientConnectServiceErrorKind,
    message: String,
}

impl ClientConnectServiceError {
    #[must_use]
    pub const fn kind(&self) -> ClientConnectServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientConnectServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientConnectServiceError {}

impl From<ClientConnectStoreError> for ClientConnectServiceError {
    fn from(source: ClientConnectStoreError) -> Self {
        Self {
            kind: match source.kind() {
                ClientConnectStoreErrorKind::InvalidInput => {
                    ClientConnectServiceErrorKind::InvalidInput
                }
                ClientConnectStoreErrorKind::UnknownClientNode => {
                    ClientConnectServiceErrorKind::UnknownClientNode
                }
                ClientConnectStoreErrorKind::UnknownConnectCode => {
                    ClientConnectServiceErrorKind::UnknownConnectCode
                }
                ClientConnectStoreErrorKind::UnknownAccessGrant => {
                    ClientConnectServiceErrorKind::UnknownAccessGrant
                }
                ClientConnectStoreErrorKind::CodeNotActive => {
                    ClientConnectServiceErrorKind::CodeNotActive
                }
                ClientConnectStoreErrorKind::ConnectCodeExpired => {
                    ClientConnectServiceErrorKind::ConnectCodeExpired
                }
                ClientConnectStoreErrorKind::AttemptsExhausted => {
                    ClientConnectServiceErrorKind::AttemptsExhausted
                }
                ClientConnectStoreErrorKind::GenerationMismatch => {
                    ClientConnectServiceErrorKind::GenerationMismatch
                }
                ClientConnectStoreErrorKind::ConnectCodeDigestConflict => {
                    ClientConnectServiceErrorKind::ConnectCodeDigestConflict
                }
                ClientConnectStoreErrorKind::ConnectCodeIdConflict => {
                    ClientConnectServiceErrorKind::ConnectCodeIdConflict
                }
                ClientConnectStoreErrorKind::AccessGrantConflict => {
                    ClientConnectServiceErrorKind::AccessGrantConflict
                }
                ClientConnectStoreErrorKind::ChallengeConflict => {
                    ClientConnectServiceErrorKind::ChallengeConflict
                }
                ClientConnectStoreErrorKind::IllegalStateTransition => {
                    ClientConnectServiceErrorKind::IllegalStateTransition
                }
                ClientConnectStoreErrorKind::RevisionConflict => {
                    ClientConnectServiceErrorKind::RevisionConflict
                }
                ClientConnectStoreErrorKind::CorruptState => {
                    ClientConnectServiceErrorKind::CorruptState
                }
                ClientConnectStoreErrorKind::Storage => ClientConnectServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

/// Connect-code application service over one storage connection.
///
/// Owns the code lifecycle (publish, refresh, revoke, expire) and the atomic
/// consume-and-grant boundary, plus the durable fixed-window connect attempt
/// counters that throttle code verification.
pub struct ConnectCodeService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ConnectCodeService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Registers one one-time connect code digest as `active`.
    ///
    /// Only the SHA-256 digest persists; the 8-digit code itself never
    /// reaches the Server (plan 11.3).
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, an already-registered digest or code
    /// id, an expiry at or before `now`, or storage failure.
    pub fn publish(
        &mut self,
        publication: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .publish(publication, now)?)
    }

    /// Refreshes one connect code: revokes the old `active` code and
    /// publishes the replacement in one transaction (plan 11.1 code refresh).
    ///
    /// # Errors
    ///
    /// Rejects a stale `expectedRevision`, an old code that is not `active`,
    /// an invalid replacement, or storage failure.
    pub fn refresh_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
        replacement: &ConnectCodePublication,
        now: &Instant,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .refresh_code(revocation, replacement, now)?)
    }

    /// Revokes one connect code; revoking an already-`revoked` code is an
    /// accepted idempotent replay.
    ///
    /// # Errors
    ///
    /// Rejects an unknown code, a stale `expectedRevision`, an illegal
    /// terminal-state transition, or storage failure.
    pub fn revoke_code(
        &mut self,
        revocation: &ConnectCodeRevocation,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .revoke_code(revocation)?)
    }

    /// Records one failed verification attempt, burning one of the code's
    /// finite `remainingAttempts`.
    ///
    /// # Errors
    ///
    /// Rejects an unknown code, a stale `expectedRevision`, a code that is
    /// not `active`, an exhausted attempt budget, or storage failure.
    pub fn record_failed_attempt(
        &mut self,
        connect_code_id: &str,
        expected_revision: u64,
    ) -> Result<ConnectCodeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_failed_attempt(connect_code_id, expected_revision)?)
    }

    /// Projects due `active` codes to `expired` (contract 2 time judgement).
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_codes_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .expire_codes_due(cutoff)?)
    }

    /// Returns one durable connect code projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical code identity or storage failure.
    pub fn code_snapshot(
        &mut self,
        connect_code_id: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .code_snapshot(connect_code_id)?)
    }

    /// Returns one durable connect code projection by presented digest.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical digest or storage failure.
    pub fn code_snapshot_by_digest(
        &mut self,
        code_digest: &str,
    ) -> Result<Option<ConnectCodeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .code_snapshot_by_digest(code_digest)?)
    }

    /// Consumes one `active` connect code and creates the derived access
    /// grant in one storage transaction (plan 11.4, contract 2 and 3).
    ///
    /// Exactly one concurrent consume wins: the consume is a compare-and-swap
    /// on the `active` code row, and the losing transaction rolls back
    /// entirely, so no second grant can appear. The first user ever granted
    /// on the Client receives `use+manage+share`; every later user receives
    /// `use` (plan 11.5). The expiry and remaining-attempt budgets are
    /// validated inside the same transaction.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or digest-mismatched code (indistinguishable by
    /// design), a code that is not `active`, an expired code, an exhausted
    /// attempt budget, a challenge-ACK generation mismatch, an unknown client
    /// node, an already-active grant for the user and client, or storage
    /// failure.
    pub fn consume_and_grant(
        &mut self,
        consume: &ConnectCodeConsume,
        issuance: &AccessGrantIssuance,
        now: &Instant,
    ) -> Result<ConnectGrantReceipt, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .consume_and_create_grant(consume, issuance, now)?)
    }

    /// Records one failed connect attempt in the fixed window anchored at
    /// `window_anchor` (plan 11.3 user, IP, and Client throttling).
    ///
    /// # Errors
    ///
    /// Rejects an invalid subject key, a non-canonical anchor, or storage
    /// failure.
    pub fn record_connect_failure(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
    ) -> Result<ConnectAttemptState, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_connect_failure(dimension, subject_key, window_anchor)?)
    }

    /// Returns the failed attempt count recorded in the window anchored at
    /// `window_anchor`; a counter anchored at an older window reads as zero.
    ///
    /// # Errors
    ///
    /// Rejects an invalid subject key, a non-canonical anchor, or storage
    /// failure.
    pub fn connect_failure_count(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
    ) -> Result<u64, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .connect_failure_count(dimension, subject_key, window_anchor)?)
    }

    /// Applies the caller's attempt threshold as policy: true when the
    /// current window already recorded at least `max_attempts` failures.
    ///
    /// # Errors
    ///
    /// Rejects a zero threshold, an invalid subject key, a non-canonical
    /// anchor, or storage failure.
    pub fn connect_attempts_blocked(
        &mut self,
        dimension: AttemptDimension,
        subject_key: &str,
        window_anchor: &Instant,
        max_attempts: u64,
    ) -> Result<bool, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .connect_attempts_blocked(dimension, subject_key, window_anchor, max_attempts)?)
    }

    /// Creates one pending `client.access.challenge` (plan 11.4, step 5).
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical command, an unknown client node, an
    /// already-used challenge id, or storage failure.
    pub fn create_challenge(
        &mut self,
        creation: &AccessChallengeCreation,
        now: &Instant,
    ) -> Result<AccessChallengeRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .create_challenge(creation, now)?)
    }

    /// Settles one pending challenge with the Device Client's challenge-ACK
    /// verdict (plan 11.4, step 7). Unknown or mismatched acknowledgements
    /// settle nothing and read as `None`.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical identity or storage failure.
    pub fn settle_challenge(
        &mut self,
        challenge_id: &str,
        client_node_id: &str,
        connect_code_id: &str,
        verdict: ConnectChallengeVerdict,
        now: &Instant,
    ) -> Result<Option<AccessChallengeRecord>, ClientConnectServiceError> {
        Ok(self.storage.client_connect_ledger()?.settle_challenge(
            challenge_id,
            client_node_id,
            connect_code_id,
            verdict,
            now,
        )?)
    }

    /// Returns one durable access challenge projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical challenge identity or storage failure.
    pub fn challenge_snapshot(
        &mut self,
        challenge_id: &str,
    ) -> Result<Option<AccessChallengeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .challenge_snapshot(challenge_id)?)
    }

    /// Returns the one live pending challenge of a user on a client for a
    /// connect code, if any.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or storage failure.
    pub fn pending_challenge_for_subject(
        &mut self,
        client_node_id: &str,
        requester_user_id: &str,
        connect_code_id: &str,
    ) -> Result<Option<AccessChallengeRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .pending_challenge_for_subject(client_node_id, requester_user_id, connect_code_id)?)
    }

    /// Appends one connect-domain authorization audit entry.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical entry, an already-used audit id, or storage
    /// failure.
    pub fn record_connect_audit(
        &mut self,
        entry: &ConnectAuditEntry,
    ) -> Result<(), ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .record_connect_audit(entry)?)
    }
}

/// Access-grant application service over one storage connection.
///
/// Owns grant creation outside the connect-code flow (administrator and
/// local-confirmation origins), active-grant lookups, immediate revocation,
/// and the temporary-grant expiry judgement.
pub struct AccessGrantService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> AccessGrantService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Creates one grant with the `administrator` or `local_confirmation`
    /// source.
    ///
    /// The `connect_code` source is rejected here: that origin may only be
    /// produced by the atomic consume path, which owns the required Device
    /// Client challenge ACK.
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
    ) -> Result<AccessGrantRecord, ClientConnectServiceError> {
        Ok(self.storage.client_connect_ledger()?.create_grant(
            issuance,
            grant_source,
            permissions,
            now,
        )?)
    }

    /// Returns the active grant of one user on one client, if any.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or storage failure.
    pub fn active_grant(
        &mut self,
        client_node_id: &str,
        user_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_grant(client_node_id, user_id)?)
    }

    /// Returns every active grant of one user across all clients.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical user identity or storage failure.
    pub fn active_grants_for_user(
        &mut self,
        user_id: &str,
    ) -> Result<Vec<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .active_grants_for_user(user_id)?)
    }

    /// Returns one durable access grant projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn grant_snapshot(
        &mut self,
        client_access_grant_id: &str,
    ) -> Result<Option<AccessGrantRecord>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .grant_snapshot(client_access_grant_id)?)
    }

    /// Revokes one grant; revocation takes effect immediately without
    /// waiting for the Device Client (contract 3).
    ///
    /// Revoking an already-`revoked` grant is an accepted idempotent replay.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a stale `expectedRevision`, an illegal
    /// terminal-state transition, or storage failure.
    pub fn revoke_grant(
        &mut self,
        client_access_grant_id: &str,
        expected_revision: u64,
    ) -> Result<AccessGrantRecord, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .revoke_grant(client_access_grant_id, expected_revision)?)
    }

    /// Projects due temporary grants to `expired` (contract 3 time
    /// judgement); trusted grants without an expiry never expire here.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_grants_due(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientConnectServiceError> {
        Ok(self
            .storage
            .client_connect_ledger()?
            .expire_grants_due(cutoff)?)
    }
}
