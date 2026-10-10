use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    ClientExchangeCursors, ClientLockState, ClientNodeRecord, ClientNodeRegistration,
    ClientNodeRegistrationReceipt, ClientPresenceState, ClientRegistryError,
    ClientRegistryErrorKind, SqliteStorage,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRegistryServiceErrorKind {
    InvalidInput,
    UnknownClientNode,
    IdentityConflict,
    RevisionConflict,
    PresenceTransition,
    CorruptState,
    Storage,
}

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

pub struct ClientRegistryService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> ClientRegistryService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

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

    pub fn snapshot(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<ClientNodeRecord>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .snapshot(client_node_id)?)
    }

    pub fn snapshot_by_public_client_id(
        &mut self,
        public_client_id: &str,
    ) -> Result<Option<ClientNodeRecord>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .snapshot_by_public_client_id(public_client_id)?)
    }

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

    pub fn update_connection_policy(
        &mut self,
        client_node_id: &str,
        accepting_connections: bool,
        lock_state: ClientLockState,
        expected_revision: u64,
    ) -> Result<ClientNodeRecord, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .update_connection_policy(
                client_node_id,
                accepting_connections,
                lock_state,
                expected_revision,
            )?)
    }

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

    pub fn sweep_offline(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, ClientRegistryServiceError> {
        Ok(self.storage.client_node_registry()?.sweep_offline(cutoff)?)
    }

    pub fn exchange_cursors(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<ClientExchangeCursors>, ClientRegistryServiceError> {
        Ok(self
            .storage
            .client_node_registry()?
            .exchange_cursors(client_node_id)?)
    }

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
