// SPDX-License-Identifier: Apache-2.0

//! Generated enterprise Fleet queries over the canonical Worker Registry.
//!
//! The application adapter owns only validation, public DTO conversion, and
//! an opaque scope-bound cursor. Inventory snapshots remain in
//! [`winwincode_storage::WorkerFleetInventoryStore`], while Worker identity,
//! heartbeat, capacity, leases, and pool placement remain in the single
//! durable Worker Registry.

use std::collections::HashSet;
use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, EnterpriseFleetListQuery, EnterpriseFleetListResultResponse,
    EnterpriseFleetListResultResponseQuery, EnterpriseFleetPage, EnterpriseFleetPageKind,
    EnterpriseFleetProjection, PageInfo, Scope,
};
use winwincode_domain::{EnterpriseWorkerPoolId, Instant, OpaqueCursor, Revision};
use winwincode_storage::{
    ReceiptScopeKey, SqliteStorage, StorageError, StorageErrorKind, WorkerFleetInventoryState,
    WorkerFleetPoolInventory, WorkerFleetSnapshotCursor, WorkerFleetSnapshotRequest,
    WorkerRegistryScope,
};

use crate::command_receipt_identity;

const CURSOR_SCHEMA: &str = "winwincode.enterprise-fleet-page.v1";
const MAX_CURSOR_BYTES: usize = 2_048;
const DEFAULT_STALE_AFTER_MS: u64 = 15_000;

/// Stable public error categories for Fleet query transport mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerFleetProjectionServiceErrorKind {
    InvalidRequest,
    CursorExpired,
    Storage,
}

/// Bounded Fleet query failure that never exposes storage diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFleetProjectionServiceError {
    kind: WorkerFleetProjectionServiceErrorKind,
    message: &'static str,
}

impl WorkerFleetProjectionServiceError {
    const fn new(kind: WorkerFleetProjectionServiceErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid_request() -> Self {
        Self::new(
            WorkerFleetProjectionServiceErrorKind::InvalidRequest,
            "Fleet query is invalid",
        )
    }

    const fn cursor_expired() -> Self {
        Self::new(
            WorkerFleetProjectionServiceErrorKind::CursorExpired,
            "Fleet page cursor has expired",
        )
    }

    const fn storage() -> Self {
        Self::new(
            WorkerFleetProjectionServiceErrorKind::Storage,
            "Fleet inventory is unavailable",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerFleetProjectionServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerFleetProjectionServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for WorkerFleetProjectionServiceError {}

/// Generated query adapter over one authoritative Fleet inventory connection.
pub struct WorkerFleetProjectionService<'storage> {
    storage: &'storage mut SqliteStorage,
    stale_after_ms: u64,
}

impl<'storage> WorkerFleetProjectionService<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self {
            storage,
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
        }
    }

    /// Overrides the heartbeat age used for fresh snapshots.
    ///
    /// Invalid intervals are rejected by the inventory adapter when a query
    /// is executed. This constructor is useful for a deployment-specific
    /// heartbeat policy and deterministic integration tests.
    #[must_use]
    pub const fn with_stale_after_ms(
        storage: &'storage mut SqliteStorage,
        stale_after_ms: u64,
    ) -> Self {
        Self {
            storage,
            stale_after_ms,
        }
    }

    /// Lists a bounded, fixed-snapshot page of exact-scope Worker pools.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, scopes, filters, limits, and cursors.
    /// Retained snapshot loss is returned separately from an inventory
    /// availability failure so transports can emit the canonical cursor error.
    pub fn list(
        &mut self,
        query: &EnterpriseFleetListQuery,
        observed_at: &Instant,
    ) -> Result<EnterpriseFleetListResultResponse, WorkerFleetProjectionServiceError> {
        let scope_key = validate_query_identity(
            &query.actor,
            &query.scope,
            &query.request_id,
            query.page.limit,
        )?;
        let states = inventory_states(&query.parameters.states)?;
        let cursor = decode_cursor(
            query.page.cursor.as_ref(),
            &scope_key,
            &states,
            self.stale_after_ms,
        )?;
        let limit = usize::try_from(query.page.limit)
            .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())?;
        let page = self
            .storage
            .worker_fleet_inventory()
            .and_then(|mut inventory| {
                inventory.page(&WorkerFleetSnapshotRequest {
                    scope: worker_scope(&query.scope),
                    states: states.clone(),
                    observed_at: observed_at.clone(),
                    stale_after_ms: self.stale_after_ms,
                    limit,
                    cursor,
                })
            })
            .map_err(|error| storage_error(&error))?;
        let next_cursor = page
            .next_cursor
            .as_ref()
            .map(|cursor| encode_cursor(cursor, &scope_key, &states, self.stale_after_ms))
            .transpose()?;
        let items = page
            .items
            .iter()
            .map(fleet_projection)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EnterpriseFleetListResultResponse {
            page: PageInfo {
                has_more: next_cursor.is_some(),
                next_cursor,
            },
            query: EnterpriseFleetListResultResponseQuery::EnterpriseFleetList,
            request_id: query.request_id.clone(),
            result: EnterpriseFleetPage {
                items,
                kind: EnterpriseFleetPageKind::EnterpriseFleetPage,
                snapshot_revision: revision(page.snapshot_revision)?,
            },
            schema_version: query.schema_version.clone(),
        })
    }
}

fn validate_query_identity(
    actor: &Actor,
    scope: &Scope,
    request_id: &winwincode_domain::RequestId,
    limit: i64,
) -> Result<ReceiptScopeKey, WorkerFleetProjectionServiceError> {
    if !(1..=100).contains(&limit) {
        return Err(WorkerFleetProjectionServiceError::invalid_request());
    }
    command_receipt_identity(actor, scope, request_id.clone())
        .map(|identity| identity.scope_key().clone())
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())
}

fn worker_scope(scope: &Scope) -> WorkerRegistryScope {
    match scope {
        Scope::OrganizationScope(scope) => WorkerRegistryScope::Organization {
            organization_id: scope.organization_id.clone(),
        },
        Scope::WorkspaceScope(scope) => WorkerRegistryScope::Workspace {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        },
        Scope::ProjectScope(scope) => WorkerRegistryScope::Project {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
        },
        Scope::RepositoryScope(scope) => WorkerRegistryScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
    }
}

fn inventory_states(
    states: &[String],
) -> Result<Vec<WorkerFleetInventoryState>, WorkerFleetProjectionServiceError> {
    let mut seen = HashSet::with_capacity(states.len());
    for state in states {
        if !seen.insert(state.as_str())
            || !matches!(
                state.as_str(),
                "healthy" | "degraded" | "draining" | "offline"
            )
        {
            return Err(WorkerFleetProjectionServiceError::invalid_request());
        }
    }
    let mut result = Vec::with_capacity(states.len());
    for (name, state) in [
        ("healthy", WorkerFleetInventoryState::Healthy),
        ("degraded", WorkerFleetInventoryState::Degraded),
        ("draining", WorkerFleetInventoryState::Draining),
        ("offline", WorkerFleetInventoryState::Offline),
    ] {
        if seen.contains(name) {
            result.push(state);
        }
    }
    Ok(result)
}

fn fleet_projection(
    inventory: &WorkerFleetPoolInventory,
) -> Result<EnterpriseFleetProjection, WorkerFleetProjectionServiceError> {
    Ok(EnterpriseFleetProjection {
        active_leases: public_i64(inventory.active_leases)?,
        available_capacity: public_i64(inventory.available_capacity)?,
        display_name: inventory.display_name.clone(),
        id: EnterpriseWorkerPoolId(inventory.worker_pool_id.0.clone()),
        labels: inventory.labels.clone(),
        registered_workers: public_i64(inventory.registered_workers)?,
        revision: revision(inventory.revision)?,
        state: match inventory.state {
            WorkerFleetInventoryState::Healthy => "healthy",
            WorkerFleetInventoryState::Degraded => "degraded",
            WorkerFleetInventoryState::Draining => "draining",
            WorkerFleetInventoryState::Offline => "offline",
        }
        .to_owned(),
        updated_at: inventory.updated_at.clone(),
    })
}

fn revision(value: u64) -> Result<Revision, WorkerFleetProjectionServiceError> {
    public_i64(value).map(Revision)
}

fn public_i64(value: u64) -> Result<i64, WorkerFleetProjectionServiceError> {
    i64::try_from(value).map_err(|_| WorkerFleetProjectionServiceError::storage())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FleetPageCursor {
    schema: String,
    scope_sha256: String,
    states_sha256: String,
    stale_after_ms: u64,
    snapshot: WorkerFleetSnapshotCursor,
}

fn encode_cursor(
    cursor: &WorkerFleetSnapshotCursor,
    scope: &ReceiptScopeKey,
    states: &[WorkerFleetInventoryState],
    stale_after_ms: u64,
) -> Result<OpaqueCursor, WorkerFleetProjectionServiceError> {
    let encoded = FleetPageCursor {
        schema: CURSOR_SCHEMA.to_owned(),
        scope_sha256: digest_bytes(scope.as_bytes()),
        states_sha256: digest_json(states)?,
        stale_after_ms,
        snapshot: cursor.clone(),
    };
    serde_json::to_vec(&encoded)
        .map(|bytes| OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    scope: &ReceiptScopeKey,
    states: &[WorkerFleetInventoryState],
    stale_after_ms: u64,
) -> Result<Option<WorkerFleetSnapshotCursor>, WorkerFleetProjectionServiceError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES {
        return Err(WorkerFleetProjectionServiceError::invalid_request());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())?;
    let decoded: FleetPageCursor = serde_json::from_slice(&bytes)
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())?;
    let canonical = serde_json::to_vec(&decoded)
        .map(|value| URL_SAFE_NO_PAD.encode(value))
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())?;
    if canonical != cursor.0
        || decoded.schema != CURSOR_SCHEMA
        || decoded.scope_sha256 != digest_bytes(scope.as_bytes())
        || decoded.states_sha256 != digest_json(states)?
        || decoded.stale_after_ms != stale_after_ms
    {
        return Err(WorkerFleetProjectionServiceError::invalid_request());
    }
    Ok(Some(decoded.snapshot))
}

fn digest_json<T: Serialize + ?Sized>(
    value: &T,
) -> Result<String, WorkerFleetProjectionServiceError> {
    serde_json::to_vec(value)
        .map(|bytes| digest_bytes(&bytes))
        .map_err(|_| WorkerFleetProjectionServiceError::invalid_request())
}

fn digest_bytes(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn storage_error(error: &StorageError) -> WorkerFleetProjectionServiceError {
    match error.kind() {
        StorageErrorKind::InvalidInput => WorkerFleetProjectionServiceError::invalid_request(),
        StorageErrorKind::EventCursorExpired => WorkerFleetProjectionServiceError::cursor_expired(),
        StorageErrorKind::RevisionConflict
        | StorageErrorKind::RequestConflict
        | StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => WorkerFleetProjectionServiceError::storage(),
    }
}
