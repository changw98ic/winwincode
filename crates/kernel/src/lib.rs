//! Embedded Codex Core ownership boundary.

mod model_port;

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
use codex_core_api::ToolCallGateFileOperation;
use codex_core_api::ToolCallGatePayload;
use codex_core_api::ToolCallGateRejection;
use codex_core_api::ToolCallGateRequest;
use codex_core_api::TurnInputRequest;
use codex_core_api::TurnInputSubmission;
use codex_core_api::TurnStartOptions;
use codex_core_api::UserInput;
use codex_core_api::build_models_manager;
use codex_core_api::empty_extension_registry;
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
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
pub use winwincode_execution_port::generated::{
    RoleExecutionMode, RoleSessionPolicy, RoleSessionPolicyRoleId, RoleSessionPolicyWorkspaceMode,
};

use crate::model_port::KernelModelStreamTransport;

/// Host-owned tool request observed before Codex Core enters the tool handler.
#[derive(Clone, Eq, PartialEq)]
pub struct KernelActionRequest {
    pub session_id: String,
    pub turn_id: String,
    pub operation_id: String,
    pub namespace: Option<String>,
    pub tool_name: String,
    pub payload: KernelActionPayload,
}

/// Exact Codex tool payload retained only for in-process pre-action authorization.
#[derive(Clone, Eq, PartialEq)]
pub enum KernelActionPayload {
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
            Self::Function { .. } => "Function(<private>)",
            Self::ToolSearch { .. } => "ToolSearch(<private>)",
            Self::Custom { .. } => "Custom(<private>)",
            Self::Shell { .. } => "Shell(<private>)",
            Self::Files { .. } => "Files(<private>)",
        })
    }
}

/// Required host admission boundary for every embedded Codex tool call.
pub trait KernelActionGate: Send + Sync {
    fn authorize(&self, request: KernelActionRequest) -> BoxFuture<'static, KernelResult<()>>;
    fn revalidate(&self, request: KernelActionRequest) -> BoxFuture<'static, KernelResult<()>>;
}

/// Explicit fail-closed gate for surfaces that have not installed an action authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct RejectingKernelActionGate;

impl KernelActionGate for RejectingKernelActionGate {
    fn authorize(&self, _request: KernelActionRequest) -> BoxFuture<'static, KernelResult<()>> {
        Box::pin(async {
            Err(KernelFailure::new(
                "ACTION_GATE_UNAVAILABLE",
                "embedded tool action authority is unavailable",
            ))
        })
    }

    fn revalidate(&self, _request: KernelActionRequest) -> BoxFuture<'static, KernelResult<()>> {
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
    fn authorize(
        &self,
        request: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.authorize(kernel_action_request(request))
                .await
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
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            host.revalidate(kernel_action_request(request))
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

fn kernel_action_request(request: ToolCallGateRequest) -> KernelActionRequest {
    KernelActionRequest {
        session_id: request.thread_id,
        turn_id: request.turn_id,
        operation_id: request.call_id,
        namespace: request.namespace,
        tool_name: request.tool_name,
        payload: match request.payload {
            ToolCallGatePayload::Function { arguments } => {
                KernelActionPayload::Function { arguments }
            }
            ToolCallGatePayload::ToolSearch { arguments_json } => {
                KernelActionPayload::ToolSearch { arguments_json }
            }
            ToolCallGatePayload::Custom { input } => KernelActionPayload::Custom { input },
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

/// Exact embedded Codex source commit.
pub const CODEX_COMMIT: &str = "758ef40f50c1a458425c7cfbf1eb12cbc07af0b0";
/// Exact embedded Codex release tag.
pub const CODEX_TAG: &str = "rust-v0.149.0";
/// Native contract version, independent of the application package version.
pub const INTERFACE_VERSION: u32 = 8;
/// Patches applied to the embedded source in deterministic order.
pub const CODEX_PATCH_SET: &[&str] = &[
    "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
    "upstream/patches/codex/0002-inject-model-stream-transport.patch",
    "upstream/patches/codex/0003-export-config-builder.patch",
    "upstream/patches/codex/0005-remount-split-bwrap-root-read-only.patch",
    "upstream/patches/codex/0006-tool-gate-and-exact-turn-replay.patch",
];

const ROLE_SESSION_POLICY_SCHEMA_VERSION: u32 = 2;

const DEFAULT_EVENT_CAPACITY: usize = 256;
const MIN_EVENT_CAPACITY: usize = 16;
const MAX_EVENT_CAPACITY: usize = 4096;
const DEFAULT_SHUTDOWN_TIMEOUT_MILLIS: u64 = 5_000;

/// Static identity of the single execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDescriptor {
    /// Stable kernel name.
    pub name: &'static str,
    /// Execution authority count. `WinWinCode` deliberately has exactly one.
    pub execution_authorities: u8,
}

/// Runtime build identity exposed to TypeScript and release diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelBuildInfo {
    /// Native interface version.
    pub interface_version: u32,
    /// Upstream release tag.
    pub codex_tag: &'static str,
    /// Full upstream commit.
    pub codex_commit: &'static str,
    /// Ordered applied patch paths.
    pub patch_set: Vec<&'static str>,
    /// Configured bounded event capacity per session.
    pub event_capacity: u32,
}

/// Construction options for one embedded kernel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelOptions {
    /// Absolute data directory owned by `WinWinCode`.
    pub home: PathBuf,
    /// Bundled helper executable used for sandbox and filesystem child operations.
    pub helper_executable: PathBuf,
    /// Number of unconsumed ordered events retained per session.
    pub event_capacity: usize,
    /// Maximum graceful shutdown time per thread.
    pub shutdown_timeout: Duration,
    /// Optional bundled Linux sandbox helper.
    pub linux_sandbox_executable: Option<PathBuf>,
}

impl KernelOptions {
    /// Create options with bounded defaults.
    #[must_use]
    pub fn new(home: PathBuf, helper_executable: PathBuf) -> Self {
        Self {
            home,
            helper_executable,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            shutdown_timeout: Duration::from_millis(DEFAULT_SHUTDOWN_TIMEOUT_MILLIS),
            linux_sandbox_executable: None,
        }
    }
}

/// Options shared by create, resume, and fork operations.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionOptions {
    /// Absolute workspace path.
    pub cwd: PathBuf,
    /// Exact Kernel model provider route.
    pub provider: String,
    /// Exact model identifier within the Kernel provider route.
    pub model: String,
    /// Optional `StrongFlow` role policy applied through Codex Core before thread startup.
    pub role_policy: Option<RoleSessionPolicy>,
}

/// Parses the generated canonical role envelope at the native boundary.
///
/// # Errors
///
/// Returns a typed failure when the JSON is invalid, incomplete, contains
/// extra fields, or does not use the current role-policy schema version.
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

/// Optional configuration replacements applied while forking a live session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOptions {
    /// Optional replacement workspace path.
    pub cwd: Option<PathBuf>,
    /// Optional replacement Kernel provider route. Must be supplied with `model`.
    pub provider: Option<String>,
    /// Optional replacement model.
    pub model: Option<String>,
}

/// Codex approval callback family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Command execution approval.
    Exec,
    /// Patch application approval.
    Patch,
}

/// Human decision sent back to one suspended Codex operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Permit this operation once.
    Approved,
    /// Permit this operation and equivalent prompts for this session.
    ApprovedForSession,
    /// Reject this operation while allowing the turn to continue.
    Denied {
        /// Human-readable reason returned to the model.
        rejection: String,
    },
    /// Reject this operation and stop the active turn.
    Abort,
}

/// Exact identity and decision for one pending Codex approval callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResponse {
    /// Live kernel session that emitted the request.
    pub session_id: String,
    /// Callback family selected by the source event.
    pub kind: ApprovalKind,
    /// Effective approval id, never a UI-generated identity.
    pub operation_id: String,
    /// Source turn for command approvals, when Codex supplied it.
    pub turn_id: Option<String>,
    /// Human decision.
    pub decision: ApprovalDecision,
}

/// One registered Codex session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Codex thread identifier.
    pub session_id: String,
    /// Durable rollout path when persistence is available.
    pub rollout_path: Option<String>,
}

/// One bounded, ordered event returned to TypeScript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelEvent {
    /// Monotonically increasing sequence within this session subscription.
    pub sequence: u64,
    /// Stable Codex event kind.
    pub kind: String,
    /// Lossless serialized Codex event envelope.
    pub payload_json: String,
}

/// Outcome of polling one session's ordered event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPoll {
    /// One event was available before the deadline.
    Event(KernelEvent),
    /// No event was available before the requested deadline.
    Timeout,
    /// The producer ended and no later event can arrive.
    Closed,
}

/// Result of a start-or-steer submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionInfo {
    /// `started`, `steered`, or `not_submitted`.
    pub status: &'static str,
    /// Active turn identifier when accepted.
    pub turn_id: Option<String>,
    /// Core-provided reason when not accepted.
    pub reason: Option<String>,
}

/// Immutable options that identify one submitted turn independently of its
/// prompt text. Every Kernel submission entry point requires this value so a
/// structured-output turn cannot be started or steered through an untyped
/// fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSubmissionOptions {
    /// Exact JSON Schema passed to Codex Core for the final model output.
    /// Ordinary React turns use `None`.
    pub final_output_json_schema: Option<Value>,
}

/// Typed result of reconciling one host-reserved turn against durable rollout state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactTurnReconciliation {
    /// Core accepted the exact turn identity for execution or recovery.
    Started { turn_id: String, recovered: bool },
    /// Core remained idle because the exact turn could not be started now.
    NotSubmitted { reason: String },
    /// The rollout already contains the original successful terminal facts.
    Completed(DurableTurnTerminal),
    /// The rollout already contains the original unsuccessful terminal facts.
    Failed(DurableTurnTerminal),
}

/// Result of an all-session shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownInfo {
    /// Threads that shut down and were removed.
    pub completed: Vec<String>,
    /// Threads whose shutdown operation could not be submitted.
    pub submit_failed: Vec<String>,
    /// Threads that exceeded the shutdown deadline.
    pub timed_out: Vec<String>,
}

/// Stable failure crossing the Rust/TypeScript boundary.
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

    /// Creates the fixed failure returned when the host action authority does
    /// not permit a tool call before its handler starts.
    #[must_use]
    pub fn action_rejected() -> Self {
        Self::new(
            "ACTION_ENFORCEMENT_REJECTED",
            "Control Plane action enforcement rejected the tool call",
        )
    }

    /// Stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Human-readable error detail.
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

/// Kernel-local result.
pub type KernelResult<T> = Result<T, KernelFailure>;

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

/// Process-local embedded Codex kernel.
pub struct Kernel {
    options: KernelOptions,
    model_port: Arc<dyn ModelPort>,
    action_gate: Arc<dyn KernelActionGate>,
    runtime: Arc<Mutex<Option<Arc<Runtime>>>>,
    closed: AtomicBool,
}

impl Kernel {
    /// Validate ownership options without starting background services.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the home path is not absolute, the shutdown timeout is zero,
    /// the helper path is unusable, or the home directory cannot be created.
    pub fn new(
        mut options: KernelOptions,
        model_port: Arc<dyn ModelPort>,
        action_gate: Arc<dyn KernelActionGate>,
    ) -> KernelResult<Self> {
        if !options.home.is_absolute() {
            return Err(KernelFailure::new(
                "INVALID_HOME",
                format!("kernel home must be absolute: {}", options.home.display()),
            ));
        }
        if !options.helper_executable.is_absolute() {
            return Err(KernelFailure::new(
                "INVALID_HELPER_PATH",
                format!(
                    "kernel helper path must be absolute: {}",
                    options.helper_executable.display()
                ),
            ));
        }
        let helper_metadata =
            std::fs::symlink_metadata(&options.helper_executable).map_err(|_| {
                KernelFailure::new(
                    "HELPER_NOT_FOUND",
                    format!(
                        "kernel helper executable does not exist: {}",
                        options.helper_executable.display()
                    ),
                )
            })?;
        if helper_metadata.file_type().is_symlink() || !helper_metadata.is_file() {
            return Err(KernelFailure::new(
                "HELPER_NOT_FOUND",
                format!(
                    "kernel helper executable does not exist: {}",
                    options.helper_executable.display()
                ),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if helper_metadata.permissions().mode() & 0o111 == 0 {
                return Err(KernelFailure::new(
                    "HELPER_NOT_EXECUTABLE",
                    "kernel helper executable is not executable",
                ));
            }
        }
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
        restrict_private_tree(&options.home).map_err(|_| {
            KernelFailure::new(
                "HOME_PERMISSION_FAILED",
                "kernel home permissions could not be restricted",
            )
        })?;
        Ok(Self {
            options,
            model_port,
            action_gate,
            runtime: Arc::new(Mutex::new(None)),
            closed: AtomicBool::new(false),
        })
    }

    /// Return static and configured build identity.
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

    /// Return whether explicit shutdown has begun.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Re-applies the private runtime permissions after Core creates rollout,
    /// state, or child-thread files.  This is intentionally explicit so the
    /// Worker can invoke it at every externally visible turn boundary.
    ///
    /// # Errors
    ///
    /// Returns a `KernelFailure` when a runtime-owned path cannot be
    /// restricted to the private permission policy.
    pub fn enforce_private_permissions(&self) -> KernelResult<()> {
        restrict_private_tree(&self.options.home).map_err(|_| {
            KernelFailure::new(
                "HOME_PERMISSION_FAILED",
                "kernel runtime permissions could not be restricted",
            )
        })
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
            // The Kernel exposes portable JSON-schema tools, not provider-native Responses web
            // search.
            // Search remains available through ordinary host/MCP function tools.
            config.web_search_mode = Constrained::allow_any(WebSearchMode::Disabled);
            // The product ExecutionPort contract exposes RequestUserInput in
            // the default collaboration mode.  Codex keeps this feature
            // disabled by default, so the embedded Kernel must pin it on at
            // runtime instead of allowing the model to receive an
            // "unavailable" tool result with no CP interaction.
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
            config.codex_self_exe = Some(self.options.helper_executable.clone());
            config.codex_linux_sandbox_exe = self.options.linux_sandbox_executable.clone();

            let state_db = init_state_db(&config).await;
            let auth_manager =
                AuthManager::shared_from_config(&config, /* enable_codex_api_key_env */ false)
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
            let manager = Arc::new(
                ThreadManager::new(
                    &config,
                    Arc::clone(&auth_manager),
                    build_models_manager(&config, Arc::clone(&auth_manager)),
                    CodexAppsToolsCache::default(),
                    SessionSource::Exec,
                    environment_manager,
                    empty_extension_registry(),
                    user_instructions_provider,
                    /* analytics_events_client */ None,
                    Arc::clone(&thread_store),
                    local_agent_graph_store_from_state_db(state_db.as_ref()),
                    installation_id,
                    /* attestation_provider */ None,
                    /* external_time_provider */ None,
                )
                .with_model_stream_transport(Arc::new(KernelModelStreamTransport::new(Arc::clone(
                    &self.model_port,
                ))))
                .with_tool_call_gate(Arc::new(CoreToolCallGate {
                    host: Arc::clone(&self.action_gate),
                })),
            );
            restrict_private_tree(&self.options.home).map_err(|_| {
                KernelFailure::new(
                    "HOME_PERMISSION_FAILED",
                    "kernel runtime permissions could not be restricted",
                )
            })?;
            Ok(Arc::new(Runtime {
                manager,
                auth_manager,
                base_config: config,
                state_db,
                sessions: RwLock::new(HashMap::new()),
            }))
        })
    }

    fn session_config(runtime: &Runtime, options: &SessionOptions) -> KernelResult<Config> {
        let mut config = runtime.base_config.clone();
        set_workspace(&mut config, &options.cwd)?;
        let (provider, model) = model_route(&options.provider, &options.model)?;
        config.model_provider = kernel_provider_info(&provider);
        config.model_provider_id = provider;
        config.model = Some(model);
        if let Some(policy) = &options.role_policy {
            Self::apply_role_session_policy(&mut config, policy)?;
        }
        Ok(config)
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
        let permission_profile = if canonical.workspace_write {
            PermissionProfile::workspace_write()
        } else {
            PermissionProfile::read_only()
        };
        let mut permissions = Permissions::from_approval_and_profile(
            Constrained::allow_only(AskForApproval::OnRequest),
            Constrained::allow_only(permission_profile),
        )
        .map_err(|error| KernelFailure::new("ROLE_POLICY_UNAVAILABLE", error.to_string()))?;
        permissions.set_workspace_roots(vec![config.cwd.clone()]);
        config.permissions = permissions;
        config.explicit_permission_profile_mode = true;
        config.approvals_reviewer = ApprovalsReviewer::User;
        config.developer_instructions = Some(policy.developer_instructions.clone());
        Ok(())
    }

    /// Create and register a fresh Codex session.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when runtime initialization, workspace validation, or Codex thread
    /// creation fails.
    pub async fn create_session(&self, options: SessionOptions) -> KernelResult<SessionInfo> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let config = Self::session_config(&runtime, &options)?;
            let thread = runtime
                .manager
                .start_thread(StartThreadOptions::new(config.clone()))
                .await
                .map_err(|error| KernelFailure::new("SESSION_CREATE_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    /// Resume a durable Codex rollout.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the rollout path or workspace is invalid, runtime
    /// initialization fails, or Codex rejects the rollout.
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
            let config = Self::session_config(&runtime, &options)?;
            let thread = Box::pin(runtime.manager.resume_thread_from_rollout(
                config.clone(),
                rollout_path,
                Arc::clone(&runtime.auth_manager),
                /* parent_trace */ None,
                ClientMcpExtensions::default(),
            ))
            .await
            .map_err(|error| KernelFailure::new("SESSION_RESUME_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    /// Fork a live session from its current persisted boundary.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the source session or rollout is unavailable, when workspace
    /// validation fails, or when Codex cannot create the fork.
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
                /* thread_source */ None,
                /* parent_trace */ None,
            ))
            .await
            .map_err(|error| KernelFailure::new("SESSION_FORK_FAILED", error.to_string()))?;
            Box::pin(self.register_session(&runtime, thread, config)).await
        })
        .await
    }

    /// Start a turn when idle or steer the active turn.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for empty input, an unknown session, a closed kernel, or a Codex
    /// submission failure.
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
            self.enforce_private_permissions()?;
            let submission = session
                .thread
                .start_or_steer_turn(user_text_request(text, &options))
                .await
                .map_err(|error| KernelFailure::new("TURN_SUBMIT_FAILED", error.to_string()))?;
            self.enforce_private_permissions()?;
            Ok(submission_info(submission))
        })
        .await
    }

    /// Reconciles one host-reserved turn identity against durable rollout state.
    /// The original user input is submitted only when the identity is absent;
    /// an already-persisted interrupted turn resumes without a duplicate input.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the session or durable history is
    /// unavailable, the exact identity is invalid, or Codex Core rejects the
    /// create-or-recover submission.
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
            self.enforce_private_permissions()?;
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
            let result = match durable {
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
            };
            self.enforce_private_permissions()?;
            result
        })
        .await
    }

    /// Steer only the expected active turn.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for empty input, an unknown session, a closed kernel, or a Codex
    /// steering failure.
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
            self.enforce_private_permissions()?;
            let submission = session
                .thread
                .steer_turn(user_text_request(text, &options), expected_turn_id)
                .await
                .map_err(|error| KernelFailure::new("TURN_STEER_FAILED", error.to_string()))?;
            self.enforce_private_permissions()?;
            Ok(steer_info(submission))
        })
        .await
    }

    /// Interrupt the active turn without destroying the session.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the session is unknown, the kernel is closed, or Codex rejects
    /// the interrupt operation.
    pub async fn interrupt(&self, session_id: &str) -> KernelResult<String> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            self.enforce_private_permissions()?;
            session
                .thread
                .submit(Op::Interrupt)
                .await
                .map_err(|error| KernelFailure::new("INTERRUPT_FAILED", error.to_string()))
                .and_then(|result| {
                    self.enforce_private_permissions()?;
                    Ok(result)
                })
        })
        .await
    }

    /// Resolve one pending command or patch approval by its source operation identity.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the session or operation identity is invalid, the kernel is
    /// closed, or Codex rejects the response submission.
    pub async fn resolve_approval(&self, response: ApprovalResponse) -> KernelResult<String> {
        Self::guard(async {
            if response.operation_id.trim().is_empty() {
                return Err(KernelFailure::new(
                    "INVALID_APPROVAL_RESPONSE",
                    "approval operation id must be non-empty",
                ));
            }
            let decision = codex_review_decision(response.decision)?;
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, &response.session_id).await?;
            self.enforce_private_permissions()?;
            let operation = match response.kind {
                ApprovalKind::Exec => Op::ExecApproval {
                    id: response.operation_id,
                    turn_id: response
                        .turn_id
                        .filter(|turn_id| !turn_id.trim().is_empty()),
                    decision,
                },
                ApprovalKind::Patch => Op::PatchApproval {
                    id: response.operation_id,
                    decision,
                },
            };
            session
                .thread
                .submit(operation)
                .await
                .map_err(|error| KernelFailure::new("APPROVAL_SUBMIT_FAILED", error.to_string()))
                .and_then(|result| {
                    self.enforce_private_permissions()?;
                    Ok(result)
                })
        })
        .await
    }

    /// Resolve one pending Codex user-input request by its source turn identity.
    ///
    /// The response is submitted through the embedded thread rather than through a host-side
    /// callback. This keeps the kernel as the sole owner of the live Codex session and makes the
    /// operation safe to replay after a durable execution-port acknowledgement is lost.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the session or request identity is invalid, the kernel is
    /// closed, or Codex rejects the response submission.
    pub async fn resolve_user_input(
        &self,
        session_id: &str,
        turn_id: String,
        response: RequestUserInputResponse,
    ) -> KernelResult<String> {
        Self::guard(async {
            if session_id.trim().is_empty() || turn_id.trim().is_empty() {
                return Err(KernelFailure::new(
                    "INVALID_INPUT_RESPONSE",
                    "input response session and turn identities must be non-empty",
                ));
            }
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            self.enforce_private_permissions()?;
            session
                .thread
                .submit(Op::UserInputAnswer {
                    id: turn_id,
                    response,
                })
                .await
                .map_err(|_| {
                    KernelFailure::new(
                        "INPUT_RESPONSE_SUBMIT_FAILED",
                        "embedded Codex input response submission failed",
                    )
                })
                .and_then(|result| {
                    self.enforce_private_permissions()?;
                    Ok(result)
                })
        })
        .await
    }

    /// Read the next ordered event and distinguish timeout from stream closure.
    ///
    /// # Errors
    ///
    /// A session that has already been unregistered is a closed stream. Returns a typed failure
    /// only when the kernel itself is closed.
    pub async fn next_event(
        &self,
        session_id: &str,
        timeout: Option<Duration>,
    ) -> KernelResult<EventPoll> {
        Self::guard(async {
            self.enforce_private_permissions()?;
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
            self.enforce_private_permissions()?;
            result
        })
        .await
    }

    /// Return currently registered session identifiers.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the kernel is closed or runtime initialization fails.
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

    /// Gracefully close and unregister one session.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the session is unknown or does not shut down before the
    /// configured deadline.
    pub async fn close_session(&self, session_id: &str) -> KernelResult<()> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = runtime
                .sessions
                .write()
                .await
                .remove(session_id)
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
            if let Ok(thread_id) = ThreadId::try_from(session_id) {
                let _ = runtime.manager.remove_thread(&thread_id).await;
            }
            join_event_task(&session).await;
            self.enforce_private_permissions()?;
            Ok(())
        })
        .await
    }

    /// Shut down all sessions and reject later work.
    ///
    /// # Errors
    ///
    /// Returns a typed panic failure if the embedded shutdown path panics.
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
            self.enforce_private_permissions()?;
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
        restrict_private_tree(&self.options.home).map_err(|_| {
            KernelFailure::new(
                "HOME_PERMISSION_FAILED",
                "kernel rollout permissions could not be restricted",
            )
        })?;
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

    /// Best-effort shutdown used by the native object's finalizer.
    #[must_use]
    pub fn best_effort_shutdown(&self) -> Option<BoxFuture<'static, ()>> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let runtime = Arc::clone(&self.runtime);
        let timeout = self.options.shutdown_timeout;
        let home = self.options.home.clone();
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
            let _ = restrict_private_tree(&home);
        }))
    }
}

#[cfg(unix)]
fn restrict_private_tree(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("private runtime path is a symlink"));
    }
    if metadata.is_dir() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(root)? {
            restrict_private_tree(&entry?.path())?;
        }
    } else if metadata.is_file() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o600))?;
    } else {
        return Err(std::io::Error::other("private runtime path is not a file"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_private_tree(_root: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Return the static kernel descriptor.
#[must_use]
pub const fn descriptor() -> KernelDescriptor {
    KernelDescriptor {
        name: "codex-core",
        execution_authorities: 1,
    }
}

fn user_text_request(text: String, options: &TurnSubmissionOptions) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text,
        text_elements: Vec::new(),
    }])
    .on_start(TurnStartOptions {
        final_output_json_schema: options.final_output_json_schema.clone(),
        ..TurnStartOptions::default()
    })
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
    use super::CODEX_COMMIT;
    use super::CodexReviewDecision;
    use super::ConfigBuilder;
    use super::INTERFACE_VERSION;
    use super::Kernel;
    use super::KernelActionPayload;
    use super::KernelActionRequest;
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
    fn declares_one_execution_authority() {
        let descriptor = descriptor();
        assert_eq!(descriptor.name, "codex-core");
        assert_eq!(descriptor.execution_authorities, 1);
    }

    #[cfg(unix)]
    #[test]
    fn kernel_home_and_preexisting_state_are_restricted() {
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
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(kernel);
        std::fs::remove_dir_all(home).expect("remove kernel home");
    }

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
        assert_eq!(build.interface_version, 8);
        assert_eq!(build.codex_commit, CODEX_COMMIT);
        assert_eq!(
            build.patch_set,
            vec![
                "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
                "upstream/patches/codex/0002-inject-model-stream-transport.patch",
                "upstream/patches/codex/0003-export-config-builder.patch",
                "upstream/patches/codex/0005-remount-split-bwrap-root-read-only.patch",
                "upstream/patches/codex/0006-tool-gate-and-exact-turn-replay.patch",
            ]
        );
        assert_eq!(build.event_capacity, 16);
        let _ = std::fs::remove_dir_all(home);
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
                final_output_json_schema: Some(schema.clone()),
            },
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
        }
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
                super::AskForApproval::OnRequest,
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
