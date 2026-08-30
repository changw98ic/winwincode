// SPDX-License-Identifier: Apache-2.0

//! Customer-key encrypted loopback Vault/KMS adapter.
//!
//! This module implements the canonical [`SecretStorePort`] without claiming a
//! connection to a cloud service. It is the deterministic offline contract for
//! a future network Vault/KMS transport: reference identities are opaque,
//! values are envelope-encrypted at rest, leases are short lived, rotations
//! keep the metadata-selected version readable, and revocation is deny-first.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::CredentialReferenceId;

use crate::credential_reference::{
    CredentialReferenceResolution, ResolvedSecret, SecretStoreError, SecretStorePort,
};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const LOCK_FILE_NAME: &str = ".vault-kms.lock";
const REVOCATION_FILE_NAME: &str = ".revoked";
const ENVELOPE_SCHEMA: &str = "winwincode.vault-kms-envelope.v1";
const VERSION_PREFIX: &str = "v";
const VERSION_SUFFIX: &str = ".vault";
const TEMPORARY_PREFIX: &str = ".envelope.";
const TEMPORARY_SUFFIX: &str = ".tmp";
const NONCE_BYTES: usize = 12;
const KEY_BYTES: usize = 32;
const MAX_SECRET_BYTES: usize = 1024 * 1024;
const MAX_ENVELOPE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_KEY_VERSION: u64 = 9_007_199_254_740_991;
const MAX_LEASE_TTL_MS: u64 = 15 * 60 * 1000;

static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(1);

/// Customer-controlled AES-256 key material used only at the KMS boundary.
///
/// Debug output is redacted, cloning and serialization are absent, and the
/// owned key bytes are cleared on drop.
pub struct VaultKmsKeyMaterial {
    version: u64,
    bytes: [u8; KEY_BYTES],
}

impl VaultKmsKeyMaterial {
    /// Constructs one explicitly versioned customer-controlled key.
    ///
    /// # Errors
    ///
    /// Rejects zero/out-of-range versions or a key of the wrong size.
    pub fn try_new(version: u64, bytes: Vec<u8>) -> Result<Self, SecretStoreError> {
        if version == 0 || version > MAX_KEY_VERSION || bytes.len() != KEY_BYTES {
            return Err(SecretStoreError::corrupt());
        }
        let mut key = [0_u8; KEY_BYTES];
        key.copy_from_slice(&bytes);
        let mut bytes = bytes;
        bytes.fill(0);
        Ok(Self {
            version,
            bytes: key,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

impl fmt::Debug for VaultKmsKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsKeyMaterial")
            .field("version", &self.version)
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl Drop for VaultKmsKeyMaterial {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Keyring supplied by the customer or deployment KMS configuration.
pub struct VaultKmsKeyring {
    active_version: u64,
    keys: BTreeMap<u64, VaultKmsKeyMaterial>,
}

impl VaultKmsKeyring {
    #[must_use]
    pub fn new(active: VaultKmsKeyMaterial) -> Self {
        let active_version = active.version;
        Self {
            active_version,
            keys: BTreeMap::from([(active_version, active)]),
        }
    }

    /// Adds an older decrypt-only key needed during crash-safe rewrap recovery.
    ///
    /// # Errors
    ///
    /// Rejects a duplicate key version or the active version.
    pub fn add_decryption_key(
        mut self,
        key: VaultKmsKeyMaterial,
    ) -> Result<Self, SecretStoreError> {
        if key.version >= self.active_version || self.keys.contains_key(&key.version) {
            return Err(SecretStoreError::version_conflict());
        }
        self.keys.insert(key.version, key);
        Ok(self)
    }
}

impl fmt::Debug for VaultKmsKeyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultKmsKeyring")
            .field("active_version", &self.active_version)
            .field("key_count", &self.keys.len())
            .finish()
    }
}

/// Clock used only to bound one Vault read lease.
pub trait VaultKmsClock: Send + Sync {
    /// Returns the current Unix time in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns a stable failure when the deployment clock is unavailable.
    fn now_ms(&self) -> Result<u64, VaultKmsClockError>;
}

/// Stable clock failure with no backend or key diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultKmsClockError;

impl fmt::Display for VaultKmsClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Vault/KMS clock is unavailable")
    }
}

impl std::error::Error for VaultKmsClockError {}

/// Production wall clock for Vault read leases.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemVaultKmsClock;

impl VaultKmsClock for SystemVaultKmsClock {
    fn now_ms(&self) -> Result<u64, VaultKmsClockError> {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| VaultKmsClockError)?
            .as_millis();
        u64::try_from(millis).map_err(|_| VaultKmsClockError)
    }
}

/// Secret-free proof that one bounded Vault read lease was issued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultSecretLeaseReceipt {
    lease_id: String,
    credential_reference_id: CredentialReferenceId,
    rotation_version: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

impl VaultSecretLeaseReceipt {
    #[must_use]
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }

    #[must_use]
    pub const fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

/// Owned plaintext plus its short-lived secret-safe lease receipt.
pub struct VaultLeasedSecret {
    secret: ResolvedSecret,
    receipt: VaultSecretLeaseReceipt,
}

impl VaultLeasedSecret {
    #[must_use]
    pub const fn receipt(&self) -> &VaultSecretLeaseReceipt {
        &self.receipt
    }

    #[must_use]
    pub fn into_secret(self) -> ResolvedSecret {
        self.secret
    }
}

impl fmt::Debug for VaultLeasedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultLeasedSecret")
            .field("secret", &"[REDACTED]")
            .field("receipt", &self.receipt)
            .finish()
    }
}

/// Secret-free write receipt for an immutable encrypted Credential version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultSecretWriteReceipt {
    credential_reference_id: CredentialReferenceId,
    rotation_version: u64,
    key_version: u64,
    replayed: bool,
}

impl VaultSecretWriteReceipt {
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }

    #[must_use]
    pub const fn key_version(&self) -> u64 {
        self.key_version
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

/// Secret-free cleanup or revocation result.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VaultSecretCleanupReceipt {
    removed_versions: u64,
}

impl VaultSecretCleanupReceipt {
    #[must_use]
    pub const fn removed_versions(self) -> u64 {
        self.removed_versions
    }
}

/// Secret-free customer-key rewrap result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultKmsRewrapReceipt {
    key_version: u64,
    rewrapped_versions: u64,
}

impl VaultKmsRewrapReceipt {
    #[must_use]
    pub const fn key_version(self) -> u64 {
        self.key_version
    }

    #[must_use]
    pub const fn rewrapped_versions(self) -> u64 {
        self.rewrapped_versions
    }
}

/// Encrypted local loopback for the enterprise Vault/KMS contract.
///
/// This adapter deliberately performs no cloud or network call. A deployment
/// supplies the customer-managed keyring out of band; only canonical encrypted
/// envelopes and deny-first revocation tombstones are persisted.
pub struct VaultKmsSecretStoreAdapter {
    root: PathBuf,
    lock_path: PathBuf,
    keyring: Mutex<VaultKmsKeyring>,
    clock: Arc<dyn VaultKmsClock>,
    lease_ttl_ms: u64,
}

impl VaultKmsSecretStoreAdapter {
    /// Opens the encrypted loopback store with an out-of-band keyring.
    ///
    /// # Errors
    ///
    /// Rejects unsafe paths, invalid lease bounds, or an inconsistent keyring.
    pub fn open(
        root: impl AsRef<Path>,
        keyring: VaultKmsKeyring,
        clock: Arc<dyn VaultKmsClock>,
        lease_ttl_ms: u64,
    ) -> Result<Self, SecretStoreError> {
        if lease_ttl_ms == 0
            || lease_ttl_ms > MAX_LEASE_TTL_MS
            || !keyring.keys.contains_key(&keyring.active_version)
        {
            return Err(SecretStoreError::corrupt());
        }
        create_secure_directory(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref()).map_err(|_| SecretStoreError::unavailable())?;
        ensure_secure_directory(&root)?;
        let lock_path = root.join(LOCK_FILE_NAME);
        ensure_regular_file(&lock_path)?;
        let store = Self {
            root,
            lock_path,
            keyring: Mutex::new(keyring),
            clock,
            lease_ttl_ms,
        };
        let lock = store.acquire_lock()?;
        store.recover_temporary_envelopes()?;
        sync_directory(&store.root)?;
        drop(lock);
        Ok(store)
    }

    /// Publishes the exact encrypted version selected by metadata.
    ///
    /// # Errors
    ///
    /// Exact retries replay; changed bytes for an existing version conflict.
    pub fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<VaultSecretWriteReceipt, SecretStoreError> {
        let result = self.publish(reference, reference.rotation_version(), &secret);
        drop(secret);
        result
    }

    /// Stages the next encrypted Credential version before metadata advances.
    ///
    /// # Errors
    ///
    /// Returns a version conflict at the supported boundary or on changed
    /// replay input.
    pub fn rotate_secret(
        &self,
        current: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<VaultSecretWriteReceipt, SecretStoreError> {
        let next_version = current
            .rotation_version()
            .checked_add(1)
            .filter(|version| *version <= MAX_KEY_VERSION)
            .ok_or_else(SecretStoreError::version_conflict)?;
        let result = self.publish(current, next_version, &secret);
        drop(secret);
        result
    }

    /// Resolves one exact version and returns its bounded Vault read lease.
    ///
    /// # Errors
    ///
    /// Revoked, missing, corrupt, or undecryptable envelopes fail with stable
    /// secret-safe categories.
    pub fn resolve_lease(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<VaultLeasedSecret, SecretStoreError> {
        let lock = self.acquire_lock()?;
        let reference_digest = reference_digest(reference)?;
        let directory = self.root.join(&reference_digest);
        if !directory.try_exists().map_err(unavailable)? {
            return Err(SecretStoreError::missing());
        }
        ensure_secure_directory(&directory)?;
        ensure_not_revoked(&directory)?;
        let path = directory.join(version_file_name(reference.rotation_version()));
        if !path.try_exists().map_err(unavailable)? {
            return Err(SecretStoreError::missing());
        }
        let envelope = read_envelope(&path)?;
        let aad = envelope_aad(&reference_digest, reference.rotation_version());
        let keyring = self
            .keyring
            .lock()
            .map_err(|_| SecretStoreError::unavailable())?;
        let plaintext = decrypt_envelope(&envelope, &aad, &keyring)?;
        drop(keyring);
        drop(lock);
        let issued_at_ms = self
            .clock
            .now_ms()
            .map_err(|_| SecretStoreError::unavailable())?;
        let expires_at_ms = issued_at_ms
            .checked_add(self.lease_ttl_ms)
            .ok_or_else(SecretStoreError::unavailable)?;
        let lease_id = lease_id(&reference_digest, issued_at_ms)?;
        Ok(VaultLeasedSecret {
            secret: ResolvedSecret::from_bytes(plaintext)?,
            receipt: VaultSecretLeaseReceipt {
                lease_id,
                credential_reference_id: reference.credential_reference_id().clone(),
                rotation_version: reference.rotation_version(),
                issued_at_ms,
                expires_at_ms,
            },
        })
    }

    /// Activates a new customer key while retaining old decrypt-only keys.
    ///
    /// # Errors
    ///
    /// Rejects non-monotonic or duplicate key versions.
    pub fn activate_key(&self, key: VaultKmsKeyMaterial) -> Result<(), SecretStoreError> {
        let mut keyring = self
            .keyring
            .lock()
            .map_err(|_| SecretStoreError::unavailable())?;
        if key.version <= keyring.active_version || keyring.keys.contains_key(&key.version) {
            return Err(SecretStoreError::version_conflict());
        }
        keyring.active_version = key.version;
        keyring.keys.insert(key.version, key);
        Ok(())
    }

    /// Re-encrypts every current envelope with the active customer key.
    ///
    /// A crash may leave a mix of old and new envelopes; reopening with both
    /// keys remains readable and replaying this operation converges.
    ///
    /// # Errors
    ///
    /// Corrupt envelopes or missing decrypt-only keys fail closed.
    pub fn rewrap_all(&self) -> Result<VaultKmsRewrapReceipt, SecretStoreError> {
        let lock = self.acquire_lock()?;
        let keyring = self
            .keyring
            .lock()
            .map_err(|_| SecretStoreError::unavailable())?;
        let active_version = keyring.active_version;
        let mut rewrapped_versions = 0_u64;
        for (reference_digest, path, version) in self.envelope_paths()? {
            let envelope = read_envelope(&path)?;
            if envelope.key_version == active_version {
                continue;
            }
            let aad = envelope_aad(&reference_digest, version);
            let mut plaintext = decrypt_envelope(&envelope, &aad, &keyring)?;
            let replacement = encrypt_envelope(
                active_version,
                keyring
                    .keys
                    .get(&active_version)
                    .ok_or_else(SecretStoreError::unavailable)?,
                &aad,
                &plaintext,
            )?;
            plaintext.fill(0);
            replace_envelope(&path, &replacement)?;
            rewrapped_versions = rewrapped_versions
                .checked_add(1)
                .ok_or_else(SecretStoreError::corrupt)?;
        }
        sync_directory(&self.root)?;
        drop(keyring);
        drop(lock);
        Ok(VaultKmsRewrapReceipt {
            key_version: active_version,
            rewrapped_versions,
        })
    }

    /// Retires a decrypt-only key after every envelope has been rewrapped.
    ///
    /// # Errors
    ///
    /// The active key or any key still referenced by an envelope cannot retire.
    pub fn retire_key(&self, key_version: u64) -> Result<(), SecretStoreError> {
        let lock = self.acquire_lock()?;
        let mut keyring = self
            .keyring
            .lock()
            .map_err(|_| SecretStoreError::unavailable())?;
        if key_version == keyring.active_version || !keyring.keys.contains_key(&key_version) {
            return Err(SecretStoreError::version_conflict());
        }
        for (_, path, _) in self.envelope_paths()? {
            if read_envelope(&path)?.key_version == key_version {
                return Err(SecretStoreError::version_conflict());
            }
        }
        keyring.keys.remove(&key_version);
        drop(keyring);
        drop(lock);
        Ok(())
    }

    /// Removes obsolete Credential versions after metadata commits rotation.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe failures.
    pub fn cleanup(
        &self,
        current: &CredentialReferenceResolution,
    ) -> Result<VaultSecretCleanupReceipt, SecretStoreError> {
        let lock = self.acquire_lock()?;
        let directory = self.root.join(reference_digest(current)?);
        if !directory.try_exists().map_err(unavailable)? {
            return Ok(VaultSecretCleanupReceipt::default());
        }
        ensure_secure_directory(&directory)?;
        let mut removed_versions = 0_u64;
        for entry in fs::read_dir(&directory).map_err(unavailable)? {
            let path = entry.map_err(unavailable)?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return Err(SecretStoreError::corrupt());
            };
            if name == REVOCATION_FILE_NAME || is_temporary_name(name) {
                continue;
            }
            let version = parse_version_file_name(name).ok_or_else(SecretStoreError::corrupt)?;
            if version != current.rotation_version() {
                fs::remove_file(&path).map_err(unavailable)?;
                removed_versions = removed_versions
                    .checked_add(1)
                    .ok_or_else(SecretStoreError::corrupt)?;
            }
        }
        sync_directory(&directory)?;
        drop(lock);
        Ok(VaultSecretCleanupReceipt { removed_versions })
    }

    /// Persists a deny-first tombstone before removing encrypted versions.
    ///
    /// Already-issued [`ResolvedSecret`] values remain owned by their accepted
    /// request; every later adapter lookup is rejected immediately.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe failures.
    pub fn revoke(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<VaultSecretCleanupReceipt, SecretStoreError> {
        let lock = self.acquire_lock()?;
        let directory = self.root.join(reference_digest(reference)?);
        create_secure_directory(&directory)?;
        let tombstone = directory.join(REVOCATION_FILE_NAME);
        if !tombstone.try_exists().map_err(unavailable)? {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(FILE_MODE)
                .open(&tombstone)
                .map_err(unavailable)?;
            file.sync_all().map_err(unavailable)?;
            sync_directory(&directory)?;
        }
        let mut removed_versions = 0_u64;
        for entry in fs::read_dir(&directory).map_err(unavailable)? {
            let path = entry.map_err(unavailable)?.path();
            if path == tombstone {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(SecretStoreError::corrupt)?;
            if parse_version_file_name(name).is_some() || is_temporary_name(name) {
                fs::remove_file(&path).map_err(unavailable)?;
                if parse_version_file_name(name).is_some() {
                    removed_versions = removed_versions
                        .checked_add(1)
                        .ok_or_else(SecretStoreError::corrupt)?;
                }
            } else {
                return Err(SecretStoreError::corrupt());
            }
        }
        sync_directory(&directory)?;
        drop(lock);
        Ok(VaultSecretCleanupReceipt { removed_versions })
    }

    fn publish(
        &self,
        reference: &CredentialReferenceResolution,
        rotation_version: u64,
        secret: &ResolvedSecret,
    ) -> Result<VaultSecretWriteReceipt, SecretStoreError> {
        if secret.expose().is_empty() || secret.expose().len() > MAX_SECRET_BYTES {
            return Err(SecretStoreError::corrupt());
        }
        let lock = self.acquire_lock()?;
        let reference_digest = reference_digest(reference)?;
        let directory = self.root.join(&reference_digest);
        create_secure_directory(&directory)?;
        ensure_not_revoked(&directory)?;
        remove_temporary_files(&directory)?;
        let path = directory.join(version_file_name(rotation_version));
        let aad = envelope_aad(&reference_digest, rotation_version);
        let keyring = self
            .keyring
            .lock()
            .map_err(|_| SecretStoreError::unavailable())?;
        let active_version = keyring.active_version;
        if path.try_exists().map_err(unavailable)? {
            let envelope = read_envelope(&path)?;
            let mut existing = decrypt_envelope(&envelope, &aad, &keyring)?;
            let exact = existing == secret.expose();
            existing.fill(0);
            if !exact {
                return Err(SecretStoreError::version_conflict());
            }
            return Ok(VaultSecretWriteReceipt {
                credential_reference_id: reference.credential_reference_id().clone(),
                rotation_version,
                key_version: envelope.key_version,
                replayed: true,
            });
        }
        let envelope = encrypt_envelope(
            active_version,
            keyring
                .keys
                .get(&active_version)
                .ok_or_else(SecretStoreError::unavailable)?,
            &aad,
            secret.expose(),
        )?;
        write_new_envelope(&path, &envelope)?;
        sync_directory(&directory)?;
        sync_directory(&self.root)?;
        drop(keyring);
        drop(lock);
        Ok(VaultSecretWriteReceipt {
            credential_reference_id: reference.credential_reference_id().clone(),
            rotation_version,
            key_version: active_version,
            replayed: false,
        })
    }

    fn acquire_lock(&self) -> Result<File, SecretStoreError> {
        ensure_secure_directory(&self.root)?;
        ensure_regular_file_mode(&self.lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(unavailable)?;
        lock.lock().map_err(unavailable)?;
        Ok(lock)
    }

    fn envelope_paths(&self) -> Result<Vec<(String, PathBuf, u64)>, SecretStoreError> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
            if name == LOCK_FILE_NAME {
                continue;
            }
            if !canonical_digest(name) || !entry.file_type().map_err(unavailable)?.is_dir() {
                return Err(SecretStoreError::corrupt());
            }
            ensure_secure_directory(&entry.path())?;
            for envelope in fs::read_dir(entry.path()).map_err(unavailable)? {
                let path = envelope.map_err(unavailable)?.path();
                let file_name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(SecretStoreError::corrupt)?;
                if file_name == REVOCATION_FILE_NAME || is_temporary_name(file_name) {
                    continue;
                }
                let version =
                    parse_version_file_name(file_name).ok_or_else(SecretStoreError::corrupt)?;
                paths.push((name.to_owned(), path, version));
            }
        }
        paths.sort_by(|left, right| left.1.cmp(&right.1));
        Ok(paths)
    }

    fn recover_temporary_envelopes(&self) -> Result<(), SecretStoreError> {
        for entry in fs::read_dir(&self.root).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name();
            if name == LOCK_FILE_NAME {
                continue;
            }
            let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
            if !canonical_digest(name) || !entry.file_type().map_err(unavailable)?.is_dir() {
                return Err(SecretStoreError::corrupt());
            }
            ensure_secure_directory(&entry.path())?;
            remove_temporary_files(&entry.path())?;
        }
        Ok(())
    }
}

impl SecretStorePort for VaultKmsSecretStoreAdapter {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolve_lease(reference)
            .map(VaultLeasedSecret::into_secret)
    }
}

impl fmt::Debug for VaultKmsSecretStoreAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active_key_version = self
            .keyring
            .lock()
            .map_or(0, |keyring| keyring.active_version);
        formatter
            .debug_struct("VaultKmsSecretStoreAdapter")
            .field("root", &"[REDACTED]")
            .field("active_key_version", &active_key_version)
            .field("lease_ttl_ms", &self.lease_ttl_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EncryptedEnvelope {
    schema: String,
    key_version: u64,
    nonce: String,
    ciphertext: String,
}

fn encrypt_envelope(
    key_version: u64,
    key: &VaultKmsKeyMaterial,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<EncryptedEnvelope, SecretStoreError> {
    let cipher = Aes256Gcm::new_from_slice(&key.bytes).map_err(|_| SecretStoreError::corrupt())?;
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| SecretStoreError::unavailable())?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SecretStoreError::unavailable())?;
    Ok(EncryptedEnvelope {
        schema: ENVELOPE_SCHEMA.to_owned(),
        key_version,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

fn decrypt_envelope(
    envelope: &EncryptedEnvelope,
    aad: &[u8],
    keyring: &VaultKmsKeyring,
) -> Result<Vec<u8>, SecretStoreError> {
    if envelope.schema != ENVELOPE_SCHEMA
        || envelope.key_version == 0
        || envelope.key_version > MAX_KEY_VERSION
    {
        return Err(SecretStoreError::corrupt());
    }
    let nonce = STANDARD
        .decode(&envelope.nonce)
        .map_err(|_| SecretStoreError::corrupt())?;
    let nonce: [u8; NONCE_BYTES] = nonce.try_into().map_err(|_| SecretStoreError::corrupt())?;
    let ciphertext = STANDARD
        .decode(&envelope.ciphertext)
        .map_err(|_| SecretStoreError::corrupt())?;
    let key = keyring
        .keys
        .get(&envelope.key_version)
        .ok_or_else(SecretStoreError::unavailable)?;
    let cipher = Aes256Gcm::new_from_slice(&key.bytes).map_err(|_| SecretStoreError::corrupt())?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| SecretStoreError::corrupt())?;
    if plaintext.is_empty() || plaintext.len() > MAX_SECRET_BYTES {
        return Err(SecretStoreError::corrupt());
    }
    Ok(plaintext)
}

fn read_envelope(path: &Path) -> Result<EncryptedEnvelope, SecretStoreError> {
    ensure_regular_file_mode(path)?;
    let metadata = fs::metadata(path).map_err(unavailable)?;
    if metadata.len() == 0 || metadata.len() > MAX_ENVELOPE_BYTES {
        return Err(SecretStoreError::corrupt());
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| SecretStoreError::corrupt())?,
    );
    File::open(path)
        .map_err(unavailable)?
        .take(MAX_ENVELOPE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if u64::try_from(bytes.len()).map_err(|_| SecretStoreError::corrupt())? != metadata.len() {
        return Err(SecretStoreError::corrupt());
    }
    let envelope: EncryptedEnvelope =
        serde_json::from_slice(&bytes).map_err(|_| SecretStoreError::corrupt())?;
    if serde_json::to_vec(&envelope).map_err(|_| SecretStoreError::corrupt())? != bytes {
        return Err(SecretStoreError::corrupt());
    }
    Ok(envelope)
}

fn write_new_envelope(path: &Path, envelope: &EncryptedEnvelope) -> Result<(), SecretStoreError> {
    let bytes = serde_json::to_vec(envelope).map_err(|_| SecretStoreError::corrupt())?;
    let directory = path.parent().ok_or_else(SecretStoreError::corrupt)?;
    let temporary = temporary_path(directory);
    write_temporary_envelope(&temporary, &bytes)?;
    match fs::hard_link(&temporary, path) {
        Ok(()) => {
            fs::remove_file(&temporary).map_err(unavailable)?;
            sync_directory(directory)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary).map_err(unavailable)?;
            Err(SecretStoreError::version_conflict())
        }
        Err(_) => {
            let _ = fs::remove_file(&temporary);
            Err(SecretStoreError::unavailable())
        }
    }
}

fn replace_envelope(path: &Path, envelope: &EncryptedEnvelope) -> Result<(), SecretStoreError> {
    let bytes = serde_json::to_vec(envelope).map_err(|_| SecretStoreError::corrupt())?;
    let directory = path.parent().ok_or_else(SecretStoreError::corrupt)?;
    let temporary = temporary_path(directory);
    write_temporary_envelope(&temporary, &bytes)?;
    fs::rename(&temporary, path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        SecretStoreError::unavailable()
    })?;
    sync_directory(directory)
}

fn write_temporary_envelope(path: &Path, bytes: &[u8]) -> Result<(), SecretStoreError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(unavailable)?;
    file.write_all(bytes).map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    drop(file);
    ensure_regular_file_mode(path)
}

fn temporary_path(directory: &Path) -> PathBuf {
    let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
    directory.join(format!(
        "{TEMPORARY_PREFIX}{}.{sequence}{TEMPORARY_SUFFIX}",
        std::process::id()
    ))
}

fn reference_digest(reference: &CredentialReferenceResolution) -> Result<String, SecretStoreError> {
    let scope = serde_json::to_vec(reference.scope()).map_err(|_| SecretStoreError::corrupt())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.vault-kms-reference.v1\0");
    update_framed(&mut digest, &scope);
    update_framed(
        &mut digest,
        reference.credential_reference_id().0.as_bytes(),
    );
    update_framed(&mut digest, reference.provider_id().as_bytes());
    Ok(lower_hex(&digest.finalize()))
}

fn envelope_aad(reference_digest: &str, rotation_version: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(96);
    aad.extend_from_slice(b"winwincode.vault-kms-envelope-aad.v1\0");
    aad.extend_from_slice(reference_digest.as_bytes());
    aad.push(0);
    aad.extend_from_slice(&rotation_version.to_be_bytes());
    aad
}

fn lease_id(reference_digest: &str, issued_at_ms: u64) -> Result<String, SecretStoreError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(|_| SecretStoreError::unavailable())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.vault-kms-lease.v1\0");
    digest.update(reference_digest.as_bytes());
    digest.update(issued_at_ms.to_be_bytes());
    digest.update(entropy);
    entropy.fill(0);
    Ok(format!(
        "vlt_{}",
        lower_hex(&digest.finalize())[..32].to_owned()
    ))
}

fn update_framed(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn version_file_name(version: u64) -> String {
    format!("{VERSION_PREFIX}{version}{VERSION_SUFFIX}")
}

fn parse_version_file_name(value: &str) -> Option<u64> {
    let version = value
        .strip_prefix(VERSION_PREFIX)?
        .strip_suffix(VERSION_SUFFIX)?;
    if version.is_empty()
        || version.starts_with('0')
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    version
        .parse::<u64>()
        .ok()
        .filter(|version| *version <= MAX_KEY_VERSION)
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_temporary_name(value: &str) -> bool {
    value.starts_with(TEMPORARY_PREFIX) && value.ends_with(TEMPORARY_SUFFIX)
}

fn ensure_not_revoked(directory: &Path) -> Result<(), SecretStoreError> {
    if directory
        .join(REVOCATION_FILE_NAME)
        .try_exists()
        .map_err(unavailable)?
    {
        Err(SecretStoreError::missing())
    } else {
        Ok(())
    }
}

fn create_secure_directory(path: &Path) -> Result<(), SecretStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SecretStoreError::corrupt());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(DIRECTORY_MODE);
            builder.create(path).map_err(unavailable)?;
        }
        Err(error) => return Err(unavailable(error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE)).map_err(unavailable)?;
    ensure_secure_directory(path)
}

fn ensure_secure_directory(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.mode() & 0o777 != DIRECTORY_MODE
    {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn ensure_regular_file(path: &Path) -> Result<(), SecretStoreError> {
    if !path.try_exists().map_err(unavailable)? {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(FILE_MODE)
            .open(path)
            .map_err(unavailable)?;
        file.sync_all().map_err(unavailable)?;
    }
    ensure_regular_file_mode(path)
}

fn ensure_regular_file_mode(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.mode() & 0o777 != FILE_MODE
    {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn remove_temporary_files(directory: &Path) -> Result<(), SecretStoreError> {
    for entry in fs::read_dir(directory).map_err(unavailable)? {
        let entry = entry.map_err(unavailable)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
        if is_temporary_name(name) {
            let metadata = fs::symlink_metadata(entry.path()).map_err(unavailable)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(SecretStoreError::corrupt());
            }
            fs::remove_file(entry.path()).map_err(unavailable)?;
        }
    }
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), SecretStoreError> {
    File::open(path)
        .map_err(unavailable)?
        .sync_all()
        .map_err(unavailable)
}

fn unavailable(_error: std::io::Error) -> SecretStoreError {
    SecretStoreError::unavailable()
}
