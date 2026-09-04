// SPDX-License-Identifier: Apache-2.0

//! Bounded process execution for deterministic Writer and read-only Validation phases.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant as MonotonicInstant};

use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant as TokioInstant, sleep_until, timeout};
use winwincode_domain::{Sha256Digest, WorkspaceRevision};
use winwincode_execution_port::generated::{
    ArtifactReference, DiagnosticParserVersion, ValidationCheckStatus, ValidationCheckSummary,
    ValidationCommandPhase, ValidationCommandSpec, ValidationEnvironmentName,
    ValidationProfileName, ValidationProfileSelection, ValidationReceipt, ValidationReceiptStatus,
};
use winwincode_execution_port::validation_config::{
    ParsedValidationConfiguration, ValidationConfigurationError, resolve_validation_profile,
    validate_validation_receipt_binding,
};

/// Maximum combined stdout and stderr bytes retained for one command.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Default command deadline used by the canonical validation configuration.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_ARGUMENTS: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const CANCELLATION_POLL: Duration = Duration::from_millis(20);
const ALLOWED_ENVIRONMENT: [&str; 8] = [
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "HOME",
    "LANG",
    "LC_ALL",
    "PATH",
    "RUSTUP_HOME",
    "TMPDIR",
];

/// Stable command rejection or execution category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseProcessErrorCode {
    InvalidCommand,
    SandboxUnavailable,
    SpawnFailed,
    WaitFailed,
}

/// Secret-free process execution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhaseProcessError {
    pub code: PhaseProcessErrorCode,
    message: &'static str,
}

impl fmt::Display for PhaseProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PhaseProcessError {}

/// One exact argv-based command from the canonical validation configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhaseCommand {
    pub name: String,
    pub diagnostic_parser_version: Option<DiagnosticParserVersion>,
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub working_directory: PathBuf,
    pub scratch_directory: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub access: PhaseAccess,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// Filesystem authority granted to one phase command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseAccess {
    Writer,
    ReadOnlyValidation,
}

/// Cooperative cancellation shared by the Worker and one running command.
#[derive(Clone, Debug, Default)]
pub struct PhaseCancellation(Arc<AtomicBool>);

impl PhaseCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Terminal command state after the complete process group has exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseProcessStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
}

/// Bounded, raw-output-free result retained by the Worker journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PhaseProcessReceipt {
    pub name: String,
    pub status: PhaseProcessStatus,
    pub exit_code: Option<i32>,
    pub stdout_digest: Sha256Digest,
    pub stderr_digest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_artifact_ref: Option<ArtifactReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_artifact_ref: Option<ArtifactReference>,
    pub output_bytes: usize,
    pub duration_millis: u64,
}

/// One bounded process result with the exact captured bytes kept outside the durable receipt.
///
/// Callers must place `stdout` and `stderr` in the private Artifact store before retaining a
/// diagnostic snapshot. The receipt remains raw-output-free and safe for the `SQLite` journal.
#[derive(Clone, Debug, PartialEq)]
pub struct PhaseProcessExecution {
    pub receipt: PhaseProcessReceipt,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Executable, byte-identified command selection rebuilt from one explicit config.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfiguredPhasePlan {
    pub selection: ValidationProfileSelection,
    pub writer_commands: Vec<PhaseCommand>,
    /// Canonical union of companion paths authorized by selected Writer commands.
    pub allowed_writer_paths: Vec<String>,
    pub validation_commands: Vec<PhaseCommand>,
}

impl ConfiguredPhasePlan {
    /// Rebuilds the exact selected commands rather than trusting a caller-supplied list.
    ///
    /// # Errors
    ///
    /// Rejects unknown profiles, invalid changed paths, missing executables, unsafe
    /// environment paths, or any selected command that cannot be made absolute.
    pub fn from_explicit_configuration(
        parsed: &ParsedValidationConfiguration,
        requested_profile: &str,
        changed_paths: &[String],
        workspace_root: &Path,
        scratch_directory: &Path,
        trusted_tool_path: &OsStr,
        trusted_rustup_home: Option<&Path>,
    ) -> Result<Self, ConfiguredPhasePlanError> {
        let selection = resolve_validation_profile(Some(parsed), requested_profile, changed_paths)?;
        let commands = &parsed.configuration().commands;
        let mut writer_commands = Vec::new();
        let mut allowed_writer_paths = Vec::new();
        let mut validation_commands = Vec::new();
        for command_id in &selection.command_ids {
            let command = commands
                .iter()
                .find(|command| &command.id == command_id)
                .ok_or(ConfiguredPhasePlanError::InvalidCommand)?;
            let phase_command = phase_command_from_spec(
                workspace_root,
                scratch_directory,
                trusted_tool_path,
                trusted_rustup_home,
                command,
            )?;
            match phase_command.access {
                PhaseAccess::Writer => {
                    allowed_writer_paths.extend(command.allowed_companion_paths.iter().cloned());
                    writer_commands.push(phase_command);
                }
                PhaseAccess::ReadOnlyValidation => validation_commands.push(phase_command),
            }
        }
        allowed_writer_paths.sort();
        allowed_writer_paths.dedup();
        Ok(Self {
            selection,
            writer_commands,
            allowed_writer_paths,
            validation_commands,
        })
    }

    #[must_use]
    pub fn profile(&self) -> &ValidationProfileName {
        &self.selection.profile
    }
}

/// Stable failure while converting canonical configuration into executable commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfiguredPhasePlanError {
    Configuration,
    InvalidCommand,
}

impl fmt::Display for ConfiguredPhasePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("configured validation phase is invalid")
    }
}

impl std::error::Error for ConfiguredPhasePlanError {}

impl From<ValidationConfigurationError> for ConfiguredPhasePlanError {
    fn from(_: ValidationConfigurationError) -> Self {
        Self::Configuration
    }
}

impl From<PhaseProcessError> for ConfiguredPhasePlanError {
    fn from(_: PhaseProcessError) -> Self {
        Self::InvalidCommand
    }
}

#[derive(Debug)]
struct BoundedOutput {
    digest: Sha256Digest,
    bytes: usize,
    exceeded: bool,
    content: Vec<u8>,
}

/// Runs canonical validation commands in isolated process groups.
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseProcessRunner;

impl PhaseProcessRunner {
    /// Executes one command with an empty inherited environment and network denied.
    ///
    /// # Errors
    ///
    /// Rejects non-absolute executables, foreign working directories, unsupported
    /// environment entries, unavailable network isolation, and process I/O failures.
    pub async fn execute(
        &self,
        workspace_root: &Path,
        command: &PhaseCommand,
        cancellation: &PhaseCancellation,
    ) -> Result<PhaseProcessReceipt, PhaseProcessError> {
        self.execute_with_output(workspace_root, command, cancellation)
            .await
            .map(|execution| execution.receipt)
    }

    /// Executes one command and returns the bounded raw streams for Artifact retention.
    ///
    /// # Errors
    ///
    /// Applies the same fail-closed command, sandbox, network, and process-reaping checks as
    /// [`Self::execute`]. Output beyond the configured cap is never retained in memory.
    pub async fn execute_with_output(
        &self,
        workspace_root: &Path,
        command: &PhaseCommand,
        cancellation: &PhaseCancellation,
    ) -> Result<PhaseProcessExecution, PhaseProcessError> {
        let validated = ValidatedCommand::new(workspace_root, command)?;
        let started = MonotonicInstant::now();
        let mut child = spawn_network_denied(&validated)?;
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(wait_failed)?;
        let stdout = child.stdout.take().ok_or_else(wait_failed)?;
        let stderr = child.stderr.take().ok_or_else(wait_failed)?;
        let output_bytes = Arc::new(AtomicUsize::new(0));
        let output_exceeded = Arc::new(AtomicBool::new(false));
        let stdout_task = tokio::spawn(read_bounded(
            stdout,
            command.max_output_bytes,
            Arc::clone(&output_bytes),
            Arc::clone(&output_exceeded),
        ));
        let stderr_task = tokio::spawn(read_bounded(
            stderr,
            command.max_output_bytes,
            Arc::clone(&output_bytes),
            Arc::clone(&output_exceeded),
        ));
        let deadline = TokioInstant::now() + command.timeout;
        let status =
            wait_for_terminal(&mut child, pid, deadline, cancellation, &output_exceeded).await?;
        let stdout = stdout_task.await.map_err(|_| wait_failed())??;
        let stderr = stderr_task.await.map_err(|_| wait_failed())??;
        let output_bytes = stdout.bytes.saturating_add(stderr.bytes);
        let status =
            if stdout.exceeded || stderr.exceeded || output_bytes > command.max_output_bytes {
                PhaseProcessStatus::OutputLimitExceeded
            } else {
                status
            };
        Ok(PhaseProcessExecution {
            receipt: PhaseProcessReceipt {
                name: command.name.clone(),
                status,
                exit_code: child
                    .try_wait()
                    .map_err(|_| wait_failed())?
                    .and_then(|value| value.code()),
                stdout_digest: stdout.digest,
                stderr_digest: stderr.digest,
                stdout_artifact_ref: None,
                stderr_artifact_ref: None,
                output_bytes,
                duration_millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
            stdout: stdout.content,
            stderr: stderr.content,
        })
    }

    /// Runs the read-only command suffix and constructs one exact revision-bound receipt.
    ///
    /// Ordinary failed checks continue so every configured parser contributes to the snapshot.
    /// Cancellation and infrastructure limits stop the remaining command suffix.
    ///
    /// # Errors
    ///
    /// Rejects a Writer command, more than 64 checks, invalid command execution,
    /// or a generated receipt that does not bind the validated tree.
    pub async fn validate(
        &self,
        workspace_root: &Path,
        commands: &[PhaseCommand],
        profile: &ValidationProfileName,
        revision: &WorkspaceRevision,
        cancellation: &PhaseCancellation,
    ) -> Result<ValidationReceipt, PhaseProcessError> {
        if commands.is_empty()
            || commands.len() > 64
            || commands
                .iter()
                .any(|command| command.access != PhaseAccess::ReadOnlyValidation)
        {
            return Err(invalid_command());
        }
        let mut results = Vec::with_capacity(commands.len());
        for command in commands {
            let result = self.execute(workspace_root, command, cancellation).await?;
            let terminal = matches!(
                result.status,
                PhaseProcessStatus::TimedOut
                    | PhaseProcessStatus::Cancelled
                    | PhaseProcessStatus::OutputLimitExceeded
            );
            results.push(result);
            if terminal {
                break;
            }
        }
        Self::validation_receipt(profile, revision, &results)
    }

    /// Builds the canonical revision-bound receipt from an executed command prefix.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversized prefix, commands after cancellation/infrastructure,
    /// or a receipt that does not satisfy the generated contract.
    pub fn validation_receipt(
        profile: &ValidationProfileName,
        revision: &WorkspaceRevision,
        results: &[PhaseProcessReceipt],
    ) -> Result<ValidationReceipt, PhaseProcessError> {
        if results.is_empty()
            || results.len() > 64
            || results[..results.len().saturating_sub(1)]
                .iter()
                .any(|result| {
                    matches!(
                        result.status,
                        PhaseProcessStatus::TimedOut
                            | PhaseProcessStatus::Cancelled
                            | PhaseProcessStatus::OutputLimitExceeded
                    )
                })
        {
            return Err(invalid_command());
        }
        let mut checks = Vec::with_capacity(results.len());
        let mut duration_millis = 0_i64;
        let mut receipt_status = ValidationReceiptStatus::Passed;
        for result in results {
            duration_millis = duration_millis
                .saturating_add(i64::try_from(result.duration_millis).unwrap_or(i64::MAX));
            let (status, summary, terminal) = validation_check_status(result.status);
            checks.push(ValidationCheckSummary {
                diagnostic_digest: Some(phase_diagnostic_digest(result)),
                name: result.name.clone(),
                status,
                summary: summary.to_owned(),
            });
            if let Some(status) = terminal {
                receipt_status = status;
            }
        }
        let exact_result = Some(revision.clone());
        let artifact_refs = results
            .iter()
            .flat_map(|result| {
                [
                    result.stdout_artifact_ref.clone(),
                    result.stderr_artifact_ref.clone(),
                ]
                .into_iter()
                .flatten()
            })
            .collect();
        let receipt = ValidationReceipt {
            artifact_refs,
            base_revision: revision.clone(),
            checks,
            duration_millis: duration_millis.min(604_800_000),
            profile: profile.clone(),
            result_revision: exact_result.clone(),
            status: receipt_status,
        };
        validate_validation_receipt_binding(&receipt, revision, exact_result.as_ref())
            .map_err(|_| invalid_command())?;
        Ok(receipt)
    }
}

fn validation_check_status(
    status: PhaseProcessStatus,
) -> (
    ValidationCheckStatus,
    &'static str,
    Option<ValidationReceiptStatus>,
) {
    match status {
        PhaseProcessStatus::Passed => (ValidationCheckStatus::Passed, "check passed", None),
        PhaseProcessStatus::Failed => (
            ValidationCheckStatus::Failed,
            "check failed",
            Some(ValidationReceiptStatus::Failed),
        ),
        PhaseProcessStatus::Cancelled => (
            ValidationCheckStatus::Cancelled,
            "check cancelled",
            Some(ValidationReceiptStatus::Cancelled),
        ),
        PhaseProcessStatus::TimedOut | PhaseProcessStatus::OutputLimitExceeded => (
            ValidationCheckStatus::InfrastructureError,
            "check infrastructure limit reached",
            Some(ValidationReceiptStatus::InfrastructureError),
        ),
    }
}

fn phase_diagnostic_digest(receipt: &PhaseProcessReceipt) -> Sha256Digest {
    let mut digest = Sha256::new();
    for value in [
        receipt.stdout_digest.0.as_bytes(),
        receipt.stderr_digest.0.as_bytes(),
    ] {
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value);
    }
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn phase_command_from_spec(
    workspace_root: &Path,
    scratch_directory: &Path,
    trusted_tool_path: &OsStr,
    trusted_rustup_home: Option<&Path>,
    spec: &ValidationCommandSpec,
) -> Result<PhaseCommand, PhaseProcessError> {
    let mut environment = spec
        .environment
        .iter()
        .filter(|variable| {
            matches!(
                variable.name,
                ValidationEnvironmentName::Lang | ValidationEnvironmentName::LcAll
            )
        })
        .map(|variable| {
            (
                OsString::from(environment_name(&variable.name)),
                OsString::from(&variable.value),
            )
        })
        .collect::<BTreeMap<_, _>>();
    environment.insert(OsString::from("PATH"), trusted_tool_path.to_os_string());
    environment.insert(
        OsString::from("HOME"),
        scratch_directory.as_os_str().to_os_string(),
    );
    environment.insert(
        OsString::from("TMPDIR"),
        scratch_directory.as_os_str().to_os_string(),
    );
    environment.insert(
        OsString::from("CARGO_HOME"),
        scratch_directory.join("cargo-home").into_os_string(),
    );
    environment.insert(
        OsString::from("CARGO_TARGET_DIR"),
        scratch_directory.join("cargo-target").into_os_string(),
    );
    if let Some(rustup_home) = trusted_rustup_home {
        environment.insert(
            OsString::from("RUSTUP_HOME"),
            rustup_home.as_os_str().to_os_string(),
        );
    }
    let working_directory = workspace_root.join(&spec.working_directory);
    let configured_program = spec.argv.first().ok_or_else(invalid_command)?;
    let executable = resolve_executable(configured_program, trusted_tool_path, workspace_root)?;
    let timeout_millis = u64::try_from(spec.timeout_millis).map_err(|_| invalid_command())?;
    let max_output_bytes =
        usize::try_from(spec.output_limit_bytes).map_err(|_| invalid_command())?;
    let command = PhaseCommand {
        name: spec.id.clone(),
        diagnostic_parser_version: spec.diagnostic_parser_version.clone(),
        program: executable,
        arguments: spec.argv[1..].iter().map(OsString::from).collect(),
        working_directory,
        scratch_directory: scratch_directory.to_path_buf(),
        environment,
        access: match spec.phase {
            ValidationCommandPhase::Formatter
            | ValidationCommandPhase::SafeLintFix
            | ValidationCommandPhase::Codegen
            | ValidationCommandPhase::LockfileSync => PhaseAccess::Writer,
            ValidationCommandPhase::Validation => PhaseAccess::ReadOnlyValidation,
        },
        timeout: Duration::from_millis(timeout_millis),
        max_output_bytes,
    };
    ValidatedCommand::new(workspace_root, &command)?;
    Ok(command)
}

fn resolve_executable(
    configured: &str,
    trusted_tool_path: &OsStr,
    workspace_root: &Path,
) -> Result<PathBuf, PhaseProcessError> {
    let configured = Path::new(configured);
    let trusted_directories = std::env::split_paths(trusted_tool_path)
        .filter_map(|directory| directory.canonicalize().ok())
        .collect::<Vec<_>>();
    let candidates = if configured.is_absolute() {
        vec![configured.to_path_buf()]
    } else if configured.components().count() > 1 {
        return Err(invalid_command());
    } else {
        trusted_directories
            .iter()
            .map(|directory| directory.join(configured))
            .collect()
    };
    candidates
        .into_iter()
        .find_map(|candidate| {
            let parent = candidate.parent()?.canonicalize().ok()?;
            if !trusted_directories.contains(&parent) {
                return None;
            }
            let target = candidate.canonicalize().ok()?;
            (target.is_file() && !target.starts_with(workspace_root)).then_some(candidate)
        })
        .ok_or_else(invalid_command)
}

const fn environment_name(name: &ValidationEnvironmentName) -> &'static str {
    match name {
        ValidationEnvironmentName::Home => "HOME",
        ValidationEnvironmentName::Lang => "LANG",
        ValidationEnvironmentName::LcAll => "LC_ALL",
        ValidationEnvironmentName::Path => "PATH",
        ValidationEnvironmentName::Tmpdir => "TMPDIR",
    }
}

#[derive(Debug)]
struct ValidatedCommand<'command> {
    command: &'command PhaseCommand,
    workspace_root: PathBuf,
    working_directory: PathBuf,
    scratch_directory: PathBuf,
}

impl<'command> ValidatedCommand<'command> {
    fn new(
        workspace_root: &Path,
        command: &'command PhaseCommand,
    ) -> Result<Self, PhaseProcessError> {
        if command.name.is_empty()
            || command.name.len() > 100
            || !command
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:/@-".contains(&byte))
            || !command.program.is_absolute()
            || command.arguments.len() > MAX_ARGUMENTS
            || command.timeout.is_zero()
            || command.timeout > Duration::from_hours(24)
            || command.max_output_bytes == 0
            || command.max_output_bytes > 16 * 1_048_576
        {
            return Err(invalid_command());
        }
        let argument_bytes = command
            .arguments
            .iter()
            .try_fold(0_usize, |total, argument| {
                total.checked_add(argument.as_bytes().len())
            });
        if argument_bytes.is_none_or(|bytes| bytes > MAX_ARGUMENT_BYTES)
            || command.environment.keys().any(|name| {
                name.to_str()
                    .is_none_or(|name| !ALLOWED_ENVIRONMENT.contains(&name))
            })
        {
            return Err(invalid_command());
        }
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|_| invalid_command())?;
        let working_directory = command
            .working_directory
            .canonicalize()
            .map_err(|_| invalid_command())?;
        if !working_directory.starts_with(&workspace_root) {
            return Err(invalid_command());
        }
        let scratch_directory = command
            .scratch_directory
            .canonicalize()
            .map_err(|_| invalid_command())?;
        let job_root = workspace_root.parent().ok_or_else(invalid_command)?;
        if !scratch_directory.starts_with(job_root)
            || scratch_directory == workspace_root
            || working_directory.starts_with(&scratch_directory)
        {
            return Err(invalid_command());
        }
        let program = command
            .program
            .canonicalize()
            .map_err(|_| invalid_command())?;
        if !program.is_file() || program.starts_with(&workspace_root) {
            return Err(invalid_command());
        }
        Ok(Self {
            command,
            workspace_root,
            working_directory,
            scratch_directory,
        })
    }
}

fn spawn_network_denied(validated: &ValidatedCommand<'_>) -> Result<Child, PhaseProcessError> {
    let command = validated.command;
    let mut process = if cfg!(target_os = "macos") {
        let sandbox = Path::new("/usr/bin/sandbox-exec");
        if !sandbox.is_file() {
            return Err(sandbox_unavailable());
        }
        let profile = macos_sandbox_profile(validated)?;
        let mut process = Command::new(sandbox);
        process.args([
            OsStr::new("-p"),
            profile.as_os_str(),
            command.program.as_os_str(),
        ]);
        process
    } else if cfg!(target_os = "linux") {
        let sandbox = Path::new("/usr/bin/bwrap");
        if !sandbox.is_file() {
            return Err(sandbox_unavailable());
        }
        let mut process = Command::new(sandbox);
        process.args([
            OsStr::new("--unshare-net"),
            OsStr::new("--die-with-parent"),
            OsStr::new("--ro-bind"),
            OsStr::new("/"),
            OsStr::new("/"),
        ]);
        if command.access == PhaseAccess::Writer {
            process.args([
                OsStr::new("--bind"),
                validated.workspace_root.as_os_str(),
                validated.workspace_root.as_os_str(),
            ]);
        }
        process.args([
            OsStr::new("--bind"),
            validated.scratch_directory.as_os_str(),
            validated.scratch_directory.as_os_str(),
            OsStr::new("--chdir"),
            validated.working_directory.as_os_str(),
            OsStr::new("--"),
            command.program.as_os_str(),
        ]);
        process
    } else {
        return Err(sandbox_unavailable());
    };
    process
        .args(&command.arguments)
        .current_dir(&validated.working_directory)
        .env_clear()
        .envs(&command.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process.as_std_mut().process_group(0);
    process.spawn().map_err(|_| PhaseProcessError {
        code: PhaseProcessErrorCode::SpawnFailed,
        message: "validation command could not start",
    })
}

fn macos_sandbox_profile(validated: &ValidatedCommand<'_>) -> Result<OsString, PhaseProcessError> {
    let workspace = sandbox_literal(&validated.workspace_root)?;
    let scratch = sandbox_literal(&validated.scratch_directory)?;
    let write_rule = if validated.command.access == PhaseAccess::Writer {
        format!("(allow file-write* (subpath \"{workspace}\"))")
    } else {
        String::new()
    };
    Ok(OsString::from(format!(
        "(version 1) (allow default) (deny network*) (deny file-write*) \
         {write_rule} (allow file-write* (subpath \"{scratch}\")) \
         (allow file-write-data (literal \"/dev/null\"))"
    )))
}

fn sandbox_literal(path: &Path) -> Result<String, PhaseProcessError> {
    let value = path.to_str().ok_or_else(invalid_command)?;
    if value.contains(['\0', '\n', '\r']) {
        return Err(invalid_command());
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn wait_for_terminal(
    child: &mut Child,
    pid: Pid,
    deadline: TokioInstant,
    cancellation: &PhaseCancellation,
    output_exceeded: &AtomicBool,
) -> Result<PhaseProcessStatus, PhaseProcessError> {
    loop {
        if let Some(status) = child.try_wait().map_err(|_| wait_failed())? {
            return Ok(if status.success() {
                PhaseProcessStatus::Passed
            } else {
                PhaseProcessStatus::Failed
            });
        }
        let wake = (TokioInstant::now() + CANCELLATION_POLL).min(deadline);
        sleep_until(wake).await;
        if output_exceeded.load(Ordering::Acquire) {
            if child.try_wait().map_err(|_| wait_failed())?.is_none() {
                terminate_process_group(child, pid).await?;
            }
            return Ok(PhaseProcessStatus::OutputLimitExceeded);
        }
        if cancellation.is_cancelled() {
            terminate_process_group(child, pid).await?;
            return Ok(PhaseProcessStatus::Cancelled);
        }
        if TokioInstant::now() >= deadline {
            terminate_process_group(child, pid).await?;
            return Ok(PhaseProcessStatus::TimedOut);
        }
    }
}

async fn terminate_process_group(child: &mut Child, pid: Pid) -> Result<(), PhaseProcessError> {
    match kill_process_group(pid, Signal::TERM) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(_) => return Err(wait_failed()),
    }
    if timeout(TERMINATION_GRACE, child.wait()).await.is_ok() {
        return Ok(());
    }
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(_) => return Err(wait_failed()),
    }
    child.wait().await.map_err(|_| wait_failed())?;
    Ok(())
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    maximum: usize,
    total: Arc<AtomicUsize>,
    exceeded: Arc<AtomicBool>,
) -> Result<BoundedOutput, PhaseProcessError> {
    let mut digest = Sha256::new();
    let mut bytes = 0_usize;
    let mut content = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await.map_err(|_| wait_failed())?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes = bytes.saturating_add(read);
        let retained = maximum.saturating_sub(content.len()).min(read);
        content.extend_from_slice(&buffer[..retained]);
        let previous = total.fetch_add(read, Ordering::AcqRel);
        if previous.saturating_add(read) > maximum {
            exceeded.store(true, Ordering::Release);
        }
    }
    Ok(BoundedOutput {
        digest: Sha256Digest(format!("sha256:{:x}", digest.finalize())),
        bytes,
        exceeded: bytes > maximum,
        content,
    })
}

const fn invalid_command() -> PhaseProcessError {
    PhaseProcessError {
        code: PhaseProcessErrorCode::InvalidCommand,
        message: "validation command is invalid",
    }
}

const fn sandbox_unavailable() -> PhaseProcessError {
    PhaseProcessError {
        code: PhaseProcessErrorCode::SandboxUnavailable,
        message: "validation network sandbox is unavailable",
    }
}

const fn wait_failed() -> PhaseProcessError {
    PhaseProcessError {
        code: PhaseProcessErrorCode::WaitFailed,
        message: "validation command could not be reaped",
    }
}
