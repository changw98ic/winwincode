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
            if root.join("blobs").exists() && fs::read_dir(root.join("blobs"))?.next().is_some() {
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
}
