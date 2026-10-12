// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use winwincode_client_port::managed_app::ManagedAppRunTemplate;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::RepositoryBindingService;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::RepositoryBindingRecord;
use winwincode_storage::{ManagedAppRunTemplateRecord, SqliteStorage};

const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

const UNKNOWN_FIELD: &str = "unknown";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRepositoriesErrorKind {
    InvalidRequest,
    ClientNotFound,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRepositoriesError {
    kind: ClientRepositoriesErrorKind,
    message: String,
}

impl ClientRepositoriesError {
    #[must_use]
    pub const fn kind(&self) -> ClientRepositoriesErrorKind {
        self.kind
    }

    fn invalid_request() -> Self {
        Self::new(
            ClientRepositoriesErrorKind::InvalidRequest,
            "repository list requires exactly one digit clientId query parameter",
        )
    }

    fn client_not_found() -> Self {
        Self::new(
            ClientRepositoriesErrorKind::ClientNotFound,
            "no client matches the requested id",
        )
    }

    fn unavailable() -> Self {
        Self::new(
            ClientRepositoriesErrorKind::Unavailable,
            "repository directory service is unavailable",
        )
    }

    fn new(kind: ClientRepositoriesErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ClientRepositoriesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientRepositoriesError {}

#[derive(Debug, Clone)]
pub struct ClientRepositoriesApplication {
    data_directory: PathBuf,
}

impl ClientRepositoriesApplication {
    #[must_use]
    pub fn open(data_directory: impl Into<PathBuf>) -> Self {
        Self {
            data_directory: data_directory.into(),
        }
    }

    pub fn list(
        &self,
        user_id: &str,
        query: Option<&str>,
    ) -> Result<Value, ClientRepositoriesError> {
        let public_client_id = repository_list_client_id(query)?;
        let mut storage = self.open_storage()?;
        let node = {
            let mut registry = ClientRegistryService::new(&mut storage);
            match registry
                .snapshot_by_public_client_id(&public_client_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
            {
                Some(node)
                    if !matches!(
                        node.presence_state,
                        ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked
                    ) =>
                {
                    node
                }
                _ => return Err(ClientRepositoriesError::client_not_found()),
            }
        };
        let bindings = {
            let mut bindings = RepositoryBindingService::new(&mut storage);
            bindings
                .visible_bindings(user_id, &node.client_node_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
        };
        let repositories = bindings.iter().map(repository_summary).collect::<Vec<_>>();
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "repositories": repositories,
        }))
    }

    pub fn save_managed_app_template(
        &self,
        user_id: &str,
        repository_binding_id: &str,
        value: Value,
    ) -> Result<Value, ClientRepositoriesError> {
        let template: ManagedAppRunTemplate =
            serde_json::from_value(value).map_err(|_| ClientRepositoriesError::unavailable())?;
        template
            .validate()
            .map_err(|_| ClientRepositoriesError::unavailable())?;
        let mut storage = self.open_storage()?;
        let binding = {
            let mut bindings = RepositoryBindingService::new(&mut storage);
            bindings
                .snapshot(repository_binding_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
                .ok_or_else(ClientRepositoriesError::unavailable)?
        };
        let visible = {
            let mut bindings = RepositoryBindingService::new(&mut storage);
            bindings
                .visible_bindings(user_id, &binding.client_node_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
                .into_iter()
                .any(|record| record.repository_binding_id == repository_binding_id)
        };
        if !visible {
            return Err(ClientRepositoriesError::unavailable());
        }
        let revision = storage
            .load_managed_app_run_template(repository_binding_id)
            .map_err(|_| ClientRepositoriesError::unavailable())?
            .map_or(1, |record| record.revision.saturating_add(1));
        storage
            .save_managed_app_run_template(&ManagedAppRunTemplateRecord {
                repository_binding_id: repository_binding_id.to_owned(),
                revision,
                template_json: serde_json::to_vec(&template)
                    .map_err(|_| ClientRepositoriesError::unavailable())?,
            })
            .map_err(|_| ClientRepositoriesError::unavailable())?;
        Ok(json!({
            "schemaVersion": template.schema_version,
            "repositoryBindingId": repository_binding_id,
            "revision": revision,
        }))
    }

    pub fn load_managed_app_template(
        &self,
        user_id: &str,
        repository_binding_id: &str,
    ) -> Result<Value, ClientRepositoriesError> {
        let mut storage = self.open_storage()?;
        let binding = {
            let mut bindings = RepositoryBindingService::new(&mut storage);
            bindings
                .snapshot(repository_binding_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
                .ok_or_else(ClientRepositoriesError::unavailable)?
        };
        let visible = {
            let mut bindings = RepositoryBindingService::new(&mut storage);
            bindings
                .visible_bindings(user_id, &binding.client_node_id)
                .map_err(|_| ClientRepositoriesError::unavailable())?
                .into_iter()
                .any(|record| record.repository_binding_id == repository_binding_id)
        };
        if !visible {
            return Err(ClientRepositoriesError::unavailable());
        }
        let record = storage
            .load_managed_app_run_template(repository_binding_id)
            .map_err(|_| ClientRepositoriesError::unavailable())?;
        let Some(record) = record else {
            return Ok(json!({
                "schemaVersion": SUPPORTED_SCHEMA_VERSION,
                "repositoryBindingId": repository_binding_id,
                "revision": null,
                "template": null,
            }));
        };
        let template: ManagedAppRunTemplate = serde_json::from_slice(&record.template_json)
            .map_err(|_| ClientRepositoriesError::unavailable())?;
        template
            .validate()
            .map_err(|_| ClientRepositoriesError::unavailable())?;
        Ok(json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "repositoryBindingId": repository_binding_id,
            "revision": record.revision,
            "template": template,
        }))
    }

    fn open_storage(&self) -> Result<SqliteStorage, ClientRepositoriesError> {
        SqliteStorage::open(&self.data_directory)
            .map_err(|_| ClientRepositoriesError::unavailable())
    }
}

fn repository_summary(record: &RepositoryBindingRecord) -> Value {
    json!({
        "repositoryBindingId": record.repository_binding_id,
        "displayName": record.display_name,
        "defaultBranch": record.default_branch.as_deref().unwrap_or(UNKNOWN_FIELD),
        "headCommit": record.head_commit.as_deref().unwrap_or(UNKNOWN_FIELD),
        "dirtyState": record.dirty_state.as_str(),
        "availability": record.availability.as_str(),
    })
}

fn repository_list_client_id(query: Option<&str>) -> Result<String, ClientRepositoriesError> {
    let Some(query) = query else {
        return Err(ClientRepositoriesError::invalid_request());
    };
    let mut client_id: Option<&str> = None;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(ClientRepositoriesError::invalid_request());
        };
        if name != "clientId" || client_id.replace(value).is_some() {
            return Err(ClientRepositoriesError::invalid_request());
        }
    }
    let Some(value) = client_id else {
        return Err(ClientRepositoriesError::invalid_request());
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ClientRepositoriesError::invalid_request());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_control_plane::RepositoryAvailability;
    use winwincode_control_plane::RepositoryDirtyState;
    use winwincode_domain::Instant;

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn record(
        default_branch: Option<&str>,
        head_commit: Option<&str>,
        dirty_state: RepositoryDirtyState,
        availability: RepositoryAvailability,
    ) -> RepositoryBindingRecord {
        RepositoryBindingRecord {
            repository_binding_id: "rbd_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
            client_node_id: "cnd_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
            display_name: "Alpha".to_owned(),
            repository_kind: "git".to_owned(),
            default_branch: default_branch.map(str::to_owned),
            head_commit: head_commit.map(str::to_owned),
            dirty_state,
            availability,
            repository_fingerprint: "fingerprint".to_owned(),
            last_scanned_at: Some(instant("2026-09-04T12:00:01.000Z")),
            created_at: instant("2026-09-04T12:00:00.000Z"),
            revision: 3,
        }
    }

    #[test]
    fn repository_summary_matches_the_facade_contract_field_by_field() {
        let value = repository_summary(&record(
            Some("main"),
            Some(&"a".repeat(40)),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
        ));
        let object = value.as_object().expect("summary object");
        let mut field_names = object.keys().map(String::as_str).collect::<Vec<_>>();
        field_names.sort_unstable();
        assert_eq!(
            field_names,
            vec![
                "availability",
                "defaultBranch",
                "dirtyState",
                "displayName",
                "headCommit",
                "repositoryBindingId",
            ],
            "exactly the facade fields"
        );
        assert_eq!(
            value["repositoryBindingId"],
            "rbd_AAAAAAAAAAAAAAAAAAAAAAAA1"
        );
        assert_eq!(value["displayName"], "Alpha");
        assert_eq!(value["defaultBranch"], "main");
        assert_eq!(value["headCommit"], "a".repeat(40));
        assert_eq!(value["dirtyState"], "clean");
        assert_eq!(value["availability"], "available");
    }

    #[test]
    fn repository_summary_maps_the_seven_state_and_two_state_vocabularies() {
        let moved = repository_summary(&record(
            Some("trunk"),
            Some(&"b".repeat(64)),
            RepositoryDirtyState::Dirty,
            RepositoryAvailability::Moved,
        ));
        assert_eq!(moved["dirtyState"], "dirty");
        assert_eq!(moved["availability"], "moved");
        assert_eq!(moved["headCommit"], "b".repeat(64));
    }

    #[test]
    fn unknown_device_facts_project_stable_non_empty_placeholders() {
        let value = repository_summary(&record(
            None,
            None,
            RepositoryDirtyState::Dirty,
            RepositoryAvailability::ScanFailed,
        ));
        assert_eq!(value["defaultBranch"], "unknown");
        assert_eq!(value["headCommit"], "unknown");
        assert_eq!(value["availability"], "scan_failed");
    }

    #[test]
    fn client_id_query_parsing_accepts_only_one_digit_parameter() {
        assert_eq!(
            repository_list_client_id(Some("clientId=927351842")).expect("valid id"),
            "927351842"
        );
        for malformed in [
            None,
            Some(""),
            Some("clientId="),
            Some("clientId=12ab"),
            Some("foo=1"),
            Some("clientId=1&clientId=2"),
            Some("clientId=927351842&extra=1"),
            Some("clientId=927351842&clientId"),
            Some("clientid=927351842"),
        ] {
            assert!(
                repository_list_client_id(malformed).is_err(),
                "expected {malformed:?} to be rejected"
            );
        }
    }
}
