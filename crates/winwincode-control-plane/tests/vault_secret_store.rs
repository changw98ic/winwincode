// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload,
    CredentialReferenceRotateCommand, CredentialReferenceRotateCommandCommand,
    CredentialReferenceRotatePayload, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CredentialReferenceErrorKind, CredentialReferenceResolution, CredentialReferenceService,
    CredentialSecretResolutionError, ProductStateStorage, ResolvedSecret, SecretStoreError,
    SecretStoreErrorKind, SecretStorePort, VaultKmsClock, VaultKmsClockError, VaultKmsKeyMaterial,
    VaultKmsKeyring, VaultKmsSecretStoreAdapter,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, RequestId, Revision, SchemaVersion, UserId,
};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_storage::SqliteStorage;

const INITIAL_SECRET: &[u8] = b"VAULT_INITIAL_PROVIDER_SECRET";
const ROTATED_SECRET: &[u8] = b"VAULT_ROTATED_PROVIDER_SECRET";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct TestClock(AtomicU64);

impl TestClock {
    fn at(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }
}

impl VaultKmsClock for TestClock {
    fn now_ms(&self) -> Result<u64, VaultKmsClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-vault-kms-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create Vault/KMS fixture root");
        Self(path)
    }

    fn metadata(&self) -> PathBuf {
        self.0.join("metadata")
    }

    fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
            display_name: "Vault/KMS Provider credential".to_owned(),
            provider_id: "provider-vault-loopback".to_owned(),
            vault_locator: "vault-kms://write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
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
        .create(&command, 1_900_000_000_000)
        .expect("create Vault/KMS Credential metadata");
    let reference = CredentialReferenceService::new(storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve Vault/KMS Credential metadata");
    (command, reference)
}

fn rotate_metadata(
    storage: &mut SqliteStorage,
    command: &CredentialReferenceCreateCommand,
    seed: u64,
) -> CredentialReferenceResolution {
    CredentialReferenceService::new(storage)
        .rotate(
            &CredentialReferenceRotateCommand {
                actor: command.actor.clone(),
                command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
                expected_revision: Revision(1),
                payload: CredentialReferenceRotatePayload {
                    credential_reference_id: command.payload.credential_reference_id.clone(),
                    vault_locator: "vault-kms://rotated-write-only".to_owned(),
                },
                request_id: RequestId(id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: command.scope.clone(),
            },
            1_900_000_001_000,
        )
        .expect("rotate Vault/KMS Credential metadata");
    CredentialReferenceService::new(storage)
        .resolve(&command.scope, &command.payload.credential_reference_id)
        .expect("resolve rotated Vault/KMS Credential metadata")
}

fn revoke_metadata(
    storage: &mut SqliteStorage,
    command: &CredentialReferenceCreateCommand,
    seed: u64,
) {
    CredentialReferenceService::new(storage)
        .revoke(
            &CredentialReferenceRevokeCommand {
                actor: command.actor.clone(),
                command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
                expected_revision: Revision(2),
                payload: CredentialReferenceRevokePayload {
                    credential_reference_id: command.payload.credential_reference_id.clone(),
                },
                request_id: RequestId(id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: command.scope.clone(),
            },
            1_900_000_002_000,
        )
        .expect("revoke Vault/KMS Credential metadata");
}

fn key(version: u64, byte: u8) -> VaultKmsKeyMaterial {
    VaultKmsKeyMaterial::try_new(version, vec![byte; 32]).expect("valid fixture KMS key")
}

fn open_store(
    root: &Path,
    keyring: VaultKmsKeyring,
    clock: &Arc<TestClock>,
) -> VaultKmsSecretStoreAdapter {
    let clock: Arc<dyn VaultKmsClock> = Arc::clone(clock) as Arc<dyn VaultKmsClock>;
    VaultKmsSecretStoreAdapter::open(root, keyring, clock, 300).expect("open Vault/KMS loopback")
}

fn assert_files_omit(root: &Path, forbidden: &[&[u8]]) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .expect("read encrypted Vault directory")
                    .map(|entry| entry.expect("Vault entry").path()),
            );
        } else if path.is_file() {
            let bytes = fs::read(path).expect("read encrypted Vault file");
            for value in forbidden {
                assert!(
                    !bytes.windows(value.len()).any(|window| window == *value),
                    "restricted material reached a durable Vault file"
                );
            }
        }
    }
}

fn vault_envelope(root: &Path) -> PathBuf {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .expect("read Vault directory")
                    .map(|entry| entry.expect("Vault entry").path()),
            );
        } else if path
            .extension()
            .is_some_and(|extension| extension == "vault")
        {
            return path;
        }
    }
    panic!("encrypted Vault envelope not found")
}

#[test]
fn encrypted_restart_leases_and_customer_key_rewrap_are_deterministic() {
    let root = TestDirectory::new("restart-rewrap");
    let mut storage = SqliteStorage::open(root.metadata()).expect("open Credential metadata");
    let (_, reference) = create_reference(&mut storage, 1);
    let clock = Arc::new(TestClock::at(10_000));
    let store = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x11)), &clock);

    let stored = store
        .store(
            &reference,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("store encrypted secret");
    assert_eq!(stored.key_version(), 1);
    let first = store.resolve_lease(&reference).expect("first short lease");
    let second = store.resolve_lease(&reference).expect("second short lease");
    assert_ne!(first.receipt().lease_id(), second.receipt().lease_id());
    assert_eq!(first.receipt().issued_at_ms(), 10_000);
    assert_eq!(first.receipt().expires_at_ms(), 10_300);
    assert_eq!(first.into_secret().expose(), INITIAL_SECRET);

    store
        .activate_key(key(2, 0x22))
        .expect("activate customer key 2");
    assert_eq!(
        store
            .retire_key(1)
            .expect_err("old key still referenced")
            .kind(),
        SecretStoreErrorKind::VersionConflict
    );
    drop(store);
    let recovering_keyring = VaultKmsKeyring::new(key(2, 0x22))
        .add_decryption_key(key(1, 0x11))
        .expect("retain decrypt-only customer key 1");
    let recovering = open_store(&root.vault(), recovering_keyring, &clock);
    assert_eq!(
        recovering
            .resolve(&reference)
            .expect("mixed-key crash recovery")
            .expose(),
        INITIAL_SECRET
    );
    let rewrapped = recovering.rewrap_all().expect("rewrap with customer key 2");
    assert_eq!(rewrapped.key_version(), 2);
    assert_eq!(rewrapped.rewrapped_versions(), 1);
    recovering
        .retire_key(1)
        .expect("retire fully rewrapped key 1");
    drop(recovering);

    let restarted = open_store(&root.vault(), VaultKmsKeyring::new(key(2, 0x22)), &clock);
    assert_eq!(
        restarted
            .resolve(&reference)
            .expect("restart resolve")
            .expose(),
        INITIAL_SECRET
    );
    let public_text = format!("{restarted:?} {:?}", key(9, 0x99));
    assert!(!public_text.contains("17, 17"));
    assert_files_omit(&root.vault(), &[INITIAL_SECRET, &[0x11; 32], &[0x22; 32]]);

    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}

struct CountingStore<'a> {
    inner: &'a VaultKmsSecretStoreAdapter,
    calls: AtomicU64,
}

impl SecretStorePort for CountingStore<'_> {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.resolve(reference)
    }
}

fn revoke_metadata_and_assert_adapter_not_called(
    storage: &mut SqliteStorage,
    store: &VaultKmsSecretStoreAdapter,
    command: &CredentialReferenceCreateCommand,
) {
    let counting = CountingStore {
        inner: store,
        calls: AtomicU64::new(0),
    };
    CredentialReferenceService::new(storage)
        .resolve_secret(
            &counting,
            &command.scope,
            &command.payload.credential_reference_id,
        )
        .expect("current secret resolves through canonical service");
    assert_eq!(counting.calls.load(Ordering::Relaxed), 1);
    revoke_metadata(storage, command, 21);
    let revoked = CredentialReferenceService::new(storage)
        .resolve_secret(
            &counting,
            &command.scope,
            &command.payload.credential_reference_id,
        )
        .expect_err("revoked metadata stops before SecretStore");
    assert!(matches!(
        revoked,
        CredentialSecretResolutionError::Reference(error)
            if error.kind() == CredentialReferenceErrorKind::Revoked
    ));
    assert_eq!(counting.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn credential_rotation_keeps_accepted_request_and_revocation_stops_new_requests() {
    let root = TestDirectory::new("rotation-revocation");
    let mut storage = SqliteStorage::open(root.metadata()).expect("open Credential metadata");
    let (command, version_one) = create_reference(&mut storage, 2);
    let clock = Arc::new(TestClock::at(20_000));
    let store = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x33)), &clock);
    store
        .store(
            &version_one,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("store initial version");
    let accepted_before_rotation = store
        .resolve(&version_one)
        .expect("accepted request secret");
    store
        .rotate_secret(
            &version_one,
            ResolvedSecret::from_bytes(ROTATED_SECRET.to_vec()).expect("rotated secret"),
        )
        .expect("stage rotated version");
    drop(store);
    let store = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x33)), &clock);
    assert_eq!(
        store
            .resolve(&version_one)
            .expect("old metadata resolves after staging crash")
            .expose(),
        INITIAL_SECRET
    );

    let version_two = rotate_metadata(&mut storage, &command, 20);
    let accepted_before_revoke = store.resolve(&version_two).expect("rotated request secret");
    assert_eq!(accepted_before_revoke.expose(), ROTATED_SECRET);
    assert_eq!(
        store
            .cleanup(&version_two)
            .expect("cleanup old version")
            .removed_versions(),
        1
    );
    assert_eq!(
        store
            .resolve(&version_one)
            .expect_err("old version removed")
            .kind(),
        SecretStoreErrorKind::Missing
    );

    revoke_metadata_and_assert_adapter_not_called(&mut storage, &store, &command);
    assert_eq!(
        store
            .revoke(&version_two)
            .expect("deny-first Vault revoke")
            .removed_versions(),
        1
    );
    assert_eq!(
        store
            .resolve(&version_two)
            .expect_err("revoked Vault lookup")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    drop(store);
    let restarted = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x33)), &clock);
    assert_eq!(
        restarted
            .resolve(&version_two)
            .expect_err("revocation tombstone survives restart")
            .kind(),
        SecretStoreErrorKind::Missing
    );
    assert_eq!(
        restarted
            .revoke(&version_two)
            .expect("revocation replay")
            .removed_versions(),
        0
    );
    assert_eq!(accepted_before_rotation.expose(), INITIAL_SECRET);
    assert_eq!(accepted_before_revoke.expose(), ROTATED_SECRET);
    assert_files_omit(&root.vault(), &[INITIAL_SECRET, ROTATED_SECRET]);

    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}

#[test]
fn wrong_key_noncanonical_envelope_and_public_diagnostics_fail_closed() {
    let root = TestDirectory::new("corruption");
    let mut storage = SqliteStorage::open(root.metadata()).expect("open Credential metadata");
    let (_, reference) = create_reference(&mut storage, 3);
    let clock = Arc::new(TestClock::at(30_000));
    let store = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x44)), &clock);
    let receipt = store
        .store(
            &reference,
            ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("initial secret"),
        )
        .expect("store encrypted fixture");
    drop(store);

    let wrong_key = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x55)), &clock);
    let decrypt_error = wrong_key.resolve(&reference).expect_err("wrong KMS key");
    assert_eq!(decrypt_error.kind(), SecretStoreErrorKind::Corrupt);
    drop(wrong_key);

    let path = vault_envelope(&root.vault());
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open envelope fixture");
    file.write_all(b" ").expect("make JSON noncanonical");
    file.sync_all().expect("sync corrupt envelope");
    let store = open_store(&root.vault(), VaultKmsKeyring::new(key(1, 0x44)), &clock);
    let format_error = store
        .resolve(&reference)
        .expect_err("noncanonical envelope");
    assert_eq!(format_error.kind(), SecretStoreErrorKind::Corrupt);
    let public = format!("{receipt:?} {decrypt_error:?} {format_error:?} {store:?}");
    assert!(!public.contains(String::from_utf8_lossy(INITIAL_SECRET).as_ref()));
    assert_files_omit(&root.vault(), &[INITIAL_SECRET, &[0x44; 32], &[0x55; 32]]);

    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}

#[test]
fn concurrent_exact_publication_has_one_ciphertext_and_changed_bytes_conflict() {
    const CALLERS: usize = 6;
    let root = TestDirectory::new("concurrent-publication");
    let mut storage = SqliteStorage::open(root.metadata()).expect("open Credential metadata");
    let (_, reference) = create_reference(&mut storage, 4);
    let clock = Arc::new(TestClock::at(40_000));
    let store = Arc::new(open_store(
        &root.vault(),
        VaultKmsKeyring::new(key(1, 0x66)),
        &clock,
    ));
    let barrier = Arc::new(Barrier::new(CALLERS));
    let handles = (0..CALLERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let reference = reference.clone();
            let store = Arc::clone(&store);
            thread::spawn(move || {
                barrier.wait();
                store.store(
                    &reference,
                    ResolvedSecret::from_bytes(INITIAL_SECRET.to_vec()).expect("concurrent secret"),
                )
            })
        })
        .collect::<Vec<_>>();
    let receipts = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("join Vault writer")
                .expect("exact Vault publication")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.replayed())
            .count(),
        1
    );
    let conflict = store
        .store(
            &reference,
            ResolvedSecret::from_bytes(ROTATED_SECRET.to_vec()).expect("changed secret"),
        )
        .expect_err("changed bytes cannot replace an immutable envelope");
    assert_eq!(conflict.kind(), SecretStoreErrorKind::VersionConflict);
    assert_eq!(
        store.resolve(&reference).expect("resolve winner").expose(),
        INITIAL_SECRET
    );
    assert_files_omit(&root.vault(), &[INITIAL_SECRET, ROTATED_SECRET]);

    Box::new(storage)
        .close()
        .expect("close Credential metadata");
}
