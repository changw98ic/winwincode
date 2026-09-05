// SPDX-License-Identifier: Apache-2.0

//! Local repository registration, listing, removal, and launch-time
//! revalidation (plan sections 8.1, 9.3, 13.1–13.3, and 13.5).
//!
//! The registration check chain follows plan §13.2 exactly:
//!
//! ```text
//! canonicalize (symlinks resolved, replacement detected)
//!   ↓
//! directory exists and is readable
//!   ↓
//! confirm-or-initialize Git (an explicit confirmation is required for init)
//!   ↓
//! Git common directory
//!   ↓
//! HEAD / branch / dirty state
//!   ↓
//! random rbd_ RepositoryBindingId
//!   ↓
//! local binding → absolute path (path mapping, never uploaded)
//!   ↓
//! safe metadata report (`client.repository.upsert`, no absolute paths)
//! ```
//!
//! [`revalidate_repository`] re-runs the same chain against the stored
//! canonical path and returns the current plan 13.5 state; the Worker epic
//! must call it before every worker launch instead of trusting the last scan
//! (`每次 Worker Launch 前必须重新 canonicalize`). When the fresh scan
//! disagrees with the durable scan projection, a `client.repository.status`
//! report is enqueued automatically.
//!
//! Local-data boundary: the canonical path and the Git common directory live
//! only in the local `repository_path_mapping` / `repository_local_state`
//! tables and are returned only to local callers. Every server-bound frame
//! (`client.repository.upsert`, `client.repository.removed`,
//! `client.repository.status`) carries the binding identity, the derived
//! fingerprint, and scan facts — never an absolute path.
//!
//! Git interconnect: the scan delegates every Git probe to the independent
//! inspector in [`crate::repository_git`], which shells out to the system
//! `git` binary through `std::process::Command` — the same dependency-free
//! convention `winwincode-repository-context` uses for its baseline
//! snapshots; no new crate dependency is introduced. The local Git
//! installation must exist, a remote origin may be empty, and GitHub is
//! never required — the Server only ever sees the commit, branch, dirty
//! projection, and binding identity (plan §13.3).

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
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

// The fingerprint rule is owned and documented by the Git inspector; the
// registry re-exports it so every historical import path keeps working.
pub use crate::repository_git::repository_fingerprint;

/// Canonical repository binding id prefix, matching the schema's
/// `RepositoryBindingId` pattern (`rbd_` + 26 Crockford characters).
const REPOSITORY_BINDING_ID_PREFIX: &str = "rbd_";
/// Upper bound on binding-id regeneration when the random draw collides with
/// a stored binding (practically unreachable with 26 Crockford characters).
const MAX_BINDING_ID_ATTEMPTS: usize = 4;
/// Display-name bound shared with the other server-visible identifiers.
const MAX_DISPLAY_NAME_BYTES: usize = 200;
const MAX_ID_BYTES: usize = 200;

/// Explicit confirmations a registration accepts. `git init` never runs
/// without [`RegistrationOptions::confirm_git_init`] (plan §13.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrationOptions {
    /// Whether a non-Git directory may be initialized (`wwc repo add --init`).
    pub confirm_git_init: bool,
}

/// Why the registration check chain refused a directory. `availability` is
/// the plan 13.5 state the refusal maps to; a refused registration persists
/// nothing locally and reports nothing to the Server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationRejection {
    /// The seven-state projection of the failure (`unavailable`, `moved`,
    /// `invalid_git`, `permission_denied`, or `scan_failed`).
    pub availability: RepositoryAvailability,
    /// Local-only human-readable reason (absolute paths allowed; this never
    /// reaches a frame).
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

/// Failure of a repository registry operation.
#[derive(Debug)]
pub enum RepositoryRegistryError {
    /// The durable store failed.
    Store(DeviceStoreError),
    /// The operation reports a frame, so it requires an adopted enrollment:
    /// a pending repository frame on the placeholder stream could never be
    /// re-keyed onto the server-assigned node.
    NotEnrolled,
    /// The binding id is unknown locally.
    NotFound,
    /// The directory is already registered under another binding id.
    AlreadyRegistered {
        /// The binding id the canonical path is registered under.
        repository_binding_id: String,
    },
    /// The registration check chain refused the directory.
    Rejected(RegistrationRejection),
    /// A caller-supplied value is invalid.
    InvalidInput(String),
    /// A frame could not be encoded or the outbox rejected the append.
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

/// The facts one scan observed about a directory. Absolute paths stay
/// local-only.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryScan {
    canonical_path: PathBuf,
    git_common_directory: Option<PathBuf>,
    branch: String,
    head_commit: String,
    dirty_state: RepositoryDirtyState,
    /// Whether this scan ran `git init` (only a confirmed registration may).
    git_initialized_by_scan: bool,
}

/// Result of one successful registration (plan §13.1 `repo add`).
#[derive(Clone, Debug, PartialEq)]
pub struct RepositoryRegistration {
    /// The freshly generated binding identity (`rbd_`).
    pub repository_binding_id: String,
    /// Local-only canonical absolute path of the binding.
    pub canonical_path: PathBuf,
    /// Local-only Git common directory, when Git reported one.
    pub git_common_directory: Option<PathBuf>,
    /// Whether this registration initialized Git after explicit confirmation.
    pub git_initialized_by_registration: bool,
    /// The safe projection the enqueued `client.repository.upsert` carries.
    pub projection: RepositoryBindingProjection,
    /// Durable outbox sequence of the enqueued upsert frame.
    pub upsert_outbox_sequence: u64,
}

/// Result of one removal (plan §13.1 `repo remove`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRemoval {
    /// The binding id that was removed.
    pub repository_binding_id: String,
    /// Durable outbox sequence of the enqueued `client.repository.removed`.
    pub removed_outbox_sequence: u64,
}

/// Result of one launch-time revalidation (plan §13.5): the current
/// seven-state projection of a binding, whatever the check chain found.
#[derive(Clone, Debug, PartialEq)]
pub struct RepositoryRevalidation {
    /// The binding that was revalidated.
    pub repository_binding_id: String,
    /// Current availability state (`available`/`dirty` on a healthy scan;
    /// the failure state otherwise).
    pub availability: RepositoryAvailability,
    /// Current HEAD commit; empty when the scan could not read one.
    pub head_commit: String,
    /// Current working-tree dirty projection (`clean` when unreadable).
    pub dirty_state: RepositoryDirtyState,
    /// Fingerprint over HEAD and branch (`sha256:…`).
    pub repository_fingerprint: String,
    /// RFC 3339 stamp of this scan.
    pub last_scanned_at: String,
    /// Whether this revalidation enqueued a `client.repository.status`
    /// report because the projection differs from the durable one.
    pub status_reported: bool,
    /// Durable outbox sequence of the enqueued status frame, when one was
    /// enqueued.
    pub status_outbox_sequence: Option<u64>,
    /// Local-only detail of a failed scan; empty on a healthy scan.
    pub detail: String,
}

/// One binding row joined with its latest durable scan projection
/// (plan §13.1 `repo list`). Absolute paths stay local-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingSummary {
    /// The path mapping row (canonical path, Git common directory, last
    /// canonicalization stamp, availability vocabulary).
    pub mapping: PathMappingRecord,
    /// The latest durable scan projection, when one exists.
    pub scan: Option<RepositoryLocalStateRecord>,
}

/// Registers a directory as a repository binding (plan §13.1 `repo add`).
///
/// Runs the plan §13.2 check chain, persists the local path mapping plus the
/// scan projection, and enqueues the durable `client.repository.upsert`
/// frame (persist-before-send). The durable outbox stream must already be
/// bound (`DeviceStore::bind_outbox_stream`), and the node identity must be
/// the server-assigned `clientNodeId` — a registration before the enrollment
/// adoption is refused so its frame can never be stranded.
///
/// # Errors
///
/// Returns [`RepositoryRegistryError::Rejected`] when the check chain refuses
/// the directory (the seven-state reason rides the rejection),
/// [`RepositoryRegistryError::AlreadyRegistered`] when the canonical path is
/// bound already, [`RepositoryRegistryError::NotEnrolled`] before the
/// enrollment adoption, and the remaining variants for durable or encoding
/// failures.
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

    // Plan §13.2: the binding id is drawn only after every check passed.
    let binding_id = fresh_binding_id(store)?;
    let stamp = rfc3339(now)?;
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
    // Plan §9.3: `client.repository.upsert` is a C-class command carrying
    // expectedRevision + idempotencyKey and no fencing stamp; v1 has no
    // server-initiated repository change, so the expected revision is the
    // fresh binding's 0. The scan stamp keeps the key deterministic per scan
    // and unique across scans of the same binding.
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

/// Removes a repository binding (plan §13.1 `repo remove`): the durable
/// `client.repository.removed` frame is enqueued first (persist-before-send)
/// and the path mapping plus scan projection are then deleted atomically, so
/// the Server invalidates its projection and the local absolute path leaves
/// the store in one step.
///
/// # Errors
///
/// Returns [`RepositoryRegistryError::NotFound`] for an unknown binding id,
/// [`RepositoryRegistryError::NotEnrolled`] before the enrollment adoption,
/// and the remaining variants for durable or encoding failures.
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
            // One removal per binding lifetime: a re-added directory always
            // draws a fresh binding id, so the key never collides.
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

/// Re-runs the registration check chain against a binding's stored canonical
/// path and returns the current plan 13.5 state (the Worker epic calls this
/// before every worker launch; the result is never taken from the last
/// scan). Revalidation never initializes Git and never moves the binding: a
/// missing or replaced directory maps to `moved`, and the stored canonical
/// path stays authoritative so a later restore revalidates back.
///
/// When the fresh projection differs from the durable scan projection
/// (availability, HEAD, or dirty state), a `client.repository.status` report
/// is enqueued automatically and [`RepositoryRevalidation::status_reported`]
/// is set.
///
/// # Errors
///
/// Returns [`RepositoryRegistryError::NotFound`] for an unknown binding id,
/// [`RepositoryRegistryError::NotEnrolled`] before the enrollment adoption,
/// and the remaining variants for durable or encoding failures. Scan
/// failures are values (`moved`, `invalid_git`, …), not errors.
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

    let stamp = rfc3339(now)?;
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
            // A failed revalidation keeps the binding where it was: only the
            // projection moves to the failure state, with nothing claimed
            // about a tree that could not be read.
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

    // Plan §13.5: report when the fresh scan disagrees with the durable
    // projection. A missing previous projection (never written by this lane)
    // counts as a difference so the Server learns the current state.
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

/// Lists every local binding with its latest durable scan projection
/// (plan §13.1 `repo list`), in binding-id order. Reads only — no scan runs.
///
/// # Errors
///
/// Returns a store failure when a read fails or the store is closed.
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

/// The availability a healthy scan reports: `dirty` when the working tree
/// has local modifications, `available` otherwise (plan §13.5 treats the two
/// as distinct states).
const fn scan_availability(dirty_state: RepositoryDirtyState) -> RepositoryAvailability {
    match dirty_state {
        RepositoryDirtyState::Clean => RepositoryAvailability::Available,
        RepositoryDirtyState::Dirty => RepositoryAvailability::Dirty,
    }
}

/// The registration check chain (plan §13.2), shared by registration and
/// revalidation. `allow_git_init` permits `git init` after an explicit
/// confirmation; `missing_maps_to_moved` switches the missing-directory and
/// replaced-directory failures from `unavailable` (nothing was ever bound) to
/// `moved` (a bound path disappeared or changed shape).
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

/// Chain step 1: canonicalize with symlink resolution and replacement
/// detection. At revalidation a directory entry that became a symlink is
/// exactly the "软链接替换" shape: the bound path no longer is the directory.
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

/// Chain step 2: the canonical path must be an existing, readable directory.
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

/// Chain steps 3–5 on the already-canonical directory: confirm-or-initialize
/// Git, the Git common directory, then HEAD / branch / dirty state. The
/// probes live in [`crate::repository_git`]; this wrapper maps the
/// inspector's stable refusal classification onto the registry's rejection
/// vocabulary unchanged.
#[allow(clippy::type_complexity)] // the five scan facts in plan §13.2 order
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

/// The failure state for a missing or reshaped path: `moved` when a binding
/// already pointed there, `unavailable` when nothing was ever bound.
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

/// Draws a fresh binding id, re-drawing on the (practically unreachable)
/// collision with a stored binding.
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

/// Finds the binding a canonical path is already registered under, if any.
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

/// The server-visible repository display name: the canonical directory name.
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

/// Repository frames report facts, so the sender stream must carry the
/// server-assigned node; an unbound or placeholder stream is refused.
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

/// Appends one durable repository frame to the outbox
/// (persist-before-send), using the same message-id convention as the
/// daemon's enqueue path.
fn enqueue_repository_frame(
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
        occurred_at: rfc3339(now)?,
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

/// Maps an outbox state-machine failure onto the registry error set.
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

/// RFC 3339 UTC stamp of the caller's clock observation.
fn rfc3339(time: OffsetDateTime) -> Result<String, RepositoryRegistryError> {
    time.format(&Rfc3339)
        .map_err(|error| RepositoryRegistryError::Protocol(format!("timestamp failed: {error}")))
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
