// SPDX-License-Identifier: Apache-2.0

//! Durable Fleet rollout intent and disconnected-Worker fencing.
//!
//! This module deliberately does not own Worker identity, health, capacity,
//! placement, or leases. Rollout records contain only operator intent and a
//! fixed observation digest. Failure fencing updates the canonical
//! [`crate::ExecutionRegistry`] lease rows in the same `SQLite` transaction and
//! leaves the normal placement adapter to select the replacement Worker.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionJobId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest, WorkerId,
    WorkerInstanceId,
};

use crate::{
    AuthenticatedWorkerPlacement, ExecutionRegistry, SqliteStorage, StorageError, WorkerHealth,
    WorkerPoolId, WorkerRegistryScope,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_POOL_CAPACITY: u64 = 100_000;
const MAX_ROLLOUT_MEMBERS: usize = 10_000;
const MAX_FENCED_LEASES: usize = 1_024;

const FLEET_OPERATIONS_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS worker_fleet_rollout_plans (
    scope_json TEXT NOT NULL,
    worker_pool_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    record_digest TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY (scope_json, worker_pool_id)
);
CREATE TABLE IF NOT EXISTS worker_fleet_rollout_receipts (
    scope_json TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_json, request_id)
);
CREATE TABLE IF NOT EXISTS worker_fleet_failure_fences (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    worker_pool_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id),
    UNIQUE (scope_json, request_id)
);
";

/// Monotonic deployment release used by one Fleet controller.
///
/// The release number is not a second Worker identity. A production Fleet
/// adapter must derive it from its authenticated deployment observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorkerFleetReleaseVersion(pub u64);

/// Health fact from one fixed Fleet-controller observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFleetMemberHealth {
    Ready,
    Draining,
    Degraded,
    Offline,
}

/// One member in a fixed, authority-sealed Fleet observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetMemberObservation {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub release_version: WorkerFleetReleaseVersion,
    pub health: WorkerFleetMemberHealth,
    pub active_leases: u64,
}

/// Exact Fleet cut consumed by the deterministic rollout reconciler.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetObservation {
    pub observed_at: Instant,
    pub current_capacity: u64,
    pub members: Vec<WorkerFleetMemberObservation>,
    pub source_digest: Sha256Digest,
}

impl WorkerFleetObservation {
    /// Seals canonical observation bytes for a production adapter or fake
    /// Registry used by the enterprise Fleet module.
    ///
    /// # Errors
    ///
    /// Rejects malformed, duplicate, or unbounded member facts.
    pub fn seal(
        observed_at: Instant,
        current_capacity: u64,
        members: Vec<WorkerFleetMemberObservation>,
    ) -> Result<Self, StorageError> {
        validate_observation_fields(&observed_at, current_capacity, &members)?;
        let source_digest = observation_digest(&observed_at, current_capacity, &members)?;
        Ok(Self {
            observed_at,
            current_capacity,
            members,
            source_digest,
        })
    }
}

/// Operator policy supplied on every compare-and-swap rollout command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetRolloutPolicy {
    pub stable_version: WorkerFleetReleaseVersion,
    pub target_version: WorkerFleetReleaseVersion,
    pub minimum_version: WorkerFleetReleaseVersion,
    pub canary_size: u64,
    pub max_unavailable: u64,
    pub desired_capacity: u64,
}

/// Current durable rollout phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFleetRolloutPhase {
    Stable,
    Canary,
    Rolling,
    RollingBack,
}

/// Worker process awaiting one exact drain-and-replace intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetPendingReplacement {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub target_version: WorkerFleetReleaseVersion,
}

/// Durable rollout state. Identity/capacity values remain observation data;
/// this record owns only desired policy and outstanding idempotent intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetRolloutRecord {
    pub scope: WorkerRegistryScope,
    pub worker_pool_id: WorkerPoolId,
    pub revision: u64,
    pub phase: WorkerFleetRolloutPhase,
    pub policy: WorkerFleetRolloutPolicy,
    pub canary_workers: Vec<WorkerId>,
    pub pending_replacements: Vec<WorkerFleetPendingReplacement>,
    pub pending_capacity: Option<u64>,
    pub observation_digest: Sha256Digest,
    pub updated_at: Instant,
}

/// Exact compare-and-swap command for a Fleet rollout reconciliation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerFleetRolloutCommand {
    pub request_id: RequestId,
    pub scope: WorkerRegistryScope,
    pub worker_pool_id: WorkerPoolId,
    pub expected_revision: u64,
    pub policy: WorkerFleetRolloutPolicy,
    pub observation: WorkerFleetObservation,
}

/// Idempotent intent produced for the infrastructure Fleet controller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum WorkerFleetAction {
    SetPoolCapacity {
        action_id: String,
        desired_capacity: u64,
    },
    DrainAndReplace {
        action_id: String,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        target_version: WorkerFleetReleaseVersion,
    },
}

/// Durable response for one rollout command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetRolloutReceipt {
    pub record: WorkerFleetRolloutRecord,
    pub actions: Vec<WorkerFleetAction>,
    pub replayed: bool,
}

/// Exact disconnected-process command that fences all of its current leases.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerFleetFailureCommand {
    pub request_id: RequestId,
    pub scope: WorkerRegistryScope,
    pub worker_pool_id: WorkerPoolId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub detected_at: Instant,
}

/// One canonical Registry lease shortened by a disconnected-Worker fence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetFencedLease {
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub fencing_token: FencingToken,
    pub next_fencing_token: FencingToken,
    pub next_attempt: u64,
    pub prior_expires_at: Instant,
    pub fenced_at: Instant,
}

/// Durable fence proof consumed by the existing placement/claim adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFleetFailureReceipt {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub fenced_leases: Vec<WorkerFleetFencedLease>,
    pub replayed: bool,
}

/// `SQLite` adapter for rollout intent and failure fencing over one Registry.
pub struct WorkerFleetOperations<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens Fleet operations beside the canonical Registry tables.
    ///
    /// # Errors
    ///
    /// Returns a storage error when either schema cannot be prepared.
    pub fn worker_fleet_operations(&mut self) -> Result<WorkerFleetOperations<'_>, StorageError> {
        WorkerFleetOperations::new(self)
    }
}

impl<'storage> WorkerFleetOperations<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, StorageError> {
        {
            let _registry = ExecutionRegistry::new(storage)?;
        }
        storage
            .connection()?
            .execute_batch(FLEET_OPERATIONS_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        Ok(Self { storage })
    }

    /// Reconciles one fixed observation into deterministic rollout intents.
    ///
    /// Exact retries return the stored action ids. Changed request reuse,
    /// stale revisions, malformed observations, and corrupt durable bytes fail
    /// closed.
    ///
    /// # Errors
    ///
    /// Returns an input, request, revision, or adapter error as described
    /// above.
    pub fn reconcile_rollout(
        &mut self,
        command: &WorkerFleetRolloutCommand,
    ) -> Result<WorkerFleetRolloutReceipt, StorageError> {
        validate_rollout_command(command)?;
        let scope_json = encode_json(&command.scope)?;
        let request_digest = digest(command)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))?;
        if let Some(receipt) =
            load_rollout_replay(&transaction, command, &scope_json, &request_digest)?
        {
            transaction.commit().map_err(|error| sql_error(&error))?;
            return Ok(receipt);
        }
        let current = load_rollout_record(&transaction, &scope_json, &command.worker_pool_id)?;
        let actual_revision = current.as_ref().map_or(0, |record| record.revision);
        if command.expected_revision != actual_revision {
            return Err(StorageError::revision_conflict(
                command.expected_revision,
                actual_revision,
            ));
        }
        validate_policy_transition(current.as_ref(), &command.policy)?;
        let (record, actions) = reconcile_record(current, command)?;
        persist_rollout_record(&transaction, &scope_json, &record)?;
        let receipt = WorkerFleetRolloutReceipt {
            record,
            actions,
            replayed: false,
        };
        transaction
            .execute(
                "INSERT INTO worker_fleet_rollout_receipts
                    (scope_json, request_id, request_digest, response_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    scope_json,
                    command.request_id.0,
                    request_digest.0,
                    encode_json(&receipt)?,
                ],
            )
            .map_err(|error| sql_error(&error))?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(receipt)
    }

    /// Loads one current rollout plan from an exact scope and pool.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities and corrupt stored bytes.
    pub fn load_rollout(
        &self,
        scope: &WorkerRegistryScope,
        worker_pool_id: &WorkerPoolId,
    ) -> Result<Option<WorkerFleetRolloutRecord>, StorageError> {
        validate_scope(scope)?;
        validate_id(&worker_pool_id.0, "wpl_", "workerPoolId")?;
        load_rollout_record(
            self.storage.connection()?,
            &encode_json(scope)?,
            worker_pool_id,
        )
    }

    /// Fences every current lease of one exact disconnected Worker process.
    ///
    /// The canonical lease expiry is shortened in the same transaction as the
    /// immutable fence receipt. This makes late renewal/results fail against
    /// the Registry and allows the existing placement adapter to claim the
    /// next attempt with the returned higher fencing token.
    ///
    /// # Errors
    ///
    /// Rejects a live, stale, foreign, changed, or unbounded Worker/lease cut.
    pub fn fence_disconnected_worker(
        &mut self,
        command: &WorkerFleetFailureCommand,
    ) -> Result<WorkerFleetFailureReceipt, StorageError> {
        validate_failure_command(command)?;
        let scope_json = encode_json(&command.scope)?;
        let request_digest = digest(command)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))?;
        if let Some(receipt) =
            load_failure_replay(&transaction, command, &scope_json, &request_digest)?
        {
            transaction.commit().map_err(|error| sql_error(&error))?;
            return Ok(receipt);
        }
        require_disconnected_placement(&transaction, command, &scope_json)?;
        let fenced_leases = fence_worker_leases(&transaction, command)?;
        let receipt = WorkerFleetFailureReceipt {
            worker_id: command.worker_id.clone(),
            worker_instance_id: command.worker_instance_id.clone(),
            fenced_leases,
            replayed: false,
        };
        transaction
            .execute(
                "INSERT INTO worker_fleet_failure_fences
                    (worker_id, worker_instance_id, scope_json, worker_pool_id,
                     request_id, request_digest, response_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    command.worker_id.0,
                    command.worker_instance_id.0,
                    scope_json,
                    command.worker_pool_id.0,
                    command.request_id.0,
                    request_digest.0,
                    encode_json(&receipt)?,
                ],
            )
            .map_err(|error| sql_error(&error))?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(receipt)
    }
}

fn reconcile_record(
    current: Option<WorkerFleetRolloutRecord>,
    command: &WorkerFleetRolloutCommand,
) -> Result<(WorkerFleetRolloutRecord, Vec<WorkerFleetAction>), StorageError> {
    let revision = command
        .expected_revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| StorageError::invalid_input("Fleet rollout revision overflowed"))?;
    let mut record = current.unwrap_or_else(|| WorkerFleetRolloutRecord {
        scope: command.scope.clone(),
        worker_pool_id: command.worker_pool_id.clone(),
        revision: 0,
        phase: if command.policy.stable_version == command.policy.target_version {
            WorkerFleetRolloutPhase::Stable
        } else {
            WorkerFleetRolloutPhase::Canary
        },
        policy: command.policy.clone(),
        canary_workers: Vec::new(),
        pending_replacements: Vec::new(),
        pending_capacity: None,
        observation_digest: command.observation.source_digest.clone(),
        updated_at: command.observation.observed_at.clone(),
    });
    record.revision = revision;
    record.policy = command.policy.clone();
    record.updated_at = command.observation.observed_at.clone();
    record.observation_digest = command.observation.source_digest.clone();
    clear_satisfied_intents(&mut record, &command.observation);

    let mut actions = Vec::new();
    reconcile_capacity(&mut record, &command.observation, &mut actions)?;
    reconcile_release(&mut record, &command.observation, &mut actions)?;
    Ok((record, actions))
}

fn reconcile_capacity(
    record: &mut WorkerFleetRolloutRecord,
    observation: &WorkerFleetObservation,
    actions: &mut Vec<WorkerFleetAction>,
) -> Result<(), StorageError> {
    if observation.current_capacity == record.policy.desired_capacity {
        record.pending_capacity = None;
    } else if record.pending_capacity != Some(record.policy.desired_capacity) {
        record.pending_capacity = Some(record.policy.desired_capacity);
        actions.push(WorkerFleetAction::SetPoolCapacity {
            action_id: action_id(record, "scale", None)?,
            desired_capacity: record.policy.desired_capacity,
        });
    }
    Ok(())
}

fn reconcile_release(
    record: &mut WorkerFleetRolloutRecord,
    observation: &WorkerFleetObservation,
    actions: &mut Vec<WorkerFleetAction>,
) -> Result<(), StorageError> {
    let members = member_map(observation);
    let target_failed = observation.members.iter().any(|member| {
        member.release_version == record.policy.target_version
            && matches!(
                member.health,
                WorkerFleetMemberHealth::Degraded | WorkerFleetMemberHealth::Offline
            )
    });
    if record.policy.target_version != record.policy.stable_version && target_failed {
        let stable_version = record.policy.stable_version;
        record.phase = WorkerFleetRolloutPhase::RollingBack;
        record
            .pending_replacements
            .retain(|pending| pending.target_version == stable_version);
        let candidates = rollback_candidates(record, observation, stable_version);
        enqueue_replacements(record, candidates, stable_version, actions)?;
        return Ok(());
    }

    if record.phase == WorkerFleetRolloutPhase::RollingBack {
        let stable_version = record.policy.stable_version;
        if all_ready_at(observation, stable_version) {
            record.phase = WorkerFleetRolloutPhase::Stable;
            record.policy.target_version = record.policy.stable_version;
            record.canary_workers.clear();
        } else {
            let candidates = rollback_candidates(record, observation, stable_version);
            enqueue_replacements(record, candidates, stable_version, actions)?;
        }
        return Ok(());
    }

    if record.policy.target_version == record.policy.stable_version {
        record.phase = WorkerFleetRolloutPhase::Stable;
        enforce_minimum_version(record, observation, actions)?;
        return Ok(());
    }

    if record.phase == WorkerFleetRolloutPhase::Canary {
        if record.canary_workers.is_empty() {
            let candidates = selectable_members(record, observation)
                .into_iter()
                .take(usize::try_from(record.policy.canary_size).unwrap_or(usize::MAX))
                .collect::<Vec<_>>();
            record.canary_workers = candidates
                .iter()
                .map(|member| member.worker_id.clone())
                .collect();
            enqueue_replacements(record, candidates, record.policy.target_version, actions)?;
            return Ok(());
        }
        let canary_ready = record.canary_workers.iter().all(|worker_id| {
            members.get(worker_id).is_some_and(|member| {
                member.release_version == record.policy.target_version
                    && member.health == WorkerFleetMemberHealth::Ready
                    && member.active_leases == 0
            })
        });
        if !canary_ready || !record.pending_replacements.is_empty() {
            return Ok(());
        }
        record.phase = WorkerFleetRolloutPhase::Rolling;
    }

    if all_ready_at(observation, record.policy.target_version) {
        record.phase = WorkerFleetRolloutPhase::Stable;
        record.policy.stable_version = record.policy.target_version;
        record.canary_workers.clear();
        return Ok(());
    }
    let candidates = selectable_members(record, observation);
    enqueue_replacements(record, candidates, record.policy.target_version, actions)
}

fn enforce_minimum_version(
    record: &mut WorkerFleetRolloutRecord,
    observation: &WorkerFleetObservation,
    actions: &mut Vec<WorkerFleetAction>,
) -> Result<(), StorageError> {
    let candidates = selectable_members(record, observation)
        .into_iter()
        .filter(|member| member.release_version < record.policy.minimum_version)
        .collect::<Vec<_>>();
    enqueue_replacements(record, candidates, record.policy.stable_version, actions)
}

fn selectable_members<'a>(
    record: &WorkerFleetRolloutRecord,
    observation: &'a WorkerFleetObservation,
) -> Vec<&'a WorkerFleetMemberObservation> {
    let pending = record
        .pending_replacements
        .iter()
        .map(|replacement| replacement.worker_id.0.as_str())
        .collect::<HashSet<_>>();
    let unavailable = observation
        .members
        .iter()
        .filter(|member| member.health != WorkerFleetMemberHealth::Ready)
        .count() as u64;
    let budget = record.policy.max_unavailable.saturating_sub(unavailable);
    let mut candidates = observation
        .members
        .iter()
        .filter(|member| {
            member.health == WorkerFleetMemberHealth::Ready
                && member.active_leases == 0
                && !pending.contains(member.worker_id.0.as_str())
                && member.release_version != record.policy.target_version
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.worker_id.0.cmp(&right.worker_id.0));
    candidates.truncate(usize::try_from(budget).unwrap_or(usize::MAX));
    candidates
}

fn rollback_candidates<'a>(
    record: &WorkerFleetRolloutRecord,
    observation: &'a WorkerFleetObservation,
    stable_version: WorkerFleetReleaseVersion,
) -> Vec<&'a WorkerFleetMemberObservation> {
    let pending = record
        .pending_replacements
        .iter()
        .map(|replacement| replacement.worker_id.0.as_str())
        .collect::<HashSet<_>>();
    let mut candidates = observation
        .members
        .iter()
        .filter(|member| {
            member.release_version != stable_version
                && !pending.contains(member.worker_id.0.as_str())
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        let left_ready = left.health == WorkerFleetMemberHealth::Ready;
        let right_ready = right.health == WorkerFleetMemberHealth::Ready;
        left_ready
            .cmp(&right_ready)
            .then_with(|| left.worker_id.0.cmp(&right.worker_id.0))
    });
    candidates.truncate(usize::try_from(record.policy.max_unavailable).unwrap_or(usize::MAX));
    candidates
}

fn enqueue_replacements<'a>(
    record: &mut WorkerFleetRolloutRecord,
    members: impl IntoIterator<Item = &'a WorkerFleetMemberObservation>,
    target_version: WorkerFleetReleaseVersion,
    actions: &mut Vec<WorkerFleetAction>,
) -> Result<(), StorageError> {
    for member in members {
        if record
            .pending_replacements
            .iter()
            .any(|pending| pending.worker_id == member.worker_id)
        {
            continue;
        }
        let pending = WorkerFleetPendingReplacement {
            worker_id: member.worker_id.clone(),
            worker_instance_id: member.worker_instance_id.clone(),
            target_version,
        };
        actions.push(WorkerFleetAction::DrainAndReplace {
            action_id: action_id(record, "replace", Some(&pending))?,
            worker_id: pending.worker_id.clone(),
            worker_instance_id: pending.worker_instance_id.clone(),
            target_version,
        });
        record.pending_replacements.push(pending);
    }
    Ok(())
}

fn clear_satisfied_intents(
    record: &mut WorkerFleetRolloutRecord,
    observation: &WorkerFleetObservation,
) {
    record.pending_replacements.retain(|pending| {
        !observation.members.iter().any(|member| {
            member.worker_id == pending.worker_id
                && member.release_version == pending.target_version
                && member.health == WorkerFleetMemberHealth::Ready
                && member.active_leases == 0
        })
    });
}

fn member_map(
    observation: &WorkerFleetObservation,
) -> HashMap<&WorkerId, &WorkerFleetMemberObservation> {
    observation
        .members
        .iter()
        .map(|member| (&member.worker_id, member))
        .collect()
}

fn all_ready_at(observation: &WorkerFleetObservation, version: WorkerFleetReleaseVersion) -> bool {
    !observation.members.is_empty()
        && observation.members.iter().all(|member| {
            member.release_version == version && member.health == WorkerFleetMemberHealth::Ready
        })
}

fn action_id(
    record: &WorkerFleetRolloutRecord,
    kind: &str,
    pending: Option<&WorkerFleetPendingReplacement>,
) -> Result<String, StorageError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ActionIdentity<'a> {
        scope: &'a WorkerRegistryScope,
        worker_pool_id: &'a WorkerPoolId,
        revision: u64,
        kind: &'a str,
        pending: Option<&'a WorkerFleetPendingReplacement>,
    }
    let bytes = serde_json::to_vec(&ActionIdentity {
        scope: &record.scope,
        worker_pool_id: &record.worker_pool_id,
        revision: record.revision,
        kind,
        pending,
    })
    .map_err(|error| StorageError::adapter(format!("failed to encode Fleet action: {error}")))?;
    Ok(format!("fop_{}", hex_digest(&bytes)))
}

fn validate_rollout_command(command: &WorkerFleetRolloutCommand) -> Result<(), StorageError> {
    validate_id(&command.request_id.0, "req_", "requestId")?;
    validate_scope(&command.scope)?;
    validate_id(&command.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_policy(&command.policy)?;
    validate_observation(&command.observation)
}

fn validate_policy(policy: &WorkerFleetRolloutPolicy) -> Result<(), StorageError> {
    for version in [
        policy.stable_version,
        policy.target_version,
        policy.minimum_version,
    ] {
        if version.0 == 0 || version.0 > MAX_SAFE_INTEGER {
            return Err(StorageError::invalid_input(
                "Fleet release version is outside the supported range",
            ));
        }
    }
    if policy.minimum_version > policy.stable_version
        || policy.stable_version > policy.target_version
        || policy.canary_size == 0
        || policy.max_unavailable == 0
        || policy.canary_size > policy.max_unavailable
        || policy.max_unavailable > policy.desired_capacity
        || policy.desired_capacity == 0
        || policy.desired_capacity > MAX_POOL_CAPACITY
    {
        return Err(StorageError::invalid_input(
            "Fleet rollout policy is inconsistent",
        ));
    }
    Ok(())
}

fn validate_policy_transition(
    current: Option<&WorkerFleetRolloutRecord>,
    policy: &WorkerFleetRolloutPolicy,
) -> Result<(), StorageError> {
    if let Some(current) = current
        && current.phase != WorkerFleetRolloutPhase::Stable
        && current.policy != *policy
    {
        return Err(StorageError::invalid_input(
            "an active Fleet rollout cannot change policy",
        ));
    }
    if let Some(current) = current
        && current.phase == WorkerFleetRolloutPhase::Stable
        && policy.stable_version != current.policy.stable_version
    {
        return Err(StorageError::invalid_input(
            "a new Fleet rollout must start from the durable stable version",
        ));
    }
    Ok(())
}

fn validate_observation(observation: &WorkerFleetObservation) -> Result<(), StorageError> {
    validate_observation_fields(
        &observation.observed_at,
        observation.current_capacity,
        &observation.members,
    )?;
    if observation.source_digest
        != observation_digest(
            &observation.observed_at,
            observation.current_capacity,
            &observation.members,
        )?
    {
        return Err(StorageError::invalid_input(
            "Fleet observation digest does not match its canonical facts",
        ));
    }
    Ok(())
}

fn validate_observation_fields(
    observed_at: &Instant,
    current_capacity: u64,
    members: &[WorkerFleetMemberObservation],
) -> Result<(), StorageError> {
    validate_instant(observed_at, "observedAt")?;
    if current_capacity > MAX_POOL_CAPACITY || members.len() > MAX_ROLLOUT_MEMBERS {
        return Err(StorageError::invalid_input(
            "Fleet observation exceeds its supported bound",
        ));
    }
    let mut worker_ids = HashSet::with_capacity(members.len());
    let mut worker_instances = HashSet::with_capacity(members.len());
    for member in members {
        validate_id(&member.worker_id.0, "wrk_", "workerId")?;
        validate_id(&member.worker_instance_id.0, "wki_", "workerInstanceId")?;
        if member.release_version.0 == 0
            || member.release_version.0 > MAX_SAFE_INTEGER
            || member.active_leases > MAX_SAFE_INTEGER
            || !worker_ids.insert(member.worker_id.0.as_str())
            || !worker_instances.insert(member.worker_instance_id.0.as_str())
        {
            return Err(StorageError::invalid_input(
                "Fleet observation contains invalid or duplicate members",
            ));
        }
    }
    Ok(())
}

fn validate_failure_command(command: &WorkerFleetFailureCommand) -> Result<(), StorageError> {
    validate_id(&command.request_id.0, "req_", "requestId")?;
    validate_scope(&command.scope)?;
    validate_id(&command.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_id(&command.worker_id.0, "wrk_", "workerId")?;
    validate_id(&command.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_instant(&command.detected_at, "detectedAt")
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

fn validate_instant(value: &Instant, field: &str) -> Result<(), StorageError> {
    let bytes = value.0.as_bytes();
    let valid = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(StorageError::invalid_input(format!(
            "Fleet {field} is invalid"
        )))
    }
}

fn validate_digest(value: &Sha256Digest, field: &str) -> Result<(), StorageError> {
    let valid = value.0.strip_prefix("sha256:").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(StorageError::adapter(format!(
            "stored Fleet {field} is invalid"
        )))
    }
}

fn observation_digest(
    observed_at: &Instant,
    current_capacity: u64,
    members: &[WorkerFleetMemberObservation],
) -> Result<Sha256Digest, StorageError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Facts<'a> {
        observed_at: &'a Instant,
        current_capacity: u64,
        members: &'a [WorkerFleetMemberObservation],
    }
    digest(&Facts {
        observed_at,
        current_capacity,
        members,
    })
}

fn digest<T: Serialize + ?Sized>(value: &T) -> Result<Sha256Digest, StorageError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| StorageError::adapter(format!("failed to encode Fleet fact: {error}")))?;
    Ok(Sha256Digest(format!("sha256:{}", hex_digest(&bytes))))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut result, byte| {
            write!(result, "{byte:02x}").expect("writing to a String cannot fail");
            result
        },
    )
}

fn encode_json<T: Serialize + ?Sized>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|error| StorageError::adapter(format!("failed to encode Fleet state: {error}")))
}

fn decode_canonical<T>(value: &str, name: &str) -> Result<T, StorageError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let parsed = serde_json::from_str::<T>(value)
        .map_err(|_| StorageError::adapter(format!("stored {name} is corrupt")))?;
    if encode_json(&parsed)? != value {
        return Err(StorageError::adapter(format!(
            "stored {name} is not canonical"
        )));
    }
    Ok(parsed)
}

fn persist_rollout_record(
    transaction: &Transaction<'_>,
    scope_json: &str,
    record: &WorkerFleetRolloutRecord,
) -> Result<(), StorageError> {
    let record_json = encode_json(record)?;
    let record_digest = digest(record)?;
    transaction
        .execute(
            "INSERT INTO worker_fleet_rollout_plans
                (scope_json, worker_pool_id, revision, record_digest, record_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_json, worker_pool_id) DO UPDATE SET
                revision = excluded.revision,
                record_digest = excluded.record_digest,
                record_json = excluded.record_json",
            params![
                scope_json,
                record.worker_pool_id.0,
                to_i64(record.revision, "rollout revision")?,
                record_digest.0,
                record_json,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn load_rollout_record(
    connection: &rusqlite::Connection,
    scope_json: &str,
    worker_pool_id: &WorkerPoolId,
) -> Result<Option<WorkerFleetRolloutRecord>, StorageError> {
    let stored = connection
        .query_row(
            "SELECT revision, record_digest, record_json
             FROM worker_fleet_rollout_plans
             WHERE scope_json = ?1 AND worker_pool_id = ?2",
            params![scope_json, worker_pool_id.0],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    let Some((revision, stored_digest, record_json)) = stored else {
        return Ok(None);
    };
    let record = decode_canonical::<WorkerFleetRolloutRecord>(&record_json, "Fleet rollout")?;
    if record.revision != stored_u64(revision, "rollout revision")?
        || record.scope != decode_canonical::<WorkerRegistryScope>(scope_json, "Fleet scope")?
        || record.worker_pool_id != *worker_pool_id
        || digest(&record)?.0 != stored_digest
    {
        return Err(StorageError::adapter(
            "stored Fleet rollout columns differ from the canonical record",
        ));
    }
    validate_stored_rollout_record(&record)?;
    Ok(Some(record))
}

fn load_rollout_replay(
    transaction: &Transaction<'_>,
    command: &WorkerFleetRolloutCommand,
    scope_json: &str,
    request_digest: &Sha256Digest,
) -> Result<Option<WorkerFleetRolloutReceipt>, StorageError> {
    let stored = transaction
        .query_row(
            "SELECT request_digest, response_json
             FROM worker_fleet_rollout_receipts
             WHERE scope_json = ?1 AND request_id = ?2",
            params![scope_json, command.request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    let Some((stored_digest, response_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != request_digest.0 {
        return Err(StorageError::request_conflict(&command.request_id));
    }
    let mut receipt =
        decode_canonical::<WorkerFleetRolloutReceipt>(&response_json, "Fleet rollout receipt")?;
    validate_rollout_receipt(&receipt, command)?;
    receipt.replayed = true;
    Ok(Some(receipt))
}

fn validate_stored_rollout_record(record: &WorkerFleetRolloutRecord) -> Result<(), StorageError> {
    validate_scope(&record.scope)?;
    validate_id(&record.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_policy(&record.policy)?;
    validate_instant(&record.updated_at, "updatedAt")?;
    validate_digest(&record.observation_digest, "observationDigest")?;
    if record.revision == 0 || record.revision > MAX_SAFE_INTEGER {
        return Err(StorageError::adapter(
            "stored Fleet rollout revision is invalid",
        ));
    }
    let mut canaries = HashSet::with_capacity(record.canary_workers.len());
    for worker_id in &record.canary_workers {
        validate_id(&worker_id.0, "wrk_", "canary.workerId")?;
        if !canaries.insert(worker_id.0.as_str()) {
            return Err(StorageError::adapter(
                "stored Fleet rollout has duplicate canary Workers",
            ));
        }
    }
    let mut pending_workers = HashSet::with_capacity(record.pending_replacements.len());
    for pending in &record.pending_replacements {
        validate_id(&pending.worker_id.0, "wrk_", "pending.workerId")?;
        validate_id(
            &pending.worker_instance_id.0,
            "wki_",
            "pending.workerInstanceId",
        )?;
        if pending.target_version.0 == 0
            || pending.target_version.0 > MAX_SAFE_INTEGER
            || !pending_workers.insert(pending.worker_id.0.as_str())
        {
            return Err(StorageError::adapter(
                "stored Fleet rollout replacement is invalid",
            ));
        }
    }
    if record
        .pending_capacity
        .is_some_and(|capacity| capacity == 0 || capacity > MAX_POOL_CAPACITY)
    {
        return Err(StorageError::adapter(
            "stored Fleet rollout capacity intent is invalid",
        ));
    }
    Ok(())
}

fn validate_rollout_receipt(
    receipt: &WorkerFleetRolloutReceipt,
    command: &WorkerFleetRolloutCommand,
) -> Result<(), StorageError> {
    validate_stored_rollout_record(&receipt.record)?;
    if receipt.replayed
        || receipt.record.scope != command.scope
        || receipt.record.worker_pool_id != command.worker_pool_id
        || receipt.record.revision != command.expected_revision.saturating_add(1)
        || receipt.record.observation_digest != command.observation.source_digest
        || receipt.record.updated_at != command.observation.observed_at
    {
        return Err(StorageError::adapter(
            "stored Fleet rollout receipt differs from its command authority",
        ));
    }
    let mut action_ids = HashSet::with_capacity(receipt.actions.len());
    for action in &receipt.actions {
        let valid = match action {
            WorkerFleetAction::SetPoolCapacity {
                action_id: stored_action_id,
                desired_capacity,
            } => {
                *desired_capacity == command.policy.desired_capacity
                    && receipt.record.pending_capacity == Some(*desired_capacity)
                    && *stored_action_id == action_id(&receipt.record, "scale", None)?
                    && action_ids.insert(stored_action_id.as_str())
            }
            WorkerFleetAction::DrainAndReplace {
                action_id: stored_action_id,
                worker_id,
                worker_instance_id,
                target_version,
            } => receipt
                .record
                .pending_replacements
                .iter()
                .find(|pending| {
                    pending.worker_id == *worker_id
                        && pending.worker_instance_id == *worker_instance_id
                        && pending.target_version == *target_version
                })
                .is_some_and(|pending| {
                    action_id(&receipt.record, "replace", Some(pending))
                        .is_ok_and(|expected| expected == *stored_action_id)
                        && action_ids.insert(stored_action_id.as_str())
                }),
        };
        if !valid {
            return Err(StorageError::adapter(
                "stored Fleet rollout action is invalid",
            ));
        }
    }
    Ok(())
}

fn require_disconnected_placement(
    transaction: &Transaction<'_>,
    command: &WorkerFleetFailureCommand,
    scope_json: &str,
) -> Result<(), StorageError> {
    let stored = transaction
        .query_row(
            "SELECT workers.health, scopes.scope_json, placements.worker_pool_id,
                    placements.record_json
             FROM execution_workers AS workers
             JOIN execution_worker_scopes AS scopes
               ON scopes.worker_id = workers.worker_id
             JOIN execution_worker_authenticated_placements AS placements
               ON placements.worker_id = workers.worker_id
              AND placements.worker_instance_id = workers.worker_instance_id
             WHERE workers.worker_id = ?1 AND workers.worker_instance_id = ?2",
            params![command.worker_id.0, command.worker_instance_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(|| StorageError::invalid_input("disconnected Worker placement is missing"))?;
    let (health, stored_scope, worker_pool_id, record_json) = stored;
    let placement = decode_canonical::<AuthenticatedWorkerPlacement>(
        &record_json,
        "authenticated Worker placement",
    )?;
    if health != WorkerHealth::TimedOut.as_str()
        || stored_scope != scope_json
        || worker_pool_id != command.worker_pool_id.0
        || placement.worker_id != command.worker_id
        || placement.worker_instance_id != command.worker_instance_id
        || placement.worker_pool_id != command.worker_pool_id
        || placement.management_scope != command.scope
    {
        return Err(StorageError::invalid_input(
            "Worker failure does not match a disconnected authenticated placement",
        ));
    }
    Ok(())
}

fn fence_worker_leases(
    transaction: &Transaction<'_>,
    command: &WorkerFleetFailureCommand,
) -> Result<Vec<WorkerFleetFencedLease>, StorageError> {
    let mut statement = transaction
        .prepare(
            "SELECT job_id, lease_id, attempt, fencing_token, expires_at
             FROM execution_leases
             WHERE worker_id = ?1 AND worker_instance_id = ?2 AND expires_at > ?3
               AND NOT EXISTS (
                   SELECT 1 FROM execution_lease_terminals AS terminals
                   WHERE terminals.lease_id = execution_leases.lease_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM execution_lease_request_receipts AS receipts
                   WHERE receipts.operation = 'dispatch_result'
                     AND receipts.job_id = execution_leases.job_id
               )
             ORDER BY job_id
             LIMIT ?4",
        )
        .map_err(|error| sql_error(&error))?;
    let rows = statement
        .query_map(
            params![
                command.worker_id.0,
                command.worker_instance_id.0,
                command.detected_at.0,
                i64::try_from(MAX_FENCED_LEASES + 1)
                    .map_err(|_| StorageError::adapter("Fleet lease bound is invalid"))?,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    drop(statement);
    if rows.len() > MAX_FENCED_LEASES {
        return Err(StorageError::invalid_input(
            "disconnected Worker has too many active leases to fence atomically",
        ));
    }
    let mut fenced = Vec::with_capacity(rows.len());
    for (job_id, lease_id, attempt, fencing_token, expires_at) in rows {
        let fenced_at = command.detected_at.clone();
        let changed = transaction
            .execute(
                "UPDATE execution_leases SET expires_at = ?1
                 WHERE job_id = ?2 AND worker_id = ?3 AND worker_instance_id = ?4
                   AND expires_at = ?5",
                params![
                    fenced_at.0,
                    job_id,
                    command.worker_id.0,
                    command.worker_instance_id.0,
                    expires_at,
                ],
            )
            .map_err(|error| sql_error(&error))?;
        if changed != 1 {
            return Err(StorageError::adapter(
                "disconnected Worker lease fence lost Registry authority",
            ));
        }
        let attempt = stored_u64(attempt, "lease attempt")?;
        fenced.push(WorkerFleetFencedLease {
            job_id: ExecutionJobId(job_id),
            lease_id: LeaseId(lease_id),
            fencing_token: FencingToken(fencing_token.clone()),
            next_fencing_token: increment_fencing_token(&fencing_token)?,
            next_attempt: attempt
                .checked_add(1)
                .filter(|value| *value <= MAX_SAFE_INTEGER)
                .ok_or_else(|| StorageError::adapter("lease attempt overflowed"))?,
            prior_expires_at: Instant(expires_at),
            fenced_at,
        });
    }
    Ok(fenced)
}

fn load_failure_replay(
    transaction: &Transaction<'_>,
    command: &WorkerFleetFailureCommand,
    scope_json: &str,
    request_digest: &Sha256Digest,
) -> Result<Option<WorkerFleetFailureReceipt>, StorageError> {
    let by_request = transaction
        .query_row(
            "SELECT worker_id, worker_instance_id, request_digest, response_json
             FROM worker_fleet_failure_fences
             WHERE scope_json = ?1 AND request_id = ?2",
            params![scope_json, command.request_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    if let Some((worker_id, worker_instance_id, stored_digest, response_json)) = by_request {
        if worker_id != command.worker_id.0
            || worker_instance_id != command.worker_instance_id.0
            || stored_digest != request_digest.0
        {
            return Err(StorageError::request_conflict(&command.request_id));
        }
        let mut receipt =
            decode_canonical::<WorkerFleetFailureReceipt>(&response_json, "Fleet failure receipt")?;
        validate_failure_receipt(&receipt, command)?;
        receipt.replayed = true;
        return Ok(Some(receipt));
    }
    if transaction
        .query_row(
            "SELECT 1 FROM worker_fleet_failure_fences
             WHERE worker_id = ?1 AND worker_instance_id = ?2",
            params![command.worker_id.0, command.worker_instance_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .is_some()
    {
        return Err(StorageError::invalid_input(
            "Worker process failure was already fenced by another command",
        ));
    }
    Ok(None)
}

fn validate_failure_receipt(
    receipt: &WorkerFleetFailureReceipt,
    command: &WorkerFleetFailureCommand,
) -> Result<(), StorageError> {
    if receipt.replayed
        || receipt.worker_id != command.worker_id
        || receipt.worker_instance_id != command.worker_instance_id
        || receipt.fenced_leases.len() > MAX_FENCED_LEASES
    {
        return Err(StorageError::adapter(
            "stored Fleet failure receipt differs from its command authority",
        ));
    }
    let mut jobs = HashSet::with_capacity(receipt.fenced_leases.len());
    let mut leases = HashSet::with_capacity(receipt.fenced_leases.len());
    for lease in &receipt.fenced_leases {
        validate_id(&lease.job_id.0, "job_", "jobId")?;
        validate_id(&lease.lease_id.0, "lse_", "leaseId")?;
        validate_fencing_token(&lease.fencing_token)?;
        validate_fencing_token(&lease.next_fencing_token)?;
        validate_instant(&lease.prior_expires_at, "priorExpiresAt")?;
        validate_instant(&lease.fenced_at, "fencedAt")?;
        if lease.next_fencing_token != increment_fencing_token(&lease.fencing_token.0)?
            || lease.next_attempt == 0
            || lease.next_attempt > MAX_SAFE_INTEGER
            || lease.fenced_at != command.detected_at
            || lease.prior_expires_at.0 <= lease.fenced_at.0
            || !jobs.insert(lease.job_id.0.as_str())
            || !leases.insert(lease.lease_id.0.as_str())
        {
            return Err(StorageError::adapter(
                "stored Fleet fenced lease is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_fencing_token(token: &FencingToken) -> Result<(), StorageError> {
    if token.0.is_empty()
        || token.0.len() > 20
        || token.0.starts_with('0')
        || !token.0.bytes().all(|byte| byte.is_ascii_digit())
    {
        Err(StorageError::adapter(
            "stored Fleet fencing token is invalid",
        ))
    } else {
        Ok(())
    }
}

fn increment_fencing_token(value: &str) -> Result<FencingToken, StorageError> {
    let number = value
        .parse::<u128>()
        .map_err(|_| StorageError::adapter("stored fencing token is invalid"))?;
    let next = number
        .checked_add(1)
        .ok_or_else(|| StorageError::adapter("fencing token overflowed"))?
        .to_string();
    if next.len() > 20 {
        return Err(StorageError::adapter("fencing token overflowed"));
    }
    Ok(FencingToken(next))
}

fn to_i64(value: u64, field: &str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::invalid_input(format!("{field} is too large")))
}

fn stored_u64(value: i64, field: &str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::adapter(format!("stored {field} is invalid")))
}

fn sql_error(error: &rusqlite::Error) -> StorageError {
    StorageError::adapter(format!("Fleet operations SQLite failure: {error}"))
}
