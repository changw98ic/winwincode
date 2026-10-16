// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use winwincode_control_plane::{
    ProductStateStorage, RemoteWorkerConnectionState, RemoteWorkerPoolAdapter,
    RemoteWorkerPoolErrorKind, RemoteWorkerPrincipal,
};
use winwincode_domain::{Instant, WorkerInstanceId};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, WorkerRegisterMessage, WorkerRegistrationResultMessageStatus,
};
use winwincode_storage::{
    ExecutionRegistry, SqliteStorage, WorkerHealth, WorkerPoolId, WorkerRegistryScope,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
fn root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "device-worker-pool-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn registration() -> WorkerRegisterMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "worker.register")
            .unwrap()
            .clone(),
    )
    .unwrap()
}
fn principal(message: &WorkerRegisterMessage, node: &str) -> RemoteWorkerPrincipal {
    RemoteWorkerPrincipal::new(
        message.worker_id.clone(),
        message.worker_instance_id.clone(),
        WorkerPoolId("wpl_00000000000000000000000001".into()),
        WorkerRegistryScope::local_default(),
        node.into(),
        "wsn_00000000000000000000000001".into(),
    )
    .unwrap()
}
fn register(
    adapter: &mut RemoteWorkerPoolAdapter<'_>,
    connection: &mut winwincode_control_plane::RemoteWorkerConnection,
    message: &WorkerRegisterMessage,
) -> WorkerRegistrationResultMessageStatus {
    let result = adapter
        .accept(
            connection,
            &ExecutionPortMessage::WorkerRegisterMessage(message.clone()),
            &message.sent_at,
        )
        .unwrap();
    let ExecutionPortMessage::WorkerRegistrationResultMessage(result) = result else {
        panic!("registration result")
    };
    result.status
}

#[test]
fn registration_replays_and_resume_requires_the_exact_device_and_process() {
    let root = root();
    let mut storage = SqliteStorage::open(&root).unwrap();
    let message = registration();
    let route = principal(&message, "cnd_00000000000000000000000001");
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage);
        let mut connection = adapter.connect(route.clone());
        assert_eq!(
            register(&mut adapter, &mut connection, &message),
            WorkerRegistrationResultMessageStatus::Accepted
        );
        assert_eq!(
            register(&mut adapter, &mut connection, &message),
            WorkerRegistrationResultMessageStatus::Duplicate
        );
        assert_eq!(connection.state(), RemoteWorkerConnectionState::Registered);
        let resumed = adapter
            .resume(
                route.clone(),
                &message.worker_id,
                &message.worker_instance_id,
            )
            .unwrap();
        assert_eq!(resumed.state(), RemoteWorkerConnectionState::Registered);
        assert!(
            adapter
                .resume(
                    route.clone(),
                    &message.worker_id,
                    &WorkerInstanceId("wki_00000000000000000000009999".into())
                )
                .is_err()
        );
        let foreign = principal(&message, "cnd_00000000000000000000000002");
        assert!(
            adapter
                .resume(foreign, &message.worker_id, &message.worker_instance_id)
                .is_err()
        );
        assert!(adapter.disconnect(&mut connection).unwrap());
        assert!(!adapter.disconnect(&mut connection).unwrap());
        let error = adapter
            .authorize_registered_message(
                &mut connection,
                &message.worker_id,
                &message.worker_instance_id,
                &message.sent_at,
            )
            .unwrap_err();
        assert_eq!(error.kind(), RemoteWorkerPoolErrorKind::InvalidConnection);
    }
    let worker = ExecutionRegistry::new(&mut storage)
        .unwrap()
        .load_worker(&message.worker_id)
        .unwrap()
        .unwrap();
    assert_eq!(worker.health, WorkerHealth::TimedOut);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn route_identity_rejects_a_foreign_register_frame() {
    let root = root();
    let mut storage = SqliteStorage::open(&root).unwrap();
    let mut message = registration();
    let route = principal(&message, "cnd_00000000000000000000000001");
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage);
        let mut connection = adapter.connect(route);
        message.worker_instance_id = WorkerInstanceId("wki_00000000000000000000009999".into());
        let now = Instant(message.sent_at.0.clone());
        let error = adapter
            .accept(
                &mut connection,
                &ExecutionPortMessage::WorkerRegisterMessage(message),
                &now,
            )
            .unwrap_err();
        assert_eq!(error.kind(), RemoteWorkerPoolErrorKind::InvalidConnection);
    }
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_same_device_can_replace_an_exited_process_with_a_new_session() {
    let root = root();
    let mut storage = SqliteStorage::open(&root).unwrap();
    let first = registration();
    let mut replacement = first.clone();
    replacement.worker_instance_id = WorkerInstanceId("wki_00000000000000000000000999".into());
    replacement.message_id =
        winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000999".into());
    replacement.request_id = winwincode_domain::RequestId("req_00000000000000000000000999".into());
    replacement.sent_at = Instant("2100-01-01T00:00:00.000Z".into());
    replacement.started_at = replacement.sent_at.clone();
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage);
        let mut previous = adapter.connect(principal(&first, "cnd_00000000000000000000000001"));
        assert_eq!(
            register(&mut adapter, &mut previous, &first),
            WorkerRegistrationResultMessageStatus::Accepted
        );
        assert!(adapter.disconnect(&mut previous).unwrap());
        let route = RemoteWorkerPrincipal::new(
            replacement.worker_id.clone(),
            replacement.worker_instance_id.clone(),
            WorkerPoolId("wpl_00000000000000000000000001".into()),
            WorkerRegistryScope::local_default(),
            "cnd_00000000000000000000000001".into(),
            "wsn_00000000000000000000000999".into(),
        )
        .unwrap();
        assert_eq!(route.worker_session_id(), "wsn_00000000000000000000000999");
        let mut current = adapter.connect(route);
        assert_eq!(
            register(&mut adapter, &mut current, &replacement),
            WorkerRegistrationResultMessageStatus::Accepted
        );
        assert!(!adapter.disconnect(&mut previous).unwrap());
    }
    let worker = ExecutionRegistry::new(&mut storage)
        .unwrap()
        .load_worker(&replacement.worker_id)
        .unwrap()
        .unwrap();
    assert_eq!(worker.worker_instance_id, replacement.worker_instance_id);
    assert_eq!(worker.health, WorkerHealth::Registered);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}
