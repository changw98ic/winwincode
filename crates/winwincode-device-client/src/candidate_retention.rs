// SPDX-License-Identifier: Apache-2.0

//! Candidate retention policy, discard, and garbage collection (GIT-100.9).
//!
//! One module that closes the candidate lifecycle the registry opened: it
//! decides how long each locally retained candidate is kept, executes the
//! discard decision, and reclaims the stable Git refs of finished candidates.
//! The five plan 15 retention classes map onto the registry states and this
//! module's rules as follows:
//!
//! - **In use / pending review** (`retained`, `branch_created`): kept, and
//!   bounded per repository binding by [`CandidateRetentionPolicy::
//!   max_active_per_binding`] — when a binding holds more active candidates
//!   than the limit, [`enforce_retention_policy`] discards the oldest ones
//!   (by first retention stamp) through exactly the same discard vertical a
//!   user discard takes. The default limit is deliberately conservative and
//!   `failed` rows are never counted nor touched: they belong to the
//!   apply/settlement lane.
//! - **Applied / discarded** (terminal): the frozen ref
//!   `refs/winwincode/candidates/<commit>` is still needed as the audit
//!   anchor until the Control Plane has settled every receipt, so
//!   [`collect_expired_candidates`] reclaims a terminal candidate's ref only
//!   when (a) its first retention stamp is older than
//!   [`CandidateRetentionPolicy::terminal_retention`], (b) every uplink
//!   frame the device ever enqueued for the candidate is acknowledged by the
//!   server (`published = 1` in the durable outbox — a candidate the server
//!   has never heard of, or not yet acknowledged, is *pending attention*
//!   and is never collected), and (c) the ref still resolves exactly to the
//!   recorded candidate commit — a drifted ref is reported, never deleted.
//!   Only refs this device created (registry rows) are ever deleted; stray
//!   refs inside the namespace (for example from another device sharing the
//!   repository) are invisible to the collection scan.
//! - **Expired**: a terminal candidate past its retention window whose
//!   collection already ran is durably recorded in the
//!   `candidate_gc_collections` table, so a later run skips it instead of
//!   re-examining it.
//!
//! The discard vertical ([`discard_candidate`]) is one fail-closed
//! sequence: request shapes, the registry row (unknown candidate, foreign
//! binding, and an `applied` terminal all refuse before any mutation), the
//! occupancy mirror (a discard that could not be reported is not executed —
//! the same discipline as the branch engine), then the durable
//! created-branch deletion, the durable discard record, the registry
//! transition to `discarded` along the contract 6 transition table, and the
//! `client.candidate.apply_result` frame with result `discarded` stamped
//! `C + L` from the mirror. Repeating a discard is an idempotent duplicate
//! that re-reports the identical receipt under the identical idempotency
//! key; a crash between any two steps leaves a state the next call resumes
//! from (a branch already deleted is simply confirmed gone).
//!
//! The collection vertical is equally idempotent and crash-resumable: the
//! ref deletion (`git update-ref -d`) is confirmed by an exact re-read and
//! the durable collected record is written after it, so a crash before the
//! deletion simply re-runs and a crash after the deletion (but before the
//! bookkeeping) resumes from the absent-ref answer on the next run. The
//! receipt-versus-ref ordering is fixed by construction: the Control Plane's
//! acknowledgements are asserted *before* the ref disappears, so the server
//! ledger (GIT-100.6) always outlives the local ref it audits. Object
//! pruning (`git prune --expire=now`) runs only when
//! [`CandidateRetentionPolicy::prune_objects`] is enabled — the default is
//! `false`, so the conservative default never removes a single object from
//! the user's repository; deleting the ref already makes the objects
//! collectable by Git's own expiry.
//!
//! Local-data boundary: the discard and collection scans read the binding's
//! canonical local checkout to run Git, but no path enters any durable row,
//! any wire frame, or any error this module returns.

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

/// Outbox kind of the retained-candidate uplink frame.
const RETAINED_KIND: &str = "client.candidate.retained";

/// Outbox kind of the apply-result uplink frame.
const APPLY_RESULT_KIND: &str = "client.candidate.apply_result";

const MAX_ID_BYTES: usize = 200;

/// Smallest retention window a policy may configure; anything shorter would
/// let a misconfiguration reclaim candidates the user never had a chance to
/// see.
const MIN_TERMINAL_RETENTION: Duration = Duration::from_mins(1);

/// Largest active-candidate limit a policy may configure.
const MAX_ACTIVE_LIMIT: u32 = 10_000;

/// Whether a candidate's uplink is settled: every frame the device ever
/// enqueued for it is acknowledged by the server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateUplinkState {
    /// Every candidate frame of this candidate is acknowledged.
    Settled,
    /// At least one frame is still awaiting the server acknowledgement.
    Unsettled,
    /// No frame was ever enqueued — the candidate is pending attention: the
    /// server does not know it exists, so it must never be reclaimed.
    NeverReported,
}

/// The retention policy for one device ([`CandidateRetentionPolicy::default`]
/// is the conservative shipped configuration).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateRetentionPolicy {
    /// How many active (`retained` / `branch_created`) candidates are kept
    /// per repository binding; the oldest beyond the limit are auto-
    /// discarded by [`enforce_retention_policy`].
    pub max_active_per_binding: u32,
    /// How long a terminal (`applied` / `discarded`) candidate's stable ref
    /// is kept before [`collect_expired_candidates`] may reclaim it. The
    /// window is measured from the first retention stamp, which is never
    /// later than the terminal transition, so the effective retention is
    /// always at least this long.
    pub terminal_retention: Duration,
    /// Whether the collection may run `git prune --expire=now` after
    /// deleting refs. The conservative default is `false`: the ref deletion
    /// alone already makes objects collectable by Git's own expiry, and a
    /// forced prune touches unreachable objects this feature does not own.
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
    /// Fails closed on a policy outside the safe bounds: the limit must keep
    /// at least one active candidate, and the retention window must be at
    /// least one minute (and encodable as a signed span).
    ///
    /// # Errors
    ///
    /// Returns [`CandidateRetentionErrorKind::InvalidInput`] for a zero
    /// limit, a limit beyond [`MAX_ACTIVE_LIMIT`], or a retention window
    /// below one minute or beyond the encodable range.
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

    /// The validated retention window as a signed span.
    fn retention_span(&self) -> Result<time::Duration, CandidateRetentionError> {
        time::Duration::try_from(self.terminal_retention).map_err(|_| {
            CandidateRetentionError::invalid(
                "the terminal retention window is not an encodable span",
            )
        })
    }
}

/// Bounded retention/discard/GC failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRetentionErrorKind {
    /// A request or policy input violated the frozen shapes; nothing was
    /// read or written.
    InvalidInput,
    /// Fail closed: a diverging fact was met (foreign binding, drifted
    /// created branch, an `applied` candidate, a branch checked out in a
    /// worktree). Every conflict is stable: the same request retries to the
    /// same verdict.
    Conflict,
    /// The device holds no occupancy mirror, so the discard uplink cannot
    /// stamp its lease (fail closed; nothing was mutated).
    NoOccupancyMirror,
    /// Fail closed: the candidate is not retained on this device, or the
    /// binding maps no local checkout.
    CandidateMissing,
    /// The durable store or the Git execution failed.
    Store,
}

/// Retention failure with an adapter-neutral category.
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

    /// Returns the stable machine-readable failure category.
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

/// One discard command: retire the candidate named by `candidate_ref`
/// (the stable `refs/winwincode/candidates/<commit>` reference) on the
/// binding it was retained against, delete its created local branch if one
/// exists, and report the discard upstream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardRequest {
    /// Repository binding the candidate was retained against.
    pub repository_binding_id: String,
    /// Stable candidate reference
    /// (`refs/winwincode/candidates/<candidate commit>`).
    pub candidate_ref: String,
    /// RFC 3339 stamp of the request; the durable discard record keeps the
    /// first one it ever saw.
    pub requested_at: String,
}

/// The stable facts of one discard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardFacts {
    /// Candidate identity (the frozen commit).
    pub candidate_id: String,
    /// Repository binding the candidate was retained against.
    pub repository_binding_id: String,
    /// Stable candidate reference.
    pub candidate_ref: String,
    /// Name of the created `winwincode/` branch that was (or had already
    /// been) deleted, when the candidate ever grew one.
    pub deleted_branch: Option<String>,
}

/// Whether this call executed the discard or met it as already done.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateDiscardOutcome {
    /// This call retired the candidate.
    Discarded(CandidateDiscardFacts),
    /// The candidate was already discarded (an idempotent replay); the
    /// original facts are returned and the identical receipt is re-reported
    /// under the identical idempotency key.
    Duplicate(CandidateDiscardFacts),
}

/// The result of one full discard vertical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardReport {
    /// Whether this call discarded the candidate or met it as a duplicate.
    pub outcome: CandidateDiscardOutcome,
    /// Outbox sequence of the `client.candidate.apply_result` frame.
    pub frame_sequence: u64,
}

/// The durable device-local record of one discard — the facts the
/// `client.candidate.apply_result` receipt is derived from, so every report
/// of the same discard encodes identical receipt fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDiscardRecord {
    /// Candidate identity the discard retired.
    pub candidate_id: String,
    /// Repository binding the candidate was retained against.
    pub repository_binding_id: String,
    /// Wire `targetBranch` of the receipt: the deleted branch name, or the
    /// candidate's own stable local ref when no branch ever existed.
    pub target_branch: String,
    /// RFC 3339 stamp of the first discard; never rewritten.
    pub created_at: String,
}

/// Executes one candidate discard (the full vertical).
///
/// See the module documentation for the whole sequence and its guarantees.
/// Idempotent: a repeated request returns the original discard as a
/// [`CandidateDiscardOutcome::Duplicate`] and re-reports the identical
/// receipt under the identical idempotency key. A retired (`applied`)
/// candidate is a delivered fact and refuses to be discarded; every failure
/// leaves the repository, the registry row, and the durable records in
/// their prior state.
///
/// # Errors
///
/// Returns [`CandidateRetentionErrorKind::InvalidInput`] for a malformed
/// request, [`CandidateRetentionErrorKind::CandidateMissing`] for an
/// unknown candidate or an absent checkout that still holds a created
/// branch, [`CandidateRetentionErrorKind::Conflict`] for a foreign binding,
/// an applied candidate, or a created branch that no longer holds the
/// candidate (or is checked out in a worktree),
/// [`CandidateRetentionErrorKind::NoOccupancyMirror`] when the device holds
/// no occupancy mirror, and a store failure for durable or Git failures.
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
    // The uplink stamp exists before anything is mutated: a discard that
    // could not be reported is not executed.
    if daemon.occupancy_mirror().is_none() {
        return Err(CandidateRetentionError {
            kind: CandidateRetentionErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the discard cannot be \
                      stamped with a lease"
                .to_owned(),
        });
    }

    // The created branch (if any) is deleted while it still holds exactly
    // the candidate commit; a missing checkout with a live branch record
    // refuses because the branch could not be verified.
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

    // Durable discard record first: the receipt facts exist before the
    // lifecycle moves, so a crash anywhere leaves a resumable state whose
    // eventual receipt is byte-identical.
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

/// Enqueues the durable `client.candidate.apply_result` frame for one
/// discarded candidate.
///
/// The receipt is derived deterministically from the registry row and the
/// durable discard record (receipt id a canonical `lar_` id, the
/// first discard stamp, revision 1, expected head the frozen candidate
/// commit, no resulting commit), so every report of the same discard encodes
/// byte-identical receipt facts under the identical idempotency key — the
/// Control Plane settles the replay as the same discard.
///
/// # Errors
///
/// Returns [`CandidateRetentionErrorKind::NoOccupancyMirror`] when the
/// device holds no occupancy mirror and a store failure when the outbox
/// append fails.
/// Deterministic canonical `lar_` receipt id for one discard delivery: the
/// first 25 uppercase hex characters of the candidate commit plus an `H`
/// domain tag (distinct from the branch-creation `G` receipt for the same
/// candidate), so discard replays stay byte-identical.
fn deterministic_lar_id(candidate_id: &str, tag: char) -> String {
    let hex: String = candidate_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
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
        // The wire vocabulary has no discard strategy; the receipt is tied
        // to the branch-creation delivery the discard undoes (or would have
        // been served by), so `create_branch` is the stable spelling.
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

/// Loads the durable discard record of one candidate, if any.
///
/// # Errors
///
/// Returns a store failure when the read fails, the store is closed, or the
/// stored row disagrees with its shape.
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

/// Persists the discard record; the first discard's stamp wins, and a stored
/// record for another binding or target fails closed.
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

/// Creates the `candidate_discards` table on first use; the table is
/// additive and idempotent, so no store schema migration is involved.
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

/// Why one terminal candidate was not collected in this run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcDeferralReason {
    /// The candidate's uplink is not fully acknowledged (or was never
    /// reported at all): pending attention, never reclaimed.
    PendingUplinkAck,
    /// The terminal retention window has not elapsed yet.
    RetentionPending,
    /// The stable ref no longer resolves to the recorded candidate commit.
    RefDrifted,
    /// The binding currently maps no local checkout, so the ref could not
    /// be verified.
    CheckoutUnavailable,
}

/// One reclaimed candidate ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectedCandidate {
    /// Candidate identity (the frozen commit).
    pub candidate_id: String,
    /// Repository binding whose checkout held the ref.
    pub repository_binding_id: String,
    /// The stable candidate reference that was reclaimed.
    pub candidate_ref: String,
    /// Whether the ref was still present in this run (a resumed run after a
    /// crash between deletion and bookkeeping sees `false`).
    pub ref_was_present: bool,
}

/// One terminal candidate this run deliberately left alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredCandidate {
    /// Candidate identity (the frozen commit).
    pub candidate_id: String,
    /// Why the candidate was not collected.
    pub reason: GcDeferralReason,
}

/// The result of one collection run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    /// Terminal candidates examined (already-collected rows excluded).
    pub examined: usize,
    /// Terminal candidates found already durably collected by an earlier
    /// run.
    pub already_collected: usize,
    /// Candidates whose stable ref was reclaimed in this run.
    pub collected: Vec<CollectedCandidate>,
    /// Candidates deliberately left alone, with the reason.
    pub deferred: Vec<DeferredCandidate>,
    /// Bindings whose checkout was pruned (only when the policy enables
    /// pruning and a ref was actually reclaimed there).
    pub pruned_bindings: Vec<String>,
}

/// Reclaims the stable refs of expired terminal candidates.
///
/// Only registry rows in the terminal `applied` / `discarded` states are
/// examined, only refs this device created are ever deleted, and every
/// candidate must pass all three gates before its ref disappears: the
/// retention window has elapsed, the uplink is fully acknowledged (pending
/// attention is never reclaimed), and the ref still resolves exactly to the
/// recorded commit. Any Git execution failure aborts the whole run with an
/// error instead of guessing — the run is idempotent, so the caller simply
/// retries.
///
/// # Errors
///
/// Returns [`CandidateRetentionErrorKind::InvalidInput`] for an invalid
/// policy and a store failure for durable or Git execution failures; in
/// every error case the already-collected refs stay collected and the rest
/// stays untouched.
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
        // The ref must be exactly the identity the registry recorded before
        // this module may delete it.
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

/// The wire/storage spelling of one plan 15 lifecycle state, matching the
/// registry's vocabulary; an unknown value fails closed.
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

/// Loads every terminal registry row (`applied` / `discarded`) in candidate
/// id order — the collection scan's input.
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

/// Creates the `candidate_gc_collections` table on first use and returns the
/// ids already durably collected by an earlier run.
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

/// Durably records one collected candidate; the first collection stamp wins.
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

/// Classifies one candidate's uplink against the durable outbox: a frame is
/// settled exactly when the server acknowledged it (`published = 1`, the
/// durable mark the acknowledgement state machine writes). A candidate with
/// no frames at all is `NeverReported`.
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

/// Extracts the receipt's candidate reference from one stored outbox frame;
/// `None` when the frame belongs to another candidate. A frame whose kind
/// and payload disagree is corrupt state and fails closed.
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

/// The per-binding retention sweep: discards the oldest active candidates
/// beyond the policy limit.
///
/// Only `retained` / `branch_created` rows count as active (`failed` rows
/// belong to the apply/settlement lane and are never counted nor touched);
/// ordering is by first retention stamp with the candidate id as the
/// deterministic tie-break, and a stamp that does not parse fails closed.
/// Each excess candidate is discarded through [`discard_candidate`], so the
/// sweep is exactly as fail-closed and idempotent as a user discard: the
/// first failure stops the sweep (everything before it stays durably done)
/// and a re-run resumes where it stopped.
///
/// # Errors
///
/// Returns the failure of an invalid policy, an unparsable durable stamp,
/// or the first discard that could not complete.
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

/// The result of one retention-policy sweep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionSweepReport {
    /// Repository binding the policy was enforced for.
    pub repository_binding_id: String,
    /// The configured active-candidate limit.
    pub limit: u32,
    /// Active candidates found before the sweep.
    pub active_before: usize,
    /// Facts of every candidate the sweep discarded.
    pub discarded: Vec<CandidateDiscardFacts>,
    /// Active candidates left after the sweep (equals `limit` only when the
    /// sweep completed; a failed sweep stops early).
    pub active_remaining: usize,
}

/// Lists one binding's active (`retained` / `branch_created`) registry rows
/// oldest-first, failing closed on an unparsable stamp.
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

/// Resolves one binding's canonical local checkout; a binding that maps no
/// existing checkout is the candidate-missing verdict, exactly as the
/// registry's reconciliation treats it.
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

/// Deletes one candidate's created branch when — and only when — it still
/// resolves exactly to the frozen candidate commit: an already-deleted
/// branch is the idempotent success answer, a drifted branch and a branch
/// checked out in any worktree fail closed with a stable conflict, and any
/// other Git failure is a store failure. The deletion is confirmed by an
/// exact re-read.
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
                // Git spells the checked-out refusal either way depending on
                // the version; both mean a worktree holds the branch.
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

/// Defense-in-depth re-check of a durable created-branch name before any
/// deletion: the name must still be inside the `winwincode/` namespace with
/// a canonical shape (the branch engine validated it at creation; this
/// re-check keeps a corrupt durable row from ever naming a user branch).
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

/// Deletes one stable candidate ref with `git update-ref -d` and confirms
/// the deletion by an exact re-read. The ref name is validated as the
/// canonical candidate ref of the record before this call.
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

/// Runs `git prune --expire=now` in one binding's checkout; only reachable
/// from the explicit `prune_objects` policy switch.
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

/// Runs `git rev-parse --verify --quiet` for one ref: a status of 1 is the
/// missing-ref answer, success must yield exactly one identity, anything
/// else is an error.
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

/// Validates one discard request before anything is read.
fn validate_discard_request(
    request: &CandidateDiscardRequest,
) -> Result<(), CandidateRetentionError> {
    require_non_empty(&request.repository_binding_id, "repository binding id")?;
    require_non_empty(&request.requested_at, "requested at")?;
    validate_candidate_ref(&request.candidate_ref)
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

/// Parses one durable RFC 3339 stamp, failing closed on a corrupted value.
fn parse_durable_stamp(
    value: &str,
    label: &str,
) -> Result<OffsetDateTime, CandidateRetentionError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        CandidateRetentionError::store(format!("{label} is not an RFC 3339 stamp: {error}"))
    })
}

fn now_rfc3339(now: &OffsetDateTime) -> String {
    now.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
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
