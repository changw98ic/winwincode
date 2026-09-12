// SPDX-License-Identifier: Apache-2.0

//! Foreground Device Client service and its local CLI control files.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
};

use crate::{
    DaemonConfig, DaemonError, DeviceDaemon, DeviceIdentitySeed, DeviceStore, DeviceStoreError,
    HttpExchangeTransport, LeaseWorkerController, SessionSupervisor, SupervisorConfig,
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
const TLS_ROOT_DER_ENVIRONMENT: &str = "WWC_DEVICE_TLS_ROOT_DER_FILE";

/// Runtime configuration supplied by the native service manager.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceServiceConfig {
    pub data_directory: PathBuf,
    pub server_url: String,
    pub server_display_name: String,
    pub device_display_name: String,
}

/// Secret-free service process status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceServiceStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub restart_pending: bool,
}

/// Device service startup or local-control failure.
#[derive(Debug)]
pub enum DeviceServiceError {
    InvalidConfig(String),
    AlreadyRunning(u32),
    NotRunning,
    Io(std::io::Error),
    Store(DeviceStoreError),
    Daemon(DaemonError),
    Supervisor(SupervisorError),
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

struct ServiceWorkerLaunchMaterial {
    data_directory: PathBuf,
}

impl WorkerLaunchMaterialSource for ServiceWorkerLaunchMaterial {
    fn worker_credential(&self, _credential_digest: &str) -> Option<String> {
        None
    }

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

/// Runs the Device Client in the foreground until its native service manager
/// stops the process. A local restart request rotates only the daemon session;
/// durable outbox and occupancy state stay in the same SQLite store.
///
/// # Errors
///
/// Fails on invalid HTTPS configuration, a second live service process, or a
/// fatal local store/daemon error.
pub fn run_device_service(config: &DeviceServiceConfig) -> Result<(), DeviceServiceError> {
    run_device_service_until(config, || false)
}

fn run_device_service_until(
    config: &DeviceServiceConfig,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), DeviceServiceError> {
    let endpoint = exchange_endpoint(&config.server_url)?;
    let origin = server_origin(&config.server_url)?;
    prepare_data_directory(&config.data_directory)?;
    let _lease = ServiceLease::acquire(&config.data_directory)?;
    let restart_path = config.data_directory.join(RESTART_FILE);
    if restart_path.exists() {
        fs::remove_file(&restart_path)?;
    }
    append_log(&config.data_directory, "service started")?;

    loop {
        let mut store = DeviceStore::open(&config.data_directory)?;
        let identity = ensure_device_identity(
            &mut store,
            &device_identity_seed(&config.device_display_name),
            &stamp(),
        )?;
        let transport = match std::env::var_os(TLS_ROOT_DER_ENVIRONMENT) {
            Some(path) => {
                HttpExchangeTransport::new(endpoint.clone()).with_tls_root_der(fs::read(path)?)
            }
            None => HttpExchangeTransport::new(endpoint.clone()),
        };
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
                capacity: ClientCapacityReport {
                    max_concurrent_worker_sessions: 1,
                    running_worker_sessions: 0,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
            },
            store,
            Arc::new(transport),
            &identity,
        )?;
        let mut worker_lane_wired = false;
        if identity.identity().is_enrolled() {
            wire_worker_lane(
                &mut daemon,
                &config.data_directory,
                &origin,
                identity.identity().client_node_id(),
                identity.current_instance_id(),
            )?;
            worker_lane_wired = true;
        }
        let mut last_retry = None;
        loop {
            if should_stop() {
                append_log(&config.data_directory, "service stopped")?;
                return Ok(());
            }
            if restart_path.exists() {
                fs::remove_file(&restart_path)?;
                append_log(&config.data_directory, "daemon session restarted")?;
                break;
            }
            let ready_in = match daemon.tick(Instant::now())? {
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
                )?;
                worker_lane_wired = true;
                append_log(&config.data_directory, "local worker lane ready")?;
            }
            thread::sleep(ready_in.min(CONTROL_POLL_INTERVAL));
        }
    }
}

fn wire_worker_lane(
    daemon: &mut DeviceDaemon,
    data_directory: &Path,
    server_origin: &str,
    client_node_id: &str,
    client_instance_id: &str,
) -> Result<(), DeviceServiceError> {
    let supervisor = SessionSupervisor::new(
        SupervisorConfig {
            client_node_id: client_node_id.to_owned(),
            client_instance_id: client_instance_id.to_owned(),
            server_origin: server_origin.to_owned(),
            model_route: None,
            worker_binary_path: None,
            max_concurrent_worker_sessions: 1,
            stop_grace_period: Duration::from_secs(10),
        },
        DeviceStore::open(data_directory)?,
    )?;
    daemon.set_worker_supervisor(supervisor.clone());
    daemon
        .set_worker_capacity_source(Arc::new(supervisor.clone()) as Arc<dyn WorkerCapacitySource>);
    daemon.set_lease_worker_controller(Arc::new(supervisor) as Arc<dyn LeaseWorkerController>);
    daemon.set_worker_launch_material_source(Arc::new(ServiceWorkerLaunchMaterial {
        data_directory: data_directory.to_path_buf(),
    }));
    Ok(())
}

fn canonical_worker_session_id(value: &str) -> bool {
    value.strip_prefix("ws_").is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

/// Reads the live process status from the private pid file.
#[must_use]
pub fn device_service_status(data_directory: &Path) -> DeviceServiceStatus {
    let pid = fs::read_to_string(data_directory.join(PID_FILE))
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| process_is_alive(*pid));
    DeviceServiceStatus {
        running: pid.is_some(),
        pid,
        restart_pending: data_directory.join(RESTART_FILE).is_file(),
    }
}

/// Requests an in-process daemon-session restart from the running service.
///
/// # Errors
///
/// Returns [`DeviceServiceError::NotRunning`] if no live pid owns the data
/// directory, and an I/O error if the private request file cannot be written.
pub fn request_device_service_restart(data_directory: &Path) -> Result<(), DeviceServiceError> {
    if !device_service_status(data_directory).running {
        return Err(DeviceServiceError::NotRunning);
    }
    write_private(
        &data_directory.join(RESTART_FILE),
        format!("{}\n", stamp()).as_bytes(),
    )
}

/// Returns the last `line_limit` bounded, redacted service log lines.
///
/// # Errors
///
/// Returns an I/O error when an existing log cannot be read.
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
    let uri = http::Uri::from_str(server_url).map_err(|_| {
        DeviceServiceError::InvalidConfig("server URL is not a valid URI".to_owned())
    })?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        return Err(DeviceServiceError::InvalidConfig(
            "server URL must be an absolute https:// URL".to_owned(),
        ));
    }
    match uri.path() {
        "" | "/" => Ok(format!(
            "https://{}{EXCHANGE_PATH}",
            uri.authority().expect("authority checked")
        )),
        EXCHANGE_PATH if uri.query().is_none() => Ok(server_url.to_owned()),
        _ => Err(DeviceServiceError::InvalidConfig(format!(
            "server URL path must be / or {EXCHANGE_PATH}"
        ))),
    }
}

fn server_origin(server_url: &str) -> Result<String, DeviceServiceError> {
    let uri = http::Uri::from_str(server_url).map_err(|_| {
        DeviceServiceError::InvalidConfig("server URL is not a valid URI".to_owned())
    })?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        return Err(DeviceServiceError::InvalidConfig(
            "server URL must be an absolute https:// URL".to_owned(),
        ));
    }
    Ok(format!(
        "https://{}",
        uri.authority().expect("authority checked")
    ))
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
    Command::new("kill")
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

fn device_identity_seed(display_name: &str) -> DeviceIdentitySeed {
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
            server_url: "https://server.example:8443".to_owned(),
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
    fn service_requires_https_and_canonical_exchange_path() {
        assert!(exchange_endpoint("http://server.example:8080").is_err());
        assert_eq!(
            exchange_endpoint("https://server.example:8443").expect("origin"),
            "https://server.example:8443/internal/v1/client/exchange"
        );
        assert!(exchange_endpoint("https://server.example:8443/other").is_err());
    }
}
