use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_core_api::AbsolutePathBuf;
use codex_core_api::Config;
use codex_core_api::PermissionProfile;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::permissions::project_roots_glob_pattern;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::SpawnRequest;
use codex_sandboxing::spawn_process;
use codex_utils_path_uri::PathUri;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::GovernedSessionAuthority;
use crate::KernelFailure;
use crate::KernelOptions;
use crate::KernelResult;

pub const GOVERNED_COMMAND_SCHEMA_VERSION: u32 = 1;

const MAX_COMMAND_ARGUMENTS: usize = 256;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_COMMAND_BYTES: usize = 256 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_mins(10);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const IDENTIFIER_MAX_BYTES: usize = 200;
const SENSITIVE_WORKSPACE_GLOBS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/.credentials.yaml",
    "**/.netrc",
    "**/.npmrc",
    "**/.pypirc",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/id_rsa",
    "**/id_ed25519",
    "**/.docker/config.json",
];

static COMMAND_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernedCommandRequest {
    pub schema_version: u32,
    pub session_id: String,
    pub command_id: String,
    pub tool: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub environment: HashMap<String, String>,
    pub timeout: Duration,
    pub output_limit_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernedCommandResult {
    pub schema_version: u32,
    pub session_id: String,
    pub command_id: String,
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub sandbox: &'static str,
    pub network: &'static str,
    pub environment_names: Vec<String>,
}

pub(crate) struct GovernedCommandController {
    active: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl GovernedCommandController {
    pub(crate) fn new() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn cancel(&self, command_id: &str) -> bool {
        let active = self.active.lock().await;
        active
            .get(command_id)
            .is_some_and(|cancel| cancel.send(true).is_ok())
    }

    pub(crate) async fn cancel_all(&self) {
        let active = self.active.lock().await;
        for cancel in active.values() {
            let _ = cancel.send(true);
        }
    }

    pub(crate) async fn execute(
        &self,
        options: &KernelOptions,
        authority: &GovernedSessionAuthority,
        config: &Config,
        request: GovernedCommandRequest,
    ) -> KernelResult<GovernedCommandResult> {
        validate_request(authority, &request)?;
        let workspace = canonical_directory(Path::new(&authority.workspace_root), "workspace")?;
        if workspace != config.cwd.to_path_buf() {
            return Err(KernelFailure::new(
                "GOVERNED_COMMAND_POLICY_DENIED",
                "governed command workspace differs from its accepted native authority",
            ));
        }
        let cwd = canonical_directory(&request.cwd, "command cwd")?;
        if !cwd.starts_with(&workspace) {
            return Err(KernelFailure::new(
                "GOVERNED_COMMAND_POLICY_DENIED",
                "governed command cwd is outside its assigned workspace",
            ));
        }
        let program = canonical_program(&request.argv[0])?;
        let temp = create_command_temp(options, &request)?;
        let result = self
            .execute_prepared(options, authority, request, workspace, cwd, program, &temp)
            .await;
        let _ = std::fs::remove_dir_all(&temp);
        result
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_prepared(
        &self,
        options: &KernelOptions,
        authority: &GovernedSessionAuthority,
        request: GovernedCommandRequest,
        workspace: PathBuf,
        cwd: PathBuf,
        program: PathBuf,
        temp: &Path,
    ) -> KernelResult<GovernedCommandResult> {
        let (cancel, mut cancelled) = watch::channel(false);
        {
            let mut active = self.active.lock().await;
            if active.insert(request.command_id.clone(), cancel).is_some() {
                return Err(KernelFailure::new(
                    "GOVERNED_COMMAND_ALREADY_ACTIVE",
                    "governed command identity is already active in this session",
                ));
            }
        }

        let command_id = request.command_id.clone();
        let outcome = async {
            let environment = governed_environment(&request.environment, temp)?;
            let permissions = governed_permissions(authority, &workspace, temp, &program)?;
            let manager = SandboxManager::new();
            let sandbox = manager.select_initial(
                &permissions,
                SandboxablePreference::Require,
                WindowsSandboxLevel::Disabled,
                false,
            );
            if !matches!(
                sandbox,
                SandboxType::MacosSeatbelt | SandboxType::LinuxSeccomp
            ) {
                return Err(KernelFailure::new(
                    "ENFORCEMENT_UNAVAILABLE",
                    "this host has no supported governed process sandbox",
                ));
            }
            let mut command = request.argv.clone();
            command[0] = program.to_string_lossy().into_owned();
            let cwd_absolute = absolute_path(&cwd, "command cwd")?;
            let cwd_uri = PathUri::from_abs_path(&cwd_absolute);
            let sandbox_request = manager
                .transform(SandboxTransformRequest {
                    command: SandboxCommand {
                        program: command[0].clone().into(),
                        args: command[1..].to_vec(),
                        cwd: cwd_uri.clone(),
                        env: environment.clone(),
                        managed_network: None,
                        additional_permissions: None,
                    },
                    permissions: &permissions,
                    sandbox,
                    enforce_managed_network: false,
                    environment_id: None,
                    network: None,
                    sandbox_policy_cwd: &cwd_uri,
                    codex_linux_sandbox_exe: options.linux_sandbox_executable.as_deref(),
                    use_legacy_landlock: false,
                    windows_sandbox_level: WindowsSandboxLevel::Disabled,
                    windows_sandbox_private_desktop: false,
                })
                .map_err(|error| {
                    KernelFailure::new(
                        "ENFORCEMENT_UNAVAILABLE",
                        format!("governed process sandbox could not be prepared: {error}"),
                    )
                })?;
            let native_cwd = sandbox_request.cwd.to_abs_path().map_err(|error| {
                KernelFailure::new(
                    "ENFORCEMENT_UNAVAILABLE",
                    format!("sandboxed command cwd could not be materialized: {error}"),
                )
            })?;
            let spawned = spawn_process(SpawnRequest {
                command: &sandbox_request.command,
                cwd: native_cwd.as_path(),
                env: &sandbox_request.env,
                arg0: &sandbox_request.arg0,
                sandbox,
                windows_sandbox: None,
                tty: false,
                stdin_open: false,
                inherited_fds: &[],
            })
            .await
            .map_err(|error| {
                KernelFailure::new(
                    "GOVERNED_COMMAND_SPAWN_FAILED",
                    format!("sandboxed command could not start: {error}"),
                )
            })?;

            let session = spawned.session;
            let total = Arc::new(AtomicUsize::new(0));
            let (limit, mut limit_reached) = watch::channel(false);
            let mut stdout_task = tokio::spawn(collect_output(
                spawned.stdout_rx,
                Arc::clone(&total),
                request.output_limit_bytes,
                limit.clone(),
            ));
            let mut stderr_task = tokio::spawn(collect_output(
                spawned.stderr_rx,
                Arc::clone(&total),
                request.output_limit_bytes,
                limit.clone(),
            ));
            let limit_guard = limit;
            let mut exit = spawned.exit_rx;
            let timeout = tokio::time::sleep(request.timeout);
            tokio::pin!(timeout);
            let (status, exit_code) = tokio::select! {
                code = &mut exit => ("exited", code.ok()),
                changed = cancelled.changed() => {
                    let _ = changed;
                    session.request_terminate();
                    ("cancelled", wait_for_exit(&mut exit).await)
                },
                changed = limit_reached.changed() => {
                    let _ = changed;
                    session.request_terminate();
                    ("output-limit", wait_for_exit(&mut exit).await)
                },
                () = &mut timeout => {
                    session.request_terminate();
                    ("timed-out", wait_for_exit(&mut exit).await)
                },
            };
            drop(limit_guard);
            let stdout = drain_output(&mut stdout_task).await;
            let stderr = drain_output(&mut stderr_task).await;
            let status = if status == "exited"
                && exit_code
                    .is_some_and(|code| likely_sandbox_denial(sandbox, code, &stdout, &stderr))
            {
                "sandbox-denied"
            } else {
                status
            };
            let mut environment_names = environment.keys().cloned().collect::<Vec<_>>();
            environment_names.sort();
            Ok(GovernedCommandResult {
                schema_version: GOVERNED_COMMAND_SCHEMA_VERSION,
                session_id: request.session_id,
                command_id: request.command_id,
                status,
                exit_code,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                sandbox: sandbox_name(sandbox),
                network: "restricted",
                environment_names,
            })
        }
        .await;
        self.active.lock().await.remove(&command_id);
        outcome
    }
}

fn validate_request(
    authority: &GovernedSessionAuthority,
    request: &GovernedCommandRequest,
) -> KernelResult<()> {
    if request.schema_version != GOVERNED_COMMAND_SCHEMA_VERSION {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            "governed command schema version is unsupported",
        ));
    }
    validate_identifier(&request.session_id, "session id")?;
    validate_identifier(&request.command_id, "command id")?;
    if !matches!(request.tool.as_str(), "command.run" | "test.run")
        || !authority
            .visible_tools
            .iter()
            .any(|tool| tool == &request.tool)
    {
        return Err(KernelFailure::new(
            "GOVERNED_COMMAND_POLICY_DENIED",
            "governed role has no authority for this process tool",
        ));
    }
    if request.argv.is_empty()
        || request.argv.len() > MAX_COMMAND_ARGUMENTS
        || request
            .argv
            .iter()
            .any(|argument| argument.is_empty() || argument.contains('\0'))
        || request
            .argv
            .iter()
            .any(|argument| argument.len() > MAX_COMMAND_ARGUMENT_BYTES)
        || request.argv.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
    {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            "governed command argv is empty or exceeds its bounded shape",
        ));
    }
    let sensitive_argument = request.argv.iter().any(|argument| {
        let lower = argument.to_ascii_lowercase();
        lower.starts_with("bearer ")
            || [
                "api_key=",
                "apikey=",
                "authorization=",
                "password=",
                "secret=",
                "token=",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
    });
    if sensitive_argument {
        return Err(KernelFailure::new(
            "GOVERNED_COMMAND_POLICY_DENIED",
            "governed command argv contains a credential-shaped value",
        ));
    }
    if request.timeout.is_zero() || request.timeout > MAX_COMMAND_TIMEOUT {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            "governed command timeout is outside its bounded range",
        ));
    }
    if request.output_limit_bytes == 0 || request.output_limit_bytes > MAX_COMMAND_OUTPUT_BYTES {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            "governed command output limit is outside its bounded range",
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> KernelResult<()> {
    if value.is_empty()
        || value.len() > IDENTIFIER_MAX_BYTES
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
    {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            format!("governed command {label} is invalid"),
        ));
    }
    Ok(())
}

fn canonical_directory(path: &Path, label: &str) -> KernelResult<PathBuf> {
    if !path.is_absolute() {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            format!("{label} must be absolute"),
        ));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            format!("{label} cannot be resolved: {error}"),
        )
    })?;
    if !canonical.is_dir() {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            format!("{label} is not a directory"),
        ));
    }
    Ok(canonical)
}

fn canonical_program(value: &str) -> KernelResult<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(KernelFailure::new(
            "GOVERNED_COMMAND_POLICY_DENIED",
            "governed commands require an absolute executable path",
        ));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            format!("governed command executable cannot be resolved: {error}"),
        )
    })?;
    if !canonical.is_file() {
        return Err(KernelFailure::new(
            "INVALID_GOVERNED_COMMAND",
            "governed command executable is not a file",
        ));
    }
    Ok(canonical)
}

fn absolute_path(path: &Path, label: &str) -> KernelResult<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path_checked(path).map_err(|error| {
        KernelFailure::new(
            "ENFORCEMENT_UNAVAILABLE",
            format!("{label} could not be represented for the sandbox: {error}"),
        )
    })
}

fn path_entry(
    path: &Path,
    access: FileSystemAccessMode,
    label: &str,
) -> KernelResult<FileSystemSandboxEntry> {
    Ok(FileSystemSandboxEntry::new(
        FileSystemPath::from(absolute_path(path, label)?),
        access,
    ))
}

fn governed_permissions(
    authority: &GovernedSessionAuthority,
    workspace: &Path,
    temp: &Path,
    program: &Path,
) -> KernelResult<PermissionProfile> {
    let workspace_access = if authority.workspace_mode == "candidate-write" {
        FileSystemAccessMode::Write
    } else {
        FileSystemAccessMode::Read
    };
    let mut entries = vec![
        FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            FileSystemAccessMode::Read,
        ),
        path_entry(workspace, workspace_access, "workspace")?,
        path_entry(temp, FileSystemAccessMode::Write, "command temp")?,
        path_entry(program, FileSystemAccessMode::Read, "command executable")?,
    ];
    if workspace_access == FileSystemAccessMode::Write {
        for metadata in [".git", ".agents", ".codex"] {
            entries.push(FileSystemSandboxEntry::skip_missing_path(
                FileSystemPath::from(absolute_path(&workspace.join(metadata), metadata)?),
                FileSystemAccessMode::Read,
            ));
        }
    }
    entries.extend(SENSITIVE_WORKSPACE_GLOBS.iter().map(|pattern| {
        FileSystemSandboxEntry::new(
            FileSystemPath::GlobPattern {
                pattern: project_roots_glob_pattern(Path::new(pattern)),
            },
            FileSystemAccessMode::Deny,
        )
    }));
    let workspace_absolute = absolute_path(workspace, "workspace")?;
    let file_system = FileSystemSandboxPolicy::restricted(entries)
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&workspace_absolute));
    Ok(PermissionProfile::from_runtime_permissions(
        &file_system,
        NetworkSandboxPolicy::Restricted,
    ))
}

fn governed_environment(
    requested: &HashMap<String, String>,
    temp: &Path,
) -> KernelResult<HashMap<String, String>> {
    let mut environment = HashMap::from([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), temp.to_string_lossy().into_owned()),
        ("TMPDIR".to_string(), temp.to_string_lossy().into_owned()),
        ("TMP".to_string(), temp.to_string_lossy().into_owned()),
        ("TEMP".to_string(), temp.to_string_lossy().into_owned()),
        ("CI".to_string(), "1".to_string()),
        ("NO_COLOR".to_string(), "1".to_string()),
    ]);
    for (name, value) in requested {
        if !matches!(name.as_str(), "LANG" | "LC_ALL")
            || value.is_empty()
            || value.len() > 64
            || !value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._@-".contains(character))
        {
            return Err(KernelFailure::new(
                "GOVERNED_COMMAND_POLICY_DENIED",
                "governed command environment is outside the explicit allowlist",
            ));
        }
        environment.insert(name.clone(), value.clone());
    }
    Ok(environment)
}

fn create_command_temp(
    options: &KernelOptions,
    request: &GovernedCommandRequest,
) -> KernelResult<PathBuf> {
    let sequence = COMMAND_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = options.home.join("governed-command-temp").join(format!(
        "{}-{}-{sequence}",
        request.session_id, request.command_id
    ));
    std::fs::create_dir_all(&temp).map_err(|error| {
        KernelFailure::new(
            "ENFORCEMENT_UNAVAILABLE",
            format!("governed command temp directory could not be created: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                KernelFailure::new(
                    "ENFORCEMENT_UNAVAILABLE",
                    format!("governed command temp permissions could not be set: {error}"),
                )
            },
        )?;
    }
    std::fs::canonicalize(&temp).map_err(|error| {
        KernelFailure::new(
            "ENFORCEMENT_UNAVAILABLE",
            format!("governed command temp directory could not be resolved: {error}"),
        )
    })
}

async fn collect_output(
    mut receiver: mpsc::Receiver<Vec<u8>>,
    total: Arc<AtomicUsize>,
    limit: usize,
    limit_reached: watch::Sender<bool>,
) -> Vec<u8> {
    let mut output = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        let previous = total.fetch_add(chunk.len(), Ordering::AcqRel);
        let remaining = limit.saturating_sub(previous);
        output.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            let _ = limit_reached.send(true);
            break;
        }
    }
    output
}

async fn wait_for_exit(exit: &mut tokio::sync::oneshot::Receiver<i32>) -> Option<i32> {
    tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, exit)
        .await
        .ok()
        .and_then(Result::ok)
}

async fn drain_output(task: &mut JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut *task).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => Vec::new(),
        Err(_) => {
            task.abort();
            Vec::new()
        }
    }
}

fn likely_sandbox_denial(
    sandbox: SandboxType,
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> bool {
    #[cfg(not(target_os = "linux"))]
    let _ = sandbox;
    if exit_code == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if sandbox == SandboxType::LinuxSeccomp && exit_code == 159 {
        return true;
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    )
    .to_ascii_lowercase();
    [
        "operation not permitted",
        "permission denied",
        "read-only file system",
        "sandbox: deny",
        "seccomp",
        "landlock",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

const fn sandbox_name(sandbox: SandboxType) -> &'static str {
    match sandbox {
        SandboxType::MacosSeatbelt => "macos-seatbelt",
        SandboxType::LinuxSeccomp => "linux-seccomp",
        SandboxType::None | SandboxType::WindowsRestrictedToken => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::GOVERNED_COMMAND_SCHEMA_VERSION;
    use super::GovernedCommandRequest;
    use super::validate_request;
    use crate::GovernedSessionAuthority;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn authority() -> GovernedSessionAuthority {
        GovernedSessionAuthority {
            schema_version: 1,
            role_id: "executor".to_string(),
            permission_preset: "candidate-write".to_string(),
            workspace_mode: "candidate-write".to_string(),
            workspace_root: "/fixture".to_string(),
            system_instructions: "fixture".to_string(),
            reasoning_effort: None,
            visible_tools: vec!["command.run".to_string()],
        }
    }

    fn request() -> GovernedCommandRequest {
        GovernedCommandRequest {
            schema_version: GOVERNED_COMMAND_SCHEMA_VERSION,
            session_id: "session-1".to_string(),
            command_id: "command-1".to_string(),
            tool: "command.run".to_string(),
            argv: vec!["/usr/bin/true".to_string()],
            cwd: PathBuf::from("/fixture"),
            environment: HashMap::new(),
            timeout: Duration::from_secs(1),
            output_limit_bytes: 1024,
        }
    }

    #[test]
    fn rejects_ungranted_tools_and_credential_shaped_arguments() {
        let mut denied = request();
        denied.tool = "candidate.patch".to_string();
        assert_eq!(
            validate_request(&authority(), &denied)
                .expect_err("tool must be denied")
                .code(),
            "GOVERNED_COMMAND_POLICY_DENIED"
        );
        let mut credential = request();
        credential.argv.push("TOKEN=fixture-secret".to_string());
        assert_eq!(
            validate_request(&authority(), &credential)
                .expect_err("credential argument must be denied")
                .code(),
            "GOVERNED_COMMAND_POLICY_DENIED"
        );
    }
}
