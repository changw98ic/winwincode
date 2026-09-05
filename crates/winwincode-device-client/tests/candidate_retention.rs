// SPDX-License-Identifier: Apache-2.0

//! GIT-100.9 coverage: the candidate retention policy, the discard vertical,
//! and the garbage collection of expired terminal refs — the fail-closed
//! discard of created branches, the idempotent duplicate discard with the
//! identical receipt, the oldest-first per-binding policy sweep, and the
//! crash-resumable GC that never reclaims a pending-attention or drifted
//! candidate and prunes objects only behind the policy switch. Every
//! Git-facing scenario runs against a real Git repository. Harness mirrors
//! `tests/candidate_branch.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
    LocalCandidateState,
};
use winwincode_client_port::exchange::OutboxSession;
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerEnvelope, ClientToServerMessage,
};
use winwincode_device_client::candidate_branch::{BranchCreationOutcome, BranchCreationRequest};
use winwincode_device_client::candidate_registry::{CandidateRefVerdict, CandidateRetention};
use winwincode_device_client::candidate_retention::{
    CandidateDiscardOutcome, CandidateDiscardRequest, CandidateRetentionErrorKind,
    CandidateRetentionPolicy, GcDeferralReason,
};
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeTransport,
    ExchangeTransportError, IdentityRecord, IssuedEnrollment, OccupancyMirrorUpdate,
    PathMappingRecord, adopt_enrollment, candidate_discard_record, candidate_local_ref,
    collect_expired_candidates, create_candidate_branch, discard_candidate,
    enforce_retention_policy, ensure_device_identity, load_device_identity,
    progress_candidate_lifecycle, reconcile_retained_candidates, record_candidate_retention,
    retain_candidate,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-retention-{name}-{}-{suffix}",
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
const ISSUED_SECRET: [u8; 32] = [0x5d; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";
const MID_STAMP: &str = "2026-09-04T01:00:00.000Z";
const NEW_STAMP: &str = "2026-09-04T02:00:00.000Z";
const RECENT_STAMP: &str = "2026-09-04T11:30:00.000Z";
const NOW_STAMP: &str = "2026-09-04T12:00:00Z";
const LEASE: &str = "ocl_RETENTIONENGINE00000000000000000";
const BINDING: &str = "rbd_RETENTIONBINDING00000000000000";
const FOREIGN_BINDING: &str = "rbd_FOREIGNBINDING000000000000000";
const UNMAPPED_BINDING: &str = "rbd_UNMAPPEDBINDING0000000000000";
const SESSION: &str = "wks_RETENTIONSESSION00000000000000";
const SLUG: &str = "fix-login";

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

fn open_resumed(root: &Path) -> (DeviceStore, IdentityRecord) {
    let store = DeviceStore::open(root).expect("restarted device store should open");
    let record = load_device_identity(&store)
        .expect("identity read")
        .expect("enrolled identity");
    (store, record)
}

struct NeverTransport;

impl ExchangeTransport for NeverTransport {
    fn exchange(
        &self,
        _credential: Option<&str>,
        _request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        Err(ExchangeTransportError::new(
            "the retention suite must not exchange on its own",
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
            fencing_token: 5,
            holder_user_id: Some("usr_HOLDER0000000000000000000".to_owned()),
            claim_request_id: Some("ocq_CLAIM0000000000000000000000".to_owned()),
            idle_expires_at: None,
            acknowledged_at: STAMP.to_owned(),
        })
        .expect("the occupancy mirror should advance");
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW_STAMP, &Rfc3339).expect("the fixed test instant parses")
}

/// A one-hour terminal retention window: `STAMP` is twelve hours before
/// `NOW` (expired) and `RECENT_STAMP` thirty minutes (not yet). The
/// active-candidate limit is two.
fn policy(retention: Duration, prune: bool) -> CandidateRetentionPolicy {
    policy_with(2, retention, prune)
}

fn policy_with(limit: u32, retention: Duration, prune: bool) -> CandidateRetentionPolicy {
    CandidateRetentionPolicy {
        max_active_per_binding: limit,
        terminal_retention: retention,
        prune_objects: prune,
    }
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

/// A non-asserting Git probe: `true` exactly when the command succeeds.
fn git_succeeds(repository: &Path, arguments: &[&str]) -> bool {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.args(arguments);
    command.output().expect("git should run").status.success()
}

/// Whether one full ref name currently resolves.
fn ref_exists(repository: &Path, ref_name: &str) -> bool {
    git_succeeds(repository, &["rev-parse", "--verify", "--quiet", ref_name])
}

/// Creates a repository with one base commit on `main` and returns
/// (path, base commit). The name is folded into the seed content so two
/// repositories created within the same second never share a commit id.
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
            "user.name=Retention Suite",
            "-c",
            "user.email=retention@example.test",
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

/// Creates a follow-up commit with a fixed worker author and returns its id.
fn add_candidate_commit(repository: &Path, message: &str) -> String {
    fs::write(repository.join("file.txt"), format!("{message}\n")).expect("file should be written");
    git(repository, &["add", "-A"]);
    git(
        repository,
        &[
            "-c",
            "user.name=Wanda Worker",
            "-c",
            "user.email=worker@example.test",
            "commit",
            "-q",
            "-a",
            "-m",
            message,
        ],
    );
    git(repository, &["rev-parse", "HEAD"])
}

/// Publishes the stable candidate ref for one frozen commit.
fn write_candidate_ref(repository: &Path, commit: &str) {
    git(
        repository,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{commit}"),
            commit,
        ],
    );
}

fn retention(commit: &str, binding: &str, stamp: &str) -> CandidateRetention {
    CandidateRetention {
        candidate_commit: commit.to_owned(),
        repository_binding_id: binding.to_owned(),
        worker_session_id: SESSION.to_owned(),
        local_git_ref: format!("refs/winwincode/candidates/{commit}"),
        retained_at: stamp.to_owned(),
    }
}

fn candidate_ref(commit: &str) -> String {
    format!("refs/winwincode/candidates/{commit}")
}

fn discard_request(commit: &str, stamp: &str) -> CandidateDiscardRequest {
    CandidateDiscardRequest {
        repository_binding_id: BINDING.to_owned(),
        candidate_ref: candidate_ref(commit),
        requested_at: stamp.to_owned(),
    }
}

/// Maps one binding onto a repository.
fn map_binding(store: &mut DeviceStore, repository: &Path, binding: &str) {
    store
        .put_path_mapping(&PathMappingRecord {
            repository_binding_id: binding.to_owned(),
            canonical_path: repository
                .to_str()
                .expect("the repo path is UTF-8")
                .to_owned(),
            git_common_directory: None,
            last_canonicalized_at: None,
            local_state: "ready".to_owned(),
        })
        .expect("the binding should map");
}

/// The durable outbox frames of one candidate: (sequence, payload).
fn apply_result_frames(store: &mut DeviceStore) -> Vec<(u64, ClientCandidateApplyResultPayload)> {
    store
        .pending_outbox_envelopes()
        .expect("the outbox should be readable")
        .into_iter()
        .filter(|entry| entry.kind == "client.candidate.apply_result")
        .map(|entry| {
            let envelope: ClientToServerEnvelope =
                serde_json::from_slice(&entry.payload).expect("the stored envelope decodes");
            match envelope.message {
                ClientToServerMessage::CandidateApplyResult(payload) => {
                    (envelope.sequence, payload)
                }
                _ => panic!("the apply_result frame must carry the apply_result payload"),
            }
        })
        .collect()
}

/// One decoded JSON text per durable outbox frame — the local-data-boundary
/// scan input.
fn pending_frame_text(store: &mut DeviceStore) -> String {
    store
        .pending_outbox_envelopes()
        .expect("the outbox should be readable")
        .into_iter()
        .map(|entry| {
            serde_json::from_slice::<Value>(&entry.payload)
                .expect("the stored envelope decodes")
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Acknowledges every frame through `sequence` exactly as the daemon's
/// exchange loop does after the server answers.
fn acknowledge_through(store: &mut DeviceStore, sequence: u64) {
    OutboxSession::new()
        .acknowledge(store, sequence)
        .expect("the acknowledgement should persist");
}

/// Progresses one candidate's lifecycle along the contract 6 table.
fn progress_to(store: &mut DeviceStore, commit: &str, target: LocalCandidateState) {
    progress_candidate_lifecycle(store, commit, target).expect("the transition records");
}

/// A scenario bundle: one store root plus one real repository with one base
/// commit and one frozen candidate commit whose stable ref is published.
struct Scenario {
    root: PathBuf,
    repository: PathBuf,
    base_commit: String,
    candidate_commit: String,
}

fn scenario(name: &str) -> Scenario {
    let (repository, base_commit) = init_repository(name);
    let candidate_commit = add_candidate_commit(&repository, &format!("candidate {name}"));
    write_candidate_ref(&repository, &candidate_commit);
    let root = temporary_directory(&format!("{name}-store"));
    Scenario {
        root,
        repository,
        base_commit,
        candidate_commit,
    }
}

/// Seeds mapping, the scenario candidate's retention, and the occupancy
/// mirror, then hands the store to a started daemon.
fn seeded_daemon(name: &str, state: &Scenario) -> DeviceDaemon {
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    map_binding(&mut store, &state.repository, BINDING);
    record_candidate_retention(
        &mut store,
        &retention(&state.candidate_commit, BINDING, STAMP),
    )
    .expect("the retention should record");
    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    started_daemon(name, &state.root)
}

/// Seeds mapping and the occupancy mirror only — for tests that retain
/// their own candidates with explicit stamps.
fn mapped_daemon(name: &str, state: &Scenario) -> DeviceDaemon {
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    map_binding(&mut store, &state.repository, BINDING);
    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    started_daemon(name, &state.root)
}

/// Creates the candidate's local branch through the real branch engine and
/// returns the branch name.
fn create_branch(daemon: &mut DeviceDaemon, commit: &str) -> String {
    let report = create_candidate_branch(
        daemon,
        &BranchCreationRequest {
            repository_binding_id: BINDING.to_owned(),
            candidate_ref: candidate_ref(commit),
            task_slug: SLUG.to_owned(),
            requested_branch_name: None,
            requested_at: STAMP.to_owned(),
        },
    )
    .expect("the branch should be created");
    match report.outcome {
        BranchCreationOutcome::Created(facts) | BranchCreationOutcome::Duplicate(facts) => {
            facts.branch_name
        }
    }
}

const HOUR: Duration = Duration::from_hours(1);

#[test]
fn a_discard_deletes_the_created_branch_and_stamps_the_discarded_uplink() {
    let state = scenario("discard-vertical");
    let mut daemon = seeded_daemon("discard-vertical", &state);
    let branch = create_branch(&mut daemon, &state.candidate_commit);
    let branch_ref = format!("refs/heads/{branch}");
    assert!(
        ref_exists(&state.repository, &branch_ref),
        "the branch engine created the branch"
    );

    let report = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect("the discard should run");
    let CandidateDiscardOutcome::Discarded(facts) = report.outcome else {
        panic!("a fresh discard executes, it cannot be a duplicate");
    };
    assert_eq!(facts.candidate_id, state.candidate_commit);
    assert_eq!(facts.repository_binding_id, BINDING);
    assert_eq!(facts.candidate_ref, candidate_ref(&state.candidate_commit));
    assert_eq!(
        facts.deleted_branch.as_deref(),
        Some(branch.as_str()),
        "the created branch is the discard's deleted branch"
    );

    // The created branch is gone; the stable candidate ref survives until GC.
    assert!(
        !ref_exists(&state.repository, &branch_ref),
        "the created branch must be deleted"
    );
    assert!(
        ref_exists(&state.repository, &candidate_ref(&state.candidate_commit)),
        "the stable candidate ref is not GC's business during a discard"
    );

    // The registry row moved to the terminal discarded state.
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry should read")
        .expect("the candidate row");
    assert_eq!(record.local_state, LocalCandidateState::Discarded);

    // The durable discard record carries the first stamp and the target.
    let durable = candidate_discard_record(daemon.store_mut(), &state.candidate_commit)
        .expect("the discard record should read")
        .expect("the discard record");
    assert_eq!(durable.target_branch, branch);
    assert_eq!(durable.created_at, STAMP);

    // The uplink: one discarded apply_result frame stamped C + L.
    let frames = apply_result_frames(daemon.store_mut());
    assert_eq!(frames.len(), 2, "the branch creation and discard frames");
    let (sequence, payload) = &frames[1];
    let receipt = &payload.receipt;
    assert_eq!(
        receipt.local_apply_receipt_id,
        format!("lar_discard_{}", state.candidate_commit),
        "the discard receipt id is deterministic per candidate"
    );
    assert_eq!(
        receipt.candidate_ref,
        candidate_ref(&state.candidate_commit)
    );
    assert_eq!(receipt.repository_binding_id, BINDING);
    assert_eq!(receipt.target_branch, branch);
    assert_eq!(receipt.expected_head, state.candidate_commit);
    assert_eq!(receipt.strategy, ApplyStrategy::CreateBranch);
    assert_eq!(receipt.result, ApplyResult::Discarded);
    assert_eq!(
        receipt.resulting_commit, None,
        "a discard produces no commit"
    );
    assert_eq!(receipt.conflict_artifact_ref, None);
    assert_eq!(receipt.created_at, STAMP);
    assert_eq!(
        payload.occupancy.occupancy_lease_id, LEASE,
        "the lease comes from the occupancy mirror"
    );
    assert_eq!(payload.occupancy.occupancy_fencing_token, 5);
    assert_eq!(payload.occupancy.command.expected_revision, 1);
    assert_eq!(
        payload.occupancy.command.idempotency_key,
        format!(
            "candidate-discarded-{}",
            candidate_ref(&state.candidate_commit)
        ),
        "the idempotency key is deterministic per candidate"
    );
    assert!(*sequence > 0);

    // Local-data boundary: no path rides any frame.
    let frame_text = pending_frame_text(daemon.store_mut());
    assert!(!frame_text.contains(state.root.to_str().expect("UTF-8 root")));
    assert!(!frame_text.contains(state.repository.to_str().expect("UTF-8 repo")));

    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_discard_without_a_created_branch_reports_the_stable_ref_as_target() {
    let state = scenario("discard-retained");
    let mut daemon = seeded_daemon("discard-retained", &state);
    let report = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect("the discard should run");
    let CandidateDiscardOutcome::Discarded(facts) = report.outcome else {
        panic!("a fresh discard executes");
    };
    assert_eq!(
        facts.deleted_branch, None,
        "a retained-only candidate has no branch to delete"
    );
    let frames = apply_result_frames(daemon.store_mut());
    assert_eq!(frames.len(), 1, "only the discard frame is durable");
    assert_eq!(
        frames[0].1.receipt.target_branch,
        candidate_ref(&state.candidate_commit),
        "the stable local ref is the wire target of a branchless discard"
    );
    assert!(
        ref_exists(&state.repository, &candidate_ref(&state.candidate_commit)),
        "the stable ref survives the discard"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_repeated_discard_is_a_duplicate_reporting_the_identical_receipt() {
    let state = scenario("discard-replay");
    let mut daemon = seeded_daemon("discard-replay", &state);
    let first = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect("the first discard");
    let replay = discard_candidate(
        &mut daemon,
        // A replay may carry a fresh stamp; the first one stays durable.
        &discard_request(&state.candidate_commit, "2026-09-04T09:00:00.000Z"),
    )
    .expect("the replayed discard");
    let CandidateDiscardOutcome::Duplicate(facts) = replay.outcome else {
        panic!("the replay must meet the discard as a duplicate");
    };
    let CandidateDiscardOutcome::Discarded(original) = first.outcome else {
        panic!("the first discard executes");
    };
    assert_eq!(facts, original, "the duplicate returns the original facts");

    let frames = apply_result_frames(daemon.store_mut());
    assert_eq!(frames.len(), 2, "both reports are durable");
    let receipt = |payload: &ClientCandidateApplyResultPayload| {
        serde_json::to_value(&payload.receipt).expect("the receipt should encode")
    };
    assert_eq!(
        receipt(&frames[0].1),
        receipt(&frames[1].1),
        "the same discard always reports the identical receipt"
    );
    assert_eq!(
        frames[0].1.occupancy.command.idempotency_key,
        frames[1].1.occupancy.command.idempotency_key,
        "the replay reuses the idempotency key the ledger dedupes on"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn applied_unknown_and_foreign_discards_fail_closed() {
    let state = scenario("discard-refusals");
    let mut daemon = seeded_daemon("discard-refusals", &state);

    // An applied candidate is a delivered fact: discard refuses.
    progress_to(
        daemon.store_mut(),
        &state.candidate_commit,
        LocalCandidateState::Applied,
    );
    let error = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect_err("an applied candidate must refuse the discard");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::Conflict);
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry should read")
        .expect("the candidate row");
    assert_eq!(
        record.local_state,
        LocalCandidateState::Applied,
        "the refusal changed nothing"
    );
    assert!(apply_result_frames(daemon.store_mut()).is_empty());

    // An unknown candidate fails closed with the missing verdict.
    let unknown = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let error = discard_candidate(&mut daemon, &discard_request(unknown, STAMP))
        .expect_err("an unknown candidate must refuse");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::CandidateMissing);

    // A foreign binding refuses before anything is read from Git.
    let error = discard_candidate(
        &mut daemon,
        &CandidateDiscardRequest {
            repository_binding_id: FOREIGN_BINDING.to_owned(),
            candidate_ref: candidate_ref(&state.candidate_commit),
            requested_at: STAMP.to_owned(),
        },
    )
    .expect_err("a foreign binding must refuse");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::Conflict);

    // A malformed candidate reference is refused as invalid input.
    let error = discard_candidate(
        &mut daemon,
        &CandidateDiscardRequest {
            repository_binding_id: BINDING.to_owned(),
            candidate_ref: "refs/heads/main".to_owned(),
            requested_at: STAMP.to_owned(),
        },
    )
    .expect_err("a malformed reference must refuse");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::InvalidInput);

    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_drifted_or_checked_out_created_branch_refuses_the_discard() {
    // Drifted: the branch no longer holds the candidate commit.
    let state = scenario("discard-drifted");
    let mut daemon = seeded_daemon("discard-drifted", &state);
    let branch = create_branch(&mut daemon, &state.candidate_commit);
    git(
        &state.repository,
        &[
            "update-ref",
            &format!("refs/heads/{branch}"),
            &state.base_commit,
        ],
    );
    let error = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect_err("a drifted branch must refuse the discard");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::Conflict);
    assert!(
        ref_exists(&state.repository, &format!("refs/heads/{branch}")),
        "the drifted branch stays untouched"
    );
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry should read")
        .expect("the candidate row");
    assert_eq!(
        record.local_state,
        LocalCandidateState::BranchCreated,
        "the refusal leaves the candidate retryable"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);

    // Checked out: the branch is the user's current checkout.
    let state = scenario("discard-checked-out");
    let mut daemon = seeded_daemon("discard-checked-out", &state);
    let branch = create_branch(&mut daemon, &state.candidate_commit);
    git(&state.repository, &["checkout", "-q", &branch]);
    let error = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect_err("a checked-out branch must refuse the discard");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::Conflict);
    assert!(
        ref_exists(&state.repository, &format!("refs/heads/{branch}")),
        "the checked-out branch stays untouched"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_discard_fails_closed_without_an_occupancy_mirror() {
    let state = scenario("discard-no-mirror");
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    map_binding(&mut store, &state.repository, BINDING);
    record_candidate_retention(
        &mut store,
        &retention(&state.candidate_commit, BINDING, STAMP),
    )
    .expect("the retention should record");
    store.close().expect("store should close");

    // No mirror was ever persisted for this store.
    let mut daemon = started_daemon("discard-no-mirror", &state.root);
    let error = discard_candidate(
        &mut daemon,
        &discard_request(&state.candidate_commit, STAMP),
    )
    .expect_err("without a mirror the discard must fail closed");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::NoOccupancyMirror);

    let mut store = daemon.into_store();
    let record = candidate_local_ref(&store, &state.candidate_commit)
        .expect("the registry should read")
        .expect("the candidate row");
    assert_eq!(
        record.local_state,
        LocalCandidateState::Retained,
        "the refusal leaves the retention untouched"
    );
    assert!(
        apply_result_frames(&mut store).is_empty(),
        "nothing was reported without a lease"
    );
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn the_retention_policy_discards_the_oldest_active_beyond_the_limit() {
    let state = scenario("policy-sweep");
    let first = add_candidate_commit(&state.repository, "first candidate");
    let second = add_candidate_commit(&state.repository, "second candidate");
    write_candidate_ref(&state.repository, &first);
    write_candidate_ref(&state.repository, &second);
    let mut daemon = mapped_daemon("policy-sweep", &state);
    record_candidate_retention(daemon.store_mut(), &retention(&first, BINDING, STAMP))
        .expect("the oldest retention records");
    record_candidate_retention(daemon.store_mut(), &retention(&second, BINDING, MID_STAMP))
        .expect("the middle retention records");
    record_candidate_retention(
        daemon.store_mut(),
        &retention(&state.candidate_commit, BINDING, NEW_STAMP),
    )
    .expect("the newest retention records");

    let report = enforce_retention_policy(&mut daemon, BINDING, &policy(HOUR, false))
        .expect("the sweep should run");
    assert_eq!(report.repository_binding_id, BINDING);
    assert_eq!(report.limit, 2);
    assert_eq!(report.active_before, 3);
    assert_eq!(report.active_remaining, 2);
    assert_eq!(
        report
            .discarded
            .iter()
            .map(|facts| facts.candidate_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.as_str()],
        "only the oldest active candidate is auto-discarded"
    );
    assert_eq!(
        report.discarded[0].deleted_branch, None,
        "the policy discard reports its branchless shape"
    );

    let store = daemon.into_store();
    let oldest = candidate_local_ref(&store, &first)
        .expect("the registry should read")
        .expect("the oldest row");
    assert_eq!(
        oldest.local_state,
        LocalCandidateState::Discarded,
        "the policy discard is a real discard"
    );
    for survivor in [&second, &state.candidate_commit] {
        let record = candidate_local_ref(&store, survivor)
            .expect("the registry should read")
            .expect("the survivor row");
        assert_eq!(
            record.local_state,
            LocalCandidateState::Retained,
            "within-limit candidates stay active"
        );
    }
    store.close().expect("store should close");

    // The sweep is idempotent: a re-run finds exactly the limit and stops.
    let mut daemon = resumed_daemon("policy-sweep-resume", &state.root);
    let report = enforce_retention_policy(&mut daemon, BINDING, &policy(HOUR, false))
        .expect("the resumed sweep should run");
    assert_eq!(report.active_before, 2);
    assert!(
        report.discarded.is_empty(),
        "a within-limit sweep is a no-op"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn failed_and_terminal_rows_are_never_counted_by_the_policy() {
    let state = scenario("policy-classes");
    let failed = add_candidate_commit(&state.repository, "failed candidate");
    let applied = add_candidate_commit(&state.repository, "applied candidate");
    write_candidate_ref(&state.repository, &failed);
    write_candidate_ref(&state.repository, &applied);
    let mut daemon = mapped_daemon("policy-classes", &state);
    record_candidate_retention(daemon.store_mut(), &retention(&failed, BINDING, STAMP))
        .expect("the failed retention records");
    record_candidate_retention(daemon.store_mut(), &retention(&applied, BINDING, MID_STAMP))
        .expect("the applied retention records");
    record_candidate_retention(
        daemon.store_mut(),
        &retention(&state.candidate_commit, BINDING, NEW_STAMP),
    )
    .expect("the active retention records");
    progress_to(daemon.store_mut(), &failed, LocalCandidateState::Failed);
    progress_to(daemon.store_mut(), &applied, LocalCandidateState::Applied);

    // One active candidate, limit one: nothing is discarded, whatever the
    // failed and applied rows look like.
    let report = enforce_retention_policy(&mut daemon, BINDING, &policy(HOUR, false))
        .expect("the sweep should run");
    assert_eq!(report.active_before, 1);
    assert!(
        report.discarded.is_empty(),
        "failed and applied rows never count"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_policy_sweep_stops_fail_closed_when_a_discard_is_refused() {
    let state = scenario("policy-fail-closed");
    let oldest = add_candidate_commit(&state.repository, "oldest candidate");
    write_candidate_ref(&state.repository, &oldest);
    let mut daemon = mapped_daemon("policy-fail-closed", &state);
    record_candidate_retention(daemon.store_mut(), &retention(&oldest, BINDING, STAMP))
        .expect("the oldest retention records");
    record_candidate_retention(
        daemon.store_mut(),
        &retention(&state.candidate_commit, BINDING, NEW_STAMP),
    )
    .expect("the newest retention records");
    // The oldest candidate's branch drifted, so its discard must refuse.
    let branch = create_branch(&mut daemon, &oldest);
    git(
        &state.repository,
        &[
            "update-ref",
            &format!("refs/heads/{branch}"),
            &state.base_commit,
        ],
    );

    let error = enforce_retention_policy(&mut daemon, BINDING, &policy_with(1, HOUR, false))
        .expect_err("a refused discard must stop the sweep");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::Conflict);
    let record = candidate_local_ref(daemon.store_mut(), &oldest)
        .expect("the registry should read")
        .expect("the oldest row");
    assert_eq!(
        record.local_state,
        LocalCandidateState::BranchCreated,
        "the sweep leaves every row exactly as it found them"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

/// The prepared GC matrix: five terminal candidates (one settled-expired,
/// one retention-pending, one pending-attention with an unacked frame, one
/// drifted, one checkout-unavailable), one non-terminal candidate, and a
/// stray ref no registry row owns.
struct GcMatrix {
    state: Scenario,
    store: DeviceStore,
    applied_past: String,
    applied_recent: String,
    discarded_unacked: String,
    discarded_drifted: String,
    applied_unmapped: String,
    retained_past: String,
}

impl GcMatrix {
    fn close(self) -> (Scenario, DeviceStore) {
        (self.state, self.store)
    }
}

/// Builds the GC matrix exactly as its doc comment describes.
fn gc_matrix(name: &str) -> GcMatrix {
    let state = scenario(name);
    let commits = [
        add_candidate_commit(&state.repository, "applied past"),
        add_candidate_commit(&state.repository, "applied recent"),
        add_candidate_commit(&state.repository, "discarded unacked"),
        add_candidate_commit(&state.repository, "discarded drifted"),
        add_candidate_commit(&state.repository, "applied unmapped"),
        add_candidate_commit(&state.repository, "retained past"),
    ];
    let [
        applied_past,
        applied_recent,
        discarded_unacked,
        discarded_drifted,
        applied_unmapped,
        retained_past,
    ] = commits.each_ref().map(String::as_str);
    for commit in commits.iter().take(4) {
        write_candidate_ref(&state.repository, commit);
    }
    write_candidate_ref(&state.repository, retained_past);
    // A stray ref inside the namespace that no registry row owns: another
    // device sharing this repository could have written it.
    write_candidate_ref(&state.repository, &state.base_commit);

    let mut store = DeviceStore::open(&state.root).expect("store should open");
    map_binding(&mut store, &state.repository, BINDING);
    mirror_the_lease(&mut store);
    store.close().expect("store should close");

    // Retain the four candidates that need upstream frames, then acknowledge
    // exactly the first three — the unacked candidate's frame keeps its
    // pending attention. The unmapped candidate is retained against a
    // binding that maps no local checkout.
    let mut daemon = started_daemon(name, &state.root);
    for (commit, binding, stamp) in [
        (applied_past, BINDING, STAMP),
        (discarded_drifted, BINDING, STAMP),
        (applied_unmapped, UNMAPPED_BINDING, STAMP),
        (discarded_unacked, BINDING, STAMP),
    ] {
        retain_candidate(&mut daemon, &retention(commit, binding, stamp))
            .expect("the retention should record and report");
    }
    let mut store = daemon.into_store();
    acknowledge_through(&mut store, 3);
    store.close().expect("store should close");

    // Progress every terminal candidate and record the frameless ones.
    let mut daemon = resumed_daemon(&format!("{name}-progress"), &state.root);
    record_candidate_retention(
        daemon.store_mut(),
        &retention(applied_recent, BINDING, RECENT_STAMP),
    )
    .expect("the recent retention records");
    record_candidate_retention(
        daemon.store_mut(),
        &retention(retained_past, BINDING, STAMP),
    )
    .expect("the retained retention records");
    for (commit, target) in [
        (applied_past, LocalCandidateState::Applied),
        (applied_recent, LocalCandidateState::Applied),
        (discarded_unacked, LocalCandidateState::Discarded),
        (discarded_drifted, LocalCandidateState::Discarded),
        (applied_unmapped, LocalCandidateState::Applied),
    ] {
        progress_to(daemon.store_mut(), commit, target);
    }
    let store = daemon.into_store();
    // The drifted candidate's ref now resolves elsewhere.
    git(
        &state.repository,
        &[
            "update-ref",
            &candidate_ref(discarded_drifted),
            &state.base_commit,
        ],
    );

    GcMatrix {
        state,
        store,
        applied_past: applied_past.to_owned(),
        applied_recent: applied_recent.to_owned(),
        discarded_unacked: discarded_unacked.to_owned(),
        discarded_drifted: discarded_drifted.to_owned(),
        applied_unmapped: applied_unmapped.to_owned(),
        retained_past: retained_past.to_owned(),
    }
}

/// The six-candidate GC matrix: collected, retention-pending,
/// pending-attention (unacked), drifted, checkout-unavailable, and a
/// non-terminal row the scan must not even examine — plus a stray ref the
/// device never retained.
#[test]
fn gc_collects_expired_terminal_refs_and_defers_everything_else() {
    let mut matrix = gc_matrix("gc-matrix");
    let state = &matrix.state;
    let report = collect_expired_candidates(&mut matrix.store, &policy(HOUR, false), &now())
        .expect("the collection should run");

    assert_eq!(report.examined, 5, "the non-terminal row is never examined");
    assert_eq!(report.collected.len(), 1, "exactly the settled expired ref");
    assert_eq!(report.collected[0].candidate_id, matrix.applied_past);
    assert_eq!(report.collected[0].repository_binding_id, BINDING);
    assert_eq!(
        report.collected[0].candidate_ref,
        candidate_ref(&matrix.applied_past)
    );
    assert!(report.collected[0].ref_was_present);
    assert!(report.pruned_bindings.is_empty(), "pruning is off");

    let deferred = |commit: &str| {
        report
            .deferred
            .iter()
            .find(|row| row.candidate_id == commit)
            .unwrap_or_else(|| panic!("{commit} must be deferred"))
            .reason
    };
    assert_eq!(
        deferred(&matrix.applied_recent),
        GcDeferralReason::RetentionPending
    );
    assert_eq!(
        deferred(&matrix.discarded_unacked),
        GcDeferralReason::PendingUplinkAck
    );
    assert_eq!(
        deferred(&matrix.discarded_drifted),
        GcDeferralReason::RefDrifted
    );
    assert_eq!(
        deferred(&matrix.applied_unmapped),
        GcDeferralReason::CheckoutUnavailable
    );

    // The collected ref is gone; every deferred ref and the stray ref stay.
    assert!(
        !ref_exists(&state.repository, &candidate_ref(&matrix.applied_past)),
        "the expired settled ref must be reclaimed"
    );
    for commit in [
        &matrix.applied_recent,
        &matrix.discarded_unacked,
        &matrix.discarded_drifted,
        &matrix.retained_past,
    ] {
        assert!(
            ref_exists(&state.repository, &candidate_ref(commit)),
            "{commit} must keep its stable ref"
        );
    }
    assert!(
        ref_exists(&state.repository, &candidate_ref(&state.base_commit)),
        "a stray ref this device never retained is never reclaimed"
    );
    // The retained (non-terminal) candidate still reconciles as verified.
    let reconciliations =
        reconcile_retained_candidates(&matrix.store).expect("the reconciliation should scan");
    assert_eq!(reconciliations.len(), 1);
    assert_eq!(reconciliations[0].verdict, CandidateRefVerdict::Verified);

    // The re-run is idempotent: the collected row is skipped, the rest
    // defers exactly the same way.
    let rerun = collect_expired_candidates(&mut matrix.store, &policy(HOUR, false), &now())
        .expect("the resumed collection should run");
    assert_eq!(rerun.already_collected, 1);
    assert!(rerun.collected.is_empty());
    assert_eq!(rerun.deferred.len(), 4);

    let (state, store) = matrix.close();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn gc_resumes_after_a_crash_between_ref_deletion_and_bookkeeping() {
    let state = scenario("gc-crash");
    let mut daemon = seeded_daemon("gc-crash", &state);
    retain_candidate(
        &mut daemon,
        &retention(&state.candidate_commit, BINDING, STAMP),
    )
    .expect("the retention should record and report");
    let mut store = daemon.into_store();
    acknowledge_through(&mut store, 1);
    progress_to(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Applied,
    );

    // Simulate the crash: the ref was already deleted but no collection
    // record was written.
    git(
        &state.repository,
        &["update-ref", "-d", &candidate_ref(&state.candidate_commit)],
    );
    assert!(!ref_exists(
        &state.repository,
        &candidate_ref(&state.candidate_commit)
    ));

    let report = collect_expired_candidates(&mut store, &policy(HOUR, false), &now())
        .expect("the resumed collection should run");
    assert_eq!(report.collected.len(), 1);
    assert!(
        !report.collected[0].ref_was_present,
        "the resumed run sees the ref the crashed run already deleted"
    );
    store.close().expect("store should close");

    // A further run only reports the durable collection.
    let mut store = DeviceStore::open(&state.root).expect("restarted store should open");
    let rerun = collect_expired_candidates(&mut store, &policy(HOUR, false), &now())
        .expect("the idempotent rerun should run");
    assert_eq!(rerun.already_collected, 1);
    assert!(rerun.collected.is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn gc_prunes_objects_only_behind_the_policy_switch() {
    let state = scenario("gc-prune");
    let mut daemon = seeded_daemon("gc-prune", &state);
    retain_candidate(
        &mut daemon,
        &retention(&state.candidate_commit, BINDING, STAMP),
    )
    .expect("the retention should record and report");
    let mut store = daemon.into_store();
    acknowledge_through(&mut store, 1);
    progress_to(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Applied,
    );

    // An unreachable loose object the candidate refs do not hold.
    let dangling = git(
        &state.repository,
        &[
            "-c",
            "user.name=Retention Suite",
            "-c",
            "user.email=retention@example.test",
            "commit-tree",
            "HEAD^{tree}",
            "-m",
            "dangling",
        ],
    );
    assert!(
        git_succeeds(&state.repository, &["cat-file", "-e", &dangling]),
        "the dangling object exists before the collection"
    );

    // The conservative default never prunes.
    collect_expired_candidates(&mut store, &policy(HOUR, false), &now())
        .expect("the collection should run");
    assert!(
        git_succeeds(&state.repository, &["cat-file", "-e", &dangling]),
        "the default policy must not prune any object"
    );
    store.close().expect("store should close");

    // With the switch on, the next collection prunes the repository —
    // here demonstrated on a fresh scenario with the same shapes.
    let state = scenario("gc-prune-on");
    let mut daemon = seeded_daemon("gc-prune-on", &state);
    retain_candidate(
        &mut daemon,
        &retention(&state.candidate_commit, BINDING, STAMP),
    )
    .expect("the retention should record and report");
    let mut store = daemon.into_store();
    acknowledge_through(&mut store, 1);
    progress_to(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Applied,
    );
    let dangling = git(
        &state.repository,
        &[
            "-c",
            "user.name=Retention Suite",
            "-c",
            "user.email=retention@example.test",
            "commit-tree",
            "HEAD^{tree}",
            "-m",
            "dangling",
        ],
    );
    let report = collect_expired_candidates(&mut store, &policy(HOUR, true), &now())
        .expect("the pruning collection should run");
    assert_eq!(report.pruned_bindings, vec![BINDING.to_owned()]);
    assert!(
        !git_succeeds(&state.repository, &["cat-file", "-e", &dangling]),
        "the enabled policy prunes the unreachable object"
    );
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn an_invalid_policy_or_request_fails_closed() {
    let state = scenario("invalid-inputs");
    let mut daemon = seeded_daemon("invalid-inputs", &state);

    for (label, bad) in [
        (
            "a zero limit",
            CandidateRetentionPolicy {
                max_active_per_binding: 0,
                ..policy(HOUR, false)
            },
        ),
        (
            "an instant window",
            CandidateRetentionPolicy {
                terminal_retention: Duration::from_secs(0),
                ..policy(HOUR, false)
            },
        ),
    ] {
        let error = enforce_retention_policy(&mut daemon, BINDING, &bad)
            .expect_err("an invalid policy must refuse the sweep");
        assert_eq!(
            error.kind(),
            CandidateRetentionErrorKind::InvalidInput,
            "{label}"
        );
        let error = collect_expired_candidates(daemon.store_mut(), &bad, &now())
            .expect_err("an invalid policy must refuse the collection");
        assert_eq!(
            error.kind(),
            CandidateRetentionErrorKind::InvalidInput,
            "{label}"
        );
    }

    // An empty binding id refuses the sweep.
    let error = enforce_retention_policy(&mut daemon, "", &policy(HOUR, false))
        .expect_err("an empty binding must refuse");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::InvalidInput);

    // A malformed discard request refuses before anything is read.
    let error = discard_candidate(
        &mut daemon,
        &CandidateDiscardRequest {
            repository_binding_id: BINDING.to_owned(),
            candidate_ref: candidate_ref(&state.candidate_commit),
            requested_at: String::new(),
        },
    )
    .expect_err("an empty stamp must refuse");
    assert_eq!(error.kind(), CandidateRetentionErrorKind::InvalidInput);

    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}
