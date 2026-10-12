// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use winwincode_client_port::domain::{
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState, RepositoryKind,
};
use winwincode_client_port::exchange::{FrameCodec, OutboxSession};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientRepositoryRemovedPayload,
    ClientRepositoryStatusPayload, ClientRepositoryUpsertPayload, ClientToServerEnvelope,
    ClientToServerMessage, CommandContext,
};

use crate::identity::generate_prefixed_id;
use crate::repository_git::{GitInspectOptions, GitInspector};
use crate::store::{
    DeviceStore, DeviceStoreError, PathMappingRecord, RepositoryLocalStateRecord,
    availability_wire_name,
};

pub use crate::repository_git::repository_fingerprint;

const REPOSITORY_BINDING_ID_PREFIX: &str = "rbd_";
const MAX_BINDING_ID_ATTEMPTS: usize = 4;
const MAX_DISPLAY_NAME_BYTES: usize = 200;
const MAX_ID_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrationOptions {
    pub confirm_git_init: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationRejection {
    pub availability: RepositoryAvailability,
    pub detail: String,
}

impl RegistrationRejection {
    fn new(availability: RepositoryAvailability, detail: impl Into<String>) -> Self {
        Self {
            availability,
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
pub enum RepositoryRegistryError {
    Store(DeviceStoreError),
    NotEnrolled,
    NotFound,
    AlreadyRegistered { repository_binding_id: String },
    Rejected(RegistrationRejection),
    InvalidInput(String),
    Protocol(String),
}

impl fmt::Display for RepositoryRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "repository registry store failure: {error}"),
            Self::NotEnrolled => {
                formatter.write_str("repository registry frames require an adopted enrollment")
            }
            Self::NotFound => formatter.write_str("the repository binding is unknown locally"),
            Self::AlreadyRegistered {
                repository_binding_id,
            } => write!(
                formatter,
                "the directory is already registered as binding {repository_binding_id}"
            ),
            Self::Rejected(rejection) => write!(
                formatter,
                "registration rejected ({}): {}",
                availability_wire_name(rejection.availability),
                rejection.detail
            ),
            Self::InvalidInput(message) => {
                write!(formatter, "repository registry input is invalid: {message}")
            }
            Self::Protocol(message) => {
                write!(formatter, "repository registry protocol failure: {message}")
            }
        }
    }
}

impl std::error::Error for RepositoryRegistryError {}

impl From<DeviceStoreError> for RepositoryRegistryError {
    fn from(error: DeviceStoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryScan {
    canonical_path: PathBuf,
    git_common_directory: Option<PathBuf>,
    branch: String,
    head_commit: String,
    dirty_state: RepositoryDirtyState,
    git_initialized_by_scan: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepositoryRegistration {
    pub repository_binding_id: String,
    pub canonical_path: PathBuf,
    pub git_common_directory: Option<PathBuf>,
    pub git_initialized_by_registration: bool,
    pub projection: RepositoryBindingProjection,
    pub upsert_outbox_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRemoval {
    pub repository_binding_id: String,
    pub removed_outbox_sequence: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepositoryRevalidation {
    pub repository_binding_id: String,
    pub availability: RepositoryAvailability,
    pub head_commit: String,
    pub dirty_state: RepositoryDirtyState,
    pub repository_fingerprint: String,
    pub last_scanned_at: String,
    pub status_reported: bool,
    pub status_outbox_sequence: Option<u64>,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingSummary {
    pub mapping: PathMappingRecord,
    pub scan: Option<RepositoryLocalStateRecord>,
}

pub fn register_repository(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    requested_path: &Path,
    options: &RegistrationOptions,
    now: OffsetDateTime,
) -> Result<RepositoryRegistration, RepositoryRegistryError> {
    require_stream_identity(client_node_id, client_instance_id)?;
    let scan = scan_repository(requested_path, options.confirm_git_init, false)
        .map_err(RepositoryRegistryError::Rejected)?;
    if let Some(binding_id) = binding_for_canonical_path(store, &scan.canonical_path)? {
        return Err(RepositoryRegistryError::AlreadyRegistered {
            repository_binding_id: binding_id,
        });
    }

    let binding_id = fresh_binding_id(store)?;
    let stamp = rfc3339(now);
    let availability = scan_availability(scan.dirty_state);
    let fingerprint = repository_fingerprint(&scan.head_commit, &scan.branch);
    let mapping = PathMappingRecord {
        repository_binding_id: binding_id.clone(),
        canonical_path: scan.canonical_path.to_string_lossy().into_owned(),
        git_common_directory: scan
            .git_common_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        last_canonicalized_at: Some(stamp.clone()),
        local_state: availability_wire_name(availability).to_owned(),
    };
    store.put_path_mapping(&mapping)?;
    store.put_repository_local_state(&RepositoryLocalStateRecord {
        repository_binding_id: binding_id.clone(),
        dirty_state: scan.dirty_state,
        availability,
        head_commit: Some(scan.head_commit.clone()),
        last_scanned_at: Some(stamp.clone()),
        updated_at: stamp.clone(),
    })?;
    let projection = RepositoryBindingProjection {
        repository_binding_id: binding_id.clone(),
        display_name: display_name(&scan.canonical_path)?,
        repository_kind: RepositoryKind::Git,
        default_branch: scan.branch.clone(),
        head_commit: scan.head_commit.clone(),
        dirty_state: scan.dirty_state,
        availability,
        repository_fingerprint: fingerprint,
        last_scanned_at: stamp.clone(),
    };
    let message = ClientToServerMessage::RepositoryUpsert(ClientRepositoryUpsertPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: format!("repository-upsert-{binding_id}-{stamp}"),
        },
        repository: projection.clone(),
    });
    let upsert_outbox_sequence = enqueue_repository_frame(
        store,
        client_node_id,
        client_instance_id,
        message,
        "client.repository.upsert",
        now,
    )?;
    Ok(RepositoryRegistration {
        repository_binding_id: binding_id,
        canonical_path: scan.canonical_path,
        git_common_directory: scan.git_common_directory,
        git_initialized_by_registration: scan.git_initialized_by_scan,
        projection,
        upsert_outbox_sequence,
    })
}

pub fn remove_repository(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    repository_binding_id: &str,
    now: OffsetDateTime,
) -> Result<RepositoryRemoval, RepositoryRegistryError> {
    require_stream_identity(client_node_id, client_instance_id)?;
    if repository_binding_id.is_empty() || repository_binding_id.len() > MAX_ID_BYTES {
        return Err(RepositoryRegistryError::InvalidInput(
            "repository binding id must be non-empty and bounded".to_owned(),
        ));
    }
    if store.path_mapping(repository_binding_id)?.is_none() {
        return Err(RepositoryRegistryError::NotFound);
    }
    let message = ClientToServerMessage::RepositoryRemoved(ClientRepositoryRemovedPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: format!("repository-removed-{repository_binding_id}"),
        },
        repository_binding_id: repository_binding_id.to_owned(),
    });
    let removed_outbox_sequence = enqueue_repository_frame(
        store,
        client_node_id,
        client_instance_id,
        message,
        "client.repository.removed",
        now,
    )?;
    if !store.delete_repository_binding(repository_binding_id)? {
        return Err(RepositoryRegistryError::NotFound);
    }
    Ok(RepositoryRemoval {
        repository_binding_id: repository_binding_id.to_owned(),
        removed_outbox_sequence,
    })
}

pub fn revalidate_repository(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    repository_binding_id: &str,
    now: OffsetDateTime,
) -> Result<RepositoryRevalidation, RepositoryRegistryError> {
    require_stream_identity(client_node_id, client_instance_id)?;
    if repository_binding_id.is_empty() || repository_binding_id.len() > MAX_ID_BYTES {
        return Err(RepositoryRegistryError::InvalidInput(
            "repository binding id must be non-empty and bounded".to_owned(),
        ));
    }
    let mapping = store
        .path_mapping(repository_binding_id)?
        .ok_or(RepositoryRegistryError::NotFound)?;
    let stored_path = PathBuf::from(&mapping.canonical_path);
    let scan = scan_repository(&stored_path, false, true);

    let stamp = rfc3339(now);
    let (availability, head_commit, dirty_state, fingerprint, detail, git_common_directory) =
        match scan {
            Ok(scan) => {
                let availability = scan_availability(scan.dirty_state);
                let fingerprint = repository_fingerprint(&scan.head_commit, &scan.branch);
                (
                    availability,
                    scan.head_commit,
                    scan.dirty_state,
                    fingerprint,
                    String::new(),
                    scan.git_common_directory,
                )
            }
            Err(rejection) => (
                rejection.availability,
                String::new(),
                RepositoryDirtyState::Clean,
                repository_fingerprint("", ""),
                rejection.detail,
                None,
            ),
        };

    let previous = store.repository_local_state(repository_binding_id)?;
    store.put_repository_local_state(&RepositoryLocalStateRecord {
        repository_binding_id: repository_binding_id.to_owned(),
        dirty_state,
        availability,
        head_commit: if head_commit.is_empty() {
            None
        } else {
            Some(head_commit.clone())
        },
        last_scanned_at: Some(stamp.clone()),
        updated_at: stamp.clone(),
    })?;
    let mut mapping = mapping;
    mapping.last_canonicalized_at = Some(stamp.clone());
    availability_wire_name(availability).clone_into(&mut mapping.local_state);
    if let Some(git_common_directory) = git_common_directory {
        mapping.git_common_directory = Some(git_common_directory.to_string_lossy().into_owned());
    }
    store.put_path_mapping(&mapping)?;

    let differs = previous.as_ref().is_none_or(|previous| {
        previous.availability != availability
            || previous.dirty_state != dirty_state
            || previous.head_commit.clone().unwrap_or_default() != head_commit
    });
    let (status_reported, status_outbox_sequence) = if differs {
        let message = ClientToServerMessage::RepositoryStatus(ClientRepositoryStatusPayload {
            repository_binding_id: repository_binding_id.to_owned(),
            availability,
            head_commit: head_commit.clone(),
            dirty_state,
            last_scanned_at: stamp.clone(),
        });
        let sequence = enqueue_repository_frame(
            store,
            client_node_id,
            client_instance_id,
            message,
            "client.repository.status",
            now,
        )?;
        (true, Some(sequence))
    } else {
        (false, None)
    };
    Ok(RepositoryRevalidation {
        repository_binding_id: repository_binding_id.to_owned(),
        availability,
        head_commit,
        dirty_state,
        repository_fingerprint: fingerprint,
        last_scanned_at: stamp,
        status_reported,
        status_outbox_sequence,
        detail,
    })
}

pub fn list_bindings(
    store: &DeviceStore,
) -> Result<Vec<RepositoryBindingSummary>, RepositoryRegistryError> {
    let mappings = store.path_mappings()?;
    let mut summaries = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let scan = store.repository_local_state(&mapping.repository_binding_id)?;
        summaries.push(RepositoryBindingSummary { mapping, scan });
    }
    Ok(summaries)
}

const fn scan_availability(dirty_state: RepositoryDirtyState) -> RepositoryAvailability {
    match dirty_state {
        RepositoryDirtyState::Clean => RepositoryAvailability::Available,
        RepositoryDirtyState::Dirty => RepositoryAvailability::Dirty,
    }
}

fn scan_repository(
    requested_path: &Path,
    allow_git_init: bool,
    missing_maps_to_moved: bool,
) -> Result<RepositoryScan, RegistrationRejection> {
    let canonical_path = canonicalize_entry(requested_path, missing_maps_to_moved)?;
    require_readable_directory(&canonical_path, missing_maps_to_moved)?;
    let (git_common_directory, branch, head_commit, dirty_state, git_initialized_by_scan) =
        scan_git_state(&canonical_path, allow_git_init)?;
    Ok(RepositoryScan {
        canonical_path,
        git_common_directory,
        branch,
        head_commit,
        dirty_state,
        git_initialized_by_scan,
    })
}

fn canonicalize_entry(
    requested_path: &Path,
    missing_maps_to_moved: bool,
) -> Result<PathBuf, RegistrationRejection> {
    let entry = match fs::symlink_metadata(requested_path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(missing_rejection(
                missing_maps_to_moved,
                format!("the path is gone: {}", requested_path.to_string_lossy()),
            ));
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return Err(RegistrationRejection::new(
                RepositoryAvailability::PermissionDenied,
                format!(
                    "the path cannot be inspected: {}",
                    requested_path.to_string_lossy()
                ),
            ));
        }
        Err(error) => {
            return Err(RegistrationRejection::new(
                RepositoryAvailability::ScanFailed,
                format!(
                    "the path cannot be inspected: {} ({error})",
                    requested_path.to_string_lossy()
                ),
            ));
        }
    };
    if missing_maps_to_moved && entry.file_type().is_symlink() {
        return Err(RegistrationRejection::new(
            RepositoryAvailability::Moved,
            format!(
                "the bound directory was replaced by a symbolic link: {}",
                requested_path.to_string_lossy()
            ),
        ));
    }
    match fs::canonicalize(requested_path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(missing_rejection(
            missing_maps_to_moved,
            format!("the path is gone: {}", requested_path.to_string_lossy()),
        )),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            Err(RegistrationRejection::new(
                RepositoryAvailability::PermissionDenied,
                format!(
                    "the path cannot be resolved: {}",
                    requested_path.to_string_lossy()
                ),
            ))
        }
        Err(error) => Err(RegistrationRejection::new(
            RepositoryAvailability::ScanFailed,
            format!(
                "the path cannot be resolved: {} ({error})",
                requested_path.to_string_lossy()
            ),
        )),
    }
}

fn require_readable_directory(
    canonical_path: &Path,
    missing_maps_to_moved: bool,
) -> Result<(), RegistrationRejection> {
    let metadata = match fs::metadata(canonical_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return Err(RegistrationRejection::new(
                RepositoryAvailability::PermissionDenied,
                format!(
                    "the directory cannot be inspected: {}",
                    canonical_path.to_string_lossy()
                ),
            ));
        }
        Err(error) => {
            return Err(missing_rejection(
                missing_maps_to_moved,
                format!(
                    "the directory cannot be inspected: {} ({error})",
                    canonical_path.to_string_lossy()
                ),
            ));
        }
    };
    if !metadata.is_dir() {
        return Err(missing_rejection(
            missing_maps_to_moved,
            format!(
                "the path is not a directory: {}",
                canonical_path.to_string_lossy()
            ),
        ));
    }
    if let Err(error) = fs::read_dir(canonical_path) {
        let availability = if error.kind() == ErrorKind::PermissionDenied {
            RepositoryAvailability::PermissionDenied
        } else {
            RepositoryAvailability::ScanFailed
        };
        return Err(RegistrationRejection::new(
            availability,
            format!(
                "the directory is not readable: {} ({error})",
                canonical_path.to_string_lossy()
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn scan_git_state(
    canonical_path: &Path,
    allow_git_init: bool,
) -> Result<(Option<PathBuf>, String, String, RepositoryDirtyState, bool), RegistrationRejection> {
    let scan = GitInspector::new()
        .inspect(canonical_path, &GitInspectOptions { allow_git_init })
        .map_err(|error| RegistrationRejection::new(error.availability(), error.detail()))?;
    Ok((
        Some(scan.git_common_directory),
        scan.branch,
        scan.head_commit,
        scan.dirty_state,
        scan.initialized_by_inspection,
    ))
}

fn missing_rejection(missing_maps_to_moved: bool, detail: String) -> RegistrationRejection {
    RegistrationRejection::new(
        if missing_maps_to_moved {
            RepositoryAvailability::Moved
        } else {
            RepositoryAvailability::Unavailable
        },
        detail,
    )
}

fn fresh_binding_id(store: &DeviceStore) -> Result<String, RepositoryRegistryError> {
    for _ in 0..MAX_BINDING_ID_ATTEMPTS {
        let candidate = generate_prefixed_id(REPOSITORY_BINDING_ID_PREFIX)?;
        if store.path_mapping(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Err(RepositoryRegistryError::Protocol(
        "binding id entropy never produced a fresh id".to_owned(),
    ))
}

fn binding_for_canonical_path(
    store: &DeviceStore,
    canonical_path: &Path,
) -> Result<Option<String>, RepositoryRegistryError> {
    let canonical = canonical_path.to_string_lossy();
    Ok(store
        .path_mappings()?
        .into_iter()
        .find(|mapping| mapping.canonical_path == canonical)
        .map(|mapping| mapping.repository_binding_id))
}

fn display_name(canonical_path: &Path) -> Result<String, RepositoryRegistryError> {
    let name = canonical_path.file_name().map_or_else(
        || "repository".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    if name.is_empty() || name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(RepositoryRegistryError::InvalidInput(format!(
            "the directory name must be 1 to {MAX_DISPLAY_NAME_BYTES} bytes, got {}",
            name.len()
        )));
    }
    Ok(name)
}

fn require_stream_identity(
    client_node_id: &str,
    client_instance_id: &str,
) -> Result<(), RepositoryRegistryError> {
    if client_node_id.is_empty() {
        return Err(RepositoryRegistryError::NotEnrolled);
    }
    if client_instance_id.is_empty() || client_instance_id.len() > MAX_ID_BYTES {
        return Err(RepositoryRegistryError::InvalidInput(
            "client instance id must be non-empty and bounded".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn enqueue_repository_frame(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    message: ClientToServerMessage,
    kind: &'static str,
    now: OffsetDateTime,
) -> Result<u64, RepositoryRegistryError> {
    let session = OutboxSession::new();
    let expected = session.next_sequence(store).map_err(map_outbox_error)?;
    let envelope = ClientToServerEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: format!("{client_node_id}-{kind}-{expected}"),
        client_node_id: client_node_id.to_owned(),
        client_instance_id: client_instance_id.to_owned(),
        sequence: expected,
        occurred_at: rfc3339(now),
        message,
    };
    let stored = FrameCodec::default()
        .encode_envelope(&envelope)
        .map_err(|error| {
            RepositoryRegistryError::Protocol(format!("{kind} frame encoding failed: {error:?}"))
        })?;
    session
        .enqueue(store, expected, &stored)
        .map_err(map_outbox_error)?;
    Ok(expected)
}

fn map_outbox_error(
    error: winwincode_client_port::exchange::OutboxError<DeviceStoreError>,
) -> RepositoryRegistryError {
    use winwincode_client_port::exchange::OutboxError;
    match error {
        OutboxError::Store(store) => RepositoryRegistryError::Store(store),
        OutboxError::CorruptState(state) => RepositoryRegistryError::Store(
            DeviceStoreError::adapter(format!("the durable outbox is corrupt: {state:?}")),
        ),
        other => RepositoryRegistryError::Protocol(format!(
            "the outbox state machine rejected the repository frame: {other:?}"
        )),
    }
}

fn rfc3339(time: OffsetDateTime) -> String {
    crate::canonical_rfc3339(time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_binds_head_and_branch_without_paths() {
        let first = repository_fingerprint("abc", "main");
        let again = repository_fingerprint("abc", "main");
        let other_commit = repository_fingerprint("abd", "main");
        let other_branch = repository_fingerprint("abc", "feature");
        let unborn = repository_fingerprint("", "main");
        assert_eq!(first, again);
        assert_ne!(first, other_commit);
        assert_ne!(first, other_branch);
        assert_ne!(first, unborn);
        assert!(first.starts_with("sha256:"));
    }

    #[test]
    fn availability_vocabulary_matches_plan_13_5() {
        use crate::store::availability_wire_name;
        assert_eq!(
            availability_wire_name(RepositoryAvailability::Available),
            "available"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::Dirty),
            "dirty"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::Unavailable),
            "unavailable"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::Moved),
            "moved"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::InvalidGit),
            "invalid_git"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::PermissionDenied),
            "permission_denied"
        );
        assert_eq!(
            availability_wire_name(RepositoryAvailability::ScanFailed),
            "scan_failed"
        );
    }
}
