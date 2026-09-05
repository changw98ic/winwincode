// SPDX-License-Identifier: Apache-2.0

//! GIT-100.4 coverage: the target-branch safe apply engine over real Git
//! repositories — fast-forward and cherry-pick/merge delivery inside an
//! isolated integration worktree, the `expectedHead` strong checks (preflight
//! and the compare-and-swap ref update), the dirty-policy rejection,
//! conflict isolation away from the user's working tree, `candidate_missing`
//! verdicts, fencing and terminal refusals, and the durable
//! `client.candidate.apply_result` receipt frame. Harness mirrors
//! `tests/candidate_registry.rs`.

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
use winwincode_device_client::apply_engine::{CandidateApplyRequest, apply_candidate_to_branch};
use winwincode_device_client::{
    CandidateApplyErrorKind, CandidateRetention, DaemonConfig, DeviceDaemon, DeviceIdentitySeed,
    DeviceStore, ExchangeTransport, ExchangeTransportError, FencingRejection, IdentityRecord,
    IssuedEnrollment, OccupancyMirrorUpdate, PathMappingRecord, adopt_enrollment,
    candidate_local_ref, ensure_device_identity, load_device_identity, record_candidate_retention,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-apply-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn cleanup(root: &Path) {
    // The integration roots only materialize when an attempt gets that far;
    // a missing directory has nothing to release.
    if root.exists() {
        fs::remove_dir_all(root).expect("temporary directory should be released");
    }
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
const LEASE: &str = "ocl_APPLYENGINE00000000000000000000";
const TOKEN: u64 = 5;
const BINDING: &str = "rbd_APPLYENGINEBINDING000000000000";
const WORKER_SESSION: &str = "wks_APPLYENGINE00000000000000000";

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

/// A transport that must never be called: the apply engine's uplink is a
/// durable outbox append, not an exchange.
struct NeverTransport;

impl ExchangeTransport for NeverTransport {
    fn exchange(
        &self,
        _credential: Option<&str>,
        _request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        Err(ExchangeTransportError::new(
            "the apply engine must not exchange on its own",
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

/// Opens a store that already carries an adopted enrollment (the restart
/// scenario: the identity is durable, never re-adopted).
fn open_resumed(root: &Path) -> (DeviceStore, IdentityRecord) {
    let store = DeviceStore::open(root).expect("restarted device store should open");
    let record = load_device_identity(&store)
        .expect("identity read")
        .expect("enrolled identity");
    (store, record)
}

/// Starts a daemon whose occupancy mirror already holds the lease the apply
/// commands stamp with.
fn started_daemon(name: &str, root: &Path) -> DeviceDaemon {
    {
        let (mut store, _) = open_enrolled(root);
        store
            .advance_occupancy_mirror(&OccupancyMirrorUpdate {
                occupancy_lease_id: LEASE.to_owned(),
                fencing_token: TOKEN,
                holder_user_id: Some("usr_HOLDER0000000000000000000".to_owned()),
                claim_request_id: Some("ocq_CLAIM0000000000000000000000".to_owned()),
                idle_expires_at: None,
                acknowledged_at: STAMP.to_owned(),
            })
            .expect("the occupancy mirror should advance");
        store.close().expect("store should close");
    }
    let (store, identity) = open_resumed(root);
    DeviceDaemon::start(
        daemon_config(name),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .expect("daemon should start enrolled")
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

/// Stages everything and commits on the currently checked-out branch.
fn commit_everything(repository: &Path, message: &str) -> String {
    git(repository, &["add", "-A"]);
    git(
        repository,
        &[
            "-c",
            "user.name=Candidate Author",
            "-c",
            "user.email=author@example.test",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(repository, &["rev-parse", "HEAD"])
}

/// One real repository: `main` and `workspace` both at a base commit, the
/// user checked out on `workspace` (so `main` — the usual apply target — is
/// never checked out anywhere). The user's workspace file content is
/// returned for untouched assertions. The returned path is canonical: the
/// binding's stored mapping must be the canonical spelling, exactly as the
/// registration check chain stores it.
fn init_repository(name: &str) -> (PathBuf, String) {
    let created = temporary_directory(name);
    fs::create_dir_all(&created).expect("repository directory should be created");
    let root = fs::canonicalize(&created).expect("canonical repository path");
    git(&root, &["init", "-q", "--initial-branch=main"]);
    fs::write(root.join("file.txt"), format!("base {name}\n")).expect("seed file");
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Candidate Author",
            "-c",
            "user.email=author@example.test",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    let base = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["branch", "workspace", &base]);
    git(&root, &["checkout", "-q", "workspace"]);
    (root, base)
}

/// Creates the candidate commit on top of `main` and then returns `main` to
/// where it was (the frozen-candidate world: the candidate ref holds the
/// work, the target branch is untouched), returning to the user's
/// `workspace`. Returns the candidate commit id.
fn candidate_commit_on_main(repository: &Path, name: &str, step: &str) -> String {
    let original_main = git(repository, &["rev-parse", "main"]);
    git(repository, &["checkout", "-q", "main"]);
    fs::write(
        repository.join("candidate.txt"),
        format!("candidate work {name} {step}\n"),
    )
    .expect("candidate file");
    let candidate = commit_everything(repository, "the candidate change");
    // Reset `main` (the branch we are on) back to where it was: the
    // candidate ref holds the frozen work, the target branch is untouched.
    git(repository, &["reset", "-q", "--hard", &original_main]);
    git(repository, &["checkout", "-q", "workspace"]);
    candidate
}

/// Writes the stable candidate ref for `candidate_commit`.
fn write_candidate_ref(repository: &Path, candidate_commit: &str) {
    git(
        repository,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{candidate_commit}"),
            candidate_commit,
        ],
    );
}

/// Retains `candidate_commit` in the device registry against `BINDING`.
fn retain_candidate(store: &mut DeviceStore, candidate_commit: &str) {
    record_candidate_retention(
        store,
        &CandidateRetention {
            candidate_commit: candidate_commit.to_owned(),
            repository_binding_id: BINDING.to_owned(),
            worker_session_id: WORKER_SESSION.to_owned(),
            local_git_ref: format!("refs/winwincode/candidates/{candidate_commit}"),
            retained_at: STAMP.to_owned(),
        },
    )
    .expect("the retention should record");
}

fn bind(store: &mut DeviceStore, repository: &Path) {
    store
        .put_path_mapping(&PathMappingRecord {
            repository_binding_id: BINDING.to_owned(),
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

fn request(
    candidate_commit: &str,
    target_branch: &str,
    expected_head: &str,
    strategy: ApplyStrategy,
) -> CandidateApplyRequest {
    CandidateApplyRequest {
        repository_binding_id: BINDING.to_owned(),
        candidate_ref: format!("refs/winwincode/candidates/{candidate_commit}"),
        target_branch: target_branch.to_owned(),
        expected_head: expected_head.to_owned(),
        strategy,
        occupancy_lease_id: LEASE.to_owned(),
        occupancy_fencing_token: TOKEN,
    }
}

fn apply_result_frames(
    store: &mut DeviceStore,
) -> Vec<(u64, String, ClientCandidateApplyResultPayload)> {
    store
        .pending_outbox_envelopes()
        .expect("the outbox should be readable")
        .into_iter()
        .filter(|entry| entry.kind == "client.candidate.apply_result")
        .map(|entry| {
            let frame: Value =
                serde_json::from_slice(&entry.payload).expect("the stored envelope decodes");
            let envelope: ClientToServerEnvelope =
                serde_json::from_value(frame).expect("the apply result frame decodes");
            let (sequence, node) = (envelope.sequence, envelope.client_node_id);
            match envelope.message {
                ClientToServerMessage::CandidateApplyResult(payload) => (sequence, node, payload),
                _ => panic!("the apply result frame must carry the apply result payload"),
            }
        })
        .collect()
}

fn registry_state(store: &mut DeviceStore, candidate_commit: &str) -> LocalCandidateState {
    candidate_local_ref(store, candidate_commit)
        .expect("the registry read")
        .expect("the retained row")
        .local_state
}

/// The user's workspace: clean, on `workspace` at `expected_head`, with the
/// expected file content.
fn user_tree_is_untouched(repository: &Path, expected_head: &str, file_content: &str) {
    assert_eq!(
        fs::read_to_string(repository.join("file.txt")).expect("the user's file"),
        file_content,
        "the user's working file must never change"
    );
    assert_eq!(
        git(repository, &["status", "--porcelain"]),
        "",
        "the user's working tree must stay clean"
    );
    assert_eq!(
        git(repository, &["rev-parse", "workspace"]),
        expected_head,
        "the user's branch never moves"
    );
}

fn integration_attempt_directory(
    integration_root: &Path,
    candidate_commit: &str,
    receipt_id: &str,
) -> PathBuf {
    integration_root
        .join("conflict-artifacts")
        .join(candidate_commit)
        .join(receipt_id)
}

#[test]
fn fast_forward_applies_the_candidate_commit_to_the_target_branch() {
    let (repo, base) = init_repository("fast-forward");
    let integration_root = temporary_directory("fast-forward-root");
    let candidate = candidate_commit_on_main(&repo, "fast-forward", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("fast-forward-store");
    let mut daemon = started_daemon("fast-forward", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the fast-forward apply should settle applied");
    let receipt = &outcome.receipt;
    assert_eq!(receipt.result, ApplyResult::Applied, "the apply applies");
    assert_eq!(
        receipt.resulting_commit.as_deref(),
        Some(candidate.as_str()),
        "a fast-forward resulting commit is exactly the candidate commit"
    );
    assert_eq!(
        receipt.candidate_ref,
        format!("refs/winwincode/candidates/{candidate}")
    );
    assert_eq!(receipt.target_branch, "main");
    assert_eq!(receipt.expected_head, base);
    assert_eq!(receipt.strategy, ApplyStrategy::FastForward);
    assert_eq!(receipt.conflict_artifact_ref, None);
    assert_eq!(receipt.revision, 1);
    assert!(receipt.local_apply_receipt_id.starts_with("lar_"));

    // The target branch really moved; the user's checkout did not.
    assert_eq!(git(&repo, &["rev-parse", "main"]), candidate);
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert!(
        !repo.join("candidate.txt").exists(),
        "the candidate's file exists only on the target branch, not in the user's tree"
    );
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Applied,
        "the registry write-back reports the applied state"
    );
    // The integration worktree left nothing behind.
    let artifacts = integration_root.join("conflict-artifacts");
    assert!(
        !artifacts.exists()
            || artifacts
                .read_dir()
                .is_ok_and(|entries| entries.count() == 0),
        "a successful apply leaves no integration artifacts"
    );
    let worktree_lines = git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktree_lines
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "only the user's worktree remains: {worktree_lines}"
    );

    // The durable apply_result frame carries the receipt under the mirrored
    // lease, with no local path.
    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 1, "exactly one apply result frame is durable");
    let (sequence, node, payload) = &frames[0];
    assert_eq!(node, ASSIGNED_NODE);
    assert!(sequence > &0, "the frame rides the advancing stream");
    assert_eq!(payload.occupancy.occupancy_lease_id, LEASE);
    assert_eq!(payload.occupancy.occupancy_fencing_token, TOKEN);
    assert_eq!(payload.receipt, outcome.receipt);
    let frame_text = serde_json::to_string(&payload).expect("the payload should serialize");
    for local_root in [
        root.to_str().expect("UTF-8"),
        integration_root.to_str().expect("UTF-8"),
        repo.to_str().expect("UTF-8"),
    ] {
        assert!(
            !frame_text.contains(local_root),
            "no local path may ride the frame: {frame_text}"
        );
    }
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn cherry_pick_applies_with_the_original_author_and_device_committer() {
    let (repo, _base) = init_repository("cherry-pick");
    let integration_root = temporary_directory("cherry-pick-root");
    // `main` and the candidate diverge without conflict: main gets its own
    // file, the candidate is a child of the shared base.
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main work\n").expect("main file");
    let expected_head = commit_everything(&repo, "main work");
    git(&repo, &["checkout", "-q", "workspace"]);
    fs::write(repo.join("candidate.txt"), "candidate work\n").expect("candidate file");
    let candidate = commit_everything(&repo, "the candidate change");
    write_candidate_ref(&repo, &candidate);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("cherry-pick-store");
    let mut daemon = started_daemon("cherry-pick", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &request(
            &candidate,
            "main",
            &expected_head,
            ApplyStrategy::CherryPick,
        ),
        &integration_root,
    )
    .expect("the cherry-pick apply should settle applied");
    assert_eq!(outcome.receipt.result, ApplyResult::Applied);
    let resulting = outcome
        .receipt
        .resulting_commit
        .clone()
        .expect("applied carries a commit");
    assert_ne!(
        resulting, candidate,
        "a cherry-pick creates a new commit on the target"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "main"]),
        "the candidate change",
        "the cherry-picked commit keeps the candidate message"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%an <%ae>", "main"]),
        "Candidate Author <author@example.test>",
        "the cherry-pick preserves the original author; it is never fabricated"
    );
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%cn <%ce>", "main"]),
        "WinWinCode Device Client <device-client@winwincode.invalid>",
        "the device client is the honest committer"
    );
    assert_eq!(git(&repo, &["rev-parse", "main"]), resulting);
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Applied
    );

    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn merge_applies_as_a_merge_commit_when_the_target_diverged() {
    let (repo, _base) = init_repository("merge");
    let integration_root = temporary_directory("merge-root");
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main work\n").expect("main file");
    let expected_head = commit_everything(&repo, "main work");
    git(&repo, &["checkout", "-q", "workspace"]);
    fs::write(repo.join("candidate.txt"), "candidate work\n").expect("candidate file");
    let candidate = commit_everything(&repo, "the candidate change");
    write_candidate_ref(&repo, &candidate);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("merge-store");
    let mut daemon = started_daemon("merge", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &expected_head, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect("the merge apply should settle applied");
    assert_eq!(outcome.receipt.result, ApplyResult::Applied);
    let resulting = outcome
        .receipt
        .resulting_commit
        .clone()
        .expect("applied carries a commit");
    let parents = git(&repo, &["rev-list", "--parents", "-n", "1", "main"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "a merge commit has two parents: {parents}"
    );
    assert_eq!(git(&repo, &["rev-parse", "main"]), resulting);
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);

    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_moved_target_branch_settles_base_stale_and_stays_retryable() {
    let (repo, base) = init_repository("base-stale");
    let integration_root = temporary_directory("base-stale-root");
    let candidate = candidate_commit_on_main(&repo, "base-stale", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("base-stale-store");
    let mut daemon = started_daemon("base-stale", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    // The caller's expected head drifted away from the real target tip:
    // `main` moved on after the candidate was frozen.
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main moved on\n").expect("main file");
    let moved_on = commit_everything(&repo, "main moved on");
    git(&repo, &["checkout", "-q", "workspace"]);
    let drifted = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("a stale base is a settled result, not an engine error");
    assert_eq!(drifted.receipt.result, ApplyResult::BaseStale);
    assert_eq!(drifted.receipt.resulting_commit, None);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        moved_on,
        "the target branch sits where it was"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Failed,
        "the failure code projects onto the retryable failed state"
    );

    // Retry with the correct expected head (a cherry-pick now that the
    // target diverged): the apply succeeds.
    let retried = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &moved_on, ApplyStrategy::CherryPick),
        &integration_root,
    )
    .expect("the retry settles");
    assert_eq!(retried.receipt.result, ApplyResult::Applied);
    assert_ne!(
        retried.receipt.local_apply_receipt_id, drifted.receipt.local_apply_receipt_id,
        "every attempt settles its own fresh receipt"
    );
    assert_ne!(git(&repo, &["rev-parse", "main"]), candidate);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "main"]),
        "the candidate change",
        "the retried apply delivered the candidate onto the moved target"
    );

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 2, "both attempts appended one receipt each");
    assert_eq!(frames[0].2.receipt.result, ApplyResult::BaseStale);
    assert_eq!(frames[1].2.receipt.result, ApplyResult::Applied);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn the_expected_head_drift_during_execution_is_refused_by_the_cas() {
    let (repo, base) = init_repository("cas-drift");
    let integration_root = temporary_directory("cas-drift-root");
    let candidate = candidate_commit_on_main(&repo, "cas-drift", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    // A concurrent writer's commit exists, and a post-checkout hook moves
    // `main` to it while the engine is creating its integration worktree —
    // a deterministic stand-in for a writer between preflight and the
    // atomic ref update.
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "a concurrent commit\n").expect("concurrent file");
    let concurrent = commit_everything(&repo, "a concurrent commit");
    git(&repo, &["reset", "-q", "--hard", &base]);
    git(&repo, &["checkout", "-q", "workspace"]);
    let hook = repo.join(".git").join("hooks").join("post-checkout");
    fs::create_dir_all(hook.parent().expect("hooks directory")).expect("hooks directory");
    fs::write(
        &hook,
        format!("#!/bin/sh\nexec git update-ref refs/heads/main \"{concurrent}\" \"{base}\"\n"),
    )
    .expect("the hook script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("hook permissions");
    }

    let root = temporary_directory("cas-drift-store");
    let mut daemon = started_daemon("cas-drift", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the drift is a settled result, not an engine error");
    assert_eq!(
        settled.receipt.result,
        ApplyResult::BaseStale,
        "the compare-and-swap ref update refuses the drifted target"
    );
    assert_eq!(settled.receipt.resulting_commit, None);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        concurrent,
        "only the concurrent writer's commit is on the branch"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert!(
        !integration_root
            .join("conflict-artifacts")
            .join(&candidate)
            .exists(),
        "the refused attempt leaves no integration worktree behind"
    );
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Failed
    );
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_dirty_working_tree_is_refused_before_any_git_write() {
    let (repo, base) = init_repository("dirty");
    let integration_root = temporary_directory("dirty-root");
    let candidate = candidate_commit_on_main(&repo, "dirty", "1");
    // The user's dirty tree: an untracked file.
    fs::write(repo.join("scratch.txt"), "user's local note\n").expect("untracked file");

    let root = temporary_directory("dirty-store");
    let mut daemon = started_daemon("dirty", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the dirty policy settles a result");
    assert_eq!(settled.receipt.result, ApplyResult::WorkingTreeDirty);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        base,
        "the target branch never moved"
    );
    assert_eq!(
        fs::read_to_string(repo.join("scratch.txt")).expect("the user's file"),
        "user's local note\n",
        "the user's dirty tree is never touched"
    );
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Failed
    );
    assert!(
        !integration_root
            .join("conflict-artifacts")
            .join(&candidate)
            .exists(),
        "the refusal happened before any integration worktree was created"
    );

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 1, "the settled attempt is still audited");
    assert_eq!(frames[0].2.receipt.result, ApplyResult::WorkingTreeDirty);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_conflict_is_kept_only_in_the_isolated_artifact_directory() {
    let (repo, _base) = init_repository("conflict");
    let integration_root = temporary_directory("conflict-root");
    // Diverging edits of the same tracked file: `main` moved its copy, the
    // candidate edited the workspace copy differently.
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("file.txt"), "main's own line\n").expect("main edit");
    let expected_head = commit_everything(&repo, "main edit");
    git(&repo, &["checkout", "-q", "workspace"]);
    fs::write(repo.join("file.txt"), "candidate's line\n").expect("candidate edit");
    let candidate = commit_everything(&repo, "candidate edit");
    write_candidate_ref(&repo, &candidate);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("conflict-store");
    let mut daemon = started_daemon("conflict", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &expected_head, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect("the conflict settles a result");
    assert_eq!(settled.receipt.result, ApplyResult::MergeConflict);
    assert_eq!(settled.receipt.resulting_commit, None);
    let artifact = settled
        .receipt
        .conflict_artifact_ref
        .clone()
        .expect("a merge conflict carries its conflict artifact reference");
    assert!(
        !artifact.contains(root.to_str().expect("UTF-8")),
        "the artifact reference is opaque, never a device path: {artifact}"
    );
    assert!(
        !artifact.starts_with('/'),
        "the artifact reference is not a path"
    );

    // The user's working tree: untouched, clean, still on the old content.
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(git(&repo, &["rev-parse", "main"]), expected_head);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Failed
    );

    // The conflict artifact lives only inside the isolated integration root:
    // the conflicted worktree is real, inspectable Git state.
    let artifact_directory = integration_attempt_directory(
        &integration_root,
        &candidate,
        settled.receipt.local_apply_receipt_id.as_str(),
    );
    let conflicted_worktree = artifact_directory.join("worktree");
    assert!(
        conflicted_worktree.exists(),
        "the conflicted worktree is kept"
    );
    let conflicted =
        fs::read_to_string(conflicted_worktree.join("file.txt")).expect("conflict file");
    assert!(
        conflicted.contains("<<<<<<<") && conflicted.contains("candidate's line"),
        "the kept worktree shows the real conflict markers: {conflicted}"
    );
    assert_eq!(
        git(
            &conflicted_worktree,
            &["diff", "--name-only", "--diff-filter=U"]
        ),
        "file.txt",
        "the kept worktree reports the unmerged path"
    );
    let summary: Value = serde_json::from_str(
        &fs::read_to_string(artifact_directory.join("conflict.json")).expect("conflict summary"),
    )
    .expect("the summary is JSON");
    assert_eq!(summary["candidateCommit"], candidate);
    assert_eq!(summary["targetBranch"], "main");
    assert_eq!(summary["strategy"], "merge");
    assert_eq!(summary["unmergedPaths"][0], "file.txt");

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].2.receipt.conflict_artifact_ref.as_deref(),
        Some(artifact.as_str()),
        "the durable frame carries the same opaque artifact reference"
    );
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_missing_or_drifted_candidate_ref_settles_candidate_missing() {
    let (repo, base) = init_repository("candidate-missing");
    let integration_root = temporary_directory("candidate-missing-root");
    let candidate = candidate_commit_on_main(&repo, "candidate-missing", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("candidate-missing-store");
    let mut daemon = started_daemon("candidate-missing", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    // No candidate ref was ever written in this checkout.
    let missing = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the missing ref settles a result");
    assert_eq!(missing.receipt.result, ApplyResult::CandidateMissing);

    // A drifted ref (pointing elsewhere) is equally "the candidate is gone".
    let other = git(&repo, &["rev-parse", "workspace"]);
    write_candidate_ref(&repo, &candidate);
    git(
        &repo,
        &[
            "update-ref",
            &format!("refs/winwincode/candidates/{candidate}"),
            &other,
        ],
    );
    let drifted = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the drifted ref settles a result");
    assert_eq!(drifted.receipt.result, ApplyResult::CandidateMissing);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        base,
        "the target branch never moved"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 2);
    for (_, _, payload) in &frames {
        assert_eq!(payload.receipt.result, ApplyResult::CandidateMissing);
    }
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_target_branch_checked_out_anywhere_is_never_moved() {
    let (repo, base) = init_repository("checked-out");
    let integration_root = temporary_directory("checked-out-root");
    let candidate = candidate_commit_on_main(&repo, "checked-out", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");
    // A second worktree holds another branch; both checked-out targets must
    // be refused.
    let linked = temporary_directory("checked-out-linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            linked.to_str().expect("UTF-8"),
        ],
    );

    let root = temporary_directory("checked-out-store");
    let mut daemon = started_daemon("checked-out", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    for (label, target) in [
        ("the bound checkout's branch", "workspace"),
        ("a linked worktree's branch", "other"),
    ] {
        let settled = apply_candidate_to_branch(
            &mut daemon,
            &request(&candidate, target, &base, ApplyStrategy::FastForward),
            &integration_root,
        )
        .expect("the checked-out target settles a result");
        assert_eq!(settled.receipt.result, ApplyResult::Failed, "{label}");
        assert_eq!(
            git(&repo, &["rev-parse", target]),
            base,
            "{label}: the checked-out branch never moved"
        );
    }
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(
        git(&linked, &["rev-parse", "HEAD"]),
        base,
        "the linked worktree is untouched too"
    );

    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 2);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&linked);
    cleanup(&repo);
}

#[test]
fn a_non_descendant_candidate_fails_closed_under_the_fast_forward_strategy() {
    let (repo, _base) = init_repository("not-ff");
    let integration_root = temporary_directory("not-ff-root");
    // The candidate is a child of the shared base; `main` moved elsewhere.
    fs::write(repo.join("candidate.txt"), "candidate work\n").expect("candidate file");
    let candidate = commit_everything(&repo, "the candidate change");
    write_candidate_ref(&repo, &candidate);
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main work\n").expect("main file");
    let expected_head = commit_everything(&repo, "main work");
    git(&repo, &["checkout", "-q", "workspace"]);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("not-ff-store");
    let mut daemon = started_daemon("not-ff", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &request(
            &candidate,
            "main",
            &expected_head,
            ApplyStrategy::FastForward,
        ),
        &integration_root,
    )
    .expect("the strategy mismatch settles a result");
    assert_eq!(
        settled.receipt.result,
        ApplyResult::Failed,
        "fast_forward refuses a candidate that is not a descendant"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        expected_head,
        "the target branch never moved"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert!(
        !integration_root
            .join("conflict-artifacts")
            .join(&candidate)
            .exists(),
        "the failed attempt cleaned up its integration worktree"
    );
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_stale_fencing_stamp_is_refused_before_any_local_action() {
    let (repo, base) = init_repository("fencing");
    let integration_root = temporary_directory("fencing-root");
    let candidate = candidate_commit_on_main(&repo, "fencing", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("fencing-store");
    let mut daemon = started_daemon("fencing", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    let mut stale = request(&candidate, "main", &base, ApplyStrategy::FastForward);
    stale.occupancy_fencing_token = TOKEN - 1;
    let error = apply_candidate_to_branch(&mut daemon, &stale, &integration_root)
        .expect_err("a stale token must refuse the command");
    assert_eq!(
        error.kind(),
        CandidateApplyErrorKind::FencingRejected(FencingRejection::StaleFencingToken),
        "the fencing layer refuses before any git action"
    );

    let mut foreign = request(&candidate, "main", &base, ApplyStrategy::FastForward);
    foreign.occupancy_lease_id = "ocl_FOREIGN0000000000000000000000".to_owned();
    let error = apply_candidate_to_branch(&mut daemon, &foreign, &integration_root)
        .expect_err("a foreign lease must refuse the command");
    assert_eq!(
        error.kind(),
        CandidateApplyErrorKind::FencingRejected(FencingRejection::StaleFencingToken)
    );

    assert_eq!(git(&repo, &["rev-parse", "main"]), base, "nothing moved");
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Retained,
        "the refusal never touched the registry"
    );
    let mut store = daemon.into_store();
    assert!(
        apply_result_frames(&mut store).is_empty(),
        "a refused command never settles a receipt"
    );
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn unknown_terminal_and_branch_creation_commands_are_refused() {
    let (repo, base) = init_repository("refusals");
    let integration_root = temporary_directory("refusals-root");
    let candidate = candidate_commit_on_main(&repo, "refusals", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("refusals-store");
    let mut daemon = started_daemon("refusals", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    // The branch-creation strategy belongs to the other engine.
    let error = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::CreateBranch),
        &integration_root,
    )
    .expect_err("create_branch is refused by the apply engine");
    assert_eq!(error.kind(), CandidateApplyErrorKind::InvalidInput);

    // No registry row for the candidate: the command identity is unknown.
    let unknown = "ffffffffffffffffffffffffffffffffffffffff";
    let error = apply_candidate_to_branch(
        &mut daemon,
        &request(unknown, "main", &base, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect_err("an unknown candidate must refuse");
    assert_eq!(error.kind(), CandidateApplyErrorKind::UnknownCandidate);

    // Progress the candidate to its applied terminal; a further apply is
    // refused without a receipt.
    {
        let connection = rusqlite::Connection::open(daemon.store_mut().database_path())
            .expect("the database opens");
        connection
            .execute(
                "UPDATE candidate_local_refs SET local_state = 'applied' WHERE candidate_id = ?1",
                [candidate.as_str()],
            )
            .expect("the lifecycle should progress");
        connection.close().expect("the inspection closes");
    }
    let error = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect_err("a terminal candidate must refuse");
    assert_eq!(error.kind(), CandidateApplyErrorKind::TerminalCandidate);

    assert_eq!(git(&repo, &["rev-parse", "main"]), base, "nothing moved");
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    let mut store = daemon.into_store();
    assert!(
        apply_result_frames(&mut store).is_empty(),
        "refusals never settle a receipt"
    );
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(unix)]
fn an_unusable_integration_root_settles_permission_denied() {
    use std::os::unix::fs::PermissionsExt;

    let (repo, base) = init_repository("permission");
    let candidate = candidate_commit_on_main(&repo, "permission", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let integration_root = temporary_directory("permission-root");
    fs::create_dir_all(&integration_root).expect("the integration root");
    fs::set_permissions(&integration_root, fs::Permissions::from_mode(0o555))
        .expect("the integration root should be read-only");

    let root = temporary_directory("permission-store");
    let mut daemon = started_daemon("permission", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate);
    write_candidate_ref(&repo, &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &request(&candidate, "main", &base, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect("the permission refusal settles a result");
    assert_eq!(settled.receipt.result, ApplyResult::PermissionDenied);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        base,
        "the target branch never moved"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);

    fs::set_permissions(&integration_root, fs::Permissions::from_mode(0o755))
        .expect("the integration root should be writable again for cleanup");
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}
