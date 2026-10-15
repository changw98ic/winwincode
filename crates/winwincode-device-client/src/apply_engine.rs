// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use winwincode_client_port::candidate_application::candidate_application_digest;
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ApplicationReconciliationOutcome, ClientApplicationReconciledPayload,
    ClientCandidateApplyResultPayload, ClientToServerMessage, CommandContext,
    OccupancyCommandContext,
};

use crate::application_journal::{application, finish_application, start_application};
use crate::candidate_registry::{CANDIDATE_REF_PREFIX, CandidateLocalRefRecord};
use crate::daemon::DeviceDaemon;
use crate::fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
use crate::identity::generate_prefixed_id;
use crate::path_confinement::ConfinedRoot;
use crate::store::{DeviceStore, DeviceStoreError};

const APPLY_REF_MESSAGE_PREFIX: &str = "winwincode: apply candidate";

const COMMITTER_NAME: &str = "WinWinCode Device Client";
const COMMITTER_EMAIL: &str = "device-client@winwincode.invalid";

const ARTIFACT_DIRECTORY: &str = "conflict-artifacts";

const CONFLICT_SUMMARY_FILE: &str = "conflict.json";

const WORKTREE_DIRECTORY: &str = "worktree";
const INTEGRATION_DIRECTORY: &str = "candidate-integrations";

pub const INTEGRATION_REF_PREFIX: &str = "refs/winwincode/integrations/";

const MAX_ID_BYTES: usize = 200;
const MAX_BRANCH_BYTES: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplyRequest {
    pub application_id: String,
    pub approval_id: String,
    pub candidate_digest: String,
    pub reconciled_retry: bool,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub target_branch: String,
    pub expected_head: String,
    pub strategy: ApplyStrategy,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateApplyErrorKind {
    InvalidInput,
    FencingRejected(FencingRejection),
    UnknownCandidate,
    TerminalCandidate,
    OutcomeUnknown,
    ReconciliationRequired,
    Store,
}

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

    pub(crate) fn store(message: impl Into<String>) -> Self {
        Self {
            kind: CandidateApplyErrorKind::Store,
            message: message.into(),
        }
    }

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplyOutcome {
    pub receipt: LocalApplyReceipt,
    pub frame_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateIntegrationRequest {
    pub repository_binding_id: String,
    pub source_candidate_refs: Vec<String>,
    pub expected_head: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCandidateIntegration {
    pub integration_ref: String,
    pub integration_commit: String,
    pub integration_digest: String,
    pub source_candidate_refs: Vec<String>,
    pub requires_integration_verification: bool,
    pub task_level_verdicts_authorize_delivery: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateIntegrationConflict {
    pub conflict_artifact_ref: String,
    pub conflicting_candidate_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateIntegrationOutcome {
    Prepared(PreparedCandidateIntegration),
    Conflict(CandidateIntegrationConflict),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryCommandDirectory {
    Repository,
    IsolatedRecoveryWorktree,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRecoveryCommand {
    pub directory: RecoveryCommandDirectory,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRecoveryGuidance {
    pub recovery_branch: String,
    pub commands: Vec<GitRecoveryCommand>,
}

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

enum AttemptStop {
    Fencing(FencingRejection),
    Settled(SettledAttempt),
}

pub fn apply_candidate_to_branch(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
    integration_root: &Path,
) -> Result<CandidateApplyOutcome, CandidateApplyError> {
    apply_candidate(daemon, request, integration_root, None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationCrashPoint {
    AfterStarted,
    AfterRepositoryCommit,
}

#[cfg(feature = "test-support")]
pub fn apply_candidate_with_crash_point(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
    integration_root: &Path,
    crash_point: ApplicationCrashPoint,
) -> Result<CandidateApplyOutcome, CandidateApplyError> {
    apply_candidate(daemon, request, integration_root, Some(crash_point))
}

#[allow(clippy::too_many_lines)]
fn apply_candidate(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
    integration_root: &Path,
    crash_point: Option<ApplicationCrashPoint>,
) -> Result<CandidateApplyOutcome, CandidateApplyError> {
    validate_request(request)?;
    let ticket = authorize_fencing(daemon, request)?;
    if crate::application_journal::blocked_application(
        daemon.store_mut(),
        &request.repository_binding_id,
        &request.candidate_ref,
    )?
    .is_some_and(|record| record.request.application_id != request.application_id)
    {
        return Err(CandidateApplyError {
            kind: CandidateApplyErrorKind::OutcomeUnknown,
            message: "an unresolved application blocks another apply for this candidate".into(),
        });
    }
    if let Some(existing) = application(daemon.store_mut(), &request.application_id)? {
        ensure_application_binding(&existing.request, request)?;
        if let Some(receipt) = existing.finished_receipt {
            return replay_finished_application(daemon, request, &ticket, receipt);
        }
        let observed = inspect_started_application(daemon.store_mut(), &existing.request)?;
        let observed = if existing.recovery_outcome.as_deref() == Some("outcome_unknown")
            && !matches!(observed, ApplicationRecovery::Applied(_))
        {
            ApplicationRecovery::OutcomeUnknown
        } else {
            observed
        };
        match observed {
            ApplicationRecovery::Applied(resulting_commit) => {
                let receipt = application_receipt(
                    &existing.request,
                    SettledAttempt {
                        result: ApplyResult::Applied,
                        resulting_commit: Some(resulting_commit),
                        conflict_artifact_ref: None,
                        detail: "the application commit already exists".into(),
                    },
                    &existing.started_at,
                );
                finish_application(daemon.store_mut(), &receipt)?;
                return replay_finished_application(daemon, request, &ticket, receipt);
            }
            ApplicationRecovery::NotStarted
                if request.reconciled_retry
                    && existing.recovery_outcome.as_deref() == Some("not_started") => {}
            ApplicationRecovery::NotStarted => {
                crate::application_journal::note_recovery_outcome(
                    daemon.store_mut(),
                    &request.application_id,
                    "not_started",
                )?;
                enqueue_application_reconciled(
                    daemon,
                    &ticket,
                    &existing.request,
                    &ApplicationRecovery::NotStarted,
                )?;
                return Err(CandidateApplyError {
                    kind: CandidateApplyErrorKind::ReconciliationRequired,
                    message: "the application has not started; a current server retry authorization is required".into(),
                });
            }
            ApplicationRecovery::OutcomeUnknown => {
                crate::application_journal::note_recovery_outcome(
                    daemon.store_mut(),
                    &request.application_id,
                    "outcome_unknown",
                )?;
                enqueue_application_reconciled(
                    daemon,
                    &ticket,
                    &existing.request,
                    &ApplicationRecovery::OutcomeUnknown,
                )?;
                return Err(CandidateApplyError {
                    kind: CandidateApplyErrorKind::OutcomeUnknown,
                    message: "the application outcome cannot be established; repository writes are blocked".into(),
                });
            }
        }
    }
    if let Some(replay) = pending_apply_result(daemon, request)? {
        return Ok(replay);
    }
    let record = load_candidate_for_request(daemon.store_mut(), request)?;
    if request.candidate_digest
        != candidate_application_digest(
            &request.repository_binding_id,
            &request.expected_head,
            &record.candidate_commit,
        )
    {
        return Err(CandidateApplyError::invalid(
            "the approval digest does not match the repository, baseline and frozen candidate",
        ));
    }
    let started_at = if let Some(existing) =
        application(daemon.store_mut(), &request.application_id)?
    {
        existing.started_at
    } else {
        if request.reconciled_retry {
            return Err(CandidateApplyError::invalid(
                "a retry authorization requires a retained application and a not_started reconciliation",
            ));
        }
        let started_at = now_rfc3339();
        start_application(daemon.store_mut(), request, &started_at)?;
        started_at
    };
    #[cfg(feature = "test-support")]
    maybe_test_application_crash(daemon.store_mut(), request, "after_started", None)?;
    if crash_point == Some(ApplicationCrashPoint::AfterStarted) {
        return Err(CandidateApplyError::store(
            "injected crash after application.started",
        ));
    }

    let stop = run_attempt(
        daemon.store_mut(),
        &record,
        request,
        integration_root,
        &request.application_id,
        &ticket,
    );
    let settled = match stop {
        AttemptStop::Fencing(rejection) => return Err(CandidateApplyError::fencing(rejection)),
        AttemptStop::Settled(settled) => settled,
    };
    #[cfg(feature = "test-support")]
    if settled.result == ApplyResult::Applied {
        maybe_test_application_crash(
            daemon.store_mut(),
            request,
            "after_repository_commit",
            settled.resulting_commit.as_deref(),
        )?;
    }
    if settled.result == ApplyResult::Applied
        && crash_point == Some(ApplicationCrashPoint::AfterRepositoryCommit)
    {
        return Err(CandidateApplyError::store(
            "injected crash after repository commit",
        ));
    }

    let target_state = if settled.result == ApplyResult::Applied {
        "applied"
    } else {
        "failed"
    };
    let receipt = application_receipt(request, settled, &started_at);
    finish_application(daemon.store_mut(), &receipt)?;
    let frame_sequence = enqueue_apply_result(daemon, &ticket, &receipt)?;
    transition_candidate_state(daemon.store_mut(), &record.candidate_id, target_state)?;
    Ok(CandidateApplyOutcome {
        receipt,
        frame_sequence,
    })
}

#[cfg(feature = "test-support")]
fn maybe_test_application_crash(
    store: &DeviceStore,
    request: &CandidateApplyRequest,
    point: &str,
    resulting_commit: Option<&str>,
) -> Result<(), CandidateApplyError> {
    if std::env::var("WWC_TEST_APPLICATION_CRASH_POINT").as_deref() != Ok(point)
        || std::env::var("WWC_TEST_APPLICATION_ID").is_ok_and(|id| id != request.application_id)
    {
        return Ok(());
    }
    let connection = store.connection()?;
    assert!(connection.is_autocommit());
    let synchronous: i64 = connection
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(crate::store::sql_error)?;
    assert!(synchronous >= 2);
    let retained =
        application(store, &request.application_id)?.expect("the started application is durable");
    assert!(retained.finished_receipt.is_none());
    if let Some(commit) = resulting_commit {
        let repository = resolve_binding_checkout(store, &request.repository_binding_id)
            .map_err(|failure| CandidateApplyError::store(failure.detail))?;
        assert_eq!(
            read_git_ref(
                &repository,
                &format!("refs/heads/{}", request.target_branch)
            )
            .map_err(|failure| CandidateApplyError::store(failure.detail))?
            .as_deref(),
            Some(commit)
        );
        let message = probe_git(&repository, &["show", "-s", "--format=%B", commit])
            .map_err(|failure| CandidateApplyError::store(failure.detail))?;
        assert!(
            message
                .lines()
                .any(|line| line == format!("WinWinCode-Application: {}", request.application_id))
        );
    }
    eprintln!(
        "WWC_TEST_APPLICATION_CRASH_POINT reached point={point} application_id={}",
        request.application_id
    );
    std::process::exit(86)
}

fn application_receipt(
    request: &CandidateApplyRequest,
    settled: SettledAttempt,
    started_at: &str,
) -> LocalApplyReceipt {
    LocalApplyReceipt {
        local_apply_receipt_id: request.application_id.clone(),
        candidate_ref: request.candidate_ref.clone(),
        repository_binding_id: request.repository_binding_id.clone(),
        target_branch: request.target_branch.clone(),
        expected_head: request.expected_head.clone(),
        strategy: request.strategy,
        result: settled.result,
        resulting_commit: settled.resulting_commit,
        conflict_artifact_ref: settled.conflict_artifact_ref,
        created_at: started_at.into(),
        revision: 1,
    }
}

fn ensure_application_binding(
    original: &CandidateApplyRequest,
    request: &CandidateApplyRequest,
) -> Result<(), CandidateApplyError> {
    if original.application_id == request.application_id
        && original.approval_id == request.approval_id
        && original.repository_binding_id == request.repository_binding_id
        && original.candidate_ref == request.candidate_ref
        && original.candidate_digest == request.candidate_digest
        && original.target_branch == request.target_branch
        && original.expected_head == request.expected_head
        && original.strategy == request.strategy
    {
        return Ok(());
    }
    Err(CandidateApplyError::invalid(
        "the application id is already bound to a different approval or candidate",
    ))
}

fn replay_finished_application(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
    ticket: &FencingTicket,
    receipt: LocalApplyReceipt,
) -> Result<CandidateApplyOutcome, CandidateApplyError> {
    let original = application(daemon.store_mut(), &request.application_id)?
        .ok_or_else(|| CandidateApplyError::store("finished application has no journal"))?;
    if original.request.occupancy_lease_id != request.occupancy_lease_id
        || original.request.occupancy_fencing_token != request.occupancy_fencing_token
    {
        let recovery = if let Some(commit) = &receipt.resulting_commit {
            ApplicationRecovery::Applied(commit.clone())
        } else {
            inspect_started_application(daemon.store_mut(), &original.request)?
        };
        let sequence =
            enqueue_application_reconciled(daemon, ticket, &original.request, &recovery)?;
        if receipt.result == ApplyResult::Applied {
            transition_candidate_state(
                daemon.store_mut(),
                &candidate_id_from_ref(&request.candidate_ref)?,
                "applied",
            )?;
        }
        return Ok(CandidateApplyOutcome {
            receipt,
            frame_sequence: sequence,
        });
    }
    if let Some(replay) = pending_apply_result(daemon, request)? {
        return Ok(replay);
    }
    let sequence = enqueue_apply_result(daemon, ticket, &receipt)?;
    transition_candidate_state(
        daemon.store_mut(),
        &candidate_id_from_ref(&request.candidate_ref)?,
        if receipt.result == ApplyResult::Applied {
            "applied"
        } else {
            "failed"
        },
    )?;
    Ok(CandidateApplyOutcome {
        receipt,
        frame_sequence: sequence,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationRecovery {
    Applied(String),
    NotStarted,
    OutcomeUnknown,
}

fn inspect_started_application(
    store: &DeviceStore,
    request: &CandidateApplyRequest,
) -> Result<ApplicationRecovery, CandidateApplyError> {
    let repository = resolve_binding_checkout(store, &request.repository_binding_id)
        .map_err(|failure| CandidateApplyError::store(failure.detail))?;
    let target_ref = format!("refs/heads/{}", request.target_branch);
    let target = read_git_ref(&repository, &target_ref)
        .map_err(|failure| CandidateApplyError::store(failure.detail))?;
    let Some(target) = target else {
        return Ok(ApplicationRecovery::OutcomeUnknown);
    };
    let ancestry = git_command(&repository)
        .args([
            "merge-base",
            "--is-ancestor",
            &request.expected_head,
            &target,
        ])
        .output()
        .map_err(|error| CandidateApplyError::store(error.to_string()))?;
    if ancestry.status.success() {
        let range = format!("{}..{target}", request.expected_head);
        let output = git_command(&repository)
            .args(["log", "--format=%H%x00%B%x00", &range, "--"])
            .output()
            .map_err(|error| CandidateApplyError::store(error.to_string()))?;
        if !output.status.success() {
            return Err(CandidateApplyError::store(
                "application recovery cannot inspect target history",
            ));
        }
        let trailer = format!("WinWinCode-Application: {}", request.application_id);
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split('\0');
        let mut found = None;
        while let (Some(commit), Some(message)) = (fields.next(), fields.next()) {
            if message.lines().any(|line| line == trailer) {
                if found.is_some() {
                    return Ok(ApplicationRecovery::OutcomeUnknown);
                }
                found = Some(commit.trim().to_owned());
            }
        }
        if let Some(commit) = found {
            return Ok(ApplicationRecovery::Applied(commit));
        }
    }
    if target != request.expected_head || !working_tree_dirty(&repository).is_ok_and(|dirty| !dirty)
    {
        return Ok(ApplicationRecovery::OutcomeUnknown);
    }
    let worktrees = probe_git(&repository, &["worktree", "list", "--porcelain"])
        .map_err(|failure| CandidateApplyError::store(failure.detail))?;
    if worktrees.lines().any(|line| {
        line.starts_with("worktree ")
            && Path::new(&line[9..])
                .components()
                .any(|part| part.as_os_str() == request.application_id.as_str())
    }) {
        return Ok(ApplicationRecovery::OutcomeUnknown);
    }
    Ok(ApplicationRecovery::NotStarted)
}

pub fn recover_started_applications(
    daemon: &mut DeviceDaemon,
) -> Result<Vec<(String, ApplicationRecovery)>, CandidateApplyError> {
    let records = crate::application_journal::unfinished_applications(daemon.store_mut())?;
    let mut recovered = Vec::with_capacity(records.len());
    for record in records {
        let observed = inspect_started_application(daemon.store_mut(), &record.request)?;
        let outcome = if record.recovery_outcome.as_deref() == Some("outcome_unknown")
            && !matches!(observed, ApplicationRecovery::Applied(_))
        {
            ApplicationRecovery::OutcomeUnknown
        } else {
            observed
        };
        if let ApplicationRecovery::Applied(commit) = &outcome {
            let receipt = application_receipt(
                &record.request,
                SettledAttempt {
                    result: ApplyResult::Applied,
                    resulting_commit: Some(commit.clone()),
                    conflict_artifact_ref: None,
                    detail: "startup recovered an existing application commit".into(),
                },
                &record.started_at,
            );
            finish_application(daemon.store_mut(), &receipt)?;
        } else {
            crate::application_journal::note_recovery_outcome(
                daemon.store_mut(),
                &record.request.application_id,
                if outcome == ApplicationRecovery::NotStarted {
                    "not_started"
                } else {
                    "outcome_unknown"
                },
            )?;
        }
        recovered.push((record.request.application_id, outcome));
    }
    Ok(recovered)
}

pub fn report_recovered_applications(
    daemon: &mut DeviceDaemon,
) -> Result<usize, CandidateApplyError> {
    recover_started_applications(daemon)?;
    let Some(mirror) = daemon.occupancy_mirror().cloned() else {
        return Ok(0);
    };
    let ticket = match FencingGuard::from_store(daemon.store_mut())?.authorize_command(
        FencedCommandKind::CandidateApply,
        &mirror.occupancy_lease_id,
        mirror.fencing_token,
    ) {
        FencingVerdict::Authorized(ticket) => ticket,
        FencingVerdict::Rejected(_) => return Ok(0),
    };
    let records = crate::application_journal::all_applications(daemon.store_mut())?;
    let mut reported = 0;
    for record in records {
        if record.reported_epoch == Some(ticket.occupancy_fencing_token) {
            continue;
        }
        let recovery = if let Some(commit) = record
            .finished_receipt
            .as_ref()
            .filter(|receipt| receipt.result == ApplyResult::Applied)
            .and_then(|receipt| receipt.resulting_commit.as_ref())
        {
            ApplicationRecovery::Applied(commit.clone())
        } else if record.recovery_outcome.as_deref() == Some("outcome_unknown") {
            ApplicationRecovery::OutcomeUnknown
        } else {
            inspect_started_application(daemon.store_mut(), &record.request)?
        };
        enqueue_application_reconciled(daemon, &ticket, &record.request, &recovery)?;
        crate::application_journal::mark_reported(
            daemon.store_mut(),
            &record.request.application_id,
            ticket.occupancy_fencing_token,
        )?;
        if matches!(recovery, ApplicationRecovery::Applied(_)) {
            let candidate_id = candidate_id_from_ref(&record.request.candidate_ref)?;
            if crate::candidate_registry::candidate_local_ref(daemon.store_mut(), &candidate_id)?
                .is_some()
            {
                transition_candidate_state(daemon.store_mut(), &candidate_id, "applied")?;
            }
        }
        reported += 1;
    }
    Ok(reported)
}

fn pending_apply_result(
    daemon: &mut DeviceDaemon,
    request: &CandidateApplyRequest,
) -> Result<Option<CandidateApplyOutcome>, CandidateApplyError> {
    let snapshot = daemon
        .outbox_snapshot()
        .map_err(|error| CandidateApplyError::store(error.to_string()))?;
    for frame in snapshot.frames.iter().rev() {
        let envelope: winwincode_client_port::messages::ClientToServerEnvelope =
            serde_json::from_slice(&frame.frame)
                .map_err(|error| CandidateApplyError::store(error.to_string()))?;
        let ClientToServerMessage::CandidateApplyResult(payload) = envelope.message else {
            continue;
        };
        let receipt = payload.receipt;
        if receipt.result == ApplyResult::Applied
            && receipt.local_apply_receipt_id == request.application_id
            && payload.occupancy.occupancy_lease_id == request.occupancy_lease_id
            && payload.occupancy.occupancy_fencing_token == request.occupancy_fencing_token
            && receipt.repository_binding_id == request.repository_binding_id
            && receipt.candidate_ref == request.candidate_ref
            && receipt.target_branch == request.target_branch
            && receipt.expected_head == request.expected_head
            && receipt.strategy == request.strategy
        {
            let id = candidate_id_from_ref(&request.candidate_ref)?;
            if crate::candidate_registry::candidate_local_ref(daemon.store_mut(), &id)?
                .is_some_and(|record| record.local_state != LocalCandidateState::Applied)
            {
                transition_candidate_state(
                    daemon.store_mut(),
                    &id,
                    if receipt.result == ApplyResult::Applied {
                        "applied"
                    } else {
                        "failed"
                    },
                )?;
            }
            return Ok(Some(CandidateApplyOutcome {
                receipt,
                frame_sequence: frame.sequence,
            }));
        }
    }
    Ok(None)
}

pub fn integrate_candidates_for_verification(
    daemon: &mut DeviceDaemon,
    request: &CandidateIntegrationRequest,
    integration_root: &Path,
) -> Result<CandidateIntegrationOutcome, CandidateApplyError> {
    validate_integration_request(request)?;
    let ticket = authorize_integration_fencing(daemon, request)?;
    let repository = resolve_binding_checkout(daemon.store_mut(), &request.repository_binding_id)
        .map_err(|settled| CandidateApplyError::store(settled.detail))?;
    let records = load_integration_candidates(daemon.store_mut(), request, &repository)?;
    if read_git_ref(&repository, &request.expected_head)
        .map_err(|failure| CandidateApplyError::store(failure.detail))?
        .as_deref()
        != Some(request.expected_head.as_str())
    {
        return Err(CandidateApplyError::invalid(
            "the integration base commit does not exist in the bound repository",
        ));
    }

    let guard = FencingGuard::from_store(daemon.store_mut())?;
    guard
        .verify_ticket(&ticket)
        .map_err(CandidateApplyError::fencing)?;

    execute_candidate_integration(&repository, request, &records, integration_root)
}

fn execute_candidate_integration(
    repository: &Path,
    request: &CandidateIntegrationRequest,
    records: &[CandidateLocalRefRecord],
    integration_root: &Path,
) -> Result<CandidateIntegrationOutcome, CandidateApplyError> {
    let integration_id = generate_prefixed_id("cit_")
        .map_err(|error| CandidateApplyError::store(format!("integration id: {error}")))?;
    let attempt_directory = integration_root
        .join(INTEGRATION_DIRECTORY)
        .join(&integration_id);
    let worktree = attempt_directory.join(WORKTREE_DIRECTORY);
    attach_integration_worktree(
        repository,
        &attempt_directory,
        &worktree,
        &request.expected_head,
    )?;
    if let Some(conflict) = combine_candidate_refs(
        repository,
        &attempt_directory,
        &worktree,
        request,
        records,
        &integration_id,
    )? {
        return Ok(CandidateIntegrationOutcome::Conflict(conflict));
    }
    let integration_commit = integration_head(repository, &attempt_directory, &worktree)?;
    let integration_ref = pin_integration_ref(
        repository,
        &attempt_directory,
        &worktree,
        &integration_commit,
    )?;
    cleanup_attempt(repository, &attempt_directory, &worktree);
    Ok(CandidateIntegrationOutcome::Prepared(
        PreparedCandidateIntegration {
            integration_digest: candidate_integration_digest(
                &request.expected_head,
                &request.source_candidate_refs,
                &integration_commit,
            ),
            integration_ref,
            integration_commit,
            source_candidate_refs: request.source_candidate_refs.clone(),
            requires_integration_verification: true,
            task_level_verdicts_authorize_delivery: false,
        },
    ))
}

fn attach_integration_worktree(
    repository: &Path,
    attempt_directory: &Path,
    worktree: &Path,
    expected_head: &str,
) -> Result<(), CandidateApplyError> {
    fs::create_dir_all(attempt_directory).map_err(|error| {
        CandidateApplyError::store(format!(
            "the integration directory cannot be created: {error}"
        ))
    })?;
    let attached = git_command(repository)
        .args(["worktree", "add", "--quiet", "--detach", "--"])
        .arg(worktree)
        .arg(expected_head)
        .output();
    match attached {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store(format!(
                "the integration worktree cannot be created: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
        Err(error) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store(format!(
                "git cannot create the integration worktree: {error}"
            )))
        }
    }
}

fn combine_candidate_refs(
    repository: &Path,
    attempt_directory: &Path,
    worktree: &Path,
    request: &CandidateIntegrationRequest,
    records: &[CandidateLocalRefRecord],
    integration_id: &str,
) -> Result<Option<CandidateIntegrationConflict>, CandidateApplyError> {
    for record in records {
        let mut command = git_command(worktree);
        command.env("GIT_COMMITTER_NAME", COMMITTER_NAME);
        command.env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL);
        command.args(["cherry-pick", "--", &record.candidate_ref]);
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => {
                cleanup_attempt(repository, attempt_directory, worktree);
                return Err(CandidateApplyError::store(format!(
                    "git cannot combine a candidate: {error}"
                )));
            }
        };
        if output.status.success() {
            continue;
        }
        if unmerged_paths(worktree).is_ok_and(|paths| !paths.is_empty()) {
            let summary = serde_json::json!({
                "expectedHead": request.expected_head,
                "sourceCandidateRefs": request.source_candidate_refs,
                "conflictingCandidateRef": record.candidate_ref,
            });
            fs::write(
                attempt_directory.join(CONFLICT_SUMMARY_FILE),
                summary.to_string(),
            )
            .map_err(|error| {
                CandidateApplyError::store(format!(
                    "the integration conflict summary cannot be written: {error}"
                ))
            })?;
            return Ok(Some(CandidateIntegrationConflict {
                conflict_artifact_ref: format!("{INTEGRATION_DIRECTORY}/{integration_id}"),
                conflicting_candidate_ref: record.candidate_ref.clone(),
            }));
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        cleanup_attempt(repository, attempt_directory, worktree);
        return Err(CandidateApplyError::store(format!(
            "the candidate integration failed: {detail}"
        )));
    }
    Ok(None)
}

fn integration_head(
    repository: &Path,
    attempt_directory: &Path,
    worktree: &Path,
) -> Result<String, CandidateApplyError> {
    match read_git_ref(worktree, "HEAD") {
        Ok(Some(commit)) => Ok(commit),
        Ok(None) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store("the combined commit is missing"))
        }
        Err(failure) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store(failure.detail))
        }
    }
}

fn pin_integration_ref(
    repository: &Path,
    attempt_directory: &Path,
    worktree: &Path,
    integration_commit: &str,
) -> Result<String, CandidateApplyError> {
    let integration_ref = format!("{INTEGRATION_REF_PREFIX}{integration_commit}");
    match read_git_ref(repository, &integration_ref) {
        Ok(Some(existing)) if existing == integration_commit => return Ok(integration_ref),
        Ok(Some(_)) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            return Err(CandidateApplyError::store(
                "the integration ref already exists with a different commit",
            ));
        }
        Ok(None) => {}
        Err(failure) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            return Err(CandidateApplyError::store(failure.detail));
        }
    }
    let published = git_command(repository)
        .args(["update-ref", "--create-reflog", "--", &integration_ref])
        .arg(integration_commit)
        .arg("")
        .output();
    match published {
        Ok(output) if output.status.success() => Ok(integration_ref),
        Ok(output) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store(format!(
                "the integration ref cannot be pinned: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
        Err(error) => {
            cleanup_attempt(repository, attempt_directory, worktree);
            Err(CandidateApplyError::store(format!(
                "git cannot pin the integration ref: {error}"
            )))
        }
    }
}

pub fn candidate_recovery_guidance(
    receipt: &LocalApplyReceipt,
) -> Result<Option<CandidateRecoveryGuidance>, CandidateApplyError> {
    if receipt.result != ApplyResult::Applied {
        return Ok(None);
    }
    validate_target_branch(&receipt.target_branch)?;
    validate_commit_sha(&receipt.expected_head, "expected head")?;
    let resulting_commit = receipt.resulting_commit.as_deref().ok_or_else(|| {
        CandidateApplyError::invalid("an applied receipt must name its resulting commit")
    })?;
    validate_commit_sha(resulting_commit, "resulting commit")?;
    require_non_empty(
        &receipt.local_apply_receipt_id,
        "local apply receipt id",
        MAX_ID_BYTES,
    )?;
    if !receipt
        .local_apply_receipt_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(CandidateApplyError::invalid(
            "local apply receipt id is not safe for a recovery branch",
        ));
    }
    let recovery_branch = format!(
        "winwincode/recovery/{}",
        receipt.local_apply_receipt_id.to_ascii_lowercase()
    );
    let message = format!(
        "Reverse WinWinCode apply {}",
        receipt.local_apply_receipt_id
    );
    Ok(Some(CandidateRecoveryGuidance {
        recovery_branch: recovery_branch.clone(),
        commands: vec![
            GitRecoveryCommand {
                directory: RecoveryCommandDirectory::Repository,
                arguments: vec![
                    "worktree".into(),
                    "add".into(),
                    "--detach".into(),
                    "RECOVERY_PATH".into(),
                    resulting_commit.into(),
                ],
            },
            GitRecoveryCommand {
                directory: RecoveryCommandDirectory::IsolatedRecoveryWorktree,
                arguments: vec!["switch".into(), "-c".into(), recovery_branch],
            },
            GitRecoveryCommand {
                directory: RecoveryCommandDirectory::IsolatedRecoveryWorktree,
                arguments: vec![
                    "read-tree".into(),
                    "--reset".into(),
                    "-u".into(),
                    receipt.expected_head.clone(),
                ],
            },
            GitRecoveryCommand {
                directory: RecoveryCommandDirectory::IsolatedRecoveryWorktree,
                arguments: vec!["commit".into(), "-m".into(), message],
            },
        ],
    }))
}

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

fn authorize_integration_fencing(
    daemon: &mut DeviceDaemon,
    request: &CandidateIntegrationRequest,
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

fn load_candidate_for_request(
    store: &mut DeviceStore,
    request: &CandidateApplyRequest,
) -> Result<CandidateLocalRefRecord, CandidateApplyError> {
    load_candidate_for_ref(
        store,
        &request.repository_binding_id,
        &request.candidate_ref,
    )
}

fn load_candidate_for_ref(
    store: &mut DeviceStore,
    repository_binding_id: &str,
    candidate_ref: &str,
) -> Result<CandidateLocalRefRecord, CandidateApplyError> {
    let candidate_id = candidate_id_from_ref(candidate_ref)?;
    let Some(record) = crate::candidate_registry::candidate_local_ref(store, &candidate_id)? else {
        return Err(CandidateApplyError::unknown_candidate(format!(
            "no retained candidate exists for {candidate_ref}"
        )));
    };
    if record.candidate_ref != candidate_ref {
        return Err(CandidateApplyError::unknown_candidate(
            "the stored candidate reference does not match the request",
        ));
    }
    if record.repository_binding_id != repository_binding_id {
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

fn load_integration_candidates(
    store: &mut DeviceStore,
    request: &CandidateIntegrationRequest,
    repository: &Path,
) -> Result<Vec<CandidateLocalRefRecord>, CandidateApplyError> {
    let mut records = Vec::with_capacity(request.source_candidate_refs.len());
    for candidate_ref in &request.source_candidate_refs {
        let record = load_candidate_for_ref(store, &request.repository_binding_id, candidate_ref)?;
        let resolved = read_git_ref(repository, candidate_ref)
            .map_err(|failure| CandidateApplyError::store(failure.detail))?;
        if resolved.as_deref() != Some(record.candidate_commit.as_str()) {
            return Err(CandidateApplyError::unknown_candidate(format!(
                "the candidate ref {candidate_ref} is missing or drifted"
            )));
        }
        records.push(record);
    }
    Ok(records)
}

#[allow(clippy::too_many_lines)]
fn run_attempt(
    store: &mut DeviceStore,
    record: &CandidateLocalRefRecord,
    request: &CandidateApplyRequest,
    integration_root: &Path,
    receipt_id: &str,
    ticket: &FencingTicket,
) -> AttemptStop {
    if request.expected_head != record.base_commit {
        return AttemptStop::Settled(SettledAttempt::refused(
            ApplyResult::BaseStale,
            "the approval baseline differs from the frozen candidate source baseline",
        ));
    }
    let repository = match resolve_binding_checkout(store, &request.repository_binding_id) {
        Ok(repository) => repository,
        Err(settled) => return AttemptStop::Settled(settled),
    };

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
    match git_command(&repository)
        .args([
            "merge-base",
            "--is-ancestor",
            &record.base_commit,
            &record.candidate_commit,
        ])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(_) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::BaseStale,
                "the frozen candidate does not descend from its retained source baseline",
            ));
        }
        Err(error) => {
            return AttemptStop::Settled(io_failure_settled(
                &error,
                "the candidate source ancestry check",
            ));
        }
    }

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

    match worktrees_holding(&repository, &target_ref) {
        Ok(count) if count > 0 && !(count == 1 && is_current_branch(&repository, &target_ref)) => {
            return AttemptStop::Settled(SettledAttempt::refused(
                ApplyResult::Failed,
                format!("the target branch is checked out in {count} other worktree(s)"),
            ));
        }
        Ok(_) => {}
        Err(failure) => {
            return AttemptStop::Settled(failure.settled("the worktree listing"));
        }
    }

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

fn resolve_binding_checkout(
    store: &DeviceStore,
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
    match ConfinedRoot::new(&path) {
        Ok(_) => Ok(path),
        Err(error) => Err(SettledAttempt::refused(
            ApplyResult::Failed,
            format!("the bound checkout is not the canonical path: {error}"),
        )),
    }
}

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

fn run_strategy(
    worktree: &Path,
    request: &CandidateApplyRequest,
) -> Result<String, SettledAttempt> {
    match request.strategy {
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
            let mut snapshot = git_command(worktree);
            snapshot.args(["read-tree", "--reset", "-u", &request.candidate_ref]);
            finish_commit_step(worktree, snapshot, "frozen candidate tree")?;
            let mut command = git_command(worktree);
            command.env("GIT_COMMITTER_NAME", COMMITTER_NAME);
            command.env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL);
            command.args([
                "commit",
                "--allow-empty",
                "--reuse-message",
                &request.candidate_ref,
            ]);
            finish_commit_step(worktree, command, "cherry_pick")
        }
        ApplyStrategy::Merge => {
            let mut command = git_command(worktree);
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
    let mut marker = git_command(worktree);
    marker.env("GIT_COMMITTER_NAME", COMMITTER_NAME);
    marker.env("GIT_COMMITTER_EMAIL", COMMITTER_EMAIL);
    if request.strategy == ApplyStrategy::FastForward {
        marker.env("GIT_AUTHOR_NAME", COMMITTER_NAME);
        marker.env("GIT_AUTHOR_EMAIL", COMMITTER_EMAIL);
        marker.args([
            "commit",
            "--allow-empty",
            "-m",
            "Apply WinWinCode candidate",
        ]);
    } else {
        marker.args(["commit", "--amend", "--no-edit"]);
    }
    marker.arg("--trailer").arg(format!(
        "WinWinCode-Application: {}",
        request.application_id
    ));
    finish_commit_step(worktree, marker, "application marker")?;
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

fn publish_resulting_commit(
    repository: &Path,
    request: &CandidateApplyRequest,
    receipt_id: &str,
    attempt_directory: &Path,
    worktree: &Path,
    resulting_commit: String,
) -> SettledAttempt {
    let target_ref = format!("refs/heads/{}", request.target_branch);
    let mut command = git_command(repository);
    if is_current_branch(repository, &target_ref) {
        if !working_tree_dirty(repository).is_ok_and(|dirty| !dirty)
            || read_git_ref(repository, "HEAD").ok().flatten().as_deref()
                != Some(request.expected_head.as_str())
        {
            cleanup_attempt(repository, attempt_directory, worktree);
            return SettledAttempt::refused(
                ApplyResult::BaseStale,
                "the checkout changed during integration",
            );
        }
        command.args(["merge", "--ff-only", "--no-edit", "--", &resulting_commit]);
    } else {
        command
            .arg("update-ref")
            .arg("-m")
            .arg(format!(
                "{APPLY_REF_MESSAGE_PREFIX} {} ({receipt_id})",
                request.candidate_ref
            ))
            .arg("--")
            .arg(&target_ref)
            .arg(&resulting_commit)
            .arg(&request.expected_head);
    }
    let status = command.output();
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

fn is_current_branch(repository: &Path, target_ref: &str) -> bool {
    git_command(repository)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == target_ref
        })
}

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

fn worktrees_holding(repository: &Path, target_ref: &str) -> Result<usize, GitFailure> {
    let listing = probe_git(repository, &["worktree", "list", "--porcelain"])?;
    let count = listing
        .lines()
        .filter_map(|line| line.strip_prefix("branch "))
        .filter(|branch| *branch == target_ref)
        .count();
    Ok(count)
}

fn working_tree_dirty(repository: &Path) -> Result<bool, GitFailure> {
    let status = probe_git(repository, &["status", "--porcelain"])?;
    Ok(!status.is_empty())
}

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

fn git_command(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args(["-c", "commit.gpgsign=false"]);
    command
}

fn io_failure_settled(error: &std::io::Error, context: &str) -> SettledAttempt {
    let result = if error.kind() == std::io::ErrorKind::PermissionDenied {
        ApplyResult::PermissionDenied
    } else {
        ApplyResult::Failed
    };
    SettledAttempt::refused(result, format!("{context}: {error}"))
}

fn git_failure_settled(stderr: &str, context: &str) -> SettledAttempt {
    let failure = GitFailure::refused(stderr);
    let result = if failure.permission {
        ApplyResult::PermissionDenied
    } else {
        ApplyResult::Failed
    };
    SettledAttempt::refused(result, format!("{context}: {}", failure.detail))
}

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
    if updated == 1
        || crate::candidate_registry::candidate_local_ref(store, candidate_id)?.is_some_and(
            |record| {
                matches!(
                    record.local_state,
                    LocalCandidateState::Applied | LocalCandidateState::Discarded
                )
            },
        )
    {
        return Ok(());
    }
    Err(CandidateApplyError::store(format!(
        "the candidate {candidate_id} has no transitionable registry row"
    )))
}

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

fn enqueue_application_reconciled(
    daemon: &mut DeviceDaemon,
    ticket: &FencingTicket,
    original: &CandidateApplyRequest,
    recovery: &ApplicationRecovery,
) -> Result<u64, CandidateApplyError> {
    let (outcome, result_commit) = match recovery {
        ApplicationRecovery::Applied(commit) => (
            ApplicationReconciliationOutcome::Applied,
            Some(commit.clone()),
        ),
        ApplicationRecovery::NotStarted => (ApplicationReconciliationOutcome::NotStarted, None),
        ApplicationRecovery::OutcomeUnknown => {
            (ApplicationReconciliationOutcome::OutcomeUnknown, None)
        }
    };
    daemon
        .enqueue(ClientToServerMessage::ApplicationReconciled(
            ClientApplicationReconciledPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: ticket.mirror_revision,
                        idempotency_key: generate_prefixed_id("application-reconciled-")
                            .map_err(|error| CandidateApplyError::store(error.to_string()))?,
                    },
                    occupancy_lease_id: ticket.occupancy_lease_id.clone(),
                    occupancy_fencing_token: ticket.occupancy_fencing_token,
                },
                application_id: original.application_id.clone(),
                approval_id: original.approval_id.clone(),
                original_epoch: original.occupancy_fencing_token,
                repository_binding_id: original.repository_binding_id.clone(),
                candidate_ref: original.candidate_ref.clone(),
                candidate_digest: original.candidate_digest.clone(),
                base_commit: original.expected_head.clone(),
                target_branch: original.target_branch.clone(),
                outcome,
                result_commit,
            },
        ))
        .map_err(|error| {
            CandidateApplyError::store(format!(
                "application reconciliation cannot enter the durable outbox: {error:?}"
            ))
        })
}

fn validate_request(request: &CandidateApplyRequest) -> Result<(), CandidateApplyError> {
    for (value, label, prefix) in [
        (request.application_id.as_str(), "application id", "lar_"),
        (request.approval_id.as_str(), "approval id", "apr_"),
    ] {
        require_non_empty(value, label, MAX_ID_BYTES)?;
        if !value.starts_with(prefix) || value.len() != prefix.len() + 26
            || !value[prefix.len()..].bytes().all(|byte| byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'))
        {
            return Err(CandidateApplyError::invalid(format!("{label} is invalid")));
        }
    }
    if !request
        .candidate_digest
        .strip_prefix("sha256:")
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return Err(CandidateApplyError::invalid("candidate digest is invalid"));
    }
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

fn validate_integration_request(
    request: &CandidateIntegrationRequest,
) -> Result<(), CandidateApplyError> {
    require_non_empty(
        &request.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    validate_commit_sha(&request.expected_head, "expected head")?;
    require_non_empty(
        &request.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    if !(2..=100).contains(&request.source_candidate_refs.len()) {
        return Err(CandidateApplyError::invalid(
            "candidate integration requires between 2 and 100 source candidates",
        ));
    }
    let mut unique = HashSet::with_capacity(request.source_candidate_refs.len());
    for candidate_ref in &request.source_candidate_refs {
        candidate_id_from_ref(candidate_ref)?;
        if !unique.insert(candidate_ref) {
            return Err(CandidateApplyError::invalid(
                "candidate integration source refs must be unique",
            ));
        }
    }
    Ok(())
}

fn candidate_integration_digest(
    expected_head: &str,
    source_candidate_refs: &[String],
    integration_commit: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.candidate-integration.v1\0");
    digest.update(expected_head.as_bytes());
    for candidate_ref in source_candidate_refs {
        digest.update(b"\0");
        digest.update(candidate_ref.as_bytes());
    }
    digest.update(b"\0");
    digest.update(integration_commit.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

fn candidate_id_from_ref(candidate_ref: &str) -> Result<String, CandidateApplyError> {
    let Some(candidate_id) = candidate_ref.strip_prefix(CANDIDATE_REF_PREFIX) else {
        return Err(CandidateApplyError::invalid(format!(
            "candidate ref is not inside the {CANDIDATE_REF_PREFIX} namespace"
        )));
    };
    validate_commit_sha(candidate_id, "candidate id")?;
    Ok(candidate_id.to_owned())
}

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
    crate::canonical_rfc3339(OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    type RequestMutation = (&'static str, fn(&mut CandidateApplyRequest));

    fn base_request(strategy: ApplyStrategy) -> CandidateApplyRequest {
        CandidateApplyRequest {
            application_id: "lar_AAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            approval_id: "apr_AAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            candidate_digest: format!("sha256:{}", "a".repeat(64)),
            reconciled_retry: false,
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
