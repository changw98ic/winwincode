// SPDX-License-Identifier: Apache-2.0

//! Stable enterprise Fleet projections over the canonical Worker Registry.
//!
//! This module owns only immutable read snapshots and their derived labels.
//! Worker identity, heartbeat, capacity, leases, and pool attribution remain
//! in [`crate::ExecutionRegistry`].

use std::collections::HashSet;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError, WorkerPoolId, WorkerRegistryScope};

const MAX_PAGE_SIZE: usize = 100;
const MAX_SNAPSHOT_POOLS: usize = 10_000;
const MAX_SNAPSHOT_PAYLOAD_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_POOL_PAYLOAD_BYTES: usize = 32 * 1_024;
const MAX_LABELS: usize = 64;
const RETAINED_SNAPSHOTS_PER_SCOPE: usize = 32;
const MIN_STALE_AFTER_MS: u64 = 1_000;
const MAX_STALE_AFTER_MS: u64 = 3_600_000;

const FLEET_INVENTORY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS worker_fleet_inventory_snapshots (
    snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_json TEXT NOT NULL,
    scope_revision INTEGER NOT NULL CHECK (scope_revision > 0),
    states_json TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    stale_after_ms INTEGER NOT NULL CHECK (stale_after_ms >= 1000),
    item_count INTEGER NOT NULL DEFAULT 0 CHECK (item_count >= 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS worker_fleet_inventory_scope_revision
    ON worker_fleet_inventory_snapshots (scope_json, scope_revision);
CREATE INDEX IF NOT EXISTS worker_fleet_inventory_scope_snapshots
    ON worker_fleet_inventory_snapshots (scope_json, snapshot_id DESC);
CREATE TABLE IF NOT EXISTS worker_fleet_inventory_snapshot_items (
    snapshot_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    worker_pool_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('healthy', 'degraded', 'draining', 'offline')),
    item_json TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, ordinal),
    UNIQUE (snapshot_id, worker_pool_id),
    FOREIGN KEY (snapshot_id) REFERENCES worker_fleet_inventory_snapshots(snapshot_id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS worker_fleet_inventory_snapshot_state
    ON worker_fleet_inventory_snapshot_items (snapshot_id, state, ordinal);
";

/// Public health state derived from current Registry and heartbeat facts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFleetInventoryState {
    Healthy,
    Degraded,
    Draining,
    Offline,
}

impl WorkerFleetInventoryState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::Offline => "offline",
        }
    }
}

/// One immutable pool aggregate from the single Worker Registry authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetPoolInventory {
    pub worker_pool_id: WorkerPoolId,
    pub display_name: String,
    pub state: WorkerFleetInventoryState,
    pub registered_workers: u64,
    pub usable_workers: u64,
    pub stale_workers: u64,
    pub active_leases: u64,
    pub max_capacity: u64,
    pub running_capacity: u64,
    pub reported_available_capacity: u64,
    pub available_capacity: u64,
    pub labels: Vec<String>,
    pub revision: u64,
    pub updated_at: Instant,
}

/// Durable continuation inside one materialized Fleet snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetSnapshotCursor {
    pub snapshot_id: u64,
    pub after_ordinal: u64,
}

/// Exact request used to create or continue one stable Fleet snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFleetSnapshotRequest {
    pub scope: WorkerRegistryScope,
    pub states: Vec<WorkerFleetInventoryState>,
    pub observed_at: Instant,
    pub stale_after_ms: u64,
    pub limit: usize,
    pub cursor: Option<WorkerFleetSnapshotCursor>,
}

/// One bounded page whose items never change while its cursor is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFleetInventoryPage {
    pub snapshot_revision: u64,
    pub items: Vec<WorkerFleetPoolInventory>,
    pub next_cursor: Option<WorkerFleetSnapshotCursor>,
}

/// Read-only Fleet projection adapter over the canonical Registry tables.
pub struct WorkerFleetInventoryStore<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the stable Fleet inventory projection.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the canonical Registry or projection
    /// snapshot schema cannot be prepared.
    pub fn worker_fleet_inventory(
        &mut self,
    ) -> Result<WorkerFleetInventoryStore<'_>, StorageError> {
        WorkerFleetInventoryStore::new(self)
    }
}

impl<'storage> WorkerFleetInventoryStore<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, StorageError> {
        {
            let _registry = storage.execution_registry()?;
        }
        storage
            .connection()?
            .execute_batch(FLEET_INVENTORY_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        Ok(Self { storage })
    }

    /// Creates or continues one immutable, exact-scope inventory snapshot.
    ///
    /// # Errors
    ///
    /// Rejects malformed scopes, filters, times, limits, and cursors. Returns
    /// cursor-expired only after the bounded retention window has removed a
    /// previously valid snapshot.
    pub fn page(
        &mut self,
        request: &WorkerFleetSnapshotRequest,
    ) -> Result<WorkerFleetInventoryPage, StorageError> {
        validate_request(request)?;
        let scope_json = encode_json(&request.scope)?;
        let states = canonical_states(&request.states)?;
        let states_json = encode_json(&states)?;
        let snapshot_id = match &request.cursor {
            Some(cursor) => {
                validate_snapshot_header(
                    self.storage.connection()?,
                    cursor,
                    &scope_json,
                    &states_json,
                    request.stale_after_ms,
                )?;
                cursor.snapshot_id
            }
            None => self.create_snapshot(request, &scope_json, &states_json, &states)?,
        };
        load_page(
            self.storage.connection()?,
            snapshot_id,
            request
                .cursor
                .as_ref()
                .map_or(0, |cursor| cursor.after_ordinal),
            request.limit,
        )
    }

    fn create_snapshot(
        &mut self,
        request: &WorkerFleetSnapshotRequest,
        scope_json: &str,
        states_json: &str,
        states: &[WorkerFleetInventoryState],
    ) -> Result<u64, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))?;
        validate_sqlite_instant(&transaction, &request.observed_at)?;
        let scope_revision = next_scope_revision(&transaction, scope_json)?;
        transaction
            .execute(
                "INSERT INTO worker_fleet_inventory_snapshots
                    (scope_json, scope_revision, states_json, observed_at,
                     stale_after_ms, item_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                params![
                    scope_json,
                    to_i64(scope_revision, "Fleet scope revision")?,
                    states_json,
                    request.observed_at.0,
                    to_i64(request.stale_after_ms, "Fleet stale interval")?,
                ],
            )
            .map_err(|error| sql_error(&error))?;
        let snapshot_id = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| StorageError::adapter("Fleet snapshot sequence is invalid"))?;
        let pool_ids = current_pool_ids(&transaction, scope_json)?;
        let mut ordinal = 0_u64;
        let mut payload_bytes = 0_usize;
        for worker_pool_id in pool_ids {
            let item = pool_inventory(
                &transaction,
                &worker_pool_id,
                scope_json,
                &request.observed_at,
                request.stale_after_ms,
                scope_revision,
            )?;
            if !states.is_empty() && !states.contains(&item.state) {
                continue;
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| StorageError::adapter("Fleet snapshot ordinal overflowed"))?;
            let item_json = encode_json(&item)?;
            if item_json.len() > MAX_POOL_PAYLOAD_BYTES {
                return Err(StorageError::invalid_input(
                    "Fleet pool inventory exceeds the supported payload size",
                ));
            }
            payload_bytes = payload_bytes
                .checked_add(item_json.len())
                .filter(|bytes| *bytes <= MAX_SNAPSHOT_PAYLOAD_BYTES)
                .ok_or_else(|| {
                    StorageError::invalid_input("Fleet snapshot exceeds the supported payload size")
                })?;
            transaction
                .execute(
                    "INSERT INTO worker_fleet_inventory_snapshot_items
                        (snapshot_id, ordinal, worker_pool_id, state, item_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        to_i64(snapshot_id, "Fleet snapshot sequence")?,
                        to_i64(ordinal, "Fleet snapshot ordinal")?,
                        item.worker_pool_id.0,
                        item.state.as_str(),
                        item_json,
                    ],
                )
                .map_err(|error| sql_error(&error))?;
        }
        transaction
            .execute(
                "UPDATE worker_fleet_inventory_snapshots SET item_count = ?1
                 WHERE snapshot_id = ?2",
                params![
                    to_i64(ordinal, "Fleet snapshot item count")?,
                    to_i64(snapshot_id, "Fleet snapshot sequence")?,
                ],
            )
            .map_err(|error| sql_error(&error))?;
        retain_recent_snapshots(&transaction, scope_json)?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(snapshot_id)
    }
}

fn validate_request(request: &WorkerFleetSnapshotRequest) -> Result<(), StorageError> {
    validate_scope(&request.scope)?;
    if request.limit == 0 || request.limit > MAX_PAGE_SIZE {
        return Err(StorageError::invalid_input(
            "Fleet page limit is outside the supported range",
        ));
    }
    if !(MIN_STALE_AFTER_MS..=MAX_STALE_AFTER_MS).contains(&request.stale_after_ms) {
        return Err(StorageError::invalid_input(
            "Fleet stale interval is outside the supported range",
        ));
    }
    if request.observed_at.0.is_empty() || request.observed_at.0.len() > 64 {
        return Err(StorageError::invalid_input(
            "Fleet observation time is invalid",
        ));
    }
    canonical_states(&request.states)?;
    if let Some(cursor) = &request.cursor
        && (cursor.snapshot_id == 0 || cursor.after_ordinal == 0)
    {
        return Err(StorageError::invalid_input("Fleet cursor is invalid"));
    }
    Ok(())
}

fn canonical_states(
    states: &[WorkerFleetInventoryState],
) -> Result<Vec<WorkerFleetInventoryState>, StorageError> {
    let mut seen = HashSet::with_capacity(states.len());
    if states.iter().any(|state| !seen.insert(*state)) {
        return Err(StorageError::invalid_input(
            "Fleet state filter contains duplicates",
        ));
    }
    let mut result = Vec::with_capacity(states.len());
    for state in [
        WorkerFleetInventoryState::Healthy,
        WorkerFleetInventoryState::Degraded,
        WorkerFleetInventoryState::Draining,
        WorkerFleetInventoryState::Offline,
    ] {
        if seen.contains(&state) {
            result.push(state);
        }
    }
    Ok(result)
}

fn current_pool_ids(
    transaction: &Transaction<'_>,
    scope_json: &str,
) -> Result<Vec<WorkerPoolId>, StorageError> {
    let mut statement = transaction
        .prepare(
            "SELECT DISTINCT placements.worker_pool_id
             FROM execution_worker_authenticated_placements AS placements
             JOIN execution_workers AS workers
               ON workers.worker_id = placements.worker_id
              AND workers.worker_instance_id = placements.worker_instance_id
             JOIN execution_worker_scopes AS scopes
               ON scopes.worker_id = workers.worker_id
             WHERE scopes.scope_json = ?1
             ORDER BY placements.worker_pool_id
             LIMIT ?2",
        )
        .map_err(|error| sql_error(&error))?;
    let pools = statement
        .query_map(
            params![
                scope_json,
                i64::try_from(MAX_SNAPSHOT_POOLS + 1)
                    .map_err(|_| StorageError::adapter("Fleet pool limit is invalid"))?,
            ],
            |row| Ok(WorkerPoolId(row.get(0)?)),
        )
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    if pools.len() > MAX_SNAPSHOT_POOLS {
        return Err(StorageError::invalid_input(
            "Fleet snapshot exceeds the supported pool count",
        ));
    }
    for pool in &pools {
        validate_id(&pool.0, "wpl_", "workerPoolId")?;
    }
    Ok(pools)
}

fn next_scope_revision(
    transaction: &Transaction<'_>,
    scope_json: &str,
) -> Result<u64, StorageError> {
    let current = transaction
        .query_row(
            "SELECT COALESCE(MAX(scope_revision), 0)
             FROM worker_fleet_inventory_snapshots WHERE scope_json = ?1",
            params![scope_json],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| sql_error(&error))?;
    stored_u64(current, "Fleet scope revision")?
        .checked_add(1)
        .ok_or_else(|| StorageError::adapter("Fleet scope revision overflowed"))
}

fn pool_inventory(
    transaction: &Transaction<'_>,
    worker_pool_id: &WorkerPoolId,
    scope_json: &str,
    observed_at: &Instant,
    stale_after_ms: u64,
    revision: u64,
) -> Result<WorkerFleetPoolInventory, StorageError> {
    let stale_after_ms = to_i64(stale_after_ms, "Fleet stale interval")?;
    let stored_counts = transaction
        .query_row(
            "WITH current_workers AS (
                 SELECT workers.*
                 FROM execution_worker_authenticated_placements AS placements
                 JOIN execution_workers AS workers
                   ON workers.worker_id = placements.worker_id
                  AND workers.worker_instance_id = placements.worker_instance_id
                 JOIN execution_worker_scopes AS scopes
                   ON scopes.worker_id = workers.worker_id
                 WHERE placements.worker_pool_id = ?1 AND scopes.scope_json = ?2
             ), active_lease_counts AS (
                 SELECT leases.worker_id, leases.worker_instance_id, COUNT(*) AS active_leases
                 FROM execution_leases AS leases
                 JOIN current_workers AS workers
                   ON workers.worker_id = leases.worker_id
                  AND workers.worker_instance_id = leases.worker_instance_id
                 WHERE julianday(leases.expires_at) > julianday(?3)
                   AND NOT EXISTS (
                       SELECT 1 FROM execution_lease_terminals AS terminals
                       WHERE terminals.lease_id = leases.lease_id
                   )
                 GROUP BY leases.worker_id, leases.worker_instance_id
             )
             SELECT COUNT(*),
                    COALESCE(SUM(COALESCE(active.active_leases, 0)), 0),
                    COALESCE(SUM(workers.max_slots), 0),
                    COALESCE(SUM(workers.running_slots), 0),
                    COALESCE(SUM(workers.available_slots), 0),
                    COALESCE(SUM(CASE WHEN workers.health = 'healthy'
                        AND workers.last_heartbeat_at IS NOT NULL
                        AND julianday(workers.last_heartbeat_at) <= julianday(?3)
                        AND (julianday(?3) - julianday(workers.last_heartbeat_at))
                            * 86400000.0 <= CAST(?4 AS REAL)
                        THEN workers.available_slots ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN workers.health = 'healthy'
                        AND workers.last_heartbeat_at IS NOT NULL
                        AND julianday(workers.last_heartbeat_at) <= julianday(?3)
                        AND (julianday(?3) - julianday(workers.last_heartbeat_at))
                            * 86400000.0 <= CAST(?4 AS REAL)
                        THEN 1 ELSE 0 END), 0)
             FROM current_workers AS workers
             LEFT JOIN active_lease_counts AS active
               ON active.worker_id = workers.worker_id
              AND active.worker_instance_id = workers.worker_instance_id",
            params![worker_pool_id.0, scope_json, observed_at.0, stale_after_ms],
            |row| {
                Ok(SqlPoolCounts {
                    registered_workers: row.get(0)?,
                    active_leases: row.get(1)?,
                    max_capacity: row.get(2)?,
                    running_capacity: row.get(3)?,
                    reported_available_capacity: row.get(4)?,
                    available_capacity: row.get(5)?,
                    usable_workers: row.get(6)?,
                })
            },
        )
        .map_err(|error| sql_error(&error))?;
    let counts = PoolCounts::try_from(stored_counts)?;
    if counts.registered_workers == 0
        || counts
            .running_capacity
            .checked_add(counts.reported_available_capacity)
            != Some(counts.max_capacity)
    {
        return Err(StorageError::adapter(
            "Fleet capacity facts do not reconcile",
        ));
    }
    let stale_workers = counts
        .registered_workers
        .checked_sub(counts.usable_workers)
        .ok_or_else(|| StorageError::adapter("Fleet usable Worker count is invalid"))?;
    let state = if counts.usable_workers == 0 {
        WorkerFleetInventoryState::Offline
    } else if stale_workers > 0 || counts.available_capacity == 0 {
        WorkerFleetInventoryState::Degraded
    } else {
        WorkerFleetInventoryState::Healthy
    };
    Ok(WorkerFleetPoolInventory {
        worker_pool_id: worker_pool_id.clone(),
        display_name: format!("Worker Pool {}", worker_pool_id.0),
        state,
        registered_workers: counts.registered_workers,
        usable_workers: counts.usable_workers,
        stale_workers,
        active_leases: counts.active_leases,
        max_capacity: counts.max_capacity,
        running_capacity: counts.running_capacity,
        reported_available_capacity: counts.reported_available_capacity,
        available_capacity: counts.available_capacity,
        labels: pool_labels(transaction, worker_pool_id, scope_json)?,
        revision,
        updated_at: observed_at.clone(),
    })
}

#[derive(Clone, Copy)]
struct PoolCounts {
    registered_workers: u64,
    active_leases: u64,
    max_capacity: u64,
    running_capacity: u64,
    reported_available_capacity: u64,
    available_capacity: u64,
    usable_workers: u64,
}

struct SqlPoolCounts {
    registered_workers: i64,
    active_leases: i64,
    max_capacity: i64,
    running_capacity: i64,
    reported_available_capacity: i64,
    available_capacity: i64,
    usable_workers: i64,
}

impl TryFrom<SqlPoolCounts> for PoolCounts {
    type Error = StorageError;

    fn try_from(value: SqlPoolCounts) -> Result<Self, Self::Error> {
        Ok(Self {
            registered_workers: stored_u64(value.registered_workers, "registered Workers")?,
            active_leases: stored_u64(value.active_leases, "active leases")?,
            max_capacity: stored_u64(value.max_capacity, "maximum capacity")?,
            running_capacity: stored_u64(value.running_capacity, "running capacity")?,
            reported_available_capacity: stored_u64(
                value.reported_available_capacity,
                "reported available capacity",
            )?,
            available_capacity: stored_u64(value.available_capacity, "available capacity")?,
            usable_workers: stored_u64(value.usable_workers, "usable Workers")?,
        })
    }
}

fn pool_labels(
    transaction: &Transaction<'_>,
    worker_pool_id: &WorkerPoolId,
    scope_json: &str,
) -> Result<Vec<String>, StorageError> {
    let mut statement = transaction
        .prepare(
            "WITH current_workers AS (
                 SELECT workers.*
                 FROM execution_worker_authenticated_placements AS placements
                 JOIN execution_workers AS workers
                   ON workers.worker_id = placements.worker_id
                  AND workers.worker_instance_id = placements.worker_instance_id
                 JOIN execution_worker_scopes AS scopes
                   ON scopes.worker_id = workers.worker_id
                 WHERE placements.worker_pool_id = ?1 AND scopes.scope_json = ?2
             ), labels(label) AS (
                 SELECT 'platform:' || platform FROM current_workers
                 UNION SELECT 'protocol:' || protocol_version FROM current_workers
                 UNION SELECT 'network-zone:' || security_zone FROM current_workers
                 UNION SELECT 'capability:' || capabilities.value
                     FROM current_workers, json_each(current_workers.capabilities) AS capabilities
             )
             SELECT label FROM labels ORDER BY label LIMIT ?3",
        )
        .map_err(|error| sql_error(&error))?;
    let labels = statement
        .query_map(
            params![
                worker_pool_id.0,
                scope_json,
                i64::try_from(MAX_LABELS + 1)
                    .map_err(|_| StorageError::adapter("Fleet label limit is invalid"))?,
            ],
            |row| row.get(0),
        )
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|error| sql_error(&error))?;
    if labels.len() > MAX_LABELS {
        return Err(StorageError::invalid_input(
            "Fleet inventory exceeds the supported label count",
        ));
    }
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 256
            || label.trim() != label
            || label.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err(StorageError::adapter("Fleet label facts are invalid"));
    }
    Ok(labels)
}

fn validate_snapshot_header(
    connection: &rusqlite::Connection,
    cursor: &WorkerFleetSnapshotCursor,
    scope_json: &str,
    states_json: &str,
    stale_after_ms: u64,
) -> Result<(), StorageError> {
    let header = connection
        .query_row(
            "SELECT scope_json, states_json, stale_after_ms
             FROM worker_fleet_inventory_snapshots
             WHERE snapshot_id = ?1",
            params![to_i64(cursor.snapshot_id, "Fleet snapshot sequence")?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(StorageError::event_cursor_expired)?;
    let stored_stale_after_ms = stored_u64(header.2, "Fleet stale interval")?;
    if header.0 != scope_json || header.1 != states_json || stored_stale_after_ms != stale_after_ms
    {
        return Err(StorageError::invalid_input(
            "Fleet cursor belongs to another query scope",
        ));
    }
    let ordinal_exists = connection
        .query_row(
            "SELECT 1 FROM worker_fleet_inventory_snapshot_items
             WHERE snapshot_id = ?1 AND ordinal = ?2",
            params![
                to_i64(cursor.snapshot_id, "Fleet snapshot sequence")?,
                to_i64(cursor.after_ordinal, "Fleet snapshot ordinal")?,
            ],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .is_some();
    if !ordinal_exists {
        return Err(StorageError::invalid_input(
            "Fleet cursor position is invalid",
        ));
    }
    Ok(())
}

fn load_page(
    connection: &rusqlite::Connection,
    snapshot_id: u64,
    after_ordinal: u64,
    limit: usize,
) -> Result<WorkerFleetInventoryPage, StorageError> {
    let snapshot_revision = connection
        .query_row(
            "SELECT scope_revision FROM worker_fleet_inventory_snapshots
             WHERE snapshot_id = ?1",
            params![to_i64(snapshot_id, "Fleet snapshot sequence")?],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(StorageError::event_cursor_expired)
        .and_then(|value| stored_u64(value, "Fleet scope revision"))?;
    let mut statement = connection
        .prepare(
            "SELECT ordinal, item_json
             FROM worker_fleet_inventory_snapshot_items
             WHERE snapshot_id = ?1 AND ordinal > ?2
             ORDER BY ordinal LIMIT ?3",
        )
        .map_err(|error| sql_error(&error))?;
    let stored_rows = statement
        .query_map(
            params![
                to_i64(snapshot_id, "Fleet snapshot sequence")?,
                to_i64(after_ordinal, "Fleet snapshot ordinal")?,
                i64::try_from(limit + 1)
                    .map_err(|_| StorageError::adapter("Fleet page limit is invalid"))?,
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    let rows = stored_rows
        .into_iter()
        .map(|(ordinal, item)| {
            stored_u64(ordinal, "Fleet snapshot ordinal").map(|ordinal| (ordinal, item))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = rows.len() > limit;
    let visible = rows.into_iter().take(limit).collect::<Vec<_>>();
    let next_cursor = if has_more {
        visible
            .last()
            .map(|(ordinal, _)| WorkerFleetSnapshotCursor {
                snapshot_id,
                after_ordinal: *ordinal,
            })
    } else {
        None
    };
    let items = visible
        .into_iter()
        .map(|(_, item_json)| decode_json(&item_json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkerFleetInventoryPage {
        snapshot_revision,
        items,
        next_cursor,
    })
}

fn retain_recent_snapshots(
    transaction: &Transaction<'_>,
    scope_json: &str,
) -> Result<(), StorageError> {
    transaction
        .execute(
            "DELETE FROM worker_fleet_inventory_snapshots
             WHERE scope_json = ?1 AND snapshot_id NOT IN (
                 SELECT snapshot_id FROM worker_fleet_inventory_snapshots
                 WHERE scope_json = ?1 ORDER BY snapshot_id DESC LIMIT ?2
             )",
            params![
                scope_json,
                i64::try_from(RETAINED_SNAPSHOTS_PER_SCOPE)
                    .map_err(|_| StorageError::adapter("Fleet retention limit is invalid"))?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn validate_sqlite_instant(
    transaction: &Transaction<'_>,
    instant: &Instant,
) -> Result<(), StorageError> {
    let valid = transaction
        .query_row(
            "SELECT julianday(?1) IS NOT NULL",
            params![instant.0],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| sql_error(&error))?;
    if valid {
        Ok(())
    } else {
        Err(StorageError::invalid_input(
            "Fleet observation time is invalid",
        ))
    }
}

fn validate_scope(scope: &WorkerRegistryScope) -> Result<(), StorageError> {
    match scope {
        WorkerRegistryScope::Organization { organization_id } => {
            validate_id(&organization_id.0, "org_", "organizationId")
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "workspaceId")
        }
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "workspaceId")?;
            validate_id(&project_id.0, "prj_", "projectId")
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "workspaceId")?;
            validate_id(&project_id.0, "prj_", "projectId")?;
            validate_id(&repository_id.0, "rep_", "repositoryId")
        }
    }
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(StorageError::invalid_input(format!(
            "Fleet {field} is invalid"
        )))
    }
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|error| StorageError::adapter(format!("failed to encode Fleet data: {error}")))
}

fn decode_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value)
        .map_err(|error| StorageError::adapter(format!("failed to decode Fleet data: {error}")))
}

fn to_i64(value: u64, field: &str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::invalid_input(format!("{field} is too large")))
}

fn stored_u64(value: i64, field: &str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::adapter(format!("stored {field} is invalid")))
}

fn sql_error(error: &rusqlite::Error) -> StorageError {
    StorageError::adapter(format!("Fleet inventory SQLite operation failed: {error}"))
}
