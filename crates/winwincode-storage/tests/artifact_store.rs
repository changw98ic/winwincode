use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactError, ArtifactErrorKind, ArtifactMeteringAttribution,
    ArtifactObjectStore, ArtifactOpen, ArtifactProvenance, ArtifactRetention,
    ArtifactStorageOperationKind, ArtifactStore, FakeArtifactObjectStore, LocalArtifactObjectStore,
    ReceiptScopeKey,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-artifact-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn scope(value: &str) -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(value.as_bytes().to_vec()).expect("artifact scope")
}

fn message(value: u64) -> ExecutionMessageId {
    ExecutionMessageId(format!("xmsg_{value:026}"))
}

fn request(value: u64) -> RequestId {
    RequestId(format!("req_{value:026}"))
}

fn provenance() -> ArtifactProvenance {
    provenance_for_job(ExecutionJobId("job_00000000000000000000000003".into()))
}

fn provenance_for_job(execution_job_id: ExecutionJobId) -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        execution_job_id,
        1,
        LeaseId("lse_00000000000000000000000004".into()),
        FencingToken("42".into()),
        WorkerId("wrk_00000000000000000000000001".into()),
        WorkerInstanceId("wki_00000000000000000000000002".into()),
        WorkerSessionId("wsn_00000000000000000000000005".into()),
    )
    .expect("execution artifact provenance")
}

#[test]
fn unfinished_quota_authority_is_bounded_to_one_exact_job_and_excludes_completed_artifacts() {
    let root = temporary_directory("unfinished-quota-authority");
    let mut store = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("Artifact store");
    let job_id = ExecutionJobId("job_00000000000000000000000003".into());
    let foreign_job_id = ExecutionJobId("job_00000000000000000000000013".into());
    let bytes = b"quota-authority";
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    for (seed, job) in [
        (901, job_id.clone()),
        (902, job_id.clone()),
        (903, foreign_job_id),
    ] {
        store
            .open_artifact(ArtifactOpen::new(
                scope("repository:unfinished-quota"),
                message(seed * 2),
                request(seed),
                ArtifactId(format!("art_{seed:026}")),
                "report",
                "application/octet-stream",
                digest.clone(),
                bytes.len() as u64,
                None,
                provenance_for_job(job),
                metering_attribution(),
                ArtifactRetention::Indefinite,
                1_000,
            ))
            .expect("Artifact open");
    }
    store
        .append_chunk(&artifact_chunk(
            scope("repository:unfinished-quota"),
            message(1_805),
            ArtifactId(format!("art_{:026}", 902)),
            1,
            digest,
            bytes.to_vec(),
            true,
        ))
        .expect("complete same-Job Artifact");

    let unfinished = store
        .unfinished_quota_opens_for_job(&job_id)
        .expect("unfinished quota authority");
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].artifact_id().0, format!("art_{:026}", 901));
    assert_eq!(unfinished[0].request_id(), &request(901));

    store.close().expect("Artifact close");
    let restarted = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("restart Artifact store");
    assert_eq!(
        restarted
            .unfinished_quota_opens_for_job(&job_id)
            .expect("restart unfinished authority"),
        unfinished
    );
    restarted.close().expect("restart Artifact close");
    fs::remove_dir_all(root).expect("cleanup");
}

fn metering_attribution() -> ArtifactMeteringAttribution {
    ArtifactMeteringAttribution {
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId("rep_00000000000000000000000001".into()),
        delivery_id: Some(DeliveryId("dlv_00000000000000000000000001".into())),
        product_session_id: Some(ProductSessionId("psn_00000000000000000000000001".into())),
        user_id: UserId("usr_00000000000000000000000001".into()),
    }
}

#[test]
fn concurrent_artifact_catalog_startup_converges_on_one_schema() {
    const CALLERS: usize = 8;
    const ROUNDS: usize = 32;
    let fixture_root = temporary_directory("concurrent-catalog-startup");
    for round in 0..ROUNDS {
        let root = Arc::new(fixture_root.join(round.to_string()));
        let barrier = Arc::new(Barrier::new(CALLERS));
        let callers = (0..CALLERS)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let store = ArtifactStore::open(
                        root.as_path(),
                        Box::new(FakeArtifactObjectStore::new()),
                    )
                    .map_err(|error| format!("{:?}: {error}", error.kind()))?;
                    store
                        .close()
                        .map_err(|error| format!("{:?}: {error}", error.kind()))
                })
            })
            .collect::<Vec<_>>();
        let results = callers
            .into_iter()
            .map(|caller| caller.join().expect("catalog startup thread"))
            .collect::<Vec<_>>();

        assert!(
            results.iter().all(Result::is_ok),
            "every catalog connection must open in round {round}: {results:?}"
        );
    }
    fs::remove_dir_all(fixture_root).expect("artifact fixture release");
}

fn artifact_chunk(
    scope_key: ReceiptScopeKey,
    message_id: ExecutionMessageId,
    artifact_id: ArtifactId,
    sequence: u64,
    digest: Sha256Digest,
    bytes: Vec<u8>,
    is_final: bool,
) -> ArtifactChunk {
    ArtifactChunk::new(
        scope_key,
        message_id,
        artifact_id,
        provenance(),
        1_100 + sequence,
        sequence,
        "application/octet-stream",
        digest,
        bytes,
        is_final,
    )
}

struct BarrierObjectStore {
    inner: Box<dyn ArtifactObjectStore>,
    before_chunk_write: Arc<Barrier>,
}

struct BlockingDeleteObjectStore {
    inner: FakeArtifactObjectStore,
    delete_entered: Arc<Barrier>,
    delete_release: Arc<Barrier>,
}

impl ArtifactObjectStore for BlockingDeleteObjectStore {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        self.inner.put_chunk(artifact_id, sequence, digest, bytes)
    }

    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        self.inner
            .finalize(artifact_id, last_sequence, digest, size_bytes)
    }

    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        self.inner.read(digest)
    }

    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        self.delete_entered.wait();
        self.delete_release.wait();
        self.inner.delete(digest)
    }
}

impl ArtifactObjectStore for BarrierObjectStore {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        self.before_chunk_write.wait();
        self.inner.put_chunk(artifact_id, sequence, digest, bytes)
    }

    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        self.inner
            .finalize(artifact_id, last_sequence, digest, size_bytes)
    }

    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        self.inner.read(digest)
    }

    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        self.inner.delete(digest)
    }
}

#[allow(clippy::too_many_lines)]
fn exercise_completed_artifact(name: &str, objects: Box<dyn ArtifactObjectStore>) {
    let root = temporary_directory(name);
    let mut store = ArtifactStore::open(&root, objects).expect("artifact store");
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let bytes = b"canonical artifact bytes";
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();

    let opened = store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(1),
            request(1),
            artifact_id.clone(),
            "report",
            "application/json",
            digest.clone(),
            bytes.len() as u64,
            Some("report.json".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect("artifact open");
    assert_eq!(opened.acknowledged_sequence(), 0);
    assert!(!opened.is_complete());
    let duplicate_open = store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(1),
            request(1),
            artifact_id.clone(),
            "report",
            "application/json",
            digest.clone(),
            bytes.len() as u64,
            Some("report.json".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect("exact Artifact open replay");
    assert!(duplicate_open.is_duplicate());

    let gap = store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(2),
            artifact_id.clone(),
            2,
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect_err("both adapters must reject a sequence gap");
    assert_eq!(gap.kind(), ArtifactErrorKind::SequenceGap);
    assert!(
        store
            .scan_storage_sources(None, 10)
            .expect("incomplete source page")
            .entries
            .is_empty()
    );
    assert_eq!(
        store
            .acknowledged_sequence(&artifact_scope, &artifact_id)
            .expect("Artifact write cursor"),
        0
    );

    let completed = store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(3),
            artifact_id.clone(),
            1,
            Sha256Digest(
                "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
            ),
            bytes.to_vec(),
            true,
        ))
        .expect("final artifact chunk");
    assert_eq!(completed.acknowledged_sequence(), 1);
    assert!(completed.is_complete());
    let source_page = store
        .scan_storage_sources(None, 10)
        .expect("completed source page");
    assert_eq!(source_page.snapshot_sequence, 1);
    assert_eq!(source_page.entries.len(), 1);
    let source = &source_page.entries[0];
    assert_eq!(source.sequence, 1);
    assert_eq!(source.fact.operation_id, message(3));
    assert_eq!(source.fact.request_id, request(1));
    assert_eq!(
        source.fact.operation_kind,
        ArtifactStorageOperationKind::ArtifactFinalize
    );
    assert_eq!(source.fact.bytes, bytes.len() as u64);
    assert_eq!(source.fact.attribution, metering_attribution());

    let duplicate = store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(3),
            artifact_id.clone(),
            1,
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect("exact final replay");
    assert!(duplicate.is_duplicate());
    assert_eq!(
        store
            .scan_storage_sources(None, 10)
            .expect("replayed source page")
            .entries,
        source_page.entries
    );

    let changed = store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(3),
            artifact_id.clone(),
            1,
            Sha256Digest(
                "sha256:f4c7d9e14aa144ce14a9133901b77f4fe236c746e9b6b19f43030c2c84d27876".into(),
            ),
            b"changed artifact content".to_vec(),
            true,
        ))
        .expect_err("both adapters must reject a changed repeat");
    assert_eq!(changed.kind(), ArtifactErrorKind::Conflict);

    let loaded = store
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            artifact_provenance,
        ))
        .expect("content-addressed artifact read");
    assert_eq!(loaded.bytes(), bytes);
    assert_eq!(loaded.metadata().size_bytes(), bytes.len() as u64);
    assert_eq!(loaded.metadata().kind(), "report");

    store.close().expect("artifact store close");
    let restarted = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("restart Artifact store");
    assert_eq!(
        restarted
            .scan_storage_sources(None, 10)
            .expect("restart source page")
            .entries,
        source_page.entries
    );
    restarted.close().expect("restart close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn completed_artifact_is_content_addressed_with_local_and_fake_object_adapters() {
    let local_root = temporary_directory("local-objects");
    let local = LocalArtifactObjectStore::open(&local_root).expect("local object adapter");
    exercise_completed_artifact("local", Box::new(local));
    fs::remove_dir_all(local_root).expect("local object fixture release");

    exercise_completed_artifact("fake", Box::new(FakeArtifactObjectStore::new()));
}

fn exercise_concurrent_exact_chunk<F>(
    name: &str,
    setup_objects: Box<dyn ArtifactObjectStore>,
    object_factory: F,
) where
    F: Fn() -> Box<dyn ArtifactObjectStore> + Send + Sync + 'static,
{
    let root = temporary_directory(name);
    let artifact_id = ArtifactId("art_0000000000000000000000000F".into());
    let bytes = b"canonical artifact bytes";
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let mut setup = ArtifactStore::open(&root, setup_objects).expect("setup Artifact store");
    setup
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(80),
            request(80),
            artifact_id.clone(),
            "report",
            "application/json",
            digest.clone(),
            bytes.len() as u64,
            None,
            provenance(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("Artifact open");
    setup.close().expect("setup close");

    let barrier = Arc::new(Barrier::new(2));
    let object_factory = Arc::new(object_factory);
    let mut threads = Vec::new();
    for _ in 0..2 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let object_factory = Arc::clone(&object_factory);
        let artifact_scope = artifact_scope.clone();
        let artifact_id = artifact_id.clone();
        let digest = digest.clone();
        threads.push(std::thread::spawn(move || {
            let mut store = ArtifactStore::open(
                root,
                Box::new(BarrierObjectStore {
                    inner: object_factory(),
                    before_chunk_write: barrier,
                }),
            )
            .expect("concurrent Artifact store");
            let result = store.append_chunk(&artifact_chunk(
                artifact_scope,
                message(81),
                artifact_id,
                1,
                digest,
                bytes.to_vec(),
                true,
            ));
            store.close().expect("concurrent close");
            result
        }));
    }
    let mut duplicate_flags = threads
        .into_iter()
        .map(|thread| {
            thread
                .join()
                .expect("concurrent writer")
                .expect("exact concurrent chunk must converge")
                .is_duplicate()
        })
        .collect::<Vec<_>>();
    duplicate_flags.sort_unstable();
    assert_eq!(duplicate_flags, [false, true]);

    let verifier = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("concurrent source verifier");
    assert_eq!(
        verifier
            .scan_storage_sources(None, 10)
            .expect("concurrent source page")
            .entries
            .len(),
        1
    );
    verifier.close().expect("concurrent source close");

    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn concurrent_exact_chunk_messages_converge_to_one_commit_and_one_duplicate() {
    let fake = FakeArtifactObjectStore::new();
    let fake_factory = fake.clone();
    exercise_concurrent_exact_chunk("concurrent-fake-chunk", Box::new(fake), move || {
        Box::new(fake_factory.clone())
    });

    let local_root = temporary_directory("concurrent-local-objects");
    let setup_local =
        LocalArtifactObjectStore::open(&local_root).expect("setup local object adapter");
    let concurrent_local_root = local_root.clone();
    exercise_concurrent_exact_chunk("concurrent-local-chunk", Box::new(setup_local), move || {
        Box::new(
            LocalArtifactObjectStore::open(&concurrent_local_root)
                .expect("concurrent local object adapter"),
        )
    });
    fs::remove_dir_all(local_root).expect("local object fixture release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn retention_and_shared_content_prevent_early_or_cross_artifact_deletion() {
    let root = temporary_directory("retention");
    let objects = FakeArtifactObjectStore::new();
    let object_probe = objects.clone();
    let mut store = ArtifactStore::open(&root, Box::new(objects)).expect("artifact store");
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let bytes = b"canonical artifact bytes";
    let first = ArtifactId("art_0000000000000000000000000C".into());
    let second = ArtifactId("art_0000000000000000000000000D".into());

    for (index, (artifact_id, retention)) in [
        (first.clone(), ArtifactRetention::UntilMillis(2_000)),
        (second.clone(), ArtifactRetention::UntilMillis(1_000)),
    ]
    .into_iter()
    .enumerate()
    {
        let index = u64::try_from(index).expect("fixture index");
        store
            .open_artifact(ArtifactOpen::new(
                artifact_scope.clone(),
                message(10 + index),
                request(10 + index),
                artifact_id.clone(),
                "report",
                "application/json",
                digest.clone(),
                bytes.len() as u64,
                None,
                artifact_provenance.clone(),
                metering_attribution(),
                retention,
                1_000,
            ))
            .expect("artifact open");
        store
            .append_chunk(&artifact_chunk(
                artifact_scope.clone(),
                message(20 + index),
                artifact_id,
                1,
                digest.clone(),
                bytes.to_vec(),
                true,
            ))
            .expect("artifact complete");
    }
    assert_eq!(
        object_probe
            .pending_chunk_count()
            .expect("fake staging probe"),
        0,
        "finalization must release staging chunks even when the content address already exists"
    );

    let first_access = ArtifactAccess::new(
        artifact_scope.clone(),
        first.clone(),
        digest.clone(),
        artifact_provenance.clone(),
    );
    let second_access = ArtifactAccess::new(
        artifact_scope.clone(),
        second,
        digest.clone(),
        artifact_provenance.clone(),
    );
    let early = store
        .delete(&first_access, 1_999)
        .expect_err("retained Artifact must reject early deletion");
    assert_eq!(early.kind(), ArtifactErrorKind::Retained);

    store
        .delete(&first_access, 2_000)
        .expect("first metadata deletion");
    assert_eq!(
        store
            .read_exact(&second_access)
            .expect("shared content remains")
            .bytes(),
        bytes
    );
    let deleted = store
        .read_exact(&first_access)
        .expect_err("deleted Artifact identity stays tombstoned");
    assert_eq!(deleted.kind(), ArtifactErrorKind::NotFound);

    let rebound = store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(30),
            request(30),
            first,
            "report",
            "application/json",
            digest.clone(),
            bytes.len() as u64,
            None,
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect_err("deleted ArtifactId cannot be rebound");
    assert_eq!(rebound.kind(), ArtifactErrorKind::Conflict);

    store
        .delete(&second_access, 2_000)
        .expect("last shared metadata deletion");
    assert!(
        object_probe
            .read(&digest)
            .expect("content-addressed deletion probe")
            .is_none(),
        "the last deleted reference must release its content object"
    );

    let incomplete = ArtifactId("art_0000000000000000000000000E".into());
    let empty_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest([])));
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(31),
            request(31),
            incomplete.clone(),
            "report",
            "application/json",
            empty_digest.clone(),
            0,
            None,
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(1_000),
            1_000,
        ))
        .expect("incomplete Artifact open");
    let incomplete_delete = store
        .delete(
            &ArtifactAccess::new(
                artifact_scope.clone(),
                incomplete,
                empty_digest,
                artifact_provenance.clone(),
            ),
            1_000,
        )
        .expect_err("incomplete Artifact cannot enter retention deletion");
    assert_eq!(incomplete_delete.kind(), ArtifactErrorKind::Incomplete);

    let indefinite = ArtifactId("art_0000000000000000000000000G".into());
    let held_bytes = b"held forever";
    let held_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(held_bytes)));
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(32),
            request(32),
            indefinite.clone(),
            "report",
            "application/json",
            held_digest.clone(),
            held_bytes.len() as u64,
            None,
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("indefinite Artifact open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(33),
            indefinite.clone(),
            1,
            held_digest.clone(),
            held_bytes.to_vec(),
            true,
        ))
        .expect("indefinite Artifact complete");
    let indefinite_delete = store
        .delete(
            &ArtifactAccess::new(artifact_scope, indefinite, held_digest, artifact_provenance),
            10_000,
        )
        .expect_err("indefinite retention cannot be deleted");
    assert_eq!(indefinite_delete.kind(), ArtifactErrorKind::Retained);

    store.close().expect("artifact store close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_new_content_reference_is_serialized_with_physical_deletion() {
    let root = temporary_directory("concurrent-retention");
    let objects = FakeArtifactObjectStore::new();
    let delete_entered = Arc::new(Barrier::new(2));
    let delete_release = Arc::new(Barrier::new(2));
    let bytes = b"canonical artifact bytes";
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let first = ArtifactId("art_0000000000000000000000000C".into());
    let second = ArtifactId("art_0000000000000000000000000D".into());
    let mut first_store = ArtifactStore::open(
        &root,
        Box::new(BlockingDeleteObjectStore {
            inner: objects.clone(),
            delete_entered: Arc::clone(&delete_entered),
            delete_release: Arc::clone(&delete_release),
        }),
    )
    .expect("first Artifact store");
    first_store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(90),
            request(90),
            first.clone(),
            "report",
            "application/json",
            digest.clone(),
            bytes.len() as u64,
            None,
            provenance(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(1_000),
            1_000,
        ))
        .expect("first Artifact open");
    first_store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(91),
            first.clone(),
            1,
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect("first Artifact complete");

    let first_access =
        ArtifactAccess::new(artifact_scope.clone(), first, digest.clone(), provenance());
    let deletion = std::thread::spawn(move || {
        let result = first_store.delete(&first_access, 1_000);
        first_store.close().expect("first Artifact store close");
        result
    });
    delete_entered.wait();

    let upload_started = Arc::new(Barrier::new(2));
    let (upload_complete, observed_upload) = mpsc::channel();
    let uploader_root = root.clone();
    let uploader_objects = objects.clone();
    let uploader_scope = artifact_scope.clone();
    let uploader_digest = digest.clone();
    let uploader_started = Arc::clone(&upload_started);
    let upload = std::thread::spawn(move || {
        uploader_started.wait();
        let mut store = ArtifactStore::open(&uploader_root, Box::new(uploader_objects))
            .expect("concurrent Artifact store");
        store
            .open_artifact(ArtifactOpen::new(
                uploader_scope.clone(),
                message(92),
                request(92),
                second.clone(),
                "report",
                "application/json",
                uploader_digest.clone(),
                bytes.len() as u64,
                None,
                provenance(),
                metering_attribution(),
                ArtifactRetention::Indefinite,
                1_000,
            ))
            .expect("concurrent Artifact open");
        store
            .append_chunk(&artifact_chunk(
                uploader_scope.clone(),
                message(93),
                second.clone(),
                1,
                uploader_digest.clone(),
                bytes.to_vec(),
                true,
            ))
            .expect("concurrent Artifact complete");
        upload_complete.send(()).expect("upload completion signal");
        let object = store
            .read_exact(&ArtifactAccess::new(
                uploader_scope,
                second,
                uploader_digest,
                provenance(),
            ))
            .expect("concurrent Artifact remains readable");
        assert_eq!(object.bytes(), bytes);
        store.close().expect("concurrent Artifact store close");
    });
    upload_started.wait();
    let completed_before_delete = observed_upload.recv_timeout(Duration::from_secs(1)).is_ok();
    delete_release.wait();

    deletion
        .join()
        .expect("deletion thread")
        .expect("retention deletion");
    upload.join().expect("upload thread");
    assert!(
        !completed_before_delete,
        "a new shared content reference committed while physical deletion was unguarded"
    );
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn artifact_chunks_require_the_exact_open_provenance_and_message_body() {
    let root = temporary_directory("chunk-authority");
    let mut store = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("artifact store");
    let artifact_id = ArtifactId("art_0000000000000000000000000E".into());
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let bytes = b"canonical artifact bytes";
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(35),
            request(35),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            bytes.len() as u64,
            None,
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("artifact open");

    let foreign_provenance = ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000006".into()),
        1,
        LeaseId("lse_00000000000000000000000007".into()),
        FencingToken("43".into()),
        WorkerId("wrk_00000000000000000000000008".into()),
        WorkerInstanceId("wki_00000000000000000000000009".into()),
        WorkerSessionId("wsn_0000000000000000000000000A".into()),
    )
    .expect("foreign provenance");
    let foreign = store
        .append_chunk(&ArtifactChunk::new(
            artifact_scope.clone(),
            message(36),
            artifact_id.clone(),
            foreign_provenance,
            1_100,
            1,
            "application/octet-stream",
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect_err("another ExecutionJob must not append to the opened Artifact");
    assert_eq!(foreign.kind(), ArtifactErrorKind::PermissionDenied);
    assert_eq!(
        store
            .acknowledged_sequence(&artifact_scope, &artifact_id)
            .expect("write cursor after foreign chunk"),
        0
    );

    store
        .append_chunk(&ArtifactChunk::new(
            artifact_scope.clone(),
            message(37),
            artifact_id.clone(),
            artifact_provenance.clone(),
            1_101,
            1,
            "application/octet-stream",
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect("authorized chunk");
    let changed_message = store
        .append_chunk(&ArtifactChunk::new(
            artifact_scope,
            message(37),
            artifact_id,
            artifact_provenance,
            1_102,
            1,
            "application/json",
            digest,
            bytes.to_vec(),
            true,
        ))
        .expect_err("one message identity cannot replay a changed transport body");
    assert_eq!(changed_message.kind(), ArtifactErrorKind::Conflict);

    store.close().expect("artifact store close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn artifact_reads_require_exact_scope_digest_and_execution_provenance() {
    let root = temporary_directory("authority");
    let mut store = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("artifact store");
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let bytes = b"canonical artifact bytes";
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(40),
            request(40),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            bytes.len() as u64,
            None,
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("artifact open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(41),
            artifact_id.clone(),
            1,
            digest.clone(),
            bytes.to_vec(),
            true,
        ))
        .expect("artifact complete");

    let foreign_scope = store
        .read_exact(&ArtifactAccess::new(
            scope("repository:two"),
            artifact_id.clone(),
            digest.clone(),
            artifact_provenance.clone(),
        ))
        .expect_err("foreign scope cannot read Artifact bytes");
    assert_eq!(foreign_scope.kind(), ArtifactErrorKind::PermissionDenied);

    let old_job = ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000006".into()),
        1,
        LeaseId("lse_00000000000000000000000004".into()),
        FencingToken("42".into()),
        WorkerId("wrk_00000000000000000000000001".into()),
        WorkerInstanceId("wki_00000000000000000000000002".into()),
        WorkerSessionId("wsn_00000000000000000000000005".into()),
    )
    .expect("foreign provenance");
    let rebound = store
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            old_job,
        ))
        .expect_err("an old candidate Artifact cannot be rebound to another Job");
    assert_eq!(rebound.kind(), ArtifactErrorKind::PermissionDenied);

    store.close().expect("artifact store close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn object_corruption_is_detected_before_bytes_are_returned() {
    let root = temporary_directory("corruption");
    let objects = FakeArtifactObjectStore::new();
    let corruption_probe = objects.clone();
    let mut store = ArtifactStore::open(&root, Box::new(objects)).expect("artifact store");
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(50),
            request(50),
            artifact_id.clone(),
            "test_output",
            "text/plain",
            digest.clone(),
            24,
            Some("tests.txt".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect("artifact open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(51),
            artifact_id.clone(),
            1,
            digest.clone(),
            b"canonical artifact bytes".to_vec(),
            true,
        ))
        .expect("artifact complete");
    corruption_probe
        .corrupt_object(&digest, b"changed after acceptance".to_vec())
        .expect("corrupt fake object");

    let error = store
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            artifact_provenance,
        ))
        .expect_err("corrupt bytes must fail closed");
    assert_eq!(error.kind(), ArtifactErrorKind::Corrupt);

    store.close().expect("artifact store close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn local_metadata_and_content_survive_restart_without_exposing_object_paths() {
    let root = temporary_directory("restart");
    let object_root = root.join("objects");
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    let access = ArtifactAccess::new(
        artifact_scope.clone(),
        artifact_id.clone(),
        digest.clone(),
        artifact_provenance.clone(),
    );

    let local = LocalArtifactObjectStore::open(&object_root).expect("local object adapter");
    let mut first = ArtifactStore::open(&root, Box::new(local)).expect("first artifact store");
    first
        .open_artifact(ArtifactOpen::new(
            artifact_scope,
            message(60),
            request(60),
            artifact_id.clone(),
            "diff",
            "text/x-diff",
            digest.clone(),
            24,
            Some("candidate.diff".into()),
            artifact_provenance,
            metering_attribution(),
            ArtifactRetention::UntilMillis(2_000),
            1_000,
        ))
        .expect("artifact open");
    first
        .append_chunk(&artifact_chunk(
            scope("repository:one"),
            message(61),
            artifact_id.clone(),
            1,
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"canonical "))),
            b"canonical ".to_vec(),
            false,
        ))
        .expect("first Artifact chunk");
    first.close().expect("first close");

    let local = LocalArtifactObjectStore::open(&object_root).expect("restart object adapter");
    let mut restarted =
        ArtifactStore::open(&root, Box::new(local)).expect("restart artifact store");
    assert_eq!(
        restarted
            .acknowledged_sequence(&scope("repository:one"), &artifact_id)
            .expect("restart upload cursor"),
        1
    );
    restarted
        .append_chunk(&artifact_chunk(
            scope("repository:one"),
            message(62),
            artifact_id,
            2,
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"artifact bytes"))),
            b"artifact bytes".to_vec(),
            true,
        ))
        .expect("resumed final Artifact chunk");
    assert_eq!(
        restarted
            .read_exact(&access)
            .expect("restart exact read")
            .bytes(),
        b"canonical artifact bytes"
    );
    restarted.close().expect("restart close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

#[test]
fn catalog_corruption_is_detected_before_metadata_or_bytes_are_returned() {
    let root = temporary_directory("catalog-corruption");
    let objects = FakeArtifactObjectStore::new();
    let restart_objects = objects.clone();
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let digest = Sha256Digest(
        "sha256:6b0a4af70a524f7303cb8a26242e0a8719c4747370cb51e8d1168179f272c5bc".into(),
    );
    let artifact_scope = scope("repository:one");
    let artifact_provenance = provenance();
    let access = ArtifactAccess::new(
        artifact_scope.clone(),
        artifact_id.clone(),
        digest.clone(),
        artifact_provenance.clone(),
    );
    let mut store = ArtifactStore::open(&root, Box::new(objects)).expect("artifact store");
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(70),
            request(70),
            artifact_id.clone(),
            "report",
            "application/json",
            digest.clone(),
            24,
            Some("report.json".into()),
            artifact_provenance,
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("artifact open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope,
            message(71),
            artifact_id.clone(),
            1,
            digest,
            b"canonical artifact bytes".to_vec(),
            true,
        ))
        .expect("artifact complete");
    store.close().expect("artifact close");

    let catalog = rusqlite::Connection::open(root.join("artifact-catalog.sqlite3"))
        .expect("catalog corruption injector");
    catalog
        .execute(
            "UPDATE artifacts SET kind = '' WHERE artifact_id = ?1",
            [&artifact_id.0],
        )
        .expect("corrupt Artifact metadata");
    catalog.close().expect("corruption injector close");

    let restarted =
        ArtifactStore::open(&root, Box::new(restart_objects)).expect("restart Artifact store");
    let error = restarted
        .read_exact(&access)
        .expect_err("invalid durable metadata must fail closed");
    assert_eq!(error.kind(), ArtifactErrorKind::Corrupt);

    restarted.close().expect("restart close");
    fs::remove_dir_all(root).expect("artifact fixture release");
}

fn table_shape(connection: &rusqlite::Connection, table: &str) -> Vec<(String, String, bool, u32)> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).expect("table shape");
    statement
        .query_map([], |row| {
            Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?))
        })
        .expect("shape rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("shape values")
}

fn create_legacy_catalog(root: &std::path::Path, populated: bool) {
    fs::create_dir_all(root).expect("legacy root");
    let connection =
        rusqlite::Connection::open(root.join("artifact-catalog.sqlite3")).expect("legacy catalog");
    connection
        .execute_batch(
            "CREATE TABLE artifacts (artifact_id TEXT);
             CREATE TABLE artifact_chunks (artifact_id TEXT);
             PRAGMA user_version = 1;",
        )
        .expect("legacy schema");
    if populated {
        connection
            .execute(
                "INSERT INTO artifacts (artifact_id) VALUES (?1)",
                ["art_00000000000000000000000801"],
            )
            .expect("legacy row");
    }
}

#[test]
fn empty_v1_catalog_migrates_to_the_exact_fresh_v2_shape() {
    let migrated_root = temporary_directory("empty-v1-migration");
    create_legacy_catalog(&migrated_root, false);
    ArtifactStore::open(&migrated_root, Box::new(FakeArtifactObjectStore::new()))
        .expect("empty v1 migration")
        .close()
        .expect("migrated close");

    let fresh_root = temporary_directory("fresh-v2-shape");
    ArtifactStore::open(&fresh_root, Box::new(FakeArtifactObjectStore::new()))
        .expect("fresh v2")
        .close()
        .expect("fresh close");
    let migrated = rusqlite::Connection::open(migrated_root.join("artifact-catalog.sqlite3"))
        .expect("migrated catalog");
    let fresh = rusqlite::Connection::open(fresh_root.join("artifact-catalog.sqlite3"))
        .expect("fresh catalog");
    for table in ["artifacts", "artifact_chunks", "artifact_metering_sources"] {
        assert_eq!(table_shape(&migrated, table), table_shape(&fresh, table));
    }
    assert_eq!(
        migrated
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("migrated version"),
        2
    );
    drop((migrated, fresh));
    fs::remove_dir_all(migrated_root).expect("migrated cleanup");
    fs::remove_dir_all(fresh_root).expect("fresh cleanup");
}

#[test]
fn populated_v1_catalog_fails_without_changing_version_or_rows() {
    let root = temporary_directory("populated-v1-migration");
    create_legacy_catalog(&root, true);
    let error = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .err()
        .expect("legacy attribution cannot be fabricated");
    assert_eq!(error.kind(), ArtifactErrorKind::Adapter);
    let connection =
        rusqlite::Connection::open(root.join("artifact-catalog.sqlite3")).expect("legacy verify");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("legacy version"),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM artifacts", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("legacy count"),
        1
    );
    drop(connection);
    fs::remove_dir_all(root).expect("legacy cleanup");
}

#[test]
fn malformed_metering_table_with_matching_names_fails_closed() {
    let root = temporary_directory("malformed-metering-schema");
    ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("fresh catalog")
        .close()
        .expect("fresh close");
    let catalog_path = root.join("artifact-catalog.sqlite3");
    let connection = rusqlite::Connection::open(&catalog_path).expect("schema injector");
    connection
        .execute_batch(
            "DROP TABLE artifact_metering_sources;
             CREATE TABLE artifact_metering_sources (
                sequence TEXT PRIMARY KEY,
                source_key TEXT,
                source_digest TEXT,
                fact_json TEXT,
                artifact_id TEXT,
                operation_id TEXT
             );
             CREATE TABLE preserved_metering_marker (value TEXT NOT NULL);
             INSERT INTO preserved_metering_marker VALUES ('unchanged');",
        )
        .expect("malformed schema");
    drop(connection);

    let error = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .err()
        .expect("matching names do not prove canonical schema");
    assert_eq!(error.kind(), ArtifactErrorKind::Adapter);
    let connection = rusqlite::Connection::open(catalog_path).expect("verify injector");
    assert_eq!(
        connection
            .query_row("SELECT value FROM preserved_metering_marker", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("preserved marker"),
        "unchanged"
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("preserved version"),
        2
    );
    drop(connection);
    fs::remove_dir_all(root).expect("schema cleanup");
}

fn seed_metering_source(root: &std::path::Path) {
    let bytes = b"metered artifact";
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    let artifact_id = ArtifactId("art_00000000000000000000000811".into());
    let artifact_scope = scope("repository:metering");
    let mut store =
        ArtifactStore::open(root, Box::new(FakeArtifactObjectStore::new())).expect("seed store");
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(811),
            request(811),
            artifact_id.clone(),
            "report",
            "application/octet-stream",
            digest.clone(),
            bytes.len() as u64,
            None,
            provenance(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("seed open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope,
            message(812),
            artifact_id,
            1,
            digest,
            bytes.to_vec(),
            true,
        ))
        .expect("seed completion");
    store.close().expect("seed close");
}

#[test]
fn noncanonical_durable_metering_json_is_rejected() {
    for (label, statement) in [
        (
            "attribution",
            "UPDATE artifacts
             SET metering_attribution_json = ' ' || metering_attribution_json",
        ),
        (
            "source-fact",
            "UPDATE artifact_metering_sources SET fact_json = ' ' || fact_json",
        ),
    ] {
        let root = temporary_directory(label);
        seed_metering_source(&root);
        let connection = rusqlite::Connection::open(root.join("artifact-catalog.sqlite3"))
            .expect("corruption injector");
        connection.execute(statement, []).expect("corrupt JSON");
        drop(connection);
        let restarted = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
            .expect("restart store");
        assert!(
            restarted.scan_storage_sources(None, 10).is_err(),
            "{label} JSON with noncanonical bytes must fail closed"
        );
        restarted.close().expect("restart close");
        fs::remove_dir_all(root).expect("corruption cleanup");
    }
}

#[test]
fn failed_finalization_leaves_no_storage_metering_source() {
    let root = temporary_directory("failed-finalization-source");
    let bytes = b"canonical bytes";
    let chunk_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    let declared_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"different bytes")));
    let artifact_id = ArtifactId("art_00000000000000000000000821".into());
    let artifact_scope = scope("repository:failed-metering");
    let mut store = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("Artifact store");
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(821),
            request(821),
            artifact_id.clone(),
            "report",
            "application/octet-stream",
            declared_digest,
            bytes.len() as u64,
            None,
            provenance(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("Artifact open");
    assert!(
        store
            .append_chunk(&artifact_chunk(
                artifact_scope,
                message(822),
                artifact_id,
                1,
                chunk_digest,
                bytes.to_vec(),
                true,
            ))
            .is_err()
    );
    assert!(
        store
            .scan_storage_sources(None, 10)
            .expect("failed source page")
            .entries
            .is_empty()
    );
    store.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("cleanup");
}

fn append_metered_artifact(store: &mut ArtifactStore, seed: u64) {
    let bytes = format!("metered-{seed}").into_bytes();
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let artifact_id = ArtifactId(format!("art_{seed:026}"));
    let artifact_scope = scope("repository:paged-metering");
    store
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(seed * 2),
            request(seed),
            artifact_id.clone(),
            "report",
            "application/octet-stream",
            digest.clone(),
            bytes.len() as u64,
            None,
            provenance(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_000,
        ))
        .expect("paged open");
    store
        .append_chunk(&artifact_chunk(
            artifact_scope,
            message(seed * 2 + 1),
            artifact_id,
            1,
            digest,
            bytes,
            true,
        ))
        .expect("paged completion");
}

#[test]
fn metering_cursor_keeps_a_fixed_upper_bound_across_new_completions() {
    let root = temporary_directory("metering-fixed-snapshot");
    let mut store = ArtifactStore::open(&root, Box::new(FakeArtifactObjectStore::new()))
        .expect("Artifact store");
    for seed in 830..833 {
        append_metered_artifact(&mut store, seed);
    }
    let first = store
        .scan_storage_sources(None, 2)
        .expect("first metering page");
    assert_eq!(first.snapshot_sequence, 3);
    assert_eq!(first.entries.len(), 2);
    let cursor = first.next.expect("next cursor");
    append_metered_artifact(&mut store, 833);
    let second = store
        .scan_storage_sources(Some(&cursor), 2)
        .expect("second metering page");
    assert_eq!(second.snapshot_sequence, 3);
    assert_eq!(second.entries.len(), 1);
    assert!(second.next.is_none());
    assert_eq!(
        store
            .scan_storage_sources(None, 10)
            .expect("new metering snapshot")
            .entries
            .len(),
        4
    );
    store.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("cleanup");
}
