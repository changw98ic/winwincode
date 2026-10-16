// SPDX-License-Identifier: Apache-2.0

#[path = "support/device_provider.rs"]
mod device_provider;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_control_plane::ClientOccupancyService;
use winwincode_domain::Instant;
use winwincode_server::{
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ClientSessionsApplication,
    ClientSessionsConfig, ClientSessionsError, ClientSessionsErrorKind,
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
        storage.client_downlink_outbox().expect("downlink schema");
        drop(storage);
        device_provider::stage_chat(&directory, NODE, HOLDER, 100);
        device_provider::stage_chat(&directory, NODE, HOLDER, 101);
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

    fn launch_facts(&self) -> (i64, Vec<(i64, Value)>) {
        let db = rusqlite::Connection::open(self.directory.join("control-plane.sqlite3"))
            .expect("facts connection");
        let grants = db
            .query_row("SELECT COUNT(*) FROM worker_launch_grants", [], |r| {
                r.get(0)
            })
            .expect("grant count");
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
        (grants, frames)
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
            "SELECT COUNT(*) FROM worker_launch_grants g JOIN worker_launch_publications p ON p.grant_id=g.worker_launch_grant_id JOIN client_downlink_frames f ON f.client_node_id=p.client_node_id AND f.sequence=p.sequence AND f.message_id=p.message_id WHERE json_extract(f.frame,'$.payload.launchGrant.workerLaunchGrantId')=g.worker_launch_grant_id AND json_extract(f.frame,'$.payload.launchGrant.workerSessionId')=g.worker_session_id AND json_extract(f.frame,'$.payload.launchGrant.workerId')=g.worker_id AND json_extract(f.frame,'$.payload.launchGrant.workerInstanceId')=g.worker_instance_id",
            [], |r| r.get(0),
        ).expect("exact grant and frame binding");
        assert_eq!(publications, expected);
        assert_eq!(matched, expected);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
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
    let gate = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let threads: Vec<_> = sessions
            .into_iter()
            .map(|session| {
                let app = fixture.application(Arc::new(fixture.real_exchange()));
                let gate = &gate;
                scope.spawn(move || {
                    gate.wait();
                    launch(&app, &request(session))
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().expect("launch thread"))
            .collect::<Vec<_>>()
    });
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
    let (grants, frames) = fixture.launch_facts();
    assert_eq!((grants, frames.len()), (2, 2));
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
    let (grants, frames) = fixture.launch_facts();
    assert_eq!((grants, frames.len()), (1, 1));
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
    let (grants, frames) = fixture.launch_facts();
    assert_eq!(
        (grants, frames.len()),
        (1, 1),
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
fn downlink_publication_failure_rolls_back_the_launch_and_allows_retry() {
    let fixture = Fixture::new("publication-failure", 2);
    let db = rusqlite::Connection::open(fixture.directory.join("control-plane.sqlite3"))
        .expect("failure injection connection");
    db.execute_batch("CREATE TRIGGER reject_launch_downlink BEFORE INSERT ON client_downlink_frames BEGIN SELECT RAISE(ABORT, 'fixture downlink publication failure'); END")
        .expect("inject downlink failure");
    let app = fixture.application(Arc::new(fixture.real_exchange()));
    let error = launch(&app, &request(100)).expect_err("downlink publication fails");
    assert_eq!(fixture.launch_facts(), (0, Vec::new()));
    assert_eq!(error.kind(), ClientSessionsErrorKind::Unavailable);
    assert_eq!(
        error.to_string(),
        "client session launch service is unavailable"
    );
    assert_eq!(error.failure_stage(), Some("launch_bundle"));
    assert_eq!(error.failure_code(), Some("storage_invalid_input"));
    assert!(!format!("{error:?}").contains(fixture.directory.to_string_lossy().as_ref()));
    db.execute_batch("DROP TRIGGER reject_launch_downlink")
        .expect("allow downlink publication");

    let retry = fixture.application(Arc::new(fixture.real_exchange()));
    assert_eq!(
        launch(&retry, &request(100)).expect_err("no ack").kind(),
        ClientSessionsErrorKind::LaunchAckTimeout
    );
    let facts = fixture.launch_facts();
    assert_eq!((facts.0, facts.1.len()), (1, 1));
    fixture.assert_coherent_publication(1);
    let reopened = fixture.application(Arc::new(fixture.real_exchange()));
    assert_eq!(
        launch(&reopened, &request(100)).expect_err("no ack").kind(),
        ClientSessionsErrorKind::LaunchAckTimeout
    );
    assert_eq!(fixture.launch_facts(), facts);
}
