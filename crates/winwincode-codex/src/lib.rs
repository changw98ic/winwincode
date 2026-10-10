// SPDX-License-Identifier: Apache-2.0

//! Production embedded Codex Core composition for the Execution Worker.
//!
//! The adapter links [`winwincode_kernel::Kernel`] in process. Model traffic is
//! reduced to generated `model.open`, `model.chunk`, and `model.ack` messages by
//! [`model_port_client::WorkerModelPortClient`].
//! `WorkerMain` remains the only owner of the outbound `ExecutionPort`.

mod action_bridge;
mod adapter;
pub mod candidate_artifact_outbox;
mod contract;
pub mod diagnostic_artifact_outbox;
mod failure_diagnostic;
pub use failure_diagnostic::CodexFailureDiagnostic;
mod durable_fusion;
pub use durable_fusion::FusionPanelFuture;
pub mod fusion_provider;
mod helper_release;
mod judge_bridge;
mod model_bridge;
pub mod model_port_client;
mod outbox;
pub use fusion_provider::FusionModelPortProvider;
mod parallel_model_runner;
pub use parallel_model_runner::{
    FusionPanelSeat, ParallelModelAttempt, ParallelModelAttemptRecord, ParallelModelBatchResult,
    ParallelModelBudget, ParallelModelCancelHandle, ParallelModelCancelSignal, ParallelModelResult,
    ParallelModelRunError, ParallelModelRunner, ParallelModelStatus, ParallelModelTarget,
    ParallelModelTotals, fusion_compose_frame_rows, fusion_panel, parallel_model_cancellation,
};
mod performance;
pub mod performance_evidence;
mod public_smoke_adapter;
pub mod stage_product;
mod store;
mod structured_result;
mod tool_dependencies;
mod tool_input_source;
pub mod workrun_runtime_projection;

pub use adapter::{
    ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexError, ProductionCodexErrorKind,
    ProductionCodexInstallation, ProductionCodexOptions,
};
#[cfg(feature = "test-support")]
pub use adapter::{
    ProductionDelegatedTransitionFault, ProductionEventPollFault, ProductionFormatRepairFault,
    ProductionSubmissionFault,
};
pub use contract::{
    ActionRequestTransport, ArtifactAckOutcome, CodexCoreAdapter, CodexPoll, CodexRunKey,
    CodexRunKeyError, CodexThreadSession, CodexThreadStart, CodexTurnCompletion,
    DelegatedLoopPhase, DelegatedLoopStopFact, DelegatedLoopTransition,
    DelegatedLoopTransitionOutcome, DelegatedObserverPreflight, DelegatedObserverPreflightOutcome,
    DelegatedObserverSettlement, DurableExecutionDelivery, ExecutionPortFailureKind,
    LocalModelStartGuard, WorkerExecutionPort, delegated_loop_turn_id, secret_safe_runtime_summary,
};
pub use diagnostic_artifact_outbox::{
    DiagnosticArtifactAckOutcome, DiagnosticArtifactAuthority, DiagnosticArtifactUpload,
    RetainedDiagnosticArtifact,
};
pub use helper_release::{HelperReleaseManifest, HelperReleaseManifestError};
pub use model_bridge::set_model_intake_log_path;
pub use winwincode_execution_port::runtime_trace_outbox::{ExecutionMode, ObserverMode};
pub use winwincode_kernel::RoleExecutionMode;

#[cfg(test)]
mod audit_model_metrics;

#[cfg(test)]
mod storage_mechanism_regression;
