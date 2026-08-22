//! Embedded Codex Core ownership boundary.

mod governed_command;
mod model_port;

pub use governed_command::GOVERNED_COMMAND_SCHEMA_VERSION;
pub use governed_command::GovernedCommandRequest;
pub use governed_command::GovernedCommandResult;
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
use codex_core_api::DynamicToolFunctionSpec;
use codex_core_api::DynamicToolNamespaceSpec;
use codex_core_api::DynamicToolNamespaceTool;
use codex_core_api::DynamicToolSpec;
use codex_core_api::EnvironmentManager;
use codex_core_api::ExecServerRuntimePaths;
use codex_core_api::Features;
use codex_core_api::ForkSnapshot;
use codex_core_api::NewThread;
use codex_core_api::Op;
use codex_core_api::PermissionProfile;
use codex_core_api::Permissions;
use codex_core_api::SessionSource;
use codex_core_api::ShellEnvironmentPolicy;
use codex_core_api::StartThreadOptions;
use codex_core_api::SteerSubmission;
use codex_core_api::ThreadId;
use codex_core_api::ThreadManager;
use codex_core_api::TurnInputRequest;
use codex_core_api::TurnInputSubmission;
use codex_core_api::UserInput;
use codex_core_api::build_models_manager;
use codex_core_api::empty_extension_registry;
use codex_core_api::init_state_db;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::resolve_installation_id;
use codex_core_api::thread_store_from_config;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Event as CodexEvent;
use codex_protocol::protocol::ReviewDecision as CodexReviewDecision;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::model_port::KernelModelStreamTransport;

/// Exact embedded Codex source commit.
pub const CODEX_COMMIT: &str = "758ef40f50c1a458425c7cfbf1eb12cbc07af0b0";
/// Exact embedded Codex release tag.
pub const CODEX_TAG: &str = "rust-v0.149.0";
/// Native contract version, independent of the application package version.
pub const INTERFACE_VERSION: u32 = 4;
/// Patches applied to the embedded source in deterministic order.
pub const CODEX_PATCH_SET: &[&str] = &[
    "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
    "upstream/patches/codex/0002-inject-model-stream-transport.patch",
    "upstream/patches/codex/0003-export-config-builder.patch",
    "upstream/patches/codex/0004-resume-with-caller-options.patch",
];

const GOVERNED_AUTHORITY_SCHEMA_VERSION: u32 = 1;
const GOVERNED_DEVELOPER_INSTRUCTIONS: &str = "Operate only through the model-visible StrongFlow tools supplied for this role. The host owns authorization and human decisions. Do not request hidden tools, alternate execution paths, broader permissions, credentials, network access, or remote publication.";

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOptions {
    /// Absolute workspace path.
    pub cwd: PathBuf,
    /// Exact DSH provider route.
    pub provider: String,
    /// Exact model identifier within the DSH provider route.
    pub model: String,
    /// Optional immutable `StrongFlow` authority applied before thread startup.
    pub governed_authority: Option<GovernedSessionAuthority>,
}

/// Immutable role authority accepted only when it matches the kernel's canonical role matrix.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GovernedSessionAuthority {
    pub schema_version: u32,
    pub role_id: String,
    pub permission_preset: String,
    pub workspace_mode: String,
    pub workspace_root: String,
    pub system_instructions: String,
    pub reasoning_effort: Option<String>,
    pub visible_tools: Vec<String>,
}

impl GovernedSessionAuthority {
    /// Parse the strict host envelope without granting authority for missing or extra fields.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the JSON is invalid, incomplete, or contains extra fields.
    pub fn from_json(value: &str) -> KernelResult<Self> {
        serde_json::from_str(value).map_err(|error| {
            KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                format!("governed session authority is invalid: {error}"),
            )
        })
    }
}

/// Text result returned to one suspended dynamic-tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicToolCallResponse {
    pub session_id: String,
    pub call_id: String,
    pub success: bool,
    pub text: String,
}

/// Optional configuration replacements applied while forking a live session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOptions {
    /// Optional replacement workspace path.
    pub cwd: Option<PathBuf>,
    /// Optional replacement DSH provider route. Must be supplied with `model`.
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
    /// Strict JSON evidence for governed sessions; absent for ordinary DSH chat sessions.
    pub effective_policy_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GovernedSessionEffectivePolicy {
    schema_version: u32,
    authority: &'static str,
    role_id: String,
    permission_preset: String,
    workspace_mode: String,
    workspace_root: String,
    visible_tools: Vec<String>,
    filesystem: &'static str,
    network: &'static str,
    process: &'static str,
    environment: &'static str,
    governed_process: &'static str,
    governed_process_network: &'static str,
    governed_process_environment: &'static str,
    credentials: &'static str,
    approval_policy: &'static str,
    approvals_reviewer: &'static str,
    login_shell: bool,
    environment_selections: Vec<String>,
    instruction_sources: Vec<String>,
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
    governed_authority: Option<GovernedSessionAuthority>,
    governed_commands: governed_command::GovernedCommandController,
    events: Mutex<mpsc::Receiver<KernelEvent>>,
    stop: watch::Sender<bool>,
    event_task: Mutex<Option<JoinHandle<()>>>,
}

struct Runtime {
    manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    base_config: Config,
    sessions: RwLock<HashMap<String, Arc<SessionRuntime>>>,
}

#[derive(Debug, Clone, Copy)]
struct GovernedRolePolicy {
    permission_preset: &'static str,
    workspace_mode: &'static str,
    visible_tools: &'static [&'static str],
    filesystem: &'static str,
}

const DEFINITION_TOOLS: &[&str] = &[
    "artifact.read",
    "artifact.write",
    "workspace.read",
    "code.search",
];
const SNAPSHOT_TOOLS: &[&str] = &[
    "artifact.read",
    "artifact.write",
    "workspace.read",
    "code.search",
    "candidate.diff",
    "command.run",
    "test.run",
];
const CANDIDATE_WRITE_TOOLS: &[&str] = &[
    "artifact.read",
    "artifact.write",
    "workspace.read",
    "code.search",
    "candidate.diff",
    "command.run",
    "test.run",
    "candidate.patch",
];

fn governed_role_policy(role_id: &str) -> Option<GovernedRolePolicy> {
    Some(match role_id {
        "requirements" => GovernedRolePolicy {
            permission_preset: "definition-read",
            workspace_mode: "source-read-only",
            visible_tools: DEFINITION_TOOLS,
            filesystem: "managed-read-only",
        },
        "solution" => GovernedRolePolicy {
            permission_preset: "solution-read",
            workspace_mode: "source-read-only",
            visible_tools: DEFINITION_TOOLS,
            filesystem: "managed-read-only",
        },
        "planner" => GovernedRolePolicy {
            permission_preset: "source-read",
            workspace_mode: "source-read-only",
            visible_tools: DEFINITION_TOOLS,
            filesystem: "managed-read-only",
        },
        "executor" => GovernedRolePolicy {
            permission_preset: "candidate-write",
            workspace_mode: "candidate-write",
            visible_tools: CANDIDATE_WRITE_TOOLS,
            filesystem: "managed-workspace-write",
        },
        "reviewer" | "verifier" | "adversarial-verifier" => GovernedRolePolicy {
            permission_preset: "snapshot-verify",
            workspace_mode: "candidate-read-only",
            visible_tools: SNAPSHOT_TOOLS,
            filesystem: "managed-read-only",
        },
        "remediator" => GovernedRolePolicy {
            permission_preset: "remediation-write",
            workspace_mode: "candidate-write",
            visible_tools: CANDIDATE_WRITE_TOOLS,
            filesystem: "managed-workspace-write",
        },
        _ => return None,
    })
}

fn tool_schema(tool: &str) -> serde_json::Value {
    let text = || json!({ "type": "string", "minLength": 1 });
    match tool {
        "artifact.read" => json!({
            "type": "object",
            "properties": { "artifactId": text() },
            "required": ["artifactId"],
            "additionalProperties": false
        }),
        "artifact.write" => json!({
            "type": "object",
            "properties": {
                "kind": text(),
                "artifact": { "type": "object" }
            },
            "required": ["kind", "artifact"],
            "additionalProperties": false
        }),
        "workspace.read" => json!({
            "type": "object",
            "properties": { "path": text() },
            "required": ["path"],
            "additionalProperties": false
        }),
        "code.search" => json!({
            "type": "object",
            "properties": {
                "query": text(),
                "paths": { "type": "array", "items": text() }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "candidate.diff" => json!({
            "type": "object",
            "properties": { "path": text() },
            "additionalProperties": false
        }),
        "command.run" | "test.run" => json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" }
                },
                "cwd": text()
            },
            "required": ["argv"],
            "additionalProperties": false
        }),
        "candidate.patch" => json!({
            "type": "object",
            "properties": {
                "path": text(),
                "patch": { "type": "string", "minLength": 1 }
            },
            "required": ["path", "patch"],
            "additionalProperties": false
        }),
        _ => json!({ "type": "object", "additionalProperties": false }),
    }
}

fn tool_description(tool: &str) -> &'static str {
    match tool {
        "artifact.read" => "Read one accepted StrongFlow artifact by exact identity.",
        "artifact.write" => "Publish one role-authorized StrongFlow artifact.",
        "workspace.read" => "Read one portable path inside the assigned workspace.",
        "code.search" => "Search source text inside the assigned workspace.",
        "candidate.diff" => "Read the candidate diff or one path-limited portion.",
        "command.run" => "Run one approved plan command in the assigned sandbox.",
        "test.run" => "Run one approved verification command in the assigned sandbox.",
        "candidate.patch" => "Apply one bounded patch inside the assigned candidate workspace.",
        _ => "Unknown StrongFlow tool.",
    }
}

fn dynamic_tool_specs(tools: &[String]) -> Vec<DynamicToolSpec> {
    let mut specs: Vec<DynamicToolSpec> = Vec::new();
    for qualified in tools {
        let (namespace_name, tool_name) = qualified
            .split_once('.')
            .expect("validated StrongFlow tool names always have a namespace");
        let function = DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
            name: tool_name.to_string(),
            description: tool_description(qualified).to_string(),
            input_schema: tool_schema(qualified),
            defer_loading: false,
        });
        if let Some(DynamicToolSpec::Namespace(namespace)) = specs.iter_mut().find(|spec| {
            matches!(spec, DynamicToolSpec::Namespace(namespace) if namespace.name == namespace_name)
        }) {
            namespace.tools.push(function);
        } else {
            specs.push(DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
                name: namespace_name.to_string(),
                description: format!("StrongFlow {namespace_name} operations."),
                tools: vec![function],
            }));
        }
    }
    specs
}

/// Process-local embedded Codex kernel.
pub struct Kernel {
    options: KernelOptions,
    model_port: Arc<dyn ModelPort>,
    runtime: OnceCell<Arc<Runtime>>,
    closed: AtomicBool,
}

impl Kernel {
    /// Validate ownership options without starting background services.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the home path is not absolute, the shutdown timeout is zero,
    /// the helper path is unusable, or the home directory cannot be created.
    pub fn new(mut options: KernelOptions, model_port: Arc<dyn ModelPort>) -> KernelResult<Self> {
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
        if !options.helper_executable.is_file() {
            return Err(KernelFailure::new(
                "HELPER_NOT_FOUND",
                format!(
                    "kernel helper executable does not exist: {}",
                    options.helper_executable.display()
                ),
            ));
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
        Ok(Self {
            options,
            model_port,
            runtime: OnceCell::new(),
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

    fn runtime(&self) -> BoxFuture<'_, KernelResult<Arc<Runtime>>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(KernelFailure::new(
                    "KERNEL_CLOSED",
                    "kernel has already shut down",
                ));
            }
            self.runtime
                .get_or_try_init(|| self.initialize_runtime())
                .await
                .cloned()
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
            // DSH exposes portable JSON-schema tools, not provider-native Responses web search.
            // Search remains available through ordinary host/MCP function tools.
            config.web_search_mode = Constrained::allow_any(WebSearchMode::Disabled);
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
                .with_model_stream_transport(Arc::new(
                    KernelModelStreamTransport::new(Arc::clone(&self.model_port)),
                )),
            );
            Ok(Arc::new(Runtime {
                manager,
                auth_manager,
                base_config: config,
                sessions: RwLock::new(HashMap::new()),
            }))
        })
    }

    fn session_config(runtime: &Runtime, options: &SessionOptions) -> KernelResult<Config> {
        let mut config = runtime.base_config.clone();
        set_workspace(&mut config, &options.cwd)?;
        let (provider, model) = model_route(&options.provider, &options.model)?;
        config.model_provider = dsh_provider_info(&provider);
        config.model_provider_id = provider;
        config.model = Some(model);
        if let Some(authority) = &options.governed_authority {
            Self::apply_governed_authority(&mut config, authority)?;
        }
        Ok(config)
    }

    fn validate_governed_authority(
        config: &Config,
        authority: &GovernedSessionAuthority,
    ) -> KernelResult<(GovernedRolePolicy, Option<ReasoningEffort>)> {
        if authority.schema_version != GOVERNED_AUTHORITY_SCHEMA_VERSION {
            return Err(KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                "governed session authority schema version is unsupported",
            ));
        }
        let policy = governed_role_policy(&authority.role_id).ok_or_else(|| {
            KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                format!("unknown StrongFlow role {}", authority.role_id),
            )
        })?;
        let expected_tools = policy
            .visible_tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect::<Vec<_>>();
        if authority.permission_preset != policy.permission_preset
            || authority.workspace_mode != policy.workspace_mode
            || authority.visible_tools != expected_tools
        {
            return Err(KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                format!(
                    "role {} does not match its canonical permission preset, workspace, or tool surface",
                    authority.role_id
                ),
            ));
        }
        if authority.system_instructions.trim().is_empty() {
            return Err(KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                "governed role instructions must be non-empty",
            ));
        }
        let authority_root = std::fs::canonicalize(&authority.workspace_root).map_err(|error| {
            KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                format!("governed workspace root cannot be resolved: {error}"),
            )
        })?;
        if authority_root != config.cwd.to_path_buf() {
            return Err(KernelFailure::new(
                "INVALID_GOVERNED_AUTHORITY",
                "governed workspace root differs from the native session cwd",
            ));
        }
        let reasoning_effort = authority
            .reasoning_effort
            .as_ref()
            .map(|value| {
                serde_json::from_value::<ReasoningEffort>(json!(value)).map_err(|error| {
                    KernelFailure::new(
                        "INVALID_GOVERNED_AUTHORITY",
                        format!("unknown governed reasoning effort {value}: {error}"),
                    )
                })
            })
            .transpose()?;
        Ok((policy, reasoning_effort))
    }

    fn apply_governed_authority(
        config: &mut Config,
        authority: &GovernedSessionAuthority,
    ) -> KernelResult<()> {
        let (policy, reasoning_effort) = Self::validate_governed_authority(config, authority)?;
        let permission_profile = if policy.filesystem == "managed-workspace-write" {
            PermissionProfile::workspace_write()
        } else {
            PermissionProfile::read_only()
        };
        let mut permissions = Permissions::from_approval_and_profile(
            Constrained::allow_only(AskForApproval::OnRequest),
            Constrained::allow_only(permission_profile),
        )
        .map_err(|error| KernelFailure::new("ENFORCEMENT_UNAVAILABLE", error.to_string()))?;
        permissions.set_workspace_roots(vec![config.cwd.clone()]);
        permissions.allow_login_shell = false;
        permissions.shell_environment_policy = ShellEnvironmentPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: false,
            exclude: Vec::new(),
            r#set: HashMap::new(),
            include_only: Vec::new(),
            use_profile: false,
        };
        permissions.network = None;
        config.permissions = permissions;
        config.explicit_permission_profile_mode = true;
        config.approvals_reviewer = ApprovalsReviewer::User;
        config.base_instructions = Some(authority.system_instructions.clone());
        config.developer_instructions = Some(GOVERNED_DEVELOPER_INSTRUCTIONS.to_string());
        config.include_permissions_instructions = false;
        config.include_apps_instructions = false;
        config.include_collaboration_mode_instructions = false;
        config.include_skill_instructions = false;
        config.orchestrator_skills_enabled = false;
        config.orchestrator_mcp_enabled = false;
        config.include_environment_context = false;
        config.notify = None;
        config.mcp_servers = Constrained::allow_only(HashMap::new());
        config.non_prefixed_mcp_tool_servers = None;
        config.project_doc_max_bytes = 0;
        config.project_doc_fallback_filenames.clear();
        config.agents_enabled = false;
        config.agent_roles.clear();
        config.experimental_request_user_input_enabled = false;
        config.update_plan_enabled = false;
        config.web_search_mode = Constrained::allow_only(WebSearchMode::Disabled);
        config.web_search_config = None;
        config.respect_system_proxy = false;
        config.model_reasoning_effort = reasoning_effort;
        config.features.set(Features::default()).map_err(|error| {
            KernelFailure::new(
                "ENFORCEMENT_UNAVAILABLE",
                format!("governed feature set could not be disabled: {error}"),
            )
        })?;
        Ok(())
    }

    fn start_thread_options(
        config: Config,
        authority: Option<&GovernedSessionAuthority>,
    ) -> StartThreadOptions {
        let mut options = StartThreadOptions::new(config);
        if let Some(authority) = authority {
            options.dynamic_tools = dynamic_tool_specs(&authority.visible_tools);
            options.environments = Some(Vec::new());
        }
        options
    }

    fn governed_config_is_preserved(
        config: &Config,
        authority: &GovernedSessionAuthority,
        policy: GovernedRolePolicy,
        expected_reasoning_effort: Option<&ReasoningEffort>,
    ) -> bool {
        let expected_permission_profile = if policy.filesystem == "managed-workspace-write" {
            PermissionProfile::workspace_write()
        } else {
            PermissionProfile::read_only()
        };
        config.model_reasoning_effort.as_ref() == expected_reasoning_effort
            && config.permissions.permission_profile() == &expected_permission_profile
            && !config.permissions.network_sandbox_policy().is_enabled()
            && config.permissions.network.is_none()
            && !config.permissions.allow_login_shell
            && config.permissions.shell_environment_policy.inherit
                == ShellEnvironmentPolicyInherit::None
            && !config
                .permissions
                .shell_environment_policy
                .ignore_default_excludes
            && config
                .permissions
                .shell_environment_policy
                .exclude
                .is_empty()
            && !config.permissions.shell_environment_policy.use_profile
            && config.permissions.shell_environment_policy.r#set.is_empty()
            && config
                .permissions
                .shell_environment_policy
                .include_only
                .is_empty()
            && config.workspace_roots == vec![config.cwd.clone()]
            && config.permissions.workspace_roots() == [config.cwd.clone()]
            && config.explicit_permission_profile_mode
            && config.base_instructions.as_deref() == Some(authority.system_instructions.as_str())
            && config.developer_instructions.as_deref() == Some(GOVERNED_DEVELOPER_INSTRUCTIONS)
            && !config.include_permissions_instructions
            && !config.include_apps_instructions
            && !config.include_collaboration_mode_instructions
            && !config.include_skill_instructions
            && !config.orchestrator_skills_enabled
            && !config.orchestrator_mcp_enabled
            && !config.include_environment_context
            && config.notify.is_none()
            && config.mcp_servers.is_empty()
            && config.non_prefixed_mcp_tool_servers.is_none()
            && config.project_doc_max_bytes == 0
            && config.project_doc_fallback_filenames.is_empty()
            && !config.agents_enabled
            && config.agent_roles.is_empty()
            && !config.experimental_request_user_input_enabled
            && !config.update_plan_enabled
            && config.web_search_mode.value() == WebSearchMode::Disabled
            && config.web_search_config.is_none()
            && !config.respect_system_proxy
            && config.features.get() == &Features::default()
    }

    async fn effective_governed_policy(
        thread: &CodexThread,
        authority: &GovernedSessionAuthority,
    ) -> KernelResult<String> {
        let snapshot = thread.config_snapshot().await;
        let config = thread.config().await;
        let (policy, expected_reasoning_effort) =
            Self::validate_governed_authority(&config, authority).map_err(|error| {
                KernelFailure::new("ENFORCEMENT_UNAVAILABLE", error.message().to_string())
            })?;
        let workspace_root = config.cwd.to_path_buf().to_string_lossy().into_owned();
        let sources = thread.instruction_sources().await;
        let expected_thread_permission_profile = config
            .permissions
            .permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&snapshot.workspace_roots);
        let mismatch = [
            (
                "model provider",
                snapshot.model_provider_id != config.model_provider_id,
            ),
            (
                "model identity",
                snapshot.model != config.model.as_deref().unwrap_or_default(),
            ),
            (
                "approval policy",
                snapshot.approval_policy != AskForApproval::OnRequest,
            ),
            (
                "approval reviewer",
                snapshot.approvals_reviewer != ApprovalsReviewer::User,
            ),
            (
                "thread permission profile",
                snapshot.permission_profile != expected_thread_permission_profile,
            ),
            ("workspace", snapshot.cwd() != &config.cwd),
            (
                "reasoning effort",
                snapshot.reasoning_effort != expected_reasoning_effort,
            ),
            (
                "environment selections",
                !snapshot.environment_selections().is_empty(),
            ),
            (
                "kernel configuration",
                !Self::governed_config_is_preserved(
                    &config,
                    authority,
                    policy,
                    expected_reasoning_effort.as_ref(),
                ),
            ),
            ("instruction sources", !sources.is_empty()),
        ]
        .into_iter()
        .find_map(|(name, changed)| changed.then_some(name));
        if let Some(mismatch) = mismatch {
            return Err(KernelFailure::new(
                "ENFORCEMENT_UNAVAILABLE",
                format!(
                    "Codex did not preserve governed {mismatch} for role {}",
                    authority.role_id,
                ),
            ));
        }
        let evidence = GovernedSessionEffectivePolicy {
            schema_version: GOVERNED_AUTHORITY_SCHEMA_VERSION,
            authority: "codex-core",
            role_id: authority.role_id.clone(),
            permission_preset: authority.permission_preset.clone(),
            workspace_mode: authority.workspace_mode.clone(),
            workspace_root,
            visible_tools: authority.visible_tools.clone(),
            filesystem: policy.filesystem,
            network: "restricted",
            process: "dynamic-tools-with-governed-command-api",
            environment: "empty",
            governed_process: "platform-sandbox-required",
            governed_process_network: "restricted",
            governed_process_environment: "explicit-allowlist",
            credentials: "dsh-reference-only",
            approval_policy: "on-request",
            approvals_reviewer: "user",
            login_shell: false,
            environment_selections: Vec::new(),
            instruction_sources: Vec::new(),
        };
        serde_json::to_string(&evidence).map_err(|error| {
            KernelFailure::new(
                "ENFORCEMENT_UNAVAILABLE",
                format!("effective governed policy could not be serialized: {error}"),
            )
        })
    }

    async fn discard_unaccepted_thread(runtime: &Runtime, thread: &NewThread) {
        let _ = thread.thread.shutdown_and_wait().await;
        let _ = runtime.manager.remove_thread(&thread.thread_id).await;
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
            let governed_authority = options.governed_authority.clone();
            let authority = governed_authority.as_ref();
            let thread = runtime
                .manager
                .start_thread(Self::start_thread_options(config.clone(), authority))
                .await
                .map_err(|error| KernelFailure::new("SESSION_CREATE_FAILED", error.to_string()))?;
            let effective_policy_json = match authority {
                Some(authority) => {
                    match Self::effective_governed_policy(&thread.thread, authority).await {
                        Ok(evidence) => Some(evidence),
                        Err(error) => {
                            Self::discard_unaccepted_thread(&runtime, &thread).await;
                            return Err(error);
                        }
                    }
                }
                None => None,
            };
            self.register_session(
                &runtime,
                thread,
                config,
                governed_authority,
                effective_policy_json,
            )
            .await
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
            let governed_authority = options.governed_authority.clone();
            let authority = governed_authority.as_ref();
            let thread = match authority {
                Some(authority) => {
                    Box::pin(runtime.manager.resume_thread_from_rollout_with_options(
                        Self::start_thread_options(config.clone(), Some(authority)),
                        rollout_path,
                        Arc::clone(&runtime.auth_manager),
                    ))
                    .await
                }
                None => {
                    Box::pin(runtime.manager.resume_thread_from_rollout(
                        config.clone(),
                        rollout_path,
                        Arc::clone(&runtime.auth_manager),
                        /* parent_trace */ None,
                        ClientMcpExtensions::default(),
                    ))
                    .await
                }
            }
            .map_err(|error| KernelFailure::new("SESSION_RESUME_FAILED", error.to_string()))?;
            let effective_policy_json = match authority {
                Some(authority) => {
                    match Self::effective_governed_policy(&thread.thread, authority).await {
                        Ok(evidence) => Some(evidence),
                        Err(error) => {
                            Self::discard_unaccepted_thread(&runtime, &thread).await;
                            return Err(error);
                        }
                    }
                }
                None => None,
            };
            self.register_session(
                &runtime,
                thread,
                config,
                governed_authority,
                effective_policy_json,
            )
            .await
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
            if source.governed_authority.is_some() {
                return Err(KernelFailure::new(
                    "GOVERNED_SESSION_FORK_DENIED",
                    "governed StrongFlow sessions cannot fork outside their immutable authority",
                ));
            }
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
                    config.model_provider = dsh_provider_info(&provider);
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
            self.register_session(&runtime, thread, config, None, None)
                .await
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
                .start_or_steer_turn(user_text_request(text))
                .await
                .map_err(|error| KernelFailure::new("TURN_SUBMIT_FAILED", error.to_string()))?;
            Ok(submission_info(submission))
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
                .steer_turn(user_text_request(text), expected_turn_id)
                .await
                .map_err(|error| KernelFailure::new("TURN_STEER_FAILED", error.to_string()))?;
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
            session
                .thread
                .submit(Op::Interrupt)
                .await
                .map_err(|error| KernelFailure::new("INTERRUPT_FAILED", error.to_string()))
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
        })
        .await
    }

    /// Resolve one pending `StrongFlow` dynamic-tool request by its source call identity.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for an unknown session, empty call identity, or rejected Codex
    /// submission.
    pub async fn resolve_dynamic_tool(
        &self,
        response: DynamicToolCallResponse,
    ) -> KernelResult<String> {
        Self::guard(async {
            let operation = dynamic_tool_response_operation(&response)?;
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, &response.session_id).await?;
            session.thread.submit(operation).await.map_err(|error| {
                KernelFailure::new("DYNAMIC_TOOL_SUBMIT_FAILED", error.to_string())
            })
        })
        .await
    }

    /// Read the next ordered event and distinguish timeout from stream closure.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when the session is unknown or the kernel is closed.
    pub async fn next_event(
        &self,
        session_id: &str,
        timeout: Option<Duration>,
    ) -> KernelResult<EventPoll> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            let mut events = session.events.lock().await;
            match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, events.recv()).await {
                    Ok(Some(event)) => Ok(EventPoll::Event(event)),
                    Ok(None) => Ok(EventPoll::Closed),
                    Err(_) => Ok(EventPoll::Timeout),
                },
                None => Ok(match events.recv().await {
                    Some(event) => EventPoll::Event(event),
                    None => EventPoll::Closed,
                }),
            }
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

    /// Execute one already-authorized `StrongFlow` command under the stored native role authority.
    ///
    /// # Errors
    ///
    /// Returns a typed policy denial for an ordinary session, a role/tool mismatch, invalid
    /// command facts, or an unavailable platform sandbox.
    pub async fn execute_governed_command(
        &self,
        request: GovernedCommandRequest,
    ) -> KernelResult<GovernedCommandResult> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, &request.session_id).await?;
            let authority = session.governed_authority.as_ref().ok_or_else(|| {
                KernelFailure::new(
                    "GOVERNED_COMMAND_POLICY_DENIED",
                    "ordinary DSH chat sessions cannot execute governed commands",
                )
            })?;
            session
                .governed_commands
                .execute(&self.options, authority, &session.config, request)
                .await
        })
        .await
    }

    /// Cancel one active governed command by its source identity.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for an unknown session or command.
    pub async fn cancel_governed_command(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> KernelResult<()> {
        Self::guard(async {
            let runtime = self.runtime().await?;
            let session = self.session(&runtime, session_id).await?;
            if !session.governed_commands.cancel(command_id).await {
                return Err(KernelFailure::new(
                    "GOVERNED_COMMAND_NOT_FOUND",
                    "governed command is not active in this session",
                ));
            }
            Ok(())
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
            session.governed_commands.cancel_all().await;
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
            let Some(runtime) = self.runtime.get().cloned() else {
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
                session.governed_commands.cancel_all().await;
                let _ = session.stop.send(true);
            }
            let report = runtime
                .manager
                .shutdown_all_threads_bounded(self.options.shutdown_timeout)
                .await;
            for session in &sessions {
                join_event_task(session).await;
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
        governed_authority: Option<GovernedSessionAuthority>,
        effective_policy_json: Option<String>,
    ) -> KernelResult<SessionInfo> {
        let session_id = new_thread.thread_id.to_string();
        let rollout_path = new_thread
            .thread
            .rollout_path()
            .map(|path| path.to_string_lossy().into_owned());
        let (event_tx, event_rx) = mpsc::channel(self.options.event_capacity);
        let (stop, stop_rx) = watch::channel(false);
        let thread = Arc::clone(&new_thread.thread);
        let event_task = tokio::spawn(pump_events(thread, event_tx, stop_rx));
        let session = Arc::new(SessionRuntime {
            thread: new_thread.thread,
            config,
            governed_authority,
            governed_commands: governed_command::GovernedCommandController::new(),
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
            effective_policy_json,
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
        let runtime = self.runtime.get()?.clone();
        let timeout = self.options.shutdown_timeout;
        Some(Box::pin(async move {
            let sessions = runtime
                .sessions
                .write()
                .await
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>();
            for session in &sessions {
                session.governed_commands.cancel_all().await;
                let _ = session.stop.send(true);
            }
            let _ = runtime.manager.shutdown_all_threads_bounded(timeout).await;
            for session in &sessions {
                join_event_task(session).await;
            }
        }))
    }
}

/// Return the static kernel descriptor.
#[must_use]
pub const fn descriptor() -> KernelDescriptor {
    KernelDescriptor {
        name: "codex-core",
        execution_authorities: 1,
    }
}

fn user_text_request(text: String) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text,
        text_elements: Vec::new(),
    }])
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

fn dynamic_tool_response_operation(response: &DynamicToolCallResponse) -> KernelResult<Op> {
    if response.call_id.trim().is_empty() {
        return Err(KernelFailure::new(
            "INVALID_DYNAMIC_TOOL_RESPONSE",
            "dynamic-tool call id must be non-empty",
        ));
    }
    Ok(Op::DynamicToolResponse {
        id: response.call_id.clone(),
        response: DynamicToolResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: response.text.clone(),
            }],
            success: response.success,
        },
    })
}

fn dsh_provider_info(provider: &str) -> ModelProviderInfo {
    ModelProviderInfo {
        name: format!("DSH route {provider}"),
        ..ModelProviderInfo::default()
    }
}

async fn pump_events(
    thread: Arc<CodexThread>,
    sender: mpsc::Sender<KernelEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let mut sequence = 0_u64;
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
    use super::DynamicToolCallOutputContentItem;
    use super::DynamicToolCallResponse;
    use super::GovernedSessionAuthority;
    use super::INTERFACE_VERSION;
    use super::Kernel;
    use super::KernelOptions;
    use super::ModelPort;
    use super::ModelPortFailure;
    use super::ModelPortRequest;
    use super::ModelPortStream;
    use super::Op;
    use super::PermissionProfile;
    use super::codex_review_decision;
    use super::descriptor;
    use super::dynamic_tool_response_operation;
    use super::governed_role_policy;
    use super::model_route;
    use super::serialize_codex_event;
    use super::set_workspace;

    type ExpectedRolePolicy = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [&'static str],
    );

    const EXPECTED_DEFINITION_TOOLS: &[&str] = &[
        "artifact.read",
        "artifact.write",
        "workspace.read",
        "code.search",
    ];
    const EXPECTED_SNAPSHOT_TOOLS: &[&str] = &[
        "artifact.read",
        "artifact.write",
        "workspace.read",
        "code.search",
        "candidate.diff",
        "command.run",
        "test.run",
    ];
    const EXPECTED_CANDIDATE_WRITE_TOOLS: &[&str] = &[
        "artifact.read",
        "artifact.write",
        "workspace.read",
        "code.search",
        "candidate.diff",
        "command.run",
        "test.run",
        "candidate.patch",
    ];
    const EXPECTED_ROLE_POLICIES: &[ExpectedRolePolicy] = &[
        (
            "requirements",
            "definition-read",
            "source-read-only",
            "managed-read-only",
            EXPECTED_DEFINITION_TOOLS,
        ),
        (
            "solution",
            "solution-read",
            "source-read-only",
            "managed-read-only",
            EXPECTED_DEFINITION_TOOLS,
        ),
        (
            "planner",
            "source-read",
            "source-read-only",
            "managed-read-only",
            EXPECTED_DEFINITION_TOOLS,
        ),
        (
            "executor",
            "candidate-write",
            "candidate-write",
            "managed-workspace-write",
            EXPECTED_CANDIDATE_WRITE_TOOLS,
        ),
        (
            "reviewer",
            "snapshot-verify",
            "candidate-read-only",
            "managed-read-only",
            EXPECTED_SNAPSHOT_TOOLS,
        ),
        (
            "verifier",
            "snapshot-verify",
            "candidate-read-only",
            "managed-read-only",
            EXPECTED_SNAPSHOT_TOOLS,
        ),
        (
            "adversarial-verifier",
            "snapshot-verify",
            "candidate-read-only",
            "managed-read-only",
            EXPECTED_SNAPSHOT_TOOLS,
        ),
        (
            "remediator",
            "remediation-write",
            "candidate-write",
            "managed-workspace-write",
            EXPECTED_CANDIDATE_WRITE_TOOLS,
        ),
    ];

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
    fn declares_one_execution_authority() {
        let descriptor = descriptor();
        assert_eq!(descriptor.name, "codex-core");
        assert_eq!(descriptor.execution_authorities, 1);
    }

    #[test]
    fn clamps_event_capacity_and_reports_exact_source() {
        let home =
            std::env::temp_dir().join(format!("winwincode-kernel-options-{}", std::process::id()));
        let helper = std::env::current_exe().expect("current test executable");
        let mut options = KernelOptions::new(home.clone(), helper);
        options.event_capacity = 1;
        options.shutdown_timeout = Duration::from_millis(10);
        let kernel = Kernel::new(options, Arc::new(UnusedModelPort)).expect("construct kernel");
        let build = kernel.build_info();
        assert_eq!(build.interface_version, INTERFACE_VERSION);
        assert_eq!(build.interface_version, 4);
        assert_eq!(build.codex_commit, CODEX_COMMIT);
        assert_eq!(
            build.patch_set,
            vec![
                "upstream/patches/codex/0001-export-client-mcp-extensions.patch",
                "upstream/patches/codex/0002-inject-model-stream-transport.patch",
                "upstream/patches/codex/0003-export-config-builder.patch",
                "upstream/patches/codex/0004-resume-with-caller-options.patch",
            ]
        );
        assert_eq!(build.event_capacity, 16);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn validates_exact_dsh_model_routes() {
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
    }

    #[test]
    fn defines_the_exact_eight_role_authority_matrix() {
        for &(role, preset, workspace, filesystem, tools) in EXPECTED_ROLE_POLICIES {
            let policy = governed_role_policy(role).expect("known governed role");
            assert_eq!(policy.permission_preset, preset, "{role}");
            assert_eq!(policy.workspace_mode, workspace, "{role}");
            assert_eq!(policy.filesystem, filesystem, "{role}");
            assert_eq!(policy.visible_tools, tools, "{role}");
        }
        assert!(governed_role_policy("unknown").is_none());
    }

    #[tokio::test]
    async fn rejects_role_preset_and_tool_drift_before_thread_start() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-governed-authority-{}",
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
        set_workspace(&mut base, &workspace).expect("select governed workspace");
        let workspace_root = workspace
            .canonicalize()
            .expect("canonical workspace")
            .to_string_lossy()
            .into_owned();

        for role in [
            "requirements",
            "solution",
            "planner",
            "executor",
            "reviewer",
            "verifier",
            "adversarial-verifier",
            "remediator",
        ] {
            let policy = governed_role_policy(role).expect("known governed role");
            let authority = GovernedSessionAuthority {
                schema_version: 1,
                role_id: role.to_string(),
                permission_preset: policy.permission_preset.to_string(),
                workspace_mode: policy.workspace_mode.to_string(),
                workspace_root: workspace_root.clone(),
                system_instructions: format!("Act only as {role}."),
                reasoning_effort: Some("medium".to_string()),
                visible_tools: policy
                    .visible_tools
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            };
            let mut config = base.clone();
            Kernel::apply_governed_authority(&mut config, &authority)
                .expect("apply exact authority");
            let expected_profile = if policy.filesystem == "managed-workspace-write" {
                PermissionProfile::workspace_write()
            } else {
                PermissionProfile::read_only()
            };
            assert_eq!(
                config.permissions.permission_profile(),
                &expected_profile,
                "{role}"
            );
        }

        let policy = governed_role_policy("requirements").expect("requirements policy");
        let valid = GovernedSessionAuthority {
            schema_version: 1,
            role_id: "requirements".to_string(),
            permission_preset: policy.permission_preset.to_string(),
            workspace_mode: policy.workspace_mode.to_string(),
            workspace_root,
            system_instructions: "Gather requirements.".to_string(),
            reasoning_effort: Some("medium".to_string()),
            visible_tools: policy
                .visible_tools
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let mut unknown_role = valid.clone();
        unknown_role.role_id = "unknown".to_string();
        let mut changed_preset = valid.clone();
        changed_preset.permission_preset = "candidate-write".to_string();
        let mut changed_tools = valid;
        changed_tools.visible_tools.pop();
        for authority in [unknown_role, changed_preset, changed_tools] {
            let error = Kernel::apply_governed_authority(&mut base.clone(), &authority)
                .expect_err("authority drift must fail");
            assert_eq!(error.code(), "INVALID_GOVERNED_AUTHORITY");
        }
        std::fs::remove_dir_all(root).expect("remove authority fixture");
    }

    #[test]
    fn maps_one_dynamic_tool_result_to_the_exact_codex_call_identity() {
        let operation = dynamic_tool_response_operation(&DynamicToolCallResponse {
            session_id: "session-1".to_string(),
            call_id: "call-7".to_string(),
            success: false,
            text: "denied by the role policy".to_string(),
        })
        .expect("valid dynamic-tool response");
        match operation {
            Op::DynamicToolResponse { id, response } => {
                assert_eq!(id, "call-7");
                assert!(!response.success);
                assert!(matches!(
                    response.content_items.as_slice(),
                    [DynamicToolCallOutputContentItem::InputText { text }]
                        if text == "denied by the role policy"
                ));
            }
            other => panic!("unexpected Codex operation: {other:?}"),
        }
        assert_eq!(
            dynamic_tool_response_operation(&DynamicToolCallResponse {
                session_id: "session-1".to_string(),
                call_id: "  ".to_string(),
                success: true,
                text: String::new(),
            })
            .expect_err("blank call identity must fail")
            .code(),
            "INVALID_DYNAMIC_TOOL_RESPONSE"
        );
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
