// SPDX-License-Identifier: Apache-2.0

//! Worker-private, replay-idempotent storage for raw validation command output.

use std::{
    fs::{self, File, OpenOptions},
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ArtifactId, Sha256Digest};
use winwincode_execution_port::{
    change_batch_identity::validate_change_batch_identity_derivation, generated::ArtifactReference,
};

use crate::workspace_runtime::{
    ValidationArtifactError, ValidationArtifactPort, ValidationArtifactRequest,
    ValidationArtifactStream,
};

const DATABASE_FILE: &str = "validation-artifacts.sqlite3";
const BLOB_DIRECTORY: &str = "blobs";
const MAX_ARTIFACT_BYTES: usize = 16_777_216;
const ARTIFACT_ID_DOMAIN: &[u8] = b"winwincode.validation-artifact-id.v1";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Authority and exact reference required to consume one retained validation stream.
#[derive(Clone, Copy, Debug)]
pub struct ValidationArtifactReadRequest<'request> {
    pub identity: &'request winwincode_execution_port::generated::ChangeBatchIdentity,
    pub command_ordinal: usize,
    pub command_id: &'request str,
    pub stream: ValidationArtifactStream,
    pub reference: &'request ArtifactReference,
}

/// Production private Artifact store for bounded validation stdout/stderr.
#[derive(Debug)]
pub struct DurableValidationArtifactStore {
    connection: Connection,
    blob_root: PathBuf,
}

impl DurableValidationArtifactStore {
    /// Opens the private store and its single canonical schema.
    ///
    /// # Errors
    ///
    /// Rejects unsafe paths, unsupported schemas, and unavailable durable storage.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ValidationArtifactError> {
        let root = root.into();
        ensure_private_directory(&root)?;
        let blob_root = root.join(BLOB_DIRECTORY);
        ensure_private_directory(&blob_root)?;
        remove_stale_temporary_blobs(&blob_root)?;
        let database = root.join(DATABASE_FILE);
        ensure_private_file(&database)?;
        let connection = Connection::open(database).map_err(|_| ValidationArtifactError)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|_| ValidationArtifactError)?;
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|_| ValidationArtifactError)?;
        match version {
            0 => connection
                .execute_batch(
                    "CREATE TABLE validation_artifact (
                   batch_id TEXT NOT NULL,
                   command_ordinal INTEGER NOT NULL,
                   command_id TEXT NOT NULL,
                   stream TEXT NOT NULL,
                   identity_json BLOB NOT NULL,
                   media_type TEXT NOT NULL,
                   artifact_id TEXT NOT NULL UNIQUE,
                   content_digest TEXT NOT NULL,
                   PRIMARY KEY (batch_id, command_ordinal, command_id, stream)
                 );
                 PRAGMA user_version = 1;",
                )
                .map_err(|_| ValidationArtifactError)?,
            1 if validation_artifact_schema_is_current(&connection)? => {}
            _ => return Err(ValidationArtifactError),
        }
        Ok(Self {
            connection,
            blob_root,
        })
    }

    /// Reads one exact stream after revalidating its full batch authority and reference.
    ///
    /// # Errors
    ///
    /// Rejects foreign authority, changed references, missing blobs, and digest drift.
    pub fn read(
        &self,
        request: ValidationArtifactReadRequest<'_>,
    ) -> Result<Vec<u8>, ValidationArtifactError> {
        if validate_change_batch_identity_derivation(request.identity).is_err()
            || request.command_ordinal >= 64
            || request.command_id.is_empty()
        {
            return Err(ValidationArtifactError);
        }
        let identity_json =
            serde_json::to_vec(request.identity).map_err(|_| ValidationArtifactError)?;
        let ordinal =
            i64::try_from(request.command_ordinal).map_err(|_| ValidationArtifactError)?;
        let retained = self
            .connection
            .query_row(
                "SELECT identity_json, artifact_id, content_digest
                 FROM validation_artifact
                 WHERE batch_id = ?1 AND command_ordinal = ?2
                   AND command_id = ?3 AND stream = ?4",
                params![
                    request.identity.batch_id.0,
                    ordinal,
                    request.command_id,
                    stream_text(request.stream)
                ],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ValidationArtifactError)?
            .ok_or(ValidationArtifactError)?;
        if retained.0 != identity_json
            || retained.1 != request.reference.artifact_id.0
            || retained.2 != request.reference.digest.0
        {
            return Err(ValidationArtifactError);
        }
        let bytes = fs::read(blob_path(&self.blob_root, &request.reference.digest)?)
            .map_err(|_| ValidationArtifactError)?;
        if Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))) != request.reference.digest
        {
            return Err(ValidationArtifactError);
        }
        Ok(bytes)
    }

    fn persist_inner(
        &mut self,
        request: ValidationArtifactRequest<'_>,
    ) -> Result<ArtifactReference, ValidationArtifactError> {
        validate_request(&request)?;
        let identity_json =
            serde_json::to_vec(request.identity).map_err(|_| ValidationArtifactError)?;
        let stream = stream_text(request.stream);
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(request.bytes)));
        let artifact_id = derive_artifact_id(&request);
        let ordinal =
            i64::try_from(request.command_ordinal).map_err(|_| ValidationArtifactError)?;
        let existing = self
            .connection
            .query_row(
                "SELECT identity_json, media_type, artifact_id, content_digest
                 FROM validation_artifact
                 WHERE batch_id = ?1 AND command_ordinal = ?2
                   AND command_id = ?3 AND stream = ?4",
                params![
                    request.identity.batch_id.0,
                    ordinal,
                    request.command_id,
                    stream
                ],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| ValidationArtifactError)?;
        if let Some((stored_identity, stored_media_type, stored_id, stored_digest)) = existing {
            return if stored_identity == identity_json
                && stored_media_type == request.media_type
                && stored_id == artifact_id.0
                && stored_digest == digest.0
                && self.blob_bytes_equal(&digest, request.bytes)?
            {
                Ok(ArtifactReference {
                    artifact_id,
                    digest,
                })
            } else {
                Err(ValidationArtifactError)
            };
        }
        self.persist_blob(&digest, request.bytes)?;
        self.connection
            .execute(
                "INSERT INTO validation_artifact
                 (batch_id, command_ordinal, command_id, stream, identity_json,
                  media_type, artifact_id, content_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    request.identity.batch_id.0,
                    ordinal,
                    request.command_id,
                    stream,
                    identity_json,
                    request.media_type,
                    artifact_id.0,
                    digest.0
                ],
            )
            .map_err(|_| ValidationArtifactError)?;
        Ok(ArtifactReference {
            artifact_id,
            digest,
        })
    }

    fn persist_blob(
        &self,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ValidationArtifactError> {
        let path = blob_path(&self.blob_root, digest)?;
        if path.exists() {
            return if self.blob_bytes_equal(digest, bytes)? {
                Ok(())
            } else {
                Err(ValidationArtifactError)
            };
        }
        let temporary = self.blob_root.join(format!(
            ".{}.{}.{}.tmp",
            digest.0.trim_start_matches("sha256:"),
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| ValidationArtifactError)?;
        if file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return Err(ValidationArtifactError);
        }
        if fs::rename(&temporary, &path).is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(ValidationArtifactError);
        }
        sync_directory(&self.blob_root)
    }

    fn blob_bytes_equal(
        &self,
        digest: &Sha256Digest,
        expected: &[u8],
    ) -> Result<bool, ValidationArtifactError> {
        let bytes =
            fs::read(blob_path(&self.blob_root, digest)?).map_err(|_| ValidationArtifactError)?;
        Ok(bytes == expected)
    }
}

impl ValidationArtifactPort for DurableValidationArtifactStore {
    fn persist(
        &mut self,
        request: ValidationArtifactRequest<'_>,
    ) -> Result<ArtifactReference, ValidationArtifactError> {
        self.persist_inner(request)
    }
}

fn validate_request(
    request: &ValidationArtifactRequest<'_>,
) -> Result<(), ValidationArtifactError> {
    if validate_change_batch_identity_derivation(request.identity).is_err()
        || request.command_ordinal >= 64
        || request.command_id.is_empty()
        || request.command_id.len() > 100
        || !request
            .command_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || request.bytes.len() > MAX_ARTIFACT_BYTES
        || !matches!(
            request.media_type,
            "application/json"
                | "application/x-ndjson"
                | "application/xml"
                | "text/plain; charset=utf-8"
        )
    {
        return Err(ValidationArtifactError);
    }
    Ok(())
}

fn validation_artifact_schema_is_current(
    connection: &Connection,
) -> Result<bool, ValidationArtifactError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(validation_artifact)")
        .map_err(|_| ValidationArtifactError)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| ValidationArtifactError)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ValidationArtifactError)?;
    Ok(columns
        == [
            "batch_id",
            "command_ordinal",
            "command_id",
            "stream",
            "identity_json",
            "media_type",
            "artifact_id",
            "content_digest",
        ])
}

fn remove_stale_temporary_blobs(root: &Path) -> Result<(), ValidationArtifactError> {
    let mut removed = false;
    for entry in fs::read_dir(root).map_err(|_| ValidationArtifactError)? {
        let entry = entry.map_err(|_| ValidationArtifactError)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(ValidationArtifactError);
        };
        if name.starts_with('.')
            && Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        {
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(|_| ValidationArtifactError)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(ValidationArtifactError);
            }
            fs::remove_file(entry.path()).map_err(|_| ValidationArtifactError)?;
            removed = true;
        }
    }
    if removed {
        sync_directory(root)?;
    }
    Ok(())
}

fn derive_artifact_id(request: &ValidationArtifactRequest<'_>) -> ArtifactId {
    let mut digest = Sha256::new();
    frame(&mut digest, ARTIFACT_ID_DOMAIN);
    frame(&mut digest, request.identity.batch_id.0.as_bytes());
    frame(
        &mut digest,
        &u64::try_from(request.command_ordinal)
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    frame(&mut digest, request.command_id.as_bytes());
    frame(&mut digest, stream_text(request.stream).as_bytes());
    ArtifactId(format!("art_{}", crockford_130(&digest.finalize())))
}

fn crockford_130(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    (0..26)
        .map(|index| {
            let bit = index * 5;
            let byte = bit / 8;
            let shift = bit % 8;
            let pair = (u16::from(bytes[byte]) << 8) | u16::from(bytes[byte + 1]);
            let value = (pair >> (11 - shift)) & 0x1f;
            char::from(ALPHABET[usize::from(value)])
        })
        .collect()
}

fn frame(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

const fn stream_text(stream: ValidationArtifactStream) -> &'static str {
    match stream {
        ValidationArtifactStream::Stdout => "stdout",
        ValidationArtifactStream::Stderr => "stderr",
    }
}

fn blob_path(root: &Path, digest: &Sha256Digest) -> Result<PathBuf, ValidationArtifactError> {
    let hex = digest
        .0
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(ValidationArtifactError)?;
    Ok(root.join(hex))
}

fn ensure_private_directory(path: &Path) -> Result<(), ValidationArtifactError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(|_| ValidationArtifactError)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ValidationArtifactError);
        }
    } else {
        fs::create_dir_all(path).map_err(|_| ValidationArtifactError)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| ValidationArtifactError)
}

fn ensure_private_file(path: &Path) -> Result<(), ValidationArtifactError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(|_| ValidationArtifactError)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ValidationArtifactError);
        }
    } else {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| ValidationArtifactError)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| ValidationArtifactError)
}

fn sync_directory(path: &Path) -> Result<(), ValidationArtifactError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ValidationArtifactError)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, FencingToken, LeaseId, ProductSessionId, RepositoryId,
        SessionIdentity, Sha256Digest, StageRunId, WorkerSessionId, WorkspaceRevision,
    };
    use winwincode_execution_port::{
        change_batch_identity::derive_change_batch_id, generated::ChangeBatchIdentity,
    };

    use crate::workspace_runtime::{
        ValidationArtifactPort, ValidationArtifactRequest, ValidationArtifactStream,
    };

    use super::{DurableValidationArtifactStore, ValidationArtifactReadRequest};

    #[test]
    fn production_store_replays_exact_refs_and_rejects_changed_bytes_or_authority() {
        let root = TempDir::new().expect("artifact root");
        let identity = identity();
        let mut store = DurableValidationArtifactStore::open(root.path()).expect("open store");
        let first = store
            .persist(request(&identity, b"diagnostic\n"))
            .expect("persist first output");
        assert_eq!(
            store
                .persist(request(&identity, b"diagnostic\n"))
                .expect("replay output"),
            first
        );
        assert!(store.persist(request(&identity, b"changed\n")).is_err());
        let blob_count = std::fs::read_dir(root.path().join("blobs"))
            .expect("blob directory")
            .count();
        let mut stale = identity.clone();
        stale.fencing_token = FencingToken("2".to_owned());
        assert!(store.persist(request(&stale, b"diagnostic\n")).is_err());
        assert_eq!(
            std::fs::read_dir(root.path().join("blobs"))
                .expect("blob directory")
                .count(),
            blob_count
        );
        let stale_temporary = root.path().join("blobs/.stale.42.0.tmp");
        std::fs::write(&stale_temporary, b"interrupted temporary")
            .expect("write interrupted temporary");
        drop(store);
        let mut restarted = DurableValidationArtifactStore::open(root.path()).expect("restart");
        assert!(!stale_temporary.exists());
        assert_eq!(
            restarted
                .persist(request(&identity, b"diagnostic\n"))
                .expect("restart replay"),
            first
        );
        assert_eq!(
            restarted
                .read(ValidationArtifactReadRequest {
                    identity: &identity,
                    command_ordinal: 1,
                    command_id: "typescript-check",
                    stream: ValidationArtifactStream::Stdout,
                    reference: &first,
                })
                .expect("authority-bound read"),
            b"diagnostic\n"
        );
    }

    fn request<'request>(
        identity: &'request ChangeBatchIdentity,
        bytes: &'request [u8],
    ) -> ValidationArtifactRequest<'request> {
        ValidationArtifactRequest {
            identity,
            command_ordinal: 1,
            command_id: "typescript-check",
            stream: ValidationArtifactStream::Stdout,
            media_type: "text/plain; charset=utf-8",
            bytes,
        }
    }

    fn identity() -> ChangeBatchIdentity {
        let patch_digest = Sha256Digest(format!("sha256:{}", "1".repeat(64)));
        let run_key = "run-key-validation-artifact".to_owned();
        let turn_id = "turn-validation-artifact".to_owned();
        ChangeBatchIdentity {
            attempt: 1,
            batch_id: derive_change_batch_id(&run_key, &turn_id, None, &patch_digest)
                .expect("batch id"),
            call_id: None,
            fencing_token: FencingToken("1".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000001".to_owned()),
            patch_digest,
            repository_id: RepositoryId("repo_00000000000000000000000001".to_owned()),
            run_key,
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId("thr_00000000000000000000000001".to_owned()),
                product_session_id: ProductSessionId("psn_00000000000000000000000001".to_owned()),
                stage_run_id: Some(StageRunId("run_00000000000000000000000001".to_owned())),
                worker_session_id: WorkerSessionId("wss_00000000000000000000000001".to_owned()),
            },
            turn_id,
            workspace_revision: WorkspaceRevision(format!("git-tree:{}", "a".repeat(40))),
        }
    }
}
