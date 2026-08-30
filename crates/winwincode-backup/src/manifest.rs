// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::AuditScope;
use winwincode_domain::Sha256Digest;

use crate::{BackupError, BackupErrorKind, MAX_SAFE_INTEGER};

const FORMAT: &str = "winwincode.backup-manifest.v1";
const MANIFEST_DOMAIN: &[u8] = b"winwincode.backup-manifest.v1";
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Stable identity for one immutable backup generation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BackupId(String);

impl BackupId {
    /// Builds a canonical `bkp_` identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Crockford identity.
    pub fn try_new(value: impl Into<String>) -> Result<Self, BackupError> {
        let value = value.into();
        if !canonical_id(&value, "bkp") {
            return Err(BackupError::invalid());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed set that must be present in every restorable generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupComponentKind {
    DeliveryState,
    AuditLedger,
    LeaseRegistry,
    UsageLedger,
    ReferenceCatalog,
    ArtifactObjects,
    SecretReferences,
}

impl BackupComponentKind {
    /// Every component required by one restorable generation.
    pub const REQUIRED: [Self; 7] = [
        Self::DeliveryState,
        Self::AuditLedger,
        Self::LeaseRegistry,
        Self::UsageLedger,
        Self::ReferenceCatalog,
        Self::ArtifactObjects,
        Self::SecretReferences,
    ];
}

/// Secret-free snapshot receipt emitted by one authoritative backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupComponentSnapshot {
    kind: BackupComponentKind,
    scope: AuditScope,
    consistency_cut_digest: Sha256Digest,
    checkpoint_digest: Sha256Digest,
    content_digest: Sha256Digest,
    record_count: u64,
    byte_count: u64,
}

impl BackupComponentSnapshot {
    /// Builds one backend snapshot receipt.
    ///
    /// # Errors
    ///
    /// Rejects malformed tenant, digest, or exact-count facts.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        kind: BackupComponentKind,
        scope: AuditScope,
        consistency_cut_digest: Sha256Digest,
        checkpoint_digest: Sha256Digest,
        content_digest: Sha256Digest,
        record_count: u64,
        byte_count: u64,
    ) -> Result<Self, BackupError> {
        validate_scope(&scope)?;
        validate_digest(&consistency_cut_digest)?;
        validate_digest(&checkpoint_digest)?;
        validate_digest(&content_digest)?;
        validate_count(record_count)?;
        validate_count(byte_count)?;
        Ok(Self {
            kind,
            scope,
            consistency_cut_digest,
            checkpoint_digest,
            content_digest,
            record_count,
            byte_count,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> BackupComponentKind {
        self.kind
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn consistency_cut_digest(&self) -> &Sha256Digest {
        &self.consistency_cut_digest
    }

    #[must_use]
    pub const fn checkpoint_digest(&self) -> &Sha256Digest {
        &self.checkpoint_digest
    }

    #[must_use]
    pub const fn content_digest(&self) -> &Sha256Digest {
        &self.content_digest
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

/// Exact component-level reference needed to prove the restored cut is joined.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupDependency {
    source: BackupComponentKind,
    target: BackupComponentKind,
    target_content_digest: Sha256Digest,
}

impl BackupDependency {
    /// Builds one dependency on the exact target snapshot.
    ///
    /// # Errors
    ///
    /// Rejects self references or malformed digests.
    pub fn try_new(
        source: BackupComponentKind,
        target: BackupComponentKind,
        target_content_digest: Sha256Digest,
    ) -> Result<Self, BackupError> {
        if source == target {
            return Err(BackupError::invalid());
        }
        validate_digest(&target_content_digest)?;
        Ok(Self {
            source,
            target,
            target_content_digest,
        })
    }

    #[must_use]
    pub const fn source(&self) -> BackupComponentKind {
        self.source
    }

    #[must_use]
    pub const fn target(&self) -> BackupComponentKind {
        self.target
    }

    #[must_use]
    pub const fn target_content_digest(&self) -> &Sha256Digest {
        &self.target_content_digest
    }
}

/// The only accepted backup manifest version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupManifest {
    backup_id: BackupId,
    scope: AuditScope,
    resident_region: String,
    captured_at_millis: u64,
    consistency_cut_digest: Sha256Digest,
    components: Vec<BackupComponentSnapshot>,
    dependencies: Vec<BackupDependency>,
    manifest_digest: Sha256Digest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestWire {
    format: String,
    backup_id: BackupId,
    scope: AuditScope,
    resident_region: String,
    captured_at_millis: u64,
    consistency_cut_digest: Sha256Digest,
    components: Vec<BackupComponentSnapshot>,
    dependencies: Vec<BackupDependency>,
    manifest_digest: Sha256Digest,
}

#[derive(Serialize)]
struct ManifestDigestWire<'a> {
    format: &'static str,
    backup_id: &'a BackupId,
    scope: &'a AuditScope,
    resident_region: &'a str,
    captured_at_millis: u64,
    consistency_cut_digest: &'a Sha256Digest,
    components: &'a [BackupComponentSnapshot],
    dependencies: &'a [BackupDependency],
}

impl BackupManifest {
    /// Assembles one complete cross-backend snapshot cut.
    ///
    /// # Errors
    ///
    /// Requires every component exactly once, one tenant and cut, and the
    /// complete canonical dependency graph.
    pub fn try_new(
        backup_id: BackupId,
        scope: AuditScope,
        resident_region: &str,
        captured_at_millis: u64,
        components: impl IntoIterator<Item = BackupComponentSnapshot>,
        dependencies: impl IntoIterator<Item = BackupDependency>,
    ) -> Result<Self, BackupError> {
        if !canonical_id(backup_id.as_str(), "bkp") {
            return Err(BackupError::invalid());
        }
        validate_scope(&scope)?;
        validate_region(resident_region)?;
        validate_time(captured_at_millis)?;
        let mut components = components.into_iter().collect::<Vec<_>>();
        components.sort_by_key(BackupComponentSnapshot::kind);
        validate_components(&scope, &components)?;
        let consistency_cut_digest = components
            .first()
            .ok_or_else(BackupError::incomplete)?
            .consistency_cut_digest
            .clone();
        let mut dependencies = dependencies.into_iter().collect::<Vec<_>>();
        dependencies.sort_by_key(|dependency| (dependency.source, dependency.target));
        validate_dependencies(&components, &dependencies)?;
        let manifest_digest = manifest_digest(
            &backup_id,
            &scope,
            resident_region,
            captured_at_millis,
            &consistency_cut_digest,
            &components,
            &dependencies,
        )?;
        Ok(Self {
            backup_id,
            scope,
            resident_region: resident_region.to_owned(),
            captured_at_millis,
            consistency_cut_digest,
            components,
            dependencies,
            manifest_digest,
        })
    }

    /// Decodes only the canonical v1 byte representation.
    ///
    /// # Errors
    ///
    /// Rejects unknown versions, non-canonical encoding, and any altered fact.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, BackupError> {
        let wire =
            serde_json::from_slice::<ManifestWire>(bytes).map_err(|_| BackupError::invalid())?;
        if wire.format != FORMAT {
            return Err(BackupError::new(BackupErrorKind::UnsupportedVersion));
        }
        let expected_digest = wire.manifest_digest.clone();
        let manifest = Self::try_new(
            wire.backup_id,
            wire.scope,
            &wire.resident_region,
            wire.captured_at_millis,
            wire.components,
            wire.dependencies,
        )?;
        if manifest.manifest_digest != expected_digest || manifest.encode_canonical()? != bytes {
            return Err(BackupError::integrity());
        }
        Ok(manifest)
    }

    /// Encodes the single canonical manifest representation.
    ///
    /// # Errors
    ///
    /// Returns an integrity failure if validated facts cannot be encoded.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, BackupError> {
        serde_json::to_vec(&ManifestWire {
            format: FORMAT.to_owned(),
            backup_id: self.backup_id.clone(),
            scope: self.scope.clone(),
            resident_region: self.resident_region.clone(),
            captured_at_millis: self.captured_at_millis,
            consistency_cut_digest: self.consistency_cut_digest.clone(),
            components: self.components.clone(),
            dependencies: self.dependencies.clone(),
            manifest_digest: self.manifest_digest.clone(),
        })
        .map_err(|_| BackupError::integrity())
    }

    #[must_use]
    pub const fn backup_id(&self) -> &BackupId {
        &self.backup_id
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub fn resident_region(&self) -> &str {
        &self.resident_region
    }

    #[must_use]
    pub const fn captured_at_millis(&self) -> u64 {
        self.captured_at_millis
    }

    #[must_use]
    pub const fn consistency_cut_digest(&self) -> &Sha256Digest {
        &self.consistency_cut_digest
    }

    #[must_use]
    pub fn components(&self) -> &[BackupComponentSnapshot] {
        &self.components
    }

    #[must_use]
    pub fn dependencies(&self) -> &[BackupDependency] {
        &self.dependencies
    }

    #[must_use]
    pub const fn manifest_digest(&self) -> &Sha256Digest {
        &self.manifest_digest
    }
}

fn validate_components(
    scope: &AuditScope,
    components: &[BackupComponentSnapshot],
) -> Result<(), BackupError> {
    if components.len() != BackupComponentKind::REQUIRED.len() {
        return Err(BackupError::incomplete());
    }
    let mut kinds = BTreeSet::new();
    let mut cut = None;
    for component in components {
        validate_scope(component.scope())?;
        validate_digest(component.consistency_cut_digest())?;
        validate_digest(component.checkpoint_digest())?;
        validate_digest(component.content_digest())?;
        validate_count(component.record_count())?;
        validate_count(component.byte_count())?;
        if component.scope() != scope {
            return Err(BackupError::tenant());
        }
        if !kinds.insert(component.kind()) {
            return Err(BackupError::conflict());
        }
        if cut
            .replace(component.consistency_cut_digest())
            .is_some_and(|previous| previous != component.consistency_cut_digest())
        {
            return Err(BackupError::integrity());
        }
    }
    if BackupComponentKind::REQUIRED
        .iter()
        .any(|kind| !kinds.contains(kind))
    {
        return Err(BackupError::incomplete());
    }
    Ok(())
}

fn validate_dependencies(
    components: &[BackupComponentSnapshot],
    dependencies: &[BackupDependency],
) -> Result<(), BackupError> {
    let indexed = components
        .iter()
        .map(|component| (component.kind(), component.content_digest()))
        .collect::<BTreeMap<_, _>>();
    let expected = [
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::AuditLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::LeaseRegistry,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::UsageLedger,
        ),
        (
            BackupComponentKind::DeliveryState,
            BackupComponentKind::ArtifactObjects,
        ),
        (
            BackupComponentKind::ReferenceCatalog,
            BackupComponentKind::SecretReferences,
        ),
    ];
    if dependencies.len() != expected.len() {
        return Err(BackupError::incomplete());
    }
    let mut seen = BTreeSet::new();
    for dependency in dependencies {
        validate_digest(dependency.target_content_digest())?;
        let edge = (dependency.source(), dependency.target());
        if !expected.contains(&edge) || !seen.insert(edge) {
            return Err(BackupError::conflict());
        }
        if indexed.get(&dependency.target()).copied() != Some(dependency.target_content_digest()) {
            return Err(BackupError::integrity());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn manifest_digest(
    backup_id: &BackupId,
    scope: &AuditScope,
    resident_region: &str,
    captured_at_millis: u64,
    cut: &Sha256Digest,
    components: &[BackupComponentSnapshot],
    dependencies: &[BackupDependency],
) -> Result<Sha256Digest, BackupError> {
    let bytes = serde_json::to_vec(&ManifestDigestWire {
        format: FORMAT,
        backup_id,
        scope,
        resident_region,
        captured_at_millis,
        consistency_cut_digest: cut,
        components,
        dependencies,
    })
    .map_err(|_| BackupError::integrity())?;
    let mut hash = Sha256::new();
    hash.update(MANIFEST_DOMAIN);
    hash.update([0]);
    hash.update(bytes);
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

pub(crate) fn validate_digest(digest: &Sha256Digest) -> Result<(), BackupError> {
    let valid = digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(BackupError::invalid())
    }
}

fn validate_scope(scope: &AuditScope) -> Result<(), BackupError> {
    let valid = match scope {
        AuditScope::Organization { organization_id } => canonical_id(&organization_id.0, "org"),
        AuditScope::Workspace {
            organization_id,
            workspace_id,
        } => canonical_id(&organization_id.0, "org") && canonical_id(&workspace_id.0, "wsp"),
        AuditScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            canonical_id(&organization_id.0, "org")
                && canonical_id(&workspace_id.0, "wsp")
                && canonical_id(&project_id.0, "prj")
        }
        AuditScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            canonical_id(&organization_id.0, "org")
                && canonical_id(&workspace_id.0, "wsp")
                && canonical_id(&project_id.0, "prj")
                && canonical_id(&repository_id.0, "rep")
        }
    };
    if valid {
        Ok(())
    } else {
        Err(BackupError::invalid())
    }
}

fn validate_region(region: &str) -> Result<(), BackupError> {
    let valid = (2..=64).contains(&region.len())
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && region
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && region
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(BackupError::invalid())
    }
}

fn validate_time(value: u64) -> Result<(), BackupError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(BackupError::invalid())
    } else {
        Ok(())
    }
}

fn validate_count(value: u64) -> Result<(), BackupError> {
    if value > MAX_SAFE_INTEGER {
        Err(BackupError::invalid())
    } else {
        Ok(())
    }
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(&format!("{prefix}_"))
        .is_some_and(|suffix| {
            suffix.len() == 26 && suffix.bytes().all(|byte| CROCKFORD.contains(&byte))
        })
}
