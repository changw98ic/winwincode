// SPDX-License-Identifier: Apache-2.0

//! Production handling of Server repository commands.
//!
//! `client.repository.rescan` resolves only the durable local binding id,
//! re-runs the repository registry's canonicalization and Git checks, and
//! emits both the path-free `client.repository.status` response and the
//! universal `client.command_ack`. Absolute paths and local failure detail
//! remain in the device store and never enter either frame.

use time::OffsetDateTime;
use winwincode_client_port::domain::{
    ClientControlError, ClientControlErrorCode, ClientControlMessageKind, CommandAckStatus,
};
use winwincode_client_port::messages::{
    ClientCommandAckPayload, ClientToServerMessage, ServerRepositoryRescanPayload,
};

use crate::repository::{
    RepositoryRegistryError, RepositoryRevalidation, enqueue_repository_frame,
    revalidate_repository,
};
use crate::store::DeviceStore;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registration {
    path: String,
    confirm_git_init: bool,
}

/// Register a browser-selected path; retain the result before acknowledging the command.
pub(crate) fn apply_repository_registration(
    store: &mut DeviceStore,
    provider_directory: &std::path::Path,
    node: &str,
    instance: &str,
    payload: &winwincode_client_port::messages::ServerConfigurationApplyPayload,
) -> Result<winwincode_api::generated::DeviceRepositoryRegistrationReceipt, crate::DaemonError> {
    use sha2::{Digest, Sha256};
    use std::{fs, path::Path};
    use winwincode_api::generated::DeviceRepositoryRegistrationReceipt;
    let failure =
        || crate::DaemonError::Protocol("device repository registration failed".to_owned());
    let encrypted = &payload.encrypted;
    if encrypted.client_node_id != node
        || encrypted.request_id != payload.command.idempotency_key
        || i64::try_from(payload.command.expected_revision).ok()
            != Some(encrypted.expected_revision)
        || encrypted.request_id.is_empty()
        || encrypted.request_id.len() > 200
        || !encrypted
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(failure());
    }
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(encrypted).map_err(|_| failure())?)
    );
    let directory = store
        .database_path()
        .parent()
        .ok_or_else(failure)?
        .join("repository-receipts");
    fs::create_dir_all(&directory).map_err(|_| failure())?;
    let receipt_path = directory.join(format!("{}.json", encrypted.request_id));
    match fs::read(&receipt_path) {
        Ok(bytes) => {
            let (previous, receipt): (String, DeviceRepositoryRegistrationReceipt) =
                serde_json::from_slice(&bytes).map_err(|_| failure())?;
            return if previous == digest {
                Ok(receipt)
            } else {
                Err(failure())
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(failure()),
    }
    let provider = winwincode_provider::DeviceProviderStore::open(provider_directory)
        .map_err(|_| failure())?;
    let mutation = provider
        .decrypt_configuration::<Registration>(encrypted, "winwincode.device-repository.v1");
    let mut receipt = DeviceRepositoryRegistrationReceipt {
        request_id: encrypted.request_id.clone(),
        outcome: "invalid_request".to_owned(),
        repository_binding_id: None,
    };
    if let Ok(mutation) = mutation
        && !mutation.path.is_empty()
        && mutation.path.len() <= 4096
        && Path::new(&mutation.path).is_absolute()
    {
        let binding = match crate::repository::register_repository(
            store,
            node,
            instance,
            Path::new(&mutation.path),
            &crate::repository::RegistrationOptions {
                confirm_git_init: mutation.confirm_git_init,
            },
            OffsetDateTime::now_utc(),
        ) {
            Ok(result) => Some(result.repository_binding_id),
            Err(RepositoryRegistryError::AlreadyRegistered {
                repository_binding_id,
            }) => Some(repository_binding_id),
            Err(RepositoryRegistryError::Rejected(reason)) => {
                crate::availability_wire_name(reason.availability).clone_into(&mut receipt.outcome);
                None
            }
            Err(_) => return Err(failure()),
        };
        if let Some(binding) = binding {
            "registered".clone_into(&mut receipt.outcome);
            receipt.repository_binding_id = Some(
                serde_json::from_value(serde_json::Value::String(binding))
                    .map_err(|_| failure())?,
            );
        }
    }
    let temporary = receipt_path.with_extension("tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(&(digest, &receipt)).map_err(|_| failure())?,
    )
    .map_err(|_| failure())?;
    fs::rename(temporary, receipt_path).map_err(|_| failure())?;
    Ok(receipt)
}

/// Result of one consumed repository rescan command.
#[derive(Debug)]
pub struct RepositoryRescanApplication {
    /// Fresh scan facts when the binding existed. Local-only detail may
    /// contain a path and must not be serialized onto the control port.
    pub revalidation: Option<RepositoryRevalidation>,
    /// Durable outbox sequence of the command acknowledgement.
    pub command_ack_outbox_sequence: u64,
    /// Whether the request was accepted or rejected as an unknown binding.
    pub status: CommandAckStatus,
}

/// Applies one `client.repository.rescan` command against the device-local
/// binding registry and durably enqueues its responses.
///
/// A missing binding is a handled command: it emits a stable rejection and
/// leaves the daemon running. Storage and protocol failures still fail the
/// exchange loop so they are observable rather than silently acknowledged.
///
/// # Errors
///
/// Returns a repository registry failure when the local store, Git scan, or
/// durable outbox cannot complete the command.
pub fn apply_repository_rescan(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    command_message_id: &str,
    payload: &ServerRepositoryRescanPayload,
    now: OffsetDateTime,
) -> Result<RepositoryRescanApplication, RepositoryRegistryError> {
    match revalidate_repository(
        store,
        client_node_id,
        client_instance_id,
        &payload.repository_binding_id,
        now,
    ) {
        Ok(mut revalidation) => {
            if !revalidation.status_reported {
                let sequence = enqueue_repository_frame(
                    store,
                    client_node_id,
                    client_instance_id,
                    ClientToServerMessage::RepositoryStatus(
                        winwincode_client_port::messages::ClientRepositoryStatusPayload {
                            repository_binding_id: revalidation.repository_binding_id.clone(),
                            availability: revalidation.availability,
                            head_commit: revalidation.head_commit.clone(),
                            dirty_state: revalidation.dirty_state,
                            last_scanned_at: revalidation.last_scanned_at.clone(),
                        },
                    ),
                    "client.repository.status",
                    now,
                )?;
                revalidation.status_reported = true;
                revalidation.status_outbox_sequence = Some(sequence);
            }
            let command_ack_outbox_sequence = enqueue_ack(
                store,
                client_node_id,
                client_instance_id,
                command_message_id,
                CommandAckStatus::Accepted,
                None,
                now,
            )?;
            Ok(RepositoryRescanApplication {
                revalidation: Some(revalidation),
                command_ack_outbox_sequence,
                status: CommandAckStatus::Accepted,
            })
        }
        Err(RepositoryRegistryError::NotFound) => {
            let command_ack_outbox_sequence = enqueue_ack(
                store,
                client_node_id,
                client_instance_id,
                command_message_id,
                CommandAckStatus::RejectedWrongState,
                Some(ClientControlError {
                    code: ClientControlErrorCode::RepositoryUnavailable,
                    message: "the repository binding is unknown locally".to_owned(),
                    retryable: false,
                }),
                now,
            )?;
            Ok(RepositoryRescanApplication {
                revalidation: None,
                command_ack_outbox_sequence,
                status: CommandAckStatus::RejectedWrongState,
            })
        }
        Err(error) => Err(error),
    }
}

fn enqueue_ack(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    command_message_id: &str,
    status: CommandAckStatus,
    error: Option<ClientControlError>,
    now: OffsetDateTime,
) -> Result<u64, RepositoryRegistryError> {
    enqueue_repository_frame(
        store,
        client_node_id,
        client_instance_id,
        ClientToServerMessage::CommandAck(ClientCommandAckPayload {
            command_kind: ClientControlMessageKind::RepositoryRescan,
            command_message_id: command_message_id.to_owned(),
            status,
            current_revision: None,
            error,
        }),
        "client.command_ack",
        now,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use winwincode_client_port::domain::ClientRepositoryRescanReason;
    use winwincode_client_port::messages::{
        ClientToServerEnvelope, CommandContext, ServerRepositoryRescanPayload,
    };

    use super::*;
    use crate::repository::{RegistrationOptions, list_bindings, register_repository};

    const NODE_ID: &str = "cnd_AAAAAAAAAAAAAAAAAAAAAAAAAA";
    const INSTANCE_ID: &str = "cix_AAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn git(root: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(["-C"])
            .arg(root)
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture(name: &str) -> (std::path::PathBuf, DeviceStore, String) {
        let root = std::env::temp_dir().join(format!(
            "winwincode-repository-exchange-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let repository = root.join("repository");
        fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init"]);
        git(
            &repository,
            &["config", "user.email", "exchange@example.test"],
        );
        git(&repository, &["config", "user.name", "Exchange Tests"]);
        git(&repository, &["config", "commit.gpgsign", "false"]);
        git(&repository, &["commit", "--allow-empty", "-m", "baseline"]);

        let mut store = DeviceStore::open(root.join("device")).expect("device store");
        store
            .bind_outbox_stream(NODE_ID, INSTANCE_ID)
            .expect("bind outbox");
        let registration = register_repository(
            &mut store,
            NODE_ID,
            INSTANCE_ID,
            &repository,
            &RegistrationOptions {
                confirm_git_init: false,
            },
            OffsetDateTime::now_utc(),
        )
        .expect("register repository");
        (root, store, registration.repository_binding_id)
    }

    fn rescan(binding_id: &str) -> ServerRepositoryRescanPayload {
        ServerRepositoryRescanPayload {
            command: CommandContext {
                expected_revision: 1,
                idempotency_key: "repository-rescan-test".to_owned(),
            },
            repository_binding_id: binding_id.to_owned(),
            reason: ClientRepositoryRescanReason::Policy,
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "One encrypted registration scenario verifies rejection, retry and durable identity together"
    )]
    fn encrypted_registration_checks_paths_and_replays_the_same_result() {
        use std::io::Write;
        use std::process::Stdio;
        let (root, mut store, _) = fixture("web-register");
        let provider_directory = root.join("provider");
        let provider =
            winwincode_provider::DeviceProviderStore::open(&provider_directory).expect("provider");
        let snapshot = provider.snapshot(NODE_ID).expect("public key");
        let encrypt = |id: &str, path: &Path| {
            let mut child = Command::new("node").args(["--input-type=module", "-e", r"
                import { readFileSync } from 'node:fs';
                import { pathToFileURL } from 'node:url';
                const { encryptDeviceRepository } = await import(pathToFileURL(process.argv[1]));
                const {snapshot, id, path} = JSON.parse(readFileSync(0, 'utf8'));
                process.stdout.write(JSON.stringify(await encryptDeviceRepository(snapshot, id, {path, confirmGitInit:false})));
            "]).arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/client/src/device-provider-encryption.ts"))
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("WebCrypto");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(
                    &serde_json::to_vec(
                        &serde_json::json!({"snapshot":snapshot,"id":id,"path":path}),
                    )
                    .expect("input"),
                )
                .expect("write");
            let output = child.wait_with_output().expect("encrypt");
            assert!(output.status.success());
            let encrypted = serde_json::from_slice(&output.stdout).expect("envelope");
            winwincode_client_port::messages::ServerConfigurationApplyPayload {
                command: CommandContext {
                    expected_revision: 0,
                    idempotency_key: id.to_owned(),
                },
                encrypted,
            }
        };
        let path = root.join("missing");
        let missing = encrypt("repository_missing", &path);
        let rejected = apply_repository_registration(
            &mut store,
            &provider_directory,
            NODE_ID,
            INSTANCE_ID,
            &missing,
        )
        .expect("receipt");
        assert_ne!(rejected.outcome, "registered");
        fs::create_dir_all(&path).expect("directory");
        assert_eq!(
            apply_repository_registration(
                &mut store,
                &provider_directory,
                NODE_ID,
                INSTANCE_ID,
                &missing
            )
            .expect("replay"),
            rejected
        );
        let not_git = encrypt("repository_not_git", &path);
        assert_eq!(
            apply_repository_registration(
                &mut store,
                &provider_directory,
                NODE_ID,
                INSTANCE_ID,
                &not_git
            )
            .expect("non-Git")
            .outcome,
            "invalid_git"
        );
        git(&path, &["init"]);
        git(
            &path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let registered = encrypt("repository_registered", &path);
        let receipt = apply_repository_registration(
            &mut store,
            &provider_directory,
            NODE_ID,
            INSTANCE_ID,
            &registered,
        )
        .expect("register");
        assert_eq!(receipt.outcome, "registered");
        assert!(receipt.repository_binding_id.is_some());
        assert_eq!(list_bindings(&store).expect("bindings").len(), 2);
        assert_eq!(
            apply_repository_registration(
                &mut store,
                &provider_directory,
                NODE_ID,
                INSTANCE_ID,
                &registered
            )
            .expect("replay"),
            receipt
        );
        let mut changed = registered.clone();
        changed.encrypted.ciphertext.push('A');
        assert!(
            apply_repository_registration(
                &mut store,
                &provider_directory,
                NODE_ID,
                INSTANCE_ID,
                &changed
            )
            .is_err()
        );
        assert!(
            !serde_json::to_string(&receipt)
                .expect("receipt JSON")
                .contains(&path.to_string_lossy().to_string())
        );
        drop(store);
        drop(provider);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn unchanged_rescan_always_emits_path_free_status_and_ack() {
        let (root, mut store, binding_id) = fixture("accepted");
        let applied = apply_repository_rescan(
            &mut store,
            NODE_ID,
            INSTANCE_ID,
            "msg_repository_rescan",
            &rescan(&binding_id),
            OffsetDateTime::now_utc(),
        )
        .expect("apply rescan");
        let revalidation = applied.revalidation.expect("accepted scan");
        assert_eq!(revalidation.repository_binding_id, binding_id);
        assert!(revalidation.status_reported);
        assert_eq!(applied.status, CommandAckStatus::Accepted);

        let frames = store.pending_outbox_envelopes().expect("pending frames");
        assert_eq!(frames.len(), 3, "upsert, status, and command ack");
        assert_eq!(frames[1].kind, "client.repository.status");
        assert_eq!(frames[2].kind, "client.command_ack");
        let status: ClientToServerEnvelope =
            serde_json::from_slice(&frames[1].payload).expect("status frame");
        let ack: ClientToServerEnvelope =
            serde_json::from_slice(&frames[2].payload).expect("ack frame");
        let serialized = serde_json::to_string(&(status, ack)).expect("serialize responses");
        let canonical_path = fs::canonicalize(root.join("repository"))
            .expect("canonical path")
            .to_string_lossy()
            .into_owned();
        assert!(!serialized.contains(&canonical_path));

        store.close().expect("close store");
        let reopened = DeviceStore::open(root.join("device")).expect("reopen store");
        let bindings = list_bindings(&reopened).expect("list bindings after restart");
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].mapping.repository_binding_id, binding_id,
            "binding identity remains stable across restart"
        );
        reopened.close().expect("close reopened store");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn missing_binding_emits_one_non_retryable_rejection() {
        let (root, mut store, _) = fixture("missing");
        let applied = apply_repository_rescan(
            &mut store,
            NODE_ID,
            INSTANCE_ID,
            "msg_repository_rescan_missing",
            &rescan("rbd_BBBBBBBBBBBBBBBBBBBBBBBBBB"),
            OffsetDateTime::now_utc(),
        )
        .expect("handle missing binding");
        assert!(applied.revalidation.is_none());
        assert_eq!(applied.status, CommandAckStatus::RejectedWrongState);
        let frames = store.pending_outbox_envelopes().expect("pending frames");
        assert_eq!(frames.len(), 2, "upsert plus rejection ack");
        let ack: ClientToServerEnvelope =
            serde_json::from_slice(&frames[1].payload).expect("ack frame");
        let ClientToServerMessage::CommandAck(payload) = ack.message else {
            panic!("expected command ack");
        };
        assert_eq!(
            payload.command_kind,
            ClientControlMessageKind::RepositoryRescan
        );
        assert_eq!(payload.status, CommandAckStatus::RejectedWrongState);
        let error = payload.error.expect("rejection error");
        assert_eq!(error.code, ClientControlErrorCode::RepositoryUnavailable);
        assert!(!error.retryable);

        store.close().expect("close store");
        fs::remove_dir_all(root).expect("cleanup");
    }
}
