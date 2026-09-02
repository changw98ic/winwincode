// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::archive::ARCHIVE_MAGIC;
use crate::export::lowercase_hex;
use crate::model::{EvidenceError, EvidenceManifest, MANIFEST_FILE_NAME};
use crate::secret::{SecretScanner, reject_secret_bytes};

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Verify every manifest binding and reject unlisted package files.
///
/// # Errors
///
/// Returns corruption, secret-detection, or I/O errors for any mismatch.
pub fn verify_evidence_package(package: &Path) -> Result<EvidenceManifest, EvidenceError> {
    let manifest_path = package.join(MANIFEST_FILE_NAME);
    if fs::metadata(&manifest_path)
        .map_err(|error| EvidenceError::io("read evidence manifest metadata", &error))?
        .len()
        > MAX_MANIFEST_BYTES
    {
        return Err(EvidenceError::corrupt("evidence manifest is too large"));
    }
    let manifest_bytes = fs::read(manifest_path)
        .map_err(|error| EvidenceError::io("read evidence manifest", &error))?;
    reject_secret_bytes(MANIFEST_FILE_NAME, &manifest_bytes)?;
    let manifest: EvidenceManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| EvidenceError::corrupt(format!("parse evidence manifest: {error}")))?;
    validate_manifest(&manifest)?;

    let mut expected: BTreeSet<String> = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    expected.insert(MANIFEST_FILE_NAME.to_owned());
    let actual = collect_relative_files(package)?;
    if actual != expected {
        return Err(EvidenceError::corrupt(
            "package file set does not match manifest",
        ));
    }
    for binding in &manifest.files {
        verify_file(
            &package.join(&binding.path),
            binding.byte_length,
            &binding.sha256,
        )?;
    }
    Ok(manifest)
}

/// Verify the deterministic archive without extracting it.
///
/// # Errors
///
/// Returns corruption, secret-detection, or I/O errors for malformed headers,
/// duplicate paths, content mismatches, or an incomplete manifest.
pub fn verify_evidence_archive(archive: &Path) -> Result<EvidenceManifest, EvidenceError> {
    let mut file =
        File::open(archive).map_err(|error| EvidenceError::io("open evidence archive", &error))?;
    let mut magic = vec![0_u8; ARCHIVE_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|error| EvidenceError::io("read evidence archive magic", &error))?;
    if magic != ARCHIVE_MAGIC {
        return Err(EvidenceError::corrupt("invalid evidence archive magic"));
    }

    let mut entries = BTreeMap::new();
    let mut manifest = None;
    loop {
        let path_length = read_u32(&mut file)?;
        if path_length == 0 {
            break;
        }
        if path_length > 4096 {
            return Err(EvidenceError::corrupt("archive path is too long"));
        }
        let mut path_bytes = vec![0_u8; path_length as usize];
        file.read_exact(&mut path_bytes)
            .map_err(|error| EvidenceError::io("read archive path", &error))?;
        let path = String::from_utf8(path_bytes)
            .map_err(|_| EvidenceError::corrupt("archive path is not UTF-8"))?;
        validate_archive_path(&path)?;
        let byte_length = read_u64(&mut file)?;
        if path == MANIFEST_FILE_NAME && byte_length > MAX_MANIFEST_BYTES {
            return Err(EvidenceError::corrupt("archive manifest is too large"));
        }
        let (sha256, bytes) = read_entry(&mut file, byte_length, path == MANIFEST_FILE_NAME)?;
        if entries
            .insert(path.clone(), (byte_length, sha256))
            .is_some()
        {
            return Err(EvidenceError::corrupt("duplicate archive path"));
        }
        if path == MANIFEST_FILE_NAME {
            let content = bytes.ok_or_else(|| EvidenceError::corrupt("missing manifest bytes"))?;
            reject_secret_bytes(MANIFEST_FILE_NAME, &content)?;
            manifest = Some(serde_json::from_slice(&content).map_err(|error| {
                EvidenceError::corrupt(format!("parse archive manifest: {error}"))
            })?);
        }
    }
    if file
        .stream_position()
        .map_err(|error| EvidenceError::io("read archive position", &error))?
        != file
            .metadata()
            .map_err(|error| EvidenceError::io("read archive metadata", &error))?
            .len()
    {
        return Err(EvidenceError::corrupt("archive has trailing bytes"));
    }
    let manifest: EvidenceManifest =
        manifest.ok_or_else(|| EvidenceError::corrupt("archive has no manifest"))?;
    validate_manifest(&manifest)?;
    verify_archive_bindings(&entries, &manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &EvidenceManifest) -> Result<(), EvidenceError> {
    if manifest.schema_version != 1
        || !manifest.stable_bytes
        || !manifest.non_deterministic_fields.is_empty()
    {
        return Err(EvidenceError::corrupt(
            "unsupported or non-deterministic evidence manifest",
        ));
    }
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        validate_archive_path(&file.path)?;
        if file.path == MANIFEST_FILE_NAME || !paths.insert(file.path.as_str()) {
            return Err(EvidenceError::corrupt(
                "manifest contains duplicate or reserved path",
            ));
        }
    }
    Ok(())
}

fn verify_file(
    path: &Path,
    expected_bytes: u64,
    expected_digest: &str,
) -> Result<(), EvidenceError> {
    let mut file =
        File::open(path).map_err(|error| EvidenceError::io("read evidence file", &error))?;
    let mut byte_length = 0_u64;
    let mut hash = Sha256::new();
    let mut scanner = SecretScanner::new(path.display().to_string());
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| EvidenceError::io("read evidence file", &error))?;
        if count == 0 {
            break;
        }
        scanner.inspect(&buffer[..count])?;
        hash.update(&buffer[..count]);
        byte_length = byte_length
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| EvidenceError::corrupt("evidence file byte count overflow"))?;
    }
    if byte_length != expected_bytes || lowercase_hex(&hash.finalize()) != expected_digest {
        return Err(EvidenceError::corrupt(format!(
            "evidence binding mismatch for {}",
            path.display()
        )));
    }
    Ok(())
}

fn collect_relative_files(root: &Path) -> Result<BTreeSet<String>, EvidenceError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| EvidenceError::io("read evidence directory", &error))?
        {
            let entry = entry.map_err(|error| EvidenceError::io("read evidence entry", &error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| EvidenceError::io("read evidence file type", &error))?;
            if file_type.is_symlink() {
                return Err(EvidenceError::corrupt(
                    "evidence package contains a symlink",
                ));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| EvidenceError::corrupt("evidence path escaped package"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                validate_archive_path(&relative)?;
                files.insert(relative);
            } else {
                return Err(EvidenceError::corrupt(
                    "evidence package contains a special file",
                ));
            }
        }
    }
    Ok(files)
}

fn verify_archive_bindings(
    entries: &BTreeMap<String, (u64, String)>,
    manifest: &EvidenceManifest,
) -> Result<(), EvidenceError> {
    if entries.len() != manifest.files.len() + 1 || !entries.contains_key(MANIFEST_FILE_NAME) {
        return Err(EvidenceError::corrupt(
            "archive file set does not match manifest",
        ));
    }
    for binding in &manifest.files {
        if entries.get(&binding.path) != Some(&(binding.byte_length, binding.sha256.clone())) {
            return Err(EvidenceError::corrupt(format!(
                "archive binding mismatch for {}",
                binding.path
            )));
        }
    }
    Ok(())
}

fn validate_archive_path(path: &str) -> Result<(), EvidenceError> {
    let parsed = Path::new(path);
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || parsed
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(EvidenceError::corrupt("unsafe evidence path"));
    }
    Ok(())
}

fn read_entry(
    file: &mut File,
    byte_length: u64,
    retain: bool,
) -> Result<(String, Option<Vec<u8>>), EvidenceError> {
    let mut remaining = byte_length;
    let mut hash = Sha256::new();
    let mut retained = retain.then(Vec::new);
    let mut scanner = SecretScanner::new("archive entry");
    let mut buffer = vec![0_u8; 64 * 1024];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        file.read_exact(&mut buffer[..requested])
            .map_err(|error| EvidenceError::io("read archive entry", &error))?;
        hash.update(&buffer[..requested]);
        scanner.inspect(&buffer[..requested])?;
        if let Some(bytes) = &mut retained {
            bytes.extend_from_slice(&buffer[..requested]);
        }
        remaining -= u64::try_from(requested).unwrap_or(remaining);
    }
    Ok((lowercase_hex(&hash.finalize()), retained))
}

fn read_u32(file: &mut File) -> Result<u32, EvidenceError> {
    let mut bytes = [0_u8; 4];
    file.read_exact(&mut bytes)
        .map_err(|error| EvidenceError::io("read archive u32", &error))?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(file: &mut File) -> Result<u64, EvidenceError> {
    let mut bytes = [0_u8; 8];
    file.read_exact(&mut bytes)
        .map_err(|error| EvidenceError::io("read archive u64", &error))?;
    Ok(u64::from_be_bytes(bytes))
}
