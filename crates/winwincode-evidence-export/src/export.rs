// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::archive::write_archive;
use crate::model::{
    ArtifactSource, DocumentKind, EvidenceError, EvidenceErrorKind, EvidenceManifest,
    ExportClassification, ExportReport, ExportRequest, MANIFEST_FILE_NAME, ManifestFile,
    ManifestFileKind, TRACE_FILE_NAME,
};
use crate::secret::{SecretScanner, reject_secret_bytes};

static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);
const COPY_BUFFER_BYTES: usize = 64 * 1024;
type PreparedDocument = (String, Vec<u8>);

struct PreparedExport {
    manifest: EvidenceManifest,
    manifest_bytes: Vec<u8>,
    trace_bytes: Vec<u8>,
    documents: Vec<PreparedDocument>,
    artifacts: Vec<(String, ArtifactSource)>,
    estimated_bytes: u64,
    disk_warning: bool,
}

/// Create an atomic, byte-stable evidence directory and optional archive.
///
/// # Errors
///
/// Fails before output for invalid inputs, secrets, digest mismatches, existing
/// destinations, or an insufficient disk budget. I/O failures remove staging
/// files and never publish a partial package.
pub fn export_evidence(
    output_root: &Path,
    request: &ExportRequest,
) -> Result<ExportReport, EvidenceError> {
    let prepared = prepare(request)?;
    fs::create_dir_all(output_root)
        .map_err(|error| EvidenceError::io("create output root", &error))?;
    let final_package = output_root.join(&request.package_id);
    let final_archive = request
        .create_archive
        .then(|| output_root.join(format!("{}.wwcevidence", request.package_id)));
    reject_existing(&final_package, final_archive.as_deref())?;

    let unique = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
    let staging = output_root.join(format!(
        ".{}.tmp-{}-{unique}",
        request.package_id,
        std::process::id()
    ));
    let archive_staging = final_archive.as_ref().map(|_| {
        output_root.join(format!(
            ".{}.archive-tmp-{}-{unique}",
            request.package_id,
            std::process::id()
        ))
    });
    let result = write_prepared(&staging, archive_staging.as_deref(), &prepared);
    if let Err(error) = result {
        remove_staging(&staging, archive_staging.as_deref());
        return Err(error);
    }

    fs::rename(&staging, &final_package).map_err(|error| {
        remove_staging(&staging, archive_staging.as_deref());
        EvidenceError::io("publish evidence package", &error)
    })?;
    if let (Some(staging_archive), Some(final_archive_path)) = (&archive_staging, &final_archive)
        && let Err(error) = fs::rename(staging_archive, final_archive_path)
    {
        let _ = fs::remove_dir_all(&final_package);
        let _ = fs::remove_file(staging_archive);
        return Err(EvidenceError::io("publish evidence archive", &error));
    }

    Ok(ExportReport {
        package_path: final_package,
        archive_path: final_archive,
        manifest_sha256: digest_bytes(&prepared.manifest_bytes),
        estimated_bytes: prepared.estimated_bytes,
        disk_warning: prepared.disk_warning,
    })
}

fn prepare(request: &ExportRequest) -> Result<PreparedExport, EvidenceError> {
    validate_safe_component(&request.package_id, "packageId")?;
    validate_source_commit(&request.source_commit)?;
    validate_trace_records(request)?;
    let trace_bytes = serialize_trace(request)?;
    reject_secret_bytes(TRACE_FILE_NAME, &trace_bytes)?;

    let (documents, mut files) = prepare_documents(request)?;
    let artifacts = prepare_artifacts(request, &mut files)?;
    files.push(manifest_file(
        TRACE_FILE_NAME,
        ManifestFileKind::Trace,
        &trace_bytes,
    ));
    files.sort();

    let manifest = EvidenceManifest {
        schema_version: 1,
        package_id: request.package_id.clone(),
        source_commit: request.source_commit.clone(),
        stable_bytes: true,
        non_deterministic_fields: Vec::new(),
        trace_record_count: u64::try_from(request.trace_records.len())
            .map_err(|_| EvidenceError::invalid("trace record count exceeds u64"))?,
        files,
    };
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| EvidenceError::invalid(format!("serialize manifest: {error}")))?;
    manifest_bytes.push(b'\n');

    let content_bytes = manifest.files.iter().try_fold(
        u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX),
        |sum, file| {
            sum.checked_add(file.byte_length)
                .ok_or_else(|| EvidenceError::invalid("evidence package byte count overflow"))
        },
    )?;
    let archive_bytes = request
        .create_archive
        .then(|| estimate_archive_bytes(&manifest, manifest_bytes.len()))
        .transpose()?
        .unwrap_or(0);
    let estimated_bytes = content_bytes
        .checked_add(archive_bytes)
        .ok_or_else(|| EvidenceError::invalid("evidence export byte count overflow"))?;
    let required = estimated_bytes
        .checked_add(request.capacity.reserve_bytes)
        .ok_or_else(|| EvidenceError::invalid("disk reserve byte count overflow"))?;
    if request.capacity.available_bytes < required {
        return Err(EvidenceError::new(
            EvidenceErrorKind::InsufficientDisk,
            format!(
                "evidence export requires {required} bytes including reserve, only {} available",
                request.capacity.available_bytes
            ),
        ));
    }
    let remaining = request.capacity.available_bytes - required;

    Ok(PreparedExport {
        manifest,
        manifest_bytes,
        trace_bytes,
        documents,
        artifacts,
        estimated_bytes,
        disk_warning: remaining < request.capacity.warning_below_bytes,
    })
}

fn validate_trace_records(request: &ExportRequest) -> Result<(), EvidenceError> {
    let mut identities = BTreeSet::new();
    for record in &request.trace_records {
        validate_text(&record.record_id, "trace recordId")?;
        validate_text(&record.scope_id, "trace scopeId")?;
        validate_text(&record.kind, "trace kind")?;
        validate_digest(&record.content_digest, "trace contentDigest")?;
        let identity = (record.source, record.record_id.as_str());
        if !identities.insert(identity) {
            return Err(EvidenceError::invalid(
                "duplicate trace source and recordId",
            ));
        }
        for (label, text) in [
            ("trace recordId", record.record_id.as_bytes()),
            ("trace scopeId", record.scope_id.as_bytes()),
            ("trace kind", record.kind.as_bytes()),
        ] {
            reject_secret_bytes(label, text)?;
        }
    }
    Ok(())
}

fn serialize_trace(request: &ExportRequest) -> Result<Vec<u8>, EvidenceError> {
    let mut records = request.trace_records.clone();
    records.sort();
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record)
            .map_err(|error| EvidenceError::invalid(format!("serialize trace record: {error}")))?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn prepare_documents(
    request: &ExportRequest,
) -> Result<(Vec<PreparedDocument>, Vec<ManifestFile>), EvidenceError> {
    let required = [
        DocumentKind::PatchDiff,
        DocumentKind::Verdict,
        DocumentKind::MergeGuide,
    ];
    let mut seen = BTreeSet::new();
    let mut documents = Vec::new();
    let mut files = Vec::new();
    for document in &request.documents {
        if !seen.insert(document.kind) {
            return Err(EvidenceError::invalid(format!(
                "duplicate evidence document {}",
                document.kind.path()
            )));
        }
        validate_digest(&document.expected_sha256, "document expectedSha256")?;
        reject_secret_bytes(document.kind.path(), &document.bytes)?;
        let actual = digest_bytes(&document.bytes);
        if actual != document.expected_sha256 {
            return Err(EvidenceError::new(
                EvidenceErrorKind::DigestMismatch,
                format!("document digest mismatch for {}", document.kind.path()),
            ));
        }
        let kind = match document.kind {
            DocumentKind::PatchDiff => ManifestFileKind::PatchDiff,
            DocumentKind::Verdict => ManifestFileKind::Verdict,
            DocumentKind::MergeGuide => ManifestFileKind::MergeGuide,
        };
        files.push(manifest_file(document.kind.path(), kind, &document.bytes));
        documents.push((document.kind.path().to_owned(), document.bytes.clone()));
    }
    if required.into_iter().any(|kind| !seen.contains(&kind)) {
        return Err(EvidenceError::invalid(
            "patch.diff, verdict.json, and merge-guide.md are all required",
        ));
    }
    documents.sort_by(|left, right| left.0.cmp(&right.0));
    Ok((documents, files))
}

fn prepare_artifacts(
    request: &ExportRequest,
    files: &mut Vec<ManifestFile>,
) -> Result<Vec<(String, ArtifactSource)>, EvidenceError> {
    let mut artifacts = Vec::new();
    let mut paths = BTreeSet::new();
    for artifact in &request.artifacts {
        if artifact.classification == ExportClassification::Secret {
            return Err(EvidenceError::new(
                EvidenceErrorKind::SecretDetected,
                format!(
                    "secret Artifact {} cannot be exported",
                    artifact.artifact_id
                ),
            ));
        }
        validate_safe_component(&artifact.artifact_id, "artifactId")?;
        validate_safe_component(&artifact.logical_name, "Artifact logicalName")?;
        validate_digest(&artifact.expected_sha256, "Artifact expectedSha256")?;
        let path = format!(
            "artifacts/{}-{}",
            artifact.artifact_id, artifact.logical_name
        );
        if !paths.insert(path.clone()) {
            return Err(EvidenceError::invalid("duplicate Artifact export path"));
        }
        let (byte_length, sha256) = inspect_artifact(artifact)?;
        files.push(ManifestFile {
            path: path.clone(),
            kind: ManifestFileKind::Artifact,
            byte_length,
            sha256,
        });
        artifacts.push((path, artifact.clone()));
    }
    artifacts.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(artifacts)
}

fn inspect_artifact(artifact: &ArtifactSource) -> Result<(u64, String), EvidenceError> {
    let metadata = fs::metadata(&artifact.source_path)
        .map_err(|error| EvidenceError::io("read Artifact metadata", &error))?;
    if !metadata.is_file() || metadata.len() != artifact.expected_bytes {
        return Err(EvidenceError::new(
            EvidenceErrorKind::DigestMismatch,
            format!("Artifact {} byte length mismatch", artifact.artifact_id),
        ));
    }
    let mut file = File::open(&artifact.source_path)
        .map_err(|error| EvidenceError::io("open Artifact source", &error))?;
    let mut hash = Sha256::new();
    let mut scanner = SecretScanner::new(format!("Artifact {}", artifact.artifact_id));
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| EvidenceError::io("read Artifact source", &error))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        scanner.inspect(&buffer[..count])?;
    }
    let digest = lowercase_hex(&hash.finalize());
    if digest != artifact.expected_sha256 {
        return Err(EvidenceError::new(
            EvidenceErrorKind::DigestMismatch,
            format!("Artifact {} digest mismatch", artifact.artifact_id),
        ));
    }
    Ok((metadata.len(), digest))
}

fn write_prepared(
    staging: &Path,
    archive_staging: Option<&Path>,
    prepared: &PreparedExport,
) -> Result<(), EvidenceError> {
    fs::create_dir(staging)
        .map_err(|error| EvidenceError::io("create staging directory", &error))?;
    write_new(&staging.join(TRACE_FILE_NAME), &prepared.trace_bytes)?;
    for (path, bytes) in &prepared.documents {
        write_new(&staging.join(path), bytes)?;
    }
    for (path, source) in &prepared.artifacts {
        copy_verified_artifact(source, &staging.join(path))?;
    }
    write_new(&staging.join(MANIFEST_FILE_NAME), &prepared.manifest_bytes)?;
    sync_tree(staging, &prepared.manifest)?;
    if let Some(archive_path) = archive_staging {
        write_archive(staging, &prepared.manifest, archive_path)?;
    }
    Ok(())
}

fn copy_verified_artifact(
    source: &ArtifactSource,
    destination: &Path,
) -> Result<(), EvidenceError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| EvidenceError::io("create Artifact directory", &error))?;
    }
    let mut input = File::open(&source.source_path)
        .map_err(|error| EvidenceError::io("reopen Artifact source", &error))?;
    let mut output = new_file(destination)?;
    let mut hash = Sha256::new();
    let mut byte_length = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| EvidenceError::io("copy Artifact source", &error))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| EvidenceError::io("write Artifact copy", &error))?;
        hash.update(&buffer[..count]);
        byte_length = byte_length
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| EvidenceError::invalid("Artifact copy byte count overflow"))?;
    }
    output
        .sync_all()
        .map_err(|error| EvidenceError::io("sync Artifact copy", &error))?;
    if byte_length != source.expected_bytes
        || lowercase_hex(&hash.finalize()) != source.expected_sha256
    {
        return Err(EvidenceError::new(
            EvidenceErrorKind::DigestMismatch,
            format!("Artifact {} changed during export", source.artifact_id),
        ));
    }
    Ok(())
}

fn sync_tree(staging: &Path, manifest: &EvidenceManifest) -> Result<(), EvidenceError> {
    for file in &manifest.files {
        File::open(staging.join(&file.path))
            .and_then(|open| open.sync_all())
            .map_err(|error| EvidenceError::io("sync evidence file", &error))?;
    }
    File::open(staging.join(MANIFEST_FILE_NAME))
        .and_then(|open| open.sync_all())
        .map_err(|error| EvidenceError::io("sync manifest", &error))?;
    File::open(staging)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| EvidenceError::io("sync evidence directory", &error))
}

fn reject_existing(package: &Path, archive: Option<&Path>) -> Result<(), EvidenceError> {
    if package.exists() || archive.is_some_and(Path::exists) {
        return Err(EvidenceError::new(
            EvidenceErrorKind::Conflict,
            "evidence package or archive already exists",
        ));
    }
    Ok(())
}

fn remove_staging(staging: &Path, archive: Option<&Path>) {
    let _ = fs::remove_dir_all(staging);
    if let Some(path) = archive {
        let _ = fs::remove_file(path);
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), EvidenceError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| EvidenceError::io("create evidence directory", &error))?;
    }
    let mut file = new_file(path)?;
    file.write_all(bytes)
        .map_err(|error| EvidenceError::io("write evidence file", &error))?;
    file.sync_all()
        .map_err(|error| EvidenceError::io("sync evidence file", &error))
}

fn new_file(path: &Path) -> Result<File, EvidenceError> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| EvidenceError::io("create evidence file", &error))
}

fn manifest_file(path: &str, kind: ManifestFileKind, bytes: &[u8]) -> ManifestFile {
    ManifestFile {
        path: path.to_owned(),
        kind,
        byte_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        sha256: digest_bytes(bytes),
    }
}

fn validate_text(value: &str, label: &str) -> Result<(), EvidenceError> {
    if value.is_empty() || value.len() > 240 || value.chars().any(char::is_control) {
        return Err(EvidenceError::invalid(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_safe_component(value: &str, label: &str) -> Result<(), EvidenceError> {
    if value.is_empty()
        || value.len() > 120
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(EvidenceError::invalid(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_digest(value: &str, label: &str) -> Result<(), EvidenceError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(EvidenceError::invalid(format!(
            "{label} must be lowercase SHA-256"
        )));
    }
    Ok(())
}

fn validate_source_commit(value: &str) -> Result<(), EvidenceError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(EvidenceError::invalid(
            "sourceCommit must be a lowercase 40- or 64-character Git object ID",
        ));
    }
    Ok(())
}

pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

pub(crate) fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn estimate_archive_bytes(
    manifest: &EvidenceManifest,
    manifest_bytes: usize,
) -> Result<u64, EvidenceError> {
    let mut bytes = u64::try_from(crate::archive::ARCHIVE_MAGIC.len())
        .map_err(|_| EvidenceError::invalid("archive magic byte count exceeds u64"))?;
    for (path, length) in manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.byte_length))
        .chain(std::iter::once((
            MANIFEST_FILE_NAME,
            u64::try_from(manifest_bytes).unwrap_or(u64::MAX),
        )))
    {
        bytes = bytes
            .checked_add(4)
            .and_then(|value| value.checked_add(u64::try_from(path.len()).ok()?))
            .and_then(|value| value.checked_add(8))
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| EvidenceError::invalid("archive byte count overflow"))?;
    }
    bytes
        .checked_add(4)
        .ok_or_else(|| EvidenceError::invalid("archive byte count overflow"))
}
