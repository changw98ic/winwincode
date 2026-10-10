use std::fmt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerEnvelope, ClientToServerMessage,
    CommandContext, OccupancyCommandContext,
};

use crate::candidate_branch::{
    CreatedBranchRecord, WINWINCODE_BRANCH_PREFIX, created_branch_record,
};
use crate::candidate_registry::{
    CANDIDATE_REF_PREFIX, CandidateLocalRefRecord, candidate_local_ref,
    progress_candidate_lifecycle,
};
use crate::daemon::DeviceDaemon;
use crate::store::{DeviceStore, DeviceStoreError};

const RETAINED_KIND: &str = "client.candidate.retained";

const APPLY_RESULT_KIND: &str = "client.candidate.apply_result";

const MAX_ID_BYTES: usize = 200;

const MIN_TERMINAL_RETENTION: Duration = Duration::from_mins(1);

const MAX_ACTIVE_LIMIT: u32 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateUplinkState {
    Settled,
    Unsettled,
    NeverReported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateRetentionPolicy {
    pub max_active_per_binding: u32,
    pub terminal_retention: Duration,
    pub prune_objects: bool,
}

impl Default for CandidateRetentionPolicy {
    fn default() -> Self {
        Self {
            max_active_per_binding: 8,
            terminal_retention: Duration::from_hours(720),
            prune_objects: false,
        }
    }
}

impl CandidateRetentionPolicy {
    pub fn ensure_valid(&self) -> Result<(), CandidateRetentionError> {
        if self.max_active_per_binding == 0 {
            return Err(CandidateRetentionError::invalid(
                "the retention policy must keep at least one active candidate per binding",
            ));
        }
        if self.max_active_per_binding > MAX_ACTIVE_LIMIT {
            return Err(CandidateRetentionError::invalid(format!(
                "the retention policy limit must not exceed {MAX_ACTIVE_LIMIT}"
            )));
        }
        if self.terminal_retention < MIN_TERMINAL_RETENTION {
            return Err(CandidateRetentionError::invalid(
                "the terminal retention window must be at least one minute",
            ));
        }
        time::Duration::try_from(self.terminal_retention).map_err(|_| {
            CandidateRetentionError::invalid(
                "the terminal retention window is not an encodable span",
            )
        })?;
        Ok(())
    }

    fn retention_span(&self) -> Result<time::Duration, CandidateRetentionError> {
        time::Duration::try_from(self.terminal_retention).map_err(|_| {
            CandidateRetentionError::invalid(
                "the terminal retention window is not an encodable span",
            )
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRetentionErrorKind {
    InvalidInput,
    Conflict,
    NoOccupancyMirror,
    CandidateMissing,
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRetentionError {
    kind: CandidateRetentionErrorKind,
    message: String,
}

impl CandidateRetentionError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRetentionErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRetentionErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn candidate_missing(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRetentionErrorKind::CandidateMissing,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRetentionErrorKind::Store,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> CandidateRetentionErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateRetentionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateRetentionError {}

impl From<DeviceStoreError> for CandidateRetentionError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

impl From<crate::candidate_registry::CandidateRegistryError> for CandidateRetentionError {
    fn from(error: crate::candidate_registry::CandidateRegistryError) -> Self {
        Self {
            kind: match error.kind() {
                crate::candidate_registry::CandidateRegistryErrorKind::InvalidInput => {
                    CandidateRetentionErrorKind::InvalidInput
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Conflict => {
                    CandidateRetentionErrorKind::Conflict
                }
                crate::candidate_registry::CandidateRegistryErrorKind::NoOccupancyMirror => {
                    CandidateRetentionErrorKind::NoOccupancyMirror
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Store => {
                    CandidateRetentionErrorKind::Store
                }
            },
            message: error.message().to_owned(),
        }
    }
}

impl From<crate::candidate_branch::CandidateBranchError> for CandidateRetentionError {
    fn from(error: crate::candidate_branch::CandidateBranchError) -> Self {
        Self {
            kind: match error.kind() {
                crate::candidate_branch::CandidateBranchErrorKind::InvalidInput => {
                    CandidateRetentionErrorKind::InvalidInput
                }
                crate::candidate_branch::CandidateBranchErrorKind::Conflict => {
                    CandidateRetentionErrorKind::Conflict
                }
                crate::candidate_branch::CandidateBranchErrorKind::NoOccupancyMirror => {
                    CandidateRetentionErrorKind::NoOccupancyMirror
                }
                crate::candidate_branch::CandidateBranchErrorKind::CandidateMissing => {
                    CandidateRetentionErrorKind::CandidateMissing
                }
                crate::candidate_branch::CandidateBranchErrorKind::Store => {
                    CandidateRetentionErrorKind::Store
                }
            },
            message: error.message().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardRequest {
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub requested_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardFacts {
    pub candidate_id: String,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub deleted_branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateDiscardOutcome {
    Discarded(CandidateDiscardFacts),
    Duplicate(CandidateDiscardFacts),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardReport {
    pub outcome: CandidateDiscardOutcome,
    pub frame_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardRecord {
    pub candidate_id: String,
    pub repository_binding_id: String,
    pub target_branch: String,
    pub created_at: String,
}

pub fn discard_candidate(
    daemon: &mut DeviceDaemon,
    request: &CandidateDiscardRequest,
) -> Result<CandidateDiscardReport, CandidateRetentionError> {
    validate_discard_request(request)?;
    let candidate_id = candidate_id_of(&request.candidate_ref);
    let record = candidate_local_ref(daemon.store_mut(), &candidate_id)?.ok_or_else(|| {
        CandidateRetentionError::candidate_missing(format!(
            "candidate ref {CANDIDATE_REF_PREFIX}{candidate_id} is not retained on this device"
        ))
    })?;
    if record.repository_binding_id != request.repository_binding_id {
        return Err(CandidateRetentionError::conflict(format!(
            "candidate {candidate_id} is retained for binding {} and cannot be discarded \
             on {}",
            record.repository_binding_id, request.repository_binding_id
        )));
    }
    if record.local_state == LocalCandidateState::Applied {
        return Err(CandidateRetentionError::conflict(format!(
            "candidate {candidate_id} was applied; a delivered candidate is a settled \
             fact and cannot be discarded"
        )));
    }
    if daemon.occupancy_mirror().is_none() {
        return Err(CandidateRetentionError {
            kind: CandidateRetentionErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the discard cannot be \
                      stamped with a lease"
                .to_owned(),
        });
    }

    let branch = created_branch_record(daemon.store_mut(), &candidate_id)?;
    if let Some(branch) = &branch {
        if branch.repository_binding_id != request.repository_binding_id {
            return Err(CandidateRetentionError::conflict(format!(
                "the created branch of candidate {candidate_id} belongs to binding {} \
                 and cannot be discarded on {}",
                branch.repository_binding_id, request.repository_binding_id
            )));
        }
        let repository_path =
            bound_repository_path(daemon.store_mut(), &request.repository_binding_id)?;
        delete_created_branch(&repository_path, branch, &record)?;
    }
    let deleted_branch = branch.map(|branch| branch.branch_name);

    let target_branch = deleted_branch
        .clone()
        .unwrap_or_else(|| record.local_git_ref.clone());
    let durable = write_discard_record(
        daemon.store_mut(),
        &candidate_id,
        &request.repository_binding_id,
        &target_branch,
        &request.requested_at,
    )?;

    let facts = CandidateDiscardFacts {
        candidate_id: candidate_id.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        candidate_ref: record.candidate_ref.clone(),
        deleted_branch,
    };
    let outcome = if record.local_state == LocalCandidateState::Discarded {
        CandidateDiscardOutcome::Duplicate(facts)
    } else {
        progress_candidate_lifecycle(
            daemon.store_mut(),
            &candidate_id,
            LocalCandidateState::Discarded,
        )?;
        CandidateDiscardOutcome::Discarded(facts)
    };
    let frame_sequence = enqueue_candidate_discarded(daemon, &record, &durable)?;
    Ok(CandidateDiscardReport {
        outcome,
        frame_sequence,
    })
}

fn deterministic_lar_id(candidate_id: &str, tag: char) -> String {
    let hex: String = candidate_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(25)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    format!("lar_{hex}{tag}")
}

pub fn enqueue_candidate_discarded(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
    durable: &CandidateDiscardRecord,
) -> Result<u64, CandidateRetentionError> {
    let mirror = daemon
        .occupancy_mirror()
        .ok_or_else(|| CandidateRetentionError {
            kind: CandidateRetentionErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the discard cannot be \
                      stamped with a lease"
                .to_owned(),
        })?;
    let receipt = LocalApplyReceipt {
        local_apply_receipt_id: deterministic_lar_id(&record.candidate_id, 'H'),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        target_branch: durable.target_branch.clone(),
        expected_head: record.candidate_commit.clone(),
        strategy: ApplyStrategy::CreateBranch,
        result: ApplyResult::Discarded,
        resulting_commit: None,
        conflict_artifact_ref: None,
        created_at: durable.created_at.clone(),
        revision: 1,
    };
    daemon
        .enqueue(ClientToServerMessage::CandidateApplyResult(
            ClientCandidateApplyResultPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: mirror.mirror_revision,
                        idempotency_key: format!("candidate-discarded-{}", record.candidate_ref),
                    },
                    occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                    occupancy_fencing_token: mirror.fencing_token,
                },
                receipt,
            },
        ))
        .map_err(|error| {
            CandidateRetentionError::store(format!(
                "the discard frame cannot enter the durable outbox: {error:?}"
            ))
        })
}

pub fn candidate_discard_record(
    store: &mut DeviceStore,
    candidate_id: &str,
) -> Result<Option<CandidateDiscardRecord>, CandidateRetentionError> {
    ensure_discard_schema(store)?;
    let connection = store.connection_mut()?;
    connection
        .query_row(
            "SELECT candidate_id, repository_binding_id, target_branch, created_at \
             FROM candidate_discards WHERE candidate_id = ?1",
            params![candidate_id],
            |row| {
                Ok(CandidateDiscardRecord {
                    candidate_id: row.get(0)?,
                    repository_binding_id: row.get(1)?,
                    target_branch: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(crate::store::sql_error)
        .map_err(CandidateRetentionError::from)
}

fn write_discard_record(
    store: &mut DeviceStore,
    candidate_id: &str,
    repository_binding_id: &str,
    target_branch: &str,
    requested_at: &str,
) -> Result<CandidateDiscardRecord, CandidateRetentionError> {
    ensure_discard_schema(store)?;
    {
        let connection = store.connection_mut()?;
        connection
            .execute(
                "INSERT OR IGNORE INTO candidate_discards \
                 (candidate_id, repository_binding_id, target_branch, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    candidate_id,
                    repository_binding_id,
                    target_branch,
                    requested_at
                ],
            )
            .map_err(crate::store::sql_error)?;
    }
    let stored = candidate_discard_record(store, candidate_id)?.ok_or_else(|| {
        CandidateRetentionError::store("the discard record disappeared before the read-back")
    })?;
    if stored.repository_binding_id != repository_binding_id
        || stored.target_branch != target_branch
    {
        return Err(CandidateRetentionError::conflict(format!(
            "candidate {candidate_id} is already recorded as discarded against a \
             different target"
        )));
    }
    Ok(stored)
}

fn ensure_discard_schema(store: &mut DeviceStore) -> Result<(), CandidateRetentionError> {
    store
        .connection_mut()?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS candidate_discards (
                candidate_id TEXT PRIMARY KEY NOT NULL,
                repository_binding_id TEXT NOT NULL,
                target_branch TEXT NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcDeferralReason {
    PendingUplinkAck,
    RetentionPending,
    RefDrifted,
    CheckoutUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectedCandidate {
    pub candidate_id: String,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub ref_was_present: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredCandidate {
    pub candidate_id: String,
    pub reason: GcDeferralReason,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub examined: usize,
    pub already_collected: usize,
    pub collected: Vec<CollectedCandidate>,
    pub deferred: Vec<DeferredCandidate>,
    pub pruned_bindings: Vec<String>,
}

pub fn collect_expired_candidates(
    store: &mut DeviceStore,
    policy: &CandidateRetentionPolicy,
    now: &OffsetDateTime,
) -> Result<GcReport, CandidateRetentionError> {
    policy.ensure_valid()?;
    let retention_span = policy.retention_span()?;
    let mut report = GcReport::default();
    let mut prune_bindings: Vec<String> = Vec::new();
    let collected_ids = ensure_gc_schema(store)?;
    let records = terminal_candidates(store)?;
    for record in &records {
        if collected_ids.contains(&record.candidate_id) {
            report.already_collected += 1;
            continue;
        }
        report.examined += 1;
        let created = parse_durable_stamp(&record.created_at, "candidate created at")?;
        if *now - created < retention_span {
            report.deferred.push(DeferredCandidate {
                candidate_id: record.candidate_id.clone(),
                reason: GcDeferralReason::RetentionPending,
            });
            continue;
        }
        let Some(mapping) = store.path_mapping(&record.repository_binding_id)? else {
            report.deferred.push(DeferredCandidate {
                candidate_id: record.candidate_id.clone(),
                reason: GcDeferralReason::CheckoutUnavailable,
            });
            continue;
        };
        if !Path::new(&mapping.canonical_path).exists() {
            report.deferred.push(DeferredCandidate {
                candidate_id: record.candidate_id.clone(),
                reason: GcDeferralReason::CheckoutUnavailable,
            });
            continue;
        }
        if candidate_uplink_state(store, &record.candidate_ref)? != CandidateUplinkState::Settled {
            report.deferred.push(DeferredCandidate {
                candidate_id: record.candidate_id.clone(),
                reason: GcDeferralReason::PendingUplinkAck,
            });
            continue;
        }
        let canonical_ref = format!("{CANDIDATE_REF_PREFIX}{}", record.candidate_id);
        if record.local_git_ref != canonical_ref || record.candidate_ref != canonical_ref {
            return Err(CandidateRetentionError::store(format!(
                "candidate {} carries a divergent stable ref; the collection refuses \
                 to proceed",
                record.candidate_id
            )));
        }
        let observed = read_git_ref(&mapping.canonical_path, &record.local_git_ref)?;
        let ref_was_present = match observed {
            Some(commit) if commit == record.candidate_commit => true,
            Some(_) => {
                report.deferred.push(DeferredCandidate {
                    candidate_id: record.candidate_id.clone(),
                    reason: GcDeferralReason::RefDrifted,
                });
                continue;
            }
            None => false,
        };
        if ref_was_present {
            delete_candidate_ref(&mapping.canonical_path, &record.local_git_ref)?;
        }
        record_gc_collection(store, record, now)?;
        if ref_was_present && !prune_bindings.contains(&record.repository_binding_id) {
            prune_bindings.push(record.repository_binding_id.clone());
        }
        report.collected.push(CollectedCandidate {
            candidate_id: record.candidate_id.clone(),
            repository_binding_id: record.repository_binding_id.clone(),
            candidate_ref: record.candidate_ref.clone(),
            ref_was_present,
        });
    }
    if policy.prune_objects {
        for binding in &prune_bindings {
            prune_binding_objects(store, binding)?;
            report.pruned_bindings.push(binding.clone());
        }
    }
    Ok(report)
}

fn parse_local_state_wire(value: &str) -> rusqlite::Result<LocalCandidateState> {
    match value {
        "retained" => Ok(LocalCandidateState::Retained),
        "branch_created" => Ok(LocalCandidateState::BranchCreated),
        "applied" => Ok(LocalCandidateState::Applied),
        "discarded" => Ok(LocalCandidateState::Discarded),
        "failed" => Ok(LocalCandidateState::Failed),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("stored candidate state {other} is not a plan 15 lifecycle state").into(),
        )),
    }
}

fn terminal_candidates(
    store: &DeviceStore,
) -> Result<Vec<CandidateLocalRefRecord>, CandidateRetentionError> {
    let mut statement = store
        .connection()?
        .prepare(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at \
             FROM candidate_local_refs ORDER BY candidate_id",
        )
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map([], row_to_candidate_record)
        .map_err(crate::store::sql_error)?;
    let mut records = Vec::new();
    for row in rows {
        let record = row.map_err(crate::store::sql_error)?;
        if matches!(
            record.local_state,
            LocalCandidateState::Applied | LocalCandidateState::Discarded
        ) {
            records.push(record);
        }
    }
    Ok(records)
}

fn ensure_gc_schema(
    store: &mut DeviceStore,
) -> Result<std::collections::BTreeSet<String>, CandidateRetentionError> {
    store
        .connection_mut()?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS candidate_gc_collections (
                candidate_id TEXT PRIMARY KEY NOT NULL,
                repository_binding_id TEXT NOT NULL,
                candidate_ref TEXT NOT NULL,
                collected_at TEXT NOT NULL
            );",
        )
        .map_err(crate::store::sql_error)?;
    let connection = store.connection()?;
    let mut statement = connection
        .prepare("SELECT candidate_id FROM candidate_gc_collections ORDER BY candidate_id")
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(crate::store::sql_error)?;
    let mut ids = std::collections::BTreeSet::new();
    for id in rows {
        ids.insert(id.map_err(crate::store::sql_error)?);
    }
    Ok(ids)
}

fn record_gc_collection(
    store: &mut DeviceStore,
    record: &CandidateLocalRefRecord,
    now: &OffsetDateTime,
) -> Result<(), CandidateRetentionError> {
    store
        .connection_mut()?
        .execute(
            "INSERT OR IGNORE INTO candidate_gc_collections \
             (candidate_id, repository_binding_id, candidate_ref, collected_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                record.candidate_id,
                record.repository_binding_id,
                record.candidate_ref,
                now_rfc3339(now),
            ],
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

fn candidate_uplink_state(
    store: &DeviceStore,
    candidate_ref: &str,
) -> Result<CandidateUplinkState, CandidateRetentionError> {
    let mut statement = store
        .connection()?
        .prepare(
            "SELECT kind, payload, published FROM client_outbox \
             WHERE kind IN (?1, ?2) \
             ORDER BY outbox_sequence",
        )
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map(params![RETAINED_KIND, APPLY_RESULT_KIND], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(crate::store::sql_error)?;
    let mut settled_seen = false;
    for row in rows {
        let (kind, payload, published) = row.map_err(crate::store::sql_error)?;
        if frame_candidate_ref(&kind, &payload)?.as_deref() != Some(candidate_ref) {
            continue;
        }
        if published != 1 {
            return Ok(CandidateUplinkState::Unsettled);
        }
        settled_seen = true;
    }
    if settled_seen {
        Ok(CandidateUplinkState::Settled)
    } else {
        Ok(CandidateUplinkState::NeverReported)
    }
}

fn frame_candidate_ref(
    kind: &str,
    payload: &[u8],
) -> Result<Option<String>, CandidateRetentionError> {
    let envelope: ClientToServerEnvelope = serde_json::from_slice(payload).map_err(|error| {
        CandidateRetentionError::store(format!(
            "a stored {kind} frame is not a decodable envelope: {error}"
        ))
    })?;
    match envelope.message {
        ClientToServerMessage::CandidateRetained(payload) => {
            Ok(Some(payload.receipt.candidate_ref))
        }
        ClientToServerMessage::CandidateApplyResult(payload) => {
            Ok(Some(payload.receipt.candidate_ref))
        }
        other => {
            let unrelated = serde_json::to_value(&other)
                .ok()
                .and_then(|value| {
                    value
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "unknown".to_owned());
            Err(CandidateRetentionError::store(format!(
                "a stored {kind} frame carries the unrelated message {unrelated}; the \
                 outbox row is corrupt"
            )))
        }
    }
}

pub fn enforce_retention_policy(
    daemon: &mut DeviceDaemon,
    repository_binding_id: &str,
    policy: &CandidateRetentionPolicy,
) -> Result<RetentionSweepReport, CandidateRetentionError> {
    policy.ensure_valid()?;
    if repository_binding_id.is_empty() || repository_binding_id.len() > MAX_ID_BYTES {
        return Err(CandidateRetentionError::invalid(
            "repository binding id must be 1..=200 bytes",
        ));
    }
    let active = active_candidates(daemon.store_mut(), repository_binding_id)?;
    let limit = policy.max_active_per_binding as usize;
    let active_before = active.len();
    let mut report = RetentionSweepReport {
        repository_binding_id: repository_binding_id.to_owned(),
        limit: policy.max_active_per_binding,
        active_before,
        discarded: Vec::new(),
        active_remaining: active_before,
    };
    if active.len() <= limit {
        return Ok(report);
    }
    let excess = &active[..active.len() - limit];
    for record in excess {
        let discard = discard_candidate(
            daemon,
            &CandidateDiscardRequest {
                repository_binding_id: repository_binding_id.to_owned(),
                candidate_ref: record.candidate_ref.clone(),
                requested_at: now_rfc3339(&OffsetDateTime::now_utc()),
            },
        )?;
        report.discarded.push(match discard.outcome {
            CandidateDiscardOutcome::Discarded(facts)
            | CandidateDiscardOutcome::Duplicate(facts) => facts,
        });
    }
    report.active_remaining = limit;
    Ok(report)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionSweepReport {
    pub repository_binding_id: String,
    pub limit: u32,
    pub active_before: usize,
    pub discarded: Vec<CandidateDiscardFacts>,
    pub active_remaining: usize,
}

fn active_candidates(
    store: &DeviceStore,
    repository_binding_id: &str,
) -> Result<Vec<CandidateLocalRefRecord>, CandidateRetentionError> {
    let mut statement = store
        .connection()?
        .prepare(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at \
             FROM candidate_local_refs \
             WHERE repository_binding_id = ?1 \
               AND local_state IN ('retained', 'branch_created') \
             ORDER BY created_at, candidate_id",
        )
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map(params![repository_binding_id], row_to_candidate_record)
        .map_err(crate::store::sql_error)?;
    let mut records = Vec::new();
    for row in rows {
        let record = row.map_err(crate::store::sql_error)?;
        parse_durable_stamp(&record.created_at, "candidate created at")?;
        records.push(record);
    }
    Ok(records)
}

fn row_to_candidate_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateLocalRefRecord> {
    let local_state: String = row.get(4)?;
    Ok(CandidateLocalRefRecord {
        candidate_id: row.get(0)?,
        worker_session_id: row.get(1)?,
        repository_binding_id: row.get(2)?,
        local_git_ref: row.get(3)?,
        local_state: parse_local_state_wire(&local_state)?,
        created_at: row.get(5)?,
        candidate_ref: row.get(6)?,
        candidate_commit: row.get(7)?,
        retained_at: row.get(8)?,
    })
}

fn bound_repository_path(
    store: &mut DeviceStore,
    repository_binding_id: &str,
) -> Result<String, CandidateRetentionError> {
    let mapping = store.path_mapping(repository_binding_id)?.ok_or_else(|| {
        CandidateRetentionError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps no local checkout"
        ))
    })?;
    if !Path::new(&mapping.canonical_path).exists() {
        return Err(CandidateRetentionError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps a checkout that is absent"
        )));
    }
    Ok(mapping.canonical_path)
}

fn delete_created_branch(
    repository_path: &str,
    branch: &CreatedBranchRecord,
    record: &CandidateLocalRefRecord,
) -> Result<(), CandidateRetentionError> {
    if !is_engine_branch_name(&branch.branch_name) {
        return Err(CandidateRetentionError::conflict(format!(
            "the durable branch record of candidate {} carries a name outside the \
             {} namespace",
            record.candidate_id, WINWINCODE_BRANCH_PREFIX
        )));
    }
    let full_ref = format!("refs/heads/{}", branch.branch_name);
    match read_git_ref(repository_path, &full_ref)? {
        None => Ok(()),
        Some(commit) if commit == record.candidate_commit => {
            let mut command = git_command(repository_path);
            command.args(["branch", "--delete", "--force", "--end-of-options"]);
            command.arg(&branch.branch_name);
            let output = command.output().map_err(|error| {
                CandidateRetentionError::store(format!("Git cannot be run: {error}"))
            })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                if stderr.contains("checked out") || stderr.contains("used by worktree") {
                    return Err(CandidateRetentionError::conflict(format!(
                        "the branch {} is checked out in a worktree; discard refuses \
                         to delete it",
                        branch.branch_name
                    )));
                }
                return Err(CandidateRetentionError::store(format!(
                    "Git branch cannot be deleted: {stderr}"
                )));
            }
            if read_git_ref(repository_path, &full_ref)?.is_some() {
                return Err(CandidateRetentionError::store(format!(
                    "the deleted branch {} still resolves after the deletion",
                    branch.branch_name
                )));
            }
            Ok(())
        }
        Some(_) => Err(CandidateRetentionError::conflict(format!(
            "the branch {} no longer holds the candidate commit; discard refuses to \
             delete a branch it did not freeze",
            branch.branch_name
        ))),
    }
}

fn is_engine_branch_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(WINWINCODE_BRANCH_PREFIX) else {
        return false;
    };
    !suffix.is_empty()
        && !suffix.contains("..")
        && !suffix.contains("@{")
        && !suffix.ends_with('.')
        && !suffix.ends_with('/')
        && suffix.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.starts_with('-')
                && !segment.as_bytes().ends_with(b".lock")
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn delete_candidate_ref(
    repository_path: &str,
    ref_name: &str,
) -> Result<(), CandidateRetentionError> {
    let mut command = git_command(repository_path);
    command.args(["update-ref", "-d"]);
    command.arg(ref_name);
    let output = command
        .output()
        .map_err(|error| CandidateRetentionError::store(format!("Git cannot be run: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CandidateRetentionError::store(format!(
            "Git candidate ref cannot be deleted: {}",
            stderr.trim()
        )));
    }
    if read_git_ref(repository_path, ref_name)?.is_some() {
        return Err(CandidateRetentionError::store(format!(
            "the candidate ref {ref_name} still resolves after the deletion"
        )));
    }
    Ok(())
}

fn prune_binding_objects(
    store: &mut DeviceStore,
    repository_binding_id: &str,
) -> Result<(), CandidateRetentionError> {
    let repository_path = bound_repository_path(store, repository_binding_id)?;
    let mut command = git_command(&repository_path);
    command.args(["prune", "--expire=now"]);
    let output = command
        .output()
        .map_err(|error| CandidateRetentionError::store(format!("Git cannot be run: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CandidateRetentionError::store(format!(
            "Git prune cannot run: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

fn read_git_ref(
    repository_path: &str,
    ref_name: &str,
) -> Result<Option<String>, CandidateRetentionError> {
    let mut command = git_command(repository_path);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command
        .output()
        .map_err(|error| CandidateRetentionError::store(format!("Git cannot be run: {error}")))?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateRetentionError::invalid("Git ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateRetentionError::store(
                "Git ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateRetentionError::store(format!(
        "Git ref cannot be read: {}",
        stderr.trim()
    )))
}

fn git_command(repository_path: &str) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository_path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn validate_discard_request(
    request: &CandidateDiscardRequest,
) -> Result<(), CandidateRetentionError> {
    require_non_empty(&request.repository_binding_id, "repository binding id")?;
    require_non_empty(&request.requested_at, "requested at")?;
    validate_candidate_ref(&request.candidate_ref)
}

fn candidate_id_of(candidate_ref: &str) -> String {
    candidate_ref
        .strip_prefix(CANDIDATE_REF_PREFIX)
        .unwrap_or(candidate_ref)
        .to_owned()
}

fn validate_candidate_ref(candidate_ref: &str) -> Result<(), CandidateRetentionError> {
    let Some(suffix) = candidate_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateRetentionError::invalid(format!(
            "candidate ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    let valid = (suffix.len() == 40 || suffix.len() == 64)
        && suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(CandidateRetentionError::invalid(
            "candidate ref does not name a full lowercase git commit id",
        ))
    }
}

fn require_non_empty(value: &str, label: &str) -> Result<(), CandidateRetentionError> {
    if value.is_empty() {
        return Err(CandidateRetentionError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(CandidateRetentionError::invalid(format!(
            "{label} must contain at most {MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn parse_durable_stamp(
    value: &str,
    label: &str,
) -> Result<OffsetDateTime, CandidateRetentionError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        CandidateRetentionError::store(format!("{label} is not an RFC 3339 stamp: {error}"))
    })
}

fn now_rfc3339(now: &OffsetDateTime) -> String {
    crate::canonical_rfc3339(*now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_client_port::domain::{
        ClientControlError, ClientControlMessageKind, CommandAckStatus,
    };
    use winwincode_client_port::messages::{
        ClientCandidateApplyResultPayload, ClientCommandAckPayload, OccupancyCommandContext,
    };

    const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

    fn apply_result_envelope(candidate_ref: &str) -> Vec<u8> {
        let payload = ClientCandidateApplyResultPayload {
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision: 1,
                    idempotency_key: "candidate-discarded-x".to_owned(),
                },
                occupancy_lease_id: "ocl_x".to_owned(),
                occupancy_fencing_token: 1,
            },
            receipt: LocalApplyReceipt {
                local_apply_receipt_id: "lar_discard_x".to_owned(),
                candidate_ref: candidate_ref.to_owned(),
                repository_binding_id: "rbd_x".to_owned(),
                target_branch: "winwincode/x".to_owned(),
                expected_head: COMMIT.to_owned(),
                strategy: ApplyStrategy::CreateBranch,
                result: ApplyResult::Discarded,
                resulting_commit: None,
                conflict_artifact_ref: None,
                created_at: "2026-09-04T00:00:00.000Z".to_owned(),
                revision: 1,
            },
        };
        serde_json::to_vec(&ClientToServerEnvelope {
            schema_version: "1".to_owned(),
            message_id: "msg-1".to_owned(),
            client_node_id: "cnd_x".to_owned(),
            client_instance_id: "cin_x".to_owned(),
            sequence: 1,
            occurred_at: "2026-09-04T00:00:00.000Z".to_owned(),
            message: ClientToServerMessage::CandidateApplyResult(payload),
        })
        .expect("the envelope should encode")
    }

    #[test]
    fn the_default_policy_is_conservative_and_validates() {
        let policy = CandidateRetentionPolicy::default();
        assert_eq!(
            policy.ensure_valid(),
            Ok(()),
            "the shipped defaults must pass the safety bounds"
        );
        assert!(
            policy.max_active_per_binding > 1,
            "the default keeps more than one candidate"
        );
        assert!(!policy.prune_objects, "the default never prunes objects");
    }

    #[test]
    fn policies_outside_the_safe_bounds_are_rejected() {
        for (label, policy) in [
            (
                "a zero limit",
                CandidateRetentionPolicy {
                    max_active_per_binding: 0,
                    ..CandidateRetentionPolicy::default()
                },
            ),
            (
                "an overlarge limit",
                CandidateRetentionPolicy {
                    max_active_per_binding: MAX_ACTIVE_LIMIT + 1,
                    ..CandidateRetentionPolicy::default()
                },
            ),
            (
                "an instant-reclaim window",
                CandidateRetentionPolicy {
                    terminal_retention: Duration::from_secs(0),
                    ..CandidateRetentionPolicy::default()
                },
            ),
            (
                "a sub-minute window",
                CandidateRetentionPolicy {
                    terminal_retention: Duration::from_secs(59),
                    ..CandidateRetentionPolicy::default()
                },
            ),
        ] {
            assert_eq!(
                policy.ensure_valid().unwrap_err().kind(),
                CandidateRetentionErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
    }

    #[test]
    fn candidate_refs_outside_the_frozen_shape_are_rejected() {
        assert_eq!(
            validate_candidate_ref(&format!("{CANDIDATE_REF_PREFIX}{COMMIT}")),
            Ok(())
        );
        for (label, reference) in [
            ("a user branch", "refs/heads/main"),
            (
                "an abbreviated commit",
                &format!("{CANDIDATE_REF_PREFIX}0f9e8d7c"),
            ),
            (
                "an uppercase commit",
                &format!("{CANDIDATE_REF_PREFIX}{}", COMMIT.to_uppercase()),
            ),
        ] {
            assert_eq!(
                validate_candidate_ref(reference).unwrap_err().kind(),
                CandidateRetentionErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
        assert_eq!(
            candidate_id_of(&format!("{CANDIDATE_REF_PREFIX}{COMMIT}")),
            COMMIT
        );
    }

    #[test]
    fn the_branch_name_recheck_accepts_engine_names_and_refuses_otherwise() {
        assert!(is_engine_branch_name("winwincode/fix-login-0f9e8d7"));
        assert!(is_engine_branch_name("winwincode/task/sub_task-v1.2"));
        for (label, name) in [
            ("a user branch", "main"),
            ("outside the namespace", "feature/x"),
            ("an empty suffix", "winwincode/"),
            ("a dot-dot walk", "winwincode/../escape"),
            ("a reflog walk", "winwincode/task@{{x}}"),
            ("a lock suffix", "winwincode/task.lock"),
            ("a dash-led segment", "winwincode/-flag"),
        ] {
            assert!(
                !is_engine_branch_name(name),
                "{label} must never pass the deletion re-check"
            );
        }
    }

    #[test]
    fn uplink_frame_extraction_names_the_receipt_candidate() {
        let candidate = format!("{CANDIDATE_REF_PREFIX}{COMMIT}");
        let payload = apply_result_envelope(&candidate);
        assert_eq!(
            frame_candidate_ref("client.candidate.apply_result", &payload),
            Ok(Some(candidate)),
            "the receipt's candidate reference is extracted"
        );
    }

    #[test]
    fn a_candidate_frame_that_is_not_a_candidate_message_is_corrupt_state() {
        let envelope = ClientToServerEnvelope {
            schema_version: "1".to_owned(),
            message_id: "msg-2".to_owned(),
            client_node_id: "cnd_x".to_owned(),
            client_instance_id: "cin_x".to_owned(),
            sequence: 2,
            occurred_at: "2026-09-04T00:00:00.000Z".to_owned(),
            message: ClientToServerMessage::CommandAck(ClientCommandAckPayload {
                command_kind: ClientControlMessageKind::Heartbeat,
                command_message_id: "msg-1".to_owned(),
                status: CommandAckStatus::Accepted,
                current_revision: None,
                error: Option::<ClientControlError>::None,
            }),
        };
        let payload = serde_json::to_vec(&envelope).expect("the envelope should encode");
        let error = frame_candidate_ref("client.candidate.retained", &payload)
            .expect_err("a non-candidate frame must fail closed");
        assert_eq!(
            error.kind(),
            CandidateRetentionErrorKind::Store,
            "an outbox kind that claims to be a candidate frame but is not, is corrupt"
        );
    }
}
