// SPDX-License-Identifier: Apache-2.0

//! Worker-owned lifecycle for one short-lived `DebugProbe` experiment.
//!
//! The workspace and Action Gateway are injected at this seam so production
//! assembly can use the existing `WorkspaceManager`, change-batch executor,
//! and `DurableWorkerActionEnforcement` without creating a second execution
//! authority.  This module never calls candidate snapshot APIs.

use winwincode_domain::{DebugExperimentId, Instant, WorkspaceRevision};
use winwincode_execution_port::{
    debug_experiment::{
        DebugExperimentError, InstrumentationDsl, InstrumentationOperation,
        ValidatedInstrumentationDsl, cleanup_allows_reuse, transition_experiment,
        validate_instrumentation_dsl, validate_ttl,
    },
    generated::{
        DebugExperiment, DebugExperimentCleanupReceipt, DebugExperimentCleanupStatus,
        DebugExperimentStatus, DebugProbeError, DebugProbeErrorCode, DebugProbeIdentity,
    },
};

/// Workspace seam backed by the existing detached Worker workspace.
///
/// `apply_log_probe` is where a production adapter delegates a canonical
/// change batch. `snapshot_revision` is a tree snapshot, not a candidate
/// commit; `rollback` and `cleanup` must be durable and idempotent.
pub trait EphemeralExperimentWorkspace {
    /// Returns the accepted revision from which this experiment was opened.
    fn base_revision(&self) -> &WorkspaceRevision;
    /// Applies one validated log probe through the existing change-batch path.
    ///
    /// # Errors
    ///
    /// Returns a host-adapter error when the canonical change batch is rejected.
    fn apply_log_probe(
        &mut self,
        path: &str,
        symbol: &str,
        expression: &str,
    ) -> Result<(), ExperimentRuntimeError>;
    /// Returns the current ephemeral tree revision.
    ///
    /// # Errors
    ///
    /// Returns a host-adapter error when the tree cannot be snapshotted.
    fn snapshot_revision(&mut self) -> Result<WorkspaceRevision, ExperimentRuntimeError>;
    /// Restores the accepted base tree before cleanup or retry.
    ///
    /// # Errors
    ///
    /// Returns a host-adapter error when the revision cannot be restored.
    fn rollback(&mut self, base: &WorkspaceRevision) -> Result<(), ExperimentRuntimeError>;
    /// Removes checkout, process, port, and temporary resources. The returned
    /// count is the authoritative residual-resource count.
    ///
    /// # Errors
    ///
    /// Returns a host-adapter error when cleanup cannot be confirmed.
    fn cleanup(&mut self) -> Result<u32, ExperimentRuntimeError>;
}

/// Action Gateway seam for declared `repeat_test` operations.
pub trait ExperimentActionGate {
    /// Executes one exact, host-admitted command. Shell text is never accepted.
    ///
    /// # Errors
    ///
    /// Returns a host-adapter error when the Action Gateway rejects execution.
    fn execute_repeat_test(
        &mut self,
        path: &str,
        command: &[String],
    ) -> Result<(), ExperimentRuntimeError>;
}

/// Adapter for the released detached `WorkerWorkspace` primitive. The
/// closure is the injected D4/PR3 change-batch application point; all Git
/// snapshot, rollback, and removal operations stay in `workspace.rs`.
pub struct WorkerWorkspaceExperimentAdapter<F> {
    workspace: Option<super::workspace::WorkerWorkspace>,
    base_revision: WorkspaceRevision,
    apply_log_probe: F,
}

impl<F> WorkerWorkspaceExperimentAdapter<F> {
    /// Couples one detached workspace to its canonical change-batch adapter.
    #[must_use]
    pub fn new(workspace: super::workspace::WorkerWorkspace, apply_log_probe: F) -> Self {
        Self {
            base_revision: workspace.resolved_source_tree(),
            workspace: Some(workspace),
            apply_log_probe,
        }
    }
}

impl<F> EphemeralExperimentWorkspace for WorkerWorkspaceExperimentAdapter<F>
where
    F: FnMut(
        &mut super::workspace::WorkerWorkspace,
        &str,
        &str,
        &str,
    ) -> Result<(), ExperimentRuntimeError>,
{
    fn base_revision(&self) -> &WorkspaceRevision {
        &self.base_revision
    }

    fn apply_log_probe(
        &mut self,
        path: &str,
        symbol: &str,
        expression: &str,
    ) -> Result<(), ExperimentRuntimeError> {
        let workspace = self
            .workspace
            .as_mut()
            .ok_or_else(|| ExperimentRuntimeError::host("experiment workspace was released"))?;
        (self.apply_log_probe)(workspace, path, symbol, expression)
    }

    fn snapshot_revision(&mut self) -> Result<WorkspaceRevision, ExperimentRuntimeError> {
        self.workspace
            .as_mut()
            .ok_or_else(|| ExperimentRuntimeError::host("experiment workspace was released"))?
            .snapshot_ephemeral_revision()
            .map_err(|_| ExperimentRuntimeError::host("ephemeral tree snapshot failed"))
    }

    fn rollback(&mut self, base: &WorkspaceRevision) -> Result<(), ExperimentRuntimeError> {
        self.workspace
            .as_mut()
            .ok_or_else(|| ExperimentRuntimeError::host("experiment workspace was released"))?
            .rollback_ephemeral_revision(base)
            .map_err(|_| ExperimentRuntimeError::host("ephemeral rollback failed"))
    }

    fn cleanup(&mut self) -> Result<u32, ExperimentRuntimeError> {
        let workspace = self
            .workspace
            .as_mut()
            .ok_or_else(|| ExperimentRuntimeError::host("experiment workspace was released"))?;
        let result = workspace.close_in_place(super::workspace::WorkspaceCloseReason::Completed);
        if result.is_ok() {
            self.workspace = None;
            Ok(0)
        } else {
            Err(ExperimentRuntimeError::host(
                "ephemeral workspace cleanup failed",
            ))
        }
    }
}

/// Runtime failure with a bounded, secret-free message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExperimentRuntimeError {
    message: &'static str,
}

impl ExperimentRuntimeError {
    /// Creates a host-adapter failure without retaining command output.
    #[must_use]
    pub const fn host(message: &'static str) -> Self {
        Self { message }
    }
}

impl std::fmt::Display for ExperimentRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ExperimentRuntimeError {}

/// Error returned before an unsafe operation or an invalid state transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EphemeralExperimentError {
    Contract(DebugExperimentError),
    Runtime(ExperimentRuntimeError),
    BaseRevisionMismatch,
    Expired,
    CleanupRequired,
}

impl From<DebugExperimentError> for EphemeralExperimentError {
    fn from(error: DebugExperimentError) -> Self {
        Self::Contract(error)
    }
}

impl From<ExperimentRuntimeError> for EphemeralExperimentError {
    fn from(error: ExperimentRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// Result of one bounded run. The revision is intentionally informational and
/// cannot be promoted by this API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExperimentRunReport {
    pub experiment_id: DebugExperimentId,
    pub experiment_revision: WorkspaceRevision,
    pub repeated_tests: u32,
}

/// Ownership state of the experiment's exclusive workspace barrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExperimentWorkspaceBarrier {
    /// The detached checkout and declared resources are exclusively held.
    Held,
    /// Cleanup released the checkout and all associated resources.
    Released,
}

/// A durable lifecycle record coupled to one isolated workspace.
pub struct EphemeralExperiment<W, G> {
    workspace: W,
    gate: G,
    dsl: ValidatedInstrumentationDsl,
    record: DebugExperiment,
    barrier: ExperimentWorkspaceBarrier,
}

impl<W, G> EphemeralExperiment<W, G>
where
    W: EphemeralExperimentWorkspace,
    G: ExperimentActionGate,
{
    /// Plans an experiment from one accepted workspace revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the DSL, TTL, or workspace identity is invalid.
    pub fn create(
        workspace: W,
        gate: G,
        identity: DebugProbeIdentity,
        experiment_id: DebugExperimentId,
        dsl: InstrumentationDsl,
        created_at: Instant,
        expires_at: Instant,
    ) -> Result<Self, EphemeralExperimentError> {
        let dsl = validate_instrumentation_dsl(dsl)?;
        validate_ttl(&created_at.0, &expires_at.0)?;
        if workspace.base_revision() != &identity.workspace_revision {
            return Err(EphemeralExperimentError::BaseRevisionMismatch);
        }
        let base_revision = workspace.base_revision().clone();
        Ok(Self {
            workspace,
            gate,
            record: DebugExperiment {
                base_revision,
                cleanup_receipt: None,
                created_at,
                experiment_id,
                experiment_revision: None,
                expires_at,
                identity,
                instrumentation_digest: dsl.digest().clone(),
                schema_version: 1,
                status: DebugExperimentStatus::Planned,
            },
            dsl,
            barrier: ExperimentWorkspaceBarrier::Held,
        })
    }

    /// Returns the current durable experiment record.
    #[must_use]
    pub const fn record(&self) -> &DebugExperiment {
        &self.record
    }

    /// Returns whether the exclusive workspace barrier is still held.
    #[must_use]
    pub const fn barrier(&self) -> ExperimentWorkspaceBarrier {
        self.barrier
    }

    /// Moves Planned → Preparing → Ready after the workspace barrier is held.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace revision or lifecycle is stale.
    pub fn prepare(&mut self) -> Result<(), EphemeralExperimentError> {
        self.move_to(DebugExperimentStatus::Preparing)?;
        if self.workspace.base_revision() != &self.record.base_revision {
            return Err(EphemeralExperimentError::BaseRevisionMismatch);
        }
        self.move_to(DebugExperimentStatus::Ready)
    }

    /// Reconciles a durable record after Worker restart. Any non-terminal
    /// experiment is quarantined for cleanup; it is never resumed in place.
    ///
    /// # Errors
    ///
    /// Returns an error when the persisted lifecycle cannot enter cleanup.
    pub fn recover_after_crash(&mut self) -> Result<(), EphemeralExperimentError> {
        if matches!(
            self.record.status,
            DebugExperimentStatus::CleanedUp | DebugExperimentStatus::CleanupFailed
        ) {
            return Ok(());
        }
        self.move_to(DebugExperimentStatus::CleanupPending)
    }

    /// Applies probes and runs declared tests through the Action Gateway.
    ///
    /// # Errors
    ///
    /// Returns an error when the TTL, barrier, lifecycle, workspace, or Action
    /// Gateway contract rejects the operation.
    pub fn run(&mut self, now: &Instant) -> Result<ExperimentRunReport, EphemeralExperimentError> {
        if now.0 >= self.record.expires_at.0 {
            return Err(EphemeralExperimentError::Expired);
        }
        if self.record.status != DebugExperimentStatus::Ready {
            return Err(EphemeralExperimentError::CleanupRequired);
        }
        if self.barrier != ExperimentWorkspaceBarrier::Held {
            return Err(EphemeralExperimentError::CleanupRequired);
        }
        self.move_to(DebugExperimentStatus::Running)?;
        let mut repeated_tests = 0_u32;
        for operation in self.dsl.operations() {
            let result = match operation {
                InstrumentationOperation::InsertLogProbe {
                    path,
                    symbol,
                    expression,
                } => self.workspace.apply_log_probe(path, symbol, expression),
                InstrumentationOperation::RepeatTest {
                    path,
                    command,
                    repetitions,
                } => {
                    let mut result = Ok(());
                    for _ in 0..*repetitions {
                        if let Err(error) = self.gate.execute_repeat_test(path, command) {
                            result = Err(error);
                            break;
                        }
                        repeated_tests = repeated_tests.saturating_add(1);
                    }
                    result
                }
            };
            if let Err(error) = result {
                let _ = self.move_to(DebugExperimentStatus::CleanupPending);
                return Err(error.into());
            }
        }
        let revision = match self.workspace.snapshot_revision() {
            Ok(revision) => revision,
            Err(error) => {
                let _ = self.move_to(DebugExperimentStatus::CleanupPending);
                return Err(error.into());
            }
        };
        self.record.experiment_revision = Some(revision.clone());
        self.move_to(DebugExperimentStatus::Ready)?;
        Ok(ExperimentRunReport {
            experiment_id: self.record.experiment_id.clone(),
            experiment_revision: revision,
            repeated_tests,
        })
    }

    /// Requests cancellation and performs mandatory cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle or cleanup contract rejects it.
    pub fn cancel(&mut self, cleaned_at: Instant) -> Result<(), EphemeralExperimentError> {
        if self.record.status != DebugExperimentStatus::CleanupPending {
            self.move_to(DebugExperimentStatus::Cancelled)?;
        }
        self.cleanup(cleaned_at)
    }

    /// Marks TTL expiry and performs mandatory cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle or cleanup contract rejects it.
    pub fn expire(&mut self, cleaned_at: Instant) -> Result<(), EphemeralExperimentError> {
        if matches!(
            self.record.status,
            DebugExperimentStatus::CleanedUp | DebugExperimentStatus::CleanupFailed
        ) {
            return Err(EphemeralExperimentError::CleanupRequired);
        }
        self.move_to(DebugExperimentStatus::Expired)?;
        self.cleanup(cleaned_at)
    }

    /// Cleans the detached workspace and records the receipt. A failure keeps
    /// the experiment quarantined and makes reuse impossible.
    ///
    /// # Errors
    ///
    /// Returns an error when rollback or resource cleanup fails.
    pub fn cleanup(&mut self, cleaned_at: Instant) -> Result<(), EphemeralExperimentError> {
        if self.record.status != DebugExperimentStatus::CleanupPending {
            self.move_to(DebugExperimentStatus::CleanupPending)?;
        }
        let cleanup = self.workspace.rollback(&self.record.base_revision);
        let residual = match cleanup.and_then(|()| self.workspace.cleanup()) {
            Ok(residual) => residual,
            Err(error) => {
                self.record.status = DebugExperimentStatus::CleanupFailed;
                self.record.cleanup_receipt = Some(self.receipt(
                    cleaned_at,
                    DebugExperimentCleanupStatus::CleanupFailed,
                    1,
                    Some(DebugProbeError {
                        code: DebugProbeErrorCode::ProcessCleanupFailed,
                        message: "ephemeral experiment cleanup failed".into(),
                        retryable: false,
                    }),
                ));
                return Err(error.into());
            }
        };
        if !cleanup_allows_reuse(&DebugExperimentStatus::CleanedUp, residual) {
            self.record.status = DebugExperimentStatus::CleanupFailed;
            self.record.cleanup_receipt = Some(self.receipt(
                cleaned_at,
                DebugExperimentCleanupStatus::CleanupFailed,
                residual,
                Some(DebugProbeError {
                    code: DebugProbeErrorCode::ProcessCleanupFailed,
                    message: "ephemeral experiment left residual resources".into(),
                    retryable: false,
                }),
            ));
            return Err(
                ExperimentRuntimeError::host("experiment cleanup left residual resources").into(),
            );
        }
        self.record.status = DebugExperimentStatus::CleanedUp;
        self.barrier = ExperimentWorkspaceBarrier::Released;
        self.record.cleanup_receipt =
            Some(self.receipt(cleaned_at, DebugExperimentCleanupStatus::CleanedUp, 0, None));
        Ok(())
    }

    fn receipt(
        &self,
        cleaned_at: Instant,
        status: DebugExperimentCleanupStatus,
        residual_resource_count: u32,
        error: Option<DebugProbeError>,
    ) -> DebugExperimentCleanupReceipt {
        DebugExperimentCleanupReceipt {
            cleaned_at,
            error,
            experiment_id: self.record.experiment_id.clone(),
            identity: self.record.identity.clone(),
            residual_resource_count: i64::from(residual_resource_count),
            status,
        }
    }

    fn move_to(&mut self, next: DebugExperimentStatus) -> Result<(), EphemeralExperimentError> {
        self.record.status = transition_experiment(&self.record.status, next)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Debug)]
    struct Workspace {
        base: WorkspaceRevision,
        revision: WorkspaceRevision,
        applied: Cell<u32>,
        fail_cleanup: bool,
    }

    impl EphemeralExperimentWorkspace for Workspace {
        fn base_revision(&self) -> &WorkspaceRevision {
            &self.base
        }
        fn apply_log_probe(
            &mut self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(), ExperimentRuntimeError> {
            self.applied.set(self.applied.get() + 1);
            Ok(())
        }
        fn snapshot_revision(&mut self) -> Result<WorkspaceRevision, ExperimentRuntimeError> {
            Ok(self.revision.clone())
        }
        fn rollback(&mut self, _: &WorkspaceRevision) -> Result<(), ExperimentRuntimeError> {
            Ok(())
        }
        fn cleanup(&mut self) -> Result<u32, ExperimentRuntimeError> {
            if self.fail_cleanup {
                Err(ExperimentRuntimeError::host("cleanup failed"))
            } else {
                Ok(0)
            }
        }
    }

    #[derive(Debug, Default)]
    struct Gate {
        calls: u32,
    }
    impl ExperimentActionGate for Gate {
        fn execute_repeat_test(
            &mut self,
            _: &str,
            _: &[String],
        ) -> Result<(), ExperimentRuntimeError> {
            self.calls += 1;
            Ok(())
        }
    }

    fn identity(revision: &WorkspaceRevision) -> DebugProbeIdentity {
        serde_json::from_value(serde_json::json!({
            "attempt": 1, "debugSessionId": "ses_00000000000000000000000000", "environmentDigest": format!("sha256:{}", "a".repeat(64)),
            "fencingToken": "1", "jobId": "job_00000000000000000000000000", "leaseId": "lea_00000000000000000000000000", "probeExecutionId": format!("sha256:{}", "b".repeat(64)),
            "probeId": "prb_00000000000000000000000000", "repositoryId": "repo_0000000000000000000000000", "roundId": "rnd_00000000000000000000000000", "sessionIdentity": {
              "workerSessionId":"wse_00000000000000000000000000", "productSessionId":"pse_00000000000000000000000000", "workRunId":"wrn_01J00000000000000000000000", "codexThreadId":"thr_00000000000000000000000000"
            }, "workspaceRevision": revision
        })).unwrap()
    }

    #[test]
    fn runs_through_action_gate_and_cleans_ephemeral_revision() {
        let base = WorkspaceRevision(format!("git-tree:{}", "a".repeat(40)));
        let workspace = Workspace {
            base: base.clone(),
            revision: WorkspaceRevision(format!("git-tree:{}", "b".repeat(40))),
            applied: Cell::new(0),
            fail_cleanup: false,
        };
        let dsl = InstrumentationDsl {
            operations: vec![InstrumentationOperation::RepeatTest {
                path: "tests/smoke.rs".into(),
                command: vec!["cargo".into(), "test".into()],
                repetitions: 2,
            }],
        };
        let mut experiment = EphemeralExperiment::create(
            workspace,
            Gate::default(),
            identity(&base),
            DebugExperimentId("exp_00000000000000000000000000".into()),
            dsl,
            Instant("2026-09-07T00:00:00Z".into()),
            Instant("2026-09-07T00:00:10Z".into()),
        )
        .unwrap();
        experiment.prepare().unwrap();
        assert_eq!(
            experiment
                .run(&Instant("2026-09-07T00:00:01Z".into()))
                .unwrap()
                .repeated_tests,
            2
        );
        experiment
            .expire(Instant("2026-09-07T00:00:11Z".into()))
            .unwrap();
        assert_eq!(experiment.record().status, DebugExperimentStatus::CleanedUp);
        assert_eq!(experiment.barrier(), ExperimentWorkspaceBarrier::Released);
        assert_eq!(
            experiment
                .record()
                .cleanup_receipt
                .as_ref()
                .unwrap()
                .residual_resource_count,
            0
        );
    }

    #[test]
    fn crash_recovery_and_cleanup_failure_quarantine_reuse() {
        let base = WorkspaceRevision(format!("git-tree:{}", "a".repeat(40)));
        let workspace = Workspace {
            base: base.clone(),
            revision: base.clone(),
            applied: Cell::new(0),
            fail_cleanup: true,
        };
        let dsl = InstrumentationDsl {
            operations: vec![InstrumentationOperation::RepeatTest {
                path: "tests/smoke.rs".into(),
                command: vec!["cargo".into(), "test".into()],
                repetitions: 1,
            }],
        };
        let mut experiment = EphemeralExperiment::create(
            workspace,
            Gate::default(),
            identity(&base),
            DebugExperimentId("exp_00000000000000000000000000".into()),
            dsl,
            Instant("2026-09-07T00:00:00Z".into()),
            Instant("2026-09-07T00:00:10Z".into()),
        )
        .unwrap();
        experiment.prepare().unwrap();
        experiment.recover_after_crash().unwrap();
        assert_eq!(
            experiment.record().status,
            DebugExperimentStatus::CleanupPending
        );
        assert!(
            experiment
                .cleanup(Instant("2026-09-07T00:00:02Z".into()))
                .is_err()
        );
        assert_eq!(
            experiment.record().status,
            DebugExperimentStatus::CleanupFailed
        );
        assert_eq!(
            experiment
                .record()
                .cleanup_receipt
                .as_ref()
                .unwrap()
                .residual_resource_count,
            1
        );
    }
}
