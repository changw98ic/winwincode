// SPDX-License-Identifier: Apache-2.0

//! `StrongFlow` candidate Artifact preparation at the Worker workspace boundary.
//!
//! This module freezes and re-verifies the actual detached Git checkout before
//! it exposes canonical candidate bytes. Durable Artifact/message identity and
//! acknowledgement remain owned by the Worker outbox.

use std::fmt;

use sha2::{Digest as _, Sha256};
pub use winwincode_codex::candidate_artifact_outbox::{
    CANDIDATE_FILE_NAME, CANDIDATE_MEDIA_TYPE, CandidateArtifactUpload,
};
use winwincode_domain::{ArtifactId, Sha256Digest};
use winwincode_execution_port::generated::{
    ArtifactDescriptor, ArtifactKind, ExecutionScope, ExecutionWorkspaceWriteMode,
};

use crate::workspace::{CandidateSnapshot, WorkerWorkspace, WorkspaceError, WorkspaceProvenance};
use crate::{ActiveJob, ActiveJobLifecycle};

/// Stable candidate preparation failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateProductErrorCode {
    InvalidLifecycle,
    InvalidRole,
    InvalidScope,
    AuthorityMismatch,
    Workspace,
}

/// Bounded failure which does not retain repository content.
#[derive(Debug, Eq, PartialEq)]
pub struct CandidateProductError {
    code: CandidateProductErrorCode,
    message: &'static str,
}

impl CandidateProductError {
    const fn new(code: CandidateProductErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn code(&self) -> CandidateProductErrorCode {
        self.code
    }
}

impl fmt::Display for CandidateProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CandidateProductError {}

impl From<WorkspaceError> for CandidateProductError {
    fn from(_: WorkspaceError) -> Self {
        Self::new(
            CandidateProductErrorCode::Workspace,
            "candidate workspace snapshot or verification failed",
        )
    }
}

/// Verified candidate bytes ready for durable Artifact identity allocation.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedCandidateArtifact {
    snapshot: CandidateSnapshot,
    bytes: Vec<u8>,
    digest: Sha256Digest,
    size_bytes: i64,
    job_digest: Sha256Digest,
    logical_job_digest: Sha256Digest,
    execution_profile: String,
    scope: ExecutionScope,
    lease: winwincode_execution_port::generated::ExecutionLeaseStamp,
    worker_session_id: winwincode_domain::WorkerSessionId,
    session_identity: winwincode_domain::SessionIdentity,
}

impl PreparedCandidateArtifact {
    /// Exact detached Git candidate facts used to build the manifest.
    #[must_use]
    pub const fn snapshot(&self) -> &CandidateSnapshot {
        &self.snapshot
    }

    /// Canonical candidate manifest bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Digest of the canonical candidate manifest bytes.
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// Builds the exact descriptor after the outbox has allocated a durable
    /// Artifact identity.
    #[must_use]
    pub fn descriptor(&self, artifact_id: ArtifactId) -> ArtifactDescriptor {
        ArtifactDescriptor {
            artifact_id,
            digest: self.digest.clone(),
            file_name: Some(CANDIDATE_FILE_NAME.to_owned()),
            kind: ArtifactKind::Candidate,
            media_type: CANDIDATE_MEDIA_TYPE.to_owned(),
            size_bytes: self.size_bytes,
        }
    }

    /// Consumes the verified snapshot into the exact durable upload intent.
    #[must_use]
    pub fn into_upload(self, created_at: winwincode_domain::Instant) -> CandidateArtifactUpload {
        CandidateArtifactUpload {
            job_digest: self.job_digest,
            logical_job_digest: self.logical_job_digest,
            execution_profile: self.execution_profile,
            scope: self.scope,
            lease: self.lease,
            worker_session_id: self.worker_session_id,
            session_identity: self.session_identity,
            bytes: self.bytes,
            digest: self.digest,
            created_at,
            replacement_authority: None,
        }
    }
}

/// Freezes the executor/remediator checkout into one verified Git candidate.
///
/// The active Job and workspace authority are checked before Git is mutated.
/// A cancelling Job is rejected, which prevents a post-cancel candidate from
/// entering the Artifact outbox.
///
/// # Errors
///
/// Rejects another lifecycle, role or scope, a workspace from another exact
/// attempt/session, unchanged or invalid Git state, and a manifest whose size
/// cannot be represented by the `ExecutionPort` descriptor.
pub fn prepare_candidate_artifact(
    active: &ActiveJob,
    workspace: &mut WorkerWorkspace,
) -> Result<PreparedCandidateArtifact, CandidateProductError> {
    if active.lifecycle != ActiveJobLifecycle::Running {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidLifecycle,
            "candidate product requires a running Job",
        ));
    }
    if !matches!(
        active.job.execution_profile.as_str(),
        "executor" | "remediator"
    ) {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidRole,
            "candidate product requires an executor or remediator role",
        ));
    }
    if active.job.workspace.write_mode != ExecutionWorkspaceWriteMode::Candidate {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidScope,
            "candidate Job requires a candidate-write workspace",
        ));
    }
    winwincode_codex::stage_product::role_session_policy(
        &active.job,
        winwincode_codex::RoleExecutionMode::React,
    )
    .map_err(|_| {
        CandidateProductError::new(
            CandidateProductErrorCode::InvalidScope,
            "candidate Job is missing its exact sealed stage input",
        )
    })?;
    let provenance = candidate_workspace_provenance(active, workspace)?;

    let snapshot = workspace
        .snapshot_candidate()
        .map_err(CandidateProductError::from)?;
    workspace
        .verify_candidate(&snapshot)
        .map_err(CandidateProductError::from)?;
    if snapshot.repository_id != active.job.workspace.repository_id
        || snapshot.checkout_revision != active.job.workspace.checkout_revision
        || snapshot.provenance != provenance
    {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::AuthorityMismatch,
            "candidate snapshot does not match its Job workspace",
        ));
    }
    prepared_artifact(active, snapshot)
}

/// Captures the already-frozen candidate from a read-only verification
/// checkout. The verifier receives its own Artifact authority and ACK, while
/// the bytes remain the exact candidate manifest observed by that stage.
///
/// # Errors
///
/// Rejects a non-running or non-verification Job, a writable workspace, a
/// missing sealed stage input, a dirty/foreign checkout, or invalid artifact
/// identity facts.
pub fn prepare_verification_artifact(
    active: &ActiveJob,
    workspace: &WorkerWorkspace,
) -> Result<PreparedCandidateArtifact, CandidateProductError> {
    if active.lifecycle != ActiveJobLifecycle::Running {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidLifecycle,
            "verification product requires a running Job",
        ));
    }
    if !matches!(
        active.job.execution_profile.as_str(),
        "reviewer" | "verifier" | "adversarial-verifier"
    ) {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidRole,
            "verification product requires an independent verification role",
        ));
    }
    if active.job.workspace.write_mode != ExecutionWorkspaceWriteMode::ReadOnly {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidScope,
            "verification Job requires a read-only workspace",
        ));
    }
    winwincode_codex::stage_product::role_session_policy(
        &active.job,
        winwincode_codex::RoleExecutionMode::React,
    )
    .map_err(|_| {
        CandidateProductError::new(
            CandidateProductErrorCode::InvalidScope,
            "verification Job is missing its exact sealed stage input",
        )
    })?;
    let provenance = candidate_workspace_provenance(active, workspace)?;
    let snapshot = workspace
        .snapshot_verification()
        .map_err(CandidateProductError::from)?;
    workspace
        .verify_verification(&snapshot)
        .map_err(CandidateProductError::from)?;
    if snapshot.repository_id != active.job.workspace.repository_id
        || snapshot.checkout_revision != active.job.workspace.checkout_revision
        || snapshot.provenance != provenance
    {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::AuthorityMismatch,
            "verification snapshot does not match its Job workspace",
        ));
    }
    prepared_artifact(active, snapshot)
}

fn prepared_artifact(
    active: &ActiveJob,
    snapshot: CandidateSnapshot,
) -> Result<PreparedCandidateArtifact, CandidateProductError> {
    let bytes = snapshot.manifest_bytes().to_vec();
    let size_bytes = i64::try_from(bytes.len()).map_err(|_| {
        CandidateProductError::new(
            CandidateProductErrorCode::Workspace,
            "candidate manifest size is unsupported",
        )
    })?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let job_digest = winwincode_codex::stage_product::stage_product_job_digest(&active.job)
        .map_err(|_| {
            CandidateProductError::new(
                CandidateProductErrorCode::AuthorityMismatch,
                "candidate Job cannot be sealed for durable upload",
            )
        })?;
    let logical_job_digest = winwincode_codex::stage_product::stage_product_logical_job_digest(
        &active.job,
    )
    .map_err(|_| {
        CandidateProductError::new(
            CandidateProductErrorCode::AuthorityMismatch,
            "candidate logical Job cannot be sealed for replacement replay",
        )
    })?;
    Ok(PreparedCandidateArtifact {
        snapshot,
        bytes,
        digest,
        size_bytes,
        job_digest,
        logical_job_digest,
        execution_profile: active.job.execution_profile.clone(),
        scope: active.job.scope.clone(),
        lease: active.lease.clone(),
        worker_session_id: active.worker_session_id.clone(),
        session_identity: active.session_identity.clone(),
    })
}

fn candidate_workspace_provenance(
    active: &ActiveJob,
    workspace: &WorkerWorkspace,
) -> Result<WorkspaceProvenance, CandidateProductError> {
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &active.job.scope else {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::InvalidScope,
            "candidate product requires a Delivery-stage execution scope",
        ));
    };
    let expected = WorkspaceProvenance::from_active_job(active).map_err(|_| {
        CandidateProductError::new(
            CandidateProductErrorCode::AuthorityMismatch,
            "candidate Job authority is internally inconsistent",
        )
    })?;
    if scope.product_session_id != active.session_identity.product_session_id
        || active.session_identity.stage_run_id.as_ref() != Some(&scope.stage_run_id)
        || workspace.provenance() != &expected
    {
        return Err(CandidateProductError::new(
            CandidateProductErrorCode::AuthorityMismatch,
            "candidate workspace does not match the exact active Job authority",
        ));
    }
    Ok(expected)
}
