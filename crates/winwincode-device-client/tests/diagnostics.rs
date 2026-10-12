// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rusqlite::Connection;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
};
use winwincode_device_client::{
    DaemonConfig, DaemonError, DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeResponse,
    ExchangeTransport, ExchangeTransportError, IdentityRecord, ensure_device_identity,
};
use winwincode_network::{Acceptance, DiagnosticCode, ErrorKind, NetworkFailure, Phase};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    identity: IdentityRecord,
    journal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "wwc-device-diagnostic-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = DeviceStore::open(&root).expect("isolated device store");
        let identity = ensure_device_identity(
            &mut store,
            &DeviceIdentitySeed {
                display_name: "diagnostic fixture".into(),
                platform: "darwin".into(),
                architecture: "arm64".into(),
                client_version: "0.1.0-alpha.1".into(),
            },
            "2026-10-08T00:00:00.000Z",
        )
        .expect("fixture identity");
        let journal = store
            .database_path()
            .with_file_name("network-requests.sqlite3");
        Self {
            root,
            identity,
            journal,
        }
    }

    fn daemon(&self, transport: Arc<FixedResponse>) -> DeviceDaemon {
        DeviceDaemon::start(
            DaemonConfig {
                server_profile_id: "diagnostic-server".into(),
                base_url: "https://invalid.example/internal/v1/client/exchange".into(),
                server_display_name: "diagnostic fixture".into(),
                device_display_name: "diagnostic fixture".into(),
                platform: ClientPlatformTarget::Aarch64AppleDarwin,
                architecture: ClientArchitecture::Aarch64,
                client_version: "0.1.0-alpha.1".into(),
                heartbeat_interval: Duration::from_secs(5),
                enroll_poll_interval: Duration::from_secs(1),
                max_frames_per_exchange: 8,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(16),
                capacity: ClientCapacityReport {
                    max_concurrent_worker_sessions: 1,
                    running_worker_sessions: 0,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
            },
            DeviceStore::open(&self.root).expect("reopen fixture store"),
            transport,
            &self.identity,
        )
        .expect("fixture daemon")
    }

    fn failures(&self) -> Vec<NetworkFailure> {
        let connection = Connection::open(&self.journal).expect("read journal");
        let mut query = connection
            .prepare("SELECT failure_json FROM network_request_attempts ORDER BY sequence")
            .unwrap();
        query
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| {
                let json = row.unwrap();
                assert!(!json.contains("SYNTHETIC_PRIVATE"));
                assert!(!json.contains("invalid.example"));
                serde_json::from_str(&json).unwrap()
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("fixture cleanup");
    }
}

struct FixedResponse {
    bytes: Vec<u8>,
    sends: AtomicUsize,
}

impl ExchangeTransport for FixedResponse {
    fn exchange(
        &self,
        _credential: Option<&str>,
        _request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(self.bytes.clone())
    }
}

fn network_error(daemon: &mut DeviceDaemon, mut now: Instant) -> NetworkFailure {
    for _ in 0..8 {
        match daemon.tick(now) {
            Err(DaemonError::Network(failure)) => return failure,
            Ok(winwincode_device_client::TickOutcome::Waiting { ready_in }) => {
                now += ready_in.max(Duration::from_millis(1));
            }
            _ => panic!("expected network failure"),
        }
    }
    panic!("fixture never became ready");
}

#[test]
fn schema_mismatch_is_persisted_and_stops_exact_control_replay_after_restart() {
    let fixture = Fixture::new();
    let transport = Arc::new(FixedResponse {
        bytes: serde_json::to_vec(&ExchangeResponse {
            schema_version: "SYNTHETIC_PRIVATE_SCHEMA".into(),
            ack_sequence: 0,
            replay_from_sequence: None,
            frames: vec![],
            enrollment: None,
            worker_credentials: vec![],
        })
        .unwrap(),
        sends: AtomicUsize::new(0),
    });
    let now = Instant::now();
    let mut daemon = fixture.daemon(Arc::clone(&transport));
    let first = network_error(&mut daemon, now);
    assert_eq!(first.kind, ErrorKind::RequestInvalid);
    assert_eq!(first.acceptance, Acceptance::ResponseReceived);
    assert_eq!(first.phase, Phase::Decode);
    assert_eq!(
        first.diagnostic.unwrap().code,
        DiagnosticCode::SchemaVersion
    );
    assert_eq!(
        network_error(&mut daemon, now + Duration::from_hours(1)),
        first
    );
    assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failures(), vec![first]);
    drop(daemon);
    let mut restarted = fixture.daemon(Arc::clone(&transport));
    assert_eq!(
        network_error(&mut restarted, now + Duration::from_hours(2)),
        first
    );
    assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failures(), vec![first]);
}

#[test]
fn malformed_json_retains_syntax_location_and_can_retry_exact_control() {
    let fixture = Fixture::new();
    let transport = Arc::new(FixedResponse {
        bytes: b"{\n SYNTHETIC_PRIVATE_RESPONSE".to_vec(),
        sends: AtomicUsize::new(0),
    });
    let now = Instant::now();
    let mut daemon = fixture.daemon(Arc::clone(&transport));
    let first = network_error(&mut daemon, now);
    assert_eq!(first.kind, ErrorKind::ProtocolInvalid);
    let diagnostic = first.diagnostic.unwrap();
    assert_eq!(diagnostic.code, DiagnosticCode::JsonSyntax);
    assert_eq!(diagnostic.line, Some(2));
    assert!(diagnostic.column.is_some());
    let second = network_error(&mut daemon, now + Duration::from_hours(1));
    assert_eq!(second.kind, ErrorKind::ProtocolInvalid);
    assert_eq!(transport.sends.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.failures(), vec![first, second]);
}
