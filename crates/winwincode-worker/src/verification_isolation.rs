// SPDX-License-Identifier: Apache-2.0

//! Execution-time protection for verification inputs and private scratch.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::{
    FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
    FileSystemSpecialPath, NetworkSandboxPolicy,
};
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_pty::process_group::kill_process_group;
use sandboxing::{
    SandboxCommand, SandboxManager, SandboxTransformRequest, SandboxType, SandboxablePreference,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};

/// Version of the protected input and writable scratch contract.
pub const PROTECTED_INPUT_SCOPE_VERSION: u8 = 1;

const PROTECTED_INPUT_CATEGORIES: &[&str] = &[
    "source",
    "tests",
    "build-scripts",
    "lockfiles",
    "controlled-config",
];

/// Stable error returned by verification isolation setup or execution.
#[derive(Debug)]
pub struct IsolationError {
    message: String,
}

impl IsolationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for IsolationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IsolationError {}

/// Versioned protected source contract. Enforcement covers the whole checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtectedInputScope {
    version: u8,
    root: PathBuf,
    declared_paths: Vec<String>,
    digest: String,
}

#[derive(Serialize)]
struct ProtectedInputScopeDigest<'a> {
    version: u8,
    root: &'a Path,
    categories: &'static [&'static str],
    declared_paths: &'a [String],
}

impl ProtectedInputScope {
    /// Declares the immutable input root and additional contract paths.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical roots and declared paths that escape the root.
    pub fn new(root: impl AsRef<Path>, declared_paths: &[String]) -> Result<Self, IsolationError> {
        let root = canonical_directory(root.as_ref(), "protected input root")?;
        let mut normalized = declared_paths.to_vec();
        normalized.sort();
        normalized.dedup();
        for relative in &normalized {
            resolve_below(&root, Path::new(relative), "protected input declaration")?;
        }
        let mut value = serde_json::to_value(ProtectedInputScopeDigest {
            version: PROTECTED_INPUT_SCOPE_VERSION,
            root: root.as_path(),
            categories: PROTECTED_INPUT_CATEGORIES,
            declared_paths: &normalized,
        })
        .map_err(|_| IsolationError::new("protected input scope cannot be encoded"))?;
        if let Some(object) = value.as_object_mut() {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by_key(|(key, _)| key.as_str());
            value = serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            );
        }
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| IsolationError::new("protected input scope cannot be hashed"))?;
        Ok(Self {
            version: PROTECTED_INPUT_SCOPE_VERSION,
            root,
            declared_paths: normalized,
            digest: format!("sha256:{:x}", Sha256::digest(bytes)),
        })
    }

    pub const fn version(&self) -> u8 {
        self.version
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn categories(&self) -> &'static [&'static str] {
        PROTECTED_INPUT_CATEGORIES
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Private writable directories allocated to one verification profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationScratch {
    id: String,
    root: PathBuf,
    home: PathBuf,
    temporary: PathBuf,
    build: PathBuf,
    cache: PathBuf,
    output: PathBuf,
}

impl VerificationScratch {
    /// Allocates a collision-free scratch tree below a job-private workspace.
    ///
    /// # Errors
    ///
    /// Rejects invalid run identifiers or filesystem failures.
    pub fn create(
        workspace_root: impl AsRef<Path>,
        run_identity: &str,
        ordinal: usize,
    ) -> Result<Self, IsolationError> {
        let base = canonical_directory(workspace_root.as_ref(), "verification scratch base")?;
        if run_identity.is_empty() || run_identity.len() > 4096 {
            return Err(IsolationError::new(
                "verification scratch identity is invalid",
            ));
        }
        let mut identity_digest = Sha256::new();
        identity_digest.update([PROTECTED_INPUT_SCOPE_VERSION]);
        identity_digest.update(run_identity.as_bytes());
        identity_digest.update(ordinal.to_be_bytes());
        let id = format!("vs-{:x}", identity_digest.finalize());
        let pool = base.join("verification");
        fs::create_dir_all(&pool).map_err(|error| {
            IsolationError::new(format!(
                "verification scratch pool cannot be created: {error}"
            ))
        })?;
        let root = pool.join(&id);
        fs::create_dir(&root).map_err(|error| {
            IsolationError::new(format!("verification scratch cannot be created: {error}"))
        })?;
        let scratch = Self {
            id,
            home: root.join("home"),
            temporary: root.join("tmp"),
            build: root.join("build"),
            cache: root.join("cache"),
            output: root.join("output"),
            root,
        };
        for directory in [
            scratch.home.as_path(),
            scratch.temporary.as_path(),
            scratch.build.as_path(),
            scratch.cache.as_path(),
            scratch.output.as_path(),
        ] {
            fs::create_dir(directory).map_err(|error| {
                IsolationError::new(format!(
                    "verification scratch child cannot be created: {error}"
                ))
            })?;
        }
        Ok(scratch)
    }

    /// Allocates an independent sibling scratch from the same workspace pool.
    ///
    /// # Errors
    ///
    /// Rejects invalid scratch roots or run identities and filesystem failures.
    pub fn sibling(&self, run_id: &str, ordinal: usize) -> Result<Self, IsolationError> {
        let base = self
            .root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| IsolationError::new("verification scratch has no workspace root"))?;
        Self::create(base, run_id, ordinal.saturating_add(1))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    pub fn build(&self) -> &Path {
        &self.build
    }

    pub fn cache(&self) -> &Path {
        &self.cache
    }

    pub fn output(&self) -> &Path {
        &self.output
    }
}

/// Host-enforced protection capability discovered by a real mutation probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtectionCapability {
    Available { backend: String },
    Unavailable { backend: String, reason: String },
}

impl ProtectionCapability {
    pub const fn allows_strong_pass(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// One verification profile bound to immutable input and isolated scratch.
#[derive(Clone, Debug)]
pub struct VerificationIsolation {
    scope: ProtectedInputScope,
    scratch: VerificationScratch,
    helper_executable: Option<PathBuf>,
    capability: ProtectionCapability,
}

/// Complete output from an isolated test-only or capability command.
#[derive(Debug)]
pub struct IsolatedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub overflowed: bool,
}

impl IsolatedOutput {
    pub fn stderr_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

impl VerificationIsolation {
    /// Builds the host policy and proves the enforcement with live mutation attempts.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope or scratch. An unsupported host is retained as a
    /// fail-closed capability instead of permitting verification execution.
    pub async fn open(
        scope: ProtectedInputScope,
        scratch: VerificationScratch,
        helper_executable: Option<PathBuf>,
    ) -> Result<Self, IsolationError> {
        let scratch_root = canonical_directory(&scratch.root, "verification scratch root")?;
        if paths_overlap(&scope.root, &scratch_root) {
            return Err(IsolationError::new(
                "verification scratch overlaps protected input",
            ));
        }
        let scratch = VerificationScratch {
            home: scratch_root.join("home"),
            temporary: scratch_root.join("tmp"),
            build: scratch_root.join("build"),
            cache: scratch_root.join("cache"),
            output: scratch_root.join("output"),
            root: scratch_root,
            ..scratch
        };
        let mut candidate = Self {
            scope,
            scratch,
            helper_executable,
            capability: ProtectionCapability::Unavailable {
                backend: platform_backend_name().to_owned(),
                reason: "capability probe has not completed".to_owned(),
            },
        };
        candidate.capability = match Box::pin(candidate.probe()).await {
            Ok(()) => ProtectionCapability::Available {
                backend: platform_backend_name().to_owned(),
            },
            Err(error) => ProtectionCapability::Unavailable {
                backend: platform_backend_name().to_owned(),
                reason: error.to_string(),
            },
        };
        Ok(candidate)
    }

    pub const fn capability(&self) -> &ProtectionCapability {
        &self.capability
    }

    pub const fn scope(&self) -> &ProtectedInputScope {
        &self.scope
    }

    pub const fn scratch(&self) -> &VerificationScratch {
        &self.scratch
    }

    pub const fn allows_strong_pass_with(capability: &ProtectionCapability) -> bool {
        capability.allows_strong_pass()
    }

    /// Runs one no-shell command with all descendants under the host policy.
    ///
    /// # Errors
    ///
    /// Rejects unavailable protection, path escapes, or process launch failure.
    pub async fn run(
        &self,
        argv: &[String],
        working_directory: &Path,
        environment: HashMap<String, String>,
        timeout: Duration,
        output_limit_bytes: usize,
    ) -> Result<IsolatedOutput, IsolationError> {
        let command = self.command(argv, working_directory, environment)?;
        Box::pin(run_isolated_command(command, timeout, output_limit_bytes)).await
    }

    /// Builds the exact production command wrapper used by validation execution.
    ///
    /// # Errors
    ///
    /// Rejects unavailable protection, empty argv, path escapes, or policy errors.
    pub fn command(
        &self,
        argv: &[String],
        working_directory: &Path,
        environment: HashMap<String, String>,
    ) -> Result<Command, IsolationError> {
        if !self.capability.allows_strong_pass() {
            return Err(IsolationError::new(
                "protected input enforcement is unavailable",
            ));
        }
        self.command_without_capability_gate(argv, working_directory, environment)
    }

    fn command_without_capability_gate(
        &self,
        argv: &[String],
        working_directory: &Path,
        mut environment: HashMap<String, String>,
    ) -> Result<Command, IsolationError> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| IsolationError::new("verification command argv must not be empty"))?;
        let working_directory =
            canonical_directory(working_directory, "verification command working directory")?;
        if !working_directory.starts_with(&self.scope.root) {
            return Err(IsolationError::new(
                "verification working directory leaves the protected input root",
            ));
        }
        let request =
            self.sandbox_request(program, arguments, &working_directory, &mut environment)?;
        let (program, arguments) = request
            .command
            .split_first()
            .ok_or_else(|| IsolationError::new("sandbox launcher is empty"))?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(request.cwd.to_abs_path().map_err(|error| {
                IsolationError::new(format!("sandbox cwd cannot be recovered: {error}"))
            })?)
            .env_clear()
            .envs(request.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(arg0) = request.arg0 {
            command.arg0(arg0);
        }
        Ok(command)
    }

    fn sandbox_request(
        &self,
        program: &str,
        arguments: &[String],
        working_directory: &Path,
        environment: &mut HashMap<String, String>,
    ) -> Result<sandboxing::SandboxExecRequest, IsolationError> {
        let policy = self.permission_profile()?;
        let manager = SandboxManager::new();
        let sandbox = manager.select_initial(
            &policy,
            SandboxablePreference::Require,
            WindowsSandboxLevel::Disabled,
            false,
        );
        if sandbox == SandboxType::None {
            return Err(IsolationError::new(
                "host cannot enforce protected verification input",
            ));
        }
        // The helper discovers the platform sandbox launcher through PATH,
        // including during the capability probe before a command environment exists.
        environment.entry("PATH".to_owned()).or_insert_with(|| {
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned())
        });
        for (name, path) in [
            ("HOME", self.scratch.home.as_path()),
            ("TMPDIR", self.scratch.temporary.as_path()),
            ("TEMP", self.scratch.temporary.as_path()),
            ("TMP", self.scratch.temporary.as_path()),
        ] {
            environment.insert(name.to_owned(), path_string(path)?);
        }
        environment.insert("WWC_SCRATCH".to_owned(), path_string(&self.scratch.root)?);
        environment.insert(
            "WWC_CACHE_DIR".to_owned(),
            path_string(&self.scratch.cache)?,
        );
        environment.insert(
            "WWC_OUTPUT_DIR".to_owned(),
            path_string(&self.scratch.output)?,
        );
        environment.insert(
            "CARGO_TARGET_DIR".to_owned(),
            path_string(&self.scratch.build)?,
        );
        environment.insert(
            "PYTHONPYCACHEPREFIX".to_owned(),
            path_string(&self.scratch.cache.join("python"))?,
        );
        environment.insert(
            "npm_config_cache".to_owned(),
            path_string(&self.scratch.cache.join("npm"))?,
        );
        let mut command = vec![program.to_owned()];
        command.extend(arguments.iter().cloned());
        manager
            .transform(SandboxTransformRequest {
                command: SandboxCommand {
                    program: program.into(),
                    args: arguments.to_vec(),
                    cwd: PathUri::from_abs_path(&absolute_path(working_directory)?),
                    env: environment.clone(),
                    managed_network: None,
                    additional_permissions: None,
                },
                permissions: &policy,
                sandbox,
                enforce_managed_network: false,
                environment_id: None,
                network: None,
                sandbox_policy_cwd: &PathUri::from_abs_path(&absolute_path(&self.scope.root)?),
                codex_linux_sandbox_exe: self.helper_executable.as_deref(),
                use_legacy_landlock: false,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
            })
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    fn permission_profile(&self) -> Result<PermissionProfile, IsolationError> {
        let root = FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        };
        let scratch = FileSystemPath::from(absolute_path(&self.scratch.root)?);
        let policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry::new(root, FileSystemAccessMode::Read),
            FileSystemSandboxEntry::new(scratch, FileSystemAccessMode::Write),
            // Linux bubblewrap mounts its own writable /dev. Treating a device
            // file as a writable root also invents directory exclusions below it.
            #[cfg(not(target_os = "linux"))]
            FileSystemSandboxEntry::new(
                FileSystemPath::from(absolute_path(Path::new("/dev/null"))?),
                FileSystemAccessMode::Write,
            ),
        ]);
        Ok(PermissionProfile::from_runtime_permissions(
            &policy,
            NetworkSandboxPolicy::Restricted,
        ))
    }

    async fn probe(&mut self) -> Result<(), IsolationError> {
        // Bubblewrap versions without --argv0 re-exec the helper by its first
        // argument. Keep that absolute alias outside the writable scratch.
        #[cfg(target_os = "linux")]
        if let Some(helper) = &mut self.helper_executable {
            let source = fs::canonicalize(&*helper)
                .map_err(|error| IsolationError::new(format!("sandbox helper: {error}")))?;
            let directory = self.scratch.root.with_extension("launcher");
            fs::create_dir(&directory)
                .map_err(|error| IsolationError::new(format!("sandbox launcher: {error}")))?;
            let alias = directory.join("codex-linux-sandbox");
            std::os::unix::fs::symlink(source, &alias)
                .map_err(|error| IsolationError::new(format!("sandbox launcher alias: {error}")))?;
            *helper = alias;
        }
        // Probe the same read-only policy outside both the sealed input and
        // writable scratch. Never create/delete probe files in a Snapshot:
        // it may already contain those names or serve concurrent verifiers.
        let probe_root = self.scratch.root.with_extension("probe");
        fs::create_dir(&probe_root)
            .map_err(|error| IsolationError::new(format!("probe directory: {error}")))?;
        let result = self.probe_at(&probe_root).await;
        let cleanup = fs::remove_dir_all(&probe_root)
            .map_err(|error| IsolationError::new(format!("probe cleanup: {error}")));
        result.and(cleanup)
    }

    async fn probe_at(&self, probe_root: &Path) -> Result<(), IsolationError> {
        let source = probe_root.join("source");
        let created = probe_root.join("created");
        let child = probe_root.join("child");
        let grandchild = probe_root.join("grandchild");
        fs::write(&source, b"sealed").map_err(|error| {
            IsolationError::new(format!("isolation probe input cannot be created: {error}"))
        })?;
        let script = r#"set -eu
blocked() { ! "$@"; }
blocked /bin/sh -c 'printf x > "$WWC_PROBE_CREATED"'
blocked /bin/sh -c 'printf x > "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'rm -f "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'mv "$WWC_PROBE_SOURCE" "$WWC_PROBE_SOURCE.moved"'
blocked /bin/sh -c 'printf x > "$WWC_SCRATCH/replacement" && mv "$WWC_SCRATCH/replacement" "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'cp "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/original" && printf changed > "$WWC_PROBE_SOURCE" && cp "$WWC_SCRATCH/original" "$WWC_PROBE_SOURCE"'
blocked /bin/sh -c 'ln -s "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/source-link" && printf changed > "$WWC_SCRATCH/source-link"'
blocked /bin/sh -c 'ln "$WWC_PROBE_SOURCE" "$WWC_SCRATCH/source-hardlink" && printf changed > "$WWC_SCRATCH/source-hardlink"'
blocked /bin/sh -c '/bin/sh -c '\''printf x > "$WWC_PROBE_CHILD"'\'''
blocked /bin/sh -c '/bin/sh -c '\''printf x > "$WWC_PROBE_GRANDCHILD"'\'''
blocked /bin/sh -c '/bin/sh -c '\''printf changed > "$WWC_PROBE_SOURCE"'\'''
printf ok > "$WWC_SCRATCH/probe-ok"
test "$(cat "$WWC_PROBE_SOURCE")" = sealed
test ! -e "$WWC_PROBE_CREATED"
test ! -e "$WWC_PROBE_CHILD"
test ! -e "$WWC_PROBE_GRANDCHILD"
"#;
        let mut environment = HashMap::new();
        environment.insert("WWC_PROBE_SOURCE".to_owned(), path_string(&source)?);
        environment.insert("WWC_PROBE_CREATED".to_owned(), path_string(&created)?);
        environment.insert("WWC_PROBE_CHILD".to_owned(), path_string(&child)?);
        environment.insert("WWC_PROBE_GRANDCHILD".to_owned(), path_string(&grandchild)?);
        let command = self.command_without_capability_gate(
            &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            &self.scope.root,
            environment,
        )?;
        let output = Box::pin(run_isolated_command(
            command,
            Duration::from_secs(10),
            1024 * 1024,
        ))
        .await?;
        if !output.status.success() || output.timed_out || output.overflowed {
            return Err(IsolationError::new(format!(
                "isolation mutation probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }
}

async fn run_isolated_command(
    mut command: Command,
    timeout: Duration,
    output_limit_bytes: usize,
) -> Result<IsolatedOutput, IsolationError> {
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        IsolationError::new(format!(
            "isolated verification command cannot start: {error}"
        ))
    })?;
    let process_group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| IsolationError::new("isolated command stdout cannot be captured"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| IsolationError::new("isolated command stderr cannot be captured"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut overflowed = false;
    let read = async {
        let mut stdout_open = true;
        let mut stderr_open = true;
        loop {
            let mut out_chunk = [0_u8; 8192];
            let mut err_chunk = [0_u8; 8192];
            if !stdout_open && !stderr_open {
                break;
            }
            tokio::select! {
                left = stdout_pipe.read(&mut out_chunk), if stdout_open => {
                    let left = left.map_err(|error| {
                        IsolationError::new(format!("isolated command stdout failed: {error}"))
                    })?;
                    if left == 0 {
                        stdout_open = false;
                    } else {
                        overflowed |= extend_bounded(&mut stdout, &out_chunk[..left], output_limit_bytes);
                    }
                }
                right = stderr_pipe.read(&mut err_chunk), if stderr_open => {
                    let right = right.map_err(|error| {
                        IsolationError::new(format!("isolated command stderr failed: {error}"))
                    })?;
                    if right == 0 {
                        stderr_open = false;
                    } else {
                        overflowed |= extend_bounded(&mut stderr, &err_chunk[..right], output_limit_bytes);
                    }
                }
            }
            if overflowed {
                break;
            }
        }
        Ok::<(), IsolationError>(())
    };
    let read_result = tokio::time::timeout(timeout, Box::pin(read)).await;
    let timed_out = read_result.is_err();
    let read_error = if timed_out {
        None
    } else {
        match read_result {
            Ok(result) => result.err(),
            Err(_) => unreachable!(),
        }
    };
    let status = if timed_out || overflowed || read_error.is_some() {
        terminate_process_group(&mut child, process_group).await?
    } else {
        let status = child.wait().await.map_err(|error| {
            IsolationError::new(format!("isolated command cannot be reaped: {error}"))
        })?;
        if let Some(process_group) = process_group {
            kill_process_group_by_id(process_group)?;
        }
        status
    };
    if let Some(error) = read_error {
        return Err(error);
    }
    Ok(IsolatedOutput {
        status,
        stdout,
        stderr,
        timed_out,
        overflowed,
    })
}

fn extend_bounded(target: &mut Vec<u8>, source: &[u8], output_limit_bytes: usize) -> bool {
    let remaining = output_limit_bytes.saturating_sub(target.len());
    let bound = source.len().min(remaining);
    target.extend_from_slice(&source[..bound]);
    bound < source.len()
}

async fn terminate_process_group(
    child: &mut Child,
    process_group: Option<i32>,
) -> Result<std::process::ExitStatus, IsolationError> {
    if let Some(process_group) = process_group {
        kill_process_group_by_id(process_group)?;
    }
    let _ = child.start_kill();
    let status = child.wait().await.map_err(|error| {
        IsolationError::new(format!("isolated command cannot be reaped: {error}"))
    })?;
    Ok(status)
}

fn kill_process_group_by_id(process_group: i32) -> Result<(), IsolationError> {
    let process_group = u32::try_from(process_group)
        .map_err(|_| IsolationError::new("isolated command process group is invalid"))?;
    kill_process_group(process_group).map_err(|error| {
        IsolationError::new(format!(
            "isolated command process group cannot be terminated: {error}"
        ))
    })
}

const fn platform_backend_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos-seatbelt-v1"
    } else if cfg!(target_os = "linux") {
        "linux-codex-sandbox-v1"
    } else {
        "unsupported"
    }
}

fn absolute_path(path: &Path) -> Result<AbsolutePathBuf, IsolationError> {
    AbsolutePathBuf::try_from(path.to_path_buf()).map_err(|error| {
        IsolationError::new(format!("absolute isolation path is invalid: {error}"))
    })
}

fn path_string(path: &Path) -> Result<String, IsolationError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| IsolationError::new("verification path is not valid UTF-8"))
}

fn canonical_directory(path: &Path, context: &str) -> Result<PathBuf, IsolationError> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| IsolationError::new(format!("{context} cannot be resolved: {error}")))?;
    if !canonical.is_dir() {
        return Err(IsolationError::new(format!("{context} is not a directory")));
    }
    Ok(canonical)
}

fn resolve_below(root: &Path, relative: &Path, context: &str) -> Result<PathBuf, IsolationError> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(IsolationError::new(format!("{context} is not a safe path")));
    }
    let mut resolved = root.to_path_buf();
    for component in relative.components() {
        resolved.push(component);
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(IsolationError::new(format!(
                    "{context} contains a symbolic link"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(IsolationError::new(format!(
                    "{context} cannot be resolved: {error}"
                )));
            }
        }
    }
    let mut existing_ancestor = resolved.clone();
    while !existing_ancestor.exists() {
        if !existing_ancestor.pop() {
            return Err(IsolationError::new(format!("{context} leaves its root")));
        }
    }
    let canonical = fs::canonicalize(&existing_ancestor)
        .map_err(|error| IsolationError::new(format!("{context} cannot be resolved: {error}")))?;
    if !canonical.starts_with(root) {
        return Err(IsolationError::new(format!("{context} leaves its root")));
    }
    let tail = resolved
        .strip_prefix(existing_ancestor)
        .map_err(|_| IsolationError::new(format!("{context} leaves its root")))?;
    Ok(canonical.join(tail))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
