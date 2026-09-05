// SPDX-License-Identifier: Apache-2.0

//! CLIENT-200.2 coverage: the dynamic connect code lifecycle (plan 11.1,
//! 11.3) and the daemon downlink extensions — `client.access.challenge`
//! answering, `client.client_lock` application, and skipped unknown commands.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientLockState, ClientPlatformTarget,
    ClientRepositoryRescanReason, ConnectCodeState,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientToServerMessage, CommandContext,
    ServerAccessChallengePayload, ServerClientLockPayload, ServerRepositoryRescanPayload,
    ServerToClientEnvelope, ServerToClientMessage,
};
use winwincode_device_client::connect_code::{
    self, ChallengeVerdict, connect_code_digest, generate_connect_code, is_weak_connect_code,
};
use winwincode_device_client::{
    ConnectCodeStateRecord, DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore,
    ExchangeRequest, ExchangeResponse, ExchangeTransport, ExchangeTransportError, IdentityRecord,
    IssuedEnrollment, adopt_enrollment, ensure_device_identity, load_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_CHALLENGE: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-connect-code-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn cleanup(root: &Path) {
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

/// The server-issued enrollment fixture (canonical `cnd_` + 26 Crockford,
/// 10 public digits, one 32-byte credential).
const ASSIGNED_NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "0123456789";
const ISSUED_SECRET: [u8; 32] = [0xcd; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";

fn issued_credential_material() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(ISSUED_SECRET.len() * 2);
    for byte in ISSUED_SECRET {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn issued_credential_digest() -> String {
    format!("sha256:{:x}", Sha256::digest(ISSUED_SECRET))
}

/// Opens a store whose identity already adopted the server-issued
/// enrollment, so daemon sessions start in the enrolled phase.
fn open_enrolled(root: &Path) -> (DeviceStore, IdentityRecord) {
    let mut store = DeviceStore::open(root).expect("device store should open");
    ensure_device_identity(&mut store, &seed(), STAMP).expect("device identity should load");
    let device_id = load_device_identity(&store)
        .expect("identity read")
        .expect("fresh identity")
        .identity()
        .device_id()
        .to_owned();
    adopt_enrollment(
        &mut store,
        &device_id,
        &IssuedEnrollment {
            client_node_id: ASSIGNED_NODE.to_owned(),
            public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
            credential_material: issued_credential_material(),
            credential_digest: issued_credential_digest(),
        },
        STAMP,
    )
    .expect("enrollment adoption");
    let record = load_device_identity(&store)
        .expect("identity reload")
        .expect("enrolled identity");
    (store, record)
}

/// One client-to-server frame the fake server received.
#[derive(Clone, Debug)]
struct SimFrame {
    kind: String,
    frame: Value,
}

#[derive(Default)]
struct ServerState {
    received: Vec<SimFrame>,
    sequences: BTreeSet<u64>,
}

/// In-memory fake of the exchange endpoint: it records every frame,
/// acknowledges the contiguous prefix, and delivers the queued downlink
/// batches with the responses.
struct ServerSim {
    state: Mutex<ServerState>,
    downlink: Mutex<VecDeque<Vec<Value>>>,
}

impl ServerSim {
    fn new() -> Self {
        Self {
            state: Mutex::new(ServerState::default()),
            downlink: Mutex::new(VecDeque::new()),
        }
    }

    fn queue_downlink(&self, frames: Vec<Value>) {
        self.downlink
            .lock()
            .expect("downlink lock")
            .push_back(frames);
    }

    fn all_frames(&self) -> Vec<SimFrame> {
        self.state.lock().expect("server sim lock").received.clone()
    }

    fn frames_with_kind(&self, kind: &str) -> Vec<SimFrame> {
        self.all_frames()
            .into_iter()
            .filter(|frame| frame.kind == kind)
            .collect()
    }
}

impl ExchangeTransport for ServerSim {
    fn exchange(
        &self,
        _credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        let request: ExchangeRequest = serde_json::from_slice(request_bytes)
            .map_err(|error| ExchangeTransportError::new(format!("fake decode: {error}")))?;
        let (ack, frames) = {
            let mut state = self.state.lock().expect("server sim lock");
            for frame in &request.frames {
                let kind = frame
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let sequence = frame
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                state.sequences.insert(sequence);
                state.received.push(SimFrame {
                    kind,
                    frame: frame.clone(),
                });
            }
            let mut ack = 0_u64;
            while state.sequences.contains(&(ack + 1)) {
                ack += 1;
            }
            let frames = self
                .downlink
                .lock()
                .expect("downlink lock")
                .pop_front()
                .unwrap_or_default();
            (ack, frames)
        };
        let response = ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence: ack,
            replay_from_sequence: None,
            frames,
            enrollment: None,
        };
        serde_json::to_vec(&response)
            .map_err(|error| ExchangeTransportError::new(format!("fake encode: {error}")))
    }
}

fn daemon_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: format!("server-{name}"),
        base_url: format!("https://{name}.example.test/internal/v1/client/exchange"),
        server_display_name: "WinWinCode Control Plane".to_owned(),
        device_display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
        heartbeat_interval: Duration::from_millis(2),
        enroll_poll_interval: Duration::from_millis(1),
        max_frames_per_exchange: 16,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(16),
        capacity: ClientCapacityReport {
            max_concurrent_worker_sessions: 0,
            running_worker_sessions: 0,
            reserved_worker_sessions: 0,
            draining_worker_sessions: 0,
        },
    }
}

fn started_daemon(name: &str, root: &Path, sim: &Arc<ServerSim>) -> DeviceDaemon {
    let (store, identity) = open_enrolled(root);
    DeviceDaemon::start(daemon_config(name), store, sim.clone(), &identity)
        .expect("daemon should start enrolled")
}

/// Drives ticks until `condition` holds or the budget runs out.
fn drive_until(daemon: &mut DeviceDaemon, condition: impl Fn(&DeviceDaemon) -> bool) {
    for _ in 0..600 {
        if condition(daemon) {
            return;
        }
        match daemon.tick(Instant::now()) {
            Ok(_) => thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("daemon tick failed fatally: {error:?}"),
        }
    }
    panic!("the awaited daemon condition never held");
}

fn challenge_payload(code: &ConnectCodeStateRecord) -> ServerAccessChallengePayload {
    ServerAccessChallengePayload {
        challenge_id: format!(
            "cac_CCCCCCCCCCCCCCCCCCCCCCCCCC{}",
            NEXT_CHALLENGE.fetch_add(1, Ordering::Relaxed)
        ),
        connect_code_id: code.connect_code_id.clone(),
        code_digest: code.code_digest.clone(),
        expires_at: "2026-09-04T00:05:00.000Z".to_owned(),
        requester_user_id: "usr_01j2".to_owned(),
    }
}

fn downlink_envelope(sequence: u64, message: ServerToClientMessage) -> Value {
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: format!("srv-down-{sequence}"),
        client_node_id: ASSIGNED_NODE.to_owned(),
        client_instance_id: "srv-instance".to_owned(),
        sequence,
        occurred_at: STAMP.to_owned(),
        message,
    };
    serde_json::to_value(&envelope).expect("downlink frame value")
}

fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

// ---------------------------------------------------------------------------
// Code generation shape and weak-form rejection.
// ---------------------------------------------------------------------------

#[test]
fn generated_codes_have_the_canonical_strong_shape() {
    for _ in 0..64 {
        let code = generate_connect_code().expect("code generation");
        let raw = code.expose();
        assert_eq!(raw.len(), connect_code::CONNECT_CODE_DIGITS);
        assert!(
            raw.bytes().all(|byte| byte.is_ascii_digit()),
            "the code must be digits only: {raw}"
        );
        assert!(
            !is_weak_connect_code(raw),
            "published shape must be strong: {raw}"
        );
        assert_eq!(code.grouped(), format!("{} {}", &raw[..4], &raw[4..]));
        // The redacted Debug and absence of Display keep the plaintext out
        // of logs.
        assert!(!format!("{code:?}").contains(raw));
    }
}

#[test]
fn weak_shapes_are_rejected() {
    for weak in [
        "11111111",  // all identical
        "12345678",  // full ascending run
        "87654321",  // full descending run
        "01234567",  // ascending from zero
        "1234567",   // too short
        "123456789", // too long
        "1234 678",  // not pure digits
        "1234567a",  // letter
    ] {
        assert!(is_weak_connect_code(weak), "{weak} must be rejected");
    }
    for strong in ["01824673", "94027153", "09090909", "77712345"] {
        assert!(!is_weak_connect_code(strong), "{strong} must be accepted");
    }
}

// ---------------------------------------------------------------------------
// Publication lifecycle: digest-only durability, generation supersession.
// ---------------------------------------------------------------------------

#[test]
fn publication_persists_digest_state_and_never_the_plaintext() {
    let root = temporary_directory("publish-digest");
    let (mut store, identity) = open_enrolled(&root);
    let published = connect_code::publish_connect_code(
        &mut store,
        identity.current_instance_id(),
        now_utc(),
        connect_code::CONNECT_CODE_TTL,
    )
    .expect("first publication");
    assert_eq!(published.record.generation, 1);
    assert_eq!(published.record.state, ConnectCodeState::Active);
    assert_eq!(
        published.record.code_digest,
        connect_code_digest(&published.plaintext),
        "the stored digest is the sha256 of the plaintext"
    );
    assert!(
        published.record.connect_code_id.starts_with("cct_"),
        "connect code ids follow the schema pattern: {:?}",
        published.record.connect_code_id
    );

    // The plaintext never entered durable state: neither the connect-code
    // row nor any outbox payload carries it.
    let stored = store
        .connect_code_state()
        .expect("stored state")
        .expect("row");
    assert_eq!(stored.code_digest, published.record.code_digest);
    let database = Connection::open(store.database_path()).expect("database reopen");
    for (table, column) in [
        ("connect_code_state", "code_digest"),
        ("connect_code_state", "connect_code_id"),
    ] {
        let mut statement = database
            .prepare(&format!("SELECT {column} FROM {table}"))
            .expect("statement should prepare");
        let values: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("query should map")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows should collect");
        drop(statement);
        for value in values {
            assert!(
                !value.contains(published.plaintext.expose()),
                "{table}.{column} must not carry the plaintext"
            );
        }
    }
    database.close().expect("inspection close");
    store.close().expect("store close");
    cleanup(&root);
}

#[test]
fn refresh_supersedes_the_previous_generation_monotonically() {
    let root = temporary_directory("refresh-generation");
    let (mut store, identity) = open_enrolled(&root);
    let first = connect_code::publish_connect_code(
        &mut store,
        identity.current_instance_id(),
        now_utc(),
        connect_code::CONNECT_CODE_TTL,
    )
    .expect("first publication");
    let second = connect_code::publish_connect_code(
        &mut store,
        identity.current_instance_id(),
        now_utc(),
        connect_code::CONNECT_CODE_TTL,
    )
    .expect("refresh publication");
    assert_eq!(second.record.generation, first.record.generation + 1);
    assert_ne!(
        second.record.connect_code_id, first.record.connect_code_id,
        "the refresh issues a new code identity"
    );
    assert_ne!(
        second.record.code_digest, first.record.code_digest,
        "the refresh invalidates the old digest"
    );

    // A racing publisher at the same generation is refused.
    let stale = ConnectCodeStateRecord {
        generation: first.record.generation,
        ..second.record.clone()
    };
    let error = store
        .replace_connect_code_state(&stale)
        .expect_err("non-advancing generations must be refused");
    assert_eq!(
        error.kind(),
        winwincode_device_client::DeviceStoreErrorKind::Conflict
    );

    // The old generation no longer validates challenges; the new one does.
    let old_challenge = ServerAccessChallengePayload {
        challenge_id: "cac_OLDOLDOLDOLDOLDOLDOLDOLD00".to_owned(),
        connect_code_id: first.record.connect_code_id.clone(),
        code_digest: first.record.code_digest.clone(),
        expires_at: "2026-09-04T00:05:00.000Z".to_owned(),
        requester_user_id: "usr_01j2".to_owned(),
    };
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &old_challenge, now_utc())
            .expect("old challenge evaluation"),
        ChallengeVerdict::UnknownCode,
        "the refreshed-over generation is unknown"
    );
    let current = store.connect_code_state().expect("current").expect("row");
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&current), now_utc())
            .expect("current challenge evaluation"),
        ChallengeVerdict::Confirmed
    );

    store.close().expect("store close");
    cleanup(&root);
}

#[test]
fn expiry_revocation_and_policy_reject_challenges_in_order() {
    let root = temporary_directory("challenge-branches");
    let (mut store, identity) = open_enrolled(&root);
    let expired = connect_code::publish_connect_code(
        &mut store,
        identity.current_instance_id(),
        now_utc() - time::Duration::seconds(1),
        Duration::ZERO,
    )
    .expect("expired publication");
    let code = store.connect_code_state().expect("state").expect("row");
    assert_eq!(expired.record.connect_code_id, code.connect_code_id);
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&code), now_utc())
            .expect("expired evaluation"),
        ChallengeVerdict::CodeExpired,
        "an expired code refuses challenges even while unlocked"
    );

    // Revocation beats the clock only before expiry; revoked codes refuse.
    connect_code::revoke_connect_code(&mut store, now_utc())
        .expect("revocation")
        .expect("an active code was revoked");
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&code), now_utc())
            .expect("revoked evaluation"),
        ChallengeVerdict::CodeRevoked
    );
    assert!(
        connect_code::revoke_connect_code(&mut store, now_utc())
            .expect("second revocation")
            .is_none(),
        "revoking again is a no-op"
    );

    // Fresh code, locked node: locked wins over the disabled flag.
    connect_code::publish_connect_code(
        &mut store,
        identity.current_instance_id(),
        now_utc(),
        connect_code::CONNECT_CODE_TTL,
    )
    .expect("post-lock publication");
    connect_code::set_connection_policy(&mut store, false, ClientLockState::Locked, now_utc())
        .expect("lock policy");
    let code = store.connect_code_state().expect("state").expect("row");
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&code), now_utc())
            .expect("locked evaluation"),
        ChallengeVerdict::Locked
    );

    // Unlocked but new connections disabled: still refused.
    connect_code::set_connection_policy(&mut store, false, ClientLockState::Unlocked, now_utc())
        .expect("disable policy");
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&code), now_utc())
            .expect("disabled evaluation"),
        ChallengeVerdict::NewConnectionsDisabled
    );

    // Fully open again: confirmed.
    connect_code::set_connection_policy(&mut store, true, ClientLockState::Unlocked, now_utc())
        .expect("open policy");
    assert_eq!(
        connect_code::evaluate_access_challenge(&store, &challenge_payload(&code), now_utc())
            .expect("open evaluation"),
        ChallengeVerdict::Confirmed
    );

    // Every rejection maps onto the single negative wire verdict; only the
    // confirmation maps onto `confirmed`.
    for verdict in [
        ChallengeVerdict::UnknownCode,
        ChallengeVerdict::CodeRevoked,
        ChallengeVerdict::CodeExpired,
        ChallengeVerdict::Locked,
        ChallengeVerdict::NewConnectionsDisabled,
    ] {
        assert_eq!(
            verdict.wire_status(),
            winwincode_client_port::domain::ClientChallengeAckStatus::StaleGeneration
        );
    }
    assert_eq!(
        ChallengeVerdict::Confirmed.wire_status(),
        winwincode_client_port::domain::ClientChallengeAckStatus::Confirmed
    );

    store.close().expect("store close");
    cleanup(&root);
}

// ---------------------------------------------------------------------------
// Daemon end-to-end: published frames, challenge ACKs, client lock, unknown
// downlink kinds.
// ---------------------------------------------------------------------------

#[test]
fn published_frames_reach_the_outbox_without_the_plaintext() {
    let root = temporary_directory("daemon-publish");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("publish", &root, &sim);
    let published = daemon.publish_connect_code().expect("publication");
    drive_until(&mut daemon, |daemon| {
        daemon.status().connect_codes_published >= 1 && daemon.status().frames_sent >= 1
    });

    let sent = sim.frames_with_kind("client.connect_code.published");
    assert_eq!(
        sent.len(),
        1,
        "exactly one published frame was delivered: {sent:?}"
    );
    let payload = &sent[0].frame["payload"];
    assert_eq!(
        payload["codeDigest"], published.record.code_digest,
        "the wire digest matches the durable record"
    );
    assert_eq!(
        payload["idempotencyKey"],
        format!("connect-code-publish-{}", published.record.connect_code_id)
    );
    let serialized = sent[0].frame.to_string();
    assert!(
        !serialized.contains(published.plaintext.expose()),
        "the wire frame must not carry the plaintext: {serialized}"
    );

    // The durable outbox is plaintext-free as well.
    let pending = daemon.store_mut().pending_outbox_envelopes().expect("rows");
    for row in &pending {
        let blob = String::from_utf8_lossy(&row.payload);
        assert!(
            !blob.contains(published.plaintext.expose()),
            "outbox row {} must not carry the plaintext",
            row.message_id
        );
    }

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn challenges_are_answered_confirmed_replayed_and_expired() {
    let root = temporary_directory("daemon-challenge");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("challenge", &root, &sim);
    let published = daemon.publish_connect_code().expect("publication");

    // The same challenge queued twice: a server replay of an unanswered
    // challenge. Both deliveries must produce the identical idempotent ACK.
    let challenge = challenge_payload(&published.record);
    let first = downlink_envelope(
        1,
        ServerToClientMessage::AccessChallenge(Box::new(challenge.clone())),
    );
    let second = downlink_envelope(
        2,
        ServerToClientMessage::AccessChallenge(Box::new(challenge.clone())),
    );
    sim.queue_downlink(vec![first]);
    sim.queue_downlink(vec![second]);
    drive_until(&mut daemon, |_| {
        sim.frames_with_kind("client.access.challenge_ack").len() >= 2
    });

    let acks = sim.frames_with_kind("client.access.challenge_ack");
    assert_eq!(acks.len(), 2, "both deliveries were answered: {acks:?}");
    for ack in &acks {
        assert_eq!(ack.frame["payload"]["challengeId"], challenge.challenge_id);
        assert_eq!(
            ack.frame["payload"]["connectCodeId"],
            published.record.connect_code_id
        );
        assert_eq!(ack.frame["payload"]["status"], "confirmed");
    }
    assert_eq!(
        acks[0].frame["payload"]["idempotencyKey"], acks[1].frame["payload"]["idempotencyKey"],
        "replays carry the deterministic per-challenge idempotency key"
    );
    assert_eq!(
        acks[0].frame["payload"], acks[1].frame["payload"],
        "the replayed verdict is byte-identical"
    );
    assert_eq!(daemon.status().access_challenges_confirmed, 2);

    // An expired publication answers stale_generation.
    let expired = daemon
        .publish_connect_code_with_ttl(Duration::ZERO)
        .expect("expired publication");
    let expired_challenge = challenge_payload(&expired.record);
    sim.queue_downlink(vec![downlink_envelope(
        3,
        ServerToClientMessage::AccessChallenge(Box::new(expired_challenge.clone())),
    )]);
    drive_until(&mut daemon, |_| {
        sim.frames_with_kind("client.access.challenge_ack").len() >= 3
    });
    let acks = sim.frames_with_kind("client.access.challenge_ack");
    let last = acks.last().expect("expired ack");
    assert_eq!(
        last.frame["payload"]["status"], "stale_generation",
        "the expired generation answers with the single negative verdict"
    );
    assert_eq!(
        last.frame["payload"]["connectCodeId"],
        expired.record.connect_code_id
    );
    assert_eq!(daemon.status().access_challenges_confirmed, 2);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn locked_nodes_refuse_challenges_and_report_the_policy() {
    let root = temporary_directory("daemon-lock");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("lock", &root, &sim);
    let published = daemon.publish_connect_code().expect("publication");

    // Local lock: acceptingConnections=false + lockState=locked, durable.
    let policy = daemon.lock_client().expect("local lock");
    assert!(!policy.accepting_connections);
    assert_eq!(policy.lock_state, ClientLockState::Locked);
    let reopened = daemon.connection_policy().expect("durable policy");
    assert!(!reopened.accepting_connections);
    assert_eq!(reopened.lock_state, ClientLockState::Locked);

    let challenge = challenge_payload(&published.record);
    sim.queue_downlink(vec![downlink_envelope(
        1,
        ServerToClientMessage::AccessChallenge(Box::new(challenge.clone())),
    )]);
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.access.challenge_ack")
            .is_empty()
    });
    let acks = sim.frames_with_kind("client.access.challenge_ack");
    assert_eq!(
        acks.last().expect("locked ack").frame["payload"]["status"],
        "stale_generation",
        "the lock window refuses every challenge"
    );
    assert_eq!(
        daemon.status().access_challenges_confirmed,
        0,
        "no challenge was confirmed while locked"
    );

    // The durable policy rides every subsequent heartbeat report.
    drive_until(&mut daemon, |daemon| {
        daemon.status().heartbeats_enqueued >= 1
    });
    let heartbeats = sim.frames_with_kind("client.heartbeat");
    let heartbeat = heartbeats.last().expect("heartbeat frame");
    assert_eq!(heartbeat.frame["payload"]["acceptingConnections"], false);
    assert_eq!(heartbeat.frame["payload"]["lockState"], "locked");

    // Unlock restores the open policy.
    let unlocked = daemon.unlock_client().expect("local unlock");
    assert!(unlocked.accepting_connections);
    assert_eq!(unlocked.lock_state, ClientLockState::Unlocked);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn server_client_lock_commands_persist_and_acknowledge() {
    let root = temporary_directory("daemon-client-lock");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("client-lock", &root, &sim);

    sim.queue_downlink(vec![downlink_envelope(
        1,
        ServerToClientMessage::ClientLock(ServerClientLockPayload {
            command: CommandContext {
                expected_revision: 7,
                idempotency_key: "srv-lock-1".to_owned(),
            },
            lock_state: ClientLockState::Locked,
        }),
    )]);
    drive_until(&mut daemon, |daemon| {
        !sim.frames_with_kind("client.command_ack").is_empty()
            && daemon.status().client_lock_commands_applied >= 1
    });

    let policy = daemon.connection_policy().expect("durable policy");
    assert!(!policy.accepting_connections);
    assert_eq!(policy.lock_state, ClientLockState::Locked);

    // The server command is acknowledged with client.command_ack, echoing
    // the command's message id.
    let acks = sim.frames_with_kind("client.command_ack");
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(
        acks[0].frame["payload"]["commandKind"],
        "client.client_lock"
    );
    assert_eq!(acks[0].frame["payload"]["commandMessageId"], "srv-down-1");
    assert_eq!(acks[0].frame["payload"]["status"], "accepted");

    // Unlock via the same command path.
    sim.queue_downlink(vec![downlink_envelope(
        2,
        ServerToClientMessage::ClientLock(ServerClientLockPayload {
            command: CommandContext {
                expected_revision: 8,
                idempotency_key: "srv-lock-2".to_owned(),
            },
            lock_state: ClientLockState::Unlocked,
        }),
    )]);
    drive_until(&mut daemon, |daemon| {
        sim.frames_with_kind("client.command_ack").len() >= 2
            && daemon.status().client_lock_commands_applied >= 2
    });
    let policy = daemon.connection_policy().expect("durable policy");
    assert!(policy.accepting_connections);
    assert_eq!(policy.lock_state, ClientLockState::Unlocked);
    assert_eq!(sim.frames_with_kind("client.command_ack").len(), 2);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn unknown_lane_commands_are_counted_and_skipped_without_blocking_the_cursor() {
    let root = temporary_directory("daemon-unhandled");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("unhandled", &root, &sim);
    daemon.publish_connect_code().expect("publication");

    // A rescan command (owned by a later lane) between two challenges.
    let published = daemon.connect_code_state().expect("state").expect("row");
    let challenge = challenge_payload(&published);
    sim.queue_downlink(vec![
        downlink_envelope(
            1,
            ServerToClientMessage::RepositoryRescan(ServerRepositoryRescanPayload {
                command: CommandContext {
                    expected_revision: 3,
                    idempotency_key: "srv-rescan-1".to_owned(),
                },
                repository_binding_id: "rb_01j2".to_owned(),
                reason: ClientRepositoryRescanReason::Policy,
            }),
        ),
        downlink_envelope(
            2,
            ServerToClientMessage::AccessChallenge(Box::new(challenge.clone())),
        ),
    ]);
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.access.challenge_ack")
            .is_empty()
    });

    assert_eq!(
        daemon.status().unhandled_downlink_commands,
        1,
        "the later-lane command was counted without acting"
    );
    // The cursor advanced across the skipped frame: the follow-up challenge
    // at sequence 2 was still accepted and answered.
    let cursor = daemon
        .store_mut()
        .inbox_cursor(daemon_config("unhandled").server_profile_id.as_str())
        .expect("cursor read")
        .expect("cursor row");
    assert_eq!(cursor.last_sequence, 2);
    let acks = sim.frames_with_kind("client.access.challenge_ack");
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].frame["payload"]["status"], "confirmed");

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn publication_before_enrollment_is_refused() {
    let root = temporary_directory("publish-unenrolled");
    let mut store = DeviceStore::open(&root).expect("store opens");
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).expect("identity");
    let sim = Arc::new(ServerSim::new());
    let mut daemon =
        DeviceDaemon::start(daemon_config("unenrolled"), store, sim.clone(), &identity)
            .expect("daemon starts in the enrollment phase");
    let error = daemon
        .publish_connect_code()
        .expect_err("a placeholder-stream publication must be refused");
    let message = error.to_string();
    assert!(
        message.contains("enrollment"),
        "the refusal names the missing enrollment: {message}"
    );
    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

// The daemon-side challenge answer path constructs its ack through
// `connect_code::challenge_ack_message`; pin its message shape here.
#[test]
fn challenge_ack_message_shape_is_stable() {
    let challenge = ServerAccessChallengePayload {
        challenge_id: "cac_AAAAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
        connect_code_id: "cct_AAAAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
        code_digest: "sha256:aa11".to_owned(),
        expires_at: "2026-09-04T00:05:00.000Z".to_owned(),
        requester_user_id: "usr_01j2".to_owned(),
    };
    let confirmed = connect_code::challenge_ack_message(&challenge, ChallengeVerdict::Confirmed);
    let value = serde_json::to_value(&confirmed).expect("message value");
    assert_eq!(value["kind"], "client.access.challenge_ack");
    assert_eq!(value["payload"]["challengeId"], challenge.challenge_id);
    assert_eq!(value["payload"]["connectCodeId"], challenge.connect_code_id);
    assert_eq!(value["payload"]["status"], "confirmed");
    assert_eq!(
        value["payload"]["idempotencyKey"], "challenge-ack-cac_AAAAAAAAAAAAAAAAAAAAAAAAAA1",
        "the idempotency key is deterministic per challenge"
    );

    let rejected = connect_code::challenge_ack_message(&challenge, ChallengeVerdict::Locked);
    let value = serde_json::to_value(&rejected).expect("message value");
    assert_eq!(value["payload"]["status"], "stale_generation");

    // The published message shape: digest and expiry only, no plaintext
    // field at all.
    let record = ConnectCodeStateRecord {
        connect_code_id: "cct_AAAAAAAAAAAAAAAAAAAAAAAAAA2".to_owned(),
        code_digest: "sha256:bb22".to_owned(),
        generation: 3,
        issued_by_instance_id: "cix_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
        expires_at: "2026-09-04T00:02:00.000Z".to_owned(),
        state: ConnectCodeState::Active,
        created_at: STAMP.to_owned(),
        updated_at: STAMP.to_owned(),
    };
    let published = connect_code::published_message(&record);
    let value = serde_json::to_value(&published).expect("message value");
    assert_eq!(value["kind"], "client.connect_code.published");
    assert_eq!(value["payload"]["codeDigest"], "sha256:bb22");
    assert_eq!(value["payload"]["expiresAt"], record.expires_at);
    assert_eq!(
        value["payload"].as_object().expect("payload object").len(),
        5,
        "exactly the command context plus the three payload fields"
    );
    assert!(matches!(
        published,
        ClientToServerMessage::ConnectCodePublished(_)
    ));
}
