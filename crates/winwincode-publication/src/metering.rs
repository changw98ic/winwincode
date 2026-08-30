// SPDX-License-Identifier: Apache-2.0

//! Immutable Publication provider-write receipts for enterprise reconciliation.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    DeliveryId, Instant, OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId,
    UserId, WorkspaceId,
};
use winwincode_storage::{ProductStateStorage, StateMutation, StorageError, StoredState};

use crate::{PublicationOperation, RepositoryPolicyScope};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PAGE_SIZE: u64 = 200;
const MAX_SCAN_ROWS: u64 = 1_000;
const ATTRIBUTION_SCHEMA: &str = "winwincode.publication-enterprise-attribution.v1";
const SOURCE_SCHEMA: &str = "winwincode.publication-enterprise-source.v1";
const CATALOG_SCHEMA: &str = "winwincode.publication-enterprise-source-catalog.v1";
const ATTRIBUTION_PREFIX: &str = "publication-enterprise-attribution:";
const SOURCE_IDENTITY_PREFIX: &str = "publication-enterprise-source-identity:";
const SOURCE_ENTRY_PREFIX: &str = "publication-enterprise-source-entry:";
const SOURCE_CATALOG_STREAM: &str = "publication-enterprise-source-catalog";

/// Full immutable business attribution captured with the Publication intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationEnterpriseAttribution {
    #[serde(rename = "organizationId")]
    organization: OrganizationId,
    #[serde(rename = "workspaceId")]
    workspace: WorkspaceId,
    #[serde(rename = "projectId")]
    project: ProjectId,
    #[serde(rename = "repositoryId")]
    repository: RepositoryId,
    #[serde(rename = "deliveryId")]
    delivery: DeliveryId,
    #[serde(rename = "productSessionId")]
    product_session: ProductSessionId,
    #[serde(rename = "userId")]
    user: UserId,
}

impl PublicationEnterpriseAttribution {
    /// Seals the exact repository ancestry, Delivery, `ProductSession`, and User.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities or a Delivery outside the supplied facts.
    pub fn try_new(
        scope: &RepositoryPolicyScope,
        delivery_id: DeliveryId,
        product_session_id: ProductSessionId,
        user_id: UserId,
    ) -> Result<Self, PublicationMeteringError> {
        let attribution = Self {
            organization: scope.organization_id().clone(),
            workspace: scope.workspace_id().clone(),
            project: scope.project_id().clone(),
            repository: scope.repository_id().clone(),
            delivery: delivery_id,
            product_session: product_session_id,
            user: user_id,
        };
        validate_attribution(&attribution)?;
        Ok(attribution)
    }

    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        &self.organization
    }

    #[must_use]
    pub const fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace
    }

    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project
    }

    #[must_use]
    pub const fn repository_id(&self) -> &RepositoryId {
        &self.repository
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery
    }

    #[must_use]
    pub const fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session
    }

    #[must_use]
    pub const fn user_id(&self) -> &UserId {
        &self.user
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAttribution {
    schema: String,
    publication_id: PublicationId,
    attribution: PublicationEnterpriseAttribution,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceCatalog {
    schema: String,
    revision: u64,
    entry_count: u64,
}

/// One immutable provider mutation that performed a confirmed remote write.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationMeteringSourceEntry {
    schema: String,
    pub sequence: u64,
    pub source_digest: String,
    pub publication_id: PublicationId,
    pub operation_key: String,
    pub request_sha256: String,
    pub remote_write_performed: bool,
    pub attribution: PublicationEnterpriseAttribution,
    pub occurred_at: Instant,
}

/// Exact optional filters for bounded source reconciliation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationMeteringFilter {
    pub organization_id: Option<OrganizationId>,
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub repository_id: Option<RepositoryId>,
    pub delivery_id: Option<DeliveryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub user_id: Option<UserId>,
    pub publication_id: Option<PublicationId>,
}

/// Cursor bound to one filter and immutable catalog upper bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationMeteringCursor {
    filter_digest: String,
    snapshot_sequence: u64,
    after_sequence: u64,
}

impl PublicationMeteringCursor {
    #[must_use]
    pub fn filter_digest(&self) -> &str {
        &self.filter_digest
    }

    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One bounded fixed-snapshot source page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationMeteringSourcePage {
    pub snapshot_sequence: u64,
    pub entries: Vec<PublicationMeteringSourceEntry>,
    pub next: Option<PublicationMeteringCursor>,
}

/// Stable Publication metering failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationMeteringErrorKind {
    InvalidInput,
    SourceConflict,
    CorruptState,
    Storage,
}

/// Bounded source-catalog error without provider content.
#[derive(Clone, Copy, Debug)]
pub struct PublicationMeteringError {
    kind: PublicationMeteringErrorKind,
}

impl PublicationMeteringError {
    const fn new(kind: PublicationMeteringErrorKind) -> Self {
        Self { kind }
    }

    const fn invalid() -> Self {
        Self::new(PublicationMeteringErrorKind::InvalidInput)
    }

    const fn conflict() -> Self {
        Self::new(PublicationMeteringErrorKind::SourceConflict)
    }

    const fn corrupt() -> Self {
        Self::new(PublicationMeteringErrorKind::CorruptState)
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationMeteringErrorKind {
        self.kind
    }
}

impl fmt::Display for PublicationMeteringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Publication enterprise metering source is unavailable")
    }
}

impl std::error::Error for PublicationMeteringError {}

impl From<StorageError> for PublicationMeteringError {
    fn from(_error: StorageError) -> Self {
        Self::new(PublicationMeteringErrorKind::Storage)
    }
}

/// Read-only source catalog over the canonical product-state database.
pub struct PublicationMeteringLedger<'storage> {
    storage: &'storage dyn ProductStateStorage,
}

impl<'storage> PublicationMeteringLedger<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Loads the immutable enterprise attribution sealed with one Publication intent.
    ///
    /// # Errors
    ///
    /// Rejects a missing or changed attribution record, malformed identity, or
    /// unavailable product-state storage.
    pub fn attribution(
        &self,
        publication_id: &PublicationId,
    ) -> Result<PublicationEnterpriseAttribution, PublicationMeteringError> {
        load_attribution(self.storage, publication_id)
    }

    /// Reads one fixed-snapshot bounded source page.
    ///
    /// Each call inspects at most 1,000 immutable source rows. A selective
    /// filter can therefore return an empty page with a continuation cursor.
    ///
    /// # Errors
    ///
    /// Rejects malformed filters, changed cursors, corrupt facts, or storage
    /// failures.
    pub fn scan_sources(
        &self,
        filter: &PublicationMeteringFilter,
        cursor: Option<&PublicationMeteringCursor>,
        limit: u64,
    ) -> Result<PublicationMeteringSourcePage, PublicationMeteringError> {
        validate_filter(filter)?;
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(PublicationMeteringError::invalid());
        }
        let filter_digest = digest(filter)?;
        let (catalog, _) = load_catalog(self.storage)?;
        let (snapshot_sequence, after_sequence) = cursor.map_or_else(
            || Ok((catalog.entry_count, 0)),
            |cursor| cursor_position(cursor, &filter_digest, &catalog),
        )?;
        let (entries, last_scanned) = scan_entries(
            self.storage,
            filter,
            snapshot_sequence,
            after_sequence,
            limit,
        )?;
        let next = (last_scanned < snapshot_sequence).then_some(PublicationMeteringCursor {
            filter_digest,
            snapshot_sequence,
            after_sequence: last_scanned,
        });
        Ok(PublicationMeteringSourcePage {
            snapshot_sequence,
            entries,
            next,
        })
    }
}

pub(crate) fn attribution_mutation(
    publication_id: &PublicationId,
    attribution: &PublicationEnterpriseAttribution,
) -> Result<StateMutation, PublicationMeteringError> {
    validate_attribution(attribution)?;
    let stored = StoredAttribution {
        schema: ATTRIBUTION_SCHEMA.to_owned(),
        publication_id: publication_id.clone(),
        attribution: attribution.clone(),
    };
    StateMutation::new(attribution_stream(publication_id)?, 0, encode(&stored)?).map_err(Into::into)
}

pub(crate) fn validate_stored_attribution(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
    expected: &PublicationEnterpriseAttribution,
) -> Result<(), PublicationMeteringError> {
    let actual = load_attribution(storage, publication_id)?;
    if &actual != expected {
        return Err(PublicationMeteringError::conflict());
    }
    Ok(())
}

pub(crate) fn source_mutations(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
    operation: &PublicationOperation,
    occurred_at: Instant,
) -> Result<Vec<StateMutation>, PublicationMeteringError> {
    operation
        .validate()
        .map_err(|_| PublicationMeteringError::corrupt())?;
    let attribution = load_attribution(storage, publication_id)?;
    let identity_stream = source_identity_stream(publication_id, operation)?;
    if let Some(stored) = storage.load_state(&identity_stream)? {
        let entry = decode_entry(&stored, None)?;
        if entry.publication_id != *publication_id
            || entry.operation_key != operation.operation_key()
            || entry.request_sha256 != operation.request_sha256()
            || entry.attribution != attribution
            || entry.occurred_at != occurred_at
        {
            return Err(PublicationMeteringError::conflict());
        }
        return Ok(Vec::new());
    }
    let (mut catalog, expected_catalog_revision) = load_catalog(storage)?;
    let sequence = catalog
        .entry_count
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(PublicationMeteringError::corrupt)?;
    let mut entry = PublicationMeteringSourceEntry {
        schema: SOURCE_SCHEMA.to_owned(),
        sequence,
        source_digest: String::new(),
        publication_id: publication_id.clone(),
        operation_key: operation.operation_key().to_owned(),
        request_sha256: operation.request_sha256().to_owned(),
        remote_write_performed: true,
        attribution,
        occurred_at,
    };
    entry.source_digest = source_digest(&entry)?;
    validate_entry(&entry, sequence)?;
    let payload = encode(&entry)?;
    catalog.revision = sequence;
    catalog.entry_count = sequence;
    Ok(vec![
        StateMutation::new(identity_stream, 0, payload.clone())?,
        StateMutation::new(entry_stream(sequence)?, 0, payload)?,
        StateMutation::new(
            SOURCE_CATALOG_STREAM.to_owned(),
            expected_catalog_revision,
            encode(&catalog)?,
        )?,
    ])
}

fn load_attribution(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
) -> Result<PublicationEnterpriseAttribution, PublicationMeteringError> {
    let stored = storage
        .load_state(&attribution_stream(publication_id)?)?
        .ok_or_else(PublicationMeteringError::corrupt)?;
    if stored.revision != 1 {
        return Err(PublicationMeteringError::corrupt());
    }
    let value: StoredAttribution = decode(&stored.payload)?;
    if encode(&value)? != stored.payload
        || value.schema != ATTRIBUTION_SCHEMA
        || value.publication_id != *publication_id
    {
        return Err(PublicationMeteringError::corrupt());
    }
    validate_attribution(&value.attribution)?;
    Ok(value.attribution)
}

fn load_catalog(
    storage: &dyn ProductStateStorage,
) -> Result<(SourceCatalog, u64), PublicationMeteringError> {
    let Some(stored) = storage.load_state(SOURCE_CATALOG_STREAM)? else {
        return Ok((
            SourceCatalog {
                schema: CATALOG_SCHEMA.to_owned(),
                revision: 0,
                entry_count: 0,
            },
            0,
        ));
    };
    let catalog: SourceCatalog = decode(&stored.payload)?;
    if encode(&catalog)? != stored.payload
        || catalog.schema != CATALOG_SCHEMA
        || catalog.revision != stored.revision
        || catalog.entry_count != stored.revision
        || catalog.revision == 0
        || catalog.revision > MAX_SAFE_INTEGER
    {
        return Err(PublicationMeteringError::corrupt());
    }
    Ok((catalog, stored.revision))
}

fn load_entry(
    storage: &dyn ProductStateStorage,
    sequence: u64,
) -> Result<PublicationMeteringSourceEntry, PublicationMeteringError> {
    let stored = storage
        .load_state(&entry_stream(sequence)?)?
        .ok_or_else(PublicationMeteringError::corrupt)?;
    let entry = decode_entry(&stored, Some(sequence))?;
    let identity = storage
        .load_state(&source_identity_stream_from_entry(&entry)?)?
        .ok_or_else(PublicationMeteringError::corrupt)?;
    if identity.revision != 1 || identity.payload != stored.payload {
        return Err(PublicationMeteringError::corrupt());
    }
    Ok(entry)
}

fn decode_entry(
    stored: &StoredState,
    expected_sequence: Option<u64>,
) -> Result<PublicationMeteringSourceEntry, PublicationMeteringError> {
    if stored.revision != 1 {
        return Err(PublicationMeteringError::corrupt());
    }
    let entry: PublicationMeteringSourceEntry = decode(&stored.payload)?;
    if encode(&entry)? != stored.payload
        || expected_sequence.is_some_and(|sequence| entry.sequence != sequence)
    {
        return Err(PublicationMeteringError::corrupt());
    }
    validate_entry(&entry, entry.sequence)?;
    Ok(entry)
}

fn scan_entries(
    storage: &dyn ProductStateStorage,
    filter: &PublicationMeteringFilter,
    snapshot_sequence: u64,
    after_sequence: u64,
    limit: u64,
) -> Result<(Vec<PublicationMeteringSourceEntry>, u64), PublicationMeteringError> {
    let capacity = usize::try_from(limit).map_err(|_| PublicationMeteringError::invalid())?;
    let mut entries = Vec::with_capacity(capacity);
    let mut sequence = after_sequence;
    let mut inspected = 0_u64;
    while sequence < snapshot_sequence && inspected < MAX_SCAN_ROWS && entries.len() < capacity {
        sequence = sequence
            .checked_add(1)
            .ok_or_else(PublicationMeteringError::corrupt)?;
        inspected = inspected
            .checked_add(1)
            .ok_or_else(PublicationMeteringError::corrupt)?;
        let entry = load_entry(storage, sequence)?;
        if matches_filter(&entry, filter) {
            entries.push(entry);
        }
    }
    Ok((entries, sequence))
}

fn cursor_position(
    cursor: &PublicationMeteringCursor,
    filter_digest: &str,
    catalog: &SourceCatalog,
) -> Result<(u64, u64), PublicationMeteringError> {
    if cursor.filter_digest != filter_digest
        || cursor.after_sequence > cursor.snapshot_sequence
        || cursor.snapshot_sequence > catalog.entry_count
        || cursor.snapshot_sequence > MAX_SAFE_INTEGER
    {
        return Err(PublicationMeteringError::invalid());
    }
    Ok((cursor.snapshot_sequence, cursor.after_sequence))
}

fn validate_filter(filter: &PublicationMeteringFilter) -> Result<(), PublicationMeteringError> {
    if filter
        .organization_id
        .as_ref()
        .is_some_and(|id| !canonical_id(&id.0, "org_"))
        || filter
            .workspace_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "wsp_"))
        || filter
            .project_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "prj_"))
        || filter
            .repository_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "rep_"))
        || filter
            .delivery_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "dlv_"))
        || filter
            .product_session_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "psn_"))
        || filter
            .user_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "usr_"))
        || filter
            .publication_id
            .as_ref()
            .is_some_and(|id| !canonical_id(&id.0, "pub_"))
    {
        return Err(PublicationMeteringError::invalid());
    }
    Ok(())
}

fn validate_attribution(
    attribution: &PublicationEnterpriseAttribution,
) -> Result<(), PublicationMeteringError> {
    if !canonical_id(&attribution.organization.0, "org_")
        || !canonical_id(&attribution.workspace.0, "wsp_")
        || !canonical_id(&attribution.project.0, "prj_")
        || !canonical_id(&attribution.repository.0, "rep_")
        || !canonical_id(&attribution.delivery.0, "dlv_")
        || !canonical_id(&attribution.product_session.0, "psn_")
        || !canonical_id(&attribution.user.0, "usr_")
    {
        return Err(PublicationMeteringError::invalid());
    }
    Ok(())
}

fn validate_entry(
    entry: &PublicationMeteringSourceEntry,
    sequence: u64,
) -> Result<(), PublicationMeteringError> {
    if entry.schema != SOURCE_SCHEMA
        || entry.sequence != sequence
        || sequence == 0
        || sequence > MAX_SAFE_INTEGER
        || !entry.remote_write_performed
        || !canonical_id(&entry.publication_id.0, "pub_")
        || !portable(&entry.operation_key, 512)
        || !sha256(&entry.request_sha256)
        || !sha256(&entry.source_digest)
        || !valid_instant(&entry.occurred_at)
    {
        return Err(PublicationMeteringError::corrupt());
    }
    validate_attribution(&entry.attribution)?;
    if entry.source_digest != source_digest(entry)? {
        return Err(PublicationMeteringError::corrupt());
    }
    Ok(())
}

fn matches_filter(
    entry: &PublicationMeteringSourceEntry,
    filter: &PublicationMeteringFilter,
) -> bool {
    let attribution = &entry.attribution;
    filter
        .organization_id
        .as_ref()
        .is_none_or(|id| id == attribution.organization_id())
        && filter
            .workspace_id
            .as_ref()
            .is_none_or(|id| id == attribution.workspace_id())
        && filter
            .project_id
            .as_ref()
            .is_none_or(|id| id == attribution.project_id())
        && filter
            .repository_id
            .as_ref()
            .is_none_or(|id| id == attribution.repository_id())
        && filter
            .delivery_id
            .as_ref()
            .is_none_or(|id| id == attribution.delivery_id())
        && filter
            .product_session_id
            .as_ref()
            .is_none_or(|id| id == attribution.product_session_id())
        && filter
            .user_id
            .as_ref()
            .is_none_or(|id| id == attribution.user_id())
        && filter
            .publication_id
            .as_ref()
            .is_none_or(|id| id == &entry.publication_id)
}

fn source_digest(
    entry: &PublicationMeteringSourceEntry,
) -> Result<String, PublicationMeteringError> {
    let mut value = entry.clone();
    value.source_digest.clear();
    digest(&value)
}

fn attribution_stream(publication_id: &PublicationId) -> Result<String, PublicationMeteringError> {
    if !canonical_id(&publication_id.0, "pub_") {
        return Err(PublicationMeteringError::invalid());
    }
    Ok(format!("{ATTRIBUTION_PREFIX}{}", publication_id.0))
}

fn source_identity_stream(
    publication_id: &PublicationId,
    operation: &PublicationOperation,
) -> Result<String, PublicationMeteringError> {
    if !canonical_id(&publication_id.0, "pub_") || !portable(operation.operation_key(), 512) {
        return Err(PublicationMeteringError::invalid());
    }
    Ok(format!(
        "{SOURCE_IDENTITY_PREFIX}{:x}",
        Sha256::digest(
            [
                publication_id.0.as_bytes(),
                b"\0",
                operation.operation_key().as_bytes(),
            ]
            .concat()
        )
    ))
}

fn source_identity_stream_from_entry(
    entry: &PublicationMeteringSourceEntry,
) -> Result<String, PublicationMeteringError> {
    if !canonical_id(&entry.publication_id.0, "pub_") || !portable(&entry.operation_key, 512) {
        return Err(PublicationMeteringError::corrupt());
    }
    Ok(format!(
        "{SOURCE_IDENTITY_PREFIX}{:x}",
        Sha256::digest(
            [
                entry.publication_id.0.as_bytes(),
                b"\0",
                entry.operation_key.as_bytes(),
            ]
            .concat()
        )
    ))
}

fn entry_stream(sequence: u64) -> Result<String, PublicationMeteringError> {
    if sequence == 0 || sequence > MAX_SAFE_INTEGER {
        return Err(PublicationMeteringError::invalid());
    }
    Ok(format!("{SOURCE_ENTRY_PREFIX}{sequence:016}"))
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, PublicationMeteringError> {
    serde_json::to_vec(value).map_err(|_| PublicationMeteringError::corrupt())
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, PublicationMeteringError> {
    serde_json::from_slice(bytes).map_err(|_| PublicationMeteringError::corrupt())
}

fn digest(value: &impl Serialize) -> Result<String, PublicationMeteringError> {
    Ok(format!("sha256:{:x}", Sha256::digest(encode(value)?)))
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A' | b'B'
                            | b'C'
                            | b'D'
                            | b'E'
                            | b'F'
                            | b'G'
                            | b'H'
                            | b'J'
                            | b'K'
                            | b'M'
                            | b'N'
                            | b'P'
                            | b'Q'
                            | b'R'
                            | b'S'
                            | b'T'
                            | b'V'
                            | b'W'
                            | b'X'
                            | b'Y'
                            | b'Z'
                    )
            })
    })
}

fn portable(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_instant(value: &Instant) -> bool {
    let bytes = value.0.as_bytes();
    bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
}
