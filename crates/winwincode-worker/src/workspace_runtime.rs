// SPDX-License-Identifier: Apache-2.0

//! Exact active-Job ownership for detached Worker workspaces.
//!
//! This deep module is the only mutable map from an authenticated active Job
//! to its private checkout. It resumes the original worktree after a process
//! crash, freezes writer changes through the canonical candidate builder, and
//! consumes the checkout at every terminal cleanup boundary.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use sha2::{Digest as _, Sha256};

use winwincode_change_batch::{
    ChangeBatchMutationStatus, ChangeBatchPolicy, LocalNoFollowFileSystem, PreparedChangeBatchPlan,
    PreparedPreimageJournalRecord, canonical_applied_file_summaries, derive_delta_digest,
    execute_prepared_change_batch, prepare_change_batch, recover_prepared_change_batch,
};
use winwincode_domain::{ExecutionJobId, Instant, WorkspaceRevision};
use winwincode_execution_port::diagnostic_parser::{
    build_diagnostic_baseline, compare_diagnostic_baselines, diagnostic_input,
    diagnostic_media_type, parse_diagnostics,
};
use winwincode_execution_port::generated::{
    AppliedFileSummary, ArtifactReference, ChangeBatchIdentity, ChangeBatchProgressEvent,
    ChangeBatchProgressState, ChangeBatchProposalEvent, ChangeBatchReceipt,
    ChangeBatchReceiptStatus, ExecutionJobReplacementAuthority, NormalizerReceipt,
    NormalizerReceiptStatus, ValidationProfileName, ValidationReceiptStatus,
};
use winwincode_execution_port::validation_config::{
    MAX_VALIDATION_CONFIGURATION_BYTES, VALIDATION_CONFIGURATION_PATH,
    parse_validation_configuration, resolve_validation_profile,
};

use crate::{
    ActiveJob, ActiveJobLifecycle,
    change_batch_journal::{
        ActiveBatchState, ChangeBatchJournal, ChangeBatchJournalError, JournalRetention,
        ObservationGateResult, ValidationDiagnosticEvaluation, WorkspaceBatchBarrier,
    },
    stage_product::{
        CandidateProductError, PreparedCandidateArtifact, prepare_candidate_artifact,
        prepare_verification_artifact,
    },
    validation_diagnostics::{ValidationDiagnosticDisposition, decide_validation_diagnostics},
    workspace::{
        WorkerWorkspace, WorkspaceCleanupReport, WorkspaceCloseReason, WorkspaceError,
        WorkspaceManager, WorkspaceProvenance,
    },
    workspace_phase::{
        ConfiguredPhasePlan, PhaseCancellation, PhaseProcessRunner, PhaseProcessStatus,
    },
    workspace_tree::{
        WorkspaceTreeComparison, WorkspaceTreeRestoreOutcome, WorkspaceTreeStore,
        WorkspaceWriterSnapshotOutcome,
    },
};

#[cfg(feature = "test-support")]
use crate::workspace::WorkspaceCreationRollbackFailure;
#[cfg(feature = "test-support")]
use crate::workspace::{WorkspaceCleanupInterruption, WorkspaceCreationInterruption};

/// Stable active-workspace failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobWorkspaceErrorCode {
    AuthorityMismatch,
    Workspace,
    Candidate,
    ChangeBatch,
}

/// Secret-safe active-workspace failure.
#[derive(Debug)]
pub struct JobWorkspaceError {
    code: JobWorkspaceErrorCode,
    message: &'static str,
}

impl JobWorkspaceError {
    fn new(code: JobWorkspaceErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn code(&self) -> JobWorkspaceErrorCode {
        self.code
    }
}

impl fmt::Display for JobWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for JobWorkspaceError {}

impl From<WorkspaceError> for JobWorkspaceError {
    fn from(_: WorkspaceError) -> Self {
        Self::new(
            JobWorkspaceErrorCode::Workspace,
            "detached Job workspace operation failed",
        )
    }
}

impl From<CandidateProductError> for JobWorkspaceError {
    fn from(_: CandidateProductError) -> Self {
        Self::new(
            JobWorkspaceErrorCode::Candidate,
            "detached Job candidate preparation failed",
        )
    }
}

impl From<ChangeBatchJournalError> for JobWorkspaceError {
    fn from(error: ChangeBatchJournalError) -> Self {
        Self::new(JobWorkspaceErrorCode::ChangeBatch, error.safe_message())
    }
}

/// Exact immutable input passed to the injected deterministic executor.
#[derive(Clone, Copy)]
pub struct ChangeBatchExecutionRequest<'request> {
    pub active: &'request ActiveJob,
    pub checkout: &'request Path,
    pub plan: &'request PreparedChangeBatchPlan,
}

/// Bounded result returned by an injected deterministic executor.
#[derive(Clone, Debug, PartialEq)]
pub enum ChangeBatchExecutionResult {
    Applied {
        files: Vec<AppliedFileSummary>,
        artifact_ref: Option<ArtifactReference>,
    },
    PartiallyApplied {
        files: Vec<AppliedFileSummary>,
        artifact_ref: Option<ArtifactReference>,
    },
    RolledBack {
        artifact_ref: Option<ArtifactReference>,
    },
    StateUncertain {
        files: Vec<AppliedFileSummary>,
        artifact_ref: Option<ArtifactReference>,
    },
}

/// Secret-free deterministic executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeBatchExecutorError;

impl fmt::Display for ChangeBatchExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChangeBatch executor failed")
    }
}

impl std::error::Error for ChangeBatchExecutorError {}

/// Exact validation output stream persisted before its bounded command receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationArtifactStream {
    Stdout,
    Stderr,
}

/// Replay-stable identity and bytes submitted to the private validation Artifact store.
#[derive(Clone, Copy, Debug)]
pub struct ValidationArtifactRequest<'request> {
    pub identity: &'request ChangeBatchIdentity,
    pub command_ordinal: usize,
    pub command_id: &'request str,
    pub stream: ValidationArtifactStream,
    pub media_type: &'static str,
    pub bytes: &'request [u8],
}

/// Secret-free failure from the authority-bound validation Artifact store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationArtifactError;

impl fmt::Display for ValidationArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("validation Artifact persistence failed")
    }
}

impl std::error::Error for ValidationArtifactError {}

/// Authority-bound, replay-idempotent raw validation output store.
pub trait ValidationArtifactPort: fmt::Debug + Send {
    /// Persists exact bytes and returns the canonical Artifact identity and digest.
    ///
    /// # Errors
    ///
    /// Exact-key changed bytes and storage failures are rejected.
    fn persist(
        &mut self,
        request: ValidationArtifactRequest<'_>,
    ) -> Result<ArtifactReference, ValidationArtifactError>;
}

#[derive(Debug, Default)]
struct UnavailableValidationArtifactPort;

impl ValidationArtifactPort for UnavailableValidationArtifactPort {
    fn persist(
        &mut self,
        _request: ValidationArtifactRequest<'_>,
    ) -> Result<ArtifactReference, ValidationArtifactError> {
        Err(ValidationArtifactError)
    }
}

/// One non-blocking deterministic executor operation.
pub type ChangeBatchExecutorFuture<'operation> = Pin<
    Box<
        dyn Future<Output = Result<ChangeBatchExecutionResult, ChangeBatchExecutorError>>
            + Send
            + 'operation,
    >,
>;

/// Explicit mutation port. Production defaults to a fail-closed implementation
/// until the real no-follow filesystem adapter is installed.
pub trait ChangeBatchExecutor: fmt::Debug + Send {
    /// Starts one new deterministic execution.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the executor cannot prove an exact result.
    fn execute<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation>;

    /// Reconciles an execution whose durable stream reached `apply_started`.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when recovery cannot prove an exact result.
    fn recover<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation>;

    /// Rolls back or proves the state of a cancelling execution.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when cancellation cannot prove an exact result.
    fn cancel<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation>;
}

/// Secret-free checkpoint computation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceTreePortError;

impl fmt::Display for WorkspaceTreePortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("workspace tree operation failed")
    }
}

impl std::error::Error for WorkspaceTreePortError {}

/// One non-blocking private-index tree computation.
pub type WorkspaceTreeFuture<'operation> = Pin<
    Box<dyn Future<Output = Result<WorkspaceRevision, WorkspaceTreePortError>> + Send + 'operation>,
>;

/// Bounded checkout-to-tree comparison result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceTreeCompareResult {
    Exact,
    Different,
    StateUncertain,
}

/// Bounded accepted-tree restoration result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceTreeRestoreResult {
    AlreadyAtTarget,
    ExactRestored,
    ExactRolledBack,
    StateUncertain,
}

pub type WorkspaceTreeCompareFuture<'operation> = Pin<
    Box<
        dyn Future<Output = Result<WorkspaceTreeCompareResult, WorkspaceTreePortError>>
            + Send
            + 'operation,
    >,
>;

pub type WorkspaceTreeRestoreFuture<'operation> = Pin<
    Box<
        dyn Future<Output = Result<WorkspaceTreeRestoreResult, WorkspaceTreePortError>>
            + Send
            + 'operation,
    >,
>;

pub type WorkspaceTreeBlobFuture<'operation> = Pin<
    Box<dyn Future<Output = Result<Option<Vec<u8>>, WorkspaceTreePortError>> + Send + 'operation>,
>;

/// Exact outcome after the bounded Writer command prefix.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceWriterSnapshotResult {
    Unchanged {
        revision: WorkspaceRevision,
        files: Vec<AppliedFileSummary>,
        delta_digest: winwincode_domain::Sha256Digest,
    },
    Normalized {
        revision: WorkspaceRevision,
        files: Vec<AppliedFileSummary>,
        delta_digest: winwincode_domain::Sha256Digest,
        changed_file_digests: Vec<winwincode_domain::Sha256Digest>,
    },
    ScopeViolation {
        observed_revision: WorkspaceRevision,
    },
    StateUncertain,
}

pub type WorkspaceWriterSnapshotFuture<'operation> = Pin<
    Box<
        dyn Future<Output = Result<WorkspaceWriterSnapshotResult, WorkspaceTreePortError>>
            + Send
            + 'operation,
    >,
>;

/// Injected checkpoint boundary. Production isolates Git work in `spawn_blocking`.
pub trait WorkspaceTreePort: fmt::Debug + Send {
    /// Reads a bounded regular-file blob from one exact accepted tree.
    fn read_blob_at_revision<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        revision: &'operation WorkspaceRevision,
        path: &'operation str,
        maximum_bytes: usize,
    ) -> WorkspaceTreeBlobFuture<'operation> {
        let _ = (checkout, state_root, revision, path, maximum_bytes);
        Box::pin(async { Ok(None) })
    }

    /// Proves Writer scope and derives the complete accepted-to-result delta.
    fn snapshot_writer_changes<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        accepted_base: &'operation WorkspaceRevision,
        pre_writer: &'operation WorkspaceRevision,
        applied_files: &'operation [AppliedFileSummary],
        allowed_writer_paths: &'operation [String],
    ) -> WorkspaceWriterSnapshotFuture<'operation> {
        let _ = (
            checkout,
            state_root,
            accepted_base,
            pre_writer,
            applied_files,
            allowed_writer_paths,
        );
        Box::pin(async { Ok(WorkspaceWriterSnapshotResult::StateUncertain) })
    }

    /// Computes and verifies the exact base-to-result Git tree.
    fn compute_tree<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        base: &'operation WorkspaceRevision,
        files: &'operation [AppliedFileSummary],
        delta_digest: &'operation winwincode_domain::Sha256Digest,
    ) -> WorkspaceTreeFuture<'operation>;

    /// Compares checkout bytes with one exact tree.
    fn compare_tree<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        expected: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeCompareFuture<'operation>;

    /// Restores an expected current tree to the accepted target after journaling.
    fn restore_tree<'operation>(
        &'operation mut self,
        workspace_id: &'operation str,
        checkout: &'operation Path,
        state_root: &'operation Path,
        journal_root: &'operation Path,
        expected_current: &'operation WorkspaceRevision,
        target: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeRestoreFuture<'operation>;
}

#[derive(Debug, Default)]
struct LocalWorkspaceTreePort;

impl WorkspaceTreePort for LocalWorkspaceTreePort {
    fn read_blob_at_revision<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        revision: &'operation WorkspaceRevision,
        path: &'operation str,
        maximum_bytes: usize,
    ) -> WorkspaceTreeBlobFuture<'operation> {
        let checkout = checkout.to_path_buf();
        let state_root = state_root.to_path_buf();
        let revision = revision.clone();
        let path = path.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                WorkspaceTreeStore::open(checkout, state_root)
                    .and_then(|store| store.read_blob_at_revision(&revision, &path, maximum_bytes))
                    .map_err(|_| WorkspaceTreePortError)
            })
            .await
            .map_err(|_| WorkspaceTreePortError)?
        })
    }

    fn snapshot_writer_changes<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        accepted_base: &'operation WorkspaceRevision,
        pre_writer: &'operation WorkspaceRevision,
        applied_files: &'operation [AppliedFileSummary],
        allowed_writer_paths: &'operation [String],
    ) -> WorkspaceWriterSnapshotFuture<'operation> {
        let checkout = checkout.to_path_buf();
        let state_root = state_root.to_path_buf();
        let accepted_base = accepted_base.clone();
        let pre_writer = pre_writer.clone();
        let applied_files = applied_files.to_vec();
        let allowed_writer_paths = allowed_writer_paths.to_vec();
        Box::pin(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                WorkspaceTreeStore::open(checkout, state_root)
                    .and_then(|store| {
                        store.snapshot_writer_changes(
                            &accepted_base,
                            &pre_writer,
                            &applied_files,
                            &allowed_writer_paths,
                        )
                    })
                    .map_err(|_| WorkspaceTreePortError)
            })
            .await
            .map_err(|_| WorkspaceTreePortError)??;
            Ok(match outcome {
                WorkspaceWriterSnapshotOutcome::Unchanged {
                    revision,
                    files,
                    delta_digest,
                } => WorkspaceWriterSnapshotResult::Unchanged {
                    revision,
                    files,
                    delta_digest,
                },
                WorkspaceWriterSnapshotOutcome::Normalized {
                    revision,
                    files,
                    delta_digest,
                    changed_file_digests,
                } => WorkspaceWriterSnapshotResult::Normalized {
                    revision,
                    files,
                    delta_digest,
                    changed_file_digests,
                },
                WorkspaceWriterSnapshotOutcome::ScopeViolation { observed_revision } => {
                    WorkspaceWriterSnapshotResult::ScopeViolation { observed_revision }
                }
                WorkspaceWriterSnapshotOutcome::StateUncertain => {
                    WorkspaceWriterSnapshotResult::StateUncertain
                }
            })
        })
    }

    fn compute_tree<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        base: &'operation WorkspaceRevision,
        files: &'operation [AppliedFileSummary],
        delta_digest: &'operation winwincode_domain::Sha256Digest,
    ) -> WorkspaceTreeFuture<'operation> {
        let checkout = checkout.to_path_buf();
        let state_root = state_root.to_path_buf();
        let base = base.clone();
        let files = files.to_vec();
        let delta_digest = delta_digest.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                WorkspaceTreeStore::open(checkout, state_root)
                    .and_then(|store| store.compute_tree(&base, &files, &delta_digest))
                    .map_err(|_| WorkspaceTreePortError)
            })
            .await
            .map_err(|_| WorkspaceTreePortError)?
        })
    }

    fn compare_tree<'operation>(
        &'operation mut self,
        checkout: &'operation Path,
        state_root: &'operation Path,
        expected: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeCompareFuture<'operation> {
        let checkout = checkout.to_path_buf();
        let state_root = state_root.to_path_buf();
        let expected = expected.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let comparison = WorkspaceTreeStore::open(checkout, state_root)
                    .and_then(|store| store.compare_tree(&expected))
                    .map_err(|_| WorkspaceTreePortError)?;
                Ok(match comparison {
                    WorkspaceTreeComparison::Exact => WorkspaceTreeCompareResult::Exact,
                    WorkspaceTreeComparison::Different => WorkspaceTreeCompareResult::Different,
                    WorkspaceTreeComparison::StateUncertain => {
                        WorkspaceTreeCompareResult::StateUncertain
                    }
                })
            })
            .await
            .map_err(|_| WorkspaceTreePortError)?
        })
    }

    fn restore_tree<'operation>(
        &'operation mut self,
        workspace_id: &'operation str,
        checkout: &'operation Path,
        state_root: &'operation Path,
        journal_root: &'operation Path,
        expected_current: &'operation WorkspaceRevision,
        target: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeRestoreFuture<'operation> {
        let workspace_id = workspace_id.to_owned();
        let checkout = checkout.to_path_buf();
        let state_root = state_root.to_path_buf();
        let journal_root = journal_root.to_path_buf();
        let expected_current = expected_current.clone();
        let target = target.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut journal =
                    ChangeBatchJournal::open(journal_root).map_err(|_| WorkspaceTreePortError)?;
                let outcome = WorkspaceTreeStore::open(checkout, state_root)
                    .and_then(|store| {
                        store.restore_tree(&workspace_id, &expected_current, &target, &mut journal)
                    })
                    .map_err(|_| WorkspaceTreePortError)?;
                Ok(match outcome {
                    WorkspaceTreeRestoreOutcome::AlreadyAtTarget => {
                        WorkspaceTreeRestoreResult::AlreadyAtTarget
                    }
                    WorkspaceTreeRestoreOutcome::ExactRestored => {
                        WorkspaceTreeRestoreResult::ExactRestored
                    }
                    WorkspaceTreeRestoreOutcome::ExactRolledBack => {
                        WorkspaceTreeRestoreResult::ExactRolledBack
                    }
                    WorkspaceTreeRestoreOutcome::StateUncertain => {
                        WorkspaceTreeRestoreResult::StateUncertain
                    }
                })
            })
            .await
            .map_err(|_| WorkspaceTreePortError)?
        })
    }
}

#[derive(Debug)]
struct LocalChangeBatchExecutor {
    journal_root: PathBuf,
}

impl ChangeBatchExecutor for LocalChangeBatchExecutor {
    fn execute<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        local_mutation_future(
            self.journal_root.clone(),
            request.checkout.to_path_buf(),
            request.plan.clone(),
            LocalMutationMode::Execute,
        )
    }

    fn recover<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        local_mutation_future(
            self.journal_root.clone(),
            request.checkout.to_path_buf(),
            request.plan.clone(),
            LocalMutationMode::Recover,
        )
    }

    fn cancel<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        local_mutation_future(
            self.journal_root.clone(),
            request.checkout.to_path_buf(),
            request.plan.clone(),
            LocalMutationMode::Cancel,
        )
    }
}

#[derive(Clone, Copy)]
enum LocalMutationMode {
    Execute,
    Recover,
    Cancel,
}

fn local_mutation_future(
    journal_root: PathBuf,
    checkout: PathBuf,
    plan: PreparedChangeBatchPlan,
    mode: LocalMutationMode,
) -> ChangeBatchExecutorFuture<'static> {
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            let mut journal =
                ChangeBatchJournal::open(journal_root).map_err(|_| ChangeBatchExecutorError)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|_| ChangeBatchExecutorError)?;
            let file_system = LocalNoFollowFileSystem::default();
            let outcome = match mode {
                LocalMutationMode::Execute => runtime.block_on(execute_prepared_change_batch(
                    &plan,
                    &checkout,
                    &mut journal,
                    &file_system,
                )),
                LocalMutationMode::Recover | LocalMutationMode::Cancel => {
                    let record = journal
                        .mutation_preimage_record(&plan)
                        .map_err(|_| ChangeBatchExecutorError)?;
                    match (mode, record) {
                        (LocalMutationMode::Recover, None) => {
                            runtime.block_on(execute_prepared_change_batch(
                                &plan,
                                &checkout,
                                &mut journal,
                                &file_system,
                            ))
                        }
                        (LocalMutationMode::Cancel, None) => {
                            return Ok(ChangeBatchExecutionResult::RolledBack {
                                artifact_ref: None,
                            });
                        }
                        (_, Some(record)) => {
                            let recovered = runtime.block_on(recover_prepared_change_batch(
                                &plan,
                                &checkout,
                                &record,
                                &file_system,
                            ));
                            match recovered {
                                Ok(outcome)
                                    if outcome.status()
                                        == ChangeBatchMutationStatus::PreMutation =>
                                {
                                    if matches!(mode, LocalMutationMode::Cancel) {
                                        return Ok(ChangeBatchExecutionResult::RolledBack {
                                            artifact_ref: None,
                                        });
                                    }
                                    runtime.block_on(execute_prepared_change_batch(
                                        &plan,
                                        &checkout,
                                        &mut journal,
                                        &file_system,
                                    ))
                                }
                                other => other,
                            }
                        }
                        (LocalMutationMode::Execute, _) => unreachable!("mode already matched"),
                    }
                }
            }
            .map_err(|_| ChangeBatchExecutorError)?;
            mutation_execution_result(&outcome)
        })
        .await
        .map_err(|_| ChangeBatchExecutorError)?
    })
}

fn mutation_execution_result(
    outcome: &winwincode_change_batch::ChangeBatchMutationOutcome,
) -> Result<ChangeBatchExecutionResult, ChangeBatchExecutorError> {
    match outcome.status() {
        ChangeBatchMutationStatus::Applied if outcome.delta_digest().is_some() => {
            Ok(ChangeBatchExecutionResult::Applied {
                files: outcome.files().to_vec(),
                artifact_ref: None,
            })
        }
        ChangeBatchMutationStatus::ExactRolledBack
            if outcome.files().is_empty() && outcome.delta_digest().is_none() =>
        {
            Ok(ChangeBatchExecutionResult::RolledBack { artifact_ref: None })
        }
        ChangeBatchMutationStatus::PartiallyApplied if outcome.delta_digest().is_some() => {
            Ok(ChangeBatchExecutionResult::PartiallyApplied {
                files: outcome.files().to_vec(),
                artifact_ref: None,
            })
        }
        ChangeBatchMutationStatus::StateUncertain if outcome.delta_digest().is_none() => {
            Ok(ChangeBatchExecutionResult::StateUncertain {
                files: outcome.files().to_vec(),
                artifact_ref: None,
            })
        }
        ChangeBatchMutationStatus::PreMutation
        | ChangeBatchMutationStatus::Applied
        | ChangeBatchMutationStatus::ExactRolledBack
        | ChangeBatchMutationStatus::PartiallyApplied
        | ChangeBatchMutationStatus::StateUncertain => Err(ChangeBatchExecutorError),
    }
}

async fn reconcile_local_mutation(
    checkout: PathBuf,
    plan: PreparedChangeBatchPlan,
    preimages: PreparedPreimageJournalRecord,
) -> Result<Option<ChangeBatchExecutionResult>, JobWorkspaceError> {
    let outcome = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|_| ChangeBatchExecutorError)?;
        runtime
            .block_on(recover_prepared_change_batch(
                &plan,
                &checkout,
                &preimages,
                &LocalNoFollowFileSystem::default(),
            ))
            .map_err(|_| ChangeBatchExecutorError)
    })
    .await
    .map_err(|_| {
        JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch recovery task could not complete",
        )
    })?
    .map_err(|_| {
        JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch recovery could not inspect the workspace",
        )
    })?;
    if outcome.status() == ChangeBatchMutationStatus::PreMutation {
        return Ok(None);
    }
    mutation_execution_result(&outcome).map(Some).map_err(|_| {
        JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch recovery outcome is invalid",
        )
    })
}

/// Durable typed values produced by one proposal execution or exact replay.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutedChangeBatch {
    pub progress: Vec<ChangeBatchProgressEvent>,
    pub receipt: ChangeBatchReceipt,
    pub replayed: bool,
}

/// Recovery hook invoked after an exact checkout is opened and before it is
/// exposed to Codex or any deterministic Writer.
pub trait ChangeBatchWorkspaceRecovery: fmt::Debug + Send {
    /// Reconciles private batch state against the still-exclusive checkout.
    ///
    /// # Errors
    ///
    /// Returns a bounded workspace error before the checkout becomes active.
    fn recover(
        &mut self,
        active: &ActiveJob,
        workspace: &mut WorkerWorkspace,
    ) -> Result<(), JobWorkspaceError>;
}

#[derive(Debug, Default)]
struct NoopChangeBatchWorkspaceRecovery;

impl ChangeBatchWorkspaceRecovery for NoopChangeBatchWorkspaceRecovery {
    fn recover(
        &mut self,
        _active: &ActiveJob,
        _workspace: &mut WorkerWorkspace,
    ) -> Result<(), JobWorkspaceError> {
        Ok(())
    }
}

/// Process-owned manager for all live detached Job workspaces.
#[derive(Debug)]
pub struct JobWorkspaceRuntime {
    manager: WorkspaceManager,
    active: HashMap<String, WorkerWorkspace>,
    change_batch_recovery: Box<dyn ChangeBatchWorkspaceRecovery>,
    change_batch_executor: Box<dyn ChangeBatchExecutor>,
    workspace_tree: Box<dyn WorkspaceTreePort>,
    validation_artifacts: Box<dyn ValidationArtifactPort>,
    change_batch_journal_root: PathBuf,
    change_batch_journal: ChangeBatchJournal,
    trusted_tool_path: std::ffi::OsString,
    trusted_rustup_home: Option<PathBuf>,
}

impl JobWorkspaceRuntime {
    /// Opens the controlled workspace and source roots.
    ///
    /// Existing directories are left untouched until an exact active Job asks
    /// to resume them; foreign orphan cleanup remains an explicit startup
    /// policy rather than deleting a resumable writer checkout.
    ///
    /// # Errors
    ///
    /// Returns the canonical workspace-root validation failure.
    pub fn open(
        root: impl Into<PathBuf>,
        source_root: impl Into<PathBuf>,
    ) -> Result<Self, JobWorkspaceError> {
        let root = root.into();
        let source_root = source_root.into();
        let journal_root = change_batch_journal_root(&root)?;
        let trusted_tool_path = trusted_tool_path_snapshot(&root, &source_root);
        let trusted_rustup_home = trusted_rustup_home_snapshot(&root, &source_root);
        Ok(Self {
            manager: WorkspaceManager::open(root, source_root)?,
            active: HashMap::new(),
            change_batch_recovery: Box::new(NoopChangeBatchWorkspaceRecovery),
            change_batch_executor: Box::new(LocalChangeBatchExecutor {
                journal_root: journal_root.clone(),
            }),
            workspace_tree: Box::new(LocalWorkspaceTreePort),
            validation_artifacts: Box::new(UnavailableValidationArtifactPort),
            change_batch_journal: ChangeBatchJournal::open(journal_root.clone())?,
            change_batch_journal_root: journal_root,
            trusted_tool_path,
            trusted_rustup_home,
        })
    }

    /// Installs the sole deterministic batch recovery hook.
    #[must_use]
    pub fn with_change_batch_recovery(
        mut self,
        recovery: impl ChangeBatchWorkspaceRecovery + 'static,
    ) -> Self {
        self.change_batch_recovery = Box::new(recovery);
        self
    }

    /// Installs the sole deterministic mutation executor.
    #[must_use]
    pub fn with_change_batch_executor(
        mut self,
        executor: impl ChangeBatchExecutor + 'static,
    ) -> Self {
        self.change_batch_executor = Box::new(executor);
        self
    }

    /// Installs the sole exact tree checkpoint port.
    #[must_use]
    pub fn with_workspace_tree_port(mut self, port: impl WorkspaceTreePort + 'static) -> Self {
        self.workspace_tree = Box::new(port);
        self
    }

    /// Installs the authority-bound raw validation Artifact store.
    #[must_use]
    pub fn with_validation_artifact_port(
        mut self,
        port: impl ValidationArtifactPort + 'static,
    ) -> Self {
        self.validation_artifacts = Box::new(port);
        self
    }

    /// Creates or resumes the one exact checkout for an active Job.
    ///
    /// Repeated calls for the same authority return the original path. A Job ID
    /// whose active lease/session/thread changed is rejected without opening a
    /// second checkout in this process.
    ///
    /// # Errors
    ///
    /// Rejects changed authority or any create/recovery failure.
    pub fn open_for_job(
        &mut self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
    ) -> Result<PathBuf, JobWorkspaceError> {
        if let Some(workspace) = self.active.get(&active.job.job_id.0) {
            if !same_authority(workspace.provenance(), active) {
                return Err(authority_error());
            }
            self.change_batch_journal.retain_workspace_barrier(
                workspace.id(),
                &workspace.resolved_source_tree(),
                &active.lease.issued_at,
            )?;
            return Ok(workspace.layout().checkout().to_path_buf());
        }
        let mut workspace = self.manager.create_or_recover(active, replacement)?;
        self.change_batch_recovery.recover(active, &mut workspace)?;
        self.change_batch_journal.retain_workspace_barrier(
            workspace.id(),
            &workspace.resolved_source_tree(),
            &active.lease.issued_at,
        )?;
        let checkout = workspace.layout().checkout().to_path_buf();
        self.active.insert(active.job.job_id.0.clone(), workspace);
        Ok(checkout)
    }

    /// Opens a checkout only after every durable interrupted batch is reconciled.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, corrupt recovery state, or any workspace whose
    /// exact final state cannot be proven.
    #[allow(clippy::too_many_lines)]
    pub async fn open_for_job_recovering(
        &mut self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
        now: &Instant,
    ) -> Result<PathBuf, JobWorkspaceError> {
        let records = self
            .change_batch_journal
            .records_for_job(&active.job.job_id)?;
        for record in &records {
            if !same_change_batch_lease_authority(&record.event, active) {
                return Err(authority_error());
            }
            if record.receipt.as_ref().is_some_and(|receipt| {
                matches!(
                    receipt.status,
                    ChangeBatchReceiptStatus::PartiallyApplied
                        | ChangeBatchReceiptStatus::StateUncertain
                )
            }) {
                return Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch workspace remains quarantined",
                ));
            }
        }
        let checkout = self.open_for_job(active, replacement)?;
        for record in records {
            if record.receipt.is_some() {
                continue;
            }
            let plan = prepare_change_batch(&record.event, ChangeBatchPolicy::default()).map_err(
                |_| {
                    JobWorkspaceError::new(
                        JobWorkspaceErrorCode::ChangeBatch,
                        "ChangeBatch recovery plan cannot be rebuilt",
                    )
                },
            )?;
            let Some(preimages) = self.change_batch_journal.mutation_preimage_record(&plan)? else {
                continue;
            };
            let outcome = reconcile_local_mutation(checkout.clone(), plan, preimages).await?;
            let Some(result) = outcome else {
                continue;
            };
            let (mut progress, _) =
                prepare_execution_progress(&mut self.change_batch_journal, &record.event, now)?;
            let receipt = match result {
                ChangeBatchExecutionResult::Applied {
                    files,
                    artifact_ref,
                } => {
                    let workspace = self
                        .active
                        .get(&active.job.job_id.0)
                        .ok_or_else(authority_error)?;
                    finalize_applied_checkpoint(
                        &mut self.change_batch_journal,
                        self.workspace_tree.as_mut(),
                        workspace,
                        &mut progress,
                        &record.event,
                        &record.base_revision,
                        &files,
                        artifact_ref,
                        now,
                    )
                    .await?
                }
                ChangeBatchExecutionResult::PartiallyApplied {
                    files,
                    artifact_ref,
                } => {
                    let workspace = self
                        .active
                        .get(&active.job.job_id.0)
                        .ok_or_else(authority_error)?;
                    finalize_partial_checkpoint(
                        &mut self.change_batch_journal,
                        self.workspace_tree.as_mut(),
                        workspace,
                        &mut progress,
                        &record.event,
                        &record.base_revision,
                        &files,
                        artifact_ref,
                        now,
                    )
                    .await?
                }
                ChangeBatchExecutionResult::RolledBack { artifact_ref } => {
                    finalize_rollback_workspace(
                        &mut self.change_batch_journal,
                        self.active
                            .get(&active.job.job_id.0)
                            .ok_or_else(authority_error)?,
                        &mut progress,
                        &record.event,
                        &record.base_revision,
                        artifact_ref,
                        now,
                    )?
                }
                ChangeBatchExecutionResult::StateUncertain {
                    files,
                    artifact_ref,
                } => finalize_uncertain_workspace(
                    &mut self.change_batch_journal,
                    self.active
                        .get(&active.job.job_id.0)
                        .ok_or_else(authority_error)?,
                    &mut progress,
                    &record.event,
                    &record.base_revision,
                    &files,
                    artifact_ref,
                    ActiveBatchState::Applying,
                    now,
                )?,
            };
            if matches!(
                receipt.status,
                ChangeBatchReceiptStatus::PartiallyApplied
                    | ChangeBatchReceiptStatus::StateUncertain
            ) {
                return Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch recovery could not prove a safe workspace",
                ));
            }
        }
        self.reconcile_open_workspace_tree(&active.job.job_id, now)
            .await?;
        Ok(checkout)
    }

    async fn reconcile_open_workspace_tree(
        &mut self,
        job_id: &ExecutionJobId,
        now: &Instant,
    ) -> Result<(), JobWorkspaceError> {
        let workspace = self.active.get(&job_id.0).ok_or_else(authority_error)?;
        let workspace_id = workspace.id().to_owned();
        let checkout = workspace.layout().checkout().to_path_buf();
        let state_root = workspace.layout().sandbox().join("workspace-tree");
        let source_revision = workspace.resolved_source_tree();
        let barrier = self
            .change_batch_journal
            .workspace_barrier(&workspace_id)?
            .ok_or_else(authority_error)?;
        if barrier.state == ActiveBatchState::Idle {
            return Ok(());
        }
        if barrier.state == ActiveBatchState::Quarantined {
            return Err(JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch workspace remains quarantined",
            ));
        }
        if barrier.state == ActiveBatchState::RollbackPending {
            return self
                .recover_pending_workspace_restore(
                    &workspace_id,
                    &checkout,
                    &state_root,
                    &barrier,
                    now,
                )
                .await;
        }
        let expected = barrier
            .checkpoint_revision
            .as_ref()
            .filter(|_| {
                !matches!(
                    barrier.state,
                    ActiveBatchState::RolledBack | ActiveBatchState::RepairRequired
                )
            })
            .unwrap_or(&barrier.accepted_revision);
        match self
            .workspace_tree
            .compare_tree(&checkout, &state_root, expected)
            .await
            .map_err(|_| {
                JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch workspace tree cannot be compared",
                )
            })? {
            WorkspaceTreeCompareResult::Exact => Ok(()),
            WorkspaceTreeCompareResult::Different
                if self
                    .workspace_tree
                    .compare_tree(&checkout, &state_root, &source_revision)
                    .await
                    .ok()
                    == Some(WorkspaceTreeCompareResult::Exact) =>
            {
                match self
                    .workspace_tree
                    .restore_tree(
                        &workspace_id,
                        &checkout,
                        &state_root,
                        &self.change_batch_journal_root,
                        &source_revision,
                        &barrier.accepted_revision,
                    )
                    .await
                    .map_err(|_| {
                        JobWorkspaceError::new(
                            JobWorkspaceErrorCode::ChangeBatch,
                            "ChangeBatch accepted tree cannot be restored",
                        )
                    })? {
                    WorkspaceTreeRestoreResult::AlreadyAtTarget
                    | WorkspaceTreeRestoreResult::ExactRestored => Ok(()),
                    WorkspaceTreeRestoreResult::ExactRolledBack
                    | WorkspaceTreeRestoreResult::StateUncertain => Err(JobWorkspaceError::new(
                        JobWorkspaceErrorCode::ChangeBatch,
                        "ChangeBatch workspace tree remains uncertain",
                    )),
                }
            }
            WorkspaceTreeCompareResult::Different | WorkspaceTreeCompareResult::StateUncertain => {
                Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch workspace tree differs from durable state",
                ))
            }
        }
    }

    async fn recover_pending_workspace_restore(
        &mut self,
        workspace_id: &str,
        checkout: &Path,
        state_root: &Path,
        barrier: &WorkspaceBatchBarrier,
        now: &Instant,
    ) -> Result<(), JobWorkspaceError> {
        let checkpoint = barrier
            .checkpoint_revision
            .as_ref()
            .ok_or_else(authority_error)?;
        let batch_id = barrier
            .active_batch_id
            .as_ref()
            .ok_or_else(authority_error)?;
        let record = self
            .change_batch_journal
            .load(batch_id)?
            .ok_or_else(authority_error)?;
        let mut progress = self.change_batch_journal.progress_events(batch_id)?;
        let restored = self
            .workspace_tree
            .restore_tree(
                workspace_id,
                checkout,
                state_root,
                &self.change_batch_journal_root,
                checkpoint,
                &barrier.accepted_revision,
            )
            .await
            .map_err(|_| {
                JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch pending accepted-tree restore failed",
                )
            })?;
        match restored {
            WorkspaceTreeRestoreResult::AlreadyAtTarget
            | WorkspaceTreeRestoreResult::ExactRestored => {
                retain_rollback_completion(
                    &mut self.change_batch_journal,
                    workspace_id,
                    &record.event,
                    &mut progress,
                    now,
                )?;
                Ok(())
            }
            WorkspaceTreeRestoreResult::ExactRolledBack
            | WorkspaceTreeRestoreResult::StateUncertain => {
                retain_restore_failure(
                    &mut self.change_batch_journal,
                    workspace_id,
                    &record.event,
                    &mut progress,
                    now,
                )?;
                Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch pending accepted-tree restore remains uncertain",
                ))
            }
        }
    }

    /// Leaves the exact durable workspace creation state at one crash point.
    ///
    /// # Errors
    ///
    /// Returns the injected interruption or any authority/filesystem failure.
    #[cfg(feature = "test-support")]
    pub fn interrupt_workspace_creation_for_test(
        &mut self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
        interruption: WorkspaceCreationInterruption,
    ) -> Result<(), JobWorkspaceError> {
        self.manager
            .create_or_recover_interrupted(active, replacement, interruption)
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Leaves an exact workspace at one durable cleanup crash point.
    ///
    /// # Errors
    ///
    /// Returns the injected interruption or any authority/filesystem failure.
    #[cfg(feature = "test-support")]
    pub fn interrupt_workspace_cleanup_for_test(
        &mut self,
        job_id: &ExecutionJobId,
        reason: WorkspaceCloseReason,
        interruption: WorkspaceCleanupInterruption,
    ) -> Result<(), JobWorkspaceError> {
        self.active
            .get_mut(&job_id.0)
            .ok_or_else(authority_error)?
            .close_in_place_interrupted(reason, interruption)
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Leaves a Creating intent after a normal creation error whose rollback fails.
    ///
    /// # Errors
    ///
    /// Returns the injected rollback failure or any authority/filesystem failure.
    #[cfg(feature = "test-support")]
    pub fn fail_workspace_creation_rollback_for_test(
        &mut self,
        active: &ActiveJob,
        failure: WorkspaceCreationRollbackFailure,
    ) -> Result<(), JobWorkspaceError> {
        self.manager
            .create_with_failed_rollback_for_test(active, failure)
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Freezes and verifies the live writer checkout into a Candidate upload.
    ///
    /// # Errors
    ///
    /// Rejects a missing/foreign workspace, non-writer or cancelling Job,
    /// unchanged checkout, or any Git verification failure.
    pub fn prepare_candidate(
        &mut self,
        active: &ActiveJob,
    ) -> Result<PreparedCandidateArtifact, JobWorkspaceError> {
        let workspace = self
            .active
            .get_mut(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        if !same_authority(workspace.provenance(), active) {
            return Err(authority_error());
        }
        match prepare_candidate_artifact(active, workspace) {
            Ok(prepared) => Ok(prepared),
            Err(error) => Err(error.into()),
        }
    }

    /// Captures and verifies the clean candidate checkout used by a read-only
    /// reviewer or verifier into its own Candidate upload.
    ///
    /// # Errors
    ///
    /// Rejects a missing/foreign workspace, an invalid verification Job, a
    /// dirty checkout, or any Git/authority failure.
    pub fn prepare_verification(
        &mut self,
        active: &ActiveJob,
    ) -> Result<PreparedCandidateArtifact, JobWorkspaceError> {
        let workspace = self
            .active
            .get(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        if !same_authority(workspace.provenance(), active) {
            return Err(authority_error());
        }
        match prepare_verification_artifact(active, workspace) {
            Ok(prepared) => Ok(prepared),
            Err(error) => Err(error.into()),
        }
    }

    /// Executes or replays one canonical proposal without ending its Job.
    ///
    /// The proposal, every progress transition, and the final receipt are
    /// durable before this method returns. A replay with a terminal receipt
    /// never invokes the executor again. A replay interrupted after
    /// `apply_started` uses the executor's explicit recovery entry point.
    ///
    /// # Errors
    ///
    /// Rejects foreign authority, changed intent bytes, invalid plans/results,
    /// or unavailable durable journal state.
    #[allow(clippy::too_many_lines)]
    pub async fn execute_change_batch(
        &mut self,
        active: &ActiveJob,
        event: &ChangeBatchProposalEvent,
        now: &Instant,
    ) -> Result<ExecutedChangeBatch, JobWorkspaceError> {
        let workspace = self
            .active
            .get(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        let accepted_revision = self
            .change_batch_journal
            .workspace_barrier(workspace.id())?
            .map_or_else(
                || workspace.resolved_source_tree(),
                |barrier| barrier.accepted_revision,
            );
        if !same_authority(workspace.provenance(), active)
            || !same_change_batch_authority(event, active, &accepted_revision)
        {
            return Err(authority_error());
        }
        let plan = prepare_change_batch(event, ChangeBatchPolicy::default()).map_err(|_| {
            JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch proposal cannot be planned",
            )
        })?;
        let retention = self.change_batch_journal.retain_claimed_intent(
            workspace.id(),
            event,
            &accepted_revision,
            plan.plan_digest(),
            now,
        )?;
        let existing_receipt = self
            .change_batch_journal
            .load(&event.identity.batch_id)?
            .and_then(|record| record.receipt);
        if let Some(receipt) = existing_receipt.as_ref()
            && self
                .change_batch_journal
                .phase_record(&event.identity.batch_id)?
                .is_none()
        {
            return Ok(ExecutedChangeBatch {
                progress: self
                    .change_batch_journal
                    .progress_events(&event.identity.batch_id)?,
                receipt: receipt.clone(),
                replayed: true,
            });
        }
        let resolved_phases = load_configured_phase_plan(
            self.workspace_tree.as_mut(),
            workspace,
            &accepted_revision,
            &event.proposal.validation_profile,
            plan.touched_paths(),
            &self.trusted_tool_path,
            self.trusted_rustup_home.as_deref(),
        )
        .await?;
        let phase_selection = match &resolved_phases {
            ResolvedPhasePlan::Executable(configured) => &configured.selection,
            ResolvedPhasePlan::Advisory(selection) => selection,
        };
        self.change_batch_journal.retain_phase_selection(
            workspace.id(),
            &event.identity.batch_id,
            phase_selection,
            now,
        )?;
        if let Some(receipt) = existing_receipt {
            if receipt.status == ChangeBatchReceiptStatus::Applied
                && receipt.normalizer.is_some()
                && receipt.validation.is_none()
                && let ResolvedPhasePlan::Executable(configured) = &resolved_phases
            {
                let mut progress = self
                    .change_batch_journal
                    .progress_events(&event.identity.batch_id)?;
                let receipt = complete_configured_validation(
                    &mut self.change_batch_journal,
                    self.validation_artifacts.as_mut(),
                    workspace,
                    &mut progress,
                    event,
                    configured,
                    receipt,
                    now,
                )
                .await?;
                return Ok(ExecutedChangeBatch {
                    progress,
                    receipt,
                    replayed: true,
                });
            }
            if let ResolvedPhasePlan::Executable(configured) = &resolved_phases
                && configured
                    .validation_commands
                    .iter()
                    .any(|command| command.diagnostic_parser_version.is_some())
                && self
                    .change_batch_journal
                    .diagnostic_evaluation(&event.identity.batch_id)?
                    .is_none()
            {
                return Err(phase_error(
                    "validated ChangeBatch diagnostic decision is missing",
                ));
            }
            return Ok(ExecutedChangeBatch {
                progress: self
                    .change_batch_journal
                    .progress_events(&event.identity.batch_id)?,
                receipt,
                replayed: true,
            });
        }

        let (mut progress, recover_executor) =
            prepare_execution_progress(&mut self.change_batch_journal, event, now)?;

        let last_state = progress.last().map(|entry| entry.state.clone());
        if !same_authority(workspace.provenance(), active)
            || !same_change_batch_authority(event, active, &accepted_revision)
        {
            return Err(authority_error());
        }
        let request = ChangeBatchExecutionRequest {
            active,
            checkout: workspace.layout().checkout(),
            plan: &plan,
        };
        let result = drive_change_batch_executor(
            self.change_batch_executor.as_mut(),
            request,
            last_state,
            active.lifecycle,
            retention,
            recover_executor,
        )
        .await;
        let receipt = match result {
            ChangeBatchExecutionResult::Applied {
                files,
                artifact_ref,
            } => match &resolved_phases {
                ResolvedPhasePlan::Executable(configured) => {
                    finalize_configured_applied_checkpoint(
                        &mut self.change_batch_journal,
                        self.workspace_tree.as_mut(),
                        self.validation_artifacts.as_mut(),
                        workspace,
                        &mut progress,
                        event,
                        &accepted_revision,
                        &files,
                        artifact_ref,
                        configured,
                        &self.change_batch_journal_root,
                        now,
                    )
                    .await?
                }
                ResolvedPhasePlan::Advisory(_) => {
                    finalize_applied_checkpoint(
                        &mut self.change_batch_journal,
                        self.workspace_tree.as_mut(),
                        workspace,
                        &mut progress,
                        event,
                        &accepted_revision,
                        &files,
                        artifact_ref,
                        now,
                    )
                    .await?
                }
            },
            ChangeBatchExecutionResult::PartiallyApplied {
                files,
                artifact_ref,
            } => {
                finalize_partial_checkpoint(
                    &mut self.change_batch_journal,
                    self.workspace_tree.as_mut(),
                    workspace,
                    &mut progress,
                    event,
                    &accepted_revision,
                    &files,
                    artifact_ref,
                    now,
                )
                .await?
            }
            ChangeBatchExecutionResult::RolledBack { artifact_ref } => finalize_rollback_workspace(
                &mut self.change_batch_journal,
                workspace,
                &mut progress,
                event,
                &accepted_revision,
                artifact_ref,
                now,
            )?,
            ChangeBatchExecutionResult::StateUncertain {
                files,
                artifact_ref,
            } => finalize_uncertain_workspace(
                &mut self.change_batch_journal,
                workspace,
                &mut progress,
                event,
                &accepted_revision,
                &files,
                artifact_ref,
                ActiveBatchState::Applying,
                now,
            )?,
        };
        Ok(ExecutedChangeBatch {
            progress,
            receipt,
            replayed: retention == JournalRetention::Replay,
        })
    }

    /// Stops after the real executor returns but before terminal progress and
    /// receipt retention, modeling a process loss at that exact boundary.
    ///
    /// # Errors
    ///
    /// Rejects foreign authority, invalid plans, or executor failure.
    #[cfg(feature = "test-support")]
    pub async fn interrupt_change_batch_after_mutation_for_test(
        &mut self,
        active: &ActiveJob,
        event: &ChangeBatchProposalEvent,
        now: &Instant,
    ) -> Result<ChangeBatchExecutionResult, JobWorkspaceError> {
        let workspace = self
            .active
            .get(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        let accepted_revision = self
            .change_batch_journal
            .workspace_barrier(workspace.id())?
            .map_or_else(
                || workspace.resolved_source_tree(),
                |barrier| barrier.accepted_revision,
            );
        if !same_authority(workspace.provenance(), active)
            || !same_change_batch_authority(event, active, &accepted_revision)
        {
            return Err(authority_error());
        }
        let plan = prepare_change_batch(event, ChangeBatchPolicy::default()).map_err(|_| {
            JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch interrupted proposal cannot be planned",
            )
        })?;
        self.change_batch_journal.retain_claimed_intent(
            workspace.id(),
            event,
            &accepted_revision,
            plan.plan_digest(),
            now,
        )?;
        prepare_execution_progress(&mut self.change_batch_journal, event, now)?;
        self.change_batch_executor
            .execute(ChangeBatchExecutionRequest {
                active,
                checkout: workspace.layout().checkout(),
                plan: &plan,
            })
            .await
            .map_err(|_| {
                JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch interrupted executor failed",
                )
            })
    }

    /// Consumes and removes one exact Job workspace at terminal cleanup.
    ///
    /// # Errors
    ///
    /// Rejects a missing active workspace or cleanup failure.
    pub fn close_job(
        &mut self,
        job_id: &ExecutionJobId,
        reason: WorkspaceCloseReason,
    ) -> Result<WorkspaceCleanupReport, JobWorkspaceError> {
        let workspace = self.active.get(&job_id.0).ok_or_else(authority_error)?;
        if self
            .change_batch_journal
            .workspace_barrier(workspace.id())?
            .is_some_and(|barrier| barrier.active_batch_id.is_some())
        {
            return Err(JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch workspace requires prepared close",
            ));
        }
        self.consume_workspace(job_id, reason)
    }

    fn consume_workspace(
        &mut self,
        job_id: &ExecutionJobId,
        reason: WorkspaceCloseReason,
    ) -> Result<WorkspaceCleanupReport, JobWorkspaceError> {
        let report = self
            .active
            .get_mut(&job_id.0)
            .ok_or_else(authority_error)?
            .close_in_place(reason)?;
        self.active.remove(&job_id.0);
        Ok(report)
    }

    /// Restores any unaccepted checkpoint before consuming the checkout.
    ///
    /// Uncertain or quarantined state is retained for startup reconciliation.
    ///
    /// # Errors
    ///
    /// Rejects a missing workspace, a durable barrier mismatch, an unproven
    /// restore, or a workspace cleanup failure.
    #[allow(clippy::too_many_lines)]
    pub async fn prepare_close_job(
        &mut self,
        job_id: &ExecutionJobId,
        reason: WorkspaceCloseReason,
        now: &Instant,
    ) -> Result<WorkspaceCleanupReport, JobWorkspaceError> {
        let workspace = self.active.get(&job_id.0).ok_or_else(authority_error)?;
        let workspace_id = workspace.id().to_owned();
        let checkout = workspace.layout().checkout().to_path_buf();
        let state_root = workspace.layout().sandbox().join("workspace-tree");
        let barrier = self
            .change_batch_journal
            .workspace_barrier(&workspace_id)?
            .ok_or_else(authority_error)?;
        if matches!(
            barrier.state,
            ActiveBatchState::RolledBack | ActiveBatchState::RepairRequired
        ) {
            if self
                .workspace_tree
                .compare_tree(&checkout, &state_root, &barrier.accepted_revision)
                .await
                .map_err(|_| {
                    JobWorkspaceError::new(
                        JobWorkspaceErrorCode::ChangeBatch,
                        "ChangeBatch restored workspace cannot be compared",
                    )
                })?
                != WorkspaceTreeCompareResult::Exact
            {
                return Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch restored workspace differs from the accepted tree",
                ));
            }
        } else if let (Some(checkpoint), Some(batch_id)) = (
            barrier.checkpoint_revision.as_ref(),
            barrier.active_batch_id.as_ref(),
        ) {
            let record = self
                .change_batch_journal
                .load(batch_id)?
                .ok_or_else(authority_error)?;
            let mut progress = self.change_batch_journal.progress_events(batch_id)?;
            if barrier.state != ActiveBatchState::RollbackPending {
                let rollback_started = next_progress(
                    &progress,
                    &record.event,
                    ChangeBatchProgressState::RollbackStarted,
                    "ChangeBatch close rollback started",
                    Vec::new(),
                    now,
                );
                self.change_batch_journal.retain_workspace_progress(
                    &workspace_id,
                    &rollback_started,
                    barrier.state,
                    ActiveBatchState::RollbackPending,
                )?;
                progress.push(rollback_started);
            }
            let restored = self
                .workspace_tree
                .restore_tree(
                    &workspace_id,
                    &checkout,
                    &state_root,
                    &self.change_batch_journal_root,
                    checkpoint,
                    &barrier.accepted_revision,
                )
                .await
                .map_err(|_| {
                    JobWorkspaceError::new(
                        JobWorkspaceErrorCode::ChangeBatch,
                        "ChangeBatch workspace close restore failed",
                    )
                })?;
            match restored {
                WorkspaceTreeRestoreResult::AlreadyAtTarget
                | WorkspaceTreeRestoreResult::ExactRestored => {
                    retain_rollback_completion(
                        &mut self.change_batch_journal,
                        &workspace_id,
                        &record.event,
                        &mut progress,
                        now,
                    )?;
                }
                WorkspaceTreeRestoreResult::ExactRolledBack
                | WorkspaceTreeRestoreResult::StateUncertain => {
                    retain_restore_failure(
                        &mut self.change_batch_journal,
                        &workspace_id,
                        &record.event,
                        &mut progress,
                        now,
                    )?;
                    return Err(JobWorkspaceError::new(
                        JobWorkspaceErrorCode::ChangeBatch,
                        "ChangeBatch workspace close remains uncertain",
                    ));
                }
            }
        } else if barrier.state.has_unresolved_mutation() {
            return Err(JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch workspace cannot close with unresolved mutation",
            ));
        }
        self.consume_workspace(job_id, reason)
    }

    /// Returns whether this process owns an open checkout for the Job.
    #[must_use]
    pub fn contains(&self, job_id: &ExecutionJobId) -> bool {
        self.active.contains_key(&job_id.0)
    }

    /// Returns the currently accepted tree that authorizes the next batch.
    ///
    /// # Errors
    ///
    /// Rejects a missing active workspace or corrupt durable barrier state.
    pub fn accepted_revision(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<WorkspaceRevision, JobWorkspaceError> {
        let workspace = self.active.get(&job_id.0).ok_or_else(authority_error)?;
        self.change_batch_journal
            .workspace_barrier(workspace.id())?
            .map_or_else(
                || Ok(workspace.resolved_source_tree()),
                |barrier| Ok(barrier.accepted_revision),
            )
    }

    /// Atomically records one post-apply validation or observation progress fact.
    ///
    /// This seam persists externally produced facts but does not invoke a
    /// validator or observer.
    ///
    /// # Errors
    ///
    /// Rejects foreign authority, stale workspace state, or a progress fact
    /// outside the bounded post-apply sequence.
    pub fn record_checkpoint_progress(
        &mut self,
        active: &ActiveJob,
        event: &ChangeBatchProgressEvent,
    ) -> Result<JournalRetention, JobWorkspaceError> {
        let workspace = self
            .active
            .get(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        let barrier = self
            .change_batch_journal
            .workspace_barrier(workspace.id())?
            .ok_or_else(authority_error)?;
        if !same_authority(workspace.provenance(), active)
            || !same_change_batch_progress_authority(event, active, &barrier.accepted_revision)
        {
            return Err(authority_error());
        }
        let (expected, next) = match event.state {
            ChangeBatchProgressState::ValidationStarted => (
                ActiveBatchState::Checkpointed,
                ActiveBatchState::ValidationPending,
            ),
            ChangeBatchProgressState::ValidationCompleted => (
                ActiveBatchState::ValidationPending,
                ActiveBatchState::ValidationPending,
            ),
            ChangeBatchProgressState::ObservationRequested => (
                ActiveBatchState::ValidationPending,
                ActiveBatchState::ObservationPending,
            ),
            ChangeBatchProgressState::ObservationCompleted => (
                ActiveBatchState::ObservationPending,
                ActiveBatchState::ObservationPending,
            ),
            _ => {
                return Err(JobWorkspaceError::new(
                    JobWorkspaceErrorCode::ChangeBatch,
                    "ChangeBatch checkpoint progress state is not supported",
                ));
            }
        };
        self.change_batch_journal
            .retain_workspace_progress(workspace.id(), event, expected, next)
            .map_err(Into::into)
    }

    /// Accepts one checkpoint only after an exact typed observation fact.
    ///
    /// Stale or foreign revision and digest facts return `Stale` without
    /// changing the accepted tree or releasing the Writer barrier.
    ///
    /// # Errors
    ///
    /// Rejects unavailable or corrupt durable state and invalid progress bytes.
    pub fn accept_observed_checkpoint(
        &mut self,
        active: &ActiveJob,
        progress: &ChangeBatchProgressEvent,
        observed_revision: &WorkspaceRevision,
        observed_delta_digest: &winwincode_domain::Sha256Digest,
    ) -> Result<ObservationGateResult, JobWorkspaceError> {
        let workspace = self
            .active
            .get(&active.job.job_id.0)
            .ok_or_else(authority_error)?;
        let barrier = self
            .change_batch_journal
            .workspace_barrier(workspace.id())?
            .ok_or_else(authority_error)?;
        if !same_authority(workspace.provenance(), active)
            || !same_change_batch_progress_authority(progress, active, &barrier.accepted_revision)
        {
            return Ok(ObservationGateResult::Stale);
        }
        self.change_batch_journal
            .accept_observed_checkpoint(
                workspace.id(),
                progress,
                observed_revision,
                observed_delta_digest,
                &progress.occurred_at,
            )
            .map_err(Into::into)
    }
}

async fn drive_change_batch_executor(
    executor: &mut dyn ChangeBatchExecutor,
    request: ChangeBatchExecutionRequest<'_>,
    last_state: Option<ChangeBatchProgressState>,
    lifecycle: ActiveJobLifecycle,
    retention: JournalRetention,
    recover_executor: bool,
) -> ChangeBatchExecutionResult {
    let result = if last_state == Some(ChangeBatchProgressState::RolledBack) {
        Ok(ChangeBatchExecutionResult::RolledBack { artifact_ref: None })
    } else if lifecycle == ActiveJobLifecycle::Cancelling {
        executor.cancel(request).await
    } else if retention == JournalRetention::Replay && recover_executor {
        executor.recover(request).await
    } else {
        executor.execute(request).await
    };
    result.unwrap_or(ChangeBatchExecutionResult::StateUncertain {
        files: Vec::new(),
        artifact_ref: None,
    })
}

fn same_authority(provenance: &WorkspaceProvenance, active: &ActiveJob) -> bool {
    let Ok(expected) = WorkspaceProvenance::from_active_job(active) else {
        return false;
    };
    provenance == &expected
}

fn same_change_batch_lease_authority(event: &ChangeBatchProposalEvent, active: &ActiveJob) -> bool {
    event.identity.job_id == active.job.job_id
        && event.identity.attempt == active.job.attempt
        && event.identity.lease_id == active.lease.lease_id
        && event.identity.fencing_token == active.lease.fencing_token
        && event.identity.session_identity == active.session_identity
        && event.identity.repository_id == active.job.workspace.repository_id
}

fn same_change_batch_authority(
    event: &ChangeBatchProposalEvent,
    active: &ActiveJob,
    expected_revision: &WorkspaceRevision,
) -> bool {
    same_change_batch_lease_authority(event, active)
        && event.identity.workspace_revision == *expected_revision
}

fn same_change_batch_progress_authority(
    event: &ChangeBatchProgressEvent,
    active: &ActiveJob,
    expected_revision: &WorkspaceRevision,
) -> bool {
    event.identity.job_id == active.job.job_id
        && event.identity.attempt == active.job.attempt
        && event.identity.lease_id == active.lease.lease_id
        && event.identity.fencing_token == active.lease.fencing_token
        && event.identity.session_identity == active.session_identity
        && event.identity.repository_id == active.job.workspace.repository_id
        && event.identity.workspace_revision == *expected_revision
}

fn authority_error() -> JobWorkspaceError {
    JobWorkspaceError::new(
        JobWorkspaceErrorCode::AuthorityMismatch,
        "active Job does not own this detached workspace",
    )
}

fn change_batch_journal_root(workspace_root: &Path) -> Result<PathBuf, JobWorkspaceError> {
    let parent = workspace_root.parent().ok_or_else(|| {
        JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch journal root has no private parent",
        )
    })?;
    let name = workspace_root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            JobWorkspaceError::new(
                JobWorkspaceErrorCode::ChangeBatch,
                "ChangeBatch journal root name is invalid",
            )
        })?;
    Ok(parent.join(format!(".{name}-change-batches")))
}

fn prepare_execution_progress(
    journal: &mut ChangeBatchJournal,
    proposal: &ChangeBatchProposalEvent,
    now: &Instant,
) -> Result<(Vec<ChangeBatchProgressEvent>, bool), JobWorkspaceError> {
    let mut progress = journal.progress_events(&proposal.identity.batch_id)?;
    let recover_executor = progress.iter().any(|entry| {
        matches!(
            entry.state,
            ChangeBatchProgressState::ApplyStarted | ChangeBatchProgressState::RollbackStarted
        )
    });
    for (state, summary) in [
        (
            ChangeBatchProgressState::Proposed,
            "ChangeBatch proposal retained",
        ),
        (
            ChangeBatchProgressState::Authorized,
            "ChangeBatch authority verified",
        ),
        (
            ChangeBatchProgressState::ApplyStarted,
            "ChangeBatch apply started",
        ),
    ] {
        append_progress_state(
            journal,
            &mut progress,
            proposal,
            state,
            summary,
            Vec::new(),
            now,
        )?;
    }
    Ok((progress, recover_executor))
}

enum ResolvedPhasePlan {
    Executable(ConfiguredPhasePlan),
    Advisory(winwincode_execution_port::generated::ValidationProfileSelection),
}

async fn load_configured_phase_plan(
    workspace_tree: &mut dyn WorkspaceTreePort,
    workspace: &WorkerWorkspace,
    accepted_revision: &WorkspaceRevision,
    profile: &ValidationProfileName,
    changed_paths: &[String],
    trusted_tool_path: &std::ffi::OsStr,
    trusted_rustup_home: Option<&Path>,
) -> Result<ResolvedPhasePlan, JobWorkspaceError> {
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let config = workspace_tree
        .read_blob_at_revision(
            workspace.layout().checkout(),
            &state_root,
            accepted_revision,
            VALIDATION_CONFIGURATION_PATH,
            MAX_VALIDATION_CONFIGURATION_BYTES,
        )
        .await
        .map_err(|_| phase_error("validation config cannot be read from the accepted tree"))?;
    let Some(config) = config else {
        let selection =
            resolve_validation_profile(None, validation_profile_text(profile), changed_paths)
                .map_err(|_| phase_error("validation profile suggestion is invalid"))?;
        return Ok(ResolvedPhasePlan::Advisory(selection));
    };
    let config =
        std::str::from_utf8(&config).map_err(|_| phase_error("validation config is not UTF-8"))?;
    let parsed = parse_validation_configuration(config)
        .map_err(|_| phase_error("validation config is invalid"))?;
    let scratch = workspace.layout().sandbox().join("validation-scratch");
    let scratch_for_create = scratch.clone();
    let sandbox_for_create = workspace.layout().sandbox().to_path_buf();
    tokio::task::spawn_blocking(move || {
        create_private_scratch(&sandbox_for_create, &scratch_for_create)
    })
    .await
    .map_err(|_| phase_error("validation scratch setup did not complete"))?
    .map_err(|_| phase_error("validation scratch setup failed"))?;
    ConfiguredPhasePlan::from_explicit_configuration(
        &parsed,
        validation_profile_text(profile),
        changed_paths,
        workspace.layout().checkout(),
        &scratch,
        trusted_tool_path,
        trusted_rustup_home,
    )
    .map(ResolvedPhasePlan::Executable)
    .map_err(|_| phase_error("validation profile cannot be executed"))
}

fn create_private_scratch(state_root: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode().trailing_zeros() >= 6 => {}
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "validation scratch is not a private directory",
        ))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(path)?;
        }
        Err(error) => return Err(error),
    }
    let state_root = state_root.canonicalize()?;
    let path = path.canonicalize()?;
    if path.starts_with(&state_root) && path != state_root {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "validation scratch is outside private state",
        ))
    }
}

const fn validation_profile_text(profile: &ValidationProfileName) -> &'static str {
    match profile {
        ValidationProfileName::Changed => "changed",
        ValidationProfileName::Fast => "fast",
        ValidationProfileName::Affected => "affected",
        ValidationProfileName::Final => "final",
    }
}

fn phase_error(message: &'static str) -> JobWorkspaceError {
    JobWorkspaceError::new(JobWorkspaceErrorCode::ChangeBatch, message)
}

fn trusted_tool_path_snapshot(workspace_root: &Path, source_root: &Path) -> std::ffi::OsString {
    let configured = std::env::var_os("PATH").unwrap_or_else(|| {
        std::ffi::OsString::from("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    });
    let directories = std::env::split_paths(&configured)
        .filter(|path| path.is_absolute())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| {
            path.is_dir() && !path.starts_with(workspace_root) && !path.starts_with(source_root)
        })
        .collect::<Vec<_>>();
    std::env::join_paths(directories).unwrap_or_default()
}

fn trusted_rustup_home_snapshot(workspace_root: &Path, source_root: &Path) -> Option<PathBuf> {
    let configured = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")))?;
    let canonical = configured.canonicalize().ok()?;
    (canonical.is_dir()
        && !canonical.starts_with(workspace_root)
        && !canonical.starts_with(source_root))
    .then_some(canonical)
}

#[allow(clippy::too_many_arguments)]
async fn finalize_applied_checkpoint(
    journal: &mut ChangeBatchJournal,
    workspace_tree: &mut dyn WorkspaceTreePort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    files: &[AppliedFileSummary],
    artifact_ref: Option<ArtifactReference>,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let files = canonical_files(files, "ChangeBatch applied file summaries are invalid")?;
    let delta_digest = exact_delta_digest(&files)?;
    let barrier = journal
        .workspace_barrier(workspace.id())?
        .ok_or_else(authority_error)?;
    if barrier.state == ActiveBatchState::Applying {
        journal.transition_workspace_batch(
            workspace.id(),
            &proposal.identity.batch_id,
            ActiveBatchState::Applying,
            ActiveBatchState::CheckpointPending,
            now,
        )?;
    } else if barrier.state != ActiveBatchState::CheckpointPending {
        return Err(JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch workspace checkpoint state is stale",
        ));
    }
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let Ok(result_revision) = workspace_tree
        .compute_tree(
            workspace.layout().checkout(),
            &state_root,
            base_revision,
            &files,
            &delta_digest,
        )
        .await
    else {
        return finalize_uncertain_workspace(
            journal,
            workspace,
            progress,
            proposal,
            base_revision,
            &files,
            artifact_ref,
            ActiveBatchState::CheckpointPending,
            now,
        );
    };
    let receipt = exact_receipt(
        proposal,
        base_revision,
        &result_revision,
        files,
        artifact_ref.clone(),
        ChangeBatchReceiptStatus::Applied,
    )?;
    let event = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::Applied,
        "ChangeBatch apply checkpointed",
        artifact_ref.into_iter().collect(),
        now,
    );
    journal.retain_applied_checkpoint(workspace.id(), &event, &receipt, now)?;
    progress.push(event);
    Ok(receipt)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn finalize_configured_applied_checkpoint(
    journal: &mut ChangeBatchJournal,
    workspace_tree: &mut dyn WorkspaceTreePort,
    validation_artifacts: &mut dyn ValidationArtifactPort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    files: &[AppliedFileSummary],
    artifact_ref: Option<ArtifactReference>,
    configured: &ConfiguredPhasePlan,
    journal_root: &Path,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let files = canonical_files(files, "ChangeBatch applied file summaries are invalid")?;
    let delta_digest = exact_delta_digest(&files)?;
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let pre_writer = workspace_tree
        .compute_tree(
            workspace.layout().checkout(),
            &state_root,
            base_revision,
            &files,
            &delta_digest,
        )
        .await
        .map_err(|_| phase_error("pre-Writer tree cannot be checkpointed"))?;
    let runner = PhaseProcessRunner;
    let cancellation = PhaseCancellation::default();
    let mut phase_record = journal
        .phase_record(&proposal.identity.batch_id)?
        .ok_or_else(|| phase_error("validation selection is missing"))?;
    if phase_record.command_receipts.len() > configured.writer_commands.len() {
        return Err(phase_error("Writer command cursor is invalid"));
    }
    for (ordinal, command) in configured.writer_commands.iter().enumerate() {
        if ordinal < phase_record.command_receipts.len() {
            if phase_record.command_receipts[ordinal].status != PhaseProcessStatus::Passed {
                return Err(phase_error("Writer command replay is terminal"));
            }
            continue;
        }
        let receipt = runner
            .execute(workspace.layout().checkout(), command, &cancellation)
            .await
            .map_err(|_| phase_error("Writer command execution failed"))?;
        journal.retain_phase_command_receipt(
            &proposal.identity.batch_id,
            ordinal,
            &receipt,
            now,
        )?;
        let passed = receipt.status == PhaseProcessStatus::Passed;
        let failed_status = receipt.status;
        phase_record.command_receipts.push(receipt);
        if !passed {
            return rollback_failed_writer(
                journal,
                workspace_tree,
                workspace,
                progress,
                proposal,
                base_revision,
                &pre_writer,
                &files,
                configured,
                failed_status,
                artifact_ref,
                journal_root,
                now,
            )
            .await;
        }
    }
    let mut allowed_writer_paths = configured.allowed_writer_paths.clone();
    for file in &files {
        allowed_writer_paths.push(file.path.clone());
        allowed_writer_paths.extend(file.move_path.iter().cloned());
    }
    allowed_writer_paths.sort();
    allowed_writer_paths.dedup();
    let snapshot = workspace_tree
        .snapshot_writer_changes(
            workspace.layout().checkout(),
            &state_root,
            base_revision,
            &pre_writer,
            &files,
            &allowed_writer_paths,
        )
        .await
        .map_err(|_| phase_error("post-Writer tree cannot be proven"))?;
    let (result_revision, files, changed_file_digests, normalizer_status) = match snapshot {
        WorkspaceWriterSnapshotResult::Unchanged {
            revision,
            files,
            delta_digest: _,
        } => (
            revision,
            files,
            Vec::new(),
            NormalizerReceiptStatus::Unchanged,
        ),
        WorkspaceWriterSnapshotResult::Normalized {
            revision,
            files,
            delta_digest: _,
            changed_file_digests,
        } => (
            revision,
            files,
            changed_file_digests,
            NormalizerReceiptStatus::Normalized,
        ),
        WorkspaceWriterSnapshotResult::ScopeViolation { observed_revision } => {
            return rollback_writer_scope_violation(
                journal,
                workspace_tree,
                workspace,
                progress,
                proposal,
                base_revision,
                &observed_revision,
                &pre_writer,
                configured.writer_commands.len(),
                NormalizerReceiptStatus::Rejected,
                artifact_ref,
                journal_root,
                now,
            )
            .await;
        }
        WorkspaceWriterSnapshotResult::StateUncertain => {
            return finalize_uncertain_workspace(
                journal,
                workspace,
                progress,
                proposal,
                base_revision,
                &files,
                artifact_ref,
                ActiveBatchState::Applying,
                now,
            );
        }
    };
    let normalizer = NormalizerReceipt {
        artifact_refs: Vec::new(),
        base_revision: pre_writer,
        changed_file_digests,
        result_revision: Some(result_revision.clone()),
        status: normalizer_status,
    };
    journal.retain_normalizer_receipt(
        &proposal.identity.batch_id,
        &normalizer,
        configured.writer_commands.len(),
        &normalizer.base_revision,
        Some(&result_revision),
        now,
    )?;
    let barrier = journal
        .workspace_barrier(workspace.id())?
        .ok_or_else(authority_error)?;
    if barrier.state == ActiveBatchState::Applying {
        journal.transition_workspace_batch(
            workspace.id(),
            &proposal.identity.batch_id,
            ActiveBatchState::Applying,
            ActiveBatchState::CheckpointPending,
            now,
        )?;
    } else if barrier.state != ActiveBatchState::CheckpointPending {
        return Err(phase_error("configured checkpoint state is stale"));
    }
    let mut receipt = exact_receipt(
        proposal,
        base_revision,
        &result_revision,
        files,
        artifact_ref.clone(),
        ChangeBatchReceiptStatus::Applied,
    )?;
    receipt.normalizer = Some(normalizer);
    let applied = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::Applied,
        "ChangeBatch Writer checkpointed",
        artifact_ref.into_iter().collect(),
        now,
    );
    journal.retain_applied_checkpoint(workspace.id(), &applied, &receipt, now)?;
    progress.push(applied);

    complete_configured_validation(
        journal,
        validation_artifacts,
        workspace,
        progress,
        proposal,
        configured,
        receipt,
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn complete_configured_validation(
    journal: &mut ChangeBatchJournal,
    validation_artifacts: &mut dyn ValidationArtifactPort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    configured: &ConfiguredPhasePlan,
    mut receipt: ChangeBatchReceipt,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    if receipt.validation.is_some() {
        return Ok(receipt);
    }
    let result_revision = receipt
        .result_revision
        .clone()
        .ok_or_else(|| phase_error("validated checkpoint has no result tree"))?;
    append_workspace_progress_state(
        journal,
        workspace,
        progress,
        proposal,
        ChangeBatchProgressState::ValidationStarted,
        "ChangeBatch read-only validation started",
        Vec::new(),
        ActiveBatchState::Checkpointed,
        ActiveBatchState::ValidationPending,
        now,
    )?;
    let runner = PhaseProcessRunner;
    let cancellation = PhaseCancellation::default();
    let first_validation = configured.writer_commands.len();
    let record = journal
        .phase_record(&proposal.identity.batch_id)?
        .ok_or_else(|| phase_error("validation execution record is missing"))?;
    if record.command_receipts.len() < first_validation {
        return Err(phase_error("validation cursor precedes Writer completion"));
    }
    let mut validation_results = record.command_receipts[first_validation..].to_vec();
    let mut diagnostic_batches = record.diagnostic_batches[first_validation..].to_vec();
    let mut diagnostic_parse_failed = record.diagnostic_parse_failures[first_validation..]
        .iter()
        .copied()
        .any(std::convert::identity);
    for (index, command) in configured.validation_commands.iter().enumerate() {
        if index < validation_results.len() {
            continue;
        }
        let execution = runner
            .execute_with_output(workspace.layout().checkout(), command, &cancellation)
            .await
            .map_err(|_| phase_error("validation command execution failed"))?;
        let ordinal = first_validation + index;
        let mut result = execution.receipt;
        result.stdout_artifact_ref = Some(persist_validation_artifact(
            validation_artifacts,
            proposal,
            ordinal,
            &result.name,
            ValidationArtifactStream::Stdout,
            command
                .diagnostic_parser_version
                .as_ref()
                .map_or("text/plain; charset=utf-8", diagnostic_media_type),
            &execution.stdout,
        )?);
        result.stderr_artifact_ref = Some(persist_validation_artifact(
            validation_artifacts,
            proposal,
            ordinal,
            &result.name,
            ValidationArtifactStream::Stderr,
            "text/plain; charset=utf-8",
            &execution.stderr,
        )?);
        let (diagnostic_batch, parse_failed) = command
            .diagnostic_parser_version
            .clone()
            .map(|version| {
                parse_diagnostics(
                    version.clone(),
                    diagnostic_input(&version, &execution.stdout, &execution.stderr),
                    workspace.layout().checkout(),
                )
            })
            .map_or((None, false), |parsed| match parsed {
                Ok(batch) => (Some(batch), false),
                Err(_) => (None, true),
            });
        journal.retain_phase_command_result(
            &proposal.identity.batch_id,
            ordinal,
            &result,
            diagnostic_batch.as_ref(),
            parse_failed,
            now,
        )?;
        diagnostic_parse_failed |= parse_failed;
        diagnostic_batches.push(diagnostic_batch);
        let terminal = matches!(
            result.status,
            PhaseProcessStatus::TimedOut
                | PhaseProcessStatus::Cancelled
                | PhaseProcessStatus::OutputLimitExceeded
        );
        validation_results.push(result);
        if terminal {
            break;
        }
    }
    let validation = PhaseProcessRunner::validation_receipt(
        configured.profile(),
        &result_revision,
        &validation_results,
    )
    .map_err(|_| phase_error("validation receipt is invalid"))?;
    journal.retain_validation_receipt(
        &proposal.identity.batch_id,
        &validation,
        first_validation + validation_results.len(),
        &result_revision,
        validation.result_revision.as_ref(),
        now,
    )?;
    let diagnostic_disposition = retain_validation_diagnostic_evaluation(
        journal,
        proposal,
        configured,
        &receipt.base_revision,
        &result_revision,
        &validation.status,
        diagnostic_batches,
        diagnostic_parse_failed,
        now,
    )?;
    receipt.validation = Some(validation);
    let validation_completed = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::ValidationCompleted,
        "ChangeBatch read-only validation completed",
        Vec::new(),
        now,
    );
    journal.retain_validated_checkpoint(workspace.id(), &validation_completed, &receipt, now)?;
    progress.push(validation_completed);
    let requires_repair = match diagnostic_disposition {
        Some(ValidationDiagnosticDisposition::RepairRequired { .. }) => true,
        Some(
            ValidationDiagnosticDisposition::Pass
            | ValidationDiagnosticDisposition::BaselineUnavailable,
        ) => false,
        None => receipt
            .validation
            .as_ref()
            .is_none_or(|validation| validation.status != ValidationReceiptStatus::Passed),
    };
    if requires_repair {
        append_workspace_progress_state(
            journal,
            workspace,
            progress,
            proposal,
            ChangeBatchProgressState::RepairRequired,
            "ChangeBatch validation requires repair",
            Vec::new(),
            ActiveBatchState::ValidationPending,
            ActiveBatchState::RepairRequired,
            now,
        )?;
        return Ok(receipt);
    }
    append_workspace_progress_state(
        journal,
        workspace,
        progress,
        proposal,
        ChangeBatchProgressState::ObservationRequested,
        "ChangeBatch observation requested",
        Vec::new(),
        ActiveBatchState::ValidationPending,
        ActiveBatchState::ObservationPending,
        now,
    )?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
fn retain_validation_diagnostic_evaluation(
    journal: &mut ChangeBatchJournal,
    proposal: &ChangeBatchProposalEvent,
    configured: &ConfiguredPhasePlan,
    base_revision: &WorkspaceRevision,
    result_revision: &WorkspaceRevision,
    validation_status: &ValidationReceiptStatus,
    diagnostic_batches: Vec<
        Option<winwincode_execution_port::diagnostic_parser::DiagnosticParseBatch>,
    >,
    parser_failed: bool,
    now: &Instant,
) -> Result<Option<ValidationDiagnosticDisposition>, JobWorkspaceError> {
    let parser_count = configured
        .validation_commands
        .iter()
        .filter(|command| command.diagnostic_parser_version.is_some())
        .count();
    if parser_count == 0 {
        return Ok(None);
    }
    let parsed = diagnostic_batches.into_iter().flatten().collect::<Vec<_>>();
    let parser_failed = parser_failed || parsed.len() != parser_count;
    let baseline = journal.diagnostic_baseline(&proposal.identity.batch_id, base_revision)?;
    let result = if parser_failed {
        None
    } else {
        Some(
            build_diagnostic_baseline(result_revision.clone(), &parsed)
                .map_err(|_| phase_error("validation diagnostic baseline is invalid"))?,
        )
    };
    let comparison = baseline
        .as_ref()
        .zip(result.as_ref())
        .map(|(baseline, result)| compare_diagnostic_baselines(baseline, result))
        .transpose()
        .map_err(|_| phase_error("validation diagnostic baseline is not comparable"))?;
    let disposition =
        decide_validation_diagnostics(validation_status, comparison.as_ref(), parser_failed);
    let (disposition_text, reason_code) = match disposition {
        ValidationDiagnosticDisposition::Pass => ("pass", None),
        ValidationDiagnosticDisposition::BaselineUnavailable => ("baseline_unavailable", None),
        ValidationDiagnosticDisposition::RepairRequired { reason_code } => {
            ("repair_required", Some(reason_code.to_owned()))
        }
    };
    journal.retain_diagnostic_evaluation(
        &proposal.identity.batch_id,
        &ValidationDiagnosticEvaluation {
            base_revision: base_revision.clone(),
            result_revision: result_revision.clone(),
            baseline,
            result,
            comparison,
            parser_failed,
            disposition: disposition_text.to_owned(),
            reason_code,
        },
        now,
    )?;
    Ok(Some(disposition))
}

#[allow(clippy::too_many_arguments)]
fn persist_validation_artifact(
    port: &mut dyn ValidationArtifactPort,
    proposal: &ChangeBatchProposalEvent,
    command_ordinal: usize,
    command_id: &str,
    stream: ValidationArtifactStream,
    media_type: &'static str,
    bytes: &[u8],
) -> Result<ArtifactReference, JobWorkspaceError> {
    let artifact = port
        .persist(ValidationArtifactRequest {
            identity: &proposal.identity,
            command_ordinal,
            command_id,
            stream,
            media_type,
            bytes,
        })
        .map_err(|_| phase_error("validation Artifact cannot be persisted"))?;
    let expected_digest =
        winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    if artifact.digest != expected_digest {
        return Err(phase_error(
            "validation Artifact digest does not match output",
        ));
    }
    Ok(artifact)
}

#[allow(clippy::too_many_arguments)]
async fn rollback_failed_writer(
    journal: &mut ChangeBatchJournal,
    workspace_tree: &mut dyn WorkspaceTreePort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    pre_writer: &WorkspaceRevision,
    files: &[AppliedFileSummary],
    configured: &ConfiguredPhasePlan,
    failed_status: PhaseProcessStatus,
    artifact_ref: Option<ArtifactReference>,
    journal_root: &Path,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let mut allowed = configured.allowed_writer_paths.clone();
    for file in files {
        allowed.push(file.path.clone());
        allowed.extend(file.move_path.iter().cloned());
    }
    allowed.sort();
    allowed.dedup();
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let snapshot = workspace_tree
        .snapshot_writer_changes(
            workspace.layout().checkout(),
            &state_root,
            base_revision,
            pre_writer,
            files,
            &allowed,
        )
        .await
        .map_err(|_| phase_error("failed Writer state cannot be inspected"))?;
    let observed_revision = match snapshot {
        WorkspaceWriterSnapshotResult::Unchanged { revision, .. }
        | WorkspaceWriterSnapshotResult::Normalized { revision, .. }
        | WorkspaceWriterSnapshotResult::ScopeViolation {
            observed_revision: revision,
        } => revision,
        WorkspaceWriterSnapshotResult::StateUncertain => {
            return finalize_uncertain_workspace(
                journal,
                workspace,
                progress,
                proposal,
                base_revision,
                files,
                artifact_ref,
                ActiveBatchState::Applying,
                now,
            );
        }
    };
    let status = match failed_status {
        PhaseProcessStatus::Failed => NormalizerReceiptStatus::Rejected,
        PhaseProcessStatus::Cancelled => NormalizerReceiptStatus::Cancelled,
        PhaseProcessStatus::TimedOut | PhaseProcessStatus::OutputLimitExceeded => {
            NormalizerReceiptStatus::InfrastructureError
        }
        PhaseProcessStatus::Passed => return Err(phase_error("Writer failure status is invalid")),
    };
    rollback_writer_scope_violation(
        journal,
        workspace_tree,
        workspace,
        progress,
        proposal,
        base_revision,
        &observed_revision,
        pre_writer,
        configured.writer_commands.len(),
        status,
        artifact_ref,
        journal_root,
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn rollback_writer_scope_violation(
    journal: &mut ChangeBatchJournal,
    workspace_tree: &mut dyn WorkspaceTreePort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    observed_revision: &WorkspaceRevision,
    normalizer_base: &WorkspaceRevision,
    completed_writer_commands: usize,
    normalizer_status: NormalizerReceiptStatus,
    artifact_ref: Option<ArtifactReference>,
    journal_root: &Path,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let normalizer = NormalizerReceipt {
        artifact_refs: Vec::new(),
        base_revision: normalizer_base.clone(),
        changed_file_digests: Vec::new(),
        result_revision: None,
        status: normalizer_status,
    };
    journal.retain_normalizer_receipt(
        &proposal.identity.batch_id,
        &normalizer,
        completed_writer_commands,
        normalizer_base,
        None,
        now,
    )?;
    append_workspace_progress_state(
        journal,
        workspace,
        progress,
        proposal,
        ChangeBatchProgressState::RollbackStarted,
        "ChangeBatch Writer scope rollback started",
        Vec::new(),
        ActiveBatchState::Applying,
        ActiveBatchState::RollbackPending,
        now,
    )?;
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let restore = workspace_tree
        .restore_tree(
            workspace.id(),
            workspace.layout().checkout(),
            &state_root,
            journal_root,
            observed_revision,
            base_revision,
        )
        .await
        .map_err(|_| phase_error("Writer scope rollback failed"))?;
    if !matches!(
        restore,
        WorkspaceTreeRestoreResult::AlreadyAtTarget | WorkspaceTreeRestoreResult::ExactRestored
    ) {
        return finalize_uncertain_workspace(
            journal,
            workspace,
            progress,
            proposal,
            base_revision,
            &[],
            artifact_ref,
            ActiveBatchState::RollbackPending,
            now,
        );
    }
    append_workspace_progress_state(
        journal,
        workspace,
        progress,
        proposal,
        ChangeBatchProgressState::RolledBack,
        "ChangeBatch Writer scope rollback completed",
        Vec::new(),
        ActiveBatchState::RollbackPending,
        ActiveBatchState::RolledBack,
        now,
    )?;
    let mut receipt = exact_receipt(
        proposal,
        base_revision,
        base_revision,
        Vec::new(),
        artifact_ref.clone(),
        ChangeBatchReceiptStatus::Rejected,
    )?;
    receipt.normalizer = Some(normalizer);
    retain_workspace_terminal(
        journal,
        workspace,
        progress,
        proposal,
        &receipt,
        ChangeBatchProgressState::RepairRequired,
        "ChangeBatch Writer requires repair",
        artifact_ref.into_iter().collect(),
        ActiveBatchState::RolledBack,
        ActiveBatchState::RepairRequired,
        now,
    )?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
async fn finalize_partial_checkpoint(
    journal: &mut ChangeBatchJournal,
    workspace_tree: &mut dyn WorkspaceTreePort,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    files: &[AppliedFileSummary],
    artifact_ref: Option<ArtifactReference>,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let files = canonical_files(files, "ChangeBatch partial file summaries are invalid")?;
    let delta_digest = exact_delta_digest(&files)?;
    let state_root = workspace.layout().sandbox().join("workspace-tree");
    let Ok(result_revision) = workspace_tree
        .compute_tree(
            workspace.layout().checkout(),
            &state_root,
            base_revision,
            &files,
            &delta_digest,
        )
        .await
    else {
        return finalize_uncertain_workspace(
            journal,
            workspace,
            progress,
            proposal,
            base_revision,
            &files,
            artifact_ref,
            ActiveBatchState::Applying,
            now,
        );
    };
    let receipt = exact_receipt(
        proposal,
        base_revision,
        &result_revision,
        files,
        artifact_ref.clone(),
        ChangeBatchReceiptStatus::PartiallyApplied,
    )?;
    retain_workspace_terminal(
        journal,
        workspace,
        progress,
        proposal,
        &receipt,
        ChangeBatchProgressState::InfrastructureFailed,
        "ChangeBatch rollback left an exact partial delta",
        artifact_ref.into_iter().collect(),
        ActiveBatchState::Applying,
        ActiveBatchState::Quarantined,
        now,
    )?;
    Ok(receipt)
}

fn finalize_rollback_workspace(
    journal: &mut ChangeBatchJournal,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    artifact_ref: Option<ArtifactReference>,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    for (state, summary, expected, next) in [
        (
            ChangeBatchProgressState::RollbackStarted,
            "ChangeBatch rollback started",
            ActiveBatchState::Applying,
            ActiveBatchState::RollbackPending,
        ),
        (
            ChangeBatchProgressState::RolledBack,
            "ChangeBatch rollback completed",
            ActiveBatchState::RollbackPending,
            ActiveBatchState::RolledBack,
        ),
    ] {
        append_workspace_progress_state(
            journal,
            workspace,
            progress,
            proposal,
            state,
            summary,
            artifact_ref.clone().into_iter().collect(),
            expected,
            next,
            now,
        )?;
    }
    let receipt = exact_receipt(
        proposal,
        base_revision,
        base_revision,
        Vec::new(),
        artifact_ref.clone(),
        ChangeBatchReceiptStatus::Rejected,
    )?;
    retain_workspace_terminal(
        journal,
        workspace,
        progress,
        proposal,
        &receipt,
        ChangeBatchProgressState::RepairRequired,
        "ChangeBatch repair required after rollback",
        artifact_ref.into_iter().collect(),
        ActiveBatchState::RolledBack,
        ActiveBatchState::RepairRequired,
        now,
    )?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
fn finalize_uncertain_workspace(
    journal: &mut ChangeBatchJournal,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    files: &[AppliedFileSummary],
    artifact_ref: Option<ArtifactReference>,
    expected: ActiveBatchState,
    now: &Instant,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let files = canonical_files(files, "ChangeBatch uncertain file summaries are invalid")?;
    let receipt = ChangeBatchReceipt {
        artifact_ref: artifact_ref.clone(),
        base_revision: base_revision.clone(),
        delta_digest: None,
        delta_exact: false,
        files,
        identity: proposal.identity.clone(),
        normalizer: None,
        observation: None,
        result_revision: None,
        status: ChangeBatchReceiptStatus::StateUncertain,
        validation: None,
    };
    retain_workspace_terminal(
        journal,
        workspace,
        progress,
        proposal,
        &receipt,
        ChangeBatchProgressState::InfrastructureFailed,
        "ChangeBatch state is uncertain",
        artifact_ref.into_iter().collect(),
        expected,
        ActiveBatchState::Quarantined,
        now,
    )?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
fn append_workspace_progress_state(
    journal: &mut ChangeBatchJournal,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    state: ChangeBatchProgressState,
    summary: &str,
    artifact_refs: Vec<ArtifactReference>,
    expected: ActiveBatchState,
    next: ActiveBatchState,
    now: &Instant,
) -> Result<(), JobWorkspaceError> {
    if progress.iter().any(|event| event.state == state) {
        return Ok(());
    }
    let event = next_progress(progress, proposal, state, summary, artifact_refs, now);
    journal.retain_workspace_progress(workspace.id(), &event, expected, next)?;
    progress.push(event);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn retain_workspace_terminal(
    journal: &mut ChangeBatchJournal,
    workspace: &WorkerWorkspace,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    receipt: &ChangeBatchReceipt,
    state: ChangeBatchProgressState,
    summary: &str,
    artifact_refs: Vec<ArtifactReference>,
    expected: ActiveBatchState,
    next: ActiveBatchState,
    now: &Instant,
) -> Result<(), JobWorkspaceError> {
    let event = next_progress(progress, proposal, state, summary, artifact_refs, now);
    journal.retain_terminal_workspace_receipt(
        workspace.id(),
        &event,
        receipt,
        expected,
        next,
        now,
    )?;
    progress.push(event);
    Ok(())
}

fn next_progress(
    progress: &[ChangeBatchProgressEvent],
    proposal: &ChangeBatchProposalEvent,
    state: ChangeBatchProgressState,
    summary: &str,
    artifact_refs: Vec<ArtifactReference>,
    now: &Instant,
) -> ChangeBatchProgressEvent {
    ChangeBatchProgressEvent {
        artifact_refs,
        identity: proposal.identity.clone(),
        occurred_at: now.clone(),
        sequence: progress
            .last()
            .map_or(1, |event| event.sequence.saturating_add(1)),
        state,
        summary: summary.to_owned(),
    }
}

fn retain_rollback_completion(
    journal: &mut ChangeBatchJournal,
    workspace_id: &str,
    proposal: &ChangeBatchProposalEvent,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    now: &Instant,
) -> Result<(), JobWorkspaceError> {
    let rolled_back = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::RolledBack,
        "ChangeBatch accepted tree restored",
        Vec::new(),
        now,
    );
    journal.retain_workspace_progress(
        workspace_id,
        &rolled_back,
        ActiveBatchState::RollbackPending,
        ActiveBatchState::RolledBack,
    )?;
    progress.push(rolled_back);
    let repair_required = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::RepairRequired,
        "ChangeBatch repair required after accepted tree restore",
        Vec::new(),
        now,
    );
    journal.retain_workspace_progress(
        workspace_id,
        &repair_required,
        ActiveBatchState::RolledBack,
        ActiveBatchState::RepairRequired,
    )?;
    progress.push(repair_required);
    Ok(())
}

fn retain_restore_failure(
    journal: &mut ChangeBatchJournal,
    workspace_id: &str,
    proposal: &ChangeBatchProposalEvent,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    now: &Instant,
) -> Result<(), JobWorkspaceError> {
    let failure = next_progress(
        progress,
        proposal,
        ChangeBatchProgressState::InfrastructureFailed,
        "ChangeBatch accepted tree restore is uncertain",
        Vec::new(),
        now,
    );
    journal.retain_workspace_progress(
        workspace_id,
        &failure,
        ActiveBatchState::RollbackPending,
        ActiveBatchState::Quarantined,
    )?;
    progress.push(failure);
    Ok(())
}

fn canonical_files(
    files: &[AppliedFileSummary],
    message: &'static str,
) -> Result<Vec<AppliedFileSummary>, JobWorkspaceError> {
    canonical_applied_file_summaries(files)
        .map_err(|_| JobWorkspaceError::new(JobWorkspaceErrorCode::ChangeBatch, message))
}

fn exact_delta_digest(
    files: &[AppliedFileSummary],
) -> Result<winwincode_domain::Sha256Digest, JobWorkspaceError> {
    derive_delta_digest(files).map_err(|_| {
        JobWorkspaceError::new(
            JobWorkspaceErrorCode::ChangeBatch,
            "ChangeBatch exact delta digest cannot be derived",
        )
    })
}

fn append_progress_state(
    journal: &mut ChangeBatchJournal,
    progress: &mut Vec<ChangeBatchProgressEvent>,
    proposal: &ChangeBatchProposalEvent,
    state: ChangeBatchProgressState,
    summary: &str,
    artifact_refs: Vec<ArtifactReference>,
    now: &Instant,
) -> Result<(), JobWorkspaceError> {
    if progress.iter().any(|event| event.state == state) {
        return Ok(());
    }
    let sequence = progress
        .last()
        .map_or(1, |event| event.sequence.saturating_add(1));
    let event = ChangeBatchProgressEvent {
        artifact_refs,
        identity: proposal.identity.clone(),
        occurred_at: now.clone(),
        sequence,
        state,
        summary: summary.to_owned(),
    };
    journal.append_progress(&event)?;
    progress.push(event);
    Ok(())
}

fn exact_receipt(
    proposal: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    result_revision: &WorkspaceRevision,
    files: Vec<AppliedFileSummary>,
    artifact_ref: Option<ArtifactReference>,
    status: ChangeBatchReceiptStatus,
) -> Result<ChangeBatchReceipt, JobWorkspaceError> {
    let delta_digest = exact_delta_digest(&files)?;
    Ok(ChangeBatchReceipt {
        artifact_ref,
        base_revision: base_revision.clone(),
        delta_digest: Some(delta_digest),
        delta_exact: true,
        files,
        identity: proposal.identity.clone(),
        normalizer: None,
        observation: None,
        result_revision: Some(result_revision.clone()),
        status,
        validation: None,
    })
}
