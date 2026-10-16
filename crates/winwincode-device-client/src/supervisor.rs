// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use time::OffsetDateTime;

use crate::daemon::{
    ExchangeTransport, LeaseWorkerController, WorkerCapacitySnapshot, WorkerCapacitySource,
};
use crate::fencing::{FencedCommandKind, FencingGuard, FencingRejection, FencingVerdict};
use crate::store::{DeviceStore, DeviceStoreError, WorkerProcessRecord};
use crate::worker_logs::{
    WorkerLogConfig, WorkerLogContentKind, WorkerLogRecorder, WorkerLogStream,
};

pub(crate) const PROTOCOL_WORKER_SESSION_CAPACITY: u32 = 1024;

pub const WORKER_STATE_RUNNING: &str = "running";
pub const WORKER_STATE_EXITED: &str = "exited";
pub const WORKER_STATE_CRASHED: &str = "crashed";
pub const WORKER_STATE_MISSING: &str = "missing";

const MANAGED_SESSION_CONFIG_FILE: &str = "managed-session.json";
const MANAGED_SESSION_ARG: &str = "--managed-session";
const WORKER_BINARY_NAME: &str = "winwincode-worker";
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorConfig {
    pub client_node_id: String,
    pub client_instance_id: String,
    pub server_origin: String,
    pub model_route: Option<ModelRoute>,
    pub worker_binary_path: Option<PathBuf>,
    pub max_concurrent_worker_sessions: u32,
    pub stop_grace_period: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            client_node_id: String::new(),
            client_instance_id: String::new(),
            server_origin: String::new(),
            model_route: None,
            worker_binary_path: None,
            max_concurrent_worker_sessions: 0,
            stop_grace_period: Duration::from_secs(20),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    pub capability: String,
    pub route: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnRequest<'a> {
    pub worker_session_id: &'a str,
    pub worker_id: &'a str,
    pub worker_instance_id: &'a str,
    pub occupancy_lease_id: &'a str,
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: &'a str,
    pub launch_grant_id: &'a str,
    pub product_session_id: Option<&'a str>,
    pub work_run_id: Option<&'a str>,
    pub source_directory: &'a Path,
    pub data_directory: &'a Path,
    pub worker_root: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerHandle {
    pub worker_session_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub pid: u32,
    pub process_start_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnOutcome {
    Started(WorkerHandle),
    AlreadyRunning(WorkerProcessRecord),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReapedWorker {
    pub worker_session_id: String,
    pub worker_instance_id: String,
    pub state: &'static str,
    pub exit_code: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerStopOutcome {
    pub worker_session_id: String,
    pub stopped: bool,
    pub state: Option<String>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerReconcileVerdict {
    StillRunning,
    Terminal,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerReconcileReport {
    pub worker_session_id: String,
    pub worker_instance_id: String,
    pub pid: u32,
    pub process_start_identity: String,
    pub verdict: WorkerReconcileVerdict,
    pub record: WorkerProcessRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorError {
    Store(DeviceStoreError),
    Fencing(FencingRejection),
    NotFound { worker_session_id: String },
    UnverifiableExisting { worker_session_id: String, pid: u32 },
    CapacityExhausted { max_concurrent_worker_sessions: u32 },
    InvalidInput(String),
    Spawn { reason: String },
    WorkerLogs(String),
    ProcessIdentityUnavailable { pid: u32 },
}

impl SupervisorError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "worker supervisor store failure: {error}"),
            Self::Fencing(rejection) => {
                write!(
                    formatter,
                    "worker supervisor fencing rejection: {rejection:?}"
                )
            }
            Self::NotFound { worker_session_id } => write!(
                formatter,
                "worker session {worker_session_id} has no process registry row"
            ),
            Self::UnverifiableExisting {
                worker_session_id,
                pid,
            } => write!(
                formatter,
                "worker session {worker_session_id} claims a running process {pid} \
                 whose boot identity cannot be verified"
            ),
            Self::CapacityExhausted {
                max_concurrent_worker_sessions,
            } => write!(
                formatter,
                "local worker capacity is exhausted ({max_concurrent_worker_sessions} \
                 concurrent worker sessions)"
            ),
            Self::InvalidInput(message) => write!(formatter, "worker supervisor input: {message}"),
            Self::Spawn { reason } => write!(formatter, "worker process spawn failed: {reason}"),
            Self::WorkerLogs(reason) => write!(formatter, "worker log recorder failure: {reason}"),
            Self::ProcessIdentityUnavailable { pid } => write!(
                formatter,
                "the platform could not supply the boot identity of process {pid}"
            ),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<DeviceStoreError> for SupervisorError {
    fn from(error: DeviceStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<crate::worker_logs::WorkerLogError> for SupervisorError {
    fn from(error: crate::worker_logs::WorkerLogError) -> Self {
        Self::WorkerLogs(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessObservation {
    Boot {
        identity: String,
        working: bool,
    },
    Absent,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessProbe {
    Alive,
    Gone,
    Unknown,
}

struct TrackedChild {
    child: Child,
    log_readers: Vec<thread::JoinHandle<()>>,
}

struct SupervisorInner {
    config: SupervisorConfig,
    store: Mutex<DeviceStore>,
    children: Mutex<BTreeMap<String, TrackedChild>>,
    worker_logs: WorkerLogRecorder,
    worker_relay_transport: Mutex<Option<Arc<dyn ExchangeTransport>>>,
}

#[derive(Clone)]
pub struct SessionSupervisor {
    inner: Arc<SupervisorInner>,
}

impl fmt::Debug for SessionSupervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSupervisor")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl SessionSupervisor {
    pub fn new(config: SupervisorConfig, store: DeviceStore) -> Result<Self, SupervisorError> {
        validate_config(&config)?;
        let logs_root = store
            .database_path()
            .parent()
            .ok_or_else(|| SupervisorError::invalid("device data directory is unavailable"))?
            .join("worker-logs");
        let worker_logs = WorkerLogRecorder::open(&logs_root, WorkerLogConfig::default())?;
        let supervisor = Self {
            inner: Arc::new(SupervisorInner {
                config,
                store: Mutex::new(store),
                children: Mutex::new(BTreeMap::new()),
                worker_logs,
                worker_relay_transport: Mutex::new(None),
            }),
        };
        supervisor.prepare_provider_runtime()?;
        Ok(supervisor)
    }

    fn provider_directory(&self) -> Result<PathBuf, SupervisorError> {
        self.lock_store()
            .database_path()
            .parent()
            .map(|path| path.join("providers"))
            .ok_or_else(|| SupervisorError::invalid("device Provider directory is unavailable"))
    }

    fn prepare_provider_runtime(&self) -> Result<(), SupervisorError> {
        let directory = self.provider_directory()?;
        let database = directory.join("providers.sqlite3");
        if !database.exists() {
            return Ok(());
        }
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| SupervisorError::invalid("Provider storage version is unavailable"))?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| SupervisorError::invalid("Provider storage version is unavailable"))?;
        drop(connection);
        if version == 16 {
            for record in self.worker_processes()? {
                if record.state == WORKER_STATE_RUNNING {
                    self.stop_with_fencing(&record.worker_session_id, true, false)?;
                } else if probe_process(record.pid, &record.process_start_identity)
                    != ProcessProbe::Gone
                {
                    return Err(SupervisorError::UnverifiableExisting {
                        worker_session_id: record.worker_session_id,
                        pid: record.pid,
                    });
                }
            }
            winwincode_provider::DeviceProviderStore::open(&directory)
                .map_err(|_| SupervisorError::invalid("Provider runtime migration failed"))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn config(&self) -> &SupervisorConfig {
        &self.inner.config
    }

    /// Required request/result recovery before reporting a confirmed process
    /// closure. The shipped Worker delegates model and tool writes to their owners.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn recover_terminated_calls(
        &self,
        record: &WorkerProcessRecord,
    ) -> Result<(), SupervisorError> {
        if !matches!(
            record.state.as_str(),
            WORKER_STATE_EXITED | WORKER_STATE_CRASHED | WORKER_STATE_MISSING
        ) {
            return Err(SupervisorError::InvalidInput(
                "call recovery requires a terminated process".into(),
            ));
        }
        let config_path = Path::new(&record.data_directory).join(MANAGED_SESSION_CONFIG_FILE);
        let mut config: serde_json::Value = serde_json::from_slice(
            &fs::read(&config_path)
                .map_err(|error| SupervisorError::InvalidInput(error.to_string()))?,
        )
        .map_err(|error| SupervisorError::InvalidInput(error.to_string()))?;
        if config["workerSessionId"] != record.worker_session_id
            || config["workerInstanceId"] != record.worker_instance_id
        {
            return Err(SupervisorError::InvalidInput(
                "call recovery process identity mismatch".into(),
            ));
        }
        if config.get("providerDirectory").is_some() {
            let runtime_directory = self.provider_directory()?.join("runtime");
            let object = config
                .as_object_mut()
                .ok_or_else(|| SupervisorError::invalid("managed configuration is invalid"))?;
            object.remove("providerDirectory");
            object.insert(
                "providerRuntimeDirectory".into(),
                serde_json::json!(runtime_directory),
            );
            object.insert("providerExecutionProfile".into(), serde_json::json!({}));
            let bytes = serde_json::to_vec(&config)
                .map_err(|_| SupervisorError::invalid("managed configuration cannot be encoded"))?;
            let temporary = config_path.with_extension("runtime.tmp");
            write_private_file(&temporary, &bytes)?;
            fs::File::open(&temporary)
                .and_then(|file| file.sync_all())
                .map_err(|_| SupervisorError::invalid("managed configuration cannot be synced"))?;
            fs::rename(&temporary, &config_path).map_err(|_| {
                SupervisorError::invalid("managed configuration cannot be replaced")
            })?;
            if let Some(parent) = config_path.parent() {
                fs::File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|_| {
                        SupervisorError::invalid("managed configuration directory cannot be synced")
                    })?;
            }
        }
        let has_core = Path::new(&record.data_directory)
            .join("codex-runtime/kernel-home")
            .is_dir();
        let has_model = config["providerRuntimeDirectory"]
            .as_str()
            .is_some_and(|path| Path::new(path).join("provider-runtime.sqlite3").is_file());
        if !has_core && !has_model {
            return Ok(());
        }
        let recovery_stderr = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(Path::new(&record.data_directory).join("call-recovery.log"))
            .map_or_else(|_| Stdio::null(), Stdio::from);
        let status = Command::new(self.resolve_worker_binary()?)
            .arg("--recover-interrupted-calls")
            .arg(&config_path)
            .env_remove("WWC_DEVICE_CA_FILE")
            .env_remove("WWC_DEVICE_PAIRING_TOKEN")
            .env_remove("WWC_DEVICE_SERVER_URL")
            .env_remove("WWC_DEVICE_JEV_SETTINGS_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(recovery_stderr)
            .status()
            .map_err(|error| SupervisorError::Spawn {
                reason: error.to_string(),
            })?;
        if !status.success() {
            return Err(SupervisorError::InvalidInput(
                "terminated call recovery failed".into(),
            ));
        }
        Ok(())
    }

    pub fn worker_process(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerProcessRecord>, SupervisorError> {
        Ok(self.lock_store().worker_process(worker_session_id)?)
    }

    pub fn worker_processes(&self) -> Result<Vec<WorkerProcessRecord>, SupervisorError> {
        Ok(self.lock_store().worker_processes()?)
    }

    pub fn set_worker_relay_transport(&self, transport: Arc<dyn ExchangeTransport>) {
        *self
            .inner
            .worker_relay_transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(transport);
    }

    pub fn shutdown(&self) -> Result<(), SupervisorError> {
        let mut failure = None;
        for record in self.worker_processes()? {
            if record.state == WORKER_STATE_RUNNING {
                match self.stop_with_fencing(&record.worker_session_id, true, false) {
                    Ok(stopped) => {
                        if let Err(error) = self.recover_terminated_calls(&stopped) {
                            failure.get_or_insert(error);
                        }
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub fn count_worker_processes_in_state(&self, state: &str) -> Result<u64, SupervisorError> {
        Ok(self.lock_store().count_worker_processes_in_state(state)?)
    }

    pub fn running_worker_sessions(&self) -> Result<u32, SupervisorError> {
        self.reap()?;
        let rows = self
            .lock_store()
            .worker_report_candidates("reconcile", None)?;
        self.reconcile_rows(rows)?;
        let running = self.count_worker_processes_in_state(WORKER_STATE_RUNNING)?;
        Ok(u32::try_from(running).unwrap_or(u32::MAX))
    }

    #[allow(clippy::too_many_lines)]
    pub fn spawn(&self, request: SpawnRequest<'_>) -> Result<SpawnOutcome, SupervisorError> {
        let stamp = now_rfc3339();
        let mut store = self.lock_store();

        let guard = FencingGuard::from_store(&store)?;
        if let FencingVerdict::Rejected(rejection) = guard.authorize_command(
            FencedCommandKind::WorkerLaunch,
            request.occupancy_lease_id,
            request.occupancy_fencing_token,
        ) {
            return Err(SupervisorError::Fencing(rejection));
        }
        drop(guard);
        validate_request(request)?;

        if let Some(existing) = store.worker_process(request.worker_session_id)?
            && existing.state == WORKER_STATE_RUNNING
            && let Some(outcome) = self.already_running_outcome(&mut store, &existing, &stamp)?
        {
            return Ok(outcome);
        }

        let max = self.inner.config.max_concurrent_worker_sessions;
        if max > 0 {
            let running = store.count_worker_processes_in_state(WORKER_STATE_RUNNING)?;
            if running >= u64::from(max) {
                return Err(SupervisorError::CapacityExhausted {
                    max_concurrent_worker_sessions: max,
                });
            }
        }

        let worker_id = request.worker_id;
        let worker_instance_id = request.worker_instance_id;
        create_directory(request.data_directory, "worker data directory")?;
        create_directory(request.worker_root, "worker root")?;
        let config_path = request.data_directory.join(MANAGED_SESSION_CONFIG_FILE);
        let provider_directory = store
            .database_path()
            .parent()
            .ok_or_else(|| SupervisorError::invalid("device data directory is unavailable"))?
            .join("providers");
        let provider_store = winwincode_provider::DeviceProviderStore::open(&provider_directory)
            .map_err(|_| SupervisorError::invalid("Provider runtime is unavailable"))?;
        let runtime_directory = provider_store.runtime_directory().to_path_buf();
        drop(provider_store);
        let provider_execution_profile =
            crate::worker_channel::provider_execution_profile(&provider_directory)
                .map_err(|_| SupervisorError::invalid("Provider execution profile is invalid"))?;
        let config_json = managed_session_config_json(
            &self.inner.config,
            request,
            worker_id,
            worker_instance_id,
            &runtime_directory,
            &provider_execution_profile,
        )?;
        let mut config_value: serde_json::Value = serde_json::from_str(&config_json)
            .map_err(|_| SupervisorError::invalid("managed config cannot be decoded"))?;
        let workspace_root = store
            .database_path()
            .parent()
            .ok_or_else(|| SupervisorError::invalid("device data directory is missing"))?
            .join("execution-workspaces");
        create_directory(&workspace_root, "Device execution workspace directory")?;
        config_value["executionWorkspaceDirectory"] = serde_json::Value::String(
            workspace_root
                .to_str()
                .ok_or_else(|| {
                    SupervisorError::invalid("execution workspace directory is not UTF-8")
                })?
                .to_owned(),
        );
        let config_json = serde_json::to_string(&config_value)
            .map_err(|_| SupervisorError::invalid("managed config cannot be encoded"))?;
        write_private_file(&config_path, config_json.as_bytes())?;

        let mut child = self.launch_process(&config_path, request)?;
        let record = match self.register_child(
            &mut store,
            request,
            &mut child,
            worker_id.to_owned(),
            worker_instance_id.to_owned(),
            &stamp,
        ) {
            Ok(record) => record,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let log_readers = self.start_log_readers(
            &mut child,
            request,
            &provider_execution_profile,
            &provider_directory,
            &record,
        );
        self.lock_children().insert(
            request.worker_session_id.to_owned(),
            TrackedChild { child, log_readers },
        );

        Ok(SpawnOutcome::Started(WorkerHandle {
            worker_session_id: record.worker_session_id,
            worker_id: record.worker_id,
            worker_instance_id: record.worker_instance_id,
            pid: record.pid,
            process_start_identity: record.process_start_identity,
        }))
    }

    fn already_running_outcome(
        &self,
        store: &mut DeviceStore,
        existing: &WorkerProcessRecord,
        stamp: &str,
    ) -> Result<Option<SpawnOutcome>, SupervisorError> {
        match self.tracked_child_verdict(&existing.worker_session_id) {
            Some(ProcessProbe::Alive) => {
                return Ok(Some(SpawnOutcome::AlreadyRunning(existing.clone())));
            }
            Some(ProcessProbe::Gone) => {}
            _ => match probe_process(existing.pid, &existing.process_start_identity) {
                ProcessProbe::Alive => {
                    return Ok(Some(SpawnOutcome::AlreadyRunning(existing.clone())));
                }
                ProcessProbe::Gone => {
                    store.mark_worker_process_state(
                        &existing.worker_session_id,
                        WORKER_STATE_MISSING,
                        None,
                        stamp,
                    )?;
                }
                ProcessProbe::Unknown => {
                    return Err(SupervisorError::UnverifiableExisting {
                        worker_session_id: existing.worker_session_id.clone(),
                        pid: existing.pid,
                    });
                }
            },
        }
        Ok(None)
    }

    fn launch_process(
        &self,
        config_path: &Path,
        request: SpawnRequest<'_>,
    ) -> Result<Child, SupervisorError> {
        let binary = std::path::absolute(self.resolve_worker_binary()?).map_err(|error| {
            SupervisorError::Spawn {
                reason: error.to_string(),
            }
        })?;
        let mut command = Command::new(&binary);
        command
            .arg(MANAGED_SESSION_ARG)
            .arg(config_path)
            .env_remove("WWC_DEVICE_CA_FILE")
            .env_remove("WWC_DEVICE_PAIRING_TOKEN")
            .env_remove("WWC_DEVICE_SERVER_URL")
            .env_remove("WWC_DEVICE_JEV_SETTINGS_FILE")
            .current_dir(request.worker_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        command.spawn().map_err(|error| SupervisorError::Spawn {
            reason: format!("{}: {error}", binary.display()),
        })
    }

    fn start_log_readers(
        &self,
        child: &mut Child,
        request: SpawnRequest<'_>,
        profile: &winwincode_client_port::private_worker_channel::ProviderExecutionProfile,
        provider_directory: &Path,
        record: &WorkerProcessRecord,
    ) -> Vec<thread::JoinHandle<()>> {
        let mut readers = Vec::with_capacity(2);
        if let Some(pipe) = child.stdout.take()
            && let Some(input) = child.stdin.take()
        {
            readers.push(self.start_worker_relay(
                pipe,
                input,
                request,
                profile,
                provider_directory,
                record,
            ));
        }
        if let Some(pipe) = child.stderr.take() {
            readers.push(self.start_log_reader(pipe, WorkerLogStream::Stderr, request));
        }
        readers
    }

    fn start_worker_relay<R: Read + Send + 'static>(
        &self,
        output: R,
        input: std::process::ChildStdin,
        request: SpawnRequest<'_>,
        profile: &winwincode_client_port::private_worker_channel::ProviderExecutionProfile,
        provider_directory: &Path,
        record: &WorkerProcessRecord,
    ) -> thread::JoinHandle<()> {
        let endpoint = format!(
            "{}/internal/v1/client/worker-exchange",
            self.inner.config.server_origin
        );
        let node = self.inner.config.client_node_id.clone();
        let worker = request.worker_id.to_owned();
        let instance = request.worker_instance_id.to_owned();
        let transport = self
            .inner
            .worker_relay_transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let supervisor = self.clone();
        let expected = record.clone();
        let occupancy_token = request.occupancy_fencing_token;
        let live_process = Arc::new(move || {
            let store = supervisor.lock_store();
            let Ok(Some(current)) = store.worker_process(&expected.worker_session_id) else {
                return false;
            };
            if current.state != WORKER_STATE_RUNNING
                || current.worker_id != expected.worker_id
                || current.worker_instance_id != expected.worker_instance_id
                || current.pid != expected.pid
                || current.process_start_identity != expected.process_start_identity
                || current.launch_grant_id != expected.launch_grant_id
            {
                return false;
            }
            let Ok(guard) = FencingGuard::from_store(&store) else {
                return false;
            };
            matches!(
                guard.authorize_command(
                    FencedCommandKind::WorkerLaunch,
                    &expected.occupancy_lease_id,
                    occupancy_token
                ),
                FencingVerdict::Authorized(_)
            ) && probe_process(expected.pid, &expected.process_start_identity)
                == ProcessProbe::Alive
        });
        let broker = Arc::new(crate::provider_broker::ProviderBroker::new(
            provider_directory.to_path_buf(),
            request.data_directory.join("codex-runtime/kernel-home"),
            worker.clone(),
            instance.clone(),
            request.product_session_id.map(str::to_owned),
            request.work_run_id.map(str::to_owned),
            profile.clone(),
            live_process,
        ));
        thread::spawn(move || {
            crate::worker_channel::serve(
                output, input, endpoint, transport, node, worker, instance, &broker,
            );
        })
    }

    fn start_log_reader<R: Read + Send + 'static>(
        &self,
        mut pipe: R,
        stream: WorkerLogStream,
        request: SpawnRequest<'_>,
    ) -> thread::JoinHandle<()> {
        let recorder = self.inner.worker_logs.clone();
        let session = request.worker_session_id.to_owned();
        let instance = request.worker_instance_id.to_owned();
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(bytes) => {
                        let _ = recorder.append(
                            &session,
                            &instance,
                            stream,
                            WorkerLogContentKind::Diagnostic,
                            &buffer[..bytes],
                            &now_rfc3339(),
                        );
                    }
                }
            }
        })
    }

    fn register_child(
        &self,
        store: &mut DeviceStore,
        request: SpawnRequest<'_>,
        child: &mut Child,
        worker_id: String,
        worker_instance_id: String,
        stamp: &str,
    ) -> Result<WorkerProcessRecord, SupervisorError> {
        let pid = child.id();
        let process_start_identity = match observe_process(pid) {
            ProcessObservation::Boot { identity, .. } => identity,
            ProcessObservation::Absent | ProcessObservation::Unknown => {
                let _ = child.kill();
                let _ = child.wait();
                self.lock_children().remove(request.worker_session_id);
                return Err(SupervisorError::ProcessIdentityUnavailable { pid });
            }
        };
        match observe_process_group_id(pid) {
            Some(group) if group == pid => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                self.lock_children().remove(request.worker_session_id);
                return Err(SupervisorError::ProcessIdentityUnavailable { pid });
            }
        }

        let record = WorkerProcessRecord {
            worker_session_id: request.worker_session_id.to_owned(),
            worker_id,
            worker_instance_id,
            pid,
            process_start_identity,
            repository_binding_id: request.repository_binding_id.to_owned(),
            occupancy_lease_id: request.occupancy_lease_id.to_owned(),
            launch_grant_id: request.launch_grant_id.to_owned(),
            data_directory: request
                .data_directory
                .to_str()
                .unwrap_or_default()
                .to_owned(),
            state: WORKER_STATE_RUNNING.to_owned(),
            exit_code: None,
            last_observed_at: stamp.to_owned(),
        };
        store.put_worker_process(&record)?;
        Ok(record)
    }

    pub fn stop(
        &self,
        worker_session_id: &str,
        graceful: bool,
    ) -> Result<WorkerProcessRecord, SupervisorError> {
        self.stop_with_fencing(worker_session_id, graceful, true)
    }

    pub fn stop_recovered(
        &self,
        worker_session_id: &str,
        worker_id: &str,
        occupancy_lease_id: &str,
        graceful: bool,
    ) -> Result<WorkerProcessRecord, SupervisorError> {
        let record =
            self.worker_process(worker_session_id)?
                .ok_or_else(|| SupervisorError::NotFound {
                    worker_session_id: worker_session_id.to_owned(),
                })?;
        if record.occupancy_lease_id != occupancy_lease_id {
            return Err(SupervisorError::InvalidInput(
                "recovered worker stop lease does not match the registry row".to_owned(),
            ));
        }
        if record.worker_id != worker_id {
            return Err(SupervisorError::InvalidInput(
                "recovered worker stop identity does not match the registry row".to_owned(),
            ));
        }
        self.stop_with_fencing(worker_session_id, graceful, false)
    }

    fn stop_with_fencing(
        &self,
        worker_session_id: &str,
        graceful: bool,
        enforce_fencing: bool,
    ) -> Result<WorkerProcessRecord, SupervisorError> {
        let stamp = now_rfc3339();
        let mut store = self.lock_store();
        let record =
            store
                .worker_process(worker_session_id)?
                .ok_or_else(|| SupervisorError::NotFound {
                    worker_session_id: worker_session_id.to_owned(),
                })?;
        if record.state != WORKER_STATE_RUNNING {
            return Ok(record);
        }

        if enforce_fencing {
            let mirror = store.occupancy_mirror()?;
            let token = mirror.as_ref().map_or(0, |record| record.fencing_token);
            let guard = FencingGuard::new(mirror);
            if let FencingVerdict::Rejected(rejection) = guard.authorize_command(
                FencedCommandKind::WorkerStop,
                &record.occupancy_lease_id,
                token,
            ) {
                return Err(SupervisorError::Fencing(rejection));
            }
        }

        let tracked = self.lock_children().remove(worker_session_id);
        let observation = if let Some(mut tracked) = tracked {
            let (status, _target) =
                stop_tracked_child(&mut tracked.child, graceful, self.stop_grace()).map_err(
                    |()| SupervisorError::UnverifiableExisting {
                        worker_session_id: record.worker_session_id.clone(),
                        pid: record.pid,
                    },
                )?;
            join_log_readers(tracked.log_readers);
            stop_observation(status)
        } else {
            match stop_registered_process(&record, graceful, self.stop_grace()) {
                StopOutcome::Stopped => (WORKER_STATE_EXITED, None),
                StopOutcome::AlreadyGone => (WORKER_STATE_MISSING, None),
                StopOutcome::Unverifiable => {
                    return Err(SupervisorError::UnverifiableExisting {
                        worker_session_id: record.worker_session_id.clone(),
                        pid: record.pid,
                    });
                }
            }
        };
        store.mark_worker_process_state(worker_session_id, observation.0, observation.1, &stamp)?;
        self.record_log_exit(&record, observation.0, observation.1, &stamp)?;
        Ok(store.worker_process(worker_session_id)?.unwrap_or(record))
    }

    pub fn reap(&self) -> Result<Vec<ReapedWorker>, SupervisorError> {
        let stamp = now_rfc3339();
        let mut finished: Vec<(String, &'static str, Option<i64>)> = Vec::new();
        {
            let mut children = self.lock_children();
            for (worker_session_id, tracked) in children.iter_mut() {
                let Ok(Some(status)) = tracked.child.try_wait() else {
                    continue;
                };
                let (state, exit_code) = terminal_observation(Some(status));
                finished.push((worker_session_id.clone(), state, exit_code));
            }
            for (worker_session_id, _, _) in &finished {
                if let Some(tracked) = children.remove(worker_session_id) {
                    if let Err(error) = retire_child_group(tracked.child.id(), self.stop_grace()) {
                        children.insert(worker_session_id.clone(), tracked);
                        return Err(error);
                    }
                    join_log_readers(tracked.log_readers);
                }
            }
        }
        let mut reaped = Vec::with_capacity(finished.len());
        if finished.is_empty() {
            return Ok(reaped);
        }
        let mut store = self.lock_store();
        for (worker_session_id, state, exit_code) in finished {
            store.mark_worker_process_state(&worker_session_id, state, exit_code, &stamp)?;
            if let Some(record) = store.worker_process(&worker_session_id)? {
                self.record_log_exit(&record, state, exit_code, &stamp)?;
                reaped.push(ReapedWorker {
                    worker_session_id: record.worker_session_id,
                    worker_instance_id: record.worker_instance_id,
                    state,
                    exit_code,
                });
            }
        }
        Ok(reaped)
    }

    pub fn reconcile(&self) -> Result<Vec<WorkerReconcileReport>, SupervisorError> {
        self.reconcile_rows(self.worker_processes()?)
    }

    pub fn reconcile_pending(
        &self,
        lease: &str,
    ) -> Result<Vec<WorkerReconcileReport>, SupervisorError> {
        let rows = self
            .lock_store()
            .worker_report_candidates("reconcile", Some(lease))?;
        self.reconcile_rows(rows)
    }

    fn reconcile_rows(
        &self,
        rows: Vec<WorkerProcessRecord>,
    ) -> Result<Vec<WorkerReconcileReport>, SupervisorError> {
        let stamp = now_rfc3339();
        let mut reports = Vec::with_capacity(rows.len());
        let mut updates: Vec<(String, &'static str, Option<i64>)> = Vec::new();
        {
            let mut children = self.lock_children();
            for mut row in rows {
                let verdict = if let Some(tracked) = children.get_mut(&row.worker_session_id) {
                    match tracked.child.try_wait() {
                        Ok(Some(status)) => {
                            let (state, exit_code) = terminal_observation(Some(status));
                            updates.push((row.worker_session_id.clone(), state, exit_code));
                            WorkerReconcileVerdict::Terminal
                        }
                        Ok(None) => {
                            updates.push((
                                row.worker_session_id.clone(),
                                WORKER_STATE_RUNNING,
                                None,
                            ));
                            WorkerReconcileVerdict::StillRunning
                        }
                        Err(_) => WorkerReconcileVerdict::Unknown,
                    }
                } else {
                    match row.state.as_str() {
                        WORKER_STATE_EXITED | WORKER_STATE_CRASHED => {
                            WorkerReconcileVerdict::Terminal
                        }
                        WORKER_STATE_MISSING => WorkerReconcileVerdict::Missing,
                        _ => match probe_process(row.pid, &row.process_start_identity) {
                            ProcessProbe::Alive => {
                                updates.push((
                                    row.worker_session_id.clone(),
                                    WORKER_STATE_RUNNING,
                                    None,
                                ));
                                WorkerReconcileVerdict::StillRunning
                            }
                            ProcessProbe::Gone => {
                                updates.push((
                                    row.worker_session_id.clone(),
                                    WORKER_STATE_MISSING,
                                    None,
                                ));
                                WorkerReconcileVerdict::Missing
                            }
                            ProcessProbe::Unknown => WorkerReconcileVerdict::Unknown,
                        },
                    }
                };
                if let Some((session, state, exit_code)) = updates.last()
                    && *session == row.worker_session_id
                {
                    (*state).clone_into(&mut row.state);
                    row.exit_code = *exit_code;
                    row.last_observed_at.clone_from(&stamp);
                }
                reports.push(WorkerReconcileReport {
                    worker_session_id: row.worker_session_id.clone(),
                    worker_instance_id: row.worker_instance_id.clone(),
                    pid: row.pid,
                    process_start_identity: row.process_start_identity.clone(),
                    verdict,
                    record: row,
                });
            }
        }
        if !updates.is_empty() {
            let mut store = self.lock_store();
            for (worker_session_id, state, exit_code) in updates {
                store.mark_worker_process_state(&worker_session_id, state, exit_code, &stamp)?;
                if state != WORKER_STATE_RUNNING
                    && let Some(record) = store.worker_process(&worker_session_id)?
                {
                    self.record_log_exit(&record, state, exit_code, &stamp)?;
                }
            }
        }
        Ok(reports)
    }

    pub fn stop_lease_workers(
        &self,
        occupancy_lease_id: &str,
    ) -> Result<Vec<WorkerStopOutcome>, SupervisorError> {
        let rows = self
            .lock_store()
            .worker_processes_for_lease(occupancy_lease_id)?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            if row.state != WORKER_STATE_RUNNING {
                outcomes.push(WorkerStopOutcome {
                    worker_session_id: row.worker_session_id,
                    stopped: false,
                    state: Some(row.state),
                    exit_code: row.exit_code,
                    error: None,
                });
                continue;
            }
            match self.stop_with_fencing(&row.worker_session_id, true, false) {
                Ok(final_row) => outcomes.push(WorkerStopOutcome {
                    worker_session_id: final_row.worker_session_id,
                    stopped: true,
                    state: Some(final_row.state),
                    exit_code: final_row.exit_code,
                    error: None,
                }),
                Err(error) => outcomes.push(WorkerStopOutcome {
                    worker_session_id: row.worker_session_id,
                    stopped: false,
                    state: None,
                    exit_code: None,
                    error: Some(error.to_string()),
                }),
            }
        }
        Ok(outcomes)
    }

    fn stop_grace(&self) -> Duration {
        self.inner.config.stop_grace_period
    }

    fn record_log_exit(
        &self,
        record: &WorkerProcessRecord,
        state: &str,
        exit_code: Option<i64>,
        stamp: &str,
    ) -> Result<(), SupervisorError> {
        self.inner.worker_logs.record_exit(
            &record.worker_session_id,
            &record.worker_instance_id,
            state,
            exit_code,
            stamp,
        )?;
        Ok(())
    }

    fn tracked_child_verdict(&self, worker_session_id: &str) -> Option<ProcessProbe> {
        let mut children = self.lock_children();
        let mut tracked = children.remove(worker_session_id)?;
        match tracked.child.try_wait() {
            Ok(None) => {
                children.insert(worker_session_id.to_owned(), tracked);
                Some(ProcessProbe::Alive)
            }
            Ok(Some(_)) => Some(ProcessProbe::Gone),
            Err(_) => {
                children.insert(worker_session_id.to_owned(), tracked);
                Some(ProcessProbe::Unknown)
            }
        }
    }

    fn lock_store(&self) -> MutexGuard<'_, DeviceStore> {
        self.inner
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_children(&self) -> MutexGuard<'_, BTreeMap<String, TrackedChild>> {
        self.inner
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn resolve_worker_binary(&self) -> Result<PathBuf, SupervisorError> {
        if let Some(configured) = &self.inner.config.worker_binary_path {
            return Ok(configured.clone());
        }
        std::env::current_exe()
            .map(|path| path.with_file_name(WORKER_BINARY_NAME))
            .map_err(|error| SupervisorError::Spawn {
                reason: error.to_string(),
            })
    }
}

pub(crate) fn relay_worker_request(
    endpoint: &str,
    transport: Option<&dyn ExchangeTransport>,
    payload: &winwincode_client_port::worker_relay::DeviceWorkerRelayRequest,
) -> winwincode_client_port::worker_relay::WorkerRelayResponse {
    use winwincode_client_port::worker_relay::WorkerRelayResponse;
    use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
    let protocol_error = || {
        let mut failure = NetworkFailure::new(
            ErrorKind::ProtocolInvalid,
            Acceptance::ResponseReceived,
            Phase::Decode,
        )
        .with_diagnostic(winwincode_network::NetworkDiagnostic::new(
            winwincode_network::DiagnosticCode::JsonSchema,
        ));
        failure.http_status = Some(200);
        failure
    };
    let result = serde_json::to_vec(payload)
        .map_err(|_| {
            NetworkFailure::new(
                ErrorKind::RequestInvalid,
                Acceptance::NotSent,
                Phase::Connect,
            )
        })
        .and_then(|body| {
            transport
                .map_or_else(
                    || crate::http::post_json(endpoint, &body, Duration::from_secs(10), &|| true),
                    |transport| transport.worker_exchange(&body),
                )
                .map_err(|error| error.network_failure())
        })
        .and_then(|body| serde_json::from_slice(&body).map_err(|_| protocol_error()));
    result.unwrap_or_else(|failure| WorkerRelayResponse {
        status: failure.http_status.unwrap_or(502),
        body: String::new(),
        failure: Some(failure),
    })
}

fn helper_executable() -> Result<PathBuf, SupervisorError> {
    let path = std::env::var_os("WWC_WORKER_HELPER_EXECUTABLE")
        .map_or_else(
            || std::env::current_exe().map(|path| path.with_file_name("winwincode-kernel-helper")),
            |path| Ok(PathBuf::from(path)),
        )
        .map_err(|error| SupervisorError::Spawn {
            reason: error.to_string(),
        })?;
    std::path::absolute(path).map_err(|error| SupervisorError::Spawn {
        reason: error.to_string(),
    })
}

impl WorkerCapacitySource for SessionSupervisor {
    fn worker_capacity(&self) -> WorkerCapacitySnapshot {
        let configured_capacity = self.config().max_concurrent_worker_sessions;
        let unobserved_capacity = if configured_capacity == 0 {
            PROTOCOL_WORKER_SESSION_CAPACITY
        } else {
            configured_capacity
        };
        let running = self
            .running_worker_sessions()
            .unwrap_or(unobserved_capacity);
        let reserved = self.reserved_worker_slots().unwrap_or(0);
        WorkerCapacitySnapshot {
            running_worker_sessions: running,
            reserved_worker_sessions: reserved,
        }
    }
}

impl SessionSupervisor {
    fn reserved_worker_slots(&self) -> Result<u32, SupervisorError> {
        let store = self.lock_store();
        let Some(mirror) = store.occupancy_mirror()? else {
            return Ok(0);
        };
        let running_for_lease = store.has_running_worker_for_lease(&mirror.occupancy_lease_id)?;
        Ok(u32::from(!running_for_lease))
    }
}

impl LeaseWorkerController for SessionSupervisor {
    fn stop_lease_workers(&self, occupancy_lease_id: &str) -> usize {
        self.stop_lease_workers(occupancy_lease_id)
            .map_or(0, |outcomes| {
                outcomes.iter().filter(|outcome| outcome.stopped).count()
            })
    }
}

fn validate_config(config: &SupervisorConfig) -> Result<(), SupervisorError> {
    for (value, label) in [
        (&config.client_node_id, "client node id"),
        (&config.client_instance_id, "client instance id"),
    ] {
        if value.is_empty() {
            return Err(SupervisorError::invalid(format!(
                "{label} must not be empty"
            )));
        }
    }
    validate_server_origin(&config.server_origin)?;
    if let Some(route) = &config.model_route
        && (route.capability.is_empty() || route.route.is_empty())
    {
        return Err(SupervisorError::invalid(
            "model route capability and route must not be empty",
        ));
    }
    Ok(())
}

fn validate_server_origin(origin: &str) -> Result<(), SupervisorError> {
    crate::remote_auth::server_origin(origin)
        .map(|_| ())
        .map_err(|error| SupervisorError::invalid(error.to_string()))
}

fn validate_request(request: SpawnRequest<'_>) -> Result<(), SupervisorError> {
    for (value, label) in [
        (request.worker_session_id, "worker session id"),
        (request.worker_id, "worker id"),
        (request.worker_instance_id, "worker instance id"),
        (request.occupancy_lease_id, "occupancy lease id"),
        (request.repository_binding_id, "repository binding id"),
        (request.launch_grant_id, "launch grant id"),
    ] {
        if value.is_empty() {
            return Err(SupervisorError::invalid(format!(
                "{label} must not be empty"
            )));
        }
    }
    for (value, label) in [
        (request.product_session_id, "product session id"),
        (request.work_run_id, "work run id"),
    ] {
        if value.is_some_and(str::is_empty) {
            return Err(SupervisorError::invalid(format!(
                "{label} must not be empty when present"
            )));
        }
    }
    if request.occupancy_fencing_token == 0 {
        return Err(SupervisorError::invalid(
            "occupancy fencing token must be positive",
        ));
    }
    for (path, label) in [
        (request.source_directory, "source directory"),
        (request.data_directory, "data directory"),
        (request.worker_root, "worker root"),
    ] {
        let text = path
            .to_str()
            .ok_or_else(|| SupervisorError::invalid(format!("{label} must be valid UTF-8")))?;
        if text.is_empty() {
            return Err(SupervisorError::invalid(format!(
                "{label} must not be empty"
            )));
        }
    }
    Ok(())
}

fn create_directory(path: &Path, label: &str) -> Result<(), SupervisorError> {
    fs::create_dir_all(path)
        .map_err(|error| SupervisorError::invalid(format!("{label} {}: {error}", path.display())))
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), SupervisorError> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| SupervisorError::invalid(format!("{}: {error}", path.display())))?;
    file.write_all(contents)
        .map_err(|error| SupervisorError::invalid(format!("{}: {error}", path.display())))?;
    let mut permissions = file
        .metadata()
        .map_err(|error| SupervisorError::invalid(format!("{}: {error}", path.display())))?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
        .map_err(|error| SupervisorError::invalid(format!("{}: {error}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), SupervisorError> {
    fs::write(path, contents)
        .map_err(|error| SupervisorError::invalid(format!("{}: {error}", path.display())))
}

fn managed_session_config_json(
    config: &SupervisorConfig,
    request: SpawnRequest<'_>,
    worker_id: &str,
    worker_instance_id: &str,
    runtime_directory: &Path,
    provider_execution_profile: &winwincode_client_port::private_worker_channel::ProviderExecutionProfile,
) -> Result<String, SupervisorError> {
    let local_path = |path: &Path, label: &str| -> Result<String, SupervisorError> {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| SupervisorError::invalid(format!("{label} must be valid UTF-8")))
    };
    let mut object = serde_json::json!({
        "clientNodeId": config.client_node_id,
        "clientInstanceId": config.client_instance_id,
        "occupancyLeaseId": request.occupancy_lease_id,
        "occupancyFencingToken": request.occupancy_fencing_token.to_string(),
        "repositoryBindingId": request.repository_binding_id,
        "workerSessionId": request.worker_session_id,
        "workerId": worker_id,
        "workerInstanceId": worker_instance_id,
        "sourceDirectory": local_path(request.source_directory, "source directory")?,
        "dataDirectory": local_path(request.data_directory, "data directory")?,
        "providerRuntimeDirectory": local_path(runtime_directory, "Provider runtime directory")?,
        "providerExecutionProfile": provider_execution_profile,
        "helperExecutable": local_path(&helper_executable()?, "helper executable")?,
    });
    if let Some(product_session_id) = request.product_session_id {
        object["productSessionId"] = serde_json::Value::String(product_session_id.to_owned());
    }
    if let Some(work_run_id) = request.work_run_id {
        object["workRunId"] = serde_json::Value::String(work_run_id.to_owned());
    }
    if let Some(route) = &config.model_route {
        object["modelRoute"] = serde_json::json!({
            "capability": route.capability,
            "route": route.route,
        });
    }
    serde_json::to_string(&object)
        .map_err(|error| SupervisorError::invalid(format!("config encoding failed: {error}")))
}

fn now_rfc3339() -> String {
    crate::canonical_rfc3339(OffsetDateTime::now_utc())
}

fn observe_process(pid: u32) -> ProcessObservation {
    #[cfg(target_os = "linux")]
    {
        linux_process_observation(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_process_observation(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        ProcessObservation::Unknown
    }
}

#[cfg(target_os = "linux")]
fn linux_process_observation(pid: u32) -> ProcessObservation {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return ProcessObservation::Absent;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return ProcessObservation::Unknown;
    };
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next().unwrap_or_default();
    let start_time = fields.nth(18).unwrap_or_default();
    if state.is_empty() || start_time.is_empty() {
        return ProcessObservation::Unknown;
    }
    ProcessObservation::Boot {
        identity: format!("linux-{start_time}"),
        working: state != "Z",
    }
}

#[cfg(target_os = "macos")]
fn macos_process_observation(pid: u32) -> ProcessObservation {
    let Some((start_time, state)) = macos_process_snapshot(pid) else {
        return ProcessObservation::Absent;
    };
    ProcessObservation::Boot {
        identity: format!("darwin-ps-{start_time}"),
        working: !state.starts_with('Z'),
    }
}

#[cfg(target_os = "macos")]
fn macos_process_snapshot(pid: u32) -> Option<(String, String)> {
    let output = Command::new("/bin/ps")
        .arg("-o")
        .arg("lstart=,stat=")
        .arg("-p")
        .arg(pid.to_string())
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 {
        return None;
    }
    let start_time = fields[..5].join(" ");
    let state = fields[5..].join(" ");
    Some((start_time, state))
}

fn probe_process(pid: u32, expected_start_identity: &str) -> ProcessProbe {
    match observe_process(pid) {
        ProcessObservation::Boot { identity, working } => {
            if identity != expected_start_identity {
                ProcessProbe::Gone
            } else if working {
                ProcessProbe::Alive
            } else {
                ProcessProbe::Gone
            }
        }
        ProcessObservation::Absent => ProcessProbe::Gone,
        ProcessObservation::Unknown => ProcessProbe::Unknown,
    }
}

fn terminal_observation(status: Option<ExitStatus>) -> (&'static str, Option<i64>) {
    match status {
        Some(status) => match status.code() {
            Some(0) => (WORKER_STATE_EXITED, Some(0)),
            Some(code) => (WORKER_STATE_CRASHED, Some(i64::from(code))),
            None => (WORKER_STATE_CRASHED, None),
        },
        None => (WORKER_STATE_MISSING, None),
    }
}

fn join_log_readers(readers: Vec<thread::JoinHandle<()>>) {
    for reader in readers {
        let _ = reader.join();
    }
}

fn retire_child_group(pid: u32, grace: Duration) -> Result<(), SupervisorError> {
    let target = ProcessSignalTarget::Group(pid);
    if process_group_is_empty(target) == Some(true) {
        return Ok(());
    }
    if (!signal_process(target, "KILL") && process_group_is_empty(target) != Some(true))
        || wait_for_group_empty(target, Instant::now() + grace) != StopOutcome::Stopped
    {
        return Err(SupervisorError::Spawn {
            reason: format!("terminated Worker {pid} still owns live subprocesses"),
        });
    }
    Ok(())
}

fn stop_observation(status: Option<ExitStatus>) -> (&'static str, Option<i64>) {
    match status {
        Some(status) => match status.code() {
            Some(0) => (WORKER_STATE_EXITED, Some(0)),
            Some(code) => (WORKER_STATE_CRASHED, Some(i64::from(code))),
            None => (WORKER_STATE_EXITED, None),
        },
        None => (WORKER_STATE_MISSING, None),
    }
}

#[derive(PartialEq, Eq)]
enum StopOutcome {
    Stopped,
    AlreadyGone,
    Unverifiable,
}

fn stop_registered_process(
    record: &WorkerProcessRecord,
    graceful: bool,
    grace: Duration,
) -> StopOutcome {
    match probe_process(record.pid, &record.process_start_identity) {
        ProcessProbe::Alive => {}
        ProcessProbe::Gone => return StopOutcome::AlreadyGone,
        ProcessProbe::Unknown => return StopOutcome::Unverifiable,
    }
    let Some(target) = validated_process_group_target(record.pid) else {
        return StopOutcome::Unverifiable;
    };
    let terminate_signal = if graceful { "TERM" } else { "KILL" };
    if !signal_process(target, terminate_signal) {
        return StopOutcome::Unverifiable;
    }
    let deadline = Instant::now() + grace;
    loop {
        match probe_process(record.pid, &record.process_start_identity) {
            ProcessProbe::Gone => {
                if !signal_process(target, "KILL") {
                    return if process_group_is_empty(target) == Some(true) {
                        StopOutcome::Stopped
                    } else {
                        StopOutcome::Unverifiable
                    };
                }
                return wait_for_group_empty(target, Instant::now() + grace);
            }
            ProcessProbe::Unknown => {
                return StopOutcome::Unverifiable;
            }
            ProcessProbe::Alive => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
    if !signal_process(target, "KILL") && process_group_is_empty(target) != Some(true) {
        return StopOutcome::Unverifiable;
    }
    loop {
        match probe_process(record.pid, &record.process_start_identity) {
            ProcessProbe::Gone => {
                return wait_for_group_empty(target, deadline + grace);
            }
            ProcessProbe::Unknown => {
                return StopOutcome::Unverifiable;
            }
            ProcessProbe::Alive => {}
        }
        if Instant::now() >= deadline + grace {
            return StopOutcome::Unverifiable;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

#[derive(Clone, Copy)]
enum ProcessSignalTarget {
    Group(u32),
}

fn validated_process_group_target(pid: u32) -> Option<ProcessSignalTarget> {
    (observe_process_group_id(pid) == Some(pid)).then_some(ProcessSignalTarget::Group(pid))
}

fn wait_for_group_empty(target: ProcessSignalTarget, deadline: Instant) -> StopOutcome {
    loop {
        match process_group_is_empty(target) {
            Some(true) => return StopOutcome::Stopped,
            Some(false) if Instant::now() < deadline => {
                thread::sleep(STOP_POLL_INTERVAL);
            }
            Some(false) | None => return StopOutcome::Unverifiable,
        }
    }
}

#[cfg(unix)]
fn observe_process_group_id(pid: u32) -> Option<u32> {
    let output = Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p"])
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
        .split(u8::is_ascii_whitespace)
        .find(|field| !field.is_empty())
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse::<u32>().ok())
        .filter(|group| *group > 0)
}

#[cfg(not(unix))]
fn observe_process_group_id(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_group_is_empty(target: ProcessSignalTarget) -> Option<bool> {
    let ProcessSignalTarget::Group(group) = target;
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let group = group.to_string();
    Some(output.stdout.split(|byte| *byte == b'\n').all(|line| {
        let mut fields = line
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty());
        let (Some(_pid), Some(pgid), Some(stat)) = (fields.next(), fields.next(), fields.next())
        else {
            return true;
        };
        pgid != group.as_bytes() || stat.first() == Some(&b'Z')
    }))
}

#[cfg(not(unix))]
fn process_group_is_empty(_target: ProcessSignalTarget) -> Option<bool> {
    None
}

#[cfg(unix)]
fn signal_process(target: ProcessSignalTarget, signal: &str) -> bool {
    let ProcessSignalTarget::Group(group) = target;
    Command::new("/bin/kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{group}"))
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn signal_process(_target: ProcessSignalTarget, _signal: &str) -> bool {
    false
}

fn stop_tracked_child(
    child: &mut Child,
    graceful: bool,
    grace: Duration,
) -> Result<(Option<ExitStatus>, ProcessSignalTarget), ()> {
    let Some(target) = validated_process_group_target(child.id()) else {
        return Err(());
    };
    if let Ok(Some(status)) = child.try_wait() {
        match process_group_is_empty(target) {
            Some(true) => return Ok((Some(status), target)),
            Some(false) => {}
            None => return Err(()),
        }
        if !signal_process(target, "KILL") && process_group_is_empty(target) != Some(true) {
            return Err(());
        }
        if wait_for_group_empty(target, Instant::now() + grace) != StopOutcome::Stopped {
            return Err(());
        }
        return Ok((Some(status), target));
    }
    if graceful {
        if !signal_process(target, "TERM") {
            return Err(());
        }
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !signal_process(target, "KILL")
                        && process_group_is_empty(target) != Some(true)
                    {
                        return Err(());
                    }
                    if wait_for_group_empty(target, Instant::now() + grace) != StopOutcome::Stopped
                    {
                        return Err(());
                    }
                    return Ok((Some(status), target));
                }
                Ok(None) => {}
                Err(_) => return Err(()),
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(STOP_POLL_INTERVAL);
        }
    }
    if !signal_process(target, "KILL") && process_group_is_empty(target) != Some(true) {
        return Err(());
    }
    let status = child.wait().map_err(|_| ())?;
    if wait_for_group_empty(target, Instant::now() + grace) != StopOutcome::Stopped {
        return Err(());
    }
    Ok((Some(status), target))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::store::OccupancyMirrorUpdate;
    use crate::worker_logs::{WorkerLogConfig, WorkerLogRecorder};

    static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(1);

    const STAMP: &str = "2026-09-04T00:00:00.000Z";
    const LEASE: &str = "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA";
    const NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
    const CLIENT_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
    const ORIGIN: &str = "http://127.0.0.1:8443";
    const WORKER_ID: &str = "wrk_TESTWORKER00000000000001";
    const WORKER_INSTANCE: &str = "wki_TESTINSTANCE000000000001";
    const WORKER_INSTANCE_2: &str = "wki_TESTINSTANCE000000000002";

    const LONG_RUNNING_BODY: &str = "trap 'exit 0' TERM\n\
         echo ready > \"$(dirname \"$2\")/worker-ready\"\n\
         while :; do sleep 0.1; done";
    const TREE_RUNNING_BODY: &str = "(trap '' TERM; while :; do sleep 1; done) &\n\
         echo $! > \"$(dirname \"$2\")/child-pid\"\n\
         trap 'exit 0' TERM\n\
         echo ready > \"$(dirname \"$2\")/worker-ready\"\n\
         while :; do sleep 0.1; done";
    const CRASH_BODY: &str = "sleep 0.1; exit 3";
    const LOGGING_CRASH_BODY: &str =
        "printf 'worker diagnostic\n' >&2; printf '%*s\\n' 1500 x >&2; sleep 0.1; exit 3";

    fn wait_for_ready_marker(data_directory: &Path, worker_session_id: &str) {
        let marker = data_directory.join("worker-ready");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "worker {worker_session_id} never signalled readiness"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn process_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn http_relay_fixture(
        responses: Vec<(u16, Option<u64>, String)>,
    ) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/internal/v1/client/worker-exchange",
            listener.local_addr().unwrap()
        );
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, retry_after, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                requests.push(request);
                let retry = retry_after.map_or(String::new(), |seconds| {
                    format!("Retry-After: {seconds}\r\n")
                });
                write!(stream, "HTTP/1.1 {status} fixture\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (endpoint, task)
    }

    #[test]
    fn worker_relay_preserves_permanent_http_and_protocol_failures() {
        use winwincode_client_port::worker_relay::{DeviceWorkerRelayRequest, WorkerRelayRequest};
        use winwincode_network::ErrorKind;
        for (status, body, expected) in [
            (401, "denied", ErrorKind::Authentication),
            (403, "denied", ErrorKind::Authorization),
            (404, "missing", ErrorKind::RequestInvalid),
            (413, "too large", ErrorKind::RequestInvalid),
            (
                200,
                "<html>proxy failure</html>",
                ErrorKind::ProtocolInvalid,
            ),
        ] {
            let (endpoint, server) = http_relay_fixture(vec![(status, None, body.into())]);
            let response = relay_worker_request(
                &endpoint,
                None,
                &DeviceWorkerRelayRequest {
                    client_node_id: NODE.into(),
                    worker_id: WORKER_ID.into(),
                    worker_instance_id: WORKER_INSTANCE.into(),
                    request: WorkerRelayRequest::Exchange {
                        body: "registration".into(),
                    },
                },
            );
            let failure = response.failure.unwrap();
            assert_eq!(failure.kind, expected);
            assert!(!failure.retryable() || failure.phase == winwincode_network::Phase::Decode);
            assert_eq!(failure.http_status, Some(status));
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let inner = winwincode_client_port::worker_relay::WorkerRelayResponse {
            status: 400,
            body: "inner rejection".into(),
            failure: None,
        };
        let (endpoint, server) =
            http_relay_fixture(vec![(200, None, serde_json::to_string(&inner).unwrap())]);
        let response = relay_worker_request(
            &endpoint,
            None,
            &DeviceWorkerRelayRequest {
                client_node_id: NODE.into(),
                worker_id: WORKER_ID.into(),
                worker_instance_id: WORKER_INSTANCE.into(),
                request: WorkerRelayRequest::Exchange {
                    body: "registration".into(),
                },
            },
        );
        assert_eq!(response, inner);
        server.join().unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn worker_relay_retries_the_same_message_after_http_backoff_and_keeps_cancel() {
        use std::os::unix::net::UnixStream;
        use winwincode_client_port::worker_relay::{DeviceWorkerRelayRequest, WorkerRelayResponse};
        use winwincode_worker::WorkerExecutionPort;
        use winwincode_worker::composition::{
            ExecutionPortCore, ExecutionPortMessage, FrameDirection, RemoteTransportAdapter,
            TypedFrame,
        };
        use winwincode_worker::remote_transport::{RemoteWorkerPort, RemoteWorkerPortError};
        struct Core;
        impl ExecutionPortCore for Core {
            type Output = ();
            type Error = std::convert::Infallible;
            fn accept(&mut self, _: &ExecutionPortMessage) -> Result<(), Self::Error> {
                Ok(())
            }
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let message = |kind| {
            serde_json::from_value::<ExecutionPortMessage>(
                fixture["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["kind"] == kind)
                    .unwrap()
                    .clone(),
            )
            .unwrap()
        };
        let register = message("worker.register");
        let ExecutionPortMessage::WorkerRegisterMessage(registration) = &register else {
            unreachable!()
        };
        let cancel = message("job.cancel");
        let ExecutionPortMessage::JobCancelMessage(cancellation) = &cancel else {
            unreachable!()
        };
        let frame = RemoteTransportAdapter::<Core>::encode(
            &TypedFrame::new(FrameDirection::ControlPlaneToWorker, cancel.clone()).unwrap(),
        )
        .unwrap();
        let body = serde_json::json!({
            "schemaVersion": "execution-port.remote-exchange.v2", "frameAccepted": true,
            "serverTime": "2026-10-15T00:00:00.000Z",
            "deliveries": [{"deliveryId": cancellation.message_id, "frame": frame}]
        })
        .to_string();
        let success = serde_json::to_string(&WorkerRelayResponse {
            status: 200,
            body,
            failure: None,
        })
        .unwrap();
        let (endpoint, server) = http_relay_fixture(vec![
            (503, Some(1), String::new()),
            (429, Some(1), String::new()),
            (200, None, success),
        ]);
        let (worker, device) = UnixStream::pair().unwrap();
        let relay = thread::spawn(move || {
            let mut output = device.try_clone().unwrap();
            for line in BufReader::new(device).lines() {
                let envelope = serde_json::from_str::<
                    winwincode_client_port::private_worker_channel::PrivateWorkerRequest,
                >(&line.unwrap())
                .unwrap();
                let winwincode_client_port::private_worker_channel::PrivateWorkerRequestBody::Relay { request } = envelope.request else { panic!("relay request"); };
                let response = relay_worker_request(
                    &endpoint,
                    None,
                    &DeviceWorkerRelayRequest {
                        client_node_id: NODE.into(),
                        worker_id: WORKER_ID.into(),
                        worker_instance_id: WORKER_INSTANCE.into(),
                        request,
                    },
                );
                serde_json::to_writer(&mut output, &winwincode_client_port::private_worker_channel::PrivateWorkerResponse { request_id: envelope.request_id, response: winwincode_client_port::private_worker_channel::PrivateWorkerResponseBody::Relay { response } }).unwrap();
                output.write_all(b"\n").unwrap();
                output.flush().unwrap();
            }
        });
        let (mut port, handle) = RemoteWorkerPort::with_io(
            registration.worker_id.clone(),
            registration.worker_instance_id.clone(),
            BufReader::new(worker.try_clone().unwrap()),
            worker,
        );
        let started = Instant::now();
        for (status, delay) in [(503, 5300), (429, 10300)] {
            let RemoteWorkerPortError::Network(failure) =
                port.send(register.clone()).await.unwrap_err()
            else {
                panic!("transient HTTP must retry")
            };
            assert_eq!(failure.http_status, Some(status));
            assert_eq!(failure.retry_after_ms, Some(1000));
            assert!(matches!(
                port.send(register.clone()).await,
                Err(RemoteWorkerPortError::Network(_))
            ));
            assert!(handle.terminal_error().is_none());
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        port.send(register).await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(matches!(
            handle.next_control().unwrap(),
            Some((_, ExecutionPortMessage::JobCancelMessage(_)))
        ));
        drop(port);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[1], requests[2]);
        relay.join().unwrap();
    }

    #[test]
    fn process_group_signal_stops_only_the_owned_group() {
        let spawn = || {
            Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .expect("isolated process group")
        };
        let mut owned = spawn();
        let mut foreign = spawn();
        let target = validated_process_group_target(owned.id());
        let signalled = target.is_some_and(|target| signal_process(target, "TERM"));
        let deadline = Instant::now() + Duration::from_secs(2);
        let exited = loop {
            if owned.try_wait().expect("owned status").is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(STOP_POLL_INTERVAL);
        };
        let foreign_alive = foreign.try_wait().expect("foreign status").is_none();
        let _ = owned.kill();
        let _ = owned.wait();
        let _ = foreign.kill();
        let _ = foreign.wait();
        assert!(signalled, "the owned group signal must report success");
        assert!(exited, "the owned process must exit");
        assert!(foreign_alive, "another process group must stay alive");
    }

    fn temp_root(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wwc-supervisor-{name}-{}-{suffix}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root creates");
        root
    }

    fn write_executable_script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script writes");
        let mut permissions = fs::metadata(path).expect("script metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("script chmod");
    }

    fn test_config(worker_binary: &Path) -> SupervisorConfig {
        SupervisorConfig {
            client_node_id: NODE.to_owned(),
            client_instance_id: CLIENT_INSTANCE.to_owned(),
            server_origin: ORIGIN.to_owned(),
            model_route: None,
            worker_binary_path: Some(worker_binary.to_path_buf()),
            max_concurrent_worker_sessions: 8,
            stop_grace_period: Duration::from_secs(5),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn worker_stdio_requests_are_relayed_with_device_routes_and_responses() {
        use std::net::TcpListener;
        use winwincode_client_port::worker_relay::{
            DeviceWorkerRelayRequest, WorkerRelayRequest, WorkerRelayResponse,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for expected in [
                WorkerRelayRequest::Exchange { body: "{}".into() },
                WorkerRelayRequest::Accounting {
                    body: Some("[]".into()),
                    offset: 7,
                },
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /internal/v1/client/worker-exchange "));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: DeviceWorkerRelayRequest = serde_json::from_slice(&body).unwrap();
                assert_eq!(request.client_node_id, NODE);
                assert_eq!(request.worker_id, WORKER_ID);
                assert_eq!(request.worker_instance_id, WORKER_INSTANCE);
                assert_eq!(request.request, expected);
                let response = serde_json::to_string(&WorkerRelayResponse {
                    failure: None,
                    status: 202,
                    body: "relayed".into(),
                })
                .unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let bin = temp_root("relay-bin");
        let binary = bin.join(WORKER_BINARY_NAME);
        write_executable_script(
            &binary,
            r#"
            if [ "$1" = "--recover-interrupted-calls" ]; then exit 0; fi
            trap 'exit 0' TERM
            printf '%s\n' '{"requestId":1,"request":{"kind":"relay","request":{"kind":"exchange","body":"{}"}}}'
            IFS= read -r response
            printf '%s' "$response" > "$(dirname "$2")/exchange-response.json"
            printf '%s\n' '{"requestId":2,"request":{"kind":"relay","request":{"kind":"accounting","body":"[]","offset":7}}}'
            IFS= read -r response
            printf '%s' "$response" > "$(dirname "$2")/accounting-response.json"
            printf 'worker diagnostic\n' >&2
            echo ready > "$(dirname "$2")/worker-ready"
            while :; do sleep 0.1; done
        "#,
        );
        let root = temp_root("relay-device");
        let mut store = DeviceStore::open(&root).unwrap();
        store
            .advance_occupancy_mirror(&mirror_update(LEASE, 7))
            .unwrap();
        let mut config = test_config(&binary);
        config.server_origin = format!("http://{address}");
        let supervisor = SessionSupervisor::new(config, store).unwrap();
        let data = root.join("session");
        supervisor
            .spawn(spawn_request(
                "wss_RELAY",
                WORKER_INSTANCE,
                &root,
                &data,
                &root,
            ))
            .unwrap();
        let guard = StopOnDrop::watch(&supervisor, "wss_RELAY");
        wait_for_ready_marker(&data, "wss_RELAY");
        server.join().unwrap();
        for file in ["exchange-response.json", "accounting-response.json"] {
            let envelope: winwincode_client_port::private_worker_channel::PrivateWorkerResponse =
                serde_json::from_slice(&fs::read(data.join(file)).unwrap()).unwrap();
            let winwincode_client_port::private_worker_channel::PrivateWorkerResponseBody::Relay {
                response,
            } = envelope.response
            else {
                panic!("relay response");
            };
            assert_eq!(response.status, 202);
            assert_eq!(response.body, "relayed");
        }
        supervisor.shutdown().unwrap();
        assert_eq!(
            supervisor
                .worker_process("wss_RELAY")
                .unwrap()
                .unwrap()
                .state,
            WORKER_STATE_EXITED
        );
        drop(guard);
        drop(supervisor);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bin).unwrap();
    }

    fn mirror_update(lease: &str, token: u64) -> OccupancyMirrorUpdate {
        OccupancyMirrorUpdate {
            occupancy_lease_id: lease.to_owned(),
            fencing_token: token,
            holder_user_id: Some("usr_HOLDER00000000000000000".to_owned()),
            claim_request_id: Some("ocq_CLAIM0000000000000000000".to_owned()),
            idle_expires_at: None,
            acknowledged_at: STAMP.to_owned(),
        }
    }

    fn supervisor_with_mirror(
        name: &str,
        worker_binary: &Path,
        lease: &str,
        token: u64,
    ) -> (SessionSupervisor, PathBuf) {
        let root = temp_root(name);
        let mut store = DeviceStore::open(&root).expect("store opens");
        store
            .advance_occupancy_mirror(&mirror_update(lease, token))
            .expect("mirror advances");
        let supervisor =
            SessionSupervisor::new(test_config(worker_binary), store).expect("supervisor builds");
        (supervisor, root)
    }

    fn spawn_request<'a>(
        worker_session_id: &'a str,
        worker_instance_id: &'a str,
        source_directory: &'a Path,
        data_directory: &'a Path,
        worker_root: &'a Path,
    ) -> SpawnRequest<'a> {
        SpawnRequest {
            worker_session_id,
            worker_id: WORKER_ID,
            worker_instance_id,
            occupancy_lease_id: LEASE,
            occupancy_fencing_token: 7,
            repository_binding_id: "rbn_TESTREPO",
            launch_grant_id: "wlg_TESTGRANT",
            product_session_id: None,
            work_run_id: None,
            source_directory,
            data_directory,
            worker_root,
        }
    }

    struct StopOnDrop {
        supervisor: SessionSupervisor,
        sessions: Vec<String>,
    }

    impl StopOnDrop {
        fn watch(supervisor: &SessionSupervisor, worker_session_id: &str) -> Self {
            Self {
                supervisor: supervisor.clone(),
                sessions: vec![worker_session_id.to_owned()],
            }
        }
    }

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            for worker_session_id in &self.sessions {
                let _ = self.supervisor.stop(worker_session_id, false);
            }
        }
    }

    fn file_mode(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777
    }

    fn assert_mode_0600(path: &Path) {
        assert_eq!(
            file_mode(path),
            0o600,
            "{} must be mode 0600",
            path.display()
        );
    }

    const WORKER_CONFIG_CONTRACT_FIELDS: [&str; 17] = [
        "clientNodeId",
        "clientInstanceId",
        "occupancyLeaseId",
        "occupancyFencingToken",
        "repositoryBindingId",
        "productSessionId",
        "workRunId",
        "workerSessionId",
        "workerId",
        "workerInstanceId",
        "sourceDirectory",
        "dataDirectory",
        "executionWorkspaceDirectory",
        "providerRuntimeDirectory",
        "providerExecutionProfile",
        "helperExecutable",
        "modelRoute",
    ];

    #[test]
    fn managed_config_preserves_optional_workrun_identity() {
        let path = Path::new("/workspace");
        for work_run_id in [None, Some("wrn_01J00000000000000000000009")] {
            let mut request = spawn_request("wss_ONE", WORKER_INSTANCE, path, path, path);
            request.work_run_id = work_run_id;
            let encoded = managed_session_config_json(
                &test_config(path),
                request,
                WORKER_ID,
                WORKER_INSTANCE,
                Path::new("/workspace/providers/runtime"),
                &winwincode_client_port::private_worker_channel::ProviderExecutionProfile::default(
                ),
            )
            .expect("managed config");
            let value: serde_json::Value = serde_json::from_str(&encoded).expect("JSON");
            assert_eq!(
                value.get("workRunId").and_then(serde_json::Value::as_str),
                work_run_id
            );
            assert_eq!(value.get("workRunId").is_some(), work_run_id.is_some());
            assert!(value.get("stageRunId").is_none());
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn spawn_registers_starts_and_stops_a_real_worker_process() {
        let bin_root = temp_root("spawn-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("spawn", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data").join("wss_ONE");
        let worker_root = root.join("worker-root");

        let outcome = supervisor
            .spawn(spawn_request(
                "wss_ONE",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("spawn starts");
        let SpawnOutcome::Started(handle) = &outcome else {
            panic!("first spawn must start");
        };
        let _guard = StopOnDrop::watch(&supervisor, "wss_ONE");

        assert!(handle.pid > 0);
        assert_eq!(
            handle.worker_id, WORKER_ID,
            "the launch grant's worker identity is registered verbatim"
        );
        assert_eq!(
            handle.worker_instance_id, WORKER_INSTANCE,
            "the launch grant's instance identity is registered verbatim"
        );
        assert!(
            handle.process_start_identity.starts_with("darwin-ps-")
                || handle.process_start_identity.starts_with("linux-"),
            "platform boot identity: {}",
            handle.process_start_identity
        );
        let record = supervisor
            .worker_process("wss_ONE")
            .expect("registry read")
            .expect("row exists");
        assert_eq!(record.state, WORKER_STATE_RUNNING);
        assert_eq!(record.pid, handle.pid);
        assert_eq!(record.process_start_identity, handle.process_start_identity);
        assert_eq!(record.worker_id, handle.worker_id);
        assert_eq!(record.worker_instance_id, handle.worker_instance_id);
        assert_eq!(record.occupancy_lease_id, LEASE);
        assert_eq!(record.launch_grant_id, "wlg_TESTGRANT");
        assert_eq!(record.exit_code, None);

        assert_mode_0600(&data.join("managed-session.json"));
        let config_text =
            fs::read_to_string(data.join("managed-session.json")).expect("config read");
        let config: serde_json::Value =
            serde_json::from_str(&config_text).expect("config is valid JSON");
        let object = config.as_object().expect("config is an object");
        for field in object.keys() {
            assert!(
                WORKER_CONFIG_CONTRACT_FIELDS.contains(&field.as_str()),
                "config field {field} is not in the worker contract"
            );
        }
        for required in [
            "clientNodeId",
            "clientInstanceId",
            "occupancyLeaseId",
            "occupancyFencingToken",
            "repositoryBindingId",
            "workerSessionId",
            "workerId",
            "workerInstanceId",
            "sourceDirectory",
            "dataDirectory",
            "providerRuntimeDirectory",
            "providerExecutionProfile",
            "helperExecutable",
        ] {
            assert!(
                object.contains_key(required),
                "missing config field {required}"
            );
        }
        assert_eq!(object["clientNodeId"], NODE);
        assert_eq!(object["clientInstanceId"], CLIENT_INSTANCE);
        assert_eq!(object["occupancyLeaseId"], LEASE);
        assert_eq!(object["occupancyFencingToken"], "7", "decimal string");
        assert_eq!(object["repositoryBindingId"], "rbn_TESTREPO");
        assert_eq!(object["workerSessionId"], "wss_ONE");
        assert_eq!(object["workerId"], handle.worker_id);
        assert_eq!(object["workerInstanceId"], handle.worker_instance_id);
        assert_eq!(object["sourceDirectory"], source.to_str().expect("utf8"));
        assert_eq!(object["dataDirectory"], data.to_str().expect("utf8"));

        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            1
        );
        assert!(supervisor.reap().expect("reap").is_empty(), "still alive");

        wait_for_ready_marker(&data, "wss_ONE");

        let stopped = supervisor.stop("wss_ONE", true).expect("graceful stop");
        assert_eq!(stopped.state, WORKER_STATE_EXITED);
        assert_eq!(stopped.exit_code, Some(0));
        let closure = crate::daemon::worker_process_closure(&stopped, "cix_REBOOTED")
            .expect("closure decodes actual supervisor config");
        assert_eq!(closure.client_instance_id, CLIENT_INSTANCE);
        assert_eq!(closure.reporting_client_instance_id, "cix_REBOOTED");
        assert_eq!(closure.occupancy_fencing_token, 7);
        assert!(!closure.never_started);

        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            0
        );
        supervisor
            .lock_store()
            .advance_occupancy_mirror(&mirror_update("ocl_BBBBBBBBBBBBBBBBBBBBBBBBBB", 8))
            .expect("a later occupancy advances the mirror");
        let again = supervisor.stop("wss_ONE", true).expect("second stop");
        assert_eq!(again.state, WORKER_STATE_EXITED);
    }

    #[test]
    fn reaping_a_killed_worker_stops_its_surviving_subprocesses() {
        let bin_root = temp_root("reap-tree");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, TREE_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("reap-tree", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");
        let SpawnOutcome::Started(handle) = supervisor
            .spawn(spawn_request(
                "wss_REAP_TREE",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .unwrap()
        else {
            panic!("worker starts")
        };
        let _guard = StopOnDrop::watch(&supervisor, "wss_REAP_TREE");
        wait_for_ready_marker(&data, "wss_REAP_TREE");
        let child_pid: u32 = fs::read_to_string(data.join("child-pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(process_alive(child_pid));
        assert!(
            Command::new("/bin/kill")
                .args(["-KILL", &handle.pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !supervisor.reap().unwrap().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "killed Worker was not reaped");
            thread::sleep(STOP_POLL_INTERVAL);
        }
        assert!(!matches!(
            observe_process(child_pid),
            ProcessObservation::Boot { working: true, .. }
        ));
        assert_eq!(
            process_group_is_empty(ProcessSignalTarget::Group(handle.pid)),
            Some(true)
        );
        assert_ne!(
            supervisor
                .worker_process("wss_REAP_TREE")
                .unwrap()
                .unwrap()
                .state,
            WORKER_STATE_RUNNING
        );
    }

    #[test]
    fn stop_terminates_the_worker_tree_but_leaves_an_unrelated_process_alive() {
        let bin_root = temp_root("process-group");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, TREE_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("process-group", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");
        let mut unrelated = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("unrelated process starts");

        let outcome = supervisor
            .spawn(spawn_request(
                "wss_TREE",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("spawn starts");
        let SpawnOutcome::Started(handle) = outcome else {
            panic!("tree worker must start");
        };
        wait_for_ready_marker(&data, "wss_TREE");
        let child_pid = fs::read_to_string(data.join("child-pid"))
            .expect("child pid")
            .trim()
            .parse::<u32>()
            .expect("numeric child pid");
        assert!(
            process_alive(child_pid),
            "worker child must be alive before stop"
        );
        assert!(
            process_alive(handle.pid),
            "worker root must be alive before stop"
        );
        assert!(
            process_alive(unrelated.id()),
            "unrelated process must be alive before stop"
        );

        let registered = supervisor
            .worker_process("wss_TREE")
            .expect("registry read")
            .expect("worker row");
        assert_eq!(registered.pid, handle.pid);
        assert_eq!(observe_process_group_id(handle.pid), Some(handle.pid));
        drop(supervisor);
        let restarted = SessionSupervisor::new(
            test_config(&binary),
            DeviceStore::open(&root).expect("store reopens"),
        )
        .expect("restarted supervisor builds");
        let stopped = restarted.stop("wss_TREE", true).expect("tree stop");
        assert_eq!(stopped.state, WORKER_STATE_EXITED);
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_alive(child_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_alive(child_pid),
            "stop must terminate the worker child"
        );
        assert!(
            process_alive(unrelated.id()),
            "stop must leave unrelated process alive"
        );
        let _ = unrelated.kill();
        let _ = unrelated.wait();
    }

    #[test]
    fn duplicate_spawn_is_idempotent_for_a_live_session() {
        let bin_root = temp_root("dup-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("dup", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        let SpawnOutcome::Started(first) = supervisor
            .spawn(spawn_request(
                "wss_DUP",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("first spawn")
        else {
            panic!("first spawn must start");
        };
        let _guard = StopOnDrop::watch(&supervisor, "wss_DUP");
        wait_for_ready_marker(&data, "wss_DUP");

        let second = supervisor
            .spawn(spawn_request(
                "wss_DUP",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("duplicate spawn");
        assert_eq!(
            second,
            SpawnOutcome::AlreadyRunning(
                supervisor
                    .worker_process("wss_DUP")
                    .expect("read")
                    .expect("row")
            ),
            "the duplicate spawn returns the existing record"
        );
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            1,
            "one session one worker: no second process"
        );
        let record = supervisor
            .worker_process("wss_DUP")
            .expect("read")
            .expect("row");
        assert_eq!(record.worker_instance_id, first.worker_instance_id);
        assert_eq!(record.pid, first.pid);
    }

    #[test]
    fn consumed_historical_corrupt_config_does_not_block_current_provider_startup() {
        let root = temp_root("consumed-corrupt-config");
        let directory = root.join("history");
        fs::create_dir(&directory).unwrap();
        let path = directory.join(MANAGED_SESSION_CONFIG_FILE);
        fs::write(&path, b"{").unwrap();
        drop(winwincode_provider::DeviceProviderStore::open(&root.join("providers")).unwrap());
        let mut store = DeviceStore::open(&root).unwrap();
        let record = WorkerProcessRecord {
            worker_session_id: "wss_HISTORY".into(),
            worker_id: WORKER_ID.into(),
            worker_instance_id: WORKER_INSTANCE.into(),
            pid: 999_999,
            process_start_identity: "already-exited".into(),
            repository_binding_id: "rbn_HISTORY".into(),
            occupancy_lease_id: LEASE.into(),
            launch_grant_id: "wlg_HISTORY".into(),
            data_directory: directory.to_str().unwrap().to_owned(),
            state: WORKER_STATE_EXITED.into(),
            exit_code: Some(0),
            last_observed_at: STAMP.into(),
        };
        store.put_worker_process(&record).unwrap();
        for family in ["exit", "reconcile"] {
            store.worker_report_queued(family, &record).unwrap();
            assert!(
                store
                    .worker_report_candidates(family, None)
                    .unwrap()
                    .is_empty()
            );
        }
        drop(store);
        let supervisor = SessionSupervisor::new(
            test_config(&root.join("unused-worker-binary")),
            DeviceStore::open(&root).unwrap(),
        )
        .expect("consumed historical configuration is not a startup dependency");
        assert_eq!(supervisor.running_worker_sessions().unwrap(), 0);
        assert_eq!(fs::read(&path).unwrap(), b"{");
        assert_eq!(supervisor.worker_processes().unwrap(), vec![record]);
        drop(supervisor);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn provider_runtime_upgrade_stops_legacy_worker_before_cutover_and_resumes_config_rewrite() {
        let bin_root = temp_root("provider-upgrade-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(
            &binary,
            r#"data="$(dirname "$2")"
if [ "$1" = --recover-interrupted-calls ]; then
    echo recovered > "$data/recovered"
    exit 0
fi
trap 'if [ ! -d "$data/../providers/runtime" ]; then echo stopped > "$data/stopped-before-migration"; fi; exit 0' TERM
echo ready > "$data/worker-ready"
while :; do sleep 0.1; done"#,
        );
        let (previous, root) = supervisor_with_mirror("provider-upgrade", &binary, LEASE, 7);
        let data = root.join("session");
        previous
            .spawn(spawn_request(
                "wss_UPGRADE",
                WORKER_INSTANCE,
                &root.join("repo"),
                &data,
                &root.join("worker-root"),
            ))
            .unwrap();
        let guard = StopOnDrop::watch(&previous, "wss_UPGRADE");
        wait_for_ready_marker(&data, "wss_UPGRADE");
        let path = data.join(MANAGED_SESSION_CONFIG_FILE);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("providerRuntimeDirectory");
        object.remove("providerExecutionProfile");
        object.insert(
            "providerDirectory".into(),
            serde_json::json!(root.join("providers")),
        );
        write_private_file(&path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        let configuration = root.join("providers/providers.sqlite3");
        fs::remove_dir_all(root.join("providers/runtime")).unwrap();
        let database = rusqlite::Connection::open(&configuration).unwrap();
        database
            .execute_batch("DROP TABLE provider_runtime_cutover; PRAGMA user_version=16;")
            .unwrap();
        database.execute("INSERT INTO accounting_closed_attempts(job_id,attempt,lease_id) VALUES('retained-job',1,'retained-lease')", []).unwrap();
        drop(database);
        let restarted =
            SessionSupervisor::new(test_config(&binary), DeviceStore::open(&root).unwrap())
                .unwrap();
        assert_eq!(
            fs::read_to_string(data.join("stopped-before-migration")).unwrap(),
            "stopped\n"
        );
        let record = restarted.worker_process("wss_UPGRADE").unwrap().unwrap();
        assert_ne!(record.state, WORKER_STATE_RUNNING);
        assert_eq!(
            probe_process(record.pid, &record.process_start_identity),
            ProcessProbe::Gone
        );
        let retained: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            retained, legacy,
            "startup does not rewrite historical configs"
        );
        restarted.recover_terminated_calls(&record).unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(rewritten.get("providerDirectory").is_none());
        assert_eq!(
            rewritten["providerRuntimeDirectory"],
            serde_json::json!(fs::canonicalize(root.join("providers/runtime")).unwrap())
        );
        assert_eq!(rewritten["providerExecutionProfile"], serde_json::json!({}));
        assert_eq!(
            fs::read_to_string(data.join("recovered")).unwrap(),
            "recovered\n"
        );
        write_private_file(&path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        let resumed =
            SessionSupervisor::new(test_config(&binary), DeviceStore::open(&root).unwrap())
                .unwrap();
        let retained: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(retained, legacy);
        resumed.recover_terminated_calls(&record).unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(rewritten.get("providerDirectory").is_none());
        let database = rusqlite::Connection::open(&configuration).unwrap();
        assert_eq!(
            database
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            17
        );
        let runtime =
            rusqlite::Connection::open(root.join("providers/runtime/provider-runtime.sqlite3"))
                .unwrap();
        assert_eq!(runtime.query_row("SELECT lease_id FROM accounting_closed_attempts WHERE job_id='retained-job' AND attempt=1", [], |row| row.get::<_, String>(0)).unwrap(), "retained-lease");
        drop(runtime);
        drop(database);
        previous.reap().unwrap();
        drop(guard);
        drop(resumed);
        drop(restarted);
        drop(previous);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bin_root).unwrap();
    }

    #[test]
    fn respawn_after_stop_gets_a_fresh_instance_and_keeps_the_worker_id() {
        let bin_root = temp_root("respawn-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("respawn", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        let SpawnOutcome::Started(first) = supervisor
            .spawn(spawn_request(
                "wss_RESPAWN",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("first spawn")
        else {
            panic!("first spawn must start");
        };
        supervisor.stop("wss_RESPAWN", true).expect("stop");

        let SpawnOutcome::Started(second) = supervisor
            .spawn(spawn_request(
                "wss_RESPAWN",
                WORKER_INSTANCE_2,
                &source,
                &data,
                &worker_root,
            ))
            .expect("replacement spawn")
        else {
            panic!("replacement spawn must start");
        };
        let _guard = StopOnDrop::watch(&supervisor, "wss_RESPAWN");

        assert_eq!(
            second.worker_id, first.worker_id,
            "the grant's stable worker identity rides the replacement boot"
        );
        assert_eq!(second.worker_instance_id, WORKER_INSTANCE_2);
        assert_ne!(
            second.worker_instance_id, first.worker_instance_id,
            "the replacement grant carries a fresh workerInstanceId"
        );
        assert_ne!(second.pid, first.pid);
    }

    #[test]
    fn spawn_without_a_current_stamp_is_refused_before_any_local_action() {
        let bin_root = temp_root("fence-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let root = temp_root("fence");
        let mut store = DeviceStore::open(&root).expect("store opens");
        let supervisor =
            SessionSupervisor::new(test_config(&binary), store).expect("supervisor builds");
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");
        let error = supervisor
            .spawn(spawn_request(
                "wss_FENCED",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect_err("no mirror must refuse");
        assert_eq!(
            error,
            SupervisorError::Fencing(FencingRejection::MirrorNotSet)
        );
        assert!(
            !data.exists(),
            "a refused spawn must not create the data directory"
        );
        assert!(
            supervisor
                .worker_process("wss_FENCED")
                .expect("read")
                .is_none(),
            "a refused spawn must not write the registry"
        );
        supervisor
            .stop("wss_FENCED", false)
            .expect_err("nothing to stop");

        store = DeviceStore::open(&root).expect("store reopens");
        store
            .advance_occupancy_mirror(&mirror_update(LEASE, 7))
            .expect("mirror advances");
        let supervisor =
            SessionSupervisor::new(test_config(&binary), store).expect("supervisor builds");
        let mut stale = spawn_request("wss_FENCED", WORKER_INSTANCE, &source, &data, &worker_root);
        stale.occupancy_fencing_token = 9;
        let error = supervisor
            .spawn(stale)
            .expect_err("stale token must refuse");
        assert_eq!(
            error,
            SupervisorError::Fencing(FencingRejection::StaleFencingToken)
        );
        assert!(
            supervisor
                .worker_process("wss_FENCED")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn crash_detection_records_crashed_with_the_exit_code() {
        let bin_root = temp_root("crash-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, CRASH_BODY);
        let (supervisor, root) = supervisor_with_mirror("crash", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        assert!(
            matches!(
                supervisor
                    .spawn(spawn_request(
                        "wss_CRASH",
                        WORKER_INSTANCE,
                        &source,
                        &data,
                        &worker_root
                    ))
                    .expect("spawn starts"),
                SpawnOutcome::Started(_)
            ),
            "the crashing double starts"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let reaped = loop {
            let reaped = supervisor.reap().expect("reap polls");
            if !reaped.is_empty() {
                break reaped;
            }
            assert!(
                Instant::now() < deadline,
                "the crash was never observed by reap"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].worker_session_id, "wss_CRASH");
        assert_eq!(reaped[0].state, WORKER_STATE_CRASHED);
        assert_eq!(reaped[0].exit_code, Some(3));

        let record = supervisor
            .worker_process("wss_CRASH")
            .expect("read")
            .expect("row");
        assert_eq!(record.state, WORKER_STATE_CRASHED);
        assert_eq!(record.exit_code, Some(3));
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            0
        );
    }

    #[test]
    fn review_recovery_log_failure_does_not_block_required_recovery() {
        let bin_root = temp_root("recovery-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(
            &binary,
            "if [ \"$1\" = --recover-interrupted-calls ]; then \
             echo recovered > \"$(dirname \"$2\")/recovered\"; exit 0; fi; exit 3",
        );
        let (supervisor, root) = supervisor_with_mirror("recovery", &binary, LEASE, 7);
        let data = root.join("data");
        supervisor
            .spawn(spawn_request(
                "wss_RECOVERY",
                WORKER_INSTANCE,
                &root.join("repo"),
                &data,
                &root.join("worker-root"),
            ))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let record = loop {
            if let Some(record) = supervisor.reap().unwrap().into_iter().next() {
                break supervisor
                    .worker_process(&record.worker_session_id)
                    .unwrap()
                    .unwrap();
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        };
        fs::create_dir_all(data.join("codex-runtime/kernel-home")).unwrap();
        fs::create_dir(data.join("call-recovery.log")).unwrap();
        supervisor
            .recover_terminated_calls(&record)
            .expect("optional log must not gate recovery");
        assert_eq!(
            fs::read_to_string(data.join("recovered")).unwrap(),
            "recovered\n"
        );
        write_executable_script(&binary, "exit 1");
        assert!(
            supervisor.recover_terminated_calls(&record).is_err(),
            "required recovery failure must remain a failure"
        );
        drop(supervisor);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(bin_root).unwrap();
    }

    #[test]
    fn real_worker_output_is_bound_and_bounded_per_session() {
        let bin_root = temp_root("logs-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LOGGING_CRASH_BODY);
        let (supervisor, root) = supervisor_with_mirror("logs", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");
        let session = "wss_LOGS1";
        supervisor
            .spawn(spawn_request(
                session,
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("spawn starts");
        let deadline = Instant::now() + Duration::from_secs(5);
        while supervisor.reap().expect("reap").is_empty() {
            assert!(Instant::now() < deadline, "worker did not exit");
            thread::sleep(Duration::from_millis(10));
        }

        let logs = WorkerLogRecorder::open(&root.join("worker-logs"), WorkerLogConfig::default())
            .expect("logs open");
        let summary = logs
            .summary(session)
            .expect("summary reads")
            .expect("session is bound");
        assert_eq!(summary.worker_session_id, session);
        assert_eq!(summary.worker_instance_id.as_deref(), Some(WORKER_INSTANCE));
        assert_eq!(summary.stderr_lines, 1);
        assert_eq!(summary.stdout_lines, 1);
        assert_eq!(summary.lines_omitted_oversize, 1);
        let session_logs = root.join("worker-logs").join(session);
        assert_eq!(
            fs::read_to_string(session_logs.join("stderr.log")).unwrap(),
            "worker diagnostic\n"
        );
        let terminal_log = fs::read_to_string(session_logs.join("stdout.log")).unwrap();
        assert_eq!(terminal_log.lines().count(), 1);
        assert!(
            terminal_log.starts_with("[winwincode] worker process exit state=crashed exitCode=3")
        );
        let reference = logs
            .crash_reference(session)
            .expect("crash reference reads")
            .expect("crash reference exists");
        assert_eq!(reference.worker_session_id, session);
        assert_eq!(reference.exit_code, Some(3));
    }

    #[test]
    fn local_capacity_cap_refuses_the_spawn_when_full() {
        let bin_root = temp_root("cap-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("cap", &binary, LEASE, 7);
        let mut bounded = supervisor.config().clone();
        bounded.max_concurrent_worker_sessions = 1;
        let supervisor =
            SessionSupervisor::new(bounded, DeviceStore::open(&root).expect("store reopens"))
                .expect("supervisor builds");
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        assert!(matches!(
            supervisor
                .spawn(spawn_request(
                    "wss_CAP1",
                    WORKER_INSTANCE,
                    &source,
                    &data,
                    &worker_root,
                ))
                .expect("first spawn"),
            SpawnOutcome::Started(_)
        ));
        let _guard = StopOnDrop::watch(&supervisor, "wss_CAP1");
        let error = supervisor
            .spawn(spawn_request(
                "wss_CAP2",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect_err("local cap must refuse the second spawn");
        assert_eq!(
            error,
            SupervisorError::CapacityExhausted {
                max_concurrent_worker_sessions: 1
            },
            "the test config caps concurrency at 1"
        );
        assert!(
            supervisor
                .worker_process("wss_CAP2")
                .expect("read")
                .is_none(),
            "the refused spawn wrote no registry row"
        );
    }

    #[test]
    fn cancel_and_release_stops_every_worker_bound_to_the_lease() {
        let bin_root = temp_root("release-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("release", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        for worker_session_id in ["wss_REL1", "wss_REL2"] {
            assert!(matches!(
                supervisor
                    .spawn(spawn_request(
                        worker_session_id,
                        WORKER_INSTANCE,
                        &source,
                        &data,
                        &worker_root
                    ))
                    .expect("spawn starts"),
                SpawnOutcome::Started(_)
            ));
        }
        wait_for_ready_marker(&data, "wss_REL1");
        wait_for_ready_marker(&data, "wss_REL2");
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            2
        );

        let stopped = supervisor.stop_lease_workers(LEASE).expect("lease stop");
        assert_eq!(stopped.len(), 2);
        assert!(stopped.iter().all(|outcome| outcome.stopped));
        assert!(
            stopped
                .iter()
                .all(|outcome| outcome.state.as_deref() == Some(WORKER_STATE_EXITED)),
            "graceful stops land as exited: {stopped:?}"
        );
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            0
        );
        let again = supervisor.stop_lease_workers(LEASE).expect("lease stop");
        assert!(again.iter().all(|outcome| !outcome.stopped));
        assert!(
            again
                .iter()
                .all(|outcome| outcome.error.is_none() && outcome.state.is_some())
        );

        assert_eq!(
            LeaseWorkerController::stop_lease_workers(&supervisor, LEASE),
            0,
            "no running worker is left to stop"
        );
    }

    #[test]
    fn capacity_source_reports_running_and_the_claim_reserved_slot() {
        let bin_root = temp_root("capacity-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("capacity", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        let capacity = WorkerCapacitySource::worker_capacity(&supervisor);
        assert_eq!(capacity.running_worker_sessions, 0);
        assert_eq!(capacity.reserved_worker_sessions, 1);

        assert!(matches!(
            supervisor
                .spawn(spawn_request(
                    "wss_CAPACITY",
                    WORKER_INSTANCE,
                    &source,
                    &data,
                    &worker_root
                ))
                .expect("spawn starts"),
            SpawnOutcome::Started(_)
        ));
        let _guard = StopOnDrop::watch(&supervisor, "wss_CAPACITY");
        let capacity = WorkerCapacitySource::worker_capacity(&supervisor);
        assert_eq!(capacity.running_worker_sessions, 1);
        assert_eq!(
            capacity.reserved_worker_sessions, 0,
            "the running worker consumed the reserved slot"
        );

        supervisor.stop("wss_CAPACITY", true).expect("stop");
        let capacity = WorkerCapacitySource::worker_capacity(&supervisor);
        assert_eq!(capacity.running_worker_sessions, 0);
        assert_eq!(capacity.reserved_worker_sessions, 1);
    }

    #[test]
    fn capacity_source_does_not_report_drained_when_worker_registry_is_unavailable() {
        for (local_capacity, expected_running) in [(0, PROTOCOL_WORKER_SESSION_CAPACITY), (8, 8)] {
            let root = temp_root("capacity-query-failure");
            let store = DeviceStore::open(&root).expect("store opens");
            let database_path = store.database_path().to_path_buf();
            let mut config = test_config(&root.join("winwincode-worker"));
            config.max_concurrent_worker_sessions = local_capacity;
            let supervisor = SessionSupervisor::new(config, store).expect("supervisor builds");
            let connection =
                rusqlite::Connection::open(database_path).expect("fault injection opens database");
            connection
                .execute_batch(
                    "ALTER TABLE worker_process_registry RENAME TO unavailable_worker_registry",
                )
                .expect("worker registry becomes unavailable");

            assert!(
                supervisor.running_worker_sessions().is_err(),
                "the actual worker observation must fail"
            );
            let capacity = WorkerCapacitySource::worker_capacity(&supervisor);
            assert_eq!(capacity.running_worker_sessions, expected_running);
            assert!(
                capacity.running_worker_sessions > 0,
                "a failed observation cannot prove that all workers have drained"
            );

            connection
                .execute_batch(
                    "ALTER TABLE unavailable_worker_registry RENAME TO worker_process_registry",
                )
                .expect("worker registry is restored");
            drop(connection);
            drop(supervisor);
            fs::remove_dir_all(root).expect("test root removes");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn restart_reconcile_classifies_live_dead_and_terminal_rows() {
        let bin_root = temp_root("reconcile-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);

        let (previous, root) = supervisor_with_mirror("reconcile", &binary, LEASE, 7);
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");
        let SpawnOutcome::Started(live) = previous
            .spawn(spawn_request(
                "wss_LIVE",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect("live spawn")
        else {
            panic!("live spawn must start");
        };
        let _guard = StopOnDrop::watch(&previous, "wss_LIVE");
        wait_for_ready_marker(&data, "wss_LIVE");

        let crash_binary = bin_root.join("crashing-worker");
        write_executable_script(&crash_binary, CRASH_BODY);
        let mut crash_config = test_config(&crash_binary);
        crash_config.worker_binary_path = Some(crash_binary.clone());
        let crash_supervisor = SessionSupervisor::new(
            crash_config,
            DeviceStore::open(&root).expect("store reopens for the crasher"),
        )
        .expect("crash supervisor builds");
        let crash_request =
            spawn_request("wss_CRASHED", WORKER_INSTANCE, &source, &data, &worker_root);
        assert!(matches!(
            crash_supervisor.spawn(crash_request).expect("crash spawn"),
            SpawnOutcome::Started(_)
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if !crash_supervisor.reap().expect("crash reap").is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "the crash was never reaped");
            thread::sleep(Duration::from_millis(10));
        }

        let restarted = SessionSupervisor::new(
            test_config(&binary),
            DeviceStore::open(&root).expect("store reopens"),
        )
        .expect("supervisor builds");
        let mut reports = restarted.reconcile().expect("reconcile");
        reports.sort_by(|left, right| left.worker_session_id.cmp(&right.worker_session_id));

        let live_report = reports
            .iter()
            .find(|report| report.worker_session_id == "wss_LIVE")
            .expect("live report");
        assert_eq!(live_report.verdict, WorkerReconcileVerdict::StillRunning);
        assert_eq!(live_report.pid, live.pid);
        let crashed_report = reports
            .iter()
            .find(|report| report.worker_session_id == "wss_CRASHED")
            .expect("crashed report");
        assert_eq!(crashed_report.verdict, WorkerReconcileVerdict::Terminal);

        let mut foreign_store = DeviceStore::open(&root).expect("third connection");
        let test_pid = std::process::id();
        foreign_store
            .put_worker_process(&WorkerProcessRecord {
                worker_session_id: "wss_FOREIGN".to_owned(),
                worker_id: "wrk_FOREIGN".to_owned(),
                worker_instance_id: "cix_FOREIGNINSTANCE00000000".to_owned(),
                pid: test_pid,
                process_start_identity: "bogus-identity".to_owned(),
                repository_binding_id: "rbn_TESTREPO".to_owned(),
                occupancy_lease_id: LEASE.to_owned(),
                launch_grant_id: "wlg_FOREIGN".to_owned(),
                data_directory: data.to_str().expect("utf8").to_owned(),
                state: WORKER_STATE_RUNNING.to_owned(),
                exit_code: None,
                last_observed_at: STAMP.to_owned(),
            })
            .expect("foreign row writes");
        let mut dead_child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("dead child spawns");
        let mut dead_pid = dead_child.id();
        dead_child.wait().expect("dead child waits");
        while matches!(observe_process(dead_pid), ProcessObservation::Boot { .. }) {
            dead_pid = dead_pid.wrapping_sub(1).max(1);
        }
        foreign_store
            .put_worker_process(&WorkerProcessRecord {
                worker_session_id: "wss_DEAD".to_owned(),
                worker_id: "wrk_DEAD".to_owned(),
                worker_instance_id: "cix_DEADINSTANCE0000000000".to_owned(),
                pid: dead_pid,
                process_start_identity: "linux-12345".to_owned(),
                repository_binding_id: "rbn_TESTREPO".to_owned(),
                occupancy_lease_id: LEASE.to_owned(),
                launch_grant_id: "wlg_DEAD".to_owned(),
                data_directory: data.to_str().expect("utf8").to_owned(),
                state: WORKER_STATE_RUNNING.to_owned(),
                exit_code: None,
                last_observed_at: STAMP.to_owned(),
            })
            .expect("dead row writes");
        drop(foreign_store);

        let reports = restarted.reconcile().expect("second reconcile");
        let foreign = reports
            .iter()
            .find(|report| report.worker_session_id == "wss_FOREIGN")
            .expect("foreign report");
        assert_eq!(
            foreign.verdict,
            WorkerReconcileVerdict::Missing,
            "a pid bound to a different boot identity is missing (PID reuse)"
        );
        let dead = reports
            .iter()
            .find(|report| report.worker_session_id == "wss_DEAD")
            .expect("dead report");
        assert_eq!(dead.verdict, WorkerReconcileVerdict::Missing);

        let rows = restarted.worker_processes().expect("rows");
        let dead_row = rows
            .iter()
            .find(|row| row.worker_session_id == "wss_DEAD")
            .expect("dead row");
        assert_eq!(dead_row.state, WORKER_STATE_MISSING);

        let stopped = restarted.stop("wss_LIVE", true).expect("registry stop");
        assert_eq!(stopped.state, WORKER_STATE_EXITED);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let reaped = previous.reap().expect("previous reap");
            if !reaped.is_empty() {
                assert_eq!(reaped[0].worker_session_id, "wss_LIVE");
                assert_eq!(reaped[0].state, WORKER_STATE_EXITED);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the killed worker was never reaped"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn supervisor_config_and_requests_validate_fail_closed() {
        let bin_root = temp_root("validate-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let root = temp_root("validate");
        let mut store = DeviceStore::open(&root).expect("store opens");
        store
            .advance_occupancy_mirror(&mirror_update(LEASE, 7))
            .expect("mirror advances");

        let mut bad_config = test_config(&binary);
        bad_config.server_origin = "http://server.example:8443".to_owned();
        let error = SessionSupervisor::new(bad_config, DeviceStore::open(&root).expect("open"))
            .expect_err("remote cleartext origin must refuse");
        assert!(error.to_string().contains("HTTP"), "{error}");

        drop(SessionSupervisor::new(test_config(&binary), store).expect("builds"));
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        let mut request = spawn_request(
            "wss_VALIDATE",
            WORKER_INSTANCE,
            &source,
            &data,
            &worker_root,
        );
        request.occupancy_fencing_token = 0;
        let error = validate_request(request).expect_err("zero fencing token must refuse");
        assert!(error.to_string().contains("fencing token"), "{error}");

        let request = spawn_request(
            "wss_VALIDATE",
            WORKER_INSTANCE,
            &source,
            &data,
            &worker_root,
        );
        assert!(
            validate_request(request).is_ok(),
            "the complete launch request is valid"
        );

        let mut config = test_config(&binary);
        config.worker_binary_path = Some(root.join("absent-worker"));
        let mut reopened = DeviceStore::open(&root).expect("reopen");
        let advance = reopened
            .advance_occupancy_mirror(&mirror_update(LEASE, 7))
            .expect("the same mirror advance is the idempotent replay");
        assert!(matches!(
            advance,
            crate::store::OccupancyMirrorAdvance::Unchanged(_)
        ));
        let supervisor = SessionSupervisor::new(config, reopened).expect("builds");
        let error = supervisor
            .spawn(spawn_request(
                "wss_VALIDATE",
                WORKER_INSTANCE,
                &source,
                &data,
                &worker_root,
            ))
            .expect_err("missing binary must refuse");
        assert!(matches!(error, SupervisorError::Spawn { .. }), "{error}");
        assert!(
            supervisor
                .worker_process("wss_VALIDATE")
                .expect("read")
                .is_none(),
            "a failed spawn writes no registry row"
        );
    }
}
