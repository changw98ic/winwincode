// SPDX-License-Identifier: Apache-2.0

//! The signed-in user's repository directory for one Client device (plan
//! 13.4, REPO-100.4): `GET /api/v1/repositories?clientId=…` projects the
//! durable repository bindings of that Client onto the repo-ui facade shape
//! (REPO-100.3): `{schemaVersion, repositories: [{repositoryBindingId,
//! displayName, defaultBranch, headCommit, dirtyState, availability}]}`.
//!
//! Visibility follows plan 13.4 exactly: an `active` `ClientAccessGrant`
//! carrying `use` on the client node AND an `active`
//! `RepositoryAccessGrant` on the binding must both exist
//! (`RepositoryBindingService::visible_bindings`); a binding missing either
//! grant is invisible. The route is a read-only directory projection over the
//! durable Device-Client-reported facts: occupancy leases and presence play
//! no part, every card is produced from the same projection the device
//! refreshes at launch (`client.repository.status`), and no absolute path
//! ever crosses the boundary.
//!
//! The query identity is the public Client id (plan 11.2: digits only). A
//! pending-enrollment or revoked identity is not a Client (or any more) and
//! reads as `CLIENT_NOT_FOUND`, matching the connect boundary.

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use serde_json::json;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::RepositoryBindingService;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::RepositoryBindingRecord;
use winwincode_storage::SqliteStorage;

/// Schema version of the public browser-facing repository directory.
const SUPPORTED_SCHEMA_VERSION: &str = "winwincode/v1";

/// Wire text for a device-reported fact the Server does not know. The repo-ui
/// facade requires non-empty `defaultBranch` and `headCommit` strings, so a
/// binding whose last scan could not determine them projects a stable
/// non-empty placeholder instead of breaking the whole list parse.
const UNKNOWN_FIELD: &str = "unknown";

/// Stable failure categories of the repository directory boundary. Each
/// category maps to exactly one wire error code of the §16.3 taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRepositoriesErrorKind {
    /// The request query did not carry exactly one digit `clientId`.
    InvalidRequest,
    /// The public Client ID does not name a Client.
    ClientNotFound,
    /// Durable state or storage failed; nothing was decided.
    Unavailable,
}

/// Secret-free repository directory failure.
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

/// The signed-in user's repository directory over the Server's one
/// product-state database directory. Like the client connections surface,
/// every operation opens and closes its own storage connection so concurrent
/// flows never share state in memory.
#[derive(Debug, Clone)]
pub struct ClientRepositoriesApplication {
    data_directory: PathBuf,
}

impl ClientRepositoriesApplication {
    /// Composes the directory application over one product-state directory.
    #[must_use]
    pub fn open(data_directory: impl Into<PathBuf>) -> Self {
        Self {
            data_directory: data_directory.into(),
        }
    }

    /// Projects the user's visible repository list for the Client named by
    /// the request query (`?clientId=…`).
    ///
    /// # Errors
    ///
    /// Returns the stable boundary categories: a missing, repeated, or
    /// malformed `clientId` is `InvalidRequest`; a public id that names no
    /// Client, or a pending-enrollment or revoked one, is `ClientNotFound`;
    /// storage failure is `Unavailable`.
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
                // Pending-enrollment and revoked identities are not Clients
                // yet (or any more); the boundary cannot list them.
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

    fn open_storage(&self) -> Result<SqliteStorage, ClientRepositoriesError> {
        SqliteStorage::open(&self.data_directory)
            .map_err(|_| ClientRepositoriesError::unavailable())
    }
}

/// One repository card: exactly the fields the repo-ui facade validates
/// (REPO-100.3), under the facade's names.
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

/// Reads the query identity: exactly one `clientId` parameter carrying a
/// non-empty digit string (the public Client id, plan 11.2). Any other query
/// shape is a request failure, mirroring the browser facade's own input
/// assertion.
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
