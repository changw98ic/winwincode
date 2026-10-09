// SPDX-License-Identifier: Apache-2.0

//! Immutable encrypted launch material retained across Server restarts.
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Private encrypted material; callers retain only secret-free identities in `SQLite`.
pub struct PrivateLaunchMaterialStore {
    root: PathBuf,
    key: [u8; 32],
}
impl PrivateLaunchMaterialStore {
    /// Opens the durable private key and encrypted blob directory.
    /// # Errors
    /// Returns a secret-free failure for invalid permissions or storage.
    pub fn open(directory: &Path) -> io::Result<Self> {
        let root = directory.join("private-launch-material");
        fs::create_dir_all(&root)?;
        if !fs::symlink_metadata(&root)?.is_dir() {
            return Err(invalid());
        }
        fs::File::open(directory)?.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let key_path = root.join("launch-material.key");
        if !key_path.exists() {
            #[cfg(test)]
            tests::run_after_missing_key_hook();
            // A concurrent initializer can publish its key and first blob
            // after the missing-key observation above.
            if root.join("blobs").exists()
                && fs::read_dir(root.join("blobs"))?.next().is_some()
                && !key_path.exists()
            {
                return Err(invalid());
            }
            let mut key = [0u8; 32];
            getrandom::fill(&mut key).map_err(|_| invalid())?;
            atomic_private_file(&root, &key_path, &key)?;
        }
        let key_bytes = read_private(&key_path, 32)?;
        let key: [u8; 32] = key_bytes.try_into().map_err(|_| invalid())?;
        let blobs = root.join("blobs");
        fs::create_dir_all(&blobs)?;
        if !fs::symlink_metadata(&blobs)?.is_dir() {
            return Err(invalid());
        }
        fs::File::open(&root)?.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&blobs, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root: blobs, key })
    }
    /// Retains exact material before its public launch is committed.
    /// # Errors
    /// Rejects changed replay material, invalid identity or storage failures.
    pub fn put(&self, id: &str, plain: &[u8]) -> io::Result<()> {
        let path = self.path(id)?;
        if plain.len() > 4096 {
            return Err(invalid());
        }
        if path.exists() {
            return if self.get(id)? == plain {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| invalid())?;
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|_| invalid())?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| invalid())?;
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        atomic_private_file(&self.root, &path, &bytes)?;
        if self.get(id)? != plain {
            return Err(invalid());
        }
        Ok(())
    }
    /// Loads the exact authenticated private launch material.
    /// # Errors
    /// Rejects corruption, changed binding, invalid permissions or missing blobs.
    pub fn get(&self, id: &str) -> io::Result<Vec<u8>> {
        let bytes = read_private(&self.path(id)?, 4124)?;
        if bytes.len() < 28 {
            return Err(invalid());
        }
        Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| invalid())?
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| invalid())
    }
    fn path(&self, id: &str) -> io::Result<PathBuf> {
        if id.len() != 30
            || !id.starts_with("wlg_")
            || !id[4..].bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(invalid());
        }
        Ok(self.root.join(id))
    }
}
fn invalid() -> io::Error {
    io::Error::other("private launch material unavailable")
}
fn read_private(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > u64::try_from(max).map_err(|_| invalid())? {
        return Err(invalid());
    }
    #[cfg(unix)]
    if meta.permissions().mode() & 0o777 != 0o600 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(u64::try_from(max + 1).map_err(|_| invalid())?)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(invalid());
    }
    Ok(bytes)
}
fn atomic_private_file(root: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| invalid())?;
    let name = random
        .iter()
        .fold(String::with_capacity(32), |mut value, byte| {
            use std::fmt::Write as _;
            write!(value, "{byte:02x}").expect("writing to a String cannot fail");
            value
        });
    let temporary = root.join(format!(".write-{name}"));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        fs::File::open(root)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn material_is_private_restart_safe_immutable_and_bound() {
        let root = std::env::temp_dir().join(format!("launch-material-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let id = "wlg_00000000000000000000000001";
        let store = PrivateLaunchMaterialStore::open(&root).unwrap();
        store.put(id, b"private-token").unwrap();
        store.put(id, b"private-token").unwrap();
        assert!(store.put(id, b"changed").is_err());
        drop(store);
        let store = PrivateLaunchMaterialStore::open(&root).unwrap();
        assert_eq!(store.get(id).unwrap(), b"private-token");
        let path = store.path(id).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.windows(13).any(|b| b == b"private-token"));
        let other = "wlg_00000000000000000000000002";
        fs::copy(&path, store.path(other).unwrap()).unwrap();
        assert!(store.get(other).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    std::thread_local! {
        static AFTER_MISSING_KEY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn run_after_missing_key_hook() {
        let hook = AFTER_MISSING_KEY.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    struct MissingKeyHookGuard;

    impl MissingKeyHookGuard {
        fn install(hook: impl FnOnce() + 'static) -> Self {
            AFTER_MISSING_KEY.with(|slot| {
                let mut slot = slot.borrow_mut();
                assert!(slot.is_none(), "missing-key hook already installed");
                *slot = Some(Box::new(hook));
            });
            Self
        }
    }

    impl Drop for MissingKeyHookGuard {
        fn drop(&mut self) {
            AFTER_MISSING_KEY.with(|slot| {
                let _ = slot.borrow_mut().take();
            });
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).expect("test directory identity");
            let root = std::env::temp_dir().join(format!(
                "launch-material-{label}-{}-{:032x}",
                std::process::id(),
                u128::from_ne_bytes(random)
            ));
            fs::create_dir(&root).expect("unique test directory");
            Self(root)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cold_open_accepts_concurrent_key_and_first_blob() {
        let directory = TestDirectory::new("concurrent");
        let id = "wlg_00000000000000000000000001";
        let plain = b"public-test-fixture";
        let timeout = std::time::Duration::from_secs(10);
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let suspended_root = directory.0.clone();
        std::thread::scope(|scope| {
            let pending = scope.spawn(move || {
                let _hook = MissingKeyHookGuard::install(move || {
                    observed_tx.send(()).expect("missing-key observation");
                    resume_rx.recv_timeout(timeout).expect("resume cold open");
                });
                PrivateLaunchMaterialStore::open(&suspended_root)
            });
            observed_rx
                .recv_timeout(timeout)
                .expect("cold open reached actual missing-key branch");
            let winner = PrivateLaunchMaterialStore::open(&directory.0)
                .expect("concurrent initializer publishes its key");
            winner.put(id, plain).expect("publish first real blob");
            assert_eq!(winner.get(id).expect("winner reads its blob"), plain);
            let key_path = directory
                .0
                .join("private-launch-material/launch-material.key");
            let key_before = fs::read(&key_path).expect("published winning key");
            assert_eq!(key_before.len(), 32);
            #[cfg(unix)]
            assert_eq!(
                fs::symlink_metadata(&key_path)
                    .expect("winning key metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            let blob_path = winner.path(id).expect("retained blob path");
            let blob_before = fs::read(&blob_path).expect("retained encrypted blob");
            resume_tx.send(()).expect("release suspended cold open");
            let resumed = pending
                .join()
                .expect("cold open thread")
                .expect("valid concurrent key and first blob must remain available");
            assert_eq!(resumed.get(id).expect("resumed open decrypts blob"), plain);
            assert!(key_before == fs::read(&key_path).expect("winning key remains"));
            assert!(blob_before == fs::read(&blob_path).expect("encrypted blob remains"));
            let restarted = PrivateLaunchMaterialStore::open(&directory.0)
                .expect("fresh reopen after the interleaving");
            assert_eq!(restarted.get(id).expect("restart decrypts blob"), plain);
        });
    }

    #[test]
    fn cold_open_rejects_existing_blob_without_key() {
        let directory = TestDirectory::new("missing-key");
        let id = "wlg_00000000000000000000000001";
        let store = PrivateLaunchMaterialStore::open(&directory.0).expect("initial store");
        store
            .put(id, b"public-test-fixture")
            .expect("real retained blob");
        let blob_path = store.path(id).expect("retained blob path");
        let blob_before = fs::read(&blob_path).expect("retained encrypted blob");
        drop(store);
        let key_path = directory
            .0
            .join("private-launch-material/launch-material.key");
        fs::remove_file(&key_path).expect("simulate missing retained key");
        assert!(PrivateLaunchMaterialStore::open(&directory.0).is_err());
        assert!(
            !key_path.exists(),
            "retained blobs must not cause key rotation"
        );
        assert!(blob_before == fs::read(&blob_path).expect("retained blob remains"));
    }

    #[test]
    fn cold_open_rejects_concurrent_malformed_key_without_rotation() {
        for length in [0, 31, 33] {
            let directory = TestDirectory::new("malformed-key");
            let concurrent_root = directory.0.clone();
            let malformed = vec![0xA5; length];
            let published = malformed.clone();
            let _hook = MissingKeyHookGuard::install(move || {
                let store = PrivateLaunchMaterialStore::open(&concurrent_root)
                    .expect("concurrent initializer");
                store
                    .put("wlg_00000000000000000000000001", b"public-test-fixture")
                    .expect("real retained blob");
                fs::write(
                    concurrent_root.join("private-launch-material/launch-material.key"),
                    published,
                )
                .expect("simulate malformed concurrent key");
            });
            assert!(PrivateLaunchMaterialStore::open(&directory.0).is_err());
            let retained = fs::read(
                directory
                    .0
                    .join("private-launch-material/launch-material.key"),
            )
            .expect("malformed key must remain unchanged");
            assert!(retained == malformed, "malformed key must not be rotated");
        }
    }
}
