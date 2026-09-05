// SPDX-License-Identifier: Apache-2.0

//! Local branch creation engine for frozen Worker candidates (GIT-100.3).
//!
//! One vertical that turns a retained candidate into a durable local branch:
//! given a repository binding, a stable candidate reference
//! (`refs/winwincode/candidates/<candidate-commit>`), and a task slug (or an
//! explicit `winwincode/` branch name), the engine creates
//! `winwincode/<task-slug>-<short-id>` pointing exactly at the frozen
//! candidate commit with `git branch` — the user's current checkout, HEAD,
//! and working tree are never touched and no commit is created, so the
//! candidate commit's authorship is preserved verbatim.
//!
//! The full sequence, every step fail-closed before the next one mutates:
//!
//! 1. Validate the request shapes (binding id, canonical candidate ref,
//!    branch-name vocabulary) — nothing is read or written on refusal.
//! 2. Load the registry row: an unretained candidate is
//!    [`CandidateBranchErrorKind::CandidateMissing`], a foreign binding or a
//!    terminal row (`applied`/`discarded`) is
//!    [`CandidateBranchErrorKind::Conflict`].
//! 3. Require the durable occupancy mirror — the uplink must stamp a valid
//!    lease plus fencing token (contract 6), so without a mirror the engine
//!    refuses *before* touching the repository. Unlike the retention vertical
//!    (which records durably and then fails the report), a branch is a change
//!    to the user's repository: it is not created when it cannot be reported.
//! 4. Resolve the bound checkout and read the candidate ref through Git: a
//!    missing ref is [`CandidateBranchErrorKind::CandidateMissing`], a ref
//!    resolving elsewhere is [`CandidateBranchErrorKind::Conflict`].
//! 5. Determine the branch name. An explicit request must be a canonical
//!    `winwincode/` name that is either free or already resolves to the
//!    candidate commit — a name occupied by another commit fails closed with
//!    [`CandidateBranchErrorKind::Conflict`]. A derived name walks the stable
//!    ladder `winwincode/<slug>-<7|12|20|full commit id>` and takes the first
//!    rung that is free or already resolves to the candidate commit; the
//!    derivation is a pure function of (slug, commit, existing refs), so the
//!    same inputs always resolve to the same branch.
//! 6. Create the branch when it does not already resolve to the candidate,
//!    then confirm it by an exact re-read (the freeze-path discipline).
//! 7. Persist the durable created-branch record (the device-local equivalent
//!    of the `LocalApplyReceipt` facts) and progress the registry row to
//!    `branch_created` along the contract 6 transition table.
//! 8. Enqueue the durable `client.candidate.apply_result` frame stamped
//!    `C + L` from the occupancy mirror: strategy `create_branch`, result
//!    `branch_created`, deterministic receipt id and idempotency key per
//!    candidate, so a repeated request re-reports the identical receipt the
//!    Control Plane settles as the same creation.
//!
//! A repeated request returns the original branch: the durable record names
//! it, and any follow-up request verifies it still resolves to the candidate
//! and reports it again instead of creating a second branch. The engine
//! never silently re-creates a recorded branch that vanished and never
//! rewrites a diverged candidate ref.
//!
//! Failure reporting boundary: the engine reports the *success* apply result
//! (with its idempotent replay) and fails closed locally with the mapped
//! error kinds otherwise — the registry row stays retryable (`retained` /
//! `branch_created`) and unreported local `failed` progressions are left to
//! the apply-result settlement lane, which owns per-attempt receipt
//! identity. Callers surface [`CandidateBranchErrorKind`] to the user.
//!
//! Local-data boundary: the engine reads the binding's canonical local path
//! to run Git, but no path enters the durable rows, the frame, or any error.

use std::fmt;
use std::path::Path;
use std::process::Command;

use rusqlite::{OptionalExtension, params};
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerMessage, CommandContext,
    OccupancyCommandContext,
};

use crate::candidate_registry::{
    CANDIDATE_REF_PREFIX, CandidateLocalRefRecord, candidate_local_ref,
    progress_candidate_lifecycle,
};
use crate::daemon::DeviceDaemon;
use crate::store::{DeviceStore, DeviceStoreError};

/// Branch namespace of every branch this engine creates; an explicit branch
/// name must stay inside it so the engine can never touch a user branch such
/// as `main`.
pub const WINWINCODE_BRANCH_PREFIX: &str = "winwincode/";

/// Longest derived slug segment (before the `-<commit id>` suffix).
const MAX_SLUG_BYTES: usize = 48;

/// Longest explicit `winwincode/` branch name suffix.
const MAX_BRANCH_SUFFIX_BYTES: usize = 160;

const MAX_ID_BYTES: usize = 200;

/// Short-id ladder of the deterministic branch-name derivation, in rung
/// order. The final rung is the full commit id, so the ladder always
/// terminates with a name unique to the candidate.
const SHORT_ID_RUNGS: [usize; 4] = [7, 12, 20, usize::MAX];

/// The durable device-local record of one created branch — the facts the
/// `client.candidate.apply_result` receipt is derived from, so every report
/// of the same creation encodes identical receipt fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedBranchRecord {
    /// Candidate identity the branch was created from (the frozen commit).
    pub candidate_id: String,
    /// Repository binding the branch lives in.
    pub repository_binding_id: String,
    /// Branch name inside the `winwincode/` namespace (no `refs/heads/`).
    pub branch_name: String,
    /// RFC 3339 stamp of the first creation; never rewritten.
    pub created_at: String,
}

/// One validated branch-creation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationRequest {
    /// Repository binding whose local checkout holds the candidate ref.
    pub repository_binding_id: String,
    /// Stable candidate reference
    /// (`refs/winwincode/candidates/<candidate-commit>`).
    pub candidate_ref: String,
    /// Task slug for the derived `winwincode/<slug>-<short-id>` name. Only
    /// used (and only validated) when [`BranchCreationRequest::
    /// requested_branch_name`] is `None`.
    pub task_slug: String,
    /// Explicit `winwincode/` branch name; the derivation is skipped when
    /// present.
    pub requested_branch_name: Option<String>,
    /// RFC 3339 stamp of the request; the durable record keeps the first
    /// one it ever saw.
    pub requested_at: String,
}

/// Bounded branch-creation failure categories, mapped onto the contract 8
/// result-code vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateBranchErrorKind {
    /// A request input violated the frozen shapes; nothing was read or
    /// written.
    InvalidInput,
    /// Fail closed: the candidate lifecycle ended, a diverging fact was met
    /// (foreign binding, drifted candidate ref), the requested or derived
    /// branch name is occupied by another commit, or a recorded branch no
    /// longer resolves. Every conflict is stable: the same request retries
    /// to the same verdict.
    Conflict,
    /// The device holds no occupancy mirror, so the apply-result uplink
    /// cannot stamp its lease (fail closed; the repository was not touched).
    NoOccupancyMirror,
    /// Fail closed: the candidate ref does not resolve locally (the
    /// candidate was never retained, the binding maps no checkout, or the
    /// ref is gone) — contract 8 `candidate_missing`.
    CandidateMissing,
    /// The durable store or the Git execution failed.
    Store,
}

/// Branch-creation failure with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBranchError {
    kind: CandidateBranchErrorKind,
    message: String,
}

impl CandidateBranchError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn candidate_missing(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::CandidateMissing,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateBranchErrorKind::Store,
            message: message.into(),
        }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> CandidateBranchErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateBranchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateBranchError {}

impl From<DeviceStoreError> for CandidateBranchError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

impl From<crate::candidate_registry::CandidateRegistryError> for CandidateBranchError {
    fn from(error: crate::candidate_registry::CandidateRegistryError) -> Self {
        Self {
            kind: match error.kind() {
                crate::candidate_registry::CandidateRegistryErrorKind::InvalidInput => {
                    CandidateBranchErrorKind::InvalidInput
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Conflict => {
                    CandidateBranchErrorKind::Conflict
                }
                crate::candidate_registry::CandidateRegistryErrorKind::NoOccupancyMirror => {
                    CandidateBranchErrorKind::NoOccupancyMirror
                }
                crate::candidate_registry::CandidateRegistryErrorKind::Store => {
                    CandidateBranchErrorKind::Store
                }
            },
            message: error.message().to_owned(),
        }
    }
}

/// The stable facts of one branch-creation vertical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationFacts {
    /// Candidate identity the branch was created from (the frozen commit).
    pub candidate_id: String,
    /// Frozen candidate commit the branch points at.
    pub candidate_commit: String,
    /// Stable candidate reference the branch was created from.
    pub candidate_ref: String,
    /// Repository binding the branch lives in.
    pub repository_binding_id: String,
    /// Branch name inside the `winwincode/` namespace (no `refs/heads/`).
    pub branch_name: String,
    /// Full ref name of the created branch
    /// (`refs/heads/<branch name>`).
    pub branch_ref: String,
}

/// Whether this call created the branch or met it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchCreationOutcome {
    /// This call ran `git branch` and created the branch.
    Created(BranchCreationFacts),
    /// The branch already resolved to the candidate commit — either a
    /// earlier run of this engine (the durable record names it) or a
    /// pre-existing branch at the candidate — and nothing was created. The
    /// facts name the original branch a repeated request returns.
    Duplicate(BranchCreationFacts),
}

/// The result of one full branch-creation vertical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchCreationReport {
    /// Whether this call created the branch or met the original one.
    pub outcome: BranchCreationOutcome,
    /// Outbox sequence of the `client.candidate.apply_result` frame.
    pub frame_sequence: u64,
}

/// Creates the local branch for one retained candidate and reports it
/// upstream as one vertical.
///
/// See the module documentation for the full sequence and the failure
/// mapping. Idempotent: a repeated request — identical or with a different
/// slug — returns the original recorded branch and re-reports the identical
/// receipt under the identical idempotency key.
///
/// # Errors
///
/// Returns the failure categories of [`CandidateBranchErrorKind`]; every
/// error leaves the repository, the registry row, and the durable record in
/// their prior state.
pub fn create_candidate_branch(
    daemon: &mut DeviceDaemon,
    request: &BranchCreationRequest,
) -> Result<BranchCreationReport, CandidateBranchError> {
    validate_request(request)?;
    let candidate_id = candidate_id_of(&request.candidate_ref);
    let record = creation_guards(daemon, request, &candidate_id)?;
    let repository_path =
        bound_repository_path(daemon.store_mut(), &request.repository_binding_id)?;

    if let Some(durable) = created_branch_record(daemon.store_mut(), &candidate_id)? {
        // The repeated-request path: the recorded branch is the original
        // branch. Verify it still resolves to the candidate and re-report
        // it; never create a second branch beside it.
        let head = read_git_identity(&repository_path, &branch_ref_name(&durable.branch_name))?
            .ok_or_else(|| {
                CandidateBranchError::conflict(format!(
                    "the recorded branch {} no longer resolves; creation refuses \
                         to silently re-create it",
                    durable.branch_name
                ))
            })?;
        if head != record.candidate_commit {
            return Err(CandidateBranchError::conflict(format!(
                "the recorded branch {} drifted to another commit",
                durable.branch_name
            )));
        }
        let outcome = BranchCreationOutcome::Duplicate(facts_of(
            &record,
            &candidate_id,
            &durable.branch_name,
        ));
        return finish_creation(daemon, &record, &durable, outcome);
    }

    // Fresh creation: the candidate ref must resolve to exactly the frozen
    // commit the registry retained.
    let observed =
        read_git_identity(&repository_path, &request.candidate_ref)?.ok_or_else(|| {
            CandidateBranchError::candidate_missing(format!(
                "the candidate ref {CANDIDATE_REF_PREFIX}{candidate_id} no longer resolves locally"
            ))
        })?;
    if observed != record.candidate_commit {
        return Err(CandidateBranchError::conflict(format!(
            "the candidate ref resolves to {observed}, not the retained commit {}",
            record.candidate_commit
        )));
    }

    let (branch_name, preexisting) = determine_branch_name(&repository_path, request, &record)?;
    if !preexisting {
        create_git_branch(&repository_path, &branch_name, &record.candidate_commit)?;
    }
    let outcome = if preexisting {
        BranchCreationOutcome::Duplicate(facts_of(&record, &candidate_id, &branch_name))
    } else {
        BranchCreationOutcome::Created(facts_of(&record, &candidate_id, &branch_name))
    };

    let durable = write_branch_record(
        daemon.store_mut(),
        &candidate_id,
        &request.repository_binding_id,
        &branch_name,
        &request.requested_at,
    )?;
    finish_creation(daemon, &record, &durable, outcome)
}

/// The fail-closed guards that run before anything is read from Git: the
/// candidate must be retained on this binding and non-terminal, and the
/// occupancy mirror must exist so the vertical can be reported.
fn creation_guards(
    daemon: &mut DeviceDaemon,
    request: &BranchCreationRequest,
    candidate_id: &str,
) -> Result<CandidateLocalRefRecord, CandidateBranchError> {
    let record = candidate_local_ref(daemon.store_mut(), candidate_id)?.ok_or_else(|| {
        CandidateBranchError::candidate_missing(format!(
            "candidate ref {candidate_id} is not retained on this device"
        ))
    })?;
    if record.repository_binding_id != request.repository_binding_id {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} is retained for binding {} and cannot branch on {}",
            record.repository_binding_id, request.repository_binding_id
        )));
    }
    if matches!(
        record.local_state,
        LocalCandidateState::Applied | LocalCandidateState::Discarded
    ) {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} already reached the terminal state {:?}",
            record.local_state
        )));
    }
    // The uplink stamp exists before the repository is touched: a branch
    // that could not be reported is not created.
    if daemon.occupancy_mirror().is_none() {
        return Err(CandidateBranchError {
            kind: CandidateBranchErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the branch creation \
                      cannot be stamped with a lease"
                .to_owned(),
        });
    }
    Ok(record)
}

/// Resolves the target branch name: an explicit name must be free or already
/// resolve to the candidate commit, and a derived name walks the stable
/// short-id ladder. Returns the name plus whether it already resolves to the
/// candidate commit.
fn determine_branch_name(
    repository_path: &str,
    request: &BranchCreationRequest,
    record: &CandidateLocalRefRecord,
) -> Result<(String, bool), CandidateBranchError> {
    match &request.requested_branch_name {
        Some(name) => {
            let head = read_git_identity(repository_path, &branch_ref_name(name))?;
            match head {
                Some(commit) if commit == record.candidate_commit => Ok((name.clone(), true)),
                Some(_) => Err(CandidateBranchError::conflict(format!(
                    "the requested branch {name} already exists pointing at another commit"
                ))),
                None => Ok((name.clone(), false)),
            }
        }
        None => derive_branch_name(
            repository_path,
            &request.task_slug,
            &record.candidate_commit,
        ),
    }
}

/// Assembles the stable facts of one creation from the registry row.
fn facts_of(
    record: &CandidateLocalRefRecord,
    candidate_id: &str,
    branch_name: &str,
) -> BranchCreationFacts {
    BranchCreationFacts {
        candidate_id: candidate_id.to_owned(),
        candidate_commit: record.candidate_commit.clone(),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        branch_name: branch_name.to_owned(),
        branch_ref: branch_ref_name(branch_name),
    }
}

/// Shared tail of the vertical: progress the registry row to
/// `branch_created` and enqueue the lease-stamped apply-result frame.
fn finish_creation(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
    durable: &CreatedBranchRecord,
    outcome: BranchCreationOutcome,
) -> Result<BranchCreationReport, CandidateBranchError> {
    let record = progress_candidate_lifecycle(
        daemon.store_mut(),
        &record.candidate_id,
        LocalCandidateState::BranchCreated,
    )?;
    let frame_sequence = enqueue_branch_created(daemon, &record, durable)?;
    Ok(BranchCreationReport {
        outcome,
        frame_sequence,
    })
}

/// Enqueues the durable `client.candidate.apply_result` frame for one
/// created branch.
///
/// The receipt is derived deterministically from the durable facts (receipt
/// id `lar_branch_<candidate id>`, the creation stamp, revision 1, expected
/// head and resulting commit both the frozen candidate commit), so every
/// report of the same creation encodes byte-identical receipt facts under
/// the identical idempotency key — the Control Plane settles the replay as
/// the same creation.
///
/// # Errors
///
/// Returns [`CandidateBranchErrorKind::NoOccupancyMirror`] when the device
/// holds no occupancy mirror and a store failure when the outbox append
/// fails.
pub fn enqueue_branch_created(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
    durable: &CreatedBranchRecord,
) -> Result<u64, CandidateBranchError> {
    let mirror = daemon
        .occupancy_mirror()
        .ok_or_else(|| CandidateBranchError {
            kind: CandidateBranchErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the branch creation \
                  cannot be stamped with a lease"
                .to_owned(),
        })?;
    let receipt = LocalApplyReceipt {
        local_apply_receipt_id: format!("lar_branch_{}", record.candidate_id),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        target_branch: durable.branch_name.clone(),
        expected_head: record.candidate_commit.clone(),
        strategy: ApplyStrategy::CreateBranch,
        result: ApplyResult::BranchCreated,
        resulting_commit: Some(record.candidate_commit.clone()),
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
                        idempotency_key: format!(
                            "candidate-branch-created-{}",
                            record.candidate_ref
                        ),
                    },
                    occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                    occupancy_fencing_token: mirror.fencing_token,
                },
                receipt,
            },
        ))
        .map_err(|error| {
            CandidateBranchError::store(format!(
                "the branch creation frame cannot enter the durable outbox: {error:?}"
            ))
        })
}

/// Loads the durable created-branch record of one candidate, if any.
///
/// # Errors
///
/// Returns a store failure when the read fails, the store is closed, or the
/// stored row disagrees with its shape.
pub fn created_branch_record(
    store: &mut DeviceStore,
    candidate_id: &str,
) -> Result<Option<CreatedBranchRecord>, CandidateBranchError> {
    ensure_branch_schema(store)?;
    let connection = store.connection_mut()?;
    connection
        .query_row(
            "SELECT candidate_id, repository_binding_id, branch_name, created_at \
             FROM candidate_created_branches WHERE candidate_id = ?1",
            params![candidate_id],
            |row| {
                Ok(CreatedBranchRecord {
                    candidate_id: row.get(0)?,
                    repository_binding_id: row.get(1)?,
                    branch_name: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(crate::store::sql_error)
        .map_err(CandidateBranchError::from)
}

/// Persists the created-branch record; the first creation's stamp wins, and
/// a stored record for another binding or branch fails closed.
fn write_branch_record(
    store: &mut DeviceStore,
    candidate_id: &str,
    repository_binding_id: &str,
    branch_name: &str,
    requested_at: &str,
) -> Result<CreatedBranchRecord, CandidateBranchError> {
    ensure_branch_schema(store)?;
    let connection = store.connection_mut()?;
    connection
        .execute(
            "INSERT OR IGNORE INTO candidate_created_branches \
             (candidate_id, repository_binding_id, branch_name, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                candidate_id,
                repository_binding_id,
                branch_name,
                requested_at
            ],
        )
        .map_err(crate::store::sql_error)?;
    let stored = created_branch_record(store, candidate_id)?.ok_or_else(|| {
        CandidateBranchError::store("the branch record disappeared before the read-back")
    })?;
    if stored.repository_binding_id != repository_binding_id || stored.branch_name != branch_name {
        return Err(CandidateBranchError::conflict(format!(
            "candidate {candidate_id} already recorded branch {} on binding {}",
            stored.branch_name, stored.repository_binding_id
        )));
    }
    Ok(stored)
}

/// Creates the `candidate_created_branches` table on first use; the table is
/// additive and idempotent, so no store schema migration is involved.
fn ensure_branch_schema(store: &mut DeviceStore) -> Result<(), CandidateBranchError> {
    store
        .connection_mut()?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS candidate_created_branches (
                candidate_id TEXT PRIMARY KEY NOT NULL,
                repository_binding_id TEXT NOT NULL,
                branch_name TEXT NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .map_err(crate::store::sql_error)?;
    Ok(())
}

/// Resolves one binding's canonical local checkout; a binding that maps no
/// existing checkout is the candidate-missing verdict, exactly as the
/// registry's reconciliation treats it.
fn bound_repository_path(
    store: &mut DeviceStore,
    repository_binding_id: &str,
) -> Result<String, CandidateBranchError> {
    let mapping = store.path_mapping(repository_binding_id)?.ok_or_else(|| {
        CandidateBranchError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps no local checkout"
        ))
    })?;
    if !Path::new(&mapping.canonical_path).exists() {
        return Err(CandidateBranchError::candidate_missing(format!(
            "repository binding {repository_binding_id} maps a checkout that is absent"
        )));
    }
    Ok(mapping.canonical_path)
}

/// Walks the stable short-id ladder and returns the first branch name that
/// is free or already resolves to the candidate commit, plus whether the
/// name already resolves to the candidate.
fn derive_branch_name(
    repository_path: &str,
    task_slug: &str,
    candidate_commit: &str,
) -> Result<(String, bool), CandidateBranchError> {
    validate_task_slug(task_slug)?;
    for rung in SHORT_ID_RUNGS {
        let short_id = match rung {
            usize::MAX => candidate_commit,
            length => &candidate_commit[..length],
        };
        let name = format!("{WINWINCODE_BRANCH_PREFIX}{task_slug}-{short_id}");
        if validate_branch_name(&name).is_err() {
            continue;
        }
        match read_git_identity(repository_path, &branch_ref_name(&name))? {
            None => return Ok((name, false)),
            Some(commit) if commit == candidate_commit => return Ok((name, true)),
            Some(_) => {}
        }
    }
    Err(CandidateBranchError::conflict(format!(
        "every deterministic branch name for slug {task_slug} is occupied by another commit"
    )))
}

/// Validates one request before anything is read or written.
fn validate_request(request: &BranchCreationRequest) -> Result<(), CandidateBranchError> {
    require_non_empty(&request.repository_binding_id, "repository binding id")?;
    validate_candidate_ref(&request.candidate_ref)?;
    require_non_empty(&request.requested_at, "requested at")?;
    if let Some(name) = &request.requested_branch_name {
        validate_branch_name(name)?;
    }
    Ok(())
}

/// Extracts the candidate id (the frozen commit) from a validated candidate
/// reference.
fn candidate_id_of(candidate_ref: &str) -> String {
    candidate_ref
        .strip_prefix(CANDIDATE_REF_PREFIX)
        .unwrap_or(candidate_ref)
        .to_owned()
}

/// Validates the canonical stable candidate reference:
/// `refs/winwincode/candidates/<full lowercase git commit id>`.
fn validate_candidate_ref(candidate_ref: &str) -> Result<(), CandidateBranchError> {
    let Some(suffix) = candidate_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateBranchError::invalid(format!(
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
        Err(CandidateBranchError::invalid(
            "candidate ref does not name a full lowercase git commit id",
        ))
    }
}

/// Validates one `winwincode/` branch name against a conservative
/// `git check-ref-format` subset: every `/`-segment is non-empty, starts
/// with an alphanumeric or `_` (never `.` or `-`), never ends in `.lock`,
/// the charset is alphanumeric plus `. _ - /`, and the whole suffix carries
/// no `..`, no `@{`, and no trailing `.` or `/`. The engine can therefore
/// never name a branch outside the namespace or inject an option.
fn validate_branch_name(name: &str) -> Result<(), CandidateBranchError> {
    let Some(suffix) = name.strip_prefix(WINWINCODE_BRANCH_PREFIX) else {
        return Err(CandidateBranchError::invalid(format!(
            "branch name is not inside the {WINWINCODE_BRANCH_PREFIX} namespace"
        )));
    };
    let shaped = !suffix.is_empty()
        && suffix.len() <= MAX_BRANCH_SUFFIX_BYTES
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
        });
    if shaped {
        Ok(())
    } else {
        Err(CandidateBranchError::invalid(
            "branch name is not a canonical winwincode branch name",
        ))
    }
}

/// Validates one task slug for the derived
/// `winwincode/<slug>-<short-id>` name: a single ASCII segment of
/// alphanumerics, `_`, and `-`, starting and ending alphanumeric, so the
/// derived name always passes [`validate_branch_name`].
fn validate_task_slug(task_slug: &str) -> Result<(), CandidateBranchError> {
    let valid = !task_slug.is_empty()
        && task_slug.len() <= MAX_SLUG_BYTES
        && task_slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && task_slug.as_bytes()[0].is_ascii_alphanumeric()
        && task_slug.as_bytes()[task_slug.len() - 1].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(CandidateBranchError::invalid(
            "task slug is not a canonical single-segment slug",
        ))
    }
}

fn require_non_empty(value: &str, label: &str) -> Result<(), CandidateBranchError> {
    if value.is_empty() {
        return Err(CandidateBranchError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(CandidateBranchError::invalid(format!(
            "{label} must contain at most {MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

/// The full ref name of one branch name in the `winwincode/` namespace.
fn branch_ref_name(branch_name: &str) -> String {
    format!("refs/heads/{branch_name}")
}

/// Runs `git rev-parse --verify --quiet` for one ref: a status of 1 is the
/// missing-ref answer, success must yield exactly one identity, anything
/// else is an error.
fn read_git_identity(
    repository_path: &str,
    ref_name: &str,
) -> Result<Option<String>, CandidateBranchError> {
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
        .map_err(|error| CandidateBranchError::store(format!("Git cannot be run: {error}")))?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateBranchError::invalid("Git ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateBranchError::store(
                "Git ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateBranchError::store(format!(
        "Git ref cannot be read: {}",
        stderr.trim()
    )))
}

/// Runs `git branch <name> <commit>` — a ref write only: the current
/// checkout, HEAD, and working tree are untouched and no commit is created,
/// so the candidate commit's authorship is preserved verbatim. The result is
/// confirmed by an exact re-read before the caller may proceed.
fn create_git_branch(
    repository_path: &str,
    branch_name: &str,
    candidate_commit: &str,
) -> Result<(), CandidateBranchError> {
    let mut command = git_command(repository_path);
    command.args(["branch", "--end-of-options"]);
    command.arg(branch_name).arg(candidate_commit);
    let output = command
        .output()
        .map_err(|error| CandidateBranchError::store(format!("Git cannot be run: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("already exists") {
            return Err(CandidateBranchError::conflict(format!(
                "the branch {branch_name} already exists pointing at another commit"
            )));
        }
        return Err(CandidateBranchError::store(format!(
            "Git branch cannot be created: {stderr}"
        )));
    }
    let confirmed = read_git_identity(repository_path, &branch_ref_name(branch_name))?;
    if confirmed.as_deref() != Some(candidate_commit) {
        return Err(CandidateBranchError::store(format!(
            "the created branch {branch_name} does not resolve to the candidate commit"
        )));
    }
    Ok(())
}

fn git_command(repository_path: &str) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository_path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

    #[test]
    fn a_canonical_winwincode_branch_name_validates() {
        assert_eq!(
            validate_branch_name("winwincode/fix-login"),
            Ok(()),
            "a plain namespace branch validates"
        );
        assert_eq!(
            validate_branch_name("winwincode/task/sub_task-v1.2"),
            Ok(()),
            "nested segments with dot, underscore, and dash validate"
        );
    }

    #[test]
    fn branch_names_outside_the_vocabulary_are_rejected() {
        for (label, name) in [
            ("a user branch", "main"),
            ("an empty suffix", "winwincode/"),
            ("outside the namespace", "feature/x"),
            ("a dot segment", "winwincode/.hidden"),
            ("a dash-led segment", "winwincode/-flag"),
            ("a lock suffix", "winwincode/task.lock"),
            ("a dot-dot walk", "winwincode/../escape"),
            ("a reflog walk", "winwincode/task@{{x}}"),
            ("a trailing dot", "winwincode/task."),
            ("a trailing slash", "winwincode/task/"),
            ("an empty segment", "winwincode/task//more"),
            ("a space", "winwincode/fix login"),
            (
                "an overlong name",
                &format!("winwincode/{}", "a".repeat(161)),
            ),
        ] {
            assert_eq!(
                validate_branch_name(name).unwrap_err().kind(),
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
    }

    #[test]
    fn slugs_outside_the_vocabulary_are_rejected() {
        for (label, slug) in [
            ("empty", ""),
            ("an overlong slug", &"a".repeat(MAX_SLUG_BYTES + 1)),
            ("a path walk", "../evil"),
            ("a namespace escape", "a/b"),
            ("a leading dash", "-task"),
            ("a trailing dash", "task-"),
            ("a dot", "task."),
            ("a space", "fix login"),
        ] {
            assert_eq!(
                validate_task_slug(slug).unwrap_err().kind(),
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
        assert_eq!(validate_task_slug("fix_login-2"), Ok(()));
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
                CandidateBranchErrorKind::InvalidInput,
                "{label} must fail closed"
            );
        }
        assert_eq!(
            candidate_id_of(&format!("{CANDIDATE_REF_PREFIX}{COMMIT}")),
            COMMIT
        );
    }

    #[test]
    fn the_branch_ref_names_the_heads_namespace() {
        assert_eq!(
            branch_ref_name("winwincode/fix-login"),
            "refs/heads/winwincode/fix-login"
        );
    }
}
