// SPDX-License-Identifier: Apache-2.0

//! Versioned test-asset manifests and bounded Delivery evidence bindings.
//!
//! A manifest is an artifact owned outside the Control Plane. This module only
//! derives compact references, evidence bindings, and invalidation facts from
//! a validated manifest; it does not introduce another Delivery aggregate.

use std::{
    collections::HashSet,
    error::Error,
    fmt::{self, Write as _},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_delivery::domain::{
    DeliveryVerdict, DeliveryVerdictId, EvidenceId, EvidenceRef, EvidenceRefType,
};

pub const TEST_ASSET_MANIFEST_SCHEMA_VERSION: u8 = 1;

const MAX_TEXT_LENGTH: usize = 4_096;
const MAX_ASSETS: usize = 10_000;
const MAX_REQUIREMENTS: usize = 1_000;

/// Whether a test is an accepted product authority or a candidate proposal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestAssetAuthority {
    Canonical,
    Candidate,
}

/// Whether an Executor may update the asset through a manifest transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestAssetMutability {
    Protected,
    ExecutorManaged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestAssetLifecycle {
    Active,
    Retired,
}

/// Only canonical assets can carry an automatic requirement gate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestAssetGate {
    Advisory,
    RequirementBlocking,
}

/// One content-addressed test asset in a versioned manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAsset {
    pub id: String,
    pub owner: String,
    pub scope: Vec<String>,
    pub purpose: String,
    pub authority: TestAssetAuthority,
    pub mutability: TestAssetMutability,
    pub lifecycle: TestAssetLifecycle,
    pub gate: TestAssetGate,
    pub requirement_refs: Vec<String>,
    pub source_path: String,
    pub content_sha256: String,
}

impl TestAsset {
    /// Candidate tests are advisory unless they are promoted into the
    /// canonical manifest by its owner.
    #[must_use]
    pub fn candidate(
        id: impl Into<String>,
        owner: impl Into<String>,
        scope: Vec<String>,
        purpose: impl Into<String>,
        source_path: impl Into<String>,
        content_sha256: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            owner: owner.into(),
            scope,
            purpose: purpose.into(),
            authority: TestAssetAuthority::Candidate,
            mutability: TestAssetMutability::ExecutorManaged,
            lifecycle: TestAssetLifecycle::Active,
            gate: TestAssetGate::Advisory,
            requirement_refs: Vec::new(),
            source_path: source_path.into(),
            content_sha256: content_sha256.into(),
        }
    }

    #[must_use]
    pub const fn blocks_delivery(&self) -> bool {
        matches!(
            (self.authority, self.lifecycle, self.gate),
            (
                TestAssetAuthority::Canonical,
                TestAssetLifecycle::Active,
                TestAssetGate::RequirementBlocking
            )
        )
    }

    fn executor_protected(&self) -> bool {
        self.authority == TestAssetAuthority::Canonical
            || self.mutability == TestAssetMutability::Protected
    }
}

/// Versioned artifact describing the exact test assets for one candidate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAssetManifest {
    pub schema_version: u8,
    pub id: String,
    pub revision: u64,
    pub candidate_ref: String,
    pub source_commit: String,
    pub assets: Vec<TestAsset>,
}

impl TestAssetManifest {
    /// Validates the current manifest contract and its deterministic ordering.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities or
    /// digests, duplicate entries, non-canonical ordering, and candidate tests
    /// that attempt to become automatic gates.
    pub fn validate(&self) -> Result<(), TestAssetManifestError> {
        if self.schema_version != TEST_ASSET_MANIFEST_SCHEMA_VERSION {
            return Err(manifest_error(
                TestAssetManifestErrorCode::UnsupportedSchemaVersion,
                "schemaVersion",
                "unsupported TestAsset manifest schema version",
            ));
        }
        portable_identifier(&self.id, "id")?;
        if self.revision == 0 {
            return Err(manifest_error(
                TestAssetManifestErrorCode::InvalidValue,
                "revision",
                "manifest revision must be positive",
            ));
        }
        bounded_text(&self.candidate_ref, "candidateRef")?;
        git_commit(&self.source_commit, "sourceCommit")?;
        if self.assets.len() > MAX_ASSETS {
            return Err(manifest_error(
                TestAssetManifestErrorCode::InvalidValue,
                "assets",
                "manifest contains too many assets",
            ));
        }

        let mut asset_ids = HashSet::with_capacity(self.assets.len());
        let mut previous_id: Option<&str> = None;
        for (index, asset) in self.assets.iter().enumerate() {
            let path = format!("assets[{index}]");
            validate_asset(asset, &path)?;
            if !asset_ids.insert(asset.id.as_str()) {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::DuplicateAsset,
                    format!("{path}.id"),
                    "asset id is duplicated",
                ));
            }
            if previous_id.is_some_and(|previous| previous >= asset.id.as_str()) {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::InvalidValue,
                    "assets",
                    "assets must be sorted by id",
                ));
            }
            previous_id = Some(asset.id.as_str());
        }
        Ok(())
    }

    /// Returns the compact identity retained by evidence and verdict facts.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is not valid.
    pub fn artifact_ref(&self) -> Result<TestAssetManifestRef, TestAssetManifestError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|error| {
            manifest_error(
                TestAssetManifestErrorCode::InvalidValue,
                "manifest",
                format!("manifest could not be encoded: {error}"),
            )
        })?;
        Ok(TestAssetManifestRef {
            id: self.id.clone(),
            revision: self.revision,
            digest_sha256: sha256(&encoded),
        })
    }

    /// Binds one existing canonical test `EvidenceRef` to an exact manifest
    /// revision and asset content hash.
    ///
    /// # Errors
    ///
    /// Rejects non-test Evidence, another candidate, or a missing/retired
    /// asset.
    pub fn bind_evidence(
        &self,
        evidence: &EvidenceRef,
        asset_id: &str,
    ) -> Result<TestAssetEvidenceBinding, TestAssetManifestError> {
        let manifest_ref = self.artifact_ref()?;
        if evidence.evidence_type != EvidenceRefType::Test {
            return Err(manifest_error(
                TestAssetManifestErrorCode::EvidenceMismatch,
                "evidence.type",
                "only test EvidenceRef values can bind to TestAsset",
            ));
        }
        if evidence.candidate_ref != self.candidate_ref {
            return Err(manifest_error(
                TestAssetManifestErrorCode::EvidenceMismatch,
                "evidence.candidateRef",
                "EvidenceRef belongs to another candidate",
            ));
        }
        let asset = self
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .ok_or_else(|| {
                manifest_error(
                    TestAssetManifestErrorCode::EvidenceMismatch,
                    "assetId",
                    "EvidenceRef names an unknown TestAsset",
                )
            })?;
        if asset.lifecycle != TestAssetLifecycle::Active {
            return Err(manifest_error(
                TestAssetManifestErrorCode::EvidenceMismatch,
                "assetId",
                "retired TestAsset cannot supply current Evidence",
            ));
        }

        Ok(TestAssetEvidenceBinding {
            evidence_ref_id: evidence.id.clone(),
            candidate_ref: self.candidate_ref.clone(),
            manifest_ref,
            asset_id: asset.id.clone(),
            asset_content_sha256: asset.content_sha256.clone(),
            blocks_delivery: asset.blocks_delivery(),
        })
    }
}

/// Compact external artifact identity stored by the Control Plane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAssetManifestRef {
    pub id: String,
    pub revision: u64,
    pub digest_sha256: String,
}

/// Exact link from an existing Delivery `EvidenceRef` to one `TestAsset`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAssetEvidenceBinding {
    evidence_ref_id: EvidenceId,
    candidate_ref: String,
    manifest_ref: TestAssetManifestRef,
    asset_id: String,
    asset_content_sha256: String,
    blocks_delivery: bool,
}

impl TestAssetEvidenceBinding {
    #[must_use]
    pub fn evidence_ref_id(&self) -> &EvidenceId {
        &self.evidence_ref_id
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn manifest_ref(&self) -> &TestAssetManifestRef {
        &self.manifest_ref
    }

    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub fn asset_content_sha256(&self) -> &str {
        &self.asset_content_sha256
    }

    #[must_use]
    pub const fn blocks_delivery(&self) -> bool {
        self.blocks_delivery
    }
}

/// Identity retained when a verdict consumes test-asset Evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAssetVerdictBinding {
    verdict_id: DeliveryVerdictId,
    candidate_ref: String,
    manifest_ref: TestAssetManifestRef,
    evidence_ref_ids: Vec<EvidenceId>,
}

impl TestAssetVerdictBinding {
    /// Binds a verdict only to test Evidence that it actually cites.
    ///
    /// # Errors
    ///
    /// Rejects mismatched manifest/candidate identities, duplicate Evidence,
    /// or bindings not cited by the verdict.
    pub fn new(
        verdict: &DeliveryVerdict,
        manifest: &TestAssetManifest,
        evidence: &[TestAssetEvidenceBinding],
    ) -> Result<Self, TestAssetManifestError> {
        let manifest_ref = manifest.artifact_ref()?;
        if verdict.candidate_ref != manifest.candidate_ref {
            return Err(manifest_error(
                TestAssetManifestErrorCode::VerdictMismatch,
                "verdict.candidateRef",
                "verdict belongs to another candidate",
            ));
        }
        if evidence.is_empty() {
            return Err(manifest_error(
                TestAssetManifestErrorCode::VerdictMismatch,
                "evidence",
                "test-asset verdict binding requires cited test Evidence",
            ));
        }

        let cited: HashSet<&EvidenceId> = verdict
            .criteria
            .iter()
            .flat_map(|criterion| criterion.evidence_refs.iter())
            .collect();
        let mut evidence_ref_ids = Vec::with_capacity(evidence.len());
        let mut unique = HashSet::with_capacity(evidence.len());
        for (index, binding) in evidence.iter().enumerate() {
            if binding.candidate_ref != manifest.candidate_ref
                || binding.manifest_ref != manifest_ref
            {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::EvidenceMismatch,
                    format!("evidence[{index}]"),
                    "test Evidence binding belongs to another manifest or candidate",
                ));
            }
            let asset = manifest
                .assets
                .iter()
                .find(|asset| asset.id == binding.asset_id)
                .ok_or_else(|| {
                    manifest_error(
                        TestAssetManifestErrorCode::EvidenceMismatch,
                        format!("evidence[{index}].assetId"),
                        "test Evidence binding names an unknown TestAsset",
                    )
                })?;
            if asset.lifecycle != TestAssetLifecycle::Active
                || binding.asset_content_sha256 != asset.content_sha256
                || binding.blocks_delivery != asset.blocks_delivery()
            {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::EvidenceMismatch,
                    format!("evidence[{index}]"),
                    "test Evidence binding does not match the current TestAsset",
                ));
            }
            if !cited.contains(&binding.evidence_ref_id) {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::VerdictMismatch,
                    format!("evidence[{index}].evidenceRefId"),
                    "test Evidence binding is not cited by the verdict",
                ));
            }
            if !unique.insert(binding.evidence_ref_id.clone()) {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::EvidenceMismatch,
                    "evidence",
                    "test Evidence binding is duplicated",
                ));
            }
            evidence_ref_ids.push(binding.evidence_ref_id.clone());
        }
        evidence_ref_ids.sort_by(|left, right| left.0.cmp(&right.0));

        Ok(Self {
            verdict_id: verdict.id.clone(),
            candidate_ref: verdict.candidate_ref.clone(),
            manifest_ref,
            evidence_ref_ids,
        })
    }

    #[must_use]
    pub fn verdict_id(&self) -> &DeliveryVerdictId {
        &self.verdict_id
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn manifest_ref(&self) -> &TestAssetManifestRef {
        &self.manifest_ref
    }

    #[must_use]
    pub fn evidence_ref_ids(&self) -> &[EvidenceId] {
        &self.evidence_ref_ids
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestAssetManifestActor {
    ControlPlane,
    Executor,
}

/// Validates a replacement of one manifest revision.
///
/// # Errors
///
/// Rejects identity/revision jumps and every Executor create, update, delete,
/// retire, or promotion that touches canonical or protected assets.
pub fn validate_manifest_transition(
    previous: &TestAssetManifest,
    next: &TestAssetManifest,
    actor: TestAssetManifestActor,
) -> Result<(), TestAssetManifestError> {
    previous.validate()?;
    next.validate()?;
    if previous.id != next.id {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidTransition,
            "id",
            "manifest identity cannot change",
        ));
    }
    if next.revision != previous.revision.saturating_add(1) {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidTransition,
            "revision",
            "manifest revision must increase by exactly one",
        ));
    }
    if actor == TestAssetManifestActor::Executor {
        let previous_by_id: std::collections::HashMap<&str, &TestAsset> = previous
            .assets
            .iter()
            .map(|asset| (asset.id.as_str(), asset))
            .collect();
        let next_by_id: std::collections::HashMap<&str, &TestAsset> = next
            .assets
            .iter()
            .map(|asset| (asset.id.as_str(), asset))
            .collect();
        let asset_ids: HashSet<&str> = previous_by_id
            .keys()
            .chain(next_by_id.keys())
            .copied()
            .collect();
        for asset_id in asset_ids {
            let old = previous_by_id.get(asset_id).copied();
            let new = next_by_id.get(asset_id).copied();
            let protected = old.is_some_and(TestAsset::executor_protected)
                || new.is_some_and(TestAsset::executor_protected);
            if protected && old != new {
                return Err(manifest_error(
                    TestAssetManifestErrorCode::ExecutorModificationDenied,
                    format!("assets[{asset_id}]"),
                    "Executor cannot modify canonical or protected TestAsset",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestAssetVerdictInvalidationReason {
    CandidateChanged,
    ManifestChanged,
    CandidateAndManifestChanged,
}

/// Bounded fact saved when the current candidate or manifest no longer agrees
/// with the identities used for a verdict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestAssetVerdictInvalidation {
    verdict_id: DeliveryVerdictId,
    reason: TestAssetVerdictInvalidationReason,
    bound_candidate_ref: String,
    current_candidate_ref: String,
    bound_manifest_ref: TestAssetManifestRef,
    current_manifest_ref: TestAssetManifestRef,
    invalidated_at_millis: u64,
}

impl TestAssetVerdictInvalidation {
    #[must_use]
    pub fn verdict_id(&self) -> &DeliveryVerdictId {
        &self.verdict_id
    }

    #[must_use]
    pub const fn reason(&self) -> TestAssetVerdictInvalidationReason {
        self.reason
    }

    #[must_use]
    pub fn bound_candidate_ref(&self) -> &str {
        &self.bound_candidate_ref
    }

    #[must_use]
    pub fn current_candidate_ref(&self) -> &str {
        &self.current_candidate_ref
    }

    #[must_use]
    pub fn bound_manifest_ref(&self) -> &TestAssetManifestRef {
        &self.bound_manifest_ref
    }

    #[must_use]
    pub fn current_manifest_ref(&self) -> &TestAssetManifestRef {
        &self.current_manifest_ref
    }

    #[must_use]
    pub const fn invalidated_at_millis(&self) -> u64 {
        self.invalidated_at_millis
    }
}

/// Produces a compact invalidation fact instead of retaining the manifest in
/// canonical Delivery state.
///
/// # Errors
///
/// Returns an error when the current candidate or manifest is malformed.
pub fn detect_verdict_invalidation(
    binding: &TestAssetVerdictBinding,
    current_candidate_ref: &str,
    current_manifest: &TestAssetManifest,
    invalidated_at_millis: u64,
) -> Result<Option<TestAssetVerdictInvalidation>, TestAssetManifestError> {
    bounded_text(current_candidate_ref, "currentCandidateRef")?;
    if invalidated_at_millis == 0 {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            "invalidatedAtMillis",
            "invalidation time must be positive",
        ));
    }
    let current_manifest_ref = current_manifest.artifact_ref()?;
    let candidate_changed = binding.candidate_ref != current_candidate_ref;
    let manifest_changed = binding.manifest_ref != current_manifest_ref;
    let reason = match (candidate_changed, manifest_changed) {
        (false, false) => return Ok(None),
        (true, false) => TestAssetVerdictInvalidationReason::CandidateChanged,
        (false, true) => TestAssetVerdictInvalidationReason::ManifestChanged,
        (true, true) => TestAssetVerdictInvalidationReason::CandidateAndManifestChanged,
    };
    Ok(Some(TestAssetVerdictInvalidation {
        verdict_id: binding.verdict_id.clone(),
        reason,
        bound_candidate_ref: binding.candidate_ref.clone(),
        current_candidate_ref: current_candidate_ref.to_owned(),
        bound_manifest_ref: binding.manifest_ref.clone(),
        current_manifest_ref,
        invalidated_at_millis,
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestAssetManifestErrorCode {
    UnsupportedSchemaVersion,
    InvalidValue,
    DuplicateAsset,
    InvalidTransition,
    ExecutorModificationDenied,
    EvidenceMismatch,
    VerdictMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestAssetManifestError {
    code: TestAssetManifestErrorCode,
    path: String,
    message: String,
}

impl TestAssetManifestError {
    #[must_use]
    pub const fn code(&self) -> TestAssetManifestErrorCode {
        self.code
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TestAssetManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl Error for TestAssetManifestError {}

fn validate_asset(asset: &TestAsset, path: &str) -> Result<(), TestAssetManifestError> {
    portable_identifier(&asset.id, &format!("{path}.id"))?;
    bounded_text(&asset.owner, &format!("{path}.owner"))?;
    bounded_text(&asset.purpose, &format!("{path}.purpose"))?;
    portable_path(&asset.source_path, &format!("{path}.sourcePath"))?;
    lowercase_sha256(&asset.content_sha256, &format!("{path}.contentSha256"))?;
    sorted_unique_texts(&asset.scope, &format!("{path}.scope"))?;
    sorted_unique_texts(&asset.requirement_refs, &format!("{path}.requirementRefs"))?;
    if asset.scope.is_empty() {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            format!("{path}.scope"),
            "asset scope must not be empty",
        ));
    }
    if asset.requirement_refs.len() > MAX_REQUIREMENTS {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            format!("{path}.requirementRefs"),
            "asset has too many requirement references",
        ));
    }
    if asset.authority == TestAssetAuthority::Candidate && asset.gate != TestAssetGate::Advisory {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            format!("{path}.gate"),
            "candidate TestAsset must remain advisory until canonical promotion",
        ));
    }
    if asset.gate == TestAssetGate::RequirementBlocking && asset.requirement_refs.is_empty() {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            format!("{path}.requirementRefs"),
            "blocking TestAsset must cite at least one requirement",
        ));
    }
    Ok(())
}

fn sorted_unique_texts(values: &[String], path: &str) -> Result<(), TestAssetManifestError> {
    let mut previous: Option<&str> = None;
    for (index, value) in values.iter().enumerate() {
        bounded_text(value, &format!("{path}[{index}]"))?;
        if previous.is_some_and(|previous| previous >= value) {
            return Err(manifest_error(
                TestAssetManifestErrorCode::InvalidValue,
                path,
                "values must be sorted and unique",
            ));
        }
        previous = Some(value);
    }
    Ok(())
}

fn bounded_text(value: &str, path: &str) -> Result<(), TestAssetManifestError> {
    if value.is_empty()
        || value.len() > MAX_TEXT_LENGTH
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            path,
            "value must be non-empty, bounded, trimmed, and printable",
        ));
    }
    Ok(())
}

fn portable_identifier(value: &str, path: &str) -> Result<(), TestAssetManifestError> {
    bounded_text(value, path)?;
    if value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            path,
            "value is not a portable identifier",
        ));
    }
    Ok(())
}

fn portable_path(value: &str, path: &str) -> Result<(), TestAssetManifestError> {
    bounded_text(value, path)?;
    if value.starts_with('/')
        || value.starts_with("../")
        || value.contains("/../")
        || value.contains('\\')
    {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            path,
            "source path must be repository-relative",
        ));
    }
    Ok(())
}

fn git_commit(value: &str, path: &str) -> Result<(), TestAssetManifestError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            path,
            "source commit must be a lowercase Git object id",
        ));
    }
    Ok(())
}

fn lowercase_sha256(value: &str, path: &str) -> Result<(), TestAssetManifestError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(manifest_error(
            TestAssetManifestErrorCode::InvalidValue,
            path,
            "value must be a lowercase SHA-256 digest",
        ));
    }
    Ok(())
}

fn sha256(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn manifest_error(
    code: TestAssetManifestErrorCode,
    path: impl Into<String>,
    message: impl Into<String>,
) -> TestAssetManifestError {
    TestAssetManifestError {
        code,
        path: path.into(),
        message: message.into(),
    }
}
