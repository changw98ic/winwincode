// SPDX-License-Identifier: Apache-2.0

//! GIT-100.3 coverage: the local branch creation engine — the fail-closed
//! validation chain (registry state, occupancy mirror, branch-name
//! vocabulary and conflicts), the stable short-id ladder, the `git branch`
//! creation that never touches the user's checkout and never forges a
//! commit author, the idempotent repeated request that returns the original
//! branch, and the lease-stamped durable `client.candidate.apply_result`
//! uplink. Every Git-facing scenario runs against a real Git repository.
//! Harness mirrors `tests/candidate_registry.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
    LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerEnvelope, ClientToServerMessage,
};
use winwincode_device_client::candidate_branch::{
    BranchCreationOutcome, BranchCreationRequest, CandidateBranchErrorKind,
    WINWINCODE_BRANCH_PREFIX,
};
use winwincode_device_client::{
    CandidateBranchError, CandidateRegistryErrorKind, CandidateRetention, DaemonConfig,
    DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeTransport, ExchangeTransportError,
    IdentityRecord, IssuedEnrollment, OccupancyMirrorUpdate, PathMappingRecord, adopt_enrollment,
    candidate_local_ref, create_candidate_branch, created_branch_record, ensure_device_identity,
    load_device_identity, progress_candidate_lifecycle, record_candidate_retention,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-branch-{name}-{}-{suffix}",
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
const LATER_STAMP: &str = "2026-09-04T12:00:00.000Z";
const LEASE: &str = "ocl_BRANCHENGINE00000000000000000000";
const BINDING: &str = "rbd_BRANCHENGINEBINDING00000000000";
const SESSION: &str = "wks_BRANCHENGINESESSION00000000000";
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
            "the branch engine must not exchange on its own",
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
            "user.name=Branch Engine Suite",
            "-c",
            "user.email=branch@example.test",
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

/// Publishes the stable candidate ref for one frozen commit (the worker
/// freeze path's convention).
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

fn retention(commit: &str, binding: &str) -> CandidateRetention {
    CandidateRetention {
        candidate_commit: commit.to_owned(),
        repository_binding_id: binding.to_owned(),
        worker_session_id: SESSION.to_owned(),
        local_git_ref: format!("refs/winwincode/candidates/{commit}"),
        retained_at: STAMP.to_owned(),
    }
}

/// Maps one binding onto a repository and retains one frozen candidate, the
/// state the branch engine consumes.
fn seed_retained(store: &mut DeviceStore, repository: &Path, commit: &str, binding: &str) {
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
    record_candidate_retention(store, &retention(commit, binding))
        .expect("the retention should record");
}

fn request(candidate_ref: &str, slug: &str, stamp: &str) -> BranchCreationRequest {
    BranchCreationRequest {
        repository_binding_id: BINDING.to_owned(),
        candidate_ref: candidate_ref.to_owned(),
        task_slug: slug.to_owned(),
        requested_branch_name: None,
        requested_at: stamp.to_owned(),
    }
}

fn request_for_commit(commit: &str) -> BranchCreationRequest {
    request(&format!("refs/winwincode/candidates/{commit}"), SLUG, STAMP)
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

/// Seeds the scenario's store (mapping, retention, optionally the occupancy
/// mirror), closes it, and returns the enrolled identity for a daemon start.
fn seed_scenario_store(state: &Scenario, with_mirror: bool) -> (DeviceStore, IdentityRecord) {
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    seed_retained(
        &mut store,
        &state.repository,
        &state.candidate_commit,
        BINDING,
    );
    if with_mirror {
        mirror_the_lease(&mut store);
    }
    store.close().expect("store should close");
    open_enrolled(&state.root)
}

fn apply_result_frames(
    store: &mut DeviceStore,
) -> Vec<(u64, String, ClientCandidateApplyResultPayload)> {
    store
        .pending_outbox_envelopes()
        .expect("the outbox should be readable")
        .into_iter()
        .filter_map(|entry| {
            if entry.kind != "client.candidate.apply_result" {
                return None;
            }
            let frame: Value =
                serde_json::from_slice(&entry.payload).expect("the stored envelope decodes");
            let envelope: ClientToServerEnvelope =
                serde_json::from_value(frame).expect("the apply-result frame decodes");
            let sequence = envelope.sequence;
            let client_node_id = envelope.client_node_id;
            match envelope.message {
                ClientToServerMessage::CandidateApplyResult(payload) => {
                    Some((sequence, client_node_id, payload))
                }
                _ => panic!("the apply-result frame must carry the apply-result payload"),
            }
        })
        .collect()
}

fn winwincode_branches(repository: &Path) -> Vec<String> {
    git(
        repository,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/winwincode/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn branch_head(repository: &Path, branch: &str) -> String {
    git(
        repository,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    )
}

fn commit_author(repository: &Path, commit: &str) -> String {
    git(
        repository,
        &["log", "-1", "--format=%an|%ae|%at|%cn|%ce", commit],
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_branch_creation_round_trips_without_touching_the_checkout() {
    let state = scenario("round-trip");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("round-trip"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    let head_before = git(&state.repository, &["rev-parse", "HEAD"]);
    let status_before = git(&state.repository, &["status", "--porcelain"]);

    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the branch should be created and reported");
    let expected_branch = format!(
        "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
        &state.candidate_commit[..7]
    );
    let BranchCreationOutcome::Created(facts) = &report.outcome else {
        panic!("a fresh candidate creates its branch");
    };
    assert_eq!(
        facts.branch_name, expected_branch,
        "the derived name uses the short id"
    );
    assert_eq!(facts.branch_ref, format!("refs/heads/{expected_branch}"));
    assert_eq!(facts.candidate_id, state.candidate_commit);
    assert_eq!(facts.candidate_commit, state.candidate_commit);
    assert_eq!(facts.repository_binding_id, BINDING);
    assert!(
        report.frame_sequence > 0,
        "the apply-result frame rides the strictly advancing stream"
    );

    // The real repository: the branch exists, points at the candidate, and
    // the user's checkout is untouched.
    assert_eq!(
        branch_head(&state.repository, &expected_branch),
        state.candidate_commit,
        "the branch points exactly at the frozen candidate commit"
    );
    assert_eq!(
        git(&state.repository, &["rev-parse", "HEAD"]),
        head_before,
        "the user's HEAD never moves"
    );
    assert_eq!(
        git(&state.repository, &["status", "--porcelain"]),
        status_before,
        "the user's working tree never changes"
    );
    assert_eq!(
        winwincode_branches(&state.repository),
        vec![expected_branch.clone()],
        "exactly one winwincode branch exists"
    );

    // The registry row progressed to branch_created and the durable record
    // names the branch.
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::BranchCreated);
    let durable = created_branch_record(daemon.store_mut(), &state.candidate_commit)
        .expect("the record should read")
        .expect("the creation is durable");
    assert_eq!(durable.branch_name, expected_branch);
    assert_eq!(durable.created_at, STAMP);
    assert_eq!(durable.repository_binding_id, BINDING);

    // The uplink: exactly one lease-stamped apply-result frame.
    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 1, "exactly one apply-result frame is durable");
    let (sequence, node, payload) = &frames[0];
    assert_eq!(node, ASSIGNED_NODE);
    assert_eq!(sequence, &report.frame_sequence);
    assert_eq!(
        payload.occupancy.occupancy_lease_id, LEASE,
        "the lease comes from the occupancy mirror"
    );
    assert_eq!(
        payload.occupancy.occupancy_fencing_token, 5,
        "the token comes from the occupancy mirror"
    );
    assert_eq!(
        payload.occupancy.command.expected_revision, 1,
        "the command context carries the mirror revision"
    );
    assert_eq!(
        payload.occupancy.command.idempotency_key,
        format!(
            "candidate-branch-created-refs/winwincode/candidates/{}",
            state.candidate_commit
        ),
        "the idempotency key is deterministic per candidate"
    );
    let receipt = &payload.receipt;
    assert_eq!(
        receipt.local_apply_receipt_id,
        format!("lar_branch_{}", state.candidate_commit),
        "the receipt id is deterministic per candidate"
    );
    assert_eq!(
        receipt.candidate_ref,
        format!("refs/winwincode/candidates/{}", state.candidate_commit)
    );
    assert_eq!(receipt.repository_binding_id, BINDING);
    assert_eq!(receipt.target_branch, expected_branch);
    assert_eq!(receipt.expected_head, state.candidate_commit);
    assert_eq!(receipt.strategy, ApplyStrategy::CreateBranch);
    assert_eq!(receipt.result, ApplyResult::BranchCreated);
    assert_eq!(
        receipt.resulting_commit.as_deref(),
        Some(state.candidate_commit.as_str()),
        "the branch head is the resulting commit"
    );
    assert_eq!(receipt.conflict_artifact_ref, None);
    assert_eq!(receipt.created_at, STAMP);
    assert_eq!(receipt.revision, 1);

    // The local-data boundary: no local path rides the frame.
    let frame_text = serde_json::to_string(&payload).expect("the frame should serialize");
    assert!(
        !frame_text.contains(state.repository.to_str().expect("UTF-8")),
        "no local path may be uploaded: {frame_text}"
    );
    assert!(
        !frame_text.contains("canonicalPath"),
        "no path fact rides the frame"
    );

    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_repeated_request_returns_the_original_branch_and_reports_identically() {
    let state = scenario("repeat");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("repeat"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    let first = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the first request creates");
    let BranchCreationOutcome::Created(original) = &first.outcome else {
        panic!("the first request creates");
    };

    // A repeated request with a different slug, a later stamp, and even an
    // explicit name still returns the original branch.
    let mut replay = request_for_commit(&state.candidate_commit);
    replay.task_slug = "renamed-task".to_owned();
    replay.requested_at = LATER_STAMP.to_owned();
    let second = create_candidate_branch(&mut daemon, &replay)
        .expect("the repeated request is an idempotent success");
    let BranchCreationOutcome::Duplicate(returned) = &second.outcome else {
        panic!("the repeated request must meet the original branch");
    };
    assert_eq!(
        returned.branch_name, original.branch_name,
        "the repeated request returns the original branch"
    );
    assert_eq!(
        winwincode_branches(&state.repository),
        vec![original.branch_name.clone()],
        "no second branch is ever created"
    );

    // The durable record keeps the first creation's stamp.
    let durable = created_branch_record(daemon.store_mut(), &state.candidate_commit)
        .expect("the record should read")
        .expect("the creation is durable");
    assert_eq!(durable.created_at, STAMP, "the first stamp never rewrites");

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 2, "both reports are durable");
    let receipt_json = |payload: &ClientCandidateApplyResultPayload| {
        serde_json::to_value(&payload.receipt).expect("the receipt should encode")
    };
    assert_eq!(
        receipt_json(&frames[0].2),
        receipt_json(&frames[1].2),
        "the same creation always reports the identical receipt"
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
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
#[allow(clippy::too_many_lines)]
fn naming_conflicts_resolve_down_the_stable_short_id_ladder() {
    let state = scenario("ladder");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("ladder"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    // Occupy the first three rungs with a foreign branch pointing at the
    // base commit; the engine must walk to the next rung every time and the
    // same request must always resolve to the same name.
    let foreign_branches: Vec<String> = [7, 12, 20]
        .iter()
        .map(|length| {
            format!(
                "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                &state.candidate_commit[..*length]
            )
        })
        .collect();
    for branch in &foreign_branches {
        git(
            &state.repository,
            &["branch", "--end-of-options", branch, &state.base_commit],
        );
    }

    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the ladder resolves to a free rung");
    let BranchCreationOutcome::Created(facts) = &report.outcome else {
        panic!("the ladder rung is free, so the branch is created");
    };
    let expected_branch = format!(
        "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
        state.candidate_commit
    );
    assert_eq!(
        facts.branch_name, expected_branch,
        "the full-commit-id rung is the first free one"
    );
    assert_eq!(
        branch_head(&state.repository, &expected_branch),
        state.candidate_commit
    );

    // Stability: the same inputs resolve to the same branch on a repeat.
    let replay = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the repeat resolves stably");
    let BranchCreationOutcome::Duplicate(returned) = &replay.outcome else {
        panic!("the repeat meets the original branch");
    };
    assert_eq!(returned.branch_name, expected_branch);

    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);

    // Exhausted ladder: when even the full-commit name is occupied by
    // another commit, the engine fails closed with a stable conflict.
    let exhausted = scenario("ladder-exhausted");
    let (store, identity) = seed_scenario_store(&exhausted, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("ladder-exhausted"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    for length in [7, 12, 20, exhausted.candidate_commit.len()] {
        git(
            &exhausted.repository,
            &[
                "branch",
                "--end-of-options",
                &format!(
                    "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                    &exhausted.candidate_commit[..length]
                ),
                &exhausted.base_commit,
            ],
        );
    }
    let error = create_candidate_branch(
        &mut daemon,
        &request_for_commit(&exhausted.candidate_commit),
    )
    .expect_err("an exhausted ladder must fail closed");
    assert_eq!(
        error.kind(),
        CandidateBranchErrorKind::Conflict,
        "the exhausted ladder is a stable conflict"
    );
    assert_eq!(
        winwincode_branches(&exhausted.repository),
        vec![
            format!(
                "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                &exhausted.candidate_commit[..7]
            ),
            format!(
                "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                &exhausted.candidate_commit[..12]
            ),
            format!(
                "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                &exhausted.candidate_commit[..20]
            ),
            format!(
                "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
                exhausted.candidate_commit
            ),
        ],
        "the engine created nothing of its own"
    );
    let record = candidate_local_ref(daemon.store_mut(), &exhausted.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(
        record.local_state,
        LocalCandidateState::Retained,
        "a conflicted creation leaves the candidate retryable"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&exhausted.root);
    cleanup(&exhausted.repository);
}

#[test]
fn a_preexisting_branch_at_the_candidate_is_an_idempotent_success() {
    let state = scenario("preexisting");
    let expected_branch = format!(
        "{WINWINCODE_BRANCH_PREFIX}{SLUG}-{}",
        &state.candidate_commit[..7]
    );
    git(
        &state.repository,
        &[
            "branch",
            "--end-of-options",
            &expected_branch,
            &state.candidate_commit,
        ],
    );
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("preexisting"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("a branch already at the candidate is an idempotent success");
    let BranchCreationOutcome::Duplicate(facts) = &report.outcome else {
        panic!("nothing needed creating");
    };
    assert_eq!(facts.branch_name, expected_branch);
    assert_eq!(
        created_branch_record(daemon.store_mut(), &state.candidate_commit)
            .expect("the record should read")
            .expect("the creation is durable")
            .branch_name,
        expected_branch,
        "the preexisting branch becomes the durable original"
    );
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::BranchCreated);
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn an_explicit_branch_name_conflict_fails_closed() {
    let state = scenario("explicit-conflict");
    let requested = "winwincode/release-notes";
    git(
        &state.repository,
        &["branch", "--end-of-options", requested, &state.base_commit],
    );
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("explicit-conflict"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    let mut request = request_for_commit(&state.candidate_commit);
    request.requested_branch_name = Some(requested.to_owned());
    let error = create_candidate_branch(&mut daemon, &request)
        .expect_err("an occupied explicit name must fail closed");
    assert_eq!(error.kind(), CandidateBranchErrorKind::Conflict);
    assert_eq!(
        branch_head(&state.repository, requested),
        state.base_commit,
        "the occupied branch is untouched"
    );
    assert!(
        winwincode_branches(&state.repository)
            .into_iter()
            .all(|branch| branch == requested),
        "the engine created no branch beside the occupied one"
    );
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::Retained);
    let mut store = daemon.into_store();
    assert!(
        apply_result_frames(&mut store).is_empty(),
        "a local fail-closed verdict reports no frame in this lane"
    );
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_missing_candidate_fails_closed_as_candidate_missing() {
    // (a) The retention row exists but the Git ref is gone.
    let state = scenario("missing-ref");
    git(
        &state.repository,
        &[
            "update-ref",
            "-d",
            &format!("refs/winwincode/candidates/{}", state.candidate_commit),
        ],
    );
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("missing-ref"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let error = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect_err("a missing ref must fail closed");
    assert_eq!(
        error.kind(),
        CandidateBranchErrorKind::CandidateMissing,
        "the contract 8 candidate_missing mapping"
    );
    assert!(winwincode_branches(&state.repository).is_empty());
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::Retained);
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);

    // (b) The candidate was never retained on this device.
    let state = scenario("never-retained");
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    let (store, identity) = open_enrolled(&state.root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("never-retained"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let error = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect_err("an unretained candidate must fail closed");
    assert_eq!(error.kind(), CandidateBranchErrorKind::CandidateMissing);
    assert!(winwincode_branches(&state.repository).is_empty());
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);

    // (c) The binding maps no local checkout.
    let state = scenario("unmapped-binding");
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    record_candidate_retention(&mut store, &retention(&state.candidate_commit, BINDING))
        .expect("the retention should record");
    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    let (store, identity) = open_enrolled(&state.root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("unmapped-binding"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let error = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect_err("an unmapped binding must fail closed");
    assert_eq!(error.kind(), CandidateBranchErrorKind::CandidateMissing);
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn creation_fails_closed_without_a_mirror_and_completes_after_recovery() {
    let state = scenario("no-mirror");
    let (store, identity) = seed_scenario_store(&state, false);
    let mut daemon = DeviceDaemon::start(
        daemon_config("no-mirror"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let error = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect_err("without a mirror the engine must refuse before touching the repo");
    assert_eq!(
        error.kind(),
        CandidateBranchErrorKind::NoOccupancyMirror,
        "the missing lease is the precise failure"
    );
    assert!(
        winwincode_branches(&state.repository).is_empty(),
        "the repository was never touched"
    );
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::Retained);
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());

    // Once occupancy is mirrored, the retry completes the full vertical.
    mirror_the_lease(&mut store);
    store.close().expect("store should close");
    let (store, identity) = open_resumed(&state.root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("no-mirror-recovery"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("restarted daemon should start");
    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the recovery retry creates and reports");
    let BranchCreationOutcome::Created(facts) = &report.outcome else {
        panic!("the recovery retry creates the branch");
    };
    assert_eq!(
        branch_head(&state.repository, &facts.branch_name),
        state.candidate_commit
    );
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn terminal_candidates_refuse_branch_creation() {
    for terminal in [LocalCandidateState::Applied, LocalCandidateState::Discarded] {
        let state = scenario("terminal");
        let mut store = DeviceStore::open(&state.root).expect("store should open");
        seed_retained(
            &mut store,
            &state.repository,
            &state.candidate_commit,
            BINDING,
        );
        mirror_the_lease(&mut store);
        progress_candidate_lifecycle(&mut store, &state.candidate_commit, terminal)
            .expect("the lifecycle should progress to the terminal state");
        store.close().expect("store should close");
        let (store, identity) = open_enrolled(&state.root);
        let mut daemon = DeviceDaemon::start(
            daemon_config("terminal"),
            store,
            Arc::new(NeverTransport),
            &identity,
        )
        .expect("daemon should start");
        let error =
            create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
                .expect_err("a terminal candidate must refuse branch creation");
        assert_eq!(
            error.kind(),
            CandidateBranchErrorKind::Conflict,
            "{terminal:?} is terminal"
        );
        assert!(winwincode_branches(&state.repository).is_empty());
        let mut store = daemon.into_store();
        assert!(apply_result_frames(&mut store).is_empty());
        store.close().expect("store should close");
        cleanup(&state.root);
        cleanup(&state.repository);
    }
}

#[test]
fn a_drifted_candidate_ref_fails_closed() {
    let state = scenario("drifted");
    // Point the candidate ref at the base commit instead of the candidate.
    git(
        &state.repository,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{}", state.candidate_commit),
            &state.base_commit,
        ],
    );
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("drifted"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let error = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect_err("a drifted ref must fail closed");
    assert_eq!(error.kind(), CandidateBranchErrorKind::Conflict);
    assert!(winwincode_branches(&state.repository).is_empty());
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn a_binding_mismatch_fails_closed() {
    let state = scenario("binding");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("binding"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let mut request = request_for_commit(&state.candidate_commit);
    request.repository_binding_id = "rbd_FOREIGN000000000000000000000000".to_owned();
    let error = create_candidate_branch(&mut daemon, &request)
        .expect_err("a foreign binding must fail closed");
    assert_eq!(error.kind(), CandidateBranchErrorKind::Conflict);
    assert!(winwincode_branches(&state.repository).is_empty());
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn the_candidate_commit_author_is_preserved_verbatim() {
    let state = scenario("author");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("author"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    let head_before = git(&state.repository, &["rev-parse", "HEAD"]);
    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the branch should be created");
    let BranchCreationOutcome::Created(facts) = &report.outcome else {
        panic!("the fresh candidate creates");
    };
    // No commit is forged: the branch resolves to exactly the candidate
    // commit, whose author and committer are the worker's, unchanged.
    assert_eq!(
        branch_head(&state.repository, &facts.branch_name),
        state.candidate_commit,
        "the branch is a ref write only, never a new commit"
    );
    assert_eq!(
        commit_author(&state.repository, &facts.branch_name),
        commit_author(&state.repository, &state.candidate_commit),
        "author and committer facts are the candidate commit's own"
    );
    assert!(
        commit_author(&state.repository, &facts.branch_name)
            .starts_with("Wanda Worker|worker@example.test|"),
        "the author is the worker's, never the engine's"
    );
    assert_eq!(
        git(&state.repository, &["rev-parse", "HEAD"]),
        head_before,
        "the user's checkout never moves"
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn the_creation_survives_a_restart_and_a_failed_retry_can_succeed() {
    let state = scenario("restart");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("restart"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let first = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the first creation");
    let BranchCreationOutcome::Created(original) = &first.outcome else {
        panic!("the first creation creates");
    };
    let store = daemon.into_store();
    store.close().expect("store should close");

    // Restart: the durable record and registry state return the original
    // branch without a second creation.
    let (store, identity) = open_resumed(&state.root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("restart-resume"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("restarted daemon should start");
    let replay = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the restarted replay is idempotent");
    let BranchCreationOutcome::Duplicate(returned) = &replay.outcome else {
        panic!("the replay meets the durable record");
    };
    assert_eq!(returned.branch_name, original.branch_name);
    assert_eq!(
        winwincode_branches(&state.repository),
        vec![original.branch_name.clone()]
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);

    // A candidate whose earlier attempt failed (row `failed`, no branch)
    // still creates on retry: contract 6 keeps `failed` retryable.
    let state = scenario("failed-retry");
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    seed_retained(
        &mut store,
        &state.repository,
        &state.candidate_commit,
        BINDING,
    );
    mirror_the_lease(&mut store);
    progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Failed,
    )
    .expect("the lifecycle should progress to failed");
    store.close().expect("store should close");
    let (store, identity) = open_enrolled(&state.root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("failed-retry"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");
    let report = create_candidate_branch(&mut daemon, &request_for_commit(&state.candidate_commit))
        .expect("the retry succeeds");
    let BranchCreationOutcome::Created(facts) = &report.outcome else {
        panic!("the retry creates");
    };
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::BranchCreated);
    assert_eq!(
        branch_head(&state.repository, &facts.branch_name),
        state.candidate_commit
    );
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn malformed_requests_are_refused_without_any_effect() {
    let state = scenario("malformed");
    let (store, identity) = seed_scenario_store(&state, true);
    let mut daemon = DeviceDaemon::start(
        daemon_config("malformed"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start");

    for (label, malformed) in [
        (
            "an uppercase commit",
            request(
                "refs/winwincode/candidates/0F9E8D7C6B5A4938271605F4E3D2C1B0A9988776",
                SLUG,
                STAMP,
            ),
        ),
        (
            "an abbreviated commit",
            request("refs/winwincode/candidates/0f9e8d7c", SLUG, STAMP),
        ),
        (
            "a ref outside the candidate namespace",
            request("refs/heads/main", SLUG, STAMP),
        ),
        (
            "an empty binding",
            BranchCreationRequest {
                repository_binding_id: String::new(),
                ..request_for_commit(&state.candidate_commit)
            },
        ),
        (
            "an empty stamp",
            BranchCreationRequest {
                requested_at: String::new(),
                ..request_for_commit(&state.candidate_commit)
            },
        ),
        (
            "a namespace escape as the explicit name",
            BranchCreationRequest {
                requested_branch_name: Some("main".to_owned()),
                ..request_for_commit(&state.candidate_commit)
            },
        ),
        (
            "an illegal slug with no explicit name",
            BranchCreationRequest {
                task_slug: "../evil".to_owned(),
                ..request_for_commit(&state.candidate_commit)
            },
        ),
    ] {
        let error = create_candidate_branch(&mut daemon, &malformed)
            .expect_err("a malformed request must be refused");
        assert_eq!(
            error.kind(),
            CandidateBranchErrorKind::InvalidInput,
            "{label} is invalid input"
        );
    }
    assert!(
        winwincode_branches(&state.repository).is_empty(),
        "no refused request created a branch"
    );
    let record = candidate_local_ref(daemon.store_mut(), &state.candidate_commit)
        .expect("the registry row should read")
        .expect("the retained candidate");
    assert_eq!(record.local_state, LocalCandidateState::Retained);
    let durable = created_branch_record(daemon.store_mut(), &state.candidate_commit)
        .expect("the record should read");
    assert!(
        durable.is_none(),
        "no refused request left a durable record"
    );
    let mut store = daemon.into_store();
    assert!(apply_result_frames(&mut store).is_empty());
    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}

#[test]
fn the_branch_engine_error_names_a_stable_category_and_message() {
    let error = CandidateBranchError::from({
        // A store failure converts with its category preserved.
        winwincode_device_client::DeviceStoreError::closed()
    });
    assert_eq!(error.kind(), CandidateBranchErrorKind::Store);
    assert!(error.message().contains("closed"));
    assert_eq!(
        error.to_string(),
        format!("Store: {}", error.message()),
        "the display names the category"
    );
}

#[test]
fn the_registry_lifecycle_transition_table_stays_fail_closed() {
    let state = scenario("transitions");
    let mut store = DeviceStore::open(&state.root).expect("store should open");
    seed_retained(
        &mut store,
        &state.repository,
        &state.candidate_commit,
        BINDING,
    );

    // retained -> branch_created is legal; the same-state replay is an
    // accepted no-op.
    let record = progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::BranchCreated,
    )
    .expect("retained -> branch_created is legal");
    assert_eq!(record.local_state, LocalCandidateState::BranchCreated);
    let record = progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::BranchCreated,
    )
    .expect("the same-state replay is an accepted no-op");
    assert_eq!(record.local_state, LocalCandidateState::BranchCreated);

    // branch_created -> retained is not a contract 6 transition.
    let error = progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Retained,
    )
    .expect_err("branch_created -> retained is illegal");
    assert_eq!(error.kind(), CandidateRegistryErrorKind::Conflict);

    // branch_created -> failed is legal, failed -> branch_created is the
    // retry, failed -> failed is an accepted no-op.
    progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Failed,
    )
    .expect("branch_created -> failed is legal");
    progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Failed,
    )
    .expect("the failed -> failed no-op is accepted");
    progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::BranchCreated,
    )
    .expect("failed -> branch_created is the contract 6 retry");

    // Terminal refusal: applied ends the lifecycle.
    progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::Applied,
    )
    .expect("branch_created -> applied is legal");
    let error = progress_candidate_lifecycle(
        &mut store,
        &state.candidate_commit,
        LocalCandidateState::BranchCreated,
    )
    .expect_err("a terminal candidate refuses every move");
    assert_eq!(error.kind(), CandidateRegistryErrorKind::Conflict);

    let error = progress_candidate_lifecycle(
        &mut store,
        "ffffffffffffffffffffffffffffffffffffffff",
        LocalCandidateState::Failed,
    )
    .expect_err("an unknown candidate is refused");
    assert_eq!(error.kind(), CandidateRegistryErrorKind::InvalidInput);

    store.close().expect("store should close");
    cleanup(&state.root);
    cleanup(&state.repository);
}
