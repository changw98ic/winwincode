// SPDX-License-Identifier: Apache-2.0

mod archive;
mod export;
mod model;
mod secret;
mod verify;

pub use export::export_evidence;
pub use model::{
    ArtifactSource, DocumentKind, EvidenceDocument, EvidenceError, EvidenceErrorKind,
    EvidenceManifest, ExportCapacity, ExportClassification, ExportReport, ExportRequest,
    ManifestFile, ManifestFileKind, TraceRecord, TraceSource,
};
pub use verify::{verify_evidence_archive, verify_evidence_package};
