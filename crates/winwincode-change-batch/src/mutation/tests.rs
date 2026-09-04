// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::{fs::PermissionsExt as _, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use codex_exec_server::{
    CopyOptions, CreateDirectoryOptions, ExecutorFileSystem, ExecutorFileSystemFuture,
    FileMetadata, FileSystemReadStream, FileSystemSandboxContext, GetMetadataOptions,
    ReadDirectoryEntry, ReadFileOptions, RemoveOptions, WalkOptions, WalkOutcome, WriteFileOptions,
};
use codex_utils_path_uri::PathUri;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId,
    SessionIdentity, Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        ChangeBatchIdentity, ChangeBatchProposal, ChangeBatchProposalDisposition,
        ChangeBatchProposalEvent, ValidationProfileName,
    },
};

use super::*;
use crate::{ChangeBatchPolicy, prepare_change_batch};

#[derive(Default)]
struct RecordingJournal {
    record: Option<PreparedPreimageJournalRecord>,
    durable: Arc<AtomicBool>,
    fail: Option<ExecutionJournalError>,
}

impl ExecutionJournalPort for RecordingJournal {
    fn persist_preimages_and_sync(
        &mut self,
        record: &PreparedPreimageJournalRecord,
    ) -> Result<(), ExecutionJournalError> {
        if let Some(error) = self.fail {
            return Err(error);
        }
        self.record = Some(record.clone());
        self.durable.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct CaptureRejectJournal {
    record: Option<PreparedPreimageJournalRecord>,
}

impl ExecutionJournalPort for CaptureRejectJournal {
    fn persist_preimages_and_sync(
        &mut self,
        record: &PreparedPreimageJournalRecord,
    ) -> Result<(), ExecutionJournalError> {
        self.record = Some(record.clone());
        Err(ExecutionJournalError::Unavailable)
    }
}

#[derive(Clone, Copy)]
enum FaultPoint {
    Before(usize),
    After(usize),
}

struct FaultExecutorFileSystem {
    inner: Arc<dyn ExecutorFileSystem>,
    point: FaultPoint,
    next_mutation: AtomicUsize,
    journal_durable: Arc<AtomicBool>,
}

impl FaultExecutorFileSystem {
    fn new(point: FaultPoint, journal_durable: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::clone(&codex_exec_server::LOCAL_FS),
            point,
            next_mutation: AtomicUsize::new(0),
            journal_durable,
        }
    }

    fn begin_mutation(&self) -> io::Result<(usize, bool)> {
        if !self.journal_durable.load(Ordering::SeqCst) {
            return Err(io::Error::other("mutation preceded durable journal"));
        }
        let index = self.next_mutation.fetch_add(1, Ordering::SeqCst);
        if matches!(self.point, FaultPoint::Before(fault) if fault == index) {
            return Err(io::Error::other("injected before mutation"));
        }
        Ok((
            index,
            matches!(self.point, FaultPoint::After(fault) if fault == index),
        ))
    }
}

impl ExecutorFileSystem for FaultExecutorFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        self.inner.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        self.inner.read_file(path, options, sandbox)
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        self.inner.read_file_stream(path, sandbox)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            let (_, fail_after) = self.begin_mutation()?;
            self.inner
                .write_file(path, contents, options, sandbox)
                .await?;
            if fail_after {
                return Err(io::Error::other("injected after mutation"));
            }
            Ok(())
        })
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        options: GetMetadataOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        self.inner.get_metadata(path, options, sandbox)
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        self.inner.read_directory(path, sandbox)
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        self.inner.walk(path, options, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            let (_, fail_after) = self.begin_mutation()?;
            self.inner.remove(path, options, sandbox).await?;
            if fail_after {
                return Err(io::Error::other("injected after mutation"));
            }
            Ok(())
        })
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner
            .copy(source_path, destination_path, options, sandbox)
    }
}

struct RestoreFaultFileSystem {
    inner: LocalNoFollowFileSystem,
    fail_at: usize,
    next_restore: AtomicUsize,
}

impl ChangeBatchFileSystemPort for RestoreFaultFileSystem {
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>> {
        self.inner.capture(root, paths, byte_limit)
    }

    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>> {
        self.inner.apply(root, plan, preimages)
    }

    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>> {
        let index = self.next_restore.fetch_add(1, Ordering::SeqCst);
        if index == self.fail_at {
            return Box::pin(async { Err(FileSystemMutationError::Unavailable) });
        }
        self.inner.restore_if_matches(root, path, expected, before)
    }
}

struct SymlinkSwapFileSystem {
    inner: LocalNoFollowFileSystem,
    outside: PathBuf,
}

struct CasTamperFileSystem {
    inner: LocalNoFollowFileSystem,
    tampered: AtomicBool,
}

impl ChangeBatchFileSystemPort for CasTamperFileSystem {
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>> {
        self.inner.capture(root, paths, byte_limit)
    }

    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>> {
        self.inner.apply(root, plan, preimages)
    }

    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>> {
        Box::pin(async move {
            if path == "file-00.txt" && !self.tampered.swap(true, Ordering::SeqCst) {
                write_mode(&root.join(path), "foreign\n", 0o644);
            }
            self.inner
                .restore_if_matches(root, path, expected, before)
                .await
        })
    }
}

struct ModeDriftFileSystem {
    inner: LocalNoFollowFileSystem,
}

impl ChangeBatchFileSystemPort for ModeDriftFileSystem {
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>> {
        self.inner.capture(root, paths, byte_limit)
    }

    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>> {
        Box::pin(async move {
            let report = self.inner.apply(root, plan, preimages).await?;
            fs::set_permissions(root.join("updated.txt"), fs::Permissions::from_mode(0o755))
                .map_err(|_| FileSystemMutationError::Unavailable)?;
            Ok(report)
        })
    }

    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>> {
        self.inner.restore_if_matches(root, path, expected, before)
    }
}

impl ChangeBatchFileSystemPort for SymlinkSwapFileSystem {
    fn capture<'a>(
        &'a self,
        root: &'a Path,
        paths: &'a [String],
        byte_limit: u64,
    ) -> MutationFuture<'a, Result<CapturedWorkspace, FileSystemMutationError>> {
        self.inner.capture(root, paths, byte_limit)
    }

    fn apply<'a>(
        &'a self,
        root: &'a Path,
        plan: &'a PreparedChangeBatchPlan,
        preimages: &'a CapturedWorkspace,
    ) -> MutationFuture<'a, Result<AppliedMutationReport, FileSystemMutationError>> {
        Box::pin(async move {
            std::os::unix::fs::symlink(&self.outside, root.join("added.txt"))
                .map_err(|_| FileSystemMutationError::Unavailable)?;
            self.inner.apply(root, plan, preimages).await
        })
    }

    fn restore_if_matches<'a>(
        &'a self,
        root: &'a Path,
        path: &'a str,
        expected: &'a CapturedFile,
        before: &'a CapturedFile,
    ) -> MutationFuture<'a, Result<RestoreStep, FileSystemMutationError>> {
        self.inner.restore_if_matches(root, path, expected, before)
    }
}

fn patch_digest(patch: &str) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(patch.as_bytes())))
}

fn event(patch: String) -> ChangeBatchProposalEvent {
    let digest = patch_digest(&patch);
    let run_key = "run-key-mutation";
    let turn_id = "turn-mutation";
    ChangeBatchProposalEvent {
        identity: ChangeBatchIdentity {
            attempt: 1,
            batch_id: derive_change_batch_id(run_key, turn_id, None, &digest).expect("batch ID"),
            call_id: None,
            fencing_token: FencingToken("1".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
            patch_digest: digest,
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
            run_key: run_key.to_owned(),
            session_identity: SessionIdentity {
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                stage_run_id: None,
                worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            },
            turn_id: turn_id.to_owned(),
            workspace_revision: WorkspaceRevision(
                "git-tree:0123456789abcdef0123456789abcdef01234567".to_owned(),
            ),
        },
        occurred_at: Instant("2026-09-01T00:00:00.000Z".to_owned()),
        proposal: ChangeBatchProposal {
            acceptance_criteria_ids: vec!["criterion-1".to_owned()],
            disposition: ChangeBatchProposalDisposition::Final,
            patch,
            schema_version: 1,
            validation_profile: ValidationProfileName::Fast,
        },
    }
}

fn plan(patch: &str) -> PreparedChangeBatchPlan {
    prepare_change_batch(&event(patch.to_owned()), ChangeBatchPolicy::default())
        .expect("prepared plan")
}

fn plan_with_policy(patch: &str, policy: ChangeBatchPolicy) -> PreparedChangeBatchPlan {
    prepare_change_batch(&event(patch.to_owned()), policy).expect("prepared plan")
}

fn write_mode(path: &Path, contents: &str, mode: u32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture mode");
}

fn add_patch(count: usize) -> String {
    let mut patch = String::from("*** Begin Patch\n");
    for index in 0..count {
        use std::fmt::Write as _;
        writeln!(
            &mut patch,
            "*** Add File: file-{index:02}.txt\n+created-{index}"
        )
        .expect("patch fixture");
    }
    patch.push_str("*** End Patch");
    patch
}

async fn captured_record(
    plan: &PreparedChangeBatchPlan,
    root: &Path,
) -> PreparedPreimageJournalRecord {
    let mut journal = CaptureRejectJournal::default();
    assert_eq!(
        execute_prepared_change_batch(
            plan,
            root,
            &mut journal,
            &LocalNoFollowFileSystem::default(),
        )
        .await,
        Err(ChangeBatchExecutionError::Journal(
            ExecutionJournalError::Unavailable
        ))
    );
    journal.record.expect("captured manifest")
}

const ALL_OPERATIONS_PATCH: &str = concat!(
    "*** Begin Patch\n",
    "*** Add File: added.txt\n+created\n",
    "*** Update File: updated.txt\n@@\n-old\n+new\n",
    "*** Delete File: deleted.txt\n",
    "*** Update File: moved-source.txt\n",
    "*** Move to: moved-destination.txt\n",
    "@@\n-old\n+new\n",
    "*** End Patch"
);

fn setup_all_operations(root: &Path) {
    write_mode(&root.join("updated.txt"), "old\n", 0o644);
    write_mode(&root.join("deleted.txt"), "delete me\n", 0o644);
    write_mode(&root.join("moved-source.txt"), "old\n", 0o755);
}

#[tokio::test]
async fn real_adapter_applies_all_operations_and_preserves_modes() {
    let temp = TempDir::new().expect("temp workspace");
    setup_all_operations(temp.path());
    let mut journal = RecordingJournal::default();
    let outcome = execute_prepared_change_batch(
        &plan(ALL_OPERATIONS_PATCH),
        temp.path(),
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("execute");

    assert_eq!(outcome.status(), ChangeBatchMutationStatus::Applied);
    assert_eq!(outcome.files().len(), 4);
    assert!(outcome.delta_digest().is_some());
    assert_eq!(
        fs::read_to_string(temp.path().join("added.txt")).unwrap(),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("updated.txt")).unwrap(),
        "new\n"
    );
    assert!(!temp.path().join("deleted.txt").exists());
    assert!(!temp.path().join("moved-source.txt").exists());
    assert_eq!(
        fs::read_to_string(temp.path().join("moved-destination.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::metadata(temp.path().join("added.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    assert_eq!(
        fs::metadata(temp.path().join("updated.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    assert_eq!(
        fs::metadata(temp.path().join("moved-destination.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    let record = journal.record.expect("journal record");
    assert_eq!(record.files().len(), 5);
    assert_eq!(record.total_preimage_bytes(), 18);
}

#[tokio::test]
async fn real_adapter_handles_one_ten_and_twenty_files() {
    for count in [1, 10, 20] {
        let temp = TempDir::new().expect("temp workspace");
        let mut journal = RecordingJournal::default();
        let outcome = execute_prepared_change_batch(
            &plan(&add_patch(count)),
            temp.path(),
            &mut journal,
            &LocalNoFollowFileSystem::default(),
        )
        .await
        .expect("execute");
        assert_eq!(outcome.status(), ChangeBatchMutationStatus::Applied);
        assert_eq!(outcome.files().len(), count);
    }
}

#[tokio::test]
async fn applies_one_update_with_one_hundred_actual_chunks() {
    let temp = TempDir::new().expect("temp workspace");
    let mut contents = String::new();
    let mut patch = String::from("*** Begin Patch\n*** Update File: chunks.txt\n");
    for index in 0..100 {
        use std::fmt::Write as _;
        writeln!(&mut contents, "old-{index}").expect("contents");
        writeln!(&mut patch, "@@\n-old-{index}\n+new-{index}").expect("patch");
    }
    patch.push_str("*** End Patch");
    write_mode(&temp.path().join("chunks.txt"), &contents, 0o644);
    let mut journal = RecordingJournal::default();
    let outcome = execute_prepared_change_batch(
        &plan(&patch),
        temp.path(),
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("execute");
    assert_eq!(outcome.status(), ChangeBatchMutationStatus::Applied);
    let result = fs::read_to_string(temp.path().join("chunks.txt")).expect("result");
    assert!(result.contains("new-0\n"));
    assert!(result.contains("new-99\n"));
    assert!(!result.contains("old-"));
}

#[tokio::test]
async fn preserves_crlf_during_update() {
    let temp = TempDir::new().expect("temp workspace");
    write_mode(&temp.path().join("crlf.txt"), "old\r\nsecond\r\n", 0o644);
    let patch = "*** Begin Patch\n*** Update File: crlf.txt\n@@\n-old\n+new\n*** End Patch";
    let mut journal = RecordingJournal::default();
    let outcome = execute_prepared_change_batch(
        &plan(patch),
        temp.path(),
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("execute");
    assert_eq!(outcome.status(), ChangeBatchMutationStatus::Applied);
    assert_eq!(
        fs::read(temp.path().join("crlf.txt")).expect("result"),
        b"new\r\nsecond\r\n"
    );
}

#[tokio::test]
async fn journal_failure_prevents_first_mutation() {
    let temp = TempDir::new().expect("temp workspace");
    let mut journal = RecordingJournal {
        fail: Some(ExecutionJournalError::Unavailable),
        ..RecordingJournal::default()
    };
    let result = execute_prepared_change_batch(
        &plan(&add_patch(1)),
        temp.path(),
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await;
    assert_eq!(
        result,
        Err(ChangeBatchExecutionError::Journal(
            ExecutionJournalError::Unavailable
        ))
    );
    assert!(!temp.path().join("file-00.txt").exists());
}

#[tokio::test]
async fn preimage_limit_is_enforced_before_journal_or_write() {
    let temp = TempDir::new().expect("temp workspace");
    write_mode(&temp.path().join("large.txt"), "12345\n", 0o644);
    let patch = "*** Begin Patch\n*** Delete File: large.txt\n*** End Patch";
    let strict = ChangeBatchPolicy::with_max_preimage_bytes(5).expect("strict policy");
    let mut journal = RecordingJournal::default();
    let result = execute_prepared_change_batch(
        &plan_with_policy(patch, strict),
        temp.path(),
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await;
    assert_eq!(
        result,
        Err(ChangeBatchExecutionError::PreimageLimitExceeded)
    );
    assert!(journal.record.is_none());
    assert_eq!(
        fs::read_to_string(temp.path().join("large.txt")).unwrap(),
        "12345\n"
    );
}

#[tokio::test]
async fn every_apply_mutation_fault_rolls_back_exactly_in_reverse() {
    for point in (0..5).flat_map(|index| [FaultPoint::Before(index), FaultPoint::After(index)]) {
        let temp = TempDir::new().expect("temp workspace");
        setup_all_operations(temp.path());
        let original = directory_snapshot(temp.path());
        let durable = Arc::new(AtomicBool::new(false));
        let mut journal = RecordingJournal {
            durable: Arc::clone(&durable),
            ..RecordingJournal::default()
        };
        let injected = Arc::new(FaultExecutorFileSystem::new(point, durable));
        let file_system = LocalNoFollowFileSystem::with_executor(injected);
        let outcome = execute_prepared_change_batch(
            &plan(ALL_OPERATIONS_PATCH),
            temp.path(),
            &mut journal,
            &file_system,
        )
        .await
        .expect("classified outcome");
        assert_eq!(outcome.status(), ChangeBatchMutationStatus::ExactRolledBack);
        assert_eq!(directory_snapshot(temp.path()), original);
    }
}

#[tokio::test]
async fn move_destination_write_then_source_remove_failure_rolls_back() {
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: source.txt\n",
        "*** Move to: destination.txt\n",
        "@@\n-old\n+new\n",
        "*** End Patch"
    );
    for point in [FaultPoint::Before(1), FaultPoint::After(1)] {
        let temp = TempDir::new().expect("temp workspace");
        write_mode(&temp.path().join("source.txt"), "old\n", 0o755);
        let durable = Arc::new(AtomicBool::new(false));
        let mut journal = RecordingJournal {
            durable: Arc::clone(&durable),
            ..RecordingJournal::default()
        };
        let fs = LocalNoFollowFileSystem::with_executor(Arc::new(FaultExecutorFileSystem::new(
            point, durable,
        )));
        let outcome = execute_prepared_change_batch(&plan(patch), temp.path(), &mut journal, &fs)
            .await
            .expect("classified outcome");
        assert_eq!(outcome.status(), ChangeBatchMutationStatus::ExactRolledBack);
        assert_eq!(
            fs::read_to_string(temp.path().join("source.txt")).unwrap(),
            "old\n"
        );
        assert!(!temp.path().join("destination.txt").exists());
    }
}

#[tokio::test]
async fn rollback_failure_returns_exact_partial_delta() {
    let temp = TempDir::new().expect("temp workspace");
    let durable = Arc::new(AtomicBool::new(false));
    let mut journal = RecordingJournal {
        durable: Arc::clone(&durable),
        ..RecordingJournal::default()
    };
    let applying = LocalNoFollowFileSystem::with_executor(Arc::new(FaultExecutorFileSystem::new(
        FaultPoint::Before(1),
        durable,
    )));
    let file_system = RestoreFaultFileSystem {
        inner: applying,
        fail_at: 1,
        next_restore: AtomicUsize::new(0),
    };
    let outcome = execute_prepared_change_batch(
        &plan(&add_patch(2)),
        temp.path(),
        &mut journal,
        &file_system,
    )
    .await
    .expect("classified outcome");
    assert_eq!(
        outcome.status(),
        ChangeBatchMutationStatus::PartiallyApplied
    );
    assert_eq!(outcome.files().len(), 1);
    assert!(outcome.delta_digest().is_some());
}

#[tokio::test]
async fn rollback_cas_never_overwrites_a_concurrent_change() {
    let temp = TempDir::new().expect("temp workspace");
    let durable = Arc::new(AtomicBool::new(false));
    let mut journal = RecordingJournal {
        durable: Arc::clone(&durable),
        ..RecordingJournal::default()
    };
    let applying = LocalNoFollowFileSystem::with_executor(Arc::new(FaultExecutorFileSystem::new(
        FaultPoint::Before(1),
        durable,
    )));
    let file_system = CasTamperFileSystem {
        inner: applying,
        tampered: AtomicBool::new(false),
    };
    let outcome = execute_prepared_change_batch(
        &plan(&add_patch(2)),
        temp.path(),
        &mut journal,
        &file_system,
    )
    .await
    .expect("classified outcome");
    assert_eq!(outcome.status(), ChangeBatchMutationStatus::StateUncertain);
    assert_eq!(
        fs::read_to_string(temp.path().join("file-00.txt")).unwrap(),
        "foreign\n"
    );
}

#[tokio::test]
async fn update_mode_drift_is_rejected_and_rolled_back() {
    let temp = TempDir::new().expect("temp workspace");
    write_mode(&temp.path().join("updated.txt"), "old\n", 0o644);
    let patch = "*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** End Patch";
    let mut journal = RecordingJournal::default();
    let file_system = ModeDriftFileSystem {
        inner: LocalNoFollowFileSystem::default(),
    };
    let outcome =
        execute_prepared_change_batch(&plan(patch), temp.path(), &mut journal, &file_system)
            .await
            .expect("classified outcome");
    assert_eq!(outcome.status(), ChangeBatchMutationStatus::ExactRolledBack);
    assert_eq!(
        fs::read_to_string(temp.path().join("updated.txt")).unwrap(),
        "old\n"
    );
    assert_eq!(
        fs::metadata(temp.path().join("updated.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
}

#[tokio::test]
async fn symlink_swap_never_writes_outside_and_is_uncertain() {
    let workspace = TempDir::new().expect("temp workspace");
    let outside = TempDir::new().expect("outside workspace");
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "untouched\n").expect("outside fixture");
    let mut journal = RecordingJournal::default();
    let file_system = SymlinkSwapFileSystem {
        inner: LocalNoFollowFileSystem::default(),
        outside: outside_file.clone(),
    };
    let patch = "*** Begin Patch\n*** Add File: added.txt\n+changed\n*** End Patch";
    let outcome =
        execute_prepared_change_batch(&plan(patch), workspace.path(), &mut journal, &file_system)
            .await
            .expect("classified outcome");
    assert_eq!(outcome.status(), ChangeBatchMutationStatus::StateUncertain);
    assert_eq!(fs::read_to_string(outside_file).unwrap(), "untouched\n");
}

#[tokio::test]
async fn recovery_classifies_before_after_partial_and_other_without_reapply() {
    let before_root = TempDir::new().expect("before workspace");
    let two_adds = plan(&add_patch(2));
    let record = captured_record(&two_adds, before_root.path()).await;
    validate_preimage_journal_record(&two_adds, &record).expect("valid manifest");

    let before = recover_prepared_change_batch(
        &two_adds,
        before_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover before");
    assert_eq!(before.status(), ChangeBatchMutationStatus::PreMutation);

    let applied_root = TempDir::new().expect("applied workspace");
    write_mode(
        &applied_root.path().join("file-00.txt"),
        "created-0\n",
        0o644,
    );
    write_mode(
        &applied_root.path().join("file-01.txt"),
        "created-1\n",
        0o644,
    );
    let applied = recover_prepared_change_batch(
        &two_adds,
        applied_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover applied");
    assert_eq!(applied.status(), ChangeBatchMutationStatus::Applied);
    assert_eq!(applied.files().len(), 2);

    let partial_root = TempDir::new().expect("partial workspace");
    write_mode(
        &partial_root.path().join("file-00.txt"),
        "created-0\n",
        0o644,
    );
    let rolled_back = recover_prepared_change_batch(
        &two_adds,
        partial_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover partial");
    assert_eq!(
        rolled_back.status(),
        ChangeBatchMutationStatus::ExactRolledBack
    );
    assert!(!partial_root.path().join("file-00.txt").exists());

    let other_root = TempDir::new().expect("other workspace");
    write_mode(&other_root.path().join("file-00.txt"), "foreign\n", 0o644);
    let uncertain = recover_prepared_change_batch(
        &two_adds,
        other_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover other");
    assert_eq!(
        uncertain.status(),
        ChangeBatchMutationStatus::StateUncertain
    );
    assert_eq!(
        fs::read_to_string(other_root.path().join("file-00.txt")).unwrap(),
        "foreign\n"
    );
}

#[tokio::test]
async fn recovery_handles_full_and_mid_move_states_with_all_operations() {
    let source = TempDir::new().expect("source workspace");
    setup_all_operations(source.path());
    let all = plan(ALL_OPERATIONS_PATCH);
    let record = captured_record(&all, source.path()).await;

    let applied_root = TempDir::new().expect("applied workspace");
    write_mode(&applied_root.path().join("added.txt"), "created\n", 0o644);
    write_mode(&applied_root.path().join("updated.txt"), "new\n", 0o644);
    write_mode(
        &applied_root.path().join("moved-destination.txt"),
        "new\n",
        0o755,
    );
    let applied = recover_prepared_change_batch(
        &all,
        applied_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover all applied");
    assert_eq!(applied.status(), ChangeBatchMutationStatus::Applied);
    assert_eq!(applied.files().len(), 4);

    let mid_move_root = TempDir::new().expect("mid-move workspace");
    setup_all_operations(mid_move_root.path());
    write_mode(
        &mid_move_root.path().join("moved-destination.txt"),
        "new\n",
        0o755,
    );
    let rolled_back = recover_prepared_change_batch(
        &all,
        mid_move_root.path(),
        &record,
        &LocalNoFollowFileSystem::default(),
    )
    .await
    .expect("recover mid move");
    assert_eq!(
        rolled_back.status(),
        ChangeBatchMutationStatus::ExactRolledBack
    );
    assert_eq!(
        fs::read_to_string(mid_move_root.path().join("moved-source.txt")).unwrap(),
        "old\n"
    );
    assert!(!mid_move_root.path().join("moved-destination.txt").exists());
}

#[tokio::test]
async fn persisted_manifest_rebuild_rejects_byte_and_expected_state_tampering() {
    let temp = TempDir::new().expect("temp workspace");
    write_mode(&temp.path().join("updated.txt"), "old\n", 0o644);
    let update =
        plan("*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** End Patch");
    let record = captured_record(&update, temp.path()).await;
    let rebuilt = rebuild_preimage_journal_record(
        &update,
        record.preimage_digest().clone(),
        record.total_preimage_bytes(),
        record.files().to_vec(),
    )
    .expect("rebuild valid record");
    assert_eq!(rebuilt, record);

    let original = &record.files()[0];
    let changed_bytes = FilePreimage::from_persisted(
        original.path().to_owned(),
        Some(b"changed\n".to_vec()),
        original.digest().cloned(),
        original.mode().map(ToOwned::to_owned),
        original.expected_after_digest().cloned(),
        original.expected_after_mode().map(ToOwned::to_owned),
    );
    assert_eq!(
        rebuild_preimage_journal_record(
            &update,
            record.preimage_digest().clone(),
            record.total_preimage_bytes(),
            vec![changed_bytes],
        ),
        Err(PreimageJournalValidationError::BeforeState)
    );

    let changed_expected = FilePreimage::from_persisted(
        original.path().to_owned(),
        original.bytes().map(ToOwned::to_owned),
        original.digest().cloned(),
        original.mode().map(ToOwned::to_owned),
        Some(Sha256Digest(format!("sha256:{}", "f".repeat(64)))),
        original.expected_after_mode().map(ToOwned::to_owned),
    );
    assert_eq!(
        rebuild_preimage_journal_record(
            &update,
            record.preimage_digest().clone(),
            record.total_preimage_bytes(),
            vec![changed_expected],
        ),
        Err(PreimageJournalValidationError::Digest)
    );
}

#[tokio::test]
async fn preflight_rejects_wrong_existence_entry_type_links_binary_and_mode() {
    let cases = [
        (
            "add-existing",
            "*** Begin Patch\n*** Add File: target\n+x\n*** End Patch",
        ),
        (
            "update-missing",
            "*** Begin Patch\n*** Update File: target\n@@\n-a\n+b\n*** End Patch",
        ),
        (
            "delete-missing",
            "*** Begin Patch\n*** Delete File: target\n*** End Patch",
        ),
        (
            "move-missing",
            "*** Begin Patch\n*** Update File: target\n*** Move to: destination\n@@\n-a\n+b\n*** End Patch",
        ),
    ];
    for (label, patch) in cases {
        let temp = TempDir::new().expect("temp workspace");
        if label == "add-existing" {
            write_mode(&temp.path().join("target"), "a\n", 0o644);
        }
        assert_preflight_rejected(temp.path(), patch).await;
    }

    let temp = TempDir::new().expect("temp workspace");
    write_mode(&temp.path().join("source"), "a\n", 0o644);
    write_mode(&temp.path().join("destination"), "occupied\n", 0o644);
    assert_preflight_rejected(
        temp.path(),
        "*** Begin Patch\n*** Update File: source\n*** Move to: destination\n@@\n-a\n+b\n*** End Patch",
    )
    .await;

    let temp = TempDir::new().expect("temp workspace");
    assert_preflight_rejected(
        temp.path(),
        "*** Begin Patch\n*** Add File: missing-parent/target\n+x\n*** End Patch",
    )
    .await;

    for kind in [
        "directory",
        "fifo",
        "socket",
        "symlink",
        "hardlink",
        "binary",
        "mode",
    ] {
        let temp = TempDir::new().expect("temp workspace");
        let target = temp.path().join("target");
        match kind {
            "directory" => fs::create_dir(&target).expect("directory fixture"),
            "fifo" => {
                let status = std::process::Command::new("mkfifo")
                    .arg(&target)
                    .status()
                    .expect("run mkfifo");
                assert!(status.success());
            }
            "socket" => drop(UnixListener::bind(&target).expect("socket fixture")),
            "symlink" => {
                let real = temp.path().join("real");
                write_mode(&real, "a\n", 0o644);
                std::os::unix::fs::symlink(real, &target).expect("symlink fixture");
            }
            "hardlink" => {
                let other = temp.path().join("other");
                write_mode(&other, "a\n", 0o644);
                fs::hard_link(other, &target).expect("hardlink fixture");
            }
            "binary" => fs::write(&target, [0, 1, 2]).expect("binary fixture"),
            "mode" => write_mode(&target, "a\n", 0o600),
            _ => unreachable!(),
        }
        let patch = "*** Begin Patch\n*** Delete File: target\n*** End Patch";
        assert_preflight_rejected(temp.path(), patch).await;
    }

    let temp = TempDir::new().expect("temp workspace");
    fs::write(temp.path().join("target"), [0xff, 0xfe]).expect("invalid UTF-8 fixture");
    assert_preflight_rejected(
        temp.path(),
        "*** Begin Patch\n*** Delete File: target\n*** End Patch",
    )
    .await;

    let temp = TempDir::new().expect("temp workspace");
    let outside = TempDir::new().expect("outside");
    write_mode(&outside.path().join("target"), "a\n", 0o644);
    std::os::unix::fs::symlink(outside.path(), temp.path().join("linked"))
        .expect("ancestor symlink");
    assert_preflight_rejected(
        temp.path(),
        "*** Begin Patch\n*** Delete File: linked/target\n*** End Patch",
    )
    .await;

    assert_eq!(
        capture_file(Path::new("/dev/null"), 1),
        Err(FileSystemMutationError::InvalidEntry)
    );
}

async fn assert_preflight_rejected(root: &Path, patch: &str) {
    let mut journal = RecordingJournal::default();
    let result = execute_prepared_change_batch(
        &plan(patch),
        root,
        &mut journal,
        &LocalNoFollowFileSystem::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(ChangeBatchExecutionError::InvalidPreflightState)
    ));
    assert!(journal.record.is_none());
}

fn directory_snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).expect("read workspace") {
        let entry = entry.expect("directory entry");
        let metadata = entry.metadata().expect("metadata");
        if metadata.is_file() {
            result.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (
                    fs::read(entry.path()).expect("file bytes"),
                    metadata.permissions().mode() & 0o7777,
                ),
            );
        }
    }
    result
}
