use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    ControlPlaneWebSocketDeliveryChangedEvent, ControlPlaneWebSocketDeliveryChangedEventTypeValue,
};
use winwincode_control_plane::{ControlPlane, ControlPlaneConfig, EventPublisher, OutboxEvent};
use winwincode_delivery::domain::{Delivery, DeliveryStatus, StageRunStatus};
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant,
    LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateGitReleaseAuthority, CandidateGitRetentionState,
    CandidateGitTerminalOutcome, CandidateSourceManifest, FakeArtifactObjectStore,
    LocalGitSourceResolver, NewOutboxEvent, ProductStateStorage, PublicEventActor,
    PublicEventScope, PublicEventSource, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-candidate-release-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git UTF-8")
        .trim()
        .to_owned()
}

fn git_status(root: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git status command")
        .success()
}

fn commit(root: &Path, message: &str, timestamp: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn candidate_repository(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository root");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .status()
            .expect("git init")
            .success()
    );
    fs::write(root.join("candidate.txt"), b"base\n").expect("base file");
    git(root, &["add", "--", "candidate.txt"]);
    commit(root, "base", "2026-08-25T00:00:00Z");
    let base = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("candidate.txt"), b"base\ncandidate\n").expect("candidate file");
    git(root, &["add", "--", "candidate.txt"]);
    commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = git(root, &["rev-parse", "HEAD"]);
    (base, candidate)
}

fn artifact_provenance() -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        ExecutionJobId(canonical_id("job", 3)),
        1,
        LeaseId(canonical_id("lse", 4)),
        FencingToken("42".into()),
        WorkerId(canonical_id("wrk", 1)),
        WorkerInstanceId(canonical_id("wki", 2)),
        WorkerSessionId(canonical_id("wsn", 5)),
    )
    .expect("Artifact provenance")
}

fn artifact_attribution(delivery_id: DeliveryId) -> ArtifactMeteringAttribution {
    ArtifactMeteringAttribution {
        organization_id: OrganizationId(canonical_id("org", 1)),
        workspace_id: WorkspaceId(canonical_id("wsp", 1)),
        project_id: ProjectId(canonical_id("prj", 1)),
        repository_id: RepositoryId(canonical_id("rep", 1)),
        delivery_id: Some(delivery_id),
        product_session_id: Some(ProductSessionId(canonical_id("psn", 1))),
        user_id: UserId(canonical_id("usr", 1)),
    }
}

struct CandidateFixture {
    root: PathBuf,
    control: PathBuf,
    repositories: PathBuf,
    repository: PathBuf,
    delivery_id: DeliveryId,
    artifact_id: winwincode_domain::ArtifactId,
    pin: winwincode_storage::CandidateGitPinReceipt,
    candidate_commit: String,
    base_commit: String,
}

fn candidate_fixture() -> CandidateFixture {
    let root = temporary_directory("vertical");
    let control = root.join("control");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = candidate_repository(&repository);
    let delivery_id = DeliveryId("dlv_01J00000000000000000000000".into());
    let artifact_id = winwincode_domain::ArtifactId("art_0000000000000000000000000C".into());
    let bytes = CandidateSourceManifest::new(candidate_commit.clone())
        .expect("candidate manifest")
        .encode()
        .expect("candidate manifest bytes");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let scope = ReceiptScopeKey::from_encoded(b"repository:one".to_vec()).expect("scope");
    let provenance = artifact_provenance();
    let mut artifacts = ArtifactStore::open(
        control.join("artifact-catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", 1)),
            RequestId(canonical_id("req", 1)),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            bytes.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            artifact_attribution(delivery_id.clone()),
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("Artifact open");
    let receipt = artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", 2)),
            artifact_id.clone(),
            provenance.clone(),
            1_800_000_000_001,
            1,
            "application/vnd.winwincode.git-candidate+json",
            digest,
            bytes,
            true,
        ))
        .expect("final Artifact chunk");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope,
            receipt.record().artifact_id().clone(),
            receipt.record().digest().clone(),
            provenance,
        ))
        .expect("complete Artifact");
    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");
    let source = resolver
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect("validated candidate source");
    drop(artifacts);

    let ack_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
    let mut storage = SqliteStorage::open(&control).expect("retention storage");
    let pin = {
        let mut retention = storage
            .git_candidate_retention(&repositories)
            .expect("candidate retention");
        retention
            .pin_after_final_artifact_ack(&receipt, &source, &ack_digest)
            .expect("pin candidate")
    };
    assert_eq!(pin.state(), CandidateGitRetentionState::Pinned);
    Box::new(storage).close().expect("retention storage close");

    CandidateFixture {
        root,
        control,
        repositories,
        repository,
        delivery_id,
        artifact_id,
        pin,
        candidate_commit,
        base_commit,
    }
}

fn public_scope() -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId(canonical_id("org", 1)),
        workspace_id: WorkspaceId(canonical_id("wsp", 1)),
        project_id: ProjectId(canonical_id("prj", 1)),
        repository_id: RepositoryId(canonical_id("rep", 1)),
    }
}

fn delivered_with_reader(reader_open: bool, revision: u64) -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical Delivery fixture")
    .into_snapshot();
    snapshot.revision = revision;
    snapshot.status = DeliveryStatus::Delivered;
    snapshot.updated_at_millis = 1_800_000_000_100 + revision;
    let run = snapshot.stage_runs.first_mut().expect("reader StageRun");
    if reader_open {
        run.status = StageRunStatus::Waiting;
        run.finished_at_millis = None;
    } else {
        run.status = StageRunStatus::Succeeded;
        run.finished_at_millis = Some(snapshot.updated_at_millis);
    }
    Delivery::try_from_snapshot(snapshot).expect("terminal Delivery fixture")
}

fn delivery_event_id(scope_key: &ReceiptScopeKey, payload: &[u8]) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.delivery-changed-event.v1\0");
    digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope_key.as_bytes());
    digest.update((payload.len() as u64).to_be_bytes());
    digest.update(payload);
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

fn digest_label(label: &str) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(label.as_bytes())))
}

fn commit_delivery_revision(
    control: &Path,
    delivery: &Delivery,
    expected_revision: u64,
    request_seed: u64,
) -> winwincode_storage::CommitReceipt {
    let scope = public_scope();
    let scope_key = winwincode_storage::receipt_scope_key(&scope).expect("receipt scope");
    let actor = PublicEventActor::User {
        id: UserId(canonical_id("usr", 1)),
    };
    let identity = ReceiptIdentity::new(
        winwincode_storage::receipt_actor_key(&actor).expect("receipt actor"),
        scope_key.clone(),
        RequestId(canonical_id("req", request_seed)),
    )
    .expect("terminal receipt identity");
    let event_payload = serde_json::to_vec(&ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: "advanced".into(),
        delivery_id: delivery.id().clone(),
        revision: Revision(i64::try_from(delivery.revision()).expect("public revision")),
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    })
    .expect("Delivery changed payload");
    let event = NewOutboxEvent::public_projection(
        delivery_event_id(&scope_key, &event_payload),
        "delivery.changed.v1",
        event_payload,
        winwincode_storage::ProjectionEventStream::Delivery(delivery.id().clone()),
        scope,
        Instant("2027-01-15T08:00:00.000Z".into()),
        PublicEventSource::ControlPlane {
            actor,
            component: "candidate-release-vertical".into(),
        },
    )
    .expect("Delivery changed event");
    let mut storage = SqliteStorage::open(control).expect("Delivery seed storage");
    let receipt = storage
        .commit(&StateCommit::new(
            identity,
            digest_label(&format!("terminal-command-{request_seed}")),
            format!("delivery:{}", delivery.id().0),
            expected_revision,
            delivery.encode_json().expect("Delivery JSON"),
            vec![event],
        ))
        .expect("Delivery state commit");
    Box::new(storage)
        .close()
        .expect("Delivery seed storage close");
    receipt
}

fn load_pin(fixture: &CandidateFixture) -> winwincode_storage::CandidateGitPinReceipt {
    let mut storage = SqliteStorage::open(&fixture.control).expect("pin reload storage");
    let pin = {
        let mut retention = storage
            .git_candidate_retention(&fixture.repositories)
            .expect("pin reload retention");
        retention
            .load_by_artifact(&fixture.artifact_id)
            .expect("pin reload lookup")
            .expect("pin row")
    };
    Box::new(storage).close().expect("pin reload close");
    pin
}

fn retention_row(fixture: &CandidateFixture) -> (String, Option<String>) {
    let connection = rusqlite::Connection::open(fixture.control.join("control-plane.sqlite3"))
        .expect("retention inspection");
    let row = connection
        .query_row(
            "SELECT state, json_extract(record_json, '$.release')\n             FROM git_candidate_retentions WHERE artifact_id = ?1",
            [&fixture.artifact_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("retention row");
    connection.close().expect("retention inspection close");
    row
}

fn closure_counts(fixture: &CandidateFixture) -> (i64, i64, i64) {
    let connection = rusqlite::Connection::open(fixture.control.join("control-plane.sqlite3"))
        .expect("closure inspection");
    let counts = connection
        .query_row(
            "SELECT\n               (SELECT COUNT(*) FROM product_state WHERE stream_id = ?1),\n               (SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1),\n               (SELECT COUNT(*) FROM outbox WHERE topic = 'delivery.candidate.git-reads-closed.v1')",
            [format!("delivery-candidate-reads-closed:{}", fixture.delivery_id.0)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("closure counts");
    connection.close().expect("closure inspection close");
    counts
}

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(
        &mut self,
        _event: &OutboxEvent,
    ) -> Result<(), winwincode_control_plane::EventPublishError> {
        Ok(())
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn delivery_terminal_reader_hold_then_reads_closed_releases_receipt_first_exactly() {
    let fixture = candidate_fixture();
    let reader_open = delivered_with_reader(true, 1);
    let terminal_reader_open = commit_delivery_revision(&fixture.control, &reader_open, 0, 10);

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&fixture.control),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .install_git_repository_root(&fixture.repositories)
        .expect("canonical Git retention root");

    // Delivery is already terminal, but its reader StageRun remains active.
    // The closure receipt cannot be minted and no release can be authorized.
    control_plane
        .commit_candidate_git_reads_closed(
            &fixture.delivery_id,
            &terminal_reader_open,
            CandidateGitTerminalOutcome::Delivered,
        )
        .expect_err("an active reader cannot be marked reads-closed");
    let blocked_authority = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        fixture.delivery_id.clone(),
        CandidateGitTerminalOutcome::Delivered,
        terminal_reader_open.command_digest.clone(),
        digest_label("reader-still-open"),
    )
    .expect("blocked authority fixture");
    control_plane
        .release_candidate_git_after_delivery_final(&fixture.pin, &blocked_authority)
        .expect_err("terminal Delivery with an open reader must not release");
    assert_eq!(retention_row(&fixture).0, "pinned");
    assert_eq!(
        git(
            &fixture.repository,
            &["rev-parse", "--verify", fixture.pin.reference_name()]
        ),
        fixture.candidate_commit
    );

    // Close the reader as a separate durable Delivery revision.  This is the
    // exact terminal receipt that the later read-closure commit must bind.
    let reader_closed = delivered_with_reader(false, 2);
    let terminal = commit_delivery_revision(&fixture.control, &reader_closed, 1, 11);
    let reads_closed = control_plane
        .commit_candidate_git_reads_closed(
            &fixture.delivery_id,
            &terminal,
            CandidateGitTerminalOutcome::Delivered,
        )
        .expect("durable reads-closed commit");
    let expected_reads_closed = reads_closed.clone();
    assert_eq!(closure_counts(&fixture), (1, 1, 1));

    // Simulate losing the response after the outbox/state transaction.  A
    // restart reconstructs the exact closure receipt without another event.
    drop(reads_closed);
    control_plane
        .shutdown()
        .expect("Control Plane restart boundary");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&fixture.control),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane restart");
    restarted
        .install_git_repository_root(&fixture.repositories)
        .expect("canonical Git root after restart");
    let replayed_reads_closed = restarted
        .commit_candidate_git_reads_closed(
            &fixture.delivery_id,
            &terminal,
            CandidateGitTerminalOutcome::Delivered,
        )
        .expect("exact reads-closed replay");
    assert_eq!(replayed_reads_closed, expected_reads_closed);
    assert_eq!(closure_counts(&fixture), (1, 1, 1));

    // Foreign and tampered authority are rejected before the stable ref is
    // touched; a moved ref is also fail-closed and leaves the foreign target.
    let foreign = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        DeliveryId(canonical_id("dlv", 99)),
        CandidateGitTerminalOutcome::Delivered,
        terminal.command_digest.clone(),
        replayed_reads_closed.reads_closed_receipt_digest().clone(),
    )
    .expect("foreign authority fixture");
    restarted
        .release_candidate_git_after_delivery_final(&fixture.pin, &foreign)
        .expect_err("foreign Delivery authority must be rejected");
    let tampered = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        fixture.delivery_id.clone(),
        CandidateGitTerminalOutcome::Delivered,
        digest_label("tampered-terminal"),
        replayed_reads_closed.reads_closed_receipt_digest().clone(),
    )
    .expect("tampered authority fixture");
    restarted
        .release_candidate_git_after_delivery_final(&fixture.pin, &tampered)
        .expect_err("tampered terminal authority must be rejected");
    assert_eq!(retention_row(&fixture).0, "pinned");

    git(
        &fixture.repository,
        &[
            "update-ref",
            fixture.pin.reference_name(),
            fixture.base_commit.as_str(),
        ],
    );
    restarted
        .release_candidate_git_after_delivery_reads_closed(&fixture.pin, &replayed_reads_closed)
        .expect_err("tampered stable ref must be rejected");
    assert_eq!(
        git(
            &fixture.repository,
            &["rev-parse", "--verify", fixture.pin.reference_name()]
        ),
        fixture.base_commit
    );
    git(
        &fixture.repository,
        &[
            "update-ref",
            fixture.pin.reference_name(),
            fixture.candidate_commit.as_str(),
        ],
    );

    let release = restarted
        .release_candidate_git_after_delivery_reads_closed(&fixture.pin, &replayed_reads_closed)
        .expect("receipt-first release");
    assert_eq!(release.state(), CandidateGitRetentionState::Released);
    assert!(!git_status(
        &fixture.repository,
        &["rev-parse", "--verify", fixture.pin.reference_name()]
    ));
    restarted.shutdown().expect("release restart boundary");

    let mut duplicate_host = ControlPlane::start_local(
        ControlPlaneConfig::local(&fixture.control),
        Box::new(RecordingPublisher),
    )
    .expect("duplicate release restart");
    duplicate_host
        .install_git_repository_root(&fixture.repositories)
        .expect("duplicate release Git root");
    let released_pin = load_pin(&fixture);
    let duplicate = duplicate_host
        .release_candidate_git_after_delivery_reads_closed(&released_pin, &replayed_reads_closed)
        .expect("exact duplicate release");
    assert!(duplicate.is_idempotent_replay());
    assert_eq!(duplicate.receipt_digest(), release.receipt_digest());
    assert_eq!(retention_row(&fixture).0, "released");
    assert!(retention_row(&fixture).1.is_some());
    duplicate_host.shutdown().expect("duplicate host shutdown");
    fs::remove_dir_all(&fixture.root).expect("fixture cleanup");
}
