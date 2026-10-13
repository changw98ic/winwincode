// SPDX-License-Identifier: Apache-2.0

//! Real subprocesses exercise the daemon/CLI store initialization boundary.

use super::{CLIENT_STORE_SCHEMA_VERSION, DeviceStore, DeviceStoreErrorKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ROLE_ENV: &str = "WWC_TEST_DEVICE_OPEN_ROLE";
const ROOT_ENV: &str = "WWC_TEST_DEVICE_OPEN_ROOT";
const GATE_ENV: &str = "WWC_TEST_DEVICE_OPEN_GATE";
const CHILD_TEST: &str =
    "store::open_process_concurrency_tests::device_store_open_subprocess_entry";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn child_gate() -> Option<PathBuf> {
    std::env::var_os(GATE_ENV).map(PathBuf::from)
}

pub(super) fn after_sqlite_connection_opened() {
    if std::env::var(ROLE_ENV).as_deref() != Ok("holder") {
        return;
    }
    let gate = child_gate().expect("holder gate");
    fs::write(
        gate.join("holder.sqlite-opened"),
        std::process::id().to_string(),
    )
    .expect("record real SQLite-open boundary");
    wait_for(&gate.join("release"), Duration::from_secs(10));
}

pub(super) fn on_process_open_lock_contention() {
    if std::env::var(ROLE_ENV).as_deref() != Ok("contender") {
        return;
    }
    let gate = child_gate().expect("contender gate");
    let marker = gate.join("contender.process-lock-busy");
    if !marker.exists() {
        fs::write(marker, std::process::id().to_string())
            .expect("record actual process-lock contention");
    }
}

#[test]
fn device_store_open_subprocess_entry() {
    let Some(role) = std::env::var_os(ROLE_ENV) else {
        return;
    };
    let root = std::env::var_os(ROOT_ENV).expect("child store root");
    let gate = child_gate().expect("child gate");
    let started = Instant::now();
    let result = DeviceStore::open(root);
    let elapsed_ms = started.elapsed().as_millis();
    let receipt = match result {
        Ok(store) => {
            let connection = store.connection().expect("real store connection");
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("committed schema version");
            let journal_mode: String = connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .expect("committed WAL mode");
            let integrity: String = connection
                .query_row("PRAGMA quick_check", [], |row| row.get(0))
                .expect("real integrity check");
            let schema: Vec<(String, String, Option<String>)> = connection
                .prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name")
                .expect("schema statement")
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .expect("canonical schema rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("complete schema");
            assert_eq!(version, CLIENT_STORE_SCHEMA_VERSION);
            assert_eq!(journal_mode, "wal");
            assert_eq!(integrity, "ok");

            store.close().expect("close real initialized store");
            serde_json::json!({
                "status":"opened", "pid":std::process::id(), "elapsedMs":elapsed_ms,
                "version":version, "journalMode":journal_mode, "integrity":integrity,
                "schema":schema
            })
        }
        Err(error) => {
            assert_eq!(
                role, "contender",
                "holder must complete real initialization"
            );
            assert_eq!(error.kind(), DeviceStoreErrorKind::Adapter);
            serde_json::json!({
                "status":"refused", "pid":std::process::id(), "elapsedMs":elapsed_ms,
                "error":error.message()
            })
        }
    };
    fs::write(
        gate.join(format!("{}.result.json", role.to_string_lossy())),
        serde_json::to_vec(&receipt).expect("receipt encoding"),
    )
    .expect("persist child result");
}

struct OpenProcess(Child);

impl OpenProcess {
    fn spawn(root: &Path, gate: &Path, role: &str) -> Self {
        let log = fs::File::create(gate.join(format!("{role}.child.log")))
            .expect("real subprocess output");
        Self(
            Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", CHILD_TEST, "--nocapture"])
                .env(ROOT_ENV, root)
                .env(GATE_ENV, gate)
                .env(ROLE_ENV, role)
                .stdin(Stdio::null())
                .stderr(Stdio::from(log.try_clone().expect("child stderr")))
                .stdout(Stdio::from(log))
                .spawn()
                .expect("start independent process"),
        )
    }

    fn finish(&mut self, gate: &Path, role: &str) -> serde_json::Value {
        let status = self.0.wait().expect("wait for bounded store open");
        assert!(
            status.success(),
            "child failed: {}",
            fs::read_to_string(gate.join(format!("{role}.child.log"))).expect("read child failure")
        );
        serde_json::from_slice(
            &fs::read(gate.join(format!("{role}.result.json"))).expect("real child receipt"),
        )
        .expect("child receipt JSON")
    }
}

impl Drop for OpenProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "wwc-device-process-open-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("store")).expect("fresh data directory");
        fs::create_dir_all(root.join("gate")).expect("test-only process gate");
        Self(root)
    }

    fn store(&self) -> PathBuf {
        self.0.join("store")
    }

    fn gate(&self) -> PathBuf {
        self.0.join("gate")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        path.exists(),
        "missing real process boundary {}",
        path.display()
    );
}

fn assert_real_contention(gate: &Path, holder: &mut OpenProcess, contender: &mut OpenProcess) {
    wait_for(
        &gate.join("contender.process-lock-busy"),
        Duration::from_secs(2),
    );
    assert!(holder.0.try_wait().expect("holder process state").is_none());
    assert!(
        contender
            .0
            .try_wait()
            .expect("contender process state")
            .is_none()
    );
    assert!(!gate.join("contender.result.json").exists());
    assert!(!gate.join("holder.result.json").exists());
}

#[test]
fn daemon_and_cli_initialization_share_one_canonical_database_process_lock() {
    let fixture = Fixture::new("canonical-alias");
    let store = fixture.store();
    let gate = fixture.gate();
    let alias = fixture.0.join("store-alias");
    std::os::unix::fs::symlink(&store, &alias).expect("canonical directory alias");
    let mut holder = OpenProcess::spawn(&store, &gate, "holder");
    wait_for(&gate.join("holder.sqlite-opened"), Duration::from_secs(2));
    let mut contender = OpenProcess::spawn(&alias, &gate, "contender");
    assert_real_contention(&gate, &mut holder, &mut contender);
    fs::write(gate.join("release"), b"release actual initializer").expect("release holder");
    let holder_result = holder.finish(&gate, "holder");
    let contender_result = contender.finish(&gate, "contender");
    assert_eq!(holder_result["status"], "opened");
    assert_eq!(contender_result["status"], "opened");
    assert_ne!(holder_result["pid"], contender_result["pid"]);
    assert_eq!(holder_result["schema"], contender_result["schema"]);
    DeviceStore::open(&alias)
        .expect("subsequent real reopen")
        .close()
        .expect("close reopen");
}

#[test]
fn contended_process_lock_keeps_the_original_five_second_open_deadline() {
    let fixture = Fixture::new("deadline");
    let store = fixture.store();
    let gate = fixture.gate();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.join("device-client.sqlite3.open.lock"))
        .expect("same canonical initialization lock");
    file.try_lock().expect("hold actual OS lock");
    let mut contender = OpenProcess::spawn(&store, &gate, "contender");
    wait_for(
        &gate.join("contender.process-lock-busy"),
        Duration::from_secs(2),
    );
    let result = contender.finish(&gate, "contender");
    assert_eq!(result["status"], "refused");
    assert!(
        result["error"]
            .as_str()
            .expect("deadline error")
            .contains("five-second limit")
    );
    let elapsed_ms = result["elapsedMs"]
        .as_u64()
        .expect("original open duration");
    assert!(
        (4_500..=6_500).contains(&elapsed_ms),
        "open duration: {elapsed_ms} ms"
    );
    assert!(
        !store.join("device-client.sqlite3").exists(),
        "timed-out open must not initialize SQLite"
    );
    drop(file);
    DeviceStore::open(&store)
        .expect("real open after OS lock release")
        .close()
        .expect("close reopen");
}

#[test]
fn process_termination_releases_the_initialization_lock_for_the_waiting_opener() {
    let fixture = Fixture::new("process-death");
    let store = fixture.store();
    let gate = fixture.gate();
    let mut holder = OpenProcess::spawn(&store, &gate, "holder");
    wait_for(&gate.join("holder.sqlite-opened"), Duration::from_secs(2));
    let mut contender = OpenProcess::spawn(&store, &gate, "contender");
    assert_real_contention(&gate, &mut holder, &mut contender);
    holder
        .0
        .kill()
        .expect("terminate actual initializer process");
    assert!(
        !holder
            .0
            .wait()
            .expect("observe actual process death")
            .success()
    );
    let result = contender.finish(&gate, "contender");
    assert_eq!(result["status"], "opened");
    assert_eq!(result["integrity"], "ok");
    assert_eq!(result["version"], CLIENT_STORE_SCHEMA_VERSION);
    DeviceStore::open(&store)
        .expect("real reopen after recovery")
        .close()
        .expect("close reopen");
}
