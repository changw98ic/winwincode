// SPDX-License-Identifier: Apache-2.0

//! Durable raw evidence for one host-sealed `DebugProbe` execution.
//!
//! Process output enters this module before crossing the scheduler seam. Raw
//! bytes remain in private, content-addressed blobs; callers receive only
//! authority-bound Artifact references and byte counts. The later typed
//! evidence bundle is retained beside the same capture after the `ExecutionPort`
//! normalizer contract has sealed it.

use std::{
    fmt, fs,
    fs::{File, OpenOptions},
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ArtifactId, Sha256Digest};
use winwincode_execution_port::{
    debug_probe_contract::{
        ValidatedProbeExecutionIntent, derive_probe_definition_digest,
        validate_probe_execution_receipt,
    },
    generated::{
        ArtifactReference, DebugProbeErrorCode, DebugProbeIdentity, ProbeBaselineSelection,
        ProbeBaselineSelectionState, ProbeExecutionIntent, ProbeExecutionReceipt, ProbeRawStream,
        ProbeRawStreamBinding, ProbeRawStreamEncoding,
    },
    probe_result_normalizer::{
        ProbeEvidenceProjection, ValidatedPriorProbeEvidenceBundle, ValidatedProbeBaselineInput,
        ValidatedProbeEvidenceBundle, ValidatedProbeNormalizerProfile,
        canonical_probe_evidence_bundle_bytes, probe_baseline_not_applicable,
        probe_baseline_unavailable, project_probe_evidence,
        reopen_probe_evidence_bundle_from_journal, seal_prior_probe_evidence_bundle_bytes,
        seal_probe_baseline_selection, seal_probe_normalizer_profile,
        select_probe_evidence_baseline, validate_probe_evidence_bundle,
        validate_probe_evidence_bundle_inputs,
    },
};

const DATABASE_FILE: &str = "probe-evidence.sqlite3";
const BLOB_DIRECTORY: &str = "blobs";
const SCHEMA_VERSION: i64 = 1;
const ARTIFACT_ID_DOMAIN: &[u8] = b"winwincode.probe-evidence-artifact-id.v1";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const SCHEMA: &str = "
CREATE TABLE probe_capture (
    round_id TEXT NOT NULL,
    probe_id TEXT NOT NULL,
    probe_execution_id TEXT NOT NULL UNIQUE,
    debug_session_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    identity_json BLOB NOT NULL,
    intent_json BLOB NOT NULL,
    normalization_json BLOB NOT NULL,
    plan_digest TEXT NOT NULL,
    probe_definition_digest TEXT NOT NULL,
    manifest_json BLOB NOT NULL,
    receipt_json BLOB NOT NULL,
    PRIMARY KEY (round_id, probe_id)
);
CREATE TABLE probe_artifact (
    round_id TEXT NOT NULL,
    probe_id TEXT NOT NULL,
    slot TEXT NOT NULL,
    artifact_id TEXT NOT NULL UNIQUE,
    content_digest TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    PRIMARY KEY (round_id, probe_id, slot),
    FOREIGN KEY (round_id, probe_id)
        REFERENCES probe_capture(round_id, probe_id)
);
CREATE TABLE probe_bundle (
    round_id TEXT NOT NULL,
    probe_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL UNIQUE,
    content_digest TEXT NOT NULL,
    bundle_digest TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    normalized_at TEXT NOT NULL,
    PRIMARY KEY (round_id, probe_id),
    FOREIGN KEY (round_id, probe_id)
        REFERENCES probe_capture(round_id, probe_id)
);
PRAGMA user_version = 1;
";

/// Stable private-store failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeEvidenceStoreErrorKind {
    InvalidInput,
    Conflict,
    NotFound,
    DigestMismatch,
    Corrupt,
    Unavailable,
}

/// Secret-safe failure returned by the durable probe evidence module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProbeEvidenceStoreError {
    kind: ProbeEvidenceStoreErrorKind,
    message: &'static str,
}

impl ProbeEvidenceStoreError {
    const fn new(kind: ProbeEvidenceStoreErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> ProbeEvidenceStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for ProbeEvidenceStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProbeEvidenceStoreError {}

/// Authority-bound manifest for the exact stdout and stderr capture.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProbeRawArtifactManifest {
    schema_version: i64,
    identity: DebugProbeIdentity,
    plan_digest: Sha256Digest,
    probe_definition_digest: Sha256Digest,
    output_bytes: i64,
    output_truncated: bool,
    streams: Vec<ProbeRawStreamBinding>,
}

impl ProbeRawArtifactManifest {
    #[must_use]
    pub(crate) const fn identity(&self) -> &DebugProbeIdentity {
        &self.identity
    }

    #[must_use]
    pub(crate) const fn output_bytes(&self) -> i64 {
        self.output_bytes
    }

    #[must_use]
    pub(crate) const fn output_truncated(&self) -> bool {
        self.output_truncated
    }

    #[must_use]
    pub(crate) fn streams(&self) -> &[ProbeRawStreamBinding] {
        &self.streams
    }

    /// Returns stdout and stderr references in their canonical role order.
    #[must_use]
    pub(crate) fn artifact_references(&self) -> Vec<ArtifactReference> {
        self.streams
            .iter()
            .map(|artifact| artifact.artifact_ref.clone())
            .collect()
    }

    fn artifact(&self, stream: &ProbeRawStream) -> Option<&ProbeRawStreamBinding> {
        self.streams
            .iter()
            .find(|artifact| &artifact.stream == stream)
    }
}

/// Prepared bounded capture. It stays Worker-private until its exact L0
/// receipt can be committed atomically with the manifest.
#[derive(Clone, Debug)]
pub(crate) struct PreparedProbeCapture<'capture> {
    intent: &'capture ValidatedProbeExecutionIntent,
    stdout: &'capture [u8],
    stderr: &'capture [u8],
    manifest: ProbeRawArtifactManifest,
    normalization: ProbeNormalizationContext,
}

impl<'capture> PreparedProbeCapture<'capture> {
    pub(crate) fn try_new(
        intent: &'capture ValidatedProbeExecutionIntent,
        stdout: &'capture [u8],
        stderr: &'capture [u8],
        output_truncated: bool,
        normalization: &ProbeNormalizationContext,
    ) -> Result<Self, ProbeEvidenceStoreError> {
        let manifest = build_manifest(intent, stdout, stderr, output_truncated)?;
        Ok(Self {
            intent,
            stdout,
            stderr,
            manifest,
            normalization: normalization.clone(),
        })
    }

    #[must_use]
    pub(crate) const fn manifest(&self) -> &ProbeRawArtifactManifest {
        &self.manifest
    }
}

/// Exact host-selected normalization inputs bound before probe execution.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProbeNormalizationContext {
    binding: StoredNormalizationBinding,
    profile: ValidatedProbeNormalizerProfile,
    baseline: Box<ValidatedProbeBaselineInput>,
}

impl ProbeNormalizationContext {
    #[must_use]
    pub(crate) const fn profile(&self) -> &ValidatedProbeNormalizerProfile {
        &self.profile
    }

    #[must_use]
    pub(crate) const fn baseline(&self) -> &ValidatedProbeBaselineInput {
        &self.baseline
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredNormalizationBinding {
    profile: winwincode_execution_port::generated::ProbeNormalizerProfile,
    baseline: ProbeBaselineSelection,
}

struct SerializedCompletedExecution {
    identity: Vec<u8>,
    intent: Vec<u8>,
    normalization: Vec<u8>,
    manifest: Vec<u8>,
    receipt: Vec<u8>,
}

type RetainedBundleRow = (String, String, String, i64, String);

/// Exact authority and reference required to read one raw stream.
#[derive(Clone, Debug)]
pub(crate) struct ProbeRawArtifactReadRequest<'read> {
    intent: &'read ValidatedProbeExecutionIntent,
    stream: ProbeRawStream,
    reference: &'read ArtifactReference,
}

impl<'read> ProbeRawArtifactReadRequest<'read> {
    #[must_use]
    pub(crate) const fn new(
        intent: &'read ValidatedProbeExecutionIntent,
        stream: ProbeRawStream,
        reference: &'read ArtifactReference,
    ) -> Self {
        Self {
            intent,
            stream,
            reference,
        }
    }
}

/// Deep Worker-private store for probe raw Artifacts and normalized bundles.
#[derive(Debug)]
pub(crate) struct DurableProbeEvidenceStore {
    connection: Mutex<Connection>,
    blob_root: PathBuf,
}

/// Durable recovery point for one exact probe execution.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RecoveredProbeEvidence {
    /// Raw streams and the exact L0 execution receipt can be normalized again
    /// without starting the process a second time.
    ReceiptRetained {
        manifest: ProbeRawArtifactManifest,
        receipt: ProbeExecutionReceipt,
        normalization: ProbeNormalizationContext,
    },
    /// Raw streams, L0 receipt, canonical bundle, and bounded projection were
    /// all durably verified and can be replayed without process or parser work.
    Complete {
        manifest: ProbeRawArtifactManifest,
        receipt: ProbeExecutionReceipt,
        normalization: ProbeNormalizationContext,
        evidence: Box<RetainedProbeEvidenceBundle>,
    },
}

/// Verified normalized evidence retained as one canonical bundle Artifact.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RetainedProbeEvidenceBundle {
    bundle_artifact_ref: ArtifactReference,
    bundle: ValidatedProbeEvidenceBundle,
    projection: ProbeEvidenceProjection,
}

/// Exact prior bundle selected by durable host round context, never inferred
/// from wall-clock completion order or a model-provided probe identifier.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProbePriorEvidenceReference {
    artifact_ref: ArtifactReference,
    bundle_digest: Sha256Digest,
}

impl RetainedProbeEvidenceBundle {
    #[must_use]
    pub(crate) const fn bundle_artifact_ref(&self) -> &ArtifactReference {
        &self.bundle_artifact_ref
    }

    #[must_use]
    pub(crate) const fn bundle(&self) -> &ValidatedProbeEvidenceBundle {
        &self.bundle
    }

    #[must_use]
    pub(crate) const fn projection(&self) -> &ProbeEvidenceProjection {
        &self.projection
    }

    #[must_use]
    pub(crate) fn prior_reference(&self) -> ProbePriorEvidenceReference {
        ProbePriorEvidenceReference {
            artifact_ref: self.bundle_artifact_ref.clone(),
            bundle_digest: self.bundle.bundle().bundle_digest.clone(),
        }
    }
}

impl DurableProbeEvidenceStore {
    /// Opens the private store and verifies its one canonical schema.
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, ProbeEvidenceStoreError> {
        let root = root.as_ref();
        ensure_private_directory(root)?;
        let blob_root = root.join(BLOB_DIRECTORY);
        ensure_private_directory(&blob_root)?;
        remove_stale_temporary_blobs(&blob_root)?;
        let database = root.join(DATABASE_FILE);
        ensure_private_file(&database)?;
        let connection = Connection::open(database).map_err(|_| unavailable())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|_| unavailable())?;
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|_| unavailable())?;
        match version {
            0 => connection
                .execute_batch(SCHEMA)
                .map_err(|_| unavailable())?,
            SCHEMA_VERSION if schema_is_current(&connection)? => {}
            _ => return Err(corrupt()),
        }
        Ok(Self {
            connection: Mutex::new(connection),
            blob_root,
        })
    }

    /// Atomically retains the authority-bound raw manifest and exact L0 receipt.
    ///
    /// Blob files are synced before the `SQLite` transaction. A crash may leave
    /// an unreachable content-addressed blob, but can never expose a capture
    /// without its process status and timing facts. Exact repeats return the
    /// original manifest; any changed authority, bytes, or receipt conflicts.
    pub(crate) fn persist_completed_execution(
        &self,
        capture: &PreparedProbeCapture<'_>,
        receipt: &ProbeExecutionReceipt,
    ) -> Result<ProbeRawArtifactManifest, ProbeEvidenceStoreError> {
        let manifest = &capture.manifest;
        validate_manifest_for_intent(manifest, capture.intent)?;
        validate_receipt_for_manifest(receipt, capture.intent, manifest, false)?;
        let resolved =
            self.resolve_normalization_binding(capture.intent, &capture.normalization.binding)?;
        if resolved != capture.normalization {
            return Err(conflict());
        }
        let serialized = serialize_completed_execution(capture, receipt)?;
        if let Some(existing) = self.replay_completed_execution(capture, &serialized)? {
            return Ok(existing);
        }

        for (stream, bytes) in [
            (ProbeRawStream::Stdout, capture.stdout),
            (ProbeRawStream::Stderr, capture.stderr),
        ] {
            let artifact = manifest.artifact(&stream).ok_or_else(corrupt)?;
            self.persist_blob(&artifact.artifact_ref.digest, bytes)?;
        }

        self.insert_completed_execution(capture, &serialized)
    }

    fn replay_completed_execution(
        &self,
        capture: &PreparedProbeCapture<'_>,
        serialized: &SerializedCompletedExecution,
    ) -> Result<Option<ProbeRawArtifactManifest>, ProbeEvidenceStoreError> {
        let manifest = &capture.manifest;
        let Some(existing) = self.load_manifest_by_key(manifest.identity())? else {
            return Ok(None);
        };
        let existing_receipt = self
            .load_receipt_by_key(manifest.identity())?
            .ok_or_else(corrupt)?;
        let existing_normalization = self.load_normalization_binding(manifest.identity())?;
        if serde_json::to_vec(&existing).map_err(|_| corrupt())? != serialized.manifest
            || serde_json::to_vec(&existing_receipt).map_err(|_| corrupt())? != serialized.receipt
            || existing_normalization != capture.normalization.binding
        {
            return Err(conflict());
        }
        self.verify_capture_row(&existing)?;
        self.verify_manifest_rows(&existing)?;
        self.verify_manifest_blobs(&existing)?;
        Ok(Some(existing))
    }

    fn insert_completed_execution(
        &self,
        capture: &PreparedProbeCapture<'_>,
        serialized: &SerializedCompletedExecution,
    ) -> Result<ProbeRawArtifactManifest, ProbeEvidenceStoreError> {
        let manifest = &capture.manifest;
        let identity = &capture.intent.intent().identity;
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let existing = transaction
            .query_row(
                "SELECT manifest_json, receipt_json, normalization_json FROM probe_capture
                 WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        if let Some(existing) = existing {
            if existing
                != (
                    serialized.manifest.clone(),
                    serialized.receipt.clone(),
                    serialized.normalization.clone(),
                )
            {
                return Err(conflict());
            }
            transaction.commit().map_err(|_| unavailable())?;
            self.verify_capture_row(manifest)?;
            self.verify_manifest_rows(manifest)?;
            self.verify_manifest_blobs(manifest)?;
            return Ok(manifest.clone());
        }
        transaction
            .execute(
                "INSERT INTO probe_capture
                    (round_id, probe_id, probe_execution_id, debug_session_id,
                    repository_id, identity_json, intent_json, plan_digest,
                    probe_definition_digest, normalization_json, manifest_json,
                    receipt_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    identity.round_id.0,
                    identity.probe_id.0,
                    identity.probe_execution_id.0,
                    identity.debug_session_id.0,
                    identity.repository_id.0,
                    serialized.identity,
                    serialized.intent,
                    capture.intent.intent().plan_digest.0,
                    capture.intent.probe().spec().probe_definition_digest.0,
                    serialized.normalization,
                    serialized.manifest,
                    serialized.receipt,
                ],
            )
            .map_err(|_| conflict())?;
        for artifact in &manifest.streams {
            transaction
                .execute(
                    "INSERT INTO probe_artifact
                       (round_id, probe_id, slot, artifact_id, content_digest, size_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        identity.round_id.0,
                        identity.probe_id.0,
                        stream_name(&artifact.stream),
                        artifact.artifact_ref.artifact_id.0,
                        artifact.artifact_ref.digest.0,
                        artifact.retained_bytes,
                    ],
                )
                .map_err(|_| conflict())?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(manifest.clone())
    }

    /// Persists one already-sealed normalized bundle as a canonical Artifact.
    ///
    /// The exact raw manifest and L0 receipt must already be retained by
    /// [`Self::persist_completed_execution`]. Exact replays are idempotent;
    /// changed bundle bytes or semantic digests conflict.
    pub(crate) fn persist_bundle(
        &self,
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
        receipt: &ProbeExecutionReceipt,
        bundle: &ValidatedProbeEvidenceBundle,
    ) -> Result<RetainedProbeEvidenceBundle, ProbeEvidenceStoreError> {
        validate_manifest_for_intent(manifest, intent)?;
        self.verify_capture_row(manifest)?;
        self.verify_manifest_rows(manifest)?;
        self.verify_manifest_blobs(manifest)?;
        validate_receipt_for_manifest(receipt, intent, manifest, true)?;
        validate_bundle_for_capture(bundle, intent, manifest, receipt, false)?;
        let normalization = self.resolve_normalization_binding(
            intent,
            &self.load_normalization_binding(manifest.identity())?,
        )?;
        if validate_probe_evidence_bundle_inputs(
            bundle,
            intent,
            normalization.profile(),
            normalization.baseline(),
        )
        .is_err()
        {
            return Err(conflict());
        }

        let bytes = canonical_probe_evidence_bundle_bytes(bundle).map_err(|_| invalid_input())?;
        let content_digest = sha256_digest(&bytes);
        let artifact_ref = ArtifactReference {
            artifact_id: derive_artifact_id(&intent.intent().identity, "bundle"),
            digest: content_digest.clone(),
        };
        let projection =
            project_probe_evidence(bundle, artifact_ref.clone()).map_err(|_| invalid_input())?;
        let retained = RetainedProbeEvidenceBundle {
            bundle_artifact_ref: artifact_ref.clone(),
            bundle: bundle.clone(),
            projection,
        };
        self.persist_blob(&content_digest, &bytes)?;

        let size_bytes = i64::try_from(bytes.len()).map_err(|_| invalid_input())?;
        let expected = (
            artifact_ref.artifact_id.0.clone(),
            content_digest.0.clone(),
            bundle.bundle().bundle_digest.0.clone(),
            size_bytes,
            bundle.bundle().normalized_at.0.clone(),
        );
        let replay = self.persist_bundle_row(
            &intent.intent().identity,
            &serde_json::to_vec(manifest).map_err(|_| corrupt())?,
            &serde_json::to_vec(receipt).map_err(|_| corrupt())?,
            &expected,
        )?;
        if replay {
            return self
                .load_retained_bundle(intent, manifest, receipt, &normalization)?
                .ok_or_else(corrupt);
        }
        Ok(retained)
    }

    fn persist_bundle_row(
        &self,
        identity: &DebugProbeIdentity,
        manifest: &[u8],
        receipt: &[u8],
        expected: &RetainedBundleRow,
    ) -> Result<bool, ProbeEvidenceStoreError> {
        let mut connection = self.connection.lock().map_err(|_| unavailable())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let capture = transaction
            .query_row(
                "SELECT manifest_json, receipt_json FROM probe_capture
                 WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(corrupt)?;
        if capture.0 != manifest || capture.1 != receipt {
            return Err(conflict());
        }
        let existing = transaction
            .query_row(
                "SELECT artifact_id, content_digest, bundle_digest, size_bytes, normalized_at
                 FROM probe_bundle WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        if let Some(existing) = existing {
            if &existing != expected {
                return Err(conflict());
            }
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(true);
        }
        transaction
            .execute(
                "INSERT INTO probe_bundle
                   (round_id, probe_id, artifact_id, content_digest,
                    bundle_digest, size_bytes, normalized_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    identity.round_id.0,
                    identity.probe_id.0,
                    &expected.0,
                    &expected.1,
                    &expected.2,
                    expected.3,
                    &expected.4,
                ],
            )
            .map_err(|_| conflict())?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(false)
    }

    /// Reads and fully revalidates the canonical bundle Artifact, when one
    /// exists, without rerunning a parser or consulting the workspace.
    pub(crate) fn read_bundle(
        &self,
        intent: &ValidatedProbeExecutionIntent,
    ) -> Result<Option<RetainedProbeEvidenceBundle>, ProbeEvidenceStoreError> {
        let Some(manifest) = self.recover_capture(intent)? else {
            return Ok(None);
        };
        let receipt = self
            .load_receipt_by_key(manifest.identity())?
            .ok_or_else(corrupt)?;
        validate_receipt_for_manifest(&receipt, intent, &manifest, true)?;
        let normalization = self.resolve_normalization_binding(
            intent,
            &self.load_normalization_binding(manifest.identity())?,
        )?;
        let evidence = self.load_retained_bundle(intent, &manifest, &receipt, &normalization)?;
        if let Some(evidence) = &evidence {
            validate_probe_evidence_bundle_inputs(
                evidence.bundle(),
                intent,
                normalization.profile(),
                normalization.baseline(),
            )
            .map_err(|_| corrupt())?;
        }
        Ok(evidence)
    }

    /// Binds host-selected normalization inputs before probe execution.
    ///
    /// The optional prior reference must come from durable host round context.
    /// This store never guesses a predecessor from probe identifiers or wall
    /// clock order. Every supplied prior is rebound to private journal bytes
    /// and both of its digests before the `ExecutionPort` selects comparable or
    /// explicitly incompatible baseline evidence.
    pub(crate) fn prepare_normalization(
        &self,
        current_intent: &ValidatedProbeExecutionIntent,
        current_profile: &ValidatedProbeNormalizerProfile,
        prior: Option<&ProbePriorEvidenceReference>,
    ) -> Result<ProbeNormalizationContext, ProbeEvidenceStoreError> {
        if current_profile
            .profile()
            .diagnostic_parser_version
            .is_none()
        {
            if prior.is_some() {
                return Err(invalid_input());
            }
            return Ok(ProbeNormalizationContext {
                binding: StoredNormalizationBinding {
                    profile: current_profile.profile().clone(),
                    baseline: probe_baseline_not_applicable().selection().clone(),
                },
                profile: current_profile.clone(),
                baseline: Box::new(probe_baseline_not_applicable()),
            });
        }
        let baseline = if let Some(prior) = prior {
            let prior = self.load_prior_bundle(prior)?;
            select_probe_evidence_baseline(&prior, current_intent, current_profile)
                .map_err(|_| invalid_input())?
        } else {
            probe_baseline_unavailable()
        };
        Ok(ProbeNormalizationContext {
            binding: StoredNormalizationBinding {
                profile: current_profile.profile().clone(),
                baseline: baseline.selection().clone(),
            },
            profile: current_profile.clone(),
            baseline: Box::new(baseline),
        })
    }

    /// Returns the latest exact recovery point for a validated intent.
    pub(crate) fn recover(
        &self,
        intent: &ValidatedProbeExecutionIntent,
    ) -> Result<Option<RecoveredProbeEvidence>, ProbeEvidenceStoreError> {
        let Some(manifest) = self.recover_capture(intent)? else {
            return Ok(None);
        };
        let receipt = self
            .load_receipt_by_key(manifest.identity())?
            .ok_or_else(corrupt)?;
        validate_receipt_for_manifest(&receipt, intent, &manifest, true)?;
        let normalization = self.resolve_normalization_binding(
            intent,
            &self.load_normalization_binding(manifest.identity())?,
        )?;
        if let Some(evidence) =
            self.load_retained_bundle(intent, &manifest, &receipt, &normalization)?
        {
            validate_probe_evidence_bundle_inputs(
                evidence.bundle(),
                intent,
                normalization.profile(),
                normalization.baseline(),
            )
            .map_err(|_| corrupt())?;
            Ok(Some(RecoveredProbeEvidence::Complete {
                manifest,
                receipt,
                normalization,
                evidence: Box::new(evidence),
            }))
        } else {
            Ok(Some(RecoveredProbeEvidence::ReceiptRetained {
                manifest,
                receipt,
                normalization,
            }))
        }
    }

    /// Recovers the exact manifest for a validated intent after restart.
    pub(crate) fn recover_capture(
        &self,
        intent: &ValidatedProbeExecutionIntent,
    ) -> Result<Option<ProbeRawArtifactManifest>, ProbeEvidenceStoreError> {
        let Some(manifest) = self.load_manifest_by_key(&intent.intent().identity)? else {
            return Ok(None);
        };
        validate_manifest_for_intent(&manifest, intent)?;
        self.verify_capture_row(&manifest)?;
        self.verify_manifest_rows(&manifest)?;
        self.verify_manifest_blobs(&manifest)?;
        Ok(Some(manifest))
    }

    /// Reads one exact raw stream after revalidating authority, role, reference,
    /// metadata, content length, and digest.
    pub(crate) fn read_raw(
        &self,
        request: &ProbeRawArtifactReadRequest<'_>,
    ) -> Result<Vec<u8>, ProbeEvidenceStoreError> {
        let manifest = self
            .recover_capture(request.intent)?
            .ok_or_else(not_found)?;
        let artifact = manifest.artifact(&request.stream).ok_or_else(corrupt)?;
        if &artifact.artifact_ref != request.reference {
            return Err(conflict());
        }
        let bytes = read_private_blob(&self.blob_root, &artifact.artifact_ref.digest)?;
        verify_bytes(
            &artifact.artifact_ref.digest,
            artifact.retained_bytes,
            &bytes,
        )?;
        Ok(bytes)
    }

    fn load_manifest_by_key(
        &self,
        identity: &DebugProbeIdentity,
    ) -> Result<Option<ProbeRawArtifactManifest>, ProbeEvidenceStoreError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let bytes = connection
            .query_row(
                "SELECT manifest_json FROM probe_capture
                 WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable())?;
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| corrupt()))
            .transpose()
    }

    fn load_receipt_by_key(
        &self,
        identity: &DebugProbeIdentity,
    ) -> Result<Option<ProbeExecutionReceipt>, ProbeEvidenceStoreError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let bytes = connection
            .query_row(
                "SELECT receipt_json FROM probe_capture
                 WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable())?;
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| corrupt()))
            .transpose()
    }

    fn load_normalization_binding(
        &self,
        identity: &DebugProbeIdentity,
    ) -> Result<StoredNormalizationBinding, ProbeEvidenceStoreError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let bytes = connection
            .query_row(
                "SELECT normalization_json FROM probe_capture
                 WHERE round_id = ?1 AND probe_id = ?2",
                params![identity.round_id.0, identity.probe_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(corrupt)?;
        let binding =
            serde_json::from_slice::<StoredNormalizationBinding>(&bytes).map_err(|_| corrupt())?;
        if serde_json::to_vec(&binding).map_err(|_| corrupt())? != bytes {
            return Err(corrupt());
        }
        Ok(binding)
    }

    fn resolve_normalization_binding(
        &self,
        intent: &ValidatedProbeExecutionIntent,
        binding: &StoredNormalizationBinding,
    ) -> Result<ProbeNormalizationContext, ProbeEvidenceStoreError> {
        let profile =
            seal_probe_normalizer_profile(binding.profile.clone()).map_err(|_| corrupt())?;
        let prior_reference = match binding.baseline.state {
            ProbeBaselineSelectionState::PriorBundle => {
                let (Some(artifact_ref), Some(bundle_digest)) = (
                    binding.baseline.prior_bundle_artifact_ref.clone(),
                    binding.baseline.prior_bundle_digest.clone(),
                ) else {
                    return Err(corrupt());
                };
                Some(ProbePriorEvidenceReference {
                    artifact_ref,
                    bundle_digest,
                })
            }
            ProbeBaselineSelectionState::NotApplicable
            | ProbeBaselineSelectionState::Unavailable => None,
        };
        let prior = prior_reference
            .as_ref()
            .map(|reference| self.load_prior_bundle(reference))
            .transpose()?;
        let baseline =
            seal_probe_baseline_selection(&binding.baseline, prior.as_ref(), intent, &profile)
                .map_err(|_| corrupt())?;
        Ok(ProbeNormalizationContext {
            binding: binding.clone(),
            profile,
            baseline: Box::new(baseline),
        })
    }

    fn load_prior_bundle(
        &self,
        reference: &ProbePriorEvidenceReference,
    ) -> Result<ValidatedPriorProbeEvidenceBundle, ProbeEvidenceStoreError> {
        let retained = {
            let connection = self.connection.lock().map_err(|_| unavailable())?;
            connection
                .query_row(
                    "SELECT content_digest, bundle_digest, size_bytes
                     FROM probe_bundle WHERE artifact_id = ?1",
                    [&reference.artifact_ref.artifact_id.0],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| unavailable())?
                .ok_or_else(corrupt)?
        };
        if retained.0 != reference.artifact_ref.digest.0
            || retained.1 != reference.bundle_digest.0
            || retained.2 < 0
        {
            return Err(corrupt());
        }
        let bytes = read_private_blob(&self.blob_root, &reference.artifact_ref.digest)?;
        verify_bytes(&reference.artifact_ref.digest, retained.2, &bytes)?;
        seal_prior_probe_evidence_bundle_bytes(
            &bytes,
            reference.artifact_ref.clone(),
            &reference.bundle_digest,
        )
        .map_err(|error| normalizer_read_error(error.code()))
    }

    fn load_retained_bundle(
        &self,
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
        receipt: &ProbeExecutionReceipt,
        normalization: &ProbeNormalizationContext,
    ) -> Result<Option<RetainedProbeEvidenceBundle>, ProbeEvidenceStoreError> {
        let identity = &intent.intent().identity;
        let retained = {
            let connection = self.connection.lock().map_err(|_| unavailable())?;
            connection
                .query_row(
                    "SELECT artifact_id, content_digest, bundle_digest, size_bytes, normalized_at
                     FROM probe_bundle WHERE round_id = ?1 AND probe_id = ?2",
                    params![identity.round_id.0, identity.probe_id.0],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| unavailable())?
        };
        let Some((artifact_id, content_digest, bundle_digest, size_bytes, normalized_at)) =
            retained
        else {
            return Ok(None);
        };
        let artifact_ref = ArtifactReference {
            artifact_id: ArtifactId(artifact_id),
            digest: Sha256Digest(content_digest),
        };
        if artifact_ref.artifact_id != derive_artifact_id(identity, "bundle") || size_bytes < 0 {
            return Err(corrupt());
        }
        let bytes = read_private_blob(&self.blob_root, &artifact_ref.digest)?;
        verify_bytes(&artifact_ref.digest, size_bytes, &bytes)?;
        let expected_bundle_digest = Sha256Digest(bundle_digest.clone());
        let bundle = reopen_probe_evidence_bundle_from_journal(
            &bytes,
            intent,
            &artifact_ref,
            &expected_bundle_digest,
            normalization.profile(),
            normalization.baseline(),
        )
        .map_err(|error| normalizer_read_error(error.code()))?;
        validate_bundle_for_capture(&bundle, intent, manifest, receipt, true)?;
        if bundle.bundle().bundle_digest.0 != bundle_digest
            || bundle.bundle().normalized_at.0 != normalized_at
        {
            return Err(ProbeEvidenceStoreError::new(
                ProbeEvidenceStoreErrorKind::DigestMismatch,
                "probe evidence semantic digest does not match retained metadata",
            ));
        }
        let projection =
            project_probe_evidence(&bundle, artifact_ref.clone()).map_err(|_| corrupt())?;
        Ok(Some(RetainedProbeEvidenceBundle {
            bundle_artifact_ref: artifact_ref,
            bundle,
            projection,
        }))
    }

    fn verify_capture_row(
        &self,
        manifest: &ProbeRawArtifactManifest,
    ) -> Result<(), ProbeEvidenceStoreError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let retained = connection
            .query_row(
                "SELECT probe_execution_id, identity_json, intent_json,
                        plan_digest, probe_definition_digest, manifest_json
                 FROM probe_capture WHERE round_id = ?1 AND probe_id = ?2",
                params![manifest.identity.round_id.0, manifest.identity.probe_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(corrupt)?;
        let expected_identity = serde_json::to_vec(&manifest.identity).map_err(|_| corrupt())?;
        let retained_intent =
            serde_json::from_slice::<ProbeExecutionIntent>(&retained.2).map_err(|_| corrupt())?;
        let canonical_intent = serde_json::to_vec(&retained_intent).map_err(|_| corrupt())?;
        let retained_definition =
            derive_probe_definition_digest(&retained_intent.spec).map_err(|_| corrupt())?;
        let expected_manifest = serde_json::to_vec(manifest).map_err(|_| corrupt())?;
        if retained.0 != manifest.identity.probe_execution_id.0
            || retained.1 != expected_identity
            || retained.2 != canonical_intent
            || retained_intent.identity != manifest.identity
            || retained_intent.identity.probe_id != retained_intent.spec.probe_id
            || retained_intent.plan_digest != manifest.plan_digest
            || retained_intent.spec.probe_definition_digest != manifest.probe_definition_digest
            || retained_definition != manifest.probe_definition_digest
            || retained.3 != manifest.plan_digest.0
            || retained.4 != manifest.probe_definition_digest.0
            || retained.5 != expected_manifest
        {
            return Err(corrupt());
        }
        Ok(())
    }

    fn verify_manifest_rows(
        &self,
        manifest: &ProbeRawArtifactManifest,
    ) -> Result<(), ProbeEvidenceStoreError> {
        let connection = self.connection.lock().map_err(|_| unavailable())?;
        let mut statement = connection
            .prepare(
                "SELECT slot, artifact_id, content_digest, size_bytes
                 FROM probe_artifact WHERE round_id = ?1 AND probe_id = ?2
                 ORDER BY slot ASC",
            )
            .map_err(|_| unavailable())?;
        let rows = statement
            .query_map(
                params![manifest.identity.round_id.0, manifest.identity.probe_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|_| unavailable())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        let expected = manifest
            .streams
            .iter()
            .map(|artifact| {
                (
                    stream_name(&artifact.stream).to_owned(),
                    artifact.artifact_ref.artifact_id.0.clone(),
                    artifact.artifact_ref.digest.0.clone(),
                    artifact.retained_bytes,
                )
            })
            .collect::<Vec<_>>();
        let mut expected = expected;
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        if rows == expected {
            Ok(())
        } else {
            Err(corrupt())
        }
    }

    fn verify_manifest_blobs(
        &self,
        manifest: &ProbeRawArtifactManifest,
    ) -> Result<(), ProbeEvidenceStoreError> {
        for artifact in &manifest.streams {
            let bytes = read_private_blob(&self.blob_root, &artifact.artifact_ref.digest)?;
            verify_bytes(
                &artifact.artifact_ref.digest,
                artifact.retained_bytes,
                &bytes,
            )?;
            let actual_encoding = if std::str::from_utf8(&bytes).is_ok() {
                ProbeRawStreamEncoding::Utf8
            } else {
                ProbeRawStreamEncoding::InvalidUtf8
            };
            if artifact.encoding != actual_encoding {
                return Err(corrupt());
            }
        }
        Ok(())
    }

    fn persist_blob(
        &self,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ProbeEvidenceStoreError> {
        let path = blob_path(&self.blob_root, digest)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let retained = read_private_blob(&self.blob_root, digest)?;
                return verify_bytes(
                    digest,
                    i64::try_from(bytes.len()).map_err(|_| invalid_input())?,
                    &retained,
                )
                .and_then(|()| {
                    if retained == bytes {
                        Ok(())
                    } else {
                        Err(ProbeEvidenceStoreError::new(
                            ProbeEvidenceStoreErrorKind::DigestMismatch,
                            "probe Artifact content conflicts with its digest",
                        ))
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(unavailable()),
        }
        let temporary = self.blob_root.join(format!(
            ".{}.{}.{}.tmp",
            digest.0.trim_start_matches("sha256:"),
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| unavailable())?;
        if file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return Err(unavailable());
        }
        if fs::rename(&temporary, &path).is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(unavailable());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|_| unavailable())?;
        sync_directory(&self.blob_root)
    }
}

fn serialize_completed_execution(
    capture: &PreparedProbeCapture<'_>,
    receipt: &ProbeExecutionReceipt,
) -> Result<SerializedCompletedExecution, ProbeEvidenceStoreError> {
    Ok(SerializedCompletedExecution {
        identity: serde_json::to_vec(&capture.intent.intent().identity).map_err(|_| corrupt())?,
        intent: serde_json::to_vec(capture.intent.intent()).map_err(|_| corrupt())?,
        normalization: serde_json::to_vec(&capture.normalization.binding).map_err(|_| corrupt())?,
        manifest: serde_json::to_vec(&capture.manifest).map_err(|_| corrupt())?,
        receipt: serde_json::to_vec(receipt).map_err(|_| corrupt())?,
    })
}

fn build_manifest(
    validated_intent: &ValidatedProbeExecutionIntent,
    stdout: &[u8],
    stderr: &[u8],
    output_truncated: bool,
) -> Result<ProbeRawArtifactManifest, ProbeEvidenceStoreError> {
    let intent = validated_intent.intent();
    let total = stdout
        .len()
        .checked_add(stderr.len())
        .ok_or_else(invalid_input)?;
    let output_bytes = i64::try_from(total).map_err(|_| invalid_input())?;
    let output_limit = validated_intent.probe().spec().output_limit_bytes;
    if output_bytes > output_limit || (output_truncated && output_bytes != output_limit) {
        return Err(invalid_input());
    }
    let streams = [
        (ProbeRawStream::Stdout, stdout),
        (ProbeRawStream::Stderr, stderr),
    ]
    .into_iter()
    .map(|(stream, bytes)| {
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
        let artifact_id = derive_artifact_id(&intent.identity, stream_name(&stream));
        Ok(ProbeRawStreamBinding {
            stream,
            artifact_ref: ArtifactReference {
                artifact_id,
                digest,
            },
            retained_bytes: i64::try_from(bytes.len()).map_err(|_| invalid_input())?,
            encoding: if std::str::from_utf8(bytes).is_ok() {
                ProbeRawStreamEncoding::Utf8
            } else {
                ProbeRawStreamEncoding::InvalidUtf8
            },
        })
    })
    .collect::<Result<Vec<_>, ProbeEvidenceStoreError>>()?;
    Ok(ProbeRawArtifactManifest {
        schema_version: 1,
        identity: intent.identity.clone(),
        plan_digest: intent.plan_digest.clone(),
        probe_definition_digest: validated_intent
            .probe()
            .spec()
            .probe_definition_digest
            .clone(),
        output_bytes,
        output_truncated,
        streams,
    })
}

fn validate_receipt_for_manifest(
    receipt: &ProbeExecutionReceipt,
    intent: &ValidatedProbeExecutionIntent,
    manifest: &ProbeRawArtifactManifest,
    retained: bool,
) -> Result<(), ProbeEvidenceStoreError> {
    if validate_probe_execution_receipt(receipt, intent).is_err()
        || receipt.output_bytes != manifest.output_bytes
        || receipt.output_truncated != manifest.output_truncated
        || receipt.artifact_refs != manifest.artifact_references()
    {
        return Err(if retained { corrupt() } else { conflict() });
    }
    Ok(())
}

fn validate_bundle_for_capture(
    bundle: &ValidatedProbeEvidenceBundle,
    intent: &ValidatedProbeExecutionIntent,
    manifest: &ProbeRawArtifactManifest,
    receipt: &ProbeExecutionReceipt,
    retained: bool,
) -> Result<(), ProbeEvidenceStoreError> {
    if validate_probe_evidence_bundle(bundle.bundle(), intent).is_err()
        || bundle.bundle().l0_receipt != *receipt
        || bundle.bundle().raw_streams != manifest.streams
    {
        return Err(if retained { corrupt() } else { conflict() });
    }
    Ok(())
}

fn validate_manifest_for_intent(
    manifest: &ProbeRawArtifactManifest,
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), ProbeEvidenceStoreError> {
    if manifest.schema_version != 1
        || manifest.identity != intent.intent().identity
        || manifest.plan_digest != intent.intent().plan_digest
        || manifest.probe_definition_digest != intent.probe().spec().probe_definition_digest
        || manifest.streams.len() != 2
        || manifest.output_bytes < 0
        || manifest.output_bytes > intent.probe().spec().output_limit_bytes
        || (manifest.output_truncated
            && manifest.output_bytes != intent.probe().spec().output_limit_bytes)
    {
        return Err(conflict());
    }
    let stdout = manifest
        .artifact(&ProbeRawStream::Stdout)
        .ok_or_else(corrupt)?;
    let stderr = manifest
        .artifact(&ProbeRawStream::Stderr)
        .ok_or_else(corrupt)?;
    if stdout.artifact_ref.artifact_id
        != derive_artifact_id(&manifest.identity, stream_name(&ProbeRawStream::Stdout))
        || stderr.artifact_ref.artifact_id
            != derive_artifact_id(&manifest.identity, stream_name(&ProbeRawStream::Stderr))
        || stdout.retained_bytes < 0
        || stderr.retained_bytes < 0
        || stdout.retained_bytes.saturating_add(stderr.retained_bytes) != manifest.output_bytes
    {
        return Err(corrupt());
    }
    Ok(())
}

fn verify_bytes(
    expected_digest: &Sha256Digest,
    expected_size: i64,
    bytes: &[u8],
) -> Result<(), ProbeEvidenceStoreError> {
    let size = i64::try_from(bytes.len()).map_err(|_| corrupt())?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    if size != expected_size || digest != *expected_digest {
        Err(ProbeEvidenceStoreError::new(
            ProbeEvidenceStoreErrorKind::DigestMismatch,
            "probe Artifact bytes do not match retained metadata",
        ))
    } else {
        Ok(())
    }
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn derive_artifact_id(identity: &DebugProbeIdentity, slot: &str) -> ArtifactId {
    let mut digest = Sha256::new();
    frame(&mut digest, ARTIFACT_ID_DOMAIN);
    frame(&mut digest, identity.probe_execution_id.0.as_bytes());
    frame(&mut digest, slot.as_bytes());
    ArtifactId(format!("art_{}", crockford_130(&digest.finalize())))
}

const fn stream_name(stream: &ProbeRawStream) -> &'static str {
    match stream {
        ProbeRawStream::Stdout => "stdout",
        ProbeRawStream::Stderr => "stderr",
    }
}

fn frame(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
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

fn blob_path(root: &Path, digest: &Sha256Digest) -> Result<PathBuf, ProbeEvidenceStoreError> {
    let hex = digest
        .0
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(corrupt)?;
    Ok(root.join(hex))
}

fn read_private_blob(
    root: &Path,
    digest: &Sha256Digest,
) -> Result<Vec<u8>, ProbeEvidenceStoreError> {
    let path = blob_path(root, digest)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            not_found()
        } else {
            unavailable()
        }
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(corrupt());
    }
    fs::read(path).map_err(|_| unavailable())
}

fn ensure_private_directory(path: &Path) -> Result<(), ProbeEvidenceStoreError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(unavailable());
        }
    } else {
        fs::create_dir_all(path).map_err(|_| unavailable())?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| unavailable())
}

fn ensure_private_file(path: &Path) -> Result<(), ProbeEvidenceStoreError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(unavailable());
        }
    } else {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| unavailable())?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| unavailable())
}

fn remove_stale_temporary_blobs(root: &Path) -> Result<(), ProbeEvidenceStoreError> {
    let mut removed = false;
    for entry in fs::read_dir(root).map_err(|_| unavailable())? {
        let entry = entry.map_err(|_| unavailable())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(corrupt());
        };
        if name.starts_with('.')
            && Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| unavailable())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(corrupt());
            }
            fs::remove_file(entry.path()).map_err(|_| unavailable())?;
            removed = true;
        }
    }
    if removed {
        sync_directory(root)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ProbeEvidenceStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| unavailable())
}

fn schema_is_current(connection: &Connection) -> Result<bool, ProbeEvidenceStoreError> {
    Ok(table_columns(connection, "probe_capture")?
        == [
            "round_id",
            "probe_id",
            "probe_execution_id",
            "debug_session_id",
            "repository_id",
            "identity_json",
            "intent_json",
            "normalization_json",
            "plan_digest",
            "probe_definition_digest",
            "manifest_json",
            "receipt_json",
        ]
        && table_columns(connection, "probe_artifact")?
            == [
                "round_id",
                "probe_id",
                "slot",
                "artifact_id",
                "content_digest",
                "size_bytes",
            ]
        && table_columns(connection, "probe_bundle")?
            == [
                "round_id",
                "probe_id",
                "artifact_id",
                "content_digest",
                "bundle_digest",
                "size_bytes",
                "normalized_at",
            ])
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<String>, ProbeEvidenceStoreError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| unavailable())?;
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| unavailable())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable())
}

const fn invalid_input() -> ProbeEvidenceStoreError {
    ProbeEvidenceStoreError::new(
        ProbeEvidenceStoreErrorKind::InvalidInput,
        "probe capture exceeds its validated bounds",
    )
}

const fn conflict() -> ProbeEvidenceStoreError {
    ProbeEvidenceStoreError::new(
        ProbeEvidenceStoreErrorKind::Conflict,
        "probe evidence conflicts with retained authority or bytes",
    )
}

const fn not_found() -> ProbeEvidenceStoreError {
    ProbeEvidenceStoreError::new(
        ProbeEvidenceStoreErrorKind::NotFound,
        "probe evidence is unavailable",
    )
}

const fn corrupt() -> ProbeEvidenceStoreError {
    ProbeEvidenceStoreError::new(
        ProbeEvidenceStoreErrorKind::Corrupt,
        "probe evidence durable state is corrupt",
    )
}

const fn normalizer_read_error(code: &DebugProbeErrorCode) -> ProbeEvidenceStoreError {
    if matches!(code, DebugProbeErrorCode::ArtifactDigestMismatch) {
        ProbeEvidenceStoreError::new(
            ProbeEvidenceStoreErrorKind::DigestMismatch,
            "probe evidence digest does not match retained metadata",
        )
    } else {
        corrupt()
    }
}

const fn unavailable() -> ProbeEvidenceStoreError {
    ProbeEvidenceStoreError::new(
        ProbeEvidenceStoreErrorKind::Unavailable,
        "probe evidence durable store is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use tempfile::TempDir;
    use winwincode_domain::{
        CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, FencingToken, Instant,
        LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId, SessionIdentity,
        WorkerSessionId, WorkspaceRevision,
    };
    use winwincode_execution_port::{
        debug_probe_contract::{
            ValidatedDebugProbePlan, derive_debug_probe_plan_digest, derive_probe_budget_digest,
            derive_probe_command_arg_bytes, derive_probe_definition_digest, seal_debug_probe_plan,
            seal_probe_execution_intent,
        },
        generated::{
            DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority, DiagnosticParserVersion,
            ProbeBaselineState, ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind,
            ProbeExecutionIntent, ProbeNetworkAccess, ProbeNormalizerProfile,
            ProbeNormalizerVersion, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget,
            ProbeSideEffectClass, ProbeSpec, ProbeStackParserVersion, ProbeWorkspaceAccess,
        },
        probe_result_normalizer::{
            ProbeRawStreamInput, derive_probe_normalizer_profile_digest, normalize_probe_evidence,
            probe_baseline_not_applicable, seal_probe_normalizer_profile,
        },
    };

    use super::*;

    fn digest(character: char) -> Sha256Digest {
        Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
    }

    fn authority(round: char, lease: char, environment: char) -> DebugProbeRoundAuthority {
        DebugProbeRoundAuthority {
            attempt: 1,
            debug_session_id: DebugSessionId("dbg_00000000000000000000000000".to_owned()),
            environment_digest: digest(environment),
            fencing_token: FencingToken("7".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            lease_id: LeaseId(format!("lse_{}", lease.to_string().repeat(26))),
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
            round_id: ProbeRoundId(format!("prn_{}", round.to_string().repeat(26))),
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                stage_run_id: None,
                worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
            },
            workspace_revision: WorkspaceRevision(format!("git-tree:{}", "0".repeat(40))),
        }
    }

    fn sealed_plan(round: char, lease: char, environment: char) -> ValidatedDebugProbePlan {
        let mut probe = ProbeSpec {
            command: ProbeCommandSpec {
                argv: vec!["fixture-probe".to_owned()],
                command_arg_bytes: 1,
                working_directory: ".".to_owned(),
            },
            kind: DebugProbeKind::StaticAnalysis,
            output_limit_bytes: 1_024,
            probe_definition_digest: digest('2'),
            probe_id: ProbeId("prb_00000000000000000000000000".to_owned()),
            required: true,
            resources: ProbeResourceClaim {
                cpu_limit_millis: 100,
                database_keys: Vec::new(),
                exclusive_keys: Vec::new(),
                memory_limit_bytes: 1_048_576,
                network_access: ProbeNetworkAccess::None,
                paths: vec!["src".to_owned()],
                port_numbers: Vec::new(),
                service_keys: Vec::new(),
                side_effect_class: ProbeSideEffectClass::PureRead,
                workspace_access: ProbeWorkspaceAccess::ReadOnly,
            },
            target_hypothesis_ids: vec![DebugHypothesisId(
                "hyp_00000000000000000000000000".to_owned(),
            )],
            timeout_millis: 1_000,
        };
        probe.command.command_arg_bytes =
            derive_probe_command_arg_bytes(&probe.command.argv).expect("command bytes");
        probe.probe_definition_digest =
            derive_probe_definition_digest(&probe).expect("probe digest");
        let mut budget = ProbeRoundBudget {
            budget_digest: digest('3'),
            parallel_probe_limit: 1,
            peak_memory_limit_bytes: 1_048_576,
            probe_limit: 1,
            total_command_arg_limit_bytes: 1_024,
            total_cpu_limit_millis: 100,
            total_output_limit_bytes: 1_024,
            wall_time_limit_millis: 1_000,
        };
        budget.budget_digest = derive_probe_budget_digest(&budget).expect("budget digest");
        let mut plan = DebugProbePlan {
            authority: authority(round, lease, environment),
            budget,
            completion_rule: ProbeCompletionRule {
                kind: ProbeCompletionRuleKind::AllTerminal,
                minimum_completed_probes: 1,
                minimum_successful_probes: 1,
                stop_on_required_probe_failure: true,
            },
            created_at: Instant("2026-09-06T08:00:00.000Z".to_owned()),
            plan_digest: digest('4'),
            probes: vec![probe],
            schema_version: 1,
        };
        plan.plan_digest = derive_debug_probe_plan_digest(&plan).expect("plan digest");
        seal_debug_probe_plan(plan.clone(), &plan.authority).expect("sealed plan")
    }

    fn sealed_intent(lease: char) -> ValidatedProbeExecutionIntent {
        sealed_intent_for('0', lease, '1')
    }

    fn sealed_intent_for(
        round: char,
        lease: char,
        environment: char,
    ) -> ValidatedProbeExecutionIntent {
        let plan = sealed_plan(round, lease, environment);
        let probe = &plan.probes()[0];
        let intent = ProbeExecutionIntent {
            created_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
            identity: plan
                .probe_identity(&probe.spec().probe_id)
                .expect("probe identity"),
            plan_digest: plan.plan().plan_digest.clone(),
            schema_version: 1,
            spec: probe.spec().clone(),
        };
        seal_probe_execution_intent(intent, &plan).expect("sealed intent")
    }

    fn success_receipt(
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
    ) -> ProbeExecutionReceipt {
        ProbeExecutionReceipt {
            artifact_refs: manifest.artifact_references(),
            duration_millis: 10,
            error: None,
            exit_code: Some(0),
            finished_at: Instant("2026-09-06T08:00:03.000Z".to_owned()),
            identity: intent.intent().identity.clone(),
            output_bytes: manifest.output_bytes(),
            output_truncated: manifest.output_truncated(),
            plan_digest: intent.intent().plan_digest.clone(),
            schema_version: 1,
            signal: None,
            started_at: Instant("2026-09-06T08:00:02.000Z".to_owned()),
            status: ProbeReceiptStatus::Succeeded,
            timed_out: false,
        }
    }

    fn normalization_context(
        store: &DurableProbeEvidenceStore,
        intent: &ValidatedProbeExecutionIntent,
    ) -> ProbeNormalizationContext {
        let mut profile = ProbeNormalizerProfile {
            diagnostic_parser_version: None,
            normalizer_version: ProbeNormalizerVersion::L0L1V1,
            profile_digest: digest('8'),
            stack_parser_version: None,
        };
        profile.profile_digest =
            derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
        let profile = seal_probe_normalizer_profile(profile).expect("sealed profile");
        store
            .prepare_normalization(intent, &profile, None)
            .expect("prepare normalization")
    }

    fn capture<'a>(
        store: &DurableProbeEvidenceStore,
        intent: &'a ValidatedProbeExecutionIntent,
    ) -> PreparedProbeCapture<'a> {
        PreparedProbeCapture::try_new(
            intent,
            b"ok\n",
            b"\xff!",
            false,
            &normalization_context(store, intent),
        )
        .expect("prepare capture")
    }

    fn normalized_bundle(
        root: &Path,
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
        receipt: &ProbeExecutionReceipt,
        stack_parser: bool,
    ) -> ValidatedProbeEvidenceBundle {
        let mut profile = ProbeNormalizerProfile {
            diagnostic_parser_version: None,
            normalizer_version: ProbeNormalizerVersion::L0L1V1,
            profile_digest: digest('8'),
            stack_parser_version: stack_parser.then_some(ProbeStackParserVersion::RustV1),
        };
        profile.profile_digest =
            derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
        let profile = seal_probe_normalizer_profile(profile).expect("sealed profile");
        let stdout = manifest
            .artifact(&ProbeRawStream::Stdout)
            .expect("stdout binding");
        let stderr = manifest
            .artifact(&ProbeRawStream::Stderr)
            .expect("stderr binding");
        let raw = [
            ProbeRawStreamInput::new(ProbeRawStream::Stdout, stdout.artifact_ref.clone(), b"ok\n"),
            ProbeRawStreamInput::new(
                ProbeRawStream::Stderr,
                stderr.artifact_ref.clone(),
                b"\xff!",
            ),
        ];
        normalize_probe_evidence(
            intent,
            receipt,
            &profile,
            &raw,
            &probe_baseline_not_applicable(),
            root,
        )
        .expect("normalized bundle")
    }

    fn diagnostic_profile(parser: DiagnosticParserVersion) -> ValidatedProbeNormalizerProfile {
        let mut profile = ProbeNormalizerProfile {
            diagnostic_parser_version: Some(parser),
            normalizer_version: ProbeNormalizerVersion::L0L1V1,
            profile_digest: digest('9'),
            stack_parser_version: None,
        };
        profile.profile_digest =
            derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
        seal_probe_normalizer_profile(profile).expect("sealed diagnostic profile")
    }

    fn normalize_bytes(
        root: &Path,
        intent: &ValidatedProbeExecutionIntent,
        manifest: &ProbeRawArtifactManifest,
        receipt: &ProbeExecutionReceipt,
        normalization: &ProbeNormalizationContext,
        stdout_bytes: &[u8],
        stderr_bytes: &[u8],
    ) -> ValidatedProbeEvidenceBundle {
        let stdout = manifest
            .artifact(&ProbeRawStream::Stdout)
            .expect("stdout binding");
        let stderr = manifest
            .artifact(&ProbeRawStream::Stderr)
            .expect("stderr binding");
        normalize_probe_evidence(
            intent,
            receipt,
            normalization.profile(),
            &[
                ProbeRawStreamInput::new(
                    ProbeRawStream::Stdout,
                    stdout.artifact_ref.clone(),
                    stdout_bytes,
                ),
                ProbeRawStreamInput::new(
                    ProbeRawStream::Stderr,
                    stderr.artifact_ref.clone(),
                    stderr_bytes,
                ),
            ],
            normalization.baseline(),
            root,
        )
        .expect("normalize bytes")
    }

    const TYPESCRIPT_DIAGNOSTIC: &[u8] =
        b"src/probe.ts(12,7): error TS2322: Type 'string' is not assignable to type 'number'.\n";

    fn retain_prior_bundle(
        store: &DurableProbeEvidenceStore,
        evidence_root: &Path,
        profile: &ValidatedProbeNormalizerProfile,
    ) -> ProbePriorEvidenceReference {
        let prior_intent = sealed_intent_for('1', '1', '1');
        let prior_normalization = store
            .prepare_normalization(&prior_intent, profile, None)
            .expect("unavailable first baseline");
        assert_eq!(
            prior_normalization.baseline().selection().state,
            ProbeBaselineSelectionState::Unavailable
        );
        let prior_capture = PreparedProbeCapture::try_new(
            &prior_intent,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
            false,
            &prior_normalization,
        )
        .expect("prior capture");
        let prior_receipt = success_receipt(&prior_intent, prior_capture.manifest());
        let prior_manifest = store
            .persist_completed_execution(&prior_capture, &prior_receipt)
            .expect("retain prior execution");
        let prior_bundle = normalize_bytes(
            evidence_root,
            &prior_intent,
            &prior_manifest,
            &prior_receipt,
            &prior_normalization,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
        );
        assert_eq!(
            prior_bundle.bundle().baseline.state,
            ProbeBaselineState::Unavailable
        );
        let prior_evidence = store
            .persist_bundle(
                &prior_intent,
                &prior_manifest,
                &prior_receipt,
                &prior_bundle,
            )
            .expect("persist prior bundle");
        prior_evidence.prior_reference()
    }

    fn assert_incompatible_prior_environment(
        store: &DurableProbeEvidenceStore,
        evidence_root: &Path,
        profile: &ValidatedProbeNormalizerProfile,
        prior: &ProbePriorEvidenceReference,
    ) {
        let incompatible_intent = sealed_intent_for('3', '3', '2');
        let incompatible = store
            .prepare_normalization(&incompatible_intent, profile, Some(prior))
            .expect("bind explicit incompatible environment");
        let incompatible_capture = PreparedProbeCapture::try_new(
            &incompatible_intent,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
            false,
            &incompatible,
        )
        .expect("incompatible capture");
        let incompatible_receipt =
            success_receipt(&incompatible_intent, incompatible_capture.manifest());
        let incompatible_bundle = normalize_bytes(
            evidence_root,
            &incompatible_intent,
            incompatible_capture.manifest(),
            &incompatible_receipt,
            &incompatible,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
        );
        assert_eq!(
            incompatible_bundle.bundle().baseline.state,
            ProbeBaselineState::Incompatible
        );
    }

    fn assert_explicit_prior_binding_survives_reopen(root: &Path) {
        let evidence_root = root.join("baseline-evidence");
        let store = DurableProbeEvidenceStore::open(&evidence_root).expect("open baseline store");
        let profile = diagnostic_profile(DiagnosticParserVersion::TypescriptV1);
        let prior = retain_prior_bundle(&store, &evidence_root, &profile);

        let current_intent = sealed_intent_for('2', '2', '1');
        let current_normalization = store
            .prepare_normalization(&current_intent, &profile, Some(&prior))
            .expect("select explicit comparable baseline");
        let current_capture = PreparedProbeCapture::try_new(
            &current_intent,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
            false,
            &current_normalization,
        )
        .expect("current capture");
        let current_receipt = success_receipt(&current_intent, current_capture.manifest());
        let current_manifest = store
            .persist_completed_execution(&current_capture, &current_receipt)
            .expect("retain current execution");
        drop(store);

        let reopened = DurableProbeEvidenceStore::open(&evidence_root).expect("reopen current");
        let recovered_normalization = match reopened
            .recover(&current_intent)
            .expect("recover current execution")
            .expect("current execution retained")
        {
            RecoveredProbeEvidence::ReceiptRetained { normalization, .. } => normalization,
            RecoveredProbeEvidence::Complete { .. } => panic!("bundle was not retained yet"),
        };
        assert_eq!(recovered_normalization, current_normalization);

        let substituted_normalization = reopened
            .prepare_normalization(&current_intent, &profile, None)
            .expect("independently valid unavailable baseline");
        let substituted_bundle = normalize_bytes(
            &evidence_root,
            &current_intent,
            &current_manifest,
            &current_receipt,
            &substituted_normalization,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
        );
        assert_eq!(
            reopened
                .persist_bundle(
                    &current_intent,
                    &current_manifest,
                    &current_receipt,
                    &substituted_bundle,
                )
                .expect_err("reject substituted baseline"),
            conflict()
        );

        let current_bundle = normalize_bytes(
            &evidence_root,
            &current_intent,
            &current_manifest,
            &current_receipt,
            &recovered_normalization,
            TYPESCRIPT_DIAGNOSTIC,
            b"",
        );
        assert_eq!(
            current_bundle.bundle().baseline.state,
            ProbeBaselineState::Available
        );
        reopened
            .persist_bundle(
                &current_intent,
                &current_manifest,
                &current_receipt,
                &current_bundle,
            )
            .expect("persist exact selected baseline");
        assert_incompatible_prior_environment(&reopened, &evidence_root, &profile, &prior);
    }

    #[test]
    fn capture_is_exact_and_changed_bytes_or_authority_conflict() {
        let root = TempDir::new().expect("store root");
        let store = DurableProbeEvidenceStore::open(root.path()).expect("open store");
        let intent = sealed_intent('0');
        let prepared = capture(&store, &intent);
        let receipt = success_receipt(&intent, prepared.manifest());
        let manifest = store
            .persist_completed_execution(&prepared, &receipt)
            .expect("capture");
        assert_eq!(
            store
                .persist_completed_execution(&prepared, &receipt)
                .expect("exact replay"),
            manifest
        );
        for stream in [ProbeRawStream::Stdout, ProbeRawStream::Stderr] {
            let artifact = manifest.artifact(&stream).expect("stream artifact");
            let bytes = store
                .read_raw(&ProbeRawArtifactReadRequest::new(
                    &intent,
                    stream,
                    &artifact.artifact_ref,
                ))
                .expect("read raw");
            assert_eq!(
                bytes,
                if artifact.stream == ProbeRawStream::Stdout {
                    b"ok\n".as_slice()
                } else {
                    b"\xff!".as_slice()
                }
            );
        }
        let changed = PreparedProbeCapture::try_new(
            &intent,
            b"no\n",
            b"\xff!",
            false,
            &normalization_context(&store, &intent),
        )
        .expect("changed");
        let changed_receipt = success_receipt(&intent, changed.manifest());
        assert_eq!(
            store
                .persist_completed_execution(&changed, &changed_receipt)
                .expect_err("changed bytes"),
            conflict()
        );
        let changed_intent = sealed_intent('1');
        let changed_authority = capture(&store, &changed_intent);
        let changed_authority_receipt =
            success_receipt(&changed_intent, changed_authority.manifest());
        assert_eq!(
            store
                .persist_completed_execution(&changed_authority, &changed_authority_receipt)
                .expect_err("changed authority"),
            conflict()
        );
    }

    #[test]
    fn reopen_recovers_one_atomic_completed_execution_without_raw_output() {
        let root = TempDir::new().expect("store root");
        let intent = sealed_intent('0');
        let store = DurableProbeEvidenceStore::open(root.path()).expect("open store");
        let prepared = capture(&store, &intent);
        let receipt = success_receipt(&intent, prepared.manifest());
        assert_eq!(store.recover(&intent).expect("not retained"), None);
        let manifest = store
            .persist_completed_execution(&prepared, &receipt)
            .expect("retain completed execution");
        let mut changed_receipt = receipt.clone();
        changed_receipt.duration_millis = 11;
        assert_eq!(
            store
                .persist_completed_execution(&prepared, &changed_receipt)
                .expect_err("changed receipt"),
            conflict()
        );
        drop(store);
        let reopened = DurableProbeEvidenceStore::open(root.path()).expect("reopen receipt");
        assert_eq!(
            reopened.recover(&intent).expect("recover receipt"),
            Some(RecoveredProbeEvidence::ReceiptRetained {
                manifest: manifest.clone(),
                receipt: receipt.clone(),
                normalization: prepared.normalization.clone(),
            })
        );
        let bundle = normalized_bundle(root.path(), &intent, &manifest, &receipt, false);
        let substituted_profile_bundle =
            normalized_bundle(root.path(), &intent, &manifest, &receipt, true);
        assert_eq!(
            reopened
                .persist_bundle(&intent, &manifest, &receipt, &substituted_profile_bundle,)
                .expect_err("reject substituted profile"),
            conflict()
        );
        let evidence = reopened
            .persist_bundle(&intent, &manifest, &receipt, &bundle)
            .expect("persist bundle");
        assert_eq!(
            reopened
                .persist_bundle(&intent, &manifest, &receipt, &bundle)
                .expect("exact bundle replay"),
            evidence
        );
        assert_ne!(
            evidence.bundle_artifact_ref().digest,
            evidence.bundle().bundle().bundle_digest
        );
        assert_eq!(evidence.projection().candidates().len(), 1);
        assert_eq!(
            evidence.projection().summary().bundle_artifact_ref,
            *evidence.bundle_artifact_ref()
        );
        assert!(!evidence.projection().summary().summary.contains("ok\n"));
        assert_eq!(receipt.artifact_refs, manifest.artifact_references());
        assert!(
            !receipt
                .artifact_refs
                .contains(evidence.bundle_artifact_ref())
        );
        assert_explicit_prior_binding_survives_reopen(root.path());
        drop(reopened);

        let reopened = DurableProbeEvidenceStore::open(root.path()).expect("reopen bundle");
        assert_eq!(
            reopened.read_bundle(&intent).expect("read bundle"),
            Some(evidence.clone())
        );
        assert_eq!(
            reopened.recover(&intent).expect("recover complete"),
            Some(RecoveredProbeEvidence::Complete {
                manifest,
                receipt,
                normalization: prepared.normalization,
                evidence: Box::new(evidence),
            })
        );
        reopened
            .connection
            .lock()
            .expect("database lock")
            .execute(
                "UPDATE probe_bundle SET bundle_digest = ?1",
                [digest('f').0],
            )
            .expect("tamper semantic digest");
        assert_eq!(
            reopened
                .recover(&intent)
                .expect_err("semantic digest mismatch")
                .kind(),
            ProbeEvidenceStoreErrorKind::DigestMismatch
        );
    }

    #[test]
    fn bounds_digest_and_private_paths_fail_closed() {
        let intent = sealed_intent('0');
        let root = TempDir::new().expect("store root");
        let store = DurableProbeEvidenceStore::open(root.path()).expect("open store");
        assert_eq!(
            PreparedProbeCapture::try_new(
                &intent,
                &vec![b'x'; 1_025],
                b"",
                false,
                &normalization_context(&store, &intent),
            )
            .expect_err("oversized"),
            invalid_input()
        );
        let prepared = capture(&store, &intent);
        let receipt = success_receipt(&intent, prepared.manifest());
        let manifest = store
            .persist_completed_execution(&prepared, &receipt)
            .expect("capture");
        let stdout = manifest
            .artifact(&ProbeRawStream::Stdout)
            .expect("stdout artifact");
        fs::write(
            blob_path(&store.blob_root, &stdout.artifact_ref.digest).expect("blob path"),
            b"changed",
        )
        .expect("tamper blob");
        assert_eq!(
            store.recover(&intent).expect_err("digest mismatch").kind(),
            ProbeEvidenceStoreErrorKind::DigestMismatch
        );

        let private = TempDir::new().expect("permission root");
        let directory = private.path().join("evidence");
        let opened = DurableProbeEvidenceStore::open(&directory).expect("create private store");
        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join(DATABASE_FILE))
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(opened);
        let redirected = private.path().join("redirected");
        symlink(&directory, &redirected).expect("directory symlink");
        assert_eq!(
            DurableProbeEvidenceStore::open(&redirected)
                .expect_err("reject directory symlink")
                .kind(),
            ProbeEvidenceStoreErrorKind::Unavailable
        );
    }
}
