// SPDX-License-Identifier: Apache-2.0
//! Offline mechanism regression fixtures. Execution belongs to regression_runner.
use super::*;
use crate::identity::{DeviceIdentitySeed, ensure_device_identity};
use crate::store::WorkerProcessRecord;
use crate::supervisor::{SpawnRequest, SupervisorConfig};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use winwincode_client_port::domain::ClientWorkerStopReason;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
const STAMP: &str = "2026-10-10T00:00:00.000Z";
const LEASE: &str = "ocl_00000000000000000000000001";
const WORKER: &str = "wrk_00000000000000000000000001";
fn write_receipt(name: &str, value: &serde_json::Value) {
    let output = std::env::var_os("WWC_MECHANISM_AUDIT_OUTPUT")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                std::env::temp_dir()
                    .join(format!("wwc-mechanism-audit-output-{}", std::process::id()))
            },
            PathBuf::from,
        );
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    println!("{}", serde_json::to_string(value).unwrap());
}

fn root(name: &str) -> PathBuf {
    let result = std::env::temp_dir().join(format!(
        "wwc-mechanism-{name}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&result).unwrap();
    result
}

fn mirrored_store(path: &Path) -> DeviceStore {
    let mut store = DeviceStore::open(path).unwrap();
    store
        .advance_occupancy_mirror(&OccupancyMirrorUpdate {
            occupancy_lease_id: LEASE.to_owned(),
            fencing_token: 1,
            holder_user_id: Some("usr_00000000000000000000000001".to_owned()),
            claim_request_id: Some("ocq_00000000000000000000000001".to_owned()),
            idle_expires_at: None,
            acknowledged_at: STAMP.to_owned(),
        })
        .unwrap();
    store
}

struct LocalTransport {
    frames: Mutex<Vec<serde_json::Value>>,
    requests: Mutex<Vec<serde_json::Value>>,
}

impl LocalTransport {
    fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl ExchangeTransport for LocalTransport {
    fn exchange(
        &self,
        _credential: Option<&str>,
        request: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        let value: serde_json::Value = serde_json::from_slice(request).unwrap();
        let ack = value["frames"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|frame| frame["sequence"].as_u64())
            .max()
            .unwrap_or(0);
        self.requests.lock().unwrap().push(value);
        Ok(serde_json::to_vec(&ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence: ack,
            replay_from_sequence: None,
            frames: std::mem::take(&mut *self.frames.lock().unwrap()),
            enrollment: None,
            worker_credentials: Vec::new(),
        })
        .unwrap())
    }
}

fn daemon(path: &Path, transport: Arc<LocalTransport>) -> DeviceDaemon {
    let mut store = mirrored_store(path);
    let identity = ensure_device_identity(
        &mut store,
        &DeviceIdentitySeed {
            display_name: "offline mechanism fixture".to_owned(),
            platform: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            client_version: "0.1.0-alpha.1".to_owned(),
        },
        STAMP,
    )
    .unwrap();
    let mut daemon = DeviceDaemon::start(
        DaemonConfig {
            server_profile_id: "offline-mechanism".to_owned(),
            base_url: "https://offline.example.test/internal/v1/client/exchange".to_owned(),
            server_display_name: "offline fixture".to_owned(),
            device_display_name: "offline fixture".to_owned(),
            platform: ClientPlatformTarget::Aarch64AppleDarwin,
            architecture: ClientArchitecture::Aarch64,
            client_version: "0.1.0-alpha.1".to_owned(),
            heartbeat_interval: Duration::from_secs(5),
            enroll_poll_interval: Duration::from_millis(1),
            max_frames_per_exchange: 16,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(16),
            capacity: ClientCapacityReport {
                max_concurrent_worker_sessions: 8,
                running_worker_sessions: 0,
                reserved_worker_sessions: 0,
                draining_worker_sessions: 0,
            },
        },
        store,
        transport,
        &identity,
    )
    .unwrap();
    // Test-only boot phase injection. Registry, supervision, frames, fencing,
    // outbox, ingest and tick methods below are the actual production methods.
    daemon.enrolled = true;
    daemon.hello_announced = true;
    daemon.node_id = "cnd_00000000000000000000000001".to_owned();
    daemon
        .store
        .bind_outbox_stream(&daemon.node_id, &daemon.instance_id)
        .unwrap();
    daemon
}

fn supervisor(path: &Path, binary: Option<PathBuf>) -> SessionSupervisor {
    SessionSupervisor::new(
        SupervisorConfig {
            client_node_id: "cnd_00000000000000000000000001".to_owned(),
            client_instance_id: "cix_00000000000000000000000001".to_owned(),
            server_origin: "https://127.0.0.1:1".to_owned(),
            model_route: None,
            worker_binary_path: binary,
            max_concurrent_worker_sessions: 8,
            ..SupervisorConfig::default()
        },
        mirrored_store(path),
    )
    .unwrap()
}

#[test]
#[ignore = "mechanism audit: full-history cost baseline retains the incremental target assertion"]
fn m20_terminal_history_repeats_in_actual_reconciliation() {
    let mut measurements = Vec::new();
    for history in [10_usize, 100, 1000] {
        let root = root("registry");
        let device = root.join("device");
        let mut store = mirrored_store(&device);
        for index in 0..history {
            store
                .put_worker_process(&WorkerProcessRecord {
                    worker_session_id: format!("wsn_{index:026}"),
                    worker_id: WORKER.to_owned(),
                    worker_instance_id: format!("wki_{index:026}"),
                    pid: 999_999,
                    process_start_identity: "terminal history fixture".to_owned(),
                    repository_binding_id: "rbn_fixture".to_owned(),
                    occupancy_lease_id: LEASE.to_owned(),
                    launch_grant_id: format!("wlg_{index:026}"),
                    data_directory: root.display().to_string(),
                    state: "exited".to_owned(),
                    exit_code: Some(0),
                    last_observed_at: STAMP.to_owned(),
                })
                .unwrap();
        }
        drop(store);
        let transport = Arc::new(LocalTransport::new());
        let mut daemon = daemon(&device, transport);
        let supervisor = supervisor(&device, None);
        daemon.set_worker_supervisor(supervisor.clone());
        daemon.set_worker_capacity_source(Arc::new(supervisor.clone()));
        // Mark only exit notifications as previously sent. Reconcile history
        // has no equivalent cursor, so every heartbeat includes it again.
        daemon.reported_worker_exits = supervisor
            .worker_processes()
            .unwrap()
            .iter()
            .map(|row| row.worker_instance_id.clone())
            .collect();
        let first = Instant::now();
        daemon.next_heartbeat_at = first;
        let start = Instant::now();
        daemon.ensure_pending_reports(first).unwrap();
        daemon
            .ensure_pending_reports(first + Duration::from_secs(6))
            .unwrap();
        let elapsed = start.elapsed();
        let reports = daemon
            .store
            .pending_outbox_envelopes()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "client.worker.reconcile")
            .map(|entry| serde_json::from_slice::<serde_json::Value>(&entry.payload).unwrap())
            .collect::<Vec<_>>();
        let counts = reports
            .iter()
            .map(|value| value["payload"]["workers"].as_array().unwrap().len())
            .collect::<Vec<_>>();
        assert_eq!(counts, vec![history, history]);
        measurements.push(serde_json::json!({ "history": history, "heartbeatCount": 2,
            "reconciliationWorkerCounts": counts, "elapsedMicros": elapsed.as_micros(),
            "totalReconciliationBytes": reports.iter().map(|value| serde_json::to_vec(value).unwrap().len()).sum::<usize>() }));
        drop(daemon);
        drop(supervisor);
        fs::remove_dir_all(root).unwrap();
    }
    write_receipt(
        "m20-device-registry-receipt.json",
        &serde_json::json!({ "id": "M20",
        "actualMethods": ["DeviceStore.put_worker_process", "SessionSupervisor.reconcile",
            "SessionSupervisor.worker_capacity", "DeviceDaemon.ensure_pending_reports"],
        "measurements": measurements, "classification": "confirmed historical work growth; formal lease causality unproved" }),
    );
    // A desired incremental reconciliation must not resend unchanged terminal
    // history in its second heartbeat. This intentional regression is red.
    assert_eq!(
        measurements[0]["reconciliationWorkerCounts"][1], 0,
        "unchanged terminal registry history is repeated in the next heartbeat"
    );
}

fn kill_fixture_group(pid: u32, script: &Path) {
    // Verify the unique owned script before signalling. Never signal by PID alone.
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    if output.is_ok_and(|value| {
        String::from_utf8_lossy(&value.stdout).contains(script.to_str().unwrap())
    }) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
    }
}

struct OwnedCleanup {
    supervisor: SessionSupervisor,
    sessions: Vec<String>,
    pids: Vec<u32>,
    script: PathBuf,
}
impl Drop for OwnedCleanup {
    fn drop(&mut self) {
        for pid in &self.pids {
            kill_fixture_group(*pid, &self.script);
        }
        for session in &self.sessions {
            let _ = self.supervisor.stop(session, false);
        }
    }
}

#[test]
#[ignore = "mechanism audit: five owned TERM-ignoring children retain the 45-second tick budget assertion"]
#[allow(
    clippy::too_many_lines,
    reason = "the audit keeps five owned children, bounded cleanup, and tick budget assertions in one lifecycle"
)]
fn sc05_five_owned_term_ignoring_workers_block_actual_device_tick() {
    let root = root("stop");
    let device = root.join("device");
    let script = root.join("owned-worker.py");
    fs::write(&script, "#!/usr/bin/python3\nimport signal,sys,pathlib,time\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nassert sys.stdin.buffer.read(1)==b'\\x01'\npathlib.Path(sys.argv[2]).with_name('worker-ready').write_text('ready')\nwhile True: time.sleep(0.1)\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let supervisor = supervisor(&device, Some(script.clone()));
    assert_eq!(
        supervisor.config().stop_grace_period,
        Duration::from_secs(10)
    );
    let mut cleanup = OwnedCleanup {
        supervisor: supervisor.clone(),
        sessions: Vec::new(),
        pids: Vec::new(),
        script: script.clone(),
    };
    let transport = Arc::new(LocalTransport::new());
    let mut daemon = daemon(&device, transport.clone());
    daemon.set_worker_supervisor(supervisor.clone());
    for index in 1..=5 {
        let session = format!("wsn_{index:026}");
        let instance = format!("wki_{index:026}");
        let source = root.join(format!("source-{index}"));
        let data = root.join(format!("data-{index}"));
        let worker_root = root.join(format!("worker-{index}"));
        fs::create_dir_all(&source).unwrap();
        supervisor
            .spawn(SpawnRequest {
                worker_session_id: &session,
                worker_id: WORKER,
                worker_instance_id: &instance,
                occupancy_lease_id: LEASE,
                occupancy_fencing_token: 1,
                worker_credential_token: "offline-fixture-credential",
                repository_binding_id: "rbn_fixture",
                launch_grant_id: "wlg_fixture",
                product_session_id: None,
                work_run_id: None,
                source_directory: &source,
                data_directory: &data,
                worker_root: &worker_root,
            })
            .unwrap();
        cleanup
            .pids
            .push(supervisor.worker_process(&session).unwrap().unwrap().pid);
        cleanup.sessions.push(session.clone());
        let envelope = ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("msg_{index:026}"),
            client_node_id: daemon.node_id.clone(),
            client_instance_id: daemon.instance_id.clone(),
            sequence: index,
            occurred_at: STAMP.to_owned(),
            message: ServerToClientMessage::WorkerStop(ServerWorkerStopPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: daemon
                            .occupancy_mirror
                            .as_ref()
                            .unwrap()
                            .mirror_revision,
                        idempotency_key: format!("offline-stop-{index}"),
                    },
                    occupancy_lease_id: LEASE.to_owned(),
                    occupancy_fencing_token: 1,
                },
                worker_session_id: session,
                worker_id: WORKER.to_owned(),
                reason: ClientWorkerStopReason::OccupantRequested,
            }),
        };
        transport
            .frames
            .lock()
            .unwrap()
            .push(serde_json::to_value(envelope).unwrap());
    }
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    while !(1..=5).all(|index| root.join(format!("data-{index}/worker-ready")).exists()) {
        assert!(
            Instant::now() < ready_deadline,
            "owned workers did not become ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // A bounded watchdog cleans only the five verified fixture groups if a
    // production stop/join unexpectedly hangs. No running batch is touched.
    let owned_pids = cleanup.pids.clone();
    let owned_script = script.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let watchdog = thread::spawn(move || {
        if finished_rx.recv_timeout(Duration::from_secs(65)).is_err() {
            for pid in owned_pids {
                kill_fixture_group(pid, &owned_script);
            }
        }
    });
    let start = Instant::now();
    daemon.next_heartbeat_at = start;
    let outcome = daemon.tick(start);
    let elapsed = start.elapsed();
    let _ = finished_tx.send(());
    watchdog.join().unwrap();
    let heartbeats_before_next_tick = daemon.status.heartbeats_enqueued;
    let next = daemon.tick(Instant::now());
    let states = cleanup
        .sessions
        .iter()
        .map(|session| supervisor.worker_process(session).unwrap().unwrap().state)
        .collect::<Vec<_>>();
    let value = serde_json::json!({ "id": "SC-05", "workerCount": 5, "stopGraceMillis": 10000,
        "tickElapsedMillis": elapsed.as_millis(), "firstTick": format!("{outcome:?}"),
        "nextTick": format!("{next:?}"), "heartbeatsBeforeNextTick": heartbeats_before_next_tick,
        "heartbeatsAfterNextTick": daemon.status.heartbeats_enqueued,
        "states": states, "presenceStaleThresholdMillis": 45000,
        "thresholdInterpretation": "actual tick interval compared with server source 45s threshold; server sweep evaluator not invoked",
        "actualMethods": ["SessionSupervisor.spawn", "DeviceDaemon.tick", "DeviceDaemon.ingest_downlink", "DeviceDaemon.apply_worker_stop", "SessionSupervisor.stop"],
        "productionProcessesSignalled": 0, "cleanupBoundSeconds": 65 });
    drop(cleanup);
    drop(daemon);
    drop(supervisor);
    fs::remove_dir_all(root).unwrap();
    write_receipt("sc05-device-stop-receipt.json", &value);
    assert!(
        outcome.is_ok(),
        "actual downlink tick must complete with valid fixture commands"
    );
    assert!(
        states.iter().all(|state| state != "running"),
        "every owned fixture worker must be terminal before assertion"
    );
    assert!(
        elapsed < Duration::from_secs(45),
        "five synchronous 10s stops delayed the next heartbeat beyond the server 45s stale threshold"
    );
}
