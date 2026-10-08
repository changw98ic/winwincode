// SPDX-License-Identifier: Apache-2.0

use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

const CHILD_DIRECTORY: &str = "WWC_PROVIDER_MODEL_SLOTS_TEST_DIRECTORY";
const CHILD_READY: &str = "PROVIDER_MODEL_SLOTS_READY";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "wwc-provider-slots-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn model_open(provider: &str, identity: usize) -> ModelOpenMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    open.model_exchange_id.0 = format!("mdl_{identity:026}");
    let payload = serde_json::to_vec(
        &serde_json::json!({"provider":provider,"request":{"model":"fixture-model"}}),
    )
    .unwrap();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.content_type = "application/json".into();
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    open
}

fn permit(store: &DeviceProviderStore, open: &ModelOpenMessage) -> DeviceModelPermit {
    match store.try_model_permit(open).unwrap() {
        DeviceModelAdmission::Ready(permit) => permit,
        DeviceModelAdmission::Deferred => panic!("expected an available Provider slot"),
    }
}

fn assert_full(store: &DeviceProviderStore, open: &ModelOpenMessage) {
    assert!(matches!(
        store.try_model_permit(open).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    assert!(
        !store.model_start_recorded(open).unwrap(),
        "waiting creates no first-start exchange record"
    );
}

#[test]
fn provider_slots_are_shared_across_processes_and_release_on_process_exit() {
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        let store = DeviceProviderStore::open(Path::new(&directory)).unwrap();
        let permits = (0..3)
            .map(|index| permit(&store, &model_open("provider-a", index)))
            .collect::<Vec<_>>();
        println!("{CHILD_READY}");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read_exact(&mut [0]);
        drop(permits);
        return;
    }
    let directory = TestDirectory::new("process");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let mut child = TestChild(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "device_model_concurrency::tests::provider_slots_are_shared_across_processes_and_release_on_process_exit", "--nocapture"])
        .env(CHILD_DIRECTORY, &directory.0)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
        .spawn().unwrap());
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "child exited before holding all three slots"
        );
        if line.contains(CHILD_READY) {
            break;
        }
    }
    let waiting = model_open("provider-a", 4);
    assert_full(&store, &waiting);
    let other = (0..3)
        .map(|index| permit(&store, &model_open("provider-b", index + 10)))
        .collect::<Vec<_>>();
    assert_full(&store, &model_open("provider-b", 14));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let _recovered = permit(&store, &waiting);
    let _recovered_siblings = (0..2)
        .map(|index| permit(&store, &model_open("provider-a", index + 20)))
        .collect::<Vec<_>>();
    assert_full(&store, &model_open("provider-a", 24));
    drop(other);
}

#[test]
fn shared_provider_slots_release_on_drop_without_a_device_or_model_global_limit() {
    let directory = TestDirectory::new("drop");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let reopened = DeviceProviderStore::open(&directory.0.join(".")).unwrap();
    let mut permits = (0..3)
        .map(|index| permit(&store, &model_open("provider-a", index)))
        .collect::<Vec<_>>();
    let waiting = model_open("provider-a", 4);
    assert_full(&reopened, &waiting);
    // Different Providers remain independent, even beyond the former per-Worker total of 16.
    let others = (0..20)
        .map(|index| {
            permit(
                &reopened,
                &model_open(&format!("provider-{index}"), index + 10),
            )
        })
        .collect::<Vec<_>>();
    drop(permits.pop());
    permits.push(permit(&reopened, &waiting));
    assert_full(&store, &model_open("provider-a", 5));
    assert_eq!(others.len(), 20);
}

#[test]
fn replay_cancellation_and_invalid_payload_do_not_wait_for_another_provider_slot() {
    let directory = TestDirectory::new("non-invocation");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let retained = model_open("provider-a", 10);
    let previous = store.execute_model(&retained).unwrap();
    let permits = (0..3)
        .map(|index| permit(&store, &model_open("provider-a", index)))
        .collect::<Vec<_>>();
    assert_full(&store, &model_open("provider-a", 4));
    let _replay_permit = permit(&store, &retained);
    assert_eq!(store.execute_model(&retained).unwrap(), previous);
    let cancelled = model_open("provider-a", 11);
    store.cancel_model(&cancelled.model_exchange_id.0).unwrap();
    let _cancelled_permit = permit(&store, &cancelled);
    assert!(store.execute_model(&cancelled).unwrap().is_empty());
    let mut invalid = model_open("provider-a", 12);
    invalid.request.data_base64 = "invalid-base64".into();
    let _invalid_permit = permit(&store, &invalid);
    assert!(store.execute_model(&invalid).unwrap()[0].error.is_some());
    drop(permits);
}

#[test]
fn an_actual_retry_requires_a_provider_slot_while_receipt_replay_does_not() {
    let directory = TestDirectory::new("retry-admission");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let retained = model_open("provider-a", 10);
    let previous = store.execute_model(&retained).unwrap();
    let mut held = (0..3)
        .map(|index| permit(&store, &model_open("provider-a", index)))
        .collect::<Vec<_>>();
    let _replay = permit(&store, &retained);
    assert_eq!(store.execute_model(&retained).unwrap(), previous);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(held.pop());
    let retry = match store.try_model_attempt_permit(&retained).unwrap() {
        DeviceModelAdmission::Ready(acquired) => acquired,
        DeviceModelAdmission::Deferred => panic!("the released invocation slot is available"),
    };
    assert!(matches!(
        store
            .try_model_attempt_permit(&model_open("provider-a", 11))
            .unwrap(),
        DeviceModelAdmission::Deferred
    ));
    let _independent = permit(&store, &model_open("provider-b", 12));
    drop(retry);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Ready(_)
    ));
}

#[test]
fn unsafe_provider_slot_paths_are_hard_errors_and_never_report_a_full_queue() {
    let directory = TestDirectory::new("permissions");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let open = model_open("provider-a", 0);
    drop(permit(&store, &open));
    let slots = directory
        .0
        .join("model-provider-slots")
        .join(format!("{:x}", Sha256::digest(b"provider-a")));
    let slot = slots.join("slot-0");
    fs::set_permissions(&slot, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.try_model_permit(&open).is_err());
    fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&slot).unwrap();
    std::os::unix::fs::symlink(directory.0.join("providers.sqlite3"), &slot).unwrap();
    assert!(store.try_model_permit(&open).is_err());
}

#[test]
fn invocation_slot_drop_releases_a_lock_with_a_duplicated_descriptor() {
    let directory = TestDirectory::new("drop-with-duplicate");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let retained = model_open("provider-a", 10);
    let previous = store.execute_model(&retained).unwrap();
    let mut held = (0..3)
        .map(|index| permit(&store, &model_open("provider-a", index)))
        .collect::<Vec<_>>();
    let duplicate = held
        .last()
        .unwrap()
        ._slot
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    let replay = permit(&store, &retained);
    assert!(replay._slot.is_none());
    assert_eq!(store.execute_model(&retained).unwrap(), previous);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(held.pop());
    let retry = match store.try_model_attempt_permit(&retained).unwrap() {
        DeviceModelAdmission::Ready(acquired) => acquired,
        DeviceModelAdmission::Deferred => {
            panic!("the invocation owner returns its slot while an alias remains open")
        }
    };
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(duplicate);
    assert!(
        matches!(
            store.try_model_attempt_permit(&retained).unwrap(),
            DeviceModelAdmission::Deferred
        ),
        "closing the old alias cannot release the new invocation's lock"
    );
    drop(retry);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Ready(_)
    ));
}

const SLOT_ALIAS_CHILD_RELEASE: &str = "WWC_PROVIDER_SLOT_ALIAS_CHILD_RELEASE";
const SLOT_ALIAS_CHILD_READY: &str = "PROVIDER_SLOT_ALIAS_CHILD_READY";

struct SlotAliasChild {
    child: Child,
    release: PathBuf,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SlotAliasChild {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"release");
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn provider_slot_inherited_descriptor_child() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let Some(release) = std::env::var_os(SLOT_ALIAS_CHILD_RELEASE) else {
        return;
    };
    println!("{SLOT_ALIAS_CHILD_READY}");
    std::io::stdout().flush().unwrap();
    while !Path::new(&release).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "parent must release the descriptor child"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn invocation_slot_drop_releases_a_lock_still_inherited_by_a_live_child() {
    let directory = TestDirectory::new("drop-with-child-alias");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let retained = model_open("provider-a", 10);
    let previous = store.execute_model(&retained).unwrap();
    let mut held = (0..3)
        .map(|index| permit(&store, &model_open("provider-a", index)))
        .collect::<Vec<_>>();
    let inherited = held
        .last()
        .unwrap()
        ._slot
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    let release = directory.0.join("release-child");
    // Explicit stdin inheritance pins the same open file description after exec.
    // This deterministically models the transient inherited descriptor before exec.
    let mut child = SlotAliasChild {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device_model_concurrency::tests::provider_slot_inherited_descriptor_child",
                "--nocapture",
            ])
            .env(SLOT_ALIAS_CHILD_RELEASE, &release)
            .stdin(Stdio::from(inherited))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        release,
        reader: None,
    };
    let mut output = BufReader::new(child.child.stdout.take().unwrap());
    let (ready, receiver) = std::sync::mpsc::sync_channel(1);
    child.reader = Some(std::thread::spawn(move || {
        let result = (|| -> std::io::Result<()> {
            loop {
                let mut line = String::new();
                if output.read_line(&mut line)? == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "child exited before holding the inherited descriptor",
                    ));
                }
                if line.contains(SLOT_ALIAS_CHILD_READY) {
                    return Ok(());
                }
            }
        })();
        let _ = ready.send(result);
        // libtest still writes its result after the helper's release.
        let _ = std::io::copy(&mut output, &mut std::io::sink());
    }));
    receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("descriptor child must become ready before the handshake deadline")
        .expect("descriptor child must hold the inherited descriptor");
    let replay = permit(&store, &retained);
    assert!(replay._slot.is_none());
    assert_eq!(store.execute_model(&retained).unwrap(), previous);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(held.pop());
    let admission = store.try_model_attempt_permit(&retained).unwrap();
    assert!(
        child.child.try_wait().unwrap().is_none(),
        "the child must still retain stdin at the release boundary"
    );
    // Always release and reap naturally before asserting the red-capable verdict.
    fs::write(&child.release, b"release").unwrap();
    assert!(child.child.wait().unwrap().success());
    child.reader.take().unwrap().join().unwrap();
    let DeviceModelAdmission::Ready(retry) = admission else {
        panic!(
            "the invocation owner returns its slot before the inherited child descriptor closes"
        );
    };
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(retry);
    assert!(matches!(
        store.try_model_attempt_permit(&retained).unwrap(),
        DeviceModelAdmission::Ready(_)
    ));
}
