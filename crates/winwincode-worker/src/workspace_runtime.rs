// SPDX-License-Identifier: Apache-2.0

//! Exact active-Job ownership for detached Worker workspaces.
//!
//! This deep module is the only mutable map from an authenticated active Job
//! to its private checkout. It resumes the original worktree after a process
//! crash, freezes writer changes through the canonical candidate builder, and
//! consumes the checkout at every terminal cleanup boundary.

use std::{collections::HashMap, fmt, path::PathBuf};

use winwincode_domain::ExecutionJobId;
use winwincode_execution_port::generated::ExecutionJobReplacementAuthority;

use crate::{
    ActiveJob,
    stage_product::{
        CandidateProductError, PreparedCandidateArtifact, prepare_candidate_artifact,
        prepare_verification_artifact,
    },
    workspace::{
        WorkerWorkspace, WorkspaceCleanupReport, WorkspaceCloseReason, WorkspaceError,
        WorkspaceManager, WorkspaceProvenance,
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

/// Process-owned manager for all live detached Job workspaces.
#[derive(Debug)]
pub struct JobWorkspaceRuntime {
    manager: WorkspaceManager,
    active: HashMap<String, WorkerWorkspace>,
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
        Ok(Self {
            manager: WorkspaceManager::open(root, source_root)?,
            active: HashMap::new(),
        })
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
            return Ok(workspace.layout().checkout().to_path_buf());
        }
        let workspace = self.manager.create_or_recover(active, replacement)?;
        let checkout = workspace.layout().checkout().to_path_buf();
        self.active.insert(active.job.job_id.0.clone(), workspace);
        Ok(checkout)
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
        let report = self
            .active
            .get_mut(&job_id.0)
            .ok_or_else(authority_error)?
            .close_in_place(reason)?;
        self.active.remove(&job_id.0);
        Ok(report)
    }

    pub(crate) fn close_job_if_open(
        &mut self,
        job_id: &ExecutionJobId,
        reason: WorkspaceCloseReason,
    ) -> Result<Option<WorkspaceCleanupReport>, JobWorkspaceError> {
        if !self.active.contains_key(&job_id.0) {
            return Ok(None);
        }
        self.close_job(job_id, reason).map(Some)
    }

    /// Returns whether this process owns an open checkout for the Job.
    #[must_use]
    pub fn contains(&self, job_id: &ExecutionJobId) -> bool {
        self.active.contains_key(&job_id.0)
    }
}

fn same_authority(provenance: &WorkspaceProvenance, active: &ActiveJob) -> bool {
    let Ok(expected) = WorkspaceProvenance::from_active_job(active) else {
        return false;
    };
    provenance == &expected
}

fn authority_error() -> JobWorkspaceError {
    JobWorkspaceError::new(
        JobWorkspaceErrorCode::AuthorityMismatch,
        "active Job does not own this detached workspace",
    )
}
