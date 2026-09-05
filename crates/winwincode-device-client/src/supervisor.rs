// SPDX-License-Identifier: Apache-2.0

//! Local one-session-one-worker process supervision (WORKER-100.2, plan
//! sections 14.1, 14.5, 8.2, 18.3).
//!
//! The [`SessionSupervisor`] is the Device Client's local execution
//! authority for managed Worker processes. "一 Session 一 Worker" is
//! enforced locally: one `WorkerSessionId` maps to at most one live Worker
//! process — a spawn naming a session that already has a verified-live
//! process is the idempotent replay of the same launch and returns the
//! existing record, and a spawn naming a session whose registry row claims
//! `running` but whose process cannot be verified fails closed.
//!
//! Lifecycle:
//!
//! 1. **Fencing first** (plan 12.6): every launch and stop is judged by
//!    [`FencingGuard::authorize_command`] against the durable occupancy
//!    mirror *before* any local action; a rejected stamp spawns nothing and
//!    stops nothing.
//! 2. **Registry before acknowledgement**: the spawn writes the plan-8.2
//!    `worker_process_registry` row binding `pid` **and**
//!    `process_start_identity` — a PID alone is never sufficient because
//!    PIDs are reused — with the launch grant's `workerInstanceId` and state
//!    `running`. The server-minted stable `workerId` is reused across
//!    replacement boots of the same session; every launch grant carries a
//!    fresh instance id.
//! 3. **Locality**: the supervisor writes the two private (mode-0600)
//!    files the managed Worker entry reads — the Worker Session Credential
//!    and the `managed-session.json` config — field-for-field aligned with
//!    `winwincode-worker --managed-session` (the reader is fail-closed and
//!    rejects unknown fields), then spawns the Worker binary with the
//!    config path. The Worker binary path is configurable; the default
//!    locates the `winwincode-worker` binary shipped next to the current
//!    executable.
//! 4. **Supervision**: [`SessionSupervisor::reap`] polls the tracked
//!    children (`waitpid`-style) and moves rows to their terminal states
//!    (`exited` with the exit code, or `crashed` for a nonzero/signal
//!    death); [`SessionSupervisor::stop`] sends the graceful signal first
//!    (SIGTERM via the platform shell — this crate denies `unsafe`, so
//!    there is no direct `libc` call) and escalates to a hard kill after
//!    [`SupervisorConfig::stop_grace_period`].
//! 5. **Recovery**: after a Device Client restart the children map is
//!    empty, so [`SessionSupervisor::reconcile`] probes every registry row
//!    by `pid` + start identity and answers the plan-18.3 vocabulary —
//!    `still_running`, `terminal`, `missing`, `unknown` — the durable data
//!    source for the later `client.worker.reconcile` uplink frames.
//!
//! Capacity (plan 14.5): the live `runningWorkerSessions` figure is the
//! count of `running` registry rows, wired into the daemon's hello/heartbeat
//! through the [`crate::daemon::WorkerCapacitySource`] trait. The
//! `reservedWorkerSessions` slot is simplified: one reserved slot while the
//! device holds an occupancy claim whose lease has no running worker yet;
//! the durable reservation math stays server-owned (the FLOW epic). The
//! spawn path additionally re-checks the local
//! [`SupervisorConfig::max_concurrent_worker_sessions`] bound ("Device
//! Client 负责二次校验").
//!
//! Known crash window: the registry row is written immediately *after* the
//! process is spawned (the pid exists only then), so a crash in between
//! leaves an unregistered orphan; the plan-18.3 launch-intent scan owned by
//! the launch-grant lane closes that window.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::daemon::{LeaseWorkerController, WorkerCapacitySnapshot, WorkerCapacitySource};
use crate::fencing::{FencedCommandKind, FencingGuard, FencingRejection, FencingVerdict};
use crate::store::{DeviceStore, DeviceStoreError, WorkerProcessRecord};

/// Registry lifecycle state of a supervised, believed-live worker process.
pub const WORKER_STATE_RUNNING: &str = "running";
/// Registry lifecycle state of a worker that exited with status zero.
pub const WORKER_STATE_EXITED: &str = "exited";
/// Registry lifecycle state of a worker that died abnormally (nonzero exit
/// or a fatal signal).
pub const WORKER_STATE_CRASHED: &str = "crashed";
/// Registry lifecycle state of a row whose process is gone without a
/// locally observed exit (post-restart probe or unrecoverable wait).
pub const WORKER_STATE_MISSING: &str = "missing";

/// File name of the managed-session config written under the worker data
/// directory (mode 0600, the exact file `--managed-session` reads).
const MANAGED_SESSION_CONFIG_FILE: &str = "managed-session.json";
/// File name of the Worker Session Credential written under the worker
/// data directory (mode 0600; the config names it at `workerCredentialPath`).
const WORKER_CREDENTIAL_FILE: &str = "worker-credential";
/// The managed entry's argument: `winwincode-worker --managed-session <file>`.
const MANAGED_SESSION_ARG: &str = "--managed-session";
/// Default Worker binary name looked up next to the current executable.
const WORKER_BINARY_NAME: &str = "winwincode-worker";
/// Poll slice while waiting for a graceful stop to land.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Static configuration of one [`SessionSupervisor`]: the device-level
/// identity facts written into every managed-session config plus the local
/// supervision policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// `ClientNode` the launching Device Client runs on (config
    /// `clientNodeId`).
    pub client_node_id: String,
    /// One Device Client process boot (config `clientInstanceId`); the
    /// grant binding requires the current instance.
    pub client_instance_id: String,
    /// `https://HOST:PORT` origin of the `ExecutionPort` exchange endpoint
    /// (config `serverOrigin`).
    pub server_origin: String,
    /// Optional model gateway route; the canonical embedded route is used
    /// when absent (config `modelRoute`).
    pub model_route: Option<ModelRoute>,
    /// Configured Worker binary; `None` locates `winwincode-worker` next to
    /// the current executable (same directory, the shipped sibling binary).
    pub worker_binary_path: Option<PathBuf>,
    /// Local second-check bound for concurrent worker sessions (plan 14.5).
    /// `0` disables the local cap (the Control Plane still owns durable
    /// capacity; V1 不允许超卖).
    pub max_concurrent_worker_sessions: u32,
    /// How long a graceful stop waits for the worker to exit before
    /// escalating to a hard kill. Zero escalates immediately.
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
            stop_grace_period: Duration::from_secs(10),
        }
    }
}

/// Optional model gateway route written into the managed-session config,
/// mirroring the Worker entry's `modelRoute` shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    pub capability: String,
    pub route: String,
}

/// One launch request: the identity facts of the Worker Session to start.
///
/// The identity facts arrive server-minted from the launch grant the
/// `client.worker.launch` frame carried: the stable `workerId` (reused
/// across replacement boots of the same session) and the fresh
/// `workerInstanceId` of this exact boot. The registry records them
/// verbatim, so a `client.worker.launch_ack` echoing the registry facts
/// settles against the grant.
///
/// `worker_credential_token` is the Worker Session Credential material —
/// the fourth, strictly separated credential class. It is written exactly
/// once to the mode-0600 credential file the config names and is never
/// stored, logged, or uploaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnRequest<'a> {
    /// The one `WorkerSession` this process authenticates.
    pub worker_session_id: &'a str,
    /// Stable server-minted worker identity (launch grant `workerId`,
    /// config `workerId`).
    pub worker_id: &'a str,
    /// Fresh server-minted identity of this process boot (launch grant
    /// `workerInstanceId`, config `workerInstanceId`).
    pub worker_instance_id: &'a str,
    /// Occupancy lease the session consumes (config `occupancyLeaseId`).
    pub occupancy_lease_id: &'a str,
    /// Fencing token of the lease; judged against the durable mirror before
    /// any local action (plan 12.6 `WorkerLaunch`).
    pub occupancy_fencing_token: u64,
    /// Worker Session Credential material (written to the 0600 credential
    /// file; never persisted anywhere else).
    pub worker_credential_token: &'a str,
    /// Repository binding the session executes against.
    pub repository_binding_id: &'a str,
    /// Server-issued launch grant the launch is settled against (registry
    /// `launch_grant_id`).
    pub launch_grant_id: &'a str,
    /// Product session scope from the grant (optional config
    /// `productSessionId`).
    pub product_session_id: Option<&'a str>,
    /// Stage run scope from the grant (optional config `stageRunId`).
    pub stage_run_id: Option<&'a str>,
    /// Local source root — the only Worker-visible filesystem path the
    /// Device Client writes into the config (`sourceDirectory`).
    pub source_directory: &'a Path,
    /// Local Worker data root; the config and credential files are written
    /// under it (`dataDirectory`).
    pub data_directory: &'a Path,
    /// Local launch state root the worker process starts in (the child's
    /// working directory).
    pub worker_root: &'a Path,
}

/// The process handle of one started worker boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerHandle {
    pub worker_session_id: String,
    /// Stable worker identity (reused across replacement boots).
    pub worker_id: String,
    /// Fresh `cix_` identity of this exact process boot.
    pub worker_instance_id: String,
    pub pid: u32,
    /// Platform process-boot identity binding the pid (plan 8.2).
    pub process_start_identity: String,
}

/// Outcome of one [`SessionSupervisor::spawn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// A new worker process was started and registered.
    Started(WorkerHandle),
    /// One-session-one-worker: the session already has a verified-live
    /// process, so the spawn was an idempotent replay of the same launch
    /// and no second process was created.
    AlreadyRunning(WorkerProcessRecord),
}

/// One worker exit observed by [`SessionSupervisor::reap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReapedWorker {
    pub worker_session_id: String,
    pub worker_instance_id: String,
    /// Terminal registry state: [`WORKER_STATE_EXITED`] or
    /// [`WORKER_STATE_CRASHED`].
    pub state: &'static str,
    /// Exit status when the platform supplied one (`None` for signal
    /// deaths).
    pub exit_code: Option<i64>,
}

/// One worker stop requested through [`SessionSupervisor::stop_lease_workers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerStopOutcome {
    pub worker_session_id: String,
    /// Whether a stop was requested for a believed-live worker (`false`
    /// means the row was already terminal or the stop was refused; see
    /// `error`).
    pub stopped: bool,
    /// Terminal registry state after the stop, when reached.
    pub state: Option<String>,
    pub exit_code: Option<i64>,
    /// Refusal reason when the stop could not be requested or finished.
    pub error: Option<String>,
}

/// The plan-18.3 reconcile vocabulary for one registry row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerReconcileVerdict {
    /// The process is alive with the bound boot identity.
    StillRunning,
    /// The row already carries a locally observed terminal state.
    Terminal,
    /// The process is gone (unobserved exit, or the pid now names a
    /// different boot — PID reuse).
    Missing,
    /// The platform could not determine the process state; the row is left
    /// untouched and the server should treat the session as unconfirmed.
    Unknown,
}

/// One row's reconcile report — the durable data source for the later
/// `client.worker.reconcile` uplink frames (frame construction belongs to
/// the launch-grant lane).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerReconcileReport {
    pub worker_session_id: String,
    pub worker_instance_id: String,
    pub pid: u32,
    pub process_start_identity: String,
    pub verdict: WorkerReconcileVerdict,
}

/// Failure of one supervision operation. Every variant is secret-free: the
/// Worker Session Credential never appears in any message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorError {
    /// The durable store failed.
    Store(DeviceStoreError),
    /// The fencing guard refused the command before any local action.
    Fencing(FencingRejection),
    /// No registry row exists for the worker session.
    NotFound { worker_session_id: String },
    /// A registry row claims `running` but its process boot cannot be
    /// verified; spawning over it could violate one-session-one-worker, so
    /// the operation fails closed.
    UnverifiableExisting { worker_session_id: String, pid: u32 },
    /// The local concurrent-worker bound rejected the spawn (plan 14.5).
    CapacityExhausted { max_concurrent_worker_sessions: u32 },
    /// A request or configuration field is invalid.
    InvalidInput(String),
    /// The Worker binary could not be located.
    WorkerBinary { path: PathBuf, reason: String },
    /// The worker process could not be spawned.
    Spawn { reason: String },
    /// The platform could not supply the process-boot identity the registry
    /// binding requires (plan 8.2: a bare pid is never sufficient).
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
            Self::WorkerBinary { path, reason } => {
                write!(formatter, "worker binary {}: {reason}", path.display())
            }
            Self::Spawn { reason } => write!(formatter, "worker process spawn failed: {reason}"),
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

/// Platform observation of one pid's process boot.
///
/// The observation distinguishes the boot identity (readable until the
/// process is reaped, zombies included — the capture right after spawn
/// must tolerate a worker that already exited) from liveness (a zombie
/// holds its pid but stopped working, so it probes as gone).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessObservation {
    /// The pid names a process boot with this identity; `working` is false
    /// for a zombie (still holding the pid, already dead).
    Boot { identity: String, working: bool },
    /// The pid names no process at all.
    Absent,
    /// The platform cannot tell. (Constructed on Linux and the stubbed
    /// platforms; the macOS `ps` snapshot is always conclusive.)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unknown,
}

/// Liveness verdict for a registry row's `pid` + expected boot identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessProbe {
    /// The pid is live *and* its boot identity matches the row.
    Alive,
    /// The pid is gone, or names a different boot (PID reuse).
    Gone,
    /// The platform cannot tell.
    Unknown,
}

/// The tracked child of one supervised session. The registry row owns the
/// identity facts; the bookkeeping only needs the process itself.
struct TrackedChild {
    child: Child,
}

/// Shared state of one supervisor handle.
struct SupervisorInner {
    config: SupervisorConfig,
    store: Mutex<DeviceStore>,
    children: Mutex<BTreeMap<String, TrackedChild>>,
}

/// The Device Client's local worker supervisor.
///
/// Cheap to clone; every method is `&self` — the store connection and the
/// child bookkeeping live behind internal mutexes, so one handle can be
/// shared as the daemon's capacity source and release controller.
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
    /// Builds one supervisor over an open device store and a configuration.
    ///
    /// The store is owned by the supervisor; pass a dedicated
    /// [`DeviceStore::open`] connection or hand the daemon's store over via
    /// [`crate::daemon::DeviceDaemon::into_store`]. The store may name the
    /// same database file other device-client components use: `SQLite` WAL
    /// mode multiplexes the connections.
    ///
    /// # Errors
    ///
    /// Rejects an empty device identity or a malformed
    /// [`SupervisorConfig::server_origin`] (the same `https://HOST:PORT`
    /// rule the Worker entry enforces, so a config this supervisor writes
    /// can never be refused by its reader).
    pub fn new(config: SupervisorConfig, store: DeviceStore) -> Result<Self, SupervisorError> {
        validate_config(&config)?;
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                config,
                store: Mutex::new(store),
                children: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// The static supervision configuration.
    #[must_use]
    pub fn config(&self) -> &SupervisorConfig {
        &self.inner.config
    }

    /// Loads one worker-process registry row.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the read fails.
    pub fn worker_process(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerProcessRecord>, SupervisorError> {
        Ok(self.lock_store().worker_process(worker_session_id)?)
    }

    /// Writes (or overwrites) the mode-0600 Worker Session Credential file
    /// of one supervised worker session — the deferred-material delivery the
    /// daemon invokes when the launch response's one-time credential lands
    /// after the spawn. The worker transport re-reads the private file on
    /// every exchange, so the next retry picks the material up.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::NotFound`] for an unknown session and the
    /// write failure otherwise.
    pub fn write_worker_credential(
        &self,
        worker_session_id: &str,
        material: &str,
    ) -> Result<(), SupervisorError> {
        let record = self
            .lock_store()
            .worker_process(worker_session_id)?
            .ok_or_else(|| SupervisorError::NotFound {
                worker_session_id: worker_session_id.to_owned(),
            })?;
        let path = PathBuf::from(record.data_directory).join(WORKER_CREDENTIAL_FILE);
        write_private_file(&path, material.as_bytes())
    }

    /// Loads every worker-process registry row.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the read fails.
    pub fn worker_processes(&self) -> Result<Vec<WorkerProcessRecord>, SupervisorError> {
        Ok(self.lock_store().worker_processes()?)
    }

    /// Counts the registry rows in one lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the read fails.
    pub fn count_worker_processes_in_state(&self, state: &str) -> Result<u64, SupervisorError> {
        Ok(self.lock_store().count_worker_processes_in_state(state)?)
    }

    /// The live `runningWorkerSessions` capacity fact: reaps observed exits
    /// first, then counts the `running` registry rows.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the read fails.
    pub fn running_worker_sessions(&self) -> Result<u32, SupervisorError> {
        self.reap()?;
        let running = self.count_worker_processes_in_state(WORKER_STATE_RUNNING)?;
        Ok(u32::try_from(running).unwrap_or(u32::MAX))
    }

    /// Starts the one Worker process of one Worker Session (plan 14.3
    /// steps 6-9).
    ///
    /// Order of operations: fencing authorization, the one-session-one-worker
    /// idempotence check, the local capacity second-check, then credential +
    /// config files (mode 0600), the process spawn, and the registry row
    /// binding `pid` + `process_start_identity` with the launch grant's
    /// `workerInstanceId` (the grant owns both worker identities).
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::Fencing`] for a rejected launch stamp (no
    /// local action happens), [`SupervisorError::CapacityExhausted`] when
    /// the local bound is full, [`SupervisorError::UnverifiableExisting`]
    /// when a `running` row cannot be verified (fail closed), and the
    /// remaining variants for spawn-time failures.
    pub fn spawn(&self, request: SpawnRequest<'_>) -> Result<SpawnOutcome, SupervisorError> {
        let stamp = now_rfc3339();
        let mut store = self.lock_store();

        // Plan 12.6: the launch must carry the current mirror stamp; a
        // rejected stamp never touches the filesystem or the registry.
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

        // One session one worker: a verified-live process makes the spawn an
        // idempotent replay; an unverifiable `running` row fails closed; a
        // gone one is demoted to `missing` before the replacement boot.
        if let Some(existing) = store.worker_process(request.worker_session_id)?
            && existing.state == WORKER_STATE_RUNNING
            && let Some(outcome) = self.already_running_outcome(&mut store, &existing, &stamp)?
        {
            return Ok(outcome);
        }

        // Plan 14.5: the Device Client's local second check on capacity.
        let max = self.inner.config.max_concurrent_worker_sessions;
        if max > 0 {
            let running = store.count_worker_processes_in_state(WORKER_STATE_RUNNING)?;
            if running >= u64::from(max) {
                return Err(SupervisorError::CapacityExhausted {
                    max_concurrent_worker_sessions: max,
                });
            }
        }

        // The launch grant owns the identity facts: the stable workerId and
        // the fresh workerInstanceId of this boot arrive server-minted, and
        // the registry records them verbatim so the launch acknowledgement
        // echoes exactly what the grant carries.
        let worker_id = request.worker_id;
        let worker_instance_id = request.worker_instance_id;

        // Locality: the two private files the managed entry reads, written
        // under the worker data directory with exactly mode 0600.
        create_directory(request.data_directory, "worker data directory")?;
        create_directory(request.worker_root, "worker root")?;
        let credential_path = request.data_directory.join(WORKER_CREDENTIAL_FILE);
        write_private_file(&credential_path, request.worker_credential_token.as_bytes())?;
        let config_path = request.data_directory.join(MANAGED_SESSION_CONFIG_FILE);
        let config_json = managed_session_config_json(
            &self.inner.config,
            request,
            worker_id,
            worker_instance_id,
            &credential_path,
        )?;
        write_private_file(&config_path, config_json.as_bytes())?;

        let mut child = self.launch_process(&config_path, request)?;
        let record = self.register_child(
            &mut store,
            request,
            &mut child,
            worker_id.to_owned(),
            worker_instance_id.to_owned(),
            &stamp,
        )?;
        self.lock_children()
            .insert(request.worker_session_id.to_owned(), TrackedChild { child });

        Ok(SpawnOutcome::Started(WorkerHandle {
            worker_session_id: record.worker_session_id,
            worker_id: record.worker_id,
            worker_instance_id: record.worker_instance_id,
            pid: record.pid,
            process_start_identity: record.process_start_identity,
        }))
    }

    /// The one-session-one-worker verdict for a `running` row: `Some(_)`
    /// ends the spawn (the idempotent replay of a verified-live process);
    /// an unverifiable row fails closed; a gone row is demoted to
    /// `missing` and the spawn proceeds to the replacement boot.
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

    /// Spawns the Worker process with the managed-session entry arguments.
    fn launch_process(
        &self,
        config_path: &Path,
        request: SpawnRequest<'_>,
    ) -> Result<Child, SupervisorError> {
        let binary = self.resolve_worker_binary()?;
        let mut command = Command::new(&binary);
        command
            .arg(MANAGED_SESSION_ARG)
            .arg(config_path)
            .current_dir(request.worker_root);
        command.spawn().map_err(|error| SupervisorError::Spawn {
            reason: format!("{}: {error}", binary.display()),
        })
    }

    /// Captures the boot identity of the fresh child and writes the plan-8.2
    /// registry row. A child whose identity is unavailable is killed and the
    /// launch refused (a bare pid can never be probed safely).
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
            ProcessObservation::Boot { identity, .. } => {
                // Zombie-tolerant: an instantly-exiting worker still names
                // the boot it ran under.
                identity
            }
            ProcessObservation::Absent | ProcessObservation::Unknown => {
                let _ = child.kill();
                let _ = child.wait();
                self.lock_children().remove(request.worker_session_id);
                return Err(SupervisorError::ProcessIdentityUnavailable { pid });
            }
        };

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

    /// Stops the Worker process of one Worker Session (plan 12.6
    /// `WorkerStop`).
    ///
    /// `graceful` sends the terminate signal first and waits up to
    /// [`SupervisorConfig::stop_grace_period`] before the hard kill;
    /// `!graceful` kills immediately. The registry row moves to its terminal
    /// state (`exited`/`crashed` with the exit code when observed, or
    /// `missing` when the process was already gone). Stopping an already
    /// terminal row is the idempotent no-op returning the stored row.
    ///
    /// Works across supervisor restarts: a row without a tracked child is
    /// stopped by its registered `pid` after a boot-identity probe
    /// re-confirms it.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::Fencing`] for a rejected stop stamp (no
    /// signal is sent), [`SupervisorError::NotFound`] for an unknown
    /// session, and [`SupervisorError::UnverifiableExisting`] when the
    /// process state cannot be determined (fail closed).
    pub fn stop(
        &self,
        worker_session_id: &str,
        graceful: bool,
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

        // Plan 12.6: the stop must carry the current mirror stamp of the
        // worker's own lease. The mirror is the only current-token source;
        // a force-fenced or released lease can no longer stop its workers.
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
        drop(guard);

        let tracked = self.lock_children().remove(worker_session_id);
        let observation = if let Some(mut tracked) = tracked {
            let status = stop_tracked_child(&mut tracked.child, graceful, self.stop_grace());
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
        Ok(store.worker_process(worker_session_id)?.unwrap_or(record))
    }

    /// Polls every tracked child (`waitpid`-style) and records observed
    /// exits: status zero moves the row to `exited` with its exit code, an
    /// abnormal death to `crashed`. Answers the workers whose exits this
    /// call observed (crash detection); live workers are not listed.
    ///
    /// # Errors
    ///
    /// Returns the store failure when a state write fails.
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
                children.remove(worker_session_id);
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

    /// The plan-18.3 restart scan: probes every registry row by `pid` +
    /// process-start identity and answers the reconcile vocabulary
    /// (`still_running` / `terminal` / `missing` / `unknown`), updating the
    /// rows it could classify. The result is the durable data source for
    /// the `client.worker.reconcile` uplink the launch-grant lane sends.
    ///
    /// # Errors
    ///
    /// Returns the store failure when a read or write fails.
    pub fn reconcile(&self) -> Result<Vec<WorkerReconcileReport>, SupervisorError> {
        let stamp = now_rfc3339();
        let rows = self.worker_processes()?;
        let mut reports = Vec::with_capacity(rows.len());
        let mut updates: Vec<(String, &'static str, Option<i64>)> = Vec::new();
        {
            let mut children = self.lock_children();
            for row in rows {
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
                reports.push(WorkerReconcileReport {
                    worker_session_id: row.worker_session_id.clone(),
                    worker_instance_id: row.worker_instance_id.clone(),
                    pid: row.pid,
                    process_start_identity: row.process_start_identity.clone(),
                    verdict,
                });
            }
        }
        if !updates.is_empty() {
            let mut store = self.lock_store();
            for (worker_session_id, state, exit_code) in updates {
                store.mark_worker_process_state(&worker_session_id, state, exit_code, &stamp)?;
            }
        }
        Ok(reports)
    }

    /// Stops every supervised worker bound to one occupancy lease (the
    /// `cancel_tasks_and_release` execution path) and answers one outcome
    /// per registry row of the lease.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the lease scan fails; per-worker
    /// stop refusals are reported in the outcomes instead.
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
            match self.stop(&row.worker_session_id, true) {
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

    /// Whether the session's tracked child is still live, removing an
    /// already-exited child from the bookkeeping (`try_wait` collects the
    /// exit status, so no zombie remains).
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

    /// The Worker binary to spawn: the configured path when set (verified
    /// present), else the `winwincode-worker` sibling of the current
    /// executable.
    fn resolve_worker_binary(&self) -> Result<PathBuf, SupervisorError> {
        if let Some(configured) = &self.inner.config.worker_binary_path {
            if configured.is_file() {
                return Ok(configured.clone());
            }
            return Err(SupervisorError::WorkerBinary {
                path: configured.clone(),
                reason: "the configured worker binary is missing".to_owned(),
            });
        }
        if let Ok(current) = std::env::current_exe()
            && let Some(directory) = current.parent()
        {
            let sibling = directory.join(WORKER_BINARY_NAME);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
        Err(SupervisorError::WorkerBinary {
            path: PathBuf::from(WORKER_BINARY_NAME),
            reason: "no configured worker binary and no winwincode-worker sibling \
                     next to the current executable"
                .to_owned(),
        })
    }
}

/// Live capacity facts for the daemon's hello/heartbeat reports (plan 14.5).
impl WorkerCapacitySource for SessionSupervisor {
    fn worker_capacity(&self) -> WorkerCapacitySnapshot {
        // Best effort: a supervision hiccup must not block the exchange
        // loop, so failures report the conservative zero facts.
        let _ = self.reap();
        let running = self
            .count_worker_processes_in_state(WORKER_STATE_RUNNING)
            .unwrap_or(0);
        let reserved = self.reserved_worker_slots().unwrap_or(0);
        WorkerCapacitySnapshot {
            running_worker_sessions: u32::try_from(running).unwrap_or(u32::MAX),
            reserved_worker_sessions: reserved,
        }
    }
}

impl SessionSupervisor {
    /// The simplified reserved-slot hint: one slot while the device holds
    /// an occupancy claim whose lease has no running worker yet. The
    /// durable reservation math stays with the Control Plane (FLOW epic).
    fn reserved_worker_slots(&self) -> Result<u32, SupervisorError> {
        let store = self.lock_store();
        let Some(mirror) = store.occupancy_mirror()? else {
            return Ok(0);
        };
        let running_for_lease = store
            .worker_processes_for_lease(&mirror.occupancy_lease_id)?
            .iter()
            .filter(|row| row.state == WORKER_STATE_RUNNING)
            .count();
        Ok(u32::from(running_for_lease == 0))
    }
}

/// The daemon's `cancel_tasks_and_release` hook: stop every supervised
/// worker bound to the lease.
impl LeaseWorkerController for SessionSupervisor {
    fn stop_lease_workers(&self, occupancy_lease_id: &str) -> usize {
        self.stop_lease_workers(occupancy_lease_id)
            .map_or(0, |outcomes| {
                outcomes.iter().filter(|outcome| outcome.stopped).count()
            })
    }
}

/// Validates the static supervisor configuration.
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

/// The Worker entry's `https://HOST:PORT` origin rule: a config this
/// supervisor writes must never be refused by its reader.
fn validate_server_origin(origin: &str) -> Result<(), SupervisorError> {
    let authority = origin
        .strip_prefix("https://")
        .filter(|value| !value.contains('/') && !value.contains('@'))
        .ok_or_else(|| {
            SupervisorError::invalid("server origin must be an https://HOST:PORT origin")
        })?;
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
        SupervisorError::invalid("server origin must be an https://HOST:PORT origin")
    })?;
    if host.is_empty() || host.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(SupervisorError::invalid(
            "server origin host must be non-empty and free of control characters",
        ));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| SupervisorError::invalid("server origin port must be a port number"))?;
    if port == 0 {
        return Err(SupervisorError::invalid(
            "server origin port must be nonzero",
        ));
    }
    Ok(())
}

/// Validates one spawn request's identity facts and local paths.
///
/// The worker credential material may be empty: the launch acknowledgement
/// must not wait for the one-time material (the launch response delivers it
/// to the local user-session bridge after the grant is consumed), so the
/// supervisor writes the private credential file as a placeholder and the
/// daemon fills it via [`SessionSupervisor::write_worker_credential`] when
/// the material lands. The worker transport re-reads the file on every
/// exchange.
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
        (request.stage_run_id, "stage run id"),
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

/// Writes one file with exactly mode 0600 (the managed entry's private-file
/// rule: the config is refused on anything else, the credential on any
/// group/other bit).
#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), SupervisorError> {
    use std::io::Write as _;
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
    // The mode option only applies at creation; pin the exact mode even when
    // overwriting a pre-existing file.
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

/// Builds the managed-session config: field-for-field the camelCase shape
/// `winwincode-worker --managed-session` reads (unknown fields are refused
/// by the reader, so this writes exactly the known fields, carrying the
/// optional `productSessionId`/`stageRunId` scopes when the launch grant
/// provides them).
fn managed_session_config_json(
    config: &SupervisorConfig,
    request: SpawnRequest<'_>,
    worker_id: &str,
    worker_instance_id: &str,
    credential_path: &Path,
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
        "serverOrigin": config.server_origin,
        "workerCredentialPath": local_path(credential_path, "worker credential path")?,
    });
    if let Some(product_session_id) = request.product_session_id {
        object["productSessionId"] = serde_json::Value::String(product_session_id.to_owned());
    }
    if let Some(stage_run_id) = request.stage_run_id {
        object["stageRunId"] = serde_json::Value::String(stage_run_id.to_owned());
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

/// RFC 3339 UTC stamp of the current wall clock (server-side style).
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// The platform process-boot observation behind a pid (plan 8.2: 不能只存
/// PID). This crate denies `unsafe`, so the observations go through the
/// kernel's text interfaces (`/proc` on Linux, `ps` on macOS) instead of
/// raw syscalls.
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

/// Linux: `/proc/<pid>/stat` carries the state (field 3) and the start time
/// in clock ticks since boot (field 22); the comm field is bracketed, so
/// parsing starts after its closing parenthesis.
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
    // After the state (field 3), the start time (field 22) is the 19th
    // further field.
    let start_time = fields.nth(18).unwrap_or_default();
    if state.is_empty() || start_time.is_empty() {
        return ProcessObservation::Unknown;
    }
    ProcessObservation::Boot {
        identity: format!("linux-{start_time}"),
        working: state != "Z",
    }
}

/// macOS: `ps -o lstart=,stat=` answers the process start timestamp plus
/// the state flag column; `ps` fails for an absent pid and reports `Z*`
/// for zombies. The locale is pinned so the identity string is stable.
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

/// `(lstart, stat)` of one pid, or `None` when `ps` says it is absent.
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

/// Probes a registry row's pid against its recorded boot identity. A
/// zombie probes as gone: it holds the pid but stopped working.
fn probe_process(pid: u32, expected_start_identity: &str) -> ProcessProbe {
    match observe_process(pid) {
        ProcessObservation::Boot { identity, working } => {
            if identity != expected_start_identity {
                // The pid now names a different boot: PID reuse (plan 8.2).
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

/// Terminal registry state for one observed (or unobservable) exit during
/// a passive reap: status zero is `exited` with its exit code, an abnormal
/// self-death is `crashed`, a death whose status could not be collected
/// stays `missing` (unobserved exit).
fn terminal_observation(status: Option<ExitStatus>) -> (&'static str, Option<i64>) {
    match status {
        Some(status) => match status.code() {
            Some(0) => (WORKER_STATE_EXITED, Some(0)),
            Some(code) => (WORKER_STATE_CRASHED, Some(i64::from(code))),
            // A signal death under passive reap is a crash this lane did
            // not cause (a supervisor never signals outside `stop`).
            None => (WORKER_STATE_CRASHED, None),
        },
        None => (WORKER_STATE_MISSING, None),
    }
}

/// Terminal registry state for one exit observed while *stopping*: this
/// supervisor is the deliberate terminator, so a signal death — including
/// one landing on a worker that had not finished installing its handlers —
/// records `exited` without a code, and only a nonzero self-exit is a
/// `crashed`.
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

/// Outcome of stopping a process by its registered pid.
enum StopOutcome {
    /// The stop landed and the process is gone.
    Stopped,
    /// The process was already gone before any signal was sent.
    AlreadyGone,
    /// The platform could not confirm the process state (fail closed).
    Unverifiable,
}

/// Stops a worker this supervisor process does not own (post-restart row):
/// the pid is re-confirmed against its boot identity, then the terminate
/// (or kill) signal goes out via the platform shell — this crate denies
/// `unsafe`, so there is no direct `kill(2)` call. A signal to a reused pid
/// is prevented by the identity probe immediately before it; the residual
/// milliseconds-wide window is the platform's own race, not a registry one.
fn stop_registered_process(
    record: &WorkerProcessRecord,
    graceful: bool,
    grace: Duration,
) -> StopOutcome {
    if probe_process(record.pid, &record.process_start_identity) != ProcessProbe::Alive {
        return StopOutcome::AlreadyGone;
    }
    let terminate_signal = if graceful { "TERM" } else { "KILL" };
    if !signal_process(record.pid, terminate_signal) {
        return StopOutcome::AlreadyGone;
    }
    let deadline = Instant::now() + grace;
    loop {
        match probe_process(record.pid, &record.process_start_identity) {
            ProcessProbe::Gone => return StopOutcome::Stopped,
            ProcessProbe::Unknown => return StopOutcome::Unverifiable,
            ProcessProbe::Alive => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
    if !signal_process(record.pid, "KILL") {
        return StopOutcome::Stopped;
    }
    loop {
        match probe_process(record.pid, &record.process_start_identity) {
            ProcessProbe::Gone => return StopOutcome::Stopped,
            ProcessProbe::Unknown => return StopOutcome::Unverifiable,
            ProcessProbe::Alive => {}
        }
        if Instant::now() >= deadline + grace {
            // A process surviving SIGKILL is the platform misbehaving, not
            // a supervision decision this lane can make; report it as
            // unverifiable so the caller fails closed.
            return StopOutcome::Unverifiable;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

/// Sends one signal through `/bin/sh`'s `kill` builtin (portable across the
/// supported darwin/linux targets without `unsafe`). `false` means the
/// signal could not be delivered — usually a process that is already gone.
#[cfg(unix)]
fn signal_process(pid: u32, signal: &str) -> bool {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -{signal} -- {pid}"))
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn signal_process(_pid: u32, _signal: &str) -> bool {
    false
}

/// Stops a child this supervisor process owns: terminate signal, bounded
/// wait, then the hard kill (`Child::kill`) — always collecting the exit
/// status so no zombie remains. `None` when the status could not be
/// collected.
fn stop_tracked_child(child: &mut Child, graceful: bool, grace: Duration) -> Option<ExitStatus> {
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    if graceful {
        signal_process(child.id(), "TERM");
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(STOP_POLL_INTERVAL);
        }
    }
    let _ = child.kill();
    child.wait().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::store::OccupancyMirrorUpdate;

    static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(1);

    const STAMP: &str = "2026-09-04T00:00:00.000Z";
    const LEASE: &str = "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA";
    const NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
    const CLIENT_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
    const ORIGIN: &str = "https://127.0.0.1:8443";
    const CREDENTIAL_TOKEN: &str = "wsc-supervisor-test-token";
    /// Server-minted identities a launch grant would carry.
    const WORKER_ID: &str = "wkr_TESTWORKER00000000000001";
    const WORKER_INSTANCE: &str = "winst_TESTINSTANCE0000000001";
    const WORKER_INSTANCE_2: &str = "winst_TESTINSTANCE0000000002";

    /// A long-lived worker test double: installs the terminate trap first,
    /// then signals readiness next to the config file (`$2`), then idles.
    /// The marker lets tests wait for the worker to actually reach its
    /// handler loop — on a loaded machine a whole `/bin/sh` boot can take
    /// seconds, and a terminate landing before the trap is installed is the
    /// platform's default-disposition death.
    const LONG_RUNNING_BODY: &str = "trap 'exit 0' TERM\n\
         echo ready > \"$(dirname \"$2\")/worker-ready\"\n\
         while :; do sleep 0.1; done";
    /// A crashing worker test double: dies immediately with status 3.
    const CRASH_BODY: &str = "exit 3";

    /// Waits until the worker test double signals readiness (its handler
    /// loop is running) — bounded, so a failure cannot hang the suite.
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

    /// Opens a supervisor whose store already mirrors `lease`/`token`, so
    /// spawn/stop fencing authorizations pass.
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
            worker_credential_token: CREDENTIAL_TOKEN,
            repository_binding_id: "rbn_TESTREPO",
            launch_grant_id: "wlg_TESTGRANT",
            product_session_id: None,
            stage_run_id: None,
            source_directory,
            data_directory,
            worker_root,
        }
    }

    /// Kills every listed session through the supervisor on drop, so a
    /// failing assertion cannot leak worker processes.
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

    /// The exact camelCase field set `winwincode-worker --managed-session`
    /// accepts (fail-closed `deny_unknown_fields` reader). If the Worker
    /// entry's contract moves, this pin fails and the config writer must
    /// move with it.
    const WORKER_CONFIG_CONTRACT_FIELDS: [&str; 15] = [
        "clientNodeId",
        "clientInstanceId",
        "occupancyLeaseId",
        "occupancyFencingToken",
        "repositoryBindingId",
        "productSessionId",
        "stageRunId",
        "workerSessionId",
        "workerId",
        "workerInstanceId",
        "sourceDirectory",
        "dataDirectory",
        "serverOrigin",
        "workerCredentialPath",
        "modelRoute",
    ];

    #[test]
    // The end-to-end spawn lifecycle asserts registry, config contract, file
    // modes, liveness, and the stop observation in one realistic flow.
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

        // Handle and registry agree; the platform boot identity is bound.
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

        // The config and credential files exist with exactly mode 0600 and
        // the config matches the Worker entry's field contract exactly.
        assert_mode_0600(&data.join("managed-session.json"));
        assert_mode_0600(&data.join("worker-credential"));
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
            "serverOrigin",
            "workerCredentialPath",
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
        assert_eq!(object["serverOrigin"], ORIGIN);
        assert_eq!(
            object["workerCredentialPath"],
            data.join("worker-credential").to_str().expect("utf8")
        );
        assert_eq!(
            fs::read_to_string(data.join("worker-credential")).expect("credential read"),
            CREDENTIAL_TOKEN
        );

        // The process is really running and reaped as exited on stop.
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            1
        );
        assert!(supervisor.reap().expect("reap").is_empty(), "still alive");

        // Wait for the double's handler loop before judging the graceful
        // stop's exit status.
        wait_for_ready_marker(&data, "wss_ONE");

        let stopped = supervisor.stop("wss_ONE", true).expect("graceful stop");
        assert_eq!(stopped.state, WORKER_STATE_EXITED);
        assert_eq!(stopped.exit_code, Some(0));
        assert_eq!(
            supervisor
                .count_worker_processes_in_state(WORKER_STATE_RUNNING)
                .expect("count"),
            0
        );
        // Idempotent: stopping a terminal row returns it unchanged.
        let again = supervisor.stop("wss_ONE", true).expect("second stop");
        assert_eq!(again.state, WORKER_STATE_EXITED);
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
        // No mirror at all: fail closed.
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

        // A stale token on the same lease refuses the same way.
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
    fn local_capacity_cap_refuses_the_spawn_when_full() {
        let bin_root = temp_root("cap-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let (supervisor, root) = supervisor_with_mirror("cap", &binary, LEASE, 7);
        // Tighten the local bound for this test.
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

        // The daemon's cancel_and_release hook: every lease worker stops.
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
        // A second pass is the idempotent no-op over terminal rows.
        let again = supervisor.stop_lease_workers(LEASE).expect("lease stop");
        assert!(again.iter().all(|outcome| !outcome.stopped));
        assert!(
            again
                .iter()
                .all(|outcome| outcome.error.is_none() && outcome.state.is_some())
        );

        // The LeaseWorkerController face answers the requested-stop count.
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

        // Claim without a running worker: the simplified +1 reserved slot.
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
    // The restart scenario spans two supervisor processes, foreign rows,
    // and a cross-process stop; splitting it would hide the timeline.
    #[allow(clippy::too_many_lines)]
    fn restart_reconcile_classifies_live_dead_and_terminal_rows() {
        let bin_root = temp_root("reconcile-bin");
        let binary = bin_root.join("winwincode-worker");
        write_executable_script(&binary, LONG_RUNNING_BODY);

        // "Previous" Device Client process: spawns workers and crashes.
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

        // The crashing boot runs through its own binary (the worker binary
        // is a supervisor-level configuration) over the same registry.
        let crash_binary = bin_root.join("crashing-worker");
        write_executable_script(&crash_binary, CRASH_BODY);
        let mut crash_config = test_config(&crash_binary);
        crash_config.worker_binary_path = Some(crash_binary.clone());
        let crash_supervisor = SessionSupervisor::new(
            crash_config,
            DeviceStore::open(&root).expect("store reopens for the crasher"),
        )
        .expect("crash supervisor builds");
        let mut crash_request =
            spawn_request("wss_CRASHED", WORKER_INSTANCE, &source, &data, &worker_root);
        crash_request.worker_credential_token = "wsc-second-token";
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

        // A restart's supervisor sees the same registry without any child.
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

        // Rows the previous process never wrote: a live foreign boot and a
        // dead pid. The restart classifies them by pid + boot identity.
        let mut foreign_store = DeviceStore::open(&root).expect("third connection");
        let test_pid = std::process::id();
        foreign_store
            .put_worker_process(&WorkerProcessRecord {
                worker_session_id: "wss_FOREIGN".to_owned(),
                worker_id: "wrk_FOREIGN".to_owned(),
                worker_instance_id: "cix_FOREIGNINSTANCE00000000".to_owned(),
                pid: test_pid,
                // A wrong identity: this pid names a different boot.
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
        // A pid that is certainly absent: an already-reaped exit of this
        // test process (walk aside if the OS recycled it instantly).
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

        // The restarted supervisor can still stop the live worker through
        // the registered pid; the owning supervisor observes the exit.
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

        // Malformed origin refuses the supervisor construction.
        let mut bad_config = test_config(&binary);
        bad_config.server_origin = "http://127.0.0.1:8443".to_owned();
        let error = SessionSupervisor::new(bad_config, DeviceStore::open(&root).expect("open"))
            .expect_err("http origin must refuse");
        assert!(error.to_string().contains("https"), "{error}");

        // The remaining field-validation refusals happen before any
        // supervisor construction would matter; the store is consumed here
        // to keep one store per scenario.
        drop(SessionSupervisor::new(test_config(&binary), store).expect("builds"));
        let source = root.join("repo");
        let data = root.join("data");
        let worker_root = root.join("worker-root");

        // Field validation is directly testable; the spawn path judges the
        // fencing stamp first (a zero token can never match a mirror).
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

        let mut request = spawn_request(
            "wss_VALIDATE",
            WORKER_INSTANCE,
            &source,
            &data,
            &worker_root,
        );
        // An empty credential material is the deferred-delivery placeholder:
        // accepted at spawn, filled later via write_worker_credential.
        request.worker_credential_token = "";
        assert!(
            validate_request(request).is_ok(),
            "deferred material is valid"
        );

        // A configured worker binary that is missing refuses the spawn.
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
        assert!(
            matches!(error, SupervisorError::WorkerBinary { .. }),
            "{error}"
        );
        assert!(
            supervisor
                .worker_process("wss_VALIDATE")
                .expect("read")
                .is_none(),
            "a failed spawn writes no registry row"
        );
    }
}
