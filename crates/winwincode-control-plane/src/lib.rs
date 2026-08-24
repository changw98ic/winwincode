// SPDX-License-Identifier: Apache-2.0

//! Application lifecycle host for the `WinWinCode` Control Plane.
//!
//! This crate owns application composition and durable event publication. It
//! intentionally has no dependency on Codex Core, an HTTP server, or Delivery
//! domain logic.

pub mod delivery_execution;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_storage::SqliteStorage;
pub use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, OutboxEvent, ProductStateStorage, StateCommit, StorageError,
    StorageErrorKind, StoredState,
};

/// Local Control Plane process configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneConfig {
    data_directory: PathBuf,
    temporary_parent: PathBuf,
}

impl ControlPlaneConfig {
    #[must_use]
    pub fn local(data_directory: impl AsRef<Path>) -> Self {
        let data_directory = data_directory.as_ref().to_path_buf();
        Self {
            temporary_parent: data_directory.join(".control-plane-runtime"),
            data_directory,
        }
    }

    /// Overrides the parent under which the instance-owned temporary root is created.
    #[must_use]
    pub fn with_temporary_parent(mut self, temporary_parent: impl AsRef<Path>) -> Self {
        self.temporary_parent = temporary_parent.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    #[must_use]
    pub fn temporary_parent(&self) -> &Path {
        &self.temporary_parent
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
    temporary_root: Option<OwnedTemporaryRoot>,
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
        let ControlPlaneConfig {
            data_directory,
            temporary_parent,
        } = config;
        let temporary_root = match OwnedTemporaryRoot::create(&temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let cleanup = close_publisher(&mut publisher);
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{cleanup}"
                )));
            }
        };
        let storage = match SqliteStorage::open(data_directory) {
            Ok(storage) => storage,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                let cleanup = cleanup_suffix(&cleanup_failures);
                return Err(StartError::new(format!(
                    "failed to open Control Plane storage: {error}{cleanup}"
                )));
            }
        };
        Self::start_with_resources(Box::new(storage), publisher, temporary_root)
    }

    /// Composes the application with a storage adapter at the `PostgreSQL`-ready seam.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing both adapters when durable outbox
    /// replay fails.
    pub fn start(
        storage: Box<dyn ProductStateStorage>,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let temporary_parent = std::env::temp_dir().join("winwincode-control-plane");
        let temporary_root = match OwnedTemporaryRoot::create(temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    failures.push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(close_error) = storage.close() {
                    failures.push(format!("storage close also failed: {close_error}"));
                }
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{}",
                    cleanup_suffix(&failures)
                )));
            }
        };
        Self::start_with_resources(storage, publisher, temporary_root)
    }

    fn start_with_resources(
        storage: Box<dyn ProductStateStorage>,
        publisher: Box<dyn EventPublisher>,
        temporary_root: OwnedTemporaryRoot,
    ) -> Result<Self, StartError> {
        let mut control_plane = Self {
            storage: Some(storage),
            publisher: Some(publisher),
            temporary_root: Some(temporary_root),
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

    /// Returns the instance-owned temporary root while the host is running.
    ///
    /// # Panics
    ///
    /// Panics only if the internal lifecycle invariant is broken and a running
    /// host has already lost ownership of its temporary root. Shutdown consumes
    /// the host, so callers cannot observe a normally released root through this
    /// method.
    #[must_use]
    pub fn temporary_root(&self) -> &Path {
        self.temporary_root
            .as_ref()
            .expect("a running Control Plane always owns a temporary root")
            .path()
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
        if let Some(temporary_root) = self.temporary_root.take()
            && let Err(error) = temporary_root.release()
        {
            failures.push(format!("temporary root release failed: {error}"));
        }
        failures
    }
}

const OWNERSHIP_MARKER: &str = ".winwincode-control-plane-owner";
static NEXT_TEMPORARY_ROOT_ID: AtomicU64 = AtomicU64::new(1);

struct OwnedTemporaryRoot {
    path: PathBuf,
    marker: String,
}

impl OwnedTemporaryRoot {
    fn create(parent: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let parent = parent.as_ref();
        fs::create_dir_all(parent)?;
        // Phase 2.1 deliberately does not enumerate or remove roots left by a
        // previous process. A PID or old-looking marker is not proof that a
        // lease is stale. Each instance only creates and releases its own exact
        // marker/path pair until a real renewable lease is introduced.
        loop {
            let instance_id = NEXT_TEMPORARY_ROOT_ID.fetch_add(1, Ordering::Relaxed);
            let marker = format!(
                "winwincode-control-plane\npid={}\ninstance={instance_id}\n",
                std::process::id()
            );
            let path = parent.join(format!("instance-{}-{instance_id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let marker_path = path.join(OWNERSHIP_MARKER);
                    let marker_result = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&marker_path)
                        .and_then(|mut file| {
                            file.write_all(marker.as_bytes())?;
                            file.sync_all()
                        });
                    if let Err(error) = marker_result {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(Self { path, marker });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn release(self) -> Result<(), std::io::Error> {
        let marker_path = self.path.join(OWNERSHIP_MARKER);
        let actual_marker = fs::read_to_string(&marker_path)?;
        if actual_marker != self.marker {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "temporary root ownership marker does not match this instance",
            ));
        }
        fs::remove_dir_all(self.path)
    }
}

fn close_publisher(publisher: &mut Box<dyn EventPublisher>) -> String {
    publisher.close().err().map_or_else(String::new, |error| {
        format!("; event publisher close also failed: {error}")
    })
}

fn cleanup_suffix(failures: &[String]) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        format!("; cleanup also failed: {}", failures.join("; "))
    }
}
