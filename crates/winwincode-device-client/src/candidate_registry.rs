// SPDX-License-Identifier: Apache-2.0

//! Device-local candidate registry (GIT-100.2, plan 7.9/15.2).
//!
//! One place that manages the durable `candidate_local_refs` table: it
//! records the retention of a frozen Worker candidate (repository binding,
//! frozen commit, worker session, lifecycle state, retention stamp), reports
//! the retention upstream as a durable `client.candidate.retained` frame
//! stamped with the occupancy lease (`C + L`: command context plus the lease
//! and fencing token taken from the durable occupancy mirror), lists the
//! retained candidates after a restart for recovery, and reconciles the
//! retained set against the actual Git refs inside the bound repository
//! checkout so a missing or drifted ref is an explicit verdict instead of a
//! silent failure.
//!
//! Identity convention (the GIT-100.1 freeze precedent): the candidate id is
//! exactly the frozen candidate commit id, and the stable local ref is
//! `refs/winwincode/candidates/<candidate-commit>`. The registry derives both
//! from the retention's commit, so a repeated retention of the same candidate
//! re-encounters the same primary key and every derived wire fact (receipt
//! id, idempotency key, receipt payload) is deterministic — the same
//! candidate reported twice is the idempotent re-report the Control Plane's
//! ledger dedupes, never a second candidate.
//!
//! Local-data boundary: the reconciliation reads absolute paths from the
//! local `repository_path_mapping` to run Git, but no path ever enters the
//! registry rows, the retained frame, or any error the uplink returns. Only
//! the stable identities ride the wire.

use std::fmt;
use std::path::Path;
use std::process::Command;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use winwincode_client_port::domain::LocalCandidateState;
use winwincode_client_port::messages::{
    ClientCandidateRetainedPayload, ClientToServerMessage, CommandContext, OccupancyCommandContext,
};

use crate::daemon::DeviceDaemon;
use crate::store::{DeviceStore, DeviceStoreError};

/// Ref namespace prefix of every frozen Worker candidate; also the canonical
/// spelling of the product-level candidate reference the Control Plane's
/// ledger keys on.
pub const CANDIDATE_REF_PREFIX: &str = "refs/winwincode/candidates/";

/// Wire/storage spelling of the retained lifecycle state — the only state a
/// retention records and the only state a `client.candidate.retained` frame
/// reports.
const STATE_RETAINED: &str = "retained";

const MAX_ID_BYTES: usize = 200;
const MAX_REF_BYTES: usize = 200;

/// Bounded candidate-registry failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRegistryErrorKind {
    /// A retention, record, or report input violated the frozen shapes.
    InvalidInput,
    /// The candidate is already retained with different facts.
    Conflict,
    /// The device holds no occupancy mirror, so the retain uplink cannot
    /// stamp its lease (fail closed; nothing was reported).
    NoOccupancyMirror,
    /// The durable store or the Git reconciliation failed.
    Store,
}

/// Candidate-registry failure with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRegistryError {
    kind: CandidateRegistryErrorKind,
    message: String,
}

impl CandidateRegistryError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateRegistryErrorKind::Store,
            message: message.into(),
        }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> CandidateRegistryErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateRegistryError {}

impl From<DeviceStoreError> for CandidateRegistryError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

/// The facts of one candidate retention, as produced by the Worker freeze
/// path (the stable ref receipt) plus the owning identities.
///
/// `candidate_commit` is the frozen candidate commit; the candidate id and
/// the product-level candidate reference derive from it, so this input can
/// never describe two different candidates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRetention {
    /// Frozen candidate commit (full lowercase Git object id).
    pub candidate_commit: String,
    /// Repository binding the candidate was produced against.
    pub repository_binding_id: String,
    /// Worker session that froze the candidate.
    pub worker_session_id: String,
    /// Stable local Git ref holding the candidate,
    /// `refs/winwincode/candidates/<candidate-commit>`.
    pub local_git_ref: String,
    /// RFC 3339 stamp of the retention fact (the freeze).
    pub retained_at: String,
}

/// One durable registry row (`candidate_local_refs`).
///
/// LOCAL ONLY: `local_git_ref` names a ref inside the local checkout and no
/// path ever joins the row; the server-visible facts are the stable
/// candidate identity, the binding id, and the lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateLocalRefRecord {
    /// Stable candidate identity — exactly the frozen candidate commit id.
    pub candidate_id: String,
    /// Worker session that froze the candidate.
    pub worker_session_id: String,
    /// Repository binding the candidate was produced against.
    pub repository_binding_id: String,
    /// Stable local Git ref inside the bound checkout.
    pub local_git_ref: String,
    /// Plan 15 lifecycle state (`retained` until a later lane progresses it).
    pub local_state: LocalCandidateState,
    /// RFC 3339 stamp of the first recording.
    pub created_at: String,
    /// Product-level candidate reference (`refs/winwincode/candidates/<id>`);
    /// the identity the Control Plane's ledger keys on.
    pub candidate_ref: String,
    /// Frozen candidate commit (full lowercase Git object id).
    pub candidate_commit: String,
    /// RFC 3339 stamp of the retention fact the wire receipt reports.
    pub retained_at: String,
}

/// Outcome of one retention recording.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateRetentionOutcome {
    /// A new registry row was persisted.
    Recorded(CandidateLocalRefRecord),
    /// The same candidate was already retained (an idempotent re-report);
    /// the stored row is returned unchanged — original stamps and state —
    /// and no second row exists.
    Duplicate(CandidateLocalRefRecord),
}

/// The result of one full retain vertical (record plus uplink).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRetainReport {
    /// Whether this call recorded the row or met it as a duplicate.
    pub outcome: CandidateRetentionOutcome,
    /// Outbox sequence of the `client.candidate.retained` frame.
    pub frame_sequence: u64,
}

/// Verdict of one reconciliation against the actual Git refs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRefVerdict {
    /// The ref resolves to the recorded frozen commit.
    Verified,
    /// The ref is absent (or its binding no longer maps to a local
    /// checkout): the candidate is no longer resolvable locally.
    Missing,
    /// The ref resolves to a different commit than the recorded one.
    Drifted,
}

/// One reconciliation row: the retained record plus what Git actually
/// resolves today.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateReconciliation {
    pub record: CandidateLocalRefRecord,
    pub verdict: CandidateRefVerdict,
    /// The commit the ref resolves to now (`None` while missing).
    pub observed_commit: Option<String>,
}

/// Records one candidate retention (idempotent by the candidate identity).
///
/// The candidate commit is validated as a full lowercase Git object id and
/// the local ref as the canonical
/// `refs/winwincode/candidates/<candidate-commit>` name, so the derived
/// candidate id and candidate reference are pinned to the commit. When the
/// candidate is already retained, the stored row is returned unchanged
/// ([`CandidateRetentionOutcome::Duplicate`]) as long as every identity fact
/// (worker session, binding, ref, commit) matches; a diverging fact fails
/// closed with [`CandidateRegistryErrorKind::Conflict`]. Retention stamps are
/// never compared or rewritten: the first retention's stamps stay durable, a
/// replay may carry a freshly derived stamp.
///
/// # Errors
///
/// Returns [`CandidateRegistryErrorKind::InvalidInput`] for a shape
/// violation, [`CandidateRegistryErrorKind::Conflict`] for a diverging
/// re-report, and a store failure when the write fails or the store is
/// closed.
pub fn record_candidate_retention(
    store: &mut DeviceStore,
    retention: &CandidateRetention,
) -> Result<CandidateRetentionOutcome, CandidateRegistryError> {
    validate_retention(retention)?;
    let candidate_id = retention.candidate_commit.clone();
    let connection = store.connection_mut()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(crate::store::sql_error)?;
    let existing = read_candidate_row(&transaction, &candidate_id)?;
    if let Some(existing) = existing {
        ensure_same_candidate_facts(&existing, retention)?;
        transaction.commit().map_err(crate::store::sql_error)?;
        return Ok(CandidateRetentionOutcome::Duplicate(existing));
    }
    transaction
        .execute(
            "INSERT INTO candidate_local_refs \
             (candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
              local_state, created_at, candidate_ref, candidate_commit, retained_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                candidate_id,
                retention.worker_session_id,
                retention.repository_binding_id,
                retention.local_git_ref,
                STATE_RETAINED,
                retention.retained_at,
                retention.local_git_ref,
                retention.candidate_commit,
                retention.retained_at,
            ],
        )
        .map_err(crate::store::sql_error)?;
    transaction.commit().map_err(crate::store::sql_error)?;
    let record = candidate_local_ref(store, &candidate_id)?.ok_or_else(|| {
        CandidateRegistryError::store("recorded candidate row disappeared before the read-back")
    })?;
    Ok(CandidateRetentionOutcome::Recorded(record))
}

/// Loads one registry row by candidate id, whatever its lifecycle state.
///
/// # Errors
///
/// Returns a store failure when the read fails, the store is closed, or a
/// stored row disagrees with its schema vocabulary.
pub fn candidate_local_ref(
    store: &DeviceStore,
    candidate_id: &str,
) -> Result<Option<CandidateLocalRefRecord>, CandidateRegistryError> {
    if candidate_id.is_empty() {
        return Err(CandidateRegistryError::invalid(
            "candidate id must not be empty",
        ));
    }
    let connection = store.connection()?;
    connection
        .query_row(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at \
             FROM candidate_local_refs WHERE candidate_id = ?1",
            params![candidate_id],
            row_to_candidate_record,
        )
        .optional()
        .map_err(crate::store::sql_error)
        .map_err(CandidateRegistryError::from)
}

/// Lists every registry row in the `retained` state, in candidate-id order —
/// the recovery surface: after a restart this is the set of candidates the
/// device still holds for delivery.
///
/// # Errors
///
/// Returns a store failure when the read fails, the store is closed, or a
/// stored row disagrees with its schema vocabulary.
pub fn retained_candidates(
    store: &DeviceStore,
) -> Result<Vec<CandidateLocalRefRecord>, CandidateRegistryError> {
    let mut statement = store
        .connection()?
        .prepare(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at \
             FROM candidate_local_refs WHERE local_state = 'retained' \
             ORDER BY candidate_id",
        )
        .map_err(crate::store::sql_error)?;
    let rows = statement
        .query_map([], row_to_candidate_record)
        .map_err(crate::store::sql_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(crate::store::sql_error)
        .map_err(CandidateRegistryError::from)
}

/// Records the retention and reports it upstream as one vertical.
///
/// The recording is durable and idempotent (see
/// [`record_candidate_retention`]); then — replay or not — the
/// `client.candidate.retained` frame is enqueued into the durable outbox,
/// stamped `C + L` from the durable occupancy mirror: `expectedRevision` is
/// the mirror revision, the idempotency key is deterministic per candidate
/// reference, and the lease id plus fencing token are exactly the mirrored
/// lease. Re-reporting the same candidate therefore produces the identical
/// receipt under the identical idempotency key, which the Control Plane's
/// ledger settles as the same retention. Without an occupancy mirror the
/// uplink fails closed *after* the durable recording — the local retention
/// fact survives, and re-invoking the vertical once occupancy is mirrored
/// completes the report.
///
/// # Errors
///
/// Returns the recording failures of [`record_candidate_retention`],
/// [`CandidateRegistryErrorKind::NoOccupancyMirror`] when the device holds
/// no occupancy mirror, and a store failure when the outbox append fails.
pub fn retain_candidate(
    daemon: &mut DeviceDaemon,
    retention: &CandidateRetention,
) -> Result<CandidateRetainReport, CandidateRegistryError> {
    let outcome = record_candidate_retention(daemon.store_mut(), retention)?;
    let record = match &outcome {
        CandidateRetentionOutcome::Recorded(record)
        | CandidateRetentionOutcome::Duplicate(record) => record.clone(),
    };
    let frame_sequence = enqueue_candidate_retained(daemon, &record)?;
    Ok(CandidateRetainReport {
        outcome,
        frame_sequence,
    })
}

/// Enqueues the durable `client.candidate.retained` frame for one retained
/// registry row.
///
/// The receipt is derived deterministically from the row (receipt id
/// `lcr_<candidate id>`, the retention stamps, revision 1), so every report
/// of the same candidate encodes byte-identical receipt facts. Only rows in
/// the `retained` state may be reported — the retained frame reports the
/// retention event, and progressed lifecycle states belong to the later
/// apply-result lane.
///
/// # Errors
///
/// Returns [`CandidateRegistryErrorKind::InvalidInput`] for a row outside
/// the retained state or with divergent facts,
/// [`CandidateRegistryErrorKind::NoOccupancyMirror`] when the device holds
/// no occupancy mirror, and a store failure when the outbox append fails.
pub fn enqueue_candidate_retained(
    daemon: &mut DeviceDaemon,
    record: &CandidateLocalRefRecord,
) -> Result<u64, CandidateRegistryError> {
    validate_record(record)?;
    if record.local_state != LocalCandidateState::Retained {
        return Err(CandidateRegistryError::invalid(format!(
            "candidate {} is in state {:?}; only a retained candidate is reported \
             as client.candidate.retained",
            record.candidate_id, record.local_state
        )));
    }
    let mirror = daemon
        .occupancy_mirror()
        .ok_or_else(|| CandidateRegistryError {
            kind: CandidateRegistryErrorKind::NoOccupancyMirror,
            message: "the device holds no occupancy mirror; the candidate retention \
                      cannot be stamped with a lease"
                .to_owned(),
        })?;
    let receipt = winwincode_client_port::domain::LocalCandidateReceipt {
        local_candidate_receipt_id: format!("lcr_{}", record.candidate_id),
        candidate_ref: record.candidate_ref.clone(),
        repository_binding_id: record.repository_binding_id.clone(),
        candidate_commit: record.candidate_commit.clone(),
        local_ref_name: record.local_git_ref.clone(),
        state: record.local_state,
        created_at: record.retained_at.clone(),
        revision: 1,
    };
    daemon
        .enqueue(ClientToServerMessage::CandidateRetained(
            ClientCandidateRetainedPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: mirror.mirror_revision,
                        idempotency_key: format!("candidate-retained-{}", record.candidate_ref),
                    },
                    occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                    occupancy_fencing_token: mirror.fencing_token,
                },
                worker_session_id: record.worker_session_id.clone(),
                receipt,
            },
        ))
        .map_err(|error| {
            CandidateRegistryError::store(format!(
                "the candidate retention frame cannot enter the durable outbox: {error:?}"
            ))
        })
}

/// Reconciles every retained candidate against the actual Git refs of its
/// bound checkout — the post-restart audit the plan asks for.
///
/// For each retained row the bound repository's canonical local path (the
/// local `repository_path_mapping`, never uploaded) is resolved and the row's
/// ref is read through Git exactly as the worker reads it: a ref resolving to
/// the recorded commit is [`CandidateRefVerdict::Verified`], an absent ref —
/// including a binding that no longer maps to a local checkout — is
/// [`CandidateRefVerdict::Missing`], and a ref resolving to a different
/// commit is [`CandidateRefVerdict::Drifted`]. The scan is read-only: it
/// reports explicit verdicts instead of rewriting lifecycle facts, so a
/// transiently unmounted volume can never flip a candidate to a terminal
/// state as a side effect. A Git execution failure (anything other than a
/// clean missing-ref answer) is a store-kind error, never silently a verdict.
///
/// # Errors
///
/// Returns a store failure when a read fails, the store is closed, a stored
/// row disagrees with its schema vocabulary, or a Git invocation fails.
pub fn reconcile_retained_candidates(
    store: &DeviceStore,
) -> Result<Vec<CandidateReconciliation>, CandidateRegistryError> {
    let mut reconciliations = Vec::new();
    for record in retained_candidates(store)? {
        let observed = read_candidate_ref_through_git(store, &record)?;
        let (verdict, observed_commit) = match observed {
            Some(commit) if commit == record.candidate_commit => {
                (CandidateRefVerdict::Verified, Some(commit))
            }
            Some(commit) => (CandidateRefVerdict::Drifted, Some(commit)),
            None => (CandidateRefVerdict::Missing, None),
        };
        reconciliations.push(CandidateReconciliation {
            record,
            verdict,
            observed_commit,
        });
    }
    Ok(reconciliations)
}

/// Reads one candidate's ref through Git in its bound checkout.
///
/// `None` is the explicit missing-ref answer: either the binding no longer
/// maps to a local checkout or Git reports the ref absent.
fn read_candidate_ref_through_git(
    store: &DeviceStore,
    record: &CandidateLocalRefRecord,
) -> Result<Option<String>, CandidateRegistryError> {
    let Some(mapping) = store.path_mapping(&record.repository_binding_id)? else {
        return Ok(None);
    };
    if !Path::new(&mapping.canonical_path).exists() {
        return Ok(None);
    }
    read_git_ref(&mapping.canonical_path, &record.local_git_ref)
}

/// Runs `git rev-parse --verify --quiet` for one ref, mirroring the worker
/// freeze path's reading discipline: a status of 1 is the missing-ref
/// answer, success must yield exactly one identity, anything else is an
/// error.
fn read_git_ref(
    repository_path: &str,
    ref_name: &str,
) -> Result<Option<String>, CandidateRegistryError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository_path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command
        .output()
        .map_err(|error| CandidateRegistryError::store(format!("Git cannot be run: {error}")))?;
    if output.status.success() {
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| CandidateRegistryError::invalid("Git ref output is not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(CandidateRegistryError::store(
                "Git ref output is not a single identity",
            ));
        }
        return Ok(Some(text.to_owned()));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CandidateRegistryError::store(format!(
        "Git candidate ref cannot be read: {}",
        stderr.trim()
    )))
}

/// Validates one retention input before any write.
fn validate_retention(retention: &CandidateRetention) -> Result<(), CandidateRegistryError> {
    validate_commit_sha(&retention.candidate_commit, "candidate commit")?;
    require_non_empty(
        &retention.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(
        &retention.worker_session_id,
        "worker session id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&retention.retained_at, "retained at", MAX_ID_BYTES)?;
    validate_candidate_git_ref(&retention.local_git_ref, &retention.candidate_commit)
}

/// Validates one registry row before it is read back or reported.
fn validate_record(record: &CandidateLocalRefRecord) -> Result<(), CandidateRegistryError> {
    require_non_empty(&record.candidate_id, "candidate id", MAX_ID_BYTES)?;
    require_non_empty(&record.worker_session_id, "worker session id", MAX_ID_BYTES)?;
    require_non_empty(
        &record.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&record.local_git_ref, "local git ref", MAX_ID_BYTES)?;
    require_non_empty(&record.created_at, "created at", MAX_ID_BYTES)?;
    require_non_empty(&record.candidate_ref, "candidate ref", MAX_ID_BYTES)?;
    require_non_empty(&record.retained_at, "retained at", MAX_ID_BYTES)?;
    validate_commit_sha(&record.candidate_commit, "candidate commit")?;
    if record.candidate_id != record.candidate_commit {
        return Err(CandidateRegistryError::invalid(
            "candidate id must be exactly the frozen candidate commit",
        ));
    }
    validate_candidate_git_ref(&record.local_git_ref, &record.candidate_commit)?;
    if record.candidate_ref != record.local_git_ref {
        return Err(CandidateRegistryError::invalid(
            "candidate ref must be the canonical local candidate git ref",
        ));
    }
    Ok(())
}

/// A replayed (or duplicate) retention must describe the same candidate
/// facts; stamps and lifecycle state are deliberately not compared, matching
/// the Control Plane ledger's retention identity.
fn ensure_same_candidate_facts(
    existing: &CandidateLocalRefRecord,
    retention: &CandidateRetention,
) -> Result<(), CandidateRegistryError> {
    let matches = existing.worker_session_id == retention.worker_session_id
        && existing.repository_binding_id == retention.repository_binding_id
        && existing.local_git_ref == retention.local_git_ref
        && existing.candidate_ref == retention.local_git_ref
        && existing.candidate_commit == retention.candidate_commit;
    if matches {
        return Ok(());
    }
    Err(CandidateRegistryError::conflict(format!(
        "candidate {} is already retained with different facts",
        existing.candidate_id
    )))
}

fn read_candidate_row(
    connection: &rusqlite::Connection,
    candidate_id: &str,
) -> Result<Option<CandidateLocalRefRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT candidate_id, worker_session_id, repository_binding_id, local_git_ref, \
             local_state, created_at, candidate_ref, candidate_commit, retained_at \
             FROM candidate_local_refs WHERE candidate_id = ?1",
            params![candidate_id],
            row_to_candidate_record,
        )
        .optional()
        .map_err(crate::store::sql_error)
}

fn row_to_candidate_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateLocalRefRecord> {
    let local_state: String = row.get(4)?;
    Ok(CandidateLocalRefRecord {
        candidate_id: row.get(0)?,
        worker_session_id: row.get(1)?,
        repository_binding_id: row.get(2)?,
        local_git_ref: row.get(3)?,
        local_state: parse_local_state(&local_state)?,
        created_at: row.get(5)?,
        candidate_ref: row.get(6)?,
        candidate_commit: row.get(7)?,
        retained_at: row.get(8)?,
    })
}

/// Parses one stored lifecycle value, fail-closing on an unknown vocabulary.
fn parse_local_state(value: &str) -> rusqlite::Result<LocalCandidateState> {
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

/// Validates the frozen Git commit shape: full SHA-1 or SHA-256, lowercase
/// hex — the same shape the worker freeze and the server ledger enforce.
fn validate_commit_sha(value: &str, label: &str) -> Result<(), CandidateRegistryError> {
    let valid = (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(CandidateRegistryError::invalid(format!(
            "{label} is not a full lowercase git commit id"
        )))
    }
}

/// Validates the canonical stable candidate ref:
/// `refs/winwincode/candidates/<candidate-commit>` — the suffix must be
/// exactly the frozen commit, so the ref, the id, and the product reference
/// can never disagree inside one registry row.
fn validate_candidate_git_ref(
    local_git_ref: &str,
    candidate_commit: &str,
) -> Result<(), CandidateRegistryError> {
    let Some(suffix) = local_git_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateRegistryError::invalid(format!(
            "local git ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    let shaped = !suffix.is_empty()
        && suffix.len() <= MAX_REF_BYTES
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && suffix.as_bytes()[0].is_ascii_alphanumeric();
    if !shaped {
        return Err(CandidateRegistryError::invalid(
            "local git ref suffix is not a canonical candidate name",
        ));
    }
    if suffix != candidate_commit {
        return Err(CandidateRegistryError::invalid(
            "local git ref does not name the frozen candidate commit",
        ));
    }
    Ok(())
}

fn require_non_empty(
    value: &str,
    label: &str,
    max_bytes: usize,
) -> Result<(), CandidateRegistryError> {
    if value.is_empty() {
        return Err(CandidateRegistryError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(CandidateRegistryError::invalid(format!(
            "{label} must contain at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

    #[test]
    fn a_canonical_candidate_ref_names_its_commit() {
        let reference = format!("{CANDIDATE_REF_PREFIX}{COMMIT}");
        assert_eq!(
            validate_candidate_git_ref(&reference, COMMIT),
            Ok(()),
            "the freeze-convention ref validates"
        );
    }

    #[test]
    fn a_foreign_or_mismatched_ref_is_rejected() {
        assert_eq!(
            validate_candidate_git_ref("refs/heads/main", COMMIT)
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "a ref outside the namespace fails closed"
        );
        let drifted = format!("{CANDIDATE_REF_PREFIX}{}", "not-the-commit");
        assert_eq!(
            validate_candidate_git_ref(&drifted, COMMIT)
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "a ref naming another candidate fails closed"
        );
    }

    #[test]
    fn commit_shapes_beyond_the_frozen_vocabulary_are_rejected() {
        assert_eq!(
            validate_commit_sha("0F9E8D7C6B5A4938271605F4E3D2C1B0A9988776", "commit")
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "uppercase hex is not the frozen shape"
        );
        assert_eq!(
            validate_commit_sha("0f9e8d7c", "commit")
                .unwrap_err()
                .kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "an abbreviated commit is not a candidate identity"
        );
    }
}
