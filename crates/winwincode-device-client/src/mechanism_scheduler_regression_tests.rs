// SPDX-License-Identifier: Apache-2.0
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
    request_times: Mutex<Vec<Instant>>,
}

impl LocalTransport {
    fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            request_times: Mutex::new(Vec::new()),
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
        let client_ack = value["ackSequence"].as_u64().unwrap();
        self.requests.lock().unwrap().push(value);
        self.request_times.lock().unwrap().push(Instant::now());
        let frames = self
            .frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["sequence"].as_u64().unwrap() > client_ack)
            .cloned()
            .collect();
        Ok(serde_json::to_vec(&ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence: ack,
            replay_from_sequence: None,
            frames,
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
#[ignore = "extended mechanism regression: current incremental reconciliation against terminal history"]
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
        assert_eq!(counts, vec![history, 0]);
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
        "measurements": measurements, "classification": "current incremental terminal history contract; formal lease causality unproved" }),
    );
    assert_eq!(
        measurements[0]["reconciliationWorkerCounts"][1], 0,
        "unchanged terminal registry history is repeated in the next heartbeat"
    );
}

fn kill_fixture_group(pid: u32, script: &Path) {
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
    reason = "five owned children, bounded cleanup, transmitted ACKs and per-tick budget share one lifecycle"
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
    let mut ticks = Vec::new();
    for _ in 0..8 {
        let began = Instant::now();
        let outcome = daemon.tick(began);
        let elapsed = began.elapsed();
        let cursor = daemon.downlink.ack_sequence();
        let durable_cursor = daemon
            .store
            .inbox_cursor(&daemon.config.server_profile_id)
            .unwrap()
            .unwrap()
            .last_sequence;
        let suffix_still_running = ((cursor + 1)..=5).all(|index| {
            let session = format!("wsn_{index:026}");
            supervisor.worker_process(&session).unwrap().unwrap().state == "running"
                && daemon
                    .store
                    .worker_stop_intent(&format!("msg_{index:026}"))
                    .unwrap()
                    .is_none()
        });
        for index in 1..=cursor {
            let session = format!("wsn_{index:026}");
            assert_ne!(
                supervisor.worker_process(&session).unwrap().unwrap().state,
                "running"
            );
            let intent = daemon
                .store
                .worker_stop_intent(&format!("msg_{index:026}"))
                .unwrap()
                .unwrap();
            assert!(
                intent.completed_at.is_some() && intent.receipt_payload.is_some(),
                "a downlink ACK must follow the complete durable stop receipt"
            );
        }
        ticks.push(serde_json::json!({ "elapsedMillis": elapsed.as_millis(),
            "outcome": format!("{outcome:?}"), "successful": outcome.is_ok(),
            "acceptedThrough": cursor, "durableCursor": durable_cursor,
            "unacceptedSuffixStillRunning": suffix_still_running,
            "heartbeatsEnqueued": daemon.status.heartbeats_enqueued }));
        if cursor == 5 {
            break;
        }
    }
    let began = Instant::now();
    let final_flush = daemon.tick(began);
    ticks.push(serde_json::json!({ "elapsedMillis": began.elapsed().as_millis(),
        "outcome": format!("{final_flush:?}"), "successful": final_flush.is_ok(),
        "acceptedThrough": daemon.downlink.ack_sequence(),
        "durableCursor": daemon.store.inbox_cursor(&daemon.config.server_profile_id).unwrap().unwrap().last_sequence,
        "unacceptedSuffixStillRunning": true,
        "heartbeatsEnqueued": daemon.status.heartbeats_enqueued }));
    let total_elapsed = start.elapsed();
    let _ = finished_tx.send(());
    watchdog.join().unwrap();
    let states = cleanup
        .sessions
        .iter()
        .map(|session| supervisor.worker_process(session).unwrap().unwrap().state)
        .collect::<Vec<_>>();
    let requests = transport.requests.lock().unwrap();
    let request_times = transport.request_times.lock().unwrap();
    let mut sent_stop_acks = Vec::new();
    let mut heartbeat_times = Vec::new();
    for (value, time) in requests.iter().zip(request_times.iter()) {
        let request: ExchangeRequest = serde_json::from_value(value.clone()).unwrap();
        let mut heartbeat = false;
        for frame in request.frames {
            let envelope: ClientToServerEnvelope = serde_json::from_value(frame).unwrap();
            match envelope.message {
                ClientToServerMessage::CommandAck(ack)
                    if ack.command_kind == ClientControlMessageKind::WorkerStop =>
                {
                    assert_eq!(ack.status, CommandAckStatus::Accepted);
                    assert!(ack.error.is_none());
                    sent_stop_acks.push(ack.command_message_id);
                }
                ClientToServerMessage::Heartbeat(_) => heartbeat = true,
                _ => {}
            }
        }
        if heartbeat {
            heartbeat_times.push(*time);
        }
    }
    let max_heartbeat_gap = heartbeat_times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .max()
        .unwrap_or_default();
    let sent_downlink_acks = requests
        .iter()
        .map(|value| value["ackSequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    let final_cursor = daemon.downlink.ack_sequence();
    let max_tick_millis = ticks
        .iter()
        .map(|tick| tick["elapsedMillis"].as_u64().unwrap())
        .max()
        .unwrap();
    let value = serde_json::json!({ "id": "SC-05", "workerCount": 5, "stopGraceMillis": 10000,
        "tickElapsedMillis": max_tick_millis, "ticks": ticks,
        "totalElapsedMillis": total_elapsed.as_millis(), "sentStopAckMessageIds": sent_stop_acks,
        "sentDownlinkAcks": sent_downlink_acks, "finalDownlinkCursor": final_cursor,
        "heartbeatsSent": heartbeat_times.len(), "maxHeartbeatGapMillis": max_heartbeat_gap.as_millis(),
        "states": states, "presenceStaleThresholdMillis": 45000,
        "thresholdInterpretation": "actual per-tick and transmitted heartbeat intervals compared with server source45s threshold; server sweep evaluator not invoked",
        "actualMethods": ["SessionSupervisor.spawn", "DeviceDaemon.tick", "DeviceDaemon.ingest_downlink", "DeviceDaemon.apply_worker_stop", "SessionSupervisor.stop"],
        "productionProcessesSignalled": 0, "cleanupBoundSeconds": 65 });
    drop(request_times);
    drop(requests);
    drop(cleanup);
    drop(daemon);
    drop(supervisor);
    fs::remove_dir_all(root).unwrap();
    write_receipt("sc05-device-stop-receipt.json", &value);
    assert!(
        ticks.iter().all(|tick| tick["successful"] == true),
        "every actual downlink tick must complete with valid fixture commands"
    );
    assert!(
        states.iter().all(|state| state != "running"),
        "every owned fixture worker must be terminal before assertion"
    );
    assert_eq!(final_cursor, 5);
    assert_eq!(sent_downlink_acks.last(), Some(&5));
    assert!(sent_downlink_acks.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(
        sent_stop_acks
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        (1..=5)
            .map(|index| format!("msg_{index:026}"))
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        value["sentStopAckMessageIds"].as_array().unwrap().len(),
        5,
        "every exact stop receipt must be sent once through a later exchange"
    );
    assert!(
        ticks
            .iter()
            .all(|tick| tick["elapsedMillis"].as_u64().unwrap() < 45000),
        "five synchronous 10s stops must not accumulate beyond 45s in one tick"
    );
    assert!(heartbeat_times.len() >= 2);
    assert!(
        max_heartbeat_gap < Duration::from_secs(45),
        "serial Worker stops must not delay transmitted heartbeats beyond 45s"
    );
    assert_eq!(ticks[0]["acceptedThrough"], 1);
    assert!(
        ticks
            .iter()
            .all(|tick| tick["durableCursor"] == tick["acceptedThrough"])
    );
    assert!(
        ticks
            .iter()
            .all(|tick| tick["unacceptedSuffixStillRunning"] == true)
    );
}

#[test]
fn incremental_terminal_history_is_published_once_in_actual_reconciliation() {
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
        assert_eq!(counts, vec![history, 0]);
        measurements.push(serde_json::json!({ "history": history, "heartbeatCount": 2,
            "reconciliationWorkerCounts": counts, "elapsedMicros": elapsed.as_micros(),
            "totalReconciliationBytes": reports.iter().map(|value| serde_json::to_vec(value).unwrap().len()).sum::<usize>() }));
        drop(daemon);
        drop(supervisor);
        fs::remove_dir_all(root).unwrap();
    }
    write_receipt(
        "m20-device-registry-incremental-receipt.json",
        &serde_json::json!({ "id": "M20",
        "actualMethods": ["DeviceStore.put_worker_process", "SessionSupervisor.reconcile",
            "SessionSupervisor.worker_capacity", "DeviceDaemon.ensure_pending_reports"],
        "measurements": measurements, "classification": "current incremental terminal-history contract; formal lease causality unproved" }),
    );
    assert_eq!(
        measurements[0]["reconciliationWorkerCounts"][1], 0,
        "unchanged terminal registry history is repeated in the next heartbeat"
    );
}

fn indexed_worker_record(root: &Path, session: &str, state: &str) -> WorkerProcessRecord {
    WorkerProcessRecord {
        worker_session_id: session.to_owned(),
        worker_id: WORKER.to_owned(),
        worker_instance_id: format!("instance-{session}"),
        pid: 999_999,
        process_start_identity: "owned indexed fixture".to_owned(),
        repository_binding_id: "rbn_fixture".to_owned(),
        occupancy_lease_id: LEASE.to_owned(),
        launch_grant_id: format!("grant-{session}"),
        data_directory: root.display().to_string(),
        state: state.to_owned(),
        exit_code: (state == "exited").then_some(0),
        last_observed_at: STAMP.to_owned(),
    }
}

#[test]
fn indexed_worker_reports_reopen_and_enqueue_mark_preserves_new_state() {
    let root = root("report-reopen");
    let mut store = mirrored_store(&root);
    let original = indexed_worker_record(&root, "wsn_fixture", "exited");
    store.put_worker_process(&original).unwrap();
    store
        .retain_worker_launch_receipt("grant-old", "wsn_old", "rejected", "key-old", b"{}", STAMP)
        .unwrap();
    store.close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    assert_eq!(
        store.worker_report_candidates("reconcile", None).unwrap(),
        vec![original.clone()]
    );
    assert_eq!(
        store.pending_worker_launch_receipts().unwrap(),
        vec![("grant-old".into(), b"{}".to_vec())]
    );
    store.worker_report_queued("reconcile", &original).unwrap();
    store.worker_launch_report_queued("grant-old").unwrap();
    let mut changed = original.clone();
    changed.state = "running".into();
    changed.exit_code = None;
    store.put_worker_process(&changed).unwrap();
    store.worker_report_queued("exit", &original).unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap(),
        vec![changed.clone()]
    );
    store.worker_report_queued("exit", &changed).unwrap();
    store.worker_report_queued("reconcile", &changed).unwrap();
    store.close().unwrap();
    let store = DeviceStore::open(&root).unwrap();
    assert!(
        store
            .worker_report_candidates("exit", None)
            .unwrap()
            .is_empty()
    );
    assert!(store.pending_worker_launch_receipts().unwrap().is_empty());
    assert_eq!(store.worker_processes().unwrap(), vec![changed]);
    assert_eq!(
        store.worker_launch_receipts().unwrap(),
        vec![b"{}".to_vec()]
    );
    store.close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_worker_report_failure_rolls_back_the_registry_update() {
    let root = root("report-rollback");
    let mut store = mirrored_store(&root);
    let record = indexed_worker_record(&root, "wsn_rollback", "running");
    store.put_worker_process(&record).unwrap();
    store.worker_report_queued("reconcile", &record).unwrap();
    let connection = rusqlite::Connection::open(store.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_report_pending BEFORE INSERT ON worker_report_pending BEGIN SELECT RAISE(ABORT,'report pending unavailable'); END;").unwrap();
    let mut terminal = record.clone();
    terminal.state = "exited".into();
    terminal.exit_code = Some(0);
    assert!(store.put_worker_process(&terminal).is_err());
    assert_eq!(
        store.worker_process(&record.worker_session_id).unwrap(),
        Some(record.clone())
    );
    assert!(
        store
            .worker_report_candidates("exit", None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.worker_report_candidates("reconcile", None).unwrap(),
        vec![record]
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM worker_report_pending", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    connection
        .execute_batch("DROP TRIGGER fail_report_pending;")
        .unwrap();
    store.put_worker_process(&terminal).unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap(),
        vec![terminal]
    );
    store.close().unwrap();
    drop(connection);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_running_capacity_reconciliation_releases_store_lock_before_missing_update() {
    let root = root("capacity-lock");
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let mut store = mirrored_store(&root);
    let mut record = indexed_worker_record(&root, "wsn_dead", "running");
    record.pid = pid;
    store.put_worker_process(&record).unwrap();
    drop(store);
    let supervisor = supervisor(&root, None);
    let capacity = supervisor.clone();
    let (sender, receiver) = mpsc::channel();
    let task = thread::spawn(move || {
        let _ = sender.send(capacity.running_worker_sessions());
    });
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("capacity read must release its store guard before registry update")
            .unwrap(),
        0
    );
    task.join().unwrap();
    assert_eq!(
        supervisor
            .worker_process("wsn_dead")
            .unwrap()
            .unwrap()
            .state,
        "missing"
    );
    drop(supervisor);
    fs::remove_dir_all(root).unwrap();
}

fn downlink_fixture_failure_diagnostic(error: &DownlinkFailure) -> &dyn fmt::Display {
    match error {
        DownlinkFailure::Retriable(message) => message,
        DownlinkFailure::Fatal(cause) => cause,
    }
}

#[test]
fn downlink_raw_duplicate_budget_preserves_suffix_gap_zero_and_replay() {
    let root = root("downlink-budget");
    let transport = Arc::new(LocalTransport::new());
    let mut daemon = daemon(&root.join("device"), transport);
    let node_id = daemon.node_id.clone();
    let instance_id = daemon.instance_id.clone();
    let frame = |sequence| {
        serde_json::to_value(ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("msg_{sequence:026}"),
            client_node_id: node_id.clone(),
            client_instance_id: instance_id.clone(),
            sequence,
            occurred_at: STAMP.to_owned(),
            message: ServerToClientMessage::CommandAck(ClientCommandAckPayload {
                command_kind: ClientControlMessageKind::WorkerStop,
                command_message_id: format!("msg_{sequence:026}"),
                status: CommandAckStatus::Accepted,
                current_revision: Some(1),
                error: None,
            }),
        })
        .unwrap()
    };
    let owned = frame(1);
    assert_eq!(
        match daemon.ingest_downlink(std::slice::from_ref(&owned), &[]) {
            Ok(accepted) => accepted,
            Err(ref error) => panic!(
                "valid downlink fixture must be accepted: {}",
                downlink_fixture_failure_diagnostic(error)
            ),
        },
        1
    );
    let mut duplicates = vec![owned; 64];
    duplicates.push(serde_json::json!({"invalid": "deferred suffix"}));
    assert_eq!(
        match daemon.ingest_downlink(&duplicates, &[]) {
            Ok(accepted) => accepted,
            Err(ref error) => panic!(
                "valid downlink fixture must be accepted: {}",
                downlink_fixture_failure_diagnostic(error)
            ),
        },
        0
    );
    assert_eq!(daemon.downlink.ack_sequence(), 1);
    assert!(daemon.ingest_downlink(&[frame(3)], &[]).is_err());
    assert!(daemon.ingest_downlink(&[frame(0)], &[]).is_err());
    assert_eq!(daemon.downlink.ack_sequence(), 1);
    assert_eq!(
        match daemon.ingest_downlink(&[frame(2)], &[]) {
            Ok(accepted) => accepted,
            Err(ref error) => panic!(
                "valid downlink fixture must be accepted: {}",
                downlink_fixture_failure_diagnostic(error)
            ),
        },
        1
    );
    assert_eq!(
        match daemon.ingest_downlink(&[frame(2)], &[]) {
            Ok(accepted) => accepted,
            Err(ref error) => panic!(
                "valid downlink fixture must be accepted: {}",
                downlink_fixture_failure_diagnostic(error)
            ),
        },
        0
    );
    assert_eq!(daemon.downlink.ack_sequence(), 2);
    assert_eq!(daemon.status.downlink_accepted_through, 2);
    drop(daemon);
    fs::remove_dir_all(root).unwrap();
}
