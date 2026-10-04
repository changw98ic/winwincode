// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    TEMPORARY_ROOT_LEASE_FILE, TemporaryRootLeaseConfig, TemporaryRootLeaseError,
    TemporaryRootLeaseErrorKind, TemporaryRootLeaseManager, TemporaryRootLeaseRuntime,
    TemporaryRootTarget,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-temporary-root-lease-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn child(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FakeRuntime {
    now: AtomicU64,
    next_random: AtomicU64,
    clock_failed: AtomicBool,
}

impl FakeRuntime {
    fn new(now: u64) -> Self {
        Self {
            now: AtomicU64::new(now),
            next_random: AtomicU64::new(1),
            clock_failed: AtomicBool::new(false),
        }
    }

    fn set_now(&self, now: u64) {
        self.now.store(now, Ordering::SeqCst);
    }

    fn fail_clock(&self, failed: bool) {
        self.clock_failed.store(failed, Ordering::SeqCst);
    }
}

impl TemporaryRootLeaseRuntime for FakeRuntime {
    fn now_millis(&self) -> Result<u64, TemporaryRootLeaseError> {
        if self.clock_failed.load(Ordering::SeqCst) {
            return Err(TemporaryRootLeaseError::clock_unavailable());
        }
        Ok(self.now.load(Ordering::SeqCst))
    }

    fn random_128(&self) -> Result<[u8; 16], TemporaryRootLeaseError> {
        let value = self.next_random.fetch_add(1, Ordering::SeqCst);
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&value.to_be_bytes());
        bytes[8..].copy_from_slice(&(!value).to_be_bytes());
        Ok(bytes)
    }
}

fn config(target: TemporaryRootTarget) -> TemporaryRootLeaseConfig {
    TemporaryRootLeaseConfig::try_new(100, 50, 5, target).expect("valid test lease")
}

fn manager(
    parent: &Path,
    target: TemporaryRootTarget,
    runtime: &Arc<FakeRuntime>,
) -> TemporaryRootLeaseManager {
    let runtime_port: Arc<dyn TemporaryRootLeaseRuntime> = runtime.clone();
    TemporaryRootLeaseManager::open(parent, config(target), runtime_port).expect("open manager")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("read mode").permissions().mode() & 0o777
}

#[test]
fn four_release_targets_share_the_fenced_path_lifecycle() {
    let targets = [
        (
            TemporaryRootTarget::Aarch64AppleDarwin,
            "aarch64-apple-darwin",
        ),
        (
            TemporaryRootTarget::X86_64AppleDarwin,
            "x86-64-apple-darwin",
        ),
        (
            TemporaryRootTarget::Aarch64UnknownLinuxGnu,
            "aarch64-unknown-linux-gnu",
        ),
        (
            TemporaryRootTarget::X86_64UnknownLinuxGnu,
            "x86-64-unknown-linux-gnu",
        ),
    ];
    for (target, expected_target) in targets {
        exercise_target_lifecycle(target, expected_target);
    }
}

fn exercise_target_lifecycle(target: TemporaryRootTarget, expected_target: &str) {
    let directory = TestDirectory::new(expected_target);
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let manager = manager(&directory.0, target, &runtime);
    let mut lease = manager.acquire().expect("acquire lease");
    let root = lease.path().to_path_buf();
    let marker = root.join(TEMPORARY_ROOT_LEASE_FILE);

    let canonical_parent = fs::canonicalize(&directory.0).expect("canonical test parent");
    assert_eq!(root.parent(), Some(canonical_parent.as_path()));
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("instance-")
    );
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&marker), 0o600);
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker).expect("read lease")).expect("decode lease");
    assert_eq!(record["target"], expected_target);

    runtime.set_now(900);
    lease.renew().expect("backward clock renewal");
    runtime.set_now(1_199);
    lease.renew().expect("forward jump within renewed expiry");
    assert_eq!(
        manager
            .reclaim_expired()
            .expect("active scan")
            .retained_active,
        1
    );

    runtime.set_now(1_299);
    assert_eq!(
        lease
            .renew()
            .expect_err("expired lease cannot revive")
            .kind(),
        TemporaryRootLeaseErrorKind::OwnershipLost
    );
    lease
        .release()
        .expect("owner can still release exact fenced root");
    assert!(!root.exists());
}

#[test]
fn concurrent_stale_takeover_has_one_winner_and_fences_the_old_owner() {
    let directory = TestDirectory::new("concurrent-takeover");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let manager = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let mut lease = manager.acquire().expect("acquire lease");
    let root = lease.path().to_path_buf();
    runtime.set_now(1_150);

    let barrier = Arc::new(Barrier::new(4));
    let first = spawn_cleaner(manager.clone(), Arc::clone(&barrier));
    let second = spawn_cleaner(manager, Arc::clone(&barrier));
    let owner_barrier = Arc::clone(&barrier);
    let owner = thread::spawn(move || {
        owner_barrier.wait();
        lease
            .renew()
            .expect_err("expired owner must lose the concurrent race")
            .kind()
    });
    barrier.wait();
    let first = first.join().expect("first cleaner");
    let second = second.join().expect("second cleaner");
    let owner_error = owner.join().expect("expired owner");

    assert_eq!(first.reclaimed + second.reclaimed, 1);
    assert!(!root.exists());
    assert_eq!(owner_error, TemporaryRootLeaseErrorKind::OwnershipLost);
}

fn spawn_cleaner(
    manager: TemporaryRootLeaseManager,
    barrier: Arc<Barrier>,
) -> thread::JoinHandle<winwincode_control_plane::TemporaryRootReclaimReport> {
    thread::spawn(move || {
        barrier.wait();
        manager.reclaim_expired().expect("concurrent reclaim")
    })
}

#[test]
fn target_mismatch_never_reclaims_another_release_target() {
    let directory = TestDirectory::new("target-mismatch");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let owner = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let lease = owner.acquire().expect("acquire owner lease");
    let root = lease.path().to_path_buf();
    runtime.set_now(1_150);
    let other_target = manager(
        &directory.0,
        TemporaryRootTarget::X86_64AppleDarwin,
        &runtime,
    );

    assert_eq!(
        other_target
            .reclaim_expired()
            .expect("foreign scan")
            .rejected,
        1
    );
    assert!(root.exists());
    assert_eq!(
        owner
            .reclaim_expired()
            .expect("owner target scan")
            .reclaimed,
        1
    );
    assert!(!root.exists());
    drop(lease);
}

#[test]
fn pid_reuse_tampering_and_symlinks_are_not_ownership_proof() {
    let directory = TestDirectory::new("tampering");
    let parent = directory.child("bounded-parent");
    fs::create_dir(&parent).expect("create bounded parent");
    let outside = directory.child("outside");
    fs::create_dir(&outside).expect("create outside directory");
    fs::write(outside.join("sentinel"), b"keep").expect("write sentinel");
    let pid_root = parent.join(format!("instance-{}", "1".repeat(32)));
    fs::create_dir(&pid_root).expect("create PID candidate");
    fs::write(
        pid_root.join(".winwincode-control-plane-owner"),
        std::process::id().to_string(),
    )
    .expect("write reused PID marker");
    symlink(
        &outside,
        parent.join(format!("instance-{}", "2".repeat(32))),
    )
    .expect("create outside symlink");

    let runtime = Arc::new(FakeRuntime::new(1_000));
    let manager = manager(&parent, TemporaryRootTarget::Aarch64AppleDarwin, &runtime);
    let mut lease = manager.acquire().expect("acquire real lease");
    let marker = lease.path().join(TEMPORARY_ROOT_LEASE_FILE);
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker).expect("read lease")).expect("decode lease");
    record["instanceId"] = serde_json::Value::String("a".repeat(32));
    fs::write(&marker, serde_json::to_vec(&record).expect("encode tamper")).expect("tamper marker");
    runtime.set_now(10_000);

    assert_eq!(
        lease.renew().expect_err("tampering fences owner").kind(),
        TemporaryRootLeaseErrorKind::OwnershipLost
    );
    let report = manager.reclaim_expired().expect("bounded scan");
    assert!(report.rejected >= 2);
    assert!(pid_root.exists());
    assert!(outside.join("sentinel").exists());
}

#[test]
fn reclaim_and_release_crash_quarantines_resume_deterministically() {
    let directory = TestDirectory::new("crash-resume");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let manager = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );

    resume_stale_reclaim(&manager, &runtime, &directory.0);
    restore_active_reclaim(&manager, &runtime, &directory.0);
    resume_explicit_release(&manager, &runtime, &directory.0);
}

fn resume_stale_reclaim(manager: &TemporaryRootLeaseManager, runtime: &FakeRuntime, parent: &Path) {
    runtime.set_now(2_000);
    let lease = manager.acquire().expect("acquire crash lease");
    let original = lease.path().to_path_buf();
    let quarantine = parent.join(format!(
        ".reclaim-{}-{}",
        lease.instance_id(),
        "3".repeat(32)
    ));
    drop(lease);
    runtime.set_now(2_150);
    fs::rename(&original, &quarantine).expect("simulate crash after takeover rename");
    assert_eq!(
        manager.reclaim_expired().expect("resume reclaim").reclaimed,
        1
    );
    assert!(!quarantine.exists());
}

fn restore_active_reclaim(
    manager: &TemporaryRootLeaseManager,
    runtime: &FakeRuntime,
    parent: &Path,
) {
    runtime.set_now(3_000);
    let lease = manager.acquire().expect("acquire active lease");
    let original = lease.path().to_path_buf();
    let quarantine = parent.join(format!(
        ".reclaim-{}-{}",
        lease.instance_id(),
        "4".repeat(32)
    ));
    fs::rename(&original, &quarantine).expect("simulate active takeover interruption");
    assert_eq!(
        manager.reclaim_expired().expect("restore active").restored,
        1
    );
    assert!(original.exists());
    lease.release().expect("release restored owner root");
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseClaimFixture<'a> {
    schema: &'a str,
    lease_sha256: String,
    releaser_id: &'a str,
    released_at_millis: u64,
}

fn resume_explicit_release(
    manager: &TemporaryRootLeaseManager,
    runtime: &FakeRuntime,
    parent: &Path,
) {
    runtime.set_now(4_000);
    let lease = manager.acquire().expect("acquire release lease");
    let original = lease.path().to_path_buf();
    let lease_bytes = fs::read(original.join(TEMPORARY_ROOT_LEASE_FILE)).expect("read lease");
    let releaser = "5".repeat(32);
    let claim = ReleaseClaimFixture {
        schema: "winwincode.control-plane-temporary-root-release.v1",
        lease_sha256: format!("sha256:{:x}", Sha256::digest(&lease_bytes)),
        releaser_id: &releaser,
        released_at_millis: 4_000,
    };
    write_private_record(
        &original.join(".winwincode-control-plane-release.json"),
        &serde_json::to_vec(&claim).expect("encode release claim"),
    );
    let quarantine = parent.join(format!(".release-{}-{releaser}", lease.instance_id()));
    fs::rename(&original, &quarantine).expect("simulate crash after release rename");
    drop(lease);

    assert_eq!(
        manager.reclaim_expired().expect("resume release").released,
        1
    );
    assert!(!quarantine.exists());
}

fn write_private_record(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create private record");
    file.write_all(bytes).expect("write private record");
    file.sync_all().expect("sync private record");
}

#[test]
fn renewal_failure_is_observable_and_graceful_release_remains_fenced() {
    let directory = TestDirectory::new("renewal-failure");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let manager = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let owned = manager.acquire_renewing().expect("start renewal task");
    let root = owned.path().expect("healthy initial lease").to_path_buf();
    runtime.fail_clock(true);

    let mut observed_kind = None;
    for _attempt in 0..200 {
        if let Err(error) = owned.path() {
            observed_kind = Some(error.kind());
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        observed_kind,
        Some(TemporaryRootLeaseErrorKind::ClockUnavailable)
    );
    runtime.fail_clock(false);
    owned
        .release()
        .expect("exact owner can still release after clock recovery");
    assert!(!root.exists());
}

#[test]
fn concurrent_crash_quarantine_finishers_count_exactly_one_removal() {
    for release in [false, true] {
        for iteration in 0..10 {
            let directory =
                TestDirectory::new(&format!("quarantine-contention-{release}-{iteration}"));
            let runtime = Arc::new(FakeRuntime::new(1_000));
            let owner = manager(
                &directory.0,
                TemporaryRootTarget::Aarch64AppleDarwin,
                &runtime,
            );
            let lease = owner.acquire().expect("acquire interrupted lease");
            let original = lease.path().to_path_buf();
            let lease_bytes =
                fs::read(original.join(TEMPORARY_ROOT_LEASE_FILE)).expect("read lease");
            // A real, populated root keeps recursive deletion overlapping after
            // multiple independent recovery scans validate the same crash claim.
            for item in 0..128 {
                fs::write(
                    original.join(format!("payload-{item}")),
                    b"retained root payload",
                )
                .expect("write payload");
            }
            let token = "3".repeat(32);
            let (prefix, claim_name, claim) = if release {
                (
                    ".release",
                    ".winwincode-control-plane-release.json",
                    serde_json::to_vec(&ReleaseClaimFixture {
                        schema: "winwincode.control-plane-temporary-root-release.v1",
                        lease_sha256: format!("sha256:{:x}", Sha256::digest(&lease_bytes)),
                        releaser_id: &token,
                        released_at_millis: 1_000,
                    })
                    .expect("encode release claim"),
                )
            } else {
                (
                    ".reclaim",
                    ".winwincode-control-plane-reclaim.json",
                    serde_json::to_vec(&ReclaimClaimFixture {
                        schema: "winwincode.control-plane-temporary-root-reclaim.v1",
                        lease_sha256: format!("sha256:{:x}", Sha256::digest(&lease_bytes)),
                        reclaimer_id: &token,
                        claimed_at_millis: 1_150,
                    })
                    .expect("encode reclaim claim"),
                )
            };
            write_private_record(&original.join(claim_name), &claim);
            let quarantine = directory
                .0
                .join(format!("{prefix}-{}-{token}", lease.instance_id()));
            fs::rename(&original, &quarantine).expect("simulate crash after verified claim");
            runtime.set_now(1_150);
            let barrier = Arc::new(Barrier::new(9));
            let cleaners: Vec<_> = (0..8)
                .map(|_| {
                    let independent = manager(
                        &directory.0,
                        TemporaryRootTarget::Aarch64AppleDarwin,
                        &runtime,
                    );
                    spawn_cleaner(independent, Arc::clone(&barrier))
                })
                .collect();
            barrier.wait();
            let removed: u64 = cleaners
                .into_iter()
                .map(|cleaner| {
                    let report = cleaner.join().expect("recovery scan");
                    report.reclaimed + report.released
                })
                .sum();
            assert_eq!(removed, 1, "release={release}, iteration={iteration}");
            assert!(!quarantine.exists());
            assert_eq!(
                lease
                    .release()
                    .expect_err("old owner remains fenced")
                    .kind(),
                TemporaryRootLeaseErrorKind::OwnershipLost
            );
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReclaimClaimFixture<'a> {
    schema: &'a str,
    lease_sha256: String,
    reclaimer_id: &'a str,
    claimed_at_millis: u64,
}

#[test]
fn lifecycle_lock_is_private_stable_and_independent_of_readable_parent() {
    use std::os::unix::fs::MetadataExt;
    let directory = TestDirectory::new("private-lifecycle-lock");
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).expect("readable parent");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let owner = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let parent_lock = fs::File::open(&directory.0).expect("open readable parent");
    parent_lock
        .lock()
        .expect("simulate unrelated directory lock");
    let independent = owner.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let acquisition = thread::spawn(move || send.send(independent.acquire()).expect("send result"));
    let result = receive.recv_timeout(Duration::from_secs(2));
    parent_lock
        .unlock()
        .expect("release unrelated directory lock");
    acquisition.join().expect("acquisition process");
    let lease = result
        .expect("directory reader must not block lifecycle")
        .expect("acquire lease");
    let lock_path = directory.child(".winwincode-control-plane-lifecycle.lock");
    let before = fs::metadata(&lock_path).expect("private lock");
    assert_eq!(before.mode() & 0o7777, 0o600);
    assert_eq!(before.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(before.nlink(), 1);
    lease.release().expect("release original lease");
    owner
        .acquire()
        .expect("acquire next lease")
        .release()
        .expect("release next lease");
    let after = fs::metadata(lock_path).expect("stable lock survives root deletion");
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
}

#[test]
fn lifecycle_lock_rejects_writable_parents_and_linked_or_readable_files() {
    let directory = TestDirectory::new("unsafe-lifecycle-lock");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o777)).expect("unsafe parent");
    assert_eq!(
        TemporaryRootLeaseManager::open(
            &directory.0,
            config(TemporaryRootTarget::Aarch64AppleDarwin),
            runtime.clone()
        )
        .expect_err("foreign UID must not replace lifecycle inode")
        .kind(),
        TemporaryRootLeaseErrorKind::OwnershipLost
    );
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).expect("safe parent");
    let alias = directory.child("parent-alias");
    symlink(&directory.0, &alias).expect("parent symlink");
    assert!(
        TemporaryRootLeaseManager::open(
            &alias,
            config(TemporaryRootTarget::Aarch64AppleDarwin),
            runtime.clone()
        )
        .is_err()
    );
    let owner = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let target = directory.child("retained-evidence");
    fs::write(&target, b"preserve this evidence").expect("evidence");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("private evidence");
    let lock = directory.child(".winwincode-control-plane-lifecycle.lock");
    symlink(&target, &lock).expect("lock symlink");
    assert!(owner.reclaim_expired().is_err());
    fs::remove_file(&lock).expect("remove test symlink");
    fs::hard_link(&target, &lock).expect("lock hard link");
    assert!(owner.reclaim_expired().is_err());
    fs::remove_file(&lock).expect("remove test hard link");
    fs::write(&lock, b"readable lock").expect("readable lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).expect("readable permission");
    assert!(owner.reclaim_expired().is_err());
    assert_eq!(
        fs::metadata(&lock)
            .expect("preserved mode")
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert_eq!(
        fs::read(target).expect("preserved evidence"),
        b"preserve this evidence"
    );
}

// Independent OS processes exercise the same manager and persistent proof.
// The ignored probe is executed only by its parent test with a private path.
#[test]
#[ignore = "subprocess probe for independent lifecycle tests"]
fn temporary_root_process_probe() {
    use std::io::BufRead;
    let parent = std::env::var_os("WWC_TEST_LIFECYCLE_PARENT").expect("private parent");
    if std::env::var_os("WWC_TEST_LIFECYCLE_HOLD").is_some() {
        let file =
            fs::File::open(PathBuf::from(parent).join(".winwincode-control-plane-lifecycle.lock"))
                .expect("private lock");
        file.lock().expect("hold lifecycle lock");
        println!("LOCK_HELD");
        std::io::stdout().flush().expect("flush handshake");
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .expect("wait for exit");
    } else {
        let runtime = Arc::new(FakeRuntime::new(1_150));
        let owner = manager(
            Path::new(&parent),
            TemporaryRootTarget::Aarch64AppleDarwin,
            &runtime,
        );
        let gate = std::env::var_os("WWC_TEST_LIFECYCLE_GATE").expect("private gate");
        println!("READY");
        std::io::stdout().flush().expect("flush handshake");
        while !Path::new(&gate).exists() {
            thread::sleep(Duration::from_millis(5));
        }
        let report = owner.reclaim_expired().expect("recover actual manager");
        println!("REMOVED={}", report.reclaimed + report.released);
    }
}

#[test]
fn independent_processes_reclaim_once_and_recover_after_lock_holder_dies() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let directory = TestDirectory::new("process-lifecycle-lock");
    let runtime = Arc::new(FakeRuntime::new(1_000));
    let owner = manager(
        &directory.0,
        TemporaryRootTarget::Aarch64AppleDarwin,
        &runtime,
    );
    let lease = owner.acquire().expect("original lease");
    let lease_bytes = fs::read(lease.path().join(TEMPORARY_ROOT_LEASE_FILE)).expect("lease proof");
    let token = "5".repeat(32);
    let claim = serde_json::to_vec(&ReclaimClaimFixture {
        schema: "winwincode.control-plane-temporary-root-reclaim.v1",
        lease_sha256: format!("sha256:{:x}", Sha256::digest(&lease_bytes)),
        reclaimer_id: &token,
        claimed_at_millis: 1_150,
    })
    .expect("claim proof");
    write_private_record(
        &lease.path().join(".winwincode-control-plane-reclaim.json"),
        &claim,
    );
    let quarantine = directory.child(&format!(".reclaim-{}-{token}", lease.instance_id()));
    fs::rename(lease.path(), &quarantine).expect("persist crash quarantine");
    let gate = directory.child("start-cleaners");
    let mut children = Vec::new();
    for _ in 0..8 {
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "temporary_root_process_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("WWC_TEST_LIFECYCLE_PARENT", &directory.0)
            .env("WWC_TEST_LIFECYCLE_GATE", &gate)
            .stdout(Stdio::piped())
            .spawn()
            .expect("independent cleaner");
        let mut output = BufReader::new(child.stdout.take().expect("cleaner pipe"));
        wait_for_process_output(&mut output, "READY");
        children.push((child, output));
    }
    fs::write(&gate, b"start").expect("release cleaners together");
    let mut removed = 0;
    for (mut child, output) in children {
        for line in output.lines() {
            if let Some(count) = line.expect("cleaner result").strip_prefix("REMOVED=") {
                removed += count.parse::<u64>().expect("removal count");
            }
        }
        assert!(child.wait().expect("cleaner exit").success());
    }
    assert_eq!(removed, 1);
    assert!(!quarantine.exists());
    assert_eq!(
        lease
            .release()
            .expect_err("old owner remains fenced")
            .kind(),
        TemporaryRootLeaseErrorKind::OwnershipLost
    );
    let mut holder = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "temporary_root_process_probe",
            "--ignored",
            "--nocapture",
        ])
        .env("WWC_TEST_LIFECYCLE_PARENT", &directory.0)
        .env("WWC_TEST_LIFECYCLE_HOLD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("lock holder");
    let mut output = BufReader::new(holder.stdout.take().expect("holder pipe"));
    wait_for_process_output(&mut output, "LOCK_HELD");
    holder.kill().expect("terminate this fixture lock holder");
    holder.wait().expect("release descriptors on exit");
    let (send, receive) = std::sync::mpsc::channel();
    thread::spawn(move || {
        send.send(owner.reclaim_expired())
            .expect("send recovery result");
    });
    receive
        .recv_timeout(Duration::from_secs(2))
        .expect("holder death releases lock")
        .expect("recover manager");
}

fn wait_for_process_output(output: &mut impl std::io::BufRead, expected: &str) {
    let mut line = String::new();
    loop {
        assert!(output.read_line(&mut line).expect("process readiness") > 0);
        if line.trim() == expected {
            break;
        }
        line.clear();
    }
}
