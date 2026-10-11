// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use winwincode_device_client::repository::{self, RegistrationOptions, RepositoryRegistryError};
use winwincode_device_client::{
    DeviceStore, RepositoryRegistration, availability_wire_name, dirty_state_wire_name,
    load_device_identity,
};

use crate::device_admin::{DeviceAdminError, open_store};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryBindingView {
    pub repository_binding_id: String,
    pub canonical_path: String,
    pub git_common_directory: Option<String>,
    pub availability: String,
    pub dirty_state: Option<String>,
    pub default_branch: Option<String>,
    pub head_commit: Option<String>,
    pub last_scanned_at: Option<String>,
    pub last_canonicalized_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum RepoAdminOutcome {
    Registered {
        repository: RepositoryBindingView,
        git_initialized: bool,
    },
    List {
        repositories: Vec<RepositoryBindingView>,
    },
    Removed {
        repository_binding_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoAdminError {
    NotInitialized,
    NotEnrolled,
    NotFound,
    Rejected {
        availability: String,
        detail: String,
    },
    Failed {
        code: &'static str,
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

fn prepare_bound_stream(store: &mut DeviceStore) -> Result<(String, String), RepoAdminError> {
    let Some(identity) = load_device_identity(store).map_err(|error| store_failure(&error))? else {
        return Err(RepoAdminError::NotInitialized);
    };
    if !identity.identity().is_enrolled() {
        return Err(RepoAdminError::NotEnrolled);
    }
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
