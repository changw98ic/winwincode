// SPDX-License-Identifier: Apache-2.0

//! The target-branch safe apply engine (GIT-100.4, plan 15.4/15.5).
//!
//! One call applies a retained candidate onto a target branch without ever
//! writing the user's current working tree:
//!
//! 1. **Preflight** (fail closed, in order): request shapes, the candidate
//!    registry row (identity match, non-terminal), the occupancy fencing
//!    stamp ([`FencingGuard::authorize_command`] over
//!    [`FencedCommandKind::CandidateApply`]), the binding's canonical
//!    checkout ([`ConfinedRoot`]), the candidate ref through Git, the
//!    `expectedHead == target HEAD` equality, the dirty policy of the
//!    user's working tree, and the guard against a target branch that is
//!    checked out in any worktree.
//! 2. **Isolated execution**: a detached *integration worktree* is created
//!    from the validated target tip inside a device-local integration root —
//!    never inside the user's checkout — and the requested strategy
//!    (`fast_forward`, `cherry_pick`, `merge`) runs there. A conflict stops
//!    inside the integration worktree; the conflict artifact (the worktree
//!    plus a machine-readable summary) stays in the isolated directory.
//! 3. **Atomic target ref update**: the produced commit is published with a
//!    compare-and-swap `git update-ref refs/heads/<target> <new>
//!    <expectedHead>` — Git itself refuses when the branch moved during the
//!    apply, which answers the drift case (`base_stale`) without a race.
//!    Plumbing `update-ref` would happily move a branch that is checked out
//!    in a worktree (silently re-staging the user's tree), which is exactly
//!    why preflight refuses such targets outright.
//! 4. **Durable record**: the candidate registry row transitions to
//!    `applied` (success) or `failed` (every fail-closed result, retryable),
//!    and one `client.candidate.apply_result` frame carrying the
//!    `LocalApplyReceipt` is appended to the durable outbox. Every attempt
//!    settles exactly one receipt with a fresh `lar_` id — a retry appends a
//!    new receipt instead of rewriting history, matching the Control Plane's
//!    append-only ledger (GIT-100.6).
//!
//! Result-code mapping (the frozen ten-code vocabulary): `candidate_missing`
//! (unmapped binding, vanished checkout, absent or drifted candidate ref),
//! `base_stale` (target branch absent, HEAD ≠ `expectedHead`, or CAS drift),
//! `working_tree_dirty` (the user's tree violates the dirty policy),
//! `merge_conflict` (isolated worktree reported unmerged paths, with the
//! conflict artifact reference), `permission_denied` (the operating system
//! refused the local Git work), `failed` (every other execution failure,
//! including a non-fast-forward candidate under the `fast_forward` strategy
//! and a target branch checked out in a worktree), `applied` (success, with
//! `resulting_commit`). `retained`, `branch_created`, and `discarded` are
//! not produced by this engine: branch creation belongs to the
//! `candidate_branch` engine (GIT-100.3) and discarding is the later discard
//! path — requesting [`ApplyStrategy::CreateBranch`] here is refused as
//! invalid input.
//!
//! Refusals versus settled failures: malformed input, an unknown or terminal
//! candidate, and a rejected fencing stamp refuse the command *before any
//! local action* ([`CandidateApplyError`], no receipt, nothing to retry
//! beyond re-fencing); every attempt that actually reached Git settles into
//! a receipt whose failure code is retryable by design.
//!
//! Local-data boundary: the integration root, checkout paths, and conflict
//! artifact files never leave the device. The wire receipt carries only
//! stable identities; the conflict artifact reference is an opaque
//! device-local reference (validated server-side to never be a filesystem
//! path).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerMessage, CommandContext,
    OccupancyCommandContext,
};

use crate::candidate_registry::{CANDIDATE_REF_PREFIX, CandidateLocalRefRecord};
use crate::daemon::DeviceDaemon;
use crate::fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
use crate::identity::generate_prefixed_id;
use crate::path_confinement::ConfinedRoot;
use crate::store::{DeviceStore, DeviceStoreError};

/// Ref message stamped onto the atomic target ref update.
const APPLY_REF_MESSAGE_PREFIX: &str = "winwincode: apply candidate";

/// Committer identity of the commits this engine creates. The device client
/// is honestly the committer; a cherry-pick preserves the candidate commit's
/// original author, and a merge commit's author is the device client.
const COMMITTER_NAME: &str = "WinWinCode Device Client";
const COMMITTER_EMAIL: &str = "device-client@winwincode.invalid";

/// Device-local directory (under the caller's integration root) holding the
/// per-attempt integration worktrees, keyed by
/// `<candidate id>/<apply receipt id>`. Conflict artifacts are kept here;
/// every other attempt removes its directory when it finishes.
const ARTIFACT_DIRECTORY: &str = "conflict-artifacts";

/// Machine-readable summary written next to a kept conflict worktree.
const CONFLICT_SUMMARY_FILE: &str = "conflict.json";

/// Subdirectory of one attempt holding the integration worktree itself.
const WORKTREE_DIRECTORY: &str = "worktree";

const MAX_ID_BYTES: usize = 200;
const MAX_BRANCH_BYTES: usize = 200;

/// One apply command: apply the candidate named by `candidate_ref` (the
/// stable `refs/winwincode/candidates/<commit>` reference) onto
/// `target_branch`, which must still sit exactly at `expected_head`.
///
/// The occupancy lease id and fencing token stamp the command the same way
/// every other fenced device command does (plan 12.6). The strategy is
/// explicit; [`ApplyStrategy::CreateBranch`] is refused here because branch
/// creation is the separate GIT-100.3 engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplyRequest {
    /// Repository binding the candidate and target branch live in.
    pub repository_binding_id: String,
    /// Stable candidate reference
    /// (`refs/winwincode/candidates/<candidate commit>`).
    pub candidate_ref: String,
    /// Target branch to update (`refs/heads/<target branch>`).
    pub target_branch: String,
    /// Expected target HEAD the apply is validated against.
    pub expected_head: String,
    /// Delivery strategy of this attempt.
    pub strategy: ApplyStrategy,
    /// Occupancy lease id stamping the command.
    pub occupancy_lease_id: String,
    /// Occupancy fencing token stamping the command.
    pub occupancy_fencing_token: u64,
}

/// Stable apply-engine failure categories for the *refusals* — commands that
/// never reached Git and therefore never settled a receipt. Everything that
/// reached Git is a settled [`ApplyResult`] on the returned receipt instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateApplyErrorKind {
    /// The request violated the frozen shapes, or asked this engine for the
    /// branch-creation strategy.
    InvalidInput,
    /// The occupancy stamp (or a mid-flight re-check of it) was rejected:
    /// the command is refused before any local action and may be retried
    /// once the occupancy is re-established.
    FencingRejected(FencingRejection),
    /// No retained registry row exists for the candidate reference.
    UnknownCandidate,
    /// The candidate already reached its `applied` or `discarded` terminal.
    TerminalCandidate,
    /// The durable store failed. When this happens after the target ref
    /// moved, the state is truthful-but-unreported and a retry re-settles it.
    Store,
}

/// Apply-engine refusal with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplyError {
    kind: CandidateApplyErrorKind,
    message: String,
}

impl CandidateApplyError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateApplyErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn fencing(rejection: FencingRejection) -> Self {
        Self {
            kind: CandidateApplyErrorKind::FencingRejected(rejection),
            message: format!("the candidate apply is refused by occupancy fencing: {rejection:?}"),
        }
    }

    fn unknown_candidate(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateApplyErrorKind::UnknownCandidate,
            message: message.into(),
        }
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateApplyErrorKind::TerminalCandidate,
            message: message.into(),
        }
    }

    fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateApplyErrorKind::Store,
            message: message.into(),
        }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> CandidateApplyErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CandidateApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CandidateApplyError {}

impl From<DeviceStoreError> for CandidateApplyError {
    fn from(error: DeviceStoreError) -> Self {
        Self::store(format!("device client store failure: {error}"))
    }
}

impl From<crate::candidate_registry::CandidateRegistryError> for CandidateApplyError {
    fn from(error: crate::candidate_registry::CandidateRegistryError) -> Self {
        Self::store(format!("candidate registry failure: {error}"))
    }
}

/// The durable outcome of one settled apply attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplyOutcome {
    /// The immutable receipt of this attempt (identical to the receipt
    /// inside the durable `client.candidate.apply_result` frame).
    pub receipt: LocalApplyReceipt,
    /// Outbox sequence of the `client.candidate.apply_result` frame.
    pub frame_sequence: u64,
}

/// What one settled attempt concluded. A settled attempt always becomes
/// exactly one receipt.
struct SettledAttempt {
    result: ApplyResult,
    resulting_commit: Option<String>,
    conflict_artifact_ref: Option<String>,
    detail: String,
}

impl SettledAttempt {
    fn refused(result: ApplyResult, detail: impl Into<String>) -> Self {
        Self {
            result,
            resulting_commit: None,
            conflict_artifact_ref: None,
            detail: detail.into(),
        }
    }
}

/// Why one attempt stopped: either a settled outcome that becomes a receipt,
/// or a fencing re-check that lost between authorization and execution —
/// nothing ran, so nothing settles.
enum AttemptStop {
    Fencing(FencingRejection),
    Settled(SettledAttempt),
}

/// Applies one retained candidate onto its target branch (the full vertical).
///
/// See the module documentation for the whole sequence. Every failure is
/// fail closed: the user's working tree is only ever read, and the target
/// branch is only ever changed by the final compare-and-swap ref update of
/// a successful apply. Every settled failure is retryable with a fresh
/// attempt.
///
/// # Errors
///
/// Returns [`CandidateApplyErrorKind::InvalidInput`] for a malformed request
/// or the branch-creation strategy,
/// [`CandidateApplyErrorKind::UnknownCandidate`] when no retained registry
/// row matches the candidate reference,
/// [`CandidateApplyErrorKind::TerminalCandidate`] when the candidate already
/// ended, [`CandidateApplyErrorKind::FencingRejected`] when the occupancy
/// stamp is not current, and [`CandidateApplyErrorKind::Store`] for durable
/// failures. Every other outcome — including `candidate_missing`,
/// `base_stale`, `working_tree_dirty`, `merge_conflict`,
/// `permission_denied`, and `failed` — is a settled receipt, not an error.
pub fn apply_candidate_to_branch(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
    integration_root: &Path,
) -> Result<CandidateApplyOutcome, CandidateApplyError> {
    validate_request(request)?;
    let record = load_candidate_for_request(daemon.store_mut(), request)?;
    let ticket = authorize_fencing(daemon, request)?;
    let receipt_id = generate_prefixed_id("lar_")
        .map_err(|error| CandidateApplyError::store(format!("apply receipt id: {error}")))?;

    let stop = run_attempt(
        daemon.store_mut(),
        &record,
        request,
        integration_root,
        &receipt_id,
        &ticket,
    );
    let settled = match stop {
        AttemptStop::Fencing(rejection) => return Err(CandidateApplyError::fencing(rejection)),
        AttemptStop::Settled(settled) => settled,
    };

    // Durable record: registry transition first (the local ledger
    // equivalent), then the durable outbox frame (the uplink). A failure
    // after the ref moved leaves a truthful retryable state; the next
    // attempt re-settles it.
    let target_state = if settled.result == ApplyResult::Applied {
        "applied"
    } else {
        "failed"
    };
    transition_candidate_state(daemon.store_mut(), &record.candidate_id, target_state)?;
    let receipt = LocalApplyReceipt {
        local_apply_receipt_id: receipt_id,
        candidate_ref: request.candidate_ref.clone(),
        repository_binding_id: request.repository_binding_id.clone(),
        target_branch: request.target_branch.clone(),
        expected_head: request.expected_head.clone(),
        strategy: request.strategy,
        result: settled.result,
        resulting_commit: settled.resulting_commit,
        conflict_artifact_ref: settled.conflict_artifact_ref,
        created_at: now_rfc3339(),
        revision: 1,
    };
    let frame_sequence = enqueue_apply_result(daemon, &ticket, &receipt)?;
    Ok(CandidateApplyOutcome {
        receipt,
        frame_sequence,
    })
}

/// Authorizes the command's occupancy stamp against the durable mirror.
fn authorize_fencing(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
) -> Result<FencingTicket, CandidateApplyError> {
    let guard = FencingGuard::from_store(daemon.store_mut())?;
    match guard.authorize_command(
        FencedCommandKind::CandidateApply,
        &request.occupancy_lease_id,
        request.occupancy_fencing_token,
    ) {
        FencingVerdict::Authorized(ticket) => Ok(ticket),
        FencingVerdict::Rejected(rejection) => Err(CandidateApplyError::fencing(rejection)),
    }
}

/// Loads the registry row for the request and enforces the candidate
/// identity (the device-side mirror of the server ledger's settlement
/// binding check) and the non-terminal lifecycle.
fn load_candidate_for_request(
    store: &mut DeviceStore,
    request: &CandidateApplyRequest,
) -> Result<CandidateLocalRefRecord, CandidateApplyError> {
    let candidate_id = candidate_id_from_ref(&request.candidate_ref)?;
    let Some(record) = crate::candidate_registry::candidate_local_ref(store, &candidate_id)? else {
        return Err(CandidateApplyError::unknown_candidate(format!(
            "no retained candidate exists for {}",
            request.candidate_ref
        )));
    };
    if record.candidate_ref != request.candidate_ref {
        return Err(CandidateApplyError::unknown_candidate(
            "the stored candidate reference does not match the request",
        ));
    }
    if record.repository_binding_id != request.repository_binding_id {
        return Err(CandidateApplyError::invalid(
            "the candidate is retained against a different repository binding",
        ));
    }
    if matches!(
        record.local_state,
        LocalCandidateState::Applied | LocalCandidateState::Discarded
    ) {
        return Err(CandidateApplyError::terminal(format!(
            "the candidate already reached the terminal state {:?}",
            record.local_state
        )));
    }
    Ok(record)
}

/// Runs the preflight checks and, when they pass, the isolated integration
/// worktree execution. Read-only against the user's checkout; the only write
/// anywhere outside the integration root is the final compare-and-swap ref
/// update of a successful apply.
fn run_attempt(
    store: &mut DeviceStore,
    record: &CandidateLocalRefRecord,
    request: &CandidateApplyRequest,
    integration_root: &Path,
    receipt_id: &str,
    ticket: &FencingTicket,
) -> AttemptStop {
    // Preflight: the binding resolves to a canonical, confined checkout.
    let repository = match resolve_binding_checkout(store, &request.repository_binding_id) {
        Ok(repository) => repository,
        Err(settled) => return AttemptStop::Settled(settled),
    };

    // Preflight: the candidate ref still resolves to the frozen commit.
    match read_git_ref(&repository, &request.candidate_ref) {
        Ok(None) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::CandidateMissing,
                "the candidate ref no longer resolves in the bound checkout",
            ));
        }
        Ok(Some(commit)) if commit != record.candidate_commit => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::CandidateMissing,
                "the candidate ref drifted away from the frozen candidate commit",
            ));
        }
        Ok(Some(_)) => {}
        Err(failure) => {
            return AttemptStop::Settled(failure.settled("the candidate ref read"));
        }
    }

    // Preflight: the target branch sits exactly at the expected head.
    let target_ref = format!("refs/heads/{}", request.target_branch);
    let current_head = match read_git_ref(&repository, &target_ref) {
        Ok(None) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::BaseStale,
                format!("the target branch {target_ref} does not exist"),
            ));
        }
        Ok(Some(head)) => head,
        Err(failure) => {
            return AttemptStop::Settled(failure.settled("the target branch read"));
        }
    };
    if current_head != request.expected_head {
        return AttemptStop::Settled(SettledAttempt::refused(
            ApplyResult::BaseStale,
            format!(
                "the target branch is at {current_head}, not the expected {}",
                request.expected_head
            ),
        ));
    }

    // Preflight: the user's working tree satisfies the dirty policy.
    match working_tree_dirty(&repository) {
        Ok(false) => {}
        Ok(true) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::WorkingTreeDirty,
                "the bound working tree is dirty; the dirty policy refuses the apply",
            ));
        }
        Err(failure) => {
            return AttemptStop::Settled(failure.settled("the working-tree status read"));
        }
    }

    // Preflight: the target branch must not be checked out anywhere.
    // Plumbing update-ref would silently move such a branch and turn the
    // user's tree into phantom staged changes — never acceptable.
    match worktrees_holding(&repository, &target_ref) {
        Ok(count) if count > 0 => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::Failed,
                format!(
                    "the target branch is checked out in {count} worktree(s); the engine \
                     never moves a user's checked-out branch"
                ),
            ));
        }
        Ok(_) => {}
        Err(failure) => {
            return AttemptStop::Settled(failure.settled("the worktree listing"));
        }
    }

    // The check-then-execute window closes here: a mirror advance after the
    // authorization above strands this attempt before anything ran.
    let guard = match FencingGuard::from_store(store) {
        Ok(guard) => guard,
        Err(error) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::Failed,
                format!("the fencing re-check could not read the mirror: {error}"),
            ));
        }
    };
    if let Err(rejection) = guard.verify_ticket(ticket) {
        return AttemptStop::Fencing(rejection);
    }

    execute_in_integration_worktree(
        &repository,
        record,
        request,
        integration_root,
        receipt_id,
        &current_head,
    )
}

/// Resolves the binding's canonical checkout and proves it confined.
fn resolve_binding_checkout(
    store: &mut DeviceStore,
    repository_binding_id: &str,
) -> Result<PathBuf, SettledAttempt> {
    let Some(mapping) = store.path_mapping(repository_binding_id).map_err(|error| {
        SettledAttempt::refused(
            ApplyResult::Failed,
            format!("the binding mapping read failed: {error}"),
        )
    })?
    else {
        return Err(SettledAttempt::refused(
            ApplyResult::CandidateMissing,
            "the repository binding maps to no local checkout",
        ));
    };
    let path = PathBuf::from(&mapping.canonical_path);
    if !path.exists() {
        return Err(SettledAttempt::refused(
            ApplyResult::CandidateMissing,
            "the bound checkout no longer exists on this device",
        ));
    }
    // The stored mapping must be the canonical spelling it was registered
    // as: fail closed on anything else instead of running Git in an
    // unproven directory.
    match ConfinedRoot::new(&path) {
        Ok(_) => Ok(path),
        Err(error) => Err(SettledAttempt::refused(
            ApplyResult::Failed,
            format!("the bound checkout is not the canonical path: {error}"),
        )),
    }
}

/// Creates the detached integration worktree from the validated target tip,
/// runs the strategy inside it, and publishes the result (or keeps the
/// conflict artifact, or cleans the attempt up).
fn execute_in_integration_worktree(
    repository: &Path,
    record: &CandidateLocalRefRecord,
    request: &CandidateApplyRequest,
    integration_root: &Path,
    receipt_id: &str,
    target_tip: &str,
) -> AttemptStop {
    let attempt_directory = integration_root
        .join(ARTIFACT_DIRECTORY)
        .join(&record.candidate_id)
        .join(receipt_id);
    let worktree = attempt_directory.join(WORKTREE_DIRECTORY);
    if let Err(error) = fs::create_dir_all(&attempt_directory) {
        return AttemptStop::Settled(io_failure_settled(
            &error,
            "the integration attempt directory",
        ));
    }
    // Detached at the validated tip: the integration worktree shares the
    // repository's object database but owns its own index and working
    // files, so nothing the strategy does can reach the user's checkout.
    let attach_status = git_command(repository)
        .args([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            "--",
            worktree.to_string_lossy().as_ref(),
            target_tip,
        ])
        .output();
    let attach_output = match attach_status {
        Ok(output) => output,
        Err(error) => {
            cleanup_attempt(repository, &attempt_directory, &worktree);
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::Failed,
                format!("git cannot be run for the integration worktree: {error}"),
            ));
        }
    };
    if !attach_output.status.success() {
        let stderr = String::from_utf8_lossy(&attach_output.stderr);
        cleanup_attempt(repository, &attempt_directory, &worktree);
        return AttemptStop::Settled(git_failure_settled(
            &stderr,
            "the integration worktree creation",
        ));
    }

    let settled = match run_strategy(&worktree, request) {
        Ok(resulting_commit) => publish_resulting_commit(
            repository,
            request,
            receipt_id,
            &attempt_directory,
            &worktree,
            resulting_commit,
        ),
        Err(mut settled) => {
            if settled.result == ApplyResult::MergeConflict {
                keep_conflict_artifact(
                    &worktree,
                    &attempt_directory,
                    record,
                    request,
                    receipt_id,
                    &mut settled,
                );
            } else {
                cleanup_attempt(repository, &attempt_directory, &worktree);
            }
            settled
        }
    };
    AttemptStop::Settled(settled)
}

/// Runs the requested strategy inside the integration worktree. `Ok` carries
/// the resulting commit (the integration worktree's new HEAD).
fn run_strategy(
    worktree: &Path,
    request: &CandidateApplyRequest,
) -> Result<String, SettledAttempt> {
    match request.strategy {
        // Refused in validation; kept total for exhaustiveness.
        ApplyStrategy::CreateBranch => Err(SettledAttempt::refused(
            ApplyResult::Failed,
            "branch creation belongs to the candidate branch engine",
        )),
        ApplyStrategy::FastForward => {
            let mut command = git_command(worktree);
            command.args(["merge", "--ff-only", "--", &request.candidate_ref]);
            finish_commit_step(worktree, command, "fast_forward")
        }
        ApplyStrategy::CherryPick => {
            let mut command = git_command(worktree);
            // The cherry-pick preserves the candidate commit's original
            // author; only the committer identity is the device client.
            command.env("GIT_COMMITTER_NAME", COMMITTER_NAME);
            command.env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL);
            command.args(["cherry-pick", "--", &request.candidate_ref]);
            finish_commit_step(worktree, command, "cherry_pick")
        }
        ApplyStrategy::Merge => {
            let mut command = git_command(worktree);
            // A merge commit is authored and committed by the device client.
            command.env("GIT_COMMITTER_NAME", COMMITTER_NAME);
            command.env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL);
            command.env("GIT_AUTHOR_NAME", COMMITTER_NAME);
            command.env("GIT_AUTHOR_EMAIL", COMMITTER_EMAIL);
            command.args([
                "merge",
                "--no-ff",
                "--no-edit",
                "--",
                &request.candidate_ref,
            ]);
            finish_commit_step(worktree, command, "merge")
        }
    }?;
    read_git_ref(worktree, "HEAD")
        .ok()
        .flatten()
        .ok_or_else(|| {
            SettledAttempt::refused(
                ApplyResult::Failed,
                "the integration worktree HEAD is unreadable after a successful strategy",
            )
        })
}

/// Classifies one commit-producing Git step: unmerged paths mean
/// `merge_conflict`, an operating-system permission refusal means
/// `permission_denied`, and everything else is `failed` with the local-only
/// Git detail.
fn finish_commit_step(
    worktree: &Path,
    mut command: Command,
    strategy: &str,
) -> Result<(), SettledAttempt> {
    let output = command.output().map_err(|error| {
        SettledAttempt::refused(ApplyResult::Failed, format!("git cannot be run: {error}"))
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if unmerged_paths(worktree).is_ok_and(|paths| !paths.is_empty()) {
        return Err(SettledAttempt {
            result: ApplyResult::MergeConflict,
            resulting_commit: None,
            conflict_artifact_ref: None,
            detail: format!("the {strategy} step reported conflicts"),
        });
    }
    let failure = GitFailure::refused(&stderr);
    let result = if failure.permission {
        ApplyResult::PermissionDenied
    } else {
        ApplyResult::Failed
    };
    Err(SettledAttempt::refused(
        result,
        format!("the {strategy} step failed: {}", failure.detail),
    ))
}

/// Publishes the integration commit with the compare-and-swap target ref
/// update: `update-ref <target> <resulting> <expectedHead>` — Git refuses
/// when the branch moved during the apply, closing the drift window. Either
/// way the integration worktree has served its purpose and is cleaned up.
fn publish_resulting_commit(
    repository: &Path,
    request: &CandidateApplyRequest,
    receipt_id: &str,
    attempt_directory: &Path,
    worktree: &Path,
    resulting_commit: String,
) -> SettledAttempt {
    let target_ref = format!("refs/heads/{}", request.target_branch);
    let status = git_command(repository)
        .arg("update-ref")
        .arg("-m")
        .arg(format!(
            "{APPLY_REF_MESSAGE_PREFIX} {} ({receipt_id})",
            request.candidate_ref
        ))
        .arg("--")
        .arg(&target_ref)
        .arg(&resulting_commit)
        .arg(&request.expected_head)
        .output();
    let settled = match status {
        Ok(output) if output.status.success() => SettledAttempt {
            result: ApplyResult::Applied,
            resulting_commit: Some(resulting_commit),
            conflict_artifact_ref: None,
            detail: format!("the target branch {target_ref} was updated"),
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            SettledAttempt::refused(
                ApplyResult::BaseStale,
                format!(
                    "the target branch moved during the apply; the compare-and-swap \
                     update was refused: {}",
                    stderr.trim()
                ),
            )
        }
        Err(error) => SettledAttempt::refused(
            ApplyResult::Failed,
            format!("git cannot be run for the ref update: {error}"),
        ),
    };
    cleanup_attempt(repository, attempt_directory, worktree);
    settled
}

/// Keeps the conflicted integration worktree as the conflict artifact: the
/// machine-readable summary is written next to it and the opaque artifact
/// reference is stamped onto the settlement. Nothing is ever written into
/// the user's checkout.
fn keep_conflict_artifact(
    worktree: &Path,
    attempt_directory: &Path,
    record: &CandidateLocalRefRecord,
    request: &CandidateApplyRequest,
    receipt_id: &str,
    settled: &mut SettledAttempt,
) {
    let unmerged = unmerged_paths(worktree).unwrap_or_default();
    let summary = serde_json::json!({
        "candidateRef": request.candidate_ref,
        "candidateCommit": record.candidate_commit,
        "targetBranch": request.target_branch,
        "expectedHead": request.expected_head,
        "strategy": strategy_name(request.strategy),
        "unmergedPaths": unmerged,
    });
    if let Err(error) = fs::write(
        attempt_directory.join(CONFLICT_SUMMARY_FILE),
        summary.to_string(),
    ) {
        settled.detail = format!(
            "{} (the conflict summary could not be written: {error})",
            settled.detail
        );
    }
    settled.conflict_artifact_ref = Some(format!(
        "{}/{}/{}",
        ARTIFACT_DIRECTORY, record.candidate_id, receipt_id
    ));
}

/// Removes one finished attempt's integration worktree and directory. The
/// user's checkout is never touched; best-effort cleanup never masks the
/// settled result. The emptied per-candidate parent directory is pruned
/// too, so a finished candidate leaves nothing behind at all.
fn cleanup_attempt(repository: &Path, attempt_directory: &Path, worktree: &Path) {
    let _ = git_command(repository)
        .args(["worktree", "remove", "--force", "--"])
        .arg(worktree)
        .output();
    let _ = git_command(repository).args(["worktree", "prune"]).output();
    let _ = fs::remove_dir_all(attempt_directory);
    if let Some(candidate_directory) = attempt_directory.parent() {
        let _ = fs::remove_dir(candidate_directory);
        if let Some(artifacts) = candidate_directory.parent() {
            let _ = fs::remove_dir(artifacts);
        }
    }
}

/// Lists the unmerged paths inside a (possibly conflicted) worktree.
fn unmerged_paths(worktree: &Path) -> Result<Vec<String>, GitFailure> {
    let mut command = git_command(worktree);
    command.args(["diff", "--name-only", "--diff-filter=U"]);
    let output = command
        .output()
        .map_err(|error| GitFailure::unavailable(&error))?;
    if !output.status.success() {
        return Err(GitFailure::refused(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Counts the worktrees that currently hold `target_ref` checked out (the
/// `branch` lines of `git worktree list --porcelain`).
fn worktrees_holding(repository: &Path, target_ref: &str) -> Result<usize, GitFailure> {
    let listing = probe_git(repository, &["worktree", "list", "--porcelain"])?;
    let count = listing
        .lines()
        .filter_map(|line| line.strip_prefix("branch "))
        .filter(|branch| *branch == target_ref)
        .count();
    Ok(count)
}

/// Whether the working tree of `repository` violates the dirty policy: any
/// `git status --porcelain` row (staged, unstaged, or untracked) is dirty —
/// the same projection the Git inspector reports.
fn working_tree_dirty(repository: &Path) -> Result<bool, GitFailure> {
    let status = probe_git(repository, &["status", "--porcelain"])?;
    Ok(!status.is_empty())
}

/// Runs one Git probe to a single answer line. Probe refusals are never the
/// missing-ref answer: only the dedicated rev-parse reader classifies
/// Git's exit-status-1 convention.
fn probe_git(repository: &Path, arguments: &[&str]) -> Result<String, GitFailure> {
    let mut command = git_command(repository);
    command.args(arguments);
    let output = command
        .output()
        .map_err(|error| GitFailure::unavailable(&error))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    Err(GitFailure::refused(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// A Git step failure with its local-only detail and its verdict facets.
struct GitFailure {
    detail: String,
    permission: bool,
}

impl GitFailure {
    fn unavailable(error: &std::io::Error) -> Self {
        Self {
            detail: format!("git cannot be run: {error}"),
            permission: error.kind() == std::io::ErrorKind::PermissionDenied,
        }
    }

    fn refused(stderr: &str) -> Self {
        let stderr = stderr.trim();
        Self {
            detail: stderr.to_owned(),
            permission: stderr.contains("Permission denied")
                || stderr.contains("permission denied"),
        }
    }

    /// Projects the failure onto the settled result vocabulary.
    fn settled(self, context: &str) -> SettledAttempt {
        let result = if self.permission {
            ApplyResult::PermissionDenied
        } else {
            ApplyResult::Failed
        };
        SettledAttempt::refused(
            result,
            if self.detail.is_empty() {
                context.to_owned()
            } else {
                format!("{context}: {}", self.detail)
            },
        )
    }
}

/// Reads one ref through Git: the exit-status-1 answer is the explicit
/// missing-ref answer, mirroring the registry's reading discipline.
fn read_git_ref(repository: &Path, ref_name: &str) -> Result<Option<String>, GitFailure> {
    let mut command = git_command(repository);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        ref_name,
    ]);
    let output = command
        .output()
        .map_err(|error| GitFailure::unavailable(&error))?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.trim().to_owned();
        if text.is_empty() || text.contains(['\r', '\n']) {
            return Err(GitFailure {
                detail: "git ref output is not a single identity".to_owned(),
                permission: false,
            });
        }
        return Ok(Some(text));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(GitFailure::refused(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// The shared Git invocation discipline: fail closed, prompt-free, no
/// system or global configuration surprises, no commit signing.
fn git_command(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args(["-c", "commit.gpgsign=false"]);
    command
}

/// Projects an `std::io` failure onto the settled vocabulary: an operating
/// system permission refusal is `permission_denied`, everything else
/// `failed`.
fn io_failure_settled(error: &std::io::Error, context: &str) -> SettledAttempt {
    let result = if error.kind() == std::io::ErrorKind::PermissionDenied {
        ApplyResult::PermissionDenied
    } else {
        ApplyResult::Failed
    };
    SettledAttempt::refused(result, format!("{context}: {error}"))
}

/// Projects a Git stderr refusal onto the settled vocabulary.
fn git_failure_settled(stderr: &str, context: &str) -> SettledAttempt {
    let failure = GitFailure::refused(stderr);
    let result = if failure.permission {
        ApplyResult::PermissionDenied
    } else {
        ApplyResult::Failed
    };
    SettledAttempt::refused(result, format!("{context}: {}", failure.detail))
}

/// Transitions the candidate registry row to `applied` or `failed` — the
/// device-side mirror of the server ledger's result-to-state projection.
/// Terminal rows can never be rewritten (the durable `CHECK` vocabulary plus
/// this predicate).
fn transition_candidate_state(
    store: &mut DeviceStore,
    candidate_id: &str,
    target_state: &str,
) -> Result<(), CandidateApplyError> {
    let updated = store
        .connection_mut()
        .map_err(CandidateApplyError::from)?
        .execute(
            "UPDATE candidate_local_refs SET local_state = ?2 \
             WHERE candidate_id = ?1 AND local_state NOT IN ('applied', 'discarded')",
            params![candidate_id, target_state],
        )
        .map_err(crate::store::sql_error)
        .map_err(CandidateApplyError::from)?;
    if updated == 1 {
        return Ok(());
    }
    Err(CandidateApplyError::store(format!(
        "the candidate {candidate_id} has no transitionable registry row"
    )))
}

/// Enqueues the durable `client.candidate.apply_result` frame: the receipt,
/// stamped with the authorized occupancy lease and the mirror revision the
/// command was authorized under.
fn enqueue_apply_result(
    daemon: &mut DeviceDaemon,
    ticket: &FencingTicket,
    receipt: &LocalApplyReceipt,
) -> Result<u64, CandidateApplyError> {
    let sequence = daemon
        .enqueue(ClientToServerMessage::CandidateApplyResult(
            ClientCandidateApplyResultPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: ticket.mirror_revision,
                        idempotency_key: format!(
                            "candidate-apply-{}",
                            receipt.local_apply_receipt_id
                        ),
                    },
                    occupancy_lease_id: ticket.occupancy_lease_id.clone(),
                    occupancy_fencing_token: ticket.occupancy_fencing_token,
                },
                receipt: receipt.clone(),
            },
        ))
        .map_err(|error| {
            CandidateApplyError::store(format!(
                "the apply result frame cannot enter the durable outbox: {error:?}"
            ))
        })?;
    Ok(sequence)
}

/// Validates one apply request before anything else runs.
fn validate_request(request: &CandidateApplyRequest) -> Result<(), CandidateApplyError> {
    require_non_empty(
        &request.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    candidate_id_from_ref(&request.candidate_ref)?;
    validate_target_branch(&request.target_branch)?;
    validate_commit_sha(&request.expected_head, "expected head")?;
    require_non_empty(
        &request.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    if request.strategy == ApplyStrategy::CreateBranch {
        return Err(CandidateApplyError::invalid(
            "branch creation is the candidate branch engine's strategy; the apply engine \
             executes fast_forward, cherry_pick, and merge",
        ));
    }
    Ok(())
}

/// Extracts the candidate id (the frozen commit) from the canonical
/// candidate reference.
fn candidate_id_from_ref(candidate_ref: &str) -> Result<String, CandidateApplyError> {
    let Some(candidate_id) = candidate_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateApplyError::invalid(format!(
            "candidate ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    validate_commit_sha(candidate_id, "candidate id")?;
    Ok(candidate_id.to_owned())
}

/// Validates the target branch shape accepted by `git check-ref-format` for
/// a branch under `refs/heads/`: canonical components, no climbs, no `@{`,
/// no `.lock` suffix, never the detached label.
fn validate_target_branch(branch: &str) -> Result<(), CandidateApplyError> {
    let shaped = !branch.is_empty()
        && branch.len() <= MAX_BRANCH_BYTES
        && branch != "HEAD"
        && branch.as_bytes()[0].is_ascii_alphanumeric()
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.ends_with('.')
        && !branch
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("lock"))
        && !branch
            .split('/')
            .any(|component| component.starts_with('.'))
        && !branch.contains("@{");
    if shaped {
        Ok(())
    } else {
        Err(CandidateApplyError::invalid(
            "target branch is not a canonical git branch name",
        ))
    }
}

/// Validates the frozen Git commit shape: full SHA-1 or SHA-256, lowercase
/// hex — the same shape the worker freeze, the registry, and the server
/// ledger enforce.
fn validate_commit_sha(value: &str, label: &str) -> Result<(), CandidateApplyError> {
    let valid = (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(CandidateApplyError::invalid(format!(
            "{label} is not a full lowercase git commit id"
        )))
    }
}

fn require_non_empty(
    value: &str,
    label: &str,
    max_bytes: usize,
) -> Result<(), CandidateApplyError> {
    if value.is_empty() {
        return Err(CandidateApplyError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(CandidateApplyError::invalid(format!(
            "{label} must contain at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn strategy_name(strategy: ApplyStrategy) -> &'static str {
    match strategy {
        ApplyStrategy::CreateBranch => "create_branch",
        ApplyStrategy::FastForward => "fast_forward",
        ApplyStrategy::CherryPick => "cherry_pick",
        ApplyStrategy::Merge => "merge",
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One labeled request mutation exercised by the validation tests.
    type RequestMutation = (&'static str, fn(&mut CandidateApplyRequest));

    fn base_request(strategy: ApplyStrategy) -> CandidateApplyRequest {
        CandidateApplyRequest {
            repository_binding_id: "rbd_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            candidate_ref: format!("{CANDIDATE_REF_PREFIX}{}", "a".repeat(40)),
            target_branch: "main".to_owned(),
            expected_head: "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776".to_owned(),
            strategy,
            occupancy_lease_id: "ocl_AAAA".to_owned(),
            occupancy_fencing_token: 3,
        }
    }

    #[test]
    fn request_validation_accepts_the_canonical_shapes() {
        assert_eq!(
            validate_request(&base_request(ApplyStrategy::CherryPick)),
            Ok(())
        );
        assert_eq!(
            validate_request(&base_request(ApplyStrategy::FastForward)),
            Ok(())
        );
        assert_eq!(
            validate_request(&base_request(ApplyStrategy::Merge)),
            Ok(())
        );
    }

    #[test]
    fn request_validation_refuses_malformed_shapes_and_foreign_strategies() {
        assert_eq!(
            validate_request(&base_request(ApplyStrategy::CreateBranch))
                .unwrap_err()
                .kind(),
            CandidateApplyErrorKind::InvalidInput,
            "branch creation is refused by the apply engine"
        );
        let mutations: [RequestMutation; 5] = [
            (
                "a foreign candidate namespace",
                |request: &mut CandidateApplyRequest| {
                    request.candidate_ref = "refs/heads/main".to_owned();
                },
            ),
            (
                "an abbreviated expected head",
                |request: &mut CandidateApplyRequest| {
                    request.expected_head = "0f9e8d7c".to_owned();
                },
            ),
            (
                "a dot-dot branch climb",
                |request: &mut CandidateApplyRequest| {
                    request.target_branch = "main/../secret".to_owned();
                },
            ),
            (
                "the detached branch label",
                |request: &mut CandidateApplyRequest| {
                    request.target_branch = "HEAD".to_owned();
                },
            ),
            (
                "an empty lease id",
                |request: &mut CandidateApplyRequest| {
                    request.occupancy_lease_id = String::new();
                },
            ),
        ];
        for (label, mutate) in mutations {
            let mut request = base_request(ApplyStrategy::Merge);
            mutate(&mut request);
            assert_eq!(
                validate_request(&request).unwrap_err().kind(),
                CandidateApplyErrorKind::InvalidInput,
                "{label} must be refused"
            );
        }
    }

    #[test]
    fn branch_validation_tracks_the_git_refname_rules() {
        for branch in ["main", "feature/x-1_y.z", "release/2.0"] {
            assert_eq!(validate_target_branch(branch), Ok(()), "{branch} is valid");
        }
        for branch in [
            "",
            "HEAD",
            "/main",
            "main/",
            "main..other",
            "main//other",
            ".hidden",
            "feature/.locked/x",
            "main.lock",
            "main@{x}",
            "main main",
        ] {
            assert!(
                validate_target_branch(branch).is_err(),
                "{branch} must be refused"
            );
        }
    }

    #[test]
    fn candidate_ids_extract_only_from_the_canonical_namespace() {
        let commit = "a".repeat(40);
        assert_eq!(
            candidate_id_from_ref(&format!("{CANDIDATE_REF_PREFIX}{commit}")),
            Ok(commit)
        );
        assert!(candidate_id_from_ref("refs/heads/main").is_err());
        assert!(candidate_id_from_ref(&format!("{CANDIDATE_REF_PREFIX}short")).is_err());
    }

    #[test]
    fn errors_carry_stable_categories_and_messages() {
        let error = CandidateApplyError::terminal("already applied");
        assert_eq!(error.kind(), CandidateApplyErrorKind::TerminalCandidate);
        assert_eq!(error.to_string(), "TerminalCandidate: already applied");
        let store_error = CandidateApplyError::from(DeviceStoreError::closed());
        assert_eq!(store_error.kind(), CandidateApplyErrorKind::Store);
        assert!(store_error.message().contains("closed"));
    }
}
