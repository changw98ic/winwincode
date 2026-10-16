// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
};

use crate::supervisor::PROTOCOL_WORKER_SESSION_CAPACITY;
use crate::{
    AuthorizedPreviewSourceRegistry, DaemonConfig, DaemonError, DeviceDaemon, DeviceIdentitySeed,
    DeviceStore, DeviceStoreError, HttpExchangeTransport, IdentityRecord, LeaseWorkerController,
    ManagedAppError, ManagedAppRepositorySource, ManagedAppSupervisor, ManagedAppSupervisorConfig,
    PreviewTunnelError, PreviewTunnelSupervisor, SessionSupervisor, SupervisorConfig,
    SupervisorError, TickOutcome, WorkerCapacitySource, WorkerLaunchDirectories,
    WorkerLaunchMaterialSource, ensure_device_identity, load_device_identity,
};

const PID_FILE: &str = "device-client.pid";
const RESTART_FILE: &str = "device-client.restart";
const LOG_FILE: &str = "device-client.log";
const OLD_LOG_FILE: &str = "device-client.log.1";
const LOG_LIMIT_BYTES: u64 = 1024 * 1024;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const EXCHANGE_PATH: &str = "/internal/v1/client/exchange";
const PREVIEW_TUNNEL_PATH: &str = "/internal/v1/preview/tunnel";
const MANAGED_APP_EXECUTABLE_ALLOWLIST_ENVIRONMENT: &str =
    "WWC_DEVICE_MANAGED_APP_EXECUTABLE_ALLOWLIST";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceServiceConfig {
    pub data_directory: PathBuf,
    pub server_url: String,
    pub server_display_name: String,
    pub device_display_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceServiceStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub restart_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_failure: Option<String>,
}

#[derive(Debug)]
pub enum DeviceServiceError {
    InvalidConfig(String),
    AlreadyRunning(u32),
    NotRunning,
    Io(std::io::Error),
    Store(DeviceStoreError),
    Daemon(DaemonError),
    Supervisor(SupervisorError),
    ManagedApp(ManagedAppError),
    Preview(PreviewTunnelError),
}

impl fmt::Display for DeviceServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "device service config: {message}"),
            Self::AlreadyRunning(pid) => {
                write!(formatter, "device service is already running as pid {pid}")
            }
            Self::NotRunning => formatter.write_str("device service is not running"),
            Self::Io(error) => write!(formatter, "device service file: {error}"),
            Self::Store(error) => write!(formatter, "device service store: {error}"),
            Self::Daemon(error) => write!(formatter, "device service daemon: {error}"),
            Self::Supervisor(error) => write!(formatter, "device service worker: {error}"),
            Self::ManagedApp(error) => write!(formatter, "device service managed app: {error}"),
            Self::Preview(error) => write!(formatter, "device service preview tunnel: {error}"),
        }
    }
}

impl std::error::Error for DeviceServiceError {}

impl From<std::io::Error> for DeviceServiceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<DeviceStoreError> for DeviceServiceError {
    fn from(error: DeviceStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DaemonError> for DeviceServiceError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<SupervisorError> for DeviceServiceError {
    fn from(error: SupervisorError) -> Self {
        Self::Supervisor(error)
    }
}

impl From<ManagedAppError> for DeviceServiceError {
    fn from(error: ManagedAppError) -> Self {
        Self::ManagedApp(error)
    }
}

impl From<PreviewTunnelError> for DeviceServiceError {
    fn from(error: PreviewTunnelError) -> Self {
        Self::Preview(error)
    }
}

struct ServiceWorkerLaunchMaterial {
    data_directory: PathBuf,
}

impl WorkerLaunchMaterialSource for ServiceWorkerLaunchMaterial {
    fn launch_directories(
        &self,
        worker_session_id: &str,
        repository_binding_id: &str,
    ) -> Option<WorkerLaunchDirectories> {
        if !canonical_worker_session_id(worker_session_id) {
            return None;
        }
        let store = DeviceStore::open(&self.data_directory).ok()?;
        let mapping = store.path_mapping(repository_binding_id).ok()??;
        let source_directory = PathBuf::from(mapping.canonical_path);
        if !source_directory.is_dir() {
            return None;
        }
        let root = self
            .data_directory
            .join("worker-sessions")
            .join(worker_session_id);
        Some(WorkerLaunchDirectories {
            source_directory,
            data_directory: root.join("data"),
            worker_root: root.join("root"),
        })
    }
}

impl ManagedAppRepositorySource for ServiceWorkerLaunchMaterial {
    fn repository_root(&self, repository_binding_id: &str) -> Option<PathBuf> {
        let store = DeviceStore::open(&self.data_directory).ok()?;
        let mapping = store.path_mapping(repository_binding_id).ok()??;
        let root = PathBuf::from(mapping.canonical_path);
        root.is_dir().then_some(root)
    }
}

pub fn run_device_service(config: &DeviceServiceConfig) -> Result<(), DeviceServiceError> {
    run_device_service_with_ca(config, None)
}

pub fn run_device_service_with_ca(
    config: &DeviceServiceConfig,
    ca_file: Option<&Path>,
) -> Result<(), DeviceServiceError> {
    let ca_roots = crate::remote_auth::load_ca_roots(ca_file)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let stop = Arc::new(AtomicBool::new(false));
    let service_stop = Arc::clone(&stop);
    let config = config.clone();
    runtime.block_on(async move {
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let shutdown = async {
            #[cfg(unix)]
            {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => result,
                    _ = terminate.recv() => Ok(()),
                }
            }
            #[cfg(not(unix))]
            tokio::signal::ctrl_c().await
        };
        let mut service = tokio::task::spawn_blocking(move || {
            run_device_service_with_roots_until(&config, &ca_roots, || {
                service_stop.load(Ordering::Acquire)
            })
        });
        tokio::select! {
            result = &mut service => result.map_err(std::io::Error::other)?,
            result = shutdown => {
                stop.store(true, Ordering::Release);
                let stopped = service.await.map_err(std::io::Error::other)?;
                result?;
                stopped
            }
        }
    })
}

#[cfg(test)]
fn run_device_service_until(
    config: &DeviceServiceConfig,
    should_stop: impl FnMut() -> bool,
) -> Result<(), DeviceServiceError> {
    run_device_service_with_roots_until(config, &[], should_stop)
}

#[allow(clippy::too_many_lines)]
fn run_device_service_with_roots_until(
    config: &DeviceServiceConfig,
    ca_roots: &[Vec<u8>],
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), DeviceServiceError> {
    prepare_data_directory(&config.data_directory)?;
    let _lease = ServiceLease::acquire(&config.data_directory)?;
    let restart_path = config.data_directory.join(RESTART_FILE);
    if restart_path.exists() {
        fs::remove_file(&restart_path)?;
    }
    append_log(&config.data_directory, "service started")?;
    let preview_registry = AuthorizedPreviewSourceRegistry::default();

    loop {
        let mut store = DeviceStore::open(&config.data_directory)?;
        crate::remote_auth::retry_remote_authentication(&store)?;
        if crate::remote_auth::load_remote_binding(&store)?
            .is_some_and(|binding| !binding.is_bound())
        {
            crate::remote_auth::bind_device(&crate::remote_auth::DeviceBindConfig {
                data_directory: config.data_directory.clone(),
                server_url: None,
                pairing_token: None,
                ca_file: None,
                seed: device_identity_seed(&config.device_display_name),
            })?;
        }
        let binding = crate::remote_auth::load_remote_binding(&store)?;
        if let Some(reason) = binding
            .as_ref()
            .and_then(|binding| binding.failure_reason.as_deref())
        {
            return Err(DeviceServiceError::InvalidConfig(reason.to_owned()));
        }
        let server_url = binding
            .as_ref()
            .map_or(config.server_url.as_str(), |binding| {
                binding.server_url.as_str()
            });
        let endpoint = exchange_endpoint(server_url)?;
        let origin = server_origin(server_url)?;
        let auth = binding
            .as_ref()
            .map(crate::remote_auth::RemoteBinding::auth)
            .map(|auth| {
                if ca_roots.is_empty() {
                    auth
                } else {
                    auth.with_ca_roots(ca_roots.to_vec())
                }
            });
        let identity = ensure_device_identity(
            &mut store,
            &device_identity_seed(&config.device_display_name),
            &stamp(),
        )?;
        let transport =
            HttpExchangeTransport::new(endpoint.clone()).with_ca_roots(ca_roots.to_vec());
        let transport = Arc::new(if let Some(auth) = &auth {
            transport.with_device_auth(auth.clone())
        } else {
            transport
        });
        let mut daemon = DeviceDaemon::start(
            DaemonConfig {
                server_profile_id: "default".to_owned(),
                base_url: endpoint.clone(),
                server_display_name: config.server_display_name.clone(),
                device_display_name: config.device_display_name.clone(),
                platform: release_target().0,
                architecture: release_target().1,
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
                heartbeat_interval: Duration::from_secs(15),
                enroll_poll_interval: Duration::from_secs(1),
                max_frames_per_exchange: 16,
                initial_backoff: Duration::from_secs(1),
                max_backoff: Duration::from_secs(30),
                capacity: service_worker_capacity(),
            },
            store,
            transport.clone(),
            &identity,
        )?;
        crate::apply_engine::recover_started_applications(&mut daemon)
            .map_err(|error| DeviceServiceError::InvalidConfig(error.to_string()))?;
        let mut worker_lane_wired = false;
        let mut preview_tunnel = None;
        if identity.identity().is_enrolled() {
            wire_worker_lane(
                &mut daemon,
                &config.data_directory,
                &origin,
                identity.identity().client_node_id(),
                identity.current_instance_id(),
                preview_registry.clone(),
                transport.clone(),
            )?;
            worker_lane_wired = true;
            preview_tunnel = Some(start_preview_tunnel(
                &origin,
                &identity,
                preview_registry.clone(),
                auth.clone(),
            )?);
        }
        let mut last_retry = None;
        loop {
            if should_stop() {
                drop(preview_tunnel.take());
                daemon.shutdown_workers()?;
                append_log(&config.data_directory, "service stopped")?;
                return Ok(());
            }
            if restart_path.exists() {
                fs::remove_file(&restart_path)?;
                append_log(&config.data_directory, "daemon session restarted")?;
                drop(preview_tunnel.take());
                daemon.shutdown_workers()?;
                break;
            }
            let tick = match daemon.tick(Instant::now()) {
                Ok(tick) => tick,
                Err(error) => {
                    drop(preview_tunnel.take());
                    daemon.shutdown_workers()?;
                    append_log(&config.data_directory, &error.to_string())?;
                    return Err(DeviceServiceError::Daemon(error));
                }
            };
            let ready_in = match tick {
                TickOutcome::Waiting { ready_in } => ready_in,
                TickOutcome::Retrying { after, reason } => {
                    if last_retry.as_deref() != Some(reason.as_str()) {
                        append_log(&config.data_directory, &format!("exchange retry: {reason}"))?;
                        last_retry = Some(reason);
                    }
                    after
                }
                TickOutcome::Exchanged { .. } => {
                    last_retry = None;
                    Duration::ZERO
                }
            };
            if !worker_lane_wired && daemon.is_enrolled() {
                let store = DeviceStore::open(&config.data_directory)?;
                let enrolled = load_device_identity(&store)?.ok_or_else(|| {
                    DeviceServiceError::InvalidConfig(
                        "enrolled daemon has no local identity".to_owned(),
                    )
                })?;
                wire_worker_lane(
                    &mut daemon,
                    &config.data_directory,
                    &origin,
                    enrolled.identity().client_node_id(),
                    enrolled.current_instance_id(),
                    preview_registry.clone(),
                    transport.clone(),
                )?;
                worker_lane_wired = true;
                let enrolled = load_device_identity(&DeviceStore::open(&config.data_directory)?)?
                    .ok_or_else(|| {
                    DeviceServiceError::InvalidConfig(
                        "enrolled daemon has no local identity".to_owned(),
                    )
                })?;
                preview_tunnel = Some(start_preview_tunnel(
                    &origin,
                    &enrolled,
                    preview_registry.clone(),
                    auth.clone(),
                )?);
                append_log(&config.data_directory, "local worker lane ready")?;
            }
            thread::sleep(ready_in.min(CONTROL_POLL_INTERVAL));
        }
    }
}

fn start_preview_tunnel(
    origin: &str,
    identity: &IdentityRecord,
    registry: AuthorizedPreviewSourceRegistry,
    auth: Option<crate::remote_auth::DeviceRemoteAuth>,
) -> Result<PreviewTunnelSupervisor, DeviceServiceError> {
    PreviewTunnelSupervisor::start_with_auth(
        preview_tunnel_endpoint(origin)?,
        identity.identity().client_node_id().to_owned(),
        registry,
        auth,
    )
    .map_err(DeviceServiceError::from)
}

fn preview_tunnel_endpoint(server_url: &str) -> Result<String, DeviceServiceError> {
    let origin = server_origin(server_url)?;
    let uri = http::Uri::from_str(&origin).expect("validated origin");
    let scheme = if uri.scheme_str() == Some("https") {
        "wss"
    } else {
        "ws"
    };
    Ok(format!(
        "{scheme}://{}{PREVIEW_TUNNEL_PATH}",
        uri.authority().expect("validated authority")
    ))
}

fn wire_worker_lane(
    daemon: &mut DeviceDaemon,
    data_directory: &Path,
    server_origin: &str,
    client_node_id: &str,
    client_instance_id: &str,
    preview_sources: AuthorizedPreviewSourceRegistry,
    transport: Arc<dyn crate::ExchangeTransport>,
) -> Result<(), DeviceServiceError> {
    let supervisor = SessionSupervisor::new(
        service_worker_supervisor_config(server_origin, client_node_id, client_instance_id),
        DeviceStore::open(data_directory)?,
    )?;
    supervisor.set_worker_relay_transport(transport);
    daemon.set_worker_supervisor(supervisor.clone());
    daemon
        .set_worker_capacity_source(Arc::new(supervisor.clone()) as Arc<dyn WorkerCapacitySource>);
    daemon.set_lease_worker_controller(Arc::new(supervisor) as Arc<dyn LeaseWorkerController>);
    daemon.set_worker_launch_material_source(Arc::new(ServiceWorkerLaunchMaterial {
        data_directory: data_directory.to_path_buf(),
    }));
    let managed_app = ManagedAppSupervisor::open(ManagedAppSupervisorConfig {
        data_directory: data_directory.join("managed-app"),
        executable_allowlist: managed_app_executable_allowlist()?,
        preview_sources,
    })?;
    managed_app.reconcile()?;
    daemon.set_managed_app_supervisor(
        managed_app,
        Arc::new(ServiceWorkerLaunchMaterial {
            data_directory: data_directory.to_path_buf(),
        }),
    );
    Ok(())
}

const fn service_worker_capacity() -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: PROTOCOL_WORKER_SESSION_CAPACITY,
        running_worker_sessions: 0,
        reserved_worker_sessions: 0,
        draining_worker_sessions: 0,
    }
}

fn service_worker_supervisor_config(
    server_origin: &str,
    client_node_id: &str,
    client_instance_id: &str,
) -> SupervisorConfig {
    SupervisorConfig {
        client_node_id: client_node_id.to_owned(),
        client_instance_id: client_instance_id.to_owned(),
        server_origin: server_origin.to_owned(),
        model_route: None,
        worker_binary_path: None,
        max_concurrent_worker_sessions: 0,
        stop_grace_period: Duration::from_secs(20),
    }
}

fn managed_app_executable_allowlist() -> Result<BTreeSet<String>, DeviceServiceError> {
    managed_app_executable_allowlist_from(
        std::env::var_os(MANAGED_APP_EXECUTABLE_ALLOWLIST_ENVIRONMENT).as_deref(),
    )
    .map_err(DeviceServiceError::InvalidConfig)
}

fn managed_app_executable_allowlist_from(
    custom: Option<&OsStr>,
) -> Result<BTreeSet<String>, String> {
    let mut allowlist = BTreeSet::new();
    for default in [
        "/usr/bin/node",
        "/usr/local/bin/node",
        "/opt/homebrew/bin/node",
    ] {
        if let Ok(path) = canonical_executable(Path::new(default)) {
            allowlist.insert(path);
        }
    }
    if let Some(custom) = custom {
        for path in std::env::split_paths(custom) {
            allowlist.insert(canonical_executable(&path)?);
        }
    }
    Ok(allowlist)
}

fn canonical_executable(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err(format!(
            "managed app executable must be absolute: {}",
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| format!("managed app executable is unavailable: {}", path.display()))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|_| format!("managed app executable is unavailable: {}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "managed app executable is not executable: {}",
            path.display()
        ));
    }
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "managed app executable path is not UTF-8".to_owned())
}

fn canonical_worker_session_id(value: &str) -> bool {
    value.strip_prefix("wsn_").is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

#[must_use]
pub fn device_service_status(data_directory: &Path) -> DeviceServiceStatus {
    let pid = fs::read_to_string(data_directory.join(PID_FILE))
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| process_is_alive(*pid));
    let binding = if data_directory.join("device-client.sqlite3").is_file() {
        DeviceStore::open(data_directory).ok().and_then(|store| {
            crate::remote_auth::load_remote_binding(&store)
                .ok()
                .flatten()
        })
    } else {
        None
    };
    DeviceServiceStatus {
        server_url: binding.as_ref().map(|binding| binding.server_url.clone()),
        authorization_failure: binding.and_then(|binding| binding.failure_reason),
        running: pid.is_some(),
        pid,
        restart_pending: data_directory.join(RESTART_FILE).is_file(),
    }
}

pub fn request_device_service_restart(data_directory: &Path) -> Result<(), DeviceServiceError> {
    if !device_service_status(data_directory).running {
        return Err(DeviceServiceError::NotRunning);
    }
    write_private(
        &data_directory.join(RESTART_FILE),
        format!("{}\n", stamp()).as_bytes(),
    )
}

pub fn device_service_logs(
    data_directory: &Path,
    line_limit: usize,
) -> Result<String, DeviceServiceError> {
    let path = data_directory.join(LOG_FILE);
    if !path.exists() {
        return Ok(String::new());
    }
    let contents = fs::read_to_string(path)?;
    let lines = contents.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(line_limit);
    let mut output = lines[start..].join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

struct ServiceLease {
    path: PathBuf,
    pid: u32,
}

impl ServiceLease {
    fn acquire(data_directory: &Path) -> Result<Self, DeviceServiceError> {
        let path = data_directory.join(PID_FILE);
        let pid = std::process::id();
        for attempt in 0..2 {
            match private_options().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{pid}")?;
                    file.sync_all()?;
                    return Ok(Self { path, pid });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let status = device_service_status(data_directory);
                    if let Some(active_pid) = status.pid {
                        return Err(DeviceServiceError::AlreadyRunning(active_pid));
                    }
                    if attempt == 0 {
                        fs::remove_file(&path)?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(DeviceServiceError::InvalidConfig(
            "could not acquire the service pid file".to_owned(),
        ))
    }
}

impl Drop for ServiceLease {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|value| value.trim() == self.pid.to_string());
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn exchange_endpoint(server_url: &str) -> Result<String, DeviceServiceError> {
    Ok(format!("{}{EXCHANGE_PATH}", server_origin(server_url)?))
}

fn server_origin(server_url: &str) -> Result<String, DeviceServiceError> {
    crate::remote_auth::server_origin(server_url).map_err(DeviceServiceError::Store)
}

fn prepare_data_directory(path: &Path) -> Result<(), DeviceServiceError> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn append_log(data_directory: &Path, message: &str) -> Result<(), DeviceServiceError> {
    let path = data_directory.join(LOG_FILE);
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= LOG_LIMIT_BYTES)
    {
        let old = data_directory.join(OLD_LOG_FILE);
        if old.exists() {
            fs::remove_file(&old)?;
        }
        fs::rename(&path, old)?;
    }
    let mut file = private_options().create(true).append(true).open(path)?;
    writeln!(file, "{} {message}", stamp())?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), DeviceServiceError> {
    let mut file = private_options()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

fn process_is_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

pub fn device_identity_seed(display_name: &str) -> DeviceIdentitySeed {
    let (platform, architecture) = release_target();
    DeviceIdentitySeed {
        display_name: display_name.to_owned(),
        platform: platform_label(platform).to_owned(),
        architecture: architecture_label(architecture).to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const fn release_target() -> (ClientPlatformTarget, ClientArchitecture) {
    (
        ClientPlatformTarget::Aarch64AppleDarwin,
        ClientArchitecture::Aarch64,
    )
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const fn release_target() -> (ClientPlatformTarget, ClientArchitecture) {
    (
        ClientPlatformTarget::X8664AppleDarwin,
        ClientArchitecture::X8664,
    )
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const fn release_target() -> (ClientPlatformTarget, ClientArchitecture) {
    (
        ClientPlatformTarget::Aarch64UnknownLinuxGnu,
        ClientArchitecture::Aarch64,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const fn release_target() -> (ClientPlatformTarget, ClientArchitecture) {
    (
        ClientPlatformTarget::X8664UnknownLinuxGnu,
        ClientArchitecture::X8664,
    )
}

const fn platform_label(platform: ClientPlatformTarget) -> &'static str {
    match platform {
        ClientPlatformTarget::Aarch64AppleDarwin => "aarch64-apple-darwin",
        ClientPlatformTarget::X8664AppleDarwin => "x86_64-apple-darwin",
        ClientPlatformTarget::Aarch64UnknownLinuxGnu => "aarch64-unknown-linux-gnu",
        ClientPlatformTarget::X8664UnknownLinuxGnu => "x86_64-unknown-linux-gnu",
    }
}

const fn architecture_label(architecture: ClientArchitecture) -> &'static str {
    match architecture {
        ClientArchitecture::Aarch64 => "aarch64",
        ClientArchitecture::X8664 => "x86_64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OccupancyMirrorUpdate, SpawnOutcome, SpawnRequest};

    struct StopWorkersOnDrop(SessionSupervisor);

    impl Drop for StopWorkersOnDrop {
        fn drop(&mut self) {
            for worker in self.0.worker_processes().unwrap_or_default() {
                let _ = self.0.stop(&worker.worker_session_id, false);
            }
        }
    }

    fn temporary_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "winwincode-device-service-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn service_start_creates_identity_and_releases_pid() {
        let root = temporary_directory("start");
        let _ = fs::remove_dir_all(&root);
        let config = DeviceServiceConfig {
            data_directory: root.clone(),
            server_url: "http://127.0.0.1:8443".to_owned(),
            server_display_name: "Server".to_owned(),
            device_display_name: "Device".to_owned(),
        };
        run_device_service_until(&config, || true).expect("service composes");
        assert!(!device_service_status(&root).running);
        assert!(root.join("device-client.sqlite3").is_file());
        assert!(
            device_service_logs(&root, 10)
                .expect("logs")
                .contains("service started")
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn authentication_failure_allows_signed_service_restart_and_revocation_stays_terminal() {
        use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD as BASE64};
        use ed25519_dalek::{Signature, SigningKey};
        use std::io::Read as _;
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::AtomicUsize;

        let root = temporary_directory("authentication-restart");
        let _ = fs::remove_dir_all(&root);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_url = format!("http://{address}");
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stopping);
        let exchanges = Arc::new(AtomicUsize::new(0));
        let received = Arc::clone(&exchanges);
        let response_status = Arc::new(AtomicUsize::new(401));
        let responding_status = Arc::clone(&response_status);
        let revoked = Arc::new(AtomicBool::new(false));
        let responding_revoked = Arc::clone(&revoked);
        let server = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut socket, _) = listener.accept().unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let path = head
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let size = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0_u8; size];
                socket.read_exact(&mut body).unwrap();
                if path == "/internal/v1/device/challenge" {
                    let body = br#"{"schemaVersion":"winwincode/v1","challengeId":"fixture","challenge":"fixture","expiresAt":"2099-01-01T00:00:00.000Z"}"#;
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    socket.write_all(body).unwrap();
                } else if path == EXCHANGE_PATH {
                    let headers: std::collections::BTreeMap<_, _> = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
                        .collect();
                    let key = SigningKey::from_bytes(&[7; 32]).verifying_key();
                    assert_eq!(
                        headers["x-winwincode-device-public-key"],
                        BASE64.encode(key.as_bytes())
                    );
                    let signature = Signature::from_slice(
                        &BASE64
                            .decode(headers["x-winwincode-device-signature"])
                            .unwrap(),
                    )
                    .unwrap();
                    key.verify_strict(
                        &winwincode_client_port::device_auth::signing_bytes(
                            "fixture",
                            "fixture",
                            "POST",
                            EXCHANGE_PATH,
                            &format!("cnd_{}", "A".repeat(26)),
                            &body,
                        ),
                        &signature,
                    )
                    .unwrap();
                    received.fetch_add(1, Ordering::AcqRel);
                    let accepted = if responding_status.load(Ordering::Acquire) == 200 {
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        serde_json::to_vec(&serde_json::json!({"schemaVersion":"winwincode/v1","ackSequence":request["frames"].as_array().unwrap().last().unwrap()["sequence"],"frames":[]})).unwrap()
                    } else {
                        Vec::new()
                    };
                    let body = if !accepted.is_empty() {
                        accepted.as_slice()
                    } else if responding_revoked.load(Ordering::Acquire) {
                        br#"{"schemaVersion":"winwincode/v1","code":"DEVICE_REVOKED"}"#.as_slice()
                    } else if responding_status.load(Ordering::Acquire) == 403 {
                        br#"{"schemaVersion":"winwincode/v1","code":"PERMISSION_DENIED"}"#
                            .as_slice()
                    } else {
                        b""
                    };
                    write!(
                        socket,
                        "HTTP/1.1 {} Rejected\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        responding_status.load(Ordering::Acquire),
                        body.len()
                    )
                    .unwrap();
                    socket.write_all(body).unwrap();
                } else {
                    assert_eq!(path, PREVIEW_TUNNEL_PATH);
                    socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            }
        });
        let config = DeviceServiceConfig {
            data_directory: root.clone(),
            server_url: server_url.clone(),
            server_display_name: "Server".to_owned(),
            device_display_name: "Device".to_owned(),
        };
        let mut store = DeviceStore::open(&root).unwrap();
        let identity =
            ensure_device_identity(&mut store, &device_identity_seed("Device"), &stamp()).unwrap();
        let node = format!("cnd_{}", "A".repeat(26));
        crate::adopt_enrollment(
            &mut store,
            identity.identity().device_id(),
            &crate::IssuedEnrollment {
                client_node_id: node.clone(),
                public_client_id: "123456789".to_owned(),
            },
            &stamp(),
        )
        .unwrap();
        store.connection().unwrap().execute(
            "INSERT INTO remote_device_binding (singleton,server_url,signing_key,request_id,intent_json,ca_roots_json,client_node_id,public_client_id,owner_user_id,bound_at) VALUES(1,?1,?2,'dbr_authentication_restart','{}','[]',?3,'123456789','usr_owner',?4)",
            rusqlite::params![server_url, [7_u8; 32].as_slice(), node, stamp()],
        ).unwrap();
        let mut expected_exchanges = 0;
        for (status, attempts) in [(401, 2), (403, 1)] {
            response_status.store(status, Ordering::Release);
            for restart in 0..2 {
                if restart == 1 {
                    crate::remote_auth::record_remote_failure(
                        &store,
                        if status == 401 {
                            "Device 身份认证失败；服务已停止。"
                        } else {
                            "Device 绑定已吊销；服务已停止。"
                        },
                    )
                    .unwrap();
                }
                let started = Instant::now();
                let result = run_device_service_until(&config, || {
                    started.elapsed() > Duration::from_secs(5)
                });
                assert!(
                    matches!(result, Err(DeviceServiceError::Daemon(DaemonError::Network(failure))) if failure.http_status == Some(u16::try_from(status).unwrap()))
                );
                expected_exchanges += attempts;
                assert_eq!(exchanges.load(Ordering::Acquire), expected_exchanges);
                assert!(
                    crate::remote_auth::load_remote_binding(&store)
                        .unwrap()
                        .unwrap()
                        .failure_reason
                        .is_none()
                );
            }
        }
        let binding_material = || {
            store.connection().unwrap().query_row("SELECT signing_key,request_id,intent_json FROM remote_device_binding WHERE singleton=1", [], |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?))).unwrap()
        };
        let original_material = binding_material();
        crate::remote_auth::record_remote_failure(&store, "Device 绑定已吊销；服务已停止。")
            .unwrap();
        response_status.store(200, Ordering::Release);
        let expected_before_success = expected_exchanges;
        let started = Instant::now();
        run_device_service_until(&config, || {
            exchanges.load(Ordering::Acquire) > expected_before_success
                || started.elapsed() > Duration::from_secs(5)
        })
        .expect("an active identity recovers after an old ambiguous 403 cache");
        expected_exchanges += 1;
        assert_eq!(exchanges.load(Ordering::Acquire), expected_exchanges);
        assert_eq!(binding_material(), original_material);
        assert!(
            crate::remote_auth::load_remote_binding(&store)
                .unwrap()
                .unwrap()
                .failure_reason
                .is_none()
        );
        crate::remote_auth::record_remote_failure(&store, "Device 绑定已吊销；服务已停止。")
            .unwrap();
        response_status.store(403, Ordering::Release);
        revoked.store(true, Ordering::Release);
        assert!(
            matches!(run_device_service_until(&config, || false), Err(DeviceServiceError::Daemon(DaemonError::Network(failure))) if failure.http_status == Some(403))
        );
        expected_exchanges += 1;
        assert_eq!(
            crate::remote_auth::load_remote_binding(&store)
                .unwrap()
                .unwrap()
                .failure_reason
                .as_deref(),
            Some(crate::remote_auth::DEVICE_REVOKED_REASON)
        );
        assert!(matches!(
            run_device_service_until(&config, || false),
            Err(DeviceServiceError::InvalidConfig(_))
        ));
        assert_eq!(exchanges.load(Ordering::Acquire), expected_exchanges);
        stopping.store(true, Ordering::Release);
        drop(TcpStream::connect(address).unwrap());
        server.join().unwrap();
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn service_restart_announces_before_application_and_worker_stop_recovery() {
        use winwincode_client_port::domain::{ApplyResult, ApplyStrategy, LocalApplyReceipt};
        use winwincode_client_port::messages::{ClientToServerEnvelope, ClientToServerMessage};
        let root = temporary_directory("recovery-order");
        let _ = fs::remove_dir_all(&root);
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let config = DeviceServiceConfig {
            data_directory: root.clone(),
            server_url: format!("http://{address}"),
            server_display_name: "Server".into(),
            device_display_name: "Device".into(),
        };
        let mut store = DeviceStore::open(&root).unwrap();
        let seed = device_identity_seed("Device");
        let identity = ensure_device_identity(&mut store, &seed, &stamp()).unwrap();
        crate::adopt_enrollment(
            &mut store,
            identity.identity().device_id(),
            &crate::IssuedEnrollment {
                client_node_id: format!("cnd_{}", "A".repeat(26)),
                public_client_id: "0123456789".into(),
            },
            &stamp(),
        )
        .unwrap();
        store
            .advance_occupancy_mirror(&OccupancyMirrorUpdate {
                occupancy_lease_id: "ocl_service_recovery".into(),
                fencing_token: 7,
                holder_user_id: Some("usr_service_owner".into()),
                claim_request_id: None,
                idle_expires_at: None,
                acknowledged_at: stamp(),
            })
            .unwrap();
        let identity = load_device_identity(&store).unwrap().unwrap();
        let mut daemon = DeviceDaemon::start(
            DaemonConfig {
                server_profile_id: "default".into(),
                base_url: format!("http://{address}{EXCHANGE_PATH}"),
                ..DaemonConfig::default()
            },
            store,
            Arc::new(HttpExchangeTransport::new(format!(
                "http://{address}{EXCHANGE_PATH}"
            ))),
            &identity,
        )
        .unwrap();
        daemon
            .enqueue(ClientToServerMessage::Heartbeat(
                winwincode_client_port::messages::ClientHeartbeatPayload {
                    capacity: service_worker_capacity(),
                    accepting_connections: true,
                    lock_state: winwincode_client_port::domain::ClientLockState::Unlocked,
                    presence_state: winwincode_client_port::domain::PresenceState::Online,
                    occupancy_lease_id: Some("ocl_service_recovery".into()),
                },
            ))
            .unwrap();
        let retained = daemon.outbox_snapshot().unwrap();
        let request = crate::apply_engine::CandidateApplyRequest {
            application_id: "lar_service_recovery".into(),
            approval_id: "apr_service_recovery".into(),
            candidate_digest: "sha256:service_recovery".into(),
            reconciled_retry: false,
            repository_binding_id: "rbd_service_recovery".into(),
            candidate_ref: format!("refs/winwincode/candidates/{}", "a".repeat(40)),
            target_branch: "main".into(),
            expected_head: "b".repeat(40),
            strategy: ApplyStrategy::CherryPick,
            occupancy_lease_id: "ocl_service_recovery".into(),
            occupancy_fencing_token: 7,
        };
        crate::application_journal::start_application(daemon.store_mut(), &request, &stamp())
            .unwrap();
        crate::application_journal::finish_application(
            daemon.store_mut(),
            &LocalApplyReceipt {
                local_apply_receipt_id: request.application_id.clone(),
                candidate_ref: request.candidate_ref.clone(),
                repository_binding_id: request.repository_binding_id.clone(),
                target_branch: request.target_branch.clone(),
                expected_head: request.expected_head.clone(),
                strategy: request.strategy,
                result: ApplyResult::Applied,
                resulting_commit: Some("c".repeat(40)),
                conflict_artifact_ref: None,
                created_at: stamp(),
                revision: 1,
            },
        )
        .unwrap();
        daemon
            .store_mut()
            .record_worker_stop_intent(&crate::WorkerStopIntentRecord {
                command_message_id: "service-recovery-stop".into(),
                idempotency_key: "service-recovery-stop".into(),
                worker_session_id: "wsn_service_recovery".into(),
                worker_id: "wrk_service_recovery".into(),
                reason: winwincode_client_port::domain::ClientWorkerStopReason::LeaseRecovered,
                occupancy_lease_id: request.occupancy_lease_id.clone(),
                fencing_token: 7,
                expected_revision: 1,
                receipt_payload: None,
                recorded_at: stamp(),
                completed_at: None,
            })
            .unwrap();
        daemon.into_store().close().unwrap();
        let mut polls = 0;
        run_device_service_until(&config, || {
            polls += 1;
            polls > 1
        })
        .unwrap();
        let mut store = DeviceStore::open(&root).unwrap();
        let restarted = load_device_identity(&store).unwrap().unwrap();
        assert_ne!(
            restarted.current_instance_id(),
            identity.current_instance_id()
        );
        store
            .bind_outbox_stream(
                restarted.identity().client_node_id(),
                restarted.current_instance_id(),
            )
            .unwrap();
        let queued = winwincode_client_port::exchange::FrameOutbox::load(&mut store)
            .unwrap()
            .unwrap();
        assert_eq!(&queued.frames[..retained.frames.len()], &retained.frames);
        let new_frames = queued.frames[retained.frames.len()..]
            .iter()
            .map(|frame| serde_json::from_slice::<ClientToServerEnvelope>(&frame.frame).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            new_frames[0].message,
            ClientToServerMessage::Hello(_)
        ));
        assert!(
            new_frames
                .iter()
                .all(|frame| frame.client_instance_id == restarted.current_instance_id())
        );
        assert!(new_frames.iter().any(|frame| matches!(&frame.message, ClientToServerMessage::ApplicationReconciled(payload) if payload.application_id == request.application_id)));
        assert!(new_frames.iter().any(|frame| matches!(&frame.message, ClientToServerMessage::CommandAck(payload) if payload.command_message_id == "service-recovery-stop")));
        store.close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_worker_configuration_accepts_a_fifth_live_session() {
        let root = temporary_directory("fifth-worker");
        let _ = fs::remove_dir_all(&root);
        prepare_data_directory(&root).expect("data directory");
        let binary = root.join("worker-bin");
        fs::write(&binary, "#!/bin/sh\nexec sleep 60\n").expect("worker fixture");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("worker executable");
        let mut store = DeviceStore::open(&root).expect("device store");
        let lease_id = "ocl_SERVICE_WORKERS";
        store
            .advance_occupancy_mirror(&OccupancyMirrorUpdate {
                occupancy_lease_id: lease_id.to_owned(),
                fencing_token: 7,
                holder_user_id: Some("usr_SERVICE_HOLDER".to_owned()),
                claim_request_id: Some("ocq_SERVICE_CLAIM".to_owned()),
                idle_expires_at: None,
                acknowledged_at: "2026-01-01T00:00:00.000Z".to_owned(),
            })
            .expect("authorized occupancy");
        let mut config = service_worker_supervisor_config(
            "http://127.0.0.1:8443",
            "cnd_SERVICE_DEVICE",
            "cix_SERVICE_INSTANCE",
        );
        assert_eq!(
            service_worker_capacity().max_concurrent_worker_sessions,
            1024
        );
        assert_eq!(config.max_concurrent_worker_sessions, 0);
        config.worker_binary_path = Some(binary);
        let supervisor = SessionSupervisor::new(config, store).expect("production supervisor");
        let workers = StopWorkersOnDrop(supervisor.clone());
        let source = root.join("source");
        fs::create_dir_all(&source).expect("source directory");
        for index in 0..5 {
            let session_id = format!("wsn_SERVICE_{index}");
            let worker_id = format!("wrk_SERVICE_{index}");
            let instance_id = format!("wki_SERVICE_{index}");
            let data_directory = root.join(format!("worker-data-{index}"));
            let worker_root = root.join(format!("worker-root-{index}"));
            let outcome = supervisor
                .spawn(SpawnRequest {
                    worker_session_id: &session_id,
                    worker_id: &worker_id,
                    worker_instance_id: &instance_id,
                    occupancy_lease_id: lease_id,
                    occupancy_fencing_token: 7,
                    repository_binding_id: "rbd_SERVICE_SOURCE",
                    launch_grant_id: "wlg_SERVICE_LAUNCH",
                    product_session_id: None,
                    work_run_id: None,
                    source_directory: &source,
                    data_directory: &data_directory,
                    worker_root: &worker_root,
                })
                .expect("every authorized session starts, including the fifth");
            assert!(matches!(outcome, SpawnOutcome::Started(_)));
        }
        assert_eq!(
            supervisor.running_worker_sessions().expect("live workers"),
            5
        );
        assert_eq!(supervisor.worker_capacity().running_worker_sessions, 5);
        drop(workers);
        assert_eq!(
            supervisor
                .running_worker_sessions()
                .expect("stopped workers"),
            0
        );
        drop(supervisor);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn status_restart_and_logs_use_private_bounded_control_files() {
        let root = temporary_directory("control");
        let _ = fs::remove_dir_all(&root);
        prepare_data_directory(&root).expect("data directory");
        write_private(
            &root.join(PID_FILE),
            format!("{}\n", std::process::id()).as_bytes(),
        )
        .expect("pid");
        append_log(&root, "first").expect("first log");
        append_log(&root, "second").expect("second log");

        let status = device_service_status(&root);
        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        request_device_service_restart(&root).expect("restart request");
        assert!(device_service_status(&root).restart_pending);
        assert!(
            device_service_logs(&root, 1)
                .expect("tail")
                .ends_with(" second\n")
        );
        assert_eq!(
            root.join(RESTART_FILE)
                .metadata()
                .expect("restart metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn service_accepts_https_and_canonical_exchange_path() {
        assert!(exchange_endpoint("https://server.example:8080").is_ok());
        assert!(exchange_endpoint("http://server.example:8080").is_err());
        assert_eq!(
            exchange_endpoint("http://127.0.0.1:8443").expect("origin"),
            "http://127.0.0.1:8443/internal/v1/client/exchange"
        );
        assert!(exchange_endpoint("http://127.0.0.1:8443/other").is_err());
    }

    #[test]
    fn managed_app_allowlist_canonicalizes_device_paths_and_rejects_relative() {
        let executable = std::env::current_exe().expect("test executable");
        let canonical = executable.canonicalize().expect("canonical executable");
        let executable_text = executable.to_str().expect("test executable path");
        let allowlist = managed_app_executable_allowlist_from(Some(OsStr::new(executable_text)))
            .expect("absolute executable");
        assert!(allowlist.contains(canonical.to_str().expect("canonical path")));
        let error = managed_app_executable_allowlist_from(Some(OsStr::new("node")))
            .expect_err("relative executable must be rejected");
        assert!(error.contains("must be absolute"));
    }
}
