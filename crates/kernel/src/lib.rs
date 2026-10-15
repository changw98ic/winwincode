mod model_port;
#[cfg(test)]
mod task_handoff_tests;

pub use model_port::ModelPort;
pub use model_port::ModelPortFailure;
pub use model_port::ModelPortRequest;
pub use model_port::ModelPortStream;

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_core_api::AbsolutePathBuf;
use codex_core_api::ApprovalsReviewer;
use codex_core_api::AskForApproval;
use codex_core_api::AuthManager;
use codex_core_api::ClientMcpExtensions;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::ConfigBuilder;
use codex_core_api::Constrained;
use codex_core_api::DurableTurnInspection;
use codex_core_api::DurableTurnTerminal;
use codex_core_api::EnvironmentManager;
use codex_core_api::ExecServerRuntimePaths;
use codex_core_api::Feature;
use codex_core_api::ForkSnapshot;
use codex_core_api::NewThread;
use codex_core_api::Op;
use codex_core_api::PermissionProfile;
use codex_core_api::Permissions;
use codex_core_api::RecoverTurnRequest;
use codex_core_api::SessionSource;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::StateDbHandle;
use codex_core_api::SteerSubmission;
use codex_core_api::ThreadId;
use codex_core_api::ThreadManager;
use codex_core_api::ToolCallGate;
use codex_core_api::ToolCallGateAuthorization;
use codex_core_api::ToolCallGateExecutableAuthorization;
use codex_core_api::ToolCallGateFileOperation;
use codex_core_api::ToolCallGatePayload;
use codex_core_api::ToolCallGateRejection;
use codex_core_api::ToolCallGateRequest;
use codex_core_api::TurnInputRequest;
use codex_core_api::TurnInputSubmission;
use codex_core_api::TurnStartOptions;
use codex_core_api::UserInput;
use codex_core_api::build_models_manager;
use codex_core_api::init_state_db;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::resolve_installation_id;
use codex_core_api::thread_store_from_config;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::protocol::Event as CodexEvent;
use codex_protocol::protocol::EventMsg as CodexEventMsg;
use codex_protocol::protocol::ReviewDecision as CodexReviewDecision;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::request_user_input::RequestUserInputResponse;

#[cfg(feature = "test-support")]
mod mechanism_interaction_test_options;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use winwincode_execution_port::agent_config::{
    AgentSessionConfigSnapshot, validate_agent_session_config,
};
pub use winwincode_execution_port::generated::{
    RoleExecutionMode, RoleSessionPolicy, RoleSessionPolicyRoleId, RoleSessionPolicyWorkspaceMode,
};

use crate::model_port::KernelModelStreamTransport;

mod result_read;
mod tool_facts;
pub use result_read::KernelToolResultReadRequest;
pub use tool_facts::KernelToolRuntimeEvent;

#[derive(Clone, Eq, PartialEq)]
pub struct KernelActionRequest {
    pub session_id: String,
    pub turn_id: String,
    pub operation_id: String,
    pub namespace: Option<String>,
    pub tool_name: String,
    pub payload: KernelActionPayload,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct KernelActionAuthorization {
    request_binding: String,
    executable: Option<KernelExecutableAuthorization>,
}

impl fmt::Debug for KernelActionAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelActionAuthorization")
            .field("request_binding", &"<private>")
            .field("executable", &self.executable.as_ref().map(|_| "<private>"))
            .finish()
    }
}

impl KernelActionAuthorization {
    #[must_use]
    pub fn new(request_binding: String, executable: Option<KernelExecutableAuthorization>) -> Self {
        Self {
            request_binding,
            executable,
        }
    }

    #[must_use]
    pub fn request_binding(&self) -> &str {
        &self.request_binding
    }

    #[must_use]
    pub fn executable(&self) -> Option<&KernelExecutableAuthorization> {
        self.executable.as_ref()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct KernelExecutableAuthorization {
    canonical_absolute_path: String,
    arguments: Vec<String>,
    identity: String,
}

impl fmt::Debug for KernelExecutableAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelExecutableAuthorization")
            .field("canonical_absolute_path", &"<private>")
            .field("arguments", &"<private>")
            .field("identity", &"<private>")
            .finish()
    }
}

impl KernelExecutableAuthorization {
    #[must_use]
    pub fn new(canonical_absolute_path: String, arguments: Vec<String>, identity: String) -> Self {
        Self {
            canonical_absolute_path,
            arguments,
            identity,
        }
    }

    #[must_use]
    pub fn canonical_absolute_path(&self) -> &str {
        &self.canonical_absolute_path
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum KernelActionPayload {
    CoreControl {
        input: String,
    },
    Function {
        arguments: String,
    },
    ToolSearch {
        arguments_json: String,
    },
    Custom {
        input: String,
    },
    Shell {
        program: String,
        args: Vec<String>,
        working_directory: String,
    },
    FileRead {
        path: String,
    },
    McpResource {
        server: String,
        method: String,
        arguments: String,
    },
    ProcessInteraction {
        process_id: i32,
        origin_call_id: String,
        input: String,
    },
    Files {
        changes: Vec<KernelFileChange>,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct KernelFileChange {
    pub operation: KernelFileOperation,
    pub path: String,
    pub move_path: Option<String>,
}

impl fmt::Debug for KernelFileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelFileChange")
            .field("operation", &self.operation)
            .field("path", &"<private>")
            .field("move_path", &self.move_path.as_ref().map(|_| "<private>"))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelFileOperation {
    Create,
    Write,
    Delete,
}

impl fmt::Debug for KernelActionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelActionRequest")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("operation_id", &self.operation_id)
            .field("namespace", &self.namespace)
            .field("tool_name", &self.tool_name)
            .field("payload", &self.payload)
            .finish()
    }
}

impl fmt::Debug for KernelActionPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CoreControl { .. } => "CoreControl(<private>)",
            Self::Function { .. } => "Function(<private>)",
            Self::ToolSearch { .. } => "ToolSearch(<private>)",
            Self::Custom { .. } => "Custom(<private>)",
            Self::Shell { .. } => "Shell(<private>)",
            Self::FileRead { .. } => "FileRead(<private>)",
            Self::McpResource { .. } => "McpResource(<private>)",
            Self::ProcessInteraction { .. } => "ProcessInteraction(<private>)",
            Self::Files { .. } => "Files(<private>)",
        })
    }
}

pub use codex_core_api::ToolCallGatePayload as ToolInputGatePayload;
pub use codex_core_api::ToolCallGateRequest as ToolInputGateRequest;
pub use codex_core_api::{
    ToolCoalescingPermission, ToolDependencySnapshot, ToolInputContext, ToolInputProof,
    ToolInputProofRequest, ToolReusePermission,
};

pub trait KernelActionGate: Send + Sync {
    fn freeze_tool_input(
        &self,
        _context: ToolInputContext,
    ) -> BoxFuture<'static, Option<ToolDependencySnapshot>> {
        Box::pin(async { None })
    }
    fn verify_tool_input(
        &self,
        _request: ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<ToolInputProof>> {
        Box::pin(async { None })
    }

    fn authorize_result_read(
        &self,
        _request: KernelToolResultReadRequest,
    ) -> BoxFuture<'static, KernelResult<KernelActionAuthorization>> {
        Box::pin(async { Err(KernelFailure::action_rejected()) })
    }
    fn revalidate_result_read(
        &self,
        _request: KernelToolResultReadRequest,
        _authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, KernelResult<()>> {
        Box::pin(async { Err(KernelFailure::action_rejected()) })
    }

    fn authorize(
        &self,
        request: KernelActionRequest,
    ) -> BoxFuture<'static, KernelResult<KernelActionAuthorization>>;
    fn revalidate(
        &self,
        request: KernelActionRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, KernelResult<()>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RejectingKernelActionGate;

impl KernelActionGate for RejectingKernelActionGate {
    fn authorize(
        &self,
        _request: KernelActionRequest,
    ) -> BoxFuture<'static, KernelResult<KernelActionAuthorization>> {
        Box::pin(async {
            Err(KernelFailure::new(
                "ACTION_GATE_UNAVAILABLE",
                "embedded tool action authority is unavailable",
            ))
        })
    }

    fn revalidate(
        &self,
        _request: KernelActionRequest,
        _authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, KernelResult<()>> {
        Box::pin(async {
            Err(KernelFailure::new(
                "ACTION_GATE_UNAVAILABLE",
                "embedded tool action authority is unavailable",
            ))
        })
    }
}

struct CoreToolCallGate {
    host: Arc<dyn KernelActionGate>,
}

impl ToolCallGate for CoreToolCallGate {
    fn freeze_tool_input(
        &self,
        context: ToolInputContext,
    ) -> BoxFuture<'static, Option<ToolDependencySnapshot>> {
        self.host.freeze_tool_input(context)
    }
    fn verify_tool_input(
        &self,
        request: ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<ToolInputProof>> {
        self.host.verify_tool_input(request)
    }

    fn authorize_result_read(
        &self,
        request: codex_core_api::ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.authorize_result_read(request.into())
                .await
                .map(tool_call_gate_authorization)
                .map_err(|_| {
                    ToolCallGateRejection::new(
                        "RESULT_READ_DENIED",
                        "current result read authority rejected recovery",
                    )
                })
        })
    }

    fn revalidate_result_read(
        &self,
        request: codex_core_api::ToolResultReadRequest,
        authorization: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.revalidate_result_read(request.into(), kernel_action_authorization(&authorization))
                .await
                .map_err(|_| {
                    ToolCallGateRejection::new(
                        "RESULT_READ_STALE",
                        "result read authority is no longer current",
                    )
                })
        })
    }

    fn authorize(
        &self,
        request: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.authorize(kernel_action_request(request))
                .await
                .map(tool_call_gate_authorization)
                .map_err(|_| {
                    ToolCallGateRejection::new(
                        "HOST_ACTION_REJECTED",
                        "host action authority rejected the tool call",
                    )
                })
        })
    }

    fn revalidate(
        &self,
        request: ToolCallGateRequest,
        authorization: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.revalidate(
                kernel_action_request(request),
                kernel_action_authorization(&authorization),
            )
            .await
            .map_err(|_| {
                ToolCallGateRejection::new(
                    "HOST_ACTION_STALE",
                    "host action authority is no longer current",
                )
            })
        })
    }
}

fn tool_call_gate_authorization(
    authorization: KernelActionAuthorization,
) -> ToolCallGateAuthorization {
    ToolCallGateAuthorization::new(
        authorization.request_binding,
        authorization.executable.map(|executable| {
            ToolCallGateExecutableAuthorization::new(
                executable.canonical_absolute_path,
                executable.arguments,
                executable.identity,
            )
        }),
    )
}

fn kernel_action_authorization(
    authorization: &ToolCallGateAuthorization,
) -> KernelActionAuthorization {
    KernelActionAuthorization::new(
        authorization.request_binding().to_owned(),
        authorization.executable().map(|executable| {
            KernelExecutableAuthorization::new(
                executable.canonical_absolute_path().to_owned(),
                executable.arguments().to_vec(),
                executable.identity().to_owned(),
            )
        }),
    )
}

fn kernel_action_request(request: ToolCallGateRequest) -> KernelActionRequest {
    KernelActionRequest {
        session_id: request.thread_id,
        turn_id: request.turn_id,
        operation_id: request.call_id,
        namespace: request.namespace,
        tool_name: request.tool_name,
        payload: match request.payload {
            ToolCallGatePayload::CoreControl { input } => {
                KernelActionPayload::CoreControl { input }
            }
            ToolCallGatePayload::Function { arguments } => {
                KernelActionPayload::Function { arguments }
            }
            ToolCallGatePayload::ToolSearch { arguments_json } => {
                KernelActionPayload::ToolSearch { arguments_json }
            }
            ToolCallGatePayload::Custom { input } => KernelActionPayload::Custom { input },
            ToolCallGatePayload::FileRead { path } => KernelActionPayload::FileRead { path },
            ToolCallGatePayload::McpResource {
                server,
                method,
                arguments,
            } => KernelActionPayload::McpResource {
                server,
                method,
                arguments,
            },
            ToolCallGatePayload::ProcessInteraction {
                process_id,
                origin_call_id,
                input,
            } => KernelActionPayload::ProcessInteraction {
                process_id,
                origin_call_id,
                input,
            },
            ToolCallGatePayload::Shell {
                program,
                args,
                working_directory,
            } => KernelActionPayload::Shell {
                program,
                args,
                working_directory,
            },
            ToolCallGatePayload::Files { changes } => KernelActionPayload::Files {
                changes: changes
                    .into_iter()
                    .map(|change| KernelFileChange {
                        operation: match change.operation {
                            ToolCallGateFileOperation::Create => KernelFileOperation::Create,
                            ToolCallGateFileOperation::Write => KernelFileOperation::Write,
                            ToolCallGateFileOperation::Delete => KernelFileOperation::Delete,
                        },
                        path: change.path,
                        move_path: change.move_path,
                    })
                    .collect(),
            },
        },
    }
}

pub const CODEX_COMMIT: &str = "758ef40f50c1a458425c7cfbf1eb12cbc07af0b0";
pub const CODEX_TAG: &str = "rust-v0.149.0";
pub const INTERFACE_VERSION: u32 = 12;
pub const CODEX_PATCH_SET: &[&str] = &[
    "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
    "upstream/patches/codex/0002-inject-model-stream-transport.patch",
    "upstream/patches/codex/0003-export-config-builder.patch",
    "upstream/patches/codex/0005-remount-split-bwrap-root-read-only.patch",
    "upstream/patches/codex/0006-tool-gate-and-exact-turn-replay.patch",
    "upstream/patches/codex/0007-bind-tool-gate-executable-identity.patch",
    "upstream/patches/codex/0008-atomic-apply-patch.patch",
    "upstream/patches/codex/0009-canonicalize-intercepted-apply-patch.patch",
    "upstream/patches/codex/0010-submit-change-batch-handoff.patch",
    "upstream/patches/codex/0011-deterministic-tool-context-gc.patch",
    "upstream/patches/codex/0012-record-delegated-handoff-output.patch",
    "upstream/patches/codex/0013-code-mode-authorized-tool-definitions.patch",
    "upstream/patches/codex/0014-code-mode-dispatch-catalog.patch",
    "upstream/patches/codex/0015-code-mode-dispatch-portable-requests.patch",
    "upstream/patches/codex/0016-code-mode-dispatch-authorization.patch",
    "upstream/patches/codex/0017-code-mode-dispatch-hook-tests.patch",
    "upstream/patches/codex/0018-code-mode-dispatch-cell-scope.patch",
    "upstream/patches/codex/0019-code-mode-dispatch-continuations.patch",
    "upstream/patches/codex/0020-code-mode-dispatch-terminal-boundaries.patch",
    "upstream/patches/codex/0021-code-mode-dispatch-io-authorization.patch",
    "upstream/patches/codex/0022-tool-execution-facts-schema.patch",
    "upstream/patches/codex/0023-tool-execution-facts-store.patch",
    "upstream/patches/codex/0024-tool-execution-facts-store-tests.patch",
    "upstream/patches/codex/0025-tool-execution-facts-dispatch.patch",
    "upstream/patches/codex/0026-tool-execution-facts-wiring.patch",
    "upstream/patches/codex/0027-tool-execution-facts-tests.patch",
    "upstream/patches/codex/0028-tool-execution-facts-catalog-tests.patch",
    "upstream/patches/codex/0029-tool-runtime-recovery-relations-schema.patch",
    "upstream/patches/codex/0030-tool-runtime-recovery-relations-store.patch",
    "upstream/patches/codex/0031-tool-runtime-recovery-read-authority.patch",
    "upstream/patches/codex/0032-tool-runtime-recovery-result-recovery.patch",
    "upstream/patches/codex/0033-tool-runtime-recovery-recovery-tests.patch",
    "upstream/patches/codex/0034-tool-runtime-recovery-cell-lifecycle.patch",
    "upstream/patches/codex/0035-tool-runtime-reconciliation-state.patch",
    "upstream/patches/codex/0036-tool-runtime-reconciliation-dispatch.patch",
    "upstream/patches/codex/0037-tool-runtime-reconciliation-tests.patch",
    "upstream/patches/codex/0038-tool-runtime-reconciliation-process-receipt.patch",
    "upstream/patches/codex/0039-tool-diagnostics-schema.patch",
    "upstream/patches/codex/0040-tool-diagnostics-queue.patch",
    "upstream/patches/codex/0041-tool-diagnostics-responses.patch",
    "upstream/patches/codex/0042-tool-diagnostics-queue-tests.patch",
    "upstream/patches/codex/0043-tool-diagnostics-detector.patch",
    "upstream/patches/codex/0044-tool-diagnostics-detector-tests.patch",
    "upstream/patches/codex/0045-tool-diagnostics-feedback.patch",
    "upstream/patches/codex/0046-tool-diagnostics-boundaries.patch",
    "upstream/patches/codex/0047-tool-dependencies-schema.patch",
    "upstream/patches/codex/0048-tool-dependencies-input-store.patch",
    "upstream/patches/codex/0049-tool-dependencies-input-tests.patch",
    "upstream/patches/codex/0050-tool-dependencies-sharing-store.patch",
    "upstream/patches/codex/0051-tool-dependencies-sharing-store-tests.patch",
    "upstream/patches/codex/0052-tool-dependencies-adapter-contract.patch",
    "upstream/patches/codex/0053-tool-dependencies-dispatch.patch",
    "upstream/patches/codex/0054-tool-dependencies-sharing-runtime.patch",
    "upstream/patches/codex/0055-tool-dependencies-sharing-io.patch",
    "upstream/patches/codex/0056-tool-dependencies-sharing-tests.patch",
    "upstream/patches/codex/0057-tool-dependencies-responses.patch",
    "upstream/patches/codex/0058-tool-dependencies-diagnostics.patch",
    "upstream/patches/codex/0059-tool-receipts-state.patch",
    "upstream/patches/codex/0060-tool-receipts-state-tests.patch",
    "upstream/patches/codex/0061-tool-receipts-context.patch",
    "upstream/patches/codex/0062-tool-receipts-boundaries.patch",
    "upstream/patches/codex/0063-tool-receipts-test-isolation.patch",
    "upstream/patches/codex/0064-tool-receipts-interrupted-history.patch",
    "upstream/patches/codex/0065-tool-receipts-workspace-v8-link.patch",
    "upstream/patches/codex/0066-tool-receipts-integration-feedback.patch",
    "upstream/patches/codex/0067-tool-receipts-concurrent-writes.patch",
    "upstream/patches/codex/0068-tool-receipts-surface-fixtures.patch",
    "upstream/patches/codex/0069-tool-receipts-portable-fixtures.patch",
    "upstream/patches/codex/0070-tool-receipts-stream-dispatch.patch",
    "upstream/patches/codex/0071-tool-review-stream-delivery.patch",
    "upstream/patches/codex/0072-tool-review-progress-isolation.patch",
    "upstream/patches/codex/0073-tool-review-sharing-settlement.patch",
    "upstream/patches/codex/0074-tool-review-sharing-cancellation.patch",
    "upstream/patches/codex/0075-tool-review-diagnostic-evidence.patch",
    "upstream/patches/codex/0076-tool-review-tls.patch",
    "upstream/patches/codex/0077-tool-review-memmap.patch",
    "upstream/patches/codex/0078-tool-review-diagnostic-offer-regressions.patch",
    "upstream/patches/codex/0079-tool-review-optional-diagnostic-isolation.patch",
    "upstream/patches/codex/0080-tool-review-diagnostic-receipt-recovery.patch",
    "upstream/patches/codex/0081-tool-review-typed-handoff-replay.patch",
    "upstream/patches/codex/0082-tool-review-fixture-resource-lifetime.patch",
    "upstream/patches/codex/0083-tool-review-tool-fact-shutdown-regressions.patch",
    "upstream/patches/codex/0084-tool-review-tool-fact-shutdown-barrier.patch",
    "upstream/patches/codex/0085-tool-review-app-client-timeout-phases.patch",
    "upstream/patches/codex/0086-tool-review-completion-wait-state.patch",
    "upstream/patches/codex/0087-tool-review-agent-completion-graph.patch",
    "upstream/patches/codex/0088-tool-review-agent-completion-runtime.patch",
    "upstream/patches/codex/0089-tool-review-shell-telemetry-outcomes.patch",
    "upstream/patches/codex/0090-tool-review-wait-agent-tree-fixtures.patch",
    "upstream/patches/codex/0091-tool-review-exec-server-resume-diagnostics.patch",
    "upstream/patches/codex/0092-tool-review-rollout-budget-diagnostics.patch",
    "upstream/patches/codex/0093-recover-original-turn-start-options.patch",
    "upstream/patches/codex/0094-export-mcp-server-transport-config.patch",
    "upstream/patches/codex/0095-tool-payload-mismatch-model-feedback.patch",
    "upstream/patches/codex/0096-seatbelt-explicit-literal-deny.patch",
    "upstream/patches/codex/0097-zsh-parent-approval-context.patch",
    "upstream/patches/codex/0098-mechanism-behavior-audit.patch",
    "upstream/patches/codex/0099-task-state-compaction-budget.patch",
    "upstream/patches/codex/0100-runtime-timer-and-pending-cost.patch",
    "upstream/patches/codex/0101-stdio-lifecycle-deadlines.patch",
    "upstream/patches/codex/0102-exact-input-call-routing.patch",
    "upstream/patches/codex/0103-private-input-recovery-store.patch",
    "upstream/patches/codex/0104-input-recovery-dispatch-and-pages.patch",
    "upstream/patches/codex/0105-host-model-retry-semantics.patch",
    "upstream/patches/codex/0106-required-tool-fact-storage-fatal.patch",
    "upstream/patches/codex/0107-terminal-core-error-rollout.patch",
    "upstream/patches/codex/0108-mcp-timeout-and-test-runtime.patch",
    "upstream/patches/codex/0109-execution-terminal-cause-and-drain.patch",
    "upstream/patches/codex/0110-model-authority-expiry-cause.patch",
    "upstream/patches/codex/0111-tool-call-request-result-pairs.patch",
    "upstream/patches/codex/0112-tool-review-handoff-receipt-order.patch",
    "upstream/patches/codex/0113-tool-review-shutdown-owner-release.patch",
    "upstream/patches/codex/0114-tool-review-call-pair-regressions.patch",
    "upstream/patches/codex/0115-app-server-internal-error-schema.patch",
    "upstream/patches/codex/0116-tool-review-output-result-reference.patch",
    "upstream/patches/codex/0117-shared-tool-fatal-storage.patch",
    "upstream/patches/codex/0118-guardian-transcript-test-registration.patch",
    "upstream/patches/codex/0119-native-fixture-transport-lifecycle.patch",
    "upstream/patches/codex/0120-native-fixture-network-isolation.patch",
    "upstream/patches/codex/0121-tool-review-supervised-process-shutdown.patch",
];

const ROLE_SESSION_POLICY_SCHEMA_VERSION: u32 = 2;

const DEFAULT_EVENT_CAPACITY: usize = 256;
const MIN_EVENT_CAPACITY: usize = 16;
const MAX_EVENT_CAPACITY: usize = 4096;
const DEFAULT_SHUTDOWN_TIMEOUT_MILLIS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDescriptor {
    pub name: &'static str,
    pub execution_authorities: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelBuildInfo {
    pub interface_version: u32,
    pub codex_tag: &'static str,
    pub codex_commit: &'static str,
    pub patch_set: Vec<&'static str>,
    pub event_capacity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelOptions {
    pub home: PathBuf,
    pub helper_executable: PathBuf,
    pub event_capacity: usize,
    pub shutdown_timeout: Duration,
    pub linux_sandbox_executable: Option<PathBuf>,
}

impl KernelOptions {
    #[must_use]
    pub fn new(home: PathBuf, helper_executable: PathBuf) -> Self {
        let linux_sandbox_executable = cfg!(target_os = "linux").then(|| helper_executable.clone());
        Self {
            home,
            helper_executable,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            shutdown_timeout: Duration::from_millis(DEFAULT_SHUTDOWN_TIMEOUT_MILLIS),
            linux_sandbox_executable,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub role_policy: Option<RoleSessionPolicy>,
    pub agent_config: AgentSessionConfigSnapshot,
}

pub fn parse_role_session_policy(value: &str) -> KernelResult<RoleSessionPolicy> {
    let policy: RoleSessionPolicy = serde_json::from_str(value).map_err(|error| {
        KernelFailure::new(
            "INVALID_ROLE_POLICY",
            format!("StrongFlow role policy is invalid: {error}"),
        )
    })?;
    if policy.schema_version != i64::from(ROLE_SESSION_POLICY_SCHEMA_VERSION) {
        return Err(KernelFailure::new(
            "INVALID_ROLE_POLICY",
            "StrongFlow role policy schema version is unsupported",
        ));
    }
    Ok(policy)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOptions {
    pub cwd: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    Exec,
    Patch,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    ApprovedForSession,
    Denied { rejection: String },
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResponse {
    pub session_id: String,
    pub kind: ApprovalKind,
    pub operation_id: String,
    pub turn_id: Option<String>,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub rollout_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelEvent {
    pub sequence: u64,
    pub kind: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPoll {
    Event(KernelEvent),
    Timeout,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionInfo {
    pub status: &'static str,
    pub turn_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSubmissionOptions {
    pub image_urls: Vec<String>,
    pub final_output_json_schema: Option<Value>,
    pub submit_change_batch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactTurnReconciliation {
    Started { turn_id: String, recovered: bool },
    NotSubmitted { reason: String },
    Completed(DurableTurnTerminal),
    Failed(DurableTurnTerminal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownInfo {
    pub completed: Vec<String>,
    pub submit_failed: Vec<String>,
    pub timed_out: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionEnforcementFailureKind {
    Unavailable,
    StaleAuthority,
    InvalidAction,
    UnknownCapability,
    UnknownReceipt,
    Rejected,
    Consumed,
    Conflict,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelFailure {
    code: &'static str,
    message: String,
}

impl KernelFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn action_rejected() -> Self {
        Self::action_enforcement(ActionEnforcementFailureKind::Rejected)
    }

    /// Preserves a finite host authority failure while every tool path fails closed.
    #[must_use]
    pub fn action_enforcement(kind: ActionEnforcementFailureKind) -> Self {
        let code = match kind {
            ActionEnforcementFailureKind::Unavailable => "ACTION_ENFORCEMENT_UNAVAILABLE",
            ActionEnforcementFailureKind::StaleAuthority => "ACTION_AUTHORITY_STALE",
            ActionEnforcementFailureKind::InvalidAction => "ACTION_REQUEST_INVALID",
            ActionEnforcementFailureKind::UnknownCapability => "ACTION_CAPABILITY_UNKNOWN",
            ActionEnforcementFailureKind::UnknownReceipt => "ACTION_RECEIPT_UNKNOWN",
            ActionEnforcementFailureKind::Rejected => "ACTION_ENFORCEMENT_REJECTED",
            ActionEnforcementFailureKind::Consumed => "ACTION_RECEIPT_CONSUMED",
            ActionEnforcementFailureKind::Conflict => "ACTION_IDENTITY_CONFLICT",
            ActionEnforcementFailureKind::Cancelled => "ACTION_CANCELLED",
        };
        Self::new(
            code,
            "Control Plane action enforcement stopped the tool call",
        )
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for KernelFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for KernelFailure {}

pub type KernelResult<T> = Result<T, KernelFailure>;

/// Closes abandoned tool calls through the same Core state authority. Live
/// threads are protected by process-held OS locks and are skipped.
/// Returns a storage failure when pending calls cannot be closed durably.
pub async fn recover_interrupted_tool_calls(kernel_home: &Path) -> KernelResult<u64> {
    let sqlite = codex_core_api::SqliteConfig::from_sqlite_home(
        AbsolutePathBuf::try_from(kernel_home.to_path_buf()).map_err(|error| {
            KernelFailure::new("KERNEL_RECOVERY_PATH_INVALID", error.to_string())
        })?,
    );
    if !sqlite.state_db_path().is_file() {
        return Ok(0);
    }
    let store = codex_core_api::StateRuntime::init(sqlite, "recovery".into())
        .await
        .map_err(|error| KernelFailure::new("KERNEL_RECOVERY_STORAGE_FAILED", error.to_string()))?;
    store
        .recover_abandoned_tool_calls()
        .await
        .map_err(|error| KernelFailure::new("KERNEL_RECOVERY_STORAGE_FAILED", error.to_string()))
}

struct SessionRuntime {
    thread: Arc<CodexThread>,
    config: Config,
    events: Mutex<mpsc::Receiver<KernelEvent>>,
    stop: watch::Sender<bool>,
    event_task: Mutex<Option<JoinHandle<()>>>,
}

struct Runtime {
    manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    base_config: Config,
    state_db: Option<StateDbHandle>,
    sessions: RwLock<HashMap<String, Arc<SessionRuntime>>>,
}

#[derive(Debug, Clone, Copy)]
struct CanonicalRolePolicy {
    workspace_mode: &'static str,
    workspace_write: bool,
}

fn canonical_role_policy(
    role_id: &RoleSessionPolicyRoleId,
    execution_mode: &RoleExecutionMode,
) -> CanonicalRolePolicy {
    match execution_mode {
        RoleExecutionMode::DebugProbe => {
            return CanonicalRolePolicy {
                workspace_mode: "candidate-read-only",
                workspace_write: false,
            };
        }
        RoleExecutionMode::React | RoleExecutionMode::DelegatedBatch => {}
    }
    let mut policy = match role_id {
        RoleSessionPolicyRoleId::Requirements
        | RoleSessionPolicyRoleId::Solution
        | RoleSessionPolicyRoleId::Planner => CanonicalRolePolicy {
            workspace_mode: "source-read-only",
            workspace_write: false,
        },
        RoleSessionPolicyRoleId::Executor | RoleSessionPolicyRoleId::Remediator => {
            CanonicalRolePolicy {
                workspace_mode: "candidate-write",
                workspace_write: true,
            }
        }
        RoleSessionPolicyRoleId::Reviewer
        | RoleSessionPolicyRoleId::Verifier
        | RoleSessionPolicyRoleId::AdversarialVerifier => CanonicalRolePolicy {
            workspace_mode: "candidate-read-only",
            workspace_write: false,
        },
    };
    if execution_mode == &RoleExecutionMode::DelegatedBatch
        && matches!(
            role_id,
            RoleSessionPolicyRoleId::Executor | RoleSessionPolicyRoleId::Remediator
        )
    {
        policy = CanonicalRolePolicy {
            workspace_mode: "candidate-read-only",
            workspace_write: false,
        };
    }
    policy
}

const fn role_id_name(role_id: &RoleSessionPolicyRoleId) -> &'static str {
    match role_id {
        RoleSessionPolicyRoleId::Requirements => "requirements",
        RoleSessionPolicyRoleId::Solution => "solution",
        RoleSessionPolicyRoleId::Planner => "planner",
        RoleSessionPolicyRoleId::Executor => "executor",
        RoleSessionPolicyRoleId::Reviewer => "reviewer",
        RoleSessionPolicyRoleId::Verifier => "verifier",
        RoleSessionPolicyRoleId::AdversarialVerifier => "adversarial-verifier",
        RoleSessionPolicyRoleId::Remediator => "remediator",
    }
}

const fn workspace_mode_name(mode: &RoleSessionPolicyWorkspaceMode) -> &'static str {
    match mode {
        RoleSessionPolicyWorkspaceMode::SourceReadOnly => "source-read-only",
        RoleSessionPolicyWorkspaceMode::CandidateReadOnly => "candidate-read-only",
        RoleSessionPolicyWorkspaceMode::CandidateWrite => "candidate-write",
    }
}

pub struct Kernel {
    options: KernelOptions,
    model_port: Arc<dyn ModelPort>,
    action_gate: Arc<dyn KernelActionGate>,
    host_action_approvals: bool,
    runtime: Arc<Mutex<Option<Arc<Runtime>>>>,
    closed: AtomicBool,
    #[cfg(target_os = "linux")]
    _linux_sandbox_alias: Option<tempfile::TempDir>,
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn install_linux_sandbox_alias(
    home: &Path,
    executable: &Path,
) -> KernelResult<(PathBuf, tempfile::TempDir)> {
    let install = || -> std::io::Result<_> {
        let executable = std::path::absolute(executable)?;
        let directory = tempfile::Builder::new()
            .prefix(".linux-sandbox-")
            .tempdir_in(home.canonicalize()?)?;
        let alias = directory.path().join("codex-linux-sandbox");
        std::os::unix::fs::symlink(executable, &alias)?;
        Ok((alias, directory))
    };
    install().map_err(|error| KernelFailure::new("LINUX_SANDBOX_ALIAS_FAILED", error.to_string()))
}

impl Kernel {
    pub fn new(
        mut options: KernelOptions,
        model_port: Arc<dyn ModelPort>,
        action_gate: Arc<dyn KernelActionGate>,
    ) -> KernelResult<Self> {
        options.event_capacity = options
            .event_capacity
            .clamp(MIN_EVENT_CAPACITY, MAX_EVENT_CAPACITY);
        if options.shutdown_timeout.is_zero() {
            return Err(KernelFailure::new(
                "INVALID_SHUTDOWN_TIMEOUT",
                "shutdown timeout must be greater than zero",
            ));
        }
        std::fs::create_dir_all(&options.home).map_err(|error| {
            KernelFailure::new(
                "HOME_CREATE_FAILED",
                format!("{}: {error}", options.home.display()),
            )
        })?;
        #[cfg(target_os = "linux")]
        let linux_sandbox_alias = options
            .linux_sandbox_executable
            .as_ref()
            .map(|executable| install_linux_sandbox_alias(&options.home, executable))
            .transpose()?;
        #[cfg(target_os = "linux")]
        if let Some((alias, _directory)) = &linux_sandbox_alias {
            options.linux_sandbox_executable = Some(alias.clone());
        }
        Ok(Self {
            options,
            model_port,
            action_gate,
            host_action_approvals: false,
            runtime: Arc::new(Mutex::new(None)),
            closed: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            _linux_sandbox_alias: linux_sandbox_alias.map(|(_alias, directory)| directory),
        })
    }

    #[must_use]
    pub fn with_host_action_approvals(mut self, enabled: bool) -> Self {
        self.host_action_approvals = enabled;
        self
    }

    #[must_use]
    pub fn build_info(&self) -> KernelBuildInfo {
        KernelBuildInfo {
            interface_version: INTERFACE_VERSION,
            codex_tag: CODEX_TAG,
            codex_commit: CODEX_COMMIT,
            patch_set: CODEX_PATCH_SET.to_vec(),
            event_capacity: u32::try_from(self.options.event_capacity).unwrap_or(u32::MAX),
        }
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn runtime(&self) -> BoxFuture<'_, KernelResult<Arc<Runtime>>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(KernelFailure::new(
                    "KERNEL_CLOSED",
                    "kernel has already shut down",
                ));
            }
            let mut runtime = self.runtime.lock().await;
            if self.is_closed() {
                return Err(KernelFailure::new(
                    "KERNEL_CLOSED",
                    "kernel has already shut down",
                ));
            }
            if let Some(runtime) = runtime.as_ref() {
                return Ok(Arc::clone(runtime));
            }
            let initialized = self.initialize_runtime().await?;
            *runtime = Some(Arc::clone(&initialized));
            if self.is_closed() {
                return Err(KernelFailure::new(
                    "KERNEL_CLOSED",
                    "kernel shut down during initialization",
                ));
            }
            Ok(initialized)
        })
    }

    #[allow(clippy::too_many_lines)]
    fn initialize_runtime(&self) -> BoxFuture<'_, KernelResult<Arc<Runtime>>> {
        Box::pin(async move {
            let mut config = ConfigBuilder::default()
                .codex_home(self.options.home.clone())
                .fallback_cwd(Some(self.options.home.clone()))
                .strict_config(true)
                .build()
                .await
                .map_err(|error| KernelFailure::new("CONFIG_LOAD_FAILED", error.to_string()))?;
            config.ephemeral = false;
            config.analytics_enabled = Some(false);
            config.feedback_enabled = false;
            config.check_for_update_on_startup = false;
            config.web_search_mode = Constrained::allow_any(WebSearchMode::Disabled);
            config
                .features
                .enable(Feature::DefaultModeRequestUserInput)
                .map_err(|error| {
                    KernelFailure::new(
                        "CONFIG_FEATURE_FAILED",
                        format!("request_user_input feature could not be enabled: {error}"),
                    )
                })?;
            config.experimental_request_user_input_enabled = true;
            config
                .features
                .enable(Feature::CodeModeOnly)
                .map_err(|error| {
                    KernelFailure::new(
                        "CONFIG_FEATURE_FAILED",
                        format!("Code Mode tool surface could not be enabled: {error}"),
                    )
                })?;
            config
                .features
                .enable(Feature::CodeModeInterrupt)
                .map_err(|error| {
                    KernelFailure::new(
                        "CONFIG_FEATURE_FAILED",
                        format!("Code Mode interruption could not be enabled: {error}"),
                    )
                })?;
            config.codex_self_exe = Some(self.options.helper_executable.clone());
            config
                .codex_linux_sandbox_exe
                .clone_from(&self.options.linux_sandbox_executable);

            let state_db = init_state_db(&config).await;
            let auth_manager = AuthManager::shared_from_config(&config, false)
                .await
                .map_err(|error| {
                    KernelFailure::new("AUTH_INITIALIZATION_FAILED", error.to_string())
                })?;
            let runtime_paths = ExecServerRuntimePaths::new(
                self.options.helper_executable.clone(),
                self.options.linux_sandbox_executable.clone(),
            )
            .map_err(|error| KernelFailure::new("RUNTIME_PATHS_INVALID", error.to_string()))?;
            let environment_manager = Arc::new(
                EnvironmentManager::from_codex_home(
                    config.codex_home.clone(),
                    Some(runtime_paths),
                    config.http_client_factory(),
                )
                .await
                .map_err(|error| {
                    KernelFailure::new("ENVIRONMENT_INITIALIZATION_FAILED", error.to_string())
                })?,
            );
            let thread_store = thread_store_from_config(&config, state_db.clone());
            let installation_id = resolve_installation_id(&config.codex_home)
                .await
                .map_err(|error| KernelFailure::new("INSTALLATION_ID_FAILED", error.to_string()))?;
            let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
                config.codex_home.clone(),
            ));
            let mut extensions = codex_core_api::ExtensionRegistryBuilder::new();
            codex_skills_extension::install(&mut extensions, |config: &Config| {
                codex_skills_extension::SkillsExtensionConfig {
                    include_instructions: config.include_skill_instructions,
                    max_context_tokens: config.skill_max_context_tokens,
                    bundled_skills_enabled: false,
                    orchestrator_skills_enabled: false,
                    shadow_selection_enabled: false,
                }
            });
            let manager = Arc::new(
                ThreadManager::new(
                    &config,
                    Arc::clone(&auth_manager),
                    build_models_manager(&config, Arc::clone(&auth_manager)),
                    CodexAppsToolsCache::default(),
                    SessionSource::Exec,
                    environment_manager,
                    extensions.build().into(),
                    user_instructions_provider,
                    None,
                    Arc::clone(&thread_store),
                    local_agent_graph_store_from_state_db(state_db.as_ref()),
                    installation_id,
                    None,
                    None,
                )
                .with_model_stream_transport(Arc::new(KernelModelStreamTransport::new(Arc::clone(
                    &self.model_port,
                ))))
                .with_code_mode_session_provider(Arc::new(
                    codex_code_mode::ProcessOwnedCodeModeSessionProvider::with_host_program(
                        self.options.helper_executable.clone(),
                    ),
                ))
                .with_tool_call_gate(Arc::new(CoreToolCallGate {
                    host: Arc::clone(&self.action_gate),
                })),
            );
            Ok(Arc::new(Runtime {
                manager,
                auth_manager,
                base_config: config,
                state_db,
                sessions: RwLock::new(HashMap::new()),
            }))
        })
    }

    async fn session_config(
        runtime: &Runtime,
        options: &SessionOptions,
        host_action_approvals: bool,
    ) -> KernelResult<Config> {
        validate_agent_session_config(&options.agent_config).map_err(|_| {
            KernelFailure::new(
                "INVALID_AGENT_CONFIG",
                "Agent session configuration snapshot is invalid",
            )
        })?;
        let settings = &options.agent_config.profile.source.settings;
        if settings.provider != options.provider || settings.model != options.model {
            return Err(KernelFailure::new(
                "INVALID_AGENT_CONFIG",
                "Agent profile does not match the selected model route",
            ));
        }
        let mut config = runtime.base_config.clone();
        Self::apply_task_state_compaction_policy(&mut config)?;
        let current = ConfigBuilder::default()
            .codex_home(config.codex_home.to_path_buf())
            .fallback_cwd(Some(config.codex_home.to_path_buf()))
            .strict_config(true)
            .build()
            .await
            .map_err(|_| {
                KernelFailure::new(
                    "EXTENSION_CONFIG_UNAVAILABLE",
                    "Device extension configuration could not be loaded",
                )
            })?;
        config.config_layer_stack = config
            .config_layer_stack
            .with_user_layer_from(&current.config_layer_stack);
        config.mcp_servers = current.mcp_servers;
        runtime.manager.skills_service().clear_cache();
        set_workspace(&mut config, &options.cwd)?;
        let mut servers = config.mcp_servers.get().clone();
        for server in servers.values_mut() {
            if let codex_core_api::McpServerTransportConfig::Stdio { cwd, .. } =
                &mut server.transport
                && cwd.is_none()
            {
                *cwd = Some(
                    serde_json::from_value(Value::String(
                        options.cwd.to_string_lossy().into_owned(),
                    ))
                    .map_err(|_| {
                        KernelFailure::new("INVALID_WORKSPACE", "Invalid MCP workspace")
                    })?,
                );
            }
        }
        config.mcp_servers.set(servers).map_err(|_| {
            KernelFailure::new(
                "EXTENSION_CONFIG_UNAVAILABLE",
                "MCP workspace configuration failed",
            )
        })?;

        let (provider, model) = model_route(&options.provider, &options.model)?;
        config.model_provider = kernel_provider_info(&provider);
        config.model_provider_id = provider;
        config.model = Some(model);
        config.model_reasoning_effort = if settings.reasoning == "provider_default" {
            None
        } else {
            Some(
                serde_json::from_value(Value::String(settings.reasoning.clone())).map_err(
                    |_| KernelFailure::new("INVALID_AGENT_CONFIG", "Invalid reasoning effort"),
                )?,
            )
        };
        if let Some(policy) = &options.role_policy {
            if options.agent_config.identity.role != role_id_name(&policy.role_id)
                || settings.sandbox != workspace_mode_name(&policy.workspace_mode)
                || settings.instructions.as_deref() != Some(&policy.developer_instructions)
            {
                return Err(KernelFailure::new(
                    "INVALID_AGENT_CONFIG",
                    "Agent profile does not match the role policy",
                ));
            }
            Self::apply_role_session_policy(&mut config, policy)?;
        } else {
            let workspace_write = match settings.sandbox.as_str() {
                "candidate" => true,
                "read-only" => false,
                _ => {
                    return Err(KernelFailure::new(
                        "INVALID_AGENT_CONFIG",
                        "Chat profile has an unsupported workspace mode",
                    ));
                }
            };
            Self::apply_workspace_permissions(&mut config, workspace_write)?;
        }
        if host_action_approvals {
            Self::apply_host_action_approvals(&mut config)?;
        }
        Ok(config)
    }

    fn apply_task_state_compaction_policy(config: &mut Config) -> KernelResult<()> {
        config.features.disable(Feature::TokenBudget).map_err(|_| {
            KernelFailure::new(
                "SESSION_POLICY_UNAVAILABLE",
                "Task-state compaction policy could not be applied",
            )
        })?;
        config.compact_prompt =
            Some(winwincode_execution_port::task_handoff::TASK_HANDOFF_COMPACT_PROMPT.to_owned());
        config.compact_context_max_tokens =
            Some(winwincode_execution_port::task_handoff::TASK_HANDOFF_CONTEXT_MAX_TOKENS);
        Ok(())
    }

    fn apply_host_action_approvals(config: &mut Config) -> KernelResult<()> {
        let mut servers = config.mcp_servers.get().clone();
        for server in servers.values_mut() {
            let mode = serde_json::to_value(server.default_tools_approval_mode).map_err(|_| {
                KernelFailure::new(
                    "SESSION_POLICY_UNAVAILABLE",
                    "Tool approval policy is invalid",
                )
            })?;
            if mode.is_null() || mode == Value::String("auto".to_owned()) {
                server.default_tools_approval_mode = Some(
                    serde_json::from_value(Value::String("approve".to_owned())).map_err(|_| {
                        KernelFailure::new(
                            "SESSION_POLICY_UNAVAILABLE",
                            "Host tool approval policy is invalid",
                        )
                    })?,
                );
            }
        }
        config.mcp_servers.set(servers).map_err(|_| {
            KernelFailure::new(
                "SESSION_POLICY_UNAVAILABLE",
                "Host tool approval policy could not be installed",
            )
        })
    }

    fn validate_role_session_policy(
        policy: &RoleSessionPolicy,
    ) -> KernelResult<CanonicalRolePolicy> {
        if policy.schema_version != i64::from(ROLE_SESSION_POLICY_SCHEMA_VERSION) {
            return Err(KernelFailure::new(
                "INVALID_ROLE_POLICY",
                "StrongFlow role policy schema version is unsupported",
            ));
        }
        let canonical = canonical_role_policy(&policy.role_id, &policy.execution_mode);
        if workspace_mode_name(&policy.workspace_mode) != canonical.workspace_mode {
            return Err(KernelFailure::new(
                "INVALID_ROLE_POLICY",
                format!(
                    "role {} does not match its canonical workspace mode",
                    role_id_name(&policy.role_id)
                ),
            ));
        }
        if policy.developer_instructions.trim().is_empty() {
            return Err(KernelFailure::new(
                "INVALID_ROLE_POLICY",
                "StrongFlow role developer instructions must be non-empty",
            ));
        }
        Ok(canonical)
    }

    fn apply_role_session_policy(
        config: &mut Config,
        policy: &RoleSessionPolicy,
    ) -> KernelResult<()> {
        let canonical = Self::validate_role_session_policy(policy)?;
        Self::apply_workspace_permissions(config, canonical.workspace_write)?;
        config.developer_instructions = Some(policy.developer_instructions.clone());
        Ok(())
    }

    fn apply_workspace_permissions(config: &mut Config, workspace_write: bool) -> KernelResult<()> {
        let permission_profile = if workspace_write {
            PermissionProfile::workspace_write()
        } else {
            PermissionProfile::read_only()
        };
        let mut permissions = Permissions::from_approval_and_profile(
            Constrained::allow_only(if workspace_write {
                AskForApproval::OnRequest
            } else {
                AskForApproval::Never
            }),
            Constrained::allow_only(permission_profile),
        )
        .map_err(|error| KernelFailure::new("SESSION_POLICY_UNAVAILABLE", error.to_string()))?;
        permissions.set_workspace_roots(vec![config.cwd.clone()]);
        config.permissions = permissions;
        config.explicit_permission_profile_mode = true;
        config.approvals_reviewer = ApprovalsReviewer::User;
        Ok(())
    }

    pub async fn create_session(&self, options: SessionOptions) -> KernelResult<SessionInfo> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let config =
                Self::session_config(&runtime, &options, self.host_action_approvals).await?;
            let thread = runtime
                .manager
                .start_thread(StartThreadOptions::new(config.clone()))
                .await
                .map_err(|error| KernelFailure::new("SESSION_CREATE_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    pub async fn resume_session(
        &self,
        rollout_path: PathBuf,
        options: SessionOptions,
    ) -> KernelResult<SessionInfo> {
        Self::guard(async {
            if !rollout_path.is_absolute() {
                return Err(KernelFailure::new(
                    "INVALID_ROLLOUT_PATH",
                    format!("rollout path must be absolute: {}", rollout_path.display()),
                ));
            }
            let runtime = self.runtime().await?;
            let config =
                Self::session_config(&runtime, &options, self.host_action_approvals).await?;
            let thread = Box::pin(runtime.manager.resume_thread_from_rollout(
                config.clone(),
                rollout_path,
                Arc::clone(&runtime.auth_manager),
                None,
                ClientMcpExtensions::default(),
            ))
            .await
            .map_err(|error| KernelFailure::new("SESSION_RESUME_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    pub async fn fork_session(
        &self,
        source_session_id: &str,
        options: ForkOptions,
    ) -> KernelResult<SessionInfo> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let source = self.session(&runtime, source_session_id).await?;
            source.thread.ensure_rollout_materialized().await;
            source
                .thread
                .flush_rollout()
                .await
                .map_err(|error| KernelFailure::new("SESSION_FLUSH_FAILED", error.to_string()))?;
            let rollout_path = source.thread.rollout_path().ok_or_else(|| {
                KernelFailure::new(
                    "ROLLOUT_UNAVAILABLE",
                    format!("session {source_session_id} has no durable rollout"),
                )
            })?;
            let mut config = source.config.clone();
            if let Some(cwd) = options.cwd {
                set_workspace(&mut config, &cwd)?;
            }
            match (options.provider, options.model) {
                (Some(provider), Some(model)) => {
                    let (provider, model) = model_route(&provider, &model)?;
                    config.model_provider = kernel_provider_info(&provider);
                    config.model_provider_id = provider;
                    config.model = Some(model);
                }
                (None, None) => {}
                _ => {
                    return Err(KernelFailure::new(
                        "INVALID_MODEL_ROUTE",
                        "fork provider and model must be supplied together",
                    ));
                }
            }
            let thread = Box::pin(runtime.manager.fork_thread(
                ForkSnapshot::Interrupted,
                config.clone(),
                rollout_path,
                None,
                None,
            ))
            .await
            .map_err(|error| KernelFailure::new("SESSION_FORK_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    pub async fn submit_turn(
        &self,
        session_id: &str,
        text: String,
        options: TurnSubmissionOptions,
    ) -> KernelResult<SubmissionInfo> {
        Self::guard(async {
            if text.trim().is_empty() {
                return Err(KernelFailure::new(
                    "EMPTY_INPUT",
                    "turn input must contain text",
                ));
            }
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            let submission = session
                .thread
                .start_or_steer_turn(user_text_request(text, &options))
                .await
                .map_err(|error| KernelFailure::new("TURN_SUBMIT_FAILED", error.to_string()))?;
            Ok(submission_info(submission))
        })
        .await
    }

    pub async fn reconcile_turn_exact(
        &self,
        session_id: &str,
        turn_id: String,
        text: String,
        options: TurnSubmissionOptions,
    ) -> KernelResult<ExactTurnReconciliation> {
        Self::guard(async {
            if turn_id.trim().is_empty() || text.trim().is_empty() {
                return Err(KernelFailure::new(
                    "INVALID_EXACT_TURN",
                    "exact turn identity and input must be non-empty",
                ));
            }
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            let durable = session
                .thread
                .inspect_durable_turn(&turn_id)
                .await
                .map_err(|_| {
                    KernelFailure::new(
                        "TURN_HISTORY_LOOKUP_FAILED",
                        "durable turn history lookup failed",
                    )
                })?;
            match durable {
                DurableTurnInspection::Completed(terminal) => {
                    Ok(ExactTurnReconciliation::Completed(terminal))
                }
                DurableTurnInspection::Failed(terminal) => {
                    Ok(ExactTurnReconciliation::Failed(terminal))
                }
                DurableTurnInspection::Absent | DurableTurnInspection::InProgress => {
                    let recovered = matches!(durable, DurableTurnInspection::InProgress);
                    let submission = if recovered {
                        session
                            .thread
                            .recover_turn_if_idle(RecoverTurnRequest {
                                turn_id,
                                thread_settings: ThreadSettingsOverrides::default(),
                                trace: None,
                                start: turn_start_options(&options),
                            })
                            .await
                    } else {
                        session
                            .thread
                            .start_turn_with_id_if_idle(user_text_request(text, &options), turn_id)
                            .await
                    }
                    .map_err(|_| {
                        KernelFailure::new(
                            "TURN_RECONCILE_SUBMIT_FAILED",
                            "exact turn reconciliation submission failed",
                        )
                    })?;
                    Ok(exact_turn_submission(submission, recovered))
                }
            }
        })
        .await
    }

    pub async fn steer(
        &self,
        session_id: &str,
        expected_turn_id: String,
        text: String,
        options: TurnSubmissionOptions,
    ) -> KernelResult<SubmissionInfo> {
        Self::guard(async {
            if text.trim().is_empty() {
                return Err(KernelFailure::new(
                    "EMPTY_INPUT",
                    "steering input must contain text",
                ));
            }
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            let submission = session
                .thread
                .steer_turn(user_text_request(text, &options), expected_turn_id)
                .await
                .map_err(|error| KernelFailure::new("TURN_STEER_FAILED", error.to_string()))?;
            Ok(steer_info(submission))
        })
        .await
    }

    pub async fn interrupt(&self, session_id: &str) -> KernelResult<String> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            session
                .thread
                .submit(Op::Interrupt)
                .await
                .map_err(|error| KernelFailure::new("INTERRUPT_FAILED", error.to_string()))
        })
        .await
    }

    pub async fn resolve_approval(&self, response: ApprovalResponse) -> KernelResult<String> {
        Self::guard(async {
            let session_id = response.session_id.clone();
            let operation = codex_approval_operation(response)?;
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, &session_id).await?;
            session
                .thread
                .submit(operation)
                .await
                .map_err(|error| KernelFailure::new("APPROVAL_SUBMIT_FAILED", error.to_string()))
        })
        .await
    }

    pub async fn resolve_user_input(
        &self,
        session_id: &str,
        turn_id: String,
        call_id: String,
        response: RequestUserInputResponse,
    ) -> KernelResult<String> {
        Self::guard(async {
            if session_id.trim().is_empty()
                || turn_id.trim().is_empty()
                || call_id.trim().is_empty()
            {
                return Err(KernelFailure::new(
                    "INVALID_INPUT_RESPONSE",
                    "input response session, turn and call identities must be non-empty",
                ));
            }
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            session
                .thread
                .submit(Op::UserInputAnswer {
                    id: turn_id,
                    call_id,
                    response,
                })
                .await
                .map_err(|_| {
                    KernelFailure::new(
                        "INPUT_RESPONSE_SUBMIT_FAILED",
                        "embedded Codex input response submission failed",
                    )
                })
        })
        .await
    }

    pub async fn next_event(
        &self,
        session_id: &str,
        timeout: Option<Duration>,
    ) -> KernelResult<EventPoll> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let Some(session) = runtime.sessions.read().await.get(session_id).cloned() else {
                return Ok(EventPoll::Closed);
            };
            let mut events = session.events.lock().await;
            let result = match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, events.recv()).await {
                    Ok(Some(event)) => Ok(EventPoll::Event(event)),
                    Ok(None) => Ok(EventPoll::Closed),
                    Err(_) => Ok(EventPoll::Timeout),
                },
                None => Ok(match events.recv().await {
                    Some(event) => EventPoll::Event(event),
                    None => EventPoll::Closed,
                }),
            };
            drop(events);
            result
        })
        .await
    }

    pub async fn list_sessions(&self) -> KernelResult<Vec<String>> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let mut sessions = runtime
                .sessions
                .read()
                .await
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            sessions.sort();
            Ok(sessions)
        })
        .await
    }

    pub async fn close_session(&self, session_id: &str) -> KernelResult<()> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = runtime
                .sessions
                .read()
                .await
                .get(session_id)
                .cloned()
                .ok_or_else(|| session_not_found(session_id))?;
            let _ = session.stop.send(true);
            tokio::time::timeout(
                self.options.shutdown_timeout,
                session.thread.shutdown_and_wait(),
            )
            .await
            .map_err(|_| {
                KernelFailure::new(
                    "SESSION_SHUTDOWN_TIMEOUT",
                    format!("session {session_id} exceeded shutdown timeout"),
                )
            })?
            .map_err(|error| KernelFailure::new("SESSION_SHUTDOWN_FAILED", error.to_string()))?;
            runtime.sessions.write().await.remove(session_id);
            if let Ok(thread_id) = ThreadId::try_from(session_id) {
                let _ = runtime.manager.remove_thread(&thread_id).await;
            }
            join_event_task(&session).await;
            Ok(())
        })
        .await
    }

    pub async fn shutdown(&self) -> KernelResult<ShutdownInfo> {
        Self::guard(async {
            if self.closed.swap(true, Ordering::AcqRel) {
                return Ok(ShutdownInfo {
                    completed: Vec::new(),
                    submit_failed: Vec::new(),
                    timed_out: Vec::new(),
                });
            }
            let Some(runtime) = self.runtime.lock().await.take() else {
                return Ok(ShutdownInfo {
                    completed: Vec::new(),
                    submit_failed: Vec::new(),
                    timed_out: Vec::new(),
                });
            };
            let sessions = runtime
                .sessions
                .write()
                .await
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>();
            for session in &sessions {
                let _ = session.stop.send(true);
            }
            let report = runtime
                .manager
                .shutdown_all_threads_bounded(self.options.shutdown_timeout)
                .await;
            for session in &sessions {
                join_event_task(session).await;
            }
            if let Some(state_db) = &runtime.state_db {
                state_db.close().await;
            }
            Ok(ShutdownInfo {
                completed: report
                    .completed
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect(),
                submit_failed: report
                    .submit_failed
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect(),
                timed_out: report
                    .timed_out
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect(),
            })
        })
        .await
    }

    async fn register_session(
        &self,
        runtime: &Runtime,
        new_thread: NewThread,
        config: Config,
    ) -> KernelResult<SessionInfo> {
        let session_id = new_thread.thread_id.to_string();
        let rollout_path = new_thread
            .thread
            .rollout_path()
            .map(|path| path.to_string_lossy().into_owned());
        let (event_tx, event_rx) = mpsc::channel(self.options.event_capacity);
        let (stop, stop_rx) = watch::channel(false);
        let thread = Arc::clone(&new_thread.thread);
        let configured = CodexEvent {
            id: "winwincode-session-configured".to_string(),
            msg: CodexEventMsg::SessionConfigured(thread.session_configured()),
        };
        event_tx
            .send(serialize_codex_event(1, &configured))
            .await
            .map_err(|_| {
                KernelFailure::new(
                    "SESSION_EVENT_STREAM_FAILED",
                    "session configuration event could not be queued",
                )
            })?;
        let event_task = tokio::spawn(pump_events(thread, event_tx, stop_rx, 1));
        let session = Arc::new(SessionRuntime {
            thread: new_thread.thread,
            config,
            events: Mutex::new(event_rx),
            stop,
            event_task: Mutex::new(Some(event_task)),
        });
        let previous = runtime
            .sessions
            .write()
            .await
            .insert(session_id.clone(), session);
        if previous.is_some() {
            return Err(KernelFailure::new(
                "SESSION_ALREADY_REGISTERED",
                format!("session {session_id} is already live"),
            ));
        }
        Ok(SessionInfo {
            session_id,
            rollout_path,
        })
    }

    async fn session(
        &self,
        runtime: &Runtime,
        session_id: &str,
    ) -> KernelResult<Arc<SessionRuntime>> {
        runtime
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| session_not_found(session_id))
    }

    fn guard<'a, T>(
        future: impl Future<Output = KernelResult<T>> + Send + 'a,
    ) -> BoxFuture<'a, KernelResult<T>>
    where
        T: Send + 'a,
    {
        Box::pin(async move {
            std::panic::AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .map_err(|payload| {
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("embedded kernel panicked");
                    KernelFailure::new("KERNEL_PANIC", message)
                })?
        })
    }

    #[must_use]
    pub fn best_effort_shutdown(&self) -> Option<BoxFuture<'static, ()>> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let runtime = Arc::clone(&self.runtime);
        let timeout = self.options.shutdown_timeout;
        Some(Box::pin(async move {
            let Some(runtime) = runtime.lock().await.take() else {
                return;
            };
            let sessions = runtime
                .sessions
                .write()
                .await
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>();
            for session in &sessions {
                let _ = session.stop.send(true);
            }
            let _ = runtime.manager.shutdown_all_threads_bounded(timeout).await;
            for session in &sessions {
                join_event_task(session).await;
            }
            if let Some(state_db) = &runtime.state_db {
                state_db.close().await;
            }
        }))
    }
}

#[must_use]
pub const fn descriptor() -> KernelDescriptor {
    KernelDescriptor {
        name: "codex-core",
        execution_authorities: 1,
    }
}

fn user_text_request(text: String, options: &TurnSubmissionOptions) -> TurnInputRequest {
    let mut input = vec![UserInput::Text {
        text,
        text_elements: Vec::new(),
    }];
    input.extend(options.image_urls.iter().map(|url| UserInput::Image {
        image_url: url.clone(),
        detail: None,
    }));
    let request = TurnInputRequest::user_input(input).on_start(turn_start_options(options));
    #[cfg(feature = "test-support")]
    let request = mechanism_interaction_test_options::apply(request);
    request
}

fn turn_start_options(options: &TurnSubmissionOptions) -> TurnStartOptions {
    TurnStartOptions {
        final_output_json_schema: options.final_output_json_schema.clone(),
        submit_change_batch: options.submit_change_batch,
        ..TurnStartOptions::default()
    }
}

fn submission_info(submission: TurnInputSubmission) -> SubmissionInfo {
    match submission {
        TurnInputSubmission::Started { turn_id } => SubmissionInfo {
            status: "started",
            turn_id: Some(turn_id),
            reason: None,
        },
        TurnInputSubmission::Steered { turn_id } => SubmissionInfo {
            status: "steered",
            turn_id: Some(turn_id),
            reason: None,
        },
        TurnInputSubmission::NotSubmitted { reason } => SubmissionInfo {
            status: "not_submitted",
            turn_id: None,
            reason: Some(format!("{reason:?}")),
        },
    }
}

fn exact_turn_submission(
    submission: StartIfIdleSubmission,
    recovered: bool,
) -> ExactTurnReconciliation {
    match submission {
        StartIfIdleSubmission::Started { turn_id } => {
            ExactTurnReconciliation::Started { turn_id, recovered }
        }
        StartIfIdleSubmission::NotSubmitted { reason } => ExactTurnReconciliation::NotSubmitted {
            reason: format!("{reason:?}"),
        },
    }
}

fn steer_info(submission: SteerSubmission) -> SubmissionInfo {
    match submission {
        SteerSubmission::Steered { turn_id } => SubmissionInfo {
            status: "steered",
            turn_id: Some(turn_id),
            reason: None,
        },
        SteerSubmission::NotSubmitted { reason } => SubmissionInfo {
            status: "not_submitted",
            turn_id: None,
            reason: Some(format!("{reason:?}")),
        },
    }
}

fn session_not_found(session_id: &str) -> KernelFailure {
    KernelFailure::new(
        "SESSION_NOT_FOUND",
        format!("session {session_id} is not registered"),
    )
}

fn set_workspace(config: &mut Config, workspace: &Path) -> KernelResult<()> {
    if !workspace.is_absolute() {
        return Err(KernelFailure::new(
            "INVALID_WORKSPACE",
            format!("workspace must be absolute: {}", workspace.display()),
        ));
    }
    let canonical = std::fs::canonicalize(workspace).map_err(|error| {
        KernelFailure::new(
            "INVALID_WORKSPACE",
            format!(
                "workspace cannot be resolved {}: {error}",
                workspace.display()
            ),
        )
    })?;
    if !canonical.is_dir() {
        return Err(KernelFailure::new(
            "INVALID_WORKSPACE",
            format!("workspace is not a directory: {}", workspace.display()),
        ));
    }
    let cwd = AbsolutePathBuf::from_absolute_path_checked(canonical)
        .map_err(|error| KernelFailure::new("INVALID_WORKSPACE", error.to_string()))?;
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd.clone()];
    config.workspace_roots_explicit = true;
    config.permissions.set_workspace_roots(vec![cwd]);
    Ok(())
}

fn model_route(provider: &str, model: &str) -> KernelResult<(String, String)> {
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return Err(KernelFailure::new(
            "INVALID_MODEL_ROUTE",
            "provider and model must both be non-empty",
        ));
    }
    Ok((provider.to_string(), model.to_string()))
}

fn codex_approval_operation(response: ApprovalResponse) -> KernelResult<Op> {
    if response.operation_id.trim().is_empty() {
        return Err(KernelFailure::new(
            "INVALID_APPROVAL_RESPONSE",
            "approval id must be non-empty",
        ));
    }
    if response.kind == ApprovalKind::Mcp {
        use codex_protocol::approvals::ElicitationAction;
        let (server_name, request_id): (String, codex_protocol::mcp::RequestId) =
            serde_json::from_str(&response.operation_id).map_err(|_| {
                KernelFailure::new("INVALID_APPROVAL_RESPONSE", "invalid MCP callback identity")
            })?;
        if server_name.trim().is_empty() {
            return Err(KernelFailure::new(
                "INVALID_APPROVAL_RESPONSE",
                "MCP server is missing",
            ));
        }
        let decision = match response.decision {
            ApprovalDecision::Approved => ElicitationAction::Accept,
            ApprovalDecision::Denied { .. } => ElicitationAction::Decline,
            ApprovalDecision::Abort => ElicitationAction::Cancel,
            ApprovalDecision::ApprovedForSession => {
                return Err(KernelFailure::new(
                    "INVALID_APPROVAL_RESPONSE",
                    "MCP approvals must be single-use",
                ));
            }
        };
        return Ok(Op::ResolveElicitation {
            server_name,
            request_id,
            decision,
            content: None,
            meta: None,
        });
    }
    let decision = codex_review_decision(response.decision)?;
    match response.kind {
        ApprovalKind::Exec => Ok(Op::ExecApproval {
            id: response.operation_id,
            turn_id: response.turn_id.filter(|id| !id.trim().is_empty()),
            decision,
        }),
        ApprovalKind::Patch => Ok(Op::PatchApproval {
            id: response.operation_id,
            decision,
        }),
        ApprovalKind::Mcp => unreachable!("MCP callback handled above"),
    }
}

fn codex_review_decision(decision: ApprovalDecision) -> KernelResult<CodexReviewDecision> {
    match decision {
        ApprovalDecision::Approved => Ok(CodexReviewDecision::Approved),
        ApprovalDecision::ApprovedForSession => Ok(CodexReviewDecision::ApprovedForSession),
        ApprovalDecision::Denied { rejection } => {
            if rejection.trim().is_empty() {
                return Err(KernelFailure::new(
                    "INVALID_APPROVAL_RESPONSE",
                    "denied approval must include a rejection reason",
                ));
            }
            Ok(CodexReviewDecision::Denied { rejection })
        }
        ApprovalDecision::Abort => Ok(CodexReviewDecision::Abort),
    }
}

fn kernel_provider_info(provider: &str) -> ModelProviderInfo {
    ModelProviderInfo {
        name: format!("WinWinCode Kernel route {provider}"),
        ..ModelProviderInfo::default()
    }
}

async fn pump_events(
    thread: Arc<CodexThread>,
    sender: mpsc::Sender<KernelEvent>,
    mut stop: watch::Receiver<bool>,
    initial_sequence: u64,
) {
    let mut sequence = initial_sequence;
    loop {
        let next = tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }
            event = thread.next_event() => event,
        };
        let kernel_event = match next {
            Ok(event) => {
                sequence = sequence.saturating_add(1);
                serialize_codex_event(sequence, &event)
            }
            Err(error) => {
                sequence = sequence.saturating_add(1);
                KernelEvent {
                    sequence,
                    kind: "stream_error".to_string(),
                    payload_json: json!({
                        "type": "stream_error",
                        "message": error.to_string(),
                    })
                    .to_string(),
                }
            }
        };
        let sent = tokio::select! {
            changed = stop.changed() => {
                !(changed.is_err() || *stop.borrow())
            }
            result = sender.send(kernel_event) => result.is_ok(),
        };
        if !sent {
            break;
        }
    }
}

fn serialize_codex_event(sequence: u64, event: &CodexEvent) -> KernelEvent {
    let kind = event.msg.to_string();
    match serde_json::to_string(event) {
        Ok(payload_json) => KernelEvent {
            sequence,
            kind,
            payload_json,
        },
        Err(error) => KernelEvent {
            sequence,
            kind: "serialization_error".to_string(),
            payload_json: json!({
                "type": "serialization_error",
                "message": error.to_string(),
            })
            .to_string(),
        },
    }
}

async fn join_event_task(session: &SessionRuntime) {
    let Some(mut task) = session.event_task.lock().await.take() else {
        return;
    };
    if tokio::time::timeout(Duration::from_secs(1), &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use codex_protocol::AgentPath;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpToolCallBeginEvent;
    use codex_protocol::protocol::SubAgentActivityEvent;
    use codex_protocol::protocol::SubAgentActivityKind;

    use super::AbsolutePathBuf;
    use super::ApprovalDecision;
    use super::ApprovalKind;
    use super::ApprovalResponse;
    use super::CODEX_COMMIT;
    use super::CodexReviewDecision;
    use super::ConfigBuilder;
    use super::INTERFACE_VERSION;
    use super::Kernel;
    use super::KernelActionAuthorization;
    use super::KernelActionPayload;
    use super::KernelActionRequest;
    use super::KernelExecutableAuthorization;
    use super::KernelFileChange;
    use super::KernelFileOperation;
    use super::KernelOptions;
    use super::ModelPort;
    use super::ModelPortFailure;
    use super::ModelPortRequest;
    use super::ModelPortStream;
    use super::PermissionProfile;
    use super::RejectingKernelActionGate;
    use super::RoleExecutionMode;
    use super::RoleSessionPolicy;
    use super::RoleSessionPolicyRoleId;
    use super::RoleSessionPolicyWorkspaceMode;
    use super::TurnSubmissionOptions;
    use super::canonical_role_policy;
    use super::codex_approval_operation;
    use super::codex_review_decision;
    use super::descriptor;
    use super::kernel_provider_info;
    use super::model_route;
    use super::serialize_codex_event;
    use super::set_workspace;
    use super::user_text_request;

    const EXPECTED_ROLE_POLICIES: &[(&str, &str, bool)] = &[
        ("requirements", "source-read-only", false),
        ("solution", "source-read-only", false),
        ("planner", "source-read-only", false),
        ("executor", "candidate-write", true),
        ("reviewer", "candidate-read-only", false),
        ("verifier", "candidate-read-only", false),
        ("adversarial-verifier", "candidate-read-only", false),
        ("remediator", "candidate-write", true),
    ];

    fn generated_role_id(role: &str) -> RoleSessionPolicyRoleId {
        match role {
            "requirements" => RoleSessionPolicyRoleId::Requirements,
            "solution" => RoleSessionPolicyRoleId::Solution,
            "planner" => RoleSessionPolicyRoleId::Planner,
            "executor" => RoleSessionPolicyRoleId::Executor,
            "reviewer" => RoleSessionPolicyRoleId::Reviewer,
            "verifier" => RoleSessionPolicyRoleId::Verifier,
            "adversarial-verifier" => RoleSessionPolicyRoleId::AdversarialVerifier,
            "remediator" => RoleSessionPolicyRoleId::Remediator,
            _ => panic!("unknown test role"),
        }
    }

    fn generated_workspace_mode(workspace: &str) -> RoleSessionPolicyWorkspaceMode {
        match workspace {
            "source-read-only" => RoleSessionPolicyWorkspaceMode::SourceReadOnly,
            "candidate-read-only" => RoleSessionPolicyWorkspaceMode::CandidateReadOnly,
            "candidate-write" => RoleSessionPolicyWorkspaceMode::CandidateWrite,
            _ => panic!("unknown test workspace"),
        }
    }

    #[derive(Debug)]
    struct UnusedModelPort;

    impl ModelPort for UnusedModelPort {
        fn stream(
            &self,
            _request: ModelPortRequest,
        ) -> futures::future::BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>>
        {
            Box::pin(async {
                Err(ModelPortFailure::new(
                    "UNEXPECTED_MODEL_CALL",
                    "test did not expect a model call",
                ))
            })
        }
    }

    #[test]
    fn mcp_approval_callback_preserves_typed_id_and_single_use_decision() {
        for id in [serde_json::json!(42), serde_json::json!("42")] {
            for (decision, expected) in [
                (ApprovalDecision::Approved, "accept"),
                (
                    ApprovalDecision::Denied {
                        rejection: "no".into(),
                    },
                    "decline",
                ),
                (ApprovalDecision::Abort, "cancel"),
            ] {
                let response = ApprovalResponse {
                    session_id: "session".into(),
                    kind: ApprovalKind::Mcp,
                    operation_id: serde_json::to_string(&("server", &id)).expect("callback id"),
                    turn_id: Some("turn".into()),
                    decision,
                };
                let super::Op::ResolveElicitation {
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                } = codex_approval_operation(response).expect("MCP callback")
                else {
                    panic!("expected MCP callback");
                };
                assert_eq!(
                    serde_json::to_value(request_id).expect("typed request id"),
                    id
                );
                assert_eq!(server_name, "server");
                assert_eq!(serde_json::to_value(decision).expect("decision"), expected);
                assert!(content.is_none());
                assert!(meta.is_none());
            }
        }
        assert!(
            codex_approval_operation(ApprovalResponse {
                session_id: "session".into(),
                kind: ApprovalKind::Mcp,
                operation_id: "[\"server\",42]".into(),
                turn_id: None,
                decision: ApprovalDecision::ApprovedForSession,
            })
            .is_err()
        );
    }

    #[test]
    fn private_model_and_action_payloads_are_redacted_from_debug() {
        let model = ModelPortRequest {
            request_id: "request".to_owned(),
            payload_json: "TOKEN=TOKEN_VALUE PAYLOAD=PAYLOAD_VALUE".to_owned(),
        };
        let action = KernelActionRequest {
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
            operation_id: "operation".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "shell_command".to_owned(),
            payload: KernelActionPayload::Function {
                arguments: "TOKEN=TOKEN_VALUE PAYLOAD=PAYLOAD_VALUE".to_owned(),
            },
        };
        for rendered in [format!("{model:?}"), format!("{action:?}")] {
            assert!(!rendered.contains("TOKEN_VALUE"));
            assert!(!rendered.contains("PAYLOAD_VALUE"));
        }
        let change = KernelFileChange {
            operation: KernelFileOperation::Write,
            path: "/private/TOKEN_VALUE".to_owned(),
            move_path: Some("/private/PAYLOAD_VALUE".to_owned()),
        };
        let rendered = format!("{change:?}");
        assert!(!rendered.contains("TOKEN_VALUE"));
        assert!(!rendered.contains("PAYLOAD_VALUE"));
    }

    #[test]
    fn action_authorization_debug_redacts_binding_executable_and_arguments() {
        let authorization = KernelActionAuthorization::new(
            "REQUEST_BINDING_VALUE".to_owned(),
            Some(KernelExecutableAuthorization::new(
                "/private/executable".to_owned(),
                vec!["SECRET_ARGUMENT".to_owned()],
                "IDENTITY_DIGEST_VALUE".to_owned(),
            )),
        );
        let rendered = format!("{authorization:?}");
        for secret in [
            "REQUEST_BINDING_VALUE",
            "/private/executable",
            "SECRET_ARGUMENT",
            "IDENTITY_DIGEST_VALUE",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked in {rendered}");
        }

        let executable = authorization.executable().expect("authorized executable");
        let rendered = format!("{executable:?}");
        for secret in [
            "/private/executable",
            "SECRET_ARGUMENT",
            "IDENTITY_DIGEST_VALUE",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked in {rendered}");
        }
    }

    #[test]
    fn declares_one_execution_authority() {
        let descriptor = descriptor();
        assert_eq!(descriptor.name, "codex-core");
        assert_eq!(descriptor.execution_authorities, 1);
    }

    #[cfg(unix)]
    #[test]
    fn kernel_preserves_existing_home_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = std::env::temp_dir().join(format!(
            "winwincode-kernel-private-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create kernel home");
        let state = home.join("state.sqlite3");
        std::fs::write(&state, b"state").expect("create state file");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777))
            .expect("widen home");
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o666))
            .expect("widen state");
        let script = home.join("skill-script.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("skill script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("script mode");
        let kernel = Kernel::new(
            KernelOptions::new(
                home.clone(),
                std::env::current_exe().expect("current executable"),
            ),
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .expect("construct private kernel");
        assert_eq!(
            std::fs::metadata(&home)
                .expect("home metadata")
                .permissions()
                .mode()
                & 0o777,
            0o777
        );
        assert_eq!(
            std::fs::metadata(&state)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o666
        );
        assert_eq!(
            std::fs::metadata(&script)
                .expect("script metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        drop(kernel);
        std::fs::remove_dir_all(home).expect("remove kernel home");
    }

    const EXPECTED_CODEX_PATCH_SET: &[&str] = &[
        "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
        "upstream/patches/codex/0002-inject-model-stream-transport.patch",
        "upstream/patches/codex/0003-export-config-builder.patch",
        "upstream/patches/codex/0005-remount-split-bwrap-root-read-only.patch",
        "upstream/patches/codex/0006-tool-gate-and-exact-turn-replay.patch",
        "upstream/patches/codex/0007-bind-tool-gate-executable-identity.patch",
        "upstream/patches/codex/0008-atomic-apply-patch.patch",
        "upstream/patches/codex/0009-canonicalize-intercepted-apply-patch.patch",
        "upstream/patches/codex/0010-submit-change-batch-handoff.patch",
        "upstream/patches/codex/0011-deterministic-tool-context-gc.patch",
        "upstream/patches/codex/0012-record-delegated-handoff-output.patch",
        "upstream/patches/codex/0013-code-mode-authorized-tool-definitions.patch",
        "upstream/patches/codex/0014-code-mode-dispatch-catalog.patch",
        "upstream/patches/codex/0015-code-mode-dispatch-portable-requests.patch",
        "upstream/patches/codex/0016-code-mode-dispatch-authorization.patch",
        "upstream/patches/codex/0017-code-mode-dispatch-hook-tests.patch",
        "upstream/patches/codex/0018-code-mode-dispatch-cell-scope.patch",
        "upstream/patches/codex/0019-code-mode-dispatch-continuations.patch",
        "upstream/patches/codex/0020-code-mode-dispatch-terminal-boundaries.patch",
        "upstream/patches/codex/0021-code-mode-dispatch-io-authorization.patch",
        "upstream/patches/codex/0022-tool-execution-facts-schema.patch",
        "upstream/patches/codex/0023-tool-execution-facts-store.patch",
        "upstream/patches/codex/0024-tool-execution-facts-store-tests.patch",
        "upstream/patches/codex/0025-tool-execution-facts-dispatch.patch",
        "upstream/patches/codex/0026-tool-execution-facts-wiring.patch",
        "upstream/patches/codex/0027-tool-execution-facts-tests.patch",
        "upstream/patches/codex/0028-tool-execution-facts-catalog-tests.patch",
        "upstream/patches/codex/0029-tool-runtime-recovery-relations-schema.patch",
        "upstream/patches/codex/0030-tool-runtime-recovery-relations-store.patch",
        "upstream/patches/codex/0031-tool-runtime-recovery-read-authority.patch",
        "upstream/patches/codex/0032-tool-runtime-recovery-result-recovery.patch",
        "upstream/patches/codex/0033-tool-runtime-recovery-recovery-tests.patch",
        "upstream/patches/codex/0034-tool-runtime-recovery-cell-lifecycle.patch",
        "upstream/patches/codex/0035-tool-runtime-reconciliation-state.patch",
        "upstream/patches/codex/0036-tool-runtime-reconciliation-dispatch.patch",
        "upstream/patches/codex/0037-tool-runtime-reconciliation-tests.patch",
        "upstream/patches/codex/0038-tool-runtime-reconciliation-process-receipt.patch",
        "upstream/patches/codex/0039-tool-diagnostics-schema.patch",
        "upstream/patches/codex/0040-tool-diagnostics-queue.patch",
        "upstream/patches/codex/0041-tool-diagnostics-responses.patch",
        "upstream/patches/codex/0042-tool-diagnostics-queue-tests.patch",
        "upstream/patches/codex/0043-tool-diagnostics-detector.patch",
        "upstream/patches/codex/0044-tool-diagnostics-detector-tests.patch",
        "upstream/patches/codex/0045-tool-diagnostics-feedback.patch",
        "upstream/patches/codex/0046-tool-diagnostics-boundaries.patch",
        "upstream/patches/codex/0047-tool-dependencies-schema.patch",
        "upstream/patches/codex/0048-tool-dependencies-input-store.patch",
        "upstream/patches/codex/0049-tool-dependencies-input-tests.patch",
        "upstream/patches/codex/0050-tool-dependencies-sharing-store.patch",
        "upstream/patches/codex/0051-tool-dependencies-sharing-store-tests.patch",
        "upstream/patches/codex/0052-tool-dependencies-adapter-contract.patch",
        "upstream/patches/codex/0053-tool-dependencies-dispatch.patch",
        "upstream/patches/codex/0054-tool-dependencies-sharing-runtime.patch",
        "upstream/patches/codex/0055-tool-dependencies-sharing-io.patch",
        "upstream/patches/codex/0056-tool-dependencies-sharing-tests.patch",
        "upstream/patches/codex/0057-tool-dependencies-responses.patch",
        "upstream/patches/codex/0058-tool-dependencies-diagnostics.patch",
        "upstream/patches/codex/0059-tool-receipts-state.patch",
        "upstream/patches/codex/0060-tool-receipts-state-tests.patch",
        "upstream/patches/codex/0061-tool-receipts-context.patch",
        "upstream/patches/codex/0062-tool-receipts-boundaries.patch",
        "upstream/patches/codex/0063-tool-receipts-test-isolation.patch",
        "upstream/patches/codex/0064-tool-receipts-interrupted-history.patch",
        "upstream/patches/codex/0065-tool-receipts-workspace-v8-link.patch",
        "upstream/patches/codex/0066-tool-receipts-integration-feedback.patch",
        "upstream/patches/codex/0067-tool-receipts-concurrent-writes.patch",
        "upstream/patches/codex/0068-tool-receipts-surface-fixtures.patch",
        "upstream/patches/codex/0069-tool-receipts-portable-fixtures.patch",
        "upstream/patches/codex/0070-tool-receipts-stream-dispatch.patch",
        "upstream/patches/codex/0071-tool-review-stream-delivery.patch",
        "upstream/patches/codex/0072-tool-review-progress-isolation.patch",
        "upstream/patches/codex/0073-tool-review-sharing-settlement.patch",
        "upstream/patches/codex/0074-tool-review-sharing-cancellation.patch",
        "upstream/patches/codex/0075-tool-review-diagnostic-evidence.patch",
        "upstream/patches/codex/0076-tool-review-tls.patch",
        "upstream/patches/codex/0077-tool-review-memmap.patch",
        "upstream/patches/codex/0078-tool-review-diagnostic-offer-regressions.patch",
        "upstream/patches/codex/0079-tool-review-optional-diagnostic-isolation.patch",
        "upstream/patches/codex/0080-tool-review-diagnostic-receipt-recovery.patch",
        "upstream/patches/codex/0081-tool-review-typed-handoff-replay.patch",
        "upstream/patches/codex/0082-tool-review-fixture-resource-lifetime.patch",
        "upstream/patches/codex/0083-tool-review-tool-fact-shutdown-regressions.patch",
        "upstream/patches/codex/0084-tool-review-tool-fact-shutdown-barrier.patch",
        "upstream/patches/codex/0085-tool-review-app-client-timeout-phases.patch",
        "upstream/patches/codex/0086-tool-review-completion-wait-state.patch",
        "upstream/patches/codex/0087-tool-review-agent-completion-graph.patch",
        "upstream/patches/codex/0088-tool-review-agent-completion-runtime.patch",
        "upstream/patches/codex/0089-tool-review-shell-telemetry-outcomes.patch",
        "upstream/patches/codex/0090-tool-review-wait-agent-tree-fixtures.patch",
        "upstream/patches/codex/0091-tool-review-exec-server-resume-diagnostics.patch",
        "upstream/patches/codex/0092-tool-review-rollout-budget-diagnostics.patch",
        "upstream/patches/codex/0093-recover-original-turn-start-options.patch",
        "upstream/patches/codex/0094-export-mcp-server-transport-config.patch",
        "upstream/patches/codex/0095-tool-payload-mismatch-model-feedback.patch",
        "upstream/patches/codex/0096-seatbelt-explicit-literal-deny.patch",
        "upstream/patches/codex/0097-zsh-parent-approval-context.patch",
        "upstream/patches/codex/0098-mechanism-behavior-audit.patch",
        "upstream/patches/codex/0099-task-state-compaction-budget.patch",
        "upstream/patches/codex/0100-runtime-timer-and-pending-cost.patch",
        "upstream/patches/codex/0101-stdio-lifecycle-deadlines.patch",
        "upstream/patches/codex/0102-exact-input-call-routing.patch",
        "upstream/patches/codex/0103-private-input-recovery-store.patch",
        "upstream/patches/codex/0104-input-recovery-dispatch-and-pages.patch",
        "upstream/patches/codex/0105-host-model-retry-semantics.patch",
        "upstream/patches/codex/0106-required-tool-fact-storage-fatal.patch",
        "upstream/patches/codex/0107-terminal-core-error-rollout.patch",
        "upstream/patches/codex/0108-mcp-timeout-and-test-runtime.patch",
        "upstream/patches/codex/0109-execution-terminal-cause-and-drain.patch",
        "upstream/patches/codex/0110-model-authority-expiry-cause.patch",
        "upstream/patches/codex/0111-tool-call-request-result-pairs.patch",
        "upstream/patches/codex/0112-tool-review-handoff-receipt-order.patch",
        "upstream/patches/codex/0113-tool-review-shutdown-owner-release.patch",
        "upstream/patches/codex/0114-tool-review-call-pair-regressions.patch",
        "upstream/patches/codex/0115-app-server-internal-error-schema.patch",
        "upstream/patches/codex/0116-tool-review-output-result-reference.patch",
        "upstream/patches/codex/0117-shared-tool-fatal-storage.patch",
        "upstream/patches/codex/0118-guardian-transcript-test-registration.patch",
        "upstream/patches/codex/0119-native-fixture-transport-lifecycle.patch",
        "upstream/patches/codex/0120-native-fixture-network-isolation.patch",
        "upstream/patches/codex/0121-tool-review-supervised-process-shutdown.patch",
    ];

    #[test]
    fn clamps_event_capacity_and_reports_exact_source() {
        let home =
            std::env::temp_dir().join(format!("winwincode-kernel-options-{}", std::process::id()));
        let helper = std::env::current_exe().expect("current test executable");
        let mut options = KernelOptions::new(home.clone(), helper);
        options.event_capacity = 1;
        options.shutdown_timeout = Duration::from_millis(10);
        let kernel = Kernel::new(
            options,
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .expect("construct kernel");
        let build = kernel.build_info();
        assert_eq!(build.interface_version, INTERFACE_VERSION);
        assert_eq!(build.interface_version, 12);
        assert_eq!(build.codex_commit, CODEX_COMMIT);
        assert_eq!(build.patch_set, EXPECTED_CODEX_PATCH_SET);
        assert_eq!(build.event_capacity, 16);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn kernel_options_bind_the_bundled_helper_only_as_the_linux_sandbox() {
        let home = std::env::temp_dir().join(format!(
            "winwincode-kernel-sandbox-options-{}",
            std::process::id()
        ));
        let helper = home.join("winwincode-kernel-helper");
        let options = KernelOptions::new(home, helper.clone());

        if cfg!(target_os = "linux") {
            assert_eq!(options.linux_sandbox_executable, Some(helper));
        } else {
            assert_eq!(options.linux_sandbox_executable, None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn linux_sandbox_alias_defers_helper_availability_until_execution() {
        let root = tempfile::tempdir().unwrap();
        let helper = root.path().join("missing-helper");
        let (alias, _directory) = super::install_linux_sandbox_alias(root.path(), &helper).unwrap();
        assert!(!helper.exists());
        assert_eq!(std::fs::read_link(&alias).unwrap(), helper);
        assert_eq!(
            std::process::Command::new(&alias)
                .output()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &helper).unwrap();
        assert!(
            std::process::Command::new(&alias)
                .arg("--list")
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_linux_sandbox_alias_survives_legacy_bwrap_reentry() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::process::CommandExt as _;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let helper = std::env::var_os("WWC_WORKER_HELPER_EXECUTABLE").map_or_else(
            || {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("winwincode-kernel-helper")
            },
            std::path::PathBuf::from,
        );
        let bwrap = Command::new("/bin/sh")
            .args(["-c", "command -v bwrap"])
            .output()
            .unwrap();
        assert!(bwrap.status.success(), "system bwrap is required");
        let bwrap = String::from_utf8(bwrap.stdout).unwrap().trim().to_owned();
        let help = Command::new(&bwrap).arg("--help").output().unwrap();
        let help = format!(
            "{}{}",
            String::from_utf8_lossy(&help.stdout),
            String::from_utf8_lossy(&help.stderr)
        );
        let kernel = Kernel::new(
            KernelOptions::new(root.path().join("home"), helper.clone()),
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .unwrap();
        let alias = kernel.options.linux_sandbox_executable.clone().unwrap();
        assert!(alias.is_absolute());
        assert_eq!(
            alias.canonicalize().unwrap(),
            helper.canonicalize().unwrap()
        );
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let profile = serde_json::to_string(&PermissionProfile::read_only()).unwrap();
        for supports_argv0 in [false, true] {
            if supports_argv0 && !help.contains("--argv0") {
                continue;
            }
            let path = root.path().join(format!("bwrap-{supports_argv0}"));
            std::fs::create_dir(&path).unwrap();
            let log = path.join("calls");
            let options = if supports_argv0 { " --argv0" } else { "" };
            let shim = path.join("bwrap");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '%s\\n' '--as-pid-1 --perms{options}'; exit 0; fi\nprintf '%s\\n' \"$*\" >> {}\nexec {} \"$@\"\n",
                    quote(log.to_str().unwrap()),
                    quote(&bwrap),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
            let argv0 = if alias.file_name().unwrap() == "codex-linux-sandbox" {
                alias.as_os_str()
            } else {
                std::ffi::OsStr::new("codex-linux-sandbox")
            };
            let output = Command::new(&alias)
                .arg0(argv0)
                .arg("--sandbox-policy-cwd")
                .arg(root.path())
                .arg("--permission-profile")
                .arg(&profile)
                .args(["--no-proc", "--", "/bin/echo", "sandbox-reentry"])
                .env("PATH", &path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "argv0={supports_argv0}: {output:?}"
            );
            assert_eq!(output.stdout, b"sandbox-reentry\n");
            assert_eq!(
                std::fs::read_to_string(log).unwrap().contains("--argv0"),
                supports_argv0
            );
        }
        assert_eq!(alias.file_name().unwrap(), "codex-linux-sandbox");
        drop(kernel);
        assert!(!alias.exists());
    }

    #[test]
    fn turn_submission_options_are_attached_to_every_turn_request() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["answer"],
            "properties": {"answer": {"type": "string"}},
        });
        let request = user_text_request(
            "return structured output".to_owned(),
            &TurnSubmissionOptions {
                image_urls: Vec::new(),
                final_output_json_schema: Some(schema.clone()),
                submit_change_batch: false,
            },
        );
        let image_url = "data:image/png;base64,aW1hZ2U=".to_owned();
        let image_request = user_text_request(
            "inspect".to_owned(),
            &TurnSubmissionOptions {
                image_urls: vec![image_url.clone()],
                ..TurnSubmissionOptions::default()
            },
        );
        let codex_core_api::TurnInput::UserInput { content, .. } = image_request.input else {
            panic!("expected user input")
        };
        assert!(
            matches!(&content[1], codex_core_api::UserInput::Image { image_url: actual, .. } if actual == &image_url)
        );
        assert_eq!(request.start.final_output_json_schema, Some(schema));
        assert_eq!(
            user_text_request("react".to_owned(), &TurnSubmissionOptions::default())
                .start
                .final_output_json_schema,
            None
        );
    }

    #[tokio::test]
    async fn shutdown_releases_the_initialized_runtime() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-kernel-runtime-shutdown-{}",
            std::process::id()
        ));
        let home = root.join("home");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&home).expect("create kernel home");
        let helper = std::env::current_exe().expect("current test executable");
        let kernel = Kernel::new(
            KernelOptions::new(home, helper),
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .expect("construct kernel");
        let runtime = kernel.runtime().await.expect("initialize runtime");
        let runtime_owner = Arc::downgrade(&runtime);
        drop(runtime);

        kernel.shutdown().await.expect("shut down kernel");

        assert!(
            runtime_owner.upgrade().is_none(),
            "shutdown must release the complete runtime before it returns"
        );
        std::fs::remove_dir_all(root).expect("remove kernel home after shutdown");
    }

    #[test]
    fn validates_exact_kernel_model_routes() {
        assert_eq!(
            model_route(" deepseek ", " deepseek-chat ").expect("valid route"),
            ("deepseek".to_string(), "deepseek-chat".to_string())
        );
        assert_eq!(
            model_route("", "deepseek-chat")
                .expect_err("blank provider")
                .code(),
            "INVALID_MODEL_ROUTE"
        );
        assert_eq!(
            kernel_provider_info("deepseek").name,
            "WinWinCode Kernel route deepseek"
        );
    }

    #[test]
    fn defines_the_exact_eight_role_workspace_matrix() {
        for &(role, workspace, writer) in EXPECTED_ROLE_POLICIES {
            let policy = canonical_role_policy(&generated_role_id(role), &RoleExecutionMode::React);
            assert_eq!(policy.workspace_mode, workspace, "{role}");
            assert_eq!(policy.workspace_write, writer, "{role}");

            let debug_probe =
                canonical_role_policy(&generated_role_id(role), &RoleExecutionMode::DebugProbe);
            assert_eq!(debug_probe.workspace_mode, "candidate-read-only", "{role}");
            assert!(!debug_probe.workspace_write, "{role}");
        }
    }

    fn prepare_chat_permission_workspace(workspace: &std::path::Path, home: &std::path::Path) {
        std::fs::create_dir_all(workspace).expect("create workspace");
        std::fs::create_dir_all(home).expect("create home");
        std::fs::create_dir_all(workspace.join(".git")).expect("repository marker");
        std::fs::create_dir_all(workspace.join(".codex")).expect("repository config directory");
        std::fs::write(workspace.join(".codex/config.toml"), "not valid TOML [")
            .expect("candidate config");
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one Kernel instance covers workspace policy, compaction policy and live extension refresh"
    )]
    async fn chat_session_applies_its_declared_workspace_permissions() {
        use winwincode_execution_port::agent_config::{
            AgentProfileSettings, resolve_agent_session_config,
        };

        let root = std::env::temp_dir().join(format!(
            "winwincode-chat-permissions-{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let home = root.join("home");
        prepare_chat_permission_workspace(&workspace, &home);
        let kernel = Kernel::new(
            KernelOptions::new(home, std::env::current_exe().expect("test executable")),
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .expect("construct kernel");
        let runtime = kernel.runtime().await.expect("initialize runtime");
        let worker =
            serde_json::from_value(serde_json::json!("wrk_chat_fixture")).expect("worker id");
        let capabilities = serde_json::from_value(serde_json::json!({
            "capabilityDigest": format!("sha256:{}", "a".repeat(64)),
            "features": ["sandbox"],
            "maxConcurrentJobs": 1,
            "platform": "aarch64-apple-darwin"
        }))
        .expect("capabilities");

        for (sandbox, writer) in [
            ("candidate", true),
            ("read-only", false),
            ("unrestricted", false),
        ] {
            let agent_config = resolve_agent_session_config(
                &worker,
                &capabilities,
                "codex-chat",
                AgentProfileSettings {
                    fusion: None,
                    jev_judge: None,
                    jev_context: None,
                    provider: "fixture-provider".into(),
                    model: "fixture-model".into(),
                    reasoning: if writer { "max" } else { "provider_default" }.into(),
                    tools: vec!["worker:sandbox".into()],
                    sandbox: sandbox.into(),
                    instructions: None,
                },
            )
            .expect("chat config");
            let options = super::SessionOptions {
                cwd: workspace.clone(),
                provider: "fixture-provider".into(),
                model: "fixture-model".into(),
                role_policy: None,
                agent_config,
            };
            std::fs::write(
                runtime.base_config.codex_home.join("config.toml"),
                if writer {
                    "[mcp_servers.live]\ncommand = \"python3\"\n"
                } else {
                    ""
                },
            )
            .expect("change Device config between tasks");
            let result = Kernel::session_config(&runtime, &options, false).await;
            if sandbox == "unrestricted" {
                assert_eq!(
                    result.expect_err("unknown sandbox").code(),
                    "INVALID_AGENT_CONFIG"
                );
                continue;
            }
            let config = result.expect("session config");
            assert!(!config.features.enabled(super::Feature::TokenBudget));
            assert_eq!(config.compact_context_max_tokens, Some(30_000));
            assert_eq!(
                config.compact_prompt.as_deref(),
                Some(winwincode_execution_port::task_handoff::TASK_HANDOFF_COMPACT_PROMPT)
            );
            assert_eq!(
                config
                    .model_reasoning_effort
                    .as_ref()
                    .map(codex_protocol::openai_models::ReasoningEffort::as_str),
                writer.then_some("max"),
            );
            assert_eq!(config.mcp_servers.get().contains_key("live"), writer);
            let policy = config.permissions.file_system_sandbox_policy();
            assert_eq!(
                policy.can_write_path_with_cwd(
                    config.cwd.join("package.json").as_path(),
                    config.cwd.as_path(),
                ),
                writer,
                "{sandbox}"
            );
            assert!(!policy.can_write_path_with_cwd(
                std::path::Path::new("/outside-project.txt"),
                config.cwd.as_path(),
            ));
        }
        drop(runtime);
        kernel.shutdown().await.expect("shutdown kernel");
        std::fs::remove_dir_all(root).expect("remove chat fixture");
    }

    #[tokio::test]
    async fn host_action_owner_preserves_explicit_modes_and_role_sandbox() {
        use winwincode_execution_port::agent_config::{
            AgentProfileSettings, resolve_agent_session_config,
        };
        let root =
            std::env::temp_dir().join(format!("winwincode-host-owner-{}", std::process::id()));
        let workspace = root.join("workspace");
        let home = root.join("home");
        prepare_chat_permission_workspace(&workspace, &home);
        std::fs::write(home.join("config.toml"), concat!(
            "[mcp_servers.inherited]\ncommand = \"python3\"\n",
            "[mcp_servers.automatic]\ncommand = \"python3\"\ndefault_tools_approval_mode = \"auto\"\n",
            "[mcp_servers.prompted]\ncommand = \"python3\"\ndefault_tools_approval_mode = \"prompt\"\n",
            "[mcp_servers.writes]\ncommand = \"python3\"\ndefault_tools_approval_mode = \"writes\"\n",
        )).expect("write private installation policy");
        let kernel = Kernel::new(
            KernelOptions::new(home, std::env::current_exe().expect("test executable")),
            Arc::new(UnusedModelPort),
            Arc::new(RejectingKernelActionGate),
        )
        .expect("construct kernel");
        let runtime = kernel.runtime().await.expect("initialize runtime");
        let worker =
            serde_json::from_value(serde_json::json!("wrk_host_fixture")).expect("worker id");
        let capabilities = serde_json::from_value(serde_json::json!({
            "capabilityDigest": format!("sha256:{}", "a".repeat(64)),
            "features": ["sandbox"], "maxConcurrentJobs": 1, "platform": "aarch64-apple-darwin"
        }))
        .expect("capabilities");
        for sandbox in ["candidate", "read-only"] {
            let options = super::SessionOptions {
                cwd: workspace.clone(),
                provider: "fixture-provider".into(),
                model: "fixture-model".into(),
                role_policy: None,
                agent_config: resolve_agent_session_config(
                    &worker,
                    &capabilities,
                    "codex-chat",
                    AgentProfileSettings {
                        fusion: None,
                        jev_judge: None,
                        jev_context: None,
                        provider: "fixture-provider".into(),
                        model: "fixture-model".into(),
                        reasoning: "provider_default".into(),
                        tools: vec!["worker:sandbox".into()],
                        sandbox: sandbox.into(),
                        instructions: None,
                    },
                )
                .expect("agent configuration"),
            };
            let original = Kernel::session_config(&runtime, &options, false)
                .await
                .expect("core-owned config");
            let hosted = Kernel::session_config(&runtime, &options, true)
                .await
                .expect("host-owned config");
            assert_eq!(
                original.permissions.permission_profile(),
                hosted.permissions.permission_profile()
            );
            assert_eq!(
                original.permissions.approval_policy.value(),
                hosted.permissions.approval_policy.value()
            );
            let mode = |configuration: &codex_core_api::Config, name: &str| {
                serde_json::to_value(
                    configuration.mcp_servers.get()[name].default_tools_approval_mode,
                )
                .expect("mode")
            };
            assert_eq!(mode(&original, "inherited"), serde_json::Value::Null);
            assert_eq!(mode(&original, "automatic"), "auto");
            assert_eq!(mode(&hosted, "inherited"), "approve");
            assert_eq!(mode(&hosted, "automatic"), "approve");
            for name in ["prompted", "writes"] {
                assert_eq!(mode(&hosted, name), mode(&original, name));
            }
            let policy = hosted.permissions.file_system_sandbox_policy();
            assert_eq!(
                policy.can_write_path_with_cwd(
                    workspace.join("fixture.txt").as_path(),
                    workspace.as_path()
                ),
                sandbox == "candidate"
            );
            assert!(!policy.can_write_path_with_cwd(
                std::path::Path::new("/outside-project.txt"),
                workspace.as_path()
            ));
        }
        drop(runtime);
        kernel.shutdown().await.expect("shutdown kernel");
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[tokio::test]
    async fn role_policy_uses_codex_permissions_without_disabling_codex_capabilities() {
        let root =
            std::env::temp_dir().join(format!("winwincode-role-policy-{}", std::process::id()));
        let workspace = root.join("workspace");
        let home = root.join("home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&home).expect("create home");
        let mut base = ConfigBuilder::default()
            .codex_home(home.clone())
            .fallback_cwd(Some(home))
            .strict_config(true)
            .build()
            .await
            .expect("build fixture config");
        set_workspace(&mut base, &workspace).expect("select role workspace");
        base.agents_enabled = true;
        base.update_plan_enabled = true;
        base.experimental_request_user_input_enabled = true;
        base.orchestrator_mcp_enabled = true;
        base.include_skill_instructions = true;

        for &(role, workspace_mode, writer) in EXPECTED_ROLE_POLICIES {
            let policy = RoleSessionPolicy {
                schema_version: 2,
                role_id: generated_role_id(role),
                workspace_mode: generated_workspace_mode(workspace_mode),
                execution_mode: RoleExecutionMode::React,
                developer_instructions: format!("Act only as the {role} role."),
            };
            let mut config = base.clone();
            Kernel::apply_role_session_policy(&mut config, &policy)
                .expect("apply exact role policy");
            let expected_profile = if writer {
                PermissionProfile::workspace_write()
            } else {
                PermissionProfile::read_only()
            };
            assert_eq!(
                config.permissions.permission_profile(),
                &expected_profile,
                "{role}"
            );
            assert_eq!(
                config.permissions.approval_policy.value(),
                if writer {
                    super::AskForApproval::OnRequest
                } else {
                    super::AskForApproval::Never
                },
                "{role}"
            );
            assert!(config.agents_enabled, "{role}");
            assert!(config.update_plan_enabled, "{role}");
            assert!(config.experimental_request_user_input_enabled, "{role}");
            assert!(config.orchestrator_mcp_enabled, "{role}");
            assert!(config.include_skill_instructions, "{role}");
            assert_eq!(
                config.developer_instructions.as_deref(),
                Some(format!("Act only as the {role} role.").as_str()),
                "{role}"
            );
        }

        let valid = RoleSessionPolicy {
            schema_version: 2,
            role_id: RoleSessionPolicyRoleId::Requirements,
            workspace_mode: RoleSessionPolicyWorkspaceMode::SourceReadOnly,
            execution_mode: RoleExecutionMode::React,
            developer_instructions: "Gather requirements.".to_string(),
        };
        let mut unknown_role = valid.clone();
        unknown_role.schema_version = 3;
        let mut changed_workspace = valid.clone();
        changed_workspace.workspace_mode = RoleSessionPolicyWorkspaceMode::CandidateWrite;
        let mut empty_instructions = valid;
        empty_instructions.developer_instructions = "  ".to_string();
        for policy in [unknown_role, changed_workspace, empty_instructions] {
            let error = Kernel::apply_role_session_policy(&mut base.clone(), &policy)
                .expect_err("role policy drift must fail");
            assert_eq!(error.code(), "INVALID_ROLE_POLICY");
        }
        std::fs::remove_dir_all(root).expect("remove role-policy fixture");
    }

    #[test]
    fn rejects_extra_role_policy_fields_at_the_native_boundary() {
        let error = super::parse_role_session_policy(
            r#"{"schemaVersion":2,"roleId":"verifier","workspaceMode":"candidate-read-only","executionMode":"react","developerInstructions":"Verify.","tool":"extra"}"#,
        )
        .expect_err("extra role policy fields must fail");
        assert_eq!(error.code(), "INVALID_ROLE_POLICY");
    }

    #[tokio::test]
    async fn delegated_executor_and_remediator_are_read_only_composers() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-delegated-role-policy-{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let home = root.join("home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&home).expect("create home");
        let mut base = ConfigBuilder::default()
            .codex_home(home.clone())
            .fallback_cwd(Some(home))
            .strict_config(true)
            .build()
            .await
            .expect("build fixture config");
        set_workspace(&mut base, &workspace).expect("select role workspace");
        base.agents_enabled = true;
        base.update_plan_enabled = true;

        for role in ["executor", "remediator"] {
            let policy = RoleSessionPolicy {
                schema_version: 2,
                role_id: generated_role_id(role),
                workspace_mode: RoleSessionPolicyWorkspaceMode::CandidateReadOnly,
                execution_mode: RoleExecutionMode::DelegatedBatch,
                developer_instructions: format!("Compose a change batch as {role}."),
            };
            let mut config = base.clone();
            Kernel::apply_role_session_policy(&mut config, &policy)
                .expect("apply delegated composer policy");
            assert_eq!(
                config.permissions.permission_profile(),
                &PermissionProfile::read_only(),
                "{role}"
            );
            assert!(config.agents_enabled, "{role}");
            assert!(config.update_plan_enabled, "{role}");

            let mut writable = policy;
            writable.workspace_mode = RoleSessionPolicyWorkspaceMode::CandidateWrite;
            let error = Kernel::apply_role_session_policy(&mut base.clone(), &writable)
                .expect_err("delegated composer cannot request candidate-write authority");
            assert_eq!(error.code(), "INVALID_ROLE_POLICY", "{role}");
        }
        std::fs::remove_dir_all(root).expect("remove delegated role-policy fixture");
    }

    #[test]
    fn role_policy_v2_requires_the_exact_execution_mode() {
        let v1 = super::parse_role_session_policy(
            r#"{"schemaVersion":1,"roleId":"executor","workspaceMode":"candidate-write","executionMode":"react","developerInstructions":"Execute."}"#,
        )
        .expect_err("v1 policy is no longer a runtime contract");
        assert_eq!(v1.code(), "INVALID_ROLE_POLICY");

        let unknown = super::parse_role_session_policy(
            r#"{"schemaVersion":2,"roleId":"executor","workspaceMode":"candidate-read-only","executionMode":"composer","developerInstructions":"Compose."}"#,
        )
        .expect_err("unknown execution mode must fail");
        assert_eq!(unknown.code(), "INVALID_ROLE_POLICY");

        let policy = super::parse_role_session_policy(
            r#"{"schemaVersion":2,"roleId":"executor","workspaceMode":"candidate-read-only","executionMode":"delegated_batch","developerInstructions":"Compose."}"#,
        )
        .expect("canonical v2 policy");
        assert_eq!(policy.execution_mode, RoleExecutionMode::DelegatedBatch);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonicalizes_workspace_authority_for_sandbox_enforcement() {
        use std::os::unix::fs::symlink;
        use std::time::SystemTime;

        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "winwincode-kernel-workspace-{}-{nonce}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let workspace_alias = root.join("workspace-alias");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        symlink(&workspace, &workspace_alias).expect("create workspace alias");
        let home = root.join("home");
        std::fs::create_dir(&home).expect("create Codex home");
        std::fs::write(
            home.join("config.toml"),
            "approval_policy = \"never\"\ndefault_permissions = \":workspace\"\n",
        )
        .expect("write fixture config");
        let mut config = ConfigBuilder::default()
            .codex_home(home.clone())
            .fallback_cwd(Some(home))
            .strict_config(true)
            .build()
            .await
            .expect("load fixture config");

        set_workspace(&mut config, &workspace_alias).expect("select workspace");
        let canonical = AbsolutePathBuf::from_absolute_path_checked(
            workspace.canonicalize().expect("canonical workspace"),
        )
        .expect("absolute canonical workspace");

        assert_eq!(config.cwd, canonical);
        assert_eq!(config.workspace_roots, vec![canonical.clone()]);
        assert_eq!(
            config.permissions.workspace_roots(),
            std::slice::from_ref(&canonical)
        );
        let policy = config.permissions.file_system_sandbox_policy();
        assert!(
            policy.can_write_path_with_cwd(
                canonical.join("sandbox-smoke.txt").as_path(),
                canonical.as_path(),
            ),
            "configured workspace must be writable: profile={:?}, active={:?}, policy={policy:?}",
            config.permissions.permission_profile(),
            config.permissions.active_permission_profile(),
        );
        std::fs::remove_dir_all(root).expect("remove workspace fixture");
    }

    #[test]
    fn validates_human_approval_decisions_before_submission() {
        assert!(matches!(
            codex_review_decision(ApprovalDecision::Approved),
            Ok(CodexReviewDecision::Approved)
        ));
        assert!(matches!(
            codex_review_decision(ApprovalDecision::Denied {
                rejection: "not approved".to_string(),
            }),
            Ok(CodexReviewDecision::Denied { rejection }) if rejection == "not approved"
        ));
        assert_eq!(
            codex_review_decision(ApprovalDecision::Denied {
                rejection: "   ".to_string(),
            })
            .expect_err("blank rejection")
            .code(),
            "INVALID_APPROVAL_RESPONSE"
        );
    }

    #[test]
    fn preserves_tool_and_subagent_activity_envelopes() {
        let tool = serialize_codex_event(
            41,
            &Event {
                id: "submission".to_string(),
                msg: EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                    call_id: "tool-call".to_string(),
                    invocation: McpInvocation {
                        server: "fixture".to_string(),
                        tool: "inspect".to_string(),
                        arguments: Some(serde_json::json!({ "path": "sample" })),
                    },
                    connector_id: None,
                    mcp_app_resource_uri: None,
                    link_id: None,
                    app_name: None,
                    action_name: None,
                    plugin_id: None,
                    read_only_hint: Some(true),
                }),
            },
        );
        assert_eq!(tool.sequence, 41);
        assert_eq!(tool.kind, "mcp_tool_call_begin");
        let tool_payload: serde_json::Value =
            serde_json::from_str(&tool.payload_json).expect("tool payload");
        assert_eq!(tool_payload["msg"]["type"], "mcp_tool_call_begin");
        assert_eq!(tool_payload["msg"]["invocation"]["tool"], "inspect");

        let subagent = serialize_codex_event(
            42,
            &Event {
                id: "submission".to_string(),
                msg: EventMsg::SubAgentActivity(SubAgentActivityEvent {
                    event_id: "subagent-event".to_string(),
                    occurred_at_ms: 1,
                    agent_thread_id: ThreadId::from_u128(1),
                    agent_path: AgentPath::root()
                        .join("reviewer")
                        .expect("valid agent path"),
                    kind: SubAgentActivityKind::Started,
                }),
            },
        );
        assert_eq!(subagent.sequence, 42);
        assert_eq!(subagent.kind, "sub_agent_activity");
        let subagent_payload: serde_json::Value =
            serde_json::from_str(&subagent.payload_json).expect("subagent payload");
        assert_eq!(subagent_payload["msg"]["type"], "sub_agent_activity");
        assert_eq!(subagent_payload["msg"]["kind"], "started");
        assert_eq!(subagent_payload["msg"]["agent_path"], "/root/reviewer");
    }
}
