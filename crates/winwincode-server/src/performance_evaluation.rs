// SPDX-License-Identifier: Apache-2.0

//! Production composition for one durable React-versus-Delegated pair.
//!
//! This entry point accepts one durable pair-slot identity, not assignments,
//! filesystem paths, raw samples, or caller-computed statistics. It resolves
//! the Worker ledger from trusted server composition, reloads both assignments
//! from the Control Plane, and persists only the resulting opaque pair.

use std::fmt;
use std::path::{Path, PathBuf};

use winwincode_codex::performance_evidence::{
    ProductionPerformanceEvaluationArm, ProductionPerformanceEvidenceError,
    export_performance_evaluation_arm,
};
use winwincode_control_plane::performance_evaluation_projection::{
    DurablePerformanceEvaluationAuthority, PerformanceEvaluationProjectionError,
    WorkerPerformanceArmAuthorityV1, WorkerPerformanceModelCallAuthorityV1,
};
use winwincode_control_plane::rollout_evaluation::{
    RecordProjectedEvaluationPair, RolloutEvaluationErrorKind, RolloutEvaluationService,
};
use winwincode_control_plane::rollout_gate::{PutRolloutGatePolicy, RolloutGateMutationReceipt};
use winwincode_domain::{RepositoryScope, Sha256Digest};
use winwincode_execution_port::performance_evaluation::EvaluationAssignmentV1;
use winwincode_storage::{ProductStateStorage, SqliteStorage};

/// Frozen inputs required to project and retain one exact pair.
#[derive(Clone, Debug)]
pub struct RecordProductionPerformancePair {
    pub scope: RepositoryScope,
    pub policy_revision: u64,
    pub pair_id: Sha256Digest,
    pub expected_gate_revision: u64,
    pub occurred_at_millis: u64,
}

/// Server-owned production operation over one Control Plane database.
#[derive(Clone, Debug)]
pub struct ProductionPerformanceEvaluation {
    control_plane_data_directory: PathBuf,
    worker_data_directory: PathBuf,
}

impl ProductionPerformanceEvaluation {
    #[must_use]
    pub fn from_server_config(config: &crate::ServerConfig) -> Self {
        let control_plane_data_directory = config.data_directory().to_path_buf();
        Self {
            worker_data_directory: control_plane_data_directory.join("worker-runtime"),
            control_plane_data_directory,
        }
    }

    /// Validates source/cohort Artifacts and commits one statistical policy
    /// through the same Control Plane data-directory authority.
    ///
    /// # Errors
    ///
    /// Rejects an invalid Artifact, policy, request replay, or gate revision.
    pub fn put_policy(
        &self,
        command: PutRolloutGatePolicy,
    ) -> Result<RolloutGateMutationReceipt, ProductionPerformanceEvaluationError> {
        let mut authority =
            DurablePerformanceEvaluationAuthority::open(&self.control_plane_data_directory)
                .map_err(map_projection)?;
        let retained = authority.put_policy(command).map_err(map_projection);
        let close = authority.close().map_err(map_projection);
        match retained {
            Ok(receipt) => {
                close?;
                Ok(receipt)
            }
            Err(error) => {
                let _ = close;
                Err(error)
            }
        }
    }

    /// Projects and persists one complete pair from durable ledgers.
    ///
    /// The operation does not accept raw measurements, aggregate counts,
    /// thresholds, a bootstrap seed, or a rollout decision. The Control Plane
    /// derives the idempotency identity from the opaque pair digest and checks
    /// both one-shot assignments again while committing it.
    ///
    /// # Errors
    ///
    /// Returns a stable error when either Worker ledger is incomplete, the CP
    /// authority join fails, the active gate revision changes, or persistence
    /// is unavailable.
    pub fn record_pair(
        &self,
        command: RecordProductionPerformancePair,
    ) -> Result<Sha256Digest, ProductionPerformanceEvaluationError> {
        let mut authority =
            DurablePerformanceEvaluationAuthority::open(&self.control_plane_data_directory)
                .map_err(map_projection)?;
        let assignments = authority
            .load_consumed_pair(&command.scope, command.policy_revision, &command.pair_id)
            .map_err(map_projection)?;
        let react_assignment = assignments.react().clone();
        let delegated_assignment = assignments.delegated().clone();
        let react = export_arm(&self.worker_data_directory, &react_assignment)?;
        let delegated = export_arm(&self.worker_data_directory, &delegated_assignment)?;
        let react_worker = worker_authority(&react);
        let delegated_worker = worker_authority(&delegated);
        let projected = authority
            .project_pair(
                react_assignment,
                &react_worker,
                delegated_assignment,
                &delegated_worker,
            )
            .map_err(map_projection);
        let close = authority.close().map_err(map_projection);
        let projected = match projected {
            Ok(projected) => {
                close?;
                projected
            }
            Err(error) => {
                let _ = close;
                return Err(error);
            }
        };

        let mut storage = SqliteStorage::open(&self.control_plane_data_directory)
            .map_err(|_| ProductionPerformanceEvaluationError::Unavailable)?;
        let recorded = RolloutEvaluationService::new(&mut storage)
            .record_projected_pair(RecordProjectedEvaluationPair {
                scope: command.scope,
                expected_gate_revision: command.expected_gate_revision,
                projected_pair: projected,
                occurred_at_millis: command.occurred_at_millis,
            })
            .map_err(|error| match error.kind() {
                RolloutEvaluationErrorKind::RevisionConflict => {
                    ProductionPerformanceEvaluationError::RevisionConflict
                }
                RolloutEvaluationErrorKind::Storage => {
                    ProductionPerformanceEvaluationError::Unavailable
                }
                RolloutEvaluationErrorKind::Invalid | RolloutEvaluationErrorKind::Corrupt => {
                    ProductionPerformanceEvaluationError::InvalidAuthority
                }
            });
        let close = Box::new(storage)
            .close()
            .map_err(|_| ProductionPerformanceEvaluationError::Unavailable);
        match recorded {
            Ok(digest) => {
                close?;
                Ok(digest)
            }
            Err(error) => {
                let _ = close;
                Err(error)
            }
        }
    }
}

/// Stable failure categories without filesystem or private identity content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionPerformanceEvaluationError {
    Unavailable,
    IncompleteAuthority,
    InvalidAuthority,
    ObserverAuthorityUnavailable,
    RevisionConflict,
}

impl fmt::Display for ProductionPerformanceEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "performance evaluation storage is unavailable",
            Self::IncompleteAuthority => "performance evaluation authority is incomplete",
            Self::InvalidAuthority => "performance evaluation authority is invalid",
            Self::ObserverAuthorityUnavailable => {
                "Observer performance authority is not retained end to end"
            }
            Self::RevisionConflict => "performance evaluation policy revision changed",
        })
    }
}

impl std::error::Error for ProductionPerformanceEvaluationError {}

fn export_arm(
    data_directory: &Path,
    assignment: &EvaluationAssignmentV1,
) -> Result<ProductionPerformanceEvaluationArm, ProductionPerformanceEvaluationError> {
    assignment
        .validate()
        .map_err(|_| ProductionPerformanceEvaluationError::InvalidAuthority)?;
    export_performance_evaluation_arm(
        data_directory,
        &assignment.spec().run_id,
        &assignment.spec().job_id,
    )
    .map_err(map_worker)
}

fn worker_authority(arm: &ProductionPerformanceEvaluationArm) -> WorkerPerformanceArmAuthorityV1 {
    WorkerPerformanceArmAuthorityV1 {
        measurement: arm.measurement().clone(),
        candidate_artifact: arm.candidate_artifact().clone(),
        candidate_artifact_ack_revision: arm.candidate_artifact_ack_revision(),
        worker_ledger_snapshot_digest: arm.worker_ledger_snapshot_digest().clone(),
        primary_model_calls: arm
            .primary_model_calls()
            .iter()
            .map(|call| WorkerPerformanceModelCallAuthorityV1 {
                model_call_digest: call.model_call_digest().clone(),
                request_id: call.request_id().clone(),
                initial_model_exchange_id: call.initial_model_exchange_id().clone(),
            })
            .collect(),
    }
}

fn map_worker(error: ProductionPerformanceEvidenceError) -> ProductionPerformanceEvaluationError {
    match error {
        ProductionPerformanceEvidenceError::Unavailable => {
            ProductionPerformanceEvaluationError::Unavailable
        }
        ProductionPerformanceEvidenceError::ObserverAuthorityUnavailable => {
            ProductionPerformanceEvaluationError::ObserverAuthorityUnavailable
        }
        ProductionPerformanceEvidenceError::Corrupt
        | ProductionPerformanceEvidenceError::Inconsistent(_) => {
            ProductionPerformanceEvaluationError::InvalidAuthority
        }
    }
}

fn map_projection(
    error: PerformanceEvaluationProjectionError,
) -> ProductionPerformanceEvaluationError {
    match error {
        PerformanceEvaluationProjectionError::Unavailable => {
            ProductionPerformanceEvaluationError::Unavailable
        }
        PerformanceEvaluationProjectionError::IncompleteAuthority => {
            ProductionPerformanceEvaluationError::IncompleteAuthority
        }
        PerformanceEvaluationProjectionError::InvalidAuthority => {
            ProductionPerformanceEvaluationError::InvalidAuthority
        }
        PerformanceEvaluationProjectionError::ObserverAuthorityUnavailable => {
            ProductionPerformanceEvaluationError::ObserverAuthorityUnavailable
        }
        PerformanceEvaluationProjectionError::RevisionConflict => {
            ProductionPerformanceEvaluationError::RevisionConflict
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    use winwincode_domain::{
        OrganizationId, ProjectId, RepositoryId, RepositoryScopeKind, WorkspaceId,
    };

    use super::*;

    #[test]
    fn production_entry_resolves_worker_ledger_from_server_composition() {
        let data_directory = std::env::temp_dir().join("winwincode-performance-entry-fixture");
        let config = crate::ServerConfig::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            "http://127.0.0.1",
            crate::ServerTls::Disabled,
            ["http://127.0.0.1".to_owned()],
            data_directory.clone(),
            Duration::from_secs(1),
        )
        .expect("server configuration");
        let entry = ProductionPerformanceEvaluation::from_server_config(&config);
        assert_eq!(entry.control_plane_data_directory, data_directory);
        assert_eq!(
            entry.worker_data_directory,
            config.data_directory().join("worker-runtime")
        );

        let command = RecordProductionPerformancePair {
            scope: RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
                project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
                repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
            },
            policy_revision: 1,
            pair_id: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            expected_gate_revision: 1,
            occurred_at_millis: 1,
        };
        assert_eq!(command.policy_revision, 1);
    }
}
