// SPDX-License-Identifier: Apache-2.0

//! Deterministic local evidence export and offline verification.
//!
//! Inputs are explicit canonical summaries plus content-addressed files. The
//! crate deliberately has no repository or chat discovery API.

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
