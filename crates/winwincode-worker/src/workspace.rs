// SPDX-License-Identifier: Apache-2.0

//! Per-job Git worktree, temporary home, sandbox, and Artifact isolation.
//!
//! This module owns only Worker-local files. Durable lease/session authority
//! remains in the Control Plane; an [`ActiveJob`](crate::ActiveJob) is copied
//! into immutable candidate and Artifact provenance at the boundary.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId,
    RequestId, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionScope,
};

use crate::ActiveJob;

const RECOVERY_MANIFEST: &str = ".winwincode-workspace.json";
const CREATION_INTENT_PREFIX: &str = ".winwincode-workspace-create-";
const CLEANUP_INTENT_PREFIX: &str = ".winwincode-workspace-clean-";
const OWNER_LOCK_PREFIX: &str = ".winwincode-workspace-owner-";
const CREATION_INTENT_SUFFIX: &str = ".json";
const RECOVERY_MANIFEST_SCHEMA: u8 = 3;
const CANDIDATE_MANIFEST_SCHEMA: u8 = 1;
const MAX_PATH_BYTES: usize = 4_096;

/// Stable categories for workspace failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceErrorCode {
    InvalidInput,
    NotFound,
    Conflict,
    PathEscape,
    DigestMismatch,
    Corrupt,
    Git,
    Io,
}

/// Test-only process interruption points in durable workspace creation.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceCreationInterruption {
    AfterRootCreated,
    AfterCreatingManifest,
    AfterWorktreeAdded,
}

/// Test-only process interruption points in durable workspace cleanup.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceCleanupInterruption {
    AfterCleaningManifest,
    AfterWorktreeRemoved,
    FailWorktreeRemoval,
    FailPrune,
    FailRootRemovalAfterManifest,
    FailParentSync,
}

/// Test-only cleanup failure selected for a normal creation rollback.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceCreationRollbackFailure {
    WorktreeRemoval,
    Prune,
    RootRemovalAfterManifest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum WorkspaceRecoveryPhase {
    Creating,
    Active,
    Cleaning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreationInterruption {
    None,
    #[cfg(feature = "test-support")]
    AfterRootCreated,
    #[cfg(feature = "test-support")]
    AfterCreatingManifest,
    #[cfg(feature = "test-support")]
    AfterWorktreeAdded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupInterruption {
    None,
    #[cfg(feature = "test-support")]
    AfterCleaningManifest,
    #[cfg(feature = "test-support")]
    AfterWorktreeRemoved,
    #[cfg(feature = "test-support")]
    FailWorktreeRemoval,
    #[cfg(feature = "test-support")]
    FailPrune,
    #[cfg(feature = "test-support")]
    FailRootRemovalAfterManifest,
    #[cfg(feature = "test-support")]
    FailParentSync,
}

#[cfg(feature = "test-support")]
impl From<WorkspaceCreationInterruption> for CreationInterruption {
    fn from(value: WorkspaceCreationInterruption) -> Self {
        match value {
            WorkspaceCreationInterruption::AfterRootCreated => Self::AfterRootCreated,
            WorkspaceCreationInterruption::AfterCreatingManifest => Self::AfterCreatingManifest,
            WorkspaceCreationInterruption::AfterWorktreeAdded => Self::AfterWorktreeAdded,
        }
    }
}

#[cfg(feature = "test-support")]
impl From<WorkspaceCleanupInterruption> for CleanupInterruption {
    fn from(value: WorkspaceCleanupInterruption) -> Self {
        match value {
            WorkspaceCleanupInterruption::AfterCleaningManifest => Self::AfterCleaningManifest,
            WorkspaceCleanupInterruption::AfterWorktreeRemoved => Self::AfterWorktreeRemoved,
            WorkspaceCleanupInterruption::FailWorktreeRemoval => Self::FailWorktreeRemoval,
            WorkspaceCleanupInterruption::FailPrune => Self::FailPrune,
            WorkspaceCleanupInterruption::FailRootRemovalAfterManifest => {
                Self::FailRootRemovalAfterManifest
            }
            WorkspaceCleanupInterruption::FailParentSync => Self::FailParentSync,
        }
    }
}

#[cfg(feature = "test-support")]
impl From<WorkspaceCreationRollbackFailure> for CleanupInterruption {
    fn from(value: WorkspaceCreationRollbackFailure) -> Self {
        match value {
            WorkspaceCreationRollbackFailure::WorktreeRemoval => Self::FailWorktreeRemoval,
            WorkspaceCreationRollbackFailure::Prune => Self::FailPrune,
            WorkspaceCreationRollbackFailure::RootRemovalAfterManifest => {
                Self::FailRootRemovalAfterManifest
            }
        }
    }
}

/// Failure returned by Worker-local workspace operations.
#[derive(Debug)]
pub struct WorkspaceError {
    code: WorkspaceErrorCode,
    message: String,
}

impl WorkspaceError {
    fn new(code: WorkspaceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorCode::InvalidInput, message)
    }

    fn io(context: &str, error: impl fmt::Display) -> Self {
        Self::new(WorkspaceErrorCode::Io, format!("{context}: {error}"))
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn code(&self) -> WorkspaceErrorCode {
        self.code
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkspaceError {}

/// Immutable lease, attempt, Worker, and session binding copied from an active job.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceProvenance {
    pub execution_job_id: ExecutionJobId,
    pub execution_job_digest: Sha256Digest,
    pub logical_execution_job_digest: Sha256Digest,
    pub lease_id: LeaseId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
    pub lease_issued_at: Instant,
    pub lease_expires_at: Instant,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub codex_thread_id: CodexThreadId,
}

impl WorkspaceProvenance {
    pub(crate) fn from_active_job(active: &ActiveJob) -> Result<Self, WorkspaceError> {
        if active.job.job_id != active.lease.job_id
            || active.job.attempt != active.lease.attempt
            || active.worker_session_id != active.session_identity.worker_session_id
            || active.codex_thread_id != active.session_identity.codex_thread_id
        {
            return Err(WorkspaceError::invalid(
                "active job, lease, WorkerSession, and CodexThread identities do not agree",
            ));
        }
        let attempt = u64::try_from(active.lease.attempt)
            .ok()
            .filter(|attempt| *attempt > 0)
            .ok_or_else(|| WorkspaceError::invalid("workspace attempt must be positive"))?;
        for (value, name) in [
            (&active.lease.job_id.0, "execution job"),
            (&active.lease.lease_id.0, "lease"),
            (&active.lease.worker_id.0, "Worker"),
            (&active.lease.worker_instance_id.0, "Worker instance"),
            (&active.worker_session_id.0, "WorkerSession"),
            (
                &active.session_identity.product_session_id.0,
                "ProductSession",
            ),
            (&active.codex_thread_id.0, "CodexThread"),
            (&active.lease.fencing_token.0, "fencing token"),
        ] {
            bounded_identity(value, name)?;
        }
        Ok(Self {
            execution_job_id: active.lease.job_id.clone(),
            execution_job_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&active.job).map_err(|error| {
                    WorkspaceError::io("ExecutionJob workspace authority cannot be encoded", error)
                })?)
            )),
            logical_execution_job_digest: logical_execution_job_digest(&active.job)?,
            lease_id: active.lease.lease_id.clone(),
            attempt,
            fencing_token: active.lease.fencing_token.clone(),
            lease_issued_at: active.lease.issued_at.clone(),
            lease_expires_at: active.lease.expires_at.clone(),
            worker_id: active.lease.worker_id.clone(),
            worker_instance_id: active.lease.worker_instance_id.clone(),
            worker_session_id: active.worker_session_id.clone(),
            product_session_id: active.session_identity.product_session_id.clone(),
            stage_run_id: active.session_identity.stage_run_id.clone(),
            codex_thread_id: active.codex_thread_id.clone(),
        })
    }
}

fn logical_execution_job_digest(job: &ExecutionJob) -> Result<Sha256Digest, WorkspaceError> {
    let mut value = serde_json::to_value(job).map_err(|error| {
        WorkspaceError::io("logical ExecutionJob authority cannot be encoded", error)
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "logical ExecutionJob authority is not an object",
        )
    })?;
    if object.remove("attempt").is_none() {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "logical ExecutionJob authority has no attempt",
        ));
    }
    let bytes = serde_json::to_vec(&value).map_err(|error| {
        WorkspaceError::io("logical ExecutionJob authority cannot be encoded", error)
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn validate_replacement_authority(
    active: &ActiveJob,
    current: &WorkspaceProvenance,
    replacement: Option<&ExecutionJobReplacementAuthority>,
) -> Result<Option<WorkspaceReplacementSeal>, WorkspaceError> {
    let Some(replacement) = replacement else {
        return Ok(None);
    };
    bounded_identity(&replacement.receipt_id.0, "replacement receipt")?;
    if !valid_sha256_digest(&replacement.receipt_digest)
        || replacement.successor_lease != active.lease
        || replacement.scope != active.job.scope
        || replacement.logical_job_digest != current.logical_execution_job_digest
        || replacement.predecessor_lease.job_id != active.job.job_id
        || replacement.successor_lease.job_id != active.job.job_id
        || replacement.predecessor_lease.attempt.saturating_add(1)
            != replacement.successor_lease.attempt
        || replacement.predecessor_lease.worker_id != replacement.successor_lease.worker_id
        || replacement.predecessor_lease.worker_instance_id
            == replacement.successor_lease.worker_instance_id
        || !provenance_matches_lease(current, &replacement.successor_lease)
        || !provenance_matches_session(current, Some(&active.session_identity))
    {
        return Err(WorkspaceError::conflict(
            "replacement receipt does not authorize the active successor",
        ));
    }
    Ok(Some(WorkspaceReplacementSeal {
        receipt_id: replacement.receipt_id.clone(),
        receipt_digest: replacement.receipt_digest.clone(),
        logical_job_digest: replacement.logical_job_digest.clone(),
        scope: replacement.scope.clone(),
        predecessor_lease: replacement.predecessor_lease.clone(),
        predecessor_session_identity: replacement.predecessor_session_identity.clone(),
        successor_lease: replacement.successor_lease.clone(),
        created_at: replacement.created_at.clone(),
    }))
}

fn validate_replacement_predecessor(
    predecessor: &WorkspaceProvenance,
    successor: &WorkspaceProvenance,
    replacement: &WorkspaceReplacementSeal,
) -> Result<(), WorkspaceError> {
    if predecessor.execution_job_id != successor.execution_job_id
        || predecessor.logical_execution_job_digest != successor.logical_execution_job_digest
        || predecessor.logical_execution_job_digest != replacement.logical_job_digest
        || predecessor.attempt.saturating_add(1) != successor.attempt
        || !provenance_matches_lease(predecessor, &replacement.predecessor_lease)
        || !replacement
            .predecessor_session_identity
            .as_ref()
            .is_none_or(|session| provenance_matches_session(predecessor, Some(session)))
        || !provenance_matches_lease(successor, &replacement.successor_lease)
    {
        return Err(WorkspaceError::conflict(
            "replacement receipt does not match the durable predecessor checkout",
        ));
    }
    Ok(())
}

fn provenance_matches_lease(provenance: &WorkspaceProvenance, lease: &ExecutionLeaseStamp) -> bool {
    u64::try_from(lease.attempt).ok() == Some(provenance.attempt)
        && provenance.execution_job_id == lease.job_id
        && provenance.lease_id == lease.lease_id
        && provenance.fencing_token == lease.fencing_token
        && provenance.lease_issued_at == lease.issued_at
        && provenance.lease_expires_at == lease.expires_at
        && provenance.worker_id == lease.worker_id
        && provenance.worker_instance_id == lease.worker_instance_id
}

fn provenance_matches_session(
    provenance: &WorkspaceProvenance,
    session: Option<&winwincode_domain::SessionIdentity>,
) -> bool {
    session.is_some_and(|session| {
        provenance.worker_session_id == session.worker_session_id
            && provenance.product_session_id == session.product_session_id
            && provenance.stage_run_id == session.stage_run_id
            && provenance.codex_thread_id == session.codex_thread_id
    })
}

fn valid_sha256_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Paths allocated exclusively to one active write job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLayout {
    root: PathBuf,
    checkout: PathBuf,
    home: PathBuf,
    sandbox: PathBuf,
    temporary: PathBuf,
    artifacts: PathBuf,
}

impl WorkspaceLayout {
    /// Entire job-private directory removed at terminal cleanup.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Detached Git worktree used by the embedded executor.
    #[must_use]
    pub fn checkout(&self) -> &Path {
        &self.checkout
    }

    /// Job-private home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Job-private sandbox state directory.
    #[must_use]
    pub fn sandbox(&self) -> &Path {
        &self.sandbox
    }

    /// Job-private temporary directory.
    #[must_use]
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    /// Job-private Artifact staging directory.
    #[must_use]
    pub fn artifacts(&self) -> &Path {
        &self.artifacts
    }
}

/// Why an isolated workspace reached terminal cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceCloseReason {
    Completed,
    Cancelled,
    Failed,
}

/// Evidence that one job-private directory was removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCleanupReport {
    pub workspace_id: String,
    pub reason: WorkspaceCloseReason,
    pub removed_root: PathBuf,
}

/// One file in a complete Artifact staging snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceArtifactFile {
    pub relative_path: String,
    pub size_bytes: u64,
    pub digest: Sha256Digest,
}

/// Verifiable Artifact-set identity bound to exact execution authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceArtifactSnapshot {
    pub provenance: WorkspaceProvenance,
    pub files: Vec<WorkspaceArtifactFile>,
    pub content_digest: Sha256Digest,
}

/// Immutable candidate identity produced from the detached worktree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSnapshot {
    pub repository_id: RepositoryId,
    pub checkout_revision: String,
    pub source_commit_id: String,
    pub source_tree_id: String,
    pub candidate_commit_id: String,
    pub candidate_tree_id: String,
    pub content_digest: Sha256Digest,
    pub origin_provenance: WorkspaceProvenance,
    pub provenance: WorkspaceProvenance,
    #[serde(skip)]
    manifest_bytes: Vec<u8>,
}

impl CandidateSnapshot {
    /// Canonical candidate Artifact bytes consumed by the trusted source resolver.
    #[must_use]
    pub fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateArtifactManifest<'a> {
    schema_version: u8,
    candidate_commit_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateCommitBinding<'a> {
    schema_version: u8,
    repository_id: &'a RepositoryId,
    source_commit_id: &'a str,
    origin_provenance: &'a WorkspaceProvenance,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceReplacementSeal {
    receipt_id: RequestId,
    receipt_digest: Sha256Digest,
    logical_job_digest: Sha256Digest,
    scope: ExecutionScope,
    predecessor_lease: ExecutionLeaseStamp,
    predecessor_session_identity: Option<winwincode_domain::SessionIdentity>,
    successor_lease: ExecutionLeaseStamp,
    created_at: Instant,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryManifest {
    schema_version: u8,
    phase: WorkspaceRecoveryPhase,
    workspace_id: String,
    source_repository: String,
    repository_id: RepositoryId,
    checkout_revision: String,
    source_commit_id: String,
    source_tree_id: String,
    origin_provenance: WorkspaceProvenance,
    current_provenance: WorkspaceProvenance,
    replacement: Option<WorkspaceReplacementSeal>,
    max_artifact_bytes: u64,
}

/// Factory and startup recovery boundary for Worker-local workspaces.
#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    root: PathBuf,
    source_root: PathBuf,
}

#[derive(Clone)]
struct WorkspacePlan {
    workspace_id: String,
    source_repository: PathBuf,
    repository_id: RepositoryId,
    checkout_revision: String,
    source_commit_id: String,
    source_tree_id: String,
    origin_provenance: WorkspaceProvenance,
    current_provenance: WorkspaceProvenance,
    replacement: Option<WorkspaceReplacementSeal>,
    max_artifact_bytes: u64,
    layout: WorkspaceLayout,
}

struct WorkspaceRequest {
    source_repository: PathBuf,
    repository_id: RepositoryId,
    checkout_revision: String,
    provenance: WorkspaceProvenance,
    max_artifact_bytes: u64,
}

enum WorkspaceRecoveryEntry {
    Foreign,
    UnboundCleanPredecessor(Box<ReservedWorkspacePlan>),
    Recovered(Box<ReservedWorkspacePlan>),
}

struct ReservedWorkspacePlan {
    plan: WorkspacePlan,
    owner_lock: File,
}

impl WorkspacePlan {
    fn into_workspace(self, owner_lock: File) -> WorkerWorkspace {
        WorkerWorkspace {
            workspace_id: self.workspace_id,
            source_repository: self.source_repository,
            repository_id: self.repository_id,
            checkout_revision: self.checkout_revision,
            source_commit_id: self.source_commit_id,
            source_tree_id: self.source_tree_id,
            origin_provenance: self.origin_provenance,
            current_provenance: self.current_provenance,
            max_artifact_bytes: self.max_artifact_bytes,
            layout: self.layout,
            _owner_lock: owner_lock,
        }
    }
}

impl ReservedWorkspacePlan {
    fn into_workspace(self) -> WorkerWorkspace {
        self.plan.into_workspace(self.owner_lock)
    }
}

impl WorkspaceManager {
    fn reconcile_cleaning_workspaces(&self) -> Result<(), WorkspaceError> {
        reconcile_cleaning_workspaces(&self.root, &self.source_root)
    }

    /// Opens controlled source and workspace roots.
    ///
    /// # Errors
    ///
    /// Rejects missing source roots and overlapping source/workspace roots.
    pub fn open(
        root: impl AsRef<Path>,
        source_root: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceError> {
        fs::create_dir_all(root.as_ref())
            .map_err(|error| WorkspaceError::io("workspace root cannot be created", error))?;
        let root = fs::canonicalize(root.as_ref())
            .map_err(|error| WorkspaceError::io("workspace root cannot be opened", error))?;
        let source_root = fs::canonicalize(source_root.as_ref())
            .map_err(|error| WorkspaceError::io("source root cannot be opened", error))?;
        if !root.is_dir()
            || !source_root.is_dir()
            || root.starts_with(&source_root)
            || source_root.starts_with(&root)
        {
            return Err(WorkspaceError::invalid(
                "source and workspace roots must be distinct non-overlapping directories",
            ));
        }
        let manager = Self { root, source_root };
        manager.reconcile_cleaning_workspaces()?;
        Ok(manager)
    }

    /// Creates one exclusive detached worktree for an authenticated active job.
    ///
    /// # Errors
    ///
    /// Rejects mismatched authority, foreign repositories, duplicate opens,
    /// invalid revisions, and filesystem or Git failures.
    pub fn create(&self, active: &ActiveJob) -> Result<WorkerWorkspace, WorkspaceError> {
        let plan = self.plan(active)?;
        Self::create_plan(plan)
    }

    fn create_plan(plan: WorkspacePlan) -> Result<WorkerWorkspace, WorkspaceError> {
        Self::create_plan_with_interruption(plan, CreationInterruption::None)
    }

    fn create_plan_with_interruption(
        plan: WorkspacePlan,
        interruption: CreationInterruption,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        let rollback =
            (interruption == CreationInterruption::None).then_some(CleanupInterruption::None);
        Self::create_plan_attempt(plan, interruption, rollback)
    }

    fn create_plan_attempt(
        plan: WorkspacePlan,
        interruption: CreationInterruption,
        rollback: Option<CleanupInterruption>,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        let owner_lock = acquire_workspace_owner_lock(&plan.layout)?;
        let mut manifest = recovery_manifest(&plan, WorkspaceRecoveryPhase::Creating)?;
        write_creation_intent(&plan.layout, &manifest)?;
        let result = create_workspace_from_intent(&plan, &mut manifest, interruption);
        if let Err(error) = result {
            let Some(rollback) = rollback else {
                return Err(error);
            };
            remove_workspace_with_interruption(&plan.source_repository, &plan.layout, rollback)?;
            remove_creation_intent(&plan.layout)?;
            return Err(error);
        }
        Ok(plan.into_workspace(owner_lock))
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn create_with_failed_rollback_for_test(
        &self,
        active: &ActiveJob,
        rollback_failure: WorkspaceCreationRollbackFailure,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        Self::create_plan_attempt(
            self.plan(active)?,
            CreationInterruption::AfterWorktreeAdded,
            Some(rollback_failure.into()),
        )
    }

    /// Creates the exact Job worktree or resumes its original private checkout.
    ///
    /// Recovery never rebuilds mutable files from source. The deterministic
    /// workspace identity, canonical manifest, controlled repository, private
    /// directory layout, Git worktree root, source commit, and full active Job
    /// provenance must all still match before the checkout is returned.
    ///
    /// # Errors
    ///
    /// Rejects foreign/corrupt directories, changed authority, links, missing
    /// worktree state, or any normal creation failure.
    pub fn create_or_recover(
        &self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        self.create_or_recover_inner(active, replacement, CreationInterruption::None)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn create_or_recover_interrupted(
        &self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
        interruption: WorkspaceCreationInterruption,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        self.create_or_recover_inner(active, replacement, interruption.into())
    }

    fn create_or_recover_inner(
        &self,
        active: &ActiveJob,
        replacement: Option<&ExecutionJobReplacementAuthority>,
        interruption: CreationInterruption,
    ) -> Result<WorkerWorkspace, WorkspaceError> {
        let request = self.request(active)?;
        let replacement = validate_replacement_authority(active, &request.provenance, replacement)?;
        let reconciled = self.reconcile_creation_intents(&request, replacement.as_ref())?;
        if let Some(workspace) =
            self.recovery_plan(&request, replacement.as_ref(), interruption, reconciled)?
        {
            return Ok(workspace);
        }
        if replacement
            .as_ref()
            .and_then(|seal| seal.predecessor_session_identity.as_ref())
            .is_some()
        {
            return Err(WorkspaceError::conflict(
                "replacement of a bound predecessor requires its durable checkout",
            ));
        }
        let plan = self.resolve_plan(request, replacement)?;
        Self::create_plan_with_interruption(plan, interruption)
    }

    fn reconcile_creation_intents(
        &self,
        request: &WorkspaceRequest,
        replacement: Option<&WorkspaceReplacementSeal>,
    ) -> Result<Option<WorkerWorkspace>, WorkspaceError> {
        let mut exact = None;
        let mut predecessor = None;
        for entry in fs::read_dir(&self.root)
            .map_err(|error| WorkspaceError::io("workspace root cannot be scanned", error))?
        {
            let entry = entry
                .map_err(|error| WorkspaceError::io("workspace intent cannot be read", error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(CREATION_INTENT_PREFIX) || !name.ends_with(CREATION_INTENT_SUFFIX)
            {
                continue;
            }
            let manifest = read_manifest_file(&entry.path(), "creation intent")?;
            if manifest.current_provenance.execution_job_id != request.provenance.execution_job_id {
                continue;
            }
            validate_recovery_request(&manifest, request)?;
            let source_repository =
                controlled_recovery_repository(&self.source_root, &manifest.source_repository)?;
            validate_recovery_identity(&manifest, request, &source_repository)?;
            if manifest.phase != WorkspaceRecoveryPhase::Creating {
                return Err(WorkspaceError::conflict(
                    "workspace creation intent is not in its durable creating phase",
                ));
            }
            let plan = recovery_workspace_plan(
                &manifest,
                source_repository,
                &self.root.join(&manifest.workspace_id),
            );
            if manifest.current_provenance == request.provenance {
                if manifest.replacement.as_ref() != replacement || exact.replace(plan).is_some() {
                    return Err(duplicate_workspace_error());
                }
                continue;
            }
            let replacement = replacement.ok_or_else(|| {
                WorkspaceError::conflict("workspace creation intent differs from active authority")
            })?;
            if replacement.predecessor_session_identity.is_some() {
                return Err(WorkspaceError::conflict(
                    "an unfinished workspace cannot have an accepted predecessor session",
                ));
            }
            validate_replacement_predecessor(
                &manifest.current_provenance,
                &request.provenance,
                replacement,
            )?;
            if predecessor.replace(plan).is_some() {
                return Err(duplicate_workspace_error());
            }
        }
        let mut exact_workspace = exact
            .as_ref()
            .map(reconcile_exact_creation_intent)
            .transpose()?;
        let Some(predecessor) = predecessor else {
            return Ok(exact_workspace);
        };
        let _predecessor_lock = acquire_workspace_owner_lock(&predecessor.layout)?;
        let replacement = replacement.ok_or_else(|| {
            WorkspaceError::conflict("predecessor creation intent requires replacement authority")
        })?;
        let successor =
            replacement_plan_from_predecessor(&self.root, &predecessor, request, replacement)?;
        let successor_workspace = if let Some(workspace) = exact_workspace.take() {
            validate_active_successor(&successor)?;
            workspace
        } else {
            ensure_active_successor(&successor)?
        };
        cleanup_creating_predecessor(&predecessor)?;
        Ok(Some(successor_workspace))
    }

    fn recovery_plan(
        &self,
        request: &WorkspaceRequest,
        replacement: Option<&WorkspaceReplacementSeal>,
        interruption: CreationInterruption,
        preowned: Option<WorkerWorkspace>,
    ) -> Result<Option<WorkerWorkspace>, WorkspaceError> {
        let mut entries = fs::read_dir(&self.root)
            .map_err(|error| WorkspaceError::io("workspace root cannot be scanned", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| WorkspaceError::io("workspace entry cannot be read", error))?;
        entries.sort_by_key(fs::DirEntry::file_name);
        let mut recovered = preowned;
        let mut unbound_clean_predecessor = None;
        for entry in entries {
            if !entry
                .file_type()
                .map_err(|error| WorkspaceError::io("workspace entry type cannot be read", error))?
                .is_dir()
            {
                continue;
            }
            if recovered
                .as_ref()
                .is_some_and(|workspace| workspace.layout.root == entry.path())
            {
                continue;
            }
            match self.recover_entry(&entry, request, replacement)? {
                WorkspaceRecoveryEntry::Foreign => {}
                WorkspaceRecoveryEntry::UnboundCleanPredecessor(plan) => {
                    if unbound_clean_predecessor.is_some() {
                        return Err(duplicate_workspace_error());
                    }
                    unbound_clean_predecessor = Some(*plan);
                }
                WorkspaceRecoveryEntry::Recovered(plan) => {
                    if recovered.is_some() {
                        return Err(duplicate_workspace_error());
                    }
                    recovered = Some((*plan).into_workspace());
                }
            }
        }
        let Some(predecessor) = unbound_clean_predecessor else {
            return Ok(recovered);
        };
        if let Some(recovered) = recovered {
            mark_workspace_cleaning(&predecessor.plan.layout)?;
            remove_workspace(
                &predecessor.plan.source_repository,
                &predecessor.plan.layout,
            )?;
            return Ok(Some(recovered));
        }
        let replacement = replacement.ok_or_else(|| {
            WorkspaceError::conflict("unbound predecessor requires a replacement receipt")
        })?;
        let successor =
            replacement_plan_from_predecessor(&self.root, &predecessor.plan, request, replacement)?;
        let workspace = Self::create_plan_with_interruption(successor, interruption)?;
        mark_workspace_cleaning(&predecessor.plan.layout)?;
        remove_workspace(
            &predecessor.plan.source_repository,
            &predecessor.plan.layout,
        )?;
        Ok(Some(workspace))
    }

    fn recover_entry(
        &self,
        entry: &fs::DirEntry,
        request: &WorkspaceRequest,
        replacement: Option<&WorkspaceReplacementSeal>,
    ) -> Result<WorkspaceRecoveryEntry, WorkspaceError> {
        let path = entry.path();
        if !path.join(RECOVERY_MANIFEST).exists()
            && let Some(intent) = read_creation_intent_for_workspace(&path)?
            && intent.current_provenance.execution_job_id != request.provenance.execution_job_id
        {
            return Ok(WorkspaceRecoveryEntry::Foreign);
        }
        let mut manifest = read_recovery_manifest(&path)?;
        if manifest.workspace_id != entry.file_name().to_string_lossy() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "recovery manifest identity does not match its directory",
            ));
        }
        if manifest.current_provenance.execution_job_id != request.provenance.execution_job_id {
            return Ok(WorkspaceRecoveryEntry::Foreign);
        }
        if manifest.phase != WorkspaceRecoveryPhase::Active {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "workspace manifest is not active after recovery reconciliation",
            ));
        }
        validate_recovery_request(&manifest, request)?;
        let source_repository =
            controlled_recovery_repository(&self.source_root, &manifest.source_repository)?;
        validate_recovery_identity(&manifest, request, &source_repository)?;
        let recovered_plan = recovery_workspace_plan(&manifest, source_repository.clone(), &path);
        validate_recovered_plan(&recovered_plan)?;
        let owner_lock = acquire_workspace_owner_lock(&recovered_plan.layout)?;
        if manifest.current_provenance == request.provenance {
            if manifest.replacement.as_ref() != replacement {
                return Err(WorkspaceError::conflict(
                    "workspace replacement receipt differs from its current authority",
                ));
            }
            return Ok(WorkspaceRecoveryEntry::Recovered(Box::new(
                ReservedWorkspacePlan {
                    plan: recovered_plan,
                    owner_lock,
                },
            )));
        }
        let replacement = replacement.ok_or_else(|| {
            WorkspaceError::conflict(
                "an existing checkout for this Job has different sealed authority",
            )
        })?;
        validate_replacement_predecessor(
            &manifest.current_provenance,
            &request.provenance,
            replacement,
        )?;
        if replacement.predecessor_session_identity.is_none() {
            if !workspace_checkout_clean(&recovered_plan.layout.checkout)? {
                return Err(WorkspaceError::conflict(
                    "unbound predecessor checkout contains unaccepted changes",
                ));
            }
            return Ok(WorkspaceRecoveryEntry::UnboundCleanPredecessor(Box::new(
                ReservedWorkspacePlan {
                    plan: recovered_plan,
                    owner_lock,
                },
            )));
        }
        manifest.current_provenance = request.provenance.clone();
        manifest.replacement = Some(replacement.clone());
        replace_recovery_manifest(&path, &manifest)?;
        Ok(WorkspaceRecoveryEntry::Recovered(Box::new(
            ReservedWorkspacePlan {
                plan: recovery_workspace_plan(&manifest, source_repository, &path),
                owner_lock,
            },
        )))
    }

    fn request(&self, active: &ActiveJob) -> Result<WorkspaceRequest, WorkspaceError> {
        let provenance = WorkspaceProvenance::from_active_job(active)?;
        validate_repository_id(&active.job.workspace.repository_id)?;
        bounded_revision(&active.job.workspace.checkout_revision)?;
        let source_repository =
            controlled_repository(&self.source_root, &active.job.workspace.repository_id)?;
        assert_git_repository(&source_repository)?;
        let max_artifact_bytes = u64::try_from(active.job.limits.max_artifact_bytes)
            .map_err(|_| WorkspaceError::invalid("Artifact byte limit must not be negative"))?;
        Ok(WorkspaceRequest {
            source_repository,
            repository_id: active.job.workspace.repository_id.clone(),
            checkout_revision: active.job.workspace.checkout_revision.clone(),
            provenance,
            max_artifact_bytes,
        })
    }

    fn resolve_plan(
        &self,
        request: WorkspaceRequest,
        replacement: Option<WorkspaceReplacementSeal>,
    ) -> Result<WorkspacePlan, WorkspaceError> {
        let source_commit_id = rev_parse(
            &request.source_repository,
            &format!("{}^{{commit}}", request.checkout_revision),
        )?;
        let source_tree_id = rev_parse(
            &request.source_repository,
            &format!("{source_commit_id}^{{tree}}"),
        )?;
        let workspace_id = workspace_id(
            &request.repository_id,
            &source_commit_id,
            &request.provenance,
        )?;
        let layout = layout(self.root.join(&workspace_id));
        Ok(WorkspacePlan {
            workspace_id,
            source_repository: request.source_repository,
            repository_id: request.repository_id,
            checkout_revision: request.checkout_revision,
            source_commit_id,
            source_tree_id,
            origin_provenance: request.provenance.clone(),
            current_provenance: request.provenance,
            replacement,
            max_artifact_bytes: request.max_artifact_bytes,
            layout,
        })
    }

    fn plan(&self, active: &ActiveJob) -> Result<WorkspacePlan, WorkspaceError> {
        let request = self.request(active)?;
        self.resolve_plan(request, None)
    }
}

fn replacement_plan_from_predecessor(
    workspace_root: &Path,
    predecessor: &WorkspacePlan,
    request: &WorkspaceRequest,
    replacement: &WorkspaceReplacementSeal,
) -> Result<WorkspacePlan, WorkspaceError> {
    let workspace_id = workspace_id(
        &predecessor.repository_id,
        &predecessor.source_commit_id,
        &request.provenance,
    )?;
    Ok(WorkspacePlan {
        workspace_id: workspace_id.clone(),
        source_repository: predecessor.source_repository.clone(),
        repository_id: predecessor.repository_id.clone(),
        checkout_revision: predecessor.checkout_revision.clone(),
        source_commit_id: predecessor.source_commit_id.clone(),
        source_tree_id: predecessor.source_tree_id.clone(),
        origin_provenance: request.provenance.clone(),
        current_provenance: request.provenance.clone(),
        replacement: Some(replacement.clone()),
        max_artifact_bytes: predecessor.max_artifact_bytes,
        layout: layout(workspace_root.join(workspace_id)),
    })
}

fn reconcile_exact_creation_intent(
    plan: &WorkspacePlan,
) -> Result<WorkerWorkspace, WorkspaceError> {
    let owner_lock = acquire_workspace_owner_lock(&plan.layout)?;
    remove_workspace(&plan.source_repository, &plan.layout)?;
    let mut manifest = recovery_manifest(plan, WorkspaceRecoveryPhase::Creating)?;
    create_workspace_from_intent(plan, &mut manifest, CreationInterruption::None)?;
    Ok(plan.clone().into_workspace(owner_lock))
}

fn ensure_active_successor(plan: &WorkspacePlan) -> Result<WorkerWorkspace, WorkspaceError> {
    if !plan.layout.root.exists() {
        return WorkspaceManager::create_plan(plan.clone());
    }
    let owner_lock = acquire_workspace_owner_lock(&plan.layout)?;
    validate_active_successor(plan)?;
    Ok(plan.clone().into_workspace(owner_lock))
}

fn validate_active_successor(plan: &WorkspacePlan) -> Result<(), WorkspaceError> {
    let manifest = read_recovery_manifest(&plan.layout.root)?;
    let recovered =
        recovery_workspace_plan(&manifest, plan.source_repository.clone(), &plan.layout.root);
    if manifest.phase != WorkspaceRecoveryPhase::Active
        || manifest.current_provenance != plan.current_provenance
        || manifest.origin_provenance != plan.origin_provenance
        || manifest.replacement != plan.replacement
        || manifest.source_commit_id != plan.source_commit_id
        || manifest.source_tree_id != plan.source_tree_id
    {
        return Err(WorkspaceError::conflict(
            "precreated successor differs from replacement authority",
        ));
    }
    validate_recovered_plan(&recovered)
}

fn cleanup_creating_predecessor(plan: &WorkspacePlan) -> Result<(), WorkspaceError> {
    if plan.layout.checkout.join(".git").exists() {
        match workspace_checkout_clean(&plan.layout.checkout) {
            Ok(true) | Err(_) => {}
            Ok(false) => {
                return Err(WorkspaceError::conflict(
                    "unfinished predecessor workspace contains unaccepted changes",
                ));
            }
        }
    }
    remove_workspace(&plan.source_repository, &plan.layout)?;
    remove_creation_intent(&plan.layout)
}

fn duplicate_workspace_error() -> WorkspaceError {
    WorkspaceError::conflict("more than one checkout exists for the same sealed Job authority")
}

fn validate_recovery_request(
    manifest: &RecoveryManifest,
    request: &WorkspaceRequest,
) -> Result<(), WorkspaceError> {
    if manifest.repository_id != request.repository_id
        || manifest.checkout_revision != request.checkout_revision
        || manifest.max_artifact_bytes != request.max_artifact_bytes
    {
        return Err(WorkspaceError::conflict(
            "an existing checkout for this Job has different sealed authority",
        ));
    }
    Ok(())
}

fn validate_recovery_identity(
    manifest: &RecoveryManifest,
    request: &WorkspaceRequest,
    source_repository: &Path,
) -> Result<(), WorkspaceError> {
    if source_repository != request.source_repository
        || workspace_id(
            &manifest.repository_id,
            &manifest.source_commit_id,
            &manifest.origin_provenance,
        )? != manifest.workspace_id
    {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "workspace recovery authority changed",
        ));
    }
    Ok(())
}

fn recovery_workspace_plan(
    manifest: &RecoveryManifest,
    source_repository: PathBuf,
    path: &Path,
) -> WorkspacePlan {
    WorkspacePlan {
        workspace_id: manifest.workspace_id.clone(),
        source_repository,
        repository_id: manifest.repository_id.clone(),
        checkout_revision: manifest.checkout_revision.clone(),
        source_commit_id: manifest.source_commit_id.clone(),
        source_tree_id: manifest.source_tree_id.clone(),
        origin_provenance: manifest.origin_provenance.clone(),
        current_provenance: manifest.current_provenance.clone(),
        replacement: manifest.replacement.clone(),
        max_artifact_bytes: manifest.max_artifact_bytes,
        layout: layout(path.to_path_buf()),
    }
}

/// Live exclusive workspace for one active job.
#[derive(Debug)]
pub struct WorkerWorkspace {
    workspace_id: String,
    source_repository: PathBuf,
    repository_id: RepositoryId,
    checkout_revision: String,
    source_commit_id: String,
    source_tree_id: String,
    origin_provenance: WorkspaceProvenance,
    current_provenance: WorkspaceProvenance,
    max_artifact_bytes: u64,
    layout: WorkspaceLayout,
    _owner_lock: File,
}

impl WorkerWorkspace {
    /// Stable opaque workspace identity derived from source and lease authority.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.workspace_id
    }

    /// Job-private directory layout.
    #[must_use]
    pub const fn layout(&self) -> &WorkspaceLayout {
        &self.layout
    }

    /// Immutable authority bound to all snapshots produced here.
    #[must_use]
    pub const fn provenance(&self) -> &WorkspaceProvenance {
        &self.current_provenance
    }

    /// Immutable authority that originally allocated this checkout and binds
    /// deterministic candidate commits across sealed Worker replacement.
    #[must_use]
    pub const fn origin_provenance(&self) -> &WorkspaceProvenance {
        &self.origin_provenance
    }

    /// Resolves a relative checkout path without permitting traversal or links.
    ///
    /// # Errors
    ///
    /// Rejects absolute paths, parent traversal, and symbolic links.
    pub fn checkout_path(&self, relative: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        controlled_path(&self.layout.checkout, relative.as_ref(), false)
    }

    /// Writes one new Artifact file below the job-private staging directory.
    ///
    /// # Errors
    ///
    /// Rejects traversal, links, replacement, or writes beyond the job limit.
    pub fn write_artifact(
        &mut self,
        relative: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<WorkspaceArtifactFile, WorkspaceError> {
        let current = artifact_files(&self.layout.artifacts)?;
        let current_bytes = current.iter().try_fold(0_u64, |total, file| {
            total
                .checked_add(file.size_bytes)
                .ok_or_else(|| WorkspaceError::invalid("Artifact byte accounting overflowed"))
        })?;
        let size_bytes = u64::try_from(bytes.len())
            .map_err(|_| WorkspaceError::invalid("Artifact size is unsupported"))?;
        if current_bytes
            .checked_add(size_bytes)
            .is_none_or(|total| total > self.max_artifact_bytes)
        {
            return Err(WorkspaceError::invalid(
                "Artifact staging exceeds the job byte limit",
            ));
        }
        let path = controlled_path(&self.layout.artifacts, relative.as_ref(), true)?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkspaceError::invalid("Artifact path has no parent"))?;
        fs::create_dir_all(parent)
            .map_err(|error| WorkspaceError::io("Artifact parent cannot be created", error))?;
        ensure_no_links(&self.layout.artifacts, parent)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                WorkspaceError::new(
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        WorkspaceErrorCode::Conflict
                    } else {
                        WorkspaceErrorCode::Io
                    },
                    format!("Artifact file cannot be created: {error}"),
                )
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| WorkspaceError::io("Artifact file cannot be persisted", error))?;
        artifact_file(&self.layout.artifacts, &path)
    }

    /// Freezes checkout changes into a deterministic detached candidate commit.
    ///
    /// # Errors
    ///
    /// Rejects an unchanged tree, unsupported path types, or Git failures.
    pub fn snapshot_candidate(&mut self) -> Result<CandidateSnapshot, WorkspaceError> {
        git_status(&self.layout.checkout, &["add", "--all"])?;
        let candidate_tree_id = git_text(&self.layout.checkout, &["write-tree"])?;
        if candidate_tree_id == self.source_tree_id {
            return Err(WorkspaceError::conflict(
                "candidate tree has no source changes",
            ));
        }
        let binding = CandidateCommitBinding {
            schema_version: CANDIDATE_MANIFEST_SCHEMA,
            repository_id: &self.repository_id,
            source_commit_id: &self.source_commit_id,
            origin_provenance: &self.origin_provenance,
        };
        let message = serde_json::to_vec(&binding).map_err(|error| {
            WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                format!("candidate binding cannot be encoded: {error}"),
            )
        })?;
        let candidate_commit_id = commit_tree(
            &self.layout.checkout,
            &candidate_tree_id,
            &self.source_commit_id,
            &message,
        )?;
        let content_digest = tree_digest(&self.layout.checkout, &candidate_commit_id)?;
        let manifest_bytes = serde_json::to_vec(&CandidateArtifactManifest {
            schema_version: CANDIDATE_MANIFEST_SCHEMA,
            candidate_commit_id: &candidate_commit_id,
        })
        .map_err(|error| WorkspaceError::io("candidate manifest cannot be encoded", error))?;
        Ok(CandidateSnapshot {
            repository_id: self.repository_id.clone(),
            checkout_revision: self.checkout_revision.clone(),
            source_commit_id: self.source_commit_id.clone(),
            source_tree_id: self.source_tree_id.clone(),
            candidate_commit_id,
            candidate_tree_id,
            content_digest,
            origin_provenance: self.origin_provenance.clone(),
            provenance: self.current_provenance.clone(),
            manifest_bytes,
        })
    }

    /// Captures the exact clean checkout used by a read-only verification Job.
    ///
    /// Verification workspaces are created from the frozen writer candidate
    /// commit. They must not create a second commit or mutate the checkout;
    /// this snapshot binds the existing commit and tree to the verifier's
    /// lease so its terminal outcome can carry an independently acknowledged
    /// candidate Artifact.
    ///
    /// # Errors
    ///
    /// Rejects a dirty checkout, an invalid Git head, or Git failures.
    pub fn snapshot_verification(&self) -> Result<CandidateSnapshot, WorkspaceError> {
        if !workspace_checkout_clean(&self.layout.checkout)? {
            return Err(WorkspaceError::conflict(
                "verification checkout contains unaccepted changes",
            ));
        }
        let candidate_commit_id = rev_parse(&self.layout.checkout, "HEAD^{commit}")?;
        let candidate_tree_id = rev_parse(
            &self.layout.checkout,
            &format!("{candidate_commit_id}^{{tree}}"),
        )?;
        let content_digest = tree_digest(&self.layout.checkout, &candidate_commit_id)?;
        let manifest_bytes = serde_json::to_vec(&CandidateArtifactManifest {
            schema_version: CANDIDATE_MANIFEST_SCHEMA,
            candidate_commit_id: &candidate_commit_id,
        })
        .map_err(|error| WorkspaceError::io("candidate manifest cannot be encoded", error))?;
        Ok(CandidateSnapshot {
            repository_id: self.repository_id.clone(),
            checkout_revision: self.checkout_revision.clone(),
            source_commit_id: self.source_commit_id.clone(),
            source_tree_id: self.source_tree_id.clone(),
            candidate_commit_id,
            candidate_tree_id,
            content_digest,
            origin_provenance: self.origin_provenance.clone(),
            provenance: self.current_provenance.clone(),
            manifest_bytes,
        })
    }

    /// Rebuilds a candidate's source, tree, digest, manifest, and provenance.
    ///
    /// # Errors
    ///
    /// Returns `DigestMismatch` for altered or foreign snapshot facts.
    pub fn verify_candidate(&self, snapshot: &CandidateSnapshot) -> Result<(), WorkspaceError> {
        let candidate_tree = rev_parse(
            &self.layout.checkout,
            &format!("{}^{{tree}}", snapshot.candidate_commit_id),
        )?;
        let candidate_parent = rev_parse(
            &self.layout.checkout,
            &format!("{}^", snapshot.candidate_commit_id),
        )?;
        let expected_manifest = serde_json::to_vec(&CandidateArtifactManifest {
            schema_version: CANDIDATE_MANIFEST_SCHEMA,
            candidate_commit_id: &snapshot.candidate_commit_id,
        })
        .map_err(|error| WorkspaceError::io("candidate manifest cannot be encoded", error))?;
        let digest = tree_digest(&self.layout.checkout, &snapshot.candidate_commit_id)?;
        if snapshot.repository_id != self.repository_id
            || snapshot.checkout_revision != self.checkout_revision
            || snapshot.source_commit_id != self.source_commit_id
            || snapshot.source_tree_id != self.source_tree_id
            || snapshot.origin_provenance != self.origin_provenance
            || snapshot.provenance != self.current_provenance
            || candidate_parent != self.source_commit_id
            || candidate_tree != snapshot.candidate_tree_id
            || digest != snapshot.content_digest
            || snapshot.manifest_bytes != expected_manifest
        {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::DigestMismatch,
                "candidate source, authority, or content identity does not match",
            ));
        }
        Ok(())
    }

    /// Rebuilds and verifies a read-only verification snapshot.
    ///
    /// Unlike an executor candidate, a verifier's candidate commit is the
    /// workspace source commit itself, so no parent/source rewrite is allowed.
    ///
    /// # Errors
    ///
    /// Returns `DigestMismatch` for altered or foreign snapshot facts.
    pub fn verify_verification(&self, snapshot: &CandidateSnapshot) -> Result<(), WorkspaceError> {
        let checkout_head = rev_parse(&self.layout.checkout, "HEAD^{commit}")?;
        let candidate_tree = rev_parse(
            &self.layout.checkout,
            &format!("{}^{{tree}}", snapshot.candidate_commit_id),
        )?;
        let expected_manifest = serde_json::to_vec(&CandidateArtifactManifest {
            schema_version: CANDIDATE_MANIFEST_SCHEMA,
            candidate_commit_id: &snapshot.candidate_commit_id,
        })
        .map_err(|error| WorkspaceError::io("candidate manifest cannot be encoded", error))?;
        let digest = tree_digest(&self.layout.checkout, &snapshot.candidate_commit_id)?;
        if !workspace_checkout_clean(&self.layout.checkout)?
            || snapshot.repository_id != self.repository_id
            || snapshot.checkout_revision != self.checkout_revision
            || snapshot.source_commit_id != self.source_commit_id
            || snapshot.source_tree_id != self.source_tree_id
            || snapshot.origin_provenance != self.origin_provenance
            || snapshot.provenance != self.current_provenance
            || checkout_head != self.source_commit_id
            || snapshot.candidate_commit_id != self.source_commit_id
            || candidate_tree != self.source_tree_id
            || candidate_tree != snapshot.candidate_tree_id
            || digest != snapshot.content_digest
            || snapshot.manifest_bytes != expected_manifest
        {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::DigestMismatch,
                "verification source, authority, or content identity does not match",
            ));
        }
        Ok(())
    }

    /// Captures every staged Artifact file and one aggregate content digest.
    ///
    /// # Errors
    ///
    /// Rejects links, non-regular files, non-portable paths, and read failures.
    pub fn snapshot_artifacts(&self) -> Result<WorkspaceArtifactSnapshot, WorkspaceError> {
        let files = artifact_files(&self.layout.artifacts)?;
        let content_digest = artifact_set_digest(&files);
        Ok(WorkspaceArtifactSnapshot {
            provenance: self.current_provenance.clone(),
            files,
            content_digest,
        })
    }

    /// Recomputes all Artifact file and aggregate digests.
    ///
    /// # Errors
    ///
    /// Returns `DigestMismatch` when bytes or provenance changed.
    pub fn verify_artifacts(
        &self,
        snapshot: &WorkspaceArtifactSnapshot,
    ) -> Result<(), WorkspaceError> {
        let rebuilt = self.snapshot_artifacts()?;
        if rebuilt != *snapshot {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::DigestMismatch,
                "Artifact content or authority identity does not match",
            ));
        }
        Ok(())
    }

    /// Removes the worktree, sandbox, home, temporary files, and Artifact staging.
    ///
    /// # Errors
    ///
    /// Returns a Git or filesystem error after cleanup was attempted.
    pub fn close(
        mut self,
        reason: WorkspaceCloseReason,
    ) -> Result<WorkspaceCleanupReport, WorkspaceError> {
        self.close_in_place(reason)
    }

    pub(crate) fn close_in_place(
        &mut self,
        reason: WorkspaceCloseReason,
    ) -> Result<WorkspaceCleanupReport, WorkspaceError> {
        self.close_in_place_with_interruption(reason, CleanupInterruption::None)
    }

    fn close_in_place_with_interruption(
        &mut self,
        reason: WorkspaceCloseReason,
        interruption: CleanupInterruption,
    ) -> Result<WorkspaceCleanupReport, WorkspaceError> {
        mark_workspace_cleaning(&self.layout)?;
        interrupt_cleanup(interruption, CleanupInterruptionPoint::ManifestMarked)?;
        remove_workspace_with_interruption(&self.source_repository, &self.layout, interruption)?;
        Ok(WorkspaceCleanupReport {
            workspace_id: self.workspace_id.clone(),
            reason,
            removed_root: self.layout.root.clone(),
        })
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn close_in_place_interrupted(
        &mut self,
        reason: WorkspaceCloseReason,
        interruption: WorkspaceCleanupInterruption,
    ) -> Result<WorkspaceCleanupReport, WorkspaceError> {
        self.close_in_place_with_interruption(reason, interruption.into())
    }
}

impl WorkspaceError {
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorCode::Conflict, message)
    }
}

fn bounded_identity(value: &str, name: &str) -> Result<(), WorkspaceError> {
    if value.is_empty()
        || value.len() > 200
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'/' || byte == b'\\')
    {
        return Err(WorkspaceError::invalid(format!(
            "{name} identity is invalid"
        )));
    }
    Ok(())
}

fn validate_repository_id(repository_id: &RepositoryId) -> Result<(), WorkspaceError> {
    bounded_identity(&repository_id.0, "repository")?;
    if matches!(repository_id.0.as_str(), "." | "..") {
        return Err(WorkspaceError::invalid("repository identity is invalid"));
    }
    Ok(())
}

fn bounded_revision(value: &str) -> Result<(), WorkspaceError> {
    if value.is_empty()
        || value.len() > 200
        || value.starts_with('-')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(WorkspaceError::invalid("checkout revision is invalid"));
    }
    Ok(())
}

fn controlled_repository(
    root: &Path,
    repository_id: &RepositoryId,
) -> Result<PathBuf, WorkspaceError> {
    let repository = fs::canonicalize(root.join(&repository_id.0)).map_err(|error| {
        WorkspaceError::new(
            WorkspaceErrorCode::NotFound,
            format!("controlled source repository cannot be opened: {error}"),
        )
    })?;
    if !repository.is_dir() || !repository.starts_with(root) {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "repository identity escapes the controlled source root",
        ));
    }
    Ok(repository)
}

fn controlled_recovery_repository(root: &Path, stored: &str) -> Result<PathBuf, WorkspaceError> {
    let repository = fs::canonicalize(stored).map_err(|error| {
        WorkspaceError::new(
            WorkspaceErrorCode::NotFound,
            format!("recovery source repository cannot be opened: {error}"),
        )
    })?;
    if !repository.is_dir() || !repository.starts_with(root) {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "recovery repository escapes the controlled source root",
        ));
    }
    Ok(repository)
}

fn assert_git_repository(repository: &Path) -> Result<(), WorkspaceError> {
    let bare = git_text(repository, &["rev-parse", "--is-bare-repository"])?;
    if bare != "false" {
        return Err(WorkspaceError::invalid(
            "controlled source must be a non-bare Git repository",
        ));
    }
    Ok(())
}

fn workspace_id(
    repository_id: &RepositoryId,
    source_commit_id: &str,
    provenance: &WorkspaceProvenance,
) -> Result<String, WorkspaceError> {
    let bytes = serde_json::to_vec(&(repository_id, source_commit_id, provenance))
        .map_err(|error| WorkspaceError::io("workspace identity cannot be encoded", error))?;
    Ok(format!("ws-{:x}", Sha256::digest(bytes)))
}

fn layout(root: PathBuf) -> WorkspaceLayout {
    WorkspaceLayout {
        checkout: root.join("checkout"),
        home: root.join("home"),
        sandbox: root.join("sandbox"),
        temporary: root.join("tmp"),
        artifacts: root.join("artifacts"),
        root,
    }
}

fn create_private_directories(
    plan: &WorkspacePlan,
    manifest: &RecoveryManifest,
) -> Result<(), WorkspaceError> {
    let layout = &plan.layout;
    for directory in [
        &layout.home,
        &layout.sandbox,
        &layout.temporary,
        &layout.artifacts,
    ] {
        fs::create_dir(directory).map_err(|error| {
            WorkspaceError::io("private workspace directory cannot be created", error)
        })?;
    }
    create_recovery_manifest(&layout.root, manifest)
}

fn recovery_manifest(
    plan: &WorkspacePlan,
    phase: WorkspaceRecoveryPhase,
) -> Result<RecoveryManifest, WorkspaceError> {
    let source_repository = plan
        .source_repository
        .to_str()
        .ok_or_else(|| WorkspaceError::invalid("source repository path must be valid UTF-8"))?;
    Ok(RecoveryManifest {
        schema_version: RECOVERY_MANIFEST_SCHEMA,
        phase,
        workspace_id: plan.workspace_id.clone(),
        source_repository: source_repository.to_owned(),
        repository_id: plan.repository_id.clone(),
        checkout_revision: plan.checkout_revision.clone(),
        source_commit_id: plan.source_commit_id.clone(),
        source_tree_id: plan.source_tree_id.clone(),
        origin_provenance: plan.origin_provenance.clone(),
        current_provenance: plan.current_provenance.clone(),
        replacement: plan.replacement.clone(),
        max_artifact_bytes: plan.max_artifact_bytes,
    })
}

fn create_workspace_from_intent(
    plan: &WorkspacePlan,
    manifest: &mut RecoveryManifest,
    interruption: CreationInterruption,
) -> Result<(), WorkspaceError> {
    fs::create_dir(&plan.layout.root).map_err(|error| {
        WorkspaceError::new(
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                WorkspaceErrorCode::Conflict
            } else {
                WorkspaceErrorCode::Io
            },
            format!("exclusive workspace root cannot be created: {error}"),
        )
    })?;
    interrupt_creation(interruption, CreationInterruptionPoint::RootCreated)?;
    create_private_directories(plan, manifest)?;
    interrupt_creation(interruption, CreationInterruptionPoint::ManifestCreated)?;
    add_worktree(
        &plan.source_repository,
        &plan.layout.checkout,
        &plan.source_commit_id,
    )?;
    interrupt_creation(interruption, CreationInterruptionPoint::WorktreeAdded)?;
    manifest.phase = WorkspaceRecoveryPhase::Active;
    replace_recovery_manifest(&plan.layout.root, manifest)?;
    remove_creation_intent(&plan.layout)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreationInterruptionPoint {
    RootCreated,
    ManifestCreated,
    WorktreeAdded,
}

fn interrupt_creation(
    interruption: CreationInterruption,
    point: CreationInterruptionPoint,
) -> Result<(), WorkspaceError> {
    #[cfg(feature = "test-support")]
    let interrupted = matches!(
        (interruption, point),
        (
            CreationInterruption::AfterRootCreated,
            CreationInterruptionPoint::RootCreated
        ) | (
            CreationInterruption::AfterCreatingManifest,
            CreationInterruptionPoint::ManifestCreated
        ) | (
            CreationInterruption::AfterWorktreeAdded,
            CreationInterruptionPoint::WorktreeAdded
        )
    );
    #[cfg(not(feature = "test-support"))]
    let interrupted = {
        let _ = (interruption, point);
        false
    };
    if interrupted {
        return Err(WorkspaceError::conflict(
            "workspace creation was interrupted by the test seam",
        ));
    }
    Ok(())
}

fn creation_intent_path(layout: &WorkspaceLayout) -> Result<PathBuf, WorkspaceError> {
    let parent = layout
        .root
        .parent()
        .ok_or_else(|| WorkspaceError::invalid("workspace root has no manager directory"))?;
    Ok(parent.join(format!(
        "{CREATION_INTENT_PREFIX}{}{CREATION_INTENT_SUFFIX}",
        layout
            .root
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| WorkspaceError::invalid("workspace identity is invalid"))?
    )))
}

fn owner_lock_path(layout: &WorkspaceLayout) -> Result<PathBuf, WorkspaceError> {
    let parent = layout
        .root
        .parent()
        .ok_or_else(|| WorkspaceError::invalid("workspace root has no manager directory"))?;
    Ok(parent.join(format!(
        "{OWNER_LOCK_PREFIX}{}",
        layout
            .root
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| WorkspaceError::invalid("workspace identity is invalid"))?
    )))
}

fn acquire_workspace_owner_lock(layout: &WorkspaceLayout) -> Result<File, WorkspaceError> {
    let path = owner_lock_path(layout)?;
    if path.exists()
        && fs::symlink_metadata(&path)
            .map_err(|error| WorkspaceError::io("workspace owner lock cannot be inspected", error))?
            .file_type()
            .is_symlink()
    {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "workspace owner lock cannot be a symbolic link",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| WorkspaceError::io("workspace owner lock cannot be opened", error))?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => {
            WorkspaceError::conflict("workspace is still owned by another live Worker instance")
        }
        fs::TryLockError::Error(error) => {
            WorkspaceError::io("workspace owner lock cannot be acquired", error)
        }
    })?;
    Ok(file)
}

fn write_creation_intent(
    layout: &WorkspaceLayout,
    manifest: &RecoveryManifest,
) -> Result<(), WorkspaceError> {
    let path = creation_intent_path(layout)?;
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::invalid("creation intent has no parent"))?;
    if path.exists() {
        return Err(WorkspaceError::conflict(
            "workspace creation intent already exists",
        ));
    }
    let temporary = path.with_extension("next");
    if temporary.exists() {
        fs::remove_file(&temporary).map_err(|error| {
            WorkspaceError::io("stale creation intent cannot be removed", error)
        })?;
    }
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| WorkspaceError::io("creation intent cannot be encoded", error))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| WorkspaceError::io("creation intent cannot be staged", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WorkspaceError::io("creation intent cannot be persisted", error))?;
    fs::rename(&temporary, &path)
        .map_err(|error| WorkspaceError::io("creation intent cannot be installed", error))?;
    sync_directory(parent, "creation intent directory cannot be persisted")
}

fn remove_creation_intent(layout: &WorkspaceLayout) -> Result<(), WorkspaceError> {
    let path = creation_intent_path(layout)?;
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| WorkspaceError::io("creation intent cannot be removed", error))?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkspaceError::invalid("creation intent has no parent"))?;
        sync_directory(parent, "creation intent removal cannot be persisted")?;
    }
    Ok(())
}

fn read_creation_intent_for_workspace(
    workspace_root: &Path,
) -> Result<Option<RecoveryManifest>, WorkspaceError> {
    let path = creation_intent_path(&layout(workspace_root.to_path_buf()))?;
    if !path.exists() {
        return Ok(None);
    }
    read_manifest_file(&path, "creation intent").map(Some)
}

fn cleanup_intent_path(layout: &WorkspaceLayout) -> Result<PathBuf, WorkspaceError> {
    let parent = layout
        .root
        .parent()
        .ok_or_else(|| WorkspaceError::invalid("workspace root has no manager directory"))?;
    Ok(parent.join(format!(
        "{CLEANUP_INTENT_PREFIX}{}{CREATION_INTENT_SUFFIX}",
        layout
            .root
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| WorkspaceError::invalid("workspace identity is invalid"))?
    )))
}

fn write_cleanup_intent(
    layout: &WorkspaceLayout,
    manifest: &RecoveryManifest,
) -> Result<(), WorkspaceError> {
    let path = cleanup_intent_path(layout)?;
    if path.exists() {
        let existing = read_manifest_file(&path, "cleanup intent")?;
        return if existing == *manifest {
            Ok(())
        } else {
            Err(WorkspaceError::conflict(
                "workspace cleanup intent differs from durable authority",
            ))
        };
    }
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::invalid("cleanup intent has no parent"))?;
    let temporary = path.with_extension("next");
    if temporary.exists() {
        fs::remove_file(&temporary)
            .map_err(|error| WorkspaceError::io("stale cleanup intent cannot be removed", error))?;
    }
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| WorkspaceError::io("cleanup intent cannot be encoded", error))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| WorkspaceError::io("cleanup intent cannot be staged", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WorkspaceError::io("cleanup intent cannot be persisted", error))?;
    fs::rename(&temporary, &path)
        .map_err(|error| WorkspaceError::io("cleanup intent cannot be installed", error))?;
    sync_directory(parent, "cleanup intent directory cannot be persisted")
}

fn remove_cleanup_intent(layout: &WorkspaceLayout) -> Result<(), WorkspaceError> {
    let path = cleanup_intent_path(layout)?;
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| WorkspaceError::io("cleanup intent cannot be removed", error))?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkspaceError::invalid("cleanup intent has no parent"))?;
        sync_directory(parent, "cleanup intent removal cannot be persisted")?;
    }
    Ok(())
}

fn create_recovery_manifest(
    root: &Path,
    manifest: &RecoveryManifest,
) -> Result<(), WorkspaceError> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| WorkspaceError::io("recovery manifest cannot be encoded", error))?;
    let manifest_path = root.join(RECOVERY_MANIFEST);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .map_err(|error| WorkspaceError::io("recovery manifest cannot be created", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WorkspaceError::io("recovery manifest cannot be persisted", error))?;
    sync_directory(root, "workspace manifest directory cannot be persisted")
}

fn replace_recovery_manifest(
    root: &Path,
    manifest: &RecoveryManifest,
) -> Result<(), WorkspaceError> {
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| WorkspaceError::io("replacement manifest cannot be encoded", error))?;
    let temporary = root.join(".winwincode-workspace.next");
    if let Ok(metadata) = fs::symlink_metadata(&temporary) {
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::PathEscape,
                "replacement manifest staging path is not a regular file",
            ));
        }
        fs::remove_file(&temporary).map_err(|error| {
            WorkspaceError::io("stale replacement manifest cannot be removed", error)
        })?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| WorkspaceError::io("replacement manifest cannot be staged", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WorkspaceError::io("replacement manifest cannot be persisted", error))?;
    fs::rename(&temporary, root.join(RECOVERY_MANIFEST))
        .map_err(|error| WorkspaceError::io("replacement manifest cannot be installed", error))?;
    sync_directory(root, "replacement manifest directory cannot be persisted")
}

fn mark_workspace_cleaning(layout: &WorkspaceLayout) -> Result<(), WorkspaceError> {
    if !layout.root.exists() {
        return Ok(());
    }
    let mut manifest = read_recovery_manifest(&layout.root)?;
    match manifest.phase {
        WorkspaceRecoveryPhase::Active => {
            manifest.phase = WorkspaceRecoveryPhase::Cleaning;
            replace_recovery_manifest(&layout.root, &manifest)?;
            write_cleanup_intent(layout, &manifest)
        }
        WorkspaceRecoveryPhase::Cleaning => write_cleanup_intent(layout, &manifest),
        WorkspaceRecoveryPhase::Creating => Err(WorkspaceError::conflict(
            "workspace creation cannot be cleaned as an active checkout",
        )),
    }
}

fn sync_directory(root: &Path, context: &str) -> Result<(), WorkspaceError> {
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| WorkspaceError::io(context, error))
}

fn read_recovery_manifest(root: &Path) -> Result<RecoveryManifest, WorkspaceError> {
    read_manifest_file(&root.join(RECOVERY_MANIFEST), "recovery manifest")
}

fn read_manifest_file(path: &Path, context: &str) -> Result<RecoveryManifest, WorkspaceError> {
    let bytes = fs::read(path)
        .map_err(|error| WorkspaceError::io(&format!("{context} cannot be read"), error))?;
    let manifest: RecoveryManifest = serde_json::from_slice(&bytes).map_err(|error| {
        WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            format!("{context} is invalid: {error}"),
        )
    })?;
    let canonical = serde_json::to_vec(&manifest)
        .map_err(|error| WorkspaceError::io(&format!("{context} cannot be encoded"), error))?;
    if manifest.schema_version != RECOVERY_MANIFEST_SCHEMA || canonical != bytes {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            format!("{context} is non-canonical or unsupported"),
        ));
    }
    Ok(manifest)
}

fn reconcile_cleaning_workspaces(root: &Path, source_root: &Path) -> Result<(), WorkspaceError> {
    let mut entries = fs::read_dir(root)
        .map_err(|error| WorkspaceError::io("workspace root cannot be scanned", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| WorkspaceError::io("workspace entry cannot be read", error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(CLEANUP_INTENT_PREFIX) || !name.ends_with(CREATION_INTENT_SUFFIX) {
            continue;
        }
        let manifest = read_manifest_file(&entry.path(), "cleanup intent")?;
        let (repository, layout) =
            validate_cleanup_intent(root, source_root, &entry.path(), &manifest)?;
        let _owner_lock = acquire_workspace_owner_lock(&layout)?;
        remove_workspace(&repository, &layout)?;
    }
    let mut entries = fs::read_dir(root)
        .map_err(|error| WorkspaceError::io("workspace root cannot be rescanned", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| WorkspaceError::io("workspace entry cannot be read", error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        if !entry
            .file_type()
            .map_err(|error| WorkspaceError::io("workspace entry type cannot be read", error))?
            .is_dir()
        {
            continue;
        }
        let manifest_path = entry.path().join(RECOVERY_MANIFEST);
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = read_manifest_file(&manifest_path, "cleanup manifest")?;
        if manifest.phase != WorkspaceRecoveryPhase::Cleaning {
            continue;
        }
        if manifest.workspace_id != entry.file_name().to_string_lossy() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup manifest identity does not match its directory",
            ));
        }
        let layout = layout(entry.path());
        write_cleanup_intent(&layout, &manifest)?;
        let intent = cleanup_intent_path(&layout)?;
        let (repository, layout) = validate_cleanup_intent(root, source_root, &intent, &manifest)?;
        let _owner_lock = acquire_workspace_owner_lock(&layout)?;
        remove_workspace(&repository, &layout)?;
    }
    Ok(())
}

fn validate_cleanup_intent(
    root: &Path,
    source_root: &Path,
    intent_path: &Path,
    manifest: &RecoveryManifest,
) -> Result<(PathBuf, WorkspaceLayout), WorkspaceError> {
    if manifest.phase != WorkspaceRecoveryPhase::Cleaning {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent is not in the Cleaning phase",
        ));
    }
    validate_repository_id(&manifest.repository_id)?;
    bounded_revision(&manifest.checkout_revision)?;
    validate_cleanup_provenance(manifest)?;
    let expected_workspace_id = workspace_id(
        &manifest.repository_id,
        &manifest.source_commit_id,
        &manifest.origin_provenance,
    )?;
    if manifest.workspace_id != expected_workspace_id {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent workspace identity changed",
        ));
    }
    let layout = layout(root.join(&manifest.workspace_id));
    if cleanup_intent_path(&layout)? != intent_path {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent identity does not match its path",
        ));
    }
    let repository = controlled_recovery_repository(source_root, &manifest.source_repository)?;
    let expected_repository = controlled_repository(source_root, &manifest.repository_id)?;
    if repository != expected_repository {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent repository identity changed",
        ));
    }
    assert_git_repository(&repository)?;
    let source_tree = rev_parse(
        &repository,
        &format!("{}^{{tree}}", manifest.source_commit_id),
    )?;
    if source_tree != manifest.source_tree_id {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent source tree identity changed",
        ));
    }
    let recovery_manifest = layout.root.join(RECOVERY_MANIFEST);
    if recovery_manifest.exists() {
        let recovered = read_manifest_file(&recovery_manifest, "cleanup manifest")?;
        if recovered.phase != WorkspaceRecoveryPhase::Cleaning || recovered != *manifest {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup intent differs from its durable Cleaning manifest",
            ));
        }
    }
    Ok((repository, layout))
}

fn validate_cleanup_provenance(manifest: &RecoveryManifest) -> Result<(), WorkspaceError> {
    for provenance in [&manifest.origin_provenance, &manifest.current_provenance] {
        if !valid_sha256_digest(&provenance.execution_job_digest)
            || !valid_sha256_digest(&provenance.logical_execution_job_digest)
        {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup intent Job digest is invalid",
            ));
        }
    }
    let Some(replacement) = manifest.replacement.as_ref() else {
        if manifest.origin_provenance != manifest.current_provenance {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup intent current authority has no replacement receipt",
            ));
        }
        return Ok(());
    };
    if !valid_sha256_digest(&replacement.receipt_digest)
        || !valid_sha256_digest(&replacement.logical_job_digest)
        || manifest.current_provenance.logical_execution_job_digest
            != replacement.logical_job_digest
        || replacement.predecessor_lease.job_id != replacement.successor_lease.job_id
        || replacement.predecessor_lease.attempt.saturating_add(1)
            != replacement.successor_lease.attempt
        || !provenance_matches_lease(&manifest.current_provenance, &replacement.successor_lease)
    {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent replacement authority changed",
        ));
    }
    if manifest.origin_provenance == manifest.current_provenance {
        if replacement.predecessor_session_identity.is_some() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup intent bound replacement lost its predecessor origin",
            ));
        }
        return Ok(());
    }
    if manifest.origin_provenance.execution_job_id != manifest.current_provenance.execution_job_id
        || manifest.origin_provenance.logical_execution_job_digest
            != manifest.current_provenance.logical_execution_job_digest
        || manifest.origin_provenance.attempt >= manifest.current_provenance.attempt
        || replacement.predecessor_session_identity.is_none()
    {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "cleanup intent predecessor authority changed",
        ));
    }
    if manifest.origin_provenance.attempt
        == u64::try_from(replacement.predecessor_lease.attempt).unwrap_or_default()
    {
        validate_replacement_predecessor(
            &manifest.origin_provenance,
            &manifest.current_provenance,
            replacement,
        )
        .map_err(|_| {
            WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "cleanup intent predecessor authority changed",
            )
        })?;
    }
    Ok(())
}

fn private_layout_exists(layout: &WorkspaceLayout) -> Result<bool, WorkspaceError> {
    for directory in [
        &layout.root,
        &layout.checkout,
        &layout.home,
        &layout.sandbox,
        &layout.temporary,
        &layout.artifacts,
    ] {
        if !directory.is_dir() {
            return Ok(false);
        }
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| WorkspaceError::io("workspace recovery metadata failed", error))?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::PathEscape,
                "workspace recovery symbolic links are not permitted",
            ));
        }
    }
    Ok(true)
}

fn validate_recovered_plan(plan: &WorkspacePlan) -> Result<(), WorkspaceError> {
    if !private_layout_exists(&plan.layout)? {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "workspace recovery private layout changed",
        ));
    }
    let checkout_root = fs::canonicalize(&plan.layout.checkout)
        .map_err(|error| WorkspaceError::io("workspace checkout cannot be recovered", error))?;
    let reported_root = PathBuf::from(git_text(
        &plan.layout.checkout,
        &["rev-parse", "--show-toplevel"],
    )?);
    let reported_root = fs::canonicalize(reported_root)
        .map_err(|error| WorkspaceError::io("Git worktree root cannot be recovered", error))?;
    let reported_common = PathBuf::from(git_text(
        &plan.layout.checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let reported_common = fs::canonicalize(reported_common)
        .map_err(|error| WorkspaceError::io("Git common directory cannot be recovered", error))?;
    let expected_common = fs::canonicalize(plan.source_repository.join(".git"))
        .map_err(|error| WorkspaceError::io("source Git directory cannot be recovered", error))?;
    let source_tree = rev_parse(
        &plan.source_repository,
        &format!("{}^{{tree}}", plan.source_commit_id),
    )?;
    let checkout_head = rev_parse(&plan.layout.checkout, "HEAD^{commit}")?;
    if checkout_root != reported_root
        || reported_common != expected_common
        || source_tree != plan.source_tree_id
        || checkout_head != plan.source_commit_id
    {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "workspace recovery Git source identity changed",
        ));
    }
    Ok(())
}

fn workspace_checkout_clean(checkout: &Path) -> Result<bool, WorkspaceError> {
    Ok(git_output(
        checkout,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?
    .is_empty())
}

fn add_worktree(repository: &Path, checkout: &Path, commit: &str) -> Result<(), WorkspaceError> {
    let mut command = git_command(repository);
    command.args(["worktree", "add", "--detach", "--"]);
    command.arg(checkout).arg(commit);
    command_status(command, "Git worktree cannot be created")
}

fn remove_workspace(repository: &Path, layout: &WorkspaceLayout) -> Result<(), WorkspaceError> {
    remove_workspace_with_interruption(repository, layout, CleanupInterruption::None)
}

fn remove_workspace_with_interruption(
    repository: &Path,
    layout: &WorkspaceLayout,
    interruption: CleanupInterruption,
) -> Result<(), WorkspaceError> {
    if layout.checkout.exists() {
        fail_cleanup(interruption, CleanupFailurePoint::WorktreeRemoval)?;
        let mut command = git_command(repository);
        command.args(["worktree", "remove", "--force", "--"]);
        command.arg(&layout.checkout);
        command_status(command, "Git worktree cannot be removed")?;
    }
    interrupt_cleanup(interruption, CleanupInterruptionPoint::WorktreeRemoved)?;
    fail_cleanup(interruption, CleanupFailurePoint::Prune)?;
    let mut prune = git_command(repository);
    prune.args(["worktree", "prune", "--expire", "now"]);
    command_status(prune, "Git worktree metadata cannot be pruned")?;
    if layout.root.exists() {
        #[cfg(feature = "test-support")]
        fail_root_removal_after_manifest(interruption, layout)?;
        fs::remove_dir_all(&layout.root)
            .map_err(|error| WorkspaceError::io("workspace root cannot be removed", error))?;
    }
    if let Some(parent) = layout.root.parent() {
        fail_cleanup(interruption, CleanupFailurePoint::ParentSync)?;
        sync_directory(parent, "workspace removal cannot be persisted")?;
    }
    remove_cleanup_intent(layout)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupInterruptionPoint {
    ManifestMarked,
    WorktreeRemoved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupFailurePoint {
    WorktreeRemoval,
    Prune,
    ParentSync,
}

fn fail_cleanup(
    interruption: CleanupInterruption,
    point: CleanupFailurePoint,
) -> Result<(), WorkspaceError> {
    #[cfg(feature = "test-support")]
    let failed = matches!(
        (interruption, point),
        (
            CleanupInterruption::FailWorktreeRemoval,
            CleanupFailurePoint::WorktreeRemoval
        ) | (CleanupInterruption::FailPrune, CleanupFailurePoint::Prune)
            | (
                CleanupInterruption::FailParentSync,
                CleanupFailurePoint::ParentSync
            )
    );
    #[cfg(not(feature = "test-support"))]
    let failed = {
        let _ = (interruption, point);
        false
    };
    if failed {
        return Err(WorkspaceError::io(
            "workspace cleanup fault",
            "injected failure",
        ));
    }
    Ok(())
}

#[cfg(feature = "test-support")]
fn fail_root_removal_after_manifest(
    interruption: CleanupInterruption,
    layout: &WorkspaceLayout,
) -> Result<(), WorkspaceError> {
    if interruption == CleanupInterruption::FailRootRemovalAfterManifest {
        let manifest = layout.root.join(RECOVERY_MANIFEST);
        if manifest.exists() {
            fs::remove_file(manifest).map_err(|error| {
                WorkspaceError::io("partial workspace removal cannot be injected", error)
            })?;
        }
        return Err(WorkspaceError::io(
            "workspace root cannot be removed",
            "injected partial removal failure",
        ));
    }
    Ok(())
}

fn interrupt_cleanup(
    interruption: CleanupInterruption,
    point: CleanupInterruptionPoint,
) -> Result<(), WorkspaceError> {
    #[cfg(feature = "test-support")]
    let interrupted = matches!(
        (interruption, point),
        (
            CleanupInterruption::AfterCleaningManifest,
            CleanupInterruptionPoint::ManifestMarked
        ) | (
            CleanupInterruption::AfterWorktreeRemoved,
            CleanupInterruptionPoint::WorktreeRemoved
        )
    );
    #[cfg(not(feature = "test-support"))]
    let interrupted = {
        let _ = (interruption, point);
        false
    };
    if interrupted {
        return Err(WorkspaceError::conflict(
            "workspace cleanup was interrupted by the test seam",
        ));
    }
    Ok(())
}

fn controlled_path(
    root: &Path,
    relative: &Path,
    allow_missing: bool,
) -> Result<PathBuf, WorkspaceError> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    let existing = if allow_missing {
        nearest_existing_ancestor(&path)?
    } else {
        path.clone()
    };
    ensure_no_links(root, &existing)?;
    if !allow_missing && !path.exists() {
        return Ok(path);
    }
    let canonical = fs::canonicalize(&existing)
        .map_err(|error| WorkspaceError::io("workspace path cannot be resolved", error))?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| WorkspaceError::io("workspace root cannot be resolved", error))?;
    if !canonical.starts_with(canonical_root) {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "workspace path escapes its private root",
        ));
    }
    Ok(path)
}

fn validate_relative_path(path: &Path) -> Result<(), WorkspaceError> {
    let text = path.to_str().ok_or_else(|| {
        WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "workspace path must be valid UTF-8",
        )
    })?;
    if text.is_empty() || text.len() > MAX_PATH_BYTES {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "workspace path length is invalid",
        ));
    }
    if path.components().any(|component| {
        !matches!(component, Component::Normal(_))
            || component.as_os_str().to_string_lossy().contains('\\')
    }) {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "workspace path must be a portable relative path",
        ));
    }
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let mut current = path.to_path_buf();
    while !current.exists() {
        current = current
            .parent()
            .ok_or_else(|| WorkspaceError::new(WorkspaceErrorCode::PathEscape, "path has no root"))?
            .to_path_buf();
    }
    Ok(current)
}

fn ensure_no_links(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "path escapes its private root",
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if current.exists()
            && fs::symlink_metadata(&current)
                .map_err(|error| {
                    WorkspaceError::io("workspace path metadata cannot be read", error)
                })?
                .file_type()
                .is_symlink()
        {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::PathEscape,
                "workspace symbolic links are not permitted",
            ));
        }
    }
    Ok(())
}

fn artifact_files(root: &Path) -> Result<Vec<WorkspaceArtifactFile>, WorkspaceError> {
    let mut paths = Vec::new();
    collect_regular_files(root, root, &mut paths)?;
    paths.sort();
    paths.iter().map(|path| artifact_file(root, path)).collect()
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), WorkspaceError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| WorkspaceError::io("Artifact directory cannot be scanned", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| WorkspaceError::io("Artifact entry cannot be read", error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| WorkspaceError::io("Artifact entry type cannot be read", error))?;
        if file_type.is_symlink() {
            return Err(WorkspaceError::new(
                WorkspaceErrorCode::PathEscape,
                "Artifact symbolic links are not permitted",
            ));
        }
        if file_type.is_dir() {
            collect_regular_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).map_err(|_| {
                WorkspaceError::new(
                    WorkspaceErrorCode::PathEscape,
                    "Artifact path escaped its root",
                )
            })?;
            validate_relative_path(relative)?;
            files.push(entry_path);
        } else {
            return Err(WorkspaceError::invalid(
                "Artifact staging contains a non-regular filesystem entry",
            ));
        }
    }
    Ok(())
}

fn artifact_file(root: &Path, path: &Path) -> Result<WorkspaceArtifactFile, WorkspaceError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        WorkspaceError::new(
            WorkspaceErrorCode::PathEscape,
            "Artifact path escaped its root",
        )
    })?;
    let relative_path = portable_path(relative)?;
    let mut file = File::open(path)
        .map_err(|error| WorkspaceError::io("Artifact file cannot be opened", error))?;
    let size_bytes = file
        .metadata()
        .map_err(|error| WorkspaceError::io("Artifact metadata cannot be read", error))?
        .len();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| WorkspaceError::io("Artifact file cannot be read", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(WorkspaceArtifactFile {
        relative_path,
        size_bytes,
        digest: Sha256Digest(format!("sha256:{:x}", hasher.finalize())),
    })
}

fn artifact_set_digest(files: &[WorkspaceArtifactFile]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    for file in files {
        update_field(&mut hasher, file.relative_path.as_bytes());
        hasher.update(file.size_bytes.to_be_bytes());
        update_field(&mut hasher, file.digest.0.as_bytes());
    }
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

fn tree_digest(repository: &Path, commit: &str) -> Result<Sha256Digest, WorkspaceError> {
    let listing = git_output(repository, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut hasher = Sha256::new();
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let separator = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| {
                WorkspaceError::new(WorkspaceErrorCode::Corrupt, "Git tree record is malformed")
            })?;
        let (header, path_with_separator) = record.split_at(separator);
        let path = &path_with_separator[1..];
        let header = std::str::from_utf8(header).map_err(|_| {
            WorkspaceError::new(WorkspaceErrorCode::Corrupt, "Git tree header is not UTF-8")
        })?;
        let mut fields = header.split(' ');
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let object_id = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755")
            || kind != "blob"
            || object_id.is_empty()
            || fields.next().is_some()
        {
            return Err(WorkspaceError::invalid(
                "candidate tree contains an unsupported link or object type",
            ));
        }
        let path = std::str::from_utf8(path)
            .map_err(|_| WorkspaceError::invalid("candidate tree path must be valid UTF-8"))?;
        validate_relative_path(Path::new(path))?;
        let blob = git_output(repository, &["cat-file", "blob", object_id])?;
        update_field(&mut hasher, path.as_bytes());
        update_field(&mut hasher, mode.as_bytes());
        update_field(&mut hasher, &blob);
    }
    Ok(Sha256Digest(format!("sha256:{:x}", hasher.finalize())))
}

fn update_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(bytes);
}

fn portable_path(path: &Path) -> Result<String, WorkspaceError> {
    validate_relative_path(path)?;
    Ok(path
        .components()
        .map(Component::as_os_str)
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join("/"))
}

fn rev_parse(repository: &Path, revision: &str) -> Result<String, WorkspaceError> {
    git_text(
        repository,
        &["rev-parse", "--verify", "--end-of-options", revision],
    )
}

fn git_command(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_status(repository: &Path, arguments: &[&str]) -> Result<(), WorkspaceError> {
    let mut command = git_command(repository);
    command.args(arguments);
    command_status(command, "Git operation failed")
}

fn git_text(repository: &Path, arguments: &[&str]) -> Result<String, WorkspaceError> {
    let output = git_output(repository, arguments)?;
    let text = std::str::from_utf8(&output)
        .map_err(|_| WorkspaceError::new(WorkspaceErrorCode::Corrupt, "Git output is not UTF-8"))?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return Err(WorkspaceError::new(
            WorkspaceErrorCode::Corrupt,
            "Git returned an invalid single-line identity",
        ));
    }
    Ok(text.to_owned())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<Vec<u8>, WorkspaceError> {
    let mut command = git_command(repository);
    command.args(arguments);
    checked_output(command, "Git operation failed").map(|output| output.stdout)
}

fn commit_tree(
    repository: &Path,
    tree: &str,
    parent: &str,
    message: &[u8],
) -> Result<String, WorkspaceError> {
    let mut command = git_command(repository);
    command.args(["commit-tree", tree, "-p", parent]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    for (name, value) in [
        ("GIT_AUTHOR_NAME", "WinWinCode Worker"),
        ("GIT_AUTHOR_EMAIL", "worker@winwincode.invalid"),
        ("GIT_COMMITTER_NAME", "WinWinCode Worker"),
        ("GIT_COMMITTER_EMAIL", "worker@winwincode.invalid"),
        ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
    ] {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| WorkspaceError::io("Git candidate commit cannot start", error))?;
    child
        .stdin
        .take()
        .ok_or_else(|| WorkspaceError::new(WorkspaceErrorCode::Git, "Git stdin is unavailable"))?
        .write_all(message)
        .map_err(|error| WorkspaceError::io("Git candidate binding cannot be written", error))?;
    let output = child
        .wait_with_output()
        .map_err(|error| WorkspaceError::io("Git candidate commit cannot finish", error))?;
    checked_existing_output(output, "Git candidate commit failed").and_then(|output| {
        let text = String::from_utf8(output.stdout).map_err(|_| {
            WorkspaceError::new(
                WorkspaceErrorCode::Corrupt,
                "Git commit identity is not UTF-8",
            )
        })?;
        Ok(text.trim().to_owned())
    })
}

fn command_status(command: Command, context: &str) -> Result<(), WorkspaceError> {
    checked_output(command, context).map(drop)
}

fn checked_output(mut command: Command, context: &str) -> Result<Output, WorkspaceError> {
    let output = command
        .output()
        .map_err(|error| WorkspaceError::io(context, error))?;
    checked_existing_output(output, context)
}

fn checked_existing_output(output: Output, context: &str) -> Result<Output, WorkspaceError> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(WorkspaceError::new(
        WorkspaceErrorCode::Git,
        format!("{context}: {}", stderr.trim()),
    ))
}
