// SPDX-License-Identifier: Apache-2.0

//! Independent native launch regressions. These tests use the real application,
//! credential publication and `SQLite` transaction. They start no HTTP server,
//! Worker process or model request. A retained launch frame is a launch intent,
//! not evidence that a Worker process started.

#[path = "support/device_provider.rs"]
mod device_provider;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_control_plane::{ClientOccupancyService, PrivateLaunchMaterialStore};
use winwincode_domain::Instant;
use winwincode_server::{
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangeError, ClientExchangePort,
    ClientSessionsApplication, ClientSessionsConfig, ClientSessionsError, ClientSessionsErrorKind,
    WorkerCredentialDelivery,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, OccupancyClaim, RepositoryAccessGrantIssuance,
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    RepositoryGrantPermissions, SqliteStorage,
};

const NODE: &str = "cnd_00000000000000000000000001";
const INSTANCE: &str = "cix_00000000000000000000000001";
const HOLDER: &str = "usr_00000000000000000000000001";
const BINDING: &str = "rbd_00000000000000000000000001";
const CLIENT: &str = "123456789";
const T0: &str = "2026-09-04T12:00:00.000Z";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    #[allow(clippy::too_many_lines)]
    fn new(label: &str, capacity: u32) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "wwc-adxi-{label}-{}-{}-{nanos}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        let now = Instant(T0.to_owned());
        let registration = ClientNodeRegistration::try_new(
            NODE,
            CLIENT,
            "adxi native fixture",
            "aarch64-apple-darwin",
            "aarch64",
            "1.0.0",
            None,
            Some(INSTANCE.to_owned()),
            capacity,
        )
        .expect("registration");
        let mut registry = storage.client_node_registry().expect("registry");
        registry.register(&registration, 0, &now).expect("register");
        registry
            .update_presence(NODE, ClientPresenceState::Online, 1)
            .expect("online");
        let grant = AccessGrantIssuance::try_new(
            "cag_00000000000000000000000001",
            NODE,
            HOLDER,
            HOLDER,
            GrantTrustMode::Trusted,
            None,
        )
        .expect("client grant");
        storage
            .client_connect_ledger()
            .expect("connect ledger")
            .create_grant(
                &grant,
                GrantSource::Administrator,
                GrantPermissions::USE,
                &now,
            )
            .expect("grant use");
        let projection = RepositoryBindingProjection::try_new(
            BINDING,
            NODE,
            "adxi repository",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{:x}", Sha256::digest(BINDING.as_bytes())),
        )
        .expect("binding");
        let mut bindings = storage.repository_binding_ledger().expect("bindings");
        bindings
            .upsert(&projection, None, 0, &now)
            .expect("upsert binding");
        let repo_grant = RepositoryAccessGrantIssuance::try_new(
            "rag_00000000000000000000000001",
            BINDING,
            HOLDER,
            HOLDER,
        )
        .expect("repository grant");
        bindings
            .create_grant(&repo_grant, RepositoryGrantPermissions::Use, &now)
            .expect("repository use");
        let claim = OccupancyClaim::try_new(
            "ocl_00000000000000000000000001",
            NODE,
            HOLDER,
            "req_00000000000000000000000001",
        )
        .expect("claim");
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let lease = occupancy
            .atomic_claim(&claim, &now)
            .expect("claim occupancy");
        occupancy
            .record_acknowledgement(&lease.occupancy_lease_id, lease.fencing_token, None, &now)
            .expect("occupancy ack");
        storage.worker_launch_grant_ledger().expect("launch schema");
        storage
            .worker_session_credential_ledger()
            .expect("credential schema");
        storage.client_downlink_outbox().expect("downlink schema");
        drop(storage);
        device_provider::stage_chat(&directory, NODE, HOLDER, 100);
        device_provider::stage_chat(&directory, NODE, HOLDER, 101);
        // Initialize the real private store once. Its first-key creation is a
        // separate boundary from concurrent launch admission under test.
        PrivateLaunchMaterialStore::open(&directory).expect("private material store");
        Self { directory }
    }

    fn application(&self, exchange: Arc<dyn ClientExchangePort>) -> ClientSessionsApplication {
        ClientSessionsApplication::open_with_exchange(
            &self.directory,
            &ClientSessionsConfig {
                launch_wait: Duration::from_millis(80),
                poll_interval: Duration::from_millis(5),
                grant_ttl: Duration::from_mins(2),
            },
            exchange,
        )
        .expect("launch application")
    }

    fn real_exchange(&self) -> ClientExchangeApplication {
        ClientExchangeApplication::open(&self.directory, &ClientExchangeConfig::default())
            .expect("real exchange")
    }

    fn launch_facts(&self) -> (i64, i64, Vec<(i64, Value)>) {
        let db = rusqlite::Connection::open(self.directory.join("control-plane.sqlite3"))
            .expect("facts connection");
        let grants = db
            .query_row("SELECT COUNT(*) FROM worker_launch_grants", [], |r| {
                r.get(0)
            })
            .expect("grant count");
        let credentials = db
            .query_row("SELECT COUNT(*) FROM worker_session_credentials", [], |r| {
                r.get(0)
            })
            .expect("credential count");
        let mut statement = db.prepare("SELECT sequence,frame FROM client_downlink_frames WHERE client_node_id=?1 ORDER BY sequence").expect("downlink facts");
        let frames = statement
            .query_map([NODE], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .expect("frames")
            .map(|r| {
                let (sequence, frame) = r.expect("frame");
                (sequence, serde_json::from_str(&frame).expect("frame JSON"))
            })
            .collect();
        (grants, credentials, frames)
    }

    fn assert_coherent_publication(&self, expected: i64) {
        let db = rusqlite::Connection::open(self.directory.join("control-plane.sqlite3"))
            .expect("facts connection");
        let publications: i64 = db
            .query_row("SELECT COUNT(*) FROM worker_launch_publications", [], |r| {
                r.get(0)
            })
            .expect("publication count");
        let matched: i64 = db.query_row(
            "SELECT COUNT(*) FROM worker_launch_grants g JOIN worker_session_credentials c ON c.worker_launch_grant_id=g.worker_launch_grant_id AND c.worker_session_id=g.worker_session_id AND c.worker_id=g.worker_id AND c.worker_instance_id=g.worker_instance_id AND c.credential_digest=g.credential_digest JOIN worker_launch_publications p ON p.grant_id=g.worker_launch_grant_id JOIN client_downlink_frames f ON f.client_node_id=p.client_node_id AND f.sequence=p.sequence AND f.message_id=p.message_id",
            [], |r| r.get(0),
        ).expect("exact grant credential frame binding");
        assert_eq!(publications, expected);
        assert_eq!(matched, expected);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

// Two bounded gates ensure both production prepare calls passed the prior
// grant lookup and completed real private publication before either commits.
// Timeout causes an assertion instead of leaving a test thread blocked.
#[derive(Default)]
struct Gate {
    arrivals: Mutex<usize>,
    changed: Condvar,
}

impl Gate {
    fn meet(&self) {
        let mut arrivals = self.arrivals.lock().expect("gate lock");
        *arrivals += 1;
        self.changed.notify_all();
        let (arrivals, _) = self
            .changed
            .wait_timeout_while(arrivals, Duration::from_secs(10), |arrivals| *arrivals < 2)
            .expect("bounded gate");
        assert_eq!(
            *arrivals, 2,
            "both native prepares must reach the publication boundary"
        );
    }
}

struct ConcurrentExchange {
    real: ClientExchangeApplication,
    before: Gate,
    after: Gate,
    publications: AtomicUsize,
}

impl ConcurrentExchange {
    fn new(real: ClientExchangeApplication) -> Self {
        Self {
            real,
            before: Gate::default(),
            after: Gate::default(),
            publications: AtomicUsize::new(0),
        }
    }
}

impl ClientExchangePort for ConcurrentExchange {
    fn exchange(
        &self,
        credential: Option<Vec<u8>>,
        body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, ClientExchangeError> {
        self.real.exchange(credential, body, now)
    }

    fn authenticate_device(
        &self,
        credential: &[u8],
        node: &str,
    ) -> Result<(), ClientExchangeError> {
        self.real.authenticate_device(credential, node)
    }

    fn publish_worker_credential(
        &self,
        delivery: WorkerCredentialDelivery,
    ) -> Result<(), ClientExchangeError> {
        self.before.meet();
        let result = self.real.publish_worker_credential(delivery);
        if result.is_ok() {
            self.publications.fetch_add(1, Ordering::SeqCst);
        }
        self.after.meet();
        result
    }
}

fn request(session: u64) -> Value {
    json!({"schemaVersion":"winwincode/v1","clientId":CLIENT,"repositoryBindingId":BINDING,
        "productSession":{"id":format!("psn_{session:026}"),"scope":device_provider::chat_scope()}})
}

fn launch(app: &ClientSessionsApplication, body: &Value) -> Result<Value, ClientSessionsError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(app.launch(HOLDER, body))
}

fn concurrent_launches(fixture: &Fixture, sessions: [u64; 2]) -> Vec<ClientSessionsError> {
    let exchange = Arc::new(ConcurrentExchange::new(fixture.real_exchange()));
    let results = std::thread::scope(|scope| {
        let threads: Vec<_> = sessions
            .into_iter()
            .map(|session| {
                // Separate application instances prevent an application-local
                // mutex from masquerading as durable launch admission.
                let app = fixture.application(exchange.clone());
                scope.spawn(move || launch(&app, &request(session)))
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().expect("launch thread"))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        exchange.publications.load(Ordering::SeqCst),
        2,
        "both requests published real encrypted material"
    );
    results
        .into_iter()
        .map(|r| r.expect_err("no fake Device acknowledgement is sent"))
        .collect()
}

#[test]
fn different_sessions_publish_distinct_contiguous_downlinks() {
    let fixture = Fixture::new("distinct", 2);
    let errors = concurrent_launches(&fixture, [100, 101]);
    assert!(
        errors
            .iter()
            .all(|e| e.kind() == ClientSessionsErrorKind::LaunchAckTimeout),
        "{errors:?}"
    );
    let (grants, credentials, frames) = fixture.launch_facts();
    assert_eq!((grants, credentials, frames.len()), (2, 2, 2));
    assert_eq!(
        frames.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec![1, 2]
    );
    for (sequence, frame) in &frames {
        assert_eq!(frame["kind"], "client.worker.launch");
        assert_eq!(frame["sequence"], *sequence);
        assert_eq!(frame["clientNodeId"], NODE);
        assert_eq!(frame["clientInstanceId"], INSTANCE);
    }
    assert_ne!(
        frames[0].1["payload"]["launchGrant"]["workerLaunchGrantId"],
        frames[1].1["payload"]["launchGrant"]["workerLaunchGrantId"]
    );
    fixture.assert_coherent_publication(2);
}

#[test]
fn concurrent_capacity_rejection_leaves_only_one_public_launch() {
    let fixture = Fixture::new("capacity", 1);
    let errors = concurrent_launches(&fixture, [100, 101]);
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.kind() == ClientSessionsErrorKind::LaunchAckTimeout)
            .count(),
        1,
        "{errors:?}"
    );
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.kind() == ClientSessionsErrorKind::CapacityExhausted)
            .count(),
        1,
        "{errors:?}"
    );
    let (grants, credentials, frames) = fixture.launch_facts();
    assert_eq!((grants, credentials, frames.len()), (1, 1, 1));
    fixture.assert_coherent_publication(1);
}

#[test]
fn concurrent_same_session_retains_one_idempotent_launch_identity() {
    let fixture = Fixture::new("same-session", 2);
    let errors = concurrent_launches(&fixture, [100, 100]);
    assert!(
        errors
            .iter()
            .all(|e| e.kind() == ClientSessionsErrorKind::LaunchAckTimeout),
        "{errors:?}"
    );
    let (grants, credentials, frames) = fixture.launch_facts();
    assert_eq!(
        (grants, credentials, frames.len()),
        (1, 1, 1),
        "the same ProductSession must not mint two live Worker identities"
    );
    fixture.assert_coherent_publication(1);
    let before = fixture.launch_facts();
    let repeat = fixture.application(Arc::new(fixture.real_exchange()));
    assert_eq!(
        launch(&repeat, &request(100))
            .expect_err("still no ack")
            .kind(),
        ClientSessionsErrorKind::LaunchAckTimeout
    );
    assert_eq!(
        fixture.launch_facts(),
        before,
        "response retry must reuse the retained launch identity"
    );
}

#[test]
fn credential_publication_failure_keeps_safe_stage_and_no_public_grant() {
    let fixture = Fixture::new("private-store-failure", 2);
    let private = fixture.directory.join("private-launch-material");
    std::fs::remove_dir_all(&private).expect("remove fixture private store");
    std::fs::write(&private, b"adxi-fixture-secret-must-not-enter-errors")
        .expect("obstruct private store");
    let app = fixture.application(Arc::new(fixture.real_exchange()));
    let error = launch(&app, &request(100)).expect_err("real private publication fails");
    assert_eq!(error.kind(), ClientSessionsErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        "client session launch service is unavailable"
    );
    assert_eq!(fixture.launch_facts(), (0, 0, Vec::new()));
    let diagnostic = format!("{error:?}");
    assert!(
        !diagnostic.contains("adxi-fixture-secret")
            && !diagnostic.contains(private.to_string_lossy().as_ref())
    );
    // The pre-fix type already supports Debug, so this is an executable
    // behavioral red, not a compilation failure for a proposed accessor.
    assert!(
        diagnostic.contains("credential_publication"),
        "unavailable launch must retain its safe internal stage: {diagnostic}"
    );
    assert_eq!(error.failure_stage(), Some("credential_publication"));
    assert_eq!(error.failure_code(), Some("exchange_unavailable"));
}

struct LockedPublicationExchange {
    real: ClientExchangeApplication,
    directory: PathBuf,
    held_writer: Mutex<Option<rusqlite::Connection>>,
}

impl ClientExchangePort for LockedPublicationExchange {
    fn exchange(
        &self,
        credential: Option<Vec<u8>>,
        body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, ClientExchangeError> {
        self.real.exchange(credential, body, now)
    }

    fn authenticate_device(
        &self,
        credential: &[u8],
        node: &str,
    ) -> Result<(), ClientExchangeError> {
        self.real.authenticate_device(credential, node)
    }

    fn publish_worker_credential(
        &self,
        delivery: WorkerCredentialDelivery,
    ) -> Result<(), ClientExchangeError> {
        self.real.publish_worker_credential(delivery)?;
        let writer = rusqlite::Connection::open(self.directory.join("control-plane.sqlite3"))
            .expect("independent writer");
        writer
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold writer before launch bundle");
        let observer = rusqlite::Connection::open(self.directory.join("control-plane.sqlite3"))
            .expect("lock observer");
        observer
            .busy_timeout(Duration::ZERO)
            .expect("no observer retry");
        assert!(
            matches!(
                observer.execute_batch("BEGIN IMMEDIATE"),
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy
            ),
            "the independent writer must actually hold SQLite admission"
        );
        *self.held_writer.lock().expect("writer slot") = Some(writer);
        Ok(())
    }
}

#[test]
fn forced_sqlite_writer_contention_preserves_stage_and_rolls_back_launch() {
    let fixture = Fixture::new("writer-contention", 2);
    let exchange = Arc::new(LockedPublicationExchange {
        real: fixture.real_exchange(),
        directory: fixture.directory.clone(),
        held_writer: Mutex::new(None),
    });
    let app = fixture.application(exchange.clone());
    let error = launch(&app, &request(100)).expect_err("launch writer is held");
    let writer = exchange
        .held_writer
        .lock()
        .expect("writer slot")
        .take()
        .expect("test held the writer at the launch boundary");
    writer
        .execute_batch("ROLLBACK")
        .expect("release test writer");
    assert_eq!(fixture.launch_facts(), (0, 0, Vec::new()));
    assert_eq!(error.kind(), ClientSessionsErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        "client session launch service is unavailable"
    );
    assert_eq!(error.failure_stage(), Some("launch_bundle"));
    assert_eq!(error.failure_code(), Some("storage_adapter"));
    assert!(!format!("{error:?}").contains(fixture.directory.to_string_lossy().as_ref()));

    // The failed transaction did not reserve half a grant. A fresh native
    // application may submit the original request, then reuse its exact facts.
    let retry = fixture.application(Arc::new(fixture.real_exchange()));
    assert_eq!(
        launch(&retry, &request(100)).expect_err("no ack").kind(),
        ClientSessionsErrorKind::LaunchAckTimeout
    );
    let facts = fixture.launch_facts();
    assert_eq!((facts.0, facts.1, facts.2.len()), (1, 1, 1));
    fixture.assert_coherent_publication(1);
    let reopened = fixture.application(Arc::new(fixture.real_exchange()));
    assert_eq!(
        launch(&reopened, &request(100)).expect_err("no ack").kind(),
        ClientSessionsErrorKind::LaunchAckTimeout
    );
    assert_eq!(fixture.launch_facts(), facts);
}
