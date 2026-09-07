// SPDX-License-Identifier: Apache-2.0

//! Bounded process execution seam for already-sealed `DebugProbe` commands.

#![allow(
    dead_code,
    reason = "D2 defines the closed process seam; D9 supplies its only production host adapter"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Child;
use tokio::task::JoinHandle;

const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 262_144;
const MAX_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_OUTPUT_BYTES: usize = 16_777_216;
const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Stable internal reason why a process request was rejected or could not run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeProcessErrorKind {
    InvalidProbeId,
    InvalidExecutable,
    UntrustedExecutable,
    InterpreterForbidden,
    InvalidWorkspace,
    NonCanonicalWorkingDirectory,
    WorkingDirectoryOutsideWorkspace,
    InvalidArgument,
    InvalidLimits,
    EnvironmentForbidden,
    InvalidEnvironment,
    ExternalResourceForbidden,
    HostGuaranteeUnavailable,
    DescendantContainmentUnavailable,
    SpawnFailed,
}

/// Secret-safe process boundary failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeProcessError {
    kind: ProbeProcessErrorKind,
    message: &'static str,
}

impl ProbeProcessError {
    const fn new(kind: ProbeProcessErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> ProbeProcessErrorKind {
        self.kind
    }
}

impl fmt::Display for ProbeProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProbeProcessError {}

/// Per-process bounds applied after the scheduler has reserved round budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeProcessLimits {
    timeout: Duration,
    output_limit_bytes: usize,
    termination_grace: Duration,
    cleanup_timeout: Duration,
}

impl ProbeProcessLimits {
    pub(crate) fn new(
        timeout: Duration,
        output_limit_bytes: usize,
    ) -> Result<Self, ProbeProcessError> {
        let limits = Self {
            timeout,
            output_limit_bytes,
            termination_grace: DEFAULT_TERMINATION_GRACE,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
        };
        limits.validate()?;
        Ok(limits)
    }

    #[cfg(test)]
    fn with_cleanup_bounds(
        mut self,
        termination_grace: Duration,
        cleanup_timeout: Duration,
    ) -> Self {
        self.termination_grace = termination_grace;
        self.cleanup_timeout = cleanup_timeout;
        self
    }

    fn validate(self) -> Result<(), ProbeProcessError> {
        if self.timeout.is_zero()
            || self.timeout > MAX_TIMEOUT
            || self.output_limit_bytes == 0
            || self.output_limit_bytes > MAX_OUTPUT_BYTES
            || self.termination_grace.is_zero()
            || self.cleanup_timeout.is_zero()
        {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::InvalidLimits,
                "probe process limits are invalid",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn timeout(self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub(crate) const fn output_limit_bytes(self) -> usize {
        self.output_limit_bytes
    }
}

/// Resource declarations that require a host broker instead of raw child access.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProbeExternalResources {
    network: bool,
    database_keys: Vec<String>,
    service_keys: Vec<String>,
}

impl ProbeExternalResources {
    #[must_use]
    pub(crate) const fn none() -> Self {
        Self {
            network: false,
            database_keys: Vec::new(),
            service_keys: Vec::new(),
        }
    }

    #[cfg(test)]
    fn fixture(network: bool, database_keys: Vec<String>, service_keys: Vec<String>) -> Self {
        Self {
            network,
            database_keys,
            service_keys,
        }
    }

    fn is_empty(&self) -> bool {
        !self.network && self.database_keys.is_empty() && self.service_keys.is_empty()
    }
}

/// OS properties already proven by the process host selected for this request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeHostGuarantees {
    read_only_workspace: bool,
    network_none: bool,
    descendant_containment: bool,
}

impl ProbeHostGuarantees {
    #[cfg(test)]
    const fn new(
        read_only_workspace: bool,
        network_none: bool,
        descendant_containment: bool,
    ) -> Self {
        Self {
            read_only_workspace,
            network_none,
            descendant_containment,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn read_only_network_denied() -> Self {
        Self::new(true, true, false)
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_descendant_containment(mut self) -> Self {
        self.descendant_containment = true;
        self
    }
}

/// One exact host-owned command template. Request arguments never extend it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeCommandTemplate {
    executable: PathBuf,
    arguments: Vec<String>,
    may_spawn_descendants: bool,
}

impl ProbeCommandTemplate {
    #[must_use]
    pub(crate) fn exact(
        executable: PathBuf,
        arguments: Vec<String>,
        may_spawn_descendants: bool,
    ) -> Self {
        Self {
            executable,
            arguments,
            may_spawn_descendants,
        }
    }

    fn matches(&self, spec: &ProbeProcessSpec) -> bool {
        self.executable == spec.executable
            && self.arguments == spec.arguments
            && self.may_spawn_descendants == spec.may_spawn_descendants
    }
}

/// Host-owned executable, environment, resource, and sandbox policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeHostPolicy {
    command_templates: Vec<ProbeCommandTemplate>,
    allowed_environment: BTreeSet<String>,
    guarantees: ProbeHostGuarantees,
    fixture_external_adapter: bool,
}

impl ProbeHostPolicy {
    #[must_use]
    pub(crate) fn new(
        command_templates: Vec<ProbeCommandTemplate>,
        allowed_environment: BTreeSet<String>,
        guarantees: ProbeHostGuarantees,
    ) -> Self {
        Self {
            command_templates,
            allowed_environment,
            guarantees,
            fixture_external_adapter: false,
        }
    }

    #[cfg(test)]
    fn with_fixture_external_adapter(mut self) -> Self {
        self.fixture_external_adapter = true;
        self
    }
}

/// Untrusted scheduler projection before the process module seals it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeProcessSpec {
    probe_id: String,
    executable: PathBuf,
    arguments: Vec<String>,
    workspace_root: PathBuf,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
    limits: ProbeProcessLimits,
    external_resources: ProbeExternalResources,
    may_spawn_descendants: bool,
}

impl ProbeProcessSpec {
    #[must_use]
    pub(crate) fn new(
        probe_id: String,
        executable: PathBuf,
        arguments: Vec<String>,
        workspace_root: PathBuf,
        working_directory: PathBuf,
        limits: ProbeProcessLimits,
    ) -> Self {
        Self {
            probe_id,
            executable,
            arguments,
            workspace_root,
            working_directory,
            environment: BTreeMap::new(),
            limits,
            external_resources: ProbeExternalResources::none(),
            may_spawn_descendants: false,
        }
    }

    #[must_use]
    pub(crate) fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    #[must_use]
    pub(crate) fn with_external_resources(mut self, resources: ProbeExternalResources) -> Self {
        self.external_resources = resources;
        self
    }

    #[must_use]
    pub(crate) const fn may_spawn_descendants(mut self, may_spawn: bool) -> Self {
        self.may_spawn_descendants = may_spawn;
        self
    }
}

/// Opaque authority accepted by the process runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SealedProbeProcess {
    probe_id: String,
    executable: PathBuf,
    arguments: Vec<String>,
    workspace_root: PathBuf,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
    limits: ProbeProcessLimits,
}

impl SealedProbeProcess {
    pub(crate) fn try_new(
        spec: ProbeProcessSpec,
        policy: &ProbeHostPolicy,
    ) -> Result<Self, ProbeProcessError> {
        validate_probe_id(&spec.probe_id)?;
        validate_executable(&spec.executable, policy)?;
        if !policy
            .command_templates
            .iter()
            .any(|template| template.matches(&spec))
        {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::UntrustedExecutable,
                "probe command differs from its exact host template",
            ));
        }
        let workspace_root = validate_canonical_directory(
            &spec.workspace_root,
            ProbeProcessErrorKind::InvalidWorkspace,
        )?;
        let working_directory = validate_canonical_directory(
            &spec.working_directory,
            ProbeProcessErrorKind::NonCanonicalWorkingDirectory,
        )?;
        if !working_directory.starts_with(&workspace_root) {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::WorkingDirectoryOutsideWorkspace,
                "probe working directory leaves its workspace",
            ));
        }
        validate_arguments(&spec.arguments)?;
        validate_environment(&spec.environment, policy)?;
        spec.limits.validate()?;
        if !policy.guarantees.read_only_workspace || !policy.guarantees.network_none {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::HostGuaranteeUnavailable,
                "probe host has not proven read-only filesystem and denied network",
            ));
        }
        if !spec.external_resources.is_empty() && !policy.fixture_external_adapter {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::ExternalResourceForbidden,
                "probe external resource has no explicit host adapter",
            ));
        }
        if spec.may_spawn_descendants && !policy.guarantees.descendant_containment {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::DescendantContainmentUnavailable,
                "probe template may spawn descendants without proven containment",
            ));
        }
        Ok(Self {
            probe_id: spec.probe_id,
            executable: spec.executable,
            arguments: spec.arguments,
            workspace_root,
            working_directory,
            environment: spec.environment,
            limits: spec.limits,
        })
    }

    #[must_use]
    pub(crate) fn probe_id(&self) -> &str {
        &self.probe_id
    }

    #[must_use]
    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    #[must_use]
    pub(crate) fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    #[must_use]
    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    #[must_use]
    pub(crate) const fn limits(&self) -> ProbeProcessLimits {
        self.limits
    }
}

fn validate_probe_id(probe_id: &str) -> Result<(), ProbeProcessError> {
    if probe_id.is_empty() || probe_id.len() > 200 || probe_id.contains('\0') {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::InvalidProbeId,
            "probe process identity is invalid",
        ));
    }
    Ok(())
}

fn validate_executable(
    executable: &Path,
    policy: &ProbeHostPolicy,
) -> Result<(), ProbeProcessError> {
    if !executable.is_absolute() {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::InvalidExecutable,
            "probe executable is not absolute",
        ));
    }
    let canonical = std::fs::canonicalize(executable).map_err(|_| {
        ProbeProcessError::new(
            ProbeProcessErrorKind::InvalidExecutable,
            "probe executable is unavailable",
        )
    })?;
    if canonical != executable || !canonical.is_file() {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::InvalidExecutable,
            "probe executable is not its canonical regular file",
        ));
    }
    if interpreter_forbidden(&canonical) {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::InterpreterForbidden,
            "shell and interpreter evaluation are forbidden for probes",
        ));
    }
    if !policy
        .command_templates
        .iter()
        .any(|template| template.executable == canonical)
    {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::UntrustedExecutable,
            "probe executable is absent from the host template catalog",
        ));
    }
    Ok(())
}

fn interpreter_forbidden(executable: &Path) -> bool {
    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "env"
            | "python"
            | "python3"
            | "node"
            | "perl"
            | "ruby"
            | "php"
            | "osascript"
            | "pwsh"
            | "powershell"
    )
}

fn validate_canonical_directory(
    directory: &Path,
    kind: ProbeProcessErrorKind,
) -> Result<PathBuf, ProbeProcessError> {
    if !directory.is_absolute() {
        return Err(ProbeProcessError::new(
            kind,
            "probe directory is not absolute",
        ));
    }
    let canonical = std::fs::canonicalize(directory)
        .map_err(|_| ProbeProcessError::new(kind, "probe directory is unavailable"))?;
    if canonical != directory || !canonical.is_dir() {
        return Err(ProbeProcessError::new(
            kind,
            "probe directory is not canonical",
        ));
    }
    Ok(canonical)
}

fn validate_arguments(arguments: &[String]) -> Result<(), ProbeProcessError> {
    let total_bytes = arguments.iter().try_fold(0_usize, |total, argument| {
        if argument.is_empty() || argument.contains('\0') {
            return None;
        }
        total.checked_add(argument.len().saturating_add(1))
    });
    if arguments.len() > MAX_ARGUMENTS || total_bytes.is_none_or(|total| total > MAX_ARGUMENT_BYTES)
    {
        return Err(ProbeProcessError::new(
            ProbeProcessErrorKind::InvalidArgument,
            "probe arguments are invalid or exceed their bound",
        ));
    }
    Ok(())
}

fn validate_environment(
    environment: &BTreeMap<String, String>,
    policy: &ProbeHostPolicy,
) -> Result<(), ProbeProcessError> {
    const SAFE_NAMES: &[&str] = &["HOME", "LANG", "LC_ALL", "NO_COLOR", "TMPDIR", "TZ"];
    for (name, value) in environment {
        if !SAFE_NAMES.contains(&name.as_str()) || !policy.allowed_environment.contains(name) {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::EnvironmentForbidden,
                "probe environment key is not allowlisted",
            ));
        }
        if value.contains('\0') {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::InvalidEnvironment,
                "probe environment value is invalid",
            ));
        }
        if matches!(name.as_str(), "HOME" | "TMPDIR") && !Path::new(value).is_absolute() {
            return Err(ProbeProcessError::new(
                ProbeProcessErrorKind::InvalidEnvironment,
                "probe private directory environment is not absolute",
            ));
        }
    }
    Ok(())
}

/// Cancellation authority shared by scheduler replacement, `JobCancel`, and timeout routing.
#[derive(Clone, Debug)]
pub(crate) struct ProbeCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ProbeCancellation {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Default for ProbeCancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// Opaque host containment authority; production adapters must bind more than a reusable PID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeContainmentId(String);

impl ProbeContainmentId {
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Hosted child plus its exact process-containment authority.
pub(crate) struct HostedProbeProcess {
    child: Child,
    containment: ProbeContainmentId,
}

impl HostedProbeProcess {
    #[must_use]
    pub(crate) fn new(child: Child, containment: String) -> Self {
        Self {
            child,
            containment: ProbeContainmentId(containment),
        }
    }
}

/// Signal requested against an exact host containment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeProcessSignal {
    Terminate,
    Kill,
}

/// Platform adapter which owns actual filesystem, network, and descendant isolation.
///
/// This module deliberately provides no production implementation. D9 must supply one
/// which validates the opaque containment identity before every signal and proves the
/// guarantees used to seal the request. The fixture adapter lives only under `cfg(test)`.
pub(crate) trait ProbeProcessHost: Send + Sync {
    fn spawn(&self, process: &SealedProbeProcess) -> Result<HostedProbeProcess, ProbeProcessError>;

    fn signal_containment(
        &self,
        containment: &ProbeContainmentId,
        signal: ProbeProcessSignal,
    ) -> Result<(), ProbeProcessError>;

    fn containment_has_live_processes(
        &self,
        containment: &ProbeContainmentId,
    ) -> Result<bool, ProbeProcessError>;
}

/// Terminal internal outcome consumed by the scheduler's durable receipt writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeProcessTermination {
    Exited,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
    InfrastructureError,
    CleanupFailed,
}

/// Bounded process result. It owns bytes only until the scheduler writes Artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeProcessReceipt {
    termination: ProbeProcessTermination,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration: Duration,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_truncated: bool,
    cleanup_confirmed: bool,
}

impl ProbeProcessReceipt {
    fn without_process(termination: ProbeProcessTermination, duration: Duration) -> Self {
        Self {
            termination,
            exit_code: None,
            signal: None,
            duration,
            stdout: Vec::new(),
            stderr: Vec::new(),
            output_truncated: false,
            cleanup_confirmed: true,
        }
    }

    fn infrastructure(duration: Duration) -> Self {
        Self::without_process(ProbeProcessTermination::InfrastructureError, duration)
    }

    #[must_use]
    pub(crate) const fn termination(&self) -> ProbeProcessTermination {
        self.termination
    }

    #[must_use]
    pub(crate) const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    #[must_use]
    pub(crate) const fn signal(&self) -> Option<i32> {
        self.signal
    }

    #[must_use]
    pub(crate) const fn duration(&self) -> Duration {
        self.duration
    }

    #[must_use]
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[must_use]
    pub(crate) const fn output_truncated(&self) -> bool {
        self.output_truncated
    }

    #[must_use]
    pub(crate) const fn cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed
    }
}

#[derive(Debug, Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    read_failed: bool,
}

impl CapturedOutput {
    fn append(&mut self, stream: ProbeOutputStream, bytes: &[u8], limit: usize) {
        let retained = self.stdout.len().saturating_add(self.stderr.len());
        let remaining = limit.saturating_sub(retained);
        let accepted = remaining.min(bytes.len());
        match stream {
            ProbeOutputStream::Stdout => self.stdout.extend_from_slice(&bytes[..accepted]),
            ProbeOutputStream::Stderr => self.stderr.extend_from_slice(&bytes[..accepted]),
        }
        self.truncated |= accepted < bytes.len();
    }
}

#[derive(Clone, Copy)]
enum ProbeOutputStream {
    Stdout,
    Stderr,
}

/// Executes only opaque requests already sealed after the scheduler's durable intent.
pub(crate) struct ProbeProcessRuntime {
    host: Arc<dyn ProbeProcessHost>,
}

impl ProbeProcessRuntime {
    #[must_use]
    pub(crate) fn new(host: Arc<dyn ProbeProcessHost>) -> Self {
        Self { host }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn execute(
        &self,
        process: &SealedProbeProcess,
        cancellation: ProbeCancellation,
    ) -> ProbeProcessReceipt {
        let started = std::time::Instant::now();
        if cancellation.is_cancelled() {
            return ProbeProcessReceipt::without_process(
                ProbeProcessTermination::Cancelled,
                started.elapsed(),
            );
        }
        if !request_still_sealed(process) {
            return ProbeProcessReceipt::infrastructure(started.elapsed());
        }
        let Ok(hosted) = self.host.spawn(process) else {
            return ProbeProcessReceipt::infrastructure(started.elapsed());
        };
        let HostedProbeProcess {
            mut child,
            containment,
        } = hosted;
        let Some(stdout) = child.stdout.take() else {
            let cleanup =
                cleanup_process(self.host.as_ref(), &mut child, &containment, process.limits).await;
            return receipt_after_early_cleanup(&cleanup, started.elapsed());
        };
        let Some(stderr) = child.stderr.take() else {
            let cleanup =
                cleanup_process(self.host.as_ref(), &mut child, &containment, process.limits).await;
            return receipt_after_early_cleanup(&cleanup, started.elapsed());
        };

        let captured = Arc::new(Mutex::new(CapturedOutput::default()));
        let overflow = Arc::new(AtomicBool::new(false));
        let mut stdout_task = output_reader(
            stdout,
            ProbeOutputStream::Stdout,
            Arc::clone(&captured),
            Arc::clone(&overflow),
            process.limits.output_limit_bytes,
        );
        let mut stderr_task = output_reader(
            stderr,
            ProbeOutputStream::Stderr,
            Arc::clone(&captured),
            Arc::clone(&overflow),
            process.limits.output_limit_bytes,
        );

        let (requested_termination, mut status) = loop {
            if cancellation.is_cancelled() {
                break (ProbeProcessTermination::Cancelled, None);
            }
            if overflow.load(Ordering::Acquire) {
                break (ProbeProcessTermination::OutputLimitExceeded, None);
            }
            if started.elapsed() >= process.limits.timeout {
                break (ProbeProcessTermination::TimedOut, None);
            }
            match child.try_wait() {
                Ok(Some(status)) => break (ProbeProcessTermination::Exited, Some(status)),
                Ok(None) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(_) => break (ProbeProcessTermination::InfrastructureError, None),
            }
        };

        let cleanup =
            cleanup_process(self.host.as_ref(), &mut child, &containment, process.limits).await;
        if status.is_none() {
            status = cleanup.status;
        }
        let readers_completed = await_output_readers(
            &mut stdout_task,
            &mut stderr_task,
            process.limits.cleanup_timeout,
        )
        .await;
        let captured = take_captured_output(&captured);
        let cleanup_confirmed = cleanup.confirmed && readers_completed;
        let termination = if !cleanup_confirmed {
            ProbeProcessTermination::CleanupFailed
        } else if captured.read_failed {
            ProbeProcessTermination::InfrastructureError
        } else {
            requested_termination
        };
        ProbeProcessReceipt {
            termination,
            exit_code: status.as_ref().and_then(std::process::ExitStatus::code),
            signal: status.and_then(exit_signal),
            duration: started.elapsed(),
            stdout: captured.stdout,
            stderr: captured.stderr,
            output_truncated: captured.truncated,
            cleanup_confirmed,
        }
    }
}

fn request_still_sealed(process: &SealedProbeProcess) -> bool {
    std::fs::canonicalize(&process.executable).is_ok_and(|path| path == process.executable)
        && std::fs::canonicalize(&process.workspace_root)
            .is_ok_and(|path| path == process.workspace_root)
        && std::fs::canonicalize(&process.working_directory)
            .is_ok_and(|path| path == process.working_directory)
        && process
            .working_directory
            .starts_with(&process.workspace_root)
}

fn output_reader<R>(
    mut reader: R,
    stream: ProbeOutputStream,
    captured: Arc<Mutex<CapturedOutput>>,
    overflow: Arc<AtomicBool>,
    limit: usize,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => return,
                Ok(count) => {
                    let mut state = captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.append(stream, &chunk[..count], limit);
                    if state.truncated {
                        overflow.store(true, Ordering::Release);
                    }
                }
                Err(_) => {
                    captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .read_failed = true;
                    return;
                }
            }
        }
    })
}

struct CleanupResult {
    confirmed: bool,
    status: Option<std::process::ExitStatus>,
}

fn receipt_after_early_cleanup(cleanup: &CleanupResult, duration: Duration) -> ProbeProcessReceipt {
    ProbeProcessReceipt {
        termination: if cleanup.confirmed {
            ProbeProcessTermination::InfrastructureError
        } else {
            ProbeProcessTermination::CleanupFailed
        },
        exit_code: cleanup
            .status
            .as_ref()
            .and_then(std::process::ExitStatus::code),
        signal: cleanup.status.and_then(exit_signal),
        duration,
        stdout: Vec::new(),
        stderr: Vec::new(),
        output_truncated: false,
        cleanup_confirmed: cleanup.confirmed,
    }
}

async fn cleanup_process(
    host: &dyn ProbeProcessHost,
    child: &mut Child,
    containment: &ProbeContainmentId,
    limits: ProbeProcessLimits,
) -> CleanupResult {
    let mut confirmed = host
        .signal_containment(containment, ProbeProcessSignal::Terminate)
        .is_ok();
    let grace_deadline = std::time::Instant::now() + limits.termination_grace;
    let mut status = None;
    while std::time::Instant::now() < grace_deadline {
        match child.try_wait() {
            Ok(Some(found)) => status = Some(found),
            Ok(None) => {}
            Err(_) => confirmed = false,
        }
        match host.containment_has_live_processes(containment) {
            Ok(false) => break,
            Ok(true) => tokio::time::sleep(Duration::from_millis(5)).await,
            Err(_) => {
                confirmed = false;
                break;
            }
        }
    }
    let live = host
        .containment_has_live_processes(containment)
        .unwrap_or(true);
    if live {
        confirmed &= host
            .signal_containment(containment, ProbeProcessSignal::Kill)
            .is_ok();
        let _ = child.start_kill();
    }
    if status.is_none() {
        match tokio::time::timeout(limits.cleanup_timeout, child.wait()).await {
            Ok(Ok(found)) => status = Some(found),
            Ok(Err(_)) | Err(_) => confirmed = false,
        }
    }
    let cleanup_deadline = std::time::Instant::now() + limits.cleanup_timeout;
    loop {
        match host.containment_has_live_processes(containment) {
            Ok(false) => break,
            Ok(true) if std::time::Instant::now() < cleanup_deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(true) | Err(_) => {
                confirmed = false;
                break;
            }
        }
    }
    CleanupResult { confirmed, status }
}

async fn await_output_readers(
    stdout: &mut JoinHandle<()>,
    stderr: &mut JoinHandle<()>,
    timeout: Duration,
) -> bool {
    let completed = tokio::time::timeout(timeout, async {
        let (stdout, stderr) = tokio::join!(&mut *stdout, &mut *stderr);
        stdout.is_ok() && stderr.is_ok()
    })
    .await;
    if let Ok(completed) = completed {
        completed
    } else {
        stdout.abort();
        stderr.abort();
        false
    }
}

fn take_captured_output(captured: &Mutex<CapturedOutput>) -> CapturedOutput {
    let mut captured = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *captured)
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(not(unix))]
const fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static FIXTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn canonical_echo() -> PathBuf {
        std::fs::canonicalize("/bin/echo").expect("canonical echo")
    }

    fn fixture_policy(executable: PathBuf) -> ProbeHostPolicy {
        ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(
                executable,
                vec!["probe".to_owned()],
                false,
            )],
            BTreeSet::from(["LANG".to_owned()]),
            ProbeHostGuarantees::read_only_network_denied(),
        )
    }

    fn fixture_spec(executable: PathBuf) -> ProbeProcessSpec {
        let workspace = std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory");
        ProbeProcessSpec::new(
            "prb_fixture".to_owned(),
            executable,
            vec!["probe".to_owned()],
            workspace.clone(),
            workspace,
            ProbeProcessLimits::new(Duration::from_secs(1), 1024).expect("valid limits"),
        )
    }

    #[test]
    fn sealing_requires_canonical_trusted_process_authority() {
        let executable = canonical_echo();
        let policy = fixture_policy(executable.clone());
        let sealed = SealedProbeProcess::try_new(fixture_spec(executable.clone()), &policy)
            .expect("sealed trusted process");
        assert_eq!(sealed.probe_id(), "prb_fixture");
        assert_eq!(sealed.executable(), executable);
        assert_eq!(sealed.arguments(), ["probe"]);

        let different_arguments = ProbeProcessSpec::new(
            "prb_different_arguments".to_owned(),
            executable.clone(),
            vec!["probe".to_owned(), "--write".to_owned()],
            sealed.working_directory().to_path_buf(),
            sealed.working_directory().to_path_buf(),
            ProbeProcessLimits::new(Duration::from_secs(1), 8).expect("valid limits"),
        );
        assert_eq!(
            SealedProbeProcess::try_new(different_arguments, &policy)
                .expect_err("arguments outside the exact host template must fail")
                .kind(),
            ProbeProcessErrorKind::UntrustedExecutable
        );

        let relative = ProbeProcessSpec::new(
            "prb_relative".to_owned(),
            PathBuf::from("echo"),
            Vec::new(),
            sealed.working_directory().to_path_buf(),
            sealed.working_directory().to_path_buf(),
            ProbeProcessLimits::new(Duration::from_secs(1), 8).expect("valid limits"),
        );
        assert_eq!(
            SealedProbeProcess::try_new(relative, &policy)
                .expect_err("relative executable must fail")
                .kind(),
            ProbeProcessErrorKind::InvalidExecutable
        );
    }

    #[test]
    fn sealing_rejects_interpreters_environment_and_unproved_resources() {
        let shell = std::fs::canonicalize("/bin/sh").expect("canonical shell");
        let shell_policy = fixture_policy(shell.clone());
        assert_eq!(
            SealedProbeProcess::try_new(fixture_spec(shell), &shell_policy)
                .expect_err("shell must fail")
                .kind(),
            ProbeProcessErrorKind::InterpreterForbidden
        );

        let executable = canonical_echo();
        let policy = fixture_policy(executable.clone());
        let mut environment = BTreeMap::new();
        environment.insert("TOKEN".to_owned(), "secret".to_owned());
        let with_secret = fixture_spec(executable.clone()).with_environment(environment);
        assert_eq!(
            SealedProbeProcess::try_new(with_secret, &policy)
                .expect_err("secret environment must fail")
                .kind(),
            ProbeProcessErrorKind::EnvironmentForbidden
        );

        let with_external = fixture_spec(executable.clone()).with_external_resources(
            ProbeExternalResources::fixture(true, vec!["db".to_owned()], Vec::new()),
        );
        assert_eq!(
            SealedProbeProcess::try_new(with_external, &policy)
                .expect_err("external resource must fail closed")
                .kind(),
            ProbeProcessErrorKind::ExternalResourceForbidden
        );

        let fixture_external = fixture_spec(executable.clone()).with_external_resources(
            ProbeExternalResources::fixture(true, vec!["fixture-db".to_owned()], Vec::new()),
        );
        SealedProbeProcess::try_new(
            fixture_external,
            &policy.clone().with_fixture_external_adapter(),
        )
        .expect("an explicit test-only host adapter may admit fixture resources");

        let unproved_host = ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(
                executable.clone(),
                vec!["probe".to_owned()],
                false,
            )],
            BTreeSet::from(["LANG".to_owned()]),
            ProbeHostGuarantees::new(false, true, false),
        );
        assert_eq!(
            SealedProbeProcess::try_new(fixture_spec(executable.clone()), &unproved_host)
                .expect_err("workspace and network guarantees must be host-proven")
                .kind(),
            ProbeProcessErrorKind::HostGuaranteeUnavailable
        );

        let with_descendants = fixture_spec(executable.clone()).may_spawn_descendants(true);
        let descendant_policy = ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(
                executable,
                vec!["probe".to_owned()],
                true,
            )],
            BTreeSet::from(["LANG".to_owned()]),
            ProbeHostGuarantees::read_only_network_denied(),
        );
        assert_eq!(
            SealedProbeProcess::try_new(with_descendants, &descendant_policy)
                .expect_err("unproved descendant containment must fail")
                .kind(),
            ProbeProcessErrorKind::DescendantContainmentUnavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn sealing_rejects_a_symlink_working_directory() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "winwincode-probe-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos()
        ));
        let workspace = root.join("workspace");
        let real = workspace.join("real");
        let alias = workspace.join("alias");
        std::fs::create_dir_all(&real).expect("create workspace");
        symlink(&real, &alias).expect("create alias");
        let executable = canonical_echo();
        let spec = ProbeProcessSpec::new(
            "prb_symlink".to_owned(),
            executable.clone(),
            Vec::new(),
            workspace.canonicalize().expect("canonical workspace"),
            alias,
            ProbeProcessLimits::new(Duration::from_secs(1), 8).expect("valid limits"),
        );
        let policy = ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(executable, Vec::new(), false)],
            BTreeSet::new(),
            ProbeHostGuarantees::read_only_network_denied(),
        );
        assert_eq!(
            SealedProbeProcess::try_new(spec, &policy)
                .expect_err("symlink cwd must fail")
                .kind(),
            ProbeProcessErrorKind::NonCanonicalWorkingDirectory
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    struct SpawnCountingHost {
        spawn_count: Arc<AtomicUsize>,
    }

    impl ProbeProcessHost for SpawnCountingHost {
        fn spawn(
            &self,
            _process: &SealedProbeProcess,
        ) -> Result<HostedProbeProcess, ProbeProcessError> {
            self.spawn_count.fetch_add(1, Ordering::SeqCst);
            Err(ProbeProcessError::new(
                ProbeProcessErrorKind::SpawnFailed,
                "counting host does not spawn",
            ))
        }

        fn signal_containment(
            &self,
            _containment: &ProbeContainmentId,
            _signal: ProbeProcessSignal,
        ) -> Result<(), ProbeProcessError> {
            panic!("pre-cancelled process must have no containment")
        }

        fn containment_has_live_processes(
            &self,
            _containment: &ProbeContainmentId,
        ) -> Result<bool, ProbeProcessError> {
            panic!("pre-cancelled process must have no containment")
        }
    }

    #[tokio::test]
    async fn pre_cancelled_request_never_reaches_the_process_host() {
        let executable = canonical_echo();
        let sealed = SealedProbeProcess::try_new(
            fixture_spec(executable.clone()),
            &fixture_policy(executable),
        )
        .expect("sealed fixture");
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let runtime = ProbeProcessRuntime::new(Arc::new(SpawnCountingHost {
            spawn_count: Arc::clone(&spawn_count),
        }));
        let cancellation = ProbeCancellation::new();
        cancellation.cancel();

        let receipt = runtime.execute(&sealed, cancellation).await;

        assert_eq!(receipt.termination(), ProbeProcessTermination::Cancelled);
        assert!(receipt.cleanup_confirmed());
        assert_eq!(spawn_count.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    struct FixtureProcessHost;

    #[cfg(unix)]
    struct MissingPipeCleanupFailureHost {
        missing_stdout: bool,
    }

    #[cfg(unix)]
    impl ProbeProcessHost for MissingPipeCleanupFailureHost {
        fn spawn(
            &self,
            process: &SealedProbeProcess,
        ) -> Result<HostedProbeProcess, ProbeProcessError> {
            let mut command = tokio::process::Command::new(process.executable());
            command
                .args(process.arguments())
                .current_dir(process.working_directory())
                .env_clear()
                .stdin(Stdio::null())
                .stdout(if self.missing_stdout {
                    Stdio::null()
                } else {
                    Stdio::piped()
                })
                .stderr(if self.missing_stdout {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .process_group(0);
            let child = command.spawn().map_err(|_| {
                ProbeProcessError::new(
                    ProbeProcessErrorKind::SpawnFailed,
                    "missing-pipe fixture cannot be spawned",
                )
            })?;
            let process_group_id = child.id().ok_or_else(|| {
                ProbeProcessError::new(
                    ProbeProcessErrorKind::SpawnFailed,
                    "missing-pipe fixture has no process group",
                )
            })?;
            Ok(HostedProbeProcess::new(child, process_group_id.to_string()))
        }

        fn signal_containment(
            &self,
            _containment: &ProbeContainmentId,
            _signal: ProbeProcessSignal,
        ) -> Result<(), ProbeProcessError> {
            Err(ProbeProcessError::new(
                ProbeProcessErrorKind::SpawnFailed,
                "missing-pipe fixture rejects containment signals",
            ))
        }

        fn containment_has_live_processes(
            &self,
            _containment: &ProbeContainmentId,
        ) -> Result<bool, ProbeProcessError> {
            Err(ProbeProcessError::new(
                ProbeProcessErrorKind::SpawnFailed,
                "missing-pipe fixture cannot prove containment cleanup",
            ))
        }
    }

    #[cfg(unix)]
    impl ProbeProcessHost for FixtureProcessHost {
        fn spawn(
            &self,
            process: &SealedProbeProcess,
        ) -> Result<HostedProbeProcess, ProbeProcessError> {
            let mut command = tokio::process::Command::new(process.executable());
            command
                .args(process.arguments())
                .current_dir(process.working_directory())
                .env_clear()
                .envs(process.environment())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            let child = command.spawn().map_err(|_| {
                ProbeProcessError::new(
                    ProbeProcessErrorKind::SpawnFailed,
                    "fixture process cannot be spawned",
                )
            })?;
            let process_group_id = child.id().ok_or_else(|| {
                ProbeProcessError::new(
                    ProbeProcessErrorKind::SpawnFailed,
                    "fixture process has no process group",
                )
            })?;
            Ok(HostedProbeProcess::new(child, process_group_id.to_string()))
        }

        fn signal_containment(
            &self,
            containment: &ProbeContainmentId,
            signal: ProbeProcessSignal,
        ) -> Result<(), ProbeProcessError> {
            let signal = match signal {
                ProbeProcessSignal::Terminate => "-TERM",
                ProbeProcessSignal::Kill => "-KILL",
            };
            let _ = std::process::Command::new("/bin/kill")
                .args([signal, "--", &format!("-{}", containment.as_str())])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            Ok(())
        }

        fn containment_has_live_processes(
            &self,
            containment: &ProbeContainmentId,
        ) -> Result<bool, ProbeProcessError> {
            Ok(std::process::Command::new("/bin/kill")
                .args(["-0", "--", &format!("-{}", containment.as_str())])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_output_pipe_never_claims_failed_cleanup_succeeded() {
        let executable = std::fs::canonicalize("/bin/sleep").expect("canonical sleep");
        let workspace = std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory");
        let arguments = vec!["30".to_owned()];
        let limits = ProbeProcessLimits::new(Duration::from_secs(1), 1024)
            .expect("valid limits")
            .with_cleanup_bounds(Duration::from_millis(5), Duration::from_millis(100));
        let policy = ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(
                executable.clone(),
                arguments.clone(),
                false,
            )],
            BTreeSet::new(),
            ProbeHostGuarantees::read_only_network_denied(),
        );
        let sealed = SealedProbeProcess::try_new(
            ProbeProcessSpec::new(
                "prb_missing_pipe".to_owned(),
                executable,
                arguments,
                workspace.clone(),
                workspace,
                limits,
            ),
            &policy,
        )
        .expect("seal missing-pipe fixture");

        for missing_stdout in [true, false] {
            let receipt = ProbeProcessRuntime::new(Arc::new(MissingPipeCleanupFailureHost {
                missing_stdout,
            }))
            .execute(&sealed, ProbeCancellation::new())
            .await;

            assert_eq!(
                receipt.termination(),
                ProbeProcessTermination::CleanupFailed
            );
            assert!(!receipt.cleanup_confirmed());
        }
    }

    #[cfg(unix)]
    fn process_fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "winwincode-probe-process-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&root).expect("create process fixture root");
        let source = root.join("fixture.rs");
        let executable = root.join("probe-fixture");
        std::fs::write(
            &source,
            r#"
use std::io::Write as _;
use std::process::Command;
use std::time::Duration;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("emit") => {
            let bytes = vec![b'x'; 8192];
            std::io::stdout().write_all(&bytes).unwrap();
            std::io::stderr().write_all(&bytes).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        Some("tree") => {
            let marker = &args[2];
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["leaf", marker])
                .spawn()
                .unwrap();
            std::fs::write(marker, child.id().to_string()).unwrap();
            loop { std::thread::sleep(Duration::from_secs(1)); }
        }
        Some("leaf") => {
            let marker = &args[2];
            let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            let mut prior = std::fs::read_to_string(marker).unwrap_or_default();
            prior.push(' ');
            prior.push_str(&child.id().to_string());
            std::fs::write(marker, prior).unwrap();
            loop { std::thread::sleep(Duration::from_secs(1)); }
        }
        _ => {}
    }
}
"#,
        )
        .expect("write process fixture source");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = std::process::Command::new(rustc)
            .args(["--edition=2024", "-o"])
            .arg(&executable)
            .arg(&source)
            .status()
            .expect("compile process fixture");
        assert!(status.success(), "compile process fixture");
        let root = root.canonicalize().expect("canonical process root");
        (
            executable
                .canonicalize()
                .expect("canonical process fixture"),
            root,
        )
    }

    #[cfg(unix)]
    fn executable_spec(
        executable: PathBuf,
        root: &Path,
        arguments: Vec<String>,
        limits: ProbeProcessLimits,
        descendants: bool,
    ) -> SealedProbeProcess {
        let guarantees = if descendants {
            ProbeHostGuarantees::read_only_network_denied().with_descendant_containment()
        } else {
            ProbeHostGuarantees::read_only_network_denied()
        };
        let policy = ProbeHostPolicy::new(
            vec![ProbeCommandTemplate::exact(
                executable.clone(),
                arguments.clone(),
                descendants,
            )],
            BTreeSet::new(),
            guarantees,
        );
        SealedProbeProcess::try_new(
            ProbeProcessSpec::new(
                "prb_process_fixture".to_owned(),
                executable,
                arguments,
                root.to_path_buf(),
                root.to_path_buf(),
                limits,
            )
            .may_spawn_descendants(descendants),
            &policy,
        )
        .expect("seal process fixture")
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn combined_output_limit_terminates_and_reaps_the_process() {
        let (executable, root) = process_fixture();
        let limits = ProbeProcessLimits::new(Duration::from_secs(5), 1024)
            .expect("valid limits")
            .with_cleanup_bounds(Duration::from_millis(20), Duration::from_secs(2));
        let sealed = executable_spec(executable, &root, vec!["emit".to_owned()], limits, false);
        let receipt = ProbeProcessRuntime::new(Arc::new(FixtureProcessHost))
            .execute(&sealed, ProbeCancellation::new())
            .await;

        assert_eq!(
            receipt.termination(),
            ProbeProcessTermination::OutputLimitExceeded
        );
        assert!(receipt.output_truncated());
        assert_eq!(receipt.stdout().len() + receipt.stderr().len(), 1024);
        assert!(receipt.cleanup_confirmed());
        std::fs::remove_dir_all(root).expect("remove process fixture");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_terminates_and_reaps_child_and_grandchild() {
        let (executable, root) = process_fixture();
        let marker = root.join("descendants");
        let limits = ProbeProcessLimits::new(Duration::from_secs(5), 1024)
            .expect("valid limits")
            .with_cleanup_bounds(Duration::from_millis(20), Duration::from_secs(2));
        let sealed = executable_spec(
            executable,
            &root,
            vec!["tree".to_owned(), marker.to_string_lossy().into_owned()],
            limits,
            true,
        );
        let cancellation = ProbeCancellation::new();
        let cancel = cancellation.clone();
        let marker_for_cancel = marker.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                if std::fs::read_to_string(&marker_for_cancel)
                    .is_ok_and(|contents| contents.split_whitespace().count() == 2)
                {
                    cancel.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
        });
        let receipt = ProbeProcessRuntime::new(Arc::new(FixtureProcessHost))
            .execute(&sealed, cancellation)
            .await;

        assert_eq!(receipt.termination(), ProbeProcessTermination::Cancelled);
        assert!(receipt.cleanup_confirmed());
        let descendants = std::fs::read_to_string(&marker)
            .expect("read descendant process ids")
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("parse descendant id"))
            .collect::<Vec<_>>();
        assert_eq!(descendants.len(), 2);
        for process_id in descendants {
            assert_process_stops(process_id).await;
        }
        std::fs::remove_dir_all(root).expect("remove process fixture");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_terminates_and_reaps_child_and_grandchild() {
        let (executable, root) = process_fixture();
        let marker = root.join("timeout-descendants");
        let limits = ProbeProcessLimits::new(Duration::from_millis(300), 1024)
            .expect("valid limits")
            .with_cleanup_bounds(Duration::from_millis(20), Duration::from_secs(2));
        let sealed = executable_spec(
            executable,
            &root,
            vec!["tree".to_owned(), marker.to_string_lossy().into_owned()],
            limits,
            true,
        );
        let receipt = ProbeProcessRuntime::new(Arc::new(FixtureProcessHost))
            .execute(&sealed, ProbeCancellation::new())
            .await;

        assert_eq!(receipt.termination(), ProbeProcessTermination::TimedOut);
        assert!(receipt.cleanup_confirmed());
        let descendants = std::fs::read_to_string(&marker)
            .expect("read timeout descendant process ids")
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("parse descendant id"))
            .collect::<Vec<_>>();
        assert_eq!(descendants.len(), 2);
        for process_id in descendants {
            assert_process_stops(process_id).await;
        }
        std::fs::remove_dir_all(root).expect("remove process fixture");
    }

    #[cfg(unix)]
    async fn assert_process_stops(process_id: u32) {
        for _ in 0..100 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &process_id.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("descendant process {process_id} survived cleanup");
    }
}
