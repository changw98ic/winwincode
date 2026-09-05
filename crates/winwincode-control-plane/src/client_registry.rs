// SPDX-License-Identifier: Apache-2.0

//! `ClientNode` registry application service over the durable Server-side
//! registry.
//!
//! The Control Plane owns the persisted projection of device-reported Client
//! facts (ADR-0030): registration, presence transitions, heartbeat ageing, and
//! the per-client bidirectional exchange cursors. Presence semantics follow the
//! frozen state machine in `docs/contracts/client-control-state-machines.md`;
//! every mutation carries the caller's `expectedRevision` so concurrent
//! exchange and admin paths fail closed instead of overwriting each other.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    ClientExchangeCursors, ClientNodeRecord, ClientNodeRegistration, ClientNodeRegistrationReceipt,
    ClientPresenceState, ClientRegistryError, ClientRegistryErrorKind, SqliteStorage,
};

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRegistryServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// The identity binding conflicts with durable facts or is terminal.
    IdentityConflict,
    /// The supplied `expectedRevision` no longer matches the durable revision.
    RevisionConflict,
    /// The requested presence change is not a legal state machine transition.
    PresenceTransition,
    /// A durable row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free `ClientNode` registry service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRegistryServiceError {
    kind: ClientRegistryServiceErrorKind,
    message: String,
}

impl ClientRegistryServiceError {
    #[must_use]
    pub const fn kind(&self) -> ClientRegistryServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientRegistryServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientRegistryServiceError {}

impl From<ClientRegistryError> for ClientRegistryServiceError {
    fn from(source: ClientRegistryError) -> Self {
        Self {
            kind: match source.kind() {
                ClientRegistryErrorKind::InvalidInput => {
                    ClientRegistryServiceErrorKind::InvalidInput
                }
                ClientRegistryErrorKind::UnknownClientNode => {
                    ClientRegistryServiceErrorKind::UnknownClientNode
                }
                ClientRegistryErrorKind::IdentityConflict => {
                    ClientRegistryServiceErrorKind::IdentityConflict
                }
                ClientRegistryErrorKind::RevisionConflict => {
                    ClientRegistryServiceErrorKind::RevisionConflict
                }
                ClientRegistryErrorKind::PresenceTransition => {
                    ClientRegistryServiceErrorKind::PresenceTransition
                }
                ClientRegistryErrorKind::CorruptState => {
                    ClientRegistryServiceErrorKind::CorruptState
                }
                ClientRegistryErrorKind::Storage => ClientRegistryServiceErrorKind::Storage,
            },
            message: source.to_string(),
        }
    }
}

/// `ClientNode` registry application service over one storage connection.
pub struct ClientRegistryService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ClientRegistryService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Registers a Device Client identity or refreshes its device-reported
    /// projection under `expectedRevision` compare-and-swap.
    ///
    /// A first registration creates the identity in `pending_enrollment` with
    /// zeroed exchange cursors. `revoked` identities are never re-enrollable.
    ///
    /// # Errors
    ///
    /// Rejects invalid registration facts, a conflicting identity binding,
    /// terminal identity reuse, a stale `expectedRevision`, or storage failure.
    pub fn register(
        &mut self,
        registration: &ClientNodeRegistration,
        expected_revision: u64,
        now: &Instant,
    ) -> Result<ClientNodeRegistrationReceipt, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .register(registration, expected_revision, now)?)
    }

    /// Returns one durable `ClientNode` projection.
    ///
    /// # Errors
    ///
    /// Rejects corrupt durable rows or storage failure.
    pub fn snapshot(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<ClientNodeRecord>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .snapshot(client_node_id)?)
    }

    /// Returns one durable `ClientNode` projection by its public device
    /// number (plan 11.2: the public id only locates one Client).
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical public id, corrupt durable rows, or storage
    /// failure.
    pub fn snapshot_by_public_client_id(
        &mut self,
        public_client_id: &str,
    ) -> Result<Option<ClientNodeRecord>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .snapshot_by_public_client_id(public_client_id)?)
    }

    /// Applies one presence state transition under `expectedRevision` CAS.
    ///
    /// Only frozen-state-machine transitions are accepted; the current state is
    /// an accepted idempotent replay.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a stale `expectedRevision`, an illegal
    /// transition, or storage failure.
    pub fn update_presence(
        &mut self,
        client_node_id: &str,
        target: ClientPresenceState,
        expected_revision: u64,
    ) -> Result<ClientNodeRecord, ClientRegistryServiceError> {
        Ok(self.storage.client_node_registry()?.update_presence(
            client_node_id,
            target,
            expected_revision,
        )?)
    }

    /// Records one accepted Device Client heartbeat under `expectedRevision`
    /// CAS, refreshing the heartbeat instant and the reported running Worker
    /// session count.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a stale `expectedRevision`, a heartbeat
    /// from `pending_enrollment` or `revoked`, or storage failure.
    pub fn heartbeat(
        &mut self,
        client_node_id: &str,
        reported_running_worker_sessions: u32,
        now: &Instant,
        expected_revision: u64,
    ) -> Result<ClientNodeRecord, ClientRegistryServiceError> {
        Ok(self.storage.client_node_registry()?.heartbeat(
            client_node_id,
            reported_running_worker_sessions,
            now,
            expected_revision,
        )?)
    }

    /// Projects unreachable `online` and `degraded` devices to `offline`.
    ///
    /// The caller owns the timeout policy through `cutoff`; every client whose
    /// last accepted heartbeat is at or before it is swept.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn sweep_offline(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientRegistryServiceError> {
        Ok(self.storage.client_node_registry()?.sweep_offline(cutoff)?)
    }

    /// Returns the durable per-client bidirectional exchange cursors.
    ///
    /// # Errors
    ///
    /// Rejects corrupt durable rows or storage failure.
    pub fn exchange_cursors(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<ClientExchangeCursors>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .exchange_cursors(client_node_id)?)
    }

    /// Advances the per-client bidirectional exchange acknowledgement cursors
    /// monotonically so a Server restart never replays settled frames.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, an out-of-range sequence, or storage
    /// failure.
    pub fn advance_exchange_cursors(
        &mut self,
        client_node_id: &str,
        client_to_server_ack_sequence: u64,
        server_to_client_ack_sequence: u64,
    ) -> Result<ClientExchangeCursors, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .advance_exchange_cursors(
                client_node_id,
                client_to_server_ack_sequence,
                server_to_client_ack_sequence,
            )?)
    }
}
