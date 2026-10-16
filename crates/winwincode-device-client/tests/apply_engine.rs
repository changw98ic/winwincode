// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use winwincode_client_port::candidate_application::candidate_application_digest;
use winwincode_client_port::domain::{
    ApplyResult, ApplyStrategy, ClientArchitecture, ClientCapacityReport, ClientPlatformTarget,
    LocalApplyReceipt, LocalCandidateState,
};
use winwincode_client_port::messages::{
    ClientCandidateApplyResultPayload, ClientToServerEnvelope, ClientToServerMessage,
};
use winwincode_device_client::apply_engine::{
    CandidateApplyRequest, CandidateIntegrationOutcome, CandidateIntegrationRequest,
    RecoveryCommandDirectory, apply_candidate_to_branch, candidate_recovery_guidance,
    integrate_candidates_for_verification,
};
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
const STAMP: &str = "2026-09-04T00:00:00.000Z";
const LEASE: &str = "ocl_APPLYENGINE00000000000000000000";
const TOKEN: u64 = 5;
const BINDING: &str = "rbd_APPLYENGINEBINDING000000000000";
const WORKER_SESSION: &str = "wks_APPLYENGINE00000000000000000";

struct NeverTransport;

impl ExchangeTransport for NeverTransport {
    fn exchange(&self, _request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
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

fn candidate_commit_on_main(repository: &Path, name: &str, step: &str) -> String {
    let original_main = git(repository, &["rev-parse", "main"]);
    git(repository, &["checkout", "-q", "main"]);
    fs::write(
        repository.join("candidate.txt"),
        format!("candidate work {name} {step}\n"),
    )
    .expect("candidate file");
    let candidate = commit_everything(repository, "the candidate change");
    git(repository, &["reset", "-q", "--hard", &original_main]);
    git(repository, &["checkout", "-q", "workspace"]);
    candidate
}

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

fn retain_candidate(store: &mut DeviceStore, candidate_commit: &str, base_commit: &str) {
    record_candidate_retention(
        store,
        &CandidateRetention {
            candidate_commit: candidate_commit.to_owned(),
            base_commit: base_commit.into(),
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

fn apply_request(
    candidate_commit: &str,
    target_branch: &str,
    expected_head: &str,
    strategy: ApplyStrategy,
) -> CandidateApplyRequest {
    let digest = candidate_application_digest(BINDING, expected_head, candidate_commit);
    let identity = candidate_application_digest(target_branch, &format!("{strategy:?}"), &digest);
    let identity = identity.trim_start_matches("sha256:")[..26].to_ascii_uppercase();
    CandidateApplyRequest {
        application_id: format!("lar_{identity}"),
        approval_id: format!("apr_{identity}"),
        candidate_digest: digest,
        reconciled_retry: false,
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("the fast-forward apply should settle applied");
    let receipt = &outcome.receipt;
    assert_eq!(receipt.result, ApplyResult::Applied, "the apply applies");
    let resulting = receipt.resulting_commit.as_deref().unwrap();
    assert_ne!(resulting, candidate);
    assert_eq!(
        git(&repo, &["rev-parse", &format!("{resulting}^")]),
        candidate
    );
    assert!(
        git(&repo, &["log", "-1", "--format=%B", resulting])
            .lines()
            .any(|line| line
                == format!("WinWinCode-Application: {}", receipt.local_apply_receipt_id))
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
    assert_eq!(git(&repo, &["rev-parse", "main"]), resulting);
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

    let replay = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &base, ApplyStrategy::FastForward),
        &integration_root,
    )
    .expect("pending receipt survives downlink replay");
    assert_eq!(replay.receipt, outcome.receipt);
    assert_eq!(replay.frame_sequence, outcome.frame_sequence);

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
fn applied_receipts_produce_reset_free_reverse_candidate_guidance() {
    let expected_head = "a".repeat(40);
    let receipt = LocalApplyReceipt {
        local_apply_receipt_id: "lar_01J2RECOVERY".into(),
        candidate_ref: format!("refs/winwincode/candidates/{}", "b".repeat(40)),
        repository_binding_id: BINDING.into(),
        target_branch: "main".into(),
        expected_head: expected_head.clone(),
        strategy: ApplyStrategy::Merge,
        result: ApplyResult::Applied,
        resulting_commit: Some("c".repeat(40)),
        conflict_artifact_ref: None,
        created_at: STAMP.into(),
        revision: 1,
    };
    let recovery = candidate_recovery_guidance(&receipt)
        .expect("the applied receipt is valid")
        .expect("an applied receipt has recovery guidance");
    assert_eq!(
        recovery.recovery_branch,
        "winwincode/recovery/lar_01j2recovery"
    );
    assert_eq!(recovery.commands.len(), 4);
    assert_eq!(
        recovery.commands[0].directory,
        RecoveryCommandDirectory::Repository
    );
    assert_eq!(
        recovery.commands[2].arguments,
        ["read-tree", "--reset", "-u", expected_head.as_str()]
    );
    assert!(
        recovery
            .commands
            .iter()
            .flat_map(|command| &command.arguments)
            .all(|argument| argument != "--hard" && argument != "--force"),
        "recovery creates a reverse commit and never rewrites history"
    );
}

#[test]
fn peer_candidates_combine_into_a_stable_ref_that_requires_fresh_verification() {
    let (repo, base) = init_repository("peer-integration");
    let integration_root = temporary_directory("peer-integration-root");

    fs::write(repo.join("peer-a.txt"), "peer A\n").expect("peer A file");
    let candidate_a = commit_everything(&repo, "peer A candidate");
    write_candidate_ref(&repo, &candidate_a);
    git(&repo, &["reset", "-q", "--hard", &base]);
    fs::write(repo.join("peer-b.txt"), "peer B\n").expect("peer B file");
    let candidate_b = commit_everything(&repo, "peer B candidate");
    write_candidate_ref(&repo, &candidate_b);
    git(&repo, &["reset", "-q", "--hard", &base]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("peer-integration-store");
    let mut daemon = started_daemon("peer-integration", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate_a, &base);
    retain_candidate(daemon.store_mut(), &candidate_b, &base);

    let outcome = integrate_candidates_for_verification(
        &mut daemon,
        &CandidateIntegrationRequest {
            repository_binding_id: BINDING.to_owned(),
            source_candidate_refs: vec![
                format!("refs/winwincode/candidates/{candidate_a}"),
                format!("refs/winwincode/candidates/{candidate_b}"),
            ],
            expected_head: base.clone(),
            occupancy_lease_id: LEASE.to_owned(),
            occupancy_fencing_token: TOKEN,
        },
        &integration_root,
    )
    .expect("the independent peer changes should combine");
    let CandidateIntegrationOutcome::Prepared(prepared) = outcome else {
        panic!("independent peer candidates must not conflict")
    };
    assert!(prepared.requires_integration_verification);
    assert!(!prepared.task_level_verdicts_authorize_delivery);
    assert_eq!(
        git(&repo, &["rev-parse", &prepared.integration_ref]),
        prepared.integration_commit,
        "the combined result remains checkoutable after worktree cleanup"
    );
    assert_eq!(
        git(
            &repo,
            &[
                "show",
                &format!("{}:peer-a.txt", prepared.integration_commit)
            ]
        ),
        "peer A"
    );
    assert_eq!(
        git(
            &repo,
            &[
                "show",
                &format!("{}:peer-b.txt", prepared.integration_commit)
            ]
        ),
        "peer B"
    );
    assert!(prepared.integration_digest.starts_with("sha256:"));
    assert_eq!(prepared.integration_digest.len(), 71);
    user_tree_is_untouched(&repo, &base, &workspace_content);
    assert!(
        candidate_local_ref(daemon.store_mut(), &prepared.integration_commit)
            .expect("registry read")
            .is_none(),
        "the combined result cannot enter the applyable registry before verification"
    );

    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn cherry_pick_applies_with_the_original_author_and_device_committer() {
    let (repo, _base) = init_repository("cherry-pick");
    let integration_root = temporary_directory("cherry-pick-root");
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main work\n").expect("main file");
    let expected_head = commit_everything(&repo, "main work");
    git(&repo, &["checkout", "-q", "workspace"]);
    git(&repo, &["reset", "--hard", &expected_head]);
    fs::write(repo.join("candidate.txt"), "candidate work\n").expect("candidate file");
    let candidate = commit_everything(&repo, "the candidate change");
    write_candidate_ref(&repo, &candidate);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("cherry-pick-store");
    let mut daemon = started_daemon("cherry-pick", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &expected_head);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(
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
fn merge_applies_the_frozen_candidate_with_an_application_trailer() {
    let (repo, _base) = init_repository("merge");
    let integration_root = temporary_directory("merge-root");
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main work\n").expect("main file");
    let expected_head = commit_everything(&repo, "main work");
    git(&repo, &["checkout", "-q", "workspace"]);
    git(&repo, &["reset", "--hard", &expected_head]);
    fs::write(repo.join("candidate.txt"), "candidate work\n").expect("candidate file");
    let candidate = commit_everything(&repo, "the candidate change");
    write_candidate_ref(&repo, &candidate);
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("merge-store");
    let mut daemon = started_daemon("merge", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &expected_head);

    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &expected_head, ApplyStrategy::Merge),
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
fn a_moved_target_branch_requires_a_new_frozen_candidate() {
    let (repo, base) = init_repository("base-stale");
    let integration_root = temporary_directory("base-stale-root");
    let candidate = candidate_commit_on_main(&repo, "base-stale", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");

    let root = temporary_directory("base-stale-store");
    let mut daemon = started_daemon("base-stale", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("main.txt"), "main moved on\n").expect("main file");
    let moved_on = commit_everything(&repo, "main moved on");
    git(&repo, &["checkout", "-q", "workspace"]);
    let drifted = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &base, ApplyStrategy::FastForward),
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

    let retried = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &moved_on, ApplyStrategy::CherryPick),
        &integration_root,
    )
    .expect("the retry settles");
    assert_eq!(retried.receipt.result, ApplyResult::BaseStale);
    assert_ne!(
        retried.receipt.local_apply_receipt_id, drifted.receipt.local_apply_receipt_id,
        "every attempt settles its own fresh receipt"
    );
    assert_eq!(git(&repo, &["rev-parse", "main"]), moved_on);

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 2, "both attempts appended one receipt each");
    assert_eq!(frames[0].2.receipt.result, ApplyResult::BaseStale);
    assert_eq!(frames[1].2.receipt.result, ApplyResult::BaseStale);
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &base, ApplyStrategy::FastForward),
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
    use winwincode_client_port::messages::ApplicationReconciliationOutcome;
    use winwincode_device_client::apply_engine::report_recovered_applications;
    let (repo, base) = init_repository("dirty");
    let integration_root = temporary_directory("dirty-root");
    let candidate = candidate_commit_on_main(&repo, "dirty", "1");
    fs::write(repo.join("scratch.txt"), "user's local note\n").expect("untracked file");

    let root = temporary_directory("dirty-store");
    let mut daemon = started_daemon("dirty", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let mut request = apply_request(&candidate, "main", &base, ApplyStrategy::FastForward);
    let settled = apply_candidate_to_branch(&mut daemon, &request, &integration_root)
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
        LocalCandidateState::Retained
    );
    assert!(
        !integration_root
            .join("conflict-artifacts")
            .join(&candidate)
            .exists(),
        "the refusal happened before any integration worktree was created"
    );

    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    let pending = daemon.outbox_snapshot().unwrap();
    assert!(pending.frames.iter().any(|frame| {
        let envelope: ClientToServerEnvelope = serde_json::from_slice(&frame.frame).unwrap();
        matches!(envelope.message, ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == request.application_id
                && payload.outcome == ApplicationReconciliationOutcome::NotStarted)
    }));
    fs::remove_file(repo.join("scratch.txt")).unwrap();
    request.reconciled_retry = true;
    let retried = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(retried.receipt.result, ApplyResult::Applied);
    assert_eq!(
        retried.receipt.local_apply_receipt_id,
        request.application_id
    );
    let trailer = format!("WinWinCode-Application: {}", request.application_id);
    assert_eq!(
        git(&repo, &["log", "--format=%B", "main"])
            .lines()
            .filter(|line| *line == trailer)
            .count(),
        1
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn a_diverged_baseline_is_refused_before_a_conflicting_merge() {
    let (repo, base) = init_repository("conflict");
    let integration_root = temporary_directory("conflict-root");
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
    retain_candidate(daemon.store_mut(), &candidate, &base);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &expected_head, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect("the conflict settles a result");
    assert_eq!(settled.receipt.result, ApplyResult::BaseStale);
    assert!(settled.receipt.conflict_artifact_ref.is_none());

    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);
    assert_eq!(git(&repo, &["rev-parse", "main"]), expected_head);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Failed
    );

    assert!(!integration_root.exists());

    let mut store = daemon.into_store();
    let frames = apply_result_frames(&mut store);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].2.receipt.result, ApplyResult::BaseStale);
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    let mut request = apply_request(&candidate, "main", &base, ApplyStrategy::FastForward);
    let missing = apply_candidate_to_branch(&mut daemon, &request, &integration_root)
        .expect("the missing ref settles a result");
    assert_eq!(missing.receipt.result, ApplyResult::CandidateMissing);

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
    request.reconciled_retry = true;
    let drifted = apply_candidate_to_branch(&mut daemon, &request, &integration_root)
        .expect("the drifted ref settles a result");
    assert_eq!(drifted.receipt.result, ApplyResult::CandidateMissing);
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        base,
        "the target branch never moved"
    );
    user_tree_is_untouched(&repo, &workspace_head, &workspace_content);

    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    write_candidate_ref(&repo, &candidate);
    let restored = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(restored.receipt.result, ApplyResult::Applied);
    assert_eq!(
        restored.receipt.local_apply_receipt_id,
        request.application_id
    );
    let store = daemon.into_store();
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn applies_to_the_bound_checkout_and_protects_other_worktrees() {
    let (repo, base) = init_repository("checked-out");
    let integration_root = temporary_directory("checked-out-root");
    let candidate = candidate_commit_on_main(&repo, "checked-out", "1");
    let workspace_head = git(&repo, &["rev-parse", "workspace"]);
    let workspace_content = fs::read_to_string(repo.join("file.txt")).expect("workspace file");
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    for (label, target) in [
        ("a linked worktree's branch", "other"),
        ("the bound checkout's branch", "workspace"),
    ] {
        let settled = apply_candidate_to_branch(
            &mut daemon,
            &apply_request(&candidate, target, &base, ApplyStrategy::FastForward),
            &integration_root,
        )
        .expect("the checked-out target settles a result");
        let current = target == "workspace";
        assert_eq!(
            settled.receipt.result,
            if current {
                ApplyResult::Applied
            } else {
                ApplyResult::Failed
            },
            "{label}"
        );
        assert_eq!(
            git(&repo, &["rev-parse", &format!("{target}^{{tree}}")]),
            if current {
                git(&repo, &["rev-parse", &format!("{candidate}^{{tree}}")])
            } else {
                git(&repo, &["rev-parse", &format!("{base}^{{tree}}")])
            },
            "{label}: only the bound checkout may move"
        );
    }
    assert_ne!(git(&repo, &["rev-parse", "HEAD"]), workspace_head);
    assert_eq!(
        fs::read_to_string(repo.join("file.txt")).unwrap(),
        workspace_content
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert!(repo.join("candidate.txt").exists());
    assert_eq!(
        git(&linked, &["rev-parse", "HEAD"]),
        base,
        "the linked worktree is untouched too"
    );

    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&linked);
    cleanup(&repo);
}

#[test]
fn a_non_descendant_candidate_fails_closed_under_the_fast_forward_strategy() {
    let (repo, base) = init_repository("not-ff");
    let integration_root = temporary_directory("not-ff-root");
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
    retain_candidate(daemon.store_mut(), &candidate, &base);

    let settled = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(
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
        ApplyResult::BaseStale,
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let mut stale = apply_request(&candidate, "main", &base, ApplyStrategy::FastForward);
    stale.occupancy_fencing_token = TOKEN - 1;
    let error = apply_candidate_to_branch(&mut daemon, &stale, &integration_root)
        .expect_err("a stale token must refuse the command");
    assert_eq!(
        error.kind(),
        CandidateApplyErrorKind::FencingRejected(FencingRejection::StaleFencingToken),
        "the fencing layer refuses before any git action"
    );

    let mut foreign = apply_request(&candidate, "main", &base, ApplyStrategy::FastForward);
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let error = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "main", &base, ApplyStrategy::CreateBranch),
        &integration_root,
    )
    .expect_err("create_branch is refused by the apply engine");
    assert_eq!(error.kind(), CandidateApplyErrorKind::InvalidInput);

    let unknown = "ffffffffffffffffffffffffffffffffffffffff";
    let error = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(unknown, "main", &base, ApplyStrategy::Merge),
        &integration_root,
    )
    .expect_err("an unknown candidate must refuse");
    assert_eq!(error.kind(), CandidateApplyErrorKind::UnknownCandidate);

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
        &apply_request(&candidate, "main", &base, ApplyStrategy::Merge),
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
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);

    let mut request = apply_request(&candidate, "main", &base, ApplyStrategy::Merge);
    let settled = apply_candidate_to_branch(&mut daemon, &request, &integration_root)
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
    request.reconciled_retry = true;
    let retried = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(retried.receipt.result, ApplyResult::Applied);
    assert_eq!(
        retried.receipt.local_apply_receipt_id,
        request.application_id
    );
    let mut store = daemon.into_store();
    assert_eq!(apply_result_frames(&mut store).len(), 1);
    store.close().expect("store should close");
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[allow(clippy::too_many_lines)]
fn daemon_applies_the_complete_reworked_candidate_from_its_downlink() {
    use winwincode_client_port::messages::{
        CommandContext, OccupancyCommandContext, ServerCandidateApplyPayload,
        ServerToClientEnvelope, ServerToClientMessage,
    };
    use winwincode_device_client::{ExchangeRequest, ExchangeResponse};
    struct ApplyTransport {
        candidate: String,
        base: String,
    }
    impl ExchangeTransport for ApplyTransport {
        fn exchange(&self, bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
            let request: ExchangeRequest = serde_json::from_slice(bytes).unwrap();
            let first = &request.frames[0];
            let envelope = ServerToClientEnvelope {
                schema_version: "winwincode/v1".into(),
                message_id: "apply-command".into(),
                client_node_id: first["clientNodeId"].as_str().unwrap().into(),
                client_instance_id: first["clientInstanceId"].as_str().unwrap().into(),
                sequence: 1,
                occurred_at: STAMP.into(),
                message: ServerToClientMessage::CandidateApply(ServerCandidateApplyPayload {
                    reconciled_retry: false,
                    application_id: apply_request(
                        &self.candidate,
                        "workspace",
                        &self.base,
                        ApplyStrategy::Merge,
                    )
                    .application_id,
                    approval_id: apply_request(
                        &self.candidate,
                        "workspace",
                        &self.base,
                        ApplyStrategy::Merge,
                    )
                    .approval_id,
                    candidate_digest: candidate_application_digest(
                        BINDING,
                        &self.base,
                        &self.candidate,
                    ),
                    occupancy: OccupancyCommandContext {
                        command: CommandContext {
                            expected_revision: 1,
                            idempotency_key: "apply-command".into(),
                        },
                        occupancy_lease_id: LEASE.into(),
                        occupancy_fencing_token: TOKEN,
                    },
                    candidate_ref: format!("refs/winwincode/candidates/{}", self.candidate),
                    repository_binding_id: BINDING.into(),
                    target_branch: "workspace".into(),
                    expected_head: self.base.clone(),
                    requester_user_id: "usr_HOLDER0000000000000000000".into(),
                    strategy: ApplyStrategy::Merge,
                }),
            };
            Ok(serde_json::to_vec(&ExchangeResponse {
                schema_version: "winwincode/v1".into(),
                ack_sequence: request.frames.last().unwrap()["sequence"].as_u64().unwrap(),
                replay_from_sequence: None,
                frames: vec![serde_json::to_value(envelope).unwrap()],
                enrollment: None,
            })
            .unwrap())
        }
    }
    let (repo, base) = init_repository("daemon-apply");
    let first = candidate_commit_on_main(&repo, "daemon-apply", "1");
    git(&repo, &["checkout", "--detach", &first]);
    fs::write(repo.join("candidate.txt"), "repaired\n").unwrap();
    let candidate = commit_everything(&repo, "repair candidate");
    git(&repo, &["checkout", "workspace"]);
    let root = temporary_directory("daemon-apply-store");
    let mut daemon = started_daemon("daemon-apply", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let store = daemon.into_store();
    let identity = load_device_identity(&store).unwrap().unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("daemon-apply"),
        store,
        Arc::new(ApplyTransport {
            candidate: candidate.clone(),
            base,
        }),
        &identity,
    )
    .unwrap();
    daemon
        .tick(std::time::Instant::now())
        .expect("downlink invokes apply engine");
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^{tree}"]),
        git(&repo, &["rev-parse", &format!("{candidate}^{{tree}}")])
    );
    assert_eq!(
        fs::read_to_string(repo.join("candidate.txt")).unwrap(),
        "repaired\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let frames = apply_result_frames(daemon.store_mut());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].2.receipt.result, ApplyResult::Applied);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    daemon
        .tick(std::time::Instant::now() + Duration::from_secs(1))
        .expect("duplicate downlink is acknowledged");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
#[allow(clippy::too_many_lines)]
fn an_unavailable_application_checkout_does_not_block_other_recovery_or_hello() {
    use winwincode_client_port::messages::ApplicationReconciliationOutcome;
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point, recover_started_applications,
        report_recovered_applications,
    };
    let (unavailable, unavailable_base) = init_repository("recover-unavailable");
    let (healthy, base) = init_repository("recover-healthy");
    let integration_root = temporary_directory("recover-unavailable-integration");
    let root = temporary_directory("recover-unavailable-store");
    let unavailable_candidate = candidate_commit_on_main(&unavailable, "recover-unavailable", "1");
    let candidate = candidate_commit_on_main(&healthy, "recover-healthy", "1");
    let mut daemon = started_daemon("recover-unavailable", &root);
    bind(daemon.store_mut(), &unavailable);
    retain_candidate(
        daemon.store_mut(),
        &unavailable_candidate,
        &unavailable_base,
    );
    write_candidate_ref(&unavailable, &unavailable_candidate);
    let unavailable_request = apply_request(
        &unavailable_candidate,
        "workspace",
        &unavailable_base,
        ApplyStrategy::Merge,
    );
    apply_candidate_with_crash_point(
        &mut daemon,
        &unavailable_request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    let healthy_binding = format!("rbd_{}", "C".repeat(26));
    daemon
        .store_mut()
        .put_path_mapping(&PathMappingRecord {
            repository_binding_id: healthy_binding.clone(),
            canonical_path: healthy.to_str().unwrap().into(),
            git_common_directory: None,
            last_canonicalized_at: None,
            local_state: "ready".into(),
        })
        .unwrap();
    record_candidate_retention(
        daemon.store_mut(),
        &CandidateRetention {
            candidate_commit: candidate.clone(),
            base_commit: base.clone(),
            repository_binding_id: healthy_binding.clone(),
            worker_session_id: WORKER_SESSION.into(),
            local_git_ref: format!("refs/winwincode/candidates/{candidate}"),
            retained_at: STAMP.into(),
        },
    )
    .unwrap();
    write_candidate_ref(&healthy, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    request.repository_binding_id = healthy_binding.clone();
    request.candidate_digest = candidate_application_digest(&healthy_binding, &base, &candidate);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&healthy, &["rev-parse", "HEAD"]);
    let moved = unavailable.with_extension("moved");
    fs::rename(&unavailable, &moved).unwrap();
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-unavailable"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    let recovered = recover_started_applications(&mut daemon).unwrap();
    assert_eq!(recovered.len(), 2);
    assert!(format!("{recovered:?}").contains("the bound checkout no longer exists"));
    daemon.tick(std::time::Instant::now()).unwrap();
    let queued = daemon.outbox_snapshot().unwrap();
    let envelopes = queued
        .frames
        .iter()
        .map(|frame| serde_json::from_slice::<ClientToServerEnvelope>(&frame.frame).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        envelopes[0].message,
        ClientToServerMessage::Hello(_)
    ));
    assert!(!envelopes.iter().any(|envelope| matches!(&envelope.message,
        ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == unavailable_request.application_id)));
    assert!(envelopes.iter().any(|envelope| matches!(&envelope.message,
        ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == request.application_id
                && payload.result_commit.as_deref() == Some(applied.as_str()))));
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Applied
    );
    assert_eq!(git(&healthy, &["rev-parse", "HEAD"]), applied);
    fs::rename(&moved, &unavailable).unwrap();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 1);
    let repaired = daemon.outbox_snapshot().unwrap();
    assert!(repaired.frames.iter().any(|frame| {
        let envelope: ClientToServerEnvelope = serde_json::from_slice(&frame.frame).unwrap();
        matches!(envelope.message, ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == unavailable_request.application_id
                && payload.outcome == ApplicationReconciliationOutcome::NotStarted)
    }));
    let mut retry = unavailable_request;
    retry.reconciled_retry = true;
    let resumed = apply_candidate_to_branch(&mut daemon, &retry, &integration_root).unwrap();
    assert_eq!(resumed.receipt.result, ApplyResult::Applied);
    assert_eq!(resumed.receipt.local_apply_receipt_id, retry.application_id);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&unavailable);
    cleanup(&healthy);
}

#[test]
#[cfg(feature = "test-support")]
fn restart_bootstrap_keeps_old_frames_and_announces_before_recovery_reports() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point, recover_started_applications,
    };
    use winwincode_device_client::{SessionSupervisor, SupervisorConfig, WorkerStopIntentRecord};
    let (repo, base) = init_repository("recover-bootstrap");
    let integration_root = temporary_directory("recover-bootstrap-integration");
    let root = temporary_directory("recover-bootstrap-store");
    let candidate = candidate_commit_on_main(&repo, "recover-bootstrap", "1");
    let mut daemon = started_daemon("recover-bootstrap", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    daemon.tick(std::time::Instant::now()).unwrap();
    let old_instance = daemon.client_instance_id().to_owned();
    let retained = daemon.outbox_snapshot().unwrap();
    assert!(!retained.frames.is_empty());
    let request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&repo, &["rev-parse", "HEAD"]);
    let commits = git(&repo, &["rev-list", "--count", "HEAD"]);
    daemon
        .store_mut()
        .record_worker_stop_intent(&WorkerStopIntentRecord {
            command_message_id: "recover-bootstrap-stop".into(),
            idempotency_key: "recover-bootstrap-stop".into(),
            worker_session_id: WORKER_SESSION.into(),
            worker_id: "wrk_BOOTSTRAP000000000000000000".into(),
            reason: winwincode_client_port::domain::ClientWorkerStopReason::LeaseRecovered,
            occupancy_lease_id: LEASE.into(),
            fencing_token: TOKEN,
            expected_revision: 1,
            receipt_payload: None,
            recorded_at: STAMP.into(),
            completed_at: None,
        })
        .unwrap();
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).unwrap();
    assert_ne!(identity.current_instance_id(), old_instance);
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-bootstrap"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    recover_started_applications(&mut daemon).unwrap();
    assert_eq!(daemon.outbox_snapshot().unwrap(), retained);
    let supervisor = SessionSupervisor::new(
        SupervisorConfig {
            client_node_id: ASSIGNED_NODE.into(),
            client_instance_id: identity.current_instance_id().into(),
            server_origin: "http://127.0.0.1:8443".into(),
            model_route: None,
            worker_binary_path: None,
            max_concurrent_worker_sessions: 4,
            stop_grace_period: Duration::from_millis(10),
        },
        DeviceStore::open(&root).unwrap(),
    )
    .unwrap();
    daemon.set_worker_supervisor(supervisor);
    daemon.tick(std::time::Instant::now()).unwrap();
    let queued = daemon.outbox_snapshot().unwrap();
    assert_eq!(&queued.frames[..retained.frames.len()], &retained.frames);
    let new_frames = queued.frames[retained.frames.len()..]
        .iter()
        .map(|frame| serde_json::from_slice::<ClientToServerEnvelope>(&frame.frame).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        new_frames[0].message,
        ClientToServerMessage::Hello(_)
    ));
    assert!(
        new_frames
            .iter()
            .all(|frame| frame.client_instance_id == identity.current_instance_id())
    );
    assert!(new_frames.iter().any(|frame| matches!(
        &frame.message,
        ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == request.application_id && payload.result_commit.as_deref() == Some(applied.as_str())
    )));
    assert!(new_frames.iter().any(|frame| matches!(
        &frame.message,
        ClientToServerMessage::CommandAck(payload) if payload.command_message_id == "recover-bootstrap-stop"
    )));
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]), commits);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
fn committed_application_is_recovered_after_a_crash_without_a_second_commit() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point, recover_started_applications,
    };
    let (repo, base) = init_repository("recover-committed");
    let integration_root = temporary_directory("recover-committed-integration");
    let root = temporary_directory("recover-committed-store");
    let candidate = candidate_commit_on_main(&repo, "recover-committed", "1");
    let mut daemon = started_daemon("recover-committed", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .expect_err("crash before application.finished");
    let applied = git(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(applied, base);
    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    let before = git(&repo, &["rev-list", "--count", "HEAD"]);
    daemon.into_store().close().unwrap();
    let (store, identity) = open_resumed(&root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-committed"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    let recovered = recover_started_applications(&mut daemon).unwrap();
    assert_eq!(recovered.len(), 1);
    let outcome = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(
        outcome.receipt.resulting_commit.as_deref(),
        Some(applied.as_str())
    );
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]), before);
    assert_eq!(
        git(&repo, &["rev-parse", &request.candidate_ref]),
        candidate
    );
    let repeated = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(repeated.receipt, outcome.receipt);
    assert_eq!(apply_result_frames(daemon.store_mut()).len(), 1);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
#[allow(clippy::too_many_lines)]
fn a_same_epoch_retry_reports_the_commit_after_a_second_crash() {
    use winwincode_client_port::exchange::OutboxSession;
    use winwincode_client_port::messages::ApplicationReconciliationOutcome;
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point, report_recovered_applications,
    };
    let (repo, base) = init_repository("recover-same-epoch");
    let integration_root = temporary_directory("recover-same-epoch-integration");
    let root = temporary_directory("recover-same-epoch-store");
    let candidate = candidate_commit_on_main(&repo, "recover-same-epoch", "1");
    let mut daemon = started_daemon("recover-same-epoch", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-same-epoch"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    daemon.tick(std::time::Instant::now()).unwrap();
    let before_retry = daemon.outbox_snapshot().unwrap();
    assert!(before_retry.frames.iter().any(|frame| {
        let envelope: ClientToServerEnvelope = serde_json::from_slice(&frame.frame).unwrap();
        matches!(envelope.message, ClientToServerMessage::ApplicationReconciled(payload)
            if payload.application_id == request.application_id
                && payload.outcome == ApplicationReconciliationOutcome::NotStarted
                && payload.occupancy.occupancy_fencing_token == TOKEN)
    }));
    OutboxSession::new()
        .acknowledge(daemon.store_mut(), before_retry.highest_sequence)
        .unwrap();
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    request.reconciled_retry = true;
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(applied, base);
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-same-epoch"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    daemon.tick(std::time::Instant::now()).unwrap();
    let recovered = daemon.outbox_snapshot().unwrap();
    let reports = recovered
        .frames
        .iter()
        .filter_map(|frame| {
            let envelope: ClientToServerEnvelope = serde_json::from_slice(&frame.frame).unwrap();
            match envelope.message {
                ClientToServerMessage::ApplicationReconciled(payload) => Some(payload),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reports.len(),
        1,
        "the changed fact must be reported in the same epoch"
    );
    assert_eq!(reports[0].application_id, request.application_id);
    assert_eq!(reports[0].approval_id, request.approval_id);
    assert_eq!(
        reports[0].outcome,
        ApplicationReconciliationOutcome::Applied
    );
    assert_eq!(reports[0].result_commit.as_deref(), Some(applied.as_str()));
    assert_eq!(reports[0].original_epoch, TOKEN);
    assert_eq!(reports[0].occupancy.occupancy_fencing_token, TOKEN);
    assert_eq!(
        registry_state(daemon.store_mut(), &candidate),
        LocalCandidateState::Applied
    );
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    assert_eq!(daemon.outbox_snapshot().unwrap(), recovered);
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&root).unwrap();
    let identity = ensure_device_identity(&mut store, &seed(), STAMP).unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-same-epoch"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    assert_eq!(daemon.outbox_snapshot().unwrap(), recovered);
    daemon.tick(std::time::Instant::now()).unwrap();
    let replay = daemon.outbox_snapshot().unwrap();
    assert_eq!(&replay.frames[..recovered.frames.len()], &recovered.frames);
    assert!(replay.frames[recovered.frames.len()..].iter().all(|frame| {
        let envelope: ClientToServerEnvelope = serde_json::from_slice(&frame.frame).unwrap();
        !matches!(
            envelope.message,
            ClientToServerMessage::ApplicationReconciled(_)
        )
    }));
    OutboxSession::new()
        .acknowledge(daemon.store_mut(), replay.highest_sequence)
        .unwrap();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    let trailer = format!("WinWinCode-Application: {}", request.application_id);
    assert_eq!(
        git(&repo, &["log", "--format=%B", "HEAD"])
            .lines()
            .filter(|line| *line == trailer)
            .count(),
        1
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), applied);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
fn a_changed_recovery_outcome_is_reported_once_in_the_same_epoch() {
    use winwincode_client_port::exchange::OutboxSession;
    use winwincode_client_port::messages::ApplicationReconciliationOutcome;
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point, report_recovered_applications,
    };
    let (repo, base) = init_repository("recover-changed-outcome");
    let integration_root = temporary_directory("recover-changed-outcome-integration");
    let root = temporary_directory("recover-changed-outcome-store");
    let candidate = candidate_commit_on_main(&repo, "recover-changed-outcome", "1");
    let mut daemon = started_daemon("recover-changed-outcome", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 1);
    let not_started = daemon.outbox_snapshot().unwrap();
    OutboxSession::new()
        .acknowledge(daemon.store_mut(), not_started.highest_sequence)
        .unwrap();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    let mut retry = request.clone();
    retry.reconciled_retry = true;
    apply_candidate_with_crash_point(
        &mut daemon,
        &retry,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    fs::write(repo.join("partial.txt"), "partial application").unwrap();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 1);
    let unknown = daemon.outbox_snapshot().unwrap();
    assert_eq!(unknown.frames.len(), 1);
    let envelope: ClientToServerEnvelope =
        serde_json::from_slice(&unknown.frames[0].frame).unwrap();
    let ClientToServerMessage::ApplicationReconciled(payload) = envelope.message else {
        panic!("the changed recovery fact must be reported");
    };
    assert_eq!(payload.application_id, request.application_id);
    assert_eq!(
        payload.outcome,
        ApplicationReconciliationOutcome::OutcomeUnknown
    );
    assert_eq!(payload.occupancy.occupancy_fencing_token, TOKEN);
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    assert_eq!(daemon.outbox_snapshot().unwrap(), unknown);
    OutboxSession::new()
        .acknowledge(daemon.store_mut(), unknown.highest_sequence)
        .unwrap();
    fs::remove_file(repo.join("partial.txt")).unwrap();
    daemon.into_store().close().unwrap();
    let (store, identity) = open_resumed(&root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-changed-outcome"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
fn an_unstarted_application_needs_explicit_server_retry_authorization() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point,
    };
    let (repo, base) = init_repository("recover-unstarted");
    let integration_root = temporary_directory("recover-unstarted-integration");
    let root = temporary_directory("recover-unstarted-store");
    let candidate = candidate_commit_on_main(&repo, "recover-unstarted", "1");
    let mut daemon = started_daemon("recover-unstarted", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    let error = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap_err();
    assert_eq!(
        error.kind(),
        CandidateApplyErrorKind::ReconciliationRequired
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    request.reconciled_retry = true;
    let outcome = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(outcome.receipt.result, ApplyResult::Applied);
    assert_eq!(
        outcome.receipt.local_apply_receipt_id,
        request.application_id
    );
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
fn a_changed_repository_blocks_an_interrupted_application_even_after_the_tree_is_cleaned() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point,
    };
    let (repo, base) = init_repository("recover-unknown");
    let integration_root = temporary_directory("recover-unknown-integration");
    let root = temporary_directory("recover-unknown-store");
    let candidate = candidate_commit_on_main(&repo, "recover-unknown", "1");
    let mut daemon = started_daemon("recover-unknown", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    fs::write(repo.join("partial.txt"), "partial application").unwrap();
    let error = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap_err();
    assert_eq!(error.kind(), CandidateApplyErrorKind::OutcomeUnknown);
    fs::remove_file(repo.join("partial.txt")).unwrap();
    request.reconciled_retry = true;
    let error = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap_err();
    assert_eq!(error.kind(), CandidateApplyErrorKind::OutcomeUnknown);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    request.application_id = format!("lar_{}", "B".repeat(26));
    request.approval_id = format!("apr_{}", "B".repeat(26));
    request.target_branch = "main".into();
    request.reconciled_retry = false;
    let error = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap_err();
    assert_eq!(error.kind(), CandidateApplyErrorKind::OutcomeUnknown);
    assert_eq!(git(&repo, &["rev-parse", "main"]), base);
    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn approval_identity_and_digest_cannot_be_rebound_to_another_candidate() {
    let (repo, base) = init_repository("approval-binding");
    let integration_root = temporary_directory("approval-binding-integration");
    let root = temporary_directory("approval-binding-store");
    let candidate = candidate_commit_on_main(&repo, "approval-binding", "1");
    let mut daemon = started_daemon("approval-binding", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    request.candidate_digest =
        candidate_application_digest("another-repository", &base, &candidate);
    let error = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap_err();
    assert_eq!(error.kind(), CandidateApplyErrorKind::InvalidInput);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
fn cherry_pick_delivers_the_entire_reworked_candidate_snapshot() {
    let (repo, base) = init_repository("reworked-snapshot");
    let integration_root = temporary_directory("reworked-snapshot-integration");
    let root = temporary_directory("reworked-snapshot-store");
    let first = candidate_commit_on_main(&repo, "reworked-snapshot", "1");
    git(&repo, &["checkout", "--detach", &first]);
    fs::write(repo.join("second.txt"), "second candidate change\n").unwrap();
    let second = commit_everything(&repo, "first rework");
    fs::write(repo.join("third.txt"), "third candidate change\n").unwrap();
    let candidate = commit_everything(&repo, "second rework");
    assert_ne!(candidate, second);
    git(&repo, &["checkout", "workspace"]);
    let mut daemon = started_daemon("reworked-snapshot", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let outcome = apply_candidate_to_branch(
        &mut daemon,
        &apply_request(&candidate, "workspace", &base, ApplyStrategy::CherryPick),
        &integration_root,
    )
    .unwrap();
    assert_eq!(outcome.receipt.result, ApplyResult::Applied);
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD^{tree}"]),
        git(&repo, &["rev-parse", &format!("{candidate}^{{tree}}")])
    );
    assert!(repo.join("candidate.txt").exists());
    assert!(repo.join("second.txt").exists());
    assert!(repo.join("third.txt").exists());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
fn a_new_lease_reports_a_reconciled_fact_for_the_original_application() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point,
    };
    let (repo, base) = init_repository("recover-new-lease");
    let integration_root = temporary_directory("recover-new-lease-integration");
    let root = temporary_directory("recover-new-lease-store");
    let candidate = candidate_commit_on_main(&repo, "recover-new-lease", "1");
    let mut daemon = started_daemon("recover-new-lease", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let mut request = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&repo, &["rev-parse", "HEAD"]);
    let mut store = daemon.into_store();
    request.occupancy_lease_id = "ocl_RECOVERED00000000000000000".into();
    request.occupancy_fencing_token = TOKEN + 1;
    store
        .advance_occupancy_mirror(&OccupancyMirrorUpdate {
            occupancy_lease_id: request.occupancy_lease_id.clone(),
            fencing_token: request.occupancy_fencing_token,
            holder_user_id: Some("usr_HOLDER0000000000000000000".into()),
            claim_request_id: Some("ocq_RECOVERY00000000000000000".into()),
            idle_expires_at: None,
            acknowledged_at: STAMP.into(),
        })
        .unwrap();
    let identity = load_device_identity(&store).unwrap().unwrap();
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-new-lease"),
        store,
        Arc::new(NeverTransport),
        &identity,
    )
    .unwrap();
    let outcome = apply_candidate_to_branch(&mut daemon, &request, &integration_root).unwrap();
    assert_eq!(
        outcome.receipt.resulting_commit.as_deref(),
        Some(applied.as_str())
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), applied);
    assert!(apply_result_frames(daemon.store_mut()).is_empty());
    let messages = daemon.store_mut().pending_outbox_envelopes().unwrap();
    let message = messages
        .iter()
        .find(|frame| frame.kind == "client.application.reconciled")
        .unwrap();
    let envelope: ClientToServerEnvelope = serde_json::from_slice(&message.payload).unwrap();
    let ClientToServerMessage::ApplicationReconciled(payload) = envelope.message else {
        panic!("reconciliation frame");
    };
    assert_eq!(payload.original_epoch, TOKEN);
    assert_eq!(payload.occupancy.occupancy_fencing_token, TOKEN + 1);
    assert_eq!(payload.application_id, request.application_id);
    assert_eq!(payload.approval_id, request.approval_id);
    assert_eq!(payload.result_commit.as_deref(), Some(applied.as_str()));
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}

#[test]
#[cfg(feature = "test-support")]
#[allow(clippy::too_many_lines)]
fn an_obsolete_apply_before_a_new_offer_cannot_block_crash_reconciliation() {
    use winwincode_client_port::messages::{
        CommandContext, OccupancyCommandContext, ServerCandidateApplyPayload,
        ServerOccupancyOfferPayload, ServerToClientEnvelope, ServerToClientMessage,
    };
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, apply_candidate_with_crash_point,
    };
    use winwincode_device_client::{ExchangeRequest, ExchangeResponse};
    struct RecoveryTransport {
        application: CandidateApplyRequest,
    }
    impl ExchangeTransport for RecoveryTransport {
        fn exchange(&self, bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
            let request: ExchangeRequest = serde_json::from_slice(bytes).unwrap();
            let first = &request.frames[0];
            let node = first["clientNodeId"].as_str().unwrap();
            let instance = first["clientInstanceId"].as_str().unwrap();
            let stamp = |epoch| OccupancyCommandContext {
                command: CommandContext {
                    expected_revision: 1,
                    idempotency_key: format!("recovery-{epoch}"),
                },
                occupancy_lease_id: if epoch == TOKEN {
                    LEASE.into()
                } else {
                    "ocl_00000000000000000000000002".into()
                },
                occupancy_fencing_token: epoch,
            };
            let obsolete = ServerToClientEnvelope {
                schema_version: "winwincode/v1".into(),
                message_id: "obsolete-apply".into(),
                client_node_id: node.into(),
                client_instance_id: "cix_00000000000000000000000000".into(),
                sequence: 1,
                occurred_at: STAMP.into(),
                message: ServerToClientMessage::CandidateApply(ServerCandidateApplyPayload {
                    occupancy: stamp(TOKEN),
                    application_id: self.application.application_id.clone(),
                    approval_id: self.application.approval_id.clone(),
                    candidate_digest: self.application.candidate_digest.clone(),
                    reconciled_retry: false,
                    candidate_ref: self.application.candidate_ref.clone(),
                    repository_binding_id: BINDING.into(),
                    target_branch: "workspace".into(),
                    expected_head: self.application.expected_head.clone(),
                    requester_user_id: "usr_HOLDER0000000000000000000".into(),
                    strategy: ApplyStrategy::Merge,
                }),
            };
            let offer = ServerToClientEnvelope {
                schema_version: "winwincode/v1".into(),
                message_id: "recovery-offer".into(),
                client_node_id: node.into(),
                client_instance_id: instance.into(),
                sequence: 2,
                occurred_at: STAMP.into(),
                message: ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
                    occupancy: stamp(TOKEN + 1),
                    claim_request_id: "ocq_00000000000000000000000002".into(),
                    claimed_at: STAMP.into(),
                    holder_user_id: "usr_HOLDER0000000000000000000".into(),
                    idle_expires_at: None,
                }),
            };
            Ok(serde_json::to_vec(&ExchangeResponse {
                schema_version: "winwincode/v1".into(),
                ack_sequence: request.frames.last().unwrap()["sequence"].as_u64().unwrap(),
                replay_from_sequence: None,
                frames: vec![
                    serde_json::to_value(obsolete).unwrap(),
                    serde_json::to_value(offer).unwrap(),
                ],
                enrollment: None,
            })
            .unwrap())
        }
    }
    let (repo, base) = init_repository("recover-downlink");
    let integration_root = temporary_directory("recover-downlink-integration");
    let root = temporary_directory("recover-downlink-store");
    let candidate = candidate_commit_on_main(&repo, "recover-downlink", "1");
    let mut daemon = started_daemon("recover-downlink", &root);
    bind(daemon.store_mut(), &repo);
    retain_candidate(daemon.store_mut(), &candidate, &base);
    write_candidate_ref(&repo, &candidate);
    let application = apply_request(&candidate, "workspace", &base, ApplyStrategy::Merge);
    apply_candidate_with_crash_point(
        &mut daemon,
        &application,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&repo, &["rev-parse", "HEAD"]);
    let count = git(&repo, &["rev-list", "--count", "HEAD"]);
    daemon.into_store().close().unwrap();
    let (store, identity) = open_resumed(&root);
    let mut daemon = DeviceDaemon::start(
        daemon_config("recover-downlink"),
        store,
        Arc::new(RecoveryTransport { application }),
        &identity,
    )
    .unwrap();
    for attempt in 0..4 {
        daemon
            .tick(std::time::Instant::now() + Duration::from_secs(attempt))
            .unwrap();
        if daemon.occupancy_mirror().unwrap().fencing_token == TOKEN + 1 {
            break;
        }
    }
    assert_eq!(daemon.occupancy_mirror().unwrap().fencing_token, TOKEN + 1);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), applied);
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]), count);
    let messages = daemon.store_mut().pending_outbox_envelopes().unwrap();
    let messages = messages
        .into_iter()
        .map(|frame| {
            serde_json::from_slice::<ClientToServerEnvelope>(&frame.payload)
                .unwrap()
                .message
        })
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| matches!(message, ClientToServerMessage::CommandAck(payload)
        if payload.command_message_id == "obsolete-apply" && payload.status == winwincode_client_port::domain::CommandAckStatus::RejectedStaleFencingToken)));
    assert!(messages.iter().any(|message| matches!(message, ClientToServerMessage::ApplicationReconciled(payload)
        if payload.original_epoch == TOKEN && payload.occupancy.occupancy_fencing_token == TOKEN + 1 && payload.result_commit.as_deref() == Some(applied.as_str()))));
    daemon.into_store().close().unwrap();
    cleanup(&root);
    cleanup(&integration_root);
    cleanup(&repo);
}
