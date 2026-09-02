// SPDX-License-Identifier: Apache-2.0

//! Permission-protected local implementation of the canonical secret-store port.
//!
//! Each scope-bound Credential reference owns immutable version files. Rotation
//! publishes the next complete file before metadata advances, so either the old
//! or the new metadata version remains resolvable across a crash. Cleanup only
//! removes versions after the caller supplies the newly committed resolution.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_domain::CredentialReferenceId;

use crate::credential_reference::{
    CredentialReferenceResolution, ResolvedSecret, SecretStoreError, SecretStorePort,
};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const MAX_SECRET_BYTES: u64 = 1024 * 1024;
const MAX_ROTATION_VERSION: u64 = 9_007_199_254_740_991;
const LOCK_FILE_NAME: &str = ".secret-store.lock";
const TEMPORARY_PREFIX: &str = ".secret.";
const TEMPORARY_SUFFIX: &str = ".tmp";

static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(1);

/// Result of one immutable local secret-version publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSecretWriteReceipt {
    credential_reference_id: CredentialReferenceId,
    rotation_version: u64,
    replayed: bool,
}

impl LocalSecretWriteReceipt {
    #[must_use]
    pub const fn credential_reference_id(&self) -> &CredentialReferenceId {
        &self.credential_reference_id
    }

    #[must_use]
    pub const fn rotation_version(&self) -> u64 {
        self.rotation_version
    }

    /// Whether the exact bytes were already durably published.
    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

/// Result of removing obsolete or deleted local secret versions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalSecretCleanupReceipt {
    removed_versions: u64,
}

impl LocalSecretCleanupReceipt {
    #[must_use]
    pub const fn removed_versions(self) -> u64 {
        self.removed_versions
    }
}

/// Filesystem-backed Community/local adapter for [`SecretStorePort`].
///
/// The supplied root is dedicated to secret storage. It and every reference
/// directory are forced to owner-only access; secret and lock files are
/// owner-read/write only. No ID, provider name, scope, or secret appears in a
/// filesystem name.
pub struct LocalSecretStoreAdapter {
    root: PathBuf,
    lock_path: PathBuf,
}

impl LocalSecretStoreAdapter {
    /// Opens or creates a dedicated local secret-store root and removes any
    /// incomplete temporary files left by a stopped process.
    ///
    /// # Errors
    ///
    /// Returns a stable secret-free failure if the root cannot be protected,
    /// locked, or recovered.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SecretStoreError> {
        create_secure_directory(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref()).map_err(|_| SecretStoreError::unavailable())?;
        ensure_secure_directory(&root)?;
        let lock_path = root.join(LOCK_FILE_NAME);
        ensure_lock_file(&lock_path)?;
        let store = Self { root, lock_path };
        let lock = store.acquire_lock()?;
        store.remove_orphaned_temporary_files()?;
        sync_directory(&store.root)?;
        drop(lock);
        Ok(store)
    }

    /// Atomically publishes the exact version represented by `reference`.
    /// Exact retries are idempotent; different bytes for an existing version
    /// are rejected without replacing the durable value.
    ///
    /// The secret is consumed so its owned input buffer is cleared on return.
    ///
    /// # Errors
    ///
    /// Returns only stable secret-safe categories.
    pub fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<LocalSecretWriteReceipt, SecretStoreError> {
        let result = self.publish_version(reference, reference.rotation_version(), &secret);
        drop(secret);
        result
    }

    /// Atomically stages the next immutable version while retaining the current
    /// one. The caller may then commit Credential-reference metadata and call
    /// [`Self::cleanup`] with the new resolution. A crash at either boundary
    /// therefore leaves the metadata-selected version readable.
    ///
    /// # Errors
    ///
    /// Returns a version conflict at the supported version boundary or when a
    /// different value already occupies the next version.
    pub fn rotate(
        &self,
        current: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<LocalSecretWriteReceipt, SecretStoreError> {
        let next_version = current
            .rotation_version()
            .checked_add(1)
            .filter(|version| *version <= MAX_ROTATION_VERSION)
            .ok_or_else(SecretStoreError::version_conflict)?;
        let result = self.publish_version(current, next_version, &secret);
        drop(secret);
        result
    }

    /// Removes every obsolete version after `current` has become the committed
    /// metadata resolution. The operation is idempotent and crash-recoverable.
    ///
    /// # Errors
    ///
    /// Returns a stable secret-free store failure.
    pub fn cleanup(
        &self,
        current: &CredentialReferenceResolution,
    ) -> Result<LocalSecretCleanupReceipt, SecretStoreError> {
        let lock = self.acquire_lock()?;
        self.ensure_root()?;
        let directory = self.reference_directory(current)?;
        if !directory.try_exists().map_err(unavailable)? {
            drop(lock);
            return Ok(LocalSecretCleanupReceipt::default());
        }
        ensure_secure_directory(&directory)?;
        let mut removed_versions = 0_u64;
        for entry in fs::read_dir(&directory).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
            if is_temporary_name(name) {
                remove_temporary_file(&entry.path())?;
                continue;
            }
            let version = parse_version_file_name(name).ok_or_else(SecretStoreError::corrupt)?;
            if version != current.rotation_version() {
                scrub_and_remove(&entry.path())?;
                removed_versions = removed_versions
                    .checked_add(1)
                    .ok_or_else(SecretStoreError::corrupt)?;
            }
        }
        sync_directory(&directory)?;
        drop(lock);
        Ok(LocalSecretCleanupReceipt { removed_versions })
    }

    /// Clears and removes all local versions for a captured scope-bound
    /// resolution. Callers can capture the resolution immediately before a
    /// metadata revoke or delete, then invoke this idempotent cleanup.
    ///
    /// # Errors
    ///
    /// Returns a stable secret-free store failure.
    pub fn delete(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<LocalSecretCleanupReceipt, SecretStoreError> {
        let lock = self.acquire_lock()?;
        self.ensure_root()?;
        let directory = self.reference_directory(reference)?;
        if !directory.try_exists().map_err(unavailable)? {
            drop(lock);
            return Ok(LocalSecretCleanupReceipt::default());
        }
        ensure_secure_directory(&directory)?;
        let mut removed_versions = 0_u64;
        for entry in fs::read_dir(&directory).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
            if !is_temporary_name(name) && parse_version_file_name(name).is_none() {
                return Err(SecretStoreError::corrupt());
            }
            let is_version = parse_version_file_name(name).is_some();
            if is_version {
                removed_versions = removed_versions
                    .checked_add(1)
                    .ok_or_else(SecretStoreError::corrupt)?;
            }
            if is_version {
                scrub_and_remove(&entry.path())?;
            } else {
                remove_temporary_file(&entry.path())?;
            }
        }
        fs::remove_dir(&directory).map_err(unavailable)?;
        sync_directory(&self.root)?;
        drop(lock);
        Ok(LocalSecretCleanupReceipt { removed_versions })
    }

    fn publish_version(
        &self,
        reference: &CredentialReferenceResolution,
        rotation_version: u64,
        secret: &ResolvedSecret,
    ) -> Result<LocalSecretWriteReceipt, SecretStoreError> {
        validate_secret(secret)?;
        let lock = self.acquire_lock()?;
        self.ensure_root()?;
        let directory = self.reference_directory(reference)?;
        create_secure_directory(&directory)?;
        remove_temporary_files_in(&directory)?;
        let target = directory.join(version_file_name(rotation_version));
        let replayed = publish_linked_secret(&directory, &target, secret.expose())?;
        sync_directory(&self.root)?;
        drop(lock);
        Ok(LocalSecretWriteReceipt {
            credential_reference_id: reference.credential_reference_id().clone(),
            rotation_version,
            replayed,
        })
    }

    fn acquire_lock(&self) -> Result<File, SecretStoreError> {
        self.ensure_root()?;
        ensure_regular_file_mode(&self.lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(unavailable)?;
        lock.lock().map_err(unavailable)?;
        Ok(lock)
    }

    fn ensure_root(&self) -> Result<(), SecretStoreError> {
        ensure_secure_directory(&self.root)
    }

    fn reference_directory(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<PathBuf, SecretStoreError> {
        let scope =
            serde_json::to_vec(reference.scope()).map_err(|_| SecretStoreError::corrupt())?;
        let mut digest = Sha256::new();
        digest.update(b"winwincode.local-secret-store.v1\0");
        digest.update(&scope);
        digest.update(b"\0");
        digest.update(reference.credential_reference_id().0.as_bytes());
        digest.update(b"\0");
        digest.update(reference.provider_id().as_bytes());
        Ok(self.root.join(format!("{:x}", digest.finalize())))
    }

    fn remove_orphaned_temporary_files(&self) -> Result<(), SecretStoreError> {
        for entry in fs::read_dir(&self.root).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name();
            if name == LOCK_FILE_NAME {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(unavailable)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SecretStoreError::corrupt());
            }
            ensure_secure_directory(&entry.path())?;
            remove_temporary_files_in(&entry.path())?;
        }
        Ok(())
    }
}

impl SecretStorePort for LocalSecretStoreAdapter {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        let lock = self.acquire_lock()?;
        self.ensure_root()?;
        let directory = self.reference_directory(reference)?;
        if !directory.try_exists().map_err(unavailable)? {
            return Err(SecretStoreError::missing());
        }
        ensure_secure_directory(&directory)?;
        let path = directory.join(version_file_name(reference.rotation_version()));
        let secret = read_secret(&path)?;
        drop(lock);
        ResolvedSecret::from_bytes(secret)
    }
}

fn create_secure_directory(path: &Path) -> Result<(), SecretStoreError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(DIRECTORY_MODE);
    builder.create(path).map_err(unavailable)?;
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SecretStoreError::corrupt());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE)).map_err(unavailable)?;
    ensure_secure_directory(path)
}

fn ensure_secure_directory(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn ensure_lock_file(path: &Path) -> Result<(), SecretStoreError> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
    {
        Ok(file) => file.sync_all().map_err(unavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(unavailable(error)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE)).map_err(unavailable)?;
    ensure_regular_file_mode(path)
}

fn ensure_regular_file_mode(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o177 != 0
    {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn validate_secret(secret: &ResolvedSecret) -> Result<(), SecretStoreError> {
    let length = u64::try_from(secret.expose().len()).map_err(|_| SecretStoreError::corrupt())?;
    if length == 0 || length > MAX_SECRET_BYTES {
        return Err(SecretStoreError::corrupt());
    }
    Ok(())
}

fn version_file_name(rotation_version: u64) -> String {
    format!("version-{rotation_version:020}.secret")
}

fn parse_version_file_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("version-")?.strip_suffix(".secret")?;
    (digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn is_temporary_name(name: &str) -> bool {
    let Some(value) = name
        .strip_prefix(TEMPORARY_PREFIX)
        .and_then(|value| value.strip_suffix(TEMPORARY_SUFFIX))
    else {
        return false;
    };
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn remove_temporary_files_in(directory: &Path) -> Result<(), SecretStoreError> {
    let mut changed = false;
    for entry in fs::read_dir(directory).map_err(unavailable)? {
        let entry = entry.map_err(unavailable)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(SecretStoreError::corrupt)?;
        if is_temporary_name(name) {
            remove_temporary_file(&entry.path())?;
            changed = true;
        }
    }
    if changed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn publish_linked_secret(
    directory: &Path,
    target: &Path,
    secret: &[u8],
) -> Result<bool, SecretStoreError> {
    let (temporary, mut file) = temporary_secret_file(directory)?;
    if let Err(error) = file
        .write_all(secret)
        .and_then(|()| file.sync_all())
        .map_err(unavailable)
    {
        drop(file);
        let _ = remove_temporary_file(&temporary);
        return Err(error);
    }
    drop(file);
    let replayed = match fs::hard_link(&temporary, target) {
        Ok(()) => false,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_secret(target)?;
            let matches = constant_time_eq(&existing, secret);
            let mut existing = existing;
            existing.fill(0);
            if !matches {
                let _ = remove_temporary_file(&temporary);
                return Err(SecretStoreError::version_conflict());
            }
            true
        }
        Err(error) => {
            let _ = remove_temporary_file(&temporary);
            return Err(unavailable(error));
        }
    };
    remove_temporary_file(&temporary)?;
    sync_directory(directory)?;
    Ok(replayed)
}

fn temporary_secret_file(directory: &Path) -> Result<(PathBuf, File), SecretStoreError> {
    for _ in 0..1_024 {
        let nonce = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{TEMPORARY_PREFIX}{}.{nonce}{TEMPORARY_SUFFIX}",
            std::process::id()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(unavailable(error)),
        }
    }
    Err(SecretStoreError::unavailable())
}

fn read_secret(path: &Path) -> Result<Vec<u8>, SecretStoreError> {
    let exists = path.try_exists().map_err(unavailable)?;
    if !exists {
        return Err(SecretStoreError::missing());
    }
    ensure_regular_file_mode(path)?;
    let mut file = File::open(path).map_err(unavailable)?;
    let metadata = file.metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SECRET_BYTES {
        return Err(SecretStoreError::corrupt());
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| SecretStoreError::corrupt())?,
    );
    file.read_to_end(&mut bytes).map_err(unavailable)?;
    if u64::try_from(bytes.len()).map_err(|_| SecretStoreError::corrupt())? != metadata.len() {
        bytes.fill(0);
        return Err(SecretStoreError::corrupt());
    }
    Ok(bytes)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn scrub_and_remove(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SecretStoreError::corrupt());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(unavailable)?;
    file.seek(SeekFrom::Start(0)).map_err(unavailable)?;
    let zeros = [0_u8; 8 * 1024];
    let mut remaining = metadata.len();
    while remaining > 0 {
        let count = remaining.min(zeros.len() as u64);
        file.write_all(&zeros[..usize::try_from(count).unwrap_or(zeros.len())])
            .map_err(unavailable)?;
        remaining -= count;
    }
    file.sync_all().map_err(unavailable)?;
    file.set_len(0).map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    drop(file);
    fs::remove_file(path).map_err(unavailable)
}

fn remove_temporary_file(path: &Path) -> Result<(), SecretStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SecretStoreError::corrupt());
    }
    if metadata.nlink() > 1 {
        fs::remove_file(path).map_err(unavailable)
    } else {
        scrub_and_remove(path)
    }
}

fn sync_directory(path: &Path) -> Result<(), SecretStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(unavailable)
}

fn unavailable(_error: std::io::Error) -> SecretStoreError {
    SecretStoreError::unavailable()
}
