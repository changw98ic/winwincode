// SPDX-License-Identifier: Apache-2.0

//! Device-owned lifecycle for one managed candidate/live application.
//!
//! This module intentionally does not use [`crate::supervisor::SessionSupervisor`].
//! Worker sessions and candidate HTTP processes have different identities,
//! leases and preview sources.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use winwincode_client_port::managed_app::{
    ManagedAppCommand, ManagedAppOperation, ManagedAppRunConfig, ManagedAppState, ManagedAppStatus,
};

use crate::{AuthorizedPreviewSource, AuthorizedPreviewSourceRegistry};

const STATE_FILE: &str = "managed-app-runs.json";
const MAX_RUNS: usize = 16;
const PROCESS_STOP_GRACE: Duration = Duration::from_millis(250);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const QUERY_EXIT_GRACE: Duration = Duration::from_millis(100);
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct ManagedAppSupervisorConfig {
    pub data_directory: PathBuf,
    /// Executables are matched by the exact argv[0] string.
    pub executable_allowlist: BTreeSet<String>,
    pub preview_sources: AuthorizedPreviewSourceRegistry,
}

#[derive(Debug)]
pub enum ManagedAppError {
    Invalid(String),
    Io(io::Error),
    Contract(winwincode_client_port::managed_app::ManagedAppContractError),
    LeaseMismatch,
    StaleLease,
    RunNotFound,
    AlreadyRunning,
    Spawn(String),
}

impl std::fmt::Display for ManagedAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(value) => write!(f, "managed app: invalid {value}"),
            Self::Io(error) => write!(f, "managed app state: {error}"),
            Self::Contract(error) => write!(f, "managed app contract: {error}"),
            Self::LeaseMismatch => f.write_str("managed app: lease does not match"),
            Self::StaleLease => f.write_str("managed app: stale lease"),
            Self::RunNotFound => f.write_str("managed app: run not found"),
            Self::AlreadyRunning => f.write_str("managed app: run is already running"),
            Self::Spawn(value) => write!(f, "managed app spawn: {value}"),
        }
    }
}

impl std::error::Error for ManagedAppError {}
impl From<io::Error> for ManagedAppError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<winwincode_client_port::managed_app::ManagedAppContractError> for ManagedAppError {
    fn from(error: winwincode_client_port::managed_app::ManagedAppContractError) -> Self {
        Self::Contract(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRun {
    config: ManagedAppRunConfig,
    lease_id: String,
    fencing_token: u64,
    state: ManagedAppState,
    pid: Option<u32>,
    process_start_identity: Option<String>,
    exit_code: Option<i32>,
    /// Canonical repository binding used for this run. This prevents a
    /// replayed run id from being attached to another local checkout.
    #[serde(default)]
    repository_root: Option<PathBuf>,
    /// Detached checkout used by a frozen candidate, if any.
    #[serde(default)]
    execution_root: Option<PathBuf>,
    /// The configured loopback port was free immediately before this process
    /// was spawned. This is the durable evidence used during recovery before
    /// a healthy endpoint is exposed as a preview source.
    #[serde(default)]
    port_preflight_passed: bool,
}

struct LiveRun {
    child: Child,
}

#[derive(Clone)]
pub struct ManagedAppSupervisor {
    config: Arc<ManagedAppSupervisorConfig>,
    state: Arc<Mutex<BTreeMap<String, PersistedRun>>>,
    live: Arc<Mutex<BTreeMap<String, LiveRun>>>,
}

impl std::fmt::Debug for ManagedAppSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedAppSupervisor")
            .field("data_directory", &self.config.data_directory)
            .finish_non_exhaustive()
    }
}

impl ManagedAppSupervisor {
    /// # Errors
    ///
    /// Returns an error when the state directory cannot be created or the
    /// persisted state cannot be read or decoded.
    pub fn open(config: ManagedAppSupervisorConfig) -> Result<Self, ManagedAppError> {
        fs::create_dir_all(&config.data_directory)?;
        let mut config = config;
        config.data_directory = config.data_directory.canonicalize()?;
        let path = config.data_directory.join(STATE_FILE);
        let state = if path.exists() {
            serde_json::from_slice(&fs::read(path)?)
                .map_err(|_| ManagedAppError::Invalid("state".to_owned()))?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(state)),
            live: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Reconciles persisted process identities after a Device restart. A
    /// mismatched PID is marked missing and is never signalled.
    /// # Errors
    ///
    /// Returns an error when the persisted state cannot be written or the
    /// supervisor state lock is poisoned.
    pub fn reconcile(&self) -> Result<Vec<ManagedAppStatus>, ManagedAppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ManagedAppError::Invalid("state lock poisoned".to_owned()))?;
        let mut cleanup = Vec::new();
        for run in state.values_mut() {
            if !matches!(
                run.state,
                ManagedAppState::Starting | ManagedAppState::Healthy | ManagedAppState::Unhealthy
            ) {
                continue;
            }
            let Some(pid) = run.pid else {
                run.state = ManagedAppState::Missing;
                self.config.preview_sources.remove(&run.config.source_id);
                continue;
            };
            if process_matches(pid, run.process_start_identity.as_deref()) {
                run.state = managed_health_state(&run.config, pid);
                if preview_ready(run) {
                    if let Ok(source) = preview_source(&run.config) {
                        self.config.preview_sources.insert(source);
                    }
                } else {
                    self.config.preview_sources.remove(&run.config.source_id);
                }
            } else if process_exists(pid) {
                run.state = ManagedAppState::Missing;
                run.pid = None;
                self.config.preview_sources.remove(&run.config.source_id);
            } else {
                run.state = ManagedAppState::Exited;
                run.pid = None;
                self.config.preview_sources.remove(&run.config.source_id);
                cleanup.push((run.repository_root.clone(), run.execution_root.clone()));
            }
        }
        for (repository, path) in cleanup {
            if let (Some(repository), Some(path)) = (repository, path) {
                cleanup_frozen_checkout(&self.config.data_directory, &repository, &path);
            }
        }
        let statuses = state.values().map(status).collect::<Vec<_>>();
        self.persist_locked(&state)?;
        Ok(statuses)
    }

    /// # Errors
    ///
    /// Returns an error when the command is invalid, its lease is stale, the
    /// run cannot be started or stopped, or supervisor state is unavailable.
    pub fn apply(
        &self,
        command: &ManagedAppCommand,
        repository_root: &Path,
    ) -> Result<ManagedAppStatus, ManagedAppError> {
        command.validate()?;
        match command.operation {
            ManagedAppOperation::Start => {
                let config = command
                    .config
                    .as_ref()
                    .ok_or_else(|| ManagedAppError::Invalid("config".to_owned()))?;
                self.start(command, config, repository_root, false)
            }
            ManagedAppOperation::Restart => {
                let config = command
                    .config
                    .as_ref()
                    .ok_or_else(|| ManagedAppError::Invalid("config".to_owned()))?;
                self.start(command, config, repository_root, true)
            }
            ManagedAppOperation::Stop => self.stop(command),
            ManagedAppOperation::Query => self.query(&command.run_id),
        }
    }

    /// # Errors
    ///
    /// Returns an error when the supervisor state lock is poisoned or the run
    /// does not exist.
    pub fn query(&self, run_id: &str) -> Result<ManagedAppStatus, ManagedAppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ManagedAppError::Invalid("state lock poisoned".to_owned()))?;
        let mut exited = None;
        {
            let mut live = self
                .live
                .lock()
                .map_err(|_| ManagedAppError::Invalid("live lock poisoned".to_owned()))?;
            if let Some(entry) = live.get_mut(run_id) {
                if let Some(exit) = wait_for_child_exit(&mut entry.child, QUERY_EXIT_GRACE)? {
                    exited = Some(exit.code());
                }
                if exited.is_some() {
                    live.remove(run_id);
                }
            }
        }
        let mut cleanup = None;
        if let Some(run) = state.get_mut(run_id) {
            if let Some(exit_code) = exited {
                run.state = ManagedAppState::Exited;
                run.exit_code = exit_code;
                run.pid = None;
                cleanup = run.repository_root.clone().zip(run.execution_root.clone());
                self.config.preview_sources.remove(&run.config.source_id);
            } else if let Some(pid) = run
                .pid
                .filter(|pid| process_matches(*pid, run.process_start_identity.as_deref()))
            {
                run.state = managed_health_state(&run.config, pid);
                if preview_ready(run) {
                    if let Ok(source) = preview_source(&run.config) {
                        self.config.preview_sources.insert(source);
                    }
                } else {
                    self.config.preview_sources.remove(&run.config.source_id);
                }
            } else if let Some(pid) = run.pid {
                run.state = if process_exists(pid) {
                    ManagedAppState::Missing
                } else {
                    cleanup = run.repository_root.clone().zip(run.execution_root.clone());
                    ManagedAppState::Exited
                };
                run.pid = None;
                self.config.preview_sources.remove(&run.config.source_id);
            }
            let result = status(run);
            if cleanup.is_some() || exited.is_some() {
                self.persist_locked(&state)?;
            }
            if let Some((repository, path)) = cleanup {
                cleanup_frozen_checkout(&self.config.data_directory, &repository, &path);
            }
            return Ok(result);
        }
        Err(ManagedAppError::RunNotFound)
    }

    #[allow(clippy::too_many_lines)]
    fn start(
        &self,
        command: &ManagedAppCommand,
        run_config: &ManagedAppRunConfig,
        repository_root: &Path,
        restart: bool,
    ) -> Result<ManagedAppStatus, ManagedAppError> {
        let canonical_repository_root = repository_root
            .canonicalize()
            .map_err(|_| ManagedAppError::Invalid("repository root".to_owned()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ManagedAppError::Invalid("state lock poisoned".to_owned()))?;
        let previous = state.get(&run_config.run_id).cloned();
        if let Some(previous) = &previous {
            validate_lease(previous, command)?;
            if previous.config != *run_config {
                return Err(ManagedAppError::Invalid("run config drift".to_owned()));
            }
            if previous.repository_root.as_ref() != Some(&canonical_repository_root) {
                return Err(ManagedAppError::Invalid("repository root drift".to_owned()));
            }
            if matches!(
                previous.state,
                ManagedAppState::Starting | ManagedAppState::Healthy | ManagedAppState::Unhealthy
            ) {
                if self
                    .live
                    .lock()
                    .map_err(|_| ManagedAppError::Invalid("live lock poisoned".to_owned()))?
                    .contains_key(&run_config.run_id)
                    && !restart
                {
                    return Ok(status(previous));
                }
                if !restart
                    && previous.pid.is_some_and(|pid| {
                        process_matches(pid, previous.process_start_identity.as_deref())
                    })
                {
                    return Ok(status(previous));
                }
                let live = self
                    .live
                    .lock()
                    .map_err(|_| ManagedAppError::Invalid("live lock poisoned".to_owned()))?
                    .remove(&run_config.run_id);
                let release = stop_owned_process(
                    previous.pid,
                    previous.process_start_identity.as_deref(),
                    live,
                );
                if release == ProcessRelease::Alive {
                    return Err(ManagedAppError::Invalid(
                        "previous process is still running".to_owned(),
                    ));
                }
                let cleanup_previous = release == ProcessRelease::Gone;
                if cleanup_previous
                    && let (Some(repository), Some(worktree)) = (
                        previous.repository_root.as_ref(),
                        previous.execution_root.as_ref(),
                    )
                {
                    cleanup_frozen_checkout(&self.config.data_directory, repository, worktree);
                }
                self.config
                    .preview_sources
                    .remove(&previous.config.source_id);
            }
        }
        if state.len() >= MAX_RUNS && previous.is_none() {
            return Err(ManagedAppError::Invalid("run capacity".to_owned()));
        }
        let executable = run_config
            .argv
            .first()
            .ok_or_else(|| ManagedAppError::Invalid("argv".to_owned()))?;
        if !self.config.executable_allowlist.contains(executable) {
            return Err(ManagedAppError::Invalid(
                "executable is not allowlisted".to_owned(),
            ));
        }
        let listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), run_config.listen_port);
        preflight_listen_port(listen_address)?;
        let (cwd, execution_root) =
            self.prepare_execution_root(&canonical_repository_root, run_config)?;
        let source = preview_source(run_config)?;
        let mut process = Command::new(executable);
        process
            .args(&run_config.argv[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(&run_config.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut process, 0);
        let mut child = process.spawn().map_err(|error| {
            if let Some(path) = execution_root.as_ref() {
                cleanup_frozen_checkout(
                    &self.config.data_directory,
                    &canonical_repository_root,
                    path,
                );
            }
            ManagedAppError::Spawn(error.to_string())
        })?;
        let pid = child.id();
        let Some(process_start_identity) = process_start_identity(pid) else {
            kill_process_group(pid);
            let _ = child.wait();
            if let Some(path) = execution_root.as_ref() {
                cleanup_frozen_checkout(
                    &self.config.data_directory,
                    &canonical_repository_root,
                    path,
                );
            }
            return Err(ManagedAppError::Spawn(
                "process identity unavailable".to_owned(),
            ));
        };
        let persisted = PersistedRun {
            config: run_config.clone(),
            lease_id: command.occupancy_lease_id.clone(),
            fencing_token: command.occupancy_fencing_token,
            state: ManagedAppState::Starting,
            pid: Some(pid),
            process_start_identity: Some(process_start_identity),
            exit_code: None,
            repository_root: Some(canonical_repository_root.clone()),
            execution_root,
            port_preflight_passed: true,
        };
        state.insert(run_config.run_id.clone(), persisted.clone());
        if let Err(error) = self.persist_locked(&state) {
            state.remove(&run_config.run_id);
            drop(state);
            kill_process_group(pid);
            let _ = child.wait();
            if let Some(path) = persisted.execution_root.as_ref() {
                cleanup_frozen_checkout(
                    &self.config.data_directory,
                    &canonical_repository_root,
                    path,
                );
            }
            return Err(error);
        }
        drop(state);
        if let Err(error) = self
            .live
            .lock()
            .map_err(|_| ManagedAppError::Invalid("live lock poisoned".to_owned()))
            .map(|mut live| {
                live.insert(run_config.run_id.clone(), LiveRun { child });
            })
        {
            kill_process_group(pid);
            if let Ok(mut state) = self.state.lock() {
                state.remove(&run_config.run_id);
                let _ = self.persist_locked(&state);
            }
            if let Some(path) = persisted.execution_root.as_ref() {
                cleanup_frozen_checkout(
                    &self.config.data_directory,
                    &canonical_repository_root,
                    path,
                );
            }
            return Err(error);
        }
        if wait_for_startup_ready(run_config, pid, persisted.process_start_identity.as_deref()) {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ManagedAppError::Invalid("state lock poisoned".to_owned()))?;
            let run = state
                .get_mut(&run_config.run_id)
                .ok_or(ManagedAppError::RunNotFound)?;
            run.state = ManagedAppState::Healthy;
            let result = status(run);
            self.persist_locked(&state)?;
            self.config.preview_sources.insert(source);
            Ok(result)
        } else {
            Ok(status(&persisted))
        }
    }

    fn prepare_execution_root(
        &self,
        repository_root: &Path,
        run_config: &ManagedAppRunConfig,
    ) -> Result<(PathBuf, Option<PathBuf>), ManagedAppError> {
        if run_config.mode == winwincode_client_port::managed_app::ManagedAppMode::Live {
            return Ok((confined_cwd(repository_root, &run_config.cwd)?, None));
        }
        let worktree = self
            .config
            .data_directory
            .join("managed-app-worktrees")
            .join(safe_run_component(&run_config.run_id));
        if worktree.exists() {
            return Err(ManagedAppError::Invalid(
                "frozen candidate checkout already exists".to_owned(),
            ));
        }
        fs::create_dir_all(worktree.parent().expect("worktree parent"))?;
        let commit = run_config
            .candidate_commit
            .as_deref()
            .ok_or_else(|| ManagedAppError::Invalid("candidate commit".to_owned()))?;
        let output = git_command(repository_root)
            .args(["worktree", "add", "--quiet", "--detach", "--"])
            .arg(&worktree)
            .arg(commit)
            .output()
            .map_err(|error| ManagedAppError::Spawn(format!("git worktree add: {error}")))?;
        if !output.status.success() {
            cleanup_frozen_checkout(&self.config.data_directory, repository_root, &worktree);
            return Err(ManagedAppError::Spawn(format!(
                "git worktree add: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if git_head(&worktree).as_deref() != Some(commit) {
            cleanup_frozen_checkout(&self.config.data_directory, repository_root, &worktree);
            return Err(ManagedAppError::Spawn(
                "frozen candidate checkout has the wrong HEAD".to_owned(),
            ));
        }
        let cwd = confined_cwd(&worktree, &run_config.cwd).inspect_err(|_| {
            cleanup_frozen_checkout(&self.config.data_directory, repository_root, &worktree);
        })?;
        Ok((cwd, Some(worktree)))
    }

    fn stop(&self, command: &ManagedAppCommand) -> Result<ManagedAppStatus, ManagedAppError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ManagedAppError::Invalid("state lock poisoned".to_owned()))?;
        let snapshot = state
            .get(&command.run_id)
            .ok_or(ManagedAppError::RunNotFound)?
            .clone();
        validate_lease(&snapshot, command)?;
        let live = self
            .live
            .lock()
            .map_err(|_| ManagedAppError::Invalid("live lock poisoned".to_owned()))?
            .remove(&command.run_id);
        let release = stop_owned_process(
            snapshot.pid,
            snapshot.process_start_identity.as_deref(),
            live,
        );
        let run = state
            .get_mut(&command.run_id)
            .ok_or(ManagedAppError::RunNotFound)?;
        match release {
            ProcessRelease::Gone => {
                run.state = ManagedAppState::Stopped;
                run.pid = None;
                self.config.preview_sources.remove(&run.config.source_id);
                if let Some(path) = run.execution_root.as_ref()
                    && let Some(repository) = run.repository_root.as_ref()
                {
                    cleanup_frozen_checkout(&self.config.data_directory, repository, path);
                }
            }
            ProcessRelease::Replaced => {
                run.state = ManagedAppState::Missing;
                run.pid = None;
                self.config.preview_sources.remove(&run.config.source_id);
            }
            ProcessRelease::Alive => {
                run.state = ManagedAppState::Missing;
            }
        }
        let result = status(run);
        self.persist_locked(&state)?;
        Ok(result)
    }

    fn persist_locked(
        &self,
        state: &BTreeMap<String, PersistedRun>,
    ) -> Result<(), ManagedAppError> {
        let path = self.config.data_directory.join(STATE_FILE);
        let temp = path.with_extension("tmp");
        fs::write(
            &temp,
            serde_json::to_vec_pretty(state)
                .map_err(|_| ManagedAppError::Invalid("state".to_owned()))?,
        )?;
        fs::rename(temp, path)?;
        Ok(())
    }
}

fn status(run: &PersistedRun) -> ManagedAppStatus {
    ManagedAppStatus {
        schema_version: winwincode_client_port::managed_app::MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION
            .to_owned(),
        run_id: run.config.run_id.clone(),
        state: run.state,
        pid: run.pid,
        process_start_identity: run.process_start_identity.clone(),
        exit_code: run.exit_code,
        source_id: run.config.source_id.clone(),
    }
}

fn preview_source(
    run_config: &ManagedAppRunConfig,
) -> Result<AuthorizedPreviewSource, ManagedAppError> {
    AuthorizedPreviewSource::new(
        winwincode_client_port::preview::PreviewSourceDescriptor {
            source_id: run_config.source_id.clone(),
            work_run_id: run_config.run_id.clone(),
            repository_binding_id: run_config.repository_binding_id.clone(),
            mode: match run_config.mode {
                winwincode_client_port::managed_app::ManagedAppMode::Live => {
                    winwincode_client_port::preview::PreviewSourceMode::Live
                }
                winwincode_client_port::managed_app::ManagedAppMode::FrozenCandidate => {
                    winwincode_client_port::preview::PreviewSourceMode::FrozenCandidate
                }
            },
            candidate_commit: run_config.candidate_commit.clone(),
        },
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), run_config.listen_port),
    )
    .map_err(|_| ManagedAppError::Invalid("preview source".to_owned()))
}

fn validate_lease(run: &PersistedRun, command: &ManagedAppCommand) -> Result<(), ManagedAppError> {
    if run.lease_id != command.occupancy_lease_id {
        return Err(ManagedAppError::LeaseMismatch);
    }
    if command.occupancy_fencing_token < run.fencing_token {
        return Err(ManagedAppError::StaleLease);
    }
    Ok(())
}

fn confined_cwd(root: &Path, relative: &str) -> Result<PathBuf, ManagedAppError> {
    let root = root
        .canonicalize()
        .map_err(|_| ManagedAppError::Invalid("repository root".to_owned()))?;
    let candidate = root.join(relative);
    let cwd = candidate
        .canonicalize()
        .map_err(|_| ManagedAppError::Invalid("cwd".to_owned()))?;
    if cwd == root || cwd.starts_with(root.join(".")) || cwd.starts_with(&root) {
        Ok(cwd)
    } else {
        Err(ManagedAppError::Invalid(
            "cwd escapes repository".to_owned(),
        ))
    }
}

fn process_start_identity(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn process_exists(pid: u32) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output();
    output.is_ok_and(|output| {
        output.status.success()
            && !String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
    })
}

fn process_is_zombie(pid: u32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z')
}

fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    if let Some(status) = child.try_wait()? {
        return Ok(Some(status));
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        std::thread::sleep(PROCESS_POLL_INTERVAL);
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessRelease {
    Gone,
    Replaced,
    Alive,
}

fn stop_owned_process(
    pid: Option<u32>,
    identity: Option<&str>,
    mut live: Option<LiveRun>,
) -> ProcessRelease {
    let Some(pid) = pid.or_else(|| live.as_ref().map(|run| run.child.id())) else {
        return ProcessRelease::Gone;
    };
    if let Some(live) = live.as_mut()
        && live.child.try_wait().ok().flatten().is_some()
    {
        return ProcessRelease::Gone;
    }
    let group = process_group_id(pid).filter(|group| *group == pid);
    if !process_matches(pid, identity) && live.is_none() {
        return observe_process_release(pid, identity, group);
    }
    if group.is_none() {
        return observe_process_release(pid, identity, group);
    }
    signal_process_group(pid, "TERM");
    let mut release = if let Some(live) = live.as_mut() {
        let _ = wait_for_child_exit(&mut live.child, PROCESS_STOP_GRACE);
        observe_process_release(pid, identity, group)
    } else {
        wait_for_process_release(pid, identity, group, PROCESS_STOP_GRACE)
    };
    if release == ProcessRelease::Alive {
        signal_process_group(pid, "KILL");
        if let Some(live) = live.as_mut() {
            let _ = wait_for_child_exit(&mut live.child, PROCESS_STOP_GRACE);
            release = observe_process_release(pid, identity, group);
        } else {
            release = wait_for_process_release(pid, identity, group, PROCESS_STOP_GRACE);
        }
    }
    release
}

fn wait_for_process_release(
    pid: u32,
    identity: Option<&str>,
    group: Option<u32>,
    timeout: Duration,
) -> ProcessRelease {
    let deadline = Instant::now() + timeout;
    loop {
        let release = observe_process_release(pid, identity, group);
        if release != ProcessRelease::Alive || Instant::now() >= deadline {
            return release;
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn observe_process_release(pid: u32, identity: Option<&str>, group: Option<u32>) -> ProcessRelease {
    if process_matches(pid, identity) || group.is_some_and(process_group_exists) {
        ProcessRelease::Alive
    } else if process_exists(pid) {
        ProcessRelease::Replaced
    } else {
        ProcessRelease::Gone
    }
}

fn process_matches(pid: u32, identity: Option<&str>) -> bool {
    identity.is_some_and(|expected| {
        !process_is_zombie(pid) && process_start_identity(pid).as_deref() == Some(expected)
    })
}

fn preview_ready(run: &PersistedRun) -> bool {
    run.port_preflight_passed
        && run.state == ManagedAppState::Healthy
        && run
            .pid
            .is_some_and(|pid| listening_socket_owned_by_process_group(pid, run.config.listen_port))
}

fn preflight_listen_port(address: SocketAddr) -> Result<(), ManagedAppError> {
    TcpListener::bind(address)
        .map(drop)
        .map_err(|_| ManagedAppError::Invalid("listen port is already in use".to_owned()))
}

fn wait_for_startup_ready(
    config: &ManagedAppRunConfig,
    pid: u32,
    process_identity: Option<&str>,
) -> bool {
    let deadline = Instant::now() + STARTUP_READY_TIMEOUT;
    loop {
        if health_state(config) == ManagedAppState::Healthy
            && listening_socket_owned_by_process_group(pid, config.listen_port)
        {
            return true;
        }
        if !process_matches(pid, process_identity) || Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn listening_socket_owned_by_process_group(pid: u32, port: u16) -> bool {
    let Some(group) = process_group_id(pid) else {
        return false;
    };
    let Some(owners) = listening_socket_owner_pids(port) else {
        return false;
    };
    !owners.is_empty()
        && owners.into_iter().all(|owner| {
            owner == pid || process_group_id(owner).is_some_and(|owner_group| owner_group == group)
        })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn listening_socket_owned_by_process_group(_pid: u32, _port: u16) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn listening_socket_owner_pids(port: u16) -> Option<BTreeSet<u32>> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('p')?.parse().ok())
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn listening_socket_owner_pids(port: u16) -> Option<BTreeSet<u32>> {
    let mut inodes = BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let contents = fs::read_to_string(table).ok()?;
        inodes.extend(parse_listening_socket_inodes(&contents, port));
    }
    if inodes.is_empty() {
        return Some(BTreeSet::new());
    }
    let mut owners = BTreeSet::new();
    for entry in fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Ok(owner) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let fd_directory = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(fd_directory) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_string_lossy()
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            if inodes.contains(&inode) {
                owners.insert(owner);
                break;
            }
        }
    }
    Some(owners)
}

#[cfg(any(target_os = "linux", test))]
fn parse_listening_socket_inodes(contents: &str, port: u16) -> BTreeSet<u64> {
    // /proc combines queue and timer pairs with colons; inode is token 9.
    let wanted_port = format!("{port:04X}");
    contents
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let local_port = fields.get(1)?.rsplit_once(':')?.1;
            (local_port.eq_ignore_ascii_case(&wanted_port) && fields.get(3) == Some(&"0A"))
                .then(|| fields.get(9)?.parse::<u64>().ok())
                .flatten()
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn listening_socket_owner_pids(_port: u16) -> Option<BTreeSet<u32>> {
    None
}

fn managed_health_state(config: &ManagedAppRunConfig, pid: u32) -> ManagedAppState {
    if health_state(config) == ManagedAppState::Healthy
        && listening_socket_owned_by_process_group(pid, config.listen_port)
    {
        ManagedAppState::Healthy
    } else {
        ManagedAppState::Unhealthy
    }
}

fn health_state(config: &ManagedAppRunConfig) -> ManagedAppState {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.listen_port);
    let timeout = Duration::from_millis(u64::from(config.health_check.timeout_ms));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return ManagedAppState::Unhealthy;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        config.health_check.path
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return ManagedAppState::Unhealthy;
    }
    let mut response = [0_u8; 32];
    let Ok(bytes) = stream.read(&mut response) else {
        return ManagedAppState::Unhealthy;
    };
    if response[..bytes].starts_with(b"HTTP/1.")
        && response[..bytes]
            .get(9..12)
            .is_some_and(|code| code[0] == b'2')
    {
        ManagedAppState::Healthy
    } else {
        ManagedAppState::Unhealthy
    }
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        signal_process_group(pid, "TERM");
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) {
    let Some(group) = process_group_id(pid).filter(|group| *group == pid) else {
        return;
    };
    let signalled = Command::new("/bin/kill")
        .args([format!("-{signal}"), format!("-{group}")])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !signalled {
        for member in process_group_members(group) {
            let _ = Command::new("/bin/kill")
                .args([format!("-{signal}"), member.to_string()])
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pid: u32, _signal: &str) {}

#[cfg(unix)]
fn process_group_exists(group: u32) -> bool {
    !process_group_members(group).is_empty()
}

#[cfg(unix)]
fn process_group_members(group: u32) -> Vec<u32> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let process_id = fields.next()?.parse().ok()?;
            let process_group: u32 = fields.next()?.parse().ok()?;
            let status = fields.next()?;
            (process_group == group && !status.starts_with('Z')).then_some(process_id)
        })
        .collect()
}

#[cfg(not(unix))]
fn process_group_exists(_group: u32) -> bool {
    false
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> Option<u32> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "pgid="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
        .flatten()
}

#[cfg(not(unix))]
fn process_group_id(_pid: u32) -> Option<u32> {
    None
}

fn safe_run_component(run_id: &str) -> String {
    let mut component = String::with_capacity(run_id.len());
    for byte in run_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            component.push(char::from(byte));
        } else {
            let _ = write!(component, "~{byte:02x}");
        }
    }
    component
}

fn git_command(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "commit.gpgsign=false"]);
    command
}

fn git_head(repository: &Path) -> Option<String> {
    let output = git_command(repository)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn cleanup_frozen_checkout(data_directory: &Path, repository: &Path, worktree: &Path) {
    if !worktree.starts_with(data_directory) {
        return;
    }
    let _ = git_command(repository)
        .args(["worktree", "remove", "--force", "--"])
        .arg(worktree)
        .output();
    let _ = git_command(repository).args(["worktree", "prune"]).output();
    let _ = fs::remove_dir_all(worktree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};
    use winwincode_client_port::managed_app::{
        MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION, ManagedAppHealthCheck, ManagedAppMode,
    };

    fn command(run_id: &str, operation: ManagedAppOperation) -> ManagedAppCommand {
        let mut port_hasher = std::collections::hash_map::DefaultHasher::new();
        run_id.hash(&mut port_hasher);
        let listen_port = 20_000 + (port_hasher.finish() % 30_000) as u16;
        ManagedAppCommand {
            schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
            operation,
            idempotency_key: "idem_demo".to_owned(),
            occupancy_lease_id: "lease_demo".to_owned(),
            occupancy_fencing_token: 1,
            config: (operation != ManagedAppOperation::Stop
                && operation != ManagedAppOperation::Query)
                .then(|| ManagedAppRunConfig {
                    schema_version: MANAGED_APP_RUN_CONFIG_SCHEMA_VERSION.to_owned(),
                    run_id: run_id.to_owned(),
                    repository_binding_id: "rbd_demo".to_owned(),
                    template_revision: 1,
                    attempt: 1,
                    mode: ManagedAppMode::Live,
                    candidate_commit: None,
                    cwd: ".".to_owned(),
                    argv: vec!["/bin/sleep".to_owned(), "30".to_owned()],
                    env: BTreeMap::new(),
                    health_check: ManagedAppHealthCheck {
                        path: "/health".to_owned(),
                        timeout_ms: 1000,
                    },
                    source_id: "pvs_demo".to_owned(),
                    listen_port,
                }),
            run_id: run_id.to_owned(),
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wwc-run03-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn node_path() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
            .map(|directory| directory.join("node"))
            .find(|candidate| candidate.is_file())
            .expect("node in test PATH")
    }

    #[cfg(test)]
    #[test]
    fn proc_tcp_parser_keeps_only_listening_target_ports() {
        let header = "sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode";
        let tcp = format!(
            "{header}\n0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 777 111 1\n1: 0100007F:1F91 00000000:0000 01 00000000:00000000 00:00000000 00000000 1000 888 222 1"
        );
        let tcp6 = format!(
            "{header}\n0: 00000000000000000000000001000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 999 333 1"
        );
        assert_eq!(
            parse_listening_socket_inodes(&tcp, 8080),
            BTreeSet::from([111])
        );
        assert_eq!(
            parse_listening_socket_inodes(&tcp6, 8080),
            BTreeSet::from([333])
        );
    }

    fn git(repository: &Path, arguments: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git")
    }

    #[test]
    fn start_is_idempotent_and_stale_stop_is_rejected() {
        let root = std::env::temp_dir().join(format!("wwc-run03-root-{}", std::process::id()));
        let state = std::env::temp_dir().join(format!("wwc-run03-state-{}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sleep")]),
            preview_sources: AuthorizedPreviewSourceRegistry::default(),
        })
        .expect("supervisor");
        let start = command("wrn_run_demo", ManagedAppOperation::Start);
        let first = supervisor.apply(&start, &root).expect("start");
        let second = supervisor.apply(&start, &root).expect("replay");
        assert_eq!(first.pid, second.pid);
        let registry = AuthorizedPreviewSourceRegistry::default();
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sleep")]),
            preview_sources: registry.clone(),
        })
        .expect("reopened supervisor");
        let reconciled = supervisor.reconcile().expect("reconcile");
        assert_eq!(reconciled[0].pid, first.pid);
        assert!(registry.snapshot().is_empty());
        let mut restart = command("wrn_run_demo", ManagedAppOperation::Restart);
        restart.idempotency_key = "idem_restart".to_owned();
        let restarted = supervisor.apply(&restart, &root).expect("restart");
        assert_ne!(restarted.pid, first.pid);
        assert!(registry.snapshot().is_empty());
        let mut stale_command = command("wrn_run_demo", ManagedAppOperation::Stop);
        stale_command.occupancy_fencing_token = 0;
        assert!(supervisor.apply(&stale_command, &root).is_err());
        let stopped = supervisor
            .apply(&command("wrn_run_demo", ManagedAppOperation::Stop), &root)
            .expect("stop");
        assert_eq!(stopped.state, ManagedAppState::Stopped);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn frozen_candidate_uses_the_pinned_commit_checkout() {
        let root = test_directory("candidate-root");
        let state = test_directory("candidate-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        assert!(git(&root, &["init", "--quiet"]).status.success());
        assert!(
            git(&root, &["config", "user.email", "test@example.invalid"])
                .status
                .success()
        );
        assert!(
            git(&root, &["config", "user.name", "WinWinCode Test"])
                .status
                .success()
        );
        fs::write(root.join("candidate.txt"), "pinned").expect("candidate file");
        assert!(git(&root, &["add", "candidate.txt"]).status.success());
        assert!(
            git(&root, &["commit", "--quiet", "-m", "candidate"])
                .status
                .success()
        );
        let candidate = String::from_utf8_lossy(&git(&root, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_owned();

        let config = ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sh")]),
            preview_sources: AuthorizedPreviewSourceRegistry::default(),
        };
        let supervisor = ManagedAppSupervisor::open(config).expect("supervisor");
        let mut start = command("wrn_frozen_demo", ManagedAppOperation::Start);
        let run_config = start.config.as_mut().expect("config");
        run_config.mode = ManagedAppMode::FrozenCandidate;
        run_config.candidate_commit = Some(candidate.clone());
        run_config.argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("pwd > {}/pwd; sleep 30", state.display()),
        ];
        let started = supervisor.apply(&start, &root).expect("start");
        let execution_root = supervisor
            .state
            .lock()
            .expect("state lock")
            .get("wrn_frozen_demo")
            .and_then(|run| run.execution_root.clone())
            .expect("frozen checkout");
        assert_eq!(
            git_head(&execution_root).as_deref(),
            Some(candidate.as_str())
        );
        let _ = supervisor.apply(&start, &root).expect("idempotent replay");
        let drift = {
            let mut changed = start.clone();
            changed.config.as_mut().expect("config").attempt = 2;
            supervisor.apply(&changed, &root)
        };
        assert!(
            matches!(drift, Err(ManagedAppError::Invalid(message)) if message == "run config drift")
        );
        let pwd = fs::read_to_string(state.join("pwd")).expect("pwd output");
        assert_eq!(Path::new(pwd.trim()), execution_root);
        assert_eq!(started.state, ManagedAppState::Starting);
        let stopped = supervisor
            .apply(
                &command("wrn_frozen_demo", ManagedAppOperation::Stop),
                &root,
            )
            .expect("stop");
        assert_eq!(stopped.state, ManagedAppState::Stopped);
        assert!(!execution_root.exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn start_rejects_a_loopback_port_owned_before_spawn() {
        let root = test_directory("occupied-port-root");
        let state = test_directory("occupied-port-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
        let port = listener.local_addr().expect("listener address").port();
        let registry = AuthorizedPreviewSourceRegistry::default();
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sleep")]),
            preview_sources: registry.clone(),
        })
        .expect("supervisor");
        let mut start = command("wrn_occupied_port_demo", ManagedAppOperation::Start);
        start.config.as_mut().expect("config").listen_port = port;
        let result = supervisor.apply(&start, &root);
        assert!(matches!(
            result,
            Err(ManagedAppError::Invalid(message))
                if message == "listen port is already in use"
        ));
        assert!(registry.snapshot().is_empty());
        assert!(!state.join(STATE_FILE).exists());
        drop(listener);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn healthy_child_owns_the_port_before_preview_registration() {
        let root = test_directory("healthy-port-root");
        let state = test_directory("healthy-port-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let registry = AuthorizedPreviewSourceRegistry::default();
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sh")]),
            preview_sources: registry.clone(),
        })
        .expect("supervisor");
        let mut start = command("wrn_healthy_port_demo", ManagedAppOperation::Start);
        let run_config = start.config.as_mut().expect("config");
        let port = run_config.listen_port;
        run_config.argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "exec {} -e \"const http=require('node:http');http.createServer((req,res)=>{{res.writeHead(200);res.end('ok')}}).listen({port},'127.0.0.1')\"",
                node_path().display()
            ),
        ];
        let started = supervisor.apply(&start, &root).expect("start");
        assert_eq!(started.state, ManagedAppState::Healthy);
        assert_eq!(registry.snapshot().len(), 1);
        let stopped = supervisor
            .apply(
                &command("wrn_healthy_port_demo", ManagedAppOperation::Stop),
                &root,
            )
            .expect("stop");
        assert_eq!(stopped.state, ManagedAppState::Stopped);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn foreign_healthy_listener_after_preflight_cannot_be_registered() {
        let root = test_directory("foreign-port-root");
        let state = test_directory("foreign-port-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let registry = AuthorizedPreviewSourceRegistry::default();
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sleep")]),
            preview_sources: registry.clone(),
        })
        .expect("supervisor");
        let start = command("wrn_foreign_port_demo", ManagedAppOperation::Start);
        let port = start.config.as_ref().expect("config").listen_port;
        let started = supervisor.apply(&start, &root).expect("start");
        assert_eq!(started.state, ManagedAppState::Starting);

        let mut foreign = Command::new(node_path())
            .args([
                "-e",
                &format!(
                    "const http=require('node:http');http.createServer((req,res)=>{{res.writeHead(200);res.end('foreign')}}).listen({port},'127.0.0.1')"
                ),
            ])
            .spawn()
            .expect("foreign listener");
        let config = start.config.as_ref().expect("config");
        let ready_deadline = Instant::now() + Duration::from_secs(2);
        while health_state(config) != ManagedAppState::Healthy && Instant::now() < ready_deadline {
            std::thread::sleep(PROCESS_POLL_INTERVAL);
        }
        let queried = supervisor
            .apply(
                &command("wrn_foreign_port_demo", ManagedAppOperation::Query),
                &root,
            )
            .expect("query");
        let stopped = supervisor
            .apply(
                &command("wrn_foreign_port_demo", ManagedAppOperation::Stop),
                &root,
            )
            .expect("stop");
        let _ = foreign.kill();
        let _ = foreign.wait();
        assert_eq!(queried.state, ManagedAppState::Unhealthy);
        assert!(registry.snapshot().is_empty());
        assert_eq!(stopped.state, ManagedAppState::Stopped);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn query_records_a_child_that_exited_without_a_poll() {
        let root = test_directory("exit-root");
        let state = test_directory("exit-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let supervisor = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sh")]),
            preview_sources: AuthorizedPreviewSourceRegistry::default(),
        })
        .expect("supervisor");
        let mut start = command("wrn_exit_demo", ManagedAppOperation::Start);
        start.config.as_mut().expect("config").argv =
            vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()];
        let _ = supervisor.apply(&start, &root).expect("start");
        let status = supervisor
            .apply(&command("wrn_exit_demo", ManagedAppOperation::Query), &root)
            .expect("query");
        assert_eq!(status.state, ManagedAppState::Exited);
        assert_eq!(status.exit_code, Some(7));
        assert_eq!(status.pid, None);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn stop_after_restart_escalates_when_term_is_ignored() {
        let root = test_directory("ignore-term-root");
        let state = test_directory("ignore-term-state");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&state).expect("state");
        let config = || ManagedAppSupervisorConfig {
            data_directory: state.clone(),
            executable_allowlist: BTreeSet::from([String::from("/bin/sh")]),
            preview_sources: AuthorizedPreviewSourceRegistry::default(),
        };
        let supervisor = ManagedAppSupervisor::open(config()).expect("supervisor");
        let mut start = command("wrn_ignore_term_demo", ManagedAppOperation::Start);
        start.config.as_mut().expect("config").argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "trap '' TERM; while :; do sleep 1; done".to_owned(),
        ];
        let started = supervisor.apply(&start, &root).expect("start");
        let pid = started.pid.expect("pid");
        drop(supervisor);
        let reopened = ManagedAppSupervisor::open(config()).expect("reopened supervisor");
        reopened.reconcile().expect("reconcile");
        let mut restart = start.clone();
        restart.operation = ManagedAppOperation::Restart;
        restart.idempotency_key = "idem_restart_ignores_term".to_owned();
        let restarted = reopened.apply(&restart, &root).expect("restart");
        assert_ne!(restarted.pid, Some(pid));
        assert!(
            !process_exists(pid),
            "restart must release the previous process"
        );
        let pid = restarted.pid.expect("restarted pid");
        let began = Instant::now();
        let stopped = reopened
            .apply(
                &command("wrn_ignore_term_demo", ManagedAppOperation::Stop),
                &root,
            )
            .expect("stop");
        assert_eq!(stopped.state, ManagedAppState::Stopped);
        assert!(began.elapsed() < Duration::from_secs(2));
        assert!(!process_exists(pid));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(state);
    }
}
