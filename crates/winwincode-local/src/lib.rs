// SPDX-License-Identifier: Apache-2.0

//! Single-process composition of the Rust Control Plane and Execution Worker.
//!
//! The launcher owns only process lifecycle and transport wiring. Product
//! state remains in the injected Control Plane endpoint, execution remains in
//! [`WorkerMain`], and every message crosses the canonical typed
//! `ExecutionPort` adapter used by separated deployments.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use winwincode_control_plane::{ControlPlaneInstanceRuntime, ControlPlaneInstanceRuntimeConfig};
use winwincode_worker::composition::{
    AdapterError, EndpointSide, ExecutionPortCore, FrameDirection, LocalWorkerAdapter, TypedFrame,
};
use winwincode_worker::{
    CodexCoreAdapter, ObserverMode, WorkerConfig, WorkerError, WorkerExecutionPort,
    WorkerLifecycleState, WorkerMain, WorkerShutdownReport,
    workspace_runtime::{JobWorkspaceRuntime, ObservationModelConfiguration},
};

use winwincode_worker::composition::{ExecutionPortMessage, Instant};

pub use winwincode_observability::{
    FactDigest as LocalTraceFactDigest, TraceContext as LocalObservationTraceContext,
};

const MAX_TRACE_FRAMES: usize = 256;
const MAX_TRACE_IDENTIFIER_BYTES: usize = 160;
const MAX_TRACE_KIND_BYTES: usize = 64;

/// Stable local-launcher failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalLauncherErrorKind {
    /// Process configuration was invalid before startup.
    InvalidConfiguration,
    /// The Control Plane instance could not start, drain, or release.
    ControlPlaneLifecycle,
    /// The canonical typed frame was rejected.
    ExecutionPortFrame,
    /// The injected Control Plane endpoint rejected a message.
    ControlPlaneEndpoint,
    /// The bounded trace rejected unsafe or excessive data.
    Trace,
    /// The Worker lifecycle rejected an operation.
    Worker,
    /// A shared local handle was poisoned.
    SharedState,
}

/// Secret-safe local-launcher error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalLauncherError {
    kind: LocalLauncherErrorKind,
    message: &'static str,
}

impl LocalLauncherError {
    const fn new(kind: LocalLauncherErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> LocalLauncherErrorKind {
        self.kind
    }

    const fn invalid_configuration() -> Self {
        Self::new(
            LocalLauncherErrorKind::InvalidConfiguration,
            "local launcher configuration is invalid",
        )
    }

    const fn control_plane_lifecycle() -> Self {
        Self::new(
            LocalLauncherErrorKind::ControlPlaneLifecycle,
            "local Control Plane lifecycle failed",
        )
    }

    const fn frame() -> Self {
        Self::new(
            LocalLauncherErrorKind::ExecutionPortFrame,
            "local ExecutionPort frame was rejected",
        )
    }

    const fn endpoint() -> Self {
        Self::new(
            LocalLauncherErrorKind::ControlPlaneEndpoint,
            "local Control Plane endpoint rejected an ExecutionPort message",
        )
    }

    const fn trace() -> Self {
        Self::new(
            LocalLauncherErrorKind::Trace,
            "local runtime trace rejected a frame",
        )
    }

    const fn worker() -> Self {
        Self::new(
            LocalLauncherErrorKind::Worker,
            "local Execution Worker lifecycle failed",
        )
    }

    const fn shared_state() -> Self {
        Self::new(
            LocalLauncherErrorKind::SharedState,
            "local launcher shared state is unavailable",
        )
    }
}

impl fmt::Display for LocalLauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for LocalLauncherError {}

/// Validated configuration for one local process generation.
#[derive(Clone, Debug)]
pub struct LocalLauncherConfig {
    data_directory: PathBuf,
    source_directory: PathBuf,
    control_plane_started_at_millis: u64,
    control_plane: ControlPlaneInstanceRuntimeConfig,
    trace_capacity: usize,
    observer_mode: ObserverMode,
    observation_model: Option<ObservationModelConfiguration>,
}

impl LocalLauncherConfig {
    /// Validates one process-local composition configuration.
    ///
    /// # Errors
    ///
    /// Rejects a non-absolute data directory, zero process time, or a trace
    /// capacity outside the fixed 1..=256 bound before creating any files.
    pub fn try_new(
        data_directory: impl AsRef<Path>,
        source_directory: impl AsRef<Path>,
        control_plane_started_at_millis: u64,
        control_plane: ControlPlaneInstanceRuntimeConfig,
        trace_capacity: usize,
    ) -> Result<Self, LocalLauncherError> {
        let data_directory = data_directory.as_ref();
        let source_directory = source_directory.as_ref();
        if !data_directory.is_absolute()
            || !source_directory.is_absolute()
            || !source_directory.is_dir()
            || control_plane_started_at_millis == 0
            || !(1..=MAX_TRACE_FRAMES).contains(&trace_capacity)
        {
            return Err(LocalLauncherError::invalid_configuration());
        }
        Ok(Self {
            data_directory: data_directory.to_path_buf(),
            source_directory: source_directory.to_path_buf(),
            control_plane_started_at_millis,
            control_plane,
            trace_capacity,
            observer_mode: ObserverMode::Off,
            observation_model: None,
        })
    }

    /// Selects the Worker Observer policy for this local process.
    #[must_use]
    pub const fn with_observer_mode(mut self, mode: ObserverMode) -> Self {
        self.observer_mode = mode;
        self
    }

    /// Installs the independent one-shot Observer route for the local Worker.
    #[must_use]
    pub fn with_observation_model(mut self, model: ObservationModelConfiguration) -> Self {
        self.observation_model = Some(model);
        self
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    /// Controlled root containing repositories named by `ExecutionJob`.
    #[must_use]
    pub fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    #[must_use]
    pub const fn trace_capacity(&self) -> usize {
        self.trace_capacity
    }
}

/// Trace direction after canonical frame validation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalTraceDirection {
    WorkerToControlPlane,
    ControlPlaneToWorker,
}

impl From<FrameDirection> for LocalTraceDirection {
    fn from(direction: FrameDirection) -> Self {
        match direction {
            FrameDirection::WorkerToControlPlane => Self::WorkerToControlPlane,
            FrameDirection::ControlPlaneToWorker => Self::ControlPlaneToWorker,
        }
    }
}

/// One allow-listed correlation frame. It contains no payload, summary,
/// provider route, command text, path, artifact content, or credential field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalTraceFrame {
    ordinal: u64,
    direction: LocalTraceDirection,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    product_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_sequence: Option<i64>,
}

impl LocalTraceFrame {
    fn from_typed(ordinal: u64, frame: &TypedFrame) -> Result<Self, LocalLauncherError> {
        let value =
            serde_json::to_value(frame.message()).map_err(|_| LocalLauncherError::trace())?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| safe_kind(kind))
            .ok_or_else(LocalLauncherError::trace)?
            .to_owned();
        Ok(Self {
            ordinal,
            direction: frame.direction().into(),
            kind,
            message_id: safe_path(&value, &[&["messageId"]])?,
            worker_id: safe_path(&value, &[&["workerId"], &["lease", "workerId"]])?,
            worker_instance_id: safe_path(
                &value,
                &[&["workerInstanceId"], &["lease", "workerInstanceId"]],
            )?,
            job_id: safe_path(
                &value,
                &[&["jobId"], &["job", "jobId"], &["lease", "jobId"]],
            )?,
            lease_id: safe_path(&value, &[&["leaseId"], &["lease", "leaseId"]])?,
            worker_session_id: safe_path(&value, &[&["workerSessionId"]])?,
            product_session_id: safe_path(
                &value,
                &[
                    &["productSessionId"],
                    &["sessionIdentity", "productSessionId"],
                    &["job", "scope", "productSessionId"],
                ],
            )?,
            stage_run_id: safe_path(
                &value,
                &[
                    &["stageRunId"],
                    &["sessionIdentity", "stageRunId"],
                    &["job", "scope", "stageRunId"],
                ],
            )?,
            codex_thread_id: safe_path(
                &value,
                &[
                    &["codexThreadId"],
                    &["sessionIdentity", "codexThreadId"],
                    &["outcome", "codexThreadId"],
                ],
            )?,
            event_sequence: value
                .get("event")
                .and_then(|event| event.get("sequence"))
                .and_then(Value::as_i64),
        })
    }
}

/// Bounded, deterministic, secret-safe trace of the local typed adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalRuntimeTrace {
    capacity: usize,
    frames: Vec<LocalTraceFrame>,
}

impl LocalRuntimeTrace {
    /// Creates an empty bounded trace.
    ///
    /// # Errors
    ///
    /// Rejects a capacity outside 1..=256.
    pub fn try_new(capacity: usize) -> Result<Self, LocalLauncherError> {
        if !(1..=MAX_TRACE_FRAMES).contains(&capacity) {
            return Err(LocalLauncherError::invalid_configuration());
        }
        Ok(Self {
            capacity,
            frames: Vec::with_capacity(capacity),
        })
    }

    /// Records only the canonical message kind and allow-listed correlation
    /// identities from an already validated frame.
    ///
    /// # Errors
    ///
    /// Fails closed when the bound is full or a selected identity is unsafe.
    pub fn record(&mut self, frame: &TypedFrame) -> Result<(), LocalLauncherError> {
        if self.frames.len() >= self.capacity {
            return Err(LocalLauncherError::trace());
        }
        let ordinal = u64::try_from(self.frames.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(LocalLauncherError::trace)?;
        self.frames
            .push(LocalTraceFrame::from_typed(ordinal, frame)?);
        Ok(())
    }

    #[must_use]
    pub fn frames(&self) -> &[LocalTraceFrame] {
        &self.frames
    }

    /// Serializes the stable field order for reproducible fixture comparison.
    ///
    /// # Errors
    ///
    /// Returns a fixed trace error if serialization fails.
    pub fn to_json(&self) -> Result<Vec<u8>, LocalLauncherError> {
        serde_json::to_vec(self).map_err(|_| LocalLauncherError::trace())
    }
}

fn safe_kind(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TRACE_KIND_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'.' | b'_'))
}

fn safe_path(
    value: &Value,
    alternatives: &[&[&str]],
) -> Result<Option<String>, LocalLauncherError> {
    for path in alternatives {
        let selected = path
            .iter()
            .try_fold(value, |current, part| current.get(part));
        if let Some(selected) = selected {
            let string = selected.as_str().ok_or_else(LocalLauncherError::trace)?;
            if !safe_identifier(string) {
                return Err(LocalLauncherError::trace());
            }
            return Ok(Some(string.to_owned()));
        }
    }
    Ok(None)
}

fn safe_identifier(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= MAX_TRACE_IDENTIFIER_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
        && ![
            "secret", "token", "password", "passwd", "api_key", "apikey", "bearer", "private",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
}

/// Shared handle to the exact Control Plane endpoint injected into the local
/// process. HTTP/WS composition can keep its application, storage, hub, and
/// `ExecutionPort` router in this one object; the launcher never copies them.
pub struct SharedControlPlaneHandle<Core> {
    inner: Arc<Mutex<Core>>,
}

impl<Core> Clone for SharedControlPlaneHandle<Core> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<Core> SharedControlPlaneHandle<Core> {
    fn new(core: Core) -> Self {
        Self {
            inner: Arc::new(Mutex::new(core)),
        }
    }

    /// Runs one short synchronous operation against the same Control Plane
    /// object used by the local Worker transport.
    ///
    /// # Errors
    ///
    /// Returns a fixed error if another panic poisoned the shared object.
    pub fn with<R>(&self, operation: impl FnOnce(&mut Core) -> R) -> Result<R, LocalLauncherError> {
        let mut core = self
            .inner
            .lock()
            .map_err(|_| LocalLauncherError::shared_state())?;
        Ok(operation(&mut *core))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Core>, LocalLauncherError> {
        self.inner
            .lock()
            .map_err(|_| LocalLauncherError::shared_state())
    }
}

struct LinkState {
    inbox: VecDeque<ExecutionPortMessage>,
    trace: LocalRuntimeTrace,
}

type SharedLink = Arc<Mutex<LinkState>>;

/// Cloneable CP-to-Worker side of the same local typed adapter.
///
/// A scheduler or Server composition enqueues generated messages here and the
/// process driver calls [`LocalLauncher::drive`] to apply them to `WorkerMain`.
pub struct LocalExecutionPortHandle {
    link: SharedLink,
}

impl Clone for LocalExecutionPortHandle {
    fn clone(&self) -> Self {
        Self {
            link: Arc::clone(&self.link),
        }
    }
}

impl LocalExecutionPortHandle {
    /// Validates and queues one Control Plane-to-Worker generated message.
    ///
    /// # Errors
    ///
    /// Rejects a wrong-direction frame, unsafe/full trace, or poisoned queue.
    pub fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), LocalLauncherError> {
        self.route_control(message)
    }

    fn record_control_response(
        &self,
        message: ExecutionPortMessage,
    ) -> Result<(), LocalLauncherError> {
        self.route_control(message)
    }

    fn route_control(&self, message: ExecutionPortMessage) -> Result<(), LocalLauncherError> {
        let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message)
            .map_err(|_| LocalLauncherError::frame())?;
        let mut link = lock_link(&self.link)?;
        link.trace.record(&frame)?;
        let mut queue = QueueCore {
            inbox: &mut link.inbox,
        };
        LocalWorkerAdapter::new(&mut queue, EndpointSide::Worker)
            .accept(&frame)
            .map_err(|_| LocalLauncherError::frame())
    }
}

struct QueueCore<'queue> {
    inbox: &'queue mut VecDeque<ExecutionPortMessage>,
}

impl ExecutionPortCore for QueueCore<'_> {
    type Output = ();
    type Error = ();

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        self.inbox.push_back(message.clone());
        Ok(())
    }
}

struct SameProcessExecutionPort<Core> {
    control_plane: SharedControlPlaneHandle<Core>,
    link: SharedLink,
}

impl<Core> WorkerExecutionPort for SameProcessExecutionPort<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>>,
{
    type Error = LocalLauncherError;

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let result = self.send_now(message);
        std::future::ready(result)
    }
}

impl<Core> SameProcessExecutionPort<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>>,
{
    fn send_now(&mut self, message: ExecutionPortMessage) -> Result<(), LocalLauncherError> {
        let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
            .map_err(|_| LocalLauncherError::frame())?;
        {
            let mut link = lock_link(&self.link)?;
            link.trace.record(&frame)?;
        }
        let responses = {
            let mut core = self.control_plane.lock()?;
            LocalWorkerAdapter::new(&mut *core, EndpointSide::ControlPlane)
                .accept(&frame)
                .map_err(|error| map_adapter_error(&error))?
        };
        let handle = LocalExecutionPortHandle {
            link: Arc::clone(&self.link),
        };
        for response in responses {
            handle.record_control_response(response)?;
        }
        Ok(())
    }
}

fn map_adapter_error<CoreError>(error: &AdapterError<CoreError>) -> LocalLauncherError {
    match error {
        AdapterError::Frame(_) => LocalLauncherError::frame(),
        AdapterError::Core(_) => LocalLauncherError::endpoint(),
    }
}

fn lock_link(link: &SharedLink) -> Result<MutexGuard<'_, LinkState>, LocalLauncherError> {
    link.lock().map_err(|_| LocalLauncherError::shared_state())
}

/// Successful, deterministic two-module shutdown report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalShutdownReport {
    pub worker: WorkerShutdownReport,
}

/// One local process containing a running Control Plane instance and Worker.
pub struct LocalLauncher<Core, Codex>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Codex: CodexCoreAdapter + Send + 'static,
{
    control_plane_instance: Option<ControlPlaneInstanceRuntime>,
    control_plane: SharedControlPlaneHandle<Core>,
    execution_port: LocalExecutionPortHandle,
    worker: WorkerMain<SameProcessExecutionPort<Core>, Codex>,
    stopped: bool,
}

impl<Core, Codex> LocalLauncher<Core, Codex>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Codex: CodexCoreAdapter + Send + 'static,
{
    /// Starts the Control Plane instance first, then registers the local Worker
    /// through the same typed adapter used by every later message.
    ///
    /// # Errors
    ///
    /// Returns fixed, secret-safe lifecycle, transport, endpoint, trace, or
    /// Worker failures. A failed Worker start releases the Control Plane lease.
    pub async fn start(
        config: LocalLauncherConfig,
        worker_config: WorkerConfig,
        control_plane_endpoint: Core,
        codex: Codex,
        worker_now: Instant,
    ) -> Result<Self, LocalLauncherError> {
        let control_plane_instance = ControlPlaneInstanceRuntime::start(
            &config.data_directory,
            config.control_plane_started_at_millis,
            config.control_plane,
        )
        .map_err(|_| LocalLauncherError::control_plane_lifecycle())?;
        let control_plane = SharedControlPlaneHandle::new(control_plane_endpoint);
        let link = Arc::new(Mutex::new(LinkState {
            inbox: VecDeque::new(),
            trace: LocalRuntimeTrace::try_new(config.trace_capacity)?,
        }));
        let execution_port = LocalExecutionPortHandle {
            link: Arc::clone(&link),
        };
        // Action requests remain on the ordinary Worker -> Control Plane
        // ExecutionPort path.  A synchronous response transport would enter
        // the Control Plane while Kernel is still inside `next_event`; that
        // re-entrant path can leave the follow-up model exchange waiting on a
        // Worker drive that cannot run until `next_event` returns.  The
        // adapter's bounded event poll already provides the retry window, so
        // the durable queue is both the local and separated-deployment path.
        let port = SameProcessExecutionPort {
            control_plane: control_plane.clone(),
            link,
        };
        let workspaces = JobWorkspaceRuntime::open(
            config.data_directory.join("worker-workspaces"),
            &config.source_directory,
        )
        .map_err(|_| LocalLauncherError::worker())?;
        let registration_namespace = worker_config.worker_instance_id.clone();
        let registration_started_at = worker_config.started_at.clone();
        let mut worker = WorkerMain::new(worker_config, port, codex, workspaces)
            .with_registration_request_namespace(&registration_namespace, &registration_started_at)
            .with_observer_mode(config.observer_mode);
        if let Some(observation_model) = config.observation_model.clone() {
            worker = worker.with_observation_model(observation_model);
        }
        let mut launcher = Self {
            control_plane_instance: Some(control_plane_instance),
            control_plane,
            execution_port,
            worker,
            stopped: false,
        };
        let worker_start = launcher.worker.start(worker_now.clone()).await;
        let worker_drive = if worker_start.is_ok() {
            launcher.drive(worker_now).await
        } else {
            Ok(0)
        };
        if worker_start.is_err() || worker_drive.is_err() {
            launcher.release_after_failed_start(config.control_plane_started_at_millis);
            return Err(LocalLauncherError::worker());
        }
        Ok(launcher)
    }

    /// Shared formal Control Plane composition object. A Server adapter can
    /// expose HTTP/WS APIs over this exact object without copying state.
    #[must_use]
    pub fn control_plane(&self) -> SharedControlPlaneHandle<Core> {
        self.control_plane.clone()
    }

    /// Shared typed CP-to-Worker queue for scheduler and Server composition.
    #[must_use]
    pub fn execution_port(&self) -> LocalExecutionPortHandle {
        self.execution_port.clone()
    }

    /// Applies every already-validated queued Control Plane message to the one
    /// Worker core. Messages emitted while driving are synchronously routed
    /// back through the Control Plane side before the next queued message.
    ///
    /// # Errors
    ///
    /// Returns a fixed shared-state or Worker failure.
    pub async fn drive(&mut self, now: Instant) -> Result<usize, LocalLauncherError> {
        let mut applied = 0_usize;
        loop {
            let message = {
                let mut link = lock_link(&self.execution_port.link)?;
                link.inbox.pop_front()
            };
            let Some(message) = message else {
                return Ok(applied);
            };
            self.worker
                .accept_control(&message, now.clone())
                .await
                .map_err(|_| LocalLauncherError::worker())?;
            applied = applied
                .checked_add(1)
                .ok_or_else(LocalLauncherError::shared_state)?;
        }
    }

    /// Enqueues and immediately drives one typed CP-to-Worker message.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`LocalExecutionPortHandle::enqueue_control`]
    /// or [`Self::drive`].
    pub async fn accept_control(
        &mut self,
        message: ExecutionPortMessage,
        now: Instant,
    ) -> Result<(), LocalLauncherError> {
        self.execution_port.enqueue_control(message)?;
        self.drive(now).await.map(|_| ())
    }

    /// Emits one canonical Worker heartbeat through the local adapter.
    ///
    /// # Errors
    ///
    /// Returns a fixed Worker failure.
    pub async fn heartbeat(&mut self, now: Instant) -> Result<(), LocalLauncherError> {
        self.worker
            .heartbeat(now)
            .await
            .map_err(|_| LocalLauncherError::worker())
    }

    /// Polls the embedded Codex adapter once for each active job.
    ///
    /// # Errors
    ///
    /// Returns a fixed Worker failure.
    pub async fn poll_codex(&mut self, now: Instant) -> Result<(), LocalLauncherError> {
        self.worker
            .poll_codex(now.clone())
            .await
            .map_err(|_error| LocalLauncherError::worker())?;
        self.drive(now).await.map(|_| ())
    }

    /// Drains the Worker, then drains and permanently releases the Control
    /// Plane process generation. The Control Plane object and transport trace
    /// remain readable, but no process owner or active Worker job remains.
    ///
    /// # Errors
    ///
    /// Returns a fixed Worker or Control Plane lifecycle failure.
    pub async fn shutdown(
        &mut self,
        worker_now: Instant,
        control_plane_now_millis: u64,
    ) -> Result<LocalShutdownReport, LocalLauncherError> {
        if self.stopped {
            return Err(LocalLauncherError::worker());
        }
        // Shutdown is a resource boundary: even when the Worker reports a
        // failure, the Control Plane lease must still be drained and released
        // before this launcher is dropped. Keep the first stable error while
        // running every cleanup step.
        let mut first_error = None;
        let worker = if let Ok(report) = self.worker.shutdown(worker_now).await {
            Some(report)
        } else {
            first_error = Some(LocalLauncherError::worker());
            None
        };
        if self.pending_frame_count() != 0 {
            first_error.get_or_insert_with(LocalLauncherError::shared_state);
        }

        if let Some(control_plane) = self.control_plane_instance.as_mut() {
            if control_plane.begin_drain(control_plane_now_millis).is_err() {
                first_error.get_or_insert_with(LocalLauncherError::control_plane_lifecycle);
            }
            if control_plane
                .await_drained(control_plane_now_millis)
                .is_err()
            {
                first_error.get_or_insert_with(LocalLauncherError::control_plane_lifecycle);
            }
            if control_plane.release(control_plane_now_millis).is_err() {
                first_error.get_or_insert_with(LocalLauncherError::control_plane_lifecycle);
            }
        } else {
            first_error.get_or_insert_with(LocalLauncherError::control_plane_lifecycle);
        }
        // Drop the process generation even when a lifecycle operation above
        // failed. The returned error records the failed phase; the lease and
        // owned handles do not remain reachable from this launcher.
        drop(self.control_plane_instance.take());

        let Some(worker) = worker else {
            return Err(first_error.unwrap_or_else(LocalLauncherError::worker));
        };
        if let Some(error) = first_error {
            return Err(error);
        }
        self.stopped = true;
        Ok(LocalShutdownReport { worker })
    }

    #[must_use]
    pub const fn worker_lifecycle(&self) -> WorkerLifecycleState {
        self.worker.lifecycle()
    }

    #[must_use]
    pub fn active_job_count(&self) -> usize {
        self.worker.active_jobs().len()
    }

    #[must_use]
    pub fn pending_frame_count(&self) -> usize {
        lock_link(&self.execution_port.link).map_or(usize::MAX, |link| link.inbox.len())
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped
            && self.control_plane_instance.is_none()
            && self.worker.lifecycle() == WorkerLifecycleState::Stopped
            && self.active_job_count() == 0
            && self.pending_frame_count() == 0
    }

    /// Returns a stable JSON snapshot of the bounded trace.
    ///
    /// # Errors
    ///
    /// Returns a fixed shared-state or trace serialization error.
    pub fn trace_json(&self) -> Result<Vec<u8>, LocalLauncherError> {
        lock_link(&self.execution_port.link)?.trace.to_json()
    }

    /// Starts a fresh bounded trace window for a long-lived local runtime.
    ///
    /// A supervised process may emit heartbeats for its entire lifetime. The
    /// production supervisor rotates this diagnostic window between drive
    /// cycles so a maintenance stream cannot exhaust the bounded trace while
    /// preserving the fail-closed behavior of [`LocalRuntimeTrace::record`]
    /// for each individual window.
    /// # Errors
    ///
    /// Returns a fixed shared-state error when the local runtime link is
    /// unavailable.
    pub fn reset_trace(&mut self) -> Result<(), LocalLauncherError> {
        lock_link(&self.execution_port.link)?.trace.frames.clear();
        Ok(())
    }

    fn release_after_failed_start(&mut self, now: u64) {
        if let Some(mut control_plane) = self.control_plane_instance.take() {
            let _ = control_plane.begin_drain(now);
            let _ = control_plane.release(now);
        }
        self.stopped = true;
    }
}

impl From<WorkerError> for LocalLauncherError {
    fn from(_: WorkerError) -> Self {
        Self::worker()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXECUTION_PORT_FIXTURE: &str =
        include_str!("../../../tests/fixtures/contracts/execution-port.valid.json");

    fn fixture_messages() -> Vec<ExecutionPortMessage> {
        let fixture: Value = serde_json::from_str(EXECUTION_PORT_FIXTURE).expect("fixture JSON");
        fixture["messages"]
            .as_array()
            .expect("fixture messages")
            .iter()
            .map(|message| {
                serde_json::from_value(message.clone()).expect("generated fixture message")
            })
            .collect()
    }

    fn execution_port_handle() -> LocalExecutionPortHandle {
        LocalExecutionPortHandle {
            link: Arc::new(Mutex::new(LinkState {
                inbox: VecDeque::new(),
                trace: LocalRuntimeTrace::try_new(64).expect("trace"),
            })),
        }
    }

    struct ScriptedControlPlane {
        responses: VecDeque<ExecutionPortMessage>,
    }

    impl ExecutionPortCore for ScriptedControlPlane {
        type Output = Vec<ExecutionPortMessage>;
        type Error = ();

        fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
            Ok(vec![self.responses.pop_front().expect("scripted response")])
        }
    }

    #[test]
    fn every_generated_control_plane_message_is_queued_for_the_worker() {
        let handle = execution_port_handle();
        let messages = fixture_messages()
            .into_iter()
            .filter(|message| {
                FrameDirection::for_message(message).expect("known generated message")
                    == FrameDirection::ControlPlaneToWorker
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 13, "generated CP-to-Worker family changed");

        for message in &messages {
            handle
                .enqueue_control(message.clone())
                .expect("canonical CP-to-Worker message");
        }

        let link = lock_link(&handle.link).expect("link");
        assert_eq!(
            link.inbox.iter().collect::<Vec<_>>(),
            messages.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_generated_control_plane_response_is_queued_for_the_worker() {
        let fixture = fixture_messages();
        let responses = fixture
            .iter()
            .filter(|message| {
                FrameDirection::for_message(message).expect("known generated message")
                    == FrameDirection::ControlPlaneToWorker
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 13, "generated CP-to-Worker family changed");
        let request = fixture
            .into_iter()
            .find(|message| {
                FrameDirection::for_message(message).expect("known generated message")
                    == FrameDirection::WorkerToControlPlane
            })
            .expect("Worker-to-Control-Plane fixture");
        let handle = execution_port_handle();
        let mut port = SameProcessExecutionPort {
            control_plane: SharedControlPlaneHandle::new(ScriptedControlPlane {
                responses: responses.iter().cloned().collect(),
            }),
            link: Arc::clone(&handle.link),
        };

        for _ in &responses {
            port.send_now(request.clone())
                .expect("canonical Control Plane response");
        }

        let link = lock_link(&handle.link).expect("link");
        assert_eq!(
            link.inbox.iter().collect::<Vec<_>>(),
            responses.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn worker_to_control_plane_message_fails_closed_before_queueing() {
        let handle = execution_port_handle();
        let message = fixture_messages()
            .into_iter()
            .find(|message| {
                FrameDirection::for_message(message).expect("known generated message")
                    == FrameDirection::WorkerToControlPlane
            })
            .expect("Worker-to-Control-Plane fixture");

        let error = handle
            .enqueue_control(message)
            .expect_err("wrong-direction control message");
        assert_eq!(error.kind(), LocalLauncherErrorKind::ExecutionPortFrame);
        assert!(lock_link(&handle.link).expect("link").inbox.is_empty());
    }
}
