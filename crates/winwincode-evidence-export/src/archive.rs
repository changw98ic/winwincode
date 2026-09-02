// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::model::{EvidenceError, EvidenceManifest, MANIFEST_FILE_NAME};

pub(crate) const ARCHIVE_MAGIC: &[u8] = b"WWCEVIDENCE\x01";

pub(crate) fn write_archive(
    package: &Path,
    manifest: &EvidenceManifest,
    destination: &Path,
) -> Result<(), EvidenceError> {
    let mut entries: Vec<(&str, u64)> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.byte_length))
        .collect();
    let manifest_length = std::fs::metadata(package.join(MANIFEST_FILE_NAME))
        .map_err(|error| EvidenceError::io("read manifest metadata", &error))?
        .len();
    entries.push((MANIFEST_FILE_NAME, manifest_length));
    entries.sort_by(|left, right| left.0.cmp(right.0));

    let mut archive = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| EvidenceError::io("create evidence archive", &error))?;
    archive
        .write_all(ARCHIVE_MAGIC)
        .map_err(|error| EvidenceError::io("write archive magic", &error))?;
    for (path, byte_length) in entries {
        let path_length = u32::try_from(path.len())
            .map_err(|_| EvidenceError::invalid("archive path exceeds u32"))?;
        archive
            .write_all(&path_length.to_be_bytes())
            .and_then(|()| archive.write_all(path.as_bytes()))
            .and_then(|()| archive.write_all(&byte_length.to_be_bytes()))
            .map_err(|error| EvidenceError::io("write archive entry header", &error))?;
        copy_exact(&package.join(path), byte_length, &mut archive)?;
    }
    archive
        .write_all(&0_u32.to_be_bytes())
        .and_then(|()| archive.sync_all())
        .map_err(|error| EvidenceError::io("finish evidence archive", &error))
}

fn copy_exact(source: &Path, expected: u64, output: &mut File) -> Result<(), EvidenceError> {
    let mut input =
        File::open(source).map_err(|error| EvidenceError::io("open archive input", &error))?;
    let copied = std::io::copy(&mut input, output)
        .map_err(|error| EvidenceError::io("copy archive input", &error))?;
    if copied != expected {
        return Err(EvidenceError::corrupt(format!(
            "archive input {} changed length",
            source.display()
        )));
    }
    let mut trailing = [0_u8; 1];
    if input
        .read(&mut trailing)
        .map_err(|error| EvidenceError::io("check archive input length", &error))?
        != 0
    {
        return Err(EvidenceError::corrupt("archive input grew during copy"));
    }
    Ok(())
}
