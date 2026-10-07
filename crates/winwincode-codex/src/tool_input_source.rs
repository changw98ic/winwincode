// SPDX-License-Identifier: Apache-2.0

//! Content identity for the source files read by the benchmark public smoke.
//! Selection belongs to the trusted caller; timestamps only detect an unstable
//! read and never contribute to the returned identity.

use std::fs::{self, Metadata, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::store::AdapterStoreError;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 100;
const EXCLUDED_DIRECTORIES: [&str; 8] = [
    ".git",
    "node_modules",
    "target",
    "bin",
    "obj",
    "build",
    ".venv",
    "__pycache__",
];

pub(crate) fn source_digest(
    workspace: &Path,
    suffixes: &[String],
) -> Result<String, AdapterStoreError> {
    let mut snapshot = SourceSnapshot::default();
    snapshot.visit(workspace, workspace, suffixes)?;
    snapshot.files.sort_by(|left, right| left.0.cmp(&right.0));
    let files = snapshot
        .files
        .into_iter()
        .map(|(path, digest)| (path, format!("{digest:x}")))
        .collect::<Vec<_>>();
    // Match Python json.dumps(..., separators=(',', ':')) with ensure_ascii.
    // UTF-16 escapes also preserve supplementary Unicode path characters.
    let json = serde_json::to_string(&files).map_err(|_| AdapterStoreError::Corrupt)?;
    let mut encoded = String::new();
    for character in json.chars() {
        if character.is_ascii() && character != '\u{7f}' {
            encoded.push(character);
        } else {
            use std::fmt::Write as _;
            for unit in character.encode_utf16(&mut [0; 2]) {
                write!(&mut encoded, "\\u{unit:04x}").map_err(|_| AdapterStoreError::Corrupt)?;
            }
        }
    }
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}

#[derive(Default)]
struct SourceSnapshot {
    files: Vec<(String, sha2::digest::Output<Sha256>)>,
    bytes: u64,
}

impl SourceSnapshot {
    fn visit(
        &mut self,
        root: &Path,
        directory: &Path,
        suffixes: &[String],
    ) -> Result<(), AdapterStoreError> {
        let before = checked_metadata(directory)?;
        if !before.is_dir() {
            return Err(AdapterStoreError::Corrupt);
        }
        let entries = fs::read_dir(directory)
            .map_err(|_| AdapterStoreError::Unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        for entry in entries {
            let name = entry.file_name();
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|_| AdapterStoreError::Unavailable)?;
            // The public smoke ignores these directories without traversing
            // them. Their contents cannot change a source request's identity.
            if name
                .to_str()
                .is_some_and(|name| EXCLUDED_DIRECTORIES.contains(&name))
                && (metadata.is_dir()
                    || (metadata.file_type().is_symlink()
                        && fs::metadata(&path).is_ok_and(|target| target.is_dir())))
            {
                continue;
            }
            if metadata.file_type().is_symlink() {
                return Err(AdapterStoreError::Corrupt);
            }
            if metadata.is_dir() {
                self.visit(root, &path, suffixes)?;
            } else if selected_source(&path, suffixes) {
                if self.files.len() >= MAX_SOURCE_FILES || !metadata.is_file() {
                    return Err(AdapterStoreError::Corrupt);
                }
                let content = read_source_file(&path, MAX_SOURCE_BYTES - self.bytes)?;
                self.bytes +=
                    u64::try_from(content.len()).map_err(|_| AdapterStoreError::Corrupt)?;
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| AdapterStoreError::Corrupt)?
                    .to_str()
                    .ok_or(AdapterStoreError::Corrupt)?
                    .to_owned();
                self.files.push((relative, Sha256::digest(content)));
            }
        }
        let after = checked_metadata(directory)?;
        // Logs or caches may appear during a read without changing any
        // selected source. Only replacement of the directory matters here.
        if !after.is_dir() || before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }
}

fn selected_source(path: &Path, suffixes: &[String]) -> bool {
    path.extension()
        .and_then(|suffix| suffix.to_str())
        .is_some_and(|suffix| {
            suffixes
                .iter()
                .any(|allowed| allowed == &format!(".{suffix}"))
        })
}

fn checked_metadata(path: &Path) -> Result<Metadata, AdapterStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| AdapterStoreError::Unavailable)?;
    if metadata.file_type().is_symlink() {
        return Err(AdapterStoreError::Corrupt);
    }
    Ok(metadata)
}

pub(crate) fn read_source_file(path: &Path, remaining: u64) -> Result<Vec<u8>, AdapterStoreError> {
    let before = checked_metadata(path)?;
    if !before.is_file() || before.len() > remaining {
        return Err(AdapterStoreError::Corrupt);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(if cfg!(target_os = "macos") {
        // Darwin O_NOFOLLOW | O_NONBLOCK.
        0x100 | 0x4
    } else {
        // Linux O_NOFOLLOW | O_NONBLOCK. Release targets are Darwin and Linux.
        0x20_000 | 0x800
    });
    let file = options
        .open(path)
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let opened = file
        .metadata()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    if !opened.is_file() || !same_metadata(&before, &opened) {
        return Err(AdapterStoreError::Conflict);
    }
    let mut content = Vec::new();
    (&file)
        .take(remaining + 1)
        .read_to_end(&mut content)
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let after = file
        .metadata()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let length = u64::try_from(content.len()).map_err(|_| AdapterStoreError::Corrupt)?;
    if length > remaining {
        return Err(AdapterStoreError::Corrupt);
    }
    if length != opened.len()
        || !same_metadata(&opened, &after)
        || !same_metadata(&after, &checked_metadata(path)?)
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(content)
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::*;

    struct Checkout(PathBuf);

    impl Checkout {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("wwc-src-{}", uuid::Uuid::now_v7().simple()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn digest(&self) -> String {
            source_digest(&self.0, &[".py".to_owned()]).unwrap()
        }
    }

    impl Drop for Checkout {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn source_changes_and_restores_while_timestamps_do_not_change_identity() {
        let checkout = Checkout::new();
        let source = checkout.0.join("main.py");
        fs::write(&source, "print(1)").unwrap();
        let original = checkout.digest();
        fs::write(&source, "print(2)").unwrap();
        assert_ne!(checkout.digest(), original);
        fs::write(&source, "print(1)").unwrap();
        assert_eq!(checkout.digest(), original);
        File::open(&source)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(5)),
            )
            .unwrap();
        assert_eq!(checkout.digest(), original);
        let copy = Checkout::new();
        fs::write(copy.0.join("main.py"), "print(1)").unwrap();
        assert_eq!(copy.digest(), original);
    }

    #[test]
    fn only_selected_source_contributes_and_build_caches_are_excluded() {
        let checkout = Checkout::new();
        fs::write(checkout.0.join("main.py"), "print(1)").unwrap();
        let original = checkout.digest();
        for directory in EXCLUDED_DIRECTORIES {
            let directory = checkout.0.join(directory);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("generated.py"), "build output").unwrap();
            symlink(checkout.0.join("main.py"), directory.join("cached.py")).unwrap();
        }
        for name in ["README.md", "run.log", "main.rs"] {
            fs::write(checkout.0.join(name), "irrelevant").unwrap();
        }
        assert_eq!(checkout.digest(), original);
        fs::write(checkout.0.join("run.log"), "next run").unwrap();
        assert_eq!(checkout.digest(), original);
        assert_ne!(
            source_digest(&checkout.0, &[".py".to_owned(), ".rs".to_owned()]).unwrap(),
            original
        );
    }

    #[test]
    fn excluded_cache_directory_symlinks_are_ignored_but_file_symlinks_are_rejected() {
        let checkout = Checkout::new();
        let cache = Checkout::new();
        fs::write(checkout.0.join("main.py"), "print(1)").unwrap();
        fs::write(cache.0.join("generated.py"), "irrelevant").unwrap();
        let original = checkout.digest();
        for directory in EXCLUDED_DIRECTORIES {
            symlink(&cache.0, checkout.0.join(directory)).unwrap();
        }
        assert_eq!(checkout.digest(), original);
        let target = checkout.0.join("target");
        fs::remove_file(&target).unwrap();
        symlink(cache.0.join("generated.py"), &target).unwrap();
        assert_eq!(
            source_digest(&checkout.0, &[".py".to_owned()]),
            Err(AdapterStoreError::Corrupt)
        );
    }

    #[test]
    fn source_paths_additions_and_removals_contribute_with_stable_ordering() {
        let checkout = Checkout::new();
        let first = checkout.0.join("main.py");
        let renamed = checkout.0.join("renamed.py");
        fs::write(&first, "value = 1").unwrap();
        let original = checkout.digest();
        fs::rename(&first, &renamed).unwrap();
        assert_ne!(checkout.digest(), original);
        fs::rename(&renamed, &first).unwrap();
        assert_eq!(checkout.digest(), original);
        fs::create_dir(checkout.0.join("nested")).unwrap();
        let added = checkout.0.join("nested/模块.py");
        fs::write(&added, "value = '中文'").unwrap();
        let expanded = checkout.digest();
        assert_ne!(expanded, original);
        fs::remove_file(&first).unwrap();
        fs::write(&first, "value = 1").unwrap();
        assert_eq!(checkout.digest(), expanded);
        fs::remove_file(added).unwrap();
        assert_eq!(checkout.digest(), original);
    }

    #[test]
    fn rejects_symlink_files_directories_and_non_regular_sources() {
        let checkout = Checkout::new();
        fs::write(checkout.0.join("main.py"), "print(1)").unwrap();
        for name in ["link.py", "link.txt", "link-directory"] {
            let path = checkout.0.join(name);
            symlink(&checkout.0, &path).unwrap();
            assert_eq!(
                source_digest(&checkout.0, &[".py".to_owned()]),
                Err(AdapterStoreError::Corrupt)
            );
            fs::remove_file(path).unwrap();
        }
        let socket_path = checkout.0.join("socket.py");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        assert_eq!(
            source_digest(&checkout.0, &[".py".to_owned()]),
            Err(AdapterStoreError::Corrupt)
        );
    }

    #[test]
    fn enforces_source_byte_and_file_count_limits() {
        let checkout = Checkout::new();
        let source = checkout.0.join("main.py");
        let file = File::create(&source).unwrap();
        file.set_len(MAX_SOURCE_BYTES).unwrap();
        assert!(source_digest(&checkout.0, &[".py".to_owned()]).is_ok());
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        assert_eq!(
            source_digest(&checkout.0, &[".py".to_owned()]),
            Err(AdapterStoreError::Corrupt)
        );
        file.set_len(0).unwrap();
        for index in 1..MAX_SOURCE_FILES {
            fs::write(checkout.0.join(format!("source-{index}.py")), "").unwrap();
        }
        assert!(source_digest(&checkout.0, &[".py".to_owned()]).is_ok());
        fs::write(checkout.0.join("overflow.py"), "").unwrap();
        assert_eq!(
            source_digest(&checkout.0, &[".py".to_owned()]),
            Err(AdapterStoreError::Corrupt)
        );
    }

    #[test]
    fn metadata_validation_detects_content_changes_and_file_replacement() {
        let checkout = Checkout::new();
        let source = checkout.0.join("main.py");
        fs::write(&source, "original").unwrap();
        let before = checked_metadata(&source).unwrap();
        fs::write(&source, "modified").unwrap();
        assert!(!same_metadata(&before, &checked_metadata(&source).unwrap()));
        let opened = File::open(&source).unwrap();
        fs::rename(&source, checkout.0.join("previous.py")).unwrap();
        fs::write(&source, "modified").unwrap();
        assert!(!same_metadata(
            &opened.metadata().unwrap(),
            &checked_metadata(&source).unwrap()
        ));
    }
}

#[cfg(test)]
#[path = "tool_input_source_tests.rs"]
mod contract_tests;
