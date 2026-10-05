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
