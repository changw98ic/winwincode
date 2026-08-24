// SPDX-License-Identifier: Apache-2.0

//! Application lifecycle host for the `WinWinCode` Control Plane.
//!
//! This crate owns application composition and durable event publication. It
//! intentionally has no dependency on Codex Core, an HTTP server, or Delivery
//! domain logic.

use std::fmt;
use std::path::{Path, PathBuf};

use winwincode_storage::SqliteStorage;
pub use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, OutboxEvent, ProductStateStorage, StateCommit, StorageError,
    StorageErrorKind, StoredState,
};

/// Local Control Plane process configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneConfig {
    data_directory: PathBuf,
}

impl ControlPlaneConfig {
    #[must_use]
    pub fn local(data_directory: impl AsRef<Path>) -> Self {
        Self {
            data_directory: data_directory.as_ref().to_path_buf(),
        }
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }
}

/// Error returned by an event publisher adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPublishError {
    message: String,
}

impl EventPublishError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EventPublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EventPublishError {}

/// Event transport owned and closed by the Control Plane lifecycle.
pub trait EventPublisher: Send {
    /// Publishes one durable event. Implementations must deduplicate by
    /// `event.event_id` because a crash after publish and before acknowledgement
    /// can cause the same outbox event to be offered again.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure without acknowledging the event.
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError>;

    /// Closes the event transport and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure if deterministic close fails.
    fn close(&mut self) -> Result<(), EventPublishError> {
        Ok(())
    }
}

/// Failure while draining the durable outbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboxError {
    Publish(EventPublishError),
    Acknowledge(StorageError),
}

impl fmt::Display for OutboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Publish(error) => write!(formatter, "event publication failed: {error}"),
            Self::Acknowledge(error) => {
                write!(
                    formatter,
                    "event publication acknowledgement failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for OutboxError {}

/// Control Plane startup failure. All resources are closed before this is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartError {
    message: String,
}

impl StartError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StartError {}

/// Control Plane commit failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    /// No state was committed.
    Storage(StorageError),
    /// State and outbox were committed, but the event remains pending.
    PublicationPending {
        receipt: CommitReceipt,
        source: OutboxError,
    },
}

impl CommitError {
    #[must_use]
    pub const fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::Storage(_) => None,
            Self::PublicationPending { receipt, .. } => Some(receipt),
        }
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state commit failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "state committed, but its outbox event remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for CommitError {}

/// Successful deterministic shutdown facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub published_event_count: usize,
}

/// Shutdown failure. The lifecycle still attempts every close step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownError {
    failures: Vec<String>,
}

impl ShutdownError {
    #[must_use]
    pub fn failures(&self) -> &[String] {
        &self.failures
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Control Plane shutdown had {} failure(s): {}",
            self.failures.len(),
            self.failures.join("; ")
        )
    }
}

impl std::error::Error for ShutdownError {}

/// Running Control Plane application host.
///
/// The host is deliberately synchronous in this phase: it does not detach
/// background tasks, so shutdown has a finite and observable ownership chain.
pub struct ControlPlane {
    storage: Option<Box<dyn ProductStateStorage>>,
    publisher: Option<Box<dyn EventPublisher>>,
}

impl ControlPlane {
    /// Opens and migrates the local `SQLite` database, replays durable outbox
    /// events, and only then returns a running Control Plane.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing owned resources when storage open,
    /// migration, or durable outbox replay fails.
    pub fn start_local(
        config: ControlPlaneConfig,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let ControlPlaneConfig { data_directory } = config;
        let storage = match SqliteStorage::open(data_directory) {
            Ok(storage) => storage,
            Err(error) => {
                let cleanup = publisher
                    .close()
                    .err()
                    .map_or_else(String::new, |close_error| {
                        format!("; event publisher close also failed: {close_error}")
                    });
                return Err(StartError::new(format!(
                    "failed to open Control Plane storage: {error}{cleanup}"
                )));
            }
        };
        Self::start(Box::new(storage), publisher)
    }

    /// Composes the application with a storage adapter at the `PostgreSQL`-ready seam.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing both adapters when durable outbox
    /// replay fails.
    pub fn start(
        storage: Box<dyn ProductStateStorage>,
        publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let mut control_plane = Self {
            storage: Some(storage),
            publisher: Some(publisher),
        };
        if let Err(error) = control_plane.flush_outbox() {
            let cleanup_failures = control_plane.close_resources();
            let cleanup = if cleanup_failures.is_empty() {
                String::new()
            } else {
                format!("; cleanup also failed: {}", cleanup_failures.join("; "))
            };
            return Err(StartError::new(format!(
                "failed to replay the durable outbox before startup: {error}{cleanup}"
            )));
        }
        Ok(control_plane)
    }

    /// Commits canonical state and its outbox first, then publishes pending events.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::Storage`] when nothing was committed, or
    /// [`CommitError::PublicationPending`] when state is durable and its event
    /// must be replayed.
    pub fn commit(&mut self, commit: StateCommit) -> Result<CommitReceipt, CommitError> {
        let receipt = self
            .storage_mut()
            .map_err(CommitError::Storage)?
            .commit(&commit)
            .map_err(CommitError::Storage)?;
        drop(commit);
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: receipt.clone(),
                source,
            })?;
        Ok(receipt)
    }

    /// Loads canonical state through the configured storage adapter.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the adapter read fails.
    pub fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.storage_ref()?.load_state(stream_id)
    }

    /// Stops accepting work by consuming the host, flushes the outbox, closes
    /// the publisher, and finally closes storage.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownError`] after attempting every close step when outbox
    /// flush or adapter close fails.
    pub fn shutdown(mut self) -> Result<ShutdownReport, ShutdownError> {
        let mut failures = Vec::new();
        let published_event_count = match self.flush_outbox() {
            Ok(count) => count,
            Err(error) => {
                failures.push(format!("outbox flush failed: {error}"));
                0
            }
        };
        failures.extend(self.close_resources());
        if failures.is_empty() {
            Ok(ShutdownReport {
                published_event_count,
            })
        } else {
            Err(ShutdownError { failures })
        }
    }

    fn flush_outbox(&mut self) -> Result<usize, OutboxError> {
        let events = self
            .storage_ref()
            .map_err(OutboxError::Acknowledge)?
            .pending_events()
            .map_err(OutboxError::Acknowledge)?;
        let mut published = 0;
        for event in events {
            self.publisher_mut()
                .map_err(|error| OutboxError::Publish(EventPublishError::new(error.to_string())))?
                .publish(&event)
                .map_err(OutboxError::Publish)?;
            self.storage_mut()
                .map_err(OutboxError::Acknowledge)?
                .mark_published(&event.event_id)
                .map_err(OutboxError::Acknowledge)?;
            published += 1;
        }
        Ok(published)
    }

    fn storage_ref(&self) -> Result<&dyn ProductStateStorage, StorageError> {
        self.storage
            .as_deref()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn storage_mut(&mut self) -> Result<&mut (dyn ProductStateStorage + 'static), StorageError> {
        self.storage
            .as_deref_mut()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn publisher_mut(&mut self) -> Result<&mut (dyn EventPublisher + 'static), StartError> {
        self.publisher
            .as_deref_mut()
            .ok_or_else(|| StartError::new("Control Plane event publisher is closed"))
    }

    fn close_resources(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(mut publisher) = self.publisher.take()
            && let Err(error) = publisher.close()
        {
            failures.push(format!("event publisher close failed: {error}"));
        }
        if let Some(storage) = self.storage.take()
            && let Err(error) = storage.close()
        {
            failures.push(format!("storage close failed: {error}"));
        }
        failures
    }
}
