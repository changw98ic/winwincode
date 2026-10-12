// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) const MANIFEST_FILE_NAME: &str = "manifest.json";
pub(crate) const TRACE_FILE_NAME: &str = "trace.jsonl";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSource {
    Delivery,
    WorkerRuntime,
    Artifact,
    Audit,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceDocument {
    pub kind: DocumentKind,
    pub bytes: Vec<u8>,
    pub expected_sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportClassification {
    Public,
    Confidential,
    Secret,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSource {
    pub artifact_id: String,
    pub logical_name: String,
    pub source_path: PathBuf,
    pub expected_sha256: String,
    pub expected_bytes: u64,
    pub classification: ExportClassification,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportCapacity {
    pub available_bytes: u64,
    pub reserve_bytes: u64,
    pub warning_below_bytes: u64,
}

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFileKind {
    Trace,
    PatchDiff,
    Verdict,
    MergeGuide,
    Artifact,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub path: String,
    pub kind: ManifestFileKind,
    pub byte_length: u64,
    pub sha256: String,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportReport {
    pub package_path: PathBuf,
    pub archive_path: Option<PathBuf>,
    pub manifest_sha256: String,
    pub estimated_bytes: u64,
    pub disk_warning: bool,
}

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
