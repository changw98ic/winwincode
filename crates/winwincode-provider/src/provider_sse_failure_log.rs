// SPDX-License-Identifier: Apache-2.0

//! Private raw response logs, separate from secret-free runtime diagnostics.

use std::{
    fs,
    io::{self, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::Path,
};

use sha2::{Digest as _, Sha256};

/// The first line is JSON metadata; all remaining bytes are the original body.
/// No response text is decoded, normalized or printed to the process log.
pub(crate) fn retain(
    directory: &Path,
    metadata: &serde_json::Value,
    body: &[u8],
) -> io::Result<String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let permissions = fs::symlink_metadata(directory)?;
    if !permissions.is_dir() || permissions.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let directory = fs::canonicalize(directory)?;
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let id = format!("sse-{:x}", Sha256::digest(nonce));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(format!("{id}.log")))?;
    serde_json::to_writer(&mut file, metadata)?;
    file.write_all(b"\n")?;
    file.write_all(body)?;
    file.sync_all()?;
    fs::File::open(directory)?.sync_all()?;
    Ok(id)
}
