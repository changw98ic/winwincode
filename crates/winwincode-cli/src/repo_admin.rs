// SPDX-License-Identifier: Apache-2.0

//! Owner-facing Device Client repository registry surface for the `wwc`
//! CLI (plan §13.1, §16.8: the CLI is always the no-desktop fallback).
//!
//! `wwc repo add|list|remove` drive the canonical
//! [`winwincode_device_client::repository`] library — the same registration
//! check chain the Device Client uses — and never re-implement Git probing,
//! binding-id generation, or frame construction. Boundaries carried over
//! from the device surface:
//!
//! - The canonical absolute path and the Git common directory live only in
//!   the local Device Client store. `repo list` may display them (the CLI is
//!   the local display surface); every server-bound frame the registration
//!   enqueues carries the binding identity and derived digests only.
//! - Frames require an adopted enrollment, so a repository projection can
//!   never be stranded on the placeholder stream the daemon could not
//!   re-key.
//! - One CLI run is one process launch: `repo add` / `repo remove` rotate
//!   the `clientInstanceId` like every Device Client launch does.

use std::fmt;
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use winwincode_device_client::repository::{self, RegistrationOptions, RepositoryRegistryError};
use winwincode_device_client::{
    DeviceStore, RepositoryRegistration, availability_wire_name, dirty_state_wire_name,
    ensure_device_identity, load_device_identity,
};

use crate::device_admin::{DeviceAdminError, now_rfc3339, open_store, rotation_seed};

/// Secret-free local view of one repository binding (`wwc repo list` row).
///
/// The canonical path and Git common directory are displayed on purpose:
/// this is the local display surface, and these fields never reach a server
/// frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryBindingView {
    /// Stable binding identity (`rbd_…`).
    pub repository_binding_id: String,
    /// Local-only canonical absolute path.
    pub canonical_path: String,
    /// Local-only Git common directory, when Git reported one.
    pub git_common_directory: Option<String>,
    /// Plan 13.5 availability of the last scan (`available`, `dirty`,
    /// `moved`, …).
    pub availability: String,
    /// Dirty projection of the last scan (`clean` / `dirty`), when scanned.
    pub dirty_state: Option<String>,
    /// Default branch, when this view was built from a fresh registration.
    pub default_branch: Option<String>,
    /// Last observed HEAD commit, when scanned.
    pub head_commit: Option<String>,
    /// RFC 3339 stamp of the last scan, when scanned.
    pub last_scanned_at: Option<String>,
    /// RFC 3339 stamp of the last successful canonicalization.
    pub last_canonicalized_at: Option<String>,
}

/// Human- and JSON-readable result of one `wwc repo` registry command.
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum RepoAdminOutcome {
    /// A repository binding was registered locally.
    Registered {
        /// The registered binding view.
        repository: RepositoryBindingView,
        /// Whether Git was initialized after the explicit confirmation.
        git_initialized: bool,
    },
    /// Every locally registered binding, in binding-id order.
    List {
        /// The binding views.
        repositories: Vec<RepositoryBindingView>,
    },
    /// A binding was removed locally.
    Removed {
        /// The removed binding id.
        repository_binding_id: String,
    },
}

/// Failure of one `wwc repo` registry command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoAdminError {
    /// The data directory holds no device identity: the Device Client never
    /// ran here.
    NotInitialized,
    /// The device has not completed enrollment with a Server, so a
    /// repository frame could never be re-keyed onto the assigned node.
    NotEnrolled,
    /// The binding id is unknown in this data directory.
    NotFound,
    /// The registration check chain refused the directory; `availability`
    /// carries the plan 13.5 failure state in its wire spelling.
    Rejected {
        /// The seven-state failure (`invalid_git`, `moved`, …).
        availability: String,
        /// Local-only human-readable reason.
        detail: String,
    },
    /// The command cannot be completed. `code` is stable for scripting.
    Failed {
        /// Stable machine-readable failure code.
        code: &'static str,
        /// Human-readable explanation in the CLI language.
        message: String,
    },
}

impl fmt::Display for RepoAdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => formatter.write_str("该数据目录还没有设备身份"),
            Self::NotEnrolled => formatter.write_str("设备尚未完成 enrollment 注册"),
            Self::NotFound => formatter.write_str("该数据目录没有这个仓库绑定"),
            Self::Rejected {
                availability,
                detail,
            } => write!(formatter, "注册检查未通过 [{availability}]：{detail}"),
            Self::Failed { code, message } => write!(formatter, "[{code}] {message}"),
        }
    }
}

/// Registers a directory as a repository binding (plan §13.1 `wwc repo add`).
///
/// # Errors
///
/// Returns [`RepoAdminError::NotInitialized`] before the first device boot,
/// [`RepoAdminError::NotEnrolled`] before the enrollment adoption,
/// [`RepoAdminError::Rejected`] when the registration check chain refuses
/// the directory, and [`RepoAdminError::Failed`] for durable failures.
pub fn repo_add(
    data_directory: &Path,
    requested_path: &Path,
    confirm_git_init: bool,
) -> Result<RepoAdminOutcome, RepoAdminError> {
    let mut store = open_store(data_directory).map_err(repo_store_failed)?;
    let (node, instance) = prepare_bound_stream(&mut store)?;
    let registration = repository::register_repository(
        &mut store,
        &node,
        &instance,
        requested_path,
        &RegistrationOptions { confirm_git_init },
        OffsetDateTime::now_utc(),
    )
    .map_err(map_registry_error)?;
    Ok(RepoAdminOutcome::Registered {
        repository: view_from_registration(&registration),
        git_initialized: registration.git_initialized_by_registration,
    })
}

/// Lists every locally registered binding with its latest scan projection
/// (plan §13.1 `wwc repo list`). Read-only: no scan runs and no frame is
/// produced, so no enrollment is required.
///
/// # Errors
///
/// Returns [`RepoAdminError::NotInitialized`] before the first device boot
/// and [`RepoAdminError::Failed`] when the store cannot serve the read.
pub fn repo_list(data_directory: &Path) -> Result<RepoAdminOutcome, RepoAdminError> {
    let store = open_store(data_directory).map_err(repo_store_failed)?;
    if load_device_identity(&store)
        .map_err(|error| store_failure(&error))?
        .is_none()
    {
        return Err(RepoAdminError::NotInitialized);
    }
    let summaries = repository::list_bindings(&store).map_err(map_registry_store_error)?;
    Ok(RepoAdminOutcome::List {
        repositories: summaries.iter().map(binding_view).collect(),
    })
}

/// Removes a repository binding (plan §13.1 `wwc repo remove`): the local
/// mapping and scan projection are deleted and the durable
/// `client.repository.removed` frame rides the outbox to the Server.
///
/// # Errors
///
/// Returns [`RepoAdminError::NotInitialized`] before the first device boot,
/// [`RepoAdminError::NotEnrolled`] before the enrollment adoption,
/// [`RepoAdminError::NotFound`] for an unknown binding id, and
/// [`RepoAdminError::Failed`] for durable failures.
pub fn repo_remove(
    data_directory: &Path,
    repository_binding_id: &str,
) -> Result<RepoAdminOutcome, RepoAdminError> {
    let mut store = open_store(data_directory).map_err(repo_store_failed)?;
    let (node, instance) = prepare_bound_stream(&mut store)?;
    let removal = repository::remove_repository(
        &mut store,
        &node,
        &instance,
        repository_binding_id,
        OffsetDateTime::now_utc(),
    )
    .map_err(map_registry_error)?;
    Ok(RepoAdminOutcome::Removed {
        repository_binding_id: removal.repository_binding_id,
    })
}

/// Loads the identity, requires the adopted enrollment, rotates the launch
/// instance (one CLI run is one launch), and binds the durable outbox
/// stream — the same preconditions `wwc device refresh-code` establishes
/// before appending frames. Returns the bound `(clientNodeId,
/// clientInstanceId)` sender pair.
fn prepare_bound_stream(store: &mut DeviceStore) -> Result<(String, String), RepoAdminError> {
    let Some(identity) = load_device_identity(store).map_err(|error| store_failure(&error))? else {
        return Err(RepoAdminError::NotInitialized);
    };
    if !identity.identity().is_enrolled() {
        return Err(RepoAdminError::NotEnrolled);
    }
    let identity = ensure_device_identity(store, &rotation_seed(), &now_rfc3339())
        .map_err(|error| store_failure(&error))?;
    let node = identity.identity().client_node_id().to_owned();
    let instance = identity.current_instance_id().to_owned();
    store
        .bind_outbox_stream(&node, &instance)
        .map_err(|error| store_failure(&error))?;
    Ok((node, instance))
}

fn view_from_registration(registration: &RepositoryRegistration) -> RepositoryBindingView {
    let projection = &registration.projection;
    RepositoryBindingView {
        repository_binding_id: registration.repository_binding_id.clone(),
        canonical_path: registration.canonical_path.to_string_lossy().into_owned(),
        git_common_directory: registration
            .git_common_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        availability: availability_wire_name(projection.availability).to_owned(),
        dirty_state: Some(dirty_state_wire_name(projection.dirty_state).to_owned()),
        default_branch: Some(projection.default_branch.clone()),
        head_commit: if projection.head_commit.is_empty() {
            None
        } else {
            Some(projection.head_commit.clone())
        },
        last_scanned_at: Some(projection.last_scanned_at.clone()),
        last_canonicalized_at: Some(projection.last_scanned_at.clone()),
    }
}

fn binding_view(
    summary: &winwincode_device_client::RepositoryBindingSummary,
) -> RepositoryBindingView {
    RepositoryBindingView {
        repository_binding_id: summary.mapping.repository_binding_id.clone(),
        canonical_path: summary.mapping.canonical_path.clone(),
        git_common_directory: summary.mapping.git_common_directory.clone(),
        availability: summary.mapping.local_state.clone(),
        dirty_state: summary
            .scan
            .as_ref()
            .map(|scan| dirty_state_wire_name(scan.dirty_state).to_owned()),
        default_branch: None,
        head_commit: summary
            .scan
            .as_ref()
            .and_then(|scan| scan.head_commit.clone())
            .filter(|head| !head.is_empty()),
        last_scanned_at: summary
            .scan
            .as_ref()
            .and_then(|scan| scan.last_scanned_at.clone()),
        last_canonicalized_at: summary.mapping.last_canonicalized_at.clone(),
    }
}

fn map_registry_error(error: RepositoryRegistryError) -> RepoAdminError {
    match error {
        RepositoryRegistryError::Store(store) => store_failure(&store),
        RepositoryRegistryError::NotEnrolled => RepoAdminError::NotEnrolled,
        RepositoryRegistryError::NotFound => RepoAdminError::NotFound,
        RepositoryRegistryError::AlreadyRegistered {
            repository_binding_id,
        } => RepoAdminError::Failed {
            code: "repository-already-registered",
            message: format!("该目录已注册为绑定 {repository_binding_id}"),
        },
        RepositoryRegistryError::Rejected(rejection) => RepoAdminError::Rejected {
            availability: availability_wire_name(rejection.availability).to_owned(),
            detail: rejection.detail,
        },
        RepositoryRegistryError::InvalidInput(message) => RepoAdminError::Failed {
            code: "repository-invalid-input",
            message,
        },
        RepositoryRegistryError::Protocol(message) => RepoAdminError::Failed {
            code: "repository-protocol",
            message,
        },
    }
}

/// Narrow a registry failure down to its store variant for the shared
/// store-failure rendering.
fn map_registry_store_error(error: RepositoryRegistryError) -> RepoAdminError {
    match error {
        RepositoryRegistryError::Store(store) => store_failure(&store),
        other => RepoAdminError::Failed {
            code: "repository-registry",
            message: other.to_string(),
        },
    }
}

fn store_failure(error: &winwincode_device_client::DeviceStoreError) -> RepoAdminError {
    RepoAdminError::Failed {
        code: "device-store",
        message: error.to_string(),
    }
}

fn repo_store_failed(error: DeviceAdminError) -> RepoAdminError {
    match error {
        DeviceAdminError::Failed { code, message } => RepoAdminError::Failed { code, message },
        other => RepoAdminError::Failed {
            code: "device-store",
            message: other.to_string(),
        },
    }
}
