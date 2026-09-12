// SPDX-License-Identifier: Apache-2.0

//! Community storage adapters for Delivery-owned durable facts.

use winwincode_delivery::{
    application::{
        session_binding::{
            DurableExecutionAttemptReplacementInput, DurableExecutionLeaseInput,
            DurableWorkerSlotInput,
        },
        workrun_execution::DurableDispatchAuthorityInput,
    },
    domain::{
        CandidateHunkFact, CandidatePathFact, CandidatePathState, DurableCandidateArtifactInput,
        DurableCandidateSourceInput,
    },
};

use crate::{
    ExecutionDispatchAuthority, ExecutionLeaseRecord, ExecutionScopeReplacementAuthority,
    GitSourcePathState, ValidatedGitSourceArtifact,
};

#[must_use]
pub fn delivery_dispatch_authority(
    authority: &ExecutionDispatchAuthority,
) -> DurableDispatchAuthorityInput {
    let lease = authority.lease();
    DurableDispatchAuthorityInput {
        execution_job_id: lease.job_id.clone(),
        attempt: lease.attempt,
        lease_id: lease.lease_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: authority.worker_session_id().clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    }
}

#[must_use]
pub fn delivery_candidate_source(
    source: &ValidatedGitSourceArtifact,
) -> DurableCandidateSourceInput {
    let artifact = source.artifact();
    let provenance = artifact.provenance();
    DurableCandidateSourceInput {
        repository_locator: source.repository_locator().to_owned(),
        requested_base_revision: source.requested_base_revision().to_owned(),
        base_commit_id: source.base_commit_id().to_owned(),
        base_tree_id: source.base_tree_id().to_owned(),
        candidate_commit_id: source.candidate_commit_id().to_owned(),
        candidate_tree_id: source.candidate_tree_id().to_owned(),
        diff_sha256: source.diff_sha256().to_owned(),
        changed_paths: source
            .changed_paths()
            .iter()
            .map(|path| CandidatePathFact {
                path: path.path().to_owned(),
                state: match path.state() {
                    GitSourcePathState::Present => CandidatePathState::Present,
                    GitSourcePathState::Deleted => CandidatePathState::Deleted,
                },
                object_id: path.object_id().map(str::to_owned),
            })
            .collect(),
        changed_hunks: source
            .changed_hunks()
            .iter()
            .map(|hunk| CandidateHunkFact {
                file_path: hunk.file_path().to_owned(),
                hunk_sha256: hunk.hunk_sha256().to_owned(),
                source_hunk_sha256: None,
            })
            .collect(),
        artifact: DurableCandidateArtifactInput {
            artifact_id: artifact.artifact_id().clone(),
            digest: artifact.digest().clone(),
            complete: artifact.is_complete(),
            deleted_at_millis: artifact.deleted_at_millis(),
            execution_job_id: provenance.execution_job_id().clone(),
            attempt: provenance.attempt(),
            lease_id: provenance.lease_id().clone(),
            fencing_token: provenance.fencing_token().clone(),
            worker_id: provenance.worker_id().clone(),
            worker_instance_id: provenance.worker_instance_id().clone(),
            worker_session_id: provenance.worker_session_id().clone(),
        },
    }
}

#[must_use]
pub fn delivery_execution_replacement(
    replacement: &ExecutionScopeReplacementAuthority,
) -> DurableExecutionAttemptReplacementInput {
    let lease = |value: &ExecutionLeaseRecord| DurableExecutionLeaseInput {
        job_id: value.job_id.clone(),
        lease_id: value.lease_id.clone(),
        worker_id: value.worker_id.clone(),
        worker_instance_id: value.worker_instance_id.clone(),
        attempt: value.attempt,
        fencing_token: value.fencing_token.clone(),
    };
    DurableExecutionAttemptReplacementInput {
        receipt_id: replacement.receipt_id().clone(),
        receipt_digest: replacement.receipt_digest().clone(),
        delivery_id: replacement.scope().delivery_id.clone(),
        product_session_id: replacement.scope().product_session_id.clone(),
        work_run_id: replacement.work_run_id().cloned(),
        predecessor_lease: lease(replacement.predecessor_lease()),
        predecessor_worker_session_id: replacement.previous_worker_session_id().cloned(),
        predecessor_slot: replacement
            .predecessor_slot()
            .map(|slot| DurableWorkerSlotInput {
                worker_id: slot.worker_id.clone(),
                worker_instance_id: slot.worker_instance_id.clone(),
                worker_session_id: slot.worker_session_id.clone(),
                codex_thread_id: slot.codex_thread_id.clone(),
                job_id: slot.job_id.clone(),
                lease_id: slot.lease_id.clone(),
                attempt: slot.attempt,
                fencing_token: slot.fencing_token.clone(),
            }),
        successor_lease: lease(replacement.replacement_lease()),
    }
}
