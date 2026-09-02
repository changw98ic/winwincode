// SPDX-License-Identifier: Apache-2.0

//! Remote Worker authentication and canonical registry integration.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, from_value};
use winwincode_control_plane::{
    ProductStateStorage, RemoteWorkerAuthenticationError, RemoteWorkerAuthenticator,
    RemoteWorkerConnectionState, RemoteWorkerCredential, RemoteWorkerPoolAdapter,
    RemoteWorkerPoolErrorKind, RemoteWorkerPrincipal,
};
use winwincode_domain::{Instant, Sha256Digest, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, WorkerHeartbeatMessage, WorkerRegisterMessage,
    WorkerRegistrationResultMessage, WorkerRegistrationResultMessageStatus,
};
use winwincode_storage::{
    SqliteStorage, WorkerAuthenticationIdentity, WorkerHealth, WorkerPoolId, WorkerRegistryScope,
};

const SECRET_PROOF: &[u8] = b"REMOTE_SECRET_DO_NOT_PERSIST";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct FixedAuthenticator {
    principal: RemoteWorkerPrincipal,
    revoked: AtomicBool,
}

impl FixedAuthenticator {
    fn new(principal: RemoteWorkerPrincipal) -> Self {
        Self {
            principal,
            revoked: AtomicBool::new(false),
        }
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }
}

impl RemoteWorkerAuthenticator for FixedAuthenticator {
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        _now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        if credential.expose_for_verification() != SECRET_PROOF {
            return Err(RemoteWorkerAuthenticationError::rejected());
        }
        Ok(self.principal.clone())
    }

    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        _now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        if principal != &self.principal {
            return Err(RemoteWorkerAuthenticationError::rejected());
        }
        Ok(())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-remote-worker-pool-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn fixture_message<T: serde::de::DeserializeOwned>(kind: &str) -> T {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical Execution Port fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map(from_value)
        .expect("fixture kind")
        .unwrap_or_else(|error| panic!("{kind} fixture must decode: {error}"))
}

fn principal() -> RemoteWorkerPrincipal {
    principal_with_fingerprint('1')
}

fn principal_with_fingerprint(digit: char) -> RemoteWorkerPrincipal {
    RemoteWorkerPrincipal::new(
        WorkerId("wrk_00000000000000000000000001".to_owned()),
        WorkerPoolId("wpl_00000000000000000000000001".to_owned()),
        WorkerRegistryScope::local_default(),
        "enterprise-worker-identity".to_owned(),
        "remote-worker-01".to_owned(),
        Sha256Digest(format!("sha256:{}", digit.to_string().repeat(64))),
        "enterprise-default".to_owned(),
    )
    .expect("valid remote principal")
}

fn credential(bytes: &[u8]) -> RemoteWorkerCredential {
    RemoteWorkerCredential::new(bytes.to_vec()).expect("bounded credential")
}

fn registration_result(message: ExecutionPortMessage) -> WorkerRegistrationResultMessage {
    let ExecutionPortMessage::WorkerRegistrationResultMessage(result) = message else {
        panic!("remote registration response")
    };
    result
}

fn directory_contains(root: &Path, needle: &[u8]) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                directory_contains(&path, needle)
            } else {
                fs::read(path)
                    .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
            }
        })
    })
}

fn register(
    adapter: &mut RemoteWorkerPoolAdapter<'_, '_>,
    connection: &mut winwincode_control_plane::RemoteWorkerConnection,
    message: &WorkerRegisterMessage,
) -> WorkerRegistrationResultMessage {
    registration_result(
        adapter
            .accept(
                connection,
                &ExecutionPortMessage::WorkerRegisterMessage(message.clone()),
                &message.sent_at,
            )
            .expect("remote registration"),
    )
}

#[test]
fn forged_proof_and_worker_identity_are_rejected_without_secret_persistence() {
    let root = temporary_directory("forged");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let authenticator = FixedAuthenticator::new(principal());
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let Err(error) = adapter.connect(
            &credential(b"FORGED_REMOTE_SECRET"),
            &Instant("2026-08-24T12:00:00.000Z".to_owned()),
        ) else {
            panic!("forged proof must be rejected")
        };
        assert_eq!(
            error.kind(),
            RemoteWorkerPoolErrorKind::AuthenticationRejected
        );
        assert!(!error.to_string().contains("FORGED_REMOTE_SECRET"));

        let mut connection = adapter
            .connect(
                &credential(SECRET_PROOF),
                &Instant("2026-08-24T12:00:00.000Z".to_owned()),
            )
            .expect("authenticated connection");
        let mut spoofed: WorkerRegisterMessage = fixture_message("worker.register");
        spoofed.worker_id = WorkerId("wrk_00000000000000000000000009".to_owned());
        let error = adapter
            .accept(
                &mut connection,
                &ExecutionPortMessage::WorkerRegisterMessage(spoofed.clone()),
                &spoofed.sent_at,
            )
            .expect_err("foreign Worker identity rejected");
        assert_eq!(error.kind(), RemoteWorkerPoolErrorKind::InvalidConnection);
    }
    Box::new(storage).close().expect("storage close");
    assert!(!directory_contains(&root, SECRET_PROOF));
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn repeat_registration_is_idempotent_and_disconnect_updates_exact_health() {
    let root = temporary_directory("registration");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let authenticator = FixedAuthenticator::new(principal());
    let message: WorkerRegisterMessage = fixture_message("worker.register");
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let mut connection = adapter
            .connect(&credential(SECRET_PROOF), &message.sent_at)
            .expect("authenticated connection");
        let accepted = register(&mut adapter, &mut connection, &message);
        let replay = register(&mut adapter, &mut connection, &message);
        assert_eq!(
            accepted.status,
            WorkerRegistrationResultMessageStatus::Accepted
        );
        assert_eq!(
            replay.status,
            WorkerRegistrationResultMessageStatus::Duplicate
        );
        assert_eq!(connection.state(), RemoteWorkerConnectionState::Registered);
        assert_eq!(
            connection.principal().worker_pool_id(),
            &WorkerPoolId("wpl_00000000000000000000000001".to_owned())
        );
        assert!(adapter.disconnect(&mut connection).expect("disconnect"));
        assert!(
            !adapter
                .disconnect(&mut connection)
                .expect("disconnect replay")
        );
    }
    let worker = storage
        .execution_registry()
        .expect("registry")
        .load_worker(&message.worker_id)
        .expect("worker read")
        .expect("worker record");
    assert_eq!(worker.health, WorkerHealth::TimedOut);
    assert_eq!(
        worker.management_scope,
        WorkerRegistryScope::local_default()
    );
    assert_eq!(worker.security_zone, "enterprise-default");
    assert!(matches!(
        worker.authentication_identity,
        WorkerAuthenticationIdentity::TransportPrincipal { .. }
    ));
    let placement = storage
        .execution_registry()
        .expect("registry")
        .load_authenticated_worker_placement(&message.worker_id, &message.worker_instance_id)
        .expect("placement read")
        .expect("authenticated placement");
    assert_eq!(
        placement.worker_pool_id,
        WorkerPoolId("wpl_00000000000000000000000001".to_owned())
    );
    assert_eq!(placement.registration_request_id, message.request_id);
    Box::new(storage).close().expect("storage close");
    assert!(!directory_contains(&root, SECRET_PROOF));
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn authenticated_request_resumes_only_the_exact_durable_process_binding() {
    let root = temporary_directory("resume");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let authenticator = FixedAuthenticator::new(principal());
    let message: WorkerRegisterMessage = fixture_message("worker.register");
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let mut initial = adapter
            .connect(&credential(SECRET_PROOF), &message.sent_at)
            .expect("authenticated connection");
        register(&mut adapter, &mut initial, &message);
    }
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let mut resumed = adapter
            .resume(
                &credential(SECRET_PROOF),
                &message.worker_id,
                &message.worker_instance_id,
                &message.sent_at,
            )
            .expect("exact process resume");
        assert_eq!(resumed.state(), RemoteWorkerConnectionState::Registered);
        adapter
            .authorize_registered_message(
                &mut resumed,
                &message.worker_id,
                &message.worker_instance_id,
                &message.sent_at,
            )
            .expect("active exact binding");

        let foreign_instance = WorkerInstanceId("wki_00000000000000000000000009".to_owned());
        let Err(error) = adapter.resume(
            &credential(SECRET_PROOF),
            &message.worker_id,
            &foreign_instance,
            &message.sent_at,
        ) else {
            panic!("foreign process must not resume")
        };
        assert_eq!(error.kind(), RemoteWorkerPoolErrorKind::InvalidConnection);

        authenticator.revoke();
        let error = adapter
            .authorize_registered_message(
                &mut resumed,
                &message.worker_id,
                &message.worker_instance_id,
                &message.sent_at,
            )
            .expect_err("revoked resumed request");
        assert_eq!(
            error.kind(),
            RemoteWorkerPoolErrorKind::AuthenticationRevoked
        );
    }
    Box::new(storage).close().expect("storage close");
    assert!(!directory_contains(&root, SECRET_PROOF));
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn authenticated_foreign_principal_cannot_take_over_an_existing_worker_identity() {
    let root = temporary_directory("foreign-principal");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let message: WorkerRegisterMessage = fixture_message("worker.register");
    let initial_authenticator = FixedAuthenticator::new(principal());
    {
        let mut initial_adapter =
            RemoteWorkerPoolAdapter::new(&mut storage, &initial_authenticator);
        let mut initial_connection = initial_adapter
            .connect(&credential(SECRET_PROOF), &message.sent_at)
            .expect("initial connection");
        register(&mut initial_adapter, &mut initial_connection, &message);
    }

    let foreign_authenticator = FixedAuthenticator::new(principal_with_fingerprint('2'));
    {
        let mut foreign_adapter =
            RemoteWorkerPoolAdapter::new(&mut storage, &foreign_authenticator);
        let mut foreign_connection = foreign_adapter
            .connect(&credential(SECRET_PROOF), &message.sent_at)
            .expect("foreign authenticated connection");
        let mut foreign_message = message.clone();
        foreign_message.message_id =
            winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000008".to_owned());
        foreign_message.request_id =
            winwincode_domain::RequestId("req_00000000000000000000000008".to_owned());
        let rejection = register(
            &mut foreign_adapter,
            &mut foreign_connection,
            &foreign_message,
        );
        assert_eq!(
            rejection.status,
            WorkerRegistrationResultMessageStatus::Rejected
        );
        assert_eq!(
            foreign_connection.state(),
            RemoteWorkerConnectionState::Authenticated
        );
    }
    let worker = storage
        .execution_registry()
        .expect("registry")
        .load_worker(&message.worker_id)
        .expect("worker read")
        .expect("worker record");
    assert!(matches!(
        worker.authentication_identity,
        WorkerAuthenticationIdentity::TransportPrincipal {
            credential_fingerprint: Sha256Digest(ref digest),
            ..
        } if digest == &format!("sha256:{}", "1".repeat(64))
    ));
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn revocation_terminates_connection_and_marks_registered_process_offline() {
    let root = temporary_directory("revoked");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let authenticator = FixedAuthenticator::new(principal());
    let register_message: WorkerRegisterMessage = fixture_message("worker.register");
    {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let mut connection = adapter
            .connect(&credential(SECRET_PROOF), &register_message.sent_at)
            .expect("authenticated connection");
        register(&mut adapter, &mut connection, &register_message);

        let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
        heartbeat.active_leases.clear();
        authenticator.revoke();
        let error = adapter
            .accept(
                &mut connection,
                &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat.clone()),
                &heartbeat.observed_at,
            )
            .expect_err("revoked connection rejected");
        assert_eq!(
            error.kind(),
            RemoteWorkerPoolErrorKind::AuthenticationRevoked
        );
        assert_eq!(
            connection.state(),
            RemoteWorkerConnectionState::Disconnected
        );
        let replay_error = adapter
            .accept(
                &mut connection,
                &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
                &Instant("2026-08-24T12:00:01.000Z".to_owned()),
            )
            .expect_err("closed connection rejected");
        assert_eq!(
            replay_error.kind(),
            RemoteWorkerPoolErrorKind::InvalidConnection
        );
    }
    let worker = storage
        .execution_registry()
        .expect("registry")
        .load_worker(&register_message.worker_id)
        .expect("worker read")
        .expect("worker record");
    assert_eq!(worker.health, WorkerHealth::TimedOut);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn stale_connection_cannot_disconnect_a_replacement_process() {
    let root = temporary_directory("stale-disconnect");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let authenticator = FixedAuthenticator::new(principal());
    let first_message: WorkerRegisterMessage = fixture_message("worker.register");
    let replacement_message = {
        let mut adapter = RemoteWorkerPoolAdapter::new(&mut storage, &authenticator);
        let mut first = adapter
            .connect(&credential(SECRET_PROOF), &first_message.sent_at)
            .expect("first connection");
        register(&mut adapter, &mut first, &first_message);

        let mut replacement_message = first_message.clone();
        replacement_message.worker_instance_id =
            WorkerInstanceId("wki_00000000000000000000000003".to_owned());
        replacement_message.message_id =
            winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000003".to_owned());
        replacement_message.request_id =
            winwincode_domain::RequestId("req_00000000000000000000000003".to_owned());
        replacement_message.sent_at = Instant("2026-08-24T12:01:00.000Z".to_owned());
        replacement_message.started_at = replacement_message.sent_at.clone();
        let mut replacement = adapter
            .connect(&credential(SECRET_PROOF), &replacement_message.sent_at)
            .expect("replacement connection");
        let accepted = register(&mut adapter, &mut replacement, &replacement_message);
        assert_eq!(
            accepted.status,
            WorkerRegistrationResultMessageStatus::Accepted
        );

        assert!(!adapter.disconnect(&mut first).expect("stale disconnect"));
        replacement_message
    };
    let worker = storage
        .execution_registry()
        .expect("registry")
        .load_worker(&replacement_message.worker_id)
        .expect("worker read")
        .expect("worker record");
    assert_eq!(
        worker.worker_instance_id,
        replacement_message.worker_instance_id
    );
    assert_eq!(worker.health, WorkerHealth::Registered);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}
