// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientPlatformTarget, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateRetainedPayload, ClientToServerEnvelope, ClientToServerMessage,
};
use winwincode_device_client::candidate_registry::{
    CandidateRefVerdict, CandidateRegistryErrorKind, CandidateRetention,
};
use winwincode_device_client::{
    CandidateLocalRefRecord, CandidateRetentionOutcome, DaemonConfig, DeviceDaemon,
    DeviceIdentitySeed, DeviceStore, ExchangeTransport, ExchangeTransportError, IdentityRecord,
    IssuedEnrollment, OccupancyMirrorUpdate, PathMappingRecord, adopt_enrollment,
    candidate_local_ref, enqueue_candidate_retained, ensure_device_identity, load_device_identity,
    reconcile_retained_candidates, record_candidate_retention, retain_candidate,
    retained_candidates,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-candidates-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn cleanup(root: &Path) {
    fs::remove_dir_all(root).expect("temporary directory should be released");
}

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

const ASSIGNED_NODE: &str = "cnd_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "0123456789";
const STAMP: &str = "2026-09-04T00:00:00.000Z";
const LEASE: &str = "ocl_CANDIDATE000000000000000000000";
const BINDING: &str = "rbd_CANDIDATEBINDING00000000000000";
const SESSION: &str = "wks_CANDIDATESESSION0000000000000";
const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const COMMIT: &str = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
const CANDIDATE_REF_NAME: &str =
    "refs/winwincode/candidates/0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";

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
        },
        STAMP,
    )
    .expect("enrollment adoption");
    let record = load_device_identity(&store)
        .expect("identity reload")
        .expect("enrolled identity");
    (store, record)
}

fn open_resumed(root: &Path) -> (DeviceStore, IdentityRecord) {
    let store = DeviceStore::open(root).expect("restarted device store should open");
    let record = load_device_identity(&store)
        .expect("identity read")
        .expect("enrolled identity");
    (store, record)
}

struct NeverTransport;

impl ExchangeTransport for NeverTransport {
    fn exchange(&self, _request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        Err(ExchangeTransportError::new(
            "the candidate registry must not exchange on its own",
        ))
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
            max_concurrent_worker_sessions: 2,
            running_worker_sessions: 0,
            reserved_worker_sessions: 0,
            draining_worker_sessions: 0,
        },
    }
}

fn started_daemon(name: &str, root: &Path) -> DeviceDaemon {
    let (store, identity) = open_enrolled(root);
    DeviceDaemon::start(
        daemon_config(name),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start enrolled")
}

fn resumed_daemon(name: &str, root: &Path) -> DeviceDaemon {
    let (store, identity) = open_resumed(root);
    DeviceDaemon::start(
        daemon_config(name),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("restarted daemon should start enrolled")
}

fn mirror_the_lease(store: &mut DeviceStore) {
    store
        .advance_occupancy_mirror(&OccupancyMirrorUpdate {
            occupancy_lease_id: LEASE.to_owned(),
            fencing_token: 3,
            holder_user_id: Some("usr_HOLDER0000000000000000000".to_owned()),
            claim_request_id: Some("ocq_CLAIM0000000000000000000000".to_owned()),
            idle_expires_at: None,
            acknowledged_at: STAMP.to_owned(),
        })
        .expect("the occupancy mirror should advance");
}

fn retention(
    commit: &str,
    base_commit: &str,
    binding: &str,
    session: &str,
    stamp: &str,
) -> CandidateRetention {
    CandidateRetention {
        candidate_commit: commit.to_owned(),
        base_commit: base_commit.to_owned(),
        repository_binding_id: binding.to_owned(),
        worker_session_id: session.to_owned(),
        local_git_ref: format!("refs/winwincode/candidates/{commit}"),
        retained_at: stamp.to_owned(),
    }
}

fn retention_for_commit(commit: &str) -> CandidateRetention {
    retention(commit, BASE, BINDING, SESSION, STAMP)
}

fn pending_frames(store: &mut DeviceStore) -> Vec<(String, Value)> {
    store
        .pending_outbox_envelopes()
        .expect("the outbox should be readable")
        .into_iter()
        .map(|entry| {
            let frame: Value =
                serde_json::from_slice(&entry.payload).expect("the stored envelope decodes");
            (entry.kind, frame)
        })
        .collect()
}

fn retained_frames(store: &mut DeviceStore) -> Vec<(u64, String, ClientCandidateRetainedPayload)> {
    pending_frames(store)
        .into_iter()
        .filter(|(kind, _)| kind == "client.candidate.retained")
        .map(|(_, frame)| {
            let envelope: ClientToServerEnvelope =
                serde_json::from_value(frame).expect("the retained frame decodes");
            let sequence = envelope.sequence;
            let client_node_id = envelope.client_node_id;
            match envelope.message {
                ClientToServerMessage::CandidateRetained(payload) => {
                    (sequence, client_node_id, payload)
                }
                _ => panic!("the retained frame must carry the retained payload"),
            }
        })
        .collect()
}

fn git(repository: &Path, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args(arguments);
    let output = command.output().expect("git should run");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

fn init_repository(name: &str) -> (PathBuf, String) {
    let root = temporary_directory(name);
    fs::create_dir_all(&root).expect("repository directory should be created");
    git(&root, &["init", "-q", "--initial-branch=main"]);
    fs::write(root.join("file.txt"), format!("base {name}\n"))
        .expect("seed file should be written");
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Candidate Registry",
            "-c",
            "user.email=registry@example.test",
            "commit",
            "-q",
            "-a",
            "-m",
            "base",
        ],
    );
    let commit = git(&root, &["rev-parse", "HEAD"]);
    (root, commit)
}

fn add_commit(repository: &Path, message: &str) -> String {
    fs::write(repository.join("file.txt"), format!("{message}\n")).expect("file should be written");
    git(
        repository,
        &[
            "-c",
            "user.name=Candidate Registry",
            "-c",
            "user.email=registry@example.test",
            "commit",
            "-q",
            "-a",
            "-m",
            message,
        ],
    );
    git(repository, &["rev-parse", "HEAD"])
}

#[test]
fn a_retention_round_trips_and_survives_a_restart_for_recovery() {
    let root = temporary_directory("round-trip");
    let mut store = DeviceStore::open(&root).expect("store should open");
    let outcome = record_candidate_retention(&mut store, &retention_for_commit(COMMIT))
        .expect("the retention should record");
    let CandidateRetentionOutcome::Recorded(record) = outcome else {
        panic!("a fresh candidate records, it cannot duplicate");
    };
    assert_eq!(
        record,
        CandidateLocalRefRecord {
            candidate_id: COMMIT.to_owned(),
            worker_session_id: SESSION.to_owned(),
            repository_binding_id: BINDING.to_owned(),
            local_git_ref: CANDIDATE_REF_NAME.to_owned(),
            local_state: LocalCandidateState::Retained,
            created_at: STAMP.to_owned(),
            candidate_ref: CANDIDATE_REF_NAME.to_owned(),
            candidate_commit: COMMIT.to_owned(),
            base_commit: BASE.to_owned(),
            retained_at: STAMP.to_owned(),
        },
        "the recorded row carries every registry fact"
    );
    store.close().expect("store should close");

    let store = DeviceStore::open(&root).expect("restarted store should open");
    let recovered = retained_candidates(&store).expect("the retained set should list");
    assert_eq!(recovered, vec![record], "recovery returns the same row");
    assert!(
        candidate_local_ref(&store, "ffffffffffffffffffffffffffffffffffffffff")
            .expect("the lookup should run")
            .is_none(),
        "an unknown candidate has no row"
    );
    store.close().expect("restarted store should close");
    cleanup(&root);
}

#[test]
fn a_repeated_retention_of_the_same_candidate_is_idempotent() {
    let root = temporary_directory("idempotent");
    let mut store = DeviceStore::open(&root).expect("store should open");
    let first = record_candidate_retention(&mut store, &retention_for_commit(COMMIT))
        .expect("the first retention records");
    let replay = record_candidate_retention(
        &mut store,
        &retention(COMMIT, BASE, BINDING, SESSION, "2026-09-04T12:00:00.000Z"),
    )
    .expect("the replay is an idempotent duplicate");
    let CandidateRetentionOutcome::Recorded(original) = first else {
        panic!("the first retention records");
    };
    let CandidateRetentionOutcome::Duplicate(replayed) = replay else {
        panic!("the replay must be a duplicate");
    };
    assert_eq!(
        replayed.created_at, original.created_at,
        "the first recording stamp never rewrites"
    );
    assert_eq!(
        replayed.retained_at, original.retained_at,
        "the first retention stamp never rewrites"
    );
    assert_eq!(
        retained_candidates(&store)
            .expect("the retained set should list")
            .len(),
        1,
        "the same candidate never becomes a second row"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn a_diverging_re_report_fails_closed() {
    let root = temporary_directory("diverging");
    let mut store = DeviceStore::open(&root).expect("store should open");
    record_candidate_retention(&mut store, &retention_for_commit(COMMIT))
        .expect("the retention records");
    for (label, diverging) in [
        (
            "a different repository binding",
            retention(
                COMMIT,
                BASE,
                "rbd_OTHER000000000000000000000000",
                SESSION,
                STAMP,
            ),
        ),
        (
            "a different worker session",
            retention(
                COMMIT,
                BASE,
                BINDING,
                "wks_OTHER00000000000000000000000",
                STAMP,
            ),
        ),
    ] {
        let error = record_candidate_retention(&mut store, &diverging)
            .expect_err("a diverging re-report must fail closed");
        assert_eq!(
            error.kind(),
            CandidateRegistryErrorKind::Conflict,
            "{label} conflicts"
        );
    }
    assert_eq!(
        retained_candidates(&store)
            .expect("the retained set should list")
            .len(),
        1,
        "the diverging reports changed nothing"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn a_malformed_retention_is_refused_without_a_row() {
    let root = temporary_directory("malformed");
    let mut store = DeviceStore::open(&root).expect("store should open");
    for (label, malformed) in [
        (
            "an uppercase commit",
            CandidateRetention {
                candidate_commit: COMMIT.to_uppercase(),
                ..retention_for_commit(COMMIT)
            },
        ),
        (
            "an abbreviated commit",
            CandidateRetention {
                candidate_commit: "0f9e8d7c".to_owned(),
                ..retention_for_commit(COMMIT)
            },
        ),
        (
            "a ref outside the candidate namespace",
            CandidateRetention {
                local_git_ref: "refs/heads/main".to_owned(),
                ..retention_for_commit(COMMIT)
            },
        ),
        (
            "a ref naming another candidate",
            CandidateRetention {
                local_git_ref:
                    "refs/winwincode/candidates/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned(),
                ..retention_for_commit(COMMIT)
            },
        ),
        (
            "an empty binding",
            CandidateRetention {
                repository_binding_id: String::new(),
                ..retention_for_commit(COMMIT)
            },
        ),
    ] {
        let error = record_candidate_retention(&mut store, &malformed)
            .expect_err("a malformed retention must be refused");
        assert_eq!(
            error.kind(),
            CandidateRegistryErrorKind::InvalidInput,
            "{label} is invalid input"
        );
    }
    assert!(
        retained_candidates(&store)
            .expect("the retained set should list")
            .is_empty(),
        "no refused retention left a row"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn the_retain_uplink_stamps_the_frame_with_the_mirrored_lease() {
    let root = temporary_directory("uplink");
    let mut store = DeviceStore::open(&root).expect("store should open");
    mirror_the_lease(&mut store);
    store.close().expect("store should close");

    let mut daemon = started_daemon("uplink", &root);
    let report = retain_candidate(&mut daemon, &retention_for_commit(COMMIT))
        .expect("the retention should record and report");
    let CandidateRetentionOutcome::Recorded(_) = report.outcome else {
        panic!("the fresh candidate records");
    };

    let mut store = daemon.into_store();
    let frames = retained_frames(&mut store);
    assert_eq!(frames.len(), 1, "exactly one retained frame is durable");
    let (sequence, node, payload) = &frames[0];
    assert_eq!(node, ASSIGNED_NODE);
    assert_eq!(
        payload.occupancy.occupancy_lease_id, LEASE,
        "the lease comes from the occupancy mirror"
    );
    assert_eq!(
        payload.occupancy.occupancy_fencing_token, 3,
        "the token comes from the occupancy mirror"
    );
    assert_eq!(
        payload.occupancy.command.expected_revision, 1,
        "the command context carries the mirror revision"
    );
    assert_eq!(
        payload.occupancy.command.idempotency_key,
        format!("candidate-retained-{CANDIDATE_REF_NAME}"),
        "the idempotency key is deterministic per candidate"
    );
    assert_eq!(payload.worker_session_id, SESSION);
    let expected_receipt_id: String = {
        let hex: String = COMMIT
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(25)
            .collect();
        format!("lcr_{}G", hex.to_ascii_uppercase())
    };
    assert_eq!(
        payload.receipt.local_candidate_receipt_id, expected_receipt_id,
        "the receipt id is deterministic per candidate"
    );
    assert_eq!(payload.receipt.candidate_ref, CANDIDATE_REF_NAME);
    assert_eq!(payload.receipt.repository_binding_id, BINDING);
    assert_eq!(payload.receipt.candidate_commit, COMMIT);
    assert_eq!(payload.receipt.local_ref_name, CANDIDATE_REF_NAME);
    assert_eq!(payload.receipt.state, LocalCandidateState::Retained);
    assert_eq!(payload.receipt.created_at, STAMP);
    assert_eq!(
        payload.receipt.revision, 1,
        "the retention receipt is the first revision",
    );
    assert!(
        *sequence > 0,
        "the retained frame rides the strictly advancing stream"
    );

    let frame_text = serde_json::to_string(
        &pending_frames(&mut store)
            .into_iter()
            .find(|(kind, _)| kind == "client.candidate.retained")
            .expect("the retained frame")
            .1,
    )
    .expect("the frame should serialize");
    assert!(
        !frame_text.contains(root.to_str().expect("the root is UTF-8")),
        "no local path may be uploaded: {frame_text}"
    );
    assert!(
        !frame_text.contains("canonicalPath"),
        "no path fact rides the frame"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn the_uplink_fails_closed_without_a_mirror_and_completes_after_recovery() {
    let root = temporary_directory("fail-closed");
    let mut daemon = started_daemon("fail-closed", &root);
    let error = retain_candidate(&mut daemon, &retention_for_commit(COMMIT))
        .expect_err("without a mirror the uplink must fail closed");
    assert_eq!(
        error.kind(),
        CandidateRegistryErrorKind::NoOccupancyMirror,
        "the missing lease is the precise failure"
    );

    let mut store = daemon.into_store();
    assert_eq!(
        retained_candidates(&store)
            .expect("the retained set should list")
            .len(),
        1,
        "the local retention fact is durable"
    );
    assert!(
        retained_frames(&mut store).is_empty(),
        "nothing was reported without a lease"
    );

    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    let mut daemon = resumed_daemon("fail-closed-resume", &root);
    let report = retain_candidate(&mut daemon, &retention_for_commit(COMMIT))
        .expect("the recovery re-report completes");
    let CandidateRetentionOutcome::Duplicate(_) = report.outcome else {
        panic!("the recovery re-report meets the durable row as a duplicate");
    };
    let mut store = daemon.into_store();
    assert_eq!(
        retained_frames(&mut store).len(),
        1,
        "exactly the recovery report is durable"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn a_replayed_retention_reports_the_identical_receipt_under_the_same_key() {
    let root = temporary_directory("replay");
    let mut store = DeviceStore::open(&root).expect("store should open");
    mirror_the_lease(&mut store);
    store.close().expect("store should close");

    let mut daemon = started_daemon("replay", &root);
    let first =
        retain_candidate(&mut daemon, &retention_for_commit(COMMIT)).expect("the first report");
    let replay =
        retain_candidate(&mut daemon, &retention_for_commit(COMMIT)).expect("the replayed report");
    let CandidateRetentionOutcome::Duplicate(_) = replay.outcome else {
        panic!("the replay meets the row as a duplicate");
    };
    assert_ne!(
        first.frame_sequence, replay.frame_sequence,
        "each report has its own stream sequence"
    );

    let mut store = daemon.into_store();
    let frames = retained_frames(&mut store);
    assert_eq!(frames.len(), 2, "both reports are durable");
    let receipt = |payload: &ClientCandidateRetainedPayload| {
        serde_json::to_value(&payload.receipt).expect("the receipt should encode")
    };
    assert_eq!(
        receipt(&frames[0].2),
        receipt(&frames[1].2),
        "the same candidate always reports the identical receipt"
    );
    assert_eq!(
        frames[0].2.occupancy.command.idempotency_key,
        frames[1].2.occupancy.command.idempotency_key,
        "the replay reuses the idempotency key the ledger dedupes on"
    );
    assert_eq!(
        frames[0].0 + 1,
        frames[1].0,
        "the stream sequences strictly advance"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn the_retained_frame_reports_only_retained_candidates() {
    let root = temporary_directory("state-guard");
    let mut daemon = started_daemon("state-guard", &root);
    let outcome = record_candidate_retention(daemon.store_mut(), &retention_for_commit(COMMIT))
        .expect("the retention records");
    let CandidateRetentionOutcome::Recorded(record) = outcome else {
        panic!("the fresh candidate records");
    };
    let mut progressed = record.clone();
    progressed.local_state = LocalCandidateState::Applied;
    let error = enqueue_candidate_retained(&mut daemon, &progressed)
        .expect_err("a progressed candidate is not a retention report");
    assert_eq!(
        error.kind(),
        CandidateRegistryErrorKind::InvalidInput,
        "the retained frame reports only the retained state"
    );
    assert!(
        retained_frames(daemon.store_mut()).is_empty(),
        "the refused report left no frame"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn reconciliation_reports_verified_missing_and_drifted_refs() {
    const VERIFIED_BINDING: &str = "rbd_VERIFIED000000000000000000000000";
    const DRIFTED_BINDING: &str = "rbd_DRIFTED0000000000000000000000000";
    const MISSING_BINDING: &str = "rbd_MISSING0000000000000000000000000";
    const UNMAPPED_BINDING: &str = "rbd_UNMAPPED000000000000000000000000";
    let (verified_repo, verified_commit) = init_repository("verified");
    git(
        &verified_repo,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{verified_commit}"),
            &verified_commit,
        ],
    );
    let (drifted_repo, drifted_commit) = init_repository("drifted");
    let other_commit = add_commit(&drifted_repo, "moved on");
    git(
        &drifted_repo,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{drifted_commit}"),
            &other_commit,
        ],
    );
    let (missing_repo, missing_commit) = init_repository("missing");
    let unmapped_commit = "1234567890123456789012345678901234567890";

    let root = temporary_directory("reconcile");
    let mut store = DeviceStore::open(&root).expect("store should open");
    for (binding, path) in [
        (VERIFIED_BINDING, verified_repo.as_path()),
        (DRIFTED_BINDING, drifted_repo.as_path()),
        (MISSING_BINDING, missing_repo.as_path()),
    ] {
        store
            .put_path_mapping(&PathMappingRecord {
                repository_binding_id: binding.to_owned(),
                canonical_path: path.to_str().expect("the repo path is UTF-8").to_owned(),
                git_common_directory: None,
                last_canonicalized_at: None,
                local_state: "ready".to_owned(),
            })
            .expect("the binding should map");
    }
    for (commit, binding) in [
        (verified_commit.as_str(), VERIFIED_BINDING),
        (drifted_commit.as_str(), DRIFTED_BINDING),
        (missing_commit.as_str(), MISSING_BINDING),
        (unmapped_commit, UNMAPPED_BINDING),
    ] {
        record_candidate_retention(
            &mut store,
            &retention(commit, commit, binding, SESSION, STAMP),
        )
        .expect("the retention should record");
    }

    let mut verdicts = reconcile_retained_candidates(&store).expect("the reconciliation scans");
    verdicts.sort_by(|left, right| left.record.candidate_id.cmp(&right.record.candidate_id));
    assert_eq!(verdicts.len(), 4, "every retained candidate is audited");
    let by_commit = |commit: &str| {
        verdicts
            .iter()
            .find(|row| row.record.candidate_commit == commit)
            .unwrap_or_else(|| panic!("the reconciliation should cover {commit}"))
    };
    assert_eq!(
        by_commit(verified_commit.as_str()).verdict,
        CandidateRefVerdict::Verified,
        "a ref resolving to its recorded commit verifies"
    );
    assert_eq!(
        by_commit(drifted_commit.as_str()).verdict,
        CandidateRefVerdict::Drifted,
        "a ref resolving elsewhere drifted"
    );
    assert_eq!(
        by_commit(drifted_commit.as_str())
            .observed_commit
            .as_deref(),
        Some(other_commit.as_str()),
        "the drifted row names what the ref resolves to now"
    );
    assert_eq!(
        by_commit(missing_commit.as_str()).verdict,
        CandidateRefVerdict::Missing,
        "an absent ref is explicitly missing"
    );
    assert_eq!(
        by_commit(missing_commit.as_str()).observed_commit,
        None::<String>,
        "a missing ref resolves to nothing"
    );
    assert_eq!(
        by_commit(unmapped_commit).verdict,
        CandidateRefVerdict::Missing,
        "a binding without a local checkout counts as missing"
    );

    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&verified_repo);
    cleanup(&drifted_repo);
    cleanup(&missing_repo);
}

#[test]
fn reconciliation_and_recovery_ignore_rows_outside_the_retained_state() {
    let root = temporary_directory("lifecycle");
    let mut store = DeviceStore::open(&root).expect("store should open");
    record_candidate_retention(&mut store, &retention_for_commit(COMMIT))
        .expect("the retention records");
    let connection = Connection::open(store.database_path()).expect("the database should open");
    connection
        .execute(
            "UPDATE candidate_local_refs SET local_state = 'discarded' WHERE candidate_id = ?1",
            [COMMIT],
        )
        .expect("the lifecycle should progress");
    connection.close().expect("the inspection should close");
    assert!(
        retained_candidates(&store)
            .expect("the retained set should list")
            .is_empty(),
        "recovery lists only retained candidates"
    );
    assert!(
        reconcile_retained_candidates(&store)
            .expect("the reconciliation should scan")
            .is_empty(),
        "the reconciliation audits only retained candidates"
    );
    store.close().expect("store should close");
    cleanup(&root);
}

#[test]
fn startup_rejects_a_pre_v6_database_instead_of_migrating() {
    let root = temporary_directory("pre-v6");
    let store = DeviceStore::open(&root).expect("store should open");
    let database_path = store.database_path().to_path_buf();
    store.close().expect("store should close");
    {
        let connection = Connection::open(&database_path).expect("the database should open");
        connection
            .pragma_update(None, "user_version", 5)
            .expect("the test version should be written");
        connection.close().expect("the inspection should close");
    }
    let Err(error) = DeviceStore::open(&root) else {
        panic!("a pre-v6 database must fail closed");
    };
    assert!(
        error.message().contains("unsupported schema version 5"),
        "the failure names the unsupported version: {}",
        error.message()
    );
    cleanup(&root);
}

#[test]
fn expired_exchange_backoff_does_not_stop_the_daemon_or_reserve_unstarted_attempts() {
    let root = temporary_directory("expired-exchange-backoff");
    let (store, identity) = open_enrolled(&root);
    let mut config = daemon_config("expired-exchange-backoff");
    config.initial_backoff = Duration::from_nanos(1);
    config.max_backoff = Duration::from_nanos(1);
    let mut daemon =
        DeviceDaemon::start(config, store, Arc::new(NeverTransport), &identity).unwrap();
    for expected_attempts in 1..=2 {
        let outcome = daemon.tick(std::time::Instant::now()).unwrap();
        assert!(matches!(
            outcome,
            winwincode_device_client::TickOutcome::Retrying { after, .. } if after.is_zero()
        ));
        let journal = Connection::open(root.join("network-requests.sqlite3")).unwrap();
        let (attempts, in_flight): (i64, i64) = journal
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(outcome='in_flight'),0) FROM network_request_attempts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, expected_attempts);
        assert_eq!(in_flight, 0);
        assert_eq!(
            daemon.status().exchanges_started,
            u64::try_from(expected_attempts).unwrap()
        );
    }
    drop(daemon);
    cleanup(&root);
}

#[test]
fn daemon_recovers_a_frozen_candidate_and_reports_exit_after_restart() {
    let (repository, base_commit) = init_repository("worker-freeze");
    let message = serde_json::json!({
        "schemaVersion": 1,
        "repositoryId": "repo_WRITER",
        "sourceCommitId": base_commit,
        "originProvenance": { "workerId": "wrk_WRITER", "workerInstanceId": "wki_WRITER" }
    })
    .to_string();
    let commit = add_commit(&repository, &message);
    git(
        &repository,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{commit}"),
            &commit,
        ],
    );
    let root = temporary_directory("worker-freeze");
    let (mut store, _) = open_enrolled(&root);
    mirror_the_lease(&mut store);
    store
        .put_path_mapping(&PathMappingRecord {
            repository_binding_id: BINDING.into(),
            canonical_path: repository.to_str().unwrap().into(),
            git_common_directory: None,
            last_canonicalized_at: None,
            local_state: "ready".into(),
        })
        .unwrap();
    store
        .put_worker_process(&winwincode_device_client::WorkerProcessRecord {
            worker_session_id: SESSION.into(),
            worker_id: "wrk_WRITER".into(),
            worker_instance_id: "wki_WRITER".into(),
            pid: 1,
            process_start_identity: "observed-exit".into(),
            repository_binding_id: BINDING.into(),
            occupancy_lease_id: LEASE.into(),
            launch_grant_id: "wlg_WRITER".into(),
            data_directory: root.to_str().unwrap().into(),
            state: "exited".into(),
            exit_code: Some(0),
            last_observed_at: STAMP.into(),
        })
        .unwrap();
    drop(store);
    for _ in 0..2 {
        let mut daemon = resumed_daemon("worker-freeze", &root);
        let _ = daemon.tick(std::time::Instant::now()).unwrap();
        let rows = retained_candidates(daemon.store_mut()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].candidate_commit, commit);
        assert_eq!(rows[0].base_commit, base_commit);
        assert!(
            pending_frames(daemon.store_mut())
                .iter()
                .any(|(kind, _)| kind == "client.worker.state")
        );
        drop(daemon);
    }
    cleanup(&root);
    cleanup(&repository);
}
