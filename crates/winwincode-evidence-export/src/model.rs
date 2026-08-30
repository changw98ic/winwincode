// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) const MANIFEST_FILE_NAME: &str = "manifest.json";
pub(crate) const TRACE_FILE_NAME: &str = "trace.jsonl";

/// Canonical source of one trace summary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSource {
    Delivery,
    WorkerRuntime,
    Artifact,
    Audit,
}

/// Secret-free summary of a canonical source record.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRecord {
    pub source: TraceSource,
    pub occurred_at_millis: u64,
    pub sequence: u64,
    pub record_id: String,
    pub scope_id: String,
    pub kind: String,
    pub content_digest: String,
}

/// Fixed evidence documents required for offline review.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DocumentKind {
    PatchDiff,
    Verdict,
    MergeGuide,
}

impl DocumentKind {
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::PatchDiff => "patch.diff",
            Self::Verdict => "verdict.json",
            Self::MergeGuide => "merge-guide.md",
        }
    }
}

/// One required document with a caller-bound digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceDocument {
    pub kind: DocumentKind,
    pub bytes: Vec<u8>,
    pub expected_sha256: String,
}

/// Export boundary classification. Secret content is rejected before output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportClassification {
    Public,
    Confidential,
    Secret,
}

/// One content-addressed Artifact copied by explicit local reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSource {
    pub artifact_id: String,
    pub logical_name: String,
    pub source_path: PathBuf,
    pub expected_sha256: String,
    pub expected_bytes: u64,
    pub classification: ExportClassification,
}

/// Caller-observed disk capacity and the reserve that must remain afterwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportCapacity {
    pub available_bytes: u64,
    pub reserve_bytes: u64,
    pub warning_below_bytes: u64,
}

/// Complete explicit export request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportRequest {
    pub package_id: String,
    pub source_commit: String,
    pub trace_records: Vec<TraceRecord>,
    pub documents: Vec<EvidenceDocument>,
    pub artifacts: Vec<ArtifactSource>,
    pub capacity: ExportCapacity,
    pub create_archive: bool,
}

/// Stable kind stored in the manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFileKind {
    Trace,
    PatchDiff,
    Verdict,
    MergeGuide,
    Artifact,
}

/// Digest binding for one package file.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub path: String,
    pub kind: ManifestFileKind,
    pub byte_length: u64,
    pub sha256: String,
}

/// Canonical package index. Field order is part of stable serialization.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceManifest {
    pub schema_version: u32,
    pub package_id: String,
    pub source_commit: String,
    pub stable_bytes: bool,
    pub non_deterministic_fields: Vec<String>,
    pub trace_record_count: u64,
    pub files: Vec<ManifestFile>,
}

/// Successful export paths and capacity signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportReport {
    pub package_path: PathBuf,
    pub archive_path: Option<PathBuf>,
    pub manifest_sha256: String,
    pub estimated_bytes: u64,
    pub disk_warning: bool,
}

/// Stable failure class for callers and tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceErrorKind {
    InvalidInput,
    DigestMismatch,
    SecretDetected,
    InsufficientDisk,
    Conflict,
    Corrupt,
    Io,
}

/// Export or offline-verification failure.
#[derive(Debug)]
pub struct EvidenceError {
    kind: EvidenceErrorKind,
    message: String,
}

impl EvidenceError {
    pub(crate) fn new(kind: EvidenceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(EvidenceErrorKind::InvalidInput, message)
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::new(EvidenceErrorKind::Corrupt, message)
    }

    pub(crate) fn io(context: &str, error: &std::io::Error) -> Self {
        Self::new(EvidenceErrorKind::Io, format!("{context}: {error}"))
    }

    #[must_use]
    pub const fn kind(&self) -> EvidenceErrorKind {
        self.kind
    }
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EvidenceError {}
