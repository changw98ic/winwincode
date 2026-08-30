// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRotateCommand,
    CredentialReferenceRotateCommandCommand, CredentialReferenceRotatePayload, OrganizationScope,
    OrganizationScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceResolution, CredentialReferenceService, LocalSecretStoreAdapter,
    ProductStateStorage, ResolvedSecret, SecretStoreErrorKind, SecretStorePort,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_storage::SqliteStorage;

const INITIAL_SECRET: &[u8] = b"INITIAL_PROVIDER_SECRET_MATERIAL";
const ROTATED_SECRET: &[u8] = b"ROTATED_PROVIDER_SECRET_MATERIAL";
const CONFLICTING_SECRET: &[u8] = b"CONFLICTING_PROVIDER_SECRET_MATERIAL";

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-local-secret-store-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn create_command(seed: u64) -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(seed),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", seed)),
            display_name: "Provider credential".to_owned(),
            provider_id: "provider-main".to_owned(),
            vault_locator: "local-secret-store://write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
    }
}

fn rotate_command(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
) -> CredentialReferenceRotateCommand {
    CredentialReferenceRotateCommand {
        actor: create.actor.clone(),
        command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
        expected_revision: Revision(1),
        payload: CredentialReferenceRotatePayload {
            credential_reference_id: create.payload.credential_reference_id.clone(),
            vault_locator: "local-secret-store://next-write-only".to_owned(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: create.scope.clone(),
    }
}

fn create_reference(
    storage: &mut SqliteStorage,
    seed: u64,
) -> (
    CredentialReferenceCreateCommand,
    CredentialReferenceResolution,
) {
    let command = create_command(seed);
    CredentialReferenceService::new(storage)
        .create(&command, 1_800_000_000_000)
        .expect("create Credential reference metadata");
    let reference = CredentialReferenceService::new(storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve created Credential reference");
    (command, reference)
}

fn version_files(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .expect("read secret root")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(fs::FileType::is_dir)
                .map(|_| entry.path())
        })
        .flat_map(|directory| {
            fs::read_dir(directory)
                .expect("read reference directory")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect::<Vec<_>>()
        })
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "secret")
        })
        .collect()
}

#[test]
fn restart_rotation_cleanup_and_delete_keep_the_metadata_selected_version_readable() {
    let root = temporary_directory("restart-rotation");
    let secret_root = root.join("secrets");
    let mut storage = SqliteStorage::open(root.join("metadata"))
        .expect("open Credential reference metadata storage");
    let (create, version_one) = create_reference(&mut storage, 1);

    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("open local SecretStore");
    let stored = adapter
        .store(
            &version_one,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("atomically store initial version");
    assert_eq!(stored.rotation_version(), 1);
    assert!(!stored.replayed());
    drop(adapter);

    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("reopen local SecretStore");
    assert_eq!(
        adapter
            .resolve(&version_one)
            .expect("resolve initial version after restart")
            .expose(),
        INITIAL_SECRET
    );

    let staged = adapter
        .rotate(
            &version_one,
            ResolvedSecret::from_bytes(ROTATED_SECRET.to_vec()).expect("rotated secret"),
        )
        .expect("stage next secret version atomically");
    assert_eq!(staged.rotation_version(), 2);
    assert!(!staged.replayed());
    drop(adapter);
    let adapter = LocalSecretStoreAdapter::open(&secret_root)
        .expect("restart after staging and before metadata commit");
    assert_eq!(
        adapter
            .resolve(&version_one)
            .expect("old metadata still selects old secret")
            .expose(),
        INITIAL_SECRET
    );

    let rotate = rotate_command(&create, 2);
    CredentialReferenceService::new(&mut storage)
        .rotate(&rotate, 1_800_000_001_000)
        .expect("commit rotated metadata");
    let version_two = CredentialReferenceService::new(&mut storage)
        .resolve(&create.scope, &create.payload.credential_reference_id)
        .expect("resolve rotated Credential reference");
    drop(adapter);
    let adapter = LocalSecretStoreAdapter::open(&secret_root)
        .expect("restart after metadata commit and before old-version cleanup");
    assert_eq!(
        adapter
            .resolve(&version_two)
            .expect("new metadata selects new secret")
            .expose(),
        ROTATED_SECRET
    );

    let cleaned = adapter
        .cleanup(&version_two)
        .expect("clean obsolete secret version");
    assert_eq!(cleaned.removed_versions(), 1);
    assert_eq!(
        adapter
            .resolve(&version_one)
            .expect_err("cleaned old version is absent")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    drop(adapter);

    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("reopen after cleanup");
    assert_eq!(
        adapter
            .resolve(&version_two)
            .expect("current secret survives another restart")
            .expose(),
        ROTATED_SECRET
    );
    let deleted = adapter
        .delete(&version_two)
        .expect("remove all versions after metadata deletion");
    assert_eq!(deleted.removed_versions(), 1);
    assert_eq!(
        adapter
            .resolve(&version_two)
            .expect_err("deleted local secret is absent")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    assert_eq!(
        adapter
            .delete(&version_two)
            .expect("local deletion replay is idempotent")
            .removed_versions(),
        0
    );

    Box::new(storage).close().expect("close metadata storage");
    fs::remove_dir_all(root).expect("remove local SecretStore fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn owner_only_permissions_recovery_and_failures_do_not_expose_secret_material() {
    let root = temporary_directory("permissions-recovery");
    let secret_root = root.join("secrets");
    let mut storage = SqliteStorage::open(root.join("metadata"))
        .expect("open Credential reference metadata storage");
    let (create, reference) = create_reference(&mut storage, 10);
    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("open local SecretStore");
    let receipt = adapter
        .store(
            &reference,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("store permission fixture");

    let secret_files = version_files(&secret_root);
    assert_eq!(secret_files.len(), 1);
    let secret_file = &secret_files[0];
    let reference_directory = secret_file.parent().expect("reference directory");
    assert_eq!(
        fs::metadata(&secret_root)
            .expect("secret root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(reference_directory)
            .expect("reference directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for file in [secret_root.join(".secret-store.lock"), secret_file.clone()] {
        assert_eq!(
            fs::metadata(file)
                .expect("protected file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let conflict = adapter
        .store(
            &reference,
            ResolvedSecret::from_bytes(CONFLICTING_SECRET.to_vec()).expect("conflicting secret"),
        )
        .expect_err("a different value cannot replace an immutable version");
    assert_eq!(conflict.kind(), SecretStoreErrorKind::VersionConflict);
    let public_text = format!("{receipt:?} {conflict:?} {conflict}");
    for forbidden in [
        String::from_utf8_lossy(INITIAL_SECRET).as_ref(),
        String::from_utf8_lossy(CONFLICTING_SECRET).as_ref(),
    ] {
        assert!(!public_text.contains(forbidden));
    }
    for path in fs::read_dir(&secret_root)
        .expect("list secret root")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .chain(
            fs::read_dir(reference_directory)
                .expect("list reference directory")
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned()),
        )
    {
        assert!(!path.contains(&create.payload.credential_reference_id.0));
        assert!(!path.contains(&create.payload.provider_id));
        assert!(!path.contains(String::from_utf8_lossy(INITIAL_SECRET).as_ref()));
    }

    let orphan = reference_directory.join(".secret.999.999.tmp");
    let mut orphan_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&orphan)
        .expect("create simulated crash temporary file");
    orphan_file
        .write_all(ROTATED_SECRET)
        .expect("write simulated crash temporary secret");
    orphan_file.sync_all().expect("sync simulated crash file");
    drop(orphan_file);
    drop(adapter);
    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("recover local SecretStore");
    assert!(!orphan.exists());
    assert_eq!(
        adapter
            .resolve(&reference)
            .expect("complete version survives orphan recovery")
            .expose(),
        INITIAL_SECRET
    );

    fs::set_permissions(secret_file, fs::Permissions::from_mode(0o644))
        .expect("weaken fixture permission");
    let permission_error = adapter
        .resolve(&reference)
        .expect_err("adapter rejects a secret file readable by other users");
    assert_eq!(permission_error.kind(), SecretStoreErrorKind::Corrupt);
    let permission_text = format!("{permission_error:?} {permission_error}");
    assert!(!permission_text.contains(String::from_utf8_lossy(INITIAL_SECRET).as_ref()));
    fs::set_permissions(secret_file, fs::Permissions::from_mode(0o600))
        .expect("restore fixture permission");
    adapter.delete(&reference).expect("clean secret fixture");

    Box::new(storage).close().expect("close metadata storage");
    fs::remove_dir_all(root).expect("remove local SecretStore fixture");
}

#[test]
fn concurrent_store_and_rotation_publish_only_one_value_per_version() {
    const CALLERS: usize = 6;
    let root = temporary_directory("concurrency");
    let secret_root = root.join("secrets");
    let mut storage = SqliteStorage::open(root.join("metadata"))
        .expect("open Credential reference metadata storage");
    let (create, version_one) = create_reference(&mut storage, 20);

    let barrier = Arc::new(Barrier::new(CALLERS));
    let handles = (0..CALLERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let secret_root = secret_root.clone();
            let reference = version_one.clone();
            thread::spawn(move || {
                let adapter =
                    LocalSecretStoreAdapter::open(secret_root).expect("open concurrent adapter");
                barrier.wait();
                adapter.store(
                    &reference,
                    ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec())
                        .expect("concurrent initial secret"),
                )
            })
        })
        .collect::<Vec<_>>();
    let receipts = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("join concurrent store")
                .expect("concurrent exact store")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.replayed())
            .count(),
        1
    );

    let barrier = Arc::new(Barrier::new(2));
    let handles = [ROTATED_SECRET, CONFLICTING_SECRET]
        .into_iter()
        .map(|candidate| {
            let barrier = Arc::clone(&barrier);
            let secret_root = secret_root.clone();
            let reference = version_one.clone();
            thread::spawn(move || {
                let adapter =
                    LocalSecretStoreAdapter::open(secret_root).expect("open rotation adapter");
                barrier.wait();
                let result = adapter.rotate(
                    &reference,
                    ResolvedSecret::from_bytes(candidate.to_vec()).expect("rotation candidate"),
                );
                (candidate, result)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("join concurrent rotation"))
        .collect::<Vec<_>>();
    let winner = results
        .iter()
        .find_map(|(candidate, result)| result.as_ref().ok().map(|_| *candidate))
        .expect("one rotation candidate wins");
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .find_map(|(_, result)| result.as_ref().err())
            .expect("one rotation candidate conflicts")
            .kind(),
        SecretStoreErrorKind::VersionConflict
    );

    CredentialReferenceService::new(&mut storage)
        .rotate(&rotate_command(&create, 21), 1_800_000_001_000)
        .expect("commit winning rotation metadata");
    let version_two = CredentialReferenceService::new(&mut storage)
        .resolve(&create.scope, &create.payload.credential_reference_id)
        .expect("resolve current metadata version");
    let adapter = LocalSecretStoreAdapter::open(&secret_root).expect("reopen final adapter");
    assert_eq!(
        adapter
            .resolve(&version_two)
            .expect("resolve one published rotation winner")
            .expose(),
        winner
    );
    adapter
        .cleanup(&version_two)
        .expect("cleanup initial version");
    adapter.delete(&version_two).expect("delete final version");

    Box::new(storage).close().expect("close metadata storage");
    fs::remove_dir_all(root).expect("remove local SecretStore fixture");
}
