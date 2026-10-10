// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ControlPlaneEventId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken,
    Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactError, ArtifactMeteringAttribution, ArtifactObjectRange,
    ArtifactObjectStore, ArtifactOpen, ArtifactProvenance, ArtifactRetention, ArtifactStore,
    FakeArtifactObjectStore, NewOutboxEvent, ProductStateStorage, ProjectionEventStream,
    ProjectionEventStreamKey, PublicEventActor, PublicEventScope, PublicEventSource,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, receipt_actor_key,
    receipt_scope_key,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn directory() -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-read-snapshot-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn scope() -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId("rep_00000000000000000000000001".into()),
    }
}

fn stream() -> ProjectionEventStream {
    ProjectionEventStream::Delivery(DeliveryId("dlv_00000000000000000000000001".into()))
}

fn commit(revision: u64) -> StateCommit {
    let actor = PublicEventActor::User {
        id: UserId("usr_00000000000000000000000001".into()),
    };
    StateCommit::new(
        ReceiptIdentity::new(
            receipt_actor_key(&actor).unwrap(),
            receipt_scope_key(&scope()).unwrap(),
            RequestId(format!("req_{revision:026}")),
        )
        .unwrap(),
        Sha256Digest(format!("sha256:{revision:064x}")),
        "snapshot-state",
        revision - 1,
        format!("revision-{revision}").into_bytes(),
        vec![
            NewOutboxEvent::public_projection(
                ControlPlaneEventId(format!("evt_{revision:026}")),
                "projection.invalidated",
                b"{}".to_vec(),
                stream(),
                scope(),
                Instant("2026-10-08T00:00:00.000Z".into()),
                PublicEventSource::ControlPlane {
                    actor,
                    component: "read-snapshot-test".into(),
                },
            )
            .unwrap(),
        ],
    )
}

#[test]
fn snapshot_pins_state_and_runtime_cursor_before_first_caller_read_and_rejects_writes() {
    let root = directory();
    let mut writer = SqliteStorage::open(&root).unwrap();
    writer.commit(&commit(1)).unwrap();
    let mut snapshot = SqliteStorage::open_read_snapshot(writer.database_path()).unwrap();
    // A writer remains free to commit while the read transaction is retained.
    writer.commit(&commit(2)).unwrap();
    assert_eq!(
        snapshot
            .load_state("snapshot-state")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let key =
        ProjectionEventStreamKey::new(receipt_scope_key(&scope()).unwrap(), stream()).unwrap();
    let cut = snapshot
        .load_projection_read_cut(&["snapshot-state".into()], &key, None)
        .unwrap();
    assert_eq!(cut.states()[0].revision, 1);
    assert_eq!(cut.projection_event_cursor().sequence(), 1);
    assert_eq!(
        writer
            .load_projection_read_cut(&["snapshot-state".into()], &key, None)
            .unwrap()
            .projection_event_cursor()
            .sequence(),
        2
    );
    let rejected = snapshot.mark_published("snapshot-probe").unwrap_err();
    assert!(rejected.to_string().contains("read-only"), "{rejected}");
    Box::new(snapshot).close().unwrap();
    writer.commit(&commit(3)).unwrap();
    Box::new(writer).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_rejects_git_retention_before_recovery_and_preserves_refs_and_owner() {
    let root = directory();
    let repository = root.join("repository");
    fs::create_dir_all(&repository).unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "-c",
        "user.name=WinWinCode Fixture",
        "-c",
        "user.email=fixture@winwincode.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "snapshot fixture",
    ]);
    let refs = git(&["show-ref"]);
    let mut writer = SqliteStorage::open(root.join("data")).unwrap();
    writer.commit(&commit(1)).unwrap();
    // The original owner can open this exact root, so rejection below cannot
    // be explained by an invalid repository fixture or absent authority.
    drop(writer.git_candidate_retention(&repository).unwrap());
    let mut snapshot = SqliteStorage::open_read_snapshot(writer.database_path()).unwrap();
    assert!(snapshot.git_candidate_retention(&repository).is_err());
    assert_eq!(git(&["show-ref"]), refs);
    assert_eq!(
        writer
            .load_state("snapshot-state")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    drop(writer.git_candidate_retention(&repository).unwrap());
    Box::new(snapshot).close().unwrap();
    writer.commit(&commit(2)).unwrap();
    Box::new(writer).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

fn provenance() -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000001".into()),
        1,
        LeaseId("lse_00000000000000000000000001".into()),
        FencingToken("42".into()),
        WorkerId("wrk_00000000000000000000000001".into()),
        WorkerInstanceId("wki_00000000000000000000000001".into()),
        WorkerSessionId("wsn_00000000000000000000000001".into()),
    )
    .unwrap()
}

fn artifact_scope() -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(b"snapshot-artifact-scope".to_vec()).unwrap()
}

fn artifact_open(seed: u64) -> ArtifactOpen {
    ArtifactOpen::new(
        artifact_scope(),
        ExecutionMessageId(format!("xmsg_{seed:026}")),
        RequestId(format!("req_{seed:026}")),
        ArtifactId(format!("art_{seed:026}")),
        "report",
        "application/octet-stream",
        digest(),
        5,
        None,
        provenance(),
        ArtifactMeteringAttribution {
            organization_id: OrganizationId("org_00000000000000000000000001".into()),
            workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
            project_id: ProjectId("prj_00000000000000000000000001".into()),
            repository_id: RepositoryId("rep_00000000000000000000000001".into()),
            delivery_id: Some(DeliveryId("dlv_00000000000000000000000001".into())),
            product_session_id: Some(ProductSessionId("psn_00000000000000000000000001".into())),
            user_id: UserId("usr_00000000000000000000000001".into()),
        },
        ArtifactRetention::Indefinite,
        1_000,
    )
}

fn digest() -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"hello")))
}

struct WriteProbe {
    objects: FakeArtifactObjectStore,
    writes: Arc<AtomicU64>,
}

impl ArtifactObjectStore for WriteProbe {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.objects.put_chunk(artifact_id, sequence, digest, bytes)
    }
    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.objects
            .finalize(artifact_id, last_sequence, digest, size_bytes)
    }
    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        self.objects.read(digest)
    }
    fn read_range(
        &self,
        digest: &Sha256Digest,
        size_bytes: u64,
        offset: u64,
        length: u64,
    ) -> Result<Option<ArtifactObjectRange>, ArtifactError> {
        self.objects.read_range(digest, size_bytes, offset, length)
    }
    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.objects.delete(digest)
    }
}

#[test]
fn artifact_snapshot_retains_metadata_but_revalidates_live_bytes_and_keeps_owner_open() {
    let root = directory();
    let objects = FakeArtifactObjectStore::new();
    let writes = Arc::new(AtomicU64::new(0));
    let mut owner = ArtifactStore::open(
        &root,
        Box::new(WriteProbe {
            objects: objects.clone(),
            writes: Arc::clone(&writes),
        }),
    )
    .unwrap();
    owner.open_artifact(artifact_open(1)).unwrap();
    owner
        .append_chunk(&ArtifactChunk::new(
            artifact_scope(),
            ExecutionMessageId(format!("xmsg_{:026}", 2)),
            ArtifactId(format!("art_{:026}", 1)),
            provenance(),
            1_001,
            1,
            "application/octet-stream",
            digest(),
            b"hello".to_vec(),
            true,
        ))
        .unwrap();
    owner.open_artifact(artifact_open(3)).unwrap();
    let mut snapshot = owner.read_snapshot().unwrap();
    let access = ArtifactAccess::new(
        artifact_scope(),
        ArtifactId(format!("art_{:026}", 1)),
        digest(),
        provenance(),
    );
    assert_eq!(snapshot.read_exact(&access).unwrap().bytes(), b"hello");
    owner.open_artifact(artifact_open(5)).unwrap();
    assert_eq!(
        owner
            .acknowledged_sequence(&artifact_scope(), &ArtifactId(format!("art_{:026}", 5)))
            .unwrap(),
        0
    );
    assert!(
        snapshot
            .acknowledged_sequence(&artifact_scope(), &ArtifactId(format!("art_{:026}", 5)))
            .is_err()
    );
    assert!(snapshot.open_artifact(artifact_open(4)).is_err());
    let writes_before = writes.load(Ordering::SeqCst);
    assert!(
        snapshot
            .append_chunk(&ArtifactChunk::new(
                artifact_scope(),
                ExecutionMessageId(format!("xmsg_{:026}", 6)),
                ArtifactId(format!("art_{:026}", 3)),
                provenance(),
                1_001,
                1,
                "application/octet-stream",
                digest(),
                b"hello".to_vec(),
                true
            ))
            .is_err()
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        writes_before,
        "a rejected snapshot write must never call the shared object adapter"
    );
    assert_eq!(objects.pending_chunk_count().unwrap(), 0);
    assert_eq!(
        owner
            .acknowledged_sequence(&artifact_scope(), &ArtifactId(format!("art_{:026}", 3)))
            .unwrap(),
        0
    );
    objects
        .corrupt_object(&digest(), b"wrong".to_vec())
        .unwrap();
    assert!(
        snapshot.read_exact(&access).is_err(),
        "a pinned catalog must never mask corrupt bytes"
    );
    objects
        .corrupt_object(&digest(), b"hello".to_vec())
        .unwrap();
    snapshot.close().unwrap();
    assert_eq!(
        owner.read_exact(&access).unwrap().bytes(),
        b"hello",
        "closing a snapshot must not close the installed adapter"
    );
    owner.close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn normal_close_returns_while_snapshot_and_writer_remain_held() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).unwrap();
    storage.commit(&commit(1)).unwrap();
    let snapshot = SqliteStorage::open_read_snapshot(storage.database_path()).unwrap();
    storage.commit(&commit(2)).unwrap();
    let lock = rusqlite::Connection::open(storage.database_path()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let join_handle = std::thread::spawn(move || {
        assert!(closed_tx.send(Box::new(storage).close()).is_ok());
    });

    // Capture the deadline result while both conflicting fixtures remain held.
    let closed = closed_rx.recv_timeout(std::time::Duration::from_secs(1));
    // Release and join before asserting, including on the expected old-code timeout.
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    Box::new(snapshot).close().unwrap();
    join_handle.join().unwrap();
    fs::remove_dir_all(root).unwrap();
    closed
        .expect("normal close must finish before pinned reader and writer release")
        .expect("normal close must succeed while reader and writer remain held");
}

#[test]
fn normal_close_preserves_pinned_cut_and_allows_reopen_commit() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).unwrap();
    let first = storage.commit(&commit(1)).unwrap();
    let mut snapshot = SqliteStorage::open_read_snapshot(storage.database_path()).unwrap();
    let second = storage.commit(&commit(2)).unwrap();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let join_handle = std::thread::spawn(move || {
        assert!(closed_tx.send(Box::new(storage).close()).is_ok());
    });
    let closed = closed_rx.recv_timeout(std::time::Duration::from_secs(1));
    let Ok(closed) = closed else {
        Box::new(snapshot).close().unwrap();
        join_handle.join().unwrap();
        fs::remove_dir_all(root).unwrap();
        panic!("normal close waited for the pinned snapshot: {closed:?}");
    };
    join_handle.join().unwrap();
    closed.expect("normal close must succeed before releasing the snapshot");

    let reopen_root = root.clone();
    let (reopened_tx, reopened_rx) = std::sync::mpsc::channel();
    let opener = std::thread::spawn(move || {
        let result = SqliteStorage::open(reopen_root)
            .and_then(|mut reopened| reopened.commit(&commit(3)).map(|third| (reopened, third)));
        assert!(reopened_tx.send(result).is_ok());
    });
    let reopened = reopened_rx.recv_timeout(std::time::Duration::from_secs(1));
    let Ok(reopened) = reopened else {
        Box::new(snapshot).close().unwrap();
        opener.join().unwrap();
        fs::remove_dir_all(root).unwrap();
        panic!("reopen and commit waited for the pinned snapshot");
    };
    opener.join().unwrap();
    let (mut reopened, third) = reopened.expect("real reopen and commit must succeed");
    let key =
        ProjectionEventStreamKey::new(receipt_scope_key(&scope()).unwrap(), stream()).unwrap();
    let state_ids = ["snapshot-state".to_owned()];
    let pinned = snapshot
        .load_projection_read_cut(&state_ids, &key, None)
        .unwrap();
    assert_eq!(pinned.states()[0].revision, 1);
    assert_eq!(pinned.states()[0].payload, b"revision-1");
    assert_eq!(pinned.projection_event_cursor().sequence(), 1);
    assert!(
        snapshot
            .mark_published("snapshot-probe")
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );

    let receipts = [first, second, third];
    let expected_events: Vec<_> = receipts
        .iter()
        .flat_map(|receipt| receipt.events.clone())
        .collect();
    let verify = |storage: &mut SqliteStorage| {
        let cut = storage
            .load_projection_read_cut(&state_ids, &key, None)
            .unwrap();
        assert_eq!(cut.states()[0].revision, 3);
        assert_eq!(cut.states()[0].payload, b"revision-3");
        assert_eq!(cut.projection_event_cursor().sequence(), 3);
        for (revision, expected) in (1..=3).zip(&receipts) {
            let mut replay = storage.commit(&commit(revision)).unwrap();
            assert!(replay.idempotent_replay);
            replay.idempotent_replay = false;
            assert_eq!(&replay, expected);
        }
        assert_eq!(storage.pending_events().unwrap(), expected_events);
    };
    verify(&mut reopened);
    Box::new(snapshot).close().unwrap();
    Box::new(reopened).close().unwrap();
    let mut restarted = SqliteStorage::open(&root).unwrap();
    verify(&mut restarted);
    Box::new(restarted).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}
