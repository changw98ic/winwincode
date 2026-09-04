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
mod helper_release;
mod model_bridge;
pub mod model_port_client;
mod outbox;
mod performance;
pub mod stage_product;
pub mod stage_runtime_projection;
mod store;

pub use adapter::{
    ProductionCodexAdapter, ProductionCodexConfig, ProductionCodexError, ProductionCodexErrorKind,
    ProductionCodexInstallation, ProductionCodexOptions,
};
#[cfg(feature = "test-support")]
pub use adapter::{
    ProductionEventPollFault, ProductionFormatRepairFault, ProductionSubmissionFault,
};
pub use contract::{
    ActionRequestTransport, CodexCoreAdapter, CodexPoll, CodexRunKey, CodexRunKeyError,
    CodexThreadStart, CodexTurnCompletion, DurableExecutionDelivery, WorkerExecutionPort,
    secret_safe_runtime_summary,
};
pub use helper_release::{HelperReleaseManifest, HelperReleaseManifestError};
pub use winwincode_execution_port::runtime_trace_outbox::{ExecutionMode, ObserverMode};
pub use winwincode_kernel::RoleExecutionMode;
